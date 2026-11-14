//! Manifest and loader boundary for signed, load-time core components.
//!
//! This crate validates manifests, the sealed/open port split, per-publisher
//! Ed25519 signatures, and artifact hashes. Dynamic loading and conformance
//! execution follow in the rest of M2.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComponentManifest {
    pub component: ComponentInfo,
    #[serde(default)]
    pub requires: Option<Requires>,
    #[serde(default)]
    pub artifact: Option<Artifact>,
    /// Base64 Ed25519 signature over `canonical_bytes()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
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

/// Publisher id -> base64 Ed25519 public key (32 bytes). The trust store is
/// explicit: a manifest from an unknown publisher is never accepted.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TrustStore {
    #[serde(default)]
    pub publishers: HashMap<String, String>,
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
    #[error("manifest signature is missing")]
    SignatureMissing,
    #[error("manifest signature is invalid")]
    SignatureInvalid,
    #[error("publisher is not trusted: {0}")]
    UnknownPublisher(String),
    #[error("publisher key: {0}")]
    Key(String),
    #[error("artifact hash mismatch: expected {expected}, got {actual}")]
    ArtifactHash { expected: String, actual: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
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
        if self.component.publisher.is_empty() {
            return Err(LoadError::Manifest("publisher is required".into()));
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
            if artifact.file.is_empty() {
                return Err(LoadError::Manifest("artifact file is required".into()));
            }
        }
        Ok(())
    }

    /// Serializes the manifest with the signature field removed. Signers must
    /// sign exactly these bytes (the struct's field order is the wire order).
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, LoadError> {
        let mut unsigned = self.clone();
        unsigned.signature = None;
        serde_json::to_vec(&unsigned).map_err(|err| LoadError::Manifest(err.to_string()))
    }
}

impl TrustStore {
    pub fn from_json(text: &str) -> Result<Self, LoadError> {
        serde_json::from_str(text).map_err(|err| LoadError::Manifest(err.to_string()))
    }

    pub fn verifying_key(&self, publisher: &str) -> Result<VerifyingKey, LoadError> {
        let encoded = self
            .publishers
            .get(publisher)
            .ok_or_else(|| LoadError::UnknownPublisher(publisher.to_string()))?;
        let bytes = BASE64
            .decode(encoded)
            .map_err(|err| LoadError::Key(err.to_string()))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| LoadError::Key("expected a 32-byte Ed25519 key".into()))?;
        VerifyingKey::from_bytes(&bytes).map_err(|err| LoadError::Key(err.to_string()))
    }

    pub fn verify(&self, manifest: &ComponentManifest) -> Result<(), LoadError> {
        manifest.validate()?;
        let encoded = manifest
            .signature
            .as_deref()
            .ok_or(LoadError::SignatureMissing)?;
        let signature_bytes = BASE64
            .decode(encoded)
            .map_err(|err| LoadError::Manifest(format!("signature base64: {err}")))?;
        let signature = Signature::from_slice(&signature_bytes)
            .map_err(|err| LoadError::Manifest(format!("signature bytes: {err}")))?;
        let key = self.verifying_key(&manifest.component.publisher)?;
        key.verify(&manifest.canonical_bytes()?, &signature)
            .map_err(|_| LoadError::SignatureInvalid)
    }
}

impl Artifact {
    /// Verifies the artifact's SHA-256 and returns its canonical path. Paths
    /// escaping `base_dir` are rejected.
    pub fn verify(&self, base_dir: &Path) -> Result<PathBuf, LoadError> {
        let canonical_base = base_dir.canonicalize()?;
        let canonical_path = canonical_base.join(&self.file).canonicalize()?;
        if !canonical_path.starts_with(&canonical_base) {
            return Err(LoadError::Manifest(format!(
                "artifact escapes the component directory: {}",
                self.file
            )));
        }
        let bytes = std::fs::read(&canonical_path)?;
        let actual = hex_digest(&Sha256::digest(&bytes));
        if !actual.eq_ignore_ascii_case(&self.sha256) {
            return Err(LoadError::ArtifactHash {
                expected: self.sha256.clone(),
                actual,
            });
        }
        Ok(canonical_path)
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::{Signer, SigningKey};

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
            signature: None,
        }
    }

    fn key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn trust_for(key: &SigningKey) -> TrustStore {
        let mut trust = TrustStore::default();
        trust
            .publishers
            .insert("acme".into(), BASE64.encode(key.verifying_key().to_bytes()));
        trust
    }

    fn sign(mut manifest: ComponentManifest) -> ComponentManifest {
        let signing = key();
        let signature = signing.sign(&manifest.canonical_bytes().unwrap());
        manifest.signature = Some(BASE64.encode(signature.to_bytes()));
        manifest
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

    #[test]
    fn signature_verifies_for_a_trusted_publisher() {
        let manifest = sign(manifest("llm.provider"));
        assert!(trust_for(&key()).verify(&manifest).is_ok());
    }

    #[test]
    fn tampering_breaks_the_signature() {
        let mut manifest = sign(manifest("llm.provider"));
        manifest.component.version = "2.0.0".into();
        assert!(matches!(
            trust_for(&key()).verify(&manifest),
            Err(LoadError::SignatureInvalid)
        ));
    }

    #[test]
    fn untrusted_publisher_is_rejected() {
        let manifest = sign(manifest("llm.provider"));
        let trust = TrustStore::default();
        assert!(matches!(
            trust.verify(&manifest),
            Err(LoadError::UnknownPublisher(_))
        ));
    }

    #[test]
    fn signature_is_required() {
        assert!(matches!(
            trust_for(&key()).verify(&manifest("llm.provider")),
            Err(LoadError::SignatureMissing)
        ));
    }

    #[test]
    fn artifact_hash_is_checked() {
        let dir = std::env::temp_dir().join(format!("msk-loader-{}", uuid_like()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("component.bin"), b"hello").unwrap();
        let mut manifest = manifest("llm.provider");
        manifest.artifact = Some(Artifact {
            file: "component.bin".into(),
            sha256: hex_digest(&Sha256::digest(b"hello")),
        });
        assert!(manifest.artifact.as_ref().unwrap().verify(&dir).is_ok());

        manifest.artifact.as_mut().unwrap().sha256 = "0".repeat(64);
        assert!(matches!(
            manifest.artifact.as_ref().unwrap().verify(&dir),
            Err(LoadError::ArtifactHash { .. })
        ));
        std::fs::remove_dir_all(dir).ok();
    }

    fn uuid_like() -> u128 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    }

    #[test]
    fn canonical_bytes_exclude_the_signature() {
        let unsigned = manifest("llm.provider");
        let signed = sign(unsigned.clone());
        assert_eq!(
            unsigned.canonical_bytes().unwrap(),
            signed.canonical_bytes().unwrap()
        );
        assert!(signed.signature.is_some());
    }
}
