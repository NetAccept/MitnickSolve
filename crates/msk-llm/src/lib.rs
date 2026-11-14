//! LLM provider port and an in-process scripted provider used by M0 tests.
//!
//! Real providers stream deltas and tool-call fragments; the M0 interface is
//! request/response so the kernel loop can be exercised without network access.

use std::collections::VecDeque;
use std::sync::Mutex;

use msk_core::ToolCall;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub usage_tokens: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("provider error: {0}")]
    Provider(String),
    #[error("scripted provider exhausted")]
    Exhausted,
}

pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &str;
    fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError>;
}

/// Deterministic provider for tests and the M0 kernel demo.
pub struct ScriptedProvider {
    id: String,
    responses: Mutex<VecDeque<LlmResponse>>,
}

impl ScriptedProvider {
    pub fn new(id: impl Into<String>, responses: Vec<LlmResponse>) -> Self {
        Self {
            id: id.into(),
            responses: Mutex::new(responses.into()),
        }
    }
}

impl LlmProvider for ScriptedProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn complete(&self, _request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        self.responses
            .lock()
            .expect("scripted provider poisoned")
            .pop_front()
            .ok_or(LlmError::Exhausted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripted_provider_is_deterministic() {
        let provider = ScriptedProvider::new(
            "scripted",
            vec![LlmResponse {
                text: "calling exec".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    tool: "exec".into(),
                    args: serde_json::json!({"argv": ["echo", "hi"]}),
                }],
                usage_tokens: 10,
            }],
        );
        let request = LlmRequest {
            messages: vec![Message {
                role: "user".into(),
                content: "solve".into(),
            }],
            tools: vec![],
            max_tokens: None,
        };
        let response = provider.complete(&request).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert!(provider.complete(&request).is_err());
    }
}
