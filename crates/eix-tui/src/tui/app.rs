//! TUI state and rendering.
//!
//! The prompt is a self-contained Vim-style editor (see [`super::editor`]);
//! there is no embedded editor process.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use eix_core::{
    theme_preset, ApprovalDecision, ApprovalRequiredPayload, AssistantMessagePayload, AttachResult,
    ContextEntryInfo, ContextLedgerResult, Event as SessionEvent, EventKind, HelloResult, HunkInfo,
    InputAnsweredPayload, InputOption, InputRequiredPayload, PatchProposedPayload,
    PatchResultPayload, PermissionLevel, PickerItem, ProcessInfo, ProcessList, ProcessOutput,
    ProcessStatus, RewindResult, SearchHit, Sector, Session, SessionStatus, SubmitResult,
    ToolResultPayload, UserMessagePayload, THEME_PRESETS,
};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use ratatui::Frame;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::controller::{Command, ContextOp, Event};
use super::editor::{leader_action, Editor, Mode, Outcome};
use super::keys::{KeyAction, Keybinds};
use super::theme::Theme;
use super::transcript::{self, FoldId};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Transcript,
    Input,
}

/// Two `Esc` presses within this window leave the prompt for the transcript.
const ESC_WINDOW: Duration = Duration::from_millis(500);

/// Picker value for "no preset" (keep the terminal/default palette).
const THEME_DEFAULT_VALUE: &str = "@default";

/// Running sessions use a short animated glyph; idle sessions show `○`.
const RUNNING_SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

/// Re-read the focused session's git branch at this interval.
const GIT_BRANCH_REFRESH: Duration = Duration::from_secs(5);

/// Start loading the previous history page when the viewport is this close to
/// the top, and wait until it scrolls further down before arming another page.
const HISTORY_PRELOAD_LINES: usize = 3;
const HISTORY_REARM_LINES: usize = 12;

/// A prompt `/command`. Typed as a whole message line and never sent to the
/// model. `args` is a display hint; parsing is intentionally forgiving.
/// `template` is set for user-defined commands loaded from Markdown.
#[derive(Clone)]
struct CommandSpec {
    name: String,
    args: String,
    description: String,
    template: Option<String>,
    agent: Option<String>,
    model: Option<String>,
}

fn builtin(name: &str, args: &str, description: &str) -> CommandSpec {
    CommandSpec {
        name: name.to_string(),
        args: args.to_string(),
        description: description.to_string(),
        template: None,
        agent: None,
        model: None,
    }
}

/// Built-in prompt commands, in menu order.
fn builtin_commands() -> Vec<CommandSpec> {
    vec![
        builtin("help", "", "show the key and command reference"),
        builtin("new", "[name]", "start a new session in this workspace"),
        builtin("clear", "", "forget this conversation's context"),
        builtin(
            "compact",
            "[notes]",
            "summarize the history to free context",
        ),
        builtin(
            "btw",
            "<question>",
            "ask a one-off question without touching history",
        ),
        builtin("cd", "[path]", "change this session's working directory"),
        builtin("init", "", "draft an AGENTS.md for this workspace"),
        builtin(
            "queue",
            "[clear]",
            "show or clear prompts waiting for the session",
        ),
        builtin("sessions", "", "open the session picker"),
        builtin("models", "[id]", "list or pin a model for this session"),
        builtin("agents", "[name]", "choose the session's primary agent"),
        builtin("mcp", "", "show MCP servers and their tools"),
        builtin("reload", "", "re-read the daemon config files"),
        builtin(
            "timeline",
            "[n]",
            "show recent turns, answers and tool calls",
        ),
        builtin("archive", "", "hide this session from the picker"),
        builtin("delete", "", "delete this session (run twice to confirm)"),
        builtin("undo", "", "hide the newest turn (/redo restores it)"),
        builtin("redo", "", "restore the most recently undone turn"),
        builtin(
            "skills",
            "[name] [args]",
            "browse skills with a preview, or insert one",
        ),
        builtin("todos", "", "show the model's task list"),
        builtin("image", "<path>", "attach an image to the next prompt"),
        builtin("theme", "", "choose a color preset with live preview"),
        builtin(
            "permission",
            "<level>",
            "read-only · workspace-write · full",
        ),
        builtin("fork", "[name]", "fork this session into a new pane"),
        builtin("commands", "", "reload user-defined Markdown commands"),
        builtin("exit", "", "save and close this session"),
        builtin("quit", "", "save and close this session"),
        builtin("close", "", "save and close this session"),
    ]
}

/// Merge built-ins with user commands. A custom command with the same name
/// replaces the built-in entry rather than creating a duplicate.
fn merge_commands(cwd: &std::path::Path) -> Vec<CommandSpec> {
    let mut commands = builtin_commands();
    let custom: Vec<_> = eix_core::load_commands(cwd);
    for command in custom {
        let spec = CommandSpec {
            name: command.name.clone(),
            args: String::new(),
            description: command.description,
            template: Some(command.template),
            agent: command.agent,
            model: command.model,
        };
        if let Some(existing) = commands.iter_mut().find(|item| item.name == spec.name) {
            *existing = spec;
        } else {
            commands.push(spec);
        }
    }
    commands
}

/// Prompt height: [`super::editor::PROMPT_ROWS`] content rows plus the two
/// borders. Long drafts grow the box up to a small cap before the editor
/// starts scrolling, and short terminals always keep transcript rows.
fn prompt_input_height(area_height: u16, area_width: u16, input: &Editor) -> u16 {
    const PROMPT_MAX_ROWS: usize = 12;
    let inner_width = area_width.saturating_sub(4).max(1) as usize;
    let rows = input.layout(inner_width, 1).segments.len();
    let visible = rows.clamp(super::editor::PROMPT_ROWS as usize, PROMPT_MAX_ROWS) as u16;
    (visible + 2).min(area_height.saturating_sub(5).max(3))
}

// Keep the prompt readable without a separate panel: its chrome paints over
// the theme canvas (or the terminal background) instead of an opaque composer
// box.
const COMPOSER_TEXT: Color = Color::Rgb(220, 223, 228);
const COMPOSER_MUTED: Color = Color::Rgb(110, 116, 129);
const COMPOSER_BORDER: Color = Color::Rgb(74, 88, 120);
const COMPOSER_ACCENT: Color = Color::Rgb(122, 162, 247);
const COMPOSER_SUCCESS: Color = Color::Rgb(126, 199, 149);
const COMPOSER_WARN: Color = Color::Rgb(229, 192, 123);
const COMPOSER_DANGER: Color = Color::Rgb(231, 130, 132);
const COMPOSER_BADGE_TEXT: Color = Color::Rgb(20, 22, 28);
const COMPOSER_SELECTION: Color = Color::Rgb(54, 60, 76);

/// A mouse text selection in transcript-line/cell coordinates.
#[derive(Clone, Copy, Debug)]
struct TextSelection {
    anchor: (usize, usize),
    cursor: (usize, usize),
    dragging: bool,
    moved: bool,
}

/// Parsed and wrapped history for one pane. Only the live turn is re-rendered
/// on each frame; stored events stay here until the log, width, theme or fold
/// state changes.
struct HistoryCache {
    width: usize,
    theme: Theme,
    folds: HashMap<FoldId, bool>,
    events_rev: u64,
    lines: Vec<Line<'static>>,
}

struct SessionView {
    session: Session,
    events: Vec<SessionEvent>,
    /// Bumped whenever `events` changes so the render cache can be reused.
    events_rev: u64,
    history_cache: Option<HistoryCache>,
    /// Older stored events exist before the first loaded event.
    history_has_more: bool,
    /// A history request is in flight.
    history_loading: bool,
    /// Cleared while a page loads so one page cannot immediately request more.
    history_armed: bool,
    /// `(old history line count, old scroll)` captured before prepending, so
    /// the viewport keeps its place when the new page is rendered.
    history_anchor: Option<(usize, usize)>,
    stream: Option<StreamState>,
    scroll: usize,
    follow: bool,
    /// Explicit open/closed state for collapsible Thinking and tool blocks.
    folds: HashMap<FoldId, bool>,
    /// Line -> foldable block map from the last draw, used for click-to-fold.
    slots: Vec<Option<transcript::Fold>>,
    /// Plain text for every transcript line from the last draw, used when a
    /// mouse selection is copied.
    text_lines: Vec<String>,
    /// Active mouse selection, if any.
    selection: Option<TextSelection>,
}

impl SessionView {
    fn new(session: Session, events: Vec<SessionEvent>) -> Self {
        Self {
            session,
            events,
            events_rev: 0,
            history_cache: None,
            history_has_more: false,
            history_loading: false,
            history_armed: true,
            history_anchor: None,
            stream: None,
            scroll: 0,
            follow: true,
            folds: HashMap::new(),
            slots: Vec::new(),
            text_lines: Vec::new(),
            selection: None,
        }
    }

    /// Rebuild the cached history when its inputs changed. Stored events are
    /// immutable between pushes, so this is normally a no-op on redraws.
    fn ensure_history(&mut self, width: usize, theme: Theme) {
        let stale = self.history_cache.as_ref().is_none_or(|cache| {
            cache.width != width
                || cache.theme != theme
                || cache.folds != self.folds
                || cache.events_rev != self.events_rev
        });
        if !stale {
            return;
        }
        let transcript::Transcript { lines, slots } =
            transcript::render_history(&self.events, &self.folds, width, theme);
        self.slots = slots;
        self.text_lines = lines.iter().map(text_line).collect();
        self.history_cache = Some(HistoryCache {
            width,
            theme,
            folds: self.folds.clone(),
            events_rev: self.events_rev,
            lines,
        });
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
                            // Shell approvals carry a deliberately short
                            // daemon summary. Keep the real command line so the
                            // card can show it in full instead of approving a
                            // truncated command.
                            let summary =
                                if matches!(payload.tool.as_str(), "run_command" | "start_command")
                                {
                                    payload
                                        .arguments
                                        .get("command")
                                        .and_then(serde_json::Value::as_str)
                                        .map(|command| format!("$ {command}"))
                                        .unwrap_or(payload.summary)
                                } else {
                                    payload.summary
                                };
                            let path = payload
                                .arguments
                                .get("path")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string);
                            tool = Some(PendingApproval {
                                kind: PendingKind::Tool,
                                approval_id: payload.approval_id,
                                call_id: payload.call_id,
                                tool: payload.tool,
                                summary,
                                diff: payload.diff,
                                path,
                                selected: vec![true; payload.hunks.len()],
                                hunks: payload.hunks,
                                cursor: 0,
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
                                cursor: 0,
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        tool.or(patch)
    }

    /// Find the newest unanswered `ask_user` question in the replayable log.
    fn pending_input(&self) -> Option<PendingInput> {
        let mut answered = HashSet::new();
        for event in &self.events {
            if event.kind != EventKind::InputAnswered {
                continue;
            }
            if let Ok(payload) =
                serde_json::from_value::<InputAnsweredPayload>(event.payload.clone())
            {
                answered.insert(payload.input_id);
            }
        }
        let mut pending = None;
        for event in &self.events {
            if event.kind != EventKind::InputRequired {
                continue;
            }
            if let Ok(payload) =
                serde_json::from_value::<InputRequiredPayload>(event.payload.clone())
            {
                if !answered.contains(&payload.input_id) {
                    pending = Some(PendingInput {
                        input_id: payload.input_id,
                        call_id: payload.call_id,
                        question: payload.question,
                        options: payload.options,
                        cursor: 0,
                    });
                }
            }
        }
        pending
    }
}

struct StreamState {
    turn_id: String,
    text: String,
    reasoning: String,
}

impl StreamState {
    fn new(turn_id: String) -> Self {
        Self {
            turn_id,
            text: String::new(),
            reasoning: String::new(),
        }
    }
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
    /// Pin the focused session to a configured model.
    Models,
    /// Select the focused session's primary agent.
    Agents,
    /// Browse installed skills, with a body preview.
    Skills,
    /// Preview and apply a color preset.
    Themes,
}

impl PickerKind {
    /// Daemon provider backing this kind, if any.
    fn data_kind(self) -> Option<&'static str> {
        match self {
            Self::Files => Some(eix_core::KIND_FILES),
            Self::Worktrees => Some(eix_core::KIND_WORKTREES),
            Self::Branches => Some(eix_core::KIND_BRANCHES),
            Self::Context => Some(eix_core::KIND_CONTEXT),
            Self::Sessions
            | Self::NewPane
            | Self::Commands
            | Self::Models
            | Self::Agents
            | Self::Skills
            | Self::Themes => None,
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
            Self::Models => "models — pin this session",
            Self::Agents => "agents — choose the primary agent",
            Self::Skills => "skills — preview and insert",
            Self::Themes => "themes — live preview",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Sessions => "Enter open · ↑/↓ or C-j/C-k · Esc close",
            Self::NewPane => "Enter split · ↑/↓ or C-j/C-k · Esc close",
            Self::Files => "Enter insert · ↑/↓ or C-j/C-k · Esc close",
            Self::Worktrees => "Enter insert path · ↑/↓ or C-j/C-k · Esc close",
            Self::Branches => "Enter insert name · ↑/↓ or C-j/C-k · Esc close",
            Self::Context => "Enter open ledger · ↑/↓ or C-j/C-k · Esc close",
            Self::Commands => "Enter run · ↑/↓ or C-j/C-k · Esc close",
            Self::Models => "Enter pin · Esc close",
            Self::Agents => "Enter select · ↑/↓ or C-j/C-k · Esc close",
            Self::Skills => "Enter insert · preview on the right · Esc close",
            Self::Themes => "Enter apply · ↑/↓ live preview · Esc cancel",
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
    /// Highlighted row in the action list.
    cursor: usize,
}

/// A row in the approval card's action list. The first action is the
/// recommendation and Enter picks it without moving the cursor.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ApprovalAction {
    Approve,
    Always,
    Deny,
    Reason,
    Review,
    Open,
    Edit,
}

impl PendingApproval {
    fn actions(&self) -> Vec<ApprovalAction> {
        let mut actions = vec![ApprovalAction::Approve, ApprovalAction::Always];
        if self.diff.as_deref().is_some_and(|diff| !diff.is_empty()) {
            actions.push(ApprovalAction::Review);
        }
        if self.path.is_some() {
            actions.push(ApprovalAction::Open);
        }
        if self.kind == PendingKind::Patch {
            actions.push(ApprovalAction::Edit);
        }
        actions.push(ApprovalAction::Deny);
        actions.push(ApprovalAction::Reason);
        actions
    }

    fn action_label(&self, action: ApprovalAction) -> String {
        let patch = self.kind == PendingKind::Patch;
        match action {
            ApprovalAction::Approve if patch => "Apply change".to_string(),
            ApprovalAction::Approve => "Approve".to_string(),
            ApprovalAction::Always if patch => "Apply and always allow".to_string(),
            ApprovalAction::Always => "Always allow".to_string(),
            ApprovalAction::Deny if patch => "Reject".to_string(),
            ApprovalAction::Deny => "Deny".to_string(),
            ApprovalAction::Reason if patch => "Reject with reason…".to_string(),
            ApprovalAction::Reason => "Deny with reason…".to_string(),
            ApprovalAction::Review if self.hunks.is_empty() => "Review diff".to_string(),
            ApprovalAction::Review => "Review hunks".to_string(),
            ApprovalAction::Open => "Open file".to_string(),
            ApprovalAction::Edit => "Edit staged content".to_string(),
        }
    }
}

/// An `ask_user` question waiting for the user to pick an option.
#[derive(Clone)]
struct PendingInput {
    input_id: String,
    /// Tool call that produced the question, used to match `tool.completed`.
    call_id: String,
    question: String,
    options: Vec<InputOption>,
    /// Highlighted option; starts at the recommended (first) one.
    cursor: usize,
}

/// Inline reason for denying an approval (`r` on the card).
struct ReasonPrompt {
    session_id: String,
    text: String,
}

/// A `/btw` side question: answered with a throwaway context so the main
/// conversation and its ledger stay untouched.
struct BtwView {
    session_id: String,
    btw_id: String,
    question: String,
    text: String,
    error: Option<String>,
    done: bool,
    scroll: usize,
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
    /// Last rendered body width. Zero until the first frame.
    width: usize,
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
    input: Editor,
    focus: Focus,
    picker: Option<Picker>,
    help: bool,
    /// Interactive filter for the help overlay.
    help_query: String,
    /// Selected row in the filtered help list.
    help_selected: usize,
    /// Recent timeline items for the read-only overlay.
    timeline: Option<Vec<eix_core::TimelineItem>>,
    timeline_scroll: usize,
    /// First `/delete` arms a confirmation; the second one deletes.
    pending_delete: Option<String>,
    /// Images attached with `/image`, sent with the next prompt.
    pending_images: Vec<String>,
    /// Images belonging to an in-flight prompt, restored if submit fails.
    inflight_images: Vec<String>,
    /// `Ctrl-W` was pressed; the next key is a window command.
    pending_window: bool,
    /// `<Space>` was pressed in the transcript; the next key is a leader action.
    pending_leader: bool,
    /// Text selected with the mouse, waiting for the event loop to copy it to
    /// the system clipboard.
    pending_clipboard: Option<String>,
    /// Older-history request picked up by the event loop after drawing.
    pending_history: Option<(String, u64)>,
    /// Pane rectangles from the last draw, used to pick a focus direction.
    pane_areas: Vec<Rect>,
    /// Tool calls waiting on a decision, keyed by session id.
    approvals: HashMap<String, PendingApproval>,
    /// `ask_user` questions waiting on an answer, keyed by session id.
    inputs: HashMap<String, PendingInput>,
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
    /// Cached branch of the focused session's cwd for the prompt header.
    git_cwd: Option<std::path::PathBuf>,
    git_branch: Option<String>,
    git_checked: Option<Instant>,
    /// Ring the terminal bell when a background pane needs attention.
    pub bell: bool,
    /// Emit OSC 9/777 notifications and update the title on attention.
    pub osc: bool,
    /// New-session defaults from cli.json.
    pub default_model: Option<String>,
    pub default_agent: Option<String>,
    pub default_permission: Option<String>,
    /// Set when a notification should ring; cleared by the event loop.
    ring: bool,
    /// Text for the next alert (session name + status).
    last_alert: Option<String>,
    /// Session whose daemon draft is being loaded into a fresh buffer.
    draft_loading: Option<String>,
    /// Last draft submitted from the prompt (session id, text), kept so a
    /// failed turn can put it back into the buffer.
    pending_draft: Option<(String, String)>,
    /// Session the prompt currently belongs to, for daemon drafts.
    input_session: Option<String>,
    /// Text last written to the daemon, so `tick` only saves real changes.
    last_saved_draft: String,
    /// Open reason input for a denial.
    reason_prompt: Option<ReasonPrompt>,
    /// Ask the event loop to clear ratatui's cached frame after an editor.
    force_clear: bool,
    /// Cross-session approvals overlay.
    inbox: Option<InboxView>,
    /// Colors used by the transcript and approval cards.
    pub(crate) theme: Theme,
    /// `[theme]` as loaded from config, kept so `/theme` can re-derive a
    /// palette from a different preset without losing explicit slot overrides.
    pub(crate) theme_config: eix_core::ThemeConfig,
    /// Palette/config captured when the theme picker opened. Esc restores it;
    /// Enter keeps the previewed selection.
    theme_picker_backup: Option<(eix_core::ThemeConfig, Theme)>,
    /// Transcript-mode key overrides from `[keys]`.
    pub(crate) keybinds: Keybinds,
    /// When the previous `Esc` reached the prompt, for the double-Esc
    /// "leave the input" gesture.
    esc_at: Option<Instant>,
    /// Prompt history cursor: `None` is the live draft, `Some(i)` is `i`
    /// entries back from the newest submitted prompt.
    history_index: Option<usize>,
    /// Draft captured when history browsing started, restored by Ctrl-J past
    /// the newest entry.
    history_draft: String,
    /// Prompts submitted while their session was busy, keyed by session id.
    /// `tick` drains one at a time as sessions go idle.
    queues: HashMap<String, VecDeque<String>>,
    /// A queued prompt whose `prompt.submit` is in flight, kept so a `busy`
    /// race can put it back at the head of the queue.
    queued_inflight: Option<(String, String)>,
    /// Session whose prompt submit is currently in flight (if any).
    submitting_session: Option<String>,
    /// Selected row in the `/` command menu.
    command_cursor: usize,
    /// Built-in and user-defined Markdown commands, in menu order.
    commands: Vec<CommandSpec>,
    /// `/` command menu was dismissed with Esc for the current draft.
    command_dismissed: bool,
    /// Streaming `/btw` side answer, shown above the prompt.
    btw: Option<BtwView>,
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
            input: Editor::new(),
            focus: Focus::Input,
            picker: None,
            help: false,
            help_query: String::new(),
            help_selected: 0,
            timeline: None,
            timeline_scroll: 0,
            pending_delete: None,
            pending_images: Vec::new(),
            inflight_images: Vec::new(),
            pending_window: false,
            pending_leader: false,
            pending_clipboard: None,
            pending_history: None,
            pane_areas: Vec::new(),
            approvals: HashMap::new(),
            inputs: HashMap::new(),
            diff_view: None,
            ledger: None,
            process_view: None,
            search_view: None,
            status: "connecting…".into(),
            error: None,
            connected: false,
            tick: 0,
            git_cwd: None,
            git_branch: None,
            git_checked: None,
            bell: false,
            osc: false,
            default_model: None,
            default_agent: None,
            default_permission: None,
            ring: false,
            last_alert: None,
            draft_loading: None,
            pending_draft: None,
            input_session: None,
            last_saved_draft: String::new(),
            reason_prompt: None,
            force_clear: false,
            inbox: None,
            theme: Theme::default(),
            theme_config: eix_core::ThemeConfig::default(),
            theme_picker_backup: None,
            keybinds: Keybinds::default(),
            esc_at: None,
            history_index: None,
            history_draft: String::new(),
            queues: HashMap::new(),
            queued_inflight: None,
            submitting_session: None,
            command_cursor: 0,
            commands: merge_commands(
                &std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            ),
            command_dismissed: false,
            btw: None,
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
            && self.btw.is_none()
            && self.error.is_none()
            && self.reason_prompt.is_none()
    }

    /// Open the help overlay with a fresh filter and selection.
    fn open_help(&mut self) {
        self.pending_leader = false;
        self.help = true;
        self.help_query.clear();
        self.help_selected = 0;
    }

    /// Keys while help is open: typing filters, arrows/C-n/C-p select, Esc
    /// closes. Plain `q`/`j`/`k` are search input rather than shortcuts so any
    /// key entry can be found by name.
    fn handle_help_key(&mut self, key: KeyEvent) {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => self.help = false,
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
            KeyCode::Char('p') if control => self.move_help_selection(-1),
            KeyCode::Char('n') if control => self.move_help_selection(1),
            KeyCode::Home => self.help_selected = 0,
            KeyCode::End => {
                self.help_selected = help_entry_count(&self.help_query).saturating_sub(1);
            }
            KeyCode::Char(ch) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                self.help_query.push(ch);
                self.help_selected = 0;
            }
            _ => {}
        }
    }

    fn move_help_selection(&mut self, delta: isize) {
        let count = help_entry_count(&self.help_query);
        if count == 0 {
            self.help_selected = 0;
            return;
        }
        let selected = self.help_selected.min(count - 1) as isize;
        self.help_selected = (selected + delta).clamp(0, count as isize - 1) as usize;
    }

    /// Put the last submitted draft back after a failed turn, if it belongs to
    /// `session_id` and the prompt is still empty (the user may already be
    /// typing the next message).
    fn restore_pending_draft(&mut self, session_id: &str) {
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
                self.input.set_text(&draft);
            }
        }
    }

    /// Drop the saved draft once its turn finishes successfully.
    fn clear_pending_draft(&mut self, session_id: &str) {
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

    /// Text for the most recent background alert.
    pub fn alert_message(&self) -> Option<String> {
        self.last_alert.clone()
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

    pub fn tick(&mut self, tx: &UnboundedSender<Command>) {
        self.tick = self.tick.wrapping_add(1);
        self.refresh_git_branch();
        self.sync_draft(tx);
        self.flush_queues(tx);
    }

    /// Update the branch shown in the prompt header. Spawning `git` on every
    /// tick would be wasteful, so a cached value is reused for a few seconds.
    fn refresh_git_branch(&mut self) {
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

    /// Current prompt contents.
    fn current_draft_text(&self) -> String {
        self.input.text()
    }

    pub fn handle_paste(&mut self, text: &str) {
        self.focus = Focus::Input;
        // Terminal drag-and-drop usually arrives as a bracketed paste of one
        // or more paths. Attach them instead of dumping quoted paths into the
        // prompt. Normal text pastes unchanged.
        let cwd = self
            .focused_view()
            .map(|view| view.session.cwd.clone())
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        if let Some(paths) = parse_image_paths(text, &cwd) {
            for path in paths {
                if !self.pending_images.contains(&path) {
                    self.pending_images.push(path);
                }
            }
            self.status = format!(
                "attached {} image(s) for the next prompt",
                self.pending_images.len()
            );
            return;
        }
        self.input.paste(text);
    }

    /// Mouse clicks focus a pane; dragging selects transcript text and copies
    /// it on release. A click without a drag still toggles the block under the
    /// cursor. The wheel scrolls the pane under the cursor.
    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        self.pending_leader = false;
        let (x, y) = (mouse.column, mouse.row);
        let pane = self.pane_areas.iter().position(|rect| {
            x >= rect.x && x < rect.x + rect.width && y >= rect.y && y < rect.y + rect.height
        });
        let pane = pane.filter(|index| *index < self.views.len());
        if let Some(index) = pane {
            match mouse.kind {
                MouseEventKind::ScrollUp => {
                    self.focused = index;
                    self.scroll_focused(-3);
                    return;
                }
                MouseEventKind::ScrollDown => {
                    self.focused = index;
                    self.scroll_focused(3);
                    return;
                }
                _ => {}
            }
        }

        let active = self
            .views
            .iter()
            .position(|view| view.selection.is_some_and(|selection| selection.dragging));
        let index = match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let Some(index) = pane else {
                    self.focus = Focus::Input;
                    for view in &mut self.views {
                        view.selection = None;
                    }
                    return;
                };
                self.focused = index;
                self.focus = Focus::Transcript;
                for view in &mut self.views {
                    view.selection = None;
                }
                index
            }
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left) => {
                // Keep tracking the pane where the drag started, even when the
                // pointer crosses into another pane or leaves the transcript.
                let Some(index) = active else {
                    return;
                };
                index
            }
            _ => return,
        };
        let rect = self.pane_areas.get(index).copied().unwrap_or_default();
        if rect.width < 3 || rect.height < 3 {
            return;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let position = self.selection_position(index, rect, x, y, false);
                self.views[index].selection = position.map(|(line, column)| TextSelection {
                    anchor: (line, column),
                    cursor: (line, column),
                    dragging: true,
                    moved: false,
                });
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let Some(selection) = self.views[index].selection else {
                    return;
                };
                if !selection.dragging {
                    return;
                }
                let Some(position) = self.selection_position(index, rect, x, y, true) else {
                    return;
                };
                let view = &mut self.views[index];
                if let Some(selection) = view.selection.as_mut() {
                    if position != selection.cursor {
                        selection.moved = true;
                        selection.cursor = position;
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let mut copied = None;
                let mut fold = None;
                {
                    let view = &mut self.views[index];
                    let Some(selection) = view.selection.as_mut() else {
                        return;
                    };
                    selection.dragging = false;
                    if selection.moved {
                        let text = selection_text(&view.text_lines, selection);
                        if !text.is_empty() {
                            copied = Some(text);
                        }
                    } else {
                        fold = view.slots.get(selection.anchor.0).and_then(|slot| *slot);
                    }
                }
                if let Some(fold) = fold {
                    self.views[index].folds.insert(fold.id, !fold.open);
                    self.views[index].selection = None;
                } else if let Some(text) = copied {
                    self.status = format!("copied {} chars", text.chars().count());
                    // Seed the prompt's unnamed register too: normal-mode `p`
                    // can then paste the same text even if the native
                    // clipboard helper is unavailable.
                    self.input.set_register_text(&text);
                    self.pending_clipboard = Some(text);
                } else {
                    // A plain click, or a drag over nothing, leaves no
                    // zero-width selection behind.
                    self.views[index].selection = None;
                }
            }
            _ => {}
        }
    }

    /// Map a screen cell to a transcript line/column, clamping drags to the
    /// pane and auto-scrolling when the pointer crosses its top or bottom.
    fn selection_position(
        &mut self,
        index: usize,
        rect: Rect,
        x: u16,
        y: u16,
        dragging: bool,
    ) -> Option<(usize, usize)> {
        let borders = pane_borders(&self.pane_areas, index);
        let left = rect
            .x
            .saturating_add(u16::from(borders.contains(Borders::LEFT)));
        let right = rect
            .x
            .saturating_add(rect.width)
            .saturating_sub(u16::from(borders.contains(Borders::RIGHT)));
        let top = rect
            .y
            .saturating_add(u16::from(borders.contains(Borders::TOP)));
        let bottom = rect
            .y
            .saturating_add(rect.height)
            .saturating_sub(u16::from(borders.contains(Borders::BOTTOM)));
        if right <= left || bottom <= top {
            return None;
        }
        if !dragging && (x < left || x >= right || y < top || y >= bottom) {
            return None;
        }

        let view = &mut self.views[index];
        if dragging {
            let inner_height = rect
                .height
                .saturating_sub(u16::from(borders.contains(Borders::TOP)))
                .saturating_sub(u16::from(borders.contains(Borders::BOTTOM)))
                as usize;
            if y < top {
                view.history_armed = true;
                if view.scroll > 0 {
                    view.scroll -= 1;
                    view.follow = false;
                }
            } else if y >= bottom {
                let max_scroll = view.text_lines.len().saturating_sub(inner_height);
                if view.scroll < max_scroll {
                    view.scroll += 1;
                    view.follow = false;
                }
            }
        }
        let x = x.clamp(left, right.saturating_sub(1));
        let y = y.clamp(top, bottom.saturating_sub(1));
        let line = view.scroll.saturating_add(y.saturating_sub(top) as usize);
        if view.text_lines.is_empty() {
            return Some((line, 0));
        }
        let line = line.min(view.text_lines.len() - 1);
        let width = UnicodeWidthStr::width(view.text_lines[line].as_str());
        Some((line, ((x - left) as usize).min(width)))
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
            Event::HistoryPage {
                session_id,
                events,
                has_more,
            } => self.history_page(session_id, events, has_more),
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
            Event::Models(models) => {
                if let Some(picker) = &mut self.picker {
                    if picker.kind == PickerKind::Models {
                        picker.items = model_picker_items(&models);
                        picker.selected = 0;
                        picker.loading = false;
                    }
                }
                self.status = format!("{} model(s)", models.len());
            }
            Event::Agents(agents) => {
                if let Some(picker) = &mut self.picker {
                    if picker.kind == PickerKind::Agents {
                        picker.items = agent_picker_items(&agents);
                        picker.selected = 0;
                        picker.loading = false;
                    }
                }
                self.status = format!("{} agent(s)", agents.len());
            }
            Event::Mcp(servers) => {
                if servers.is_empty() {
                    self.status = "no MCP servers configured (see [mcp.servers])".into();
                } else {
                    let parts: Vec<String> = servers
                        .iter()
                        .map(|server| {
                            if !server.enabled {
                                format!("{}: disabled", server.name)
                            } else if let Some(error) = &server.error {
                                format!("{}: error — {}", server.name, truncate(error, 60))
                            } else {
                                format!("{}: {} tool(s)", server.name, server.tools)
                            }
                        })
                        .collect();
                    self.status = parts.join(" · ");
                }
            }
            Event::ConfigReloaded { paths } => {
                self.status = format!("config reloaded ({} layer(s))", paths.len());
            }
            Event::Archived(session) => {
                let id = session.id.clone();
                self.sessions.retain(|existing| existing.id != id);
                self.views.retain(|view| view.session.id != id);
                if self.focused >= self.views.len() {
                    self.focused = self.views.len().saturating_sub(1);
                }
                self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
                self.status = format!("archived {}", session.name);
            }
            Event::Deleted(id) => {
                self.sessions.retain(|existing| existing.id != id);
                self.views.retain(|view| view.session.id != id);
                if self.focused >= self.views.len() {
                    self.focused = self.views.len().saturating_sub(1);
                }
                self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
                self.status = "session deleted".into();
            }
            Event::ShellRan { ok, output } => {
                let first = output.lines().next().unwrap_or_default();
                self.status = if first.is_empty() {
                    if ok {
                        "shell ok".into()
                    } else {
                        "shell failed".into()
                    }
                } else {
                    format!(
                        "shell {} · {}",
                        if ok { "ok" } else { "failed" },
                        truncate(first, 80)
                    )
                };
            }
            Event::Todos(items) => {
                if items.is_empty() {
                    self.status = "no tasks".into();
                } else {
                    let done = items
                        .iter()
                        .filter(|item| item.status == "completed")
                        .count();
                    let active = items
                        .iter()
                        .find(|item| item.status == "in_progress")
                        .or_else(|| items.iter().find(|item| item.status == "pending"));
                    self.status = match active {
                        Some(item) => format!(
                            "todos {done}/{} · in progress: {}",
                            items.len(),
                            item.content
                        ),
                        None => format!("todos {done}/{} · all done", items.len()),
                    };
                }
            }
            Event::Rewritten(session_id) => {
                if let Some(index) = self
                    .views
                    .iter()
                    .position(|view| view.session.id == session_id)
                {
                    self.pending_attach = Some(PendingAttach {
                        session_id: session_id.clone(),
                        slot: AttachSlot::Existing(index),
                    });
                    let _ = tx.send(Command::Attach(session_id));
                    self.status = "history updated".into();
                }
            }
            Event::Timeline(items) => {
                self.status = format!("timeline: {} item(s)", items.len());
                self.timeline_scroll = 0;
                self.timeline = Some(items);
            }
            Event::Draft { session_id, text } => {
                if self.input_session.as_deref() != Some(session_id.as_str()) {
                    return;
                }
                // The prompt was cleared on switch; load the daemon draft, but
                // never clobber text typed while the request was in flight.
                if self.input.is_empty() {
                    if !text.is_empty() {
                        self.input.set_text(&text);
                    }
                    self.last_saved_draft = self.input.text();
                }
                self.draft_loading = None;
            }
            Event::PatchEdit {
                session_id,
                patch_id,
                text,
            } => self.edit_patch(&session_id, &patch_id, &text, tx),
            Event::Submitted(submitted) => self.submitted(submitted),
            Event::Updated(session) => {
                let id = session.id.clone();
                let previous = self
                    .views
                    .iter()
                    .find(|view| view.session.id == id)
                    .map(|view| (view.session.cwd.clone(), view.session.permission));
                if let Some(view) = self.views.iter_mut().find(|view| view.session.id == id) {
                    view.session = session.clone();
                }
                if let Some((cwd, permission)) = previous {
                    if cwd != session.cwd {
                        self.status = format!("cwd: {}", session.cwd.display());
                    } else if permission != session.permission {
                        self.status = format!("permission: {}", session.permission.as_str());
                    }
                }
                self.upsert_session(session, false);
                self.error = None;
            }
            Event::ContextCleared { session_id } => {
                if let Some(view) = self
                    .views
                    .iter_mut()
                    .find(|view| view.session.id == session_id)
                {
                    view.stream = None;
                }
                self.status = "context cleared · earlier turns stay in the transcript".into();
                self.error = None;
            }
            Event::Compacted {
                session_id,
                entries,
                tokens,
            } => {
                if let Some(view) = self
                    .views
                    .iter_mut()
                    .find(|view| view.session.id == session_id)
                {
                    view.follow = true;
                }
                self.status =
                    format!("compacted · context now {tokens} tokens ({entries} entries)");
                self.error = None;
            }
            Event::BtwStarted { session_id, btw_id } => {
                if let Some(btw) = self.btw.as_mut() {
                    if btw.session_id == session_id {
                        btw.btw_id = btw_id;
                    }
                }
            }
            Event::BtwCancelled { session_id } => {
                if self
                    .btw
                    .as_ref()
                    .is_some_and(|btw| btw.session_id == session_id)
                {
                    self.btw = None;
                    self.status = "side question cancelled".into();
                }
            }
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
            Event::InputResponded { accepted } => {
                if accepted {
                    self.status = "answer sent".into();
                } else {
                    self.status = "answer was stale; the daemon is still waiting".into();
                    if let Some(session_id) =
                        self.focused_view().map(|view| view.session.id.clone())
                    {
                        self.reload_input(&session_id);
                    }
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
                        self.submitting_session = None;
                        self.pending_prompt = None;
                        self.pending_images = std::mem::take(&mut self.inflight_images);
                        if let Some((session_id, text)) = self.queued_inflight.take() {
                            // The daemon still considered the turn busy (the
                            // completion notification and the turns map race).
                            // Put it back at the head and retry on the next tick.
                            self.queues.entry(session_id).or_default().push_front(text);
                        } else if let Some((session_id, draft)) = self.pending_draft.take() {
                            if message.contains("already running") {
                                // The local status was stale: queue the prompt
                                // instead of losing the "submit while busy"
                                // intent. The tick retries it when the daemon
                                // reports the session idle.
                                self.enqueue(session_id, &draft, tx);
                                self.error = None;
                            } else {
                                // The prompt was cleared when the request was
                                // sent; put the draft back so it can be edited
                                // or retried.
                                self.input.set_text(&draft);
                            }
                        }
                    }
                    "create" => self.creating = false,
                    "attach" => self.pending_attach = None,
                    "history" => {
                        for view in &mut self.views {
                            view.history_loading = false;
                            // Re-arm only after the user scrolls down again,
                            // so a persistent error cannot spam requests.
                            view.history_armed = false;
                        }
                    }
                    "connect" => {
                        self.connected = false;
                        self.initialized = true;
                    }
                    "list" if !self.initialized => self.initialized = true,
                    "draft" => self.draft_loading = None,
                    "btw" => {
                        if let Some(btw) = self.btw.as_mut() {
                            btw.error = Some(message.clone());
                            btw.done = true;
                        }
                    }
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
        self.selected = self.find_session(&session_id).unwrap_or(self.selected);
        self.error = None;
        self.status = "attached".into();
    }

    /// Merge a page of older stored events and keep the viewport anchored on
    /// the content it was showing before the prepend.
    fn history_page(&mut self, session_id: String, events: Vec<SessionEvent>, has_more: bool) {
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

    fn submitted(&mut self, submitted: SubmitResult) {
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
            view.session = submitted.session;
            view.stream = Some(StreamState::new(submitted.turn_id));
            view.folds.remove(&FoldId::Live);
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
                    let relevant_input = matches!(
                        event.kind,
                        EventKind::InputRequired | EventKind::InputAnswered
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
                    view.events_rev = view.events_rev.wrapping_add(1);
                    if relevant {
                        self.reload_pending(&id);
                    }
                    if relevant_input {
                        self.reload_input(&id);
                    }
                }
            }
            "message.delta" | "reasoning.delta" => {
                let reasoning = notification.method == "reasoning.delta";
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
                            let target = if reasoning {
                                &mut stream.reasoning
                            } else {
                                &mut stream.text
                            };
                            target.push_str(&delta.delta);
                        }
                        Some(_) => {
                            // Deltas from a superseded turn; ignore them.
                        }
                        None => {
                            let mut stream = StreamState::new(delta.turn_id);
                            if reasoning {
                                stream.reasoning.push_str(&delta.delta);
                            } else {
                                stream.text.push_str(&delta.delta);
                            }
                            view.stream = Some(stream);
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
                    if self
                        .views
                        .get(self.focused)
                        .is_some_and(|view| view.session.id == params.session_id)
                    {
                        // The turn is blocked on the card; let j/k/Enter drive
                        // it without first leaving the prompt.
                        self.focus = Focus::Transcript;
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
                            cursor: 0,
                        },
                    );
                }
            }
            "input.required" => {
                if let Ok(params) = serde_json::from_value::<InputParams>(notification.params) {
                    if let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == params.session_id)
                    {
                        view.follow = true;
                    }
                    if self
                        .views
                        .get(self.focused)
                        .is_some_and(|view| view.session.id == params.session_id)
                    {
                        self.focus = Focus::Transcript;
                    }
                    self.status = format!("question: {}", truncate(&params.question, 60));
                    self.inputs.insert(
                        params.session_id,
                        PendingInput {
                            input_id: params.input_id,
                            call_id: params.call_id,
                            question: params.question,
                            options: params.options,
                            cursor: 0,
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
            "config.reloaded" => {
                self.status = "config reloaded".into();
            }
            "config.error" => {
                let error = notification.params["error"]
                    .as_str()
                    .unwrap_or("invalid config");
                self.error = Some(format!("config: {error}"));
                self.status = "config error".into();
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
                    if self.inputs.get(&params.session_id).is_some_and(|pending| {
                        params.call_id.is_empty() || pending.call_id == params.call_id
                    }) {
                        self.inputs.remove(&params.session_id);
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
                    // A question cannot outlive its turn.
                    self.inputs.remove(&done.session_id);
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
                        self.restore_pending_draft(&done.session_id);
                    } else if done.cancelled {
                        self.status = "cancelled".into();
                    } else {
                        self.status = turn_summary(&done);
                        self.clear_pending_draft(&done.session_id);
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
            "btw.delta" => {
                if let Ok(params) = serde_json::from_value::<BtwDeltaParams>(notification.params) {
                    if let Some(btw) = self.btw.as_mut() {
                        if btw.session_id == params.session_id
                            && (btw.btw_id.is_empty() || btw.btw_id == params.btw_id)
                        {
                            btw.text.push_str(&params.delta);
                        }
                    }
                }
            }
            "btw.completed" => {
                if let Ok(params) =
                    serde_json::from_value::<BtwCompletedParams>(notification.params)
                {
                    if let Some(btw) = self.btw.as_mut() {
                        if btw.session_id == params.session_id
                            && (btw.btw_id.is_empty() || btw.btw_id == params.btw_id)
                        {
                            if !params.text.is_empty() {
                                btw.text = params.text;
                            }
                            btw.done = true;
                            self.status = match params.error.as_deref() {
                                Some(error) => {
                                    btw.error = Some(error.to_string());
                                    "side question failed".into()
                                }
                                None => "side question answered · Esc to close".into(),
                            };
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
            // In the prompt Ctrl-C is the vim "interrupt": leave insert /
            // visual mode (or blur on a second press) instead of quitting while
            // the user is typing. Everywhere else it stays the global quit.
            let prompt_open = self.focus == Focus::Input
                && self.overlay_closed()
                && !self.help
                && self.reason_prompt.is_none();
            if prompt_open {
                let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
                self.handle_input_key(esc, tx);
            } else {
                self.sync_draft(tx);
                self.should_quit = true;
            }
            return;
        }
        // Errors float above everything else and keep the keyboard until
        // dismissed, so a failed action cannot silently disappear.
        if self.error.is_some() {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char('Q')
            ) {
                self.error = None;
            }
            return;
        }
        if matches!(key.code, KeyCode::F(1)) {
            self.open_help();
            return;
        }

        // The `/btw` answer floats above the prompt and owns the keyboard until
        // it is dismissed.
        if self.btw.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => {
                    let session_id = self.btw.as_ref().map(|btw| btw.session_id.clone());
                    self.btw = None;
                    if let Some(session_id) = session_id {
                        let _ = tx.send(Command::BtwCancel { session_id });
                    }
                    self.status = "side question closed".into();
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    if let Some(btw) = self.btw.as_mut() {
                        btw.scroll = btw.scroll.saturating_add(1);
                    }
                }
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(btw) = self.btw.as_mut() {
                        btw.scroll = btw.scroll.saturating_add(1);
                    }
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    if let Some(btw) = self.btw.as_mut() {
                        btw.scroll = btw.scroll.saturating_sub(1);
                    }
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if let Some(btw) = self.btw.as_mut() {
                        btw.scroll = btw.scroll.saturating_sub(1);
                    }
                }
                KeyCode::PageDown => {
                    if let Some(btw) = self.btw.as_mut() {
                        btw.scroll = btw.scroll.saturating_add(8);
                    }
                }
                KeyCode::PageUp => {
                    if let Some(btw) = self.btw.as_mut() {
                        btw.scroll = btw.scroll.saturating_sub(8);
                    }
                }
                _ => {}
            }
            return;
        }

        if self.timeline.is_some() {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.timeline = None,
                KeyCode::Char('j') | KeyCode::Down => self.timeline_scroll += 1,
                KeyCode::Char('k') | KeyCode::Up => {
                    self.timeline_scroll = self.timeline_scroll.saturating_sub(1)
                }
                KeyCode::PageDown => self.timeline_scroll += 10,
                KeyCode::PageUp => self.timeline_scroll = self.timeline_scroll.saturating_sub(10),
                _ => {}
            }
            if let Some(items) = &self.timeline {
                self.timeline_scroll = self.timeline_scroll.min(items.len().saturating_sub(1));
            }
            return;
        }

        if self.help {
            self.handle_help_key(key);
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
        // The prompt's editor owns its own leader; never leak a transcript
        // leader into typed input.
        self.pending_leader = false;
        // Cross-mode session picker. It is a TUI binding, so it works in every
        // editor mode (and does not consume a normal-mode letter).
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('g')) {
            self.open_picker(PickerKind::Sessions, tx);
            return;
        }

        // Normal-mode `p`/`P` can paste the desktop clipboard as well as the
        // editor's own register. Prefer the register when it already holds a
        // yank/delete so Vim editing keeps working; otherwise read the system
        // clipboard (e.g. text selected with the mouse in the transcript).
        if matches!(key.code, KeyCode::Char('p') | KeyCode::Char('P'))
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && self.input.normal_paste_ready()
            && !self.input.has_register_text()
        {
            if let Some(text) = super::read_clipboard() {
                let chars = text.chars().count();
                self.input
                    .paste_external(&text, matches!(key.code, KeyCode::Char('p')));
                self.status = format!("pasted {chars} chars");
                self.esc_at = None;
                return;
            }
        }

        // While typing a bare `/command`, the suggestion menu owns vertical
        // navigation, Enter and Tab. Other keys fall through to the editor so
        // the prefix keeps filtering the list.
        if self.command_menu_open() {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('n') | KeyCode::Char('j') => {
                        self.move_command_cursor(1);
                        return;
                    }
                    KeyCode::Char('p') | KeyCode::Char('k') => {
                        self.move_command_cursor(-1);
                        return;
                    }
                    _ => {}
                }
            }
            match key.code {
                KeyCode::Down => {
                    self.move_command_cursor(1);
                    return;
                }
                KeyCode::Up => {
                    self.move_command_cursor(-1);
                    return;
                }
                KeyCode::Tab => {
                    self.complete_command();
                    return;
                }
                KeyCode::Enter => {
                    if let Some(spec) = self.selected_command() {
                        let line = format!(
                            "/{}{}",
                            spec.name,
                            if spec.args.is_empty() { "" } else { " " }
                        );
                        self.input.set_text(&line);
                        self.input.enter_normal();
                        self.submit(tx);
                        return;
                    }
                }
                _ => {}
            }
        }

        // A single Esc returns to normal mode and keeps the prompt focused.
        // In normal mode, two Esc presses within the window interrupt a
        // running turn; when nothing is running they hand the keyboard back to
        // the transcript.
        if matches!(key.code, KeyCode::Esc) {
            let editing = self.input.mode() != Mode::Normal;
            let _ = self.input.handle_key(key);
            if editing {
                // Do not arm the double-Esc timer: after leaving insert the
                // user must still tap Esc twice to interrupt.
                self.esc_at = None;
                self.command_dismissed = true;
                return;
            }
            let now = Instant::now();
            let double = self
                .esc_at
                .is_some_and(|last| now.duration_since(last) < ESC_WINDOW);
            if double {
                self.esc_at = None;
                if self.focused_session_busy() {
                    self.cancel(tx);
                } else {
                    self.focus = Focus::Transcript;
                }
            } else {
                self.esc_at = Some(now);
            }
            self.command_dismissed = true;
            return;
        }

        self.esc_at = None;
        self.command_dismissed = false;
        match self.input.handle_key(key) {
            Outcome::Submit => self.submit(tx),
            Outcome::FilePicker => self.open_picker(PickerKind::Files, tx),
            Outcome::History(delta) => self.navigate_history(delta),
            Outcome::Action(id) if self.overlay_closed() => self.run_action(id, tx),
            Outcome::Ignored | Outcome::Action(_) => {}
        }
    }

    /// True when the prompt is a partial `/command` and the suggestion list
    /// should be visible.
    fn command_menu_open(&self) -> bool {
        if self.command_dismissed || self.input.mode() != Mode::Insert {
            return false;
        }
        let text = self.input.text();
        let Some(rest) = text.strip_prefix('/') else {
            return false;
        };
        !rest.contains(char::is_whitespace) && !rest.contains('\n')
    }

    fn command_matches(&self) -> Vec<&CommandSpec> {
        let text = self.input.text();
        let Some(prefix) = text.strip_prefix('/') else {
            return Vec::new();
        };
        let prefix = prefix.to_ascii_lowercase();
        self.commands
            .iter()
            .filter(|spec| spec.name.starts_with(&prefix))
            .collect()
    }

    fn selected_command(&self) -> Option<&CommandSpec> {
        let matches = self.command_matches();
        if matches.is_empty() {
            return None;
        }
        matches
            .get(self.command_cursor.min(matches.len() - 1))
            .copied()
    }

    fn move_command_cursor(&mut self, delta: isize) {
        let len = self.command_matches().len();
        if len == 0 {
            self.command_cursor = 0;
            return;
        }
        let current = self.command_cursor.min(len - 1) as isize;
        self.command_cursor = (current + delta).rem_euclid(len as isize) as usize;
    }

    fn complete_command(&mut self) {
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

    /// Apply a `delta` step to prompt-history browsing. Negative is older.
    fn navigate_history(&mut self, delta: isize) {
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

    /// Submitted prompts for the focused session, oldest first.
    fn history_entries(&self) -> Vec<String> {
        let Some(view) = self.focused_view() else {
            return Vec::new();
        };
        view.events
            .iter()
            .filter(|event| event.kind == EventKind::UserMessage)
            .filter_map(|event| {
                serde_json::from_value::<UserMessagePayload>(event.payload.clone())
                    .ok()
                    .map(|payload| payload.text)
            })
            .collect()
    }

    fn reset_history(&mut self) {
        self.history_index = None;
        self.history_draft.clear();
    }

    /// True while the focused turn can still be interrupted.
    fn focused_session_busy(&self) -> bool {
        self.submitting_session.is_some()
            || self
                .focused_view()
                .is_some_and(|view| view.session.status.is_busy())
    }

    fn focus_input(&mut self) {
        self.focus = Focus::Input;
        self.pending_leader = false;
        self.esc_at = None;
        self.input.enter_insert();
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
        if self.pending_leader {
            let available = self.focused_approval().is_none() && self.focused_input().is_none();
            self.pending_leader = false;
            if available {
                if let KeyCode::Char(ch) = key.code {
                    if let Some(action) = leader_action(ch) {
                        self.run_action(action, tx);
                    }
                }
            }
            return;
        }
        if self.focused_approval().is_none() && self.focused_input().is_none() {
            if key.code == KeyCode::Char(' ')
                && !key.modifiers.contains(KeyModifiers::CONTROL)
                && !key.modifiers.contains(KeyModifiers::ALT)
            {
                self.pending_leader = true;
                self.status = "leader: s/f/w/b · n new · o pane · C/q close · a commands · i input · Q quit · ? help · Esc cancel"
                    .into();
                return;
            }
            if let Some(action) = self.keybinds.action(key) {
                self.run_key_action(action, tx);
                return;
            }
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
        // While the focused session is blocked on an `ask_user` question, the
        // card owns j/k, Ctrl-N/Ctrl-P and Enter.
        if self.focused_input().is_some() {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('n') | KeyCode::Char('j') => {
                        self.move_input_cursor(1);
                        return;
                    }
                    KeyCode::Char('p') | KeyCode::Char('k') => {
                        self.move_input_cursor(-1);
                        return;
                    }
                    _ => {}
                }
            }
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.move_input_cursor(1);
                    return;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.move_input_cursor(-1);
                    return;
                }
                KeyCode::Enter => {
                    if let Some(cursor) = self.focused_input().map(|pending| pending.cursor) {
                        self.respond_input(cursor, tx);
                    }
                    return;
                }
                KeyCode::Char(ch @ '1'..='9') => {
                    self.respond_input(ch as usize - '1' as usize, tx);
                    return;
                }
                KeyCode::Char('x') => {
                    self.cancel(tx);
                    return;
                }
                KeyCode::Esc => return,
                _ => {}
            }
        }
        // While the focused session is blocked on a tool call, the card owns
        // j/k, Ctrl-N/Ctrl-P and Enter. y/n/d/e/o/r stay as direct shortcuts.
        if self.focused_approval().is_some() {
            if key.modifiers.contains(KeyModifiers::CONTROL) {
                match key.code {
                    KeyCode::Char('n') | KeyCode::Char('j') => {
                        self.move_approval_cursor(1);
                        return;
                    }
                    KeyCode::Char('p') | KeyCode::Char('k') => {
                        self.move_approval_cursor(-1);
                        return;
                    }
                    _ => {}
                }
            }
            match key.code {
                KeyCode::Char('j') | KeyCode::Down => {
                    self.move_approval_cursor(1);
                    return;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.move_approval_cursor(-1);
                    return;
                }
                KeyCode::Enter => {
                    self.activate_approval(None, tx);
                    return;
                }
                KeyCode::Char(ch @ '1'..='9') => {
                    self.activate_approval(Some(ch as usize - '1' as usize), tx);
                    return;
                }
                KeyCode::Char('y') => {
                    self.respond_approval(ApprovalDecision::Approve, tx);
                    return;
                }
                KeyCode::Char('a') => {
                    self.respond_approval(ApprovalDecision::Always, tx);
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
        // Pane actions are leader-only. Single letters are reserved for
        // navigation (j/k, g/G) and the prompt's own vim editing; the only
        // direct shortcuts left are shifted punctuation (`:`/`?`/`#`/`<`/`>`).
        match key.code {
            KeyCode::Char(':') => self.open_picker(PickerKind::Commands, tx),
            KeyCode::Char('?') => self.open_help(),
            KeyCode::Char('#') => {
                self.search_view = Some(SearchView::new());
                self.status =
                    "search history · Enter runs · ↑/↓ or C-j/C-k select · Esc closes".into();
            }
            KeyCode::Char('<') => self.cycle_pane(-1),
            KeyCode::Char('>') => self.cycle_pane(1),
            // `i` is the one bare letter that stays: focusing the prompt is
            // the normal way out of the transcript, like vim insert mode.
            KeyCode::Char('i') | KeyCode::Enter => self.focus_input(),
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
                    view.history_armed = true;
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

    fn run_key_action(&mut self, action: KeyAction, tx: &UnboundedSender<Command>) {
        match action {
            KeyAction::Quit => {
                self.sync_draft(tx);
                self.should_quit = true;
            }
            KeyAction::Help => self.open_help(),
            KeyAction::NewSession => self.create_session(tx, false),
            KeyAction::NewWorktree => self.create_session(tx, true),
            KeyAction::Sessions => self.open_picker(PickerKind::Sessions, tx),
            KeyAction::NewPane => self.open_picker(PickerKind::NewPane, tx),
            KeyAction::Palette => self.open_picker(PickerKind::Commands, tx),
            KeyAction::Worktrees => self.open_picker(PickerKind::Worktrees, tx),
            KeyAction::Branches => self.open_picker(PickerKind::Branches, tx),
            KeyAction::Ledger => self.open_ledger(tx),
            KeyAction::Processes => self.open_processes(tx),
            KeyAction::Inbox => self.open_inbox(),
            KeyAction::Search => {
                self.search_view = Some(SearchView::new());
                self.status =
                    "search history · Enter runs · ↑/↓ or C-j/C-k select · Esc closes".into();
            }
            KeyAction::Fork => self.fork_focused(tx, None),
            KeyAction::Attention => self.focus_next_attention(),
            KeyAction::Cancel => self.cancel(tx),
            KeyAction::NextSession => self.cycle_session(1, tx),
            KeyAction::PrevSession => self.cycle_session(-1, tx),
            KeyAction::ScrollDown => self.scroll_focused(1),
            KeyAction::ScrollUp => self.scroll_focused(-1),
            KeyAction::Timeline => {
                if let Some(session_id) = self.focused_session_id() {
                    let _ = tx.send(Command::TimelineData {
                        session_id,
                        limit: None,
                    });
                    self.status = "timeline: loading…".into();
                }
            }
            KeyAction::Todos => {
                if let Some(session_id) = self.focused_session_id() {
                    let _ = tx.send(Command::ListTodos { session_id });
                    self.status = "todos: loading…".into();
                }
            }
            KeyAction::Undo | KeyAction::Redo => {
                if let Some(session_id) = self.focused_session_id() {
                    let redo = action == KeyAction::Redo;
                    let _ = tx.send(Command::Undo { session_id, redo });
                    self.status = if redo { "redoing…" } else { "undoing…" }.into();
                }
            }
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
        let key = next_prev_key(key);
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
                KeyCode::Char('j') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    view.move_cursor(1);
                    None
                }
                KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    view.move_cursor(-1);
                    None
                }
                KeyCode::Char('n') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    view.move_cursor(1);
                    None
                }
                KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    view.move_cursor(-1);
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
            KeyCode::Char('#') if matches!(kind, PickerKind::Sessions | PickerKind::NewPane) => {
                self.picker = None;
                self.search_view = Some(SearchView::new());
                self.status =
                    "search history · Enter runs · ↑/↓ or C-j/C-k select · Esc closes".into();
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
        self.preview_theme_selection();
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
        self.preview_theme_selection();
    }

    /// Recolor the TUI for the theme under the picker cursor. The original
    /// palette is restored by Esc, so browsing never leaves a half-applied
    /// theme behind.
    fn preview_theme_selection(&mut self) {
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

    fn cancel_theme_picker(&mut self) {
        if let Some((config, theme)) = self.theme_picker_backup.take() {
            self.theme_config = config;
            self.theme = theme;
            self.status = "theme unchanged".into();
        }
        self.picker = None;
    }

    fn apply_theme_picker(&mut self, value: &str) {
        self.theme_picker_backup = None;
        self.theme_config.preset = (value != THEME_DEFAULT_VALUE).then(|| value.to_string());
        self.theme = Theme::from_config(&self.theme_config);
        self.status = if value == THEME_DEFAULT_VALUE {
            "theme → default".into()
        } else if let Some(preset) = theme_preset(value) {
            format!("theme → {} ({})", preset.name, preset.description)
        } else {
            format!("theme → {value}")
        };
        self.picker = None;
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
                    self.run_action(&value, tx);
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
            PickerKind::Skills => {
                if let Some(value) = value {
                    let cwd = self
                        .focused_view()
                        .map(|view| view.session.cwd.clone())
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                    if let Some(skill) = eix_core::find_skill(&cwd, &value) {
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
        }
    }

    /// Put `text` at the prompt cursor, using the embedded prompt when present.
    fn insert_prompt(&mut self, text: &str) {
        self.input.insert_text(text);
        self.focus = Focus::Input;
        self.status = format!("inserted {text}");
    }

    fn open_picker(&mut self, kind: PickerKind, tx: &UnboundedSender<Command>) {
        // A picker opening clears any half-finished double-Esc gesture.
        self.esc_at = None;
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
        } else if kind == PickerKind::Models {
            picker.loading = true;
            self.status = "models: loading…".into();
            let _ = tx.send(Command::ListModels);
        } else if kind == PickerKind::Agents {
            let Some(session_id) = self.focused_session_id() else {
                self.status = "attach a session first".into();
                return;
            };
            picker.loading = true;
            self.status = "agents: loading…".into();
            let _ = tx.send(Command::ListAgents { session_id });
        } else if kind == PickerKind::Skills {
            let cwd = self
                .focused_view()
                .map(|view| view.session.cwd.clone())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            picker.items = skill_picker_items(&eix_core::load_skills(&cwd));
            self.status = format!("{} skill(s)", picker.items.len());
        } else if kind == PickerKind::Themes {
            picker.items = theme_picker_items();
            let current = self.theme.preset.unwrap_or(THEME_DEFAULT_VALUE);
            picker.selected = picker
                .items
                .iter()
                .position(|item| item.value == current)
                .unwrap_or(0);
            self.theme_picker_backup = Some((self.theme_config.clone(), self.theme));
            self.status = "theme preview · Enter apply · Esc cancel".into();
        } else {
            picker.selected = self.selected.min(self.sessions.len().saturating_sub(1));
        }
        self.picker = Some(picker);
    }

    /// Run a command-palette entry or a prompt leader action (`<Space>xx`).
    /// Unknown ids are ignored.
    fn run_action(&mut self, id: &str, tx: &UnboundedSender<Command>) {
        match id {
            "commands" | "palette" => self.open_picker(PickerKind::Commands, tx),
            "sessions" => self.open_picker(PickerKind::Sessions, tx),
            "new-pane" => self.open_picker(PickerKind::NewPane, tx),
            "files" => self.open_picker(PickerKind::Files, tx),
            "worktrees" => self.open_picker(PickerKind::Worktrees, tx),
            "branches" => self.open_picker(PickerKind::Branches, tx),
            "models" => self.open_picker(PickerKind::Models, tx),
            "agents" => self.open_picker(PickerKind::Agents, tx),
            "toggle-agent" => self.toggle_agent_mode(tx),
            "skills" => self.open_picker(PickerKind::Skills, tx),
            "theme" => self.open_picker(PickerKind::Themes, tx),
            "context" => self.open_picker(PickerKind::Context, tx),
            "history" => {
                self.search_view = Some(SearchView::new());
                self.status =
                    "search history · Enter runs · ↑/↓ or C-j/C-k select · Esc closes".into();
            }
            "processes" => self.open_processes(tx),
            "approvals" => self.open_inbox(),
            "ledger" => self.open_ledger(tx),
            "new" => self.create_session(tx, false),
            "new-worktree" => self.create_session(tx, true),
            "fork" => self.fork_focused(tx, None),
            "cancel" => self.cancel(tx),
            "attention" => self.focus_next_attention(),
            "input" => self.focus_input(),
            "close-pane" => self.close_focused_pane(),
            "pane-prev" => self.cycle_pane(-1),
            "pane-next" => self.cycle_pane(1),
            "help" => self.open_help(),
            "quit" => {
                self.sync_draft(tx);
                self.should_quit = true;
            }
            _ => {}
        }
    }

    /// Flip the focused session between the built-in `build` and `plan`
    /// agents. `None` means the default build behavior; `Some("plan")` selects
    /// the read-only planning agent.
    fn toggle_agent_mode(&mut self, tx: &UnboundedSender<Command>) {
        let Some(view) = self.focused_view_mut() else {
            self.status = "attach a session first".into();
            return;
        };
        let session_id = view.session.id.clone();
        let next = if view.session.agent.as_deref() == Some("plan") {
            None
        } else {
            Some("plan".to_string())
        };
        // Update locally so a quick second Tab flips back before the daemon
        // round-trip lands; the session event reconciles the final state.
        view.session.agent = next.clone();
        let _ = tx.send(Command::SetAgent {
            session_id,
            agent: next.clone(),
        });
        self.status = match next.as_deref() {
            Some("plan") => "Plan".into(),
            _ => "Build".into(),
        };
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
        self.create_session_in(tx, worktree, None, None);
    }

    /// Create a session and open it in a new pane. `cwd`/`name` default to the
    /// TUI's launch directory / a generated name.
    fn create_session_in(
        &mut self,
        tx: &UnboundedSender<Command>,
        worktree: bool,
        cwd: Option<std::path::PathBuf>,
        name: Option<String>,
    ) {
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
        let _ = tx.send(Command::Create {
            worktree,
            cwd,
            name,
            model: self.default_model.clone(),
            agent: self.default_agent.clone(),
            permission: self.default_permission.clone(),
        });
    }

    fn submit(&mut self, tx: &UnboundedSender<Command>) {
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
    fn send_prompt(
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
    fn enqueue(&mut self, session_id: String, text: &str, tx: &UnboundedSender<Command>) {
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
    fn flush_queues(&mut self, tx: &UnboundedSender<Command>) {
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

    fn queue_len(&self, session_id: &str) -> usize {
        self.queues.get(session_id).map(VecDeque::len).unwrap_or(0)
    }

    fn session_busy(&self, session_id: &str) -> bool {
        self.submitting_session.as_deref() == Some(session_id)
            || self
                .views
                .iter()
                .find(|view| view.session.id == session_id)
                .is_some_and(|view| view.session.status.is_busy())
    }

    fn focused_session_id(&self) -> Option<String> {
        self.focused_view().map(|view| view.session.id.clone())
    }

    /// Prompt commands are typed as the whole message (`/name args`). They never
    /// reach the model.
    fn handle_prompt_command(&mut self, text: &str, tx: &UnboundedSender<Command>) -> bool {
        let trimmed = text.trim();
        let (name, args) = match trimmed.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (trimmed, ""),
        };
        let name = name.trim_start_matches('/');
        if name.is_empty() {
            return false;
        }
        let Some(spec) = self.commands.iter().find(|spec| spec.name == name).cloned() else {
            self.error = Some(format!("unknown command /{name} · /help lists them"));
            return true;
        };
        // Commands are not drafts: drop the typed line (and its saved draft)
        // before dispatching. `/init` installs its own prompt afterwards.
        self.input.clear();
        self.last_saved_draft.clear();
        self.reset_history();
        // `/exit`, `/quit` and `/close` save the (now empty) draft themselves.
        if !matches!(name, "exit" | "quit" | "close") {
            if let Some(session_id) = self.focused_session_id() {
                let _ = tx.send(Command::DraftSet {
                    session_id,
                    text: String::new(),
                });
            }
        }
        match name {
            "exit" | "quit" => {
                self.exit_session(tx);
                true
            }
            "close" => {
                self.close_session(tx);
                true
            }
            "help" => {
                self.open_help();
                true
            }
            "new" => {
                let cwd = self.focused_view().map(|view| view.session.cwd.clone());
                let name = (!args.is_empty()).then(|| args.to_string());
                self.create_session_in(tx, false, cwd, name);
                true
            }
            "cd" => {
                self.command_cd(args, tx);
                true
            }
            "clear" => {
                self.command_clear(tx);
                true
            }
            "compact" => {
                self.command_compact(args, tx);
                true
            }
            "btw" => {
                self.command_btw(args, tx);
                true
            }
            "init" => {
                self.command_init();
                true
            }
            "queue" => {
                self.command_queue(args);
                true
            }
            "sessions" => {
                self.open_picker(PickerKind::Sessions, tx);
                true
            }
            "models" => {
                if args.is_empty() {
                    self.open_picker(PickerKind::Models, tx);
                } else if let Some(session_id) = self.focused_session_id() {
                    let model = (args != "default").then(|| args.to_string());
                    let _ = tx.send(Command::SetModel { session_id, model });
                    self.status = format!("switching model to {args}…");
                }
                true
            }
            "agents" => {
                if args.is_empty() {
                    self.open_picker(PickerKind::Agents, tx);
                } else if let Some(session_id) = self.focused_session_id() {
                    let agent = (args != "default").then(|| args.to_string());
                    let _ = tx.send(Command::SetAgent { session_id, agent });
                    self.status = format!("switching agent to {args}…");
                }
                true
            }
            "mcp" => {
                let _ = tx.send(Command::ListMcp);
                self.status = "mcp: loading…".into();
                true
            }
            "reload" => {
                let _ = tx.send(Command::ReloadConfig);
                self.status = "reloading config…".into();
                true
            }
            "theme" => {
                if args.is_empty() {
                    self.open_picker(PickerKind::Themes, tx);
                } else {
                    self.error = Some("usage: /theme (opens the theme picker)".into());
                }
                true
            }
            "timeline" => {
                let limit = if args.is_empty() {
                    None
                } else {
                    match args.parse::<usize>() {
                        Ok(limit) => Some(limit),
                        Err(_) => {
                            self.error = Some("usage: /timeline [count]".into());
                            return true;
                        }
                    }
                };
                if let Some(session_id) = self.focused_session_id() {
                    let _ = tx.send(Command::TimelineData { session_id, limit });
                    self.status = "timeline: loading…".into();
                }
                true
            }
            "archive" => {
                if let Some(session_id) = self.focused_session_id() {
                    let _ = tx.send(Command::ArchiveSession {
                        session_id,
                        archived: true,
                    });
                    self.status = "archiving…".into();
                }
                true
            }
            "delete" => {
                let Some(session_id) = self.focused_session_id() else {
                    self.status = "no session yet".into();
                    return true;
                };
                if self.pending_delete.as_deref() == Some(session_id.as_str()) {
                    self.pending_delete = None;
                    let _ = tx.send(Command::DeleteSession { session_id });
                    self.status = "deleting…".into();
                } else {
                    self.pending_delete = Some(session_id);
                    self.status = "run /delete again to delete this session permanently".into();
                }
                true
            }
            "image" => {
                if args == "clear" {
                    self.pending_images.clear();
                    self.status = "images cleared".into();
                } else if args.is_empty() {
                    self.status = if self.pending_images.is_empty() {
                        "no images attached".into()
                    } else {
                        format!("attached: {}", self.pending_images.join(", "))
                    };
                } else {
                    let cwd = self
                        .focused_view()
                        .map(|view| view.session.cwd.clone())
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| ".".into()));
                    let paths = parse_image_paths(args, &cwd)
                        .unwrap_or_else(|| args.split_whitespace().map(str::to_string).collect());
                    for path in paths {
                        if !self.pending_images.contains(&path) {
                            self.pending_images.push(path);
                        }
                    }
                    self.status = format!(
                        "attached {} · {} image(s) for the next prompt",
                        args,
                        self.pending_images.len()
                    );
                }
                true
            }
            "todos" => {
                if let Some(session_id) = self.focused_session_id() {
                    let _ = tx.send(Command::ListTodos { session_id });
                    self.status = "todos: loading…".into();
                }
                true
            }
            "skills" => {
                let cwd = self
                    .focused_view()
                    .map(|view| view.session.cwd.clone())
                    .unwrap_or_else(|| {
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
                    });
                if args.is_empty() {
                    self.open_picker(PickerKind::Skills, tx);
                } else {
                    let (name, arguments) = args
                        .split_once(char::is_whitespace)
                        .map(|(name, rest)| (name, rest.trim()))
                        .unwrap_or((args, ""));
                    if let Some(skill) = eix_core::find_skill(&cwd, name) {
                        self.input
                            .set_text(&eix_core::expand_command(&skill.body, arguments));
                        self.focus = Focus::Input;
                        self.status = format!("inserted skill {}", skill.name);
                    } else {
                        self.error = Some(format!("unknown skill {name:?}"));
                    }
                }
                true
            }
            "undo" | "redo" => {
                if let Some(session_id) = self.focused_session_id() {
                    let redo = name == "redo";
                    let _ = tx.send(Command::Undo { session_id, redo });
                    self.status = if redo { "redoing…" } else { "undoing…" }.into();
                }
                true
            }
            "permission" => {
                self.command_permission(args, tx);
                true
            }
            "fork" => {
                let name = (!args.is_empty()).then(|| args.to_string());
                if let Some(session_id) = self.focused_session_id() {
                    let _ = tx.send(Command::Fork {
                        session_id,
                        at_seq: None,
                        name,
                    });
                    self.status = "forking…".into();
                }
                true
            }
            "commands" => {
                let cwd = self
                    .focused_view()
                    .map(|view| view.session.cwd.clone())
                    .unwrap_or_else(|| {
                        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
                    });
                self.commands = merge_commands(&cwd);
                self.status = format!(
                    "{} commands ({} user-defined)",
                    self.commands.len(),
                    self.commands
                        .iter()
                        .filter(|spec| spec.template.is_some())
                        .count()
                );
                true
            }
            _ => {
                if let Some(template) = spec.template {
                    let expanded = eix_core::expand_command(&template, args);
                    if expanded.trim().is_empty() {
                        self.error = Some(format!("/{name} produced an empty prompt"));
                        return true;
                    }
                    let Some(session_id) = self.focused_session_id() else {
                        self.status = "no session yet".into();
                        return true;
                    };
                    if let Some(model) = spec.model {
                        let _ = tx.send(Command::SetModel {
                            session_id: session_id.clone(),
                            model: Some(model),
                        });
                    }
                    if let Some(agent) = spec.agent {
                        let _ = tx.send(Command::SetAgent {
                            session_id: session_id.clone(),
                            agent: Some(agent),
                        });
                    }
                    if self.session_busy(&session_id) {
                        self.enqueue(session_id, &expanded, tx);
                    } else {
                        self.send_prompt(&session_id, &expanded, tx, false, Vec::new());
                    }
                    true
                } else {
                    self.error = Some(format!("unknown command /{name} · /help lists them"));
                    true
                }
            }
        }
    }
    fn command_cd(&mut self, args: &str, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_session_id() else {
            self.status = "no session".into();
            return;
        };
        if args.is_empty() {
            let cwd = self
                .focused_view()
                .map(|view| view.session.cwd.display().to_string())
                .unwrap_or_default();
            self.status = format!("cwd: {cwd}");
            return;
        }
        self.status = format!("changing directory to {args}…");
        let _ = tx.send(Command::SetCwd {
            session_id,
            path: args.to_string(),
        });
    }

    fn command_clear(&mut self, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_session_id() else {
            self.status = "no session".into();
            return;
        };
        if self.session_busy(&session_id) {
            self.error = Some("cancel the running turn before /clear".into());
            return;
        }
        self.status = "clearing context…".into();
        let _ = tx.send(Command::ClearContext { session_id });
    }

    fn command_compact(&mut self, args: &str, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_session_id() else {
            self.status = "no session".into();
            return;
        };
        if self.session_busy(&session_id) {
            self.error = Some("cancel the running turn before /compact".into());
            return;
        }
        let notes = (!args.is_empty()).then(|| args.to_string());
        self.status = "summarizing the conversation…".into();
        let _ = tx.send(Command::CompactContext { session_id, notes });
    }

    fn command_btw(&mut self, question: &str, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_session_id() else {
            self.status = "no session".into();
            return;
        };
        if question.is_empty() {
            self.status = "usage: /btw <question>".into();
            return;
        }
        self.status = "asking a side question…".into();
        self.btw = Some(BtwView {
            session_id: session_id.clone(),
            btw_id: String::new(),
            question: question.to_string(),
            text: String::new(),
            error: None,
            done: false,
            scroll: 0,
        });
        let _ = tx.send(Command::Btw {
            session_id,
            question: question.to_string(),
        });
    }

    /// `/init` stages a prompt that asks the model to write `AGENTS.md`. The
    /// user reviews it in insert mode before sending.
    fn command_init(&mut self) {
        let prompt = "Explore this repository and create a concise AGENTS.md at its root. \
Cover: the project's purpose, the workspace layout, the build/test/lint commands that \
actually work here, code style conventions, and any non-obvious constraints future \
contributors must respect. Read the existing docs and config first. If an AGENTS.md \
already exists, update it in place rather than rewriting it from scratch.";
        self.input.set_text(prompt);
        self.focus = Focus::Input;
        self.reset_history();
        self.status = "review the /init prompt, then press Enter to run it".into();
    }

    fn command_queue(&mut self, args: &str) {
        let Some(session_id) = self.focused_session_id() else {
            self.status = "no session".into();
            return;
        };
        let mut parts = args.split_whitespace();
        match parts.next() {
            Some("clear") => {
                let dropped = self
                    .queues
                    .remove(&session_id)
                    .map_or(0, |queue| queue.len());
                self.status = format!("cleared {dropped} queued prompt(s)");
                return;
            }
            Some("rm" | "drop") => {
                let index = match parts.next().and_then(|value| value.parse::<usize>().ok()) {
                    Some(index) if index > 0 => index - 1,
                    _ => {
                        self.error = Some("usage: /queue rm <1-based index>".into());
                        return;
                    }
                };
                let removed = self
                    .queues
                    .get_mut(&session_id)
                    .and_then(|queue| (index < queue.len()).then(|| queue.remove(index)).flatten());
                self.status = match removed {
                    Some(prompt) => format!("dropped queued prompt: {}", truncate(&prompt, 60)),
                    None => "no queued prompt at that index".into(),
                };
                return;
            }
            Some("move") => {
                let from = parts.next().and_then(|value| value.parse::<usize>().ok());
                let to = parts.next().and_then(|value| value.parse::<usize>().ok());
                let (Some(from), Some(to)) = (from, to) else {
                    self.error = Some("usage: /queue move <from> <to>".into());
                    return;
                };
                if from == 0 || to == 0 {
                    self.error = Some("queue indices are 1-based".into());
                    return;
                }
                let moved = self.queues.get_mut(&session_id).and_then(|queue| {
                    if from > queue.len() || to > queue.len() {
                        return None;
                    }
                    let prompt = queue.remove(from - 1)?;
                    queue.insert(to - 1, prompt);
                    Some(())
                });
                self.status = if moved.is_some() {
                    format!("moved queued prompt {from} → {to}")
                } else {
                    "queue index out of range".into()
                };
                return;
            }
            _ => {}
        }
        let Some(queue) = self.queues.get(&session_id) else {
            self.status = "no queued prompts".into();
            return;
        };
        if queue.is_empty() {
            self.status = "no queued prompts".into();
            return;
        }
        let preview: Vec<String> = queue
            .iter()
            .take(3)
            .enumerate()
            .map(|(index, prompt)| format!("{}: {}", index + 1, truncate(prompt, 40)))
            .collect();
        self.status = format!(
            "{} queued · {} · /queue rm N or /queue move N M",
            queue.len(),
            preview.join(" · ")
        );
    }

    fn command_permission(&mut self, args: &str, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_session_id() else {
            self.status = "no session".into();
            return;
        };
        if args.is_empty() {
            let level = self
                .focused_view()
                .map(|view| view.session.permission.as_str())
                .unwrap_or("?");
            self.status = format!("permission: {level}");
            return;
        }
        match args.parse::<PermissionLevel>() {
            Ok(permission) => {
                self.status = format!("permission → {}", permission.as_str());
                let _ = tx.send(Command::SetPermission {
                    session_id,
                    permission,
                    temporary: false,
                });
            }
            Err(()) => {
                self.error = Some("usage: /permission read-only | workspace-write | full".into());
            }
        }
    }

    /// Persist the prompt, detach the focused session and, when it was the last
    /// open pane, quit the TUI. Session history is already durable in the
    /// daemon; the only state to save here is the (cleared) draft.
    fn exit_session(&mut self, tx: &UnboundedSender<Command>) {
        self.detach_focused_session(tx, true)
    }

    /// Close the focused pane without quitting when it is the last one.
    fn close_session(&mut self, tx: &UnboundedSender<Command>) {
        self.detach_focused_session(tx, false)
    }

    fn detach_focused_session(&mut self, tx: &UnboundedSender<Command>, quit_when_last: bool) {
        let Some(view) = self.focused_view() else {
            self.should_quit = true;
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
            if quit_when_last {
                self.status = format!("saved {name} · quitting");
                self.should_quit = true;
            } else {
                self.status = "cannot close the last pane · use /quit to exit".into();
            }
            return;
        }
        self.close_focused_pane();
        self.status = format!("{name} saved · pane closed");
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
    }

    /// Recompute the cached question for a session from its replayable log.
    fn reload_input(&mut self, session_id: &str) {
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
        // Keep the highlighted option when the same question is reloaded.
        if let Some(cursor) = self
            .inputs
            .get(session_id)
            .filter(|pending| pending.input_id == next.input_id)
            .map(|pending| pending.cursor)
        {
            next.cursor = cursor.min(next.options.len().saturating_sub(1));
        }
        self.inputs.insert(session_id.to_string(), next);
    }

    fn focused_input(&self) -> Option<&PendingInput> {
        let view = self.focused_view()?;
        self.inputs.get(&view.session.id)
    }

    /// Move the highlight in the focused question's option list.
    fn move_input_cursor(&mut self, delta: isize) {
        let Some(session_id) = self.focused_view().map(|view| view.session.id.clone()) else {
            return;
        };
        let Some(pending) = self.inputs.get_mut(&session_id) else {
            return;
        };
        let len = pending.options.len();
        if len == 0 {
            return;
        }
        pending.cursor = (pending.cursor as isize + delta).clamp(0, len as isize - 1) as usize;
    }

    /// Answer the focused question with `option_index`.
    fn respond_input(&mut self, option_index: usize, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_view().map(|view| view.session.id.clone()) else {
            return;
        };
        let Some(pending) = self.inputs.get(&session_id) else {
            return;
        };
        let Some(option) = pending.options.get(option_index) else {
            return;
        };
        let input_id = pending.input_id.clone();
        self.status = format!("answered: {}", truncate(&option.label, 60));
        let _ = tx.send(Command::Input {
            session_id,
            input_id,
            option_index,
        });
    }

    /// Move the highlight in the focused approval card's action list.
    fn move_approval_cursor(&mut self, delta: isize) {
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
    fn activate_approval(&mut self, index: Option<usize>, tx: &UnboundedSender<Command>) {
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
            width: 0,
        });
        self.status = if has_hunks {
            "diff review: j/k scroll · [ ] hunk · Space toggle · y apply · n deny".into()
        } else {
            "diff review: j/k scroll · y approve · n deny · Esc close".into()
        };
    }

    fn handle_diff_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
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
        let Some((total, offsets)) = self.approvals.get(&session_id).map(|approval| {
            let rows = super::diff::rows(&approval.hunks, approval.diff.as_deref());
            if super::diff::DiffRenderer::can_render(&rows, width, "") {
                (rows.len(), super::diff::hunk_offsets(&approval.hunks))
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
            // Scrolling up is an explicit request for older content, so allow
            // another page even if the previous page added very few lines.
            view.history_armed = true;
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

        // Grow with the draft up to a small cap so long command lines are
        // readable; beyond that the editor scrolls instead of taking the pane.
        let input_height = prompt_input_height(area.height, area.width, &self.input);
        let chunks =
            Layout::vertical([Constraint::Min(3), Constraint::Length(input_height)]).split(area);

        self.draw_body(frame, chunks[0]);
        self.draw_input(frame, chunks[1]);
        if self.command_menu_open() {
            self.draw_command_menu(frame, chunks[1]);
        }
        if self.btw.is_some() {
            self.draw_btw(frame, chunks[0]);
        }

        if self.picker.is_some() {
            self.draw_picker(frame, area);
        }
        if self.help {
            draw_help(
                frame,
                area,
                &self.help_query,
                self.help_selected,
                self.theme,
            );
        }
        if let Some(items) = &self.timeline {
            draw_timeline(frame, area, items, self.timeline_scroll, self.theme);
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
        if self.error.is_some() {
            self.draw_error_dialog(frame, area);
        }
    }

    /// Glyph for the prompt header. Idle and running are icon-only so the
    /// header stays compact; other states keep their symbol and label.
    fn status_glyph(&self, status: SessionStatus) -> String {
        match status {
            SessionStatus::Idle => status.symbol().to_string(),
            SessionStatus::Running => {
                RUNNING_SPINNER[(self.tick as usize) % RUNNING_SPINNER.len()].to_string()
            }
            _ => format!("{} {}", status.symbol(), status.as_str()),
        }
    }

    /// Prompt header: current agent mode, session status and model. The old
    /// `eix` brand chip and the footer statusline are intentionally gone so the
    /// line above the input stays quiet.
    fn header_spans(&self) -> Vec<Span<'static>> {
        let session = self
            .focused_view()
            .map(|view| &view.session)
            .or_else(|| self.sessions.get(self.selected));
        let panel = composer_background(self.theme);
        let muted = composer_muted(self.theme);
        let accent = composer_accent(self.theme);
        let warn = composer_warn(self.theme);
        let plan = session.is_some_and(|session| session.agent.as_deref() == Some("plan"));
        let mode = if plan { "Plan" } else { "Build" };
        let mode_color = if plan { warn } else { accent };
        let mut spans = vec![
            // Keep Build/Plan the same cell width so the status and model do
            // not shift when the mode changes.
            Span::styled(
                format!(" {mode:<5}"),
                Style::default()
                    .fg(mode_color)
                    .bg(panel)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" · ", Style::default().fg(muted).bg(panel)),
        ];
        if let Some(session) = session {
            let (status, status_color) = if self.connected {
                (
                    self.status_glyph(session.status),
                    composer_status_color(session.status, self.theme),
                )
            } else {
                ("offline".to_string(), composer_danger(self.theme))
            };
            spans.push(Span::styled(
                status,
                Style::default().fg(status_color).bg(panel),
            ));
            spans.push(Span::styled(" ", Style::default().bg(panel)));
            spans.push(Span::styled(
                short_model(self.model()).to_string(),
                Style::default().fg(accent).bg(panel),
            ));
            let queued = self.queue_len(&session.id);
            if queued > 0 {
                spans.push(Span::styled(
                    format!("  ⏳{queued}"),
                    Style::default().fg(warn).bg(panel),
                ));
            }
        } else if self.connected {
            spans.push(Span::styled(
                "no session",
                Style::default().fg(muted).bg(panel),
            ));
        } else {
            spans.push(Span::styled(
                "connecting…",
                Style::default().fg(muted).bg(panel),
            ));
        }
        let attention = self.attention_count();
        if attention > 0 {
            spans.push(Span::styled(
                format!("  ⚠{attention}"),
                Style::default()
                    .fg(warn)
                    .bg(panel)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        spans.push(Span::styled(" ", Style::default().bg(panel)));
        spans
    }

    /// Right-aligned half of the prompt banner.
    fn header_meta_spans(&self) -> Vec<Span<'static>> {
        let session = self
            .focused_view()
            .map(|view| &view.session)
            .or_else(|| self.sessions.get(self.selected));
        let panel = composer_background(self.theme);
        let muted = composer_muted(self.theme);
        let accent = composer_accent(self.theme);
        let warn = composer_warn(self.theme);
        let mut spans = Vec::new();
        if let Some(session) = session {
            spans.push(Span::styled(
                format!(" {} ", compact_path(&session.cwd.display().to_string())),
                Style::default().fg(muted).bg(panel),
            ));
            if self.git_cwd.as_ref() == Some(&session.cwd) {
                if let Some(branch) = &self.git_branch {
                    spans.push(Span::styled(
                        format!("{branch} "),
                        Style::default().fg(accent).bg(panel),
                    ));
                }
            }
            if let Some(worktree) = &session.worktree {
                let name = worktree
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "worktree".into());
                spans.push(Span::styled(
                    format!("wt:{name} "),
                    Style::default().fg(warn).bg(panel),
                ));
            }
        }
        if !self.pending_images.is_empty() {
            if !spans.is_empty() {
                spans.push(Span::styled(" ", Style::default().bg(panel)));
            }
            spans.push(Span::styled(
                format!(" {} img ", self.pending_images.len()),
                Style::default().fg(warn).bg(panel),
            ));
        }
        spans
    }

    fn header_meta_line(&self) -> Option<Line<'static>> {
        let spans = self.header_meta_spans();
        if spans.is_empty() {
            None
        } else {
            Some(Line::from(spans))
        }
    }

    fn draw_body(&mut self, frame: &mut Frame, area: Rect) {
        // Sessions live in the floating picker (`p` / `<Space>s`), not in a
        // permanent sidebar: panes get the whole body.
        self.draw_panes(frame, area);
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
        let pending_input = if focused {
            self.inputs.get(&session_id).cloned()
        } else {
            None
        };
        let attention = !focused
            && (self.views[index].session.status.needs_attention()
                || self.approvals.contains_key(&session_id)
                || self.inputs.contains_key(&session_id));
        let theme = self.theme;
        let borders = pane_borders(&self.pane_areas, index);
        let border_cols = u16::from(borders.contains(Borders::LEFT))
            + u16::from(borders.contains(Borders::RIGHT));
        let border_rows = u16::from(borders.contains(Borders::TOP))
            + u16::from(borders.contains(Borders::BOTTOM));
        let inner_width = area.width.saturating_sub(border_cols) as usize;
        let inner_height = area.height.saturating_sub(border_rows) as usize;
        // Stored history is rendered once and cached; only the live tail is
        // rebuilt on each frame. This keeps streaming cheap for long sessions.
        self.views[index].ensure_history(inner_width.max(1), theme);
        let queued: Vec<String> = self
            .queues
            .get(&session_id)
            .map(|queue| queue.iter().cloned().collect())
            .unwrap_or_default();
        let transcript::Transcript {
            lines: tail_lines,
            slots: tail_slots,
        } = {
            let view = &self.views[index];
            let live = view.stream.as_ref().map(|stream| transcript::Live {
                reasoning: &stream.reasoning,
                text: &stream.text,
            });
            transcript::render_tail(live, &queued, &view.folds, inner_width.max(1), theme)
        };
        let history_len = self.views[index]
            .history_cache
            .as_ref()
            .map(|cache| cache.lines.len())
            .unwrap_or(0);
        {
            let view = &mut self.views[index];
            if let Some((old_len, old_scroll)) = view.history_anchor.take() {
                if !view.follow {
                    // Prepending a page shifts every existing line down. Move
                    // the scroll and any selection by the same amount so the
                    // viewport content does not jump under the user.
                    let delta = history_len.saturating_sub(old_len);
                    view.scroll = old_scroll.saturating_add(delta);
                    if let Some(selection) = &mut view.selection {
                        selection.anchor.0 = selection.anchor.0.saturating_add(delta);
                        selection.cursor.0 = selection.cursor.0.saturating_add(delta);
                    }
                }
            }
        }
        {
            let view = &mut self.views[index];
            view.slots.truncate(history_len);
            view.slots.extend(tail_slots);
            view.text_lines.truncate(history_len);
            view.text_lines.extend(tail_lines.iter().map(text_line));
        }
        let total = history_len + tail_lines.len();
        let max_scroll = total.saturating_sub(inner_height);
        {
            let view = &mut self.views[index];
            if view.follow {
                view.scroll = max_scroll;
            } else {
                view.scroll = view.scroll.min(max_scroll);
                if view.scroll > HISTORY_REARM_LINES {
                    view.history_armed = true;
                }
            }
        }
        // Ask the event loop for the previous page when the viewport reaches
        // the top. Only the focused pane loads history; background panes stay
        // idle.
        let request = {
            let view = &self.views[index];
            if focused
                && !view.follow
                && view.history_armed
                && !view.history_loading
                && view.history_has_more
                && view.scroll <= HISTORY_PRELOAD_LINES
            {
                view.events
                    .first()
                    .map(|event| (session_id.clone(), event.seq))
            } else {
                None
            }
        };
        if let Some((id, before_seq)) = request {
            let view = &mut self.views[index];
            view.history_loading = true;
            view.history_armed = false;
            self.pending_history = Some((id, before_seq));
        }
        // Only hand the visible window to ratatui. Besides reducing layout
        // work, this keeps the paragraph independent of the full line vector.
        let scroll = self.views[index].scroll;
        let end = (scroll + inner_height).min(total);
        let mut lines: Vec<Line<'static>> = Vec::with_capacity(end.saturating_sub(scroll));
        {
            let view = &self.views[index];
            if let Some(cache) = view.history_cache.as_ref() {
                if scroll < history_len {
                    let history_end = end.min(history_len);
                    lines.extend_from_slice(&cache.lines[scroll..history_end]);
                }
            }
        }
        if end > history_len {
            let tail_start = scroll.saturating_sub(history_len);
            lines.extend_from_slice(&tail_lines[tail_start..end - history_len]);
        }
        if let Some(selection) = self.views[index].selection {
            highlight_selection_window(&mut lines, &selection, scroll);
        }
        let view = &self.views[index];
        let name = truncate(
            &view.session.name,
            inner_width.saturating_sub(6).clamp(1, 18),
        );
        let marker = if attention { "!" } else { "" };
        let permission = match view.session.permission {
            PermissionLevel::ReadOnly => " ro",
            _ => "",
        };
        let title = format!(
            " {}{}{marker} {}{permission} ",
            index + 1,
            view.session.status.symbol(),
            name
        );
        let border_color = if focused {
            composer_border(theme)
        } else if attention {
            composer_warn(theme)
        } else {
            Color::Rgb(48, 52, 62)
        };
        let mut border_style = Style::default().fg(border_color);
        let mut title_style = Style::default()
            .fg(border_color)
            .add_modifier(Modifier::DIM);
        let mut block = Block::default().borders(borders).title(title);
        if let Some(background) = theme.background {
            border_style = border_style.bg(background);
            title_style = title_style.bg(background);
            block = block.style(Style::default().bg(background));
        }
        block = block.border_style(border_style).title_style(title_style);
        let mut paragraph = Paragraph::new(lines).block(block);
        if let Some(background) = theme.background {
            paragraph = paragraph.style(Style::default().bg(background));
        }
        frame.render_widget(paragraph, area);

        if let Some(approval) = approval {
            draw_approval_card(frame, area, &approval, theme);
        }
        if let Some(pending) = pending_input {
            draw_input_card(frame, area, &pending, theme);
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
        let background = self.theme.background;
        let block = Block::default().borders(Borders::ALL).title(" transcript ");
        let block = if let Some(background) = background {
            block.style(Style::default().bg(background))
        } else {
            block
        };
        let mut paragraph = Paragraph::new(lines).block(block);
        if let Some(background) = background {
            paragraph = paragraph.style(Style::default().bg(background));
        }
        frame.render_widget(paragraph, area);
    }

    fn draw_input(&mut self, frame: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Input;
        let mode = self.input.mode();
        let panel = composer_background(self.theme);
        let text = composer_text(self.theme);
        let border_color = if focused {
            composer_border(self.theme)
        } else {
            Color::Rgb(46, 50, 60)
        };

        // The top border is the session banner; the bottom border carries only
        // the mode chip. Deny reasons temporarily replace the banner; errors
        // live in a floating dialog so the prompt chrome stays stable.
        let title_width = area.width.saturating_sub(2) as usize;
        let (top_left, top_right) = if let Some(prompt) = &self.reason_prompt {
            let line = Line::from(vec![
                Span::styled(
                    " deny reason: ",
                    Style::default()
                        .fg(composer_warn(self.theme))
                        .bg(panel)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("{}▏", prompt.text),
                    Style::default().fg(text).bg(panel),
                ),
                Span::styled(
                    "  Enter denies · Esc cancels",
                    Style::default().fg(composer_muted(self.theme)).bg(panel),
                ),
            ]);
            (truncate_line(line, title_width), None)
        } else {
            fit_title(
                Line::from(self.header_spans()),
                self.header_meta_line(),
                title_width,
            )
        };
        let bottom_left = Line::from(Span::styled(
            format!(" {} ", mode.short_label()),
            composer_mode_style(mode, focused, self.theme),
        ));

        let mut block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color).bg(panel))
            .style(Style::default().bg(panel))
            .title_style(Style::default().bg(panel))
            .padding(Padding::new(1, 1, 0, 0))
            .title_top(top_left)
            .title_bottom(bottom_left);
        if let Some(title) = top_right {
            block = block.title_top(title.right_aligned());
        }

        // One cell of left padding keeps the text close to the border; one
        // cell on the right mirrors it. The old `❯` gutter marker is gone.
        let inner_width = area.width.saturating_sub(4).max(1) as usize;
        let inner_height = area.height.saturating_sub(2).max(1) as usize;
        let layout = self.input.layout(inner_width, inner_height);
        let lines: Vec<Line> = layout
            .segments
            .iter()
            .map(|segment| self.input_line(segment))
            .collect();
        let paragraph = Paragraph::new(lines)
            .block(block)
            .style(Style::default().fg(text).bg(panel))
            .scroll((layout.offset.min(u16::MAX as usize) as u16, 0));
        frame.render_widget(paragraph, area);

        // The error dialog covers the prompt; don't leave a blinking input
        // cursor on top of it.
        if focused && self.error.is_none() {
            let x = area.x + 2 + layout.cursor_col.min(inner_width.saturating_sub(1)) as u16;
            let row = layout
                .cursor_line
                .saturating_sub(layout.offset)
                .min(inner_height.saturating_sub(1));
            let y = area.y + 1 + row as u16;
            frame.set_cursor_position((x, y));
        }
    }

    /// `/` suggestion menu floated just above the prompt.
    fn draw_command_menu(&mut self, frame: &mut Frame, input_area: Rect) {
        if input_area.y < 3 {
            return;
        }
        let matches = self.command_matches();
        if matches.is_empty() {
            return;
        }
        let height = (matches.len() as u16 + 2)
            .min(15)
            .min(input_area.y.saturating_sub(1));
        let visible = height.saturating_sub(2) as usize;
        if visible == 0 {
            return;
        }
        let cursor = self.command_cursor.min(matches.len() - 1);
        let start = cursor.saturating_sub(visible.saturating_sub(1));
        let width = input_area.width.min(88);
        let rect = Rect::new(input_area.x, input_area.y - height, width, height);
        frame.render_widget(Clear, rect);

        let panel = composer_background(self.theme);
        let muted = composer_muted(self.theme);
        let text = composer_text(self.theme);
        let accent = composer_accent(self.theme);
        let inner = width.saturating_sub(2) as usize;
        let marker_width = 3usize;
        let name_width = 22.min(inner.saturating_sub(marker_width + 2));
        let desc_width = inner
            .saturating_sub(marker_width + name_width)
            .saturating_sub(2);
        let mut lines = Vec::new();
        for (offset, spec) in matches.iter().skip(start).take(visible).enumerate() {
            let index = start + offset;
            let selected = index == cursor;
            let row_bg = if selected { COMPOSER_SELECTION } else { panel };
            let marker_style = if selected {
                Style::default()
                    .fg(accent)
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(muted).bg(row_bg)
            };
            let name_style = if selected {
                Style::default()
                    .fg(accent)
                    .bg(row_bg)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(text).bg(row_bg)
            };
            let desc_style = if selected {
                Style::default().fg(text).bg(row_bg)
            } else {
                Style::default().fg(muted).bg(row_bg)
            };
            let name = if spec.args.is_empty() {
                format!("/{}", spec.name)
            } else {
                format!("/{} {}", spec.name, spec.args)
            };
            let name = pad_to(&truncate(&name, name_width), name_width);
            let description = pad_to(&truncate(&spec.description, desc_width), desc_width);
            let gap = inner.saturating_sub(marker_width + name_width + desc_width);
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", if selected { "❯" } else { " " }),
                    marker_style,
                ),
                Span::styled(name, name_style),
                Span::styled(" ".repeat(gap), desc_style),
                Span::styled(description, desc_style),
            ]));
        }
        let title_left = Line::from(Span::styled(
            " / commands ",
            Style::default()
                .fg(accent)
                .bg(panel)
                .add_modifier(Modifier::BOLD),
        ));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(accent).bg(panel))
            .style(Style::default().bg(panel))
            .title_style(Style::default().bg(panel))
            .title_top(title_left);
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .style(Style::default().fg(text).bg(panel)),
            rect,
        );
    }

    /// Floating `/btw` answer. It owns the keyboard until dismissed.
    fn draw_btw(&mut self, frame: &mut Frame, area: Rect) {
        let width = (area.width as u32 * 3 / 4).max(46).min(area.width as u32) as u16;
        let height = (area.height as u32 * 3 / 5)
            .clamp(10, 20)
            .min(area.height as u32) as u16;
        let rect = centered(area, width, height);
        let inner_width = width.saturating_sub(4) as usize;
        let inner_height = height.saturating_sub(2) as usize;

        let dialog_bg = dialog_background(self.theme);
        let session_name = self
            .btw
            .as_ref()
            .and_then(|btw| {
                self.views
                    .iter()
                    .find(|view| view.session.id == btw.session_id)
                    .map(|view| view.session.name.clone())
            })
            .unwrap_or_else(|| "side question".into());
        let Some(btw) = self.btw.as_mut() else {
            return;
        };

        let mut lines = vec![Line::from(vec![
            Span::styled("  ? ", Style::default().fg(Color::Cyan)),
            Span::styled(
                btw.question.clone(),
                Style::default()
                    .fg(Color::Gray)
                    .add_modifier(Modifier::ITALIC),
            ),
        ])];
        if let Some(error) = &btw.error {
            for segment in wrap_text(error, inner_width.saturating_sub(2)) {
                lines.push(Line::from(Span::styled(
                    format!("  {segment}"),
                    Style::default().fg(Color::Red),
                )));
            }
        } else if btw.text.is_empty() {
            lines.push(Line::from(Span::styled(
                if btw.done {
                    "  (no answer)"
                } else {
                    "  thinking…"
                },
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for segment in wrap_text(&btw.text, inner_width.saturating_sub(2)) {
                lines.push(Line::from(format!("  {segment}")));
            }
        }
        let max_scroll = lines.len().saturating_sub(inner_height);
        btw.scroll = btw.scroll.min(max_scroll);
        let border = if btw.error.is_some() {
            Color::Red
        } else if btw.done {
            Color::Green
        } else {
            Color::Cyan
        };
        frame.render_widget(Clear, rect);
        frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .style(Style::default().bg(dialog_bg))
            .title(Span::styled(
                format!(" btw · {session_name} "),
                Style::default().fg(border).add_modifier(Modifier::BOLD),
            ))
            .title_bottom(Span::styled(
                " Esc close · j/k scroll ",
                Style::default().fg(Color::DarkGray),
            ));
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .style(Style::default().bg(dialog_bg))
                .scroll((btw.scroll.min(u16::MAX as usize) as u16, 0)),
            rect,
        );
    }

    /// Floating error report. It sits above every other overlay and is
    /// dismissed with Esc / Enter / q.
    fn draw_error_dialog(&self, frame: &mut Frame, area: Rect) {
        let Some(error) = self.error.as_deref() else {
            return;
        };
        if area.width < 20 || area.height < 5 {
            return;
        }
        let width = ((area.width as u32 * 3 / 5).max(36).min(area.width as u32)) as u16;
        let text_width = width.saturating_sub(4) as usize;
        let lines: Vec<Line> = wrap_text(error, text_width)
            .into_iter()
            .map(|line| Line::from(format!("  {line}")))
            .collect();
        let height = (lines.len() as u16 + 2).clamp(3, area.height.saturating_sub(2));
        let rect = centered(area, width, height);
        let background = dialog_background(self.theme);
        let danger = composer_danger(self.theme);
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Block::default().style(Style::default().bg(background)),
            rect,
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(danger).bg(background))
            .style(Style::default().bg(background))
            .title(Span::styled(
                " error ",
                Style::default()
                    .fg(danger)
                    .bg(background)
                    .add_modifier(Modifier::BOLD),
            ))
            .title_bottom(Span::styled(
                " Esc / Enter close ",
                Style::default()
                    .fg(composer_muted(self.theme))
                    .bg(background),
            ));
        frame.render_widget(
            Paragraph::new(lines).block(block).style(
                Style::default()
                    .fg(composer_text(self.theme))
                    .bg(background),
            ),
            rect,
        );
    }

    /// One prompt row, with the visual selection reversed.
    fn input_line(&self, segment: &super::editor::Segment) -> Line<'static> {
        if self.input.selection().is_none() {
            return Line::from(segment.text.clone());
        }
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut run = String::new();
        let mut run_selected = false;
        for (offset, ch) in segment.text.chars().enumerate() {
            let selected = self
                .input
                .is_selected(segment.row, segment.start_col + offset);
            if selected != run_selected && !run.is_empty() {
                spans.push(selection_span(std::mem::take(&mut run), run_selected));
            }
            run_selected = selected;
            run.push(ch);
        }
        if !run.is_empty() {
            spans.push(selection_span(run, run_selected));
        }
        Line::from(spans)
    }

    fn draw_diff_view(&mut self, frame: &mut Frame, area: Rect) {
        let theme = self.theme;
        let dialog_bg = dialog_background(theme);
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
        let path = approval.path.clone().unwrap_or_default();
        let rows = super::diff::rows(&approval.hunks, approval.diff.as_deref());

        let rect = centered(
            area,
            area.width.saturating_sub(4),
            area.height.saturating_sub(4),
        );
        frame.render_widget(Clear, rect);
        let inner_width = rect.width.saturating_sub(2) as usize;
        let inner_height = rect.height.saturating_sub(3) as usize; // borders + hint
        frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);

        let interactive = !approval.hunks.is_empty();
        let side_by_side = (!rows.is_empty())
            .then(|| {
                super::diff::DiffRenderer::new(&rows, inner_width, &path, theme, "", dialog_bg)
            })
            .flatten()
            .map(|mut renderer| renderer.render(&rows, interactive, &approval.selected, cursor));
        let body: Vec<Line> = match side_by_side {
            Some(body) => body,
            None => {
                // Too narrow for two readable code columns: keep the old
                // unified view as a fallback.
                let mut body = Vec::new();
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
                body
            }
        };

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

        let title = if path.is_empty() {
            format!(" diff · {tool} ")
        } else {
            format!(" diff · {tool} · {path} ")
        };
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(title);
        frame.render_widget(
            Paragraph::new(body)
                .block(block)
                .style(Style::default().bg(dialog_bg)),
            rect,
        );
        if let Some(view) = &mut self.diff_view {
            view.scroll = scroll;
            view.width = inner_width;
        }
    }

    fn draw_ledger(&mut self, frame: &mut Frame, area: Rect) {
        let dialog_bg = dialog_background(self.theme);
        let Some(ledger) = &mut self.ledger else {
            return;
        };
        let rect = centered(
            area,
            area.width.saturating_sub(4),
            area.height.saturating_sub(4),
        );
        frame.render_widget(Clear, rect);
        frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);
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
                .style(Style::default().bg(dialog_bg)),
            rect,
        );
    }

    fn draw_inbox(&mut self, frame: &mut Frame, area: Rect) {
        let dialog_bg = dialog_background(self.theme);
        let Some(inbox) = &self.inbox else {
            return;
        };
        let rows = (inbox.items.len() as u16 + 4).min(area.height.saturating_sub(2).max(5));
        let rect = centered(area, area.width.saturating_sub(6).max(30), rows);
        frame.render_widget(Clear, rect);
        frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);
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
                .style(Style::default().bg(dialog_bg)),
            rect,
        );
    }

    fn draw_processes(&mut self, frame: &mut Frame, area: Rect) {
        let dialog_bg = dialog_background(self.theme);
        let Some(view) = &mut self.process_view else {
            return;
        };
        let rect = centered(
            area,
            area.width.saturating_sub(4),
            area.height.saturating_sub(4),
        );
        frame.render_widget(Clear, rect);
        frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);
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
            .style(Style::default().bg(dialog_bg)),
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
            Paragraph::new(body).style(Style::default().bg(dialog_bg)),
            output_inner,
        );
    }

    fn draw_search(&mut self, frame: &mut Frame, area: Rect) {
        let dialog_bg = dialog_background(self.theme);
        let Some(view) = &self.search_view else {
            return;
        };
        let rect = centered(
            area,
            area.width.saturating_sub(4),
            area.height.saturating_sub(4),
        );
        frame.render_widget(Clear, rect);
        frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(format!(" search history · {} hit(s) ", view.hits.len()))
            .title_bottom(" Enter run/open · ↑/↓ or C-j/C-k select · Esc close ");
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
            .style(Style::default().bg(dialog_bg)),
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
        // The theme picker previews the highlighted palette immediately, so
        // its own chrome should follow that palette too (not a fixed cyan).
        let theme_picker = kind == PickerKind::Themes;
        let theme = self.theme;
        let accent = if theme_picker {
            composer_accent(theme)
        } else {
            Color::Cyan
        };
        let muted = if theme_picker {
            composer_muted(theme)
        } else {
            Color::DarkGray
        };
        let preview_text = if theme_picker {
            composer_text(theme)
        } else {
            Color::Gray
        };
        let dialog_bg = dialog_background(theme);

        let width = (area.width as u32 * 8 / 10).max(44).min(area.width as u32) as u16;
        let height = (area.height as u32 * 7 / 10)
            .max(12)
            .min(area.height as u32) as u16;
        let rect = centered(area, width, height);
        frame.render_widget(Clear, rect);
        let title = format!(" {} ({}) ", kind.title(), filtered.len());
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(accent).bg(dialog_bg))
            .title_style(Style::default().bg(dialog_bg))
            .style(Style::default().bg(dialog_bg))
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
                Span::styled("❯ ", Style::default().fg(accent)),
                Span::raw(query.clone()),
            ]))
            .style(Style::default().bg(dialog_bg)),
            chunks[0],
        );
        frame.render_widget(
            Paragraph::new("─".repeat(chunks[1].width as usize))
                .style(Style::default().fg(muted).bg(dialog_bg)),
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
                Paragraph::new(text).style(Style::default().fg(muted).bg(dialog_bg)),
                list_area,
            );
        } else {
            let rows = self.picker_rows(kind, &filtered);
            let list = List::new(rows)
                .style(Style::default().bg(dialog_bg))
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
                .style(Style::default().fg(preview_text).bg(dialog_bg))
                .block(
                    Block::default()
                        .borders(Borders::LEFT)
                        .border_style(Style::default().fg(muted).bg(dialog_bg)),
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
                                format!(
                                    "{}{}",
                                    origin_marker(session),
                                    truncate(&session.name, 28)
                                ),
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
                let theme_picker = kind == PickerKind::Themes;
                let label_style = if theme_picker {
                    Style::default()
                        .fg(composer_text(self.theme))
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().add_modifier(Modifier::BOLD)
                };
                let detail_color = if theme_picker {
                    composer_muted(self.theme)
                } else {
                    Color::DarkGray
                };
                filtered
                    .iter()
                    .filter_map(|&index| picker.items.get(index))
                    .map(|item| {
                        let mut spans = vec![Span::styled(item.label.clone(), label_style)];
                        if let Some(detail) = &item.detail {
                            if !detail.is_empty() {
                                spans.push(Span::styled(
                                    format!("  {detail}"),
                                    Style::default().fg(detail_color),
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

/// Rows for the interactive `/theme` picker. The first row clears the preset;
/// the rest are the built-in palettes in `THEME_PRESETS` order.
fn theme_picker_items() -> Vec<PickerItem> {
    let mut items = vec![PickerItem::new(THEME_DEFAULT_VALUE)
        .with_label("default")
        .with_detail("transparent · terminal palette")
        .with_preview(
            "default\n\nUse the terminal background and palette. The prompt has no separate fill; explicit [theme] slot overrides still apply.",
        )];
    items.extend(THEME_PRESETS.iter().map(|preset| {
        let preview = format!(
            "{}\n\n{}\n\nnormal      {}\ncomment     {}\nstring      {}\nfunction    {}\nerror       {}\ndiff_add    {}\ndiff_delete {}\npanel       {}\nbackground  {}",
            preset.name,
            preset.description,
            theme_slot(preset.normal),
            theme_slot(preset.comment),
            theme_slot(preset.string),
            theme_slot(preset.function),
            theme_slot(preset.error),
            theme_slot(preset.diff_add),
            theme_slot(preset.diff_delete),
            theme_slot(preset.panel),
            theme_slot(preset.background),
        );
        PickerItem::new(preset.name)
            .with_label(preset.name)
            .with_detail(preset.description)
            .with_preview(preview)
    }));
    items
}

fn theme_slot(value: Option<&'static str>) -> &'static str {
    value.unwrap_or("—")
}

/// Rows for `/models`. `@default` clears the session's pinned model.
fn model_picker_items(models: &[eix_core::ModelInfo]) -> Vec<PickerItem> {
    let mut items = vec![PickerItem::new("@default")
        .with_label("(daemon default)")
        .with_detail("use the model from [provider]")
        .with_preview("Clear this session's model override.\n\nUse /models to pin a model again.")];
    for model in models {
        let mut detail = model.name.clone();
        if model.name != model.id {
            detail = format!("{}  {}", model.name, model.id);
        }
        if model.default {
            detail.push_str("  · default");
        }
        if !model.variants.is_empty() {
            detail.push_str(&format!("  · variants: {}", model.variants.join(", ")));
        }
        if let (Some(input), Some(output)) = (model.input_cost, model.output_cost) {
            detail.push_str(&format!("  · ${input:.2}/${output:.2} per M"));
        }
        let mut preview = vec![
            model.id.clone(),
            model.name.clone(),
            format!("provider  {}", model.provider),
        ];
        if model.default {
            preview.push("default   yes".to_string());
        }
        if !model.variants.is_empty() {
            preview.push(format!("variants  {}", model.variants.join(", ")));
        }
        if let Some(context) = model.context_tokens {
            preview.push(format!("context   {context} tokens"));
        }
        if let Some(input) = model.input_cost {
            preview.push(format!("input     ${input:.2} / 1M tokens"));
        }
        if let Some(output) = model.output_cost {
            preview.push(format!("output    ${output:.2} / 1M tokens"));
        }
        items.push(
            PickerItem::new(&model.id)
                .with_detail(detail)
                .with_preview(preview.join("\n")),
        );
        // A variant is a separate row so it is discoverable without knowing
        // the `id#variant` syntax.
        for variant in &model.variants {
            let id = format!("{}#{variant}", model.id);
            items.push(
                PickerItem::new(&id)
                    .with_detail(format!("{}  · variant: {variant}", model.name))
                    .with_preview(format!(
                        "{id}\n{} ({}) with reasoning variant `{variant}`.",
                        model.name, model.provider
                    )),
            );
        }
    }
    items
}

/// Rows for `/agents`, limited to primary-capable agents.
fn agent_picker_items(agents: &[eix_core::AgentSpec]) -> Vec<PickerItem> {
    agents
        .iter()
        .filter(|agent| agent.mode.can_primary())
        .map(|agent| {
            let mut detail = agent.description.clone();
            detail.push_str(&format!("  · {}", agent.mode.as_str()));
            if let Some(model) = &agent.model {
                detail.push_str(&format!("  · {model}"));
            }
            if let Some(permission) = agent.permission {
                detail.push_str(&format!("  · {}", permission.as_str()));
            }
            let mut preview = vec![
                agent.name.clone(),
                String::new(),
                agent.description.clone(),
                format!("mode        {}", agent.mode.as_str()),
            ];
            if let Some(model) = &agent.model {
                preview.push(format!("model       {model}"));
            }
            if let Some(permission) = agent.permission {
                preview.push(format!("permission  {}", permission.as_str()));
            }
            if !agent.prompt.is_empty() {
                preview.push(String::new());
                preview.push(agent.prompt.clone());
            }
            if let Some(source) = &agent.source {
                preview.push(String::new());
                preview.push(format!("source      {}", source.display()));
            }
            PickerItem::new(&agent.name)
                .with_detail(detail)
                .with_preview(preview.join("\n"))
        })
        .collect()
}

fn skill_picker_items(skills: &[eix_core::Skill]) -> Vec<PickerItem> {
    skills
        .iter()
        .map(|skill| {
            let mut detail = skill.description.clone();
            if !skill.allowed_tools.is_empty() {
                detail.push_str(&format!(" · tools: {}", skill.allowed_tools.join(", ")));
            }
            let mut preview = vec![skill.name.clone(), String::new(), skill.description.clone()];
            if !skill.allowed_tools.is_empty() {
                preview.push(format!("allowed tools: {}", skill.allowed_tools.join(", ")));
            }
            preview.push(format!("source: {}", skill.source.display()));
            preview.push(String::new());
            preview.push(skill.body.clone());
            PickerItem::new(&skill.name)
                .with_detail(detail)
                .with_preview(preview.join("\n"))
        })
        .collect()
}

/// Rows for the `:` command palette. `value` is dispatched by
/// [`App::run_action`]; the label and help are only for display.
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
        (
            "close-pane",
            "close pane",
            "Close the focused pane (C-w c / Space C / Space q)",
        ),
        ("new", "new session", "Create a session (n / Space n)"),
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
        (
            "agents",
            "agents",
            "Choose the focused session's primary agent",
        ),
        (
            "toggle-agent",
            "Build / Plan",
            "Toggle the focused session's agent (Tab in prompt normal mode)",
        ),
        (
            "skills",
            "skills",
            "Browse installed skills with a body preview",
        ),
        (
            "models",
            "models",
            "Pin the focused session to a configured model",
        ),
        ("theme", "theme", "Choose a color preset with live preview"),
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
    #[serde(default)]
    usage: Option<eix_core::Usage>,
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default)]
    duration_ms: Option<u64>,
}

#[derive(Deserialize)]
struct BtwDeltaParams {
    session_id: String,
    btw_id: String,
    delta: String,
}

#[derive(Deserialize)]
struct BtwCompletedParams {
    session_id: String,
    btw_id: String,
    #[serde(default)]
    text: String,
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
struct InputParams {
    session_id: String,
    input_id: String,
    #[serde(default)]
    call_id: String,
    question: String,
    #[serde(default)]
    options: Vec<InputOption>,
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

pub(super) fn tool_call_summary(tool: &str, arguments: &Value) -> String {
    let string = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    match tool {
        "run_command" | "start_command" => format!("$ {}", string("command")),
        "read_file" => format!("read {}", string("path")),
        "list_dir" => {
            let path = string("path");
            format!("ls {}", if path.is_empty() { "." } else { path })
        }
        "write_file" => {
            let bytes = string("content").len();
            format!("write {} (+{bytes} bytes)", string("path"))
        }
        "process_output" => {
            let id = string("process_id");
            if id.is_empty() {
                "process output".to_string()
            } else {
                format!("process output {id}")
            }
        }
        "spawn_agent" => {
            let name = string("name");
            let prompt = truncate(string("prompt"), 60);
            if name.is_empty() {
                format!("agent: {prompt}")
            } else {
                format!("agent {name}: {prompt}")
            }
        }
        "ask_user" => format!("? {}", truncate(string("question"), 80)),
        _ => {
            let compact = serde_json::to_string(arguments).unwrap_or_default();
            format!("{tool} {}", truncate(&compact, 80))
        }
    }
}

pub(super) fn wrap_text(text: &str, width: usize) -> Vec<String> {
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

struct HelpEntry {
    key: &'static str,
    description: &'static str,
}

struct HelpSection {
    title: &'static str,
    entries: &'static [HelpEntry],
}

impl HelpEntry {
    /// Every whitespace-separated term must appear in the key or description.
    fn matches(&self, query: &str) -> bool {
        if query.is_empty() {
            return true;
        }
        let key = self.key.to_lowercase();
        let description = self.description.to_lowercase();
        query
            .split_whitespace()
            .all(|term| key.contains(term) || description.contains(term))
    }
}

const HELP_SECTIONS: &[HelpSection] = &[
    HelpSection {
        title: "prompt commands",
        entries: &[
            HelpEntry {
                key: "/help",
                description: "open this reference",
            },
            HelpEntry {
                key: "/new",
                description: "start a session here (/new optional-name)",
            },
            HelpEntry {
                key: "/clear",
                description: "forget the conversation context (transcript stays)",
            },
            HelpEntry {
                key: "/compact",
                description: "summarize the history (/compact optional notes)",
            },
            HelpEntry {
                key: "/btw",
                description: "one-off answer (/btw <question>); kept out of the conversation",
            },
            HelpEntry {
                key: "/cd",
                description: "change the working directory (/cd <path>); alone shows it",
            },
            HelpEntry {
                key: "/init",
                description: "draft AGENTS.md; you review before sending",
            },
            HelpEntry {
                key: "/queue",
                description: "list queued prompts (/queue clear | rm N | move N M)",
            },
            HelpEntry {
                key: "/sessions",
                description: "open the session picker",
            },
            HelpEntry {
                key: "/timeline",
                description: "recent turns, answers and tool calls (j/k scroll)",
            },
            HelpEntry {
                key: "/archive · /delete",
                description: "hide from the picker · delete (confirm twice)",
            },
            HelpEntry {
                key: "/undo · /redo",
                description: "hide or restore the newest turn",
            },
            HelpEntry {
                key: "/skills [name] [args]",
                description: "browse skills with a preview, or insert one with arguments",
            },
            HelpEntry {
                key: "/todos",
                description: "show the model's current task list",
            },
            HelpEntry {
                key: "/image <path>",
                description: "attach images (paste/drop a path or file:// URI); png/jpg/gif/webp",
            },
            HelpEntry {
                key: "!<cmd>",
                description: "run a shell command in the session cwd (sandboxed, snapshotted)",
            },
            HelpEntry {
                key: "/fork",
                description: "fork this session (/fork optional-name)",
            },
            HelpEntry {
                key: "/permission",
                description: "set read-only · workspace-write · full",
            },
            HelpEntry {
                key: "/exit · /quit",
                description: "save and close this session (quits on the last pane)",
            },
            HelpEntry {
                key: "/close",
                description: "save and close the focused pane (keeps the last pane)",
            },
        ],
    },
    HelpSection {
        title: "keys",
        entries: &[
            HelpEntry {
                key: "i / Enter / Space i",
                description: "focus the prompt in insert mode",
            },
            HelpEntry {
                key: "mouse",
                description: "click folds a Thinking / tool block; wheel scrolls the transcript",
            },
            HelpEntry {
                key: "Esc",
                description: "prompt: normal mode (keeps focus)",
            },
            HelpEntry {
                key: "Esc Esc",
                description: "normal mode: interrupt the running turn, or leave for the transcript",
            },
            HelpEntry {
                key: "Enter",
                description: "send (insert mode); prompts sent while busy are queued",
            },
            HelpEntry {
                key: "Ctrl-J",
                description: "newline (insert mode)",
            },
            HelpEntry {
                key: "Space s/f/w/b",
                description: "prompt & transcript: sessions / files / worktrees / branches",
            },
            HelpEntry {
                key: "Space c/h/p",
                description: "prompt & transcript: ledger / history / processes",
            },
            HelpEntry {
                key: "Space r/x/a/?",
                description: "prompt & transcript: approvals / cancel / actions / help",
            },
            HelpEntry {
                key: "Space n/q/Q",
                description: "prompt & transcript: new session / close pane / quit",
            },
            HelpEntry {
                key: "Shift+, / Shift+.",
                description: "prompt normal & transcript: previous / next pane",
            },
            HelpEntry {
                key: "Space N/W/F",
                description: "prompt & transcript: new session / new worktree / fork",
            },
            HelpEntry {
                key: "Ctrl-J / Ctrl-K",
                description: "normal mode: browse submitted prompts (newer / older)",
            },
            HelpEntry {
                key: "j / k",
                description: "normal mode: move between lines in the draft",
            },
            HelpEntry {
                key: ":",
                description: "command palette (prompt normal mode and transcript)",
            },
            HelpEntry {
                key: "Space s / Ctrl-G",
                description: "session picker",
            },
            HelpEntry {
                key: "Space o",
                description: "open session in a new pane",
            },
            HelpEntry {
                key: "Space w / Space b",
                description: "insert a git worktree path / branch name",
            },
            HelpEntry {
                key: "@ (in prompt)",
                description: "workspace file picker",
            },
            HelpEntry {
                key: "Space A",
                description: "jump to the next pane needing attention",
            },
            HelpEntry {
                key: "C-w h/j/k/l",
                description: "focus pane left/down/up/right",
            },
            HelpEntry {
                key: "C-w v / s",
                description: "open a new pane",
            },
            HelpEntry {
                key: "C-w c",
                description: "close focused pane",
            },
            HelpEntry {
                key: "C-w w",
                description: "cycle panes",
            },
            HelpEntry {
                key: "Tab / S-Tab",
                description: "next / previous session",
            },
            HelpEntry {
                key: "Tab (prompt normal)",
                description: "toggle Build / Plan mode",
            },
            HelpEntry {
                key: "Space n / Space N",
                description: "new session in a new pane",
            },
            HelpEntry {
                key: "Space W",
                description: "new session in a fresh git worktree",
            },
            HelpEntry {
                key: "Space x",
                description: "cancel focused turn",
            },
            HelpEntry {
                key: "y / n",
                description: "approve / deny a pending tool call (card shortcut)",
            },
            HelpEntry {
                key: "j / k · Enter",
                description: "pending card: pick an approval action / answer with the highlighted option",
            },
            HelpEntry {
                key: "1-6 · C-n/C-p",
                description: "answer a question directly / next-prev option",
            },
            HelpEntry {
                key: "d",
                description: "review the pending diff full screen (Space toggles hunks)",
            },
            HelpEntry {
                key: "e / o",
                description: "edit the staged content / open the real file",
            },
            HelpEntry {
                key: "r",
                description: "deny the pending approval with a reason",
            },
            HelpEntry {
                key: "Space r",
                description: "approvals inbox across sessions",
            },
            HelpEntry {
                key: "j / k",
                description: "scroll focused transcript",
            },
            HelpEntry {
                key: "g / G",
                description: "top / follow tail",
            },
            HelpEntry {
                key: "Space c",
                description: "context ledger (f fork here, w rewind + roll back)",
            },
            HelpEntry {
                key: "Space p",
                description: "background processes (x kills, r refreshes)",
            },
            HelpEntry {
                key: "Space F",
                description: "fork the session (git worktree when inside a repo)",
            },
            HelpEntry {
                key: "#",
                description: "search message history (Enter runs / opens)",
            },
            HelpEntry {
                key: "? / F1",
                description: "open this help (F1 anywhere)",
            },
            HelpEntry {
                key: "Space Q / Ctrl-C",
                description: "quit (Ctrl-C interrupts instead while the prompt is focused)",
            },
            HelpEntry {
                key: "help search",
                description: "type to filter entries · ↑/↓ select · Ctrl-U clears · Esc closes",
            },
        ],
    },
    HelpSection {
        title: "notes",
        entries: &[
            HelpEntry {
                key: "prompt",
                description: "modal: Enter sends in insert; normal mode has vim motions, d/c/y, visual (v/V) and u/C-r",
            },
            HelpEntry {
                key: "C-w",
                description: "commands work from the transcript; Esc leaves the prompt first.",
            },
            HelpEntry {
                key: "ask_user",
                description: "the approval card is an option picker; the first option is recommended.",
            },
            HelpEntry {
                key: "ledger",
                description: "advisory: system and instruction entries are never trimmed automatically",
            },
        ],
    },
];

enum HelpRow {
    Blank,
    Section(&'static str),
    Entry(&'static HelpEntry),
}

fn help_rows(query: &str) -> Vec<HelpRow> {
    let query = query.trim().to_lowercase();
    let mut rows = Vec::new();
    for section in HELP_SECTIONS {
        let matched: Vec<&HelpEntry> = section
            .entries
            .iter()
            .filter(|entry| entry.matches(&query))
            .collect();
        if matched.is_empty() {
            continue;
        }
        if !rows.is_empty() {
            rows.push(HelpRow::Blank);
        }
        rows.push(HelpRow::Section(section.title));
        rows.extend(matched.into_iter().map(HelpRow::Entry));
    }
    rows
}

fn help_entry_count(query: &str) -> usize {
    let query = query.trim().to_lowercase();
    HELP_SECTIONS
        .iter()
        .flat_map(|section| section.entries.iter())
        .filter(|entry| entry.matches(&query))
        .count()
}

fn draw_help(frame: &mut Frame, area: Rect, query: &str, selected: usize, theme: Theme) {
    let dialog_bg = dialog_background(theme);
    let width = (area.width as u32 * 7 / 10).max(50).min(area.width as u32) as u16;
    let rows = help_rows(query);
    let matches = help_entry_count(query);
    let total: usize = HELP_SECTIONS
        .iter()
        .map(|section| section.entries.len())
        .sum();

    let height = (rows.len() as u16 + 5)
        .min(area.height.saturating_sub(2))
        .max(8);
    let rect = centered(area, width, height);
    frame.render_widget(Clear, rect);
    frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(" help ")
            .style(Style::default().bg(dialog_bg)),
        rect,
    );

    let inner = rect.inner(ratatui::layout::Margin::new(1, 1));
    let chunks = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(inner);

    let search = if query.is_empty() {
        Line::from(vec![
            Span::styled(
                "search",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "  type to filter the reference",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else {
        Line::from(vec![
            Span::styled(
                "search  ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(query.to_string()),
            Span::styled("▌", Style::default().fg(Color::DarkGray)),
        ])
    };
    let count = Line::from(Span::styled(
        format!("{matches}/{total} entries · ↑/↓ select · Ctrl-U clear · Esc close"),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(
        Paragraph::new(vec![search, count, Line::from("")]).style(Style::default().bg(dialog_bg)),
        chunks[0],
    );

    let selected_row = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| matches!(row, HelpRow::Entry(_)))
        .nth(selected)
        .map(|(index, _)| index);
    let visible = chunks[1].height as usize;
    let start = selected_row
        .map(|index| index.saturating_add(1).saturating_sub(visible))
        .unwrap_or(0);

    let mut ordinal = rows
        .iter()
        .take(start)
        .filter(|row| matches!(row, HelpRow::Entry(_)))
        .count();
    let lines: Vec<Line> = if rows.is_empty() {
        vec![Line::from(Span::styled(
            "no matching key help",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        rows.iter()
            .skip(start)
            .map(|row| match row {
                HelpRow::Blank => Line::from(""),
                HelpRow::Section(title) => Line::from(Span::styled(
                    *title,
                    Style::default().add_modifier(Modifier::BOLD),
                )),
                HelpRow::Entry(entry) => {
                    let is_selected = ordinal == selected;
                    ordinal += 1;
                    let mut line = Line::from(vec![
                        Span::styled(
                            if is_selected { "› " } else { "  " },
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(
                            format!("{:<15}", entry.key),
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(entry.description),
                    ]);
                    if is_selected {
                        line = line.style(Style::default().bg(Color::DarkGray).fg(Color::White));
                    }
                    line
                }
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .style(Style::default().bg(dialog_bg)),
        chunks[1],
    );
}

fn draw_timeline(
    frame: &mut Frame,
    area: Rect,
    items: &[eix_core::TimelineItem],
    scroll: usize,
    theme: Theme,
) {
    let dialog_bg = dialog_background(theme);
    let width = (area.width as u32 * 8 / 10).max(60).min(area.width as u32) as u16;
    let height = area.height.saturating_sub(4).max(8);
    let rect = centered(area, width, height);
    let inner_height = rect.height.saturating_sub(2) as usize;
    let lines: Vec<Line> = items
        .iter()
        .skip(scroll)
        .take(inner_height)
        .map(|item| {
            Line::from(vec![
                Span::styled(
                    format!("e{:<4} ", item.seq),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{:<17} ", item.kind),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(item.preview.replace('\n', " ")),
            ])
        })
        .collect();
    frame.render_widget(Clear, rect);
    frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);
    let paragraph = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" timeline (j/k scroll, Esc close) ")
                .style(Style::default().bg(dialog_bg)),
        )
        .style(Style::default().bg(dialog_bg));
    frame.render_widget(paragraph, rect);
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
/// Adjacent panes share one line: the left pane owns the right border, the
/// top pane owns the top border. This halves the gap between neighbouring
/// panes while keeping every title on a top border.
fn pane_borders(areas: &[Rect], index: usize) -> Borders {
    let Some(rect) = areas.get(index).copied() else {
        return Borders::ALL;
    };
    let overlaps = |a_start: u16, a_len: u16, b_start: u16, b_len: u16| {
        a_start < b_start.saturating_add(b_len) && b_start < a_start.saturating_add(a_len)
    };

    let mut borders = Borders::ALL;
    for (other_index, other) in areas.iter().enumerate() {
        if other_index == index {
            continue;
        }
        let left_neighbour = other.x.saturating_add(other.width) == rect.x
            && overlaps(other.y, other.height, rect.y, rect.height);
        let bottom_neighbour = other.y == rect.y.saturating_add(rect.height)
            && overlaps(other.x, other.width, rect.x, rect.width);
        if left_neighbour {
            borders.remove(Borders::LEFT);
        }
        if bottom_neighbour {
            borders.remove(Borders::BOTTOM);
        }
    }
    borders
}

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
fn review_unified_hunk_offsets(approval: &PendingApproval) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(approval.hunks.len());
    let mut line = 0;
    for hunk in &approval.hunks {
        offsets.push(line);
        line += hunk.lines.len().max(1);
    }
    offsets
}

/// Number of lines the unified fallback renders.
fn review_unified_total(approval: &PendingApproval) -> usize {
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

/// Map `Ctrl-N`/`Ctrl-P` (and the vim `Ctrl-J`/`Ctrl-K`) onto plain `j`/`k`
/// for list overlays whose letters already mean next/previous.
fn next_prev_key(key: KeyEvent) -> KeyEvent {
    if !key.modifiers.contains(KeyModifiers::CONTROL) {
        return key;
    }
    match key.code {
        KeyCode::Char('n') | KeyCode::Char('j') => {
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE)
        }
        KeyCode::Char('p') | KeyCode::Char('k') => {
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE)
        }
        _ => key,
    }
}

fn draw_approval_card(frame: &mut Frame, area: Rect, approval: &PendingApproval, theme: Theme) {
    let dialog_bg = dialog_background(theme);
    if area.width < 24 || area.height < 6 {
        return;
    }
    let actions = approval.actions();
    let cursor = approval.cursor.min(actions.len().saturating_sub(1));
    let diff_lines: Vec<&str> = approval.diff.as_deref().unwrap_or("").lines().collect();
    let inner = area.width.saturating_sub(4) as usize;
    let rows = super::diff::rows(&approval.hunks, approval.diff.as_deref());
    let side_by_side = (!rows.is_empty())
        .then(|| {
            super::diff::DiffRenderer::new(
                &rows,
                inner,
                approval.path.as_deref().unwrap_or(""),
                theme,
                "",
                dialog_bg,
            )
        })
        .flatten();
    let diff_len = if side_by_side.is_some() {
        rows.len()
    } else {
        diff_lines.len()
    };
    // The summary can be a full shell command. Wrap it instead of cutting it
    // off, but keep the action list and hint on screen.
    let (risk, risk_color) = approval_risk(&approval.tool);
    let risk_label = format!("  [{risk}]");
    let prefix_width = 2; // "⚠ "
    let body_width = inner
        .saturating_sub(prefix_width + UnicodeWidthStr::width(risk_label.as_str()))
        .max(8);
    let mut summary_lines = wrap_text(&approval.summary, body_width);
    let max_height = area.height.saturating_sub(1) as usize;
    let reserved = 2 + actions.len() + 1;
    let max_summary = max_height.saturating_sub(reserved).max(1);
    let summary_truncated = summary_lines.len() > max_summary;
    if summary_truncated {
        summary_lines.truncate(max_summary.saturating_sub(1));
        summary_lines.push("…".to_string());
    }

    // Borders + summary + actions + hint; the diff takes whatever is left.
    let fixed = 2 + summary_lines.len() + actions.len() + 1;
    let max_diff = max_height.saturating_sub(fixed).min(8);
    let shown = diff_len.min(max_diff);
    let truncated = diff_len > shown && max_diff > 0;
    // Leave the pane's title row visible and never underflow `y` on a pane
    // that starts at the top of the screen.
    let height = (fixed + shown + usize::from(truncated))
        .min(max_height)
        .max(3) as u16;
    let rect = Rect {
        x: area.x + 1,
        y: area.y + area.height - height - 1,
        width: area.width.saturating_sub(2),
        height,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);

    let mut lines = Vec::with_capacity(fixed + shown);
    for (index, summary) in summary_lines.into_iter().enumerate() {
        let mut spans = Vec::new();
        if index == 0 {
            spans.push(Span::styled(
                "⚠ ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));
            spans.push(Span::styled(summary, Style::default().fg(Color::Yellow)));
            spans.push(Span::styled(
                risk_label.clone(),
                Style::default().fg(risk_color),
            ));
        } else {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(summary, Style::default().fg(Color::Yellow)));
        }
        lines.push(Line::from(spans));
    }
    if let Some(mut renderer) = side_by_side {
        lines.extend(renderer.render(&rows[..rows.len().min(shown)], false, &[], 0));
    } else {
        for line in diff_lines.iter().take(shown) {
            lines.push(Line::from(Span::styled(
                truncate(line, inner),
                diff_line_style(line, theme),
            )));
        }
    }
    if truncated {
        lines.push(Line::from(Span::styled(
            "… diff truncated",
            Style::default().fg(Color::DarkGray),
        )));
    }
    let selected_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    for (index, action) in actions.iter().enumerate() {
        let marker = if index == cursor { "▸ " } else { "  " };
        let mut text = format!("{marker}{}. {}", index + 1, approval.action_label(*action));
        if index == 0 {
            text.push_str("  ★ recommended");
        }
        let text = truncate(&text, inner);
        let style = if index == cursor {
            selected_style
        } else {
            Style::default().fg(approval_action_color(*action))
        };
        lines.push(Line::from(Span::styled(pad_to(&text, inner), style)));
    }
    lines.push(Line::from(Span::styled(
        truncate(
            "j/k or C-n/C-p select · Enter confirm · y/n · x cancel",
            inner,
        ),
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Yellow))
        .title(format!(" approval · {} ", approval.tool));
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::default().bg(dialog_bg)),
        rect,
    );
}

fn approval_action_color(action: ApprovalAction) -> Color {
    match action {
        ApprovalAction::Approve => Color::Green,
        ApprovalAction::Always => Color::Green,
        ApprovalAction::Deny | ApprovalAction::Reason => Color::Red,
        ApprovalAction::Review | ApprovalAction::Open | ApprovalAction::Edit => Color::Cyan,
    }
}

/// Render an `ask_user` question as a keyboard-driven option picker.
fn draw_input_card(frame: &mut Frame, area: Rect, pending: &PendingInput, theme: Theme) {
    let dialog_bg = dialog_background(theme);
    if area.width < 24 || area.height < 5 {
        return;
    }
    let height = (3 + pending.options.len() + 1)
        .min(area.height.saturating_sub(1) as usize)
        .max(4) as u16;
    let rect = Rect {
        x: area.x + 1,
        y: area.y + area.height - height - 1,
        width: area.width.saturating_sub(2),
        height,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);
    let inner = rect.width.saturating_sub(2) as usize;
    let cursor = pending.cursor.min(pending.options.len().saturating_sub(1));
    let mut lines = vec![Line::from(vec![
        Span::styled(
            "? ",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            truncate(&pending.question, inner.saturating_sub(2)),
            Style::default().fg(Color::Yellow),
        ),
    ])];
    let selected_style = Style::default()
        .fg(Color::Black)
        .bg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    for (index, option) in pending.options.iter().enumerate() {
        let marker = if index == cursor { "▸ " } else { "  " };
        let mut text = format!("{marker}{}. {}", index + 1, option.label);
        if let Some(description) = option
            .description
            .as_deref()
            .map(str::trim)
            .filter(|description| !description.is_empty())
        {
            text.push_str(" — ");
            text.push_str(description);
        }
        if index == 0 {
            text.push_str("  ★ recommended");
        }
        let text = truncate(&text, inner);
        let style = if index == cursor {
            selected_style
        } else if index == 0 {
            Style::default().fg(Color::Green)
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(pad_to(&text, inner), style)));
    }
    lines.push(Line::from(Span::styled(
        truncate(
            "j/k or C-n/C-p select · Enter answer · 1-6 jump · x cancel",
            inner,
        ),
        Style::default().fg(Color::DarkGray),
    )));

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" question ");
    frame.render_widget(
        Paragraph::new(lines)
            .block(block)
            .style(Style::default().bg(dialog_bg)),
        rect,
    );
}

/// Pad a single-line string with spaces to `width` terminal cells.
fn pad_to(text: &str, width: usize) -> String {
    let used = UnicodeWidthStr::width(text);
    let mut out = String::with_capacity(text.len() + width.saturating_sub(used));
    out.push_str(text);
    for _ in used..width {
        out.push(' ');
    }
    out
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

/// A prompt run, highlighted while the visual selection covers it.
fn selection_span(text: String, selected: bool) -> Span<'static> {
    if selected {
        Span::styled(
            text,
            Style::default().fg(COMPOSER_TEXT).bg(COMPOSER_SELECTION),
        )
    } else {
        Span::raw(text)
    }
}

/// `provider/model` -> `model`, keeping the banner short.
fn short_model(model: &str) -> &str {
    model
        .rsplit_once('/')
        .map(|(_, tail)| tail)
        .unwrap_or(model)
}

/// Compact usage summary for the footer: tokens, throughput and cost.
fn turn_summary(done: &TurnCompleted) -> String {
    let Some(usage) = &done.usage else {
        return "idle".to_string();
    };
    let mut parts = vec![format!(
        "idle · {} tok",
        usage
            .total_tokens
            .max(usage.prompt_tokens.saturating_add(usage.completion_tokens),)
    )];
    if let Some(duration) = done.duration_ms.filter(|duration| *duration > 0) {
        let tps = usage.completion_tokens as f64 / (duration as f64 / 1000.0);
        parts.push(format!("{tps:.1} tok/s"));
    }
    if let Some(cost) = done.cost {
        parts.push(format!("${cost:.4}"));
    }
    parts.join(" · ")
}

/// Current git branch for `cwd`, or a short commit hash when HEAD is detached.
fn read_git_branch(cwd: &std::path::Path) -> Option<String> {
    let branch = git_stdout(cwd, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if branch == "HEAD" {
        return git_stdout(cwd, &["rev-parse", "--short", "HEAD"]).map(|sha| format!("@{sha}"));
    }
    Some(branch)
}

fn git_stdout(cwd: &std::path::Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Replace a leading `$HOME` with `~`.
fn compact_path(path: &str) -> String {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => {
            let home = home.to_string_lossy();
            match path.strip_prefix(home.as_ref()) {
                Some(rest) => format!("~{rest}"),
                None => path.to_string(),
            }
        }
        _ => path.to_string(),
    }
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

pub(super) fn truncate(text: &str, max: usize) -> String {
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

/// The prompt is deliberately transparent: its chrome paints text and borders
/// over the theme's canvas (or the terminal background) instead of a separate
/// panel fill.
fn composer_background(theme: Theme) -> Color {
    theme.background.unwrap_or(Color::Reset)
}

/// Background for floating dialogs. Presets may set a full-terminal
/// background, a panel color, or neither; the default transparent theme falls
/// back to the terminal background.
fn dialog_background(theme: Theme) -> Color {
    theme.background.or(theme.panel).unwrap_or(Color::Reset)
}

fn composer_text(theme: Theme) -> Color {
    theme.normal.unwrap_or(COMPOSER_TEXT)
}

fn composer_muted(theme: Theme) -> Color {
    theme.comment.unwrap_or(COMPOSER_MUTED)
}

fn composer_border(theme: Theme) -> Color {
    theme.comment.unwrap_or(COMPOSER_BORDER)
}

fn composer_accent(theme: Theme) -> Color {
    theme.function.unwrap_or(COMPOSER_ACCENT)
}

fn composer_warn(theme: Theme) -> Color {
    theme.string.unwrap_or(COMPOSER_WARN)
}

fn composer_success(theme: Theme) -> Color {
    theme.diff_add.unwrap_or(COMPOSER_SUCCESS)
}

fn composer_danger(theme: Theme) -> Color {
    theme.error.unwrap_or(COMPOSER_DANGER)
}

/// Session state on the prompt banner uses the same restrained palette as the
/// transcript instead of the brighter ANSI colors.
fn composer_status_color(status: SessionStatus, theme: Theme) -> Color {
    match status {
        SessionStatus::Idle => composer_muted(theme),
        SessionStatus::Running => composer_success(theme),
        SessionStatus::AwaitingApproval => composer_warn(theme),
        SessionStatus::AwaitingInput => composer_accent(theme),
        SessionStatus::Error => composer_danger(theme),
        SessionStatus::Paused => composer_text(theme),
    }
}

/// Insert is the active writing mode (green), normal is the Vim home mode
/// (blue), visual is a selection (amber). Blurred mode badges stay quiet.
fn composer_mode_color(mode: Mode, theme: Theme) -> Color {
    match mode {
        Mode::Insert => composer_success(theme),
        Mode::Normal => composer_accent(theme),
        Mode::Visual | Mode::VisualLine => composer_warn(theme),
    }
}

fn composer_mode_style(mode: Mode, focused: bool, theme: Theme) -> Style {
    let color = if focused {
        composer_mode_color(mode, theme)
    } else {
        composer_border(theme)
    };
    Style::default()
        .fg(COMPOSER_BADGE_TEXT)
        .bg(color)
        .add_modifier(Modifier::BOLD)
}

/// Keep a left/right pair of border titles from colliding. The right title is
/// dropped first; the left one is clipped only when it cannot fit by itself.
fn fit_title(
    left: Line<'static>,
    right: Option<Line<'static>>,
    width: usize,
) -> (Line<'static>, Option<Line<'static>>) {
    let width = width.max(1);
    let right = right.filter(|right| left.width() + right.width() + 2 <= width);
    let left = if left.width() > width {
        truncate_line(left, width)
    } else {
        left
    };
    (left, right)
}

fn truncate_line(line: Line<'static>, width: usize) -> Line<'static> {
    if width == 0 {
        return Line::default();
    }
    let mut used = 0usize;
    let mut spans = Vec::new();
    for span in line.spans {
        let span_width = UnicodeWidthStr::width(span.content.as_ref());
        if used + span_width <= width {
            used += span_width;
            spans.push(span);
            continue;
        }
        let remaining = width.saturating_sub(used);
        if remaining > 0 {
            spans.push(Span::styled(
                truncate(span.content.as_ref(), remaining),
                span.style,
            ));
        }
        break;
    }
    Line::from(spans)
}

fn text_line(line: &Line<'static>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

fn normalize_selection(selection: &TextSelection) -> ((usize, usize), (usize, usize)) {
    let mut start = selection.anchor;
    let mut end = selection.cursor;
    if start > end {
        std::mem::swap(&mut start, &mut end);
    }
    (start, end)
}

/// Extract the selected cells from the plain transcript lines. Cell columns
/// make the result line up with the reverse-video highlight, including for
/// wide glyphs.
fn selection_text(lines: &[String], selection: &TextSelection) -> String {
    let ((start_line, start_col), (end_line, end_col)) = normalize_selection(selection);
    if start_line >= lines.len() || start_line > end_line {
        return String::new();
    }
    let end_line = end_line.min(lines.len().saturating_sub(1));
    let mut selected = Vec::new();
    for (index, line) in lines
        .iter()
        .enumerate()
        .skip(start_line)
        .take(end_line + 1 - start_line)
    {
        let from = if index == start_line { start_col } else { 0 };
        let to = if index == end_line {
            end_col
        } else {
            usize::MAX
        };
        selected.push(slice_cells(line, from, to).trim_end().to_string());
    }
    selected.join("\n").trim_end().to_string()
}

fn slice_cells(text: &str, start: usize, end: usize) -> String {
    let mut out = String::new();
    let mut column = 0usize;
    for ch in text.chars() {
        let width = UnicodeWidthChar::width(ch).unwrap_or(0);
        let selected = if width == 0 {
            column >= start && column < end
        } else {
            column < end && column.saturating_add(width) > start
        };
        if selected {
            out.push(ch);
        }
        column = column.saturating_add(width);
    }
    out
}

#[cfg(test)]
fn highlight_selection(lines: &mut [Line<'static>], selection: &TextSelection) {
    highlight_selection_window(lines, selection, 0);
}

/// Highlight a visible window whose first line is `first_line` in the full
/// transcript.
fn highlight_selection_window(
    lines: &mut [Line<'static>],
    selection: &TextSelection,
    first_line: usize,
) {
    let ((start_line, start_col), (end_line, end_col)) = normalize_selection(selection);
    if start_line == end_line && start_col == end_col {
        return;
    }
    for (offset, line) in lines.iter_mut().enumerate() {
        let index = first_line.saturating_add(offset);
        if index < start_line || index > end_line {
            continue;
        }
        let from = if index == start_line { start_col } else { 0 };
        let to = if index == end_line {
            end_col
        } else {
            usize::MAX
        };
        *line = highlight_line(line, from, to);
    }
}

fn highlight_line(line: &Line<'static>, start: usize, end: usize) -> Line<'static> {
    let mut spans = Vec::new();
    let mut column = 0usize;
    for span in &line.spans {
        let mut run = String::new();
        let mut selected_run = false;
        for ch in span.content.chars() {
            let width = UnicodeWidthChar::width(ch).unwrap_or(0);
            let selected = if width == 0 {
                column >= start && column < end
            } else {
                column < end && column.saturating_add(width) > start
            };
            if run.is_empty() {
                selected_run = selected;
            } else if selected != selected_run {
                let style = if selected_run {
                    span.style.add_modifier(Modifier::REVERSED)
                } else {
                    span.style
                };
                spans.push(Span::styled(std::mem::take(&mut run), style));
                selected_run = selected;
            }
            run.push(ch);
            column = column.saturating_add(width);
        }
        if !run.is_empty() {
            let style = if selected_run {
                span.style.add_modifier(Modifier::REVERSED)
            } else {
                span.style
            };
            spans.push(Span::styled(run, style));
        }
    }
    Line {
        style: line.style,
        alignment: line.alignment,
        spans,
    }
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

/// Parse a drag-and-drop / clipboard paste as image paths. Returns `None` when
/// the paste is ordinary text, so callers can fall back to the editor.
fn parse_image_paths(text: &str, cwd: &std::path::Path) -> Option<Vec<String>> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    if !trimmed.contains('\n') {
        if let Some(path) = normalize_image_path(trimmed, cwd) {
            return Some(vec![path]);
        }
    }
    let tokens = shell_words(trimmed);
    if tokens.is_empty() {
        return None;
    }
    let mut paths = Vec::new();
    for token in tokens {
        paths.push(normalize_image_path(&token, cwd)?);
    }
    Some(paths)
}

fn normalize_image_path(raw: &str, cwd: &std::path::Path) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let path = if let Some(uri) = raw.strip_prefix("file://") {
        let uri = uri.strip_prefix("localhost").unwrap_or(uri);
        std::path::PathBuf::from(percent_decode(uri))
    } else if let Some(rest) = raw.strip_prefix("~/") {
        dirs::home_dir()?.join(rest)
    } else {
        let relative = std::path::Path::new(raw);
        if relative.is_absolute() {
            relative.to_path_buf()
        } else {
            cwd.join(relative)
        }
    };
    let extension = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if !matches!(extension.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
        return None;
    }
    if !path.is_file() {
        return None;
    }
    // Keep relative tokens relative so the daemon's workspace path checks see
    // the same path the user dropped. File URIs and `~` have to be expanded
    // because the daemon does not parse either form.
    if raw.starts_with("file://") || raw.starts_with("~/") {
        Some(path.to_string_lossy().into_owned())
    } else {
        Some(raw.to_string())
    }
}

/// Minimal shell-like tokenizer: whitespace separators, single/double quotes
/// and backslash escapes. Good enough for paths pasted by terminal emulators.
fn shell_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(ch) = chars.next() {
        match quote {
            Some(active) if ch == active => quote = None,
            Some(_) => current.push(ch),
            None if ch == '\'' || ch == '"' => quote = Some(ch),
            None if ch == '\\' => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            None if ch.is_whitespace() => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            None => current.push(ch),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(byte) = u8::from_str_radix(&value[index + 1..index + 3], 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eix_core::SearchResult;

    #[test]
    fn paste_recognizes_image_paths_and_file_uris() {
        let dir = std::env::temp_dir().join(format!(
            "eix-paste-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let first = dir.join("shot one.png");
        let second = dir.join("two.jpg");
        std::fs::write(&first, b"png").unwrap();
        std::fs::write(&second, b"jpg").unwrap();

        assert_eq!(
            parse_image_paths("shot one.png", &dir),
            Some(vec!["shot one.png".to_string()])
        );
        assert_eq!(
            parse_image_paths("\"shot one.png\" 'two.jpg'", &dir),
            Some(vec!["shot one.png".to_string(), "two.jpg".to_string()])
        );
        let uri = format!("file://{}", first.to_string_lossy().replace(' ', "%20"));
        assert_eq!(
            parse_image_paths(&uri, &dir),
            Some(vec![first.to_string_lossy().into_owned()])
        );
        assert_eq!(parse_image_paths("hello world", &dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wrapping_respects_display_width() {
        assert_eq!(wrap_text("abcdef", 3), vec!["abc", "def"]);
        assert_eq!(wrap_text("a\n\nb", 10), vec!["a", "", "b"]);
        assert_eq!(wrap_text("世界", 2), vec!["世", "界"]);
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
    fn adjacent_panes_share_one_border() {
        let area = Rect::new(0, 0, 100, 40);
        let two = pane_rects(area, 2);
        assert!(pane_borders(&two, 0).contains(Borders::RIGHT));
        assert!(!pane_borders(&two, 1).contains(Borders::LEFT));

        let stacked = vec![Rect::new(0, 0, 20, 10), Rect::new(0, 10, 20, 10)];
        assert!(!pane_borders(&stacked, 0).contains(Borders::BOTTOM));
        assert!(pane_borders(&stacked, 1).contains(Borders::TOP));
    }

    #[test]
    fn split_panes_render_both_titles() {
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "left pane", "/tmp/left"),
            Vec::new(),
        ));
        app.views.push(SessionView::new(
            Session::new("s2", "right pane", "/tmp/right"),
            Vec::new(),
        ));
        app.connected = true;

        let rendered = render_text(&mut app, 80, 20);
        assert!(rendered.contains("left pane"), "{rendered}");
        assert!(rendered.contains("right pane"), "{rendered}");
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

    fn render_transcript(view: &SessionView, width: usize) -> String {
        let live = view.stream.as_ref().map(|stream| transcript::Live {
            reasoning: &stream.reasoning,
            text: &stream.text,
        });
        transcript::render(
            &view.events,
            live,
            &[],
            &view.folds,
            width,
            Theme::default(),
        )
        .lines
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n")
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
        let mut view = SessionView::new(session, events);

        // Finished calls collapse to a single summary line with a status.
        let collapsed = render_transcript(&view, 60);
        assert!(collapsed.contains("write out.txt (+5 bytes)"));
        assert!(collapsed.contains("ok"));
        assert!(!collapsed.contains("wrote 5 bytes to out.txt"));

        // Expanding the block reveals the written body and the result without
        // repeating the summary in an `input`/`content`/`output` label.
        view.folds.insert(FoldId::Event(1), true);
        let expanded = render_transcript(&view, 60);
        assert!(expanded.contains("wrote 5 bytes to out.txt"));
        assert!(expanded.contains("hello"));
        assert!(!expanded.contains("output"), "{expanded}");
        assert!(!expanded.contains("content"), "{expanded}");
    }

    #[test]
    fn expanded_command_omits_redundant_labels_and_gutter() {
        let session = Session::new("s1", "tools", "/tmp");
        let events = vec![
            SessionEvent::new(
                "s1",
                1,
                EventKind::ToolCall,
                serde_json::json!({
                    "call_id": "c1",
                    "tool": "run_command",
                    "arguments": {"command": "echo hi"},
                }),
            ),
            SessionEvent::new(
                "s1",
                2,
                EventKind::ToolResult,
                serde_json::json!({
                    "call_id": "c1",
                    "tool": "run_command",
                    "ok": true,
                    "denied": false,
                    "output": "hi",
                }),
            ),
        ];
        let mut view = SessionView::new(session, events);
        view.folds.insert(FoldId::Event(1), true);
        let expanded = render_transcript(&view, 60);

        // The header already shows `$ echo hi`; the body is only the output.
        assert!(!expanded.contains("input"), "{expanded}");
        assert!(!expanded.contains("output"), "{expanded}");
        assert!(!expanded.contains('│'), "{expanded}");
        let output = expanded
            .lines()
            .find(|line| line.trim() == "hi")
            .expect("command output line");
        assert_eq!(output, "    hi");
    }

    #[test]
    fn long_command_wraps_in_header_without_repeating_in_body() {
        let session = Session::new("s1", "tools", "/tmp");
        let command = format!("echo {}", "x".repeat(120));
        let events = vec![
            SessionEvent::new(
                "s1",
                1,
                EventKind::ToolCall,
                serde_json::json!({
                    "call_id": "c1",
                    "tool": "run_command",
                    "arguments": {"command": command, "cwd": "/tmp"},
                }),
            ),
            SessionEvent::new(
                "s1",
                2,
                EventKind::ToolResult,
                serde_json::json!({
                    "call_id": "c1",
                    "tool": "run_command",
                    "ok": true,
                    "denied": false,
                    "output": "done",
                }),
            ),
        ];
        let mut view = SessionView::new(session, events);
        view.folds.insert(FoldId::Event(1), true);
        let expanded = render_transcript(&view, 60);
        let lines: Vec<&str> = expanded.lines().collect();

        // The first header line keeps the status and is no longer elided.
        assert!(lines[0].contains("$ echo"), "{expanded}");
        assert!(lines[0].contains("ok"), "{expanded}");
        assert!(!expanded.contains('…'), "{expanded}");
        // The command wraps onto continuation lines aligned under the summary.
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("      ") && line.contains('x')),
            "{expanded}"
        );
        // No truncated header plus a full repeated command line in the body.
        assert_eq!(expanded.matches("$ echo").count(), 1, "{expanded}");
        assert!(
            !lines.iter().any(|line| line.starts_with("    $ ")),
            "{expanded}"
        );
        // Every byte of the command is shown exactly once.
        assert_eq!(expanded.matches('x').count(), 120, "{expanded}");
    }

    #[test]
    fn transcript_renders_chat_turns_and_thinking() {
        let session = Session::new("s1", "chat", "/tmp");
        let events = vec![
            SessionEvent::new(
                "s1",
                1,
                EventKind::UserMessage,
                serde_json::json!({"text": "hi there"}),
            ),
            SessionEvent::new(
                "s1",
                2,
                EventKind::AssistantMessage,
                serde_json::json!({
                    "text": "hello",
                    "model": "m",
                    "reasoning": "pondering",
                    "cancelled": false,
                }),
            ),
        ];
        let view = SessionView::new(session, events);
        let rendered = render_transcript(&view, 40);
        // No role prefixes; the user turn is right-aligned.
        assert!(!rendered.contains("you>"));
        assert!(!rendered.contains("eix>"));
        let lines: Vec<&str> = rendered.lines().collect();
        let user = lines.iter().find(|line| line.contains("hi there")).unwrap();
        assert!(
            user.starts_with("  "),
            "user turn should be indented: {user:?}"
        );
        assert!(lines.iter().any(|line| line.contains("▾ Thinking")));
        // Stored thinking is expanded by default.
        assert!(rendered.contains("pondering"));
        let mut view = view;
        view.folds.insert(FoldId::Event(2), false);
        assert!(!render_transcript(&view, 40).contains("pondering"));
    }

    #[test]
    fn user_messages_use_a_two_row_bubble_without_margins() {
        let session = Session::new("s1", "chat", "/tmp");
        let events = vec![SessionEvent::new(
            "s1",
            1,
            EventKind::UserMessage,
            serde_json::json!({"text": "hi there"}),
        )];
        let view = SessionView::new(session, events);
        let rendered = render_transcript(&view, 40);
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines.len(),
            2,
            "bubble should be one cap plus content: {rendered}"
        );
        assert!(lines[0].contains('╭'), "{rendered}");
        assert!(lines[1].contains("│ hi there │"), "{rendered}");
        assert!(!rendered.contains('╰'), "{rendered}");
    }

    #[test]
    fn transcript_marks_context_events() {
        let session = Session::new("s1", "ctx", "/tmp");
        let events = vec![
            SessionEvent::new(
                "s1",
                1,
                EventKind::UserMessage,
                serde_json::json!({ "text": "old question" }),
            ),
            SessionEvent::new(
                "s1",
                2,
                EventKind::ContextCompacted,
                serde_json::json!({
                    "from_seq": 1,
                    "to_seq": 1,
                    "messages": 1,
                    "tokens": 10,
                    "summary": "we talked",
                }),
            ),
            SessionEvent::new("s1", 3, EventKind::ContextCleared, serde_json::json!({})),
        ];
        let view = SessionView::new(session, events);
        let rendered = render_transcript(&view, 60);
        assert!(rendered.contains("context compacted · 1 message(s)"));
        assert!(rendered.contains("context cleared"));
        // Clearing context never removes the transcript history.
        assert!(rendered.contains("old question"));
    }

    #[test]
    fn transcript_cache_refreshes_after_new_events() {
        let session = Session::new("s1", "cache", "/tmp");
        let events = vec![SessionEvent::new(
            "s1",
            1,
            EventKind::UserMessage,
            serde_json::json!({ "text": "first turn" }),
        )];
        let mut app = App::new(None);
        app.views.push(SessionView::new(session, events));
        app.focused = 0;
        assert!(render_text(&mut app, 60, 12).contains("first turn"));

        app.views[0].events.push(SessionEvent::new(
            "s1",
            2,
            EventKind::UserMessage,
            serde_json::json!({ "text": "second turn" }),
        ));
        app.views[0].events_rev += 1;
        assert!(render_text(&mut app, 60, 12).contains("second turn"));
    }

    #[test]
    fn live_tail_renders_over_cached_history() {
        let session = Session::new("s1", "cache", "/tmp");
        let events = vec![SessionEvent::new(
            "s1",
            1,
            EventKind::UserMessage,
            serde_json::json!({ "text": "stored history" }),
        )];
        let mut app = App::new(None);
        app.views.push(SessionView::new(session, events));
        app.focused = 0;
        app.views[0].stream = Some(StreamState {
            turn_id: "t1".into(),
            text: "half an answer".into(),
            reasoning: String::new(),
        });
        assert!(render_text(&mut app, 60, 12).contains("half an answer"));

        app.views[0].stream.as_mut().unwrap().text = "full answer".into();
        assert!(render_text(&mut app, 60, 12).contains("full answer"));
    }

    #[test]
    fn cached_history_honors_fold_changes() {
        let session = Session::new("s1", "cache", "/tmp");
        let events = vec![SessionEvent::new(
            "s1",
            2,
            EventKind::AssistantMessage,
            serde_json::json!({
                "text": "answer",
                "model": "test",
                "reasoning": "hidden chain",
            }),
        )];
        let mut app = App::new(None);
        app.views.push(SessionView::new(session, events));
        app.focused = 0;
        let unfolded = render_text(&mut app, 60, 24);
        assert!(unfolded.contains("hidden chain"), "{unfolded:?}");

        app.views[0].folds.insert(FoldId::Event(2), false);
        let folded = render_text(&mut app, 60, 24);
        assert!(folded.contains("Thinking"));
        assert!(!folded.contains("hidden chain"));
    }

    #[test]
    fn cached_history_renders_only_the_visible_window() {
        let session = Session::new("s1", "cache", "/tmp");
        let events: Vec<_> = (0..20)
            .map(|index| {
                SessionEvent::new(
                    "s1",
                    index + 1,
                    EventKind::UserMessage,
                    serde_json::json!({ "text": format!("turn {index}") }),
                )
            })
            .collect();
        let mut app = App::new(None);
        app.views.push(SessionView::new(session, events));
        app.focused = 0;
        app.views[0].follow = false;
        app.views[0].scroll = 0;
        let top = render_text(&mut app, 60, 12);
        assert!(top.contains("turn 0"), "{top}");
        assert!(!top.contains("turn 19"), "{top}");

        app.views[0].follow = true;
        let bottom = render_text(&mut app, 60, 12);
        assert!(bottom.contains("turn 19"), "{bottom}");
    }

    #[test]
    fn history_page_prepends_older_events_and_keeps_scroll() {
        let session = Session::new("s1", "paged", "/tmp");
        let recent: Vec<_> = (10..20)
            .map(|index| {
                SessionEvent::new(
                    "s1",
                    index + 1,
                    EventKind::UserMessage,
                    serde_json::json!({ "text": format!("turn {index}") }),
                )
            })
            .collect();
        let older: Vec<_> = (0..10)
            .map(|index| {
                SessionEvent::new(
                    "s1",
                    index + 1,
                    EventKind::UserMessage,
                    serde_json::json!({ "text": format!("turn {index}") }),
                )
            })
            .collect();
        let mut app = App::new(None);
        app.views.push(SessionView::new(session, recent));
        app.focused = 0;
        app.views[0].history_has_more = true;
        render_text(&mut app, 60, 12);

        app.views[0].follow = false;
        app.views[0].scroll = 4;
        app.history_page("s1".into(), older, false);
        assert_eq!(app.views[0].events.len(), 20);
        assert_eq!(app.views[0].events[0].seq, 1);
        assert_eq!(app.views[0].events_rev, 1);

        render_text(&mut app, 60, 12);
        assert!(
            app.views[0].scroll > 4,
            "viewport should move by the prepended line count"
        );
        assert!(app.views[0]
            .text_lines
            .iter()
            .any(|line| line.contains("turn 0")));
        assert!(!app.views[0].history_has_more);
        assert!(app.take_history_request().is_none());
    }

    #[test]
    fn history_request_loads_one_page_at_a_time() {
        let session = Session::new("s1", "paged", "/tmp");
        let events: Vec<_> = (0..10)
            .map(|index| {
                SessionEvent::new(
                    "s1",
                    index + 101,
                    EventKind::UserMessage,
                    serde_json::json!({ "text": format!("turn {index}") }),
                )
            })
            .collect();
        let mut app = App::new(None);
        app.views.push(SessionView::new(session, events));
        app.focused = 0;
        app.views[0].history_has_more = true;
        app.views[0].follow = false;
        app.views[0].scroll = 0;
        render_text(&mut app, 60, 12);

        let (session_id, before_seq) = app
            .take_history_request()
            .expect("scrolling to the top requests an older page");
        assert_eq!(session_id, "s1");
        assert_eq!(before_seq, 101);
        assert!(app.views[0].history_loading);
        assert!(!app.views[0].history_armed);

        render_text(&mut app, 60, 12);
        assert!(
            app.take_history_request().is_none(),
            "an in-flight page must not be requested twice"
        );

        let older: Vec<_> = (1..=100)
            .map(|seq| {
                SessionEvent::new(
                    "s1",
                    seq,
                    EventKind::UserMessage,
                    serde_json::json!({ "text": format!("older {seq}") }),
                )
            })
            .collect();
        app.history_page(session_id, older, false);
        render_text(&mut app, 60, 12);
        assert_eq!(app.views[0].events.len(), 110);
        assert_eq!(app.views[0].events[0].seq, 1);
        assert!(!app.views[0].history_loading);
        assert!(!app.views[0].history_has_more);
        assert!(app.views[0].scroll > 0);
        assert!(app.take_history_request().is_none());
    }

    #[test]
    fn expanded_tool_card_shows_the_full_long_command() {
        let session = Session::new("s1", "tools", "/tmp");
        let command = format!("echo {} ENDMARK", "x".repeat(70));
        let events = vec![SessionEvent::new(
            "s1",
            1,
            EventKind::ToolCall,
            serde_json::json!({
                "call_id": "c1",
                "tool": "run_command",
                "arguments": {"command": command, "cwd": "/tmp"},
            }),
        )];
        let mut view = SessionView::new(session, events);

        let open = render_transcript(&view, 60);
        assert!(open.contains("ENDMARK"), "{open}");
        assert!(open.contains("cwd: /tmp"), "{open}");

        view.folds.insert(FoldId::Event(1), false);
        let collapsed = render_transcript(&view, 60);
        assert!(!collapsed.contains("ENDMARK"), "{collapsed}");
        assert!(collapsed.contains('…'), "{collapsed}");
    }

    #[test]
    fn transcript_streams_live_thinking() {
        let session = Session::new("s1", "chat", "/tmp");
        let mut view = SessionView::new(session, Vec::new());
        view.stream = Some(StreamState {
            turn_id: "t1".into(),
            text: String::new(),
            reasoning: "thinking out loud".into(),
        });
        let rendered = render_transcript(&view, 40);
        assert!(rendered.contains("Thinking…"));
        assert!(rendered.contains("thinking out loud"));
    }

    #[test]
    fn reasoning_delta_accumulates_before_the_answer() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "chat", "/tmp"),
            Vec::new(),
        ));
        for (method, delta) in [("reasoning.delta", "think"), ("message.delta", "answer")] {
            app.handle_event(
                Event::Notification(eix_core::Notification::new(
                    method,
                    serde_json::json!({
                        "session_id": "s1",
                        "turn_id": "t1",
                        "delta": delta,
                    }),
                )),
                &tx,
            );
        }
        let stream = app.views[0].stream.as_ref().expect("stream");
        assert_eq!(stream.reasoning, "think");
        assert_eq!(stream.text, "answer");
    }

    #[test]
    fn clicking_a_fold_toggles_it() {
        let mut app = App::new(None);
        let session = Session::new("s1", "tools", "/tmp");
        let mut view = SessionView::new(session, Vec::new());
        view.slots = vec![Some(transcript::Fold {
            id: FoldId::Event(1),
            open: false,
        })];
        app.views.push(view);
        app.pane_areas = vec![Rect::new(0, 0, 40, 10)];

        let click = |kind, row| MouseEvent {
            kind,
            column: 3,
            row,
            modifiers: KeyModifiers::NONE,
        };
        let press = MouseEventKind::Down(MouseButton::Left);
        let release = MouseEventKind::Up(MouseButton::Left);
        // The first inner row is the block header. A click toggles on release
        // so the same gesture can also start a drag selection.
        app.handle_mouse(click(press, 1));
        app.handle_mouse(click(release, 1));
        assert_eq!(app.views[0].folds.get(&FoldId::Event(1)), Some(&true));
        app.views[0].slots[0] = Some(transcript::Fold {
            id: FoldId::Event(1),
            open: true,
        });
        app.handle_mouse(click(press, 1));
        app.handle_mouse(click(release, 1));
        assert_eq!(app.views[0].folds.get(&FoldId::Event(1)), Some(&false));
    }

    #[test]
    fn dragging_the_transcript_copies_the_selected_cells() {
        let mut app = App::new(None);
        let mut view = SessionView::new(Session::new("s1", "tools", "/tmp"), Vec::new());
        view.text_lines = vec!["hello world".into(), "second line".into()];
        app.views.push(view);
        app.pane_areas = vec![Rect::new(0, 0, 40, 10)];

        let mouse = |kind, column, row| MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        app.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 1, 1));
        app.handle_mouse(mouse(MouseEventKind::Drag(MouseButton::Left), 6, 1));
        app.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 6, 1));

        assert_eq!(app.take_clipboard().as_deref(), Some("hello"));
        assert!(app.status.contains("copied 5 chars"));
        assert!(app.views[0].selection.is_some());

        // The copied text also seeds the prompt register, so normal-mode `p`
        // pastes it even without a native clipboard helper.
        assert!(app.input.has_register_text());
        app.focus = Focus::Input;
        app.input.enter_normal();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &tx);
        assert_eq!(app.input.text(), "hello");
    }

    #[test]
    fn selection_text_spans_lines_and_respects_wide_glyphs() {
        let lines = vec!["hello world".to_string(), "second line".to_string()];
        let selection = TextSelection {
            anchor: (0, 6),
            cursor: (1, 6),
            dragging: false,
            moved: true,
        };
        assert_eq!(selection_text(&lines, &selection), "world\nsecond");

        let wide = vec!["你a".to_string()];
        let selection = TextSelection {
            anchor: (0, 0),
            cursor: (0, 2),
            dragging: false,
            moved: true,
        };
        assert_eq!(selection_text(&wide, &selection), "你");
        let selection = TextSelection {
            anchor: (0, 0),
            cursor: (0, 3),
            dragging: false,
            moved: true,
        };
        assert_eq!(selection_text(&wide, &selection), "你a");
    }

    #[test]
    fn selection_highlight_is_reverse_video_and_keeps_text() {
        let mut lines = vec![Line::from("hello world")];
        let selection = TextSelection {
            anchor: (0, 6),
            cursor: (0, 11),
            dragging: false,
            moved: true,
        };
        highlight_selection(&mut lines, &selection);
        assert_eq!(text_line(&lines[0]), "hello world");
        let reversed: String = lines[0]
            .spans
            .iter()
            .filter(|span| span.style.add_modifier.contains(Modifier::REVERSED))
            .map(|span| span.content.as_ref())
            .collect();
        assert_eq!(reversed, "world");
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
    fn approval_card_shows_the_full_long_command() {
        let command = format!("echo {} ENDMARK", "x".repeat(220));
        let session = Session::new("s1", "tools", "/tmp");
        let event = SessionEvent::new(
            "s1",
            1,
            EventKind::ApprovalRequired,
            serde_json::json!({
                "approval_id": "a1",
                "call_id": "c1",
                "tool": "run_command",
                "summary": "run_command: echo (truncated)",
                "arguments": {"command": command, "cwd": "/tmp"},
            }),
        );
        let pending = SessionView::new(session.clone(), vec![event])
            .pending_approval()
            .expect("pending command approval");
        assert_eq!(pending.summary, format!("$ {command}"));

        let mut app = App::new(None);
        app.views.push(SessionView::new(session, Vec::new()));
        app.focused = 0;
        app.focus = Focus::Transcript;
        app.approvals.insert("s1".to_string(), pending);

        let rendered = render_text(&mut app, 72, 30);
        assert!(rendered.contains("$ echo"), "{rendered}");
        assert!(rendered.contains("ENDMARK"), "{rendered}");
        assert!(rendered.contains("Approve"), "{rendered}");
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
                cursor: 0,
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
                cursor: 0,
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
                cursor: 0,
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

        app.run_action("approvals", &tx);
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
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
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
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &tx);
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

        app.handle_transcript_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::NONE), &tx);
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
        assert!(PickerKind::Themes.data_kind().is_none());
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
        assert!(app.help_query.is_empty());
        assert_eq!(app.help_selected, 0);
    }

    #[test]
    fn theme_command_opens_picker_and_escape_restores() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.theme_config.preset = Some("github-dark".into());
        app.theme = Theme::from_config(&app.theme_config);
        let saved = app.theme;

        assert!(app.handle_prompt_command("/theme", &tx));
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Themes)
        ));
        assert!(app.theme_picker_backup.is_some());

        app.handle_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_ne!(app.theme.preset, Some("github-dark"));
        app.handle_picker_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(app.picker.is_none());
        assert_eq!(app.theme, saved);
        assert_eq!(app.theme_config.preset.as_deref(), Some("github-dark"));
        assert!(app.theme_picker_backup.is_none());

        // Theme arguments no longer switch directly; the picker is the only path.
        assert!(app.handle_prompt_command("/theme nord", &tx));
        assert!(app.picker.is_none());
        assert!(app
            .error
            .as_deref()
            .is_some_and(|error| error.contains("usage: /theme")));
    }

    #[test]
    fn theme_background_paints_panes_and_dialogs() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.theme_config.preset = Some("nord".into());
        app.theme = Theme::from_config(&app.theme_config);
        let session = Session::new("s1", "theme background", "/home/shiyi/git/eix");
        app.views.push(SessionView::new(session, Vec::new()));
        app.focused = 0;

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let nord_bg = Color::Rgb(46, 52, 64);
        assert_eq!(terminal.backend().buffer()[(2, 2)].bg, nord_bg);

        // Previewing the first built-in preset must repaint the pane and the
        // picker dialog, not leave either on the terminal's default background.
        app.theme_config = eix_core::ThemeConfig::default();
        app.theme = Theme::default();
        app.open_picker(PickerKind::Themes, &tx);
        app.handle_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_eq!(app.theme.preset, Some("github-dark"));
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let preview_bg = Color::Rgb(13, 17, 23);
        assert_eq!(terminal.backend().buffer()[(2, 2)].bg, preview_bg);
        // The centered picker's border and body should use the previewed
        // palette, not the terminal's default background.
        assert_eq!(terminal.backend().buffer()[(8, 4)].bg, preview_bg);
        assert_eq!(terminal.backend().buffer()[(10, 7)].bg, preview_bg);
    }

    #[test]
    fn theme_picker_enter_commits_the_preview() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.open_picker(PickerKind::Themes, &tx);
        app.handle_picker_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_eq!(app.theme.preset, Some("github-dark"));

        app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(app.picker.is_none());
        assert!(app.theme_picker_backup.is_none());
        assert_eq!(app.theme_config.preset.as_deref(), Some("github-dark"));
        assert_eq!(app.theme.preset, Some("github-dark"));
        assert!(app.status.contains("theme → github-dark"));
    }

    #[test]
    fn help_overlay_filters_and_selects_key_entries() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.open_help();

        for ch in "approval".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &tx);
        }
        assert!(app.help);
        assert_eq!(app.help_query, "approval");
        let count = help_entry_count(&app.help_query);
        assert!(count >= 2, "{count}");

        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_eq!(app.help_selected, 1);
        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE), &tx);
        assert_eq!(app.help_selected, count - 1);

        // Plain q/j/k are search input, not shortcuts.
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &tx);
        assert!(app.help);
        assert_eq!(app.help_query, "approvalq");

        app.handle_key(
            KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(app.help_query.is_empty());
        assert_eq!(app.help_selected, 0);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(!app.help);
    }

    #[test]
    fn help_search_renders_the_selected_match() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.open_help();
        for ch in "approvals".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &tx);
        }

        let text = render_text(&mut app, 120, 40);
        assert!(text.contains("approvals inbox"), "{text}");
        assert!(!text.contains("prompt commands"), "{text}");
        assert!(text.contains('›'), "{text}");
    }

    #[test]
    fn command_palette_filters_across_label_value_and_detail() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.open_picker(PickerKind::Commands, &tx);
        for c in "git worktrees".chars() {
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
    fn banner_helpers_are_compact() {
        assert_eq!(
            short_model("deepseek/deepseek-v4.1-flash"),
            "deepseek-v4.1-flash"
        );
        assert_eq!(short_model("gpt-4o"), "gpt-4o");
        assert_eq!(compact_path("/tmp/x"), "/tmp/x");
        if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
            let home = home.to_string_lossy();
            assert_eq!(compact_path(&format!("{home}/git/eix")), "~/git/eix");
        }
    }

    #[test]
    fn prompt_height_grows_for_long_drafts() {
        let mut input = Editor::new();
        // Short drafts keep the minimum 6 editor rows + 2 borders.
        assert_eq!(prompt_input_height(40, 80, &input), 8);
        assert_eq!(prompt_input_height(100, 80, &input), 8);

        // A long command grows the box so wrapped rows stay visible, but the
        // cap leaves the transcript room and short terminals still win.
        input.set_text(&"x".repeat(1_000));
        assert_eq!(prompt_input_height(40, 80, &input), 14);
        assert_eq!(prompt_input_height(12, 80, &input), 7);
        assert_eq!(prompt_input_height(8, 80, &input), 3);
    }

    #[test]
    fn esc_keeps_focus_in_the_prompt_until_a_double_press() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.focus = Focus::Input;

        // Leaving insert mode does not arm the gesture.
        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(matches!(app.focus, Focus::Input));
        assert_eq!(app.input.mode(), Mode::Normal);
        assert!(app.esc_at.is_none());

        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(matches!(app.focus, Focus::Input));
        assert!(app.esc_at.is_some());

        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(matches!(app.focus, Focus::Transcript));
        assert!(app.esc_at.is_none());
    }

    #[test]
    fn colon_in_prompt_normal_mode_opens_the_palette() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.focus_input();

        // Insert mode still types a literal colon.
        app.handle_input_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        assert!(app.picker.is_none());
        assert_eq!(app.input.text(), ":");

        // In normal mode `:` opens the interactive command palette instead of
        // editing the draft, matching the transcript binding.
        app.input.clear();
        app.input.enter_normal();
        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Commands)
        ));
    }

    #[test]
    fn ctrl_c_interrupts_insert_mode_without_quitting() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.focus_input();
        assert_eq!(app.input.mode(), Mode::Insert);

        app.handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(!app.should_quit);
        assert!(matches!(app.focus, Focus::Input));
        assert_eq!(app.input.mode(), Mode::Normal);
    }

    #[test]
    fn ctrl_c_still_quits_outside_the_prompt() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.focus = Focus::Transcript;

        app.handle_key(
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(app.should_quit);
    }

    #[test]
    fn other_keys_break_the_double_esc_gesture() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.focus = Focus::Input;

        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        app.handle_input_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &tx);
        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(matches!(app.focus, Focus::Input));
    }

    #[test]
    fn transcript_leader_matches_the_prompt_leader() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.views.push(SessionView::new(
            Session::new("s2", "two", "/tmp"),
            Vec::new(),
        ));
        app.focus = Focus::Transcript;

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &tx);
        assert!(app.picker.is_none());
        match rx.try_recv().expect("create command") {
            Command::Create { worktree, .. } => assert!(!worktree),
            _ => panic!("expected a create command"),
        }

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &tx);
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Commands)
        ));

        app.picker = None;
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE), &tx);
        assert!(app.help);
    }

    #[test]
    fn transcript_pane_requires_leader_for_pane_actions() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;
        app.focus = Focus::Transcript;

        // Bare letters are inert: no pane action, no accidental quit.
        for ch in ['o', 'p', 'n', 'N', 'c', 'w', 'b', 'x', 'a', 'q'] {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &tx);
            assert!(app.picker.is_none(), "bare {ch} opened a picker");
            assert!(app.process_view.is_none(), "bare {ch} opened processes");
            assert!(app.ledger.is_none(), "bare {ch} opened the ledger");
            assert!(!app.should_quit, "bare {ch} quit the app");
        }
        assert!(!app.creating, "bare n created a session");
        assert!(rx.try_recv().is_err(), "bare keys sent a command");

        // `i` is the vim-style exception: it focuses the prompt in insert mode.
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &tx);
        assert!(matches!(app.focus, Focus::Input));
        assert_eq!(app.input.mode(), Mode::Insert);
        app.focus = Focus::Transcript;

        // The leader retains the action.
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &tx);
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::NewPane)
        ));

        app.picker = None;
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE), &tx);
        assert!(app.process_view.is_some());

        // Shift+`:` stays available as the command palette shortcut.
        app.process_view = None;
        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::SHIFT), &tx);
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Commands)
        ));

        // Focus and quit also live behind the leader now.
        app.picker = None;
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &tx);
        assert!(matches!(app.focus, Focus::Input));
        app.focus = Focus::Transcript;
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::NONE), &tx);
        assert!(app.should_quit);
    }

    #[test]
    fn leader_n_creates_a_session_and_q_closes_a_pane() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
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
        app.focus_input();
        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &tx);
        assert!(app.picker.is_none());
        match rx.try_recv().expect("create command") {
            Command::Create { worktree, .. } => assert!(!worktree),
            _ => panic!("expected a create command"),
        }

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &tx);
        assert_eq!(app.views.len(), 1);
    }

    #[test]
    fn leader_c_closes_the_focused_pane() {
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
        app.focus = Focus::Transcript;

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('C'), KeyModifiers::NONE), &tx);

        assert_eq!(app.views.len(), 1);
        assert!(
            app.picker.is_none(),
            "leader C must not open the context picker"
        );
    }

    #[test]
    fn shifted_angle_brackets_cycle_panes() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        for (id, name) in [("s1", "one"), ("s2", "two"), ("s3", "three")] {
            app.views
                .push(SessionView::new(Session::new(id, name, "/tmp"), Vec::new()));
        }
        app.focused = 0;
        app.focus = Focus::Transcript;

        for expected in [1, 2, 0] {
            app.handle_key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::SHIFT), &tx);
            assert_eq!(app.focused, expected);
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('<'), KeyModifiers::SHIFT), &tx);
        assert_eq!(app.focused, 2);

        // The prompt's normal mode uses the same bindings; insert mode does not.
        app.focus_input();
        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('>'), KeyModifiers::SHIFT), &tx);
        assert_eq!(app.focused, 0);
        app.handle_key(KeyEvent::new(KeyCode::Char('<'), KeyModifiers::SHIFT), &tx);
        assert_eq!(app.focused, 2);
    }

    #[test]
    fn leader_o_opens_the_new_pane_picker() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.focus_input();
        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE), &tx);
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::NewPane)
        ));
    }

    #[test]
    fn picker_accepts_ctrl_j_and_ctrl_k() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.sessions = vec![
            Session::new("s1", "one", "/tmp"),
            Session::new("s2", "two", "/tmp"),
            Session::new("s3", "three", "/tmp"),
        ];
        app.open_picker(PickerKind::Sessions, &tx);
        assert_eq!(app.picker.as_ref().unwrap().selected, 0);

        app.handle_picker_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.picker.as_ref().unwrap().selected, 1);
        app.handle_picker_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.picker.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn prompt_header_keeps_build_and_plan_aligned() {
        fn header(app: &App) -> String {
            app.header_spans()
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        }

        let mut app = App::new(None);
        app.connected = true;
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;
        app.views[0].session.model = Some("llmgate/deepseek-v4-flash".into());

        let build = header(&app);
        assert_eq!(build, " Build · ○ deepseek-v4-flash ");
        assert!(!build.contains("eix"));

        app.views[0].session.agent = Some("plan".into());
        let plan = header(&app);
        assert_eq!(plan, " Plan  · ○ deepseek-v4-flash ");
        // Padding Plan to Build's width keeps the status and model in place.
        assert_eq!(build.len(), plan.len());
        assert_eq!(build.find('○'), plan.find('○'));
        assert_eq!(build.find("deepseek"), plan.find("deepseek"));
    }

    #[test]
    fn prompt_header_uses_icon_for_idle_and_animated_running() {
        let mut app = App::new(None);
        assert_eq!(app.status_glyph(SessionStatus::Idle), "○");
        assert_eq!(app.status_glyph(SessionStatus::Running), "⠋");
        app.tick = 1;
        assert_eq!(app.status_glyph(SessionStatus::Running), "⠙");
        assert_eq!(
            app.status_glyph(SessionStatus::AwaitingApproval),
            "⚠ awaiting_approval"
        );
    }

    #[test]
    fn prompt_header_shows_git_branch_and_worktree() {
        let mut app = App::new(None);
        let cwd = std::path::PathBuf::from("/tmp/eix-repo");
        let mut session = Session::new("s1", "one", cwd.clone());
        session.worktree = Some(cwd.join(".worktrees/feat"));
        app.views.push(SessionView::new(session, Vec::new()));
        app.focused = 0;
        app.git_cwd = Some(cwd);
        app.git_branch = Some("eix/feat".into());
        app.git_checked = Some(Instant::now());

        let text: String = app
            .header_meta_spans()
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("eix/feat"), "{text}");
        assert!(text.contains("wt:feat"), "{text}");

        // A cached branch for another cwd must not leak into this session.
        app.git_cwd = Some(std::path::PathBuf::from("/tmp/other"));
        let text: String = app
            .header_meta_spans()
            .iter()
            .map(|span| span.content.as_ref())
            .collect();
        assert!(!text.contains("eix/feat"), "{text}");
    }

    #[test]
    fn prompt_normal_mode_tab_toggles_build_and_plan() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;
        app.focus_input();
        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
        assert_eq!(app.views[0].session.agent.as_deref(), Some("plan"));
        assert_eq!(app.status, "Plan");
        match rx.try_recv().expect("set plan agent") {
            Command::SetAgent { session_id, agent } => {
                assert_eq!(session_id, "s1");
                assert_eq!(agent.as_deref(), Some("plan"));
            }
            _ => panic!("unexpected command"),
        }

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
        assert_eq!(app.views[0].session.agent, None);
        assert_eq!(app.status, "Build");
        match rx.try_recv().expect("set build agent") {
            Command::SetAgent { session_id, agent } => {
                assert_eq!(session_id, "s1");
                assert_eq!(agent, None);
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn prompt_normal_mode_ctrl_j_and_k_browse_submitted_history() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        let events = vec![
            SessionEvent::new(
                "s1",
                1,
                EventKind::UserMessage,
                serde_json::json!({ "text": "first" }),
            ),
            SessionEvent::new(
                "s1",
                2,
                EventKind::UserMessage,
                serde_json::json!({ "text": "second" }),
            ),
        ];
        app.views
            .push(SessionView::new(Session::new("s1", "one", "/tmp"), events));
        app.focused = 0;
        app.focus_input();
        app.input.set_text("draft");
        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);

        // Ctrl-K recalls newest first; the live draft is restored by Ctrl-J.
        app.handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.input.text(), "second");
        app.handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.input.text(), "first");
        app.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.input.text(), "second");
        app.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.input.text(), "draft");
        assert!(app.history_index.is_none());
    }

    #[test]
    fn leader_actions_dispatch_like_palette_commands() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);

        app.run_action("help", &tx);
        assert!(app.help);

        app.help = false;
        app.run_action("sessions", &tx);
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Sessions)
        ));

        app.picker = None;
        app.run_action("not-an-action", &tx);
        assert!(app.picker.is_none());
    }

    fn input_app() -> (
        App,
        UnboundedSender<Command>,
        tokio::sync::mpsc::UnboundedReceiver<Command>,
    ) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "ask", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;
        app.focus = Focus::Transcript;
        app.inputs.insert(
            "s1".to_string(),
            PendingInput {
                input_id: "i1".to_string(),
                call_id: "c1".to_string(),
                question: "Which database?".to_string(),
                options: vec![
                    InputOption {
                        label: "SQLite".to_string(),
                        description: Some("embedded".to_string()),
                    },
                    InputOption {
                        label: "Postgres".to_string(),
                        description: None,
                    },
                    InputOption {
                        label: "MySQL".to_string(),
                        description: None,
                    },
                ],
                cursor: 0,
            },
        );
        (app, tx, rx)
    }

    #[test]
    fn input_keys_pick_options_and_enter_answers() {
        let (mut app, tx, mut rx) = input_app();
        let key = |code, modifiers| KeyEvent::new(code, modifiers);

        app.handle_transcript_key(key(KeyCode::Char('j'), KeyModifiers::NONE), &tx);
        assert_eq!(app.focused_input().expect("pending").cursor, 1);
        app.handle_transcript_key(key(KeyCode::Char('p'), KeyModifiers::CONTROL), &tx);
        assert_eq!(app.focused_input().expect("pending").cursor, 0);
        app.handle_transcript_key(key(KeyCode::Char('n'), KeyModifiers::CONTROL), &tx);
        assert_eq!(app.focused_input().expect("pending").cursor, 1);

        app.handle_transcript_key(key(KeyCode::Enter, KeyModifiers::NONE), &tx);
        match rx.try_recv().expect("input command") {
            Command::Input {
                session_id,
                input_id,
                option_index,
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(input_id, "i1");
                assert_eq!(option_index, 1);
            }
            _ => panic!("expected an input command"),
        }
    }

    #[test]
    fn number_keys_answer_without_moving_the_cursor() {
        let (mut app, tx, mut rx) = input_app();
        app.handle_transcript_key(KeyEvent::new(KeyCode::Char('3'), KeyModifiers::NONE), &tx);
        match rx.try_recv().expect("input command") {
            Command::Input { option_index, .. } => assert_eq!(option_index, 2),
            _ => panic!("expected an input command"),
        }
    }

    #[test]
    fn pending_input_replays_from_events() {
        let events = vec![SessionEvent::new(
            "s1",
            1,
            EventKind::InputRequired,
            eix_core::payload(&InputRequiredPayload {
                input_id: "i1".to_string(),
                call_id: "c1".to_string(),
                question: "Which?".to_string(),
                options: vec![
                    InputOption {
                        label: "A".to_string(),
                        description: None,
                    },
                    InputOption {
                        label: "B".to_string(),
                        description: None,
                    },
                ],
            }),
        )];
        let view = SessionView::new(Session::new("s1", "ask", "/tmp"), events);
        let pending = view.pending_input().expect("pending question");
        assert_eq!(pending.question, "Which?");
        assert_eq!(pending.options.len(), 2);
        assert_eq!(pending.cursor, 0);
    }

    #[test]
    fn approval_actions_lead_with_the_recommendation() {
        let mut approval = PendingApproval {
            kind: PendingKind::Tool,
            approval_id: "a1".to_string(),
            call_id: "c1".to_string(),
            tool: "run_command".to_string(),
            summary: "run_command: ls".to_string(),
            diff: None,
            path: None,
            hunks: Vec::new(),
            selected: Vec::new(),
            cursor: 0,
        };
        assert_eq!(
            approval.actions(),
            vec![
                ApprovalAction::Approve,
                ApprovalAction::Always,
                ApprovalAction::Deny,
                ApprovalAction::Reason
            ]
        );
        approval.path = Some("src/main.rs".to_string());
        approval.diff = Some("@@ -1 +1 @@".to_string());
        assert_eq!(
            approval.actions(),
            vec![
                ApprovalAction::Approve,
                ApprovalAction::Always,
                ApprovalAction::Review,
                ApprovalAction::Open,
                ApprovalAction::Deny,
                ApprovalAction::Reason,
            ]
        );
        assert_eq!(approval.action_label(ApprovalAction::Approve), "Approve");
        approval.kind = PendingKind::Patch;
        assert_eq!(
            approval.action_label(ApprovalAction::Approve),
            "Apply change"
        );
    }

    #[test]
    fn enter_approves_the_recommended_action() {
        let (mut app, tx, mut rx) = input_app();
        app.inputs.clear();
        app.approvals.insert(
            "s1".to_string(),
            PendingApproval {
                kind: PendingKind::Tool,
                approval_id: "a1".to_string(),
                call_id: "c1".to_string(),
                tool: "run_command".to_string(),
                summary: "run_command: ls".to_string(),
                diff: None,
                path: None,
                hunks: Vec::new(),
                selected: Vec::new(),
                cursor: 0,
            },
        );
        app.handle_transcript_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        match rx.try_recv().expect("approval command") {
            Command::Approval { decision, .. } => {
                assert_eq!(decision, ApprovalDecision::Approve)
            }
            _ => panic!("expected an approval command"),
        }
    }

    fn session_pane(id: &str, name: &str) -> SessionView {
        SessionView::new(Session::new(id, name, "/tmp"), Vec::new())
    }

    #[test]
    fn slash_exit_saves_and_closes_the_focused_pane() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.views.push(session_pane("s2", "two"));
        app.focused = 0;
        app.focus = Focus::Input;
        app.input.set_text("/exit");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert!(!app.should_quit, "another pane is still open");
        assert_eq!(app.views.len(), 1);
        assert_eq!(app.views[0].session.id, "s2");
        assert!(app.input.is_empty(), "the command line is cleared");
        match rx.try_recv().expect("draft save") {
            Command::DraftSet { session_id, text } => {
                assert_eq!(session_id, "s1");
                assert!(text.is_empty(), "the command is not kept as a draft");
            }
            _ => panic!("expected a draft save"),
        }
    }

    #[test]
    fn slash_close_saves_and_closes_the_focused_pane() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.views.push(session_pane("s2", "two"));
        app.focused = 0;
        app.focus = Focus::Input;
        app.input.set_text("/close");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert!(!app.should_quit, "another pane is still open");
        assert_eq!(app.views.len(), 1);
        assert_eq!(app.views[0].session.id, "s2");
        assert!(app.input.is_empty(), "the command line is cleared");
        match rx.try_recv().expect("draft save") {
            Command::DraftSet { session_id, text } => {
                assert_eq!(session_id, "s1");
                assert!(text.is_empty(), "the command is not kept as a draft");
            }
            _ => panic!("expected a draft save"),
        }
    }

    #[test]
    fn slash_close_keeps_the_last_pane_open() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.focused = 0;
        app.focus = Focus::Input;
        app.input.set_text("/close");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert!(!app.should_quit);
        assert_eq!(app.views.len(), 1);
        assert!(app.status.contains("cannot close the last pane"));
        match rx.try_recv().expect("draft save") {
            Command::DraftSet { session_id, .. } => assert_eq!(session_id, "s1"),
            _ => panic!("expected a draft save"),
        }
    }

    #[test]
    fn slash_quit_on_the_last_pane_quits_after_saving() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.focused = 0;
        app.focus = Focus::Input;
        app.input.set_text("/quit");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert!(app.should_quit, "the last pane quits the app");
        assert_eq!(app.views.len(), 1);
        match rx.try_recv().expect("draft save") {
            Command::DraftSet { session_id, text } => {
                assert_eq!(session_id, "s1");
                assert!(text.is_empty());
            }
            _ => panic!("expected a draft save"),
        }
    }

    #[test]
    fn submitting_while_busy_queues_and_flushes_when_idle() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.focused = 0;
        app.focus = Focus::Input;
        app.views[0].session.status = SessionStatus::Running;
        app.input.set_text("next prompt");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert_eq!(app.queue_len("s1"), 1);
        assert!(app.input.is_empty());
        let mut submitted = false;
        while let Ok(command) = rx.try_recv() {
            if matches!(command, Command::Submit { .. }) {
                submitted = true;
            }
        }
        assert!(!submitted, "busy sessions must not receive a submit");

        app.views[0].session.status = SessionStatus::Idle;
        app.tick(&tx);
        let mut text = None;
        while let Ok(command) = rx.try_recv() {
            if let Command::Submit { text: value, .. } = command {
                text = Some(value);
            }
        }
        assert_eq!(text.as_deref(), Some("next prompt"));
        assert!(app.submitting);
        assert_eq!(app.queue_len("s1"), 0);
    }

    #[test]
    fn a_busy_race_requeues_the_inflight_prompt() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.focused = 0;
        app.views[0].session.status = SessionStatus::Running;
        app.input.set_text("queued");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        app.views[0].session.status = SessionStatus::Idle;
        app.tick(&tx);
        while rx.try_recv().is_ok() {}

        app.handle_event(
            Event::Error {
                context: "submit".into(),
                message: "session s1 is already running".into(),
            },
            &tx,
        );
        assert!(!app.submitting);
        assert_eq!(app.queue_len("s1"), 1);

        app.tick(&tx);
        let mut text = None;
        while let Ok(command) = rx.try_recv() {
            if let Command::Submit { text: value, .. } = command {
                text = Some(value);
            }
        }
        assert_eq!(text.as_deref(), Some("queued"));
    }

    #[test]
    fn a_stale_busy_error_queues_the_prompt() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.focused = 0;
        app.focus = Focus::Input;
        app.input.set_text("retry me");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        while rx.try_recv().is_ok() {}

        // The TUI thought the session was idle, but the daemon was busy.
        app.handle_event(
            Event::Error {
                context: "submit".into(),
                message: "busy: session s1 is already running".into(),
            },
            &tx,
        );
        assert_eq!(app.queue_len("s1"), 1);
        assert!(app.input.is_empty());
        assert!(app.error.is_none());
        assert!(!app.submitting);
    }

    #[test]
    fn double_esc_in_normal_mode_interrupts_a_running_turn() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.focused = 0;
        app.views[0].session.status = SessionStatus::Running;
        app.focus_input();
        let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        app.handle_input_key(esc, &tx); // insert -> normal
        app.handle_input_key(esc, &tx); // arm
        app.handle_input_key(esc, &tx); // interrupt
        let mut cancelled = None;
        while let Ok(command) = rx.try_recv() {
            if let Command::Cancel(id) = command {
                cancelled = Some(id);
            }
        }
        assert_eq!(cancelled.as_deref(), Some("s1"));
        assert!(matches!(app.focus, Focus::Input));
    }

    #[test]
    fn slash_commands_dispatch_requests_and_clear_the_prompt() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.focused = 0;
        app.focus = Focus::Input;

        app.input.set_text("/clear");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(app.input.is_empty());
        let mut cleared = false;
        while let Ok(command) = rx.try_recv() {
            if let Command::ClearContext { session_id } = command {
                assert_eq!(session_id, "s1");
                cleared = true;
            }
        }
        assert!(cleared);

        app.input.set_text("/cd /tmp");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        let mut cwd = None;
        while let Ok(command) = rx.try_recv() {
            if let Command::SetCwd { path, .. } = command {
                cwd = Some(path);
            }
        }
        assert_eq!(cwd.as_deref(), Some("/tmp"));

        app.input.set_text("/compact tighten it");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        let mut notes = None;
        while let Ok(command) = rx.try_recv() {
            if let Command::CompactContext { notes: value, .. } = command {
                notes = value;
            }
        }
        assert_eq!(notes.as_deref(), Some("tighten it"));

        app.input.set_text("/btw why is the sky blue?");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        let mut question = None;
        while let Ok(command) = rx.try_recv() {
            if let Command::Btw {
                question: value, ..
            } = command
            {
                question = Some(value);
            }
        }
        assert_eq!(question.as_deref(), Some("why is the sky blue?"));
        assert!(app.btw.is_some());
        app.btw = None;

        app.input.set_text("/help");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(app.help);

        app.input.set_text("/init");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(app.input.text().contains("AGENTS.md"));
        let mut submitted = false;
        while let Ok(command) = rx.try_recv() {
            if matches!(command, Command::Submit { .. }) {
                submitted = true;
            }
        }
        assert!(!submitted, "/init only stages a prompt");
    }

    #[test]
    fn command_menu_filters_completes_and_runs() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.focus_input();
        app.input.set_text("/cl");
        assert!(app.command_menu_open());
        let names: Vec<&str> = app
            .command_matches()
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
        assert!(names.contains(&"clear"), "{names:?}");
        assert!(names.iter().all(|name| name.starts_with("cl")), "{names:?}");
        app.complete_command();
        assert_eq!(app.input.text(), "/clear");

        app.input.set_text("/hel");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(app.help, "Enter runs the highlighted command");
    }

    fn render_text(app: &mut App, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn prompt_grows_to_show_a_long_command() {
        let mut app = App::new(None);
        app.focus = Focus::Input;
        app.input
            .set_text(&format!("echo {} ENDMARK", "x".repeat(500)));

        let rendered = render_text(&mut app, 80, 40);
        assert!(rendered.contains("echo"), "{rendered}");
        assert!(rendered.contains("ENDMARK"), "{rendered}");
    }

    #[test]
    fn prompt_is_a_minimal_styled_panel_with_command_menu() {
        let mut app = App::new(None);
        let mut session = Session::new("s1", "prompt polish", "/home/shiyi/git/eix");
        session.model = Some("openai/gpt-5".into());
        session.status = SessionStatus::Running;
        app.views.push(SessionView::new(session, Vec::new()));
        app.focused = 0;
        app.focus = Focus::Input;
        app.connected = true;

        let empty = render_text(&mut app, 80, 20);
        assert!(empty.contains("Build · ⠋ gpt-5"), "{empty}");
        assert!(!empty.contains("eixd"), "no footer statusline: {empty}");
        assert!(!empty.contains("/cmds"), "no footer hints: {empty}");
        assert!(empty.contains("1● prompt polish"), "{empty}");
        assert!(
            empty.contains('╭'),
            "the composer keeps its border: {empty}"
        );
        assert!(
            empty.contains('╮'),
            "the composer keeps its border: {empty}"
        );
        assert!(!empty.contains("❯"), "{empty}");
        assert!(empty.contains(" I "), "{empty}");
        assert!(!empty.contains("INSERT"), "{empty}");
        assert!(empty.contains("~/git/eix"), "{empty}");
        assert!(!empty.contains("Message eix…"), "{empty}");
        assert!(!empty.contains("C-j newline"), "{empty}");
        assert!(!empty.contains("Esc normal"), "{empty}");

        app.input.set_text("/cl");
        let menu = render_text(&mut app, 80, 20);
        assert!(menu.contains("/ commands"), "{menu}");
        assert!(menu.contains("❯ /clear"), "{menu}");
        assert!(!menu.contains("Tab complete"), "{menu}");
    }

    #[test]
    fn default_theme_keeps_the_prompt_transparent() {
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.focused = 0;
        app.focus = Focus::Input;
        app.connected = true;
        assert!(app.theme.background.is_none());
        assert_eq!(composer_background(app.theme), Color::Reset);
        assert_eq!(dialog_background(app.theme), Color::Reset);

        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                assert_ne!(
                    buffer[(x, y)].bg,
                    Color::Rgb(30, 33, 42),
                    "prompt kept an opaque panel at ({x}, {y})"
                );
            }
        }
    }

    #[test]
    fn prompt_errors_render_in_a_dialog_not_the_input_header() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.focused = 0;
        app.focus = Focus::Input;
        app.connected = true;
        app.error = Some("provider returned 500".into());

        let text = render_text(&mut app, 80, 20);
        assert!(text.contains("Build ·"), "{text}");
        assert!(!text.contains("error: provider returned 500"), "{text}");
        assert!(text.contains(" error "), "{text}");
        assert!(text.contains("provider returned 500"), "{text}");

        // The dialog is modal; typing does not reach the editor.
        let mode = app.input.mode();
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &tx);
        assert!(app.error.is_some());
        assert_eq!(app.input.mode(), mode);

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(app.error.is_none());
        let text = render_text(&mut app, 80, 20);
        assert!(!text.contains("provider returned 500"), "{text}");
    }

    #[test]
    fn a_message_mentioning_slash_exit_is_still_sent() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.focused = 0;
        app.focus = Focus::Input;
        app.input.set_text("please run /exit later");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert!(!app.should_quit);
        assert_eq!(app.views.len(), 1);
        match rx.try_recv().expect("submit") {
            Command::Submit { text, .. } => assert_eq!(text, "please run /exit later"),
            _ => panic!("expected a submit"),
        }
    }
}
