use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use clap::Parser;
use msk_core::{Capability, ToolCall};
use msk_events::{Event, EventSink, JsonlSink};
use msk_kernel::policy;
use msk_llm::{LlmProvider, LlmRequest, LlmResponse, Message, ScriptedProvider};
use msk_mcp::EnvClient;
use msk_wasm::{ToolContext, ToolHost, WasmtimeRuntime};
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
struct Cli {
    /// Path to the envd binary built from tools/envd.
    #[arg(long)]
    envd: PathBuf,
    /// Kernel state directory (events, envd data).
    #[arg(long, default_value = ".msk")]
    root: PathBuf,
    /// WAT/WasM tool to invoke in the M0 tool-call slice.
    #[arg(long, default_value = "tools/wasm/exec.wat")]
    wasm: PathBuf,
    /// Manifest declaring the tool name and requested capabilities.
    #[arg(long, default_value = "tools/wasm/exec.json")]
    manifest: PathBuf,
}

/// M0 host bridge: the only native capability exposed to WASM tools.
/// It currently forwards to envd `exec`; policy and data handles come later.
struct EnvHost {
    client: Mutex<EnvClient>,
}

impl ToolHost for EnvHost {
    fn call(&self, ctx: &ToolContext, input: &[u8]) -> Result<Vec<u8>, String> {
        let envd_tool = match ctx.tool.as_str() {
            "exec" => "exec",
            "session_exec" => "session_exec",
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
        json!({"root": cli.root.display().to_string()}),
    )?;

    if !cli.manifest.exists() {
        bail!("tool manifest not found: {}", cli.manifest.display());
    }
    let manifest = policy::load_manifest(&cli.manifest)?;
    emit(
        &sink,
        "tool.manifest",
        json!({
            "path": cli.manifest.display().to_string(),
            "name": manifest.name.clone(),
            "capabilities": manifest.capabilities.clone()
        }),
    )?;

    let tool_ctx = match policy::context_for(&manifest) {
        Ok(context) => context,
        Err(err) => {
            emit(
                &sink,
                "policy.decision",
                json!({
                    "tool": manifest.name.clone(),
                    "declared": manifest.capabilities.clone(),
                    "decision": "deny",
                    "reason": err.to_string()
                }),
            )?;
            return Err(err.into());
        }
    };
    emit(
        &sink,
        "policy.decision",
        json!({
            "tool": tool_ctx.tool.clone(),
            "declared": manifest.capabilities.clone(),
            "granted": tool_ctx.capabilities.clone(),
            "decision": "allow"
        }),
    )?;

    let envd_bin = cli.envd.canonicalize().unwrap_or_else(|_| cli.envd.clone());
    if !envd_bin.exists() {
        bail!(
            "envd binary not found: {} — build it with `cd tools/envd && go build -o ../../target/debug/envd ./cmd/envd`",
            envd_bin.display()
        );
    }
    if !cli.wasm.exists() {
        bail!(
            "WASM tool not found: {} — expected tools/wasm/exec.wat or tools/wasm/echo.wat",
            cli.wasm.display()
        );
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
    println!("tools: {tool_count}");
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

    let argv = if cfg!(windows) {
        json!(["cmd.exe", "/C", "echo exec-ok"])
    } else {
        json!(["/bin/sh", "-c", "echo exec-ok"])
    };
    let exec = tool_data(
        &mut client,
        "exec",
        json!({"env_id": env_id.clone(), "argv": argv.clone(), "timeout_ms": 10000}),
        &sink,
    )?;
    let exit_code = exec.get("exit_code").and_then(Value::as_i64).unwrap_or(-1);
    let stdout = exec.get("stdout").and_then(Value::as_str).unwrap_or("");
    println!("direct exec exit={exit_code} stdout={stdout:?}");

    // Scripted LLM -> tool call -> WASM -> host bridge -> envd.
    let wasm_argv = if cfg!(windows) {
        json!(["cmd.exe", "/C", "echo from-wasm"])
    } else {
        json!(["/bin/sh", "-c", "echo from-wasm"])
    };
    let provider = ScriptedProvider::new(
        "scripted",
        vec![LlmResponse {
            text: "run the demo command through the WASM tool".into(),
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                tool: "exec".into(),
                args: json!({
                    "env_id": env_id.clone(),
                    "argv": wasm_argv,
                    "timeout_ms": 10000
                }),
            }],
            usage_tokens: 42,
        }],
    );
    let request = LlmRequest {
        messages: vec![Message {
            role: "user".into(),
            content: "run the demo command".into(),
        }],
        tools: vec![],
        max_tokens: Some(256),
    };
    let response = provider.complete(&request).context("scripted LLM")?;
    emit(
        &sink,
        "llm.response",
        json!({
            "text": response.text.clone(),
            "tool_calls": response.tool_calls.len(),
            "usage_tokens": response.usage_tokens
        }),
    )?;

    let env_host = Arc::new(EnvHost {
        client: Mutex::new(client),
    });
    let host: Arc<dyn ToolHost> = env_host.clone();
    let runtime = WasmtimeRuntime::from_file(host, &cli.wasm)
        .with_context(|| format!("load WASM tool {}", cli.wasm.display()))?;
    emit(
        &sink,
        "wasm.load",
        json!({"path": cli.wasm.display().to_string()}),
    )?;

    for call in response.tool_calls {
        let input = serde_json::to_vec(&call.args)?;
        emit(
            &sink,
            "wasm.call",
            json!({"tool": call.tool, "input_bytes": input.len()}),
        )?;
        let started = std::time::Instant::now();
        let output = runtime
            .run_with_context(&tool_ctx, &input)
            .context("WASM tool run")?;
        let duration_ms = started.elapsed().as_millis();
        let value: Value =
            serde_json::from_slice(&output).context("WASM tool returned non-JSON")?;
        emit(
            &sink,
            "wasm.result",
            json!({"tool": call.tool, "duration_ms": duration_ms, "result": value}),
        )?;

        let is_error = value
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_error {
            bail!("WASM tool failed: {value}");
        }
        let structured = value
            .get("structuredContent")
            .cloned()
            .unwrap_or_else(|| value.clone());
        let data = structured
            .get("data")
            .cloned()
            .unwrap_or_else(|| structured.clone());
        println!("wasm tool result: {}", serde_json::to_string(&data)?);
    }

    if let Ok(mut client) = env_host.client.lock() {
        client.shutdown();
    }
    println!("events: {}", events_path.display());
    Ok(())
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
