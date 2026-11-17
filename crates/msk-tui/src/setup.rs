use super::*;

impl DaemonApp {
    pub(crate) fn open_setup(&mut self) {
        self.composing = false;
        self.setup = Some(SetupState::from_app(self));
        self.status = "setup: provider → service → model → effort → endpoint → key".into();
    }

    pub(crate) fn open_setup_new(&mut self) {
        self.composing = false;
        let mut setup = SetupState::from_app(self);
        setup.step = SetupStep::Provider;
        setup.model = MODELS.len() - 1;
        setup.model_input.clear();
        setup.base_url.clear();
        setup.api_key.clear();
        setup.input_cursor = 0;
        setup.edit_profile = None;
        self.setup = Some(setup);
        self.status = "new provider profile · choose a provider".into();
    }

    pub(crate) fn open_setup_edit_profile(&mut self, index: usize) {
        let Some(setup) = SetupState::from_profile(self, index) else {
            return;
        };
        self.composing = false;
        self.show_providers = false;
        self.provider_delete_armed = None;
        self.setup = Some(setup);
        self.status = "edit provider profile · choose a provider".into();
    }

    pub(crate) fn open_setup_at(&mut self, step: SetupStep) {
        self.open_setup();
        if let Some(setup) = self.setup.as_mut() {
            setup.step = step;
        }
    }

    /// Pasting a key into the wizard must never fall through to the composer.
    pub(crate) fn setup_paste(&mut self, text: &str) {
        let Some(mut setup) = self.setup.take() else {
            return;
        };
        if is_text_step(setup.step) {
            let cleaned = match setup.step {
                SetupStep::ApiKey => clean_key_paste(text),
                _ => text
                    .replace("\r\n", " ")
                    .replace(['\r', '\n'], " ")
                    .trim()
                    .to_string(),
            };
            if !cleaned.is_empty() {
                let cursor = setup.input_cursor;
                if let Some(field) = setup.text_field() {
                    field.insert_str(cursor, &cleaned);
                }
                setup.input_cursor += cleaned.len();
                self.status = if setup.step == SetupStep::ApiKey {
                    "key pasted (hidden)".into()
                } else {
                    "pasted".into()
                };
            }
        }
        self.setup = Some(setup);
    }

    pub(crate) fn setup_key(&mut self, key: KeyEvent) -> bool {
        let Some(mut setup) = self.setup.take() else {
            return false;
        };
        if let Some(length) = setup.text_len() {
            setup.input_cursor = clamp_cursor(setup.input_cursor, length);
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control && key.code == KeyCode::Char('u') {
            if let Some(field) = setup.text_field() {
                field.clear();
            }
            if is_text_step(setup.step) {
                setup.input_cursor = 0;
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
                SetupStep::Model => setup.step = SetupStep::Preset,
                SetupStep::Preset => setup.step = SetupStep::Provider,
                SetupStep::CustomModel => setup.step = SetupStep::Model,
                SetupStep::Effort => setup.step = SetupStep::Model,
                SetupStep::BaseUrl => setup.step = SetupStep::Effort,
                SetupStep::ApiKey => setup.step = SetupStep::BaseUrl,
            },
            KeyCode::Up | KeyCode::Char('k') if !is_text_step(setup.step) => match setup.step {
                SetupStep::Provider => setup.provider = setup.provider.saturating_sub(1),
                SetupStep::Preset => setup.preset = setup.preset.saturating_sub(1),
                SetupStep::Model => setup.model = setup.model.saturating_sub(1),
                SetupStep::Effort => setup.effort = setup.effort.saturating_sub(1),
                _ => {}
            },
            KeyCode::Down | KeyCode::Char('j') if !is_text_step(setup.step) => match setup.step {
                SetupStep::Provider => {
                    setup.provider = (setup.provider + 1).min(PROVIDERS.len() - 1)
                }
                SetupStep::Preset => {
                    setup.preset = (setup.preset + 1).min(SERVICE_PRESETS.len() - 1)
                }
                SetupStep::Model => setup.model = (setup.model + 1).min(MODELS.len() - 1),
                SetupStep::Effort => setup.effort = (setup.effort + 1).min(EFFORTS.len() - 1),
                _ => {}
            },
            KeyCode::Enter => match setup.step {
                SetupStep::Provider => {
                    if !requires_key(setup.selected_provider()) {
                        self.apply_setup(&setup);
                        return false;
                    }
                    setup.step = SetupStep::Preset;
                }
                SetupStep::Preset => {
                    let (_, _, url, model) =
                        SERVICE_PRESETS[setup.preset.min(SERVICE_PRESETS.len() - 1)];
                    if !url.is_empty() {
                        setup.base_url = (*url).to_string();
                    }
                    // Only preselect a model for a fresh profile; never overwrite
                    // the model an existing profile already uses.
                    if !model.is_empty()
                        && setup.model_input.is_empty()
                        && setup.model >= MODELS.len() - 1
                    {
                        let index = model_index(model);
                        setup.model = index;
                        setup.model_input = if index == MODELS.len() - 1 {
                            (*model).to_string()
                        } else {
                            String::new()
                        };
                    }
                    setup.step = SetupStep::Model;
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
                    if requires_key(setup.selected_provider()) {
                        setup.input_cursor = setup.api_key.len();
                        setup.step = SetupStep::ApiKey;
                    } else {
                        self.apply_setup(&setup);
                        return false;
                    }
                }
                SetupStep::ApiKey => {
                    self.apply_setup(&setup);
                    return false;
                }
            },
            KeyCode::Char(ch) if !control && !ch.is_control() && is_text_step(setup.step) => {
                let cursor = setup.input_cursor;
                if let Some(field) = setup.text_field() {
                    field.insert(cursor, ch);
                }
                setup.input_cursor += ch.len_utf8();
            }
            KeyCode::Backspace if is_text_step(setup.step) => {
                let cursor = setup.input_cursor;
                let mut removed = None;
                if let Some(field) = setup.text_field() {
                    let offset = cursor.min(field.len());
                    if let Some((index, _)) = field[..offset].char_indices().last() {
                        field.remove(index);
                        removed = Some(index);
                    }
                }
                if let Some(index) = removed {
                    setup.input_cursor = index;
                }
            }
            KeyCode::Left if is_text_step(setup.step) => {
                let cursor = setup.input_cursor;
                if let Some(field) = setup.text_ref() {
                    if let Some((index, _)) = field[..cursor.min(field.len())].char_indices().last()
                    {
                        setup.input_cursor = index;
                    }
                }
            }
            KeyCode::Right if is_text_step(setup.step) => {
                let cursor = setup.input_cursor;
                let next = setup
                    .text_ref()
                    .and_then(|field| field.get(cursor..))
                    .and_then(|rest| rest.chars().next())
                    .map(|ch| cursor + ch.len_utf8());
                if let Some(next) = next {
                    setup.input_cursor = next;
                }
            }
            KeyCode::Home if is_text_step(setup.step) => {
                setup.input_cursor = 0;
            }
            KeyCode::End if is_text_step(setup.step) => {
                setup.input_cursor = setup.text_len().unwrap_or(0);
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
            self.api_key = setup.api_key.trim().to_string();
        } else {
            self.model.clear();
            self.effort = default_effort();
            self.base_url.clear();
            self.api_key.clear();
        }
        let profile_label = self.upsert_profile(setup);
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
        self.show_providers = false;
        self.provider_delete_armed = None;
        if self.tasks.is_empty() {
            self.new_task = true;
            self.composing = true;
        }
        match config.save(&self.root) {
            Ok(()) => {
                let mut status = format!(
                    "saved {} · {} / {} / effort {}",
                    profile_label.as_deref().unwrap_or("provider"),
                    self.provider,
                    if self.model.is_empty() {
                        "-"
                    } else {
                        &self.model
                    },
                    self.effort
                );
                if requires_key(&self.provider) && !self.has_llm_key() {
                    status.push_str(" · API key missing");
                }
                self.status = status;
            }
            Err(err) => self.status = format!("save setup failed: {err}"),
        }
    }

    /// Saves the wizard result as a provider profile. Editing replaces the
    /// selected entry; otherwise an entry with the same endpoint is updated.
    fn upsert_profile(&mut self, setup: &SetupState) -> Option<String> {
        let provider = setup.selected_provider().to_string();
        let needs_key = requires_key(&provider);
        let profile = ProviderProfile {
            name: String::new(),
            provider: provider.clone(),
            model: if needs_key {
                setup.selected_model()
            } else {
                String::new()
            },
            effort: if needs_key {
                EFFORTS[setup.effort.min(EFFORTS.len() - 1)].0.to_string()
            } else {
                default_effort()
            },
            base_url: if needs_key {
                setup.base_url.trim().to_string()
            } else {
                String::new()
            },
            api_key: if needs_key {
                setup.api_key.trim().to_string()
            } else {
                String::new()
            },
        };
        if let Some(index) = setup.edit_profile {
            let existing = self.profiles.get(index);
            let existing_name = existing
                .map(|profile| profile.name.clone())
                .unwrap_or_default();
            let auto_name = existing
                .map(|profile| profile_name(&profile.provider, &profile.model))
                .unwrap_or_default();
            let name = if existing_name.trim().is_empty() || existing_name == auto_name {
                let base = profile_name(&profile.provider, &profile.model);
                unique_profile_name_excluding(&self.profiles, &base, Some(index))
            } else {
                existing_name
            };
            if let Some(existing) = self.profiles.get_mut(index) {
                *existing = ProviderProfile {
                    name: name.clone(),
                    ..profile
                };
                return Some(name);
            }
        }
        if let Some(existing) = self.profiles.iter_mut().find(|existing| {
            existing.provider == profile.provider
                && existing.model == profile.model
                && existing.base_url == profile.base_url
        }) {
            existing.effort = profile.effort.clone();
            if !profile.api_key.is_empty() {
                existing.api_key = profile.api_key.clone();
            }
            if existing.name.trim().is_empty() {
                existing.name = profile_name(&profile.provider, &profile.model);
            }
            return Some(existing.name.clone());
        }
        let base = profile_name(&profile.provider, &profile.model);
        let name = unique_profile_name(&self.profiles, &base);
        self.profiles.push(ProviderProfile {
            name: name.clone(),
            ..profile
        });
        Some(name)
    }

    pub(crate) fn draw_setup(&self, frame: &mut ratatui::Frame) {
        let Some(setup) = &self.setup else {
            return;
        };
        let outer = frame.area();
        let width = 84.min(outer.width.saturating_sub(6)).max(32);
        let height = 27.min(outer.height.saturating_sub(2)).max(14);
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
        lines.push(step_line(setup.step, requires_key(provider)));
        lines.push(Line::from(""));

        let mut input_line: Option<usize> = None;
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
            SetupStep::Preset => {
                lines.push(Line::from(Span::styled(
                    "Service endpoint",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(Span::styled(
                    "Presets fill the base URL; choose or type a model next.",
                    Style::default().fg(DIM),
                )));
                lines.push(Line::from(""));
                for (index, (_, label, url, _)) in SERVICE_PRESETS.iter().enumerate() {
                    lines.push(option_line(
                        index == setup.preset,
                        label,
                        if url.is_empty() {
                            "keep the endpoint you type in the next step"
                        } else {
                            url
                        },
                    ));
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
                input_line = Some(lines.len().saturating_sub(1));
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
                input_line = Some(lines.len().saturating_sub(1));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    "Example: http://127.0.0.1:8080/v1",
                    Style::default().fg(FAINT),
                )));
            }
            SetupStep::ApiKey => {
                lines.push(Line::from(Span::styled(
                    "API key",
                    Style::default().fg(TEXT).add_modifier(Modifier::BOLD),
                )));
                lines.push(Line::from(Span::styled(
                    "Sent to mskd for tasks using this profile, never printed back.",
                    Style::default().fg(DIM),
                )));
                lines.push(Line::from(""));
                let masked = masked_key(&setup.api_key);
                lines.push(Line::from(vec![
                    Span::styled("❯ ", Style::default().fg(ACCENT)),
                    Span::styled(
                        if masked.is_empty() {
                            "paste or type the key".to_string()
                        } else {
                            masked
                        },
                        Style::default().fg(if setup.api_key.is_empty() {
                            FAINT
                        } else {
                            TEXT
                        }),
                    ),
                    Span::styled("▌", Style::default().fg(ACCENT)),
                ]));
                input_line = Some(lines.len().saturating_sub(1));
                lines.push(Line::from(""));
                lines.push(Line::from(Span::styled(
                    if setup.api_key.is_empty() {
                        if self.daemon_llm_key {
                            "Empty keeps using mskd's MSK_LLM_API_KEY."
                        } else {
                            "Empty is allowed only if mskd has MSK_LLM_API_KEY."
                        }
                    } else {
                        "Stored in tui-config.json on this machine."
                    },
                    Style::default().fg(FAINT),
                )));
            }
        }

        lines.push(Line::from(""));
        lines.push(Line::from(""));
        let key_state = if requires_key(provider) && !setup.api_key.trim().is_empty() {
            "this profile has an API key"
        } else if self.daemon_llm_key {
            "mskd has MSK_LLM_API_KEY"
        } else {
            "no API key yet (type one here or export MSK_LLM_API_KEY in mskd)"
        };
        lines.push(Line::from(Span::styled(
            key_state,
            Style::default().fg(if requires_key(provider) && !self.has_llm_key() {
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
            let cursor = setup.input_cursor;
            let masked_prefix;
            let cursor_text = match setup.step {
                SetupStep::CustomModel => &setup.model_input[..cursor.min(setup.model_input.len())],
                SetupStep::BaseUrl => &setup.base_url[..cursor.min(setup.base_url.len())],
                SetupStep::ApiKey => {
                    let offset = cursor.min(setup.api_key.len());
                    masked_prefix = masked_key(&setup.api_key[..offset]);
                    masked_prefix.as_str()
                }
                _ => "",
            };
            let width = UnicodeWidthStr::width(cursor_text) as u16;
            let y = inner.y + line_index as u16;
            let x = inner.x + 3 + width;
            if y < inner.bottom() {
                frame.set_cursor_position((x.min(inner.right().saturating_sub(2)), y));
            }
        }
    }
}

fn step_line(step: SetupStep, needs_key: bool) -> Line<'static> {
    let mut steps = vec![
        ("provider", SetupStep::Provider),
        ("service", SetupStep::Preset),
        ("model", SetupStep::Model),
        ("effort", SetupStep::Effort),
        ("endpoint", SetupStep::BaseUrl),
    ];
    if needs_key {
        steps.push(("key", SetupStep::ApiKey));
    }
    let active = match step {
        SetupStep::CustomModel => SetupStep::Model,
        SetupStep::ApiKey if !needs_key => SetupStep::BaseUrl,
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

fn is_text_step(step: SetupStep) -> bool {
    matches!(
        step,
        SetupStep::CustomModel | SetupStep::BaseUrl | SetupStep::ApiKey
    )
}

impl SetupState {
    fn text_len(&self) -> Option<usize> {
        match self.step {
            SetupStep::CustomModel => Some(self.model_input.len()),
            SetupStep::BaseUrl => Some(self.base_url.len()),
            SetupStep::ApiKey => Some(self.api_key.len()),
            _ => None,
        }
    }

    fn text_ref(&self) -> Option<&str> {
        match self.step {
            SetupStep::CustomModel => Some(&self.model_input),
            SetupStep::BaseUrl => Some(&self.base_url),
            SetupStep::ApiKey => Some(&self.api_key),
            _ => None,
        }
    }

    fn text_field(&mut self) -> Option<&mut String> {
        match self.step {
            SetupStep::CustomModel => Some(&mut self.model_input),
            SetupStep::BaseUrl => Some(&mut self.base_url),
            SetupStep::ApiKey => Some(&mut self.api_key),
            _ => None,
        }
    }
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
