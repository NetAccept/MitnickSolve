use super::*;

impl DaemonApp {
    pub(crate) fn open_setup(&mut self) {
        self.composing = false;
        self.setup = Some(SetupState::from_app(self));
        self.status = "setup: provider → model → effort → base URL".into();
    }

    pub(crate) fn open_setup_at(&mut self, step: SetupStep) {
        self.open_setup();
        if let Some(setup) = self.setup.as_mut() {
            setup.step = step;
        }
    }

    pub(crate) fn setup_key(&mut self, key: KeyEvent) -> bool {
        let Some(mut setup) = self.setup.take() else {
            return false;
        };
        if matches!(setup.step, SetupStep::CustomModel | SetupStep::BaseUrl) {
            let length = if setup.step == SetupStep::CustomModel {
                setup.model_input.len()
            } else {
                setup.base_url.len()
            };
            setup.input_cursor = clamp_cursor(setup.input_cursor, length);
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && key.code == KeyCode::Char('u') {
            match setup.step {
                SetupStep::CustomModel => {
                    setup.model_input.clear();
                    setup.input_cursor = 0;
                }
                SetupStep::BaseUrl => {
                    setup.base_url.clear();
                    setup.input_cursor = 0;
                }
                _ => {}
            }
            self.setup = Some(setup);
            return false;
        }
        match key.code {
            KeyCode::Esc => match setup.step {
                SetupStep::Provider => {
                    self.status = "setup cancelled".into();
                    return false;
                }
                SetupStep::Model => setup.step = SetupStep::Provider,
                SetupStep::CustomModel => setup.step = SetupStep::Model,
                SetupStep::Effort => setup.step = SetupStep::Model,
                SetupStep::BaseUrl => setup.step = SetupStep::Effort,
            },
            KeyCode::Up | KeyCode::Char('k')
                if !matches!(setup.step, SetupStep::CustomModel | SetupStep::BaseUrl) =>
            {
                match setup.step {
                    SetupStep::Provider => setup.provider = setup.provider.saturating_sub(1),
                    SetupStep::Model => setup.model = setup.model.saturating_sub(1),
                    SetupStep::Effort => setup.effort = setup.effort.saturating_sub(1),
                    _ => {}
                }
            }
            KeyCode::Down | KeyCode::Char('j')
                if !matches!(setup.step, SetupStep::CustomModel | SetupStep::BaseUrl) =>
            {
                match setup.step {
                    SetupStep::Provider => {
                        setup.provider = (setup.provider + 1).min(PROVIDERS.len() - 1)
                    }
                    SetupStep::Model => setup.model = (setup.model + 1).min(MODELS.len() - 1),
                    SetupStep::Effort => setup.effort = (setup.effort + 1).min(EFFORTS.len() - 1),
                    _ => {}
                }
            }
            KeyCode::Enter => match setup.step {
                SetupStep::Provider => {
                    if !requires_key(setup.selected_provider()) {
                        self.apply_setup(&setup);
                        return false;
                    }
                    setup.step = SetupStep::Model;
                    if setup.model < MODELS.len() - 1 && !self.model.is_empty() {
                        setup.model = model_index(&self.model);
                    }
                }
                SetupStep::Model => {
                    if setup.model >= MODELS.len() - 1 {
                        if setup.model_input.is_empty() {
                            setup.model_input = self.model.clone();
                        }
                        setup.input_cursor = setup.model_input.len();
                        setup.step = SetupStep::CustomModel;
                    } else {
                        setup.step = SetupStep::Effort;
                    }
                }
                SetupStep::CustomModel => {
                    if setup.model_input.trim().is_empty() {
                        self.status = "enter a model name or Esc to go back".into();
                    } else {
                        setup.step = SetupStep::Effort;
                    }
                }
                SetupStep::Effort => {
                    setup.input_cursor = setup.base_url.len();
                    setup.step = SetupStep::BaseUrl;
                }
                SetupStep::BaseUrl => {
                    self.apply_setup(&setup);
                    return false;
                }
            },
            KeyCode::Char(ch)
                if !control
                    && !ch.is_control()
                    && matches!(setup.step, SetupStep::CustomModel | SetupStep::BaseUrl) =>
            {
                let field = if setup.step == SetupStep::CustomModel {
                    &mut setup.model_input
                } else {
                    &mut setup.base_url
                };
                field.insert(setup.input_cursor, ch);
                setup.input_cursor += ch.len_utf8();
            }
            KeyCode::Backspace
                if matches!(setup.step, SetupStep::CustomModel | SetupStep::BaseUrl) =>
            {
                let field = if setup.step == SetupStep::CustomModel {
                    &mut setup.model_input
                } else {
                    &mut setup.base_url
                };
                if let Some((index, _)) = field[..setup.input_cursor].char_indices().last() {
                    field.remove(index);
                    setup.input_cursor = index;
                }
            }
            KeyCode::Left if matches!(setup.step, SetupStep::CustomModel | SetupStep::BaseUrl) => {
                let field = if setup.step == SetupStep::CustomModel {
                    &setup.model_input
                } else {
                    &setup.base_url
                };
                if let Some((index, _)) = field[..setup.input_cursor].char_indices().last() {
                    setup.input_cursor = index;
                }
            }
            KeyCode::Right if matches!(setup.step, SetupStep::CustomModel | SetupStep::BaseUrl) => {
                let field = if setup.step == SetupStep::CustomModel {
                    &setup.model_input
                } else {
                    &setup.base_url
                };
                if let Some(character) = field[setup.input_cursor..].chars().next() {
                    setup.input_cursor += character.len_utf8();
                }
            }
            KeyCode::Home if matches!(setup.step, SetupStep::CustomModel | SetupStep::BaseUrl) => {
                setup.input_cursor = 0;
            }
            KeyCode::End if matches!(setup.step, SetupStep::CustomModel | SetupStep::BaseUrl) => {
                setup.input_cursor = if setup.step == SetupStep::CustomModel {
                    setup.model_input.len()
                } else {
                    setup.base_url.len()
                };
            }
            _ => {}
        }
        self.setup = Some(setup);
        false
    }

    fn apply_setup(&mut self, setup: &SetupState) {
        self.provider = setup.selected_provider().to_string();
        if requires_key(&self.provider) {
            self.model = setup.selected_model();
            self.effort = EFFORTS[setup.effort.min(EFFORTS.len() - 1)].0.to_string();
            self.base_url = setup.base_url.trim().to_string();
        } else {
            self.model.clear();
            self.effort = default_effort();
            self.base_url.clear();
        }
        let config = TuiConfig {
            provider: self.provider.clone(),
            model: self.model.clone(),
            effort: self.effort.clone(),
            base_url: self.base_url.clone(),
            mode: self.mode.clone(),
            permission: self.permission.clone(),
        };
        if self.tasks.is_empty() {
            self.new_task = true;
            self.composing = true;
        }
        match config.save(&self.root) {
            Ok(()) => {
                self.status = if requires_key(&self.provider) && !self.daemon_llm_key {
                    "setup saved — mskd is missing MSK_LLM_API_KEY".into()
                } else {
                    format!(
                        "setup saved: {} / {} / effort {}",
                        self.provider,
                        if self.model.is_empty() {
                            "-"
                        } else {
                            &self.model
                        },
                        self.effort
                    )
                };
            }
            Err(err) => self.status = format!("save setup failed: {err}"),
        }
    }

    pub(crate) fn draw_setup(&self, frame: &mut ratatui::Frame) {
        let Some(setup) = &self.setup else {
            return;
        };
        let outer = frame.area();
        let width = 84.min(outer.width.saturating_sub(6)).max(32);
        let height = 20.min(outer.height.saturating_sub(2)).max(14);
        let area = Rect {
            x: outer.x + outer.width.saturating_sub(width) / 2,
            y: outer.y + outer.height.saturating_sub(height) / 2,
            width,
            height,
        };
        frame.render_widget(Clear, area);
        let block = panel_block("setup", ACCENT);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width < 20 || inner.height < 8 {
            return;
        }

        let provider = setup.selected_provider();
        let mut lines: Vec<Line> = Vec::new();
        lines.push(Line::from(vec![
            Span::styled(
                "MitnickSolve",
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("   connect a model", Style::default().fg(DIM)),
        ]));
        lines.push(Line::from(Span::styled(
            "You can change all of this later with ctrl+p.",
            Style::default().fg(FAINT),
        )));
        lines.push(Line::from(""));
        lines.push(step_line(setup.step));
        lines.push(Line::from(""));

        match setup.step {
            SetupStep::Provider => {
                lines.push(Line::from(Span::styled(
                    "Choose how the agent reaches a model",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(Span::styled(
                    "scripted-demo needs no key and works offline.",
                    Style::default().fg(DIM),
                )));
                lines.push(Line::from(""));
                for (index, (name, description)) in PROVIDERS.iter().enumerate() {
                    lines.push(option_line(index == setup.provider, *name, description));
                }
            }
            SetupStep::Model => {
                lines.push(Line::from(Span::styled(
                    format!("Model for {provider}"),
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(Span::styled(
                    "Choose a preset, or type an exact model id.",
                    Style::default().fg(DIM),
                )));
                lines.push(Line::from(""));
                for (index, name) in MODELS.iter().enumerate() {
                    let label = if *name == "custom…" && !setup.model_input.is_empty() {
                        format!("custom ({})", setup.model_input)
                    } else {
                        (*name).to_string()
                    };
                    lines.push(option_line(
                        index == setup.model,
                        &label,
                        if *name == "custom…" {
                            "type any model id"
                        } else if *name == "gpt-5" {
                            "flagship reasoning model"
                        } else {
                            "OpenAI-compatible model"
                        },
                    ));
                }
            }
            SetupStep::CustomModel => {
                lines.push(Line::from(Span::styled(
                    "Exact model name",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(Span::styled(
                    "This is sent unchanged to the provider.",
                    Style::default().fg(DIM),
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled("❯ ", Style::default().fg(ACCENT)),
                    Span::styled(setup.model_input.clone(), Style::default().fg(TEXT)),
                    Span::styled("▌", Style::default().fg(ACCENT)),
                ]));
            }
            SetupStep::Effort => {
                lines.push(Line::from(Span::styled(
                    "Reasoning effort",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(Span::styled(
                    "Sent to OpenAI-compatible providers that support it.",
                    Style::default().fg(DIM),
                )));
                lines.push(Line::from(""));
                for (index, (name, description)) in EFFORTS.iter().enumerate() {
                    lines.push(option_line(index == setup.effort, *name, description));
                }
            }
            SetupStep::BaseUrl => {
                lines.push(Line::from(Span::styled(
                    "Endpoint",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(Span::styled(
                    "OpenAI-compatible base URL. Empty uses MSK_LLM_BASE_URL or the provider default.",
                    Style::default().fg(DIM),
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(vec![
                    Span::styled("❯ ", Style::default().fg(ACCENT)),
                    Span::styled(setup.base_url.clone(), Style::default().fg(TEXT)),
                    Span::styled("▌", Style::default().fg(ACCENT)),
                ]));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Example: http://127.0.0.1:8080/v1",
                    Style::default().fg(FAINT),
                )));
            }
        }

        let input_line = match setup.step {
            SetupStep::CustomModel | SetupStep::BaseUrl => Some(lines.len().saturating_sub(1)),
            _ => None,
        };

        lines.push(Line::from(""));
        lines.push(Line::from(""));
        let key_state = if self.daemon_llm_key {
            "mskd has MSK_LLM_API_KEY"
        } else {
            "mskd is missing MSK_LLM_API_KEY (export it before starting mskd)"
        };
        lines.push(Line::from(Span::styled(
            key_state,
            Style::default().fg(if requires_key(provider) && !self.daemon_llm_key {
                YELLOW
            } else {
                FAINT
            }),
        )));
        lines.push(Line::from(Span::styled(
            format!("config  {}", self.root.join("tui-config.json").display()),
            Style::default().fg(FAINT),
        )));
        lines.push(Line::from(Span::styled(
            "↑/↓ choose   Enter next   Esc back/close   Ctrl+U clear input",
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
        if let Some(line_index) = input_line {
            if let Some((value, cursor)) = match setup.step {
                SetupStep::CustomModel => Some((&setup.model_input, setup.input_cursor)),
                SetupStep::BaseUrl => Some((&setup.base_url, setup.input_cursor)),
                _ => None,
            } {
                let width = UnicodeWidthStr::width(&value[..cursor.min(value.len())]) as u16;
                let y = inner.y + line_index as u16;
                let x = inner.x + 3 + width;
                if y < inner.bottom() {
                    frame.set_cursor_position((x.min(inner.right().saturating_sub(2)), y));
                }
            }
        }
    }
}

fn step_line(step: SetupStep) -> Line<'static> {
    let steps = [
        ("provider", SetupStep::Provider),
        ("model", SetupStep::Model),
        ("effort", SetupStep::Effort),
        ("endpoint", SetupStep::BaseUrl),
    ];
    let active = match step {
        SetupStep::CustomModel => SetupStep::Model,
        other => other,
    };
    let mut spans = Vec::new();
    for (index, (label, candidate)) in steps.iter().enumerate() {
        if index > 0 {
            spans.push(Span::styled("  ›  ", Style::default().fg(FAINT)));
        }
        if *candidate == active {
            spans.push(Span::styled(
                format!("● {label}"),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                format!("○ {label}"),
                Style::default().fg(FAINT),
            ));
        }
    }
    Line::from(spans)
}

fn clamp_cursor(cursor: usize, length: usize) -> usize {
    cursor.min(length)
}

fn option_line(selected: bool, name: &str, description: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            if selected { "❯ " } else { "  " },
            Style::default().fg(ACCENT),
        ),
        Span::styled(
            format!("{name:<20}"),
            if selected {
                Style::default().fg(TEXT).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(TEXT)
            },
        ),
        Span::styled(description.to_string(), Style::default().fg(DIM)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_is_clamped_when_switching_to_an_empty_field() {
        assert_eq!(clamp_cursor(24, 0), 0);
        assert_eq!(clamp_cursor(4, 0), 0);
        assert_eq!(clamp_cursor(2, 7), 2);
    }
}
