//! Shared kernel runtime pieces used by the one-shot demo and the persistent
//! session engine. Keeping the host bridge and tool registry here means both
//! paths exercise the same WASM/policy boundary.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use msk_core::context::{ContextLimits, ContextManager, summarize_output};
use msk_core::{Capability, DataLabel, ObjectRef, PolicyDecision, PolicyTable};
use msk_data::ObjectStore;
use msk_ipc::{Client, methods};
use msk_kernel::policy;
use msk_llm::{LlmDelta, LlmError, LlmProvider, LlmRequest, LlmResponse, Message, ToolSchema};
use msk_mcp::EnvTransport;
use msk_secret::SecretStore;
use msk_wasm::{ToolContext, ToolHost, ToolManifest, WasmtimeRuntime};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// M0 host bridge: the only native capability exposed to WASM tools.
pub struct EnvHost {
    pub client: Arc<Mutex<Box<dyn EnvTransport>>>,
    pub artifacts: Arc<dyn ObjectStore>,
    pub control: Option<Arc<ControlPlane>>,
    pub secrets: Arc<dyn SecretStore>,
    pub agent_depth: u32,
    pub task_id: Option<String>,
    pub default_provider: String,
}

/// Daemon control plane used by spawn_agent/agent_result. The agent process
/// connects with the same per-root token as mskctl.
pub struct ControlPlane {
    client: Mutex<Client>,
}

impl ControlPlane {
    pub fn connect(root: &Path) -> std::io::Result<Self> {
        let mut client = Client::connect(root)?;
        client.hello()?;
        Ok(Self {
            client: Mutex::new(client),
        })
    }

    pub fn spawn_agent(
        &self,
        goal: &str,
        provider: &str,
        max_steps: usize,
        max_context_tokens: usize,
        parent_task: Option<&str>,
        depth: u32,
    ) -> std::result::Result<Value, String> {
        if depth >= 2 {
            return Err("subagent depth limit reached (max 2)".into());
        }
        let mut client = self
            .client
            .lock()
            .map_err(|_| "control client lock poisoned".to_string())?;
        client
            .request(
                methods::TASK_CREATE,
                json!({
                    "goal": goal,
                    "provider": provider,
                    "interactive": false,
                    "headless": true,
                    "approvals": "allow",
                    "agent_depth": depth + 1,
                    "parent_task": parent_task,
                    "max_steps": max_steps,
                    "max_context_tokens": max_context_tokens
                }),
            )
            .map_err(|err| err.to_string())
    }

    pub fn agent_result(&self, task_id: &str, wait_ms: u64) -> std::result::Result<Value, String> {
        let deadline = Instant::now() + Duration::from_millis(wait_ms.min(300_000));
        let mut client = self
            .client
            .lock()
            .map_err(|_| "control client lock poisoned".to_string())?;
        loop {
            let value = client
                .request(methods::TASK_RESULT, json!({"task_id": task_id}))
                .map_err(|err| err.to_string())?;
            let state = value
                .get("task")
                .and_then(|task| task.get("state"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            if matches!(state, "done" | "failed" | "cancelled") || Instant::now() >= deadline {
                return Ok(value);
            }
            thread::sleep(Duration::from_millis(200));
        }
    }

    pub fn blackboard_get(&self, key: &str) -> std::result::Result<Value, String> {
        let mut client = self
            .client
            .lock()
            .map_err(|_| "control client lock poisoned".to_string())?;
        client
            .request(methods::BLACKBOARD_GET, json!({"key": key}))
            .map_err(|err| err.to_string())
    }

    pub fn blackboard_set(
        &self,
        key: &str,
        value: Value,
        expected_version: Option<u64>,
    ) -> std::result::Result<Value, String> {
        let mut client = self
            .client
            .lock()
            .map_err(|_| "control client lock poisoned".to_string())?;
        client
            .request(
                methods::BLACKBOARD_SET,
                json!({
                    "key": key,
                    "value": value,
                    "expected_version": expected_version
                }),
            )
            .map_err(|err| err.to_string())
    }

    pub fn blackboard_list(&self) -> std::result::Result<Value, String> {
        let mut client = self
            .client
            .lock()
            .map_err(|_| "control client lock poisoned".to_string())?;
        client
            .request(methods::BLACKBOARD_LIST, json!({}))
            .map_err(|err| err.to_string())
    }
}

#[derive(Debug, serde::Deserialize)]
struct ReadArtifactArgs {
    id: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize)]
struct HttpRequestArgs {
    #[serde(default = "default_http_method")]
    method: String,
    url: String,
    #[serde(default)]
    headers: HashMap<String, String>,
    #[serde(default)]
    body_base64: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_bytes: Option<usize>,
}

fn default_http_method() -> String {
    "GET".into()
}

/// Host-side HTTP egress for the `http_request` WASM tool. The WASM guest
/// cannot bypass the capability check in `EnvHost::call`; this function only
/// performs the I/O after that check. Redirects and http/https only.
fn http_request(
    input: &[u8],
    secrets: &dyn SecretStore,
    allow_secrets: bool,
) -> Result<Vec<u8>, String> {
    let args: HttpRequestArgs = serde_json::from_slice(input)
        .map_err(|err| format!("invalid http request input: {err}"))?;
    let method = args.method.to_ascii_uppercase();
    if method.is_empty() {
        return Err("method is empty".into());
    }
    if !args.url.starts_with("http://") && !args.url.starts_with("https://") {
        return Err("only http:// and https:// URLs are allowed".into());
    }
    let timeout = Duration::from_millis(args.timeout_ms.unwrap_or(30_000).clamp(100, 120_000));
    let max_bytes = args
        .max_bytes
        .unwrap_or(4 * 1024 * 1024)
        .clamp(1, 16 * 1024 * 1024);
    let body = match &args.body_base64 {
        Some(encoded) => STANDARD
            .decode(encoded)
            .map_err(|err| format!("body_base64: {err}"))?,
        None => Vec::new(),
    };

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into();
    let mut builder = ureq::http::Request::builder()
        .method(method.as_str())
        .uri(&args.url);
    for (name, value) in &args.headers {
        let resolved = if let Some(secret_name) = value.strip_prefix("$secret:") {
            if !allow_secrets {
                return Err("denied: secrets.read not granted".into());
            }
            let secret_name = secret_name.trim();
            let secret = secrets
                .get(secret_name)
                .ok_or_else(|| format!("secret not found: {secret_name}"))?;
            secret.expose().to_string()
        } else {
            value.clone()
        };
        builder = builder.header(name, &resolved);
    }
    let request = builder
        .body(body)
        .map_err(|err| format!("http request: {err}"))?;
    let mut response = agent
        .run(request)
        .map_err(|err| format!("http request failed: {err}"))?;
    let status = response.status().as_u16();
    let mut headers = Vec::new();
    for (name, value) in response.headers().iter() {
        if let Ok(value) = value.to_str() {
            headers.push((name.to_string(), value.to_string()));
        }
    }
    let mut buffer = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut buffer)
        .map_err(|err| format!("http body: {err}"))?;
    let truncated = buffer.len() > max_bytes;
    buffer.truncate(max_bytes);
    let response = json!({
        "status": status,
        "headers": headers,
        "body_base64": STANDARD.encode(&buffer),
        "bytes": buffer.len(),
        "truncated": truncated
    });
    serde_json::to_vec(&response).map_err(|err| err.to_string())
}

impl EnvHost {
    fn read_artifact(&self, input: &[u8]) -> Result<Vec<u8>, String> {
        let args: ReadArtifactArgs = serde_json::from_slice(input)
            .map_err(|err| format!("invalid artifact read input: {err}"))?;
        let bytes = self
            .artifacts
            .get(&args.id)
            .map_err(|err| err.to_string())?;
        let offset = args.offset.unwrap_or(0).min(bytes.len());
        let limit = args.limit.unwrap_or(64 * 1024).min(1024 * 1024);
        let end = offset.saturating_add(limit).min(bytes.len());
        let chunk = &bytes[offset..end];
        let (data, encoding) = match std::str::from_utf8(chunk) {
            Ok(text) => (text.to_string(), "utf8"),
            Err(_) => (STANDARD.encode(chunk), "base64"),
        };
        let response = json!({
            "id": args.id,
            "offset": offset,
            "next_offset": end,
            "eof": end >= bytes.len(),
            "encoding": encoding,
            "data": data
        });
        serde_json::to_vec(&response).map_err(|err| err.to_string())
    }

    pub fn shutdown(&self) {
        if let Ok(mut client) = self.client.lock() {
            client.shutdown();
        }
    }
}

impl ToolHost for EnvHost {
    fn call(&self, ctx: &ToolContext, input: &[u8]) -> Result<Vec<u8>, String> {
        if matches!(ctx.tool.as_str(), "flag_add" | "flag_list") {
            if !ctx.allows(Capability::SharedState) {
                return Err("denied: state.shared not granted".to_string());
            }
            let control = self
                .control
                .as_ref()
                .ok_or_else(|| "control plane is not configured".to_string())?;
            let args: Value = serde_json::from_slice(input)
                .map_err(|err| format!("invalid flag input: {err}"))?;
            let value = if ctx.tool == "flag_add" {
                let flag = args
                    .get("flag")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "flag is required".to_string())?;
                if flag.is_empty() || flag.len() > 4096 {
                    return Err("flag must be 1..4096 bytes".into());
                }
                let source = args.get("source").and_then(Value::as_str);
                let key = format!("flag:{}", sha256_hex(flag));
                let existing = control.blackboard_get(&key)?;
                if !existing.get("value").unwrap_or(&Value::Null).is_null() {
                    json!({"added": false, "key": key, "existing": existing.get("value")})
                } else {
                    let entry = json!({
                        "flag": flag,
                        "source": source,
                        "recorded_ms": now_unix_ms()
                    });
                    control.blackboard_set(&key, entry.clone(), None)?;
                    json!({"added": true, "key": key, "entry": entry})
                }
            } else {
                let listed = control.blackboard_list()?;
                let flags: Vec<Value> = listed
                    .get("entries")
                    .and_then(Value::as_array)
                    .map(|entries| {
                        entries
                            .iter()
                            .filter(|entry| {
                                entry
                                    .get("key")
                                    .and_then(Value::as_str)
                                    .is_some_and(|key| key.starts_with("flag:"))
                            })
                            .map(|entry| entry.get("value").cloned().unwrap_or(Value::Null))
                            .collect()
                    })
                    .unwrap_or_default();
                json!({"flags": flags})
            };
            return serde_json::to_vec(&value).map_err(|err| err.to_string());
        }
        if matches!(
            ctx.tool.as_str(),
            "blackboard_get" | "blackboard_set" | "blackboard_list"
        ) {
            if !ctx.allows(Capability::SharedState) {
                return Err("denied: state.shared not granted".to_string());
            }
            let control = self
                .control
                .as_ref()
                .ok_or_else(|| "control plane is not configured".to_string())?;
            let args: Value = serde_json::from_slice(input)
                .map_err(|err| format!("invalid blackboard input: {err}"))?;
            let value = match ctx.tool.as_str() {
                "blackboard_get" => {
                    let key = args
                        .get("key")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "key is required".to_string())?;
                    control.blackboard_get(key)?
                }
                "blackboard_set" => {
                    let key = args
                        .get("key")
                        .and_then(Value::as_str)
                        .ok_or_else(|| "key is required".to_string())?;
                    let value = args.get("value").cloned().unwrap_or(Value::Null);
                    let expected_version = args.get("expected_version").and_then(Value::as_u64);
                    control.blackboard_set(key, value, expected_version)?
                }
                _ => control.blackboard_list()?,
            };
            return serde_json::to_vec(&value).map_err(|err| err.to_string());
        }
        if matches!(ctx.tool.as_str(), "spawn_agent" | "agent_result") {
            if !ctx.allows(Capability::AgentSpawn) {
                return Err("denied: agent.spawn not granted".to_string());
            }
            let control = self
                .control
                .as_ref()
                .ok_or_else(|| "control plane is not configured".to_string())?;
            let args: Value = serde_json::from_slice(input)
                .map_err(|err| format!("invalid agent input: {err}"))?;
            let value = if ctx.tool == "spawn_agent" {
                let goal = args
                    .get("goal")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "goal is required".to_string())?;
                let provider = args
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or(&self.default_provider);
                let max_steps = args
                    .get("max_steps")
                    .and_then(Value::as_u64)
                    .unwrap_or(8)
                    .clamp(1, 50) as usize;
                let max_context_tokens = args
                    .get("max_context_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(50_000)
                    .clamp(4_096, 1_000_000) as usize;
                control.spawn_agent(
                    goal,
                    provider,
                    max_steps,
                    max_context_tokens,
                    self.task_id.as_deref(),
                    self.agent_depth,
                )?
            } else {
                let task_id = args
                    .get("task_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "task_id is required".to_string())?;
                let wait_ms = args
                    .get("wait_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(10_000);
                control.agent_result(task_id, wait_ms)?
            };
            return serde_json::to_vec(&value).map_err(|err| err.to_string());
        }
        if ctx.tool == "http_request" {
            if !ctx.allows(Capability::NetEgress) {
                return Err("denied: net.egress not granted".to_string());
            }
            return http_request(
                input,
                self.secrets.as_ref(),
                ctx.allows(Capability::SecretsRead),
            );
        }
        if ctx.tool == "artifact_read" {
            if !ctx.allows(Capability::ArtifactRead) {
                return Err("denied: artifact.read not granted".to_string());
            }
            return self.read_artifact(input);
        }

        let (envd_tool, capability) = match ctx.tool.as_str() {
            "exec" | "session_create" | "session_exec" | "session_close" | "session_write"
            | "session_read" | "session_resize" => (ctx.tool.as_str(), Capability::ExecSession),
            "read_file" | "write_file" | "list_dir" | "stat_file" => {
                (ctx.tool.as_str(), Capability::FsWorkspace)
            }
            other => return Err(format!("denied: tool {other} is not allowed")),
        };
        if !ctx.allows(capability.clone()) {
            return Err(format!("denied: {} not granted", capability.as_str()));
        }
        let args: Value =
            serde_json::from_slice(input).map_err(|err| format!("invalid tool input: {err}"))?;
        let mut client = self
            .client
            .lock()
            .map_err(|_| "envd client lock poisoned".to_string())?;
        let result = client
            .call_tool(envd_tool, args)
            .map_err(|err| err.to_string())?;
        serde_json::to_vec(&result).map_err(|err| err.to_string())
    }
}

pub struct RegisteredTool {
    pub context: ToolContext,
    pub runtime: WasmtimeRuntime,
    pub manifest: ToolManifest,
    pub decision: PolicyDecision,
}

pub fn load_registry(
    tools_dir: &Path,
    fallback_wasm: &Path,
    host: Arc<dyn ToolHost>,
    names: &[&str],
    wasm_cache: Option<&Path>,
) -> Result<HashMap<String, RegisteredTool>> {
    load_registry_with_policy(
        tools_dir,
        fallback_wasm,
        host,
        names,
        &policy::default_policy(),
        wasm_cache,
    )
}

pub fn load_registry_with_policy(
    tools_dir: &Path,
    fallback_wasm: &Path,
    host: Arc<dyn ToolHost>,
    names: &[&str],
    policy: &PolicyTable,
    wasm_cache: Option<&Path>,
) -> Result<HashMap<String, RegisteredTool>> {
    let mut tools = HashMap::new();
    for name in names {
        let manifest_path = tools_dir.join(format!("{name}.json"));
        let manifest = policy::load_manifest(&manifest_path)
            .with_context(|| format!("load manifest {}", manifest_path.display()))?;
        let context = policy::context_with(policy, &manifest)
            .with_context(|| format!("authorize tool {name}"))?;
        let decision = policy.decision(&manifest.name);

        let candidate = tools_dir.join(format!("{name}.wat"));
        let wasm = if candidate.exists() {
            candidate
        } else {
            fallback_wasm.to_path_buf()
        };
        let runtime = WasmtimeRuntime::from_file_with_cache(host.clone(), &wasm, wasm_cache)
            .with_context(|| format!("load WASM {}", wasm.display()))?;
        tools.insert(
            (*name).to_string(),
            RegisteredTool {
                context,
                runtime,
                manifest,
                decision,
            },
        );
    }
    Ok(tools)
}

pub fn llm_request(
    context: &ContextManager,
    registry: &HashMap<String, RegisteredTool>,
) -> LlmRequest {
    let tools = registry
        .values()
        .map(|tool| ToolSchema {
            name: tool.manifest.name.clone(),
            description: tool.manifest.description.clone(),
            parameters: tool.manifest.input_schema.clone(),
        })
        .collect();
    LlmRequest {
        messages: context
            .messages()
            .iter()
            .map(|message| {
                if let Some(call_id) = &message.tool_call_id {
                    Message::tool_result(
                        call_id.clone(),
                        message.tool_name.clone().unwrap_or_default(),
                        message.content.clone(),
                    )
                } else if !message.tool_calls.is_empty() {
                    Message::assistant_tool_calls(
                        message.content.clone(),
                        message.tool_calls.clone(),
                    )
                } else {
                    Message::text(message.role.as_llm_role(), message.content.clone())
                }
            })
            .collect(),
        tools,
        max_tokens: Some(512),
    }
}

pub fn complete_or_stream(
    provider: &dyn LlmProvider,
    request: &LlmRequest,
    stream: bool,
    sink: &mut dyn FnMut(LlmDelta),
) -> Result<LlmResponse, LlmError> {
    if stream {
        provider.stream(request, sink)
    } else {
        provider.complete(request)
    }
}

pub fn build_observation(
    _tool: &str,
    data: &Value,
    limits: &ContextLimits,
    store: &dyn ObjectStore,
) -> Result<(String, Option<ObjectRef>)> {
    let text = serde_json::to_string(data)?;
    if text.len() <= limits.inline_tool_bytes {
        return Ok((text, None));
    }
    let object = store.put(text.as_bytes(), DataLabel::Derived)?;
    let summary = summarize_output(&text, limits.summary_head_chars, limits.summary_tail_chars);
    Ok((
        format!(
            "[artifact {} ({} bytes)] {}",
            object.id,
            text.len(),
            summary
        ),
        Some(object),
    ))
}

pub fn substitute(
    value: &mut Value,
    env_id: &str,
    session: Option<&str>,
    artifact: Option<&str>,
) -> Result<()> {
    match value {
        Value::String(text) => {
            if text == "$ENV" {
                *text = env_id.to_string();
            } else if text == "$SESSION" {
                let session = session
                    .ok_or_else(|| anyhow::anyhow!("$SESSION used before session_create"))?;
                *text = session.to_string();
            } else if text == "$ARTIFACT" {
                let artifact = artifact
                    .ok_or_else(|| anyhow::anyhow!("$ARTIFACT used before an artifact existed"))?;
                *text = artifact.to_string();
            }
        }
        Value::Array(items) => {
            for item in items {
                substitute(item, env_id, session, artifact)?;
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                substitute(item, env_id, session, artifact)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn sha256_hex(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn extract_data(value: &Value) -> Result<Value> {
    if value
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!("tool failed: {value}");
    }
    if let Some(structured) = value.get("structuredContent") {
        if structured.get("ok").and_then(Value::as_bool) != Some(true) {
            bail!("tool returned not ok: {structured}");
        }
        return Ok(structured.get("data").cloned().unwrap_or(Value::Null));
    }
    Ok(value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    #[test]
    fn http_request_returns_status_headers_and_body() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let body = "hello http";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let input = serde_json::to_vec(&json!({
            "url": format!("http://{address}/"),
            "headers": {"X-Test": "1"}
        }))
        .unwrap();
        let output = http_request(&input, &msk_secret::MemorySecretStore::new(), false).unwrap();
        let value: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(value["status"], 200);
        let body = STANDARD
            .decode(value["body_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(body, b"hello http");
        assert!(
            value["headers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|header| header[0] == "content-type")
        );
        server.join().unwrap();
    }

    #[test]
    fn http_request_resolves_secret_headers_without_leaking_them() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let read = stream.read(&mut request).unwrap();
            let request = String::from_utf8_lossy(&request[..read]).to_ascii_lowercase();
            assert!(request.contains("x-api-key: s3cret"), "{request}");
            let body = "ok";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream.write_all(response.as_bytes()).unwrap();
        });

        let secrets = msk_secret::MemorySecretStore::new();
        secrets.insert("api", "s3cret");
        let input = serde_json::to_vec(&json!({
            "url": format!("http://{address}/"),
            "headers": {"X-Api-Key": "$secret:api"}
        }))
        .unwrap();
        let output = http_request(&input, &secrets, true).unwrap();
        assert!(!String::from_utf8_lossy(&output).contains("s3cret"));
        server.join().unwrap();

        assert!(http_request(&input, &secrets, false).is_err());
        let missing = serde_json::to_vec(&json!({
            "url": format!("http://{address}/"),
            "headers": {"X-Api-Key": "$secret:missing"}
        }))
        .unwrap();
        assert!(http_request(&missing, &secrets, true).is_err());
    }

    #[test]
    fn http_request_rejects_non_http_urls() {
        let input = serde_json::to_vec(&json!({"url": "file:///etc/passwd"})).unwrap();
        assert!(http_request(&input, &msk_secret::MemorySecretStore::new(), false).is_err());
    }
}
