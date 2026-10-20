//! A small, self-contained Vim-style modal editor for the prompt.
//!
//! The prompt used to be a real Neovim process rendered into the TUI. That
//! bought complete Vim semantics but paid for it with a second process, a full
//! config load and a visible round-trip on every keystroke. This module keeps
//! the parts that matter for writing a prompt — normal/visual editing, motions,
//! operators, counts, undo — and implements them directly on the buffer.
//!
//! Supported commands (normal mode):
//!
//! * motions: `h j k l w W b B e E 0 ^ $ gg G f F t T ; ,`
//! * operators: `d c y` with the motions above, doubled (`dd cc yy`) and text
//!   objects (`iw aw`)
//! * edits: `i a I A o O x X D C s S r ~ J p P u Ctrl-r`
//! * visual: `v V` + motions, then `d x y c p ~ o`
//! * plain `j`/`k` scroll the transcript; `3j` / `dj` stay vim motions
//! * `<Space>` is the leader for TUI actions, so plain letters stay native.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use unicode_width::UnicodeWidthChar;

/// Visible editor rows. The prompt is a fixed-height box: normal-mode editing
/// must never resize it (long drafts scroll instead).
pub const PROMPT_ROWS: u16 = 6;

type Pos = (usize, usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Visual,
    VisualLine,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Normal => "normal",
            Self::Insert => "insert",
            Self::Visual => "visual",
            Self::VisualLine => "v-line",
        }
    }
}

/// Something the host needs to act on after a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Ignored,
    Submit,
    FilePicker,
    Action(&'static str),
    /// Browse previously submitted prompts (negative = older, positive =
    /// newer). The host owns the history list; the editor only reports intent.
    History(isize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Inclusive start and end, in buffer order.
    pub start: Pos,
    pub end: Pos,
    pub linewise: bool,
}

/// One rendered row: a slice of a buffer line, since long lines wrap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub text: String,
    pub row: usize,
    /// Char index (not byte) in `row` where this segment starts.
    pub start_col: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub segments: Vec<Segment>,
    pub cursor_line: usize,
    pub cursor_col: usize,
    pub offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Delete,
    Change,
    Yank,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FindKind {
    To,
    Fill,
    ToBack,
    FillBack,
}

impl FindKind {
    fn reversed(self) -> Self {
        match self {
            Self::To => Self::ToBack,
            Self::Fill => Self::FillBack,
            Self::ToBack => Self::To,
            Self::FillBack => Self::Fill,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Range {
    Chars {
        start: Pos,
        end: Pos,
        inclusive: bool,
    },
    Lines {
        start: usize,
        end: usize,
    },
}

#[derive(Debug, Clone, Copy)]
enum Motion {
    Chars { target: Pos, inclusive: bool },
}

#[derive(Debug, Clone, Default)]
struct Register {
    lines: Vec<String>,
    linewise: bool,
}

impl Register {
    fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

#[derive(Debug, Clone)]
struct Snapshot {
    lines: Vec<String>,
    row: usize,
    col: usize,
}

#[derive(Debug, Clone)]
pub struct Editor {
    lines: Vec<String>,
    row: usize,
    col: usize,
    mode: Mode,
    /// Column remembered across `j`/`k`.
    preferred: Option<usize>,
    /// Digits typed before a command.
    count: Option<usize>,
    op: Option<Op>,
    pending_g: bool,
    pending_find: Option<FindKind>,
    pending_replace: bool,
    pending_object: Option<bool>,
    leader: bool,
    anchor: Option<Pos>,
    last_find: Option<(FindKind, char)>,
    register: Register,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            row: 0,
            col: 0,
            mode: Mode::Insert,
            preferred: None,
            count: None,
            op: None,
            pending_g: false,
            pending_find: None,
            pending_replace: false,
            pending_object: None,
            leader: false,
            anchor: None,
            last_find: None,
            register: Register::default(),
            undo: Vec::new(),
            redo: Vec::new(),
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    pub fn is_empty(&self) -> bool {
        self.lines.iter().all(|line| line.is_empty())
    }

    pub fn clear(&mut self) {
        *self = Self::new();
    }

    /// Replace the buffer (draft load, failed submit) and edit at the end.
    pub fn set_text(&mut self, text: &str) {
        *self = Self::new();
        self.lines = text.split('\n').map(str::to_string).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = self.lines.len() - 1;
        self.col = self.line_len();
        self.mode = Mode::Insert;
    }

    /// Focus the prompt without changing the buffer.
    pub fn enter_insert(&mut self) {
        self.mode = Mode::Insert;
        self.col = self.col.min(self.line_len());
    }

    /// Leave insert/visual mode without touching the text. Used after the host
    /// replaces the buffer (history recall) and wants to stay in normal mode.
    pub fn enter_normal(&mut self) {
        self.mode = Mode::Normal;
        self.clamp_normal();
    }

    /// Insert host-provided text (pickers) at the cursor.
    pub fn insert_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.begin_change();
        self.mode = Mode::Insert;
        let end = self.insert_at((self.row, self.col), text);
        self.row = end.0;
        self.col = end.1;
    }

    /// Paste a (possibly multi-line) block at the cursor.
    pub fn paste(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.begin_change();
        self.mode = Mode::Insert;
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let end = self.insert_at((self.row, self.col), &normalized);
        self.row = end.0;
        self.col = end.1;
    }

    pub fn selection(&self) -> Option<Selection> {
        if !matches!(self.mode, Mode::Visual | Mode::VisualLine) {
            return None;
        }
        let anchor = self.anchor?;
        let cursor = (self.row, self.col);
        let (start, end) = if anchor <= cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        };
        Some(Selection {
            start,
            end,
            linewise: self.mode == Mode::VisualLine,
        })
    }

    pub fn is_selected(&self, row: usize, col: usize) -> bool {
        let Some(selection) = self.selection() else {
            return false;
        };
        if selection.linewise {
            row >= selection.start.0 && row <= selection.end.0
        } else {
            (row, col) >= selection.start && (row, col) <= selection.end
        }
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Outcome {
        if key.code == KeyCode::Esc {
            self.cancel();
            return Outcome::Ignored;
        }
        if self.leader {
            self.leader = false;
            return match key.code {
                KeyCode::Char(c) => leader_action(c).map_or(Outcome::Ignored, Outcome::Action),
                _ => Outcome::Ignored,
            };
        }
        if let Some(inner) = self.pending_object.take() {
            let op = self.op.take();
            if matches!(key.code, KeyCode::Char('w')) {
                if let (Some(op), Some(range)) = (op, self.word_object(inner)) {
                    self.apply_operator(op, range);
                }
            } else {
                self.count = None;
            }
            return Outcome::Ignored;
        }
        if let Some(kind) = self.pending_find.take() {
            if let KeyCode::Char(c) = key.code {
                self.resolve_find(kind, c);
            } else {
                self.count = None;
            }
            return Outcome::Ignored;
        }
        if self.pending_replace {
            self.pending_replace = false;
            if let KeyCode::Char(c) = key.code {
                self.replace_chars(c);
            } else {
                self.count = None;
            }
            return Outcome::Ignored;
        }
        if self.pending_g {
            self.pending_g = false;
            if matches!(key.code, KeyCode::Char('g')) {
                let target = self.count.take().map(|n| n.saturating_sub(1)).unwrap_or(0);
                self.goto_row(target);
            } else {
                self.count = None;
            }
            return Outcome::Ignored;
        }

        match self.mode {
            Mode::Insert => self.insert_key(key),
            Mode::Normal => self.normal_key(key),
            Mode::Visual | Mode::VisualLine => self.visual_key(key),
        }
    }

    pub fn layout(&self, width: usize, height: usize) -> Layout {
        let width = width.max(1);
        let height = height.max(1);
        let mut segments: Vec<Segment> = Vec::new();
        let mut cursor_line = 0;
        let mut cursor_col = 0;

        for (row, line) in self.lines.iter().enumerate() {
            let chars: Vec<char> = line.chars().collect();
            let mut text = String::new();
            let mut segment_width = 0usize;
            let mut start_col = 0usize;

            if row == self.row && self.col == 0 {
                cursor_line = segments.len();
                cursor_col = 0;
            }

            for (index, &ch) in chars.iter().enumerate() {
                let ch_width = UnicodeWidthChar::width(ch).unwrap_or(0);
                if segment_width + ch_width > width && !text.is_empty() {
                    segments.push(Segment {
                        text: std::mem::take(&mut text),
                        row,
                        start_col,
                    });
                    start_col = index;
                    segment_width = 0;
                }
                if row == self.row && self.col == index {
                    cursor_line = segments.len();
                    cursor_col = segment_width;
                }
                text.push(ch);
                segment_width += ch_width;
            }

            if row == self.row && self.col >= chars.len() {
                if segment_width >= width {
                    segments.push(Segment {
                        text: std::mem::take(&mut text),
                        row,
                        start_col,
                    });
                    start_col = chars.len();
                    segment_width = 0;
                }
                cursor_line = segments.len();
                cursor_col = segment_width;
            }

            segments.push(Segment {
                text,
                row,
                start_col,
            });
        }

        if segments.is_empty() {
            segments.push(Segment {
                text: String::new(),
                row: 0,
                start_col: 0,
            });
        }

        let offset = if segments.len() > height {
            (cursor_line + 1)
                .saturating_sub(height)
                .min(segments.len().saturating_sub(height))
        } else {
            0
        };

        Layout {
            segments,
            cursor_line,
            cursor_col,
            offset,
        }
    }

    // ----------------------------------------------------------------- modes

    fn insert_key(&mut self, key: KeyEvent) -> Outcome {
        if key.code == KeyCode::Enter
            && key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::SHIFT)
        {
            self.newline();
            return Outcome::Ignored;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('j') => self.newline(),
                KeyCode::Char('w') => self.delete_word(),
                KeyCode::Char('u') => self.delete_to_start(),
                KeyCode::Char('k') => self.delete_to_end(),
                KeyCode::Char('a') => self.home(),
                KeyCode::Char('e') => self.end(),
                KeyCode::Char('h') => self.backspace(),
                _ => {}
            }
            return Outcome::Ignored;
        }
        match key.code {
            KeyCode::Enter => return Outcome::Submit,
            KeyCode::Char('@') if self.at_word_start() => return Outcome::FilePicker,
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::ALT) => self.insert_char(c),
            KeyCode::Backspace => self.backspace(),
            KeyCode::Delete => self.delete(),
            KeyCode::Left => self.left(),
            KeyCode::Right => self.right(),
            KeyCode::Up => self.up(),
            KeyCode::Down => self.down(),
            KeyCode::Home => self.home(),
            KeyCode::End => self.end(),
            KeyCode::Tab => self.insert_str_raw("    "),
            _ => {}
        }
        Outcome::Ignored
    }

    fn normal_key(&mut self, key: KeyEvent) -> Outcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            if key.code == KeyCode::Char('r') {
                self.redo();
            }
            return Outcome::Ignored;
        }
        match key.code {
            KeyCode::Esc => self.cancel(),
            KeyCode::Enter => {
                let n = self.take_count();
                self.move_rows(n as isize);
                self.col = self.first_non_blank(self.row);
            }
            KeyCode::Left => self.motion_char('h'),
            KeyCode::Right => self.motion_char('l'),
            KeyCode::Up => self.motion_char('k'),
            KeyCode::Down => self.motion_char('j'),
            KeyCode::Home => self.motion_char('0'),
            KeyCode::End => self.motion_char('$'),
            KeyCode::Delete => {
                let n = self.take_count();
                self.delete_chars(n);
            }
            KeyCode::Backspace => self.motion_char('h'),
            KeyCode::Char(c) => return self.normal_char(c),
            _ => {}
        }
        Outcome::Ignored
    }

    fn normal_char(&mut self, c: char) -> Outcome {
        match c {
            ' ' if self.op.is_none() && self.count.is_none() => {
                self.leader = true;
                return Outcome::Ignored;
            }
            '1'..='9' => {
                self.push_count(c);
                return Outcome::Ignored;
            }
            '0' if self.count.is_some() => {
                self.push_count(c);
                return Outcome::Ignored;
            }
            _ => {}
        }

        // Plain `j`/`k` browse the prompt's own submitted-message history while
        // the prompt keeps focus. Counts and operators still move within the
        // draft (`3j`, `dj`).
        if matches!(c, 'j' | 'k') && self.op.is_none() && self.count.is_none() {
            return Outcome::History(if c == 'k' { -1 } else { 1 });
        }

        if let Some(op) = self.op {
            let doubled = matches!(
                (op, c),
                (Op::Delete, 'd') | (Op::Change, 'c') | (Op::Yank, 'y')
            );
            if doubled {
                let n = self.take_count();
                self.op = None;
                let end = (self.row + n.saturating_sub(1)).min(self.lines.len() - 1);
                self.apply_operator(
                    op,
                    Range::Lines {
                        start: self.row,
                        end,
                    },
                );
                return Outcome::Ignored;
            }
            if c == 'i' || c == 'a' {
                self.pending_object = Some(c == 'i');
                return Outcome::Ignored;
            }
            self.motion_char(c);
            return Outcome::Ignored;
        }

        match c {
            'h' | 'l' | 'j' | 'k' | 'w' | 'W' | 'b' | 'B' | 'e' | 'E' | '0' | '^' | '$' | 'f'
            | 'F' | 't' | 'T' | ';' | ',' => self.motion_char(c),
            'g' => self.pending_g = true,
            'G' => {
                let target = self
                    .count
                    .take()
                    .map(|n| n.saturating_sub(1))
                    .unwrap_or(self.lines.len() - 1);
                self.goto_row(target);
            }
            'i' => self.start_insert(InsertAt::Cursor),
            'a' => self.start_insert(InsertAt::After),
            'I' => {
                self.col = self.first_non_blank(self.row);
                self.start_insert(InsertAt::Cursor);
            }
            'A' => {
                self.col = self.line_len();
                self.start_insert(InsertAt::Cursor);
            }
            'o' => self.open_line(true),
            'O' => self.open_line(false),
            'x' => {
                let n = self.take_count();
                self.delete_chars(n);
            }
            'X' => {
                let n = self.take_count();
                self.backspace_chars(n);
            }
            'D' => {
                self.op = Some(Op::Delete);
                self.motion_char('$');
            }
            'C' => {
                self.op = Some(Op::Change);
                self.motion_char('$');
            }
            'Y' => {
                let n = self.take_count();
                let end = (self.row + n.saturating_sub(1)).min(self.lines.len() - 1);
                self.apply_operator(
                    Op::Yank,
                    Range::Lines {
                        start: self.row,
                        end,
                    },
                );
            }
            's' => {
                self.op = Some(Op::Change);
                self.motion_char('l');
            }
            'S' => {
                let n = self.take_count();
                let end = (self.row + n.saturating_sub(1)).min(self.lines.len() - 1);
                self.apply_operator(
                    Op::Change,
                    Range::Lines {
                        start: self.row,
                        end,
                    },
                );
            }
            'r' => self.pending_replace = true,
            '~' => {
                let n = self.take_count();
                self.toggle_case(n);
            }
            'J' => {
                let n = self.take_count();
                self.join_lines(n);
            }
            'u' => self.undo(),
            'p' => {
                let n = self.take_count();
                self.paste_register(true, n);
            }
            'P' => {
                let n = self.take_count();
                self.paste_register(false, n);
            }
            'v' => self.enter_visual(false),
            'V' => self.enter_visual(true),
            'd' => self.op = Some(Op::Delete),
            'c' => self.op = Some(Op::Change),
            'y' => self.op = Some(Op::Yank),
            _ => self.reset_pending(),
        }
        Outcome::Ignored
    }

    fn visual_key(&mut self, key: KeyEvent) -> Outcome {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return Outcome::Ignored;
        }
        match key.code {
            KeyCode::Esc => self.cancel(),
            KeyCode::Char('v') => {
                if self.mode == Mode::VisualLine {
                    self.mode = Mode::Visual;
                } else {
                    self.cancel();
                }
            }
            KeyCode::Char('V') => {
                self.mode = if self.mode == Mode::VisualLine {
                    Mode::Visual
                } else {
                    Mode::VisualLine
                };
            }
            KeyCode::Char('o') => {
                if let Some(anchor) = self.anchor {
                    self.anchor = Some((self.row, self.col));
                    self.set_pos(anchor);
                }
            }
            KeyCode::Char('d') | KeyCode::Char('x') => self.visual_operator(Op::Delete),
            KeyCode::Char('y') => self.visual_operator(Op::Yank),
            KeyCode::Char('c') => self.visual_operator(Op::Change),
            KeyCode::Char('p') | KeyCode::Char('P') => self.visual_paste(),
            KeyCode::Char('~') => self.visual_toggle_case(),
            KeyCode::Left => self.motion_char('h'),
            KeyCode::Right => self.motion_char('l'),
            KeyCode::Up => self.motion_char('k'),
            KeyCode::Down => self.motion_char('j'),
            KeyCode::Home => self.motion_char('0'),
            KeyCode::End => self.motion_char('$'),
            KeyCode::Char('g') => self.pending_g = true,
            KeyCode::Char(c @ '1'..='9') => self.push_count(c),
            KeyCode::Char('0') if self.count.is_some() => self.push_count('0'),
            KeyCode::Char(c) => self.motion_char(c),
            _ => {}
        }
        Outcome::Ignored
    }

    // -------------------------------------------------------------- motions

    fn motion_char(&mut self, c: char) {
        match c {
            'j' | 'k' => {
                let n = self.take_count();
                let target = if c == 'j' {
                    (self.row + n).min(self.lines.len() - 1)
                } else {
                    self.row.saturating_sub(n)
                };
                if let Some(op) = self.op.take() {
                    self.apply_operator(
                        op,
                        Range::Lines {
                            start: self.row.min(target),
                            end: self.row.max(target),
                        },
                    );
                } else {
                    let preferred = self.preferred.unwrap_or(self.col);
                    self.preferred = Some(preferred);
                    self.row = target;
                    self.col = preferred.min(self.line_len().saturating_sub(1));
                }
                return;
            }
            'g' => {
                self.pending_g = true;
                return;
            }
            'f' | 'F' | 't' | 'T' => {
                self.pending_find = Some(match c {
                    'f' => FindKind::To,
                    'F' => FindKind::ToBack,
                    't' => FindKind::Fill,
                    _ => FindKind::FillBack,
                });
                return;
            }
            ';' | ',' => {
                let Some((kind, target)) = self.last_find else {
                    return;
                };
                let kind = if c == ',' { kind.reversed() } else { kind };
                self.resolve_find(kind, target);
                return;
            }
            _ => {}
        }

        let op = self.op.take();
        let n = self.take_count();
        let motion = match c {
            'h' => Motion::Chars {
                target: (self.row, self.col.saturating_sub(n)),
                inclusive: false,
            },
            'l' => Motion::Chars {
                target: (self.row, (self.col + n).min(self.line_len())),
                inclusive: false,
            },
            'w' => Motion::Chars {
                target: self.word_motion('w', n),
                inclusive: false,
            },
            'W' => Motion::Chars {
                target: self.word_motion('W', n),
                inclusive: false,
            },
            'b' => Motion::Chars {
                target: self.word_motion('b', n),
                inclusive: false,
            },
            'B' => Motion::Chars {
                target: self.word_motion('B', n),
                inclusive: false,
            },
            'e' => Motion::Chars {
                target: self.word_motion('e', n),
                inclusive: true,
            },
            'E' => Motion::Chars {
                target: self.word_motion('E', n),
                inclusive: true,
            },
            '0' => Motion::Chars {
                target: (self.row, 0),
                inclusive: false,
            },
            '^' => Motion::Chars {
                target: (self.row, self.first_non_blank(self.row)),
                inclusive: false,
            },
            '$' => Motion::Chars {
                target: (self.row, self.line_len().saturating_sub(1)),
                inclusive: true,
            },
            _ => return,
        };
        self.commit_motion(motion, op);
    }

    fn commit_motion(&mut self, motion: Motion, op: Option<Op>) {
        match motion {
            Motion::Chars { target, inclusive } => {
                if let Some(op) = op {
                    self.apply_operator(
                        op,
                        Range::Chars {
                            start: (self.row, self.col),
                            end: target,
                            inclusive,
                        },
                    );
                } else {
                    self.preferred = None;
                    self.move_to(target);
                }
            }
        }
    }

    fn goto_row(&mut self, target: usize) {
        let target = target.min(self.lines.len() - 1);
        if let Some(op) = self.op.take() {
            self.apply_operator(
                op,
                Range::Lines {
                    start: self.row.min(target),
                    end: self.row.max(target),
                },
            );
        } else {
            self.row = target;
            self.col = self.first_non_blank(self.row);
            self.preferred = None;
        }
    }

    fn resolve_find(&mut self, kind: FindKind, target: char) {
        let n = self.take_count();
        let Some(col) = self.find_target(kind, target, n) else {
            return;
        };
        self.last_find = Some((kind, target));
        let pos = (self.row, col);
        if let Some(op) = self.op.take() {
            self.apply_operator(
                op,
                Range::Chars {
                    start: (self.row, self.col),
                    end: pos,
                    inclusive: matches!(kind, FindKind::To | FindKind::Fill),
                },
            );
        } else {
            self.move_to(pos);
        }
    }

    fn find_target(&self, kind: FindKind, target: char, count: usize) -> Option<usize> {
        let chars: Vec<char> = self.line().chars().collect();
        let mut col = self.col;
        for _ in 0..count.max(1) {
            match kind {
                FindKind::To | FindKind::Fill => {
                    let mut index = col + 1;
                    while index < chars.len() && chars[index] != target {
                        index += 1;
                    }
                    if index >= chars.len() {
                        return None;
                    }
                    if kind == FindKind::Fill {
                        if index == col + 1 {
                            return None;
                        }
                        col = index - 1;
                    } else {
                        col = index;
                    }
                }
                FindKind::ToBack | FindKind::FillBack => {
                    if col == 0 {
                        return None;
                    }
                    let mut index = col - 1;
                    loop {
                        if chars[index] == target {
                            break;
                        }
                        if index == 0 {
                            return None;
                        }
                        index -= 1;
                    }
                    if kind == FindKind::FillBack {
                        if index + 1 == col {
                            return None;
                        }
                        col = index + 1;
                    } else {
                        col = index;
                    }
                }
            }
        }
        Some(col)
    }

    fn word_motion(&self, kind: char, count: usize) -> Pos {
        let big = kind.is_uppercase();
        let flat: Vec<char> = self.flat_buffer();
        let len = flat.len();
        let mut index = self.to_flat((self.row, self.col));

        for _ in 0..count.max(1) {
            match kind.to_ascii_lowercase() {
                'w' => {
                    if index >= len {
                        break;
                    }
                    let class = class_of(flat[index], big);
                    if class != 0 {
                        while index < len && class_of(flat[index], big) == class {
                            index += 1;
                        }
                    }
                    while index < len && class_of(flat[index], big) == 0 {
                        index += 1;
                    }
                }
                'b' => {
                    if index == 0 {
                        break;
                    }
                    index -= 1;
                    while index > 0 && class_of(flat[index], big) == 0 {
                        index -= 1;
                    }
                    if class_of(flat[index], big) == 0 {
                        break;
                    }
                    let class = class_of(flat[index], big);
                    while index > 0 && class_of(flat[index - 1], big) == class {
                        index -= 1;
                    }
                }
                'e' => {
                    if index + 1 >= len {
                        break;
                    }
                    index += 1;
                    while index < len && class_of(flat[index], big) == 0 {
                        index += 1;
                    }
                    if index >= len {
                        break;
                    }
                    let class = class_of(flat[index], big);
                    while index + 1 < len && class_of(flat[index + 1], big) == class {
                        index += 1;
                    }
                }
                _ => break,
            }
        }

        self.pos_from_flat(index)
    }

    // -------------------------------------------------------------- operators

    fn apply_operator(&mut self, op: Op, range: Range) {
        let (text, linewise, start) = self.range_snapshot(range);
        self.register = Register {
            lines: text,
            linewise,
        };
        match op {
            Op::Yank => {
                self.set_pos(start);
                self.clamp_normal();
            }
            Op::Delete => {
                self.begin_change();
                self.delete_range(range);
                self.set_pos(start);
                self.clamp_normal();
            }
            Op::Change => {
                self.begin_change();
                if linewise {
                    let (start_row, end_row) = match range {
                        Range::Lines { start, end } => (start.min(end), start.max(end)),
                        Range::Chars { .. } => unreachable!("linewise range"),
                    };
                    self.change_lines(start_row, end_row);
                } else {
                    self.delete_range(range);
                    self.set_pos(start);
                    self.col = self.col.min(self.line_len());
                    self.mode = Mode::Insert;
                }
            }
        }
    }

    fn range_snapshot(&self, range: Range) -> (Vec<String>, bool, Pos) {
        match range {
            Range::Chars {
                start,
                end,
                inclusive,
            } => {
                let (start, end) = self.normalize_chars(start, end, inclusive);
                (self.extract_chars(start, end), false, start)
            }
            Range::Lines { start, end } => {
                let start = start.min(end).min(self.lines.len() - 1);
                let end = start.max(end.min(self.lines.len() - 1));
                (
                    self.lines[start..=end].to_vec(),
                    true,
                    (start, self.first_non_blank(start)),
                )
            }
        }
    }

    fn delete_range(&mut self, range: Range) {
        match range {
            Range::Chars {
                start,
                end,
                inclusive,
            } => {
                let (start, end) = self.normalize_chars(start, end, inclusive);
                self.delete_chars_between(start, end);
            }
            Range::Lines { start, end } => self.delete_lines(start, end),
        }
    }

    fn normalize_chars(&self, a: Pos, b: Pos, inclusive: bool) -> (Pos, Pos) {
        let (start, end, inclusive) = if a <= b {
            (a, b, inclusive)
        } else {
            (b, a, false)
        };
        let end = if inclusive {
            if end.1 < self.line_len_at(end.0) {
                (end.0, end.1 + 1)
            } else {
                end
            }
        } else {
            end
        };
        (start, end)
    }

    fn extract_chars(&self, start: Pos, end: Pos) -> Vec<String> {
        let (start_row, start_col) = start;
        let (end_row, end_col) = end;
        if start_row == end_row {
            let text: String = self.lines[start_row]
                .chars()
                .skip(start_col)
                .take(end_col.saturating_sub(start_col))
                .collect();
            return vec![text];
        }
        let mut lines = Vec::new();
        lines.push(self.lines[start_row].chars().skip(start_col).collect());
        for row in start_row + 1..end_row {
            lines.push(self.lines[row].clone());
        }
        lines.push(self.lines[end_row].chars().take(end_col).collect());
        lines
    }

    fn delete_chars_between(&mut self, start: Pos, end: Pos) {
        if start == end {
            return;
        }
        let (start_row, start_col) = start;
        let (end_row, end_col) = end;
        if start_row == end_row {
            let from = char_to_byte(&self.lines[start_row], start_col);
            let to = char_to_byte(&self.lines[start_row], end_col);
            self.lines[start_row].replace_range(from..to, "");
        } else {
            let from = char_to_byte(&self.lines[start_row], start_col);
            let to = char_to_byte(&self.lines[end_row], end_col);
            let head = self.lines[start_row][..from].to_string();
            let tail = self.lines[end_row][to..].to_string();
            self.lines[start_row] = head + &tail;
            self.lines.drain(start_row + 1..=end_row);
        }
    }

    fn delete_lines(&mut self, start: usize, end: usize) {
        let start = start.min(self.lines.len() - 1);
        let end = end.min(self.lines.len() - 1).max(start);
        self.lines.drain(start..=end);
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = start.min(self.lines.len() - 1);
        self.col = self.first_non_blank(self.row);
    }

    fn change_lines(&mut self, start: usize, end: usize) {
        self.delete_lines(start, end);
        if !(self.lines.len() == 1 && self.lines[0].is_empty()) {
            let at = start.min(self.lines.len());
            self.lines.insert(at, String::new());
            self.row = at;
        } else {
            self.row = 0;
        }
        self.col = 0;
        self.mode = Mode::Insert;
    }

    fn word_object(&self, inner: bool) -> Option<Range> {
        let chars: Vec<char> = self.line().chars().collect();
        if chars.is_empty() {
            return None;
        }
        let col = self.col.min(chars.len() - 1);
        let class = class_of(chars[col], false);
        let mut start = col;
        while start > 0 && class_of(chars[start - 1], false) == class {
            start -= 1;
        }
        let mut end = col;
        while end + 1 < chars.len() && class_of(chars[end + 1], false) == class {
            end += 1;
        }
        if inner || class == 0 {
            return Some(Range::Chars {
                start: (self.row, start),
                end: (self.row, end),
                inclusive: true,
            });
        }
        while end + 1 < chars.len() && chars[end + 1].is_whitespace() {
            end += 1;
        }
        Some(Range::Chars {
            start: (self.row, start),
            end: (self.row, end),
            inclusive: true,
        })
    }

    // ---------------------------------------------------------------- visual

    fn enter_visual(&mut self, linewise: bool) {
        self.anchor = Some((self.row, self.col));
        self.mode = if linewise {
            Mode::VisualLine
        } else {
            Mode::Visual
        };
        self.reset_pending();
    }

    fn visual_operator(&mut self, op: Op) {
        let Some(selection) = self.selection() else {
            self.cancel();
            return;
        };
        let range = if selection.linewise {
            Range::Lines {
                start: selection.start.0,
                end: selection.end.0,
            }
        } else {
            Range::Chars {
                start: selection.start,
                end: selection.end,
                inclusive: true,
            }
        };
        self.apply_operator(op, range);
        self.anchor = None;
        if op != Op::Change {
            self.mode = Mode::Normal;
            self.clamp_normal();
        }
    }

    fn visual_paste(&mut self) {
        let Some(selection) = self.selection() else {
            return;
        };
        let range = if selection.linewise {
            Range::Lines {
                start: selection.start.0,
                end: selection.end.0,
            }
        } else {
            Range::Chars {
                start: selection.start,
                end: selection.end,
                inclusive: true,
            }
        };
        let register = self.register.clone();
        self.apply_operator(Op::Delete, range);
        self.register = register;
        self.paste_register(false, 1);
        self.mode = Mode::Normal;
        self.anchor = None;
        self.clamp_normal();
    }

    fn visual_toggle_case(&mut self) {
        let Some(selection) = self.selection() else {
            return;
        };
        self.begin_change();
        let (start, end) = if selection.linewise {
            (
                (selection.start.0, 0),
                (
                    selection.end.0,
                    self.lines[selection.end.0]
                        .chars()
                        .count()
                        .saturating_sub(1),
                ),
            )
        } else {
            (selection.start, selection.end)
        };
        for row in start.0..=end.0 {
            let from = if row == start.0 { start.1 } else { 0 };
            let mut chars: Vec<char> = self.lines[row].chars().collect();
            let to = if row == end.0 {
                (end.1 + 1).min(chars.len())
            } else {
                chars.len()
            };
            for ch in chars.iter_mut().take(to).skip(from) {
                *ch = toggle_case(*ch);
            }
            self.lines[row] = chars.into_iter().collect();
        }
        self.mode = Mode::Normal;
        self.anchor = None;
        self.set_pos(start);
        self.clamp_normal();
    }

    // ----------------------------------------------------------- insert-side

    fn insert_char(&mut self, ch: char) {
        if ch == '\t' {
            for _ in 0..4 {
                self.insert_char(' ');
            }
            return;
        }
        if ch == '\n' {
            self.newline();
            return;
        }
        let index = char_to_byte(self.line(), self.col);
        self.lines[self.row].insert(index, ch);
        self.col += 1;
    }

    fn insert_str_raw(&mut self, text: &str) {
        for ch in text.chars() {
            self.insert_char(ch);
        }
    }

    fn newline(&mut self) {
        let index = char_to_byte(self.line(), self.col);
        let tail = self.lines[self.row].split_off(index);
        self.row += 1;
        self.col = 0;
        self.lines.insert(self.row, tail);
    }

    fn backspace(&mut self) {
        if self.col > 0 {
            let end = char_to_byte(self.line(), self.col);
            let start = char_to_byte(self.line(), self.col - 1);
            self.lines[self.row].replace_range(start..end, "");
            self.col -= 1;
        } else if self.row > 0 {
            let current = self.lines.remove(self.row);
            self.row -= 1;
            self.col = self.line_len();
            self.lines[self.row].push_str(&current);
        }
    }

    fn delete(&mut self) {
        if self.col < self.line_len() {
            let start = char_to_byte(self.line(), self.col);
            let end = char_to_byte(self.line(), self.col + 1);
            self.lines[self.row].replace_range(start..end, "");
        } else if self.row + 1 < self.lines.len() {
            let next = self.lines.remove(self.row + 1);
            self.lines[self.row].push_str(&next);
        }
    }

    fn left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.line_len();
        }
        self.preferred = None;
    }

    fn right(&mut self) {
        if self.col < self.line_len() {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
        self.preferred = None;
    }

    fn up(&mut self) {
        let preferred = self.preferred.unwrap_or(self.col);
        if self.row > 0 {
            self.row -= 1;
            self.col = preferred.min(self.line_len());
        }
        self.preferred = Some(preferred);
    }

    fn down(&mut self) {
        let preferred = self.preferred.unwrap_or(self.col);
        if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = preferred.min(self.line_len());
        }
        self.preferred = Some(preferred);
    }

    fn home(&mut self) {
        self.col = 0;
        self.preferred = None;
    }

    fn end(&mut self) {
        self.col = self.line_len();
        self.preferred = None;
    }

    fn delete_to_start(&mut self) {
        let end = char_to_byte(self.line(), self.col);
        self.lines[self.row].replace_range(..end, "");
        self.col = 0;
    }

    fn delete_to_end(&mut self) {
        let start = char_to_byte(self.line(), self.col);
        self.lines[self.row].truncate(start);
    }

    fn delete_word(&mut self) {
        if self.col == 0 {
            self.backspace();
            return;
        }
        let chars: Vec<char> = self.line().chars().collect();
        let mut start = self.col;
        while start > 0 && chars[start - 1].is_whitespace() {
            start -= 1;
        }
        while start > 0 && !chars[start - 1].is_whitespace() {
            start -= 1;
        }
        let from = char_to_byte(self.line(), start);
        let to = char_to_byte(self.line(), self.col);
        self.lines[self.row].replace_range(from..to, "");
        self.col = start;
    }

    fn start_insert(&mut self, at: InsertAt) {
        self.begin_change();
        let len = self.line_len();
        self.col = match at {
            InsertAt::Cursor => self.col.min(len),
            InsertAt::After => (self.col + 1).min(len),
        };
        self.mode = Mode::Insert;
    }

    fn open_line(&mut self, below: bool) {
        self.begin_change();
        if below {
            if self.row + 1 == self.lines.len() {
                self.lines.push(String::new());
            } else {
                self.lines.insert(self.row + 1, String::new());
            }
            self.row += 1;
        } else {
            self.lines.insert(self.row, String::new());
        }
        self.col = 0;
        self.mode = Mode::Insert;
    }

    fn delete_chars(&mut self, count: usize) {
        let len = self.line_len();
        if self.col >= len {
            return;
        }
        self.begin_change();
        let end = (self.col + count).min(len);
        self.delete_chars_between((self.row, self.col), (self.row, end));
        self.clamp_normal();
    }

    fn backspace_chars(&mut self, count: usize) {
        if self.col == 0 {
            return;
        }
        self.begin_change();
        let start = self.col.saturating_sub(count);
        self.delete_chars_between((self.row, start), (self.row, self.col));
        self.clamp_normal();
    }

    fn replace_chars(&mut self, replacement: char) {
        let count = self.take_count();
        let len = self.line_len();
        if self.col + count > len {
            return;
        }
        self.begin_change();
        let mut chars: Vec<char> = self.lines[self.row].chars().collect();
        for ch in chars.iter_mut().skip(self.col).take(count) {
            *ch = replacement;
        }
        self.lines[self.row] = chars.into_iter().collect();
        self.col = (self.col + count - 1).min(len.saturating_sub(1));
    }

    fn toggle_case(&mut self, count: usize) {
        let len = self.line_len();
        if len == 0 {
            return;
        }
        self.begin_change();
        let start = self.col;
        let end = (self.col + count).min(len);
        let mut chars: Vec<char> = self.lines[self.row].chars().collect();
        for ch in chars.iter_mut().take(end).skip(start) {
            *ch = toggle_case(*ch);
        }
        self.lines[self.row] = chars.into_iter().collect();
        self.col = end.saturating_sub(1);
    }

    fn join_lines(&mut self, count: usize) {
        let count = count.max(2);
        let end = (self.row + count - 1).min(self.lines.len() - 1);
        if self.row >= end {
            return;
        }
        self.begin_change();
        let mut joined = self.lines[self.row].trim_end().to_string();
        for row in self.row + 1..=end {
            let next = self.lines[row].trim_start();
            if !joined.is_empty() && !joined.ends_with(' ') && !next.is_empty() {
                joined.push(' ');
            }
            joined.push_str(next);
        }
        self.lines[self.row] = joined;
        self.lines.drain(self.row + 1..=end);
        self.col = self.first_non_blank(self.row);
    }

    fn paste_register(&mut self, after: bool, count: usize) {
        if self.register.is_empty() || count == 0 {
            return;
        }
        self.begin_change();
        if self.register.linewise {
            let at = if after { self.row + 1 } else { self.row };
            for repeat in 0..count {
                for (offset, line) in self.register.lines.iter().enumerate() {
                    let index = at + repeat * self.register.lines.len() + offset;
                    self.lines.insert(index.min(self.lines.len()), line.clone());
                }
            }
            self.row = at.min(self.lines.len() - 1);
            self.col = self.first_non_blank(self.row);
        } else {
            let text = self.register.lines.join("\n");
            let text = text.repeat(count);
            let mut pos = (self.row, self.col);
            if after && self.line_len() > 0 {
                pos.1 += 1;
            }
            let end = self.insert_at(pos, &text);
            self.row = end.0;
            self.col = end.1.saturating_sub(1);
            self.clamp_normal();
        }
    }

    fn insert_at(&mut self, (row, col): Pos, text: &str) -> Pos {
        let parts: Vec<&str> = text.split('\n').collect();
        if parts.len() == 1 {
            let index = char_to_byte(&self.lines[row], col);
            self.lines[row].insert_str(index, parts[0]);
            return (row, col + parts[0].chars().count());
        }
        let index = char_to_byte(&self.lines[row], col);
        let tail = self.lines[row].split_off(index);
        self.lines[row].push_str(parts[0]);
        let last = parts.len() - 1;
        for (offset, part) in parts.iter().enumerate().skip(1) {
            if offset == last {
                self.lines.insert(row + offset, format!("{part}{tail}"));
            } else {
                self.lines.insert(row + offset, (*part).to_string());
            }
        }
        (row + last, parts[last].chars().count())
    }

    // ------------------------------------------------------------- utilities

    fn line(&self) -> &str {
        &self.lines[self.row]
    }

    fn line_len(&self) -> usize {
        self.line().chars().count()
    }

    fn line_len_at(&self, row: usize) -> usize {
        self.lines[row].chars().count()
    }

    fn first_non_blank(&self, row: usize) -> usize {
        self.lines[row]
            .chars()
            .position(|ch| !ch.is_whitespace())
            .unwrap_or(0)
    }

    fn at_word_start(&self) -> bool {
        self.col == 0
            || self
                .line()
                .chars()
                .nth(self.col.saturating_sub(1))
                .is_some_and(|ch| ch.is_whitespace())
    }

    fn push_count(&mut self, ch: char) {
        let digit = ch.to_digit(10).unwrap_or(0) as usize;
        self.count = Some(
            self.count
                .unwrap_or(0)
                .saturating_mul(10)
                .saturating_add(digit),
        );
    }

    fn take_count(&mut self) -> usize {
        self.count.take().unwrap_or(1).max(1)
    }

    fn reset_pending(&mut self) {
        self.count = None;
        self.op = None;
        self.pending_g = false;
        self.pending_find = None;
        self.pending_replace = false;
        self.pending_object = None;
        self.leader = false;
    }

    fn cancel(&mut self) {
        self.reset_pending();
        self.anchor = None;
        if matches!(self.mode, Mode::Insert | Mode::Visual | Mode::VisualLine) {
            self.mode = Mode::Normal;
        }
        self.clamp_normal();
        self.preferred = None;
    }

    fn move_to(&mut self, (row, col): Pos) {
        self.row = row.min(self.lines.len() - 1);
        let len = self.line_len();
        self.col = if self.mode == Mode::Insert {
            col.min(len)
        } else {
            col.min(len.saturating_sub(1))
        };
    }

    fn move_rows(&mut self, delta: isize) {
        let row = self.row as isize + delta;
        self.row = row.clamp(0, self.lines.len() as isize - 1) as usize;
        self.col = self.col.min(self.line_len().saturating_sub(1));
    }

    fn set_pos(&mut self, (row, col): Pos) {
        self.row = row.min(self.lines.len().saturating_sub(1));
        self.col = col;
    }

    fn clamp_normal(&mut self) {
        self.col = self.col.min(self.line_len().saturating_sub(1));
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            lines: self.lines.clone(),
            row: self.row,
            col: self.col,
        }
    }

    fn begin_change(&mut self) {
        self.undo.push(self.snapshot());
        self.redo.clear();
    }

    fn undo(&mut self) {
        let Some(snapshot) = self.undo.pop() else {
            return;
        };
        self.redo.push(self.snapshot());
        self.restore(snapshot);
    }

    fn redo(&mut self) {
        let Some(snapshot) = self.redo.pop() else {
            return;
        };
        self.undo.push(self.snapshot());
        self.restore(snapshot);
    }

    fn restore(&mut self, snapshot: Snapshot) {
        self.lines = snapshot.lines;
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.row = snapshot.row.min(self.lines.len() - 1);
        self.col = snapshot.col;
        self.mode = Mode::Normal;
        self.reset_pending();
        self.preferred = None;
        self.clamp_normal();
    }

    fn flat_buffer(&self) -> Vec<char> {
        let mut flat = Vec::new();
        for (row, line) in self.lines.iter().enumerate() {
            if row > 0 {
                flat.push('\n');
            }
            flat.extend(line.chars());
        }
        flat
    }

    fn to_flat(&self, (row, col): Pos) -> usize {
        let mut index = 0;
        for line in &self.lines[..row] {
            index += line.chars().count() + 1;
        }
        index + col
    }

    fn pos_from_flat(&self, index: usize) -> Pos {
        let mut remaining = index;
        for (row, line) in self.lines.iter().enumerate() {
            let len = line.chars().count();
            if remaining <= len {
                return (row, remaining);
            }
            remaining -= len + 1;
        }
        let row = self.lines.len() - 1;
        (row, self.lines[row].chars().count())
    }
}

#[derive(Debug, Clone, Copy)]
enum InsertAt {
    Cursor,
    After,
}

fn class_of(ch: char, big_word: bool) -> u8 {
    if ch.is_whitespace() {
        0
    } else if big_word || ch.is_alphanumeric() || ch == '_' {
        1
    } else {
        2
    }
}

fn toggle_case(ch: char) -> char {
    if ch.is_uppercase() {
        ch.to_lowercase().next().unwrap_or(ch)
    } else {
        ch.to_uppercase().next().unwrap_or(ch)
    }
}

fn char_to_byte(text: &str, char_index: usize) -> usize {
    text.char_indices()
        .nth(char_index)
        .map(|(byte, _)| byte)
        .unwrap_or(text.len())
}

/// Leader (`<Space>xx`) actions. These mirror the mappings the embedded editor
/// used to install: plain letters stay native, TUI features live behind Space.
fn leader_action(ch: char) -> Option<&'static str> {
    Some(match ch {
        's' => "sessions",
        'o' => "new-pane",
        'f' => "files",
        'w' => "worktrees",
        'b' => "branches",
        'c' => "ledger",
        'C' => "context",
        'h' => "history",
        'p' => "processes",
        'e' => "diagnostics",
        'r' => "approvals",
        'n' => "new-pane",
        'N' => "new",
        'W' => "new-worktree",
        'F' => "fork",
        'x' => "cancel",
        'q' => "close-pane",
        'a' | ':' => "commands",
        '?' => "help",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor_with(text: &str) -> Editor {
        let mut editor = Editor::new();
        editor.set_text(text);
        // Tests start in normal mode at the first line, so motions are explicit.
        editor.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        press(&mut editor, "gg");
        editor
    }

    fn key(editor: &mut Editor, code: KeyCode) -> Outcome {
        editor.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn press(editor: &mut Editor, text: &str) {
        for ch in text.chars() {
            key(editor, KeyCode::Char(ch));
        }
    }

    fn ctrl(editor: &mut Editor, ch: char) -> Outcome {
        editor.handle_key(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::CONTROL))
    }

    #[test]
    fn insert_mode_types_and_submits() {
        let mut editor = Editor::new();
        for ch in "hello".chars() {
            key(&mut editor, KeyCode::Char(ch));
        }
        assert_eq!(editor.text(), "hello");
        assert_eq!(editor.mode(), Mode::Insert);
        assert_eq!(key(&mut editor, KeyCode::Enter), Outcome::Submit);
    }

    #[test]
    fn esc_enters_normal_mode_and_motions_work() {
        let mut editor = editor_with("one two");
        assert_eq!(editor.mode(), Mode::Normal);
        press(&mut editor, "0");
        assert_eq!(editor.col, 0);
        press(&mut editor, "w");
        assert_eq!(editor.col, 4);
        press(&mut editor, "$");
        assert_eq!(editor.col, 6);
        press(&mut editor, "b");
        assert_eq!(editor.col, 4);
    }

    #[test]
    fn dw_dd_and_p_edit_the_buffer() {
        let mut editor = editor_with("one two three");
        press(&mut editor, "dw");
        assert_eq!(editor.text(), "two three");

        let mut editor = editor_with("one two three\nfour");
        press(&mut editor, "dd");
        assert_eq!(editor.text(), "four");
        press(&mut editor, "p");
        assert_eq!(editor.text(), "four\none two three");
    }

    #[test]
    fn counts_repeat_motions_and_operators() {
        let mut editor = editor_with("a b c d");
        press(&mut editor, "3w");
        assert_eq!(editor.col, 6);
        press(&mut editor, "0");
        press(&mut editor, "2dw");
        assert_eq!(editor.text(), "c d");
    }

    #[test]
    fn x_and_undo_restore_text() {
        let mut editor = editor_with("abc");
        press(&mut editor, "x");
        assert_eq!(editor.text(), "bc");
        press(&mut editor, "u");
        assert_eq!(editor.text(), "abc");
        ctrl(&mut editor, 'r');
        assert_eq!(editor.text(), "bc");
    }

    #[test]
    fn visual_mode_selects_and_deletes() {
        let mut editor = editor_with("hello world");
        press(&mut editor, "v");
        press(&mut editor, "ll");
        assert_eq!(editor.mode(), Mode::Visual);
        assert!(editor.is_selected(0, 0));
        assert!(editor.is_selected(0, 2));
        assert!(!editor.is_selected(0, 3));
        press(&mut editor, "d");
        assert_eq!(editor.text(), "lo world");
        assert_eq!(editor.mode(), Mode::Normal);
    }

    #[test]
    fn visual_line_mode_deletes_whole_lines() {
        let mut editor = editor_with("one\ntwo\nthree");
        press(&mut editor, "V");
        press(&mut editor, "d");
        assert_eq!(editor.text(), "two\nthree");
    }

    #[test]
    fn find_and_till_move_within_the_line() {
        let mut editor = editor_with("alpha beta");
        press(&mut editor, "fb");
        assert_eq!(editor.col, 6); // the `b` in "beta"
        press(&mut editor, "0");
        press(&mut editor, "dt ");
        assert_eq!(editor.text(), " beta");
    }

    #[test]
    fn paste_before_and_after_the_cursor() {
        let mut editor = editor_with("ab");
        press(&mut editor, "yl");
        press(&mut editor, "p");
        assert_eq!(editor.text(), "aab");
        press(&mut editor, "u");
        assert_eq!(editor.text(), "ab");
    }

    #[test]
    fn enter_inserts_a_newline_on_ctrl_j() {
        let mut editor = Editor::new();
        press(&mut editor, "ab");
        editor.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL));
        press(&mut editor, "cd");
        assert_eq!(editor.text(), "ab\ncd");
    }

    #[test]
    fn at_word_start_opens_the_file_picker() {
        let mut editor = Editor::new();
        assert_eq!(key(&mut editor, KeyCode::Char('@')), Outcome::FilePicker);
        assert_eq!(editor.text(), "");
        press(&mut editor, "a");
        assert_eq!(key(&mut editor, KeyCode::Char('@')), Outcome::Ignored);
        assert_eq!(editor.text(), "a@");
    }

    #[test]
    fn space_is_the_leader_in_normal_mode() {
        let mut editor = editor_with("");
        let _ = key(&mut editor, KeyCode::Char(' '));
        assert_eq!(
            key(&mut editor, KeyCode::Char('s')),
            Outcome::Action("sessions")
        );
        // In insert mode space is a literal.
        let mut editor = Editor::new();
        key(&mut editor, KeyCode::Char(' '));
        assert_eq!(editor.text(), " ");
    }

    #[test]
    fn plain_j_and_k_browse_history_but_counts_and_operators_still_move() {
        let mut editor = editor_with("one\ntwo\nthree");
        assert_eq!(key(&mut editor, KeyCode::Char('k')), Outcome::History(-1));
        assert_eq!(key(&mut editor, KeyCode::Char('j')), Outcome::History(1));
        // A count keeps `j`/`k` as vim line motions.
        press(&mut editor, "2");
        assert_eq!(key(&mut editor, KeyCode::Char('j')), Outcome::Ignored);
        assert_eq!(editor.row, 2);
        // An operator keeps `j` as a linewise motion (`dj`).
        press(&mut editor, "d");
        assert_eq!(key(&mut editor, KeyCode::Char('j')), Outcome::Ignored);
    }

    #[test]
    fn layout_wraps_and_tracks_the_cursor() {
        let mut editor = Editor::new();
        type_text(&mut editor, "abcdef");
        let layout = editor.layout(3, 4);
        assert!(layout.segments.len() >= 2);
        assert_eq!(layout.segments[0].text, "abc");
        assert_eq!(layout.segments[1].row, 0);
        assert_eq!(layout.segments[1].start_col, 3);
    }

    fn type_text(editor: &mut Editor, text: &str) {
        for ch in text.chars() {
            key(editor, KeyCode::Char(ch));
        }
    }
}
