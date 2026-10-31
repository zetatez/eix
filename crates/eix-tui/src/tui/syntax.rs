//! Syntax highlighting for diff previews.
//!
//! Diffs in the transcript and the review overlay are rendered side by side,
//! so each side owns a stateful highlighter: the old side consumes context and
//! deletion lines, the new side consumes context and insertion lines. Both use
//! the bundled `syntect` definitions and pick a light/dark theme from the app
//! palette.

use std::collections::{HashMap, VecDeque};
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::highlighting::{
    FontStyle, Highlighter, Style as SyntectStyle, Theme as SyntectTheme, ThemeSet,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

use super::theme::Theme;

struct Assets {
    syntaxes: SyntaxSet,
    themes: ThemeSet,
}

static ASSETS: OnceLock<Assets> = OnceLock::new();

fn assets() -> &'static Assets {
    ASSETS.get_or_init(|| Assets {
        syntaxes: SyntaxSet::load_defaults_newlines(),
        themes: ThemeSet::load_defaults(),
    })
}

/// A stateful highlighter for one side of a side-by-side diff.
pub(super) struct CodeHighlighter {
    parse: Option<ParseState>,
    scope: ScopeStack,
    plain: Style,
    /// Syntax name identifies a cached token stream. It is `'static` because
    /// the syntax set is loaded once.
    syntax: Option<&'static str>,
    light: bool,
    /// Rolling hash of every line this highlighter consumed. Syntect's parser
    /// state is a pure function of that prefix, so the pair (prefix, line)
    /// identifies the tokenized line even though the state itself is opaque.
    prefix: u64,
}

impl CodeHighlighter {
    pub(super) fn new(path: &str, theme: Theme) -> Self {
        let syntax = syntax_for(path, &assets().syntaxes);
        Self::with_syntax(syntax, theme)
    }

    /// Build a highlighter from a fenced-code language token such as `rust`,
    /// `go` or `ts`. Unknown languages stay unstyled so the Markdown renderer
    /// can keep its normal code-block color.
    pub(super) fn for_language(language: &str, theme: Theme) -> Option<Self> {
        let syntax = syntax_for_language(language, &assets().syntaxes)?;
        Some(Self::with_syntax(Some(syntax), theme))
    }

    fn with_syntax(syntax: Option<&'static SyntaxReference>, theme: Theme) -> Self {
        Self {
            parse: syntax.map(ParseState::new),
            scope: ScopeStack::new(),
            plain: Style::default().fg(theme.normal.unwrap_or(Color::Gray)),
            syntax: syntax.map(|syntax| syntax.name.as_str()),
            light: light_background(theme),
            prefix: 0,
        }
    }

    /// Highlight one line, returning owned spans that can outlive the input.
    pub(super) fn spans(&mut self, line: &str) -> Vec<Span<'static>> {
        if self.parse.is_none() {
            return vec![Span::styled(line.to_string(), self.plain)];
        }
        let key = self.syntax.map(|syntax| SpanKey {
            syntax,
            prefix: self.prefix,
            line: hash_line(line),
        });
        if let Some(key) = key {
            if let Some(cached) = span_cache_get(&key) {
                self.prefix = extend_prefix(self.prefix, line);
                return tokens_to_spans(&cached, self.light);
            }
        }
        let Some(tokens) = self.tokenize(line) else {
            self.prefix = extend_prefix(self.prefix, line);
            return vec![Span::styled(line.to_string(), self.plain)];
        };
        let tokens = Arc::new(tokens);
        if let Some(key) = key {
            span_cache_insert(key, Arc::clone(&tokens));
        }
        self.prefix = extend_prefix(self.prefix, line);
        tokens_to_spans(&tokens, self.light)
    }

    /// Parse one line and resolve each token against both bundled syntect
    /// themes. Parsing is the expensive half; resolving both themes here means
    /// the cache never has to be invalidated by a theme switch.
    fn tokenize(&mut self, line: &str) -> Option<Vec<Token>> {
        let ops = self
            .parse
            .as_mut()?
            .parse_line(line, &assets().syntaxes)
            .ok()?;
        let mut tokens = Vec::new();
        let mut pos = 0;
        for (end, op) in ops {
            if end > pos {
                tokens.push(self.token(&line[pos..end]));
            }
            self.scope.apply(&op).ok()?;
            pos = end;
        }
        if pos < line.len() {
            tokens.push(self.token(&line[pos..]));
        }
        Some(tokens)
    }

    fn token(&self, text: &str) -> Token {
        Token {
            text: text.to_string(),
            dark: highlighter(false).style_for_stack(&self.scope.scopes),
            light: highlighter(true).style_for_stack(&self.scope.scopes),
        }
    }
}

/// One highlighted token, resolved for both bundled syntect themes.
struct Token {
    text: String,
    dark: SyntectStyle,
    light: SyntectStyle,
}

fn tokens_to_spans(tokens: &[Token], light: bool) -> Vec<Span<'static>> {
    tokens
        .iter()
        .map(|token| {
            let style = if light { token.light } else { token.dark };
            Span::styled(token.text.clone(), map_style(style))
        })
        .collect()
}

fn highlighter(light: bool) -> &'static Highlighter<'static> {
    static DARK: OnceLock<Highlighter<'static>> = OnceLock::new();
    static LIGHT: OnceLock<Highlighter<'static>> = OnceLock::new();
    let slot = if light { &LIGHT } else { &DARK };
    slot.get_or_init(|| Highlighter::new(syntect_theme(light, &assets().themes)))
}

/// Tokenizing a line is the expensive part of syntax highlighting (~0.5 ms);
/// recoloring the same tokens for another theme is not. Theme changes and pane
/// resizes replay identical line streams, so cache the parsed tokens by syntax
/// and the parser state prefix. A theme switch then costs a hash lookup per
/// line instead of a syntect parse, whichever light/dark class it crosses into.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct SpanKey {
    syntax: &'static str,
    prefix: u64,
    line: u64,
}

/// Bound the cache in lines rather than bytes: a line can hold only a handful
/// of tokens, and this is large enough for long transcripts while keeping the
/// worst-case memory in the low tens of megabytes.
const SPAN_CACHE_LINES: usize = 50_000;

#[derive(Default)]
struct SpanCache {
    map: HashMap<SpanKey, Arc<Vec<Token>>>,
    order: VecDeque<SpanKey>,
}

static SPAN_CACHE: Mutex<Option<SpanCache>> = Mutex::new(None);

fn with_span_cache<T>(f: impl FnOnce(&mut SpanCache) -> T) -> Option<T> {
    let mut slot = SPAN_CACHE.lock().ok()?;
    let cache = slot.get_or_insert_with(SpanCache::default);
    Some(f(cache))
}

fn span_cache_get(key: &SpanKey) -> Option<Arc<Vec<Token>>> {
    with_span_cache(|cache| cache.map.get(key).cloned()).flatten()
}

fn span_cache_insert(key: SpanKey, tokens: Arc<Vec<Token>>) {
    with_span_cache(|cache| {
        if cache.map.len() >= SPAN_CACHE_LINES {
            if let Some(old) = cache.order.pop_front() {
                cache.map.remove(&old);
            }
        }
        cache.order.push_back(key);
        cache.map.insert(key, tokens);
    });
}

fn hash_line(line: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    line.hash(&mut hasher);
    hasher.finish()
}

fn extend_prefix(prefix: u64, line: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    prefix.hash(&mut hasher);
    line.hash(&mut hasher);
    hasher.finish()
}

fn syntax_for<'a>(path: &str, syntaxes: &'a SyntaxSet) -> Option<&'a SyntaxReference> {
    let file_name = Path::new(path).file_name()?.to_str()?;
    if let Some(extension) = Path::new(file_name)
        .extension()
        .and_then(|ext| ext.to_str())
    {
        if let Some(syntax) = syntaxes.find_syntax_by_extension(extension) {
            return Some(syntax);
        }
    }
    syntaxes
        .find_syntax_by_token(file_name)
        .or_else(|| syntaxes.find_syntax_by_extension(file_name))
}

fn syntax_for_language<'a>(language: &str, syntaxes: &'a SyntaxSet) -> Option<&'a SyntaxReference> {
    let language = language
        .split(|ch: char| ch.is_whitespace() || matches!(ch, ',' | '{' | '}'))
        .next()?
        .trim();
    if language.is_empty() {
        return None;
    }
    // Common fence aliases that are not the syntax's file extension or name.
    let language = match language.to_ascii_lowercase().as_str() {
        "sh" | "shell" | "zsh" => "bash",
        "yml" => "yaml",
        // The bundled syntax set has no TypeScript grammar; JavaScript is a
        // close enough fallback for highlighting fenced code.
        "ts" | "tsx" | "typescript" => "javascript",
        _ => language,
    };
    syntaxes.find_syntax_by_token(language)
}

fn syntect_theme(light: bool, themes: &ThemeSet) -> &SyntectTheme {
    let name = if light {
        "InspiredGitHub"
    } else {
        "base16-ocean.dark"
    };
    themes
        .themes
        .get(name)
        .or_else(|| themes.themes.values().next())
        .expect("syntect ships at least one default theme")
}

pub(super) fn light_background(theme: Theme) -> bool {
    if let Some(background) = theme.background {
        return luminance(background) > 0.6;
    }
    // Without an explicit background, dark body text implies a light terminal.
    theme.normal.is_some_and(|color| luminance(color) < 0.4)
}

pub(super) fn luminance(color: Color) -> f32 {
    let (red, green, blue) = rgb(color).unwrap_or((0, 0, 0));
    (0.299 * f32::from(red) + 0.587 * f32::from(green) + 0.114 * f32::from(blue)) / 255.0
}

pub(super) fn rgb(color: Color) -> Option<(u8, u8, u8)> {
    Some(match color {
        Color::Rgb(red, green, blue) => (red, green, blue),
        Color::Black => (0, 0, 0),
        Color::Red => (128, 0, 0),
        Color::Green => (0, 128, 0),
        Color::Yellow => (128, 128, 0),
        Color::Blue => (0, 0, 128),
        Color::Magenta => (128, 0, 128),
        Color::Cyan => (0, 128, 128),
        Color::Gray => (192, 192, 192),
        Color::DarkGray => (128, 128, 128),
        Color::LightRed => (255, 0, 0),
        Color::LightGreen => (0, 255, 0),
        Color::LightYellow => (255, 255, 0),
        Color::LightBlue => (0, 0, 255),
        Color::LightMagenta => (255, 0, 255),
        Color::LightCyan => (0, 255, 255),
        Color::White => (255, 255, 255),
        _ => return None,
    })
}

fn map_style(style: SyntectStyle) -> Style {
    let mut mapped = Style::default().fg(Color::Rgb(
        style.foreground.r,
        style.foreground.g,
        style.foreground.b,
    ));
    if style.font_style.contains(FontStyle::BOLD) {
        mapped = mapped.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        mapped = mapped.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        mapped = mapped.add_modifier(Modifier::UNDERLINED);
    }
    mapped
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;

    #[test]
    fn picks_light_theme_for_light_backgrounds() {
        let light = Theme {
            background: Some(Color::Rgb(250, 250, 250)),
            ..Theme::default()
        };
        let dark = Theme {
            background: Some(Color::Rgb(20, 20, 20)),
            ..Theme::default()
        };
        assert!(light_background(light));
        assert!(!light_background(dark));
    }

    #[test]
    fn rust_lines_produce_multiple_styles() {
        let mut highlighter = CodeHighlighter::new("src/lib.rs", Theme::default());
        let spans = highlighter.spans("fn main() { let x = \"hi\"; }");
        let colors: std::collections::HashSet<_> = spans.iter().map(|span| span.style.fg).collect();
        assert!(colors.len() > 1, "expected syntax colors, got {spans:?}");
    }

    #[test]
    fn fenced_language_tokens_resolve() {
        for language in ["rust", "rs", "go", "python", "ts", "sh"] {
            let mut highlighter = CodeHighlighter::for_language(language, Theme::default())
                .unwrap_or_else(|| panic!("no syntax for {language}"));
            let spans = highlighter.spans("fn main() { let x = 1; }");
            assert!(!spans.is_empty(), "{language}");
        }
        assert!(CodeHighlighter::for_language("not-a-real-language", Theme::default()).is_none());
    }

    #[test]
    fn cached_spans_respect_the_parser_state() {
        let theme = Theme::default();
        let mut highlighter = CodeHighlighter::new("src/lib.rs", theme);
        highlighter.spans("/* open a block comment");
        let inside = highlighter.spans("still a comment */");

        let mut fresh = CodeHighlighter::new("src/lib.rs", theme);
        let outside = fresh.spans("still a comment */");

        // The same text highlights differently inside a comment, so the cache
        // must key on the lines consumed before it, not just the line itself.
        assert_ne!(inside, outside);
    }

    #[test]
    fn token_boundaries_are_theme_independent() {
        let dark = Theme::default();
        let light = Theme {
            background: Some(Color::Rgb(250, 250, 250)),
            ..Theme::default()
        };
        let line = "fn themed() -> u32 { 42 }";
        let mut dark_highlighter = CodeHighlighter::new("src/lib.rs", dark);
        let mut light_highlighter = CodeHighlighter::new("src/lib.rs", light);
        let dark_spans = dark_highlighter.spans(line);
        let light_spans = light_highlighter.spans(line);
        let texts = |spans: &[Span<'static>]| {
            spans
                .iter()
                .map(|span| span.content.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(texts(&dark_spans), texts(&light_spans));
        // The token boundaries match, but each class resolves its own colors.
        assert_ne!(dark_spans, light_spans);
    }

    #[test]
    fn manual_tokenizer_matches_syntect() {
        use syntect::easy::HighlightLines;

        let assets = assets();
        let syntax = syntax_for("src/lib.rs", &assets.syntaxes).expect("rust syntax");
        let mut reference = HighlightLines::new(syntax, syntect_theme(false, &assets.themes));
        let mut ours = CodeHighlighter::new("src/lib.rs", Theme::default());

        for line in [
            "fn main() {",
            "    let x = \"a string\"; /* comment",
            "    still comment */",
            "}",
        ] {
            let expected: Vec<Span<'static>> = reference
                .highlight_line(line, &assets.syntaxes)
                .expect("highlight")
                .into_iter()
                .filter(|(_, text)| !text.is_empty())
                .map(|(style, text)| Span::styled(text.to_string(), map_style(style)))
                .collect();
            assert_eq!(ours.spans(line), expected, "{line}");
        }
    }

    #[test]
    fn span_cache_round_trips() {
        let key = SpanKey {
            syntax: "Rust",
            prefix: 0x1234,
            line: 0x5678,
        };
        let color = syntect::highlighting::Color {
            r: 1,
            g: 2,
            b: 3,
            a: 255,
        };
        let style = SyntectStyle {
            foreground: color,
            background: color,
            font_style: FontStyle::empty(),
        };
        let tokens = Arc::new(vec![Token {
            text: "cached".into(),
            dark: style,
            light: style,
        }]);
        span_cache_insert(key, Arc::clone(&tokens));
        let cached = span_cache_get(&key).expect("cached tokens");
        assert_eq!(cached.len(), 1);
        assert_eq!(cached[0].text, "cached");
    }
}
