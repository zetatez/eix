//! Toast queue and status bar rendering.

use super::*;

impl App {
    /// Turn a changed app status into a notification only when it reports a
    /// failure that is not already visible in a dialog or overlay. Most call
    /// sites assign `status` directly, so observing it at draw time keeps the
    /// rule in one place.
    pub(super) fn sync_status_toast(&mut self) {
        if self.status == self.last_seen_status {
            return;
        }
        self.last_seen_status.clone_from(&self.status);
        if !status_needs_warning(&self.status) || self.status_warning_is_redundant() {
            return;
        }
        self.push_toast(
            self.status.clone(),
            ToastTone::Warning,
            WARNING_TOAST_DISPLAY,
        );
    }

    /// Some failures are already explained by the surface that caused them.
    /// Do not repeat them as a toast while that surface is still open.
    pub(super) fn status_warning_is_redundant(&self) -> bool {
        self.error.is_some()
            || self.picker.is_some()
            || self.help
            || self.diff_view.is_some()
            || self.process_view.is_some()
            || self.btw.is_some()
            || self.reason_prompt.is_some()
    }

    pub(super) fn push_toast(&mut self, text: String, tone: ToastTone, duration: Duration) {
        let text = text
            .trim()
            .replace(['\n', '\r', '\t'], " ")
            .trim()
            .to_string();
        if text.is_empty() {
            return;
        }
        let now = Instant::now();
        // Repeating a message refreshes it and moves it back to the top instead
        // of stacking identical rows.
        self.toasts.retain(|toast| toast.text != text);
        self.toasts.push(Toast {
            text,
            tone,
            expires_at: now + duration.min(MAX_TOAST_DISPLAY),
        });
        if self.toasts.len() > MAX_TOASTS {
            self.toasts.remove(0);
        }
    }

    /// Drop records independently; there is no minimum display time and no
    /// waiting queue, so new messages are visible immediately.
    pub(super) fn expire_toasts(&mut self, now: Instant) {
        self.toasts.retain(|toast| now < toast.expires_at);
    }

    /// Notifications stack in the top-right corner, newest first, floating
    /// above panes and dialogs. Each row can have a different width.
    pub(super) fn draw_toasts(&self, frame: &mut Frame, area: Rect) {
        if area.width < 2 || area.height < 2 {
            return;
        }
        let available = area.width.saturating_sub(2);
        let rows = area.height.saturating_sub(1) as usize;
        for (row, toast) in self.toasts.iter().rev().take(rows).enumerate() {
            let text = truncate(&format!(" {} ", toast.text), available as usize);
            let width = text.width() as u16;
            if width == 0 {
                continue;
            }
            let rect = Rect {
                x: area.x + area.width - width - 1,
                y: area.y + 1 + row as u16,
                width,
                height: 1,
            };
            let color = match toast.tone {
                ToastTone::Success => composer_success(self.theme),
                ToastTone::Warning => composer_warn(self.theme),
            };
            frame.render_widget(Clear, rect);
            frame.render_widget(
                Paragraph::new(text).alignment(Alignment::Right).style(
                    Style::default()
                        .fg(badge_text_color(color))
                        .bg(color)
                        .add_modifier(Modifier::BOLD),
                ),
                rect,
            );
        }
    }

    pub(super) fn draw_input(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        mut layout: crate::tui::editor::Layout,
    ) {
        let focused = self.focus == Focus::Input;
        let panel = composer_background(self.theme);
        let text = composer_text(self.theme);
        let border_color = if focused {
            composer_focus(self.theme)
        } else {
            composer_inactive_border(self.theme)
        };

        // Deny reasons render in their own dialog that takes over this slot;
        // the caller skips this prompt entirely while that editor is open, so
        // the two can never overlap.
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color).bg(panel))
            .style(Style::default().bg(panel))
            .padding(Padding::new(1, 1, 0, 0));

        // One cell of left padding keeps the text close to the border; one
        // cell on the right mirrors it.
        let inner_width = area.width.saturating_sub(4).max(1) as usize;
        let inner_height = area.height.saturating_sub(2).max(1) as usize;
        self.input.fit(&mut layout, inner_height);
        let lines: Vec<Line> = layout
            .segments
            .iter()
            .skip(layout.offset)
            .take(inner_height)
            .map(|segment| self.input_line(segment))
            .collect();
        let paragraph = Paragraph::new(lines)
            .block(block)
            .style(Style::default().fg(text).bg(panel));
        frame.render_widget(paragraph, area);

        // The error dialog covers the prompt; don't leave a blinking input
        // cursor on top of it. The reason dialog owns the cursor while open.
        if focused && self.error.is_none() && self.cmdline.is_none() && self.reason_prompt.is_none()
        {
            let x = area.x + 2 + layout.cursor_col.min(inner_width.saturating_sub(1)) as u16;
            let row = layout
                .cursor_line
                .saturating_sub(layout.offset)
                .min(inner_height.saturating_sub(1));
            let y = area.y + 1 + row as u16;
            frame.set_cursor_position((x, y));
        }
    }

    /// Primary statusline: Vim mode, agent mode, execution status,
    /// model/provider and reasoning variant. The workspace (cwd + branch) is
    /// right-aligned.
    pub(super) fn status_bar_left(&self) -> Line<'static> {
        let theme = self.theme;
        let panel = composer_background(theme);
        let muted = composer_muted(theme);
        let warn = composer_warn(theme);
        let active = self.focus == Focus::Input;
        let input_mode = if active {
            self.input.mode()
        } else {
            Mode::Normal
        };
        let cmdline = self.cmdline.is_some();
        let mode_label = if cmdline {
            "C"
        } else {
            input_mode.short_label()
        };
        let mode_style = if cmdline {
            composer_mode_style(Mode::Normal, true, theme)
        } else {
            composer_mode_style(input_mode, active, theme)
        };
        let session = self.status_session();
        let session_mode = session.map(|session| session.mode).unwrap_or_default();
        let session_mode_label = session_mode.label();
        let mode_color = session_mode_color(session_mode, theme);
        let mut spans = vec![
            Span::styled(format!(" {mode_label} "), mode_style),
            Span::styled("· ", Style::default().fg(muted).bg(panel)),
            Span::styled(
                format!("{session_mode_label:<width$}", width = SESSION_MODE_WIDTH),
                Style::default()
                    .fg(mode_color)
                    .bg(panel)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", Style::default().fg(muted).bg(panel)),
        ];
        if let Some(session) = session {
            let (status, status_color) = if self.connected {
                (
                    self.status_glyph(session.status),
                    composer_status_color(session.status, theme),
                )
            } else {
                ("offline".to_string(), composer_danger(theme))
            };
            spans.push(Span::styled(
                status,
                Style::default().fg(status_color).bg(panel),
            ));

            if !self.plugin_failed.is_empty() || !self.plugin_degraded.is_empty() {
                let (text, color) = if !self.plugin_failed.is_empty() {
                    (
                        format!("{} plugin failed", self.plugin_failed.len()),
                        composer_danger(theme),
                    )
                } else {
                    (
                        format!("{} plugin degraded", self.plugin_degraded.len()),
                        warn,
                    )
                };
                spans.push(Span::styled(" · ", Style::default().fg(muted).bg(panel)));
                spans.push(Span::styled(
                    format!("⚠ {text}"),
                    Style::default()
                        .fg(color)
                        .bg(panel)
                        .add_modifier(Modifier::BOLD),
                ));
            }

            let (model, provider, variant) = self.model_header_parts();
            let model = short_model(&model);
            if model != "-" && !model.is_empty() {
                spans.push(Span::styled(" · ", Style::default().fg(muted).bg(panel)));
                spans.push(Span::styled(
                    model.to_string(),
                    Style::default().fg(muted).bg(panel),
                ));
                if !provider.is_empty() {
                    spans.push(Span::styled(" · ", Style::default().fg(muted).bg(panel)));
                    spans.push(Span::styled(
                        provider,
                        Style::default()
                            .fg(muted)
                            .bg(panel)
                            .add_modifier(Modifier::DIM),
                    ));
                }
                if let Some(variant) = variant {
                    spans.push(Span::styled(" · ", Style::default().fg(muted).bg(panel)));
                    spans.push(Span::styled(variant, Style::default().fg(warn).bg(panel)));
                }
            }
        } else if self.connected {
            spans.push(Span::styled(
                "no session",
                Style::default().fg(muted).bg(panel),
            ));
        } else {
            spans.push(Span::styled(
                "connecting…",
                Style::default().fg(muted).bg(panel),
            ));
        }
        Line::from(spans)
    }

    /// Secondary statusline context: the pending leader hint plus persistent
    /// integration and image state. Transient messages live in top-right
    /// toasts instead, so this area stays stable while commands run. It is
    /// dropped first when the bar gets narrow.
    pub(super) fn status_bar_middle(&self) -> Option<Line<'static>> {
        let theme = self.theme;
        let panel = composer_background(theme);
        let muted = composer_muted(theme);
        let warn = composer_warn(theme);
        let mut spans = Vec::new();
        // The leader hint is a key guide rather than command feedback, so it
        // stays in the statusline until the next key restores the old status.
        if self.pending_leader && self.status.starts_with("leader:") {
            spans.push(Span::styled(
                self.status.clone(),
                Style::default().fg(muted).bg(panel),
            ));
        }
        if let Some(session) = self.status_session() {
            if let Some(state) = self.integrations.get(&session.id) {
                if !matches!(
                    state.status,
                    IntegrationStatus::Pending | IntegrationStatus::Merged
                ) {
                    if !spans.is_empty() {
                        spans.push(Span::styled(" · ", Style::default().fg(muted).bg(panel)));
                    }
                    spans.push(Span::styled(
                        format!("{} {}", state.status.symbol(), state.status.as_str()),
                        Style::default()
                            .fg(
                                if matches!(
                                    state.status,
                                    IntegrationStatus::Retrying | IntegrationStatus::Repairing
                                ) {
                                    warn
                                } else {
                                    muted
                                },
                            )
                            .bg(panel),
                    ));
                }
            }
        }
        // Graphics support is a startup probe; the status bar stays quiet and
        // `/math status` reports the protocol or why images were not used.
        if !self.pending_images.is_empty() {
            if !spans.is_empty() {
                spans.push(Span::styled(" · ", Style::default().fg(muted).bg(panel)));
            }
            spans.push(Span::styled(
                format!("{} img", self.pending_images.len()),
                Style::default().fg(warn).bg(panel),
            ));
        }
        // Dictation is live input, so it shows as a level meter rather than a
        // one-shot message: silence while the key is on is worth noticing.
        if let Some(level) = self.voice {
            if !spans.is_empty() {
                spans.push(Span::styled(" · ", Style::default().fg(muted).bg(panel)));
            }
            let bars = ((level * 8.0).round() as usize).clamp(1, 8);
            spans.push(Span::styled(
                format!("mic {}", "▮".repeat(bars)),
                Style::default().fg(warn).bg(panel),
            ));
        }
        if spans.is_empty() {
            None
        } else {
            spans.insert(0, Span::styled(" · ", Style::default().fg(muted).bg(panel)));
            Some(Line::from(spans))
        }
    }

    /// Workspace context kept against the right edge: cwd and git branch.
    pub(super) fn status_bar_workspace(&self) -> Option<Line<'static>> {
        let panel = composer_background(self.theme);
        let muted = composer_muted(self.theme);
        let accent = composer_accent(self.theme);
        let session = self.status_session()?;
        let mut spans = vec![Span::styled(
            compact_path(&session.cwd.display().to_string()),
            Style::default().fg(muted).bg(panel),
        )];
        if self.git_cwd.as_ref() == Some(&session.cwd) {
            if let Some(branch) = &self.git_branch {
                spans.push(Span::styled(" · ", Style::default().fg(muted).bg(panel)));
                spans.push(Span::styled(
                    branch.clone(),
                    Style::default().fg(accent).bg(panel),
                ));
            }
        }
        Some(Line::from(spans))
    }

    pub(super) fn status_bar_right(&self) -> Line<'static> {
        let theme = self.theme;
        let panel = composer_background(theme);
        let muted = composer_muted(theme);
        let warn = composer_warn(theme);
        let mut spans = Vec::new();
        let push = |spans: &mut Vec<Span<'static>>, text: String, color: Color| {
            if !spans.is_empty() {
                spans.push(Span::styled(" · ", Style::default().fg(muted).bg(panel)));
            }
            spans.push(Span::styled(text, Style::default().fg(color).bg(panel)));
        };
        if self.views.len() > 1 {
            push(
                &mut spans,
                format!("{}/{}", self.focused + 1, self.views.len()),
                muted,
            );
        }
        if let Some(view) = self.focused_view() {
            let queued = self.queue_len(&view.session.id);
            if queued > 0 {
                push(&mut spans, format!("⏳{queued}"), warn);
            }
        }
        let attention = self.attention_count();
        if attention > 0 {
            push(&mut spans, format!("⚠{attention}"), warn);
        }
        if !spans.is_empty() {
            spans.insert(0, Span::styled(" ", Style::default().bg(panel)));
        }
        Line::from(spans)
    }

    pub(super) fn draw_status_bar(&self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let panel = composer_background(self.theme);
        let background = Style::default().bg(panel);
        // Paint the whole row first: the middle section is often empty now
        // that transient messages live in toasts, and unpainted cells would
        // keep the terminal's default background instead of the theme's.
        frame.render_widget(Paragraph::new(Line::default()).style(background), area);
        let left = self.status_bar_left();
        let middle = self.status_bar_middle();
        let counters = self.status_bar_right();
        let workspace = self.status_bar_workspace();

        let counters_width = counters.width();
        let workspace_width = workspace.as_ref().map_or(0, |line| line.width());
        let workspace_with_padding = if workspace_width > 0 {
            workspace_width + 1
        } else {
            0
        };
        let combined_right_width = if counters_width > 0 && workspace_width > 0 {
            counters_width + workspace_width + 3
        } else if workspace_width > 0 {
            workspace_with_padding
        } else {
            counters_width
        };

        // Keep cwd/branch visible when the left side grows; the counters are
        // dropped before the workspace on narrow terminals.
        let left_width = left
            .width()
            .min((area.width as usize).saturating_sub(workspace_with_padding))
            as u16;
        let right_available = area.width as usize - left_width as usize;
        let combined_fits = combined_right_width <= right_available;
        let right_width = if combined_fits {
            combined_right_width
        } else {
            workspace_with_padding.min(right_available)
        } as u16;
        let middle_width = area
            .width
            .saturating_sub(left_width)
            .saturating_sub(right_width);

        let mut right_spans = Vec::new();
        if combined_fits {
            right_spans.extend(counters.spans);
            if let Some(workspace) = workspace {
                if right_spans.is_empty() {
                    right_spans.push(Span::styled(" ", background));
                } else {
                    right_spans.push(Span::styled(
                        " · ",
                        Style::default().fg(composer_muted(self.theme)).bg(panel),
                    ));
                }
                right_spans.extend(workspace.spans);
            }
        } else if let Some(workspace) = workspace {
            right_spans.push(Span::styled(" ", background));
            right_spans.extend(workspace.spans);
        }
        let right = truncate_line(Line::from(right_spans), right_width as usize);

        let chunks = Layout::horizontal([
            Constraint::Length(left_width),
            Constraint::Min(0),
            Constraint::Length(right_width),
        ])
        .split(area);
        if left_width > 0 {
            let left = truncate_line(left, left_width as usize);
            frame.render_widget(Paragraph::new(left).style(background), chunks[0]);
        }
        if middle_width > 0 {
            if let Some(middle) = middle {
                let middle = truncate_line(middle, middle_width as usize);
                frame.render_widget(Paragraph::new(middle).style(background), chunks[1]);
            }
        }
        if right_width > 0 {
            frame.render_widget(
                Paragraph::new(right)
                    .alignment(Alignment::Right)
                    .style(background),
                chunks[2],
            );
        }
    }
}
