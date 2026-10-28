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
        if picker.kind != PickerKind::Grep {
            return;
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
            self.status = "closed".into();
        }
    }

    pub(super) fn open_text_view(&mut self, title: impl Into<String>, text: &str) {
        self.text_view = Some(TextView::new(title, text));
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
                    inbox.cursor = (cursor + 1).min(len.saturating_sub(1));
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if let Some(inbox) = self.inbox.as_mut() {
                    inbox.cursor = cursor.saturating_sub(1);
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
                if view.cursor + 1 < len {
                    view.cursor += 1;
                }
                if let Some(process) = view.selected() {
                    let process_id = process.id.clone();
                    let _ = tx.send(Command::ProcessOutput { process_id });
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                view.cursor = view.cursor.saturating_sub(1);
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
            }
            KeyCode::Up => self.move_picker(-1),
            KeyCode::Down => self.move_picker(1),
            KeyCode::Enter => self.picker_enter(kind, tx),
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
        if kind != PickerKind::Grep {
            return;
        }
        self.picker_query_due = true;
        if let Some(picker) = &mut self.picker {
            picker.clear_items();
            picker.selected = 0;
            picker.loading = !picker.query.trim().is_empty();
        }
    }

    pub(super) fn move_picker(&mut self, delta: i32) {
        let len = self.picker_filtered().len();
        if let Some(picker) = &mut self.picker {
            if len == 0 {
                picker.selected = 0;
            } else {
                let current = picker.selected.min(len - 1) as i32;
                picker.selected = (current + delta).clamp(0, len as i32 - 1) as usize;
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

    /// Rows for the open picker: session indices or item indices. The result is
    /// cached until the query or the source rows change, because both the draw
    /// path and key handling ask for it more than once per keystroke.
    pub(super) fn picker_filtered(&mut self) -> Vec<usize> {
        let Some(kind) = self.picker.as_ref().map(|picker| picker.kind) else {
            return Vec::new();
        };
        let sessions_rev = if matches!(kind, PickerKind::Sessions | PickerKind::NewPane) {
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
            PickerKind::Sessions | PickerKind::NewPane => self.filtered_sessions(),
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
                let haystack = format!(
                    "{} {} {}",
                    item.label,
                    item.value,
                    item.detail.as_deref().unwrap_or_default()
                )
                .to_lowercase();
                fuzzy_score(&query, &haystack).map(|score| (score, index))
            })
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        hits.into_iter().map(|(_, index)| index).collect()
    }

    pub(super) fn picker_enter(&mut self, kind: PickerKind, tx: &UnboundedSender<Command>) {
        let filtered = self.picker_filtered();
        let Some(selected) = self.picker.as_ref().map(|picker| picker.selected) else {
            return;
        };
        let Some(index) = filtered.get(selected).copied() else {
            return;
        };
        let value = self
            .picker
            .as_ref()
            .and_then(|picker| picker.items.get(index))
            .map(|item| item.value.clone());
        match kind {
            PickerKind::Sessions => {
                self.picker = None;
                self.select_session(index, tx);
            }
            PickerKind::NewPane => {
                self.picker = None;
                self.open_in_new_pane(index, tx);
            }
            PickerKind::Files | PickerKind::Grep => {
                if let Some(value) = value {
                    self.picker = None;
                    self.insert_prompt(&format!("@{value}"));
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
                    self.open_session_id_in_new_pane(child, tx);
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
                    let cwd = self
                        .focused_view()
                        .map(|view| view.session.cwd.clone())
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                    if let Some(skill) = eix_core::find_skill(&cwd, &value, &self.compat) {
                        self.picker = None;
                        self.input.set_text(&skill.body);
                        self.focus = Focus::Input;
                        self.status = format!("inserted skill {}", skill.name);
                    }
                }
            }
            PickerKind::Themes => {
                if let Some(value) = value {
                    self.apply_theme_picker(&value);
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
        let mut picker = Picker {
            kind,
            query: String::new(),
            selected: 0,
            items: Vec::new(),
            items_rev: 0,
            filter: PickerFilterCache::default(),
            scroll: 0,
            loading: false,
            preview: PickerPreview::default(),
            preview_request: None,
        };
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
        } else if kind == PickerKind::Timeline {
            let Some(view) = self.focused_view() else {
                self.status = "attach a session first".into();
                return;
            };
            picker.set_items(timeline_picker_items(&view.events));
            self.status = format!("timeline: {} user message(s)", picker.items.len());
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
            "sessions" => self.open_picker(PickerKind::Sessions, tx),
            "new-pane" => self.open_picker(PickerKind::NewPane, tx),
            "files" => self.open_picker(PickerKind::Files, tx),
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
            "new" => self.new_session(tx, None),
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
