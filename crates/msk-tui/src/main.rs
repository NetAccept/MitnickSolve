use std::cell::Cell;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Stdout, Write, stdout};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event as CrosstermEvent, KeyCode, KeyEvent,
    KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use msk_events::{Event, read_events};
use msk_ipc::data::{DataClient, DataEvent};
use msk_ipc::{Client, methods};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Widget, Wrap};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use unicode_width::UnicodeWidthStr;

mod setup;

const MAX_EVENTS: usize = 5000;
const MAX_DETAIL_LINES: usize = 400;
const MAX_CHAT_LINES: usize = 4000;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const BG: Color = Color::Rgb(12, 12, 14);
const PANEL: Color = Color::Rgb(22, 22, 25);
const ACCENT: Color = Color::Rgb(96, 165, 250);
const TEXT: Color = Color::Rgb(228, 228, 231);
const DIM: Color = Color::Rgb(128, 128, 138);
const FAINT: Color = Color::Rgb(72, 72, 82);
const GREEN: Color = Color::Rgb(74, 222, 128);
const YELLOW: Color = Color::Rgb(250, 204, 21);
const RED: Color = Color::Rgb(248, 113, 113);

struct CommandEntry {
    label: &'static str,
    hint: &'static str,
    command: &'static str,
}

const COMMANDS: &[CommandEntry] = &[
    CommandEntry {
        label: "New session",
        hint: "start a fresh conversation (ctrl+n)",
        command: "/new",
    },
    CommandEntry {
        label: "Sessions",
        hint: "switch to another session (tab)",
        command: "/sessions",
    },
    CommandEntry {
        label: "Resume latest",
        hint: "restore the newest checkpoint",
        command: "/resume",
    },
    CommandEntry {
        label: "Providers",
        hint: "saved provider profiles and API keys",
        command: "/providers",
    },
    CommandEntry {
        label: "Provider and model",
        hint: "choose provider, model, effort, key",
        command: "/setup",
    },
    CommandEntry {
        label: "Model",
        hint: "pick a model preset",
        command: "/models",
    },
    CommandEntry {
        label: "Reasoning effort",
        hint: "minimal, low, medium, high",
        command: "/effort",
    },
    CommandEntry {
        label: "Agent mode",
        hint: "plan, investigate, exploit, harden, auto",
        command: "/mode",
    },
    CommandEntry {
        label: "Permission mode",
        hint: "auto, manual, bypass",
        command: "/permissions",
    },
    CommandEntry {
        label: "Offline demo",
        hint: "run a canned tool session",
        command: "/demo",
    },
    CommandEntry {
        label: "Terminal",
        hint: "open the PTY workspace",
        command: "/terminal",
    },
    CommandEntry {
        label: "Event inspector",
        hint: "show the raw event stream",
        command: "/events",
    },
    CommandEntry {
        label: "Pending approvals",
        hint: "check queued tool approvals",
        command: "/approvals",
    },
    CommandEntry {
        label: "Assessment findings",
        hint: "review severity-ranked findings",
        command: "/findings",
    },
    CommandEntry {
        label: "Clear transcript",
        hint: "hide messages in this view",
        command: "/clear",
    },
    CommandEntry {
        label: "Keyboard shortcuts",
        hint: "open the help overlay",
        command: "/help",
    },
    CommandEntry {
        label: "Quit",
        hint: "leave MitnickSolve",
        command: "/quit",
    },
];

/// Seven-row pixel glyphs for the home wordmark. `1` is an inked cell.
const WORDMARK: &[(&str, [&str; 7])] = &[
    (
        "M",
        [
            "10001", "11011", "10101", "10101", "10001", "10001", "10001",
        ],
    ),
    (
        "I",
        [
            "11111", "00100", "00100", "00100", "00100", "00100", "11111",
        ],
    ),
    (
        "T",
        [
            "11111", "00100", "00100", "00100", "00100", "00100", "00100",
        ],
    ),
    (
        "N",
        [
            "10001", "11001", "10101", "10101", "10011", "10001", "10001",
        ],
    ),
    (
        "I",
        [
            "11111", "00100", "00100", "00100", "00100", "00100", "11111",
        ],
    ),
    (
        "C",
        [
            "01111", "10000", "10000", "10000", "10000", "10000", "01111",
        ],
    ),
    (
        "K",
        [
            "10001", "10010", "10100", "11000", "10100", "10010", "10001",
        ],
    ),
    (
        "S",
        [
            "01111", "10000", "10000", "01110", "00001", "00001", "11110",
        ],
    ),
    (
        "O",
        [
            "01110", "10001", "10001", "10001", "10001", "10001", "01110",
        ],
    ),
    (
        "L",
        [
            "10000", "10000", "10000", "10000", "10000", "10000", "11111",
        ],
    ),
    (
        "V",
        [
            "10001", "10001", "10001", "10001", "10001", "01010", "00100",
        ],
    ),
    (
        "E",
        [
            "11111", "10000", "10000", "11110", "10000", "10000", "11111",
        ],
    ),
];

#[derive(Debug, Clone, Default)]
struct PaletteState {
    query: String,
    selected: usize,
}

const PROVIDERS: &[(&str, &str)] = &[
    (
        "scripted-demo",
        "offline canned tool session, no model or key",
    ),
    ("openai", "OpenAI-compatible Chat Completions"),
    ("openai-responses", "OpenAI Responses API function calls"),
    (
        "component",
        "signed llm.provider component configured on mskd",
    ),
];

const MODELS: &[&str] = &["gpt-5", "gpt-5-mini", "o3", "o4-mini", "custom…"];
const SERVICE_PRESETS: &[(&str, &str, &str, &str)] = &[
    ("openai", "OpenAI", "https://api.openai.com/v1", "gpt-5"),
    (
        "deepseek",
        "DeepSeek",
        "https://api.deepseek.com/v1",
        "deepseek-chat",
    ),
    (
        "openrouter",
        "OpenRouter",
        "https://openrouter.ai/api/v1",
        "openai/gpt-5",
    ),
    (
        "groq",
        "Groq",
        "https://api.groq.com/openai/v1",
        "llama-3.3-70b-versatile",
    ),
    ("together", "Together", "https://api.together.xyz/v1", ""),
    ("ollama", "Ollama (local)", "http://127.0.0.1:11434/v1", ""),
    (
        "lmstudio",
        "LM Studio (local)",
        "http://127.0.0.1:1234/v1",
        "",
    ),
    ("custom", "Custom endpoint", "", ""),
];
const PERMISSIONS: &[(&str, &str)] = &[
    (
        "auto",
        "自动：写入/扫描/HTTP 自动放行，命令和原始连接需审批",
    ),
    ("manual", "手动：所有受控工具都询问"),
    ("bypass", "放行：不弹审批；硬性策略拒绝仍然有效"),
];

#[derive(Debug, Clone, Deserialize)]
struct ModeInfo {
    id: String,
    #[serde(default)]
    label: String,
    #[serde(default)]
    description: String,
}

fn fallback_modes() -> Vec<ModeInfo> {
    [
        ("auto", "自动", "route to the least-invasive mode that fits"),
        ("plan", "计划", "read-only: produce a plan, execute nothing"),
        (
            "investigate",
            "调查",
            "read-only recon and evidence collection",
        ),
        ("exploit", "攻击", "authorized active testing, approvals on"),
        ("harden", "加固", "defensive remediation and verification"),
    ]
    .into_iter()
    .map(|(id, label, description)| ModeInfo {
        id: id.into(),
        label: label.into(),
        description: description.into(),
    })
    .collect()
}

const EFFORTS: &[(&str, &str)] = &[
    ("default", "let the provider decide"),
    ("minimal", "fastest / least reasoning"),
    ("low", "light reasoning"),
    ("medium", "balanced"),
    ("high", "deep reasoning"),
];

fn default_provider() -> String {
    "scripted-demo".into()
}

fn default_effort() -> String {
    "default".into()
}

fn default_mode() -> String {
    "auto".into()
}

fn default_permission() -> String {
    "auto".into()
}

fn permission_index(permission: &str) -> usize {
    PERMISSIONS
        .iter()
        .position(|(id, _)| *id == permission)
        .unwrap_or(0)
}

fn permission_label(permission: &str) -> &'static str {
    match permission {
        "manual" => "手动",
        "bypass" => "放行",
        _ => "自动",
    }
}

fn permission_color(permission: &str) -> Color {
    match permission {
        "manual" => YELLOW,
        "bypass" => RED,
        _ => GREEN,
    }
}

fn mode_color(mode: &str) -> Color {
    match mode {
        "plan" => ACCENT,
        "investigate" => GREEN,
        "exploit" | "attack" => RED,
        "harden" => YELLOW,
        "auto" => DIM,
        _ => Color::Rgb(167, 139, 250),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TuiConfig {
    #[serde(default = "default_provider")]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default = "default_effort")]
    effort: String,
    #[serde(default)]
    base_url: String,
    #[serde(default = "default_mode")]
    mode: String,
    #[serde(default = "default_permission")]
    permission: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    profiles: Vec<ProviderProfile>,
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            model: String::new(),
            effort: default_effort(),
            base_url: String::new(),
            mode: default_mode(),
            permission: default_permission(),
            api_key: String::new(),
            profiles: Vec::new(),
        }
    }
}

impl TuiConfig {
    fn load(root: &Path) -> Self {
        std::fs::read_to_string(root.join("tui-config.json"))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn save(&self, root: &Path) -> Result<()> {
        std::fs::create_dir_all(root)?;
        let path = root.join("tui-config.json");
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600));
        }
        #[cfg(windows)]
        {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::rename(&temporary, &path)?;
        Ok(())
    }
}

fn provider_index(provider: &str) -> usize {
    PROVIDERS
        .iter()
        .position(|(name, _)| *name == provider)
        .unwrap_or(0)
}

fn preset_index(base_url: &str) -> usize {
    let base_url = base_url.trim();
    if base_url.is_empty() {
        return 0;
    }
    SERVICE_PRESETS
        .iter()
        .position(|(_, _, url, _)| *url == base_url)
        .unwrap_or(SERVICE_PRESETS.len() - 1)
}

fn model_index(model: &str) -> usize {
    MODELS
        .iter()
        .position(|name| *name == model)
        .unwrap_or(MODELS.len() - 1)
}

fn effort_index(effort: &str) -> usize {
    EFFORTS
        .iter()
        .position(|(name, _)| *name == effort)
        .unwrap_or(0)
}

fn requires_key(provider: &str) -> bool {
    matches!(provider, "openai" | "openai-responses")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SetupStep {
    Provider,
    Preset,
    Model,
    CustomModel,
    Effort,
    BaseUrl,
    ApiKey,
}

#[derive(Debug, Clone)]
struct SetupState {
    step: SetupStep,
    provider: usize,
    preset: usize,
    model: usize,
    effort: usize,
    model_input: String,
    base_url: String,
    api_key: String,
    input_cursor: usize,
    /// Index of the saved profile being edited. `None` creates or updates one.
    edit_profile: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ProviderProfile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    provider: String,
    #[serde(default)]
    model: String,
    #[serde(default)]
    effort: String,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: String,
}

impl SetupState {
    fn from_app(app: &DaemonApp) -> Self {
        let model = if app.model.is_empty() {
            app.daemon_llm_model
                .clone()
                .or_else(|| std::env::var("MSK_LLM_MODEL").ok())
                .unwrap_or_default()
        } else {
            app.model.clone()
        };
        let base_url = if app.base_url.is_empty() {
            std::env::var("MSK_LLM_BASE_URL").unwrap_or_default()
        } else {
            app.base_url.clone()
        };
        let provider = if app.provider.is_empty() {
            default_provider()
        } else {
            app.provider.clone()
        };
        let model_index = model_index(&model);
        Self {
            step: SetupStep::Provider,
            provider: provider_index(&provider),
            preset: preset_index(&base_url),
            model: model_index,
            effort: effort_index(&app.effort),
            model_input: if model_index == MODELS.len() - 1 {
                model
            } else {
                String::new()
            },
            base_url,
            api_key: app.api_key.clone(),
            input_cursor: 0,
            edit_profile: None,
        }
    }

    fn from_profile(app: &DaemonApp, index: usize) -> Option<Self> {
        let profile = app.profiles.get(index)?;
        let effort = if profile.effort.is_empty() {
            default_effort()
        } else {
            profile.effort.clone()
        };
        let model_index = model_index(&profile.model);
        Some(Self {
            step: SetupStep::Provider,
            provider: provider_index(&profile.provider),
            preset: preset_index(&profile.base_url),
            model: model_index,
            effort: effort_index(&effort),
            model_input: if model_index == MODELS.len() - 1 {
                profile.model.clone()
            } else {
                String::new()
            },
            base_url: profile.base_url.clone(),
            api_key: profile.api_key.clone(),
            input_cursor: 0,
            edit_profile: Some(index),
        })
    }

    fn selected_provider(&self) -> &str {
        PROVIDERS[self.provider.min(PROVIDERS.len() - 1)].0
    }

    fn selected_model(&self) -> String {
        if self.model >= MODELS.len() - 1 {
            self.model_input.trim().to_string()
        } else {
            MODELS[self.model].to_string()
        }
    }
}

fn profile_name(provider: &str, model: &str) -> String {
    if model.trim().is_empty() {
        provider.to_string()
    } else {
        format!("{provider} · {}", model.trim())
    }
}

fn unique_profile_name(profiles: &[ProviderProfile], base: &str) -> String {
    unique_profile_name_excluding(profiles, base, None)
}

fn unique_profile_name_excluding(
    profiles: &[ProviderProfile],
    base: &str,
    except: Option<usize>,
) -> String {
    let taken = |name: &str| {
        profiles
            .iter()
            .enumerate()
            .any(|(index, profile)| Some(index) != except && profile.name == name)
    };
    if !taken(base) {
        return base.to_string();
    }
    for suffix in 2..1000 {
        let candidate = format!("{base} ({suffix})");
        if !taken(&candidate) {
            return candidate;
        }
    }
    base.to_string()
}

/// One bullet per character keeps the caret position exact while never
/// putting the key itself on screen.
fn masked_key(key: &str) -> String {
    "•".repeat(key.chars().count())
}

/// Pasted keys often carry a trailing newline or wrapping spaces from the
/// browser; neither belongs in the header value.
fn clean_key_paste(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

/// Turns an agent/provider failure into one readable transcript block. The
/// provider JSON is noisy; keep its message and code, not the whole body.
fn turn_error_text(status: &str, message: &str) -> String {
    let message = message.trim();
    let mut detail = message.to_string();
    if let Some(start) = message.find('{')
        && let Ok(value) = serde_json::from_str::<Value>(&message[start..])
        && let Some(error) = value.get("error")
    {
        let code = error.get("code").and_then(Value::as_str).unwrap_or("");
        let text = error.get("message").and_then(Value::as_str).unwrap_or("");
        if !text.is_empty() {
            detail = if code.is_empty() {
                text.to_string()
            } else {
                format!("{code}: {text}")
            };
        }
    }
    if detail.is_empty() {
        detail = status.to_string();
    }
    let mut text = format!("✗ {}", truncate(&detail, 400));
    if message.contains("LLM call failed")
        || message.contains("model_not_found")
        || message.contains("HTTP 4")
        || message.contains("HTTP 5")
    {
        text.push_str("\ncheck the model id and endpoint · ctrl+p providers");
    }
    text
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChatRole {
    User,
    Assistant,
    Tool,
    System,
}

#[derive(Debug, Clone)]
struct ChatLine {
    task_id: String,
    role: ChatRole,
    text: String,
    streaming: bool,
}

fn append_agent_text(chat: &mut Vec<ChatLine>, task_id: &str, text: &str, delta: bool) {
    if task_id.is_empty() || text.is_empty() {
        return;
    }
    if delta {
        if let Some(line) = chat.iter_mut().rev().find(|line| {
            line.task_id == task_id && line.role == ChatRole::Assistant && line.streaming
        }) {
            line.text.push_str(text);
        } else {
            chat.push(ChatLine {
                task_id: task_id.to_string(),
                role: ChatRole::Assistant,
                text: text.to_string(),
                streaming: true,
            });
        }
    } else {
        chat.push(ChatLine {
            task_id: task_id.to_string(),
            role: ChatRole::Assistant,
            text: text.to_string(),
            streaming: false,
        });
    }
}

fn finish_streaming(chat: &mut [ChatLine], task_id: &str) {
    if let Some(line) = chat
        .iter_mut()
        .rev()
        .find(|line| line.task_id == task_id && line.role == ChatRole::Assistant && line.streaming)
    {
        line.streaming = false;
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "msk-tui",
    version,
    about = "MitnickSolve interactive CTF agent"
)]
struct Cli {
    /// Read a JSONL event log instead of opening the conversational daemon.
    #[arg(long)]
    events: Option<PathBuf>,
    /// Daemon state directory. Defaults to .msk and auto-starts mskd if needed.
    #[arg(long, conflicts_with = "events")]
    connect: Option<PathBuf>,
    /// Daemon state directory used when --connect is not given.
    #[arg(long, default_value = ".msk")]
    root: PathBuf,
    /// Do not spawn mskd when no daemon is listening.
    #[arg(long)]
    no_autostart: bool,
    /// Provider for tasks created from this TUI (default: $MSK_PROVIDER or scripted).
    #[arg(long)]
    provider: Option<String>,
    /// Refresh interval in milliseconds.
    #[arg(long, default_value_t = 100)]
    refresh_ms: u64,
    /// Render one daemon frame to stdout and exit (no alternate screen).
    #[arg(long)]
    snapshot: bool,
    /// Open the provider/model/effort setup wizard on launch.
    #[arg(long)]
    setup: bool,
    /// Diagnostic: open the setup wizard at provider, service, model, effort, base, or key.
    #[arg(long, value_parser = ["provider", "service", "model", "effort", "base", "key"])]
    setup_step: Option<String>,
    /// Diagnostic: render the command palette in a snapshot.
    #[arg(long, hide = true)]
    snapshot_palette: bool,
    /// Diagnostic: render the session switcher in a snapshot.
    #[arg(long, hide = true)]
    snapshot_sessions: bool,
    /// Diagnostic: render the assessment findings overlay in a snapshot.
    #[arg(long, hide = true)]
    snapshot_findings: bool,
    /// Diagnostic: render the agent mode picker in a snapshot.
    #[arg(long, hide = true)]
    snapshot_modes: bool,
    /// Diagnostic: render the provider profile picker in a snapshot.
    #[arg(long, hide = true)]
    snapshot_providers: bool,
    /// Diagnostic: render the permission picker in a snapshot.
    #[arg(long, hide = true)]
    snapshot_permissions: bool,
    /// Print environment/daemon diagnostics and exit.
    #[arg(long)]
    doctor: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Llm,
    Tools,
    Artifacts,
    Context,
}

impl Filter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Llm => "llm",
            Self::Tools => "tools",
            Self::Artifacts => "artifacts",
            Self::Context => "context",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::All => Self::Llm,
            Self::Llm => Self::Tools,
            Self::Tools => Self::Artifacts,
            Self::Artifacts => Self::Context,
            Self::Context => Self::All,
        }
    }

    fn matches(self, event: &Event) -> bool {
        match self {
            Self::All => true,
            Self::Llm => event.kind.starts_with("llm.") || event.kind.starts_with("mcp."),
            Self::Tools => {
                event.kind.starts_with("tool.")
                    || event.kind.starts_with("wasm.")
                    || event.kind == "policy.decision"
                    || event.kind == "tool.registry"
            }
            Self::Artifacts => event.kind == "data.put" || event.kind.contains("artifact"),
            Self::Context => event.kind.starts_with("context.") || event.kind.starts_with("file."),
        }
    }
}

struct App {
    path: PathBuf,
    events: Vec<Event>,
    offset: u64,
    selected: usize,
    filter: Filter,
    search: String,
    searching: bool,
    follow: bool,
    show_help: bool,
    status: String,
}

impl App {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            events: Vec::new(),
            offset: 0,
            selected: 0,
            filter: Filter::All,
            search: String::new(),
            searching: false,
            follow: true,
            show_help: false,
            status: "waiting for events".into(),
        }
    }

    fn refresh(&mut self) -> Result<()> {
        let metadata = match std::fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                self.status = format!("waiting for {}", self.path.display());
                return Ok(());
            }
            Err(err) => return Err(err.into()),
        };

        if metadata.len() < self.offset {
            self.events.clear();
            self.offset = 0;
        }

        if metadata.len() > self.offset {
            let mut file =
                File::open(&self.path).with_context(|| format!("open {}", self.path.display()))?;
            file.seek(SeekFrom::Start(self.offset))?;
            let mut reader = BufReader::new(file);
            loop {
                let mut raw = Vec::new();
                let read = reader.read_until(b'\n', &mut raw)?;
                if read == 0 {
                    break;
                }
                // Leave a partially written line for the next refresh.
                if !raw.ends_with(b"\n") {
                    break;
                }
                self.offset += read as u64;
                if let Ok(line) = std::str::from_utf8(&raw) {
                    if let Ok(event) = serde_json::from_str::<Event>(line.trim()) {
                        self.events.push(event);
                    }
                }
            }
        }

        if self.events.len() > MAX_EVENTS {
            let excess = self.events.len() - MAX_EVENTS;
            self.events.drain(0..excess);
        }

        let visible = self.visible_indices().len();
        if self.follow && visible > 0 {
            self.selected = visible - 1;
        } else if self.selected >= visible {
            self.selected = visible.saturating_sub(1);
        }

        if let Some(event) = self.events.last() {
            self.status = format!("{}  {}", event.kind, event.ts);
        }
        Ok(())
    }

    fn visible_indices(&self) -> Vec<usize> {
        let needle = self.search.to_lowercase();
        self.events
            .iter()
            .enumerate()
            .filter(|(_, event)| {
                self.filter.matches(event)
                    && (needle.is_empty()
                        || event.kind.to_lowercase().contains(&needle)
                        || event.actor.to_lowercase().contains(&needle)
                        || serde_json::to_string(&event.data)
                            .map(|data| data.to_lowercase().contains(&needle))
                            .unwrap_or(false))
            })
            .map(|(index, _)| index)
            .collect()
    }

    fn stats(&self) -> String {
        let tools = self
            .events
            .iter()
            .filter(|event| event.kind.starts_with("tool.call"))
            .count();
        let artifacts = self
            .events
            .iter()
            .filter(|event| event.kind == "data.put")
            .count();
        let llm = self
            .events
            .iter()
            .filter(|event| event.kind == "llm.response")
            .count();
        let context = self
            .events
            .iter()
            .rev()
            .find(|event| event.kind == "context.state")
            .and_then(|event| event.data.get("approx_tokens"))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        format!(
            "events {}  tools {}  llm {}  artifacts {}  ctx {} tok",
            self.events.len(),
            tools,
            llm,
            artifacts,
            context
        )
    }

    fn selected_event(&self) -> Option<&Event> {
        let visible = self.visible_indices();
        visible
            .get(self.selected)
            .and_then(|index| self.events.get(*index))
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if self.searching {
            match key.code {
                KeyCode::Char(ch) => self.search.push(ch),
                KeyCode::Backspace => {
                    self.search.pop();
                }
                KeyCode::Enter => {
                    self.searching = false;
                    self.follow = true;
                }
                KeyCode::Esc => {
                    self.searching = false;
                    self.search.clear();
                }
                _ => {}
            }
            return false;
        }

        let visible = self.visible_indices().len();
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc => {
                self.search.clear();
            }
            KeyCode::Char('?') => self.show_help = !self.show_help,
            KeyCode::Char('/') => self.searching = true,
            KeyCode::Char('f') => {
                self.follow = !self.follow;
                if self.follow && visible > 0 {
                    self.selected = visible - 1;
                }
            }
            KeyCode::Char('r') => {
                self.events.clear();
                self.offset = 0;
                self.selected = 0;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if visible > 0 {
                    self.selected = (self.selected + 1).min(visible - 1);
                }
                self.follow = false;
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                self.follow = false;
            }
            KeyCode::Char('g') => {
                self.selected = 0;
                self.follow = false;
            }
            KeyCode::Char('G') => {
                if visible > 0 {
                    self.selected = visible - 1;
                }
                self.follow = true;
            }
            KeyCode::Tab => {
                self.filter = self.filter.next();
                self.selected = 0;
            }
            KeyCode::Char('1') => self.set_filter(Filter::All),
            KeyCode::Char('2') => self.set_filter(Filter::Llm),
            KeyCode::Char('3') => self.set_filter(Filter::Tools),
            KeyCode::Char('4') => self.set_filter(Filter::Artifacts),
            KeyCode::Char('5') => self.set_filter(Filter::Context),
            _ => {}
        }
        false
    }

    fn set_filter(&mut self, filter: Filter) {
        self.filter = filter;
        self.selected = 0;
        self.follow = true;
    }

    fn draw(&self, frame: &mut ratatui::Frame) {
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(frame.area());

        let header = Paragraph::new(format!(
            "MitnickSolve events  [{}{}]  {}\n{}",
            self.filter.label(),
            if self.follow { " follow" } else { "" },
            self.status,
            self.stats()
        ))
        .block(Block::default().borders(Borders::ALL).title("msk"));
        frame.render_widget(header, areas[0]);

        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(areas[1]);

        let visible = self.visible_indices();
        let items: Vec<ListItem> = visible
            .iter()
            .map(|index| {
                let event = &self.events[*index];
                ListItem::new(format!(
                    "{}  {:<20} {:<8} {}",
                    short_time(&event.ts),
                    event.kind,
                    event.actor,
                    summarize(event, 90)
                ))
            })
            .collect();

        let mut state = ListState::default();
        if !items.is_empty() {
            state.select(Some(self.selected.min(items.len() - 1)));
        }
        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!("events ({})", visible.len())),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("› ");
        frame.render_stateful_widget(list, panes[0], &mut state);

        let detail = match self.selected_event() {
            Some(event) => detail_text(event),
            None => "no event selected\n\n[r] reload  [f] follow  [/] search  [?] help".into(),
        };
        let detail = Paragraph::new(detail)
            .block(Block::default().borders(Borders::ALL).title("detail"))
            .wrap(Wrap { trim: false });
        frame.render_widget(detail, panes[1]);

        let footer = if self.searching {
            format!("search: {}_   [enter] apply  [esc] clear", self.search)
        } else {
            "q quit  j/k move  g/G top/bottom  tab filter  / search  f follow  r reload  ? help"
                .to_string()
        };
        frame.render_widget(Paragraph::new(footer), areas[2]);

        if self.show_help {
            let area = centered_rect(64, 56, frame.area());
            frame.render_widget(Clear, area);
            let help = Paragraph::new(
                "MitnickSolve conversational workbench

                 Enter / i  write a message, or describe a new challenge
                 Enter      send / start the task
                 Esc        leave the editor without sending
                 ← / →      move the input cursor
                 Ctrl+U     clear the input
                 PgUp/PgDn  scroll the conversation
                 v          toggle the raw event inspector
                 End        jump to the newest message
                 j / k      previous / next session
                 g / G      first / last session
                 n          start a new session
                 s          interrupt the running turn
                 x          cancel the selected task
                 t          open/focus the PTY terminal
                 Ctrl+Q     detach from terminal focus
                 y / n      approve / deny a pending request
                 ?          toggle this help
                 q          quit

                 Slash commands in the editor:
                 /new [goal]  /provider NAME  /terminal  /approvals
                 /interrupt  /cancel  /clear  /status  /help  /quit

                 Providers: scripted, scripted-demo, openai, responses, component.
                 ◆ marks an interactive task; tool calls appear inline in the conversation.",
            )
            .block(Block::default().borders(Borders::ALL).title("help"))
            .wrap(Wrap { trim: false });
            frame.render_widget(help, area);
        }
    }
}

struct TerminalView {
    session_id: String,
    parser: vt100::Parser,
    cols: u16,
    rows: u16,
}

impl TerminalView {
    fn new(session_id: String, cols: u16, rows: u16) -> Self {
        Self {
            session_id,
            parser: vt100::Parser::new(rows.max(1), cols.max(1), 2000),
            cols,
            rows,
        }
    }

    fn process(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    fn resize(&mut self, cols: u16, rows: u16) {
        self.parser.set_size(rows.max(1), cols.max(1));
        self.cols = cols;
        self.rows = rows;
    }

    fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }
}

struct DaemonApp {
    client: Client,
    root: PathBuf,
    tasks: Vec<Value>,
    selected_task: usize,
    events: Vec<(String, Event)>,
    chat: Vec<ChatLine>,
    approvals: Vec<Value>,
    composing: bool,
    input: String,
    input_cursor: usize,
    terminal: Option<TerminalView>,
    terminal_data: Option<DataClient>,
    terminal_size: Cell<(u16, u16)>,
    terminal_focus: bool,
    show_help: bool,
    status: String,
    refresh_ms: u64,
    last_refresh: Instant,
    provider: String,
    model: String,
    effort: String,
    base_url: String,
    api_key: String,
    profiles: Vec<ProviderProfile>,
    show_providers: bool,
    provider_selection: usize,
    provider_delete_armed: Option<usize>,
    mode: String,
    modes: Vec<ModeInfo>,
    permission: String,
    daemon_llm_key: bool,
    daemon_llm_model: Option<String>,
    daemon_workspace: Option<String>,
    setup: Option<SetupState>,
    new_task: bool,
    quit_requested: bool,
    pending_initial: Option<(String, String)>,
    scroll_from_bottom: u16,
    auto_scroll: bool,
    transcript_loaded: Option<String>,
    input_history: Vec<String>,
    history_index: Option<usize>,
    show_events: bool,
    show_sessions: bool,
    session_selection: usize,
    palette: Option<PaletteState>,
    spin: Cell<usize>,
    findings: Vec<Value>,
    show_findings: bool,
    findings_scroll: usize,
    show_modes: bool,
    mode_selection: usize,
    show_permissions: bool,
    permission_selection: usize,
}

impl DaemonApp {
    fn connect(root: PathBuf, refresh_ms: u64, provider: Option<String>) -> Result<Self> {
        let mut client =
            Client::connect(&root).with_context(|| format!("connect {}", root.display()))?;
        let hello = client.hello().context("kernel.hello")?;
        let daemon_workspace = hello
            .get("workspace")
            .and_then(Value::as_str)
            .map(str::to_string);
        let daemon_llm_key = hello
            .get("llm")
            .and_then(|llm| llm.get("api_key"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let daemon_llm_model = hello
            .get("llm")
            .and_then(|llm| llm.get("model"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let daemon_llm_effort = hello
            .get("llm")
            .and_then(|llm| llm.get("effort"))
            .and_then(Value::as_str)
            .map(str::to_string);
        let modes: Vec<ModeInfo> = hello
            .get("modes")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| serde_json::from_value::<ModeInfo>(item.clone()).ok())
                    .collect::<Vec<_>>()
            })
            .filter(|modes| !modes.is_empty())
            .unwrap_or_else(fallback_modes);
        client.subscribe(None)?;
        let configured = root.join("tui-config.json").exists();
        let mut config = TuiConfig::load(&root);
        if !modes.iter().any(|mode| mode.id == config.mode) {
            config.mode = default_mode();
        }
        if !PERMISSIONS.iter().any(|(id, _)| *id == config.permission) {
            config.permission = default_permission();
        }
        if let Some(provider) = provider {
            config.provider = provider;
        } else if !configured && daemon_llm_key {
            config.provider = "openai".into();
        }
        if requires_key(&config.provider) {
            if config.model.is_empty() {
                config.model = std::env::var("MSK_LLM_MODEL")
                    .ok()
                    .or_else(|| daemon_llm_model.clone())
                    .unwrap_or_default();
            }
            if config.effort.is_empty() {
                config.effort = daemon_llm_effort.clone().unwrap_or_else(default_effort);
            }
            if config.base_url.is_empty() {
                config.base_url = std::env::var("MSK_LLM_BASE_URL").unwrap_or_default();
            }
        } else {
            config.model.clear();
            config.effort = default_effort();
            config.base_url.clear();
        }
        if config.api_key.is_empty() {
            if let Some(profile) = config.profiles.iter().find(|profile| {
                profile.provider == config.provider
                    && profile.model == config.model
                    && profile.base_url == config.base_url
                    && !profile.api_key.is_empty()
            }) {
                config.api_key = profile.api_key.clone();
            }
        }
        let mut app = Self {
            client,
            root,
            tasks: Vec::new(),
            selected_task: 0,
            events: Vec::new(),
            chat: Vec::new(),
            approvals: Vec::new(),
            composing: false,
            input: String::new(),
            input_cursor: 0,
            terminal: None,
            terminal_data: None,
            terminal_size: Cell::new((0, 0)),
            terminal_focus: false,
            show_help: false,
            status: String::new(),
            refresh_ms,
            last_refresh: Instant::now(),
            provider: config.provider,
            model: config.model,
            effort: config.effort,
            base_url: config.base_url,
            api_key: config.api_key,
            profiles: config.profiles,
            show_providers: false,
            provider_selection: 0,
            provider_delete_armed: None,
            mode: config.mode,
            modes,
            permission: config.permission,
            daemon_llm_key,
            daemon_llm_model,
            daemon_workspace,
            setup: None,
            new_task: false,
            quit_requested: false,
            pending_initial: None,
            scroll_from_bottom: 0,
            auto_scroll: true,
            transcript_loaded: None,
            input_history: Vec::new(),
            history_index: None,
            show_events: false,
            show_sessions: false,
            session_selection: 0,
            palette: None,
            spin: Cell::new(0),
            findings: Vec::new(),
            show_findings: false,
            findings_scroll: 0,
            show_modes: false,
            mode_selection: 0,
            show_permissions: false,
            permission_selection: 0,
        };
        app.reload_tasks()?;
        let _ = app.reload_approvals();
        if !app.tasks.is_empty() {
            app.selected_task = app.tasks.len() - 1;
        }
        if let Some(task_id) = app.selected_task_id() {
            app.load_transcript(&task_id);
        } else if !configured {
            app.setup = Some(SetupState::from_app(&app));
            app.status = "welcome · pick a provider, or Esc to try the offline demo".into();
        } else {
            app.new_task = true;
            app.composing = true;
            app.status = String::new();
        }
        Ok(app)
    }

    fn reload_tasks(&mut self) -> Result<()> {
        let result = self.client.request_with_timeout(
            methods::TASK_LIST,
            json!({}),
            Duration::from_secs(1),
        )?;
        self.tasks = result
            .get("tasks")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        self.tasks.sort_by(|left, right| {
            left.get("task_id")
                .and_then(Value::as_str)
                .unwrap_or("")
                .cmp(right.get("task_id").and_then(Value::as_str).unwrap_or(""))
        });
        if self.selected_task >= self.tasks.len() {
            self.selected_task = self.tasks.len().saturating_sub(1);
        }
        Ok(())
    }

    fn reload_approvals(&mut self) -> Result<()> {
        let result = self.client.request_with_timeout(
            methods::APPROVAL_LIST,
            json!({}),
            Duration::from_secs(1),
        )?;
        self.approvals = result
            .get("approvals")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(())
    }

    fn save_config(&mut self) {
        let config = TuiConfig {
            provider: self.provider.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            base_url: self.base_url.clone(),
            mode: self.mode.clone(),
            permission: self.permission.clone(),
            api_key: self.api_key.clone(),
            profiles: self.profiles.clone(),
        };
        if let Err(err) = config.save(&self.root) {
            self.status = format!("save config failed: {err}");
        }
    }

    fn load_findings(&mut self) -> Result<()> {
        let result = self.client.request_with_timeout(
            methods::BLACKBOARD_LIST,
            json!({}),
            Duration::from_secs(5),
        )?;
        let mut findings: Vec<Value> = result
            .get("entries")
            .and_then(Value::as_array)
            .map(|entries| {
                entries
                    .iter()
                    .filter(|entry| {
                        entry
                            .get("key")
                            .and_then(Value::as_str)
                            .is_some_and(|key| key.starts_with("finding:"))
                    })
                    .filter_map(|entry| entry.get("value").cloned())
                    .filter(Value::is_object)
                    .collect()
            })
            .unwrap_or_default();
        findings.sort_by(|left, right| {
            let left_key = (
                finding_rank(left.get("severity").and_then(Value::as_str).unwrap_or("")),
                left.get("recorded_ms").and_then(Value::as_u64).unwrap_or(0),
            );
            let right_key = (
                finding_rank(right.get("severity").and_then(Value::as_str).unwrap_or("")),
                right
                    .get("recorded_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            );
            right_key.cmp(&left_key)
        });
        self.findings = findings;
        Ok(())
    }

    fn refresh(&mut self) -> Result<()> {
        while let Some(value) = self.client.try_notification() {
            match value.get("method").and_then(Value::as_str) {
                Some("event") => {
                    let params = value.get("params").cloned().unwrap_or(Value::Null);
                    let task_id = params
                        .get("task_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if let Some(event) = params.get("event") {
                        if let Ok(event) = serde_json::from_value::<Event>(event.clone()) {
                            let selected =
                                self.selected_task_id().as_deref() == Some(task_id.as_str());
                            if selected {
                                match event.kind.as_str() {
                                    "tool.call" => {
                                        let tool = event
                                            .data
                                            .get("tool")
                                            .and_then(Value::as_str)
                                            .unwrap_or("tool");
                                        let args =
                                            event.data.get("args").cloned().unwrap_or(Value::Null);
                                        self.chat.push(ChatLine {
                                            task_id: task_id.clone(),
                                            role: ChatRole::Tool,
                                            text: format!("▸ {tool} {}", compact_json(&args, 180)),
                                            streaming: false,
                                        });
                                    }
                                    "tool.result" => {
                                        let tool = event
                                            .data
                                            .get("tool")
                                            .and_then(Value::as_str)
                                            .unwrap_or("tool");
                                        let ok = !event
                                            .data
                                            .get("is_error")
                                            .and_then(Value::as_bool)
                                            .unwrap_or(false);
                                        let mut text =
                                            format!("{} {tool}", if ok { "✓" } else { "✗" });
                                        if let Some(summary) = observation_summary(&event.data) {
                                            text.push_str(&format!("  {summary}"));
                                        }
                                        self.chat.push(ChatLine {
                                            task_id: task_id.clone(),
                                            role: ChatRole::Tool,
                                            text,
                                            streaming: false,
                                        });
                                    }
                                    _ => {}
                                }
                                self.trim_chat();
                            }
                            self.events.push((task_id, event));
                        }
                    }
                }
                Some("task.state") => {
                    if let Some(snapshot) = value.get("params") {
                        let task_id = snapshot
                            .get("task_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let state = snapshot
                            .get("state")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let finished = matches!(state, "failed" | "done" | "cancelled");
                        if let Some(existing) = self.tasks.iter_mut().find(|task| {
                            task.get("task_id").and_then(Value::as_str) == Some(task_id)
                        }) {
                            *existing = snapshot.clone();
                        }
                        if finished {
                            finish_streaming(&mut self.chat, task_id);
                            self.approvals.retain(|approval| {
                                approval.get("task_id").and_then(Value::as_str) != Some(task_id)
                            });
                            if self.selected_task_id().as_deref() == Some(task_id) {
                                let task_id = task_id.to_string();
                                self.load_transcript(&task_id);
                            }
                        }
                        if state == "waiting_input"
                            && let Some((pending_task, text)) = self.pending_initial.clone()
                            && pending_task == task_id
                        {
                            match self.client.request(
                                methods::TASK_MESSAGE,
                                json!({"task_id": task_id, "text": text}),
                            ) {
                                Ok(_) => {
                                    self.pending_initial = None;
                                    self.status.clear();
                                }
                                Err(err) => {
                                    self.status = format!("initial prompt failed: {err}");
                                }
                            }
                        }
                    }
                }
                Some("approval.request") => {
                    if let Some(pending) = value.get("params") {
                        let duplicate = self.approvals.iter().any(|approval| {
                            approval.get("task_id") == pending.get("task_id")
                                && approval.get("id") == pending.get("id")
                        });
                        if !duplicate {
                            self.approvals.push(pending.clone());
                            self.status = "approval requested".into();
                        }
                    }
                }
                Some("approval.resolved") => {
                    if let Some(resolved) = value.get("params") {
                        self.approvals.retain(|approval| {
                            approval.get("task_id") != resolved.get("task_id")
                                || approval.get("id") != resolved.get("id")
                        });
                    }
                }
                Some("task.message") => {
                    let params = value.get("params").cloned().unwrap_or(Value::Null);
                    let task_id = params
                        .get("task_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let text = params.get("text").and_then(Value::as_str).unwrap_or("");
                    if !task_id.is_empty() && !text.is_empty() {
                        self.chat.push(ChatLine {
                            task_id,
                            role: ChatRole::User,
                            text: text.to_string(),
                            streaming: false,
                        });
                        self.trim_chat();
                    }
                }
                Some("agent.text") => {
                    let params = value.get("params").cloned().unwrap_or(Value::Null);
                    let task_id = params
                        .get("task_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let text = params.get("text").and_then(Value::as_str).unwrap_or("");
                    let delta = params
                        .get("delta")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    append_agent_text(&mut self.chat, &task_id, text, delta);
                    self.trim_chat();
                }
                Some("turn.end") => {
                    let params = value.get("params").cloned().unwrap_or(Value::Null);
                    let task_id = params
                        .get("task_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string();
                    let status = params
                        .get("status")
                        .and_then(Value::as_str)
                        .unwrap_or("awaiting_input");
                    finish_streaming(&mut self.chat, &task_id);
                    self.approvals.retain(|approval| {
                        approval.get("task_id").and_then(Value::as_str) != Some(task_id.as_str())
                    });
                    if status != "awaiting_input" {
                        let message = params
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        self.status = if message.is_empty() {
                            format!("last turn {status}")
                        } else {
                            format!("last turn {status}: {}", truncate(message, 160))
                        };
                    }
                    if self.selected_task_id().as_deref() == Some(task_id.as_str()) {
                        self.load_transcript(&task_id);
                    }
                }
                _ => {}
            }
        }

        if self.events.len() > MAX_EVENTS {
            let excess = self.events.len() - MAX_EVENTS;
            self.events.drain(0..excess);
        }

        if let Some(data) = self.terminal_data.take() {
            let mut closed = false;
            let mut credits = 0usize;
            while let Some(event) = data.try_event() {
                match event {
                    DataEvent::Data(bytes) => {
                        if let Some(terminal) = self.terminal.as_mut() {
                            terminal.process(&bytes);
                        }
                        credits += bytes.len();
                    }
                    DataEvent::Exit | DataEvent::Closed => {
                        self.status = "terminal exited".into();
                        closed = true;
                        break;
                    }
                    DataEvent::Error(message) => {
                        self.status = format!("terminal error: {message}");
                    }
                }
            }
            if credits > 0 {
                let _ = data.credit(credits);
            }
            if closed {
                self.terminal = None;
                self.terminal_focus = false;
            } else {
                self.terminal_data = Some(data);
            }
        }

        if self.terminal_focus {
            let (cols, rows) = self.terminal_size.get();
            if cols > 0 && rows > 0 {
                if let Some(terminal) = self.terminal.as_mut() {
                    if terminal.cols != cols || terminal.rows != rows {
                        terminal.resize(cols, rows);
                        if let Some(data) = self.terminal_data.as_ref() {
                            let _ = data.resize(cols, rows);
                        }
                    }
                }
            }
        }

        if self.last_refresh.elapsed() >= Duration::from_secs(1) {
            if let Err(err) = self.reload_tasks() {
                self.status = format!("refresh failed: {err}");
            }
            if let Err(err) = self.reload_approvals() {
                self.status = format!("approval refresh failed: {err}");
            }
            self.last_refresh = Instant::now();
        }
        self.flush_pending_initial();
        Ok(())
    }

    fn append_history_event(&mut self, task_id: &str, event: &Event) {
        let data = &event.data;
        match event.kind.as_str() {
            "turn.start" => {
                if let Some(user) = data.get("user").and_then(Value::as_str) {
                    self.chat.push(ChatLine {
                        task_id: task_id.to_string(),
                        role: ChatRole::User,
                        text: user.to_string(),
                        streaming: false,
                    });
                }
            }
            "llm.response" => {
                if let Some(text) = data.get("text").and_then(Value::as_str)
                    && !text.is_empty()
                {
                    self.chat.push(ChatLine {
                        task_id: task_id.to_string(),
                        role: ChatRole::Assistant,
                        text: text.to_string(),
                        streaming: false,
                    });
                }
            }
            "tool.call" => {
                let tool = data.get("tool").and_then(Value::as_str).unwrap_or("tool");
                let args = data.get("args").cloned().unwrap_or(Value::Null);
                self.chat.push(ChatLine {
                    task_id: task_id.to_string(),
                    role: ChatRole::Tool,
                    text: format!("▸ {tool} {}", compact_json(&args, 180)),
                    streaming: false,
                });
            }
            "tool.result" => {
                let tool = data.get("tool").and_then(Value::as_str).unwrap_or("tool");
                let failed = data
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut text = format!("{} {tool}", if failed { "✗" } else { "✓" });
                if let Some(artifact) = data.get("artifact").and_then(Value::as_str) {
                    text.push_str(&format!("  artifact {artifact}"));
                } else if let Some(summary) = observation_summary(data) {
                    text.push_str(&format!("  {summary}"));
                }
                self.chat.push(ChatLine {
                    task_id: task_id.to_string(),
                    role: ChatRole::Tool,
                    text,
                    streaming: false,
                });
            }
            "turn.end" => {
                let status = data.get("status").and_then(Value::as_str).unwrap_or("done");
                if status != "awaiting_input" {
                    let message = data
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    self.chat.push(ChatLine {
                        task_id: task_id.to_string(),
                        role: ChatRole::System,
                        text: turn_error_text(status, message),
                        streaming: false,
                    });
                }
            }
            "approval.request" => {
                let tool = data.get("tool").and_then(Value::as_str).unwrap_or("tool");
                self.chat.push(ChatLine {
                    task_id: task_id.to_string(),
                    role: ChatRole::System,
                    text: format!("approval requested: {tool}"),
                    streaming: false,
                });
            }
            _ => {}
        }
    }

    fn load_transcript(&mut self, task_id: &str) {
        let result = match self.client.request_with_timeout(
            methods::TASK_HISTORY,
            json!({"task_id": task_id}),
            Duration::from_secs(5),
        ) {
            Ok(result) => result,
            Err(_) => match self.client.request_with_timeout(
                methods::TASK_RESULT,
                json!({"task_id": task_id}),
                Duration::from_secs(5),
            ) {
                Ok(result) => result,
                Err(err) => {
                    self.status = format!("load transcript failed: {err}");
                    return;
                }
            },
        };
        self.chat.retain(|line| line.task_id != task_id);
        if let Some(history) = result.get("history").and_then(Value::as_array) {
            for event in history {
                if let Ok(event) = serde_json::from_value::<Event>(event.clone()) {
                    self.append_history_event(task_id, &event);
                }
            }
            self.trim_chat();
            self.transcript_loaded = Some(task_id.to_string());
            self.auto_scroll = true;
            self.scroll_from_bottom = 0;
            return;
        }
        let transcript = result
            .get("transcript")
            .and_then(Value::as_str)
            .unwrap_or_default();
        for raw in transcript.lines() {
            let raw = raw.trim_end();
            if raw.trim().is_empty() {
                continue;
            }
            let (role, text) = if raw.starts_with("[turn") {
                if let Some((_, text)) = raw.split_once("] user: ") {
                    (ChatRole::User, text.to_string())
                } else {
                    (ChatRole::System, raw.to_string())
                }
            } else if let Some(text) = raw.strip_prefix("assistant: ") {
                (ChatRole::Assistant, text.to_string())
            } else if raw.starts_with("tool ") {
                (ChatRole::Tool, raw.to_string())
            } else {
                (ChatRole::System, raw.to_string())
            };
            self.chat.push(ChatLine {
                task_id: task_id.to_string(),
                role,
                text,
                streaming: false,
            });
        }
        self.trim_chat();
        self.transcript_loaded = Some(task_id.to_string());
        self.auto_scroll = true;
        self.scroll_from_bottom = 0;
    }

    fn open_sessions(&mut self) {
        self.composing = false;
        self.show_sessions = true;
        self.session_selection = if self.new_task || self.tasks.is_empty() {
            0
        } else {
            (self.selected_task + 1).min(self.tasks.len())
        };
        self.status = "sessions · ↑/↓ select · Enter open · n new · Esc close".into();
    }

    fn toggle_sessions(&mut self) {
        if self.show_sessions {
            self.show_sessions = false;
            self.status.clear();
        } else {
            self.open_sessions();
        }
    }

    fn move_session_selection(&mut self, delta: isize) {
        let rows = self.tasks.len() + 1;
        let current = self.session_selection.min(rows - 1) as isize;
        self.session_selection = (current + delta).clamp(0, rows as isize - 1) as usize;
    }

    fn start_new_session(&mut self) {
        self.show_sessions = false;
        self.new_task = true;
        self.composing = true;
        self.status = if self.mode.is_empty() {
            "new session · describe the challenge".into()
        } else {
            format!(
                "new session · mode {} · permission {} · send a message",
                self.mode, self.permission
            )
        };
    }

    fn open_session_at(&mut self, index: usize) {
        if self.tasks.is_empty() {
            return;
        }
        self.selected_task = index.min(self.tasks.len() - 1);
        self.new_task = false;
        self.show_sessions = false;
        self.status.clear();
        if let Some(task_id) = self.selected_task_id() {
            self.load_transcript(&task_id);
        }
    }

    fn flush_pending_initial(&mut self) {
        let Some((task_id, text)) = self.pending_initial.clone() else {
            return;
        };
        let waiting = self.tasks.iter().any(|task| {
            task.get("task_id").and_then(Value::as_str) == Some(task_id.as_str())
                && task.get("state").and_then(Value::as_str) == Some("waiting_input")
        });
        if !waiting {
            return;
        }
        match self.client.request(
            methods::TASK_MESSAGE,
            json!({"task_id": task_id, "text": text}),
        ) {
            Ok(_) => {
                self.pending_initial = None;
                self.status.clear();
            }
            Err(err) => {
                self.status = format!("initial prompt failed: {err}");
            }
        }
    }

    fn selected_task_id(&self) -> Option<String> {
        self.tasks
            .get(self.selected_task)
            .and_then(|task| task.get("task_id"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    fn selected_last_error(&self) -> Option<String> {
        self.tasks
            .get(self.selected_task)
            .and_then(|task| task.get("last_error"))
            .and_then(Value::as_str)
            .filter(|error| !error.trim().is_empty())
            .map(str::to_string)
    }

    fn selected_events(&self) -> Vec<usize> {
        let task_id = self.selected_task_id();
        self.events
            .iter()
            .enumerate()
            .filter(|(_, (id, _))| Some(id.as_str()) == task_id.as_deref())
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_task_title(&self) -> String {
        self.tasks
            .get(self.selected_task)
            .and_then(|task| task.get("goal"))
            .and_then(Value::as_str)
            .map(|goal| truncate(goal, 64))
            .unwrap_or_else(|| "new session".into())
    }

    fn selected_state(&self) -> &str {
        self.tasks
            .get(self.selected_task)
            .and_then(|task| task.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("?")
    }

    fn selected_mode(&self) -> String {
        self.tasks
            .get(self.selected_task)
            .and_then(|task| task.get("mode"))
            .and_then(Value::as_str)
            .unwrap_or(&self.mode)
            .to_string()
    }

    fn display_mode(&self) -> String {
        if self.new_task || self.selected_task_id().is_none() {
            self.mode.clone()
        } else {
            self.selected_mode()
        }
    }

    /// Settings changes are for the next session. They must never silently
    /// replace the session the user is reading.
    fn announce_provider_saved(&mut self) {
        if self.selected_task_id().is_some() && !self.new_task {
            self.status = format!(
                "provider for next session: {} · current stays {} · ctrl+n starts it",
                self.provider,
                self.display_provider()
            );
        } else {
            self.status = format!("next session provider: {}", self.provider);
        }
    }

    fn announce_mode_saved(&mut self) {
        if self.new_task || self.selected_task_id().is_none() {
            self.status = format!("next session mode: {}", self.mode);
        } else {
            self.status = format!(
                "mode for next session: {} · current stays {} · ctrl+n starts it",
                self.mode,
                self.selected_mode()
            );
        }
    }

    fn announce_permission_saved(&mut self) {
        if self.new_task || self.selected_task_id().is_none() {
            self.status = format!("next session permission: {}", self.permission);
        } else {
            self.status = format!(
                "permission for next session: {} · current stays {} · ctrl+n starts it",
                self.permission,
                self.display_permission()
            );
        }
    }

    fn display_permission(&self) -> String {
        if self.new_task || self.selected_task_id().is_none() {
            self.permission.clone()
        } else {
            self.tasks
                .get(self.selected_task)
                .and_then(|task| task.get("permission"))
                .and_then(Value::as_str)
                .unwrap_or(&self.permission)
                .to_string()
        }
    }

    fn display_task_field(&self, field: &str, fallback: &str) -> String {
        if self.new_task || self.selected_task_id().is_none() {
            fallback.to_string()
        } else {
            self.tasks
                .get(self.selected_task)
                .and_then(|task| task.get(field))
                .and_then(Value::as_str)
                .unwrap_or(fallback)
                .to_string()
        }
    }

    fn display_provider(&self) -> String {
        self.display_task_field("provider", &self.provider)
    }

    fn display_model(&self) -> String {
        self.display_task_field("model", &self.model)
    }

    fn display_effort(&self) -> String {
        let effort = self.display_task_field("effort", &self.effort);
        if effort.is_empty() {
            default_effort()
        } else {
            effort
        }
    }

    fn mode_index(&self, mode: &str) -> usize {
        self.modes
            .iter()
            .position(|entry| entry.id == mode)
            .unwrap_or(0)
    }

    fn mode_label(&self, mode: &str) -> String {
        self.modes
            .iter()
            .find(|entry| entry.id == mode)
            .map(|entry| entry.label.clone())
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| mode.to_string())
    }

    fn find_mode(&self, value: &str) -> Option<String> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        self.modes
            .iter()
            .find(|entry| {
                entry.id.eq_ignore_ascii_case(value) || entry.label.eq_ignore_ascii_case(value)
            })
            .map(|entry| entry.id.clone())
    }

    fn trim_chat(&mut self) {
        if self.chat.len() > MAX_CHAT_LINES {
            let excess = self.chat.len() - MAX_CHAT_LINES;
            self.chat.drain(0..excess);
        }
    }

    fn enter_compose(&mut self) {
        self.composing = true;
        if self.selected_task_id().is_none() {
            self.new_task = true;
        }
    }

    fn send_message(&mut self) {
        let text = self.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        if text.starts_with('/') {
            self.run_command(&text);
            return;
        }
        if self.new_task || self.selected_task_id().is_none() {
            self.start_new_task(&text);
            return;
        }
        let Some(task_id) = self.selected_task_id() else {
            self.status = "no task selected".into();
            return;
        };
        match self.client.request(
            methods::TASK_MESSAGE,
            json!({"task_id": task_id, "text": text}),
        ) {
            Ok(_) => {
                self.remember_history(&text);
                self.input.clear();
                self.input_cursor = 0;
                self.status.clear();
            }
            Err(err) => self.status = format!("send failed: {err}"),
        }
    }

    fn run_command(&mut self, text: &str) {
        let mut parts = text.trim().splitn(2, char::is_whitespace);
        let command = parts.next().unwrap_or_default();
        let argument = parts.next().unwrap_or_default().trim();
        self.input.clear();
        self.input_cursor = 0;
        match command {
            "/help" => {
                self.show_help = true;
                self.status = "help".into();
            }
            "/setup" => self.open_setup(),
            "/providers" => self.open_providers(),
            "/resume" => self.resume_latest(),
            "/sessions" => self.toggle_sessions(),
            "/models" | "/model" if argument.is_empty() => self.open_setup_at(SetupStep::Model),
            "/model" => {
                self.model = argument.to_string();
                self.status = format!("next session model: {}", self.model);
            }
            "/effort" if argument.is_empty() => self.open_setup_at(SetupStep::Effort),
            "/effort" => {
                let effort = if argument.is_empty() {
                    "default"
                } else {
                    argument
                };
                if matches!(effort, "default" | "minimal" | "low" | "medium" | "high") {
                    self.effort = effort.to_string();
                    self.save_config();
                    self.status = format!("next session effort: {}", self.effort);
                } else {
                    self.status = "usage: /effort default|minimal|low|medium|high".into();
                }
            }
            "/mode" if argument.is_empty() => {
                self.mode_selection = self.mode_index(&self.mode);
                self.show_modes = true;
                self.status = "agent mode · ↑/↓ choose · Enter apply · Esc cancel".into();
            }
            "/mode" => {
                if let Some(mode) = self.find_mode(argument) {
                    self.mode = mode;
                    self.save_config();
                    self.announce_mode_saved();
                } else {
                    self.status = "unknown mode; open /mode to list configured modes".into();
                }
            }
            "/permissions" if argument.is_empty() => {
                self.permission_selection = permission_index(&self.permission);
                self.show_permissions = true;
                self.status = "permissions · ↑/↓ choose · Enter apply · Esc cancel".into();
            }
            "/permissions" => {
                let value = argument.trim();
                let found = PERMISSIONS.iter().find(|(id, _)| {
                    id.eq_ignore_ascii_case(value) || permission_label(id) == value
                });
                if let Some((id, _)) = found {
                    self.permission = (*id).to_string();
                    self.save_config();
                    self.announce_permission_saved();
                } else {
                    self.status = "usage: /permissions auto|manual|bypass".into();
                }
            }
            "/demo" => {
                self.provider = "scripted-demo".into();
                self.create_task_with_goal("run the offline tool demo", true);
            }
            "/new" => {
                if argument.is_empty() {
                    self.start_new_session();
                } else {
                    self.start_new_task(argument);
                }
            }
            "/quit" | "/exit" => {
                self.quit_requested = true;
            }
            "/cancel" => self.cancel_task(),
            "/interrupt" => self.interrupt_task(),
            "/terminal" => self.toggle_terminal(),
            "/events" => {
                self.show_events = !self.show_events;
                self.status = if self.show_events {
                    "event inspector on".into()
                } else {
                    String::new()
                };
            }
            "/approvals" => {
                self.status = if self.approvals.is_empty() {
                    "no pending approvals".into()
                } else {
                    format!("{} pending approval(s)", self.approvals.len())
                };
            }
            "/findings" => match self.load_findings() {
                Ok(()) => {
                    self.show_findings = true;
                    self.findings_scroll = 0;
                    self.status = format!(
                        "{} finding(s) · j/k scroll · r reload · Esc close",
                        self.findings.len()
                    );
                }
                Err(err) => self.status = format!("findings failed: {err}"),
            },
            "/provider" => {
                let canonical = match argument {
                    "responses" => "openai-responses",
                    value => value,
                };
                if matches!(
                    canonical,
                    "scripted" | "scripted-demo" | "openai" | "openai-responses" | "component"
                ) {
                    self.provider = canonical.to_string();
                    self.save_config();
                    self.announce_provider_saved();
                } else {
                    self.status =
                        "usage: /provider scripted|scripted-demo|openai|openai-responses|component"
                            .into();
                }
            }
            "/clear" => {
                if let Some(task_id) = self.selected_task_id() {
                    self.chat.retain(|line| line.task_id != task_id);
                }
                self.status = "conversation cleared".into();
            }
            "/status" => {
                self.status = format!(
                    "root {} · provider {} · task {} · {}",
                    self.root.display(),
                    self.provider,
                    self.selected_task_id().as_deref().unwrap_or("-"),
                    self.selected_state()
                );
            }
            other => {
                self.status = format!("unknown command {other}; try /help");
            }
        }
    }

    fn start_new_task(&mut self, goal: &str) {
        self.create_task_with_goal(goal, true);
    }

    fn create_task_with_goal(&mut self, goal: &str, send_initial: bool) {
        self.create_task_with_options(goal, send_initial, None);
    }

    fn resume_latest(&mut self) {
        let tasks_dir = self.root.join("tasks");
        let mut latest: Option<(std::time::SystemTime, PathBuf, u64)> = None;
        if let Ok(entries) = std::fs::read_dir(&tasks_dir) {
            for entry in entries.flatten() {
                let checkpoint = entry.path().join("checkpoint.json");
                let Ok(metadata) = std::fs::metadata(&checkpoint) else {
                    continue;
                };
                let Ok(modified) = metadata.modified() else {
                    continue;
                };
                let turns = std::fs::read(&checkpoint)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .and_then(|value| value.get("turns").and_then(Value::as_u64))
                    .unwrap_or(0);
                if latest.as_ref().is_none_or(|(time, _, _)| modified > *time) {
                    latest = Some((modified, checkpoint, turns));
                }
            }
        }
        let Some((_, checkpoint, turns)) = latest else {
            self.status = "no checkpoint to resume; send a message to start a session".into();
            return;
        };
        self.status = format!("resuming checkpoint with {} turns", turns);
        self.create_task_with_options(
            &format!("Resumed session ({turns} turns)"),
            false,
            Some(&checkpoint),
        );
    }

    fn create_task_with_options(
        &mut self,
        goal: &str,
        send_initial: bool,
        resume_checkpoint: Option<&Path>,
    ) {
        if requires_key(&self.provider) {
            if !self.has_llm_key() {
                self.status = "missing API key · Ctrl+P → Providers".into();
                return;
            }
            if self.model.trim().is_empty() && self.daemon_llm_model.is_none() {
                self.status = "provider needs a model; use Ctrl+P → Model".into();
                self.open_setup();
                return;
            }
        }
        let result = self.client.request(methods::TASK_CREATE, {
            let mut params = json!({
                "goal": goal,
                "demo": "session",
                "provider": self.provider,
                "mode": self.mode,
                "permission": self.permission,
                "interactive": true,
                "stream": true,
                "approvals": "ask"
            });
            if !self.model.trim().is_empty() {
                params["model"] = json!(self.model.trim());
            }
            if self.effort != "default" && !self.effort.is_empty() {
                params["effort"] = json!(self.effort);
            }
            if !self.base_url.trim().is_empty() {
                params["base_url"] = json!(self.base_url.trim());
            }
            if requires_key(&self.provider) && !self.api_key.trim().is_empty() {
                params["api_key"] = json!(self.api_key.trim());
            }
            if let Some(checkpoint) = resume_checkpoint {
                params["resume_checkpoint"] = json!(checkpoint.display().to_string());
            }
            params
        });
        match result {
            Ok(snapshot) => {
                let task_id = snapshot
                    .get("task_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                self.new_task = false;
                if send_initial {
                    self.remember_history(goal);
                }
                self.input.clear();
                self.input_cursor = 0;
                self.pending_initial = if send_initial && !task_id.is_empty() {
                    Some((task_id.clone(), goal.to_string()))
                } else {
                    None
                };
                self.status = if resume_checkpoint.is_some() {
                    format!("session resumed · {}", self.provider)
                } else {
                    format!("session started · {}", self.provider)
                };
                if let Err(err) = self.reload_tasks() {
                    self.status = format!("reload failed: {err}");
                }
                self.selected_task = self.tasks.len().saturating_sub(1);
                if !task_id.is_empty() {
                    self.load_transcript(&task_id);
                }
            }
            Err(err) => self.status = format!("create failed: {err}"),
        }
    }

    fn interrupt_task(&mut self) {
        let Some(task_id) = self.selected_task_id() else {
            self.status = "no task selected".into();
            return;
        };
        match self
            .client
            .request(methods::TASK_INTERRUPT, json!({"task_id": task_id}))
        {
            Ok(snapshot) => {
                if snapshot
                    .get("interrupted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    self.status = "interrupt sent".into();
                } else {
                    self.status = "not running a turn".into();
                }
            }
            Err(err) => self.status = format!("interrupt failed: {err}"),
        }
    }

    fn respond_approval(&mut self, approved: bool) {
        let Some(pending) = self.approvals.first().cloned() else {
            return;
        };
        let task_id = pending
            .get("task_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let id = pending.get("id").and_then(Value::as_u64).unwrap_or(0);
        match self.client.request(
            methods::APPROVAL_RESPOND,
            json!({"task_id": task_id, "id": id, "approved": approved}),
        ) {
            Ok(_) => {
                self.approvals.retain(|approval| {
                    approval.get("task_id").and_then(Value::as_str) != Some(task_id.as_str())
                        || approval.get("id").and_then(Value::as_u64) != Some(id)
                });
                self.status = if approved {
                    "approved tool call".into()
                } else {
                    "denied tool call".into()
                };
            }
            Err(err) => self.status = format!("approval response failed: {err}"),
        }
    }

    fn remember_history(&mut self, text: &str) {
        if text.is_empty() || text.starts_with('/') {
            return;
        }
        if self.input_history.last().map(String::as_str) != Some(text) {
            self.input_history.push(text.to_string());
            if self.input_history.len() > 100 {
                self.input_history.remove(0);
            }
        }
        self.history_index = None;
    }

    fn history_previous(&mut self) {
        if self.input_history.is_empty() {
            return;
        }
        let index = match self.history_index {
            Some(index) => index.saturating_sub(1),
            None => self.input_history.len() - 1,
        };
        self.history_index = Some(index);
        self.input = self.input_history[index].clone();
        self.input_cursor = self.input.len();
    }

    fn history_next(&mut self) {
        let Some(index) = self.history_index else {
            return;
        };
        if index + 1 >= self.input_history.len() {
            self.history_index = None;
            self.input.clear();
            self.input_cursor = 0;
            return;
        }
        let index = index + 1;
        self.history_index = Some(index);
        self.input = self.input_history[index].clone();
        self.input_cursor = self.input.len();
    }

    fn handle_paste(&mut self, text: &str) {
        if self.setup.is_some() {
            self.setup_paste(text);
            return;
        }
        if self.terminal_focus {
            if let Some(data) = self.terminal_data.as_ref() {
                if let Err(err) = data.send_data(text.as_bytes()) {
                    self.status = format!("terminal paste failed: {err}");
                }
            }
            return;
        }
        if !self.approvals.is_empty() {
            return;
        }
        if !self.composing {
            self.composing = true;
            if self.selected_task_id().is_none() {
                self.new_task = true;
            }
        }
        self.input_cursor = self.input_cursor.min(self.input.len());
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.input.insert_str(self.input_cursor, &normalized);
        self.input_cursor += normalized.len();
        self.history_index = None;
        self.status = "pasted".into();
    }

    fn compose_key(&mut self, key: KeyEvent) -> bool {
        self.input_cursor = self.input_cursor.min(self.input.len());
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('c') => {
                    self.composing = false;
                    self.input.clear();
                    self.input_cursor = 0;
                    self.status = "message cancelled".into();
                }
                KeyCode::Char('u') => {
                    self.input.clear();
                    self.input_cursor = 0;
                }
                KeyCode::Char('a') => self.input_cursor = 0,
                KeyCode::Char('e') => self.input_cursor = self.input.len(),
                KeyCode::Char('k') => {
                    self.input.truncate(self.input_cursor);
                }
                KeyCode::Char('w') => {
                    let before = &self.input[..self.input_cursor];
                    let trimmed = before.trim_end();
                    let cut = trimmed
                        .rfind(char::is_whitespace)
                        .map(|index| index + 1)
                        .unwrap_or(0);
                    self.input.replace_range(cut..self.input_cursor, "");
                    self.input_cursor = cut;
                }
                KeyCode::Char('j') => {
                    self.input.insert(self.input_cursor, '\n');
                    self.input_cursor += 1;
                }
                _ => {}
            }
            return false;
        }
        match key.code {
            KeyCode::Esc => {
                self.composing = false;
                if self.new_task {
                    self.new_task = false;
                    self.input.clear();
                    self.input_cursor = 0;
                    self.status = "new task cancelled".into();
                } else {
                    self.status = "editor closed".into();
                }
            }
            KeyCode::Enter
                if key.modifiers.contains(KeyModifiers::SHIFT)
                    || key.modifiers.contains(KeyModifiers::ALT) =>
            {
                self.input.insert(self.input_cursor, '\n');
                self.input_cursor += 1;
            }
            KeyCode::Enter => self.send_message(),
            KeyCode::Up => self.history_previous(),
            KeyCode::Down => self.history_next(),
            KeyCode::Backspace => {
                if let Some((index, _)) = self.input[..self.input_cursor].char_indices().last() {
                    self.input.remove(index);
                    self.input_cursor = index;
                }
            }
            KeyCode::Delete => {
                if self.input_cursor < self.input.len() {
                    self.input.remove(self.input_cursor);
                }
            }
            KeyCode::Left => {
                if let Some((index, _)) = self.input[..self.input_cursor].char_indices().last() {
                    self.input_cursor = index;
                }
            }
            KeyCode::Right => {
                if let Some(character) = self.input[self.input_cursor..].chars().next() {
                    self.input_cursor += character.len_utf8();
                }
            }
            KeyCode::Home => self.input_cursor = 0,
            KeyCode::End => self.input_cursor = self.input.len(),
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::ALT) && !character.is_control() =>
            {
                self.input.insert(self.input_cursor, character);
                self.input_cursor += character.len_utf8();
            }
            _ => {}
        }
        false
    }

    fn open_palette(&mut self) {
        self.composing = false;
        self.palette = Some(PaletteState::default());
        self.status = "commands · type to filter · Enter run · Esc close".into();
    }

    fn palette_key(&mut self, key: KeyEvent) -> bool {
        let Some(mut palette) = self.palette.take() else {
            return false;
        };
        let filtered = filtered_commands(&palette.query);
        match key.code {
            KeyCode::Esc => {
                self.status.clear();
                return false;
            }
            KeyCode::Up => palette.selected = palette.selected.saturating_sub(1),
            KeyCode::Down => {
                if !filtered.is_empty() {
                    palette.selected = (palette.selected + 1).min(filtered.len() - 1);
                }
            }
            KeyCode::Enter => {
                let Some(index) = filtered
                    .get(palette.selected.min(filtered.len().saturating_sub(1)))
                    .copied()
                else {
                    return false;
                };
                self.palette = None;
                self.show_sessions = false;
                self.run_command(COMMANDS[index].command);
                return false;
            }
            KeyCode::Backspace => {
                palette.query.pop();
                palette.selected = 0;
            }
            KeyCode::Char(character)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT)
                    && !character.is_control() =>
            {
                palette.query.push(character);
                palette.selected = 0;
            }
            _ => {}
        }
        let count = filtered_commands(&palette.query).len();
        palette.selected = palette.selected.min(count.saturating_sub(1));
        self.palette = Some(palette);
        false
    }

    fn sessions_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Tab => {
                self.show_sessions = false;
                self.status.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_session_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_session_selection(1),
            KeyCode::Home | KeyCode::Char('g') => self.session_selection = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.session_selection = self.tasks.len();
            }
            KeyCode::Enter => {
                if self.session_selection == 0 {
                    self.start_new_session();
                } else {
                    self.open_session_at(self.session_selection - 1);
                }
            }
            KeyCode::Char('n') => self.start_new_session(),
            _ => {}
        }
        false
    }

    fn findings_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Tab => {
                self.show_findings = false;
                self.status.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.findings_scroll = self.findings_scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.findings_scroll = self
                    .findings_scroll
                    .saturating_add(1)
                    .min(self.findings.len().saturating_sub(1));
            }
            KeyCode::Home | KeyCode::Char('g') => self.findings_scroll = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.findings_scroll = self.findings.len().saturating_sub(1);
            }
            KeyCode::Char('r') => match self.load_findings() {
                Ok(()) => {
                    self.status = format!("{} finding(s)", self.findings.len());
                }
                Err(err) => self.status = format!("findings failed: {err}"),
            },
            _ => {}
        }
        false
    }

    fn has_llm_key(&self) -> bool {
        self.daemon_llm_key || !self.api_key.trim().is_empty()
    }

    fn active_profile_index(&self) -> Option<usize> {
        self.profiles.iter().position(|profile| {
            profile.provider == self.provider
                && profile.model == self.model
                && profile.effort == self.effort
                && profile.base_url == self.base_url
                && profile.api_key == self.api_key
        })
    }

    fn open_providers(&mut self) {
        self.composing = false;
        self.show_providers = true;
        self.provider_delete_armed = None;
        self.provider_selection = self
            .active_profile_index()
            .unwrap_or(0)
            .min(self.profiles.len().saturating_sub(1));
        self.status = if self.profiles.is_empty() {
            "no saved providers · n add one".into()
        } else {
            format!(
                "{} provider profile(s) · Enter use · n new · e edit · d delete",
                self.profiles.len()
            )
        };
    }

    fn activate_profile(&mut self, index: usize) {
        let Some(profile) = self.profiles.get(index).cloned() else {
            return;
        };
        self.provider = if profile.provider.is_empty() {
            default_provider()
        } else {
            profile.provider.clone()
        };
        self.model = profile.model.clone();
        self.effort = if profile.effort.is_empty() {
            default_effort()
        } else {
            profile.effort.clone()
        };
        self.base_url = profile.base_url.clone();
        self.api_key = profile.api_key.clone();
        self.show_providers = false;
        self.provider_delete_armed = None;
        self.save_config();
        let name = if profile.name.is_empty() {
            profile_name(&self.provider, &self.model)
        } else {
            profile.name.clone()
        };
        if requires_key(&self.provider) && !self.has_llm_key() {
            self.status = format!("{name} selected · API key missing · e to edit");
        } else if self.selected_task_id().is_some() && !self.new_task {
            self.status = format!(
                "provider for next session: {name} · current stays {} · ctrl+n starts it",
                self.display_provider()
            );
        } else {
            self.status = format!("next session provider: {name}");
        }
    }

    fn providers_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc | KeyCode::Tab => {
                self.show_providers = false;
                self.provider_delete_armed = None;
                self.status.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.provider_delete_armed = None;
                self.provider_selection = self.provider_selection.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.provider_delete_armed = None;
                if !self.profiles.is_empty() {
                    self.provider_selection =
                        (self.provider_selection + 1).min(self.profiles.len() - 1);
                }
            }
            KeyCode::Home | KeyCode::Char('g') => {
                self.provider_delete_armed = None;
                self.provider_selection = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                self.provider_delete_armed = None;
                self.provider_selection = self.profiles.len().saturating_sub(1);
            }
            KeyCode::Enter => {
                if !self.profiles.is_empty() {
                    let index = self.provider_selection.min(self.profiles.len() - 1);
                    self.activate_profile(index);
                }
            }
            KeyCode::Char('n') => {
                self.show_providers = false;
                self.open_setup_new();
            }
            KeyCode::Char('e') => {
                if !self.profiles.is_empty() {
                    let index = self.provider_selection.min(self.profiles.len() - 1);
                    self.open_setup_edit_profile(index);
                }
            }
            KeyCode::Char('s') => {
                self.show_providers = false;
                self.open_setup();
            }
            KeyCode::Char('d') => {
                if self.profiles.is_empty() {
                    return false;
                }
                let index = self.provider_selection.min(self.profiles.len() - 1);
                if self.provider_delete_armed == Some(index) {
                    let name = std::mem::take(&mut self.profiles[index].name);
                    self.profiles.remove(index);
                    self.provider_selection = self
                        .provider_selection
                        .min(self.profiles.len().saturating_sub(1));
                    self.provider_delete_armed = None;
                    self.save_config();
                    self.status = if name.is_empty() {
                        "provider profile deleted".into()
                    } else {
                        format!("provider profile {name} deleted")
                    };
                } else {
                    self.provider_delete_armed = Some(index);
                    let name = self.profiles[index].name.clone();
                    self.status = format!("press d again to delete {name}");
                }
            }
            _ => {
                self.provider_delete_armed = None;
            }
        }
        false
    }

    fn mode_key(&mut self, key: KeyEvent) -> bool {
        if self.modes.is_empty() {
            self.show_modes = false;
            return false;
        }
        match key.code {
            KeyCode::Esc => {
                self.show_modes = false;
                self.status.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.mode_selection = self.mode_selection.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.mode_selection = (self.mode_selection + 1).min(self.modes.len() - 1);
            }
            KeyCode::Enter => {
                self.mode = self.modes[self.mode_selection].id.clone();
                self.save_config();
                self.show_modes = false;
                self.announce_mode_saved();
            }
            _ => {}
        }
        false
    }

    fn permission_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.show_permissions = false;
                self.status.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.permission_selection = self.permission_selection.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.permission_selection =
                    (self.permission_selection + 1).min(PERMISSIONS.len() - 1);
            }
            KeyCode::Enter => {
                self.permission = PERMISSIONS[self.permission_selection].0.to_string();
                self.save_config();
                self.show_permissions = false;
                self.announce_permission_saved();
            }
            _ => {}
        }
        false
    }

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if self.setup.is_some() {
            return self.setup_key(key);
        }
        if self.terminal_focus {
            if self.terminal.is_none() {
                self.terminal_focus = false;
            } else {
                if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('q') {
                    self.terminal_focus = false;
                    self.status = "terminal detached".into();
                    return false;
                }
                if let Some(bytes) = terminal_key_bytes(key) {
                    if let Some(data) = self.terminal_data.as_ref() {
                        if let Err(err) = data.send_data(&bytes) {
                            self.status = format!("terminal write failed: {err}");
                        }
                    }
                }
                return false;
            }
        }

        if self.palette.is_some() {
            return self.palette_key(key);
        }

        if !self.approvals.is_empty() {
            match key.code {
                KeyCode::Char('y') => self.respond_approval(true),
                KeyCode::Char('n') | KeyCode::Esc => self.respond_approval(false),
                _ => {}
            }
            return false;
        }

        if self.show_findings {
            return self.findings_key(key);
        }

        if self.show_modes {
            return self.mode_key(key);
        }

        if self.show_providers {
            return self.providers_key(key);
        }

        if self.show_permissions {
            return self.permission_key(key);
        }

        if self.show_sessions {
            return self.sessions_key(key);
        }

        if self.show_help {
            if matches!(key.code, KeyCode::Esc | KeyCode::F(1)) {
                self.show_help = false;
            }
            return false;
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('p') {
            self.open_palette();
            return false;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('n') {
            self.start_new_session();
            return false;
        }
        if key.code == KeyCode::F(1) {
            self.show_help = !self.show_help;
            return false;
        }
        if key.code == KeyCode::Tab {
            self.toggle_sessions();
            return false;
        }

        if self.composing {
            return self.compose_key(key);
        }

        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if control {
            match key.code {
                KeyCode::Char('p') => self.open_palette(),
                KeyCode::Char('c') => {
                    if self.selected_state() == "running" {
                        self.interrupt_task();
                    } else {
                        self.close_terminal();
                        return true;
                    }
                }
                _ => {}
            }
            return false;
        }

        match key.code {
            KeyCode::Esc => {
                if self.selected_state() == "running" {
                    self.interrupt_task();
                } else {
                    self.show_help = false;
                    self.status.clear();
                }
            }
            KeyCode::Tab => self.toggle_sessions(),
            KeyCode::F(1) => self.show_help = !self.show_help,
            KeyCode::PageUp | KeyCode::Up => {
                self.auto_scroll = false;
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(5);
            }
            KeyCode::PageDown | KeyCode::Down => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(5);
                if self.scroll_from_bottom == 0 {
                    self.auto_scroll = true;
                }
            }
            KeyCode::End => {
                self.auto_scroll = true;
                self.scroll_from_bottom = 0;
            }
            KeyCode::Enter => self.enter_compose(),
            KeyCode::Char(character) if !alt && !character.is_control() => {
                self.enter_compose();
                self.input.insert(self.input_cursor, character);
                self.input_cursor += character.len_utf8();
            }
            _ => {}
        }
        false
    }

    fn cancel_task(&mut self) {
        let Some(task_id) = self.selected_task_id() else {
            self.status = "no task selected".into();
            return;
        };
        match self
            .client
            .request(methods::TASK_CANCEL, json!({"task_id": task_id}))
        {
            Ok(_) => {
                self.chat.push(ChatLine {
                    task_id: task_id.clone(),
                    role: ChatRole::System,
                    text: "[cancelled]".into(),
                    streaming: false,
                });
                self.trim_chat();
                self.status = "session cancelled".into();
            }
            Err(err) => self.status = format!("cancel failed: {err}"),
        }
    }

    fn toggle_terminal(&mut self) {
        if let Some(terminal) = &self.terminal {
            self.terminal_focus = true;
            self.status = format!("terminal {} focused; Ctrl+Q detaches", terminal.session_id);
            return;
        }
        let data = match DataClient::connect(&self.root) {
            Ok(data) => data,
            Err(err) => {
                self.status = format!("terminal connect failed: {err}");
                return;
            }
        };
        match data.open_terminal(100, 30) {
            Ok(opened) => {
                self.terminal = Some(TerminalView::new(
                    opened.session_id,
                    opened.cols,
                    opened.rows,
                ));
                self.terminal_data = Some(data);
                self.terminal_focus = true;
                self.status = "terminal opened; Ctrl+Q detaches".into();
            }
            Err(err) => self.status = format!("terminal open failed: {err}"),
        }
    }

    fn close_terminal(&mut self) {
        self.terminal_focus = false;
        if let Some(data) = self.terminal_data.take() {
            let _ = data.close();
        }
        self.terminal = None;
    }

    fn input_height(&self) -> u16 {
        if !self.composing {
            return 5;
        }
        let lines = self.input.matches('\n').count() as u16 + 1;
        (lines + 4).clamp(5, 12)
    }

    fn workspace_label(&self) -> String {
        let path = self
            .daemon_workspace
            .clone()
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|path| path.display().to_string())
            })
            .unwrap_or_else(|| self.root.display().to_string());
        let workspace = Path::new(&path);
        if let Ok(cwd) = std::env::current_dir() {
            if workspace == cwd {
                return "~".into();
            }
            if let Ok(rest) = workspace.strip_prefix(&cwd) {
                let rest = rest.display().to_string().replace('\\', "/");
                if !rest.is_empty() {
                    return format!("~/{rest}");
                }
            }
        }
        workspace
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or(&path)
            .to_string()
    }

    fn draw(&self, frame: &mut ratatui::Frame) {
        let area = frame.area();
        frame.render_widget(Block::default().style(Style::default().bg(BG)), area);

        if self.terminal_focus && self.terminal.is_some() {
            self.draw_terminal(frame, area);
            return;
        }

        let task_id = self.selected_task_id();
        let has_messages = task_id
            .as_ref()
            .is_some_and(|task_id| self.chat.iter().any(|line| &line.task_id == task_id));
        let home = self.new_task || task_id.is_none() || !has_messages;
        let input_height = self.input_height();

        let footer_area = Rect {
            x: area.x,
            y: area.bottom().saturating_sub(1),
            width: area.width,
            height: 1,
        };
        let input_width = if home {
            88.min(area.width.saturating_sub(6)).max(24)
        } else {
            104.min(area.width.saturating_sub(4)).max(24)
        };
        let input_x = area.x + area.width.saturating_sub(input_width) / 2;
        let (input_area, hints_area) = if home {
            let target = area.y + ((area.height as u32 * 52 / 100) as u16);
            let input_y = target
                .min(area.bottom().saturating_sub(input_height + 2))
                .max(area.y + 1);
            (
                Rect {
                    x: input_x,
                    y: input_y,
                    width: input_width,
                    height: input_height,
                },
                Rect {
                    x: input_x,
                    y: input_y + input_height,
                    width: input_width,
                    height: 1,
                },
            )
        } else {
            let hints_y = footer_area.y.saturating_sub(1);
            (
                Rect {
                    x: input_x,
                    y: hints_y.saturating_sub(input_height).max(area.y + 1),
                    width: input_width,
                    height: input_height,
                },
                Rect {
                    x: input_x,
                    y: hints_y,
                    width: input_width,
                    height: 1,
                },
            )
        };

        if home {
            self.draw_home(frame, area, input_area);
        } else {
            self.draw_chat(frame, area, input_area);
        }

        let focused = self.setup.is_none()
            && self.palette.is_none()
            && self.approvals.is_empty()
            && !self.show_sessions
            && !self.show_findings
            && !self.show_modes
            && !self.show_providers
            && !self.show_permissions
            && !self.show_help;
        let modal = !focused;
        if !modal {
            self.draw_prompt(frame, input_area, focused);
            self.draw_hints(frame, input_area, hints_area);
        }
        self.draw_footer(frame, footer_area);

        if self.show_events {
            let width = (area.width / 3).max(24).min(area.width);
            let event_area = Rect {
                x: area.right().saturating_sub(width + 1),
                y: area.y + 1,
                width,
                height: input_area.y.saturating_sub(area.y + 2),
            };
            self.draw_events(frame, event_area);
        }

        if !focused {
            dim_background(frame);
        }
        if self.show_help {
            self.draw_help(frame, area);
        }
        if self.palette.is_some() {
            self.draw_palette(frame, area);
        }
        if self.show_sessions {
            self.draw_sessions(frame, area);
        }
        if self.show_findings {
            self.draw_findings(frame, area);
        }
        if self.show_modes {
            self.draw_modes(frame, area);
        }
        if self.show_providers {
            self.draw_providers(frame, area);
        }
        if self.show_permissions {
            self.draw_permissions(frame, area);
        }
        if self.setup.is_some() {
            self.draw_setup(frame);
        }
        if let Some(pending) = self.approvals.first() {
            self.draw_approval(frame, area, pending);
        }
    }

    fn draw_home(&self, frame: &mut ratatui::Frame, area: Rect, input_area: Rect) {
        let mark = wordmark(area.width);
        let mark_height = mark.len() as u16;
        let group = mark_height + 3;
        if input_area.y <= area.y + group {
            return;
        }
        let mark_y = input_area.y.saturating_sub(group).max(area.y);
        if mark_y + mark_height >= input_area.y {
            return;
        }
        let mark_area = Rect {
            x: area.x,
            y: mark_y,
            width: area.width,
            height: mark_height,
        };
        frame.render_widget(
            Paragraph::new(Text::from(mark)).alignment(Alignment::Center),
            mark_area,
        );

        let mut chips: Vec<Span> = Vec::new();
        chips.push(chip(
            format!(" {} ", self.workspace_label()),
            BG,
            Color::Rgb(49, 49, 56),
        ));
        chips.push(Span::raw(" "));
        let display_provider = self.display_provider();
        let display_model = self.display_model();
        let display_effort = self.display_effort();
        chips.push(chip(
            format!(" {} ", display_provider),
            ACCENT,
            Color::Rgb(24, 38, 58),
        ));
        if !display_model.is_empty() {
            chips.push(Span::raw(" "));
            chips.push(chip(
                format!(" {} ", display_model),
                GREEN,
                Color::Rgb(20, 42, 30),
            ));
        }
        if display_effort != "default" {
            chips.push(Span::raw(" "));
            chips.push(chip(
                format!(" {} ", display_effort),
                YELLOW,
                Color::Rgb(43, 37, 20),
            ));
        }
        let chip_area = Rect {
            x: area.x,
            y: mark_area.y + mark_height + 1,
            width: area.width,
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(Line::from(chips)).alignment(Alignment::Center),
            chip_area,
        );
    }

    fn draw_chat(&self, frame: &mut ratatui::Frame, area: Rect, input_area: Rect) {
        let header_area = Rect {
            x: area.x + 2,
            y: area.y,
            width: area.width.saturating_sub(4),
            height: 1,
        };
        let state = self.selected_state();
        let last_error = self.selected_last_error();
        let spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let (state_text, state_color) = match state {
            "running" => (
                format!("{} running", spinner[(self.spin.get() / 2) % spinner.len()]),
                ACCENT,
            ),
            "waiting_input" if last_error.is_some() => ("✗ last turn failed".to_string(), RED),
            "waiting_input" => ("● ready".to_string(), GREEN),
            "failed" => ("✗ failed".to_string(), RED),
            "cancelled" => ("✗ cancelled".to_string(), RED),
            "done" => ("✓ done".to_string(), DIM),
            other => (format!("● {other}"), DIM),
        };
        self.spin.set(self.spin.get().wrapping_add(1));
        let display_provider = self.display_provider();
        let display_model = self.display_model();
        let display_effort = self.display_effort();
        let right_text = format!(
            "{} · {} · effort {}",
            display_provider,
            if display_model.is_empty() {
                "-"
            } else {
                &display_model
            },
            display_effort
        );
        let right_width = right_text.chars().count() as u16;
        let left_width = header_area.width.saturating_sub(right_width + 2).max(12);
        let left = Line::from(vec![
            Span::styled("▌ ", Style::default().fg(ACCENT)),
            Span::styled(
                truncate(
                    &self.selected_task_title(),
                    left_width.saturating_sub(15) as usize,
                ),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  {state_text}"), Style::default().fg(state_color)),
        ]);
        frame.render_widget(
            Paragraph::new(left),
            Rect {
                x: header_area.x,
                y: header_area.y,
                width: left_width,
                height: 1,
            },
        );
        let right = Line::from(Span::styled(right_text, Style::default().fg(FAINT)));
        frame.render_widget(
            Paragraph::new(right).alignment(Alignment::Right),
            header_area,
        );
        let rule = Rect {
            x: area.x + 2,
            y: area.y + 1,
            width: area.width.saturating_sub(4),
            height: 1,
        };
        frame.render_widget(
            Paragraph::new("─".repeat(rule.width as usize))
                .style(Style::default().fg(Color::Rgb(38, 38, 44))),
            rule,
        );

        let transcript_area = Rect {
            x: area.x + 2,
            y: area.y + 2,
            width: area.width.saturating_sub(4),
            height: input_area.y.saturating_sub(area.y + 3),
        };
        if transcript_area.width == 0 || transcript_area.height == 0 {
            return;
        }

        let task_id = self.selected_task_id();
        let mut transcript: Vec<Line> = Vec::new();
        for line in self
            .chat
            .iter()
            .filter(|line| Some(line.task_id.as_str()) == task_id.as_deref())
        {
            let parts: Vec<&str> = line.text.split('\n').collect();
            match line.role {
                ChatRole::User => {
                    for (index, part) in parts.iter().enumerate() {
                        let prefix = if index == 0 { "❯ " } else { "  " };
                        transcript.push(Line::from(vec![
                            Span::styled(prefix, Style::default().fg(ACCENT)),
                            Span::styled((*part).to_string(), Style::default().fg(TEXT)),
                        ]));
                    }
                }
                ChatRole::Assistant => {
                    for (index, part) in parts.iter().enumerate() {
                        let suffix = if line.streaming && index + 1 == parts.len() {
                            "▍"
                        } else {
                            ""
                        };
                        transcript.push(Line::from(vec![
                            Span::raw("  "),
                            Span::styled((*part).to_string(), Style::default().fg(TEXT)),
                            Span::styled(suffix.to_string(), Style::default().fg(ACCENT)),
                        ]));
                    }
                }
                ChatRole::Tool => {
                    let color = if line.text.starts_with('✓') {
                        GREEN
                    } else if line.text.starts_with('✗') {
                        RED
                    } else {
                        DIM
                    };
                    transcript.push(Line::from(Span::styled(
                        format!("  {}", line.text),
                        Style::default().fg(color),
                    )));
                }
                ChatRole::System => {
                    if line.text.starts_with("turn end:") {
                        continue;
                    }
                    let is_error = line.text.starts_with('✗');
                    for (index, part) in parts.iter().enumerate() {
                        let color = if is_error && index == 0 { RED } else { FAINT };
                        transcript.push(Line::from(Span::styled(
                            format!("  {part}"),
                            Style::default().fg(color).add_modifier(Modifier::ITALIC),
                        )));
                    }
                }
            }
            transcript.push(Line::from(""));
        }

        if transcript.is_empty() {
            transcript.push(Line::from(Span::styled(
                "No messages yet. Type below to start.",
                Style::default().fg(DIM),
            )));
        }

        let paragraph = Paragraph::new(Text::from(transcript)).wrap(Wrap { trim: false });
        let line_count = paragraph.line_count(transcript_area.width);
        let max_scroll = line_count.saturating_sub(transcript_area.height as usize);
        let scroll = if self.auto_scroll {
            max_scroll
        } else {
            max_scroll.saturating_sub(self.scroll_from_bottom as usize)
        };
        frame.render_widget(
            paragraph.scroll((scroll.min(u16::MAX as usize) as u16, 0)),
            transcript_area,
        );
    }

    fn draw_prompt(&self, frame: &mut ratatui::Frame, area: Rect, focused: bool) {
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(FAINT))
            .style(Style::default().bg(PANEL));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if area.height > 2 {
            let rows = area.height - 2;
            let bar = std::iter::repeat("│")
                .take(rows as usize)
                .collect::<Vec<_>>()
                .join("\n");
            frame.render_widget(
                Paragraph::new(bar).style(Style::default().fg(ACCENT).bg(PANEL)),
                Rect {
                    x: area.x,
                    y: area.y + 1,
                    width: 1,
                    height: rows,
                },
            );
        }
        if inner.width < 2 || inner.height < 3 {
            return;
        }

        let meta_y = inner.y + inner.height - 1;
        let input_rows = inner.height.saturating_sub(2).max(1);
        let input_area = Rect {
            x: inner.x + 1,
            y: inner.y,
            width: inner.width.saturating_sub(1),
            height: input_rows,
        };

        if self.input.is_empty() {
            let placeholder = if self.new_task || self.selected_task_id().is_none() {
                "Ask anything…  \"inspect ./challenge and find the flag\""
            } else {
                "Reply…   Shift+Enter for a new line"
            };
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    placeholder,
                    Style::default().fg(DIM).bg(PANEL),
                ))),
                input_area,
            );
            if focused {
                frame.set_cursor_position((input_area.x, input_area.y));
            }
        } else {
            let cursor = self.input_cursor.min(self.input.len());
            let before = &self.input[..cursor];
            let row = before.matches('\n').count();
            let line_start = before.rfind('\n').map(|index| index + 1).unwrap_or(0);
            let cursor_cols = UnicodeWidthStr::width(&self.input[line_start..cursor]);
            let lines: Vec<&str> = self.input.split('\n').collect();
            let start_line = row.saturating_sub(input_rows.saturating_sub(1) as usize);
            let display = lines
                .iter()
                .skip(start_line)
                .take(input_rows as usize)
                .copied()
                .collect::<Vec<_>>()
                .join("\n");
            frame.render_widget(
                Paragraph::new(display).style(Style::default().fg(TEXT).bg(PANEL)),
                input_area,
            );
            if focused {
                let x = input_area.x + cursor_cols as u16;
                let y = input_area.y + (row - start_line) as u16;
                frame.set_cursor_position((
                    x.min(input_area.right().saturating_sub(1)),
                    y.min(input_area.bottom().saturating_sub(1)),
                ));
            }
        }

        let display_mode = self.display_mode();
        let mut meta = vec![Span::styled(
            format!(" {}", self.mode_label(&display_mode)),
            Style::default()
                .fg(mode_color(&display_mode))
                .bg(PANEL)
                .add_modifier(Modifier::BOLD),
        )];
        let display_provider = self.display_provider();
        let display_model = self.display_model();
        let display_effort = self.display_effort();
        meta.push(Span::styled(" · ", Style::default().fg(FAINT).bg(PANEL)));
        meta.push(Span::styled(
            display_provider.clone(),
            Style::default().fg(TEXT).bg(PANEL),
        ));
        if !display_model.is_empty() {
            meta.push(Span::styled(" · ", Style::default().fg(FAINT).bg(PANEL)));
            meta.push(Span::styled(
                display_model.clone(),
                Style::default().fg(GREEN).bg(PANEL),
            ));
        }
        if display_effort != "default" {
            meta.push(Span::styled(" · ", Style::default().fg(FAINT).bg(PANEL)));
            meta.push(Span::styled(
                format!("effort {display_effort}"),
                Style::default().fg(DIM).bg(PANEL),
            ));
        }
        if requires_key(&display_provider) && !self.has_llm_key() {
            meta.push(Span::styled("  ·  ", Style::default().fg(FAINT).bg(PANEL)));
            meta.push(Span::styled(
                "missing API key · ctrl+p providers",
                Style::default().fg(YELLOW).bg(PANEL),
            ));
        }
        frame.render_widget(
            Paragraph::new(Line::from(meta)),
            Rect {
                x: inner.x + 1,
                y: meta_y,
                width: inner.width.saturating_sub(1),
                height: 1,
            },
        );
    }

    fn draw_hints(&self, frame: &mut ratatui::Frame, input_area: Rect, hints_area: Rect) {
        let hints_width = 54.min(hints_area.width).max(1);
        let hints = Line::from(vec![
            Span::styled(
                "tab",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" sessions   ", Style::default().fg(DIM)),
            Span::styled(
                "ctrl+n",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" new   ", Style::default().fg(DIM)),
            Span::styled(
                "ctrl+p",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" commands   ", Style::default().fg(DIM)),
            Span::styled("F1", Style::default().fg(TEXT).add_modifier(Modifier::BOLD)),
            Span::styled(" help", Style::default().fg(DIM)),
        ]);
        let hint_area = Rect {
            x: input_area.right().saturating_sub(hints_width + 1),
            y: hints_area.y,
            width: hints_width,
            height: 1,
        };
        frame.render_widget(Paragraph::new(hints), hint_area);

        if !self.status.is_empty() {
            let width = input_area.width.saturating_sub(hints_width + 5).max(1);
            let color = if self.status.contains("failed") || self.status.contains("error") {
                RED
            } else if self.status.contains("missing") || self.status.contains("needs") {
                YELLOW
            } else {
                DIM
            };
            frame.render_widget(
                Paragraph::new(truncate(&self.status, width as usize))
                    .style(Style::default().fg(color)),
                Rect {
                    x: input_area.x + 2,
                    y: hints_area.y,
                    width,
                    height: 1,
                },
            );
        }
    }

    fn draw_footer(&self, frame: &mut ratatui::Frame, area: Rect) {
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(self.workspace_label(), Style::default().fg(FAINT)),
                Span::styled("  ·  ", Style::default().fg(FAINT)),
                Span::styled(
                    format!("权限 {}", permission_label(&self.display_permission())),
                    Style::default().fg(permission_color(&self.display_permission())),
                ),
            ])),
            Rect {
                x: area.x + 2,
                y: area.y,
                width: area.width.saturating_sub(4),
                height: 1,
            },
        );
        let right = format!("MitnickSolve {VERSION}");
        let width = right.chars().count() as u16;
        if area.width > width + 4 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled(right, Style::default().fg(FAINT))))
                    .alignment(Alignment::Right),
                Rect {
                    x: area.x + 2,
                    y: area.y,
                    width: area.width.saturating_sub(4),
                    height: 1,
                },
            );
        }
    }

    fn draw_events(&self, frame: &mut ratatui::Frame, area: Rect) {
        if area.width < 8 || area.height < 3 {
            return;
        }
        let visible = self.selected_events();
        let items: Vec<ListItem> = visible
            .iter()
            .rev()
            .take(area.height as usize)
            .map(|index| {
                let (_, event) = &self.events[*index];
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!("{} ", short_time(&event.ts)),
                        Style::default().fg(FAINT),
                    ),
                    Span::styled(event.kind.clone(), Style::default().fg(DIM)),
                    Span::styled(
                        format!("  {}", summarize(event, 48)),
                        Style::default().fg(FAINT),
                    ),
                ]))
            })
            .collect();
        frame.render_widget(Clear, area);
        let list = List::new(items).block(panel_block("events", DIM));
        frame.render_widget(list, area);
    }

    fn draw_help(&self, frame: &mut ratatui::Frame, area: Rect) {
        let area = centered_rect(70, 66, area);
        frame.render_widget(Clear, area);
        let lines = vec![
            help_line("Enter", "write a message / send"),
            help_line("Shift+Enter", "insert a newline"),
            help_line("Esc", "interrupt a running turn or close the editor"),
            help_line("Tab", "open the session switcher"),
            help_line("Ctrl+N", "start a new session"),
            help_line("Ctrl+P", "command palette"),
            help_line("F1", "toggle this help"),
            help_line("↑ / ↓", "scroll the transcript"),
            help_line("PgUp / PgDn", "scroll faster"),
            help_line("y / n", "approve or deny a pending tool call"),
            help_line("Ctrl+Q", "detach from the terminal"),
            help_line("Ctrl+C", "interrupt the turn, or quit when idle"),
            Line::from(""),
            Line::from(Span::styled(
                "The agent runs in the daemon. Every tool call goes through envd;",
                Style::default().fg(DIM),
            )),
            Line::from(Span::styled(
                "the working directory is your project unless --workspace says otherwise.",
                Style::default().fg(DIM),
            )),
        ];
        frame.render_widget(
            Paragraph::new(lines)
                .block(panel_block("keyboard shortcuts", ACCENT))
                .wrap(Wrap { trim: false }),
            area,
        );
    }

    fn draw_palette(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(palette) = &self.palette else {
            return;
        };
        let width = 76.min(area.width.saturating_sub(6)).max(24);
        let height = (COMMANDS.len() as u16 + 6)
            .min(area.height.saturating_sub(4))
            .max(8);
        let area = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, area);
        let block = panel_block("commands", ACCENT);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width < 6 || inner.height < 4 {
            return;
        }
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("❯ ", Style::default().fg(ACCENT)),
                Span::styled(palette.query.clone(), Style::default().fg(TEXT)),
                Span::styled("▌", Style::default().fg(ACCENT)),
                Span::styled(
                    if palette.query.is_empty() {
                        "  type a command"
                    } else {
                        ""
                    },
                    Style::default().fg(FAINT),
                ),
            ])),
            Rect {
                x: inner.x + 1,
                y: inner.y,
                width: inner.width.saturating_sub(2),
                height: 1,
            },
        );
        let filtered = filtered_commands(&palette.query);
        let list_area = Rect {
            x: inner.x,
            y: inner.y + 2,
            width: inner.width,
            height: inner.height.saturating_sub(3),
        };
        if filtered.is_empty() {
            frame.render_widget(
                Paragraph::new(Span::styled(
                    "  no matching command",
                    Style::default().fg(DIM),
                )),
                list_area,
            );
            return;
        }
        let items: Vec<ListItem> = filtered
            .iter()
            .map(|index| {
                let command = &COMMANDS[*index];
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<26}", command.label), Style::default().fg(TEXT)),
                    Span::styled(command.hint, Style::default().fg(DIM)),
                ]))
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(palette.selected.min(filtered.len() - 1)));
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(Color::Rgb(30, 45, 66))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");
        frame.render_stateful_widget(list, list_area, &mut state);
    }

    fn draw_sessions(&self, frame: &mut ratatui::Frame, area: Rect) {
        let width = 76.min(area.width.saturating_sub(6)).max(26);
        let height = ((self.tasks.len() as u16) + 5)
            .min(area.height.saturating_sub(4))
            .max(7);
        let area = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, area);
        let block = panel_block("sessions", ACCENT);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 3 {
            return;
        }
        frame.render_widget(
            Paragraph::new(Span::styled(
                "Open a saved conversation or start a fresh one.",
                Style::default().fg(DIM),
            )),
            Rect {
                x: inner.x + 1,
                y: inner.y,
                width: inner.width.saturating_sub(2),
                height: 1,
            },
        );
        let list_area = Rect {
            x: inner.x + 1,
            y: inner.y + 1,
            width: inner.width.saturating_sub(2),
            height: inner.height.saturating_sub(2),
        };
        let mut items: Vec<ListItem> = Vec::with_capacity(self.tasks.len() + 1);
        items.push(ListItem::new(Line::from(vec![
            Span::styled(
                if self.new_task { "● " } else { "＋ " },
                Style::default().fg(if self.new_task { GREEN } else { ACCENT }),
            ),
            Span::styled(
                format!("{:<14}", "New session"),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("start a fresh conversation", Style::default().fg(DIM)),
        ])));
        for (index, task) in self.tasks.iter().enumerate() {
            let state = task.get("state").and_then(Value::as_str).unwrap_or("?");
            let state_color = state_color(state);
            let current = !self.new_task && index == self.selected_task;
            let title = task.get("goal").and_then(Value::as_str).unwrap_or("");
            items.push(ListItem::new(Line::from(vec![
                Span::styled(
                    if current { "● " } else { "  " },
                    Style::default().fg(if current { GREEN } else { FAINT }),
                ),
                Span::styled(
                    format!("{:<14}", state_label(state)),
                    Style::default().fg(state_color),
                ),
                Span::styled(
                    truncate(title, list_area.width.saturating_sub(19) as usize),
                    Style::default().fg(TEXT),
                ),
            ])));
        }
        let mut state = ListState::default();
        state.select(Some(
            self.session_selection.min(items.len().saturating_sub(1)),
        ));
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(Color::Rgb(30, 45, 66))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");
        frame.render_stateful_widget(list, list_area, &mut state);
        frame.render_widget(
            Paragraph::new(Span::styled(
                "↑/↓ select · Enter open · n new · Esc close",
                Style::default().fg(FAINT),
            ))
            .alignment(Alignment::Right),
            Rect {
                x: inner.x,
                y: inner.bottom().saturating_sub(1),
                width: inner.width.saturating_sub(1),
                height: 1,
            },
        );
    }

    fn draw_findings(&self, frame: &mut ratatui::Frame, area: Rect) {
        let width = 100.min(area.width.saturating_sub(6)).max(30);
        let height = ((self.findings.len() as u16).saturating_mul(3) + 7)
            .min(30)
            .min(area.height.saturating_sub(4))
            .max(10);
        let area = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, area);
        let block = panel_block("assessment findings", ACCENT);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width < 10 || inner.height < 4 {
            return;
        }
        if self.findings.is_empty() {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        "No findings recorded.",
                        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        "The agent records findings with the finding_add tool.",
                        Style::default().fg(DIM),
                    )),
                ]),
                inner,
            );
            return;
        }

        let mut counts = [0usize; 5];
        for finding in &self.findings {
            let rank = finding_rank(
                finding
                    .get("severity")
                    .and_then(Value::as_str)
                    .unwrap_or("info"),
            ) as usize;
            if rank > 0 {
                counts[rank - 1] += 1;
            }
        }
        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    format!("{} finding(s)", self.findings.len()),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "   critical {} · high {} · medium {} · low {} · info {}",
                        counts[4], counts[3], counts[2], counts[1], counts[0]
                    ),
                    Style::default().fg(DIM),
                ),
            ]),
            Line::from(""),
        ];
        for finding in &self.findings {
            let severity = finding
                .get("severity")
                .and_then(Value::as_str)
                .unwrap_or("info");
            let color = match severity {
                "critical" | "high" => RED,
                "medium" => YELLOW,
                "low" => ACCENT,
                _ => DIM,
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{:<9}", severity.to_ascii_uppercase()),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    truncate(
                        finding
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("untitled"),
                        inner.width.saturating_sub(12) as usize,
                    ),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
            ]));
            let asset = finding.get("asset").and_then(Value::as_str).unwrap_or("");
            let status = finding.get("status").and_then(Value::as_str).unwrap_or("");
            let tags = finding
                .get("tags")
                .and_then(Value::as_array)
                .map(|tags| {
                    tags.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .unwrap_or_default();
            let mut meta = vec![Span::raw("  ")];
            if !asset.is_empty() {
                meta.push(Span::styled(asset.to_string(), Style::default().fg(ACCENT)));
            }
            if !status.is_empty() {
                meta.push(Span::styled(
                    format!("  {status}"),
                    Style::default().fg(DIM),
                ));
            }
            if !tags.is_empty() {
                meta.push(Span::styled(
                    format!("  [{tags}]"),
                    Style::default().fg(FAINT),
                ));
            }
            lines.push(Line::from(meta));
            if let Some(description) = finding.get("description").and_then(Value::as_str)
                && !description.is_empty()
            {
                lines.push(Line::from(Span::styled(
                    format!(
                        "  {}",
                        truncate(description, inner.width.saturating_sub(4) as usize)
                    ),
                    Style::default().fg(DIM),
                )));
            }
            lines.push(Line::from(""));
        }

        let list_area = Rect {
            x: inner.x,
            y: inner.y,
            width: inner.width,
            height: inner.height.saturating_sub(1),
        };
        let scroll = self.findings_scroll.min(lines.len().saturating_sub(1));
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll.min(u16::MAX as usize) as u16, 0)),
            list_area,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                "j/k scroll · r reload · Esc close",
                Style::default().fg(FAINT),
            ))
            .alignment(Alignment::Right),
            Rect {
                x: inner.x,
                y: inner.bottom().saturating_sub(1),
                width: inner.width.saturating_sub(1),
                height: 1,
            },
        );
    }

    fn draw_modes(&self, frame: &mut ratatui::Frame, area: Rect) {
        let width = 72.min(area.width.saturating_sub(6)).max(32);
        let height = (self.modes.len() as u16 + 7)
            .min(area.height.saturating_sub(4))
            .max(10);
        let area = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, area);
        let block = panel_block("agent mode", ACCENT);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut lines = vec![
            Line::from(vec![Span::styled(
                "Next session operates in the selected mode.",
                Style::default().fg(DIM),
            )]),
            Line::from(""),
        ];
        for (index, mode) in self.modes.iter().enumerate() {
            let selected = index == self.mode_selection;
            let current = mode.id == self.mode;
            lines.push(Line::from(vec![
                Span::styled(
                    if selected { "❯ " } else { "  " },
                    Style::default().fg(ACCENT),
                ),
                Span::styled(
                    format!("{:<12}", mode.id),
                    if selected {
                        Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(TEXT)
                    },
                ),
                Span::styled(
                    format!("{:<6}", self.mode_label(&mode.id)),
                    Style::default().fg(if current { GREEN } else { FAINT }),
                ),
                Span::styled(mode.description.clone(), Style::default().fg(DIM)),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "↑/↓ choose · Enter apply · Esc cancel",
            Style::default().fg(ACCENT),
        )));
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
            Rect {
                x: inner.x + 1,
                y: inner.y,
                width: inner.width.saturating_sub(2),
                height: inner.height,
            },
        );
    }

    fn draw_providers(&self, frame: &mut ratatui::Frame, area: Rect) {
        let width = 88.min(area.width.saturating_sub(6)).max(36);
        let rows = if self.profiles.is_empty() {
            3
        } else {
            self.profiles.len() * 2 + 1
        };
        let armed = if self.provider_delete_armed.is_some() {
            1
        } else {
            0
        };
        let height = (rows as u16 + 5 + armed)
            .min(area.height.saturating_sub(4))
            .max(10);
        let area = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, area);
        let block = panel_block("providers", ACCENT);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut lines = vec![
            Line::from(Span::styled(
                "Saved provider profiles. Switch with Enter; keys stay on this machine.",
                Style::default().fg(DIM),
            )),
            Line::from(""),
        ];
        if self.profiles.is_empty() {
            lines.push(Line::from(Span::styled(
                "No saved providers yet.",
                Style::default().fg(TEXT),
            )));
            lines.push(Line::from(Span::styled(
                "Press n to add one, or s for the full setup.",
                Style::default().fg(DIM),
            )));
        }
        let active = self.active_profile_index();
        for (index, profile) in self.profiles.iter().enumerate() {
            let selected = index == self.provider_selection;
            let current = active == Some(index);
            let name = if profile.name.is_empty() {
                profile_name(&profile.provider, &profile.model)
            } else {
                profile.name.clone()
            };
            lines.push(Line::from(vec![
                Span::styled(
                    if selected { "❯ " } else { "  " },
                    Style::default().fg(ACCENT),
                ),
                Span::styled(
                    if current { "● " } else { "○ " },
                    Style::default().fg(if current { GREEN } else { FAINT }),
                ),
                Span::styled(
                    name,
                    if selected {
                        Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(TEXT)
                    },
                ),
            ]));
            let model = if profile.model.is_empty() {
                "-".to_string()
            } else {
                profile.model.clone()
            };
            let effort = if profile.effort.is_empty() {
                "default".to_string()
            } else {
                profile.effort.clone()
            };
            let mut detail = vec![Span::styled(
                format!("    {} · {} · effort {}", profile.provider, model, effort),
                Style::default().fg(DIM),
            )];
            if !profile.base_url.is_empty() {
                detail.push(Span::styled(" · ", Style::default().fg(FAINT)));
                detail.push(Span::styled(
                    truncate(&profile.base_url, 40),
                    Style::default().fg(DIM),
                ));
            }
            detail.push(Span::styled(" · ", Style::default().fg(FAINT)));
            if !requires_key(&profile.provider) {
                detail.push(Span::styled("no key needed", Style::default().fg(FAINT)));
            } else if profile.api_key.is_empty() {
                detail.push(Span::styled(
                    if self.daemon_llm_key {
                        "key from mskd env"
                    } else {
                        "key missing"
                    },
                    Style::default().fg(if self.daemon_llm_key { DIM } else { YELLOW }),
                ));
            } else {
                detail.push(Span::styled("key set", Style::default().fg(GREEN)));
            }
            lines.push(Line::from(detail));
        }
        lines.push(Line::from(""));
        if let Some(index) = self.provider_delete_armed {
            if let Some(profile) = self.profiles.get(index) {
                lines.push(Line::from(Span::styled(
                    format!("press d again to delete {}", profile.name),
                    Style::default().fg(RED).add_modifier(Modifier::BOLD),
                )));
            }
        }
        lines.push(Line::from(Span::styled(
            "↑/↓ choose · Enter use · n new · e edit · d delete · s setup · Esc close",
            Style::default().fg(ACCENT),
        )));
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
            Rect {
                x: inner.x + 1,
                y: inner.y,
                width: inner.width.saturating_sub(2),
                height: inner.height,
            },
        );
    }

    fn draw_permissions(&self, frame: &mut ratatui::Frame, area: Rect) {
        let width = 78.min(area.width.saturating_sub(6)).max(34);
        let height = (PERMISSIONS.len() as u16 + 7)
            .min(area.height.saturating_sub(4))
            .max(10);
        let area = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, area);
        let block = panel_block("permissions", GREEN);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut lines = vec![
            Line::from(Span::styled(
                "Controls approval prompts for tool calls in the next session.",
                Style::default().fg(DIM),
            )),
            Line::from(""),
        ];
        for (index, (id, description)) in PERMISSIONS.iter().enumerate() {
            let selected = index == self.permission_selection;
            let current = *id == self.permission;
            lines.push(Line::from(vec![
                Span::styled(
                    if selected { "❯ " } else { "  " },
                    Style::default().fg(GREEN),
                ),
                Span::styled(
                    format!("{:<10}", id),
                    if selected {
                        Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(TEXT)
                    },
                ),
                Span::styled(
                    format!("{:<6}", permission_label(id)),
                    Style::default().fg(if current { permission_color(id) } else { FAINT }),
                ),
                Span::styled((*description).to_string(), Style::default().fg(DIM)),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "↑/↓ choose · Enter apply · Esc cancel",
            Style::default().fg(ACCENT),
        )));
        frame.render_widget(
            Paragraph::new(lines).wrap(Wrap { trim: false }),
            Rect {
                x: inner.x + 1,
                y: inner.y,
                width: inner.width.saturating_sub(2),
                height: inner.height,
            },
        );
    }

    fn draw_approval(&self, frame: &mut ratatui::Frame, area: Rect, pending: &Value) {
        let id = pending.get("id").and_then(Value::as_u64).unwrap_or(0);
        let tool = pending.get("tool").and_then(Value::as_str).unwrap_or("?");
        let reason = pending
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let args = serde_json::to_string_pretty(pending.get("args").unwrap_or(&Value::Null))
            .unwrap_or_default();
        let area = centered_rect(80, 48, area);
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(YELLOW))
            .style(Style::default().bg(PANEL))
            .title(Span::styled(
                format!(
                    " approval required · {} of {} · #{id} ",
                    1,
                    self.approvals.len()
                ),
                Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut lines = vec![
            Line::from(vec![
                Span::styled("tool  ", Style::default().fg(FAINT)),
                Span::styled(
                    tool.to_string(),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                Span::styled("why   ", Style::default().fg(FAINT)),
                Span::styled(reason.to_string(), Style::default().fg(DIM)),
            ]),
            Line::from(""),
            Line::from(Span::styled("arguments", Style::default().fg(FAINT))),
        ];
        let budget = inner.height.saturating_sub(7) as usize;
        for part in args.lines().take(budget.max(1)) {
            lines.push(Line::from(Span::styled(
                format!("  {part}"),
                Style::default().fg(DIM),
            )));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("y", Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
            Span::styled(" allow     ", Style::default().fg(DIM)),
            Span::styled("n", Style::default().fg(RED).add_modifier(Modifier::BOLD)),
            Span::styled(" deny", Style::default().fg(DIM)),
        ]));
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
    }

    fn draw_terminal(&self, frame: &mut ratatui::Frame, area: Rect) {
        let Some(terminal) = &self.terminal else {
            return;
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(FAINT))
            .style(Style::default().bg(BG))
            .title(Span::styled(
                format!(
                    " terminal {} · {}×{} · Ctrl+Q detach ",
                    terminal.session_id, terminal.cols, terminal.rows
                ),
                Style::default().fg(DIM),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        self.terminal_size.set((inner.width, inner.height));
        frame.render_widget(
            TerminalWidget {
                screen: terminal.screen(),
            },
            inner,
        );
        let screen = terminal.screen();
        if !screen.hide_cursor() {
            let (row, col) = screen.cursor_position();
            if col < inner.width && row < inner.height {
                frame.set_cursor_position((inner.x + col, inner.y + row));
            }
        }
    }
}

fn run_daemon(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    root: PathBuf,
    refresh_ms: u64,
    provider: Option<String>,
    setup: bool,
    setup_step: Option<String>,
) -> Result<()> {
    let mut app = DaemonApp::connect(root, refresh_ms, provider)?;
    if setup {
        app.open_setup();
        open_setup_step(&mut app, setup_step.as_deref());
    }
    loop {
        app.refresh()?;
        if app.quit_requested {
            return Ok(());
        }
        terminal.draw(|frame| app.draw(frame))?;
        let poll_ms = if app.terminal_focus {
            50
        } else {
            app.refresh_ms
        };
        if event::poll(Duration::from_millis(poll_ms))? {
            match event::read()? {
                CrosstermEvent::Key(key)
                    if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                {
                    if app.handle_key(key) {
                        return Ok(());
                    }
                }
                CrosstermEvent::Paste(text) => app.handle_paste(&text),
                _ => {}
            }
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let provider = cli
        .provider
        .clone()
        .or_else(|| std::env::var("MSK_PROVIDER").ok());

    if cli.doctor {
        return doctor(&cli, &provider);
    }
    let daemon_root = if cli.events.is_none() {
        let root = cli.connect.clone().unwrap_or_else(|| cli.root.clone());
        let root = msk_ipc::normalize_root(&root);
        ensure_daemon(&root, cli.no_autostart || cli.connect.is_some())?;
        Some(root)
    } else {
        None
    };

    if cli.snapshot {
        let root = daemon_root.as_ref().context("--snapshot needs a daemon")?;
        return snapshot_daemon(
            root,
            provider,
            cli.setup,
            cli.setup_step.clone(),
            cli.snapshot_palette,
            cli.snapshot_sessions,
            cli.snapshot_findings,
            cli.snapshot_modes,
            cli.snapshot_providers,
            cli.snapshot_permissions,
        );
    }

    enable_raw_mode()?;
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        hook(info);
    }));
    let mut out = stdout();
    execute!(out, EnterAlternateScreen, EnableBracketedPaste)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let result = match (cli.events.clone(), daemon_root) {
        (Some(events), _) => run_log(&mut terminal, events, cli.refresh_ms),
        (None, Some(root)) => run_daemon(
            &mut terminal,
            root,
            cli.refresh_ms,
            provider,
            cli.setup,
            cli.setup_step.clone(),
        ),
        (None, None) => unreachable!("daemon root is set when no event log is given"),
    };

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    )?;
    terminal.show_cursor()?;
    result
}

fn snapshot_daemon(
    root: &Path,
    provider: Option<String>,
    setup: bool,
    setup_step: Option<String>,
    palette: bool,
    sessions: bool,
    findings: bool,
    modes: bool,
    providers: bool,
    permissions: bool,
) -> Result<()> {
    let mut app = DaemonApp::connect(root.to_path_buf(), 100, provider)?;
    let _ = app.refresh();
    if palette || sessions || findings || modes || providers || permissions {
        app.setup = None;
    }
    if setup {
        app.open_setup();
        open_setup_step(&mut app, setup_step.as_deref());
    }
    if palette {
        app.open_palette();
    }
    if sessions {
        app.open_sessions();
    }
    if findings {
        let _ = app.load_findings();
        app.show_findings = true;
    }
    if modes {
        app.mode_selection = app.mode_index(&app.mode);
        app.show_modes = true;
    }
    if providers {
        app.open_providers();
    }
    if permissions {
        app.permission_selection = permission_index(&app.permission);
        app.show_permissions = true;
    }
    app.auto_scroll = false;
    app.scroll_from_bottom = 10_000;
    let backend = ratatui::backend::TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| app.draw(frame))?;
    let buffer = terminal.backend().buffer();
    let mut out = std::io::stdout().lock();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        // A downstream `grep -q`/`head` may close the pipe early; that is not a
        // reason to panic a release binary.
        if writeln!(out, "{}", line.trim_end()).is_err() {
            break;
        }
    }
    Ok(())
}

fn doctor(cli: &Cli, provider: &Option<String>) -> Result<()> {
    let root = cli.connect.clone().unwrap_or_else(|| cli.root.clone());
    let root = msk_ipc::normalize_root(&root);
    println!("root: {}", root.display());
    println!(
        "daemon: {}",
        if daemon_reachable(&root) {
            "reachable"
        } else {
            "not running"
        }
    );
    let config = TuiConfig::load(&root);
    let provider = provider.clone().unwrap_or(config.provider);
    println!("provider: {provider}");
    let envd = std::env::var("MSK_ENVD")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            sibling_binary(if cfg!(windows) { "envd.exe" } else { "envd" }).or_else(|| {
                sibling_binary(if cfg!(windows) {
                    "msk-envd.exe"
                } else {
                    "msk-envd"
                })
            })
        });
    println!(
        "envd: {}",
        envd.as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "not found (set MSK_ENVD)".into())
    );
    let tools = tools_dir();
    println!(
        "tools: {} ({})",
        tools.display(),
        if tools.exists() { "ok" } else { "missing" }
    );
    let model = if config.model.is_empty() {
        std::env::var("MSK_LLM_MODEL").unwrap_or_else(|_| "-".into())
    } else {
        config.model.clone()
    };
    let effort = if config.effort.is_empty() {
        "default".to_string()
    } else {
        config.effort.clone()
    };
    let base = if config.base_url.is_empty() {
        std::env::var("MSK_LLM_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into())
    } else {
        config.base_url.clone()
    };
    let env_key = std::env::var("MSK_LLM_API_KEY").is_ok();
    let profile_key = !config.api_key.trim().is_empty();
    let key_state = if env_key {
        "set (env)"
    } else if profile_key {
        "set (profile)"
    } else {
        "missing"
    };
    println!("llm: model={model} effort={effort} base={base} key={key_state}");
    if provider == "openai" || provider == "openai-responses" {
        if model == "-" || (!env_key && !profile_key) {
            println!(
                "hint: set a key with Ctrl+P -> Providers, or export MSK_LLM_MODEL, MSK_LLM_API_KEY, and optionally MSK_LLM_BASE_URL"
            );
        }
    } else {
        println!("hint: offline provider is ready; use /demo for a tool demo");
    }
    Ok(())
}

fn tools_dir() -> PathBuf {
    if let Ok(path) = std::env::var("MSK_TOOLS_DIR") {
        return PathBuf::from(path);
    }
    let relative = PathBuf::from("tools/wasm");
    if relative.exists() {
        return relative;
    }
    if let Ok(exe) = std::env::current_exe() {
        for ancestor in exe.ancestors().skip(1).take(4) {
            let candidate = ancestor.join("tools/wasm");
            if candidate.exists() {
                return candidate;
            }
        }
    }
    relative
}

fn open_setup_step(app: &mut DaemonApp, step: Option<&str>) {
    let Some(step) = step else {
        return;
    };
    let Some(setup) = app.setup.as_mut() else {
        return;
    };
    setup.step = match step {
        "provider" => SetupStep::Provider,
        "service" => SetupStep::Preset,
        "model" => SetupStep::Model,
        "effort" => SetupStep::Effort,
        "key" => SetupStep::ApiKey,
        _ => SetupStep::BaseUrl,
    };
    if matches!(setup.step, SetupStep::BaseUrl | SetupStep::ApiKey) {
        setup.input_cursor = if setup.step == SetupStep::ApiKey {
            setup.api_key.len()
        } else {
            setup.base_url.len()
        };
    }
}

fn sibling_binary(name: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let candidate = exe.with_file_name(name);
    candidate.exists().then_some(candidate)
}

fn ensure_daemon(root: &Path, no_autostart: bool) -> Result<()> {
    if daemon_reachable(root) {
        return Ok(());
    }
    if no_autostart {
        bail!(
            "no mskd is listening at {}; start mskd or omit --no-autostart",
            root.display()
        );
    }
    let daemon = sibling_binary(if cfg!(windows) { "mskd.exe" } else { "mskd" })
        .context("mskd is not next to msk-tui; run `cargo build --bins`")?;
    let envd = sibling_binary(if cfg!(windows) { "envd.exe" } else { "envd" })
        .or_else(|| {
            sibling_binary(if cfg!(windows) {
                "msk-envd.exe"
            } else {
                "msk-envd"
            })
        })
        .context("envd is not next to msk-tui; build tools/envd first")?;
    if !root.exists() {
        std::fs::create_dir_all(root)
            .with_context(|| format!("create daemon root {}", root.display()))?;
    }
    let log_path = root.join("mskd.log");
    let log = File::create(&log_path).with_context(|| format!("create {}", log_path.display()))?;
    let errlog = log.try_clone()?;
    let mut child = Command::new(&daemon)
        .arg("--root")
        .arg(root)
        .arg("--envd")
        .arg(&envd)
        .arg("--tools-dir")
        .arg(tools_dir())
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(errlog))
        .spawn()
        .with_context(|| format!("start {}", daemon.display()))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if daemon_reachable(root) {
            return Ok(());
        }
        if let Ok(Some(status)) = child.try_wait() {
            bail!(
                "mskd exited during startup ({status}); see {}",
                log_path.display()
            );
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!(
        "mskd did not become ready within 10s; see {}",
        log_path.display()
    )
}

fn daemon_reachable(root: &Path) -> bool {
    Client::connect(root)
        .and_then(|mut client| client.hello())
        .is_ok()
}

fn run_log(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    events: PathBuf,
    refresh_ms: u64,
) -> Result<()> {
    let mut app = App::new(events);
    // Load any existing events before the first draw.
    let _ = app.refresh();

    loop {
        app.refresh()?;
        terminal.draw(|frame| app.draw(frame))?;

        if event::poll(Duration::from_millis(refresh_ms))?
            && let CrosstermEvent::Key(key) = event::read()?
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && app.handle_key(key)
        {
            return Ok(());
        }
    }
}

fn terminal_key_bytes(key: KeyEvent) -> Option<Vec<u8>> {
    if key.modifiers.contains(KeyModifiers::CONTROL) {
        if let KeyCode::Char(ch) = key.code {
            let ch = ch.to_ascii_lowercase();
            return Some(vec![(ch as u8) & 0x1f]);
        }
        return None;
    }
    match key.code {
        KeyCode::Char(ch) => Some(ch.to_string().into_bytes()),
        KeyCode::Enter => Some(vec![b'\r']),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![b'\t']),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        KeyCode::Insert => Some(b"\x1b[2~".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::F(1) => Some(b"\x1bOP".to_vec()),
        KeyCode::F(2) => Some(b"\x1bOQ".to_vec()),
        KeyCode::F(3) => Some(b"\x1bOR".to_vec()),
        KeyCode::F(4) => Some(b"\x1bOS".to_vec()),
        KeyCode::F(5) => Some(b"\x1b[15~".to_vec()),
        KeyCode::F(6) => Some(b"\x1b[17~".to_vec()),
        KeyCode::F(7) => Some(b"\x1b[18~".to_vec()),
        KeyCode::F(8) => Some(b"\x1b[19~".to_vec()),
        KeyCode::F(9) => Some(b"\x1b[20~".to_vec()),
        KeyCode::F(10) => Some(b"\x1b[21~".to_vec()),
        KeyCode::F(11) => Some(b"\x1b[23~".to_vec()),
        KeyCode::F(12) => Some(b"\x1b[24~".to_vec()),
        _ => None,
    }
}

struct TerminalWidget<'a> {
    screen: &'a vt100::Screen,
}

impl Widget for TerminalWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let (rows, cols) = self.screen.size();
        let rows = rows.min(area.height);
        let cols = cols.min(area.width);
        for row in 0..rows {
            let mut col = 0;
            while col < cols {
                let Some(cell) = self.screen.cell(row, col) else {
                    col += 1;
                    continue;
                };
                let symbol = cell.contents();
                if symbol.is_empty() {
                    col += 1;
                    continue;
                }
                let width = UnicodeWidthStr::width(symbol.as_str()).max(1) as u16;
                if col + width > cols {
                    break;
                }
                let style = Style::default()
                    .fg(map_color(cell.fgcolor()))
                    .bg(map_color(cell.bgcolor()))
                    .add_modifier(cell_modifier(cell));
                buf[(area.x + col, area.y + row)]
                    .set_symbol(&symbol)
                    .set_style(style);
                col += width;
            }
        }
    }
}

fn map_color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

fn cell_modifier(cell: &vt100::Cell) -> Modifier {
    let mut modifier = Modifier::empty();
    if cell.bold() {
        modifier |= Modifier::BOLD;
    }
    if cell.italic() {
        modifier |= Modifier::ITALIC;
    }
    if cell.underline() {
        modifier |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        modifier |= Modifier::REVERSED;
    }
    modifier
}

fn filtered_commands(query: &str) -> Vec<usize> {
    let needle = query.trim().to_lowercase();
    COMMANDS
        .iter()
        .enumerate()
        .filter(|(_, command)| {
            needle.is_empty()
                || command.label.to_lowercase().contains(&needle)
                || command.hint.to_lowercase().contains(&needle)
                || command.command.contains(&needle)
        })
        .map(|(index, _)| index)
        .collect()
}

fn state_color(state: &str) -> Color {
    match state {
        "running" => ACCENT,
        "waiting_input" => GREEN,
        "failed" | "cancelled" => RED,
        "done" => DIM,
        _ => FAINT,
    }
}

fn finding_rank(severity: &str) -> u8 {
    match severity {
        "critical" => 5,
        "high" => 4,
        "medium" => 3,
        "low" => 2,
        "info" => 1,
        _ => 0,
    }
}

fn state_label(state: &str) -> &'static str {
    match state {
        "running" => "● running",
        "waiting_input" => "● ready",
        "failed" => "✗ failed",
        "cancelled" => "✗ cancelled",
        "done" => "✓ done",
        _ => "· unknown",
    }
}

fn dim_background(frame: &mut ratatui::Frame) {
    let area = frame.area();
    let buffer = frame.buffer_mut();
    let fg = Color::Rgb(64, 64, 72);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            let cell = &mut buffer[(x, y)];
            cell.set_fg(fg);
            cell.set_bg(BG);
        }
    }
}

fn chip(text: String, fg: Color, bg: Color) -> Span<'static> {
    Span::styled(
        text,
        Style::default().fg(fg).bg(bg).add_modifier(Modifier::BOLD),
    )
}

fn panel_block(title: &str, accent: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(Style::default().fg(FAINT))
        .style(Style::default().bg(PANEL))
        .title(Span::styled(
            format!(" {title} "),
            Style::default().fg(accent).add_modifier(Modifier::BOLD),
        ))
}

fn help_line(key: &'static str, description: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{key:<14}"),
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(description, Style::default().fg(DIM)),
    ])
}

fn wordmark(width: u16) -> Vec<Line<'static>> {
    const PIXEL_WIDTH: u16 = 71;
    if width >= PIXEL_WIDTH + 6 {
        (0..7)
            .map(|row| {
                let mut spans = Vec::new();
                for (index, (_, glyph)) in WORDMARK.iter().enumerate() {
                    let mut text = String::new();
                    for cell in glyph[row].chars() {
                        text.push(if cell == '1' { '█' } else { ' ' });
                    }
                    if index + 1 < WORDMARK.len() {
                        text.push(' ');
                    }
                    let shade = 242u16
                        .saturating_sub(index as u16 * 10)
                        .saturating_sub(row as u16 * 5) as u8;
                    spans.push(Span::styled(
                        text,
                        Style::default().fg(Color::Rgb(shade, shade, shade.saturating_add(5))),
                    ));
                }
                Line::from(spans)
            })
            .collect()
    } else if width >= 24 {
        vec![Line::from(Span::styled(
            "M I T N I C K S O L V E",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ))]
    } else {
        vec![Line::from(Span::styled(
            "MSK",
            Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
        ))]
    }
}

fn short_time(ts: &str) -> String {
    ts.get(11..19).unwrap_or(ts).to_string()
}

fn summarize(event: &Event, max_chars: usize) -> String {
    let text = match event.kind.as_str() {
        "tool.call" => format!(
            "{} {}",
            event
                .data
                .get("tool")
                .and_then(|value| value.as_str())
                .unwrap_or("?"),
            truncate(
                &serde_json::to_string(event.data.get("args").unwrap_or(&event.data))
                    .unwrap_or_default(),
                70
            )
        ),
        "wasm.result" => format!(
            "{} {}ms{}",
            event
                .data
                .get("tool")
                .and_then(|value| value.as_str())
                .unwrap_or("?"),
            event
                .data
                .get("duration_ms")
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
            event
                .data
                .get("artifact")
                .and_then(|value| value.as_str())
                .map(|id| format!(" artifact {}", &id[..8.min(id.len())]))
                .unwrap_or_default()
        ),
        "data.put" => format!(
            "{} bytes {}",
            event
                .data
                .get("bytes")
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
            event
                .data
                .get("id")
                .and_then(|value| value.as_str())
                .map(|id| &id[..8.min(id.len())])
                .unwrap_or("?")
        ),
        "context.state" => format!(
            "{} messages, {} tokens",
            event
                .data
                .get("messages")
                .and_then(|value| value.as_u64())
                .unwrap_or(0),
            event
                .data
                .get("approx_tokens")
                .and_then(|value| value.as_u64())
                .unwrap_or(0)
        ),
        "llm.response" => format!(
            "{}",
            truncate(
                event
                    .data
                    .get("text")
                    .and_then(|value| value.as_str())
                    .unwrap_or(""),
                80
            )
        ),
        _ => truncate(
            &serde_json::to_string(&event.data).unwrap_or_default(),
            max_chars,
        ),
    };
    truncate(&text, max_chars)
}

fn detail_text(event: &Event) -> String {
    let mut lines = vec![
        format!("kind:  {}", event.kind),
        format!("actor: {}", event.actor),
        format!("ts:    {}", event.ts),
        format!("id:    {}", event.id),
        String::new(),
        serde_json::to_string_pretty(&event.data).unwrap_or_default(),
    ];
    lines.truncate(MAX_DETAIL_LINES);
    lines.join("\n")
}

fn observation_summary(data: &Value) -> Option<String> {
    let observation = data.get("observation")?;
    if let Some(text) = observation.as_str() {
        if let Ok(value) = serde_json::from_str::<Value>(text) {
            return Some(compact_json(&value, 120));
        }
        return Some(truncate(text, 120));
    }
    Some(compact_json(observation, 120))
}

fn compact_json(value: &Value, max_chars: usize) -> String {
    truncate(
        &serde_json::to_string(value).unwrap_or_else(|_| "null".into()),
        max_chars,
    )
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        format!("{}…", value.chars().take(max_chars).collect::<String>())
    }
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}

// Kept for callers that want a one-shot dump without the TUI.
#[allow(dead_code)]
fn dump_events(path: &Path) -> Result<Vec<Event>> {
    Ok(read_events(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;
    use serde_json::json;

    #[test]
    fn filters_match_event_kinds() {
        let tool = Event::new("tool.call", "kernel", json!({"tool": "exec"}));
        assert!(Filter::Tools.matches(&tool));
        assert!(!Filter::Llm.matches(&tool));

        let artifact = Event::new("data.put", "kernel", json!({"bytes": 10}));
        assert!(Filter::Artifacts.matches(&artifact));

        let context = Event::new("context.state", "kernel", json!({}));
        assert!(Filter::Context.matches(&context));
    }

    #[test]
    fn time_and_truncate_helpers() {
        assert_eq!(short_time("2026-11-14T08:01:09.123Z"), "08:01:09");
        assert_eq!(truncate("abcdef", 3), "abc…");
    }

    #[test]
    fn wordmark_scales_with_terminal_width() {
        assert_eq!(wordmark(120).len(), 7);
        assert_eq!(wordmark(40).len(), 1);
        let narrow = wordmark(20);
        assert_eq!(narrow.len(), 1);
        assert!(narrow[0].spans[0].content.contains("MSK"));
    }

    #[test]
    fn palette_filters_command_labels() {
        assert_eq!(filtered_commands("").len(), COMMANDS.len());
        let terminal = filtered_commands("term");
        assert!(
            terminal
                .iter()
                .any(|index| COMMANDS[*index].command == "/terminal")
        );
        let effort = filtered_commands("reasoning");
        assert!(
            effort
                .iter()
                .any(|index| COMMANDS[*index].command == "/effort")
        );
        let providers = filtered_commands("provider");
        assert!(
            providers
                .iter()
                .any(|index| COMMANDS[*index].command == "/providers")
        );
        assert!(filtered_commands("no-such-command").is_empty());
    }

    #[test]
    fn turn_errors_are_readable() {
        let message = r#"LLM call failed: http: HTTP 503: {"error":{"code":"model_not_found","message":"no channel for deepseek-v4.1-flash","type":"new_api_error"}}"#;
        let text = turn_error_text("error", message);
        assert!(text.starts_with("✗ model_not_found: no channel"));
        assert!(text.contains("ctrl+p providers"));
        assert_eq!(turn_error_text("error", ""), "✗ error");
    }

    #[test]
    fn provider_profile_helpers() {
        assert_eq!(profile_name("openai", ""), "openai");
        assert_eq!(profile_name("openai", " gpt-5 "), "openai · gpt-5");
        let profiles = vec![ProviderProfile {
            name: "openai · gpt-5".into(),
            provider: "openai".into(),
            ..Default::default()
        }];
        assert_eq!(
            unique_profile_name(&profiles, "openai · gpt-5"),
            "openai · gpt-5 (2)"
        );
        assert_eq!(unique_profile_name(&profiles, "local"), "local");
        assert_eq!(masked_key("sk-test"), "•••••••");
        assert_eq!(masked_key(""), "");
        assert_eq!(clean_key_paste(" sk-test\n"), "sk-test");
        assert_eq!(clean_key_paste("sk-\r\nabc "), "sk-abc");
        assert_eq!(preset_index(""), 0);
        assert_eq!(
            SERVICE_PRESETS[preset_index("https://api.deepseek.com/v1")].0,
            "deepseek"
        );
        assert_eq!(
            preset_index("http://127.0.0.1:9999/v1"),
            SERVICE_PRESETS.len() - 1
        );
    }

    #[test]
    fn mode_picker_covers_all_modes() {
        let modes = fallback_modes();
        assert_eq!(modes[0].id, "auto");
        assert!(
            modes
                .iter()
                .any(|mode| mode.id == "harden" && mode.label == "加固")
        );
        assert_eq!(mode_color("plan"), ACCENT);
        assert_eq!(mode_color("exploit"), RED);
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn daemon_input_recovers_from_stale_state() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app = DaemonApp::connect(PathBuf::from(root), 100, None).unwrap();
        app.setup = None;

        app.composing = true;
        app.input.clear();
        app.input_cursor = 24;
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(app.palette.is_some(), "Ctrl+P must work while composing");
        app.palette = None;

        app.composing = true;
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.input, "x");
        assert_eq!(app.input_cursor, 1);

        app.composing = false;
        app.terminal_focus = true;
        app.terminal = None;
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| app.draw(frame))
            .expect("a stale terminal focus must not freeze the UI");
    }

    #[test]
    fn terminal_parser_renders_text() {
        let mut parser = vt100::Parser::new(2, 10, 0);
        parser.process(b"hi");
        let area = Rect::new(0, 0, 10, 2);
        let mut buffer = Buffer::empty(area);
        TerminalWidget {
            screen: parser.screen(),
        }
        .render(area, &mut buffer);
        assert_eq!(buffer[(0, 0)].symbol(), "h");
        assert_eq!(buffer[(1, 0)].symbol(), "i");
        let key = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(terminal_key_bytes(key), Some(vec![b'\r']));
    }

    #[test]
    fn streamed_agent_text_accumulates_and_turns_end() {
        let mut chat = Vec::new();
        append_agent_text(&mut chat, "task-1", "hel", true);
        append_agent_text(&mut chat, "task-1", "lo", true);
        assert_eq!(chat.len(), 1);
        assert_eq!(chat[0].text, "hello");
        assert!(chat[0].streaming);
        finish_streaming(&mut chat, "task-1");
        assert!(!chat[0].streaming);
        append_agent_text(&mut chat, "task-1", "done", false);
        assert_eq!(chat.len(), 2);
        assert_eq!(chat[1].text, "done");
    }

    #[test]
    fn streamed_text_is_scoped_to_one_task() {
        let mut chat = Vec::new();
        append_agent_text(&mut chat, "task-1", "one", true);
        append_agent_text(&mut chat, "task-2", "two", true);
        assert_eq!(chat.len(), 2);
        assert_eq!(chat[0].task_id, "task-1");
        assert_eq!(chat[1].task_id, "task-2");
    }

    #[test]
    #[ignore = "requires a running mskd with a mock OpenAI-compatible LLM"]
    fn daemon_approval_modal_draws_and_responds() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app =
            DaemonApp::connect(PathBuf::from(root), 100, Some("scripted".into())).unwrap();
        let created = app
            .client
            .request(
                methods::TASK_CREATE,
                json!({
                    "goal": "approval draw test",
                    "provider": "openai",
                    "mode": "exploit",
                    "interactive": true,
                    "stream": true,
                    "approvals": "ask"
                }),
            )
            .unwrap();
        let task_id = created["task_id"].as_str().unwrap().to_string();
        for _ in 0..100 {
            let _ = app.reload_tasks();
            if let Some(index) = app.tasks.iter().position(|task| {
                task.get("task_id").and_then(Value::as_str) == Some(task_id.as_str())
            }) {
                app.selected_task = index;
            }
            if app.selected_state() == "waiting_input" {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(app.selected_state(), "waiting_input");

        app.enter_compose();
        app.input = "please exec echo approved".into();
        app.input_cursor = app.input.len();
        app.send_message();
        for _ in 0..100 {
            let _ = app.refresh();
            if !app.approvals.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(!app.approvals.is_empty());

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains("approval required"));
        assert!(content.contains("exec"));

        app.respond_approval(true);
        for _ in 0..100 {
            let _ = app.refresh();
            let _ = app.reload_tasks();
            if app.selected_state() == "waiting_input" && app.approvals.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(app.approvals.is_empty());
        app.cancel_task();
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn daemon_setup_saves_model_and_effort() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app = DaemonApp::connect(PathBuf::from(root), 100, Some("openai".into())).unwrap();
        app.model.clear();
        app.effort = "default".into();
        app.open_setup();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_key(enter); // provider -> service preset
        app.handle_key(enter); // service -> model
        for _ in 0..MODELS.len() - 1 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        app.handle_key(enter); // custom model -> text input
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for character in "gpt-5".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(enter); // model -> effort
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(enter); // effort -> base URL
        app.handle_key(enter); // base URL -> API key
        app.handle_key(enter); // API key -> save
        assert!(app.setup.is_none());
        assert_eq!(app.model, "gpt-5");
        assert_eq!(app.effort, "minimal");
        assert!(app.root.join("tui-config.json").exists());
        assert!(
            app.profiles
                .iter()
                .any(|profile| profile.model == "gpt-5" && profile.provider == "openai"),
            "the wizard result must be saved as a provider profile"
        );
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn daemon_setup_applies_service_preset() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app = DaemonApp::connect(PathBuf::from(root), 100, Some("openai".into())).unwrap();
        app.model.clear();
        app.base_url.clear();
        app.open_setup();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_key(enter); // provider -> service preset
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)); // DeepSeek
        app.handle_key(enter); // service -> model
        app.handle_key(enter); // custom model (prefilled) -> text input
        app.handle_key(enter); // model -> effort
        app.handle_key(enter); // effort -> base URL
        app.handle_key(enter); // base URL -> API key
        app.handle_key(enter); // save
        assert!(app.setup.is_none());
        assert_eq!(app.model, "deepseek-chat");
        assert_eq!(app.base_url, "https://api.deepseek.com/v1");
        assert!(
            app.profiles.iter().any(|profile| {
                profile.model == "deepseek-chat"
                    && profile.base_url == "https://api.deepseek.com/v1"
            }),
            "preset settings must be saved in the profile"
        );
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn daemon_provider_overlay_switches_profile() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app = DaemonApp::connect(PathBuf::from(root), 100, Some("openai".into())).unwrap();
        if !app.profiles.iter().any(|profile| {
            profile.base_url == "https://api.deepseek.com/v1" && profile.model == "deepseek-chat"
        }) {
            app.profiles.push(ProviderProfile {
                name: "deepseek".into(),
                provider: "openai".into(),
                model: "deepseek-chat".into(),
                effort: "high".into(),
                base_url: "https://api.deepseek.com/v1".into(),
                api_key: "sk-deepseek".into(),
            });
        }
        app.provider = "scripted-demo".into();
        app.model.clear();
        app.base_url.clear();
        app.api_key.clear();
        app.open_providers();
        assert!(app.show_providers);
        let index = app
            .profiles
            .iter()
            .position(|profile| profile.base_url == "https://api.deepseek.com/v1")
            .unwrap();
        let expected = app.profiles[index].clone();
        app.provider_selection = index;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.show_providers);
        assert_eq!(app.provider, "openai");
        assert_eq!(app.model, expected.model);
        assert_eq!(app.effort, expected.effort);
        assert_eq!(app.base_url, expected.base_url);
        assert_eq!(app.api_key, expected.api_key);
        if !expected.api_key.trim().is_empty() {
            assert!(app.has_llm_key());
        }
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn daemon_setup_saves_profile_key() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app = DaemonApp::connect(PathBuf::from(root), 100, Some("openai".into())).unwrap();
        app.api_key.clear();
        app.profiles.clear();
        app.open_setup();
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        app.handle_key(enter); // provider -> service preset
        app.handle_key(enter); // service -> model
        for _ in 0..MODELS.len() - 1 {
            app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        app.handle_key(enter); // custom model -> text input
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for character in "gpt-5".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(enter); // model -> effort
        app.handle_key(enter); // effort -> base URL
        app.handle_key(enter); // base URL -> API key
        for character in "sk-test-key".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(enter); // save
        assert!(app.setup.is_none());
        assert_eq!(app.api_key, "sk-test-key");
        assert!(app.has_llm_key());
        assert!(
            app.profiles
                .iter()
                .any(|profile| profile.api_key == "sk-test-key"),
            "the key must be stored with the profile"
        );
        let text = std::fs::read_to_string(app.root.join("tui-config.json")).unwrap();
        assert!(text.contains("sk-test-key"));
        assert!(text.contains("gpt-5"));
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn daemon_resume_loads_history() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app = DaemonApp::connect(PathBuf::from(root), 100, Some("openai".into())).unwrap();
        let before = app.tasks.len();
        app.resume_latest();
        for _ in 0..200 {
            let _ = app.refresh();
            let _ = app.reload_tasks();
            if app.tasks.len() > before && !app.chat.is_empty() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(app.tasks.len() > before);
        assert!(
            app.chat
                .iter()
                .any(|line| line.text.contains("mock complete")
                    || line.text.contains("session demo complete")),
            "resumed history was empty: {:?}",
            app.status
        );
        app.cancel_task();
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn daemon_setup_modal_draws() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app =
            DaemonApp::connect(PathBuf::from(root), 100, Some("scripted-demo".into())).unwrap();
        app.open_setup();
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains("setup"));
        assert!(content.contains("scripted-demo"));
        assert!(content.contains("MSK_LLM_API_KEY"));
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn failed_turn_is_visible_in_the_transcript() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app = DaemonApp::connect(PathBuf::from(root), 100, Some("openai".into())).unwrap();
        app.setup = None;
        let message = r#"LLM call failed: http: HTTP 503: {"error":{"code":"model_not_found","message":"no channel for deepseek-v4.1-flash","type":"new_api_error"}}"#;
        app.tasks = vec![json!({
            "task_id": "task-a", "goal": "failed turn", "state": "waiting_input",
            "provider": "openai", "model": "gpt-5", "effort": "default",
            "permission": "auto", "mode": "auto", "last_error": message
        })];
        app.selected_task = 0;
        app.new_task = false;
        app.chat = vec![ChatLine {
            task_id: "task-a".into(),
            role: ChatRole::System,
            text: turn_error_text("error", message),
            streaming: false,
        }];
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains("last turn failed"));
        assert!(content.contains("model_not_found"));
        assert!(content.contains("ctrl+p providers"));
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn session_switcher_moves_before_opening() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app =
            DaemonApp::connect(PathBuf::from(root), 100, Some("scripted".into())).unwrap();
        app.setup = None;
        app.tasks = vec![
            json!({
                "task_id": "task-a", "goal": "first", "state": "waiting_input",
                "provider": "scripted", "model": null, "effort": "default",
                "permission": "auto", "mode": "auto"
            }),
            json!({
                "task_id": "task-b", "goal": "second", "state": "waiting_input",
                "provider": "scripted", "model": null, "effort": "default",
                "permission": "auto", "mode": "auto"
            }),
        ];
        app.selected_task = 1;
        app.new_task = false;
        app.open_sessions();
        assert_eq!(app.session_selection, 2);
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(app.session_selection, 1);
        assert_eq!(app.selected_task, 1, "moving must not switch immediately");
        assert!(app.show_sessions, "the switcher stays open until Enter");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.selected_task, 0);
        assert!(!app.show_sessions);
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn mode_change_does_not_replace_the_open_session() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app =
            DaemonApp::connect(PathBuf::from(root), 100, Some("scripted".into())).unwrap();
        app.setup = None;
        app.tasks = vec![json!({
            "task_id": "task-a", "goal": "first", "state": "waiting_input",
            "provider": "scripted", "model": null, "effort": "default",
            "permission": "auto", "mode": "auto"
        })];
        app.selected_task = 0;
        app.new_task = false;
        let replacement = app
            .modes
            .iter()
            .position(|mode| mode.id != app.mode)
            .expect("another mode");
        app.mode_selection = replacement;
        app.show_modes = true;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.new_task, "changing a mode must not open a session");
        assert_eq!(app.selected_task, 0);
        assert!(app.status.contains("next session"));
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn daemon_create_uses_saved_key() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app = DaemonApp::connect(PathBuf::from(root), 100, Some("openai".into())).unwrap();
        app.provider = "openai".into();
        app.model = "gpt-5".into();
        app.effort = "default".into();
        app.base_url = "http://127.0.0.1:1/v1".into();
        app.api_key = "sk-test-key".into();
        let before = app.tasks.len();
        app.create_task_with_options("saved key gate test", false, None);
        assert!(
            !app.status.contains("missing API key"),
            "a saved key must satisfy the create gate: {}",
            app.status
        );
        let _ = app.reload_tasks();
        assert!(
            app.tasks.len() > before,
            "task was not created: {}",
            app.status
        );
        assert_eq!(app.selected_state(), "running");
        app.cancel_task();
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn daemon_chat_draws_transcript() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app =
            DaemonApp::connect(PathBuf::from(root), 100, Some("scripted".into())).unwrap();
        app.create_task_with_goal("Interactive session", false);
        for _ in 0..100 {
            let _ = app.refresh();
            let _ = app.reload_tasks();
            if app.selected_state() == "waiting_input" {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(app.selected_state(), "waiting_input");

        app.enter_compose();
        app.input = "hello tui".into();
        app.input_cursor = app.input.len();
        app.send_message();
        for _ in 0..100 {
            let _ = app.refresh();
            if app.chat.iter().any(|line| {
                line.role == ChatRole::Assistant && line.text.contains("echo: hello tui")
            }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(app.chat.iter().any(|line| {
            line.role == ChatRole::Assistant && line.text.contains("echo: hello tui")
        }));

        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let content: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(content.contains("hello tui"));
        assert!(content.contains("echo: hello tui"));
        assert!(content.contains("攻击") || content.contains("exploit"));
        assert!(content.contains("scripted"));
        app.cancel_task();
    }
}
