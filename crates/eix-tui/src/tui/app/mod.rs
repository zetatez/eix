//! TUI state and rendering.
//!
//! The prompt is a self-contained Vim-style editor (see [`super::editor`]);
//! there is no embedded editor process.

use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};

use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use eix_core::{
    theme_preset, ApprovalDecision, ApprovalRequiredPayload, AssistantMessagePayload, AttachResult,
    Event as SessionEvent, EventKind, HelloResult, HunkInfo, InputAnsweredPayload, InputQuestion,
    InputRequiredPayload, IntegrationState, IntegrationStatus, ModelInfo, PatchProposedPayload,
    PickerItem, ProcessInfo, ProcessList, ProcessOutput, ProcessStatus, Session, SessionMode,
    SessionStatus, SubmitResult, TodoItem, ToolResultPayload, UserMessagePayload, THEME_PRESETS,
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
use super::diff::DiffRow;
use super::editor::{Editor, Mode, Outcome};
use super::keys::{KeyAction, Keybinds};
use super::theme::Theme;
use super::transcript::{self, FoldId, Subagent, SubagentStatus};
use super::workspace::{ActiveWorkspace, WorkspaceLayout};

mod approval;
mod command;
mod event;
mod help;
mod lifecycle;
mod notification;
mod overlay;
mod overlay_draw;
mod prompt;
mod search;
mod session;
mod status;
mod view;

#[cfg(test)]
mod tests;

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

/// Keep the Vim `:` history bounded for long-running clients.
const CMDLINE_HISTORY_LIMIT: usize = 100;

/// Width of the command name column in the `/` menu and the Ctrl-P palette.
/// Sharing it keeps the description/annotation column aligned in both places.
const COMMAND_NAME_WIDTH: usize = 22;

/// Maximum label column for picker rows that also render a detail. Labels
/// longer than this are truncated so the detail column stays visible.
const PICKER_LABEL_MAX: usize = 28;

/// Maximum key column in the searchable help/key reference.
const HELP_KEY_MAX: usize = 28;

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
const COMMAND_ALIASES: &[(&str, &str)] =
    &[("mode", "perm"), ("worktrees", "git"), ("branches", "git")];

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
        builtin(
            "new",
            "[name]",
            "start a session; reuse the focused session's worktree if present",
        ),
        builtin("new-pane", "", "open a session in a new pane"),
        builtin(
            "new-worktree",
            "[name]",
            "move this session into a new git worktree",
        ),
        builtin(
            "ready",
            "",
            "queue this worktree for rebase, tests and merge",
        ),
        builtin("integrate", "", "show this session's integration state"),
        builtin("files", "", "browse and insert workspace files"),
        builtin(
            "grep",
            "[query]",
            "search file contents and insert @path:line",
        ),
        builtin(
            "git",
            "",
            "browse worktrees and branches, insert a path or name",
        ),
        builtin("processes", "", "list and inspect background processes"),
        builtin("approvals", "", "open the approval inbox"),
        builtin("attention", "", "jump to the next pane needing attention"),
        builtin("cancel", "", "cancel the focused session's running turn"),
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
        builtin(
            "subagents",
            "[n|limit]",
            "monitor live sub-agents or set the max parallel spawn_agent calls",
        ),
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
fn merge_commands(
    cwd: &std::path::Path,
    compat: &eix_core::CompatConfig,
    plugins: &[eix_core::PluginCommandInfo],
) -> Vec<CommandSpec> {
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
    for command in plugins {
        let spec = CommandSpec {
            name: command.name.clone(),
            args: command.args.clone(),
            description: command.description.clone(),
            template: Some(command.prompt.clone()),
            agent: None,
            model: None,
        };
        // Plugin names are namespaced, but a user can still create a Markdown
        // command under the same path. Built-ins and user commands win: a
        // plugin must never silently replace either.
        if !commands.iter().any(|item| item.name == spec.name) {
            commands.push(spec);
        }
    }
    commands
}

/// Prompt height: at least [`super::editor::PROMPT_ROWS`] content rows plus the
/// two borders. Long drafts grow the box up to twelve total rows before the
/// editor starts scrolling, and short terminals always keep transcript rows.
#[cfg(test)]
fn prompt_input_height(area_height: u16, area_width: u16, input: &Editor) -> u16 {
    let inner_width = area_width.saturating_sub(4).max(1) as usize;
    let rows = input.wrap(inner_width).segments.len();
    prompt_input_height_for_rows(area_height, rows)
}

fn prompt_input_height_for_rows(area_height: u16, rows: usize) -> u16 {
    // Ten editor rows + two borders = twelve rows total.
    const PROMPT_MAX_ROWS: usize = 10;
    let visible = rows.clamp(super::editor::PROMPT_ROWS as usize, PROMPT_MAX_ROWS) as u16;
    (visible + 2).min(area_height.saturating_sub(5).max(4))
}

// Keep the prompt readable without a separate panel: its chrome paints over
// the theme canvas (or the terminal background) instead of an opaque composer
// box.
const COMPOSER_TEXT: Color = Color::Rgb(220, 223, 228);
const COMPOSER_MUTED: Color = Color::Rgb(140, 149, 168);
const COMPOSER_BORDER: Color = Color::Rgb(74, 88, 120);
const COMPOSER_INACTIVE_BORDER: Color = Color::Rgb(50, 55, 68);
const COMPOSER_ACCENT: Color = Color::Rgb(122, 162, 247);
const COMPOSER_FOCUS: Color = Color::Rgb(154, 202, 255);
const COMPOSER_SUCCESS: Color = Color::Rgb(126, 199, 149);
const COMPOSER_WARN: Color = Color::Rgb(229, 192, 123);
const COMPOSER_ORANGE: Color = Color::Rgb(245, 169, 127);
const COMPOSER_DANGER: Color = Color::Rgb(231, 130, 132);
const COMPOSER_BADGE_TEXT: Color = Color::Rgb(20, 22, 28);
const COMPOSER_BADGE_TEXT_LIGHT: Color = Color::Rgb(248, 250, 252);
/// Keep accent text/badges legible on themed backgrounds without abandoning
/// their hue. Bold status text is checked against the WCAG large-text floor.
const MIN_ACCENT_CONTRAST: f32 = 3.0;
const COMPOSER_SELECTION: Color = Color::Rgb(58, 66, 84);

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

/// Visual tone for a transient top-right notification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ToastTone {
    Success,
    Warning,
}

/// Short-lived top-right notification. Only failures that have no other UI
/// surface, background attention, copy confirmations and layout changes use
/// this; successful commands and popup navigation are already visible where
/// they happen.
struct Toast {
    text: String,
    tone: ToastTone,
    expires_at: Instant,
}

/// Notifications never wait for a previous one to finish. Each record expires
/// independently, so a burst can stack while a new message appears at once.
const COPY_TOAST_DISPLAY: Duration = Duration::from_millis(1500);
const LAYOUT_TOAST_DISPLAY: Duration = Duration::from_millis(1500);
const WARNING_TOAST_DISPLAY: Duration = Duration::from_secs(3);
const ATTENTION_TOAST_DISPLAY: Duration = Duration::from_secs(3);
const RELOAD_TOAST_DISPLAY: Duration = Duration::from_millis(1500);
/// Keep the top-right corner from becoming a long-lived notification log.
const MAX_TOAST_DISPLAY: Duration = Duration::from_secs(3);
const MAX_TOASTS: usize = 8;

/// Whether a status change is a failure the user cannot otherwise see. Normal
/// progress, successful command feedback and popup contents stay silent.
fn status_needs_warning(status: &str) -> bool {
    const MARKERS: &[&str] = &[
        "failed",
        "error",
        "stale",
        "conflict",
        "cannot ",
        "usage:",
        "editor:",
        "dropped ",
        "only staged",
        "wait for ",
        "still loading",
        "out of range",
        "no session",
        "no previous search",
        "no matches",
        "no earlier prompts",
        "no queued prompt",
        "no pending approval",
        "no diff to review",
        "no hunks selected",
        "no file to open",
        "no integration state",
        "no model",
        "no longer listed",
        "attach a session",
        "does not exist",
        "not running",
        "not loaded",
        "not open",
    ];
    let status = status.trim();
    // A retry is progress: the eventual failure arrives through the error
    // dialog, so do not turn each attempt into a warning toast.
    !status.is_empty()
        && !status.contains("retrying ")
        && MARKERS.iter().any(|marker| status.contains(marker))
}

/// Inputs of the live tail render. Streaming text only grows, so lengths plus
/// the reasoning fold and queued prompts are enough to know the cached tail is
/// still current.
struct TailKey {
    width: usize,
    theme: Theme,
    text_len: usize,
    reasoning_len: usize,
    reasoning_open: Option<bool>,
    queued: Vec<String>,
    /// Waiting dots advance with the 200 ms tick and only matter while the
    /// model has produced nothing yet.
    dots: Option<&'static str>,
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
    history_cache: Option<transcript::HistoryCache>,
    /// Cached live tail (streaming text, reasoning and queued prompts).
    tail: transcript::Transcript,
    tail_key: Option<TailKey>,
    /// Total rendered lines (history + tail), used for scroll bounds without
    /// materialising the text of every line.
    line_count: usize,
    /// Whether `text_lines` matches the current history and tail. It is rebuilt
    /// lazily for search, copy and click-to-fold.
    text_lines_valid: bool,
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
            tail: transcript::Transcript::new(),
            tail_key: None,
            line_count: 0,
            text_lines_valid: true,
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

    /// Rebuild the cached history when its inputs changed. The cache compares
    /// per-block identities, so a new event or a single fold toggle only
    /// re-renders the blocks it actually touched.
    fn ensure_history(&mut self, width: usize, theme: Theme) {
        let stale = self.history_cache.as_ref().is_none_or(|cache| {
            cache.needs_render(
                width,
                theme,
                self.events_rev,
                self.subagents_rev,
                &self.folds,
            )
        });
        if !stale {
            return;
        }
        let cache = self
            .history_cache
            .get_or_insert_with(transcript::HistoryCache::default);
        cache.render(
            &self.events,
            &self.folds,
            width,
            theme,
            &self.subagents,
            self.subagents_rev,
            self.events_rev,
        );
        self.slots = cache.slots.clone();
        self.user_lines = cache.user_lines.clone();
        self.text_lines_valid = false;
    }

    /// What the live tail should render. A running turn with no streamed text
    /// still needs a placeholder: after a tool result is stored the stream is
    /// cleared, and the model can spend a while producing the next step.
    fn live(&self) -> Option<transcript::Live<'_>> {
        match self.stream.as_ref() {
            Some(stream) => Some(transcript::Live {
                reasoning: &stream.reasoning,
                text: &stream.text,
            }),
            None if self.session.status == SessionStatus::Running => Some(transcript::Live {
                reasoning: "",
                text: "",
            }),
            None => None,
        }
    }

    /// Render the in-flight turn only when its inputs changed. A 200 ms tick or
    /// an unrelated notification no longer re-parses a streaming message.
    fn ensure_tail(&mut self, width: usize, theme: Theme, tick: u64, queued: &[String]) {
        let (text_len, reasoning_len) = self
            .stream
            .as_ref()
            .map(|stream| (stream.text.len(), stream.reasoning.len()))
            .unwrap_or((0, 0));
        let dots = if self
            .live()
            .is_some_and(|live| live.text.is_empty() && live.reasoning.is_empty())
        {
            Some(transcript::waiting_dots(tick))
        } else {
            None
        };
        let reasoning_open = self.folds.get(&FoldId::Live).copied();
        if let Some(key) = self.tail_key.as_ref() {
            if key.width == width
                && key.theme == theme
                && key.text_len == text_len
                && key.reasoning_len == reasoning_len
                && key.reasoning_open == reasoning_open
                && key.dots == dots
                && key.queued.as_slice() == queued
            {
                return;
            }
        }
        let live = self.live();
        let mut tail = transcript::render_tail(live, queued, &self.folds, width, theme, tick);
        if self.session.is_agent() && self.session.parent_id.is_some() {
            tail.lines.push(parent_link_line(theme));
            tail.slots.push(None);
        }
        self.tail = tail;
        self.tail_key = Some(TailKey {
            width,
            theme,
            text_len,
            reasoning_len,
            reasoning_open,
            queued: queued.to_vec(),
            dots,
        });
        self.text_lines_valid = false;
    }

    /// Plain text for search/copy/fold clicks. Building it on demand keeps the
    /// per-event path free of a `String` per rendered line.
    fn ensure_text_lines(&mut self) {
        if self.text_lines_valid {
            return;
        }
        let mut lines = Vec::with_capacity(self.line_count);
        if let Some(cache) = self.history_cache.as_ref() {
            lines.extend(cache.lines.iter().map(text_line));
        }
        lines.extend(self.tail.lines.iter().map(text_line));
        self.text_lines = lines;
        self.text_lines_valid = true;
    }

    /// Child agents in a stable order for the picker. Sorting by name keeps
    /// rows from moving when a child changes status.
    fn subagents_sorted(&self) -> Vec<(&str, &Subagent)> {
        let mut agents: Vec<(&str, &Subagent)> = self
            .subagents
            .iter()
            .map(|(id, agent)| (id.as_str(), agent))
            .collect();
        agents.sort_by(|a, b| {
            a.1.name
                .cmp(&b.1.name)
                .then_with(|| a.1.call_id.cmp(&b.1.call_id))
                .then_with(|| a.0.cmp(b.0))
        });
        agents
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

    /// Append a streamed child delta. The card shows thinking live; the picker
    /// preview shows both thinking and the answer. Stored assistant events
    /// clear these buffers.
    fn push_subagent_delta(&mut self, agent_id: &str, turn_id: &str, reasoning: bool, delta: &str) {
        let Some(subagent) = self.subagents.get_mut(agent_id) else {
            return;
        };
        if reasoning {
            if subagent.reasoning_turn != turn_id {
                subagent.reasoning.clear();
                subagent.reasoning_turn.clear();
                subagent.reasoning_turn.push_str(turn_id);
            }
            subagent.reasoning.push_str(delta);
        } else {
            if subagent.text_turn != turn_id {
                subagent.text.clear();
                subagent.text_turn.clear();
                subagent.text_turn.push_str(turn_id);
            }
            subagent.text.push_str(delta);
        }
        self.subagents_rev = self.subagents_rev.wrapping_add(1);
    }

    /// Drop the live buffers when the turn is reset or retried. A retry
    /// streams the reasoning and answer again from the start.
    fn clear_subagent_reasoning(&mut self, agent_id: &str, turn_id: &str) {
        let Some(subagent) = self.subagents.get_mut(agent_id) else {
            return;
        };
        let mut changed = false;
        if subagent.reasoning_turn == turn_id {
            subagent.reasoning.clear();
            subagent.reasoning_turn.clear();
            changed = true;
        }
        if subagent.text_turn == turn_id {
            subagent.text.clear();
            subagent.text_turn.clear();
            changed = true;
        }
        if changed {
            self.subagents_rev = self.subagents_rev.wrapping_add(1);
        }
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

    /// Resolve a rendered `spawn_agent` card back to the child session id.
    fn subagent_session_for_fold(&self, fold: FoldId) -> Option<&str> {
        let FoldId::Event(seq) = fold else {
            return None;
        };
        let event = self.events.iter().find(|event| event.seq == seq)?;
        if event.kind != EventKind::ToolCall
            || event.payload.get("tool").and_then(Value::as_str) != Some("spawn_agent")
        {
            return None;
        }
        let call_id = event.payload.get("call_id").and_then(Value::as_str)?;
        self.subagents
            .iter()
            .find(|(_, subagent)| subagent.call_id == call_id)
            .map(|(session_id, _)| session_id.as_str())
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
                    // Only the id is needed here; reading it from the payload
                    // avoids cloning the (possibly large) tool output.
                    if let Some(call_id) = event.payload.get("call_id").and_then(Value::as_str) {
                        resolved_calls.insert(call_id.to_string());
                    }
                }
                EventKind::PatchApplied | EventKind::PatchRejected | EventKind::PatchConflict => {
                    if let Some(patch_id) = event.payload.get("patch_id").and_then(Value::as_str) {
                        resolved_patches.insert(patch_id.to_string());
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
                    if let Ok(payload) = ApprovalRequiredPayload::deserialize(&event.payload) {
                        if !resolved_calls.contains(&payload.call_id) {
                            // Shell approvals carry a deliberately short
                            // daemon summary. Keep the real command line so the
                            // card can show it in full instead of approving a
                            // truncated command.
                            let mut summary = if matches!(payload.tool.as_str(), "exec_command") {
                                payload
                                    .arguments
                                    .get("cmd")
                                    .or_else(|| payload.arguments.get("command"))
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
                    if let Ok(payload) = PatchProposedPayload::deserialize(&event.payload) {
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
            if let Ok(payload) = InputAnsweredPayload::deserialize(&event.payload) {
                answered.insert(payload.input_id);
            }
        }
        let mut pending = None;
        for event in &self.events {
            if event.kind != EventKind::InputRequired {
                continue;
            }
            if let Ok(payload) = InputRequiredPayload::deserialize(&event.payload) {
                if !answered.contains(&payload.input_id) {
                    pending = Some(PendingInput::new(
                        payload.input_id,
                        payload.call_id,
                        payload.questions,
                    ));
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
    /// Search workspace file contents and insert `@path:line`.
    Grep,
    /// Insert a git worktree path or branch name into the prompt.
    Git,
    /// Client-side command palette (`Ctrl-P`).
    Commands,
    /// Pin the focused session to a configured model.
    Models,
    /// Select the focused session's primary agent.
    Agents,
    /// Adjust the daemon's runtime subagent concurrency limit.
    Subagents,
    /// Browse the focused session's child agents with live activity.
    SubagentActivity,
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
            Self::Grep => Some(eix_core::KIND_GREP),
            Self::Git => Some(eix_core::KIND_GIT),
            Self::Sessions
            | Self::NewPane
            | Self::Commands
            | Self::Models
            | Self::Agents
            | Self::Subagents
            | Self::SubagentActivity
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
            Self::Grep => "grep — file contents",
            Self::Git => "git — worktree/branch",
            Self::Commands => "commands",
            Self::Models => "model — search and switch",
            Self::Agents => "agents — choose the primary agent",
            Self::Subagents => "subagents — parallel limit",
            Self::SubagentActivity => "subagents — live activity",
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
            Self::Grep => "type to search · Enter insert @path:line · Esc close",
            Self::Git => "Enter insert · ↑/↓ or C-j/C-k/C-n/C-p · Esc close",
            Self::Commands => "Enter run · ↑/↓ or C-j/C-k/C-n/C-p · Esc close",
            Self::Models => "type to search · Enter switch · Esc close",
            Self::Agents => "Enter select · ↑/↓ or C-j/C-k/C-n/C-p · Esc close",
            Self::Subagents => "Enter apply · ↑/↓ or C-j/C-k/C-n/C-p · Esc close",
            Self::SubagentActivity => {
                "type to search · Enter open child pane · ↑/↓ or C-j/C-k/C-n/C-p · Esc close"
            }
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

/// Filtered row indices, recomputed only when the query or the source rows
/// change. Fuzzy matching over a large file list used to run on every frame.
#[derive(Default)]
struct PickerFilterCache {
    kind: Option<PickerKind>,
    query: String,
    items_rev: u64,
    sessions_rev: u64,
    subagents_rev: u64,
    rows: Vec<usize>,
    valid: bool,
}

struct Picker {
    kind: PickerKind,
    query: String,
    selected: usize,
    /// Rows for data providers and the command palette. Session kinds read
    /// `App::sessions` directly so live status changes show through.
    items: Vec<PickerItem>,
    /// Bumped on every row replacement so the filter cache can notice it.
    items_rev: u64,
    filter: PickerFilterCache,
    /// First visible row of the list window.
    scroll: usize,
    /// True until the daemon answers a data kind, so the overlay can say
    /// "loading" instead of "no matches".
    loading: bool,
    /// Lazy right-pane content for the selected file/session.
    preview: PickerPreview,
    /// Request queued by the last draw; the event loop sends it after drawing.
    preview_request: Option<(String, Option<String>, String)>,
}

impl Picker {
    fn set_items(&mut self, items: Vec<PickerItem>) {
        self.items = items;
        self.items_rev = self.items_rev.wrapping_add(1);
    }

    fn clear_items(&mut self) {
        self.set_items(Vec::new());
    }
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
    /// A blocking tool call (`exec_command`) waiting in the turn loop.
    Tool,
    /// A staged `apply_patch` proposal; the turn has already continued.
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

/// One `request_user_input` request waiting for answers. Each question is a
/// tab; the automatic "Other" row at the end of every option list opens a
/// free-form editor.
#[derive(Clone)]
struct PendingInput {
    input_id: String,
    /// Tool call that produced the questions, used to match `tool.completed`.
    call_id: String,
    questions: Vec<InputQuestion>,
    /// Visible question tab.
    tab: usize,
    /// Highlighted row per question, including the trailing "Other" row.
    cursors: Vec<usize>,
    /// Free-form text per question.
    other: Vec<String>,
    /// Committed answer per question (`None` until answered).
    answers: Vec<Option<String>>,
    /// Set while the "Other" row is being edited.
    editing: bool,
}

impl PendingInput {
    fn new(input_id: String, call_id: String, questions: Vec<InputQuestion>) -> Self {
        let count = questions.len();
        Self {
            input_id,
            call_id,
            questions,
            tab: 0,
            cursors: vec![0; count],
            other: vec![String::new(); count],
            answers: vec![None; count],
            editing: false,
        }
    }

    fn row_count(&self, tab: usize) -> usize {
        self.questions
            .get(tab)
            .map(|question| question.options.len() + 1)
            .unwrap_or(0)
    }

    fn is_other(&self, tab: usize) -> bool {
        let question = self.questions.get(tab);
        question.is_some_and(|question| self.cursors[tab] >= question.options.len())
    }

    fn all_answered(&self) -> bool {
        self.answers.iter().all(Option::is_some)
    }

    /// Advance to the next unanswered tab. Returns false when every question
    /// already has an answer.
    fn next_unanswered(&mut self) -> bool {
        let count = self.questions.len();
        for offset in 1..=count {
            let tab = (self.tab + offset) % count;
            if self.answers[tab].is_none() {
                self.tab = tab;
                self.editing = false;
                return true;
            }
        }
        false
    }

    /// Answers keyed by question id, in the shape `input.respond` expects.
    fn answer_map(&self) -> std::collections::BTreeMap<String, Vec<String>> {
        self.questions
            .iter()
            .zip(&self.answers)
            .filter_map(|(question, answer)| {
                answer
                    .as_ref()
                    .map(|answer| (question.id.clone(), vec![answer.clone()]))
            })
            .collect()
    }
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

/// Cross-session list of pending approvals, opened with `Space a`.
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

/// Parsed diff rows for the newest approval render. Parsing a large unified
/// diff allocates a row per line, so keep it until the approval changes.
struct DiffRowsCache {
    rev: u64,
    session: String,
    rows: Vec<DiffRow>,
}

/// Rendered side-by-side body for the approval card.
struct CardDiffCache {
    rev: u64,
    session: String,
    width: usize,
    theme: Theme,
    shown: usize,
    lines: Vec<Line<'static>>,
}

/// Rendered interactive diff for the full-screen review. Scrolling does not
/// invalidate it; only the hunk cursor, selection or width do.
struct DiffBodyCache {
    rev: u64,
    session: String,
    width: usize,
    theme: Theme,
    interactive: bool,
    selected: Vec<bool>,
    cursor: usize,
    lines: Vec<Line<'static>>,
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
            Self::Grid => "grid",
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

    fn workspace_layout(self) -> WorkspaceLayout {
        match self {
            Self::Grid => WorkspaceLayout::Grid,
            Self::Horizontal => WorkspaceLayout::Horizontal,
            Self::Tabby => WorkspaceLayout::Tabby,
            Self::Fibonacci => WorkspaceLayout::Fibonacci,
        }
    }

    fn from_workspace_layout(layout: WorkspaceLayout) -> Self {
        match layout {
            WorkspaceLayout::Grid => Self::Grid,
            WorkspaceLayout::Horizontal => Self::Horizontal,
            WorkspaceLayout::Tabby => Self::Tabby,
            WorkspaceLayout::Fibonacci => Self::Fibonacci,
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
    /// Session ids to re-open as panes after the first session list arrives.
    restore_sessions: Vec<String>,
    /// Restore ids still waiting for the previous attach to finish.
    restore_queue: Vec<String>,
    /// Pane to focus after all restored panes have attached.
    restore_focused: Option<String>,
    /// Set when `:q` / `:qa` intentionally closes the client's panes.
    clear_active_on_quit: bool,
    initialized: bool,
    creating: bool,
    submitting: bool,
    pending_prompt: Option<String>,
    input: Editor,
    focus: Focus,
    picker: Option<Picker>,
    /// A live picker query changed and should be sent on the next tick.
    picker_query_due: bool,
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
    /// Top-right notifications, oldest first. The newest record is drawn on
    /// the top row; each one expires independently.
    toasts: Vec<Toast>,
    /// Status text last observed, so repeated assignments do not retrigger the
    /// warning check even though most call sites write `status` directly.
    last_seen_status: String,
    /// Older-history request picked up by the event loop after drawing.
    pending_history: Option<(String, u64)>,
    /// Pane rectangles from the last draw, used to pick a focus direction.
    pane_areas: Vec<Rect>,
    /// Tab-strip hit boxes from the last draw, keyed by view index.
    tab_areas: Vec<(Rect, usize)>,
    /// Latest integration state keyed by session id, fed by daemon events.
    integrations: HashMap<String, IntegrationState>,
    /// Tool calls waiting on a decision, keyed by session id.
    approvals: HashMap<String, PendingApproval>,
    /// Bumped whenever the pending approval set is replaced.
    approval_rev: u64,
    /// Rows/body caches shared by the card and the full-screen diff review.
    diff_rows: Option<DiffRowsCache>,
    card_diff: Option<CardDiffCache>,
    diff_body: Option<DiffBodyCache>,
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
    /// Submitted prompts for the focused session, keyed by `(session id,
    /// events revision)` so browsing does not rescan the whole event log on
    /// every Ctrl-J/Ctrl-K.
    history_entries: Option<(String, u64, Rc<Vec<String>>)>,
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
    /// Prompt commands advertised by server plugins.
    plugin_commands: Vec<eix_core::PluginCommandInfo>,
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
            let payload = UserMessagePayload::deserialize(&event.payload).ok()?;
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

/// Rows for the `Ctrl-P` command palette. It is generated from the same
/// built-in/user command list as the prompt's `/` menu, so both stay in sync.
/// `value` is the command name dispatched as `/name`.
fn command_items(commands: &[CommandSpec]) -> Vec<PickerItem> {
    commands
        .iter()
        .map(|spec| {
            let label = command_label(spec, "/");
            PickerItem::new(&spec.name)
                .with_label(label.clone())
                .with_detail(&spec.description)
                .with_preview(format!("{label}\n\n{}", spec.description))
        })
        .collect()
}

/// The display form used by the `/` and `:` menus and the command palette.
fn command_label(spec: &CommandSpec, prefix: &str) -> String {
    if spec.args.is_empty() {
        format!("{prefix}{}", spec.name)
    } else {
        format!("{prefix}{} {}", spec.name, spec.args)
    }
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
    #[serde(default)]
    questions: Vec<InputQuestion>,
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
        "exec_command" => {
            let command = arguments
                .get("cmd")
                .and_then(Value::as_str)
                .unwrap_or_else(|| string("command"));
            format!("$ {command}")
        }
        "write_stdin" => format!("stdin {}", truncate(string("session_id"), 12)),
        "view_image" => format!("image {}", string("path")),
        "apply_patch" => {
            let text = ["patchText", "patch", "input"]
                .iter()
                .find_map(|key| arguments.get(*key).and_then(Value::as_str))
                .unwrap_or_default();
            let files: Vec<&str> = text
                .lines()
                .filter_map(|line| {
                    line.strip_prefix("*** Add File: ")
                        .or_else(|| line.strip_prefix("*** Update File: "))
                        .or_else(|| line.strip_prefix("*** Delete File: "))
                        .map(str::trim)
                })
                .collect();
            match files.as_slice() {
                [] => "apply_patch".to_string(),
                [file] => format!("apply_patch {file}"),
                files => format!("apply_patch ({} files)", files.len()),
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
                description: "start a session; reuse the focused session's worktree if present",
            },
            HelpEntry {
                key: "/new-pane",
                description: "choose an existing session for a new pane",
            },
            HelpEntry {
                key: "/new-worktree",
                description: "move the current session into an isolated git worktree",
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
                key: "/grep",
                description: "search file contents and insert @path:line (/grep query)",
            },
            HelpEntry {
                key: "/git",
                description: "search git worktrees and branches, insert a path or name",
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
                description: "click folds Thinking / tool blocks; click a subagent card to open it; click ← parent agent in a child to return; wheel scrolls",
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
                description: "insert: send (queued while busy); normal / transcript: promote the focused session to first",
            },
            HelpEntry {
                key: "Ctrl-J",
                description: "newline (insert mode)",
            },
            HelpEntry {
                key: "Space o/f/g",
                description: "prompt & transcript: new pane / files / git",
            },
            HelpEntry {
                key: "Space s",
                description: "prompt & transcript: search file contents and insert @path:line",
            },
            HelpEntry {
                key: "Space i",
                description: "prompt & transcript: monitor live sub-agents (children first, else the concurrency limit)",
            },
            HelpEntry {
                key: "Space p",
                description: "prompt & transcript: processes",
            },
            HelpEntry {
                key: "Space a/x",
                description: "prompt & transcript: approvals / cancel",
            },
            HelpEntry {
                key: "Space n/c",
                description: "prompt & transcript: new session / close pane",
            },
            HelpEntry {
                key: "q",
                description: "prompt normal & transcript: close the focused pane (the last one stays)",
            },
            HelpEntry {
                key: "< / >",
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
                key: "Space l",
                description: "cycle grid / horizontal / tabby / fibonacci layout",
            },
            HelpEntry {
                key: "Ctrl-1 … Ctrl-9",
                description: "focus the Nth open session / tabby tab (Ctrl-7 opens the model picker on some terminals)",
            },
            HelpEntry {
                key: "session order",
                description: "normal Enter promotes the focused session to first; Ctrl-1…9 jumps directly, < / > or Ctrl-W Ctrl-W moves prev/next",
            },
            HelpEntry {
                key: "Space W",
                description: "prompt & transcript: move this session into a worktree",
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
                key: "Space g",
                description: "session picker (switch the focused pane's session)",
            },
            HelpEntry {
                key: "Space h",
                description: "search this session's user-input history and jump",
            },
            HelpEntry {
                key: "Ctrl-T",
                description: "cycle reasoning (default medium): low / medium / high",
            },
            HelpEntry {
                key: "Ctrl-/",
                description: "open the searchable model picker (Ctrl-_ / Ctrl-7 also work)",
            },
            HelpEntry {
                key: "Space o",
                description: "open session in a new pane",
            },
            HelpEntry {
                key: "Space G",
                description: "insert a git worktree path or branch name",
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
                description: "new session; reuse the focused session's worktree if present",
            },
            HelpEntry {
                key: "Space W",
                description: "move this session into a fresh git worktree",
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
                key: "Space a",
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
                key: "PageUp / PageDown",
                description: "page the transcript (Ctrl-B / Ctrl-U and Ctrl-F also work)",
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
                description: "quit (Z in normal/transcript; Ctrl-C/D in any mode; draft is saved first)",
            },
            HelpEntry {
                key: "[keys]",
                description: "override global/TUI actions and leader chords in config.yaml; `leader` changes the prefix",
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
    // Size the key column from the whole scope, not the filtered rows, so
    // descriptions do not jump horizontally while the user types.
    let key_width = HELP_SECTIONS
        .iter()
        .filter(|section| scope.is_none_or(|scope| scope == section.title))
        .flat_map(|section| section.entries.iter())
        .map(|entry| UnicodeWidthStr::width(entry.key))
        .max()
        .unwrap_or(0)
        .min(HELP_KEY_MAX);

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
        composer_text(theme),
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
            Style::default().fg(composer_muted(theme)),
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
                            Style::default().fg(composer_muted(theme)),
                        ),
                        Span::styled(
                            format!("{} ", pad_to(&truncate(entry.key, key_width), key_width)),
                            Style::default()
                                .fg(composer_accent(theme))
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(entry.description, Style::default().fg(composer_text(theme))),
                    ]);
                    if is_selected {
                        line = line.style(Style::default().bg(composer_selection(theme)));
                    }
                    line
                }
            })
            .collect()
    };
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .style(Style::default().fg(composer_text(theme)).bg(dialog_bg)),
        list_area,
    );
}

fn process_line(
    process: &ProcessInfo,
    selected: bool,
    width: usize,
    theme: Theme,
) -> Line<'static> {
    let marker = if selected { "▸" } else { " " };
    let (tag, color) = match process.status {
        ProcessStatus::Running => ("run ", composer_success(theme)),
        ProcessStatus::Exited => ("exit", composer_muted(theme)),
        ProcessStatus::Killed => ("kill", composer_warn(theme)),
        ProcessStatus::Failed => ("fail", composer_danger(theme)),
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
            .bg(composer_selection(theme))
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
fn risk_level_color(level: &str, theme: Theme) -> Color {
    match level.to_ascii_uppercase().as_str() {
        "LOW" => composer_success(theme),
        "MEDIUM" => composer_warn(theme),
        "HIGH" | "CRITICAL" => composer_danger(theme),
        _ => composer_warn(theme),
    }
}

fn approval_risk(tool: &str, theme: Theme) -> (&'static str, Color) {
    match tool {
        "exec_command" => ("shell", composer_danger(theme)),
        "apply_patch" => ("workspace-write", composer_warn(theme)),
        "spawn_agent" => ("agent", composer_accent(theme)),
        _ => ("read", composer_success(theme)),
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

impl App {
    fn draw_approval_card(&mut self, frame: &mut Frame, area: Rect, session_id: &str) {
        let theme = self.theme;
        let dialog_bg = dialog_background(theme);
        if area.width < 24 || area.height < 6 {
            return;
        }
        if !self.approvals.contains_key(session_id) {
            return;
        }
        self.ensure_diff_rows(session_id);
        let Some(approval) = self.approvals.get(session_id) else {
            return;
        };
        let rows = self
            .diff_rows
            .as_ref()
            .map(|cache| cache.rows.as_slice())
            .unwrap_or(&[]);
        let file_change = transcript::is_file_change_tool(&approval.tool);
        let actions = approval.actions();
        let cursor = approval.cursor.min(actions.len().saturating_sub(1));
        let inner = area.width.saturating_sub(4) as usize;
        let side_by_side = (!rows.is_empty())
            .then(|| {
                super::diff::DiffRenderer::new(
                    rows,
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
            approval.diff.as_deref().unwrap_or("").lines().count()
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
            Some((level, body)) => (level, risk_level_color(level, theme), body),
            None => {
                let (risk, color) = approval_risk(&approval.tool, theme);
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
        // vimdiff-style view for apply_patch. Other tools keep the
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
                        .fg(composer_warn(theme))
                        .add_modifier(Modifier::BOLD),
                ));
                spans.push(Span::styled(
                    summary,
                    Style::default().fg(composer_warn(theme)),
                ));
                spans.push(Span::styled(
                    risk_label.clone(),
                    Style::default().fg(risk_color),
                ));
            } else {
                spans.push(Span::raw("  "));
                spans.push(Span::styled(
                    summary,
                    Style::default().fg(composer_warn(theme)),
                ));
            }
            lines.push(Line::from(spans));
        }
        if let Some(mut renderer) = side_by_side {
            let cache_hit = self.card_diff.as_ref().is_some_and(|cache| {
                cache.rev == self.approval_rev
                    && cache.session == session_id
                    && cache.width == inner
                    && cache.theme == theme
                    && cache.shown == shown
            });
            if !cache_hit {
                let rendered = renderer.render(&rows[..rows.len().min(shown)], false, &[], 0);
                self.card_diff = Some(CardDiffCache {
                    rev: self.approval_rev,
                    session: session_id.to_string(),
                    width: inner,
                    theme,
                    shown,
                    lines: rendered,
                });
            }
            if let Some(cache) = self.card_diff.as_ref() {
                lines.extend(cache.lines.iter().cloned());
            }
        } else {
            for line in approval.diff.as_deref().unwrap_or("").lines().take(shown) {
                lines.push(Line::from(Span::styled(
                    truncate(line, inner),
                    diff_line_style(line, theme),
                )));
            }
        }
        if truncated {
            lines.push(Line::from(Span::styled(
                "… diff truncated",
                Style::default().fg(composer_muted(theme)),
            )));
        }
        let selected_bg = composer_warn(theme);
        let selected_style = Style::default()
            .fg(badge_text_color(selected_bg))
            .bg(selected_bg)
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
                Style::default().fg(approval_action_color(*action, theme))
            };
            lines.push(Line::from(Span::styled(pad_to(&text, inner), style)));
        }
        lines.push(Line::from(Span::styled(
            truncate(
                "Ctrl-J/K or j/k select · Enter confirm · 1-9 jump · y/n · x cancel",
                inner,
            ),
            Style::default().fg(composer_muted(theme)),
        )));

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(composer_warn(theme)))
            .title(format!(" approval · {} ", approval.tool));
        frame.render_widget(
            Paragraph::new(lines)
                .block(block)
                .style(Style::default().bg(dialog_bg)),
            rect,
        );
    }
}

fn approval_action_color(action: ApprovalAction, theme: Theme) -> Color {
    match action {
        ApprovalAction::Approve | ApprovalAction::Always => composer_success(theme),
        ApprovalAction::Deny | ApprovalAction::Reason => composer_danger(theme),
        ApprovalAction::Review | ApprovalAction::Open | ApprovalAction::Edit => {
            composer_accent(theme)
        }
    }
}

/// Render an `ask_user` question as a keyboard-driven option picker.
fn draw_input_card(frame: &mut Frame, area: Rect, pending: &PendingInput, theme: Theme) {
    let dialog_bg = dialog_background(theme);
    if area.width < 24 || area.height < 6 {
        return;
    }
    let tab = pending.tab.min(pending.questions.len().saturating_sub(1));
    let Some(question) = pending.questions.get(tab) else {
        return;
    };
    let rows = question.options.len() + 1;
    // Borders + tabs + prompt + rows + help.
    let height = (rows + 5)
        .min(area.height.saturating_sub(1) as usize)
        .max(5) as u16;
    let rect = Rect {
        x: area.x + 1,
        y: area.y + area.height - height - 1,
        width: area.width.saturating_sub(2),
        height,
    };
    frame.render_widget(Clear, rect);
    frame.render_widget(Block::default().style(Style::default().bg(dialog_bg)), rect);
    let inner = rect.width.saturating_sub(2) as usize;
    let cursor = pending.cursors[tab].min(rows.saturating_sub(1));
    let selected_bg = composer_accent(theme);
    let selected_style = Style::default()
        .fg(badge_text_color(selected_bg))
        .bg(selected_bg)
        .add_modifier(Modifier::BOLD);

    // One tab per question, marked with the answer state so a multi-question
    // request shows progress at a glance.
    let mut tabs = Vec::new();
    for (index, item) in pending.questions.iter().enumerate() {
        let mark = match &pending.answers[index] {
            Some(_) => "✓",
            None => "·",
        };
        let label = format!(" {mark} {} ", truncate(&item.header, 16));
        let style = if index == tab {
            selected_style
        } else if pending.answers[index].is_some() {
            Style::default().fg(composer_success(theme))
        } else {
            Style::default().fg(composer_muted(theme))
        };
        tabs.push(Span::styled(label, style));
    }
    let mut lines = vec![
        Line::from(tabs),
        Line::from(vec![
            Span::styled(
                "? ",
                Style::default()
                    .fg(composer_accent(theme))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                truncate(&question.question, inner.saturating_sub(2)),
                Style::default().fg(composer_warn(theme)),
            ),
        ]),
    ];
    // Small panes clip the card; keep the highlighted row visible instead of
    // always rendering the first N rows.
    let visible = (height as usize).saturating_sub(5).max(1);
    let start = cursor.saturating_sub(visible.saturating_sub(1));
    for (index, option) in question
        .options
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
    {
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
            Style::default().fg(composer_success(theme))
        } else {
            Style::default()
        };
        lines.push(Line::from(Span::styled(pad_to(&text, inner), style)));
    }
    // The synthetic free-form row is always last.
    let other_index = question.options.len();
    if other_index >= start && other_index < start + visible {
        let editing = pending.editing && other_index == cursor;
        let value = pending.other[tab].trim();
        let mut text = if value.is_empty() {
            "- [ ] Other…".to_string()
        } else {
            format!("- [ ] Other: {value}")
        };
        if editing {
            text.push('▌');
        }
        let text = truncate(&text, inner);
        let style = if other_index == cursor {
            selected_style
        } else {
            Style::default().fg(composer_muted(theme))
        };
        lines.push(Line::from(Span::styled(pad_to(&text, inner), style)));
    }
    lines.push(Line::from(Span::styled(
        truncate(
            if pending.editing {
                "type answer · Enter confirm · Esc cancel"
            } else {
                "Tab switch · j/k select · Enter answer · 1-3 jump · x cancel"
            },
            inner,
        ),
        Style::default().fg(composer_muted(theme)),
    )));

    let title = if pending.questions.len() > 1 {
        format!(" questions {}/{} ", tab + 1, pending.questions.len())
    } else {
        " question ".to_string()
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(composer_accent(theme)))
        .title(title);
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
        Style::default().fg(theme.function.unwrap_or(COMPOSER_ACCENT))
    } else if line.starts_with("+++") || line.starts_with("---") {
        Style::default().fg(theme.comment.unwrap_or(COMPOSER_MUTED))
    } else if line.starts_with('+') {
        Style::default().fg(theme.diff_add.unwrap_or(COMPOSER_SUCCESS))
    } else if line.starts_with('-') {
        Style::default().fg(theme.diff_delete.unwrap_or(COMPOSER_DANGER))
    } else {
        Style::default().fg(theme.normal.unwrap_or(COMPOSER_TEXT))
    }
}

/// Draw the query row and separator shared by the theme picker and help/keys.
/// Returns the remaining area for results.
fn draw_search_field(
    frame: &mut Frame,
    area: Rect,
    query: &str,
    accent: Color,
    text: Color,
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
            Span::styled(query.to_string(), Style::default().fg(text)),
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

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

/// A prompt run, highlighted while the visual selection covers it.
fn selection_span(text: String, selected: bool, theme: Theme) -> Span<'static> {
    if selected {
        Span::styled(
            text,
            Style::default()
                .fg(composer_text(theme))
                .bg(composer_selection(theme)),
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

/// Clickable footer shown at the end of a child agent's transcript.
fn parent_link_line(theme: Theme) -> Line<'static> {
    let muted = composer_muted(theme);
    Line::from(vec![
        Span::styled("  ← ", Style::default().fg(muted)),
        Span::styled(
            "parent agent",
            Style::default()
                .fg(composer_accent(theme))
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" · click to return", Style::default().fg(muted)),
    ])
}

/// Whether a transcript line is the clickable parent link.
fn is_parent_link(text: &str) -> bool {
    text.trim_start().starts_with("← parent agent")
}

/// Picker marker column before the session id: `⑂` for worktrees.
/// Subagent sessions are reached from their parent transcript and are not
/// listed in the session picker.
fn origin_marker(session: &Session) -> &'static str {
    if session.has_worktree() {
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

/// Quiet borders for unfocused panes and the blurred prompt. Presets use their
/// own muted tone; the transparent default stays a low-contrast slate.
fn composer_inactive_border(theme: Theme) -> Color {
    theme.comment.unwrap_or(COMPOSER_INACTIVE_BORDER)
}

/// Selected rows get a low-alpha accent tint so both light and dark presets
/// stay readable. The default transparent palette keeps its fixed slate.
fn composer_selection(theme: Theme) -> Color {
    let Some(base) = theme.panel.or(theme.background) else {
        return COMPOSER_SELECTION;
    };
    blend_color(base, composer_accent(theme), 0.22)
}

fn blend_color(base: Color, tint: Color, amount: f32) -> Color {
    let (Color::Rgb(br, bg, bb), Color::Rgb(tr, tg, tb)) = (base, tint) else {
        return base;
    };
    let mix = |from: u8, to: u8| {
        (f32::from(from) + (f32::from(to) - f32::from(from)) * amount).round() as u8
    };
    Color::Rgb(mix(br, tr), mix(bg, tg), mix(bb, tb))
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
    let color = match mode {
        SessionMode::Plan => composer_warn(theme),
        SessionMode::Build => composer_accent(theme),
        SessionMode::Auto => COMPOSER_ORANGE,
        SessionMode::Bypass => composer_danger(theme),
    };
    readable_mode_color(color, theme)
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

/// Pick dark or light badge text using WCAG contrast. ANSI colors keep the
/// default dark text because their RGB values depend on the terminal palette.
fn badge_text_color(background: Color) -> Color {
    let Color::Rgb(red, green, blue) = background else {
        return COMPOSER_BADGE_TEXT;
    };
    let background = relative_luminance(red, green, blue);
    let dark = relative_luminance(20, 22, 28);
    let light = relative_luminance(248, 250, 252);
    if contrast_ratio(background, light) > contrast_ratio(background, dark) {
        COMPOSER_BADGE_TEXT_LIGHT
    } else {
        COMPOSER_BADGE_TEXT
    }
}

fn relative_luminance(red: u8, green: u8, blue: u8) -> f32 {
    fn channel(value: u8) -> f32 {
        let value = value as f32 / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    }
    0.2126 * channel(red) + 0.7152 * channel(green) + 0.0722 * channel(blue)
}

fn contrast_ratio(first: f32, second: f32) -> f32 {
    let (lighter, darker) = if first > second {
        (first, second)
    } else {
        (second, first)
    };
    (lighter + 0.05) / (darker + 0.05)
}

/// Darken accents on light backgrounds (and lighten them on dark ones) until
/// they reach the minimum statusline contrast. Presets keep their hue; custom
/// themes without an explicit background are left untouched.
fn readable_mode_color(color: Color, theme: Theme) -> Color {
    let Some(Color::Rgb(background_red, background_green, background_blue)) = theme.background
    else {
        return color;
    };
    let Color::Rgb(red, green, blue) = color else {
        return color;
    };
    let background = relative_luminance(background_red, background_green, background_blue);
    let light = background > 0.6;
    let mut channels = [red, green, blue];
    for _ in 0..64 {
        if contrast_ratio(
            relative_luminance(channels[0], channels[1], channels[2]),
            background,
        ) >= MIN_ACCENT_CONTRAST
        {
            break;
        }
        for channel in &mut channels {
            *channel = if light {
                (*channel as f32 * 0.92) as u8
            } else {
                (*channel as f32 + (255.0 - *channel as f32) * 0.08) as u8
            };
        }
    }
    Color::Rgb(channels[0], channels[1], channels[2])
}

fn composer_mode_style(mode: Mode, focused: bool, theme: Theme) -> Style {
    let (color, foreground) = if focused {
        let color = composer_mode_color(mode, theme);
        (color, badge_text_color(color))
    } else {
        // Blurred badges keep deliberately low contrast.
        (composer_border(theme), COMPOSER_BADGE_TEXT)
    };
    Style::default()
        .fg(foreground)
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
