//! Core layout and pane rendering.

use super::*;

impl App {
    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.height < 4 || area.width < 20 {
            let notice = Paragraph::new("terminal too small for eix")
                .alignment(Alignment::Center)
                .style(Style::default().fg(composer_danger(self.theme)));
            frame.render_widget(notice, area);
            return;
        }

        // Grow with the draft up to a small cap so long command lines are
        // readable; beyond that the editor scrolls instead of taking the pane.
        // The status bar and the `:` line own the bottom two rows when the
        // terminal has room for them; very short terminals only show the
        // command line while it is open.
        let show_cmdline = self.cmdline.is_some() || area.height >= 7;
        let status_height = u16::from(area.height >= 7);
        let cmdline_height = u16::from(show_cmdline);
        let inner_width = area.width.saturating_sub(4).max(1) as usize;
        let input_layout = self.input.wrap(inner_width);
        let input_height = prompt_input_height_for_rows(area.height, input_layout.segments.len());
        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(input_height),
            Constraint::Length(status_height),
            Constraint::Length(cmdline_height),
        ])
        .split(area);

        self.draw_body(frame, chunks[0]);
        self.draw_input(frame, chunks[1], input_layout);
        self.draw_status_bar(frame, chunks[2]);
        self.draw_cmdline(frame, chunks[3]);
        if self.cmdline_menu_open() {
            self.draw_cmdline_menu(frame, chunks[3]);
        }
        if self.command_menu_open() {
            self.draw_command_menu(frame, chunks[1]);
        }
        if self.btw.is_some() {
            self.draw_btw(frame, chunks[0]);
        }

        if self.picker.is_some() {
            self.draw_picker(frame, area);
        }
        if self.help {
            draw_help(
                frame,
                area,
                &self.help_query,
                self.help_selected,
                self.help_scope,
                self.theme,
            );
        }
        if self.diff_view.is_some() {
            self.draw_diff_view(frame, area);
        }
        if self.process_view.is_some() {
            self.draw_processes(frame, area);
        }
        if self.inbox.is_some() {
            self.draw_inbox(frame, area);
        }
        if self.error.is_some() {
            self.draw_error_dialog(frame, area);
        }

        let now = Instant::now();
        self.expire_toasts(now);
        self.sync_status_toast();
        self.draw_toasts(frame, area);
    }

    /// Compact status icon shared by the pane title and the prompt header.
    /// Running sessions animate; idle sessions use a small static dot.
    pub(super) fn status_icon(&self, status: SessionStatus) -> String {
        match status {
            SessionStatus::Idle => "⠶".to_string(),
            SessionStatus::Running => {
                RUNNING_SPINNER[(self.tick as usize) % RUNNING_SPINNER.len()].to_string()
            }
            _ => status.symbol().to_string(),
        }
    }

    /// Glyph for the prompt header. Idle and running are icon-only so the
    /// header stays compact; other states keep their symbol and label.
    pub(super) fn status_glyph(&self, status: SessionStatus) -> String {
        match status {
            SessionStatus::Idle | SessionStatus::Running => self.status_icon(status),
            _ => format!("{} {}", status.symbol(), status.as_str()),
        }
    }

    pub(super) fn draw_body(&mut self, frame: &mut Frame, area: Rect) {
        // Sessions live in the floating picker (`Space g` / `/sessions`), not in
        // a permanent sidebar: panes get the whole body.
        self.tab_areas.clear();
        if self.views.is_empty()
            || self.effective_layout(area) != PaneLayout::Tabby
            || area.height <= 1
        {
            self.draw_panes(frame, area);
            return;
        }
        // Keep the tab strip against the prompt box instead of the top of the
        // transcript. Fibonacci shows every pane, so it needs no tab strip at
        // all.
        let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
        self.draw_panes(frame, chunks[0]);
        self.draw_tab_bar(frame, chunks[1]);
    }

    pub(super) fn draw_tab_bar(&mut self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let theme = self.theme;
        let focused = self.focused.min(self.views.len().saturating_sub(1));
        let mut spans = Vec::with_capacity(self.views.len() * 2);
        // Track each label's cells so a click can focus that tab. The
        // Paragraph clips labels that run past the strip, so clip the hit
        // rectangles too: hidden cells must not be clickable.
        let right = area.x.saturating_add(area.width);
        let mut column = area.x;
        for (index, view) in self.views.iter().enumerate() {
            let active = index == focused;
            let attention = !active
                && (view.session.status.needs_attention()
                    || self.approvals.contains_key(&view.session.id)
                    || self.inputs.contains_key(&view.session.id));
            let marker = if attention { "!" } else { "" };
            // Reserve enough room for the index and marker; the Paragraph
            // clips the rest when more tabs are open than fit.
            let name = truncate(&view.session.name, 24);
            let label = if active {
                format!("[{} {name}{marker}]", index + 1)
            } else {
                format!(" {} {name}{marker} ", index + 1)
            };
            let width = UnicodeWidthStr::width(label.as_str()) as u16;
            if column < right {
                let visible = width.min(right - column);
                self.tab_areas
                    .push((Rect::new(column, area.y, visible, 1), index));
            }
            column = column.saturating_add(width);
            let style = if active {
                Style::default()
                    .fg(composer_accent(theme))
                    .add_modifier(Modifier::BOLD)
            } else if attention {
                Style::default().fg(composer_warn(theme))
            } else {
                Style::default().fg(composer_muted(theme))
            };
            spans.push(Span::styled(label, style));
            if index + 1 < self.views.len() {
                spans.push(Span::styled(
                    "│",
                    Style::default().fg(composer_border(theme)),
                ));
                column = column.saturating_add(1);
            }
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans))
                .style(Style::default().bg(composer_background(theme))),
            area,
        );
    }

    /// Resolve the layout for this frame. Horizontal layout always stays
    /// horizontal: panes get compact title/content rows instead of silently
    /// collapsing into tabby when the terminal is short.
    pub(super) fn effective_layout(&self, _area: Rect) -> PaneLayout {
        self.layout
    }

    /// Horizontal layout shows the first pane plus the next four panes after
    /// it. If focus moved into a hidden pane, slide the tail window so the
    /// focused pane stays visible.
    pub(super) fn horizontal_visible_indices(&self) -> Vec<usize> {
        let len = self.views.len();
        if len <= HORIZONTAL_TAIL_PANES + 1 {
            return (0..len).collect();
        }
        let start = if self.focused > HORIZONTAL_TAIL_PANES {
            self.focused + 1 - HORIZONTAL_TAIL_PANES
        } else {
            1
        };
        let mut indices = Vec::with_capacity(HORIZONTAL_TAIL_PANES + 1);
        indices.push(0);
        indices.extend(start..(start + HORIZONTAL_TAIL_PANES).min(len));
        indices
    }

    pub(super) fn draw_panes(&mut self, frame: &mut Frame, area: Rect) {
        if self.views.is_empty() {
            self.pane_areas.clear();
            self.draw_empty_pane(frame, area);
            return;
        }
        self.focused = self.focused.min(self.views.len() - 1);
        let layout = self.effective_layout(area);
        if layout == PaneLayout::Tabby {
            // Only the active tab is visible; leave zero-sized hit boxes for
            // the hidden panes so clicks cannot reach them.
            let mut areas = vec![Rect::new(0, 0, 0, 0); self.views.len()];
            areas[self.focused] = area;
            self.pane_areas = areas;
            self.draw_pane(frame, area, self.focused);
            return;
        }
        let indices: Vec<usize> = if layout == PaneLayout::Horizontal {
            self.horizontal_visible_indices()
        } else {
            (0..self.views.len()).collect()
        };
        let rects = layout_pane_rects(layout, area, indices.len());
        let mut areas = vec![Rect::new(0, 0, 0, 0); self.views.len()];
        for (rect, index) in rects.iter().zip(indices.iter()) {
            areas[*index] = *rect;
        }
        self.pane_areas = areas;
        for (rect, index) in rects.into_iter().zip(indices) {
            self.draw_pane(frame, rect, index);
        }
    }

    pub(super) fn draw_pane(&mut self, frame: &mut Frame, area: Rect, index: usize) {
        // Horizontal layout may squeeze a pane down to a title row. Draw it
        // rather than leaving a blank gap; only an empty slot is skipped.
        if area.width < 4 || area.height == 0 {
            return;
        }
        let focused = index == self.focused;
        let session_id = self.views[index].session.id.clone();
        let attention = !focused
            && (self.views[index].session.status.needs_attention()
                || self.approvals.contains_key(&session_id)
                || self.inputs.contains_key(&session_id));
        let theme = self.theme;
        let borders = if self.effective_layout(area) == PaneLayout::Tabby {
            let mut borders = Borders::ALL;
            borders.remove(Borders::BOTTOM);
            borders
        } else {
            pane_borders(&self.pane_areas, index)
        };
        let border_cols = u16::from(borders.contains(Borders::LEFT))
            + u16::from(borders.contains(Borders::RIGHT));
        let border_rows = u16::from(borders.contains(Borders::TOP))
            + u16::from(borders.contains(Borders::BOTTOM));
        let inner_width = area.width.saturating_sub(border_cols) as usize;
        let inner_height = area.height.saturating_sub(border_rows) as usize;
        // Stored history is rendered once and cached; only the live tail is
        // rebuilt on each frame. This keeps streaming cheap for long sessions.
        self.views[index].ensure_history(inner_width.max(1), theme);
        let queued: Vec<String> = self
            .queues
            .get(&session_id)
            .map(|queue| queue.iter().cloned().collect())
            .unwrap_or_default();
        self.views[index].ensure_tail(inner_width.max(1), theme, self.tick, &queued);
        let history_len = self.views[index]
            .history_cache
            .as_ref()
            .map(|cache| cache.lines.len())
            .unwrap_or(0);
        let tail_len = self.views[index].tail.lines.len();
        {
            let view = &mut self.views[index];
            if let Some((old_len, old_scroll)) = view.history_anchor.take() {
                if !view.follow {
                    // Prepending a page shifts every existing line down. Move
                    // the scroll and any selection by the same amount so the
                    // viewport content does not jump under the user.
                    let delta = history_len.saturating_sub(old_len);
                    view.scroll = old_scroll.saturating_add(delta);
                    if let Some(selection) = &mut view.selection {
                        selection.anchor.0 = selection.anchor.0.saturating_add(delta);
                        selection.cursor.0 = selection.cursor.0.saturating_add(delta);
                    }
                }
            }
            view.line_count = history_len + tail_len;
            view.slots.truncate(history_len);
            view.slots.extend_from_slice(&view.tail.slots);
        }
        let total = history_len + tail_len;
        let max_scroll = total.saturating_sub(inner_height);
        {
            let view = &mut self.views[index];
            if view.follow {
                view.scroll = max_scroll;
            } else {
                view.scroll = view.scroll.min(max_scroll);
                if view.scroll > HISTORY_REARM_LINES {
                    view.history_armed = true;
                }
            }
        }
        // Ask the event loop for the previous page when the viewport reaches
        // the top. Only the focused pane loads history; background panes stay
        // idle.
        let request = {
            let view = &self.views[index];
            if focused
                && !view.follow
                && view.history_armed
                && !view.history_loading
                && view.history_has_more
                && view.scroll <= HISTORY_PRELOAD_LINES
            {
                view.events
                    .first()
                    .map(|event| (session_id.clone(), event.seq))
            } else {
                None
            }
        };
        if let Some((id, before_seq)) = request {
            let view = &mut self.views[index];
            view.history_loading = true;
            view.history_armed = false;
            self.pending_history = Some((id, before_seq));
        }
        // Only hand the visible window to ratatui. Besides reducing layout
        // work, this keeps the paragraph independent of the full line vector.
        let scroll = self.views[index].scroll;
        let end = (scroll + inner_height).min(total);
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(end.saturating_sub(scroll));
        {
            let view = &self.views[index];
            if let Some(cache) = view.history_cache.as_ref() {
                if scroll < history_len {
                    let history_end = end.min(history_len);
                    lines.extend_from_slice(&cache.lines[scroll..history_end]);
                }
            }
        }
        if end > history_len {
            let tail_start = scroll.saturating_sub(history_len);
            lines.extend_from_slice(&self.views[index].tail.lines[tail_start..end - history_len]);
        }
        if let Some(selection) = self.views[index].selection {
            highlight_selection_window(&mut lines, &selection, scroll);
        }
        if let Some(search) = self.transcript_search.as_ref() {
            if search.session_id == session_id {
                for (offset, line) in lines.iter_mut().enumerate() {
                    let absolute = scroll + offset;
                    if !search.matches.contains(&absolute) {
                        continue;
                    }
                    let style = if search.matches.get(search.current) == Some(&absolute) {
                        let background = composer_warn(theme);
                        Style::default()
                            .fg(badge_text_color(background))
                            .bg(background)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().bg(composer_selection(theme))
                    };
                    for span in &mut line.spans {
                        span.style = span.style.patch(style);
                    }
                }
            }
        }
        let view = &self.views[index];
        let name = truncate(
            &view.session.name,
            inner_width.saturating_sub(6).clamp(1, 18),
        );
        let marker = if attention { "!" } else { "" };
        let border_color = if focused {
            // The active pane needs to read as the input target at a glance;
            // the muted border used for inactive panes is too subtle once
            // several sessions are tiled together.
            composer_focus(theme)
        } else if attention {
            composer_warn(theme)
        } else {
            composer_inactive_border(theme)
        };
        let mut border_style = Style::default().fg(border_color);
        let mut title_style = Style::default().fg(border_color);
        if focused {
            title_style = title_style.add_modifier(Modifier::BOLD);
        } else {
            title_style = title_style.add_modifier(Modifier::DIM);
        }
        let mut block = Block::default().borders(borders);
        if focused {
            block = block.border_set(PANE_FOCUS_BORDER);
        }
        if let Some(background) = theme.background {
            border_style = border_style.bg(background);
            title_style = title_style.bg(background);
            block = block.style(Style::default().bg(background));
        }
        // Keep the session name in the border color, but make the running
        // quadrant green and the idle glyph gray so the state is obvious.
        let status = view.session.status;
        let icon_style = title_style.fg(composer_status_color(status, theme));
        let title = Line::from(vec![
            Span::styled(format!(" {} ", index + 1), title_style),
            Span::styled(self.status_icon(status), icon_style),
            Span::styled(format!("{marker} {name} "), title_style),
        ]);
        block = block
            .title(title)
            .border_style(border_style)
            .title_style(title_style);
        let mut paragraph = Paragraph::new(lines).block(block);
        if let Some(background) = theme.background {
            paragraph = paragraph.style(Style::default().bg(background));
        }
        frame.render_widget(paragraph, area);

        if focused {
            self.draw_approval_card(frame, area, &session_id);
            if let Some(pending) = self.inputs.get(&session_id) {
                draw_input_card(frame, area, pending, theme);
            }
        }
    }

    pub(super) fn draw_empty_pane(&self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();
        if !self.initialized {
            lines.push(Line::from("starting…"));
        } else if self.sessions.is_empty() && self.error.is_none() {
            lines.push(Line::from(Span::styled(
                "no sessions yet",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from("press n to create one"));
        } else if self.pending_attach.is_some() {
            lines.push(Line::from("loading session…"));
        } else if self.error.is_some() {
            lines.push(Line::from(Span::styled(
                "could not load the session",
                Style::default().fg(composer_danger(self.theme)),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(
                "press p to pick another one, or n for a new session",
            ));
        } else {
            lines.push(Line::from("no pane open"));
            lines.push(Line::from(""));
            lines.push(Line::from(
                "press p to pick a session, o to open a new pane",
            ));
        }
        let background = self.theme.background;
        let mut borders = Borders::ALL;
        borders.remove(Borders::BOTTOM);
        let block = Block::default().borders(borders).title(" transcript ");
        let block = if let Some(background) = background {
            block.style(Style::default().bg(background))
        } else {
            block
        };
        let mut paragraph = Paragraph::new(lines).block(block);
        if let Some(background) = background {
            paragraph = paragraph.style(Style::default().bg(background));
        }
        frame.render_widget(paragraph, area);
    }

    pub(super) fn status_session(&self) -> Option<&Session> {
        self.focused_view()
            .map(|view| &view.session)
            .or_else(|| self.sessions.get(self.selected))
    }

    /// Floating error report. It sits above every other overlay and is
    /// dismissed with Esc / Enter / q.
    pub(super) fn draw_error_dialog(&self, frame: &mut Frame, area: Rect) {
        let Some(error) = self.error.as_deref() else {
            return;
        };
        if area.width < 20 || area.height < 5 {
            return;
        }
        let width = ((area.width as u32 * 3 / 5).max(36).min(area.width as u32)) as u16;
        let text_width = width.saturating_sub(4) as usize;
        let lines: Vec<Line> = wrap_text(error, text_width)
            .into_iter()
            .map(|line| Line::from(format!("  {line}")))
            .collect();
        let height = (lines.len() as u16 + 2).clamp(3, area.height.saturating_sub(2));
        let rect = centered(area, width, height);
        let background = dialog_background(self.theme);
        let danger = composer_danger(self.theme);
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Block::default().style(Style::default().bg(background)),
            rect,
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(danger).bg(background))
            .style(Style::default().bg(background))
            .title(Span::styled(
                " error ",
                Style::default()
                    .fg(danger)
                    .bg(background)
                    .add_modifier(Modifier::BOLD),
            ))
            .title_bottom(Span::styled(
                " Esc / Enter close ",
                Style::default()
                    .fg(composer_muted(self.theme))
                    .bg(background),
            ));
        frame.render_widget(
            Paragraph::new(lines).block(block).style(
                Style::default()
                    .fg(composer_text(self.theme))
                    .bg(background),
            ),
            rect,
        );
    }

    /// One prompt row, with the visual selection reversed.
    pub(super) fn input_line(&self, segment: &crate::tui::editor::Segment) -> Line<'static> {
        if self.input.selection().is_none() {
            return Line::from(segment.text.clone());
        }
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut run = String::new();
        let mut run_selected = false;
        for (offset, ch) in segment.text.chars().enumerate() {
            let selected = self
                .input
                .is_selected(segment.row, segment.start_col + offset);
            if selected != run_selected && !run.is_empty() {
                spans.push(selection_span(
                    std::mem::take(&mut run),
                    run_selected,
                    self.theme,
                ));
            }
            run_selected = selected;
            run.push(ch);
        }
        if !run.is_empty() {
            spans.push(selection_span(run, run_selected, self.theme));
        }
        Line::from(spans)
    }
}
