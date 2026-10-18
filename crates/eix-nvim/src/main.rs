//! `eix-nvim` spike: embed a real Neovim, render it with `ext_linegrid`, and
//! prove out the input-box contract described in `DESIGN.md`.
//!
//! Two modes:
//!
//! * `--probe`  no TTY needed: attach, wait for a frame, inject sample text
//!   (CJK / wide / special keys), print the resulting grid, quit.
//! * default    interactive: crossterm raw mode + alternate screen, forward
//!   keys to Neovim via `nvim_input`, draw the grid with ratatui, and keep the
//!   last terminal row for an `eix` status bar.

mod grid;
mod keys;
mod prompt;

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind, KeyModifiers,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use grid::Grid;
use nvim_rs::compat::tokio::Compat;
use nvim_rs::create::tokio as create_nvim;
use nvim_rs::{Handler, Neovim, UiAttachOptions, Value};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;
use tokio::process::{ChildStdin, Command};

type Nvim = Neovim<Compat<ChildStdin>>;

/// Everything the RPC handler writes and the render loop reads.
struct Shared {
    grid: Grid,
    /// Drafts submitted with `<CR>`, oldest first.
    submitted: Vec<String>,
    /// Frames observed by the host (mirrors `grid.frame`, kept for clarity).
    frames: u64,
}

impl Shared {
    fn new(width: u16, height: u16) -> Self {
        Self {
            grid: Grid::new(width, height),
            submitted: Vec::new(),
            frames: 0,
        }
    }
}

#[derive(Clone)]
struct NvimHandler {
    shared: Arc<Mutex<Shared>>,
}

#[async_trait::async_trait]
impl Handler for NvimHandler {
    type Writer = Compat<ChildStdin>;

    async fn handle_notify(&self, name: String, args: Vec<Value>, _nvim: Nvim) {
        match name.as_str() {
            "redraw" => {
                // Neovim packs the redraw events directly into the
                // notification params: `redraw(event, event, ...)`.
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

#[derive(Default)]
struct Args {
    minimal: bool,
    probe: bool,
    width: Option<u16>,
    height: Option<u16>,
    extra: Vec<String>,
}

impl Args {
    fn parse() -> Self {
        let mut args = Args::default();
        let mut it = std::env::args().skip(1);
        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--minimal" => args.minimal = true,
                "--probe" => args.probe = true,
                "--width" => args.width = it.next().and_then(|v| v.parse().ok()),
                "--height" => args.height = it.next().and_then(|v| v.parse().ok()),
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                "--" => {
                    args.extra.extend(it);
                    break;
                }
                other => args.extra.push(other.to_string()),
            }
        }
        args
    }
}

fn print_help() {
    println!(
        "\
eix-nvim — embedded Neovim + ext_linegrid spike

USAGE:
    eix-nvim [OPTIONS] [-- <nvim args>]

OPTIONS:
    --minimal         run Neovim with `-u NONE` (no user config)
    --probe           attach, print one rendered frame, exit (no TTY)
    --width  <cols>   override reported width
    --height <rows>   override reported height
    -h, --help        show this help"
    );
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    let (cols, rows) = resolve_size(&args)?;
    // In the TUI the last row is the host status bar; the probe has no bar.
    let nvim_height = if args.probe {
        rows.max(1)
    } else {
        rows.saturating_sub(1).max(1)
    };

    let shared = Arc::new(Mutex::new(Shared::new(cols, nvim_height)));
    let handler = NvimHandler {
        shared: Arc::clone(&shared),
    };

    let mut cmd = build_command(&args);
    let (nvim, io_handle, mut child) = create_nvim::new_child_cmd(&mut cmd, handler)
        .await
        .context("failed to spawn `nvim --embed` (is nvim on PATH?)")?;

    let channel = channel_id(&nvim).await?;
    nvim.set_var("eix_host_chan", Value::from(channel))
        .await
        .map_err(|e| anyhow!("nvim_set_var failed: {e}"))?;

    let mut opts = UiAttachOptions::new();
    opts.set_rgb(true);
    opts.set_linegrid_external(true);
    nvim.ui_attach(cols as i64, nvim_height as i64, &opts)
        .await
        .map_err(|e| anyhow!("nvim_ui_attach failed: {e}"))?;

    // Truecolor must be on *after* the user's config so colorschemes feed us RGB.
    run_lua(&nvim, "vim.o.termguicolors = true").await?;
    run_lua(&nvim, prompt::PROFILE_LUA).await?;

    if std::env::var_os("EIX_DEBUG").is_some() {
        let info = run_lua(
            &nvim,
            "return string.format(\
               'colors=%s config=%s laststatus=%d bt=%s filetype=%s mode=%s',\
               tostring(vim.g.colors_name), vim.fn.stdpath('config'),\
               vim.o.laststatus, vim.bo.buftype, vim.bo.filetype, vim.fn.mode())",
        )
        .await?;
        eprintln!("[eix] {}", info.as_str().unwrap_or("?"));
    }

    let result = if args.probe {
        run_probe(&nvim, &shared).await
    } else {
        run_tui(nvim.clone(), Arc::clone(&shared), cols, rows).await
    };

    let _ = nvim.ui_detach().await;
    let _ = nvim.quit_no_save().await;
    let _ = child.kill().await;
    let _ = child.wait().await;
    io_handle.abort();

    result
}

fn resolve_size(args: &Args) -> Result<(u16, u16)> {
    if args.probe {
        return Ok((args.width.unwrap_or(80), args.height.unwrap_or(24)));
    }
    let (cols, rows) = crossterm::terminal::size().context("failed to read terminal size")?;
    Ok((args.width.unwrap_or(cols), args.height.unwrap_or(rows)))
}

fn build_command(args: &Args) -> Command {
    let mut cmd = Command::new("nvim");
    cmd.arg("--embed");
    // Let user configs detect the prompt profile and opt out of things that
    // don't survive a single-grid render (cursor smear, animations, ...).
    cmd.arg("--cmd").arg("let g:eix_prompt = 1");
    // An embedded child has no real tty; give it a sane TERM so plugins that
    // inspect it don't bail out.
    cmd.env("TERM", "xterm-256color");
    if args.minimal {
        cmd.arg("-u").arg("NONE");
    }
    cmd.args(&args.extra);
    cmd
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

/// Block until the host has seen at least `target` flushed frames.
async fn wait_frames(shared: &Arc<Mutex<Shared>>, target: u64, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let frame = shared.lock().map(|s| s.frames).unwrap_or(0);
        if frame >= target {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!("timed out waiting for frame {target} (at {frame})"));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

const PROBE_SAMPLE_LUA: &str = r#"
local b = vim.api.nvim_get_current_buf()
vim.api.nvim_buf_set_lines(b, 0, -1, false, {
  "eix spike: hello 世界 🌍 — ✓",
  "keys: <CR> <Tab> <C-g>  <<<",
  "wide: ありがとう 汉字 ＡＢ fullwidth",
  "α β γ λ μ :: termguicolors",
})
vim.api.nvim_win_set_cursor(0, { 3, 4 })
vim.cmd("redraw")
"#;

const FLOATS_LUA: &str = r#"
local out = {}
for _, w in ipairs(vim.api.nvim_list_wins()) do
  local cfg = vim.api.nvim_win_get_config(w)
  if cfg.relative ~= "" then
    local b = vim.api.nvim_win_get_buf(w)
    out[#out+1] = string.format(
      "win=%d rel=%s anchor=%s zindex=%s pos=(%s,%s) size=%dx%d ft=%s bt=%s loaded=%s",
      w, cfg.relative, tostring(cfg.anchor), tostring(cfg.zindex),
      tostring(cfg.row), tostring(cfg.col), cfg.width, cfg.height,
      vim.bo[b].filetype, vim.bo[b].buftype, tostring(vim.b[b].loaded))
  end
end
if #out == 0 then return "none" end
return table.concat(out, "\n")
"#;

async fn run_probe(nvim: &Nvim, shared: &Arc<Mutex<Shared>>) -> Result<()> {
    wait_frames(shared, 1, Duration::from_secs(10))
        .await
        .context("no redraw received: linegrid attach is not delivering events")?;

    run_lua(nvim, PROBE_SAMPLE_LUA).await?;
    let base = shared.lock().map(|s| s.frames).unwrap_or(0);
    wait_frames(shared, base + 2, Duration::from_secs(5)).await?;
    // Let plugin animations (e.g. smear-cursor floats) finish, then force a
    // full repaint so every cell reflects the settled state.
    tokio::time::sleep(Duration::from_millis(400)).await;
    run_lua(nvim, "vim.cmd('redraw!')").await?;
    let settled = shared.lock().map(|s| s.frames).unwrap_or(0);
    let _ = wait_frames(shared, settled + 1, Duration::from_secs(2)).await;

    if std::env::var_os("EIX_DEBUG").is_some() {
        let floats = run_lua(nvim, FLOATS_LUA).await?;
        println!("floating windows:\n{}", floats.as_str().unwrap_or("<none>"));
    }

    let (width, height, mode, title, frame, cursor, lines) = {
        let s = shared.lock().map_err(|_| anyhow!("shared lock poisoned"))?;
        let lines: Vec<String> = (0..s.grid.height)
            .map(|row| s.grid.line(row as usize).trim_end().to_string())
            .collect();
        (
            s.grid.width,
            s.grid.height,
            s.grid.mode.clone(),
            s.grid.title.clone(),
            s.grid.frame,
            (s.grid.cursor_row, s.grid.cursor_col),
            lines,
        )
    };

    println!("eix-nvim probe");
    println!("  grid     : {width}x{height}  frames={frame}",);
    println!("  mode     : {mode}");
    println!("  cursor   : row={} col={}", cursor.0, cursor.1);
    if !title.is_empty() {
        println!("  title    : {title}");
    }
    println!("  ---");
    for (i, line) in lines.iter().enumerate() {
        println!("  {i:>2}|{line}");
    }
    // Exercise the real submit path: type via `nvim_input` (exactly what the TUI
    // does), then let the buffer-local mapping notify us.
    let before = shared.lock().map(|s| s.submitted.len()).unwrap_or(0);
    run_lua(
        nvim,
        "local b = vim.api.nvim_get_current_buf()\n\
         vim.api.nvim_buf_set_lines(b, 0, -1, false, { '' })\n\
         vim.api.nvim_win_set_cursor(0, { 1, 0 })\n\
         vim.cmd('startinsert!')",
    )
    .await?;
    nvim.input("probe round-trip ✓")
        .await
        .map_err(|e| anyhow!("nvim_input failed: {e}"))?;
    nvim.input("<CR>")
        .await
        .map_err(|e| anyhow!("nvim_input failed: {e}"))?;

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    let submitted = loop {
        let latest = shared.lock().ok().and_then(|s| {
            if s.submitted.len() > before {
                s.submitted.last().cloned()
            } else {
                None
            }
        });
        if let Some(text) = latest {
            break Some(text);
        }
        if tokio::time::Instant::now() >= deadline {
            break None;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };

    println!("  ---");
    match &submitted {
        Some(text) => println!("  submit   : {text:?}  (round-trip ok)"),
        None => println!("  submit   : <timed out: <CR> did not reach the host>"),
    }

    Ok(())
}

async fn run_tui(nvim: Nvim, shared: Arc<Mutex<Shared>>, cols: u16, rows: u16) -> Result<()> {
    let mut stdout = std::io::stdout();
    enable_raw_mode().context("failed to enable raw mode")?;
    execute!(stdout, EnterAlternateScreen, EnableBracketedPaste)?;

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;

    // Index into `Shared::submitted` of the next draft to echo + consume.
    let mut consumed = 0usize;

    let outcome = 'outer: loop {
        // Consume freshly submitted drafts: clear the input buffer and drop
        // back into insert mode so the user can keep typing.
        let fresh: Vec<String> = {
            match shared.lock() {
                Ok(s) => s.submitted[consumed..].to_vec(),
                Err(_) => Vec::new(),
            }
        };
        if !fresh.is_empty() {
            consumed += fresh.len();
            for _ in &fresh {
                if run_lua(
                    &nvim,
                    "local b = vim.api.nvim_get_current_buf()\n\
                     vim.api.nvim_buf_set_lines(b, 0, -1, false, { '' })\n\
                     vim.api.nvim_win_set_cursor(0, { 1, 0 })\n\
                     vim.cmd('startinsert!')",
                )
                .await
                .is_err()
                {
                    break 'outer Ok(());
                }
            }
        }

        if let Err(err) = terminal.draw(|frame| draw(frame, &shared)) {
            break Err(anyhow!("draw failed: {err}"));
        }

        if !event::poll(Duration::from_millis(16)).context("poll failed")? {
            continue;
        }

        match event::read().context("read failed")? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if std::env::var_os("EIX_DEBUG").is_some() {
                    eprintln!("key: {:?} {:?} -> {:?}", key.code, key.modifiers, keys::encode(key));
                }
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c'))
                {
                    break Ok(());
                }
                if let Some(seq) = keys::encode(key) {
                    if nvim.input(&seq).await.is_err() {
                        break Ok(());
                    }
                }
            }
            Event::Paste(data) => {
                let _ = nvim.paste(&data, false, -1).await;
            }
            Event::Resize(w, h) => {
                let nh = h.saturating_sub(1).max(1);
                if nvim.ui_try_resize(w as i64, nh as i64).await.is_err() {
                    break Ok(());
                }
                if let Ok(mut s) = shared.lock() {
                    s.grid.resize(w, nh);
                }
            }
            _ => {}
        }
    };

    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableBracketedPaste
    )
    .ok();
    terminal.show_cursor().ok();

    // Drafts are echoed here rather than into the alternate screen.
    let unsent = run_lua(
        &nvim,
        "local b = vim.api.nvim_get_current_buf()\n\
         return table.concat(vim.api.nvim_buf_get_lines(b, 0, -1, false), '\\n')",
    )
    .await
    .ok()
    .and_then(|v| v.as_str().map(|s| s.to_string()))
    .unwrap_or_default();

    if let Ok(s) = shared.lock() {
        if !s.submitted.is_empty() {
            println!("--- submitted drafts ({}) ---", s.submitted.len());
            for (i, draft) in s.submitted.iter().enumerate() {
                println!("[{}] {draft}", i + 1);
            }
        }
    }
    if !unsent.trim().is_empty() {
        println!("--- unsent draft (not submitted) ---");
        println!("{unsent}");
    }
    let _ = std::io::stdout().flush();

    let _ = (cols, rows);
    outcome
}

fn draw(frame: &mut ratatui::Frame, shared: &Arc<Mutex<Shared>>) {
    let area = frame.area();
    if area.height == 0 {
        return;
    }
    let grid_area = Rect {
        width: area.width,
        height: area.height.saturating_sub(1),
        ..area
    };
    let bar_area = Rect {
        y: area.y + area.height.saturating_sub(1),
        height: 1,
        ..area
    };

    let s = match shared.lock() {
        Ok(s) => s,
        Err(_) => return,
    };

    grid::draw_in(frame, &s.grid, grid_area);

    let mode = if s.grid.mode.is_empty() {
        "--"
    } else {
        s.grid.mode.as_str()
    };
    let last = s
        .submitted
        .last()
        .map(|d| d.lines().next().unwrap_or(""))
        .unwrap_or("");
    let preview: String = last.chars().take(60).collect();
    let bar = format!(
        " eix-nvim  [{mode}]  drafts:{}  {preview}",
        s.submitted.len()
    );

    frame.render_widget(
        Paragraph::new(bar).style(Style::default().fg(Color::Black).bg(Color::Gray)),
        bar_area,
    );
    frame.set_cursor_position((
        bar_area.x + s.grid.cursor_col.min(grid_area.width.saturating_sub(1)),
        s.grid.cursor_row.min(grid_area.height.saturating_sub(1)),
    ));
}
