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
use msk_kernel::mode::ModeRegistry;
use msk_kernel::policy::{self, PermissionMode};
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
    pub mode: String,
    pub modes: Arc<ModeRegistry>,
    pub permission: PermissionMode,
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
        mode: &str,
        permission: PermissionMode,
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
                    "permission": permission.as_str(),
                    "agent_depth": depth + 1,
                    "parent_task": parent_task,
                    "max_steps": max_steps,
                    "max_context_tokens": max_context_tokens,
                    "mode": mode
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
        if matches!(
            ctx.tool.as_str(),
            "finding_add" | "finding_list" | "report_generate"
        ) {
            if !ctx.allows(Capability::SharedState) {
                return Err("denied: state.shared not granted".to_string());
            }
            let control = self
                .control
                .as_ref()
                .ok_or_else(|| "control plane is not configured".to_string())?;
            let args: Value = serde_json::from_slice(input)
                .map_err(|err| format!("invalid finding input: {err}"))?;
            let value = match ctx.tool.as_str() {
                "finding_add" => add_finding(control, &args)?,
                "finding_list" => list_findings(control, &args)?,
                _ => generate_report(control, &args)?,
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
                let child_mode = args
                    .get("mode")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|mode| !mode.is_empty())
                    .unwrap_or(&self.mode)
                    .to_string();
                if !self.modes.can_spawn(&self.mode, &child_mode) {
                    return Err(format!(
                        "denied: {} mode cannot spawn a {} subagent",
                        self.mode, child_mode
                    ));
                }
                control.spawn_agent(
                    goal,
                    provider,
                    max_steps,
                    max_context_tokens,
                    self.task_id.as_deref(),
                    self.agent_depth,
                    &child_mode,
                    self.permission,
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
        if ctx.tool == "wasm_eval" {
            let args: Value = serde_json::from_slice(input)
                .map_err(|err| format!("invalid wasm_eval input: {err}"))?;
            let wat = args
                .get("wat")
                .and_then(Value::as_str)
                .ok_or_else(|| "wat is required".to_string())?;
            if wat.is_empty() || wat.len() > 64 * 1024 {
                return Err("wat must be 1..64 KiB".into());
            }
            let encoded = args
                .get("input_base64")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let data = STANDARD
                .decode(encoded)
                .map_err(|err| format!("input_base64: {err}"))?;
            if data.len() > 64 * 1024 {
                return Err("input_base64 exceeds 64 KiB".into());
            }
            let output =
                msk_wasm::run_pure_wat(wat, &data).map_err(|err| format!("wasm_eval: {err}"))?;
            if output.len() > 256 * 1024 {
                return Err("wasm_eval output exceeds 256 KiB".into());
            }
            let value = json!({
                "output_base64": STANDARD.encode(&output),
                "bytes": output.len()
            });
            return serde_json::to_vec(&value).map_err(|err| err.to_string());
        }

        let (envd_tool, capability) = match ctx.tool.as_str() {
            "exec" | "session_create" | "session_exec" | "session_close" | "session_write"
            | "session_read" | "session_resize" => {
                (ctx.tool.as_str(), Some(Capability::ExecSession))
            }
            "read_file" | "write_file" | "list_dir" | "stat_file" | "file_hash" | "file_type"
            | "file_strings" | "file_hexdump" | "file_entropy" | "file_search" | "archive_list"
            | "archive_extract" | "binary_info" | "secret_scan" => {
                (ctx.tool.as_str(), Some(Capability::FsWorkspace))
            }
            "net_dns" | "net_dial" | "net_tls" | "net_scan" | "net_info" | "web_request" => {
                (ctx.tool.as_str(), Some(Capability::NetEgress))
            }
            "transform" => (ctx.tool.as_str(), None),
            other => return Err(format!("denied: tool {other} is not allowed")),
        };
        if let Some(capability) = capability {
            if !ctx.allows(capability.clone()) {
                return Err(format!("denied: {} not granted", capability.as_str()));
            }
        }
        if ctx.tool == "web_request" && !self.modes.write_capable(&self.mode) {
            let args: Value = serde_json::from_slice(input)
                .map_err(|err| format!("invalid web request input: {err}"))?;
            let method = args
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("GET")
                .to_ascii_uppercase();
            let has_body = args
                .get("body_base64")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
                || args.get("json").is_some_and(|value| !value.is_null())
                || args
                    .get("form")
                    .and_then(Value::as_object)
                    .is_some_and(|form| !form.is_empty());
            if !matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS") || has_body {
                return Err(format!(
                    "denied: {} mode allows only GET/HEAD/OPTIONS web requests without a body",
                    self.mode
                ));
            }
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

        let wat = tools_dir.join(format!("{name}.wat"));
        let compiled = tools_dir.join(format!("{name}.wasm"));
        let wasm = if wat.exists() {
            wat
        } else if compiled.exists() {
            compiled
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
                } else if !message.tool_calls.is_empty() || !message.provider_items.is_empty() {
                    Message::assistant_tool_calls(
                        message.content.clone(),
                        message.tool_calls.clone(),
                    )
                    .with_provider_items(message.provider_items.clone())
                } else {
                    Message::text(message.role.as_llm_role(), message.content.clone())
                }
            })
            .collect(),
        tools,
        // Let the provider choose an output budget. A small fixed cap (512)
        // truncates reasoning models and produces invalid tool-call JSON.
        max_tokens: None,
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

fn severity_rank(value: &str) -> u8 {
    match value {
        "critical" => 5,
        "high" => 4,
        "medium" => 3,
        "low" => 2,
        "info" => 1,
        _ => 0,
    }
}

fn clean_field(value: &str, max: usize) -> String {
    let text = value.replace(['\r', '\n', '\t'], " ");
    let text = text.trim();
    if text.chars().count() <= max {
        text.to_string()
    } else {
        text.chars().take(max).collect()
    }
}

fn optional_field(args: &Value, name: &str, max: usize) -> Option<String> {
    args.get(name)
        .and_then(Value::as_str)
        .map(|value| clean_field(value, max))
        .filter(|value| !value.is_empty())
}

fn add_finding(control: &ControlPlane, args: &Value) -> Result<Value, String> {
    let title = args
        .get("title")
        .and_then(Value::as_str)
        .map(|value| clean_field(value, 200))
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "title is required (1..200 characters)".to_string())?;
    let severity = args
        .get("severity")
        .and_then(Value::as_str)
        .unwrap_or("info")
        .to_ascii_lowercase();
    if severity_rank(&severity) == 0 {
        return Err("severity must be info, low, medium, high, or critical".into());
    }
    let status = args
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("open")
        .to_ascii_lowercase();
    if !matches!(
        status.as_str(),
        "open" | "confirmed" | "fixed" | "false_positive"
    ) {
        return Err("status must be open, confirmed, fixed, or false_positive".into());
    }
    let asset = optional_field(args, "asset", 512).unwrap_or_default();
    let description = optional_field(args, "description", 8192).unwrap_or_default();
    let evidence = optional_field(args, "evidence", 32768).unwrap_or_default();
    let source = optional_field(args, "source", 200).unwrap_or_default();
    let tags: Vec<String> = args
        .get("tags")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|tag| clean_field(tag, 64))
                .filter(|tag| !tag.is_empty())
                .take(16)
                .collect()
        })
        .unwrap_or_default();
    let key = format!("finding:{}", sha256_hex(&format!("{title}\0{asset}")));
    let existing = control.blackboard_get(&key)?;
    let existing_value = existing.get("value").cloned().unwrap_or(Value::Null);
    let added = existing_value.is_null();
    let recorded_ms = existing_value
        .get("recorded_ms")
        .and_then(Value::as_u64)
        .unwrap_or_else(now_unix_ms);
    let finding = json!({
        "id": key.trim_start_matches("finding:"),
        "title": title,
        "severity": severity,
        "asset": asset,
        "description": description,
        "evidence": evidence,
        "status": status,
        "tags": tags,
        "source": source,
        "recorded_ms": recorded_ms,
        "updated_ms": now_unix_ms()
    });
    control.blackboard_set(&key, finding.clone(), None)?;
    Ok(json!({"added": added, "key": key, "finding": finding}))
}

fn collect_findings(control: &ControlPlane, args: Option<&Value>) -> Result<Vec<Value>, String> {
    let listed = control.blackboard_list()?;
    let mut findings = Vec::new();
    if let Some(entries) = listed.get("entries").and_then(Value::as_array) {
        for entry in entries {
            let key = entry.get("key").and_then(Value::as_str).unwrap_or_default();
            if !key.starts_with("finding:") {
                continue;
            }
            let value = entry.get("value").cloned().unwrap_or(Value::Null);
            if !value.is_object() {
                continue;
            }
            if let Some(args) = args {
                let matches = |field: &str| {
                    args.get(field)
                        .and_then(Value::as_str)
                        .filter(|wanted| !wanted.is_empty())
                        .is_none_or(|wanted| {
                            value.get(field).and_then(Value::as_str) == Some(wanted)
                        })
                };
                if !matches("severity") || !matches("status") || !matches("asset") {
                    continue;
                }
                if let Some(tag) = args
                    .get("tag")
                    .and_then(Value::as_str)
                    .filter(|tag| !tag.is_empty())
                {
                    let has_tag = value
                        .get("tags")
                        .and_then(Value::as_array)
                        .is_some_and(|tags| tags.iter().any(|item| item.as_str() == Some(tag)));
                    if !has_tag {
                        continue;
                    }
                }
            }
            findings.push(value);
        }
    }
    findings.sort_by(|left, right| {
        let left_key = (
            severity_rank(left.get("severity").and_then(Value::as_str).unwrap_or("")),
            left.get("recorded_ms").and_then(Value::as_u64).unwrap_or(0),
        );
        let right_key = (
            severity_rank(right.get("severity").and_then(Value::as_str).unwrap_or("")),
            right
                .get("recorded_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        );
        right_key.cmp(&left_key)
    });
    Ok(findings)
}

fn list_findings(control: &ControlPlane, args: &Value) -> Result<Value, String> {
    let mut findings = collect_findings(control, Some(args))?;
    let limit = args
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;
    findings.truncate(limit);
    let count = findings.len();
    Ok(json!({"findings": findings, "count": count}))
}

fn collect_flags(control: &ControlPlane) -> Result<Vec<Value>, String> {
    let listed = control.blackboard_list()?;
    let mut flags = Vec::new();
    if let Some(entries) = listed.get("entries").and_then(Value::as_array) {
        for entry in entries {
            let key = entry.get("key").and_then(Value::as_str).unwrap_or_default();
            if !key.starts_with("flag:") {
                continue;
            }
            let value = entry.get("value").cloned().unwrap_or(Value::Null);
            if value.is_object() {
                flags.push(value);
            }
        }
    }
    Ok(flags)
}

fn render_report_markdown(
    title: &str,
    findings: &[Value],
    flags: &[Value],
    generated_ms: u64,
) -> String {
    let mut severity_counts = [0usize; 5];
    for finding in findings {
        let rank = severity_rank(
            finding
                .get("severity")
                .and_then(Value::as_str)
                .unwrap_or("info"),
        ) as usize;
        if rank > 0 {
            severity_counts[rank - 1] += 1;
        }
    }
    let mut out = String::new();
    out.push_str(&format!("# {title}\n\n"));
    out.push_str(&format!("Generated: {generated_ms} (unix ms)\n\n"));
    out.push_str("## Summary\n\n");
    out.push_str(&format!("- Findings: {}\n", findings.len()));
    for (rank, label) in ["info", "low", "medium", "high", "critical"]
        .iter()
        .enumerate()
    {
        out.push_str(&format!("  - {label}: {}\n", severity_counts[rank]));
    }
    out.push_str(&format!("- Flags: {}\n\n", flags.len()));

    out.push_str("## Findings\n\n");
    if findings.is_empty() {
        out.push_str("No findings recorded.\n\n");
    } else {
        for finding in findings {
            let finding_title = clean_field(
                finding
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("untitled"),
                200,
            );
            let severity = finding
                .get("severity")
                .and_then(Value::as_str)
                .unwrap_or("info")
                .to_ascii_uppercase();
            out.push_str(&format!("### [{severity}] {finding_title}\n\n"));
            for (label, field) in [("Asset", "asset"), ("Status", "status")] {
                if let Some(value) = finding.get(field).and_then(Value::as_str)
                    && !value.is_empty()
                {
                    out.push_str(&format!("- **{label}:** {}\n", clean_field(value, 512)));
                }
            }
            if let Some(tags) = finding.get("tags").and_then(Value::as_array)
                && !tags.is_empty()
            {
                let tags = tags
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|tag| clean_field(tag, 64))
                    .collect::<Vec<_>>()
                    .join(", ");
                out.push_str(&format!("- **Tags:** {tags}\n"));
            }
            out.push('\n');
            if let Some(description) = finding.get("description").and_then(Value::as_str)
                && !description.is_empty()
            {
                out.push_str(&clean_field(description, 8192));
                out.push_str("\n\n");
            }
            if let Some(evidence) = finding.get("evidence").and_then(Value::as_str)
                && !evidence.is_empty()
            {
                out.push_str("Evidence:\n\n");
                for line in evidence.lines().take(200) {
                    out.push_str(&format!("    {line}\n"));
                }
                out.push('\n');
            }
        }
    }

    out.push_str("## Flags\n\n");
    if flags.is_empty() {
        out.push_str("No flags recorded.\n");
    } else {
        for flag in flags {
            let value = flag
                .get("flag")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .replace(['\r', '\n'], " ");
            let source = flag
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if source.is_empty() {
                out.push_str(&format!("    {value}\n"));
            } else {
                out.push_str(&format!("    {value}  ({})\n", clean_field(source, 200)));
            }
        }
    }
    out
}

fn generate_report(control: &ControlPlane, args: &Value) -> Result<Value, String> {
    let title = clean_field(
        args.get("title")
            .and_then(Value::as_str)
            .unwrap_or("Security assessment"),
        200,
    );
    let findings = collect_findings(control, None)?;
    let flags = collect_flags(control)?;
    let generated_ms = now_unix_ms();
    let markdown = render_report_markdown(&title, &findings, &flags, generated_ms);
    let mut counts = serde_json::Map::new();
    for (rank, label) in ["info", "low", "medium", "high", "critical"]
        .iter()
        .enumerate()
    {
        let count = findings
            .iter()
            .filter(|finding| {
                severity_rank(
                    finding
                        .get("severity")
                        .and_then(Value::as_str)
                        .unwrap_or("info"),
                ) as usize
                    == rank + 1
            })
            .count();
        counts.insert((*label).to_string(), json!(count));
    }
    Ok(json!({
        "title": title,
        "markdown": markdown,
        "findings": findings,
        "flags": flags,
        "counts": counts,
        "generated_ms": generated_ms
    }))
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

    struct RecordingTransport {
        calls: Arc<Mutex<Vec<String>>>,
    }

    impl EnvTransport for RecordingTransport {
        fn call_tool(&mut self, name: &str, _arguments: Value) -> std::io::Result<Value> {
            self.calls.lock().unwrap().push(name.to_string());
            Ok(json!({
                "structuredContent": {
                    "ok": true,
                    "data": {"routed": name}
                }
            }))
        }

        fn request(&mut self, _method: &str, _params: Value) -> std::io::Result<Value> {
            Ok(Value::Null)
        }

        fn shutdown(&mut self) {}
    }

    fn recording_host() -> (EnvHost, Arc<Mutex<Vec<String>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let transport = RecordingTransport {
            calls: calls.clone(),
        };
        (
            EnvHost {
                client: Arc::new(Mutex::new(Box::new(transport))),
                artifacts: Arc::new(msk_data::MemoryObjectStore::default()),
                control: None,
                secrets: Arc::new(msk_secret::MemorySecretStore::new()),
                agent_depth: 0,
                task_id: None,
                default_provider: "scripted".into(),
                mode: "investigate".into(),
                modes: Arc::new(ModeRegistry::builtin().unwrap()),
                permission: PermissionMode::Bypass,
            },
            calls,
        )
    }

    #[test]
    fn security_tools_are_forwarded_to_envd_with_capabilities() {
        let (host, calls) = recording_host();
        let net = ToolContext::new("net_scan", vec![Capability::NetEgress]);
        let result = host
            .call(
                &net,
                br#"{"env_id":"env_1","targets":["127.0.0.1"],"ports":[80]}"#,
            )
            .unwrap();
        let value: Value = serde_json::from_slice(&result).unwrap();
        assert_eq!(value["structuredContent"]["data"]["routed"], "net_scan");

        let files = ToolContext::new("binary_info", vec![Capability::FsWorkspace]);
        host.call(&files, br#"{"env_id":"env_1","path":"challenge"}"#)
            .unwrap();

        let transform = ToolContext::new("transform", vec![]);
        host.call(&transform, br#"{"op":"hex_encode","data":"hi"}"#)
            .unwrap();

        assert_eq!(
            calls.lock().unwrap().as_slice(),
            ["net_scan", "binary_info", "transform"]
        );
    }

    #[test]
    fn security_tools_deny_missing_capabilities() {
        let (host, calls) = recording_host();
        let scan = ToolContext::new("net_scan", vec![Capability::FsWorkspace]);
        let error = host
            .call(
                &scan,
                br#"{"env_id":"env_1","targets":["127.0.0.1"],"ports":[80]}"#,
            )
            .unwrap_err();
        assert!(error.contains("net.egress"), "{error}");
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn report_markdown_groups_findings_and_flags() {
        let findings = vec![
            json!({
                "title": "Exposed admin panel\nsecond line",
                "severity": "high",
                "asset": "https://example.test/admin",
                "status": "confirmed",
                "tags": ["web", "auth"],
                "description": "Admin panel is reachable without authentication.",
                "evidence": "GET /admin -> 200\nSet-Cookie: session=..."
            }),
            json!({
                "title": "TLS certificate expires soon",
                "severity": "low",
                "asset": "example.test:443",
                "status": "open"
            }),
        ];
        let flags = vec![json!({"flag": "flag{report}", "source": "ctf"})];
        let markdown = render_report_markdown("Assessment", &findings, &flags, 42);
        assert!(markdown.contains("# Assessment"));
        assert!(markdown.contains("- Findings: 2"));
        assert!(markdown.contains("- high: 1"));
        assert!(markdown.contains("- low: 1"));
        assert!(markdown.contains("### [HIGH] Exposed admin panel second line"));
        assert!(markdown.contains("- **Asset:** https://example.test/admin"));
        assert!(markdown.contains("Evidence:"));
        assert!(markdown.contains("    flag{report}  (ctf)"));
    }

    #[test]
    fn report_fields_are_sanitized() {
        assert_eq!(clean_field("  a\nb\tc  ", 200), "a b c");
        assert_eq!(severity_rank("critical"), 5);
        assert_eq!(severity_rank("info"), 1);
        assert_eq!(severity_rank("nope"), 0);
    }

    #[test]
    fn investigate_mode_restricts_web_methods() {
        let (host, calls) = recording_host();
        let ctx = ToolContext::new("web_request", vec![Capability::NetEgress]);
        let error = host
            .call(
                &ctx,
                br#"{"env_id":"env_1","url":"http://127.0.0.1/","method":"POST","json":{"a":1}}"#,
            )
            .unwrap_err();
        assert!(error.contains("GET/HEAD"), "{error}");
        assert!(calls.lock().unwrap().is_empty());

        host.call(&ctx, br#"{"env_id":"env_1","url":"http://127.0.0.1/"}"#)
            .unwrap();
        assert_eq!(calls.lock().unwrap().as_slice(), ["web_request"]);
    }
}
