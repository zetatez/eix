//! Transcript-mode key overrides.
//!
//! Only actions that are safe outside text entry can be rebound. The prompt
//! keeps its vim/insert behavior, so a binding never swallows typed text.

use std::collections::BTreeMap;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Quit,
    Help,
    NewSession,
    NewWorktree,
    Sessions,
    NewPane,
    Palette,
    Models,
    Worktrees,
    Branches,
    Processes,
    Inbox,
    Search,
    Fork,
    Attention,
    Cancel,
    NextSession,
    PrevSession,
    ScrollDown,
    ScrollUp,
    Timeline,
    Todos,
    Undo,
    Redo,
}

impl KeyAction {
    pub fn parse(name: &str) -> Option<Self> {
        Some(
            match name.trim().to_ascii_lowercase().replace('-', "_").as_str() {
                "quit" => Self::Quit,
                "help" => Self::Help,
                "new_session" | "new" => Self::NewSession,
                "new_worktree" => Self::NewWorktree,
                "sessions" => Self::Sessions,
                "new_pane" => Self::NewPane,
                "palette" => Self::Palette,
                "model" | "models" => Self::Models,
                "worktrees" => Self::Worktrees,
                "branches" => Self::Branches,
                "processes" => Self::Processes,
                "inbox" | "approvals" => Self::Inbox,
                "search" | "history" => Self::Search,
                "fork" => Self::Fork,
                "attention" => Self::Attention,
                "cancel" => Self::Cancel,
                "next_session" => Self::NextSession,
                "prev_session" => Self::PrevSession,
                "scroll_down" => Self::ScrollDown,
                "scroll_up" => Self::ScrollUp,
                "timeline" => Self::Timeline,
                "todos" => Self::Todos,
                "undo" => Self::Undo,
                "redo" => Self::Redo,
                _ => return None,
            },
        )
    }
}

#[derive(Debug, Clone, Default)]
pub struct Keybinds {
    bindings: Vec<(KeyEvent, KeyAction)>,
}

impl Keybinds {
    pub fn from_config(config: &BTreeMap<String, String>) -> Self {
        let mut bindings = Vec::new();
        for (name, spec) in config {
            let Some(action) = KeyAction::parse(name) else {
                continue;
            };
            let Some(key) = parse_key(spec) else {
                continue;
            };
            // Outside the prompt a bare character key is too easy to trigger
            // by accident. TUI actions must use a leader chord, a named key or
            // a modified/shifted punctuation key.
            if !is_safe_binding(key) {
                continue;
            }
            bindings.push((key, action));
        }
        Self { bindings }
    }

    pub fn action(&self, key: KeyEvent) -> Option<KeyAction> {
        self.bindings
            .iter()
            .find(|(bound, _)| {
                if bound.code != key.code {
                    return false;
                }
                if bound.modifiers == key.modifiers {
                    return true;
                }
                // Terminals disagree on whether shifted punctuation (`?`,
                // `:`, ...) carries SHIFT. Treat it as optional there, but keep
                // it significant for letters and named keys.
                bound.modifiers == key.modifiers.difference(KeyModifiers::SHIFT)
                    && matches!(key.code, KeyCode::Char(ch) if !ch.is_ascii_uppercase())
                    && key.modifiers.contains(KeyModifiers::SHIFT)
            })
            .map(|(_, action)| *action)
    }
}

/// Whether a configured binding is safe outside text entry. Plain letters are
/// reserved for input editing/navigation; actions need a modifier, a named
/// key, or punctuation such as `?` / `:`.
fn is_safe_binding(key: KeyEvent) -> bool {
    match key.code {
        KeyCode::Char(ch) => {
            key.modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
                || (!ch.is_ascii_alphanumeric() && !ch.is_whitespace())
        }
        _ => true,
    }
}

/// Parse `C-x`, `S-Tab`, `A-Enter`, `Space`, `F2`, or a single character.
pub fn parse_key(spec: &str) -> Option<KeyEvent> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    let mut modifiers = KeyModifiers::NONE;
    let mut parts: Vec<&str> = spec.split('-').collect();
    while parts.len() > 1 {
        let modifier = parts.remove(0);
        match modifier.to_ascii_lowercase().as_str() {
            "c" | "ctrl" => modifiers |= KeyModifiers::CONTROL,
            "s" | "shift" => modifiers |= KeyModifiers::SHIFT,
            "a" | "alt" | "m" => modifiers |= KeyModifiers::ALT,
            _ => return None,
        }
    }
    let token = parts[0];
    let code = match token.to_ascii_lowercase().as_str() {
        "space" => KeyCode::Char(' '),
        "tab" => KeyCode::Tab,
        "backtab" | "btab" => KeyCode::BackTab,
        "enter" | "return" => KeyCode::Enter,
        "esc" | "escape" => KeyCode::Esc,
        "backspace" => KeyCode::Backspace,
        "delete" | "del" => KeyCode::Delete,
        "insert" => KeyCode::Insert,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        _ => {
            let lower = token.to_ascii_lowercase();
            if let Some(number) = lower.strip_prefix('f') {
                if let Ok(number) = number.parse::<u8>() {
                    if (1..=12).contains(&number) {
                        return Some(KeyEvent::new(KeyCode::F(number), modifiers));
                    }
                }
            }
            let mut chars = token.chars();
            let ch = chars.next()?;
            if chars.next().is_some() {
                return None;
            }
            if ch.is_ascii_uppercase() && !modifiers.contains(KeyModifiers::SHIFT) {
                modifiers |= KeyModifiers::SHIFT;
            }
            KeyCode::Char(ch)
        }
    };
    Some(KeyEvent::new(code, modifiers))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_modifiers_and_named_keys() {
        assert_eq!(
            parse_key("q"),
            Some(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key("C-x"),
            Some(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_key("S-Tab"),
            Some(KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT))
        );
        assert_eq!(
            parse_key("F1"),
            Some(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key("C-/"),
            Some(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL))
        );
        assert_eq!(parse_key("not-a-key"), None);
    }

    #[test]
    fn config_actions_win_over_defaults_by_lookup() {
        let mut map = BTreeMap::new();
        map.insert("help".to_string(), "F2".to_string());
        map.insert("nonsense".to_string(), "z".to_string());
        let binds = Keybinds::from_config(&map);
        assert_eq!(
            binds.action(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)),
            Some(KeyAction::Help)
        );
        assert_eq!(
            binds.action(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE)),
            None
        );
        map.insert("help".into(), "?".into());
        let binds = Keybinds::from_config(&map);
        assert_eq!(
            binds.action(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT)),
            Some(KeyAction::Help)
        );
    }

    #[test]
    fn model_binding_accepts_ctrl_slash() {
        let mut map = BTreeMap::new();
        map.insert("model".to_string(), "C-/".to_string());
        let binds = Keybinds::from_config(&map);
        assert_eq!(
            binds.action(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL)),
            Some(KeyAction::Models)
        );
    }

    #[test]
    fn bare_letter_bindings_are_ignored_outside_the_prompt() {
        let mut map = BTreeMap::new();
        map.insert("sessions".to_string(), "o".to_string());
        map.insert("new_session".to_string(), "P".to_string());
        map.insert("palette".to_string(), "C-o".to_string());
        let binds = Keybinds::from_config(&map);

        assert_eq!(
            binds.action(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            binds.action(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT)),
            None
        );
        assert_eq!(
            binds.action(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some(KeyAction::Palette)
        );
    }
}
