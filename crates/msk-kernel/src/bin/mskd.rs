use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use clap::Parser;
use msk_events::Event;
use msk_ipc::data::{DATA_MAX_FRAME, DataFrame, DataKind, write_frame};
use msk_ipc::local::{self, Stream};
use msk_ipc::{RpcNotification, RpcRequest, RpcResponse, TryClone, methods};
use msk_kernel::mode::ModeRegistry;
use msk_kernel::policy::PermissionMode;
use msk_mcp::{EnvClient, EnvDataClient, EnvTransport, ServiceClient};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "mskd", version)]
struct Cli {
    /// Kernel state directory containing the socket and token.
    #[arg(long, default_value = ".msk")]
    root: PathBuf,
    /// Optional modes.json schema file. Defaults to $MSK_MODES or <root>/modes.json.
    #[arg(long)]
    modes_file: Option<PathBuf>,
    /// SQLite event index (optional).
    #[arg(long)]
    store: Option<PathBuf>,
    /// Path to the envd binary (embedded mode).
    #[arg(long)]
    envd: Option<PathBuf>,
    /// Remote envd service URL; tasks and the terminal use it instead.
    #[arg(long)]
    envd_url: Option<String>,
    /// Bearer token for --envd-url (defaults to ENVD_TOKEN).
    #[arg(long)]
    envd_token: Option<String>,
    /// envd service data address; enables the binary terminal path.
    #[arg(long)]
    envd_data_addr: Option<String>,
    /// Use TLS 1.3 for --envd-data-addr.
    #[arg(long)]
    envd_data_tls: bool,
    /// TLS server name for the data plane (defaults to the address host).
    #[arg(long)]
    envd_data_tls_name: Option<String>,
    /// PEM CA bundle for the envd HTTPS control plane and data-plane TLS.
    #[arg(long)]
    envd_ca: Option<PathBuf>,
    /// envd workspace profile for tasks and the terminal.
    #[arg(long, default_value = "dev", value_parser = ["dev", "host", "isolated"])]
    envd_profile: String,
    /// Signed llm.provider manifest forwarded to component-provider tasks.
    #[arg(long)]
    component_manifest: Option<PathBuf>,
    /// Trust store for the component manifest.
    #[arg(long)]
    component_trust: Option<PathBuf>,
    /// Out-of-process component host binary.
    #[arg(long)]
    component_host: Option<PathBuf>,
    /// Project directory used by embedded envd workspaces. Defaults to the current directory.
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Directory containing tool manifests and WAT modules.
    #[arg(long, default_value = "tools/wasm")]
    tools_dir: PathBuf,
    /// Optional platform-specific wasmtime precompiled-module cache.
    #[arg(long)]
    wasm_cache: Option<PathBuf>,
    /// Agent binary; defaults to `msk` next to this daemon.
    #[arg(long)]
    agent_bin: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskState {
    Running,
    WaitingInput,
    Done,
    Failed,
    Cancelled,
}

impl TaskState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::WaitingInput => "waiting_input",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

struct Task {
    id: String,
    goal: String,
    demo: String,
    provider: String,
    model: Option<String>,
    effort: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    interactive: bool,
    stream: bool,
    approvals: String,
    permission: String,
    requested_mode: String,
    mode: String,
    mode_reason: String,
    headless: bool,
    agent_depth: u32,
    parent_task: Option<String>,
    forked_from: Option<String>,
    resume_checkpoint: Option<PathBuf>,
    max_steps: usize,
    max_context_tokens: usize,
    state: TaskState,
    dir: PathBuf,
    child: Arc<Mutex<Option<Child>>>,
    input: Arc<Mutex<Option<ChildStdin>>>,
    events: u64,
    turns: u64,
    last_error: Option<String>,
    created_ms: u128,
}

impl Task {
    fn snapshot(&self) -> Value {
        json!({
            "task_id": self.id,
            "goal": self.goal,
            "demo": self.demo,
            "provider": self.provider,
            "model": self.model,
            "effort": self.effort,
            "base_url": self.base_url,
            "interactive": self.interactive,
            "stream": self.stream,
            "approvals": self.approvals,
            "permission": self.permission,
            "requested_mode": self.requested_mode,
            "mode": self.mode,
            "mode_reason": self.mode_reason,
            "headless": self.headless,
            "agent_depth": self.agent_depth,
            "parent_task": self.parent_task,
            "forked_from": self.forked_from,
            "max_steps": self.max_steps,
            "max_context_tokens": self.max_context_tokens,
            "state": self.state.as_str(),
            "dir": self.dir.display().to_string(),
            "events": self.events,
            "turns": self.turns,
            "last_error": self.last_error,
            "created_ms": self.created_ms
        })
    }
}

struct Subscriber {
    id: u64,
    task: Option<String>,
    tx: mpsc::Sender<Value>,
}

struct TerminalSession {
    client: Box<dyn EnvTransport>,
    data: Option<EnvDataClient>,
    session_id: String,
    cols: u16,
    rows: u16,
}

#[derive(Debug, Clone)]
struct PendingApproval {
    id: u64,
    tool: String,
    args: Value,
    reason: String,
    requested_ms: u128,
}

impl PendingApproval {
    fn snapshot(&self, task_id: &str) -> Value {
        json!({
            "task_id": task_id,
            "id": self.id,
            "tool": self.tool,
            "args": self.args,
            "reason": self.reason,
            "requested_ms": self.requested_ms
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BlackboardEntry {
    value: Value,
    version: u64,
    updated_ms: u64,
}

struct Blackboard {
    path: PathBuf,
    entries: BTreeMap<String, BlackboardEntry>,
}

impl Blackboard {
    fn load(path: PathBuf) -> Self {
        let entries = std::fs::read_to_string(&path)
            .ok()
            .map(|text| {
                if let Ok(entries) =
                    serde_json::from_str::<BTreeMap<String, BlackboardEntry>>(&text)
                {
                    return entries;
                }
                // Backward compatibility with the unversioned value map.
                serde_json::from_str::<BTreeMap<String, Value>>(&text)
                    .map(|values| {
                        values
                            .into_iter()
                            .map(|(key, value)| {
                                (
                                    key,
                                    BlackboardEntry {
                                        value,
                                        version: 1,
                                        updated_ms: now_ms() as u64,
                                    },
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        Self { path, entries }
    }

    fn snapshot(key: &str, entry: Option<&BlackboardEntry>) -> Value {
        match entry {
            Some(entry) => json!({
                "key": key,
                "value": entry.value,
                "version": entry.version,
                "updated_ms": entry.updated_ms
            }),
            None => json!({
                "key": key,
                "value": Value::Null,
                "version": 0
            }),
        }
    }

    fn get(&self, key: &str) -> Value {
        Self::snapshot(key, self.entries.get(key))
    }

    fn list(&self) -> Value {
        let entries: Vec<Value> = self
            .entries
            .iter()
            .map(|(key, entry)| Self::snapshot(key, Some(entry)))
            .collect();
        json!({"entries": entries})
    }

    fn set(&mut self, key: &str, value: Value, expected_version: Option<u64>) -> Result<Value> {
        if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
            bail!("blackboard key must be 1..128 printable characters");
        }
        if let Some(expected) = expected_version {
            let current = self.entries.get(key).map_or(0, |entry| entry.version);
            if current != expected {
                return Ok(json!({
                    "ok": false,
                    "conflict": true,
                    "current": Self::snapshot(key, self.entries.get(key))
                }));
            }
        }
        let encoded = serde_json::to_vec(&value)?;
        if encoded.len() > 64 * 1024 {
            bail!("blackboard value exceeds 64 KiB");
        }
        let existing: usize = self
            .entries
            .iter()
            .filter(|(existing_key, _)| existing_key.as_str() != key)
            .map(|(_, entry)| {
                serde_json::to_vec(&entry.value)
                    .map(|bytes| bytes.len())
                    .unwrap_or(0)
            })
            .sum();
        if existing.saturating_add(encoded.len()) > 1 << 20 {
            bail!("blackboard exceeds 1 MiB");
        }
        let version = self
            .entries
            .get(key)
            .map_or(1, |entry| entry.version.saturating_add(1));
        self.entries.insert(
            key.to_string(),
            BlackboardEntry {
                value,
                version,
                updated_ms: now_ms() as u64,
            },
        );
        self.save()?;
        Ok(json!({
            "ok": true,
            "entry": Self::snapshot(key, self.entries.get(key))
        }))
    }

    fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temporary = self
            .path
            .with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&temporary, serde_json::to_vec_pretty(&self.entries)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600));
        }
        #[cfg(windows)]
        {
            let _ = std::fs::remove_file(&self.path);
        }
        std::fs::rename(&temporary, &self.path)?;
        Ok(())
    }
}

#[derive(Default)]
struct Inner {
    tasks: HashMap<String, Task>,
}

struct Daemon {
    root: PathBuf,
    store: Option<Arc<msk_store::SqliteStore>>,
    envd: Option<PathBuf>,
    envd_url: Option<String>,
    envd_token: Option<String>,
    envd_data_addr: Option<String>,
    envd_data_tls: bool,
    envd_data_tls_name: Option<String>,
    envd_ca: Option<PathBuf>,
    envd_profile: String,
    component_manifest: Option<PathBuf>,
    component_trust: Option<PathBuf>,
    component_host: Option<PathBuf>,
    workspace: PathBuf,
    tools_dir: PathBuf,
    wasm_cache: Option<PathBuf>,
    agent_bin: PathBuf,
    token: String,
    modes: Arc<ModeRegistry>,
    modes_file: Option<PathBuf>,
    inner: Mutex<Inner>,
    subscribers: Mutex<Vec<Subscriber>>,
    terminal: Mutex<Option<TerminalSession>>,
    approvals: Mutex<HashMap<String, PendingApproval>>,
    blackboard: Mutex<Blackboard>,
    data_active: AtomicBool,
    next_task: AtomicU64,
    next_sub: AtomicU64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    fs::create_dir_all(&cli.root)?;
    let root = msk_ipc::normalize_root(&cli.root);

    let envd = cli.envd.as_ref().map(|path| msk_ipc::normalize_root(path));
    if envd.is_none() && cli.envd_url.is_none() {
        bail!("either --envd or --envd-url is required");
    }
    if envd.is_some() && cli.envd_url.is_some() {
        bail!("use either --envd or --envd-url, not both");
    }
    if envd.is_some() && cli.envd_profile != "dev" {
        bail!(
            "--envd-profile {} requires --envd-url (service mode)",
            cli.envd_profile
        );
    }
    if let Some(envd) = &envd {
        if !envd.exists() {
            bail!("envd binary not found: {}", envd.display());
        }
    }
    let envd_url = cli.envd_url.clone();
    let envd_token = cli
        .envd_token
        .clone()
        .or_else(|| std::env::var("ENVD_TOKEN").ok());
    let envd_data_addr = cli.envd_data_addr.clone();
    if envd_url.is_some() && envd_token.is_none() {
        bail!("--envd-token or ENVD_TOKEN is required with --envd-url");
    }
    if cli.envd_data_tls && envd_data_addr.is_none() {
        bail!("--envd-data-tls requires --envd-data-addr");
    }
    if cli.envd_data_tls && envd_url.is_none() {
        bail!("--envd-data-tls is only used with --envd-url");
    }
    if cli.envd_ca.is_some() && envd_url.is_none() {
        bail!("--envd-ca requires --envd-url");
    }
    let envd_ca = cli
        .envd_ca
        .as_ref()
        .map(|path| msk_ipc::normalize_root(path));
    if let Some(path) = &envd_ca {
        if !path.exists() {
            bail!("envd CA bundle not found: {}", path.display());
        }
    }
    let component_manifest = cli
        .component_manifest
        .as_ref()
        .map(|path| msk_ipc::normalize_root(path));
    let component_trust = cli
        .component_trust
        .as_ref()
        .map(|path| msk_ipc::normalize_root(path));
    let component_host = cli
        .component_host
        .as_ref()
        .map(|path| msk_ipc::normalize_root(path));
    let modes_file = cli
        .modes_file
        .clone()
        .map(|path| msk_ipc::normalize_root(&path))
        .or_else(|| std::env::var_os("MSK_MODES").map(PathBuf::from))
        .or_else(|| {
            let candidate = root.join("modes.json");
            candidate.exists().then_some(candidate)
        });
    let modes =
        Arc::new(ModeRegistry::load(&root, modes_file.as_deref()).context("load agent modes")?);
    let workspace = cli
        .workspace
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .context("workspace directory")?;
    if !workspace.is_dir() {
        bail!("workspace is not a directory: {}", workspace.display());
    }
    let workspace = msk_ipc::normalize_root(&workspace);
    let tools_dir = msk_ipc::normalize_root(&cli.tools_dir);
    if !tools_dir.exists() {
        bail!("tools directory not found: {}", tools_dir.display());
    }
    let agent_bin = msk_ipc::normalize_root(&cli.agent_bin.unwrap_or(default_agent_bin()?));
    if !agent_bin.exists() {
        bail!(
            "agent binary not found: {} — run `cargo build --bins` first",
            agent_bin.display()
        );
    }

    let token = Uuid::new_v4().to_string();
    msk_ipc::write_token(&root, &token)?;
    let listener = local::listener(&root)?;
    let data_listener = local::data_listener(&root)?;
    println!("mskd listening at {}", local::display_name(&root));
    println!("mskd data at {}", local::data_display_name(&root));
    println!("token file: {}", msk_ipc::token_path(&root).display());
    println!("agent: {}", agent_bin.display());
    if let Some(url) = &envd_url {
        println!("envd service: {url}");
    } else if let Some(envd) = &envd {
        println!("envd: {}", envd.display());
    }

    let blackboard_path = root.join("blackboard.json");
    let store = match &cli.store {
        Some(path) => Some(Arc::new(
            msk_store::SqliteStore::open(msk_ipc::normalize_root(path))
                .context("open event store")?,
        )),
        None => None,
    };
    let daemon = Arc::new(Daemon {
        root,
        store,
        envd,
        envd_url,
        envd_token,
        envd_data_addr,
        envd_data_tls: cli.envd_data_tls,
        envd_data_tls_name: cli.envd_data_tls_name.clone(),
        envd_ca,
        envd_profile: cli.envd_profile.clone(),
        component_manifest,
        component_trust,
        component_host,
        workspace,
        tools_dir,
        wasm_cache: cli.wasm_cache.clone(),
        agent_bin,
        token,
        modes,
        modes_file,
        inner: Mutex::new(Inner::default()),
        subscribers: Mutex::new(Vec::new()),
        terminal: Mutex::new(None),
        approvals: Mutex::new(HashMap::new()),
        blackboard: Mutex::new(Blackboard::load(blackboard_path)),
        data_active: AtomicBool::new(false),
        next_task: AtomicU64::new(1),
        next_sub: AtomicU64::new(1),
    });

    let data_daemon = daemon.clone();
    thread::spawn(move || {
        for stream in local::data_incoming(&data_listener) {
            match stream {
                Ok(stream) => {
                    let daemon = data_daemon.clone();
                    thread::spawn(move || {
                        if let Err(err) = handle_data_client(stream, daemon) {
                            eprintln!("mskd data client: {err:#}");
                        }
                    });
                }
                Err(err) => eprintln!("mskd data accept: {err}"),
            }
        }
    });

    for stream in local::incoming(&listener) {
        match stream {
            Ok(stream) => {
                let daemon = daemon.clone();
                thread::spawn(move || {
                    if let Err(err) = handle_client(stream, daemon) {
                        eprintln!("mskd client: {err:#}");
                    }
                });
            }
            Err(err) => eprintln!("mskd accept: {err}"),
        }
    }
    Ok(())
}

fn default_agent_bin() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("current executable")?;
    let name = if cfg!(windows) { "msk.exe" } else { "msk" };
    Ok(exe.with_file_name(name))
}

fn handle_client(stream: Stream, daemon: Arc<Daemon>) -> Result<()> {
    let reader_stream = stream.try_clone()?;
    let writer = Arc::new(Mutex::new(stream));
    let (event_tx, event_rx) = mpsc::channel::<Value>();
    let event_writer = writer.clone();
    let notifier = thread::spawn(move || {
        while let Ok(value) = event_rx.recv() {
            let mut writer = match event_writer.lock() {
                Ok(writer) => writer,
                Err(_) => break,
            };
            if msk_ipc::write_json_line(&mut *writer, &value).is_err() {
                break;
            }
        }
    });

    let mut reader = BufReader::new(reader_stream);
    let mut subscriptions = Vec::new();
    let mut authenticated = false;
    while let Some(value) = msk_ipc::read_json_line(&mut reader)? {
        let request: RpcRequest = match serde_json::from_value(value) {
            Ok(request) => request,
            Err(err) => {
                eprintln!("mskd bad request: {err}");
                continue;
            }
        };
        let (response, subscription) = daemon.handle(request, &event_tx, &mut authenticated);
        if let Some(id) = subscription {
            subscriptions.push(id);
        }
        let value = serde_json::to_value(response)?;
        let mut writer = writer
            .lock()
            .map_err(|_| anyhow::anyhow!("writer poisoned"))?;
        msk_ipc::write_json_line(&mut *writer, &value)?;
    }

    for id in subscriptions {
        daemon.unsubscribe(id);
    }
    drop(event_tx);
    let _ = notifier.join();
    Ok(())
}

impl Daemon {
    fn handle(
        self: &Arc<Self>,
        request: RpcRequest,
        tx: &mpsc::Sender<Value>,
        authenticated: &mut bool,
    ) -> (RpcResponse, Option<u64>) {
        let id = request.id;
        let method = request.method;
        let params = request.params;
        if method != methods::HELLO && !*authenticated {
            return (RpcResponse::err(id, -32001, "unauthorized"), None);
        }
        match method.as_str() {
            methods::HELLO => {
                let token = params
                    .get("token")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !token_eq(token, &self.token) {
                    *authenticated = false;
                    return (RpcResponse::err(id, -32001, "unauthorized"), None);
                }
                *authenticated = true;
                (
                    RpcResponse::ok(
                        id,
                        json!({
                            "protocol": msk_ipc::IPC_PROTOCOL_VERSION,
                            "server": "mskd",
                            "capabilities": ["tasks", "events", "sessions", "approvals", "terminal"],
                            "workspace": self.workspace.display().to_string(),
                            "modes": self.modes.infos(),
                            "default_mode": "auto",
                            "llm": {
                                "api_key": std::env::var("MSK_LLM_API_KEY").is_ok(),
                                "model": std::env::var("MSK_LLM_MODEL").ok(),
                                "base_url": std::env::var("MSK_LLM_BASE_URL").ok(),
                                "effort": std::env::var("MSK_LLM_EFFORT").ok()
                            }
                        }),
                    ),
                    None,
                )
            }
            methods::TASK_CREATE => {
                let goal = params
                    .get("goal")
                    .and_then(Value::as_str)
                    .unwrap_or("run the agent demo")
                    .to_string();
                let demo = params
                    .get("demo")
                    .and_then(Value::as_str)
                    .unwrap_or("session")
                    .to_string();
                let provider = params
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or("scripted")
                    .to_string();
                let model = params
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                let effort = params
                    .get("effort")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty() && *value != "default")
                    .map(str::to_string);
                if let Some(effort) = &effort
                    && !matches!(effort.as_str(), "minimal" | "low" | "medium" | "high")
                {
                    return (
                        RpcResponse::err(id, -32602, format!("unknown effort: {effort}")),
                        None,
                    );
                }
                let base_url = params
                    .get("base_url")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                let resume_checkpoint = params
                    .get("resume_checkpoint")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let api_key = params
                    .get("api_key")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string);
                let interactive = params
                    .get("interactive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let stream = params
                    .get("stream")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let approvals = params
                    .get("approvals")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        if interactive {
                            "ask".to_string()
                        } else {
                            "allow".to_string()
                        }
                    });
                let permission = params
                    .get("permission")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| match approvals.as_str() {
                        "ask" => "manual".to_string(),
                        "allow" => "bypass".to_string(),
                        _ => "auto".to_string(),
                    });
                if PermissionMode::parse(&permission).is_none() {
                    return (
                        RpcResponse::err(id, -32602, format!("unknown permission: {permission}")),
                        None,
                    );
                }
                let requested_mode = params
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("auto")
                    .to_string();
                if requested_mode != "auto" && self.modes.spec(&requested_mode).is_none() {
                    return (
                        RpcResponse::err(id, -32602, format!("unknown mode: {requested_mode}")),
                        None,
                    );
                }
                let headless = params
                    .get("headless")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let agent_depth = params
                    .get("agent_depth")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .min(8) as u32;
                let max_steps = params
                    .get("max_steps")
                    .and_then(Value::as_u64)
                    .unwrap_or(12)
                    .clamp(1, 50) as usize;
                let max_context_tokens = params
                    .get("max_context_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(100_000)
                    .clamp(4_096, 1_000_000) as usize;
                let parent_task = params
                    .get("parent_task")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let forked_from = params
                    .get("fork_from")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                match self.create_task(
                    goal,
                    demo,
                    provider,
                    model,
                    effort,
                    base_url,
                    api_key,
                    interactive,
                    stream,
                    approvals,
                    permission,
                    requested_mode,
                    headless,
                    agent_depth,
                    max_steps,
                    max_context_tokens,
                    parent_task,
                    forked_from,
                    resume_checkpoint,
                ) {
                    Ok(snapshot) => (RpcResponse::ok(id, snapshot), None),
                    Err(err) => (RpcResponse::err(id, -32000, err.to_string()), None),
                }
            }
            methods::TASK_MESSAGE => {
                let task_id = params
                    .get("task_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let text = params
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match self.message_task(task_id, text) {
                    Ok(snapshot) => (RpcResponse::ok(id, snapshot), None),
                    Err(err) => (RpcResponse::err(id, -32002, err.to_string()), None),
                }
            }
            methods::TASK_LIST => {
                let mut tasks: Vec<Value> = self
                    .inner
                    .lock()
                    .map(|inner| inner.tasks.values().map(Task::snapshot).collect())
                    .unwrap_or_default();
                tasks.sort_by(|left, right| {
                    left["task_id"]
                        .as_str()
                        .unwrap_or("")
                        .cmp(right["task_id"].as_str().unwrap_or(""))
                });
                (RpcResponse::ok(id, json!({"tasks": tasks})), None)
            }
            methods::TASK_STATE => {
                let task_id = params
                    .get("task_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let snapshot = self
                    .inner
                    .lock()
                    .ok()
                    .and_then(|inner| inner.tasks.get(task_id).map(Task::snapshot));
                match snapshot {
                    Some(snapshot) => (RpcResponse::ok(id, snapshot), None),
                    None => (RpcResponse::err(id, -32002, "task not found"), None),
                }
            }
            methods::TASK_HISTORY => match self.task_history(&params) {
                Ok(value) => (RpcResponse::ok(id, value), None),
                Err(err) => (RpcResponse::err(id, -32002, err.to_string()), None),
            },
            methods::TASK_RESULT => {
                let task_id = params
                    .get("task_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match self.task_result(task_id) {
                    Ok(value) => (RpcResponse::ok(id, value), None),
                    Err(err) => (RpcResponse::err(id, -32002, err.to_string()), None),
                }
            }
            methods::BLACKBOARD_GET => {
                let key = params
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let value = self
                    .blackboard
                    .lock()
                    .map(|blackboard| blackboard.get(key))
                    .unwrap_or(Value::Null);
                (RpcResponse::ok(id, value), None)
            }
            methods::BLACKBOARD_SET => {
                let key = params
                    .get("key")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let value = params.get("value").cloned().unwrap_or(Value::Null);
                let expected_version = params.get("expected_version").and_then(Value::as_u64);
                let result = self
                    .blackboard
                    .lock()
                    .map_err(|_| anyhow::anyhow!("blackboard poisoned"))
                    .and_then(|mut blackboard| blackboard.set(key, value, expected_version));
                match result {
                    Ok(value) => (RpcResponse::ok(id, value), None),
                    Err(err) => (RpcResponse::err(id, -32000, err.to_string()), None),
                }
            }
            methods::BLACKBOARD_LIST => {
                let value = self
                    .blackboard
                    .lock()
                    .map(|blackboard| blackboard.list())
                    .unwrap_or_else(|_| json!({"entries": []}));
                (RpcResponse::ok(id, value), None)
            }
            methods::TASK_CANCEL => {
                let task_id = params
                    .get("task_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match self.cancel_task(task_id) {
                    Ok(snapshot) => (RpcResponse::ok(id, snapshot), None),
                    Err(err) => (RpcResponse::err(id, -32002, err.to_string()), None),
                }
            }
            methods::TASK_INTERRUPT => {
                let task_id = params
                    .get("task_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                match self.interrupt_task(task_id) {
                    Ok(snapshot) => (RpcResponse::ok(id, snapshot), None),
                    Err(err) => (RpcResponse::err(id, -32002, err.to_string()), None),
                }
            }
            methods::EVENTS_QUERY => match self.event_query(&params) {
                Ok(value) => (RpcResponse::ok(id, value), None),
                Err(err) => (RpcResponse::err(id, -32000, err.to_string()), None),
            },
            methods::EVENTS_SUBSCRIBE => {
                let task = params
                    .get("task_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                let subscriber_id = self.next_sub.fetch_add(1, Ordering::SeqCst);
                if let Ok(mut subscribers) = self.subscribers.lock() {
                    subscribers.push(Subscriber {
                        id: subscriber_id,
                        task,
                        tx: tx.clone(),
                    });
                }
                (
                    RpcResponse::ok(id, json!({"subscriber_id": subscriber_id})),
                    Some(subscriber_id),
                )
            }
            methods::APPROVAL_LIST => (
                RpcResponse::ok(id, json!({"approvals": self.approval_snapshots()})),
                None,
            ),
            methods::APPROVAL_RESPOND => match self.respond_approval(&params) {
                Ok(value) => (RpcResponse::ok(id, value), None),
                Err(err) => (RpcResponse::err(id, -32002, err.to_string()), None),
            },
            methods::TERMINAL_OPEN => match self.terminal_open(&params) {
                Ok(value) => (RpcResponse::ok(id, value), None),
                Err(err) => (RpcResponse::err(id, -32000, err.to_string()), None),
            },
            methods::TERMINAL_WRITE => match self.terminal_write(&params) {
                Ok(value) => (RpcResponse::ok(id, value), None),
                Err(err) => (RpcResponse::err(id, -32000, err.to_string()), None),
            },
            methods::TERMINAL_READ => match self.terminal_read(&params) {
                Ok(value) => (RpcResponse::ok(id, value), None),
                Err(err) => (RpcResponse::err(id, -32000, err.to_string()), None),
            },
            methods::TERMINAL_RESIZE => match self.terminal_resize(&params) {
                Ok(value) => (RpcResponse::ok(id, value), None),
                Err(err) => (RpcResponse::err(id, -32000, err.to_string()), None),
            },
            methods::TERMINAL_CLOSE => match self.terminal_close(&params) {
                Ok(value) => (RpcResponse::ok(id, value), None),
                Err(err) => (RpcResponse::err(id, -32000, err.to_string()), None),
            },
            other => (
                RpcResponse::err(id, -32601, format!("method not found: {other}")),
                None,
            ),
        }
    }

    fn create_task(
        self: &Arc<Self>,
        goal: String,
        demo: String,
        provider: String,
        model: Option<String>,
        effort: Option<String>,
        base_url: Option<String>,
        api_key: Option<String>,
        interactive: bool,
        stream: bool,
        approvals: String,
        permission: String,
        requested_mode: String,
        headless: bool,
        agent_depth: u32,
        max_steps: usize,
        max_context_tokens: usize,
        parent_task: Option<String>,
        forked_from: Option<String>,
        resume_checkpoint: Option<String>,
    ) -> Result<Value> {
        if !matches!(approvals.as_str(), "allow" | "ask") {
            bail!("approvals must be allow or ask");
        }
        let requested = if requested_mode.trim().is_empty() {
            "auto".to_string()
        } else {
            requested_mode.trim().to_string()
        };
        let decision = self
            .modes
            .resolve(&requested, &goal)
            .with_context(|| format!("resolve mode {requested}"))?;
        let mut resolved_mode = decision.mode.clone();
        let mode_reason = decision.reason.clone();
        // The scripted demo drives shell/session tools; a read-only mode would
        // leave it without the tools it was built to exercise.
        if demo == "session" && matches!(provider.as_str(), "scripted" | "scripted-demo") {
            if self.modes.spec("exploit").is_some() {
                resolved_mode = "exploit".into();
            } else {
                bail!("scripted session demo requires an 'exploit' mode in modes.json");
            }
        }
        // A resumed session may contain shell/tool history; default auto to
        // the active mode unless the caller chose a concrete mode.
        if resume_checkpoint.is_some() && requested == "auto" {
            if self.modes.spec("exploit").is_some() {
                resolved_mode = "exploit".into();
            }
        }
        if provider == "component" && self.component_manifest.is_none() {
            bail!("component provider is not configured on this daemon");
        }
        let id = format!(
            "task-{}-{}",
            now_ms(),
            self.next_task.fetch_add(1, Ordering::SeqCst)
        );
        let dir = self.root.join("tasks").join(&id);
        fs::create_dir_all(&dir)?;
        let resume_checkpoint = if let Some(source) = resume_checkpoint {
            let source = fs::canonicalize(&source)
                .with_context(|| format!("resume checkpoint {}", source))?;
            let tasks_root = fs::canonicalize(self.root.join("tasks"))?;
            if !source.starts_with(&tasks_root) || !source.is_file() {
                bail!("resume checkpoint must be a file under the task root");
            }
            let target = dir.join("checkpoint.json");
            fs::copy(&source, &target)?;
            if let Some(source_events) = source.parent().map(|parent| parent.join("events.jsonl"))
                && source_events.exists()
            {
                let _ = fs::copy(source_events, dir.join("events.jsonl"));
            }
            Some(target)
        } else {
            None
        };
        let child = Arc::new(Mutex::new(None));
        let input = Arc::new(Mutex::new(None));
        let task = Task {
            id: id.clone(),
            goal,
            demo,
            provider,
            model,
            effort,
            base_url,
            api_key,
            interactive,
            stream,
            approvals,
            permission,
            requested_mode: requested.as_str().to_string(),
            mode: resolved_mode.as_str().to_string(),
            mode_reason,
            headless,
            agent_depth,
            parent_task,
            forked_from,
            resume_checkpoint,
            max_steps,
            max_context_tokens,
            state: TaskState::Running,
            dir,
            child: child.clone(),
            input: input.clone(),
            events: 0,
            turns: 0,
            last_error: None,
            created_ms: now_ms(),
        };
        let snapshot = task.snapshot();
        {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("task map poisoned"))?;
            if let Some(source) = &task.forked_from {
                if !inner.tasks.contains_key(source) {
                    bail!("fork source task not found: {source}");
                }
            }
            if let Some(parent) = &task.parent_task {
                let mut children = 0usize;
                let mut subagents = 0usize;
                for existing in inner.tasks.values() {
                    let active =
                        matches!(existing.state, TaskState::Running | TaskState::WaitingInput);
                    if !active {
                        continue;
                    }
                    if existing.parent_task.is_some() {
                        subagents += 1;
                    }
                    if existing.parent_task.as_deref() == Some(parent.as_str()) {
                        children += 1;
                    }
                }
                if children >= 2 {
                    bail!("parent {parent} already has 2 active subagents");
                }
                if subagents >= 8 {
                    bail!("global subagent limit reached (8)");
                }
            }
            inner.tasks.insert(id.clone(), task);
        }
        self.broadcast_state(&id, &snapshot);
        let daemon = self.clone();
        thread::spawn(move || daemon.run_task(id));
        Ok(snapshot)
    }

    fn run_task(self: Arc<Self>, task_id: String) {
        let (
            dir,
            goal,
            demo,
            provider,
            model,
            effort,
            base_url,
            api_key,
            interactive,
            stream,
            permission,
            mode,
            headless,
            agent_depth,
            forked_from,
            resume_checkpoint,
            max_steps,
            max_context_tokens,
            child_handle,
            input_handle,
        ) = {
            let inner = match self.inner.lock() {
                Ok(inner) => inner,
                Err(_) => return,
            };
            match inner.tasks.get(&task_id) {
                Some(task) => (
                    task.dir.clone(),
                    task.goal.clone(),
                    task.demo.clone(),
                    task.provider.clone(),
                    task.model.clone(),
                    task.effort.clone(),
                    task.base_url.clone(),
                    task.api_key.clone(),
                    task.interactive,
                    task.stream,
                    task.permission.clone(),
                    task.mode.clone(),
                    task.headless,
                    task.agent_depth,
                    task.forked_from.clone(),
                    task.resume_checkpoint.clone(),
                    task.max_steps,
                    task.max_context_tokens,
                    task.child.clone(),
                    task.input.clone(),
                ),
                None => return,
            }
        };

        let resume_checkpoint = if resume_checkpoint.is_some() {
            resume_checkpoint
        } else if let Some(source) = &forked_from {
            let source_dir = self
                .inner
                .lock()
                .ok()
                .and_then(|inner| inner.tasks.get(source).map(|task| task.dir.clone()));
            let Some(source_dir) = source_dir else {
                self.finish_task(
                    &task_id,
                    TaskState::Failed,
                    Some(format!("fork source task not found: {source}")),
                );
                return;
            };
            let source_checkpoint = source_dir.join("checkpoint.json");
            if !source_checkpoint.exists() {
                self.finish_task(
                    &task_id,
                    TaskState::Failed,
                    Some(format!("fork source has no checkpoint: {source}")),
                );
                return;
            }
            let target = dir.join("checkpoint.json");
            if let Err(err) = std::fs::copy(&source_checkpoint, &target) {
                self.finish_task(
                    &task_id,
                    TaskState::Failed,
                    Some(format!("copy fork checkpoint: {err}")),
                );
                return;
            }
            Some(target)
        } else {
            None
        };

        let stderr = match OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("agent.stderr.log"))
        {
            Ok(file) => file,
            Err(err) => {
                self.finish_task(&task_id, TaskState::Failed, Some(err.to_string()));
                return;
            }
        };

        let mut command = Command::new(&self.agent_bin);
        if let Some(url) = &self.envd_url {
            command
                .arg("--envd-url")
                .arg(url)
                .arg("--envd-token")
                .arg(self.envd_token.as_deref().unwrap_or_default());
            if let Some(ca) = &self.envd_ca {
                command.arg("--envd-ca").arg(ca);
            }
        } else if let Some(envd) = &self.envd {
            command.arg("--envd").arg(envd);
        }
        command
            .arg("--root")
            .arg(&dir)
            .arg("--workspace")
            .arg(&self.workspace)
            .arg("--tools-dir")
            .arg(&self.tools_dir)
            .arg("--wasm")
            .arg(self.tools_dir.join("exec.wat"));
        if let Some(cache) = &self.wasm_cache {
            command.arg("--wasm-cache").arg(cache);
        }
        if let Some(modes_file) = &self.modes_file {
            command.arg("--modes-file").arg(modes_file);
        }
        command.arg("--provider").arg(&provider);
        command.arg("--mode").arg(&mode);
        command.arg("--permission").arg(&permission);
        if let Some(model) = &model {
            command.arg("--llm-model").arg(model);
        }
        if let Some(effort) = &effort {
            command.arg("--llm-effort").arg(effort);
        }
        if let Some(base_url) = &base_url {
            command.arg("--llm-base-url").arg(base_url);
        }
        if let Some(api_key) = &api_key {
            command.env("MSK_LLM_API_KEY", api_key);
        }
        command
            .arg("--control-root")
            .arg(&self.root)
            .arg("--task-id")
            .arg(&task_id)
            .arg("--agent-depth")
            .arg(agent_depth.to_string())
            .arg("--max-steps")
            .arg(max_steps.to_string())
            .arg("--max-context-tokens")
            .arg(max_context_tokens.to_string())
            .arg("--envd-profile")
            .arg(&self.envd_profile);
        if let Some(checkpoint) = &resume_checkpoint {
            command.arg("--resume-checkpoint").arg(checkpoint);
        }
        if forked_from.is_some() {
            // A fork explores a branch with a fresh environment; it must not
            // share the parent's shells or filesystem state.
            command.arg("--fresh-env");
        }
        if provider == "component" {
            if let Some(manifest) = &self.component_manifest {
                command.arg("--component-manifest").arg(manifest);
            }
            if let Some(trust) = &self.component_trust {
                command.arg("--component-trust").arg(trust);
            }
            if let Some(host) = &self.component_host {
                command.arg("--component-host").arg(host);
            }
        }
        if interactive {
            command
                .arg("--session")
                .arg("--demo")
                .arg(&demo)
                .arg("--stdio-json")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            if stream {
                command.arg("--stream");
            }
        } else if headless {
            command
                .arg("--headless")
                .arg("--demo")
                .arg(&demo)
                .arg("--goal")
                .arg(&goal)
                .stdout(Stdio::null());
        } else {
            command
                .arg("--demo")
                .arg(&demo)
                .arg("--goal")
                .arg(&goal)
                .stdout(Stdio::null());
        }
        command.stderr(Stdio::from(stderr));

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                self.finish_task(&task_id, TaskState::Failed, Some(err.to_string()));
                return;
            }
        };

        if interactive {
            let stdout = match child.stdout.take() {
                Some(stdout) => stdout,
                None => {
                    let _ = child.kill();
                    self.finish_task(
                        &task_id,
                        TaskState::Failed,
                        Some("agent stdout unavailable".into()),
                    );
                    return;
                }
            };
            if let Ok(mut slot) = input_handle.lock() {
                *slot = child.stdin.take();
            }
            if let Ok(mut slot) = child_handle.lock() {
                *slot = Some(child);
            }
            let daemon = self.clone();
            let reader_task = task_id.clone();
            thread::spawn(move || daemon.read_agent_stdout(&reader_task, stdout));
            self.set_state(&task_id, TaskState::WaitingInput, None);
        } else if let Ok(mut slot) = child_handle.lock() {
            *slot = Some(child);
        }

        let event_path = dir.join("events.jsonl");
        let mut offset = 0u64;
        loop {
            offset = self.tail_events(&task_id, &event_path, offset);

            let status = {
                let mut slot = match child_handle.lock() {
                    Ok(slot) => slot,
                    Err(_) => {
                        self.finish_task(&task_id, TaskState::Failed, None);
                        return;
                    }
                };
                match slot.as_mut() {
                    Some(child) => match child.try_wait() {
                        Ok(Some(status)) => Some(Ok(status)),
                        Ok(None) => None,
                        Err(err) => Some(Err(err.to_string())),
                    },
                    None => None,
                }
            };

            if let Some(result) = status {
                let (cancelled, last_error) = self
                    .inner
                    .lock()
                    .ok()
                    .and_then(|inner| {
                        inner.tasks.get(&task_id).map(|task| {
                            (task.state == TaskState::Cancelled, task.last_error.clone())
                        })
                    })
                    .unwrap_or((false, None));
                let mut error = result.as_ref().err().cloned();
                let state = if cancelled {
                    TaskState::Cancelled
                } else {
                    match &result {
                        Ok(status) if status.success() && !interactive => TaskState::Done,
                        Ok(status) if status.success() => {
                            // A session that exited after an error is still a
                            // failed task, even if the process returned 0.
                            if last_error.is_some() {
                                error = last_error;
                                TaskState::Failed
                            } else {
                                TaskState::Done
                            }
                        }
                        _ => TaskState::Failed,
                    }
                };
                if let Ok(mut slot) = child_handle.lock() {
                    *slot = None;
                }
                if let Ok(mut slot) = input_handle.lock() {
                    *slot = None;
                }
                self.finish_task(&task_id, state, error);
                break;
            }

            thread::sleep(Duration::from_millis(100));
        }
    }

    /// Reads the strict stdout protocol of an interactive agent. Anything
    /// that is not protocol JSON is logged to the daemon's stderr instead of
    /// being forwarded, so a stray `println!` cannot corrupt the channel.
    fn read_agent_stdout(&self, task_id: &str, stdout: std::process::ChildStdout) {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let line = match line {
                Ok(line) => line,
                Err(_) => break,
            };
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(err) => {
                    eprintln!("mskd task {task_id}: non-protocol stdout ({err}): {line}");
                    continue;
                }
            };
            match value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "agent.text" => {
                    let notification = serde_json::to_value(RpcNotification::new(
                        "agent.text",
                        json!({
                            "task_id": task_id,
                            "text": value.get("text").cloned().unwrap_or(Value::Null),
                            "delta": value.get("delta").and_then(Value::as_bool).unwrap_or(false)
                        }),
                    ))
                    .unwrap_or(Value::Null);
                    self.broadcast(notification, task_id);
                }
                "turn.end" => {
                    let status = value
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("awaiting_input");
                    let message = value
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let snapshot = self.record_turn_end(task_id, status, message.clone());
                    let notification = serde_json::to_value(RpcNotification::new(
                        "turn.end",
                        json!({
                            "task_id": task_id,
                            "status": status,
                            "turns": snapshot.as_ref().and_then(|snapshot| snapshot.get("turns")).cloned().unwrap_or(Value::Null),
                            "message": message
                        }),
                    ))
                    .unwrap_or(Value::Null);
                    self.broadcast(notification, task_id);
                }
                "error" => {
                    eprintln!("mskd task {task_id}: agent protocol error: {value}");
                }
                "approval.request" => {
                    self.record_approval_request(task_id, &value);
                }
                "pong" => {}
                other => {
                    eprintln!("mskd task {task_id}: unknown protocol frame: {other}");
                }
            }
        }
    }

    fn interrupt_task(&self, task_id: &str) -> Result<Value> {
        let (input, snapshot, running) = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("task map poisoned"))?;
            let task = inner
                .tasks
                .get(task_id)
                .ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))?;
            if !task.interactive {
                bail!("task {task_id} is not interactive");
            }
            match task.state {
                TaskState::WaitingInput => (None, task.snapshot(), false),
                TaskState::Running => (Some(task.input.clone()), task.snapshot(), true),
                TaskState::Done | TaskState::Failed => bail!("task {task_id} has exited"),
                TaskState::Cancelled => bail!("task {task_id} is cancelled"),
            }
        };

        if !running {
            let mut snapshot = snapshot;
            snapshot["interrupted"] = json!(false);
            return Ok(snapshot);
        }

        let write_result = (|| -> Result<()> {
            let input =
                input.ok_or_else(|| anyhow::anyhow!("task {task_id} stdin is not available"))?;
            let mut slot = input
                .lock()
                .map_err(|_| anyhow::anyhow!("task input poisoned"))?;
            let stdin = slot
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("task {task_id} stdin is not available"))?;
            writeln!(stdin, "{}", json!({"type": "interrupt"}))?;
            stdin.flush()?;
            Ok(())
        })();
        if let Err(err) = write_result {
            self.set_state(task_id, TaskState::Failed, Some(err.to_string()));
            return Err(err);
        }
        self.clear_approval(task_id);
        let mut snapshot = snapshot;
        snapshot["interrupted"] = json!(true);
        Ok(snapshot)
    }

    fn message_task(&self, task_id: &str, text: &str) -> Result<Value> {
        if text.trim().is_empty() {
            bail!("message text is empty");
        }
        let (input, snapshot) = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("task map poisoned"))?;
            let task = inner
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))?;
            if !task.interactive {
                bail!("task {task_id} is not interactive");
            }
            match task.state {
                TaskState::Running => bail!("task {task_id} is still running a turn"),
                TaskState::Done | TaskState::Failed => bail!("task {task_id} has exited"),
                TaskState::Cancelled => bail!("task {task_id} is cancelled"),
                TaskState::WaitingInput => {}
            }
            task.state = TaskState::Running;
            task.last_error = None;
            (task.input.clone(), task.snapshot())
        };

        let line = serde_json::to_string(&json!({"type": "user", "text": text}))?;
        let write_result = (|| -> Result<()> {
            let mut slot = input
                .lock()
                .map_err(|_| anyhow::anyhow!("task input poisoned"))?;
            let Some(stdin) = slot.as_mut() else {
                drop(slot);
                self.set_state(
                    task_id,
                    TaskState::WaitingInput,
                    Some("task stdin is not available".into()),
                );
                bail!("task {task_id} stdin is not available");
            };
            writeln!(stdin, "{line}")?;
            stdin.flush()?;
            Ok(())
        })();
        if let Err(err) = write_result {
            if self
                .inner
                .lock()
                .ok()
                .and_then(|inner| inner.tasks.get(task_id).map(|task| task.state))
                != Some(TaskState::WaitingInput)
            {
                self.set_state(task_id, TaskState::Failed, Some(err.to_string()));
            }
            return Err(err);
        }
        self.broadcast_state(task_id, &snapshot);
        let notification = serde_json::to_value(RpcNotification::new(
            "task.message",
            json!({"task_id": task_id, "text": text}),
        ))
        .unwrap_or(Value::Null);
        self.broadcast(notification, task_id);
        Ok(snapshot)
    }

    fn task_history(&self, params: &Value) -> Result<Value> {
        let task_id = params
            .get("task_id")
            .and_then(Value::as_str)
            .context("task_id")?;
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(1000)
            .clamp(1, 5000) as usize;
        let (snapshot, dir) = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("task map poisoned"))?;
            let task = inner
                .tasks
                .get(task_id)
                .ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))?;
            (task.snapshot(), task.dir.clone())
        };
        let mut events = msk_events::read_events(dir.join("events.jsonl")).unwrap_or_default();
        let truncated = events.len() > limit;
        if truncated {
            events.drain(0..events.len() - limit);
        }
        Ok(json!({
            "task": snapshot,
            "history": events,
            "truncated": truncated
        }))
    }

    fn task_result(&self, task_id: &str) -> Result<Value> {
        let (snapshot, dir) = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("task map poisoned"))?;
            let task = inner
                .tasks
                .get(task_id)
                .ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))?;
            (task.snapshot(), task.dir.clone())
        };
        let events = msk_events::read_events(dir.join("events.jsonl")).unwrap_or_default();
        let transcript = msk_kernel::replay::summarize(&events, Some(task_id), 8_000);
        Ok(json!({
            "task": snapshot,
            "events": events.len(),
            "transcript": transcript
        }))
    }

    fn record_turn_end(
        &self,
        task_id: &str,
        status: &str,
        message: Option<String>,
    ) -> Option<Value> {
        let snapshot = {
            let mut inner = self.inner.lock().ok()?;
            let task = inner.tasks.get_mut(task_id)?;
            match task.state {
                TaskState::Done | TaskState::Failed | TaskState::Cancelled => return None,
                TaskState::Running | TaskState::WaitingInput => {}
            }
            task.turns += 1;
            task.state = TaskState::WaitingInput;
            if status == "awaiting_input" {
                task.last_error = None;
            } else {
                task.last_error = message;
            }
            task.snapshot()
        };
        self.clear_approval(task_id);
        self.broadcast_state(task_id, &snapshot);
        Some(snapshot)
    }

    fn record_approval_request(&self, task_id: &str, value: &Value) {
        let pending = PendingApproval {
            id: value.get("id").and_then(Value::as_u64).unwrap_or(0),
            tool: value
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("?")
                .to_string(),
            args: value.get("args").cloned().unwrap_or(Value::Null),
            reason: value
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            requested_ms: now_ms(),
        };
        if let Ok(mut approvals) = self.approvals.lock() {
            approvals.insert(task_id.to_string(), pending.clone());
        }
        let notification = serde_json::to_value(RpcNotification::new(
            "approval.request",
            pending.snapshot(task_id),
        ))
        .unwrap_or(Value::Null);
        self.broadcast(notification, task_id);
    }

    fn approval_snapshots(&self) -> Vec<Value> {
        let now = now_ms();
        let mut approvals = match self.approvals.lock() {
            Ok(approvals) => approvals,
            Err(_) => return Vec::new(),
        };
        approvals.retain(|_, pending| now.saturating_sub(pending.requested_ms) < 10 * 60 * 1000);
        let mut snapshots: Vec<Value> = approvals
            .iter()
            .map(|(task_id, pending)| pending.snapshot(task_id))
            .collect();
        snapshots.sort_by_key(|snapshot| {
            snapshot
                .get("requested_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        });
        snapshots
    }

    fn clear_approval(&self, task_id: &str) {
        let pending = self
            .approvals
            .lock()
            .ok()
            .and_then(|mut approvals| approvals.remove(task_id));
        if let Some(pending) = pending {
            let notification = serde_json::to_value(RpcNotification::new(
                "approval.resolved",
                json!({
                    "task_id": task_id,
                    "id": pending.id,
                    "approved": null,
                    "reason": "no longer pending"
                }),
            ))
            .unwrap_or(Value::Null);
            self.broadcast(notification, task_id);
        }
    }

    fn respond_approval(&self, params: &Value) -> Result<Value> {
        let task_id = params
            .get("task_id")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("task_id is required"))?;
        let requested_id = params.get("id").and_then(Value::as_u64);
        let approved = params
            .get("approved")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let pending = {
            let mut approvals = self
                .approvals
                .lock()
                .map_err(|_| anyhow::anyhow!("approval map poisoned"))?;
            let pending = approvals
                .get(task_id)
                .ok_or_else(|| anyhow::anyhow!("no pending approval for {task_id}"))?;
            if let Some(id) = requested_id {
                if id != pending.id {
                    bail!("approval id mismatch for {task_id}");
                }
            }
            approvals.remove(task_id).unwrap()
        };

        let input = {
            let inner = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("task map poisoned"))?;
            let task = inner
                .tasks
                .get(task_id)
                .ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))?;
            task.input.clone()
        };
        let write_result = (|| -> Result<()> {
            let mut slot = input
                .lock()
                .map_err(|_| anyhow::anyhow!("task input poisoned"))?;
            let stdin = slot
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("task {task_id} stdin is not available"))?;
            writeln!(
                stdin,
                "{}",
                json!({"type": "approval.respond", "id": pending.id, "approved": approved})
            )?;
            stdin.flush()?;
            Ok(())
        })();
        if let Err(err) = write_result {
            self.set_state(task_id, TaskState::Failed, Some(err.to_string()));
            return Err(err);
        }

        let notification = serde_json::to_value(RpcNotification::new(
            "approval.resolved",
            json!({"task_id": task_id, "id": pending.id, "approved": approved}),
        ))
        .unwrap_or(Value::Null);
        self.broadcast(notification, task_id);
        Ok(json!({
            "accepted": true,
            "task_id": task_id,
            "id": pending.id,
            "approved": approved
        }))
    }

    fn set_state(&self, task_id: &str, state: TaskState, error: Option<String>) {
        let snapshot = {
            let mut inner = match self.inner.lock() {
                Ok(inner) => inner,
                Err(_) => return,
            };
            match inner.tasks.get_mut(task_id) {
                Some(task) => {
                    if task.state != TaskState::Cancelled {
                        task.state = state;
                        if let Some(error) = error {
                            task.last_error = Some(error);
                        }
                    }
                    task.snapshot()
                }
                None => return,
            }
        };
        self.broadcast_state(task_id, &snapshot);
    }

    fn tail_events(&self, task_id: &str, path: &Path, mut offset: u64) -> u64 {
        let metadata = match fs::metadata(path) {
            Ok(metadata) => metadata,
            Err(_) => return offset,
        };
        if metadata.len() < offset {
            offset = 0;
        }
        if metadata.len() == offset {
            return offset;
        }
        let mut file = match File::open(path) {
            Ok(file) => file,
            Err(_) => return offset,
        };
        if file.seek(SeekFrom::Start(offset)).is_err() {
            return offset;
        }
        let mut reader = BufReader::new(file);
        loop {
            let mut raw = Vec::new();
            match reader.read_until(b'\n', &mut raw) {
                Ok(0) => break,
                Ok(read) => {
                    if !raw.ends_with(b"\n") {
                        break;
                    }
                    offset += read as u64;
                    if let Ok(event) = serde_json::from_slice::<Event>(&raw) {
                        if event.kind == "turn.end" {
                            let turn = event
                                .data
                                .get("turn")
                                .and_then(Value::as_u64)
                                .unwrap_or_default();
                            if let Ok(mut inner) = self.inner.lock()
                                && let Some(task) = inner.tasks.get_mut(task_id)
                            {
                                task.turns = task.turns.max(turn);
                            }
                        }
                        self.record_event(task_id, event);
                    }
                }
                Err(_) => break,
            }
        }
        offset
    }

    fn event_query(&self, params: &Value) -> Result<Value> {
        let store = self
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("event store is not configured"))?;
        let query = msk_store::EventQuery {
            task: params
                .get("task_id")
                .or_else(|| params.get("task"))
                .and_then(Value::as_str)
                .map(str::to_string),
            kind: params
                .get("kind")
                .and_then(Value::as_str)
                .map(str::to_string),
            from_seq: params.get("from_seq").and_then(Value::as_u64),
            limit: params.get("limit").and_then(Value::as_u64).unwrap_or(1000) as usize,
        };
        let events = store.query(&query)?;
        Ok(json!({"events": events, "count": events.len()}))
    }

    fn record_event(&self, task_id: &str, event: Event) {
        if let Some(store) = &self.store {
            if let Err(err) = store.append(&event) {
                eprintln!("mskd event store: {err}");
            }
        }
        if let Ok(mut inner) = self.inner.lock() {
            if let Some(task) = inner.tasks.get_mut(task_id) {
                task.events += 1;
            }
        }
        let notification = serde_json::to_value(RpcNotification::new(
            "event",
            json!({"task_id": task_id, "event": event}),
        ))
        .unwrap_or(Value::Null);
        self.broadcast(notification, task_id);
    }

    fn broadcast_state(&self, task_id: &str, snapshot: &Value) {
        let notification =
            serde_json::to_value(RpcNotification::new("task.state", snapshot.clone()))
                .unwrap_or(Value::Null);
        self.broadcast(notification, task_id);
    }

    fn broadcast(&self, notification: Value, task_id: &str) {
        let mut subscribers = match self.subscribers.lock() {
            Ok(subscribers) => subscribers,
            Err(_) => return,
        };
        subscribers.retain(|subscriber| {
            if let Some(wanted) = &subscriber.task {
                if wanted != task_id {
                    return true;
                }
            }
            subscriber.tx.send(notification.clone()).is_ok()
        });
    }

    fn cancel_task(&self, task_id: &str) -> Result<Value> {
        let (snapshot, child) = {
            let mut inner = self
                .inner
                .lock()
                .map_err(|_| anyhow::anyhow!("task map poisoned"))?;
            let task = inner
                .tasks
                .get_mut(task_id)
                .ok_or_else(|| anyhow::anyhow!("task not found: {task_id}"))?;
            task.state = TaskState::Cancelled;
            (task.snapshot(), task.child.clone())
        };
        if let Ok(mut slot) = child.lock() {
            if let Some(child) = slot.as_mut() {
                let _ = child.kill();
            }
        }
        self.clear_approval(task_id);
        self.broadcast_state(task_id, &snapshot);
        Ok(snapshot)
    }

    fn finish_task(&self, task_id: &str, state: TaskState, error: Option<String>) {
        let snapshot = {
            let mut inner = match self.inner.lock() {
                Ok(inner) => inner,
                Err(_) => return,
            };
            match inner.tasks.get_mut(task_id) {
                Some(task) => {
                    if task.state != TaskState::Cancelled {
                        task.state = state;
                    }
                    task.snapshot()
                }
                None => return,
            }
        };
        if let Some(error) = error {
            eprintln!("mskd task {task_id}: {error}");
        }
        self.clear_approval(task_id);
        self.broadcast_state(task_id, &snapshot);
    }

    fn unsubscribe(&self, id: u64) {
        if let Ok(mut subscribers) = self.subscribers.lock() {
            subscribers.retain(|subscriber| subscriber.id != id);
        }
    }

    fn terminal_open(&self, params: &Value) -> Result<Value> {
        let mut guard = self
            .terminal
            .lock()
            .map_err(|_| anyhow::anyhow!("terminal lock poisoned"))?;
        if let Some(terminal) = guard.as_ref() {
            return Ok(json!({
                "session_id": terminal.session_id,
                "cols": terminal.cols,
                "rows": terminal.rows
            }));
        }

        let cols = params.get("cols").and_then(Value::as_u64).unwrap_or(100) as u16;
        let rows = params.get("rows").and_then(Value::as_u64).unwrap_or(30) as u16;
        let mut client: Box<dyn EnvTransport> = if let Some(url) = &self.envd_url {
            let ca_pem = match &self.envd_ca {
                Some(path) => Some(
                    fs::read(path).with_context(|| format!("read envd CA {}", path.display()))?,
                ),
                None => None,
            };
            let mut service = ServiceClient::new_with_ca(
                url,
                self.envd_token.as_deref().unwrap_or_default(),
                ca_pem,
            )
            .context("configure envd TLS")?;
            service.initialize()?;
            Box::new(service)
        } else {
            let envd = self.envd.as_ref().context("envd is not configured")?;
            let envd_root = self.workspace.clone();
            fs::create_dir_all(&envd_root)?;
            let mut embedded = EnvClient::spawn(envd, &envd_root).context("spawn terminal envd")?;
            embedded.handshake("mskd-terminal")?;
            embedded.initialize()?;
            Box::new(embedded)
        };

        let mut env_params = json!({
            "name": "terminal",
            "kind": "managed",
            "profile": self.envd_profile
        });
        if self.envd_url.is_none() {
            env_params["root"] = json!(".");
        }
        let env = client.call_tool("env_create", env_params)?;
        let env_data = mcp_data(&env)?;
        let env_id = env_data
            .get("env_id")
            .and_then(Value::as_str)
            .context("terminal env_id")?
            .to_string();
        let session = client.call_tool(
            "session_create",
            json!({"env_id": env_id, "pty": true, "cols": cols, "rows": rows}),
        )?;
        let session_data = mcp_data(&session)?;
        let session_id = session_data
            .get("session_id")
            .and_then(Value::as_str)
            .context("terminal session_id")?
            .to_string();

        let data = if let (Some(address), Some(_)) = (&self.envd_data_addr, &self.envd_url) {
            let tls = if self.envd_data_tls {
                let ca_pem = match &self.envd_ca {
                    Some(path) => Some(
                        fs::read(path)
                            .with_context(|| format!("read envd CA {}", path.display()))?,
                    ),
                    None => None,
                };
                Some(msk_mcp::TlsOptions {
                    server_name: self
                        .envd_data_tls_name
                        .clone()
                        .unwrap_or_else(|| host_part(address)),
                    ca_pem,
                })
            } else {
                None
            };
            let connected = match &tls {
                Some(tls) => EnvDataClient::connect_tls(
                    address,
                    self.envd_token.as_deref().unwrap_or_default(),
                    &session_id,
                    cols,
                    rows,
                    None,
                    tls,
                ),
                None => EnvDataClient::connect(
                    address,
                    self.envd_token.as_deref().unwrap_or_default(),
                    &session_id,
                    cols,
                    rows,
                    None,
                ),
            };
            match connected {
                Ok(data) => Some(data),
                Err(err) => {
                    let _ = client.call_tool("session_close", json!({"session_id": session_id}));
                    return Err(err).context("connect envd data plane");
                }
            }
        } else {
            None
        };

        *guard = Some(TerminalSession {
            client,
            data,
            session_id: session_id.clone(),
            cols,
            rows,
        });
        Ok(json!({"session_id": session_id, "cols": cols, "rows": rows}))
    }

    fn terminal_write(&self, params: &Value) -> Result<Value> {
        let session_id = params
            .get("session_id")
            .and_then(Value::as_str)
            .context("session_id")?;
        let data = params
            .get("data_base64")
            .and_then(Value::as_str)
            .context("data_base64")?;
        let mut guard = self
            .terminal
            .lock()
            .map_err(|_| anyhow::anyhow!("terminal lock poisoned"))?;
        let terminal = guard.as_mut().context("terminal is not open")?;
        if terminal.session_id != session_id {
            bail!("terminal session mismatch");
        }
        if let Some(data_client) = &terminal.data {
            let bytes = STANDARD.decode(data)?;
            data_client.send_data(&bytes)?;
            return Ok(json!({"bytes": bytes.len()}));
        }
        let result = terminal.client.call_tool(
            "session_write",
            json!({"session_id": session_id, "data_base64": data}),
        )?;
        mcp_data(&result)
    }

    fn terminal_read(&self, params: &Value) -> Result<Value> {
        let session_id = params
            .get("session_id")
            .and_then(Value::as_str)
            .context("session_id")?;
        let max_bytes = params
            .get("max_bytes")
            .and_then(Value::as_u64)
            .unwrap_or(64 * 1024);
        let timeout_ms = params
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let mut guard = self
            .terminal
            .lock()
            .map_err(|_| anyhow::anyhow!("terminal lock poisoned"))?;
        let terminal = guard.as_mut().context("terminal is not open")?;
        if terminal.session_id != session_id {
            bail!("terminal session mismatch");
        }
        if let Some(data_client) = &mut terminal.data {
            let bytes = data_client.read(max_bytes as usize, Duration::from_millis(timeout_ms))?;
            return Ok(json!({
                "data_base64": STANDARD.encode(&bytes),
                "bytes": bytes.len(),
                "closed": data_client.is_closed(),
                "error": data_client.last_error()
            }));
        }
        let result = terminal.client.call_tool(
            "session_read",
            json!({
                "session_id": session_id,
                "max_bytes": max_bytes,
                "timeout_ms": timeout_ms
            }),
        )?;
        mcp_data(&result)
    }

    fn terminal_resize(&self, params: &Value) -> Result<Value> {
        let session_id = params
            .get("session_id")
            .and_then(Value::as_str)
            .context("session_id")?
            .to_string();
        let cols = params.get("cols").and_then(Value::as_u64).context("cols")? as u16;
        let rows = params.get("rows").and_then(Value::as_u64).context("rows")? as u16;
        let mut guard = self
            .terminal
            .lock()
            .map_err(|_| anyhow::anyhow!("terminal lock poisoned"))?;
        let terminal = guard.as_mut().context("terminal is not open")?;
        if terminal.session_id != session_id {
            bail!("terminal session mismatch");
        }
        if let Some(data_client) = &terminal.data {
            data_client.resize(cols, rows)?;
            terminal.cols = cols;
            terminal.rows = rows;
            return Ok(json!({"cols": cols, "rows": rows}));
        }
        let result = terminal.client.call_tool(
            "session_resize",
            json!({"session_id": session_id, "cols": cols, "rows": rows}),
        )?;
        terminal.cols = cols;
        terminal.rows = rows;
        mcp_data(&result)
    }

    fn terminal_close(&self, params: &Value) -> Result<Value> {
        let session_id = params
            .get("session_id")
            .and_then(Value::as_str)
            .context("session_id")?
            .to_string();
        let mut guard = self
            .terminal
            .lock()
            .map_err(|_| anyhow::anyhow!("terminal lock poisoned"))?;
        let data = {
            let terminal = guard.as_mut().context("terminal is not open")?;
            if terminal.session_id != session_id {
                bail!("terminal session mismatch");
            }
            if let Some(data_client) = &terminal.data {
                let _ = data_client.close();
            }
            let result = terminal
                .client
                .call_tool("session_close", json!({"session_id": session_id}))?;
            mcp_data(&result)?
        };
        *guard = None;
        Ok(data)
    }
}

fn host_part(address: &str) -> String {
    if let Some(rest) = address.strip_prefix('[')
        && let Some((host, _)) = rest.split_once(']')
    {
        return host.to_string();
    }
    address
        .rsplit_once(':')
        .map(|(host, _)| host.to_string())
        .unwrap_or_else(|| address.to_string())
}

fn mcp_data(result: &Value) -> Result<Value> {
    let structured = result
        .get("structuredContent")
        .context("missing structuredContent")?;
    if structured.get("ok").and_then(Value::as_bool) != Some(true) {
        bail!("tool not ok: {structured}");
    }
    Ok(structured.get("data").cloned().unwrap_or(Value::Null))
}

fn handle_data_client(stream: Stream, daemon: Arc<Daemon>) -> Result<()> {
    let reader_stream = stream.try_clone()?;
    let writer = Arc::new(Mutex::new(stream));
    let mut reader = BufReader::new(reader_stream);

    let hello =
        DataFrame::decode(&mut reader, DATA_MAX_FRAME)?.context("expected data HELLO frame")?;
    if hello.kind != DataKind::Hello {
        bail!("expected HELLO, got {:?}", hello.kind);
    }
    let hello_value = hello.json_value()?;
    let token = hello_value
        .get("token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !token_eq(token, &daemon.token) {
        send_data_error(&writer, "unauthorized");
        return Ok(());
    }
    let ack = DataFrame::json(
        DataKind::HelloAck,
        0,
        &json!({
            "protocol": 1,
            "max_frame": DATA_MAX_FRAME,
            "credits_initial": 262144
        }),
    )?;
    {
        let mut writer = writer
            .lock()
            .map_err(|_| anyhow::anyhow!("data writer poisoned"))?;
        write_frame(&mut *writer, &ack)?;
    }

    let credit = Arc::new(Mutex::new(262_144u64));
    let stop = Arc::new(AtomicBool::new(false));
    let mut session_id: Option<String> = None;

    while let Some(frame) = DataFrame::decode(&mut reader, DATA_MAX_FRAME)? {
        match frame.kind {
            DataKind::Open => {
                if session_id.is_some() {
                    send_data_error(&writer, "stream already open");
                    continue;
                }
                if daemon.data_active.swap(true, Ordering::SeqCst) {
                    send_data_error(&writer, "another terminal data stream is active");
                    continue;
                }
                let open = frame.json_value()?;
                let cols = open.get("cols").and_then(Value::as_u64).unwrap_or(100) as u16;
                let rows = open.get("rows").and_then(Value::as_u64).unwrap_or(30) as u16;
                let opened = daemon.terminal_open(&json!({"cols": cols, "rows": rows}))?;
                let sid = opened
                    .get("session_id")
                    .and_then(Value::as_str)
                    .context("terminal session_id")?
                    .to_string();
                let ack = DataFrame::json(
                    DataKind::OpenAck,
                    1,
                    &json!({
                        "session_id": sid,
                        "cols": cols,
                        "rows": rows,
                        "credits_initial": 262144
                    }),
                )?;
                {
                    let mut writer = writer
                        .lock()
                        .map_err(|_| anyhow::anyhow!("data writer poisoned"))?;
                    write_frame(&mut *writer, &ack)?;
                }
                session_id = Some(sid.clone());
                let pump_daemon = daemon.clone();
                let pump_writer = writer.clone();
                let pump_credit = credit.clone();
                let pump_stop = stop.clone();
                thread::spawn(move || {
                    pump_terminal(pump_daemon, pump_writer, pump_credit, pump_stop, sid)
                });
            }
            DataKind::Data => {
                let Some(sid) = session_id.as_ref() else {
                    send_data_error(&writer, "stream not open");
                    continue;
                };
                let encoded = STANDARD.encode(&frame.payload);
                if let Err(err) =
                    daemon.terminal_write(&json!({"session_id": sid, "data_base64": encoded}))
                {
                    send_data_error(&writer, &err.to_string());
                }
            }
            DataKind::Resize => {
                let Some(sid) = session_id.as_ref() else {
                    send_data_error(&writer, "stream not open");
                    continue;
                };
                let value = frame.json_value()?;
                let cols = value.get("cols").and_then(Value::as_u64).unwrap_or(100);
                let rows = value.get("rows").and_then(Value::as_u64).unwrap_or(30);
                if let Err(err) =
                    daemon.terminal_resize(&json!({"session_id": sid, "cols": cols, "rows": rows}))
                {
                    send_data_error(&writer, &err.to_string());
                }
            }
            DataKind::Credit => {
                if frame.payload.len() == 8 {
                    let amount = u64::from_be_bytes(frame.payload[..8].try_into().unwrap());
                    if let Ok(mut credit) = credit.lock() {
                        *credit = credit.saturating_add(amount);
                    }
                }
            }
            DataKind::Close => {
                stop.store(true, Ordering::SeqCst);
                daemon.data_active.store(false, Ordering::SeqCst);
                if let Some(sid) = session_id.take() {
                    let _ = daemon.terminal_close(&json!({"session_id": sid}));
                }
                let close =
                    DataFrame::json(DataKind::Close, 1, &json!({"reason": "server_close"}))?;
                if let Ok(mut writer) = writer.lock() {
                    let _ = write_frame(&mut *writer, &close);
                }
                return Ok(());
            }
            DataKind::Ping => {
                let pong = DataFrame::new(DataKind::Pong, 0, Vec::new());
                if let Ok(mut writer) = writer.lock() {
                    let _ = write_frame(&mut *writer, &pong);
                }
            }
            _ => {}
        }
    }

    stop.store(true, Ordering::SeqCst);
    daemon.data_active.store(false, Ordering::SeqCst);
    if let Some(sid) = session_id {
        let _ = daemon.terminal_close(&json!({"session_id": sid}));
    }
    Ok(())
}

fn pump_terminal(
    daemon: Arc<Daemon>,
    writer: Arc<Mutex<Stream>>,
    credit: Arc<Mutex<u64>>,
    stop: Arc<AtomicBool>,
    session_id: String,
) {
    while !stop.load(Ordering::SeqCst) {
        let available = match credit.lock() {
            Ok(credit) => *credit,
            Err(_) => break,
        };
        if available == 0 {
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        let max_bytes = available.min(64 * 1024);
        let result = daemon.terminal_read(&json!({
            "session_id": session_id.clone(),
            "max_bytes": max_bytes,
            "timeout_ms": 20
        }));
        match result {
            Ok(value) => {
                let encoded = value
                    .get("data_base64")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let bytes = STANDARD.decode(encoded).unwrap_or_default();
                if !bytes.is_empty() {
                    let frame = DataFrame::new(DataKind::Data, 1, bytes.clone());
                    let written = writer
                        .lock()
                        .map(|mut writer| write_frame(&mut *writer, &frame));
                    if !matches!(written, Ok(Ok(()))) {
                        break;
                    }
                    if let Ok(mut credit) = credit.lock() {
                        *credit = credit.saturating_sub(bytes.len() as u64);
                    }
                }
                if value.get("eof").and_then(Value::as_bool).unwrap_or(false)
                    || value
                        .get("closed")
                        .and_then(Value::as_bool)
                        .unwrap_or(false)
                {
                    let frame = DataFrame::new(DataKind::Exit, 1, Vec::new());
                    if let Ok(mut writer) = writer.lock() {
                        let _ = write_frame(&mut *writer, &frame);
                    }
                    break;
                }
            }
            Err(err) => {
                send_data_error(&writer, &err.to_string());
                break;
            }
        }
    }
}

fn send_data_error(writer: &Arc<Mutex<Stream>>, message: &str) {
    let frame = DataFrame::json(
        DataKind::Error,
        0,
        &json!({"code": 1, "name": "ERROR", "message": message}),
    );
    if let Ok(frame) = frame {
        if let Ok(mut writer) = writer.lock() {
            let _ = write_frame(&mut *writer, &frame);
        }
    }
}

fn token_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in left.iter().zip(right) {
        difference |= left ^ right;
    }
    difference == 0
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison_is_exact() {
        assert!(token_eq("abc", "abc"));
        assert!(!token_eq("abc", "abd"));
        assert!(!token_eq("abc", "ab"));
        assert!(!token_eq("", "x"));
        assert!(token_eq("", ""));
    }

    #[test]
    fn blackboard_persists_and_enforces_limits() {
        let dir = std::env::temp_dir().join(format!("msk-blackboard-{}", now_ms()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("blackboard.json");
        let mut blackboard = Blackboard::load(path.clone());
        let first = blackboard
            .set("finding", json!({"port": 8080}), None)
            .unwrap();
        assert_eq!(first["ok"], true);
        assert_eq!(first["entry"]["version"], 1);
        assert_eq!(blackboard.get("finding")["value"]["port"], 8080);
        let reloaded = Blackboard::load(path.clone());
        assert_eq!(reloaded.get("finding")["value"]["port"], 8080);
        let updated = blackboard
            .set("finding", json!({"port": 8081}), Some(1))
            .unwrap();
        assert_eq!(updated["entry"]["version"], 2);
        let conflict = blackboard
            .set("finding", json!({"port": 8082}), Some(1))
            .unwrap();
        assert_eq!(conflict["conflict"], true);
        assert_eq!(conflict["current"]["version"], 2);
        assert!(blackboard.set("", Value::Null, None).is_err());
        assert!(
            blackboard
                .set("huge", Value::String("x".repeat(70 * 1024)), None)
                .is_err()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
