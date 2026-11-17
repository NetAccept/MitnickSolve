//! Install-time conformance check for a signed component. Verifies the
//! manifest/trust/artifact, loads and initializes the library, sends one
//! `llm.provider` request, validates the response envelope, and lets Drop run
//! exactly one shutdown. This executes component code; use it only on code you
//! are willing to run in-process.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use clap::Parser;
use msk_loader::{ComponentManifest, LoadedComponent, TrustStore, verify_component};
use serde_json::Value;

#[derive(Debug, Parser)]
#[command(name = "msk-component-check", version)]
struct Cli {
    /// Signed component manifest.
    #[arg(long)]
    manifest: PathBuf,
    /// Publisher trust store.
    #[arg(long)]
    trust: PathBuf,
    /// Optional request JSON file (defaults to a minimal conformance request).
    #[arg(long)]
    request: Option<PathBuf>,
    /// Only verify the manifest, trust, and artifact hash; do not load code.
    #[arg(long)]
    verify_only: bool,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("component check failed: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let manifest_text =
        std::fs::read_to_string(&cli.manifest).map_err(|error| format!("manifest: {error}"))?;
    let manifest: ComponentManifest =
        serde_json::from_str(&manifest_text).map_err(|error| format!("manifest JSON: {error}"))?;
    let trust_text =
        std::fs::read_to_string(&cli.trust).map_err(|error| format!("trust: {error}"))?;
    let trust = TrustStore::from_json(&trust_text).map_err(|error| error.to_string())?;
    let base_dir = cli
        .manifest
        .parent()
        .ok_or_else(|| "manifest has no parent directory".to_string())?;
    let artifact =
        verify_component(&manifest, &trust, base_dir).map_err(|error| error.to_string())?;
    if cli.verify_only {
        println!("component verify ok");
        println!("id: {}", manifest.component.id);
        println!("version: {}", manifest.component.version);
        println!("artifact: {}", artifact.display());
        return Ok(());
    }

    let loaded =
        LoadedComponent::load(&manifest, &trust, base_dir).map_err(|error| error.to_string())?;
    let id = loaded.id().to_string();
    let version = loaded.version().to_string();
    let port = loaded.port_kind();
    let vtable_abi = loaded.vtable_abi();
    let instance = loaded
        .into_llm_instance(HashMap::new())
        .map_err(|error| error.to_string())?;

    let request = match &cli.request {
        Some(path) => {
            let length = std::fs::metadata(path)
                .map_err(|error| format!("request: {error}"))?
                .len();
            if length > msk_abi::MS_LLM_MAX_REQUEST_BYTES as u64 {
                return Err("request exceeds 16 MiB".into());
            }
            std::fs::read(path).map_err(|error| format!("request: {error}"))?
        }
        None => br#"{"schema_version":1,"messages":[],"tools":[],"max_tokens":1}"#.to_vec(),
    };
    let response = instance
        .complete(&request)
        .map_err(|error| error.to_string())?;
    let value: Value =
        serde_json::from_slice(&response).map_err(|error| format!("response JSON: {error}"))?;
    validate_response(&value)?;
    let outstanding = instance.outstanding_bytes();
    if outstanding != 0 {
        return Err(format!(
            "component leaked {outstanding} bytes in the host allocation ledger"
        ));
    }
    drop(instance);

    println!("component check ok");
    println!("id: {id}");
    println!("version: {version}");
    println!("port: {port}");
    println!("vtable_abi: {vtable_abi}");
    println!("response_bytes: {}", response.len());
    println!("outstanding_bytes: {outstanding}");
    Ok(())
}

fn validate_response(value: &Value) -> Result<(), String> {
    if value.get("schema_version").and_then(Value::as_u64) != Some(1) {
        return Err("response schema_version must be 1".into());
    }
    if value.get("text").is_some_and(|text| !text.is_string()) {
        return Err("response text must be a string".into());
    }
    let calls = value
        .get("tool_calls")
        .and_then(Value::as_array)
        .ok_or_else(|| "response tool_calls must be an array".to_string())?;
    if calls.len() > 128 {
        return Err("response has more than 128 tool calls".into());
    }
    let mut ids = HashSet::new();
    for call in calls {
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .ok_or_else(|| "tool call id is missing".to_string())?;
        let tool = call
            .get("tool")
            .and_then(Value::as_str)
            .ok_or_else(|| "tool call tool name is missing".to_string())?;
        if id.is_empty() || tool.is_empty() {
            return Err("tool call id/name must not be empty".into());
        }
        if !ids.insert(id.to_string()) {
            return Err(format!("duplicate tool call id: {id}"));
        }
        if !call.get("args").is_some_and(Value::is_object) {
            return Err("tool call args must be an object".into());
        }
    }
    if value
        .get("usage_tokens")
        .is_some_and(|usage| usage.as_u64().is_none())
    {
        return Err("usage_tokens must be a non-negative integer".into());
    }
    Ok(())
}
