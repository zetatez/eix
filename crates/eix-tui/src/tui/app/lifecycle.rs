//! App construction, workspace restore, draft persistence, cleanup and one-shot take_* accessors.

use super::*;

impl App {
    pub fn new(initial_session: Option<String>) -> Self {
        let compat = eix_core::CompatConfig::default();
        // Opening the prompt is not an edit: start in normal mode and let the
        // user enter insert mode with `i`, `a`, etc.
        let mut input = Editor::new();
        input.enter_normal();
        Self {
            should_quit: false,
            exit_message: None,
            hello: None,
            subagent_concurrency: eix_core::DEFAULT_MAX_PARALLEL_SUBAGENTS,
            models: Vec::new(),
            mcp_servers: Vec::new(),
            sessions: Vec::new(),
            selected: 0,
            views: Vec::new(),
            focused: 0,
            layout: PaneLayout::Fibonacci,
            pending_attach: None,
            initial_session,
            restore_sessions: Vec::new(),
            restore_queue: Vec::new(),
            restore_focused: None,
            clear_active_on_quit: false,
            initialized: false,
            creating: false,
            submitting: false,
            pending_prompt: None,
            input,
            focus: Focus::Input,
            picker: None,
            picker_query_due: false,
            help: false,
            help_scope: None,
            help_query: String::new(),
            help_selected: 0,
            pending_rewind: None,
            created_sessions: HashSet::new(),
            pending_images: Vec::new(),
            inflight_images: Vec::new(),
            pending_leader: false,
            leader_status_backup: None,
            pending_clipboard: None,
            toasts: Vec::new(),
            last_seen_status: "connecting…".into(),
            pending_history: None,
            pane_areas: Vec::new(),
            tab_areas: Vec::new(),
            integrations: HashMap::new(),
            approvals: HashMap::new(),
            approval_rev: 0,
            diff_rows: None,
            card_diff: None,
            diff_body: None,
            inputs: HashMap::new(),
            diff_view: None,
            text_view: None,
            process_view: None,
            transcript_search: None,
            status: "connecting…".into(),
            error: None,
            connected: false,
            tick: 0,
            git_cwd: None,
            git_branch: None,
            git_checked: None,
            bell: false,
            default_model: None,
            default_agent: None,
            default_mode: None,
            ring: false,
            draft_loading: None,
            pending_draft: None,
            input_session: None,
            last_saved_draft: String::new(),
            reason_prompt: None,
            force_clear: false,
            inbox: None,
            theme: Theme::default(),
            theme_config: eix_core::ThemeConfig::default(),
            math_config: eix_core::MathConfig::default(),
            config_path: None,
            theme_picker_backup: None,
            keybinds: Keybinds::default(),
            esc_at: None,
            ctrl_w_at: None,
            history_index: None,
            history_draft: String::new(),
            history_entries: None,
            queues: HashMap::new(),
            pending_resumes: VecDeque::new(),
            resumed_sessions: HashSet::new(),
            queued_inflight: None,
            submitting_session: None,
            command_cursor: 0,
            cd_prefix_snapshot: None,
            // Custom commands are loaded once by `set_compat` at startup.
            commands: builtin_commands(),
            plugin_commands: Vec::new(),
            plugin_failed: Vec::new(),
            plugin_degraded: Vec::new(),
            plugin_panel: None,
            compat,
            command_dismissed: false,
            cmdline: None,
            cmdline_menu_armed: false,
            cmdline_menu_prefix: None,
            cmdline_menu_cursor: None,
            cmdline_history: Vec::new(),
            cmdline_history_index: None,
            cmdline_history_draft: String::new(),
            btw: None,
        }
    }

    /// Stage the pane state saved by the previous TUI run in this directory.
    /// The ids are resolved against the first session list, so stale entries
    /// are ignored instead of blocking startup. An explicit `--session`
    /// overrides the saved workspace, including its layout.
    pub fn restore_workspace(&mut self, active: ActiveWorkspace) {
        self.restore_sessions = active.sessions;
        self.restore_focused = active.focused;
        if self.initial_session.is_none() {
            self.layout = PaneLayout::from_workspace_layout(active.layout);
        }
    }

    /// Snapshot of the state to persist for this working directory.
    pub fn workspace_state(&self) -> ActiveWorkspace {
        ActiveWorkspace {
            sessions: self.workspace_sessions(),
            focused: self.workspace_focused(),
            layout: self.layout.workspace_layout(),
        }
    }

    /// Session ids currently open as panes, in pane order.
    pub fn workspace_sessions(&self) -> Vec<String> {
        self.views
            .iter()
            .map(|view| view.session.id.clone())
            .collect()
    }

    /// Session id of the focused pane, when one is open.
    pub fn workspace_focused(&self) -> Option<String> {
        self.focused_view().map(|view| view.session.id.clone())
    }

    /// Whether the exit path explicitly closed the panes (`:q` / `:qa`).
    pub fn should_clear_active_workspace(&self) -> bool {
        self.clear_active_on_quit
    }

    /// Re-read user-defined Markdown commands from the focused workspace.
    pub(super) fn reload_commands(&mut self) {
        let cwd = self
            .focused_view()
            .map(|view| view.session.cwd.clone())
            .unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
            });
        self.commands = merge_commands(&cwd, &self.compat, &self.plugin_commands);
    }

    /// Apply the daemon config's compatibility switches and load commands from
    /// all enabled harness sources. This is called once at startup.
    pub fn set_compat(&mut self, compat: eix_core::CompatConfig) {
        self.compat = compat;
        self.reload_commands();
    }

    /// Replace the plugin command set returned by `plugin.commands`. Plugin
    /// command names are namespaced (`plugin/<package>/<name>`), so they can
    /// never shadow a built-in or user command.
    pub fn set_plugin_commands(&mut self, commands: Vec<eix_core::PluginCommandInfo>) {
        self.plugin_commands = commands;
        self.reload_commands();
    }

    /// True when no full-screen overlay owns the keyboard, so a prompt request
    /// can safely open the picker.
    pub(super) fn overlay_closed(&self) -> bool {
        self.picker.is_none()
            && self.diff_view.is_none()
            && self.process_view.is_none()
            && self.inbox.is_none()
            && self.btw.is_none()
            && self.error.is_none()
            && self.reason_prompt.is_none()
    }

    /// Replace the prompt text without pulling a user who is in normal mode
    /// back into insert mode.
    pub(super) fn replace_prompt_text(&mut self, text: &str) {
        let mode = self.input.mode();
        self.input.set_text(text);
        if mode == Mode::Normal {
            self.input.enter_normal();
        }
    }

    /// Put the last submitted draft back after a failed turn, if it belongs to
    /// `session_id` and the prompt is still empty (the user may already be
    /// typing the next message).
    pub(super) fn restore_pending_draft(&mut self, session_id: &str) {
        let matches = self
            .pending_draft
            .as_ref()
            .is_some_and(|(id, _)| id == session_id);
        if !matches {
            return;
        }
        if self.focused_session_id().as_deref() != Some(session_id) {
            self.pending_draft = None;
            return;
        }
        if let Some((_, draft)) = self.pending_draft.take() {
            if self.input.is_empty() {
                self.replace_prompt_text(&draft);
            }
        }
    }

    /// Drop the saved draft once its turn finishes successfully.
    pub(super) fn clear_pending_draft(&mut self, session_id: &str) {
        if self
            .pending_draft
            .as_ref()
            .is_some_and(|(id, _)| id == session_id)
        {
            self.pending_draft = None;
        }
    }

    /// Take the pending bell request, if any.
    pub fn take_ring(&mut self) -> bool {
        std::mem::take(&mut self.ring)
    }

    /// True once after an external editor ran, so the frame is fully repainted.
    pub fn take_force_clear(&mut self) -> bool {
        std::mem::take(&mut self.force_clear)
    }

    /// Text selected with the mouse, for the event loop's clipboard writer.
    pub fn take_clipboard(&mut self) -> Option<String> {
        self.pending_clipboard.take()
    }

    /// An older-history page request produced by the last draw.
    pub fn take_history_request(&mut self) -> Option<(String, u64)> {
        self.pending_history.take()
    }

    /// A lazy picker preview request produced by the last draw.
    pub fn take_picker_preview_request(&mut self) -> Option<(String, Option<String>, String)> {
        self.picker.as_mut()?.preview_request.take()
    }

    /// Whether a session created by this client still has local work that must
    /// not be raced by a cleanup delete.
    pub(super) fn created_session_is_in_use(&self, session_id: &str) -> bool {
        let in_flight = self.submitting_session.as_deref() == Some(session_id)
            || self
                .queued_inflight
                .as_ref()
                .is_some_and(|(id, _)| id == session_id);
        let queued = self
            .queues
            .get(session_id)
            .is_some_and(|queue| !queue.is_empty());
        let loading_draft = self.draft_loading.as_deref() == Some(session_id);
        let local_draft = !self.input.text().is_empty()
            && (self.input_session.as_deref() == Some(session_id)
                || self
                    .focused_view()
                    .is_some_and(|view| view.session.id == session_id));
        in_flight || queued || loading_draft || local_draft
    }

    /// Drop sessions created by this client that were never used. The daemon
    /// re-checks the event log and draft, so an in-flight prompt or a saved
    /// draft can never be deleted by this cleanup.
    pub fn cleanup_empty_sessions(&mut self, tx: &UnboundedSender<Command>) {
        let session_ids = std::mem::take(&mut self.created_sessions);
        for session_id in session_ids {
            if self.created_session_is_in_use(&session_id) {
                continue;
            }
            let _ = tx.send(Command::DeleteSessionIfEmpty { session_id });
        }
    }

    /// Ask the daemon to discard a created session after its pane closes. It
    /// stays in `created_sessions` until `session.deleted` arrives, so a failed
    /// or deferred check can be retried when the TUI quits.
    pub(super) fn cleanup_created_session(
        &mut self,
        session_id: &str,
        tx: &UnboundedSender<Command>,
    ) {
        if self.created_sessions.contains(session_id) && !self.created_session_is_in_use(session_id)
        {
            let _ = tx.send(Command::DeleteSessionIfEmpty {
                session_id: session_id.to_string(),
            });
        }
    }

    pub fn tick(&mut self, tx: &UnboundedSender<Command>) {
        self.tick = self.tick.wrapping_add(1);
        self.refresh_git_branch();
        self.sync_draft(tx);
        self.flush_picker_query(tx);
        self.flush_queues(tx);
        self.flush_resumes(tx);
    }

    /// Update the branch shown in the prompt header. Spawning `git` on every
    /// tick would be wasteful, so a cached value is reused for a few seconds.
    pub(super) fn refresh_git_branch(&mut self) {
        let cwd = self
            .focused_view()
            .map(|view| view.session.cwd.clone())
            .or_else(|| {
                self.sessions
                    .get(self.selected)
                    .map(|session| session.cwd.clone())
            });
        let Some(cwd) = cwd else {
            self.git_cwd = None;
            self.git_branch = None;
            self.git_checked = None;
            return;
        };
        if self.git_cwd.as_ref() != Some(&cwd) {
            self.git_cwd = Some(cwd.clone());
            self.git_branch = None;
            self.git_checked = None;
        }
        if self
            .git_checked
            .is_some_and(|checked| checked.elapsed() < GIT_BRANCH_REFRESH)
        {
            return;
        }
        self.git_checked = Some(Instant::now());
        self.git_branch = read_git_branch(&cwd);
    }

    /// Persist the prompt as a per-session draft when the focused pane changes
    /// or the text is edited.
    pub(super) fn sync_draft(&mut self, tx: &UnboundedSender<Command>) {
        // Wait for the daemon draft to load before writing; otherwise stale
        // buffer text would overwrite the new session's draft.
        if self.draft_loading.is_some() {
            return;
        }
        let focused = self
            .views
            .get(self.focused)
            .map(|view| view.session.id.clone());
        if focused != self.input_session {
            if let Some(previous) = self.input_session.take() {
                let text = self.current_draft_text();
                if text != self.last_saved_draft {
                    let _ = tx.send(Command::DraftSet {
                        session_id: previous,
                        text,
                    });
                }
            }
            // Switching panes is not an edit: keep the prompt in normal mode
            // instead of letting `clear()` reset it to insert.
            let normal = self.input.mode() == Mode::Normal;
            self.input.clear();
            if normal {
                self.input.enter_normal();
            }
            self.last_saved_draft.clear();
            self.input_session = focused;
            if let Some(session_id) = self.input_session.clone() {
                self.draft_loading = Some(session_id.clone());
                let _ = tx.send(Command::DraftGet { session_id });
            }
            return;
        }
        let Some(session_id) = self.input_session.clone() else {
            return;
        };
        let text = self.current_draft_text();
        if text != self.last_saved_draft {
            self.last_saved_draft = text.clone();
            let _ = tx.send(Command::DraftSet { session_id, text });
        }
    }

    /// Explicit `:w`: persist the prompt draft without switching panes or
    /// clearing the buffer.
    pub(super) fn save_draft_now(&mut self, tx: &UnboundedSender<Command>) {
        if self.draft_loading.is_some() {
            self.status = "draft still loading…".into();
            return;
        }
        let Some(session_id) = self
            .input_session
            .clone()
            .or_else(|| self.focused_session_id())
        else {
            self.status = "no session".into();
            return;
        };
        let text = self.current_draft_text();
        self.last_saved_draft = text.clone();
        let _ = tx.send(Command::DraftSet { session_id, text });
        self.status = "draft saved".into();
    }

    /// Current prompt contents.
    pub(super) fn current_draft_text(&self) -> String {
        self.input.text()
    }

    pub(super) fn attached(&mut self, attached: AttachResult, tx: &UnboundedSender<Command>) {
        let Some(pending) = &self.pending_attach else {
            return;
        };
        if pending.session_id != attached.session.id {
            // A stale response from an earlier selection.
            return;
        }
        let pending = self.pending_attach.take().expect("checked above");
        self.upsert_session(attached.session.clone(), false);
        let session_id = attached.session.id.clone();
        let is_subagent = attached.session.is_agent();
        // Recovery marks a turn that was running when the daemon restarted as
        // `paused`. Its event log is intact, so an ordinary user session can
        // be continued without asking the user to retype the prompt.
        // Waiting-for-approval and waiting-for-input turns stay errored.
        let resume = !is_subagent && attached.session.status == SessionStatus::Paused;
        let has_more = attached.has_more;
        let mut view = SessionView::new(attached.session, attached.events);
        view.history_has_more = has_more;

        if let Some(open) = self.open_session(&session_id) {
            // One session lives in at most one pane.
            self.views[open] = view;
            self.focused = open;
        } else {
            match pending.slot {
                AttachSlot::Existing(index) if index < self.views.len() => {
                    self.views[index] = view;
                    self.focused = index;
                }
                _ => {
                    self.views.push(view);
                    self.focused = self.views.len() - 1;
                }
            }
        }
        self.focused = self.focused.min(self.views.len().saturating_sub(1));
        self.reload_pending(&session_id);
        self.reload_input(&session_id);
        if !is_subagent {
            self.selected = self.find_session(&session_id).unwrap_or(self.selected);
        }
        self.error = None;
        self.status = "attached".into();
        if resume {
            self.queue_resume(&session_id);
        }
        self.attach_next_restore(tx);
    }

    /// Restored panes are attached one at a time because the app only keeps a
    /// single pending attach. The last response restores the saved focus.
    pub(super) fn attach_next_restore(&mut self, tx: &UnboundedSender<Command>) {
        if !self.restore_queue.is_empty() {
            let session_id = self.restore_queue.remove(0);
            self.pending_attach = Some(PendingAttach {
                session_id: session_id.clone(),
                slot: AttachSlot::New,
            });
            self.error = None;
            let _ = tx.send(Command::Attach(session_id));
            return;
        }
        if let Some(focused) = self.restore_focused.take() {
            if let Some(index) = self.open_session(&focused) {
                self.focused = index;
                self.sync_selected_to_focus();
            }
        }
    }

    pub(super) fn submitted(&mut self, submitted: SubmitResult) {
        self.created_sessions.remove(&submitted.session.id);
        self.inflight_images.clear();
        self.submitting = false;
        self.submitting_session = None;
        self.queued_inflight = None;
        self.reset_history();
        if let Some(prompt) = self.pending_prompt.take() {
            if self.input.text() == prompt {
                self.input.clear();
            }
        }
        self.upsert_session(submitted.session.clone(), false);
        if let Some(view) = self
            .views
            .iter_mut()
            .find(|view| view.session.id == submitted.session.id)
        {
            view.session = submitted.session.clone();
            if submitted.queued {
                // The daemon queued the prompt behind a running turn. Its
                // `turn.started` notification carries the real turn id; a
                // stream keyed on an empty id would drop every delta.
                view.folds.remove(&FoldId::Live);
                view.follow = true;
                self.status = "queued · waiting for the running turn".into();
            } else {
                // Older deltas may already have created the stream through
                // `turn.started`; keep them instead of resetting the text.
                if view
                    .stream
                    .as_ref()
                    .is_none_or(|stream| stream.turn_id != submitted.turn_id)
                {
                    view.stream = Some(StreamState::new(submitted.turn_id));
                }
                view.folds.remove(&FoldId::Live);
                view.follow = true;
                self.status = "running".into();
            }
        }
        self.error = None;
    }

    /// Hide the `<Space>` leader hint as soon as the next key arrives. The
    /// action that consumes the key can still publish its own status.
    pub(super) fn restore_leader_status(&mut self) {
        let Some(status) = self.leader_status_backup.take() else {
            return;
        };
        if self.status.starts_with("leader:") {
            self.status = status;
            // The restored text was already seen before the leader hint; do
            // not replay it as a brand-new notification.
            self.last_seen_status.clone_from(&self.status);
        }
    }

    /// Resolve the ids saved by the previous TUI run. Unknown, agent and
    /// duplicate ids are dropped so a stale workspace file can never block
    /// startup. The focused id is discarded when its pane is gone.
    pub(super) fn take_restore_sessions(&mut self) -> Vec<String> {
        let requested = std::mem::take(&mut self.restore_sessions);
        let mut seen = HashSet::new();
        let restored: Vec<String> = requested
            .into_iter()
            .filter(|id| !id.is_empty() && seen.insert(id.clone()))
            .filter(|id| {
                self.sessions
                    .iter()
                    .any(|session| session.id == *id && !session.is_agent())
            })
            .collect();
        if self
            .restore_focused
            .as_ref()
            .is_some_and(|focused| !restored.contains(focused))
        {
            self.restore_focused = None;
        }
        restored
    }

    /// Fallback for a workspace with no saved panes: the most recently updated
    /// user session bound to this directory. `list_sessions` is newest first.
    pub(super) fn last_session_for_cwd(&self) -> Option<usize> {
        match std::env::current_dir() {
            Ok(cwd) => self
                .sessions
                .iter()
                .position(|session| !session.is_agent() && session.cwd_matches(&cwd)),
            // Without a usable launch directory, preserve the previous
            // "most recent session" fallback.
            Err(_) => (!self.sessions.is_empty()).then_some(0),
        }
    }
}
