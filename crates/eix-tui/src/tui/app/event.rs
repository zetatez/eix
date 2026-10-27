//! Top-level event loop: bus events, key/mouse/paste input dispatch and pane routing.

use super::*;

impl App {
    pub fn handle_paste(&mut self, text: &str) {
        if self.cmdline.is_some() {
            if let Some(cmdline) = self.cmdline.as_mut() {
                cmdline.insert_str(text);
            }
            self.reset_cmdline_menu_selection();
            return;
        }
        self.focus = Focus::Input;
        // Terminal drag-and-drop usually arrives as a bracketed paste of one
        // or more paths. Attach them instead of dumping quoted paths into the
        // prompt. Normal text pastes unchanged.
        let cwd = self
            .focused_view()
            .map(|view| view.session.cwd.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        if let Some(paths) = parse_image_paths(text, &cwd) {
            for path in paths {
                if !self.pending_images.contains(&path) {
                    self.pending_images.push(path);
                }
            }
            self.status = format!(
                "attached {} image(s) for the next prompt",
                self.pending_images.len()
            );
            return;
        }
        self.input.paste(text);
    }

    /// Mouse clicks focus a pane; dragging selects transcript text and copies
    /// it on release. A click without a drag still toggles the block under the
    /// cursor, except that clicking a subagent card opens its session. The
    /// wheel scrolls the pane under the cursor.
    pub fn handle_mouse(&mut self, mouse: MouseEvent, tx: &UnboundedSender<Command>) {
        self.ctrl_w_at = None;
        self.restore_leader_status();
        self.pending_leader = false;
        let (x, y) = (mouse.column, mouse.row);
        let pane = self.pane_areas.iter().position(|rect| {
            x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
        });
        let pane = pane.filter(|index| *index < self.views.len());
        if let Some(index) = pane {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.focused = index;
                    self.scroll_focused(-3);
                    return;
                }
                MouseEventKind::ScrollDown => {
                    self.focused = index;
                    self.scroll_focused(3);
                    return;
                }
                _ => {}
            }
        }

        // A click on the tab strip switches panes without moving the text
        // cursor away from the prompt, mirroring `Ctrl-N`/`Ctrl-1..9`.
        if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
            let tab = self
                .tab_areas
                .iter()
                .find(|(rect, index)| {
                    *index < self.views.len()
                        && x >= rect.x
                        && x < rect.x.saturating_add(rect.width)
                        && y >= rect.y
                        && y < rect.y.saturating_add(rect.height)
                })
                .map(|(_, index)| *index);
            if let Some(index) = tab {
                for view in &mut self.views {
                    view.selection = None;
                }
                self.focus_pane(index);
                return;
            }
        }

        let active = self
            .views
            .iter()
            .position(|view| view.selection.is_some_and(|selection| selection.dragging));
        let index = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(index) = pane else {
                    self.focus = Focus::Input;
                    for view in &mut self.views {
                        view.selection = None;
                    }
                    return;
                };
                self.focused = index;
                self.focus = Focus::Transcript;
                for view in &mut self.views {
                    view.selection = None;
                }
                index
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => {
                // Keep tracking the pane where the drag started, even when the
                // pointer crosses into another pane or leaves the transcript.
                let Some(index) = active else {
                    return;
                };
                index
            }
            _ => return,
        };
        let rect = self.pane_areas.get(index).copied().unwrap_or_default();
        if rect.width < 3 || rect.height < 3 {
            return;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let position = self.selection_position(index, rect, x, y, false);
                self.views[index].selection = position.map(|(line, column)| TextSelection {
                    anchor: (line, column),
                    cursor: (line, column),
                    dragging: true,
                    moved: false,
                });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(selection) = self.views[index].selection else {
                    return;
                };
                if !selection.dragging {
                    return;
                }
                let Some(position) = self.selection_position(index, rect, x, y, true) else {
                    return;
                };
                let view = &mut self.views[index];
                if let Some(selection) = view.selection.as_mut() {
                    if position != selection.cursor {
                        selection.moved = true;
                        selection.cursor = position;
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let mut copied = None;
                let mut fold = None;
                let mut jump = None;
                let mut clicked = None;
                {
                    let view = &mut self.views[index];
                    view.ensure_text_lines();
                    let Some(selection) = view.selection.as_mut() else {
                        return;
                    };
                    selection.dragging = false;
                    if selection.moved {
                        let text = selection_text(&view.text_lines, selection);
                        if !text.is_empty() {
                            copied = Some(text);
                        }
                    } else {
                        clicked = Some(selection.anchor);
                    }
                }
                if let Some((line, column)) = clicked {
                    let parent_link = self.views[index]
                        .text_lines
                        .get(line)
                        .is_some_and(|text| is_parent_link(text));
                    if parent_link {
                        self.views[index].selection = None;
                        self.back_to_parent(tx);
                        return;
                    }
                    let view = &self.views[index];
                    let candidate = view.slots.get(line).and_then(|slot| *slot);
                    if let Some(candidate) = candidate {
                        let text = view
                            .text_lines
                            .get(line)
                            .map(String::as_str)
                            .unwrap_or_default();
                        let is_subagent_header = (text.starts_with("  ▾ ")
                            || text.starts_with("  ▸ "))
                            && text.contains(" ↳ ");
                        // The caret remains the fold handle; the rest of the
                        // subagent header opens the child session.
                        if is_subagent_header && column > 3 {
                            jump = view
                                .subagent_session_for_fold(candidate.id)
                                .map(str::to_string);
                        }
                        if jump.is_none() {
                            fold = Some(candidate);
                        }
                    }
                }
                if let Some(session_id) = jump {
                    self.views[index].selection = None;
                    self.open_session_id_in_new_pane(session_id, tx);
                } else if let Some(fold) = fold {
                    self.views[index].folds.insert(fold.id, !fold.open);
                    self.views[index].selection = None;
                } else if let Some(text) = copied {
                    self.push_toast(
                        format!("Copied {} chars", text.chars().count()),
                        ToastTone::Success,
                        COPY_TOAST_DISPLAY,
                    );
                    // Seed the prompt's unnamed register too: normal-mode `p`
                    // can then paste the same text even if the native
                    // clipboard helper is unavailable.
                    self.input.set_register_text(&text);
                    self.pending_clipboard = Some(text);
                } else {
                    // A plain click, or a drag over nothing, leaves no
                    // zero-width selection behind.
                    self.views[index].selection = None;
                }
            }
            _ => {}
        }
    }

    /// Map a screen cell to a transcript line/column, clamping drags to the
    /// pane and auto-scrolling when the pointer crosses its top or bottom.
    pub(super) fn selection_position(
        &mut self,
        index: usize,
        rect: Rect,
        x: u16,
        y: u16,
        dragging: bool,
    ) -> Option<(usize, usize)> {
        let borders = pane_borders(&self.pane_areas, index);
        let left = rect
            .x
            .saturating_add(u16::from(borders.contains(Borders::LEFT)));
        let right = rect
            .x
            .saturating_add(rect.width)
            .saturating_sub(u16::from(borders.contains(Borders::RIGHT)));
        let top = rect
            .y
            .saturating_add(u16::from(borders.contains(Borders::TOP)));
        let bottom = rect
            .y
            .saturating_add(rect.height)
            .saturating_sub(u16::from(borders.contains(Borders::BOTTOM)));
        if right <= left || bottom <= top {
            return None;
        }
        if !dragging && (x < left || x >= right || y < top || y >= bottom) {
            return None;
        }

        let view = &mut self.views[index];
        view.ensure_text_lines();
        if dragging {
            let inner_height = rect
                .height
                .saturating_sub(u16::from(borders.contains(Borders::TOP)))
                .saturating_sub(u16::from(borders.contains(Borders::BOTTOM)))
                as usize;
            if y < top {
                view.history_armed = true;
                if view.scroll > 0 {
                    view.scroll -= 1;
                    view.follow = false;
                }
            } else if y >= bottom {
                let max_scroll = view.line_count.saturating_sub(inner_height);
                if view.scroll < max_scroll {
                    view.scroll += 1;
                    view.follow = false;
                }
            }
        }
        let x = x.clamp(left, right.saturating_sub(1));
        let y = y.clamp(top, bottom.saturating_sub(1));
        let line = view.scroll.saturating_add(y.saturating_sub(top) as usize);
        if view.text_lines.is_empty() {
            return Some((line, 0));
        }
        let line = line.min(view.text_lines.len() - 1);
        let width = UnicodeWidthStr::width(view.text_lines[line].as_str());
        Some((line, ((x - left) as usize).min(width)))
    }

    pub fn handle_event(&mut self, event: Event, tx: &UnboundedSender<Command>) {
        match event {
            Event::Hello(hello) => {
                self.connected = true;
                if hello.protocol != eix_core::PROTOCOL_VERSION {
                    self.error = Some(format!(
                        "daemon speaks protocol {}, client expects {}",
                        hello.protocol,
                        eix_core::PROTOCOL_VERSION
                    ));
                }
                self.status = format!("connected to eixd {}", hello.version);
                self.subagent_concurrency = hello.max_parallel_subagents;
                self.hello = Some(hello);
                // Fetch the catalog once so the prompt header can show the
                // raw model id and provider before `/model` is opened.
                let _ = tx.send(Command::ListModels);
                let _ = tx.send(Command::ListIntegrations);
            }
            Event::Integrations(integrations) => {
                for state in integrations {
                    self.integrations.insert(state.session_id.clone(), state);
                }
            }
            Event::Sessions(sessions) => {
                self.sessions = sessions;
                if !self.initialized {
                    self.initialized = true;
                    let index = if let Some(needle) = self.initial_session.as_deref() {
                        self.restore_sessions.clear();
                        self.restore_queue.clear();
                        self.restore_focused = None;
                        match self.find_session(needle) {
                            Some(index) => Some(index),
                            None => {
                                self.error = Some(format!("unknown session {needle:?}"));
                                (!self.sessions.is_empty())
                                    .then(|| self.selected.min(self.sessions.len() - 1))
                            }
                        }
                    } else {
                        let restored = self.take_restore_sessions();
                        match restored.split_first() {
                            Some((first, rest)) => {
                                self.restore_queue = rest.to_vec();
                                self.sessions
                                    .iter()
                                    .position(|session| session.id == *first)
                            }
                            None => self.last_session_for_cwd(),
                        }
                    };
                    match index {
                        Some(index) => self.select_session(index, tx),
                        // A launch from a new directory gets its own session
                        // instead of attaching to an unrelated one.
                        None => self.create_session(tx, None),
                    }
                } else {
                    self.reselect_current();
                }
            }
            Event::Attached(attached) => self.attached(attached, tx),
            Event::HistoryPage {
                session_id,
                events,
                has_more,
            } => self.history_page(session_id, events, has_more),
            Event::Created(session) => {
                self.creating = false;
                let id = session.id.clone();
                if session.worktree.is_none() {
                    self.created_sessions.insert(id.clone());
                }
                self.upsert_session(session, true);
                self.selected = self
                    .sessions
                    .iter()
                    .position(|session| session.id == id)
                    .unwrap_or(0);
                self.pending_attach = Some(PendingAttach {
                    session_id: id.clone(),
                    slot: AttachSlot::New,
                });
                self.error = None;
                self.status = "session created".into();
                let _ = tx.send(Command::Attach(id));
            }
            Event::Forked(session) => {
                let id = session.id.clone();
                let name = session.name.clone();
                let worktree = session.worktree.clone();
                self.upsert_session(session, true);
                self.selected = self
                    .sessions
                    .iter()
                    .position(|session| session.id == id)
                    .unwrap_or(0);
                self.pending_attach = Some(PendingAttach {
                    session_id: id.clone(),
                    slot: AttachSlot::New,
                });
                self.error = None;
                self.status = match worktree {
                    Some(path) => format!("forked {name} · worktree {}", path.display()),
                    None => format!("forked {name} · shared workspace"),
                };
                let _ = tx.send(Command::Attach(id));
            }
            Event::PickerData { kind, items, query } => {
                let Some(picker) = &mut self.picker else {
                    return;
                };
                if picker.kind.data_kind() != Some(kind.as_str()) {
                    return;
                }
                // A grep response can arrive after the user typed more; keep
                // the newer query's rows instead of flashing stale matches.
                if kind == eix_core::KIND_GREP
                    && query.as_deref().unwrap_or_default() != picker.query.trim()
                {
                    return;
                }
                let count = items.len();
                let grep = kind == eix_core::KIND_GREP;
                picker.set_items(items);
                picker.selected = 0;
                picker.loading = false;
                if grep {
                    self.status = if count == 0 {
                        "grep: no matches".into()
                    } else {
                        format!("grep: {count} match(es)")
                    };
                } else if count == 0 {
                    self.status = format!("no {kind} matched");
                } else {
                    self.status = format!("{count} {kind} row(s)");
                }
            }
            Event::PickerPreview {
                kind,
                value,
                text,
                error,
            } => {
                if let Some(picker) = &mut self.picker {
                    if picker.preview.key.as_deref() == Some(value.as_str())
                        && picker.kind.preview_kind() == Some(kind.as_str())
                    {
                        picker.preview.text = text;
                        picker.preview.error = error;
                        picker.preview.loading = false;
                    }
                }
            }
            Event::DefaultModel(model) => {
                if let Some(hello) = self.hello.as_mut() {
                    hello.model = model.clone();
                }
                let message = format!("default model → {model}");
                self.status = message.clone();
                self.push_toast(message, ToastTone::Success, RELOAD_TOAST_DISPLAY);
            }
            Event::Models {
                models,
                warnings,
                default_model,
            } => {
                if !default_model.is_empty() {
                    if let Some(hello) = self.hello.as_mut() {
                        hello.model = default_model;
                    }
                }
                let count = models.len();
                self.models = models;
                let current = self
                    .focused_view()
                    .and_then(|view| view.session.model.clone());
                let items = model_picker_items(&self.models);
                let selected = current
                    .as_deref()
                    .and_then(|model| {
                        let (base, variant) = split_model_variant(model);
                        items
                            .iter()
                            .position(|item| item.value == model)
                            .or_else(|| {
                                (variant == Some("none"))
                                    .then(|| items.iter().position(|item| item.value == base))
                                    .flatten()
                            })
                    })
                    .unwrap_or(0);
                if let Some(picker) = &mut self.picker {
                    if picker.kind == PickerKind::Models {
                        picker.set_items(items);
                        picker.selected = selected;
                        picker.loading = false;
                    }
                }
                self.status = format!("{count} model(s)");
                if let Some(warning) = warnings.first() {
                    self.status.push_str(&format!(" · {warning}"));
                }
            }
            Event::Agents(agents) => {
                if let Some(picker) = &mut self.picker {
                    if picker.kind == PickerKind::Agents {
                        picker.set_items(agent_picker_items(&agents));
                        picker.selected = 0;
                        picker.loading = false;
                    }
                }
                self.status = format!("{} agent(s)", agents.len());
            }
            Event::Mcp(servers) => {
                if let Some(picker) = &mut self.picker {
                    if picker.kind == PickerKind::Mcp {
                        picker.set_items(mcp_picker_items(&servers));
                        picker.selected = 0;
                        picker.loading = false;
                    }
                }
                if servers.is_empty() {
                    self.status = "no MCP servers configured (see [mcp.servers])".into();
                } else {
                    let parts: Vec<String> = servers
                        .iter()
                        .map(|server| {
                            if !server.enabled {
                                format!("{}: disabled", server.name)
                            } else if let Some(error) = &server.error {
                                format!("{}: error — {}", server.name, truncate(error, 60))
                            } else {
                                format!("{}: {} tool(s)", server.name, server.tools)
                            }
                        })
                        .collect();
                    self.status = parts.join(" · ");
                }
            }
            Event::PluginCommands(commands) => {
                self.set_plugin_commands(commands);
            }
            Event::SubagentConcurrency { value } => {
                self.subagent_concurrency = value;
                self.status = format!("subagents → {value} (runtime)");
            }
            Event::ConfigReloaded {
                paths,
                max_parallel_subagents,
            } => {
                self.subagent_concurrency = max_parallel_subagents;
                // Command files are intentionally read at startup and on an
                // explicit reload, not every time the menu opens.
                self.reload_commands();
                let message = format!("config reloaded ({} layer(s))", paths.len());
                self.status = message.clone();
                self.push_toast(message, ToastTone::Success, RELOAD_TOAST_DISPLAY);
            }
            Event::Archived(session) => {
                let id = session.id.clone();
                self.created_sessions.remove(&id);
                self.sessions.retain(|existing| existing.id != id);
                self.views.retain(|view| view.session.id != id);
                if self.focused >= self.views.len() {
                    self.focused = self.views.len().saturating_sub(1);
                }
                self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
                self.status = format!("archived {}", session.name);
            }
            Event::Deleted(id) => {
                self.created_sessions.remove(&id);
                self.sessions.retain(|existing| existing.id != id);
                self.views.retain(|view| view.session.id != id);
                if self.focused >= self.views.len() {
                    self.focused = self.views.len().saturating_sub(1);
                }
                self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
                self.status = "session deleted".into();
            }
            Event::ShellRan {
                ok,
                output,
                insert_output,
            } => {
                if insert_output {
                    let text = output.trim_end_matches(['\r', '\n']);
                    if text.is_empty() {
                        self.status = if ok {
                            "shell output empty".into()
                        } else {
                            "shell failed with no output".into()
                        };
                    } else {
                        self.focus = Focus::Input;
                        self.pending_leader = false;
                        self.error = None;
                        let prefix = if self.input.is_empty() { "" } else { "\n" };
                        self.input.insert_text(&format!("{prefix}{text}"));
                        self.status = format!(
                            "read {} line(s) from shell{}",
                            text.lines().count(),
                            if ok { "" } else { " (failed)" }
                        );
                    }
                    return;
                }
                let first = output.lines().next().unwrap_or_default();
                self.status = if first.is_empty() {
                    if ok {
                        "shell ok".into()
                    } else {
                        "shell failed".into()
                    }
                } else {
                    format!(
                        "shell {} · {}",
                        if ok { "ok" } else { "failed" },
                        truncate(first, 80)
                    )
                };
            }
            Event::Todos(items) => {
                if let Some(picker) = &mut self.picker {
                    if picker.kind == PickerKind::Todos {
                        picker.set_items(todo_picker_items(&items));
                        picker.selected = 0;
                        picker.loading = false;
                    }
                }
                if items.is_empty() {
                    self.status = "no tasks".into();
                } else {
                    let done = items
                        .iter()
                        .filter(|item| item.status == "completed")
                        .count();
                    let active = items
                        .iter()
                        .find(|item| item.status == "in_progress")
                        .or_else(|| items.iter().find(|item| item.status == "pending"));
                    self.status = match active {
                        Some(item) => format!(
                            "todos {done}/{} · in progress: {}",
                            items.len(),
                            item.content
                        ),
                        None => format!("todos {done}/{} · all done", items.len()),
                    };
                }
            }
            Event::Rewritten(session_id) => {
                if let Some(index) = self
                    .views
                    .iter()
                    .position(|view| view.session.id == session_id)
                {
                    self.pending_attach = Some(PendingAttach {
                        session_id: session_id.clone(),
                        slot: AttachSlot::Existing(index),
                    });
                    let _ = tx.send(Command::Attach(session_id));
                    self.status = "history updated".into();
                }
            }
            Event::Draft { session_id, text } => {
                if self.input_session.as_deref() != Some(session_id.as_str()) {
                    return;
                }
                // The prompt was cleared on switch; load the daemon draft, but
                // never clobber text typed while the request was in flight.
                // Keep normal mode: a late draft load must not turn the
                // prompt back into insert mode under the user.
                if self.input.is_empty() {
                    if !text.is_empty() {
                        self.replace_prompt_text(&text);
                    }
                    self.last_saved_draft = self.input.text();
                }
                self.draft_loading = None;
            }
            Event::PatchEdit {
                session_id,
                patch_id,
                text,
            } => self.edit_patch(&session_id, &patch_id, &text, tx),
            Event::Submitted(submitted) => self.submitted(submitted),
            Event::Updated(session) => {
                let id = session.id.clone();
                let previous = self
                    .views
                    .iter()
                    .find(|view| view.session.id == id)
                    .map(|view| (view.session.cwd.clone(), view.session.mode));
                if let Some(view) = self.views.iter_mut().find(|view| view.session.id == id) {
                    view.session = session.clone();
                }
                if let Some((cwd, mode)) = previous {
                    if cwd != session.cwd {
                        self.status = format!("cwd: {}", session.cwd.display());
                    } else if mode != session.mode {
                        self.status = format!("mode: {}", session.mode.label());
                    }
                }
                self.upsert_session(session, false);
                self.error = None;
            }
            Event::ContextCleared { session_id } => {
                if let Some(view) = self
                    .views
                    .iter_mut()
                    .find(|view| view.session.id == session_id)
                {
                    view.stream = None;
                }
                self.status = "context cleared · earlier turns stay in the transcript".into();
                self.error = None;
            }
            Event::Compacted {
                session_id,
                entries,
                tokens,
            } => {
                if let Some(view) = self
                    .views
                    .iter_mut()
                    .find(|view| view.session.id == session_id)
                {
                    view.follow = true;
                }
                self.status =
                    format!("compacted · context now {tokens} tokens ({entries} entries)");
                self.error = None;
            }
            Event::BtwStarted { session_id, btw_id } => {
                if let Some(btw) = self.btw.as_mut() {
                    if btw.session_id == session_id {
                        btw.btw_id = btw_id;
                    }
                }
            }
            Event::BtwCancelled { session_id } => {
                if self
                    .btw
                    .as_ref()
                    .is_some_and(|btw| btw.session_id == session_id)
                {
                    self.btw = None;
                    self.status = "side question cancelled".into();
                }
            }
            Event::Cancelled(cancelled) => {
                self.status = if cancelled {
                    "cancel requested".into()
                } else {
                    "nothing is running".into()
                };
            }
            Event::Approval {
                approval_id,
                accepted,
            } => {
                if accepted {
                    let sessions: Vec<String> = self
                        .approvals
                        .iter()
                        .filter(|(_, pending)| pending.approval_id == approval_id)
                        .map(|(session_id, _)| session_id.clone())
                        .collect();
                    for session_id in sessions {
                        self.reload_pending(&session_id);
                    }
                    self.status = "approval sent".into();
                } else {
                    self.status = "approval was stale; the daemon is still waiting".into();
                }
            }
            Event::InputResponded { accepted } => {
                if accepted {
                    self.status = "answer sent".into();
                } else {
                    self.status = "answer was stale; the daemon is still waiting".into();
                    if let Some(session_id) =
                        self.focused_view().map(|view| view.session.id.clone())
                    {
                        self.reload_input(&session_id);
                    }
                }
            }
            Event::Processes { session_id, result } => {
                if let Some(view) = &mut self.process_view {
                    if view.session_id == session_id {
                        let selected = view.selected().map(|process| process.id.clone());
                        view.set_list(result);
                        self.status = format!("processes: {}", view.processes.len());
                        // Load the tail of the (possibly new) selection.
                        let next = view.selected().map(|process| process.id.clone());
                        if next != selected {
                            if let Some(process_id) = next {
                                let _ = tx.send(Command::ProcessOutput { process_id });
                            }
                        }
                    }
                }
            }
            Event::ProcessOutput { result } => {
                if let Some(view) = &mut self.process_view {
                    if view.selected().map(|process| process.id.as_str())
                        == Some(result.process_id.as_str())
                    {
                        view.set_tail(result);
                    }
                }
            }
            Event::ProcessKilled { process_id, killed } => {
                self.status = if killed {
                    format!("killing process {}", short_id(&process_id))
                } else {
                    format!("process {} is not running", short_id(&process_id))
                };
            }
            Event::Notification(notification) => self.notification(notification, tx),
            Event::Error { context, message } => {
                self.error = Some(message.clone());
                self.status = format!("{context} failed");
                match context.as_str() {
                    "submit" => {
                        self.submitting = false;
                        self.submitting_session = None;
                        self.pending_prompt = None;
                        self.pending_images = std::mem::take(&mut self.inflight_images);
                        if let Some((session_id, text)) = self.queued_inflight.take() {
                            // The daemon still considered the turn busy (the
                            // completion notification and the turns map race).
                            // Put it back at the head and retry on the next tick.
                            self.queues.entry(session_id).or_default().push_front(text);
                        } else if let Some((session_id, draft)) = self.pending_draft.take() {
                            if message.contains("already running") {
                                // The local status was stale: queue the prompt
                                // instead of losing the "submit while busy"
                                // intent. The tick retries it when the daemon
                                // reports the session idle.
                                self.enqueue(session_id, &draft, tx);
                                self.error = None;
                            } else {
                                // The prompt was cleared when the request was
                                // sent; put the draft back so it can be edited
                                // or retried without changing the mode.
                                self.replace_prompt_text(&draft);
                            }
                        }
                    }
                    "create" => self.creating = false,
                    "attach" => self.pending_attach = None,
                    "history" => {
                        for view in &mut self.views {
                            view.history_loading = false;
                            // Re-arm only after the user scrolls down again,
                            // so a persistent error cannot spam requests.
                            view.history_armed = false;
                        }
                    }
                    "connect" => {
                        self.connected = false;
                        self.initialized = true;
                    }
                    "list" if !self.initialized => self.initialized = true,
                    "draft" => self.draft_loading = None,
                    "btw" => {
                        if let Some(btw) = self.btw.as_mut() {
                            btw.error = Some(message.clone());
                            btw.done = true;
                        }
                    }
                    _ => {}
                }
            }
            Event::Closed(reason) => {
                self.connected = false;
                self.error = Some(reason.clone());
                self.exit_message = Some(reason);
                self.should_quit = true;
            }
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        // Any other key breaks the double-Ctrl-W gesture, so a later Ctrl-W
        // cannot accidentally pair with one pressed earlier.
        if !(key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('w') | KeyCode::Char('W')))
        {
            self.ctrl_w_at = None;
        }
        if self.pending_leader {
            self.restore_leader_status();
        }
        // Quit works from every mode, including insert and any open overlay.
        // Save the draft first so quitting never loses it.
        if self.keybinds.global_action(key) == Some(KeyAction::Quit) {
            self.pending_leader = false;
            self.sync_draft(tx);
            self.should_quit = true;
            return;
        }
        // Errors float above everything else and keep the keyboard until
        // dismissed, so a failed action cannot silently disappear.
        if self.error.is_some() {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('Q')
            ) {
                self.error = None;
            }
            return;
        }
        // The `:` command line is modal: every key edits it or submits/cancels
        // it until it closes.
        if self.cmdline.is_some() {
            self.handle_cmdline_key(key, tx);
            return;
        }
        if self.keybinds.global_action(key) == Some(KeyAction::Help) {
            self.pending_leader = false;
            self.open_help();
            return;
        }

        // The `/btw` answer floats above the prompt and owns the keyboard until
        // it is dismissed.
        if self.btw.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                    let session_id = self.btw.as_ref().map(|btw| btw.session_id.clone());
                    self.btw = None;
                    if let Some(session_id) = session_id {
                        let _ = tx.send(Command::BtwCancel { session_id });
                    }
                    self.status = "side question closed".into();
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if let Some(btw) = self.btw.as_mut() {
                        btw.scroll = btw.scroll.saturating_add(1);
                    }
                }
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(btw) = self.btw.as_mut() {
                        btw.scroll = btw.scroll.saturating_add(1);
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if let Some(btw) = self.btw.as_mut() {
                        btw.scroll = btw.scroll.saturating_sub(1);
                    }
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(btw) = self.btw.as_mut() {
                        btw.scroll = btw.scroll.saturating_sub(1);
                    }
                }
                KeyCode::PageDown => {
                    if let Some(btw) = self.btw.as_mut() {
                        btw.scroll = btw.scroll.saturating_add(8);
                    }
                }
                KeyCode::PageUp => {
                    if let Some(btw) = self.btw.as_mut() {
                        btw.scroll = btw.scroll.saturating_sub(8);
                    }
                }
                KeyCode::Char('b') | KeyCode::Char('B')
                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if let Some(btw) = self.btw.as_mut() {
                        btw.scroll = btw.scroll.saturating_sub(8);
                    }
                }
                KeyCode::Char('f') | KeyCode::Char('F')
                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if let Some(btw) = self.btw.as_mut() {
                        btw.scroll = btw.scroll.saturating_add(8);
                    }
                }
                _ => {}
            }
            return;
        }

        if self.help {
            self.handle_help_key(key);
            return;
        }

        if self.reason_prompt.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.reason_prompt = None;
                    self.status = "denial cancelled".into();
                }
                KeyCode::Enter => {
                    if let Some(prompt) = self.reason_prompt.take() {
                        let reason = prompt.text.trim().to_string();
                        self.respond_approval_for(
                            &prompt.session_id,
                            ApprovalDecision::Deny,
                            None,
                            (!reason.is_empty()).then_some(reason),
                            tx,
                        );
                    }
                }
                KeyCode::Backspace => {
                    if let Some(prompt) = self.reason_prompt.as_mut() {
                        prompt.text.pop();
                    }
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(prompt) = self.reason_prompt.as_mut() {
                        prompt.text.push(ch);
                    }
                }
                _ => {}
            }
            return;
        }

        if self.process_view.is_some() {
            self.handle_process_key(key, tx);
            return;
        }

        if self.inbox.is_some() {
            self.handle_inbox_key(key, tx);
            return;
        }

        if self.picker.is_some() {
            self.handle_picker_key(key, tx);
            return;
        }

        // A blocking approval/question card owns its navigation keys no matter
        // whether the prompt or the transcript has focus. Other overlays above
        // keep priority.
        if self.diff_view.is_none() && !self.pending_leader && self.handle_card_key(key, tx) {
            return;
        }

        // Global/TUI actions that work from both the prompt and the
        // transcript. Ctrl-1..9 needs a terminal with keyboard disambiguation
        // (kitty protocol); Ctrl-7 is the legacy encoding of Ctrl-/ on some
        // terminals and is therefore a model-picker alias by default.
        if let Some(action) = self.keybinds.global_action(key) {
            self.pending_leader = false;
            self.run_key_action(action, tx);
            return;
        }

        // Vim-style command line: `:` in normal mode, or from the transcript.
        // Insert/visual modes keep typing a literal colon.
        if key.code == KeyCode::Char(':')
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && self.overlay_closed()
            && !self.pending_leader
            && self.focused_approval().is_none()
            && self.focused_input().is_none()
            && (self.focus == Focus::Transcript
                || (self.input.mode() == Mode::Normal && self.input.normal_idle()))
        {
            self.open_cmdline();
            return;
        }

        if self.focus == Focus::Input {
            self.handle_input_key(key, tx);
        } else {
            self.handle_transcript_key(key, tx);
        }
    }

    /// TUI actions shared by transcript focus and the prompt's normal mode.
    /// The prompt keeps its vim editing keys (`j`/`k`, ...); this handler owns
    /// the configurable normal/transcript keymap, leader, and session promotion.
    pub(super) fn handle_shared_normal_key(
        &mut self,
        key: KeyEvent,
        tx: &UnboundedSender<Command>,
    ) -> bool {
        let available = self.focused_approval().is_none() && self.focused_input().is_none();
        if self.pending_leader {
            self.pending_leader = false;
            if available {
                if let Some(action) = self.keybinds.leader_action(key) {
                    self.run_key_action(action, tx);
                }
            }
            return true;
        }
        if self.keybinds.is_leader(key) {
            // A blocking card owns the keyboard; never start a hidden leader
            // under it, and never let the editor start a second one either.
            if available {
                self.pending_leader = true;
                self.leader_status_backup = Some(std::mem::take(&mut self.status));
                self.status = "leader: f/g/s · i subagents · n new · o pane · v/t/r/l layout · a approvals · c close · m mode · Esc cancel"
                    .into();
            }
            return true;
        }
        if available {
            if let Some(action) = self.keybinds.normal_action(key) {
                self.run_key_action(action, tx);
                return true;
            }
        }
        // Normal-mode Enter promotes the focused pane's session to the front.
        // Keep it after `normal_action` so an explicit `[keys]` binding wins.
        if available
            && self.pending_attach.is_none()
            && key.code == KeyCode::Enter
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            self.promote_focused_session();
            return true;
        }
        // The editor has its own legacy Space leader for direct tests. In the
        // app, consume Space when the user moved the leader elsewhere so that
        // old chords cannot stay active behind the config.
        if key.code == KeyCode::Char(' ')
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return true;
        }
        // Ctrl-W stays a Vim-style gesture (and deletes a word in insert).
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('w') | KeyCode::Char('W'))
        {
            let now = Instant::now();
            let double = self
                .ctrl_w_at
                .is_some_and(|last| now.duration_since(last) < CTRL_W_WINDOW);
            self.ctrl_w_at = (!double).then_some(now);
            if double {
                self.cycle_pane(1);
            }
            return true;
        }
        if matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N'))
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && self.transcript_search.is_some()
        {
            self.search_transcript_next(matches!(key.code, KeyCode::Char('N')));
            return true;
        }
        match key.code {
            KeyCode::Char(ch @ ('/' | '?' | '#')) => {
                self.open_cmdline_with(ch);
                true
            }
            _ => false,
        }
    }

    pub(super) fn handle_input_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        // The diff overlay owns the keyboard regardless of which pane was
        // focused when it opened, matching the transcript path.
        if self.diff_view.is_some() {
            self.handle_diff_key(key, tx);
            return;
        }
        // Insert/visual keys belong to the editor, so a pane leader must not
        // leak into typed input. Normal mode shares the transcript leader.
        if self.input.mode() != Mode::Normal {
            self.pending_leader = false;
        }
        // In normal mode the prompt behaves like a focused pane for the TUI
        // actions the editor does not own. A half-typed vim prefix keeps the
        // key for the editor instead. This runs before the clipboard paste
        // path so leader+`p` still opens processes instead of pasting.
        if self.input.mode() == Mode::Normal
            && self.input.normal_idle()
            && self.handle_shared_normal_key(key, tx)
        {
            return;
        }
        // A half-typed vim prefix keeps the key for the editor, but Space must
        // not revive the editor's legacy leader map behind `[keys].leader`.
        if self.input.mode() == Mode::Normal
            && key.code == KeyCode::Char(' ')
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return;
        }
        // Normal-mode `p`/`P` can paste the desktop clipboard as well as the
        // editor's own register. Prefer the register when it already holds a
        // yank/delete so Vim editing keeps working; otherwise read the system
        // clipboard (e.g. text selected with the mouse in the transcript).
        if matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P'))
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && self.input.normal_paste_ready()
            && !self.input.has_register_text()
        {
            if let Some(text) = crate::tui::read_clipboard() {
                let chars = text.chars().count();
                self.input
                    .paste_external(&text, matches!(key.code, KeyCode::Char('p')));
                self.status = format!("pasted {chars} chars");
                self.esc_at = None;
                return;
            }
        }

        // While typing a bare `/command`, the suggestion menu owns vertical
        // navigation, Enter and Tab. Other keys fall through to the editor so
        // the prefix keeps filtering the list.
        if self.command_menu_open() {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('n') | KeyCode::Char('j') => {
                        self.move_command_cursor(1);
                        return;
                    }
                    KeyCode::Char('p') | KeyCode::Char('k') => {
                        self.move_command_cursor(-1);
                        return;
                    }
                    _ => {}
                }
            }
            match key.code {
                KeyCode::Down => {
                    self.move_command_cursor(1);
                    return;
                }
                KeyCode::Up => {
                    self.move_command_cursor(-1);
                    return;
                }
                KeyCode::Tab => {
                    self.complete_command();
                    return;
                }
                KeyCode::Enter => {
                    if let Some(spec) = self.selected_command() {
                        let line = format!(
                            "/{}{}",
                            spec.name,
                            if spec.args.is_empty() { "" } else { " " }
                        );
                        self.input.set_text(&line);
                        self.input.enter_normal();
                        self.submit(tx);
                        return;
                    }
                }
                _ => {}
            }
        }

        // A single Esc returns to normal mode and keeps the prompt focused.
        // In normal mode, two Esc presses within the window interrupt a
        // running turn; when nothing is running they hand the keyboard back to
        // the transcript.
        if matches!(key.code, KeyCode::Esc) {
            // A running `/` or `?` search owns a single Esc: drop its
            // highlights before the double-Esc focus gesture can fire.
            if self.input.mode() == Mode::Normal && self.transcript_search.is_some() {
                self.transcript_search = None;
                self.esc_at = None;
                self.status = "search cleared".into();
                return;
            }
            let editing = self.input.mode() != Mode::Normal;
            let _ = self.input.handle_key(key);
            if editing {
                // Do not arm the double-Esc timer: after leaving insert the
                // user must still tap Esc twice to interrupt.
                self.esc_at = None;
                self.command_dismissed = true;
                return;
            }
            let now = Instant::now();
            let double = self
                .esc_at
                .is_some_and(|last| now.duration_since(last) < ESC_WINDOW);
            if double {
                self.esc_at = None;
                if self.focused_session_busy() {
                    self.cancel(tx);
                } else {
                    self.focus = Focus::Transcript;
                }
            } else {
                self.esc_at = Some(now);
            }
            self.command_dismissed = true;
            return;
        }

        self.esc_at = None;
        self.command_dismissed = false;
        match self.input.handle_key(key) {
            Outcome::Submit => self.submit(tx),
            Outcome::FilePicker => self.open_picker(PickerKind::Files, tx),
            Outcome::History(delta) => self.navigate_history(delta),
            Outcome::Action(id) if self.overlay_closed() => {
                // The editor still knows the old Vim-adjacent TUI shortcuts
                // (`q`, `Z`, `<`, `>`, Tab). They are only allowed when the
                // current keymap still binds that action to this key; after a
                // `[keys]` override the legacy key becomes inert.
                if let Some(action) = KeyAction::parse(id) {
                    if self.keybinds.normal_action(key) == Some(action) {
                        self.run_key_action(action, tx);
                    }
                } else {
                    self.run_action(id, tx);
                }
            }
            Outcome::Ignored | Outcome::Action(_) => {}
        }
    }

    pub(super) fn focus_input(&mut self) {
        self.focus = Focus::Input;
        self.pending_leader = false;
        self.esc_at = None;
        self.input.enter_insert();
    }

    pub(super) fn handle_transcript_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        if self.diff_view.is_some() {
            self.handle_diff_key(key, tx);
            return;
        }
        // Direct test callers may reach the transcript handler without going
        // through `handle_key`; keep card navigation working there too.
        if self.handle_card_key(key, tx) {
            return;
        }
        if self.handle_shared_normal_key(key, tx) {
            return;
        }
        if key.code == KeyCode::Esc && self.transcript_search.is_some() {
            self.transcript_search = None;
            self.esc_at = None;
            self.status = "search cleared".into();
            return;
        }
        if key.code == KeyCode::Esc {
            // The transcript is also a valid start point: after `:` closes
            // (which may have been opened from here), two Esc presses should
            // interrupt the focused turn just like they do in the prompt.
            let now = Instant::now();
            let double = self
                .esc_at
                .is_some_and(|last| now.duration_since(last) < ESC_WINDOW);
            if double {
                self.esc_at = None;
                if self.focused_session_busy() {
                    self.cancel(tx);
                }
            } else {
                self.esc_at = Some(now);
            }
            return;
        }
        // Vim navigation stays in the transcript. Global/TUI actions (including
        // q/Z/< >/Tab) are handled by the shared keymap above so `[keys]` can
        // rebind or remove them.
        match key.code {
            // `i` is the one bare letter that stays: focusing the prompt is
            // the normal way out of the transcript, like vim insert mode.
            KeyCode::Char('i') => self.focus_input(),
            KeyCode::Char('j') | KeyCode::Down => self.scroll_focused(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_focused(-1),
            KeyCode::Char('g') => {
                if let Some(view) = self.focused_view_mut() {
                    view.scroll = 0;
                    view.follow = false;
                    view.history_armed = true;
                }
            }
            KeyCode::Char('G') => {
                if let Some(view) = self.focused_view_mut() {
                    view.follow = true;
                }
            }
            KeyCode::Esc => {}
            _ => {}
        }
    }

    pub(super) fn run_key_action(&mut self, action: KeyAction, tx: &UnboundedSender<Command>) {
        match action {
            KeyAction::Quit => self.run_action("quit", tx),
            KeyAction::Help => self.open_help(),
            KeyAction::NewSession => self.run_action("new", tx),
            KeyAction::NewWorktree => self.run_action("new-worktree", tx),
            KeyAction::Sessions => self.open_picker(PickerKind::Sessions, tx),
            KeyAction::NewPane => self.run_action("new-pane", tx),
            KeyAction::Palette => self.run_action("commands", tx),
            KeyAction::Files => self.run_action("files", tx),
            KeyAction::Grep => self.run_action("grep", tx),
            KeyAction::Subagents => self.open_subagents(tx),
            KeyAction::Models => self.run_action("model", tx),
            KeyAction::Git => self.run_action("git", tx),
            KeyAction::Processes => self.run_action("processes", tx),
            KeyAction::Inbox => self.run_action("approvals", tx),
            KeyAction::Fork => self.run_action("fork", tx),
            KeyAction::Attention => self.run_action("attention", tx),
            KeyAction::Cancel => self.run_action("cancel", tx),
            KeyAction::NextSession => self.cycle_session(1, tx),
            KeyAction::PrevSession => self.cycle_session(-1, tx),
            KeyAction::PaneNext => self.run_action("pane-next", tx),
            KeyAction::PanePrev => self.run_action("pane-prev", tx),
            KeyAction::ScrollDown => self.scroll_focused(1),
            KeyAction::ScrollUp => self.scroll_focused(-1),
            KeyAction::PageDown => self.scroll_focused(10),
            KeyAction::PageUp => self.scroll_focused(-10),
            KeyAction::Timeline => self.open_picker(PickerKind::Timeline, tx),
            KeyAction::Todos => {
                if let Some(session_id) = self.focused_session_id() {
                    let _ = tx.send(Command::ListTodos { session_id });
                    self.status = "todos: loading…".into();
                }
            }
            KeyAction::Undo | KeyAction::Redo => {
                if let Some(session_id) = self.focused_session_id() {
                    let redo = action == KeyAction::Redo;
                    let _ = tx.send(Command::Undo { session_id, redo });
                    self.status = if redo { "redoing…" } else { "undoing…" }.into();
                }
            }
            KeyAction::ClosePane => self.run_action("close-pane", tx),
            KeyAction::ModePicker => self.run_action("mode", tx),
            KeyAction::LayoutHorizontal => self.run_action("layout-horizontal", tx),
            KeyAction::LayoutTabby => self.run_action("layout-tabby", tx),
            KeyAction::LayoutFibonacci => self.run_action("layout-fibonacci", tx),
            KeyAction::LayoutCycle => self.run_action("layout-cycle", tx),
            KeyAction::CycleMode => self.toggle_mode(tx),
            KeyAction::CycleModeBack => self.cycle_mode(-1, tx),
            KeyAction::CycleReasoning => self.cycle_reasoning(tx),
            KeyAction::FocusTab(index) => self.focus_tab(index as usize - 1),
        }
    }

    pub(super) fn cancel(&mut self, tx: &UnboundedSender<Command>) {
        if let Some(view) = self.focused_view() {
            let session_id = view.session.id.clone();
            self.status = "cancelling…".into();
            let _ = tx.send(Command::Cancel(session_id));
        } else {
            self.status = "no session".into();
        }
    }
}
