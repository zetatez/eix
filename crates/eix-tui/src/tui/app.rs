//! TUI state and rendering.
//!
//! The prompt is an embedded Neovim when one can be spawned; the native
//! ratatui text box remains as the fallback (`[input] backend`).

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use eix_core::{
    ApprovalDecision, ApprovalRequiredPayload, AssistantMessagePayload, AttachResult,
    ContextEntryInfo, ContextLedgerResult, Event as SessionEvent, EventKind, HelloResult, HunkInfo,
    PatchProposedPayload, PatchResultPayload, PermissionLevel, PermissionPayload, PickerItem,
    ProcessInfo, ProcessList, ProcessOutput, ProcessStatus, RewindResult, SearchHit, Sector,
    Session, SessionStatus, SubmitResult, ToolCallPayload, ToolResultPayload, UserMessagePayload,
};
use eix_nvim::embedded::{NvimInput, Theme};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::controller::{Command, ContextOp, Event};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Transcript,
    Input,
}

/// Two `Esc` presses within this window leave the embedded prompt.
const ESC_WINDOW: Duration = Duration::from_millis(500);

/// True when a Neovim `mode_change` string means text is being inserted. An
/// empty string is the state before the first mode report, when the prompt is
/// still in insert mode.
fn nvim_is_insert(mode: &str) -> bool {
    mode.is_empty() || mode.starts_with("insert") || mode.starts_with("replace")
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

    fn set_text(&mut self, text: &str) {
        *self = Self::default();
        if text.is_empty() {
            return;
        }
        self.lines = text.split('\n').map(str::to_string).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = self.lines[self.row].chars().count();
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

    /// Find the pending approval or staged patch in the replayable log. A
    /// blocking tool approval wins (the turn is paused on it); otherwise the
    /// oldest pending patch is shown so stacked writes apply in order.
    fn pending_approval(&self) -> Option<PendingApproval> {
        let mut resolved_calls = HashSet::new();
        let mut resolved_patches = HashSet::new();
        for event in &self.events {
            match event.kind {
                EventKind::ToolResult => {
                    if let Ok(payload) =
                        serde_json::from_value::<ToolResultPayload>(event.payload.clone())
                    {
                        resolved_calls.insert(payload.call_id);
                    }
                }
                EventKind::PatchApplied | EventKind::PatchRejected | EventKind::PatchConflict => {
                    if let Ok(payload) =
                        serde_json::from_value::<PatchResultPayload>(event.payload.clone())
                    {
                        resolved_patches.insert(payload.patch_id);
                    }
                }
                _ => {}
            }
        }
        let mut tool = None;
        let mut patch = None;
        for event in &self.events {
            match event.kind {
                EventKind::ApprovalRequired if tool.is_none() => {
                    if let Ok(payload) =
                        serde_json::from_value::<ApprovalRequiredPayload>(event.payload.clone())
                    {
                        if !resolved_calls.contains(&payload.call_id) {
                            tool = Some(PendingApproval {
                                kind: PendingKind::Tool,
                                approval_id: payload.approval_id,
                                call_id: payload.call_id,
                                tool: payload.tool,
                                summary: payload.summary,
                                diff: payload.diff,
                                path: payload
                                    .arguments
                                    .get("path")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_string),
                                selected: vec![true; payload.hunks.len()],
                                hunks: payload.hunks,
                            });
                        }
                    }
                }
                EventKind::PatchProposed => {
                    if let Ok(payload) =
                        serde_json::from_value::<PatchProposedPayload>(event.payload.clone())
                    {
                        if !resolved_patches.contains(&payload.patch_id) {
                            // A later proposal for the same patch (an edit
                            // before approval) supersedes the earlier one.
                            patch = Some(PendingApproval {
                                kind: PendingKind::Patch,
                                approval_id: payload.patch_id,
                                call_id: payload.call_id,
                                tool: payload.tool,
                                summary: payload.summary,
                                diff: payload.diff,
                                path: Some(payload.path),
                                selected: vec![true; payload.hunks.len()],
                                hunks: payload.hunks,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        tool.or(patch)
    }
}

struct StreamState {
    turn_id: String,
    text: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PickerKind {
    /// Replace the focused pane's session.
    Sessions,
    /// Open the chosen session in a new pane (or focus it if already open).
    NewPane,
    /// Insert a workspace file after `@` in the prompt.
    Files,
    /// Insert a git worktree path into the prompt.
    Worktrees,
    /// Insert a git branch name into the prompt.
    Branches,
    /// Open the focused session's context ledger.
    Context,
    /// Client-side command palette (`:`).
    Commands,
}

impl PickerKind {
    /// Daemon provider backing this kind, if any.
    fn data_kind(self) -> Option<&'static str> {
        match self {
            Self::Files => Some(eix_core::KIND_FILES),
            Self::Worktrees => Some(eix_core::KIND_WORKTREES),
            Self::Branches => Some(eix_core::KIND_BRANCHES),
            Self::Context => Some(eix_core::KIND_CONTEXT),
            Self::Sessions | Self::NewPane | Self::Commands => None,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Sessions => "sessions — switch focused pane",
            Self::NewPane => "sessions — open in a new pane",
            Self::Files => "files — insert after @",
            Self::Worktrees => "worktrees — insert path",
            Self::Branches => "branches — insert name",
            Self::Context => "context — open ledger",
            Self::Commands => "commands",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Sessions => "Enter open · ↑/↓ · Esc close",
            Self::NewPane => "Enter split · ↑/↓ · Esc close",
            Self::Files => "Enter insert · ↑/↓ · Esc close",
            Self::Worktrees => "Enter insert path · ↑/↓ · Esc close",
            Self::Branches => "Enter insert name · ↑/↓ · Esc close",
            Self::Context => "Enter open ledger · ↑/↓ · Esc close",
            Self::Commands => "Enter run · ↑/↓ · Esc close",
        }
    }
}

struct Picker {
    kind: PickerKind,
    query: String,
    selected: usize,
    /// Rows for data providers and the command palette. Session kinds read
    /// `App::sessions` directly so live status changes show through.
    items: Vec<PickerItem>,
    /// True until the daemon answers a data kind, so the overlay can say
    /// "loading" instead of "no matches".
    loading: bool,
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

/// A tool call or staged patch the daemon is holding until the user decides.
#[derive(Clone, Copy, PartialEq, Eq)]
enum PendingKind {
    /// A blocking tool call (`run_command`) waiting in the turn loop.
    Tool,
    /// A staged `write_file` proposal; the turn has already continued.
    Patch,
}

/// A tool call or staged patch the daemon is holding until the user decides.
#[derive(Clone)]
struct PendingApproval {
    kind: PendingKind,
    approval_id: String,
    /// Tool call that produced this card, used to match `tool.completed`.
    call_id: String,
    tool: String,
    summary: String,
    diff: Option<String>,
    /// Workspace-relative target path, when the tool has one.
    path: Option<String>,
    /// Per-hunk preview for file-writing tools.
    hunks: Vec<HunkInfo>,
    /// Whether each hunk is selected; parallel to `hunks`.
    selected: Vec<bool>,
}

/// Inline reason for denying an approval (`r` on the card).
struct ReasonPrompt {
    session_id: String,
    text: String,
}

/// Cross-session list of pending approvals, opened with `A`.
struct InboxView {
    items: Vec<InboxItem>,
    cursor: usize,
}

struct InboxItem {
    session_id: String,
    session_name: String,
    tool: String,
    summary: String,
    kind: PendingKind,
}

/// Full-screen review of an approval's diff, opened with `d`.
struct DiffView {
    session_id: String,
    scroll: usize,
    /// Hunk under the selection cursor.
    cursor: usize,
}

/// Full-screen context ledger, opened with `c` from the transcript.
struct LedgerView {
    session_id: String,
    result: ContextLedgerResult,
    cursor: usize,
    scroll: usize,
    /// Entries whose detail is expanded.
    expanded: HashSet<String>,
    /// Source seq of the first entry in a pending compaction range.
    mark: Option<u64>,
}

impl LedgerView {
    fn new(session_id: String, result: ContextLedgerResult) -> Self {
        Self {
            session_id,
            result,
            cursor: 0,
            scroll: 0,
            expanded: HashSet::new(),
            mark: None,
        }
    }

    fn entry(&self) -> Option<&ContextEntryInfo> {
        self.result.entries.get(self.cursor)
    }

    fn set_result(&mut self, result: ContextLedgerResult) {
        self.result = result;
        self.cursor = self.cursor.min(self.result.entries.len().saturating_sub(1));
    }

    /// Event entries are named `e<seq>`; synthesized entries are not
    /// compactable.
    fn seq_of(entry: &ContextEntryInfo) -> Option<u64> {
        entry.id.strip_prefix('e')?.parse().ok()
    }
}

/// Full-screen background process list, opened with `P` from the transcript.
struct ProcessView {
    session_id: String,
    processes: Vec<ProcessInfo>,
    cursor: usize,
    /// Tail of the selected process.
    tail: String,
    /// Process the tail belongs to, so late chunks can be filtered.
    tail_id: Option<String>,
    /// Lines scrolled up from the end (0 follows the output).
    scroll: usize,
}

/// Live tail kept in the view; older bytes are dropped.
const PROCESS_TAIL_BYTES: usize = 128 * 1024;

impl ProcessView {
    fn new(session_id: String) -> Self {
        Self {
            session_id,
            processes: Vec::new(),
            cursor: 0,
            tail: String::new(),
            tail_id: None,
            scroll: 0,
        }
    }

    fn selected(&self) -> Option<&ProcessInfo> {
        self.processes.get(self.cursor)
    }

    /// Replace the list, keeping the cursor on the same process.
    fn set_list(&mut self, list: ProcessList) {
        let selected = self.selected().map(|process| process.id.clone());
        self.processes = list.processes;
        if let Some(id) = selected {
            if let Some(index) = self.processes.iter().position(|process| process.id == id) {
                self.cursor = index;
            }
        }
        self.cursor = self.cursor.min(self.processes.len().saturating_sub(1));
        if self.processes.is_empty() {
            self.tail.clear();
            self.tail_id = None;
            self.scroll = 0;
        }
    }

    fn set_tail(&mut self, output: ProcessOutput) {
        self.tail = output.text;
        self.tail_id = Some(output.process_id);
        self.scroll = 0;
    }

    /// Append a live chunk when it belongs to the shown process.
    fn append(&mut self, process_id: &str, chunk: &str) {
        if self.tail_id.as_deref() != Some(process_id) {
            return;
        }
        self.tail.push_str(chunk);
        if self.tail.len() > PROCESS_TAIL_BYTES {
            let mut cut = self.tail.len() - PROCESS_TAIL_BYTES;
            while !self.tail.is_char_boundary(cut) {
                cut += 1;
            }
            self.tail = self.tail.split_off(cut);
        }
    }

    /// Reflect a `process.exited` notification in the list.
    fn mark_exited(&mut self, process_id: &str, status: ProcessStatus, exit_code: Option<i32>) {
        if let Some(process) = self
            .processes
            .iter_mut()
            .find(|process| process.id == process_id)
        {
            process.status = status;
            process.exit_code = exit_code;
        }
    }
}

/// Full-screen history search, opened with `#` from the transcript.
struct SearchView {
    query: String,
    hits: Vec<SearchHit>,
    cursor: usize,
    /// A request is in flight.
    searching: bool,
    /// The query changed since the shown hits were produced, so Enter searches
    /// again instead of opening the selection.
    dirty: bool,
}

impl SearchView {
    fn new() -> Self {
        Self {
            query: String::new(),
            hits: Vec::new(),
            cursor: 0,
            searching: false,
            dirty: false,
        }
    }

    fn selected(&self) -> Option<&SearchHit> {
        self.hits.get(self.cursor)
    }

    fn move_cursor(&mut self, delta: isize) {
        if self.hits.is_empty() {
            return;
        }
        let len = self.hits.len() as isize;
        self.cursor = (self.cursor as isize + delta).clamp(0, len - 1) as usize;
    }
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
    /// Full-screen diff review, when open.
    diff_view: Option<DiffView>,
    /// Full-screen context ledger, when open.
    ledger: Option<LedgerView>,
    /// Full-screen background process list, when open.
    process_view: Option<ProcessView>,
    /// Full-screen history search, when open.
    search_view: Option<SearchView>,
    status: String,
    error: Option<String>,
    connected: bool,
    tick: u64,
    /// Ring the terminal bell when a background pane needs attention.
    pub bell: bool,
    /// Emit OSC 9/777 notifications and update the title on attention.
    pub osc: bool,
    /// Set when a notification should ring; cleared by the event loop.
    ring: bool,
    /// Text for the next alert (session name + status).
    last_alert: Option<String>,
    /// Embedded Neovim prompt, when available.
    nvim: Option<NvimInput>,
    /// In-flight restart after the prompt exited (`:q`, crash).
    nvim_restart: Option<std::sync::mpsc::Receiver<Result<NvimInput, anyhow::Error>>>,
    /// Draft captured when the prompt exited, restored after the restart.
    pending_nvim_text: Option<String>,
    /// Session whose daemon draft is being loaded into a fresh buffer.
    draft_loading: Option<String>,
    /// Last draft submitted from the Neovim prompt (session id, text), kept so
    /// a failed turn can put it back into the buffer.
    nvim_draft: Option<(String, String)>,
    /// Session the native prompt currently belongs to, for daemon drafts.
    input_session: Option<String>,
    /// Text last written to the daemon, so `tick` only saves real changes.
    last_saved_draft: String,
    /// Open reason input for a denial.
    reason_prompt: Option<ReasonPrompt>,
    /// Ask the event loop to clear ratatui's cached frame after an editor.
    force_clear: bool,
    /// Cross-session approvals overlay.
    inbox: Option<InboxView>,
    /// Colors sampled from the embedded Neovim's colorscheme.
    theme: Theme,
    /// When the previous `Esc` reached the embedded prompt, for the double-Esc
    /// "leave the input" gesture.
    esc_at: Option<Instant>,
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
            diff_view: None,
            ledger: None,
            process_view: None,
            search_view: None,
            status: "connecting…".into(),
            error: None,
            connected: false,
            tick: 0,
            bell: false,
            osc: false,
            ring: false,
            last_alert: None,
            nvim: None,
            nvim_restart: None,
            pending_nvim_text: None,
            draft_loading: None,
            nvim_draft: None,
            input_session: None,
            last_saved_draft: String::new(),
            reason_prompt: None,
            force_clear: false,
            inbox: None,
            theme: Theme::default(),
            esc_at: None,
        }
    }

    /// Attach an embedded-Neovim prompt, replacing the native text box.
    pub fn set_nvim_input(&mut self, input: NvimInput) {
        self.nvim = Some(input);
    }

    pub fn has_nvim_input(&self) -> bool {
        self.nvim.is_some()
    }

    /// Collect drafts submitted with `<CR>` in the embedded prompt, plus file
    /// picker requests, and swap in a restarted prompt when it is ready.
    pub fn poll_nvim(&mut self, tx: &UnboundedSender<Command>) {
        self.poll_nvim_restart(tx);

        let Some((theme, file_picker, drafts)) = self
            .nvim
            .as_ref()
            .map(|nvim| (nvim.theme(), nvim.take_file_picker(), nvim.take_submitted()))
        else {
            return;
        };
        self.theme = theme;
        if file_picker && self.overlay_closed() {
            self.open_picker(PickerKind::Files, tx);
        }
        for draft in drafts {
            if draft.trim().is_empty() {
                continue;
            }
            let session_id = self.focused_view().map(|view| view.session.id.clone());
            match self.begin_submit(&draft, tx) {
                Ok(()) => {
                    if let Some(session_id) = session_id {
                        self.nvim_draft = Some((session_id, draft));
                    }
                    if let Some(nvim) = &self.nvim {
                        nvim.reset();
                    }
                }
                Err(message) => {
                    self.error = Some(message);
                    self.status = "prompt kept".into();
                }
            }
        }
    }

    /// True when no full-screen overlay owns the keyboard, so a prompt request
    /// can safely open the picker.
    fn overlay_closed(&self) -> bool {
        self.picker.is_none()
            && self.diff_view.is_none()
            && self.ledger.is_none()
            && self.process_view.is_none()
            && self.search_view.is_none()
            && self.inbox.is_none()
            && self.reason_prompt.is_none()
    }

    /// Take an exit reported by the prompt and start a replacement Neovim.
    fn poll_nvim_restart(&mut self, tx: &UnboundedSender<Command>) {
        let quit = self.nvim.as_ref().and_then(NvimInput::take_quit);
        if let Some(text) = quit {
            self.restart_nvim(text, tx);
            return;
        }
        let Some(rx) = &self.nvim_restart else {
            return;
        };
        match rx.try_recv() {
            Ok(Ok(nvim)) => {
                self.nvim_restart = None;
                self.nvim = Some(nvim);
                let text = self.pending_nvim_text.take().unwrap_or_default();
                // Restore through the daemon so the buffer and its stored draft
                // cannot diverge: `restart_nvim` already wrote `text`.
                if let Some(session_id) = self.input_session.clone() {
                    self.last_saved_draft.clear();
                    self.draft_loading = Some(session_id.clone());
                    let _ = tx.send(Command::DraftGet { session_id });
                } else if let Some(nvim) = &self.nvim {
                    if text.is_empty() {
                        nvim.reset();
                    } else {
                        nvim.set_text(&text);
                    }
                    self.last_saved_draft = text;
                }
                self.status = "prompt restarted".into();
            }
            Ok(Err(err)) => {
                self.nvim_restart = None;
                self.error = Some(format!("nvim restart failed: {err:#}"));
                self.status = "using the native prompt".into();
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.nvim_restart = None;
                self.status = "prompt restart lost".into();
            }
        }
    }

    /// Drop the dead prompt and spawn a replacement in the background.
    fn restart_nvim(&mut self, text: String, tx: &UnboundedSender<Command>) {
        let Some(nvim) = self.nvim.take() else {
            return;
        };
        let (width, height) = nvim.size();
        let minimal = nvim.used_minimal_fallback();
        // Persist the captured draft so another frontend can still recover it.
        if let Some(session_id) = self.input_session.clone() {
            let _ = tx.send(Command::DraftSet {
                session_id,
                text: text.clone(),
            });
        }
        self.status = "prompt exited; restarting nvim…".into();
        self.pending_nvim_text = Some(text);
        let (tx, rx) = std::sync::mpsc::channel();
        self.nvim_restart = Some(rx);
        tokio::spawn(async move {
            let _ = tx.send(NvimInput::spawn(width, height, minimal).await);
        });
    }

    /// Put the last Neovim draft back after a failed turn, if it belongs to
    /// `session_id`.
    fn restore_nvim_draft(&mut self, session_id: &str) {
        let matches = self
            .nvim_draft
            .as_ref()
            .is_some_and(|(id, _)| id == session_id);
        if !matches {
            return;
        }
        if let Some((_, draft)) = self.nvim_draft.take() {
            if let Some(nvim) = &self.nvim {
                nvim.set_text(&draft);
            }
        }
    }

    /// Drop the saved draft once its turn finishes successfully.
    fn clear_nvim_draft(&mut self, session_id: &str) {
        if self
            .nvim_draft
            .as_ref()
            .is_some_and(|(id, _)| id == session_id)
        {
            self.nvim_draft = None;
        }
    }

    /// Set the footer status line.
    pub fn set_status(&mut self, message: impl Into<String>) {
        self.status = message.into();
    }

    /// Take the pending bell request, if any.
    pub fn take_ring(&mut self) -> bool {
        std::mem::take(&mut self.ring)
    }

    /// Text for the most recent background alert.
    pub fn alert_message(&self) -> Option<String> {
        self.last_alert.clone()
    }

    /// True once after an external editor ran, so the frame is fully repainted.
    pub fn take_force_clear(&mut self) -> bool {
        std::mem::take(&mut self.force_clear)
    }

    pub fn tick(&mut self, tx: &UnboundedSender<Command>) {
        self.tick = self.tick.wrapping_add(1);
        self.sync_draft(tx);
    }

    /// Persist the prompt as a per-session draft when the focused pane changes
    /// or the text is edited. Uses the Neovim buffer mirror when the embedded
    /// prompt is active, the native text box otherwise.
    fn sync_draft(&mut self, tx: &UnboundedSender<Command>) {
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
            self.input.clear();
            self.last_saved_draft.clear();
            self.input_session = focused;
            if let Some(session_id) = self.input_session.clone() {
                if let Some(nvim) = &self.nvim {
                    // Clear the buffer so the daemon's draft can replace it.
                    self.draft_loading = Some(session_id.clone());
                    nvim.reset();
                }
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

    /// Current prompt contents: the Neovim mirror or the native text box.
    fn current_draft_text(&self) -> String {
        match &self.nvim {
            Some(nvim) => nvim.draft_text(),
            None => self.input.text(),
        }
    }

    pub fn handle_paste(&mut self, text: &str) {
        self.focus = Focus::Input;
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        if let Some(nvim) = &self.nvim {
            nvim.start_insert();
            nvim.paste(&normalized);
        } else {
            self.input.insert_str(&normalized);
        }
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
                        self.create_session(tx, false);
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
            Event::Forked(session) => {
                let id = session.id.clone();
                let name = session.name.clone();
                let worktree = session.worktree.clone();
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
                self.status = match worktree {
                    Some(path) => format!("forked {name} · worktree {}", path.display()),
                    None => format!("forked {name} · shared workspace"),
                };
                let _ = tx.send(Command::Attach(id));
            }
            Event::Rewound(result) => {
                let RewindResult {
                    session,
                    restored,
                    snapshots,
                    files_restored,
                    files_removed,
                    files_missing,
                    warnings,
                } = *result;
                let id = session.id.clone();
                let name = session.name.clone();
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
                let mut status = format!(
                    "rewound {name} · {snapshots} snapshot(s) · {files_restored} restored, {files_removed} removed"
                );
                if files_missing > 0 {
                    status.push_str(&format!(", {files_missing} missing"));
                }
                if !restored {
                    status.push_str(" · INCOMPLETE");
                }
                if let Some(warning) = warnings.first() {
                    status.push_str(&format!(" · {warning}"));
                }
                self.status = status;
                let _ = tx.send(Command::Attach(id));
            }
            Event::Search(result) => {
                let Some(view) = &mut self.search_view else {
                    return;
                };
                view.searching = false;
                view.hits = result.hits;
                view.cursor = 0;
                view.dirty = false;
                let count = view.hits.len();
                self.status = if count == 0 {
                    format!("no matches for {:?}", result.query)
                } else {
                    format!("{count} hit(s) · Enter opens the selected session")
                };
            }
            Event::PickerData { kind, items } => {
                let count = items.len();
                if let Some(picker) = &mut self.picker {
                    if picker.kind.data_kind() == Some(kind.as_str()) {
                        picker.items = items;
                        picker.selected = 0;
                        picker.loading = false;
                    }
                }
                if count == 0 {
                    self.status = format!("no {kind} matched");
                } else {
                    self.status = format!("{count} {kind} row(s)");
                }
            }
            Event::Draft { session_id, text } => {
                if self.input_session.as_deref() != Some(session_id.as_str()) {
                    return;
                }
                if let Some(nvim) = &self.nvim {
                    // The buffer was cleared on switch; load the daemon draft.
                    if text.is_empty() {
                        nvim.reset();
                    } else {
                        nvim.set_text(&text);
                    }
                    self.last_saved_draft = text;
                    self.draft_loading = None;
                } else if self.input.is_empty() {
                    // Prefill only the prompt the draft belongs to, and only
                    // when the user has not already started typing.
                    if !text.is_empty() {
                        self.input.set_text(&text);
                    }
                    self.last_saved_draft = self.input.text();
                }
            }
            Event::PatchEdit {
                session_id,
                patch_id,
                text,
            } => self.edit_patch(&session_id, &patch_id, &text, tx),
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
                    let sessions: Vec<String> = self
                        .approvals
                        .iter()
                        .filter(|(_, pending)| pending.approval_id == approval_id)
                        .map(|(session_id, _)| session_id.clone())
                        .collect();
                    for session_id in sessions {
                        self.reload_pending(&session_id);
                    }
                    self.status = "approval sent".into();
                } else {
                    self.status = "approval was stale; the daemon is still waiting".into();
                }
            }
            Event::Ledger { session_id, result } => {
                if let Some(ledger) = &mut self.ledger {
                    if ledger.session_id == session_id {
                        ledger.set_result(result);
                        self.status = format!(
                            "context: {} / {} tokens",
                            ledger.result.total_tokens, ledger.result.budget_tokens
                        );
                    }
                }
            }
            Event::Processes { session_id, result } => {
                if let Some(view) = &mut self.process_view {
                    if view.session_id == session_id {
                        let selected = view.selected().map(|process| process.id.clone());
                        view.set_list(result);
                        self.status = format!("processes: {}", view.processes.len());
                        // Load the tail of the (possibly new) selection.
                        let next = view.selected().map(|process| process.id.clone());
                        if next != selected {
                            if let Some(process_id) = next {
                                let _ = tx.send(Command::ProcessOutput { process_id });
                            }
                        }
                    }
                }
            }
            Event::ProcessOutput { result } => {
                if let Some(view) = &mut self.process_view {
                    if view.selected().map(|process| process.id.as_str())
                        == Some(result.process_id.as_str())
                    {
                        view.set_tail(result);
                    }
                }
            }
            Event::ProcessKilled { process_id, killed } => {
                self.status = if killed {
                    format!("killing process {}", short_id(&process_id))
                } else {
                    format!("process {} is not running", short_id(&process_id))
                };
            }
            Event::Notification(notification) => self.notification(notification, tx),
            Event::Error { context, message } => {
                self.error = Some(message.clone());
                self.status = format!("{context} failed");
                match context.as_str() {
                    "submit" => {
                        self.submitting = false;
                        self.pending_prompt = None;
                        // The prompt was cleared when the request was sent;
                        // put the draft back so it can be edited or retried.
                        if let Some((_, draft)) = self.nvim_draft.take() {
                            if let Some(nvim) = &self.nvim {
                                nvim.set_text(&draft);
                            }
                        }
                    }
                    "create" => self.creating = false,
                    "attach" => self.pending_attach = None,
                    "connect" => {
                        self.connected = false;
                        self.initialized = true;
                    }
                    "list" if !self.initialized => self.initialized = true,
                    "draft" => self.draft_loading = None,
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
        self.reload_pending(&session_id);
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

    fn notification(
        &mut self,
        notification: eix_core::Notification,
        tx: &UnboundedSender<Command>,
    ) {
        match notification.method.as_str() {
            "session.updated" => {
                if let Ok(session) = serde_json::from_value::<Session>(notification.params) {
                    let id = session.id.clone();
                    let index = self.views.iter().position(|view| view.session.id == id);
                    if let Some(index) = index {
                        self.views[index].session = session.clone();
                        if index != self.focused && self.pane_needs_attention(index) {
                            self.status = format!("{} needs attention", session.name);
                        }
                    }
                    self.upsert_session(session, false);
                }
            }
            "session.attention" => {
                if let Ok(params) = serde_json::from_value::<AttentionParams>(notification.params) {
                    let mut alert = false;
                    if let Some((index, view)) = self
                        .views
                        .iter_mut()
                        .enumerate()
                        .find(|(_, view)| view.session.id == params.session_id)
                    {
                        view.session.status = params.status;
                        if index != self.focused {
                            self.status = format!(
                                "{} · {} in pane {}",
                                view.session.name,
                                params.status.as_str(),
                                index + 1
                            );
                            alert = true;
                        }
                    }
                    if alert {
                        self.last_alert = self
                            .views
                            .iter()
                            .find(|view| view.session.id == params.session_id)
                            .map(|view| {
                                format!("{} · {}", view.session.name, params.status.as_str())
                            });
                        if self.bell || self.osc {
                            self.ring = true;
                        }
                    }
                }
            }
            "session.event" => {
                if let Ok(event) = serde_json::from_value::<SessionEvent>(notification.params) {
                    let id = event.session_id.clone();
                    let relevant = matches!(
                        event.kind,
                        EventKind::ToolResult
                            | EventKind::ApprovalRequired
                            | EventKind::PatchProposed
                            | EventKind::PatchApplied
                            | EventKind::PatchRejected
                            | EventKind::PatchConflict
                    );
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
                    if relevant {
                        self.reload_pending(&id);
                    }
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
                            kind: PendingKind::Tool,
                            approval_id: params.approval_id,
                            call_id: params.call_id,
                            tool: params.tool,
                            summary: params.summary,
                            selected: vec![true; params.hunks.len()],
                            path: params
                                .arguments
                                .get("path")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string),
                            diff: params.diff,
                            hunks: params.hunks,
                        },
                    );
                }
            }
            "patch.proposed" => {
                if let Ok(params) =
                    serde_json::from_value::<PatchProposedParams>(notification.params)
                {
                    if let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == params.session_id)
                    {
                        view.follow = true;
                    }
                    self.status = format!("patch staged: {}", params.path);
                    self.reload_pending(&params.session_id);
                }
            }
            "patch.applied" | "patch.rejected" | "patch.conflict" => {
                if let Ok(params) =
                    serde_json::from_value::<PatchOutcomeParams>(notification.params)
                {
                    let message = match notification.method.as_str() {
                        "patch.applied" => format!("patch applied: {}", params.path),
                        "patch.rejected" => format!("patch rejected: {}", params.path),
                        _ => format!("patch conflict: {}", params.path),
                    };
                    self.status = message;
                    self.reload_pending(&params.session_id);
                    self.close_diff_view_for(&params.session_id);
                }
            }
            "tool.completed" => {
                if let Ok(params) = serde_json::from_value::<ToolCompleted>(notification.params) {
                    let matches_completed =
                        self.approvals
                            .get(&params.session_id)
                            .is_some_and(|pending| {
                                pending.kind == PendingKind::Tool
                                    && (params.call_id.is_empty()
                                        || pending.call_id == params.call_id)
                            });
                    if matches_completed {
                        self.reload_pending(&params.session_id);
                        self.close_diff_view_for(&params.session_id);
                    }
                }
            }
            "turn.completed" => {
                if let Ok(done) = serde_json::from_value::<TurnCompleted>(notification.params) {
                    // A blocking approval belongs to the finished turn; a staged
                    // patch survives until the user decides.
                    if self
                        .approvals
                        .get(&done.session_id)
                        .is_some_and(|pending| pending.kind == PendingKind::Tool)
                    {
                        self.reload_pending(&done.session_id);
                        self.close_diff_view_for(&done.session_id);
                    }
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
                        // The prompt was cleared on submit; restore the draft so
                        // a provider failure does not lose it.
                        self.restore_nvim_draft(&done.session_id);
                    } else if done.cancelled {
                        self.status = "cancelled".into();
                    } else {
                        self.status = "idle".into();
                        self.clear_nvim_draft(&done.session_id);
                    }
                }
            }
            "context.updated" => {
                if let Ok(params) =
                    serde_json::from_value::<ContextUpdatedParams>(notification.params)
                {
                    if let Some(ledger) = &mut self.ledger {
                        if ledger.session_id == params.session_id {
                            ledger.set_result(params.ledger);
                        }
                    }
                }
            }
            "process.started" => {
                if let Ok(info) = serde_json::from_value::<ProcessInfo>(notification.params) {
                    if self
                        .views
                        .iter()
                        .any(|view| view.session.id == info.session_id)
                    {
                        self.status =
                            format!("process {} started: {}", short_id(&info.id), info.command);
                    }
                    if let Some(view) = &self.process_view {
                        if view.session_id == info.session_id {
                            let session_id = view.session_id.clone();
                            let _ = tx.send(Command::ProcessList { session_id });
                        }
                    }
                }
            }
            "process.output" => {
                if let Ok(params) =
                    serde_json::from_value::<ProcessChunkParams>(notification.params)
                {
                    if let Some(view) = &mut self.process_view {
                        if view.session_id == params.session_id {
                            view.append(&params.process_id, &params.chunk);
                        }
                    }
                }
            }
            "process.exited" => {
                if let Ok(params) =
                    serde_json::from_value::<ProcessExitedParams>(notification.params)
                {
                    let status = params.status;
                    if self
                        .views
                        .iter()
                        .any(|view| view.session.id == params.session_id)
                    {
                        self.status = format!(
                            "process {} {}{}",
                            short_id(&params.process_id),
                            status.as_str(),
                            params
                                .exit_code
                                .map(|code| format!(" ({code})"))
                                .unwrap_or_default()
                        );
                    }
                    if let Some(view) = &mut self.process_view {
                        if view.session_id == params.session_id {
                            view.mark_exited(&params.process_id, status, params.exit_code);
                            if view.selected().map(|process| process.id.as_str())
                                == Some(params.process_id.as_str())
                            {
                                let _ = tx.send(Command::ProcessOutput {
                                    process_id: params.process_id,
                                });
                            }
                        }
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
            // With the embedded editor focused, `<C-c>` belongs to Neovim
            // (leave insert / interrupt). Quitting stays a transcript binding
            // so typing in the prompt cannot kill the app by accident.
            if self.nvim_has_focus() {
                self.handle_nvim_input_key(key);
            } else {
                self.should_quit = true;
            }
            return;
        }
        if matches!(key.code, KeyCode::F(1)) {
            if self.nvim_has_focus() {
                self.handle_nvim_input_key(key);
            } else {
                self.help = true;
            }
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

        if self.reason_prompt.is_some() {
            match key.code {
                KeyCode::Esc => {
                    self.reason_prompt = None;
                    self.status = "denial cancelled".into();
                }
                KeyCode::Enter => {
                    if let Some(prompt) = self.reason_prompt.take() {
                        let reason = prompt.text.trim().to_string();
                        self.respond_approval_for(
                            &prompt.session_id,
                            ApprovalDecision::Deny,
                            None,
                            (!reason.is_empty()).then_some(reason),
                            tx,
                        );
                    }
                }
                KeyCode::Backspace => {
                    if let Some(prompt) = self.reason_prompt.as_mut() {
                        prompt.text.pop();
                    }
                }
                KeyCode::Char(ch) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(prompt) = self.reason_prompt.as_mut() {
                        prompt.text.push(ch);
                    }
                }
                _ => {}
            }
            return;
        }

        if self.search_view.is_some() {
            self.handle_search_key(key, tx);
            return;
        }

        if self.ledger.is_some() {
            self.handle_ledger_key(key, tx);
            return;
        }

        if self.process_view.is_some() {
            self.handle_process_key(key, tx);
            return;
        }

        if self.inbox.is_some() {
            self.handle_inbox_key(key, tx);
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
        // `<C-g>` is the cross-mode session picker, but a real editor in normal
        // mode gets Neovim's own binding (file info) so Vim users are not
        // surprised. In insert mode it still opens the picker.
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('g')) {
            if self.nvim.is_some() && !self.nvim_insert_mode() {
                self.handle_nvim_input_key(key);
            } else {
                self.open_picker(PickerKind::Sessions, tx);
            }
            return;
        }

        if self.nvim.is_some() {
            self.handle_nvim_input_key(key);
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

    /// True when keys should go to the embedded editor rather than the TUI.
    fn nvim_has_focus(&self) -> bool {
        self.focus == Focus::Input && self.nvim.is_some() && !self.help && self.overlay_closed()
    }

    /// True when the embedded prompt is taking typed text (insert/replace).
    fn nvim_insert_mode(&self) -> bool {
        nvim_is_insert(&self.nvim.as_ref().map(NvimInput::mode).unwrap_or_default())
    }

    fn handle_nvim_input_key(&mut self, key: KeyEvent) {
        let inserting = self.nvim_insert_mode();
        match key.code {
            // A single Esc leaves insert mode but keeps the prompt focused, so
            // normal-mode Vim editing works. A second Esc within the window
            // hands the keyboard back to the transcript.
            KeyCode::Esc => {
                let now = Instant::now();
                let double = self
                    .esc_at
                    .is_some_and(|last| now.duration_since(last) < ESC_WINDOW);
                if let Some(nvim) = &self.nvim {
                    nvim.input(key);
                }
                if double {
                    self.esc_at = None;
                    self.focus = Focus::Transcript;
                } else {
                    self.esc_at = Some(now);
                }
            }
            // Shift/Ctrl-Enter and Ctrl-J insert a newline, matching the
            // native prompt. They are only intercepted while inserting: in
            // normal mode they are ordinary Vim motions. Completion plugins
            // often claim <C-j> for menu navigation, so ask Neovim for a
            // literal newline instead.
            KeyCode::Enter
                if inserting
                    && key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::SHIFT) =>
            {
                self.esc_at = None;
                if let Some(nvim) = &self.nvim {
                    nvim.newline();
                }
            }
            KeyCode::Char('j') if inserting && key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.esc_at = None;
                if let Some(nvim) = &self.nvim {
                    nvim.newline();
                }
            }
            _ => {
                self.esc_at = None;
                if let Some(nvim) = &self.nvim {
                    nvim.input(key);
                }
            }
        }
    }

    fn focus_input(&mut self) {
        self.focus = Focus::Input;
        self.esc_at = None;
        if let Some(nvim) = &self.nvim {
            nvim.start_insert();
        }
    }

    fn handle_transcript_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        if self.diff_view.is_some() {
            self.handle_diff_key(key, tx);
            return;
        }
        if self.pending_window {
            self.pending_window = false;
            self.handle_window_key(key, tx);
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('g')) {
            self.open_picker(PickerKind::Sessions, tx);
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
                KeyCode::Char('d') => {
                    self.open_diff_view();
                    return;
                }
                KeyCode::Char('e') => {
                    self.edit_proposed(tx);
                    return;
                }
                KeyCode::Char('o') => {
                    self.open_approval_file();
                    return;
                }
                KeyCode::Char('r') => {
                    self.start_reason_prompt();
                    return;
                }
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('i') | KeyCode::Enter => self.focus_input(),
            KeyCode::Char('n') => self.create_session(tx, false),
            KeyCode::Char('N') => self.create_session(tx, true),
            KeyCode::Char('p') => self.open_picker(PickerKind::Sessions, tx),
            KeyCode::Char('o') => self.open_picker(PickerKind::NewPane, tx),
            KeyCode::Char(':') => self.open_picker(PickerKind::Commands, tx),
            KeyCode::Char('w') => self.open_picker(PickerKind::Worktrees, tx),
            KeyCode::Char('b') => self.open_picker(PickerKind::Branches, tx),
            KeyCode::Char('c') => self.open_ledger(tx),
            KeyCode::Char('P') => self.open_processes(tx),
            KeyCode::Char('A') => self.open_inbox(),
            KeyCode::Char('#') => {
                self.search_view = Some(SearchView::new());
                self.status = "search history · Enter runs · ↑/↓ select · Esc closes".into();
            }
            KeyCode::Char('F') => self.fork_focused(tx, None),
            KeyCode::Char('a') => self.focus_next_attention(),
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

    fn open_ledger(&mut self, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_view().map(|view| view.session.id.clone()) else {
            self.status = "no session to inspect".into();
            return;
        };
        self.ledger = Some(LedgerView::new(
            session_id.clone(),
            ContextLedgerResult::default(),
        ));
        self.status = "context ledger: loading…".into();
        let _ = tx.send(Command::ContextLedger { session_id });
    }

    fn handle_ledger_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.ledger = None;
            self.status = "context ledger closed".into();
            return;
        }
        let Some(ledger) = &mut self.ledger else {
            return;
        };
        let len = ledger.result.entries.len();
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => {
                if ledger.cursor + 1 < len {
                    ledger.cursor += 1;
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                ledger.cursor = ledger.cursor.saturating_sub(1);
            }
            KeyCode::PageDown => {
                ledger.cursor = (ledger.cursor + 10).min(len.saturating_sub(1));
            }
            KeyCode::PageUp => {
                ledger.cursor = ledger.cursor.saturating_sub(10);
            }
            KeyCode::Char('g') => ledger.cursor = 0,
            KeyCode::Char('G') => ledger.cursor = len.saturating_sub(1),
            KeyCode::Char('o') | KeyCode::Enter => {
                if let Some(entry) = ledger.entry() {
                    let id = entry.id.clone();
                    if !ledger.expanded.remove(&id) {
                        ledger.expanded.insert(id);
                    }
                }
            }
            KeyCode::Char('p') => {
                let Some(entry) = ledger.entry() else { return };
                if !entry.editable {
                    self.status = "this entry cannot be pinned".into();
                    return;
                }
                let op = ContextOp::Pin {
                    entry: entry.id.clone(),
                    pinned: !entry.pinned,
                };
                let session_id = ledger.session_id.clone();
                let _ = tx.send(Command::ContextOp { session_id, op });
            }
            KeyCode::Char('d') => {
                let Some(entry) = ledger.entry() else { return };
                if !entry.editable {
                    self.status = "this entry cannot be dropped".into();
                    return;
                }
                let op = ContextOp::Drop {
                    entry: entry.id.clone(),
                    dropped: !entry.dropped,
                };
                let session_id = ledger.session_id.clone();
                let _ = tx.send(Command::ContextOp { session_id, op });
            }
            KeyCode::Char('m') => {
                let Some(entry) = ledger.entry() else { return };
                let Some(seq) = LedgerView::seq_of(entry) else {
                    self.status = "only message entries can be marked".into();
                    return;
                };
                if ledger.mark == Some(seq) {
                    ledger.mark = None;
                    self.status = "mark cleared".into();
                } else {
                    ledger.mark = Some(seq);
                    self.status = format!("marked e{seq}; move and press s to compact the range");
                }
            }
            KeyCode::Char('s') => {
                let Some(entry) = ledger.entry() else { return };
                let Some(seq) = LedgerView::seq_of(entry) else {
                    self.status = "only message entries can be compacted".into();
                    return;
                };
                let (from, to) = match ledger.mark {
                    Some(mark) if mark != seq => (mark.min(seq), mark.max(seq)),
                    _ => (seq, seq),
                };
                let op = ContextOp::Compact {
                    from_seq: from,
                    to_seq: to,
                };
                let session_id = ledger.session_id.clone();
                ledger.mark = None;
                let _ = tx.send(Command::ContextOp { session_id, op });
                self.status = format!("compacting {from}..={to}…");
            }
            KeyCode::Char('f') => {
                let Some(entry) = ledger.entry() else { return };
                let Some(seq) = LedgerView::seq_of(entry) else {
                    self.status = "only message entries can be forked".into();
                    return;
                };
                let session_id = ledger.session_id.clone();
                let _ = tx.send(Command::Fork {
                    session_id,
                    at_seq: Some(seq),
                    name: None,
                });
                self.status = format!("forking before e{seq} · files unchanged");
            }
            KeyCode::Char('w') => {
                let Some(entry) = ledger.entry() else { return };
                let Some(seq) = LedgerView::seq_of(entry) else {
                    self.status = "only message entries can be rewound".into();
                    return;
                };
                let session_id = ledger.session_id.clone();
                let _ = tx.send(Command::Rewind {
                    session_id,
                    at_seq: Some(seq),
                    name: None,
                });
                self.status = format!("rewinding before e{seq} · restoring the workspace…");
            }
            _ => {}
        }
    }

    /// Fork the focused session. `at_seq` copies history strictly before that
    /// sequence (used by the ledger's rewind); `None` copies everything.
    fn fork_focused(&mut self, tx: &UnboundedSender<Command>, at_seq: Option<u64>) {
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

    fn open_processes(&mut self, tx: &UnboundedSender<Command>) {
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
    fn open_inbox(&mut self) {
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

    fn handle_inbox_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
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

    fn handle_process_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        if matches!(key.code, KeyCode::Esc | KeyCode::Char('q')) {
            self.process_view = None;
            self.status = "processes closed".into();
            return;
        }
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

    fn handle_window_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        match key.code {
            KeyCode::Char('h') | KeyCode::Left => self.focus_direction(Direction::Left),
            KeyCode::Char('l') | KeyCode::Right => self.focus_direction(Direction::Right),
            KeyCode::Char('j') | KeyCode::Down => self.focus_direction(Direction::Down),
            KeyCode::Char('k') | KeyCode::Up => self.focus_direction(Direction::Up),
            KeyCode::Char('v') | KeyCode::Char('s') => self.open_picker(PickerKind::NewPane, tx),
            KeyCode::Char('c') | KeyCode::Char('q') => self.close_focused_pane(),
            KeyCode::Char('w') => self.cycle_pane(1),
            KeyCode::Esc => self.status = "window command cancelled".into(),
            _ => {}
        }
    }

    /// History search. Typing edits the query; the first Enter runs it, then
    /// Enter opens the selected hit. Up/Down browse results.
    fn handle_search_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        enum Action {
            Close,
            Empty,
            Run(String),
            Open(String),
        }

        let action = {
            let Some(view) = &mut self.search_view else {
                return;
            };
            match key.code {
                KeyCode::Esc => Some(Action::Close),
                KeyCode::Up => {
                    view.move_cursor(-1);
                    None
                }
                KeyCode::Down => {
                    view.move_cursor(1);
                    None
                }
                KeyCode::Backspace => {
                    view.query.pop();
                    view.cursor = 0;
                    view.dirty = true;
                    None
                }
                KeyCode::Enter => {
                    let query = view.query.trim().to_string();
                    if query.is_empty() {
                        Some(Action::Empty)
                    } else if view.dirty || (!view.searching && view.hits.is_empty()) {
                        view.searching = true;
                        view.dirty = false;
                        Some(Action::Run(query))
                    } else {
                        view.selected()
                            .map(|hit| Action::Open(hit.session_id.clone()))
                    }
                }
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    view.query.push(c);
                    view.cursor = 0;
                    view.dirty = true;
                    None
                }
                _ => None,
            }
        };

        match action {
            Some(Action::Close) => self.search_view = None,
            Some(Action::Empty) => self.status = "type a query first".into(),
            Some(Action::Run(query)) => {
                self.status = format!("searching for {query:?}…");
                let _ = tx.send(Command::Search {
                    query,
                    session_id: None,
                    limit: None,
                });
            }
            Some(Action::Open(session_id)) => self.open_search_hit(&session_id, tx),
            None => {}
        }
    }

    /// Focus the hit's pane if it is already open, otherwise attach it in a
    /// new pane. Search results may mention sessions the client has not
    /// refreshed yet, so this keys off the id rather than the list.
    fn open_search_hit(&mut self, session_id: &str, tx: &UnboundedSender<Command>) {
        self.search_view = None;
        if let Some(open) = self.open_session(session_id) {
            self.focus_pane(open);
            return;
        }
        self.pending_attach = Some(PendingAttach {
            session_id: session_id.to_string(),
            slot: AttachSlot::New,
        });
        self.error = None;
        self.status = "opening search hit…".into();
        let _ = tx.send(Command::Attach(session_id.to_string()));
    }

    fn handle_picker_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        let Some(kind) = self.picker.as_ref().map(|picker| picker.kind) else {
            return;
        };
        match key.code {
            KeyCode::Esc => self.picker = None,
            KeyCode::Up => self.move_picker(-1),
            KeyCode::Down => self.move_picker(1),
            KeyCode::Enter => self.picker_enter(kind, tx),
            KeyCode::Char('#') if matches!(kind, PickerKind::Sessions | PickerKind::NewPane) => {
                self.picker = None;
                self.search_view = Some(SearchView::new());
                self.status = "search history · Enter runs · ↑/↓ select · Esc closes".into();
            }
            KeyCode::Backspace => {
                if let Some(picker) = &mut self.picker {
                    picker.query.pop();
                    picker.selected = 0;
                }
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && !key.modifiers.contains(KeyModifiers::ALT) =>
            {
                if let Some(picker) = &mut self.picker {
                    picker.query.push(c);
                    picker.selected = 0;
                }
            }
            _ => {}
        }
    }

    fn move_picker(&mut self, delta: i32) {
        let len = self.picker_filtered().len();
        if let Some(picker) = &mut self.picker {
            if len == 0 {
                picker.selected = 0;
            } else {
                let current = picker.selected.min(len - 1) as i32;
                picker.selected = (current + delta).clamp(0, len as i32 - 1) as usize;
            }
        }
    }

    /// Rows for the open picker: session indices or item indices.
    fn picker_filtered(&self) -> Vec<usize> {
        match self.picker.as_ref().map(|picker| picker.kind) {
            Some(PickerKind::Sessions | PickerKind::NewPane) => self.filtered_sessions(),
            _ => self.filtered_items(),
        }
    }

    fn filtered_items(&self) -> Vec<usize> {
        let Some(picker) = &self.picker else {
            return Vec::new();
        };
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

    fn picker_enter(&mut self, kind: PickerKind, tx: &UnboundedSender<Command>) {
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
            PickerKind::Files => {
                if let Some(value) = value {
                    self.picker = None;
                    self.insert_prompt(&format!("@{value}"));
                }
            }
            PickerKind::Worktrees | PickerKind::Branches => {
                if let Some(value) = value {
                    self.picker = None;
                    self.insert_prompt(&value);
                }
            }
            PickerKind::Context => {
                self.picker = None;
                self.open_ledger(tx);
            }
            PickerKind::Commands => {
                if let Some(value) = value {
                    self.picker = None;
                    self.run_picker_command(&value, tx);
                }
            }
        }
    }

    /// Put `text` at the prompt cursor, using the embedded prompt when present.
    fn insert_prompt(&mut self, text: &str) {
        if let Some(nvim) = &self.nvim {
            nvim.insert_text(text);
        } else {
            self.input.insert_str(text);
        }
        self.focus = Focus::Input;
        self.status = format!("inserted {text}");
    }

    fn open_picker(&mut self, kind: PickerKind, tx: &UnboundedSender<Command>) {
        // A picker opening clears any half-finished double-Esc gesture.
        self.esc_at = None;
        let mut picker = Picker {
            kind,
            query: String::new(),
            selected: 0,
            items: Vec::new(),
            loading: false,
        };
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
            });
        } else if kind == PickerKind::Commands {
            picker.items = command_items();
        } else {
            picker.selected = self.selected.min(self.sessions.len().saturating_sub(1));
        }
        self.picker = Some(picker);
    }

    /// Run a command-palette entry. Unknown ids are ignored.
    fn run_picker_command(&mut self, id: &str, tx: &UnboundedSender<Command>) {
        match id {
            "sessions" => self.open_picker(PickerKind::Sessions, tx),
            "new-pane" => self.open_picker(PickerKind::NewPane, tx),
            "files" => self.open_picker(PickerKind::Files, tx),
            "worktrees" => self.open_picker(PickerKind::Worktrees, tx),
            "branches" => self.open_picker(PickerKind::Branches, tx),
            "context" => self.open_picker(PickerKind::Context, tx),
            "history" => {
                self.search_view = Some(SearchView::new());
                self.status = "search history · Enter runs · ↑/↓ select · Esc closes".into();
            }
            "processes" => self.open_processes(tx),
            "approvals" => self.open_inbox(),
            "ledger" => self.open_ledger(tx),
            "new" => self.create_session(tx, false),
            "new-worktree" => self.create_session(tx, true),
            "fork" => self.fork_focused(tx, None),
            "cancel" => self.cancel(tx),
            "help" => self.help = true,
            "quit" => self.should_quit = true,
            _ => {}
        }
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

    fn create_session(&mut self, tx: &UnboundedSender<Command>, worktree: bool) {
        if self.creating {
            return;
        }
        self.creating = true;
        self.error = None;
        self.status = if worktree {
            "creating session in a new worktree…".into()
        } else {
            "creating session…".into()
        };
        let _ = tx.send(Command::Create { worktree });
    }

    fn submit(&mut self, tx: &UnboundedSender<Command>) {
        let text = self.input.text();
        if text.trim().is_empty() {
            return;
        }
        if let Err(message) = self.begin_submit(&text, tx) {
            self.status = message;
        }
    }

    /// Ask the daemon to run `text` in the focused session. `Ok` means the
    /// request was sent, so a caller may clear its own draft.
    fn begin_submit(&mut self, text: &str, tx: &UnboundedSender<Command>) -> Result<(), String> {
        if self.submitting {
            return Err("submit already in flight".into());
        }
        if text.trim().is_empty() {
            return Err("nothing to send".into());
        }
        let Some(view) = self.focused_view() else {
            return Err("no session yet".into());
        };
        if view.session.status.is_busy() {
            self.error = Some("session is running; press x to cancel".into());
            return Err("session is running".into());
        }
        let session_id = view.session.id.clone();
        self.submitting = true;
        self.pending_prompt = Some(text.to_string());
        self.error = None;
        self.status = "submitting…".into();
        let _ = tx.send(Command::Submit {
            session_id,
            text: text.to_string(),
        });
        Ok(())
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

    /// Recompute the cached card for a session from its replayable log. The
    /// newest unresolved approval or staged patch is shown; resolving it
    /// reveals the next older one.
    fn reload_pending(&mut self, session_id: &str) {
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
            self.close_diff_view_for(session_id);
            return;
        };
        // Keep an in-progress hunk selection when the same card is reloaded.
        if let Some(selected) = self
            .approvals
            .get(session_id)
            .filter(|pending| pending.approval_id == next.approval_id)
            .map(|pending| pending.selected.clone())
        {
            next.selected = selected;
        }
        self.approvals.insert(session_id.to_string(), next);
    }

    fn respond_approval(&mut self, decision: ApprovalDecision, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_view().map(|view| view.session.id.clone()) else {
            return;
        };
        self.respond_approval_for(&session_id, decision, None, None, tx);
    }

    fn respond_approval_for(
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

    fn edit_proposed(&mut self, tx: &UnboundedSender<Command>) {
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

    fn open_approval_file(&mut self) {
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

    fn start_reason_prompt(&mut self) {
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
    fn edit_patch(
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

    fn open_diff_view(&mut self) {
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
        });
        self.status = if has_hunks {
            "diff review: j/k scroll · [ ] hunk · Space toggle · y apply · n deny".into()
        } else {
            "diff review: j/k scroll · y approve · n deny · Esc close".into()
        };
    }

    fn handle_diff_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        let Some((session_id, scroll, cursor)) = self
            .diff_view
            .as_ref()
            .map(|view| (view.session_id.clone(), view.scroll, view.cursor))
        else {
            return;
        };
        let Some((total, offsets)) = self
            .approvals
            .get(&session_id)
            .map(|approval| (review_total_lines(approval), review_hunk_offsets(approval)))
        else {
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
            KeyCode::Char('g') => self.set_diff_scroll(0, total),
            KeyCode::Char('G') => self.set_diff_scroll(usize::MAX, total),
            _ => {}
        }
    }

    fn set_diff_scroll(&mut self, scroll: usize, total: usize) {
        if let Some(view) = &mut self.diff_view {
            view.scroll = scroll.min(total.saturating_sub(1));
        }
    }

    fn set_diff_cursor(&mut self, cursor: usize, line: usize, total: usize) {
        if let Some(view) = &mut self.diff_view {
            view.cursor = cursor;
            view.scroll = line.min(total.saturating_sub(1));
        }
    }

    /// Close the review overlay if it is showing `session_id`.
    fn close_diff_view_for(&mut self, session_id: &str) {
        if self
            .diff_view
            .as_ref()
            .is_some_and(|view| view.session_id == session_id)
        {
            self.diff_view = None;
        }
    }

    fn focused_view(&self) -> Option<&SessionView> {
        self.views.get(self.focused)
    }

    fn focused_view_mut(&mut self) -> Option<&mut SessionView> {
        let index = self.focused;
        self.views.get_mut(index)
    }

    /// A pane needs attention when its session is blocked and the pane is not
    /// focused. This is derived from the session status and any pending
    /// approval/patch instead of a one-shot flag, so switching away from a
    /// pending card still marks it.
    fn pane_needs_attention(&self, index: usize) -> bool {
        index != self.focused
            && self.views.get(index).is_some_and(|view| {
                view.session.status.needs_attention()
                    || self.approvals.contains_key(&view.session.id)
            })
    }

    fn attention_count(&self) -> usize {
        (0..self.views.len())
            .filter(|index| self.pane_needs_attention(*index))
            .count()
    }

    fn focus_next_attention(&mut self) {
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
        if matches.len() == 1 {
            Some(matches[0])
        } else {
            None
        }
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

        let input_height = if let Some(nvim) = &self.nvim {
            // One header row plus the prompt grid: `clamp(lines, 3, 10)` while
            // focused, a single line as a preview when the transcript has focus.
            let rows = if self.focus == Focus::Input {
                nvim.line_count().clamp(3, 10)
            } else {
                1
            };
            (rows + 1).min(area.height.saturating_sub(5).max(2))
        } else {
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
            (input_lines as u16 + 2).clamp(3, 10)
        };
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
        if self.diff_view.is_some() {
            self.draw_diff_view(frame, area);
        }
        if self.ledger.is_some() {
            self.draw_ledger(frame, area);
        }
        if self.process_view.is_some() {
            self.draw_processes(frame, area);
        }
        if self.inbox.is_some() {
            self.draw_inbox(frame, area);
        }
        if self.search_view.is_some() {
            self.draw_search(frame, area);
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
        let attention = self.attention_count();
        if attention > 0 {
            spans.push(Span::styled(
                format!("  ⚠ {attention} attention"),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
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
                let open = self.open_session(&session.id);
                let attention = open.is_some_and(|index| self.pane_needs_attention(index));
                let name_style = if attention {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let pane = match open {
                    Some(index) => {
                        Span::styled(format!("{}", index + 1), Style::default().fg(Color::Cyan))
                    }
                    None => Span::styled("·", Style::default().fg(Color::DarkGray)),
                };
                ListItem::new(Line::from(vec![
                    pane,
                    Span::raw(" "),
                    Span::styled(
                        format!("{}{} ", origin_marker(session), session.status.symbol()),
                        Style::default().fg(status_color(session.status)),
                    ),
                    Span::styled(truncate(&session.name, 12), name_style),
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
        let session_id = self.views[index].session.id.clone();
        let approval = if focused {
            self.approvals.get(&session_id).cloned()
        } else {
            None
        };
        let attention = !focused
            && (self.views[index].session.status.needs_attention()
                || self.approvals.contains_key(&session_id));
        let theme = self.theme;
        let view = &mut self.views[index];
        let inner_width = area.width.saturating_sub(2) as usize;
        let inner_height = area.height.saturating_sub(2) as usize;
        let lines = Self::transcript_lines(view, inner_width.max(1), theme);
        let max_scroll = lines.len().saturating_sub(inner_height);
        if view.follow {
            view.scroll = max_scroll;
        } else {
            view.scroll = view.scroll.min(max_scroll);
        }
        let name = truncate(&view.session.name, inner_width.saturating_sub(8).max(1));
        let marker = if attention { "!" } else { "" };
        let permission = match view.session.permission {
            PermissionLevel::ReadOnly => " [ro]",
            PermissionLevel::Full => " [full]",
            PermissionLevel::WorkspaceWrite => "",
        };
        let title = format!(
            " {}{marker} {} {}{permission} ",
            index + 1,
            view.session.status.symbol(),
            name
        );
        let border_style = if focused {
            Style::default().fg(Color::Cyan)
        } else if attention {
            Style::default().fg(status_color(view.session.status))
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
            draw_approval_card(frame, area, &approval, theme);
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

    fn transcript_lines(view: &SessionView, width: usize, theme: Theme) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for event in &view.events {
            match event.kind {
                EventKind::UserMessage => {
                    if let Ok(payload) =
                        serde_json::from_value::<UserMessagePayload>(event.payload.clone())
                    {
                        push_message(
                            &mut lines,
                            "you",
                            theme.string.unwrap_or(Color::Cyan),
                            &payload.text,
                            "",
                            width,
                        );
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
                            theme.function.unwrap_or(Color::Green),
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
                        push_message(
                            &mut lines,
                            "tool",
                            theme.function.unwrap_or(Color::Yellow),
                            &text,
                            "",
                            width,
                        );
                    }
                }
                EventKind::ApprovalRequired => {
                    if let Ok(payload) =
                        serde_json::from_value::<ApprovalRequiredPayload>(event.payload.clone())
                    {
                        let text = format!("{} — y to approve, n to deny", payload.summary);
                        push_message(
                            &mut lines,
                            "need",
                            theme.function.unwrap_or(Color::Yellow),
                            &text,
                            "",
                            width,
                        );
                    }
                }
                EventKind::PatchProposed => {
                    if let Ok(payload) =
                        serde_json::from_value::<PatchProposedPayload>(event.payload.clone())
                    {
                        let text = format!("{} — y to apply, n to reject", payload.summary);
                        push_message(
                            &mut lines,
                            "patch",
                            theme.function.unwrap_or(Color::Magenta),
                            &text,
                            "",
                            width,
                        );
                    }
                }
                EventKind::PatchApplied => {
                    if let Ok(payload) =
                        serde_json::from_value::<PatchResultPayload>(event.payload.clone())
                    {
                        push_message(
                            &mut lines,
                            "patch",
                            theme.diff_add.unwrap_or(Color::Green),
                            &format!("applied {}", payload.path),
                            "",
                            width,
                        );
                    }
                }
                EventKind::PatchRejected => {
                    if let Ok(payload) =
                        serde_json::from_value::<PatchResultPayload>(event.payload.clone())
                    {
                        push_message(
                            &mut lines,
                            "patch",
                            theme.diff_delete.unwrap_or(Color::Red),
                            &format!("rejected {}", payload.path),
                            "",
                            width,
                        );
                    }
                }
                EventKind::PatchConflict => {
                    if let Ok(payload) =
                        serde_json::from_value::<PatchResultPayload>(event.payload.clone())
                    {
                        push_message(
                            &mut lines,
                            "patch",
                            theme.diff_delete.unwrap_or(Color::Red),
                            &format!("conflict on {}", payload.path),
                            "",
                            width,
                        );
                    }
                }
                EventKind::ToolResult => {
                    if let Ok(payload) =
                        serde_json::from_value::<ToolResultPayload>(event.payload.clone())
                    {
                        let (label, color) = if payload.denied {
                            ("denied", theme.error.unwrap_or(Color::Red))
                        } else if payload.ok {
                            ("ok", theme.diff_add.unwrap_or(Color::Green))
                        } else {
                            ("failed", theme.error.unwrap_or(Color::Red))
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
                                        Style::default()
                                            .fg(theme.comment.unwrap_or(Color::DarkGray)),
                                    )));
                                }
                            }
                            if output.lines().count() > max_lines {
                                lines.push(Line::from(Span::styled(
                                    "  …",
                                    Style::default().fg(theme.comment.unwrap_or(Color::DarkGray)),
                                )));
                            }
                            lines.push(Line::from(""));
                        }
                    }
                }
                EventKind::Permission => {
                    if let Ok(payload) =
                        serde_json::from_value::<PermissionPayload>(event.payload.clone())
                    {
                        let text = format!(
                            "permission {} → {}{}",
                            payload.from.as_str(),
                            payload.to.as_str(),
                            if payload.temporary {
                                " (temporary)"
                            } else {
                                ""
                            }
                        );
                        push_message(
                            &mut lines,
                            "perm",
                            theme.function.unwrap_or(Color::Yellow),
                            &text,
                            "",
                            width,
                        );
                    }
                }
                EventKind::Error => {
                    let message = event
                        .payload
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    push_message(
                        &mut lines,
                        "error",
                        theme.error.unwrap_or(Color::Red),
                        message,
                        "",
                        width,
                    );
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
            push_message(
                &mut lines,
                "eix",
                theme.function.unwrap_or(Color::Green),
                text,
                "",
                width,
            );
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "no messages yet — type a prompt below",
                Style::default().fg(theme.comment.unwrap_or(Color::DarkGray)),
            )));
        }
        lines
    }

    fn draw_input(&mut self, frame: &mut Frame, area: Rect) {
        if self.nvim.is_some() {
            self.draw_nvim_input(frame, area);
            return;
        }
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

    fn draw_nvim_input(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Input;
        let mode = self
            .nvim
            .as_ref()
            .map(NvimInput::mode)
            .filter(|mode| !mode.is_empty())
            .unwrap_or_else(|| "--".to_string());
        let accent = if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let title = Line::from(vec![
            Span::styled(
                " prompt ",
                Style::default().fg(Color::Black).bg(if focused {
                    Color::Cyan
                } else {
                    Color::DarkGray
                }),
            ),
            Span::raw(" "),
            Span::styled(format!("[{mode}]"), accent),
        ]);
        let hint = if focused {
            " Enter send  Ctrl-J newline  Esc normal  Esc Esc transcript "
        } else {
            " i to type "
        };
        let columns = Layout::horizontal([Constraint::Min(20), Constraint::Length(44)]).split(area);
        frame.render_widget(
            Paragraph::new(title).style(accent),
            Rect {
                height: 1,
                ..columns[0]
            },
        );
        frame.render_widget(
            Paragraph::new(hint)
                .alignment(Alignment::Right)
                .style(Style::default().fg(Color::DarkGray)),
            Rect {
                height: 1,
                ..columns[1]
            },
        );

        let grid = Rect {
            y: area.y + 1,
            height: area.height.saturating_sub(1),
            ..area
        };
        if let Some(nvim) = self.nvim.as_mut() {
            nvim.resize(grid.width, grid.height);
            nvim.draw(frame, grid, focused);
        }
    }

    fn draw_footer(&mut self, frame: &mut Frame, area: Rect) {
        let connected = if self.connected { "●" } else { "○" };
        let status = match &self.reason_prompt {
            Some(prompt) => format!("deny reason: {}▏ (Enter denies · Esc cancels)", prompt.text),
            None => self.status.clone(),
        };
        let message = match &self.error {
            Some(error) => format!(" error: {error}"),
            None => format!(
                " {} eixd  {}  {} pane(s)  {} session(s)  {}",
                connected,
                self.model(),
                self.views.len(),
                self.sessions.len(),
                status
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
        let hint = if self.focused_approval().is_some_and(|approval| {
            approval
                .diff
                .as_deref()
                .is_some_and(|diff| !diff.is_empty())
        }) {
            " y approve  n deny  e edit  o open  r reason  d review  ? help "
        } else if self.focused_approval().is_some() {
            " y approve  n deny  o open  r reason  x cancel  ? help "
        } else if self.attention_count() > 0 {
            " a next attention  A approvals  ? help  q quit "
        } else {
            " c ctx  P ps  A approvals  p picker  q quit "
        };
        frame.render_widget(
            Paragraph::new(hint)
                .alignment(Alignment::Right)
                .style(style),
            chunks[1],
        );
    }

    fn draw_diff_view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = self.theme;
        let Some((session_id, scroll, cursor)) = self
            .diff_view
            .as_ref()
            .map(|view| (view.session_id.clone(), view.scroll, view.cursor))
        else {
            return;
        };
        let Some(approval) = self.approvals.get(&session_id) else {
            return;
        };
        let tool = approval.tool.clone();

        let rect = centered(
            area,
            area.width.saturating_sub(4),
            area.height.saturating_sub(4),
        );
        frame.render_widget(Clear, rect);
        let inner_width = rect.width.saturating_sub(2) as usize;
        let inner_height = rect.height.saturating_sub(3) as usize; // borders + hint

        let interactive = !approval.hunks.is_empty();
        let mut body: Vec<Line> = Vec::new();
        if interactive {
            for (index, hunk) in approval.hunks.iter().enumerate() {
                let selected = approval.selected.get(index).copied().unwrap_or(true);
                let focused = index == cursor;
                for (line_index, line) in hunk.lines.iter().enumerate() {
                    if line_index == 0 {
                        let marker = if focused { "▸ " } else { "  " };
                        let check = if selected { "[x] " } else { "[ ] " };
                        let style = if focused {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else if selected {
                            Style::default().fg(Color::Cyan)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        };
                        body.push(Line::from(Span::styled(
                            truncate(&format!("{marker}{check}{line}"), inner_width),
                            style,
                        )));
                    } else {
                        body.push(Line::from(Span::styled(
                            truncate(&format!("    {line}"), inner_width),
                            diff_line_style(line, theme),
                        )));
                    }
                }
            }
        } else {
            for line in approval.diff.as_deref().unwrap_or("").lines() {
                body.push(Line::from(Span::styled(
                    truncate(line, inner_width),
                    diff_line_style(line, theme),
                )));
            }
        }

        let total = body.len();
        let max_scroll = total.saturating_sub(inner_height);
        let scroll = scroll.min(max_scroll);
        let mut body: Vec<Line> = body.into_iter().skip(scroll).take(inner_height).collect();
        let hint = if interactive {
            " j/k scroll  [ ] hunk  Space toggle  a all  y apply  n deny  Esc close "
        } else {
            " j/k scroll  y approve  n deny  Esc close "
        };
        body.push(Line::from(Span::styled(
            hint,
            Style::default().fg(Color::DarkGray),
        )));

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(format!(" diff · {tool} "));
        frame.render_widget(
            Paragraph::new(body)
                .block(block)
                .style(Style::default().bg(Color::Black)),
            rect,
        );
        if let Some(view) = &mut self.diff_view {
            view.scroll = scroll;
        }
    }

    fn draw_ledger(&mut self, frame: &mut Frame, area: Rect) {
        let Some(ledger) = &mut self.ledger else {
            return;
        };
        let rect = centered(
            area,
            area.width.saturating_sub(4),
            area.height.saturating_sub(4),
        );
        frame.render_widget(Clear, rect);
        let inner_width = rect.width.saturating_sub(2) as usize;
        let inner_height = rect.height.saturating_sub(2) as usize;
        // Two header rows (totals + per-sector budgets) and one hint row.
        let list_height = inner_height.saturating_sub(3).max(1);

        let mut lines: Vec<Line> = Vec::new();
        let mut starts = Vec::with_capacity(ledger.result.entries.len());
        for (index, entry) in ledger.result.entries.iter().enumerate() {
            starts.push(lines.len());
            lines.push(ledger_entry_line(
                entry,
                index == ledger.cursor,
                inner_width,
            ));
            if ledger.expanded.contains(&entry.id) {
                for detail in entry.detail.lines().take(8) {
                    lines.push(Line::from(Span::styled(
                        truncate(&format!("      {detail}"), inner_width),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
        }

        let cursor_line = starts.get(ledger.cursor).copied().unwrap_or(0);
        let max_scroll = lines.len().saturating_sub(list_height);
        if cursor_line < ledger.scroll {
            ledger.scroll = cursor_line;
        } else if cursor_line >= ledger.scroll + list_height {
            ledger.scroll = cursor_line + 1 - list_height;
        }
        ledger.scroll = ledger.scroll.min(max_scroll);

        let mut body: Vec<Line> = Vec::with_capacity(list_height + 3);
        body.push(ledger_totals_line(&ledger.result));
        body.push(ledger_sectors_line(&ledger.result, inner_width));
        body.extend(lines.into_iter().skip(ledger.scroll).take(list_height));
        body.push(ledger_hint_line(ledger));

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(" context ledger ");
        frame.render_widget(
            Paragraph::new(body)
                .block(block)
                .style(Style::default().bg(Color::Black)),
            rect,
        );
    }

    fn draw_inbox(&mut self, frame: &mut Frame, area: Rect) {
        let Some(inbox) = &self.inbox else {
            return;
        };
        let rows = (inbox.items.len() as u16 + 4).min(area.height.saturating_sub(2).max(5));
        let rect = centered(area, area.width.saturating_sub(6).max(30), rows);
        frame.render_widget(Clear, rect);
        let inner = rect.width.saturating_sub(2) as usize;

        let mut lines: Vec<Line> = Vec::new();
        for (index, item) in inbox.items.iter().enumerate() {
            let marker = if index == inbox.cursor { "▸ " } else { "  " };
            let kind = match item.kind {
                PendingKind::Tool => "tool",
                PendingKind::Patch => "patch",
            };
            let label = format!(
                "{marker}{:<20} {:<5} {:<14} {}",
                truncate(&item.session_name, 20),
                kind,
                item.tool,
                item.summary
            );
            let style = if index == inbox.cursor {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(truncate(&label, inner), style)));
        }
        lines.push(Line::from(Span::styled(
            truncate(
                " Enter open · y approve · n deny · j/k select · Esc close ",
                inner,
            ),
            Style::default().fg(Color::DarkGray),
        )));

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(format!(" approvals · {} ", inbox.items.len()));
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .style(Style::default().bg(Color::Black)),
            rect,
        );
    }

    fn draw_processes(&mut self, frame: &mut Frame, area: Rect) {
        let Some(view) = &mut self.process_view else {
            return;
        };
        let rect = centered(
            area,
            area.width.saturating_sub(4),
            area.height.saturating_sub(4),
        );
        frame.render_widget(Clear, rect);
        let inner_height = rect.height.saturating_sub(2) as usize;
        let inner_width = rect.width.saturating_sub(2) as usize;
        // List on top (bounded), live output below.
        let list_height = (view.processes.len() + 1)
            .clamp(2, 10)
            .min(inner_height.saturating_sub(3).max(3));

        let mut list_lines: Vec<Line> = Vec::new();
        for (index, process) in view.processes.iter().enumerate() {
            list_lines.push(process_line(process, index == view.cursor, inner_width));
        }
        if view.processes.is_empty() {
            list_lines.push(Line::from(Span::styled(
                "  no background processes",
                Style::default().fg(Color::DarkGray),
            )));
        }
        let max_scroll = view.processes.len().saturating_sub(list_height);
        let list_scroll = view
            .cursor
            .saturating_sub(list_height.saturating_sub(1))
            .min(max_scroll);

        let title = match view.selected() {
            Some(process) => format!(
                " processes · {} · {} ",
                short_id(&process.id),
                process.status.as_str()
            ),
            None => format!(" processes · {} ", short_id(&view.session_id)),
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title)
            .title_bottom(" j/k select · x kill · r refresh · PgUp/PgDn · Esc close ");
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        let chunks = Layout::vertical([Constraint::Length(list_height as u16), Constraint::Min(1)])
            .split(inner);
        frame.render_widget(
            Paragraph::new(
                list_lines
                    .into_iter()
                    .skip(list_scroll)
                    .take(list_height)
                    .collect::<Vec<_>>(),
            )
            .style(Style::default().bg(Color::Black)),
            chunks[0],
        );

        let output_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray))
            .title(" output ");
        let output_inner = output_block.inner(chunks[1]);
        frame.render_widget(output_block, chunks[1]);
        let width = output_inner.width as usize;
        let height = output_inner.height as usize;
        let lines: Vec<&str> = view.tail.lines().collect();
        let start = lines.len().saturating_sub(height + view.scroll);
        let body: Vec<Line> = if lines.is_empty() {
            vec![Line::from(Span::styled(
                "(no output yet)",
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            lines
                .iter()
                .skip(start)
                .take(height)
                .map(|line| Line::from(truncate(line, width)))
                .collect()
        };
        frame.render_widget(
            Paragraph::new(body).style(Style::default().bg(Color::Black)),
            output_inner,
        );
    }

    fn draw_search(&mut self, frame: &mut Frame, area: Rect) {
        let Some(view) = &self.search_view else {
            return;
        };
        let rect = centered(
            area,
            area.width.saturating_sub(4),
            area.height.saturating_sub(4),
        );
        frame.render_widget(Clear, rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(format!(" search history · {} hit(s) ", view.hits.len()))
            .title_bottom(" Enter run/open · ↑/↓ select · Esc close ");
        let inner = block.inner(rect);
        frame.render_widget(block, rect);

        let chunks = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).split(inner);
        let prompt = Line::from(vec![
            Span::styled("> ", Style::default().fg(Color::Cyan)),
            Span::raw(view.query.as_str()),
            Span::styled("▏", Style::default().fg(Color::Cyan)),
        ]);
        let hint = if view.searching {
            "searching…"
        } else if view.hits.is_empty() {
            "type a query and press Enter"
        } else {
            "Enter opens the selected session"
        };
        frame.render_widget(
            Paragraph::new(vec![
                prompt,
                Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))),
            ]),
            chunks[0],
        );

        let width = chunks[1].width as usize;
        let height = chunks[1].height as usize;
        let mut lines: Vec<Line> = Vec::new();
        if view.hits.is_empty() {
            lines.push(Line::from(Span::styled(
                if view.searching {
                    "searching…"
                } else {
                    "no matches"
                },
                Style::default().fg(Color::DarkGray),
            )));
        }
        for (index, hit) in view.hits.iter().enumerate() {
            let selected = index == view.cursor;
            let style = if selected {
                Style::default()
                    .bg(Color::DarkGray)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let head = format!(
                "{} ({}) e{} {}",
                hit.session_name,
                short_id(&hit.session_id),
                hit.seq,
                hit.role
            );
            lines.push(Line::from(Span::styled(truncate(&head, width), style)));
            lines.push(Line::from(Span::styled(
                format!("  {}", truncate(&hit.snippet, width.saturating_sub(2))),
                style.fg(Color::Gray),
            )));
        }
        // Each hit takes two rows; keep the cursor on screen.
        let visible = (height / 2).max(1);
        let start = view.cursor.saturating_sub(visible - 1) * 2;
        frame.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .skip(start)
                    .take(height)
                    .collect::<Vec<_>>(),
            )
            .style(Style::default().bg(Color::Black)),
            chunks[1],
        );
    }

    fn draw_picker(&mut self, frame: &mut Frame, area: Rect) {
        let filtered = self.picker_filtered();
        let Some(kind) = self.picker.as_ref().map(|picker| picker.kind) else {
            return;
        };
        // Keep the cursor on a real row (the list may have shrunk under it).
        let selected = {
            let Some(picker) = &mut self.picker else {
                return;
            };
            if filtered.is_empty() {
                picker.selected = 0;
            } else {
                picker.selected = picker.selected.min(filtered.len() - 1);
            }
            picker.selected
        };
        let query = self
            .picker
            .as_ref()
            .map(|picker| picker.query.clone())
            .unwrap_or_default();
        let loading = self.picker.as_ref().is_some_and(|picker| picker.loading);

        let width = (area.width as u32 * 8 / 10).max(44).min(area.width as u32) as u16;
        let height = (area.height as u32 * 7 / 10)
            .max(12)
            .min(area.height as u32) as u16;
        let rect = centered(area, width, height);
        frame.render_widget(Clear, rect);
        let title = format!(" {} ({}) ", kind.title(), filtered.len());
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title)
            .title_bottom(format!(" {} ", kind.hint()));
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
                Span::raw(query.clone()),
            ])),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new("─".repeat(chunks[1].width as usize))
                .style(Style::default().fg(Color::DarkGray)),
            chunks[1],
        );

        // A preview column is only worth it on a reasonably wide terminal.
        let body = chunks[2];
        let (list_area, preview_area) = if body.width >= 64 {
            let columns =
                Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
                    .split(body);
            (columns[0], Some(columns[1]))
        } else {
            (body, None)
        };

        if filtered.is_empty() {
            let text = if loading { "loading…" } else { "no matches" };
            frame.render_widget(
                Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
                list_area,
            );
        } else {
            let rows = self.picker_rows(kind, &filtered);
            let list = List::new(rows)
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("› ");
            let mut state = ListState::default().with_selected(Some(selected));
            frame.render_stateful_widget(list, list_area, &mut state);
        }

        if let Some(preview_area) = preview_area {
            let lines = self.picker_preview_lines(&filtered);
            let width = preview_area.width.saturating_sub(2) as usize;
            let body: Vec<Line> = lines
                .into_iter()
                .take(preview_area.height as usize)
                .map(|line| Line::from(truncate(&line, width)))
                .collect();
            let preview = Paragraph::new(body)
                .style(Style::default().fg(Color::Gray))
                .block(
                    Block::default()
                        .borders(Borders::LEFT)
                        .border_style(Style::default().fg(Color::DarkGray)),
                )
                .wrap(ratatui::widgets::Wrap { trim: false });
            frame.render_widget(preview, preview_area);
        }
    }

    /// List rows for the open picker. Session kinds render live session state;
    /// other kinds render the provider's label and detail.
    fn picker_rows(&self, kind: PickerKind, filtered: &[usize]) -> Vec<ListItem<'_>> {
        match kind {
            PickerKind::Sessions | PickerKind::NewPane => {
                let open_panes: Vec<Option<usize>> = self
                    .sessions
                    .iter()
                    .map(|session| {
                        self.views
                            .iter()
                            .position(|view| view.session.id == session.id)
                    })
                    .collect();
                filtered
                    .iter()
                    .map(|&index| {
                        let session = &self.sessions[index];
                        let pane = match open_panes[index] {
                            Some(pane) => Span::styled(
                                format!("{} ", pane + 1),
                                Style::default().fg(Color::Cyan),
                            ),
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
                    .collect()
            }
            _ => {
                let Some(picker) = &self.picker else {
                    return Vec::new();
                };
                filtered
                    .iter()
                    .filter_map(|&index| picker.items.get(index))
                    .map(|item| {
                        let mut spans = vec![Span::styled(
                            item.label.clone(),
                            Style::default().add_modifier(Modifier::BOLD),
                        )];
                        if let Some(detail) = &item.detail {
                            if !detail.is_empty() {
                                spans.push(Span::styled(
                                    format!("  {detail}"),
                                    Style::default().fg(Color::DarkGray),
                                ));
                            }
                        }
                        ListItem::new(Line::from(spans))
                    })
                    .collect()
            }
        }
    }

    /// Right-pane preview lines for the selected picker row.
    fn picker_preview_lines(&self, filtered: &[usize]) -> Vec<String> {
        let Some(picker) = &self.picker else {
            return Vec::new();
        };
        let Some(&index) = filtered.get(picker.selected) else {
            return Vec::new();
        };
        match picker.kind {
            PickerKind::Sessions | PickerKind::NewPane => {
                let Some(session) = self.sessions.get(index) else {
                    return Vec::new();
                };
                let mut lines = vec![
                    session.name.clone(),
                    String::new(),
                    format!("id          {}", session.id),
                    format!("status      {}", session.status.as_str()),
                    format!("cwd         {}", session.cwd.display()),
                    format!("permission  {}", session.permission.as_str()),
                ];
                if let Some(worktree) = &session.worktree {
                    lines.push(format!("worktree    {}", worktree.display()));
                }
                lines.push(format!(
                    "model       {}",
                    session.model.as_deref().unwrap_or("(daemon default)")
                ));
                lines
            }
            _ => {
                let Some(item) = picker.items.get(index) else {
                    return Vec::new();
                };
                if let Some(preview) = &item.preview {
                    preview.lines().map(str::to_string).collect()
                } else if let Some(detail) = &item.detail {
                    vec![item.label.clone(), String::new(), detail.clone()]
                } else {
                    vec![item.label.clone()]
                }
            }
        }
    }
}

/// Rows for the `:` command palette. `value` is dispatched by
/// [`App::run_picker_command`]; the label and help are only for display.
fn command_items() -> Vec<PickerItem> {
    const COMMANDS: &[(&str, &str, &str)] = &[
        (
            "sessions",
            "switch session",
            "Pick the focused pane's session (p)",
        ),
        (
            "new-pane",
            "open session in a new pane",
            "Pick a session for a split (o)",
        ),
        (
            "files",
            "workspace files",
            "Insert @path into the prompt (@)",
        ),
        (
            "worktrees",
            "git worktrees",
            "Insert a worktree path into the prompt (w)",
        ),
        (
            "branches",
            "git branches",
            "Insert a branch name into the prompt (b)",
        ),
        (
            "context",
            "context entries",
            "Browse the context ledger with previews",
        ),
        ("ledger", "context ledger", "Open the full ledger (c)"),
        (
            "history",
            "search history",
            "Full-text search across sessions (#)",
        ),
        (
            "processes",
            "background processes",
            "List and inspect processes (P)",
        ),
        ("approvals", "approval inbox", "Pending approvals (A)"),
        ("new", "new session", "Create a session (n)"),
        (
            "new-worktree",
            "new session in a worktree",
            "Create an isolated session (N)",
        ),
        (
            "fork",
            "fork focused session",
            "Fork at the current tail (F)",
        ),
        (
            "cancel",
            "cancel the running turn",
            "Stop the focused session (x)",
        ),
        ("help", "key help", "Show the key map (?)"),
        ("quit", "quit eix", "Leave the TUI (q)"),
    ];
    COMMANDS
        .iter()
        .map(|(id, label, help)| {
            PickerItem::new(*id)
                .with_label(*label)
                .with_detail(*help)
                .with_preview(format!("{label}\n\n{help}"))
        })
        .collect()
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
    #[serde(default)]
    call_id: String,
    tool: String,
    summary: String,
    #[serde(default)]
    arguments: serde_json::Value,
    #[serde(default)]
    diff: Option<String>,
    #[serde(default)]
    hunks: Vec<HunkInfo>,
}

#[derive(Deserialize)]
struct PatchProposedParams {
    session_id: String,
    path: String,
}

#[derive(Deserialize)]
struct PatchOutcomeParams {
    session_id: String,
    path: String,
}

#[derive(Deserialize)]
struct ToolCompleted {
    session_id: String,
    #[serde(default)]
    call_id: String,
}

#[derive(Deserialize)]
struct AttentionParams {
    session_id: String,
    status: SessionStatus,
}

#[derive(Deserialize)]
struct ContextUpdatedParams {
    session_id: String,
    ledger: ContextLedgerResult,
}

#[derive(Deserialize)]
struct ProcessChunkParams {
    session_id: String,
    process_id: String,
    #[serde(default)]
    chunk: String,
}

#[derive(Deserialize)]
struct ProcessExitedParams {
    session_id: String,
    process_id: String,
    status: ProcessStatus,
    #[serde(default)]
    exit_code: Option<i32>,
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
        key_line("Esc", "leave the prompt (nvim: first Esc is normal mode)"),
        key_line("Esc Esc", "from the nvim prompt back to the transcript"),
        key_line("Enter", "send (in prompt)"),
        key_line("Ctrl-J", "newline (in prompt)"),
        key_line("p / Ctrl-G", "session picker"),
        key_line("o", "open session in a new pane"),
        key_line(":", "command palette (every action and provider)"),
        key_line("w / b", "insert a git worktree path / branch name"),
        key_line("@ (in prompt)", "workspace file picker"),
        key_line("a", "jump to the next pane needing attention"),
        key_line("C-w h/j/k/l", "focus pane left/down/up/right"),
        key_line("C-w v / s", "open a new pane"),
        key_line("C-w c", "close focused pane"),
        key_line("C-w w", "cycle panes"),
        key_line("Tab / S-Tab", "next / previous session"),
        key_line("n / N", "new session (N: in a fresh git worktree)"),
        key_line("x", "cancel focused turn"),
        key_line("y / n", "approve / deny a pending tool call"),
        key_line(
            "d",
            "review the pending diff full screen (Space toggles hunks)",
        ),
        key_line("e / o", "edit the staged content / open the real file"),
        key_line("r", "deny the pending approval with a reason"),
        key_line("A", "approvals inbox across sessions"),
        key_line("j / k", "scroll focused transcript"),
        key_line("g / G", "top / follow tail"),
        key_line("c", "context ledger (f fork here, w rewind + roll back)"),
        key_line("P", "background processes (x kills, r refreshes)"),
        key_line("F", "fork the session (git worktree when inside a repo)"),
        key_line("#", "search message history (Enter runs / opens)"),
        key_line("? / F1", "open this help (F1 anywhere)"),
        key_line("q / Ctrl-C", "quit"),
        Line::from(""),
        Line::from(Span::styled(
            "C-w commands work from the transcript; Esc leaves the prompt first.",
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(Span::styled(
            "the ledger is advisory: system and instruction entries are never trimmed automatically",
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

fn sector_tag(sector: Sector) -> &'static str {
    match sector {
        Sector::System => "SYS ",
        Sector::Instructions => "INS ",
        Sector::Workspace => "WS  ",
        Sector::History => "HIS ",
        Sector::Tools => "TOOL",
        Sector::Memory => "MEM ",
    }
}

fn process_line(process: &ProcessInfo, selected: bool, width: usize) -> Line<'static> {
    let marker = if selected { "▸" } else { " " };
    let (tag, color) = match process.status {
        ProcessStatus::Running => ("run ", Color::Green),
        ProcessStatus::Exited => ("exit", Color::Gray),
        ProcessStatus::Killed => ("kill", Color::Yellow),
        ProcessStatus::Failed => ("fail", Color::Red),
    };
    let code = process
        .exit_code
        .map(|code| format!(" ({code})"))
        .unwrap_or_default();
    let text = format!(
        "{marker} {tag}{code} {} {}",
        short_id(&process.id),
        process.command
    );
    let style = if selected {
        Style::default()
            .bg(Color::DarkGray)
            .fg(color)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(color)
    };
    Line::from(Span::styled(truncate(&text, width), style))
}

fn ledger_entry_line(entry: &ContextEntryInfo, selected: bool, width: usize) -> Line<'static> {
    let marker = if selected { "▸" } else { " " };
    let flag = if entry.dropped {
        if entry.auto {
            "cut"
        } else {
            "off"
        }
    } else if entry.compacted {
        "sum"
    } else if entry.pinned {
        "pin"
    } else {
        "   "
    };
    let text = format!(
        "{marker} {flag} {} {:>6}t  {}",
        sector_tag(entry.sector),
        entry.tokens,
        entry.preview
    );
    let style = if selected {
        Style::default()
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else if entry.dropped || entry.compacted {
        Style::default().fg(Color::DarkGray)
    } else if entry.pinned {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default()
    };
    Line::from(Span::styled(truncate(&text, width), style))
}

fn ledger_totals_line(result: &ContextLedgerResult) -> Line<'static> {
    let remaining = result.budget_tokens.saturating_sub(result.total_tokens);
    let mut spans = vec![
        Span::styled(
            format!(" {} / {} tokens", result.total_tokens, result.budget_tokens),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  ({remaining} left)"),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if result.trimmed > 0 {
        spans.push(Span::styled(
            format!("  · {} auto-trimmed", result.trimmed),
            Style::default().fg(Color::Yellow),
        ));
    }
    Line::from(spans)
}

fn ledger_sectors_line(result: &ContextLedgerResult, width: usize) -> Line<'static> {
    let parts: Vec<String> = result
        .sectors
        .iter()
        .filter(|sector| sector.entries > 0)
        .map(|sector| {
            format!(
                "{} {}/{}t",
                sector_tag(sector.sector).trim(),
                sector.tokens,
                sector.budget_tokens
            )
        })
        .collect();
    let text = if parts.is_empty() {
        " (nothing in context yet)".to_string()
    } else {
        format!(" {}", parts.join("  ·  "))
    };
    Line::from(Span::styled(
        truncate(&text, width),
        Style::default().fg(Color::DarkGray),
    ))
}

fn ledger_hint_line(ledger: &LedgerView) -> Line<'static> {
    let text = match ledger.mark {
        Some(mark) => format!(" mark e{mark} · s compact range · m clear · Esc close "),
        None => {
            " j/k move · o expand · p pin · d drop · m mark · s compact · Esc close ".to_string()
        }
    };
    Line::from(Span::styled(text, Style::default().fg(Color::DarkGray)))
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

/// Line index where each hunk starts in the review body.
fn review_hunk_offsets(approval: &PendingApproval) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(approval.hunks.len());
    let mut line = 0;
    for hunk in &approval.hunks {
        offsets.push(line);
        line += hunk.lines.len().max(1);
    }
    offsets
}

/// Number of lines the review body renders.
fn review_total_lines(approval: &PendingApproval) -> usize {
    if approval.hunks.is_empty() {
        approval
            .diff
            .as_deref()
            .map(|diff| diff.lines().count())
            .unwrap_or(0)
    } else {
        approval
            .hunks
            .iter()
            .map(|hunk| hunk.lines.len().max(1))
            .sum()
    }
}

/// Hand the terminal to `$VISUAL`/`$EDITOR` (default `nvim`) and return to the
/// alternate screen. The caller sets `force_clear` so ratatui repaints.
fn run_editor(path: &std::path::Path) -> anyhow::Result<()> {
    use crossterm::cursor::{Hide, Show};
    use crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
    };
    let editor = std::env::var("VISUAL")
        .or_else(|_| std::env::var("EDITOR"))
        .unwrap_or_else(|_| "nvim".to_string());
    let mut parts = editor.split_whitespace();
    let program = parts.next().unwrap_or("nvim");
    let mut stdout = std::io::stdout();
    disable_raw_mode().ok();
    crossterm::execute!(stdout, LeaveAlternateScreen, Show).ok();
    let status = std::process::Command::new(program)
        .args(parts)
        .arg(path)
        .status();
    enable_raw_mode().ok();
    crossterm::execute!(stdout, EnterAlternateScreen, Hide).ok();
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(anyhow::anyhow!("editor exited with {status}")),
        Err(err) => Err(anyhow::anyhow!("cannot launch {program}: {err}")),
    }
}

/// Risk label for an approval card (DESIGN 9.3): shell commands are called
/// out separately from workspace writes and read-only tools.
fn approval_risk(tool: &str) -> (&'static str, Color) {
    match tool {
        "run_command" => ("shell", Color::Red),
        "write_file" | "edit_file" | "apply_patch" => ("workspace-write", Color::Yellow),
        "spawn_agent" => ("agent", Color::Magenta),
        _ => ("read", Color::Green),
    }
}

fn draw_approval_card(frame: &mut Frame, area: Rect, approval: &PendingApproval, theme: Theme) {
    if area.width < 20 || area.height < 6 {
        return;
    }
    // The card grows to fit a capped diff preview, never taller than the pane.
    let max_diff = area.height.saturating_sub(5) as usize; // borders + summary + keys
    let diff_lines: Vec<&str> = approval.diff.as_deref().unwrap_or("").lines().collect();
    let max_diff = max_diff.min(10);
    let shown = diff_lines.len().min(max_diff);
    let height = (4 + shown + usize::from(diff_lines.len() > shown)) as u16;
    let rect = Rect {
        x: area.x + 1,
        y: area.y + area.height - height - 1,
        width: area.width.saturating_sub(2),
        height,
    };
    frame.render_widget(Clear, rect);

    let summary = truncate(&approval.summary, rect.width.saturating_sub(4) as usize);
    let (risk, risk_color) = approval_risk(&approval.tool);
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "⚠ ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(summary, Style::default().fg(Color::Yellow)),
        Span::styled(format!("  [{risk}]"), Style::default().fg(risk_color)),
    ])];
    let inner = rect.width.saturating_sub(2) as usize;
    for line in diff_lines.iter().take(shown) {
        lines.push(Line::from(Span::styled(
            truncate(line, inner),
            diff_line_style(line, theme),
        )));
    }
    if diff_lines.len() > shown {
        lines.push(Line::from(Span::styled(
            "… diff truncated",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let mut keys = vec![
        Span::styled("[y] approve  ", Style::default().fg(Color::Green)),
        Span::styled("[n] deny  ", Style::default().fg(Color::Red)),
    ];
    if approval.path.is_some() {
        keys.push(Span::styled("[o] open  ", Style::default().fg(Color::Cyan)));
    }
    if approval.kind == PendingKind::Patch {
        keys.push(Span::styled("[e] edit  ", Style::default().fg(Color::Cyan)));
    }
    keys.push(Span::styled(
        "[r] reason  ",
        Style::default().fg(Color::DarkGray),
    ));
    if !diff_lines.is_empty() {
        keys.push(Span::styled(
            if approval.hunks.is_empty() {
                "[d] review  "
            } else {
                "[d] review hunks  "
            },
            Style::default().fg(Color::Cyan),
        ));
    }
    keys.push(Span::styled(
        "[x] cancel",
        Style::default().fg(Color::DarkGray),
    ));
    lines.push(Line::from(keys));

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

fn diff_line_style(line: &str, theme: Theme) -> Style {
    if line.starts_with("@@") {
        Style::default().fg(theme.function.unwrap_or(Color::Cyan))
    } else if line.starts_with("+++") || line.starts_with("---") {
        Style::default().fg(theme.comment.unwrap_or(Color::DarkGray))
    } else if line.starts_with('+') {
        Style::default().fg(theme.diff_add.unwrap_or(Color::Green))
    } else if line.starts_with('-') {
        Style::default().fg(theme.diff_delete.unwrap_or(Color::Red))
    } else {
        Style::default().fg(theme.normal.unwrap_or(Color::Gray))
    }
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

/// Sidebar prefix: `↳` for sub-agents and `⑂` for sessions in a worktree.
fn origin_marker(session: &Session) -> &'static str {
    if session.is_agent() {
        "↳"
    } else if session.has_worktree() {
        "⑂"
    } else {
        ""
    }
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
    use eix_core::SearchResult;

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
        let text = App::transcript_lines(&view, 60, Theme::default())
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("write_file out.txt (+5 bytes)"));
        assert!(text.contains("write_file → ok"));
        assert!(text.contains("wrote 5 bytes to out.txt"));
    }

    #[test]
    fn background_attention_is_marked_and_cleared() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.views.push(SessionView::new(
            Session::new("s2", "two", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;

        app.handle_event(
            Event::Notification(eix_core::Notification::new(
                "session.attention",
                serde_json::json!({"session_id": "s2", "status": "awaiting_approval"}),
            )),
            &tx,
        );
        assert_eq!(app.views[1].session.status, SessionStatus::AwaitingApproval);
        assert!(app.pane_needs_attention(1));
        assert_eq!(app.attention_count(), 1);

        app.focus_next_attention();
        assert_eq!(app.focused, 1);
        assert!(!app.pane_needs_attention(1));
        assert_eq!(app.attention_count(), 0);
    }

    #[test]
    fn background_attention_rings_only_for_background_panes_when_enabled() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.views.push(SessionView::new(
            Session::new("s2", "two", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;

        let attention = |session: &str| {
            Event::Notification(eix_core::Notification::new(
                "session.attention",
                serde_json::json!({"session_id": session, "status": "awaiting_approval"}),
            ))
        };

        // Off by default.
        app.handle_event(attention("s2"), &tx);
        assert!(!app.take_ring());

        app.bell = true;
        // The focused pane is already visible.
        app.handle_event(attention("s1"), &tx);
        assert!(!app.take_ring());

        // A background pane rings exactly once.
        app.handle_event(attention("s2"), &tx);
        assert!(app.take_ring());
        assert!(!app.take_ring());
    }

    #[test]
    fn attach_replays_a_pending_approval() {
        let session = Session::new("s1", "tools", "/tmp");
        let approval = SessionEvent::new(
            "s1",
            1,
            EventKind::ApprovalRequired,
            serde_json::json!({
                "approval_id": "a1",
                "call_id": "c1",
                "tool": "write_file",
                "summary": "write_file out.txt",
                "arguments": {"path": "out.txt", "content": "x"},
                "diff": "--- a/out.txt\n+++ b/out.txt\n@@ -1 +1 @@\n-x\n+y\n",
            }),
        );
        let result = SessionEvent::new(
            "s1",
            2,
            EventKind::ToolResult,
            serde_json::json!({
                "call_id": "c1",
                "tool": "write_file",
                "ok": true,
                "denied": false,
                "output": "done",
            }),
        );

        let pending = SessionView::new(session.clone(), vec![approval.clone()]).pending_approval();
        let pending = pending.expect("restored approval");
        assert_eq!(pending.approval_id, "a1");
        assert!(pending
            .diff
            .as_deref()
            .is_some_and(|diff| diff.contains("+y")));

        let resolved = SessionView::new(session, vec![approval, result]).pending_approval();
        assert!(resolved.is_none());
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
                kind: PendingKind::Tool,
                approval_id: "a1".to_string(),
                call_id: "c1".to_string(),
                tool: "write_file".to_string(),
                summary: "write_file out.txt".to_string(),
                diff: None,
                path: None,
                hunks: Vec::new(),
                selected: Vec::new(),
            },
        );

        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &tx);
        match rx.try_recv().expect("approval command") {
            Command::Approval {
                session_id,
                approval_id,
                decision,
                hunks,
                reason,
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(approval_id, "a1");
                assert_eq!(decision, ApprovalDecision::Approve);
                assert!(hunks.is_none());
                assert!(reason.is_none());
            }
            _ => panic!("expected an approval command"),
        }
        // The card stays until the daemon confirms the decision.
        assert!(app.approvals.contains_key("s1"));
    }

    fn app_with_diff(
        diff: Option<&str>,
    ) -> (
        App,
        tokio::sync::mpsc::UnboundedSender<Command>,
        tokio::sync::mpsc::UnboundedReceiver<Command>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
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
                kind: PendingKind::Tool,
                approval_id: "a1".to_string(),
                call_id: "c1".to_string(),
                tool: "write_file".to_string(),
                summary: "write_file out.txt".to_string(),
                diff: diff.map(str::to_string),
                path: None,
                hunks: Vec::new(),
                selected: Vec::new(),
            },
        );
        (app, tx, rx)
    }

    fn hunk(index: usize, header: &str, lines: &[&str]) -> HunkInfo {
        HunkInfo {
            index,
            header: header.to_string(),
            lines: lines.iter().map(|line| line.to_string()).collect(),
        }
    }

    fn app_with_hunks(hunks: Vec<HunkInfo>) -> App {
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
                kind: PendingKind::Tool,
                approval_id: "a1".to_string(),
                call_id: "c1".to_string(),
                tool: "write_file".to_string(),
                summary: "write_file out.txt".to_string(),
                diff: Some("unused".to_string()),
                path: None,
                selected: vec![true; hunks.len()],
                hunks,
            },
        );
        app
    }

    #[test]
    fn approval_reason_is_sent_with_a_denial() {
        let (mut app, tx, mut rx) = app_with_diff(None);

        app.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE), &tx);
        assert!(app.reason_prompt.is_some());
        for ch in "scope".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &tx);
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(app.reason_prompt.is_none());

        match rx.try_recv().expect("deny command") {
            Command::Approval {
                decision, reason, ..
            } => {
                assert_eq!(decision, ApprovalDecision::Deny);
                assert_eq!(reason.as_deref(), Some("scope"));
            }
            _ => panic!("expected a denial"),
        }
    }

    #[test]
    fn editing_a_staged_patch_requests_its_content() {
        let (mut app, tx, mut rx) = app_with_diff(Some("@@ -1 +1 @@\n-x\n+y\n"));
        if let Some(approval) = app.approvals.get_mut("s1") {
            approval.kind = PendingKind::Patch;
            approval.path = Some("out.txt".to_string());
        }

        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE), &tx);
        match rx.try_recv().expect("patch.get command") {
            Command::PatchGet {
                session_id,
                patch_id,
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(patch_id, "a1");
            }
            _ => panic!("expected a patch.get command"),
        }
    }

    #[test]
    fn inbox_lists_and_answers_pending_approvals() {
        let (mut app, tx, mut rx) = app_with_diff(None);
        app.sessions.push(Session::new("s1", "tools", "/tmp"));

        app.handle_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE), &tx);
        let inbox = app.inbox.as_ref().expect("inbox opened");
        assert_eq!(inbox.items.len(), 1);
        assert_eq!(inbox.items[0].session_name, "tools");

        app.handle_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &tx);
        match rx.try_recv().expect("approval command") {
            Command::Approval {
                session_id,
                decision,
                ..
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(decision, ApprovalDecision::Approve);
            }
            _ => panic!("expected an approval command"),
        }

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(app.inbox.is_none());
    }

    #[test]
    fn diff_review_scrolls_and_closes() {
        let (mut app, tx, _rx) =
            app_with_diff(Some("--- a/out.txt\n+++ b/out.txt\n@@ -1 +1 @@\n-x\n+y\n"));

        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &tx);
        assert!(app.diff_view.is_some());

        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &tx);
        assert_eq!(app.diff_view.as_ref().map(|view| view.scroll), Some(1));

        app.handle_transcript_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(app.diff_view.is_none());
    }

    #[test]
    fn diff_review_answers_the_approval() {
        let (mut app, tx, mut rx) = app_with_diff(Some("@@ -1 +1 @@\n-x\n+y\n"));

        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &tx);
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &tx);
        assert!(app.diff_view.is_none());
        match rx.try_recv().expect("approval command") {
            Command::Approval {
                session_id,
                approval_id,
                decision,
                hunks,
                reason,
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(approval_id, "a1");
                assert_eq!(decision, ApprovalDecision::Approve);
                assert!(hunks.is_none());
                assert!(reason.is_none());
            }
            _ => panic!("expected an approval command"),
        }

        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &tx);
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &tx);
        match rx.try_recv().expect("deny command") {
            Command::Approval { decision, .. } => {
                assert_eq!(decision, ApprovalDecision::Deny);
            }
            _ => panic!("expected a deny command"),
        }
    }

    #[test]
    fn diff_review_applies_only_selected_hunks() {
        let hunks = vec![
            hunk(0, "@@ -1,2 +1,2 @@", &["@@ -1,2 +1,2 @@", "-a", "+A"]),
            hunk(1, "@@ -8,2 +8,2 @@", &["@@ -8,2 +8,2 @@", "-b", "+B"]),
        ];
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_hunks(hunks);
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &tx);
        // Move to the second hunk and deselect it.
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char(']'), KeyModifiers::NONE), &tx);
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &tx);

        match rx.try_recv().expect("approval command") {
            Command::Approval {
                decision, hunks, ..
            } => {
                assert_eq!(decision, ApprovalDecision::Approve);
                assert_eq!(hunks, Some(vec![0]));
            }
            _ => panic!("expected an approval command"),
        }
    }

    #[test]
    fn diff_review_approves_everything_when_all_hunks_are_selected() {
        let hunks = vec![
            hunk(0, "@@ -1 +1 @@", &["@@ -1 +1 @@", "-a", "+A"]),
            hunk(1, "@@ -8 +8 @@", &["@@ -8 +8 @@", "-b", "+B"]),
        ];
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_hunks(hunks);
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &tx);
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &tx);
        match rx.try_recv().expect("approval command") {
            Command::Approval { hunks, .. } => assert!(hunks.is_none()),
            _ => panic!("expected an approval command"),
        }
    }

    #[test]
    fn diff_review_refuses_to_apply_with_no_hunks_selected() {
        let hunks = vec![
            hunk(0, "@@ -1 +1 @@", &["@@ -1 +1 @@", "-a", "+A"]),
            hunk(1, "@@ -8 +8 @@", &["@@ -8 +8 @@", "-b", "+B"]),
        ];
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_hunks(hunks);
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &tx);
        // `a` deselects everything because everything starts selected.
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &tx);
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('y'), KeyModifiers::NONE), &tx);
        assert!(rx.try_recv().is_err());
        assert!(app.status.contains("no hunks selected"));
        assert!(app.diff_view.is_some());
    }

    #[test]
    fn diff_review_needs_a_diff() {
        let (mut app, tx, _rx) = app_with_diff(None);
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &tx);
        assert!(app.diff_view.is_none());
        assert!(app.status.contains("no diff"));
    }

    fn ledger_entry(id: &str, sector: Sector, kind: &str, tokens: usize) -> ContextEntryInfo {
        ContextEntryInfo {
            id: id.into(),
            sector,
            kind: kind.into(),
            tokens,
            chars: 0,
            preview: format!("{kind} preview"),
            detail: format!("{kind} detail"),
            pinned: false,
            dropped: false,
            compacted: false,
            auto: false,
            editable: true,
        }
    }

    #[test]
    fn ledger_view_tracks_the_cursor_and_parses_seqs() {
        let result = ContextLedgerResult {
            entries: vec![
                ledger_entry("system", Sector::System, "system", 10),
                ledger_entry("e12", Sector::History, "user", 40),
            ],
            sectors: Vec::new(),
            total_tokens: 50,
            budget_tokens: 100,
            trimmed: 0,
        };
        let mut view = LedgerView::new("s1".into(), result);
        assert_eq!(view.entry().map(|entry| entry.id.as_str()), Some("system"));
        assert!(LedgerView::seq_of(view.entry().unwrap()).is_none());
        view.cursor = 1;
        assert_eq!(LedgerView::seq_of(view.entry().unwrap()), Some(12));

        // A refresh never moves the cursor past the end.
        view.set_result(ContextLedgerResult {
            entries: vec![ledger_entry("system", Sector::System, "system", 10)],
            ..Default::default()
        });
        assert_eq!(view.cursor, 0);
    }

    #[test]
    fn ledger_lines_mark_dropped_and_pinned_entries() {
        let mut entry = ledger_entry("e1", Sector::History, "user", 40);
        entry.pinned = true;
        let text: String = ledger_entry_line(&entry, false, 80)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("pin"));
        assert!(text.contains("HIS"));

        entry.pinned = false;
        entry.dropped = true;
        entry.auto = true;
        let text: String = ledger_entry_line(&entry, true, 80)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("cut"));
        assert!(text.starts_with('▸'));
    }

    #[test]
    fn opening_the_ledger_sends_a_request_and_closes_with_esc() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &tx);
        assert!(app.ledger.is_some());
        assert!(rx.try_recv().is_ok());

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(app.ledger.is_none());
    }

    fn process_info(id: &str, status: ProcessStatus) -> ProcessInfo {
        ProcessInfo {
            id: id.into(),
            session_id: "s1".into(),
            command: "npm run dev".into(),
            cwd: "/tmp".into(),
            pid: 42,
            status,
            exit_code: None,
            started_ms: 0,
            ended_ms: None,
            bytes: 0,
            truncated: false,
            log: "/tmp/x.log".into(),
        }
    }

    #[test]
    fn process_view_keeps_the_selection_across_refreshes() {
        let mut view = ProcessView::new("s1".into());
        view.set_list(ProcessList {
            processes: vec![
                process_info("aaaaaaaa-1", ProcessStatus::Running),
                process_info("bbbbbbbb-2", ProcessStatus::Exited),
            ],
        });
        view.cursor = 1;
        // A newer process appears at the front; the cursor follows the id.
        view.set_list(ProcessList {
            processes: vec![
                process_info("cccccccc-3", ProcessStatus::Running),
                process_info("aaaaaaaa-1", ProcessStatus::Running),
                process_info("bbbbbbbb-2", ProcessStatus::Exited),
            ],
        });
        assert_eq!(
            view.selected().map(|process| process.id.as_str()),
            Some("bbbbbbbb-2")
        );
        // Shrinking the list clamps the cursor.
        view.set_list(ProcessList {
            processes: vec![process_info("aaaaaaaa-1", ProcessStatus::Running)],
        });
        assert_eq!(view.cursor, 0);
    }

    #[test]
    fn process_view_only_appends_output_for_the_shown_process() {
        let mut view = ProcessView::new("s1".into());
        view.set_tail(ProcessOutput {
            process_id: "aaaaaaaa-1".into(),
            text: "one\n".into(),
            bytes: 4,
            truncated: false,
            status: ProcessStatus::Running,
            exit_code: None,
        });
        view.append("bbbbbbbb-2", "ignored");
        assert_eq!(view.tail, "one\n");
        view.append("aaaaaaaa-1", "two\n");
        assert_eq!(view.tail, "one\ntwo\n");

        // Exits are reflected in the list.
        view.set_list(ProcessList {
            processes: vec![process_info("aaaaaaaa-1", ProcessStatus::Running)],
        });
        view.mark_exited("aaaaaaaa-1", ProcessStatus::Killed, None);
        assert_eq!(view.selected().unwrap().status, ProcessStatus::Killed);
    }

    #[test]
    fn opening_processes_sends_a_list_request_and_closes_with_q() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('P'), KeyModifiers::SHIFT), &tx);
        assert!(app.process_view.is_some());
        assert!(rx.try_recv().is_ok());

        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &tx);
        assert!(app.process_view.is_none());
        assert!(!app.should_quit, "q closes the overlay, not the app");
    }

    #[test]
    fn fork_key_copies_everything_and_ledger_f_forks_before_the_entry() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));

        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::SHIFT), &tx);
        match rx.try_recv().expect("fork command") {
            Command::Fork {
                session_id,
                at_seq,
                name,
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(at_seq, None);
                assert!(name.is_none());
            }
            _ => panic!("expected a fork command"),
        }

        // The ledger's `f` forks strictly before the selected message.
        let mut result = ContextLedgerResult {
            entries: vec![
                ledger_entry("system", Sector::System, "system", 10),
                ledger_entry("e12", Sector::History, "user", 40),
            ],
            ..Default::default()
        };
        result.entries[0].id = "system".into();
        let mut ledger = LedgerView::new("s1".into(), result);
        ledger.cursor = 1;
        app.ledger = Some(ledger);
        app.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE), &tx);
        match rx.try_recv().expect("ledger fork command") {
            Command::Fork { at_seq, .. } => assert_eq!(at_seq, Some(12)),
            _ => panic!("expected a fork command"),
        }
    }

    #[test]
    fn forked_session_opens_a_new_pane() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        let mut fork = Session::new("fork-1", "one~2", "/tmp");
        fork.parent_id = Some("s1".into());
        fork.forked_from_seq = Some(12);
        app.handle_event(Event::Forked(fork), &tx);
        assert_eq!(
            app.pending_attach
                .as_ref()
                .map(|pending| pending.session_id.as_str()),
            Some("fork-1")
        );
        assert!(app.sessions.iter().any(|session| session.id == "fork-1"));
        match rx.try_recv().expect("attach command") {
            Command::Attach(id) => assert_eq!(id, "fork-1"),
            _ => panic!("expected an attach command"),
        }
        assert!(app.status.contains("workspace") || app.status.contains("files"));
    }

    #[test]
    fn search_overlay_runs_a_query_and_opens_the_selected_hit() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));

        // `#` from the transcript opens the overlay.
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('#'), KeyModifiers::SHIFT), &tx);
        assert!(app.search_view.is_some());

        // Typing edits the query; the first Enter runs the search.
        for c in "timeout".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &tx);
        }
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        match rx.try_recv().expect("search command") {
            Command::Search {
                query,
                session_id,
                limit,
            } => {
                assert_eq!(query, "timeout");
                assert!(session_id.is_none());
                assert!(limit.is_none());
            }
            _ => panic!("expected a search command"),
        }

        let hit = |session: &str, seq: u64| SearchHit {
            session_id: session.to_string(),
            session_name: session.to_string(),
            seq,
            role: "user".into(),
            ts: 0,
            snippet: format!("snippet {seq}"),
        };
        app.handle_event(
            Event::Search(SearchResult {
                query: "timeout".into(),
                hits: vec![hit("s1", 3), hit("s9", 7)],
            }),
            &tx,
        );
        assert_eq!(app.search_view.as_ref().unwrap().hits.len(), 2);
        assert!(!app.search_view.as_ref().unwrap().dirty);

        // Down selects the second hit; Enter opens its session in a new pane.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(app.search_view.is_none());
        match rx.try_recv().expect("attach command") {
            Command::Attach(id) => assert_eq!(id, "s9"),
            _ => panic!("expected an attach command"),
        }
        assert!(app.pending_attach.is_some());
    }

    #[test]
    fn picker_kinds_map_to_daemon_providers() {
        assert_eq!(PickerKind::Files.data_kind(), Some(eix_core::KIND_FILES));
        assert_eq!(
            PickerKind::Worktrees.data_kind(),
            Some(eix_core::KIND_WORKTREES)
        );
        assert_eq!(
            PickerKind::Branches.data_kind(),
            Some(eix_core::KIND_BRANCHES)
        );
        assert_eq!(
            PickerKind::Context.data_kind(),
            Some(eix_core::KIND_CONTEXT)
        );
        assert!(PickerKind::Sessions.data_kind().is_none());
        assert!(PickerKind::Commands.data_kind().is_none());
        assert!(!PickerKind::Files.title().is_empty());
        assert!(!PickerKind::Commands.hint().is_empty());
    }

    #[test]
    fn command_palette_ids_are_unique() {
        let items = command_items();
        assert!(items.len() >= 10);
        let mut ids: Vec<&str> = items.iter().map(|item| item.value.as_str()).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), items.len());
        assert!(items.iter().all(|item| !item.label.is_empty()));
        assert!(items.iter().any(|item| item.value == "quit"));
    }

    #[test]
    fn file_picker_inserts_at_path_into_the_native_prompt() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.picker = Some(Picker {
            kind: PickerKind::Files,
            query: String::new(),
            selected: 0,
            items: vec![PickerItem::new("src/main.rs"), PickerItem::new("README.md")],
            loading: false,
        });
        app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(app.picker.is_none());
        assert_eq!(app.input.text(), "@src/main.rs");
        assert!(matches!(app.focus, Focus::Input));
    }

    #[test]
    fn command_palette_enter_runs_the_command() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.open_picker(PickerKind::Commands, &tx);
        let help = app
            .picker
            .as_ref()
            .unwrap()
            .items
            .iter()
            .position(|item| item.value == "help")
            .expect("help command");
        app.picker.as_mut().unwrap().selected = help;
        app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(app.help);
        assert!(app.picker.is_none());
    }

    #[test]
    fn command_palette_filters_across_label_value_and_detail() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.open_picker(PickerKind::Commands, &tx);
        for c in "git work".chars() {
            app.handle_picker_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &tx);
        }
        let filtered = app.picker_filtered();
        assert_eq!(filtered.len(), 1);
        assert_eq!(
            app.picker.as_ref().unwrap().items[filtered[0]].value,
            "worktrees"
        );
    }

    #[test]
    fn data_picker_requests_its_provider() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;
        app.open_picker(PickerKind::Branches, &tx);
        assert!(app.picker.as_ref().unwrap().loading);
        match rx.try_recv().expect("picker command") {
            Command::PickerData { session_id, kind } => {
                assert_eq!(session_id, "s1");
                assert_eq!(kind, eix_core::KIND_BRANCHES);
            }
            _ => panic!("expected a picker command"),
        }
    }

    #[test]
    fn data_picker_without_a_session_stays_closed() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.open_picker(PickerKind::Files, &tx);
        assert!(app.picker.is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn nvim_mode_strings_classify_insert() {
        assert!(nvim_is_insert(""));
        assert!(nvim_is_insert("insert"));
        assert!(nvim_is_insert("replace"));
        assert!(!nvim_is_insert("normal"));
        assert!(!nvim_is_insert("visual"));
        assert!(!nvim_is_insert("cmdline_normal"));
    }

    #[test]
    fn esc_keeps_focus_in_the_prompt_until_a_double_press() {
        let mut app = App::new(None);
        app.focus = Focus::Input;

        app.handle_nvim_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.focus, Focus::Input));

        app.handle_nvim_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.focus, Focus::Transcript));
        assert!(app.esc_at.is_none());
    }

    #[test]
    fn other_keys_break_the_double_esc_gesture() {
        let mut app = App::new(None);
        app.focus = Focus::Input;

        app.handle_nvim_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        app.handle_nvim_input_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE));
        app.handle_nvim_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(matches!(app.focus, Focus::Input));
    }
}
