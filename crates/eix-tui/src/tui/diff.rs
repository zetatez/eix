//! Vim-style side-by-side diff rows and rendering.
//!
//! The daemon sends unified diffs (`-`/`+`/context lines). For display we pair
//! each run of deletions with the corresponding additions, so the old file is
//! on the left and the new file is on the right. Hunk headers stay full-width
//! so they can carry the review checkbox.

use eix_core::HunkInfo;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::syntax::{luminance, rgb, CodeHighlighter};
use super::theme::Theme;

const SEPARATOR: &str = " │ ";
const MIN_TEXT_WIDTH: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DiffRow {
    /// A hunk header. `index` is set when the header maps to a selectable hunk.
    Hunk {
        index: Option<usize>,
        header: String,
    },
    /// A non-diff line, e.g. "diff omitted" for a huge file.
    Meta(String),
    /// One aligned row. Either side can be empty.
    Line {
        old_no: Option<usize>,
        new_no: Option<usize>,
        old: Option<String>,
        new: Option<String>,
    },
}

/// Build aligned rows for a review card. Hunk previews are authoritative when
/// present; otherwise parse the raw unified diff.
pub(super) fn rows(hunks: &[HunkInfo], diff: Option<&str>) -> Vec<DiffRow> {
    if !hunks.is_empty() {
        let mut rows = Vec::new();
        for (index, hunk) in hunks.iter().enumerate() {
            rows.push(DiffRow::Hunk {
                index: Some(index),
                header: hunk.header.clone(),
            });
            let body = hunk_body(hunk);
            let (old_start, new_start) = parse_header(&hunk.header)
                .map(|header| (header.old_start, header.new_start))
                .unwrap_or((1, 1));
            push_lines(&mut rows, &body, old_start, new_start);
        }
        return rows;
    }
    let Some(diff) = diff else {
        return Vec::new();
    };
    parse_unified(diff)
}

/// First display row of every hunk, for `[`/`]` navigation.
pub(super) fn hunk_offsets(hunks: &[HunkInfo]) -> Vec<usize> {
    let mut offsets = Vec::with_capacity(hunks.len());
    let mut line = 0;
    for hunk in hunks {
        offsets.push(line);
        line += 1 + body_row_count(hunk);
    }
    offsets
}

fn body_row_count(hunk: &HunkInfo) -> usize {
    let body = hunk_body(hunk);
    let (old_start, new_start) = parse_header(&hunk.header)
        .map(|header| (header.old_start, header.new_start))
        .unwrap_or((1, 1));
    let mut rows = Vec::new();
    push_lines(&mut rows, &body, old_start, new_start);
    rows.len()
}

fn hunk_body(hunk: &HunkInfo) -> Vec<&str> {
    let mut lines: Vec<&str> = hunk.lines.iter().map(String::as_str).collect();
    if lines.first().is_some_and(|line| line.starts_with("@@")) {
        lines.remove(0);
    }
    lines
}

fn parse_unified(diff: &str) -> Vec<DiffRow> {
    let mut rows = Vec::new();
    let mut current: Option<HunkHeader> = None;
    let mut old_left = 0;
    let mut new_left = 0;
    let mut body: Vec<&str> = Vec::new();
    for line in diff.lines() {
        if line.starts_with("@@") {
            flush_hunk(&mut rows, &mut body, current);
            match parse_header(line) {
                Some(header) => {
                    rows.push(DiffRow::Hunk {
                        index: None,
                        header: line.to_string(),
                    });
                    old_left = header.old_count;
                    new_left = header.new_count;
                    current = Some(header);
                }
                None => {
                    rows.push(DiffRow::Meta(line.to_string()));
                    current = None;
                }
            }
            continue;
        }
        if current.is_some() && (old_left > 0 || new_left > 0) {
            body.push(line);
            match line.chars().next() {
                Some(' ') => {
                    old_left = old_left.saturating_sub(1);
                    new_left = new_left.saturating_sub(1);
                }
                Some('-') => old_left = old_left.saturating_sub(1),
                Some('+') => new_left = new_left.saturating_sub(1),
                _ => {}
            }
            continue;
        }
        if current.is_some() {
            flush_hunk(&mut rows, &mut body, current);
            current = None;
        }
        if !line.starts_with("diff --git")
            && !line.starts_with("index ")
            && !line.starts_with("---")
            && !line.starts_with("+++")
            && !line.trim().is_empty()
        {
            rows.push(DiffRow::Meta(line.to_string()));
        }
    }
    flush_hunk(&mut rows, &mut body, current);
    rows
}

fn flush_hunk(rows: &mut Vec<DiffRow>, body: &mut Vec<&str>, header: Option<HunkHeader>) {
    if let Some(header) = header {
        if !body.is_empty() {
            push_lines(rows, body, header.old_start, header.new_start);
            body.clear();
        }
    }
}

#[derive(Clone, Copy)]
struct HunkHeader {
    old_start: usize,
    new_start: usize,
    old_count: usize,
    new_count: usize,
}

fn parse_header(header: &str) -> Option<HunkHeader> {
    let mut parts = header.split_whitespace();
    if parts.next()? != "@@" {
        return None;
    }
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let (old_start, old_count) = parse_range(old);
    let (new_start, new_count) = parse_range(new);
    Some(HunkHeader {
        old_start,
        new_start,
        old_count,
        new_count,
    })
}

fn parse_range(range: &str) -> (usize, usize) {
    let mut parts = range.split(',');
    let start = parts
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    let count = parts
        .next()
        .and_then(|value| value.parse().ok())
        .unwrap_or(1);
    (start, count)
}

fn push_lines(rows: &mut Vec<DiffRow>, lines: &[&str], old_start: usize, new_start: usize) {
    let mut old_no = old_start;
    let mut new_no = new_start;
    let mut deletions: Vec<(usize, String)> = Vec::new();
    let mut additions: Vec<(usize, String)> = Vec::new();

    for line in lines {
        let mut chars = line.chars();
        let sign = chars.next();
        let text = chars.as_str();
        match sign {
            Some(' ') => {
                flush_changes(rows, &mut deletions, &mut additions);
                rows.push(DiffRow::Line {
                    old_no: Some(old_no),
                    new_no: Some(new_no),
                    old: Some(text.to_string()),
                    new: Some(text.to_string()),
                });
                old_no += 1;
                new_no += 1;
            }
            Some('-') => {
                deletions.push((old_no, text.to_string()));
                old_no += 1;
            }
            Some('+') => {
                additions.push((new_no, text.to_string()));
                new_no += 1;
            }
            // "\ No newline at end of file" has no side-by-side content.
            Some('\\') => {}
            // Be forgiving of a diff that lost its leading context space.
            None => {
                flush_changes(rows, &mut deletions, &mut additions);
                rows.push(DiffRow::Line {
                    old_no: Some(old_no),
                    new_no: Some(new_no),
                    old: Some(String::new()),
                    new: Some(String::new()),
                });
                old_no += 1;
                new_no += 1;
            }
            Some(_) => {
                flush_changes(rows, &mut deletions, &mut additions);
                rows.push(DiffRow::Meta(line.to_string()));
            }
        }
    }
    flush_changes(rows, &mut deletions, &mut additions);
}

fn flush_changes(
    rows: &mut Vec<DiffRow>,
    deletions: &mut Vec<(usize, String)>,
    additions: &mut Vec<(usize, String)>,
) {
    let paired = deletions.len().max(additions.len());
    for index in 0..paired {
        let (old_no, old) = deletions
            .get(index)
            .map(|(number, text)| (Some(*number), Some(text.clone())))
            .unwrap_or((None, None));
        let (new_no, new) = additions
            .get(index)
            .map(|(number, text)| (Some(*number), Some(text.clone())))
            .unwrap_or((None, None));
        rows.push(DiffRow::Line {
            old_no,
            new_no,
            old,
            new,
        });
    }
    deletions.clear();
    additions.clear();
}

/// Background used by transcript diff blocks. Presets normally set a real
/// background; when they only set body text, infer the terminal polarity so
/// syntax colors and the green/red tints agree.
pub(super) fn base_background(theme: Theme) -> Color {
    theme.background.or(theme.panel).unwrap_or_else(|| {
        if theme.normal.is_some_and(|color| luminance(color) < 0.4) {
            Color::White
        } else {
            Color::Black
        }
    })
}

fn layout(rows: &[DiffRow], width: usize, indent: &str) -> Option<(usize, usize)> {
    let indent_width = UnicodeWidthStr::width(indent);
    let available = width.saturating_sub(indent_width);
    let separator_width = UnicodeWidthStr::width(SEPARATOR);
    let col_width = available.saturating_sub(separator_width) / 2;
    let max_no = rows
        .iter()
        .filter_map(|row| match row {
            DiffRow::Line {
                old_no: Some(old),
                new_no: Some(new),
                ..
            } => Some((*old).max(*new)),
            DiffRow::Line {
                old_no: Some(old), ..
            } => Some(*old),
            DiffRow::Line {
                new_no: Some(new), ..
            } => Some(*new),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    let num_width = max_no.to_string().len().max(3);
    // "number space marker space"
    let prefix_width = num_width + 3;
    let text_width = col_width.saturating_sub(prefix_width);
    (text_width >= MIN_TEXT_WIDTH).then_some((num_width, text_width))
}

/// Renders aligned rows with one stateful syntax highlighter per side.
pub(super) struct DiffRenderer {
    indent: String,
    theme: Theme,
    base_bg: Color,
    add_bg: Color,
    delete_bg: Color,
    num_width: usize,
    text_width: usize,
    full_width: usize,
    old_highlighter: CodeHighlighter,
    new_highlighter: CodeHighlighter,
}

impl DiffRenderer {
    /// Whether both sides can get a readable code column at this width.
    pub(super) fn can_render(rows: &[DiffRow], width: usize, indent: &str) -> bool {
        layout(rows, width, indent).is_some()
    }

    /// `None` when the width cannot give both sides a readable code column.
    pub(super) fn new(
        rows: &[DiffRow],
        width: usize,
        path: &str,
        theme: Theme,
        indent: &str,
        base_bg: Color,
    ) -> Option<Self> {
        let (num_width, text_width) = layout(rows, width, indent)?;
        let (add_bg, delete_bg) = diff_backgrounds(base_bg);
        Some(Self {
            indent: indent.to_string(),
            theme,
            base_bg,
            add_bg,
            delete_bg,
            num_width,
            text_width,
            full_width: width.saturating_sub(UnicodeWidthStr::width(indent)),
            old_highlighter: CodeHighlighter::new(path, theme),
            new_highlighter: CodeHighlighter::new(path, theme),
        })
    }

    pub(super) fn render(
        &mut self,
        rows: &[DiffRow],
        interactive: bool,
        selected: &[bool],
        cursor: usize,
    ) -> Vec<Line<'static>> {
        rows.iter()
            .map(|row| match row {
                DiffRow::Hunk { index, header } => {
                    self.hunk_line(*index, header, interactive, selected, cursor)
                }
                DiffRow::Meta(text) => self.meta_line(text),
                DiffRow::Line {
                    old_no,
                    new_no,
                    old,
                    new,
                } => self.line_row(*old_no, *new_no, old.as_deref(), new.as_deref()),
            })
            .collect()
    }

    fn hunk_line(
        &self,
        index: Option<usize>,
        header: &str,
        interactive: bool,
        selected: &[bool],
        cursor: usize,
    ) -> Line<'static> {
        let (text, style) = if interactive {
            let focused = index == Some(cursor);
            let checked = index
                .and_then(|index| selected.get(index).copied())
                .unwrap_or(true);
            let marker = if focused { "▸ " } else { "  " };
            let check = if checked { "[x] " } else { "[ ] " };
            let style = if focused {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else if checked {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            (format!("{marker}{check}{header}"), style)
        } else {
            (
                header.to_string(),
                Style::default().fg(self.theme.function.unwrap_or(Color::Cyan)),
            )
        };
        self.full_line(&text, style)
    }

    fn meta_line(&self, text: &str) -> Line<'static> {
        self.full_line(
            text,
            Style::default().fg(self.theme.comment.unwrap_or(Color::DarkGray)),
        )
    }

    fn full_line(&self, text: &str, style: Style) -> Line<'static> {
        let text = truncate_cells(text, self.full_width);
        let text = pad_to(&text, self.full_width);
        Line::from(vec![
            Span::styled(self.indent.clone(), Style::default().bg(self.base_bg)),
            Span::styled(text, style.bg(self.base_bg)),
        ])
    }

    fn line_row(
        &mut self,
        old_no: Option<usize>,
        new_no: Option<usize>,
        old: Option<&str>,
        new: Option<&str>,
    ) -> Line<'static> {
        let old_changed = old.is_some() && new != old;
        let new_changed = new.is_some() && old != new;
        let old_bg = if old_changed {
            self.delete_bg
        } else {
            self.base_bg
        };
        let new_bg = if new_changed {
            self.add_bg
        } else {
            self.base_bg
        };

        let mut spans = vec![Span::styled(
            self.indent.clone(),
            Style::default().bg(self.base_bg),
        )];
        spans.extend(side_spans(
            &mut self.old_highlighter,
            old_no,
            if old_changed { "-" } else { " " },
            old,
            old_bg,
            self.num_width,
            self.text_width,
            self.theme,
        ));
        spans.push(Span::styled(
            SEPARATOR,
            Style::default()
                .fg(self.theme.comment.unwrap_or(Color::DarkGray))
                .bg(self.base_bg),
        ));
        spans.extend(side_spans(
            &mut self.new_highlighter,
            new_no,
            if new_changed { "+" } else { " " },
            new,
            new_bg,
            self.num_width,
            self.text_width,
            self.theme,
        ));
        Line::from(spans)
    }
}

#[allow(clippy::too_many_arguments)]
fn side_spans(
    highlighter: &mut CodeHighlighter,
    number: Option<usize>,
    marker: &'static str,
    text: Option<&str>,
    background: Color,
    num_width: usize,
    text_width: usize,
    theme: Theme,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let Some(text) = text else {
        spans.push(Span::styled(
            " ".repeat(num_width + 3 + text_width),
            Style::default().bg(background),
        ));
        return spans;
    };
    let number = number
        .map(|number| format!("{number:>num_width$}"))
        .unwrap_or_else(|| " ".repeat(num_width));
    let marker_color = if marker == "-" {
        theme.diff_delete.unwrap_or(Color::Red)
    } else if marker == "+" {
        theme.diff_add.unwrap_or(Color::Green)
    } else {
        theme.comment.unwrap_or(Color::DarkGray)
    };
    spans.push(Span::styled(
        format!("{number} "),
        Style::default()
            .fg(theme.comment.unwrap_or(Color::DarkGray))
            .bg(background),
    ));
    spans.push(Span::styled(
        marker,
        Style::default().fg(marker_color).bg(background),
    ));
    spans.push(Span::styled(" ", Style::default().bg(background)));

    let expanded = text.replace('\t', "    ");
    let mut used = 0;
    for span in highlighter.spans(&expanded) {
        if used >= text_width {
            break;
        }
        let content = span.content.into_owned();
        let width = UnicodeWidthStr::width(content.as_str());
        if used + width <= text_width {
            used += width;
            spans.push(Span::styled(content, span.style.bg(background)));
            continue;
        }
        let remaining = text_width - used;
        let mut clipped = take_cells(&content, remaining.saturating_sub(1));
        clipped.push('…');
        used += UnicodeWidthStr::width(clipped.as_str());
        spans.push(Span::styled(clipped, span.style.bg(background)));
        break;
    }
    if used < text_width {
        spans.push(Span::styled(
            " ".repeat(text_width - used),
            Style::default().bg(background),
        ));
    }
    spans
}

fn diff_backgrounds(base: Color) -> (Color, Color) {
    let light = luminance(base) > 0.6;
    if light {
        (
            tint(base, (205, 248, 216), 0.72),
            tint(base, (255, 220, 220), 0.72),
        )
    } else {
        (
            tint(base, (38, 92, 54), 0.55),
            tint(base, (105, 42, 48), 0.55),
        )
    }
}

fn tint(base: Color, target: (u8, u8, u8), amount: f32) -> Color {
    let (red, green, blue) = rgb(base).unwrap_or((0, 0, 0));
    let blend = |base: u8, target: u8| -> u8 {
        (f32::from(base) * (1.0 - amount) + f32::from(target) * amount)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color::Rgb(
        blend(red, target.0),
        blend(green, target.1),
        blend(blue, target.2),
    )
}

fn truncate_cells(text: &str, max: usize) -> String {
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

fn take_cells(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut width = 0;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + ch_width > max {
            break;
        }
        out.push(ch);
        width += ch_width;
    }
    out
}

fn pad_to(text: &str, width: usize) -> String {
    let used = UnicodeWidthStr::width(text);
    let mut out = String::with_capacity(text.len() + width.saturating_sub(used));
    out.push_str(text);
    out.push_str(&" ".repeat(width.saturating_sub(used)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunk(header: &str, body: &[&str]) -> HunkInfo {
        let mut lines = vec![header.to_string()];
        lines.extend(body.iter().map(|line| line.to_string()));
        HunkInfo {
            index: 0,
            header: header.to_string(),
            lines,
        }
    }

    fn text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn pairs_deletions_and_additions_on_one_row() {
        let hunk = hunk(
            "@@ -10,3 +10,3 @@",
            &[" context", "-before", "+after", " tail"],
        );
        let rows = rows(&[hunk], None);
        assert_eq!(
            rows,
            vec![
                DiffRow::Hunk {
                    index: Some(0),
                    header: "@@ -10,3 +10,3 @@".into(),
                },
                DiffRow::Line {
                    old_no: Some(10),
                    new_no: Some(10),
                    old: Some("context".into()),
                    new: Some("context".into()),
                },
                DiffRow::Line {
                    old_no: Some(11),
                    new_no: Some(11),
                    old: Some("before".into()),
                    new: Some("after".into()),
                },
                DiffRow::Line {
                    old_no: Some(12),
                    new_no: Some(12),
                    old: Some("tail".into()),
                    new: Some("tail".into()),
                },
            ]
        );
    }

    #[test]
    fn unpaired_runs_leave_one_side_blank() {
        let hunk = hunk("@@ -1,2 +1,1 @@", &["-one", "-two", "+three"]);
        let rows = rows(&[hunk], None);
        assert_eq!(
            rows[1],
            DiffRow::Line {
                old_no: Some(1),
                new_no: Some(1),
                old: Some("one".into()),
                new: Some("three".into()),
            }
        );
        assert_eq!(
            rows[2],
            DiffRow::Line {
                old_no: Some(2),
                new_no: None,
                old: Some("two".into()),
                new: None,
            }
        );
    }

    #[test]
    fn parses_plain_unified_diff_and_skips_file_headers() {
        let diff = "\
--- a/src/lib.rs
+++ b/src/lib.rs
@@ -1,2 +1,2 @@
-old
+new
 same
";
        let rows = rows(&[], Some(diff));
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows[0],
            DiffRow::Hunk {
                index: None,
                header: "@@ -1,2 +1,2 @@".into(),
            }
        );
        assert_eq!(
            rows[1],
            DiffRow::Line {
                old_no: Some(1),
                new_no: Some(1),
                old: Some("old".into()),
                new: Some("new".into()),
            }
        );
    }

    #[test]
    fn hunk_offsets_use_aligned_row_counts() {
        let first = hunk("@@ -1,2 +1,2 @@", &["-a", "+b"]);
        let second = hunk("@@ -10,1 +10,1 @@", &[" same"]);
        assert_eq!(hunk_offsets(&[first, second]), vec![0, 2]);
        assert_eq!(
            rows(
                &[
                    hunk("@@ -1,2 +1,2 @@", &["-a", "+b"]),
                    hunk("@@ -10,1 +10,1 @@", &[" same"]),
                ],
                None
            )
            .len(),
            4
        );
    }

    #[test]
    fn renders_old_on_the_left_and_new_on_the_right() {
        let hunk = hunk("@@ -1,1 +1,1 @@", &["-let answer = 1;", "+let answer = 2;"]);
        let rows = rows(&[hunk], None);
        let mut renderer = DiffRenderer::new(
            &rows,
            100,
            "src/lib.rs",
            Theme::default(),
            "",
            Color::Rgb(20, 20, 20),
        )
        .expect("wide enough");
        let lines = renderer.render(&rows, false, &[], 0);
        let row = text(&lines[1]);
        assert!(row.contains("let answer = 1;"));
        assert!(row.contains("let answer = 2;"));
        assert!(row.find("let answer = 1;") < row.find("let answer = 2;"));
        assert!(row.contains('│'));
    }

    #[test]
    fn narrow_width_falls_back() {
        let hunk = hunk("@@ -1,1 +1,1 @@", &["-a", "+b"]);
        let rows = rows(&[hunk], None);
        assert!(DiffRenderer::new(&rows, 12, "a.rs", Theme::default(), "", Color::Black).is_none());
    }
}
