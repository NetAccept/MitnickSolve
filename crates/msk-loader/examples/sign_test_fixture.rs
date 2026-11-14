//! Test fixture generator: signs the workspace msk-test-component artifact so
//! the CLI/daemon component path can be exercised end to end. Do not use this
//! fixed key for real components.
//!
//! Usage: cargo run -p msk-loader --example sign_test_fixture -- <artifact> <out-dir>

use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use ed25519_dalek::{Signer, SigningKey};
use msk_loader::{Artifact, ComponentInfo, ComponentManifest};
use sha2::{Digest, Sha256};

fn main() {
    let mut args = std::env::args_os().skip(1);
    let artifact = PathBuf::from(args.next().expect("artifact path"));
    let out_dir = PathBuf::from(args.next().expect("out dir"));
    std::fs::create_dir_all(&out_dir).unwrap();
    let file_name = artifact.file_name().unwrap().to_string_lossy().into_owned();
    let copied = out_dir.join(&file_name);
    std::fs::copy(&artifact, &copied).unwrap();
    let bytes = std::fs::read(&copied).unwrap();
    let sha256 = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();

    let signing = SigningKey::from_bytes(&[9u8; 32]);
    let mut manifest = ComponentManifest {
        component: ComponentInfo {
            id: "test.llm".into(),
            version: "1.0.0".into(),
            abi: msk_abi::MS_ABI_VERSION,
            port: "llm.provider".into(),
            publisher: "acme".into(),
            thread_safety: "serial".into(),
        },
        requires: None,
        artifact: Some(Artifact {
            file: file_name,
            sha256,
        }),
        signature: None,
    };
    let signature = signing.sign(&manifest.canonical_bytes().unwrap());
    manifest.signature = Some(BASE64.encode(signature.to_bytes()));
    std::fs::write(
        out_dir.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    let trust = serde_json::json!({
        "publishers": {
            "acme": BASE64.encode(signing.verifying_key().to_bytes())
        }
    });
    std::fs::write(
        out_dir.join("trust.json"),
        serde_json::to_vec_pretty(&trust).unwrap(),
    )
    .unwrap();
    println!("{}", out_dir.join("manifest.json").display());
}
