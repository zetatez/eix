//! Session and pane management, submit/queue plumbing, model selection.

use super::*;

impl App {
    /// True while the focused turn can still be interrupted.
    pub(super) fn focused_session_busy(&self) -> bool {
        self.submitting_session.is_some()
            || self
                .focused_view()
                .is_some_and(|view| view.session.status.is_busy())
    }

    /// Fork the focused session. `at_seq` copies history strictly before that
    /// sequence (used by the ledger's rewind); `None` copies everything.
    pub(super) fn fork_focused(&mut self, tx: &UnboundedSender<Command>, at_seq: Option<u64>) {
        let Some(session_id) = self.focused_view().map(|view| view.session.id.clone()) else {
            self.status = "no session to fork".into();
            return;
        };
        let _ = tx.send(Command::Fork {
            session_id,
            at_seq,
            name: None,
        });
        self.status = match at_seq {
            Some(seq) => format!("forking before e{seq}…"),
            None => "forking…".into(),
        };
    }

    pub(super) fn jump_to_user_seq(&mut self, seq: u64) {
        let Some(view) = self.focused_view_mut() else {
            self.status = "attach a session first".into();
            return;
        };
        let Some(line) = view
            .user_lines
            .iter()
            .find(|(message_seq, _)| *message_seq == seq)
            .map(|(_, line)| *line)
        else {
            self.status = format!("e{seq} is not loaded in this pane");
            return;
        };
        view.follow = false;
        view.scroll = line.saturating_sub(2);
        self.status = format!("jumped to e{seq}");
    }

    /// Advance the focused session through `Plan → Build → Auto → Full`.
    /// `delta` is +1 for Tab and -1 for Shift-Tab, so the reverse walk is
    /// `Build → Plan → Full → Auto`. Update locally first so rapid presses
    /// keep moving before the daemon round-trip lands; the session event
    /// reconciles the final state.
    pub(super) fn cycle_mode(&mut self, delta: isize, tx: &UnboundedSender<Command>) {
        const MODES: [SessionMode; 4] = [
            SessionMode::Plan,
            SessionMode::Build,
            SessionMode::Auto,
            SessionMode::Full,
        ];
        let Some(view) = self.focused_view_mut() else {
            self.status = "attach a session first".into();
            return;
        };
        let session_id = view.session.id.clone();
        let current = MODES
            .iter()
            .position(|mode| *mode == view.session.mode)
            .unwrap_or(1);
        let next = MODES[(current as isize + delta).rem_euclid(MODES.len() as isize) as usize];
        view.session.mode = next;
        let _ = tx.send(Command::SetMode {
            session_id,
            mode: next,
            temporary: false,
        });
    }

    /// Tab cycles forward; Shift-Tab reverses in the host.
    pub(super) fn toggle_mode(&mut self, tx: &UnboundedSender<Command>) {
        self.cycle_mode(1, tx);
    }

    /// Session indices shown in the picker. Subagents live inside their
    /// parent's transcript, not in the global session list.
    pub(super) fn visible_session_indices(&self) -> Vec<usize> {
        self.sessions
            .iter()
            .enumerate()
            .filter(|(_, session)| !session.is_agent())
            .map(|(index, _)| index)
            .collect()
    }

    pub(super) fn filtered_sessions(&self) -> Vec<usize> {
        let Some(picker) = &self.picker else {
            return Vec::new();
        };
        let query = picker.query.trim().to_lowercase();
        if query.is_empty() {
            return self.visible_session_indices();
        }
        let mut hits: Vec<(i32, usize)> = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| {
                if session.is_agent() {
                    return None;
                }
                let haystack = format!("{} {} {}", session.name, session.id, session.cwd.display())
                    .to_lowercase();
                fuzzy_score(&query, &haystack).map(|score| (score, index))
            })
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        hits.into_iter().map(|(_, index)| index).collect()
    }

    pub(super) fn create_session(&mut self, tx: &UnboundedSender<Command>, worktree: Option<bool>) {
        self.create_session_in(tx, worktree, None, None);
    }

    /// Create a session on the focused session's workspace. If it already runs
    /// in a worktree the new session shares that worktree; otherwise it shares
    /// the plain workspace. `/new` never creates a new worktree — that is what
    /// `/new-worktree` is for.
    pub(super) fn new_session(&mut self, tx: &UnboundedSender<Command>, name: Option<String>) {
        let cwd = self.focused_view().map(|view| view.session.cwd.clone());
        self.create_session_in(tx, Some(false), cwd, name);
    }

    /// Move the focused session into a new worktree. A session can only own
    /// one worktree; when it already has one, leave everything untouched and
    /// surface the reason in the top-right toast.
    pub(super) fn new_worktree(&mut self, tx: &UnboundedSender<Command>, name: Option<String>) {
        let Some(session) = self.focused_view().map(|view| view.session.clone()) else {
            let message = "attach a session first";
            self.status = message.into();
            self.push_toast(message.into(), ToastTone::Warning, WARNING_TOAST_DISPLAY);
            return;
        };
        if session.has_worktree() {
            let message = "this session already has a worktree";
            self.error = None;
            self.status = message.into();
            self.push_toast(message.into(), ToastTone::Warning, WARNING_TOAST_DISPLAY);
            return;
        }
        if session.status.is_busy() {
            let message = "wait for the turn to finish before creating a worktree";
            self.error = None;
            self.status = message.into();
            self.push_toast(message.into(), ToastTone::Warning, WARNING_TOAST_DISPLAY);
            return;
        }
        self.error = None;
        self.status = "creating worktree…".into();
        let _ = tx.send(Command::CreateWorktree {
            session_id: session.id,
            name,
        });
    }

    /// Create a session and open it in a new pane. `cwd`/`name` default to the
    /// TUI's launch directory / a generated name.
    pub(super) fn create_session_in(
        &mut self,
        tx: &UnboundedSender<Command>,
        worktree: Option<bool>,
        cwd: Option<std::path::PathBuf>,
        name: Option<String>,
    ) {
        if self.creating {
            return;
        }
        self.creating = true;
        self.error = None;
        self.status = if worktree == Some(true) {
            "creating session in a new worktree…".into()
        } else {
            "creating session…".into()
        };
        let _ = tx.send(Command::Create {
            worktree,
            cwd,
            name,
            model: self.default_model.clone(),
            agent: self.default_agent.clone(),
            mode: self.default_mode.clone(),
        });
    }

    pub(super) fn submit(&mut self, tx: &UnboundedSender<Command>) {
        let text = self.input.text();
        if text.trim_start().starts_with('/') && self.handle_prompt_command(&text, tx) {
            return;
        }
        if text.trim().is_empty() && self.pending_images.is_empty() {
            return;
        }
        let images = std::mem::take(&mut self.pending_images);
        if !images.is_empty()
            && self
                .focused_session_id()
                .is_some_and(|id| self.session_busy(&id))
        {
            self.pending_images = images;
            self.status = "wait for the running turn before sending images".into();
            return;
        }
        let Some(session_id) = self.focused_session_id() else {
            self.pending_images = images;
            self.status = "no session yet".into();
            return;
        };
        if let Some(command) = text.trim_start().strip_prefix('!') {
            self.pending_images = images;
            let command = command.trim();
            if command.is_empty() {
                self.status = "usage: !<shell command>".into();
                return;
            }
            self.input.clear();
            self.input.enter_insert();
            self.last_saved_draft.clear();
            self.reset_history();
            self.error = None;
            let _ = tx.send(Command::RunShell {
                session_id: session_id.clone(),
                command: command.to_string(),
                insert_output: false,
            });
            let _ = tx.send(Command::DraftSet {
                session_id,
                text: String::new(),
            });
            self.status = "running shell command…".into();
            return;
        }
        // A turn is already running (or its submit is in flight): queue the new
        // prompt instead of refusing it. `tick` drains the queue when idle.
        if self.session_busy(&session_id) {
            if !images.is_empty() {
                self.pending_images = images;
                self.status = "wait for the running turn before sending images".into();
                return;
            }
            self.enqueue(session_id, &text, tx);
            return;
        }
        self.reset_history();
        self.send_prompt(&session_id, &text, tx, true, images);
    }

    /// Send a prompt to a specific session. `from_prompt` means it came from
    /// the focused prompt (restore the draft if it fails); queued prompts are
    /// put back at the head of their queue instead.
    pub(super) fn send_prompt(
        &mut self,
        session_id: &str,
        text: &str,
        tx: &UnboundedSender<Command>,
        from_prompt: bool,
        images: Vec<String>,
    ) {
        self.submitting = true;
        self.submitting_session = Some(session_id.to_string());
        self.error = None;
        if from_prompt {
            self.pending_prompt = Some(text.to_string());
            self.pending_draft = Some((session_id.to_string(), text.to_string()));
            self.inflight_images = images.clone();
            self.status = "sending…".into();
        } else {
            self.queued_inflight = Some((session_id.to_string(), text.to_string()));
            self.status = "sending queued prompt…".into();
        }
        let _ = tx.send(Command::Submit {
            session_id: session_id.to_string(),
            text: text.to_string(),
            images,
        });
    }

    /// Park a prompt until its session goes idle. The prompt box is cleared so
    /// the user can keep typing the next one.
    pub(super) fn enqueue(
        &mut self,
        session_id: String,
        text: &str,
        tx: &UnboundedSender<Command>,
    ) {
        let queue = self.queues.entry(session_id.clone()).or_default();
        queue.push_back(text.to_string());
        let pending = queue.len();
        let insert = self.input.mode() == Mode::Insert;
        self.input.clear();
        if insert {
            self.input.enter_insert();
        }
        self.last_saved_draft.clear();
        self.reset_history();
        self.error = None;
        let _ = tx.send(Command::DraftSet {
            session_id,
            text: String::new(),
        });
        self.status = format!("queued · {pending} prompt(s) waiting");
    }

    /// Drain at most one queued prompt for an idle session. Called on every
    /// tick, which also retries after a transient `busy` race.
    pub(super) fn flush_queues(&mut self, tx: &UnboundedSender<Command>) {
        if self.submitting {
            return;
        }
        let Some(session_id) = self
            .queues
            .iter()
            .find(|(id, queue)| !queue.is_empty() && !self.session_busy(id))
            .map(|(id, _)| id.clone())
        else {
            return;
        };
        let Some(text) = self
            .queues
            .get_mut(&session_id)
            .and_then(VecDeque::pop_front)
        else {
            return;
        };
        self.send_prompt(&session_id, &text, tx, false, Vec::new());
    }

    pub(super) fn queue_len(&self, session_id: &str) -> usize {
        self.queues.get(session_id).map(VecDeque::len).unwrap_or(0)
    }

    pub(super) fn session_busy(&self, session_id: &str) -> bool {
        self.submitting_session.as_deref() == Some(session_id)
            || self
                .views
                .iter()
                .find(|view| view.session.id == session_id)
                .is_some_and(|view| view.session.status.is_busy())
    }

    pub(super) fn focused_session_id(&self) -> Option<String> {
        self.focused_view().map(|view| view.session.id.clone())
    }

    /// Persist the (cleared) draft and quit the whole TUI. Open sessions stay
    /// alive in the daemon; `/close` is the command that closes a session pane.
    pub(super) fn exit_session(&mut self, tx: &UnboundedSender<Command>) {
        if let Some(view) = self.focused_view() {
            let session_id = view.session.id.clone();
            let name = view.session.name.clone();
            // Do not persist the command line itself as the next draft.
            self.input.clear();
            self.last_saved_draft.clear();
            let _ = tx.send(Command::DraftSet {
                session_id,
                text: String::new(),
            });
            self.status = format!("saved {name} · quitting");
        }
        self.should_quit = true;
    }

    /// Close the focused session pane. Closing the last pane leaves the client
    /// with nothing to render, so it also quits the TUI.
    pub(super) fn close_session(&mut self, tx: &UnboundedSender<Command>) {
        let Some(view) = self.focused_view() else {
            return;
        };
        let session_id = view.session.id.clone();
        let name = view.session.name.clone();
        // Do not persist the command line itself as the next draft.
        self.input.clear();
        self.last_saved_draft.clear();
        let _ = tx.send(Command::DraftSet {
            session_id,
            text: String::new(),
        });
        if self.views.len() <= 1 {
            self.status = format!("saved {name} · quitting");
            self.clear_active_on_quit = true;
            self.should_quit = true;
            return;
        }
        if let Some(session_id) = self.close_focused_pane() {
            self.cleanup_created_session(&session_id, tx);
        }
        self.status = format!("{name} saved · pane closed");
    }

    /// Quit from the `:` command line. Unlike `/quit`, the prompt holds a real
    /// draft rather than the command text, so persist it first.
    pub(super) fn quit_preserving_draft(&mut self, tx: &UnboundedSender<Command>) {
        let name = self.focused_view().map(|view| view.session.name.clone());
        self.save_draft_now(tx);
        self.status = match name {
            Some(name) => format!("saved {name} · quitting"),
            None => "quitting".into(),
        };
        self.should_quit = true;
    }

    /// Close the focused pane from the `:` command line without treating the
    /// prompt as command text. The next pane loads its own draft normally.
    pub(super) fn close_preserving_draft(&mut self, tx: &UnboundedSender<Command>) {
        let Some(view) = self.focused_view() else {
            return;
        };
        let name = view.session.name.clone();
        if self.draft_loading.is_some() {
            self.status = "draft still loading…".into();
            return;
        }
        self.save_draft_now(tx);
        if self.views.len() <= 1 {
            self.status = format!("saved {name} · quitting");
            self.clear_active_on_quit = true;
            self.should_quit = true;
            return;
        }
        self.input.clear();
        self.last_saved_draft.clear();
        self.input_session = None;
        if let Some(session_id) = self.close_focused_pane() {
            self.cleanup_created_session(&session_id, tx);
        }
        self.sync_draft(tx);
        self.status = format!("{name} saved · pane closed");
    }

    pub(super) fn focused_view(&self) -> Option<&SessionView> {
        self.views.get(self.focused)
    }

    pub(super) fn focused_view_mut(&mut self) -> Option<&mut SessionView> {
        let index = self.focused;
        self.views.get_mut(index)
    }

    /// A pane needs attention when its session is blocked and the pane is not
    /// focused. This is derived from the session status and any pending
    /// approval/patch instead of a one-shot flag, so switching away from a
    /// pending card still marks it.
    pub(super) fn pane_needs_attention(&self, index: usize) -> bool {
        index != self.focused
            && self.views.get(index).is_some_and(|view| {
                view.session.status.needs_attention()
                    || self.approvals.contains_key(&view.session.id)
            })
    }

    pub(super) fn attention_count(&self) -> usize {
        (0..self.views.len())
            .filter(|index| self.pane_needs_attention(*index))
            .count()
    }

    pub(super) fn focus_next_attention(&mut self) {
        let len = self.views.len();
        for offset in 1..len {
            let index = (self.focused + offset) % len;
            if self.views[index].session.status.needs_attention()
                || self.approvals.contains_key(&self.views[index].session.id)
            {
                self.focus_pane(index);
                let name = self.views[index].session.name.clone();
                self.status = format!("attention: {name}");
                return;
            }
        }
        self.status = if self.focused_view().is_some_and(|view| {
            view.session.status.needs_attention() || self.approvals.contains_key(&view.session.id)
        }) {
            "the focused pane needs attention".into()
        } else {
            "nothing needs attention".into()
        };
    }

    pub(super) fn open_session(&self, session_id: &str) -> Option<usize> {
        self.views
            .iter()
            .position(|view| view.session.id == session_id)
    }

    pub(super) fn focus_pane(&mut self, index: usize) {
        if index >= self.views.len() {
            return;
        }
        self.focused = index;
        self.sync_selected_to_focus();
        let unit = if self.layout == PaneLayout::Tabby {
            "tab"
        } else {
            "pane"
        };
        self.status = format!("{unit} {}/{} focused", index + 1, self.views.len());
    }

    pub(super) fn sync_selected_to_focus(&mut self) {
        if let Some(view) = self.focused_view() {
            if view.session.is_agent() {
                return;
            }
            if let Some(index) = self.find_session(&view.session.id) {
                self.selected = index;
            }
        }
    }

    pub(super) fn set_layout(&mut self, layout: PaneLayout) {
        self.layout = layout;
        let status = format!("layout: {}", layout.label());
        self.status.clone_from(&status);
        self.push_toast(status, ToastTone::Success, LAYOUT_TOAST_DISPLAY);
    }

    /// Focus the Nth open session (`Ctrl-1` is the first pane). In tabby
    /// layout the focused session is also the active tab.
    pub(super) fn focus_tab(&mut self, index: usize) {
        if index >= self.views.len() {
            self.status = format!("session {} is not open", index + 1);
            return;
        }
        self.focus_pane(index);
    }

    /// Jump focus to an open session without changing the pane order.
    pub(super) fn jump_to_session(&mut self, session_id: &str) -> bool {
        let Some(index) = self.open_session(session_id) else {
            return false;
        };
        self.focus_pane(index);
        true
    }

    pub(super) fn cycle_pane(&mut self, delta: isize) {
        if self.views.len() < 2 {
            return;
        }
        let len = self.views.len() as isize;
        let next = (self.focused as isize + delta).rem_euclid(len) as usize;
        self.focus_pane(next);
    }

    /// Move the focused pane's session to the first slot while keeping the
    /// same session focused. Bound to Enter in normal/transcript mode.
    pub(super) fn promote_focused_session(&mut self) {
        if self.focused == 0 || self.views.len() < 2 {
            return;
        }
        let view = self.views.remove(self.focused);
        let name = view.session.name.clone();
        self.views.insert(0, view);
        self.focused = 0;
        self.sync_selected_to_focus();
        self.status = format!("{name} promoted to first");
    }

    pub(super) fn close_focused_pane(&mut self) -> Option<String> {
        if self.views.len() <= 1 {
            self.status = "cannot close the last pane".into();
            return None;
        }
        let removed = self.views.remove(self.focused);
        self.focused = self.focused.min(self.views.len().saturating_sub(1));
        self.sync_selected_to_focus();
        self.status = "pane closed".into();
        Some(removed.session.id)
    }

    pub(super) fn cycle_session(&mut self, delta: isize, tx: &UnboundedSender<Command>) {
        let visible = self.visible_session_indices();
        if visible.is_empty() {
            return;
        }
        let current = visible
            .iter()
            .position(|&index| index == self.selected)
            .unwrap_or_else(|| {
                visible
                    .iter()
                    .position(|&index| index > self.selected)
                    .unwrap_or(0)
            });
        let len = visible.len() as isize;
        let next = visible[(current as isize + delta).rem_euclid(len) as usize];
        self.select_session(next, tx);
    }

    pub(super) fn select_session(&mut self, index: usize, tx: &UnboundedSender<Command>) {
        if self.sessions.is_empty() {
            return;
        }
        let index = index.min(self.sessions.len() - 1);
        if !self.sessions[index].is_agent() {
            self.selected = index;
        }
        let id = self.sessions[index].id.clone();
        if self.jump_to_session(&id) {
            self.focus = Focus::Input;
            self.status = "focused open pane".into();
            return;
        }
        let slot = if self.views.is_empty() {
            AttachSlot::New
        } else {
            AttachSlot::Existing(self.focused)
        };
        self.pending_attach = Some(PendingAttach {
            session_id: id.clone(),
            slot,
        });
        self.error = None;
        self.status = "loading session…".into();
        self.focus = Focus::Input;
        let _ = tx.send(Command::Attach(id));
    }

    pub(super) fn open_in_new_pane(&mut self, index: usize, tx: &UnboundedSender<Command>) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = index.min(self.sessions.len() - 1);
        let id = self.sessions[self.selected].id.clone();
        self.open_session_id_in_new_pane(id, tx);
    }

    /// Open or focus one session in a new pane. The subagent card uses the
    /// child session id directly because it is not listed in the picker.
    pub(super) fn open_session_id_in_new_pane(
        &mut self,
        id: String,
        tx: &UnboundedSender<Command>,
    ) {
        if let Some(open) = self.open_session(&id) {
            self.focused = open;
            self.status = "session is already open".into();
            return;
        }
        self.pending_attach = Some(PendingAttach {
            session_id: id.clone(),
            slot: AttachSlot::New,
        });
        self.error = None;
        self.status = "opening new pane…".into();
        let _ = tx.send(Command::Attach(id));
    }

    /// Return from a subagent session to the main agent that spawned it.
    pub(super) fn back_to_parent(&mut self, tx: &UnboundedSender<Command>) {
        let Some(view) = self.focused_view() else {
            self.status = "attach a session first".into();
            return;
        };
        if !view.session.is_agent() {
            self.status = "current session has no parent agent".into();
            return;
        }
        let Some(parent_id) = view.session.parent_id.clone() else {
            self.status = "subagent has no parent session".into();
            return;
        };
        if self.open_session(&parent_id).is_some() {
            // The child pane is transient: returning to the parent closes it
            // instead of leaving a finished subagent beside the main session.
            if self.views.len() > 1 {
                self.close_focused_pane();
            }
            if let Some(index) = self.open_session(&parent_id) {
                self.focus_pane(index);
            }
            self.status = "parent agent focused".into();
            return;
        }
        self.pending_attach = Some(PendingAttach {
            session_id: parent_id.clone(),
            slot: AttachSlot::Existing(self.focused),
        });
        self.error = None;
        self.status = "opening parent agent…".into();
        self.focus = Focus::Input;
        let _ = tx.send(Command::Attach(parent_id));
    }

    pub(super) fn reselect_current(&mut self) {
        if let Some(view) = self.focused_view() {
            if !view.session.is_agent() {
                if let Some(index) = self.find_session(&view.session.id) {
                    self.selected = index;
                    return;
                }
            }
        }
        if let Some(pending) = &self.pending_attach {
            if let Some(index) = self.find_session(&pending.session_id) {
                if !self.sessions[index].is_agent() {
                    self.selected = index;
                }
                return;
            }
        }
        self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
    }

    pub(super) fn find_session(&self, needle: &str) -> Option<usize> {
        if let Some(index) = self.sessions.iter().position(|s| s.id == needle) {
            return Some(index);
        }
        let matches: Vec<usize> = self
            .sessions
            .iter()
            .enumerate()
            .filter(|(_, s)| s.id.starts_with(needle))
            .map(|(index, _)| index)
            .collect();
        if matches.len() == 1 {
            Some(matches[0])
        } else {
            None
        }
    }

    pub(super) fn upsert_session(&mut self, session: Session, front: bool) {
        if let Some(existing) = self
            .sessions
            .iter_mut()
            .find(|existing| existing.id == session.id)
        {
            *existing = session;
        } else if front {
            self.sessions.insert(0, session);
        } else {
            self.sessions.push(session);
        }
    }

    pub(super) fn scroll_focused(&mut self, amount: isize) {
        let index = self.focused;
        // Pane borders omit the bottom row, so the transcript viewport is the
        // pane height minus its top border. This is only needed to detect the
        // tail; rendering still clamps the actual window.
        let inner_height = self
            .pane_areas
            .get(index)
            .map(|rect| rect.height.saturating_sub(1) as usize)
            .unwrap_or(0);
        let Some(view) = self.focused_view_mut() else {
            return;
        };
        let max_scroll = view.line_count.saturating_sub(inner_height);
        if amount < 0 {
            // Nothing above the tail to pause on: keep following so the next
            // streamed line still scrolls into view.
            if max_scroll == 0 {
                view.follow = true;
                view.scroll = 0;
                return;
            }
            // Start from the current tail even if new output arrived since the
            // last frame, so one wheel-up pauses one line above it.
            if view.follow {
                view.scroll = max_scroll;
            }
            view.follow = false;
            // Scrolling up is an explicit request for older content, so allow
            // another page even if the previous page added very few lines.
            view.history_armed = true;
            view.scroll = view.scroll.saturating_sub(amount.unsigned_abs());
            return;
        }
        view.scroll = view.scroll.saturating_add(amount as usize).min(max_scroll);
        // Resume auto-follow once the viewport is back at (or just above) the
        // tail. A deliberate pause further up stays paused as output streams.
        view.follow = max_scroll.saturating_sub(view.scroll) <= FOLLOW_SNAP_LINES;
        if view.follow {
            view.scroll = max_scroll;
        }
    }

    pub(super) fn model(&self) -> &str {
        self.focused_view()
            .and_then(|view| view.session.model.as_deref())
            .or_else(|| self.hello.as_ref().map(|hello| hello.model.as_str()))
            .unwrap_or("-")
    }

    /// Model id, provider and variant shown in the prompt header. The catalog
    /// supplies the raw API id so named providers do not repeat their routing
    /// prefix in front of the model name.
    pub(super) fn model_header_parts(&self) -> (String, String, Option<String>) {
        let selection = self.model();
        let (base, variant) = split_model_variant(selection);
        let variant = match variant {
            // `#none` is the reserved "no reasoning_effort" step in the
            // Ctrl-T cycle, so the footer omits the field entirely.
            Some("none") => None,
            Some(variant) => Some(variant.to_string()),
            // With no explicit variant the effective default is medium.
            None if !selection.is_empty() && selection != "-" => Some("medium".to_string()),
            None => None,
        };
        if let Some(info) = self.models.iter().find(|info| info.id == base) {
            return (
                info.display_id().to_string(),
                info.provider.clone(),
                variant,
            );
        }
        if let Some(rest) = base.strip_prefix("default/") {
            return (rest.to_string(), "default".to_string(), variant);
        }
        let provider = base
            .split_once('/')
            .map(|(provider, _)| provider.to_string())
            .unwrap_or_else(|| "default".to_string());
        (base.to_string(), provider, variant)
    }

    /// Cycle the focused session's reasoning strength:
    /// low → medium → high → low. A model without an explicit variant starts
    /// at the default medium, so the first step is high.
    pub(super) fn cycle_reasoning(&mut self, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_session_id() else {
            self.status = "reasoning: no session".into();
            return;
        };
        let selection = self.model().to_string();
        if selection.is_empty() || selection == "-" {
            self.status = "reasoning: no model".into();
            return;
        }
        let (base, current) = split_model_variant(&selection);
        let next = match current.unwrap_or("medium") {
            "low" => "medium",
            "medium" => "high",
            "high" => "low",
            // Older sessions may still carry the retired `#none` step.
            "none" => "low",
            _ => "low",
        };
        let model = if next == "medium" {
            // Medium is the default, so keep the plain model id.
            base.to_string()
        } else {
            format!("{base}#{next}")
        };
        if let Some(view) = self
            .views
            .iter_mut()
            .find(|view| view.session.id == session_id)
        {
            view.session.model = Some(model.clone());
        }
        self.pending_leader = false;
        let _ = tx.send(Command::SetModel {
            session_id,
            model: Some(model),
        });
    }
}
