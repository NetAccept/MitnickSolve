//! Human-readable reconstruction of a task transcript from the event log.
//! This is the read-only half of replay: no tool is executed and no state is
//! mutated. A resumed session receives the transcript as a pinned system
//! message and must recreate any envd sessions it still needs.

use msk_events::Event;
use serde_json::Value;

fn truncate(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_string();
    }
    let marker = format!("… [{} chars omitted] …", count.saturating_sub(max_chars));
    let marker_len = marker.chars().count();
    let budget = max_chars.saturating_sub(marker_len);
    let head_len = budget / 2;
    let tail_len = budget - head_len;
    let head: String = text.chars().take(head_len).collect();
    let tail: String = text.chars().skip(count - tail_len).collect();
    format!("{head}{marker}{tail}")
}

fn value_text(value: &Value, max_chars: usize) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    truncate(&text, max_chars)
}

/// Builds a compact transcript for one task (or all tasks when `task` is
/// None). Unknown event kinds are ignored; the raw log remains the authority.
pub fn transcript(events: &[Event], task: Option<&str>) -> String {
    let mut out = String::new();
    for event in events {
        if let Some(task) = task {
            if event.task.as_deref() != Some(task) {
                continue;
            }
        }
        let task_label = if task.is_none() {
            event
                .task
                .as_deref()
                .map(|task| format!("[{task}] "))
                .unwrap_or_default()
        } else {
            String::new()
        };
        match event.kind.as_str() {
            "turn.start" => {
                let turn = event.data.get("turn").and_then(Value::as_u64).unwrap_or(0);
                let user = event.data.get("user").and_then(Value::as_str).unwrap_or("");
                out.push_str(&format!("\n{task_label}[turn {turn}] user: {user}\n"));
            }
            "llm.response" => {
                let text = event.data.get("text").and_then(Value::as_str).unwrap_or("");
                let calls = event
                    .data
                    .get("tool_calls")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                if !text.is_empty() {
                    out.push_str(&format!("assistant: {text}\n"));
                }
                if calls > 0 {
                    out.push_str(&format!("assistant: [{calls} tool call(s)]\n"));
                }
            }
            "tool.call" => {
                let tool = event
                    .data
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let args = event.data.get("args").cloned().unwrap_or(Value::Null);
                out.push_str(&format!("tool {tool} args: {}\n", value_text(&args, 400)));
            }
            "tool.result" => {
                let tool = event
                    .data
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let failed = event
                    .data
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || event.data.get("ok").and_then(Value::as_bool) == Some(false);
                let body = event
                    .data
                    .get("observation")
                    .or_else(|| event.data.get("structured"))
                    .cloned()
                    .unwrap_or(Value::Null);
                out.push_str(&format!(
                    "tool {tool} {}: {}\n",
                    if failed { "error" } else { "result" },
                    value_text(&body, 600)
                ));
            }
            "tool.error" => {
                let tool = event
                    .data
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let error = event
                    .data
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                out.push_str(&format!("tool {tool} error: {error}\n"));
            }
            "mode.switch" => {
                let mode = event
                    .data
                    .get("mode")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                out.push_str(&format!("mode: switched to {mode}\n"));
            }
            "approval.request" => {
                let tool = event
                    .data
                    .get("tool")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let reason = event
                    .data
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                out.push_str(&format!("approval requested for {tool}: {reason}\n"));
            }
            "approval.result" => {
                let approved = event
                    .data
                    .get("approved")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                out.push_str(&format!(
                    "approval {}\n",
                    if approved { "approved" } else { "denied" }
                ));
            }
            "context.compact" => {
                let removed = event
                    .data
                    .get("removed")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                out.push_str(&format!(
                    "[context compacted: {removed} messages removed]\n"
                ));
            }
            "data.put" => {
                let id = event.data.get("id").and_then(Value::as_str).unwrap_or("?");
                let bytes = event.data.get("bytes").and_then(Value::as_u64).unwrap_or(0);
                out.push_str(&format!("[artifact {id}: {bytes} bytes]\n"));
            }
            "turn.end" => {
                let status = event
                    .data
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let message = event
                    .data
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                if message.is_empty() {
                    out.push_str(&format!("turn end: {status}\n"));
                } else {
                    out.push_str(&format!("turn end: {status} — {message}\n"));
                }
            }
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Transcript capped to `max_chars` for use as a resumed-session preamble.
pub fn summarize(events: &[Event], task: Option<&str>, max_chars: usize) -> String {
    truncate(&transcript(events, task), max_chars)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(kind: &str, data: Value) -> Event {
        Event::new(kind, "test", data).with_task("task-1")
    }

    #[test]
    fn transcript_reconstructs_a_turn() {
        let events = vec![
            event("turn.start", json!({"turn": 1, "user": "run echo"})),
            event(
                "llm.response",
                json!({"text": "", "tool_calls": 1, "usage_tokens": 1}),
            ),
            event(
                "tool.call",
                json!({"tool": "exec", "args": {"argv": ["echo"]}}),
            ),
            event(
                "tool.result",
                json!({"tool": "exec", "ok": true, "observation": "{\"stdout\":\"hi\"}"}),
            ),
            event("llm.response", json!({"text": "done", "tool_calls": 0})),
            event("turn.end", json!({"status": "awaiting_input", "turn": 1})),
        ];
        let text = transcript(&events, Some("task-1"));
        assert!(text.contains("user: run echo"));
        assert!(text.contains("[1 tool call(s)]"));
        assert!(text.contains("tool exec args"));
        assert!(text.contains("stdout"));
        assert!(text.contains("assistant: done"));
        assert!(text.contains("turn end: awaiting_input"));
        assert!(!text.contains("task-1"));
    }

    #[test]
    fn summarize_caps_long_transcripts() {
        let mut events = Vec::new();
        for index in 0..30 {
            events.push(event(
                "turn.start",
                json!({"turn": index, "user": "x".repeat(500)}),
            ));
        }
        let summary = summarize(&events, Some("task-1"), 300);
        assert!(summary.chars().count() <= 300);
        assert!(summary.contains("chars omitted"));
    }

    #[test]
    fn task_filter_excludes_other_tasks() {
        let other = Event::new("turn.start", "test", json!({"turn": 2, "user": "other"}))
            .with_task("task-2");
        let events = vec![
            event("turn.start", json!({"turn": 1, "user": "mine"})),
            other,
        ];
        let text = transcript(&events, Some("task-1"));
        assert!(text.contains("mine"));
        assert!(!text.contains("other"));
    }
}
