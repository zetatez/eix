//! Syntax highlighting for diff previews.
//!
//! Diffs in the transcript and the review overlay are rendered side by side,
//! so each side owns a stateful highlighter: the old side consumes context and
//! deletion lines, the new side consumes context and insertion lines. Both use
//! the bundled `syntect` definitions and pick a light/dark theme from the app
//! palette.

use std::path::Path;
use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Style as SyntectStyle, Theme as SyntectTheme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};

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
    lines: Option<HighlightLines<'static>>,
    plain: Style,
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
        let syntax_theme = theme_for(theme, &assets().themes);
        Self {
            lines: syntax.map(|syntax| HighlightLines::new(syntax, syntax_theme)),
            plain: Style::default().fg(theme.normal.unwrap_or(Color::Gray)),
        }
    }

    /// Highlight one line, returning owned spans that can outlive the input.
    pub(super) fn spans(&mut self, line: &str) -> Vec<Span<'static>> {
        let Some(lines) = self.lines.as_mut() else {
            return vec![Span::styled(line.to_string(), self.plain)];
        };
        let assets = assets();
        match lines.highlight_line(line, &assets.syntaxes) {
            Ok(ranges) => ranges
                .into_iter()
                .filter(|(_, text)| !text.is_empty())
                .map(|(style, text)| Span::styled(text.to_string(), map_style(style)))
                .collect(),
            Err(_) => vec![Span::styled(line.to_string(), self.plain)],
        }
    }
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

fn theme_for(theme: Theme, themes: &ThemeSet) -> &SyntectTheme {
    let name = if light_background(theme) {
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

fn light_background(theme: Theme) -> bool {
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
}
