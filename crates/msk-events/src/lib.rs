//! Append-only event log. Every model call, tool call, policy decision, and
//! envd interaction is recorded here so runs can be replayed and audited.

use std::collections::HashSet;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

/// Current event envelope version. Additive fields keep this at 1; readers
/// must ignore unknown fields so newer writers stay compatible.
pub const SCHEMA_VERSION: u32 = 1;

static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    /// Envelope version, written first so log consumers can branch on it.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// Process-local monotonic sequence number. The JSONL sink assigns it
    /// while holding the append lock, so log order and seq order agree.
    /// Consumers must not compare seq values across processes; it exists to
    /// detect gaps within one log.
    #[serde(default)]
    pub seq: u64,
    pub id: String,
    pub ts: String,
    pub task: Option<String>,
    pub kind: String,
    pub actor: String,
    pub parents: Vec<String>,
    pub data: serde_json::Value,
}

impl Event {
    pub fn new(kind: impl Into<String>, actor: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            seq: 0,
            id: Uuid::new_v4().to_string(),
            ts: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_else(|_| String::from("1970-01-01T00:00:00Z")),
            task: None,
            kind: kind.into(),
            actor: actor.into(),
            parents: Vec::new(),
            data,
        }
    }

    pub fn with_task(mut self, task: impl Into<String>) -> Self {
        self.task = Some(task.into());
        self
    }

    pub fn with_parents(mut self, parents: Vec<String>) -> Self {
        self.parents = parents;
        self
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EventError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
}

pub trait EventSink: Send + Sync {
    fn emit(&self, event: &Event) -> Result<(), EventError>;
}

pub struct JsonlSink {
    writer: Mutex<BufWriter<File>>,
}

impl JsonlSink {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, EventError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            writer: Mutex::new(BufWriter::new(file)),
        })
    }
}

impl EventSink for JsonlSink {
    fn emit(&self, event: &Event) -> Result<(), EventError> {
        let mut writer = self.writer.lock().expect("event writer poisoned");
        let mut event = event.clone();
        if event.seq == 0 {
            event.seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
        }
        serde_json::to_writer(&mut *writer, &event)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    }
}

pub fn read_events(path: impl AsRef<Path>) -> Result<Vec<Event>, EventError> {
    let path = path.as_ref();
    let file = match File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut events = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        events.push(serde_json::from_str(&line)?);
    }
    Ok(events)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IssueSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone)]
pub struct EventIssue {
    pub index: usize,
    pub id: String,
    pub severity: IssueSeverity,
    pub message: String,
}

impl fmt::Display for EventIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let level = match self.severity {
            IssueSeverity::Error => "error",
            IssueSeverity::Warning => "warning",
        };
        write!(
            formatter,
            "{} at event {} ({}): {}",
            level, self.index, self.id, self.message
        )
    }
}

/// Checks envelope integrity without executing any tool. Errors mean the log
/// cannot be trusted for replay; warnings point at older or incomplete logs.
pub fn check_events(events: &[Event]) -> Vec<EventIssue> {
    let mut issues = Vec::new();
    let mut seen = HashSet::new();
    let mut last_seq = 0u64;

    for (index, event) in events.iter().enumerate() {
        if event.schema_version != SCHEMA_VERSION {
            issues.push(EventIssue {
                index,
                id: event.id.clone(),
                severity: IssueSeverity::Error,
                message: format!(
                    "unsupported schema_version {} (expected {})",
                    event.schema_version, SCHEMA_VERSION
                ),
            });
        }
        if !seen.insert(event.id.clone()) {
            issues.push(EventIssue {
                index,
                id: event.id.clone(),
                severity: IssueSeverity::Error,
                message: "duplicate event id".into(),
            });
        }
        if event.seq == 0 {
            issues.push(EventIssue {
                index,
                id: event.id.clone(),
                severity: IssueSeverity::Warning,
                message: "event has no seq (legacy or non-JSONL sink)".into(),
            });
        } else {
            if event.seq <= last_seq {
                issues.push(EventIssue {
                    index,
                    id: event.id.clone(),
                    severity: IssueSeverity::Error,
                    message: format!(
                        "seq {} is not greater than previous {}",
                        event.seq, last_seq
                    ),
                });
            } else if last_seq > 0 && event.seq != last_seq + 1 {
                issues.push(EventIssue {
                    index,
                    id: event.id.clone(),
                    severity: IssueSeverity::Warning,
                    message: format!("seq gap: {} -> {}", last_seq, event.seq),
                });
            }
            last_seq = event.seq.max(last_seq);
        }
        for parent in &event.parents {
            if !seen.contains(parent) {
                issues.push(EventIssue {
                    index,
                    id: event.id.clone(),
                    severity: IssueSeverity::Warning,
                    message: format!("parent {parent} is not in this log"),
                });
            }
        }
    }
    issues
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jsonl_roundtrip() {
        let path = std::env::temp_dir().join(format!("msk-events-{}.jsonl", Uuid::new_v4()));
        let sink = JsonlSink::open(&path).unwrap();
        sink.emit(&Event::new(
            "tool.call",
            "kernel",
            serde_json::json!({"tool": "exec"}),
        ))
        .unwrap();
        sink.emit(&Event::new(
            "tool.result",
            "kernel",
            serde_json::json!({"ok": true}),
        ))
        .unwrap();

        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "tool.call");
        assert_eq!(events[0].schema_version, SCHEMA_VERSION);
        assert!(events[1].seq > events[0].seq);
        assert!(events[0].ts.starts_with("20"));
        std::fs::remove_file(path).ok();
    }

    #[test]
    fn old_events_without_envelope_fields_still_load() {
        let line = r#"{"id":"e1","ts":"2025-01-01T00:00:00Z","task":null,"kind":"tool.call","actor":"kernel","parents":[],"data":{}}"#;
        let event: Event = serde_json::from_str(line).unwrap();
        assert_eq!(event.schema_version, SCHEMA_VERSION);
        assert_eq!(event.seq, 0);
    }

    #[test]
    fn check_reports_gaps_duplicates_and_bad_schema() {
        let mut first = Event::new("a", "test", serde_json::json!({}));
        first.seq = 1;
        let mut second = Event::new("b", "test", serde_json::json!({}));
        second.seq = 3;
        let mut third = Event::new("c", "test", serde_json::json!({}));
        third.seq = 2;
        third.schema_version = 99;
        let duplicate = first.clone();

        let issues = check_events(&[first, second, third, duplicate]);
        assert!(issues.iter().any(|issue| issue.message.contains("seq gap")));
        assert!(
            issues
                .iter()
                .any(|issue| issue.message.contains("not greater"))
        );
        assert!(
            issues
                .iter()
                .any(|issue| issue.message.contains("schema_version"))
        );
        assert!(
            issues
                .iter()
                .any(|issue| issue.message.contains("duplicate"))
        );
    }

    #[test]
    fn check_accepts_a_clean_log() {
        let mut first = Event::new("a", "test", serde_json::json!({}));
        first.seq = 1;
        let mut second = Event::new("b", "test", serde_json::json!({}));
        second.seq = 2;
        second.parents.push(first.id.clone());
        assert!(check_events(&[first, second]).is_empty());
    }
}
