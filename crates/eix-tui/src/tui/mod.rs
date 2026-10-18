//! Full-screen terminal frontend (`eix tui`).
//!
//! The event loop is deliberately thin: terminal input, daemon events and a
//! slow tick all funnel into [`App`], which owns every piece of UI state.

mod app;
mod controller;
mod net;

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::Once;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{Event as TermEvent, EventStream};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use eix_core::{Endpoint, InputBackend, InputConfig};
use eix_nvim::embedded::NvimInput;
use futures_util::StreamExt;
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use app::App;
use controller::Command;

type Backend = CrosstermBackend<io::Stdout>;

pub async fn run(
    endpoint: Endpoint,
    data_dir: PathBuf,
    config: Option<PathBuf>,
    initial_session: Option<String>,
    bell: bool,
    input: InputConfig,
) -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("`eix tui` needs a terminal (stdout is not a tty)");
    }

    // Spawn the prompt before entering the alternate screen so a required
    // backend can fail without leaving the terminal in a broken state.
    let (cols, _rows) = crossterm::terminal::size().unwrap_or((80, 24));
    let mut fallback_note = None;
    let nvim = match input.backend {
        InputBackend::Native => None,
        InputBackend::Nvim | InputBackend::Auto => {
            match NvimInput::spawn(cols, 3, input.minimal).await {
                Ok(nvim) => Some(nvim),
                Err(err) if input.backend == InputBackend::Nvim => {
                    return Err(err).context("embedded nvim input");
                }
                Err(err) => {
                    fallback_note = Some(format!("embedded nvim unavailable: {err:#}"));
                    None
                }
            }
        }
    };

    let mut terminal = init_terminal()?;
    let mut app = App::new(initial_session);
    app.bell = bell;
    if let Some(nvim) = nvim {
        app.set_nvim_input(nvim);
    }
    if let Some(note) = fallback_note {
        app.set_status(format!("{note}; using the native prompt"));
    }
    let (cmd_tx, mut ev_rx) = controller::spawn(endpoint, data_dir, config);
    let _ = cmd_tx.send(Command::Hello);
    let _ = cmd_tx.send(Command::List);

    let mut events = EventStream::new();
    let tick = if app.has_nvim_input() {
        Duration::from_millis(30)
    } else {
        Duration::from_millis(200)
    };
    let mut outcome = Ok(());
    loop {
        app.poll_nvim(&cmd_tx);
        if let Err(err) = terminal.draw(|frame| app.draw(frame)) {
            outcome = Err(err).context("cannot draw the tui");
            break;
        }
        if app.should_quit {
            break;
        }

        tokio::select! {
            maybe = events.next() => match maybe {
                Some(Ok(TermEvent::Key(key))) => app.handle_key(key, &cmd_tx),
                Some(Ok(TermEvent::Paste(text))) => app.handle_paste(&text),
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
                    if app.take_ring() {
                        use std::io::Write;
                        let mut stdout = io::stdout();
                        let _ = stdout.write_all(b"\x07");
                        let _ = stdout.flush();
                    }
                }
                None => break,
            },
            _ = tokio::time::sleep(tick) => app.tick(),
        }
    }

    restore_terminal(&mut terminal)?;
    if let Some(message) = app.exit_message.take() {
        eprintln!("eix: {message}");
    }
    outcome
}

fn init_terminal() -> Result<Terminal<Backend>> {
    install_panic_hook();
    enable_raw_mode().context("cannot enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide).context("cannot enter alternate screen")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear().ok();
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<Backend>) -> Result<()> {
    disable_raw_mode().ok();
    execute!(terminal.backend_mut(), LeaveAlternateScreen, Show).ok();
    terminal.show_cursor().ok();
    Ok(())
}

fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = disable_raw_mode();
            let _ = execute!(io::stdout(), LeaveAlternateScreen, Show);
            previous(info);
        }));
    });
}
