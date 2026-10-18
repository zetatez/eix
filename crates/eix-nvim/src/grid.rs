//! A cell grid fed by Neovim's `ext_linegrid` redraw events.

use std::collections::HashMap;

use nvim_rs::Value;
use ratatui::style::{Color, Modifier, Style};

#[derive(Clone, Default)]
pub struct Cell {
    pub text: String,
    pub hl_id: u64,
}

#[derive(Clone, Copy, Default)]
pub struct Attr {
    pub fg: Option<Color>,
    pub bg: Option<Color>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub reverse: bool,
    pub strikethrough: bool,
}

pub struct Grid {
    pub width: u16,
    pub height: u16,
    pub cells: Vec<Cell>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    pub attrs: HashMap<u64, Attr>,
    pub default_fg: Option<Color>,
    pub default_bg: Option<Color>,
    pub mode: String,
    pub title: String,
    /// Incremented on every `flush`; useful for probes.
    pub frame: u64,
    pub dirty: bool,
}

impl Grid {
    pub fn new(width: u16, height: u16) -> Self {
        let mut g = Self {
            width,
            height,
            cells: Vec::new(),
            cursor_row: 0,
            cursor_col: 0,
            attrs: HashMap::new(),
            default_fg: None,
            default_bg: None,
            mode: String::new(),
            title: String::new(),
            frame: 0,
            dirty: true,
        };
        g.reset(width, height);
        g
    }

    pub fn reset(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.cells = vec![
            Cell {
                text: " ".to_string(),
                hl_id: 0,
            };
            width as usize * height as usize
        ];
        self.cursor_row = 0;
        self.cursor_col = 0;
        self.dirty = true;
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.reset(width, height);
    }

    fn put(&mut self, row: usize, col: usize, text: &str, hl_id: u64) {
        if row >= self.height as usize || col >= self.width as usize {
            return;
        }
        let idx = row * self.width as usize + col;
        self.cells[idx] = Cell {
            text: text.to_string(),
            hl_id,
        };
    }

    /// Consume a `redraw` notification payload.
    ///
    /// Neovim documents this as a single argument holding an array (batch) of
    /// events, but what arrives over the wire is the batch itself as the params
    /// list: one element per event, with consecutive same-name events packed
    /// into a single element as `[name, param_tuple, param_tuple, ...]`.
    /// Both shapes are accepted here.
    pub fn apply(&mut self, events: &[Value]) {
        let events = unwrap_batch(events);
        for event in events {
            let Some(event) = event.as_array() else {
                continue;
            };
            let Some(name) = event.first().and_then(|v| v.as_str()) else {
                continue;
            };
            let rest = &event[1..];
            if !rest.is_empty() && rest.iter().all(|v| v.is_array()) {
                // Packed form: every remaining element is a parameter tuple.
                for tuple in rest {
                    if let Some(args) = tuple.as_array() {
                        self.apply_event(name, args);
                    }
                }
            } else {
                self.apply_event(name, rest);
            }
        }
    }

    fn apply_event(&mut self, name: &str, args: &[Value]) {
        match name {
            "grid_resize" => {
                if let (Some(w), Some(h)) = (u64_arg(args, 1), u64_arg(args, 2)) {
                    self.reset(w as u16, h as u16);
                }
            }
            "grid_clear" => {
                for cell in &mut self.cells {
                    cell.text = " ".to_string();
                    cell.hl_id = 0;
                }
                self.dirty = true;
            }
            "grid_line" => self.apply_grid_line(args),
            "grid_scroll" => self.apply_grid_scroll(args),
            "grid_cursor_goto" => {
                if let (Some(row), Some(col)) = (u64_arg(args, 1), u64_arg(args, 2)) {
                    self.cursor_row = row as u16;
                    self.cursor_col = col as u16;
                }
            }
            "hl_attr_define" => self.apply_hl(args),
            "default_colors_set" => {
                self.default_fg = args.first().and_then(color);
                self.default_bg = args.get(1).and_then(color);
            }
            "mode_change" => {
                if let Some(mode) = args.first().and_then(|v| v.as_str()) {
                    self.mode = mode.to_string();
                    self.dirty = true;
                }
            }
            "set_title" => {
                if let Some(title) = args.first().and_then(|v| v.as_str()) {
                    self.title = title.to_string();
                }
            }
            "flush" => {
                self.frame += 1;
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn apply_grid_line(&mut self, args: &[Value]) {
        let Some(row) = u64_arg(args, 1) else {
            return;
        };
        let Some(col_start) = u64_arg(args, 2) else {
            return;
        };
        let Some(cells) = args.get(3).and_then(|v| v.as_array()) else {
            return;
        };

        let mut col = col_start as usize;
        for cell in cells {
            let Some(cell) = cell.as_array() else {
                continue;
            };
            let text = cell.first().and_then(|v| v.as_str()).unwrap_or("");
            let hl_id = cell.get(1).and_then(|v| v.as_u64()).unwrap_or(0);
            let repeat = cell.get(2).and_then(|v| v.as_u64()).unwrap_or(1).max(1);
            for _ in 0..repeat {
                self.put(row as usize, col, text, hl_id);
                col += 1;
            }
        }
        self.dirty = true;
    }

    fn apply_grid_scroll(&mut self, args: &[Value]) {
        let (Some(top), Some(bot), Some(left), Some(right)) = (
            u64_arg(args, 1),
            u64_arg(args, 2),
            u64_arg(args, 3),
            u64_arg(args, 4),
        ) else {
            return;
        };
        let rows = args.get(5).and_then(|v| v.as_i64()).unwrap_or(0);
        if rows == 0 {
            return;
        }

        let w = self.width as usize;
        let top = (top as usize).min(self.height as usize);
        let bot = (bot as usize).min(self.height as usize);
        let left = (left as usize).min(w);
        let right = (right as usize).min(w);
        if top >= bot || left >= right {
            return;
        }

        let shift = rows.unsigned_abs() as usize;
        if rows > 0 {
            for row in top..bot {
                let src = row + shift;
                for col in left..right {
                    if src < bot {
                        self.cells[row * w + col] = self.cells[src * w + col].clone();
                    } else {
                        self.cells[row * w + col] = blank();
                    }
                }
            }
        } else {
            for row in (top..bot).rev() {
                if row >= top + shift {
                    let src = row - shift;
                    for col in left..right {
                        self.cells[row * w + col] = self.cells[src * w + col].clone();
                    }
                } else {
                    for col in left..right {
                        self.cells[row * w + col] = blank();
                    }
                }
            }
        }
        self.dirty = true;
    }

    fn apply_hl(&mut self, args: &[Value]) {
        let Some(id) = u64_arg(args, 0) else {
            return;
        };
        let Some(map) = args.get(1).and_then(|v| v.as_map()) else {
            return;
        };
        let mut attr = Attr::default();
        for (key, value) in map {
            match key.as_str().unwrap_or("") {
                "fg" => attr.fg = color(value),
                "bg" => attr.bg = color(value),
                "bold" => attr.bold = value.as_bool().unwrap_or(false),
                "italic" => attr.italic = value.as_bool().unwrap_or(false),
                "underline" => attr.underline = value.as_bool().unwrap_or(false),
                "reverse" => attr.reverse = value.as_bool().unwrap_or(false),
                "strikethrough" => attr.strikethrough = value.as_bool().unwrap_or(false),
                _ => {}
            }
        }
        self.attrs.insert(id, attr);
    }

    pub fn style_for(&self, hl_id: u64) -> Style {
        let attr = self.attrs.get(&hl_id).copied().unwrap_or_default();
        let fg = attr.fg.or(self.default_fg).unwrap_or(Color::Reset);
        let bg = attr.bg.or(self.default_bg).unwrap_or(Color::Reset);
        let mut style = Style::default().fg(fg).bg(bg);
        if attr.bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        if attr.italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if attr.underline {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        if attr.reverse {
            style = style.add_modifier(Modifier::REVERSED);
        }
        if attr.strikethrough {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        style
    }

    pub fn line(&self, row: usize) -> String {
        if row >= self.height as usize {
            return String::new();
        }
        let mut out = String::new();
        for col in 0..self.width as usize {
            let cell = &self.cells[row * self.width as usize + col];
            if !cell.text.is_empty() {
                out.push_str(&cell.text);
            }
        }
        out
    }
}

pub fn draw_in(frame: &mut ratatui::Frame, grid: &Grid, area: ratatui::layout::Rect) {
    draw_in_with_cursor(frame, grid, area, true);
}

pub fn draw_in_with_cursor(
    frame: &mut ratatui::Frame,
    grid: &Grid,
    area: ratatui::layout::Rect,
    show_cursor: bool,
) {
    let height = grid.height.min(area.height);
    let width = grid.width.min(area.width);

    {
        let buf = frame.buffer_mut();
        for row in 0..height {
            for col in 0..width {
                let cell = &grid.cells[row as usize * grid.width as usize + col as usize];
                // Empty text marks the second half of a wide character; the wide
                // character itself already occupied this terminal cell. Any other
                // empty cell must be cleared explicitly, otherwise stale glyphs
                // survive ratatui's diff.
                if cell.text.is_empty() {
                    let prev = if col > 0 {
                        grid.cells[row as usize * grid.width as usize + col as usize - 1]
                            .text
                            .as_str()
                    } else {
                        ""
                    };
                    if unicode_width::UnicodeWidthStr::width(prev) >= 2 {
                        continue;
                    }
                    buf.set_string(area.x + col, area.y + row, " ", grid.style_for(cell.hl_id));
                    continue;
                }
                let style = grid.style_for(cell.hl_id);
                buf.set_string(area.x + col, area.y + row, &cell.text, style);
            }
        }
    }

    if show_cursor && width > 0 && height > 0 {
        let x = area.x + grid.cursor_col.min(width - 1);
        let y = area.y + grid.cursor_row.min(height - 1);
        frame.set_cursor_position((x, y));
    }
}

fn blank() -> Cell {
    Cell {
        text: " ".to_string(),
        hl_id: 0,
    }
}

/// Accept both `[[event...], ...]` and the flattened wire shape.
fn unwrap_batch(events: &[Value]) -> &[Value] {
    if events.len() == 1 {
        if let Some(inner) = events[0].as_array() {
            // A real event starts with its name, a string.
            if inner.first().and_then(|v| v.as_str()).is_none() {
                return inner;
            }
        }
    }
    events
}

fn u64_arg(args: &[Value], index: usize) -> Option<u64> {
    args.get(index).and_then(|v| v.as_u64())
}

fn color(value: &Value) -> Option<Color> {
    let n = value.as_i64().filter(|n| *n >= 0)? as u32;
    Some(Color::Rgb(
        ((n >> 16) & 0xff) as u8,
        ((n >> 8) & 0xff) as u8,
        (n & 0xff) as u8,
    ))
}
