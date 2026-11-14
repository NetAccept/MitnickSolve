//! Context budgeting and tool-output handling.
//!
//! Tool results larger than `inline_tool_bytes` are stored as artifacts and
//! replaced by a head/tail summary plus the artifact ID. Compaction removes
//! old observations while preserving pinned facts and the most recent ones.

use std::fmt;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

#[derive(Debug, Clone)]
pub struct ContextMessage {
    pub role: Role,
    pub content: String,
    pub pinned: bool,
    pub observation: bool,
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

    pub fn limits(&self) -> &ContextLimits {
        &self.limits
    }

    pub fn push(&mut self, role: Role, content: impl Into<String>) {
        self.messages.push(ContextMessage {
            role,
            content: content.into(),
            pinned: false,
            observation: false,
        });
    }

    pub fn push_pinned(&mut self, role: Role, content: impl Into<String>) {
        self.messages.push(ContextMessage {
            role,
            content: content.into(),
            pinned: true,
            observation: false,
        });
    }

    pub fn push_observation(&mut self, tool: &str, content: impl Into<String>) {
        self.messages.push(ContextMessage {
            role: Role::Observation,
            content: format!("observation {tool}: {}", content.into()),
            pinned: false,
            observation: true,
        });
    }

    pub fn messages(&self) -> &[ContextMessage] {
        &self.messages
    }

    pub fn approx_tokens(&self) -> usize {
        self.messages
            .iter()
            .map(|message| approx_tokens(&message.content))
            .sum()
    }

    pub fn compactions(&self) -> usize {
        self.compactions
    }

    /// Removes the oldest unpinned observations until the context fits or only
    /// the minimum recent observations remain. Returns the number removed.
    pub fn compact(&mut self) -> usize {
        if self.approx_tokens() <= self.limits.max_tokens {
            return 0;
        }

        let candidates: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message.observation && !message.pinned)
            .map(|(index, _)| index)
            .collect();
        if candidates.len() <= self.limits.min_recent_observations {
            return 0;
        }

        let removable = candidates.len() - self.limits.min_recent_observations;
        let mut keep = vec![true; self.messages.len()];
        let mut removed = 0;
        for index in candidates.into_iter().take(removable) {
            keep[index] = false;
            removed += 1;
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

    fn tokens_with(&self, keep: &[bool]) -> usize {
        self.messages
            .iter()
            .zip(keep)
            .filter(|(_, keep)| **keep)
            .map(|(message, _)| approx_tokens(&message.content))
            .sum()
    }
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
}
