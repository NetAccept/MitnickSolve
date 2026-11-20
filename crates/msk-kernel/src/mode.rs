//! Configurable agent modes.
//!
//! Built-in modes are `plan`, `investigate`, `exploit`, `harden`, plus the
//! reserved `auto` router. A `modes.json` file can override built-ins, disable
//! them, or add new modes. Each mode declares:
//!
//! - `tools`: the only tool names the kernel loads for that mode. `"*"` means
//!   every registered tool; `"@other-mode"` reuses another mode's tool set.
//! - `spawn`: child modes this mode may delegate to. `"*"` allows any.
//! - `prompt`: operating rules appended to the system prompt.
//! - `keywords` and `priority`: deterministic inputs for `auto` routing.
//! - `write_capable`: whether HTTP bodies and write tools are allowed. If
//!   omitted, it is inferred from the tool set.
//!
//! A mode is a security boundary: the kernel loads only the mode's tools.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const AUTO_ID: &str = "auto";

pub const ALL_TOOLS: &[&str] = &[
    "exec",
    "session_create",
    "session_exec",
    "session_write",
    "session_read",
    "session_resize",
    "session_close",
    "artifact_read",
    "read_file",
    "write_file",
    "list_dir",
    "stat_file",
    "http_request",
    "spawn_agent",
    "agent_result",
    "blackboard_get",
    "blackboard_set",
    "blackboard_list",
    "finding_add",
    "finding_list",
    "report_generate",
    "flag_add",
    "flag_list",
    "net_dns",
    "net_dial",
    "net_tls",
    "net_scan",
    "net_info",
    "web_request",
    "file_hash",
    "file_type",
    "file_strings",
    "file_hexdump",
    "file_entropy",
    "file_search",
    "archive_list",
    "archive_extract",
    "binary_info",
    "secret_scan",
    "transform",
    "wasm_eval",
    "echo",
];

const WRITE_TOOLS: &[&str] = &[
    "exec",
    "session_create",
    "session_exec",
    "session_write",
    "write_file",
    "http_request",
    "net_dial",
    "web_request",
    "archive_extract",
];

const PLAN_DENY: &[&str] = &[
    "exec",
    "session_create",
    "session_exec",
    "session_write",
    "session_read",
    "session_resize",
    "session_close",
    "write_file",
    "http_request",
    "blackboard_set",
    "finding_add",
    "flag_add",
    "net_dial",
    "net_scan",
    "web_request",
    "archive_extract",
];

const INVESTIGATE_DENY: &[&str] = &[
    "exec",
    "session_create",
    "session_exec",
    "session_write",
    "session_read",
    "session_resize",
    "session_close",
    "write_file",
    "http_request",
    "blackboard_set",
    "net_dial",
    "archive_extract",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeSpec {
    pub id: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub prompt: String,
    /// Optional base mode to copy before applying non-empty overrides.
    #[serde(default)]
    pub extends: Option<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub spawn: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(default = "default_order")]
    pub order: u32,
    #[serde(default)]
    pub write_capable: Option<bool>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub builtin: bool,
}

fn default_priority() -> u32 {
    1
}

fn default_order() -> u32 {
    100
}

impl ModeSpec {
    pub fn info(&self) -> ModeInfo {
        ModeInfo {
            id: self.id.clone(),
            label: self.label.clone(),
            description: self.description.clone(),
            priority: self.priority,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModeInfo {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub priority: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AutoConfig {
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ModesFile {
    #[serde(default)]
    pub modes: Vec<ModeSpec>,
    #[serde(default)]
    pub auto: AutoConfig,
}

#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub mode: String,
    pub reason: String,
    pub scores: Vec<(String, usize)>,
}

#[derive(Debug, Clone)]
pub struct ModeRegistry {
    modes: Vec<ModeSpec>,
    by_id: HashMap<String, usize>,
    auto_default: String,
}

impl Default for ModeRegistry {
    fn default() -> Self {
        Self::builtin().expect("built-in modes are valid")
    }
}

impl ModeRegistry {
    pub fn builtin() -> Result<Self> {
        let mut modes = vec![
            ModeSpec {
                id: "plan".into(),
                label: "计划".into(),
                description: "read-only: produce a plan, execute nothing".into(),
                prompt: "MODE: PLAN. Do not execute commands, modify the workspace or target, or \
                         send payloads. Inspect read-only evidence and produce an actionable plan: \
                         objectives, scope, ordered steps, required tools and approvals, risks, \
                         stop conditions, and rollback. State clearly that nothing has been \
                         executed."
                    .into(),
                extends: None,
                tools: tools_without(PLAN_DENY),
                spawn: vec!["plan".into()],
                keywords: vec![
                    "plan".into(),
                    "strategy".into(),
                    "roadmap".into(),
                    "design".into(),
                    "approach".into(),
                    "how would".into(),
                    "steps".into(),
                    "方案".into(),
                    "计划".into(),
                    "设计".into(),
                    "架构".into(),
                    "步骤".into(),
                ],
                priority: 3,
                order: 0,
                write_capable: Some(false),
                disabled: false,
                builtin: true,
            },
            ModeSpec {
                id: "investigate".into(),
                label: "调查".into(),
                description: "read-only recon and evidence collection".into(),
                prompt: "MODE: INVESTIGATE. Collect evidence read-only. Reading files, DNS, TLS, \
                         interfaces and file/binary analysis are allowed. Scanning and HTTP \
                         requests are approval-gated; use GET/HEAD only. Do not write files, run \
                         commands, create sessions, send raw payloads, extract archives, or change \
                         a target. Record observations as structured findings and distinguish \
                         facts from hypotheses."
                    .into(),
                extends: None,
                tools: tools_without(INVESTIGATE_DENY),
                spawn: vec!["plan".into(), "investigate".into()],
                keywords: vec![
                    "investigate".into(),
                    "investigation".into(),
                    "recon".into(),
                    "enumerate".into(),
                    "scan".into(),
                    "triage".into(),
                    "audit".into(),
                    "analyze".into(),
                    "analyse".into(),
                    "inspect".into(),
                    "identify".into(),
                    "find".into(),
                    "review".into(),
                    "调查".into(),
                    "侦察".into(),
                    "排查".into(),
                    "分析".into(),
                    "审计".into(),
                    "取证".into(),
                    "扫描".into(),
                ],
                priority: 1,
                order: 1,
                write_capable: Some(false),
                disabled: false,
                builtin: true,
            },
            ModeSpec {
                id: "exploit".into(),
                label: "攻击".into(),
                description: "authorized active testing, approvals on".into(),
                prompt:
                    "MODE: EXPLOIT. Perform only the authorized active testing in scope. Choose \
                         the smallest action that answers the question, expect approval for \
                         commands and network actions, stop on unexpected impact, and preserve \
                         evidence. Record confirmed findings and never target anything outside the \
                         engagement."
                        .into(),
                extends: None,
                tools: vec!["*".into()],
                spawn: vec!["*".into()],
                keywords: vec![
                    "exploit".into(),
                    "attack".into(),
                    "pwn".into(),
                    "bypass".into(),
                    "inject".into(),
                    "brute".into(),
                    "get a shell".into(),
                    "reverse shell".into(),
                    "shell".into(),
                    "shellcode".into(),
                    "execute".into(),
                    "run the".into(),
                    "challenge".into(),
                    "flag".into(),
                    "escalate".into(),
                    "privilege".into(),
                    "payload".into(),
                    "ctf".into(),
                    "攻击".into(),
                    "利用".into(),
                    "渗透".into(),
                    "提权".into(),
                    "拿flag".into(),
                    "绕过".into(),
                ],
                priority: 2,
                order: 2,
                write_capable: Some(true),
                disabled: false,
                builtin: true,
            },
            ModeSpec {
                id: "harden".into(),
                label: "加固".into(),
                description: "defensive remediation and verification".into(),
                prompt: "MODE: HARDEN. Defensive remediation only. Back up before changing a \
                         system, make minimal reversible changes, prefer configuration over \
                         patching binaries, verify the result, and record before/after evidence. \
                         Do not run exploit actions or attack other hosts."
                    .into(),
                extends: None,
                tools: vec!["*".into()],
                spawn: vec!["plan".into(), "investigate".into(), "harden".into()],
                keywords: vec![
                    "harden".into(),
                    "fix".into(),
                    "patch".into(),
                    "remediate".into(),
                    "secure".into(),
                    "configure".into(),
                    "firewall".into(),
                    "backup".into(),
                    "restore".into(),
                    "加固".into(),
                    "修复".into(),
                    "配置".into(),
                    "防护".into(),
                    "备份".into(),
                    "恢复".into(),
                ],
                priority: 3,
                order: 3,
                write_capable: Some(true),
                disabled: false,
                builtin: true,
            },
        ];
        modes.sort_by(|left, right| left.id.cmp(&right.id));
        Self::from_parts(modes, "investigate".into())
    }

    pub fn load(root: &Path, explicit: Option<&Path>) -> Result<Self> {
        // Only the implicit <root>/modes.json is optional. A path the caller
        // named explicitly must exist; silently falling back to the built-in
        // modes would ignore the configured tool and spawn policy.
        let (path, required) = match explicit {
            Some(path) => (path.to_path_buf(), true),
            None => match std::env::var_os("MSK_MODES") {
                Some(path) => (PathBuf::from(path), true),
                None => (root.join("modes.json"), false),
            },
        };
        if !path.exists() {
            if required {
                bail!("modes file not found: {}", path.display());
            }
            return Self::builtin();
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read modes file {}", path.display()))?;
        let file: ModesFile = serde_json::from_str(&text)
            .with_context(|| format!("parse modes file {}", path.display()))?;
        Self::from_file(file).with_context(|| format!("validate modes file {}", path.display()))
    }

    pub fn from_file(file: ModesFile) -> Result<Self> {
        let mut modes = Self::builtin()?.modes;
        for mut spec in file.modes {
            if spec.disabled {
                modes.retain(|existing| existing.id != spec.id);
                continue;
            }
            if let Some(base_id) = spec.extends.clone() {
                let Some(base) = modes
                    .iter()
                    .find(|existing| existing.id == base_id)
                    .cloned()
                else {
                    bail!("mode {} extends unknown mode {}", spec.id, base_id);
                };
                let mut merged = base;
                if !spec.label.trim().is_empty() {
                    merged.label = spec.label;
                }
                if !spec.description.trim().is_empty() {
                    merged.description = spec.description;
                }
                if !spec.prompt.trim().is_empty() {
                    merged.prompt = spec.prompt;
                }
                if !spec.tools.is_empty() {
                    merged.tools = spec.tools;
                }
                if !spec.spawn.is_empty() {
                    merged.spawn = spec.spawn;
                }
                if !spec.keywords.is_empty() {
                    merged.keywords = spec.keywords;
                }
                if spec.priority != default_priority() {
                    merged.priority = spec.priority;
                }
                if spec.order != default_order() {
                    merged.order = spec.order;
                }
                if spec.write_capable.is_some() {
                    merged.write_capable = spec.write_capable;
                }
                merged.id = spec.id;
                merged.extends = None;
                merged.builtin = false;
                merged.disabled = false;
                spec = merged;
            }
            if let Some(existing) = modes.iter_mut().find(|existing| existing.id == spec.id) {
                let builtin = existing.builtin;
                let mut replacement = spec;
                replacement.builtin = builtin;
                *existing = replacement;
            } else {
                modes.push(spec);
            }
        }
        let auto_default = file
            .auto
            .default
            .unwrap_or_else(|| "investigate".to_string());
        Self::from_parts(modes, auto_default)
    }

    fn from_parts(mut modes: Vec<ModeSpec>, auto_default: String) -> Result<Self> {
        if modes.is_empty() {
            bail!("at least one concrete mode is required");
        }
        let mut by_id = HashMap::new();
        for (index, spec) in modes.iter_mut().enumerate() {
            validate_id(&spec.id)?;
            if spec.id == AUTO_ID {
                bail!("mode id 'auto' is reserved for routing");
            }
            if spec.label.trim().is_empty() {
                spec.label = spec.id.clone();
            }
            if spec.prompt.trim().is_empty() {
                spec.prompt = format!("MODE: {}.", spec.id.to_uppercase());
            }
            if spec.label.chars().count() > 64 {
                bail!("mode {} label exceeds 64 characters", spec.id);
            }
            if spec.prompt.chars().count() > 16_000 {
                bail!("mode {} prompt exceeds 16000 characters", spec.id);
            }
            if by_id.insert(spec.id.clone(), index).is_some() {
                bail!("duplicate mode id: {}", spec.id);
            }
        }
        for spec in &modes {
            for tool in &spec.tools {
                if tool == "*" || tool.starts_with('@') {
                    continue;
                }
                if !ALL_TOOLS.contains(&tool.as_str()) {
                    bail!("mode {} references unknown tool {}", spec.id, tool);
                }
            }
            for child in &spec.spawn {
                if child == "*" {
                    continue;
                }
                if !by_id.contains_key(child) {
                    bail!("mode {} spawns unknown mode {}", spec.id, child);
                }
            }
        }
        let auto_default = if by_id.contains_key(&auto_default) {
            auto_default
        } else {
            modes
                .iter()
                .find(|spec| spec.id != AUTO_ID)
                .map(|spec| spec.id.clone())
                .unwrap_or_else(|| modes[0].id.clone())
        };
        let registry = Self {
            modes,
            by_id,
            auto_default,
        };
        // Validate @mode tool references (and cycles) once the registry exists.
        for spec in &registry.modes {
            registry.resolve_tools(&spec.id, &mut HashSet::new())?;
        }
        Ok(registry)
    }

    pub fn modes(&self) -> &[ModeSpec] {
        &self.modes
    }

    pub fn infos(&self) -> Vec<ModeInfo> {
        let mut ordered: Vec<(u32, ModeInfo)> = self
            .modes
            .iter()
            .map(|spec| (spec.order, spec.info()))
            .collect();
        ordered.sort_by(|left, right| left.0.cmp(&right.0));
        let mut infos: Vec<ModeInfo> = ordered.into_iter().map(|(_, info)| info).collect();
        infos.insert(
            0,
            ModeInfo {
                id: AUTO_ID.into(),
                label: "自动".into(),
                description: "route to the least-invasive mode that fits".into(),
                priority: 0,
            },
        );
        infos
    }

    pub fn spec(&self, id: &str) -> Option<&ModeSpec> {
        self.by_id.get(id).map(|index| &self.modes[*index])
    }

    pub fn auto_default(&self) -> &str {
        &self.auto_default
    }

    pub fn resolve(&self, requested: &str, goal: &str) -> Result<RouteDecision> {
        let requested = requested.trim();
        if requested.is_empty() || requested == AUTO_ID {
            return self.route(goal);
        }
        if self.spec(requested).is_none() {
            let available = self
                .modes
                .iter()
                .map(|spec| spec.id.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("unknown mode {requested}; available: {available}, auto");
        }
        Ok(RouteDecision {
            mode: requested.to_string(),
            reason: "explicit mode".into(),
            scores: Vec::new(),
        })
    }

    pub fn route(&self, goal: &str) -> Result<RouteDecision> {
        let lower = goal.to_lowercase();
        let mut scores = Vec::new();
        let mut best: Option<(usize, u32, usize)> = None;
        for (index, spec) in self.modes.iter().enumerate() {
            let mut hits = Vec::new();
            for keyword in &spec.keywords {
                let keyword = keyword.to_lowercase();
                if !keyword.is_empty() && lower.contains(&keyword) {
                    hits.push(keyword);
                }
            }
            if hits.is_empty() {
                continue;
            }
            let score = hits.len() * spec.priority.max(1) as usize;
            scores.push((spec.id.clone(), score));
            let candidate = (score, spec.priority, index);
            if best.is_none_or(|current| candidate > current) {
                best = Some(candidate);
            }
        }
        let (mode, reason) = match best {
            Some((_, _, index)) => {
                let spec = &self.modes[index];
                let matched = spec
                    .keywords
                    .iter()
                    .filter(|keyword| lower.contains(&keyword.to_lowercase()))
                    .map(|keyword| keyword.to_lowercase())
                    .collect::<Vec<_>>();
                (
                    spec.id.clone(),
                    format!("matched keywords: {}", matched.join(", ")),
                )
            }
            None => (
                self.auto_default.clone(),
                "no strong signal; using the configured default".into(),
            ),
        };
        Ok(RouteDecision {
            mode,
            reason,
            scores,
        })
    }

    pub fn tool_names(&self, id: &str) -> Vec<&'static str> {
        self.resolve_tools(id, &mut HashSet::new())
            .unwrap_or_default()
    }

    fn resolve_tools(&self, id: &str, visiting: &mut HashSet<String>) -> Result<Vec<&'static str>> {
        if !visiting.insert(id.to_string()) {
            bail!("mode {id} has a circular @mode tool reference");
        }
        let spec = self
            .spec(id)
            .with_context(|| format!("unknown mode {id}"))?;
        let mut selected = HashSet::new();
        for token in &spec.tools {
            if token == "*" {
                selected.extend(ALL_TOOLS.iter().copied());
            } else if let Some(reference) = token.strip_prefix('@') {
                for tool in self.resolve_tools(reference, visiting)? {
                    selected.insert(tool);
                }
            } else if let Some(tool) = ALL_TOOLS.iter().find(|tool| **tool == token) {
                selected.insert(*tool);
            } else {
                bail!("mode {} references unknown tool {}", id, token);
            }
        }
        visiting.remove(id);
        Ok(ALL_TOOLS
            .iter()
            .copied()
            .filter(|tool| selected.contains(tool))
            .collect())
    }

    pub fn can_spawn(&self, parent: &str, child: &str) -> bool {
        let Some(spec) = self.spec(parent) else {
            return false;
        };
        if self.spec(child).is_none() {
            return false;
        }
        spec.spawn
            .iter()
            .any(|entry| entry == "*" || entry == child)
    }

    pub fn write_capable(&self, id: &str) -> bool {
        let Some(spec) = self.spec(id) else {
            return false;
        };
        spec.write_capable.unwrap_or_else(|| {
            let tools = self.tool_names(id);
            tools.iter().any(|tool| WRITE_TOOLS.contains(tool))
        })
    }

    pub fn prompt(&self, id: &str) -> String {
        self.spec(id)
            .map(|spec| spec.prompt.clone())
            .unwrap_or_else(|| format!("MODE: {}.", id.to_uppercase()))
    }

    pub fn label(&self, id: &str) -> String {
        if id == AUTO_ID {
            return "自动".into();
        }
        self.spec(id)
            .map(|spec| spec.label.clone())
            .unwrap_or_else(|| id.to_string())
    }
}

fn tools_without(excluded: &[&str]) -> Vec<String> {
    ALL_TOOLS
        .iter()
        .filter(|tool| !excluded.contains(tool))
        .map(|tool| (*tool).to_string())
        .collect()
}

fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 40 {
        bail!("mode id must be 1..40 characters");
    }
    if !id.chars().all(|character| {
        character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || character == '-'
            || character == '_'
    }) {
        bail!("mode id {id:?} must use lowercase ASCII letters, digits, '-' or '_'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry_with(json: &str) -> ModeRegistry {
        let file: ModesFile = serde_json::from_str(json).unwrap();
        ModeRegistry::from_file(file).unwrap()
    }

    #[test]
    fn an_explicit_missing_modes_file_is_rejected() {
        let missing = std::env::temp_dir().join(format!(
            "msk-modes-missing-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&missing);
        assert!(ModeRegistry::load(Path::new("."), Some(&missing)).is_err());
    }

    #[test]
    fn builtins_are_valid_and_plan_is_stricter() {
        let registry = ModeRegistry::builtin().unwrap();
        assert!(registry.spec("plan").is_some());
        assert!(!registry.tool_names("plan").contains(&"exec"));
        assert!(registry.tool_names("plan").contains(&"read_file"));
        assert!(!registry.tool_names("investigate").contains(&"exec"));
        assert!(registry.tool_names("investigate").contains(&"net_scan"));
        assert_eq!(registry.tool_names("exploit").len(), ALL_TOOLS.len());
        assert!(!registry.write_capable("plan"));
        assert!(registry.write_capable("harden"));
    }

    #[test]
    fn auto_routes_by_keywords_and_priority() {
        let registry = ModeRegistry::builtin().unwrap();
        assert_eq!(
            registry
                .route("plan how to exploit the admin panel")
                .unwrap()
                .mode,
            "plan"
        );
        assert_eq!(
            registry
                .route("harden ssh and patch the firewall")
                .unwrap()
                .mode,
            "harden"
        );
        assert_eq!(
            registry
                .route("exploit the upload and get a shell")
                .unwrap()
                .mode,
            "exploit"
        );
        assert_eq!(registry.route("do the thing").unwrap().mode, "investigate");
    }

    #[test]
    fn config_can_override_and_add_modes() {
        let registry = registry_with(
            r#"{
              "auto": {"default": "osint"},
              "modes": [
                {"id": "plan", "label": "Plan", "prompt": "custom plan",
                 "tools": ["read_file", "file_type"], "spawn": []},
                {"id": "osint", "label": "OSINT", "description": "public sources",
                 "prompt": "MODE: OSINT.", "tools": ["net_dns", "net_tls", "web_request"],
                 "spawn": ["osint"], "keywords": ["osint", "public"], "priority": 2},
                {"id": "harden", "disabled": true}
              ]
            }"#,
        );
        assert_eq!(registry.label("plan"), "Plan");
        assert_eq!(registry.prompt("plan"), "custom plan");
        assert_eq!(registry.tool_names("plan"), vec!["read_file", "file_type"]);
        assert!(registry.spec("harden").is_none());
        assert_eq!(registry.auto_default(), "osint");
        assert_eq!(registry.route("do the thing").unwrap().mode, "osint");
        assert_eq!(registry.tool_names("osint").len(), 3);
        assert!(registry.can_spawn("osint", "osint"));
        assert!(!registry.can_spawn("osint", "plan"));
    }

    #[test]
    fn config_rejects_unknown_tools_and_modes() {
        let file: ModesFile = serde_json::from_str(
            r#"{"modes":[{"id":"bad","label":"Bad","tools":["no_such_tool"]}]}"#,
        )
        .unwrap();
        assert!(ModeRegistry::from_file(file).is_err());
        let file: ModesFile =
            serde_json::from_str(r#"{"modes":[{"id":"bad","label":"Bad","spawn":["missing"]}]}"#)
                .unwrap();
        assert!(ModeRegistry::from_file(file).is_err());
    }

    #[test]
    fn at_mode_references_reuse_tools() {
        let registry = registry_with(
            r#"{"modes":[{"id":"reporting","label":"Reporting","prompt":"report",
                 "tools":["@investigate","write_file","report_generate"],"spawn":[]}]}"#,
        );
        let tools = registry.tool_names("reporting");
        assert!(tools.contains(&"net_scan"));
        assert!(tools.contains(&"write_file"));
        assert!(!tools.contains(&"exec"));
    }

    #[test]
    fn extends_keeps_base_tools_and_overrides_selected_fields() {
        let registry = registry_with(
            r#"{
              "modes": [
                {"id": "plan", "label": "规划", "extends": "plan"},
                {"id": "osint", "label": "OSINT", "extends": "investigate",
                 "tools": ["net_dns", "net_tls"], "keywords": ["osint"]}
              ]
            }"#,
        );
        assert_eq!(registry.label("plan"), "规划");
        assert!(registry.tool_names("plan").contains(&"read_file"));
        assert!(!registry.tool_names("plan").contains(&"exec"));
        assert_eq!(registry.tool_names("osint"), vec!["net_dns", "net_tls"]);
        assert!(registry.can_spawn("osint", "investigate"));
        assert!(registry.prompt("osint").contains("INVESTIGATE"));
    }
}
