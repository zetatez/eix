//! Host-side wrapper around an embedded `nvim --embed` prompt.
//!
//! The TUI is synchronous, so keys and resizes are queued to a worker task that
//! owns the RPC connection. The `redraw` handler fills a [`Grid`] and pushes
//! submitted drafts, both behind a mutex the UI can read without awaiting.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use crossterm::event::KeyEvent;
use nvim_rs::compat::tokio::Compat;
use nvim_rs::create::tokio as create_nvim;
use nvim_rs::error::LoopError;
use nvim_rs::{Handler, Neovim, UiAttachOptions, Value};
use ratatui::layout::Rect;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use crate::grid::{self, Grid};
use crate::keys;
use crate::prompt;

pub type Nvim = Neovim<Compat<ChildStdin>>;

/// State written by the RPC handler and read by the UI.
pub struct Shared {
    pub grid: Grid,
    /// Drafts submitted with `<CR>`, oldest first.
    pub submitted: Vec<String>,
    /// Frames observed by the host (mirrors `grid.frame`).
    pub frames: u64,
    /// Number of lines in the prompt buffer, refreshed after every command.
    pub lines: u16,
}

impl Shared {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            grid: Grid::new(width, height),
            submitted: Vec::new(),
            frames: 0,
            lines: 1,
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
                    shared.grid.apply(&args);
                    shared.frames = shared.grid.frame;
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
    /// Spawn `nvim --embed`, attach `ext_linegrid`, and install the prompt
    /// profile. `minimal` starts Neovim with `-u NONE`.
    pub async fn spawn(width: u16, height: u16, minimal: bool) -> Result<Self> {
        let shared = Arc::new(Mutex::new(Shared::new(width, height)));
        let handler = NvimHandler {
            shared: Arc::clone(&shared),
        };

        let mut cmd = Command::new("nvim");
        cmd.arg("--embed");
        // Let user configs detect the prompt profile and opt out of animations
        // that don't survive a single-grid render (cursor smear, ...).
        cmd.arg("--cmd").arg("let g:eix_prompt = 1");
        // An embedded child has no real tty; give it a sane TERM.
        cmd.env("TERM", "xterm-256color");
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
        nvim.ui_attach(width as i64, height as i64, &opts)
            .await
            .map_err(|e| anyhow!("nvim_ui_attach failed: {e}"))?;

        // Truecolor must be on *after* the user's config so colorschemes feed
        // us RGB.
        run_lua(&nvim, "vim.o.termguicolors = true").await?;
        run_lua(&nvim, prompt::PROFILE_LUA).await?;

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
            .map(|shared| shared.grid.mode.clone())
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
            .map(|shared| shared.grid.line(row))
            .unwrap_or_default()
    }

    /// Prompt buffer line count, used to size the input area.
    pub fn line_count(&self) -> u16 {
        self.shared.lock().map(|shared| shared.lines).unwrap_or(1)
    }

    /// Render the last received frame into `area`.
    pub fn draw(&self, frame: &mut ratatui::Frame, area: Rect, show_cursor: bool) {
        if let Ok(shared) = self.shared.lock() {
            grid::draw_in_with_cursor(frame, &shared.grid, area, show_cursor);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::time::Duration;

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
