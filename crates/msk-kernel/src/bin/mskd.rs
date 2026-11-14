use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::Parser;
use msk_events::Event;
use msk_ipc::local::{self, Stream};
use msk_ipc::{RpcNotification, RpcRequest, RpcResponse, TryClone, methods};
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
    println!("mskd listening at {}", local::display_name(&root));
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
        next_task: AtomicU64::new(1),
        next_sub: AtomicU64::new(1),
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
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}
