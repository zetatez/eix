//! Rendering of cmdline, menus and overlay panels.

use super::*;

impl App {
    /// Vim-style command line below the status bar. It is always visible when
    /// the terminal has room, but stays blank until `:` opens it.
    pub(super) fn draw_cmdline(&self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let panel = composer_background(self.theme);
        let background = Style::default().bg(panel);
        let Some(cmdline) = self.cmdline.as_ref() else {
            frame.render_widget(Paragraph::new(Line::default()).style(background), area);
            return;
        };
        let line = Line::from(vec![
            Span::styled(
                cmdline.prompt.to_string(),
                Style::default()
                    .fg(composer_accent(self.theme))
                    .bg(panel)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                cmdline.text.clone(),
                Style::default().fg(composer_text(self.theme)).bg(panel),
            ),
        ]);
        frame.render_widget(Paragraph::new(line).style(background), area);
        let cursor = UnicodeWidthStr::width(&cmdline.text[..cmdline.cursor_byte()]) as u16;
        let max_x = area.x + area.width.saturating_sub(1);
        let x = (area.x + 1 + cursor).min(max_x);
        frame.set_cursor_position((x, area.y));
    }

    /// `/` suggestion menu floated just above the prompt.
    pub(super) fn draw_command_menu(&mut self, frame: &mut Frame, input_area: Rect) {
        if input_area.y < 3 {
            return;
        }
        let matches = self.command_matches();
        if matches.is_empty() {
            return;
        }
        let height = (matches.len() as u16 + 2)
            .min(15)
            .min(input_area.y.saturating_sub(1));
        let visible = height.saturating_sub(2) as usize;
        if visible == 0 {
            return;
        }
        let cursor = self.command_cursor.min(matches.len() - 1);
        let start = cursor.saturating_sub(visible.saturating_sub(1));
        let width = input_area.width.min(88);
        let rect = Rect::new(input_area.x, input_area.y - height, width, height);
        frame.render_widget(Clear, rect);

        let panel = composer_background(self.theme);
        let muted = composer_muted(self.theme);
        let text = composer_text(self.theme);
        let accent = composer_accent(self.theme);
        let inner = width.saturating_sub(2) as usize;
        let marker_width = 3usize;
        let name_width = COMMAND_NAME_WIDTH.min(inner.saturating_sub(marker_width + 2));
        let desc_width = inner
            .saturating_sub(marker_width + name_width)
            .saturating_sub(2);
        let mut lines = Vec::new();
        for (offset, spec) in matches.iter().skip(start).take(visible).enumerate() {
            let index = start + offset;
            let selected = index == cursor;
            let row_bg = if selected { COMPOSER_SELECTION } else { panel };
            let marker_style = if selected {
                Style::default()
                    .fg(accent)
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(muted).bg(row_bg)
            };
            let name_style = if selected {
                Style::default()
                    .fg(accent)
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(text).bg(row_bg)
            };
            let desc_style = if selected {
                Style::default().fg(text).bg(row_bg)
            } else {
                Style::default().fg(muted).bg(row_bg)
            };
            let name = command_label(spec, "/");
            let name = pad_to(&truncate(&name, name_width), name_width);
            let description = pad_to(&truncate(&spec.description, desc_width), desc_width);
            let gap = inner.saturating_sub(marker_width + name_width + desc_width);
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", if selected { "❯" } else { " " }),
                    marker_style,
                ),
                Span::styled(name, name_style),
                Span::styled(" ".repeat(gap), desc_style),
                Span::styled(description, desc_style),
            ]));
        }
        let title_left = Line::from(Span::styled(
            " / commands ",
            Style::default()
                .fg(accent)
                .bg(panel)
                .add_modifier(Modifier::BOLD),
        ));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(accent).bg(panel))
            .style(Style::default().bg(panel))
            .title_style(Style::default().bg(panel))
            .title_top(title_left);
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .style(Style::default().fg(text).bg(panel)),
            rect,
        );
    }

    /// Dynamic `:` command list floated just above the command line. Typing
    /// filters it; Tab completes a match, while Ctrl-J/K and Ctrl-N/P move the
    /// selection without rewriting the typed prefix.
    pub(super) fn draw_cmdline_menu(&mut self, frame: &mut Frame, cmdline_area: Rect) {
        // Keep the statusline visible: the menu grows upward from the row
        // immediately above the command line.
        let status_y = cmdline_area.y.saturating_sub(1);
        if status_y < 3 {
            return;
        }
        let matches = self.cmdline_matches();
        if matches.is_empty() {
            return;
        }
        let height = (matches.len() as u16 + 2).min(15).min(status_y);
        let visible = height.saturating_sub(2) as usize;
        if visible == 0 {
            return;
        }
        let cursor = self.cmdline_menu_cursor.unwrap_or(0).min(matches.len() - 1);
        let start = cursor.saturating_sub(visible.saturating_sub(1));
        let width = cmdline_area.width.min(88);
        let rect = Rect::new(cmdline_area.x, status_y - height, width, height);
        frame.render_widget(Clear, rect);

        let panel = composer_background(self.theme);
        let muted = composer_muted(self.theme);
        let text = composer_text(self.theme);
        let accent = composer_accent(self.theme);
        let inner = width.saturating_sub(2) as usize;
        let marker_width = 3usize;
        let name_width = COMMAND_NAME_WIDTH.min(inner.saturating_sub(marker_width + 2));
        let desc_width = inner
            .saturating_sub(marker_width + name_width)
            .saturating_sub(2);
        let mut lines = Vec::new();
        for (offset, spec) in matches.iter().skip(start).take(visible).enumerate() {
            let index = start + offset;
            let selected = index == cursor;
            let row_bg = if selected { COMPOSER_SELECTION } else { panel };
            let marker_style = if selected {
                Style::default()
                    .fg(accent)
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(muted).bg(row_bg)
            };
            let name_style = if selected {
                Style::default()
                    .fg(accent)
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(text).bg(row_bg)
            };
            let desc_style = if selected {
                Style::default().fg(text).bg(row_bg)
            } else {
                Style::default().fg(muted).bg(row_bg)
            };
            let name = command_label(spec, ":");
            let name = pad_to(&truncate(&name, name_width), name_width);
            let description = pad_to(&truncate(&spec.description, desc_width), desc_width);
            let gap = inner.saturating_sub(marker_width + name_width + desc_width);
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", if selected { "❯" } else { " " }),
                    marker_style,
                ),
                Span::styled(name, name_style),
                Span::styled(" ".repeat(gap), desc_style),
                Span::styled(description, desc_style),
            ]));
        }
        let title_left = Line::from(Span::styled(
            " : commands ",
            Style::default()
                .fg(accent)
                .bg(panel)
                .add_modifier(Modifier::BOLD),
        ));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(accent).bg(panel))
            .style(Style::default().bg(panel))
            .title_style(Style::default().bg(panel))
            .title_top(title_left);
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .style(Style::default().fg(text).bg(panel)),
            rect,
        );
    }

    /// Floating `/btw` answer. It owns the keyboard until dismissed.
    pub(super) fn draw_btw(&mut self, frame: &mut Frame, area: Rect) {
        let width = (area.width as u32 * 3 / 4).max(46).min(area.width as u32) as u16;
        let height = (area.height as u32 * 3 / 5)
            .clamp(10, 20)
            .min(area.height as u32) as u16;
        let rect = centered(area, width, height);
        let inner_width = width.saturating_sub(4) as usize;
        let inner_height = height.saturating_sub(2) as usize;

        let dialog_bg = dialog_background(self.theme);
        let session_name = self
            .btw
            .as_ref()
            .and_then(|btw| {
                self.views
                    .iter()
                    .find(|view| view.session.id == btw.session_id)
                    .map(|view| view.session.name.clone())
            })
            .unwrap_or_else(|| "side question".into());
        let Some(btw) = self.btw.as_mut() else {
            return;
        };

        let mut lines = vec![Line::from(vec![
            Span::styled("  ? ", Style::default().fg(composer_accent(self.theme))),
            Span::styled(
                btw.question.clone(),
                Style::default()
                    .fg(composer_text(self.theme))
                    .add_modifier(Modifier::ITALIC),
            ),
        ])];
        if let Some(error) = &btw.error {
            for segment in wrap_text(error, inner_width.saturating_sub(2)) {
                lines.push(Line::from(Span::styled(
                    format!("  {segment}"),
                    Style::default().fg(composer_danger(self.theme)),
                )));
            }
        } else if btw.text.is_empty() {
            lines.push(Line::from(Span::styled(
                if btw.done {
                    "  (no answer)"
                } else {
                    "  thinking…"
                },
                Style::default().fg(composer_muted(self.theme)),
            )));
        } else {
            for segment in wrap_text(&btw.text, inner_width.saturating_sub(2)) {
                lines.push(Line::from(format!("  {segment}")));
            }
        }
        let max_scroll = lines.len().saturating_sub(inner_height);
        btw.scroll = btw.scroll.min(max_scroll);
        let border = if btw.error.is_some() {
            composer_danger(self.theme)
        } else if btw.done {
            composer_success(self.theme)
        } else {
            composer_accent(self.theme)
        };
        frame.render_widget(Clear, rect);
        frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .style(Style::default().bg(dialog_bg))
            .title(Span::styled(
                format!(" btw · {session_name} "),
                Style::default().fg(border).add_modifier(Modifier::BOLD),
            ))
            .title_bottom(Span::styled(
                " Esc close · j/k scroll ",
                Style::default().fg(composer_muted(self.theme)),
            ));
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .style(Style::default().bg(dialog_bg))
                .scroll((btw.scroll.min(u16::MAX as usize) as u16, 0)),
            rect,
        );
    }

    pub(super) fn draw_diff_view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = self.theme;
        let dialog_bg = dialog_background(theme);
        let Some(session_id) = self.diff_view.as_ref().map(|view| view.session_id.clone()) else {
            return;
        };
        self.ensure_diff_rows(&session_id);
        let Some(approval) = self.approvals.get(&session_id) else {
            return;
        };
        let Some(rows) = self.diff_rows.as_ref().map(|cache| cache.rows.as_slice()) else {
            return;
        };
        let tool = approval.tool.as_str();
        let path = approval.path.as_deref().unwrap_or("");

        let rect = centered(
            area,
            area.width.saturating_sub(4),
            area.height.saturating_sub(4),
        );
        frame.render_widget(Clear, rect);
        let inner_width = rect.width.saturating_sub(2) as usize;
        let inner_height = rect.height.saturating_sub(3) as usize; // borders + hint
        frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);

        let interactive = !approval.hunks.is_empty();
        let (scroll, cursor) = self
            .diff_view
            .as_ref()
            .map(|view| (view.scroll, view.cursor))
            .unwrap_or((0, 0));
        let body_hit = self.diff_body.as_ref().is_some_and(|cache| {
            cache.rev == self.approval_rev
                && cache.session == session_id
                && cache.width == inner_width
                && cache.theme == theme
                && cache.interactive == interactive
                && cache.cursor == cursor
                && cache.selected.as_slice() == approval.selected.as_slice()
        });
        if !body_hit {
            let body = if !rows.is_empty() {
                match crate::tui::diff::DiffRenderer::new(
                    rows,
                    inner_width,
                    path,
                    theme,
                    "",
                    dialog_bg,
                ) {
                    Some(mut renderer) => {
                        renderer.render(rows, interactive, &approval.selected, cursor)
                    }
                    None => unified_review_body(approval, theme, inner_width, interactive, cursor),
                }
            } else {
                unified_review_body(approval, theme, inner_width, interactive, cursor)
            };
            self.diff_body = Some(DiffBodyCache {
                rev: self.approval_rev,
                session: session_id.clone(),
                width: inner_width,
                theme,
                interactive,
                selected: approval.selected.clone(),
                cursor,
                lines: body,
            });
        }
        let body = &self.diff_body.as_ref().expect("diff body").lines;
        let total = body.len();
        let max_scroll = total.saturating_sub(inner_height);
        let scroll = scroll.min(max_scroll);
        let mut window: Vec<Line> = body
            .iter()
            .skip(scroll)
            .take(inner_height)
            .cloned()
            .collect();
        let hint = if interactive {
            " j/k scroll  [ ] hunk  Space toggle  a all  y apply  n deny  Esc close "
        } else {
            " j/k scroll  y approve  n deny  Esc close "
        };
        window.push(Line::from(Span::styled(
            hint,
            Style::default().fg(composer_muted(theme)),
        )));

        let title = if path.is_empty() {
            format!(" diff · {tool} ")
        } else {
            format!(" diff · {tool} · {path} ")
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(composer_warn(theme)))
            .title(title);
        frame.render_widget(
            Paragraph::new(window)
                .block(block)
                .style(Style::default().fg(composer_text(theme)).bg(dialog_bg)),
            rect,
        );
        if let Some(view) = &mut self.diff_view {
            view.scroll = scroll;
            view.width = inner_width;
        }
    }

    /// Parse the focused approval's diff once. The rows are immutable for a
    /// given approval revision, so the card and the review share them.
    pub(super) fn ensure_diff_rows(&mut self, session_id: &str) {
        if self
            .diff_rows
            .as_ref()
            .is_some_and(|cache| cache.rev == self.approval_rev && cache.session == session_id)
        {
            return;
        }
        let Some(approval) = self.approvals.get(session_id) else {
            self.diff_rows = None;
            return;
        };
        let rows = crate::tui::diff::rows(&approval.hunks, approval.diff.as_deref());
        self.diff_rows = Some(DiffRowsCache {
            rev: self.approval_rev,
            session: session_id.to_string(),
            rows,
        });
    }

    pub(super) fn draw_inbox(&mut self, frame: &mut Frame, area: Rect) {
        let dialog_bg = dialog_background(self.theme);
        let Some(inbox) = &self.inbox else {
            return;
        };
        let rows = (inbox.items.len() as u16 + 4).min(area.height.saturating_sub(2).max(5));
        let rect = centered(area, area.width.saturating_sub(6).max(30), rows);
        frame.render_widget(Clear, rect);
        frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);
        let inner = rect.width.saturating_sub(2) as usize;

        let mut lines: Vec<Line> = Vec::new();
        for (index, item) in inbox.items.iter().enumerate() {
            let marker = if index == inbox.cursor { "▸ " } else { "  " };
            let kind = match item.kind {
                PendingKind::Tool => "tool",
                PendingKind::Patch => "patch",
            };
            let label = format!(
                "{marker}{:<20} {:<5} {:<14} {}",
                truncate(&item.session_name, 20),
                kind,
                item.tool,
                item.summary
            );
            let style = if index == inbox.cursor {
                Style::default()
                    .fg(composer_warn(self.theme))
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(composer_text(self.theme))
            };
            lines.push(Line::from(Span::styled(truncate(&label, inner), style)));
        }
        lines.push(Line::from(Span::styled(
            truncate(
                " Enter open · y approve · n deny · j/k select · Esc close ",
                inner,
            ),
            Style::default().fg(composer_muted(self.theme)),
        )));

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(composer_warn(self.theme)))
            .title(format!(" approvals · {} ", inbox.items.len()));
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .style(Style::default().fg(composer_text(self.theme)).bg(dialog_bg)),
            rect,
        );
    }

    pub(super) fn draw_processes(&mut self, frame: &mut Frame, area: Rect) {
        let dialog_bg = dialog_background(self.theme);
        let Some(view) = &mut self.process_view else {
            return;
        };
        // Process inspection is an overlay like the pickers, not a full-screen
        // mode; keep the same inset instead of covering the terminal.
        let rect = search_dialog_rect(area);
        frame.render_widget(Clear, rect);
        frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);
        let inner_height = rect.height.saturating_sub(2) as usize;
        let inner_width = rect.width.saturating_sub(2) as usize;
        // List on top (bounded), live output below.
        let list_height = (view.processes.len() + 1)
            .clamp(2, 10)
            .min(inner_height.saturating_sub(3).max(3));

        let mut list_lines: Vec<Line> = Vec::new();
        for (index, process) in view.processes.iter().enumerate() {
            list_lines.push(process_line(
                process,
                index == view.cursor,
                inner_width,
                self.theme,
            ));
        }
        if view.processes.is_empty() {
            list_lines.push(Line::from(Span::styled(
                "  no background processes",
                Style::default().fg(composer_muted(self.theme)),
            )));
        }
        let max_scroll = view.processes.len().saturating_sub(list_height);
        let list_scroll = view
            .cursor
            .saturating_sub(list_height.saturating_sub(1))
            .min(max_scroll);

        let title = match view.selected() {
            Some(process) => format!(
                " processes · {} · {} ",
                short_id(&process.id),
                process.status.as_str()
            ),
            None => format!(" processes · {} ", short_id(&view.session_id)),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(composer_accent(self.theme)))
            .title(title)
            .title_bottom(" j/k select · x kill · r refresh · PgUp/PgDn · Esc close ");
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        let chunks = Layout::vertical([Constraint::Length(list_height as u16), Constraint::Min(1)])
            .split(inner);
        frame.render_widget(
            Paragraph::new(
                list_lines
                    .into_iter()
                    .skip(list_scroll)
                    .take(list_height)
                    .collect::<Vec<_>>(),
            )
            .style(Style::default().fg(composer_text(self.theme)).bg(dialog_bg)),
            chunks[0],
        );

        let output_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(composer_inactive_border(self.theme)))
            .title(" output ");
        let output_inner = output_block.inner(chunks[1]);
        frame.render_widget(output_block, chunks[1]);
        let width = output_inner.width as usize;
        let height = output_inner.height as usize;
        let lines: Vec<&str> = view.tail.lines().collect();
        let start = lines.len().saturating_sub(height + view.scroll);
        let body: Vec<Line> = if lines.is_empty() {
            vec![Line::from(Span::styled(
                "(no output yet)",
                Style::default().fg(composer_muted(self.theme)),
            ))]
        } else {
            lines
                .iter()
                .skip(start)
                .take(height)
                .map(|line| Line::from(truncate(line, width)))
                .collect()
        };
        frame.render_widget(
            Paragraph::new(body)
                .style(Style::default().fg(composer_text(self.theme)).bg(dialog_bg)),
            output_inner,
        );
    }

    pub(super) fn draw_picker(&mut self, frame: &mut Frame, area: Rect) {
        let filtered = self.picker_filtered();
        let Some(kind) = self.picker.as_ref().map(|picker| picker.kind) else {
            return;
        };
        // Keep the cursor on a real row (the list may have shrunk under it).
        let selected = {
            let Some(picker) = &mut self.picker else {
                return;
            };
            if filtered.is_empty() {
                picker.selected = 0;
            } else {
                picker.selected = picker.selected.min(filtered.len() - 1);
            }
            picker.selected
        };
        let query = self
            .picker
            .as_ref()
            .map(|picker| picker.query.clone())
            .unwrap_or_default();
        let loading = self.picker.as_ref().is_some_and(|picker| picker.loading);
        // Queue a lazy preview whenever the selected row changes. The event
        // loop sends it after this draw so the UI never blocks on file/history
        // reads.
        self.sync_picker_preview(&filtered, selected);
        // Every picker follows the active palette; the theme picker previews
        // its highlighted palette immediately, including its own chrome.
        let theme = self.theme;
        let accent = composer_accent(theme);
        let muted = composer_muted(theme);
        let preview_text = composer_text(theme);
        let dialog_bg = dialog_background(theme);

        let rect = search_dialog_rect(area);
        frame.render_widget(Clear, rect);
        let title = format!(" {} ({}) ", kind.title(), filtered.len());
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(accent).bg(dialog_bg))
            .title_style(Style::default().bg(dialog_bg))
            .style(Style::default().bg(dialog_bg))
            .title(title)
            .title_bottom(format!(" {} ", kind.hint()));
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let body = draw_search_field(frame, inner, &query, accent, preview_text, muted, dialog_bg);
        let (list_area, preview_area) = if body.width >= 64 {
            // The sub-agent picker is a live console: keep the child list a
            // narrow rail and let the activity preview take the rest.
            let list_width = if kind == PickerKind::SubagentActivity {
                (body.width / 3).clamp(30, 34)
            } else {
                body.width * 58 / 100
            };
            let columns = Layout::horizontal([Constraint::Length(list_width), Constraint::Min(0)])
                .split(body);
            (columns[0], Some(columns[1]))
        } else {
            (body, None)
        };

        if filtered.is_empty() {
            let text = if loading {
                "loading…"
            } else if kind == PickerKind::Grep && query.trim().is_empty() {
                "type to search…"
            } else {
                "no matches"
            };
            frame.render_widget(
                Paragraph::new(text).style(Style::default().fg(muted).bg(dialog_bg)),
                list_area,
            );
        } else {
            // Build rows only for the visible window. A file picker can hold
            // thousands of rows, and every idle frame used to rebuild them all
            // just for ratatui to clip the overflow.
            let height = list_area.height as usize;
            let (start, end) = if height == 0 {
                (0, 0)
            } else {
                let max_start = filtered.len().saturating_sub(height);
                let mut start = self
                    .picker
                    .as_ref()
                    .map(|picker| picker.scroll.min(max_start))
                    .unwrap_or(0);
                if selected < start {
                    start = selected;
                } else if selected >= start + height {
                    start = selected + 1 - height;
                }
                if let Some(picker) = &mut self.picker {
                    picker.scroll = start;
                }
                (start, (start + height).min(filtered.len()))
            };
            let rows = self.picker_rows(kind, &filtered[start..end], list_area.width as usize);
            let list = List::new(rows)
                .style(Style::default().bg(dialog_bg))
                .highlight_style(
                    Style::default()
                        .bg(composer_selection(theme))
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("› ");
            let mut state = ListState::default().with_selected(Some(selected - start));
            frame.render_stateful_widget(list, list_area, &mut state);
        }

        if let Some(preview_area) = preview_area {
            let width = preview_area.width.saturating_sub(2) as usize;
            let height = preview_area.height as usize;
            self.sync_subagent_preview(&filtered, selected, width);
            let lines = self.picker_preview_lines(&filtered, height);
            let mut wrapped: Vec<String> = lines
                .iter()
                .flat_map(|line| wrap_text(line, width))
                .collect();
            // Session previews are conversations: keep the newest lines in
            // view instead of showing the oldest part of the fetched window.
            if matches!(
                kind,
                PickerKind::Sessions | PickerKind::NewPane | PickerKind::SubagentActivity
            ) && wrapped.len() > height
            {
                wrapped.drain(..wrapped.len() - height);
            }
            let body: Vec<Line> = wrapped.into_iter().take(height).map(Line::from).collect();
            let preview = Paragraph::new(body)
                .style(Style::default().fg(preview_text).bg(dialog_bg))
                .block(
                    Block::default()
                        .borders(Borders::LEFT)
                        .border_style(Style::default().fg(muted).bg(dialog_bg)),
                );
            frame.render_widget(preview, preview_area);
        }
    }

    /// List rows for the open picker. Session kinds render live session state;
    /// other kinds render the provider's label and detail.
    pub(super) fn picker_rows(
        &self,
        kind: PickerKind,
        filtered: &[usize],
        width: usize,
    ) -> Vec<ListItem<'_>> {
        match kind {
            PickerKind::SubagentActivity => {
                let Some(view) = self.focused_view() else {
                    return Vec::new();
                };
                let agents = view.subagents_sorted();
                // Dot, status label, padding and the highlight symbol claim 14
                // cells; the name gets what the narrow rail leaves so the
                // status stays visible.
                let name_width = width.saturating_sub(14).max(8);
                filtered
                    .iter()
                    .filter_map(|&index| agents.get(index))
                    .map(|(_, agent)| {
                        let (dot, label, color) = match agent.status {
                            transcript::SubagentStatus::Running => {
                                ("◐", "running", composer_accent(self.theme))
                            }
                            transcript::SubagentStatus::Completed => {
                                ("●", "done", composer_success(self.theme))
                            }
                            transcript::SubagentStatus::Failed => {
                                ("✗", "failed", composer_danger(self.theme))
                            }
                        };
                        let mut detail = transcript::last_activity(agent).unwrap_or_default();
                        if detail.is_empty() {
                            detail = if agent.status == transcript::SubagentStatus::Running {
                                if !agent.text.trim().is_empty() {
                                    "answering…".into()
                                } else if agent.reasoning.trim().is_empty() {
                                    "waiting for agent…".into()
                                } else {
                                    "thinking…".into()
                                }
                            } else {
                                "no activity".into()
                            };
                        }
                        ListItem::new(Line::from(vec![
                            Span::styled(format!("{dot} "), Style::default().fg(color)),
                            Span::styled(
                                pad_to(&truncate(&agent.name, name_width), name_width),
                                Style::default()
                                    .fg(composer_text(self.theme))
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(format!("  {label:<7} "), Style::default().fg(color)),
                            Span::styled(
                                truncate(&detail, 60),
                                Style::default().fg(composer_muted(self.theme)),
                            ),
                        ]))
                    })
                    .collect()
            }
            PickerKind::Sessions | PickerKind::NewPane => {
                let open_panes: Vec<Option<usize>> = self
                    .sessions
                    .iter()
                    .map(|session| {
                        self.views
                            .iter()
                            .position(|view| view.session.id == session.id)
                    })
                    .collect();
                filtered
                    .iter()
                    .map(|&index| {
                        let session = &self.sessions[index];
                        let pane = match open_panes[index] {
                            Some(pane) => Span::styled(
                                format!("{} ", pane + 1),
                                Style::default().fg(composer_accent(self.theme)),
                            ),
                            None => {
                                Span::styled("· ", Style::default().fg(composer_muted(self.theme)))
                            }
                        };
                        ListItem::new(Line::from(vec![
                            pane,
                            Span::styled(
                                format!("{} ", session.status.symbol()),
                                Style::default()
                                    .fg(composer_status_color(session.status, self.theme)),
                            ),
                            Span::styled(
                                pad_to(&truncate(&session.name, 28), 28),
                                Style::default()
                                    .fg(composer_text(self.theme))
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(
                                pad_to(origin_marker(session), 1),
                                Style::default().fg(composer_muted(self.theme)),
                            ),
                            Span::styled(
                                format!("  {}  ", short_id(&session.id)),
                                Style::default().fg(composer_muted(self.theme)),
                            ),
                            Span::styled(
                                session.cwd.display().to_string(),
                                Style::default().fg(composer_muted(self.theme)),
                            ),
                        ]))
                    })
                    .collect()
            }
            _ => {
                let Some(picker) = &self.picker else {
                    return Vec::new();
                };
                let label_style = Style::default()
                    .fg(composer_text(self.theme))
                    .add_modifier(Modifier::BOLD);
                let detail_color = composer_muted(self.theme);
                // Keep the second column aligned under the longest label in
                // the whole picker, not just the currently filtered rows.
                // Models use a wider separator; commands keep the `/` menu's
                // fixed command-name width.
                let model_width = if kind == PickerKind::Models {
                    picker
                        .items
                        .iter()
                        .map(|item| UnicodeWidthStr::width(item.label.as_str()))
                        .max()
                        .unwrap_or(0)
                        .min(PICKER_LABEL_MAX)
                } else {
                    0
                };
                let detail_width = if matches!(kind, PickerKind::Models | PickerKind::Commands) {
                    0
                } else {
                    picker
                        .items
                        .iter()
                        .filter(|item| {
                            item.detail
                                .as_deref()
                                .is_some_and(|detail| !detail.is_empty())
                        })
                        .map(|item| UnicodeWidthStr::width(item.label.as_str()))
                        .max()
                        .unwrap_or(0)
                        .min(PICKER_LABEL_MAX)
                };
                let separator = if kind == PickerKind::Models {
                    "    "
                } else {
                    "  "
                };
                filtered
                    .iter()
                    .filter_map(|&index| picker.items.get(index))
                    .map(|item| {
                        let label = match kind {
                            PickerKind::Models => {
                                pad_to(&truncate(&item.label, model_width), model_width)
                            }
                            PickerKind::Commands => pad_to(
                                &truncate(&item.label, COMMAND_NAME_WIDTH),
                                COMMAND_NAME_WIDTH,
                            ),
                            _ if item
                                .detail
                                .as_deref()
                                .is_some_and(|detail| !detail.is_empty()) =>
                            {
                                pad_to(&truncate(&item.label, detail_width), detail_width)
                            }
                            _ => item.label.clone(),
                        };
                        let mut spans = vec![Span::styled(label, label_style)];
                        if let Some(detail) = &item.detail {
                            if !detail.is_empty() {
                                spans.push(Span::styled(
                                    format!("{separator}{detail}"),
                                    Style::default().fg(detail_color),
                                ));
                            }
                        }
                        ListItem::new(Line::from(spans))
                    })
                    .collect()
            }
        }
    }

    /// Queue a preview request for the selected file or session row.
    pub(super) fn sync_picker_preview(&mut self, filtered: &[usize], selected: usize) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        let Some(provider) = picker.kind.preview_kind() else {
            return;
        };
        let Some(&index) = filtered.get(selected) else {
            return;
        };
        let value = match picker.kind {
            PickerKind::Sessions | PickerKind::NewPane => {
                self.sessions.get(index).map(|session| session.id.clone())
            }
            PickerKind::Files => picker.items.get(index).map(|item| item.value.clone()),
            _ => None,
        };
        let Some(value) = value else {
            return;
        };
        if picker.preview.key.as_deref() == Some(value.as_str()) {
            return;
        }
        let session_id = if picker.kind == PickerKind::Files {
            let Some(session_id) = self.focused_session_id() else {
                return;
            };
            Some(session_id)
        } else {
            None
        };
        if let Some(picker) = self.picker.as_mut() {
            picker.preview = PickerPreview {
                key: Some(value.clone()),
                text: None,
                error: None,
                loading: true,
            };
            picker.preview_request = Some((provider.to_string(), session_id, value));
        }
    }

    /// Rebuild the local preview for the selected child agent when its live
    /// revision or the preview width changes. Unlike file/session previews
    /// this never hits the daemon: child events and thinking are already in
    /// the parent view.
    pub(super) fn sync_subagent_preview(
        &mut self,
        filtered: &[usize],
        selected: usize,
        width: usize,
    ) {
        if self.picker.as_ref().map(|picker| picker.kind) != Some(PickerKind::SubagentActivity) {
            return;
        }
        let Some(&index) = filtered.get(selected) else {
            return;
        };
        // The focused session is part of the key: two views can hold children
        // with the same id and revision, and the picker can survive a focus
        // change between frames.
        let Some(key) = self.focused_view().and_then(|view| {
            view.subagents_sorted()
                .get(index)
                .map(|(id, _)| format!("{}:{id}:{}:{width}", view.session.id, view.subagents_rev))
        }) else {
            return;
        };
        if self
            .picker
            .as_ref()
            .is_some_and(|picker| picker.preview.key.as_deref() == Some(key.as_str()))
        {
            return;
        }
        let text = {
            let Some(view) = self.focused_view() else {
                return;
            };
            let Some((_, agent)) = view.subagents_sorted().get(index).copied() else {
                return;
            };
            subagent_preview_text(agent, width, self.theme)
        };
        if let Some(picker) = &mut self.picker {
            picker.preview = PickerPreview {
                key: Some(key),
                text: Some(text),
                error: None,
                loading: false,
            };
        }
    }

    /// Right-pane preview lines for the selected picker row. `height` bounds how
    /// much of a fetched file is copied; the draw path only needs one screen.
    pub(super) fn picker_preview_lines(&self, filtered: &[usize], height: usize) -> Vec<String> {
        let Some(picker) = &self.picker else {
            return Vec::new();
        };
        let Some(&index) = filtered.get(picker.selected) else {
            return Vec::new();
        };
        match picker.kind {
            PickerKind::SubagentActivity => {
                if let Some(text) = &picker.preview.text {
                    return preview_tail(text, height);
                }
                vec!["select a sub-agent".into()]
            }
            PickerKind::Sessions | PickerKind::NewPane => {
                if let Some(text) = &picker.preview.text {
                    return preview_tail(text, height);
                }
                let Some(session) = self.sessions.get(index) else {
                    return Vec::new();
                };
                let mut lines = vec![
                    session.name.clone(),
                    String::new(),
                    format!("id          {}", session.id),
                    format!("status      {}", session.status.as_str()),
                    format!("cwd         {}", session.cwd.display()),
                    format!("mode        {}", session.mode.as_str()),
                ];
                if let Some(worktree) = &session.worktree {
                    lines.push(format!("worktree    {}", worktree.display()));
                }
                lines.push(format!(
                    "model       {}",
                    session.model.as_deref().unwrap_or("(daemon default)")
                ));
                if let Some(error) = &picker.preview.error {
                    lines.push(String::new());
                    lines.push(format!("conversation preview: {error}"));
                } else if picker.preview.loading {
                    lines.push(String::new());
                    lines.push("loading conversation…".into());
                }
                lines
            }
            PickerKind::Files => {
                if let Some(text) = &picker.preview.text {
                    return preview_head(text, height);
                }
                if let Some(error) = &picker.preview.error {
                    return vec![format!("preview: {error}")];
                }
                if picker.preview.loading {
                    return vec!["loading file…".into()];
                }
                let Some(item) = picker.items.get(index) else {
                    return Vec::new();
                };
                item.preview
                    .as_ref()
                    .map(|preview| preview_head(preview, height))
                    .unwrap_or_else(|| vec![item.label.clone()])
            }
            _ => {
                let Some(item) = picker.items.get(index) else {
                    return Vec::new();
                };
                if let Some(preview) = &item.preview {
                    preview_head(preview, height)
                } else if let Some(detail) = &item.detail {
                    vec![item.label.clone(), String::new(), detail.clone()]
                } else {
                    vec![item.label.clone()]
                }
            }
        }
    }
}

/// Plain text of the selected child agent: identity/status plus its recent
/// event log and still-streaming thinking. Older events are dropped first so a
/// long-running child does not make every picker frame re-render its entire
/// history.
fn subagent_preview_text(agent: &Subagent, width: usize, theme: Theme) -> String {
    const MAX_EVENTS: usize = 300;
    const MAX_BYTES: usize = 64 * 1024;

    let status = match agent.status {
        transcript::SubagentStatus::Running => "running",
        transcript::SubagentStatus::Completed => "done",
        transcript::SubagentStatus::Failed => "failed",
    };
    let mut lines = vec![
        agent.name.clone(),
        String::new(),
        format!("status      {status}"),
        format!("call        {}", agent.call_id),
        format!("events      {}", agent.events.len()),
        String::new(),
        "── live activity ──".to_string(),
    ];
    let start = agent.events.len().saturating_sub(MAX_EVENTS);
    if start > 0 {
        lines.push(format!("… {} older event(s)", start));
    }
    let rendered = transcript::render_events(&agent.events[start..], width, theme);
    lines.extend(rendered.lines.iter().map(line_text));
    if !agent.reasoning.trim().is_empty() {
        lines.push(String::new());
        lines.push("✳ Thinking…".into());
        lines.extend(agent.reasoning.lines().map(str::to_string));
    }
    if !agent.text.trim().is_empty() {
        lines.push(String::new());
        lines.push("✎ Answer…".into());
        lines.extend(agent.text.lines().map(str::to_string));
    }
    if let Some(error) = agent
        .error
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        lines.push(String::new());
        lines.push(format!("! {error}"));
    }
    if agent.events.is_empty() {
        if let Some(result) = agent
            .result
            .as_deref()
            .filter(|text| !text.trim().is_empty())
        {
            lines.push(String::new());
            lines.push(result.to_string());
        }
    }
    let mut text = lines.join("\n");
    if text.len() > MAX_BYTES {
        let mut cut = text.len() - MAX_BYTES;
        while !text.is_char_boundary(cut) {
            cut += 1;
        }
        text = text[cut..].to_string();
    }
    text
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// First `height` source lines. Each source line wraps to at least one display
/// line, so this is enough to fill a preview pane.
fn preview_head(text: &str, height: usize) -> Vec<String> {
    text.lines()
        .take(height.max(1))
        .map(str::to_string)
        .collect()
}

/// Last `height` source lines, used for conversations where the newest output
/// matters more than the oldest.
fn preview_tail(text: &str, height: usize) -> Vec<String> {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let keep = height.max(1);
    if lines.len() > keep {
        lines.drain(..lines.len() - keep);
    }
    lines
}

/// Unified fallback used when the pane is too narrow for two code columns.
fn unified_review_body(
    approval: &PendingApproval,
    theme: Theme,
    width: usize,
    interactive: bool,
    cursor: usize,
) -> Vec<Line<'static>> {
    let mut body = Vec::new();
    if interactive {
        for (index, hunk) in approval.hunks.iter().enumerate() {
            let selected = approval.selected.get(index).copied().unwrap_or(true);
            let focused = index == cursor;
            for (line_index, line) in hunk.lines.iter().enumerate() {
                if line_index == 0 {
                    let marker = if focused { "▸ " } else { "  " };
                    let check = if selected { "[x] " } else { "[ ] " };
                    let style = if focused {
                        Style::default()
                            .fg(composer_focus(theme))
                            .add_modifier(Modifier::BOLD)
                    } else if selected {
                        Style::default().fg(composer_accent(theme))
                    } else {
                        Style::default().fg(composer_muted(theme))
                    };
                    body.push(Line::from(Span::styled(
                        truncate(&format!("{marker}{check}{line}"), width),
                        style,
                    )));
                } else {
                    body.push(Line::from(Span::styled(
                        truncate(&format!("    {line}"), width),
                        diff_line_style(line, theme),
                    )));
                }
            }
        }
    } else {
        for line in approval.diff.as_deref().unwrap_or("").lines() {
            body.push(Line::from(Span::styled(
                truncate(line, width),
                diff_line_style(line, theme),
            )));
        }
    }
    body
}
