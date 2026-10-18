//! `eix`: CLI frontend and the first user-facing slice of the eix agent.
//!
//! It talks to `eixd` over the Unix socket and starts the daemon on demand.
//! Phase 0 commands are intentionally small: create/list sessions, send a
//! prompt, stream the answer, inspect the persisted transcript, cancel.

mod client;
mod tui;

use std::io::{self, IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use eix_core::config as core_config;
use eix_core::sandbox as core_sandbox;
use eix_core::{
    ApprovalRequiredPayload, ApprovalResult, AssistantMessagePayload, AttachResult, CancelResult,
    Config, Event, EventKind, HelloResult, Session, SessionList, SessionResult, SubmitResult,
    ToolCallPayload, ToolResultPayload, UserMessagePayload,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncBufReadExt;

use client::{ensure_daemon, Client};

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse()?;
    match cli.command.clone() {
        Command::Help => {
            print_help();
            Ok(())
        }
        Command::Daemon => daemon(&cli).await,
        Command::Doctor => doctor(&cli).await,
        Command::Sessions => sessions(&cli).await,
        Command::New { name, cwd, model } => new_session(&cli, name, cwd, model).await,
        Command::Send { session, text } => send(&cli, session, text).await,
        Command::Tui { session } => {
            let bell = Config::load(cli.config_path.as_deref())
                .map(|config| config.notifications.bell)
                .unwrap_or(false);
            tui::run(
                cli.socket.clone(),
                cli.data_dir.clone(),
                cli.config_path.clone(),
                session,
                bell,
            )
            .await
        }
        Command::Chat { session } => chat(&cli, session).await,
        Command::Log { session } => log(&cli, session).await,
        Command::Rename { session, name } => rename(&cli, session, name).await,
        Command::Cancel { session } => cancel(&cli, session).await,
        Command::Approve { session, hunks } => approval(&cli, session, true, hunks).await,
        Command::Deny { session } => approval(&cli, session, false, None).await,
    }
}

struct Cli {
    socket: PathBuf,
    data_dir: PathBuf,
    config_path: Option<PathBuf>,
    command: Command,
}

#[derive(Clone)]
enum Command {
    Tui {
        session: Option<String>,
    },
    Chat {
        session: Option<String>,
    },
    Sessions,
    New {
        name: Option<String>,
        cwd: Option<PathBuf>,
        model: Option<String>,
    },
    Send {
        session: Option<String>,
        text: String,
    },
    Log {
        session: Option<String>,
    },
    Rename {
        session: Option<String>,
        name: String,
    },
    Cancel {
        session: Option<String>,
    },
    Approve {
        session: Option<String>,
        /// Hunk indices to apply; `None` applies the whole change.
        hunks: Option<Vec<usize>>,
    },
    Deny {
        session: Option<String>,
    },
    Doctor,
    Daemon,
    Help,
}

impl Cli {
    fn parse() -> Result<Self> {
        let mut args: Vec<String> = std::env::args().skip(1).collect();
        let mut socket = None;
        let mut data_dir = None;
        let mut config_path = None;

        // Global flags may appear anywhere (before or after the subcommand).
        // `--` stops flag scanning so prompt text can contain flag-like words.
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--" => {
                    args.remove(i);
                    break;
                }
                "--socket" => {
                    socket = Some(PathBuf::from(take_value(&mut args, i, "--socket")?));
                }
                "--data-dir" => {
                    data_dir = Some(PathBuf::from(take_value(&mut args, i, "--data-dir")?));
                }
                "--config" => {
                    config_path = Some(PathBuf::from(take_value(&mut args, i, "--config")?));
                }
                "-h" | "--help" => return Ok(Self::help(socket, data_dir, config_path)),
                "-V" | "--version" => {
                    println!("eix {}", env!("CARGO_PKG_VERSION"));
                    std::process::exit(0);
                }
                _ => i += 1,
            }
        }

        let command = if args.is_empty() || args.first().is_some_and(|arg| arg == "--session") {
            // Bare `eix` (or `eix --session ID`) opens the TUI. When stdout is
            // piped, keep the non-interactive `chat` behavior.
            let name = if io::stdout().is_terminal() {
                "tui"
            } else {
                "chat"
            };
            parse_command(name.to_string(), args)?
        } else {
            let name = args.remove(0);
            parse_command(name, args)?
        };

        Ok(Self {
            socket: socket.unwrap_or_else(core_config::socket_path),
            data_dir: data_dir.unwrap_or_else(core_config::data_dir),
            config_path,
            command,
        })
    }

    fn help(
        socket: Option<PathBuf>,
        data_dir: Option<PathBuf>,
        config_path: Option<PathBuf>,
    ) -> Self {
        Self {
            socket: socket.unwrap_or_else(core_config::socket_path),
            data_dir: data_dir.unwrap_or_else(core_config::data_dir),
            config_path,
            command: Command::Help,
        }
    }

    async fn connect(&self) -> Result<Client> {
        ensure_daemon(&self.socket, &self.data_dir, self.config_path.as_deref()).await
    }
}

fn parse_command(name: String, mut args: Vec<String>) -> Result<Command> {
    match name.as_str() {
        "tui" => {
            let session = take_flag(&mut args, "--session")?;
            let session = session.or_else(|| args.first().cloned());
            Ok(Command::Tui { session })
        }
        "chat" => {
            let session = take_flag(&mut args, "--session")?;
            let session = session.or_else(|| args.first().cloned());
            Ok(Command::Chat { session })
        }
        "ls" | "list" | "sessions" => Ok(Command::Sessions),
        "new" => {
            let name = take_flag(&mut args, "--name")?;
            let cwd = take_flag(&mut args, "--cwd")?.map(PathBuf::from);
            let model = take_flag(&mut args, "--model")?;
            let name = name.or_else(|| {
                let joined = args.join(" ").trim().to_string();
                (!joined.is_empty()).then_some(joined)
            });
            Ok(Command::New { name, cwd, model })
        }
        "send" => {
            let session = take_flag(&mut args, "--session")?;
            Ok(Command::Send {
                session,
                text: args.join(" "),
            })
        }
        "log" => {
            let session = take_flag(&mut args, "--session")?;
            let session = session.or_else(|| args.first().cloned());
            Ok(Command::Log { session })
        }
        "rename" => {
            let session = take_flag(&mut args, "--session")?;
            let name = args.join(" ").trim().to_string();
            if name.is_empty() {
                bail!("usage: eix rename [--session ID] <name>");
            }
            Ok(Command::Rename { session, name })
        }
        "cancel" => {
            let session = take_flag(&mut args, "--session")?;
            let session = session.or_else(|| args.first().cloned());
            Ok(Command::Cancel { session })
        }
        "approve" => {
            let session = take_flag(&mut args, "--session")?;
            let hunks = take_flag(&mut args, "--hunks")?
                .map(|value| parse_hunks(&value))
                .transpose()?;
            let session = session.or_else(|| args.first().cloned());
            Ok(Command::Approve { session, hunks })
        }
        "deny" => {
            let session = take_flag(&mut args, "--session")?;
            let session = session.or_else(|| args.first().cloned());
            Ok(Command::Deny { session })
        }
        "doctor" => Ok(Command::Doctor),
        "daemon" | "up" => Ok(Command::Daemon),
        "help" => Ok(Command::Help),
        other => bail!("unknown command {other:?} (try `eix help`)"),
    }
}

fn take_value(args: &mut Vec<String>, index: usize, flag: &str) -> Result<String> {
    let value = args
        .get(index + 1)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("{flag} requires a value"))?;
    args.drain(index..=index + 1);
    Ok(value)
}

fn take_flag(args: &mut Vec<String>, flag: &str) -> Result<Option<String>> {
    let Some(index) = args.iter().position(|arg| arg == flag) else {
        return Ok(None);
    };
    Ok(Some(take_value(args, index, flag)?))
}

/// Parse the comma-separated hunk selection for `eix approve --hunks`.
fn parse_hunks(value: &str) -> Result<Vec<usize>> {
    let mut hunks = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            bail!("--hunks expects comma-separated indices, e.g. 0,2");
        }
        let index = part
            .parse()
            .with_context(|| format!("invalid hunk index {part:?}"))?;
        hunks.push(index);
    }
    hunks.sort_unstable();
    hunks.dedup();
    Ok(hunks)
}

async fn sessions(cli: &Cli) -> Result<()> {
    let mut client = cli.connect().await?;
    let list = list_sessions(&mut client).await?;
    if list.is_empty() {
        println!("no sessions yet — start one with `eix new` or just `eix`");
        return Ok(());
    }
    for session in list {
        let model = session.model.as_deref().unwrap_or("-");
        println!(
            "{} {:<28} {:<8} {:<10} {}  {}",
            session.status.symbol(),
            truncate(&session.name, 28),
            short_id(&session.id),
            model,
            session.cwd.display(),
            format_ts(session.updated_at),
        );
    }
    Ok(())
}

async fn new_session(
    cli: &Cli,
    name: Option<String>,
    cwd: Option<PathBuf>,
    model: Option<String>,
) -> Result<()> {
    let mut client = cli.connect().await?;
    let session = create_session(&mut client, name, cwd, model).await?;
    println!(
        "{} {}  {}",
        session.status.symbol(),
        session.name,
        short_id(&session.id)
    );
    Ok(())
}

async fn send(cli: &Cli, session: Option<String>, mut text: String) -> Result<()> {
    let mut client = cli.connect().await?;
    if text.trim().is_empty() && !io::stdin().is_terminal() {
        io::stdin()
            .read_to_string(&mut text)
            .context("cannot read prompt from stdin")?;
    }
    if text.trim().is_empty() {
        bail!("usage: eix send [--session ID] <text>");
    }
    let session = resolve_session(&mut client, session.as_deref(), true).await?;
    let submit = submit_prompt(&mut client, &session.id, text.trim()).await?;
    let outcome = stream_turn(&mut client, &session.id, &submit.turn_id, Display::Raw).await?;
    match outcome.error {
        Some(error) => bail!("{error}"),
        None => Ok(()),
    }
}

async fn chat(cli: &Cli, session: Option<String>) -> Result<()> {
    let mut client = cli.connect().await?;
    let mut session = resolve_session(&mut client, session.as_deref(), true).await?;

    println!("eix chat — {} ({})", session.name, short_id(&session.id));
    println!("type /help for commands, /exit or Ctrl-D to quit\n");
    let events = attach_events(&mut client, &session.id).await?;
    render_events(&events);
    if !events.is_empty() {
        println!();
    }

    let stdin = tokio::io::stdin();
    let mut lines = tokio::io::BufReader::new(stdin).lines();
    loop {
        print!("> ");
        io::stdout().flush().ok();
        let line = match lines.next_line().await? {
            Some(line) => line,
            None => break,
        };
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        match text {
            "/exit" | "/quit" | "/q" => break,
            "/help" => print_chat_help(),
            "/cancel" => {
                let result: CancelResult = serde_json::from_value(
                    client
                        .request("prompt.cancel", json!({"session_id": session.id}))
                        .await?,
                )?;
                if !result.cancelled {
                    println!("nothing running");
                }
            }
            "/new" => {
                session = create_session(&mut client, None, None, None).await?;
                println!("new session {} ({})", session.name, short_id(&session.id));
            }
            "/log" => {
                let events = attach_events(&mut client, &session.id).await?;
                render_events(&events);
            }
            other if other.starts_with('/') => {
                println!("unknown command {other:?} (try /help)");
            }
            _ => {
                let submit = match submit_prompt(&mut client, &session.id, text).await {
                    Ok(submit) => submit,
                    Err(err) => {
                        eprintln!("error: {err:#}");
                        continue;
                    }
                };
                let outcome =
                    stream_turn(&mut client, &session.id, &submit.turn_id, Display::Prefixed)
                        .await?;
                if outcome.cancelled {
                    println!("[cancelled]");
                }
                if let Some(error) = outcome.error {
                    eprintln!("error: {error}");
                }
            }
        }
    }
    Ok(())
}

async fn log(cli: &Cli, session: Option<String>) -> Result<()> {
    let mut client = cli.connect().await?;
    let session = resolve_session(&mut client, session.as_deref(), false).await?;
    let events = attach_events(&mut client, &session.id).await?;
    render_events(&events);
    Ok(())
}

async fn rename(cli: &Cli, session: Option<String>, name: String) -> Result<()> {
    let mut client = cli.connect().await?;
    let session = resolve_session(&mut client, session.as_deref(), false).await?;
    let result: SessionResult = serde_json::from_value(
        client
            .request(
                "session.rename",
                json!({"session_id": session.id, "name": name}),
            )
            .await?,
    )?;
    println!("renamed to {}", result.session.name);
    Ok(())
}

async fn cancel(cli: &Cli, session: Option<String>) -> Result<()> {
    let mut client = cli.connect().await?;
    let session = resolve_session(&mut client, session.as_deref(), false).await?;
    let result: CancelResult = serde_json::from_value(
        client
            .request("prompt.cancel", json!({"session_id": session.id}))
            .await?,
    )?;
    if result.cancelled {
        println!("cancelling {}", session.name);
    } else {
        println!("{} is not running", session.name);
    }
    Ok(())
}

async fn approval(
    cli: &Cli,
    session: Option<String>,
    approve: bool,
    hunks: Option<Vec<usize>>,
) -> Result<()> {
    let mut client = cli.connect().await?;
    let session = resolve_session(&mut client, session.as_deref(), false).await?;
    let result: ApprovalResult = serde_json::from_value(
        client
            .request(
                "approval.respond",
                json!({
                    "session_id": session.id,
                    "approval_id": "",
                    "decision": if approve { "approve" } else { "deny" },
                    "hunks": hunks,
                }),
            )
            .await?,
    )?;
    if result.accepted {
        println!(
            "{} {}",
            if approve { "approved" } else { "denied" },
            session.name
        );
    } else {
        println!("{} has no pending approval", session.name);
    }
    Ok(())
}

async fn daemon(cli: &Cli) -> Result<()> {
    let mut client = cli.connect().await?;
    let hello: HelloResult = serde_json::from_value(client.request("hello", Value::Null).await?)?;
    println!(
        "eixd {} pid {} on {}",
        hello.version,
        hello.pid,
        cli.socket.display()
    );
    println!("model {} via {}", hello.model, hello.base_url);
    println!("{} session(s)", hello.sessions);
    Ok(())
}

async fn doctor(cli: &Cli) -> Result<()> {
    println!("socket   {}", cli.socket.display());
    println!("data     {}", cli.data_dir.display());
    let config_path = cli
        .config_path
        .clone()
        .unwrap_or_else(core_config::default_config_path);
    println!(
        "config   {}{}",
        config_path.display(),
        if config_path.exists() {
            ""
        } else {
            " (missing, defaults)"
        }
    );

    let config = Config::load(cli.config_path.as_deref());
    match &config {
        Ok(config) => {
            println!("model    {}", config.provider.model);
            println!("api      {}", config.chat_endpoint());
            println!(
                "key      {}",
                if config.api_key().is_some() {
                    "present"
                } else {
                    "absent (set EIX_API_KEY or the configured env var)"
                }
            );
            println!(
                "sandbox  {} ({})",
                config.tools.sandbox.as_str(),
                if core_sandbox::bwrap_available() {
                    "bwrap available"
                } else {
                    "bwrap missing"
                }
            );
        }
        Err(err) => println!("config   ERROR: {err}"),
    }

    let exe = std::env::current_exe().ok();
    if let Some(exe) = exe {
        let eixd = exe.with_file_name("eixd");
        println!(
            "eixd     {}{}",
            eixd.display(),
            if eixd.exists() {
                ""
            } else {
                " (missing; run `cargo build`)"
            }
        );
    }

    let mut client = cli.connect().await?;
    let hello: HelloResult = serde_json::from_value(client.request("hello", Value::Null).await?)?;
    println!(
        "daemon   eixd {} pid {} protocol {}",
        hello.version, hello.pid, hello.protocol
    );
    println!("model    {}", hello.model);
    println!("api      {}", hello.base_url);
    println!("sessions {}", hello.sessions);
    Ok(())
}

async fn list_sessions(client: &mut Client) -> Result<Vec<Session>> {
    let list: SessionList =
        serde_json::from_value(client.request("session.list", Value::Null).await?)?;
    Ok(list.sessions)
}

async fn create_session(
    client: &mut Client,
    name: Option<String>,
    cwd: Option<PathBuf>,
    model: Option<String>,
) -> Result<Session> {
    let cwd = match cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir().context("cannot determine current directory")?,
    };
    let result: SessionResult = serde_json::from_value(
        client
            .request(
                "session.create",
                json!({
                    "name": name,
                    "cwd": cwd,
                    "model": model,
                }),
            )
            .await?,
    )?;
    Ok(result.session)
}

async fn resolve_session(
    client: &mut Client,
    needle: Option<&str>,
    create: bool,
) -> Result<Session> {
    let sessions = list_sessions(client).await?;
    if let Some(needle) = needle {
        let matches: Vec<&Session> = sessions
            .iter()
            .filter(|session| session.id == needle || session.id.starts_with(needle))
            .collect();
        return match matches.as_slice() {
            [session] => Ok((*session).clone()),
            [] => bail!("unknown session {needle:?}"),
            _ => bail!("session id {needle:?} is ambiguous"),
        };
    }
    if let Some(session) = sessions.first() {
        return Ok(session.clone());
    }
    if create {
        return create_session(client, None, None, None).await;
    }
    bail!("no sessions yet — run `eix new` first")
}

async fn attach_events(client: &mut Client, session_id: &str) -> Result<Vec<Event>> {
    let attach: AttachResult = serde_json::from_value(
        client
            .request("session.attach", json!({"session_id": session_id}))
            .await?,
    )?;
    Ok(attach.events)
}

async fn submit_prompt(client: &mut Client, session_id: &str, text: &str) -> Result<SubmitResult> {
    Ok(serde_json::from_value(
        client
            .request(
                "prompt.submit",
                json!({"session_id": session_id, "text": text}),
            )
            .await?,
    )?)
}

#[derive(Deserialize)]
struct DeltaParams {
    session_id: String,
    turn_id: String,
    delta: String,
}

#[derive(Deserialize)]
struct TurnCompleted {
    session_id: String,
    turn_id: String,
    #[serde(default)]
    cancelled: bool,
    #[serde(default)]
    error: Option<String>,
}

struct TurnOutcome {
    cancelled: bool,
    error: Option<String>,
}

#[derive(Clone, Copy)]
enum Display {
    Raw,
    Prefixed,
}

async fn stream_turn(
    client: &mut Client,
    session_id: &str,
    turn_id: &str,
    display: Display,
) -> Result<TurnOutcome> {
    let mut wrote = false;
    loop {
        let notification = client.next_notification().await?;
        match notification.method.as_str() {
            "message.delta" => {
                let params: DeltaParams = serde_json::from_value(notification.params)?;
                if params.session_id != session_id || params.turn_id != turn_id {
                    continue;
                }
                if !wrote && matches!(display, Display::Prefixed) {
                    print!("eix> ");
                }
                print!("{}", params.delta);
                io::stdout().flush().ok();
                wrote = true;
            }
            "turn.completed" => {
                let params: TurnCompleted = serde_json::from_value(notification.params)?;
                if params.session_id != session_id || params.turn_id != turn_id {
                    continue;
                }
                if wrote {
                    println!();
                }
                return Ok(TurnOutcome {
                    cancelled: params.cancelled,
                    error: params.error,
                });
            }
            "tool.approval_required" => {
                if notification.params["session_id"] == session_id {
                    if wrote {
                        println!();
                        wrote = false;
                    }
                    let summary = notification.params["summary"]
                        .as_str()
                        .unwrap_or("tool call");
                    eprintln!("approval needed: {summary}");
                    if let Some(diff) = notification.params["diff"]
                        .as_str()
                        .filter(|diff| !diff.is_empty())
                    {
                        eprintln!("{diff}");
                    }
                    eprintln!("  run `eix approve` or `eix deny`");
                }
            }
            "stream.lagged" => {
                if let Some(skipped) = notification.params.get("skipped") {
                    eprintln!("warning: dropped {skipped} notifications (client is too slow)");
                }
            }
            _ => {}
        }
    }
}

fn render_events(events: &[Event]) {
    for event in events {
        match event.kind {
            EventKind::UserMessage => {
                if let Ok(payload) =
                    serde_json::from_value::<UserMessagePayload>(event.payload.clone())
                {
                    print_message("you", &payload.text, "");
                }
            }
            EventKind::AssistantMessage => {
                if let Ok(payload) =
                    serde_json::from_value::<AssistantMessagePayload>(event.payload.clone())
                {
                    if payload.text.is_empty() {
                        continue;
                    }
                    let suffix = if payload.cancelled {
                        " [cancelled]"
                    } else {
                        ""
                    };
                    print_message("eix", &payload.text, suffix);
                }
            }
            EventKind::ToolCall => {
                if let Ok(payload) =
                    serde_json::from_value::<ToolCallPayload>(event.payload.clone())
                {
                    println!(
                        "tool> {}",
                        tool_call_summary(&payload.tool, &payload.arguments)
                    );
                }
            }
            EventKind::ApprovalRequired => {
                if let Ok(payload) =
                    serde_json::from_value::<ApprovalRequiredPayload>(event.payload.clone())
                {
                    println!(
                        "need> {} (y/n in the TUI, or `eix approve/deny`)",
                        payload.summary
                    );
                    if let Some(diff) = payload.diff.as_deref().filter(|diff| !diff.is_empty()) {
                        print!("{diff}");
                    }
                }
            }
            EventKind::ToolResult => {
                if let Ok(payload) =
                    serde_json::from_value::<ToolResultPayload>(event.payload.clone())
                {
                    let status = if payload.denied {
                        "denied"
                    } else if payload.ok {
                        "ok"
                    } else {
                        "failed"
                    };
                    println!("tool> {} → {status}", payload.tool);
                    for line in payload.output.trim_end().lines() {
                        println!("      {line}");
                    }
                }
            }
            EventKind::Error => {
                let message = event
                    .payload
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                println!("error: {message}");
            }
            EventKind::StatusChanged => {
                let from = event
                    .payload
                    .get("from")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                let to = event
                    .payload
                    .get("to")
                    .and_then(Value::as_str)
                    .unwrap_or("?");
                println!("-- {from} -> {to}");
            }
            _ => {}
        }
    }
}

fn tool_call_summary(tool: &str, arguments: &Value) -> String {
    let string = |key: &str| {
        arguments
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    match tool {
        "write_file" => {
            let bytes = string("content").len();
            format!("write_file {} (+{bytes} bytes)", string("path"))
        }
        "run_command" => format!("run_command: {}", string("command")),
        "read_file" | "list_dir" => {
            let path = string("path");
            format!("{tool} {}", if path.is_empty() { "." } else { path })
        }
        _ => {
            let compact = serde_json::to_string(arguments).unwrap_or_default();
            format!("{tool} {}", truncate(&compact, 80))
        }
    }
}

fn print_message(role: &str, text: &str, suffix: &str) {
    let mut lines = text.lines();
    match lines.next() {
        Some(first) => println!("{role}> {first}{suffix}"),
        None => {
            println!("{role}> {suffix}");
            return;
        }
    }
    for line in lines {
        println!("     {line}");
    }
}

fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn format_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ts)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%m-%d %H:%M")
                .to_string()
        })
        .unwrap_or_default()
}

fn print_chat_help() {
    println!(
        "\
/help            this help
/cancel          cancel the running turn
/new             start a new session
/log             reprint this session's transcript
/exit, /quit     leave
anything else    send as a prompt"
    );
}

fn print_help() {
    println!(
        "\
eix — coding agent with persistent, attachable sessions

USAGE:
    eix [OPTIONS] [COMMAND]

COMMANDS:
    tui [--session ID]         full-screen terminal UI (default)
    chat [--session ID]        interactive chat (non-TUI)
    send [--session ID] TEXT   send one prompt and stream the answer
    ls                         list sessions
    new [--name N] [--cwd P] [--model M]
                               create a session
    log [--session ID]         print the persisted transcript
    rename [--session ID] NAME
    cancel [--session ID]      cancel the running turn
    approve [--session ID] [--hunks 0,2]
                               approve the pending tool call (all or selected hunks)
    deny [--session ID]        deny the pending tool call
    doctor                     diagnose config, paths and daemon
    daemon                     start/attach the daemon and print its status
    help                       show this help

OPTIONS:
    --socket <path>     daemon socket (default: $XDG_RUNTIME_DIR/eix.sock)
    --data-dir <path>   state directory (default: ~/.local/share/eix)
    --config <path>     config file (default: ~/.config/eix/config.toml)

The daemon starts automatically. With no command, `eix` opens the TUI; when stdout
is piped it falls back to `eix chat`. Prompt text after `--` is never parsed as a flag.

EXAMPLES:
    eix send 'explain the session model in DESIGN.md'
    echo 'review the diff' | eix send
    EIX_MODEL=qwen2.5-coder eix chat
    eix new --name auth --cwd ~/src/app"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hunk_selection() {
        assert_eq!(parse_hunks("0,2").unwrap(), vec![0, 2]);
        assert_eq!(parse_hunks("2, 0, 2").unwrap(), vec![0, 2]);
        assert_eq!(parse_hunks("1").unwrap(), vec![1]);
    }

    #[test]
    fn rejects_bad_hunk_selections() {
        assert!(parse_hunks("").is_err());
        assert!(parse_hunks("0,,1").is_err());
        assert!(parse_hunks("x").is_err());
        assert!(parse_hunks("-1").is_err());
    }

    #[test]
    fn default_tui_command_takes_an_optional_session() {
        match parse_command("tui".to_string(), Vec::new()).unwrap() {
            Command::Tui { session } => assert!(session.is_none()),
            _ => panic!("expected tui"),
        }
        match parse_command(
            "tui".to_string(),
            vec!["--session".to_string(), "abc".to_string()],
        )
        .unwrap()
        {
            Command::Tui { session } => assert_eq!(session.as_deref(), Some("abc")),
            _ => panic!("expected tui"),
        }
    }
}
