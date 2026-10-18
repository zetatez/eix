//! TUI state and rendering.
//!
//! Phase 1 deliberately keeps the input box native (ratatui) instead of
//! embedding Neovim: the daemon protocol and session UX need to settle before
//! the real-nvim input layer is wired in.

use std::collections::HashMap;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use eix_core::{
    ApprovalDecision, ApprovalRequiredPayload, AssistantMessagePayload, AttachResult,
    Event as SessionEvent, EventKind, HelloResult, Session, SessionStatus, SubmitResult,
    ToolCallPayload, ToolResultPayload, UserMessagePayload,
};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::controller::{Command, Event};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Transcript,
    Input,
}

struct Input {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

impl Default for Input {
    fn default() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }
}

impl Input {
    fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn is_empty(&self) -> bool {
        self.lines.iter().all(|line| line.is_empty())
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    fn line(&self) -> &str {
        &self.lines[self.row]
    }

    fn line_len(&self) -> usize {
        self.line().chars().count()
    }

    fn insert_char(&mut self, ch: char) {
        if ch == '\t' {
            for _ in 0..4 {
                self.insert_char(' ');
            }
            return;
        }
        if ch == '\n' {
            self.newline();
            return;
        }
        let index = char_to_byte(self.line(), self.col);
        self.lines[self.row].insert(index, ch);
        self.col += 1;
    }

    fn insert_str(&mut self, text: &str) {
        for ch in text.chars() {
            self.insert_char(ch);
        }
    }

    fn newline(&mut self) {
        let index = char_to_byte(self.line(), self.col);
        let tail = self.lines[self.row].split_off(index);
        self.row += 1;
        self.col = 0;
        self.lines.insert(self.row, tail);
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            let end = char_to_byte(self.line(), self.col);
            let start = char_to_byte(self.line(), self.col - 1);
            self.lines[self.row].replace_range(start..end, "");
            self.col -= 1;
        } else if self.row > 0 {
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_len();
            self.lines[self.row].push_str(&current);
        }
    }

    fn delete(&mut self) {
        if self.col < self.line_len() {
            let start = char_to_byte(self.line(), self.col);
            let end = char_to_byte(self.line(), self.col + 1);
            self.lines[self.row].replace_range(start..end, "");
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.line_len();
        }
    }

    fn right(&mut self) {
        if self.col < self.line_len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    fn up(&mut self) {
        if self.row > 0 {
            self.row -= 1;
            self.col = self.col.min(self.line_len());
        }
    }

    fn down(&mut self) {
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = self.col.min(self.line_len());
        }
    }

    fn home(&mut self) {
        self.col = 0;
    }

    fn end(&mut self) {
        self.col = self.line_len();
    }

    fn delete_to_start(&mut self) {
        let end = char_to_byte(self.line(), self.col);
        self.lines[self.row].replace_range(..end, "");
        self.col = 0;
    }

    fn delete_to_end(&mut self) {
        let start = char_to_byte(self.line(), self.col);
        self.lines[self.row].truncate(start);
    }

    fn delete_word(&mut self) {
        if self.col == 0 {
            self.backspace();
            return;
        }
        let chars: Vec<char> = self.line().chars().collect();
        let mut start = self.col;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let from = char_to_byte(self.line(), start);
        let to = char_to_byte(self.line(), self.col);
        self.lines[self.row].replace_range(from..to, "");
        self.col = start;
    }
}

fn char_to_byte(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

struct SessionView {
    session: Session,
    events: Vec<SessionEvent>,
    stream: Option<StreamState>,
    scroll: usize,
    follow: bool,
}

impl SessionView {
    fn new(session: Session, events: Vec<SessionEvent>) -> Self {
        Self {
            session,
            events,
            stream: None,
            scroll: 0,
            follow: true,
        }
    }
}

struct StreamState {
    turn_id: String,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickerMode {
    /// Replace the focused pane's session.
    Switch,
    /// Open the chosen session in a new pane (or focus it if already open).
    NewPane,
}

struct Picker {
    query: String,
    selected: usize,
    mode: PickerMode,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AttachSlot {
    Existing(usize),
    New,
}

struct PendingAttach {
    session_id: String,
    slot: AttachSlot,
}

/// A tool call the daemon is holding until the user decides.
#[derive(Clone)]
struct PendingApproval {
    approval_id: String,
    tool: String,
    summary: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Left,
    Right,
    Up,
    Down,
}

pub struct App {
    pub should_quit: bool,
    pub exit_message: Option<String>,
    hello: Option<HelloResult>,
    sessions: Vec<Session>,
    selected: usize,
    /// Open panes, in layout order.
    views: Vec<SessionView>,
    /// Index into `views` of the focused pane.
    focused: usize,
    pending_attach: Option<PendingAttach>,
    initial_session: Option<String>,
    initialized: bool,
    creating: bool,
    submitting: bool,
    pending_prompt: Option<String>,
    input: Input,
    focus: Focus,
    picker: Option<Picker>,
    help: bool,
    /// `Ctrl-W` was pressed; the next key is a window command.
    pending_window: bool,
    /// Pane rectangles from the last draw, used to pick a focus direction.
    pane_areas: Vec<Rect>,
    /// Tool calls waiting on a decision, keyed by session id.
    approvals: HashMap<String, PendingApproval>,
    status: String,
    error: Option<String>,
    connected: bool,
    tick: u64,
}

impl App {
    pub fn new(initial_session: Option<String>) -> Self {
        Self {
            should_quit: false,
            exit_message: None,
            hello: None,
            sessions: Vec::new(),
            selected: 0,
            views: Vec::new(),
            focused: 0,
            pending_attach: None,
            initial_session,
            initialized: false,
            creating: false,
            submitting: false,
            pending_prompt: None,
            input: Input::default(),
            focus: Focus::Input,
            picker: None,
            help: false,
            pending_window: false,
            pane_areas: Vec::new(),
            approvals: HashMap::new(),
            status: "connecting…".into(),
            error: None,
            connected: false,
            tick: 0,
        }
    }

    pub fn tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }

    pub fn handle_paste(&mut self, text: &str) {
        self.focus = Focus::Input;
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.input.insert_str(&normalized);
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
                self.hello = Some(hello);
            }
            Event::Sessions(sessions) => {
                self.sessions = sessions;
                if !self.initialized {
                    self.initialized = true;
                    if self.sessions.is_empty() {
                        self.create_session(tx);
                    } else {
                        let index = match self.initial_session.as_deref() {
                            Some(needle) => match self.find_session(needle) {
                                Some(index) => index,
                                None => {
                                    self.error = Some(format!("unknown session {needle:?}"));
                                    self.selected.min(self.sessions.len() - 1)
                                }
                            },
                            None => 0,
                        };
                        self.select_session(index, tx);
                    }
                } else {
                    self.reselect_current();
                }
            }
            Event::Attached(attached) => self.attached(attached),
            Event::Created(session) => {
                self.creating = false;
                let id = session.id.clone();
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
            Event::Submitted(submitted) => self.submitted(submitted),
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
                    self.approvals
                        .retain(|_, pending| pending.approval_id != approval_id);
                    self.status = "approval sent".into();
                } else {
                    self.status = "approval was stale; the daemon is still waiting".into();
                }
            }
            Event::Notification(notification) => self.notification(notification),
            Event::Error { context, message } => {
                self.error = Some(message.clone());
                self.status = format!("{context} failed");
                match context.as_str() {
                    "submit" => {
                        self.submitting = false;
                        self.pending_prompt = None;
                    }
                    "create" => self.creating = false,
                    "attach" => self.pending_attach = None,
                    "connect" => {
                        self.connected = false;
                        self.initialized = true;
                    }
                    "list" if !self.initialized => self.initialized = true,
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

    fn attached(&mut self, attached: AttachResult) {
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
        let view = SessionView::new(attached.session, attached.events);

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
        self.selected = self.find_session(&session_id).unwrap_or(self.selected);
        self.error = None;
        self.status = "attached".into();
    }

    fn submitted(&mut self, submitted: SubmitResult) {
        self.submitting = false;
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
            view.session = submitted.session;
            view.stream = Some(StreamState {
                turn_id: submitted.turn_id,
                text: String::new(),
            });
            view.follow = true;
        }
        self.error = None;
        self.status = "running".into();
    }

    fn notification(&mut self, notification: eix_core::Notification) {
        match notification.method.as_str() {
            "session.updated" => {
                if let Ok(session) = serde_json::from_value::<Session>(notification.params) {
                    if let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == session.id)
                    {
                        view.session = session.clone();
                    }
                    self.upsert_session(session, false);
                }
            }
            "session.event" => {
                if let Ok(event) = serde_json::from_value::<SessionEvent>(notification.params) {
                    let id = event.session_id.clone();
                    let Some(view) = self.views.iter_mut().find(|view| view.session.id == id)
                    else {
                        return;
                    };
                    match event.kind {
                        EventKind::AssistantMessage => {
                            view.stream = None;
                            if let Ok(payload) = serde_json::from_value::<AssistantMessagePayload>(
                                event.payload.clone(),
                            ) {
                                if payload.cancelled {
                                    self.status = "cancelled".into();
                                }
                            }
                        }
                        EventKind::Error => {
                            if let Some(message) =
                                event.payload.get("message").and_then(Value::as_str)
                            {
                                self.error = Some(message.to_string());
                                self.status = "error".into();
                            }
                        }
                        _ => {}
                    }
                    view.events.push(event);
                }
            }
            "message.delta" => {
                if let Ok(delta) = serde_json::from_value::<DeltaParams>(notification.params) {
                    let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == delta.session_id)
                    else {
                        return;
                    };
                    match &mut view.stream {
                        Some(stream) if stream.turn_id == delta.turn_id => {
                            stream.text.push_str(&delta.delta)
                        }
                        Some(_) => {
                            // Deltas from a superseded turn; ignore them.
                        }
                        None => {
                            view.stream = Some(StreamState {
                                turn_id: delta.turn_id,
                                text: delta.delta,
                            })
                        }
                    }
                }
            }
            "tool.approval_required" => {
                if let Ok(params) = serde_json::from_value::<ApprovalParams>(notification.params) {
                    if let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == params.session_id)
                    {
                        view.follow = true;
                    }
                    self.status = format!("approval needed: {}", params.summary);
                    self.approvals.insert(
                        params.session_id,
                        PendingApproval {
                            approval_id: params.approval_id,
                            tool: params.tool,
                            summary: params.summary,
                        },
                    );
                }
            }
            "tool.completed" => {
                if let Ok(params) = serde_json::from_value::<ToolCompleted>(notification.params) {
                    self.approvals.remove(&params.session_id);
                }
            }
            "turn.completed" => {
                if let Ok(done) = serde_json::from_value::<TurnCompleted>(notification.params) {
                    self.approvals.remove(&done.session_id);
                    let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == done.session_id)
                    else {
                        return;
                    };
                    if let Some(stream) = &view.stream {
                        if stream.turn_id != done.turn_id {
                            return;
                        }
                    }
                    view.stream = None;
                    if let Some(error) = done.error {
                        self.error = Some(error);
                        self.status = "error".into();
                    } else if done.cancelled {
                        self.status = "cancelled".into();
                    } else {
                        self.status = "idle".into();
                    }
                }
            }
            "stream.lagged" => {
                if let Some(skipped) = notification.params.get("skipped") {
                    self.status = format!("dropped {skipped} notifications");
                }
            }
            _ => {}
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.should_quit = true;
            return;
        }
        if matches!(key.code, KeyCode::F(1)) {
            self.help = true;
            return;
        }

        if self.help {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?')
            ) {
                self.help = false;
            }
            return;
        }

        if self.picker.is_some() {
            self.handle_picker_key(key, tx);
            return;
        }

        if self.focus == Focus::Input {
            self.handle_input_key(key, tx);
        } else {
            self.handle_transcript_key(key, tx);
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('g')) {
            self.open_picker(PickerMode::Switch);
            return;
        }

        match key.code {
            KeyCode::Esc => self.focus = Focus::Transcript,
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::SHIFT) =>
            {
                self.input.newline()
            }
            KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.newline()
            }
            KeyCode::Enter => self.submit(tx),
            KeyCode::Char(c) if key.modifiers.contains(KeyModifiers::CONTROL) => match c {
                'a' => self.input.home(),
                'e' => self.input.end(),
                'u' => self.input.delete_to_start(),
                'k' => self.input.delete_to_end(),
                'w' => self.input.delete_word(),
                _ => {}
            },
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::ALT) => {
                self.input.insert_char(c)
            }
            KeyCode::Backspace => self.input.backspace(),
            KeyCode::Delete => self.input.delete(),
            KeyCode::Left => self.input.left(),
            KeyCode::Right => self.input.right(),
            KeyCode::Up => self.input.up(),
            KeyCode::Down => self.input.down(),
            KeyCode::Home => self.input.home(),
            KeyCode::End => self.input.end(),
            KeyCode::Tab => self.input.insert_str("    "),
            _ => {}
        }
    }

    fn handle_transcript_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        if self.pending_window {
            self.pending_window = false;
            self.handle_window_key(key);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('g')) {
            self.open_picker(PickerMode::Switch);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('w')) {
            self.pending_window = true;
            self.status = "window: h/j/k/l focus · v split · c close · w cycle · Esc cancel".into();
            return;
        }
        // While the focused session is blocked on a tool call, y/n answer the
        // approval card instead of their usual bindings.
        if self.focused_approval().is_some() {
            match key.code {
                KeyCode::Char('y') => {
                    self.respond_approval(ApprovalDecision::Approve, tx);
                    return;
                }
                KeyCode::Char('n') => {
                    self.respond_approval(ApprovalDecision::Deny, tx);
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('i') | KeyCode::Enter => self.focus = Focus::Input,
            KeyCode::Char('n') => self.create_session(tx),
            KeyCode::Char('p') => self.open_picker(PickerMode::Switch),
            KeyCode::Char('o') => self.open_picker(PickerMode::NewPane),
            KeyCode::Char('?') => self.help = true,
            KeyCode::Char('x') => self.cancel(tx),
            KeyCode::Tab => self.cycle_session(1, tx),
            KeyCode::BackTab => self.cycle_session(-1, tx),
            KeyCode::Char('j') | KeyCode::Down => self.scroll_focused(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_focused(-1),
            KeyCode::PageDown => self.scroll_focused(10),
            KeyCode::PageUp => self.scroll_focused(-10),
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_focused(10)
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.scroll_focused(-10)
            }
            KeyCode::Char('g') => {
                if let Some(view) = self.focused_view_mut() {
                    view.scroll = 0;
                    view.follow = false;
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

    fn handle_window_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('h') | KeyCode::Left => self.focus_direction(Direction::Left),
            KeyCode::Char('l') | KeyCode::Right => self.focus_direction(Direction::Right),
            KeyCode::Char('j') | KeyCode::Down => self.focus_direction(Direction::Down),
            KeyCode::Char('k') | KeyCode::Up => self.focus_direction(Direction::Up),
            KeyCode::Char('v') | KeyCode::Char('s') => self.open_picker(PickerMode::NewPane),
            KeyCode::Char('c') | KeyCode::Char('q') => self.close_focused_pane(),
            KeyCode::Char('w') => self.cycle_pane(1),
            KeyCode::Esc => self.status = "window command cancelled".into(),
            _ => {}
        }
    }

    fn handle_picker_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        let filtered = self.filtered_sessions();
        let Some(picker) = &mut self.picker else {
            return;
        };
        let mode = picker.mode;
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up => picker.selected = picker.selected.saturating_sub(1),
            KeyCode::Down => {
                if picker.selected + 1 < filtered.len() {
                    picker.selected += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(index) = filtered.get(picker.selected).copied() {
                    self.picker = None;
                    match mode {
                        PickerMode::Switch => self.select_session(index, tx),
                        PickerMode::NewPane => self.open_in_new_pane(index, tx),
                    }
                }
            }
            KeyCode::Backspace => {
                picker.query.pop();
                picker.selected = 0;
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                picker.query.push(c);
                picker.selected = 0;
            }
            _ => {}
        }
    }

    fn open_picker(&mut self, mode: PickerMode) {
        let selected = self.selected.min(self.sessions.len().saturating_sub(1));
        self.picker = Some(Picker {
            query: String::new(),
            selected,
            mode,
        });
    }

    fn filtered_sessions(&self) -> Vec<usize> {
        let Some(picker) = &self.picker else {
            return Vec::new();
        };
        let query = picker.query.trim().to_lowercase();
        if query.is_empty() {
            return (0..self.sessions.len()).collect();
        }
        let mut hits: Vec<(i32, usize)> = self
            .sessions
            .iter()
            .enumerate()
            .filter_map(|(index, session)| {
                let haystack = format!("{} {} {}", session.name, session.id, session.cwd.display())
                    .to_lowercase();
                fuzzy_score(&query, &haystack).map(|score| (score, index))
            })
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        hits.into_iter().map(|(_, index)| index).collect()
    }

    fn create_session(&mut self, tx: &UnboundedSender<Command>) {
        if self.creating {
            return;
        }
        self.creating = true;
        self.error = None;
        self.status = "creating session…".into();
        let _ = tx.send(Command::Create);
    }

    fn submit(&mut self, tx: &UnboundedSender<Command>) {
        if self.submitting {
            self.status = "submit already in flight".into();
            return;
        }
        let text = self.input.text();
        if text.trim().is_empty() {
            return;
        }
        let Some(view) = self.focused_view() else {
            self.status = "no session yet".into();
            return;
        };
        if view.session.status.is_busy() {
            self.error = Some("session is running; press x to cancel".into());
            return;
        }
        let session_id = view.session.id.clone();
        self.submitting = true;
        self.pending_prompt = Some(text.clone());
        self.error = None;
        self.status = "submitting…".into();
        let _ = tx.send(Command::Submit { session_id, text });
    }

    fn cancel(&mut self, tx: &UnboundedSender<Command>) {
        if let Some(view) = self.focused_view() {
            let session_id = view.session.id.clone();
            self.status = "cancelling…".into();
            let _ = tx.send(Command::Cancel(session_id));
        } else {
            self.status = "no session".into();
        }
    }

    fn focused_approval(&self) -> Option<&PendingApproval> {
        let view = self.focused_view()?;
        self.approvals.get(&view.session.id)
    }

    fn respond_approval(&mut self, decision: ApprovalDecision, tx: &UnboundedSender<Command>) {
        let Some(view) = self.focused_view() else {
            return;
        };
        let session_id = view.session.id.clone();
        let Some(pending) = self.approvals.get(&session_id) else {
            return;
        };
        let approval_id = pending.approval_id.clone();
        self.status = match decision {
            ApprovalDecision::Approve => "approving…",
            ApprovalDecision::Deny => "denying…",
        }
        .into();
        let _ = tx.send(Command::Approval {
            session_id,
            approval_id,
            decision,
        });
    }

    fn focused_view(&self) -> Option<&SessionView> {
        self.views.get(self.focused)
    }

    fn focused_view_mut(&mut self) -> Option<&mut SessionView> {
        let index = self.focused;
        self.views.get_mut(index)
    }

    fn open_session(&self, session_id: &str) -> Option<usize> {
        self.views
            .iter()
            .position(|view| view.session.id == session_id)
    }

    fn focus_pane(&mut self, index: usize) {
        if index >= self.views.len() {
            return;
        }
        self.focused = index;
        self.sync_selected_to_focus();
        self.status = format!("pane {}/{} focused", index + 1, self.views.len());
    }

    fn sync_selected_to_focus(&mut self) {
        if let Some(view) = self.focused_view() {
            if let Some(index) = self.find_session(&view.session.id) {
                self.selected = index;
            }
        }
    }

    fn focus_direction(&mut self, direction: Direction) {
        if self.views.len() < 2 || self.pane_areas.len() != self.views.len() {
            return;
        }
        match pick_direction(&self.pane_areas, self.focused, direction) {
            Some(index) => self.focus_pane(index),
            None => self.status = "no pane in that direction".into(),
        }
    }

    fn cycle_pane(&mut self, delta: isize) {
        if self.views.len() < 2 {
            return;
        }
        let len = self.views.len() as isize;
        let next = (self.focused as isize + delta).rem_euclid(len) as usize;
        self.focus_pane(next);
    }

    fn close_focused_pane(&mut self) {
        if self.views.len() <= 1 {
            self.status = "cannot close the last pane".into();
            return;
        }
        self.views.remove(self.focused);
        self.focused = self.focused.min(self.views.len().saturating_sub(1));
        self.sync_selected_to_focus();
        self.status = "pane closed".into();
    }

    fn cycle_session(&mut self, delta: isize, tx: &UnboundedSender<Command>) {
        if self.sessions.is_empty() {
            return;
        }
        let len = self.sessions.len() as isize;
        let next = (self.selected as isize + delta).rem_euclid(len) as usize;
        self.select_session(next, tx);
    }

    fn select_session(&mut self, index: usize, tx: &UnboundedSender<Command>) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = index.min(self.sessions.len() - 1);
        let id = self.sessions[self.selected].id.clone();
        if let Some(open) = self.open_session(&id) {
            self.focused = open;
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

    fn open_in_new_pane(&mut self, index: usize, tx: &UnboundedSender<Command>) {
        if self.sessions.is_empty() {
            return;
        }
        self.selected = index.min(self.sessions.len() - 1);
        let id = self.sessions[self.selected].id.clone();
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

    fn reselect_current(&mut self) {
        if let Some(view) = self.focused_view() {
            if let Some(index) = self.find_session(&view.session.id) {
                self.selected = index;
                return;
            }
        }
        if let Some(pending) = &self.pending_attach {
            if let Some(index) = self.find_session(&pending.session_id) {
                self.selected = index;
                return;
            }
        }
        self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
    }

    fn find_session(&self, needle: &str) -> Option<usize> {
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
        (matches.len() == 1).then_some(matches[0])
    }

    fn upsert_session(&mut self, session: Session, front: bool) {
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

    fn scroll_focused(&mut self, amount: isize) {
        let Some(view) = self.focused_view_mut() else {
            return;
        };
        view.follow = false;
        if amount >= 0 {
            view.scroll = view.scroll.saturating_add(amount as usize);
        } else {
            view.scroll = view.scroll.saturating_sub(amount.unsigned_abs());
        }
    }

    fn model(&self) -> &str {
        self.focused_view()
            .and_then(|view| view.session.model.as_deref())
            .or_else(|| self.hello.as_ref().map(|hello| hello.model.as_str()))
            .unwrap_or("-")
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        if area.height < 4 || area.width < 20 {
            let notice = Paragraph::new("terminal too small for eix")
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::Red));
            frame.render_widget(notice, area);
            return;
        }

        let inner_width = area.width.saturating_sub(2).max(1) as usize;
        let input_lines: usize = self
            .input
            .lines
            .iter()
            .map(|line| {
                let width = UnicodeWidthStr::width(line.as_str());
                width.div_ceil(inner_width).max(1)
            })
            .sum();
        let input_height = (input_lines as u16 + 2).clamp(3, 10);
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(input_height),
            Constraint::Length(1),
        ])
        .split(area);

        self.draw_header(frame, chunks[0]);
        self.draw_body(frame, chunks[1]);
        self.draw_input(frame, chunks[2]);
        self.draw_footer(frame, chunks[3]);

        if self.picker.is_some() {
            self.draw_picker(frame, area);
        }
        if self.help {
            draw_help(frame, area);
        }
    }

    fn draw_header(&mut self, frame: &mut Frame, area: Rect) {
        let session = self
            .focused_view()
            .map(|view| &view.session)
            .or_else(|| self.sessions.get(self.selected));
        let mut spans = vec![
            Span::styled(
                " eix ",
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" "),
        ];
        if let Some(session) = session {
            let spinner = if session.status == SessionStatus::Running {
                SPINNER[(self.tick as usize / 2) % SPINNER.len()]
            } else {
                session.status.symbol()
            };
            spans.push(Span::styled(
                format!("{spinner} "),
                Style::default().fg(status_color(session.status)),
            ));
            spans.push(Span::styled(
                session.name.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(
                format!("  {}  ", session.status.as_str()),
                Style::default().fg(Color::DarkGray),
            ));
            spans.push(Span::styled(
                format!("{}  ", self.model()),
                Style::default().fg(Color::Magenta),
            ));
            spans.push(Span::styled(
                session.cwd.display().to_string(),
                Style::default().fg(Color::DarkGray),
            ));
        } else {
            spans.push(Span::styled(
                "no session",
                Style::default().fg(Color::DarkGray),
            ));
        }
        if !self.views.is_empty() {
            spans.push(Span::styled(
                format!("  pane {}/{}", self.focused + 1, self.views.len()),
                Style::default().fg(Color::DarkGray),
            ));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_body(&mut self, frame: &mut Frame, area: Rect) {
        let show_sidebar = area.width >= 100 && !self.sessions.is_empty();
        let (sidebar, main) = if show_sidebar {
            let chunks =
                Layout::horizontal([Constraint::Length(30), Constraint::Min(40)]).split(area);
            (Some(chunks[0]), chunks[1])
        } else {
            (None, area)
        };
        if let Some(sidebar) = sidebar {
            self.draw_sidebar(frame, sidebar);
        }
        self.draw_panes(frame, main);
    }

    fn draw_sidebar(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .sessions
            .iter()
            .map(|session| {
                let pane = match self.open_session(&session.id) {
                    Some(index) => {
                        Span::styled(format!("{}", index + 1), Style::default().fg(Color::Cyan))
                    }
                    None => Span::styled("·", Style::default().fg(Color::DarkGray)),
                };
                ListItem::new(Line::from(vec![
                    pane,
                    Span::raw(" "),
                    Span::styled(
                        format!("{} ", session.status.symbol()),
                        Style::default().fg(status_color(session.status)),
                    ),
                    Span::raw(truncate(&session.name, 12)),
                    Span::styled(
                        format!(" {}", short_id(&session.id)),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            })
            .collect();
        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" sessions "))
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");
        let mut state = ListState::default().with_selected(Some(self.selected));
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn draw_panes(&mut self, frame: &mut Frame, area: Rect) {
        if self.views.is_empty() {
            self.pane_areas.clear();
            self.draw_empty_pane(frame, area);
            return;
        }
        self.focused = self.focused.min(self.views.len() - 1);
        let rects = pane_rects(area, self.views.len());
        self.pane_areas = rects.clone();
        for (index, rect) in rects.into_iter().enumerate() {
            if index >= self.views.len() {
                break;
            }
            self.draw_pane(frame, rect, index);
        }
    }

    fn draw_pane(&mut self, frame: &mut Frame, area: Rect, index: usize) {
        if area.width < 4 || area.height < 4 {
            return;
        }
        let focused = index == self.focused;
        let approval = if focused {
            self.approvals.get(&self.views[index].session.id).cloned()
        } else {
            None
        };
        let view = &mut self.views[index];
        let inner_width = area.width.saturating_sub(2) as usize;
        let inner_height = area.height.saturating_sub(2) as usize;
        let lines = Self::transcript_lines(view, inner_width.max(1));
        let max_scroll = lines.len().saturating_sub(inner_height);
        if view.follow {
            view.scroll = max_scroll;
        } else {
            view.scroll = view.scroll.min(max_scroll);
        }
        let name = truncate(&view.session.name, inner_width.saturating_sub(8).max(1));
        let title = format!(" {} {} {} ", index + 1, view.session.status.symbol(), name);
        let border_style = if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let paragraph = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(border_style)
                    .title(title),
            )
            .scroll((view.scroll.min(u16::MAX as usize) as u16, 0));
        frame.render_widget(paragraph, area);

        if let Some(approval) = approval {
            draw_approval_card(frame, area, &approval);
        }
    }

    fn draw_empty_pane(&self, frame: &mut Frame, area: Rect) {
        let mut lines = Vec::new();
        if !self.initialized {
            lines.push(Line::from("starting…"));
        } else if self.sessions.is_empty() && self.error.is_none() {
            lines.push(Line::from(Span::styled(
                "no sessions yet",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from("press n to create one"));
        } else if self.pending_attach.is_some() {
            lines.push(Line::from("loading session…"));
        } else if self.error.is_some() {
            lines.push(Line::from(Span::styled(
                "could not load the session",
                Style::default().fg(Color::Red),
            )));
            lines.push(Line::from(""));
            lines.push(Line::from(
                "press p to pick another one, or n for a new session",
            ));
        } else {
            lines.push(Line::from("no pane open"));
            lines.push(Line::from(""));
            lines.push(Line::from(
                "press p to pick a session, o to open a new pane",
            ));
        }
        let paragraph = Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" transcript "));
        frame.render_widget(paragraph, area);
    }

    fn transcript_lines(view: &SessionView, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for event in &view.events {
            match event.kind {
                EventKind::UserMessage => {
                    if let Ok(payload) =
                        serde_json::from_value::<UserMessagePayload>(event.payload.clone())
                    {
                        push_message(&mut lines, "you", Color::Cyan, &payload.text, "", width);
                    }
                }
                EventKind::AssistantMessage => {
                    if let Ok(payload) =
                        serde_json::from_value::<AssistantMessagePayload>(event.payload.clone())
                    {
                        if payload.text.is_empty() {
                            // Tool-only rounds are represented by the tool cards.
                            continue;
                        }
                        let suffix = if payload.cancelled {
                            "  [cancelled]"
                        } else {
                            ""
                        };
                        push_message(
                            &mut lines,
                            "eix",
                            Color::Green,
                            &payload.text,
                            suffix,
                            width,
                        );
                    }
                }
                EventKind::ToolCall => {
                    if let Ok(payload) =
                        serde_json::from_value::<ToolCallPayload>(event.payload.clone())
                    {
                        let text = tool_call_summary(&payload.tool, &payload.arguments);
                        push_message(&mut lines, "tool", Color::Yellow, &text, "", width);
                    }
                }
                EventKind::ApprovalRequired => {
                    if let Ok(payload) =
                        serde_json::from_value::<ApprovalRequiredPayload>(event.payload.clone())
                    {
                        let text = format!("{} — y to approve, n to deny", payload.summary);
                        push_message(&mut lines, "need", Color::Yellow, &text, "", width);
                    }
                }
                EventKind::ToolResult => {
                    if let Ok(payload) =
                        serde_json::from_value::<ToolResultPayload>(event.payload.clone())
                    {
                        let (label, color) = if payload.denied {
                            ("denied", Color::Red)
                        } else if payload.ok {
                            ("ok", Color::Green)
                        } else {
                            ("failed", Color::Red)
                        };
                        let header = format!("{} → {label}", payload.tool);
                        push_message(&mut lines, "tool", color, &header, "", width);
                        let output = payload.output.trim_end();
                        if !output.is_empty() {
                            let max_lines = 40;
                            for line in output.lines().take(max_lines) {
                                for segment in wrap_text(line, width.saturating_sub(2).max(1)) {
                                    lines.push(Line::from(Span::styled(
                                        format!("  {segment}"),
                                        Style::default().fg(Color::DarkGray),
                                    )));
                                }
                            }
                            if output.lines().count() > max_lines {
                                lines.push(Line::from(Span::styled(
                                    "  …",
                                    Style::default().fg(Color::DarkGray),
                                )));
                            }
                            lines.push(Line::from(""));
                        }
                    }
                }
                EventKind::Error => {
                    let message = event
                        .payload
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    push_message(&mut lines, "error", Color::Red, message, "", width);
                }
                _ => {}
            }
        }
        if let Some(stream) = &view.stream {
            let text = if stream.text.is_empty() {
                "…"
            } else {
                stream.text.as_str()
            };
            push_message(&mut lines, "eix", Color::Green, text, "", width);
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "no messages yet — type a prompt below",
                Style::default().fg(Color::DarkGray),
            )));
        }
        lines
    }

    fn draw_input(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Input;
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(if focused {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            })
            .title(" prompt ")
            .title_bottom(" Enter send  Ctrl-J newline  Esc transcript ");
        let inner_width = area.width.saturating_sub(2).max(1) as usize;
        let inner_height = area.height.saturating_sub(2).max(1) as usize;

        if self.input.is_empty() && !focused {
            let paragraph = Paragraph::new(Line::from(Span::styled(
                "press i to type",
                Style::default().fg(Color::DarkGray),
            )))
            .block(block);
            frame.render_widget(paragraph, area);
            return;
        }

        let layout = layout_input(&self.input, inner_width, inner_height);
        let paragraph = Paragraph::new(layout.lines.join("\n"))
            .block(block)
            .scroll((layout.offset.min(u16::MAX as usize) as u16, 0));
        frame.render_widget(paragraph, area);
        if focused {
            let x = area.x + 1 + layout.cursor_col.min(inner_width.saturating_sub(1)) as u16;
            let row = layout
                .cursor_line
                .saturating_sub(layout.offset)
                .min(inner_height.saturating_sub(1));
            let y = area.y + 1 + row as u16;
            frame.set_cursor_position((x, y));
        }
    }

    fn draw_footer(&mut self, frame: &mut Frame, area: Rect) {
        let connected = if self.connected { "●" } else { "○" };
        let message = match &self.error {
            Some(error) => format!(" error: {error}"),
            None => format!(
                " {} eixd  {}  {} pane(s)  {} session(s)  {}",
                connected,
                self.model(),
                self.views.len(),
                self.sessions.len(),
                self.status
            ),
        };
        let style = if self.error.is_some() {
            Style::default().fg(Color::White).bg(Color::Red)
        } else {
            Style::default().fg(Color::Black).bg(Color::Gray)
        };
        let chunks = Layout::horizontal([Constraint::Min(20), Constraint::Length(46)]).split(area);
        frame.render_widget(
            Paragraph::new(truncate(&message, chunks[0].width as usize)).style(style),
            chunks[0],
        );
        let hint = if self.approvals.is_empty() {
            " C-w pane  p picker  o split  ? help  q quit "
        } else {
            " y approve  n deny  x cancel  ? help "
        };
        frame.render_widget(
            Paragraph::new(hint)
                .alignment(Alignment::Right)
                .style(style),
            chunks[1],
        );
    }

    fn draw_picker(&mut self, frame: &mut Frame, area: Rect) {
        let filtered = self.filtered_sessions();
        let open_panes: Vec<Option<usize>> = self
            .sessions
            .iter()
            .map(|session| {
                self.views
                    .iter()
                    .position(|view| view.session.id == session.id)
            })
            .collect();
        let Some(picker) = &mut self.picker else {
            return;
        };
        if !filtered.is_empty() {
            picker.selected = picker.selected.min(filtered.len() - 1);
        } else {
            picker.selected = 0;
        }

        let width = (area.width as u32 * 7 / 10).max(40).min(area.width as u32) as u16;
        let height = (area.height as u32 * 6 / 10)
            .max(10)
            .min(area.height as u32) as u16;
        let rect = centered(area, width, height);
        frame.render_widget(Clear, rect);
        let (title, hint) = match picker.mode {
            PickerMode::Switch => (
                " sessions — switch focused pane ",
                " Enter open  Esc close ",
            ),
            PickerMode::NewPane => (
                " sessions — open in a new pane ",
                " Enter split  Esc close ",
            ),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title)
            .title_bottom(hint);
        let inner = block.inner(rect);
        frame.render_widget(block, rect);
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(4),
        ])
        .split(inner);

        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("❯ ", Style::default().fg(Color::Cyan)),
                Span::raw(picker.query.clone()),
            ])),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new("─".repeat(chunks[1].width as usize))
                .style(Style::default().fg(Color::DarkGray)),
            chunks[1],
        );

        let items: Vec<ListItem> = filtered
            .iter()
            .map(|&index| {
                let session = &self.sessions[index];
                let pane = match open_panes[index] {
                    Some(pane) => {
                        Span::styled(format!("{} ", pane + 1), Style::default().fg(Color::Cyan))
                    }
                    None => Span::styled("· ", Style::default().fg(Color::DarkGray)),
                };
                ListItem::new(Line::from(vec![
                    pane,
                    Span::styled(
                        format!("{} ", session.status.symbol()),
                        Style::default().fg(status_color(session.status)),
                    ),
                    Span::styled(
                        truncate(&session.name, 28),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  {}  ", short_id(&session.id)),
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::styled(
                        session.cwd.display().to_string(),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            })
            .collect();
        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("› ");
        let mut state = ListState::default().with_selected(Some(picker.selected));
        frame.render_stateful_widget(list, chunks[2], &mut state);
    }
}

#[derive(Deserialize)]
struct DeltaParams {
    session_id: String,
    turn_id: String,
    delta: String,
}

#[derive(Deserialize)]
struct TurnCompleted {
    session_id: String,
    turn_id: String,
    #[serde(default)]
    cancelled: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct ApprovalParams {
    session_id: String,
    approval_id: String,
    tool: String,
    summary: String,
}

#[derive(Deserialize)]
struct ToolCompleted {
    session_id: String,
}

struct InputLayout {
    lines: Vec<String>,
    cursor_line: usize,
    cursor_col: usize,
    offset: usize,
}

fn layout_input(input: &Input, width: usize, height: usize) -> InputLayout {
    let width = width.max(1);
    let height = height.max(1);
    let mut lines = Vec::new();
    let mut cursor_line = 0;
    let mut cursor_col = 0;

    for (row, line) in input.lines.iter().enumerate() {
        let chars: Vec<char> = line.chars().collect();
        let mut segment = String::new();
        let mut segment_width = 0usize;

        if row == input.row && input.col == 0 {
            cursor_line = lines.len();
            cursor_col = 0;
        }

        for (index, &ch) in chars.iter().enumerate() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if segment_width + ch_width > width && !segment.is_empty() {
                lines.push(std::mem::take(&mut segment));
                segment_width = 0;
            }
            if row == input.row && input.col == index {
                cursor_line = lines.len();
                cursor_col = segment_width;
            }
            segment.push(ch);
            segment_width += ch_width;
        }

        if row == input.row && input.col >= chars.len() {
            if segment_width >= width {
                lines.push(std::mem::take(&mut segment));
                segment_width = 0;
            }
            cursor_line = lines.len();
            cursor_col = segment_width;
        }
        lines.push(segment);
    }

    if lines.is_empty() {
        lines.push(String::new());
    }
    let offset = if lines.len() > height {
        (cursor_line + 1)
            .saturating_sub(height)
            .min(lines.len().saturating_sub(height))
    } else {
        0
    };
    InputLayout {
        lines,
        cursor_line,
        cursor_col,
        offset,
    }
}

fn tool_call_summary(tool: &str, arguments: &Value) -> String {
    let string = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    match tool {
        "write_file" => {
            let bytes = string("content").len();
            format!("write_file {} (+{bytes} bytes)", string("path"))
        }
        "run_command" => format!("run_command: {}", string("command")),
        "read_file" | "list_dir" => {
            let path = string("path");
            format!("{tool} {}", if path.is_empty() { "." } else { path })
        }
        _ => {
            let compact = serde_json::to_string(arguments).unwrap_or_default();
            format!("{tool} {}", truncate(&compact, 80))
        }
    }
}

fn push_message(
    lines: &mut Vec<Line<'static>>,
    role: &str,
    color: Color,
    text: &str,
    suffix: &str,
    width: usize,
) {
    let prefix = format!("{role}> ");
    let prefix_width = UnicodeWidthStr::width(prefix.as_str());
    let content_width = width.saturating_sub(prefix_width).max(1);
    let wrapped = wrap_text(text, content_width);
    let continuation = " ".repeat(prefix_width);
    for (index, segment) in wrapped.iter().enumerate() {
        let mut spans = Vec::new();
        if index == 0 {
            spans.push(Span::styled(
                prefix.clone(),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::raw(continuation.clone()));
        }
        spans.push(Span::raw(segment.clone()));
        if index + 1 == wrapped.len() && !suffix.is_empty() {
            spans.push(Span::styled(
                suffix.to_string(),
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines.push(Line::from(""));
}

fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = Vec::new();
    for line in text.split('\n') {
        if line.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut current = String::new();
        let mut current_width = 0;
        for ch in line.chars() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if current_width + ch_width > width && !current.is_empty() {
                out.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(ch);
            current_width += ch_width;
        }
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let width = (area.width as u32 * 7 / 10).max(50).min(area.width as u32) as u16;
    let height = (area.height as u32 * 7 / 10)
        .max(16)
        .min(area.height as u32) as u16;
    let rect = centered(area, width, height);
    frame.render_widget(Clear, rect);
    let lines = vec![
        Line::from(Span::styled(
            "eix keys",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        key_line("i / Enter", "focus the prompt"),
        key_line("Esc", "leave the prompt"),
        key_line("Enter", "send (in prompt)"),
        key_line("Ctrl-J", "newline (in prompt)"),
        key_line("p / Ctrl-G", "session picker"),
        key_line("o", "open session in a new pane"),
        key_line("C-w h/j/k/l", "focus pane left/down/up/right"),
        key_line("C-w v / s", "open a new pane"),
        key_line("C-w c", "close focused pane"),
        key_line("C-w w", "cycle panes"),
        key_line("Tab / S-Tab", "next / previous session"),
        key_line("n", "new session (new pane)"),
        key_line("x", "cancel focused turn"),
        key_line("y / n", "approve / deny a pending tool call"),
        key_line("j / k", "scroll focused transcript"),
        key_line("g / G", "top / follow tail"),
        key_line("? / F1", "open this help (F1 anywhere)"),
        key_line("q / Ctrl-C", "quit"),
        Line::from(""),
        Line::from(Span::styled(
            "C-w commands work from the transcript; Esc leaves the prompt first.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "the prompt is a native text box for now; the embedded-nvim input lands later",
            Style::default().fg(Color::DarkGray),
        )),
    ];
    let paragraph = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(" help "))
        .style(Style::default().bg(Color::Black));
    frame.render_widget(paragraph, rect);
}

fn key_line(key: &'static str, description: &'static str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{key:<14}"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(description),
    ])
}

/// Tile `count` panes inside `area`. One pane is fullscreen, two split
/// vertically, three use a tall left pane plus two stacked on the right, and
/// everything else uses a near-square grid.
fn pane_rects(area: Rect, count: usize) -> Vec<Rect> {
    match count {
        0 => Vec::new(),
        1 => vec![area],
        2 => Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area)
            .to_vec(),
        3 => {
            let columns =
                Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                    .split(area);
            let right = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(columns[1]);
            vec![columns[0], right[0], right[1]]
        }
        4 => {
            let rows = Layout::vertical([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);
            let mut rects = Vec::with_capacity(4);
            for row in rows.iter() {
                let columns =
                    Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(*row);
                rects.extend(columns.iter().copied());
            }
            rects
        }
        n => {
            let columns = (n as f64).sqrt().ceil() as usize;
            let rows = n.div_ceil(columns);
            let row_rects =
                Layout::vertical(vec![Constraint::Ratio(1, rows as u32); rows]).split(area);
            let mut rects = Vec::with_capacity(n);
            for row in row_rects.iter() {
                let col_rects =
                    Layout::horizontal(vec![Constraint::Ratio(1, columns as u32); columns])
                        .split(*row);
                rects.extend(col_rects.iter().copied());
            }
            rects.truncate(n);
            rects
        }
    }
}

/// Pick the pane that lies in `direction` from `current`, preferring panes
/// aligned on the other axis (so a 2x2 grid moves predictably).
fn pick_direction(areas: &[Rect], current: usize, direction: Direction) -> Option<usize> {
    let from = *areas.get(current)?;
    let (from_x, from_y) = center_of(from);
    let mut best: Option<(usize, i64, i64)> = None;
    for (index, rect) in areas.iter().enumerate() {
        if index == current {
            continue;
        }
        let (x, y) = center_of(*rect);
        let (primary, perpendicular) = match direction {
            Direction::Left if x < from_x => (from_x - x, (y - from_y).abs()),
            Direction::Right if x > from_x => (x - from_x, (y - from_y).abs()),
            Direction::Up if y < from_y => (from_y - y, (x - from_x).abs()),
            Direction::Down if y > from_y => (y - from_y, (x - from_x).abs()),
            _ => continue,
        };
        let better = match best {
            None => true,
            Some((_, best_perpendicular, best_primary)) => {
                (perpendicular, primary) < (best_perpendicular, best_primary)
            }
        };
        if better {
            best = Some((index, perpendicular, primary));
        }
    }
    best.map(|(index, _, _)| index)
}

fn center_of(rect: Rect) -> (i64, i64) {
    (
        rect.x as i64 + rect.width as i64 / 2,
        rect.y as i64 + rect.height as i64 / 2,
    )
}

fn draw_approval_card(frame: &mut Frame, area: Rect, approval: &PendingApproval) {
    let height = 4u16;
    if area.width < 20 || area.height < height + 2 {
        return;
    }
    let rect = Rect {
        x: area.x + 1,
        y: area.y + area.height - height - 1,
        width: area.width.saturating_sub(2),
        height,
    };
    frame.render_widget(Clear, rect);
    let summary = truncate(&approval.summary, rect.width.saturating_sub(14) as usize);
    let lines = vec![
        Line::from(vec![
            Span::styled(
                "⚠ ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(summary, Style::default().fg(Color::Yellow)),
        ]),
        Line::from(vec![
            Span::styled("[y] approve  ", Style::default().fg(Color::Green)),
            Span::styled("[n] deny  ", Style::default().fg(Color::Red)),
            Span::styled("[x] cancel", Style::default().fg(Color::DarkGray)),
        ]),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(format!(" approval · {} ", approval.tool));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::default().bg(Color::Black)),
        rect,
    );
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    Rect {
        x,
        y,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn status_color(status: SessionStatus) -> Color {
    match status {
        SessionStatus::Idle => Color::DarkGray,
        SessionStatus::Running => Color::Green,
        SessionStatus::AwaitingApproval => Color::Yellow,
        SessionStatus::AwaitingInput => Color::Cyan,
        SessionStatus::Error => Color::Red,
        SessionStatus::Paused => Color::Gray,
    }
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn truncate(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max {
        return text.to_string();
    }
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width + 1 > max {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out.push('…');
    out
}

fn fuzzy_score(query: &str, haystack: &str) -> Option<i32> {
    let mut haystack = haystack.chars();
    let mut score = 0;
    let mut previous_match = None;
    let mut position = 0usize;
    for needle in query.chars() {
        let mut found = false;
        for hay in haystack.by_ref() {
            position += 1;
            if hay == needle {
                score += if previous_match == Some(position - 1) {
                    3
                } else {
                    1
                };
                previous_match = Some(position);
                found = true;
                break;
            }
        }
        if !found {
            return None;
        }
    }
    Some(score)
}

const SPINNER: [&str; 4] = ["⠋", "⠙", "⠹", "⠸"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_edits_across_lines() {
        let mut input = Input::default();
        input.insert_str("hello\nworld");
        assert_eq!(input.text(), "hello\nworld");
        assert_eq!((input.row, input.col), (1, 5));

        input.home();
        input.delete_to_end();
        assert_eq!(input.text(), "hello\n");

        input.backspace();
        assert_eq!(input.text(), "hello");
        assert_eq!((input.row, input.col), (0, 5));

        input.insert_char('!');
        assert_eq!(input.text(), "hello!");
    }

    #[test]
    fn tabs_expand_to_spaces() {
        let mut input = Input::default();
        input.insert_char('\t');
        assert_eq!(input.text(), "    ");
    }

    #[test]
    fn wrapping_respects_display_width() {
        assert_eq!(wrap_text("abcdef", 3), vec!["abc", "def"]);
        assert_eq!(wrap_text("a\n\nb", 10), vec!["a", "", "b"]);
        assert_eq!(wrap_text("世界", 2), vec!["世", "界"]);
    }

    #[test]
    fn cursor_tracks_soft_wrap() {
        let mut input = Input::default();
        input.insert_str("abcdef");
        let layout = layout_input(&input, 3, 2);
        assert_eq!(layout.lines, vec!["abc", "def", ""]);
        assert_eq!(layout.cursor_line, 2);
        assert_eq!(layout.cursor_col, 0);
        assert_eq!(layout.offset, 1);
    }

    #[test]
    fn truncate_uses_cell_width() {
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("世界abc", 5), "世界…");
        assert_eq!(truncate("abc", 10), "abc");
    }

    #[test]
    fn fuzzy_matches_subsequences() {
        assert!(fuzzy_score("ses", "session").is_some());
        assert!(fuzzy_score("xyz", "session").is_none());
        assert!(fuzzy_score("", "anything").is_some());
    }

    #[test]
    fn pane_layouts_stay_inside_the_area() {
        let area = Rect::new(0, 0, 100, 40);
        for count in 1..=6 {
            let rects = pane_rects(area, count);
            assert_eq!(rects.len(), count);
            for rect in rects {
                assert!(rect.x >= area.x);
                assert!(rect.y >= area.y);
                assert!(rect.x + rect.width <= area.x + area.width);
                assert!(rect.y + rect.height <= area.y + area.height);
            }
        }

        let two = pane_rects(area, 2);
        assert!(two[0].x < two[1].x);
        assert_eq!(two[0].height, area.height);

        let three = pane_rects(area, 3);
        assert_eq!(three[0].height, area.height);
        assert_eq!(three[1].x, three[2].x);
        assert_eq!(three[1].width, three[2].width);
    }

    #[test]
    fn direction_prefers_aligned_neighbor() {
        let rects = pane_rects(Rect::new(0, 0, 100, 40), 4);
        assert_eq!(pick_direction(&rects, 0, Direction::Right), Some(1));
        assert_eq!(pick_direction(&rects, 0, Direction::Down), Some(2));
        assert_eq!(pick_direction(&rects, 3, Direction::Left), Some(2));
        assert_eq!(pick_direction(&rects, 3, Direction::Up), Some(1));
        assert_eq!(pick_direction(&rects, 0, Direction::Up), None);
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn transcript_renders_tool_cards() {
        let session = Session::new("s1", "tools", "/tmp");
        let events = vec![
            SessionEvent::new(
                "s1",
                1,
                EventKind::ToolCall,
                serde_json::json!({
                    "call_id": "c1",
                    "tool": "write_file",
                    "arguments": {"path": "out.txt", "content": "hello"},
                }),
            ),
            SessionEvent::new(
                "s1",
                2,
                EventKind::ToolResult,
                serde_json::json!({
                    "call_id": "c1",
                    "tool": "write_file",
                    "ok": true,
                    "denied": false,
                    "output": "wrote 5 bytes to out.txt",
                }),
            ),
        ];
        let view = SessionView::new(session, events);
        let text = App::transcript_lines(&view, 60)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("write_file out.txt (+5 bytes)"));
        assert!(text.contains("write_file → ok"));
        assert!(text.contains("wrote 5 bytes to out.txt"));
    }

    #[test]
    fn approval_keys_answer_the_focused_pane() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "tools", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;
        app.focus = Focus::Transcript;
        app.approvals.insert(
            "s1".to_string(),
            PendingApproval {
                approval_id: "a1".to_string(),
                tool: "write_file".to_string(),
                summary: "write_file out.txt".to_string(),
            },
        );

        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &tx);
        match rx.try_recv().expect("approval command") {
            Command::Approval {
                session_id,
                approval_id,
                decision,
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(approval_id, "a1");
                assert_eq!(decision, ApprovalDecision::Approve);
            }
            _ => panic!("expected an approval command"),
        }
        // The card stays until the daemon confirms the decision.
        assert!(app.approvals.contains_key("s1"));
    }
}
