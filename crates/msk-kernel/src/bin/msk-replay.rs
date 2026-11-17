//! Read-only replay/audit tool for JSONL event logs.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Parser, ValueEnum};
use msk_events::{Event, IssueSeverity, check_events, read_events};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Format {
    Text,
    Jsonl,
}

#[derive(Debug, Parser)]
#[command(name = "msk-replay", version)]
struct Cli {
    /// Event log produced by the kernel or mskd task.
    #[arg(long, default_value = ".msk/events.jsonl")]
    events: PathBuf,
    /// Read from the daemon's SQLite event index instead of the JSONL file.
    #[arg(long)]
    db: Option<PathBuf>,
    /// Only include events for this task ID.
    #[arg(long)]
    task: Option<String>,
    /// Only include events with seq >= this value.
    #[arg(long)]
    from_seq: Option<u64>,
    /// Output a transcript or the filtered raw events.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
    /// Validate schema, seq order, duplicate IDs, and parent links.
    #[arg(long)]
    check: bool,
    /// Transcript cap in characters; 0 leaves it unlimited.
    #[arg(long, default_value_t = 12_000)]
    max_chars: usize,
    /// Maximum events to read from --db.
    #[arg(long, default_value_t = 10_000)]
    limit: usize,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let events = if let Some(db) = &cli.db {
        let store =
            msk_store::SqliteStore::open(db).with_context(|| format!("open {}", db.display()))?;
        store
            .query(&msk_store::EventQuery {
                task: cli.task.clone(),
                from_seq: cli.from_seq,
                limit: cli.limit,
                ..Default::default()
            })
            .with_context(|| format!("query {}", db.display()))?
    } else {
        read_events(&cli.events).with_context(|| format!("read {}", cli.events.display()))?
    };

    if cli.check {
        let issues = check_events(&events);
        for issue in &issues {
            eprintln!("{issue}");
        }
        let errors = issues
            .iter()
            .filter(|issue| issue.severity == IssueSeverity::Error)
            .count();
        eprintln!(
            "checked {} event(s): {} error(s), {} warning(s)",
            events.len(),
            errors,
            issues.len() - errors
        );
        if errors > 0 {
            bail!("event log has {errors} replay-blocking issue(s)");
        }
    }

    let filtered: Vec<Event> = events
        .into_iter()
        .filter(|event| {
            cli.task
                .as_deref()
                .map_or(true, |task| event.task.as_deref() == Some(task))
                && cli.from_seq.map_or(true, |seq| event.seq >= seq)
        })
        .collect();

    match cli.format {
        Format::Text => {
            let text = if cli.max_chars == 0 {
                msk_kernel::replay::transcript(&filtered, cli.task.as_deref())
            } else {
                msk_kernel::replay::summarize(&filtered, cli.task.as_deref(), cli.max_chars)
            };
            if text.is_empty() {
                println!("(no matching events)");
            } else {
                println!("{text}");
            }
        }
        Format::Jsonl => {
            for event in &filtered {
                println!("{}", serde_json::to_string(event)?);
            }
        }
    }
    Ok(())
}
