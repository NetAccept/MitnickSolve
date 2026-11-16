mod agent;
mod runtime;

use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use agent::{ApprovalHandler, ApprovalRequest, CancelToken, Session, SessionOptions};
use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use clap::{Parser, ValueEnum};
use msk_core::ToolCall;
use msk_core::context::{ContextLimits, ContextManager, Role, summarize_output};
use msk_data::{FileObjectStore, ObjectStore};
use msk_events::{Event, EventSink, JsonlSink};
use msk_kernel::policy;
use msk_llm::{
    EchoProvider, LlmDelta, LlmError, LlmProvider, LlmResponse, OpenAiConfig, OpenAiProvider,
    OpenAiResponsesProvider, ScriptedProvider,
};
use msk_mcp::{EnvClient, EnvTransport, ServiceClient};
use msk_secret::EnvSecretStore;
use msk_wasm::ToolHost;
use runtime::{
    ControlPlane, EnvHost, build_observation, complete_or_stream, extract_data, llm_request,
    load_registry, load_registry_with_policy, substitute,
};
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Demo {
    Session,
    Echo,
    Ctf,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum ProviderKind {
    Scripted,
    /// Replays a built-in deterministic demo script. Intended for tests and
    /// `msk-eval`, which need tool calls without a model.
    ScriptedDemo,
    /// Replays `llm.response` events recorded by an earlier run. Tools still
    /// execute against the selected envd, so use it for regression runs.
    Replay,
    Openai,
    #[value(alias = "responses")]
    OpenaiResponses,
    Component,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum Approvals {
    Allow,
    Ask,
}

#[derive(Debug, Parser)]
struct Cli {
    /// Path to the envd binary built from tools/envd (embedded mode).
    #[arg(long)]
    envd: Option<PathBuf>,
    /// Connect to a running envd --service instead of spawning the binary.
    #[arg(long)]
    envd_url: Option<String>,
    /// Bearer token for --envd-url (defaults to ENVD_TOKEN).
    #[arg(long)]
    envd_token: Option<String>,
    /// PEM CA bundle for the envd HTTPS control plane.
    #[arg(long)]
    envd_ca: Option<PathBuf>,
    /// envd workspace profile: dev, host, or isolated.
    #[arg(long, default_value = "dev", value_parser = ["dev", "host", "isolated"])]
    envd_profile: String,
    /// Kernel state directory (events, envd data).
    #[arg(long, default_value = ".msk")]
    root: PathBuf,
    /// Project directory used by the embedded envd. Defaults to the current directory.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Directory containing tool manifests and WAT modules.
    #[arg(long, default_value = "tools/wasm")]
    tools_dir: PathBuf,
    /// Fallback module for tools without a matching <tool>.wat.
    #[arg(long, default_value = "tools/wasm/exec.wat")]
    wasm: PathBuf,
    /// Optional platform-specific wasmtime precompiled-module cache.
    #[arg(long)]
    wasm_cache: Option<PathBuf>,
    /// Event log used by --provider replay.
    #[arg(long)]
    replay_events: Option<PathBuf>,
    /// Scripted agent demo to run.
    #[arg(long, value_enum, default_value_t = Demo::Session)]
    demo: Demo,
    /// Keep one agent session alive and read user messages from stdin.
    /// Without this flag the binary runs the one-shot scripted demo.
    #[arg(long)]
    session: bool,
    /// Run exactly one `--goal` turn without reading stdin, then exit.
    #[arg(long)]
    headless: bool,
    /// Control-plane root used by spawn_agent/agent_result (set by mskd).
    #[arg(long)]
    control_root: Option<PathBuf>,
    /// Subagent depth; 0 is a root agent.
    #[arg(long, default_value_t = 0)]
    agent_depth: u32,
    /// Session protocol on stdout: JSON lines only, human output on stderr.
    /// Implies --session.
    #[arg(long)]
    stdio_json: bool,
    /// Task ID attached to events written by this process (set by mskd).
    #[arg(long)]
    task_id: Option<String>,
    /// Replay this event log into the new session's pinned context.
    #[arg(long)]
    resume: Option<PathBuf>,
    /// Write a native context checkpoint here (defaults to <root>/checkpoint.json).
    #[arg(long)]
    checkpoint: Option<PathBuf>,
    /// Restore a native context checkpoint (including tool call/result pairs).
    #[arg(long)]
    resume_checkpoint: Option<PathBuf>,
    /// Create a fresh envd environment instead of reopening the checkpoint's
    /// environment (forked tasks use this).
    #[arg(long)]
    fresh_env: bool,
    /// Approximate token budget for the agent context.
    #[arg(long, default_value_t = 100_000)]
    max_context_tokens: usize,
    /// Minimum recent observations kept during compaction.
    #[arg(long, default_value_t = 4)]
    min_recent_observations: usize,
    /// LLM provider backend.
    #[arg(long, value_enum, default_value_t = ProviderKind::Scripted)]
    provider: ProviderKind,
    /// Signed llm.provider component manifest (with --provider component).
    #[arg(long)]
    component_manifest: Option<PathBuf>,
    /// Component trust store; defaults to trust.json next to the manifest.
    #[arg(long)]
    component_trust: Option<PathBuf>,
    /// Run the component in this out-of-process host binary.
    #[arg(long)]
    component_host: Option<PathBuf>,
    /// Ask before executing commands in a session; allow runs them directly.
    #[arg(long, value_enum, default_value_t = Approvals::Ask)]
    approvals: Approvals,
    /// Goal handed to a real model.
    #[arg(
        long,
        default_value = "Create a persistent shell, run a command, and close it."
    )]
    goal: String,
    /// Maximum model/tool steps within one user turn.
    #[arg(long, default_value_t = 12)]
    max_steps: usize,
    /// Stream text deltas from the provider when available.
    #[arg(long)]
    stream: bool,
    /// OpenAI-compatible base URL (defaults to MSK_LLM_BASE_URL or OpenAI).
    #[arg(long)]
    llm_base_url: Option<String>,
    /// Model name (defaults to MSK_LLM_MODEL).
    #[arg(long)]
    llm_model: Option<String>,
    /// API key (prefer MSK_LLM_API_KEY over the command line).
    #[arg(long)]
    llm_api_key: Option<String>,
    /// Reasoning effort for OpenAI-compatible models: minimal, low, medium, high.
    #[arg(long, value_parser = ["minimal", "low", "medium", "high"])]
    llm_effort: Option<String>,
}

/// Wraps the JSONL sink so every event emitted by this process carries the
/// daemon-assigned task ID. Events that already carry one are left alone.
struct TaggedSink {
    inner: Arc<dyn EventSink>,
    task: Option<String>,
}

impl EventSink for TaggedSink {
    fn emit(&self, event: &Event) -> Result<(), msk_events::EventError> {
        if event.task.is_none() {
            if let Some(task) = &self.task {
                return self.inner.emit(&event.clone().with_task(task.clone()));
            }
        }
        self.inner.emit(event)
    }
}

enum SessionInput {
    User(String),
    Interrupt,
    Ping,
    Shutdown,
    Invalid(String),
}

struct PendingApproval {
    id: u64,
    tx: mpsc::Sender<bool>,
}

/// Shared between the turn thread (which blocks in `request`) and the stdin
/// reader (which resolves `y`/`n` or `approval.respond` frames).
#[derive(Clone)]
struct ApprovalGate {
    pending: Arc<Mutex<Option<PendingApproval>>>,
    stdio_json: bool,
    timeout: Duration,
}

impl ApprovalGate {
    fn new(stdio_json: bool, timeout: Duration) -> Self {
        Self {
            pending: Arc::new(Mutex::new(None)),
            stdio_json,
            timeout,
        }
    }

    fn is_pending(&self) -> bool {
        self.pending
            .lock()
            .map(|pending| pending.is_some())
            .unwrap_or(false)
    }

    fn respond(&self, id: Option<u64>, approved: bool) -> bool {
        let Some(pending) = self
            .pending
            .lock()
            .ok()
            .and_then(|mut pending| pending.take())
        else {
            return false;
        };
        if id.is_some_and(|id| id != pending.id) {
            // Put it back; the frame was for a different request.
            if let Ok(mut slot) = self.pending.lock() {
                *slot = Some(pending);
            }
            return false;
        }
        pending.tx.send(approved).is_ok()
    }

    fn clear(&self) {
        if let Ok(mut pending) = self.pending.lock() {
            *pending = None;
        }
    }
}

impl ApprovalHandler for ApprovalGate {
    fn request(&mut self, request: &ApprovalRequest, cancel: &CancelToken) -> bool {
        let (tx, rx) = mpsc::channel();
        match self.pending.lock() {
            Ok(mut pending) => {
                *pending = Some(PendingApproval { id: request.id, tx });
            }
            Err(_) => return false,
        }

        if self.stdio_json {
            emit_protocol(json!({
                "type": "approval.request",
                "id": request.id,
                "tool": request.tool,
                "args": request.args,
                "reason": request.reason
            }));
        } else {
            eprintln!(
                "\napproval required: {} — {}\nargs: {}\napprove? [y/N] ",
                request.tool, request.reason, request.args
            );
            let _ = io::stderr().flush();
        }

        let deadline = Instant::now() + self.timeout;
        loop {
            if cancel.is_cancelled() {
                self.clear();
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                self.clear();
                return false;
            }
            match rx.recv_timeout(remaining.min(Duration::from_millis(200))) {
                Ok(approved) => return approved,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.clear();
                    return false;
                }
            }
        }
    }
}

/// Reads stdin on its own thread so `/interrupt` (or a JSON interrupt frame)
/// can reach a turn that is currently blocked on a model or tool call. The
/// reader sets the cancel token immediately, before the main loop dequeues
/// the message.
fn spawn_stdin_reader(
    cancel: agent::CancelToken,
    stdio_json: bool,
    gate: ApprovalGate,
) -> mpsc::Receiver<SessionInput> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let stdin = io::stdin();
        for line in stdin.lock().lines() {
            let line = match line {
                Ok(line) => line,
                Err(err) => {
                    let _ = tx.send(SessionInput::Invalid(format!("stdin error: {err}")));
                    break;
                }
            };
            if stdio_json {
                let value: Value = match serde_json::from_str(&line) {
                    Ok(value) => value,
                    Err(err) => {
                        let _ = tx.send(SessionInput::Invalid(format!("invalid input: {err}")));
                        continue;
                    }
                };
                let kind = value.get("type").and_then(Value::as_str).unwrap_or("user");
                if kind == "approval.respond" {
                    let id = value.get("id").and_then(Value::as_u64);
                    let approved = value
                        .get("approved")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    if !gate.respond(id, approved) {
                        let _ = tx.send(SessionInput::Invalid(
                            "approval response did not match a pending request".into(),
                        ));
                    }
                    continue;
                }
                match kind {
                    "user" => {
                        let text = value
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        if text.is_empty() {
                            let _ = tx.send(SessionInput::Invalid("empty user message".into()));
                        } else {
                            let _ = tx.send(SessionInput::User(text));
                        }
                    }
                    "interrupt" => {
                        cancel.cancel();
                        let _ = tx.send(SessionInput::Interrupt);
                    }
                    "ping" => {
                        let _ = tx.send(SessionInput::Ping);
                    }
                    "shutdown" => {
                        let _ = tx.send(SessionInput::Shutdown);
                        break;
                    }
                    other => {
                        let _ = tx.send(SessionInput::Invalid(format!(
                            "unknown input type: {other}"
                        )));
                    }
                }
            } else {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    if gate.is_pending() {
                        gate.respond(None, false);
                    }
                    continue;
                }
                if gate.is_pending() {
                    if trimmed == "/interrupt" {
                        cancel.cancel();
                        let _ = tx.send(SessionInput::Interrupt);
                        continue;
                    }
                    let answer = trimmed.to_ascii_lowercase();
                    let approved = matches!(answer.as_str(), "y" | "yes" | "approve");
                    let denied = matches!(answer.as_str(), "n" | "no" | "deny");
                    if approved || denied {
                        gate.respond(None, approved);
                    } else {
                        eprintln!("approve? answer y or n");
                    }
                    continue;
                }
                if trimmed == "/quit" || trimmed == "/exit" {
                    let _ = tx.send(SessionInput::Shutdown);
                    break;
                }
                if trimmed == "/interrupt" {
                    cancel.cancel();
                    let _ = tx.send(SessionInput::Interrupt);
                    continue;
                }
                let _ = tx.send(SessionInput::User(trimmed.to_string()));
            }
        }
        if gate.is_pending() {
            gate.respond(None, false);
        }
        let _ = tx.send(SessionInput::Shutdown);
    });
    rx
}

struct Backend {
    client: Box<dyn EnvTransport>,
    env_id: String,
    reused_env: bool,
}

fn connect_backend(
    cli: &Cli,
    sink: &dyn EventSink,
    name: &str,
    resume_env: Option<&str>,
    workspace: Option<&std::path::Path>,
) -> Result<Backend> {
    if !cli.tools_dir.exists() {
        bail!("tool directory not found: {}", cli.tools_dir.display());
    }
    if !cli.wasm.exists() {
        bail!("fallback WASM module not found: {}", cli.wasm.display());
    }
    if cli.envd_url.is_some() && cli.envd.is_some() {
        bail!("use either --envd or --envd-url, not both");
    }
    if cli.envd.is_some() && cli.envd_profile != "dev" {
        bail!(
            "--envd-profile {} requires --envd-url (service mode)",
            cli.envd_profile
        );
    }

    if let Some(url) = &cli.envd_url {
        let token = cli
            .envd_token
            .clone()
            .or_else(|| std::env::var("ENVD_TOKEN").ok())
            .context("--envd-token or ENVD_TOKEN is required with --envd-url")?;
        let ca_pem = match &cli.envd_ca {
            Some(path) => Some(
                std::fs::read(path).with_context(|| format!("read envd CA {}", path.display()))?,
            ),
            None => None,
        };
        let mut service =
            ServiceClient::new_with_ca(url, token, ca_pem).context("configure envd TLS")?;
        let init = service.initialize().context("envd service initialize")?;
        emit(sink, "mcp.initialize", init)?;
        let tools = service.request("tools/list", json!({}))?;
        let tool_count = tools
            .get("tools")
            .and_then(Value::as_array)
            .map(Vec::len)
            .unwrap_or(0);
        human_line(cli, &format!("envd service: {url} ({tool_count} tools)"));
        emit(sink, "mcp.tools", json!({"count": tool_count}))?;
        let mut reused = None;
        if !cli.fresh_env {
            if let Some(env_id) = resume_env {
                match service.call_tool("env_open", json!({"env_id": env_id})) {
                    Ok(result) => {
                        let is_error = result
                            .get("isError")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let structured = result
                            .get("structuredContent")
                            .cloned()
                            .unwrap_or(Value::Null);
                        let ok = structured
                            .get("ok")
                            .and_then(Value::as_bool)
                            .unwrap_or(false);
                        let data = structured.get("data").cloned().unwrap_or(Value::Null);
                        let profile = data.get("profile").and_then(Value::as_str);
                        if !is_error && ok && profile.is_none_or(|value| value == cli.envd_profile)
                        {
                            emit(
                                sink,
                                "env.open",
                                json!({
                                    "env_id": env_id,
                                    "profile": data.get("profile"),
                                    "isolation": data.get("isolation")
                                }),
                            )?;
                            reused = Some(data);
                        } else {
                            emit(
                                sink,
                                "env.open_failed",
                                json!({
                                    "env_id": env_id,
                                    "reason": if is_error || !ok {
                                        "not_available"
                                    } else {
                                        "profile_mismatch"
                                    }
                                }),
                            )?;
                        }
                    }
                    Err(error) => {
                        emit(
                            sink,
                            "env.open_failed",
                            json!({"env_id": env_id, "reason": error.to_string()}),
                        )?;
                    }
                }
            }
        }
        let reused_env = reused.is_some();
        let env = match reused {
            Some(env) => env,
            None => tool_data(
                &mut service,
                "env_create",
                json!({"name": name, "kind": "managed", "profile": cli.envd_profile}),
                sink,
            )?,
        };
        let env_id = env
            .get("env_id")
            .and_then(Value::as_str)
            .context("env_create/env_open did not return env_id")?
            .to_string();
        human_line(
            cli,
            &format!("env: {env_id}{}", if reused_env { " (reused)" } else { "" }),
        );
        if !reused_env {
            emit(
                sink,
                "env.create",
                json!({"env_id": env_id, "workspace": env.get("workspace")}),
            )?;
        }
        return Ok(Backend {
            client: Box::new(service),
            env_id,
            reused_env,
        });
    }

    let envd = cli
        .envd
        .as_ref()
        .context("either --envd or --envd-url is required")?;
    let envd_bin = envd.canonicalize().unwrap_or_else(|_| envd.clone());
    if !envd_bin.exists() {
        bail!(
            "envd binary not found: {} — build it with `cd tools/envd && go build -o ../../target/debug/envd ./cmd/envd`",
            envd_bin.display()
        );
    }
    let envd_root = workspace
        .map(std::path::Path::to_path_buf)
        .unwrap_or_else(|| cli.root.join("envd"));
    std::fs::create_dir_all(&envd_root)?;
    let mut embedded = EnvClient::spawn(&envd_bin, &envd_root).context("spawn embedded envd")?;
    let ack = embedded
        .handshake("agent-local")
        .context("envd handshake")?;
    human_line(
        cli,
        &format!(
            "envd: {} (protocol {}, max_frame {})",
            ack.server, ack.protocol, ack.max_frame
        ),
    );
    emit(
        sink,
        "envd.hello",
        json!({"server": ack.server, "protocol": ack.protocol, "max_frame": ack.max_frame}),
    )?;
    let init = embedded.initialize().context("MCP initialize")?;
    emit(sink, "mcp.initialize", init)?;
    let tools = embedded.request("tools/list", json!({}))?;
    let tool_count = tools
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    human_line(cli, &format!("envd tools: {tool_count}"));
    emit(sink, "mcp.tools", json!({"count": tool_count}))?;
    let mut env_params = json!({
        "name": name,
        "kind": "managed",
        "profile": cli.envd_profile
    });
    if workspace.is_some() {
        env_params["root"] = json!(".");
    }
    let env = tool_data(&mut embedded, "env_create", env_params, sink)?;
    let env_id = env
        .get("env_id")
        .and_then(Value::as_str)
        .context("env_create did not return env_id")?
        .to_string();
    human_line(
        cli,
        &format!(
            "env: {env_id} at {}",
            env.get("workspace").and_then(Value::as_str).unwrap_or("?")
        ),
    );
    emit(
        sink,
        "env.create",
        json!({"env_id": env_id, "workspace": env.get("workspace")}),
    )?;
    Ok(Backend {
        client: Box::new(embedded),
        env_id,
        reused_env: false,
    })
}
fn main() -> Result<()> {
    if std::env::args_os().len() == 1 {
        let exe = std::env::current_exe().context("current executable")?;
        let tui = exe.with_file_name(if cfg!(windows) {
            "msk-tui.exe"
        } else {
            "msk-tui"
        });
        if tui.exists() {
            let status = std::process::Command::new(&tui)
                .status()
                .with_context(|| format!("launch {}", tui.display()))?;
            std::process::exit(status.code().unwrap_or(1));
        }
        bail!(
            "msk-tui not found next to msk; run `cargo build --bins` to build the interactive app"
        );
    }

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();
    std::fs::create_dir_all(&cli.root)?;
    let events_path = cli.root.join("events.jsonl");
    let base: Arc<dyn EventSink> = Arc::new(JsonlSink::open(&events_path)?);
    let sink: Arc<dyn EventSink> = Arc::new(TaggedSink {
        inner: base,
        task: cli.task_id.clone(),
    });
    emit(
        sink.as_ref(),
        "kernel.start",
        json!({
            "root": cli.root.display().to_string(),
            "mode": if cli.session || cli.stdio_json || cli.headless { "session" } else { "demo" },
            "task_id": cli.task_id,
            "demo": format!("{:?}", cli.demo)
        }),
    )?;

    if cli.session || cli.stdio_json || cli.headless {
        return run_session(&cli, sink, &events_path);
    }

    let backend = connect_backend(&cli, sink.as_ref(), "demo", None, None)?;
    let env_id = backend.env_id.clone();
    let mut client = backend.client;

    let content = b"hello from msk\n";
    let write = tool_data(
        &mut *client,
        "write_file",
        json!({
            "env_id": env_id.clone(),
            "path": "hello.txt",
            "data": STANDARD.encode(content),
            "atomic": true
        }),
        sink.as_ref(),
    )?;
    emit(sink.as_ref(), "file.write", write)?;

    let read = tool_data(
        &mut *client,
        "read_file",
        json!({"env_id": env_id.clone(), "path": "hello.txt"}),
        sink.as_ref(),
    )?;
    let data = read
        .get("data")
        .and_then(Value::as_str)
        .context("read_file did not return data")?;
    let decoded = STANDARD.decode(data)?;
    if decoded != content {
        bail!("read_file roundtrip mismatch");
    }
    println!("file roundtrip: ok ({} bytes)", decoded.len());

    let large_content: String = (0..500).map(|index| format!("line-{index}\n")).collect();
    let large_write = tool_data(
        &mut *client,
        "write_file",
        json!({
            "env_id": env_id.clone(),
            "path": "large.txt",
            "data": STANDARD.encode(large_content.as_bytes()),
            "atomic": true
        }),
        sink.as_ref(),
    )?;
    emit(sink.as_ref(), "file.write_large", large_write)?;

    let object_store: Arc<dyn ObjectStore> =
        Arc::new(FileObjectStore::open(cli.root.join("artifacts")).context("open artifact store")?);
    let client = Arc::new(Mutex::new(client));
    let env_host = Arc::new(EnvHost {
        client: client.clone(),
        artifacts: object_store.clone(),
        control: None,
        secrets: Arc::new(EnvSecretStore::default()),
        agent_depth: 0,
        task_id: cli.task_id.clone(),
        default_provider: "scripted".into(),
    });
    let host: Arc<dyn ToolHost> = env_host.clone();

    let tool_names: &[&str] = match cli.demo {
        Demo::Session => &[
            "session_create",
            "session_exec",
            "session_write",
            "session_read",
            "session_resize",
            "session_close",
            "artifact_read",
        ],
        Demo::Echo => &["echo"],
        Demo::Ctf => &["flag_add", "flag_list"],
    };
    let registry = load_registry(
        &cli.tools_dir,
        &cli.wasm,
        host,
        tool_names,
        cli.wasm_cache.as_deref(),
    )?;
    emit(
        sink.as_ref(),
        "tool.registry",
        json!({"tools": tool_names, "dir": cli.tools_dir.display().to_string()}),
    )?;

    let context_limits = ContextLimits::default()
        .with_max_tokens(cli.max_context_tokens)
        .with_min_recent_observations(cli.min_recent_observations);
    let mut context = ContextManager::new(context_limits.clone());
    context.push_pinned(Role::User, cli.goal.clone());
    context.push_pinned(Role::System, format!("env_id={env_id}"));

    let provider = build_provider(&cli, true)?;
    let mut response = complete_or_stream(
        provider.as_ref(),
        &llm_request(&context, &registry),
        cli.stream,
        &mut |delta| {
            let LlmDelta::Text(text) = delta;
            print!("{text}");
        },
    )
    .context("LLM call")?;
    if cli.stream {
        println!();
    }
    let mut last_session: Option<String> = None;
    let mut last_artifact: Option<String> = None;
    let mut steps = 0usize;

    loop {
        if steps >= cli.max_steps {
            bail!("agent exceeded {} steps", cli.max_steps);
        }
        steps += 1;
        emit(
            sink.as_ref(),
            "llm.response",
            json!({
                "text": response.text.clone(),
                "tool_calls": response.tool_calls.len(),
                "usage_tokens": response.usage_tokens
            }),
        )?;
        if !cli.stream && !response.text.is_empty() {
            println!("llm: {}", response.text);
        }
        if response.tool_calls.is_empty() {
            context.push(Role::Assistant, response.text.clone());
            break;
        }
        context.push_assistant_tool_calls(response.text.clone(), response.tool_calls.clone());

        for call in response.tool_calls {
            let mut args = call.args.clone();
            substitute(
                &mut args,
                &env_id,
                last_session.as_deref(),
                last_artifact.as_deref(),
            )?;

            let tool = registry
                .get(&call.tool)
                .with_context(|| format!("tool not registered: {}", call.tool))?;
            emit(
                sink.as_ref(),
                "tool.call",
                json!({"tool": call.tool, "args": args}),
            )?;
            emit(
                sink.as_ref(),
                "policy.decision",
                json!({
                    "tool": tool.context.tool.clone(),
                    "granted": tool.context.capabilities.clone(),
                    "decision": "allow"
                }),
            )?;

            let input = serde_json::to_vec(&args)?;
            let started = std::time::Instant::now();
            let output = tool
                .runtime
                .run_with_context(&tool.context, &input)
                .with_context(|| format!("WASM tool {}", call.tool))?;
            let duration_ms = started.elapsed().as_millis();
            let value: Value =
                serde_json::from_slice(&output).context("WASM tool returned non-JSON")?;
            let data = extract_data(&value)?;

            let (observation, artifact) =
                build_observation(&call.tool, &data, &context_limits, object_store.as_ref())?;
            if let Some(object) = &artifact {
                last_artifact = Some(object.id.clone());
                emit(
                    sink.as_ref(),
                    "data.put",
                    json!({
                        "id": object.id,
                        "bytes": object.size,
                        "label": object.label,
                        "hash": object.hash
                    }),
                )?;
            }
            emit(
                sink.as_ref(),
                "wasm.result",
                json!({
                    "tool": call.tool,
                    "duration_ms": duration_ms,
                    "artifact": artifact.as_ref().map(|object| object.id.clone()),
                    "observation": observation.clone()
                }),
            )?;
            println!(
                "[{}] {}",
                call.tool,
                summarize_output(&observation, 200, 80)
            );
            context.push_tool_result(call.id.clone(), call.tool.clone(), observation);

            if call.tool == "session_create" {
                last_session = data
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(session) = &last_session {
                    context.push_pinned(Role::System, format!("session_id={session}"));
                } else {
                    bail!("session_create did not return session_id");
                }
            } else if call.tool == "session_close" {
                context.push_pinned(Role::System, "session_id=closed");
                last_session = None;
            }
        }

        let removed = context.compact();
        if removed > 0 {
            emit(
                sink.as_ref(),
                "context.compact",
                json!({
                    "removed": removed,
                    "messages": context.messages().len(),
                    "approx_tokens": context.approx_tokens()
                }),
            )?;
        }
        emit(
            sink.as_ref(),
            "context.state",
            json!({
                "messages": context.messages().len(),
                "approx_tokens": context.approx_tokens()
            }),
        )?;

        match complete_or_stream(
            provider.as_ref(),
            &llm_request(&context, &registry),
            cli.stream,
            &mut |delta| {
                let LlmDelta::Text(text) = delta;
                print!("{text}");
            },
        ) {
            Ok(next) => {
                if cli.stream {
                    println!();
                }
                response = next;
            }
            Err(LlmError::Exhausted) => break,
            Err(err) => return Err(err.into()),
        }
    }

    if let Ok(mut client) = env_host.client.lock() {
        client.shutdown();
    }
    println!("events: {}", events_path.display());
    Ok(())
}

fn run_session(cli: &Cli, sink: Arc<dyn EventSink>, events_path: &std::path::Path) -> Result<()> {
    let checkpoint = match &cli.resume_checkpoint {
        Some(path) => {
            let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
            let checkpoint: agent::Checkpoint =
                serde_json::from_slice(&bytes).context("parse checkpoint")?;
            if checkpoint.schema_version != 1 {
                bail!(
                    "unsupported checkpoint schema {}",
                    checkpoint.schema_version
                );
            }
            Some(checkpoint)
        }
        None => None,
    };
    let resume_env = checkpoint
        .as_ref()
        .and_then(|checkpoint| checkpoint.env_id.as_deref())
        .map(str::to_string);
    let workspace = cli
        .workspace
        .clone()
        .or_else(|| std::env::current_dir().ok());
    let backend = connect_backend(
        cli,
        sink.as_ref(),
        "session",
        resume_env.as_deref(),
        workspace.as_deref(),
    )?;
    let env_id = backend.env_id;
    let env_reused = backend.reused_env;
    let client = backend.client;

    let object_store: Arc<dyn ObjectStore> =
        Arc::new(FileObjectStore::open(cli.root.join("artifacts")).context("open artifact store")?);
    let client = Arc::new(Mutex::new(client));
    let control = cli
        .control_root
        .as_ref()
        .map(|root| {
            ControlPlane::connect(root)
                .with_context(|| format!("connect control plane {}", root.display()))
        })
        .transpose()?;
    let env_host = Arc::new(EnvHost {
        client,
        artifacts: object_store.clone(),
        control: control.map(Arc::new),
        secrets: Arc::new(EnvSecretStore::default()),
        agent_depth: cli.agent_depth,
        task_id: cli.task_id.clone(),
        default_provider: provider_kind_str(cli.provider).into(),
    });
    let host: Arc<dyn ToolHost> = env_host.clone();
    let tool_names = [
        "exec",
        "session_create",
        "session_exec",
        "session_write",
        "session_read",
        "session_resize",
        "session_close",
        "artifact_read",
        "read_file",
        "write_file",
        "list_dir",
        "stat_file",
        "http_request",
        "spawn_agent",
        "agent_result",
        "blackboard_get",
        "blackboard_set",
        "blackboard_list",
        "flag_add",
        "flag_list",
        "echo",
    ];
    let registry_policy = policy::policy_with_approvals(cli.approvals == Approvals::Ask);
    let registry = load_registry_with_policy(
        &cli.tools_dir,
        &cli.wasm,
        host,
        &tool_names,
        &registry_policy,
        cli.wasm_cache.as_deref(),
    )?;
    emit(
        sink.as_ref(),
        "tool.registry",
        json!({"tools": tool_names, "dir": cli.tools_dir.display().to_string()}),
    )?;

    let limits = ContextLimits::default()
        .with_max_tokens(cli.max_context_tokens)
        .with_min_recent_observations(cli.min_recent_observations);
    let (mut context, initial_turns) = if let Some(checkpoint) = checkpoint {
        let turns = checkpoint.turns;
        let messages = checkpoint.messages.len();
        let mut context = ContextManager::restore(limits.clone(), checkpoint.messages);
        if env_reused {
            context.push_pinned(
                Role::System,
                format!(
                    "Resumed from a context checkpoint with the original envd environment {env_id} reopened. Session IDs in the restored transcript remain valid."
                ),
            );
        } else {
            context.push_pinned(
                Role::System,
                "Resumed from a context checkpoint. Environment and session IDs in the restored transcript are stale; create a new session before running commands.",
            );
        }
        context.push_pinned(Role::System, format!("env_id={env_id}"));
        emit(
            sink.as_ref(),
            "session.checkpoint_resume",
            json!({
                "path": cli.resume_checkpoint.as_ref().map(|path| path.display().to_string()).unwrap_or_default(),
                "turns": turns,
                "messages": messages,
                "env_reused": env_reused
            }),
        )?;
        (context, turns)
    } else {
        let mut context = ContextManager::new(limits.clone());
        context.push_pinned(
            Role::System,
            "You are MitnickSolve, a CTF agent. Use the provided tools for all machine access. \
             Prefer persistent sessions for multi-command work. Tool output over 2 KiB is stored \
             as an artifact; use artifact_read to inspect it. Keep responses concise.",
        );
        context.push_pinned(Role::System, format!("env_id={env_id}"));
        (context, 0)
    };
    if let Some(path) = &cli.resume {
        let events =
            msk_events::read_events(path).with_context(|| format!("read {}", path.display()))?;
        let text = msk_kernel::replay::summarize(&events, cli.task_id.as_deref(), 12_000);
        if !text.is_empty() {
            context.push_pinned(
                Role::System,
                format!(
                    "Previous session transcript. Environment and session IDs from it are stale; \
                     create a new session before running commands.\n\n{text}"
                ),
            );
            emit(
                sink.as_ref(),
                "session.resume",
                json!({
                    "path": path.display().to_string(),
                    "events": events.len(),
                    "chars": text.chars().count()
                }),
            )?;
        }
    }

    let provider = build_provider(cli, false)?;
    let gate = ApprovalGate::new(cli.stdio_json, Duration::from_secs(300));
    let approvals: Option<Box<dyn ApprovalHandler>> = if cli.approvals == Approvals::Ask {
        Some(Box::new(gate.clone()))
    } else {
        None
    };
    let checkpoint_path = cli
        .checkpoint
        .clone()
        .unwrap_or_else(|| cli.root.join("checkpoint.json"));
    let mut session = Session::new(SessionOptions {
        env_id,
        registry,
        provider,
        context,
        limits,
        objects: object_store,
        sink: sink.clone(),
        max_steps: cli.max_steps,
        stream: cli.stream,
        task: cli.task_id.clone(),
        cancel: agent::CancelToken::new(),
        approvals,
        checkpoint: Some(checkpoint_path.clone()),
        initial_turns,
    });
    emit(
        sink.as_ref(),
        "session.checkpoint",
        json!({
            "path": checkpoint_path.display().to_string(),
            "initial_turns": initial_turns
        }),
    )?;

    if cli.headless {
        let stdio = cli.stdio_json;
        let stream = cli.stream;
        let mut printed = false;
        let outcome = {
            let mut on_text = |chunk: &str| {
                printed = true;
                if stdio {
                    emit_protocol(json!({"type": "agent.text", "text": chunk, "delta": stream}));
                } else {
                    print!("{chunk}");
                    let _ = io::stdout().flush();
                }
            };
            session.run_turn(&cli.goal, &mut on_text)
        };
        if !stdio && printed {
            println!();
        }
        let mut end = json!({
            "type": "turn.end",
            "status": outcome.status(),
            "turns": session.turns()
        });
        if let Some(message) = outcome.message() {
            end["message"] = json!(message);
        }
        if stdio {
            emit_protocol(end);
        } else if let Some(message) = outcome.message() {
            eprintln!("{}: {message}", outcome.status());
        }
        env_host.shutdown();
        emit(
            sink.as_ref(),
            "session.end",
            json!({
                "turns": session.turns(),
                "messages": session.context().messages().len()
            }),
        )?;
        return Ok(());
    }

    let inputs = spawn_stdin_reader(session.cancel_token(), cli.stdio_json, gate);
    loop {
        if !cli.stdio_json {
            eprint!("> ");
            let _ = io::stderr().flush();
        }
        let input = match inputs.recv() {
            Ok(input) => input,
            Err(_) => break,
        };
        let text = match input {
            SessionInput::Shutdown => break,
            SessionInput::Interrupt => {
                // The reader already set the token. If no turn was running,
                // clear it so the next user message starts cleanly.
                session.clear_cancel();
                continue;
            }
            SessionInput::Ping => {
                if cli.stdio_json {
                    emit_protocol(json!({"type": "pong"}));
                }
                continue;
            }
            SessionInput::Invalid(message) => {
                if cli.stdio_json {
                    emit_protocol(json!({"type": "error", "message": message}));
                } else {
                    eprintln!("{message}");
                }
                continue;
            }
            SessionInput::User(text) => text,
        };

        let mut printed = false;
        let outcome = {
            let stdio = cli.stdio_json;
            let stream = cli.stream;
            let mut on_text = |chunk: &str| {
                printed = true;
                if stdio {
                    emit_protocol(json!({"type": "agent.text", "text": chunk, "delta": stream}));
                } else {
                    print!("{chunk}");
                    let _ = io::stdout().flush();
                }
            };
            session.run_turn(&text, &mut on_text)
        };

        if !cli.stdio_json && printed {
            println!();
        }
        let mut end = json!({
            "type": "turn.end",
            "status": outcome.status(),
            "turns": session.turns()
        });
        if let Some(message) = outcome.message() {
            end["message"] = json!(message);
        }
        if cli.stdio_json {
            emit_protocol(end);
        } else if let Some(message) = outcome.message() {
            eprintln!("{}: {message}", outcome.status());
        }
    }

    env_host.shutdown();
    emit(
        sink.as_ref(),
        "session.end",
        json!({
            "turns": session.turns(),
            "messages": session.context().messages().len()
        }),
    )?;
    human_line(cli, &format!("events: {}", events_path.display()));
    Ok(())
}

fn provider_kind_str(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::Scripted => "scripted",
        ProviderKind::ScriptedDemo => "scripted-demo",
        ProviderKind::Replay => "replay",
        ProviderKind::Openai => "openai",
        ProviderKind::OpenaiResponses => "openai-responses",
        ProviderKind::Component => "component",
    }
}

fn build_provider(cli: &Cli, demo: bool) -> Result<Box<dyn LlmProvider>> {
    if cli.provider != ProviderKind::Component
        && (cli.component_manifest.is_some()
            || cli.component_trust.is_some()
            || cli.component_host.is_some())
    {
        bail!("--component-* flags require --provider component");
    }
    Ok(match cli.provider {
        ProviderKind::Scripted => {
            if demo {
                Box::new(ScriptedProvider::new("scripted", demo_responses(cli.demo)))
            } else {
                Box::new(EchoProvider::new("echo"))
            }
        }
        ProviderKind::ScriptedDemo => Box::new(ScriptedProvider::new(
            "scripted-demo",
            demo_responses(cli.demo),
        )),
        ProviderKind::Replay => {
            let path = cli
                .replay_events
                .as_ref()
                .context("--replay-events is required with --provider replay")?;
            let events = msk_events::read_events(path)
                .with_context(|| format!("read {}", path.display()))?;
            let responses = replay_responses(&events)?;
            if responses.is_empty() {
                bail!("no llm.response events in {}", path.display());
            }
            Box::new(ScriptedProvider::new("replay", responses))
        }
        ProviderKind::Openai => {
            Box::new(OpenAiProvider::new(openai_config(cli)?).context("configure OpenAI provider")?)
        }
        ProviderKind::OpenaiResponses => Box::new(
            OpenAiResponsesProvider::new(openai_config(cli)?)
                .context("configure OpenAI Responses provider")?,
        ),
        ProviderKind::Component => {
            let manifest = cli
                .component_manifest
                .clone()
                .context("--component-manifest is required with --provider component")?;
            let trust = cli.component_trust.clone().unwrap_or_else(|| {
                manifest
                    .parent()
                    .map(|parent| parent.join("trust.json"))
                    .unwrap_or_else(|| PathBuf::from("trust.json"))
            });
            let provider = msk_kernel::component::ComponentLlmProvider::load(
                &msk_kernel::component::ComponentOptions {
                    manifest,
                    trust,
                    host: cli.component_host.clone(),
                },
            )
            .context("load component provider")?;
            Box::new(provider)
        }
    })
}

fn human_line(cli: &Cli, text: &str) {
    if cli.stdio_json {
        eprintln!("{text}");
    } else {
        println!("{text}");
    }
}

fn emit_protocol(value: Value) {
    println!(
        "{}",
        serde_json::to_string(&value).unwrap_or_else(|_| "{\"type\":\"error\"}".to_string())
    );
    let _ = io::stdout().flush();
}

fn openai_config(cli: &Cli) -> Result<OpenAiConfig> {
    let base_url = cli
        .llm_base_url
        .clone()
        .or_else(|| std::env::var("MSK_LLM_BASE_URL").ok())
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let model = cli
        .llm_model
        .clone()
        .or_else(|| std::env::var("MSK_LLM_MODEL").ok())
        .context("--llm-model or MSK_LLM_MODEL is required for OpenAI providers")?;
    let api_key = cli
        .llm_api_key
        .clone()
        .or_else(|| std::env::var("MSK_LLM_API_KEY").ok())
        .context("--llm-api-key or MSK_LLM_API_KEY is required for OpenAI providers")?;
    let effort = cli
        .llm_effort
        .clone()
        .or_else(|| std::env::var("MSK_LLM_EFFORT").ok())
        .filter(|effort| !effort.is_empty() && effort != "default");
    Ok(OpenAiConfig {
        base_url,
        model,
        api_key,
        effort,
    })
}

fn demo_responses(demo: Demo) -> Vec<LlmResponse> {
    match demo {
        Demo::Echo => vec![
            LlmResponse {
                text: "echo the tool arguments".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    tool: "echo".into(),
                    args: json!({"message": "hello from echo"}),
                }],
                usage_tokens: 5,
            },
            LlmResponse {
                text: "echo demo complete".into(),
                tool_calls: Vec::new(),
                usage_tokens: 5,
            },
        ],
        Demo::Session => {
            let state_command = if cfg!(windows) {
                "set MSK_DEMO=42 && echo !MSK_DEMO!"
            } else {
                "MSK_DEMO=42; echo $MSK_DEMO"
            };
            let large_command = if cfg!(windows) {
                "for /L %i in (1,1,500) do @echo line %i"
            } else {
                "i=0; while [ $i -lt 500 ]; do echo \"line $i\"; i=$((i+1)); done"
            };
            vec![
                LlmResponse {
                    text: "create a persistent shell".into(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        tool: "session_create".into(),
                        args: json!({"env_id": "$ENV"}),
                    }],
                    usage_tokens: 10,
                },
                LlmResponse {
                    text: "run a command".into(),
                    tool_calls: vec![ToolCall {
                        id: "c2".into(),
                        tool: "session_exec".into(),
                        args: json!({"session_id": "$SESSION", "command": "echo session-one"}),
                    }],
                    usage_tokens: 10,
                },
                LlmResponse {
                    text: "verify shell state persists".into(),
                    tool_calls: vec![ToolCall {
                        id: "c3".into(),
                        tool: "session_exec".into(),
                        args: json!({"session_id": "$SESSION", "command": state_command}),
                    }],
                    usage_tokens: 10,
                },
                LlmResponse {
                    text: "produce a large output".into(),
                    tool_calls: vec![ToolCall {
                        id: "c4".into(),
                        tool: "session_exec".into(),
                        args: json!({"session_id": "$SESSION", "command": large_command}),
                    }],
                    usage_tokens: 10,
                },
                LlmResponse {
                    text: "read the artifact head".into(),
                    tool_calls: vec![ToolCall {
                        id: "c5".into(),
                        tool: "artifact_read".into(),
                        args: json!({"id": "$ARTIFACT", "offset": 0, "limit": 200}),
                    }],
                    usage_tokens: 10,
                },
                LlmResponse {
                    text: "close the session".into(),
                    tool_calls: vec![ToolCall {
                        id: "c6".into(),
                        tool: "session_close".into(),
                        args: json!({"session_id": "$SESSION"}),
                    }],
                    usage_tokens: 10,
                },
                LlmResponse {
                    text: "session demo complete".into(),
                    tool_calls: Vec::new(),
                    usage_tokens: 10,
                },
            ]
        }
        Demo::Ctf => vec![
            LlmResponse {
                text: "record the captured flag".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    tool: "flag_add".into(),
                    args: json!({"flag": "flag{eval_demo}", "source": "ctf-demo"}),
                }],
                usage_tokens: 10,
            },
            LlmResponse {
                text: "list recorded flags".into(),
                tool_calls: vec![ToolCall {
                    id: "c2".into(),
                    tool: "flag_list".into(),
                    args: json!({}),
                }],
                usage_tokens: 10,
            },
            LlmResponse {
                text: "ctf demo complete".into(),
                tool_calls: Vec::new(),
                usage_tokens: 10,
            },
        ],
    }
}

fn replay_responses(events: &[msk_events::Event]) -> Result<Vec<LlmResponse>> {
    let mut responses = Vec::new();
    for event in events {
        if event.kind != "llm.response" {
            continue;
        }
        let text = event
            .data
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let calls = event
            .data
            .get("calls")
            .cloned()
            .context("llm.response event has no recorded tool calls")?;
        let tool_calls: Vec<msk_core::ToolCall> =
            serde_json::from_value(calls).context("decode recorded tool calls")?;
        let usage_tokens = event
            .data
            .get("usage_tokens")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        responses.push(LlmResponse {
            text,
            tool_calls,
            usage_tokens,
        });
    }
    Ok(responses)
}

fn tool_data(
    client: &mut dyn EnvTransport,
    name: &str,
    args: Value,
    sink: &dyn EventSink,
) -> Result<Value> {
    emit(sink, "tool.call", json!({"tool": name, "args": args}))?;
    let result = client.call_tool(name, args)?;
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let structured = result
        .get("structuredContent")
        .cloned()
        .unwrap_or(Value::Null);
    emit(
        sink,
        "tool.result",
        json!({"tool": name, "is_error": is_error, "structured": structured}),
    )?;
    if is_error {
        bail!("tool {name} failed: {structured}");
    }
    let ok = structured
        .get("ok")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !ok {
        bail!("tool {name} returned not ok: {structured}");
    }
    Ok(structured.get("data").cloned().unwrap_or(Value::Null))
}

fn emit(sink: &dyn EventSink, kind: &str, data: Value) -> Result<()> {
    sink.emit(&Event::new(kind, "kernel", data))?;
    Ok(())
}
