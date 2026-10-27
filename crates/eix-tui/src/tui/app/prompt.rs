//! Prompt editing, history, slash-command menu and cmdline.

use super::*;

impl App {
    /// Merge a page of older stored events and keep the viewport anchored on
    /// the content it was showing before the prepend.
    pub(super) fn history_page(
        &mut self,
        session_id: String,
        events: Vec<SessionEvent>,
        has_more: bool,
    ) {
        let Some(view) = self
            .views
            .iter_mut()
            .find(|view| view.session.id == session_id)
        else {
            return;
        };
        view.history_loading = false;
        view.history_has_more = has_more;
        view.history_armed = false;
        let Some(first_seq) = view.events.first().map(|event| event.seq) else {
            view.history_has_more = false;
            return;
        };
        let older: Vec<SessionEvent> = events
            .into_iter()
            .filter(|event| event.seq < first_seq)
            .collect();
        if older.is_empty() {
            // An empty page means there is nothing further back, even if the
            // daemon's count was off.
            view.history_has_more = false;
            return;
        }
        if !view.follow {
            if let Some(cache) = &view.history_cache {
                let anchor = (cache.lines.len(), view.scroll);
                view.history_anchor.get_or_insert(anchor);
            }
        }
        view.events.splice(0..0, older);
        view.events_rev = view.events_rev.wrapping_add(1);
    }

    /// True when the prompt is a partial `/command` and the suggestion list
    /// should be visible.
    pub(super) fn command_menu_open(&self) -> bool {
        if self.command_dismissed || self.input.mode() != Mode::Insert {
            return false;
        }
        let text = self.input.text();
        let Some(rest) = text.strip_prefix('/') else {
            return false;
        };
        !rest.contains(char::is_whitespace) && !rest.contains('\n')
    }

    pub(super) fn command_matches(&self) -> Vec<&CommandSpec> {
        let text = self.input.text();
        let Some(prefix) = text.strip_prefix('/') else {
            return Vec::new();
        };
        let prefix = prefix.to_ascii_lowercase();
        // A custom command with the old spelling wins over the built-in alias.
        if !self
            .commands
            .iter()
            .any(|spec| spec.name.eq_ignore_ascii_case(&prefix))
        {
            if let Some(canonical) = command_alias(&prefix) {
                return self
                    .commands
                    .iter()
                    .filter(|spec| spec.name == canonical)
                    .collect();
            }
        }
        self.commands
            .iter()
            .filter(|spec| spec.name.starts_with(&prefix))
            .collect()
    }

    pub(super) fn selected_command(&self) -> Option<&CommandSpec> {
        let matches = self.command_matches();
        if matches.is_empty() {
            return None;
        }
        let text = self.input.text();
        let typed = text.strip_prefix('/').unwrap_or("").trim();
        // A complete command name must not be hijacked by a longer prefix
        // match (`/perm` before `/permissions`, `/mode` alias before `/model`).
        if let Some(spec) = matches
            .iter()
            .find(|spec| spec.name.eq_ignore_ascii_case(typed))
        {
            return Some(*spec);
        }
        matches
            .get(self.command_cursor.min(matches.len() - 1))
            .copied()
    }

    pub(super) fn move_command_cursor(&mut self, delta: isize) {
        let len = self.command_matches().len();
        if len == 0 {
            self.command_cursor = 0;
            return;
        }
        let current = self.command_cursor.min(len - 1) as isize;
        self.command_cursor = (current + delta).rem_euclid(len as isize) as usize;
    }

    pub(super) fn complete_command(&mut self) {
        let Some(spec) = self.selected_command() else {
            return;
        };
        let text = if spec.args.is_empty() {
            format!("/{}", spec.name)
        } else {
            format!("/{} ", spec.name)
        };
        self.input.set_text(&text);
        self.command_cursor = 0;
    }

    /// Whether the `:` command line should show its dynamic command list.
    pub(super) fn cmdline_menu_open(&self) -> bool {
        self.cmdline_menu_armed
            && self.cmdline.as_ref().is_some_and(|cmdline| {
                cmdline.prompt == ':' && !cmdline.text.contains(char::is_whitespace)
            })
    }

    pub(super) fn cmdline_matches(&self) -> Vec<&CommandSpec> {
        let Some(cmdline) = self.cmdline.as_ref() else {
            return Vec::new();
        };
        if cmdline.prompt != ':' {
            return Vec::new();
        }
        let text = self.cmdline_menu_prefix.as_deref().unwrap_or(&cmdline.text);
        if text.contains(char::is_whitespace) {
            return Vec::new();
        }
        let prefix = text.trim_start_matches(':').to_ascii_lowercase();
        if !self
            .commands
            .iter()
            .any(|spec| spec.name.eq_ignore_ascii_case(&prefix))
        {
            if let Some(canonical) = command_alias(&prefix) {
                return self
                    .commands
                    .iter()
                    .filter(|spec| spec.name == canonical)
                    .collect();
            }
        }
        self.commands
            .iter()
            .filter(|spec| spec.name.to_ascii_lowercase().starts_with(&prefix))
            .collect()
    }

    pub(super) fn selected_cmdline_match(&self) -> Option<&CommandSpec> {
        let matches = self.cmdline_matches();
        if matches.is_empty() {
            return None;
        }
        let cursor = self.cmdline_menu_cursor.unwrap_or(0).min(matches.len() - 1);
        matches.get(cursor).copied()
    }

    pub(super) fn move_cmdline_menu(&mut self, delta: isize) -> bool {
        let len = self.cmdline_matches().len();
        if len == 0 {
            return false;
        }
        if len == 1 {
            return true;
        }
        let current = self.cmdline_menu_cursor.unwrap_or(0).min(len - 1);
        self.cmdline_menu_cursor =
            Some((current as isize + delta).rem_euclid(len as isize) as usize);
        true
    }

    /// Drop the menu's filter snapshot and explicit row. Called whenever the
    /// command line text changes so the menu filters from the new input.
    pub(super) fn reset_cmdline_menu_selection(&mut self) {
        self.cmdline_menu_prefix = None;
        self.cmdline_menu_cursor = None;
    }

    pub(super) fn cmdline_name_exists(&self, name: &str) -> bool {
        self.commands.iter().any(|spec| spec.name == name)
            || ["q", "qa", "qall", "quitall", "w", "wq", "x"].contains(&name)
    }

    pub(super) fn open_cmdline(&mut self) {
        self.open_cmdline_with(':');
    }

    pub(super) fn open_cmdline_with(&mut self, prompt: char) {
        self.cmdline = Some(CmdLine::new(prompt));
        self.cmdline_menu_armed = false;
        self.cmdline_menu_prefix = None;
        self.cmdline_menu_cursor = None;
        self.cmdline_history_index = None;
        self.cmdline_history_draft.clear();
        self.pending_leader = false;
        self.esc_at = None;
        self.command_dismissed = true;
    }

    pub(super) fn close_cmdline(&mut self) {
        self.cmdline = None;
        self.esc_at = None;
        self.cmdline_menu_armed = false;
        self.cmdline_menu_prefix = None;
        self.cmdline_menu_cursor = None;
        self.cmdline_history_index = None;
        self.cmdline_history_draft.clear();
    }

    pub(super) fn handle_cmdline_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                // The menu is a transient overlay: Esc dismisses it first and
                // leaves the command-line text untouched. A second Esc closes
                // the command line itself.
                if self.cmdline_menu_open() && !self.cmdline_matches().is_empty() {
                    self.cmdline_menu_armed = false;
                    self.reset_cmdline_menu_selection();
                } else {
                    self.close_cmdline();
                }
            }
            // Terminals without kitty keyboard disambiguation report Ctrl-J
            // as Ctrl-Enter, so accept both spellings.
            KeyCode::Enter if ctrl => {
                if self.cmdline_menu_open() {
                    self.move_cmdline_menu(1);
                } else {
                    self.submit_cmdline(tx);
                }
            }
            KeyCode::Enter => self.submit_cmdline(tx),
            KeyCode::Backspace => {
                if let Some(cmdline) = self.cmdline.as_mut() {
                    cmdline.backspace();
                }
                self.reset_cmdline_menu_selection();
            }
            KeyCode::Delete => {
                if let Some(cmdline) = self.cmdline.as_mut() {
                    cmdline.delete();
                }
                self.reset_cmdline_menu_selection();
            }
            KeyCode::Left => {
                if let Some(cmdline) = self.cmdline.as_mut() {
                    cmdline.left();
                }
            }
            KeyCode::Right => {
                if let Some(cmdline) = self.cmdline.as_mut() {
                    cmdline.right();
                }
            }
            KeyCode::Home => {
                if let Some(cmdline) = self.cmdline.as_mut() {
                    cmdline.home();
                }
            }
            KeyCode::End => {
                if let Some(cmdline) = self.cmdline.as_mut() {
                    cmdline.end();
                }
            }
            KeyCode::Up => self.cmdline_history_prev(),
            KeyCode::Down => self.cmdline_history_next(),
            KeyCode::Tab => {
                // Only the `:` command line has a command menu. `/`, `?` and
                // `#` are transcript searches, so Tab must not complete command
                // names into their query.
                if self
                    .cmdline
                    .as_ref()
                    .is_some_and(|cmdline| cmdline.prompt == ':')
                {
                    self.cmdline_menu_armed = true;
                    if self.cmdline_menu_prefix.is_none() {
                        if let Some(text) =
                            self.cmdline.as_ref().map(|cmdline| cmdline.text.clone())
                        {
                            if !text.contains(char::is_whitespace) {
                                self.cmdline_menu_prefix = Some(text);
                                self.cmdline_menu_cursor = None;
                            }
                        }
                    }
                    self.complete_cmdline();
                }
            }
            KeyCode::Char('j') if ctrl => {
                if self.cmdline_menu_open() {
                    self.move_cmdline_menu(1);
                }
            }
            KeyCode::Char('k') if ctrl => {
                if !(self.cmdline_menu_open() && self.move_cmdline_menu(-1)) {
                    if let Some(cmdline) = self.cmdline.as_mut() {
                        cmdline.kill_to_end();
                    }
                    self.reset_cmdline_menu_selection();
                }
            }
            KeyCode::Char('p') if ctrl => {
                if !(self.cmdline_menu_open() && self.move_cmdline_menu(-1)) {
                    self.cmdline_history_prev();
                }
            }
            KeyCode::Char('n') if ctrl => {
                if !(self.cmdline_menu_open() && self.move_cmdline_menu(1)) {
                    self.cmdline_history_next();
                }
            }
            KeyCode::Char('b') if ctrl => {
                if let Some(cmdline) = self.cmdline.as_mut() {
                    cmdline.left();
                }
            }
            KeyCode::Char('f') if ctrl => {
                if let Some(cmdline) = self.cmdline.as_mut() {
                    cmdline.right();
                }
            }
            KeyCode::Char('h') if ctrl => {
                if let Some(cmdline) = self.cmdline.as_mut() {
                    cmdline.backspace();
                }
                self.reset_cmdline_menu_selection();
            }
            KeyCode::Char('a') if ctrl => {
                if let Some(cmdline) = self.cmdline.as_mut() {
                    cmdline.home();
                }
            }
            KeyCode::Char('e') if ctrl => {
                if let Some(cmdline) = self.cmdline.as_mut() {
                    cmdline.end();
                }
            }
            KeyCode::Char('u') if ctrl => {
                if let Some(cmdline) = self.cmdline.as_mut() {
                    cmdline.kill_to_start();
                }
                self.reset_cmdline_menu_selection();
            }
            KeyCode::Char('w') if ctrl => {
                if let Some(cmdline) = self.cmdline.as_mut() {
                    cmdline.kill_word();
                }
                self.reset_cmdline_menu_selection();
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if let Some(cmdline) = self.cmdline.as_mut() {
                    cmdline.insert_char(ch);
                }
                self.reset_cmdline_menu_selection();
            }
            _ => {}
        }
    }

    pub(super) fn cmdline_history_prev(&mut self) {
        if self.cmdline_history.is_empty() {
            return;
        }
        let current = self
            .cmdline
            .as_ref()
            .map(|cmdline| cmdline.text.clone())
            .unwrap_or_default();
        let next = match self.cmdline_history_index {
            None => {
                self.cmdline_history_draft = current;
                0
            }
            Some(index) => (index + 1).min(self.cmdline_history.len() - 1),
        };
        self.cmdline_history_index = Some(next);
        let text = self.cmdline_history[self.cmdline_history.len() - 1 - next].clone();
        if let Some(cmdline) = self.cmdline.as_mut() {
            cmdline.set_text(text);
        }
        self.reset_cmdline_menu_selection();
    }

    pub(super) fn cmdline_history_next(&mut self) {
        let Some(index) = self.cmdline_history_index else {
            return;
        };
        if index == 0 {
            self.cmdline_history_index = None;
            let draft = std::mem::take(&mut self.cmdline_history_draft);
            if let Some(cmdline) = self.cmdline.as_mut() {
                cmdline.set_text(draft);
            }
            self.reset_cmdline_menu_selection();
            return;
        }
        let next = index - 1;
        self.cmdline_history_index = Some(next);
        let text = self.cmdline_history[self.cmdline_history.len() - 1 - next].clone();
        if let Some(cmdline) = self.cmdline.as_mut() {
            cmdline.set_text(text);
        }
        self.reset_cmdline_menu_selection();
    }

    pub(super) fn complete_cmdline(&mut self) {
        let Some(name) = self.selected_cmdline_match().map(|spec| spec.name.clone()) else {
            return;
        };
        if self.cmdline_menu_cursor.is_none() {
            let Some(cmdline) = self.cmdline.as_ref() else {
                return;
            };
            let text = cmdline.text.trim_start_matches(':');
            // Preserve the old Tab behavior: an empty line stays empty, and an
            // already-complete command (including `:q` aliases) is not
            // rewritten to a different command.
            if text.is_empty() || self.cmdline_name_exists(text) {
                return;
            }
        }
        if let Some(cmdline) = self.cmdline.as_mut() {
            cmdline.set_text(name);
        }
    }

    pub(super) fn remember_cmdline(&mut self, text: &str) {
        if self.cmdline_history.last().is_some_and(|last| last == text) {
            return;
        }
        self.cmdline_history.push(text.to_string());
        if self.cmdline_history.len() > CMDLINE_HISTORY_LIMIT {
            self.cmdline_history.remove(0);
        }
    }

    pub(super) fn run_cmdline_shell(
        &mut self,
        command: &str,
        insert_output: bool,
        tx: &UnboundedSender<Command>,
    ) {
        let Some(session_id) = self.focused_session_id() else {
            self.status = "no session yet".into();
            return;
        };
        let command = command.trim();
        if command.is_empty() {
            self.status = if insert_output {
                "usage: :r!<shell command>".into()
            } else {
                "usage: :!<shell command>".into()
            };
            return;
        }
        self.error = None;
        self.pending_leader = false;
        let _ = tx.send(Command::RunShell {
            session_id,
            command: command.to_string(),
            insert_output,
        });
        self.status = if insert_output {
            "reading shell output…".into()
        } else {
            "running shell command…".into()
        };
    }

    pub(super) fn submit_cmdline(&mut self, tx: &UnboundedSender<Command>) {
        // Ctrl-J/K (and Ctrl-N/P) move a separate menu cursor; accept that
        // row without rewriting the command line on every keypress.
        let selected = if self.cmdline_menu_cursor.is_some() && self.cmdline_menu_open() {
            self.selected_cmdline_match().map(|spec| spec.name.clone())
        } else {
            None
        };
        let Some(cmdline) = self.cmdline.take() else {
            return;
        };
        self.cmdline_menu_armed = false;
        self.cmdline_menu_prefix = None;
        self.cmdline_menu_cursor = None;
        self.cmdline_history_index = None;
        self.cmdline_history_draft.clear();
        let prompt = cmdline.prompt;
        let raw = selected.unwrap_or_else(|| cmdline.text.trim().to_string());
        if matches!(prompt, '/' | '?' | '#') {
            self.search_transcript(&raw, prompt == '/');
            return;
        }
        if raw.is_empty() {
            return;
        }
        self.remember_cmdline(&raw);
        let raw = raw.trim_start_matches(':').trim();
        // Vim-style shell commands. `:!cmd` executes directly; `:r!cmd` (and
        // the spaced `:r !cmd`) inserts the command's output into the prompt.
        if let Some(command) = raw.strip_prefix('!') {
            self.run_cmdline_shell(command, false, tx);
            return;
        }
        if let Some(command) = raw.strip_prefix("r!").or_else(|| raw.strip_prefix("r !")) {
            self.run_cmdline_shell(command, true, tx);
            return;
        }
        let (name, args) = raw
            .split_once(char::is_whitespace)
            .map(|(name, args)| (name, args.trim()))
            .unwrap_or((raw, ""));
        let name = name.trim().trim_start_matches(':');
        if name.is_empty() {
            return;
        }
        // Vim-style aliases. eix auto-saves drafts, so `:w` is just an
        // explicit sync and `:wq`/`:x` close the pane after saving.
        let base = name.strip_suffix('!').unwrap_or(name);
        match base {
            "q" => {
                self.handle_command_text("/close", tx, false);
                self.input.enter_normal();
            }
            "qa" | "qall" | "quitall" => {
                self.handle_command_text("/quit", tx, false);
                if self.should_quit {
                    self.clear_active_on_quit = true;
                }
            }
            "w" => {
                self.save_draft_now(tx);
                self.input.enter_normal();
            }
            "wq" | "x" => {
                self.handle_command_text("/close", tx, false);
                self.input.enter_normal();
            }
            _ => {
                let line = if args.is_empty() {
                    format!("/{name}")
                } else {
                    format!("/{name} {args}")
                };
                self.handle_command_text(&line, tx, false);
            }
        }
    }

    /// Apply a `delta` step to prompt-history browsing. Negative is older.
    pub(super) fn navigate_history(&mut self, delta: isize) {
        let entries = self.history_entries();
        if entries.is_empty() {
            self.status = "no earlier prompts in this session".into();
            return;
        }
        let next = match (self.history_index, delta < 0) {
            (None, true) => {
                self.history_draft = self.input.text();
                Some(0)
            }
            (None, false) => return,
            (Some(0), false) => None,
            (Some(index), true) => Some((index + 1).min(entries.len() - 1)),
            (Some(index), false) => Some(index.saturating_sub(1)),
        };
        // The focused session may have changed while an older cursor was live.
        let next = next.map(|index| index.min(entries.len() - 1));
        self.history_index = next;
        let text = match next {
            Some(index) => entries[entries.len() - 1 - index].clone(),
            None => std::mem::take(&mut self.history_draft),
        };
        self.input.set_text(&text);
        self.input.enter_normal();
        let position = match next {
            Some(index) => format!("history {}/{}", index + 1, entries.len()),
            None => "draft".to_string(),
        };
        self.status = position;
    }

    /// Submitted prompts for the focused session, oldest first. The event log
    /// is scanned once per revision; Ctrl-J/Ctrl-K reuse the cached list.
    pub(super) fn history_entries(&mut self) -> Rc<Vec<String>> {
        let Some((session_id, events_rev)) = self
            .focused_view()
            .map(|view| (view.session.id.clone(), view.events_rev))
        else {
            return Rc::new(Vec::new());
        };
        if let Some((cached_id, cached_rev, entries)) = &self.history_entries {
            if *cached_id == session_id && *cached_rev == events_rev {
                return entries.clone();
            }
        }
        let entries: Vec<String> = self
            .focused_view()
            .map(|view| {
                view.events
                    .iter()
                    .filter(|event| event.kind == EventKind::UserMessage)
                    .filter_map(|event| {
                        UserMessagePayload::deserialize(&event.payload)
                            .ok()
                            .map(|payload| payload.text)
                    })
                    .collect()
            })
            .unwrap_or_default();
        let entries = Rc::new(entries);
        self.history_entries = Some((session_id, events_rev, entries.clone()));
        entries
    }

    pub(super) fn reset_history(&mut self) {
        self.history_index = None;
        self.history_draft.clear();
    }

    /// Put `text` at the prompt cursor, using the embedded prompt when present.
    pub(super) fn insert_prompt(&mut self, text: &str) {
        self.input.insert_text(text);
        self.focus = Focus::Input;
        self.status = format!("inserted {text}");
    }
}
