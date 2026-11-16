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
                            setup.input_cursor = setup.model_input.len();
                        }
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
                SetupStep::Effort => setup.step = SetupStep::BaseUrl,
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
        let area = centered_rect(88, 76, frame.area());
        frame.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title("setup — provider / model / effort / base URL");
        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();
        let provider = setup.selected_provider();
        match setup.step {
            SetupStep::Provider => {
                lines.push(Line::from(Span::styled(
                    "Choose how the agent reaches a model.",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(""));
                for (index, (name, description)) in PROVIDERS.iter().enumerate() {
                    let selected = index == setup.provider;
                    lines.push(Line::from(vec![
                        Span::styled(
                            if selected { "› " } else { "  " },
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(
                            *name,
                            if selected {
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default()
                            },
                        ),
                        Span::styled(
                            format!("  — {description}"),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                }
            }
            SetupStep::Model => {
                lines.push(Line::from(Span::styled(
                    format!("Model for {provider}. Choose a preset or custom."),
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(""));
                for (index, name) in MODELS.iter().enumerate() {
                    let selected = index == setup.model;
                    let label = if *name == "custom…" && !setup.model_input.is_empty() {
                        format!("custom… ({})", setup.model_input)
                    } else {
                        (*name).to_string()
                    };
                    lines.push(Line::from(vec![
                        Span::styled(
                            if selected { "› " } else { "  " },
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(
                            label,
                            if selected {
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default()
                            },
                        ),
                    ]));
                }
                if !self.model.is_empty() {
                    lines.push(Line::from(""));
                    lines.push(Line::from(Span::styled(
                        format!("current: {}", self.model),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            SetupStep::CustomModel => {
                lines.push(Line::from(Span::styled(
                    "Type the exact model name.",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(format!("> {}", setup.model_input)));
            }
            SetupStep::Effort => {
                lines.push(Line::from(Span::styled(
                    "Reasoning effort is sent to OpenAI-compatible providers that support it.",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(""));
                for (index, (name, description)) in EFFORTS.iter().enumerate() {
                    let selected = index == setup.effort;
                    lines.push(Line::from(vec![
                        Span::styled(
                            if selected { "› " } else { "  " },
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(
                            *name,
                            if selected {
                                Style::default()
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default()
                            },
                        ),
                        Span::styled(
                            format!("  — {description}"),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]));
                }
            }
            SetupStep::BaseUrl => {
                lines.push(Line::from(Span::styled(
                    "OpenAI-compatible base URL. Empty uses MSK_LLM_BASE_URL or the provider default.",
                    Style::default().fg(Color::DarkGray),
                )));
                lines.push(Line::from(""));
                lines.push(Line::from(format!("> {}", setup.base_url)));
            }
        }
        lines.push(Line::from(""));
        let key_state = if self.daemon_llm_key {
            "mskd has MSK_LLM_API_KEY"
        } else {
            "mskd is missing MSK_LLM_API_KEY (export it before starting mskd)"
        };
        lines.push(Line::from(Span::styled(
            key_state,
            Style::default().fg(if requires_key(provider) && !self.daemon_llm_key {
                Color::Yellow
            } else {
                Color::DarkGray
            }),
        )));
        lines.push(Line::from(Span::styled(
            format!("config: {}", self.root.join("tui-config.json").display()),
            Style::default().fg(Color::DarkGray),
        )));
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "↑/↓ choose · Enter next · Esc back/cancel · Ctrl+U clear input",
            Style::default().fg(Color::Cyan),
        )));

        let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
        frame.render_widget(paragraph, inner);
        if matches!(setup.step, SetupStep::CustomModel | SetupStep::BaseUrl) && inner.width > 2 {
            let value = if setup.step == SetupStep::CustomModel {
                &setup.model_input
            } else {
                &setup.base_url
            };
            let width = UnicodeWidthStr::width(&value[..setup.input_cursor]) as u16;
            frame.set_cursor_position((
                (inner.x + 2 + width).min(inner.right().saturating_sub(1)),
                inner.y + 3,
            ));
        }
    }
}
