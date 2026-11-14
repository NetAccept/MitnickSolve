use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
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
use msk_mcp::EnvClient;
use serde_json::{Value, json};
use uuid::Uuid;

#[derive(Debug, Parser)]
struct Cli {
    /// Kernel state directory containing the socket and token.
    #[arg(long, default_value = ".msk")]
    root: PathBuf,
    /// Path to the envd binary.
    #[arg(long)]
    envd: PathBuf,
    /// Directory containing tool manifests and WAT modules.
    #[arg(long, default_value = "tools/wasm")]
    tools_dir: PathBuf,
    /// Agent binary; defaults to `msk` next to this daemon.
    #[arg(long)]
    agent_bin: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TaskState {
    Running,
    Done,
    Failed,
    Cancelled,
}

impl TaskState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
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
    state: TaskState,
    dir: PathBuf,
    child: Arc<Mutex<Option<Child>>>,
    events: u64,
    created_ms: u128,
}

impl Task {
    fn snapshot(&self) -> Value {
        json!({
            "task_id": self.id,
            "goal": self.goal,
            "demo": self.demo,
            "provider": self.provider,
            "state": self.state.as_str(),
            "dir": self.dir.display().to_string(),
            "events": self.events,
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
    client: EnvClient,
    session_id: String,
    cols: u16,
    rows: u16,
}

#[derive(Default)]
struct Inner {
    tasks: HashMap<String, Task>,
}

struct Daemon {
    root: PathBuf,
    envd: PathBuf,
    tools_dir: PathBuf,
    agent_bin: PathBuf,
    token: String,
    inner: Mutex<Inner>,
    subscribers: Mutex<Vec<Subscriber>>,
    terminal: Mutex<Option<TerminalSession>>,
    data_active: AtomicBool,
    next_task: AtomicU64,
    next_sub: AtomicU64,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    fs::create_dir_all(&cli.root)?;
    let root = msk_ipc::normalize_root(&cli.root);

    let envd = msk_ipc::normalize_root(&cli.envd);
    if !envd.exists() {
        bail!("envd binary not found: {}", envd.display());
    }
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

    let daemon = Arc::new(Daemon {
        root,
        envd,
        tools_dir,
        agent_bin,
        token,
        inner: Mutex::new(Inner::default()),
        subscribers: Mutex::new(Vec::new()),
        terminal: Mutex::new(None),
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
    while let Some(value) = msk_ipc::read_json_line(&mut reader)? {
        let request: RpcRequest = match serde_json::from_value(value) {
            Ok(request) => request,
            Err(err) => {
                eprintln!("mskd bad request: {err}");
                continue;
            }
        };
        let (response, subscription) = daemon.handle(request, &event_tx);
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
    ) -> (RpcResponse, Option<u64>) {
        let id = request.id;
        let params = request.params;
        match request.method.as_str() {
            methods::HELLO => {
                let token = params
                    .get("token")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if token != self.token {
                    return (RpcResponse::err(id, -32001, "unauthorized"), None);
                }
                (
                    RpcResponse::ok(
                        id,
                        json!({
                            "protocol": msk_ipc::IPC_PROTOCOL_VERSION,
                            "server": "mskd",
                            "capabilities": ["tasks", "events", "approvals"]
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
                match self.create_task(goal, demo, provider) {
                    Ok(snapshot) => (RpcResponse::ok(id, snapshot), None),
                    Err(err) => (RpcResponse::err(id, -32000, err.to_string()), None),
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
            methods::APPROVAL_LIST => (RpcResponse::ok(id, json!({"approvals": []})), None),
            methods::APPROVAL_RESPOND => (
                RpcResponse::ok(
                    id,
                    json!({"accepted": false, "reason": "no pending approvals in M0"}),
                ),
                None,
            ),
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
    ) -> Result<Value> {
        let id = format!("task-{}", self.next_task.fetch_add(1, Ordering::SeqCst));
        let dir = self.root.join("tasks").join(&id);
        fs::create_dir_all(&dir)?;
        let child = Arc::new(Mutex::new(None));
        let task = Task {
            id: id.clone(),
            goal,
            demo,
            provider,
            state: TaskState::Running,
            dir,
            child: child.clone(),
            events: 0,
            created_ms: now_ms(),
        };
        let snapshot = task.snapshot();
        self.inner
            .lock()
            .map_err(|_| anyhow::anyhow!("task map poisoned"))?
            .tasks
            .insert(id.clone(), task);
        self.broadcast_state(&id, &snapshot);
        let daemon = self.clone();
        thread::spawn(move || daemon.run_task(id));
        Ok(snapshot)
    }

    fn run_task(self: Arc<Self>, task_id: String) {
        let (dir, goal, demo, provider, child_handle) = {
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
                    task.child.clone(),
                ),
                None => return,
            }
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
        command
            .arg("--envd")
            .arg(&self.envd)
            .arg("--root")
            .arg(&dir)
            .arg("--tools-dir")
            .arg(&self.tools_dir)
            .arg("--demo")
            .arg(&demo)
            .arg("--provider")
            .arg(&provider)
            .arg("--goal")
            .arg(&goal)
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr));
        let child = match command.spawn() {
            Ok(child) => child,
            Err(err) => {
                self.finish_task(&task_id, TaskState::Failed, Some(err.to_string()));
                return;
            }
        };
        if let Ok(mut slot) = child_handle.lock() {
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
                let error = result.as_ref().err().cloned();
                let cancelled = self
                    .inner
                    .lock()
                    .ok()
                    .and_then(|inner| inner.tasks.get(&task_id).map(|task| task.state))
                    == Some(TaskState::Cancelled);
                let state = if cancelled {
                    TaskState::Cancelled
                } else {
                    match &result {
                        Ok(status) if status.success() => TaskState::Done,
                        _ => TaskState::Failed,
                    }
                };
                if let Ok(mut slot) = child_handle.lock() {
                    *slot = None;
                }
                self.finish_task(&task_id, state, error);
                break;
            }

            thread::sleep(Duration::from_millis(100));
        }
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
                        self.record_event(task_id, event);
                    }
                }
                Err(_) => break,
            }
        }
        offset
    }

    fn record_event(&self, task_id: &str, event: Event) {
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
        let envd_root = self.root.join("terminal-envd");
        fs::create_dir_all(&envd_root)?;
        let mut client = EnvClient::spawn(&self.envd, &envd_root).context("spawn terminal envd")?;
        client.handshake("mskd-terminal")?;
        client.initialize()?;

        let env = client.call_tool(
            "env_create",
            json!({"name": "terminal", "kind": "managed", "profile": "dev"}),
        )?;
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

        *guard = Some(TerminalSession {
            client,
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
            let result = terminal
                .client
                .call_tool("session_close", json!({"session_id": session_id}))?;
            mcp_data(&result)?
        };
        *guard = None;
        Ok(data)
    }
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
    if token != daemon.token {
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
                if value.get("eof").and_then(Value::as_bool).unwrap_or(false) {
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

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
