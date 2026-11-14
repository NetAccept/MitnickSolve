use std::io::{Stdout, stdout};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use crossterm::event::{self, Event as CrosstermEvent, KeyCode, KeyEventKind};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use msk_events::{Event, read_events};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

#[derive(Debug, Parser)]
struct Cli {
    /// Path to the JSONL event log produced by the kernel.
    #[arg(long, default_value = ".msk/events.jsonl")]
    events: PathBuf,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    enable_raw_mode()?;
    let mut out = stdout();
    execute!(out, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(out);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &cli.events);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, path: &Path) -> Result<()> {
    loop {
        let events = read_events(path).unwrap_or_default();
        terminal.draw(|frame| draw(frame, &events))?;

        if event::poll(Duration::from_millis(500))?
            && let CrosstermEvent::Key(key) = event::read()?
            && matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
        {
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Char('r') => continue,
                _ => {}
            }
        }
    }
}

fn draw(frame: &mut ratatui::Frame, events: &[Event]) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(1)])
        .split(frame.area());

    let header = Paragraph::new("MitnickSolve events  [q] quit  [r] refresh")
        .block(Block::default().borders(Borders::ALL).title("msk"));
    frame.render_widget(header, chunks[0]);

    let items: Vec<ListItem> = events
        .iter()
        .rev()
        .take(500)
        .map(|event| {
            let data = serde_json::to_string(&event.data).unwrap_or_default();
            ListItem::new(format!(
                "{}  {:<18} {:<8} {}",
                event.ts,
                event.kind,
                event.actor,
                truncate(&data, 120)
            ))
        })
        .collect();

    let list = List::new(items).block(Block::default().borders(Borders::ALL).title("event log"));
    frame.render_widget(list, chunks[1]);
}

fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        value.to_string()
    } else {
        format!("{}…", value.chars().take(max_chars).collect::<String>())
    }
}
