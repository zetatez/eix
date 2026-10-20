//! Full-screen terminal frontend (`eix tui`).
//!
//! The event loop is deliberately thin: terminal input, daemon events and a
//! slow tick all funnel into [`App`], which owns every piece of UI state.

mod app;
mod controller;
mod editor;
mod keys;
mod net;
mod theme;
mod transcript;

use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::Once;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use crossterm::cursor::{Hide, Show};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event as TermEvent, EventStream};
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
    osc: bool,
) -> Result<()> {
    if !io::stdout().is_terminal() {
        bail!("`eix tui` needs a terminal (stdout is not a tty)");
    }

    let mut terminal = init_terminal()?;
    let mut app = App::new(initial_session);
    app.bell = bell;
    app.osc = osc;
    let loaded = eix_core::Config::load(config.as_deref()).unwrap_or_default();
    app.theme = Theme::from_config(&loaded.theme);
    app.keybinds = Keybinds::from_config(&loaded.keys);
    let cli_cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    if let Ok(Some(defaults)) = eix_core::load_cli_config(&cli_cwd) {
        app.default_model = defaults.model;
        app.default_agent = defaults.agent;
        app.default_permission = defaults.permission;
    }
    let (cmd_tx, mut ev_rx, controller) = controller::spawn(endpoint, data_dir, config);
    let _ = cmd_tx.send(Command::Hello);
    let _ = cmd_tx.send(Command::List);

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
        if app.should_quit {
            break;
        }

        tokio::select! {
            maybe = events.next() => match maybe {
                Some(Ok(TermEvent::Key(key))) => app.handle_key(key, &cmd_tx),
                Some(Ok(TermEvent::Paste(text))) => app.handle_paste(&text),
                Some(Ok(TermEvent::Mouse(mouse))) => app.handle_mouse(mouse),
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
                        let message = app
                            .alert_message()
                            .unwrap_or_else(|| "attention needed".to_string());
                        let mut stdout = io::stdout();
                        if app.bell {
                            let _ = stdout.write_all(b"\x07");
                        }
                        if app.osc {
                            // OSC 9 (iTerm/kitty), OSC 777 (rxvt-unicode) and
                            // a title update; unsupported terminals ignore them.
                            let _ = write!(stdout, "\x1b]9;eix: {message}\x07");
                            let _ = write!(stdout, "\x1b]777;notify;eix;{message}\x07");
                            let _ = write!(stdout, "\x1b]2;eix · {message}\x07");
                        }
                        let _ = stdout.flush();
                    }
                }
                None => break,
            },
            _ = tokio::time::sleep(tick) => app.tick(&cmd_tx),
        }
    }

    // Give queued commands (in particular the final draft save) a chance to
    // reach the daemon before the runtime is torn down.
    drop(cmd_tx);
    let _ = tokio::time::timeout(Duration::from_millis(500), controller).await;

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
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture, Hide)
        .context("cannot enter alternate screen")?;
    let mut terminal = Terminal::new(CrosstermBackend::new(stdout))?;
    terminal.clear().ok();
    Ok(terminal)
}

fn restore_terminal(terminal: &mut Terminal<Backend>) -> Result<()> {
    disable_raw_mode().ok();
    execute!(
        terminal.backend_mut(),
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
                LeaveAlternateScreen,
                DisableMouseCapture,
                Show
            );
            previous(info);
        }));
    });
}
