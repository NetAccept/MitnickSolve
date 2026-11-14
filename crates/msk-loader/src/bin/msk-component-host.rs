//! Out-of-process host for signed `llm.provider` components. Speaks one JSON
//! object per line on stdin/stdout. The kernel spawns this process so a
//! component fault cannot corrupt the kernel address space.

use std::collections::HashMap;
use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use msk_loader::{ComponentManifest, LlmInstance, LoadedComponent, TrustStore, verify_component};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct HostRequest {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: u64,
    #[serde(default)]
    request_base64: Option<String>,
}

fn main() {
    let mut manifest = None;
    let mut trust = None;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--manifest" => manifest = arguments.next().map(PathBuf::from),
            "--trust" => trust = arguments.next().map(PathBuf::from),
            other => {
                eprintln!("component-host: unknown argument {other}");
                std::process::exit(2);
            }
        }
    }
    let Some(manifest_path) = manifest else {
        eprintln!("component-host: --manifest is required");
        std::process::exit(2);
    };
    let Some(trust_path) = trust else {
        eprintln!("component-host: --trust is required");
        std::process::exit(2);
    };

    let instance = match load(&manifest_path, &trust_path) {
        Ok(instance) => instance,
        Err(error) => {
            let _ = write_line(&json!({"type": "ready", "ok": false, "error": error}));
            std::process::exit(1);
        }
    };
    if write_line(&json!({
        "type": "ready",
        "ok": true,
        "id": instance.id(),
        "version": instance.version()
    }))
    .is_err()
    {
        return;
    }

    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let request: HostRequest = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                let _ = write_line(&json!({
                    "type": "error", "ok": false,
                    "error": format!("invalid request: {error}")
                }));
                continue;
            }
        };
        match request.kind.as_str() {
            "complete" => {
                let result = request
                    .request_base64
                    .as_deref()
                    .ok_or_else(|| "request_base64 is required".to_string())
                    .and_then(|encoded| {
                        BASE64
                            .decode(encoded)
                            .map_err(|error| format!("request base64: {error}"))
                    })
                    .and_then(|request| {
                        instance
                            .complete(&request)
                            .map_err(|error| error.to_string())
                    });
                match result {
                    Ok(response) => {
                        let _ = write_line(&json!({
                            "type": "response",
                            "id": request.id,
                            "ok": true,
                            "response_base64": BASE64.encode(response)
                        }));
                    }
                    Err(error) => {
                        let _ = write_line(&json!({
                            "type": "response",
                            "id": request.id,
                            "ok": false,
                            "error": error
                        }));
                    }
                }
            }
            "ping" => {
                let _ = write_line(&json!({"type": "pong"}));
            }
            "shutdown" => {
                let _ = write_line(&json!({"type": "shutdown", "ok": true}));
                break;
            }
            other => {
                let _ = write_line(&json!({
                    "type": "response",
                    "id": request.id,
                    "ok": false,
                    "error": format!("unknown request type: {other}")
                }));
            }
        }
    }
    drop(instance);
}

fn load(manifest_path: &PathBuf, trust_path: &PathBuf) -> Result<LlmInstance, String> {
    let manifest_text =
        std::fs::read_to_string(manifest_path).map_err(|error| format!("manifest: {error}"))?;
    let manifest: ComponentManifest =
        serde_json::from_str(&manifest_text).map_err(|error| format!("manifest JSON: {error}"))?;
    let trust_text =
        std::fs::read_to_string(trust_path).map_err(|error| format!("trust: {error}"))?;
    let trust = TrustStore::from_json(&trust_text).map_err(|error| error.to_string())?;
    let base_dir = manifest_path
        .parent()
        .ok_or_else(|| "manifest has no parent".to_string())?;
    verify_component(&manifest, &trust, base_dir).map_err(|error| error.to_string())?;
    let loaded =
        LoadedComponent::load(&manifest, &trust, base_dir).map_err(|error| error.to_string())?;
    loaded
        .into_llm_instance(HashMap::new())
        .map_err(|error| error.to_string())
}

fn write_line(value: &serde_json::Value) -> io::Result<()> {
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, value)?;
    stdout.write_all(b"\n")?;
    stdout.flush()
}
