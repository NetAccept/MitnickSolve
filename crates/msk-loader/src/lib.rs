//! Manifest and loader boundary for signed, load-time core components.
//!
//! M0 validates manifests and the sealed/open port split. Signature
//! verification, conformance execution, and `libloading` follow in M2.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentManifest {
    pub component: ComponentInfo,
    #[serde(default)]
    pub requires: Option<Requires>,
    #[serde(default)]
    pub artifact: Option<Artifact>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentInfo {
    pub id: String,
    pub version: String,
    pub abi: u32,
    pub port: String,
    pub publisher: String,
    #[serde(default = "default_thread_safety")]
    pub thread_safety: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Requires {
    #[serde(default)]
    pub kernel: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Artifact {
    pub file: String,
    pub sha256: String,
}

fn default_thread_safety() -> String {
    "safe".into()
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("unsupported ABI: {0}")]
    Abi(u32),
    #[error("sealed port cannot be implemented by a component: {0}")]
    SealedPort(String),
    #[error("unknown port: {0}")]
    UnknownPort(String),
    #[error("manifest: {0}")]
    Manifest(String),
}

pub const SUPPORTED_ABI: u32 = 1;

pub fn validate_port(port: &str) -> Result<(), LoadError> {
    match port {
        "llm.provider" | "transport.http" | "store" | "event.sink" | "secret.store" => Ok(()),
        "policy" | "capability" | "scheduler" | "audit" | "wasm.runtime" => {
            Err(LoadError::SealedPort(port.into()))
        }
        other => Err(LoadError::UnknownPort(other.into())),
    }
}

impl ComponentManifest {
    pub fn validate(&self) -> Result<(), LoadError> {
        if self.component.abi != SUPPORTED_ABI {
            return Err(LoadError::Abi(self.component.abi));
        }
        if self.component.id.is_empty() || self.component.version.is_empty() {
            return Err(LoadError::Manifest("id and version are required".into()));
        }
        validate_port(&self.component.port)?;
        if let Some(artifact) = &self.artifact {
            if artifact.sha256.len() != 64
                || !artifact.sha256.chars().all(|c| c.is_ascii_hexdigit())
            {
                return Err(LoadError::Manifest(
                    "artifact sha256 must be 64 hex chars".into(),
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(port: &str) -> ComponentManifest {
        ComponentManifest {
            component: ComponentInfo {
                id: "acme.llm".into(),
                version: "1.0.0".into(),
                abi: SUPPORTED_ABI,
                port: port.into(),
                publisher: "acme".into(),
                thread_safety: "safe".into(),
            },
            requires: None,
            artifact: None,
        }
    }

    #[test]
    fn open_port_is_allowed() {
        assert!(manifest("llm.provider").validate().is_ok());
    }

    #[test]
    fn sealed_port_is_rejected() {
        assert!(matches!(
            manifest("policy").validate(),
            Err(LoadError::SealedPort(_))
        ));
    }

    #[test]
    fn unknown_port_is_rejected() {
        assert!(matches!(
            manifest("made.up").validate(),
            Err(LoadError::UnknownPort(_))
        ));
    }
}
