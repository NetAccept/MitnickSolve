use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use msk_core::Capability;
use msk_kernel::policy;
use msk_mcp::EnvClient;
use msk_wasm::{ToolContext, ToolHost, WasmtimeRuntime};
use serde_json::{Value, json};

#[derive(Debug, Parser)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Store + instantiate + call cost for a module with no host imports.
    WasmEcho {
        #[arg(long, default_value = "tools/wasm/echo.wat")]
        wasm: PathBuf,
        #[arg(long, default_value_t = 2000)]
        iterations: usize,
    },
    /// Store + instantiate + call cost with the host bridge (no envd).
    WasmHost {
        #[arg(long, default_value = "tools/wasm/exec.wat")]
        wasm: PathBuf,
        #[arg(long, default_value_t = 2000)]
        iterations: usize,
    },
    /// CPU-bound loop compiled by wasmtime.
    WasmCpu {
        #[arg(long, default_value = "tools/wasm/cpu.wat")]
        wasm: PathBuf,
        #[arg(long, default_value_t = 10_000_000)]
        n: u32,
        #[arg(long, default_value_t = 5)]
        iterations: usize,
    },
    /// Full WASM -> host bridge -> envd exec path.
    EnvdExec {
        #[arg(long)]
        envd: PathBuf,
        #[arg(long, default_value_t = 10)]
        iterations: usize,
        #[arg(long, default_value = "tools/wasm/exec.wat")]
        wasm: PathBuf,
        #[arg(long, default_value = "tools/wasm/exec.json")]
        manifest: PathBuf,
    },
    /// Full WASM -> host bridge -> persistent envd shell path.
    SessionExec {
        #[arg(long)]
        envd: PathBuf,
        #[arg(long, default_value_t = 20)]
        iterations: usize,
        #[arg(long, default_value = "tools/wasm/exec.wat")]
        wasm: PathBuf,
        #[arg(long, default_value = "tools/wasm/session.json")]
        manifest: PathBuf,
    },
    /// OS process spawn baseline.
    DirectExec {
        #[arg(long, default_value_t = 10)]
        iterations: usize,
    },
    /// Module load and compile time.
    Compile {
        #[arg(long, default_value = "tools/wasm/exec.wat")]
        wasm: PathBuf,
    },
}

struct FixedHost;

impl ToolHost for FixedHost {
    fn call(&self, _ctx: &ToolContext, _input: &[u8]) -> Result<Vec<u8>, String> {
        Ok(vec![0u8; 16])
    }
}

struct EnvHost {
    client: Mutex<EnvClient>,
}

impl ToolHost for EnvHost {
    fn call(&self, ctx: &ToolContext, input: &[u8]) -> Result<Vec<u8>, String> {
        if !ctx.allows(Capability::ExecSession) {
            return Err("denied: exec.session not granted".to_string());
        }
        let args: Value =
            serde_json::from_slice(input).map_err(|err| format!("invalid input: {err}"))?;
        let envd_tool = match ctx.tool.as_str() {
            "exec" => "exec",
            "session_exec" => "session_exec",
            other => return Err(format!("denied: tool {other} is not allowed")),
        };
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
    let cli = Cli::parse();
    match cli.command {
        Command::WasmEcho { wasm, iterations } => {
            bench_wasm(&wasm, iterations, Arc::new(FixedHost), b"hello")
        }
        Command::WasmHost { wasm, iterations } => {
            bench_wasm(&wasm, iterations, Arc::new(FixedHost), b"{}")
        }
        Command::WasmCpu {
            wasm,
            n,
            iterations,
        } => bench_cpu(&wasm, n, iterations),
        Command::EnvdExec {
            envd,
            iterations,
            wasm,
            manifest,
        } => bench_envd(&envd, &wasm, &manifest, iterations),
        Command::SessionExec {
            envd,
            iterations,
            wasm,
            manifest,
        } => bench_session(&envd, &wasm, &manifest, iterations),
        Command::DirectExec { iterations } => bench_direct(iterations),
        Command::Compile { wasm } => bench_compile(&wasm),
    }
}

fn bench_wasm(path: &Path, iterations: usize, host: Arc<dyn ToolHost>, input: &[u8]) -> Result<()> {
    let load_started = Instant::now();
    let runtime = WasmtimeRuntime::from_file(host, path)
        .with_context(|| format!("load {}", path.display()))?;
    let load = load_started.elapsed();

    let _ = runtime.run(input)?;
    let started = Instant::now();
    for _ in 0..iterations {
        let _ = runtime.run(input)?;
    }
    let elapsed = started.elapsed();

    report("wasm call", iterations, elapsed);
    println!("module_load_ms={:.3}", load.as_secs_f64() * 1000.0);
    Ok(())
}

fn bench_cpu(path: &Path, n: u32, iterations: usize) -> Result<()> {
    let runtime = WasmtimeRuntime::from_file(Arc::new(FixedHost), path)
        .with_context(|| format!("load {}", path.display()))?
        .with_fuel(2_000_000_000);
    let input = n.to_le_bytes().to_vec();

    let out = runtime.run(&input)?;
    if out.len() < 8 {
        bail!("cpu tool returned {} bytes", out.len());
    }
    let sum = u64::from_le_bytes(out[..8].try_into().unwrap());
    let expected = u64::from(n) * u64::from(n.saturating_sub(1)) / 2;
    if sum != expected {
        bail!("sum mismatch: got {sum}, expected {expected}");
    }

    let started = Instant::now();
    for _ in 0..iterations {
        let _ = runtime.run(&input)?;
    }
    let elapsed = started.elapsed();

    report("wasm cpu", iterations, elapsed);
    println!("n={n} sum={sum}");
    Ok(())
}

fn bench_envd(envd: &Path, wasm: &Path, manifest_path: &Path, iterations: usize) -> Result<()> {
    let envd = envd.canonicalize().unwrap_or_else(|_| envd.to_path_buf());
    if !envd.exists() {
        bail!("envd binary not found: {}", envd.display());
    }
    let root = std::env::temp_dir().join(format!("msk-bench-{}", std::process::id()));
    std::fs::create_dir_all(&root)?;

    let mut client = EnvClient::spawn(&envd, &root).context("spawn envd")?;
    client.handshake("bench")?;
    client.initialize()?;
    let env = client.call_tool(
        "env_create",
        json!({"name": "bench", "kind": "managed", "profile": "dev"}),
    )?;
    let env_id = env
        .get("structuredContent")
        .and_then(|value| value.get("data"))
        .and_then(|value| value.get("env_id"))
        .and_then(Value::as_str)
        .context("env_create did not return env_id")?
        .to_string();

    let env_host = Arc::new(EnvHost {
        client: Mutex::new(client),
    });
    let host: Arc<dyn ToolHost> = env_host.clone();
    let runtime = WasmtimeRuntime::from_file(host, wasm)
        .with_context(|| format!("load {}", wasm.display()))?;
    let manifest = policy::load_manifest(manifest_path)?;
    let tool_ctx = policy::context_for(&manifest)?;

    let argv = if cfg!(windows) {
        json!(["cmd.exe", "/C", "echo bench"])
    } else {
        json!(["/bin/sh", "-c", "echo bench"])
    };
    let input = serde_json::to_vec(&json!({
        "env_id": env_id,
        "argv": argv,
        "timeout_ms": 10000
    }))?;

    let _ = runtime.run_with_context(&tool_ctx, &input)?;
    let started = Instant::now();
    for _ in 0..iterations {
        let out = runtime.run_with_context(&tool_ctx, &input)?;
        if out.is_empty() {
            bail!("empty tool output");
        }
    }
    let elapsed = started.elapsed();

    report("wasm -> envd exec", iterations, elapsed);
    if let Ok(mut client) = env_host.client.lock() {
        client.shutdown();
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

fn bench_session(envd: &Path, wasm: &Path, manifest_path: &Path, iterations: usize) -> Result<()> {
    let envd = envd.canonicalize().unwrap_or_else(|_| envd.to_path_buf());
    if !envd.exists() {
        bail!("envd binary not found: {}", envd.display());
    }
    let root = std::env::temp_dir().join(format!("msk-bench-session-{}", std::process::id()));
    std::fs::create_dir_all(&root)?;

    let mut client = EnvClient::spawn(&envd, &root).context("spawn envd")?;
    client.handshake("bench")?;
    client.initialize()?;
    let env = client.call_tool(
        "env_create",
        json!({"name": "bench", "kind": "managed", "profile": "dev"}),
    )?;
    let env_id = env
        .get("structuredContent")
        .and_then(|value| value.get("data"))
        .and_then(|value| value.get("env_id"))
        .and_then(Value::as_str)
        .context("env_create did not return env_id")?
        .to_string();

    let session = client.call_tool("session_create", json!({"env_id": env_id}))?;
    let session_id = session
        .get("structuredContent")
        .and_then(|value| value.get("data"))
        .and_then(|value| value.get("session_id"))
        .and_then(Value::as_str)
        .context("session_create did not return session_id")?
        .to_string();

    let env_host = Arc::new(EnvHost {
        client: Mutex::new(client),
    });
    let host: Arc<dyn ToolHost> = env_host.clone();
    let runtime = WasmtimeRuntime::from_file(host, wasm)
        .with_context(|| format!("load {}", wasm.display()))?;
    let manifest = policy::load_manifest(manifest_path)?;
    let tool_ctx = policy::context_for(&manifest)?;
    let input = serde_json::to_vec(&json!({
        "session_id": session_id.clone(),
        "command": "echo bench",
        "timeout_ms": 10000
    }))?;

    let _ = runtime.run_with_context(&tool_ctx, &input)?;
    let started = Instant::now();
    for _ in 0..iterations {
        let out = runtime.run_with_context(&tool_ctx, &input)?;
        if out.is_empty() {
            bail!("empty tool output");
        }
    }
    let elapsed = started.elapsed();

    report("wasm -> envd session_exec", iterations, elapsed);
    if let Ok(mut client) = env_host.client.lock() {
        let _ = client.call_tool("session_close", json!({"session_id": session_id}));
        client.shutdown();
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

fn bench_direct(iterations: usize) -> Result<()> {
    let _ = direct_once()?;
    let started = Instant::now();
    for _ in 0..iterations {
        let _ = direct_once()?;
    }
    report("direct process spawn", iterations, started.elapsed());
    Ok(())
}

fn direct_once() -> Result<()> {
    #[cfg(windows)]
    let output = std::process::Command::new("cmd.exe")
        .args(["/C", "echo bench"])
        .output()?;

    #[cfg(not(windows))]
    let output = std::process::Command::new("/bin/sh")
        .args(["-c", "echo bench"])
        .output()?;

    if !output.status.success() {
        bail!("direct exec failed: {:?}", output.status);
    }
    Ok(())
}

fn bench_compile(path: &Path) -> Result<()> {
    let started = Instant::now();
    let _runtime = WasmtimeRuntime::from_file(Arc::new(FixedHost), path)
        .with_context(|| format!("load {}", path.display()))?;
    println!("compile_ms={:.3}", started.elapsed().as_secs_f64() * 1000.0);
    Ok(())
}

fn report(label: &str, iterations: usize, elapsed: Duration) {
    let total_ms = elapsed.as_secs_f64() * 1000.0;
    let per_call_ms = total_ms / iterations as f64;
    println!(
        "{label}: iterations={iterations} total_ms={total_ms:.3} per_call_ms={per_call_ms:.4} per_call_us={:.1}",
        per_call_ms * 1000.0
    );
}
