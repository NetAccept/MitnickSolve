use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use clap::{Parser, Subcommand};
use msk_ipc::data::{DataClient, DataEvent};
use msk_ipc::{Client, methods};
use serde_json::json;

#[derive(Debug, Parser)]
struct Cli {
    /// Kernel state directory containing the socket and token.
    #[arg(long, default_value = ".msk")]
    root: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// List tasks known to the daemon.
    List,
    /// Create a task and return its ID.
    Create {
        #[arg(
            long,
            default_value = "Create a persistent shell, run a command, and close it."
        )]
        goal: String,
        #[arg(long, default_value = "session")]
        demo: String,
        #[arg(long, default_value = "scripted")]
        provider: String,
        /// Model name for OpenAI-compatible providers.
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort: minimal, low, medium, high.
        #[arg(long, value_parser = ["minimal", "low", "medium", "high"])]
        effort: Option<String>,
        /// OpenAI-compatible base URL.
        #[arg(long)]
        base_url: Option<String>,
        /// Agent mode id from modes.json; auto routes by goal.
        #[arg(long, default_value = "auto")]
        mode: String,
        /// Resume a previous task checkpoint.
        #[arg(long)]
        resume_checkpoint: Option<PathBuf>,
        /// Keep the agent session alive and accept task.message calls.
        #[arg(long)]
        interactive: bool,
        /// Ask the agent to stream text deltas.
        #[arg(long)]
        stream: bool,
        /// Run one goal turn without stdin, then exit.
        #[arg(long)]
        headless: bool,
        /// Command policy: ask before executing, or allow directly.
        /// Defaults to ask for interactive tasks and allow otherwise.
        #[arg(long, value_parser = ["allow", "ask"])]
        approvals: Option<String>,
        /// Permission posture: auto, manual, or bypass.
        #[arg(long, value_parser = ["auto", "manual", "bypass"])]
        permission: Option<String>,
        /// Context token budget for the task (default 100000).
        #[arg(long)]
        max_context_tokens: Option<u64>,
        /// Parent task id for a delegated subagent.
        #[arg(long)]
        parent_task: Option<String>,
        /// Fork this task's checkpoint into the new task.
        #[arg(long)]
        fork_from: Option<String>,
    },
    /// Send a user message to an interactive task.
    Send {
        #[arg(long)]
        task: String,
        #[arg(long)]
        text: String,
    },
    /// Cooperatively interrupt the running turn of an interactive task.
    Interrupt { task: String },
    /// List pending approval requests.
    Approvals,
    /// Approve a pending request.
    Approve {
        #[arg(long)]
        task: String,
        #[arg(long)]
        id: u64,
    },
    /// Deny a pending request.
    Deny {
        #[arg(long)]
        task: String,
        #[arg(long)]
        id: u64,
    },
    /// Print one task snapshot.
    State { task: String },
    /// Print a task's state, event count, and transcript.
    Result { task: String },
    /// List shared blackboard entries.
    BbList,
    /// Read one shared blackboard entry.
    BbGet { key: String },
    /// Write one shared blackboard entry (JSON value or plain string).
    BbSet {
        key: String,
        value: String,
        /// Require this version for compare-and-swap.
        #[arg(long)]
        expected_version: Option<u64>,
    },
    /// Cancel a running task.
    Cancel { task: String },
    /// Query the daemon's SQLite event index.
    Events {
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        kind: Option<String>,
        #[arg(long)]
        from_seq: Option<u64>,
        #[arg(long, default_value_t = 100)]
        limit: u64,
    },
    /// Subscribe to events and print notifications.
    Watch {
        #[arg(long)]
        task: Option<String>,
        /// Stop after this many milliseconds; 0 means run until killed.
        #[arg(long, default_value_t = 0)]
        timeout_ms: u64,
    },
    /// Open a daemon-managed PTY terminal.
    TerminalOpen {
        #[arg(long, default_value_t = 100)]
        cols: u16,
        #[arg(long, default_value_t = 30)]
        rows: u16,
    },
    /// Write a line (or raw text) to a terminal.
    TerminalWrite {
        #[arg(long)]
        session: String,
        #[arg(long)]
        line: Option<String>,
        #[arg(long)]
        text: Option<String>,
    },
    /// Read available terminal output and print it.
    TerminalRead {
        #[arg(long)]
        session: String,
        #[arg(long, default_value_t = 200)]
        timeout_ms: u64,
    },
    /// Resize a terminal.
    TerminalResize {
        #[arg(long)]
        session: String,
        #[arg(long)]
        cols: u16,
        #[arg(long)]
        rows: u16,
    },
    /// Close a terminal.
    TerminalClose {
        #[arg(long)]
        session: String,
    },
    /// Open a terminal over the binary data plane, optionally write a line,
    /// and print output until the timeout.
    TerminalStream {
        #[arg(long, default_value_t = 100)]
        cols: u16,
        #[arg(long, default_value_t = 30)]
        rows: u16,
        #[arg(long)]
        line: Option<String>,
        #[arg(long, default_value_t = 2000)]
        timeout_ms: u64,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let mut client = Client::connect(&cli.root).context("connect to mskd")?;
    client.hello().context("kernel.hello")?;

    match cli.command {
        Command::List => {
            let result = client.request(methods::TASK_LIST, json!({}))?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Create {
            goal,
            demo,
            provider,
            model,
            effort,
            base_url,
            mode,
            resume_checkpoint,
            interactive,
            stream,
            headless,
            approvals,
            permission,
            max_context_tokens,
            parent_task,
            fork_from,
        } => {
            let mut params = json!({
                "goal": goal,
                "demo": demo,
                "provider": provider,
                "interactive": interactive,
                "stream": stream,
                "headless": headless,
                "mode": mode
            });
            if let Some(model) = model {
                params["model"] = json!(model);
            }
            if let Some(effort) = effort {
                params["effort"] = json!(effort);
            }
            if let Some(base_url) = base_url {
                params["base_url"] = json!(base_url);
            }
            if let Some(checkpoint) = resume_checkpoint {
                params["resume_checkpoint"] = json!(checkpoint.display().to_string());
            }
            if let Some(approvals) = approvals {
                params["approvals"] = json!(approvals);
            }
            if let Some(permission) = permission {
                params["permission"] = json!(permission);
            }
            if let Some(max_context_tokens) = max_context_tokens {
                params["max_context_tokens"] = json!(max_context_tokens);
            }
            if let Some(parent_task) = parent_task {
                params["parent_task"] = json!(parent_task);
            }
            if let Some(fork_from) = fork_from {
                params["fork_from"] = json!(fork_from);
            }
            let result = client.request(methods::TASK_CREATE, params)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Send { task, text } => {
            let result = client.request(
                methods::TASK_MESSAGE,
                json!({"task_id": task, "text": text}),
            )?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Interrupt { task } => {
            let result = client.request(methods::TASK_INTERRUPT, json!({"task_id": task}))?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Approvals => {
            let result = client.request(methods::APPROVAL_LIST, json!({}))?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Approve { task, id } => {
            let result = client.request(
                methods::APPROVAL_RESPOND,
                json!({"task_id": task, "id": id, "approved": true}),
            )?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Deny { task, id } => {
            let result = client.request(
                methods::APPROVAL_RESPOND,
                json!({"task_id": task, "id": id, "approved": false}),
            )?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::State { task } => {
            let result = client.request(methods::TASK_STATE, json!({"task_id": task}))?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Result { task } => {
            let result = client.request(methods::TASK_RESULT, json!({"task_id": task}))?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::BbList => {
            let result = client.request(methods::BLACKBOARD_LIST, json!({}))?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::BbGet { key } => {
            let result = client.request(methods::BLACKBOARD_GET, json!({"key": key}))?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::BbSet {
            key,
            value,
            expected_version,
        } => {
            let value =
                serde_json::from_str(&value).unwrap_or_else(|_| serde_json::Value::String(value));
            let result = client.request(
                methods::BLACKBOARD_SET,
                json!({
                    "key": key,
                    "value": value,
                    "expected_version": expected_version
                }),
            )?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Cancel { task } => {
            let result = client.request(methods::TASK_CANCEL, json!({"task_id": task}))?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Events {
            task,
            kind,
            from_seq,
            limit,
        } => {
            let mut params = json!({"limit": limit});
            if let Some(task) = task {
                params["task_id"] = json!(task);
            }
            if let Some(kind) = kind {
                params["kind"] = json!(kind);
            }
            if let Some(from_seq) = from_seq {
                params["from_seq"] = json!(from_seq);
            }
            let result = client.request(methods::EVENTS_QUERY, params)?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Watch { task, timeout_ms } => {
            client.subscribe(task.as_deref())?;
            let deadline =
                (timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(timeout_ms));
            loop {
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    break;
                }
                if let Some(value) = client.next_notification_timeout(Duration::from_millis(500)) {
                    println!("{}", serde_json::to_string(&value)?);
                }
            }
        }
        Command::TerminalOpen { cols, rows } => {
            let result =
                client.request(methods::TERMINAL_OPEN, json!({"cols": cols, "rows": rows}))?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::TerminalWrite {
            session,
            line,
            text,
        } => {
            let data = line
                .map(|line| format!("{line}\r"))
                .or(text)
                .unwrap_or_default();
            let encoded = STANDARD.encode(data.as_bytes());
            let result = client.request(
                methods::TERMINAL_WRITE,
                json!({"session_id": session, "data_base64": encoded}),
            )?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::TerminalRead {
            session,
            timeout_ms,
        } => {
            let result = client.request(
                methods::TERMINAL_READ,
                json!({
                    "session_id": session,
                    "max_bytes": 1 << 20,
                    "timeout_ms": timeout_ms
                }),
            )?;
            if let Some(encoded) = result.get("data_base64").and_then(|value| value.as_str()) {
                let bytes = STANDARD.decode(encoded)?;
                print!("{}", String::from_utf8_lossy(&bytes));
            }
        }
        Command::TerminalResize {
            session,
            cols,
            rows,
        } => {
            let result = client.request(
                methods::TERMINAL_RESIZE,
                json!({"session_id": session, "cols": cols, "rows": rows}),
            )?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::TerminalClose { session } => {
            let result = client.request(methods::TERMINAL_CLOSE, json!({"session_id": session}))?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::TerminalStream {
            cols,
            rows,
            line,
            timeout_ms,
        } => {
            let data = DataClient::connect(client.root()).context("connect data plane")?;
            let opened = data.open_terminal(cols, rows)?;
            println!("terminal {}", opened.session_id);
            if let Some(line) = line {
                data.send_data(format!("{line}\r").as_bytes())?;
            }
            let deadline = Instant::now() + Duration::from_millis(timeout_ms);
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                match data.next_event_timeout(remaining.min(Duration::from_millis(100))) {
                    Some(DataEvent::Data(bytes)) => {
                        print!("{}", String::from_utf8_lossy(&bytes));
                        let _ = std::io::stdout().flush();
                        let _ = data.credit(bytes.len());
                    }
                    Some(DataEvent::Exit) | Some(DataEvent::Closed) => break,
                    Some(DataEvent::Error(message)) => {
                        eprintln!("terminal error: {message}");
                        break;
                    }
                    None => {}
                }
            }
            let _ = data.close();
        }
    }
    Ok(())
}
