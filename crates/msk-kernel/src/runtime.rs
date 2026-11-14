//! Shared kernel runtime pieces used by the one-shot demo and the persistent
//! session engine. Keeping the host bridge and tool registry here means both
//! paths exercise the same WASM/policy boundary.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use msk_core::context::{ContextLimits, ContextManager, summarize_output};
use msk_core::{Capability, DataLabel, ObjectRef, PolicyDecision, PolicyTable};
use msk_data::ObjectStore;
use msk_kernel::policy;
use msk_llm::{LlmDelta, LlmError, LlmProvider, LlmRequest, LlmResponse, Message, ToolSchema};
use msk_mcp::EnvClient;
use msk_wasm::{ToolContext, ToolHost, ToolManifest, WasmtimeRuntime};
use serde_json::{Value, json};

/// M0 host bridge: the only native capability exposed to WASM tools.
pub struct EnvHost {
    pub client: Arc<Mutex<EnvClient>>,
    pub artifacts: Arc<dyn ObjectStore>,
}

#[derive(Debug, serde::Deserialize)]
struct ReadArtifactArgs {
    id: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
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
        if ctx.tool == "artifact_read" {
            if !ctx.allows(Capability::ArtifactRead) {
                return Err("denied: artifact.read not granted".to_string());
            }
            return self.read_artifact(input);
        }

        let envd_tool = match ctx.tool.as_str() {
            "exec" | "session_create" | "session_exec" | "session_close" | "session_write"
            | "session_read" | "session_resize" => ctx.tool.as_str(),
            other => return Err(format!("denied: tool {other} is not allowed")),
        };
        if !ctx.allows(Capability::ExecSession) {
            return Err("denied: exec.session not granted".to_string());
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
) -> Result<HashMap<String, RegisteredTool>> {
    load_registry_with_policy(
        tools_dir,
        fallback_wasm,
        host,
        names,
        &policy::default_policy(),
    )
}

pub fn load_registry_with_policy(
    tools_dir: &Path,
    fallback_wasm: &Path,
    host: Arc<dyn ToolHost>,
    names: &[&str],
    policy: &PolicyTable,
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
        let runtime = WasmtimeRuntime::from_file(host.clone(), &wasm)
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
