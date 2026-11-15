use std::cell::Cell;
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Stdout, stdout};
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
use ratatui::layout::{Constraint, Direction, Layout, Rect};
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
}

impl Default for TuiConfig {
    fn default() -> Self {
        Self {
            provider: default_provider(),
            model: String::new(),
            effort: default_effort(),
            base_url: String::new(),
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
    Model,
    CustomModel,
    Effort,
    BaseUrl,
}

#[derive(Debug, Clone)]
struct SetupState {
    step: SetupStep,
    provider: usize,
    model: usize,
    effort: usize,
    model_input: String,
    base_url: String,
    input_cursor: usize,
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
            model: model_index,
            effort: effort_index(&app.effort),
            model_input: if model_index == MODELS.len() - 1 {
                model
            } else {
                String::new()
            },
            base_url,
            input_cursor: 0,
        }
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
#[command(version, about = "MitnickSolve interactive CTF agent")]
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
    /// Diagnostic: open the setup wizard at provider, model, effort, or base.
    #[arg(long, value_parser = ["provider", "model", "effort", "base"])]
    setup_step: Option<String>,
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
                 j / k      previous / next task
                 g / G      first / last task
                 n          start a new task
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
    follow: bool,
    show_help: bool,
    status: String,
    refresh_ms: u64,
    last_refresh: Instant,
    provider: String,
    model: String,
    effort: String,
    base_url: String,
    daemon_llm_key: bool,
    daemon_llm_model: Option<String>,
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
}

impl DaemonApp {
    fn connect(root: PathBuf, refresh_ms: u64, provider: Option<String>) -> Result<Self> {
        let mut client =
            Client::connect(&root).with_context(|| format!("connect {}", root.display()))?;
        let hello = client.hello().context("kernel.hello")?;
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
        client.subscribe(None)?;
        let configured = root.join("tui-config.json").exists();
        let mut config = TuiConfig::load(&root);
        if let Some(provider) = provider {
            config.provider = provider;
        } else if !configured
            && std::env::var("MSK_LLM_MODEL").is_ok()
            && std::env::var("MSK_LLM_API_KEY").is_ok()
        {
            config.provider = "openai".into();
        }
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
            follow: true,
            show_help: false,
            status: "connected".into(),
            refresh_ms,
            last_refresh: Instant::now(),
            provider: config.provider,
            model: config.model,
            effort: config.effort,
            base_url: config.base_url,
            daemon_llm_key,
            daemon_llm_model,
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
        };
        app.reload_tasks()?;
        let _ = app.reload_approvals();
        if let Some(task_id) = app.selected_task_id() {
            app.load_transcript(&task_id);
        } else {
            app.new_task = true;
            app.composing = true;
            app.status = "describe a challenge to start".into();
        }
        Ok(app)
    }

    fn reload_tasks(&mut self) -> Result<()> {
        let result = self.client.request(methods::TASK_LIST, json!({}))?;
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
        let result = self.client.request(methods::APPROVAL_LIST, json!({}))?;
        self.approvals = result
            .get("approvals")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
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
                                    self.status = "initial prompt sent".into();
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
                        let text = if message.is_empty() {
                            format!("[{status}]")
                        } else {
                            format!("[{status}] {message}")
                        };
                        self.chat.push(ChatLine {
                            task_id: task_id.clone(),
                            role: ChatRole::System,
                            text,
                            streaming: false,
                        });
                        self.trim_chat();
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
                self.chat.push(ChatLine {
                    task_id: task_id.to_string(),
                    role: ChatRole::System,
                    text: format!("turn end: {status}"),
                    streaming: false,
                });
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
        let result = match self
            .client
            .request(methods::TASK_HISTORY, json!({"task_id": task_id}))
        {
            Ok(result) => result,
            Err(_) => match self
                .client
                .request(methods::TASK_RESULT, json!({"task_id": task_id}))
            {
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

    fn select_task(&mut self, delta: isize) {
        if self.tasks.is_empty() {
            return;
        }
        let len = self.tasks.len() as isize;
        self.select_task_to((self.selected_task as isize + delta).clamp(0, len - 1) as usize);
    }

    fn select_task_to(&mut self, index: usize) {
        if self.tasks.is_empty() {
            return;
        }
        self.selected_task = index.min(self.tasks.len() - 1);
        self.new_task = false;
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
                self.status = "initial prompt sent".into();
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

    fn selected_events(&self) -> Vec<usize> {
        let task_id = self.selected_task_id();
        self.events
            .iter()
            .enumerate()
            .filter(|(_, (id, _))| Some(id.as_str()) == task_id.as_deref())
            .map(|(index, _)| index)
            .collect()
    }

    fn selected_state(&self) -> &str {
        self.tasks
            .get(self.selected_task)
            .and_then(|task| task.get("state"))
            .and_then(Value::as_str)
            .unwrap_or("?")
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
            self.status = "new task: describe the challenge, Enter starts it".into();
        } else {
            self.status = "message: Enter sends, Esc leaves the editor".into();
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
                self.status = "message sent".into();
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
            "/demo" => {
                self.provider = "scripted-demo".into();
                self.create_task_with_goal("run the offline tool demo", true);
            }
            "/new" => {
                if argument.is_empty() {
                    self.new_task = true;
                    self.composing = true;
                    self.status = "new task: describe the challenge".into();
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
            "/approvals" => {
                self.status = if self.approvals.is_empty() {
                    "no pending approvals".into()
                } else {
                    format!("{} pending approval(s)", self.approvals.len())
                };
            }
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
                    self.status = format!("next task provider: {}", self.provider);
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
        if requires_key(&self.provider) {
            if !self.daemon_llm_key {
                self.status =
                    "provider needs MSK_LLM_API_KEY in mskd; export it, restart mskd, or press p"
                        .into();
                return;
            }
            if self.model.trim().is_empty() && self.daemon_llm_model.is_none() {
                self.status = "provider needs a model; press p to choose one".into();
                self.open_setup();
                return;
            }
        }
        let result = self.client.request(methods::TASK_CREATE, {
            let mut params = json!({
                "goal": goal,
                "demo": "session",
                "provider": self.provider,
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
                self.status = format!("created {task_id} with {}", self.provider);
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
                    self.status = format!("interrupt sent to {task_id}");
                } else {
                    self.status = format!("{task_id} is not running a turn");
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
                    format!("approved {id} for {task_id}")
                } else {
                    format!("denied {id} for {task_id}")
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
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.input.insert_str(self.input_cursor, &normalized);
        self.input_cursor += normalized.len();
        self.history_index = None;
        self.status = "pasted".into();
    }

    fn compose_key(&mut self, key: KeyEvent) -> bool {
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

    fn handle_key(&mut self, key: KeyEvent) -> bool {
        if self.setup.is_some() {
            return self.setup_key(key);
        }
        if self.terminal_focus {
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

        if !self.approvals.is_empty() {
            match key.code {
                KeyCode::Char('y') => self.respond_approval(true),
                KeyCode::Char('n') | KeyCode::Esc => self.respond_approval(false),
                KeyCode::Char('?') => self.show_help = !self.show_help,
                _ => {}
            }
            return false;
        }

        if self.composing {
            return self.compose_key(key);
        }

        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            if self.selected_state() == "running" {
                self.interrupt_task();
            } else {
                self.status = "press q to quit, or Enter to write".into();
            }
            return false;
        }
        if key.code == KeyCode::Esc && self.selected_state() == "running" {
            self.interrupt_task();
            return false;
        }

        match key.code {
            KeyCode::Char('q') => {
                self.close_terminal();
                return true;
            }
            KeyCode::Enter | KeyCode::Char('i') => self.enter_compose(),
            KeyCode::Char('?') => self.show_help = !self.show_help,
            KeyCode::Char('j') | KeyCode::Down => self.select_task(1),
            KeyCode::Char('k') | KeyCode::Up => self.select_task(-1),
            KeyCode::Char('g') => self.select_task_to(0),
            KeyCode::Char('G') => {
                let last = self.tasks.len().saturating_sub(1);
                self.select_task_to(last);
            }
            KeyCode::PageUp => {
                self.auto_scroll = false;
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(10);
            }
            KeyCode::PageDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(10);
                if self.scroll_from_bottom == 0 {
                    self.auto_scroll = true;
                }
            }
            KeyCode::End => {
                self.auto_scroll = true;
                self.scroll_from_bottom = 0;
            }
            KeyCode::Char('f') => self.follow = !self.follow,
            KeyCode::Char('v') => {
                self.show_events = !self.show_events;
                self.status = if self.show_events {
                    "event inspector on".into()
                } else {
                    "event inspector off".into()
                };
            }
            KeyCode::Char('r') => {
                if let Err(err) = self.reload_tasks() {
                    self.status = format!("reload failed: {err}");
                }
            }
            KeyCode::Char('n') => {
                self.new_task = true;
                self.composing = true;
                self.input.clear();
                self.input_cursor = 0;
                self.status = "new task: describe the challenge, Enter starts it".into();
            }
            KeyCode::Char('p') => self.open_setup(),
            KeyCode::Char('/') => {
                self.composing = true;
                self.input = "/".into();
                self.input_cursor = 1;
                self.status = "command: /new /provider /terminal /approvals /help".into();
            }
            KeyCode::Char('x') => self.cancel_task(),
            KeyCode::Char('s') => self.interrupt_task(),
            KeyCode::Char('a') => {
                self.status = if self.approvals.is_empty() {
                    "no pending approvals".into()
                } else {
                    format!("{} pending approval(s)", self.approvals.len())
                };
            }
            KeyCode::Char('t') => {
                self.composing = false;
                self.toggle_terminal();
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
                self.status = format!("cancelled {task_id}");
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

    fn draw(&self, frame: &mut ratatui::Frame) {
        let task_height = (self.tasks.len().min(5) as u16 + 2).max(3);
        let areas = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(task_height),
                Constraint::Min(1),
                Constraint::Length(5),
                Constraint::Length(1),
            ])
            .split(frame.area());

        let header = Paragraph::new(format!(
            "MitnickSolve  {}  ·  {} / {} / effort {}  ·  {} task(s)\n{}",
            self.root.display(),
            self.provider,
            if self.model.is_empty() {
                "-"
            } else {
                &self.model
            },
            self.effort,
            self.tasks.len(),
            self.status
        ))
        .block(Block::default().borders(Borders::ALL).title("session"));
        frame.render_widget(header, areas[0]);

        let items: Vec<ListItem> = self
            .tasks
            .iter()
            .map(|task| {
                let interactive = task
                    .get("interactive")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                ListItem::new(format!(
                    "{} {}  {:<12} {}",
                    if interactive { "◆" } else { " " },
                    task.get("task_id").and_then(Value::as_str).unwrap_or("?"),
                    task.get("state").and_then(Value::as_str).unwrap_or("?"),
                    truncate(task.get("goal").and_then(Value::as_str).unwrap_or(""), 60)
                ))
            })
            .collect();
        let mut task_state = ListState::default();
        if !items.is_empty() {
            task_state.select(Some(self.selected_task.min(items.len() - 1)));
        }
        let tasks = List::new(items)
            .block(Block::default().borders(Borders::ALL).title("tasks"))
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
            .highlight_symbol("› ");
        frame.render_stateful_widget(tasks, areas[1], &mut task_state);

        if self.terminal_focus {
            if let Some(terminal) = &self.terminal {
                let area = areas[2];
                let title = format!(
                    "terminal {} {}x{}  [Ctrl+Q] detach",
                    terminal.session_id, terminal.cols, terminal.rows
                );
                let block = Block::default().borders(Borders::ALL).title(title);
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
            frame.render_widget(
                Paragraph::new("terminal focused: Ctrl+Q detach").style(Style::default()),
                areas[3],
            );
            frame.render_widget(Paragraph::new("Ctrl+Q detach  q quit  ? help"), areas[4]);
            return;
        }

        let (conversation_area, event_area) = if self.show_events {
            let panes = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(66), Constraint::Percentage(34)])
                .split(areas[2]);
            (panes[0], Some(panes[1]))
        } else {
            (areas[2], None)
        };

        let task_id = self.selected_task_id();
        let mut transcript: Vec<Line> = Vec::new();
        for line in self
            .chat
            .iter()
            .filter(|line| Some(line.task_id.as_str()) == task_id.as_deref())
        {
            let (label, style) = match line.role {
                ChatRole::User => ("you: ", Style::default().fg(Color::Cyan)),
                ChatRole::Assistant => ("agent: ", Style::default().fg(Color::White)),
                ChatRole::Tool => ("tool: ", Style::default().fg(Color::Yellow)),
                ChatRole::System => (
                    "",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ),
            };
            let parts: Vec<&str> = line.text.split('\n').collect();
            for (index, part) in parts.iter().enumerate() {
                let prefix = if index == 0 {
                    label.to_string()
                } else {
                    " ".repeat(label.len())
                };
                let suffix = if line.streaming && index + 1 == parts.len() {
                    "▍"
                } else {
                    ""
                };
                transcript.push(Line::from(vec![Span::styled(
                    format!("{prefix}{part}{suffix}"),
                    style,
                )]));
            }
            transcript.push(Line::from(""));
        }
        if transcript.is_empty() {
            let hint = if self.tasks.is_empty() {
                "Welcome. Describe the challenge below and press Enter to start a task."
            } else {
                "No messages yet. Press Enter or i to write the first turn."
            };
            transcript.push(Line::from(Span::styled(
                hint,
                Style::default().fg(Color::DarkGray),
            )));
            transcript.push(Line::from(""));
            transcript.push(Line::from(Span::styled(
                "Examples:  inspect ./challenge and find the flag  ·  run the binary and explain the exploit",
                Style::default().fg(Color::DarkGray),
            )));
            transcript.push(Line::from(Span::styled(
                "Commands:  /new  /provider openai  /terminal  /approvals  /help",
                Style::default().fg(Color::DarkGray),
            )));
            transcript.push(Line::from(Span::styled(
                "Press p to choose provider, model and effort; /setup does the same.",
                Style::default().fg(Color::DarkGray),
            )));
            if self.provider == "scripted-demo" {
                transcript.push(Line::from(Span::styled(
                    "Offline demo provider: replies are canned. Use /provider openai for a real model.",
                    Style::default().fg(Color::Yellow),
                )));
            }
        }
        let transcript_title = match (&task_id, self.selected_state()) {
            (Some(id), state) => format!("conversation — {id} [{state}]"),
            (None, _) => "conversation".to_string(),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .title(transcript_title);
        let inner = block.inner(conversation_area);
        frame.render_widget(block, conversation_area);
        let paragraph = Paragraph::new(Text::from(transcript)).wrap(Wrap { trim: false });
        if inner.width > 0 && inner.height > 0 {
            let line_count = paragraph.line_count(inner.width);
            let max_scroll = line_count.saturating_sub(inner.height as usize);
            let scroll = if self.auto_scroll {
                max_scroll
            } else {
                max_scroll.saturating_sub(self.scroll_from_bottom as usize)
            };
            frame.render_widget(
                paragraph.scroll((scroll.min(u16::MAX as usize) as u16, 0)),
                inner,
            );
        }

        if let Some(event_area) = event_area {
            let visible = self.selected_events();
            let event_items: Vec<ListItem> = visible
                .iter()
                .rev()
                .take(400)
                .map(|index| {
                    let (_, event) = &self.events[*index];
                    ListItem::new(format!(
                        "{}  {:<18} {}",
                        short_time(&event.ts),
                        event.kind,
                        summarize(event, 60)
                    ))
                })
                .collect();
            let event_list = List::new(event_items)
                .block(Block::default().borders(Borders::ALL).title("events"));
            frame.render_widget(event_list, event_area);
        }

        let input_title = if self.new_task || task_id.is_none() {
            format!("new task — provider {}", self.provider)
        } else {
            match (&task_id, self.selected_state()) {
                (Some(id), state) => format!("message to {id} [{state}]"),
                (None, _) => "message".to_string(),
            }
        };
        let input_block = Block::default().borders(Borders::ALL).title(input_title);
        let input_inner = input_block.inner(areas[3]);
        frame.render_widget(input_block, areas[3]);
        if self.composing {
            let before = &self.input[..self.input_cursor];
            let row = before.matches('\n').count();
            let line_start = before.rfind('\n').map(|index| index + 1).unwrap_or(0);
            let cursor_cols = UnicodeWidthStr::width(&self.input[line_start..self.input_cursor]);
            let lines: Vec<&str> = self.input.split('\n').collect();
            let visible_rows = input_inner.height.max(1) as usize;
            let start_line = row.saturating_sub(visible_rows.saturating_sub(1));
            let display = lines
                .iter()
                .skip(start_line)
                .take(visible_rows)
                .copied()
                .collect::<Vec<_>>()
                .join("\n");
            let display = if display.is_empty() {
                "Describe the challenge…".to_string()
            } else {
                display
            };
            frame.render_widget(Paragraph::new(format!("> {display}")), input_inner);
            if input_inner.width > 2 && input_inner.height > 0 {
                let x = input_inner.x + 2 + (cursor_cols as u16);
                let y = input_inner.y + (row - start_line) as u16;
                frame.set_cursor_position((
                    x.min(input_inner.right().saturating_sub(1)),
                    y.min(input_inner.bottom().saturating_sub(1)),
                ));
            }
        } else {
            let hint = if self.new_task || task_id.is_none() {
                "Describe the challenge…  (Enter starts a task, / commands)"
            } else {
                "press Enter or i to write  ·  / commands"
            };
            frame.render_widget(
                Paragraph::new(format!("> {hint}")).style(Style::default().fg(Color::DarkGray)),
                input_inner,
            );
        }

        let footer = if !self.approvals.is_empty() {
            "APPROVAL PENDING: y approve  n deny"
        } else if self.composing {
            if self.new_task || task_id.is_none() {
                "Enter start task  Esc cancel  Shift+Enter newline  / commands"
            } else {
                "Enter send  Shift+Enter newline  Esc close editor  Ctrl+U clear  / commands"
            }
        } else if self.selected_state() == "running" {
            "running · Esc interrupt · Enter/i message · / commands · PgUp/PgDn scroll · ? help · q quit"
        } else {
            "Enter/i message  n new  / commands  PgUp/PgDn scroll  v events  j/k task  s interrupt  t terminal  ? help  q quit"
        };
        frame.render_widget(Paragraph::new(footer), areas[4]);

        if self.show_help {
            let area = centered_rect(70, 62, frame.area());
            frame.render_widget(Clear, area);
            let help = Paragraph::new(
                "mskd conversational workbench\n\n\
                 Enter / i  write a message to the selected task\n\
                 Enter      send (in the editor)\n\
                 Esc        leave the editor without sending\n\
                 ← / →      move the input cursor\n\
                 Ctrl+U     clear the input\n\
                 s          interrupt the running turn\n\
                 a          show pending approvals\n\
                 y / n      approve / deny the modal request\n\
                 j / k      previous / next task\n\
                 g / G      first / last task\n\
                 n          create an interactive scripted task\n\
                 x          cancel the selected task\n\
                 t          open/focus the PTY terminal\n\
                 Ctrl+Q     detach from terminal focus\n\
                 r          reload the task list\n\
                 f          follow new events\n\
                 a          show pending approvals\n\
                 ?          toggle this help\n\n\
                 ◆ marks an interactive task. Messages are accepted while the\n\
                 task is waiting_input; agent.text and turn.end stream over IPC.",
            )
            .block(Block::default().borders(Borders::ALL).title("help"))
            .wrap(Wrap { trim: false });
            frame.render_widget(help, area);
        }

        if self.setup.is_some() {
            self.draw_setup(frame);
        }

        if let Some(pending) = self.approvals.first() {
            let task_id = pending
                .get("task_id")
                .and_then(Value::as_str)
                .unwrap_or("?");
            let id = pending.get("id").and_then(Value::as_u64).unwrap_or(0);
            let tool = pending.get("tool").and_then(Value::as_str).unwrap_or("?");
            let reason = pending
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args = serde_json::to_string_pretty(pending.get("args").unwrap_or(&Value::Null))
                .unwrap_or_default();
            let area = centered_rect(78, 58, frame.area());
            frame.render_widget(Clear, area);
            let text = format!(
                "tool: {tool}\nreason: {reason}\n\nargs:\n{args}\n\n[y] approve   [n] deny"
            );
            let paragraph = Paragraph::new(text)
                .block(Block::default().borders(Borders::ALL).title(format!(
                    "approval required — {task_id} #{id}  (1/{})",
                    self.approvals.len()
                )))
                .wrap(Wrap { trim: false });
            frame.render_widget(paragraph, area);
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
        return snapshot_daemon(root, provider, cli.setup, cli.setup_step.clone());
    }

    enable_raw_mode()?;
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
) -> Result<()> {
    let mut app = DaemonApp::connect(root.to_path_buf(), 100, provider)?;
    let _ = app.refresh();
    if setup {
        app.open_setup();
        open_setup_step(&mut app, setup_step.as_deref());
    }
    app.auto_scroll = false;
    app.scroll_from_bottom = 10_000;
    let backend = ratatui::backend::TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend)?;
    terminal.draw(|frame| app.draw(frame))?;
    let buffer = terminal.backend().buffer();
    for y in 0..buffer.area.height {
        let mut line = String::new();
        for x in 0..buffer.area.width {
            line.push_str(buffer[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
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
    let key = std::env::var("MSK_LLM_API_KEY").is_ok();
    println!(
        "llm: model={model} effort={effort} base={base} key={}",
        if key { "set" } else { "missing" }
    );
    if provider == "openai" || provider == "openai-responses" {
        if model == "-" || !key {
            println!(
                "hint: export MSK_LLM_MODEL, MSK_LLM_API_KEY, and optionally MSK_LLM_BASE_URL"
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
        "model" => SetupStep::Model,
        "effort" => SetupStep::Effort,
        _ => SetupStep::BaseUrl,
    };
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
        app.handle_key(enter); // provider -> model
        app.handle_key(enter); // custom model -> text input
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        for character in "gpt-5".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE));
        }
        app.handle_key(enter); // model -> effort
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        app.handle_key(enter); // effort -> base URL
        app.handle_key(enter); // save
        assert!(app.setup.is_none());
        assert_eq!(app.model, "gpt-5");
        assert_eq!(app.effort, "minimal");
        assert!(app.root.join("tui-config.json").exists());
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
        assert!(content.contains("conversation"));
        assert!(content.contains("echo: hello tui"));
        app.cancel_task();
    }
}
