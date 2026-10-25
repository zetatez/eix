//! Configurable global/TUI keymap.
//!
//! Vim editing keys stay in `editor.rs`. This module owns the actions the host
//! dispatches itself: global shortcuts, normal/transcript actions, and
//! `<leader>` chords. Overlay-internal navigation remains hardcoded.

use std::collections::{BTreeMap, BTreeSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeyAction {
    Quit,
    Help,
    NewSession,
    NewWorktree,
    Sessions,
    NewPane,
    Palette,
    Files,
    Grep,
    Models,
    Git,
    Processes,
    Inbox,
    Fork,
    Attention,
    Cancel,
    NextSession,
    PrevSession,
    PaneNext,
    PanePrev,
    ScrollDown,
    ScrollUp,
    PageDown,
    PageUp,
    Timeline,
    Todos,
    Undo,
    Redo,
    ClosePane,
    ModePicker,
    LayoutHorizontal,
    LayoutTabby,
    LayoutFibonacci,
    LayoutCycle,
    CycleMode,
    CycleModeBack,
    CycleReasoning,
    FocusTab(u8),
}

impl KeyAction {
    pub fn parse(name: &str) -> Option<Self> {
        let name = name.trim().to_ascii_lowercase().replace('-', "_");
        if let Some(index) = name
            .strip_prefix("tab_")
            .or_else(|| name.strip_prefix("focus_tab_"))
        {
            let index = index.parse::<u8>().ok()?;
            return (1..=9).contains(&index).then_some(Self::FocusTab(index));
        }
        Some(match name.as_str() {
            "quit" => Self::Quit,
            "help" => Self::Help,
            "new_session" | "new" => Self::NewSession,
            "new_worktree" => Self::NewWorktree,
            "sessions" => Self::Sessions,
            "new_pane" => Self::NewPane,
            "palette" | "commands" => Self::Palette,
            "files" => Self::Files,
            "grep" => Self::Grep,
            "model" | "models" => Self::Models,
            "git" | "worktrees" | "branches" => Self::Git,
            "processes" | "ps" => Self::Processes,
            "inbox" | "approvals" => Self::Inbox,
            "fork" => Self::Fork,
            "attention" => Self::Attention,
            "cancel" => Self::Cancel,
            "next_session" => Self::NextSession,
            "prev_session" => Self::PrevSession,
            "pane_next" | "next_pane" => Self::PaneNext,
            "pane_prev" | "prev_pane" => Self::PanePrev,
            "scroll_down" => Self::ScrollDown,
            "scroll_up" => Self::ScrollUp,
            "page_down" => Self::PageDown,
            "page_up" => Self::PageUp,
            "timeline" => Self::Timeline,
            "todos" => Self::Todos,
            "undo" => Self::Undo,
            "redo" => Self::Redo,
            "close_pane" => Self::ClosePane,
            "mode" | "perm" => Self::ModePicker,
            "layout_horizontal" => Self::LayoutHorizontal,
            "layout_tabby" => Self::LayoutTabby,
            "layout_fibonacci" => Self::LayoutFibonacci,
            "layout_cycle" => Self::LayoutCycle,
            "cycle_mode" | "toggle_mode" => Self::CycleMode,
            "cycle_mode_back" => Self::CycleModeBack,
            "cycle_reasoning" | "reasoning" => Self::CycleReasoning,
            _ => return None,
        })
    }

    /// Actions that must keep working while the prompt is in insert mode need
    /// a global binding. Everything else is safe to expose in normal/transcript
    /// mode only.
    fn primary_scope(self) -> KeyScope {
        match self {
            Self::Quit
            | Self::Help
            | Self::Sessions
            | Self::Models
            | Self::Timeline
            | Self::CycleReasoning
            | Self::FocusTab(_) => KeyScope::Global,
            _ => KeyScope::Normal,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyScope {
    Global,
    Normal,
}

#[derive(Debug, Clone, Copy)]
struct Binding {
    action: KeyAction,
    key: KeyEvent,
    scope: KeyScope,
    leader: bool,
}

#[derive(Debug, Clone)]
pub struct Keybinds {
    leader: KeyEvent,
    bindings: Vec<Binding>,
}

impl Default for Keybinds {
    fn default() -> Self {
        Self {
            leader: parse_key("Space").expect("Space is a valid key"),
            bindings: default_bindings(),
        }
    }
}

impl Keybinds {
    /// Built-in defaults, then the `[keys]` overrides. Configuring an action
    /// replaces all of that action's default bindings; it does not add an alias.
    /// `leader` changes the leader prefix itself.
    pub fn from_config(config: &BTreeMap<String, String>) -> Self {
        let mut leader = parse_key("Space").expect("Space is a valid key");
        if let Some(spec) = config
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("leader"))
            .map(|(_, spec)| spec)
        {
            if let Some(key) = parse_key(spec) {
                leader = key;
            }
        }

        let mut overridden = BTreeSet::new();
        let mut overrides = Vec::new();
        for (name, spec) in config {
            if name.eq_ignore_ascii_case("leader") {
                continue;
            }
            let Some(action) = KeyAction::parse(name) else {
                continue;
            };
            let Some(binding) = parse_binding(spec, action) else {
                continue;
            };
            overridden.insert(action);
            overrides.push(binding);
        }

        let defaults = default_bindings()
            .into_iter()
            .filter(|binding| !overridden.contains(&binding.action));
        Self {
            leader,
            bindings: overrides.into_iter().chain(defaults).collect(),
        }
    }

    /// A global action that works from every editor mode. Overlay handlers run
    /// first, so picker/help/approval keys keep priority.
    pub fn global_action(&self, key: KeyEvent) -> Option<KeyAction> {
        self.find(key, KeyScope::Global, false)
    }

    /// A direct action shared by the transcript and the prompt's normal mode.
    pub fn normal_action(&self, key: KeyEvent) -> Option<KeyAction> {
        self.find(key, KeyScope::Normal, false)
    }

    /// The action after a leader prefix.
    pub fn leader_action(&self, key: KeyEvent) -> Option<KeyAction> {
        self.find(key, KeyScope::Normal, true)
    }

    pub fn is_leader(&self, key: KeyEvent) -> bool {
        key_matches(self.leader, key)
    }

    fn find(&self, key: KeyEvent, scope: KeyScope, leader: bool) -> Option<KeyAction> {
        self.bindings
            .iter()
            .find(|binding| {
                binding.scope == scope && binding.leader == leader && key_matches(binding.key, key)
            })
            .map(|binding| binding.action)
    }
}

fn default_bindings() -> Vec<Binding> {
    let mut bindings = Vec::new();

    push_direct(&mut bindings, KeyAction::Quit, "C-c", KeyScope::Global);
    push_direct(&mut bindings, KeyAction::Quit, "C-d", KeyScope::Global);
    push_direct(&mut bindings, KeyAction::Quit, "Z", KeyScope::Normal);
    push_direct(&mut bindings, KeyAction::Help, "F1", KeyScope::Global);
    push_direct(&mut bindings, KeyAction::Models, "C-/", KeyScope::Global);
    push_direct(&mut bindings, KeyAction::Models, "C-_", KeyScope::Global);
    push_direct(&mut bindings, KeyAction::Models, "C-7", KeyScope::Global);
    push_direct(
        &mut bindings,
        KeyAction::CycleReasoning,
        "C-t",
        KeyScope::Global,
    );
    for index in 1..=9 {
        // Ctrl-7 is the legacy encoding of Ctrl-/ on some terminals.
        if index != 7 {
            push_direct(
                &mut bindings,
                KeyAction::FocusTab(index),
                &format!("C-{index}"),
                KeyScope::Global,
            );
        }
    }

    push_direct(&mut bindings, KeyAction::Palette, "C-p", KeyScope::Normal);
    push_direct(&mut bindings, KeyAction::PageUp, "PageUp", KeyScope::Normal);
    push_direct(&mut bindings, KeyAction::PageUp, "C-b", KeyScope::Normal);
    push_direct(&mut bindings, KeyAction::PageUp, "C-u", KeyScope::Normal);
    push_direct(
        &mut bindings,
        KeyAction::PageDown,
        "PageDown",
        KeyScope::Normal,
    );
    push_direct(&mut bindings, KeyAction::PageDown, "C-f", KeyScope::Normal);
    push_direct(&mut bindings, KeyAction::PanePrev, "<", KeyScope::Normal);
    push_direct(&mut bindings, KeyAction::PaneNext, ">", KeyScope::Normal);
    push_direct(&mut bindings, KeyAction::CycleMode, "Tab", KeyScope::Normal);
    push_direct(
        &mut bindings,
        KeyAction::CycleModeBack,
        "BackTab",
        KeyScope::Normal,
    );
    push_direct(
        &mut bindings,
        KeyAction::CycleModeBack,
        "S-Tab",
        KeyScope::Normal,
    );
    push_direct(&mut bindings, KeyAction::ClosePane, "q", KeyScope::Normal);

    for (action, spec) in [
        (KeyAction::NewSession, "n"),
        (KeyAction::NewWorktree, "W"),
        (KeyAction::NewPane, "o"),
        (KeyAction::Files, "f"),
        (KeyAction::Grep, "s"),
        (KeyAction::Sessions, "g"),
        (KeyAction::Timeline, "h"),
        (KeyAction::Git, "G"),
        (KeyAction::Processes, "p"),
        (KeyAction::Inbox, "a"),
        (KeyAction::Cancel, "x"),
        (KeyAction::ClosePane, "c"),
        (KeyAction::ModePicker, "m"),
        (KeyAction::LayoutHorizontal, "v"),
        (KeyAction::LayoutTabby, "t"),
        (KeyAction::LayoutFibonacci, "r"),
        (KeyAction::LayoutCycle, "l"),
    ] {
        push_leader(&mut bindings, action, spec);
    }

    bindings
}

fn push_direct(bindings: &mut Vec<Binding>, action: KeyAction, spec: &str, scope: KeyScope) {
    if let Some(key) = parse_key(spec) {
        bindings.push(Binding {
            action,
            key,
            scope,
            leader: false,
        });
    }
}

fn push_leader(bindings: &mut Vec<Binding>, action: KeyAction, spec: &str) {
    if let Some(key) = parse_key(spec) {
        bindings.push(Binding {
            action,
            key,
            scope: KeyScope::Normal,
            leader: true,
        });
    }
}

fn parse_binding(spec: &str, action: KeyAction) -> Option<Binding> {
    let spec = spec.trim();
    if spec.is_empty() {
        return None;
    }
    let mut parts = spec.split_whitespace();
    let first = parts.next()?;
    if let Some(second) = parts.next() {
        if parts.next().is_some() {
            return None;
        }
        if first.eq_ignore_ascii_case("leader") || first.eq_ignore_ascii_case("space") {
            let key = parse_key(second)?;
            return Some(Binding {
                action,
                key,
                scope: KeyScope::Normal,
                leader: true,
            });
        }
        return None;
    }

    let key = parse_key(first)?;
    // A bare letter is only safe outside text entry. For actions that normally
    // work globally, demote a bare-letter override to normal/transcript mode
    // instead of swallowing typed input.
    let scope = if action.primary_scope() == KeyScope::Global && is_bare_letter(key) {
        KeyScope::Normal
    } else {
        action.primary_scope()
    };
    if scope == KeyScope::Global && !is_safe_binding(key) {
        return None;
    }
    Some(Binding {
        action,
        key,
        scope,
        leader: false,
    })
}

fn key_matches(bound: KeyEvent, key: KeyEvent) -> bool {
    if bound.code != key.code {
        return false;
    }
    if matches!(bound.code, KeyCode::Char(_)) {
        // Terminals disagree on whether shifted characters carry SHIFT. The
        // character itself already carries the shift state, so ignore it here.
        return bound.modifiers.difference(KeyModifiers::SHIFT)
            == key.modifiers.difference(KeyModifiers::SHIFT);
    }
    // Some terminals report Shift-Tab as BackTab with SHIFT set.
    if bound.code == KeyCode::BackTab {
        return true;
    }
    bound.modifiers == key.modifiers
}

/// Whether a configured binding is safe while the prompt is in insert mode.
/// Plain letters are reserved for typing; global actions need a modifier, a
/// named key, or punctuation such as `?` / `:`.
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

fn is_bare_letter(key: KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char(ch) if ch.is_ascii_alphabetic())
        && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
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

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn config(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(name, spec)| ((*name).to_string(), (*spec).to_string()))
            .collect()
    }

    #[test]
    fn parses_modifiers_and_named_keys() {
        assert_eq!(
            parse_key("q"),
            Some(key(KeyCode::Char('q'), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key("C-x"),
            Some(key(KeyCode::Char('x'), KeyModifiers::CONTROL))
        );
        assert_eq!(
            parse_key("S-Tab"),
            Some(key(KeyCode::Tab, KeyModifiers::SHIFT))
        );
        assert_eq!(
            parse_key("F1"),
            Some(key(KeyCode::F(1), KeyModifiers::NONE))
        );
        assert_eq!(
            parse_key("C-/"),
            Some(key(KeyCode::Char('/'), KeyModifiers::CONTROL))
        );
        assert_eq!(parse_key("not-a-key"), None);
    }

    #[test]
    fn defaults_cover_global_normal_and_leader_actions() {
        let binds = Keybinds::default();
        assert_eq!(
            binds.global_action(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some(KeyAction::Quit)
        );
        assert_eq!(
            binds.global_action(key(KeyCode::Char('g'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            binds.global_action(key(KeyCode::Char('h'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            binds.global_action(key(KeyCode::Backspace, KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(
            binds.normal_action(key(KeyCode::Char('p'), KeyModifiers::CONTROL)),
            Some(KeyAction::Palette)
        );
        assert_eq!(
            binds.normal_action(key(KeyCode::Char('q'), KeyModifiers::NONE)),
            Some(KeyAction::ClosePane)
        );
        assert_eq!(
            binds.normal_action(key(KeyCode::Char('Z'), KeyModifiers::SHIFT)),
            Some(KeyAction::Quit)
        );
        assert!(binds.is_leader(key(KeyCode::Char(' '), KeyModifiers::NONE)));
        assert_eq!(
            binds.leader_action(key(KeyCode::Char('n'), KeyModifiers::NONE)),
            Some(KeyAction::NewSession)
        );
        assert_eq!(
            binds.leader_action(key(KeyCode::Char('W'), KeyModifiers::NONE)),
            Some(KeyAction::NewWorktree)
        );
        assert_eq!(
            binds.leader_action(key(KeyCode::Char('g'), KeyModifiers::NONE)),
            Some(KeyAction::Sessions)
        );
        assert_eq!(
            binds.leader_action(key(KeyCode::Char('h'), KeyModifiers::NONE)),
            Some(KeyAction::Timeline)
        );
        assert_eq!(
            binds.leader_action(key(KeyCode::Char('G'), KeyModifiers::SHIFT)),
            Some(KeyAction::Git)
        );
    }

    #[test]
    fn ctrl_aliases_keep_their_defaults() {
        let binds = Keybinds::default();
        for code in [KeyCode::Char('/'), KeyCode::Char('_'), KeyCode::Char('7')] {
            assert_eq!(
                binds.global_action(key(code, KeyModifiers::CONTROL)),
                Some(KeyAction::Models)
            );
        }
        assert_eq!(
            binds.global_action(key(KeyCode::Char('2'), KeyModifiers::CONTROL)),
            Some(KeyAction::FocusTab(2))
        );
        assert_eq!(
            binds.global_action(key(KeyCode::Char('7'), KeyModifiers::CONTROL)),
            Some(KeyAction::Models)
        );
    }

    #[test]
    fn config_replaces_defaults_instead_of_adding_aliases() {
        let binds = Keybinds::from_config(&config(&[("help", "F2")]));
        assert_eq!(
            binds.global_action(key(KeyCode::F(2), KeyModifiers::NONE)),
            Some(KeyAction::Help)
        );
        assert_eq!(
            binds.global_action(key(KeyCode::F(1), KeyModifiers::NONE)),
            None
        );

        let binds = Keybinds::from_config(&config(&[("sessions", "C-t")]));
        assert_eq!(
            binds.global_action(key(KeyCode::Char('t'), KeyModifiers::CONTROL)),
            Some(KeyAction::Sessions)
        );
        assert_eq!(
            binds.global_action(key(KeyCode::Char('g'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn leader_prefix_and_chords_are_configurable() {
        let binds =
            Keybinds::from_config(&config(&[("leader", "C-b"), ("new_session", "leader N")]));
        assert!(binds.is_leader(key(KeyCode::Char('b'), KeyModifiers::CONTROL)));
        assert!(!binds.is_leader(key(KeyCode::Char(' '), KeyModifiers::NONE)));
        assert_eq!(
            binds.leader_action(key(KeyCode::Char('N'), KeyModifiers::NONE)),
            Some(KeyAction::NewSession)
        );
        // The default Space+n is replaced.
        assert_eq!(
            binds.leader_action(key(KeyCode::Char('n'), KeyModifiers::NONE)),
            None
        );
    }

    #[test]
    fn bare_letters_are_allowed_in_normal_mode() {
        let binds = Keybinds::from_config(&config(&[
            ("sessions", "o"),
            ("new_session", "P"),
            ("close_pane", "x"),
            ("palette", "C-o"),
        ]));
        assert_eq!(
            binds.global_action(key(KeyCode::Char('o'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            binds.normal_action(key(KeyCode::Char('o'), KeyModifiers::NONE)),
            Some(KeyAction::Sessions)
        );
        assert_eq!(
            binds.normal_action(key(KeyCode::Char('P'), KeyModifiers::SHIFT)),
            Some(KeyAction::NewSession)
        );
        assert_eq!(
            binds.normal_action(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(KeyAction::ClosePane)
        );
        assert_eq!(
            binds.normal_action(key(KeyCode::Char('o'), KeyModifiers::CONTROL)),
            Some(KeyAction::Palette)
        );
    }

    #[test]
    fn global_actions_reject_bare_letters_but_demote_them() {
        let binds = Keybinds::from_config(&config(&[("quit", "o")]));
        assert_eq!(
            binds.global_action(key(KeyCode::Char('o'), KeyModifiers::NONE)),
            None
        );
        assert_eq!(
            binds.normal_action(key(KeyCode::Char('o'), KeyModifiers::NONE)),
            Some(KeyAction::Quit)
        );
        assert_eq!(
            binds.global_action(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            None
        );
    }

    #[test]
    fn overlay_internal_actions_are_not_part_of_the_keymap() {
        assert_eq!(KeyAction::parse("picker_down"), None);
        assert_eq!(KeyAction::parse("approval_yes"), None);
    }
}
