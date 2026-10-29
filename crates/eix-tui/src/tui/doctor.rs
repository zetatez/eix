//! Shared rendering for `doctor.report`.
//!
//! The daemon reports diagnostics as flat `label value` lines so the protocol
//! stays simple and older clients keep working. Both `/doctor` and
//! `eix doctor` group those lines into sections and align the values here.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Sections in display order. Unknown labels fall into `details`.
const SECTIONS: &[&str] = &[
    "paths",
    "runtime",
    "model",
    "tools",
    "compatibility",
    "context",
    "details",
];

/// A single display row. `Plain` and `Styled` renderers walk the same rows so
/// scroll offsets stay valid whichever view is active.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    Summary,
    Blank,
    Section(&'static str),
    Field { label: String, value: String },
    Note(String),
    Warning(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DoctorReport {
    rows: Vec<Row>,
    warnings: usize,
    label_width: usize,
}

/// Colors for the TUI text overlay. Kept as plain values so this module does
/// not depend on the app's theme helpers.
#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub accent: Color,
    pub text: Color,
    pub muted: Color,
    pub ok: Color,
    pub warn: Color,
    pub danger: Color,
}

impl DoctorReport {
    /// Group flat `label value` lines into sections. Values keep their internal
    /// whitespace; only the first run of whitespace separates the label.
    pub(crate) fn parse(lines: &[String]) -> Self {
        let mut buckets: Vec<(&'static str, Vec<(String, String)>)> = SECTIONS
            .iter()
            .map(|section| (*section, Vec::new()))
            .collect();
        let mut notes = Vec::new();
        let mut warnings = Vec::new();

        for line in lines {
            let line = line.trim_end();
            if line.trim().is_empty() {
                continue;
            }
            if let Some(rest) = line.strip_prefix("warning").and_then(|rest| {
                if rest.starts_with(char::is_whitespace) {
                    Some(rest.trim())
                } else {
                    None
                }
            }) {
                warnings.push(rest.to_string());
                continue;
            }
            let Some((label, value)) = split_label(line) else {
                notes.push(line.trim().to_string());
                continue;
            };
            let section = section_for(&label);
            if let Some(bucket) = buckets.iter_mut().find(|(name, _)| *name == section) {
                bucket.1.push((label, value));
            } else {
                notes.push(line.trim().to_string());
            }
        }

        // Build sections explicitly so a section header lands before its rows
        // even when the input interleaves labels from different sections.
        let mut rows = vec![Row::Summary, Row::Blank];
        let mut label_width = 0;
        for (name, fields) in buckets {
            if fields.is_empty() {
                continue;
            }
            rows.push(Row::Section(name));
            for (label, value) in fields {
                label_width = label_width.max(UnicodeWidthStr::width(label.as_str()));
                rows.push(Row::Field { label, value });
            }
            rows.push(Row::Blank);
        }
        if !notes.is_empty() {
            rows.push(Row::Section("notes"));
            rows.extend(notes.into_iter().map(Row::Note));
            rows.push(Row::Blank);
        }
        if !warnings.is_empty() {
            rows.push(Row::Section("warnings"));
            rows.extend(warnings.iter().cloned().map(Row::Warning));
            rows.push(Row::Blank);
        }
        // Drop the trailing spacer so the overlay does not open on a blank row.
        while matches!(rows.last(), Some(Row::Blank)) {
            rows.pop();
        }

        Self {
            rows,
            warnings: warnings.len(),
            label_width: label_width.max(6),
        }
    }

    pub(crate) fn summary(&self) -> String {
        if self.warnings == 0 {
            "all checks passed".to_string()
        } else if self.warnings == 1 {
            "1 warning needs attention".to_string()
        } else {
            format!("{} warnings need attention", self.warnings)
        }
    }

    pub(crate) fn plain(&self) -> String {
        let mut out = String::new();
        for row in &self.rows {
            out.push_str(&self.plain_row(row, false));
            out.push('\n');
        }
        out
    }

    /// CLI rendering with optional ANSI colors.
    pub(crate) fn ansi(&self, colors: bool) -> String {
        let mut out = String::new();
        for row in &self.rows {
            out.push_str(&self.plain_row(row, colors));
            out.push('\n');
        }
        out
    }

    fn plain_row(&self, row: &Row, colors: bool) -> String {
        let paint = |text: &str, code: &str| {
            if colors {
                format!("\x1b[{code}m{text}\x1b[0m")
            } else {
                text.to_string()
            }
        };
        match row {
            Row::Summary => {
                let (mark, code) = if self.warnings == 0 {
                    ("✓", "32")
                } else {
                    ("!", "33")
                };
                format!("{} {}", paint(mark, code), self.summary())
            }
            Row::Blank => String::new(),
            Row::Section(name) => format!("  {}", paint(name, "1;36")),
            Row::Field { label, value } => {
                let tone = value_tone(label, value);
                let label = pad_label(label, self.label_width);
                let value = match tone {
                    Tone::Normal => value.clone(),
                    Tone::Ok => paint(value, "32"),
                    Tone::Warn => paint(value, "33"),
                    Tone::Danger => paint(value, "31"),
                };
                format!("    {}  {value}", paint(&label, "90"))
            }
            Row::Note(text) => format!("    {}", paint(text, "90")),
            Row::Warning(text) => format!("    {} {}", paint("!", "31"), paint(text, "33")),
        }
    }

    /// Styled rows for the TUI overlay.
    pub(crate) fn styled(&self, palette: Palette) -> Vec<Line<'static>> {
        let mut lines = Vec::with_capacity(self.rows.len());
        for row in &self.rows {
            let line = match row {
                Row::Summary => {
                    let (mark, color) = if self.warnings == 0 {
                        ("✓ ", palette.ok)
                    } else {
                        ("! ", palette.warn)
                    };
                    Line::from(vec![
                        Span::styled(
                            mark.to_string(),
                            Style::default().fg(color).add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            self.summary(),
                            Style::default()
                                .fg(palette.text)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ])
                }
                Row::Blank => Line::from(""),
                Row::Section(name) => Line::from(vec![
                    Span::styled("▍ ", Style::default().fg(palette.accent)),
                    Span::styled(
                        name.to_string(),
                        Style::default()
                            .fg(palette.accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]),
                Row::Field { label, value } => {
                    let color = match value_tone(label, value) {
                        Tone::Normal => palette.text,
                        Tone::Ok => palette.ok,
                        Tone::Warn => palette.warn,
                        Tone::Danger => palette.danger,
                    };
                    Line::from(vec![
                        Span::styled("    ".to_string(), Style::default().fg(palette.muted)),
                        Span::styled(
                            pad_label(label, self.label_width),
                            Style::default().fg(palette.muted),
                        ),
                        Span::styled("  ".to_string(), Style::default().fg(palette.muted)),
                        Span::styled(value.clone(), Style::default().fg(color)),
                    ])
                }
                Row::Note(text) => Line::from(Span::styled(
                    format!("    {text}"),
                    Style::default().fg(palette.muted),
                )),
                Row::Warning(text) => Line::from(vec![
                    Span::styled(
                        "    ! ".to_string(),
                        Style::default()
                            .fg(palette.danger)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(text.clone(), Style::default().fg(palette.warn)),
                ]),
            };
            lines.push(line);
        }
        lines
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tone {
    Normal,
    Ok,
    Warn,
    Danger,
}

/// Color values that carry a pass/fail signal instead of leaving the reader to
/// parse the sentence.
fn value_tone(label: &str, value: &str) -> Tone {
    match label {
        "key" if value.starts_with("absent") => Tone::Warn,
        "key" if value.starts_with("present") => Tone::Ok,
        "config" | "eixd" if value.contains("missing") => Tone::Warn,
        "config" if value.starts_with("ERROR") => Tone::Danger,
        "daemon" if value.contains("unreachable") => Tone::Danger,
        "sandbox" if value.contains("unavailable") => Tone::Warn,
        _ => Tone::Normal,
    }
}

fn section_for(label: &str) -> &'static str {
    match label {
        "socket" | "data" | "config" | "eixd" => "paths",
        "daemon" | "sessions" => "runtime",
        "model" | "api" | "key" => "model",
        "sandbox" | "input" | "snapshot" | "rules" | "classify" => "tools",
        "compations" | "compat" => "compatibility",
        "context" => "context",
        _ => "details",
    }
}

fn split_label(line: &str) -> Option<(String, String)> {
    let line = line.trim_start();
    let end = line.find(char::is_whitespace)?;
    let label = &line[..end];
    let value = line[end..].trim();
    if label.is_empty() || value.is_empty() {
        return None;
    }
    Some((label.to_string(), value.to_string()))
}

fn pad_label(label: &str, width: usize) -> String {
    let used = UnicodeWidthStr::width(label);
    let mut padded = label.to_string();
    padded.extend(std::iter::repeat_n(' ', width.saturating_sub(used)));
    padded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines() -> Vec<String> {
        vec![
            "socket   /tmp/eix.sock".into(),
            "data     /tmp/data".into(),
            "config   /tmp/config.toml (missing, defaults)".into(),
            "daemon   eixd 0.1.0 pid 42 protocol 1 uptime 3s".into(),
            "model    provider/model".into(),
            "api      http://127.0.0.1".into(),
            "key      present".into(),
            "sandbox  bubblewrap (available)".into(),
            "classify auto in auto mode · session model".into(),
            "compations off (native .eix only)".into(),
            "context  32000 tokens · AGENTS.md found · workspace summary on".into(),
            "sessions 2".into(),
            "warning  API key is stored in the config file".into(),
        ]
    }

    #[test]
    fn groups_flat_lines_into_sections() {
        let report = DoctorReport::parse(&lines());
        let sections: Vec<&str> = report
            .rows
            .iter()
            .filter_map(|row| match row {
                Row::Section(name) => Some(*name),
                _ => None,
            })
            .collect();
        assert_eq!(
            sections,
            [
                "paths",
                "runtime",
                "model",
                "tools",
                "compatibility",
                "context",
                "warnings"
            ]
        );
        assert!(report.summary().contains("1 warning"));
        assert_eq!(report.warnings, 1);
    }

    #[test]
    fn aligns_labels_and_keeps_values_intact() {
        let report = DoctorReport::parse(&lines());
        let text = report.plain();
        assert!(text.contains("    socket      /tmp/eix.sock"), "{text}");
        assert!(text.contains("  compations"), "{text}");
        assert!(
            text.contains("    classify    auto in auto mode · session model"),
            "{text}"
        );
        assert!(text.contains("! API key is stored"), "{text}");
    }

    #[test]
    fn unparsable_lines_become_notes() {
        let report = DoctorReport::parse(&["broken".into()]);
        let text = report.plain();
        assert!(text.contains("notes"), "{text}");
        assert!(text.contains("    broken"), "{text}");
    }

    #[test]
    fn clean_report_has_no_warning_section() {
        let report = DoctorReport::parse(&["socket /tmp/eix.sock".into()]);
        let text = report.plain();
        assert!(text.contains("all checks passed"), "{text}");
        assert!(!text.contains("warnings"), "{text}");
    }
}
