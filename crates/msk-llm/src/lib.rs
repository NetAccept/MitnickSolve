//! LLM provider port and an in-process scripted provider used by M0 tests.
//!
//! Real providers stream deltas and tool-call fragments; the M0 interface is
//! request/response so the kernel loop can be exercised without network access.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use msk_core::ToolCall;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

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
    #[error("config: {0}")]
    Config(String),
    #[error("http: {0}")]
    Http(String),
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

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
}

pub struct OpenAiProvider {
    id: String,
    config: OpenAiConfig,
    agent: ureq::Agent,
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig) -> Result<Self, LlmError> {
        if config.base_url.trim().is_empty() {
            return Err(LlmError::Config("base_url is empty".into()));
        }
        if config.model.trim().is_empty() {
            return Err(LlmError::Config("model is empty".into()));
        }
        if config.api_key.trim().is_empty() {
            return Err(LlmError::Config("api_key is empty".into()));
        }
        let agent_config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(120)))
            .http_status_as_error(false)
            .build();
        Ok(Self {
            id: format!("openai:{}", config.model),
            config,
            agent: agent_config.into(),
        })
    }
}

impl LlmProvider for OpenAiProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let body = openai_request(&self.config.model, request);
        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .send_json(&body)
            .map_err(|err| LlmError::Http(err.to_string()))?;
        let status = response.status();
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|err| LlmError::Http(err.to_string()))?;
        if !status.is_success() {
            return Err(LlmError::Http(format!(
                "HTTP {}: {}",
                status.as_u16(),
                truncate(&text, 800)
            )));
        }
        let value: Value = serde_json::from_str(&text)
            .map_err(|err| LlmError::Provider(format!("invalid response JSON: {err}")))?;
        parse_openai_response(&value)
    }
}

fn openai_request(model: &str, request: &LlmRequest) -> Value {
    let messages: Vec<Value> = request
        .messages
        .iter()
        .map(|message| {
            json!({
                "role": message.role.clone(),
                "content": message.content.clone()
            })
        })
        .collect();
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "function": {
                    "name": tool.name.clone(),
                    "description": tool.description.clone(),
                    "parameters": tool.parameters.clone()
                }
            })
        })
        .collect();

    let mut body = json!({
        "model": model,
        "messages": messages,
        "temperature": 0.2
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = json!("auto");
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    body
}

pub fn parse_openai_response(value: &Value) -> Result<LlmResponse, LlmError> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first())
        .ok_or_else(|| LlmError::Provider("response has no choices".into()))?;
    let message = choice
        .get("message")
        .ok_or_else(|| LlmError::Provider("response has no message".into()))?;
    let text = message
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut tool_calls = Vec::new();
    if let Some(calls) = message.get("tool_calls").and_then(Value::as_array) {
        for (index, call) in calls.iter().enumerate() {
            let function = call
                .get("function")
                .ok_or_else(|| LlmError::Provider("tool call has no function".into()))?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .ok_or_else(|| LlmError::Provider("tool call has no name".into()))?;
            let arguments_raw = function
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let args = serde_json::from_str(arguments_raw)
                .map_err(|err| LlmError::Provider(format!("tool arguments: {err}")))?;
            let id = call
                .get("id")
                .and_then(Value::as_str)
                .map(str::to_string)
                .unwrap_or_else(|| format!("call-{index}"));
            tool_calls.push(ToolCall {
                id,
                tool: name.to_string(),
                args,
            });
        }
    }

    let usage_tokens = value
        .get("usage")
        .and_then(|usage| usage.get("total_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    Ok(LlmResponse {
        text,
        tool_calls,
        usage_tokens,
    })
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(max_chars).collect::<String>())
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

    #[test]
    fn parses_openai_tool_calls() {
        let value = json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "session_exec",
                            "arguments": "{\"command\":\"echo hi\"}"
                        }
                    }]
                }
            }],
            "usage": {"total_tokens": 12}
        });
        let response = parse_openai_response(&value).unwrap();
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].tool, "session_exec");
        assert_eq!(response.tool_calls[0].args["command"], "echo hi");
        assert_eq!(response.usage_tokens, 12);
    }

    #[test]
    fn openai_request_includes_tools() {
        let request = LlmRequest {
            messages: vec![Message {
                role: "user".into(),
                content: "hi".into(),
            }],
            tools: vec![ToolSchema {
                name: "echo".into(),
                description: "echo".into(),
                parameters: json!({"type": "object"}),
            }],
            max_tokens: Some(10),
        };
        let body = openai_request("test-model", &request);
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["tools"][0]["function"]["name"], "echo");
    }
}
