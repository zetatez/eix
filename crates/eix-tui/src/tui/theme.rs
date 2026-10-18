//! Colors used by the transcript, approval cards and tool output.
//!
//! Historically these were sampled from the embedded editor's colorscheme. The
//! prompt is a self-contained Vim-style editor now, so the struct is a plain
//! optional palette: every consumer falls back to a fixed color when a field is
//! `None`.

use ratatui::style::Color;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Theme {
    pub normal: Option<Color>,
    pub comment: Option<Color>,
    pub string: Option<Color>,
    pub function: Option<Color>,
    pub error: Option<Color>,
    pub diff_add: Option<Color>,
    pub diff_delete: Option<Color>,
}
