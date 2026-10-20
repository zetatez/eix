//! Chat-style transcript rendering.
//!
//! The transcript is a conversation, not a log: user turns hug the right edge,
//! agent output sits on the left. The agent's chain of thought and every tool
//! invocation render as one-line headers that expand into their details, and
//! the same line map powers click-to-fold. Blocks still streaming live under a
//! synthetic [`FoldId::Live`] key so they collapse once the turn is stored.

use std::collections::HashMap;

use eix_core::{
    ApprovalRequiredPayload, AssistantMessagePayload, ContextCompactedPayload,
    Event as SessionEvent, EventKind, PatchProposedPayload, PatchResultPayload, PermissionPayload,
    ToolCallPayload, ToolResultPayload, UserMessagePayload,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

use super::app::{tool_call_summary, truncate, wrap_text};
use super::theme::Theme;

/// Tool output is capped so a verbose command cannot flood the transcript.
const OUTPUT_ROWS: usize = 40;
/// Diff previews are capped the same way.
const DIFF_ROWS: usize = 40;

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

/// One rendered line plus the block that owns it (for click-to-fold).
pub struct Transcript {
    pub lines: Vec<Line<'static>>,
    pub slots: Vec<Option<Fold>>,
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
    Permission {
        text: String,
    },
    Note {
        text: String,
    },
    Error {
        text: String,
    },
    LiveThinking {
        text: String,
    },
    LiveText {
        text: String,
    },
}

/// Render a session's events (plus any in-flight deltas) into chat lines.
pub fn render(
    events: &[SessionEvent],
    live: Option<Live<'_>>,
    queued: &[String],
    folds: &HashMap<FoldId, bool>,
    width: usize,
    theme: Theme,
) -> Transcript {
    // Keep a two-cell gutter on the right so right-aligned bubbles, tool
    // statuses and queued prompts never touch the pane border.
    let width = width.saturating_sub(2).max(1);
    let blocks = collect(events, live);
    let mut out = Transcript {
        lines: Vec::new(),
        slots: Vec::new(),
    };
    for block in blocks {
        match block {
            Block::User { text, images } => user_message(&mut out, &text, &images, width, theme),
            Block::Assistant {
                seq,
                text,
                reasoning,
                cancelled,
            } => {
                if let Some(reasoning) = reasoning.as_deref().filter(|r| !r.trim().is_empty()) {
                    thinking(
                        &mut out,
                        FoldId::Event(seq),
                        reasoning,
                        false,
                        width,
                        theme,
                        folds,
                    );
                }
                if !text.is_empty() {
                    assistant_message(&mut out, &text, cancelled, width, theme);
                }
            }
            Block::Tool(block) => tool_block(&mut out, &block, width, theme, folds),
            Block::Permission { text } => {
                note(&mut out, '◆', &text, theme.function.unwrap_or(WARN), width)
            }
            Block::Note { text } => note(&mut out, '◇', &text, comment(theme), width),
            Block::Error { text } => {
                note(&mut out, '!', &text, theme.error.unwrap_or(DANGER), width)
            }
            Block::LiveThinking { text } => {
                thinking(&mut out, FoldId::Live, &text, true, width, theme, folds);
            }
            Block::LiveText { text } => assistant_message(&mut out, &text, false, width, theme),
        }
    }
    if !queued.is_empty() {
        out.blank();
        for text in queued {
            queued_prompt(&mut out, text, width, theme);
        }
    }
    if out.lines.is_empty() {
        out.push(
            Line::from(Span::styled(
                "  no messages yet — type a prompt below",
                dim(theme),
            )),
            None,
        );
    }
    out
}

/// Flatten the event log into ordered blocks, merging tool call/results.
fn collect(events: &[SessionEvent], live: Option<Live<'_>>) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    let mut tool_index: Vec<(String, usize)> = Vec::new();

    for event in events {
        match event.kind {
            EventKind::UserMessage => {
                if let Ok(payload) =
                    serde_json::from_value::<UserMessagePayload>(event.payload.clone())
                {
                    blocks.push(Block::User {
                        text: payload.text,
                        images: payload.images.into_iter().map(|image| image.path).collect(),
                    });
                }
            }
            EventKind::AssistantMessage => {
                if let Ok(payload) =
                    serde_json::from_value::<AssistantMessagePayload>(event.payload.clone())
                {
                    blocks.push(Block::Assistant {
                        seq: event.seq,
                        text: payload.text,
                        reasoning: payload.reasoning,
                        cancelled: payload.cancelled,
                    });
                }
            }
            EventKind::ToolCall => {
                if let Ok(payload) =
                    serde_json::from_value::<ToolCallPayload>(event.payload.clone())
                {
                    let slot = blocks.len();
                    tool_index.push((payload.call_id.clone(), slot));
                    blocks.push(Block::Tool(Box::new(ToolBlock {
                        seq: event.seq,
                        tool: payload.tool,
                        arguments: payload.arguments,
                        result: None,
                        approval: None,
                        patch: None,
                    })));
                }
            }
            EventKind::ApprovalRequired => {
                if let Ok(payload) =
                    serde_json::from_value::<ApprovalRequiredPayload>(event.payload.clone())
                {
                    match tool_slot(&blocks, &tool_index, &payload.call_id) {
                        Some(slot) => {
                            if let Block::Tool(block) = &mut blocks[slot] {
                                block.approval = Some(payload);
                            }
                        }
                        None => {
                            let arguments = payload.arguments.clone();
                            let tool = payload.tool.clone();
                            blocks.push(Block::Tool(Box::new(ToolBlock {
                                seq: event.seq,
                                tool,
                                arguments,
                                result: None,
                                approval: Some(payload),
                                patch: None,
                            })))
                        }
                    }
                }
            }
            EventKind::PatchProposed => {
                if let Ok(payload) =
                    serde_json::from_value::<PatchProposedPayload>(event.payload.clone())
                {
                    let patch = PatchBlock {
                        path: payload.path,
                        diff: payload.diff,
                        outcome: None,
                    };
                    match tool_slot(&blocks, &tool_index, &payload.call_id) {
                        Some(slot) => {
                            if let Block::Tool(block) = &mut blocks[slot] {
                                block.patch = Some(patch);
                            }
                        }
                        None => blocks.push(Block::Tool(Box::new(ToolBlock {
                            seq: event.seq,
                            tool: payload.tool,
                            arguments: Value::Null,
                            result: None,
                            approval: None,
                            patch: Some(patch),
                        }))),
                    }
                }
            }
            EventKind::PatchApplied | EventKind::PatchRejected | EventKind::PatchConflict => {
                if let Ok(payload) =
                    serde_json::from_value::<PatchResultPayload>(event.payload.clone())
                {
                    let outcome = match event.kind {
                        EventKind::PatchApplied => PatchOutcome::Applied,
                        EventKind::PatchRejected => PatchOutcome::Rejected,
                        _ => PatchOutcome::Conflict,
                    };
                    match tool_slot(&blocks, &tool_index, &payload.call_id) {
                        Some(slot) => {
                            if let Block::Tool(block) = &mut blocks[slot] {
                                match &mut block.patch {
                                    Some(patch) => patch.outcome = Some(outcome),
                                    None => {
                                        block.patch = Some(PatchBlock {
                                            path: payload.path,
                                            diff: None,
                                            outcome: Some(outcome),
                                        })
                                    }
                                }
                            }
                        }
                        None => blocks.push(Block::Tool(Box::new(ToolBlock {
                            seq: event.seq,
                            tool: "write_file".into(),
                            arguments: Value::Null,
                            result: None,
                            approval: None,
                            patch: Some(PatchBlock {
                                path: payload.path,
                                diff: None,
                                outcome: Some(outcome),
                            }),
                        }))),
                    }
                }
            }
            EventKind::ToolResult => {
                if let Ok(payload) =
                    serde_json::from_value::<ToolResultPayload>(event.payload.clone())
                {
                    match tool_slot(&blocks, &tool_index, &payload.call_id) {
                        Some(slot) => {
                            if let Block::Tool(block) = &mut blocks[slot] {
                                block.result = Some(payload);
                            }
                        }
                        None => blocks.push(Block::Tool(Box::new(ToolBlock {
                            seq: event.seq,
                            tool: payload.tool.clone(),
                            arguments: Value::Null,
                            result: Some(payload),
                            approval: None,
                            patch: None,
                        }))),
                    }
                }
            }
            EventKind::Permission => {
                if let Ok(payload) =
                    serde_json::from_value::<PermissionPayload>(event.payload.clone())
                {
                    blocks.push(Block::Permission {
                        text: format!(
                            "permission {} → {}{}",
                            payload.from.as_str(),
                            payload.to.as_str(),
                            if payload.temporary {
                                " (temporary)"
                            } else {
                                ""
                            }
                        ),
                    });
                }
            }
            EventKind::ContextCompacted => {
                if let Ok(payload) =
                    serde_json::from_value::<ContextCompactedPayload>(event.payload.clone())
                {
                    blocks.push(Block::Note {
                        text: format!(
                            "context compacted · {} message(s) replaced by a summary",
                            payload.messages
                        ),
                    });
                }
            }
            EventKind::ContextCleared => {
                blocks.push(Block::Note {
                    text: "context cleared · earlier turns stay here but are no longer sent".into(),
                });
            }
            EventKind::Error => {
                blocks.push(Block::Error {
                    text: event
                        .payload
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_string(),
                });
            }
            _ => {}
        }
    }

    if let Some(live) = live {
        if !live.reasoning.is_empty() {
            blocks.push(Block::LiveThinking {
                text: live.reasoning.to_string(),
            });
        }
        if !live.text.is_empty() {
            blocks.push(Block::LiveText {
                text: live.text.to_string(),
            });
        } else if live.reasoning.is_empty() {
            blocks.push(Block::LiveText { text: "…".into() });
        }
    }
    blocks
}

fn tool_slot(blocks: &[Block], index: &[(String, usize)], call_id: &str) -> Option<usize> {
    index
        .iter()
        .rev()
        .find(|(id, _)| id == call_id)
        .map(|(_, slot)| *slot)
        .filter(|slot| matches!(blocks.get(*slot), Some(Block::Tool(_))))
}

impl Transcript {
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
const PANEL: Color = Color::Rgb(38, 41, 51);
const PANEL_BORDER: Color = Color::Rgb(74, 88, 120);
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

fn panel(theme: Theme) -> Color {
    theme.panel.unwrap_or(PANEL)
}

fn dim(theme: Theme) -> Style {
    Style::default().fg(comment(theme))
}

fn italic_dim(theme: Theme) -> Style {
    Style::default()
        .fg(comment(theme))
        .add_modifier(Modifier::ITALIC)
}

/// A user turn in a right-aligned rounded bubble. The bubble shrinks to fit
/// short prompts and wraps long ones at four fifths of the pane.
fn user_message(out: &mut Transcript, text: &str, images: &[String], width: usize, theme: Theme) {
    for path in images {
        out.push(
            Line::from(Span::styled(
                format!("  [image] {path}"),
                Style::default().fg(theme.comment.unwrap_or(MUTED)),
            )),
            None,
        );
    }
    let text = text.trim_end();
    if text.is_empty() {
        out.blank();
        return;
    }
    // Below this width a bubble costs more than it communicates.
    if width < 14 {
        for segment in wrap_text(text, width.saturating_sub(2).max(1)) {
            out.push(
                Line::from(vec![
                    Span::raw("  "),
                    Span::styled(segment, Style::default().fg(theme.string.unwrap_or(ACCENT))),
                ]),
                None,
            );
        }
        out.blank();
        return;
    }

    let max_content = (width.saturating_mul(4) / 5).saturating_sub(4).max(6);
    let wrapped = wrap_text(text, max_content);
    let inner = wrapped
        .iter()
        .map(|line| UnicodeWidthStr::width(line.as_str()))
        .max()
        .unwrap_or(1)
        .min(max_content)
        .max(1);
    let total = inner + 4;
    let left = width.saturating_sub(total);
    let border = Style::default()
        .fg(theme.function.unwrap_or(PANEL_BORDER))
        .bg(panel(theme));
    let body = Style::default().fg(text_color(theme)).bg(panel(theme));
    let pad = Span::raw(" ".repeat(left));

    out.blank();
    out.push(
        Line::from(vec![
            pad.clone(),
            Span::styled(format!("╭{}╮", "─".repeat(inner + 2)), border),
        ]),
        None,
    );
    for segment in &wrapped {
        let fill = inner.saturating_sub(UnicodeWidthStr::width(segment.as_str()));
        out.push(
            Line::from(vec![
                pad.clone(),
                Span::styled(format!("│ {segment}{} │", " ".repeat(fill)), body),
            ]),
            None,
        );
    }
    out.push(
        Line::from(vec![
            pad,
            Span::styled(format!("╰{}╯", "─".repeat(inner + 2)), border),
        ]),
        None,
    );
    out.blank();
}

/// Agent output: indented only enough to breathe, no role label.
fn assistant_message(
    out: &mut Transcript,
    text: &str,
    cancelled: bool,
    width: usize,
    theme: Theme,
) {
    let wrapped = wrap_text(text.trim_end_matches('\n'), width.saturating_sub(2).max(1));
    let last = wrapped.len();
    let style = Style::default().fg(text_color(theme));
    for (index, segment) in wrapped.into_iter().enumerate() {
        let mut spans = vec![Span::raw("  "), Span::styled(segment, style)];
        if cancelled && index + 1 == last {
            spans.push(Span::styled("  · cancelled", dim(theme)));
        }
        out.push(Line::from(spans), None);
    }
    out.blank();
}

/// A one-line status note (permissions, errors).
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
    let open = folds.get(&id).copied().unwrap_or(streaming);
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

/// A tool invocation: a compact status line that expands into args, approval
/// diff and output with an aligned gutter.
fn tool_block(
    out: &mut Transcript,
    block: &ToolBlock,
    width: usize,
    theme: Theme,
    folds: &HashMap<FoldId, bool>,
) {
    let id = FoldId::Event(block.seq);
    // In-flight calls open by default so their arguments are visible; finished
    // ones collapse to the summary line.
    let default_open = block.result.is_none() && block.approval.is_none();
    let open = folds.get(&id).copied().unwrap_or(default_open);
    let fold = Fold { id, open };

    let color = status_color(block, theme);
    let status = status_label(block);
    let status_width = UnicodeWidthStr::width(status);
    let prefix_width = 6; // "  ▾ ● "
    let summary = tool_summary(block);
    let room = width.saturating_sub(prefix_width + status_width + 1).max(8);
    let summary = truncate(&summary, room);
    let used = prefix_width + UnicodeWidthStr::width(summary.as_str()) + status_width;
    let pad = width.saturating_sub(used).max(1);

    out.push(
        Line::from(vec![
            Span::styled(format!("  {} ", if open { "▾" } else { "▸" }), dim(theme)),
            Span::styled(
                format!("{} ", status_dot(block)),
                Style::default().fg(color),
            ),
            Span::styled(
                summary,
                Style::default()
                    .fg(text_color(theme))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(" ".repeat(pad)),
            Span::styled(status, Style::default().fg(color)),
        ]),
        Some(fold),
    );

    if open {
        let body = width.saturating_sub(6).max(1);
        push_detail(out, "input", &detail_text(block), body, theme, fold);
        if let Some(approval) = &block.approval {
            push_detail(out, "review", &approval.summary, body, theme, fold);
            if let Some(diff) = &approval.diff {
                push_diff(out, diff, body, theme, fold);
            }
        }
        if let Some(patch) = &block.patch {
            push_detail(out, "patch", &patch.path, body, theme, fold);
            if let Some(diff) = &patch.diff {
                push_diff(out, diff, body, theme, fold);
            }
        }
        // A write with no staged diff still shows what is being written.
        if block.patch.is_none() && block.tool == "write_file" {
            if let Some(content) = block
                .arguments
                .get("content")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
            {
                push_output(out, "content", content.trim_end(), body, theme, fold);
            }
        }
        if let Some(result) = &block.result {
            let duration = result
                .duration_ms
                .map(|ms| format!(" ({ms}ms)"))
                .unwrap_or_default();
            let label = if result.denied {
                "denied".to_string()
            } else if result.ok {
                format!("output{duration}")
            } else {
                format!("error{duration}")
            };
            push_output(out, &label, result.output.trim_end(), body, theme, fold);
        } else if block.approval.is_some() {
            push_detail(
                out,
                "waiting",
                "approval required — press y to approve, n to deny",
                body,
                theme,
                fold,
            );
        } else {
            push_detail(out, "running", "…", body, theme, fold);
        }
    }
    out.blank();
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
            Line::from(vec![lead, Span::styled(segment, dim(theme))]),
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
    let summary = if block.arguments.is_null() {
        if let Some(patch) = &block.patch {
            format!("{} {}", block.tool, patch.path)
        } else {
            block.tool.clone()
        }
    } else {
        tool_call_summary(&block.tool, &block.arguments)
    };
    truncate(&summary, 100)
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

const LABEL_WIDTH: usize = 8;
const GUTTER: &str = "    │ ";

/// The expanded argument detail, tailored to the tool.
fn detail_text(block: &ToolBlock) -> String {
    let string = |key: &str| {
        block
            .arguments
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    match block.tool.as_str() {
        "run_command" | "start_command" => {
            let cwd = string("cwd");
            let command = string("command");
            if cwd.is_empty() {
                format!("$ {command}")
            } else {
                format!("$ {command}   (cwd: {cwd})")
            }
        }
        "read_file" | "list_dir" => string("path").to_string(),
        "write_file" => String::new(),
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

fn detail_prefix(label: &str) -> String {
    format!("{GUTTER}{label:<LABEL_WIDTH$} ")
}

fn push_detail(
    out: &mut Transcript,
    label: &str,
    text: &str,
    width: usize,
    theme: Theme,
    fold: Fold,
) {
    if text.is_empty() {
        return;
    }
    let prefix = detail_prefix(label);
    let prefix_width = UnicodeWidthStr::width(prefix.as_str());
    let wrapped = wrap_text(text, width.saturating_sub(prefix_width).max(1));
    for (index, segment) in wrapped.iter().enumerate() {
        let lead = if index == 0 {
            Span::styled(prefix.clone(), dim(theme))
        } else {
            Span::raw(" ".repeat(prefix_width))
        };
        out.push(
            Line::from(vec![
                lead,
                Span::styled(segment.clone(), Style::default().fg(text_color(theme))),
            ]),
            Some(fold),
        );
    }
}

/// Tool output, capped at [`OUTPUT_ROWS`] with a trailing ellipsis.
fn push_output(
    out: &mut Transcript,
    label: &str,
    text: &str,
    width: usize,
    theme: Theme,
    fold: Fold,
) {
    if text.is_empty() {
        return;
    }
    let prefix = detail_prefix(label);
    let prefix_width = UnicodeWidthStr::width(prefix.as_str());
    let continuation = " ".repeat(prefix_width);
    let lines: Vec<&str> = text.lines().collect();
    for (line_index, line) in lines.iter().take(OUTPUT_ROWS).enumerate() {
        let wrapped = wrap_text(line, width.saturating_sub(prefix_width).max(1));
        for (index, segment) in wrapped.iter().enumerate() {
            let lead = if line_index == 0 && index == 0 {
                Span::styled(prefix.clone(), dim(theme))
            } else {
                Span::raw(continuation.clone())
            };
            out.push(
                Line::from(vec![
                    lead,
                    Span::styled(segment.clone(), Style::default().fg(text_color(theme))),
                ]),
                Some(fold),
            );
        }
    }
    if lines.len() > OUTPUT_ROWS {
        out.push(
            Line::from(Span::styled(format!("{GUTTER}…"), dim(theme))),
            Some(fold),
        );
    }
}

/// Diff preview with `+`/`-`/`@@` coloring, capped at [`DIFF_ROWS`].
fn push_diff(out: &mut Transcript, diff: &str, width: usize, theme: Theme, fold: Fold) {
    let add = theme.diff_add.unwrap_or(SUCCESS);
    let delete = theme.diff_delete.unwrap_or(DANGER);
    let lines: Vec<&str> = diff.lines().collect();
    for line in lines.iter().take(DIFF_ROWS) {
        let color = if line.starts_with('+') && !line.starts_with("+++") {
            add
        } else if line.starts_with('-') && !line.starts_with("---") {
            delete
        } else if line.starts_with("@@") {
            accent(theme)
        } else {
            comment(theme)
        };
        for segment in wrap_text(line, width.saturating_sub(GUTTER.len()).max(1)) {
            out.push(
                Line::from(vec![
                    Span::styled(GUTTER, dim(theme)),
                    Span::styled(segment, Style::default().fg(color)),
                ]),
                Some(fold),
            );
        }
    }
    if lines.len() > DIFF_ROWS {
        out.push(
            Line::from(Span::styled(format!("{GUTTER}…"), dim(theme))),
            Some(fold),
        );
    }
}
