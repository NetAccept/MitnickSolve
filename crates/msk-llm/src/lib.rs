//! LLM provider port and an in-process scripted provider used by M0 tests.
//!
//! Real providers stream deltas and tool-call fragments; the M0 interface is
//! request/response so the kernel loop can be exercised without network access.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader};
use std::sync::Mutex;
use std::time::Duration;

use msk_core::ToolCall;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

impl Message {
    pub fn text(role: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: role.into(),
            content: content.into(),
            tool_call_id: None,
            tool_name: None,
            tool_calls: Vec::new(),
        }
    }

    pub fn assistant_tool_calls(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: "assistant".into(),
            content: content.into(),
            tool_call_id: None,
            tool_name: None,
            tool_calls,
        }
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: "tool".into(),
            content: content.into(),
            tool_call_id: Some(call_id.into()),
            tool_name: Some(tool_name.into()),
            tool_calls: Vec::new(),
        }
    }
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
    #[error("protocol: {0}")]
    Protocol(String),
    #[error("auth: {0}")]
    Auth(String),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("scripted provider exhausted")]
    Exhausted,
}

#[derive(Debug, Clone)]
pub enum LlmDelta {
    Text(String),
}

pub trait LlmProvider: Send + Sync {
    fn id(&self) -> &str;
    fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError>;

    /// Default streaming behavior: call `complete`, then emit the full text.
    /// Providers with native streaming override this.
    fn stream(
        &self,
        request: &LlmRequest,
        sink: &mut dyn FnMut(LlmDelta),
    ) -> Result<LlmResponse, LlmError> {
        let response = self.complete(request)?;
        if !response.text.is_empty() {
            sink(LlmDelta::Text(response.text.clone()));
        }
        Ok(response)
    }
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

/// Offline provider for the interactive session loop. It echoes the most
/// recent user message, which makes session, turn, and context behavior
/// testable without a network or a model.
pub struct EchoProvider {
    id: String,
}

impl EchoProvider {
    pub fn new(id: impl Into<String>) -> Self {
        Self { id: id.into() }
    }
}

impl LlmProvider for EchoProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let text = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == "user" && message.tool_call_id.is_none())
            .map(|message| message.content.clone())
            .unwrap_or_default();
        Ok(LlmResponse {
            text: format!("echo: {text}"),
            tool_calls: Vec::new(),
            usage_tokens: 0,
        })
    }
}

#[derive(Debug, Clone)]
pub struct OpenAiConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    /// Optional reasoning effort for models that support it (`minimal`, `low`,
    /// `medium`, `high`).
    pub effort: Option<String>,
}

pub struct OpenAiProvider {
    id: String,
    config: OpenAiConfig,
    agent: ureq::Agent,
}

pub struct OpenAiResponsesProvider {
    id: String,
    config: OpenAiConfig,
    agent: ureq::Agent,
}

fn validate_openai_config(config: &OpenAiConfig) -> Result<(), LlmError> {
    if config.base_url.trim().is_empty() {
        return Err(LlmError::Config("base_url is empty".into()));
    }
    if config.model.trim().is_empty() {
        return Err(LlmError::Config("model is empty".into()));
    }
    if let Some(effort) = &config.effort
        && !matches!(effort.as_str(), "minimal" | "low" | "medium" | "high")
    {
        return Err(LlmError::Config(format!(
            "unknown reasoning effort: {effort}"
        )));
    }
    if config.api_key.trim().is_empty() {
        return Err(LlmError::Config("api_key is empty".into()));
    }
    Ok(())
}

fn openai_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(120)))
        .http_status_as_error(false)
        .build()
        .into()
}

impl OpenAiResponsesProvider {
    pub fn new(config: OpenAiConfig) -> Result<Self, LlmError> {
        validate_openai_config(&config)?;
        Ok(Self {
            id: format!("openai-responses:{}", config.model),
            config,
            agent: openai_agent(),
        })
    }
}

impl LlmProvider for OpenAiResponsesProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn complete(&self, request: &LlmRequest) -> Result<LlmResponse, LlmError> {
        let url = format!("{}/responses", self.config.base_url.trim_end_matches('/'));
        let body = responses_request(&self.config.model, self.config.effort.as_deref(), request);
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
        parse_responses_response(&value)
    }
}

impl OpenAiProvider {
    pub fn new(config: OpenAiConfig) -> Result<Self, LlmError> {
        validate_openai_config(&config)?;
        Ok(Self {
            id: format!("openai:{}", config.model),
            config,
            agent: openai_agent(),
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
        let body = openai_request(&self.config.model, self.config.effort.as_deref(), request);
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

    fn stream(
        &self,
        request: &LlmRequest,
        sink: &mut dyn FnMut(LlmDelta),
    ) -> Result<LlmResponse, LlmError> {
        let url = format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut body = openai_request(&self.config.model, self.config.effort.as_deref(), request);
        body["stream"] = json!(true);
        body["stream_options"] = json!({"include_usage": true});

        let mut response = self
            .agent
            .post(&url)
            .header("Authorization", &format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .send_json(&body)
            .map_err(|err| LlmError::Http(err.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            let text = response
                .body_mut()
                .read_to_string()
                .map_err(|err| LlmError::Http(err.to_string()))?;
            return Err(LlmError::Http(format!(
                "HTTP {}: {}",
                status.as_u16(),
                truncate(&text, 800)
            )));
        }

        let mut text = String::new();
        let mut calls: BTreeMap<usize, (String, String, String)> = BTreeMap::new();
        let mut usage_tokens = 0u64;
        read_sse(response.body_mut().as_reader(), |_event, data| {
            if data.trim() == "[DONE]" {
                return Ok(());
            }
            let value: Value = serde_json::from_str(data)
                .map_err(|err| LlmError::Provider(format!("SSE JSON: {err}")))?;
            if let Some(total) = value
                .get("usage")
                .and_then(|usage| usage.get("total_tokens"))
                .and_then(Value::as_u64)
            {
                usage_tokens = total;
            }
            let Some(choice) = value
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|choices| choices.first())
            else {
                return Ok(());
            };
            let Some(delta) = choice.get("delta") else {
                return Ok(());
            };
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                if !content.is_empty() {
                    text.push_str(content);
                    sink(LlmDelta::Text(content.to_string()));
                }
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in tool_calls {
                    let index = call.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
                    let entry = calls.entry(index).or_default();
                    if let Some(id) = call.get("id").and_then(Value::as_str) {
                        entry.0 = id.to_string();
                    }
                    if let Some(name) = call
                        .get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                    {
                        entry.1 = name.to_string();
                    }
                    if let Some(arguments) = call
                        .get("function")
                        .and_then(|function| function.get("arguments"))
                        .and_then(Value::as_str)
                    {
                        entry.2.push_str(arguments);
                    }
                }
            }
            Ok(())
        })?;

        let tool_calls = calls
            .into_iter()
            .map(|(index, (id, name, arguments))| {
                let args = serde_json::from_str(&arguments).unwrap_or(Value::Null);
                ToolCall {
                    id: if id.is_empty() {
                        format!("call-{index}")
                    } else {
                        id
                    },
                    tool: name,
                    args,
                }
            })
            .collect();

        Ok(LlmResponse {
            text,
            tool_calls,
            usage_tokens,
        })
    }
}

fn chat_message(message: &Message) -> Value {
    if let Some(call_id) = &message.tool_call_id {
        return json!({
            "role": "tool",
            "tool_call_id": call_id,
            "content": message.content
        });
    }
    if !message.tool_calls.is_empty() {
        let calls: Vec<Value> = message
            .tool_calls
            .iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {
                        "name": call.tool,
                        "arguments": serde_json::to_string(&call.args)
                            .unwrap_or_else(|_| "{}".to_string())
                    }
                })
            })
            .collect();
        let content = if message.content.is_empty() {
            Value::Null
        } else {
            json!(message.content)
        };
        return json!({
            "role": "assistant",
            "content": content,
            "tool_calls": calls
        });
    }
    json!({"role": message.role, "content": message.content})
}

fn read_sse<R, F>(reader: R, mut on_event: F) -> Result<(), LlmError>
where
    R: std::io::Read,
    F: FnMut(&str, &str) -> Result<(), LlmError>,
{
    let mut lines = BufReader::new(reader).lines();
    let mut event = String::new();
    let mut data = String::new();
    while let Some(line) = lines.next() {
        let line = line.map_err(|err| LlmError::Http(err.to_string()))?;
        if line.is_empty() {
            if !data.is_empty() {
                on_event(&event, &data)?;
            }
            event.clear();
            data.clear();
            continue;
        }
        if let Some(value) = line.strip_prefix("event:") {
            event = value.trim().to_string();
        } else if let Some(value) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(value.trim_start());
        }
    }
    if !data.is_empty() {
        on_event(&event, &data)?;
    }
    Ok(())
}

fn openai_request(model: &str, effort: Option<&str>, request: &LlmRequest) -> Value {
    let messages: Vec<Value> = request.messages.iter().map(chat_message).collect();
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
    if let Some(effort) = effort {
        body["reasoning_effort"] = json!(effort);
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

fn responses_request(model: &str, effort: Option<&str>, request: &LlmRequest) -> Value {
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for message in &request.messages {
        if let Some(call_id) = &message.tool_call_id {
            input.push(json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": message.content
            }));
        } else if !message.tool_calls.is_empty() {
            if !message.content.is_empty() {
                input.push(json!({
                    "role": "assistant",
                    "content": message.content
                }));
            }
            for call in &message.tool_calls {
                input.push(json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.tool,
                    "arguments": serde_json::to_string(&call.args)
                        .unwrap_or_else(|_| "{}".to_string())
                }));
            }
        } else if message.role == "system" {
            instructions.push(message.content.clone());
        } else {
            input.push(json!({
                "role": message.role,
                "content": message.content
            }));
        }
    }

    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.parameters
            })
        })
        .collect();

    let mut body = json!({"model": model, "input": input});
    if !instructions.is_empty() {
        body["instructions"] = json!(instructions.join("\n\n"));
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = json!("auto");
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_output_tokens"] = json!(max_tokens);
    }
    if let Some(effort) = effort {
        body["reasoning"] = json!({"effort": effort});
    }
    body
}

pub fn parse_responses_response(value: &Value) -> Result<LlmResponse, LlmError> {
    let output = value
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| LlmError::Provider("response has no output".into()))?;

    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for (index, item) in output.iter().enumerate() {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                if let Some(content) = item.get("content").and_then(Value::as_array) {
                    for part in content {
                        if part.get("type").and_then(Value::as_str) == Some("output_text") {
                            if let Some(part_text) = part.get("text").and_then(Value::as_str) {
                                text.push_str(part_text);
                            }
                        }
                    }
                }
            }
            Some("function_call") => {
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| LlmError::Provider("function_call has no name".into()))?;
                let arguments = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                let args = serde_json::from_str(arguments)
                    .map_err(|err| LlmError::Provider(format!("function arguments: {err}")))?;
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("call-{index}"));
                tool_calls.push(ToolCall {
                    id: call_id,
                    tool: name.to_string(),
                    args,
                });
            }
            _ => {}
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
            messages: vec![Message::text("user", "solve")],
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
            messages: vec![Message::text("user", "hi")],
            tools: vec![ToolSchema {
                name: "echo".into(),
                description: "echo".into(),
                parameters: json!({"type": "object"}),
            }],
            max_tokens: Some(10),
        };
        let body = openai_request("test-model", None, &request);
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["tools"][0]["function"]["name"], "echo");
        let body = openai_request("test-model", Some("high"), &request);
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn parses_responses_function_calls() {
        let value = json!({
            "output": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "done"}]
                },
                {
                    "type": "function_call",
                    "name": "session_exec",
                    "arguments": "{\"command\":\"echo hi\"}",
                    "call_id": "call_1"
                }
            ],
            "usage": {"total_tokens": 9}
        });
        let response = parse_responses_response(&value).unwrap();
        assert_eq!(response.text, "done");
        assert_eq!(response.tool_calls.len(), 1);
        assert_eq!(response.tool_calls[0].tool, "session_exec");
        assert_eq!(response.tool_calls[0].args["command"], "echo hi");
        assert_eq!(response.usage_tokens, 9);
    }

    #[test]
    fn responses_request_uses_native_tool_items() {
        let request = LlmRequest {
            messages: vec![
                Message::text("system", "sys"),
                Message::text("user", "go"),
                Message::assistant_tool_calls(
                    "",
                    vec![ToolCall {
                        id: "call_1".into(),
                        tool: "echo".into(),
                        args: json!({"x": 1}),
                    }],
                ),
                Message::tool_result("call_1", "echo", "{\"ok\":true}"),
            ],
            tools: vec![ToolSchema {
                name: "echo".into(),
                description: "echo".into(),
                parameters: json!({"type": "object"}),
            }],
            max_tokens: Some(64),
        };
        let body = responses_request("test-model", None, &request);
        let body_with_effort = responses_request("test-model", Some("low"), &request);
        assert_eq!(body_with_effort["reasoning"]["effort"], "low");
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["max_output_tokens"], 64);
    }
}
