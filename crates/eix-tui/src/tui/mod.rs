//! Full-screen terminal frontend (`eix tui`).
//!
//! The event loop is deliberately thin: terminal input, daemon events and a
//! slow tick all funnel into [`App`], which owns every piece of UI state.

mod app;
mod controller;
mod diff;
mod editor;
mod keys;
mod markdown;
mod math;
mod math_image;
pub(crate) mod net;
mod syntax;
mod theme;
mod transcript;
mod workspace;

use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Once;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event as TermEvent, EventStream,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use eix_core::Endpoint;
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::App;
use controller::Command;
use keys::Keybinds;
use theme::Theme;

type Backend = CrosstermBackend<io::Stdout>;

pub async fn run(
    endpoint: Endpoint,
    data_dir: PathBuf,
    config: Option<PathBuf>,
    initial_session: Option<String>,
    bell: bool,
) -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("`eix tui` needs a terminal (stdout is not a tty)");
    }

    let mut terminal = init_terminal()?;
    // Probe for Kitty/Sixel/iTerm2 before the event stream starts reading
    // stdin. Without a graphics protocol display math stays on the
    // character grid.
    math_image::init();
    let mut app = App::new(initial_session);
    app.bell = bell;
    let loaded = eix_core::Config::load(config.as_deref()).unwrap_or_default();
    app.set_compat(loaded.compations.clone());
    app.theme = Theme::from_config(&loaded.theme);
    app.theme_config = loaded.theme.clone();
    app.keybinds = Keybinds::from_config(&loaded.keys);
    let cli_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let saved_workspace = workspace::load(&data_dir, &cli_cwd);
    app.restore_workspace(saved_workspace);
    if let Ok(Some(defaults)) = eix_core::load_cli_config(&cli_cwd) {
        app.default_model = defaults.model;
        app.default_agent = defaults.agent;
        app.default_mode = defaults.mode;
    }
    let (cmd_tx, mut ev_rx, controller) = controller::spawn(endpoint, data_dir.clone(), config);
    let _ = cmd_tx.send(Command::Hello);
    let _ = cmd_tx.send(Command::List);
    let _ = cmd_tx.send(Command::ListPluginCommands);

    let mut events = EventStream::new();
    let tick = Duration::from_millis(200);
    let mut outcome = Ok(());
    loop {
        if app.take_force_clear() {
            terminal.clear().ok();
        }
        if let Err(err) = terminal.draw(|frame| app.draw(frame)) {
            outcome = Err(err).context("cannot draw the tui");
            break;
        }
        if let Some((session_id, before_seq)) = app.take_history_request() {
            let _ = cmd_tx.send(Command::LoadHistory {
                session_id,
                before_seq,
            });
        }
        if let Some((kind, session_id, value)) = app.take_picker_preview_request() {
            let _ = cmd_tx.send(Command::PickerPreview {
                kind,
                session_id,
                value,
            });
        }
        if app.should_quit {
            // A pane opened for a brand-new session should not leave an empty
            // row in history when the user quits without sending anything.
            app.cleanup_empty_sessions(&cmd_tx);
            break;
        }

        tokio::select! {
            maybe = events.next() => match maybe {
                Some(Ok(TermEvent::Key(key))) => app.handle_key(key, &cmd_tx),
                Some(Ok(TermEvent::Paste(text))) => app.handle_paste(&text),
                Some(Ok(TermEvent::Mouse(mouse))) => {
                    app.handle_mouse(mouse, &cmd_tx);
                    if let Some(text) = app.take_clipboard() {
                        copy_to_clipboard(&text);
                    }
                }
                Some(Ok(_)) => {}
                Some(Err(err)) => {
                    outcome = Err(err).context("terminal input error");
                    break;
                }
                None => break,
            },
            maybe = ev_rx.recv() => match maybe {
                Some(event) => {
                    app.handle_event(event, &cmd_tx);
                    // A streaming turn produces a notification per delta.
                    // Draining the queue renders one frame for the whole burst
                    // instead of one frame per token.
                    let mut drained = 0;
                    while drained < 512 {
                        match ev_rx.try_recv() {
                            Ok(event) => {
                                app.handle_event(event, &cmd_tx);
                                drained += 1;
                            }
                            Err(_) => break,
                        }
                    }
                    // Attention alerts render as top-right toasts; no desktop
                    // notification popups or terminal-title changes.
                    if app.take_ring() {
                        let mut stdout = io::stdout();
                        let _ = stdout.write_all(b"\x07");
                        let _ = stdout.flush();
                    }
                }
                None => break,
            },
            _ = tokio::time::sleep(tick) => app.tick(&cmd_tx),
        }
    }

    let active = app.workspace_state();
    let workspace_result: Result<()> = if app.should_clear_active_workspace() {
        workspace::clear(&data_dir, &cli_cwd)
    } else if active.sessions.is_empty() {
        // An interrupted startup that never attached a pane must not erase the
        // state saved by the previous run.
        Ok(())
    } else {
        workspace::save(&data_dir, &cli_cwd, &active)
    };

    // Give queued commands (in particular the final draft save) a chance to
    // reach the daemon before the runtime is torn down.
    drop(cmd_tx);
    let _ = tokio::time::timeout(Duration::from_millis(500), controller).await;

    restore_terminal(&mut terminal)?;
    if let Err(err) = workspace_result {
        eprintln!("eix: cannot save active sessions: {err:#}");
    }
    if let Some(message) = app.exit_message.take() {
        eprintln!("eix: {message}");
    }
    outcome
}

/// Copy selected transcript text with the desktop's native helper, falling
/// back to OSC 52 (which also works over SSH when the terminal supports it).
fn copy_to_clipboard(text: &str) {
    let mut copied = false;
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        copied |= pipe_to_clipboard("wl-copy", &[], text);
    }
    if !copied && std::env::var_os("DISPLAY").is_some() {
        copied |= pipe_to_clipboard("xclip", &["-selection", "clipboard"], text)
            || pipe_to_clipboard("xsel", &["--clipboard", "--input"], text);
    }
    if !copied && cfg!(target_os = "macos") {
        copied |= pipe_to_clipboard("pbcopy", &[], text);
    }
    if !copied && cfg!(target_os = "windows") {
        copied |= pipe_to_clipboard("clip", &[], text);
    }
    if !copied {
        let encoded = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
        let mut stdout = io::stdout();
        let _ = write!(stdout, "\x1b]52;c;{encoded}\x07");
        let _ = stdout.flush();
    }
}

/// Read the desktop clipboard for normal-mode `p` when the editor's own
/// register is empty. OSC 52 is write-only in practice, so this mirrors the
/// native helpers used by [`copy_to_clipboard`].
fn read_clipboard() -> Option<String> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        if let Some(text) = read_clipboard_command("wl-paste", &["--no-newline"]) {
            return Some(text);
        }
    }
    if std::env::var_os("DISPLAY").is_some() {
        if let Some(text) = read_clipboard_command("xclip", &["-selection", "clipboard", "-o"]) {
            return Some(text);
        }
        if let Some(text) = read_clipboard_command("xsel", &["--clipboard", "--output"]) {
            return Some(text);
        }
    }
    if cfg!(target_os = "macos") {
        return read_clipboard_command("pbpaste", &[]);
    }
    if cfg!(target_os = "windows") {
        return read_clipboard_command("powershell", &["-NoProfile", "-Command", "Get-Clipboard"]);
    }
    None
}

fn read_clipboard_command(program: &str, args: &[&str]) -> Option<String> {
    let output = ProcessCommand::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    (!text.is_empty()).then_some(text)
}

fn pipe_to_clipboard(program: &str, args: &[&str], text: &str) -> bool {
    let mut child = match ProcessCommand::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    if let Some(mut stdin) = child.stdin.take() {
        if stdin.write_all(text.as_bytes()).is_err() {
            let _ = child.kill();
            return false;
        }
    }
    matches!(child.wait(), Ok(status) if status.success())
}

fn init_terminal() -> Result<Terminal<Backend>> {
    install_panic_hook();
    enable_raw_mode().context("cannot enable raw mode")?;
    let mut stdout = io::stdout();
    // Ask the terminal to disambiguate modified keys (CSI-u). Without this,
    // terminals encode Ctrl-2/Ctrl-3/... as NUL/Esc/control characters, so
    // Ctrl-1..9 cannot be told apart from existing shortcuts.
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        Hide,
        // The alternate screen has its own keyboard-mode stack, so push only
        // after switching screens.
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
    .context("cannot enter alternate screen")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear().ok();
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<Backend>) -> Result<()> {
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        DisableMouseCapture,
        Show
    )
    .ok();
    terminal.show_cursor().ok();
    Ok(())
}

fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(
                io::stdout(),
                PopKeyboardEnhancementFlags,
                LeaveAlternateScreen,
                DisableMouseCapture,
                Show
            );
            previous(info);
        }));
    });
}
