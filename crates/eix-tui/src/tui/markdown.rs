//! Small Markdown renderer for the transcript.
//!
//! Messages are Markdown, so the transcript renders them into styled
//! `ratatui` lines instead of showing raw `**`, backticks and `[](...)`. The
//! renderer deliberately stays terminal-friendly: links show their target,
//! images become a labelled placeholder, and TeX math is approximated with
//! Unicode glyphs (no typesetting engine is involved).
//!
//! It intentionally only covers the constructs that make a chat transcript
//! readable: headings, emphasis, inline/fenced code, lists, quotes, rules,
//! links, images, tables, task lists and inline/display math.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::math;
use super::theme::Theme;

const MUTED: Color = Color::Rgb(110, 116, 129);
const PANEL: Color = Color::Rgb(38, 41, 51);
const ACCENT: Color = Color::Rgb(122, 162, 247);
const SUCCESS: Color = Color::Rgb(126, 199, 149);

/// Render a transcript message. Lines are indented by two cells, matching the
/// transcript's left gutter.
pub fn render(text: &str, width: usize, theme: Theme) -> Vec<Line<'static>> {
    render_with_indent(text, width, theme, "  ")
}

pub(crate) fn render_with_indent(
    text: &str,
    width: usize,
    theme: Theme,
    indent: &str,
) -> Vec<Line<'static>> {
    let mut renderer = Renderer::new(width, theme, indent);
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_MATH
        | Options::ENABLE_FOOTNOTES;
    for event in Parser::new_ext(text, options) {
        renderer.event(event);
    }
    renderer.finish()
}

struct Renderer {
    width: usize,
    theme: Theme,
    indent: String,
    lines: Vec<Line<'static>>,
    inline: Vec<Span<'static>>,
    style: Style,
    style_stack: Vec<Style>,
    quote: usize,
    lists: Vec<ListState>,
    items: Vec<ItemState>,
    heading: Option<HeadingLevel>,
    code: Option<CodeState>,
    table: Option<TableState>,
    cell: Option<String>,
    caps: Vec<Capture>,
}

struct ListState {
    ordered: Option<u64>,
    next: u64,
}

struct ItemState {
    indent: usize,
    marker: String,
    first: bool,
}

struct CodeState {
    buffer: String,
}

struct TableState {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<String>>,
    header_rows: usize,
}

struct Capture {
    dest: String,
    label: String,
    /// The link/image lives inside a table cell, so its text has already been
    /// appended to the cell buffer instead of the inline span list.
    in_cell: bool,
}

impl Renderer {
    fn new(width: usize, theme: Theme, indent: &str) -> Self {
        let mut renderer = Self {
            width: width.max(1),
            theme,
            indent: indent.to_string(),
            lines: Vec::new(),
            inline: Vec::new(),
            style: Style::default(),
            style_stack: Vec::new(),
            quote: 0,
            lists: Vec::new(),
            items: Vec::new(),
            heading: None,
            code: None,
            table: None,
            cell: None,
            caps: Vec::new(),
        };
        renderer.style = renderer.base_style();
        renderer
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(text) => {
                let style = Style::default()
                    .fg(self.theme.string.unwrap_or(ACCENT))
                    .bg(panel(self.theme));
                self.append_styled(&text, style);
                self.capture(&text);
            }
            Event::InlineMath(text) => {
                // `$PATH and $HOME` in a shell snippet parses as math. Treat a
                // span without any math syntax (and longer than one letter) as
                // a literal dollar-quoted word instead of mangling it.
                if looks_like_math(&text) {
                    let rendered = latex_to_unicode(&text);
                    self.append_styled(
                        &rendered,
                        Style::default()
                            .fg(accent(self.theme))
                            .add_modifier(Modifier::ITALIC),
                    );
                } else {
                    self.append_styled(&format!("${text}$"), self.style);
                }
                self.capture(&text);
            }
            Event::DisplayMath(text) => self.display_math(&text),
            Event::Html(text) | Event::InlineHtml(text) => {
                self.append_styled(&text, dim(self.theme));
            }
            Event::FootnoteReference(label) => {
                self.append_styled(&format!("[^{label}]"), dim(self.theme));
            }
            Event::SoftBreak => self.append_styled(" ", self.style),
            Event::HardBreak => self.append_styled("\n", self.style),
            Event::Rule => self.rule(),
            Event::TaskListMarker(checked) => {
                let text = if checked { "[x] " } else { "[ ] " };
                self.append_styled(text, Style::default().fg(accent(self.theme)));
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.flush();
                self.begin_block();
                self.reset_style();
            }
            Tag::Heading { level, .. } => {
                self.flush();
                self.begin_block();
                self.heading = Some(level);
                self.reset_style();
            }
            Tag::BlockQuote(kind) => {
                self.flush();
                self.begin_block();
                self.quote += 1;
                if let Some(kind) = kind {
                    let label = match kind {
                        pulldown_cmark::BlockQuoteKind::Note => "note",
                        pulldown_cmark::BlockQuoteKind::Tip => "tip",
                        pulldown_cmark::BlockQuoteKind::Important => "important",
                        pulldown_cmark::BlockQuoteKind::Warning => "warning",
                        pulldown_cmark::BlockQuoteKind::Caution => "caution",
                    };
                    self.inline.push(Span::styled(
                        format!("[{label}] "),
                        Style::default()
                            .fg(self.theme.function.unwrap_or(ACCENT))
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                self.reset_style();
            }
            Tag::CodeBlock(kind) => {
                self.flush();
                self.begin_block();
                let lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.trim().to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                if !lang.is_empty() {
                    self.push_line(self.code_header(&lang));
                }
                self.code = Some(CodeState {
                    buffer: String::new(),
                });
            }
            Tag::HtmlBlock => {
                self.flush();
                self.begin_block();
                self.reset_style();
            }
            Tag::List(start) => {
                self.flush();
                if self.items.is_empty() {
                    self.ensure_blank();
                }
                self.lists.push(ListState {
                    ordered: start,
                    next: start.unwrap_or(1),
                });
            }
            Tag::Item => {
                self.flush();
                let depth = self.lists.len().saturating_sub(1);
                let marker = match self.lists.last_mut() {
                    Some(list) if list.ordered.is_some() => {
                        let number = list.next;
                        list.next += 1;
                        format!("{number}. ")
                    }
                    _ => "• ".to_string(),
                };
                self.items.push(ItemState {
                    indent: depth * 2,
                    marker,
                    first: true,
                });
            }
            Tag::FootnoteDefinition(label) => {
                self.flush();
                self.begin_block();
                self.inline.push(Span::styled(
                    format!("[^{label}]: "),
                    dim(self.theme).add_modifier(Modifier::BOLD),
                ));
                self.reset_style();
            }
            Tag::DefinitionList | Tag::DefinitionListTitle | Tag::DefinitionListDefinition => {
                self.flush();
                self.begin_block();
                self.reset_style();
            }
            Tag::Table(alignments) => {
                self.flush();
                self.begin_block();
                self.table = Some(TableState {
                    alignments,
                    rows: Vec::new(),
                    header_rows: 0,
                });
            }
            Tag::TableHead => {}
            Tag::TableRow => {
                if let Some(table) = &mut self.table {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => self.cell = Some(String::new()),
            Tag::Emphasis => self.push_style(Modifier::ITALIC),
            Tag::Strong => self.push_style(Modifier::BOLD),
            Tag::Strikethrough => self.push_style(Modifier::CROSSED_OUT),
            Tag::Superscript | Tag::Subscript => self.push_style(Modifier::BOLD),
            Tag::Link { dest_url, .. } => {
                if self.cell.is_none() {
                    self.style_stack.push(self.style);
                    self.style = self
                        .style
                        .fg(accent(self.theme))
                        .add_modifier(Modifier::UNDERLINED);
                }
                self.caps.push(Capture {
                    dest: dest_url.to_string(),
                    label: String::new(),
                    in_cell: self.cell.is_some(),
                });
            }
            Tag::Image { dest_url, .. } => {
                if let Some(cell) = &mut self.cell {
                    cell.push_str("🖼 ");
                } else {
                    self.style_stack.push(self.style);
                    self.style = self
                        .style
                        .fg(accent(self.theme))
                        .add_modifier(Modifier::ITALIC);
                    self.inline
                        .push(Span::styled("🖼 ", Style::default().fg(accent(self.theme))));
                }
                self.caps.push(Capture {
                    dest: dest_url.to_string(),
                    label: String::new(),
                    in_cell: self.cell.is_some(),
                });
            }
            Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush(),
            TagEnd::Heading(_) => {
                self.flush();
                self.heading = None;
                self.reset_style();
                self.ensure_blank();
            }
            TagEnd::BlockQuote(_) => {
                self.flush();
                self.quote = self.quote.saturating_sub(1);
                self.reset_style();
                self.ensure_blank();
            }
            TagEnd::CodeBlock => self.finish_code(),
            TagEnd::HtmlBlock => self.flush(),
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.ensure_blank();
                }
            }
            TagEnd::Item => {
                self.flush();
                self.items.pop();
            }
            TagEnd::FootnoteDefinition => self.flush(),
            TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition => self.flush(),
            TagEnd::Table => self.finish_table(),
            TagEnd::TableHead => {
                if let Some(table) = &mut self.table {
                    table.header_rows = table.rows.len();
                }
            }
            TagEnd::TableRow => {}
            TagEnd::TableCell => {
                if let Some(cell) = self.cell.take() {
                    if let Some(table) = &mut self.table {
                        if table.rows.is_empty() {
                            table.rows.push(Vec::new());
                        }
                        table.rows.last_mut().unwrap().push(cell);
                    }
                }
            }
            TagEnd::Emphasis
            | TagEnd::Strong
            | TagEnd::Strikethrough
            | TagEnd::Superscript
            | TagEnd::Subscript => self.pop_style(),
            TagEnd::Link => {
                if let Some(cap) = self.caps.pop() {
                    if !cap.in_cell {
                        self.pop_style();
                    }
                    let label = cap.label.trim();
                    if !cap.dest.is_empty() && label != cap.dest && !cap.dest.starts_with('#') {
                        self.append_styled(&format!(" ({})", cap.dest), dim(self.theme));
                    }
                }
            }
            TagEnd::Image => {
                if let Some(cap) = self.caps.pop() {
                    if !cap.in_cell {
                        self.pop_style();
                    }
                    let label = cap.label.trim();
                    if !cap.dest.is_empty() {
                        if cap.in_cell || !label.is_empty() {
                            self.append_styled(&format!(" ({})", cap.dest), dim(self.theme));
                        } else {
                            self.append_styled(cap.dest.as_str(), dim(self.theme));
                        }
                    }
                }
            }
            TagEnd::MetadataBlock(_) => {}
        }
    }

    /// Text arrives either for the current block, a fenced code buffer, or a
    /// table cell. Text inside links and images is also captured so the end tag
    /// can decide whether to show the target URL.
    fn text(&mut self, text: &str) {
        if let Some(code) = &mut self.code {
            code.buffer.push_str(text);
            self.flush_code_lines();
            return;
        }
        if let Some(cell) = self.cell.as_mut() {
            cell.push_str(text);
            return;
        }
        let style = self.style;
        self.append_styled(text, style);
        self.capture(text);
    }

    // ----- inline helpers -------------------------------------------------

    fn push_style(&mut self, modifier: Modifier) {
        self.style_stack.push(self.style);
        self.style = self.style.add_modifier(modifier);
    }

    fn pop_style(&mut self) {
        if let Some(style) = self.style_stack.pop() {
            self.style = style;
        }
    }

    fn append_styled(&mut self, text: &str, style: Style) {
        if text.is_empty() {
            return;
        }
        if let Some(cell) = &mut self.cell {
            cell.push_str(text);
        } else {
            self.inline.push(Span::styled(text.to_string(), style));
        }
    }

    fn capture(&mut self, text: &str) {
        if let Some(cap) = self.caps.last_mut() {
            cap.label.push_str(text);
        }
    }

    fn reset_style(&mut self) {
        self.style_stack.clear();
        self.style = self.base_style();
    }

    fn base_style(&self) -> Style {
        if let Some(level) = self.heading {
            let color = match level {
                HeadingLevel::H1 | HeadingLevel::H2 => accent(self.theme),
                HeadingLevel::H3 => text_color(self.theme),
                _ => comment(self.theme),
            };
            return Style::default().fg(color).add_modifier(Modifier::BOLD);
        }
        if self.quote > 0 {
            return Style::default()
                .fg(comment(self.theme))
                .add_modifier(Modifier::ITALIC);
        }
        Style::default().fg(text_color(self.theme))
    }

    // ----- block layout ---------------------------------------------------

    fn begin_block(&mut self) {
        self.ensure_blank();
    }

    fn ensure_blank(&mut self) {
        if let Some(last) = self.lines.last() {
            if last.width() > 0 {
                self.lines.push(Line::from(""));
            }
        }
    }

    fn prefix_spans(&self, first: bool) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        if !self.indent.is_empty() {
            spans.push(Span::raw(self.indent.clone()));
        }
        for _ in 0..self.quote {
            spans.push(Span::styled("│ ", dim(self.theme)));
        }
        if let Some(item) = self.items.last() {
            if item.indent > 0 {
                spans.push(Span::raw(" ".repeat(item.indent)));
            }
            if first {
                spans.push(Span::styled(
                    item.marker.clone(),
                    Style::default().fg(accent(self.theme)),
                ));
            } else {
                spans.push(Span::raw(
                    " ".repeat(UnicodeWidthStr::width(item.marker.as_str())),
                ));
            }
        }
        if let Some(level) = self.heading {
            let marker = format!("{} ", "#".repeat(level as usize));
            if first {
                spans.push(Span::styled(marker, dim(self.theme)));
            } else {
                let width = UnicodeWidthStr::width(marker.as_str());
                spans.push(Span::raw(" ".repeat(width)));
            }
        }
        spans
    }

    fn prefix_width(spans: &[Span<'static>]) -> usize {
        spans
            .iter()
            .map(|span| UnicodeWidthStr::width(span.content.as_ref()))
            .sum()
    }

    fn flush(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let first_prefix = self.prefix_spans(true);
        let cont_prefix = self.prefix_spans(false);
        let prefix_width = Self::prefix_width(&first_prefix).max(Self::prefix_width(&cont_prefix));
        let avail = self.width.saturating_sub(prefix_width).max(1);
        let wrapped = wrap_spans(&self.inline, avail);
        self.inline.clear();
        for (index, spans) in wrapped.into_iter().enumerate() {
            let prefix = if index == 0 {
                first_prefix.clone()
            } else {
                cont_prefix.clone()
            };
            let mut line = prefix;
            line.extend(spans);
            self.lines.push(Line::from(line));
        }
        if let Some(item) = self.items.last_mut() {
            item.first = false;
        }
    }

    fn push_line(&mut self, mut spans: Vec<Span<'static>>) {
        // Reuse the normal prefix logic so code/table headers respect lists and
        // quotes. Callers pass only their own marker spans.
        let prefix = self.prefix_spans(true);
        let mut line = prefix;
        line.append(&mut spans);
        self.lines.push(Line::from(line));
        if let Some(item) = self.items.last_mut() {
            item.first = false;
        }
    }

    fn rule(&mut self) {
        self.flush();
        self.ensure_blank();
        let prefix = self.prefix_spans(true);
        let width = self
            .width
            .saturating_sub(Self::prefix_width(&prefix))
            .clamp(1, 60);
        let mut spans = prefix;
        spans.push(Span::styled("─".repeat(width), dim(self.theme)));
        self.lines.push(Line::from(spans));
        self.ensure_blank();
    }

    fn display_math(&mut self, source: &str) {
        self.flush();
        self.ensure_blank();
        let prefix = self.prefix_spans(true);
        let prefix_width = Self::prefix_width(&prefix);
        let avail = self.width.saturating_sub(prefix_width).max(1);
        let style = Style::default()
            .fg(accent(self.theme))
            .add_modifier(Modifier::BOLD);
        let rendered = math::display(source, avail, style);
        for line in rendered {
            let mut spans = prefix.clone();
            spans.extend(line.spans);
            self.lines.push(Line::from(spans));
        }
        self.ensure_blank();
    }

    // ----- fenced code ----------------------------------------------------

    fn code_header(&self, lang: &str) -> Vec<Span<'static>> {
        vec![
            Span::styled("╭─ ", dim(self.theme)),
            Span::styled(
                lang.to_string(),
                Style::default()
                    .fg(self.theme.function.unwrap_or(ACCENT))
                    .add_modifier(Modifier::BOLD),
            ),
        ]
    }

    fn code_prefix(&self, first: bool) -> Vec<Span<'static>> {
        let mut spans = Vec::new();
        if !self.indent.is_empty() {
            spans.push(Span::raw(self.indent.clone()));
        }
        for _ in 0..self.quote {
            spans.push(Span::styled("│ ", dim(self.theme)));
        }
        if let Some(item) = self.items.last() {
            if item.indent > 0 {
                spans.push(Span::raw(" ".repeat(item.indent)));
            }
            if first && item.first {
                spans.push(Span::styled(
                    item.marker.clone(),
                    Style::default().fg(accent(self.theme)),
                ));
            } else {
                spans.push(Span::raw(
                    " ".repeat(UnicodeWidthStr::width(item.marker.as_str())),
                ));
            }
        }
        spans.push(Span::styled("│ ", dim(self.theme)));
        spans
    }

    fn flush_code_lines(&mut self) {
        let Some(code) = &mut self.code else {
            return;
        };
        let mut complete = Vec::new();
        while let Some(pos) = code.buffer.find('\n') {
            let line = code.buffer.drain(..=pos).collect::<String>();
            complete.push(line.trim_end_matches('\n').to_string());
        }
        for line in complete {
            self.render_code_line(&line);
        }
    }

    fn render_code_line(&mut self, line: &str) {
        let line = line.replace('\t', "    ");
        let first_prefix = self.code_prefix(true);
        let cont_prefix = self.code_prefix(false);
        let prefix_width = Self::prefix_width(&first_prefix).max(Self::prefix_width(&cont_prefix));
        let avail = self.width.saturating_sub(prefix_width).max(1);
        let wrapped = wrap_plain(&line, avail);
        let style = Style::default().fg(self.theme.string.unwrap_or(SUCCESS));
        for (index, segment) in wrapped.into_iter().enumerate() {
            let mut spans = if index == 0 {
                first_prefix.clone()
            } else {
                cont_prefix.clone()
            };
            spans.push(Span::styled(segment, style));
            self.lines.push(Line::from(spans));
        }
        if let Some(item) = self.items.last_mut() {
            item.first = false;
        }
    }

    fn finish_code(&mut self) {
        let Some(code) = self.code.take() else {
            return;
        };
        if !code.buffer.is_empty() {
            let line = code.buffer.trim_end_matches('\n').to_string();
            self.render_code_line(&line);
        }
        // An empty fenced block still deserves a visible row.
        if self.lines.last().is_some_and(|line| line.width() == 0) {
            self.render_code_line("");
        }
        self.ensure_blank();
    }

    // ----- tables ---------------------------------------------------------

    fn finish_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        if table.rows.is_empty() {
            return;
        }
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        if columns == 0 {
            return;
        }
        let prefix = self.prefix_spans(false);
        let prefix_width = Self::prefix_width(&prefix);
        // "│" + per cell: " " + content + " │"
        let borders = 1 + columns * 3;
        let available = self
            .width
            .saturating_sub(prefix_width + borders)
            .max(columns * 3);
        let mut widths = vec![3usize; columns];
        for row in &table.rows {
            for (index, cell) in row.iter().enumerate() {
                widths[index] = widths[index].max(UnicodeWidthStr::width(cell.as_str()));
            }
        }
        // Shrink the widest columns until the table fits the pane.
        while widths.iter().sum::<usize>() > available {
            let Some((widest, _)) = widths.iter().enumerate().max_by_key(|(_, width)| **width)
            else {
                break;
            };
            if widths[widest] <= 3 {
                break;
            }
            widths[widest] -= 1;
        }

        for (row_index, row) in table.rows.iter().enumerate() {
            let mut spans = prefix.clone();
            spans.push(Span::styled("│", dim(self.theme)));
            for (index, width) in widths.iter().enumerate() {
                let cell = row.get(index).map(String::as_str).unwrap_or("");
                let align = table
                    .alignments
                    .get(index)
                    .copied()
                    .unwrap_or(Alignment::None);
                let cell = truncate_cell(cell, *width, align);
                let style = if row_index < table.header_rows {
                    Style::default()
                        .fg(accent(self.theme))
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(text_color(self.theme))
                };
                spans.push(Span::styled(" ", dim(self.theme)));
                spans.push(Span::styled(cell, style));
                spans.push(Span::styled(" │", dim(self.theme)));
            }
            self.lines.push(Line::from(spans));
            if row_index + 1 == table.header_rows {
                let mut rule = prefix.clone();
                rule.push(Span::styled("├", dim(self.theme)));
                for (index, width) in widths.iter().enumerate() {
                    rule.push(Span::styled("─".repeat(width + 2), dim(self.theme)));
                    if index + 1 < widths.len() {
                        rule.push(Span::styled("┼", dim(self.theme)));
                    }
                }
                rule.push(Span::styled("┤", dim(self.theme)));
                self.lines.push(Line::from(rule));
            }
        }
        self.ensure_blank();
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush();
        if self.table.is_some() {
            self.finish_table();
        }
        if self.code.is_some() {
            if let Some(code) = self.code.take() {
                let buffered = code.buffer.trim_end_matches('\n').to_string();
                if !buffered.is_empty() {
                    self.render_code_line(&buffered);
                }
            }
        }
        while self.lines.last().is_some_and(|line| line.width() == 0) {
            self.lines.pop();
        }
        self.lines
    }
}

// ----- wrapping -----------------------------------------------------------

fn wrap_plain(text: &str, width: usize) -> Vec<String> {
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

/// Wrap styled spans, preferring breaks at spaces while preserving styles.
fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<(char, Style)>> = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    let mut current_width = 0usize;

    for span in spans {
        for ch in span.content.chars() {
            if ch == '\n' {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
                continue;
            }
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if ch_width > 0 && current_width + ch_width > width {
                if let Some(space) = current.iter().rposition(|(ch, _)| *ch == ' ') {
                    let mut rest = current.split_off(space + 1);
                    while current.last().is_some_and(|(ch, _)| *ch == ' ') {
                        current.pop();
                    }
                    lines.push(std::mem::take(&mut current));
                    while rest.first().is_some_and(|(ch, _)| *ch == ' ') {
                        rest.remove(0);
                    }
                    current = rest;
                    current_width = current
                        .iter()
                        .map(|(ch, _)| UnicodeWidthChar::width(*ch).unwrap_or(0))
                        .sum();
                } else {
                    lines.push(std::mem::take(&mut current));
                    current_width = 0;
                }
                if ch == ' ' {
                    continue;
                }
            }
            current.push((ch, span.style));
            current_width += ch_width;
        }
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }

    lines.into_iter().map(chars_to_spans).collect()
}

fn chars_to_spans(chars: Vec<(char, Style)>) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut run = String::new();
    let mut style = None;
    for (ch, ch_style) in chars {
        if style.is_some() && style != Some(ch_style) {
            spans.push(Span::styled(
                std::mem::take(&mut run),
                style.unwrap_or_default(),
            ));
        }
        style = Some(ch_style);
        run.push(ch);
    }
    if !run.is_empty() {
        spans.push(Span::styled(run, style.unwrap_or_default()));
    }
    spans
}

fn truncate_cell(text: &str, width: usize, align: Alignment) -> String {
    let text = text.replace('\n', " ");
    let text_width = UnicodeWidthStr::width(text.as_str());
    if text_width <= width {
        return match align {
            Alignment::Right => format!("{}{}", " ".repeat(width - text_width), text),
            Alignment::Center => {
                let left = (width - text_width) / 2;
                let right = width - text_width - left;
                format!("{}{text}{}", " ".repeat(left), " ".repeat(right))
            }
            _ => format!("{text}{}", " ".repeat(width - text_width)),
        };
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > width.saturating_sub(1) {
            break;
        }
        out.push(ch);
        used += ch_width;
    }
    out.push('…');
    let used = UnicodeWidthStr::width(out.as_str());
    format!("{out}{}", " ".repeat(width.saturating_sub(used)))
}

// ----- colors -------------------------------------------------------------

fn comment(theme: Theme) -> Color {
    theme.comment.unwrap_or(MUTED)
}

fn text_color(theme: Theme) -> Color {
    theme.normal.unwrap_or(Color::Rgb(220, 223, 228))
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

// ----- TeX to Unicode -----------------------------------------------------

/// A pragmatic TeX-to-Unicode conversion for terminal display. It handles the
/// math people actually put in chat answers (fractions, roots, scripts, Greek
/// letters and common operators) and leaves anything else readable as text.
pub fn latex_to_unicode(source: &str) -> String {
    let chars: Vec<char> = source.chars().collect();
    let mut out = String::new();
    let mut index = 0;
    while index < chars.len() {
        match chars[index] {
            '\\' => {
                index += 1;
                if index >= chars.len() {
                    out.push('\\');
                    break;
                }
                if chars[index].is_ascii_alphabetic() {
                    let start = index;
                    while index < chars.len() && chars[index].is_ascii_alphabetic() {
                        index += 1;
                    }
                    let command: String = chars[start..index].iter().collect();
                    match command.as_str() {
                        "frac" | "dfrac" | "tfrac" => {
                            let (numerator, next) = take_arg(&chars, index);
                            let (denominator, next) = take_arg(&chars, next);
                            index = next;
                            out.push_str(&format_fraction(&numerator, &denominator));
                        }
                        "sqrt" => {
                            let (argument, next) = take_arg(&chars, index);
                            index = next;
                            out.push_str(&format_root(&argument));
                        }
                        "text" | "textrm" | "mathrm" | "mathbf" | "mathit" | "mathbb"
                        | "mathcal" | "operatorname" => {
                            let (argument, next) = take_arg(&chars, index);
                            index = next;
                            let rendered = latex_to_unicode(&argument);
                            if command == "mathbb" {
                                out.push_str(&blackboard(&rendered));
                            } else {
                                out.push_str(&rendered);
                            }
                        }
                        "vec" | "hat" | "bar" | "tilde" | "dot" | "ddot" => {
                            let (argument, next) = take_arg(&chars, index);
                            index = next;
                            let rendered = latex_to_unicode(&argument);
                            let combining = match command.as_str() {
                                "vec" => '\u{20d7}',
                                "hat" => '\u{0302}',
                                "bar" => '\u{0304}',
                                "tilde" => '\u{0303}',
                                "dot" => '\u{0307}',
                                _ => '\u{0308}',
                            };
                            if let Some(last) = rendered.chars().last() {
                                if rendered.chars().count() == 1 {
                                    out.push(last);
                                    out.push(combining);
                                } else {
                                    out.push_str(&rendered);
                                }
                            }
                        }
                        "left" | "right" | "middle" => {
                            if index < chars.len() {
                                out.push(map_delimiter(chars[index]));
                                index += 1;
                            }
                        }
                        "begin" | "end" => {
                            let (_environment, next) = take_arg(&chars, index);
                            index = next;
                        }
                        "quad" => out.push_str("  "),
                        "qquad" => out.push_str("    "),
                        "," | ";" | ":" | "!" | " " => out.push(' '),
                        _ => {
                            if let Some(symbol) = symbol(&command) {
                                out.push_str(symbol);
                            } else {
                                out.push_str(&command);
                            }
                        }
                    }
                } else {
                    match chars[index] {
                        '\\' => out.push('\n'),
                        ',' | ';' | '!' => out.push(' '),
                        '{' | '}' | '$' | '%' | '&' | '#' | '_' => out.push(chars[index]),
                        _ => out.push(chars[index]),
                    }
                    index += 1;
                }
            }
            '^' => {
                let (argument, next) = take_arg(&chars, index + 1);
                index = next;
                out.push_str(&script(&argument, true));
            }
            '_' => {
                let (argument, next) = take_arg(&chars, index + 1);
                index = next;
                out.push_str(&script(&argument, false));
            }
            '{' | '}' => index += 1,
            '&' => {
                out.push_str("  ");
                index += 1;
            }
            ch => {
                out.push(ch);
                index += 1;
            }
        }
    }
    out.trim().to_string()
}

/// Decide whether an inline `$...$` span is real math rather than shell syntax.
fn looks_like_math(source: &str) -> bool {
    let trimmed = source.trim();
    if trimmed.chars().count() == 1 && trimmed.chars().all(char::is_alphabetic) {
        return true;
    }
    trimmed.chars().any(|ch| {
        matches!(
            ch,
            '\\' | '^'
                | '_'
                | '{'
                | '}'
                | '['
                | ']'
                | '('
                | ')'
                | '='
                | '<'
                | '>'
                | '+'
                | '-'
                | '*'
                | '/'
                | '|'
                | '!'
                | ','
                | '\''
        ) || ch.is_ascii_digit()
    })
}

fn take_arg(chars: &[char], mut index: usize) -> (String, usize) {
    while index < chars.len() && chars[index].is_whitespace() {
        index += 1;
    }
    if index >= chars.len() {
        return (String::new(), index);
    }
    if chars[index] == '[' {
        // Optional argument such as \sqrt[3]{x}; drop the index but keep the
        // following group as the argument.
        index += 1;
        while index < chars.len() && chars[index] != ']' {
            index += 1;
        }
        if index < chars.len() {
            index += 1;
        }
        return take_arg(chars, index);
    }
    if chars[index] == '{' {
        let start = index + 1;
        let mut depth = 1;
        index += 1;
        while index < chars.len() {
            match chars[index] {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                _ => {}
            }
            index += 1;
        }
        let argument: String = chars[start..index.min(chars.len())].iter().collect();
        return (argument, (index + 1).min(chars.len()));
    }
    if chars[index] == '\\' {
        let start = index;
        index += 1;
        while index < chars.len() && chars[index].is_ascii_alphabetic() {
            index += 1;
        }
        if index == start + 1 {
            index += 1;
        }
        return (chars[start..index.min(chars.len())].iter().collect(), index);
    }
    (chars[index].to_string(), index + 1)
}

fn format_fraction(numerator: &str, denominator: &str) -> String {
    let numerator = latex_to_unicode(numerator);
    let denominator = latex_to_unicode(denominator);
    let simple =
        |text: &str| text.chars().count() <= 1 || text.chars().all(|ch| ch.is_alphanumeric());
    let numerator = if simple(&numerator) {
        numerator
    } else {
        format!("({numerator})")
    };
    let denominator = if simple(&denominator) {
        denominator
    } else {
        format!("({denominator})")
    };
    format!("{numerator}/{denominator}")
}

fn format_root(argument: &str) -> String {
    let argument = latex_to_unicode(argument);
    if argument.chars().count() <= 1 || argument.chars().all(|ch| ch.is_alphanumeric()) {
        format!("√{argument}")
    } else {
        format!("√({argument})")
    }
}

fn script(argument: &str, superscript: bool) -> String {
    let rendered = latex_to_unicode(argument);
    let mut out = String::new();
    let mut mapped = true;
    for ch in rendered.chars() {
        let mapped_char = if superscript {
            superscript_char(ch)
        } else {
            subscript_char(ch)
        };
        match mapped_char {
            Some(ch) => out.push(ch),
            None => {
                mapped = false;
                break;
            }
        }
    }
    if mapped && !out.is_empty() {
        out
    } else if superscript {
        format!("^({rendered})")
    } else {
        format!("_({rendered})")
    }
}

pub(super) fn superscript_char(ch: char) -> Option<char> {
    Some(match ch {
        '0' => '⁰',
        '1' => '¹',
        '2' => '²',
        '3' => '³',
        '4' => '⁴',
        '5' => '⁵',
        '6' => '⁶',
        '7' => '⁷',
        '8' => '⁸',
        '9' => '⁹',
        '+' => '⁺',
        '-' => '⁻',
        '=' => '⁼',
        '(' => '⁽',
        ')' => '⁾',
        'n' => 'ⁿ',
        'i' => 'ⁱ',
        'a' => 'ᵃ',
        'b' => 'ᵇ',
        'c' => 'ᶜ',
        'd' => 'ᵈ',
        'e' => 'ᵉ',
        'f' => 'ᶠ',
        'g' => 'ᵍ',
        'h' => 'ʰ',
        'j' => 'ʲ',
        'k' => 'ᵏ',
        'l' => 'ˡ',
        'm' => 'ᵐ',
        'o' => 'ᵒ',
        'p' => 'ᵖ',
        'r' => 'ʳ',
        's' => 'ˢ',
        't' => 'ᵗ',
        'u' => 'ᵘ',
        'v' => 'ᵛ',
        'w' => 'ʷ',
        'x' => 'ˣ',
        'y' => 'ʸ',
        'z' => 'ᶻ',
        _ => return None,
    })
}

pub(super) fn subscript_char(ch: char) -> Option<char> {
    Some(match ch {
        '0' => '₀',
        '1' => '₁',
        '2' => '₂',
        '3' => '₃',
        '4' => '₄',
        '5' => '₅',
        '6' => '₆',
        '7' => '₇',
        '8' => '₈',
        '9' => '₉',
        '+' => '₊',
        '-' => '₋',
        '=' => '₌',
        '(' => '₍',
        ')' => '₎',
        'a' => 'ₐ',
        'e' => 'ₑ',
        'h' => 'ₕ',
        'i' => 'ᵢ',
        'j' => 'ⱼ',
        'k' => 'ₖ',
        'l' => 'ₗ',
        'm' => 'ₘ',
        'n' => 'ₙ',
        'o' => 'ₒ',
        'p' => 'ₚ',
        'r' => 'ᵣ',
        's' => 'ₛ',
        't' => 'ₜ',
        'u' => 'ᵤ',
        'v' => 'ᵥ',
        'x' => 'ₓ',
        _ => return None,
    })
}

pub(super) fn blackboard(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            'C' => 'ℂ',
            'H' => 'ℍ',
            'N' => 'ℕ',
            'P' => 'ℙ',
            'Q' => 'ℚ',
            'R' => 'ℝ',
            'Z' => 'ℤ',
            _ => ch,
        })
        .collect()
}

fn map_delimiter(ch: char) -> char {
    match ch {
        '.' => ' ',
        _ => ch,
    }
}

pub(super) fn symbol(command: &str) -> Option<&'static str> {
    Some(match command {
        "alpha" => "α",
        "beta" => "β",
        "gamma" => "γ",
        "delta" => "δ",
        "epsilon" => "ε",
        "varepsilon" => "ε",
        "zeta" => "ζ",
        "eta" => "η",
        "theta" => "θ",
        "vartheta" => "ϑ",
        "iota" => "ι",
        "kappa" => "κ",
        "lambda" => "λ",
        "mu" => "μ",
        "nu" => "ν",
        "xi" => "ξ",
        "omicron" => "ο",
        "pi" => "π",
        "varpi" => "ϖ",
        "rho" => "ρ",
        "varrho" => "ϱ",
        "sigma" => "σ",
        "varsigma" => "ς",
        "tau" => "τ",
        "upsilon" => "υ",
        "phi" => "φ",
        "varphi" => "ϕ",
        "chi" => "χ",
        "psi" => "ψ",
        "omega" => "ω",
        "Gamma" => "Γ",
        "Delta" => "Δ",
        "Theta" => "Θ",
        "Lambda" => "Λ",
        "Xi" => "Ξ",
        "Pi" => "Π",
        "Sigma" => "Σ",
        "Upsilon" => "Υ",
        "Phi" => "Φ",
        "Psi" => "Ψ",
        "Omega" => "Ω",
        "pm" => "±",
        "mp" => "∓",
        "times" => "×",
        "div" => "÷",
        "cdot" => "·",
        "ast" => "∗",
        "star" => "⋆",
        "circ" => "∘",
        "bullet" => "•",
        "le" | "leq" => "≤",
        "ge" | "geq" => "≥",
        "ne" | "neq" => "≠",
        "approx" => "≈",
        "equiv" => "≡",
        "sim" => "∼",
        "propto" => "∝",
        "infty" => "∞",
        "partial" => "∂",
        "nabla" => "∇",
        "sum" => "∑",
        "prod" => "∏",
        "coprod" => "∐",
        "int" => "∫",
        "iint" => "∬",
        "iiint" => "∭",
        "oint" => "∮",
        "to" | "rightarrow" => "→",
        "leftarrow" | "gets" => "←",
        "leftrightarrow" => "↔",
        "Rightarrow" => "⇒",
        "Leftarrow" => "⇐",
        "Leftrightarrow" => "⇔",
        "mapsto" => "↦",
        "uparrow" => "↑",
        "downarrow" => "↓",
        "in" => "∈",
        "notin" => "∉",
        "ni" => "∋",
        "subset" => "⊂",
        "subseteq" => "⊆",
        "supset" => "⊃",
        "supseteq" => "⊇",
        "cup" => "∪",
        "cap" => "∩",
        "setminus" => "∖",
        "emptyset" | "varnothing" => "∅",
        "forall" => "∀",
        "exists" => "∃",
        "nexists" => "∄",
        "neg" | "lnot" => "¬",
        "land" | "wedge" => "∧",
        "lor" | "vee" => "∨",
        "oplus" => "⊕",
        "otimes" => "⊗",
        "perp" => "⊥",
        "parallel" => "∥",
        "angle" => "∠",
        "degree" => "°",
        "prime" => "′",
        "ldots" | "dots" => "…",
        "cdots" => "⋯",
        "vdots" => "⋮",
        "ddots" => "⋱",
        "langle" => "⟨",
        "rangle" => "⟩",
        "lceil" => "⌈",
        "rceil" => "⌉",
        "lfloor" => "⌊",
        "rfloor" => "⌋",
        "lvert" | "rvert" | "vert" => "|",
        "Vert" => "‖",
        "ell" => "ℓ",
        "hbar" => "ℏ",
        "Re" => "ℜ",
        "Im" => "ℑ",
        "aleph" => "ℵ",
        "therefore" => "∴",
        "because" => "∵",
        "checkmark" => "✓",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line<'static>]) -> String {
        lines
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
    fn renders_headings_emphasis_and_lists() {
        let rendered = render(
            "# Title\n\nA **bold** and *soft* word.\n\n- one\n- two\n\n1. first\n2. second",
            60,
            Theme::default(),
        );
        let output = text(&rendered);
        assert!(output.contains("# Title"));
        assert!(output.contains("A bold and soft word."));
        assert!(output.contains("• one"));
        assert!(output.contains("1. first"));
        assert!(output.contains("2. second"));
        assert!(!output.contains("**"));
    }

    #[test]
    fn renders_links_and_images() {
        let output = text(&render(
            "See [the docs](https://example.com/docs) and ![diagram](img/flow.png).",
            80,
            Theme::default(),
        ));
        assert!(output.contains("the docs (https://example.com/docs)"));
        assert!(output.contains("🖼 diagram (img/flow.png)"));
    }

    #[test]
    fn renders_inline_and_fenced_code() {
        let output = text(&render(
            "Use `cargo test`.\n\n```rust\nfn main() {\n    println!(\"hi\");\n}\n```",
            60,
            Theme::default(),
        ));
        assert!(output.contains("Use cargo test."));
        assert!(output.contains("rust"));
        assert!(output.contains("fn main() {"));
        assert!(output.contains("    println!(\"hi\");"));
    }

    #[test]
    fn renders_math_as_unicode() {
        let output = text(&render(
            "Inline $x^2 + y_1$ and display:\n\n$$\\frac{a+b}{c} \\le \\sqrt{x} \\to \\infty$$",
            80,
            Theme::default(),
        ));
        assert!(output.contains("x² + y₁"));
        assert!(output.contains("a+b"));
        assert!(output.contains("─────"));
        assert!(output.contains("≤"));
        assert!(output.contains("√x"));
        assert!(output.contains("→ ∞"));
    }

    #[test]
    fn wraps_styled_text_without_exceeding_width() {
        let theme = Theme::default();
        let rendered = render("word ".repeat(40).trim(), 30, theme);
        assert!(rendered.iter().all(|line| line.width() <= 30));
    }

    #[test]
    fn table_columns_stay_within_width() {
        let rendered = render(
            "| a | b |\n| - | - |\n| a very long cell | another long cell |",
            40,
            Theme::default(),
        );
        assert!(rendered.iter().all(|line| line.width() <= 40));
        let output = text(&rendered);
        assert!(output.contains("a very long"));
    }

    #[test]
    fn links_and_images_inside_table_cells_stay_in_the_cell() {
        let rendered = render(
            "| name | link |\n| - | - |\n| docs | [site](https://e.com) |\n| pic | ![alt](a.png) |",
            80,
            Theme::default(),
        );
        let output = text(&rendered);
        assert!(output.contains("site (https://e.com)"));
        assert!(output.contains("🖼 alt (a.png)"));
        assert!(rendered.iter().all(|line| line.width() <= 80));
    }

    #[test]
    fn code_blocks_keep_their_list_marker() {
        let output = text(&render(
            "- item\n\n  ```rust\n  fn main() {}\n  ```",
            50,
            Theme::default(),
        ));
        assert!(output.contains("• ╭─ rust"));
        assert!(output.contains("│ fn main() {}"));
    }

    #[test]
    fn inline_dollars_from_shell_snippets_stay_literal() {
        let output = text(&render(
            "Use $PATH and $HOME here, but see $x$ and $E = mc^2$.",
            80,
            Theme::default(),
        ));
        assert!(output.contains("$PATH and $HOME"));
        assert!(output.contains("x"));
        assert!(output.contains("E = mc²"));
    }

    #[test]
    fn display_math_wraps_to_the_pane() {
        let rendered = render(
            "$$a + b + c + d + e + f + g + h + i + j + k + l + m$$",
            20,
            Theme::default(),
        );
        assert!(rendered.iter().all(|line| line.width() <= 20));
    }

    #[test]
    fn display_math_uses_two_dimensional_layout() {
        let cases = [
            ("\\frac{a+b}{c}", vec!["a+b", "─────", "c"]),
            ("\\sqrt{\\frac{a}{b}}", vec!["√", "─"]),
            (
                "\\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix}",
                vec!["⎛", "⎞", "⎝", "⎠"],
            ),
            (
                "\\lim_{x \\to 0} \\frac{\\sin x}{x} = 1",
                vec!["lim", "x → 0", "sin x"],
            ),
            ("\\sum_{i=1}^{n} i", vec!["∑", "i=1"]),
            ("\\int_0^1 x^2 \\, dx", vec!["∫₀¹", "x²"]),
            ("\\left( \\frac{a}{b} \\right)", vec!["⎛", "⎞"]),
        ];
        for (formula, needles) in cases {
            let rendered = render(&format!("$${formula}$$"), 80, Theme::default());
            assert!(
                rendered.iter().all(|line| line.width() <= 80),
                "{formula} exceeded the pane"
            );
            let output = text(&rendered);
            for needle in needles {
                assert!(output.contains(needle), "{formula}\n{output}");
            }
        }
    }

    #[test]
    fn latex_handles_common_constructs() {
        assert_eq!(latex_to_unicode("x^2 + y_1"), "x² + y₁");
        assert_eq!(latex_to_unicode("\\alpha + \\beta"), "α + β");
        assert_eq!(latex_to_unicode("\\frac{1}{2}"), "1/2");
        assert_eq!(latex_to_unicode("\\sqrt{x^2+y^2}"), "√(x²+y²)");
        assert_eq!(latex_to_unicode("\\mathbb{R}^n"), "ℝⁿ");
    }
}
