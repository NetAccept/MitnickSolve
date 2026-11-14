//! Domain types shared by the kernel, ports, and components.

pub mod context;

use std::collections::HashMap;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

pub type TaskId = String;
pub type CallId = String;
pub type ObjectId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Created,
    Planning,
    Executing,
    Waiting,
    Reflecting,
    Replanning,
    Done,
    Failed,
    Cancelled,
    Timeboxed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Budget {
    pub tokens: u64,
    pub cost_micros: u64,
    pub wall_ms: u64,
    pub tool_calls: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub goal: String,
    pub state: TaskState,
    pub budget: Budget,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: CallId,
    pub tool: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataLabel {
    Public,
    Challenge,
    Derived,
    Secret,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRef {
    pub id: ObjectId,
    pub kind: String,
    pub size: Option<u64>,
    pub label: DataLabel,
    pub hash: Option<String>,
    pub generation: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    Error,
    Denied,
    Truncated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub call_id: CallId,
    pub status: ToolStatus,
    pub summary: String,
    pub artifacts: Vec<ObjectRef>,
    pub truncated: bool,
    pub cost_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    #[serde(rename = "net.egress")]
    NetEgress,
    #[serde(rename = "exec.session")]
    ExecSession,
    #[serde(rename = "fs.workspace")]
    FsWorkspace,
    #[serde(rename = "fs.host")]
    FsHost,
    #[serde(rename = "secrets.read")]
    SecretsRead,
    #[serde(rename = "data.export")]
    DataExport,
    #[serde(rename = "artifact.read")]
    ArtifactRead,
    #[serde(rename = "mcp.connect")]
    McpConnect,
    #[serde(rename = "agent.spawn")]
    AgentSpawn,
    #[serde(rename = "state.shared")]
    SharedState,
}

impl Capability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NetEgress => "net.egress",
            Self::ExecSession => "exec.session",
            Self::FsWorkspace => "fs.workspace",
            Self::FsHost => "fs.host",
            Self::SecretsRead => "secrets.read",
            Self::DataExport => "data.export",
            Self::ArtifactRead => "artifact.read",
            Self::McpConnect => "mcp.connect",
            Self::AgentSpawn => "agent.spawn",
            Self::SharedState => "state.shared",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "net.egress" => Some(Self::NetEgress),
            "exec.session" => Some(Self::ExecSession),
            "fs.workspace" => Some(Self::FsWorkspace),
            "fs.host" => Some(Self::FsHost),
            "secrets.read" => Some(Self::SecretsRead),
            "data.export" => Some(Self::DataExport),
            "artifact.read" => Some(Self::ArtifactRead),
            "mcp.connect" => Some(Self::McpConnect),
            "agent.spawn" => Some(Self::AgentSpawn),
            "state.shared" => Some(Self::SharedState),
            _ => None,
        }
    }
}

impl FromStr for Capability {
    type Err = PolicyError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or_else(|| PolicyError::UnknownCapability(value.into()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    #[error("policy denied: {0}")]
    Denied(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("internal: {0}")]
    Internal(String),
}

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),
    #[error("unknown capability: {0}")]
    UnknownCapability(String),
    #[error("capability not granted: {0}")]
    NotGranted(String),
}

/// Tri-state policy for an invocation. Capability grants decide what a tool
/// may do; this decides whether the kernel executes it directly, asks the
/// user, or refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    Ask,
    Deny,
}

impl PolicyDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
        }
    }
}

/// Kernel-owned policy table: tool name -> capabilities the kernel is willing
/// to grant. A manifest may declare a subset of these; it can never self-grant.
#[derive(Debug, Clone, Default)]
pub struct PolicyTable {
    grants: HashMap<String, Vec<Capability>>,
    decisions: HashMap<String, PolicyDecision>,
}

impl PolicyTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_grants<I, S>(grants: I) -> Self
    where
        I: IntoIterator<Item = (S, Vec<Capability>)>,
        S: Into<String>,
    {
        let mut table = Self::default();
        for (tool, capabilities) in grants {
            table.grant(tool, capabilities);
        }
        table
    }

    pub fn grant(&mut self, tool: impl Into<String>, capabilities: Vec<Capability>) {
        self.grants.insert(tool.into(), capabilities);
    }

    pub fn set_decision(&mut self, tool: impl Into<String>, decision: PolicyDecision) {
        self.decisions.insert(tool.into(), decision);
    }

    /// Returns the explicit decision for a tool, or Allow for a granted tool
    /// and Deny for an unknown one.
    pub fn decision(&self, tool: &str) -> PolicyDecision {
        self.decisions.get(tool).copied().unwrap_or_else(|| {
            if self.grants.contains_key(tool) {
                PolicyDecision::Allow
            } else {
                PolicyDecision::Deny
            }
        })
    }

    pub fn authorize(
        &self,
        tool: &str,
        declared: &[String],
    ) -> Result<Vec<Capability>, PolicyError> {
        let allowed = self
            .grants
            .get(tool)
            .ok_or_else(|| PolicyError::UnknownTool(tool.into()))?;

        let mut granted = Vec::new();
        for raw in declared {
            let capability = Capability::parse(raw)
                .ok_or_else(|| PolicyError::UnknownCapability(raw.clone()))?;
            if !allowed.contains(&capability) {
                return Err(PolicyError::NotGranted(raw.clone()));
            }
            if !granted.contains(&capability) {
                granted.push(capability);
            }
        }
        Ok(granted)
    }
}

/// The only boundary through which the model can affect the host.
///
/// Implementations are WASM tools dispatched by the kernel. Native code never
/// implements this trait outside tests.
pub trait ToolRuntime: Send + Sync {
    fn invoke(&self, call: &ToolCall) -> Result<Observation, CoreError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_serde_roundtrip() {
        let task = Task {
            id: "t1".into(),
            goal: "solve web-01".into(),
            state: TaskState::Planning,
            budget: Budget::default(),
        };
        let json = serde_json::to_string(&task).unwrap();
        let back: Task = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "t1");
        assert_eq!(back.state, TaskState::Planning);
    }

    #[test]
    fn labels_are_snake_case() {
        let label = serde_json::to_string(&DataLabel::Secret).unwrap();
        assert_eq!(label, "\"secret\"");
    }

    #[test]
    fn capabilities_use_dotted_names() {
        let capability = serde_json::to_string(&Capability::ExecSession).unwrap();
        assert_eq!(capability, "\"exec.session\"");
        assert_eq!(Capability::ExecSession.as_str(), "exec.session");
        assert_eq!(
            Capability::parse("exec.session"),
            Some(Capability::ExecSession)
        );
        assert_eq!(
            Capability::parse("artifact.read"),
            Some(Capability::ArtifactRead)
        );
    }

    #[test]
    fn policy_grants_declared_capabilities() {
        let policy = PolicyTable::from_grants([("exec", vec![Capability::ExecSession])]);
        let granted = policy.authorize("exec", &["exec.session".into()]).unwrap();
        assert_eq!(granted, vec![Capability::ExecSession]);
    }

    #[test]
    fn policy_grants_only_the_intersection() {
        let policy = PolicyTable::from_grants([(
            "exec",
            vec![Capability::ExecSession, Capability::NetEgress],
        )]);
        let granted = policy.authorize("exec", &["exec.session".into()]).unwrap();
        assert_eq!(granted, vec![Capability::ExecSession]);
    }

    #[test]
    fn policy_rejects_ungranted_capability() {
        let policy = PolicyTable::from_grants([("exec", vec![Capability::ExecSession])]);
        assert!(matches!(
            policy.authorize("exec", &["net.egress".into()]),
            Err(PolicyError::NotGranted(_))
        ));
    }

    #[test]
    fn policy_rejects_unknown_capability() {
        let policy = PolicyTable::from_grants([("exec", vec![Capability::ExecSession])]);
        assert!(matches!(
            policy.authorize("exec", &["made.up".into()]),
            Err(PolicyError::UnknownCapability(_))
        ));
    }

    #[test]
    fn policy_rejects_unknown_tool() {
        let policy = PolicyTable::new();
        assert!(matches!(
            policy.authorize("missing", &[]),
            Err(PolicyError::UnknownTool(_))
        ));
    }

    #[test]
    fn policy_decisions_default_to_allow_or_deny() {
        let mut policy = PolicyTable::new();
        policy.grant("exec", vec![Capability::ExecSession]);
        assert_eq!(policy.decision("exec"), PolicyDecision::Allow);
        assert_eq!(policy.decision("missing"), PolicyDecision::Deny);
        policy.set_decision("exec", PolicyDecision::Ask);
        assert_eq!(policy.decision("exec"), PolicyDecision::Ask);
    }
}
