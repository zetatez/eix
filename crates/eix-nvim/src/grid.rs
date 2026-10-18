//! Cell grids fed by Neovim's redraw events.
//!
//! [`Screen`] also understands `ext_multigrid`: every window (and float /
//! message area) is a separate grid, so the host keeps a raw grid per handle,
//! tracks where each window sits, and composes the visible prompt area by
//! overlaying floating windows on top of the normal ones. The composed result
//! is exposed as `Screen::view`, a plain [`Grid`] the same shape the old
//! single-grid renderer produced.

use std::collections::{HashMap, HashSet};

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

/// Highlight table plus the default colors, shared by every grid.
#[derive(Clone, Default)]
pub struct Attributes {
    map: HashMap<u64, Attr>,
    pub fg: Option<Color>,
    pub bg: Option<Color>,
}

impl Attributes {
    fn define(&mut self, id: u64, attr: Attr) {
        self.map.insert(id, attr);
    }

    fn attr(&self, hl_id: u64) -> Option<Attr> {
        self.map.get(&hl_id).copied()
    }

    pub fn style_for(&self, hl_id: u64) -> Style {
        let attr = self.attr(hl_id).unwrap_or_default();
        let fg = attr.fg.or(self.fg).unwrap_or(Color::Reset);
        let bg = attr.bg.or(self.bg).unwrap_or(Color::Reset);
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
}

/// One Neovim grid: a `width` x `height` cell buffer with a cursor.
pub struct Grid {
    pub width: u16,
    pub height: u16,
    pub cells: Vec<Cell>,
    pub cursor_row: u16,
    pub cursor_col: u16,
    /// False when the cursor is not drawn (unknown or off-screen grid).
    pub cursor_visible: bool,
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
            cursor_visible: false,
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
        self.cursor_visible = false;
        self.dirty = true;
    }

    pub fn resize(&mut self, width: u16, height: u16) {
        self.reset(width, height);
    }

    fn clear(&mut self) {
        for cell in &mut self.cells {
            cell.text = " ".to_string();
            cell.hl_id = 0;
        }
        self.dirty = true;
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

    fn scroll(&mut self, top: usize, bot: usize, left: usize, right: usize, rows: i64, cols: i64) {
        let w = self.width as usize;
        let top = top.min(self.height as usize);
        let bot = bot.min(self.height as usize);
        let left = left.min(w);
        let right = right.min(w);
        if top >= bot || left >= right || (rows == 0 && cols == 0) {
            return;
        }
        // Copy once: src and dst rectangles overlap.
        let src_cells = self.cells.clone();
        for row in top..bot {
            for col in left..right {
                let sr = row as i64 + rows;
                let sc = col as i64 + cols;
                let inside =
                    sr >= top as i64 && sr < bot as i64 && sc >= left as i64 && sc < right as i64;
                self.cells[row * w + col] = if inside {
                    src_cells[sr as usize * w + sc as usize].clone()
                } else {
                    blank()
                };
            }
        }
        self.dirty = true;
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

/// Position of a non-floating window grid in the global UI.
#[derive(Clone, Copy)]
struct WinPos {
    grid: u64,
    row: i32,
    col: i32,
}

#[derive(Clone, Copy)]
enum Anchor {
    NorthWest,
    NorthEast,
    SouthWest,
    SouthEast,
}

/// How an overlay grid is placed on the global UI.
#[derive(Clone, Copy)]
enum Placement {
    /// Absolute screen coordinates (`win_float_pos` with `screen_row`/`col`).
    Screen { row: i32, col: i32 },
    /// Manual anchor relative to another grid (`win_float_pos` fallback).
    Anchored {
        anchor: Anchor,
        grid: u64,
        row: i32,
        col: i32,
    },
    /// Message area covering the full width at `row` on the global grid.
    Message { row: i32 },
}

/// A floating window or message grid drawn above the normal windows.
#[derive(Clone, Copy)]
struct Overlay {
    grid: u64,
    place: Placement,
    /// Higher draws later; mirrors Neovim's `compindex`.
    order: i64,
}

/// The whole Neovim screen: raw grids plus the composed prompt view.
pub struct Screen {
    /// Global UI size (grid 1), tracked for fallback and new grids.
    pub width: u16,
    pub height: u16,
    pub attrs: Attributes,
    pub mode: String,
    pub title: String,
    /// Incremented on every `flush`; useful for probes.
    pub frame: u64,
    pub dirty: bool,
    /// Composed cells for the prompt window, floats included.
    pub view: Grid,
    grids: HashMap<u64, Grid>,
    wins: HashMap<u64, WinPos>,
    overlays: HashMap<u64, Overlay>,
    /// Grids shown as separate top-level windows; not rendered in the prompt.
    external: HashSet<u64>,
    prompt_win: Option<u64>,
    first_win_grid: Option<u64>,
    cursor_grid: Option<u64>,
}

impl Screen {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            attrs: Attributes::default(),
            mode: String::new(),
            title: String::new(),
            frame: 0,
            dirty: true,
            view: Grid::new(width, height),
            grids: HashMap::new(),
            wins: HashMap::new(),
            overlays: HashMap::new(),
            external: HashSet::new(),
            prompt_win: None,
            first_win_grid: None,
            cursor_grid: None,
        }
    }

    /// Resize the fallback/global view (before Neovim's own `grid_resize`).
    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        if self.first_win_grid.is_none() {
            self.view.resize(width, height);
        }
    }

    /// Window handle that holds the prompt buffer, resolved by the host after
    /// attach so split layouts still pick the right grid as the base.
    pub fn set_prompt_win(&mut self, win: u64) {
        self.prompt_win = Some(win);
    }

    /// Text of one composed row (test/debug aid).
    pub fn line(&self, row: usize) -> String {
        self.view.line(row)
    }

    /// Consume a `redraw` notification payload. See [`unwrap_batch`] for the
    /// two wire shapes Neovim uses.
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
            "grid_resize" => self.apply_grid_resize(args),
            "grid_clear" => {
                if let Some(grid) = u64_arg(args, 0) {
                    if let Some(g) = self.grids.get_mut(&grid) {
                        g.clear();
                    }
                }
            }
            "grid_line" => self.apply_grid_line(args),
            "grid_scroll" => self.apply_grid_scroll(args),
            "grid_cursor_goto" => {
                if let (Some(grid), Some(row), Some(col)) =
                    (u64_arg(args, 0), u64_arg(args, 1), u64_arg(args, 2))
                {
                    self.cursor_grid = Some(grid);
                    if let Some(g) = self.grids.get_mut(&grid) {
                        g.cursor_row = row as u16;
                        g.cursor_col = col as u16;
                    }
                }
            }
            "grid_destroy" => {
                if let Some(grid) = u64_arg(args, 0) {
                    self.drop_grid(grid);
                }
            }
            "win_pos" => self.apply_win_pos(args),
            "win_float_pos" => self.apply_win_float_pos(args),
            "win_external_pos" => {
                if let Some(grid) = u64_arg(args, 0) {
                    self.external.insert(grid);
                }
            }
            "win_hide" | "win_close" => {
                if let Some(grid) = u64_arg(args, 0) {
                    self.forget_window(grid);
                }
            }
            "msg_set_pos" => self.apply_msg_set_pos(args),
            "hl_attr_define" => self.apply_hl(args),
            "default_colors_set" => {
                self.attrs.fg = args.first().and_then(color);
                self.attrs.bg = args.get(1).and_then(color);
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
                self.compose();
                self.dirty = true;
            }
            _ => {}
        }
    }

    fn apply_grid_resize(&mut self, args: &[Value]) {
        let (Some(grid), Some(width), Some(height)) =
            (u64_arg(args, 0), u64_arg(args, 1), u64_arg(args, 2))
        else {
            return;
        };
        if grid == 1 {
            self.width = width as u16;
            self.height = height as u16;
        }
        let (width, height) = (width as u16, height as u16);
        match self.grids.get_mut(&grid) {
            Some(g) => g.resize(width, height),
            None => {
                let g = Grid::new(width, height);
                self.grids.insert(grid, g);
            }
        }
    }

    fn apply_grid_line(&mut self, args: &[Value]) {
        let (Some(grid), Some(row), Some(col_start)) =
            (u64_arg(args, 0), u64_arg(args, 1), u64_arg(args, 2))
        else {
            return;
        };
        let Some(cells) = args.get(3).and_then(|v| v.as_array()) else {
            return;
        };
        let Some(g) = self.grids.get_mut(&grid) else {
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
                g.put(row as usize, col, text, hl_id);
                col += 1;
            }
        }
        g.dirty = true;
    }

    fn apply_grid_scroll(&mut self, args: &[Value]) {
        let (Some(grid), Some(top), Some(bot), Some(left), Some(right)) = (
            u64_arg(args, 0),
            u64_arg(args, 1),
            u64_arg(args, 2),
            u64_arg(args, 3),
            u64_arg(args, 4),
        ) else {
            return;
        };
        let rows = args.get(5).and_then(|v| v.as_i64()).unwrap_or(0);
        let cols = args.get(6).and_then(|v| v.as_i64()).unwrap_or(0);
        if let Some(g) = self.grids.get_mut(&grid) {
            g.scroll(
                top as usize,
                bot as usize,
                left as usize,
                right as usize,
                rows,
                cols,
            );
        }
    }

    fn apply_win_pos(&mut self, args: &[Value]) {
        let (Some(grid), Some(win), Some(row), Some(col)) = (
            u64_arg(args, 0),
            args.get(1).and_then(handle_value),
            i64_arg(args, 2),
            i64_arg(args, 3),
        ) else {
            return;
        };
        self.wins.insert(
            win,
            WinPos {
                grid,
                row: row as i32,
                col: col as i32,
            },
        );
        if self.first_win_grid.is_none() {
            self.first_win_grid = Some(grid);
        }
    }

    fn apply_win_float_pos(&mut self, args: &[Value]) {
        let Some(grid) = u64_arg(args, 0) else {
            return;
        };
        let anchor_grid = u64_arg(args, 3).unwrap_or(0);
        let anchor_row = num_i64(args, 4).unwrap_or(0) as i32;
        let anchor_col = num_i64(args, 5).unwrap_or(0) as i32;
        let zindex = i64_arg(args, 7).unwrap_or(0);
        let order = i64_arg(args, 8).unwrap_or(zindex);
        let screen_row = num_i64(args, 9);
        let screen_col = num_i64(args, 10);
        let place = match (screen_row, screen_col) {
            (Some(row), Some(col)) if row >= 0 && col >= 0 => Placement::Screen {
                row: row as i32,
                col: col as i32,
            },
            _ => Placement::Anchored {
                anchor: anchor_of(args.get(2).and_then(|v| v.as_str()).unwrap_or("NW")),
                grid: anchor_grid,
                row: anchor_row,
                col: anchor_col,
            },
        };
        self.overlays.insert(grid, Overlay { grid, place, order });
    }

    fn apply_msg_set_pos(&mut self, args: &[Value]) {
        let Some(grid) = u64_arg(args, 0) else {
            return;
        };
        let row = i64_arg(args, 1).unwrap_or(0) as i32;
        let zindex = i64_arg(args, 4).unwrap_or(0);
        let order = i64_arg(args, 5).unwrap_or(zindex);
        self.overlays.insert(
            grid,
            Overlay {
                grid,
                place: Placement::Message { row },
                order,
            },
        );
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
        self.attrs.define(id, attr);
    }

    fn drop_grid(&mut self, grid: u64) {
        self.grids.remove(&grid);
        self.overlays.remove(&grid);
        self.external.remove(&grid);
        self.wins.retain(|_, pos| pos.grid != grid);
        if self.cursor_grid == Some(grid) {
            self.cursor_grid = None;
        }
        if self.first_win_grid == Some(grid) {
            self.first_win_grid = self.wins.values().map(|pos| pos.grid).min();
        }
    }

    fn forget_window(&mut self, grid: u64) {
        self.wins.retain(|_, pos| pos.grid != grid);
        self.overlays.remove(&grid);
    }

    fn base_grid(&self) -> Option<u64> {
        if let Some(win) = self.prompt_win {
            if let Some(pos) = self.wins.get(&win) {
                return Some(pos.grid);
            }
        }
        self.first_win_grid
    }

    fn base_origin(&self) -> (i32, i32) {
        let Some(base) = self.base_grid() else {
            return (0, 0);
        };
        self.wins
            .values()
            .find(|pos| pos.grid == base)
            .map(|pos| (pos.row, pos.col))
            .unwrap_or((0, 0))
    }

    fn resolve_overlay(&self, overlay: &Overlay) -> (i32, i32) {
        match overlay.place {
            Placement::Screen { row, col } => (row, col),
            Placement::Message { row } => (row, 0),
            Placement::Anchored {
                anchor,
                grid,
                row,
                col,
            } => {
                let (orow, ocol) = self.grid_origin(grid);
                let (width, height) = self
                    .grids
                    .get(&overlay.grid)
                    .map(|g| (g.width as i32, g.height as i32))
                    .unwrap_or((0, 0));
                let top = match anchor {
                    Anchor::SouthWest | Anchor::SouthEast => row - height + 1,
                    _ => row,
                };
                let left = match anchor {
                    Anchor::NorthEast | Anchor::SouthEast => col - width + 1,
                    _ => col,
                };
                (orow + top, ocol + left)
            }
        }
    }

    /// Top-left corner of a grid in the global UI, for manual anchoring.
    fn grid_origin(&self, grid: u64) -> (i32, i32) {
        if grid == 0 || grid == 1 {
            return (0, 0);
        }
        if let Some(pos) = self.wins.values().find(|pos| pos.grid == grid) {
            return (pos.row, pos.col);
        }
        if let Some(overlay) = self.overlays.get(&grid) {
            return self.resolve_overlay(overlay);
        }
        (0, 0)
    }

    /// Compose `view` from the base window, other normal windows, then
    /// overlays in `compindex` order.
    fn compose(&mut self) {
        let (base_row, base_col) = self.base_origin();
        let (width, height) = match self.base_grid().and_then(|grid| self.grids.get(&grid)) {
            Some(grid) => (grid.width, grid.height),
            None => (self.width, self.height),
        };

        let mut view = std::mem::replace(&mut self.view, Grid::new(0, 0));
        view.reset(width, height);

        // Without a window (early startup, or a build without multigrid) the
        // global grid is the whole screen.
        if self.base_grid().is_none() {
            if let Some(src) = self.grids.get(&1) {
                overlay(&mut view, src, -base_row, -base_col);
            }
        }

        // Normal windows first (they never overlap).
        for pos in self.wins.values() {
            if self.overlays.contains_key(&pos.grid) || self.external.contains(&pos.grid) {
                continue;
            }
            if let Some(src) = self.grids.get(&pos.grid) {
                overlay(&mut view, src, pos.row - base_row, pos.col - base_col);
            }
        }

        // Floating windows / message areas in rendering order.
        let mut overlays: Vec<(i64, u64, i32, i32)> = self
            .overlays
            .values()
            .map(|overlay| {
                let (row, col) = self.resolve_overlay(overlay);
                (overlay.order, overlay.grid, row - base_row, col - base_col)
            })
            .collect();
        overlays.sort_by_key(|(order, ..)| *order);
        for (_, grid, row, col) in overlays {
            if let Some(src) = self.grids.get(&grid) {
                overlay(&mut view, src, row, col);
            }
        }

        self.place_cursor(&mut view, base_row, base_col);
        self.view = view;
    }

    fn place_cursor(&self, view: &mut Grid, base_row: i32, base_col: i32) {
        let Some(cursor_grid) = self.cursor_grid.or_else(|| self.base_grid()) else {
            return;
        };
        let Some(src) = self.grids.get(&cursor_grid) else {
            return;
        };
        let origin = self
            .wins
            .values()
            .find(|pos| pos.grid == cursor_grid)
            .map(|pos| (pos.row, pos.col))
            .or_else(|| {
                self.overlays
                    .get(&cursor_grid)
                    .map(|overlay| self.resolve_overlay(overlay))
            });
        let Some((row, col)) = origin else {
            return;
        };
        let x = col - base_col + src.cursor_col as i32;
        let y = row - base_row + src.cursor_row as i32;
        if x >= 0 && x < view.width as i32 && y >= 0 && y < view.height as i32 {
            view.cursor_col = x as u16;
            view.cursor_row = y as u16;
            view.cursor_visible = true;
        }
    }

    /// One-line dump of the grid topology (debug aid for `EIX_DEBUG=1`).
    pub fn debug_state(&self) -> String {
        let grids: Vec<String> = self
            .grids
            .iter()
            .map(|(grid, g)| format!("{grid}:{}x{}", g.width, g.height))
            .collect();
        let wins: Vec<String> = self
            .wins
            .iter()
            .map(|(win, pos)| format!("{win}->g{}({},{})", pos.grid, pos.row, pos.col))
            .collect();
        let overlays: Vec<String> = self
            .overlays
            .values()
            .map(|overlay| {
                let (row, col) = self.resolve_overlay(overlay);
                format!("g{}({},{})@{}", overlay.grid, row, col, overlay.order)
            })
            .collect();
        format!(
            "base={:?} prompt_win={:?} first={:?} grids=[{}] wins=[{}] overlays=[{}] view={}x{}",
            self.base_grid(),
            self.prompt_win,
            self.first_win_grid,
            grids.join(" "),
            wins.join(" "),
            overlays.join(" "),
            self.view.width,
            self.view.height
        )
    }

    /// Render the last composed frame into `area`.
    pub fn draw(&self, frame: &mut ratatui::Frame, area: ratatui::layout::Rect, show_cursor: bool) {
        draw_in_with_cursor(
            frame,
            &self.view,
            &self.attrs,
            area,
            show_cursor && self.view.cursor_visible,
        );
    }
}

/// Copy `src` onto `view` at an offset.
fn overlay(view: &mut Grid, src: &Grid, row: i32, col: i32) {
    for r in 0..src.height as i32 {
        let vr = row + r;
        if vr < 0 || vr >= view.height as i32 {
            continue;
        }
        for c in 0..src.width as i32 {
            let vc = col + c;
            if vc < 0 || vc >= view.width as i32 {
                continue;
            }
            let cell = &src.cells[r as usize * src.width as usize + c as usize];
            // Copy empty cells too: they mark the right half of a wide
            // character, and drawing a space there would erase half of it.
            view.cells[vr as usize * view.width as usize + vc as usize] = cell.clone();
        }
    }
    view.dirty = true;
}

pub fn draw_in(
    frame: &mut ratatui::Frame,
    grid: &Grid,
    attrs: &Attributes,
    area: ratatui::layout::Rect,
) {
    draw_in_with_cursor(frame, grid, attrs, area, true);
}

pub fn draw_in_with_cursor(
    frame: &mut ratatui::Frame,
    grid: &Grid,
    attrs: &Attributes,
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
                    buf.set_string(area.x + col, area.y + row, " ", attrs.style_for(cell.hl_id));
                    continue;
                }
                let style = attrs.style_for(cell.hl_id);
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

fn anchor_of(name: &str) -> Anchor {
    match name {
        "NE" => Anchor::NorthEast,
        "SW" => Anchor::SouthWest,
        "SE" => Anchor::SouthEast,
        _ => Anchor::NorthWest,
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

/// Neovim encodes API handles (Window, Buffer, Tabpage) as msgpack ext type 1
/// whose payload is itself a msgpack integer. `Value::as_u64` does not unwrap
/// that, so callers need this for `win` arguments.
pub fn handle_value(value: &Value) -> Option<u64> {
    match value {
        Value::Ext(1, payload) => decode_msgpack_uint(payload),
        _ => value.as_u64(),
    }
}

fn decode_msgpack_uint(bytes: &[u8]) -> Option<u64> {
    let (tag, rest) = bytes.split_first()?;
    match *tag {
        // Positive fixint.
        0x00..=0x7f => Some(*tag as u64),
        0xcc => rest.first().map(|value| *value as u64),
        0xcd => Some(u16::from_be_bytes(rest.get(..2)?.try_into().ok()?) as u64),
        0xce => Some(u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as u64),
        0xcf => Some(u64::from_be_bytes(rest.get(..8)?.try_into().ok()?)),
        _ => None,
    }
}

fn i64_arg(args: &[Value], index: usize) -> Option<i64> {
    args.get(index).and_then(|v| v.as_i64())
}

/// Coordinates in float events may arrive as floats (for smooth positioning)
/// or integers, depending on the Neovim version.
fn num_i64(args: &[Value], index: usize) -> Option<i64> {
    args.get(index)
        .and_then(|value| value.as_i64().or_else(|| value.as_f64().map(|n| n as i64)))
}

pub(crate) fn color(value: &Value) -> Option<Color> {
    let n = value.as_i64().filter(|n| *n >= 0)? as u32;
    Some(Color::Rgb(
        ((n >> 16) & 0xff) as u8,
        ((n >> 8) & 0xff) as u8,
        (n & 0xff) as u8,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(parts: Vec<Value>) -> Value {
        Value::Array(parts)
    }

    /// Apply a single event to a fresh screen.
    fn screen_with(events: Vec<Value>) -> Screen {
        let mut screen = Screen::new(20, 5);
        screen.apply(&events);
        screen
    }

    #[test]
    fn composes_floats_over_the_base_grid() {
        let mut screen = screen_with(vec![
            event(vec![
                Value::from("grid_resize"),
                Value::from(2u64),
                Value::from(10u64),
                Value::from(3u64),
            ]),
            event(vec![
                Value::from("win_pos"),
                Value::from(2u64),
                Value::from(100u64),
                Value::from(0u64),
                Value::from(0u64),
                Value::from(10u64),
                Value::from(3u64),
            ]),
            event(vec![
                Value::from("grid_line"),
                Value::from(2u64),
                Value::from(0u64),
                Value::from(0u64),
                Value::Array(vec![
                    Value::Array(vec![Value::from("b"), Value::from(1u64)]),
                    Value::Array(vec![Value::from("a"), Value::from(1u64)]),
                    Value::Array(vec![Value::from("s"), Value::from(1u64)]),
                    Value::Array(vec![Value::from("e"), Value::from(1u64)]),
                ]),
            ]),
        ]);
        screen.apply(&[event(vec![Value::from("flush")])]);
        assert!(screen.line(0).starts_with("base"));

        // A 4x1 float at screen position (0, 2), drawn on top.
        screen.apply(&[
            event(vec![
                Value::from("grid_resize"),
                Value::from(3u64),
                Value::from(4u64),
                Value::from(1u64),
            ]),
            event(vec![
                Value::from("win_float_pos"),
                Value::from(3u64),
                Value::from(200u64),
                Value::from("NW"),
                Value::from(2u64),
                Value::from(0u64),
                Value::from(2u64),
                Value::from(true),
                Value::from(50u64),
                Value::from(1u64),
                Value::from(0u64),
                Value::from(2u64),
            ]),
            event(vec![
                Value::from("grid_line"),
                Value::from(3u64),
                Value::from(0u64),
                Value::from(0u64),
                Value::Array(vec![
                    Value::Array(vec![Value::from("F"), Value::from(2u64)]),
                    Value::Array(vec![Value::from("L"), Value::from(2u64)]),
                    Value::Array(vec![Value::from("T"), Value::from(2u64)]),
                ]),
            ]),
            event(vec![Value::from("flush")]),
        ]);
        assert_eq!(&screen.line(0)[2..5], "FLT");
        // The float only covers the first row.
        assert!(screen.line(1).trim_end().is_empty());
    }

    #[test]
    fn manual_anchor_positions_a_float() {
        // Base window at (0,0); float anchored to grid 2 bottom-left at (0,0)
        // with size 3x2 -> top-left = (0-2+1, 0) = (-1, 0), so only its last
        // row is visible on screen row 0.
        let mut screen = screen_with(vec![
            event(vec![
                Value::from("grid_resize"),
                Value::from(2u64),
                Value::from(6u64),
                Value::from(3u64),
            ]),
            event(vec![
                Value::from("win_pos"),
                Value::from(2u64),
                Value::from(100u64),
                Value::from(0u64),
                Value::from(0u64),
                Value::from(6u64),
                Value::from(3u64),
            ]),
        ]);
        screen.apply(&[
            event(vec![
                Value::from("grid_resize"),
                Value::from(3u64),
                Value::from(3u64),
                Value::from(2u64),
            ]),
            event(vec![
                Value::from("win_float_pos"),
                Value::from(3u64),
                Value::from(200u64),
                Value::from("SW"),
                Value::from(2u64),
                Value::from(0u64),
                Value::from(0u64),
                Value::from(false),
                Value::from(50u64),
                Value::from(1u64),
            ]),
            event(vec![
                Value::from("grid_line"),
                Value::from(3u64),
                Value::from(1u64),
                Value::from(0u64),
                Value::Array(vec![
                    Value::Array(vec![Value::from("h"), Value::from(2u64)]),
                    Value::Array(vec![Value::from("i"), Value::from(2u64)]),
                ]),
            ]),
            event(vec![Value::from("flush")]),
        ]);
        assert!(screen.line(0).starts_with("hi"));
    }

    #[test]
    fn scroll_moves_rows_and_columns() {
        let mut grid = Grid::new(3, 3);
        for (i, text) in ["a", "b", "c", "d", "e", "f", "g", "h", "i"]
            .iter()
            .enumerate()
        {
            grid.put(i / 3, i % 3, text, 0);
        }
        // Move every row up by one; the bottom row becomes blank.
        grid.scroll(0, 3, 0, 3, 1, 0);
        assert_eq!(grid.line(0), "def");
        assert_eq!(grid.line(1), "ghi");
        assert_eq!(grid.line(2).trim_end(), "");
    }

    #[test]
    fn wide_characters_keep_their_second_cell() {
        let screen = screen_with(vec![
            event(vec![
                Value::from("grid_resize"),
                Value::from(1u64),
                Value::from(4u64),
                Value::from(1u64),
            ]),
            event(vec![
                Value::from("grid_line"),
                Value::from(1u64),
                Value::from(0u64),
                Value::from(0u64),
                Value::Array(vec![
                    Value::Array(vec![Value::from("你"), Value::from(1u64)]),
                    Value::Array(vec![Value::from(""), Value::from(1u64)]),
                    Value::Array(vec![Value::from("a"), Value::from(1u64)]),
                ]),
            ]),
            event(vec![Value::from("flush")]),
        ]);
        assert_eq!(screen.line(0).trim_end(), "你a");
        // The trailing cell is the wide character's second half, not a space.
        assert!(screen.view.cells[1].text.is_empty());
    }
}
