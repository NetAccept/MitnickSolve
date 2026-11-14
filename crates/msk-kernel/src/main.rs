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
use msk_mcp::EnvClient;
use msk_wasm::ToolHost;
use runtime::{
    EnvHost, build_observation, complete_or_stream, extract_data, llm_request, load_registry,
    load_registry_with_policy, substitute,
};
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Demo {
    Session,
    Echo,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ProviderKind {
    Scripted,
    Openai,
    OpenaiResponses,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
enum Approvals {
    Allow,
    Ask,
}

#[derive(Debug, Parser)]
struct Cli {
    /// Path to the envd binary built from tools/envd.
    #[arg(long)]
    envd: PathBuf,
    /// Kernel state directory (events, envd data).
    #[arg(long, default_value = ".msk")]
    root: PathBuf,
    /// Directory containing tool manifests and WAT modules.
    #[arg(long, default_value = "tools/wasm")]
    tools_dir: PathBuf,
    /// Fallback module for tools without a matching <tool>.wat.
    #[arg(long, default_value = "tools/wasm/exec.wat")]
    wasm: PathBuf,
    /// Scripted agent demo to run.
    #[arg(long, value_enum, default_value_t = Demo::Session)]
    demo: Demo,
    /// Keep one agent session alive and read user messages from stdin.
    /// Without this flag the binary runs the one-shot scripted demo.
    #[arg(long)]
    session: bool,
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
    /// Approximate token budget for the agent context.
    #[arg(long, default_value_t = 100_000)]
    max_context_tokens: usize,
    /// Minimum recent observations kept during compaction.
    #[arg(long, default_value_t = 4)]
    min_recent_observations: usize,
    /// LLM provider backend.
    #[arg(long, value_enum, default_value_t = ProviderKind::Scripted)]
    provider: ProviderKind,
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

fn main() -> Result<()> {
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
            "mode": if cli.session || cli.stdio_json { "session" } else { "demo" },
            "task_id": cli.task_id,
            "demo": format!("{:?}", cli.demo)
        }),
    )?;

    if cli.session || cli.stdio_json {
        return run_session(&cli, sink, &events_path);
    }

    let envd_bin = cli.envd.canonicalize().unwrap_or_else(|_| cli.envd.clone());
    if !envd_bin.exists() {
        bail!(
            "envd binary not found: {} — build it with `cd tools/envd && go build -o ../../target/debug/envd ./cmd/envd`",
            envd_bin.display()
        );
    }
    if !cli.tools_dir.exists() {
        bail!("tool directory not found: {}", cli.tools_dir.display());
    }
    if !cli.wasm.exists() {
        bail!("fallback WASM module not found: {}", cli.wasm.display());
    }

    let envd_root = cli.root.join("envd");
    std::fs::create_dir_all(&envd_root)?;
    let mut client = EnvClient::spawn(&envd_bin, &envd_root).context("spawn embedded envd")?;
    let ack = client.handshake("agent-local").context("envd handshake")?;
    println!(
        "envd: {} (protocol {}, max_frame {})",
        ack.server, ack.protocol, ack.max_frame
    );
    emit(
        sink.as_ref(),
        "envd.hello",
        json!({"server": ack.server, "protocol": ack.protocol, "max_frame": ack.max_frame}),
    )?;

    let init = client.initialize().context("MCP initialize")?;
    emit(sink.as_ref(), "mcp.initialize", init)?;
    let tools = client.request("tools/list", json!({}))?;
    let tool_count = tools
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    println!("envd tools: {tool_count}");
    emit(sink.as_ref(), "mcp.tools", json!({"count": tool_count}))?;

    let env = tool_data(
        &mut client,
        "env_create",
        json!({"name": "demo", "kind": "managed", "profile": "dev"}),
        sink.as_ref(),
    )?;
    let env_id = env
        .get("env_id")
        .and_then(Value::as_str)
        .context("env_create did not return env_id")?
        .to_string();
    println!(
        "env: {env_id} at {}",
        env.get("workspace").and_then(Value::as_str).unwrap_or("?")
    );
    emit(
        sink.as_ref(),
        "env.create",
        json!({"env_id": env_id, "workspace": env.get("workspace")}),
    )?;

    let content = b"hello from msk\n";
    let write = tool_data(
        &mut client,
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
        &mut client,
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
        &mut client,
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
    };
    let registry = load_registry(&cli.tools_dir, &cli.wasm, host, tool_names)?;
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

    let provider: Box<dyn LlmProvider> = match cli.provider {
        ProviderKind::Scripted => {
            Box::new(ScriptedProvider::new("scripted", demo_responses(cli.demo)))
        }
        ProviderKind::Openai => Box::new(
            OpenAiProvider::new(openai_config(&cli)?).context("configure OpenAI provider")?,
        ),
        ProviderKind::OpenaiResponses => Box::new(
            OpenAiResponsesProvider::new(openai_config(&cli)?)
                .context("configure OpenAI Responses provider")?,
        ),
    };
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
    let envd_bin = cli.envd.canonicalize().unwrap_or_else(|_| cli.envd.clone());
    if !envd_bin.exists() {
        bail!(
            "envd binary not found: {} — build it with `cd tools/envd && go build -o ../../target/debug/envd ./cmd/envd`",
            envd_bin.display()
        );
    }
    if !cli.tools_dir.exists() {
        bail!("tool directory not found: {}", cli.tools_dir.display());
    }
    if !cli.wasm.exists() {
        bail!("fallback WASM module not found: {}", cli.wasm.display());
    }

    let envd_root = cli.root.join("envd");
    std::fs::create_dir_all(&envd_root)?;
    let mut client = EnvClient::spawn(&envd_bin, &envd_root).context("spawn embedded envd")?;
    let ack = client.handshake("agent-local").context("envd handshake")?;
    human_line(
        cli,
        &format!(
            "envd: {} (protocol {}, max_frame {})",
            ack.server, ack.protocol, ack.max_frame
        ),
    );
    emit(
        sink.as_ref(),
        "envd.hello",
        json!({"server": ack.server, "protocol": ack.protocol, "max_frame": ack.max_frame}),
    )?;

    let init = client.initialize().context("MCP initialize")?;
    emit(sink.as_ref(), "mcp.initialize", init)?;
    let tools = client.request("tools/list", json!({}))?;
    let tool_count = tools
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    human_line(cli, &format!("envd tools: {tool_count}"));
    emit(sink.as_ref(), "mcp.tools", json!({"count": tool_count}))?;

    let env = tool_data(
        &mut client,
        "env_create",
        json!({"name": "session", "kind": "managed", "profile": "dev"}),
        sink.as_ref(),
    )?;
    let env_id = env
        .get("env_id")
        .and_then(Value::as_str)
        .context("env_create did not return env_id")?
        .to_string();
    human_line(cli, &format!("env: {env_id}"));
    emit(
        sink.as_ref(),
        "env.create",
        json!({"env_id": env_id, "workspace": env.get("workspace")}),
    )?;

    let object_store: Arc<dyn ObjectStore> =
        Arc::new(FileObjectStore::open(cli.root.join("artifacts")).context("open artifact store")?);
    let client = Arc::new(Mutex::new(client));
    let env_host = Arc::new(EnvHost {
        client,
        artifacts: object_store.clone(),
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
        "echo",
    ];
    let registry_policy = policy::policy_with_approvals(cli.approvals == Approvals::Ask);
    let registry = load_registry_with_policy(
        &cli.tools_dir,
        &cli.wasm,
        host,
        &tool_names,
        &registry_policy,
    )?;
    emit(
        sink.as_ref(),
        "tool.registry",
        json!({"tools": tool_names, "dir": cli.tools_dir.display().to_string()}),
    )?;

    let limits = ContextLimits::default()
        .with_max_tokens(cli.max_context_tokens)
        .with_min_recent_observations(cli.min_recent_observations);
    let mut context = ContextManager::new(limits.clone());
    context.push_pinned(
        Role::System,
        "You are MitnickSolve, a CTF agent. Use the provided tools for all machine access. \
         Prefer persistent sessions for multi-command work. Tool output over 2 KiB is stored \
         as an artifact; use artifact_read to inspect it. Keep responses concise.",
    );
    context.push_pinned(Role::System, format!("env_id={env_id}"));
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

    let provider = session_provider(cli)?;
    let gate = ApprovalGate::new(cli.stdio_json, Duration::from_secs(300));
    let approvals: Option<Box<dyn ApprovalHandler>> = if cli.approvals == Approvals::Ask {
        Some(Box::new(gate.clone()))
    } else {
        None
    };
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
    });

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

fn session_provider(cli: &Cli) -> Result<Box<dyn LlmProvider>> {
    Ok(match cli.provider {
        ProviderKind::Scripted => Box::new(EchoProvider::new("echo")),
        ProviderKind::Openai => {
            Box::new(OpenAiProvider::new(openai_config(cli)?).context("configure OpenAI provider")?)
        }
        ProviderKind::OpenaiResponses => Box::new(
            OpenAiResponsesProvider::new(openai_config(cli)?)
                .context("configure OpenAI Responses provider")?,
        ),
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
    Ok(OpenAiConfig {
        base_url,
        model,
        api_key,
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
                "type large.txt"
            } else {
                "cat large.txt"
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
    }
}

fn tool_data(
    client: &mut EnvClient,
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
