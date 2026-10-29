//! Small Markdown renderer for the transcript.
//!
//! Messages are Markdown, so the transcript renders them into styled
//! `ratatui` lines instead of showing raw `**`, backticks and `[](...)`. The
//! renderer deliberately stays terminal-friendly: links show their target,
//! local images become thumbnails when the terminal speaks Kitty, Sixel or
//! iTerm2 (and a labelled placeholder otherwise). Math uses a real LaTeX
//! image when a graphics protocol is available: display math on its own
//! block, inline math scaled to a single text row. When no image can be
//! drawn, display math falls back to the character-grid layout and inline
//! math to a compact one-line form; with math turned off both stay as the
//! literal TeX the message contained.
//!
//! It intentionally only covers the constructs that make a chat transcript
//! readable: headings, emphasis, inline/fenced code, lists, quotes, rules,
//! links, images, tables, task lists and inline/display math.

use std::path::{Path, PathBuf};

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::app::wrap_text;
use super::math;
use super::math_image;
use super::syntax::{light_background, CodeHighlighter};
use super::theme::Theme;

const MUTED: Color = Color::Rgb(110, 116, 129);
/// Transparent palettes have no accent hue; follow the terminal foreground.
const ACCENT: Color = Color::Reset;
const SUCCESS: Color = Color::Rgb(126, 199, 149);
/// Inline code borrows dwm's default bar blue (#005577). On a dark terminal the
/// same hue is lifted just enough to stay readable against a transparent
/// background; neither variant paints a fill.
const INLINE_CODE_LIGHT: Color = Color::Rgb(0, 85, 119);
const INLINE_CODE_DARK: Color = Color::Rgb(81, 168, 216);

/// Render a transcript message. Lines are indented by two cells, matching the
/// transcript's left gutter.
#[cfg(test)]
pub fn render(text: &str, width: usize, theme: Theme) -> Vec<Line<'static>> {
    render_with_indent(text, width, theme, "  ")
}

#[cfg(test)]
pub(crate) fn render_with_indent(
    text: &str,
    width: usize,
    theme: Theme,
    indent: &str,
) -> Vec<Line<'static>> {
    render_with_indent_detailed(text, width, theme, indent, false, None).lines
}

/// Rendered Markdown plus the metadata the transcript needs beyond the text:
/// display-math image placements and, for every rendered math line, the LaTeX
/// source to copy instead of the character approximation.
pub(crate) struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub images: Vec<math_image::Placement>,
    /// Display math that needed a background render when the layout was built.
    pub pending: Vec<math_image::Pending>,
    /// `(line index, LaTeX source)`.
    pub sources: Vec<(usize, String)>,
}

/// `images` is false for callers that discard the metadata (for example the
/// user-message bubble), so they keep the ASCII renderer instead of reserving
/// blank lines for an image nobody draws.
pub(crate) fn render_with_indent_detailed(
    text: &str,
    width: usize,
    theme: Theme,
    indent: &str,
    images: bool,
    base: Option<&Path>,
) -> Rendered {
    let mut renderer = Renderer::new(width, theme, indent, images, base);
    renderer.run(text);
    renderer.finish()
}

/// Render with math forced to literal TeX, matching `/math off`.
#[cfg(test)]
fn render_raw(text: &str, width: usize, theme: Theme) -> Vec<Line<'static>> {
    let mut renderer = Renderer::new(width, theme, "  ", true, None);
    renderer.raw = true;
    renderer.run(text);
    renderer.finish().lines
}

struct Renderer {
    width: usize,
    theme: Theme,
    indent: String,
    images: bool,
    /// Keep formulas as literal TeX instead of an image or the 2D grid.
    raw: bool,
    /// Session cwd for relative image paths.
    base: Option<PathBuf>,
    lines: Vec<Line<'static>>,
    placements: Vec<math_image::Placement>,
    pending: Vec<math_image::Pending>,
    sources: Vec<(usize, String)>,
    inline: Vec<Span<'static>>,
    /// Inline math images in source order. `flush` resolves each marker in the
    /// wrapped lines to its final line and column.
    inline_slots: Vec<math_image::MathImage>,
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

/// Zero-width prefix that makes an inline-image span atomic while wrapping.
const INLINE_IMAGE_MARK: char = '\u{200B}';

struct CodeState {
    buffer: String,
    highlighter: Option<CodeHighlighter>,
}

struct TableState {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<String>>,
    header_rows: usize,
}

struct Capture {
    dest: String,
    label: String,
    /// Length of `inline` before the image/link opened, so an image that is
    /// rendered as a block can remove its placeholder text again.
    inline_start: usize,
    /// The link/image lives inside a table cell, so its text has already been
    /// appended to the cell buffer instead of the inline span list.
    in_cell: bool,
}

impl Renderer {
    fn new(width: usize, theme: Theme, indent: &str, images: bool, base: Option<&Path>) -> Self {
        let mut renderer = Self {
            width: width.max(1),
            theme,
            indent: indent.to_string(),
            images,
            raw: math_image::raw_source(),
            base: base.map(PathBuf::from),
            lines: Vec::new(),
            placements: Vec::new(),
            pending: Vec::new(),
            sources: Vec::new(),
            inline: Vec::new(),
            inline_slots: Vec::new(),
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

    fn run(&mut self, text: &str) {
        let options = Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_MATH
            | Options::ENABLE_FOOTNOTES;
        for event in Parser::new_ext(text, options) {
            self.event(event);
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(text) => {
                let style = Style::default().fg(inline_code(self.theme));
                self.append_styled(&text, style);
                self.capture(&text);
            }
            Event::InlineMath(text) => {
                self.inline_math(&text);
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
                let highlighter = (!lang.is_empty())
                    .then(|| CodeHighlighter::for_language(&lang, self.theme))
                    .flatten();
                self.code = Some(CodeState {
                    buffer: String::new(),
                    highlighter,
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
                    inline_start: 0,
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
                    inline_start: self.inline.len(),
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
                    let label = cap.label.trim().to_string();
                    if !cap.in_cell && self.images && self.image_block(&cap.dest, cap.inline_start)
                    {
                        // The thumbnail (if the local file could be decoded)
                        // replaced the placeholder text.
                    } else if !cap.dest.is_empty() {
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
        let mut slots = std::mem::take(&mut self.inline_slots).into_iter();
        for (index, spans) in wrapped.into_iter().enumerate() {
            let line_no = self.lines.len();
            let prefix = if index == 0 {
                first_prefix.clone()
            } else {
                cont_prefix.clone()
            };
            let mut column = Self::prefix_width(&prefix);
            let mut line = prefix;
            for span in spans {
                let mut rest = span.content.as_ref();
                while let Some(pos) = rest.find(INLINE_IMAGE_MARK) {
                    let before = &rest[..pos];
                    if !before.is_empty() {
                        line.push(Span::styled(before.to_string(), span.style));
                        column += UnicodeWidthStr::width(before);
                    }
                    rest = &rest[pos + INLINE_IMAGE_MARK.len_utf8()..];
                    let Some(image) = slots.next() else {
                        continue;
                    };
                    let cols = usize::from(image.cols.max(1));
                    self.placements.push(math_image::Placement {
                        line: line_no,
                        column: column.min(u16::MAX as usize) as u16,
                        image,
                    });
                    line.push(Span::raw(" ".repeat(cols)));
                    column += cols;
                    // The marker is followed by exactly `cols` spaces that
                    // reserve the image's cells. They have just been emitted,
                    // so skip them instead of printing them a second time.
                    rest = skip_placeholder(rest, cols);
                }
                if !rest.is_empty() {
                    line.push(Span::styled(rest.to_string(), span.style));
                    column += UnicodeWidthStr::width(rest);
                }
            }
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

    /// Try to draw a Markdown image as a terminal thumbnail. Only local files
    /// and `file://` URIs are supported; remote URLs keep the text placeholder
    /// because the TUI never fetches the network from the render path.
    fn image_block(&mut self, dest: &str, inline_start: usize) -> bool {
        if dest.is_empty()
            || dest.starts_with("http://")
            || dest.starts_with("https://")
            || dest.starts_with("data:")
        {
            return false;
        }
        let first_prefix = self.prefix_spans(true);
        let cont_prefix = self.prefix_spans(false);
        let prefix_width = Self::prefix_width(&first_prefix);
        let avail = self.width.saturating_sub(prefix_width).max(1);
        let image = math_image::render_file(
            Path::new(dest),
            self.base.as_deref(),
            Some(avail.min(u16::MAX as usize) as u16),
            None,
        );
        let Some(image) = image else {
            return false;
        };
        // The renderer already pushed a `🖼` marker and the alt text; drop them
        // now that the image will be drawn as its own block.
        self.inline.truncate(inline_start);
        self.flush();
        if let Some(item) = self.items.last_mut() {
            item.first = false;
        }
        self.ensure_blank();
        let start = self.lines.len();
        let rows = usize::from(image.rows.max(1));
        let cols = usize::from(image.cols);
        let pad = avail.saturating_sub(cols) / 2;
        let column = (prefix_width + pad).min(u16::MAX as usize) as u16;
        for row in 0..rows {
            let mut spans = if row == 0 {
                first_prefix.clone()
            } else {
                cont_prefix.clone()
            };
            if spans.is_empty() {
                spans.push(Span::raw(" "));
            }
            self.lines.push(Line::from(spans));
        }
        self.placements.push(math_image::Placement {
            line: start,
            column,
            image,
        });
        self.ensure_blank();
        true
    }

    fn display_math(&mut self, source: &str) {
        self.flush();
        self.ensure_blank();
        let first_prefix = self.prefix_spans(true);
        let cont_prefix = self.prefix_spans(false);
        let prefix_width = Self::prefix_width(&first_prefix);
        let avail = self.width.saturating_sub(prefix_width).max(1);
        let start = self.lines.len();
        let color = math_text_color(self.theme);
        if self.raw {
            for (index, line) in raw_math_lines(source, avail).into_iter().enumerate() {
                let mut spans = if index == 0 {
                    first_prefix.clone()
                } else {
                    cont_prefix.clone()
                };
                spans.push(Span::styled(line, self.style));
                self.lines.push(Line::from(spans));
            }
            if let Some(item) = self.items.last_mut() {
                item.first = false;
            }
            self.ensure_blank();
            return;
        }
        let image = self
            .images
            .then(|| math_image::render(source, avail, color));
        if let Some(Some(image)) = image {
            // Reserve the cells the terminal protocol paints over. The
            // placeholder keeps a non-zero width so block spacing and
            // selection still treat it as a real line.
            let rows = usize::from(image.rows.max(1));
            let cols = usize::from(image.cols);
            let pad = avail.saturating_sub(cols) / 2;
            let column = (prefix_width + pad).min(u16::MAX as usize) as u16;
            for row in 0..rows {
                let mut spans = if row == 0 {
                    first_prefix.clone()
                } else {
                    cont_prefix.clone()
                };
                if spans.is_empty() {
                    spans.push(Span::raw(" "));
                }
                self.lines.push(Line::from(spans));
            }
            self.placements.push(math_image::Placement {
                line: start,
                column,
                image,
            });
            for index in start..self.lines.len() {
                self.sources.push((index, source.to_string()));
            }
        } else {
            if self.images && math_image::pending(source, avail, color, false) {
                self.pending.push(math_image::Pending {
                    line: start,
                    source: source.to_string(),
                    width: avail,
                    color,
                    inline: false,
                });
            }
            let style = Style::default()
                .fg(accent(self.theme))
                .add_modifier(Modifier::BOLD);
            for (row, line) in math::display(source, avail, style).into_iter().enumerate() {
                let mut spans = if row == 0 {
                    first_prefix.clone()
                } else {
                    cont_prefix.clone()
                };
                spans.extend(line.spans);
                self.lines.push(Line::from(spans));
            }
            // Every physical line of a formula copies back as the formula
            // source; the selection deduplicates consecutive copies.
            for index in start..self.lines.len() {
                self.sources.push((index, source.to_string()));
            }
        }
        if let Some(item) = self.items.last_mut() {
            item.first = false;
        }
        self.ensure_blank();
    }

    /// Inline math becomes a single-row image when the terminal can paint one;
    /// without a graphics protocol it falls back to a compact one-line form.
    /// `/math off` keeps the literal `$...$` source instead.
    fn inline_math(&mut self, source: &str) {
        let real_math = looks_like_math(source);
        if self.cell.is_some() || !real_math || self.raw {
            self.append_styled(&format!("${source}$"), self.style);
            return;
        }
        let color = math_text_color(self.theme);
        let width = self.inline_avail();
        if self.images {
            if let Some(image) = math_image::render_inline(source, width, color) {
                let cols = usize::from(image.cols.max(1));
                if image.rows == 1 && cols <= width {
                    let mut content = String::with_capacity(cols + 1);
                    content.push(INLINE_IMAGE_MARK);
                    content.push_str(&" ".repeat(cols));
                    self.inline.push(Span::styled(content, self.style));
                    self.inline_slots.push(image);
                    return;
                }
            } else if math_image::pending(source, width, color, true) {
                self.pending.push(math_image::Pending {
                    // The paragraph has not been flushed yet, so this is its
                    // first line; close enough for viewport scheduling.
                    line: self.lines.len(),
                    source: source.to_string(),
                    width,
                    color,
                    inline: true,
                });
            }
            // Images are available but this formula needs more than one row:
            // keep the source instead of shrinking it into an unreadable
            // single-row picture.
            self.append_styled(&format!("${source}$"), self.style);
            return;
        }
        // No graphics protocol: math is still on, so render a compact
        // single-line form rather than showing raw TeX.
        let style = Style::default()
            .fg(accent(self.theme))
            .add_modifier(Modifier::ITALIC);
        self.append_styled(&math::compact(source), style);
    }

    /// Width available to a paragraph after its list/quote/heading prefix.
    fn inline_avail(&self) -> usize {
        let first = self.prefix_spans(true);
        let cont = self.prefix_spans(false);
        let prefix = Self::prefix_width(&first).max(Self::prefix_width(&cont));
        self.width.saturating_sub(prefix).max(1)
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
        let wrapped: Vec<Vec<Span<'static>>> = {
            let highlighter = self
                .code
                .as_mut()
                .and_then(|code| code.highlighter.as_mut());
            match highlighter {
                Some(highlighter) => wrap_code_spans(&highlighter.spans(&line), avail),
                None => {
                    let style = Style::default().fg(self.theme.string.unwrap_or(SUCCESS));
                    wrap_plain(&line, avail)
                        .into_iter()
                        .map(|segment| vec![Span::styled(segment, style)])
                        .collect()
                }
            }
        };
        for (index, segment) in wrapped.into_iter().enumerate() {
            let mut spans = if index == 0 {
                first_prefix.clone()
            } else {
                cont_prefix.clone()
            };
            spans.extend(segment);
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

    fn finish(mut self) -> Rendered {
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
        let mut len = self.lines.len();
        while self.lines[..len]
            .last()
            .is_some_and(|line| line.width() == 0)
        {
            len -= 1;
        }
        self.lines.truncate(len);
        self.placements.retain(|placement| placement.line < len);
        self.pending.retain(|pending| pending.line < len);
        self.sources.retain(|(line, _)| *line < len);
        Rendered {
            lines: self.lines,
            images: self.placements,
            pending: self.pending,
            sources: self.sources,
        }
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

/// Wrap code without dropping spaces: unlike prose, every character is
/// significant. Styles are preserved across the split.
fn wrap_code_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<(char, Style)>> = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    let mut current_width = 0usize;
    for span in spans {
        for ch in span.content.chars() {
            let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
            if ch_width > 0 && current_width + ch_width > width && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
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

/// Drop the `cols` placeholder spaces that follow an inline-image marker.
fn skip_placeholder(rest: &str, cols: usize) -> &str {
    rest.char_indices()
        .nth(cols)
        .map_or("", |(index, _)| &rest[index..])
}

/// Wrap styled spans, preferring breaks at spaces while preserving styles.
fn wrap_spans(spans: &[Span<'static>], width: usize) -> Vec<Vec<Span<'static>>> {
    let width = width.max(1);
    let mut lines: Vec<Vec<(char, Style)>> = Vec::new();
    let mut current: Vec<(char, Style)> = Vec::new();
    let mut current_width = 0usize;

    for span in spans {
        if span.content.starts_with(INLINE_IMAGE_MARK) {
            // The placeholder must stay on one line: splitting it would leave
            // half an image at the end of the previous row.
            let span_width = UnicodeWidthStr::width(span.content.as_ref());
            if current_width > 0 && current_width + span_width > width {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            for ch in span.content.chars() {
                current.push((ch, span.style));
            }
            current_width += span_width;
            continue;
        }
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

/// Literal TeX lines for display math when math rendering is off. The
/// delimiters stay so the source round-trips exactly when copied.
pub(crate) fn raw_math_lines(source: &str, width: usize) -> Vec<String> {
    wrap_text(&format!("$${}$$", source.trim()), width)
}

/// Text colour for a LaTeX image. The image is painted over the terminal
/// background, so a transparent theme needs an explicit foreground rather
/// than `Reset`.
pub(super) fn math_text_color(theme: Theme) -> Option<Color> {
    if let Some(color) = theme.normal {
        if !matches!(color, Color::Reset) {
            return Some(color);
        }
    }
    if light_background(theme) {
        Some(Color::Rgb(30, 34, 42))
    } else {
        None
    }
}

fn inline_code(theme: Theme) -> Color {
    if light_background(theme) {
        INLINE_CODE_LIGHT
    } else {
        INLINE_CODE_DARK
    }
}

fn dim(theme: Theme) -> Style {
    Style::default().fg(comment(theme))
}

// ----- TeX to Unicode -----------------------------------------------------

/// Decide whether an inline `$...$` span is real math rather than shell
/// syntax. `$PATH and $HOME` in a shell snippet parses as math, so a span
/// without any math syntax (and longer than one letter) stays literal.
pub(super) fn looks_like_math(source: &str) -> bool {
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

/// Map ASCII set letters (`R`, `N`, ...) to their blackboard-bold forms.
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
        "varkappa" => "ϰ",
        "digamma" => "ϝ",
        "hslash" => "ℏ",
        "imath" => "ı",
        "jmath" => "ȷ",
        "wp" => "℘",
        "complement" => "∁",
        "cong" => "≅",
        "simeq" => "≃",
        "asymp" => "≍",
        "approxeq" => "≊",
        "doteq" => "≐",
        "ll" => "≪",
        "gg" => "≫",
        "prec" => "≺",
        "succ" => "≻",
        "preceq" => "≼",
        "succeq" => "≽",
        "subsetneq" => "⊊",
        "supsetneq" => "⊋",
        "nsubseteq" => "⊈",
        "nsupseteq" => "⊉",
        "sqsubset" => "⊏",
        "sqsupset" => "⊐",
        "sqsubseteq" => "⊑",
        "sqsupseteq" => "⊒",
        "nleq" => "≰",
        "ngeq" => "≱",
        "nparallel" => "∦",
        "nmid" => "∤",
        "mid" => "∣",
        "implies" => "⟹",
        "impliedby" => "⟸",
        "iff" => "⟺",
        "longrightarrow" => "⟶",
        "longleftarrow" => "⟵",
        "longleftrightarrow" => "⟷",
        "updownarrow" => "↕",
        "Uparrow" => "⇑",
        "Downarrow" => "⇓",
        "Updownarrow" => "⇕",
        "ominus" => "⊖",
        "odot" => "⊙",
        "oslash" => "⊘",
        "uplus" => "⊎",
        "sqcup" => "⊔",
        "sqcap" => "⊓",
        "amalg" => "⨿",
        "dagger" => "†",
        "ddagger" => "‡",
        "S" => "§",
        "P" => "¶",
        "copyright" => "©",
        "pounds" => "£",
        "euro" => "€",
        "yen" => "¥",
        "triangle" => "△",
        "triangledown" => "▽",
        "triangleleft" => "◁",
        "triangleright" => "▷",
        "square" => "□",
        "blacksquare" => "■",
        "diamond" => "⋄",
        "lozenge" => "◊",
        "measuredangle" => "∡",
        "sphericalangle" => "∢",
        "top" => "⊤",
        "bot" => "⊥",
        "vdash" => "⊢",
        "dashv" => "⊣",
        "models" => "⊨",
        "smile" => "⌣",
        "frown" => "⌢",
        "lgroup" => "⟮",
        "rgroup" => "⟯",
        "lbrace" => "{",
        "rbrace" => "}",
        "lbrack" => "[",
        "rbrack" => "]",
        "backslash" => "∖",
        "dotsb" | "dotsc" | "dotsm" | "dotsi" | "dotso" => "…",
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
    fn inline_code_is_blue_without_a_background() {
        let code_span = |theme| {
            let rendered = render("Run `git commit` first.", 60, theme);
            rendered
                .iter()
                .flat_map(|line| &line.spans)
                .find(|span| span.content.as_ref() == "git commit")
                .cloned()
                .expect("inline code span")
        };

        let dark = code_span(Theme::default());
        assert_eq!(dark.style.fg, Some(INLINE_CODE_DARK));
        assert_eq!(dark.style.bg, None, "inline code must stay transparent");
        assert!(
            matches!(dark.style.fg, Some(Color::Rgb(r, g, b)) if b > r && b > g),
            "inline code should be blue, got {:?}",
            dark.style.fg
        );

        let light = Theme {
            background: Some(Color::Rgb(250, 250, 250)),
            ..Theme::default()
        };
        let light = code_span(light);
        assert_eq!(light.style.fg, Some(INLINE_CODE_LIGHT));
        assert_eq!(light.style.bg, None, "inline code must stay transparent");
    }

    #[test]
    fn fenced_code_uses_syntax_colors() {
        let rendered = render(
            "```rust\nfn main() { let x = 1; }\n```",
            80,
            Theme::default(),
        );
        let line = rendered
            .iter()
            .find(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    .contains("fn main")
            })
            .expect("code line");
        let colors: std::collections::HashSet<_> = line
            .spans
            .iter()
            .filter(|span| !span.content.trim().is_empty() && !span.content.contains('│'))
            .map(|span| span.style.fg)
            .collect();
        assert!(colors.len() > 1, "expected syntax colors, got {line:?}");
    }

    #[test]
    fn code_wrapping_keeps_every_space() {
        let spans = vec![Span::raw("let x = 1;")];
        let wrapped = wrap_code_spans(&spans, 4);
        let joined = wrapped
            .iter()
            .flatten()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(joined, "let x = 1;");
    }

    #[test]
    fn wrapped_highlighted_code_stays_within_the_pane() {
        let rendered = render(
            "```rust\nfn main() { let value = \"a very long string that needs wrapping\"; }\n```",
            32,
            Theme::default(),
        );
        assert!(rendered.iter().all(|line| line.width() <= 32));
    }

    #[test]
    fn display_math_without_a_protocol_records_copy_sources() {
        let rendered =
            render_with_indent_detailed("$$\\frac{a}{b}$$", 40, Theme::default(), "  ", true, None);
        assert!(rendered.images.is_empty());
        assert!(!rendered.lines.is_empty());
        assert_eq!(rendered.sources.len(), rendered.lines.len());
        assert!(rendered
            .sources
            .iter()
            .all(|(_, source)| source == "\\frac{a}{b}"));
    }

    #[test]
    fn renders_inline_and_display_math() {
        let output = text(&render(
            "Inline $x^2 + y_1$ and display:\n\n$$\\frac{a+b}{c} \\le \\sqrt{x} \\to \\infty$$",
            80,
            Theme::default(),
        ));
        assert!(output.contains("Inline"), "{output}");
        // Without a graphics protocol inline math is flattened to one row.
        assert!(output.contains("x^2 + y_1"), "{output}");
        assert!(!output.contains("$x^2"), "{output}");
        assert!(output.contains("a + b"));
        assert!(output.contains("───────"));
        assert!(output.contains("≤"));
        assert!(output.contains("√x"));
        assert!(output.contains("→ ∞"));
    }

    #[test]
    fn inline_math_stays_on_one_line() {
        let rendered = render("before $x^2$ after", 80, Theme::default());
        let output = text(&rendered);
        assert_eq!(output.lines().count(), 1, "{output}");
        assert!(output.contains("before x^2 after"), "{output}");
    }

    #[test]
    fn inline_math_is_not_flattened() {
        let rendered = render("value $\\frac{a+b}{c}$ end", 80, Theme::default());
        let output = text(&rendered);
        assert_eq!(output.lines().count(), 1, "{output}");
        assert!(output.contains("value (a+b)/c end"), "{output}");
    }

    #[test]
    fn inline_math_off_keeps_the_tex_source() {
        assert_eq!(
            text(&render_raw("before $x^2$ after", 80, Theme::default())),
            "  before $x^2$ after"
        );
    }

    #[test]
    fn shell_variables_are_not_treated_as_math() {
        let output = text(&render("echo $PATH and $HOME", 80, Theme::default()));
        assert!(output.contains("$PATH and $HOME"), "{output}");
    }

    #[test]
    fn math_off_renders_display_math_as_tex() {
        let output = text(&render_raw(
            "before\n\n$$\nx^a+y^b=z^c\n$$\n\nafter",
            60,
            Theme::default(),
        ));
        assert!(output.contains("$$x^a+y^b=z^c$$"), "{output}");
        assert!(output.contains("before"), "{output}");
        assert!(output.contains("after"), "{output}");
    }

    #[test]
    fn math_off_keeps_display_math_literal() {
        assert_eq!(raw_math_lines(r"x^a+y^b=z^c", 80), vec![r"$$x^a+y^b=z^c$$"]);
        let wrapped = raw_math_lines(&"x+y".repeat(10), 12);
        assert!(wrapped.iter().all(|line| line.width() <= 12), "{wrapped:?}");
        assert_eq!(wrapped.concat(), format!("$${}$$", "x+y".repeat(10)));
    }

    #[test]
    fn inline_image_marker_spaces_are_skipped_once() {
        let rest = " ".repeat(4);
        assert_eq!(skip_placeholder(&rest, 4), "");
        assert_eq!(skip_placeholder("    next", 4), "next");
        // A short remainder (for example a clipped placeholder) is consumed
        // whole rather than panicking.
        assert_eq!(skip_placeholder("  ", 4), "");
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
        assert!(output.contains("$PATH and $HOME"), "{output}");
        // Real math is rendered even when the terminal has no image support.
        assert!(output.contains("see x and"), "{output}");
        assert!(output.contains("E = mc^2"), "{output}");
        assert!(!output.contains("$x$"), "{output}");
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
            ("\\frac{a+b}{c}", vec!["a + b", "───────", "c"]),
            ("\\sqrt{\\frac{a}{b}}", vec!["√", "─"]),
            (
                "\\begin{pmatrix} a & b \\\\ c & d \\end{pmatrix}",
                vec!["⎛", "⎞", "⎝", "⎠"],
            ),
            (
                "\\lim_{x \\to 0} \\frac{\\sin x}{x} = 1",
                vec!["lim", "x → 0", "sin x"],
            ),
            ("\\sum_{i=1}^{n} i", vec!["∑", "i = 1"]),
            ("\\int_0^1 x^2 \\, dx", vec!["∫", "0", "1", "x"]),
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
    fn display_math_inserts_tex_spacing() {
        let output = text(&render("$$E=mc^2, \\quad x=-1$$", 40, Theme::default()));
        assert!(output.contains("E = mc"), "{output}");
        assert!(!output.contains('²'), "{output}");
        assert!(output.contains("x = -1"), "{output}");
    }

    #[test]
    fn display_math_renders_alignment_environments() {
        let aligned = text(&render(
            "$$\\begin{aligned} x &= 1 \\\\ 100 &= 2 \\end{aligned}$$",
            60,
            Theme::default(),
        ));
        assert!(aligned.contains("  x = 1"), "{aligned}");
        assert!(aligned.contains("100 = 2"), "{aligned}");

        let cases = text(&render(
            "$$\\begin{cases} x & x > 0 \\\\ -x & x \\le 0 \\end{cases}$$",
            60,
            Theme::default(),
        ));
        assert!(cases.contains('⎧'), "{cases}");
        assert!(cases.contains('⎩'), "{cases}");
        assert!(cases.contains("x > 0"), "{cases}");
        assert!(cases.contains("x ≤ 0"), "{cases}");
    }
}
