//! Help and keymap overlay.

use super::*;

impl App {
    /// Open the help overlay with a fresh filter and selection.
    pub(super) fn open_help(&mut self) {
        self.open_help_scoped(None, "");
    }

    /// Open only the key map. Used by the key-map action and palette, and keeps
    /// the same interactive filter/selection as the full help overlay.
    pub(super) fn open_keys_help(&mut self, query: &str) {
        self.open_help_scoped(Some("keys"), query);
    }

    pub(super) fn open_help_scoped(&mut self, scope: Option<&'static str>, query: &str) {
        self.pending_leader = false;
        self.help = true;
        self.help_scope = scope;
        self.help_query = query.to_string();
        self.help_selected = 0;
    }

    /// Keys while help is open: typing filters, arrows/C-n/C-p select, Esc
    /// closes. Plain `q`/`j`/`k` are search input rather than shortcuts so any
    /// key entry can be found by name.
    pub(super) fn handle_help_key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => {
                self.help = false;
                self.help_scope = None;
            }
            KeyCode::Char('u') if control => {
                self.help_query.clear();
                self.help_selected = 0;
            }
            KeyCode::Backspace => {
                self.help_query.pop();
                self.help_selected = 0;
            }
            KeyCode::Up | KeyCode::PageUp => {
                let step = if key.code == KeyCode::PageUp { 8 } else { 1 };
                self.move_help_selection(-step);
            }
            KeyCode::Down | KeyCode::PageDown => {
                let step = if key.code == KeyCode::PageDown { 8 } else { 1 };
                self.move_help_selection(step);
            }
            KeyCode::Char('b') | KeyCode::Char('B') if control => self.move_help_selection(-8),
            KeyCode::Char('f') | KeyCode::Char('F') if control => self.move_help_selection(8),
            KeyCode::Char('k') | KeyCode::Char('p') if control => self.move_help_selection(-1),
            KeyCode::Char('j') | KeyCode::Char('n') if control => self.move_help_selection(1),
            // Terminals without kitty keyboard disambiguation report Ctrl-J as
            // Ctrl-Enter, so keep it as "next" here too.
            KeyCode::Enter if control => self.move_help_selection(1),
            KeyCode::Home => self.help_selected = 0,
            KeyCode::End => {
                self.help_selected =
                    help_entry_count_in(&self.help_query, self.help_scope).saturating_sub(1);
            }
            KeyCode::Char(ch) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.help_query.push(ch);
                self.help_selected = 0;
            }
            _ => {}
        }
    }

    pub(super) fn move_help_selection(&mut self, delta: isize) {
        let count = help_entry_count_in(&self.help_query, self.help_scope);
        if count == 0 {
            self.help_selected = 0;
            return;
        }
        if delta.unsigned_abs() <= 1 {
            // Single steps cycle, matching the pickers.
            self.help_selected = wrap_step(self.help_selected, count, delta);
        } else {
            let selected = self.help_selected.min(count - 1) as isize;
            self.help_selected = (selected + delta).clamp(0, count as isize - 1) as usize;
        }
    }
}
