use std::collections::HashMap;
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
    interactive: bool,
    stream: bool,
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
            "interactive": self.interactive,
            "stream": self.stream,
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
                            "capabilities": ["tasks", "events", "sessions", "approvals", "terminal"]
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
                let interactive = params
                    .get("interactive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let stream = params
                    .get("stream")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                match self.create_task(goal, demo, provider, interactive, stream) {
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
        interactive: bool,
        stream: bool,
    ) -> Result<Value> {
        let id = format!(
            "task-{}-{}",
            now_ms(),
            self.next_task.fetch_add(1, Ordering::SeqCst)
        );
        let dir = self.root.join("tasks").join(&id);
        fs::create_dir_all(&dir)?;
        let child = Arc::new(Mutex::new(None));
        let input = Arc::new(Mutex::new(None));
        let task = Task {
            id: id.clone(),
            goal,
            demo,
            provider,
            interactive,
            stream,
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
        let (dir, goal, demo, provider, interactive, stream, child_handle, input_handle) = {
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
                    task.interactive,
                    task.stream,
                    task.child.clone(),
                    task.input.clone(),
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
            .arg("--wasm")
            .arg(self.tools_dir.join("exec.wat"))
            .arg("--provider")
            .arg(&provider);
        if interactive {
            command
                .arg("--session")
                .arg("--stdio-json")
                .arg("--task-id")
                .arg(&task_id)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped());
            if stream {
                command.arg("--stream");
            }
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
        self.broadcast_state(task_id, &snapshot);
        Some(snapshot)
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
}
