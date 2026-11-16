//! Kernel policy: manifests declare capabilities, the policy table grants a
//! subset, and the resulting `ToolContext` is what the WASM host sees.

use std::path::Path;

use anyhow::{Context, Result};
use msk_core::{Capability, PolicyDecision, PolicyError, PolicyTable};
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
        ("session_create", vec![Capability::ExecSession]),
        ("session_exec", vec![Capability::ExecSession]),
        ("session_write", vec![Capability::ExecSession]),
        ("session_read", vec![Capability::ExecSession]),
        ("session_resize", vec![Capability::ExecSession]),
        ("session_close", vec![Capability::ExecSession]),
        ("artifact_read", vec![Capability::ArtifactRead]),
        ("read_file", vec![Capability::FsWorkspace]),
        ("write_file", vec![Capability::FsWorkspace]),
        ("list_dir", vec![Capability::FsWorkspace]),
        ("stat_file", vec![Capability::FsWorkspace]),
        (
            "http_request",
            vec![Capability::NetEgress, Capability::SecretsRead],
        ),
        ("net_dns", vec![Capability::NetEgress]),
        ("net_dial", vec![Capability::NetEgress]),
        ("net_tls", vec![Capability::NetEgress]),
        ("net_scan", vec![Capability::NetEgress]),
        ("net_info", vec![Capability::NetEgress]),
        ("web_request", vec![Capability::NetEgress]),
        ("file_hash", vec![Capability::FsWorkspace]),
        ("file_type", vec![Capability::FsWorkspace]),
        ("file_strings", vec![Capability::FsWorkspace]),
        ("file_hexdump", vec![Capability::FsWorkspace]),
        ("file_entropy", vec![Capability::FsWorkspace]),
        ("file_search", vec![Capability::FsWorkspace]),
        ("archive_list", vec![Capability::FsWorkspace]),
        ("archive_extract", vec![Capability::FsWorkspace]),
        ("binary_info", vec![Capability::FsWorkspace]),
        ("secret_scan", vec![Capability::FsWorkspace]),
        ("transform", vec![]),
        ("wasm_eval", vec![]),
        ("spawn_agent", vec![Capability::AgentSpawn]),
        ("agent_result", vec![Capability::AgentSpawn]),
        ("blackboard_get", vec![Capability::SharedState]),
        ("blackboard_set", vec![Capability::SharedState]),
        ("blackboard_list", vec![Capability::SharedState]),
        ("finding_add", vec![Capability::SharedState]),
        ("finding_list", vec![Capability::SharedState]),
        ("report_generate", vec![Capability::SharedState]),
        ("flag_add", vec![Capability::SharedState]),
        ("flag_list", vec![Capability::SharedState]),
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

/// Grants the same capabilities as `default_policy`, but prompts before
/// executing commands. Session creation, reads, resize, close, artifact
/// reads, and echo remain automatic.
pub fn policy_with_approvals(ask: bool) -> PolicyTable {
    let mut table = default_policy();
    if ask {
        for tool in [
            "exec",
            "session_exec",
            "session_write",
            "write_file",
            "spawn_agent",
            "http_request",
            "web_request",
            "net_dial",
            "net_scan",
            "archive_extract",
        ] {
            table.set_decision(tool, PolicyDecision::Ask);
        }
    }
    table
}

pub fn context_for(manifest: &ToolManifest) -> Result<ToolContext, PolicyError> {
    context_with(&default_policy(), manifest)
}

/// Tool-call permission posture, independent of the agent mode.
///
/// - `manual`: every action listed below asks.
/// - `auto`: workspace writes, scans and HTTP are allowed; commands, raw
///   dials, extraction and delegation still ask.
/// - `bypass`: no approval prompts; the policy table's hard denials still
///   apply, and envd deployment policy is unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionMode {
    Auto,
    Manual,
    Bypass,
}

impl PermissionMode {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" | "自动" => Some(Self::Auto),
            "manual" | "ask" | "手动" => Some(Self::Manual),
            "bypass" | "allow" | "放行" => Some(Self::Bypass),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Manual => "manual",
            Self::Bypass => "bypass",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "自动",
            Self::Manual => "手动",
            Self::Bypass => "放行",
        }
    }

    pub fn asks(self) -> bool {
        self != Self::Bypass
    }
}

impl Default for PermissionMode {
    fn default() -> Self {
        Self::Auto
    }
}

const MANUAL_ASK: &[&str] = &[
    "exec",
    "session_exec",
    "session_write",
    "write_file",
    "spawn_agent",
    "http_request",
    "web_request",
    "net_dial",
    "net_scan",
    "archive_extract",
];

const AUTO_ASK: &[&str] = &[
    "exec",
    "session_exec",
    "session_write",
    "spawn_agent",
    "http_request",
    "net_dial",
    "archive_extract",
];

pub fn policy_for_permission(mode: PermissionMode) -> PolicyTable {
    let mut table = default_policy();
    match mode {
        PermissionMode::Manual => {
            for tool in MANUAL_ASK {
                table.set_decision(*tool, PolicyDecision::Ask);
            }
        }
        PermissionMode::Auto => {
            for tool in AUTO_ASK {
                table.set_decision(*tool, PolicyDecision::Ask);
            }
        }
        PermissionMode::Bypass => {}
    }
    table
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

    #[test]
    fn session_tools_are_granted() {
        let ctx = context_for(&manifest("session_exec", &["exec.session"])).unwrap();
        assert_eq!(ctx.tool, "session_exec");
        assert!(ctx.allows(Capability::ExecSession));
    }

    #[test]
    fn approvals_only_prompt_for_commands() {
        let policy = policy_with_approvals(true);
        assert_eq!(policy.decision("exec"), PolicyDecision::Ask);
        assert_eq!(policy.decision("session_exec"), PolicyDecision::Ask);
        assert_eq!(policy.decision("session_write"), PolicyDecision::Ask);
        assert_eq!(policy.decision("session_create"), PolicyDecision::Allow);
        assert_eq!(policy.decision("artifact_read"), PolicyDecision::Allow);
        assert_eq!(policy.decision("write_file"), PolicyDecision::Ask);
        assert_eq!(policy.decision("read_file"), PolicyDecision::Allow);
        assert_eq!(policy.decision("spawn_agent"), PolicyDecision::Ask);
        assert_eq!(policy.decision("agent_result"), PolicyDecision::Allow);
        assert_eq!(policy.decision("http_request"), PolicyDecision::Ask);
        assert_eq!(policy.decision("web_request"), PolicyDecision::Ask);
        assert_eq!(policy.decision("net_dial"), PolicyDecision::Ask);
        assert_eq!(policy.decision("net_scan"), PolicyDecision::Ask);
        assert_eq!(policy.decision("net_dns"), PolicyDecision::Allow);
        assert_eq!(policy.decision("net_tls"), PolicyDecision::Allow);
        assert_eq!(policy.decision("archive_extract"), PolicyDecision::Ask);
        assert_eq!(policy.decision("binary_info"), PolicyDecision::Allow);
        assert_eq!(
            policy_with_approvals(false).decision("exec"),
            PolicyDecision::Allow
        );
    }

    #[test]
    fn security_tools_receive_only_declared_capabilities() {
        let ctx = context_for(&manifest("net_scan", &["net.egress"])).unwrap();
        assert!(ctx.allows(Capability::NetEgress));
        assert!(!ctx.allows(Capability::FsWorkspace));
        assert!(context_for(&manifest("net_scan", &["fs.workspace"])).is_err());

        let ctx = context_for(&manifest("binary_info", &["fs.workspace"])).unwrap();
        assert!(ctx.allows(Capability::FsWorkspace));
        assert!(!ctx.allows(Capability::NetEgress));

        let transform = context_for(&manifest("transform", &[])).unwrap();
        assert!(transform.capabilities.is_empty());
        assert!(context_for(&manifest("transform", &["net.egress"])).is_err());
    }

    #[test]
    fn permission_modes_choose_prompt_sets() {
        let manual = policy_for_permission(PermissionMode::Manual);
        assert_eq!(manual.decision("write_file"), PolicyDecision::Ask);
        assert_eq!(manual.decision("net_scan"), PolicyDecision::Ask);
        assert_eq!(manual.decision("read_file"), PolicyDecision::Allow);

        let auto = policy_for_permission(PermissionMode::Auto);
        assert_eq!(auto.decision("write_file"), PolicyDecision::Allow);
        assert_eq!(auto.decision("net_scan"), PolicyDecision::Allow);
        assert_eq!(auto.decision("web_request"), PolicyDecision::Allow);
        assert_eq!(auto.decision("exec"), PolicyDecision::Ask);
        assert_eq!(auto.decision("archive_extract"), PolicyDecision::Ask);
        assert_eq!(auto.decision("net_dial"), PolicyDecision::Ask);

        let bypass = policy_for_permission(PermissionMode::Bypass);
        assert_eq!(bypass.decision("exec"), PolicyDecision::Allow);
        assert_eq!(bypass.decision("archive_extract"), PolicyDecision::Allow);
        assert_eq!(bypass.decision("read_file"), PolicyDecision::Allow);
        assert_eq!(bypass.decision("not_granted"), PolicyDecision::Deny);
        assert!(PermissionMode::Bypass.asks() == false);
        assert!(PermissionMode::Auto.asks());
    }
}
