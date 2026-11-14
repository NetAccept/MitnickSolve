use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
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
    }
    Ok(())
}
