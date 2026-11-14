//! Append-only event log. Every model call, tool call, policy decision, and
//! envd interaction is recorded here so runs can be replayed and audited.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
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
        serde_json::to_writer(&mut *writer, event)?;
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
        assert!(events[0].ts.starts_with("20"));
        std::fs::remove_file(path).ok();
    }
}
