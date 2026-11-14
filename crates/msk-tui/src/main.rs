use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom, Stdout, stdout};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{self, Event as CrosstermEvent, KeyCode, KeyEvent, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use msk_events::{Event, read_events};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

const MAX_EVENTS: usize = 5000;
const MAX_DETAIL_LINES: usize = 400;

#[derive(Debug, Parser)]
struct Cli {
    /// Path to the JSONL event log produced by the kernel.
    #[arg(long, default_value = ".msk/events.jsonl")]
    events: PathBuf,
    /// Refresh interval in milliseconds.
    #[arg(long, default_value_t = 500)]
    refresh_ms: u64,
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
                "MitnickSolve event inspector\n\n\
                 q          quit\n\
                 j / k      move selection\n\
                 g / G      first / last event\n\
                 tab        cycle filter\n\
                 1..5       all / llm / tools / artifacts / context\n\
                 /          search; enter applies, esc clears\n\
                 f          follow new events\n\
                 r          reload from disk\n\
                 ?          toggle this help\n\n\
                 Events are read incrementally from the JSONL log.\n\
                 Artifacts are referenced by ObjectRef in data.put events.",
            )
            .block(Block::default().borders(Borders::ALL).title("help"))
            .wrap(Wrap { trim: false });
            frame.render_widget(help, area);
        }
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, cli);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, cli: Cli) -> Result<()> {
    let mut app = App::new(cli.events);
    // Load any existing events before the first draw.
    let _ = app.refresh();

    loop {
        app.refresh()?;
        terminal.draw(|frame| app.draw(frame))?;

        if event::poll(Duration::from_millis(cli.refresh_ms))?
            && let CrosstermEvent::Key(key) = event::read()?
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            && app.handle_key(key)
        {
            return Ok(());
        }
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
}
