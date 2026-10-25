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
    Event as SessionEvent, EventKind, HelloResult, HunkInfo, InputAnsweredPayload, InputOption,
    InputRequiredPayload, IntegrationState, IntegrationStatus, ModelInfo, PatchProposedPayload,
    PatchResultPayload, PickerItem, ProcessInfo, ProcessList, ProcessOutput, ProcessStatus,
    Session, SessionMode, SessionStatus, SubmitResult, TodoItem, ToolResultPayload,
    UserMessagePayload, THEME_PRESETS,
};
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::border::Set as BorderSet;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, BorderType, Borders, Clear, List, ListItem, ListState, Padding, Paragraph, Wrap,
};
use ratatui::Frame;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::controller::{Command, Event};
use super::editor::{leader_action, Editor, Mode, Outcome};
use super::keys::{KeyAction, Keybinds};
use super::theme::Theme;
use super::transcript::{self, FoldId, Subagent, SubagentStatus};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Focus {
    Transcript,
    Input,
}

/// Two `Esc` presses within this window interrupt the focused turn (or leave
/// the prompt for the transcript when idle). Keep it forgiving: three presses
/// (insert → normal → interrupt) can take a moment.
const ESC_WINDOW: Duration = Duration::from_millis(1000);

/// Two `Ctrl-W` presses within this window focus the next pane.
const CTRL_W_WINDOW: Duration = Duration::from_millis(1000);

/// Background-attention alerts stay centered in the status bar this long.
const ALERT_DISPLAY: Duration = Duration::from_secs(8);

/// Keep the Vim `:` history bounded for long-running clients.
const CMDLINE_HISTORY_LIMIT: usize = 100;

/// Picker value for "no preset" (keep the terminal/default palette).
const THEME_DEFAULT_VALUE: &str = "@default";

/// Running sessions rotate a quadrant square clockwise; idle sessions show `⠶`.
const RUNNING_SPINNER: [&str; 4] = ["◰", "◳", "◲", "◱"];

/// "Bypass" is the longest execution-mode label. Pad every mode badge to this
/// width so the rest of the statusline never moves when the mode changes.
const SESSION_MODE_WIDTH: usize = 6;

/// Re-read the focused session's git branch at this interval.
const GIT_BRANCH_REFRESH: Duration = Duration::from_secs(5);

/// Start loading the previous history page when the viewport is this close to
/// the top, and wait until it scrolls further down before arming another page.
const HISTORY_PRELOAD_LINES: usize = 3;
const HISTORY_REARM_LINES: usize = 12;

/// Scrolling down to within this many lines of the tail resumes auto-follow.
/// It gives the user a little slack without making a deliberate pause jump.
const FOLLOW_SNAP_LINES: usize = 2;

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

/// Old slash-command spellings that stay accepted, mapped to the canonical
/// name. Aliases are transparent in the menu: typing one shows only its
/// canonical command, so `/mode` no longer competes with `/model`.
const COMMAND_ALIASES: &[(&str, &str)] = &[("mode", "perm")];

fn command_alias(name: &str) -> Option<&'static str> {
    COMMAND_ALIASES
        .iter()
        .find(|(alias, _)| alias.eq_ignore_ascii_case(name))
        .map(|(_, canonical)| *canonical)
}

/// Built-in prompt commands, in menu order.
fn builtin_commands() -> Vec<CommandSpec> {
    vec![
        builtin("help", "[query]", "search the key and command reference"),
        builtin("keys", "[query]", "search just the key map"),
        builtin("new", "[name]", "start a new session in this workspace"),
        builtin("new-pane", "", "open a session in a new pane"),
        builtin(
            "new-worktree",
            "[name]",
            "start a session in a new git worktree",
        ),
        builtin("new-shared", "[name]", "start a session in this workspace"),
        builtin(
            "ready",
            "",
            "queue this worktree for rebase, tests and merge",
        ),
        builtin("integrate", "", "show this session's integration state"),
        builtin("files", "", "browse and insert workspace files"),
        builtin("worktrees", "", "browse and insert git worktrees"),
        builtin("branches", "", "browse and insert git branches"),
        builtin("processes", "", "list and inspect background processes"),
        builtin("approvals", "", "open the approval inbox"),
        builtin("attention", "", "jump to the next pane needing attention"),
        builtin("cancel", "", "cancel the focused session's running turn"),
        builtin(
            "toggle-mode",
            "",
            "cycle Plan / Build / Auto / Bypass (Shift-Tab reverses)",
        ),
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
        builtin(
            "model",
            "[id]",
            "search providers/models and switch this session",
        ),
        builtin("agents", "[name]", "choose the session's primary agent"),
        builtin("subagents", "[n]", "set the max parallel spawn_agent calls"),
        builtin("mcp", "", "browse MCP servers and their tools"),
        builtin("reload", "", "re-read the daemon config files"),
        builtin(
            "timeline",
            "",
            "search this session's user input history and jump",
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
        builtin("todos", "", "browse the model's task list"),
        builtin("image", "<path>", "attach an image to the next prompt"),
        builtin("colorscheme", "", "choose a colorscheme with live preview"),
        builtin(
            "perm",
            "[plan|build|auto|bypass]",
            "choose the session permission mode in a search picker",
        ),
        builtin("fork", "[name]", "fork this session into a new pane"),
        builtin("commands", "", "reload user-defined Markdown commands"),
        builtin("exit", "", "save the draft and quit eix"),
        builtin("quit", "", "save the draft and quit eix"),
        builtin(
            "close",
            "",
            "save the draft and close this session (quits on the last pane)",
        ),
    ]
}

/// Merge built-ins with user commands. A custom command with the same name
/// replaces the built-in entry rather than creating a duplicate.
fn merge_commands(cwd: &std::path::Path, compat: &eix_core::CompatConfig) -> Vec<CommandSpec> {
    let mut commands = builtin_commands();
    let custom: Vec<_> = eix_core::load_commands(cwd, compat);
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

/// Prompt height: at least [`super::editor::PROMPT_ROWS`] content rows plus the
/// two borders. Long drafts grow the box up to twelve total rows before the
/// editor starts scrolling, and short terminals always keep transcript rows.
fn prompt_input_height(area_height: u16, area_width: u16, input: &Editor) -> u16 {
    // Ten editor rows + two borders = twelve rows total.
    const PROMPT_MAX_ROWS: usize = 10;
    let inner_width = area_width.saturating_sub(4).max(1) as usize;
    let rows = input.layout(inner_width, 1).segments.len();
    let visible = rows.clamp(super::editor::PROMPT_ROWS as usize, PROMPT_MAX_ROWS) as u16;
    (visible + 2).min(area_height.saturating_sub(5).max(4))
}

// Keep the prompt readable without a separate panel: its chrome paints over
// the theme canvas (or the terminal background) instead of an opaque composer
// box.
const COMPOSER_TEXT: Color = Color::Rgb(220, 223, 228);
const COMPOSER_MUTED: Color = Color::Rgb(110, 116, 129);
const COMPOSER_BORDER: Color = Color::Rgb(74, 88, 120);
const COMPOSER_ACCENT: Color = Color::Rgb(122, 162, 247);
const COMPOSER_FOCUS: Color = Color::Rgb(154, 202, 255);
const COMPOSER_SUCCESS: Color = Color::Rgb(126, 199, 149);
const COMPOSER_WARN: Color = Color::Rgb(229, 192, 123);
const COMPOSER_ORANGE: Color = Color::Rgb(245, 169, 127);
const COMPOSER_DANGER: Color = Color::Rgb(231, 130, 132);
const COMPOSER_BADGE_TEXT: Color = Color::Rgb(20, 22, 28);
const COMPOSER_SELECTION: Color = Color::Rgb(54, 60, 76);

/// Fine dashed focus ring. Quadruple-dash box drawing keeps the focused pane
/// visible without the heavier look of the normal solid border.
const PANE_FOCUS_BORDER: BorderSet = BorderSet {
    top_left: "╭",
    top_right: "╮",
    bottom_left: "╰",
    bottom_right: "╯",
    vertical_left: "┊",
    vertical_right: "┊",
    horizontal_top: "┈",
    horizontal_bottom: "┈",
};

/// A mouse text selection in transcript-line/cell coordinates.
#[derive(Clone, Copy, Debug)]
struct TextSelection {
    anchor: (usize, usize),
    cursor: (usize, usize),
    dragging: bool,
    moved: bool,
}

/// Short-lived top-right confirmation after a mouse selection was copied.
struct CopyToast {
    text: String,
    until: Instant,
}

const COPY_TOAST_DISPLAY: Duration = Duration::from_secs(2);

/// Parsed and wrapped history for one pane. Only the live turn is re-rendered
/// on each frame; stored events stay here until the log, width, theme or fold
/// state changes.
struct HistoryCache {
    width: usize,
    theme: Theme,
    folds: HashMap<FoldId, bool>,
    events_rev: u64,
    subagents_rev: u64,
    lines: Vec<Line<'static>>,
}

struct SessionView {
    session: Session,
    events: Vec<SessionEvent>,
    /// Bumped whenever `events` changes so the render cache can be reused.
    events_rev: u64,
    /// Child agents spawned by this session, keyed by their child session id.
    subagents: HashMap<String, Subagent>,
    /// Bumped whenever a child agent's state changes.
    subagents_rev: u64,
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
    /// Stored user-message sequence -> first rendered line, for `/timeline`.
    user_lines: Vec<(u64, usize)>,
    /// Active mouse selection, if any.
    selection: Option<TextSelection>,
}

impl SessionView {
    fn new(session: Session, events: Vec<SessionEvent>) -> Self {
        Self {
            session,
            events,
            events_rev: 0,
            subagents: HashMap::new(),
            subagents_rev: 0,
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
            user_lines: Vec::new(),
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
                || cache.subagents_rev != self.subagents_rev
        });
        if !stale {
            return;
        }
        let transcript::Transcript {
            lines,
            slots,
            user_lines,
        } = transcript::render_history_with_subagents(
            &self.events,
            &self.folds,
            width,
            theme,
            &self.subagents,
        );
        self.slots = slots;
        self.text_lines = lines.iter().map(text_line).collect();
        self.user_lines = user_lines;
        self.history_cache = Some(HistoryCache {
            width,
            theme,
            folds: self.folds.clone(),
            events_rev: self.events_rev,
            subagents_rev: self.subagents_rev,
            lines,
        });
    }

    fn upsert_subagent(&mut self, agent_id: String, call_id: String, name: String) {
        match self.subagents.get_mut(&agent_id) {
            Some(subagent) => {
                if !call_id.is_empty() {
                    subagent.call_id = call_id;
                }
                if !name.is_empty() {
                    subagent.name = name;
                }
            }
            None => {
                self.subagents
                    .insert(agent_id, Subagent::new(call_id, name));
            }
        }
        self.subagents_rev = self.subagents_rev.wrapping_add(1);
    }

    fn push_subagent_event(&mut self, agent_id: &str, event: SessionEvent) -> bool {
        let Some(subagent) = self.subagents.get_mut(agent_id) else {
            return false;
        };
        subagent.push_event(event);
        self.subagents_rev = self.subagents_rev.wrapping_add(1);
        true
    }

    fn complete_subagent(
        &mut self,
        agent_id: &str,
        call_id: &str,
        name: &str,
        ok: bool,
        result: Option<String>,
        error: Option<String>,
    ) {
        if !self.subagents.contains_key(agent_id) {
            let call_id = call_id.to_string();
            let name = name.to_string();
            if call_id.is_empty() && name.is_empty() {
                return;
            }
            self.subagents
                .insert(agent_id.to_string(), Subagent::new(call_id, name));
        }
        if let Some(subagent) = self.subagents.get_mut(agent_id) {
            subagent.complete(ok, result, error);
            self.subagents_rev = self.subagents_rev.wrapping_add(1);
        }
    }

    /// A parent `spawn_agent` tool result is the final fallback when the
    /// `agent.completed` notification was missed or predates this pane.
    fn finish_subagent_by_call(&mut self, call_id: &str, ok: bool, output: &str) {
        let Some(subagent) = self
            .subagents
            .values_mut()
            .find(|subagent| subagent.call_id == call_id)
        else {
            return;
        };
        if subagent.status == SubagentStatus::Running {
            subagent.complete(ok, None, (!ok).then(|| output.to_string()));
            self.subagents_rev = self.subagents_rev.wrapping_add(1);
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
                            // Shell approvals carry a deliberately short
                            // daemon summary. Keep the real command line so the
                            // card can show it in full instead of approving a
                            // truncated command.
                            let mut summary =
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
                            let risk = payload.risk.to_ascii_uppercase();
                            if !risk.is_empty() {
                                summary = format!("[{risk}] {summary}");
                            }
                            for reason in &payload.reasons {
                                summary.push_str(&format!("\n{reason}"));
                            }
                            if !payload.risk_categories.is_empty() {
                                summary.push_str(&format!(
                                    "\nrisk: {}",
                                    payload.risk_categories.join(", ")
                                ));
                            }
                            if !payload.sandbox.is_empty() {
                                summary.push_str(&format!("\nsandbox: {}", payload.sandbox));
                            }
                            if let Some(rule) = payload.rule.as_deref() {
                                summary
                                    .push_str(&format!("\nrule: {rule} ({})", payload.rule_source));
                            }
                            let path = payload
                                .arguments
                                .get("path")
                                .and_then(serde_json::Value::as_str)
                                .map(str::to_string);
                            if !payload.scopes.is_empty() {
                                summary
                                    .push_str(&format!("\nscopes: {}", payload.scopes.join(", ")));
                            }
                            if payload.scopes.len() <= 1 {
                                summary.push_str(NO_ALWAYS_MARKER);
                            }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// Client-side command palette (`Ctrl-P`).
    Commands,
    /// Pin the focused session to a configured model.
    Models,
    /// Select the focused session's primary agent.
    Agents,
    /// Adjust the daemon's runtime subagent concurrency limit.
    Subagents,
    /// Choose the focused session's execution mode.
    Modes,
    /// Search this session's user-message history and jump to it.
    Timeline,
    /// Browse installed skills, with a body preview.
    Skills,
    /// Preview and apply a color preset.
    Themes,
    /// Configured MCP servers and their tool counts.
    Mcp,
    /// The focused session's current task list.
    Todos,
}

impl PickerKind {
    /// Daemon provider backing this kind, if any.
    fn data_kind(self) -> Option<&'static str> {
        match self {
            Self::Files => Some(eix_core::KIND_FILES),
            Self::Worktrees => Some(eix_core::KIND_WORKTREES),
            Self::Branches => Some(eix_core::KIND_BRANCHES),
            Self::Sessions
            | Self::NewPane
            | Self::Commands
            | Self::Models
            | Self::Agents
            | Self::Subagents
            | Self::Modes
            | Self::Timeline
            | Self::Skills
            | Self::Themes
            | Self::Mcp
            | Self::Todos => None,
        }
    }

    /// Provider used for the row's lazy right-pane preview, if any.
    fn preview_kind(self) -> Option<&'static str> {
        match self {
            Self::Files => Some(eix_core::KIND_FILES),
            Self::Sessions | Self::NewPane => Some(eix_core::KIND_SESSION_PREVIEW),
            _ => None,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Sessions => "sessions — switch focused pane",
            Self::NewPane => "sessions — open in a new pane",
            Self::Files => "files — insert after @",
            Self::Worktrees => "worktrees — insert path",
            Self::Branches => "branches — insert name",
            Self::Commands => "commands",
            Self::Models => "model — search and switch",
            Self::Agents => "agents — choose the primary agent",
            Self::Subagents => "subagents — parallel limit",
            Self::Modes => "mode — choose session execution",
            Self::Timeline => "timeline — user input history",
            Self::Skills => "skills — preview and insert",
            Self::Themes => "colorschemes — live preview",
            Self::Mcp => "mcp — servers and tools",
            Self::Todos => "todos — model task list",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Self::Sessions => "Enter open · ↑/↓ or C-j/C-k/C-n/C-p · Esc close",
            Self::NewPane => "Enter split · ↑/↓ or C-j/C-k/C-n/C-p · Esc close",
            Self::Files => "Enter insert · ↑/↓ or C-j/C-k/C-n/C-p · Esc close",
            Self::Worktrees => "Enter insert path · ↑/↓ or C-j/C-k/C-n/C-p · Esc close",
            Self::Branches => "Enter insert name · ↑/↓ or C-j/C-k/C-n/C-p · Esc close",
            Self::Commands => "Enter run · ↑/↓ or C-j/C-k/C-n/C-p · Esc close",
            Self::Models => "type to search · Enter switch · Esc close",
            Self::Agents => "Enter select · ↑/↓ or C-j/C-k/C-n/C-p · Esc close",
            Self::Subagents => "Enter apply · ↑/↓ or C-j/C-k/C-n/C-p · Esc close",
            Self::Modes => "type to search · Enter apply · Esc close",
            Self::Timeline => "type to search · C-j/C-k or C-n/C-p · Enter jump · Esc close",
            Self::Skills => "Enter insert · preview on the right · Esc close",
            Self::Themes => "Enter apply · ↑/↓ live preview · Esc cancel",
            Self::Mcp => "Enter close · ↑/↓ or C-j/C-k/C-n/C-p select · Esc close",
            Self::Todos => "Enter close · ↑/↓ or C-j/C-k/C-n/C-p select · Esc close",
        }
    }
}

#[derive(Default)]
struct PickerPreview {
    /// Row value the current text/loading state belongs to.
    key: Option<String>,
    text: Option<String>,
    error: Option<String>,
    loading: bool,
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
    /// Lazy right-pane content for the selected file/session.
    preview: PickerPreview,
    /// Request queued by the last draw; the event loop sends it after drawing.
    preview_request: Option<(String, Option<String>, String)>,
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

/// Marker appended to cards where the daemon refuses a persistent "Always"
/// (high/critical risk or a forced ask).
const NO_ALWAYS_MARKER: &str = "\nAlways is unavailable for this call";

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
        let mut actions = vec![ApprovalAction::Approve];
        if !self.summary.starts_with("[HIGH]")
            && !self.summary.starts_with("[CRITICAL]")
            && !self.summary.contains(NO_ALWAYS_MARKER)
        {
            actions.push(ApprovalAction::Always);
        }
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

/// Vim-style `:` command line. The cursor is a character index so editing
/// stays correct for CJK and other multibyte text.
struct CmdLine {
    /// Prompt character: `:` normally, or the normal-mode search key (`/`,
    /// `?`, `#`) that opened this command line.
    prompt: char,
    text: String,
    cursor: usize,
}

impl CmdLine {
    fn new(prompt: char) -> Self {
        Self {
            prompt,
            text: String::new(),
            cursor: 0,
        }
    }

    fn cursor_byte(&self) -> usize {
        self.text
            .char_indices()
            .nth(self.cursor)
            .map(|(index, _)| index)
            .unwrap_or(self.text.len())
    }

    fn insert_char(&mut self, ch: char) {
        let index = self.cursor_byte();
        self.text.insert(index, ch);
        self.cursor += 1;
    }

    fn insert_str(&mut self, text: &str) {
        let text: String = text
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .collect();
        if text.is_empty() {
            return;
        }
        let index = self.cursor_byte();
        self.text.insert_str(index, &text);
        self.cursor += text.chars().count();
    }

    fn set_text(&mut self, text: String) {
        self.cursor = text.chars().count();
        self.text = text;
    }

    fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let end = self.cursor_byte();
        let start = self.text[..end]
            .char_indices()
            .next_back()
            .map(|(index, _)| index)
            .unwrap_or(0);
        self.text.replace_range(start..end, "");
        self.cursor -= 1;
    }

    fn delete(&mut self) {
        let start = self.cursor_byte();
        let Some(ch) = self.text[start..].chars().next() else {
            return;
        };
        self.text.replace_range(start..start + ch.len_utf8(), "");
    }

    fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.text.chars().count());
    }

    fn home(&mut self) {
        self.cursor = 0;
    }

    fn end(&mut self) {
        self.cursor = self.text.chars().count();
    }

    fn kill_to_start(&mut self) {
        let end = self.cursor_byte();
        self.text.replace_range(..end, "");
        self.cursor = 0;
    }

    fn kill_to_end(&mut self) {
        let start = self.cursor_byte();
        self.text.truncate(start);
    }

    fn kill_word(&mut self) {
        let end = self.cursor_byte();
        let mut start = end;
        let before = &self.text[..end];
        for (index, ch) in before.char_indices().rev() {
            if ch.is_whitespace() {
                start = index;
            } else {
                break;
            }
        }
        let trimmed = &self.text[..start];
        for (index, ch) in trimmed.char_indices().rev() {
            if ch.is_whitespace() {
                break;
            }
            start = index;
        }
        self.text.replace_range(start..end, "");
        self.cursor = self.text[..start].chars().count();
    }
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

/// Vim-style search inside the focused session's rendered transcript.
#[derive(Clone, Debug)]
struct TranscriptSearch {
    session_id: String,
    query: String,
    /// Absolute rendered-line indices.
    matches: Vec<usize>,
    /// Index into `matches`.
    current: usize,
    /// Search direction; used only for status text.
    forward: bool,
}

/// Horizontal layout keeps the first pane plus this many following panes visible.
const HORIZONTAL_TAIL_PANES: usize = 4;

/// How open sessions are arranged in the body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaneLayout {
    /// The original automatic tiling.
    Grid,
    /// One column of panes; the first pane gets most of the height and the
    /// following panes compact when the terminal is short.
    Horizontal,
    /// A session tab bar with only the active session below it.
    Tabby,
    /// A tab bar plus a spiral tiling that gives the active pane the most space.
    Fibonacci,
}

impl PaneLayout {
    fn label(self) -> &'static str {
        match self {
            Self::Grid => "layout_grid",
            Self::Horizontal => "horizontal",
            Self::Tabby => "tabby",
            Self::Fibonacci => "fibonacci",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Grid => Self::Horizontal,
            Self::Horizontal => Self::Tabby,
            Self::Tabby => Self::Fibonacci,
            Self::Fibonacci => Self::Grid,
        }
    }
}

pub struct App {
    pub should_quit: bool,
    pub exit_message: Option<String>,
    hello: Option<HelloResult>,
    /// Runtime `spawn_agent` concurrency cap reported by the daemon.
    subagent_concurrency: usize,
    /// Latest `model.list` catalog, kept for the prompt header's model,
    /// provider and variant labels.
    models: Vec<ModelInfo>,
    sessions: Vec<Session>,
    selected: usize,
    /// Open panes in a stable order. Focus moves to the previous/next pane or
    /// jumps directly to a session, but sessions never change panes on their
    /// own.
    views: Vec<SessionView>,
    /// Index into `views` of the focused pane.
    focused: usize,
    /// Session pane arrangement. Session order is always `views` order.
    layout: PaneLayout,
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
    /// Restrict the help overlay to one `HELP_SECTIONS` title.
    help_scope: Option<&'static str>,
    /// Interactive filter for the help overlay.
    help_query: String,
    /// Selected row in the filtered help list.
    help_selected: usize,
    /// First `/delete` arms a confirmation; the second one deletes.
    pending_delete: Option<String>,
    /// Sessions created by this client during this run. On quit, unused ones
    /// are deleted so opening a pane without typing does not pollute history.
    created_sessions: HashSet<String>,
    /// Images attached with `/image`, sent with the next prompt.
    pending_images: Vec<String>,
    /// Images belonging to an in-flight prompt, restored if submit fails.
    inflight_images: Vec<String>,
    /// `<Space>` was pressed in the transcript; the next key is a leader action.
    pending_leader: bool,
    /// Status text replaced by the leader hint, restored on the next key.
    leader_status_backup: Option<String>,
    /// Text selected with the mouse, waiting for the event loop to copy it to
    /// the system clipboard.
    pending_clipboard: Option<String>,
    /// Top-right confirmation shown briefly after copying a selection.
    copy_toast: Option<CopyToast>,
    /// Older-history request picked up by the event loop after drawing.
    pending_history: Option<(String, u64)>,
    /// Pane rectangles from the last draw, used to pick a focus direction.
    pane_areas: Vec<Rect>,
    /// Latest integration state keyed by session id, fed by daemon events.
    integrations: HashMap<String, IntegrationState>,
    /// Tool calls waiting on a decision, keyed by session id.
    approvals: HashMap<String, PendingApproval>,
    /// `ask_user` questions waiting on an answer, keyed by session id.
    inputs: HashMap<String, PendingInput>,
    /// Full-screen diff review, when open.
    diff_view: Option<DiffView>,
    /// Full-screen background process list, when open.
    process_view: Option<ProcessView>,
    /// Vim-style `/` / `?` search within the focused transcript.
    transcript_search: Option<TranscriptSearch>,
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
    /// New-session defaults from cli.json.
    pub default_model: Option<String>,
    pub default_agent: Option<String>,
    pub default_mode: Option<String>,
    /// Set when a notification should ring; cleared by the event loop.
    ring: bool,
    /// Text for the next alert (session name + status).
    last_alert: Option<String>,
    /// When the status-bar alert should disappear.
    alert_at: Option<Instant>,
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
    /// `[theme]` as loaded from config, kept so `/colorscheme` can re-derive a
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
    /// When the previous normal-mode `Ctrl-W` arrived, for the double-press
    /// "focus the next pane" gesture.
    ctrl_w_at: Option<Instant>,
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
    /// Opt-in resources from other agent harnesses.
    compat: eix_core::CompatConfig,
    /// `/` command menu was dismissed with Esc for the current draft.
    command_dismissed: bool,
    /// Active Vim-style `:` command line; when present it owns the keyboard.
    cmdline: Option<CmdLine>,
    /// The dynamic command menu only appears after the user asks for it with Tab.
    cmdline_menu_armed: bool,
    /// Filter snapshot captured when the menu opens. Keeping it lets Tab
    /// complete the first match without discarding the other matches, so
    /// Ctrl-J/K can still move through the full list.
    cmdline_menu_prefix: Option<String>,
    /// Explicitly selected menu row; `None` means the first match.
    cmdline_menu_cursor: Option<usize>,
    /// Executed command-line entries, newest last.
    cmdline_history: Vec<String>,
    /// `Some(i)` browses history `i` entries back from newest; `None` is the
    /// live command-line draft.
    cmdline_history_index: Option<usize>,
    /// Draft captured when command-line history browsing starts.
    cmdline_history_draft: String,
    /// Streaming `/btw` side answer, shown above the prompt.
    btw: Option<BtwView>,
}

impl App {
    pub fn new(initial_session: Option<String>) -> Self {
        let launch_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let compat = eix_core::CompatConfig::default();
        Self {
            should_quit: false,
            exit_message: None,
            hello: None,
            subagent_concurrency: eix_core::DEFAULT_MAX_PARALLEL_SUBAGENTS,
            models: Vec::new(),
            sessions: Vec::new(),
            selected: 0,
            views: Vec::new(),
            focused: 0,
            layout: PaneLayout::Grid,
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
            help_scope: None,
            help_query: String::new(),
            help_selected: 0,
            pending_delete: None,
            created_sessions: HashSet::new(),
            pending_images: Vec::new(),
            inflight_images: Vec::new(),
            pending_leader: false,
            leader_status_backup: None,
            pending_clipboard: None,
            copy_toast: None,
            pending_history: None,
            pane_areas: Vec::new(),
            integrations: HashMap::new(),
            approvals: HashMap::new(),
            inputs: HashMap::new(),
            diff_view: None,
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
            last_alert: None,
            alert_at: None,
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
            ctrl_w_at: None,
            history_index: None,
            history_draft: String::new(),
            queues: HashMap::new(),
            queued_inflight: None,
            submitting_session: None,
            command_cursor: 0,
            commands: merge_commands(&launch_cwd, &compat),
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

    /// Apply the daemon config's compatibility switches and reload commands
    /// from all enabled harness sources.
    pub fn set_compat(&mut self, compat: eix_core::CompatConfig) {
        self.compat = compat;
        let cwd = self
            .focused_view()
            .map(|view| view.session.cwd.clone())
            .unwrap_or_else(|| {
                std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
            });
        self.commands = merge_commands(&cwd, &self.compat);
    }

    /// True when no full-screen overlay owns the keyboard, so a prompt request
    /// can safely open the picker.
    fn overlay_closed(&self) -> bool {
        self.picker.is_none()
            && self.diff_view.is_none()
            && self.process_view.is_none()
            && self.inbox.is_none()
            && self.btw.is_none()
            && self.error.is_none()
            && self.reason_prompt.is_none()
    }

    /// Search the focused session transcript, Vim style. `/` starts forward,
    /// `?` and `#` start backward. An empty query repeats the last search.
    fn search_transcript(&mut self, query: &str, forward: bool) {
        let query = query.trim();
        let query = if query.is_empty() {
            self.transcript_search
                .as_ref()
                .map(|search| search.query.clone())
                .unwrap_or_default()
        } else {
            query.to_string()
        };
        if query.is_empty() {
            self.status = "no previous search".into();
            return;
        }
        let Some(view) = self.focused_view() else {
            self.status = "no session to search".into();
            return;
        };
        let session_id = view.session.id.clone();
        let needle = query.to_lowercase();
        let matches: Vec<usize> = view
            .text_lines
            .iter()
            .enumerate()
            .filter_map(|(line, text)| text.to_lowercase().contains(&needle).then_some(line))
            .collect();
        if matches.is_empty() {
            self.transcript_search = Some(TranscriptSearch {
                session_id,
                query,
                matches,
                current: 0,
                forward,
            });
            self.status = "no matches".into();
            return;
        }
        let start = view.scroll;
        let current = if forward {
            matches.iter().position(|line| *line >= start).unwrap_or(0)
        } else {
            matches
                .iter()
                .rposition(|line| *line < start)
                .unwrap_or(matches.len() - 1)
        };
        let line = matches[current];
        let total = matches.len();
        let query = query.clone();
        self.transcript_search = Some(TranscriptSearch {
            session_id,
            query: query.clone(),
            matches,
            current,
            forward,
        });
        if let Some(view) = self.focused_view_mut() {
            view.follow = false;
            view.scroll = line.saturating_sub(2);
        }
        self.status = format!(
            "{}{} · {}/{}",
            if forward { "/" } else { "?" },
            query,
            current + 1,
            total
        );
    }

    /// Move to the next/previous match from the current in-session search.
    fn search_transcript_next(&mut self, reverse: bool) {
        let Some(session_id) = self.focused_view().map(|view| view.session.id.clone()) else {
            return;
        };
        let Some(search) = self.transcript_search.as_mut() else {
            return;
        };
        if search.session_id != session_id || search.matches.is_empty() {
            return;
        }
        let len = search.matches.len();
        search.current = if reverse {
            (search.current + len - 1) % len
        } else {
            (search.current + 1) % len
        };
        let line = search.matches[search.current];
        let current = search.current + 1;
        let query = search.query.clone();
        let forward = search.forward;
        if let Some(view) = self.focused_view_mut() {
            view.follow = false;
            view.scroll = line.saturating_sub(2);
        }
        self.status = format!(
            "{}{} · {}/{}",
            if forward { "/" } else { "?" },
            query,
            current,
            len
        );
    }

    /// Open the help overlay with a fresh filter and selection.
    fn open_help(&mut self) {
        self.open_help_scoped(None, "");
    }

    /// Open only the key map. `/keys [query]` keeps the same interactive
    /// filter/selection as the full help overlay.
    fn open_keys_help(&mut self, query: &str) {
        self.open_help_scoped(Some("keys"), query);
    }

    fn open_help_scoped(&mut self, scope: Option<&'static str>, query: &str) {
        self.pending_leader = false;
        self.help = true;
        self.help_scope = scope;
        self.help_query = query.to_string();
        self.help_selected = 0;
    }

    /// Keys while help is open: typing filters, arrows/C-n/C-p select, Esc
    /// closes. Plain `q`/`j`/`k` are search input rather than shortcuts so any
    /// key entry can be found by name.
    fn handle_help_key(&mut self, key: KeyEvent) {
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

    fn move_help_selection(&mut self, delta: isize) {
        let count = help_entry_count_in(&self.help_query, self.help_scope);
        if count == 0 {
            self.help_selected = 0;
            return;
        }
        let selected = self.help_selected.min(count - 1) as isize;
        self.help_selected = (selected + delta).clamp(0, count as isize - 1) as usize;
    }

    /// Replace the prompt text without pulling a user who is in normal mode
    /// back into insert mode.
    fn replace_prompt_text(&mut self, text: &str) {
        let mode = self.input.mode();
        self.input.set_text(text);
        if mode == Mode::Normal {
            self.input.enter_normal();
        }
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
                self.replace_prompt_text(&draft);
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
    fn created_session_is_in_use(&self, session_id: &str) -> bool {
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
    fn cleanup_created_session(&mut self, session_id: &str, tx: &UnboundedSender<Command>) {
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
    fn save_draft_now(&mut self, tx: &UnboundedSender<Command>) {
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
    fn current_draft_text(&self) -> String {
        self.input.text()
    }

    pub fn handle_paste(&mut self, text: &str) {
        if self.cmdline.is_some() {
            if let Some(cmdline) = self.cmdline.as_mut() {
                cmdline.insert_str(text);
            }
            self.reset_cmdline_menu_selection();
            return;
        }
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
        self.ctrl_w_at = None;
        self.restore_leader_status();
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
                    self.copy_toast = Some(CopyToast {
                        text: format!("Copied {} chars", text.chars().count()),
                        until: Instant::now() + COPY_TOAST_DISPLAY,
                    });
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
                self.subagent_concurrency = hello.max_parallel_subagents;
                self.hello = Some(hello);
                // Fetch the catalog once so the prompt header can show the
                // raw model id and provider before `/model` is opened.
                let _ = tx.send(Command::ListModels);
                let _ = tx.send(Command::ListIntegrations);
            }
            Event::Integrations(integrations) => {
                for state in integrations {
                    self.integrations.insert(state.session_id.clone(), state);
                }
            }
            Event::Sessions(sessions) => {
                self.sessions = sessions;
                if !self.initialized {
                    self.initialized = true;
                    let index = match self.initial_session.as_deref() {
                        Some(needle) => match self.find_session(needle) {
                            Some(index) => Some(index),
                            None => {
                                self.error = Some(format!("unknown session {needle:?}"));
                                (!self.sessions.is_empty())
                                    .then(|| self.selected.min(self.sessions.len() - 1))
                            }
                        },
                        None => match std::env::current_dir() {
                            Ok(cwd) => self.sessions.iter().position(|session| {
                                !session.is_agent() && session.cwd_matches(&cwd)
                            }),
                            // Without a usable launch directory, preserve the
                            // previous "most recent session" fallback.
                            Err(_) => (!self.sessions.is_empty()).then_some(0),
                        },
                    };
                    match index {
                        Some(index) => self.select_session(index, tx),
                        // A launch from a new directory gets its own session
                        // instead of attaching to an unrelated one.
                        None => self.create_session(tx, None),
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
                if session.worktree.is_none() {
                    self.created_sessions.insert(id.clone());
                }
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
            Event::PickerPreview {
                kind,
                value,
                text,
                error,
            } => {
                if let Some(picker) = &mut self.picker {
                    if picker.preview.key.as_deref() == Some(value.as_str())
                        && picker.kind.preview_kind() == Some(kind.as_str())
                    {
                        picker.preview.text = text;
                        picker.preview.error = error;
                        picker.preview.loading = false;
                    }
                }
            }
            Event::Models { models, warnings } => {
                let count = models.len();
                self.models = models;
                let current = self
                    .focused_view()
                    .and_then(|view| view.session.model.clone());
                let items = model_picker_items(&self.models);
                let selected = current
                    .as_deref()
                    .and_then(|model| {
                        let (base, variant) = split_model_variant(model);
                        items
                            .iter()
                            .position(|item| item.value == model)
                            .or_else(|| {
                                (variant == Some("none"))
                                    .then(|| items.iter().position(|item| item.value == base))
                                    .flatten()
                            })
                    })
                    .unwrap_or(0);
                if let Some(picker) = &mut self.picker {
                    if picker.kind == PickerKind::Models {
                        picker.items = items;
                        picker.selected = selected;
                        picker.loading = false;
                    }
                }
                self.status = format!("{count} model(s)");
                if let Some(warning) = warnings.first() {
                    self.status.push_str(&format!(" · {warning}"));
                }
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
                if let Some(picker) = &mut self.picker {
                    if picker.kind == PickerKind::Mcp {
                        picker.items = mcp_picker_items(&servers);
                        picker.selected = 0;
                        picker.loading = false;
                    }
                }
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
            Event::SubagentConcurrency { value } => {
                self.subagent_concurrency = value;
                self.status = format!("subagents → {value} (runtime)");
            }
            Event::ConfigReloaded {
                paths,
                max_parallel_subagents,
            } => {
                self.subagent_concurrency = max_parallel_subagents;
                self.status = format!("config reloaded ({} layer(s))", paths.len());
            }
            Event::Archived(session) => {
                let id = session.id.clone();
                self.created_sessions.remove(&id);
                self.sessions.retain(|existing| existing.id != id);
                self.views.retain(|view| view.session.id != id);
                if self.focused >= self.views.len() {
                    self.focused = self.views.len().saturating_sub(1);
                }
                self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
                self.status = format!("archived {}", session.name);
            }
            Event::Deleted(id) => {
                self.created_sessions.remove(&id);
                self.sessions.retain(|existing| existing.id != id);
                self.views.retain(|view| view.session.id != id);
                if self.focused >= self.views.len() {
                    self.focused = self.views.len().saturating_sub(1);
                }
                self.selected = self.selected.min(self.sessions.len().saturating_sub(1));
                self.status = "session deleted".into();
            }
            Event::ShellRan {
                ok,
                output,
                insert_output,
            } => {
                if insert_output {
                    let text = output.trim_end_matches(['\r', '\n']);
                    if text.is_empty() {
                        self.status = if ok {
                            "shell output empty".into()
                        } else {
                            "shell failed with no output".into()
                        };
                    } else {
                        self.focus = Focus::Input;
                        self.pending_leader = false;
                        self.error = None;
                        let prefix = if self.input.is_empty() { "" } else { "\n" };
                        self.input.insert_text(&format!("{prefix}{text}"));
                        self.status = format!(
                            "read {} line(s) from shell{}",
                            text.lines().count(),
                            if ok { "" } else { " (failed)" }
                        );
                    }
                    return;
                }
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
                if let Some(picker) = &mut self.picker {
                    if picker.kind == PickerKind::Todos {
                        picker.items = todo_picker_items(&items);
                        picker.selected = 0;
                        picker.loading = false;
                    }
                }
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
            Event::Draft { session_id, text } => {
                if self.input_session.as_deref() != Some(session_id.as_str()) {
                    return;
                }
                // The prompt was cleared on switch; load the daemon draft, but
                // never clobber text typed while the request was in flight.
                // Keep normal mode: a late draft load must not turn the
                // prompt back into insert mode under the user.
                if self.input.is_empty() {
                    if !text.is_empty() {
                        self.replace_prompt_text(&text);
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
                    .map(|view| (view.session.cwd.clone(), view.session.mode));
                if let Some(view) = self.views.iter_mut().find(|view| view.session.id == id) {
                    view.session = session.clone();
                }
                if let Some((cwd, mode)) = previous {
                    if cwd != session.cwd {
                        self.status = format!("cwd: {}", session.cwd.display());
                    } else if mode != session.mode {
                        self.status = format!("mode: {}", session.mode.label());
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
                                // or retried without changing the mode.
                                self.replace_prompt_text(&draft);
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
            "integration.updated" => {
                if let Ok(state) = serde_json::from_value::<IntegrationState>(notification.params) {
                    let session_id = state.session_id.clone();
                    let status = state.status;
                    self.integrations.insert(session_id.clone(), state);
                    if status != IntegrationStatus::Merged
                        && self
                            .status_session()
                            .is_some_and(|session| session.id == session_id)
                    {
                        self.status = format!("integration · {}", status.as_str());
                    }
                }
            }
            "integration.attention" => {
                let session_id = notification
                    .params
                    .get("session_id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let name = notification
                    .params
                    .get("session_name")
                    .and_then(Value::as_str)
                    .unwrap_or("session")
                    .to_string();
                let error = notification
                    .params
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("integration needs attention")
                    .to_string();
                self.last_alert = Some(format!("{name} · integration failed"));
                self.alert_at = Some(Instant::now());
                self.status = format!("integration failed · {}", truncate(&error, 100));
                if self
                    .status_session()
                    .is_none_or(|session| session.id != session_id)
                    && self.bell
                {
                    self.ring = true;
                }
            }
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
                        if self.last_alert.is_some() {
                            self.alert_at = Some(Instant::now());
                        }
                        if self.bell {
                            self.ring = true;
                        }
                    }
                }
            }
            "session.event" => {
                if let Ok(event) = serde_json::from_value::<SessionEvent>(notification.params) {
                    let id = event.session_id.clone();
                    // Any persisted event means the session was used, even if
                    // it is not currently open in a pane.
                    self.created_sessions.remove(&id);
                    // Child sessions are not always open as panes. Feed their
                    // events into the parent's subagent card even when the
                    // child has no view of its own.
                    if let Some(index) = self
                        .views
                        .iter()
                        .position(|view| view.subagents.contains_key(&id))
                    {
                        self.views[index].push_subagent_event(&id, event.clone());
                    }
                    let spawn_result = (event.kind == EventKind::ToolResult)
                        .then(|| {
                            serde_json::from_value::<ToolResultPayload>(event.payload.clone()).ok()
                        })
                        .flatten()
                        .filter(|payload| payload.tool == "spawn_agent");
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
                    if let Some(result) = spawn_result {
                        view.finish_subagent_by_call(&result.call_id, result.ok, &result.output);
                    }
                    if relevant {
                        self.reload_pending(&id);
                    }
                    if relevant_input {
                        self.reload_input(&id);
                    }
                }
            }
            "agent.spawned" => {
                if let Ok(params) =
                    serde_json::from_value::<AgentSpawnedParams>(notification.params)
                {
                    if let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == params.session_id)
                    {
                        view.upsert_subagent(params.agent_id, params.call_id, params.name.clone());
                        self.status = format!("sub-agent {} started", params.name);
                    }
                }
            }
            "agent.completed" => {
                if let Ok(params) =
                    serde_json::from_value::<AgentCompletedParams>(notification.params)
                {
                    if let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == params.session_id)
                    {
                        view.complete_subagent(
                            &params.agent_id,
                            &params.call_id,
                            &params.name,
                            params.ok,
                            params.result,
                            params.error,
                        );
                        self.status = if params.ok {
                            format!("sub-agent {} finished", params.name)
                        } else {
                            format!("sub-agent {} failed", params.name)
                        };
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
                    if self.status.starts_with("retrying ") {
                        self.status = "running".into();
                    }
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
            "message.reset" => {
                if let Ok(reset) = serde_json::from_value::<TurnRef>(notification.params) {
                    if let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == reset.session_id)
                    {
                        if view
                            .stream
                            .as_ref()
                            .is_some_and(|stream| stream.turn_id == reset.turn_id)
                        {
                            view.stream = None;
                        }
                    }
                }
            }
            "turn.retrying" => {
                if let Ok(retry) = serde_json::from_value::<TurnRetrying>(notification.params) {
                    if let Some(view) = self
                        .views
                        .iter_mut()
                        .find(|view| view.session.id == retry.session_id)
                    {
                        if view
                            .stream
                            .as_ref()
                            .is_some_and(|stream| stream.turn_id == retry.turn_id)
                        {
                            view.stream = None;
                        }
                    }
                    let delay = retry.delay_ms as f64 / 1000.0;
                    let reason = truncate(&retry.reason, 80);
                    let kind = match retry.kind.as_str() {
                        "rate_limited" => "rate limited · ",
                        "empty_response" => "empty response · ",
                        "transient" => "upstream error · ",
                        _ => "",
                    };
                    self.error = None;
                    self.status = if reason.is_empty() {
                        format!(
                            "{kind}retrying {}/{} in {delay:.1}s",
                            retry.attempt, retry.max_attempts
                        )
                    } else {
                        format!(
                            "{kind}retrying {}/{} in {delay:.1}s: {reason}",
                            retry.attempt, retry.max_attempts
                        )
                    };
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
                    let mut summary =
                        if matches!(params.tool.as_str(), "run_command" | "start_command") {
                            params
                                .arguments
                                .get("command")
                                .and_then(serde_json::Value::as_str)
                                .map(|command| format!("$ {command}"))
                                .unwrap_or(params.summary)
                        } else {
                            params.summary
                        };
                    let risk = params.risk.to_ascii_uppercase();
                    if !risk.is_empty() {
                        summary = format!("[{risk}] {summary}");
                    }
                    for reason in &params.reasons {
                        summary.push_str(&format!("\n{reason}"));
                    }
                    if !params.risk_categories.is_empty() {
                        summary.push_str(&format!("\nrisk: {}", params.risk_categories.join(", ")));
                    }
                    if !params.sandbox.is_empty() {
                        summary.push_str(&format!("\nsandbox: {}", params.sandbox));
                    }
                    if let Some(rule) = params.rule.as_deref() {
                        summary.push_str(&format!("\nrule: {rule} ({})", params.rule_source));
                    }
                    if !params.scopes.is_empty() {
                        summary.push_str(&format!("\nscopes: {}", params.scopes.join(", ")));
                    }
                    if !params.allow_always {
                        summary.push_str(NO_ALWAYS_MARKER);
                    }
                    self.approvals.insert(
                        params.session_id,
                        PendingApproval {
                            kind: PendingKind::Tool,
                            approval_id: params.approval_id,
                            call_id: params.call_id,
                            tool: params.tool,
                            summary,
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
                if let Some(value) = notification.params["max_parallel_subagents"]
                    .as_u64()
                    .map(|value| value as usize)
                {
                    self.subagent_concurrency = value;
                }
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

    /// Hide the `<Space>` leader hint as soon as the next key arrives. The
    /// action that consumes the key can still publish its own status.
    fn restore_leader_status(&mut self) {
        let Some(status) = self.leader_status_backup.take() else {
            return;
        };
        if self.status.starts_with("leader:") {
            self.status = status;
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        // Any other key breaks the double-Ctrl-W gesture, so a later Ctrl-W
        // cannot accidentally pair with one pressed earlier.
        if !(key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('w') | KeyCode::Char('W')))
        {
            self.ctrl_w_at = None;
        }
        if self.pending_leader {
            self.restore_leader_status();
        }
        // Ctrl-C / Ctrl-D quit the TUI from every mode, including insert and
        // any open overlay. Save the draft first so quitting never loses it.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(
                key.code,
                KeyCode::Char('c') | KeyCode::Char('C') | KeyCode::Char('d') | KeyCode::Char('D')
            )
        {
            self.sync_draft(tx);
            self.should_quit = true;
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
        // The `:` command line is modal: every key edits it or submits/cancels
        // it until it closes.
        if self.cmdline.is_some() {
            self.handle_cmdline_key(key, tx);
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
                KeyCode::Char('b') | KeyCode::Char('B')
                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if let Some(btw) = self.btw.as_mut() {
                        btw.scroll = btw.scroll.saturating_sub(8);
                    }
                }
                KeyCode::Char('f') | KeyCode::Char('F')
                    if key.modifiers.contains(KeyModifiers::CONTROL) =>
                {
                    if let Some(btw) = self.btw.as_mut() {
                        btw.scroll = btw.scroll.saturating_add(8);
                    }
                }
                _ => {}
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

        // A blocking approval/question card owns its navigation keys no matter
        // whether the prompt or the transcript has focus. Other overlays above
        // keep priority.
        if self.diff_view.is_none() && !self.pending_leader && self.handle_card_key(key, tx) {
            return;
        }

        // Queue-position shortcuts work from both the prompt and transcript.
        // Ctrl-1..9 needs a terminal with keyboard disambiguation (kitty
        // protocol).
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if let KeyCode::Char(ch @ '1'..='9') = key.code {
                // Ctrl-7 is the legacy encoding of Ctrl-/ and belongs to the
                // model picker below, not the tab strip.
                if ch != '7' {
                    self.focus_tab(ch as usize - '1' as usize);
                    return;
                }
            }
        }

        // Global model picker: Ctrl-/ (terminals encode it as '/', '_', or Ctrl-7).
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(
                key.code,
                KeyCode::Char('/') | KeyCode::Char('_') | KeyCode::Char('7')
            )
        {
            self.open_picker(PickerKind::Models, tx);
            return;
        }

        // Ctrl-H opens this session's user-input timeline. Some terminals
        // report Ctrl-H as a control-modified Backspace.
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('h') | KeyCode::Backspace)
        {
            self.open_picker(PickerKind::Timeline, tx);
            return;
        }

        // Global reasoning shortcut: cycle low → medium → high → none → low.
        // Updating the session locally first keeps rapid presses advancing
        // even before the daemon round-trip lands.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('t') {
            self.cycle_reasoning(tx);
            return;
        }

        // Vim-style command line: `:` in normal mode, or from the transcript.
        // Insert/visual modes keep typing a literal colon.
        if key.code == KeyCode::Char(':')
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && self.overlay_closed()
            && !self.pending_leader
            && self.focused_approval().is_none()
            && self.focused_input().is_none()
            && (self.focus == Focus::Transcript
                || (self.input.mode() == Mode::Normal && self.input.normal_idle()))
        {
            self.open_cmdline();
            return;
        }

        if self.focus == Focus::Input {
            self.handle_input_key(key, tx);
        } else {
            self.handle_transcript_key(key, tx);
        }
    }

    /// TUI actions shared by transcript focus and the prompt's normal mode.
    /// The prompt keeps its vim editing keys (`j`/`k`, `Tab`, `Enter`, ...);
    /// only actions the editor does not own are handled here.
    fn handle_shared_normal_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) -> bool {
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
            return true;
        }
        if key.code == KeyCode::Char(' ')
            && !key.modifiers.contains(KeyModifiers::CONTROL)
            && !key.modifiers.contains(KeyModifiers::ALT)
        {
            // A blocking card owns the keyboard; never start a hidden leader
            // under it, and never let the editor start a second one either.
            if self.focused_approval().is_none() && self.focused_input().is_none() {
                self.pending_leader = true;
                self.leader_status_backup = Some(std::mem::take(&mut self.status));
                self.status = "leader: f/w/b · n new · o pane · v/t/r/l layout · R approvals · c close · m mode · i input · Z quit · Esc cancel"
                    .into();
            }
            return true;
        }
        if self.focused_approval().is_none() && self.focused_input().is_none() {
            if let Some(action) = self.keybinds.action(key) {
                self.run_key_action(action, tx);
                return true;
            }
        }
        // Shift-Tab walks the same mode ring backwards. The prompt only reaches
        // this handler while normal-idle, so a pending count/operator stays
        // with the editor instead of being cleared by a mode switch.
        let back_tab = matches!(key.code, KeyCode::BackTab)
            || (key.code == KeyCode::Tab && key.modifiers.contains(KeyModifiers::SHIFT));
        if back_tab && self.focused_approval().is_none() && self.focused_input().is_none() {
            self.cycle_mode(-1, tx);
            return true;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('w') | KeyCode::Char('W') => {
                    let now = Instant::now();
                    let double = self
                        .ctrl_w_at
                        .is_some_and(|last| now.duration_since(last) < CTRL_W_WINDOW);
                    self.ctrl_w_at = (!double).then_some(now);
                    if double {
                        self.cycle_pane(1);
                    }
                    return true;
                }
                KeyCode::Char('p') | KeyCode::Char('P') => {
                    self.open_picker(PickerKind::Commands, tx);
                    return true;
                }
                KeyCode::Char('b') | KeyCode::Char('B') => {
                    self.scroll_focused(-10);
                    return true;
                }
                KeyCode::Char('f') | KeyCode::Char('F') => {
                    self.scroll_focused(10);
                    return true;
                }
                KeyCode::Char('u') => {
                    self.scroll_focused(-10);
                    return true;
                }
                _ => {}
            }
        }
        if matches!(key.code, KeyCode::Char('n') | KeyCode::Char('N'))
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && self.transcript_search.is_some()
        {
            self.search_transcript_next(matches!(key.code, KeyCode::Char('N')));
            return true;
        }
        match key.code {
            KeyCode::Char(ch @ ('/' | '?' | '#')) => {
                self.open_cmdline_with(ch);
                true
            }
            KeyCode::PageDown => {
                self.scroll_focused(10);
                true
            }
            KeyCode::PageUp => {
                self.scroll_focused(-10);
                true
            }
            _ => false,
        }
    }

    fn handle_input_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        // The diff overlay owns the keyboard regardless of which pane was
        // focused when it opened, matching the transcript path.
        if self.diff_view.is_some() {
            self.handle_diff_key(key, tx);
            return;
        }
        // Insert/visual keys belong to the editor, so a pane leader must not
        // leak into typed input. Normal mode shares the transcript leader.
        if self.input.mode() != Mode::Normal {
            self.pending_leader = false;
        }
        // In normal mode the prompt behaves like a focused pane for the TUI
        // actions the editor does not own. A half-typed vim prefix keeps the
        // key for the editor instead. This runs before the clipboard paste
        // path so leader+`p` still opens processes instead of pasting.
        if self.input.mode() == Mode::Normal
            && self.input.normal_idle()
            && self.handle_shared_normal_key(key, tx)
        {
            return;
        }
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
            // A running `/` or `?` search owns a single Esc: drop its
            // highlights before the double-Esc focus gesture can fire.
            if self.input.mode() == Mode::Normal && self.transcript_search.is_some() {
                self.transcript_search = None;
                self.esc_at = None;
                self.status = "search cleared".into();
                return;
            }
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

    fn selected_command(&self) -> Option<&CommandSpec> {
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

    /// Whether the `:` command line should show its dynamic command list.
    fn cmdline_menu_open(&self) -> bool {
        self.cmdline_menu_armed
            && self.cmdline.as_ref().is_some_and(|cmdline| {
                cmdline.prompt == ':' && !cmdline.text.contains(char::is_whitespace)
            })
    }

    fn cmdline_matches(&self) -> Vec<&CommandSpec> {
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

    fn selected_cmdline_match(&self) -> Option<&CommandSpec> {
        let matches = self.cmdline_matches();
        if matches.is_empty() {
            return None;
        }
        let cursor = self.cmdline_menu_cursor.unwrap_or(0).min(matches.len() - 1);
        matches.get(cursor).copied()
    }

    fn move_cmdline_menu(&mut self, delta: isize) -> bool {
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
    fn reset_cmdline_menu_selection(&mut self) {
        self.cmdline_menu_prefix = None;
        self.cmdline_menu_cursor = None;
    }

    fn cmdline_name_exists(&self, name: &str) -> bool {
        self.commands.iter().any(|spec| spec.name == name)
            || ["q", "qa", "qall", "quitall", "w", "wq", "x"].contains(&name)
    }

    fn open_cmdline(&mut self) {
        self.open_cmdline_with(':');
    }

    fn open_cmdline_with(&mut self, prompt: char) {
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

    fn close_cmdline(&mut self) {
        self.cmdline = None;
        self.esc_at = None;
        self.cmdline_menu_armed = false;
        self.cmdline_menu_prefix = None;
        self.cmdline_menu_cursor = None;
        self.cmdline_history_index = None;
        self.cmdline_history_draft.clear();
    }

    fn handle_cmdline_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
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

    fn cmdline_history_prev(&mut self) {
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

    fn cmdline_history_next(&mut self) {
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

    fn complete_cmdline(&mut self) {
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

    fn remember_cmdline(&mut self, text: &str) {
        if self.cmdline_history.last().is_some_and(|last| last == text) {
            return;
        }
        self.cmdline_history.push(text.to_string());
        if self.cmdline_history.len() > CMDLINE_HISTORY_LIMIT {
            self.cmdline_history.remove(0);
        }
    }

    fn run_cmdline_shell(
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

    fn submit_cmdline(&mut self, tx: &UnboundedSender<Command>) {
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

    /// Route keys owned by a blocking approval or question card. Returns true
    /// when the card consumed the key, so both prompt and transcript focus
    /// behave the same in every layout.
    fn handle_card_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) -> bool {
        if self.focused_input().is_some() {
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
                KeyCode::Char('j') | KeyCode::Down => {
                    self.move_input_cursor(1);
                    return true;
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.move_input_cursor(-1);
                    return true;
                }
                KeyCode::Enter => {
                    if let Some(cursor) = self.focused_input().map(|pending| pending.cursor) {
                        self.respond_input(cursor, tx);
                    }
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

    fn handle_transcript_key(&mut self, key: KeyEvent, tx: &UnboundedSender<Command>) {
        if self.diff_view.is_some() {
            self.handle_diff_key(key, tx);
            return;
        }
        // Direct test callers may reach the transcript handler without going
        // through `handle_key`; keep card navigation working there too.
        if self.handle_card_key(key, tx) {
            return;
        }
        if self.handle_shared_normal_key(key, tx) {
            return;
        }
        if key.code == KeyCode::Esc && self.transcript_search.is_some() {
            self.transcript_search = None;
            self.esc_at = None;
            self.status = "search cleared".into();
            return;
        }
        if key.code == KeyCode::Esc {
            // The transcript is also a valid start point: after `:` closes
            // (which may have been opened from here), two Esc presses should
            // interrupt the focused turn just like they do in the prompt.
            let now = Instant::now();
            let double = self
                .esc_at
                .is_some_and(|last| now.duration_since(last) < ESC_WINDOW);
            if double {
                self.esc_at = None;
                if self.focused_session_busy() {
                    self.cancel(tx);
                }
            } else {
                self.esc_at = Some(now);
            }
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('g')) {
            self.open_picker(PickerKind::Sessions, tx);
            return;
        }
        // Pane actions are leader-only. Single letters are reserved for
        // navigation (j/k, g/G) and the prompt's own vim editing; the only
        // direct shortcuts left are shifted punctuation (`?`/`#`/`<`/`>`)
        // plus Ctrl-P for the palette in the shared handler above.
        match key.code {
            // Shift+Z is the vim-adjacent quit key in normal mode.
            KeyCode::Char('Z')
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.run_action("quit", tx)
            }
            KeyCode::Char('<') => self.cycle_pane(-1),
            KeyCode::Char('>') => self.cycle_pane(1),
            // `i` is the one bare letter that stays: focusing the prompt is
            // the normal way out of the transcript, like vim insert mode.
            // Enter (pane promotion) is shared with prompt normal mode.
            KeyCode::Char('i') => self.focus_input(),
            KeyCode::Tab => self.toggle_mode(tx),
            KeyCode::Char('j') | KeyCode::Down => self.scroll_focused(1),
            KeyCode::Char('k') | KeyCode::Up => self.scroll_focused(-1),
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
            KeyAction::NewSession => self.create_session(tx, None),
            KeyAction::NewWorktree => self.create_session(tx, Some(true)),
            KeyAction::Sessions => self.open_picker(PickerKind::Sessions, tx),
            KeyAction::NewPane => self.open_picker(PickerKind::NewPane, tx),
            KeyAction::Palette => self.open_picker(PickerKind::Commands, tx),
            KeyAction::Models => self.open_picker(PickerKind::Models, tx),
            KeyAction::Worktrees => self.open_picker(PickerKind::Worktrees, tx),
            KeyAction::Branches => self.open_picker(PickerKind::Branches, tx),
            KeyAction::Processes => self.open_processes(tx),
            KeyAction::Inbox => self.open_inbox(),
            KeyAction::Fork => self.fork_focused(tx, None),
            KeyAction::Attention => self.focus_next_attention(),
            KeyAction::Cancel => self.cancel(tx),
            KeyAction::NextSession => self.cycle_session(1, tx),
            KeyAction::PrevSession => self.cycle_session(-1, tx),
            KeyAction::ScrollDown => self.scroll_focused(1),
            KeyAction::ScrollUp => self.scroll_focused(-1),
            KeyAction::Timeline => self.open_picker(PickerKind::Timeline, tx),
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

    fn jump_to_user_seq(&mut self, seq: u64) {
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
            KeyCode::Char('b') | KeyCode::Char('B')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if let Some(inbox) = self.inbox.as_mut() {
                    inbox.cursor = cursor.saturating_sub(10);
                }
            }
            KeyCode::Char('f') | KeyCode::Char('F')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if let Some(inbox) = self.inbox.as_mut() {
                    inbox.cursor = (cursor + 10).min(len.saturating_sub(1));
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
            KeyCode::Char('b') | KeyCode::Char('B')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                view.scroll = view.scroll.saturating_add(20);
            }
            KeyCode::Char('f') | KeyCode::Char('F')
                if key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                view.scroll = view.scroll.saturating_sub(20);
            }
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
                KeyCode::Char('b') | KeyCode::Char('B') => {
                    self.move_picker(-10);
                    return;
                }
                KeyCode::Char('f') | KeyCode::Char('F') => {
                    self.move_picker(10);
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
            self.status = "colorscheme unchanged".into();
        }
        self.picker = None;
    }

    fn apply_theme_picker(&mut self, value: &str) {
        self.theme_picker_backup = None;
        self.theme_config.preset = (value != THEME_DEFAULT_VALUE).then(|| value.to_string());
        self.theme = Theme::from_config(&self.theme_config);
        self.status = if value == THEME_DEFAULT_VALUE {
            "colorscheme → default".into()
        } else if let Some(preset) = theme_preset(value) {
            format!("colorscheme → {} ({})", preset.name, preset.description)
        } else {
            format!("colorscheme → {value}")
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
            PickerKind::Subagents => {
                self.picker = None;
                if let Some(value) = value {
                    match value.parse::<usize>() {
                        Ok(value) => self.set_subagent_concurrency(value, tx),
                        Err(_) => {
                            self.error = Some("invalid subagent concurrency value".into());
                        }
                    }
                }
            }
            PickerKind::Modes => {
                if let Some(value) = value {
                    self.picker = None;
                    self.command_mode(&value, tx);
                }
            }
            PickerKind::Timeline => {
                if let Some(value) = value {
                    self.picker = None;
                    if let Ok(seq) = value.parse::<u64>() {
                        self.jump_to_user_seq(seq);
                    }
                }
            }
            PickerKind::Skills => {
                if let Some(value) = value {
                    let cwd = self
                        .focused_view()
                        .map(|view| view.session.cwd.clone())
                        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                    if let Some(skill) = eix_core::find_skill(&cwd, &value, &self.compat) {
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
            PickerKind::Mcp | PickerKind::Todos => {
                self.picker = None;
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
            preview: PickerPreview::default(),
            preview_request: None,
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
            self.status = "model: loading provider catalogs…".into();
            let _ = tx.send(Command::ListModels);
        } else if kind == PickerKind::Agents {
            let Some(session_id) = self.focused_session_id() else {
                self.status = "attach a session first".into();
                return;
            };
            picker.loading = true;
            self.status = "agents: loading…".into();
            let _ = tx.send(Command::ListAgents { session_id });
        } else if kind == PickerKind::Subagents {
            let current = self.subagent_concurrency;
            picker.items = subagent_picker_items(current);
            picker.selected = picker
                .items
                .iter()
                .position(|item| item.value == current.to_string())
                .unwrap_or(0);
            self.status = format!("subagents: {current} concurrent (runtime)");
        } else if kind == PickerKind::Modes {
            let Some(current) = self.focused_view().map(|view| view.session.mode) else {
                self.status = "attach a session first".into();
                return;
            };
            picker.items = mode_picker_items(current);
            picker.selected = picker
                .items
                .iter()
                .position(|item| item.value == current.as_str())
                .unwrap_or(0);
            self.status = format!(
                "mode: {}{}",
                current.label(),
                if current.is_read_only() {
                    " (read-only)"
                } else {
                    ""
                }
            );
        } else if kind == PickerKind::Timeline {
            let Some(view) = self.focused_view() else {
                self.status = "attach a session first".into();
                return;
            };
            picker.items = timeline_picker_items(&view.events);
            self.status = format!("timeline: {} user message(s)", picker.items.len());
        } else if kind == PickerKind::Skills {
            let cwd = self
                .focused_view()
                .map(|view| view.session.cwd.clone())
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
            picker.items = skill_picker_items(&eix_core::load_skills(&cwd, &self.compat));
            self.status = format!("{} skill(s)", picker.items.len());
        } else if kind == PickerKind::Mcp {
            picker.loading = true;
            self.status = "mcp: loading…".into();
            let _ = tx.send(Command::ListMcp);
        } else if kind == PickerKind::Todos {
            let Some(session_id) = self.focused_session_id() else {
                self.status = "attach a session first".into();
                return;
            };
            picker.loading = true;
            self.status = "todos: loading…".into();
            let _ = tx.send(Command::ListTodos { session_id });
        } else if kind == PickerKind::Themes {
            picker.items = theme_picker_items();
            let current = self.theme.preset.unwrap_or(THEME_DEFAULT_VALUE);
            picker.selected = picker
                .items
                .iter()
                .position(|item| item.value == current)
                .unwrap_or(0);
            self.theme_picker_backup = Some((self.theme_config.clone(), self.theme));
            self.status = "colorscheme preview · Enter apply · Esc cancel".into();
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
            "keys" => self.open_keys_help(""),
            "sessions" => self.open_picker(PickerKind::Sessions, tx),
            "new-pane" => self.open_picker(PickerKind::NewPane, tx),
            "files" => self.open_picker(PickerKind::Files, tx),
            "worktrees" => self.open_picker(PickerKind::Worktrees, tx),
            "branches" => self.open_picker(PickerKind::Branches, tx),
            "model" => self.open_picker(PickerKind::Models, tx),
            "agents" => self.open_picker(PickerKind::Agents, tx),
            "subagents" => self.open_picker(PickerKind::Subagents, tx),
            "mode" | "perm" => self.open_picker(PickerKind::Modes, tx),
            "toggle-mode" => self.toggle_mode(tx),
            "skills" => self.open_picker(PickerKind::Skills, tx),
            "colorscheme" => self.open_picker(PickerKind::Themes, tx),
            "processes" => self.open_processes(tx),
            "approvals" => self.open_inbox(),
            "mcp" => self.open_picker(PickerKind::Mcp, tx),
            "todos" => self.open_picker(PickerKind::Todos, tx),
            "new" => self.create_session(tx, None),
            "new-worktree" => self.create_session(tx, Some(true)),
            "new-shared" => self.create_session(tx, Some(false)),
            "fork" => self.fork_focused(tx, None),
            "cancel" => self.cancel(tx),
            "attention" => self.focus_next_attention(),
            "input" => self.focus_input(),
            "close-pane" => {
                // Persist the prompt without switching panes; closing an
                // unsaved draft would otherwise make the cleanup check think
                // the session is unused.
                self.save_draft_now(tx);
                if let Some(session_id) = self.close_focused_pane() {
                    self.cleanup_created_session(&session_id, tx);
                }
            }
            "pane-prev" => self.cycle_pane(-1),
            "pane-next" => self.cycle_pane(1),
            "layout-horizontal" => self.set_layout(PaneLayout::Horizontal),
            "layout-tabby" => self.set_layout(PaneLayout::Tabby),
            "layout-fibonacci" => self.set_layout(PaneLayout::Fibonacci),
            "layout-cycle" => {
                let next = self.layout.next();
                self.set_layout(next);
            }
            "help" => self.open_help(),
            "quit" => {
                self.sync_draft(tx);
                self.should_quit = true;
            }
            _ => {}
        }
    }

    /// Advance the focused session through `Plan → Build → Auto → Bypass`.
    /// `delta` is +1 for Tab and -1 for Shift-Tab, so the reverse walk is
    /// `Build → Plan → Bypass → Auto`. Update locally first so rapid presses
    /// keep moving before the daemon round-trip lands; the session event
    /// reconciles the final state.
    fn cycle_mode(&mut self, delta: isize, tx: &UnboundedSender<Command>) {
        const MODES: [SessionMode; 4] = [
            SessionMode::Plan,
            SessionMode::Build,
            SessionMode::Auto,
            SessionMode::Bypass,
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

    /// Tab cycles forward; `/toggle-mode` and the command palette use this too.
    fn toggle_mode(&mut self, tx: &UnboundedSender<Command>) {
        self.cycle_mode(1, tx);
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

    fn create_session(&mut self, tx: &UnboundedSender<Command>, worktree: Option<bool>) {
        self.create_session_in(tx, worktree, None, None);
    }

    /// Create a session and open it in a new pane. `cwd`/`name` default to the
    /// TUI's launch directory / a generated name.
    fn create_session_in(
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
        self.handle_command_text(text, tx, true)
    }

    /// Run a `/command` line. `clear_prompt` is false for the `:` command
    /// line, where the prompt buffer is an unrelated draft that must survive.
    fn handle_command_text(
        &mut self,
        text: &str,
        tx: &UnboundedSender<Command>,
        clear_prompt: bool,
    ) -> bool {
        let trimmed = text.trim();
        let (name, args) = match trimmed.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (trimmed, ""),
        };
        let typed = name.trim_start_matches('/');
        if typed.is_empty() {
            return false;
        }
        let spec = self
            .commands
            .iter()
            .find(|spec| spec.name == typed)
            .or_else(|| {
                command_alias(typed)
                    .and_then(|canonical| self.commands.iter().find(|spec| spec.name == canonical))
            })
            .cloned();
        let Some(spec) = spec else {
            self.error = Some(format!("unknown command /{typed} · /help lists them"));
            return true;
        };
        let name = spec.name.as_str();
        if clear_prompt {
            // Commands are not drafts: drop the typed line (and its saved
            // draft) before dispatching. `/init` installs its own prompt
            // afterwards. The `:` command line leaves the prompt draft alone.
            self.input.clear();
            self.last_saved_draft.clear();
            self.reset_history();
            // `/exit`, `/quit` and `/close` save the (now empty) draft
            // themselves.
            if !matches!(name, "exit" | "quit" | "close") {
                if let Some(session_id) = self.focused_session_id() {
                    let _ = tx.send(Command::DraftSet {
                        session_id,
                        text: String::new(),
                    });
                }
            }
        }
        match name {
            "exit" | "quit" => {
                if clear_prompt {
                    self.exit_session(tx);
                } else {
                    self.quit_preserving_draft(tx);
                }
                true
            }
            "close" => {
                if clear_prompt {
                    self.close_session(tx);
                } else {
                    self.close_preserving_draft(tx);
                }
                true
            }
            "help" => {
                self.open_help_scoped(None, args);
                true
            }
            "keys" => {
                self.open_keys_help(args);
                true
            }
            "new" => {
                let cwd = self.focused_view().map(|view| view.session.cwd.clone());
                let name = (!args.is_empty()).then(|| args.to_string());
                self.create_session_in(tx, None, cwd, name);
                true
            }
            "new-pane" => {
                self.open_picker(PickerKind::NewPane, tx);
                true
            }
            "new-worktree" => {
                let cwd = self.focused_view().map(|view| view.session.cwd.clone());
                let name = (!args.is_empty()).then(|| args.to_string());
                self.create_session_in(tx, Some(true), cwd, name);
                true
            }
            "new-shared" => {
                let cwd = self.focused_view().map(|view| view.session.cwd.clone());
                let name = (!args.is_empty()).then(|| args.to_string());
                self.create_session_in(tx, Some(false), cwd, name);
                true
            }
            "ready" => {
                if let Some(session_id) = self.focused_session_id() {
                    let _ = tx.send(Command::Ready { session_id });
                    self.status = "queuing worktree for integration…".into();
                } else {
                    self.error = Some("no session to mark ready".into());
                }
                true
            }
            "integrate" => {
                if let Some(session) = self.status_session() {
                    if let Some(state) = self.integrations.get(&session.id) {
                        let error = state
                            .error
                            .as_deref()
                            .map(|error| format!(" · {}", truncate(error, 80)))
                            .unwrap_or_default();
                        self.status = format!(
                            "integration {} · {}{}",
                            state.status.symbol(),
                            state.status.as_str(),
                            error
                        );
                    } else {
                        self.status = "no integration state yet".into();
                    }
                }
                true
            }
            "files" => {
                self.open_picker(PickerKind::Files, tx);
                true
            }
            "worktrees" => {
                self.open_picker(PickerKind::Worktrees, tx);
                true
            }
            "branches" => {
                self.open_picker(PickerKind::Branches, tx);
                true
            }
            "processes" => {
                self.open_processes(tx);
                true
            }
            "approvals" => {
                self.open_inbox();
                true
            }
            "attention" => {
                self.focus_next_attention();
                true
            }
            "cancel" => {
                self.cancel(tx);
                true
            }
            "toggle-mode" => {
                self.toggle_mode(tx);
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
            "model" => {
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
            "subagents" => {
                if args.is_empty() {
                    self.open_picker(PickerKind::Subagents, tx);
                } else {
                    match args.parse::<usize>() {
                        Ok(value) => self.set_subagent_concurrency(value, tx),
                        Err(_) => {
                            self.error = Some("usage: /subagents [2|4|8|16|32|64|128|256]".into());
                        }
                    }
                }
                true
            }
            "mcp" => {
                self.open_picker(PickerKind::Mcp, tx);
                true
            }
            "reload" => {
                let _ = tx.send(Command::ReloadConfig);
                self.status = "reloading config…".into();
                true
            }
            "colorscheme" => {
                if args.is_empty() {
                    self.open_picker(PickerKind::Themes, tx);
                } else {
                    self.error = Some("usage: /colorscheme (opens the colorscheme picker)".into());
                }
                true
            }
            "timeline" => {
                self.open_picker(PickerKind::Timeline, tx);
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
                self.open_picker(PickerKind::Todos, tx);
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
                    if let Some(skill) = eix_core::find_skill(&cwd, name, &self.compat) {
                        self.input
                            .set_text(&eix_core::expand_command(&skill.body, arguments));
                        self.input.enter_insert();
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
            "perm" => {
                self.command_mode(args, tx);
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
                self.commands = merge_commands(&cwd, &self.compat);
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
        self.input.enter_insert();
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

    fn command_mode(&mut self, args: &str, tx: &UnboundedSender<Command>) {
        let Some(session_id) = self.focused_session_id() else {
            self.status = "no session".into();
            return;
        };
        if args.is_empty() {
            self.open_picker(PickerKind::Modes, tx);
            return;
        }
        match args.parse::<SessionMode>() {
            Ok(mode) => {
                self.status = format!("mode → {}", mode.label());
                let _ = tx.send(Command::SetMode {
                    session_id,
                    mode,
                    temporary: false,
                });
            }
            Err(()) => {
                self.error = Some("usage: /perm plan | build | auto | bypass".into());
            }
        }
    }

    fn set_subagent_concurrency(&mut self, value: usize, tx: &UnboundedSender<Command>) {
        if !eix_core::subagent_concurrency_supported(value) {
            self.error =
                Some("subagent concurrency must be 2, 4, 8, 16, 32, 64, 128, or 256".into());
            return;
        }
        self.status = format!("setting subagents to {value}…");
        let _ = tx.send(Command::SetSubagentConcurrency { value });
    }

    /// Persist the (cleared) draft and quit the whole TUI. Open sessions stay
    /// alive in the daemon; `/close` is the command that closes a session pane.
    fn exit_session(&mut self, tx: &UnboundedSender<Command>) {
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
    fn close_session(&mut self, tx: &UnboundedSender<Command>) {
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
    fn quit_preserving_draft(&mut self, tx: &UnboundedSender<Command>) {
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
    fn close_preserving_draft(&mut self, tx: &UnboundedSender<Command>) {
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
        let unit = if self.layout == PaneLayout::Tabby {
            "tab"
        } else {
            "pane"
        };
        self.status = format!("{unit} {}/{} focused", index + 1, self.views.len());
    }

    fn sync_selected_to_focus(&mut self) {
        if let Some(view) = self.focused_view() {
            if let Some(index) = self.find_session(&view.session.id) {
                self.selected = index;
            }
        }
    }

    fn set_layout(&mut self, layout: PaneLayout) {
        self.layout = layout;
        self.status = format!("layout: {}", layout.label());
    }

    /// Focus the Nth open session (`Ctrl-1` is the first pane). In tabby
    /// layout the focused session is also the active tab.
    fn focus_tab(&mut self, index: usize) {
        if index >= self.views.len() {
            self.status = format!("session {} is not open", index + 1);
            return;
        }
        self.focus_pane(index);
    }

    /// Jump focus to an open session without changing the pane order.
    fn jump_to_session(&mut self, session_id: &str) -> bool {
        let Some(index) = self.open_session(session_id) else {
            return false;
        };
        self.focus_pane(index);
        true
    }

    fn cycle_pane(&mut self, delta: isize) {
        if self.views.len() < 2 {
            return;
        }
        let len = self.views.len() as isize;
        let next = (self.focused as isize + delta).rem_euclid(len) as usize;
        self.focus_pane(next);
    }

    fn close_focused_pane(&mut self) -> Option<String> {
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
        let max_scroll = view.text_lines.len().saturating_sub(inner_height);
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

    fn model(&self) -> &str {
        self.focused_view()
            .and_then(|view| view.session.model.as_deref())
            .or_else(|| self.hello.as_ref().map(|hello| hello.model.as_str()))
            .unwrap_or("-")
    }

    /// Model id, provider and variant shown in the prompt header. The catalog
    /// supplies the raw API id so named providers do not repeat their routing
    /// prefix in front of the model name.
    fn model_header_parts(&self) -> (String, String, Option<String>) {
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
    fn cycle_reasoning(&mut self, tx: &UnboundedSender<Command>) {
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
        // The status bar and the `:` line own the bottom two rows when the
        // terminal has room for them; very short terminals only show the
        // command line while it is open.
        let show_cmdline = self.cmdline.is_some() || area.height >= 7;
        let status_height = u16::from(area.height >= 7);
        let cmdline_height = u16::from(show_cmdline);
        let input_height = prompt_input_height(area.height, area.width, &self.input);
        let chunks = Layout::vertical([
            Constraint::Min(3),
            Constraint::Length(input_height),
            Constraint::Length(status_height),
            Constraint::Length(cmdline_height),
        ])
        .split(area);

        self.draw_body(frame, chunks[0]);
        self.draw_input(frame, chunks[1]);
        self.draw_status_bar(frame, chunks[2]);
        self.draw_cmdline(frame, chunks[3]);
        if self.cmdline_menu_open() {
            self.draw_cmdline_menu(frame, chunks[3]);
        }
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
                self.help_scope,
                self.theme,
            );
        }
        if self.diff_view.is_some() {
            self.draw_diff_view(frame, area);
        }
        if self.process_view.is_some() {
            self.draw_processes(frame, area);
        }
        if self.inbox.is_some() {
            self.draw_inbox(frame, area);
        }
        if self.error.is_some() {
            self.draw_error_dialog(frame, area);
        }

        if self
            .copy_toast
            .as_ref()
            .is_some_and(|toast| toast.until <= Instant::now())
        {
            self.copy_toast = None;
        }
        self.draw_copy_toast(frame, area);
    }

    /// A small confirmation in the top-right corner. It intentionally floats
    /// above panes and dialogs so copy feedback is visible without moving the
    /// eye to the status bar.
    fn draw_copy_toast(&self, frame: &mut Frame, area: Rect) {
        let Some(toast) = &self.copy_toast else {
            return;
        };
        let text = format!(" {} ", toast.text);
        let width = (text.width() as u16).min(area.width.saturating_sub(2));
        if width == 0 || area.height < 2 {
            return;
        }
        let rect = Rect {
            x: area.x + area.width.saturating_sub(width + 1),
            y: area.y + 1,
            width,
            height: 1,
        };
        frame.render_widget(Clear, rect);
        frame.render_widget(
            Paragraph::new(text).alignment(Alignment::Right).style(
                Style::default()
                    .fg(COMPOSER_BADGE_TEXT)
                    .bg(COMPOSER_SUCCESS)
                    .add_modifier(Modifier::BOLD),
            ),
            rect,
        );
    }

    /// Compact status icon shared by the pane title and the prompt header.
    /// Running sessions animate; idle sessions use a small static dot.
    fn status_icon(&self, status: SessionStatus) -> String {
        match status {
            SessionStatus::Idle => "⠶".to_string(),
            SessionStatus::Running => {
                RUNNING_SPINNER[(self.tick as usize) % RUNNING_SPINNER.len()].to_string()
            }
            _ => status.symbol().to_string(),
        }
    }

    /// Glyph for the prompt header. Idle and running are icon-only so the
    /// header stays compact; other states keep their symbol and label.
    fn status_glyph(&self, status: SessionStatus) -> String {
        match status {
            SessionStatus::Idle | SessionStatus::Running => self.status_icon(status),
            _ => format!("{} {}", status.symbol(), status.as_str()),
        }
    }

    fn draw_body(&mut self, frame: &mut Frame, area: Rect) {
        // Sessions live in the floating picker (`Ctrl-G` / `/sessions`), not in
        // a permanent sidebar: panes get the whole body.
        if self.views.is_empty()
            || self.effective_layout(area) != PaneLayout::Tabby
            || area.height <= 1
        {
            self.draw_panes(frame, area);
            return;
        }
        // Keep the tab strip against the prompt box instead of the top of the
        // transcript. Fibonacci shows every pane, so it needs no tab strip at
        // all.
        let chunks = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(area);
        self.draw_panes(frame, chunks[0]);
        self.draw_tab_bar(frame, chunks[1]);
    }

    fn draw_tab_bar(&self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let theme = self.theme;
        let focused = self.focused.min(self.views.len().saturating_sub(1));
        let mut spans = Vec::with_capacity(self.views.len() * 2);
        for (index, view) in self.views.iter().enumerate() {
            let active = index == focused;
            let attention = !active
                && (view.session.status.needs_attention()
                    || self.approvals.contains_key(&view.session.id)
                    || self.inputs.contains_key(&view.session.id));
            let marker = if attention { "!" } else { "" };
            // Reserve enough room for the index and marker; the Paragraph
            // clips the rest when more tabs are open than fit.
            let name = truncate(&view.session.name, 24);
            let label = if active {
                format!("[{} {name}{marker}]", index + 1)
            } else {
                format!(" {} {name}{marker} ", index + 1)
            };
            let style = if active {
                Style::default()
                    .fg(composer_accent(theme))
                    .add_modifier(Modifier::BOLD)
            } else if attention {
                Style::default().fg(composer_warn(theme))
            } else {
                Style::default().fg(composer_muted(theme))
            };
            spans.push(Span::styled(label, style));
            if index + 1 < self.views.len() {
                spans.push(Span::styled(
                    "│",
                    Style::default().fg(composer_border(theme)),
                ));
            }
        }
        frame.render_widget(
            Paragraph::new(Line::from(spans))
                .style(Style::default().bg(composer_background(theme))),
            area,
        );
    }

    /// Resolve the layout for this frame. Horizontal layout always stays
    /// horizontal: panes get compact title/content rows instead of silently
    /// collapsing into tabby when the terminal is short.
    fn effective_layout(&self, _area: Rect) -> PaneLayout {
        self.layout
    }

    /// Horizontal layout shows the first pane plus the next four panes after
    /// it. If focus moved into a hidden pane, slide the tail window so the
    /// focused pane stays visible.
    fn horizontal_visible_indices(&self) -> Vec<usize> {
        let len = self.views.len();
        if len <= HORIZONTAL_TAIL_PANES + 1 {
            return (0..len).collect();
        }
        let start = if self.focused > HORIZONTAL_TAIL_PANES {
            self.focused + 1 - HORIZONTAL_TAIL_PANES
        } else {
            1
        };
        let mut indices = Vec::with_capacity(HORIZONTAL_TAIL_PANES + 1);
        indices.push(0);
        indices.extend(start..(start + HORIZONTAL_TAIL_PANES).min(len));
        indices
    }

    fn draw_panes(&mut self, frame: &mut Frame, area: Rect) {
        if self.views.is_empty() {
            self.pane_areas.clear();
            self.draw_empty_pane(frame, area);
            return;
        }
        self.focused = self.focused.min(self.views.len() - 1);
        let layout = self.effective_layout(area);
        if layout == PaneLayout::Tabby {
            // Only the active tab is visible; leave zero-sized hit boxes for
            // the hidden panes so clicks cannot reach them.
            let mut areas = vec![Rect::new(0, 0, 0, 0); self.views.len()];
            areas[self.focused] = area;
            self.pane_areas = areas;
            self.draw_pane(frame, area, self.focused);
            return;
        }
        let indices: Vec<usize> = if layout == PaneLayout::Horizontal {
            self.horizontal_visible_indices()
        } else {
            (0..self.views.len()).collect()
        };
        let rects = layout_pane_rects(layout, area, indices.len());
        let mut areas = vec![Rect::new(0, 0, 0, 0); self.views.len()];
        for (rect, index) in rects.iter().zip(indices.iter()) {
            areas[*index] = *rect;
        }
        self.pane_areas = areas;
        for (rect, index) in rects.into_iter().zip(indices) {
            self.draw_pane(frame, rect, index);
        }
    }

    fn draw_pane(&mut self, frame: &mut Frame, area: Rect, index: usize) {
        // Horizontal layout may squeeze a pane down to a title row. Draw it
        // rather than leaving a blank gap; only an empty slot is skipped.
        if area.width < 4 || area.height == 0 {
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
        let borders = if self.effective_layout(area) == PaneLayout::Tabby {
            let mut borders = Borders::ALL;
            borders.remove(Borders::BOTTOM);
            borders
        } else {
            pane_borders(&self.pane_areas, index)
        };
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
            user_lines: _,
        } = {
            let view = &self.views[index];
            let live = view.stream.as_ref().map(|stream| transcript::Live {
                reasoning: &stream.reasoning,
                text: &stream.text,
            });
            transcript::render_tail(
                live,
                &queued,
                &view.folds,
                inner_width.max(1),
                theme,
                self.tick,
            )
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
        if let Some(search) = self.transcript_search.as_ref() {
            if search.session_id == session_id {
                for (offset, line) in lines.iter_mut().enumerate() {
                    let absolute = scroll + offset;
                    if !search.matches.contains(&absolute) {
                        continue;
                    }
                    let style = if search.matches.get(search.current) == Some(&absolute) {
                        Style::default()
                            .fg(Color::Black)
                            .bg(Color::Yellow)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().bg(Color::Rgb(74, 68, 34))
                    };
                    for span in &mut line.spans {
                        span.style = span.style.patch(style);
                    }
                }
            }
        }
        let view = &self.views[index];
        let name = truncate(
            &view.session.name,
            inner_width.saturating_sub(6).clamp(1, 18),
        );
        let marker = if attention { "!" } else { "" };
        let border_color = if focused {
            // The active pane needs to read as the input target at a glance;
            // the muted border used for inactive panes is too subtle once
            // several sessions are tiled together.
            composer_focus(theme)
        } else if attention {
            composer_warn(theme)
        } else {
            Color::Rgb(48, 52, 62)
        };
        let mut border_style = Style::default().fg(border_color);
        let mut title_style = Style::default().fg(border_color);
        if focused {
            title_style = title_style.add_modifier(Modifier::BOLD);
        } else {
            title_style = title_style.add_modifier(Modifier::DIM);
        }
        let mut block = Block::default().borders(borders);
        if focused {
            block = block.border_set(PANE_FOCUS_BORDER);
        }
        if let Some(background) = theme.background {
            border_style = border_style.bg(background);
            title_style = title_style.bg(background);
            block = block.style(Style::default().bg(background));
        }
        // Keep the session name in the border color, but make the running
        // quadrant green and the idle glyph gray so the state is obvious.
        let status = view.session.status;
        let icon_style = title_style.fg(composer_status_color(status, theme));
        let title = Line::from(vec![
            Span::styled(format!(" {} ", index + 1), title_style),
            Span::styled(self.status_icon(status), icon_style),
            Span::styled(format!("{marker} {name} "), title_style),
        ]);
        block = block
            .title(title)
            .border_style(border_style)
            .title_style(title_style);
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
        let mut borders = Borders::ALL;
        borders.remove(Borders::BOTTOM);
        let block = Block::default().borders(borders).title(" transcript ");
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
        let panel = composer_background(self.theme);
        let text = composer_text(self.theme);
        let border_color = if focused {
            composer_border(self.theme)
        } else {
            Color::Rgb(46, 50, 60)
        };

        // Deny reasons temporarily replace the top border. Mode, session and
        // workspace context live in the statusline below the prompt.
        let title_width = area.width.saturating_sub(2) as usize;
        let top_left = if let Some(prompt) = &self.reason_prompt {
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
            truncate_line(line, title_width)
        } else {
            Line::default()
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color).bg(panel))
            .style(Style::default().bg(panel))
            .title_style(Style::default().bg(panel))
            .padding(Padding::new(1, 1, 0, 0))
            .title_top(top_left);

        // One cell of left padding keeps the text close to the border; one
        // cell on the right mirrors it.
        let inner_width = area.width.saturating_sub(4).max(1) as usize;
        let inner_height = area.height.saturating_sub(2).max(1) as usize;
        let layout = self.input.layout(inner_width, inner_height);
        let lines: Vec<Line> = layout
            .segments
            .iter()
            .skip(layout.offset)
            .take(inner_height)
            .map(|segment| self.input_line(segment))
            .collect();
        let paragraph = Paragraph::new(lines)
            .block(block)
            .style(Style::default().fg(text).bg(panel));
        frame.render_widget(paragraph, area);

        // The error dialog covers the prompt; don't leave a blinking input
        // cursor on top of it.
        if focused && self.error.is_none() && self.cmdline.is_none() {
            let x = area.x + 2 + layout.cursor_col.min(inner_width.saturating_sub(1)) as u16;
            let row = layout
                .cursor_line
                .saturating_sub(layout.offset)
                .min(inner_height.saturating_sub(1));
            let y = area.y + 1 + row as u16;
            frame.set_cursor_position((x, y));
        }
    }

    fn status_session(&self) -> Option<&Session> {
        self.focused_view()
            .map(|view| &view.session)
            .or_else(|| self.sessions.get(self.selected))
    }

    /// Primary statusline: Vim mode, agent mode, execution status,
    /// model/provider and reasoning variant. The workspace (cwd + branch) is
    /// right-aligned.
    fn status_bar_left(&self) -> Line<'static> {
        let theme = self.theme;
        let panel = composer_background(theme);
        let muted = composer_muted(theme);
        let warn = composer_warn(theme);
        let active = self.focus == Focus::Input;
        let input_mode = if active {
            self.input.mode()
        } else {
            Mode::Normal
        };
        let cmdline = self.cmdline.is_some();
        let mode_label = if cmdline {
            "C"
        } else {
            input_mode.short_label()
        };
        let mode_style = if cmdline {
            composer_mode_style(Mode::Normal, true, theme)
        } else {
            composer_mode_style(input_mode, active, theme)
        };
        let session = self.status_session();
        let session_mode = session.map(|session| session.mode).unwrap_or_default();
        let session_mode_label = session_mode.label();
        let mode_color = session_mode_color(session_mode, theme);
        let mut spans = vec![
            Span::styled(format!(" {mode_label} "), mode_style),
            Span::styled("· ", Style::default().fg(muted).bg(panel)),
            Span::styled(
                format!("{session_mode_label:<width$}", width = SESSION_MODE_WIDTH),
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
                    composer_status_color(session.status, theme),
                )
            } else {
                ("offline".to_string(), composer_danger(theme))
            };
            spans.push(Span::styled(
                status,
                Style::default().fg(status_color).bg(panel),
            ));

            let (model, provider, variant) = self.model_header_parts();
            let model = short_model(&model);
            if model != "-" && !model.is_empty() {
                spans.push(Span::styled(" · ", Style::default().fg(muted).bg(panel)));
                spans.push(Span::styled(
                    model.to_string(),
                    Style::default().fg(muted).bg(panel),
                ));
                if !provider.is_empty() {
                    spans.push(Span::styled(" · ", Style::default().fg(muted).bg(panel)));
                    spans.push(Span::styled(
                        provider,
                        Style::default()
                            .fg(muted)
                            .bg(panel)
                            .add_modifier(Modifier::DIM),
                    ));
                }
                if let Some(variant) = variant {
                    spans.push(Span::styled(" · ", Style::default().fg(muted).bg(panel)));
                    spans.push(Span::styled(variant, Style::default().fg(warn).bg(panel)));
                }
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
        Line::from(spans)
    }

    /// Secondary statusline context: transient app status, worktree and pending
    /// image count. It is dropped first when the bar gets narrow.
    fn status_bar_middle(&self) -> Option<Line<'static>> {
        let theme = self.theme;
        let panel = composer_background(theme);
        let muted = composer_muted(theme);
        let warn = composer_warn(theme);
        let mut spans = Vec::new();
        // Lifecycle chatter is redundant here: the pane title already carries
        // the session name, and the status glyph carries the connection state.
        let status = self.status.as_str();
        let show_status = !status.is_empty()
            && status != "attached"
            && status != "connecting…"
            && !status.starts_with("connected to eixd")
            && !status.starts_with("mode")
            && !status.starts_with("integration · merged")
            && !status.starts_with("patch ")
            && !status.starts_with("process ")
            && status != "running shell command…"
            && status != "reading shell output…"
            && !status.ends_with(" sent")
            && !status.starts_with("answered:")
            && status != "idle"
            && status != "running";
        if show_status {
            spans.push(Span::styled(
                self.status.clone(),
                Style::default().fg(muted).bg(panel),
            ));
        }
        if let Some(session) = self.status_session() {
            if let Some(state) = self.integrations.get(&session.id) {
                if !matches!(
                    state.status,
                    IntegrationStatus::Pending | IntegrationStatus::Merged
                ) {
                    if !spans.is_empty() {
                        spans.push(Span::styled(" · ", Style::default().fg(muted).bg(panel)));
                    }
                    spans.push(Span::styled(
                        format!("{} {}", state.status.symbol(), state.status.as_str()),
                        Style::default()
                            .fg(
                                if matches!(
                                    state.status,
                                    IntegrationStatus::Retrying | IntegrationStatus::Repairing
                                ) {
                                    warn
                                } else {
                                    muted
                                },
                            )
                            .bg(panel),
                    ));
                }
            }
        }
        if !self.pending_images.is_empty() {
            if !spans.is_empty() {
                spans.push(Span::styled(" · ", Style::default().fg(muted).bg(panel)));
            }
            spans.push(Span::styled(
                format!("{} img", self.pending_images.len()),
                Style::default().fg(warn).bg(panel),
            ));
        }
        if spans.is_empty() {
            None
        } else {
            spans.insert(0, Span::styled(" · ", Style::default().fg(muted).bg(panel)));
            Some(Line::from(spans))
        }
    }

    /// Workspace context kept against the right edge: cwd and git branch.
    fn status_bar_workspace(&self) -> Option<Line<'static>> {
        let panel = composer_background(self.theme);
        let muted = composer_muted(self.theme);
        let accent = composer_accent(self.theme);
        let session = self.status_session()?;
        let mut spans = vec![Span::styled(
            compact_path(&session.cwd.display().to_string()),
            Style::default().fg(muted).bg(panel),
        )];
        if self.git_cwd.as_ref() == Some(&session.cwd) {
            if let Some(branch) = &self.git_branch {
                spans.push(Span::styled(" · ", Style::default().fg(muted).bg(panel)));
                spans.push(Span::styled(
                    branch.clone(),
                    Style::default().fg(accent).bg(panel),
                ));
            }
        }
        Some(Line::from(spans))
    }

    fn status_bar_right(&self) -> Line<'static> {
        let theme = self.theme;
        let panel = composer_background(theme);
        let muted = composer_muted(theme);
        let warn = composer_warn(theme);
        let mut spans = Vec::new();
        let push = |spans: &mut Vec<Span<'static>>, text: String, color: Color| {
            if !spans.is_empty() {
                spans.push(Span::styled(" · ", Style::default().fg(muted).bg(panel)));
            }
            spans.push(Span::styled(text, Style::default().fg(color).bg(panel)));
        };
        if self.views.len() > 1 {
            push(
                &mut spans,
                format!("{}/{}", self.focused + 1, self.views.len()),
                muted,
            );
        }
        if let Some(view) = self.focused_view() {
            let queued = self.queue_len(&view.session.id);
            if queued > 0 {
                push(&mut spans, format!("⏳{queued}"), warn);
            }
        }
        let attention = self.attention_count();
        if attention > 0 {
            push(&mut spans, format!("⚠{attention}"), warn);
        }
        if !spans.is_empty() {
            spans.insert(0, Span::styled(" ", Style::default().bg(panel)));
        }
        Line::from(spans)
    }

    /// Background-attention notice shown in the middle of the status bar.
    /// Desktop popups are intentionally avoided, so this is the only alert UI.
    fn status_bar_alert(&self) -> Option<Line<'static>> {
        let text = self.last_alert.as_deref()?;
        if self.alert_at?.elapsed() >= ALERT_DISPLAY {
            return None;
        }
        let panel = composer_background(self.theme);
        Some(Line::from(Span::styled(
            format!(" {text} "),
            Style::default()
                .fg(composer_warn(self.theme))
                .bg(panel)
                .add_modifier(Modifier::BOLD),
        )))
    }

    fn draw_status_bar(&self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let panel = composer_background(self.theme);
        let background = Style::default().bg(panel);
        let left = self.status_bar_left();
        let middle = self.status_bar_middle();
        let counters = self.status_bar_right();
        let workspace = self.status_bar_workspace();
        let alert = self.status_bar_alert();

        let counters_width = counters.width();
        let workspace_width = workspace.as_ref().map_or(0, |line| line.width());
        let workspace_with_padding = if workspace_width > 0 {
            workspace_width + 1
        } else {
            0
        };
        let combined_right_width = if counters_width > 0 && workspace_width > 0 {
            counters_width + workspace_width + 3
        } else if workspace_width > 0 {
            workspace_with_padding
        } else {
            counters_width
        };

        // Keep cwd/branch visible when the left side grows; the counters are
        // dropped before the workspace on narrow terminals.
        let left_width = left
            .width()
            .min((area.width as usize).saturating_sub(workspace_with_padding))
            as u16;
        let right_available = area.width as usize - left_width as usize;
        let combined_fits = combined_right_width <= right_available;
        let right_width = if combined_fits {
            combined_right_width
        } else {
            workspace_with_padding.min(right_available)
        } as u16;
        let middle_width = area
            .width
            .saturating_sub(left_width)
            .saturating_sub(right_width);

        let mut right_spans = Vec::new();
        if combined_fits {
            right_spans.extend(counters.spans);
            if let Some(workspace) = workspace {
                if right_spans.is_empty() {
                    right_spans.push(Span::styled(" ", background));
                } else {
                    right_spans.push(Span::styled(
                        " · ",
                        Style::default().fg(composer_muted(self.theme)).bg(panel),
                    ));
                }
                right_spans.extend(workspace.spans);
            }
        } else if let Some(workspace) = workspace {
            right_spans.push(Span::styled(" ", background));
            right_spans.extend(workspace.spans);
        }
        let right = truncate_line(Line::from(right_spans), right_width as usize);

        let chunks = Layout::horizontal([
            Constraint::Length(left_width),
            Constraint::Min(0),
            Constraint::Length(right_width),
        ])
        .split(area);
        if left_width > 0 {
            let left = truncate_line(left, left_width as usize);
            frame.render_widget(Paragraph::new(left).style(background), chunks[0]);
        }
        if let Some(alert) = alert {
            frame.render_widget(
                Paragraph::new(alert)
                    .alignment(Alignment::Center)
                    .style(background),
                chunks[1],
            );
        } else if middle_width > 0 {
            if let Some(middle) = middle {
                let middle = truncate_line(middle, middle_width as usize);
                frame.render_widget(Paragraph::new(middle).style(background), chunks[1]);
            }
        }
        if right_width > 0 {
            frame.render_widget(
                Paragraph::new(right)
                    .alignment(Alignment::Right)
                    .style(background),
                chunks[2],
            );
        }
    }

    /// Vim-style command line below the status bar. It is always visible when
    /// the terminal has room, but stays blank until `:` opens it.
    fn draw_cmdline(&self, frame: &mut Frame, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let panel = composer_background(self.theme);
        let background = Style::default().bg(panel);
        let Some(cmdline) = self.cmdline.as_ref() else {
            frame.render_widget(Paragraph::new(Line::default()).style(background), area);
            return;
        };
        let line = Line::from(vec![
            Span::styled(
                cmdline.prompt.to_string(),
                Style::default()
                    .fg(composer_accent(self.theme))
                    .bg(panel)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                cmdline.text.clone(),
                Style::default().fg(composer_text(self.theme)).bg(panel),
            ),
        ]);
        frame.render_widget(Paragraph::new(line).style(background), area);
        let cursor = UnicodeWidthStr::width(&cmdline.text[..cmdline.cursor_byte()]) as u16;
        let max_x = area.x + area.width.saturating_sub(1);
        let x = (area.x + 1 + cursor).min(max_x);
        frame.set_cursor_position((x, area.y));
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

    /// Dynamic `:` command list floated just above the command line. Typing
    /// filters it; Tab completes a match, while Ctrl-J/K and Ctrl-N/P move the
    /// selection without rewriting the typed prefix.
    fn draw_cmdline_menu(&mut self, frame: &mut Frame, cmdline_area: Rect) {
        // Keep the statusline visible: the menu grows upward from the row
        // immediately above the command line.
        let status_y = cmdline_area.y.saturating_sub(1);
        if status_y < 3 {
            return;
        }
        let matches = self.cmdline_matches();
        if matches.is_empty() {
            return;
        }
        let height = (matches.len() as u16 + 2).min(15).min(status_y);
        let visible = height.saturating_sub(2) as usize;
        if visible == 0 {
            return;
        }
        let cursor = self.cmdline_menu_cursor.unwrap_or(0).min(matches.len() - 1);
        let start = cursor.saturating_sub(visible.saturating_sub(1));
        let width = cmdline_area.width.min(88);
        let rect = Rect::new(cmdline_area.x, status_y - height, width, height);
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
                format!(":{}", spec.name)
            } else {
                format!(":{} {}", spec.name, spec.args)
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
            " : commands ",
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
        // Process inspection is an overlay like the pickers, not a full-screen
        // mode; keep the same inset instead of covering the terminal.
        let rect = search_dialog_rect(area);
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
        // Queue a lazy preview whenever the selected row changes. The event
        // loop sends it after this draw so the UI never blocks on file/history
        // reads.
        self.sync_picker_preview(&filtered, selected);
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

        let rect = search_dialog_rect(area);
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
        let body = draw_search_field(frame, inner, &query, accent, muted, dialog_bg);
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
            let height = preview_area.height as usize;
            let mut wrapped: Vec<String> = lines
                .iter()
                .flat_map(|line| wrap_text(line, width))
                .collect();
            // Session previews are conversations: keep the newest lines in
            // view instead of showing the oldest part of the fetched window.
            if matches!(kind, PickerKind::Sessions | PickerKind::NewPane) && wrapped.len() > height
            {
                wrapped.drain(..wrapped.len() - height);
            }
            let body: Vec<Line> = wrapped.into_iter().take(height).map(Line::from).collect();
            let preview = Paragraph::new(body)
                .style(Style::default().fg(preview_text).bg(dialog_bg))
                .block(
                    Block::default()
                        .borders(Borders::LEFT)
                        .border_style(Style::default().fg(muted).bg(dialog_bg)),
                );
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
                // Keep the provider column aligned under the longest model id
                // in the catalog, not just the currently filtered rows.
                let model_width = if kind == PickerKind::Models {
                    picker
                        .items
                        .iter()
                        .map(|item| item.label.chars().count())
                        .max()
                        .unwrap_or(0)
                } else {
                    0
                };
                let separator = if kind == PickerKind::Models {
                    "    "
                } else {
                    "  "
                };
                filtered
                    .iter()
                    .filter_map(|&index| picker.items.get(index))
                    .map(|item| {
                        let label = if kind == PickerKind::Models {
                            format!("{:<width$}", item.label, width = model_width)
                        } else {
                            item.label.clone()
                        };
                        let mut spans = vec![Span::styled(label, label_style)];
                        if let Some(detail) = &item.detail {
                            if !detail.is_empty() {
                                spans.push(Span::styled(
                                    format!("{separator}{detail}"),
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

    /// Queue a preview request for the selected file or session row.
    fn sync_picker_preview(&mut self, filtered: &[usize], selected: usize) {
        let Some(picker) = self.picker.as_ref() else {
            return;
        };
        let Some(provider) = picker.kind.preview_kind() else {
            return;
        };
        let Some(&index) = filtered.get(selected) else {
            return;
        };
        let value = match picker.kind {
            PickerKind::Sessions | PickerKind::NewPane => {
                self.sessions.get(index).map(|session| session.id.clone())
            }
            PickerKind::Files => picker.items.get(index).map(|item| item.value.clone()),
            _ => None,
        };
        let Some(value) = value else {
            return;
        };
        if picker.preview.key.as_deref() == Some(value.as_str()) {
            return;
        }
        let session_id = if picker.kind == PickerKind::Files {
            let Some(session_id) = self.focused_session_id() else {
                return;
            };
            Some(session_id)
        } else {
            None
        };
        if let Some(picker) = self.picker.as_mut() {
            picker.preview = PickerPreview {
                key: Some(value.clone()),
                text: None,
                error: None,
                loading: true,
            };
            picker.preview_request = Some((provider.to_string(), session_id, value));
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
                if let Some(text) = &picker.preview.text {
                    return text.lines().map(str::to_string).collect();
                }
                let Some(session) = self.sessions.get(index) else {
                    return Vec::new();
                };
                let mut lines = vec![
                    session.name.clone(),
                    String::new(),
                    format!("id          {}", session.id),
                    format!("status      {}", session.status.as_str()),
                    format!("cwd         {}", session.cwd.display()),
                    format!("mode        {}", session.mode.as_str()),
                ];
                if let Some(worktree) = &session.worktree {
                    lines.push(format!("worktree    {}", worktree.display()));
                }
                lines.push(format!(
                    "model       {}",
                    session.model.as_deref().unwrap_or("(daemon default)")
                ));
                if let Some(error) = &picker.preview.error {
                    lines.push(String::new());
                    lines.push(format!("conversation preview: {error}"));
                } else if picker.preview.loading {
                    lines.push(String::new());
                    lines.push("loading conversation…".into());
                }
                lines
            }
            PickerKind::Files => {
                if let Some(text) = &picker.preview.text {
                    return text.lines().map(str::to_string).collect();
                }
                if let Some(error) = &picker.preview.error {
                    return vec![format!("preview: {error}")];
                }
                if picker.preview.loading {
                    return vec!["loading file…".into()];
                }
                let Some(item) = picker.items.get(index) else {
                    return Vec::new();
                };
                item.preview
                    .as_ref()
                    .map(|preview| preview.lines().map(str::to_string).collect())
                    .unwrap_or_else(|| vec![item.label.clone()])
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

/// Rows for the `/subagents` picker. The runtime override exposes exactly the
/// supported levels instead of a free-form value.
fn subagent_picker_items(current: usize) -> Vec<PickerItem> {
    eix_core::SUBAGENT_CONCURRENCY_OPTIONS
        .into_iter()
        .map(|value| {
            let label = if value == current {
                format!("{value}  (current)")
            } else {
                value.to_string()
            };
            PickerItem::new(value.to_string())
                .with_label(label)
                .with_detail("maximum concurrent spawn_agent calls")
        })
        .collect()
}

/// Rows for the interactive `/perm` picker.
fn mode_picker_items(current: SessionMode) -> Vec<PickerItem> {
    [
        (
            SessionMode::Plan,
            "hard read-only; no writes and only read-only shell commands",
        ),
        (
            SessionMode::Build,
            "workspace-write; risky commands ask, workspace edits apply",
        ),
        (
            SessionMode::Auto,
            "workspace-write; allow unruled default asks (no external writes)",
        ),
        (
            SessionMode::Bypass,
            "full access; external writes allowed, unsandboxed fallback",
        ),
    ]
    .into_iter()
    .map(|(mode, detail)| {
        let label = if mode == current {
            format!("{}  (current)", mode.label())
        } else {
            mode.label().to_string()
        };
        PickerItem::new(mode.as_str())
            .with_label(label)
            .with_detail(detail)
    })
    .collect()
}

fn timeline_timestamp(ts: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_else(|| "—".to_string())
}

/// Rows for `/timeline`: only this session's user messages, newest first.
/// The full text is used for fuzzy search and the right-side preview.
fn timeline_picker_items(events: &[SessionEvent]) -> Vec<PickerItem> {
    events
        .iter()
        .rev()
        .filter(|event| event.kind == EventKind::UserMessage)
        .filter_map(|event| {
            let payload =
                serde_json::from_value::<UserMessagePayload>(event.payload.clone()).ok()?;
            let text = payload.text.trim();
            let preview = if text.is_empty() {
                "[image]".to_string()
            } else {
                text.chars().take(4_000).collect()
            };
            let first = preview.lines().next().unwrap_or("[image]");
            Some(
                PickerItem::new(event.seq.to_string())
                    .with_label(format!(
                        "{}  {}",
                        timeline_timestamp(event.ts),
                        truncate(first, 64)
                    ))
                    .with_detail(preview.clone())
                    .with_preview(preview),
            )
        })
        .collect()
}

/// Rows for the interactive `/colorscheme` picker. The first row clears the preset;
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

/// Rows for `/model`. `@default` clears the session's pinned model.
fn model_picker_items(models: &[eix_core::ModelInfo]) -> Vec<PickerItem> {
    let mut items = vec![PickerItem::new("@default")
        .with_label("@default")
        .with_detail("daemon default")
        .with_preview("Clear this session's model override.\n\nUse /model to pin a model again.")];
    for model in models {
        // The configured default model belongs to the built-in `default`
        // provider even when its id starts with a model-family prefix.
        let provider = if model.default {
            "default".to_string()
        } else {
            model.provider.clone()
        };
        // The model id is the bold first column. The provider is rendered as
        // the aligned second column by `picker_rows`.
        let mut preview = vec![
            model.id.clone(),
            model.name.clone(),
            format!("provider  {provider}"),
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
                .with_label(model.display_id())
                .with_detail(provider.clone())
                .with_preview(preview.join("\n")),
        );
        // A variant is a separate row so it is discoverable without knowing
        // the `id#variant` syntax.
        for variant in &model.variants {
            let id = format!("{}#{variant}", model.id);
            let label = format!("{}#{variant}", model.display_id());
            items.push(
                PickerItem::new(&id)
                    .with_label(label)
                    .with_detail(provider.clone())
                    .with_preview(format!(
                        "{id}\n{} ({}) with reasoning variant `{variant}`.",
                        model.name, provider
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
        .filter(|agent| agent.role.can_primary())
        .map(|agent| {
            let mut detail = agent.description.clone();
            detail.push_str(&format!("  · {}", agent.role.as_str()));
            if let Some(model) = &agent.model {
                detail.push_str(&format!("  · {model}"));
            }
            let mut preview = vec![
                agent.name.clone(),
                String::new(),
                agent.description.clone(),
                format!("role        {}", agent.role.as_str()),
            ];
            if let Some(model) = &agent.model {
                preview.push(format!("model       {model}"));
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

fn mcp_picker_items(servers: &[eix_core::McpServerInfo]) -> Vec<PickerItem> {
    servers
        .iter()
        .map(|server| {
            let state = if !server.enabled {
                "disabled".to_string()
            } else if server.error.is_some() {
                "error".to_string()
            } else {
                format!("{} tool(s)", server.tools)
            };
            let mut preview = vec![
                server.name.clone(),
                format!("command   {}", server.command),
                format!("tools     {}", server.tools),
                format!("state     {state}"),
            ];
            if server.resources > 0 {
                preview.push(format!("resources {}", server.resources));
            }
            if server.prompts > 0 {
                preview.push(format!("prompts   {}", server.prompts));
            }
            if let Some(error) = &server.error {
                preview.push(String::new());
                preview.push(format!("error: {error}"));
            }
            PickerItem::new(&server.name)
                .with_detail(state)
                .with_preview(preview.join("\n"))
        })
        .collect()
}

fn todo_picker_items(items: &[TodoItem]) -> Vec<PickerItem> {
    items
        .iter()
        .map(|item| {
            let mark = match item.status.as_str() {
                "completed" => "[x]",
                "in_progress" => "[~]",
                "cancelled" => "[-]",
                _ => "[ ]",
            };
            PickerItem::new(&item.content)
                .with_label(format!("{mark} {}", item.content))
                .with_detail(item.status.clone())
                .with_preview(format!("{}\n\nstatus: {}", item.content, item.status))
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

/// Rows for the `Ctrl-P` command palette. `value` is dispatched by
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
            "processes",
            "background processes",
            "List and inspect processes (P)",
        ),
        ("approvals", "approval inbox", "Pending approvals (A)"),
        (
            "mcp",
            "mcp servers",
            "Browse configured MCP servers and tool counts",
        ),
        ("todos", "task list", "Browse the focused session's tasks"),
        (
            "attention",
            "next attention",
            "Jump to the next pane needing approval or input",
        ),
        (
            "close-pane",
            "close pane",
            "Close the focused pane (Space c)",
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
            "subagents",
            "subagent concurrency",
            "Set the maximum parallel spawn_agent calls",
        ),
        (
            "toggle-mode",
            "cycle execution mode",
            "Cycle Plan → Build → Auto → Bypass (Tab; Shift-Tab reverses)",
        ),
        (
            "perm",
            "permission mode",
            "Choose Plan, Build, Auto or Bypass for this session (Space m)",
        ),
        (
            "skills",
            "skills",
            "Browse installed skills with a body preview",
        ),
        (
            "model",
            "model",
            "Search provider model catalogs and switch this session",
        ),
        (
            "colorscheme",
            "colorscheme",
            "Choose a colorscheme with live preview",
        ),
        (
            "help",
            "key help",
            "Search the key and command reference (?)",
        ),
        ("keys", "key bindings", "Search just the key map"),
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
struct TurnRef {
    session_id: String,
    turn_id: String,
}

#[derive(Deserialize)]
struct TurnRetrying {
    session_id: String,
    turn_id: String,
    #[serde(default)]
    attempt: u32,
    #[serde(default)]
    max_attempts: u32,
    #[serde(default)]
    delay_ms: u64,
    #[serde(default)]
    kind: String,
    #[serde(default)]
    reason: String,
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
    cost: Option<f64>,
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

fn default_allow_always() -> bool {
    true
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
    #[serde(default)]
    risk: String,
    #[serde(default)]
    risk_categories: Vec<String>,
    #[serde(default)]
    reasons: Vec<String>,
    #[serde(default)]
    rule: Option<String>,
    #[serde(default)]
    rule_source: String,
    #[serde(default)]
    sandbox: String,
    #[serde(default)]
    scopes: Vec<String>,
    #[serde(default = "default_allow_always")]
    allow_always: bool,
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
struct AgentSpawnedParams {
    session_id: String,
    agent_id: String,
    #[serde(default)]
    call_id: String,
    #[serde(default)]
    name: String,
}

#[derive(Deserialize)]
struct AgentCompletedParams {
    session_id: String,
    agent_id: String,
    #[serde(default)]
    call_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Deserialize)]
struct AttentionParams {
    session_id: String,
    status: SessionStatus,
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
        "edit" => {
            let old = string("oldString").len();
            let new = string("newString").len();
            format!("edit {} (-{old} +{new} bytes)", string("path"))
        }
        "patch" => {
            let files = string("patchText")
                .lines()
                .filter(|line| line.starts_with("+++ "))
                .count();
            match files {
                0 => "patch".to_string(),
                1 => "patch (1 file)".to_string(),
                count => format!("patch ({count} files)"),
            }
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
                description: "search the command and key reference",
            },
            HelpEntry {
                key: "/keys",
                description: "search just the key map",
            },
            HelpEntry {
                key: "/new",
                description: "start a new session in the current workspace",
            },
            HelpEntry {
                key: "/new-pane",
                description: "choose an existing session for a new pane",
            },
            HelpEntry {
                key: "/new-worktree",
                description: "start a new session in an isolated git worktree",
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
                description: "switch the focused pane's session",
            },
            HelpEntry {
                key: "/files",
                description: "search workspace files and insert a path",
            },
            HelpEntry {
                key: "/worktrees",
                description: "search git worktrees and insert a path",
            },
            HelpEntry {
                key: "/branches",
                description: "search git branches and insert a name",
            },
            HelpEntry {
                key: "/processes",
                description: "inspect background processes",
            },
            HelpEntry {
                key: "/approvals",
                description: "open the pending-approval inbox",
            },
            HelpEntry {
                key: "/mcp",
                description: "browse MCP servers and their tools",
            },
            HelpEntry {
                key: "/todos",
                description: "browse the model's current task list",
            },
            HelpEntry {
                key: "/attention",
                description: "jump to the next pane needing attention",
            },
            HelpEntry {
                key: "/cancel",
                description: "cancel the focused session's running turn",
            },
            HelpEntry {
                key: "/toggle-mode",
                description: "cycle Plan → Build → Auto → Bypass (Shift-Tab reverses)",
            },
            HelpEntry {
                key: "/timeline",
                description: "search this session's user inputs; Enter jumps to the message",
            },
            HelpEntry {
                key: "/archive",
                description: "hide the session from the picker",
            },
            HelpEntry {
                key: "/delete",
                description: "delete the session permanently (confirm twice)",
            },
            HelpEntry {
                key: "/undo",
                description: "hide the newest turn",
            },
            HelpEntry {
                key: "/redo",
                description: "restore the most recently undone turn",
            },
            HelpEntry {
                key: "/skills [name] [args]",
                description: "browse skills with a preview, or insert one with arguments",
            },
            HelpEntry {
                key: "/image <path>",
                description: "attach images (paste/drop a path or file:// URI); png/jpg/gif/webp",
            },
            HelpEntry {
                key: "/colorscheme",
                description: "browse colorschemes with live preview; Enter applies, Esc restores",
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
                key: "/perm",
                description: "choose Plan · Build · Auto · Bypass in a search picker (alias: /mode)",
            },
            HelpEntry {
                key: "/exit",
                description: "save the draft and quit eix",
            },
            HelpEntry {
                key: "/quit",
                description: "save the draft and quit eix",
            },
            HelpEntry {
                key: "/close",
                description: "save the draft and close the focused session (the last one quits eix)",
            },
        ],
    },
    HelpSection {
        title: "keys",
        entries: &[
            HelpEntry {
                key: "i",
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
                key: ":",
                description: "normal mode: open the command line (:q closes the pane, :qa quits)",
            },
            HelpEntry {
                key: "Esc Esc",
                description: "normal mode: interrupt the running turn, or leave for the transcript",
            },
            HelpEntry {
                key: "Enter",
                description: "insert: send (queued while busy)",
            },
            HelpEntry {
                key: "Ctrl-J",
                description: "newline (insert mode)",
            },
            HelpEntry {
                key: "Space o/f/w/b",
                description: "prompt & transcript: new pane / files / worktrees / branches",
            },
            HelpEntry {
                key: "Space p",
                description: "prompt & transcript: processes",
            },
            HelpEntry {
                key: "Space R/x",
                description: "prompt & transcript: approvals / cancel",
            },
            HelpEntry {
                key: "Space n/c",
                description: "prompt & transcript: new session / close pane",
            },
            HelpEntry {
                key: "Shift+, / Shift+.",
                description: "prompt normal & transcript: previous / next pane",
            },
            HelpEntry {
                key: "Ctrl-W Ctrl-W",
                description: "prompt normal & transcript: focus the next pane (wraps around)",
            },
            HelpEntry {
                key: "Space v / Space t / Space r",
                description: "horizontal / tabby / fibonacci layout",
            },
            HelpEntry {
                key: "Space L",
                description: "cycle grid / horizontal / tabby / fibonacci layout",
            },
            HelpEntry {
                key: "Ctrl-1 … Ctrl-9",
                description: "focus the Nth open session / tabby tab (Ctrl-7 opens the model picker on some terminals)",
            },
            HelpEntry {
                key: "session order",
                description: "panes keep their order; Ctrl-1…9 jumps directly, Shift+,/. or Ctrl-W Ctrl-W moves prev/next",
            },
            HelpEntry {
                key: "Space W",
                description: "prompt & transcript: new worktree",
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
                key: "Ctrl-P",
                description: "command palette (prompt normal mode and transcript)",
            },
            HelpEntry {
                key: "Ctrl-G",
                description: "session picker (switch the focused pane's session)",
            },
            HelpEntry {
                key: "Ctrl-H",
                description: "search this session's user-input history and jump",
            },
            HelpEntry {
                key: "Ctrl-T",
                description: "cycle reasoning (default medium): low / medium / high",
            },
            HelpEntry {
                key: "Ctrl-/",
                description: "open the searchable model picker",
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
                key: "Tab",
                description: "cycle Plan → Build → Auto → Bypass → Plan",
            },
            HelpEntry {
                key: "Shift-Tab",
                description: "cycle the other way: Build → Plan → Bypass → Auto → Build",
            },
            HelpEntry {
                key: "Space m",
                description: "open the full Plan / Build / Auto / Bypass mode picker",
            },
            HelpEntry {
                key: "Space n",
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
                key: "Ctrl-J / Ctrl-K · Enter",
                description: "pending card: move the checkbox selection / run or answer the highlighted item",
            },
            HelpEntry {
                key: "1-9 · j / k",
                description: "answer a question directly / move the card selection",
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
                key: "Space p",
                description: "background processes (x kills, r refreshes)",
            },
            HelpEntry {
                key: "/ · ? · #",
                description: "search this transcript (/ forward; ?/# backward; n/N next/prev; Esc clears)",
            },
            HelpEntry {
                key: "F1",
                description: "open this searchable help anywhere",
            },
            HelpEntry {
                key: "Z / Ctrl-C / Ctrl-D",
                description: "quit from any mode (draft is saved first)",
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
                key: "ask_user",
                description: "approval/question cards use - [ ] / - [x] rows; the first option is recommended.",
            },
        ],
    },
];

enum HelpRow {
    Blank,
    Section(&'static str),
    Entry(&'static HelpEntry),
}

fn help_rows_in(query: &str, scope: Option<&str>) -> Vec<HelpRow> {
    let query = query.trim().to_lowercase();
    let mut rows = Vec::new();
    for section in HELP_SECTIONS {
        if scope.is_some_and(|scope| scope != section.title) {
            continue;
        }
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

#[cfg(test)]
fn help_entry_count(query: &str) -> usize {
    help_entry_count_in(query, None)
}

fn help_entry_count_in(query: &str, scope: Option<&str>) -> usize {
    let query = query.trim().to_lowercase();
    HELP_SECTIONS
        .iter()
        .filter(|section| scope.is_none_or(|scope| scope == section.title))
        .flat_map(|section| section.entries.iter())
        .filter(|entry| entry.matches(&query))
        .count()
}

fn draw_help(
    frame: &mut Frame,
    area: Rect,
    query: &str,
    selected: usize,
    scope: Option<&str>,
    theme: Theme,
) {
    let dialog_bg = dialog_background(theme);
    let rows = help_rows_in(query, scope);
    let matches = help_entry_count_in(query, scope);
    let total: usize = HELP_SECTIONS
        .iter()
        .filter(|section| scope.is_none_or(|scope| scope == section.title))
        .map(|section| section.entries.len())
        .sum();

    let rect = search_dialog_rect(area);
    frame.render_widget(Clear, rect);
    frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);
    let title = if scope.is_some() {
        format!(" keys ({matches}/{total}) ")
    } else {
        format!(" help ({matches}/{total}) ")
    };
    frame.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .style(Style::default().bg(dialog_bg)),
        rect,
    );

    let inner = rect.inner(ratatui::layout::Margin::new(1, 1));
    let list_area = draw_search_field(
        frame,
        inner,
        query,
        composer_accent(theme),
        composer_muted(theme),
        dialog_bg,
    );

    let selected_row = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| matches!(row, HelpRow::Entry(_)))
        .nth(selected)
        .map(|(index, _)| index);
    let visible = list_area.height as usize;
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
                            format!("{:<15} ", entry.key),
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
        list_area,
    );
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

/// Tile `count` panes inside `area`. One pane is fullscreen, two split
/// vertically, three use a tall left pane plus two stacked on the right, and
/// everything else uses a near-square grid.
/// Adjacent panes share one line: the left pane owns the right border, and
/// each pane's top border separates it from the pane above. The prompt's top
/// border closes the transcript, so panes never draw their own bottom edge;
/// this keeps the conversation and input box one line apart instead of two.
fn pane_borders(areas: &[Rect], index: usize) -> Borders {
    let Some(rect) = areas.get(index).copied() else {
        let mut borders = Borders::ALL;
        borders.remove(Borders::BOTTOM);
        return borders;
    };
    let overlaps = |a_start: u16, a_len: u16, b_start: u16, b_len: u16| {
        a_start < b_start.saturating_add(b_len) && b_start < a_start.saturating_add(a_len)
    };

    let mut borders = Borders::ALL;
    borders.remove(Borders::BOTTOM);
    for (other_index, other) in areas.iter().enumerate() {
        if other_index == index {
            continue;
        }
        let left_neighbour = other.x.saturating_add(other.width) == rect.x
            && overlaps(other.y, other.height, rect.y, rect.height);
        if left_neighbour {
            borders.remove(Borders::LEFT);
        }
    }
    borders
}

fn layout_pane_rects(layout: PaneLayout, area: Rect, count: usize) -> Vec<Rect> {
    match layout {
        PaneLayout::Grid => pane_rects(area, count),
        PaneLayout::Horizontal => horizontal_pane_rects(area, count),
        PaneLayout::Fibonacci => fibonacci_pane_rects(area, count),
        PaneLayout::Tabby => vec![area],
    }
}

/// One column, with the first pane taking over half the height.
fn horizontal_pane_rects(area: Rect, count: usize) -> Vec<Rect> {
    match count {
        0 => Vec::new(),
        1 => vec![area],
        _ => {
            let rest = count - 1;
            let height = area.height as usize;
            // Keep the first pane at its normal 55% share regardless of how
            // many panes are open. The panes below split the remainder and
            // compact to title-only rows when space is short.
            let first = if height >= count * 2 {
                (height * 55 / 100).max(1).min(height.saturating_sub(rest))
            } else if height >= count {
                // Too short for a 55% first pane; distribute evenly and give
                // the remainder to the first pane.
                height / count + height % count
            } else {
                // More panes than rows: show the first pane and whatever fits.
                height
            };
            let mut constraints = Vec::with_capacity(count);
            constraints.push(Constraint::Length(first as u16));
            constraints.extend((0..rest).map(|_| Constraint::Fill(1)));
            Layout::vertical(constraints).split(area).to_vec()
        }
    }
}

/// Spiral tiling: each pane takes a slice of the remaining rectangle. Cuts
/// cycle left, top, right, bottom, so the first pane gets the largest,
/// full-height pane and the rest spiral inward.
fn fibonacci_pane_rects(area: Rect, count: usize) -> Vec<Rect> {
    if count == 0 {
        return Vec::new();
    }
    let mut rects = Vec::with_capacity(count);
    let mut remaining = area;
    for index in 0..count {
        if index + 1 == count {
            rects.push(remaining);
            break;
        }
        let (vertical, take_second) = match index % 4 {
            0 => (true, false),
            1 => (false, false),
            2 => (true, true),
            _ => (false, true),
        };
        let first = if index == 0 { 55 } else { 50 };
        let parts = if vertical {
            Layout::horizontal([
                Constraint::Percentage(first),
                Constraint::Percentage(100 - first),
            ])
            .split(remaining)
        } else {
            Layout::vertical([
                Constraint::Percentage(first),
                Constraint::Percentage(100 - first),
            ])
            .split(remaining)
        };
        if take_second {
            rects.push(parts[1]);
            remaining = parts[0];
        } else {
            rects.push(parts[0]);
            remaining = parts[1];
        }
    }
    rects
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
    use crossterm::event::{
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    };
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
    crossterm::execute!(
        stdout,
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        Show
    )
    .ok();
    let status = std::process::Command::new(program)
        .args(parts)
        .arg(path)
        .status();
    enable_raw_mode().ok();
    crossterm::execute!(
        stdout,
        EnterAlternateScreen,
        Hide,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .ok();
    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(anyhow::anyhow!("editor exited with {status}")),
        Err(err) => Err(anyhow::anyhow!("cannot launch {program}: {err}")),
    }
}

/// Risk label for an approval card (DESIGN 9.3): shell commands are called
/// out separately from workspace writes and read-only tools.
fn risk_level_color(level: &str) -> Color {
    match level.to_ascii_uppercase().as_str() {
        "LOW" => Color::Green,
        "MEDIUM" => Color::Yellow,
        "HIGH" => Color::LightRed,
        "CRITICAL" => Color::Red,
        _ => Color::Yellow,
    }
}

fn approval_risk(tool: &str) -> (&'static str, Color) {
    match tool {
        "run_command" => ("shell", Color::Red),
        "write_file" | "edit" | "patch" | "edit_file" | "apply_patch" => {
            ("workspace-write", Color::Yellow)
        }
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
    let file_change = transcript::is_file_change_tool(&approval.tool);
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
    // off, but keep the action list and hint on screen. The daemon prefixes
    // new approvals with `[RISK]`; older/restored cards fall back to the tool
    // heuristic.
    let (risk, risk_color, body) = match approval
        .summary
        .strip_prefix('[')
        .and_then(|rest| rest.split_once("] "))
    {
        Some((level, body)) => (level, risk_level_color(level), body),
        None => {
            let (risk, color) = approval_risk(&approval.tool);
            (risk, color, approval.summary.as_str())
        }
    };
    let risk_label = format!("  [{risk}]");
    let prefix_width = 2; // "⚠ "
    let body_width = inner
        .saturating_sub(prefix_width + UnicodeWidthStr::width(risk_label.as_str()))
        .max(8);
    let mut summary_lines = wrap_text(body, body_width);
    let max_height = area.height.saturating_sub(1) as usize;
    let capacity = max_height.saturating_sub(2); // rows inside the borders
    let hint_rows = 1;
    // The summary can be a full shell command. Wrap it, but never let it push
    // the action list or hint off the card.
    let summary_cap = capacity.saturating_sub(actions.len() + hint_rows).max(1);
    if summary_lines.len() > summary_cap {
        summary_lines.truncate(summary_cap.saturating_sub(1));
        summary_lines.push("…".to_string());
    }
    let summary_rows = summary_lines.len();
    // On a short pane, scroll the action list instead of clipping the
    // highlighted row. The diff only gets space after all actions fit.
    let action_rows = actions
        .len()
        .min(capacity.saturating_sub(summary_rows + hint_rows))
        .max(1);
    let action_start = if action_rows >= actions.len() {
        0
    } else {
        cursor
            .saturating_sub(action_rows.saturating_sub(1))
            .min(actions.len().saturating_sub(action_rows))
    };
    let diff_budget = capacity.saturating_sub(summary_rows + action_rows + hint_rows);
    let diff_reserve = usize::from(diff_len > 0 && diff_budget > 0);
    // File changes use the whole review budget: the card is the primary
    // vimdiff-style view for write_file / edit / patch. Other tools keep the
    // compact 8-row preview so a long command result does not take over.
    let max_diff = diff_budget.saturating_sub(diff_reserve);
    let max_diff = if file_change {
        max_diff
    } else {
        max_diff.min(8)
    };
    let shown = diff_len.min(max_diff);
    let truncated = diff_len > shown && diff_budget > 0;
    // Leave the pane's title row visible and never underflow `y` on a pane
    // that starts at the top of the screen.
    let content_rows = summary_rows + action_rows + shown + usize::from(truncated) + hint_rows;
    let height = (content_rows + 2).min(max_height).max(3) as u16;
    let rect = Rect {
        x: area.x + 1,
        y: area.y + area.height - height - 1,
        width: area.width.saturating_sub(2),
        height,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);

    let mut lines = Vec::with_capacity(content_rows);
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
    for (index, action) in actions
        .iter()
        .enumerate()
        .skip(action_start)
        .take(action_rows)
    {
        let check = if index == cursor { "x" } else { " " };
        let mut text = format!("- [{check}] {}", approval.action_label(*action));
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
            "Ctrl-J/K or j/k select · Enter confirm · 1-9 jump · y/n · x cancel",
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
    let height = (pending.options.len() + 4)
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
    // Small panes clip the card; keep the highlighted option visible instead
    // of always rendering the first N rows.
    let visible = (height as usize).saturating_sub(4).max(1);
    let start = cursor.saturating_sub(visible.saturating_sub(1));
    for (index, option) in pending.options.iter().enumerate().skip(start).take(visible) {
        let check = if index == cursor { "x" } else { " " };
        let mut text = format!("- [{check}] {}", option.label);
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
            "Ctrl-J/K or j/k select · Enter answer · 1-9 jump · x cancel",
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

/// Draw the query row and separator shared by the theme picker and help/keys.
/// Returns the remaining area for results.
fn draw_search_field(
    frame: &mut Frame,
    area: Rect,
    query: &str,
    accent: Color,
    muted: Color,
    dialog_bg: Color,
) -> Rect {
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(area);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("❯ ", Style::default().fg(accent)),
            Span::raw(query.to_string()),
        ]))
        .style(Style::default().bg(dialog_bg)),
        chunks[0],
    );
    frame.render_widget(
        Paragraph::new("─".repeat(chunks[1].width as usize))
            .style(Style::default().fg(muted).bg(dialog_bg)),
        chunks[1],
    );
    chunks[2]
}

/// Shared size for searchable popups such as the theme picker and help/keys.
fn search_dialog_rect(area: Rect) -> Rect {
    let width = (area.width as u32 * 8 / 10).max(44).min(area.width as u32) as u16;
    let height = (area.height as u32 * 7 / 10)
        .max(12)
        .min(area.height as u32) as u16;
    centered(area, width, height)
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
    let model = match model.rsplit_once('#') {
        Some((base, "none")) => base,
        _ => model,
    };
    model
        .rsplit_once('/')
        .map(|(_, tail)| tail)
        .unwrap_or(model)
}

/// Split `model#variant` into its routing id and reasoning variant.
fn split_model_variant(selection: &str) -> (&str, Option<&str>) {
    match selection.rsplit_once('#') {
        Some((id, variant)) if !variant.is_empty() => (id, Some(variant)),
        _ => (selection, None),
    }
}

/// Compact completion state for the footer. Token totals and throughput stay
/// out of the status bar; usage is still recorded in the event log.
fn turn_summary(done: &TurnCompleted) -> String {
    match done.cost {
        Some(cost) => format!("idle · ${cost:.4}"),
        None => "idle".to_string(),
    }
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

fn composer_focus(theme: Theme) -> Color {
    theme.function.unwrap_or(COMPOSER_FOCUS)
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

/// The four execution modes use four different colors: Plan is read-only
/// amber, Build is workspace-write blue, Auto is classifier-gated light orange
/// and Bypass is unguarded pink. Auto keeps its own orange because the theme
/// palettes have no slot that stays distinct from every other mode color.
fn session_mode_color(mode: SessionMode, theme: Theme) -> Color {
    match mode {
        SessionMode::Plan => composer_warn(theme),
        SessionMode::Build => composer_accent(theme),
        SessionMode::Auto => COMPOSER_ORANGE,
        SessionMode::Bypass => composer_danger(theme),
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
    fn startup_attaches_the_session_for_the_launch_directory() {
        let cwd = std::env::current_dir().expect("cwd");
        let mut app = App::new(None);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let other = Session::new("other", "other", "/tmp/eix-other-workspace");
        let here = Session::new("here", "here", cwd);
        app.handle_event(Event::Sessions(vec![other, here]), &tx);

        assert_eq!(app.selected, 1);
        assert!(matches!(rx.try_recv(), Ok(Command::Attach(id)) if id == "here"));
    }

    #[test]
    fn startup_creates_a_session_when_the_launch_directory_has_none() {
        let mut app = App::new(None);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        app.handle_event(
            Event::Sessions(vec![Session::new(
                "other",
                "other",
                "/tmp/eix-other-workspace",
            )]),
            &tx,
        );

        assert!(matches!(
            rx.try_recv(),
            Ok(Command::Create {
                worktree: None,
                cwd: None,
                ..
            })
        ));
    }

    #[test]
    fn adjacent_panes_share_one_border() {
        let area = Rect::new(0, 0, 100, 40);
        let two = pane_rects(area, 2);
        assert!(pane_borders(&two, 0).contains(Borders::RIGHT));
        assert!(!pane_borders(&two, 1).contains(Borders::LEFT));

        let stacked = vec![Rect::new(0, 0, 20, 10), Rect::new(0, 10, 20, 10)];
        assert!(!pane_borders(&stacked, 0).contains(Borders::BOTTOM));
        assert!(!pane_borders(&stacked, 1).contains(Borders::BOTTOM));
        assert!(pane_borders(&stacked, 1).contains(Borders::TOP));
    }

    #[test]
    fn timeline_picker_lists_only_user_inputs_and_jumps() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let events = vec![
            SessionEvent::new(
                "s1",
                1,
                EventKind::UserMessage,
                serde_json::json!({"text": "first request"}),
            ),
            SessionEvent::new(
                "s1",
                2,
                EventKind::AssistantMessage,
                serde_json::json!({"text": "first answer"}),
            ),
            SessionEvent::new(
                "s1",
                3,
                EventKind::UserMessage,
                serde_json::json!({"text": "second request"}),
            ),
        ];
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "timeline", "/tmp"),
            events,
        ));
        app.focused = 0;
        let _ = render_text(&mut app, 80, 20);
        let line = app.views[0]
            .user_lines
            .iter()
            .find(|(seq, _)| *seq == 3)
            .map(|(_, line)| *line)
            .expect("user line");

        app.open_picker(PickerKind::Timeline, &tx);
        let picker = app.picker.as_ref().expect("timeline picker");
        assert_eq!(picker.kind, PickerKind::Timeline);
        let values: Vec<&str> = picker
            .items
            .iter()
            .map(|item| item.value.as_str())
            .collect();
        assert_eq!(values, ["3", "1"]);
        assert_eq!(picker.selected, 0);
        assert!(picker.items[0].label.contains(':'));
        assert!(picker.items[0]
            .preview
            .as_deref()
            .is_some_and(|preview| preview.contains("second request")));

        app.picker_enter(PickerKind::Timeline, &tx);
        assert!(app.picker.is_none());
        assert!(!app.views[0].follow);
        assert_eq!(app.views[0].scroll, line.saturating_sub(2));
        assert!(app.status.contains("jumped to e3"));
    }

    #[test]
    fn timeline_picker_is_isolated_per_session() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            vec![SessionEvent::new(
                "s1",
                1,
                EventKind::UserMessage,
                serde_json::json!({"text": "first session"}),
            )],
        ));
        app.views.push(SessionView::new(
            Session::new("s2", "two", "/tmp"),
            vec![
                SessionEvent::new(
                    "s2",
                    1,
                    EventKind::UserMessage,
                    serde_json::json!({"text": "second session A"}),
                ),
                SessionEvent::new(
                    "s2",
                    2,
                    EventKind::UserMessage,
                    serde_json::json!({"text": "second session B"}),
                ),
            ],
        ));
        app.focused = 1;

        app.open_picker(PickerKind::Timeline, &tx);
        let picker = app.picker.as_ref().expect("timeline picker");
        let previews: Vec<&str> = picker
            .items
            .iter()
            .filter_map(|item| item.preview.as_deref())
            .collect();
        assert_eq!(previews.len(), 2);
        assert!(previews
            .iter()
            .all(|text| text.starts_with("second session")));
        assert!(previews.iter().all(|text| !text.contains("first session")));
    }

    #[test]
    fn ctrl_h_opens_the_timeline_picker() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let events = vec![SessionEvent::new(
            "s1",
            1,
            EventKind::UserMessage,
            serde_json::json!({"text": "hello"}),
        )];
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "timeline", "/tmp"),
            events,
        ));
        app.focused = 0;

        app.handle_key(
            KeyEvent::new(KeyCode::Char('h'), KeyModifiers::CONTROL),
            &tx,
        );
        let picker = app.picker.as_ref().expect("timeline picker");
        assert_eq!(picker.kind, PickerKind::Timeline);
        assert_eq!(picker.items.len(), 1);
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
    fn focused_pane_border_uses_the_accent_color() {
        let mut app = app_with_panes(&["a", "b"]);
        app.focused = 0;
        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let left = app.pane_areas[0];
        let right = app.pane_areas[1];
        let focused = &buffer[(left.x, left.y + 2)];
        let inactive = &buffer[(right.x + right.width - 1, right.y + 2)];

        assert_eq!(
            focused.fg,
            composer_focus(app.theme),
            "focused pane border should be brighter"
        );
        assert_eq!(
            focused.symbol(),
            "┊",
            "focused pane border should use the fine dashed line"
        );
        assert_eq!(
            inactive.fg,
            Color::Rgb(48, 52, 62),
            "inactive pane border should stay muted"
        );
        assert_eq!(inactive.symbol(), "│");
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
    fn horizontal_layout_gives_the_first_pane_most_height() {
        let area = Rect::new(0, 0, 100, 40);
        let rects = layout_pane_rects(PaneLayout::Horizontal, area, 5);
        assert_eq!(rects.len(), 5);
        assert!(rects[0].height > rects[1].height);
        for rect in &rects {
            assert_eq!(rect.x, area.x);
            assert_eq!(rect.width, area.width);
            assert!(rect.y + rect.height <= area.y + area.height);
        }
        for pair in rects.windows(2) {
            assert_eq!(pair[0].y + pair[0].height, pair[1].y);
        }
    }

    #[test]
    fn horizontal_layout_never_falls_back_to_tabs() {
        let mut app = app_with_panes(&["a", "b", "c", "d", "e"]);
        app.layout = PaneLayout::Horizontal;
        assert_eq!(
            app.effective_layout(Rect::new(0, 0, 80, 16)),
            PaneLayout::Horizontal
        );
        assert_eq!(
            app.effective_layout(Rect::new(0, 0, 80, 40)),
            PaneLayout::Horizontal
        );
    }

    #[test]
    fn horizontal_layout_keeps_every_pane_visible_when_short() {
        for height in [12, 16, 20, 24] {
            let mut app = app_with_panes(&["a", "b", "c", "d", "e"]);
            app.layout = PaneLayout::Horizontal;
            app.focused = 0;
            let rendered = render_text(&mut app, 80, height);
            assert!(
                !rendered.contains("[1 session a]"),
                "horizontal layout fell back to tabs at height {height}:\n{rendered}"
            );
            for id in ["a", "b", "c", "d", "e"] {
                assert!(
                    rendered.contains(&format!("session {id}")),
                    "pane {id} missing at height {height}:\n{rendered}"
                );
            }
        }
    }

    #[test]
    fn horizontal_layout_shows_head_plus_four_following_panes() {
        let mut app = app_with_panes(&["a", "b", "c", "d", "e", "f", "g", "h"]);
        app.layout = PaneLayout::Horizontal;
        let rendered = render_text(&mut app, 80, 30);
        assert!(!rendered.contains("[1 session a]"), "{rendered}");
        for id in ["a", "b", "c", "d", "e"] {
            assert!(
                rendered.contains(&format!("session {id}")),
                "pane {id} should be visible:\n{rendered}"
            );
        }
        for id in ["f", "g", "h"] {
            assert!(
                !rendered.contains(&format!("session {id}")),
                "pane {id} should be hidden:\n{rendered}"
            );
        }
        assert_eq!(
            app.pane_areas.iter().filter(|rect| rect.height > 0).count(),
            5
        );

        // The first pane keeps the same height as with only five panes.
        let mut five = app_with_panes(&["a", "b", "c", "d", "e"]);
        five.layout = PaneLayout::Horizontal;
        render_text(&mut five, 80, 30);
        assert_eq!(app.pane_areas[0], five.pane_areas[0]);
    }

    #[test]
    fn horizontal_layout_slides_to_a_focused_hidden_pane() {
        let mut app = app_with_panes(&["a", "b", "c", "d", "e", "f", "g", "h"]);
        app.layout = PaneLayout::Horizontal;
        app.focused = 7;
        let rendered = render_text(&mut app, 80, 30);

        // Head plus the four following panes ending at the focused one.
        for id in ["a", "e", "f", "g", "h"] {
            assert!(
                rendered.contains(&format!("session {id}")),
                "pane {id} should be visible:\n{rendered}"
            );
        }
        for id in ["b", "c", "d"] {
            assert!(
                !rendered.contains(&format!("session {id}")),
                "pane {id} should be hidden:\n{rendered}"
            );
        }
        assert!(app.pane_areas[7].height > 0);
        assert_eq!(app.pane_areas[1].height, 0);
    }

    #[test]
    fn fibonacci_layout_spirals_from_the_first_pane() {
        let area = Rect::new(0, 0, 100, 40);
        for count in 1..=7 {
            let rects = layout_pane_rects(PaneLayout::Fibonacci, area, count);
            assert_eq!(rects.len(), count);
            for rect in &rects {
                assert!(rect.x >= area.x);
                assert!(rect.y >= area.y);
                assert!(rect.x + rect.width <= area.x + area.width);
                assert!(rect.y + rect.height <= area.y + area.height);
            }
            if count > 1 {
                assert_eq!(rects[0].height, area.height);
                assert!(rects[0].width > rects[1].width);
                assert_eq!(rects[1].x, rects[0].x + rects[0].width);
                assert_eq!(rects[1].y, area.y);
            }
            if count >= 4 {
                // The spiral turns around the right edge after the top pane.
                assert!(rects[2].x > rects[3].x);
            }
        }
    }

    fn app_with_panes(ids: &[&str]) -> App {
        let mut app = App::new(None);
        for id in ids {
            app.views.push(SessionView::new(
                Session::new(*id, format!("session {id}"), "/tmp"),
                Vec::new(),
            ));
        }
        app.connected = true;
        app
    }

    fn view_order(app: &App) -> Vec<&str> {
        app.views
            .iter()
            .map(|view| view.session.id.as_str())
            .collect()
    }

    fn type_text(app: &mut App, tx: &UnboundedSender<Command>, text: &str) {
        for ch in text.chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), tx);
        }
    }

    #[test]
    fn sending_and_status_changes_keep_the_session_order() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["a", "b", "c"]);
        app.focused = 0;

        let submitted = SubmitResult {
            session: app.views[2].session.clone(),
            turn_id: "turn-c".into(),
        };
        app.submitted(submitted);
        assert_eq!(view_order(&app), vec!["a", "b", "c"]);
        assert_eq!(app.focused, 0);
        assert_eq!(app.focused_session_id().as_deref(), Some("a"));

        let mut idle = app.views[2].session.clone();
        idle.status = SessionStatus::Idle;
        app.notification(
            eix_core::Notification::new("session.updated", serde_json::to_value(idle).unwrap()),
            &tx,
        );
        assert_eq!(view_order(&app), vec!["a", "b", "c"]);
        assert_eq!(app.focused, 0);
    }

    #[test]
    fn session_updates_do_not_replace_the_focused_draft() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["a", "b"]);
        app.focused = 0;
        app.focus = Focus::Input;
        app.input_session = Some("a".to_string());
        app.input.set_text("draft for a");

        let mut ready = app.views[1].session.clone();
        ready.status = SessionStatus::AwaitingApproval;
        app.notification(
            eix_core::Notification::new("session.updated", serde_json::to_value(ready).unwrap()),
            &tx,
        );
        assert_eq!(view_order(&app), vec!["a", "b"]);
        assert_eq!(app.focused_session_id().as_deref(), Some("a"));
        assert_eq!(app.input.text(), "draft for a");

        // The tick-driven draft sync must keep the text attached to pane 1.
        app.tick(&tx);
        assert_eq!(app.focused_session_id().as_deref(), Some("a"));
        assert_eq!(app.input.text(), "draft for a");
    }

    #[test]
    fn jumping_to_a_session_keeps_the_linked_order() {
        let mut app = app_with_panes(&["a", "b", "c", "d"]);
        app.focused = 0;

        assert!(app.jump_to_session("c"));
        assert_eq!(app.focused, 2);
        assert_eq!(app.focused_session_id().as_deref(), Some("c"));
        assert_eq!(view_order(&app), vec!["a", "b", "c", "d"]);

        app.cycle_pane(1);
        assert_eq!(app.focused_session_id().as_deref(), Some("d"));
        app.cycle_pane(1);
        assert_eq!(app.focused_session_id().as_deref(), Some("a"));
        app.cycle_pane(-1);
        assert_eq!(app.focused_session_id().as_deref(), Some("d"));
        assert_eq!(view_order(&app), vec!["a", "b", "c", "d"]);

        assert!(!app.jump_to_session("missing"));
    }

    #[test]
    fn ctrl_digits_switch_tabby_tabs_in_insert_mode() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["a", "b", "c"]);
        app.layout = PaneLayout::Tabby;
        app.focus_input();
        app.focused = 0;

        app.handle_key(
            KeyEvent::new(KeyCode::Char('3'), KeyModifiers::CONTROL),
            &tx,
        );

        assert_eq!(app.focused, 2);
        assert_eq!(app.views[app.focused].session.id, "c");
        assert_eq!(app.input.mode(), Mode::Insert);
        assert_eq!(app.status, "tab 3/3 focused");

        // The global shortcut also works while the prompt is in normal mode.
        app.input.enter_normal();
        app.handle_key(
            KeyEvent::new(KeyCode::Char('1'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.focused, 0);
        assert_eq!(app.views[app.focused].session.id, "a");
        assert_eq!(app.input.mode(), Mode::Normal);
        assert_eq!(app.status, "tab 1/3 focused");
    }

    #[test]
    fn ctrl_digits_and_leader_letters_switch_panes_and_layouts() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["a", "b", "c"]);
        app.focus = Focus::Transcript;
        app.focused = 0;

        app.handle_key(
            KeyEvent::new(KeyCode::Char('2'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.focused, 1);

        for (leader, expected) in [
            ('v', PaneLayout::Horizontal),
            ('t', PaneLayout::Tabby),
            ('r', PaneLayout::Fibonacci),
            ('l', PaneLayout::Grid),
        ] {
            app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
            app.handle_key(
                KeyEvent::new(KeyCode::Char(leader), KeyModifiers::NONE),
                &tx,
            );
            assert_eq!(app.layout, expected, "leader {leader}");
        }
        assert_eq!(app.status, "layout: layout_grid");
    }

    #[test]
    fn every_layout_renders_a_three_pane_session_set() {
        for layout in [
            PaneLayout::Grid,
            PaneLayout::Horizontal,
            PaneLayout::Tabby,
            PaneLayout::Fibonacci,
        ] {
            let mut app = app_with_panes(&["a", "b", "c"]);
            app.layout = layout;
            app.focused = 0;
            let rendered = render_text(&mut app, 80, 30);
            assert!(
                rendered.contains("session a"),
                "{layout:?} lost the first pane:\n{rendered}"
            );
        }
    }

    #[test]
    fn tab_layout_renders_all_tabs_but_only_the_active_pane() {
        let mut app = app_with_panes(&["a", "b", "c"]);
        app.layout = PaneLayout::Tabby;
        app.focused = 1;

        let rendered = render_text(&mut app, 80, 20);
        assert!(rendered.contains("session a"), "{rendered}");
        assert!(rendered.contains("session b"), "{rendered}");
        assert!(rendered.contains("session c"), "{rendered}");
        // The active tab is bracketed; inactive tabs are not separate panes.
        assert!(rendered.contains("[2 session b]"), "{rendered}");

        // Tabs sit directly above the prompt box; the session banner lives in
        // the statusline on the final row.
        let lines: Vec<&str> = rendered.lines().collect();
        let tab_row = lines
            .iter()
            .position(|line| line.contains("[2 session b]"))
            .expect("tab strip");
        let prompt_top = tab_row + 1;
        assert!(lines[prompt_top].contains('╭'), "{rendered}");
        assert!(status_bar_row(&rendered).contains("Build  ·"), "{rendered}");
    }

    #[test]
    fn fibonacci_layout_shows_every_pane_without_a_tab_strip() {
        let mut app = app_with_panes(&["a", "b", "c"]);
        app.layout = PaneLayout::Fibonacci;
        app.focused = 1;

        let rendered = render_text(&mut app, 80, 24);
        assert!(rendered.contains("session a"), "{rendered}");
        assert!(rendered.contains("session b"), "{rendered}");
        assert!(rendered.contains("session c"), "{rendered}");
        assert!(!rendered.contains("[2 session b]"), "{rendered}");
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn render_transcript_at(view: &SessionView, width: usize, tick: u64) -> String {
        let live = view.stream.as_ref().map(|stream| transcript::Live {
            reasoning: &stream.reasoning,
            text: &stream.text,
        });
        transcript::render_with_subagents(
            &view.events,
            live,
            &[],
            &view.folds,
            width,
            Theme::default(),
            tick,
            &view.subagents,
        )
        .lines
        .iter()
        .map(line_text)
        .collect::<Vec<_>>()
        .join("\n")
    }

    fn render_transcript(view: &SessionView, width: usize) -> String {
        render_transcript_at(view, width, 0)
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

        // File writes stay open by default so their diff/content is visible.
        // The status still sits directly below the summary, not right-aligned.
        let expanded = render_transcript(&view, 60);
        let lines: Vec<&str> = expanded.lines().collect();
        let summary_line = lines
            .iter()
            .position(|line| line.contains("write out.txt (+5 bytes)"))
            .expect("summary line");
        assert!(!lines[summary_line].contains("ok"), "{expanded}");
        assert_eq!(lines[summary_line + 1].trim(), "ok", "{expanded}");
        assert!(lines[summary_line + 1].starts_with("      "), "{expanded}");
        assert!(expanded.contains("wrote 5 bytes to out.txt"));
        assert!(expanded.contains("hello"));
        assert!(!expanded.contains("output"), "{expanded}");
        assert!(!expanded.contains("content"), "{expanded}");

        // The explicit fold still collapses it.
        view.folds.insert(FoldId::Event(1), false);
        let collapsed = render_transcript(&view, 60);
        assert!(!collapsed.contains("wrote 5 bytes to out.txt"));
        assert!(!collapsed.contains("hello"));
    }

    #[test]
    fn edit_and_patch_cards_open_by_default() {
        let session = Session::new("s1", "tools", "/tmp");
        let events = vec![
            SessionEvent::new(
                "s1",
                1,
                EventKind::ToolCall,
                serde_json::json!({
                    "call_id": "c1",
                    "tool": "edit",
                    "arguments": {
                        "path": "src/lib.rs",
                        "oldString": "old edit",
                        "newString": "new edit",
                    },
                }),
            ),
            SessionEvent::new(
                "s1",
                2,
                EventKind::PatchProposed,
                serde_json::json!({
                    "patch_id": "p1",
                    "call_id": "c1",
                    "tool": "edit",
                    "path": "src/lib.rs",
                    "summary": "edit src/lib.rs",
                    "diff": "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old edit\n+new edit\n",
                }),
            ),
            SessionEvent::new(
                "s1",
                3,
                EventKind::ToolResult,
                serde_json::json!({
                    "call_id": "c1",
                    "tool": "edit",
                    "ok": true,
                    "denied": false,
                    "output": "edited src/lib.rs",
                }),
            ),
            SessionEvent::new(
                "s1",
                4,
                EventKind::ToolCall,
                serde_json::json!({
                    "call_id": "c2",
                    "tool": "patch",
                    "arguments": {"patchText": "raw patch text"},
                }),
            ),
            SessionEvent::new(
                "s1",
                5,
                EventKind::PatchProposed,
                serde_json::json!({
                    "patch_id": "p2",
                    "call_id": "c2",
                    "tool": "patch",
                    "path": "src/one.rs",
                    "summary": "patch (2 files)",
                    "diff": "--- a/src/one.rs\n+++ b/src/one.rs\n@@ -1 +1 @@\n-old one\n+new one\n--- a/src/two.rs\n+++ b/src/two.rs\n@@ -1 +1 @@\n-old two\n+new two\n",
                }),
            ),
            SessionEvent::new(
                "s1",
                6,
                EventKind::ToolResult,
                serde_json::json!({
                    "call_id": "c2",
                    "tool": "patch",
                    "ok": true,
                    "denied": false,
                    "output": "applied patch",
                }),
            ),
        ];
        let view = SessionView::new(session, events);
        let rendered = render_transcript(&view, 80);
        assert!(rendered.contains("new edit"), "{rendered}");
        assert!(rendered.contains("new one"), "{rendered}");
        assert!(rendered.contains("new two"), "{rendered}");
        assert!(rendered.contains("── src/one.rs"), "{rendered}");
        assert!(rendered.contains("── src/two.rs"), "{rendered}");
        // The raw edit/patch arguments are not dumped next to the diff.
        assert!(!rendered.contains("oldString"), "{rendered}");
        assert!(!rendered.contains("patchText"), "{rendered}");
    }

    #[test]
    fn auto_applied_write_renders_its_diff() {
        let session = Session::new("s1", "tools", "/tmp");
        let events = vec![
            SessionEvent::new(
                "s1",
                1,
                EventKind::ToolCall,
                serde_json::json!({
                    "call_id": "c1",
                    "tool": "write_file",
                    "arguments": {"path": "out.txt", "content": "new line\n"},
                }),
            ),
            SessionEvent::new(
                "s1",
                2,
                EventKind::PatchApplied,
                serde_json::json!({
                    "patch_id": "p1",
                    "call_id": "c1",
                    "path": "out.txt",
                    "applied": true,
                    "conflict": false,
                    "diff": "--- a/out.txt\n+++ b/out.txt\n@@ -1 +1 @@\n-old line\n+new line\n",
                }),
            ),
            SessionEvent::new(
                "s1",
                3,
                EventKind::ToolResult,
                serde_json::json!({
                    "call_id": "c1",
                    "tool": "write_file",
                    "ok": true,
                    "denied": false,
                    "output": "wrote 9 bytes to out.txt",
                }),
            ),
        ];
        let view = SessionView::new(session, events);
        let rendered = render_transcript(&view, 80);
        assert!(rendered.contains("old line"), "{rendered}");
        assert!(rendered.contains("new line"), "{rendered}");
        assert!(rendered.contains("── out.txt"), "{rendered}");
        assert!(rendered.contains('│'), "{rendered}");
    }

    #[test]
    fn applied_patch_diff_replaces_the_staged_preview() {
        let session = Session::new("s1", "tools", "/tmp");
        let events = vec![
            SessionEvent::new(
                "s1",
                1,
                EventKind::ToolCall,
                serde_json::json!({
                    "call_id": "c1",
                    "tool": "write_file",
                    "arguments": {"path": "out.rs", "content": "fn raw(){}\n"},
                }),
            ),
            SessionEvent::new(
                "s1",
                2,
                EventKind::PatchProposed,
                serde_json::json!({
                    "patch_id": "p1",
                    "call_id": "c1",
                    "tool": "write_file",
                    "path": "out.rs",
                    "summary": "write out.rs",
                    "diff": "--- a/out.rs\n+++ b/out.rs\n@@ -1 +1 @@\n-fn old(){}\n+fn raw(){}\n",
                }),
            ),
            SessionEvent::new(
                "s1",
                3,
                EventKind::PatchApplied,
                serde_json::json!({
                    "patch_id": "p1",
                    "call_id": "c1",
                    "path": "out.rs",
                    "applied": true,
                    "conflict": false,
                    "diff": "--- a/out.rs\n+++ b/out.rs\n@@ -1 +1,3 @@\n-fn old(){}\n+fn raw() {}\n+\n+// formatted\n",
                }),
            ),
            SessionEvent::new(
                "s1",
                4,
                EventKind::ToolResult,
                serde_json::json!({
                    "call_id": "c1",
                    "tool": "write_file",
                    "ok": true,
                    "denied": false,
                    "output": "wrote out.rs",
                }),
            ),
        ];
        let view = SessionView::new(session, events);
        let rendered = render_transcript(&view, 80);
        // The applied event carries the final, formatted bytes; the staged
        // preview must not keep showing the pre-format content.
        assert!(rendered.contains("// formatted"), "{rendered}");
        assert!(!rendered.contains("+fn raw(){}"), "{rendered}");
    }

    fn parent_spawn_events() -> Vec<SessionEvent> {
        vec![SessionEvent::new(
            "parent",
            1,
            EventKind::ToolCall,
            serde_json::json!({
                "call_id": "call-1",
                "tool": "spawn_agent",
                "arguments": {"name": "scout", "prompt": "inspect the parser"},
            }),
        )]
    }

    #[test]
    fn subagent_card_has_a_fixed_live_window_and_folds() {
        let session = Session::new("parent", "parent", "/tmp");
        let mut view = SessionView::new(session, parent_spawn_events());
        view.upsert_subagent("child-1".into(), "call-1".into(), "parent/scout-1".into());
        view.push_subagent_event(
            "child-1",
            SessionEvent::new(
                "child-1",
                1,
                EventKind::UserMessage,
                serde_json::json!({"text": "inspect the parser"}),
            ),
        );
        view.push_subagent_event(
            "child-1",
            SessionEvent::new(
                "child-1",
                2,
                EventKind::ToolCall,
                serde_json::json!({
                    "call_id": "c2",
                    "tool": "read_file",
                    "arguments": {"path": "src/parser.rs"},
                }),
            ),
        );
        view.push_subagent_event(
            "child-1",
            SessionEvent::new(
                "child-1",
                3,
                EventKind::ToolResult,
                serde_json::json!({
                    "call_id": "c2",
                    "tool": "read_file",
                    "ok": true,
                    "denied": false,
                    "output": "fn parse() {}",
                }),
            ),
        );

        let rendered = render_transcript(&view, 60);
        assert!(
            rendered.contains("↳ parent/scout-1 · running"),
            "{rendered}"
        );
        assert!(rendered.contains("▶ inspect the parser"), "{rendered}");
        assert!(rendered.contains("⚙ read src/parser.rs"), "{rendered}");
        assert!(
            rendered.contains("✓ read_file: fn parse() {}"),
            "{rendered}"
        );
        // Five fixed rows between the window borders.
        assert_eq!(rendered.matches("│ ").count(), 5, "{rendered}");
        assert!(
            rendered.contains('╭') && rendered.contains('╰'),
            "{rendered}"
        );

        view.folds.insert(FoldId::Event(1), false);
        let folded = render_transcript(&view, 60);
        assert!(folded.contains("↳ parent/scout-1 · running"), "{folded}");
        assert!(!folded.contains("read src/parser.rs"), "{folded}");
        assert!(!folded.contains('╭'), "{folded}");
    }

    #[test]
    fn completed_subagent_shows_only_its_result() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("parent", "parent", "/tmp"),
            parent_spawn_events(),
        ));
        app.handle_event(
            Event::Notification(eix_core::Notification::new(
                "agent.spawned",
                serde_json::json!({
                    "session_id": "parent",
                    "agent_id": "child-1",
                    "call_id": "call-1",
                    "name": "parent/scout-1",
                }),
            )),
            &tx,
        );
        app.handle_event(
            Event::Notification(eix_core::Notification::new(
                "session.event",
                serde_json::to_value(SessionEvent::new(
                    "child-1",
                    1,
                    EventKind::ToolCall,
                    serde_json::json!({
                        "call_id": "c2",
                        "tool": "read_file",
                        "arguments": {"path": "src/parser.rs"},
                    }),
                ))
                .unwrap(),
            )),
            &tx,
        );
        let running = render_transcript(&app.views[0], 60);
        assert!(running.contains("⚙ read src/parser.rs"), "{running}");

        app.handle_event(
            Event::Notification(eix_core::Notification::new(
                "agent.completed",
                serde_json::json!({
                    "session_id": "parent",
                    "agent_id": "child-1",
                    "call_id": "call-1",
                    "name": "parent/scout-1",
                    "ok": true,
                    "result": "the parser is fine",
                }),
            )),
            &tx,
        );
        let done = render_transcript(&app.views[0], 60);
        assert!(done.contains("↳ parent/scout-1 · done"), "{done}");
        assert!(done.contains("the parser is fine"), "{done}");
        assert!(!done.contains("read src/parser.rs"), "{done}");
        assert!(!done.contains('╭'), "{done}");
    }

    #[test]
    fn failed_subagent_shows_only_the_error() {
        let session = Session::new("parent", "parent", "/tmp");
        let mut view = SessionView::new(session, parent_spawn_events());
        view.upsert_subagent("child-1".into(), "call-1".into(), "parent/scout-1".into());
        view.push_subagent_event(
            "child-1",
            SessionEvent::new(
                "child-1",
                1,
                EventKind::ToolCall,
                serde_json::json!({
                    "call_id": "c2",
                    "tool": "read_file",
                    "arguments": {"path": "src/parser.rs"},
                }),
            ),
        );
        view.complete_subagent(
            "child-1",
            "call-1",
            "parent/scout-1",
            false,
            None,
            Some("provider exploded".into()),
        );

        let rendered = render_transcript(&view, 60);
        assert!(rendered.contains("↳ parent/scout-1 · failed"), "{rendered}");
        assert!(rendered.contains("provider exploded"), "{rendered}");
        assert!(!rendered.contains("read src/parser.rs"), "{rendered}");
        assert!(!rendered.contains('╭'), "{rendered}");
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

        // The command is no longer elided and its status sits on the line
        // below the wrapped command instead of at the right edge.
        assert!(lines[0].contains("$ echo"), "{expanded}");
        assert!(!lines[0].contains("ok"), "{expanded}");
        assert!(!expanded.contains('…'), "{expanded}");
        let status_line = lines
            .iter()
            .position(|line| line.trim() == "ok")
            .expect("status line");
        assert!(status_line > 0, "{expanded}");
        assert!(
            lines[status_line - 1].starts_with("      ") && lines[status_line - 1].contains('x'),
            "{expanded}"
        );
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
        // No role prefixes; user turns sit right and agent turns sit left.
        assert!(!rendered.contains("you>"));
        assert!(!rendered.contains("eix>"));
        let lines: Vec<&str> = rendered.lines().collect();
        let user = lines.iter().find(|line| line.contains("hi there")).unwrap();
        assert!(
            user.starts_with(' ') && user.contains("│ hi there │"),
            "user turn should be a right-aligned bubble: {user:?}"
        );
        assert!(lines.iter().any(|line| line.contains("▾ Thinking")));
        // Stored thinking is expanded by default.
        assert!(rendered.contains("pondering"));
        let mut view = view;
        view.folds.insert(FoldId::Event(2), false);
        assert!(!render_transcript(&view, 40).contains("pondering"));
    }

    #[test]
    fn user_messages_render_in_a_bubble() {
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
        assert_eq!(lines.len(), 3, "{rendered}");
        assert!(rendered.contains("╭"), "{rendered}");
        assert!(rendered.contains("│ hi there │"), "{rendered}");
        assert!(rendered.contains("╰"), "{rendered}");
        assert!(
            lines.iter().all(|line| UnicodeWidthStr::width(*line) == 38),
            "{rendered:?}"
        );
        assert!(!rendered.contains("you>"), "{rendered}");
        assert!(!rendered.contains("eix>"), "{rendered}");
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
    fn transcript_hides_mode_changes() {
        let session = Session::new("s1", "modes", "/tmp");
        let events = vec![
            SessionEvent::new(
                "s1",
                1,
                EventKind::UserMessage,
                serde_json::json!({ "text": "start in plan" }),
            ),
            SessionEvent::new(
                "s1",
                2,
                EventKind::Mode,
                serde_json::json!({
                    "from": "plan",
                    "to": "build",
                    "temporary": false,
                }),
            ),
            SessionEvent::new(
                "s1",
                3,
                EventKind::AssistantMessage,
                serde_json::json!({
                    "text": "now building",
                    "model": "m",
                    "cancelled": false,
                }),
            ),
        ];
        let view = SessionView::new(session, events);
        let rendered = render_transcript(&view, 60);
        assert!(rendered.contains("start in plan"), "{rendered}");
        assert!(rendered.contains("now building"), "{rendered}");
        assert!(!rendered.contains("mode plan"), "{rendered}");
        assert!(!rendered.contains('◆'), "{rendered}");
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
    fn transcript_aligns_user_right_and_agent_left() {
        let session = Session::new("s1", "align", "/tmp");
        let events = vec![
            SessionEvent::new(
                "s1",
                1,
                EventKind::UserMessage,
                serde_json::json!({ "text": "question" }),
            ),
            SessionEvent::new(
                "s1",
                2,
                EventKind::AssistantMessage,
                serde_json::json!({ "text": "answer", "model": "test" }),
            ),
        ];
        let mut app = App::new(None);
        app.views.push(SessionView::new(session, events));
        app.focused = 0;
        render_text(&mut app, 40, 12);

        let lines = &app.views[0].history_cache.as_ref().unwrap().lines;
        let user = lines
            .iter()
            .find(|line| text_line(line).contains("question"))
            .expect("user line");
        let agent = lines
            .iter()
            .find(|line| text_line(line).contains("answer"))
            .expect("agent line");
        let indent = |line: &Line<'static>| {
            let text = text_line(line);
            text.len().saturating_sub(text.trim_start().len())
        };
        assert!(
            indent(user) > indent(agent),
            "user line should sit right of the agent line: {:?} vs {:?}",
            text_line(user),
            text_line(agent)
        );

        let rendered = render_text(&mut app, 40, 20);
        let user_x = rendered
            .lines()
            .find(|line| line.contains("question"))
            .and_then(|line| line.find("question"))
            .expect("rendered user line");
        let agent_x = rendered
            .lines()
            .find(|line| line.contains("answer"))
            .and_then(|line| line.find("answer"))
            .expect("rendered agent line");
        assert!(
            user_x > agent_x,
            "user text should render right of agent text: {rendered}"
        );
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
    fn waiting_placeholder_cycles_dots_and_yields_to_streamed_text() {
        let session = Session::new("s1", "chat", "/tmp");
        let mut view = SessionView::new(session, Vec::new());
        view.stream = Some(StreamState {
            turn_id: "t1".into(),
            text: String::new(),
            reasoning: String::new(),
        });

        for (tick, dots) in [(0, "."), (1, "."), (2, ".."), (4, "..."), (6, ".")] {
            let rendered = render_transcript_at(&view, 40, tick);
            assert!(
                rendered.lines().any(|line| line.trim() == dots),
                "tick {tick}: {rendered}"
            );
        }

        view.stream.as_mut().unwrap().text = "answer".into();
        let rendered = render_transcript_at(&view, 40, 4);
        assert!(rendered.contains("answer"), "{rendered}");
        assert!(
            !rendered
                .lines()
                .any(|line| matches!(line.trim(), "." | ".." | "...")),
            "{rendered}"
        );

        let stream = view.stream.as_mut().unwrap();
        stream.text.clear();
        stream.reasoning = "thinking".into();
        let rendered = render_transcript_at(&view, 40, 4);
        assert!(rendered.contains("Thinking…"), "{rendered}");
        assert!(
            !rendered
                .lines()
                .any(|line| matches!(line.trim(), "." | ".." | "...")),
            "{rendered}"
        );
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
    fn retry_notification_clears_the_partial_stream() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "chat", "/tmp"),
            Vec::new(),
        ));
        app.views[0].stream = Some(StreamState {
            turn_id: "t1".into(),
            text: "partial".into(),
            reasoning: "part".into(),
        });

        app.handle_event(
            Event::Notification(eix_core::Notification::new(
                "turn.retrying",
                serde_json::json!({
                    "session_id": "s1",
                    "turn_id": "t1",
                    "attempt": 2,
                    "max_attempts": 6,
                    "delay_ms": 500,
                    "kind": "rate_limited",
                    "reason": "provider returned HTTP 429: rate limited",
                }),
            )),
            &tx,
        );

        assert!(app.views[0].stream.is_none());
        assert!(
            app.status.contains("rate limited · retrying 2/6"),
            "{}",
            app.status
        );
        assert!(app.status.contains("0.5s"), "{}", app.status);
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
        assert_eq!(
            app.copy_toast.as_ref().map(|toast| toast.text.as_str()),
            Some("Copied 5 chars")
        );
        assert!(app.views[0].selection.is_some());

        let rendered = render_text(&mut app, 40, 12);
        let top_right = rendered.lines().nth(1).unwrap_or_default();
        let column = top_right.find("Copied 5 chars").unwrap_or_default();
        assert!(column > 20, "{rendered}");

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
        // Attention is marked in place; the pane order does not change.
        assert_eq!(app.views[0].session.id, "s1");
        assert_eq!(app.views[1].session.id, "s2");
        assert_eq!(app.views[1].session.status, SessionStatus::AwaitingApproval);
        assert_eq!(app.focused, 0);
        assert!(!app.pane_needs_attention(0));
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
        // The ready session is in pane 2, so it is background work and rings
        // exactly once.
        app.handle_event(attention("s2"), &tx);
        assert!(app.take_ring());
        assert!(!app.take_ring());

        // The focused working pane does not ring.
        app.handle_event(attention("s1"), &tx);
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
                "scopes": ["once", "session", "user"],
            }),
        );
        let pending = SessionView::new(session.clone(), vec![event])
            .pending_approval()
            .expect("pending command approval");
        assert_eq!(
            pending.summary,
            format!("$ {command}\nscopes: once, session, user")
        );

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

    #[test]
    fn write_file_card_uses_the_whole_diff_budget() {
        let mut diff = String::from("--- a/out.txt\n+++ b/out.txt\n@@ -1,15 +1,15 @@\n");
        for index in 0..15 {
            diff.push_str(&format!("-old_{index}\n+new_{index}\n"));
        }
        let (mut app, _tx, _rx) = app_with_diff(Some(&diff));
        let rendered = render_text(&mut app, 80, 40);
        // The card no longer stops at eight rows for file changes.
        assert!(rendered.contains("old_14"), "{rendered}");
        assert!(rendered.contains("new_14"), "{rendered}");
        assert!(rendered.contains('│'), "{rendered}");
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
    fn process_dialog_uses_the_shared_overlay_size() {
        let mut app = app_with_panes(&["s1"]);
        app.process_view = Some(ProcessView::new("s1".into()));
        let text = render_text(&mut app, 80, 24);
        let area = Rect::new(0, 0, 80, 24);
        let rect = search_dialog_rect(area);
        let top = text
            .lines()
            .nth(rect.y as usize)
            .expect("process dialog top border");
        assert_eq!(
            top.chars().position(|ch| ch == '┌'),
            Some(rect.x as usize),
            "{text}"
        );
        assert!(top.contains("processes · s1"), "{text}");
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
    fn slash_and_question_search_the_current_transcript_vim_style() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.views[0].text_lines = ["zero", "one", "alpha here", "three", "alpha again"]
            .into_iter()
            .map(str::to_string)
            .collect();
        app.focus_input();
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE), &tx);
        type_text(&mut app, &tx, "alpha");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        let search = app.transcript_search.as_ref().expect("transcript search");
        assert_eq!(search.query, "alpha");
        assert_eq!(search.matches, vec![2, 4]);
        assert_eq!(search.current, 0);
        assert_eq!(app.views[0].scroll, 0);
        assert!(app.status.contains("/alpha · 1/2"));

        // n / N cycle matches without leaving normal mode.
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &tx);
        assert_eq!(app.transcript_search.as_ref().unwrap().current, 1);
        assert_eq!(app.views[0].scroll, 2);
        app.handle_key(KeyEvent::new(KeyCode::Char('N'), KeyModifiers::NONE), &tx);
        assert_eq!(app.transcript_search.as_ref().unwrap().current, 0);

        // `?` searches backward from the current viewport.
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT), &tx);
        type_text(&mut app, &tx, "alpha");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        let search = app.transcript_search.as_ref().unwrap();
        assert_eq!(search.current, 1);
        assert!(app.status.contains("?alpha · 2/2"));

        // Esc leaves search mode and removes the highlight state.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(app.transcript_search.is_none());
        assert_eq!(app.status, "search cleared");
    }

    #[test]
    fn command_line_shows_a_dynamic_filtered_command_menu() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus_input();
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        assert!(!app.cmdline_menu_open(), "the menu must wait for Tab");
        assert!(!app.cmdline_matches().is_empty());

        type_text(&mut app, &tx, "mod");
        assert!(!app.cmdline_menu_open(), "typing alone must not open it");
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
        assert!(app.cmdline_menu_open(), "Tab enables the command menu");
        let names: Vec<&str> = app
            .cmdline_matches()
            .into_iter()
            .map(|spec| spec.name.as_str())
            .collect();
        assert!(names.contains(&"model"), "{names:?}");
        assert!(names.iter().all(|name| name.starts_with("mod")));

        // Arguments hide the command menu.
        type_text(&mut app, &tx, " provider/model");
        assert!(!app.cmdline_menu_open());

        // `/` and `?` command lines are transcript searches, not command menus.
        app.cmdline = None;
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE), &tx);
        assert!(!app.cmdline_menu_open());
        assert!(app.cmdline_matches().is_empty());
    }

    #[test]
    fn command_line_menu_navigates_with_ctrl_keys() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus_input();
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        type_text(&mut app, &tx, "re");
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
        assert_eq!(
            app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
            Some("ready")
        );
        let names: Vec<&str> = app
            .cmdline_matches()
            .into_iter()
            .map(|spec| spec.name.as_str())
            .collect();
        assert_eq!(names, vec!["ready", "reload", "redo"]);
        assert_eq!(app.cmdline_menu_cursor, None);

        app.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.cmdline_menu_cursor, Some(1));
        assert_eq!(
            app.selected_cmdline_match().map(|spec| spec.name.as_str()),
            Some("reload")
        );
        assert_eq!(
            app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
            Some("ready"),
            "moving the menu cursor must not rewrite the typed prefix"
        );

        app.handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.cmdline_menu_cursor, Some(0));
        app.handle_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.cmdline_menu_cursor, Some(1));
        app.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.cmdline_menu_cursor, Some(0));

        // Terminals may report Ctrl-J as Ctrl-Enter; both move the menu.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL), &tx);
        assert_eq!(app.cmdline_menu_cursor, Some(1));
        // Enter accepts the highlighted row, not the completed first match.
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(app.cmdline.is_none());
    }

    #[test]
    fn normal_search_keys_open_the_command_line() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        for (focus, key, prompt) in [
            (
                Focus::Input,
                KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE),
                '/',
            ),
            (
                Focus::Input,
                KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
                '?',
            ),
            (
                Focus::Transcript,
                KeyEvent::new(KeyCode::Char('#'), KeyModifiers::SHIFT),
                '#',
            ),
        ] {
            let mut app = app_with_panes(&["s1"]);
            app.focus = focus;
            if focus == Focus::Input {
                app.input.enter_normal();
            }
            app.handle_key(key, &tx);
            let cmdline = app.cmdline.as_ref().expect("command line");
            assert_eq!(cmdline.prompt, prompt);
            assert!(cmdline.text.is_empty());
        }
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
            preview: PickerPreview::default(),
            preview_request: None,
        });
        app.handle_picker_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(app.picker.is_none());
        assert_eq!(app.input.text(), "@src/main.rs");
        assert!(matches!(app.focus, Focus::Input));
    }

    #[test]
    fn file_picker_preview_requests_the_selected_file() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.picker = Some(Picker {
            kind: PickerKind::Files,
            query: String::new(),
            selected: 0,
            items: vec![PickerItem::new("src/a.rs"), PickerItem::new("src/b.rs")],
            loading: false,
            preview: PickerPreview::default(),
            preview_request: None,
        });

        let filtered = app.picker_filtered();
        app.sync_picker_preview(&filtered, 0);
        let (kind, session_id, value) = app.take_picker_preview_request().unwrap();
        assert_eq!(kind, eix_core::KIND_FILES);
        assert_eq!(session_id.as_deref(), Some("s1"));
        assert_eq!(value, "src/a.rs");

        app.picker.as_mut().unwrap().selected = 1;
        let filtered = app.picker_filtered();
        app.sync_picker_preview(&filtered, 1);
        let (_, _, value) = app.take_picker_preview_request().unwrap();
        assert_eq!(value, "src/b.rs");

        // A response for the previous row must not overwrite the new preview.
        app.handle_event(
            Event::PickerPreview {
                kind: eix_core::KIND_FILES.into(),
                value: "src/a.rs".into(),
                text: Some("old contents".into()),
                error: None,
            },
            &tx,
        );
        assert!(app.picker.as_ref().unwrap().preview.text.is_none());
        app.handle_event(
            Event::PickerPreview {
                kind: eix_core::KIND_FILES.into(),
                value: "src/b.rs".into(),
                text: Some("fn b() {}".into()),
                error: None,
            },
            &tx,
        );
        assert_eq!(
            app.picker_preview_lines(&filtered),
            vec!["fn b() {}".to_string()]
        );
    }

    #[test]
    fn file_picker_draws_the_lazy_preview_on_the_right() {
        let mut app = app_with_panes(&["s1"]);
        app.picker = Some(Picker {
            kind: PickerKind::Files,
            query: String::new(),
            selected: 0,
            items: vec![PickerItem::new("src/a.rs")],
            loading: false,
            preview: PickerPreview {
                key: Some("src/a.rs".into()),
                text: Some("   1 │ fn a() {}".into()),
                error: None,
                loading: false,
            },
            preview_request: None,
        });

        let rendered = render_text(&mut app, 100, 24);
        assert!(rendered.contains("fn a() {}"), "{rendered}");
    }

    #[test]
    fn session_picker_preview_requests_the_selected_session_history() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.sessions = vec![
            Session::new("s1", "one", "/tmp"),
            Session::new("s2", "two", "/tmp"),
        ];
        app.picker = Some(Picker {
            kind: PickerKind::NewPane,
            query: String::new(),
            selected: 0,
            items: Vec::new(),
            loading: false,
            preview: PickerPreview::default(),
            preview_request: None,
        });

        let filtered = app.picker_filtered();
        app.sync_picker_preview(&filtered, 0);
        let (kind, session_id, value) = app.take_picker_preview_request().unwrap();
        assert_eq!(kind, eix_core::KIND_SESSION_PREVIEW);
        assert!(session_id.is_none());
        assert_eq!(value, "s1");

        app.handle_event(
            Event::PickerPreview {
                kind: eix_core::KIND_SESSION_PREVIEW.into(),
                value: "s1".into(),
                text: Some("you:\n  hello".into()),
                error: None,
            },
            &tx,
        );
        assert_eq!(
            app.picker_preview_lines(&filtered),
            vec!["you:".to_string(), "  hello".to_string()]
        );
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
    fn colorscheme_command_opens_picker_and_escape_restores() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.theme_config.preset = Some("github-dark".into());
        app.theme = Theme::from_config(&app.theme_config);
        let saved = app.theme;

        assert!(app.handle_prompt_command("/colorscheme", &tx));
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

        // Colorscheme arguments no longer switch directly; the picker is the only path.
        assert!(app.handle_prompt_command("/colorscheme nord", &tx));
        assert!(app.picker.is_none());
        assert!(app
            .error
            .as_deref()
            .is_some_and(|error| error.contains("usage: /colorscheme")));
    }

    #[test]
    fn theme_alias_is_not_available() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        assert!(app.handle_prompt_command("/theme", &tx));
        assert!(app.picker.is_none());
        assert!(app
            .error
            .as_deref()
            .is_some_and(|error| error.contains("unknown command /theme")));
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
        assert!(app.status.contains("colorscheme → github-dark"));
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
    fn help_accepts_ctrl_j_k_and_ctrl_n_p() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.open_help();

        for (key, expected) in [('j', 1), ('k', 0), ('n', 1), ('p', 0)] {
            app.handle_key(
                KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL),
                &tx,
            );
            assert_eq!(app.help_selected, expected, "Ctrl-{key}");
        }
    }

    #[test]
    fn help_search_renders_the_selected_match() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.open_help();
        for ch in "approvals inbox".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE), &tx);
        }

        let text = render_text(&mut app, 120, 40);
        assert!(text.contains("/approvals"), "{text}");
        assert!(text.contains("pending-approval inbox"), "{text}");
        assert!(text.contains("› /approvals"), "{text}");
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
        // Short drafts keep the minimum 3 editor rows + 2 borders.
        assert_eq!(prompt_input_height(40, 80, &input), 5);
        assert_eq!(prompt_input_height(100, 80, &input), 5);

        // A long command grows the box so wrapped rows stay visible, but the
        // cap is 12 rows total and short terminals still win.
        input.set_text(&"x".repeat(1_000));
        assert_eq!(prompt_input_height(40, 80, &input), 12);
        assert_eq!(prompt_input_height(12, 80, &input), 7);
        assert_eq!(prompt_input_height(8, 80, &input), 4);
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
    fn double_escape_after_insert_cancels_a_busy_session() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        let mut session = Session::new("s1", "one", "/tmp");
        session.status = SessionStatus::Running;
        app.views.push(SessionView::new(session, Vec::new()));
        app.focused = 0;
        app.focus = Focus::Input;
        app.input.enter_insert();

        // One Esc leaves insert mode; the next two within the window cancel.
        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert_eq!(app.input.mode(), Mode::Normal);
        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        match rx.try_recv().expect("cancel command") {
            Command::Cancel(session_id) => assert_eq!(session_id, "s1"),
            _ => panic!("expected a cancel command"),
        }
    }

    #[test]
    fn double_escape_in_transcript_and_after_cmdline_cancels() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        let mut session = Session::new("s1", "one", "/tmp");
        session.status = SessionStatus::Running;
        app.views.push(SessionView::new(session, Vec::new()));
        app.focused = 0;
        app.focus = Focus::Transcript;

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        assert!(app.cmdline.is_some());
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(app.cmdline.is_none());
        assert!(matches!(app.focus, Focus::Transcript));

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        match rx.try_recv().expect("cancel command") {
            Command::Cancel(session_id) => assert_eq!(session_id, "s1"),
            _ => panic!("expected a cancel command"),
        }
    }

    #[test]
    fn ctrl_p_in_prompt_normal_mode_opens_the_palette() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.focus_input();

        // Insert mode still types a literal colon.
        app.handle_input_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        assert!(app.picker.is_none());
        assert!(app.cmdline.is_none());
        assert_eq!(app.input.text(), ":");

        // In normal mode Ctrl-P opens the interactive command palette instead
        // of editing the draft, matching the transcript binding.
        app.input.clear();
        app.input.enter_normal();
        app.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Commands)
        ));

        // A bare colon in normal mode opens the Vim command line, not the
        // palette, and leaves the draft untouched.
        app.picker = None;
        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        assert!(app.picker.is_none());
        assert!(app.cmdline.is_some());
        assert!(app.input.is_empty());
    }

    #[test]
    fn colon_command_line_preserves_the_prompt_draft() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus_input();
        app.input.set_text("draft");
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        assert!(app.cmdline.is_some());
        type_text(&mut app, &tx, "help");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert!(app.help);
        assert!(app.cmdline.is_none());
        assert_eq!(app.input.text(), "draft");
    }

    #[test]
    fn command_line_colorscheme_opens_the_picker() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus_input();
        app.input.set_text("draft");
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        type_text(&mut app, &tx, "colorscheme");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert!(app.cmdline.is_none());
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Themes)
        ));
        assert!(app.theme_picker_backup.is_some());
        assert_eq!(app.input.text(), "draft");
    }

    #[test]
    fn command_line_bang_runs_a_shell_command() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus_input();
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        type_text(&mut app, &tx, "!printf hi");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert!(app.cmdline.is_none());
        match rx.try_recv().expect("shell command") {
            Command::RunShell {
                session_id,
                command,
                insert_output,
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(command, "printf hi");
                assert!(!insert_output);
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn command_line_read_shell_inserts_output_into_the_prompt() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus_input();
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        type_text(&mut app, &tx, "r!printf hi");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert!(app.cmdline.is_none());
        app.handle_event(
            Event::ShellRan {
                ok: true,
                output: "hi\n".into(),
                insert_output: true,
            },
            &tx,
        );
        assert_eq!(app.input.text(), "hi");
        assert!(matches!(app.focus, Focus::Input));
        assert!(app.status.contains("read 1 line"));
    }

    #[test]
    fn command_line_esc_cancels_without_running_the_command() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus_input();
        app.input.set_text("draft");
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        type_text(&mut app, &tx, "quit");
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);

        assert!(!app.should_quit);
        assert!(app.cmdline.is_none());
        assert_eq!(app.input.text(), "draft");
    }

    #[test]
    fn command_line_edits_at_the_cursor() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus_input();
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        type_text(&mut app, &tx, "hep");
        app.handle_key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &tx);
        type_text(&mut app, &tx, "l");
        assert_eq!(
            app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
            Some("help")
        );
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert!(app.help);
    }

    #[test]
    fn command_line_unknown_commands_keep_the_draft() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus_input();
        app.input.set_text("draft");
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        type_text(&mut app, &tx, "nope");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert!(app
            .error
            .as_deref()
            .is_some_and(|error| error.contains("unknown command")));
        assert!(app.cmdline.is_none());
        assert_eq!(app.input.text(), "draft");
    }

    #[test]
    fn command_line_history_browses_previous_entries() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus_input();
        app.input.set_text("draft");
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        type_text(&mut app, &tx, "w");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert_eq!(app.status, "draft saved");
        assert_eq!(app.input.text(), "draft");

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        assert_eq!(
            app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
            Some("")
        );
        app.handle_key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &tx);
        assert_eq!(
            app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
            Some("w")
        );
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &tx);
        assert_eq!(
            app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
            Some("")
        );

        // With the command menu closed, Ctrl-P/N keep browsing history.
        app.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(
            app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
            Some("w")
        );
        app.handle_key(
            KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(
            app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
            Some("")
        );
    }

    #[test]
    fn command_line_tab_completes_command_names() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus_input();
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        type_text(&mut app, &tx, "hel");
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
        assert_eq!(
            app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
            Some("help")
        );
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(app.help);
    }

    #[test]
    fn command_line_esc_dismisses_menu_before_closing() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus_input();
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        type_text(&mut app, &tx, "mod");
        assert!(!app.cmdline_menu_open());
        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
        assert!(app.cmdline_menu_open());
        let text = app
            .cmdline
            .as_ref()
            .map(|cmdline| cmdline.text.clone())
            .expect("command line");

        // The first Esc closes only the popup and keeps the typed command.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(!app.cmdline_menu_open());
        assert!(!app.cmdline_menu_armed);
        assert_eq!(
            app.cmdline.as_ref().map(|cmdline| cmdline.text.as_str()),
            Some(text.as_str())
        );

        // A second Esc closes the command line itself.
        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(app.cmdline.is_none());
        assert_eq!(app.input.mode(), Mode::Normal);
    }

    #[test]
    fn command_line_q_closes_the_pane_and_saves_the_draft() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1", "s2"]);
        app.focus_input();
        app.input.set_text("keep me");
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        type_text(&mut app, &tx, "q");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert_eq!(view_order(&app), vec!["s2"]);
        assert_eq!(app.input.mode(), Mode::Normal);
        let mut saved = None;
        while let Ok(command) = rx.try_recv() {
            if let Command::DraftSet { session_id, text } = command {
                if session_id == "s1" {
                    saved = Some(text);
                }
            }
        }
        assert_eq!(saved.as_deref(), Some("keep me"));
    }

    #[test]
    fn command_line_w_saves_and_returns_to_normal_mode() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus_input();
        app.input.set_text("keep me");
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        type_text(&mut app, &tx, "w");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert_eq!(app.input.mode(), Mode::Normal);
        assert_eq!(app.input.text(), "keep me");
        let mut saved = None;
        while let Ok(command) = rx.try_recv() {
            if let Command::DraftSet { session_id, text } = command {
                if session_id == "s1" {
                    saved = Some(text);
                }
            }
        }
        assert_eq!(saved.as_deref(), Some("keep me"));
    }

    #[test]
    fn command_line_qa_quits_after_saving_the_draft() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus_input();
        app.input.set_text("keep me");
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        type_text(&mut app, &tx, "qa");
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert!(app.should_quit);
        let mut saved = None;
        while let Ok(command) = rx.try_recv() {
            if let Command::DraftSet { session_id, text } = command {
                if session_id == "s1" {
                    saved = Some(text);
                }
            }
        }
        assert_eq!(saved.as_deref(), Some("keep me"));
    }

    #[test]
    fn command_line_is_visible_by_default() {
        let mut app = app_with_panes(&["s1"]);
        app.focus_input();
        app.input.enter_normal();

        let text = render_text(&mut app, 80, 20);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.last().unwrap().trim().is_empty(), "{text}");
        assert!(!lines.last().unwrap().contains(':'), "{text}");
        assert!(
            status_bar_row(&text).trim_start().starts_with("N · Build"),
            "{text}"
        );
    }

    #[test]
    fn command_line_renders_below_the_status_bar() {
        let mut app = app_with_panes(&["s1"]);
        app.focus_input();
        app.input.enter_normal();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        type_text(&mut app, &tx, "model");

        let text = render_text(&mut app, 80, 20);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.last().unwrap().trim_end(), ":model");
        assert!(
            status_bar_row(&text).trim_start().starts_with("C · Build"),
            "{text}"
        );

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        let text = render_text(&mut app, 80, 20);
        let lines: Vec<&str> = text.lines().collect();
        assert!(lines.last().unwrap().trim().is_empty(), "{text}");
        assert!(
            status_bar_row(&text).trim_start().starts_with("N · Build"),
            "{text}"
        );
        assert!(!lines.last().unwrap().contains(":model"), "{text}");
    }

    #[test]
    fn colon_from_the_transcript_opens_the_command_line() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus = Focus::Transcript;
        app.handle_key(KeyEvent::new(KeyCode::Char(':'), KeyModifiers::NONE), &tx);
        assert!(app.cmdline.is_some());
    }

    #[test]
    fn ctrl_c_and_ctrl_d_quit_from_any_mode() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        for key in ['c', 'd'] {
            let mut app = App::new(None);
            app.focus_input();
            assert_eq!(app.input.mode(), Mode::Insert);
            app.handle_key(
                KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL),
                &tx,
            );
            assert!(app.should_quit, "Ctrl-{key} should quit from insert mode");

            let mut app = App::new(None);
            app.focus = Focus::Transcript;
            app.handle_key(
                KeyEvent::new(KeyCode::Char(key), KeyModifiers::CONTROL),
                &tx,
            );
            assert!(
                app.should_quit,
                "Ctrl-{key} should quit from the transcript"
            );
        }
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
            Command::Create { worktree, .. } => assert!(worktree.is_none()),
            _ => panic!("expected a create command"),
        }

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &tx);
        assert!(
            app.picker.is_none(),
            "leader+a no longer opens the command palette"
        );

        app.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Commands)
        ));

        app.picker = None;
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE), &tx);
        assert!(!app.help, "leader+? no longer opens help");
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE), &tx);
        assert!(app.cmdline.is_some(), "bare ? opens the command line");
        assert!(!app.help, "? no longer opens help");
        app.cmdline = None;
        app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::NONE), &tx);
        assert!(app.help, "F1 still opens help");
    }

    #[test]
    fn leader_hint_disappears_on_the_next_key() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus = Focus::Transcript;
        app.status = "previous status".into();

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        assert!(app.pending_leader);
        assert!(app.status.starts_with("leader:"));

        // An unknown leader key still dismisses the hint and restores the
        // status that was visible before `<Space>`.
        app.handle_key(KeyEvent::new(KeyCode::Char('z'), KeyModifiers::NONE), &tx);
        assert!(!app.pending_leader);
        assert_eq!(app.status, "previous status");
        let text = render_text(&mut app, 80, 20);
        assert!(!text.contains("leader:"), "{text}");

        // A working leader action also replaces the hint rather than leaving
        // it on screen.
        app.focus = Focus::Transcript;
        app.status = "previous status".into();
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE), &tx);
        assert_eq!(app.layout, PaneLayout::Horizontal);
        let text = render_text(&mut app, 80, 20);
        assert!(!text.contains("leader:"), "{text}");
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
            assert!(!app.should_quit, "bare {ch} quit the app");
        }
        assert!(!app.creating, "bare n created a session");
        assert!(rx.try_recv().is_err(), "bare keys sent a command");

        // `i` is the vim-style exception: it focuses the prompt in insert mode.
        app.handle_key(KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE), &tx);
        assert!(matches!(app.focus, Focus::Input));
        assert_eq!(app.input.mode(), Mode::Insert);
        app.focus = Focus::Transcript;

        // leader+s no longer switches the focused pane's session.
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE), &tx);
        assert!(app.picker.is_none(), "leader+s still opened sessions");

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

        // Ctrl-P opens the command palette from the transcript.
        app.process_view = None;
        app.handle_key(
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Commands)
        ));

        // Focus still lives behind the leader.
        app.picker = None;
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE), &tx);
        assert_eq!(app.layout, PaneLayout::Horizontal);
        assert!(matches!(app.focus, Focus::Transcript));

        // Shift+Z quits from normal mode; leader+Q no longer does.
        app.focus = Focus::Transcript;
        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('Q'), KeyModifiers::NONE), &tx);
        assert!(!app.should_quit);
        app.handle_key(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SHIFT), &tx);
        assert!(app.should_quit);
    }

    #[test]
    fn shift_z_quits_from_prompt_normal_mode() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;
        app.focus_input();
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char('Z'), KeyModifiers::SHIFT), &tx);
        assert!(app.should_quit);
    }

    #[test]
    fn leader_n_creates_a_session_and_q_does_not_close_a_pane() {
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
            Command::Create { worktree, .. } => assert!(worktree.is_none()),
            _ => panic!("expected a create command"),
        }

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE), &tx);
        assert_eq!(app.views.len(), 2, "leader+q no longer closes a pane");
    }

    #[test]
    fn leader_n_keeps_the_prompt_in_normal_mode_after_attaching() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;
        app.focus_input();
        app.input.enter_normal();
        app.input_session = Some("s1".to_string());

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE), &tx);

        let created = Session::new("s2", "two", "/tmp");
        app.handle_event(Event::Created(created.clone()), &tx);
        app.handle_event(
            Event::Attached(AttachResult {
                session: created,
                events: Vec::new(),
                has_more: false,
            }),
            &tx,
        );
        app.tick(&tx);

        assert_eq!(app.focused, 1);
        assert!(matches!(app.focus, Focus::Input));
        assert_eq!(app.input.mode(), Mode::Normal);
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
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE), &tx);

        assert_eq!(app.views.len(), 1);
    }

    #[test]
    fn prompt_normal_mode_matches_transcript_for_tui_actions() {
        fn pane_app() -> App {
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
            app.keybinds = Keybinds::from_config(&std::collections::BTreeMap::from([(
                "help".to_string(),
                "F2".to_string(),
            )]));
            app
        }

        #[derive(Debug, PartialEq, Eq)]
        struct Snapshot {
            help: bool,
            processes: bool,
            picker: Option<u8>,
            panes: usize,
            focused: usize,
            scroll: Option<(usize, bool)>,
            leader: bool,
        }

        fn snapshot(app: &App) -> Snapshot {
            Snapshot {
                help: app.help,
                processes: app.process_view.is_some(),
                picker: app.picker.as_ref().map(|picker| picker.kind as u8),
                panes: app.views.len(),
                focused: app.focused,
                scroll: app.focused_view().map(|view| (view.scroll, view.follow)),
                leader: app.pending_leader,
            }
        }

        let scenarios = vec![
            vec![KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT)],
            vec![KeyEvent::new(KeyCode::Char('#'), KeyModifiers::SHIFT)],
            vec![KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE)],
            vec![KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)],
            vec![KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL)],
            vec![
                KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
            ],
            vec![KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE)],
            vec![
                KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('c'), KeyModifiers::NONE),
            ],
            vec![
                KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            ],
            vec![
                KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('p'), KeyModifiers::NONE),
            ],
            vec![
                KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Char('?'), KeyModifiers::SHIFT),
            ],
            vec![
                KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE),
                KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
            ],
        ];

        for keys in scenarios {
            let (pane_tx, _pane_rx) = tokio::sync::mpsc::unbounded_channel();
            let (prompt_tx, _prompt_rx) = tokio::sync::mpsc::unbounded_channel();
            let mut pane = pane_app();
            pane.focus = Focus::Transcript;
            let mut prompt = pane_app();
            prompt.focus_input();
            prompt.input.enter_normal();

            for key in keys.clone() {
                pane.handle_key(key, &pane_tx);
                prompt.handle_key(key, &prompt_tx);
            }

            assert_eq!(
                snapshot(&pane),
                snapshot(&prompt),
                "transcript and prompt normal mode disagreed on {keys:?}"
            );
        }
    }

    #[test]
    fn prompt_normal_mode_ctrl_b_f_page_the_transcript() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let events: Vec<_> = (1..=30)
            .map(|seq| {
                SessionEvent::new(
                    "s1",
                    seq,
                    EventKind::UserMessage,
                    serde_json::json!({ "text": format!("turn {seq}") }),
                )
            })
            .collect();
        let mut app = App::new(None);
        app.views
            .push(SessionView::new(Session::new("s1", "one", "/tmp"), events));
        app.focused = 0;
        app.connected = true;
        app.focus_input();
        app.input.enter_normal();
        let _ = render_text(&mut app, 80, 12);
        let max = app.views[0]
            .text_lines
            .len()
            .saturating_sub(app.pane_areas[0].height.saturating_sub(1) as usize);
        app.views[0].follow = true;

        // Already at the tail: paging down keeps following.
        app.handle_key(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.views[0].scroll, max);
        assert!(app.views[0].follow);

        // Paging up pauses follow, and paging back down resumes it.
        app.handle_key(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.views[0].scroll, max.saturating_sub(10));
        assert!(!app.views[0].follow);
        app.handle_key(
            KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.views[0].scroll, max);
        assert!(app.views[0].follow);
    }

    #[test]
    fn scrolling_near_the_tail_resumes_following() {
        let events: Vec<_> = (1..=30)
            .map(|seq| {
                SessionEvent::new(
                    "s1",
                    seq,
                    EventKind::UserMessage,
                    serde_json::json!({ "text": format!("turn {seq}") }),
                )
            })
            .collect();
        let mut app = App::new(None);
        app.views
            .push(SessionView::new(Session::new("s1", "one", "/tmp"), events));
        app.focused = 0;
        app.connected = true;
        let _ = render_text(&mut app, 80, 12);
        let max = app.views[0]
            .text_lines
            .len()
            .saturating_sub(app.pane_areas[0].height.saturating_sub(1) as usize);

        // A wheel-up from the tail pauses exactly one line above it.
        app.views[0].follow = true;
        app.views[0].scroll = 0;
        app.scroll_focused(-1);
        assert_eq!(app.views[0].scroll, max.saturating_sub(1));
        assert!(!app.views[0].follow);

        // One line down is still paused; two lines from the tail re-attaches.
        app.views[0].scroll = max.saturating_sub(4);
        app.views[0].follow = false;
        app.scroll_focused(1);
        assert_eq!(app.views[0].scroll, max.saturating_sub(3));
        assert!(!app.views[0].follow);
        app.scroll_focused(1);
        assert_eq!(app.views[0].scroll, max);
        assert!(app.views[0].follow);
    }

    #[test]
    fn scrolling_up_an_empty_transcript_keeps_following() {
        let mut app = app_with_panes(&["s1"]);
        app.focused = 0;
        app.views[0].follow = true;
        app.scroll_focused(-3);
        assert_eq!(app.views[0].scroll, 0);
        assert!(app.views[0].follow);
    }

    #[test]
    fn ctrl_b_f_page_dialog_overlays() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let control = |ch| KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL);

        // Help filter results.
        let mut app = App::new(None);
        app.open_help();
        let help_last = help_entry_count("").saturating_sub(1);
        app.handle_key(control('f'), &tx);
        assert_eq!(app.help_selected, 8.min(help_last));
        app.handle_key(control('b'), &tx);
        assert_eq!(app.help_selected, 0);
        app.help = false;

        // Picker rows.
        app.sessions = (0..15)
            .map(|i| Session::new(format!("s{i}"), format!("s{i}"), "/tmp"))
            .collect();
        app.open_picker(PickerKind::Sessions, &tx);
        assert_eq!(app.picker.as_ref().unwrap().selected, 0);
        app.handle_key(control('f'), &tx);
        assert_eq!(app.picker.as_ref().unwrap().selected, 10);
        app.handle_key(control('b'), &tx);
        assert_eq!(app.picker.as_ref().unwrap().selected, 0);
        app.picker = None;

        // Cross-session approval inbox.
        app.inbox = Some(InboxView {
            items: (0..15)
                .map(|i| InboxItem {
                    session_id: format!("s{i}"),
                    session_name: format!("s{i}"),
                    tool: "run_command".into(),
                    summary: format!("item {i}"),
                    kind: PendingKind::Tool,
                })
                .collect(),
            cursor: 0,
        });
        app.handle_key(control('f'), &tx);
        assert_eq!(app.inbox.as_ref().unwrap().cursor, 10);
        app.handle_key(control('b'), &tx);
        assert_eq!(app.inbox.as_ref().unwrap().cursor, 0);
        app.inbox = None;

        // Background process output.
        let mut processes = ProcessView::new("s1".into());
        processes.processes = (0..15)
            .map(|i| ProcessInfo {
                id: format!("p{i}"),
                session_id: "s1".into(),
                command: format!("cmd {i}"),
                cwd: "/tmp".into(),
                pid: i as u32,
                status: ProcessStatus::Running,
                exit_code: None,
                started_ms: 0,
                ended_ms: None,
                bytes: 0,
                truncated: false,
                log: String::new(),
            })
            .collect();
        processes.scroll = 100;
        app.process_view = Some(processes);
        app.handle_key(control('f'), &tx);
        assert_eq!(app.process_view.as_ref().unwrap().scroll, 80);
        app.handle_key(control('b'), &tx);
        assert_eq!(app.process_view.as_ref().unwrap().scroll, 100);
        app.process_view = None;

        // `/btw` side-question overlay.
        app.btw = Some(BtwView {
            session_id: "s1".into(),
            btw_id: "b1".into(),
            question: "why?".into(),
            text: "answer".into(),
            error: None,
            done: true,
            scroll: 0,
        });
        app.handle_key(control('f'), &tx);
        assert_eq!(app.btw.as_ref().unwrap().scroll, 8);
        app.handle_key(control('b'), &tx);
        assert_eq!(app.btw.as_ref().unwrap().scroll, 0);

        // Diff review.
        let mut diff_app = app_with_hunks(
            (0..20)
                .map(|i| hunk(i, "@@ -1 +1 @@", &["@@ -1 +1 @@", "-a", "+A"]))
                .collect(),
        );
        diff_app.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE), &tx);
        assert!(diff_app.diff_view.is_some());
        diff_app.handle_key(control('f'), &tx);
        assert_eq!(diff_app.diff_view.as_ref().unwrap().scroll, 10);
        diff_app.handle_key(control('b'), &tx);
        assert_eq!(diff_app.diff_view.as_ref().unwrap().scroll, 0);
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
    fn double_ctrl_w_cycles_panes_and_wraps() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let ctrl_w = || KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL);

        for focus in [Focus::Transcript, Focus::Input] {
            let mut app = app_with_panes(&["s1", "s2", "s3"]);
            app.focused = 0;
            if focus == Focus::Input {
                app.focus_input();
                app.input.enter_normal();
            } else {
                app.focus = Focus::Transcript;
            }

            // A single Ctrl-W only arms the gesture.
            app.handle_key(ctrl_w(), &tx);
            assert_eq!(app.focused, 0);
            for expected in [1, 2, 0] {
                app.handle_key(ctrl_w(), &tx);
                assert_eq!(app.focused, expected);
                if expected != 0 {
                    app.handle_key(ctrl_w(), &tx);
                }
            }
            assert!(app.focus == focus, "pane focus changed");
        }

        // A stale first press does not pair with a much later one.
        let mut app = app_with_panes(&["s1", "s2"]);
        app.focused = 0;
        app.focus = Focus::Transcript;
        app.ctrl_w_at = Some(Instant::now() - CTRL_W_WINDOW);
        app.handle_key(ctrl_w(), &tx);
        assert_eq!(app.focused, 0);
        app.handle_key(ctrl_w(), &tx);
        assert_eq!(app.focused, 1);

        // Another key between the two presses also breaks the gesture.
        app.focused = 0;
        app.ctrl_w_at = None;
        app.handle_key(ctrl_w(), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE), &tx);
        app.handle_key(ctrl_w(), &tx);
        assert_eq!(app.focused, 0);
    }

    #[test]
    fn normal_mode_enter_keeps_the_session_order() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        for focus in [Focus::Transcript, Focus::Input] {
            let mut app = app_with_panes(&["a", "b", "c"]);
            app.focused = 2;
            if focus == Focus::Input {
                app.focus_input();
                app.input.enter_normal();
            } else {
                app.focus = Focus::Transcript;
            }

            app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
            assert_eq!(view_order(&app), vec!["a", "b", "c"]);
            assert_eq!(app.focused, 2);
            assert_eq!(app.focused_session_id().as_deref(), Some("c"));
            assert!(app.focus == focus, "Enter changed the focus");
            if focus == Focus::Input {
                assert_eq!(app.input.mode(), Mode::Normal);
            }
        }
    }

    #[test]
    fn ctrl_w_still_deletes_a_word_in_insert_mode() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1", "s2"]);
        app.focused = 0;
        app.focus_input();
        type_text(&mut app, &tx, "one two");

        app.handle_key(
            KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.focused, 0);
        assert_eq!(app.input.text(), "one ");
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
    fn pane_title_omits_read_only_badge() {
        let mut app = app_with_panes(&["plan-pane"]);
        app.focused = 0;
        app.views[0].session.mode = SessionMode::Plan;
        let rendered = render_text(&mut app, 80, 20);
        let title = rendered.lines().next().expect("pane title");
        assert!(title.contains("plan-pane"), "{title}");
        assert!(!title.contains(" ro"), "{title}");
    }

    #[test]
    fn status_bar_keeps_every_session_mode_aligned() {
        fn left(app: &App) -> String {
            app.status_bar_left()
                .spans
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
        app.views[0].session.model = Some("llmgate/deepseek/deepseek-v4.1-flash#high".into());
        app.models.push(ModelInfo {
            id: "llmgate/deepseek/deepseek-v4.1-flash".into(),
            api_model: Some("deepseek/deepseek-v4.1-flash".into()),
            name: "deepseek/deepseek-v4.1-flash".into(),
            provider: "llmgate".into(),
            default: false,
            variants: vec!["high".into()],
            input_cost: None,
            output_cost: None,
            context_tokens: None,
        });

        app.views[0].session.mode = SessionMode::Build;
        let reference = left(&app);
        assert_eq!(
            reference,
            " I · Build  · ⠶ · deepseek-v4.1-flash · llmgate · high"
        );
        let first_dot = reference.find('·').expect("separator");

        // Every label is padded to Bypass's width, so neither the total line
        // length nor the first separator moves when the mode changes.
        for mode in [SessionMode::Plan, SessionMode::Auto, SessionMode::Bypass] {
            app.views[0].session.mode = mode;
            let line = left(&app);
            assert_eq!(line.len(), reference.len(), "{mode:?}: {line}");
            assert_eq!(line.find('·'), Some(first_dot), "{mode:?}: {line}");
            assert_eq!(
                app.status_bar_left().spans[2].content.chars().count(),
                SESSION_MODE_WIDTH,
                "{mode:?}"
            );
        }
    }

    #[test]
    fn status_bar_colors_every_session_mode_differently() {
        let mut app = App::new(None);
        app.connected = true;
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;

        let mut colors = Vec::new();
        for mode in [
            SessionMode::Plan,
            SessionMode::Build,
            SessionMode::Auto,
            SessionMode::Bypass,
        ] {
            app.views[0].session.mode = mode;
            let line = app.status_bar_left();
            let badge = &line.spans[2];
            assert_eq!(badge.content.trim(), mode.label());
            colors.push(badge.style.fg.expect("mode badge color"));
        }
        for (index, color) in colors.iter().enumerate() {
            for other in &colors[index + 1..] {
                assert_ne!(color, other, "{colors:?}");
            }
        }
    }

    #[test]
    fn session_mode_colors_stay_distinct_in_every_theme() {
        let mut themes = vec![Theme::default()];
        themes.extend(THEME_PRESETS.iter().map(|preset| {
            Theme::from_config(&eix_core::ThemeConfig {
                preset: Some(preset.name.to_string()),
                ..Default::default()
            })
        }));

        for theme in themes {
            let colors: Vec<Color> = [
                SessionMode::Plan,
                SessionMode::Build,
                SessionMode::Auto,
                SessionMode::Bypass,
            ]
            .into_iter()
            .map(|mode| session_mode_color(mode, theme))
            .collect();
            for (index, color) in colors.iter().enumerate() {
                for other in &colors[index + 1..] {
                    assert_ne!(
                        color, other,
                        "mode colors collide for preset {:?}: {colors:?}",
                        theme.preset
                    );
                }
            }
        }
    }

    #[test]
    fn prompt_header_uses_icon_for_idle_and_animated_running() {
        let mut app = App::new(None);
        assert_eq!(app.status_glyph(SessionStatus::Idle), "⠶");
        assert_eq!(app.status_glyph(SessionStatus::Running), "◰");
        app.tick = 1;
        assert_eq!(app.status_glyph(SessionStatus::Running), "◳");
        app.tick = 2;
        assert_eq!(app.status_glyph(SessionStatus::Running), "◲");
        app.tick = 3;
        assert_eq!(app.status_glyph(SessionStatus::Running), "◱");
        app.tick = 4;
        assert_eq!(app.status_glyph(SessionStatus::Running), "◰");
        assert_eq!(
            app.status_glyph(SessionStatus::AwaitingApproval),
            "⚠ awaiting_approval"
        );
    }

    #[test]
    fn running_pane_icon_rotates_and_uses_status_color() {
        let mut app = App::new(None);
        let mut session = Session::new("s1", "one", "/tmp");
        session.status = SessionStatus::Running;
        app.views.push(SessionView::new(session, Vec::new()));
        app.focused = 0;
        app.connected = true;
        app.tick = 0;

        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let icons: Vec<_> = buffer
            .content
            .iter()
            .filter(|cell| cell.symbol() == "◰")
            .collect();
        assert!(!icons.is_empty(), "running icon missing");
        assert!(
            icons
                .iter()
                .all(|cell| cell.fg == composer_success(app.theme)),
            "running icon should be green"
        );

        app.tick = 1;
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        assert!(
            buffer.content.iter().any(|cell| cell.symbol() == "◳"),
            "running icon did not advance"
        );

        app.views[0].session.status = SessionStatus::Idle;
        app.tick = 0;
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let icons: Vec<_> = buffer
            .content
            .iter()
            .filter(|cell| cell.symbol() == "⠶")
            .collect();
        assert!(!icons.is_empty(), "idle icon missing");
        assert!(
            icons
                .iter()
                .all(|cell| cell.fg == composer_muted(app.theme)),
            "idle icon should be gray"
        );
    }

    #[test]
    fn status_bar_shows_git_branch_without_worktree_badge() {
        let mut app = App::new(None);
        let cwd = std::path::PathBuf::from("/tmp/eix-repo");
        let mut session = Session::new("s1", "one", cwd.clone());
        session.worktree = Some(cwd.join(".worktrees/feat"));
        app.views.push(SessionView::new(session, Vec::new()));
        app.focused = 0;
        app.git_cwd = Some(cwd);
        app.git_branch = Some("eix/feat".into());
        app.git_checked = Some(Instant::now());

        let workspace = app.status_bar_workspace().expect("workspace");
        let workspace = line_text(&workspace);
        assert!(workspace.contains("eix-repo"), "{workspace}");
        assert!(workspace.contains("eix/feat"), "{workspace}");
        assert!(
            app.status_bar_middle().is_none(),
            "a worktree badge should not occupy the status line"
        );

        // A cached branch for another cwd must not leak into this session.
        app.git_cwd = Some(std::path::PathBuf::from("/tmp/other"));
        let workspace = app.status_bar_workspace().expect("workspace");
        let workspace = line_text(&workspace);
        assert!(!workspace.contains("eix/feat"), "{workspace}");
    }

    #[test]
    fn status_bar_hides_mode_and_merged_noise() {
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;
        app.status = "mode: bypass".into();
        assert!(app.status_bar_middle().is_none());

        app.integrations.insert(
            "s1".into(),
            IntegrationState {
                session_id: "s1".into(),
                status: IntegrationStatus::Merged,
                ..IntegrationState::default()
            },
        );
        app.status = "integration · merged".into();
        assert!(app.status_bar_middle().is_none());
    }

    #[test]
    fn status_bar_hides_patch_and_command_chatter() {
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;
        for status in [
            "patch applied: go/main.go",
            "patch staged: go/main.go",
            "process abcd1234 started: go test",
            "process abcd1234 exited (0)",
            "running shell command…",
            "reading shell output…",
            "approval sent",
            "answer sent",
            "answered: Option A",
        ] {
            app.status = status.into();
            assert!(app.status_bar_middle().is_none(), "{status}");
        }
    }

    #[test]
    fn prompt_normal_mode_tab_cycles_modes() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;
        app.focus_input();
        app.status = "ready".into();
        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
        assert_eq!(app.views[0].session.mode, SessionMode::Auto);
        // The mode lives in the status bar, not in the transient app status.
        assert_eq!(app.status, "ready");
        match rx.try_recv().expect("set auto mode") {
            Command::SetMode {
                session_id,
                mode,
                temporary,
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(mode, SessionMode::Auto);
                assert!(!temporary);
            }
            _ => panic!("unexpected command"),
        }

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
        assert_eq!(app.views[0].session.mode, SessionMode::Bypass);
        assert_eq!(app.status, "ready");
        match rx.try_recv().expect("set bypass mode") {
            Command::SetMode {
                session_id, mode, ..
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(mode, SessionMode::Bypass);
            }
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn prompt_normal_mode_shift_tab_cycles_backward() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "one", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;
        app.focus_input();
        app.status = "ready".into();
        app.handle_input_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);

        // Build → Plan, then Plan → Bypass. The reverse walk stays available
        // from the prompt without a round-trip through the transcript.
        for (key, expected) in [
            (
                KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
                SessionMode::Plan,
            ),
            (
                KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT),
                SessionMode::Bypass,
            ),
        ] {
            app.handle_input_key(key, &tx);
            assert_eq!(app.views[0].session.mode, expected);
            assert_eq!(app.status, "ready");
            match rx.try_recv().expect("set mode") {
                Command::SetMode {
                    session_id, mode, ..
                } => {
                    assert_eq!(session_id, "s1");
                    assert_eq!(mode, expected);
                }
                _ => panic!("unexpected command"),
            }
        }
    }

    #[test]
    fn switching_a_manual_mode_to_build_restores_write_access() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        let mut session = Session::new("s1", "one", "/tmp");
        session.mode = SessionMode::Plan;
        session.agent = Some("reviewer".into());
        app.views.push(SessionView::new(session, Vec::new()));
        app.focused = 0;

        app.toggle_mode(&tx);

        assert_eq!(app.views[0].session.mode, SessionMode::Build);
        // The custom agent stays selected; mode is independent of the agent.
        assert_eq!(app.views[0].session.agent.as_deref(), Some("reviewer"));
        match rx.try_recv().expect("set build mode") {
            Command::SetMode { mode, .. } => assert_eq!(mode, SessionMode::Build),
            _ => panic!("unexpected command"),
        }
    }

    #[test]
    fn tab_and_shift_tab_cycle_modes_but_never_switch_sessions() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1", "s2"]);
        app.focus = Focus::Transcript;

        app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
        assert_eq!(app.views[0].session.mode, SessionMode::Auto);
        match rx.try_recv().expect("set auto mode") {
            Command::SetMode {
                session_id, mode, ..
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(mode, SessionMode::Auto);
            }
            _ => panic!("unexpected command"),
        }

        app.handle_key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT), &tx);
        assert_eq!(app.focused, 0, "Shift-Tab never switches sessions");
        assert_eq!(app.views[0].session.mode, SessionMode::Build);
        match rx.try_recv().expect("set build mode") {
            Command::SetMode {
                session_id, mode, ..
            } => {
                assert_eq!(session_id, "s1");
                assert_eq!(mode, SessionMode::Build);
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
    fn question_card_uses_markdown_checkboxes() {
        let (mut app, _, _rx) = input_app();
        let text = render_text(&mut app, 80, 30);
        assert!(text.contains("- [x] SQLite"), "{text}");
        assert!(text.contains("- [ ] Postgres"), "{text}");
        assert!(text.contains("- [ ] MySQL"), "{text}");

        app.move_input_cursor(1);
        let text = render_text(&mut app, 80, 30);
        assert!(text.contains("- [ ] SQLite"), "{text}");
        assert!(text.contains("- [x] Postgres"), "{text}");
    }

    #[test]
    fn card_ctrl_navigation_works_with_prompt_focus_in_tabby() {
        let (mut app, tx, mut rx) = input_app();
        app.layout = PaneLayout::Tabby;
        app.focus = Focus::Input;
        app.input.enter_insert();

        app.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.focused_input().expect("pending").cursor, 1);

        app.handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.focused_input().expect("pending").cursor, 0);

        app.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            &tx,
        );
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        match rx.try_recv().expect("input command") {
            Command::Input { option_index, .. } => assert_eq!(option_index, 1),
            _ => panic!("expected an input command"),
        }
    }

    #[test]
    fn approval_card_uses_markdown_checkboxes() {
        let (mut app, _, _rx) = input_app();
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

        let text = render_text(&mut app, 80, 30);
        assert!(text.contains("- [x] Approve"), "{text}");
        assert!(text.contains("- [ ] Always allow"), "{text}");

        app.move_approval_cursor(1);
        let text = render_text(&mut app, 80, 30);
        assert!(text.contains("- [ ] Approve"), "{text}");
        assert!(text.contains("- [x] Always allow"), "{text}");
    }

    #[test]
    fn approval_ctrl_navigation_works_with_prompt_focus() {
        let (mut app, tx, mut rx) = input_app();
        app.inputs.clear();
        app.layout = PaneLayout::Tabby;
        app.focus = Focus::Input;
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

        app.handle_key(
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.focused_approval().expect("approval").cursor, 1);

        app.handle_key(
            KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL),
            &tx,
        );
        assert_eq!(app.focused_approval().expect("approval").cursor, 0);
        app.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        match rx.try_recv().expect("approval command") {
            Command::Approval { decision, .. } => {
                assert_eq!(decision, ApprovalDecision::Approve)
            }
            _ => panic!("expected an approval command"),
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
    fn live_approval_notification_keeps_risk_metadata() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "chat", "/tmp"),
            Vec::new(),
        ));
        app.handle_event(
            Event::Notification(eix_core::Notification::new(
                "tool.approval_required",
                serde_json::json!({
                    "session_id": "s1",
                    "turn_id": "t1",
                    "approval_id": "a1",
                    "call_id": "c1",
                    "tool": "run_command",
                    "summary": "run_command: truncated",
                    "arguments": {"command": "curl https://example.com/?token=x"},
                    "risk": "high",
                    "risk_categories": ["network"],
                    "reasons": ["fetches a URL from the network: https://example.com/?…"],
                    "rule": "net",
                    "rule_source": "user",
                    "sandbox": "auto",
                    "scopes": ["once"],
                    "parsed_command": "curl https://example.com/?token=x",
                    "allow_always": false,
                }),
            )),
            &tx,
        );
        let pending = app.approvals.get("s1").expect("approval");
        assert!(
            pending.summary.starts_with("[HIGH] $ curl"),
            "{}",
            pending.summary
        );
        assert!(
            pending.summary.contains("risk: network"),
            "{}",
            pending.summary
        );
        assert!(
            pending.summary.contains("rule: net (user)"),
            "{}",
            pending.summary
        );
        assert!(
            pending.summary.contains("sandbox: auto"),
            "{}",
            pending.summary
        );
        assert!(
            pending.summary.contains("scopes: once"),
            "{}",
            pending.summary
        );
        assert!(
            !pending.summary.contains("parsed:"),
            "approval cards should stay short: {}",
            pending.summary
        );
        assert!(pending.summary.contains(NO_ALWAYS_MARKER));
        assert!(!pending.actions().contains(&ApprovalAction::Always));
    }

    #[test]
    fn high_risk_and_forced_ask_cards_hide_always() {
        let mut approval = PendingApproval {
            kind: PendingKind::Tool,
            approval_id: "a1".to_string(),
            call_id: "c1".to_string(),
            tool: "run_command".to_string(),
            summary: "[HIGH] run_command: rm -rf /".to_string(),
            diff: None,
            path: None,
            hunks: Vec::new(),
            selected: Vec::new(),
            cursor: 0,
        };
        assert!(!approval.actions().contains(&ApprovalAction::Always));

        approval.summary = format!("[MEDIUM] write_file out.txt{NO_ALWAYS_MARKER}");
        assert!(!approval.actions().contains(&ApprovalAction::Always));

        approval.summary = "[MEDIUM] write_file out.txt".to_string();
        assert!(approval.actions().contains(&ApprovalAction::Always));
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
    fn cleanup_deletes_a_created_session_that_was_never_used() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.handle_event(Event::Created(Session::new("s1", "one", "/tmp")), &tx);
        while rx.try_recv().is_ok() {}

        app.cleanup_empty_sessions(&tx);
        match rx.try_recv().expect("delete command") {
            Command::DeleteSessionIfEmpty { session_id } => assert_eq!(session_id, "s1"),
            _ => panic!("expected a conditional delete"),
        }
    }

    #[test]
    fn cleanup_keeps_a_created_session_after_an_event_or_draft() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.handle_event(Event::Created(Session::new("s1", "one", "/tmp")), &tx);
        while rx.try_recv().is_ok() {}

        let event = SessionEvent {
            session_id: "s1".into(),
            seq: 1,
            ts: 0,
            kind: EventKind::UserMessage,
            payload: serde_json::json!({"text": "hi"}),
        };
        app.handle_event(
            Event::Notification(eix_core::Notification::new("session.event", event)),
            &tx,
        );
        app.cleanup_empty_sessions(&tx);
        assert!(rx.try_recv().is_err(), "a used session must be kept");

        // A draft in the prompt is also input.
        app.handle_event(Event::Created(Session::new("s2", "two", "/tmp")), &tx);
        while rx.try_recv().is_ok() {}
        app.input_session = Some("s2".into());
        app.input.set_text("unsent draft");
        app.cleanup_empty_sessions(&tx);
        assert!(rx.try_recv().is_err(), "a draft must keep the session");
    }

    #[test]
    fn slash_exit_quits_without_closing_session_panes() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.views.push(session_pane("s2", "two"));
        app.focused = 0;
        app.focus = Focus::Input;
        app.input.set_text("/exit");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert!(app.should_quit, "exit quits the whole TUI");
        assert_eq!(app.views.len(), 2, "exit leaves session panes open");
        assert_eq!(app.views[0].session.id, "s1");
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
    fn slash_quit_quits_with_multiple_panes_open() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.views.push(session_pane("s2", "two"));
        app.focused = 1;
        app.focus = Focus::Input;
        app.input.set_text("/quit");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert!(app.should_quit, "quit quits the whole TUI");
        assert_eq!(app.views.len(), 2, "quit leaves session panes open");
    }

    #[test]
    fn slash_close_saves_and_closes_the_focused_pane() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.views.push(session_pane("s2", "two"));
        app.focused = 0;
        app.focus = Focus::Input;
        app.created_sessions.insert("s1".into());
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
        match rx.try_recv().expect("conditional delete") {
            Command::DeleteSessionIfEmpty { session_id } => assert_eq!(session_id, "s1"),
            _ => panic!("expected a conditional delete"),
        }
    }

    #[test]
    fn closing_a_created_pane_queues_a_conditional_delete() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.views.push(session_pane("s2", "two"));
        app.focused = 0;
        app.created_sessions.insert("s1".into());

        app.run_action("close-pane", &tx);

        assert_eq!(app.views.len(), 1);
        assert_eq!(app.views[0].session.id, "s2");
        match rx.try_recv().expect("draft save") {
            Command::DraftSet { session_id, .. } => assert_eq!(session_id, "s1"),
            _ => panic!("expected a draft save"),
        }
        match rx.try_recv().expect("conditional delete") {
            Command::DeleteSessionIfEmpty { session_id } => assert_eq!(session_id, "s1"),
            _ => panic!("expected a conditional delete"),
        }
    }

    #[test]
    fn slash_close_on_the_last_pane_quits_the_client() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.focused = 0;
        app.focus = Focus::Input;
        app.input.set_text("/close");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);

        assert!(app.should_quit, "closing the last pane quits the TUI");
        assert_eq!(app.views.len(), 1);
        assert!(app.status.contains("quitting"));
        match rx.try_recv().expect("draft save") {
            Command::DraftSet { session_id, .. } => assert_eq!(session_id, "s1"),
            _ => panic!("expected a draft save"),
        }
    }

    #[test]
    fn slash_commands_mirror_the_action_palette() {
        let commands = builtin_commands();
        let builtins: HashSet<&str> = commands.iter().map(|spec| spec.name.as_str()).collect();
        for item in command_items() {
            let slash = if item.value == "close-pane" {
                "close"
            } else {
                item.value.as_str()
            };
            assert!(
                builtins.contains(slash),
                "palette action {:?} has no /{slash} command",
                item.value
            );
        }
    }

    #[test]
    fn every_palette_action_has_a_working_slash_command() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        for item in command_items() {
            let name = if item.value == "close-pane" {
                "close"
            } else {
                item.value.as_str()
            };
            let mut app = App::new(None);
            assert!(
                app.handle_prompt_command(&format!("/{name}"), &tx),
                "/{name} was not handled"
            );
            assert!(app.error.is_none(), "/{name} failed: {:?}", app.error);
        }
    }

    #[test]
    fn slash_help_and_keys_open_filtered_interactive_references() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);

        assert!(app.handle_prompt_command("/help approval", &tx));
        assert!(app.help);
        assert_eq!(app.help_scope, None);
        assert_eq!(app.help_query, "approval");
        assert!(help_entry_count_in(&app.help_query, app.help_scope) > 0);

        app.help = false;
        assert!(app.handle_prompt_command("/keys space", &tx));
        assert!(app.help);
        assert_eq!(app.help_scope, Some("keys"));
        assert_eq!(app.help_query, "space");
        let rows = help_rows_in(&app.help_query, app.help_scope);
        assert!(!rows.is_empty());
        assert!(rows.iter().all(|row| !matches!(
            row,
            HelpRow::Section(title) if *title != "keys"
        )));

        app.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &tx);
        assert!(!app.help);
        assert_eq!(app.help_scope, None);
    }

    #[test]
    fn mcp_and_todos_commands_open_searchable_pickers() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.focused = 0;

        assert!(app.handle_prompt_command("/mcp", &tx));
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Mcp)
        ));
        app.handle_event(
            Event::Mcp(vec![eix_core::McpServerInfo {
                name: "files".into(),
                enabled: true,
                command: "npx server".into(),
                tools: 2,
                resources: 0,
                prompts: 0,
                error: None,
            }]),
            &tx,
        );
        assert_eq!(app.picker.as_ref().unwrap().items.len(), 1);
        assert!(!app.picker.as_ref().unwrap().loading);

        app.picker = None;
        assert!(app.handle_prompt_command("/todos", &tx));
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Todos)
        ));
        app.handle_event(
            Event::Todos(vec![eix_core::TodoItem {
                content: "write tests".into(),
                status: "in_progress".into(),
            }]),
            &tx,
        );
        assert_eq!(app.picker.as_ref().unwrap().items.len(), 1);
        assert!(!app.picker.as_ref().unwrap().loading);
    }

    #[test]
    fn model_command_opens_the_picker_and_preselects_the_current_model() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        let mut pane = session_pane("s1", "one");
        pane.session.model = Some("router/gpt-5".into());
        app.views.push(pane);
        app.focused = 0;

        assert!(app.handle_prompt_command("/model", &tx));
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Models)
        ));
        assert!(app.picker.as_ref().unwrap().loading);

        app.handle_event(
            Event::Models {
                models: vec![
                    eix_core::ModelInfo {
                        id: "openai/gpt-4o".into(),
                        api_model: Some("gpt-4o".into()),
                        name: "gpt-4o".into(),
                        provider: "openai".into(),
                        default: false,
                        variants: Vec::new(),
                        input_cost: None,
                        output_cost: None,
                        context_tokens: None,
                    },
                    eix_core::ModelInfo {
                        id: "router/gpt-5".into(),
                        api_model: Some("gpt-5".into()),
                        name: "GPT-5".into(),
                        provider: "Router".into(),
                        default: false,
                        variants: Vec::new(),
                        input_cost: None,
                        output_cost: None,
                        context_tokens: None,
                    },
                ],
                warnings: vec!["local: HTTP 401".into()],
            },
            &tx,
        );
        let picker = app.picker.as_ref().expect("model picker");
        assert!(!picker.loading);
        assert_eq!(picker.items[picker.selected].value, "router/gpt-5");
        assert_eq!(picker.items[0].label, "@default");
        assert_eq!(picker.items[0].detail.as_deref(), Some("daemon default"));
        assert_eq!(picker.items[1].label, "gpt-4o");
        assert_eq!(picker.items[1].value, "openai/gpt-4o");
        assert_eq!(picker.items[1].detail.as_deref(), Some("openai"));
        assert_eq!(picker.items[2].label, "gpt-5");
        assert_eq!(picker.items[2].value, "router/gpt-5");
        assert_eq!(picker.items[2].detail.as_deref(), Some("Router"));
        assert!(app.status.contains("local: HTTP 401"));
    }

    #[test]
    fn ctrl_slash_opens_the_model_picker_from_both_focus_modes() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();

        let mut transcript = App::new(None);
        transcript.focus = Focus::Transcript;
        transcript.handle_key(
            KeyEvent::new(KeyCode::Char('/'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(matches!(
            transcript.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Models)
        ));
        assert!(transcript.picker.as_ref().unwrap().loading);

        let mut prompt = App::new(None);
        prompt.focus_input();
        prompt.handle_key(
            KeyEvent::new(KeyCode::Char('_'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(matches!(
            prompt.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Models)
        ));
        assert!(prompt.picker.as_ref().unwrap().loading);

        let mut legacy = App::new(None);
        legacy.focus = Focus::Transcript;
        legacy.handle_key(
            KeyEvent::new(KeyCode::Char('7'), KeyModifiers::CONTROL),
            &tx,
        );
        assert!(matches!(
            legacy.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Models)
        ));
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
    fn leader_x_keeps_the_prompt_in_normal_mode() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(session_pane("s1", "one"));
        app.focused = 0;
        app.views[0].session.status = SessionStatus::Running;
        app.focus = Focus::Input;
        app.input.enter_normal();

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE), &tx);

        assert_eq!(app.input.mode(), Mode::Normal);
        assert!(matches!(rx.try_recv(), Ok(Command::Cancel(id)) if id == "s1"));

        // A delayed draft load must not push the prompt back into insert mode.
        app.input_session = Some("s1".to_string());
        app.handle_event(
            Event::Draft {
                session_id: "s1".to_string(),
                text: "restored draft".to_string(),
            },
            &tx,
        );
        assert_eq!(app.input.mode(), Mode::Normal);
        assert_eq!(app.input.text(), "restored draft");
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

    #[test]
    fn subagents_command_and_picker_adjust_the_runtime_limit() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);

        assert!(app.handle_prompt_command("/subagents 8", &tx));
        assert!(app.error.is_none());
        assert!(app.status.contains('8'));
        match rx.try_recv().expect("set command") {
            Command::SetSubagentConcurrency { value } => assert_eq!(value, 8),
            _ => panic!("expected a subagent concurrency command"),
        }

        app.handle_event(Event::SubagentConcurrency { value: 8 }, &tx);
        assert_eq!(app.subagent_concurrency, 8);
        assert!(app.status.contains("runtime"));

        app.open_picker(PickerKind::Subagents, &tx);
        let selected_value = {
            let picker = app.picker.as_ref().expect("subagent picker");
            assert_eq!(picker.kind, PickerKind::Subagents);
            picker.items[picker.selected].value.clone()
        };
        assert_eq!(selected_value, "8");
        let values = {
            let picker = app.picker.as_ref().expect("subagent picker");
            picker
                .items
                .iter()
                .map(|item| item.value.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(values, ["2", "4", "8", "16", "32", "64", "128", "256"]);

        let next = {
            let picker = app.picker.as_ref().expect("subagent picker");
            picker
                .items
                .iter()
                .position(|item| item.value == "16")
                .expect("16 row")
        };
        app.picker.as_mut().expect("picker").selected = next;
        app.picker_enter(PickerKind::Subagents, &tx);
        assert!(app.picker.is_none());
        match rx.try_recv().expect("picker command") {
            Command::SetSubagentConcurrency { value } => assert_eq!(value, 16),
            _ => panic!("expected a subagent concurrency command"),
        }

        for invalid in ["0", "1", "3", "6", "12", "24", "48", "100", "512"] {
            assert!(app.handle_prompt_command(&format!("/subagents {invalid}"), &tx));
            assert!(
                app.error
                    .as_deref()
                    .is_some_and(|err| err.contains("2, 4, 8, 16, 32, 64, 128, or 256")),
                "{invalid}"
            );
        }
        assert!(
            rx.try_recv().is_err(),
            "invalid values must not reach the daemon"
        );
    }

    #[test]
    fn perm_command_opens_a_searchable_picker() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "chat", "/tmp"),
            Vec::new(),
        ));

        assert!(app.handle_prompt_command("/perm", &tx));
        let values = {
            let picker = app.picker.as_ref().expect("mode picker");
            assert_eq!(picker.kind, PickerKind::Modes);
            picker
                .items
                .iter()
                .map(|item| item.value.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(values, ["plan", "build", "auto", "bypass"]);
        let current = app
            .picker
            .as_ref()
            .expect("picker")
            .items
            .iter()
            .find(|item| item.value == "build")
            .expect("build row");
        assert!(current.label.contains("(current)"), "{:?}", current.label);

        app.picker.as_mut().expect("picker").query = "bypass".into();
        app.picker.as_mut().expect("picker").selected = 0;
        app.picker_enter(PickerKind::Modes, &tx);
        assert!(app.picker.is_none());
        let mut selected = None;
        while let Ok(command) = rx.try_recv() {
            if let Command::SetMode { mode, .. } = command {
                selected = Some(mode);
            }
        }
        assert_eq!(selected, Some(SessionMode::Bypass));
    }

    #[test]
    fn mode_alias_does_not_compete_with_model_in_the_command_menu() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "chat", "/tmp"),
            Vec::new(),
        ));
        app.focused = 0;
        app.focus_input();

        // The old `/mode` spelling resolves straight to `/perm`, so Enter and
        // Tab can no longer land on `/model` just because it shares a prefix.
        app.input.set_text("/mode");
        let names: Vec<&str> = app
            .command_matches()
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
        assert_eq!(names, ["perm"]);
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Modes)
        ));

        // The canonical command itself still opens the same picker.
        app.picker = None;
        app.focus_input();
        app.input.set_text("/perm");
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Modes)
        ));

        // And `/model` remains unambiguous.
        app.picker = None;
        app.focus_input();
        app.input.set_text("/model");
        let names: Vec<&str> = app
            .command_matches()
            .iter()
            .map(|spec| spec.name.as_str())
            .collect();
        assert_eq!(names, ["model"]);
        app.handle_input_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &tx);
        assert!(matches!(
            app.picker.as_ref().map(|picker| picker.kind),
            Some(PickerKind::Models)
        ));
    }

    #[test]
    fn tab_and_shift_tab_cycle_every_session_mode() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus = Focus::Transcript;

        // The default is Build; Tab walks forward through the full ring.
        assert_eq!(app.views[0].session.mode, SessionMode::Build);
        for expected in [
            SessionMode::Auto,
            SessionMode::Bypass,
            SessionMode::Plan,
            SessionMode::Build,
        ] {
            app.handle_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &tx);
            assert_eq!(app.views[0].session.mode, expected);
            match rx.try_recv().expect("set mode") {
                Command::SetMode { mode, .. } => assert_eq!(mode, expected),
                _ => panic!("unexpected command"),
            }
        }

        // Shift-Tab mirrors the ring: Build → Plan → Bypass → Auto → Build.
        // Terminals encode it as either BackTab or Shift+Tab; both must work.
        for (step, expected) in [
            SessionMode::Plan,
            SessionMode::Bypass,
            SessionMode::Auto,
            SessionMode::Build,
        ]
        .into_iter()
        .enumerate()
        {
            let key = if step % 2 == 0 {
                KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT)
            } else {
                KeyEvent::new(KeyCode::Tab, KeyModifiers::SHIFT)
            };
            app.handle_key(key, &tx);
            assert_eq!(app.views[0].session.mode, expected);
            match rx.try_recv().expect("set mode") {
                Command::SetMode { mode, .. } => assert_eq!(mode, expected),
                _ => panic!("unexpected command"),
            }
        }
    }

    #[test]
    fn leader_m_still_opens_the_full_mode_picker() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = app_with_panes(&["s1"]);
        app.focus = Focus::Transcript;

        app.handle_key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE), &tx);
        app.handle_key(KeyEvent::new(KeyCode::Char('m'), KeyModifiers::NONE), &tx);
        let picker = app.picker.as_ref().expect("mode picker");
        assert_eq!(picker.kind, PickerKind::Modes);
        let values: Vec<&str> = picker
            .items
            .iter()
            .map(|item| item.value.as_str())
            .collect();
        assert_eq!(values, ["plan", "build", "auto", "bypass"]);
        assert!(
            rx.try_recv().is_err(),
            "opening the picker must not change the mode"
        );
        assert!(app.status.starts_with("mode: "), "{}", app.status);
    }

    #[test]
    fn old_permission_command_is_gone() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        app.views.push(SessionView::new(
            Session::new("s1", "chat", "/tmp"),
            Vec::new(),
        ));

        assert!(app.handle_prompt_command("/permission", &tx));
        assert!(app.picker.is_none());
        assert!(app
            .error
            .as_deref()
            .is_some_and(|error| error.contains("unknown command /permission")));
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

    /// The status bar sits directly above the always-visible command line.
    fn status_bar_row(rendered: &str) -> &str {
        let mut lines = rendered.lines().rev();
        lines.next().expect("command line row");
        lines.next().expect("status bar row")
    }

    #[test]
    fn prompt_grows_to_show_a_long_command() {
        let mut app = App::new(None);
        app.focus = Focus::Input;
        app.input
            .set_text(&format!("echo {} ENDMARK", "x".repeat(300)));

        let rendered = render_text(&mut app, 80, 40);
        assert!(rendered.contains("echo"), "{rendered}");
        assert!(rendered.contains("ENDMARK"), "{rendered}");
    }

    #[test]
    fn prompt_border_stays_below_the_draft_inside_the_box() {
        let mut app = App::new(None);
        let mut session = Session::new("s1", "one", "/tmp");
        session.model = Some("llmgate/deepseek/deepseek-v4.1-flash#high".into());
        app.views.push(SessionView::new(session, Vec::new()));
        app.focused = 0;
        app.focus = Focus::Input;
        app.connected = true;
        app.git_cwd = Some(std::path::PathBuf::from("/tmp"));
        app.git_branch = Some("main".into());
        app.models.push(ModelInfo {
            id: "llmgate/deepseek/deepseek-v4.1-flash".into(),
            api_model: Some("deepseek/deepseek-v4.1-flash".into()),
            name: "deepseek/deepseek-v4.1-flash".into(),
            provider: "llmgate".into(),
            default: false,
            variants: vec!["high".into()],
            input_cost: None,
            output_cost: None,
            context_tokens: None,
        });
        app.input.set_text("hello footer");

        let rendered = render_text(&mut app, 80, 20);
        let lines: Vec<&str> = rendered.lines().collect();
        let text_row = lines
            .iter()
            .position(|line| line.contains("hello footer"))
            .expect("draft");
        let bottom_row = lines
            .iter()
            .rposition(|line| line.contains('╰'))
            .expect("prompt bottom");
        assert!(!lines[bottom_row - 1].contains("Build"), "{rendered}");
        assert!(!lines[bottom_row - 1].contains("deepseek"), "{rendered}");
        let status_row = status_bar_row(&rendered);
        assert!(status_row.contains(" I · Build  ·"), "{rendered}");
        assert!(status_row.contains("deepseek-v4.1-flash"), "{rendered}");
        assert!(status_row.contains("llmgate"), "{rendered}");
        assert!(status_row.contains("· high"), "{rendered}");
        assert!(status_row.contains("· main"), "{rendered}");
        let top_row = lines
            .iter()
            .position(|line| line.contains('╭'))
            .expect("prompt top border");
        assert!(!lines[top_row].contains("main"), "{rendered}");
        assert!(
            !lines[top_row.saturating_sub(1)].contains('└'),
            "the transcript should not add a second border above the prompt:\n{rendered}"
        );
        assert!(text_row < bottom_row, "{rendered}");
    }

    #[test]
    fn status_bar_keeps_workspace_metadata_when_narrow() {
        let mut app = App::new(None);
        let mut session = Session::new("s1", "one", "/tmp");
        session.model = Some("provider/a-very-long-model-name#high".into());
        app.views.push(SessionView::new(session, Vec::new()));
        app.focused = 0;
        app.connected = true;
        app.git_cwd = Some(std::path::PathBuf::from("/tmp"));
        app.git_branch = Some("main".into());

        let rendered = render_text(&mut app, 40, 20);
        let bar = status_bar_row(&rendered);
        assert!(bar.contains("/tmp · main"), "{bar}");
        assert!(bar.contains("Build  ·"), "{bar}");
        assert!(UnicodeWidthStr::width(bar) <= 40, "{bar}");
    }

    #[test]
    fn ctrl_t_cycles_reasoning_strength() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None);
        let mut session = Session::new("s1", "one", "/tmp");
        session.model = Some("deepseek/deepseek-v4.1-flash".into());
        app.views.push(SessionView::new(session, Vec::new()));
        app.focused = 0;
        app.focus = Focus::Input;
        app.input.enter_insert();
        app.connected = true;

        // No suffix means the default medium, so the first step is high.
        assert_eq!(app.model_header_parts().2.as_deref(), Some("medium"));
        app.status = "ready".into();
        for (expected, variant) in [
            ("deepseek/deepseek-v4.1-flash#high", "high"),
            ("deepseek/deepseek-v4.1-flash#low", "low"),
            ("deepseek/deepseek-v4.1-flash", "medium"),
            ("deepseek/deepseek-v4.1-flash#high", "high"),
            ("deepseek/deepseek-v4.1-flash#low", "low"),
        ] {
            app.handle_key(
                KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
                &tx,
            );
            assert_eq!(app.views[0].session.model.as_deref(), Some(expected));
            assert_eq!(app.model_header_parts().2.as_deref(), Some(variant));
            assert_eq!(
                app.status, "ready",
                "Ctrl-T must not duplicate the reasoning label"
            );
            assert!(app.input.text().is_empty());
            match rx.try_recv().expect("set model command") {
                Command::SetModel { model, .. } => {
                    assert_eq!(model.as_deref(), Some(expected));
                }
                _ => panic!("unexpected command"),
            }
        }
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
        assert!(empty.contains("Build  · ◰"), "{empty}");
        assert!(!empty.contains("eixd"), "no footer brand: {empty}");
        assert!(!empty.contains("/cmds"), "no footer hints: {empty}");
        assert!(empty.contains("1 ◰ prompt polish"), "{empty}");
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
        let lines: Vec<&str> = empty.lines().collect();
        let top_border = lines
            .iter()
            .position(|line| line.contains('╭'))
            .expect("prompt top border");
        let prompt_bottom = lines
            .iter()
            .rposition(|line| line.contains('╰'))
            .expect("prompt bottom");
        let status_bar = status_bar_row(&empty);
        assert!(!lines[top_border].contains("~/git/eix"), "{empty}");
        assert!(!lines[prompt_bottom - 1].contains(" I "), "{empty}");
        assert!(!lines[prompt_bottom - 1].contains("Build"), "{empty}");
        assert!(status_bar.contains(" I · Build  ·"), "{empty}");
        assert!(status_bar.contains("~/git/eix"), "{empty}");
        assert!(!lines[prompt_bottom - 1].contains("gpt-5"), "{empty}");
        assert!(status_bar.contains("gpt-5"), "{empty}");
        assert!(status_bar.contains("openai"), "{empty}");
        assert!(status_bar.contains("medium"), "{empty}");
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
    fn status_bar_shows_latest_status() {
        let mut app = App::new(None);
        let mut session = Session::new("s1", "status test", "/tmp");
        session.model = Some("openai/gpt-5".into());
        app.views.push(SessionView::new(session, Vec::new()));
        app.focused = 0;
        app.focus = Focus::Input;
        app.connected = true;

        // Attach/lifecycle chatter stays out of the statusline; the pane title
        // already shows the session name.
        app.status = "attached".into();
        let text = render_text(&mut app, 80, 20);
        let bar = status_bar_row(&text);
        assert!(!bar.contains("attached"), "{bar}");
        assert!(!bar.contains("status test"), "{bar}");

        app.status = "connected to eixd 0.1.0".into();
        let text = render_text(&mut app, 80, 20);
        let bar = status_bar_row(&text);
        assert!(!bar.contains("connected"), "{bar}");

        // The spinner/status glyph already conveys idle/running.
        for hidden in ["idle", "running"] {
            app.status = hidden.into();
            let text = render_text(&mut app, 80, 20);
            let bar = status_bar_row(&text);
            assert!(!bar.contains(hidden), "{bar}");
        }

        app.status = "retrying 2/5".into();
        let text = render_text(&mut app, 80, 20);
        let bar = status_bar_row(&text);
        assert!(bar.contains("Build  ·"), "{bar}");
        assert!(bar.contains("retrying 2/5"), "{bar}");
        assert!(bar.contains("gpt-5"), "{bar}");
        // The primary group is ordered Vim mode → agent mode → status → model
        // → provider → reasoning, and cwd stays pinned to the right edge.
        assert!(bar.trim_start().starts_with("I · Build  ·"), "{bar}");
        let vim_mode = bar.find('I').unwrap();
        let build = bar.find("Build").unwrap();
        let status = bar.find("Build  · ").unwrap() + "Build  · ".len();
        let model = bar.find("gpt-5").unwrap();
        let provider = bar.find("openai").unwrap();
        let reasoning = bar.find("medium").unwrap();
        assert!(
            vim_mode < build
                && build < status
                && status < model
                && model < provider
                && provider < reasoning,
            "{bar}"
        );
        assert!(bar.trim_end().ends_with("/tmp"), "{bar}");

        // The mode badge follows the active context, not the editor buffer.
        app.focus = Focus::Transcript;
        let text = render_text(&mut app, 80, 20);
        let bar = status_bar_row(&text);
        assert!(bar.trim_start().starts_with("N · Build  ·"), "{text}");
    }

    #[test]
    fn attention_alerts_render_centered_in_the_status_bar() {
        let mut app = App::new(None);
        let mut session = Session::new("s1", "status test", "/tmp");
        session.model = Some("openai/gpt-5".into());
        app.views.push(SessionView::new(session, Vec::new()));
        app.focused = 0;
        app.focus = Focus::Input;
        app.connected = true;
        app.status = "retrying 2/5".into();
        app.last_alert = Some("worker · awaiting approval".into());
        app.alert_at = Some(Instant::now());

        let text = render_text(&mut app, 80, 20);
        let bar = status_bar_row(&text);
        let alert = bar
            .find("worker · awaiting approval")
            .expect("center alert");
        assert!(alert > bar.find("Build").unwrap(), "{bar}");
        assert!(alert < bar.find("/tmp").unwrap(), "{bar}");
        assert!(!bar.contains("retrying 2/5"), "{bar}");

        // The alert temporarily replaces the secondary metadata; once it
        // expires the session status and model context come back.
        app.alert_at = Some(Instant::now() - ALERT_DISPLAY);
        let text = render_text(&mut app, 80, 20);
        let bar = status_bar_row(&text);
        assert!(!bar.contains("worker · awaiting approval"), "{bar}");
        assert!(bar.contains("retrying 2/5"), "{bar}");
        assert!(bar.contains("gpt-5"), "{bar}");
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
        assert!(text.contains("Build  ·"), "{text}");
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
