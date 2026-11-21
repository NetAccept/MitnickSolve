//! Session-scoped execution targets.
//!
//! A target describes how a session reaches an execution backend. The built-in
//! `none` target runs the session without any execution environment; everything
//! else is either an embedded envd process or a remote envd service. Targets are
//! selected when a task is created and can be re-bound between turns.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const NONE_TARGET: &str = "none";
pub const DEFAULT_TARGET: &str = "local";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TargetKind {
    Embedded,
    Service,
    None,
}

impl TargetKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Embedded => "embedded",
            Self::Service => "service",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetSpec {
    pub id: String,
    #[serde(default)]
    pub label: Option<String>,
    pub kind: TargetKind,
    /// Embedded envd binary. Relative paths resolve against the config file.
    #[serde(default)]
    pub binary: Option<PathBuf>,
    /// Remote envd control-plane URL.
    #[serde(default)]
    pub url: Option<String>,
    /// Environment variable holding the remote bearer token.
    #[serde(default)]
    pub token_env: Option<String>,
    /// Legacy in-memory token. Never read from or written to a targets file.
    #[serde(skip)]
    pub token: Option<String>,
    /// PEM CA bundle for the remote control plane.
    #[serde(default)]
    pub ca: Option<PathBuf>,
    /// Optional binary data-plane address (used by the daemon terminal).
    #[serde(default)]
    pub data_addr: Option<String>,
    #[serde(default)]
    pub data_tls: bool,
    #[serde(default)]
    pub data_tls_name: Option<String>,
    #[serde(default = "default_profile")]
    pub profile: String,
    /// Workspace root passed to embedded envd / used as the session workspace.
    #[serde(default)]
    pub workspace: Option<PathBuf>,
}

fn default_profile() -> String {
    "dev".into()
}

impl TargetSpec {
    pub fn none() -> Self {
        Self {
            id: NONE_TARGET.into(),
            label: Some("No execution environment".into()),
            kind: TargetKind::None,
            binary: None,
            url: None,
            token_env: None,
            token: None,
            ca: None,
            data_addr: None,
            data_tls: false,
            data_tls_name: None,
            profile: "dev".into(),
            workspace: None,
        }
    }

    pub fn label(&self) -> &str {
        self.label
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or(&self.id)
    }

    pub fn is_none(&self) -> bool {
        self.kind == TargetKind::None
    }

    pub fn token_value(&self) -> Result<Option<String>> {
        if let Some(token) = &self.token
            && !token.is_empty()
        {
            return Ok(Some(token.clone()));
        }
        match &self.token_env {
            Some(name) if !name.is_empty() => match std::env::var(name) {
                Ok(value) if !value.is_empty() => Ok(Some(value)),
                _ => bail!("target {}: environment variable {name} is not set", self.id),
            },
            _ => Ok(None),
        }
    }

    fn validate(&self) -> Result<()> {
        if self.id.trim().is_empty() {
            bail!("target id cannot be empty");
        }
        if self.id == NONE_TARGET {
            bail!("target id {NONE_TARGET} is reserved");
        }
        if self.id.chars().any(char::is_whitespace) {
            bail!("target id {} contains whitespace", self.id);
        }
        match self.kind {
            TargetKind::Embedded => {
                if self.binary.is_none() {
                    bail!("embedded target {} needs a binary", self.id);
                }
                if self.url.is_some() {
                    bail!("embedded target {} cannot have a url", self.id);
                }
                if self.profile != "dev" {
                    bail!(
                        "embedded target {} only supports the dev profile (service mode is required for {})",
                        self.id,
                        self.profile
                    );
                }
            }
            TargetKind::Service => {
                let url = self
                    .url
                    .as_deref()
                    .filter(|value| !value.is_empty())
                    .with_context(|| format!("service target {} needs a url", self.id))?;
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    bail!("target {} url must start with http:// or https://", self.id);
                }
                if self.binary.is_some() {
                    bail!("service target {} cannot have a binary", self.id);
                }
                if !matches!(self.profile.as_str(), "dev" | "host" | "isolated") {
                    bail!("target {} has unknown profile {}", self.id, self.profile);
                }
                if self.data_tls && self.data_addr.is_none() {
                    bail!("target {} sets data_tls without data_addr", self.id);
                }
            }
            TargetKind::None => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetInfo {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub default: bool,
    pub profile: String,
    pub attached: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TargetsFile {
    #[serde(default)]
    pub default: Option<String>,
    #[serde(default)]
    pub targets: Vec<TargetSpec>,
}

#[derive(Debug, Clone)]
pub struct TargetRegistry {
    default: String,
    targets: Vec<TargetSpec>,
    none: TargetSpec,
    path: Option<PathBuf>,
}

impl TargetRegistry {
    /// Loads an optional JSON config and merges a legacy `--envd` /
    /// `--envd-url` definition as a user target when it does not collide.
    pub fn load(path: Option<&Path>, legacy: Option<TargetSpec>) -> Result<Self> {
        let mut targets = Vec::new();
        let mut default = None;
        if let Some(path) = path {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("read targets file {}", path.display()))?;
            let file: TargetsFile = serde_json::from_str(&text)
                .with_context(|| format!("parse targets file {}", path.display()))?;
            targets = file.targets;
            default = file.default;
        }
        if let Some(legacy) = legacy
            && targets.iter().all(|target| target.id != legacy.id)
        {
            targets.push(legacy);
        }
        if targets.is_empty() {
            bail!("no execution targets configured");
        }
        let mut seen = HashSet::new();
        for target in &targets {
            target.validate()?;
            if !seen.insert(target.id.clone()) {
                bail!("duplicate target id {}", target.id);
            }
        }
        let default = match default {
            Some(value) => {
                if !targets.iter().any(|target| target.id == value) {
                    bail!("default execution target {value} is not defined");
                }
                value
            }
            None => targets
                .iter()
                .find(|target| target.id == DEFAULT_TARGET)
                .map(|target| target.id.clone())
                .unwrap_or_else(|| targets[0].id.clone()),
        };
        Ok(Self {
            default,
            targets,
            none: TargetSpec::none(),
            path: path.map(Path::to_path_buf),
        })
    }

    /// A registry with no execution targets. Used by `--target none` when no
    /// targets file exists at all.
    pub fn none_only() -> Self {
        Self {
            default: NONE_TARGET.into(),
            targets: Vec::new(),
            none: TargetSpec::none(),
            path: None,
        }
    }

    /// Applies a CLI default override, which must name a configured target or
    /// the reserved `none` target.
    pub fn select_default(mut self, id: Option<&str>) -> Result<Self> {
        if let Some(id) = id.map(str::trim).filter(|value| !value.is_empty()) {
            if !self.contains(id) {
                bail!("unknown default execution target {id}");
            }
            self.default = id.to_string();
        }
        Ok(self)
    }

    pub fn default_id(&self) -> &str {
        &self.default
    }

    pub fn default_target(&self) -> &TargetSpec {
        self.targets
            .iter()
            .find(|target| target.id == self.default)
            .unwrap_or(&self.targets[0])
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn targets(&self) -> &[TargetSpec] {
        &self.targets
    }

    pub fn is_none(&self, id: &str) -> bool {
        id == NONE_TARGET
    }

    pub fn resolve(&self, id: &str) -> Result<&TargetSpec> {
        let id = id.trim();
        if id.is_empty() {
            return Ok(self.default_target());
        }
        if self.is_none(id) {
            return Ok(&self.none);
        }
        self.targets
            .iter()
            .find(|target| target.id == id)
            .with_context(|| format!("unknown execution target {id}"))
    }

    pub fn contains(&self, id: &str) -> bool {
        id == NONE_TARGET || self.targets.iter().any(|target| target.id == id)
    }

    pub fn list(&self, attached: Option<&str>) -> Vec<TargetInfo> {
        let mut items: Vec<TargetInfo> = self
            .targets
            .iter()
            .map(|target| TargetInfo {
                id: target.id.clone(),
                label: target.label().to_string(),
                kind: target.kind.as_str().to_string(),
                default: target.id == self.default,
                profile: target.profile.clone(),
                attached: attached == Some(target.id.as_str()) && !target.is_none(),
            })
            .collect();
        items.push(TargetInfo {
            id: NONE_TARGET.into(),
            label: self.none.label().to_string(),
            kind: "none".into(),
            default: self.default == NONE_TARGET,
            profile: "none".into(),
            attached: attached == Some(NONE_TARGET),
        });
        items
    }

    /// Resolves a relative embedded binary against the config directory, the
    /// current directory and the daemon binary directory, in that order.
    pub fn resolve_binary(
        &self,
        target: &TargetSpec,
        daemon_dir: Option<&Path>,
    ) -> Option<PathBuf> {
        let binary = target.binary.as_ref()?;
        if binary.is_absolute() {
            return binary.exists().then(|| binary.clone());
        }
        let mut candidates = Vec::new();
        if let Some(path) = self.path.as_deref().and_then(Path::parent) {
            candidates.push(path.join(binary));
        }
        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(cwd.join(binary));
        }
        if let Some(dir) = daemon_dir {
            candidates.push(dir.join(binary));
        }
        candidates.into_iter().find(|candidate| candidate.exists())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn embedded(id: &str) -> TargetSpec {
        TargetSpec {
            id: id.into(),
            label: None,
            kind: TargetKind::Embedded,
            binary: Some(PathBuf::from("envd")),
            url: None,
            token_env: None,
            token: None,
            ca: None,
            data_addr: None,
            data_tls: false,
            data_tls_name: None,
            profile: "dev".into(),
            workspace: None,
        }
    }

    #[test]
    fn parses_defaults_and_merges_legacy() {
        let dir = std::env::temp_dir().join(format!(
            "msk-targets-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("envd.json");
        std::fs::write(
            &path,
            r#"{"default":"lab","targets":[
                {"id":"lab","kind":"service","url":"https://envd.lab:7800","token_env":"LAB_TOKEN","profile":"host"}
            ]}"#,
        )
        .unwrap();
        let registry = TargetRegistry::load(Some(&path), Some(embedded("local"))).unwrap();
        assert_eq!(registry.default_id(), "lab");
        assert!(registry.contains("local"));
        assert!(registry.contains("none"));
        assert!(registry.list(Some("lab"))[0].attached);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn rejects_duplicate_and_reserved_ids() {
        let mut registry = TargetsFile {
            default: None,
            targets: vec![embedded("local"), embedded("local")],
        };
        let dir = std::env::temp_dir().join(format!(
            "msk-targets-dup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("envd.json");
        std::fs::write(&path, serde_json::to_vec(&registry).unwrap()).unwrap();
        assert!(TargetRegistry::load(Some(&path), None).is_err());
        registry.targets = vec![embedded("none")];
        std::fs::write(&path, serde_json::to_vec(&registry).unwrap()).unwrap();
        assert!(TargetRegistry::load(Some(&path), None).is_err());
        std::fs::remove_dir_all(dir).ok();
    }
}
