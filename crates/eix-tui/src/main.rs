//! `eix`: CLI frontend and the first user-facing slice of the eix agent.
//!
//! It talks to `eixd` over the Unix socket and starts the daemon on demand.
//! Phase 0 commands are intentionally small: create/list sessions, send a
//! prompt, stream the answer, inspect the persisted transcript, cancel.

mod client;
mod nvim_frontend;
mod tui;

use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use eix_core::config as core_config;
use eix_core::sandbox as core_sandbox;
use eix_core::{
    ApprovalListResult, ApprovalRequiredPayload, ApprovalResult, AssistantMessagePayload,
    AttachResult, CancelResult, Config, ContextCompactedPayload, ContextDropPayload,
    ContextLedgerResult, ContextPinPayload, Endpoint, Event, EventKind, HelloResult,
    InputAnsweredPayload, InputRequiredPayload, InputResult, PatchProposedPayload,
    PatchResultPayload, PermissionPayload, ProcessExitedPayload, ProcessKillResult, ProcessList,
    ProcessOutput, ProcessStartedPayload, RewindResult, SearchResult, Sector, Session, SessionList,
    SessionResult, SessionStatus, SnapshotPayload, SubmitResult, ToolCallPayload,
    ToolResultPayload, UserMessagePayload, WorktreeRemoveResult,
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
        Command::Sessions { archived } => sessions(&cli, archived).await,
        Command::New {
            name,
            cwd,
            model,
            worktree,
            permission,
        } => new_session(&cli, name, cwd, model, worktree, permission).await,
        Command::Permission {
            session,
            level,
            temporary,
        } => permission(&cli, session, level, temporary).await,
        Command::Send {
            session,
            text,
            images,
        } => send(&cli, session, text, images).await,
        Command::Tui { session } => {
            let config = Config::load(cli.config_path.as_deref()).unwrap_or_default();
            tui::run(
                cli.endpoint.clone(),
                cli.data_dir.clone(),
                cli.config_path.clone(),
                session,
                config.notifications.bell,
            )
            .await
        }
        Command::Chat { session } => chat(&cli, session).await,
        Command::Log { session } => log(&cli, session).await,
        Command::Rename { session, name } => rename(&cli, session, name).await,
        Command::Cancel { session } => cancel(&cli, session).await,
        Command::Approve {
            session,
            hunks,
            reason,
        } => approval(&cli, session, true, hunks, reason).await,
        Command::Deny { session, reason } => approval(&cli, session, false, None, reason).await,
        Command::Answer { session, option } => answer(&cli, session, option).await,
        Command::Approvals { json } => approvals(&cli, json).await,
        Command::Status { json } => status(&cli, json).await,
        Command::Pick { query, limit, json } => pick(&cli, query, limit, json).await,
        Command::Context(args) => context(&cli, args).await,
        Command::Search {
            query,
            session,
            limit,
        } => search(&cli, query, session, limit).await,
        Command::Fork {
            session,
            at,
            name,
            shared,
        } => fork(&cli, session, at, name, shared).await,
        Command::Rewind { session, at, name } => rewind(&cli, session, at, name).await,
        Command::Delete { session } => delete_session(&cli, session).await,
        Command::Archive { session, undo } => archive_session(&cli, session, undo).await,
        Command::Export { session, output } => export_session(&cli, session, output).await,
        Command::Import { file, name, cwd } => import_session(&cli, file, name, cwd).await,
        Command::Timeline {
            session,
            limit,
            json,
        } => timeline(&cli, session, limit, json).await,
        Command::Undo { session, redo } => undo(&cli, session, redo).await,
        Command::Worktree(cmd) => worktree(&cli, cmd).await,
        Command::Nvim { session } => nvim(&cli, session).await,
        Command::Processes { session } => processes(&cli, session).await,
        Command::Tail { process } => tail(&cli, &process).await,
        Command::Kill { process } => kill(&cli, &process).await,
    }
}

struct Cli {
    socket: PathBuf,
    data_dir: PathBuf,
    config_path: Option<PathBuf>,
    /// Resolved transport: Unix socket or TCP remote.
    endpoint: Endpoint,
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
    Sessions {
        /// Include archived sessions.
        archived: bool,
    },
    New {
        name: Option<String>,
        cwd: Option<PathBuf>,
        model: Option<String>,
        /// Create `.worktrees/<name>` and run there.
        worktree: bool,
        /// Initial per-session permission level.
        permission: Option<String>,
    },
    Permission {
        session: Option<String>,
        /// `read-only` / `workspace-write` / `full`.
        level: String,
        /// Do not persist the change (lost on daemon restart).
        temporary: bool,
    },
    Send {
        session: Option<String>,
        text: String,
        images: Vec<String>,
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
        reason: Option<String>,
    },
    Deny {
        session: Option<String>,
        reason: Option<String>,
    },
    Answer {
        session: Option<String>,
        /// 1-based option number.
        option: usize,
    },
    Approvals {
        json: bool,
    },
    Status {
        json: bool,
    },
    Pick {
        query: Option<String>,
        limit: Option<usize>,
        json: bool,
    },
    Context(ContextArgs),
    Search {
        query: String,
        session: Option<String>,
        limit: Option<u32>,
    },
    Fork {
        session: Option<String>,
        /// Copy history before this sequence; `None` copies everything.
        at: Option<u64>,
        name: Option<String>,
        /// Share the source workspace instead of creating a worktree.
        shared: bool,
    },
    Rewind {
        session: Option<String>,
        /// Restore the workspace to just before this sequence.
        at: Option<u64>,
        name: Option<String>,
    },
    Delete {
        session: Option<String>,
    },
    Archive {
        session: Option<String>,
        /// Restore an archived session instead.
        undo: bool,
    },
    Export {
        session: Option<String>,
        /// Write to this path instead of stdout.
        output: Option<PathBuf>,
    },
    Import {
        file: PathBuf,
        name: Option<String>,
        cwd: Option<PathBuf>,
    },
    Timeline {
        session: Option<String>,
        limit: Option<usize>,
        json: bool,
    },
    Undo {
        session: Option<String>,
        redo: bool,
    },
    Worktree(WorktreeCmd),
    Nvim {
        session: Option<String>,
    },
    Processes {
        session: Option<String>,
    },
    Tail {
        process: String,
    },
    Kill {
        process: String,
    },
    Doctor,
    Daemon,
    Help,
}

#[derive(Clone)]
enum WorktreeCmd {
    List,
    Remove {
        session: Option<String>,
        force: bool,
    },
}

#[derive(Clone, Default)]
struct ContextArgs {
    session: Option<String>,
    drop: Option<String>,
    undrop: Option<String>,
    pin: Option<String>,
    unpin: Option<String>,
    /// `FROM:TO` or `FROM..TO`; inclusive source event sequences.
    compact: Option<String>,
    summary: Option<String>,
}

impl Cli {
    fn parse() -> Result<Self> {
        let mut args: Vec<String> = std::env::args().skip(1).collect();
        let mut socket = None;
        let mut data_dir = None;
        let mut config_path = None;
        let mut tcp: Option<String> = std::env::var("EIX_TCP").ok();
        let mut token: Option<String> = std::env::var("EIX_TCP_TOKEN").ok();

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
                "--tcp" => {
                    tcp = Some(take_value(&mut args, i, "--tcp")?);
                }
                "--token" => {
                    token = Some(take_value(&mut args, i, "--token")?);
                }
                "-h" | "--help" => {
                    return Ok(Self::help(socket, data_dir, config_path, tcp, token))
                }
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

        let socket = socket.unwrap_or_else(core_config::socket_path);
        let endpoint = Endpoint::new(socket.clone(), tcp, token);
        Ok(Self {
            socket,
            data_dir: data_dir.unwrap_or_else(core_config::data_dir),
            config_path,
            endpoint,
            command,
        })
    }

    fn help(
        socket: Option<PathBuf>,
        data_dir: Option<PathBuf>,
        config_path: Option<PathBuf>,
        tcp: Option<String>,
        token: Option<String>,
    ) -> Self {
        let socket = socket.unwrap_or_else(core_config::socket_path);
        let endpoint = Endpoint::new(socket.clone(), tcp, token);
        Self {
            socket,
            data_dir: data_dir.unwrap_or_else(core_config::data_dir),
            config_path,
            endpoint,
            command: Command::Help,
        }
    }

    async fn connect(&self) -> Result<Client> {
        ensure_daemon(&self.endpoint, &self.data_dir, self.config_path.as_deref()).await
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
        "ls" | "list" | "sessions" => {
            let archived = take_switch(&mut args, "--archived") || take_switch(&mut args, "-a");
            Ok(Command::Sessions { archived })
        }
        "new" => {
            let name = take_flag(&mut args, "--name")?;
            let cwd = take_flag(&mut args, "--cwd")?.map(PathBuf::from);
            let model = take_flag(&mut args, "--model")?;
            let worktree = take_switch(&mut args, "--worktree");
            let permission = take_flag(&mut args, "--permission")?;
            let name = name.or_else(|| {
                let joined = args.join(" ").trim().to_string();
                (!joined.is_empty()).then_some(joined)
            });
            Ok(Command::New {
                name,
                cwd,
                model,
                worktree,
                permission,
            })
        }
        "permission" | "perm" => {
            let session = take_flag(&mut args, "--session")?;
            let temporary = take_switch(&mut args, "--temporary") || take_switch(&mut args, "-t");
            let level = args
                .first()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!(
                    "usage: eix permission <read-only|workspace-write|full> [--session ID] [--temporary]"
                ))?;
            Ok(Command::Permission {
                session,
                level,
                temporary,
            })
        }
        "send" => {
            let session = take_flag(&mut args, "--session")?;
            let mut images = Vec::new();
            while let Some(image) = take_flag(&mut args, "--image")? {
                images.push(image);
            }
            Ok(Command::Send {
                session,
                text: args.join(" "),
                images,
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
            let reason = take_flag(&mut args, "--reason")?;
            let session = session.or_else(|| args.first().cloned());
            Ok(Command::Approve {
                session,
                hunks,
                reason,
            })
        }
        "deny" => {
            let session = take_flag(&mut args, "--session")?;
            let reason = take_flag(&mut args, "--reason")?;
            let session = session.or_else(|| args.first().cloned());
            Ok(Command::Deny { session, reason })
        }
        "answer" | "reply" => {
            let session = take_flag(&mut args, "--session")?;
            let option = args
                .first()
                .context("usage: eix answer [--session ID] <option>")?
                .parse::<usize>()
                .context("option must be a number")?;
            if option == 0 {
                bail!("option numbers start at 1");
            }
            Ok(Command::Answer { session, option })
        }
        "approvals" | "inbox" => {
            let json = take_switch(&mut args, "--json");
            Ok(Command::Approvals { json })
        }
        "status" => {
            let json = take_switch(&mut args, "--json");
            Ok(Command::Status { json })
        }
        "pick" => {
            let query = take_flag(&mut args, "--query")?;
            let query = query.or_else(|| {
                let joined = args.join(" ").trim().to_string();
                (!joined.is_empty()).then_some(joined)
            });
            let limit = take_flag(&mut args, "--limit")?
                .map(|value| value.parse::<usize>().context("invalid --limit"))
                .transpose()?;
            let json = take_switch(&mut args, "--json");
            Ok(Command::Pick { query, limit, json })
        }
        "context" | "ctx" | "ledger" => {
            let session = take_flag(&mut args, "--session")?;
            let drop = take_flag(&mut args, "--drop")?;
            let undrop = take_flag(&mut args, "--undrop")?;
            let pin = take_flag(&mut args, "--pin")?;
            let unpin = take_flag(&mut args, "--unpin")?;
            let compact = take_flag(&mut args, "--compact")?;
            let summary = take_flag(&mut args, "--summary")?;
            let session = session.or_else(|| args.first().cloned());
            let edits = [
                drop.is_some(),
                undrop.is_some(),
                pin.is_some(),
                unpin.is_some(),
                compact.is_some(),
            ]
            .into_iter()
            .filter(|active| *active)
            .count();
            if edits > 1 {
                bail!(
                    "eix context takes at most one of --drop, --undrop, --pin, --unpin, --compact"
                );
            }
            if summary.is_some() && compact.is_none() {
                bail!("--summary is only meaningful with --compact");
            }
            Ok(Command::Context(ContextArgs {
                session,
                drop,
                undrop,
                pin,
                unpin,
                compact,
                summary,
            }))
        }
        "search" | "find" => {
            let session = take_flag(&mut args, "--session")?;
            let limit = take_flag(&mut args, "--limit")?
                .map(|value| value.parse::<u32>().context("invalid --limit"))
                .transpose()?;
            let query = args.join(" ").trim().to_string();
            if query.is_empty() {
                bail!("usage: eix search QUERY [--session ID] [--limit N]");
            }
            Ok(Command::Search {
                query,
                session,
                limit,
            })
        }
        "fork" => {
            let session = take_flag(&mut args, "--session")?;
            let at = take_flag(&mut args, "--at")?
                .map(|value| value.parse::<u64>().context("invalid --at"))
                .transpose()?;
            let name = take_flag(&mut args, "--name")?;
            let shared = take_switch(&mut args, "--shared");
            Ok(Command::Fork {
                session,
                at,
                name,
                shared,
            })
        }
        "rewind" => {
            let session = take_flag(&mut args, "--session")?;
            let at = take_flag(&mut args, "--at")?
                .map(|value| value.parse::<u64>().context("invalid --at"))
                .transpose()?;
            let name = take_flag(&mut args, "--name")?;
            Ok(Command::Rewind { session, at, name })
        }
        "delete" | "rm" => {
            let session = take_flag(&mut args, "--session")?;
            let session = session.or_else(|| args.first().cloned());
            if session.is_none() {
                bail!("usage: eix delete <session>");
            }
            Ok(Command::Delete { session })
        }
        "archive" => {
            let session = take_flag(&mut args, "--session")?;
            let undo = take_switch(&mut args, "--undo");
            let session = session.or_else(|| args.first().cloned());
            if session.is_none() {
                bail!("usage: eix archive <session> [--undo]");
            }
            Ok(Command::Archive { session, undo })
        }
        "export" => {
            let session = take_flag(&mut args, "--session")?;
            let output = take_flag(&mut args, "--output")?.map(PathBuf::from);
            Ok(Command::Export { session, output })
        }
        "import" => {
            let name = take_flag(&mut args, "--name")?;
            let cwd = take_flag(&mut args, "--cwd")?.map(PathBuf::from);
            let file =
                args.first().cloned().map(PathBuf::from).ok_or_else(|| {
                    anyhow::anyhow!("usage: eix import <file> [--name N] [--cwd P]")
                })?;
            Ok(Command::Import { file, name, cwd })
        }
        "undo" | "redo" => {
            let session = take_flag(&mut args, "--session")?;
            Ok(Command::Undo {
                session,
                redo: name == "redo",
            })
        }
        "timeline" | "tl" => {
            let session = take_flag(&mut args, "--session")?;
            let limit = take_flag(&mut args, "--limit")?
                .map(|value| value.parse::<usize>().context("invalid --limit"))
                .transpose()?;
            let json = take_switch(&mut args, "--json");
            Ok(Command::Timeline {
                session,
                limit,
                json,
            })
        }
        "worktree" | "wt" => {
            let sub = args.first().cloned().unwrap_or_else(|| "list".to_string());
            args.remove(0);
            match sub.as_str() {
                "list" | "ls" => {
                    take_flag(&mut args, "--session")?;
                    Ok(Command::Worktree(WorktreeCmd::List))
                }
                "remove" | "rm" => {
                    let session = take_flag(&mut args, "--session")?;
                    let force = take_switch(&mut args, "--force");
                    let session = session.or_else(|| args.first().cloned());
                    Ok(Command::Worktree(WorktreeCmd::Remove { session, force }))
                }
                other => {
                    bail!("usage: eix worktree [list | remove SESSION [--force]] (got {other:?})")
                }
            }
        }
        "nvim" => {
            let session = take_flag(&mut args, "--session")?;
            let session = session.or_else(|| args.first().cloned());
            Ok(Command::Nvim { session })
        }
        "ps" | "processes" => {
            let session = take_flag(&mut args, "--session")?;
            Ok(Command::Processes { session })
        }
        "tail" | "output" => {
            let process = args
                .first()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("usage: eix tail <process>"))?;
            Ok(Command::Tail { process })
        }
        "kill" => {
            let process = args
                .first()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("usage: eix kill <process>"))?;
            Ok(Command::Kill { process })
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

/// Remove a boolean `--flag` and report whether it was present.
fn take_switch(args: &mut Vec<String>, flag: &str) -> bool {
    match args.iter().position(|arg| arg == flag) {
        Some(index) => {
            args.remove(index);
            true
        }
        None => false,
    }
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

async fn sessions(cli: &Cli, archived: bool) -> Result<()> {
    let mut client = cli.connect().await?;
    let list = list_sessions_including(&mut client, archived).await?;
    if list.is_empty() {
        println!("no sessions yet — start one with `eix new` or just `eix`");
        return Ok(());
    }
    for session in list {
        let model = session.model.as_deref().unwrap_or("-");
        println!(
            "{} {:<28} {:<8} {:<15} {:<10} {}  {}",
            session.status.symbol(),
            truncate(&session.name, 28),
            short_id(&session.id),
            session.permission.as_str(),
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
    worktree: bool,
    permission: Option<String>,
) -> Result<()> {
    let defaults_cwd = cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();
    let defaults = eix_core::load_cli_config(&defaults_cwd)
        .ok()
        .flatten()
        .unwrap_or_default();
    let model = model.or(defaults.model);
    let permission = permission.or(defaults.permission);
    let mut client = cli.connect().await?;
    let session = create_session(&mut client, name, cwd, model, worktree, permission).await?;
    println!(
        "{} {}  {}  [{}]",
        session.status.symbol(),
        session.name,
        short_id(&session.id),
        session.permission.as_str()
    );
    if session.has_worktree() {
        println!("worktree {}", session.cwd.display());
    }
    Ok(())
}

async fn send(
    cli: &Cli,
    session: Option<String>,
    mut text: String,
    images: Vec<String>,
) -> Result<()> {
    let mut client = cli.connect().await?;
    if text.trim().is_empty() && !io::stdin().is_terminal() {
        io::stdin()
            .read_to_string(&mut text)
            .context("cannot read prompt from stdin")?;
    }
    if text.trim().is_empty() && images.is_empty() {
        bail!("usage: eix send [--session ID] [--image PATH]... <text>");
    }
    let session = resolve_session(&mut client, session.as_deref(), true).await?;
    let submit = submit_prompt(&mut client, &session.id, text.trim(), &images).await?;
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
                session = create_session(&mut client, None, None, None, false, None).await?;
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
                let submit = match submit_prompt(&mut client, &session.id, text, &[]).await {
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

async fn permission(
    cli: &Cli,
    session: Option<String>,
    level: String,
    temporary: bool,
) -> Result<()> {
    let mut client = cli.connect().await?;
    let session = resolve_session(&mut client, session.as_deref(), false).await?;
    let result: SessionResult = serde_json::from_value(
        client
            .request(
                "session.permission",
                json!({
                    "session_id": session.id,
                    "permission": level,
                    "temporary": temporary,
                }),
            )
            .await?,
    )?;
    println!(
        "{} is now {} ({})",
        result.session.name,
        result.session.permission.as_str(),
        if temporary { "temporary" } else { "persisted" }
    );
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
    reason: Option<String>,
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
                    "reason": reason,
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

async fn answer(cli: &Cli, session: Option<String>, option: usize) -> Result<()> {
    let mut client = cli.connect().await?;
    let session = resolve_session(&mut client, session.as_deref(), false).await?;
    let result: InputResult = serde_json::from_value(
        client
            .request(
                "input.respond",
                json!({
                    "session_id": session.id,
                    "input_id": "",
                    "option_index": option - 1,
                }),
            )
            .await?,
    )?;
    if result.accepted {
        println!("answered {} (option {option})", session.name);
    } else {
        println!("{} has no pending question", session.name);
    }
    Ok(())
}

async fn approvals(cli: &Cli, json: bool) -> Result<()> {
    let mut client = cli.connect().await?;
    let result: ApprovalListResult = serde_json::from_value(
        client
            .request("approval.list", json!({"session_id": null}))
            .await?,
    )?;
    if json {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    if result.approvals.is_empty() {
        println!("no pending approvals");
        return Ok(());
    }
    for item in &result.approvals {
        println!(
            "⚠ {:<24} {:<6} {:<12} {:<40} {}",
            truncate(&item.session_name, 24),
            item.kind,
            item.tool,
            truncate(&item.summary, 40),
            short_id(&item.approval_id),
        );
    }
    Ok(())
}

async fn status(cli: &Cli, json: bool) -> Result<()> {
    let mut client = cli.connect().await?;
    let hello: HelloResult = serde_json::from_value(client.request("hello", Value::Null).await?)?;
    let sessions = list_sessions(&mut client).await?;
    let running = sessions
        .iter()
        .filter(|session| session.status == SessionStatus::Running)
        .count();
    let approval = sessions
        .iter()
        .filter(|session| session.status == SessionStatus::AwaitingApproval)
        .count();
    let input = sessions
        .iter()
        .filter(|session| session.status == SessionStatus::AwaitingInput)
        .count();
    let error = sessions
        .iter()
        .filter(|session| session.status == SessionStatus::Error)
        .count();
    let attention: Vec<&Session> = sessions
        .iter()
        .filter(|session| session.status.needs_attention())
        .collect();

    if json {
        let value = json!({
            // The first three keys are the documented dwmblocks contract.
            "running": running > 0,
            "approval": approval,
            "error": error,
            "input": input,
            "sessions": sessions.len(),
            "attention": attention.iter().map(|session| json!({
                "id": session.id,
                "name": session.name,
                "status": session.status.as_str(),
                "permission": session.permission.as_str(),
                "cwd": session.cwd,
            })).collect::<Vec<_>>(),
            "daemon": {
                "version": hello.version,
                "pid": hello.pid,
                "endpoint": cli.endpoint.to_string(),
                "model": hello.model,
                "base_url": hello.base_url,
            },
        });
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    println!(
        "eixd {} pid {} on {}",
        hello.version, hello.pid, cli.endpoint
    );
    println!(
        "{} session(s): {} running, {} awaiting approval, {} awaiting input, {} error",
        sessions.len(),
        running,
        approval,
        input,
        error
    );
    for session in attention {
        println!(
            "{} {:<28} {}  {}",
            session.status.symbol(),
            truncate(&session.name, 28),
            short_id(&session.id),
            session.cwd.display()
        );
    }
    Ok(())
}

async fn pick(cli: &Cli, query: Option<String>, limit: Option<usize>, json: bool) -> Result<()> {
    let mut client = cli.connect().await?;
    let mut sessions = list_sessions(&mut client).await?;
    if let Some(query) = query.as_deref().filter(|query| !query.trim().is_empty()) {
        sessions.retain(|session| pick_matches(query, session));
    }
    // Attention first, then most recently used.
    sessions.sort_by_key(|session| {
        (
            !session.status.needs_attention(),
            std::cmp::Reverse(session.updated_at),
        )
    });
    if let Some(limit) = limit {
        sessions.truncate(limit);
    }
    if json {
        let rows: Vec<Value> = sessions
            .iter()
            .map(|session| {
                json!({
                    "id": session.id,
                    "name": session.name,
                    "status": session.status.as_str(),
                    "permission": session.permission.as_str(),
                    "cwd": session.cwd,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    // Tab-separated so rofi/dmenu/fzf can split on the first field.
    for session in sessions {
        println!(
            "{}\t{}\t{}\t{}",
            session.id,
            session.name,
            session.status.as_str(),
            session.cwd.display()
        );
    }
    Ok(())
}

/// Case-insensitive subsequence match across the id, name, status and cwd.
fn pick_matches(query: &str, session: &Session) -> bool {
    let haystack = format!(
        "{} {} {} {}",
        session.id,
        session.name,
        session.status.as_str(),
        session.cwd.display()
    )
    .to_lowercase();
    let mut chars = haystack.chars();
    query
        .to_lowercase()
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .all(|needle| chars.any(|ch| ch == needle))
}

async fn context(cli: &Cli, args: ContextArgs) -> Result<()> {
    let mut client = cli.connect().await?;
    let session = resolve_session(&mut client, args.session.as_deref(), false).await?;
    let result: ContextLedgerResult = if let Some(entry) = args.drop {
        request_ledger(
            &mut client,
            "context.drop",
            json!({"session_id": session.id, "entry": entry, "dropped": true}),
        )
        .await?
    } else if let Some(entry) = args.undrop {
        request_ledger(
            &mut client,
            "context.drop",
            json!({"session_id": session.id, "entry": entry, "dropped": false}),
        )
        .await?
    } else if let Some(entry) = args.pin {
        request_ledger(
            &mut client,
            "context.pin",
            json!({"session_id": session.id, "entry": entry, "pinned": true}),
        )
        .await?
    } else if let Some(entry) = args.unpin {
        request_ledger(
            &mut client,
            "context.pin",
            json!({"session_id": session.id, "entry": entry, "pinned": false}),
        )
        .await?
    } else if let Some(range) = args.compact {
        let (from, to) = parse_range(&range)?;
        request_ledger(
            &mut client,
            "context.compact",
            json!({
                "session_id": session.id,
                "from_seq": from,
                "to_seq": to,
                "summary": args.summary,
            }),
        )
        .await?
    } else {
        request_ledger(
            &mut client,
            "context.ledger",
            json!({"session_id": session.id}),
        )
        .await?
    };
    render_ledger(&session, &result);
    Ok(())
}

async fn request_ledger(
    client: &mut Client,
    method: &str,
    params: Value,
) -> Result<ContextLedgerResult> {
    Ok(serde_json::from_value(
        client.request(method, params).await?,
    )?)
}

/// Full-text search over user and assistant messages in every session.
async fn search(
    cli: &Cli,
    query: String,
    session: Option<String>,
    limit: Option<u32>,
) -> Result<()> {
    let mut client = cli.connect().await?;
    let session_id = match session {
        Some(id) => Some(resolve_session(&mut client, Some(&id), false).await?.id),
        None => None,
    };
    let result: SearchResult = serde_json::from_value(
        client
            .request(
                "search.query",
                json!({ "query": query, "session_id": session_id, "limit": limit }),
            )
            .await?,
    )?;
    if result.hits.is_empty() {
        println!("no matches for {:?}", result.query);
        return Ok(());
    }
    println!("{} hit(s) for {:?}", result.hits.len(), result.query);
    for hit in result.hits {
        println!(
            "  {} ({}) e{} {}",
            hit.session_name,
            short_id(&hit.session_id),
            hit.seq,
            hit.role
        );
        println!("    {}", hit.snippet);
    }
    Ok(())
}

/// Fork a session: copy the (optionally truncated) history into a new
/// session. The workspace is shared, so files are *not* rolled back.
async fn fork(
    cli: &Cli,
    session: Option<String>,
    at: Option<u64>,
    name: Option<String>,
    shared: bool,
) -> Result<()> {
    let mut client = cli.connect().await?;
    let source = resolve_session(&mut client, session.as_deref(), false).await?;
    let result: SessionResult = serde_json::from_value(
        client
            .request(
                "session.fork",
                json!({
                    "session_id": source.id,
                    "at_seq": at,
                    "name": name,
                    "worktree": if shared { Some(false) } else { None },
                }),
            )
            .await?,
    )?;
    let fork = result.session;
    match at {
        Some(seq) => println!(
            "forked {} ({}) from {} before e{seq}",
            fork.name,
            short_id(&fork.id),
            source.name
        ),
        None => println!(
            "forked {} ({}) from {} (full history)",
            fork.name,
            short_id(&fork.id),
            source.name
        ),
    }
    if let Some(worktree) = &fork.worktree {
        println!("worktree {}", worktree.display());
        println!("note: files are shared through git only; use `eix rewind` to roll back");
    } else {
        println!("note: the workspace is shared; files were not rolled back");
    }
    Ok(())
}

async fn rewind(
    cli: &Cli,
    session: Option<String>,
    at: Option<u64>,
    name: Option<String>,
) -> Result<()> {
    let mut client = cli.connect().await?;
    let source = resolve_session(&mut client, session.as_deref(), false).await?;
    let result: RewindResult = serde_json::from_value(
        client
            .request(
                "session.rewind",
                json!({ "session_id": source.id, "at_seq": at, "name": name }),
            )
            .await?,
    )?;
    let fork = &result.session;
    match at {
        Some(seq) => println!(
            "rewound {} ({}) from {} before e{seq}",
            fork.name,
            short_id(&fork.id),
            source.name
        ),
        None => println!(
            "rewound {} ({}) from {} (full history; nothing to roll back)",
            fork.name,
            short_id(&fork.id),
            source.name
        ),
    }
    println!(
        "workspace: {} snapshot(s), {} file(s) restored, {} removed{}",
        result.snapshots,
        result.files_restored,
        result.files_removed,
        if result.files_missing > 0 {
            format!(", {} missing", result.files_missing)
        } else {
            String::new()
        }
    );
    if !result.restored {
        println!("warning: the workspace rollback is incomplete");
    }
    for warning in &result.warnings {
        println!("note: {warning}");
    }
    if let Some(worktree) = &fork.worktree {
        println!("worktree {}", worktree.display());
    }
    Ok(())
}

async fn delete_session(cli: &Cli, session: Option<String>) -> Result<()> {
    let mut client = cli.connect().await?;
    let session = resolve_session(&mut client, session.as_deref(), false).await?;
    client
        .request("session.delete", json!({ "session_id": session.id }))
        .await?;
    println!("deleted {} ({})", session.name, short_id(&session.id));
    Ok(())
}

async fn archive_session(cli: &Cli, session: Option<String>, undo: bool) -> Result<()> {
    let mut client = cli.connect().await?;
    let session = resolve_session(&mut client, session.as_deref(), false).await?;
    let result: SessionResult = serde_json::from_value(
        client
            .request(
                "session.archive",
                json!({ "session_id": session.id, "archived": !undo }),
            )
            .await?,
    )?;
    println!(
        "{} {} ({})",
        if undo { "unarchived" } else { "archived" },
        result.session.name,
        short_id(&result.session.id)
    );
    Ok(())
}

async fn export_session(cli: &Cli, session: Option<String>, output: Option<PathBuf>) -> Result<()> {
    let mut client = cli.connect().await?;
    let session = resolve_session(&mut client, session.as_deref(), false).await?;
    let result: eix_core::SessionExportResult = serde_json::from_value(
        client
            .request("session.export", json!({ "session_id": session.id }))
            .await?,
    )?;
    match output {
        Some(path) => {
            std::fs::write(&path, &result.json)
                .with_context(|| format!("cannot write {}", path.display()))?;
            println!("exported {} to {}", session.name, path.display());
        }
        None => println!("{}", result.json),
    }
    Ok(())
}

async fn import_session(
    cli: &Cli,
    file: PathBuf,
    name: Option<String>,
    cwd: Option<PathBuf>,
) -> Result<()> {
    let json = std::fs::read_to_string(&file)
        .with_context(|| format!("cannot read {}", file.display()))?;
    let mut client = cli.connect().await?;
    let result: SessionResult = serde_json::from_value(
        client
            .request(
                "session.import",
                json!({ "json": json, "name": name, "cwd": cwd }),
            )
            .await?,
    )?;
    let session = result.session;
    println!(
        "imported {} ({}) cwd {}",
        session.name,
        short_id(&session.id),
        session.cwd.display()
    );
    Ok(())
}

async fn undo(cli: &Cli, session: Option<String>, redo: bool) -> Result<()> {
    let mut client = cli.connect().await?;
    let session = resolve_session(&mut client, session.as_deref(), false).await?;
    let result = client
        .request(
            if redo { "session.redo" } else { "session.undo" },
            json!({ "session_id": session.id }),
        )
        .await?;
    println!(
        "{} {} event(s) in {}",
        if redo { "restored" } else { "hid" },
        result["changed"].as_u64().unwrap_or(0),
        session.name
    );
    Ok(())
}

async fn timeline(
    cli: &Cli,
    session: Option<String>,
    limit: Option<usize>,
    json_out: bool,
) -> Result<()> {
    let mut client = cli.connect().await?;
    let session = resolve_session(&mut client, session.as_deref(), false).await?;
    let result: eix_core::TimelineResult = serde_json::from_value(
        client
            .request(
                "session.timeline",
                json!({ "session_id": session.id, "limit": limit }),
            )
            .await?,
    )?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&result)?);
        return Ok(());
    }
    if result.items.is_empty() {
        println!("timeline is empty");
        return Ok(());
    }
    for item in result.items {
        let preview = item.preview.replace('\n', " ");
        println!(
            "e{:<4} {}  {:<17} {}",
            item.seq,
            format_ts(item.ts),
            item.kind,
            preview
        );
    }
    Ok(())
}

async fn worktree(cli: &Cli, cmd: WorktreeCmd) -> Result<()> {
    let mut client = cli.connect().await?;
    match cmd {
        WorktreeCmd::List => {
            let sessions = list_sessions(&mut client).await?;
            let mut any = false;
            for session in sessions.iter().filter(|session| session.has_worktree()) {
                any = true;
                let path = session.worktree.as_deref().unwrap_or(session.cwd.as_path());
                let state = if path.is_dir() { "ok" } else { "missing" };
                println!(
                    "{} {:<28} {:<10} {:<8} {}",
                    session.status.symbol(),
                    truncate(&session.name, 28),
                    short_id(&session.id),
                    state,
                    path.display()
                );
            }
            if !any {
                println!(
                    "no session worktrees — `eix new --worktree` or fork a session inside a git repo"
                );
            }
            Ok(())
        }
        WorktreeCmd::Remove { session, force } => {
            let target = resolve_session(&mut client, session.as_deref(), false).await?;
            let result: WorktreeRemoveResult = serde_json::from_value(
                client
                    .request(
                        "session.worktree.remove",
                        json!({ "session_id": target.id, "force": force }),
                    )
                    .await?,
            )?;
            if !result.removed {
                bail!("{}", result.message);
            }
            println!("{}", result.message);
            Ok(())
        }
    }
}

async fn nvim(cli: &Cli, session: Option<String>) -> Result<()> {
    // Start the local daemon first; on TCP the plugin connects to the remote
    // endpoint directly (and `ensure_daemon` is a no-op).
    ensure_daemon(&cli.endpoint, &cli.data_dir, cli.config_path.as_deref()).await?;
    crate::nvim_frontend::launch(cli.endpoint.clone(), &cli.data_dir, session).await
}

async fn processes(cli: &Cli, session: Option<String>) -> Result<()> {
    let mut client = cli.connect().await?;
    let session_id = match session {
        Some(needle) => Some(resolve_session(&mut client, Some(&needle), false).await?.id),
        None => None,
    };
    let list: ProcessList = serde_json::from_value(
        client
            .request("process.list", json!({ "session_id": session_id }))
            .await?,
    )?;
    if list.processes.is_empty() {
        println!("no background processes");
        return Ok(());
    }
    for process in &list.processes {
        let code = process
            .exit_code
            .map(|code| format!("{code}"))
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{}  {:<7} {:<4} {}  {}",
            process.short_id(),
            process.status.as_str(),
            code,
            process.command,
            process.log
        );
    }
    Ok(())
}

async fn tail(cli: &Cli, process: &str) -> Result<()> {
    let mut client = cli.connect().await?;
    let output: ProcessOutput = serde_json::from_value(
        client
            .request("process.output", json!({ "process_id": process }))
            .await?,
    )?;
    let code = output
        .exit_code
        .map(|code| format!(" · exit {code}"))
        .unwrap_or_default();
    println!(
        "process {} · {}{} · {} bytes{}",
        output.process_id,
        output.status.as_str(),
        code,
        output.bytes,
        if output.truncated { " · tail" } else { "" }
    );
    print!("{}", output.text);
    if !output.text.is_empty() && !output.text.ends_with('\n') {
        println!();
    }
    Ok(())
}

async fn kill(cli: &Cli, process: &str) -> Result<()> {
    let mut client = cli.connect().await?;
    let result: ProcessKillResult = serde_json::from_value(
        client
            .request("process.kill", json!({ "process_id": process }))
            .await?,
    )?;
    if result.killed {
        println!("killing {process}");
    } else {
        println!("process {process} is not running");
    }
    Ok(())
}

fn render_ledger(session: &Session, result: &ContextLedgerResult) {
    let remaining = result.budget_tokens.saturating_sub(result.total_tokens);
    println!(
        "context · {} · {} / {} tokens ({} left)",
        session.name, result.total_tokens, result.budget_tokens, remaining
    );
    if result.trimmed > 0 {
        println!(
            "  {} entries auto-trimmed to fit the budget",
            result.trimmed
        );
    }
    if result.entries.is_empty() {
        println!("  (empty)");
        return;
    }
    for sector in Sector::ALL {
        let entries: Vec<_> = result
            .entries
            .iter()
            .filter(|entry| entry.sector == sector)
            .collect();
        if entries.is_empty() {
            continue;
        }
        let usage = result
            .sectors
            .iter()
            .find(|usage| usage.sector == sector)
            .map(|usage| (usage.tokens, usage.budget_tokens))
            .unwrap_or((0, 0));
        println!("\n{}  ({} / {} tokens)", sector.as_str(), usage.0, usage.1);
        for entry in entries {
            let mut flags = Vec::new();
            if entry.pinned {
                flags.push("pin");
            }
            if entry.dropped {
                flags.push(if entry.auto { "cut" } else { "off" });
            }
            if entry.compacted {
                flags.push("sum");
            }
            let flag = format!("{:<7}", flags.join(","));
            println!(
                "  {:<18} {:>6}t  {} {}",
                entry.id, entry.tokens, flag, entry.preview
            );
        }
    }
}

fn parse_range(value: &str) -> Result<(u64, u64)> {
    let (from, to) = value
        .split_once(':')
        .or_else(|| value.split_once(".."))
        .ok_or_else(|| anyhow::anyhow!("--compact expects FROM:TO, e.g. 3:9"))?;
    let from: u64 = from
        .trim()
        .parse()
        .with_context(|| format!("invalid start sequence in {value:?}"))?;
    let to: u64 = to
        .trim()
        .parse()
        .with_context(|| format!("invalid end sequence in {value:?}"))?;
    if from == 0 || to < from {
        bail!("--compact needs 0 < FROM <= TO");
    }
    Ok((from, to))
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
            println!("model    {}", config.default_model());
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
            println!("input    native (built-in Vim editor)");
            println!(
                "snapshot {}",
                if config.tools.snapshots {
                    "on (before write_file/run_command)"
                } else {
                    "off"
                }
            );
            println!(
                "approval {} · {} rule(s) · network {}",
                config.tools.approval.as_str(),
                config.tools.rules.len(),
                if config.tools.sandbox_network {
                    "allowed"
                } else {
                    "blocked"
                }
            );
            let mut compat = Vec::new();
            if config.compations.claude_code {
                compat.push("claude");
            }
            if config.compations.opencode {
                compat.push("opencode");
            }
            if config.compations.codex_cli {
                compat.push("codex");
            }
            if config.compations.gemini_cli {
                compat.push("gemini");
            }
            if config.compations.pi {
                compat.push("pi");
            }
            println!(
                "compations {}",
                if compat.is_empty() {
                    "off (native .eix only)".to_string()
                } else {
                    compat.join(", ")
                }
            );
            let agents = std::env::current_dir()
                .ok()
                .map(|cwd| cwd.join("AGENTS.md"))
                .is_some_and(|path| path.exists());
            println!(
                "context  {} tokens · AGENTS.md {} · workspace summary {}",
                config.context.budget_tokens,
                if agents { "found" } else { "missing" },
                if config.context.workspace_summary {
                    "on"
                } else {
                    "off"
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
    list_sessions_including(client, false).await
}

async fn list_sessions_including(client: &mut Client, archived: bool) -> Result<Vec<Session>> {
    let list: SessionList = serde_json::from_value(
        client
            .request("session.list", json!({ "include_archived": archived }))
            .await?,
    )?;
    Ok(list.sessions)
}

async fn create_session(
    client: &mut Client,
    name: Option<String>,
    cwd: Option<PathBuf>,
    model: Option<String>,
    worktree: bool,
    permission: Option<String>,
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
                    "worktree": worktree,
                    "permission": permission,
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
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(session) = session_for_cwd(&sessions, &cwd) {
            return Ok(session.clone());
        }
    }
    if create {
        return create_session(client, None, None, None, false, None).await;
    }
    if let Some(session) = sessions.first() {
        return Ok(session.clone());
    }
    bail!("no sessions yet — run `eix new` first")
}

/// Prefer the most recently updated user session bound to this workspace.
fn session_for_cwd<'a>(sessions: &'a [Session], cwd: &Path) -> Option<&'a Session> {
    sessions
        .iter()
        .find(|session| !session.is_agent() && session.cwd_matches(cwd))
}

async fn attach_events(client: &mut Client, session_id: &str) -> Result<Vec<Event>> {
    let attach: AttachResult = serde_json::from_value(
        client
            .request("session.attach", json!({"session_id": session_id}))
            .await?,
    )?;
    Ok(attach.events)
}

async fn submit_prompt(
    client: &mut Client,
    session_id: &str,
    text: &str,
    images: &[String],
) -> Result<SubmitResult> {
    Ok(serde_json::from_value(
        client
            .request(
                "prompt.submit",
                json!({"session_id": session_id, "text": text, "images": images}),
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
            "input.required" => {
                if notification.params["session_id"] == session_id {
                    if wrote {
                        println!();
                        wrote = false;
                    }
                    let question = notification.params["question"]
                        .as_str()
                        .unwrap_or("the model has a question");
                    eprintln!("question: {question}");
                    if let Some(options) = notification.params["options"].as_array() {
                        for (index, option) in options.iter().enumerate() {
                            let label = option["label"].as_str().unwrap_or("");
                            let description = option["description"].as_str().unwrap_or("");
                            let recommended = if index == 0 { " (recommended)" } else { "" };
                            if description.is_empty() {
                                eprintln!("  {}. {label}{recommended}", index + 1);
                            } else {
                                eprintln!("  {}. {label} — {description}{recommended}", index + 1);
                            }
                        }
                    }
                    eprintln!("  run `eix answer <n>` with the option number");
                }
            }
            "patch.proposed" => {
                if notification.params["session_id"] == session_id {
                    if wrote {
                        println!();
                        wrote = false;
                    }
                    let summary = notification.params["summary"]
                        .as_str()
                        .unwrap_or("file change");
                    eprintln!("patch staged: {summary}");
                    if let Some(diff) = notification.params["diff"]
                        .as_str()
                        .filter(|diff| !diff.is_empty())
                    {
                        eprintln!("{diff}");
                    }
                    eprintln!("  run `eix approve` to apply or `eix deny` to reject");
                }
            }
            "patch.applied" | "patch.rejected" | "patch.conflict" => {
                if notification.params["session_id"] == session_id {
                    if wrote {
                        println!();
                        wrote = false;
                    }
                    let path = notification.params["path"].as_str().unwrap_or("file");
                    let label = match notification.method.as_str() {
                        "patch.applied" => "patch applied",
                        "patch.rejected" => "patch rejected",
                        _ => "patch conflict",
                    };
                    eprintln!("{label}: {path}");
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
            EventKind::PatchProposed => {
                if let Ok(payload) =
                    serde_json::from_value::<PatchProposedPayload>(event.payload.clone())
                {
                    println!(
                        "patch> {} [{}] (y/n in the TUI, or `eix approve/deny`)",
                        payload.summary, payload.patch_id
                    );
                    if let Some(diff) = payload.diff.as_deref().filter(|diff| !diff.is_empty()) {
                        print!("{diff}");
                    }
                }
            }
            EventKind::PatchApplied => {
                if let Ok(payload) =
                    serde_json::from_value::<PatchResultPayload>(event.payload.clone())
                {
                    let hunks = if payload.hunks.is_empty() {
                        String::new()
                    } else {
                        format!(" (hunks {:?})", payload.hunks)
                    };
                    println!("patch> applied {}{hunks}", payload.path);
                }
            }
            EventKind::PatchRejected => {
                if let Ok(payload) =
                    serde_json::from_value::<PatchResultPayload>(event.payload.clone())
                {
                    println!(
                        "patch> rejected {}{}",
                        payload.path,
                        payload
                            .message
                            .as_deref()
                            .map(|message| format!(" — {message}"))
                            .unwrap_or_default()
                    );
                }
            }
            EventKind::PatchConflict => {
                if let Ok(payload) =
                    serde_json::from_value::<PatchResultPayload>(event.payload.clone())
                {
                    println!(
                        "patch> conflict on {} — {}",
                        payload.path,
                        payload.message.as_deref().unwrap_or("file changed")
                    );
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
            EventKind::Snapshot => {
                if let Ok(payload) =
                    serde_json::from_value::<SnapshotPayload>(event.payload.clone())
                {
                    if payload.ok {
                        println!(
                            "snap> {} before {} — {} file(s), {} skipped",
                            payload.scope, payload.call_id, payload.files, payload.skipped
                        );
                    } else {
                        println!(
                            "snap> {} before {} failed: {}",
                            payload.scope,
                            payload.call_id,
                            payload.error.as_deref().unwrap_or("unknown error")
                        );
                    }
                }
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
            EventKind::ContextPinned => {
                if let Ok(payload) =
                    serde_json::from_value::<ContextPinPayload>(event.payload.clone())
                {
                    println!(
                        "ctx> {} {}",
                        if payload.pinned { "pin" } else { "unpin" },
                        payload.entry
                    );
                }
            }
            EventKind::ContextDropped => {
                if let Ok(payload) =
                    serde_json::from_value::<ContextDropPayload>(event.payload.clone())
                {
                    println!(
                        "ctx> {} {}",
                        if payload.dropped { "drop" } else { "restore" },
                        payload.entry
                    );
                }
            }
            EventKind::PermissionEvaluated => {
                if let Ok(payload) = serde_json::from_value::<eix_core::PermissionEvaluatedPayload>(
                    event.payload.clone(),
                ) {
                    println!(
                        "perm> {} {} · risk {} · {}{}",
                        payload.tool,
                        payload.action,
                        payload.risk,
                        payload.rule_source,
                        payload
                            .rule
                            .as_deref()
                            .map(|rule| format!(" · rule {rule}"))
                            .unwrap_or_default()
                    );
                }
            }
            EventKind::Permission => {
                if let Ok(payload) =
                    serde_json::from_value::<PermissionPayload>(event.payload.clone())
                {
                    println!(
                        "perm> {} -> {}{}",
                        payload.from.as_str(),
                        payload.to.as_str(),
                        if payload.temporary {
                            " (temporary)"
                        } else {
                            ""
                        }
                    );
                }
            }
            EventKind::ContextCompacted => {
                if let Ok(payload) =
                    serde_json::from_value::<ContextCompactedPayload>(event.payload.clone())
                {
                    println!(
                        "ctx> compact {}..={} ({} messages, ~{}t) {}",
                        payload.from_seq,
                        payload.to_seq,
                        payload.messages,
                        payload.tokens,
                        payload.summary
                    );
                }
            }
            EventKind::ContextCleared => {
                println!("ctx> cleared (e{} and earlier stay in the log)", event.seq);
            }
            EventKind::ProcessStarted => {
                if let Ok(payload) =
                    serde_json::from_value::<ProcessStartedPayload>(event.payload.clone())
                {
                    println!(
                        "proc> started {} (pid {}) {}",
                        payload.process_id, payload.pid, payload.command
                    );
                }
            }
            EventKind::ProcessExited => {
                if let Ok(payload) =
                    serde_json::from_value::<ProcessExitedPayload>(event.payload.clone())
                {
                    let code = payload
                        .exit_code
                        .map(|code| format!(" ({code})"))
                        .unwrap_or_default();
                    println!(
                        "proc> {} {}{} in {}ms",
                        payload.process_id, payload.status, code, payload.duration_ms
                    );
                }
            }
            EventKind::InputRequired => {
                if let Ok(payload) =
                    serde_json::from_value::<InputRequiredPayload>(event.payload.clone())
                {
                    println!(
                        "ask> {} (answer in the TUI, or `eix answer <n>`)",
                        payload.question
                    );
                    for (index, option) in payload.options.iter().enumerate() {
                        let recommended = if index == 0 { " (recommended)" } else { "" };
                        match option.description.as_deref() {
                            Some(description) if !description.is_empty() => println!(
                                "  {}. {} — {}{recommended}",
                                index + 1,
                                option.label,
                                description
                            ),
                            _ => println!("  {}. {}{recommended}", index + 1, option.label),
                        }
                    }
                }
            }
            EventKind::InputAnswered => {
                if let Ok(payload) =
                    serde_json::from_value::<InputAnsweredPayload>(event.payload.clone())
                {
                    println!("ask> answered: {}", payload.answer);
                }
            }
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
    send [--session ID] [--image PATH]... TEXT
                              send one prompt and stream the answer
    ls [--archived]            list sessions (--archived includes hidden ones)
    new [--name N] [--cwd P] [--model M] [--worktree] [--permission LEVEL]
                               create a session (--worktree: .worktrees/<name>)
    permission LEVEL [--session ID] [--temporary]
                               set read-only / workspace-write / full for a session
    log [--session ID]         print the persisted transcript
    rename [--session ID] NAME
    cancel [--session ID]      cancel the running turn
    approve [--session ID] [--hunks 0,2] [--reason TEXT]
                               approve the pending tool call (all or selected hunks)
    deny [--session ID] [--reason TEXT]
                               deny the pending tool call, optionally with a reason
    answer [--session ID] <option>
                               answer the pending question with a 1-based option
    approvals [--json]         list pending approvals across sessions
    status [--json]            daemon and session summary (`--json` for dwmblocks)
    pick [--query Q] [--limit N] [--json]
                               print sessions for rofi/dmenu/fzf (id<TAB>name<TAB>status<TAB>cwd)
    context [--session ID]     show the context ledger (tokens per sector)
    context --drop ENTRY       drop an entry (e.g. e12, workspace, instructions:AGENTS.md)
    context --undrop ENTRY     restore a dropped entry
    context --pin ENTRY        never auto-trim an entry (--unpin to undo)
    context --compact FROM:TO [--summary TEXT]
                               replace a range of history with a summary
    ps [--session ID]          list background processes
    tail PROCESS               print a process's log tail
    kill PROCESS               stop a background process
    fork [--session ID] [--at SEQ] [--name NAME] [--shared]
                               copy history into a new session (--shared: same files;
                               default: a git worktree when inside a repo)
    rewind [--session ID] [--at SEQ] [--name NAME]
                               fork and roll the workspace back to before SEQ
    delete <session>           delete a session and its event log
    archive <session> [--undo] hide a session from the default list (events kept)
    export [--session ID] [--output FILE]
                               export a session as JSON
    import <file> [--name N] [--cwd P]
                               import an exported session as a new session
    timeline [--session ID] [--limit N] [--json]
                               show user/assistant/tool events for navigation
    undo [--session ID]        hide the newest turn (redo restores it)
    redo [--session ID]        restore the most recent undo
    worktree [list | remove SESSION [--force]]
                               inspect or delete session worktrees
    nvim [--session ID]        open the Neovim frontend against this daemon
    search QUERY [--session ID] [--limit N]
                               full-text search over message history
    doctor                     diagnose config, paths and daemon
    daemon                     start/attach the daemon and print its status
    help                       show this help

OPTIONS:
    --socket <path>     daemon socket (default: $XDG_RUNTIME_DIR/eix.sock;
                        macOS: ~/Library/Caches/eix/eix.sock)
    --data-dir <path>   state directory (default: ~/.local/share/eix)
    --config <path>     config file (default: ~/.config/eix/config.toml)
    --tcp <host:port>   connect to a remote daemon instead of the socket
    --token <token>     remote token (or EIX_TCP / EIX_TCP_TOKEN)

The daemon starts automatically. With no command, `eix` opens the TUI; when stdout
is piped it falls back to `eix chat`. Prompt text after `--` is never parsed as a flag.

EXAMPLES:
    eix send 'explain the session model in DESIGN.md'
    echo 'review the diff' | eix send
    EIX_MODEL=qwen2.5-coder eix chat
    eix new --name auth --cwd ~/src/app --worktree
    eix permission read-only --session 4f2a9c11
    eix context
    eix context --compact 3:9 --summary 'we settled on a SQLite event log'
    eix ps
    eix tail 4f2a9c11
    eix fork --at 12
    eix rewind --at 12
    eix worktree list
    eix worktree remove 4f2a9c11 --force
    eix --tcp 127.0.0.1:7777 --token \"$EIX_TOKEN\" ls
    eix search 'sqlite event log'"
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
    fn parses_search_query_with_optional_flags() {
        match parse_command("search".to_string(), vec!["sqlite".into(), "events".into()]).unwrap() {
            Command::Search {
                query,
                session,
                limit,
            } => {
                assert_eq!(query, "sqlite events");
                assert!(session.is_none());
                assert!(limit.is_none());
            }
            _ => panic!("expected search"),
        }
        match parse_command(
            "search".to_string(),
            vec![
                "timeout".into(),
                "--session".into(),
                "abc".into(),
                "--limit".into(),
                "5".into(),
            ],
        )
        .unwrap()
        {
            Command::Search {
                query,
                session,
                limit,
            } => {
                assert_eq!(query, "timeout");
                assert_eq!(session.as_deref(), Some("abc"));
                assert_eq!(limit, Some(5));
            }
            _ => panic!("expected search"),
        }
        assert!(parse_command("search".to_string(), Vec::new()).is_err());
        assert!(parse_command(
            "search".to_string(),
            vec!["x".into(), "--limit".into(), "no".into()]
        )
        .is_err());
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
    #[test]
    fn resolves_a_session_for_the_current_workspace() {
        let cwd = std::env::current_dir().expect("cwd");
        let other = Session::new("other", "other", "/tmp/eix-other-workspace");
        let mut child = Session::new("child", "child", cwd.clone());
        child.origin = eix_core::SessionOrigin::Agent;
        let user = Session::new("user", "user", cwd.clone());
        let sessions = vec![other, child, user];
        assert_eq!(
            session_for_cwd(&sessions, &cwd).map(|session| session.id.as_str()),
            Some("user")
        );
        assert!(session_for_cwd(&sessions, Path::new("/tmp/eix-no-session")).is_none());
    }
}
