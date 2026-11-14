//! Kernel policy: manifests declare capabilities, the policy table grants a
//! subset, and the resulting `ToolContext` is what the WASM host sees.

use std::path::Path;

use anyhow::{Context, Result};
use msk_core::{Capability, PolicyError, PolicyTable};
use msk_wasm::{ToolContext, ToolManifest};

pub fn load_manifest(path: &Path) -> Result<ToolManifest> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read manifest {}", path.display()))?;
    let manifest: ToolManifest = serde_json::from_str(&text)
        .with_context(|| format!("parse manifest {}", path.display()))?;
    manifest
        .validate()
        .with_context(|| format!("validate manifest {}", path.display()))?;
    Ok(manifest)
}

/// M0 policy table. Replace with a data-driven policy file when the policy
/// engine lands; the important property is already here: a manifest can only
/// receive capabilities this table grants.
pub fn default_policy() -> PolicyTable {
    PolicyTable::from_grants([
        ("exec", vec![Capability::ExecSession]),
        ("session_exec", vec![Capability::ExecSession]),
        ("echo", vec![]),
    ])
}

pub fn context_with(
    policy: &PolicyTable,
    manifest: &ToolManifest,
) -> Result<ToolContext, PolicyError> {
    let granted = policy.authorize(&manifest.name, &manifest.capabilities)?;
    Ok(ToolContext::new(manifest.name.clone(), granted))
}

pub fn context_for(manifest: &ToolManifest) -> Result<ToolContext, PolicyError> {
    context_with(&default_policy(), manifest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(name: &str, capabilities: &[&str]) -> ToolManifest {
        ToolManifest {
            name: name.into(),
            description: String::new(),
            input_schema: serde_json::json!({"type": "object"}),
            capabilities: capabilities.iter().map(|value| value.to_string()).collect(),
            max_memory_bytes: None,
            timeout_ms: None,
        }
    }

    #[test]
    fn granted_capability_reaches_the_context() {
        let ctx = context_for(&manifest("exec", &["exec.session"])).unwrap();
        assert_eq!(ctx.tool, "exec");
        assert!(ctx.allows(Capability::ExecSession));
    }

    #[test]
    fn manifest_cannot_self_grant() {
        assert!(context_for(&manifest("exec", &["net.egress"])).is_err());
    }

    #[test]
    fn unknown_tool_is_denied() {
        assert!(context_for(&manifest("missing", &[])).is_err());
    }
}
