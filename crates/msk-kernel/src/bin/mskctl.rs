use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use clap::{Parser, Subcommand};
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
    },
    /// Print one task snapshot.
    State { task: String },
    /// Cancel a running task.
    Cancel { task: String },
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
        } => {
            let result = client.request(
                methods::TASK_CREATE,
                json!({"goal": goal, "demo": demo, "provider": provider}),
            )?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::State { task } => {
            let result = client.request(methods::TASK_STATE, json!({"task_id": task}))?;
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        Command::Cancel { task } => {
            let result = client.request(methods::TASK_CANCEL, json!({"task_id": task}))?;
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
    }
    Ok(())
}
