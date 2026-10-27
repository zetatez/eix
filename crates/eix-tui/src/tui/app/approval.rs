//! Approval cards, prompt/input requests and diff review.

use super::*;

impl App {
    /// Route keys owned by a blocking approval or question card. Returns true
    /// when the card consumed the key, so both prompt and transcript focus
    /// behave the same in every layout.
    pub(super) fn handle_card_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) -> bool {
        if self.focused_input().is_some() {
            // While editing the "Other" row, every printable key belongs to
            // the text field instead of the option picker.
            if self.focused_input().is_some_and(|pending| pending.editing) {
                match key.code {
                    KeyCode::Enter => {
                        self.commit_input_other(tx);
                        return true;
                    }
                    KeyCode::Esc => {
                        if let Some(pending) = self.pending_input_mut() {
                            pending.editing = false;
                        }
                        return true;
                    }
                    KeyCode::Backspace => {
                        if let Some(pending) = self.pending_input_mut() {
                            pending.other[pending.tab].pop();
                        }
                        return true;
                    }
                    KeyCode::Char(ch)
                        if !key.modifiers.contains(KeyModifiers::CONTROL)
                            && !key.modifiers.contains(KeyModifiers::ALT) =>
                    {
                        if let Some(pending) = self.pending_input_mut() {
                            pending.other[pending.tab].push(ch);
                        }
                        return true;
                    }
                    _ => return true,
                }
            }
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('n') | KeyCode::Char('j') => {
                        self.move_input_cursor(1);
                        return true;
                    }
                    KeyCode::Char('p') | KeyCode::Char('k') => {
                        self.move_input_cursor(-1);
                        return true;
                    }
                    _ => {}
                }
            }
            match key.code {
                KeyCode::Tab | KeyCode::Right => {
                    self.move_input_tab(1);
                    return true;
                }
                KeyCode::BackTab | KeyCode::Left => {
                    self.move_input_tab(-1);
                    return true;
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.move_input_cursor(1);
                    return true;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.move_input_cursor(-1);
                    return true;
                }
                KeyCode::Enter => {
                    self.commit_input_selection(tx);
                    return true;
                }
                KeyCode::Char(ch @ '1'..='9') => {
                    self.respond_input(ch as usize - '1' as usize, tx);
                    return true;
                }
                KeyCode::Char('x') => {
                    self.cancel(tx);
                    return true;
                }
                // Esc dismisses nothing: the question still blocks the turn,
                // and leaving focus in place avoids a dead, untouched card.
                KeyCode::Esc => return true,
                _ => {}
            }
        }
        if self.focused_approval().is_some() {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('n') | KeyCode::Char('j') => {
                        self.move_approval_cursor(1);
                        return true;
                    }
                    KeyCode::Char('p') | KeyCode::Char('k') => {
                        self.move_approval_cursor(-1);
                        return true;
                    }
                    _ => {}
                }
            }
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.move_approval_cursor(1);
                    return true;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.move_approval_cursor(-1);
                    return true;
                }
                KeyCode::Enter => {
                    self.activate_approval(None, tx);
                    return true;
                }
                KeyCode::Char(ch @ '1'..='9') => {
                    self.activate_approval(Some(ch as usize - '1' as usize), tx);
                    return true;
                }
                KeyCode::Char('y') => {
                    self.respond_approval(ApprovalDecision::Approve, tx);
                    return true;
                }
                KeyCode::Char('a') => {
                    self.respond_approval(ApprovalDecision::Always, tx);
                    return true;
                }
                KeyCode::Char('n') => {
                    self.respond_approval(ApprovalDecision::Deny, tx);
                    return true;
                }
                KeyCode::Char('d') => {
                    self.open_diff_view();
                    return true;
                }
                KeyCode::Char('e') => {
                    self.edit_proposed(tx);
                    return true;
                }
                KeyCode::Char('o') => {
                    self.open_approval_file();
                    return true;
                }
                KeyCode::Char('r') => {
                    self.start_reason_prompt();
                    return true;
                }
                KeyCode::Char('x') => {
                    self.cancel(tx);
                    return true;
                }
                KeyCode::Esc => return true,
                _ => {}
            }
        }
        false
    }

    pub(super) fn focused_approval(&self) -> Option<&PendingApproval> {
        let view = self.focused_view()?;
        self.approvals.get(&view.session.id)
    }

    /// Recompute the cached card for a session from its replayable log. The
    /// newest unresolved approval or staged patch is shown; resolving it
    /// reveals the next older one.
    pub(super) fn reload_pending(&mut self, session_id: &str) {
        let next = self
            .views
            .iter()
            .find(|view| view.session.id == session_id)
            .and_then(|view| {
                view.pending_approval().filter(|pending| {
                    pending.kind == PendingKind::Patch
                        || view.session.status == SessionStatus::AwaitingApproval
                })
            });
        let Some(mut next) = next else {
            self.approvals.remove(session_id);
            self.approval_rev = self.approval_rev.wrapping_add(1);
            self.close_diff_view_for(session_id);
            return;
        };
        // Keep an in-progress hunk selection and cursor when the same card is
        // reloaded.
        if let Some((selected, cursor)) = self
            .approvals
            .get(session_id)
            .filter(|pending| pending.approval_id == next.approval_id)
            .map(|pending| (pending.selected.clone(), pending.cursor))
        {
            next.selected = selected;
            next.cursor = cursor.min(next.actions().len().saturating_sub(1));
        }
        self.approvals.insert(session_id.to_string(), next);
        self.approval_rev = self.approval_rev.wrapping_add(1);
    }

    /// Recompute the cached question for a session from its replayable log.
    pub(super) fn reload_input(&mut self, session_id: &str) {
        let next = self
            .views
            .iter()
            .find(|view| view.session.id == session_id)
            .and_then(|view| {
                view.pending_input()
                    .filter(|_| view.session.status == SessionStatus::AwaitingInput)
            });
        let Some(mut next) = next else {
            self.inputs.remove(session_id);
            return;
        };
        // Keep tab, highlighted rows, typed text and committed answers when
        // the same request is reloaded.
        if let Some(previous) = self
            .inputs
            .get(session_id)
            .filter(|pending| pending.input_id == next.input_id)
        {
            if previous.questions.len() == next.questions.len() {
                next.tab = previous.tab.min(next.questions.len().saturating_sub(1));
                next.cursors = previous.cursors.clone();
                next.other = previous.other.clone();
                next.answers = previous.answers.clone();
                next.editing = previous.editing;
                for tab in 0..next.cursors.len() {
                    let rows = next.row_count(tab);
                    if rows > 0 {
                        next.cursors[tab] = next.cursors[tab].min(rows - 1);
                    }
                }
            }
        }
        self.inputs.insert(session_id.to_string(), next);
    }

    pub(super) fn focused_input(&self) -> Option<&PendingInput> {
        let view = self.focused_view()?;
        self.inputs.get(&view.session.id)
    }

    fn pending_input_mut(&mut self) -> Option<&mut PendingInput> {
        let session_id = self.focused_view()?.session.id.clone();
        self.inputs.get_mut(&session_id)
    }

    /// Switch to the next/previous question tab.
    pub(super) fn move_input_tab(&mut self, delta: isize) {
        let Some(pending) = self.pending_input_mut() else {
            return;
        };
        let len = pending.questions.len();
        if len == 0 {
            return;
        }
        pending.tab = (pending.tab as isize + delta).rem_euclid(len as isize) as usize;
        pending.editing = false;
    }

    /// Move the highlight in the focused question's row list. The last row is
    /// the synthetic "Other" entry.
    pub(super) fn move_input_cursor(&mut self, delta: isize) {
        let Some(pending) = self.pending_input_mut() else {
            return;
        };
        let len = pending.row_count(pending.tab);
        if len == 0 {
            return;
        }
        let cursor = &mut pending.cursors[pending.tab];
        *cursor = (*cursor as isize + delta).clamp(0, len as isize - 1) as usize;
        pending.editing = false;
    }

    /// Enter on the highlighted row: commit a normal option, or start editing
    /// the "Other" text field.
    pub(super) fn commit_input_selection(&mut self, tx: &UnboundedSender<Command>) {
        let Some(pending) = self.pending_input_mut() else {
            return;
        };
        let tab = pending.tab;
        if pending.is_other(tab) {
            pending.editing = true;
            return;
        }
        let question = &pending.questions[tab];
        let Some(option) = question.options.get(pending.cursors[tab]) else {
            return;
        };
        let mut answer = option.label.clone();
        if let Some(description) = option
            .description
            .as_deref()
            .map(str::trim)
            .filter(|description| !description.is_empty())
        {
            answer.push_str(" — ");
            answer.push_str(description);
        }
        pending.answers[tab] = Some(answer);
        if !pending.next_unanswered() {
            pending.editing = false;
        }
        self.submit_input_if_complete(tx);
    }

    /// Commit the free-form "Other" answer.
    pub(super) fn commit_input_other(&mut self, tx: &UnboundedSender<Command>) {
        let Some(pending) = self.pending_input_mut() else {
            return;
        };
        let tab = pending.tab;
        let text = pending.other[tab].trim().to_string();
        if text.is_empty() {
            pending.editing = false;
            return;
        }
        pending.answers[tab] = Some(text);
        pending.editing = false;
        if !pending.next_unanswered() {
            pending.editing = false;
        }
        self.submit_input_if_complete(tx);
    }

    /// Answer the focused question with `option_index` and advance.
    pub(super) fn respond_input(&mut self, option_index: usize, tx: &UnboundedSender<Command>) {
        let Some(pending) = self.pending_input_mut() else {
            return;
        };
        let tab = pending.tab;
        let rows = pending.row_count(tab).saturating_sub(1);
        if option_index >= rows {
            return;
        }
        pending.cursors[tab] = option_index;
        pending.editing = false;
        self.commit_input_selection(tx);
    }

    /// Send the complete answer map once every question has an answer.
    fn submit_input_if_complete(&mut self, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_view().map(|view| view.session.id.clone()) else {
            return;
        };
        let Some(pending) = self.inputs.get(&session_id) else {
            return;
        };
        if !pending.all_answered() {
            return;
        }
        let input_id = pending.input_id.clone();
        let answers = pending.answer_map();
        let count = answers.len();
        self.status = format!(
            "answered {count} question{}",
            if count == 1 { "" } else { "s" }
        );
        let _ = tx.send(Command::Input {
            session_id,
            input_id,
            answers,
        });
    }

    /// Move the highlight in the focused approval card's action list.
    pub(super) fn move_approval_cursor(&mut self, delta: isize) {
        let Some(session_id) = self.focused_view().map(|view| view.session.id.clone()) else {
            return;
        };
        let Some(pending) = self.approvals.get_mut(&session_id) else {
            return;
        };
        let len = pending.actions().len();
        if len == 0 {
            return;
        }
        pending.cursor = (pending.cursor as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    /// Run the action on the focused card's highlighted row (or `index`).
    pub(super) fn activate_approval(
        &mut self,
        index: Option<usize>,
        tx: &UnboundedSender<Command>,
    ) {
        let Some(pending) = self.focused_approval() else {
            return;
        };
        let actions = pending.actions();
        let index = index.unwrap_or(pending.cursor);
        let Some(action) = actions.get(index).copied() else {
            return;
        };
        match action {
            ApprovalAction::Approve => {
                self.respond_approval(ApprovalDecision::Approve, tx);
            }
            ApprovalAction::Always => {
                self.respond_approval(ApprovalDecision::Always, tx);
            }
            ApprovalAction::Deny => {
                self.respond_approval(ApprovalDecision::Deny, tx);
            }
            ApprovalAction::Reason => self.start_reason_prompt(),
            ApprovalAction::Review => self.open_diff_view(),
            ApprovalAction::Open => self.open_approval_file(),
            ApprovalAction::Edit => self.edit_proposed(tx),
        }
    }

    pub(super) fn respond_approval(
        &mut self,
        decision: ApprovalDecision,
        tx: &UnboundedSender<Command>,
    ) {
        let Some(session_id) = self.focused_view().map(|view| view.session.id.clone()) else {
            return;
        };
        self.respond_approval_for(&session_id, decision, None, None, tx);
    }

    pub(super) fn respond_approval_for(
        &mut self,
        session_id: &str,
        decision: ApprovalDecision,
        hunks: Option<Vec<usize>>,
        reason: Option<String>,
        tx: &UnboundedSender<Command>,
    ) {
        let Some(pending) = self.approvals.get(session_id) else {
            return;
        };
        let approval_id = pending.approval_id.clone();
        self.status = match decision {
            ApprovalDecision::Approve => "approving…",
            ApprovalDecision::Always => "allowing always…",
            ApprovalDecision::Deny => "denying…",
        }
        .into();
        let _ = tx.send(Command::Approval {
            session_id: session_id.to_string(),
            approval_id,
            decision,
            hunks,
            reason,
        });
    }

    pub(super) fn edit_proposed(&mut self, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_view().map(|view| view.session.id.clone()) else {
            return;
        };
        let Some(pending) = self.approvals.get(&session_id) else {
            return;
        };
        if pending.kind != PendingKind::Patch {
            self.status = "only staged writes can be edited before approval".into();
            return;
        }
        let patch_id = pending.approval_id.clone();
        self.status = "loading staged content…".into();
        let _ = tx.send(Command::PatchGet {
            session_id,
            patch_id,
        });
    }

    pub(super) fn open_approval_file(&mut self) {
        let Some(view) = self.focused_view() else {
            return;
        };
        let session_id = view.session.id.clone();
        let cwd = view.session.cwd.clone();
        let Some(pending) = self.approvals.get(&session_id) else {
            return;
        };
        let Some(path) = pending.path.clone() else {
            self.status = "this approval has no file to open".into();
            return;
        };
        let full = cwd.join(&path);
        if !full.exists() {
            self.status = format!("{} does not exist yet", full.display());
            return;
        }
        match run_editor(&full) {
            Ok(()) => self.status = format!("opened {path}"),
            Err(err) => self.status = format!("editor: {err:#}"),
        }
        self.force_clear = true;
    }

    pub(super) fn start_reason_prompt(&mut self) {
        let Some(session_id) = self.focused_view().map(|view| view.session.id.clone()) else {
            return;
        };
        self.reason_prompt = Some(ReasonPrompt {
            session_id,
            text: String::new(),
        });
        self.status = "reason for denial · Enter to deny · Esc to cancel".into();
    }

    /// Write the staged content to a temp file, let the user edit it, and send
    /// the result back to the daemon as a revised patch.
    pub(super) fn edit_patch(
        &mut self,
        session_id: &str,
        patch_id: &str,
        text: &str,
        tx: &UnboundedSender<Command>,
    ) {
        let path = std::env::temp_dir().join(format!("eix-{patch_id}.txt"));
        if let Err(err) = std::fs::write(&path, text) {
            self.status = format!("cannot write {}: {err}", path.display());
            return;
        }
        let outcome = run_editor(&path);
        self.force_clear = true;
        if let Err(err) = outcome {
            self.status = format!("editor: {err:#}");
            let _ = std::fs::remove_file(&path);
            return;
        }
        match std::fs::read_to_string(&path) {
            Ok(edited) if edited != text => {
                let _ = tx.send(Command::PatchRevise {
                    session_id: session_id.to_string(),
                    patch_id: patch_id.to_string(),
                    text: edited,
                });
                self.status = "updating the staged patch…".into();
            }
            Ok(_) => self.status = "no changes".into(),
            Err(err) => self.status = format!("cannot read the edited file: {err}"),
        }
        let _ = std::fs::remove_file(&path);
    }

    pub(super) fn open_diff_view(&mut self) {
        let Some(session_id) = self.focused_view().map(|view| view.session.id.clone()) else {
            return;
        };
        let (has_hunks, has_diff) = match self.approvals.get(&session_id) {
            Some(approval) => (
                !approval.hunks.is_empty(),
                approval
                    .diff
                    .as_deref()
                    .is_some_and(|diff| !diff.is_empty()),
            ),
            None => (false, false),
        };
        if !has_hunks && !has_diff {
            self.status = "no diff to review".into();
            return;
        }
        self.diff_view = Some(DiffView {
            session_id,
            scroll: 0,
            cursor: 0,
            width: 0,
        });
        self.status = if has_hunks {
            "diff review: j/k scroll · [ ] hunk · Space toggle · y apply · n deny".into()
        } else {
            "diff review: j/k scroll · y approve · n deny · Esc close".into()
        };
    }

    pub(super) fn handle_diff_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        let key = next_prev_key(key);
        let Some((session_id, scroll, cursor, width)) = self.diff_view.as_ref().map(|view| {
            (
                view.session_id.clone(),
                view.scroll,
                view.cursor,
                view.width,
            )
        }) else {
            return;
        };
        self.ensure_diff_rows(&session_id);
        let Some((total, offsets)) = self.approvals.get(&session_id).map(|approval| {
            let rows = self
                .diff_rows
                .as_ref()
                .map(|cache| cache.rows.as_slice())
                .unwrap_or(&[]);
            if crate::tui::diff::DiffRenderer::can_render(rows, width, "") {
                (rows.len(), crate::tui::diff::hunk_offsets(&approval.hunks))
            } else {
                (
                    review_unified_total(approval),
                    review_unified_hunk_offsets(approval),
                )
            }
        }) else {
            // The approval was answered elsewhere; drop the overlay.
            self.diff_view = None;
            return;
        };
        let hunk_count = offsets.len();
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('d') => self.diff_view = None,
            KeyCode::Char('y') => {
                let selection = self.approvals.get(&session_id).map(|approval| {
                    let selected: Vec<usize> = approval
                        .selected
                        .iter()
                        .enumerate()
                        .filter_map(|(index, &on)| on.then_some(index))
                        .collect();
                    (selected, approval.hunks.len())
                });
                match selection {
                    Some((selected, count)) if count > 0 && selected.is_empty() => {
                        self.status = "no hunks selected; press a to select all".into();
                    }
                    Some((selected, count)) => {
                        let partial = (selected.len() < count).then_some(selected);
                        self.diff_view = None;
                        self.respond_approval_for(
                            &session_id,
                            ApprovalDecision::Approve,
                            partial,
                            None,
                            tx,
                        );
                    }
                    None => self.diff_view = None,
                }
            }
            KeyCode::Char('n') => {
                self.diff_view = None;
                self.respond_approval_for(&session_id, ApprovalDecision::Deny, None, None, tx);
            }
            KeyCode::Char(' ') => {
                if let Some(approval) = self.approvals.get_mut(&session_id) {
                    if let Some(flag) = approval.selected.get_mut(cursor) {
                        *flag = !*flag;
                    }
                }
            }
            KeyCode::Char('a') => {
                if let Some(approval) = self.approvals.get_mut(&session_id) {
                    let all_on = approval.selected.iter().all(|&on| on);
                    approval
                        .selected
                        .iter_mut()
                        .for_each(|flag| *flag = !all_on);
                }
            }
            KeyCode::Char(']') | KeyCode::Char('J') if hunk_count > 0 => {
                let next = (cursor + 1) % hunk_count;
                self.set_diff_cursor(next, offsets[next], total);
            }
            KeyCode::Char('[') | KeyCode::Char('K') if hunk_count > 0 => {
                let prev = (cursor + hunk_count - 1) % hunk_count;
                self.set_diff_cursor(prev, offsets[prev], total);
            }
            KeyCode::Char('j') | KeyCode::Down => self.set_diff_scroll(scroll + 1, total),
            KeyCode::Char('k') | KeyCode::Up => {
                self.set_diff_scroll(scroll.saturating_sub(1), total)
            }
            KeyCode::PageDown => self.set_diff_scroll(scroll + 10, total),
            KeyCode::PageUp => self.set_diff_scroll(scroll.saturating_sub(10), total),
            KeyCode::Char('b') | KeyCode::Char('B')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.set_diff_scroll(scroll.saturating_sub(10), total)
            }
            KeyCode::Char('f') | KeyCode::Char('F')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                self.set_diff_scroll(scroll + 10, total)
            }
            KeyCode::Char('g') => self.set_diff_scroll(0, total),
            KeyCode::Char('G') => self.set_diff_scroll(usize::MAX, total),
            _ => {}
        }
    }

    pub(super) fn set_diff_scroll(&mut self, scroll: usize, total: usize) {
        if let Some(view) = &mut self.diff_view {
            view.scroll = scroll.min(total.saturating_sub(1));
        }
    }

    pub(super) fn set_diff_cursor(&mut self, cursor: usize, line: usize, total: usize) {
        if let Some(view) = &mut self.diff_view {
            view.cursor = cursor;
            view.scroll = line.min(total.saturating_sub(1));
        }
    }

    /// Close the review overlay if it is showing `session_id`.
    pub(super) fn close_diff_view_for(&mut self, session_id: &str) {
        if self
            .diff_view
            .as_ref()
            .is_some_and(|view| view.session_id == session_id)
        {
            self.diff_view = None;
        }
    }
}
