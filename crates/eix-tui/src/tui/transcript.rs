//! Chat-style transcript rendering.
//!
//! The transcript is a conversation, not a log: user turns sit on the right in
//! a rounded bubble, while agent output stays on the left. The agent's chain
//! of thought and every tool invocation render as compact headers that expand
//! into their details, and the same line map powers click-to-fold. Blocks still
//! streaming live under a synthetic [`FoldId::Live`] key so they collapse once
//! the turn is stored.

use std::collections::HashMap;

use eix_core::{
    ApprovalRequiredPayload, AssistantMessagePayload, ContextCompactedPayload,
    Event as SessionEvent, EventKind, InputRequiredPayload, PatchProposedPayload,
    PatchResultPayload, ToolCallPayload, ToolResultPayload, UserMessagePayload,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde::Deserialize;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use super::app::{tool_call_summary, truncate, wrap_text};
use super::markdown;
use super::theme::Theme;

/// Tool output is capped so a verbose command cannot flood the transcript.
/// File-change diffs are not capped: they stay open by default so a write is
/// never silently folded away, and the daemon already bounds their size.
const OUTPUT_ROWS: usize = 40;

/// Identity of a foldable block. Stored blocks use the `seq` of their event so
/// the fold survives reloads; the in-flight turn uses [`FoldId::Live`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FoldId {
    Event(u64),
    Live,
}

/// Streaming state of the focused turn, borrowed for one render.
pub struct Live<'a> {
    pub reasoning: &'a str,
    pub text: &'a str,
}

/// Runtime state of a child agent spawned by `spawn_agent`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubagentStatus {
    Running,
    Completed,
    Failed,
}

/// A child agent associated with one `spawn_agent` tool call in its parent.
///
/// The child has its own session and event stream. Keeping those events here
/// lets the parent transcript show a compact, live activity window without
/// opening a second pane for the child.
#[derive(Debug, Clone)]
pub struct Subagent {
    pub call_id: String,
    pub name: String,
    pub status: SubagentStatus,
    pub events: Vec<SessionEvent>,
    /// The child's latest assistant answer.
    pub result: Option<String>,
    pub error: Option<String>,
    /// Thinking streamed by the child's current turn. Stored assistant events
    /// carry the same text, so `push_event` clears this buffer to avoid
    /// showing it twice in the activity feed.
    pub reasoning: String,
    /// Turn that owns [`Subagent::reasoning`]; a new turn starts a new buffer.
    pub reasoning_turn: String,
    /// Answer text streamed by the child's current turn, shown live in the
    /// sub-agent picker. The stored assistant event clears it.
    pub text: String,
    /// Turn that owns [`Subagent::text`].
    pub text_turn: String,
}

impl Subagent {
    pub fn new(call_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            call_id: call_id.into(),
            name: name.into(),
            status: SubagentStatus::Running,
            events: Vec::new(),
            result: None,
            error: None,
            reasoning: String::new(),
            reasoning_turn: String::new(),
            text: String::new(),
            text_turn: String::new(),
        }
    }

    /// Keep the latest assistant text as the result, and remember errors for
    /// the completed card. Activity lines are derived from `events` at render
    /// time so this stays cheap and replay-friendly.
    pub fn push_event(&mut self, event: SessionEvent) {
        match event.kind {
            EventKind::AssistantMessage => {
                if let Ok(payload) = AssistantMessagePayload::deserialize(&event.payload) {
                    // The stored message already carries this turn's
                    // reasoning; drop the live copies so the feed does not
                    // repeat them.
                    self.reasoning.clear();
                    self.reasoning_turn.clear();
                    self.text.clear();
                    self.text_turn.clear();
                    let text = payload.text.trim();
                    if !text.is_empty() {
                        self.result = Some(text.to_string());
                    }
                }
            }
            EventKind::Error => {
                if let Some(message) = event.payload.get("message").and_then(Value::as_str) {
                    self.error = Some(message.to_string());
                }
            }
            _ => {}
        }
        self.events.push(event);
    }

    pub fn complete(&mut self, ok: bool, result: Option<String>, error: Option<String>) {
        self.reasoning.clear();
        self.reasoning_turn.clear();
        self.text.clear();
        self.text_turn.clear();
        self.status = if ok {
            SubagentStatus::Completed
        } else {
            SubagentStatus::Failed
        };
        if ok {
            // A retried turn may have left an intermediate error behind.
            self.error = None;
        }
        if let Some(result) = result.filter(|text| !text.trim().is_empty()) {
            self.result = Some(result);
        }
        if let Some(error) = error.filter(|text| !text.trim().is_empty()) {
            self.error = Some(error);
        }
    }
}

/// One rendered line plus the block that owns it (for click-to-fold).
pub struct Transcript {
    pub lines: Vec<Line<'static>>,
    pub slots: Vec<Option<Fold>>,
    /// Stored user-message sequence -> first rendered line.
    pub user_lines: Vec<(u64, usize)>,
}

/// Fold target for a line: which block, and whether it is currently open.
#[derive(Debug, Clone, Copy)]
pub struct Fold {
    pub id: FoldId,
    pub open: bool,
}

/// A tool invocation assembled from its call/approval/result/patch events.
struct ToolBlock {
    /// `seq` of the `tool_call` event (or the first event seen for this call).
    seq: u64,
    call_id: String,
    tool: String,
    arguments: Value,
    result: Option<ToolResultPayload>,
    approval: Option<ApprovalRequiredPayload>,
    patch: Option<PatchBlock>,
}

struct PatchBlock {
    path: String,
    diff: Option<String>,
    /// None while the patch is only staged.
    outcome: Option<PatchOutcome>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PatchOutcome {
    Applied,
    Rejected,
    Conflict,
}

/// Intermediate representation, built in one pass before rendering.
enum Block {
    User {
        seq: u64,
        text: String,
        images: Vec<String>,
    },
    Assistant {
        seq: u64,
        text: String,
        reasoning: Option<String>,
        cancelled: bool,
    },
    Tool(Box<ToolBlock>),
    Note {
        text: String,
    },
    /// A manual `/compact` summary. Unlike an automatic compaction, the
    /// generated summary is user-visible so the result of the command is not
    /// hidden behind a one-line status note.
    Compaction {
        messages: usize,
        summary: String,
    },
    Error {
        text: String,
    },
}

/// Render a session's events (plus any in-flight deltas) together with any
/// child-agent activity known to the UI. Test callers use the same path as the
/// live transcript.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn render_with_subagents(
    events: &[SessionEvent],
    live: Option<Live<'_>>,
    queued: &[String],
    folds: &HashMap<FoldId, bool>,
    width: usize,
    theme: Theme,
    tick: u64,
    subagents: &HashMap<String, Subagent>,
) -> Transcript {
    let tail_width = width.saturating_sub(2).max(1);
    let mut out = render_history_with_subagents(events, folds, width, theme, subagents);
    append_tail(&mut out, live, queued, folds, tail_width, theme, tick);
    out
}

/// Render a standalone event log, used by the sub-agent picker preview. The
/// child has no nested `spawn_agent` cards, so the subagent map is empty.
pub fn render_events(events: &[SessionEvent], width: usize, theme: Theme) -> Transcript {
    let width = width.saturating_sub(2).max(1);
    let mut blocks = Vec::new();
    let mut tool_index = Vec::new();
    collect_into(events, &mut blocks, &mut tool_index);
    let no_folds = HashMap::new();
    let no_subagents = HashMap::new();
    let mut lines = Vec::new();
    let mut slots = Vec::new();
    let mut user_lines = Vec::new();
    for collected in blocks {
        let piece = render_block(&collected.block, &no_folds, width, theme, &no_subagents);
        append_piece_parts(&mut lines, &mut slots, &mut user_lines, piece);
    }
    Transcript {
        lines,
        slots,
        user_lines,
    }
}

/// Render only the stored history. The UI caches this and renders the live
/// tail separately, so streaming deltas do not reparse the whole session.
#[cfg(test)]
pub fn render_history_with_subagents(
    events: &[SessionEvent],
    folds: &HashMap<FoldId, bool>,
    width: usize,
    theme: Theme,
    subagents: &HashMap<String, Subagent>,
) -> Transcript {
    // Keep a two-cell gutter on the right so queued prompts never touch the
    // pane border.
    let width = width.saturating_sub(2).max(1);
    render_blocks(collect(events), folds, width, theme, subagents)
}

/// Render the in-flight turn and queued prompts. This is cheap to call on
/// every frame because it only looks at text that is still changing.
pub fn render_tail(
    live: Option<Live<'_>>,
    queued: &[String],
    folds: &HashMap<FoldId, bool>,
    width: usize,
    theme: Theme,
    tick: u64,
) -> Transcript {
    let width = width.saturating_sub(2).max(1);
    let mut out = Transcript {
        lines: Vec::new(),
        slots: Vec::new(),
        user_lines: Vec::new(),
    };
    append_tail(&mut out, live, queued, folds, width, theme, tick);
    out
}

#[cfg(test)]
fn render_blocks(
    blocks: Vec<Collected>,
    folds: &HashMap<FoldId, bool>,
    width: usize,
    theme: Theme,
    subagents: &HashMap<String, Subagent>,
) -> Transcript {
    let mut out = Transcript::new();
    for collected in blocks {
        let piece = render_block(&collected.block, folds, width, theme, subagents);
        append_piece(&mut out, piece);
    }
    out
}

/// Render one block on its own. Keeping blocks independent is what lets the
/// UI re-render just the block whose fold state or content changed.
fn render_block(
    block: &Block,
    folds: &HashMap<FoldId, bool>,
    width: usize,
    theme: Theme,
    subagents: &HashMap<String, Subagent>,
) -> Transcript {
    let mut out = Transcript::new();
    match block {
        Block::User { seq, text, images } => {
            let line = out.lines.len();
            user_message(&mut out, text, images, width, theme);
            out.user_lines.push((*seq, line));
        }
        Block::Assistant {
            seq,
            text,
            reasoning,
            cancelled,
        } => {
            if let Some(reasoning) = reasoning.as_deref().filter(|r| !r.trim().is_empty()) {
                thinking(
                    &mut out,
                    FoldId::Event(*seq),
                    reasoning,
                    false,
                    width,
                    theme,
                    folds,
                );
            }
            if !text.is_empty() {
                assistant_message(&mut out, text, *cancelled, width, theme);
            }
        }
        Block::Tool(block) => {
            let subagent = (block.tool == "spawn_agent")
                .then(|| {
                    subagents
                        .values()
                        .find(|subagent| subagent.call_id == block.call_id)
                })
                .flatten();
            match subagent {
                Some(subagent) => subagent_block(&mut out, block, subagent, width, theme, folds),
                None => tool_block(&mut out, block, width, theme, folds),
            }
        }
        Block::Note { text } => note(&mut out, '◇', text, comment(theme), width),
        Block::Compaction { messages, summary } => {
            compaction_summary(&mut out, *messages, summary, width, theme)
        }
        Block::Error { text } => note(&mut out, '!', text, theme.error.unwrap_or(DANGER), width),
    }
    out
}

/// Join one rendered block onto another, collapsing the blank line that both
/// sides may contribute. Rendering blocks separately must not change spacing.
fn append_piece_parts(
    lines: &mut Vec<Line<'static>>,
    slots: &mut Vec<Option<Fold>>,
    user_lines: &mut Vec<(u64, usize)>,
    piece: Transcript,
) {
    let skip = usize::from(
        lines.last().is_some_and(|line| line.width() == 0)
            && piece.lines.first().is_some_and(|line| line.width() == 0),
    );
    let base = lines.len();
    lines.extend(piece.lines.into_iter().skip(skip));
    slots.extend(piece.slots.into_iter().skip(skip));
    for (seq, line) in piece.user_lines {
        user_lines.push((seq, base + line.saturating_sub(skip)));
    }
}

#[cfg(test)]
fn append_piece(dst: &mut Transcript, piece: Transcript) {
    let mut lines = std::mem::take(&mut dst.lines);
    let mut slots = std::mem::take(&mut dst.slots);
    let mut user_lines = std::mem::take(&mut dst.user_lines);
    append_piece_parts(&mut lines, &mut slots, &mut user_lines, piece);
    dst.lines = lines;
    dst.slots = slots;
    dst.user_lines = user_lines;
}

/// Identity of a block's rendered output. `rev` changes whenever the block's
/// source events change, so an unchanged block is never re-parsed or
/// re-rendered; only the block whose fold state flipped is.
#[derive(Clone, Copy, PartialEq, Eq)]
struct BlockKey {
    rev: u64,
    fold: Option<bool>,
    subagents_rev: u64,
}

fn block_key(
    block: &Block,
    rev: u64,
    folds: &HashMap<FoldId, bool>,
    subagents_rev: u64,
) -> BlockKey {
    let (seq, foldable) = match block {
        Block::User { seq, .. } => (*seq, false),
        Block::Assistant { seq, reasoning, .. } => (
            *seq,
            reasoning
                .as_deref()
                .is_some_and(|text| !text.trim().is_empty()),
        ),
        Block::Tool(tool) => (tool.seq, true),
        _ => (0, false),
    };
    BlockKey {
        rev,
        fold: foldable
            .then(|| folds.get(&FoldId::Event(seq)).copied())
            .flatten(),
        subagents_rev: match block {
            Block::Tool(tool) if tool.tool == "spawn_agent" => subagents_rev,
            _ => 0,
        },
    }
}

/// Cached history rendering. New events only invalidate the blocks they touch;
/// fold toggles only invalidate the toggled block, and its suffix is repaired
/// by re-rendering from that block onward.
#[derive(Default)]
pub struct HistoryCache {
    width: usize,
    theme: Theme,
    events_rev: u64,
    subagents_rev: u64,
    folds: HashMap<FoldId, bool>,
    keys: Vec<BlockKey>,
    starts: Vec<usize>,
    /// Collected blocks, kept between renders so appended events only pay for
    /// themselves instead of re-deserializing the whole session.
    blocks: Vec<Collected>,
    tool_index: Vec<(String, usize)>,
    consumed: usize,
    first_seq: Option<u64>,
    last_seq: Option<u64>,
    pub lines: Vec<Line<'static>>,
    pub slots: Vec<Option<Fold>>,
    pub user_lines: Vec<(u64, usize)>,
}

impl HistoryCache {
    pub fn needs_render(
        &self,
        width: usize,
        theme: Theme,
        events_rev: u64,
        subagents_rev: u64,
        folds: &HashMap<FoldId, bool>,
    ) -> bool {
        self.width == 0
            || self.width != width
            || self.theme != theme
            || self.events_rev != events_rev
            || self.subagents_rev != subagents_rev
            || self.folds != *folds
    }

    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        events: &[SessionEvent],
        folds: &HashMap<FoldId, bool>,
        width: usize,
        theme: Theme,
        subagents: &HashMap<String, Subagent>,
        subagents_rev: u64,
        events_rev: u64,
    ) {
        // Reuse collected blocks when the event log only grew. A history page
        // prepended at the front or a fork/truncation changes the prefix, so
        // those fall back to a full collect.
        let appendable = self.consumed <= events.len()
            && self.consumed > 0
            && events.first().map(|event| event.seq) == self.first_seq
            && events.get(self.consumed - 1).map(|event| event.seq) == self.last_seq;
        if !appendable {
            self.blocks.clear();
            self.tool_index.clear();
            collect_into(events, &mut self.blocks, &mut self.tool_index);
            self.consumed = events.len();
        } else if self.consumed < events.len() {
            collect_into(
                &events[self.consumed..],
                &mut self.blocks,
                &mut self.tool_index,
            );
            self.consumed = events.len();
        }
        self.first_seq = events.first().map(|event| event.seq);
        self.last_seq = events.last().map(|event| event.seq);
        let keys: Vec<BlockKey> = self
            .blocks
            .iter()
            .map(|collected| block_key(&collected.block, collected.rev, folds, subagents_rev))
            .collect();
        let mut common = if width != self.width || theme != self.theme {
            0
        } else {
            let mut common = 0;
            while common < self.keys.len()
                && common < keys.len()
                && self.keys[common] == keys[common]
            {
                common += 1;
            }
            common
        };
        // A width/theme change invalidates every block; otherwise reuse the
        // matching prefix and repair only the suffix.
        if common > keys.len() {
            common = keys.len();
        }
        if common < self.keys.len() {
            let cut = self.starts.get(common).copied().unwrap_or(self.lines.len());
            self.lines.truncate(cut);
            self.slots.truncate(cut);
            self.user_lines.retain(|(_, line)| *line < cut);
            self.keys.truncate(common);
            self.starts.truncate(common);
        }
        for (index, collected) in self.blocks.iter().enumerate().skip(common) {
            self.starts.push(self.lines.len());
            self.keys.push(keys[index]);
            let piece = render_block(&collected.block, folds, width, theme, subagents);
            append_piece_parts(
                &mut self.lines,
                &mut self.slots,
                &mut self.user_lines,
                piece,
            );
        }
        self.width = width;
        self.theme = theme;
        self.events_rev = events_rev;
        self.subagents_rev = subagents_rev;
        self.folds = folds.clone();
    }
}

/// One dot phase every other 200ms tick, so the placeholder cycles `.`, `..`,
/// `...` at a readable pace instead of flickering on every redraw.
const WAITING_DOT_TICKS: u64 = 2;

pub(crate) fn waiting_dots(tick: u64) -> &'static str {
    match (tick / WAITING_DOT_TICKS) % 3 {
        0 => ".",
        1 => "..",
        _ => "...",
    }
}

fn append_tail(
    out: &mut Transcript,
    live: Option<Live<'_>>,
    queued: &[String],
    folds: &HashMap<FoldId, bool>,
    width: usize,
    theme: Theme,
    tick: u64,
) {
    if let Some(live) = live {
        if !live.reasoning.is_empty() {
            thinking(out, FoldId::Live, live.reasoning, true, width, theme, folds);
        }
        if !live.text.is_empty() {
            assistant_message(out, live.text, false, width, theme);
        } else if live.reasoning.is_empty() {
            assistant_message(out, waiting_dots(tick), false, width, theme);
        }
    }
    if !queued.is_empty() {
        out.blank();
        for text in queued {
            queued_prompt(out, text, width, theme);
        }
    }
}

/// One collected block plus the event sequence that last changed it. The
/// sequence is unique per block, so it doubles as a cache key for rendering.
struct Collected {
    block: Block,
    rev: u64,
}

/// Flatten the whole event log. Used by tests; the UI appends incrementally.
#[cfg(test)]
fn collect(events: &[SessionEvent]) -> Vec<Collected> {
    let mut blocks = Vec::new();
    let mut tool_index = Vec::new();
    collect_into(events, &mut blocks, &mut tool_index);
    blocks
}

/// Apply a slice of events to an existing block list, merging tool
/// call/results. Appending to a cache avoids re-deserializing the whole log for
/// every new event.
fn collect_into(
    events: &[SessionEvent],
    blocks: &mut Vec<Collected>,
    tool_index: &mut Vec<(String, usize)>,
) {
    macro_rules! push_block {
        ($rev:expr, $block:expr) => {
            blocks.push(Collected {
                block: $block,
                rev: $rev,
            })
        };
    }

    for event in events {
        match event.kind {
            EventKind::UserMessage => {
                if let Ok(payload) = UserMessagePayload::deserialize(&event.payload) {
                    push_block!(
                        event.seq,
                        Block::User {
                            seq: event.seq,
                            text: payload.text,
                            images: payload.images.into_iter().map(|image| image.path).collect(),
                        }
                    );
                }
            }
            EventKind::AssistantMessage => {
                if let Ok(payload) = AssistantMessagePayload::deserialize(&event.payload) {
                    push_block!(
                        event.seq,
                        Block::Assistant {
                            seq: event.seq,
                            text: payload.text,
                            reasoning: payload.reasoning,
                            cancelled: payload.cancelled,
                        }
                    );
                }
            }
            EventKind::ToolCall => {
                if let Ok(payload) = ToolCallPayload::deserialize(&event.payload) {
                    let slot = blocks.len();
                    tool_index.push((payload.call_id.clone(), slot));
                    push_block!(
                        event.seq,
                        Block::Tool(Box::new(ToolBlock {
                            seq: event.seq,
                            call_id: payload.call_id,
                            tool: payload.tool,
                            arguments: payload.arguments,
                            result: None,
                            approval: None,
                            patch: None,
                        }))
                    );
                }
            }
            EventKind::ApprovalRequired => {
                if let Ok(payload) = ApprovalRequiredPayload::deserialize(&event.payload) {
                    match tool_slot(blocks, tool_index, &payload.call_id) {
                        Some(slot) => {
                            blocks[slot].rev = event.seq;
                            if let Block::Tool(block) = &mut blocks[slot].block {
                                block.approval = Some(payload);
                            }
                        }
                        None => {
                            let call_id = payload.call_id.clone();
                            let arguments = payload.arguments.clone();
                            let tool = payload.tool.clone();
                            push_block!(
                                event.seq,
                                Block::Tool(Box::new(ToolBlock {
                                    seq: event.seq,
                                    call_id,
                                    tool,
                                    arguments,
                                    result: None,
                                    approval: Some(payload),
                                    patch: None,
                                }))
                            )
                        }
                    }
                }
            }
            EventKind::PatchProposed => {
                if let Ok(payload) = PatchProposedPayload::deserialize(&event.payload) {
                    let patch = PatchBlock {
                        path: payload.path,
                        diff: payload.diff,
                        outcome: None,
                    };
                    match tool_slot(blocks, tool_index, &payload.call_id) {
                        Some(slot) => {
                            blocks[slot].rev = event.seq;
                            if let Block::Tool(block) = &mut blocks[slot].block {
                                block.patch = Some(patch);
                            }
                        }
                        None => push_block!(
                            event.seq,
                            Block::Tool(Box::new(ToolBlock {
                                seq: event.seq,
                                call_id: payload.call_id,
                                tool: payload.tool,
                                arguments: Value::Null,
                                result: None,
                                approval: None,
                                patch: Some(patch),
                            }))
                        ),
                    }
                }
            }
            EventKind::PatchApplied | EventKind::PatchRejected | EventKind::PatchConflict => {
                if let Ok(payload) = PatchResultPayload::deserialize(&event.payload) {
                    let outcome = match event.kind {
                        EventKind::PatchApplied => PatchOutcome::Applied,
                        EventKind::PatchRejected => PatchOutcome::Rejected,
                        _ => PatchOutcome::Conflict,
                    };
                    match tool_slot(blocks, tool_index, &payload.call_id) {
                        Some(slot) => {
                            blocks[slot].rev = event.seq;
                            if let Block::Tool(block) = &mut blocks[slot].block {
                                match &mut block.patch {
                                    Some(patch) => {
                                        patch.outcome = Some(outcome);
                                        if let Some(diff) = payload.diff.clone() {
                                            patch.diff = Some(diff);
                                        }
                                        if patch.path.is_empty() {
                                            patch.path = payload.path.clone();
                                        }
                                    }
                                    None => {
                                        block.patch = Some(PatchBlock {
                                            path: payload.path.clone(),
                                            diff: payload.diff.clone(),
                                            outcome: Some(outcome),
                                        })
                                    }
                                }
                            }
                        }
                        None => push_block!(
                            event.seq,
                            Block::Tool(Box::new(ToolBlock {
                                seq: event.seq,
                                call_id: payload.call_id,
                                tool: "apply_patch".into(),
                                arguments: Value::Null,
                                result: None,
                                approval: None,
                                patch: Some(PatchBlock {
                                    path: payload.path,
                                    diff: payload.diff,
                                    outcome: Some(outcome),
                                }),
                            }))
                        ),
                    }
                }
            }
            EventKind::ToolResult => {
                if let Ok(payload) = ToolResultPayload::deserialize(&event.payload) {
                    match tool_slot(blocks, tool_index, &payload.call_id) {
                        Some(slot) => {
                            blocks[slot].rev = event.seq;
                            if let Block::Tool(block) = &mut blocks[slot].block {
                                block.result = Some(payload);
                            }
                        }
                        None => push_block!(
                            event.seq,
                            Block::Tool(Box::new(ToolBlock {
                                seq: event.seq,
                                call_id: payload.call_id.clone(),
                                tool: payload.tool.clone(),
                                arguments: Value::Null,
                                result: Some(payload),
                                approval: None,
                                patch: None,
                            }))
                        ),
                    }
                }
            }
            // Mode changes are session metadata: keep them out of the
            // conversation transcript. The status bar and the event log still
            // expose the current mode and its audit trail.
            EventKind::Mode => {}
            EventKind::ContextCompacted => {
                if let Ok(payload) = ContextCompactedPayload::deserialize(&event.payload) {
                    let summary = payload.summary.trim();
                    // Automatic compactions happen mid-turn and roll previous
                    // summaries forward. Keep those as a short note; only a
                    // manual `/compact` should expand the generated summary in
                    // the conversation.
                    if !payload.rolling && !summary.is_empty() {
                        push_block!(
                            event.seq,
                            Block::Compaction {
                                messages: payload.messages,
                                summary: summary.to_string(),
                            }
                        );
                    } else {
                        push_block!(
                            event.seq,
                            Block::Note {
                                text: format!(
                                    "context compacted · {} message(s) replaced by a summary",
                                    payload.messages
                                ),
                            }
                        );
                    }
                }
            }
            EventKind::ContextCleared => {
                push_block!(
                    event.seq,
                    Block::Note {
                        text: "context cleared · earlier turns stay here but are no longer sent"
                            .into(),
                    }
                );
            }
            EventKind::Error => {
                push_block!(
                    event.seq,
                    Block::Error {
                        text: event
                            .payload
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown error")
                            .to_string(),
                    }
                );
            }
            EventKind::Integration => {
                if let Some(text) = event.payload.get("text").and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        push_block!(
                            event.seq,
                            Block::Note {
                                text: text.to_string(),
                            }
                        );
                    }
                }
            }
            _ => {}
        }
    }
}

fn tool_slot(blocks: &[Collected], index: &[(String, usize)], call_id: &str) -> Option<usize> {
    index
        .iter()
        .rev()
        .find(|(id, _)| id == call_id)
        .map(|(_, slot)| *slot)
        .filter(|slot| matches!(blocks.get(*slot).map(|c| &c.block), Some(Block::Tool(_))))
}

impl Transcript {
    pub(crate) fn new() -> Self {
        Self {
            lines: Vec::new(),
            slots: Vec::new(),
            user_lines: Vec::new(),
        }
    }

    fn push(&mut self, line: Line<'static>, slot: Option<Fold>) {
        self.lines.push(line);
        self.slots.push(slot);
    }

    /// One blank separator, never two in a row.
    fn blank(&mut self) {
        if self.lines.last().is_some_and(|line| line.width() == 0) {
            return;
        }
        self.push(Line::from(""), None);
    }
}

const TEXT: Color = Color::Rgb(220, 223, 228);
const MUTED: Color = Color::Rgb(110, 116, 129);
const ACCENT: Color = Color::Rgb(122, 162, 247);
const SUCCESS: Color = Color::Rgb(126, 199, 149);
const DANGER: Color = Color::Rgb(231, 130, 132);
const WARN: Color = Color::Rgb(229, 192, 123);

fn comment(theme: Theme) -> Color {
    theme.comment.unwrap_or(MUTED)
}

fn text_color(theme: Theme) -> Color {
    theme.normal.unwrap_or(TEXT)
}

fn accent(theme: Theme) -> Color {
    theme.function.unwrap_or(ACCENT)
}

fn dim(theme: Theme) -> Style {
    Style::default().fg(comment(theme))
}

fn italic_dim(theme: Theme) -> Style {
    Style::default()
        .fg(comment(theme))
        .add_modifier(Modifier::ITALIC)
}

/// Prefix spaces so a line ends at the right edge. The padding stays in the
/// spans so mouse selection and search use the same coordinates as the screen.
fn right_align(mut line: Line<'static>, width: usize) -> Line<'static> {
    let padding = width.saturating_sub(line.width());
    if padding > 0 {
        line.spans.insert(0, Span::raw(" ".repeat(padding)));
    }
    line
}

/// Horizontal padding inside the bubble's left/right borders.
const BUBBLE_PAD: usize = 1;

/// Longest content line that still leaves room for both borders and the
/// inner horizontal padding.
fn bubble_content_limit(width: usize) -> usize {
    let available = width.saturating_sub(2 * (BUBBLE_PAD + 1)).max(1);
    let preferred = (width * 3 / 4).max(8);
    preferred.min(available).max(1)
}

fn with_panel(style: Style, panel: Option<Color>) -> Style {
    match panel {
        Some(panel) => Style::default().bg(panel).patch(style),
        None => style,
    }
}

fn bubble_border_style(theme: Theme) -> Style {
    with_panel(
        Style::default().fg(theme.function.unwrap_or(ACCENT)),
        theme.panel,
    )
}

/// Draw a right-aligned rounded bubble around the rendered content. Padding
/// and borders are normal spans, so click selection, search, and horizontal
/// positioning all see exactly what is drawn.
fn push_bubble(out: &mut Transcript, mut content: Vec<Line<'static>>, width: usize, theme: Theme) {
    while content.first().is_some_and(|line| line.width() == 0) {
        content.remove(0);
    }
    while content.last().is_some_and(|line| line.width() == 0) {
        content.pop();
    }
    if content.is_empty() {
        out.blank();
        return;
    }

    let content_width = content.iter().map(Line::width).max().unwrap_or(0).max(1);
    let box_width = content_width + 2 * (BUBBLE_PAD + 1);
    let body_pad = width.saturating_sub(box_width);

    let border = bubble_border_style(theme);
    let panel = with_panel(Style::default(), theme.panel);

    let mut top = Vec::new();
    if body_pad > 0 {
        top.push(Span::raw(" ".repeat(body_pad)));
    }
    top.push(Span::styled(
        format!("╭{}╮", "─".repeat(content_width + 2 * BUBBLE_PAD)),
        border,
    ));
    out.push(Line::from(top), None);

    for line in content {
        let line_width = line.width();
        let mut spans = Vec::with_capacity(line.spans.len() + 3);
        if body_pad > 0 {
            spans.push(Span::raw(" ".repeat(body_pad)));
        }
        spans.push(Span::styled("│", border));
        spans.push(Span::styled(" ".repeat(BUBBLE_PAD), panel));
        for mut span in line.spans {
            span.style = with_panel(span.style, theme.panel);
            spans.push(span);
        }
        if line_width < content_width {
            spans.push(Span::styled(" ".repeat(content_width - line_width), panel));
        }
        spans.push(Span::styled(" ".repeat(BUBBLE_PAD), panel));
        spans.push(Span::styled("│", border));
        out.push(Line::from(spans), None);
    }

    let mut bottom = Vec::new();
    if body_pad > 0 {
        bottom.push(Span::raw(" ".repeat(body_pad)));
    }
    bottom.push(Span::styled(
        format!("╰{}╯", "─".repeat(content_width + 2 * BUBBLE_PAD)),
        border,
    ));
    out.push(Line::from(bottom), None);
}

/// A user turn: Markdown wrapped in a right-aligned chat bubble, opposite the
/// left-aligned agent output.
fn user_message(out: &mut Transcript, text: &str, images: &[String], width: usize, theme: Theme) {
    let content_width = bubble_content_limit(width);
    let mut content = Vec::new();
    for path in images {
        content.push(Line::from(Span::styled(
            format!("[image] {path}"),
            Style::default().fg(theme.comment.unwrap_or(MUTED)),
        )));
    }
    let text = text.trim_end();
    if !text.is_empty() {
        // The bubble supplies its own horizontal padding, so render without
        // the transcript's usual two-cell left gutter.
        content.extend(markdown::render_with_indent(
            text,
            content_width.max(1),
            theme,
            "",
        ));
    }
    push_bubble(out, content, width, theme);
    out.blank();
}

/// Agent output: rendered as Markdown, indented only enough to breathe, with
/// no role label.
fn assistant_message(
    out: &mut Transcript,
    text: &str,
    cancelled: bool,
    width: usize,
    theme: Theme,
) {
    let mut lines = markdown::render(text.trim_end_matches('\n'), width.max(1), theme);
    if lines.is_empty() {
        lines.push(Line::from("  "));
    }
    if cancelled {
        if let Some(last) = lines.last_mut() {
            last.spans.push(Span::styled("  · cancelled", dim(theme)));
        }
    }
    for line in lines {
        out.push(line, None);
    }
    out.blank();
}

/// A one-line status note (context changes, errors).
fn note(out: &mut Transcript, marker: char, text: &str, color: Color, width: usize) {
    let prefix = format!("  {marker} ");
    let prefix_width = UnicodeWidthStr::width(prefix.as_str());
    let style = Style::default().fg(color);
    let wrapped = wrap_text(text, width.saturating_sub(prefix_width).max(1));
    for (index, segment) in wrapped.into_iter().enumerate() {
        let lead = if index == 0 {
            Span::styled(prefix.clone(), style.add_modifier(Modifier::BOLD))
        } else {
            Span::raw(" ".repeat(prefix_width))
        };
        out.push(Line::from(vec![lead, Span::styled(segment, style)]), None);
    }
    out.blank();
}

/// A manual compaction shows both the bookkeeping line and the summary that
/// replaced the compacted messages. The summary is Markdown, like an assistant
/// message, because that is what the model generated.
fn compaction_summary(
    out: &mut Transcript,
    messages: usize,
    summary: &str,
    width: usize,
    theme: Theme,
) {
    note(
        out,
        '◇',
        &format!("context compacted · {messages} message(s) replaced by this summary"),
        comment(theme),
        width,
    );
    let indent = "    ";
    let content_width = width.saturating_sub(indent.len()).max(1);
    for line in markdown::render_with_indent(summary, content_width, theme, indent) {
        out.push(line, None);
    }
    out.blank();
}

/// The "Thinking" module: a collapsible, quietly styled header over the chain
/// of thought.
fn thinking(
    out: &mut Transcript,
    id: FoldId,
    text: &str,
    streaming: bool,
    width: usize,
    theme: Theme,
    folds: &HashMap<FoldId, bool>,
) {
    let inner = width.saturating_sub(6).max(1);
    let wrapped = wrap_text(text.trim_end(), inner);
    let lines = wrapped.len().max(1);
    // Thinking is open by default, both while streaming and after the turn is
    // stored; an explicit click can still collapse it.
    let open = folds.get(&id).copied().unwrap_or(true);
    let fold = Fold { id, open };
    let marker = if open { '▾' } else { '▸' };
    let header = if streaming {
        "  ✳ Thinking…".to_string()
    } else if lines == 1 {
        format!("  {marker} Thinking · 1 line")
    } else {
        format!("  {marker} Thinking · {lines} lines")
    };
    out.push(
        Line::from(Span::styled(header, italic_dim(theme))),
        Some(fold),
    );
    if open {
        for line in wrapped {
            out.push(
                Line::from(vec![
                    Span::raw("    "),
                    Span::styled("│ ", dim(theme)),
                    Span::styled(line, italic_dim(theme)),
                ]),
                Some(fold),
            );
        }
    }
    out.blank();
}

/// A tool invocation: a compact summary with the status on the following line,
/// expanding into args, approval diff and output. The summary already carries
/// the command/path, so the body stays quiet: four spaces, no `input`/`output`
/// labels.
fn tool_block(
    out: &mut Transcript,
    block: &ToolBlock,
    width: usize,
    theme: Theme,
    folds: &HashMap<FoldId, bool>,
) {
    let id = FoldId::Event(block.seq);
    // File-writing tools keep their vimdiff-style preview visible after the
    // call finishes. Other in-flight calls open so their arguments are visible;
    // finished ones collapse to the summary line.
    let default_open =
        is_file_change_tool(&block.tool) || (block.result.is_none() && block.approval.is_none());
    let open = folds.get(&id).copied().unwrap_or(default_open);
    let fold = Fold { id, open };

    let color = status_color(block, theme);
    let status = status_label(block);
    let prefix_width = 6; // "  ▾ ● "
    let summary = tool_summary(block);
    let room = width.saturating_sub(prefix_width + 1).max(8);
    // Expanded shell calls wrap the command instead of truncating it and then
    // repeating the full command in the body.
    let summary_lines = if open && is_command_tool(&block.tool) {
        wrap_text(&summary, room)
    } else {
        vec![truncate(&summary, room)]
    };
    let summary_style = Style::default()
        .fg(text_color(theme))
        .add_modifier(Modifier::BOLD);
    for (index, summary) in summary_lines.iter().enumerate() {
        let first = index == 0;
        let mut spans = Vec::new();
        if first {
            spans.push(Span::styled(
                format!("  {} ", if open { "▾" } else { "▸" }),
                dim(theme),
            ));
            spans.push(Span::styled(
                format!("{} ", status_dot(block)),
                Style::default().fg(color),
            ));
        } else {
            // Continuation lines align under the summary column (the `$`).
            spans.push(Span::raw(" ".repeat(prefix_width)));
        }
        spans.push(Span::styled(summary.clone(), summary_style));
        out.push(Line::from(spans), Some(fold));
    }

    // Keep statuses under the command instead of right-aligning them: user
    // turns also live on the right, so a second right edge looks noisy.
    out.push(
        Line::from(vec![
            Span::raw(" ".repeat(prefix_width)),
            Span::styled(status, Style::default().fg(color)),
        ]),
        Some(fold),
    );

    if open {
        // `push_*` subtracts `BODY_INDENT` itself, so pass the full line width.
        push_body(out, &detail_text(block), width, theme, fold);
        let patch_diff = block.patch.as_ref().and_then(|patch| patch.diff.as_deref());
        if let Some(approval) = &block.approval {
            push_body(out, &approval.summary, width, theme, fold);
            // A rendered patch result carries the bytes that actually landed
            // on disk (including formatting). Prefer it over the pre-approval
            // preview so the card does not show stale content.
            if patch_diff.is_none() {
                if let Some(diff) = &approval.diff {
                    push_diff(out, diff_path(block), diff, width, theme, fold);
                }
            }
        }
        if let Some(diff) = patch_diff {
            push_diff(out, diff_path(block), diff, width, theme, fold);
        }
        if let Some(result) = &block.result {
            push_output(out, result.output.trim_end(), width, theme, fold);
        } else if block.approval.is_some() {
            push_body(
                out,
                "approval required — press y to approve, n to deny",
                width,
                theme,
                fold,
            );
        } else {
            push_body(out, "…", width, theme, fold);
        }
    }
    out.blank();
}

/// The live window stays a fixed height while the child works. It shows the
/// newest activity at the bottom so updates replace the viewport in place
/// instead of growing the transcript.
const SUBAGENT_WINDOW_ROWS: usize = 5;
/// Completed agents show only their answer; long answers are capped.
const SUBAGENT_RESULT_ROWS: usize = 12;

/// A child agent rendered in place of the parent's `spawn_agent` tool card.
fn subagent_block(
    out: &mut Transcript,
    block: &ToolBlock,
    subagent: &Subagent,
    width: usize,
    theme: Theme,
    folds: &HashMap<FoldId, bool>,
) {
    let id = FoldId::Event(block.seq);
    let open = folds.get(&id).copied().unwrap_or(true);
    let fold = Fold { id, open };

    let finished = subagent.status != SubagentStatus::Running || block.result.is_some();
    let failed = subagent.status == SubagentStatus::Failed
        || subagent.error.is_some()
        || block.result.as_ref().is_some_and(|result| !result.ok);
    let (label, dot, color) = if !finished {
        ("running", "◐", accent(theme))
    } else if failed {
        ("failed", "✗", theme.error.unwrap_or(DANGER))
    } else {
        ("done", "●", theme.diff_add.unwrap_or(SUCCESS))
    };

    let name = if subagent.name.trim().is_empty() {
        block
            .arguments
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("agent")
    } else {
        subagent.name.as_str()
    };
    let prefix = format!("  {} {} ↳ ", if open { "▾" } else { "▸" }, dot);
    let prefix_width = UnicodeWidthStr::width(prefix.as_str());
    let label_width = label.len() + 3; // " · " + label
    let name = truncate(
        name,
        width.saturating_sub(prefix_width + label_width).max(4),
    );
    out.push(
        Line::from(vec![
            Span::styled(prefix, Style::default().fg(color)),
            Span::styled(
                name,
                Style::default()
                    .fg(text_color(theme))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!(" · {label}"), Style::default().fg(color)),
        ]),
        Some(fold),
    );

    if open {
        if finished {
            subagent_result(out, block, subagent, width, theme, fold);
        } else {
            subagent_window(out, subagent, width, theme, fold);
        }
    }
    out.blank();
}

/// Draw the fixed-height live viewport for a running child agent.
fn subagent_window(
    out: &mut Transcript,
    subagent: &Subagent,
    width: usize,
    theme: Theme,
    fold: Fold,
) {
    let box_width = width.saturating_sub(BODY_INDENT.len()).max(6);
    let inner_width = box_width.saturating_sub(4).max(1);
    // Activity lines are the child's stored events. Its current thinking is
    // streamed as deltas and stored later, so append the live tail here and
    // let the window scroll it into view like any other newest row.
    let mut rows: Vec<(String, bool)> = subagent_activity(subagent)
        .into_iter()
        .map(|text| (text, false))
        .collect();
    if !subagent.reasoning.trim().is_empty() {
        rows.push(("✳ Thinking…".to_string(), true));
        rows.extend(
            wrapped_tail(
                &subagent.reasoning,
                inner_width,
                SUBAGENT_WINDOW_ROWS.saturating_sub(2),
            )
            .into_iter()
            .map(|line| (line, true)),
        );
    }
    if rows.is_empty() {
        rows.push(("waiting for agent…".to_string(), false));
    }
    if rows.len() > SUBAGENT_WINDOW_ROWS {
        let keep = SUBAGENT_WINDOW_ROWS.saturating_sub(1);
        rows = rows.split_off(rows.len().saturating_sub(keep));
        rows.insert(0, ("…".to_string(), false));
    }

    out.push(
        Line::from(Span::styled(
            format!("{BODY_INDENT}╭{}╮", "─".repeat(box_width.saturating_sub(2))),
            dim(theme),
        )),
        Some(fold),
    );
    for index in 0..SUBAGENT_WINDOW_ROWS {
        let (text, thinking) = rows
            .get(index)
            .map(|(text, thinking)| (text.as_str(), *thinking))
            .unwrap_or(("", false));
        let text = truncate(text, inner_width);
        let padding = inner_width.saturating_sub(UnicodeWidthStr::width(text.as_str()));
        let style = if text.is_empty() || text == "waiting for agent…" {
            dim(theme)
        } else if thinking {
            italic_dim(theme)
        } else {
            Style::default().fg(text_color(theme))
        };
        out.push(
            Line::from(vec![
                Span::raw(BODY_INDENT),
                Span::styled("│ ", dim(theme)),
                Span::styled(text, style),
                Span::raw(" ".repeat(padding)),
                Span::styled(" │", dim(theme)),
            ]),
            Some(fold),
        );
    }
    out.push(
        Line::from(Span::styled(
            format!("{BODY_INDENT}╰{}╯", "─".repeat(box_width.saturating_sub(2))),
            dim(theme),
        )),
        Some(fold),
    );
}

/// Wrap the end of `text` into at most `rows` lines. Thinking grows without
/// bound while it streams, and only the visible tail is drawn, so keep the
/// work proportional to the window instead of the whole buffer.
fn wrapped_tail(text: &str, width: usize, rows: usize) -> Vec<String> {
    if rows == 0 {
        return Vec::new();
    }
    // Two cells per row of slack: wrapping re-measures the exact lines below.
    let keep = width
        .saturating_mul(rows)
        .saturating_mul(2)
        .max(width)
        .max(1);
    let truncated = text.chars().nth(keep).is_some();
    let mut chars: Vec<char> = text.chars().rev().take(keep).collect();
    chars.reverse();
    let tail: String = chars.into_iter().collect();
    // A truncated head may start mid-line; drop that partial row so the first
    // drawn line is whole. Untruncated text keeps every line.
    let tail = if truncated {
        match tail.split_once('\n') {
            Some((_, rest)) => rest,
            None => tail.as_str(),
        }
    } else {
        tail.as_str()
    };
    let mut lines: Vec<String> = tail
        .lines()
        .flat_map(|line| wrap_text(line, width))
        .collect();
    if lines.len() > rows {
        lines = lines.split_off(lines.len() - rows);
    }
    lines
}

/// After completion the child's process disappears; only its answer remains.
fn subagent_result(
    out: &mut Transcript,
    block: &ToolBlock,
    subagent: &Subagent,
    width: usize,
    theme: Theme,
    fold: Fold,
) {
    let error = subagent
        .error
        .as_deref()
        .filter(|text| !text.trim().is_empty())
        .or_else(|| {
            block
                .result
                .as_ref()
                .filter(|result| !result.ok && !result.output.trim().is_empty())
                .map(|result| result.output.as_str())
        });
    let text = if let Some(error) = error {
        error.trim()
    } else {
        subagent
            .result
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .or_else(|| {
                block
                    .result
                    .as_ref()
                    .map(|result| agent_result_body(&result.output))
                    .filter(|text| !text.is_empty())
            })
            .unwrap_or("(no result)")
    };
    let color = if error.is_some() {
        theme.error.unwrap_or(DANGER)
    } else {
        text_color(theme)
    };
    let style = if text == "(no result)" {
        dim(theme)
    } else {
        Style::default().fg(color)
    };
    let wrapped: Vec<String> = text
        .lines()
        .flat_map(|line| wrap_text(line, width.saturating_sub(BODY_INDENT.len()).max(1)))
        .collect();
    for segment in wrapped.iter().take(SUBAGENT_RESULT_ROWS) {
        out.push(
            Line::from(vec![
                Span::raw(BODY_INDENT),
                Span::styled(segment.clone(), style),
            ]),
            Some(fold),
        );
    }
    if wrapped.len() > SUBAGENT_RESULT_ROWS {
        out.push(
            Line::from(Span::styled(format!("{BODY_INDENT}…"), dim(theme))),
            Some(fold),
        );
    }
}

/// Compact activity feed for the live window. This intentionally keeps only
/// the first line of each event so the viewport stays a fixed size.
pub(crate) fn subagent_activity(subagent: &Subagent) -> Vec<String> {
    subagent.events.iter().flat_map(activity_lines).collect()
}

/// The newest line the compact card/window would show, without formatting the
/// whole event log. Used by the sub-agent picker rows.
pub(crate) fn last_activity(subagent: &Subagent) -> Option<String> {
    subagent
        .events
        .iter()
        .rev()
        .find_map(|event| activity_lines(event).pop())
}

fn activity_lines(event: &SessionEvent) -> Vec<String> {
    let mut lines = Vec::new();
    match event.kind {
        EventKind::UserMessage => {
            if let Ok(payload) = UserMessagePayload::deserialize(&event.payload) {
                let text = first_line(&payload.text);
                if !text.is_empty() {
                    lines.push(format!("▶ {text}"));
                }
            }
        }
        EventKind::AssistantMessage => {
            if let Ok(payload) = AssistantMessagePayload::deserialize(&event.payload) {
                if let Some(reasoning) = payload
                    .reasoning
                    .as_deref()
                    .filter(|text| !text.trim().is_empty())
                {
                    lines.push(format!("✳ {}", first_line(reasoning)));
                }
                let text = first_line(&payload.text);
                if !text.is_empty() {
                    lines.push(format!("✎ {text}"));
                }
            }
        }
        EventKind::ToolCall => {
            if let Ok(payload) = ToolCallPayload::deserialize(&event.payload) {
                lines.push(format!(
                    "⚙ {}",
                    tool_call_summary(&payload.tool, &payload.arguments)
                ));
            }
        }
        EventKind::ToolResult => {
            if let Ok(payload) = ToolResultPayload::deserialize(&event.payload) {
                let marker = if payload.ok && !payload.denied {
                    "✓"
                } else {
                    "✗"
                };
                let output = first_line(&payload.output);
                if output.is_empty() {
                    lines.push(format!("{marker} {}", payload.tool));
                } else {
                    lines.push(format!("{marker} {}: {output}", payload.tool));
                }
            }
        }
        EventKind::ApprovalRequired => {
            if let Ok(payload) = ApprovalRequiredPayload::deserialize(&event.payload) {
                lines.push(format!("⚠ approval: {}", first_line(&payload.summary)));
            }
        }
        EventKind::InputRequired => {
            if let Ok(payload) = InputRequiredPayload::deserialize(&event.payload) {
                if let Some(question) = payload.questions.first() {
                    let extra = payload.questions.len().saturating_sub(1);
                    let suffix = if extra > 0 {
                        format!(" (+{extra} more)")
                    } else {
                        String::new()
                    };
                    lines.push(format!("? {}{suffix}", first_line(&question.question)));
                }
            }
        }
        EventKind::Error => {
            if let Some(message) = event.payload.get("message").and_then(Value::as_str) {
                lines.push(format!("! {}", first_line(message)));
            }
        }
        _ => {}
    }
    lines
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default()
        .to_string()
}

/// The parent's tool result wraps the child answer with a status line. Strip
/// that prefix when it is the fallback source for the result-only card.
fn agent_result_body(output: &str) -> &str {
    if let Some((_, body)) = output.split_once("finished:\n") {
        body.trim()
    } else {
        output.trim()
    }
}

/// A queued prompt waits here until the running turn finishes.
fn queued_prompt(out: &mut Transcript, text: &str, width: usize, theme: Theme) {
    let prefix = "  ⏳ queued  ";
    let prefix_width = UnicodeWidthStr::width(prefix);
    let style = Style::default().fg(theme.function.unwrap_or(WARN));
    let wrapped = wrap_text(text, width.saturating_sub(prefix_width).max(1));
    for (index, segment) in wrapped.into_iter().enumerate() {
        let lead = if index == 0 {
            Span::styled(prefix, style)
        } else {
            Span::raw(" ".repeat(prefix_width))
        };
        out.push(
            right_align(
                Line::from(vec![lead, Span::styled(segment, dim(theme))]),
                width,
            ),
            None,
        );
    }
}

fn status_dot(block: &ToolBlock) -> &'static str {
    match status_label(block) {
        "ok" | "applied" => "●",
        "failed" | "rejected" | "conflict" | "denied" => "✗",
        "needs approval" | "staged" => "⚠",
        "running" => "◐",
        _ => "•",
    }
}

fn tool_summary(block: &ToolBlock) -> String {
    if block.arguments.is_null() {
        if let Some(patch) = &block.patch {
            format!("{} {}", block.tool, patch.path)
        } else {
            block.tool.clone()
        }
    } else {
        tool_call_summary(&block.tool, &block.arguments)
    }
}

fn status_label(block: &ToolBlock) -> &'static str {
    if let Some(outcome) = block.patch.as_ref().and_then(|patch| patch.outcome) {
        return match outcome {
            PatchOutcome::Applied => "applied",
            PatchOutcome::Rejected => "rejected",
            PatchOutcome::Conflict => "conflict",
        };
    }
    if let Some(result) = &block.result {
        return if result.denied {
            "denied"
        } else if result.ok {
            "ok"
        } else {
            "failed"
        };
    }
    if block.approval.is_some() {
        return "needs approval";
    }
    if block.patch.is_some() {
        return "staged";
    }
    "running"
}

fn status_color(block: &ToolBlock, theme: Theme) -> Color {
    match status_label(block) {
        "ok" | "applied" => theme.diff_add.unwrap_or(SUCCESS),
        "failed" | "rejected" | "conflict" | "denied" => theme.error.unwrap_or(DANGER),
        "needs approval" | "staged" => theme.function.unwrap_or(WARN),
        _ => theme.function.unwrap_or(ACCENT),
    }
}

/// Expanded tool bodies stay close to the left edge; the fold chevron and
/// summary already provide the visual hierarchy.
const BODY_INDENT: &str = "    ";

fn is_command_tool(tool: &str) -> bool {
    matches!(tool, "exec_command")
}

/// Tools whose result is a reviewable file change. Their compact card already
/// prints the path, so the expanded body is just the side-by-side diff.
pub(super) fn is_file_change_tool(tool: &str) -> bool {
    matches!(tool, "apply_patch")
}

/// The expanded argument detail, tailored to the tool. The header already
/// carries the command/path, so the body never repeats it.
fn detail_text(block: &ToolBlock) -> String {
    let string = |key: &str| {
        block
            .arguments
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    match block.tool.as_str() {
        "exec_command" => {
            let cwd = string("workdir");
            let cwd = if cwd.is_empty() { string("cwd") } else { cwd };
            if cwd.is_empty() {
                String::new()
            } else {
                format!("cwd: {cwd}")
            }
        }
        // The diff preview carries the change; raw old/new strings or
        // `patchText` would only duplicate it.
        "apply_patch" | "view_image" => String::new(),
        "spawn_agent" => {
            let name = string("name");
            let prompt = string("prompt");
            if name.is_empty() {
                prompt.to_string()
            } else {
                format!("{name}: {prompt}")
            }
        }
        "ask_user" => {
            let mut text = string("question").to_string();
            if let Some(options) = block.arguments.get("options").and_then(Value::as_array) {
                for (index, option) in options.iter().enumerate() {
                    let label = option.get("label").and_then(Value::as_str).unwrap_or("");
                    let description = option
                        .get("description")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    let recommended = if index == 0 { " (recommended)" } else { "" };
                    if description.is_empty() {
                        text.push_str(&format!("\n  {}. {label}{recommended}", index + 1));
                    } else {
                        text.push_str(&format!(
                            "\n  {}. {label} — {description}{recommended}",
                            index + 1
                        ));
                    }
                }
            }
            text
        }
        _ => block
            .arguments
            .as_object()
            .map(|_| serde_json::to_string_pretty(&block.arguments).unwrap_or_default())
            .unwrap_or_default(),
    }
}

fn push_body(out: &mut Transcript, text: &str, width: usize, theme: Theme, fold: Fold) {
    push_text(out, text, None, width, theme, fold);
}

/// Tool output, capped at [`OUTPUT_ROWS`] with a trailing ellipsis.
fn push_output(out: &mut Transcript, text: &str, width: usize, theme: Theme, fold: Fold) {
    push_text(out, text, Some(OUTPUT_ROWS), width, theme, fold);
}

fn push_text(
    out: &mut Transcript,
    text: &str,
    max_lines: Option<usize>,
    width: usize,
    theme: Theme,
    fold: Fold,
) {
    if text.is_empty() {
        return;
    }
    let prefix_width = BODY_INDENT.len();
    let limit = max_lines.unwrap_or(usize::MAX);
    let lines: Vec<&str> = text.lines().collect();
    for line in lines.iter().take(limit) {
        for segment in wrap_text(line, width.saturating_sub(prefix_width).max(1)) {
            out.push(
                Line::from(vec![
                    Span::raw(BODY_INDENT),
                    Span::styled(segment, Style::default().fg(text_color(theme))),
                ]),
                Some(fold),
            );
        }
    }
    if lines.len() > limit {
        out.push(
            Line::from(Span::styled(format!("{BODY_INDENT}…"), dim(theme))),
            Some(fold),
        );
    }
}

/// The target path for syntax detection. Patches carry it directly; approval
/// cards only expose the tool arguments.
fn diff_path(block: &ToolBlock) -> &str {
    block
        .patch
        .as_ref()
        .map(|patch| patch.path.as_str())
        .filter(|path| !path.is_empty())
        .or_else(|| block.arguments.get("path").and_then(Value::as_str))
        .unwrap_or("")
}

/// Side-by-side diff preview with syntax highlighting. Narrow panes fall back
/// to the unified view so both code columns stay readable.
fn push_diff(out: &mut Transcript, path: &str, diff: &str, width: usize, theme: Theme, fold: Fold) {
    let rows = super::diff::rows(&[], Some(diff));
    let base_bg = super::diff::base_background(theme);
    if !rows.is_empty() {
        if let Some(mut renderer) =
            super::diff::DiffRenderer::new(&rows, width, path, theme, BODY_INDENT, base_bg)
        {
            for line in renderer.render(&rows, false, &[], 0) {
                out.push(line, Some(fold));
            }
            return;
        }
    }

    let add = theme.diff_add.unwrap_or(SUCCESS);
    let delete = theme.diff_delete.unwrap_or(DANGER);
    for line in diff.lines() {
        let color = if line.starts_with('+') && !line.starts_with("+++") {
            add
        } else if line.starts_with('-') && !line.starts_with("---") {
            delete
        } else if line.starts_with("@@") {
            accent(theme)
        } else {
            comment(theme)
        };
        for segment in wrap_text(line, width.saturating_sub(BODY_INDENT.len()).max(1)) {
            out.push(
                Line::from(vec![
                    Span::raw(BODY_INDENT),
                    Span::styled(segment, Style::default().fg(color)),
                ]),
                Some(fold),
            );
        }
    }
}
