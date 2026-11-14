//! Kernel-side adapter for signed `llm.provider` components.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use msk_core::ToolCall;
use msk_llm::{LlmError, LlmProvider, LlmRequest, LlmResponse, Message, ToolSchema};
use msk_loader::{ComponentManifest, LlmCallError, LlmInstance, LoadedComponent, TrustStore};
use serde::{Deserialize, Serialize};

const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone)]
pub struct ComponentOptions {
    pub manifest: PathBuf,
    pub trust: PathBuf,
}

#[derive(Serialize)]
struct WireRequest<'a> {
    schema_version: u32,
    messages: &'a [Message],
    tools: &'a [ToolSchema],
    max_tokens: Option<u32>,
}

#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    text: String,
    #[serde(default)]
    tool_calls: Vec<ToolCall>,
    #[serde(default)]
    usage_tokens: u64,
}

pub struct ComponentLlmProvider {
    instance: Mutex<LlmInstance>,
    id: String,
    manifest_id: String,
    version: String,
}

impl ComponentLlmProvider {
    pub fn load(options: &ComponentOptions) -> Result<Self> {
        let manifest_bytes = std::fs::metadata(&options.manifest)
            .with_context(|| format!("stat {}", options.manifest.display()))?
            .len();
        if manifest_bytes > MAX_MANIFEST_BYTES {
            bail!("component manifest exceeds 64 KiB");
        }
        let manifest_text = std::fs::read_to_string(&options.manifest)
            .with_context(|| format!("read {}", options.manifest.display()))?;
        let manifest: ComponentManifest =
            serde_json::from_str(&manifest_text).context("parse component manifest")?;
        manifest.validate().context("validate component manifest")?;
        let trust_text = std::fs::read_to_string(&options.trust)
            .with_context(|| format!("read {}", options.trust.display()))?;
        let trust = TrustStore::from_json(&trust_text).context("parse component trust store")?;
        let base_dir = options
            .manifest
            .parent()
            .context("component manifest has no parent directory")?;
        let loaded =
            LoadedComponent::load(&manifest, &trust, base_dir).context("load signed component")?;
        let manifest_id = loaded.id().to_string();
        let version = loaded.version().to_string();
        let instance = loaded
            .into_llm_instance(std::collections::HashMap::new())
            .context("initialize component")?;
        Ok(Self {
            instance: Mutex::new(instance),
            id: format!("component:{manifest_id}"),
            manifest_id,
            version,
        })
    }

    pub fn manifest_id(&self) -> &str {
        &self.manifest_id
    }

    pub fn version(&self) -> &str {
        &self.version
    }
}

impl LlmProvider for ComponentLlmProvider {
    fn id(&self) -> &str {
        &self.id
    }

    fn complete(&self, request: &LlmRequest) -> std::result::Result<LlmResponse, LlmError> {
        let mut tools = request.tools.clone();
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        let wire = WireRequest {
            schema_version: 1,
            messages: &request.messages,
            tools: &tools,
            max_tokens: request.max_tokens,
        };
        let bytes = serde_json::to_vec(&wire)
            .map_err(|err| LlmError::Protocol(format!("component request: {err}")))?;
        let response_bytes = {
            let instance = self
                .instance
                .lock()
                .map_err(|_| LlmError::Provider("component call lock poisoned".into()))?;
            instance
                .complete(&bytes)
                .map_err(|err| map_component_error(err))?
        };
        let response: WireResponse = serde_json::from_slice(&response_bytes)
            .map_err(|err| LlmError::Protocol(format!("component response JSON: {err}")))?;
        if response.schema_version != 1 {
            return Err(LlmError::Protocol(format!(
                "component response schema_version {} (expected 1)",
                response.schema_version
            )));
        }
        if response.tool_calls.len() > 128 {
            return Err(LlmError::Protocol(
                "component returned more than 128 tool calls".into(),
            ));
        }
        let mut ids = HashSet::new();
        for call in &response.tool_calls {
            if call.id.is_empty() || call.tool.is_empty() || !call.args.is_object() {
                return Err(LlmError::Protocol(
                    "component tool call has empty id/name or non-object args".into(),
                ));
            }
            if !ids.insert(call.id.clone()) {
                return Err(LlmError::Protocol(format!(
                    "duplicate component tool call id: {}",
                    call.id
                )));
            }
        }
        Ok(LlmResponse {
            text: response.text,
            tool_calls: response.tool_calls,
            usage_tokens: response.usage_tokens,
        })
    }
}

fn map_component_error(error: LlmCallError) -> LlmError {
    match error {
        LlmCallError::Protocol(message) => LlmError::Protocol(message),
        LlmCallError::Status { code, message } => {
            let detail = if message.is_empty() {
                String::new()
            } else {
                format!(": {message}")
            };
            match code {
                -1 => LlmError::Protocol(format!("invalid argument{detail}")),
                -2 => LlmError::Auth(format!("permission denied{detail}")),
                -3 => LlmError::Config(format!("not found{detail}")),
                -4 => LlmError::Timeout(format!("component timed out{detail}")),
                -5 => LlmError::Provider(format!("cancelled{detail}")),
                -6 => LlmError::Config(format!("unsupported{detail}")),
                -7 => LlmError::Provider(format!("internal{detail}")),
                other => LlmError::Protocol(format!("unknown status {other}{detail}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64;
    use ed25519_dalek::{Signer, SigningKey};
    use msk_llm::Message;
    use sha2::{Digest, Sha256};
    use std::path::{Path, PathBuf};

    fn component_artifact() -> Option<PathBuf> {
        let target = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target")
            .canonicalize()
            .ok()?;
        for profile in ["debug", "release"] {
            for directory in [target.join(profile), target.join(profile).join("deps")] {
                if let Ok(entries) = std::fs::read_dir(directory) {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if name.contains("msk_test_component")
                            && (name.ends_with(".dll")
                                || name.ends_with(".so")
                                || name.ends_with(".dylib"))
                        {
                            return Some(entry.path());
                        }
                    }
                }
            }
            let build_dir = target.join(profile).join("build");
            if let Ok(packages) = std::fs::read_dir(build_dir) {
                for package in packages.flatten() {
                    let mut out_dirs = vec![package.path().join("out")];
                    if let Ok(children) = std::fs::read_dir(package.path()) {
                        for child in children.flatten() {
                            out_dirs.push(child.path().join("out"));
                        }
                    }
                    for out_dir in out_dirs {
                        if let Ok(files) = std::fs::read_dir(out_dir) {
                            for file in files.flatten() {
                                let name = file.file_name().to_string_lossy().into_owned();
                                if name.contains("msk_test_component")
                                    && (name.ends_with(".dll")
                                        || name.ends_with(".so")
                                        || name.ends_with(".dylib"))
                                {
                                    return Some(file.path());
                                }
                            }
                        }
                    }
                }
            }
        }
        None
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn loads_component_and_returns_a_response() {
        let Some(artifact) = component_artifact() else {
            if std::env::var("CI").is_ok() {
                panic!("msk-test-component artifact is missing in CI");
            }
            eprintln!("msk-test-component artifact not built; skipping provider test");
            return;
        };
        let dir = std::env::temp_dir().join(format!(
            "msk-component-provider-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file_name = artifact.file_name().unwrap().to_string_lossy().into_owned();
        let copied = dir.join(&file_name);
        std::fs::copy(&artifact, &copied).unwrap();
        let bytes = std::fs::read(&copied).unwrap();

        let signing = SigningKey::from_bytes(&[9u8; 32]);
        let mut manifest = ComponentManifest {
            component: msk_loader::ComponentInfo {
                id: "test.llm".into(),
                version: "1.0.0".into(),
                abi: msk_abi::MS_ABI_VERSION,
                port: "llm.provider".into(),
                publisher: "acme".into(),
                thread_safety: "serial".into(),
            },
            requires: None,
            artifact: Some(msk_loader::Artifact {
                file: file_name,
                sha256: hex(&Sha256::digest(&bytes)),
            }),
            signature: None,
        };
        let signature = signing.sign(&manifest.canonical_bytes().unwrap());
        manifest.signature = Some(BASE64.encode(signature.to_bytes()));
        let manifest_path = dir.join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        let trust_path = dir.join("trust.json");
        let trust = serde_json::json!({
            "publishers": {
                "acme": BASE64.encode(signing.verifying_key().to_bytes())
            }
        });
        std::fs::write(&trust_path, serde_json::to_vec_pretty(&trust).unwrap()).unwrap();

        let provider = ComponentLlmProvider::load(&ComponentOptions {
            manifest: manifest_path,
            trust: trust_path,
        })
        .unwrap();
        assert_eq!(provider.id(), "component:test.llm");
        let response = provider
            .complete(&LlmRequest {
                messages: vec![Message::text("user", "hello component")],
                tools: Vec::new(),
                max_tokens: Some(64),
            })
            .unwrap();
        assert!(response.text.contains("component hello"));
        assert!(response.tool_calls.is_empty());
        drop(provider);
        let _ = std::fs::remove_dir_all(dir);
    }
}
