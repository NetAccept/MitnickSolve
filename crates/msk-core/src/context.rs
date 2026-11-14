//! Context budgeting and tool-output handling.
//!
//! Tool results larger than `inline_tool_bytes` are stored as artifacts and
//! replaced by a head/tail summary plus the artifact ID. Compaction removes
//! old observations while preserving pinned facts and the most recent ones.
//! Native tool calls are removed only together with every matching tool
//! result, because providers reject a result whose call is missing (and vice
//! versa).

use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::ToolCall;

#[derive(Debug, Clone)]
pub struct ContextLimits {
    pub max_tokens: usize,
    pub inline_tool_bytes: usize,
    pub summary_head_chars: usize,
    pub summary_tail_chars: usize,
    pub min_recent_observations: usize,
}

impl Default for ContextLimits {
    fn default() -> Self {
        Self {
            max_tokens: 100_000,
            inline_tool_bytes: 2048,
            summary_head_chars: 400,
            summary_tail_chars: 200,
            min_recent_observations: 4,
        }
    }
}

impl ContextLimits {
    pub fn with_max_tokens(mut self, max_tokens: usize) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    pub fn with_min_recent_observations(mut self, min_recent_observations: usize) -> Self {
        self.min_recent_observations = min_recent_observations;
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Observation,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Observation => "observation",
        }
    }

    /// Provider-facing role. Observations are delivered as user messages.
    pub fn as_llm_role(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Observation => "user",
        }
    }
}

impl fmt::Display for Role {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextMessage {
    pub role: Role,
    pub content: String,
    pub pinned: bool,
    pub observation: bool,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone)]
pub struct ContextManager {
    limits: ContextLimits,
    messages: Vec<ContextMessage>,
    compactions: usize,
}

impl ContextManager {
    pub fn new(limits: ContextLimits) -> Self {
        Self {
            limits,
            messages: Vec::new(),
            compactions: 0,
        }
    }

    /// Restores a serialized message list (checkpoint) under new limits.
    pub fn restore(limits: ContextLimits, messages: Vec<ContextMessage>) -> Self {
        Self {
            limits,
            messages,
            compactions: 0,
        }
    }

    pub fn limits(&self) -> &ContextLimits {
        &self.limits
    }

    pub fn push(&mut self, role: Role, content: impl Into<String>) {
        self.messages.push(ContextMessage {
            role,
            content: content.into(),
            pinned: false,
            observation: false,
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        });
    }

    pub fn push_pinned(&mut self, role: Role, content: impl Into<String>) {
        self.messages.push(ContextMessage {
            role,
            content: content.into(),
            pinned: true,
            observation: false,
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        });
    }

    pub fn push_observation(&mut self, tool: &str, content: impl Into<String>) {
        self.messages.push(ContextMessage {
            role: Role::Observation,
            content: format!("observation {tool}: {}", content.into()),
            pinned: false,
            observation: true,
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        });
    }

    pub fn push_assistant_tool_calls(
        &mut self,
        content: impl Into<String>,
        tool_calls: Vec<ToolCall>,
    ) {
        self.messages.push(ContextMessage {
            role: Role::Assistant,
            content: content.into(),
            pinned: false,
            observation: false,
            tool_call_id: None,
            tool_name: None,
            tool_calls,
        });
    }

    pub fn push_tool_result(
        &mut self,
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) {
        self.messages.push(ContextMessage {
            role: Role::Observation,
            content: content.into(),
            pinned: false,
            observation: true,
            tool_call_id: Some(call_id.into()),
            tool_name: Some(tool_name.into()),
            tool_calls: Vec::new(),
        });
    }

    pub fn messages(&self) -> &[ContextMessage] {
        &self.messages
    }

    pub fn approx_tokens(&self) -> usize {
        self.messages.iter().map(message_tokens).sum()
    }

    pub fn compactions(&self) -> usize {
        self.compactions
    }

    /// Removes the oldest removable groups until the context fits or only the
    /// minimum recent observations remain. An assistant message that issued
    /// tool calls is removed together with all of its tool results, so the
    /// provider always receives a valid call/result sequence. Returns the
    /// number of messages removed.
    pub fn compact(&mut self) -> usize {
        if self.approx_tokens() <= self.limits.max_tokens {
            return 0;
        }

        let total_observations = self
            .messages
            .iter()
            .filter(|message| message.observation && !message.pinned)
            .count();
        if total_observations <= self.limits.min_recent_observations {
            return 0;
        }
        let removable_observations = total_observations - self.limits.min_recent_observations;

        let mut keep = vec![true; self.messages.len()];
        let mut removed = 0;
        let mut removed_observations = 0;
        for group in self.removal_groups() {
            if removed_observations + group.observations > removable_observations {
                continue;
            }
            for index in group.indices {
                if keep[index] {
                    keep[index] = false;
                    removed += 1;
                }
            }
            removed_observations += group.observations;
            if self.tokens_with(&keep) <= self.limits.max_tokens {
                break;
            }
        }
        if removed > 0 {
            let mut messages = Vec::with_capacity(self.messages.len() - removed);
            for (index, message) in self.messages.iter().enumerate() {
                if keep[index] {
                    messages.push(message.clone());
                }
            }
            self.messages = messages;
            self.compactions += 1;
        }
        removed
    }

    /// Groups that can be removed without breaking a native tool call/result
    /// pair. A tool-call group is removable only when every call has a result
    /// and none of those messages are pinned. Legacy observations are single
    /// message groups.
    fn removal_groups(&self) -> Vec<RemovalGroup> {
        let mut groups = Vec::new();
        let mut claimed = vec![false; self.messages.len()];

        for (index, message) in self.messages.iter().enumerate() {
            if message.pinned || message.tool_calls.is_empty() {
                continue;
            }
            let call_ids: HashSet<&str> = message
                .tool_calls
                .iter()
                .map(|call| call.id.as_str())
                .collect();
            let mut indices = vec![index];
            let mut observations = 0;
            let mut blocked = false;
            for (other_index, other) in self.messages.iter().enumerate().skip(index + 1) {
                let Some(call_id) = other.tool_call_id.as_deref() else {
                    continue;
                };
                if !call_ids.contains(call_id) {
                    continue;
                }
                if other.pinned {
                    blocked = true;
                    break;
                }
                indices.push(other_index);
                observations += 1;
            }
            if blocked || observations != message.tool_calls.len() {
                continue;
            }
            for &index in &indices {
                claimed[index] = true;
            }
            groups.push(RemovalGroup {
                indices,
                observations,
            });
        }

        for (index, message) in self.messages.iter().enumerate() {
            if claimed[index] || message.pinned {
                continue;
            }
            if message.observation && message.tool_call_id.is_none() {
                groups.push(RemovalGroup {
                    indices: vec![index],
                    observations: 1,
                });
            }
        }
        groups.sort_by_key(|group| group.indices[0]);
        groups
    }

    fn tokens_with(&self, keep: &[bool]) -> usize {
        self.messages
            .iter()
            .zip(keep)
            .filter(|(_, keep)| **keep)
            .map(|(message, _)| message_tokens(message))
            .sum()
    }
}

struct RemovalGroup {
    indices: Vec<usize>,
    observations: usize,
}

fn message_tokens(message: &ContextMessage) -> usize {
    let mut tokens = approx_tokens(&message.content);
    for call in &message.tool_calls {
        tokens += approx_tokens(&call.id) + approx_tokens(&call.tool);
        tokens += approx_tokens(&call.args.to_string());
    }
    tokens
}

pub fn approx_tokens(text: &str) -> usize {
    text.chars().count().div_ceil(4)
}

/// Keeps the head and tail of a large tool result and marks the omitted middle.
pub fn summarize_output(text: &str, head_chars: usize, tail_chars: usize) -> String {
    let total = text.chars().count();
    if total <= head_chars + tail_chars {
        return text.to_string();
    }
    let head: String = text.chars().take(head_chars).collect();
    let tail: String = text.chars().skip(total - tail_chars).collect();
    format!(
        "{head}… [{} chars omitted] …{tail}",
        total - head_chars - tail_chars
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_is_character_based() {
        assert_eq!(approx_tokens(""), 0);
        assert_eq!(approx_tokens("abcd"), 1);
        assert_eq!(approx_tokens("abcde"), 2);
    }

    #[test]
    fn summary_keeps_head_and_tail() {
        let summary = summarize_output("0123456789abcdef", 4, 4);
        assert!(summary.starts_with("0123"));
        assert!(summary.ends_with("cdef"));
        assert!(summary.contains("chars omitted"));
    }

    #[test]
    fn compact_removes_old_observations() {
        let limits = ContextLimits {
            max_tokens: 20,
            inline_tool_bytes: 2048,
            summary_head_chars: 10,
            summary_tail_chars: 5,
            min_recent_observations: 1,
        };
        let mut context = ContextManager::new(limits);
        context.push_pinned(Role::User, "goal");
        for index in 0..10 {
            context.push_observation("exec", format!("output-{index}-{}", "x".repeat(40)));
        }

        let removed = context.compact();
        assert!(removed > 0);
        assert!(context.messages().iter().any(|message| message.pinned));
        assert_eq!(
            context
                .messages()
                .iter()
                .filter(|message| message.observation)
                .count(),
            1
        );
        assert_eq!(context.compactions(), 1);
    }

    #[test]
    fn compact_keeps_recent_observations() {
        let limits = ContextLimits {
            max_tokens: 2,
            inline_tool_bytes: 2048,
            summary_head_chars: 10,
            summary_tail_chars: 5,
            min_recent_observations: 3,
        };
        let mut context = ContextManager::new(limits);
        for index in 0..6 {
            context.push_observation("exec", format!("output-{index}"));
        }
        context.compact();
        assert_eq!(
            context
                .messages()
                .iter()
                .filter(|message| message.observation)
                .count(),
            3
        );
    }

    #[test]
    fn compact_removes_tool_calls_with_their_results() {
        let limits = ContextLimits {
            max_tokens: 30,
            inline_tool_bytes: 2048,
            summary_head_chars: 10,
            summary_tail_chars: 5,
            min_recent_observations: 1,
        };
        let mut context = ContextManager::new(limits);
        for index in 0..4 {
            context.push_assistant_tool_calls(
                "run a command",
                vec![ToolCall {
                    id: format!("call-{index}"),
                    tool: "exec".into(),
                    args: serde_json::json!({"command": "echo hi"}),
                }],
            );
            context.push_tool_result(
                format!("call-{index}"),
                "exec",
                format!("result-{index}-{}", "x".repeat(80)),
            );
        }

        let removed = context.compact();
        assert!(removed > 0);

        let messages = context.messages();
        for (index, message) in messages.iter().enumerate() {
            if let Some(call_id) = &message.tool_call_id {
                assert!(
                    messages[..index]
                        .iter()
                        .any(|previous| previous.tool_calls.iter().any(|call| &call.id == call_id)),
                    "tool result {call_id} lost its assistant call"
                );
            }
        }
        for message in messages {
            for call in &message.tool_calls {
                assert!(
                    messages
                        .iter()
                        .any(|result| result.tool_call_id.as_deref() == Some(call.id.as_str())),
                    "assistant call {} lost its tool result",
                    call.id
                );
            }
        }
        assert!(
            messages
                .iter()
                .filter(|message| message.observation)
                .count()
                >= 1
        );
    }
}
