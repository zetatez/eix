//! Host-side wrapper around an embedded `nvim --embed` prompt.
//!
//! The TUI is synchronous, so keys and resizes are queued to a worker task that
//! owns the RPC connection. The `redraw` handler fills a [`Screen`] and pushes
//! submitted drafts, both behind a mutex the UI can read without awaiting.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use crossterm::event::KeyEvent;
use nvim_rs::compat::tokio::Compat;
use nvim_rs::create::tokio as create_nvim;
use nvim_rs::error::LoopError;
use nvim_rs::{Handler, Neovim, UiAttachOptions, Value};
use ratatui::layout::Rect;
use ratatui::style::Color;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::grid::{self, Screen};
use crate::keys;
use crate::prompt;

pub type Nvim = Neovim<Compat<ChildStdin>>;

/// Colors sampled from Neovim's highlight groups so the transcript can match
/// the user's colorscheme. Every field is `None` until the theme arrives, so
/// the host keeps its defaults if Neovim never reports one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Theme {
    pub normal: Option<Color>,
    pub comment: Option<Color>,
    pub string: Option<Color>,
    pub function: Option<Color>,
    pub error: Option<Color>,
    pub diff_add: Option<Color>,
    pub diff_delete: Option<Color>,
}

impl Theme {
    /// True when no group resolved (minimal nvim, or `nvim_get_hl` missing).
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// State written by the RPC handler and read by the UI.
pub struct Shared {
    pub screen: Screen,
    /// Drafts submitted with `<CR>`, oldest first.
    pub submitted: Vec<String>,
    /// Frames observed by the host (mirrors `screen.frame`).
    pub frames: u64,
    /// Number of lines in the prompt buffer, refreshed after every command.
    pub lines: u16,
    /// True when the user's config was skipped because it failed or hung.
    pub fallback: bool,
    /// Highlight colors last reported by Neovim.
    pub theme: Theme,
}

impl Shared {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            screen: Screen::new(width, height),
            submitted: Vec::new(),
            frames: 0,
            lines: 1,
            fallback: false,
            theme: Theme::default(),
        }
    }
}

#[derive(Clone)]
pub struct NvimHandler {
    pub shared: Arc<Mutex<Shared>>,
}

#[async_trait::async_trait]
impl Handler for NvimHandler {
    type Writer = Compat<ChildStdin>;

    async fn handle_notify(&self, name: String, args: Vec<Value>, _nvim: Nvim) {
        match name.as_str() {
            "redraw" => {
                // Neovim packs the redraw events directly into the notification
                // params: `redraw(event, event, ...)`.
                if let Ok(mut shared) = self.shared.lock() {
                    shared.screen.apply(&args);
                    shared.frames = shared.screen.frame;
                }
            }
            "eix_submit" => {
                let text = args
                    .first()
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if let Ok(mut shared) = self.shared.lock() {
                    shared.submitted.push(text);
                }
            }
            "eix_theme" => {
                let theme = args.first().map(parse_theme).unwrap_or_default();
                if let Ok(mut shared) = self.shared.lock() {
                    shared.theme = theme;
                }
            }
            _ => {}
        }
    }
}

/// True when an executable `nvim` is on `PATH`.
pub fn nvim_available() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join("nvim").is_file())
}

/// Clear the prompt buffer and drop back into insert mode.
const RESET_LUA: &str = r#"
local b = vim.api.nvim_get_current_buf()
vim.api.nvim_buf_set_lines(b, 0, -1, false, { "" })
vim.api.nvim_win_set_cursor(0, { 1, 0 })
vim.cmd("startinsert!")
"#;

/// Insert a literal newline in insert mode, bypassing completion-plugin maps
/// (cmp maps `<C-j>` to "select next item").
const NEWLINE_LUA: &str = r#"
local keys = vim.api.nvim_replace_termcodes("<CR>", true, false, true)
vim.api.nvim_feedkeys(keys, "n", false)
"#;

/// Replace the prompt buffer with `text` (used to restore a failed submit).
const SET_TEXT_LUA: &str = r#"
local text = ...
local lines = vim.split(text, "\n", { plain = true })
vim.api.nvim_buf_set_lines(0, 0, -1, false, lines)
vim.api.nvim_win_set_cursor(0, { #lines, #lines[#lines] })
vim.cmd("startinsert!")
"#;

/// Sample a few highlight groups and notify the host; re-run on `ColorScheme`.
/// `nvim_get_hl` is 0.9+; older Neovim yields an empty theme via `pcall`.
const THEME_LUA: &str = r#"
local groups = {
  normal = "Normal",
  comment = "Comment",
  string = "String",
  ["function"] = "Function",
  error = "ErrorMsg",
  diff_add = "DiffAdd",
  diff_delete = "DiffDelete",
}
local function eix_theme()
  local out = {}
  for key, group in pairs(groups) do
    local ok, hl = pcall(vim.api.nvim_get_hl, 0, { name = group, link = false })
    if ok and hl then
      out[key] = { fg = hl.fg, bg = hl.bg, reverse = hl.reverse }
    end
  end
  return out
end
if vim.g.eix_host_chan then
  vim.api.nvim_create_autocmd("ColorScheme", {
    callback = function()
      pcall(vim.rpcnotify, vim.g.eix_host_chan, "eix_theme", eix_theme())
    end,
  })
end
return eix_theme()
"#;

/// Commands for the worker that owns the RPC connection.
enum Cmd {
    Input(String),
    Paste(String),
    Resize(u16, u16),
    Newline,
    StartInsert,
    Reset,
    SetText(String),
    Text(tokio::sync::oneshot::Sender<String>),
}

/// An embedded Neovim used as the TUI prompt.
pub struct NvimInput {
    shared: Arc<Mutex<Shared>>,
    tx: UnboundedSender<Cmd>,
    worker: JoinHandle<()>,
    io: JoinHandle<Result<(), Box<LoopError>>>,
    child: Child,
    width: u16,
    height: u16,
}

impl NvimInput {
    /// Spawn `nvim --embed`, attach `ext_linegrid` + `ext_multigrid`, and
    /// install the prompt profile. `minimal` starts Neovim with `-u NONE`.
    ///
    /// Multi-grid mode gives every window its own grid, so floating windows
    /// (completion menus, notifications, ...) are composed over the prompt
    /// instead of being clipped by its boundary.
    ///
    /// With a full config, startup is bounded by five seconds; a hang or an
    /// early exit falls back to `-u NONE` so the user still gets a prompt. The
    /// caller can detect the fallback with [`Self::used_minimal_fallback`].
    pub async fn spawn(width: u16, height: u16, minimal: bool) -> Result<Self> {
        if minimal {
            return Self::spawn_once(width, height, true).await;
        }
        match tokio::time::timeout(
            Duration::from_secs(5),
            Self::spawn_once(width, height, false),
        )
        .await
        {
            Ok(Ok(input)) => Ok(input),
            Ok(Err(err)) => {
                let mut fallback = Self::spawn_once(width, height, true)
                    .await
                    .with_context(|| format!("full nvim config failed: {err:#}"))?;
                fallback.mark_fallback();
                Ok(fallback)
            }
            Err(_) => {
                let mut fallback = Self::spawn_once(width, height, true)
                    .await
                    .context("embedded nvim did not start within 5s")?;
                fallback.mark_fallback();
                Ok(fallback)
            }
        }
    }

    async fn spawn_once(width: u16, height: u16, minimal: bool) -> Result<Self> {
        let shared = Arc::new(Mutex::new(Shared::new(width, height)));
        let handler = NvimHandler {
            shared: Arc::clone(&shared),
        };

        let mut cmd = Command::new("nvim");
        cmd.arg("--embed");
        // Let user configs detect the prompt profile and opt out of animations
        // that repaint the prompt on a timer (cursor smear, ...).
        cmd.arg("--cmd").arg("let g:eix_prompt = 1");
        // An embedded child has no real tty; give it a sane TERM.
        cmd.env("TERM", "xterm-256color");
        // A startup timeout drops the future; make sure the child dies with it.
        cmd.kill_on_drop(true);
        if minimal {
            cmd.arg("-u").arg("NONE");
        }

        let (nvim, io, child) = create_nvim::new_child_cmd(&mut cmd, handler)
            .await
            .context("failed to spawn `nvim --embed` (is nvim on PATH?)")?;

        let channel = channel_id(&nvim).await?;
        nvim.set_var("eix_host_chan", Value::from(channel))
            .await
            .map_err(|e| anyhow!("nvim_set_var failed: {e}"))?;

        let mut opts = UiAttachOptions::new();
        opts.set_rgb(true);
        opts.set_linegrid_external(true);
        opts.set_multigrid_external(true);
        nvim.ui_attach(width as i64, height as i64, &opts)
            .await
            .map_err(|e| anyhow!("nvim_ui_attach failed: {e}"))?;

        // Truecolor must be on *after* the user's config so colorschemes feed
        // us RGB.
        run_lua(&nvim, "vim.o.termguicolors = true").await?;
        run_lua(&nvim, prompt::PROFILE_LUA).await?;
        // Sample the colorscheme once; `ColorScheme` re-runs it via rpcnotify.
        if let Ok(value) = run_lua(&nvim, THEME_LUA).await {
            if let Ok(mut shared) = shared.lock() {
                shared.theme = parse_theme(&value);
            }
        }
        // Pin the prompt window so splits (from the user's config) don't make
        // the renderer pick the wrong grid as the base layer.
        if let Ok(value) = run_lua(&nvim, "return vim.api.nvim_get_current_win()").await {
            if let (Some(win), Ok(mut shared)) = (grid::handle_value(&value), shared.lock()) {
                shared.screen.set_prompt_win(win);
            }
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let worker = tokio::spawn(worker(rx, nvim, Arc::clone(&shared)));

        Ok(Self {
            shared,
            tx,
            worker,
            io,
            child,
            width,
            height,
        })
    }

    /// Forward a key to Neovim, in `nvim_input` notation.
    pub fn input(&self, key: KeyEvent) {
        if let Some(seq) = keys::encode(key) {
            self.raw_input(&seq);
        }
    }

    /// Forward an already-encoded `nvim_input` sequence.
    pub fn raw_input(&self, seq: &str) {
        let _ = self.tx.send(Cmd::Input(seq.to_string()));
    }

    pub fn paste(&self, text: &str) {
        let _ = self.tx.send(Cmd::Paste(text.to_string()));
    }

    /// Insert a newline without going through insert-mode mappings.
    pub fn newline(&self) {
        let _ = self.tx.send(Cmd::Newline);
    }

    /// Ensure Neovim is in insert mode (used when the prompt regains focus).
    pub fn start_insert(&self) {
        let _ = self.tx.send(Cmd::StartInsert);
    }

    /// Clear the prompt and return to insert mode.
    pub fn reset(&self) {
        let _ = self.tx.send(Cmd::Reset);
    }

    /// Replace the prompt contents (used to restore a failed submit).
    pub fn set_text(&self, text: &str) {
        let _ = self.tx.send(Cmd::SetText(text.to_string()));
    }

    /// Resize the prompt grid, skipping no-op calls.
    pub fn resize(&mut self, width: u16, height: u16) {
        if (width, height) == (self.width, self.height) {
            return;
        }
        self.width = width;
        self.height = height;
        let _ = self.tx.send(Cmd::Resize(width, height));
    }

    pub fn size(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// Take drafts submitted with `<CR>` since the last call.
    pub fn take_submitted(&self) -> Vec<String> {
        self.shared
            .lock()
            .map(|mut shared| std::mem::take(&mut shared.submitted))
            .unwrap_or_default()
    }

    /// Current Neovim mode (`insert`, `normal`, ...), for the prompt header.
    pub fn mode(&self) -> String {
        self.shared
            .lock()
            .map(|shared| shared.screen.mode.clone())
            .unwrap_or_default()
    }

    /// Read the prompt buffer through the RPC connection.
    pub async fn text(&self) -> String {
        let (tx, rx) = tokio::sync::oneshot::channel();
        if self.tx.send(Cmd::Text(tx)).is_err() {
            return String::new();
        }
        rx.await.unwrap_or_default()
    }

    /// Text of one rendered grid row (test/debug aid).
    pub fn line(&self, row: usize) -> String {
        self.shared
            .lock()
            .map(|shared| shared.screen.line(row))
            .unwrap_or_default()
    }

    /// Prompt buffer line count, used to size the input area.
    pub fn line_count(&self) -> u16 {
        self.shared.lock().map(|shared| shared.lines).unwrap_or(1)
    }

    /// True when the user's config was skipped after a failed/slow start.
    pub fn used_minimal_fallback(&self) -> bool {
        self.shared
            .lock()
            .map(|shared| shared.fallback)
            .unwrap_or(false)
    }

    /// Grid topology (debug aid for `EIX_DEBUG=1`).
    pub fn debug_state(&self) -> String {
        self.shared
            .lock()
            .map(|shared| shared.screen.debug_state())
            .unwrap_or_default()
    }

    /// Highlight colors last reported by Neovim (default when unknown).
    pub fn theme(&self) -> Theme {
        self.shared
            .lock()
            .map(|shared| shared.theme)
            .unwrap_or_default()
    }

    fn mark_fallback(&mut self) {
        if let Ok(mut shared) = self.shared.lock() {
            shared.fallback = true;
        }
    }

    /// Render the last received frame into `area`.
    pub fn draw(&self, frame: &mut ratatui::Frame, area: Rect, show_cursor: bool) {
        if let Ok(shared) = self.shared.lock() {
            shared.screen.draw(frame, area, show_cursor);
        }
    }
}

impl Drop for NvimInput {
    fn drop(&mut self) {
        self.io.abort();
        self.worker.abort();
        let _ = self.child.start_kill();
    }
}

async fn worker(mut rx: UnboundedReceiver<Cmd>, nvim: Nvim, shared: Arc<Mutex<Shared>>) {
    while let Some(cmd) = rx.recv().await {
        let ok = match cmd {
            Cmd::Input(seq) => nvim.input(&seq).await.is_ok(),
            Cmd::Paste(text) => nvim.paste(&text, false, -1).await.is_ok(),
            // The grid is resized by Neovim's own `grid_resize` event; doing it
            // here would race the redraw that follows and blank the prompt.
            Cmd::Resize(width, height) => nvim
                .ui_try_resize(width as i64, height as i64)
                .await
                .is_ok(),
            Cmd::Newline => run_lua(&nvim, NEWLINE_LUA).await.is_ok(),
            Cmd::StartInsert => run_lua(&nvim, "vim.cmd('startinsert!')").await.is_ok(),
            Cmd::Reset => run_lua(&nvim, RESET_LUA).await.is_ok(),
            Cmd::SetText(text) => nvim
                .exec_lua(SET_TEXT_LUA, vec![Value::from(text)])
                .await
                .is_ok(),
            Cmd::Text(tx) => {
                let text = nvim
                    .exec_lua(
                        "return table.concat(vim.api.nvim_buf_get_lines(0, 0, -1, false), \"\\n\")",
                        Vec::new(),
                    )
                    .await
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_string))
                    .unwrap_or_default();
                let _ = tx.send(text);
                true
            }
        };
        if !ok {
            break;
        }
        refresh_lines(&nvim, &shared).await;
    }
}

async fn refresh_lines(nvim: &Nvim, shared: &Arc<Mutex<Shared>>) {
    let count = nvim
        .exec_lua("return vim.api.nvim_buf_line_count(0)", Vec::new())
        .await
        .ok()
        .and_then(|value| value.as_u64())
        .unwrap_or(1)
        .max(1) as u16;
    if let Ok(mut shared) = shared.lock() {
        shared.lines = count;
    }
}

async fn channel_id(nvim: &Nvim) -> Result<i64> {
    let info = nvim
        .get_api_info()
        .await
        .map_err(|e| anyhow!("nvim_get_api_info failed: {e}"))?;
    info.first()
        .and_then(|v| v.as_i64())
        .ok_or_else(|| anyhow!("nvim_get_api_info returned no channel id"))
}

async fn run_lua(nvim: &Nvim, code: &str) -> Result<Value> {
    nvim.exec_lua(code, Vec::new())
        .await
        .map_err(|e| anyhow!("nvim_exec_lua failed: {e}"))
}

/// Convert the Lua table from [`THEME_LUA`] into a [`Theme`]. Unknown groups
/// and missing keys are ignored; `reverse` swaps the foreground/background.
fn parse_theme(value: &Value) -> Theme {
    let mut theme = Theme::default();
    let Some(map) = value.as_map() else {
        return theme;
    };
    for (key, entry) in map {
        let Some(name) = key.as_str() else {
            continue;
        };
        let Some(fields) = entry.as_map() else {
            continue;
        };
        let mut fg = None;
        let mut bg = None;
        let mut reverse = false;
        for (field, field_value) in fields {
            match field.as_str().unwrap_or("") {
                "fg" => fg = grid::color(field_value),
                "bg" => bg = grid::color(field_value),
                "reverse" => reverse = field_value.as_bool().unwrap_or(false),
                _ => {}
            }
        }
        let color = if reverse { bg.or(fg) } else { fg };
        let slot = match name {
            "normal" => &mut theme.normal,
            "comment" => &mut theme.comment,
            "string" => &mut theme.string,
            "function" => &mut theme.function,
            "error" => &mut theme.error,
            "diff_add" => &mut theme.diff_add,
            "diff_delete" => &mut theme.diff_delete,
            _ => continue,
        };
        *slot = color;
    }
    theme
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::Duration;

    #[test]
    fn parses_theme_groups() {
        fn group(pairs: &[(&str, i64)]) -> Value {
            Value::Map(
                pairs
                    .iter()
                    .map(|(key, value)| (Value::from(*key), Value::from(*value)))
                    .collect(),
            )
        }
        let value = Value::Map(vec![
            (Value::from("normal"), group(&[("fg", 0xff_8800)])),
            (Value::from("comment"), group(&[("fg", 0x00_00ff)])),
            (
                Value::from("string"),
                Value::Map(vec![
                    (Value::from("fg"), Value::from(0x11_1111_i64)),
                    (Value::from("bg"), Value::from(0x22_2222_i64)),
                    (Value::from("reverse"), Value::from(true)),
                ]),
            ),
            (Value::from("unknown"), group(&[("fg", 0x33_3333)])),
        ]);
        let theme = parse_theme(&value);
        assert_eq!(theme.normal, Some(Color::Rgb(0xff, 0x88, 0x00)));
        assert_eq!(theme.comment, Some(Color::Rgb(0x00, 0x00, 0xff)));
        // `reverse` swaps foreground and background; text picks the old bg.
        assert_eq!(theme.string, Some(Color::Rgb(0x22, 0x22, 0x22)));
        assert_eq!(theme.function, None);
        assert!(!theme.is_empty());
    }

    #[test]
    fn empty_theme_keeps_defaults() {
        let theme = parse_theme(&Value::Nil);
        assert!(theme.is_empty());
        assert_eq!(theme, Theme::default());
    }

    #[tokio::test]
    async fn reports_colorscheme_changes() {
        if !nvim_available() {
            return;
        }
        let nvim = NvimInput::spawn(40, 3, true).await.expect("spawn nvim");
        tokio::time::sleep(Duration::from_millis(500)).await;

        nvim.raw_input(
            "<Esc>:lua vim.api.nvim_set_hl(0, 'Comment', {fg = '#123456'}); \
             vim.cmd('doautocmd ColorScheme')<CR>",
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while nvim.theme().comment != Some(Color::Rgb(0x12, 0x34, 0x56)) {
            assert!(
                tokio::time::Instant::now() < deadline,
                "theme not refreshed after ColorScheme: {:?} text={:?}",
                nvim.theme(),
                nvim.text().await
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn types_and_submits_a_draft() {
        if !nvim_available() {
            return;
        }
        let nvim = NvimInput::spawn(40, 3, true).await.expect("spawn nvim");
        // Let the prompt profile install its mappings and enter insert mode.
        tokio::time::sleep(Duration::from_millis(500)).await;

        for ch in "hello 世界".chars() {
            nvim.input(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        nvim.raw_input("<CR>");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if nvim
                .take_submitted()
                .iter()
                .any(|draft| draft == "hello 世界")
            {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "no submit received");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn reports_buffer_line_count() {
        if !nvim_available() {
            return;
        }
        let nvim = NvimInput::spawn(40, 3, true).await.expect("spawn nvim");
        tokio::time::sleep(Duration::from_millis(500)).await;

        nvim.set_text("a\nb\nc\nd");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while nvim.line_count() != 4 {
            assert!(
                tokio::time::Instant::now() < deadline,
                "line count not updated"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn composes_floating_windows_over_the_prompt() {
        if !nvim_available() {
            return;
        }
        let nvim = NvimInput::spawn(40, 5, true).await.expect("spawn nvim");
        tokio::time::sleep(Duration::from_millis(500)).await;

        nvim.set_text("under\nsecond");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while nvim.line(0).trim_end() != "under" {
            assert!(
                tokio::time::Instant::now() < deadline,
                "prompt grid did not converge: {:?}",
                nvim.line(0)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        // Open a non-focusable float over the first row of the prompt.
        nvim.raw_input(
            "<Esc>:lua local b=vim.api.nvim_create_buf(false,true); \
             vim.api.nvim_buf_set_lines(b,0,-1,false,{'float!'}); \
             vim.api.nvim_open_win(b,false,{relative='editor',row=0,col=1,\
             width=7,height=1,style='minimal'})<CR>",
        );

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let first = nvim.line(0);
            let second = nvim.line(1);
            if first.contains("float!") && second.trim_end() == "second" {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "float was not composed: row0={first:?} row1={second:?}"
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn renders_the_command_line_overlay() {
        if !nvim_available() {
            return;
        }
        let nvim = NvimInput::spawn(40, 6, true).await.expect("spawn nvim");
        tokio::time::sleep(Duration::from_millis(500)).await;

        // `cmdheight=0` makes Neovim render the command line in the message
        // grid at the bottom of the UI, not in the prompt window grid.
        nvim.raw_input("<Esc>:");

        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let rows: Vec<String> = (0..6).map(|row| nvim.line(row)).collect();
            if rows.iter().any(|row| row.contains(':')) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "command line was not composed: rows={rows:?} state={}",
                nvim.debug_state()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn restores_wide_text_and_renders_it() {
        if !nvim_available() {
            return;
        }
        let nvim = NvimInput::spawn(40, 3, true).await.expect("spawn nvim");
        tokio::time::sleep(Duration::from_millis(500)).await;

        nvim.set_text("你好 nvim prompt");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if nvim.text().await == "你好 nvim prompt"
                && nvim.line(0).trim_end() == "你好 nvim prompt"
            {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "buffer/grid did not converge: buffer={:?} row={:?}",
                nvim.text().await,
                nvim.line(0)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}
