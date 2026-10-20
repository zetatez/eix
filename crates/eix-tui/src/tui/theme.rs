//! Colors used by the transcript, approval cards and tool output.
//!
//! Historically these were sampled from the embedded editor's colorscheme. The
//! prompt is a self-contained Vim-style editor now, so the struct is a plain
//! optional palette: every consumer falls back to a fixed color when a field is
//! `None`.

use ratatui::style::Color;

use eix_core::ThemeConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Theme {
    pub normal: Option<Color>,
    pub comment: Option<Color>,
    pub string: Option<Color>,
    pub function: Option<Color>,
    pub error: Option<Color>,
    pub diff_add: Option<Color>,
    pub diff_delete: Option<Color>,
    /// Bubble/menu background.
    pub panel: Option<Color>,
}

impl Theme {
    /// Build a palette from config. Unknown colors fall back to the default
    /// `None`, which makes each consumer use its built-in color.
    pub fn from_config(config: &ThemeConfig) -> Self {
        Self {
            normal: config.normal.as_deref().and_then(parse_color),
            comment: config.comment.as_deref().and_then(parse_color),
            string: config.string.as_deref().and_then(parse_color),
            function: config.function.as_deref().and_then(parse_color),
            error: config.error.as_deref().and_then(parse_color),
            diff_add: config.diff_add.as_deref().and_then(parse_color),
            diff_delete: config.diff_delete.as_deref().and_then(parse_color),
            panel: config.panel.as_deref().and_then(parse_color),
        }
    }
}

/// Parse `#rrggbb`, an ANSI index, or a common color name.
pub fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 6 {
            let red = u8::from_str_radix(&hex[0..2], 16).ok()?;
            let green = u8::from_str_radix(&hex[2..4], 16).ok()?;
            let blue = u8::from_str_radix(&hex[4..6], 16).ok()?;
            return Some(Color::Rgb(red, green, blue));
        }
        return None;
    }
    if let Ok(index) = value.parse::<u8>() {
        return Some(Color::Indexed(index));
    }
    Some(
        match value.to_ascii_lowercase().replace(['-', '_'], "").as_str() {
            "black" => Color::Black,
            "red" => Color::Red,
            "green" => Color::Green,
            "yellow" => Color::Yellow,
            "blue" => Color::Blue,
            "magenta" => Color::Magenta,
            "cyan" => Color::Cyan,
            "gray" | "grey" => Color::Gray,
            "darkgray" | "darkgrey" => Color::DarkGray,
            "white" => Color::White,
            "lightred" => Color::LightRed,
            "lightgreen" => Color::LightGreen,
            "lightyellow" => Color::LightYellow,
            "lightblue" => Color::LightBlue,
            "lightmagenta" => Color::LightMagenta,
            "lightcyan" => Color::LightCyan,
            _ => return None,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_names_hex_and_indices() {
        assert_eq!(parse_color("cyan"), Some(Color::Cyan));
        assert_eq!(parse_color("dark-grey"), Some(Color::DarkGray));
        assert_eq!(parse_color("#0a0b0c"), Some(Color::Rgb(10, 11, 12)));
        assert_eq!(parse_color("123"), Some(Color::Indexed(123)));
        assert_eq!(parse_color("not-a-color"), None);
    }
}
