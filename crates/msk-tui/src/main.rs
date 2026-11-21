use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::fs::File;
use std::hash::{Hash, Hasher};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Stdout, Write, stdout};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    KeyboardEnhancementFlags, MouseEventKind, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
    supports_keyboard_enhancement,
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
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

mod i18n;
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
const AGENT: Color = Color::Rgb(165, 180, 252);
const TOOL: Color = Color::Rgb(167, 139, 250);
const USER_BG: Color = Color::Rgb(26, 28, 36);
const ERR_BG: Color = Color::Rgb(42, 22, 26);

struct CommandEntry {
    label_key: &'static str,
    hint_key: &'static str,
    command: &'static str,
}

impl CommandEntry {
    fn label(&self) -> &'static str {
        i18n::t(self.label_key)
    }

    fn hint(&self) -> &'static str {
        i18n::t(self.hint_key)
    }
}

const COMMANDS: &[CommandEntry] = &[
    CommandEntry {
        label_key: "cmd.new.label",
        hint_key: "cmd.new.hint",
        command: "/new",
    },
    CommandEntry {
        label_key: "cmd.sessions.label",
        hint_key: "cmd.sessions.hint",
        command: "/sessions",
    },
    CommandEntry {
        label_key: "cmd.resume.label",
        hint_key: "cmd.resume.hint",
        command: "/resume",
    },
    CommandEntry {
        label_key: "cmd.providers.label",
        hint_key: "cmd.providers.hint",
        command: "/providers",
    },
    CommandEntry {
        label_key: "cmd.setup.label",
        hint_key: "cmd.setup.hint",
        command: "/setup",
    },
    CommandEntry {
        label_key: "cmd.models.label",
        hint_key: "cmd.models.hint",
        command: "/models",
    },
    CommandEntry {
        label_key: "cmd.effort.label",
        hint_key: "cmd.effort.hint",
        command: "/effort",
    },
    CommandEntry {
        label_key: "cmd.mode.label",
        hint_key: "cmd.mode.hint",
        command: "/mode",
    },
    CommandEntry {
        label_key: "cmd.permissions.label",
        hint_key: "cmd.permissions.hint",
        command: "/permissions",
    },
    CommandEntry {
        label_key: "cmd.settings.label",
        hint_key: "cmd.settings.hint",
        command: "/settings",
    },
    CommandEntry {
        label_key: "cmd.language.label",
        hint_key: "cmd.language.hint",
        command: "/language",
    },
    CommandEntry {
        label_key: "cmd.demo.label",
        hint_key: "cmd.demo.hint",
        command: "/demo",
    },
    CommandEntry {
        label_key: "cmd.terminal.label",
        hint_key: "cmd.terminal.hint",
        command: "/terminal",
    },
    CommandEntry {
        label_key: "cmd.events.label",
        hint_key: "cmd.events.hint",
        command: "/events",
    },
    CommandEntry {
        label_key: "cmd.approvals.label",
        hint_key: "cmd.approvals.hint",
        command: "/approvals",
    },
    CommandEntry {
        label_key: "cmd.findings.label",
        hint_key: "cmd.findings.hint",
        command: "/findings",
    },
    CommandEntry {
        label_key: "cmd.clear.label",
        hint_key: "cmd.clear.hint",
        command: "/clear",
    },
    CommandEntry {
        label_key: "cmd.help.label",
        hint_key: "cmd.help.hint",
        command: "/help",
    },
    CommandEntry {
        label_key: "cmd.quit.label",
        hint_key: "cmd.quit.hint",
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
const PERMISSIONS: &[&str] = &["auto", "manual", "bypass"];

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
        .position(|id| *id == permission)
        .unwrap_or(0)
}

fn permission_label(permission: &str) -> String {
    i18n::permission_label(permission)
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
    #[serde(default = "default_language")]
    language: String,
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
            language: default_language(),
        }
    }
}

fn default_language() -> String {
    i18n::Language::detect().id().to_string()
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

fn is_timeout_error(err: &anyhow::Error) -> bool {
    err.downcast_ref::<std::io::Error>().is_some_and(|err| {
        err.kind() == std::io::ErrorKind::TimedOut || err.kind() == std::io::ErrorKind::WouldBlock
    })
}

/// Minimal inline markdown: `code` and **bold**, preserving everything else.
fn inline_spans(text: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let code = rest.find('`');
        let bold = rest.find("**");
        let (index, is_code) = match (code, bold) {
            (Some(code), Some(bold)) if code < bold => (code, true),
            (Some(code), None) => (code, true),
            (Some(_), Some(bold)) => (bold, false),
            (None, Some(bold)) => (bold, false),
            (None, None) => break,
        };
        if index > 0 {
            spans.push(Span::styled(rest[..index].to_string(), base));
        }
        if is_code {
            let after = &rest[index + 1..];
            if let Some(end) = after.find('`') {
                spans.push(Span::styled(
                    after[..end].to_string(),
                    Style::default().fg(YELLOW).bg(PANEL),
                ));
                rest = &after[end + 1..];
            } else {
                spans.push(Span::styled(rest[index..].to_string(), base));
                rest = "";
            }
        } else {
            let after = &rest[index + 2..];
            if let Some(end) = after.find("**") {
                spans.push(Span::styled(
                    after[..end].to_string(),
                    base.add_modifier(Modifier::BOLD),
                ));
                rest = &after[end + 2..];
            } else {
                spans.push(Span::styled(rest[index..].to_string(), base));
                rest = "";
            }
        }
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_string(), base));
    }
    spans
}

fn markdown_lite(text: &str) -> Vec<Line<'static>> {
    let body = Style::default().fg(TEXT);
    let mut lines = Vec::new();
    let mut in_code = false;
    for raw in text.split('\n') {
        let trimmed = raw.trim_start();
        if let Some(language) = trimmed.strip_prefix("```") {
            if in_code {
                lines.push(Line::from(Span::styled("╰─", Style::default().fg(FAINT))));
                in_code = false;
            } else {
                let label = language.trim();
                let label = if label.is_empty() { "code" } else { label };
                lines.push(Line::from(Span::styled(
                    format!("╭─ {label}"),
                    Style::default().fg(DIM),
                )));
                in_code = true;
            }
            continue;
        }
        if in_code {
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(FAINT)),
                Span::styled(
                    raw.to_string(),
                    Style::default().fg(Color::Rgb(205, 210, 220)).bg(PANEL),
                ),
            ]));
            continue;
        }
        if trimmed.is_empty() {
            lines.push(Line::from(""));
            continue;
        }
        let hashes = trimmed.chars().take_while(|ch| *ch == '#').count();
        if hashes > 0 && trimmed[hashes..].starts_with(' ') {
            lines.push(Line::from(vec![
                Span::styled("▌ ", Style::default().fg(ACCENT)),
                Span::styled(
                    trimmed[hashes + 1..].to_string(),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
            ]));
            continue;
        }
        if let Some(item) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            let mut spans = vec![Span::styled("• ", Style::default().fg(ACCENT))];
            spans.extend(inline_spans(item, body));
            lines.push(Line::from(spans));
            continue;
        }
        let digits = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).count();
        if digits > 0 && trimmed[digits..].starts_with(". ") {
            let mut spans = vec![Span::styled(
                format!("{}. ", &trimmed[..digits]),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )];
            spans.extend(inline_spans(&trimmed[digits + 2..], body));
            lines.push(Line::from(spans));
            continue;
        }
        if let Some(quote) = trimmed.strip_prefix("> ") {
            let mut spans = vec![Span::styled("▏ ", Style::default().fg(FAINT))];
            spans.extend(inline_spans(
                quote,
                Style::default().fg(DIM).add_modifier(Modifier::ITALIC),
            ));
            lines.push(Line::from(spans));
            continue;
        }
        lines.push(Line::from(inline_spans(raw, body)));
    }
    if in_code {
        lines.push(Line::from(Span::styled("╰─", Style::default().fg(FAINT))));
    }
    lines
}

fn chat_hash(line: &ChatLine) -> u64 {
    let mut hasher = DefaultHasher::new();
    line.role.hash(&mut hasher);
    line.streaming.hash(&mut hasher);
    line.text.hash(&mut hasher);
    hasher.finish()
}

fn chat_lines(line: &ChatLine, width: u16, show_label: bool) -> Vec<Line<'static>> {
    if line.role == ChatRole::System && line.text.starts_with("turn end:") {
        return Vec::new();
    }
    let parts: Vec<&str> = line.text.split('\n').collect();
    let mut rendered = Vec::new();
    match line.role {
        ChatRole::User => {
            rendered.push(Line::from(Span::styled(
                format!("  {}", i18n::t("role.you")),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            )));
            for (index, part) in parts.iter().enumerate() {
                let prefix = if index == 0 { "❯ " } else { "  " };
                let mut spans = vec![
                    Span::styled(prefix, Style::default().fg(ACCENT).bg(USER_BG)),
                    Span::styled((*part).to_string(), Style::default().fg(TEXT).bg(USER_BG)),
                ];
                let used = 2 + UnicodeWidthStr::width(*part);
                // A compact bubble reads better than a full-width band; keep
                // two cells of right padding and let the rest of the row show
                // the normal transcript background.
                let target = used.saturating_add(2).min(width as usize);
                if target > used {
                    spans.push(Span::styled(
                        " ".repeat(target - used),
                        Style::default().bg(USER_BG),
                    ));
                }
                rendered.push(Line::from(spans));
            }
        }
        ChatRole::Assistant => {
            if show_label {
                rendered.push(Line::from(Span::styled(
                    "  Mitnick",
                    Style::default().fg(AGENT).add_modifier(Modifier::BOLD),
                )));
            }
            let markdown = markdown_lite(&line.text);
            let last = markdown.len().saturating_sub(1);
            for (index, rendered_line) in markdown.into_iter().enumerate() {
                let mut spans = vec![Span::styled("│ ", Style::default().fg(FAINT))];
                spans.extend(rendered_line.spans);
                if line.streaming && index == last {
                    spans.push(Span::styled("▍", Style::default().fg(ACCENT)));
                }
                rendered.push(Line::from(spans));
            }
        }
        ChatRole::Tool => {
            if show_label {
                rendered.push(Line::from(Span::styled(
                    "  Mitnick",
                    Style::default().fg(AGENT).add_modifier(Modifier::BOLD),
                )));
            }
            let first = line.text.chars().next().unwrap_or('·');
            let color = match first {
                '✓' => GREEN,
                '✗' => RED,
                '▸' => TOOL,
                _ => DIM,
            };
            let body = line.text[first.len_utf8()..].trim_start();
            let (name, detail) = body.split_once(' ').unwrap_or((body, ""));
            let name_color = if first == '✗' { RED } else { TEXT };
            let mut spans = vec![
                Span::styled(format!("  {first} "), Style::default().fg(color)),
                Span::styled(
                    name.to_string(),
                    Style::default().fg(name_color).add_modifier(Modifier::BOLD),
                ),
            ];
            if !detail.trim().is_empty() {
                spans.push(Span::styled(
                    format!("  {}", detail.trim()),
                    Style::default().fg(DIM),
                ));
            }
            rendered.push(Line::from(spans));
        }
        ChatRole::Meta => {
            let label = format!(" ✓ {} ", truncate(&line.text, width as usize));
            let label_width = UnicodeWidthStr::width(label.as_str());
            if label_width < width as usize {
                let left = (width as usize - label_width) / 2;
                let right = width as usize - label_width - left;
                rendered.push(Line::from(vec![
                    Span::styled("─".repeat(left), Style::default().fg(FAINT)),
                    Span::styled(label, Style::default().fg(DIM)),
                    Span::styled("─".repeat(right), Style::default().fg(FAINT)),
                ]));
            } else {
                rendered.push(Line::from(Span::styled(label, Style::default().fg(DIM))));
            }
        }
        ChatRole::System => {
            if let Some(rest) = line.text.strip_prefix("⧗ ") {
                let color = if rest.contains("denied") {
                    RED
                } else if rest.contains("approved") {
                    GREEN
                } else {
                    YELLOW
                };
                rendered.push(Line::from(vec![
                    Span::styled("  ⧗ ", Style::default().fg(color)),
                    Span::styled(rest.to_string(), Style::default().fg(color)),
                ]));
            } else {
                let is_error = line.text.starts_with('✗');
                for (index, part) in parts.iter().enumerate() {
                    if is_error && index == 0 {
                        rendered.push(Line::from(vec![
                            Span::styled("  ▌ ", Style::default().fg(RED)),
                            Span::styled(
                                (*part).to_string(),
                                Style::default()
                                    .fg(RED)
                                    .bg(ERR_BG)
                                    .add_modifier(Modifier::BOLD),
                            ),
                        ]));
                    } else {
                        rendered.push(Line::from(Span::styled(
                            format!("    {part}"),
                            Style::default().fg(FAINT).add_modifier(Modifier::ITALIC),
                        )));
                    }
                }
            }
        }
    }
    let inline = (line.role == ChatRole::Tool && line.text.starts_with('▸'))
        || (line.role == ChatRole::System && line.text.starts_with('⧗'));
    if !inline {
        rendered.push(Line::from(""));
    }
    rendered
}

fn group_label_needed(previous: Option<ChatRole>, role: ChatRole) -> bool {
    matches!(role, ChatRole::Assistant | ChatRole::Tool)
        && !previous.is_some_and(|role| matches!(role, ChatRole::Assistant | ChatRole::Tool))
}

fn build_chat_group(line: &ChatLine, width: u16, show_label: bool) -> ChatGroup {
    let lines = chat_lines(line, width, show_label);
    let height = if lines.is_empty() {
        0
    } else {
        Paragraph::new(Text::from(lines.clone()))
            .wrap(Wrap { trim: false })
            .line_count(width)
            .max(1)
    };
    ChatGroup {
        hash: chat_hash(line),
        role: line.role,
        show_label,
        lines,
        height,
    }
}

fn build_chat_groups(selected: &[&ChatLine], width: u16) -> Vec<ChatGroup> {
    let mut groups: Vec<ChatGroup> = Vec::with_capacity(selected.len());
    let mut last_content: Option<ChatRole> = None;
    for line in selected {
        let show_label = group_label_needed(last_content, line.role);
        if matches!(
            line.role,
            ChatRole::User | ChatRole::Assistant | ChatRole::Tool
        ) {
            last_content = Some(line.role);
        }
        groups.push(build_chat_group(line, width, show_label));
    }
    groups
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
    if message.contains("function arguments") || message.contains("tool arguments") {
        text.push('\n');
        text.push_str(i18n::t("chat.hint.tool_args"));
    } else if message.contains("reasoning_text") || message.contains("thinking mode") {
        text.push('\n');
        text.push_str(i18n::t("chat.hint.reasoning"));
    } else if message.contains("LLM call failed")
        || message.contains("model_not_found")
        || message.contains("HTTP 4")
        || message.contains("HTTP 5")
    {
        text.push('\n');
        text.push_str(i18n::t("chat.hint.check_model"));
    }
    text
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ChatRole {
    User,
    Assistant,
    Tool,
    System,
    Meta,
}

#[derive(Debug, Clone)]
struct ChatLine {
    task_id: String,
    role: ChatRole,
    text: String,
    streaming: bool,
}

struct ChatGroup {
    hash: u64,
    role: ChatRole,
    show_label: bool,
    lines: Vec<Line<'static>>,
    height: usize,
}

#[derive(Default)]
struct TranscriptCache {
    task: String,
    width: u16,
    revision: u64,
    groups: Vec<ChatGroup>,
    prefix: Vec<usize>,
}

fn append_agent_text(chat: &mut Vec<ChatLine>, task_id: &str, text: &str, delta: bool) {
    if task_id.is_empty() || text.is_empty() {
        return;
    }
    if delta {
        // Only extend the assistant line if it is still the newest line for
        // this task. Text that arrives after a tool call belongs to a new
        // model step, not to the paragraph before the tool ran.
        let last = chat.iter().rposition(|line| line.task_id == task_id);
        if let Some(index) = last
            && chat[index].role == ChatRole::Assistant
            && chat[index].streaming
        {
            chat[index].text.push_str(text);
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
    for line in chat
        .iter_mut()
        .filter(|line| line.task_id == task_id && line.role == ChatRole::Assistant)
    {
        line.streaming = false;
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "msk-tui",
    version,
    about = "interactive security and network operations workbench"
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
    /// Diagnostic: render the settings overlay in a snapshot.
    #[arg(long, hide = true)]
    snapshot_settings: bool,
    /// Diagnostic: render the language picker in a snapshot.
    #[arg(long, hide = true)]
    snapshot_language: bool,
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
                "MitnickSolve event viewer

                 q          quit
                 j / k      move the selection
                 g / G      first / last event
                 tab        cycle the log filter
                 1 - 5      All / LLM / Tools / Artifacts / Context
                 /          search (Enter applies, Esc clears)
                 f          follow the newest event
                 r          reload the log from disk
                 ?          toggle this help

                 The log is read-only. Run msk-tui without --events for the
                 conversational workbench.",
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
    composer_width: Cell<u16>,
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
    last_initial_try: Instant,
    scroll_top: u16,
    max_scroll: Cell<u16>,
    auto_scroll: bool,
    transcript_loaded: Option<String>,
    transcript_cache: RefCell<TranscriptCache>,
    language: i18n::Language,
    show_settings: bool,
    settings_selection: usize,
    show_language: bool,
    language_selection: usize,
    chat_revision: u64,
    events_epoch: Cell<u64>,
    usage_cache: RefCell<Option<(String, u64, (u64, usize))>>,
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
    findings_max_scroll: Cell<usize>,
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
        let language = std::env::var("MSK_LANG")
            .ok()
            .and_then(|value| i18n::Language::parse(&value))
            .or_else(|| i18n::Language::parse(&config.language))
            .unwrap_or_else(i18n::Language::detect);
        i18n::set_language(language);
        if !modes.iter().any(|mode| mode.id == config.mode) {
            config.mode = default_mode();
        }
        if !PERMISSIONS.contains(&config.permission.as_str()) {
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
            composer_width: Cell::new(0),
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
            last_initial_try: Instant::now(),
            scroll_top: 0,
            max_scroll: Cell::new(0),
            auto_scroll: true,
            transcript_loaded: None,
            transcript_cache: RefCell::new(TranscriptCache::default()),
            language,
            show_settings: false,
            settings_selection: 0,
            show_language: false,
            language_selection: 0,
            chat_revision: 0,
            events_epoch: Cell::new(0),
            usage_cache: RefCell::new(None),
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
            findings_max_scroll: Cell::new(0),
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
            Duration::from_millis(200),
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
            Duration::from_millis(200),
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
            language: self.language.id().to_string(),
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

    fn refresh(&mut self) -> Result<bool> {
        let mut changed = false;
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
                                    "tool.call" if event.actor == "agent" => {
                                        finish_streaming(&mut self.chat, &task_id);
                                        self.touch_chat();
                                        let tool = event
                                            .data
                                            .get("tool")
                                            .and_then(Value::as_str)
                                            .unwrap_or("tool");
                                        let args =
                                            event.data.get("args").cloned().unwrap_or(Value::Null);
                                        self.push_chat(ChatLine {
                                            task_id: task_id.clone(),
                                            role: ChatRole::Tool,
                                            text: format!("▸ {tool} {}", tool_summary(tool, &args)),
                                            streaming: false,
                                        });
                                    }
                                    "tool.result" if event.actor == "agent" => {
                                        finish_streaming(&mut self.chat, &task_id);
                                        self.touch_chat();
                                        let tool = event
                                            .data
                                            .get("tool")
                                            .and_then(Value::as_str)
                                            .unwrap_or("tool");
                                        let ok = event
                                            .data
                                            .get("ok")
                                            .and_then(Value::as_bool)
                                            .unwrap_or(true)
                                            && !event
                                                .data
                                                .get("is_error")
                                                .and_then(Value::as_bool)
                                                .unwrap_or(false);
                                        let mut text =
                                            format!("{} {tool}", if ok { "✓" } else { "✗" });
                                        if let Some(artifact) =
                                            event.data.get("artifact").and_then(Value::as_str)
                                        {
                                            text.push_str(&format!(
                                                "  artifact {}",
                                                short_id(artifact)
                                            ));
                                            if let Some(bytes) = artifact_bytes(&event.data) {
                                                text.push_str(&format!(" · {bytes} B"));
                                            }
                                        } else if let Some(summary) =
                                            observation_summary(tool, &event.data)
                                        {
                                            text.push_str(&format!("  {summary}"));
                                        }
                                        self.push_chat(ChatLine {
                                            task_id: task_id.clone(),
                                            role: ChatRole::Tool,
                                            text,
                                            streaming: false,
                                        });
                                    }
                                    "mode.switch" if event.actor == "agent" => {
                                        let mode = event
                                            .data
                                            .get("mode")
                                            .and_then(Value::as_str)
                                            .unwrap_or_default();
                                        self.push_chat(ChatLine {
                                            task_id: task_id.clone(),
                                            role: ChatRole::System,
                                            text: format!(
                                                "⧗ {}",
                                                i18n::tf(
                                                    "modes.switched",
                                                    &[&self.mode_label(mode)]
                                                )
                                            ),
                                            streaming: false,
                                        });
                                    }
                                    "tool.error" if event.actor == "agent" => {
                                        finish_streaming(&mut self.chat, &task_id);
                                        self.touch_chat();
                                        let tool = event
                                            .data
                                            .get("tool")
                                            .and_then(Value::as_str)
                                            .unwrap_or("tool");
                                        let error = event
                                            .data
                                            .get("error")
                                            .and_then(Value::as_str)
                                            .unwrap_or("tool failed");
                                        self.push_chat(ChatLine {
                                            task_id: task_id.clone(),
                                            role: ChatRole::Tool,
                                            text: format!("✗ {tool}  {error}"),
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
                            self.touch_chat();
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
                            match self.client.request_with_timeout(
                                methods::TASK_MESSAGE,
                                json!({"task_id": task_id, "text": text}),
                                Duration::from_secs(5),
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
                            let task_id = pending.get("task_id").and_then(Value::as_str);
                            if task_id.is_some() && self.selected_task_id().as_deref() == task_id {
                                let tool = pending
                                    .get("tool")
                                    .and_then(Value::as_str)
                                    .unwrap_or("tool");
                                self.push_chat(ChatLine {
                                    task_id: task_id.unwrap_or_default().to_string(),
                                    role: ChatRole::System,
                                    text: format!("⧗ {}", i18n::tf("approval.requested", &[tool])),
                                    streaming: false,
                                });
                                self.trim_chat();
                            }
                        }
                    }
                }
                Some("approval.resolved") => {
                    if let Some(resolved) = value.get("params") {
                        let tool = self
                            .approvals
                            .iter()
                            .find(|approval| {
                                approval.get("task_id") == resolved.get("task_id")
                                    && approval.get("id") == resolved.get("id")
                            })
                            .and_then(|approval| approval.get("tool"))
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        self.approvals.retain(|approval| {
                            approval.get("task_id") != resolved.get("task_id")
                                || approval.get("id") != resolved.get("id")
                        });
                        if let Some(approved) = resolved.get("approved").and_then(Value::as_bool)
                            && resolved
                                .get("task_id")
                                .and_then(Value::as_str)
                                .is_some_and(|task| {
                                    self.selected_task_id().as_deref() == Some(task)
                                })
                        {
                            let outcome = if approved {
                                i18n::t("approval.approved")
                            } else {
                                i18n::t("approval.denied")
                            };
                            let note = match &tool {
                                Some(tool) => format!("⧗ {outcome} · {tool}"),
                                None => format!("⧗ {outcome}"),
                            };
                            self.push_chat(ChatLine {
                                task_id: resolved
                                    .get("task_id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_string(),
                                role: ChatRole::System,
                                text: note,
                                streaming: false,
                            });
                            self.trim_chat();
                        }
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
                        self.push_chat(ChatLine {
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
                    self.touch_chat();
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
                    self.touch_chat();
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
            changed = true;
        }

        if self.events.len() > MAX_EVENTS {
            let excess = self.events.len() - MAX_EVENTS;
            self.events.drain(0..excess);
        }

        if let Some(data) = self.terminal_data.take() {
            let mut closed = false;
            let mut credits = 0usize;
            while let Some(event) = data.try_event() {
                changed = true;
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

        if self.last_refresh.elapsed() >= Duration::from_secs(3) {
            // Push notifications are the primary source of task/approval state;
            // polling is only a fallback. Keep each round trip short so a busy
            // daemon cannot freeze input for a second at a time.
            match self.reload_tasks() {
                Ok(()) => changed = true,
                Err(err) if is_timeout_error(&err) => {}
                Err(err) => self.status = format!("refresh failed: {err}"),
            }
            match self.reload_approvals() {
                Ok(()) => changed = true,
                Err(err) if is_timeout_error(&err) => {}
                Err(err) => self.status = format!("approval refresh failed: {err}"),
            }
            self.last_refresh = Instant::now();
        }
        self.flush_pending_initial();
        if changed {
            self.touch_events();
        }
        Ok(changed)
    }

    fn append_history_event(&mut self, task_id: &str, event: &Event) {
        let data = &event.data;
        match event.kind.as_str() {
            "turn.start" => {
                if let Some(user) = data.get("user").and_then(Value::as_str) {
                    self.push_chat(ChatLine {
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
                    self.push_chat(ChatLine {
                        task_id: task_id.to_string(),
                        role: ChatRole::Assistant,
                        text: text.to_string(),
                        streaming: false,
                    });
                }
            }
            "tool.call" => {
                if event.actor != "agent" {
                    return;
                }
                let tool = data.get("tool").and_then(Value::as_str).unwrap_or("tool");
                let args = data.get("args").cloned().unwrap_or(Value::Null);
                self.push_chat(ChatLine {
                    task_id: task_id.to_string(),
                    role: ChatRole::Tool,
                    text: format!("▸ {tool} {}", tool_summary(tool, &args)),
                    streaming: false,
                });
            }
            "tool.result" => {
                if event.actor != "agent" {
                    return;
                }
                let tool = data.get("tool").and_then(Value::as_str).unwrap_or("tool");
                let failed = data
                    .get("is_error")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || data.get("ok").and_then(Value::as_bool) == Some(false);
                let mut text = format!("{} {tool}", if failed { "✗" } else { "✓" });
                if let Some(artifact) = data.get("artifact").and_then(Value::as_str) {
                    text.push_str(&format!("  artifact {}", short_id(artifact)));
                    if let Some(bytes) = artifact_bytes(data) {
                        text.push_str(&format!(" · {bytes} B"));
                    }
                } else if let Some(summary) = observation_summary(tool, data) {
                    text.push_str(&format!("  {summary}"));
                }
                self.push_chat(ChatLine {
                    task_id: task_id.to_string(),
                    role: ChatRole::Tool,
                    text,
                    streaming: false,
                });
            }
            "tool.error" => {
                if event.actor != "agent" {
                    return;
                }
                let tool = data.get("tool").and_then(Value::as_str).unwrap_or("tool");
                let error = data
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("tool failed");
                self.push_chat(ChatLine {
                    task_id: task_id.to_string(),
                    role: ChatRole::Tool,
                    text: format!("✗ {tool}  {error}"),
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
                    self.push_chat(ChatLine {
                        task_id: task_id.to_string(),
                        role: ChatRole::System,
                        text: turn_error_text(status, message),
                        streaming: false,
                    });
                } else {
                    let task = self
                        .tasks
                        .iter()
                        .find(|task| task.get("task_id").and_then(Value::as_str) == Some(task_id));
                    let provider = task
                        .and_then(|task| task.get("provider"))
                        .and_then(Value::as_str)
                        .unwrap_or(&self.provider)
                        .to_string();
                    let model = task
                        .and_then(|task| task.get("model"))
                        .and_then(Value::as_str)
                        .filter(|model| !model.is_empty())
                        .unwrap_or("-")
                        .to_string();
                    let effort = task
                        .and_then(|task| task.get("effort"))
                        .and_then(Value::as_str)
                        .filter(|effort| !effort.is_empty())
                        .unwrap_or("default")
                        .to_string();
                    let mut text = format!(
                        "{} · {} · {provider} · {model} · {}",
                        i18n::t("chat.turn_complete"),
                        short_time(&event.ts),
                        i18n::tf("chat.effort", &[&i18n::effort_label(&effort)])
                    );
                    if let Some(ms) = data.get("duration_ms").and_then(Value::as_u64) {
                        text.push_str(&format!(" · {:.1}s", ms as f64 / 1000.0));
                    }
                    self.push_chat(ChatLine {
                        task_id: task_id.to_string(),
                        role: ChatRole::Meta,
                        text,
                        streaming: false,
                    });
                }
            }
            "mode.switch" => {
                let mode = data.get("mode").and_then(Value::as_str).unwrap_or_default();
                self.push_chat(ChatLine {
                    task_id: task_id.to_string(),
                    role: ChatRole::System,
                    text: format!(
                        "⧗ {}",
                        i18n::tf("modes.switched", &[&self.mode_label(mode)])
                    ),
                    streaming: false,
                });
            }
            "approval.request" => {
                let tool = data.get("tool").and_then(Value::as_str).unwrap_or("tool");
                self.push_chat(ChatLine {
                    task_id: task_id.to_string(),
                    role: ChatRole::System,
                    text: format!("⧗ {}", i18n::tf("approval.requested", &[tool])),
                    streaming: false,
                });
            }
            "approval.result" => {
                let approved = data
                    .get("approved")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.push_chat(ChatLine {
                    task_id: task_id.to_string(),
                    role: ChatRole::System,
                    text: format!(
                        "⧗ {}",
                        if approved {
                            i18n::t("approval.approved")
                        } else {
                            i18n::t("approval.denied")
                        }
                    ),
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
            Duration::from_secs(2),
        ) {
            Ok(result) => result,
            Err(_) => match self.client.request_with_timeout(
                methods::TASK_RESULT,
                json!({"task_id": task_id}),
                Duration::from_secs(2),
            ) {
                Ok(result) => result,
                Err(err) => {
                    self.status = format!("load transcript failed: {err}");
                    return;
                }
            },
        };
        let previous_scroll = if self.transcript_loaded.as_deref() == Some(task_id) {
            Some((self.auto_scroll, self.scroll_top))
        } else {
            None
        };
        self.chat.retain(|line| line.task_id != task_id);
        self.touch_chat();
        if let Some(history) = result.get("history").and_then(Value::as_array) {
            let mut known: HashSet<String> = self
                .events
                .iter()
                .filter(|(id, _)| id == task_id)
                .map(|(_, event)| event.id.clone())
                .collect();
            for event in history {
                if let Ok(event) = serde_json::from_value::<Event>(event.clone()) {
                    // The event inspector and footer usage should include the
                    // recorded history of an opened session, not just events
                    // that arrive after this client connected.
                    if known.insert(event.id.clone()) {
                        self.events.push((task_id.to_string(), event.clone()));
                    }
                    self.append_history_event(task_id, &event);
                }
            }
            // Merge live and recorded history in sequence order so the event
            // inspector and the footer usage read the newest context state.
            self.events.sort_by_key(|(_, event)| event.seq);
            if self.events.len() > MAX_EVENTS {
                let excess = self.events.len() - MAX_EVENTS;
                self.events.drain(0..excess);
            }
            self.touch_events();
            self.trim_chat();
            self.transcript_loaded = Some(task_id.to_string());
            match previous_scroll {
                Some((auto_scroll, offset)) => {
                    self.auto_scroll = auto_scroll;
                    self.scroll_top = offset;
                }
                None => self.follow_output(),
            }
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
            self.push_chat(ChatLine {
                task_id: task_id.to_string(),
                role,
                text,
                streaming: false,
            });
        }
        self.trim_chat();
        self.transcript_loaded = Some(task_id.to_string());
        match previous_scroll {
            Some((auto_scroll, offset)) => {
                self.auto_scroll = auto_scroll;
                self.scroll_top = offset;
            }
            None => self.follow_output(),
        }
    }

    fn open_sessions(&mut self) {
        self.composing = false;
        self.show_sessions = true;
        self.session_selection = if self.new_task || self.tasks.is_empty() {
            0
        } else {
            (self.selected_task + 1).min(self.tasks.len())
        };
        self.status = format!(
            "{} · {}",
            i18n::t("sessions.title"),
            i18n::t("sessions.keys")
        );
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
        if self.last_initial_try.elapsed() < Duration::from_millis(500) {
            return;
        }
        self.last_initial_try = Instant::now();
        let waiting = self.tasks.iter().any(|task| {
            task.get("task_id").and_then(Value::as_str) == Some(task_id.as_str())
                && task.get("state").and_then(Value::as_str) == Some("waiting_input")
        });
        if !waiting {
            return;
        }
        match self.client.request_with_timeout(
            methods::TASK_MESSAGE,
            json!({"task_id": task_id, "text": text}),
            Duration::from_secs(5),
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

    fn scroll_up(&mut self, lines: u16) {
        let limit = self.max_scroll.get();
        if limit == 0 {
            return;
        }
        if self.auto_scroll {
            self.scroll_top = limit;
            self.auto_scroll = false;
        }
        self.scroll_top = self.scroll_top.saturating_sub(lines);
    }

    fn scroll_down(&mut self, lines: u16) {
        if self.auto_scroll {
            return;
        }
        let limit = self.max_scroll.get();
        self.scroll_top = self.scroll_top.saturating_add(lines).min(limit);
        if self.scroll_top >= limit {
            self.auto_scroll = true;
        }
    }

    fn scroll_to_top(&mut self) {
        self.auto_scroll = false;
        self.scroll_top = 0;
    }

    fn follow_output(&mut self) {
        self.auto_scroll = true;
        self.scroll_top = 0;
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

    fn selected_usage(&self) -> (u64, usize) {
        let Some(task_id) = self.selected_task_id() else {
            return (0, 0);
        };
        let epoch = self.events_epoch.get();
        if let Some((cached_task, cached_epoch, usage)) = self.usage_cache.borrow().as_ref()
            && cached_task == &task_id
            && *cached_epoch == epoch
        {
            return *usage;
        }
        let mut tokens = 0;
        let mut tools = 0;
        for (id, event) in &self.events {
            if id != &task_id {
                continue;
            }
            match event.kind.as_str() {
                "context.state" => {
                    if let Some(value) = event.data.get("approx_tokens").and_then(Value::as_u64) {
                        tokens = value;
                    }
                }
                "tool.call" => tools += 1,
                _ => {}
            }
        }
        *self.usage_cache.borrow_mut() = Some((task_id, epoch, (tokens, tools)));
        (tokens, tools)
    }

    fn touch_events(&self) {
        self.events_epoch
            .set(self.events_epoch.get().wrapping_add(1));
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

    fn selected_checkpoint(&self) -> Option<PathBuf> {
        let path = self
            .tasks
            .get(self.selected_task)
            .and_then(|task| task.get("checkpoint"))
            .and_then(Value::as_str)?;
        let path = PathBuf::from(path);
        path.is_file().then_some(path)
    }

    fn any_running(&self) -> bool {
        self.tasks
            .iter()
            .any(|task| task.get("state").and_then(Value::as_str) == Some("running"))
    }

    fn selected_mode(&self) -> String {
        self.tasks
            .get(self.selected_task)
            .and_then(|task| task.get("mode"))
            .and_then(Value::as_str)
            .unwrap_or(&self.mode)
            .to_string()
    }

    /// The mode the session was requested with. This may differ from the
    /// resolved `mode` when auto routing picked a concrete mode.
    fn selected_requested_mode(&self) -> String {
        self.tasks
            .get(self.selected_task)
            .and_then(|task| task.get("requested_mode"))
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
            self.status = i18n::tf(
                "providers.next_current",
                &[&self.provider, &self.display_provider()],
            );
        } else {
            self.status = i18n::tf("providers.next", &[&self.provider]);
        }
    }

    fn apply_mode_to_session(&mut self, requested: &str) -> bool {
        let Some(task_id) = self.selected_task_id() else {
            return false;
        };
        if self.new_task || !matches!(self.selected_state(), "running" | "waiting_input") {
            return false;
        }
        match self.client.request_with_timeout(
            methods::TASK_MODE,
            json!({"task_id": task_id, "mode": requested}),
            Duration::from_secs(5),
        ) {
            Ok(snapshot) => {
                if let Some(existing) = self.tasks.iter_mut().find(|task| {
                    task.get("task_id").and_then(Value::as_str) == Some(task_id.as_str())
                }) {
                    *existing = snapshot;
                }
                self.mode = requested.to_string();
                self.save_config();
                let resolved = self.selected_mode();
                self.status = i18n::tf("modes.applied", &[&self.mode_label(&resolved)]);
                true
            }
            Err(err) => {
                self.status = format!("mode switch failed: {err}");
                false
            }
        }
    }

    fn announce_mode_saved(&mut self) {
        let pending = self.mode_label(&self.mode);
        if self.new_task || self.selected_task_id().is_none() {
            self.status = i18n::tf("modes.next", &[&pending]);
        } else {
            let current = self.mode_label(&self.selected_mode());
            self.status = i18n::tf("modes.next_current", &[&pending, &current]);
        }
    }

    fn announce_permission_saved(&mut self) {
        let pending = permission_label(&self.permission);
        if self.new_task || self.selected_task_id().is_none() {
            self.status = i18n::tf("permissions.next", &[&pending]);
        } else {
            let current = permission_label(&self.display_permission());
            self.status = i18n::tf("permissions.next_current", &[&pending, &current]);
        }
    }

    fn cycle_effort(&mut self, delta: isize) {
        let index = effort_index(&self.effort) as isize;
        let length = EFFORTS.len() as isize;
        let next = (index + delta).rem_euclid(length) as usize;
        self.effort = EFFORTS[next].0.to_string();
        self.save_config();
        let pending = i18n::effort_label(&self.effort);
        if self.new_task || self.selected_task_id().is_none() {
            self.status = i18n::tf("effort.next", &[&pending]);
        } else {
            let current = i18n::effort_label(&self.display_effort());
            self.status = i18n::tf("effort.next_current", &[&pending, &current]);
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
        if let Some(label) = i18n::mode_label(mode) {
            return label.to_string();
        }
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

    fn touch_chat(&mut self) {
        self.chat_revision = self.chat_revision.wrapping_add(1);
    }

    fn push_chat(&mut self, line: ChatLine) {
        self.chat.push(line);
        self.chat_revision = self.chat_revision.wrapping_add(1);
    }

    fn trim_chat(&mut self) {
        if self.chat.len() > MAX_CHAT_LINES {
            let excess = self.chat.len() - MAX_CHAT_LINES;
            self.chat.drain(0..excess);
            self.touch_chat();
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
        let state = self.selected_state();
        if !matches!(state, "running" | "waiting_input")
            && let Some(checkpoint) = self.selected_checkpoint()
        {
            // Continue with the mode shown for the saved session, not whatever
            // was last chosen for new sessions.
            let requested = self.selected_requested_mode();
            let previous = std::mem::replace(&mut self.mode, requested);
            self.create_task_with_options(&text, true, Some(&checkpoint));
            self.mode = previous;
            return;
        }
        let Some(task_id) = self.selected_task_id() else {
            self.status = "no task selected".into();
            return;
        };
        match self.client.request_with_timeout(
            methods::TASK_MESSAGE,
            json!({"task_id": task_id, "text": text}),
            Duration::from_secs(5),
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
                self.save_config();
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
                    if !self.apply_mode_to_session(&mode) {
                        self.mode = mode;
                        self.save_config();
                        self.announce_mode_saved();
                    }
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
                let found = PERMISSIONS.iter().find(|id| {
                    id.eq_ignore_ascii_case(value)
                        || i18n::permission_label(id).eq_ignore_ascii_case(value)
                });
                if let Some(id) = found {
                    self.permission = (*id).to_string();
                    self.save_config();
                    self.announce_permission_saved();
                } else {
                    self.status = "usage: /permissions auto|manual|bypass".into();
                }
            }
            "/settings" => self.open_settings(),
            "/language" if argument.is_empty() => self.open_language(),
            "/language" => {
                if let Some(language) = i18n::Language::parse(argument) {
                    self.apply_language(language);
                } else {
                    self.status = format!(
                        "{}: en, zh-CN, ja, es, fr, de",
                        i18n::t("settings.language")
                    );
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
                        "{} · {}",
                        i18n::tf("findings.count", &[&self.findings.len().to_string()]),
                        i18n::t("findings.keys")
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
                    self.touch_chat();
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
        let result = self.client.request_with_timeout(
            methods::TASK_CREATE,
            {
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
            },
            Duration::from_secs(15),
        );
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
        match self.client.request_with_timeout(
            methods::TASK_INTERRUPT,
            json!({"task_id": task_id}),
            Duration::from_secs(5),
        ) {
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
        match self.client.request_with_timeout(
            methods::APPROVAL_RESPOND,
            json!({"task_id": task_id, "id": id, "approved": approved}),
            Duration::from_secs(5),
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
        self.input_cursor = char_boundary_at_or_before(&self.input, self.input_cursor);
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.input.insert_str(self.input_cursor, &normalized);
        self.input_cursor += normalized.len();
        self.history_index = None;
        self.status = "pasted".into();
    }

    fn compose_key(&mut self, key: KeyEvent) -> bool {
        self.input_cursor = char_boundary_at_or_before(&self.input, self.input_cursor);
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
                KeyCode::Up => self.history_previous(),
                KeyCode::Down => self.history_next(),
                KeyCode::Home => self.scroll_to_top(),
                KeyCode::End => self.follow_output(),
                KeyCode::PageUp => self.scroll_up(20),
                KeyCode::PageDown => self.scroll_down(20),
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
            KeyCode::Up if key.modifiers.contains(KeyModifiers::ALT) => self.history_previous(),
            KeyCode::Down if key.modifiers.contains(KeyModifiers::ALT) => self.history_next(),
            KeyCode::Up if self.input_has_multiple_rows() => self.move_input_cursor_vertical(true),
            KeyCode::Down if self.input_has_multiple_rows() => {
                self.move_input_cursor_vertical(false)
            }
            KeyCode::Up => self.scroll_up(1),
            KeyCode::Down => self.scroll_down(1),
            KeyCode::PageUp => self.scroll_up(10),
            KeyCode::PageDown => self.scroll_down(10),
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
            KeyCode::Home => self.input_cursor = self.input_line_bounds().0,
            KeyCode::End => self.input_cursor = self.input_line_bounds().1,
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
        self.status = i18n::t("palette.status").to_string();
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
                    .min(self.findings_max_scroll.get());
            }
            KeyCode::Home | KeyCode::Char('g') => self.findings_scroll = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.findings_scroll = self.findings_max_scroll.get();
            }
            KeyCode::Char('r') => match self.load_findings() {
                Ok(()) => {
                    self.status = i18n::tf("findings.count", &[&self.findings.len().to_string()]);
                }
                Err(err) => self.status = format!("findings failed: {err}"),
            },
            _ => {}
        }
        false
    }

    fn open_settings(&mut self) {
        self.composing = false;
        self.show_language = false;
        self.show_settings = true;
        self.settings_selection = 0;
        self.status = i18n::t("settings.keys").to_string();
    }

    fn open_language(&mut self) {
        self.composing = false;
        self.show_language = true;
        self.language_selection = self.language.index();
    }

    fn apply_language(&mut self, language: i18n::Language) {
        self.language = language;
        i18n::set_language(language);
        self.language_selection = language.index();
        self.show_language = false;
        self.save_config();
        self.touch_chat();
        if let Some(task_id) = self.selected_task_id() {
            self.load_transcript(&task_id);
        }
        self.status = if self.show_settings {
            i18n::t("settings.keys").to_string()
        } else {
            String::new()
        };
    }

    fn settings_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.show_settings = false;
                self.status.clear();
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.settings_selection = self.settings_selection.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.settings_selection = (self.settings_selection + 1).min(4);
            }
            KeyCode::Enter => match self.settings_selection {
                0 => {
                    self.show_language = true;
                    self.language_selection = self.language.index();
                }
                1 => {
                    self.show_settings = false;
                    self.permission_selection = permission_index(&self.permission);
                    self.show_permissions = true;
                }
                2 => {
                    self.show_settings = false;
                    self.mode_selection = self.mode_index(&self.mode);
                    self.show_modes = true;
                }
                3 => {
                    self.show_settings = false;
                    self.open_providers();
                }
                4 => {
                    self.show_settings = false;
                    self.open_setup();
                }
                _ => {}
            },
            _ => {}
        }
        false
    }

    fn language_key(&mut self, key: KeyEvent) -> bool {
        match key.code {
            KeyCode::Esc => {
                self.show_language = false;
                if self.show_settings {
                    self.status = i18n::t("settings.keys").to_string();
                } else {
                    self.status.clear();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.language_selection = self.language_selection.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.language_selection =
                    (self.language_selection + 1).min(i18n::ALL.len().saturating_sub(1));
            }
            KeyCode::Enter => {
                let index = self
                    .language_selection
                    .min(i18n::ALL.len().saturating_sub(1));
                self.apply_language(i18n::ALL[index]);
            }
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
            i18n::t("providers.none").into()
        } else {
            format!(
                "{}",
                i18n::tf("providers.count", &[&self.profiles.len().to_string()])
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
            self.status = i18n::tf("providers.selected_missing", &[&name]);
        } else if self.selected_task_id().is_some() && !self.new_task {
            self.status = i18n::tf("providers.next_current", &[&name, &self.display_provider()]);
        } else {
            self.status = i18n::tf("providers.next", &[&name]);
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
                    self.status = i18n::tf("providers.delete_armed", &[&name]);
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
                let requested = self.modes[self.mode_selection].id.clone();
                self.show_modes = false;
                if self.apply_mode_to_session(&requested) {
                    return false;
                }
                self.mode = requested;
                self.save_config();
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
                self.permission = PERMISSIONS[self.permission_selection].to_string();
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
                KeyCode::Up => self.scroll_up(1),
                KeyCode::Down => self.scroll_down(1),
                KeyCode::PageUp => self.scroll_up(10),
                KeyCode::PageDown => self.scroll_down(10),
                KeyCode::Home => self.scroll_to_top(),
                KeyCode::End => self.follow_output(),
                _ => {}
            }
            return false;
        }

        if self.show_language {
            return self.language_key(key);
        }

        if self.show_settings {
            return self.settings_key(key);
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
        if key.code == KeyCode::F(2) {
            let delta = if key.modifiers.contains(KeyModifiers::SHIFT) {
                -1
            } else {
                1
            };
            self.cycle_effort(delta);
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
            KeyCode::PageUp => self.scroll_up(10),
            KeyCode::PageDown => self.scroll_down(10),
            KeyCode::Up => self.scroll_up(3),
            KeyCode::Down => self.scroll_down(3),
            KeyCode::Home => self.scroll_to_top(),
            KeyCode::End => self.follow_output(),
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
        match self.client.request_with_timeout(
            methods::TASK_CANCEL,
            json!({"task_id": task_id}),
            Duration::from_secs(5),
        ) {
            Ok(_) => {
                self.push_chat(ChatLine {
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

    fn composer_text_width(&self) -> usize {
        let width = self.composer_width.get();
        if width == 0 { 80 } else { width as usize }
    }

    fn input_height(&self, input_width: u16) -> u16 {
        if !self.composing {
            return 5;
        }
        let text_width = input_width.saturating_sub(5).max(1) as usize;
        let rows = composer_layout(&self.input, self.input_cursor, text_width)
            .lines
            .len()
            .clamp(1, 8);
        (rows as u16 + 4).clamp(5, 12)
    }

    fn input_has_multiple_rows(&self) -> bool {
        composer_layout(&self.input, self.input_cursor, self.composer_text_width())
            .lines
            .len()
            > 1
    }

    fn move_input_cursor_vertical(&mut self, up: bool) {
        let layout = composer_layout(&self.input, self.input_cursor, self.composer_text_width());
        if layout.lines.len() <= 1 {
            return;
        }
        let target = if up {
            layout.cursor_row.saturating_sub(1)
        } else {
            (layout.cursor_row + 1).min(layout.lines.len() - 1)
        };
        if target == layout.cursor_row {
            return;
        }
        self.input_cursor = composer_offset(&layout, target, layout.cursor_col);
    }

    fn input_line_bounds(&self) -> (usize, usize) {
        let layout = composer_layout(&self.input, self.input_cursor, self.composer_text_width());
        let row = layout.cursor_row.min(layout.lines.len().saturating_sub(1));
        let start = layout.starts.get(row).copied().unwrap_or(0);
        let end = start + layout.lines.get(row).map(String::len).unwrap_or(0);
        (start, end.min(self.input.len()))
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
        let rail_width = if !home && area.width >= 100 { 24 } else { 0 };
        let content_area = Rect {
            x: area.x + rail_width,
            y: area.y,
            width: area.width.saturating_sub(rail_width),
            height: area.height,
        };
        if rail_width > 0 {
            self.draw_session_rail(
                frame,
                Rect {
                    x: area.x + 1,
                    y: area.y,
                    width: rail_width.saturating_sub(2),
                    height: area.height.saturating_sub(1),
                },
            );
        }

        let footer_area = Rect {
            x: area.x,
            y: area.bottom().saturating_sub(1),
            width: area.width,
            height: 1,
        };
        let input_width = if home {
            88.min(content_area.width.saturating_sub(6)).max(24)
        } else {
            104.min(content_area.width.saturating_sub(4)).max(24)
        };
        let input_x = content_area.x + content_area.width.saturating_sub(input_width) / 2;
        let input_height = self.input_height(input_width);
        let (input_area, hints_area) = if home {
            let target = content_area.y + ((content_area.height as u32 * 52 / 100) as u16);
            let input_y = target
                .min(content_area.bottom().saturating_sub(input_height + 2))
                .max(content_area.y + 1);
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
            self.draw_home(frame, content_area, input_area);
        } else {
            self.draw_chat(frame, content_area, input_area);
        }

        let focused = self.setup.is_none()
            && self.palette.is_none()
            && self.approvals.is_empty()
            && !self.show_sessions
            && !self.show_findings
            && !self.show_modes
            && !self.show_providers
            && !self.show_permissions
            && !self.show_settings
            && !self.show_language
            && !self.show_help;
        let modal = !focused;
        if !modal {
            self.draw_prompt(frame, input_area, focused);
            self.draw_slash_popup(frame, input_area);
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
        if self.show_settings {
            self.draw_settings(frame, area);
        }
        if self.show_language {
            self.draw_language(frame, area);
        }
        if self.setup.is_some() {
            self.draw_setup(frame);
        }
        if let Some(pending) = self.approvals.first() {
            self.draw_approval(frame, area, pending);
        }
    }

    fn draw_home(&self, frame: &mut ratatui::Frame, area: Rect, input_area: Rect) {
        let available = input_area.y.saturating_sub(area.y);
        let full_mark = wordmark(area.width);
        let full_height = full_mark.len() as u16;
        let full = available >= full_height + 6;
        let compact = !full && available >= 5;
        if !full && !compact {
            return;
        }

        let chip_y = if full {
            let mark_y = input_area.y.saturating_sub(full_height + 6);
            frame.render_widget(
                Paragraph::new(Text::from(full_mark)).alignment(Alignment::Center),
                Rect {
                    x: area.x,
                    y: mark_y,
                    width: area.width,
                    height: full_height,
                },
            );
            let tagline_y = mark_y + full_height + 1;
            frame.render_widget(
                Paragraph::new(Span::styled(
                    truncate(i18n::t("app.tagline"), area.width as usize),
                    Style::default().fg(DIM),
                ))
                .alignment(Alignment::Center),
                Rect {
                    x: area.x,
                    y: tagline_y,
                    width: area.width,
                    height: 1,
                },
            );
            if area.width >= 58 {
                frame.render_widget(
                    Paragraph::new(Self::home_capabilities()).alignment(Alignment::Center),
                    Rect {
                        x: area.x,
                        y: tagline_y + 1,
                        width: area.width,
                        height: 1,
                    },
                );
            }
            mark_y + full_height + 4
        } else {
            let brand_y = input_area.y.saturating_sub(4);
            let brand = if area.width >= 24 {
                "M I T N I C K S O L V E"
            } else {
                "MSK"
            };
            frame.render_widget(
                Paragraph::new(Span::styled(
                    brand,
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ))
                .alignment(Alignment::Center),
                Rect {
                    x: area.x,
                    y: brand_y,
                    width: area.width,
                    height: 1,
                },
            );
            frame.render_widget(
                Paragraph::new(Span::styled(
                    truncate(i18n::t("app.tagline"), area.width as usize),
                    Style::default().fg(DIM),
                ))
                .alignment(Alignment::Center),
                Rect {
                    x: area.x,
                    y: brand_y + 1,
                    width: area.width,
                    height: 1,
                },
            );
            brand_y + 2
        };

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
        frame.render_widget(
            Paragraph::new(Line::from(chips)).alignment(Alignment::Center),
            Rect {
                x: area.x,
                y: chip_y,
                width: area.width,
                height: 1,
            },
        );

        let card_y = input_area
            .y
            .saturating_add(input_area.height)
            .saturating_add(2);
        let card_height = 6u16;
        if area.bottom().saturating_sub(1) >= card_y + card_height {
            let width = 64.min(area.width.saturating_sub(8)).max(30);
            let card = Rect {
                x: area.x + area.width.saturating_sub(width) / 2,
                y: card_y,
                width,
                height: card_height,
            };
            let block = panel_block(i18n::t("home.try"), ACCENT);
            let inner = block.inner(card);
            frame.render_widget(block, card);
            let examples = [
                i18n::t("home.example.inspect"),
                i18n::t("home.example.map"),
                i18n::t("home.example.triage"),
            ];
            let mut lines = Vec::new();
            for example in examples {
                lines.push(Line::from(vec![
                    Span::styled("  › ", Style::default().fg(ACCENT)),
                    Span::styled(example, Style::default().fg(TEXT)),
                ]));
            }
            lines.push(Line::from(Span::styled(
                format!("  {}", i18n::t("home.demo")),
                Style::default().fg(FAINT),
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
    }

    fn home_capabilities() -> Line<'static> {
        Line::from(vec![
            Span::styled(i18n::t("home.cap.recon"), Style::default().fg(ACCENT)),
            Span::styled("  ·  ", Style::default().fg(FAINT)),
            Span::styled(i18n::t("home.cap.web"), Style::default().fg(GREEN)),
            Span::styled("  ·  ", Style::default().fg(FAINT)),
            Span::styled(i18n::t("home.cap.forensics"), Style::default().fg(TOOL)),
            Span::styled("  ·  ", Style::default().fg(FAINT)),
            Span::styled(i18n::t("home.cap.findings"), Style::default().fg(YELLOW)),
            Span::styled("  ·  ", Style::default().fg(FAINT)),
            Span::styled(i18n::t("home.cap.reports"), Style::default().fg(DIM)),
            Span::styled("  ·  ", Style::default().fg(FAINT)),
            Span::styled(i18n::t("home.cap.pty"), Style::default().fg(GREEN)),
        ])
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
        let (state_text, state_color, state_bg) = match state {
            "running" => (
                format!(
                    "{} {}",
                    spinner[(self.spin.get() / 2) % spinner.len()],
                    i18n::t("state.running")
                ),
                ACCENT,
                Color::Rgb(22, 36, 54),
            ),
            "waiting_input" if last_error.is_some() => (
                format!("✗ {}", i18n::t("state.last_failed")),
                RED,
                Color::Rgb(46, 24, 28),
            ),
            "waiting_input" => (
                format!("● {}", i18n::t("state.ready")),
                GREEN,
                Color::Rgb(20, 38, 28),
            ),
            "failed" => (
                format!("✗ {}", i18n::t("state.failed")),
                RED,
                Color::Rgb(46, 24, 28),
            ),
            "cancelled" => (
                format!("✗ {}", i18n::t("state.cancelled")),
                RED,
                Color::Rgb(46, 24, 28),
            ),
            "done" => (
                format!("✓ {}", i18n::t("state.done")),
                DIM,
                Color::Rgb(32, 32, 36),
            ),
            "saved" => (
                format!("▣ {}", i18n::t("state.saved")),
                DIM,
                Color::Rgb(32, 32, 36),
            ),
            other => (format!("● {other}"), DIM, Color::Rgb(32, 32, 36)),
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
        let right_width = UnicodeWidthStr::width(right_text.as_str()) as u16;
        let left_width = header_area.width.saturating_sub(right_width + 2).max(16);
        let state_width = UnicodeWidthStr::width(state_text.as_str()) as u16 + 2;
        let title_width = left_width.saturating_sub(state_width + 4) as usize;
        let left = Line::from(vec![
            Span::styled("▌ ", Style::default().fg(ACCENT)),
            Span::styled(
                truncate(&self.selected_task_title(), title_width),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ", Style::default()),
            Span::styled(
                format!(" {state_text} "),
                Style::default().fg(state_color).bg(state_bg),
            ),
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
        let right = Line::from(vec![
            Span::styled(display_provider, Style::default().fg(ACCENT)),
            Span::styled(" · ", Style::default().fg(FAINT)),
            Span::styled(
                if display_model.is_empty() {
                    "-".to_string()
                } else {
                    display_model
                },
                Style::default().fg(GREEN),
            ),
            Span::styled(" · ", Style::default().fg(FAINT)),
            Span::styled(
                i18n::tf("chat.effort", &[&i18n::effort_label(&display_effort)]),
                Style::default().fg(DIM),
            ),
        ]);
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

        let show_bar = transcript_area.width >= 8;
        let bar_width = if show_bar { 1 } else { 0 };
        let text_width = transcript_area.width.saturating_sub(bar_width);
        let task_id = self.selected_task_id();
        let revision = self.chat_revision;
        let (scroll, line_count, visible, offset) = {
            let mut cache = self.transcript_cache.borrow_mut();
            let cache_task = task_id.as_deref().unwrap_or("");
            if cache.task != cache_task || cache.width != text_width || cache.revision != revision {
                let selected: Vec<&ChatLine> = self
                    .chat
                    .iter()
                    .filter(|line| Some(line.task_id.as_str()) == task_id.as_deref())
                    .collect();
                let hashes: Vec<u64> = selected.iter().map(|line| chat_hash(line)).collect();
                if cache.task != cache_task || cache.width != text_width {
                    cache.task = cache_task.to_string();
                    cache.width = text_width;
                    cache.groups = build_chat_groups(&selected, text_width);
                } else {
                    let old = cache.groups.len();
                    let common = old.min(hashes.len());
                    let prefix_matches = cache.groups[..common]
                        .iter()
                        .zip(hashes.iter().take(common))
                        .all(|(group, hash)| group.hash == *hash);
                    if prefix_matches {
                        cache.groups.truncate(hashes.len());
                        for line in selected.iter().skip(common) {
                            let last_content = cache.groups.iter().rev().find_map(|group| {
                                matches!(
                                    group.role,
                                    ChatRole::User | ChatRole::Assistant | ChatRole::Tool
                                )
                                .then_some(group.role)
                            });
                            let show_label = group_label_needed(last_content, line.role);
                            cache
                                .groups
                                .push(build_chat_group(line, text_width, show_label));
                        }
                    } else if old == hashes.len()
                        && old > 0
                        && cache.groups[..old - 1]
                            .iter()
                            .zip(hashes.iter().take(old - 1))
                            .all(|(group, hash)| group.hash == *hash)
                    {
                        let show_label = cache.groups[old - 1].show_label;
                        cache.groups[old - 1] =
                            build_chat_group(selected[old - 1], text_width, show_label);
                    } else {
                        cache.groups = build_chat_groups(&selected, text_width);
                    }
                }
                cache.revision = revision;
                cache.prefix.clear();
                cache.prefix.push(0);
                for index in 0..cache.groups.len() {
                    let height = cache.groups[index].height;
                    let last = *cache.prefix.last().unwrap_or(&0);
                    cache.prefix.push(last + height);
                }
            }
            let total = *cache.prefix.last().unwrap_or(&0);
            let max_scroll = total.saturating_sub(transcript_area.height as usize);
            self.max_scroll
                .set(max_scroll.min(u16::MAX as usize) as u16);
            let scroll = if self.auto_scroll {
                max_scroll
            } else {
                (self.scroll_top as usize).min(max_scroll)
            };
            if cache.groups.is_empty() {
                (
                    scroll,
                    total,
                    vec![Line::from(Span::styled(
                        i18n::t("chat.empty"),
                        Style::default().fg(DIM),
                    ))],
                    0,
                )
            } else {
                let start = cache
                    .prefix
                    .partition_point(|prefix| *prefix <= scroll)
                    .saturating_sub(1)
                    .min(cache.groups.len() - 1);
                let offset = scroll.saturating_sub(cache.prefix[start]);
                let end = cache
                    .prefix
                    .partition_point(|prefix| *prefix < scroll + transcript_area.height as usize)
                    .max(start + 1)
                    .min(cache.groups.len());
                let mut visible = Vec::new();
                for group in &cache.groups[start..end] {
                    visible.extend(group.lines.iter().cloned());
                }
                (scroll, total, visible, offset)
            }
        };
        frame.render_widget(
            Paragraph::new(Text::from(visible))
                .wrap(Wrap { trim: false })
                .scroll((offset.min(u16::MAX as usize) as u16, 0)),
            Rect {
                width: text_width,
                ..transcript_area
            },
        );
        let max_scroll = line_count.saturating_sub(transcript_area.height as usize);
        if show_bar && max_scroll > 0 {
            let height = transcript_area.height as usize;
            let thumb = ((height * height) / line_count.max(1)).max(1).min(height);
            let travel = height.saturating_sub(thumb);
            let top = if max_scroll == 0 {
                0
            } else {
                (scroll * travel / max_scroll).min(travel)
            };
            let mut bar = Vec::with_capacity(height);
            for row in 0..height {
                let active = row >= top && row < top + thumb;
                bar.push(Line::from(Span::styled(
                    if active { "█" } else { "│" },
                    Style::default().fg(if active { ACCENT } else { FAINT }),
                )));
            }
            frame.render_widget(
                Paragraph::new(bar),
                Rect {
                    x: transcript_area.right().saturating_sub(1),
                    y: transcript_area.y,
                    width: 1,
                    height: transcript_area.height,
                },
            );
        }
    }

    fn draw_prompt(&self, frame: &mut ratatui::Frame, area: Rect, focused: bool) {
        let border = if !self.approvals.is_empty() {
            YELLOW
        } else if focused {
            ACCENT
        } else {
            FAINT
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(border))
            .style(Style::default().bg(PANEL));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width < 2 || inner.height < 3 {
            return;
        }

        let meta_y = inner.y + inner.height - 1;
        let input_rows = inner.height.saturating_sub(2).max(1);
        if inner.width > 4 {
            frame.render_widget(
                Paragraph::new(Span::styled("❯", Style::default().fg(ACCENT).bg(PANEL))),
                Rect {
                    x: inner.x + 1,
                    y: inner.y,
                    width: 1,
                    height: 1,
                },
            );
        }
        let prompt_width = if inner.width > 4 { 2 } else { 0 };
        let input_area = Rect {
            x: inner.x + 1 + prompt_width,
            y: inner.y,
            width: inner.width.saturating_sub(1 + prompt_width),
            height: input_rows,
        };
        self.composer_width.set(input_area.width);

        if self.input.is_empty() {
            let home = self.new_task || self.selected_task_id().is_none();
            let placeholder = if home {
                if input_area.width < 60 {
                    i18n::t("home.ask")
                } else if input_area.width < 78 {
                    i18n::t("home.ask.short")
                } else {
                    i18n::t("home.ask.example")
                }
            } else if input_area.width < 60 {
                i18n::t("composer.reply")
            } else {
                i18n::t("composer.reply.hint")
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
            let layout = composer_layout(
                &self.input,
                self.input_cursor,
                input_area.width.max(1) as usize,
            );
            let rows = input_area.height.max(1) as usize;
            let start_line = layout.cursor_row.saturating_sub(rows.saturating_sub(1));
            let display = layout
                .lines
                .iter()
                .skip(start_line)
                .take(rows)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n");
            frame.render_widget(
                Paragraph::new(display).style(Style::default().fg(TEXT).bg(PANEL)),
                input_area,
            );
            if focused {
                let x = input_area.x + layout.cursor_col as u16;
                let y = input_area.y + (layout.cursor_row - start_line) as u16;
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
        if self.selected_task_id().is_some()
            && !self.new_task
            && self.mode != self.selected_requested_mode()
        {
            meta.push(Span::styled(" → ", Style::default().fg(FAINT).bg(PANEL)));
            meta.push(Span::styled(
                self.mode_label(&self.mode),
                Style::default()
                    .fg(YELLOW)
                    .bg(PANEL)
                    .add_modifier(Modifier::BOLD),
            ));
        }
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
                i18n::tf("chat.effort", &[&i18n::effort_label(&display_effort)]),
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

    fn draw_slash_popup(&self, frame: &mut ratatui::Frame, input_area: Rect) {
        let Some(query) = self.input.strip_prefix('/') else {
            return;
        };
        if query.contains(char::is_whitespace) {
            return;
        }
        let matches = filtered_commands(query);
        if matches.is_empty() {
            return;
        }
        let rows = matches.len().min(6) as u16;
        let height = rows + 2;
        if input_area.y < height + 1 || input_area.width < 30 {
            return;
        }
        let width = 68.min(input_area.width);
        let area = Rect {
            x: input_area.x + input_area.width.saturating_sub(width) / 2,
            y: input_area.y.saturating_sub(height + 1),
            width,
            height,
        };
        frame.render_widget(Clear, area);
        let block = panel_block(i18n::t("palette.title"), ACCENT);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let items: Vec<ListItem> = matches
            .iter()
            .take(rows as usize)
            .enumerate()
            .map(|(index, command)| {
                let command = &COMMANDS[*command];
                ListItem::new(Line::from(vec![
                    Span::styled(
                        pad_display(command.label(), 24),
                        Style::default()
                            .fg(if index == 0 { TEXT } else { DIM })
                            .add_modifier(if index == 0 {
                                Modifier::BOLD
                            } else {
                                Modifier::empty()
                            }),
                    ),
                    Span::styled(command.hint(), Style::default().fg(FAINT)),
                ]))
            })
            .collect();
        frame.render_widget(List::new(items), inner);
    }

    fn draw_hints(&self, frame: &mut ratatui::Frame, input_area: Rect, hints_area: Rect) {
        let keys = [
            "hints.sessions",
            "hints.new",
            "hints.commands",
            "hints.help",
        ];
        let mut spans = Vec::new();
        let mut plain = String::new();
        for (index, key) in keys.iter().enumerate() {
            let text = i18n::t(key);
            let (shortcut, rest) = text.split_once(' ').unwrap_or((text, ""));
            spans.push(Span::styled(
                shortcut.to_string(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ));
            let tail = if index + 1 == keys.len() { " " } else { "   " };
            spans.push(Span::styled(
                format!(" {rest}{tail}"),
                Style::default().fg(DIM),
            ));
            plain.push_str(shortcut);
            plain.push(' ');
            plain.push_str(rest);
            plain.push_str(tail);
        }
        let hints = Line::from(spans);
        let hints_width = (UnicodeWidthStr::width(plain.as_str()) as u16 + 1)
            .min(hints_area.width)
            .max(1);
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
        let right = format!("MitnickSolve {VERSION}");
        let right_width = UnicodeWidthStr::width(right.as_str()) as u16;
        let left_width = area.width.saturating_sub(right_width + 5).max(10) as usize;
        let current_permission = self.display_permission();
        let permission_text = if self.selected_task_id().is_some()
            && !self.new_task
            && self.permission != current_permission
        {
            format!(
                "{} → {}",
                permission_label(&current_permission),
                permission_label(&self.permission)
            )
        } else {
            permission_label(&current_permission)
        };
        let permission = i18n::tf("footer.permission", &[&permission_text]);
        let permission_width = UnicodeWidthStr::width(permission.as_str());
        let workspace = truncate(
            &self.workspace_label(),
            left_width.saturating_sub(permission_width + 10).max(4),
        );
        let mut used = UnicodeWidthStr::width(workspace.as_str()) + permission_width + 5;
        let mut spans = vec![
            Span::styled(workspace, Style::default().fg(FAINT)),
            Span::styled("  ·  ", Style::default().fg(FAINT)),
            Span::styled(
                permission,
                Style::default().fg(permission_color(&self.display_permission())),
            ),
        ];
        if self.selected_task_id().is_some() {
            let (tokens, tools) = self.selected_usage();
            if tokens > 0 {
                let limit = self
                    .tasks
                    .get(self.selected_task)
                    .and_then(|task| task.get("max_context_tokens"))
                    .and_then(Value::as_u64)
                    .filter(|limit| *limit > 0);
                let ctx = i18n::tf("footer.ctx", &[&format_tokens(tokens)]);
                let extra =
                    5 + UnicodeWidthStr::width(ctx.as_str()) + if limit.is_some() { 9 } else { 0 };
                if used + extra <= left_width {
                    spans.push(Span::styled("  ·  ", Style::default().fg(FAINT)));
                    spans.push(Span::styled(ctx, Style::default().fg(DIM)));
                    if let Some(limit) = limit {
                        let ratio = (tokens as f64 / limit as f64).min(1.0);
                        let filled = ((ratio * 8.0).ceil() as usize).min(8);
                        let color = if ratio >= 0.9 {
                            RED
                        } else if ratio >= 0.7 {
                            YELLOW
                        } else {
                            DIM
                        };
                        spans.push(Span::styled(
                            format!(" {}{}", "▰".repeat(filled), "▱".repeat(8 - filled)),
                            Style::default().fg(color),
                        ));
                    }
                    used += extra;
                }
            }
            if tools > 0 {
                let text = i18n::tf("footer.tools", &[&tools.to_string()]);
                let extra = 5 + UnicodeWidthStr::width(text.as_str());
                if used + extra <= left_width {
                    spans.push(Span::styled("  ·  ", Style::default().fg(FAINT)));
                    spans.push(Span::styled(text, Style::default().fg(DIM)));
                    used += extra;
                }
            }
        }
        if !self.auto_scroll && self.selected_task_id().is_some() {
            let full = i18n::t("footer.scrollback.follow");
            let short = i18n::t("footer.scrollback");
            let (label, width) = if used + 5 + UnicodeWidthStr::width(full) <= left_width {
                (full, UnicodeWidthStr::width(full))
            } else if used + 5 + UnicodeWidthStr::width(short) <= left_width {
                (short, UnicodeWidthStr::width(short))
            } else if used + 7 <= left_width {
                ("↑", 1)
            } else {
                ("", 0)
            };
            if !label.is_empty() {
                spans.push(Span::styled("  ·  ", Style::default().fg(FAINT)));
                spans.push(Span::styled(label, Style::default().fg(YELLOW)));
                used += 5 + width;
            }
        }
        let _ = used;
        frame.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect {
                x: area.x + 2,
                y: area.y,
                width: left_width as u16,
                height: 1,
            },
        );
        if area.width > right_width + 4 {
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
        let area = centered_rect(70, 78, area);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(i18n::t("help.body"))
                .block(panel_block(i18n::t("help.title"), ACCENT))
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
        let block = panel_block(i18n::t("palette.title"), ACCENT);
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
                        format!("  {}", i18n::t("palette.placeholder"))
                    } else {
                        String::new()
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
                    format!("  {}", i18n::t("palette.empty")),
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
                    Span::styled(pad_display(command.label(), 26), Style::default().fg(TEXT)),
                    Span::styled(command.hint(), Style::default().fg(DIM)),
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

    fn draw_session_rail(&self, frame: &mut ratatui::Frame, area: Rect) {
        let title = if self.tasks.is_empty() {
            i18n::t("sessions.title").to_string()
        } else {
            i18n::tf("sessions.count", &[&self.tasks.len().to_string()])
        };
        let block = panel_block(&title, ACCENT);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 3 || inner.width < 8 {
            return;
        }
        let rows = inner.height.saturating_sub(2) as usize;
        let total = self.tasks.len();
        let current_row = if self.new_task {
            total
        } else {
            self.selected_task
        };
        let start = if total <= rows {
            0
        } else {
            current_row
                .saturating_sub(rows / 2)
                .min(total.saturating_sub(rows))
        };
        let mut lines = Vec::new();
        for index in start..total.min(start + rows) {
            let task = &self.tasks[index];
            let state = task.get("state").and_then(Value::as_str).unwrap_or("?");
            let glyph = state_glyph(state);
            let current = !self.new_task && index == self.selected_task;
            let title = task.get("goal").and_then(Value::as_str).unwrap_or("");
            lines.push(Line::from(vec![
                Span::styled(
                    if current { "▌ " } else { "  " },
                    Style::default().fg(if current { ACCENT } else { FAINT }),
                ),
                Span::styled(format!("{glyph} "), Style::default().fg(state_color(state))),
                Span::styled(
                    truncate(title, inner.width.saturating_sub(5) as usize),
                    Style::default().fg(if current { TEXT } else { DIM }),
                ),
            ]));
        }
        if total == 0 {
            lines.push(Line::from(Span::styled(
                format!("  {}", i18n::t("sessions.empty")),
                Style::default().fg(FAINT),
            )));
        }
        if lines.len() < rows {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(
                    if self.new_task { "● " } else { "＋ " },
                    Style::default().fg(if self.new_task { GREEN } else { ACCENT }),
                ),
                Span::styled(i18n::t("sessions.new"), Style::default().fg(ACCENT)),
            ]));
        }
        frame.render_widget(
            Paragraph::new(lines),
            Rect {
                x: inner.x,
                y: inner.y,
                width: inner.width,
                height: inner.height.saturating_sub(1),
            },
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                i18n::t("sessions.rail.hint"),
                Style::default().fg(FAINT),
            ))
            .alignment(Alignment::Center),
            Rect {
                x: inner.x,
                y: inner.bottom().saturating_sub(1),
                width: inner.width,
                height: 1,
            },
        );
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
        let block = panel_block(i18n::t("sessions.title"), ACCENT);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height < 3 {
            return;
        }
        frame.render_widget(
            Paragraph::new(Span::styled(
                i18n::t("sessions.intro"),
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
                pad_display(i18n::t("sessions.new"), 14),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled(i18n::t("sessions.new.hint"), Style::default().fg(DIM)),
        ])));
        for (index, task) in self.tasks.iter().enumerate() {
            let state = task.get("state").and_then(Value::as_str).unwrap_or("?");
            let state_color = state_color(state);
            let current = !self.new_task && index == self.selected_task;
            let title = task.get("goal").and_then(Value::as_str).unwrap_or("");
            items.push(ListItem::new(Line::from(vec![
                Span::styled(
                    if current { "▌ " } else { "  " },
                    Style::default().fg(if current { GREEN } else { FAINT }),
                ),
                Span::styled(
                    pad_display(&state_label(state), 14),
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
                i18n::t("sessions.keys"),
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
        let block = panel_block(i18n::t("findings.title"), ACCENT);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width < 10 || inner.height < 4 {
            return;
        }
        if self.findings.is_empty() {
            self.findings_max_scroll.set(0);
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(Span::styled(
                        i18n::t("findings.empty"),
                        Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        i18n::t("findings.empty.hint"),
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
                    i18n::tf("findings.count", &[&self.findings.len().to_string()]),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(
                        "   {}",
                        i18n::tf(
                            "findings.severity",
                            &[
                                &counts[4].to_string(),
                                &counts[3].to_string(),
                                &counts[2].to_string(),
                                &counts[1].to_string(),
                                &counts[0].to_string(),
                            ]
                        )
                    ),
                    Style::default().fg(DIM),
                ),
            ]),
            Line::from(""),
        ];
        let total: usize = self.findings.len();
        if total > 0 {
            let width = inner.width.saturating_sub(4).min(48) as usize;
            let mut used = 0usize;
            let mut bar: Vec<Span> = Vec::new();
            for (count, color) in [
                (counts[4], Color::Rgb(239, 68, 68)),
                (counts[3], Color::Rgb(249, 115, 22)),
                (counts[2], YELLOW),
                (counts[1], ACCENT),
                (counts[0], DIM),
            ] {
                let cells = count * width / total;
                if cells > 0 {
                    bar.push(Span::styled("█".repeat(cells), Style::default().fg(color)));
                    used += cells;
                }
            }
            if used < width {
                bar.push(Span::styled(
                    "░".repeat(width - used),
                    Style::default().fg(FAINT),
                ));
            }
            lines.push(Line::from(bar));
            lines.push(Line::from(""));
        }
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
                    {
                        let label = i18n::severity_label(severity);
                        let padding = 9usize.saturating_sub(UnicodeWidthStr::width(label.as_str()));
                        format!("{label}{}", " ".repeat(padding))
                    },
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
                    format!("  {}", i18n::finding_status_label(status)),
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
        let wrapped = Paragraph::new(lines.clone())
            .wrap(Wrap { trim: false })
            .line_count(list_area.width)
            .max(1);
        self.findings_max_scroll
            .set(wrapped.saturating_sub(list_area.height as usize));
        let scroll = self.findings_scroll.min(self.findings_max_scroll.get());
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((scroll.min(u16::MAX as usize) as u16, 0)),
            list_area,
        );
        frame.render_widget(
            Paragraph::new(Span::styled(
                i18n::t("findings.keys"),
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

    fn draw_settings(&self, frame: &mut ratatui::Frame, area: Rect) {
        let width = 72.min(area.width.saturating_sub(6)).max(34);
        let height = 13.min(area.height.saturating_sub(2)).max(9);
        let area = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, area);
        let block = panel_block(i18n::t("settings.title"), ACCENT);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width < 24 {
            return;
        }
        let current_permission = self.display_permission();
        let permission_value = if self.selected_task_id().is_some()
            && !self.new_task
            && self.permission != current_permission
        {
            format!(
                "{}  ({})",
                i18n::permission_label(&self.permission),
                i18n::tf(
                    "settings.current",
                    &[&i18n::permission_label(&current_permission)]
                )
            )
        } else {
            i18n::permission_label(&self.permission)
        };
        let current_mode = self.selected_mode();
        let requested_mode = self.selected_requested_mode();
        let mode_value = if self.selected_task_id().is_some()
            && !self.new_task
            && (self.mode != requested_mode || current_mode != requested_mode)
        {
            format!(
                "{}  ({})",
                self.mode_label(&self.mode),
                i18n::tf("settings.current", &[&self.mode_label(&current_mode)])
            )
        } else {
            self.mode_label(&self.mode)
        };
        let rows = [
            (
                i18n::t("settings.language"),
                self.language.label().to_string(),
            ),
            (i18n::t("settings.permission"), permission_value),
            (i18n::t("settings.mode"), mode_value),
            (i18n::t("settings.providers"), String::new()),
            (i18n::t("settings.setup"), String::new()),
        ];
        let mut lines = vec![
            Line::from(Span::styled(
                i18n::t("settings.intro"),
                Style::default().fg(DIM),
            )),
            Line::from(""),
        ];
        for (index, (label, value)) in rows.iter().enumerate() {
            let selected = index == self.settings_selection;
            let padding = 22usize.saturating_sub(UnicodeWidthStr::width(*label));
            let mut spans = vec![
                Span::styled(
                    if selected { "❯ " } else { "  " },
                    Style::default().fg(ACCENT),
                ),
                Span::styled(
                    format!("{label}{}", " ".repeat(padding)),
                    if selected {
                        Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(TEXT)
                    },
                ),
            ];
            if !value.is_empty() {
                spans.push(Span::styled(value.clone(), Style::default().fg(DIM)));
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            i18n::t("settings.keys"),
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

    fn draw_language(&self, frame: &mut ratatui::Frame, area: Rect) {
        let width = 56.min(area.width.saturating_sub(6)).max(30);
        let height = (i18n::ALL.len() as u16 + 6)
            .min(area.height.saturating_sub(2))
            .max(10);
        let area = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, area);
        let block = panel_block(i18n::t("language.title"), ACCENT);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width < 20 {
            return;
        }
        let mut lines = vec![
            Line::from(Span::styled(
                i18n::t("language.intro"),
                Style::default().fg(DIM),
            )),
            Line::from(""),
        ];
        for (index, language) in i18n::ALL.iter().enumerate() {
            let selected = index == self.language_selection;
            let current = *language == self.language;
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
                    language.label().to_string(),
                    if selected {
                        Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(TEXT)
                    },
                ),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            i18n::t("language.keys"),
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
        let block = panel_block(i18n::t("modes.title"), ACCENT);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut lines = vec![
            Line::from(Span::styled(
                i18n::t("modes.intro"),
                Style::default().fg(DIM),
            )),
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
                    {
                        let label = self.mode_label(&mode.id);
                        let padding =
                            10usize.saturating_sub(UnicodeWidthStr::width(label.as_str()));
                        format!("{label}{}", " ".repeat(padding))
                    },
                    Style::default().fg(if current { GREEN } else { FAINT }),
                ),
                Span::styled(
                    i18n::mode_desc(&mode.id)
                        .map(str::to_string)
                        .unwrap_or_else(|| mode.description.clone()),
                    Style::default().fg(DIM),
                ),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            i18n::t("modes.keys"),
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
        let block = panel_block(i18n::t("providers.title"), ACCENT);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut lines = vec![
            Line::from(Span::styled(
                i18n::t("providers.intro"),
                Style::default().fg(DIM),
            )),
            Line::from(""),
        ];
        if self.profiles.is_empty() {
            lines.push(Line::from(Span::styled(
                i18n::t("providers.empty"),
                Style::default().fg(TEXT),
            )));
            lines.push(Line::from(Span::styled(
                i18n::t("providers.empty.hint"),
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
                i18n::t("effort.default").to_string()
            } else {
                i18n::effort_label(&profile.effort)
            };
            let mut detail = vec![Span::styled(
                format!(
                    "    {} · {} · {} {}",
                    profile.provider,
                    model,
                    i18n::t("providers.effort"),
                    effort
                ),
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
                detail.push(Span::styled(
                    i18n::t("providers.no_key"),
                    Style::default().fg(FAINT),
                ));
            } else if profile.api_key.is_empty() {
                detail.push(Span::styled(
                    if self.daemon_llm_key {
                        i18n::t("providers.key_env")
                    } else {
                        i18n::t("providers.key_missing")
                    },
                    Style::default().fg(if self.daemon_llm_key { DIM } else { YELLOW }),
                ));
            } else {
                detail.push(Span::styled(
                    i18n::t("providers.key_set"),
                    Style::default().fg(GREEN),
                ));
            }
            lines.push(Line::from(detail));
        }
        lines.push(Line::from(""));
        if let Some(index) = self.provider_delete_armed {
            if let Some(profile) = self.profiles.get(index) {
                lines.push(Line::from(Span::styled(
                    i18n::tf("providers.delete_armed", &[&profile.name]),
                    Style::default().fg(RED).add_modifier(Modifier::BOLD),
                )));
            }
        }
        lines.push(Line::from(Span::styled(
            i18n::t("providers.keys"),
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
        let block = panel_block(i18n::t("permissions.title"), GREEN);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let mut lines = vec![
            Line::from(Span::styled(
                i18n::t("permissions.intro"),
                Style::default().fg(DIM),
            )),
            Line::from(""),
        ];
        for (index, id) in PERMISSIONS.iter().enumerate() {
            let selected = index == self.permission_selection;
            let current = *id == self.permission;
            let label = i18n::permission_label(id);
            let padding = 10usize.saturating_sub(UnicodeWidthStr::width(label.as_str()));
            let description = i18n::permission_desc(id);
            lines.push(Line::from(vec![
                Span::styled(
                    if selected { "❯ " } else { "  " },
                    Style::default().fg(GREEN),
                ),
                Span::styled(
                    format!("{label}{}", " ".repeat(padding)),
                    if selected {
                        Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(TEXT)
                    },
                ),
                Span::styled(
                    format!("{:<8}", i18n::permission_label(id)),
                    Style::default().fg(if current { permission_color(id) } else { FAINT }),
                ),
                Span::styled(description, Style::default().fg(DIM)),
            ]));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            i18n::t("permissions.keys"),
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
        let goal = pending
            .get("task_id")
            .and_then(Value::as_str)
            .and_then(|task_id| {
                self.tasks
                    .iter()
                    .find(|task| task.get("task_id").and_then(Value::as_str) == Some(task_id))
            })
            .and_then(|task| task.get("goal"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let arg_lines: Vec<&str> = args.lines().collect();
        let shown_args = arg_lines.len().min(8);
        let header_height = if goal.is_empty() { 4 } else { 5 };
        let extra = usize::from(arg_lines.len() > shown_args);
        let width = 84.min(area.width.saturating_sub(4)).max(36).min(area.width);
        let height = (header_height + shown_args + extra + 4)
            .min(area.height.saturating_sub(2) as usize)
            .max(10) as u16;
        let area = Rect {
            x: area.x + area.width.saturating_sub(width) / 2,
            y: area.y + area.height.saturating_sub(height) / 2,
            width,
            height,
        };
        let total_approvals = self.approvals.len().to_string();
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(YELLOW))
            .style(Style::default().bg(PANEL))
            .title(Span::styled(
                format!(
                    " {} · {} · #{id} ",
                    i18n::t("approval.required"),
                    i18n::tf("approval.count", &["1", &total_approvals])
                ),
                Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let label = |key: &'static str| {
            let text = i18n::t(key);
            let padding = 6usize.saturating_sub(UnicodeWidthStr::width(text));
            format!("{text}{}", " ".repeat(padding))
        };
        let mut lines = vec![Line::from(vec![
            Span::styled(label("approval.tool"), Style::default().fg(FAINT)),
            Span::styled(
                tool.to_string(),
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
        ])];
        if !goal.is_empty() {
            lines.push(Line::from(vec![
                Span::styled(label("approval.task"), Style::default().fg(FAINT)),
                Span::styled(
                    truncate(goal, inner.width.saturating_sub(8) as usize),
                    Style::default().fg(DIM),
                ),
            ]));
        }
        lines.push(Line::from(vec![
            Span::styled(label("approval.why"), Style::default().fg(FAINT)),
            Span::styled(i18n::approval_reason(reason), Style::default().fg(DIM)),
        ]));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            i18n::t("approval.arguments"),
            Style::default().fg(FAINT),
        )));
        let arg_width = inner.width.saturating_sub(2) as usize;
        for part in arg_lines.iter().take(shown_args) {
            lines.push(Line::from(Span::styled(
                format!("  {}", truncate(part, arg_width)),
                Style::default().fg(DIM),
            )));
        }
        if arg_lines.len() > shown_args {
            lines.push(Line::from(Span::styled("  …", Style::default().fg(FAINT))));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled("y", Style::default().fg(GREEN).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" {}     ", i18n::t("approval.allow")),
                Style::default().fg(DIM),
            ),
            Span::styled("n", Style::default().fg(RED).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" {}", i18n::t("approval.deny")),
                Style::default().fg(DIM),
            ),
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
    let mut dirty = true;
    loop {
        let changed = match app.refresh() {
            Ok(changed) => changed,
            Err(err) => {
                app.status = format!("refresh failed: {err}");
                false
            }
        };
        if app.quit_requested {
            return Ok(());
        }
        dirty |= changed;
        let running = app.any_running();
        if dirty || running {
            terminal.draw(|frame| app.draw(frame))?;
            dirty = false;
        }
        let poll_ms = if app.terminal_focus {
            16
        } else if running {
            50
        } else {
            app.refresh_ms.clamp(100, 500)
        };
        if event::poll(Duration::from_millis(poll_ms))? {
            // Drain a burst (paste as key events, mouse wheel, held keys) before
            // painting. Each individual key event used to trigger a full frame,
            // which made large pastes and wheel bursts feel laggy.
            let mut batch = 0usize;
            let batch_started = Instant::now();
            loop {
                let (quit, redraw) = handle_crossterm_event(&mut app, event::read()?);
                dirty |= redraw;
                if quit {
                    return Ok(());
                }
                batch += 1;
                if batch >= 2048
                    || batch_started.elapsed() >= Duration::from_millis(12)
                    || !event::poll(Duration::ZERO)?
                {
                    break;
                }
            }
            if app.quit_requested {
                return Ok(());
            }
        }
    }
}

fn handle_crossterm_event(app: &mut DaemonApp, event: CrosstermEvent) -> (bool, bool) {
    match event {
        CrosstermEvent::Key(key)
            if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
        {
            (app.handle_key(key), true)
        }
        CrosstermEvent::Paste(text) => {
            app.handle_paste(&text);
            (false, true)
        }
        CrosstermEvent::Mouse(mouse) => match mouse.kind {
            MouseEventKind::ScrollUp => {
                app.scroll_up(3);
                (false, true)
            }
            MouseEventKind::ScrollDown => {
                app.scroll_down(3);
                (false, true)
            }
            _ => (false, false),
        },
        CrosstermEvent::Resize(_, _) => (false, true),
        _ => (false, false),
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
            cli.snapshot_settings,
            cli.snapshot_language,
        );
    }

    enable_raw_mode()?;
    let keyboard_enhanced = supports_keyboard_enhancement().unwrap_or(false);
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(
            std::io::stdout(),
            PopKeyboardEnhancementFlags,
            DisableBracketedPaste,
            DisableMouseCapture,
            LeaveAlternateScreen,
            crossterm::cursor::Show
        );
        hook(info);
    }));
    let mut out = stdout();
    execute!(
        out,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    if keyboard_enhanced {
        execute!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    // crossterm enables any-motion tracking (?1003h). We only need wheel and
    // button events; motion events would wake the loop on every mouse move.
    write!(out, "\x1b[?1003l")?;
    out.flush()?;
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
    if keyboard_enhanced {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        DisableMouseCapture,
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
    settings: bool,
    language: bool,
) -> Result<()> {
    let mut app = DaemonApp::connect(root.to_path_buf(), 100, provider)?;
    let _ = app.refresh();
    if palette || sessions || findings || modes || providers || permissions || settings || language
    {
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
    if settings {
        app.open_settings();
    }
    if language {
        app.open_language();
    }
    app.auto_scroll = false;
    app.scroll_top = 10_000;
    let width = std::env::var("MSK_SNAPSHOT_WIDTH")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(120);
    let height = std::env::var("MSK_SNAPSHOT_HEIGHT")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(40);
    let backend = ratatui::backend::TestBackend::new(width, height);
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
                || command.label().to_lowercase().contains(&needle)
                || command.hint().to_lowercase().contains(&needle)
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
        "done" | "saved" => DIM,
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

fn state_glyph(state: &str) -> char {
    match state {
        "running" | "waiting_input" => '●',
        "failed" | "cancelled" => '✗',
        "done" => '✓',
        "saved" => '▣',
        _ => '·',
    }
}

fn state_label(state: &str) -> String {
    let word = i18n::state_word(state);
    if word.is_empty() {
        format!("{} {state}", state_glyph(state))
    } else {
        format!("{} {word}", state_glyph(state))
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

/// Short, stable label for IDs shown in the transcript. Full artifact hashes
/// remain available in the event inspector and on disk.
fn short_id(value: &str) -> String {
    const MAX: usize = 12;
    if value.chars().count() <= MAX {
        value.to_string()
    } else {
        format!("{}…", value.chars().take(MAX - 1).collect::<String>())
    }
}

/// Reads the byte count from the `[artifact <id> (<bytes> bytes)]` prefix the
/// kernel attaches to truncated observations.
fn artifact_bytes(data: &Value) -> Option<u64> {
    let observation = data.get("observation")?.as_str()?;
    let rest = observation.strip_prefix("[artifact ")?;
    let open = rest.find('(')?;
    let close = rest[open..].find(')')?;
    rest[open + 1..open + close]
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn clean_inline(value: &str, max: usize) -> String {
    let text: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    truncate(text.trim(), max)
}

fn first_line(value: &str) -> String {
    value
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| clean_inline(line, 72))
        .unwrap_or_default()
}

/// Long hex runs are almost always hashes; keep them recognizable but short.
fn shorten_hex_ids(text: &str) -> String {
    let characters: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < characters.len() {
        if characters[index].is_ascii_hexdigit() {
            let start = index;
            while index < characters.len() && characters[index].is_ascii_hexdigit() {
                index += 1;
            }
            if index - start >= 32 {
                out.extend(&characters[start..start + 8]);
                out.push('…');
            } else {
                out.extend(&characters[start..index]);
            }
        } else {
            out.push(characters[index]);
            index += 1;
        }
    }
    out
}

fn structured_summary(tool: &str, value: &Value) -> Option<String> {
    let number = |key: &str| value.get(key).and_then(Value::as_u64);
    let text = |key: &str| value.get(key).and_then(Value::as_str);
    let summary = match tool {
        "exec" | "session_exec" => {
            let mut parts = Vec::new();
            if let Some(code) = number("exit_code") {
                parts.push(format!("exit {code}"));
            }
            let line = text("stdout")
                .or_else(|| text("stderr"))
                .map(first_line)
                .unwrap_or_default();
            if !line.is_empty() {
                parts.push(line);
            }
            if value.get("truncated") == Some(&Value::Bool(true)) {
                parts.push("truncated".into());
            }
            if let Some(ms) = number("duration_ms") {
                parts.push(format!("{ms}ms"));
            }
            parts.join(" · ")
        }
        "read_file" => {
            let size = number("size").or_else(|| number("bytes")).unwrap_or(0);
            let mut parts = vec![format!("{size} B")];
            if let Some(hash) = text("sha256") {
                parts.push(format!("sha256 {}", short_id(hash)));
            }
            if value.get("eof") == Some(&Value::Bool(false)) {
                parts.push("more".into());
            }
            parts.join(" · ")
        }
        "artifact_read" => {
            let bytes = number("bytes")
                .or_else(|| text("data").map(|data| data.len() as u64))
                .unwrap_or(0);
            let mut parts = vec![format!("{bytes} B")];
            if let Some(offset) = number("next_offset") {
                parts.push(format!("offset {offset}"));
            }
            if let Some(encoding) = text("encoding") {
                parts.push(encoding.to_string());
            }
            parts.join(" · ")
        }
        "stat_file" => {
            let size = number("size").unwrap_or(0);
            let mut parts = vec![format!("{size} B")];
            if let Some(mode) = text("mode") {
                parts.push(mode.to_string());
            }
            parts.join(" · ")
        }
        "session_create" => {
            let mut parts = Vec::new();
            if let Some(id) = text("session_id") {
                parts.push(format!("session {}", short_id(id)));
            }
            if let Some(state) = text("state") {
                parts.push(state.to_string());
            }
            if let Some(shell) = text("shell") {
                parts.push(shell.to_string());
            }
            parts.join(" · ")
        }
        "session_close" => "closed".into(),
        "session_write" => number("bytes")
            .map(|bytes| format!("{bytes} B"))
            .unwrap_or_default(),
        "web_request" | "http_request" => {
            let mut parts = Vec::new();
            if let Some(status) = number("status") {
                parts.push(format!("HTTP {status}"));
            }
            if let Some(bytes) = number("bytes") {
                parts.push(format!("{bytes} B"));
            }
            if value.get("truncated") == Some(&Value::Bool(true)) {
                parts.push("truncated".into());
            }
            parts.join(" · ")
        }
        _ => return None,
    };
    (!summary.is_empty()).then_some(summary)
}

fn observation_summary(tool: &str, data: &Value) -> Option<String> {
    let observation = data.get("observation")?;
    let value = match observation {
        Value::String(text) => {
            serde_json::from_str::<Value>(text).unwrap_or_else(|_| Value::String(text.clone()))
        }
        other => other.clone(),
    };
    if let Some(summary) = structured_summary(tool, &value) {
        return Some(summary);
    }
    let text = match &value {
        Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    Some(shorten_hex_ids(&clean_inline(&text, 140)))
}

fn compact_json(value: &Value, max_chars: usize) -> String {
    truncate(
        &serde_json::to_string(value).unwrap_or_else(|_| "null".into()),
        max_chars,
    )
}

fn string_field<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn tool_summary(tool: &str, args: &Value) -> String {
    let value = match tool {
        "exec" => args
            .get("argv")
            .and_then(Value::as_array)
            .map(|argv| {
                argv.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default(),
        "session_exec" => string_field(args, "command")
            .unwrap_or_default()
            .to_string(),
        "session_create" => match string_field(args, "name").or_else(|| string_field(args, "kind"))
        {
            Some(name) => name.to_string(),
            None => string_field(args, "env_id")
                .map(short_id)
                .unwrap_or_default(),
        },
        "session_close" => short_id(string_field(args, "session_id").unwrap_or_default()),
        "env_create" => {
            let kind = string_field(args, "kind").unwrap_or("managed");
            let name = string_field(args, "name").unwrap_or("");
            if name.is_empty() {
                kind.to_string()
            } else {
                format!("{kind} {name}")
            }
        }
        "env_open" | "env_close" => string_field(args, "env_id").unwrap_or_default().to_string(),
        "artifact_list" => string_field(args, "env_id").unwrap_or_default().to_string(),
        "read_file" | "write_file" | "list_dir" | "stat_file" | "file_hash" | "file_type"
        | "file_strings" | "file_hexdump" | "file_entropy" | "file_search" | "binary_info"
        | "secret_scan" => string_field(args, "path").unwrap_or_default().to_string(),
        "net_dns" => string_field(args, "host").unwrap_or_default().to_string(),
        "net_scan" => string_field(args, "target")
            .or_else(|| string_field(args, "host"))
            .unwrap_or_default()
            .to_string(),
        "net_dial" | "net_tls" => {
            let host = string_field(args, "host").unwrap_or_default();
            let port = args.get("port").and_then(Value::as_u64);
            match port {
                Some(port) => format!("{host}:{port}"),
                None => host.to_string(),
            }
        }
        "web_request" | "http_request" => {
            let method = string_field(args, "method").unwrap_or("GET");
            let url = string_field(args, "url").unwrap_or_default();
            format!("{method} {url}")
        }
        "artifact_read" => {
            let id = short_id(string_field(args, "id").unwrap_or_default());
            let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0);
            format!("artifact {id} @{offset}")
        }
        "spawn_agent" => string_field(args, "goal").unwrap_or_default().to_string(),
        "finding_add" => string_field(args, "title").unwrap_or_default().to_string(),
        "flag_add" => string_field(args, "value")
            .or_else(|| string_field(args, "flag"))
            .unwrap_or_default()
            .to_string(),
        _ => return compact_json(args, 140),
    };
    if value.trim().is_empty() {
        compact_json(args, 140)
    } else {
        truncate(value.trim(), 180)
    }
}

/// Clamps a byte cursor to the string and backs it up to a character
/// boundary. A cursor shared between setup fields can otherwise land inside a
/// multi-byte character and make a later slice panic.
fn pad_display(text: &str, width: usize) -> String {
    let used = UnicodeWidthStr::width(text);
    format!("{text}{}", " ".repeat(width.saturating_sub(used)))
}

fn char_boundary_at_or_before(text: &str, index: usize) -> usize {
    let mut index = index.min(text.len());
    while index > 0 && !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

/// Visual layout of the composer: explicit newlines plus soft wrapping at the
/// available width. Keeping this in one place means the height, rendered rows
/// and cursor position always agree.
struct ComposerLayout {
    lines: Vec<String>,
    starts: Vec<usize>,
    cursor_row: usize,
    cursor_col: usize,
}

fn composer_layout(input: &str, cursor: usize, width: usize) -> ComposerLayout {
    let width = width.max(1);
    let cursor = char_boundary_at_or_before(input, cursor);
    let mut lines = vec![String::new()];
    let mut starts = vec![0usize];
    let mut row = 0usize;
    let mut col = 0usize;
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;
    let mut marked = false;
    for (index, character) in input.char_indices() {
        if !marked && index == cursor {
            cursor_row = row;
            cursor_col = col;
            marked = true;
        }
        if character == '\n' {
            lines.push(String::new());
            starts.push(index + 1);
            row += 1;
            col = 0;
            continue;
        }
        let char_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if col > 0 && col + char_width > width {
            lines.push(String::new());
            starts.push(index);
            row += 1;
            col = 0;
        }
        lines[row].push(character);
        col += char_width;
    }
    if !marked {
        cursor_row = row;
        cursor_col = col;
    }
    ComposerLayout {
        lines,
        starts,
        cursor_row,
        cursor_col,
    }
}

fn composer_offset(layout: &ComposerLayout, row: usize, column: usize) -> usize {
    let row = row.min(layout.lines.len().saturating_sub(1));
    let start = layout.starts.get(row).copied().unwrap_or(0);
    let mut width = 0usize;
    for (offset, character) in layout.lines[row].char_indices() {
        let char_width = UnicodeWidthChar::width(character).unwrap_or(0);
        if width + char_width > column {
            return start + offset;
        }
        width += char_width;
    }
    start + layout.lines[row].len()
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        format!("{}…", value.chars().take(max_chars).collect::<String>())
    }
}

fn format_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
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
    fn composer_wraps_long_and_wide_lines() {
        let layout = composer_layout("abcdef", 6, 3);
        assert_eq!(layout.lines, vec!["abc", "def"]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 3));

        let layout = composer_layout("中文字", 9, 4);
        assert_eq!(layout.lines, vec!["中文", "字"]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 2));

        let layout = composer_layout("ab\ncd", 5, 20);
        assert_eq!(layout.lines, vec!["ab", "cd"]);
        assert_eq!((layout.cursor_row, layout.cursor_col), (1, 2));
    }

    #[test]
    fn composer_maps_rows_back_to_byte_offsets() {
        let layout = composer_layout("abcdef", 6, 3);
        assert_eq!(composer_offset(&layout, 0, 3), 3);
        assert_eq!(composer_offset(&layout, 1, 0), 3);
        assert_eq!(composer_offset(&layout, 1, 3), 6);

        let layout = composer_layout("ab\ncd", 5, 20);
        assert_eq!(composer_offset(&layout, 0, 2), 2);
        assert_eq!(composer_offset(&layout, 1, 0), 3);
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
    fn markdown_lite_renders_headings_lists_and_code() {
        let lines = markdown_lite("# Heading\n- **bold** and `code`\n```sh\necho hi\n```");
        let text: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect();
        assert!(text.iter().any(|line| line.contains("Heading")));
        assert!(text.iter().any(|line| line.contains("• bold and code")));
        assert!(text.iter().any(|line| line.contains("╭─ sh")));
        assert!(text.iter().any(|line| line.contains("echo hi")));
    }

    #[test]
    fn tool_results_use_compact_structured_summaries() {
        let exec = json!({
            "observation": {
                "exit_code": 0,
                "stdout": "session-one",
                "duration_ms": 3
            }
        });
        assert_eq!(
            observation_summary("session_exec", &exec).as_deref(),
            Some("exit 0 · session-one · 3ms")
        );

        let id = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let artifact = json!({
            "observation": format!("[artifact {id} (4954 bytes)] preview")
        });
        assert_eq!(artifact_bytes(&artifact), Some(4954));
        assert_eq!(short_id(id), "0123456789a…");

        let read = json!({
            "observation": {
                "bytes": 200,
                "next_offset": 200,
                "encoding": "utf8"
            }
        });
        assert_eq!(
            observation_summary("artifact_read", &read).as_deref(),
            Some("200 B · offset 200 · utf8")
        );
    }

    #[test]
    fn neutral_system_events_do_not_split_a_tool_group() {
        let assistant = ChatLine {
            task_id: "task-1".into(),
            role: ChatRole::Assistant,
            text: "run it".into(),
            streaming: false,
        };
        let approval = ChatLine {
            task_id: "task-1".into(),
            role: ChatRole::System,
            text: "⧗ approval requested · session_exec".into(),
            streaming: false,
        };
        let tool = ChatLine {
            task_id: "task-1".into(),
            role: ChatRole::Tool,
            text: "✓ session_exec  exit 0".into(),
            streaming: false,
        };
        let groups = build_chat_groups(&[&assistant, &approval, &tool], 80);
        assert!(groups[0].show_label);
        assert!(!groups[2].show_label);
    }

    #[test]
    fn tool_summaries_are_readable() {
        assert_eq!(
            tool_summary("session_exec", &json!({"command": "id"})),
            "id"
        );
        assert_eq!(
            tool_summary("net_scan", &json!({"target": "10.0.0.0/24", "ports": [80]})),
            "10.0.0.0/24"
        );
        assert_eq!(
            tool_summary(
                "web_request",
                &json!({"method": "GET", "url": "https://x.test"})
            ),
            "GET https://x.test"
        );
        let fallback = tool_summary("echo", &json!({"text": "hi"}));
        assert!(fallback.contains("hi"));
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
    fn text_after_a_tool_call_starts_a_new_assistant_block() {
        let mut chat = Vec::new();
        append_agent_text(&mut chat, "task-1", "first step", true);
        chat.push(ChatLine {
            task_id: "task-1".into(),
            role: ChatRole::Tool,
            text: "▸ exec".into(),
            streaming: false,
        });
        append_agent_text(&mut chat, "task-1", "second step", true);
        assert_eq!(chat.len(), 3);
        assert_eq!(chat[0].text, "first step");
        assert_eq!(chat[2].text, "second step");
        finish_streaming(&mut chat, "task-1");
        assert!(chat.iter().all(|line| !line.streaming));
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
    fn turn_complete_line_and_effort_cycle() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app =
            DaemonApp::connect(PathBuf::from(root), 100, Some("openai-responses".into())).unwrap();
        app.setup = None;
        app.tasks = vec![json!({
            "task_id": "task-a", "goal": "meta", "state": "waiting_input",
            "provider": "openai-responses", "model": "deepseek-v4.1-flash-1017",
            "effort": "high", "permission": "auto", "mode": "investigate",
            "last_error": null
        })];
        app.selected_task = 0;
        app.new_task = false;
        app.chat.clear();
        let mut event = Event::new(
            "turn.end",
            "agent",
            json!({"turn": 1, "status": "awaiting_input", "duration_ms": 2400}),
        );
        event.ts = "2026-11-18T10:11:12Z".into();
        app.append_history_event("task-a", &event);
        let meta = app
            .chat
            .iter()
            .find(|line| line.role == ChatRole::Meta)
            .expect("turn complete line");
        assert!(meta.text.contains("turn complete"));
        assert!(meta.text.contains("10:11:12"), "{}", meta.text);
        assert!(meta.text.contains("deepseek-v4.1-flash-1017"));
        assert!(meta.text.contains("effort high"));
        assert!(meta.text.contains("2.4s"));

        app.effort = "default".into();
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        assert_eq!(app.effort, "minimal");
        app.handle_key(KeyEvent::new(KeyCode::F(2), KeyModifiers::SHIFT));
        assert_eq!(app.effort, "default");
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn slash_popup_and_role_labels_render() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app =
            DaemonApp::connect(PathBuf::from(root), 100, Some("scripted".into())).unwrap();
        app.setup = None;
        app.tasks = vec![json!({
            "task_id": "task-a", "goal": "chat roles", "state": "waiting_input",
            "provider": "scripted", "model": null, "effort": "default",
            "permission": "auto", "mode": "exploit", "last_error": null
        })];
        app.selected_task = 0;
        app.new_task = false;
        app.composing = true;
        app.chat = vec![
            ChatLine {
                task_id: "task-a".into(),
                role: ChatRole::User,
                text: "find the flag".into(),
                streaming: false,
            },
            ChatLine {
                task_id: "task-a".into(),
                role: ChatRole::Assistant,
                text: "I will inspect `./challenge` first.".into(),
                streaming: false,
            },
        ];
        app.input = "/mod".into();
        app.input_cursor = 4;
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
        assert!(content.contains("commands"));
        assert!(content.contains("Agent mode"));
        assert_eq!(content.matches("you").count(), 1);
        assert!(content.contains("  Mitnick"));
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn arrows_scroll_while_the_composer_is_focused() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app =
            DaemonApp::connect(PathBuf::from(root), 100, Some("scripted".into())).unwrap();
        app.setup = None;
        app.tasks = vec![json!({
            "task_id": "task-a", "goal": "scroll", "state": "waiting_input",
            "provider": "scripted", "model": null, "effort": "default",
            "permission": "auto", "mode": "exploit", "last_error": null
        })];
        app.selected_task = 0;
        app.new_task = false;
        app.composing = true;
        app.input.clear();
        app.input_history = vec!["old message".into()];
        app.max_scroll.set(50);
        app.auto_scroll = true;
        app.scroll_top = 0;
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(!app.auto_scroll, "Up must leave follow mode");
        assert_eq!(app.scroll_top, 49);
        assert!(app.input.is_empty(), "Up must not fill the composer");
        app.handle_key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(app.scroll_top, 39);
        app.input = "draft".into();
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            app.scroll_top, 38,
            "typing does not steal Up from scrolling"
        );
        app.input.clear();
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::CONTROL));
        assert_eq!(app.input, "old message", "Ctrl+Up recalls history");
        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL));
        assert!(app.auto_scroll);
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn scroll_math_clamps_and_rearms_follow() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app =
            DaemonApp::connect(PathBuf::from(root), 100, Some("scripted".into())).unwrap();
        app.setup = None;
        app.max_scroll.set(50);
        app.auto_scroll = true;
        app.scroll_top = 0;
        app.scroll_up(3);
        assert!(!app.auto_scroll);
        assert_eq!(app.scroll_top, 47);
        app.scroll_down(3);
        assert!(app.auto_scroll);
        assert_eq!(app.scroll_top, 50);
        app.scroll_to_top();
        assert_eq!(app.scroll_top, 0);
        assert!(!app.auto_scroll);
        app.follow_output();
        assert!(app.auto_scroll);
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn long_transcript_draw_perf() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app =
            DaemonApp::connect(PathBuf::from(root), 100, Some("scripted".into())).unwrap();
        app.setup = None;
        app.tasks = vec![json!({
            "task_id": "task-a", "goal": "long transcript", "state": "waiting_input",
            "provider": "scripted", "model": null, "effort": "default",
            "permission": "auto", "mode": "exploit", "last_error": null
        })];
        app.selected_task = 0;
        app.new_task = false;
        app.chat = (0..4000)
            .map(|index| ChatLine {
                task_id: "task-a".into(),
                role: if index % 2 == 0 {
                    ChatRole::User
                } else {
                    ChatRole::Assistant
                },
                text: format!("line {index}: a moderately long message with `code` and some words"),
                streaming: false,
            })
            .collect();
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        // The first draw builds the cache; measure steady-state scrolling redraws.
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let start = Instant::now();
        for _ in 0..20 {
            terminal.draw(|frame| app.draw(frame)).unwrap();
        }
        let per_frame = start.elapsed() / 20;
        println!("long transcript draw cache-hit: {per_frame:?}/frame");
        assert!(per_frame < Duration::from_millis(250), "{per_frame:?}");
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn layout_survives_small_terminals() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app =
            DaemonApp::connect(PathBuf::from(root), 100, Some("scripted".into())).unwrap();
        app.setup = None;
        app.tasks = vec![json!({
            "task_id": "task-a", "goal": "a very long challenge title that must truncate cleanly",
            "state": "waiting_input", "provider": "scripted", "model": null,
            "effort": "default", "permission": "auto", "mode": "exploit",
            "last_error": null
        })];
        app.selected_task = 0;
        app.new_task = false;
        app.chat = vec![
            ChatLine {
                task_id: "task-a".into(),
                role: ChatRole::User,
                text: "inspect ./challenge and find the flag".into(),
                streaming: false,
            },
            ChatLine {
                task_id: "task-a".into(),
                role: ChatRole::Assistant,
                text: "# Findings\n- **TLS** on `443`\n```text\ncertificate expired\n```\nA long paragraph that should wrap without overflowing the terminal or panicking in narrow layouts.".into(),
                streaming: false,
            },
            ChatLine {
                task_id: "task-a".into(),
                role: ChatRole::Tool,
                text: "▸ session_exec id".into(),
                streaming: false,
            },
        ];
        for (width, height) in [(80, 24), (60, 20), (100, 30), (120, 40)] {
            let backend = TestBackend::new(width, height);
            let mut terminal = Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| app.draw(frame))
                .unwrap_or_else(|err| panic!("{width}x{height}: {err}"));
            let content: String = terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(content.contains("MitnickSolve"), "{width}x{height}");
        }
    }

    #[test]
    #[ignore = "requires a running mskd; set MSK_TUI_TEST_ROOT"]
    fn mode_change_for_an_unknown_task_uses_next_session() {
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
    fn mode_switch_applies_to_the_open_session() {
        let Ok(root) = std::env::var("MSK_TUI_TEST_ROOT") else {
            return;
        };
        let mut app =
            DaemonApp::connect(PathBuf::from(root), 100, Some("scripted".into())).unwrap();
        app.setup = None;
        app.create_task_with_goal("mode switch test", false);
        for _ in 0..100 {
            let _ = app.refresh();
            let _ = app.reload_tasks();
            if app.selected_state() == "waiting_input" {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(app.selected_state(), "waiting_input");
        let replacement = app
            .modes
            .iter()
            .position(|mode| mode.id == "plan")
            .expect("plan mode");
        app.mode_selection = replacement;
        app.show_modes = true;
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.new_task);
        assert_eq!(app.selected_mode(), "plan");
        assert_eq!(app.mode, "plan");
        assert!(!app.status.is_empty());
        app.cancel_task();
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
        let selected = app.selected_task_id();
        for _ in 0..100 {
            let _ = app.refresh();
            if app.chat.iter().any(|line| {
                Some(line.task_id.as_str()) == selected.as_deref()
                    && line.role == ChatRole::Assistant
                    && line.text.contains("echo: hello tui")
            }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert!(app.chat.iter().any(|line| {
            Some(line.task_id.as_str()) == selected.as_deref()
                && line.role == ChatRole::Assistant
                && line.text.contains("echo: hello tui")
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
        assert!(
            content.contains("攻击") || content.contains("攻 击") || content.contains("exploit")
        );
        assert!(content.contains("scripted"));
        app.cancel_task();
    }
}
