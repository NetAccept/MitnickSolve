use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use clap::{Parser, ValueEnum};
use msk_core::context::{ContextLimits, ContextManager, Role, summarize_output};
use msk_core::{Capability, DataLabel, ToolCall};
use msk_data::{MemoryObjectStore, ObjectStore};
use msk_events::{Event, EventSink, JsonlSink};
use msk_kernel::policy;
use msk_llm::{LlmError, LlmProvider, LlmRequest, LlmResponse, Message, ScriptedProvider};
use msk_mcp::EnvClient;
use msk_wasm::{ToolContext, ToolHost, WasmtimeRuntime};
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Demo {
    Session,
    Echo,
}

#[derive(Debug, Parser)]
struct Cli {
    /// Path to the envd binary built from tools/envd.
    #[arg(long)]
    envd: PathBuf,
    /// Kernel state directory (events, envd data).
    #[arg(long, default_value = ".msk")]
    root: PathBuf,
    /// Directory containing tool manifests and WAT modules.
    #[arg(long, default_value = "tools/wasm")]
    tools_dir: PathBuf,
    /// Fallback module for tools without a matching <tool>.wat.
    #[arg(long, default_value = "tools/wasm/exec.wat")]
    wasm: PathBuf,
    /// Scripted agent demo to run.
    #[arg(long, value_enum, default_value_t = Demo::Session)]
    demo: Demo,
    /// Approximate token budget for the agent context.
    #[arg(long, default_value_t = 100_000)]
    max_context_tokens: usize,
    /// Minimum recent observations kept during compaction.
    #[arg(long, default_value_t = 4)]
    min_recent_observations: usize,
}

/// M0 host bridge: the only native capability exposed to WASM tools.
struct EnvHost {
    client: Mutex<EnvClient>,
}

impl ToolHost for EnvHost {
    fn call(&self, ctx: &ToolContext, input: &[u8]) -> Result<Vec<u8>, String> {
        let envd_tool = match ctx.tool.as_str() {
            "exec" | "session_create" | "session_exec" | "session_close" => ctx.tool.as_str(),
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

struct RegisteredTool {
    context: ToolContext,
    runtime: WasmtimeRuntime,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .init();

    let cli = Cli::parse();
    std::fs::create_dir_all(&cli.root)?;
    let events_path = cli.root.join("events.jsonl");
    let sink = JsonlSink::open(&events_path)?;
    emit(
        &sink,
        "kernel.start",
        json!({"root": cli.root.display().to_string(), "demo": format!("{:?}", cli.demo)}),
    )?;

    let envd_bin = cli.envd.canonicalize().unwrap_or_else(|_| cli.envd.clone());
    if !envd_bin.exists() {
        bail!(
            "envd binary not found: {} — build it with `cd tools/envd && go build -o ../../target/debug/envd ./cmd/envd`",
            envd_bin.display()
        );
    }
    if !cli.tools_dir.exists() {
        bail!("tool directory not found: {}", cli.tools_dir.display());
    }
    if !cli.wasm.exists() {
        bail!("fallback WASM module not found: {}", cli.wasm.display());
    }

    let envd_root = cli.root.join("envd");
    std::fs::create_dir_all(&envd_root)?;
    let mut client = EnvClient::spawn(&envd_bin, &envd_root).context("spawn embedded envd")?;
    let ack = client.handshake("agent-local").context("envd handshake")?;
    println!(
        "envd: {} (protocol {}, max_frame {})",
        ack.server, ack.protocol, ack.max_frame
    );
    emit(
        &sink,
        "envd.hello",
        json!({"server": ack.server, "protocol": ack.protocol, "max_frame": ack.max_frame}),
    )?;

    let init = client.initialize().context("MCP initialize")?;
    emit(&sink, "mcp.initialize", init)?;
    let tools = client.request("tools/list", json!({}))?;
    let tool_count = tools
        .get("tools")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    println!("envd tools: {tool_count}");
    emit(&sink, "mcp.tools", json!({"count": tool_count}))?;

    let env = tool_data(
        &mut client,
        "env_create",
        json!({"name": "demo", "kind": "managed", "profile": "dev"}),
        &sink,
    )?;
    let env_id = env
        .get("env_id")
        .and_then(Value::as_str)
        .context("env_create did not return env_id")?
        .to_string();
    println!(
        "env: {env_id} at {}",
        env.get("workspace").and_then(Value::as_str).unwrap_or("?")
    );
    emit(
        &sink,
        "env.create",
        json!({"env_id": env_id, "workspace": env.get("workspace")}),
    )?;

    let content = b"hello from msk\n";
    let write = tool_data(
        &mut client,
        "write_file",
        json!({
            "env_id": env_id.clone(),
            "path": "hello.txt",
            "data": STANDARD.encode(content),
            "atomic": true
        }),
        &sink,
    )?;
    emit(&sink, "file.write", write)?;

    let read = tool_data(
        &mut client,
        "read_file",
        json!({"env_id": env_id.clone(), "path": "hello.txt"}),
        &sink,
    )?;
    let data = read
        .get("data")
        .and_then(Value::as_str)
        .context("read_file did not return data")?;
    let decoded = STANDARD.decode(data)?;
    if decoded != content {
        bail!("read_file roundtrip mismatch");
    }
    println!("file roundtrip: ok ({} bytes)", decoded.len());

    let large_content: String = (0..500).map(|index| format!("line-{index}\n")).collect();
    let large_write = tool_data(
        &mut client,
        "write_file",
        json!({
            "env_id": env_id.clone(),
            "path": "large.txt",
            "data": STANDARD.encode(large_content.as_bytes()),
            "atomic": true
        }),
        &sink,
    )?;
    emit(&sink, "file.write_large", large_write)?;

    let env_host = Arc::new(EnvHost {
        client: Mutex::new(client),
    });
    let host: Arc<dyn ToolHost> = env_host.clone();

    let tool_names: &[&str] = match cli.demo {
        Demo::Session => &["session_create", "session_exec", "session_close"],
        Demo::Echo => &["echo"],
    };
    let registry = load_registry(&cli.tools_dir, &cli.wasm, host, tool_names)?;
    emit(
        &sink,
        "tool.registry",
        json!({"tools": tool_names, "dir": cli.tools_dir.display().to_string()}),
    )?;

    let context_limits = ContextLimits::default()
        .with_max_tokens(cli.max_context_tokens)
        .with_min_recent_observations(cli.min_recent_observations);
    let mut context = ContextManager::new(context_limits.clone());
    context.push_pinned(Role::User, "run the M0 agent demo");
    context.push_pinned(Role::System, format!("env_id={env_id}"));
    let object_store = MemoryObjectStore::new();

    let provider = ScriptedProvider::new("scripted", demo_responses(cli.demo));
    let mut response = provider
        .complete(&llm_request(&context))
        .context("scripted LLM")?;
    let mut last_session: Option<String> = None;

    loop {
        emit(
            &sink,
            "llm.response",
            json!({
                "text": response.text.clone(),
                "tool_calls": response.tool_calls.len(),
                "usage_tokens": response.usage_tokens
            }),
        )?;
        context.push(Role::Assistant, response.text.clone());
        if response.tool_calls.is_empty() {
            break;
        }

        for call in response.tool_calls {
            let mut args = call.args.clone();
            substitute(&mut args, &env_id, last_session.as_deref())?;

            let tool = registry
                .get(&call.tool)
                .with_context(|| format!("tool not registered: {}", call.tool))?;
            emit(&sink, "tool.call", json!({"tool": call.tool, "args": args}))?;
            emit(
                &sink,
                "policy.decision",
                json!({
                    "tool": tool.context.tool.clone(),
                    "granted": tool.context.capabilities.clone(),
                    "decision": "allow"
                }),
            )?;

            let input = serde_json::to_vec(&args)?;
            let started = std::time::Instant::now();
            let output = tool
                .runtime
                .run_with_context(&tool.context, &input)
                .with_context(|| format!("WASM tool {}", call.tool))?;
            let duration_ms = started.elapsed().as_millis();
            let value: Value =
                serde_json::from_slice(&output).context("WASM tool returned non-JSON")?;
            let data = extract_data(&value)?;

            let (observation, artifact) =
                build_observation(&call.tool, &data, &context_limits, &object_store)?;
            if let Some(object) = &artifact {
                emit(
                    &sink,
                    "data.put",
                    json!({
                        "id": object.id,
                        "bytes": object.size,
                        "label": object.label,
                        "hash": object.hash
                    }),
                )?;
            }
            emit(
                &sink,
                "wasm.result",
                json!({
                    "tool": call.tool,
                    "duration_ms": duration_ms,
                    "artifact": artifact.as_ref().map(|object| object.id.clone()),
                    "observation": observation.clone()
                }),
            )?;
            println!(
                "[{}] {}",
                call.tool,
                summarize_output(&observation, 200, 80)
            );
            context.push_observation(&call.tool, observation);

            if call.tool == "session_create" {
                last_session = data
                    .get("session_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if let Some(session) = &last_session {
                    context.push_pinned(Role::System, format!("session_id={session}"));
                } else {
                    bail!("session_create did not return session_id");
                }
            } else if call.tool == "session_close" {
                context.push_pinned(Role::System, "session_id=closed");
                last_session = None;
            }
        }

        let removed = context.compact();
        if removed > 0 {
            emit(
                &sink,
                "context.compact",
                json!({
                    "removed": removed,
                    "messages": context.messages().len(),
                    "approx_tokens": context.approx_tokens()
                }),
            )?;
        }
        emit(
            &sink,
            "context.state",
            json!({
                "messages": context.messages().len(),
                "approx_tokens": context.approx_tokens()
            }),
        )?;

        match provider.complete(&llm_request(&context)) {
            Ok(next) => response = next,
            Err(LlmError::Exhausted) => break,
            Err(err) => return Err(err.into()),
        }
    }

    if let Ok(mut client) = env_host.client.lock() {
        client.shutdown();
    }
    println!("events: {}", events_path.display());
    Ok(())
}

fn load_registry(
    tools_dir: &Path,
    fallback_wasm: &Path,
    host: Arc<dyn ToolHost>,
    names: &[&str],
) -> Result<HashMap<String, RegisteredTool>> {
    let mut tools = HashMap::new();
    for name in names {
        let manifest_path = tools_dir.join(format!("{name}.json"));
        let manifest = policy::load_manifest(&manifest_path)
            .with_context(|| format!("load manifest {}", manifest_path.display()))?;
        let context =
            policy::context_for(&manifest).with_context(|| format!("authorize tool {name}"))?;

        let candidate = tools_dir.join(format!("{name}.wat"));
        let wasm = if candidate.exists() {
            candidate
        } else {
            fallback_wasm.to_path_buf()
        };
        let runtime = WasmtimeRuntime::from_file(host.clone(), &wasm)
            .with_context(|| format!("load WASM {}", wasm.display()))?;
        tools.insert((*name).to_string(), RegisteredTool { context, runtime });
    }
    Ok(tools)
}

fn llm_request(context: &ContextManager) -> LlmRequest {
    LlmRequest {
        messages: context
            .messages()
            .iter()
            .map(|message| Message {
                role: message.role.as_llm_role().to_string(),
                content: message.content.clone(),
            })
            .collect(),
        tools: Vec::new(),
        max_tokens: Some(512),
    }
}

fn build_observation(
    _tool: &str,
    data: &Value,
    limits: &ContextLimits,
    store: &MemoryObjectStore,
) -> Result<(String, Option<msk_core::ObjectRef>)> {
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

fn demo_responses(demo: Demo) -> Vec<LlmResponse> {
    match demo {
        Demo::Echo => vec![
            LlmResponse {
                text: "echo the tool arguments".into(),
                tool_calls: vec![ToolCall {
                    id: "c1".into(),
                    tool: "echo".into(),
                    args: json!({"message": "hello from echo"}),
                }],
                usage_tokens: 5,
            },
            LlmResponse {
                text: "echo demo complete".into(),
                tool_calls: Vec::new(),
                usage_tokens: 5,
            },
        ],
        Demo::Session => {
            let state_command = if cfg!(windows) {
                "set MSK_DEMO=42 && echo !MSK_DEMO!"
            } else {
                "MSK_DEMO=42; echo $MSK_DEMO"
            };
            let large_command = if cfg!(windows) {
                "type large.txt"
            } else {
                "cat large.txt"
            };
            vec![
                LlmResponse {
                    text: "create a persistent shell".into(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        tool: "session_create".into(),
                        args: json!({"env_id": "$ENV"}),
                    }],
                    usage_tokens: 10,
                },
                LlmResponse {
                    text: "run a command".into(),
                    tool_calls: vec![ToolCall {
                        id: "c2".into(),
                        tool: "session_exec".into(),
                        args: json!({"session_id": "$SESSION", "command": "echo session-one"}),
                    }],
                    usage_tokens: 10,
                },
                LlmResponse {
                    text: "verify shell state persists".into(),
                    tool_calls: vec![ToolCall {
                        id: "c3".into(),
                        tool: "session_exec".into(),
                        args: json!({"session_id": "$SESSION", "command": state_command}),
                    }],
                    usage_tokens: 10,
                },
                LlmResponse {
                    text: "produce a large output".into(),
                    tool_calls: vec![ToolCall {
                        id: "c4".into(),
                        tool: "session_exec".into(),
                        args: json!({"session_id": "$SESSION", "command": large_command}),
                    }],
                    usage_tokens: 10,
                },
                LlmResponse {
                    text: "close the session".into(),
                    tool_calls: vec![ToolCall {
                        id: "c5".into(),
                        tool: "session_close".into(),
                        args: json!({"session_id": "$SESSION"}),
                    }],
                    usage_tokens: 10,
                },
                LlmResponse {
                    text: "session demo complete".into(),
                    tool_calls: Vec::new(),
                    usage_tokens: 10,
                },
            ]
        }
    }
}

fn substitute(value: &mut Value, env_id: &str, session: Option<&str>) -> Result<()> {
    match value {
        Value::String(text) => {
            if text == "$ENV" {
                *text = env_id.to_string();
            } else if text == "$SESSION" {
                let session = session
                    .ok_or_else(|| anyhow::anyhow!("$SESSION used before session_create"))?;
                *text = session.to_string();
            }
        }
        Value::Array(items) => {
            for item in items {
                substitute(item, env_id, session)?;
            }
        }
        Value::Object(map) => {
            for item in map.values_mut() {
                substitute(item, env_id, session)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn extract_data(value: &Value) -> Result<Value> {
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

fn tool_data(
    client: &mut EnvClient,
    name: &str,
    args: Value,
    sink: &dyn EventSink,
) -> Result<Value> {
    emit(sink, "tool.call", json!({"tool": name, "args": args}))?;
    let result = client.call_tool(name, args)?;
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let structured = result
        .get("structuredContent")
        .cloned()
        .unwrap_or(Value::Null);
    emit(
        sink,
        "tool.result",
        json!({"tool": name, "is_error": is_error, "structured": structured}),
    )?;
    if is_error {
        bail!("tool {name} failed: {structured}");
    }
    let ok = structured
        .get("ok")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !ok {
        bail!("tool {name} returned not ok: {structured}");
    }
    Ok(structured.get("data").cloned().unwrap_or(Value::Null))
}

fn emit(sink: &dyn EventSink, kind: &str, data: Value) -> Result<()> {
    sink.emit(&Event::new(kind, "kernel", data))?;
    Ok(())
}
