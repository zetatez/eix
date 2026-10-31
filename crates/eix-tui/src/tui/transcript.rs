//! Chat-style transcript rendering.
//!
//! The transcript is a conversation, not a log: user turns sit on the right in
//! a rounded bubble, while agent output stays on the left. The agent's chain
//! of thought and every tool invocation render as compact headers that expand
//! into their details, and the same line map powers click-to-fold. Blocks still
//! streaming live under a synthetic [`FoldId::Live`] key so they collapse once
//! the turn is stored.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eix_core::{
    ApprovalRequiredPayload, AssistantMessagePayload, ContextCompactedPayload,
    Event as SessionEvent, EventKind, ImageAttachment, InputRequiredPayload, PatchProposedPayload,
    PatchResultPayload, ToolCallPayload, ToolResultPayload, UserMessagePayload,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde::Deserialize;
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use super::app::{tool_call_summary, truncate, wrap_text};
use super::markdown;
use super::math_image;
use super::theme::Theme;

/// Tool output is capped so a verbose command cannot flood the transcript.
/// File-change diffs are not capped: they stay open by default so a write is
/// never silently folded away, and the daemon already bounds their size.
const OUTPUT_ROWS: usize = 40;

/// Height of the live Thinking viewport. The box stays this size while the
/// model streams, so a long chain of thought scrolls instead of pushing the
/// rest of the transcript down on every delta.
const THINKING_WINDOW_ROWS: usize = 12;

/// Left rule for the thinking timeline. The live window and the expanded
/// transcript share it, so removing the box does not change the text layout.
const THINKING_INDENT: &str = "    │ ";

/// Timeline nodes for the first thought, a heading, a new paragraph and the
/// truncated top of the live window. Keep them small and muted: the rule should
/// guide the eye without competing with the reasoning text.
const THINKING_START: &str = "    ◦ ";
const THINKING_HEADING: &str = "    ▸ ";
const THINKING_POINT: &str = "    · ";
const THINKING_HIDDEN: &str = "    ⋮ ";

/// Opacity of the outermost, second and third rows of the live window. A
/// terminal cannot blur glyphs, so the fade is a color blend toward the pane
/// background; this gives the newest line a soft entrance and lets the oldest
/// line disappear as it scrolls out.
const THINKING_FADE: [f32; 3] = [0.18, 0.48, 0.76];

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
    /// Display-math images, anchored to `lines`.
    pub math: Vec<math_image::Placement>,
    /// Display math that is still waiting for its PNG. The viewport uses these
    /// to request renders and to notice when an image can replace its ASCII
    /// fallback.
    pub math_pending: Vec<math_image::Pending>,
    /// Line -> LaTeX source, so copying a rendered formula yields the source.
    pub copy: Vec<(usize, String)>,
    /// Clickable link ranges, in final transcript coordinates.
    pub links: Vec<markdown::LinkSpan>,
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
        images: Vec<ImageAttachment>,
    },
    Assistant {
        seq: u64,
        text: String,
        reasoning: Option<String>,
        cancelled: bool,
        images: Vec<ImageAttachment>,
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
    base: Option<&Path>,
) -> Transcript {
    let tail_width = width.saturating_sub(2).max(1);
    let mut out = render_history_with_subagents(events, folds, width, theme, subagents, base);
    append_tail(&mut out, live, queued, folds, tail_width, theme, tick, base);
    out
}

/// Render a standalone event log, used by the sub-agent picker preview. The
/// child has no nested `spawn_agent` cards, so the subagent map is empty.
pub fn render_events(
    events: &[SessionEvent],
    width: usize,
    theme: Theme,
    base: Option<&Path>,
) -> Transcript {
    let width = width.saturating_sub(2).max(1);
    let mut blocks = Vec::new();
    let mut tool_index = Vec::new();
    collect_into(events, &mut blocks, &mut tool_index);
    let no_folds = HashMap::new();
    let no_subagents = HashMap::new();
    let mut lines = Vec::new();
    let mut slots = Vec::new();
    let mut user_lines = Vec::new();
    let mut math = Vec::new();
    let mut math_pending = Vec::new();
    let mut copy = Vec::new();
    let mut links = Vec::new();
    for collected in blocks {
        let piece = render_block(
            &collected.block,
            &no_folds,
            width,
            theme,
            &no_subagents,
            true,
            base,
        );
        append_piece_parts(
            &mut lines,
            &mut slots,
            &mut user_lines,
            &mut math,
            &mut math_pending,
            &mut copy,
            &mut links,
            piece,
        );
    }
    Transcript {
        lines,
        slots,
        user_lines,
        math,
        math_pending,
        copy,
        links,
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
    base: Option<&Path>,
) -> Transcript {
    // Keep a two-cell gutter on the right so queued prompts never touch the
    // pane border.
    let width = width.saturating_sub(2).max(1);
    render_blocks(collect(events), folds, width, theme, subagents, base)
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
    base: Option<&Path>,
) -> Transcript {
    let width = width.saturating_sub(2).max(1);
    let mut out = Transcript {
        lines: Vec::new(),
        slots: Vec::new(),
        user_lines: Vec::new(),
        math: Vec::new(),
        math_pending: Vec::new(),
        copy: Vec::new(),
        links: Vec::new(),
    };
    append_tail(&mut out, live, queued, folds, width, theme, tick, base);
    out
}

#[cfg(test)]
fn render_blocks(
    blocks: Vec<Collected>,
    folds: &HashMap<FoldId, bool>,
    width: usize,
    theme: Theme,
    subagents: &HashMap<String, Subagent>,
    base: Option<&Path>,
) -> Transcript {
    let mut out = Transcript::new();
    for collected in blocks {
        let piece = render_block(&collected.block, folds, width, theme, subagents, true, base);
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
    images: bool,
    base: Option<&Path>,
) -> Transcript {
    let mut out = Transcript::new();
    match block {
        Block::User { seq, text, images } => {
            let line = out.lines.len();
            user_message(&mut out, text, images, width, theme, base);
            out.user_lines.push((*seq, line));
        }
        Block::Assistant {
            seq,
            text,
            reasoning,
            cancelled,
            images: attachments,
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
                    images,
                );
            }
            if !text.is_empty() || !attachments.is_empty() {
                assistant_message(
                    &mut out,
                    text,
                    attachments,
                    *cancelled,
                    width,
                    theme,
                    images,
                    base,
                );
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
                None => tool_block(&mut out, block, width, theme, folds, base),
            }
        }
        Block::Note { text } => note(&mut out, '◇', text, comment(theme), width),
        Block::Compaction { messages, summary } => {
            compaction_summary(&mut out, *messages, summary, width, theme, images, base)
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
    math: &mut Vec<math_image::Placement>,
    math_pending: &mut Vec<math_image::Pending>,
    copy: &mut Vec<(usize, String)>,
    links: &mut Vec<markdown::LinkSpan>,
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
    for mut placement in piece.math {
        if placement.line < skip {
            continue;
        }
        placement.line = base + placement.line - skip;
        math.push(placement);
    }
    for mut pending in piece.math_pending {
        if pending.line < skip {
            continue;
        }
        pending.line = base + pending.line - skip;
        math_pending.push(pending);
    }
    for (line, source) in piece.copy {
        if line < skip {
            continue;
        }
        copy.push((base + line - skip, source));
    }
    for mut link in piece.links {
        if link.line < skip {
            continue;
        }
        link.line = base + link.line - skip;
        links.push(link);
    }
}

#[cfg(test)]
fn append_piece(dst: &mut Transcript, piece: Transcript) {
    let mut lines = std::mem::take(&mut dst.lines);
    let mut slots = std::mem::take(&mut dst.slots);
    let mut user_lines = std::mem::take(&mut dst.user_lines);
    let mut math = std::mem::take(&mut dst.math);
    let mut math_pending = std::mem::take(&mut dst.math_pending);
    let mut copy = std::mem::take(&mut dst.copy);
    let mut links = std::mem::take(&mut dst.links);
    append_piece_parts(
        &mut lines,
        &mut slots,
        &mut user_lines,
        &mut math,
        &mut math_pending,
        &mut copy,
        &mut links,
        piece,
    );
    dst.lines = lines;
    dst.slots = slots;
    dst.user_lines = user_lines;
    dst.math = math;
    dst.math_pending = math_pending;
    dst.copy = copy;
    dst.links = links;
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
    base: Option<PathBuf>,
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
    pub math: Vec<math_image::Placement>,
    pub math_pending: Vec<math_image::Pending>,
    pub copy: Vec<(usize, String)>,
    pub links: Vec<markdown::LinkSpan>,
    /// Bumped by the viewport when a pending formula became displayable, so
    /// the next render replaces its ASCII fallback with the image.
    math_rev: u64,
}

impl HistoryCache {
    #[allow(clippy::too_many_arguments)]
    pub fn needs_render(
        &self,
        width: usize,
        theme: Theme,
        events_rev: u64,
        subagents_rev: u64,
        folds: &HashMap<FoldId, bool>,
        base: Option<&Path>,
        math_rev: u64,
    ) -> bool {
        self.width == 0
            || self.width != width
            || self.theme != theme
            || self.base.as_deref() != base
            || self.events_rev != events_rev
            || self.subagents_rev != subagents_rev
            || self.folds != *folds
            || self.math_rev != math_rev
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
        base: Option<&Path>,
        math_rev: u64,
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
        let mut common = if width != self.width || theme != self.theme || math_rev != self.math_rev
        {
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
        // A width/theme change or a newly displayable image invalidates every
        // block; otherwise reuse the matching prefix and repair only the suffix.
        if common > keys.len() {
            common = keys.len();
        }
        if common < self.keys.len() {
            let cut = self.starts.get(common).copied().unwrap_or(self.lines.len());
            self.lines.truncate(cut);
            self.slots.truncate(cut);
            self.user_lines.retain(|(_, line)| *line < cut);
            self.math.retain(|placement| placement.line < cut);
            self.math_pending.retain(|pending| pending.line < cut);
            self.copy.retain(|(line, _)| *line < cut);
            self.links.retain(|link| link.line < cut);
            self.keys.truncate(common);
            self.starts.truncate(common);
        }
        for (index, collected) in self.blocks.iter().enumerate().skip(common) {
            self.starts.push(self.lines.len());
            self.keys.push(keys[index]);
            let piece = render_block(&collected.block, folds, width, theme, subagents, true, base);
            append_piece_parts(
                &mut self.lines,
                &mut self.slots,
                &mut self.user_lines,
                &mut self.math,
                &mut self.math_pending,
                &mut self.copy,
                &mut self.links,
                piece,
            );
        }
        self.width = width;
        self.theme = theme;
        self.base = base.map(PathBuf::from);
        self.events_rev = events_rev;
        self.subagents_rev = subagents_rev;
        self.folds = folds.clone();
        self.math_rev = math_rev;
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

#[allow(clippy::too_many_arguments)]
fn append_tail(
    out: &mut Transcript,
    live: Option<Live<'_>>,
    queued: &[String],
    folds: &HashMap<FoldId, bool>,
    width: usize,
    theme: Theme,
    tick: u64,
    base: Option<&Path>,
) {
    if let Some(live) = live {
        if !live.reasoning.is_empty() {
            // The box is live only until the answer starts streaming; after
            // that the reasoning block collapses like a stored one, while a
            // click can still expand the full text.
            thinking(
                out,
                FoldId::Live,
                live.reasoning,
                live.text.is_empty(),
                width,
                theme,
                folds,
                true,
            );
        }
        if !live.text.is_empty() {
            assistant_message(out, live.text, &[], false, width, theme, true, base);
        } else if live.reasoning.is_empty() {
            assistant_message(
                out,
                waiting_dots(tick),
                &[],
                false,
                width,
                theme,
                true,
                base,
            );
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
                            images: payload.images,
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
                            images: payload.images,
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
            math: Vec::new(),
            math_pending: Vec::new(),
            copy: Vec::new(),
            links: Vec::new(),
        }
    }

    fn push(&mut self, line: Line<'static>, slot: Option<Fold>) {
        self.lines.push(line);
        self.slots.push(slot);
    }

    /// Append a Markdown render, shifting its image anchors and copy sources
    /// onto this transcript's existing line numbers.
    fn push_rendered(&mut self, rendered: markdown::Rendered) {
        let base = self.lines.len();
        let len = rendered.lines.len();
        for line in rendered.lines {
            self.push(line, None);
        }
        for mut placement in rendered.images {
            placement.line += base;
            self.math.push(placement);
        }
        for mut pending in rendered.pending {
            pending.line += base;
            self.math_pending.push(pending);
        }
        for (line, source) in rendered.sources {
            if line < len {
                self.copy.push((base + line, source));
            }
        }
        for mut link in rendered.links {
            if link.line < len {
                link.line += base;
                self.links.push(link);
            }
        }
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
/// Transparent palettes have no accent hue; follow the terminal foreground.
const ACCENT: Color = Color::Reset;
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
fn push_bubble(
    out: &mut Transcript,
    mut content: Vec<Line<'static>>,
    mut sources: Vec<(usize, String)>,
    mut placements: Vec<math_image::Placement>,
    mut pending: Vec<math_image::Pending>,
    mut links: Vec<markdown::LinkSpan>,
    width: usize,
    theme: Theme,
) {
    // Keep zero-width placeholder rows that an image is painted over. Trimming
    // them would drop an image-only turn (or the leading image of a turn that
    // also has text) before `push_bubble` transfers its placement.
    let image_covers = |line: usize| {
        placements.iter().any(|placement| {
            line >= placement.line
                && line < placement.line + usize::from(placement.image.rows.max(1))
        })
    };
    let mut removed = 0;
    while content.first().is_some_and(|line| line.width() == 0) && !image_covers(removed) {
        content.remove(0);
        removed += 1;
    }
    while content.last().is_some_and(|line| line.width() == 0) {
        let line = removed + content.len() - 1;
        if image_covers(line) {
            break;
        }
        content.pop();
    }
    if content.is_empty() {
        out.blank();
        return;
    }

    let mut content_width = content.iter().map(Line::width).max().unwrap_or(0);
    for placement in &placements {
        let column = usize::from(placement.column);
        let cols = usize::from(placement.image.cols);
        // The formula was centred inside the Markdown render width, so keep
        // that width; otherwise shrinking to the text would right-align it.
        content_width = content_width.max(column + cols).max(2 * column + cols);
    }
    let content_width = content_width.max(1);
    let box_width = content_width + 2 * (BUBBLE_PAD + 1);
    let body_pad = width.saturating_sub(box_width);

    let border = bubble_border_style(theme);
    let panel = with_panel(Style::default(), theme.panel);

    let base = out.lines.len();
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

    // The top border occupies one line; content starts below it. The bubble
    // adds `body_pad`, the left border and its inner padding before content.
    let left = body_pad + 1 + BUBBLE_PAD;
    sources.retain(|(line, _)| *line >= removed);
    for (line, source) in sources {
        out.copy.push((base + 1 + line - removed, source));
    }
    links.retain(|link| link.line >= removed);
    for mut link in links {
        link.line = base + 1 + link.line - removed;
        link.start += left;
        link.end += left;
        out.links.push(link);
    }
    placements.retain(|placement| placement.line >= removed);
    for mut placement in placements {
        placement.line = base + 1 + placement.line - removed;
        placement.column = (left + usize::from(placement.column)).min(u16::MAX as usize) as u16;
        out.math.push(placement);
    }
    pending.retain(|pending| pending.line >= removed);
    for mut pending in pending {
        pending.line = base + 1 + pending.line - removed;
        out.math_pending.push(pending);
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
fn user_message(
    out: &mut Transcript,
    text: &str,
    images: &[ImageAttachment],
    width: usize,
    theme: Theme,
    base: Option<&Path>,
) {
    let content_width = bubble_content_limit(width);
    let (mut content, mut placements) = thumbnail_rows(images, content_width, theme, base);
    let image_lines = content.len();
    let mut sources = Vec::new();
    let mut math_pending = Vec::new();
    let mut links = Vec::new();
    let text = text.trim_end();
    if !text.is_empty() {
        // The bubble supplies its own horizontal padding, so render without
        // the transcript's usual two-cell left gutter. Display math still uses
        // a LaTeX image; `push_bubble` translates its anchors into the box.
        let rendered = markdown::render_with_indent_detailed(
            text,
            content_width.max(1),
            theme,
            "",
            true,
            base,
        );
        content.extend(rendered.lines);
        sources.extend(
            rendered
                .sources
                .into_iter()
                .map(|(line, source)| (image_lines + line, source)),
        );
        placements.extend(rendered.images.into_iter().map(|mut placement| {
            placement.line += image_lines;
            placement
        }));
        math_pending.extend(rendered.pending.into_iter().map(|mut pending| {
            pending.line += image_lines;
            pending
        }));
        links.extend(rendered.links.into_iter().map(|mut link| {
            link.line += image_lines;
            link
        }));
    }
    push_bubble(
        out,
        content,
        sources,
        placements,
        math_pending,
        links,
        width,
        theme,
    );
    out.blank();
}

/// Agent output: rendered as Markdown, indented only enough to breathe, with
/// no role label.
#[allow(clippy::too_many_arguments)]
fn assistant_message(
    out: &mut Transcript,
    text: &str,
    attachments: &[ImageAttachment],
    cancelled: bool,
    width: usize,
    theme: Theme,
    images: bool,
    base: Option<&Path>,
) {
    let mut rendered = markdown::render_with_indent_detailed(
        text.trim_end_matches('\n'),
        width.max(1),
        theme,
        "  ",
        images,
        base,
    );
    if rendered.lines.is_empty() && attachments.is_empty() {
        rendered.lines.push(Line::from("  "));
    }
    if cancelled {
        if let Some(last) = rendered.lines.last_mut() {
            last.spans.push(Span::styled("  · cancelled", dim(theme)));
        }
    }
    out.push_rendered(rendered);
    for attachment in attachments {
        push_image_attachment(out, attachment, width, theme, base);
    }
    out.blank();
}

/// Place a provider-returned or user-attached image outside a chat bubble.
/// Undrawable images keep a one-line label instead of disappearing.
fn push_image_attachment(
    out: &mut Transcript,
    attachment: &ImageAttachment,
    width: usize,
    theme: Theme,
    base: Option<&Path>,
) {
    let prefix_width = BODY_INDENT.len();
    let avail = width.saturating_sub(prefix_width).max(1);
    let Some(image) = render_attachment(attachment, avail, base) else {
        out.push(
            Line::from(vec![
                Span::raw(BODY_INDENT),
                Span::styled(format!("[image] {}", image_label(attachment)), dim(theme)),
            ]),
            None,
        );
        return;
    };
    let start = out.lines.len();
    let rows = usize::from(image.rows.max(1));
    let cols = usize::from(image.cols);
    let pad = avail.saturating_sub(cols) / 2;
    for _ in 0..rows {
        out.push(Line::from(Span::raw(BODY_INDENT)), None);
    }
    out.math.push(math_image::Placement {
        line: start,
        column: (prefix_width + pad).min(u16::MAX as usize) as u16,
        image,
    });
}

/// Resolve one attachment to a thumbnail. Inline payloads are decoded
/// directly; URLs are downloaded by the image worker and local paths are read
/// from disk relative to the session cwd.
fn render_attachment(
    attachment: &ImageAttachment,
    width: usize,
    base: Option<&Path>,
) -> Option<math_image::MathImage> {
    let cols = Some(width.min(u16::MAX as usize) as u16);
    if !attachment.data.is_empty() {
        math_image::render_base64(&attachment.data, cols, None)
    } else if attachment.path.starts_with("http://") || attachment.path.starts_with("https://") {
        math_image::render_remote(&attachment.path, cols, None)
    } else if attachment.path.starts_with("data:") {
        math_image::render_data_uri(&attachment.path, cols, None)
    } else if !attachment.path.is_empty() {
        math_image::render_file(Path::new(&attachment.path), base, cols, None)
    } else {
        None
    }
}

fn image_label(attachment: &ImageAttachment) -> &str {
    if attachment.path.is_empty() {
        "inline image"
    } else {
        &attachment.path
    }
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
    images: bool,
    base: Option<&Path>,
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
    let rendered =
        markdown::render_with_indent_detailed(summary, content_width, theme, indent, images, base);
    out.push_rendered(rendered);
    out.blank();
}

/// The "Thinking" module: a collapsible, quietly styled header over the chain
/// of thought. While the model is still thinking, the body is a fixed-height
/// window that tails the latest output. Once thinking ends the block collapses;
/// expanding it later shows the complete reasoning, not just the window.
/// Reasoning models also emit `$$...$$` formulas, so both views use the same
/// image/2D renderer as assistant output instead of printing raw TeX.
#[allow(clippy::too_many_arguments)]
fn thinking(
    out: &mut Transcript,
    id: FoldId,
    text: &str,
    thinking: bool,
    width: usize,
    theme: Theme,
    folds: &HashMap<FoldId, bool>,
    images: bool,
) {
    let inner = width.saturating_sub(6).max(1);
    // Active thinking opens its fixed window; a finished block starts folded.
    let open = folds.get(&id).copied().unwrap_or(thinking);
    let fold = Fold { id, open };
    let marker = if open { '▾' } else { '▸' };
    let closed_lines = || {
        text.trim_end()
            .lines()
            .map(|line| wrap_text(line, inner).len())
            .sum::<usize>()
            .max(1)
    };
    let header = if thinking {
        format!("  {marker} 🧠 Thinking…")
    } else {
        let lines = closed_lines();
        if lines == 1 {
            format!("  {marker} 🧠 Thinking · 1 line")
        } else {
            format!("  {marker} 🧠 Thinking · {lines} lines")
        }
    };
    out.push(
        Line::from(Span::styled(header, italic_dim(theme))),
        Some(fold),
    );
    if open {
        if thinking {
            thinking_window(out, text.trim_end(), width, theme, fold);
        } else {
            let mut rich = rich_text(
                text.trim_end(),
                width,
                theme,
                THINKING_INDENT,
                italic_dim(theme),
                images,
            );
            decorate_thinking_timeline(&mut rich.lines, theme, false);
            let base_line = out.lines.len();
            for line in rich.lines {
                out.push(line, Some(fold));
            }
            for mut placement in rich.math {
                placement.line += base_line;
                out.math.push(placement);
            }
            for mut pending in rich.pending {
                pending.line += base_line;
                out.math_pending.push(pending);
            }
            for (line, source) in rich.sources {
                out.copy.push((base_line + line, source));
            }
        }
    }
    out.blank();
}

/// Draw the fixed-height live viewport as a timeline: a left rule instead of a
/// box, with only the tail of the stream visible. A long reasoning stream never
/// grows the transcript.
fn thinking_window(out: &mut Transcript, text: &str, width: usize, theme: Theme, fold: Fold) {
    let rich = rich_text(
        text,
        width,
        theme,
        THINKING_INDENT,
        italic_dim(theme),
        false,
    );
    let start = rich.lines.len().saturating_sub(THINKING_WINDOW_ROWS);
    let mut rows = Vec::with_capacity(THINKING_WINDOW_ROWS);
    for index in 0..THINKING_WINDOW_ROWS {
        rows.push(
            rich.lines
                .get(start + index)
                .cloned()
                .unwrap_or_else(|| Line::from(Span::raw(THINKING_INDENT))),
        );
    }
    decorate_thinking_timeline(&mut rows, theme, start > 0);
    for (index, line) in rows.into_iter().enumerate() {
        let distance = index.min(THINKING_WINDOW_ROWS - 1 - index);
        let opacity = THINKING_FADE.get(distance).copied().unwrap_or(1.0);
        out.push(fade_line(line, theme, opacity), Some(fold));
    }
}

/// Replace the left rule with a node when a rendered line starts a new idea.
/// The classifier only uses visible structure (heading, paragraph break), so it
/// stays cheap and never second-guesses the model's prose.
fn decorate_thinking_timeline(lines: &mut [Line<'static>], theme: Theme, truncated_above: bool) {
    let mut previous_blank = true;
    let mut seen_content = false;
    for (index, line) in lines.iter_mut().enumerate() {
        let Some(prefix) = line.spans.first() else {
            continue;
        };
        if prefix.content.as_ref() != THINKING_INDENT {
            continue;
        }
        let body: String = line
            .spans
            .iter()
            .skip(1)
            .map(|span| span.content.as_ref())
            .collect();
        let blank = body.trim().is_empty();
        let node = if truncated_above && index == 0 {
            Some(THINKING_HIDDEN)
        } else if blank {
            None
        } else if body.trim_start().starts_with('#') {
            Some(THINKING_HEADING)
        } else if !seen_content {
            Some(THINKING_START)
        } else if previous_blank {
            Some(THINKING_POINT)
        } else {
            None
        };
        line.spans[0] = match node {
            Some(glyph) => Span::styled(glyph, dim(theme)),
            None => Span::styled(THINKING_INDENT, dim(theme)),
        };
        if !blank {
            seen_content = true;
        }
        previous_blank = blank;
    }
}

/// Blend a line toward the pane background. Terminals have no alpha channel,
/// so opacity is approximated per span; non-RGB colors fall back to `DIM`.
fn fade_line(mut line: Line<'static>, theme: Theme, opacity: f32) -> Line<'static> {
    if opacity >= 1.0 {
        return line;
    }
    let background = super::diff::base_background(theme);
    for span in &mut line.spans {
        let style = span.style;
        let foreground = style.fg.unwrap_or_else(|| comment(theme));
        span.style = match (
            super::syntax::rgb(foreground),
            super::syntax::rgb(background),
        ) {
            (Some((red, green, blue)), Some((bg_red, bg_green, bg_blue))) => style.fg(Color::Rgb(
                blend_channel(red, bg_red, opacity),
                blend_channel(green, bg_green, opacity),
                blend_channel(blue, bg_blue, opacity),
            )),
            _ => style.add_modifier(Modifier::DIM),
        };
    }
    line
}

fn blend_channel(foreground: u8, background: u8, opacity: f32) -> u8 {
    (background as f32 + (foreground as f32 - background as f32) * opacity)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// A piece of plain text with the display-math spans pulled out.
enum MathSegment<'a> {
    Text(&'a str),
    Display(&'a str),
}

/// Split `text` on `$$...$$`. Unclosed delimiters stay literal text.
fn math_segments(text: &str) -> Vec<MathSegment<'_>> {
    let bytes = text.as_bytes();
    let mut segments = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] == b'$' && bytes[index + 1] == b'$' && !is_escaped(bytes, index) {
            if let Some(close) = text[index + 2..].find("$$") {
                let close = index + 2 + close;
                if close > index + 2 {
                    if start < index {
                        segments.push(MathSegment::Text(&text[start..index]));
                    }
                    segments.push(MathSegment::Display(&text[index + 2..close]));
                    index = close + 2;
                    start = index;
                    continue;
                }
            }
        }
        index += 1;
    }
    if start < text.len() {
        segments.push(MathSegment::Text(&text[start..]));
    }
    if segments.is_empty() {
        segments.push(MathSegment::Text(text));
    }
    segments
}

fn is_escaped(bytes: &[u8], index: usize) -> bool {
    let mut backslashes = 0;
    let mut cursor = index;
    while cursor > 0 && bytes[cursor - 1] == b'\\' {
        backslashes += 1;
        cursor -= 1;
    }
    backslashes % 2 == 1
}

struct Rich {
    lines: Vec<Line<'static>>,
    math: Vec<math_image::Placement>,
    pending: Vec<math_image::Pending>,
    /// Line -> LaTeX source, so copying a formula in plain tool output or
    /// reasoning still yields the source rather than the ASCII approximation.
    sources: Vec<(usize, String)>,
}

/// Render plain text (tool output, reasoning) while turning `$$...$$` into a
/// display-math image or the 2D character layout. Inline `$...$` stays literal.
fn rich_text(
    text: &str,
    width: usize,
    theme: Theme,
    prefix: &str,
    style: Style,
    images: bool,
) -> Rich {
    rich_text_inner(
        text,
        width,
        theme,
        prefix,
        style,
        images,
        math_image::raw_source(),
    )
}

#[allow(clippy::too_many_arguments)]
fn rich_text_inner(
    text: &str,
    width: usize,
    theme: Theme,
    prefix: &str,
    style: Style,
    images: bool,
    raw: bool,
) -> Rich {
    let prefix_width = UnicodeWidthStr::width(prefix);
    let avail = width.saturating_sub(prefix_width).max(1);
    let mut rich = Rich {
        lines: Vec::new(),
        math: Vec::new(),
        pending: Vec::new(),
        sources: Vec::new(),
    };
    let color = markdown::math_text_color(theme);
    for segment in math_segments(text) {
        match segment {
            MathSegment::Text(text) => {
                for line in text.lines() {
                    for wrapped in wrap_text(line, avail) {
                        rich.lines.push(Line::from(vec![
                            Span::raw(prefix.to_string()),
                            Span::styled(wrapped, style),
                        ]));
                    }
                }
            }
            MathSegment::Display(source) => {
                if raw {
                    for line in markdown::raw_math_lines(source, avail) {
                        rich.lines.push(Line::from(vec![
                            Span::raw(prefix.to_string()),
                            Span::styled(line, style),
                        ]));
                    }
                    continue;
                }
                let image = images.then(|| math_image::render(source, avail, color));
                let start = rich.lines.len();
                if let Some(Some(image)) = image {
                    let rows = usize::from(image.rows.max(1));
                    let cols = usize::from(image.cols);
                    let pad = avail.saturating_sub(cols) / 2;
                    let column = (prefix_width + pad).min(u16::MAX as usize) as u16;
                    for _ in 0..rows {
                        rich.lines.push(Line::from(Span::raw(prefix.to_string())));
                    }
                    rich.math.push(math_image::Placement {
                        line: start,
                        column,
                        image,
                    });
                } else {
                    if images && math_image::pending(source, avail, color, false) {
                        rich.pending.push(math_image::Pending {
                            line: start,
                            source: source.to_string(),
                            width: avail,
                            color,
                            inline: false,
                        });
                    }
                    for line in super::math::display(source, avail, style) {
                        let mut spans = vec![Span::raw(prefix.to_string())];
                        spans.extend(line.spans);
                        rich.lines.push(Line::from(spans));
                    }
                }
                for line in start..rich.lines.len() {
                    rich.sources.push((line, source.to_string()));
                }
            }
        }
    }
    rich
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
    base: Option<&Path>,
) {
    let id = FoldId::Event(block.seq);
    // File-writing tools keep their vimdiff-style preview visible after the
    // call finishes. Other in-flight calls open so their arguments are visible;
    // finished ones collapse to the summary line. Command cards stay open even
    // while an approval is pending, and the app briefly reopens them when the
    // result lands.
    let default_open = is_file_change_tool(&block.tool)
        || (block.result.is_none() && (block.approval.is_none() || is_command_tool(&block.tool)));
    let open = folds.get(&id).copied().unwrap_or(default_open);
    let fold = Fold { id, open };

    let color = status_color(block, theme);
    let status = status_label(block);
    let icon = tool_icon(&block.tool, &block.arguments);
    // "  ▾ ● " plus a fixed two-cell icon column.
    let prefix_width = 9;
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
            spans.push(Span::styled(icon_cell(icon), dim(theme)));
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
        if block.tool == "view_image" && block.result.as_ref().is_some_and(|result| result.ok) {
            if let Some(path) = block.arguments.get("path").and_then(Value::as_str) {
                push_thumbnail(out, Path::new(path), base, width, fold);
            }
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
        rows.push(("🧠 Thinking…".to_string(), true));
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
                    lines.push(format!("🧠 {}", first_line(reasoning)));
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

/// Pad an icon to the fixed two-cell column used by tool headers. Emoji and
/// plain glyphs mix widths, so the padding is measured instead of assumed.
fn icon_cell(icon: &str) -> String {
    let pad = 2usize.saturating_sub(UnicodeWidthStr::width(icon));
    format!("{icon}{} ", " ".repeat(pad))
}

/// Glyph shown before a tool's summary. A uniform dot hides what the agent is
/// doing; the tool family is recognizable at a glance, and shell commands are
/// classified by the program they run.
fn tool_icon(tool: &str, arguments: &Value) -> &'static str {
    match tool {
        "exec_command" => command_icon(
            arguments
                .get("cmd")
                .or_else(|| arguments.get("command"))
                .and_then(Value::as_str)
                .unwrap_or_default(),
        ),
        "apply_patch" => "✎",
        "write_stdin" => "⌨",
        "view_image" => "▣",
        "use_skill" => "📚",
        "update_plan" => "☑",
        "get_context_remaining" => "📊",
        "memory" => "💾",
        "ready" => "✅",
        "web_fetch" => "🌐",
        "web_search" => "🔍",
        "lsp_diagnostics" => "🩺",
        "spawn_agent" => "🤖",
        "request_user_input" => "❓",
        "tool_search" => "🧰",
        name if name.starts_with("mcp_") => "🔌",
        name if name.starts_with(eix_core::PLUGIN_TOOL_PREFIX) => "🧩",
        _ => "⚙",
    }
}

/// Classify the first program in a shell command. Pipes and `cd ... && ...`
/// chains are intentionally ignored: the leading program is the best hint and
/// anything more would guess at intent.
fn command_icon(command: &str) -> &'static str {
    let program = command.split_whitespace().next().unwrap_or_default();
    let program = program.rsplit('/').next().unwrap_or(program);
    match program {
        "cat" | "bat" | "sed" | "head" | "tail" | "nl" | "less" | "more" | "awk" => "📄",
        "rg" | "grep" | "ag" | "ack" => "🔎",
        "find" | "fd" | "ls" | "tree" | "eza" | "exa" => "📁",
        "cargo" | "make" | "cmake" | "rustc" | "npm" | "pnpm" | "yarn" | "pytest" | "go" => "🔨",
        "curl" | "wget" => "🌐",
        "git" => "⑂",
        _ => "❯",
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

/// Tool output, capped at [`OUTPUT_ROWS`] with a trailing ellipsis. Output can
/// contain formulas (for example from `pdftotext` or a notebook), so it uses
/// the math-aware text renderer.
fn push_output(out: &mut Transcript, text: &str, width: usize, theme: Theme, fold: Fold) {
    if text.is_empty() {
        return;
    }
    let mut rich = rich_text(
        text,
        width,
        theme,
        BODY_INDENT,
        Style::default().fg(text_color(theme)),
        true,
    );
    let limit = OUTPUT_ROWS;
    let truncated = rich.lines.len() > limit;
    rich.lines.truncate(limit);
    rich.math
        .retain(|placement| placement.line + usize::from(placement.image.rows) <= limit);
    let base = out.lines.len();
    for line in rich.lines {
        out.push(line, Some(fold));
    }
    for mut placement in rich.math {
        placement.line += base;
        out.math.push(placement);
    }
    rich.pending.retain(|pending| pending.line < limit);
    for mut pending in rich.pending {
        pending.line += base;
        out.math_pending.push(pending);
    }
    for (line, source) in rich.sources {
        if line < limit {
            out.copy.push((base + line, source));
        }
    }
    if truncated {
        out.push(
            Line::from(Span::styled(format!("{BODY_INDENT}…"), dim(theme))),
            Some(fold),
        );
    }
}

/// Reserve cells for a local image and pin the thumbnail to them. Used for
/// `view_image` results; unreadable or remote paths render nothing here.
fn push_thumbnail(
    out: &mut Transcript,
    path: &Path,
    base: Option<&Path>,
    width: usize,
    fold: Fold,
) {
    let prefix_width = BODY_INDENT.len();
    let avail = width.saturating_sub(prefix_width).max(1);
    let Some(image) =
        math_image::render_file(path, base, Some(avail.min(u16::MAX as usize) as u16), None)
    else {
        return;
    };
    let start = out.lines.len();
    let rows = usize::from(image.rows.max(1));
    let cols = usize::from(image.cols);
    let pad = avail.saturating_sub(cols) / 2;
    for _ in 0..rows {
        out.push(Line::from(Span::raw(BODY_INDENT)), Some(fold));
    }
    out.math.push(math_image::Placement {
        line: start,
        column: (prefix_width + pad).min(u16::MAX as usize) as u16,
        image,
    });
}

/// Thumbnail rows for user attachments inside a chat bubble. Attachments that
/// cannot be decoded keep the old `[image] path` line.
fn thumbnail_rows(
    images: &[ImageAttachment],
    width: usize,
    theme: Theme,
    base: Option<&Path>,
) -> (Vec<Line<'static>>, Vec<math_image::Placement>) {
    let mut lines = Vec::new();
    let mut placements = Vec::new();
    for attachment in images {
        let Some(image) = render_attachment(attachment, width, base) else {
            lines.push(Line::from(Span::styled(
                format!("[image] {}", image_label(attachment)),
                Style::default().fg(theme.comment.unwrap_or(MUTED)),
            )));
            continue;
        };
        let start = lines.len();
        let rows = usize::from(image.rows.max(1));
        let cols = usize::from(image.cols);
        let pad = width.saturating_sub(cols) / 2;
        for _ in 0..rows {
            lines.push(Line::from(""));
        }
        placements.push(math_image::Placement {
            line: start,
            column: pad.min(u16::MAX as usize) as u16,
            image,
        });
    }
    (lines, placements)
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::layout::Size;
    use ratatui_image::picker::Picker;
    use ratatui_image::Resize;

    fn test_image(cols: u16, rows: u16) -> math_image::MathImage {
        let picker = Picker::halfblocks();
        let decoded = image::DynamicImage::new_rgba8(u32::from(cols) * 10, u32::from(rows) * 20);
        let protocol = picker
            .new_protocol(decoded, Size::new(cols, rows), Resize::Fit(None))
            .expect("protocol");
        math_image::MathImage {
            protocol: std::sync::Arc::new(protocol),
            cols,
            rows,
        }
    }

    fn all_text(out: &Transcript) -> String {
        out.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn thinking_renders_display_math_instead_of_raw_tex() {
        let mut out = Transcript::new();
        let mut folds = HashMap::new();
        folds.insert(FoldId::Live, true);
        thinking(
            &mut out,
            FoldId::Live,
            "we need $$\\frac{a}{b}$$ here",
            false,
            60,
            Theme::default(),
            &folds,
            false,
        );
        let text = all_text(&out);
        assert!(!text.contains("$$"), "{text}");
        assert!(text.contains('─') || text.contains("a/b"), "{text}");
    }

    #[test]
    fn live_thinking_fades_at_the_window_edges() {
        let mut out = Transcript::new();
        let text = (1..=THINKING_WINDOW_ROWS)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        thinking(
            &mut out,
            FoldId::Live,
            &text,
            true,
            40,
            Theme::default(),
            &HashMap::new(),
            false,
        );
        let color = |row: usize| out.lines[row].spans.last().unwrap().style.fg;
        let top = color(1).unwrap();
        let middle = color(1 + THINKING_WINDOW_ROWS / 2).unwrap();
        let bottom = color(THINKING_WINDOW_ROWS).unwrap();
        assert_ne!(top, middle);
        assert_ne!(bottom, middle);
        assert_eq!(top, bottom);
    }

    #[test]
    fn tool_output_keeps_inline_math_literal() {
        let rich = rich_text(
            "cost $x^2$ vs $PATH and $HOME",
            60,
            Theme::default(),
            "",
            Style::default(),
            false,
        );
        let text = rich
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("$x^2$"), "{text}");
        assert!(text.contains("$PATH"), "{text}");
        assert!(text.contains("$HOME"), "{text}");
    }

    #[test]
    fn rich_text_keeps_plain_lines_and_math_together() {
        let rich = rich_text(
            "before\n$$x = 1$$\nafter",
            40,
            Theme::default(),
            "    ",
            Style::default(),
            false,
        );
        let text = rich
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("before"), "{text}");
        assert!(text.contains("after"), "{text}");
        assert!(!text.contains("$$"), "{text}");
    }

    #[test]
    fn raw_math_keeps_display_math_literal() {
        let rich = rich_text_inner(
            "before\n$$x = 1$$\nafter",
            40,
            Theme::default(),
            "    ",
            Style::default(),
            true,
            true,
        );
        let text = rich
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("$$x = 1$$"), "{text}");
        assert!(rich.math.is_empty(), "{text}");
    }

    #[test]
    fn assistant_image_parts_keep_a_label_when_undrawable() {
        let events = vec![SessionEvent {
            session_id: "s".into(),
            seq: 1,
            ts: 0,
            kind: EventKind::AssistantMessage,
            payload: serde_json::json!({
                "text": "",
                "model": "m",
                "images": [
                    {"path": "", "mime": "image/png", "data": "QUJD", "bytes": 3},
                    {"path": "https://example.test/a.png", "mime": ""}
                ]
            }),
        }];
        let out = render_events(&events, 60, Theme::default(), None);
        let text = all_text(&out);
        // No terminal graphics service in unit tests: both images keep a
        // one-line label instead of vanishing with the empty text.
        assert!(text.contains("[image] inline image"), "{text}");
        assert!(
            text.contains("[image] https://example.test/a.png"),
            "{text}"
        );
    }

    #[test]
    fn user_attachments_accept_inline_payloads() {
        let attachment = ImageAttachment {
            path: String::new(),
            mime: "image/png".into(),
            data: "QUJD".into(),
            blob: None,
            bytes: Some(3),
        };
        let mut out = Transcript::new();
        user_message(
            &mut out,
            "",
            std::slice::from_ref(&attachment),
            40,
            Theme::default(),
            None,
        );
        let text = all_text(&out);
        assert!(text.contains("[image] inline image"), "{text}");
        assert!(!text.contains("QUJD"), "base64 must stay hidden: {text}");
    }

    #[test]
    fn bubble_keeps_image_only_turns() {
        let mut out = Transcript::new();
        // User attachments reserve their rows with empty placeholder lines.
        let content = vec![Line::from(""), Line::from("")];
        let placement = math_image::Placement {
            line: 0,
            column: 0,
            image: test_image(2, 2),
        };
        push_bubble(
            &mut out,
            content,
            Vec::new(),
            vec![placement],
            Vec::new(),
            Vec::new(),
            40,
            Theme::default(),
        );
        let placement = out.math.first().expect("image-only placement");
        // The top border occupies line 0; both placeholder rows survive.
        assert_eq!(placement.line, 1);
        assert_eq!(placement.column, 36);
    }

    #[test]
    fn bubble_shifts_links_into_the_box() {
        let mut out = Transcript::new();
        let content = vec![Line::from("text")];
        let link = markdown::LinkSpan {
            line: 0,
            start: 0,
            end: 4,
            url: "https://e.com".to_string(),
        };
        push_bubble(
            &mut out,
            content,
            Vec::new(),
            Vec::new(),
            Vec::new(),
            vec![link],
            40,
            Theme::default(),
        );
        let link = out.links.first().expect("link");
        // Top border at line 0; the box's left padding precedes the content.
        assert_eq!(link.line, 1);
        assert_eq!((link.start, link.end), (34, 38));
    }

    #[test]
    fn tool_icons_classify_tools_and_leading_commands() {
        let command = |cmd: &str| tool_icon("exec_command", &serde_json::json!({ "cmd": cmd }));
        assert_eq!(command("cat src/main.rs"), "📄");
        assert_eq!(command("sed -n '1,20p' src/lib.rs"), "📄");
        assert_eq!(command("rg -n TODO src"), "🔎");
        assert_eq!(command("ls -la"), "📁");
        assert_eq!(command("cargo test -p eix-tui"), "🔨");
        assert_eq!(command("curl https://example.com"), "🌐");
        assert_eq!(command("/usr/bin/git status"), "⑂");
        assert_eq!(command("./scripts/deploy.sh"), "❯");
        assert_eq!(command(""), "❯");

        assert_eq!(tool_icon("apply_patch", &Value::Null), "✎");
        assert_eq!(tool_icon("web_search", &Value::Null), "🔍");
        assert_eq!(tool_icon("web_fetch", &Value::Null), "🌐");
        assert_eq!(tool_icon("spawn_agent", &Value::Null), "🤖");
        assert_eq!(tool_icon("mcp__github__create_issue", &Value::Null), "🔌");
        assert_eq!(
            tool_icon(
                concat!("plugin__", "demo__deploy"),
                &serde_json::json!({ "cmd": "cat x" })
            ),
            "🧩"
        );
    }

    #[test]
    fn icon_cell_keeps_a_two_cell_column() {
        assert_eq!(UnicodeWidthStr::width(icon_cell("📄").as_str()), 3);
        assert_eq!(UnicodeWidthStr::width(icon_cell("❯").as_str()), 3);
        assert_eq!(UnicodeWidthStr::width(icon_cell("⑂").as_str()), 3);
    }

    #[test]
    fn tool_header_places_the_icon_before_the_summary() {
        let mut out = Transcript::new();
        let block = ToolBlock {
            seq: 1,
            call_id: "c1".into(),
            tool: "exec_command".into(),
            arguments: serde_json::json!({ "cmd": "rg -n TODO src" }),
            result: None,
            approval: None,
            patch: None,
        };
        tool_block(
            &mut out,
            &block,
            60,
            Theme::default(),
            &HashMap::new(),
            None,
        );
        let header = all_text(&out);
        assert!(header.contains("▾ ◐ 🔎 $ rg -n TODO src"), "{header}");
    }

    #[test]
    fn thinking_header_uses_the_brain_icon() {
        let mut out = Transcript::new();
        let mut folds = HashMap::new();
        folds.insert(FoldId::Live, true);
        thinking(
            &mut out,
            FoldId::Live,
            "plan",
            true,
            60,
            Theme::default(),
            &folds,
            false,
        );
        let text = all_text(&out);
        assert!(text.contains("🧠 Thinking…"), "{text}");
    }

    #[test]
    fn bubble_shifts_math_images_into_the_box() {
        let mut out = Transcript::new();
        let content = vec![Line::from("text"), Line::from(" ")];
        let placement = math_image::Placement {
            line: 1,
            column: 2,
            image: test_image(2, 1),
        };
        push_bubble(
            &mut out,
            content,
            Vec::new(),
            vec![placement],
            Vec::new(),
            Vec::new(),
            40,
            Theme::default(),
        );
        let placement = out.math.first().expect("placement");
        // Top border at line 0, so the content image lands on line 2.
        assert_eq!(placement.line, 2);
        // content_width(6) leaves body_pad(30); then left border, bubble
        // padding and the original column(2).
        assert_eq!(placement.column, 34);
    }
}
