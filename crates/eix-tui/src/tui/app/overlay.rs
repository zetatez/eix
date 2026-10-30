//! Picker, inbox, processes and other transient overlays.

use super::*;

impl App {
    /// Send the latest content-search query. The 200 ms tick debounces fast
    /// typing without adding another timer to the event loop.
    pub(super) fn flush_picker_query(&mut self, tx: &UnboundedSender<Command>) {
        if !std::mem::take(&mut self.picker_query_due) {
            return;
        }
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        match picker.kind {
            PickerKind::Grep => {}
            PickerKind::Plugin => {
                let Some(provider) = picker.provider.clone() else {
                    return;
                };
                let query = picker.query.trim().to_string();
                if let Some(picker) = &mut self.picker {
                    picker.loading = true;
                }
                self.status = format!("{}: loading…", provider.title);
                let _ = tx.send(Command::PluginPick {
                    plugin: provider.plugin,
                    id: provider.id,
                    query,
                });
                return;
            }
            _ => return,
        }
        let query = picker.query.trim().to_string();
        if query.is_empty() {
            if let Some(picker) = &mut self.picker {
                picker.clear_items();
                picker.selected = 0;
                picker.loading = false;
            }
            self.status = "grep: type to search".into();
            return;
        }
        let Some(session_id) = self.focused_view().map(|view| view.session.id.clone()) else {
            self.status = "attach a session first".into();
            return;
        };
        if let Some(picker) = &mut self.picker {
            picker.loading = true;
            picker.selected = 0;
        }
        self.status = "grep: searching…".into();
        let _ = tx.send(Command::PickerData {
            session_id,
            kind: eix_core::KIND_GREP.to_string(),
            query: Some(query),
        });
    }

    /// `/subagents` without a value: show the live child picker when the
    /// focused session has children, otherwise fall back to the parallel-limit
    /// picker so the old entry point still works.
    pub(super) fn open_subagents(&mut self, tx: &UnboundedSender<Command>) {
        let has_children = self
            .focused_view()
            .is_some_and(|view| !view.subagents.is_empty());
        if has_children {
            self.open_picker(PickerKind::SubagentActivity, tx);
        } else {
            self.open_picker(PickerKind::Subagents, tx);
        }
    }

    /// Scroll or dismiss a read-only text overlay.
    pub(super) fn handle_text_view_key(&mut self, key: KeyEvent) {
        // C-j/C-k/C-n/C-p scroll like j/k so every list-like surface uses the
        // same next/previous gestures.
        let key = next_prev_key(key);
        let mut close = false;
        if let Some(view) = self.text_view.as_mut() {
            let last = view.lines.len().saturating_sub(1);
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => close = true,
                KeyCode::Char('j') | KeyCode::Down => {
                    view.scroll = view.scroll.saturating_add(1).min(last);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    view.scroll = view.scroll.saturating_sub(1);
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    view.scroll = view.scroll.saturating_add(12).min(last);
                }
                KeyCode::PageUp => {
                    view.scroll = view.scroll.saturating_sub(12);
                }
                KeyCode::Char('g') | KeyCode::Home => view.scroll = 0,
                KeyCode::Char('G') | KeyCode::End => view.scroll = usize::MAX,
                _ => {}
            }
        }
        if close {
            self.text_view = None;
            self.plugin_panel = None;
            self.status = "closed".into();
        }
    }

    pub(super) fn open_text_view(&mut self, title: impl Into<String>, text: &str) {
        // Any other overlay replaces whatever plugin surface was open, so
        // `/plugins act` must not keep targeting a hidden panel.
        self.plugin_panel = None;
        self.text_view = Some(TextView::new(title, text));
    }

    /// `/doctor` uses the same scrollable overlay with section styling.
    pub(super) fn open_doctor_view(&mut self, lines: &[String]) {
        self.plugin_panel = None;
        let report = crate::tui::doctor::DoctorReport::parse(lines);
        let mut view = TextView::new("doctor", &report.plain());
        view.doctor = Some(report);
        self.text_view = Some(view);
    }

    pub(super) fn open_processes(&mut self, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_view().map(|view| view.session.id.clone()) else {
            self.status = "no session to inspect".into();
            return;
        };
        self.process_view = Some(ProcessView::new(session_id.clone()));
        self.status = "processes: loading…".into();
        let _ = tx.send(Command::ProcessList { session_id });
    }

    /// Open the cross-session approvals inbox from the approvals the TUI has
    /// already seen; answering updates the per-session cards as usual.
    pub(super) fn open_inbox(&mut self) {
        let mut items: Vec<InboxItem> = self
            .approvals
            .iter()
            .map(|(session_id, pending)| {
                let session_name = self
                    .sessions
                    .iter()
                    .find(|session| &session.id == session_id)
                    .map(|session| session.name.clone())
                    .unwrap_or_else(|| session_id.chars().take(8).collect());
                InboxItem {
                    session_id: session_id.clone(),
                    session_name,
                    tool: pending.tool.clone(),
                    summary: pending.summary.clone(),
                    kind: pending.kind,
                }
            })
            .collect();
        items.sort_by(|a, b| a.session_name.cmp(&b.session_name));
        if items.is_empty() {
            self.status = "no pending approvals".into();
            return;
        }
        self.inbox = Some(InboxView { items, cursor: 0 });
        self.status = "approvals · Enter opens · y/n answer · Esc closes".into();
    }

    pub(super) fn handle_inbox_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        let key = next_prev_key(key);
        let Some(inbox) = self.inbox.as_ref() else {
            return;
        };
        let len = inbox.items.len();
        let cursor = inbox.cursor;
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('A') => {
                self.inbox = None;
                self.status = "approvals closed".into();
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if let Some(inbox) = self.inbox.as_mut() {
                    inbox.cursor = wrap_step(cursor, len, 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(inbox) = self.inbox.as_mut() {
                    inbox.cursor = wrap_step(cursor, len, -1);
                }
            }
            KeyCode::Char('b') | KeyCode::Char('B')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if let Some(inbox) = self.inbox.as_mut() {
                    inbox.cursor = cursor.saturating_sub(10);
                }
            }
            KeyCode::Char('f') | KeyCode::Char('F')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if let Some(inbox) = self.inbox.as_mut() {
                    inbox.cursor = (cursor + 10).min(len.saturating_sub(1));
                }
            }
            KeyCode::Char('y') | KeyCode::Char('n') => {
                let Some(item) = self
                    .inbox
                    .as_ref()
                    .and_then(|inbox| inbox.items.get(cursor))
                else {
                    return;
                };
                let session_id = item.session_id.clone();
                let decision = if key.code == KeyCode::Char('y') {
                    ApprovalDecision::Approve
                } else {
                    ApprovalDecision::Deny
                };
                self.respond_approval_for(&session_id, decision, None, None, tx);
            }
            KeyCode::Enter => {
                let session_id = self
                    .inbox
                    .as_ref()
                    .and_then(|inbox| inbox.items.get(cursor))
                    .map(|item| item.session_id.clone());
                self.inbox = None;
                let Some(session_id) = session_id else {
                    return;
                };
                if let Some(index) = self
                    .sessions
                    .iter()
                    .position(|session| session.id == session_id)
                {
                    self.select_session(index, tx);
                } else {
                    self.status = "that session is no longer listed".into();
                }
            }
            _ => {}
        }
    }

    pub(super) fn handle_process_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.process_view = None;
            self.status = "processes closed".into();
            return;
        }
        let key = next_prev_key(key);
        let Some(view) = &mut self.process_view else {
            return;
        };
        let len = view.processes.len();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                view.cursor = wrap_step(view.cursor, len, 1);
                if let Some(process) = view.selected() {
                    let process_id = process.id.clone();
                    let _ = tx.send(Command::ProcessOutput { process_id });
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                view.cursor = wrap_step(view.cursor, len, -1);
                if let Some(process) = view.selected() {
                    let process_id = process.id.clone();
                    let _ = tx.send(Command::ProcessOutput { process_id });
                }
            }
            KeyCode::Char('g') => view.scroll = usize::MAX / 2,
            KeyCode::Char('G') => view.scroll = 0,
            KeyCode::PageUp => view.scroll = view.scroll.saturating_add(20),
            KeyCode::PageDown => view.scroll = view.scroll.saturating_sub(20),
            KeyCode::Char('b') | KeyCode::Char('B')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                view.scroll = view.scroll.saturating_add(20);
            }
            KeyCode::Char('f') | KeyCode::Char('F')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                view.scroll = view.scroll.saturating_sub(20);
            }
            KeyCode::Char('r') => {
                let session_id = view.session_id.clone();
                let _ = tx.send(Command::ProcessList { session_id });
                if let Some(process) = view.selected() {
                    let process_id = process.id.clone();
                    let _ = tx.send(Command::ProcessOutput { process_id });
                }
            }
            KeyCode::Enter => {
                if let Some(process) = view.selected() {
                    let process_id = process.id.clone();
                    let _ = tx.send(Command::ProcessOutput { process_id });
                }
            }
            KeyCode::Char('x') => {
                if let Some(process) = view.selected() {
                    if !process.status.is_running() {
                        self.status = "process is not running".into();
                        return;
                    }
                    let process_id = process.id.clone();
                    let _ = tx.send(Command::ProcessKill { process_id });
                }
            }
            _ => {}
        }
    }

    pub(super) fn handle_picker_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        let Some(kind) = self.picker.as_ref().map(|picker| picker.kind) else {
            return;
        };
        // Ctrl-J / Ctrl-K / Ctrl-N / Ctrl-P are the vim-friendly next/previous
        // bindings; j/k still type into the query.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('j') | KeyCode::Char('n') => {
                    self.move_picker(1);
                    return;
                }
                KeyCode::Char('k') | KeyCode::Char('p') => {
                    self.move_picker(-1);
                    return;
                }
                KeyCode::Char('b') | KeyCode::Char('B') => {
                    self.move_picker(-10);
                    return;
                }
                KeyCode::Char('f') | KeyCode::Char('F') => {
                    self.move_picker(10);
                    return;
                }
                // Terminals without kitty keyboard disambiguation report
                // Ctrl-J as Ctrl-Enter, so treat it as "next" too.
                KeyCode::Enter => {
                    self.move_picker(1);
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Esc => {
                if kind == PickerKind::Themes {
                    self.cancel_theme_picker();
                } else {
                    self.picker = None;
                }
                self.pending_rewind = None;
            }
            KeyCode::Up => self.move_picker(-1),
            KeyCode::Down => self.move_picker(1),
            KeyCode::Enter => self.picker_enter(kind, tx),
            KeyCode::Char(' ')
                if kind == PickerKind::Mcp
                    && !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                // Space flips the highlighted server; typing still filters.
                if let Some(server) = self.selected_mcp_server() {
                    let enabled = !server.enabled;
                    let name = server.name;
                    let _ = tx.send(Command::SetMcpEnabled {
                        name: name.clone(),
                        enabled,
                    });
                    self.status = format!(
                        "mcp: {name} {}",
                        if enabled {
                            "enabling…"
                        } else {
                            "disabling…"
                        }
                    );
                }
            }
            KeyCode::Backspace => {
                if let Some(picker) = &mut self.picker {
                    picker.query.pop();
                    picker.selected = 0;
                }
                self.note_picker_query_change(kind);
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(picker) = &mut self.picker {
                    picker.query.push(c);
                    picker.selected = 0;
                }
                self.note_picker_query_change(kind);
            }
            _ => {}
        }
        self.preview_theme_selection();
    }

    /// Content search is provider-backed: clear the old rows now and let the
    /// next tick coalesce rapid keystrokes into one daemon request.
    pub(super) fn note_picker_query_change(&mut self, kind: PickerKind) {
        if kind != PickerKind::Grep && kind != PickerKind::Plugin {
            return;
        }
        self.picker_query_due = true;
        if let Some(picker) = &mut self.picker {
            picker.clear_items();
            picker.selected = 0;
            picker.loading = kind == PickerKind::Plugin || !picker.query.trim().is_empty();
        }
    }

    pub(super) fn move_picker(&mut self, delta: i32) {
        let filtered = self.picker_filtered();
        let len = filtered.len();
        // Command-palette category headers are visual separators: skip them
        // when stepping or paging.
        let selectable: Vec<bool> = match &self.picker {
            Some(picker) => filtered
                .iter()
                .map(|&index| !picker.items.get(index).is_some_and(is_command_header))
                .collect(),
            None => Vec::new(),
        };
        if let Some(picker) = &mut self.picker {
            if len == 0 {
                picker.selected = 0;
            } else {
                let current = picker.selected.min(len - 1);
                let step = if delta.unsigned_abs() == 1 {
                    // Step navigation cycles; page jumps still stop at the ends.
                    let target =
                        (current as isize + delta.signum() as isize).rem_euclid(len as isize);
                    selectable_after(&selectable, target, delta.signum() as isize)
                } else {
                    let target = (current as i32 + delta).clamp(0, len as i32 - 1) as isize;
                    selectable_after(&selectable, target, delta.signum() as isize)
                };
                picker.selected = step;
            }
        }
        self.preview_theme_selection();
    }

    /// Recolor the TUI for the theme under the picker cursor. The original
    /// palette is restored by Esc, so browsing never leaves a half-applied
    /// theme behind.
    pub(super) fn preview_theme_selection(&mut self) {
        if self.picker.as_ref().map(|picker| picker.kind) != Some(PickerKind::Themes) {
            return;
        }
        let Some((config, original)) = self.theme_picker_backup.clone() else {
            return;
        };
        let selected = self
            .picker
            .as_ref()
            .map(|picker| picker.selected)
            .unwrap_or(0);
        let value = self
            .picker_filtered()
            .get(selected)
            .copied()
            .and_then(|index| {
                self.picker
                    .as_ref()
                    .and_then(|picker| picker.items.get(index))
                    .map(|item| item.value.clone())
            });
        let Some(value) = value else {
            self.theme = original;
            return;
        };
        let mut config = config;
        config.preset = (value != THEME_DEFAULT_VALUE).then_some(value);
        self.theme = Theme::from_config(&config);
    }

    pub(super) fn cancel_theme_picker(&mut self) {
        if let Some((config, theme)) = self.theme_picker_backup.take() {
            self.theme_config = config;
            self.theme = theme;
            self.status = "colorscheme unchanged".into();
        }
        self.picker = None;
    }

    pub(super) fn apply_theme_picker(&mut self, value: &str) {
        self.theme_picker_backup = None;
        self.theme_config.preset = (value != THEME_DEFAULT_VALUE).then(|| value.to_string());
        self.theme = Theme::from_config(&self.theme_config);
        self.status = if value == THEME_DEFAULT_VALUE {
            "colorscheme → default".into()
        } else if let Some(preset) = theme_preset(value) {
            format!("colorscheme → {} ({})", preset.name, preset.description)
        } else {
            format!("colorscheme → {value}")
        };
        self.picker = None;
    }

    /// Preset to persist for the next launch: the applied colorscheme, or the
    /// "default" sentinel when the user cleared it.
    pub(crate) fn theme_pref(&self) -> String {
        self.theme_config
            .preset
            .clone()
            .unwrap_or_else(|| THEME_DEFAULT_VALUE.to_string())
    }

    /// Rows for the open picker: session indices or item indices. The result is
    /// cached until the query or the source rows change, because both the draw
    /// path and key handling ask for it more than once per keystroke.
    pub(super) fn picker_filtered(&mut self) -> Vec<usize> {
        let Some(kind) = self.picker.as_ref().map(|picker| picker.kind) else {
            return Vec::new();
        };
        let sessions_rev = if kind == PickerKind::Sessions {
            self.sessions_fingerprint()
        } else {
            0
        };
        let subagents_rev = if kind == PickerKind::SubagentActivity {
            self.subagents_fingerprint()
        } else {
            0
        };
        {
            let Some(picker) = self.picker.as_ref() else {
                return Vec::new();
            };
            let filter = &picker.filter;
            if filter.valid
                && filter.kind == Some(kind)
                && filter.query == picker.query
                && filter.items_rev == picker.items_rev
                && filter.sessions_rev == sessions_rev
                && filter.subagents_rev == subagents_rev
            {
                return filter.rows.clone();
            }
        }
        let rows = match kind {
            PickerKind::Sessions => self.filtered_sessions(),
            PickerKind::SubagentActivity => self.filtered_subagents(),
            _ => self.filtered_items(),
        };
        if let Some(picker) = self.picker.as_mut() {
            picker.filter = PickerFilterCache {
                kind: Some(kind),
                query: picker.query.clone(),
                items_rev: picker.items_rev,
                sessions_rev,
                subagents_rev,
                rows: rows.clone(),
                valid: true,
            };
        }
        rows
    }

    /// Fingerprint of the focused session's child agents. `subagents_rev`
    /// covers event and delta updates; the session id separates two views that
    /// happen to share the same revision counter.
    fn subagents_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        if let Some(view) = self.focused_view() {
            view.session.id.hash(&mut hasher);
            view.subagents_rev.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Fuzzy-filter the focused session's child agents in stable name order.
    fn filtered_subagents(&self) -> Vec<usize> {
        let Some(view) = self.focused_view() else {
            return Vec::new();
        };
        let agents = view.subagents_sorted();
        let query = self
            .picker
            .as_ref()
            .map(|picker| picker.query.trim().to_lowercase())
            .unwrap_or_default();
        if query.is_empty() {
            return (0..agents.len()).collect();
        }
        let mut hits: Vec<(i32, usize)> = agents
            .iter()
            .enumerate()
            .filter_map(|(index, (id, agent))| {
                let status = match agent.status {
                    transcript::SubagentStatus::Running => "running",
                    transcript::SubagentStatus::Completed => "done",
                    transcript::SubagentStatus::Failed => "failed",
                };
                let haystack =
                    format!("{} {status} {} {}", agent.name, agent.call_id, id).to_lowercase();
                fuzzy_score(&query, &haystack).map(|score| (score, index))
            })
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        hits.into_iter().map(|(_, index)| index).collect()
    }

    /// Cheap fingerprint of the fields the session filter reads. Session rows
    /// also show live status, but that does not change the matched order.
    fn sessions_fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for session in self.sessions.iter().filter(|session| !session.is_agent()) {
            session.id.hash(&mut hasher);
            session.name.hash(&mut hasher);
            session.cwd.hash(&mut hasher);
            // Scoped project pickers order by `updated_at`; a live session
            // update must invalidate the cached row order.
            session.updated_at.hash(&mut hasher);
        }
        hasher.finish()
    }

    pub(super) fn filtered_items(&self) -> Vec<usize> {
        let Some(picker) = &self.picker else {
            return Vec::new();
        };
        // Grep rows are already filtered and ordered by the daemon. Running
        // the fuzzy matcher again would reorder matches and drop rows because
        // the query usually matches the content detail, not the path:line
        // label.
        if picker.kind == PickerKind::Grep {
            return (0..picker.items.len()).collect();
        }
        let query = picker.query.trim().to_lowercase();
        if query.is_empty() {
            return (0..picker.items.len()).collect();
        }
        let mut hits: Vec<(i32, usize)> = picker
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                if picker.kind == PickerKind::Commands && is_command_header(item) {
                    return None;
                }
                let mut haystack = format!(
                    "{} {} {}",
                    item.label,
                    item.value,
                    item.detail.as_deref().unwrap_or_default()
                )
                .to_lowercase();
                // Project rows are paths; let a session name find its project
                // even when the directory is the only thing shown.
                if picker.kind == PickerKind::Projects {
                    for session in self
                        .sessions
                        .iter()
                        .filter(|session| !session.is_agent())
                        .filter(|session| session.cwd.to_string_lossy() == item.value)
                    {
                        haystack.push(' ');
                        haystack.push_str(&session.name.to_lowercase());
                    }
                }
                fuzzy_score(&query, &haystack).map(|score| (score, index))
            })
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        hits.into_iter().map(|(_, index)| index).collect()
    }

    /// `value` of the row under the cursor in the current filtered order.
    pub(super) fn selected_picker_value(&self) -> Option<String> {
        let filtered = self.filtered_items();
        let selected = self.picker.as_ref()?.selected;
        let index = *filtered.get(selected)?;
        self.picker
            .as_ref()?
            .items
            .get(index)
            .filter(|item| !is_command_header(item))
            .map(|item| item.value.clone())
    }

    /// MCP server row under the picker cursor, if the MCP picker is open.
    fn selected_mcp_server(&mut self) -> Option<eix_core::McpServerInfo> {
        if self.picker.as_ref().map(|picker| picker.kind) != Some(PickerKind::Mcp) {
            return None;
        }
        let filtered = self.picker_filtered();
        let selected = self.picker.as_ref()?.selected;
        let index = *filtered.get(selected)?;
        let name = self.picker.as_ref()?.items.get(index)?.value.clone();
        self.mcp_servers
            .iter()
            .find(|server| server.name == name)
            .cloned()
    }

    pub(super) fn picker_enter(&mut self, kind: PickerKind, tx: &UnboundedSender<Command>) {
        let filtered = self.picker_filtered();
        let Some(selected) = self.picker.as_ref().map(|picker| picker.selected) else {
            return;
        };
        let Some(index) = filtered.get(selected).copied() else {
            return;
        };
        let item = self
            .picker
            .as_ref()
            .and_then(|picker| picker.items.get(index));
        if item.is_some_and(is_command_header) {
            return;
        }
        let value = item.map(|item| item.value.clone());
        match kind {
            PickerKind::Plugin => {
                let provider = self
                    .picker
                    .as_ref()
                    .and_then(|picker| picker.provider.clone());
                self.picker = None;
                if let (Some(provider), Some(value)) = (provider, value) {
                    let _ = tx.send(Command::PluginUiAction {
                        plugin: provider.plugin,
                        id: provider.id,
                        action: "picked".into(),
                        values: serde_json::json!({ "item": value }),
                    });
                    self.status = "plugin picker: selection sent".into();
                }
            }
            PickerKind::Sessions => {
                self.picker = None;
                self.open_or_focus_session(index, tx);
            }
            PickerKind::Projects => {
                let Some(value) = value else {
                    return;
                };
                let Some(group) = self
                    .project_groups()
                    .into_iter()
                    .find(|group| group.cwd.to_string_lossy() == value)
                else {
                    self.picker = None;
                    return;
                };
                self.picker = None;
                let count = group.sessions.len();
                let cwd = group.cwd;
                // Drill into the project's own sessions, newest first, so the
                // user can search and pick one instead of being forced onto
                // the most recent session.
                self.open_picker(PickerKind::Sessions, tx);
                if let Some(picker) = self.picker.as_mut() {
                    picker.scope = Some(cwd.clone());
                    picker.selected = 0;
                }
                self.status = format!(
                    "project · {} · {count} session{}",
                    cwd.display(),
                    if count == 1 { "" } else { "s" }
                );
            }
            PickerKind::Files | PickerKind::Grep => {
                if let Some(value) = value {
                    self.picker = None;
                    self.insert_prompt(&format!("@{value}"));
                }
            }
            PickerKind::Images => {
                if let Some(value) = value {
                    self.picker = None;
                    self.insert_prompt(&format!("@{value}"));
                    if !self.pending_images.contains(&value) {
                        self.pending_images.push(value.clone());
                    }
                    self.status = format!(
                        "attached {value} · {} image(s) for the next prompt",
                        self.pending_images.len()
                    );
                }
            }
            PickerKind::Git => {
                if let Some(value) = value {
                    self.picker = None;
                    self.insert_prompt(&value);
                }
            }
            PickerKind::Commands => {
                if let Some(value) = value {
                    self.picker = None;
                    let line = self
                        .commands
                        .iter()
                        .find(|spec| spec.name == value)
                        .map(|spec| {
                            if spec.args.is_empty() {
                                format!("/{}", spec.name)
                            } else {
                                format!("/{} ", spec.name)
                            }
                        })
                        .unwrap_or_else(|| format!("/{value}"));
                    // The palette is an overlay over a possibly non-empty
                    // draft, so run the command without clearing the prompt.
                    self.handle_command_text(&line, tx, false);
                }
            }
            PickerKind::Models => {
                if let Some(value) = value {
                    self.picker = None;
                    if let Some(session_id) = self.focused_session_id() {
                        let model = (value != "@default").then_some(value);
                        let _ = tx.send(Command::SetModel { session_id, model });
                        self.status = "switching model…".into();
                    }
                }
            }
            PickerKind::PlanModels => {
                if let Some(value) = value {
                    self.picker = None;
                    if let Some(session_id) = self.focused_session_id() {
                        let model = (value != "@default").then_some(value);
                        let _ = tx.send(Command::SetPlanModel { session_id, model });
                        self.status = "switching Plan model…".into();
                    }
                }
            }
            PickerKind::Effort => {
                if let Some(value) = value {
                    self.picker = None;
                    if self.set_reasoning(&value, tx) {
                        self.status = format!("effort: {value}");
                    }
                }
            }
            PickerKind::Agents => {
                if let Some(value) = value {
                    self.picker = None;
                    if let Some(session_id) = self.focused_session_id() {
                        let agent = (value != "@default").then_some(value);
                        let _ = tx.send(Command::SetAgent { session_id, agent });
                        self.status = "switching agent…".into();
                    }
                }
            }
            PickerKind::Subagents => {
                self.picker = None;
                if let Some(value) = value {
                    match value.parse::<usize>() {
                        Ok(value) => self.set_subagent_concurrency(value, tx),
                        Err(_) => {
                            self.error = Some("invalid subagent concurrency value".into());
                        }
                    }
                }
            }
            PickerKind::SubagentActivity => {
                let child = self.focused_view().and_then(|view| {
                    view.subagents_sorted()
                        .get(index)
                        .map(|(id, _)| (*id).to_string())
                });
                self.picker = None;
                if let Some(child) = child {
                    self.open_or_focus_session_id(child, tx);
                }
            }
            PickerKind::Modes => {
                if let Some(value) = value {
                    self.picker = None;
                    self.command_mode(&value, tx);
                }
            }
            PickerKind::Timeline => {
                if let Some(value) = value {
                    self.picker = None;
                    if let Ok(seq) = value.parse::<u64>() {
                        self.jump_to_user_seq(seq);
                    }
                }
            }
            PickerKind::Skills => {
                if let Some(value) = value {
                    // Insert a mention like the file picker; the model resolves
                    // the skill through `available_skills` / `use_skill`.
                    self.picker = None;
                    self.input.insert_text(&format!("@{value}"));
                    self.focus = Focus::Input;
                    self.status = format!("inserted @{value}");
                }
            }
            PickerKind::Themes => {
                if let Some(value) = value {
                    self.apply_theme_picker(&value);
                }
            }
            PickerKind::Rewind => {
                if let Some(value) = value {
                    match value.parse::<u64>() {
                        Ok(seq) => {
                            self.picker = None;
                            self.pending_rewind = Some(seq);
                            self.open_picker(PickerKind::RewindScope, tx);
                        }
                        Err(_) => self.error = Some("invalid checkpoint".into()),
                    }
                }
            }
            PickerKind::Fork => {
                let Some(value) = value else {
                    return;
                };
                self.picker = None;
                if value == "@all" {
                    self.fork_focused(tx, None);
                    return;
                }
                match value.parse::<u64>() {
                    Ok(seq) => self.fork_focused(tx, Some(seq)),
                    Err(_) => self.error = Some("invalid fork point".into()),
                }
            }
            PickerKind::RewindScope => {
                let at_seq = self.pending_rewind.take();
                self.picker = None;
                if let Some(at_seq) = at_seq {
                    let scope = match value.as_deref() {
                        Some("chat") => Some(eix_core::RewindScope::Conversation),
                        Some("files") => Some(eix_core::RewindScope::Files),
                        Some("both") | None => Some(eix_core::RewindScope::Both),
                        Some(other) => {
                            self.error = Some(format!("unknown rewind scope {other:?}"));
                            None
                        }
                    };
                    if let (Some(scope), Some(session_id)) = (scope, self.focused_session_id()) {
                        let _ = tx.send(Command::Rewind {
                            session_id,
                            at_seq,
                            scope,
                        });
                        self.status = format!("rewinding to e{at_seq} · {}", scope.label());
                    }
                }
            }
            PickerKind::Diff => {
                if let Some(value) = value {
                    let preview = self
                        .picker
                        .as_ref()
                        .and_then(|picker| picker.items.get(index))
                        .and_then(|item| item.preview.clone())
                        .unwrap_or_default();
                    self.picker = None;
                    self.open_text_view(format!("diff · {value}"), &preview);
                    self.status = "diff · j/k scroll · Esc close".into();
                }
            }
            PickerKind::Mcp | PickerKind::Todos => {
                self.picker = None;
            }
        }
    }

    pub(super) fn open_picker(&mut self, kind: PickerKind, tx: &UnboundedSender<Command>) {
        // A picker opening clears any half-finished double-Esc gesture.
        self.esc_at = None;
        self.picker_query_due = false;
        // Switching from one picker to another must not keep a previewed theme.
        if let Some((config, theme)) = self.theme_picker_backup.take() {
            self.theme_config = config;
            self.theme = theme;
        }
        let mut picker = Picker::new(kind);
        if kind == PickerKind::Grep {
            if self.focused_view().is_none() {
                self.status = "attach a session first".into();
                return;
            }
            self.status = "grep: type to search".into();
            self.picker = Some(picker);
            return;
        }
        if let Some(provider) = kind.data_kind() {
            let Some(session_id) = self.focused_view().map(|view| view.session.id.clone()) else {
                self.status = "attach a session first".into();
                return;
            };
            picker.loading = true;
            self.status = format!("{provider}: loading…");
            let _ = tx.send(Command::PickerData {
                session_id,
                kind: provider.to_string(),
                query: None,
            });
        } else if kind == PickerKind::Commands {
            picker.set_items(command_items(&self.commands));
            picker.selected = picker
                .items
                .iter()
                .position(|item| !is_command_header(item))
                .unwrap_or(0);
        } else if kind == PickerKind::Models {
            picker.loading = true;
            self.status = "model: loading provider catalogs…".into();
            let _ = tx.send(Command::ListModels);
        } else if kind == PickerKind::PlanModels {
            picker.loading = true;
            self.status = "plan model: loading provider catalogs…".into();
            let _ = tx.send(Command::ListModels);
        } else if kind == PickerKind::Agents {
            let Some(session_id) = self.focused_session_id() else {
                self.status = "attach a session first".into();
                return;
            };
            picker.loading = true;
            self.status = "agents: loading…".into();
            let _ = tx.send(Command::ListAgents { session_id });
        } else if kind == PickerKind::Subagents {
            let current = self.subagent_concurrency;
            picker.set_items(subagent_picker_items(current));
            picker.selected = picker
                .items
                .iter()
                .position(|item| item.value == current.to_string())
                .unwrap_or(0);
            self.status = format!("subagents: {current} concurrent (runtime)");
        } else if kind == PickerKind::SubagentActivity {
            let Some(view) = self.focused_view() else {
                self.status = "attach a session first".into();
                return;
            };
            let running = view
                .subagents
                .values()
                .filter(|agent| agent.status == transcript::SubagentStatus::Running)
                .count();
            self.status = format!(
                "subagents: {running} running / {} total · Enter opens a child pane",
                view.subagents.len()
            );
        } else if kind == PickerKind::Modes {
            let Some(current) = self.focused_view().map(|view| view.session.mode) else {
                self.status = "attach a session first".into();
                return;
            };
            picker.set_items(mode_picker_items(current));
            picker.selected = picker
                .items
                .iter()
                .position(|item| item.value == current.as_str())
                .unwrap_or(0);
            self.status = format!(
                "mode: {}{}",
                current.label(),
                if current.is_read_only() {
                    " (read-only)"
                } else {
                    ""
                }
            );
        } else if kind == PickerKind::Effort {
            let (_, current) = split_model_variant(self.model());
            let current = current.unwrap_or("medium");
            picker.set_items(effort_picker_items());
            picker.selected = picker
                .items
                .iter()
                .position(|item| item.value == current)
                .unwrap_or(1);
            self.status = format!("effort: {current}");
        } else if kind == PickerKind::Timeline {
            let Some(view) = self.focused_view() else {
                self.status = "attach a session first".into();
                return;
            };
            picker.set_items(timeline_picker_items(&view.events));
            self.status = format!("timeline: {} user message(s)", picker.items.len());
        } else if kind == PickerKind::Rewind {
            let Some(view) = self.focused_view() else {
                self.status = "attach a session first".into();
                return;
            };
            picker.set_items(timeline_picker_items(&view.events));
            self.status = format!("rewind: {} checkpoint(s)", picker.items.len());
        } else if kind == PickerKind::Fork {
            let Some(view) = self.focused_view() else {
                self.status = "attach a session first".into();
                return;
            };
            let mut items: Vec<_> = timeline_picker_items(&view.events)
                .into_iter()
                .map(|item| {
                    let label = format!("e{}  {}", item.value, item.label);
                    item.with_label(label)
                })
                .collect();
            if items.is_empty() {
                self.status = "fork: no user message to pick from".into();
                self.picker = None;
                return;
            }
            items.push(
                PickerItem::new("@all")
                    .with_label("entire conversation")
                    .with_detail("copy every message as it is now"),
            );
            picker.set_items(items);
            self.status = format!(
                "fork before a message · {} user message(s)",
                picker.items.len() - 1
            );
        } else if kind == PickerKind::RewindScope {
            picker.set_items(rewind_scope_picker_items());
            self.status = "rewind: choose what to restore".into();
        } else if kind == PickerKind::Skills {
            let cwd = self
                .focused_view()
                .map(|view| view.session.cwd.clone())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            picker.set_items(skill_picker_items(&eix_core::load_skills(
                &cwd,
                &self.compat,
            )));
            self.status = format!("{} skill(s)", picker.items.len());
        } else if kind == PickerKind::Mcp {
            picker.loading = true;
            self.status = "mcp: loading…".into();
            let _ = tx.send(Command::ListMcp);
        } else if kind == PickerKind::Diff {
            picker.loading = true;
            self.status = "diff: loading…".into();
        } else if kind == PickerKind::Todos {
            let Some(session_id) = self.focused_session_id() else {
                self.status = "attach a session first".into();
                return;
            };
            picker.loading = true;
            self.status = "todos: loading…".into();
            let _ = tx.send(Command::ListTodos { session_id });
        } else if kind == PickerKind::Themes {
            picker.set_items(theme_picker_items());
            let current = self.theme.preset.unwrap_or(THEME_DEFAULT_VALUE);
            picker.selected = picker
                .items
                .iter()
                .position(|item| item.value == current)
                .unwrap_or(0);
            self.theme_picker_backup = Some((self.theme_config.clone(), self.theme));
            self.status = "colorscheme preview · Enter apply · Esc cancel".into();
        } else if kind == PickerKind::Projects {
            picker.set_items(self.project_picker_items());
            let count = picker.items.len();
            self.status = if count == 0 {
                "projects: no sessions yet".into()
            } else {
                format!(
                    "projects: {count} working director{} · newest session first",
                    if count == 1 { "y" } else { "ies" }
                )
            };
        } else {
            // Subagent sessions are intentionally absent from the picker.
            // Resolve the focused session against the visible rows instead of
            // reusing its index in the full session list.
            let visible = self.visible_session_indices();
            picker.selected = self
                .focused_session_id()
                .and_then(|id| {
                    visible
                        .iter()
                        .position(|&index| self.sessions[index].id == id)
                })
                .or_else(|| visible.iter().position(|&index| index >= self.selected))
                .unwrap_or(0);
        }
        self.picker = Some(picker);
    }

    /// Open a picker backed by a plugin's `ui/pick` provider.
    pub(super) fn open_plugin_picker(
        &mut self,
        plugin: &str,
        id: &str,
        title: &str,
        tx: &UnboundedSender<Command>,
    ) {
        self.esc_at = None;
        self.picker_query_due = false;
        if let Some((config, theme)) = self.theme_picker_backup.take() {
            self.theme_config = config;
            self.theme = theme;
        }
        let title = if title.trim().is_empty() {
            id.to_string()
        } else {
            title.to_string()
        };
        self.picker = Some(Picker {
            kind: PickerKind::Plugin,
            query: String::new(),
            selected: 0,
            provider: Some(PluginPickerProvider {
                plugin: plugin.to_string(),
                id: id.to_string(),
                title: title.clone(),
            }),
            scope: None,
            items: Vec::new(),
            items_rev: 0,
            filter: PickerFilterCache::default(),
            scroll: 0,
            loading: true,
            preview: PickerPreview::default(),
            preview_request: None,
        });
        self.status = format!("{title}: loading…");
        let _ = tx.send(Command::PluginPick {
            plugin: plugin.to_string(),
            id: id.to_string(),
            query: String::new(),
        });
    }

    /// Open the content picker, optionally with a prefilled query from
    /// `/grep <query>`.
    pub(super) fn open_grep_picker(&mut self, query: &str, tx: &UnboundedSender<Command>) {
        self.open_picker(PickerKind::Grep, tx);
        let query = query.trim();
        if query.is_empty() {
            return;
        }
        let Some(picker) = self
            .picker
            .as_mut()
            .filter(|picker| picker.kind == PickerKind::Grep)
        else {
            return;
        };
        picker.query = query.to_string();
        picker.selected = 0;
        picker.loading = true;
        self.picker_query_due = true;
        self.status = "grep: searching…".into();
    }

    /// Run a command-palette entry or a prompt leader action (`<Space>xx`).
    /// Unknown ids are ignored.
    pub(super) fn run_action(&mut self, id: &str, tx: &UnboundedSender<Command>) {
        match id {
            "commands" | "palette" => self.open_picker(PickerKind::Commands, tx),
            "keys" => self.open_keys_help(""),
            "sessions" | "new" => self.open_picker(PickerKind::Sessions, tx),
            "fzf" => self.open_picker(PickerKind::Files, tx),
            "image" | "images" => self.open_picker(PickerKind::Images, tx),
            "grep" => self.open_grep_picker("", tx),
            "git" | "worktrees" | "branches" => self.open_picker(PickerKind::Git, tx),
            "model" => self.open_picker(PickerKind::Models, tx),
            "agents" => self.open_picker(PickerKind::Agents, tx),
            "subagents" => self.open_subagents(tx),
            "mode" | "perm" => self.open_picker(PickerKind::Modes, tx),
            "toggle-mode" => self.toggle_mode(tx),
            "skills" => self.open_picker(PickerKind::Skills, tx),
            "colorscheme" => self.open_picker(PickerKind::Themes, tx),
            "processes" => self.open_processes(tx),
            "approvals" => self.open_inbox(),
            "mcp" => self.open_picker(PickerKind::Mcp, tx),
            "todos" => self.open_picker(PickerKind::Todos, tx),
            "new-session" => self.new_session(tx),
            "new-worktree" => self.new_worktree(tx, None),
            "fork" => self.fork_focused(tx, None),
            "cancel" => self.cancel(tx),
            "attention" => self.focus_next_attention(),
            "input" => self.focus_input(),
            "close-pane" => {
                // Persist the prompt without switching panes; closing an
                // unsaved draft would otherwise make the cleanup check think
                // the session is unused.
                self.save_draft_now(tx);
                if let Some(session_id) = self.close_focused_pane() {
                    self.cleanup_created_session(&session_id, tx);
                }
            }
            "pane-prev" => self.cycle_pane(-1),
            "pane-next" => self.cycle_pane(1),
            "layout-horizontal" => self.set_layout(PaneLayout::Horizontal),
            "layout-tabby" => self.set_layout(PaneLayout::Tabby),
            "layout-fibonacci" => self.set_layout(PaneLayout::Fibonacci),
            "layout-cycle" => {
                let next = self.layout.next();
                self.set_layout(next);
            }
            "help" => self.open_help(),
            "quit" => {
                self.sync_draft(tx);
                self.should_quit = true;
            }
            _ => {}
        }
    }
}
