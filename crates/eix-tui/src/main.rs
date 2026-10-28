//! `eix`: CLI frontend and the first user-facing slice of the eix agent.
//!
//! It talks to `eixd` over the Unix socket and starts the daemon on demand.
//! Phase 0 commands are intentionally small: create/list sessions, send a
//! prompt, stream the answer, inspect the persisted transcript, cancel.

mod acp;
mod client;
mod tui;

use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use eix_core::config as core_config;
use eix_core::sandbox as core_sandbox;
use eix_core::{
    ApprovalListResult, ApprovalRequiredPayload, ApprovalResult, AssistantMessagePayload,
    AttachResult, CancelResult, Config, ContextCompactedPayload, ContextDropPayload,
    ContextPinPayload, Endpoint, Event, EventKind, HelloResult, InputAnsweredPayload,
    InputRequiredPayload, InputResult, IntegrationListResult, IntegrationState, ModePayload,
    PatchProposedPayload, PatchResultPayload, ProcessExitedPayload, ProcessKillResult, ProcessList,
    ProcessOutput, ProcessStartedPayload, RewindResult, Session, SessionList, SessionResult,
    SessionStatus, SnapshotPayload, SubmitResult, ToolCallPayload, ToolResultPayload,
    UserMessagePayload, WorktreeRemoveResult,
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
        Command::Mcp(cmd) => mcp_command(&cli, cmd).await,
        Command::Plugin(cmd) => plugin_command(&cli, cmd).await,
        Command::Auth(cmd) => auth_command(&cli, cmd).await,
        Command::Model(cmd) => model_command(&cli, cmd).await,
        Command::Pair(cmd) => pair_command(&cli, cmd).await,
        Command::Acp => {
            acp::run(
                cli.endpoint.clone(),
                cli.data_dir.clone(),
                cli.config_path.clone(),
            )
            .await
        }
        Command::Sessions { archived } => sessions(&cli, archived).await,
        Command::New {
            name,
            cwd,
            model,
            worktree,
            mode,
        } => new_session(&cli, name, cwd, model, worktree, mode).await,
        Command::Mode {
            session,
            mode,
            temporary,
        } => set_mode(&cli, session, mode, temporary).await,
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
        Command::Answer {
            session,
            option,
            answers,
        } => answer(&cli, session, option, answers).await,
        Command::Approvals { json } => approvals(&cli, json).await,
        Command::Status { json } => status(&cli, json).await,
        Command::Pick { query, limit, json } => pick(&cli, query, limit, json).await,
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
        Command::Ready { session } => ready(&cli, session).await,
        Command::Integrate { action, session } => integrate(&cli, action, session).await,
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
        /// `Some(true)` creates a worktree now, `Some(false)` opts out, and
        /// `None` creates one lazily from the first prompt.
        worktree: Option<bool>,
        /// Initial session execution mode.
        mode: Option<String>,
    },
    Mode {
        session: Option<String>,
        /// `plan` / `build` / `auto` / `full`.
        mode: String,
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
        /// Legacy 1-based option number for a single-question request.
        option: Option<usize>,
        /// `id=value` answers for one or more questions.
        answers: std::collections::BTreeMap<String, Vec<String>>,
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
    Ready {
        session: Option<String>,
    },
    Integrate {
        action: String,
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
    Mcp(McpCmd),
    Plugin(PluginCmd),
    Auth(AuthCmd),
    Model(ModelCmd),
    Pair(PairCmd),
    Acp,
    Doctor,
    Daemon,
    Help,
}

#[derive(Clone)]
enum McpCmd {
    /// Run the OAuth browser flow for a remote server.
    Auth { server: String, no_browser: bool },
    /// Forget a stored OAuth token.
    Logout { server: String },
}

#[derive(Clone)]
enum PluginCmd {
    /// Write `eix-plugin.sums` for a package directory.
    Pack { directory: PathBuf },
    /// Verify checksums and the detached signature.
    Verify {
        directory: PathBuf,
        allow_unsigned: bool,
    },
    /// Verify and copy a package under the plugin root.
    Install {
        directory: PathBuf,
        allow_unsigned: bool,
        force: bool,
    },
    /// Add or replace a trusted publisher key.
    Trust {
        publisher: String,
        public_key: String,
    },
    /// Remove a trusted publisher key.
    Untrust { publisher: String },
}

#[derive(Clone)]
enum AuthCmd {
    /// Store an API key, or start the provider's OAuth flow.
    Login {
        provider: String,
        api_key: Option<String>,
        stdin: bool,
        no_browser: bool,
    },
    /// Show configured providers and whether a credential is available.
    List { json: bool },
    /// Remove the stored credential for a provider.
    Logout { provider: String },
}

#[derive(Clone)]
enum ModelCmd {
    /// Show the daemon's current default model and provider auth state.
    Show { json: bool, all: bool },
    /// Switch the daemon's runtime default model.
    Use { model: Option<String> },
}

#[derive(Clone)]
enum PairCmd {
    /// Mint a short-lived one-time code for a new client.
    Create { name: Option<String> },
    /// Show paired clients.
    List { json: bool },
    /// Revoke a client token by id.
    Revoke { id: String },
}

#[derive(Clone)]
enum WorktreeCmd {
    List,
    Remove {
        session: Option<String>,
        force: bool,
    },
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
        "acp" => Ok(Command::Acp),
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
            let explicit_worktree = take_switch(&mut args, "--worktree");
            let shared = take_switch(&mut args, "--shared");
            if explicit_worktree && shared {
                bail!("--worktree and --shared are mutually exclusive");
            }
            let worktree = if explicit_worktree {
                Some(true)
            } else if shared {
                Some(false)
            } else {
                None
            };
            let mode = take_flag(&mut args, "--mode")?;
            let name = name.or_else(|| {
                let joined = args.join(" ").trim().to_string();
                (!joined.is_empty()).then_some(joined)
            });
            Ok(Command::New {
                name,
                cwd,
                model,
                worktree,
                mode,
            })
        }
        "mode" => {
            let session = take_flag(&mut args, "--session")?;
            let temporary = take_switch(&mut args, "--temporary") || take_switch(&mut args, "-t");
            let mode = args.first().cloned().ok_or_else(|| {
                anyhow::anyhow!(
                    "usage: eix mode <plan|build|auto|full> [--session ID] [--temporary]"
                )
            })?;
            Ok(Command::Mode {
                session,
                mode,
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
            if args.is_empty() {
                bail!("usage: eix answer [--session ID] <option> | <id>=<value>...");
            }
            let mut option = None;
            let mut answers = std::collections::BTreeMap::new();
            if args.len() == 1 {
                if let Ok(number) = args[0].parse::<usize>() {
                    if number == 0 {
                        bail!("option numbers start at 1");
                    }
                    option = Some(number);
                }
            }
            if option.is_none() {
                for argument in &args {
                    let (id, value) = argument
                        .split_once('=')
                        .with_context(|| format!("expected id=value, got {argument:?}"))?;
                    if id.trim().is_empty() || value.trim().is_empty() {
                        bail!("answers need a non-empty id and value");
                    }
                    answers
                        .entry(id.trim().to_string())
                        .or_insert_with(Vec::new)
                        .push(value.to_string());
                }
            }
            Ok(Command::Answer {
                session,
                option,
                answers,
            })
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
        "ready" => {
            let session = take_flag(&mut args, "--session")?;
            let session = session.or_else(|| args.first().cloned());
            Ok(Command::Ready { session })
        }
        "integrate" | "integration" => {
            let session = take_flag(&mut args, "--session")?;
            let action = args.first().cloned().unwrap_or_else(|| "status".into());
            if !args.is_empty() {
                args.remove(0);
            }
            let session = session.or_else(|| args.first().cloned());
            if !matches!(action.as_str(), "status" | "continue" | "retry") {
                bail!("usage: eix integrate [status | continue SESSION] (got {action:?})");
            }
            Ok(Command::Integrate { action, session })
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
        "auth" => {
            let sub = if args.first().is_some_and(|arg| !arg.starts_with('-')) {
                args.remove(0)
            } else {
                "list".to_string()
            };
            let api_key = take_flag(&mut args, "--api-key")?;
            let stdin = take_switch(&mut args, "--stdin");
            let no_browser = take_switch(&mut args, "--no-browser");
            match sub.as_str() {
                "login" | "add" => {
                    let provider = args
                        .first()
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("usage: eix auth login <provider>"))?;
                    Ok(Command::Auth(AuthCmd::Login {
                        provider,
                        api_key,
                        stdin,
                        no_browser,
                    }))
                }
                "list" | "ls" | "status" => {
                    let json = take_switch(&mut args, "--json");
                    Ok(Command::Auth(AuthCmd::List { json }))
                }
                "logout" | "remove" | "rm" => {
                    let provider = args
                        .first()
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("usage: eix auth logout <provider>"))?;
                    Ok(Command::Auth(AuthCmd::Logout { provider }))
                }
                // `eix auth openai` is shorthand for `eix auth login openai`.
                other => Ok(Command::Auth(AuthCmd::Login {
                    provider: other.to_string(),
                    api_key,
                    stdin,
                    no_browser,
                })),
            }
        }
        "model" => {
            let json = take_switch(&mut args, "--json");
            let all = take_switch(&mut args, "--all");
            let first = args.first().cloned();
            match first.as_deref() {
                None | Some("show") | Some("list") | Some("ls") => {
                    if !args.is_empty() {
                        args.remove(0);
                    }
                    Ok(Command::Model(ModelCmd::Show { json, all }))
                }
                Some("default") | Some("reset") => {
                    Ok(Command::Model(ModelCmd::Use { model: None }))
                }
                Some("use") | Some("set") => {
                    args.remove(0);
                    let model = args.first().cloned();
                    Ok(Command::Model(ModelCmd::Use { model }))
                }
                Some(other) if !other.starts_with('-') => Ok(Command::Model(ModelCmd::Use {
                    model: Some(other.to_string()),
                })),
                Some(other) => bail!(
                    "usage: eix model [show | use <provider/model> | default] (got {other:?})"
                ),
            }
        }
        "mcp" => {
            let sub = args
                .first()
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("usage: eix mcp auth <server> [--no-browser]"))?;
            args.remove(0);
            match sub.as_str() {
                "auth" | "login" => {
                    let no_browser = take_switch(&mut args, "--no-browser");
                    let server = args.first().cloned().ok_or_else(|| {
                        anyhow::anyhow!("usage: eix mcp auth <server> [--no-browser]")
                    })?;
                    Ok(Command::Mcp(McpCmd::Auth { server, no_browser }))
                }
                "logout" => {
                    let server = args
                        .first()
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("usage: eix mcp logout <server>"))?;
                    Ok(Command::Mcp(McpCmd::Logout { server }))
                }
                other => bail!("usage: eix mcp auth <server> | logout <server> (got {other:?})"),
            }
        }
        "plugin" => {
            let sub = args.first().cloned().ok_or_else(|| {
                anyhow::anyhow!("usage: eix plugin <pack|verify|install|trust|untrust>")
            })?;
            args.remove(0);
            match sub.as_str() {
                "pack" => {
                    let directory = args
                        .first()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from("."));
                    Ok(Command::Plugin(PluginCmd::Pack { directory }))
                }
                "verify" => {
                    let allow_unsigned = take_switch(&mut args, "--allow-unsigned");
                    let directory = args
                        .first()
                        .map(PathBuf::from)
                        .unwrap_or_else(|| PathBuf::from("."));
                    Ok(Command::Plugin(PluginCmd::Verify {
                        directory,
                        allow_unsigned,
                    }))
                }
                "install" => {
                    let allow_unsigned = take_switch(&mut args, "--allow-unsigned");
                    let force = take_switch(&mut args, "--force");
                    let directory = args.first().map(PathBuf::from).ok_or_else(|| {
                        anyhow::anyhow!(
                            "usage: eix plugin install <directory> [--force] [--allow-unsigned]"
                        )
                    })?;
                    Ok(Command::Plugin(PluginCmd::Install {
                        directory,
                        allow_unsigned,
                        force,
                    }))
                }
                "trust" => {
                    let publisher = args.first().cloned().ok_or_else(|| {
                        anyhow::anyhow!("usage: eix plugin trust <publisher> <base64-public-key>")
                    })?;
                    let public_key = args.get(1).cloned().ok_or_else(|| {
                        anyhow::anyhow!("usage: eix plugin trust <publisher> <base64-public-key>")
                    })?;
                    Ok(Command::Plugin(PluginCmd::Trust {
                        publisher,
                        public_key,
                    }))
                }
                "untrust" => {
                    let publisher = args
                        .first()
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("usage: eix plugin untrust <publisher>"))?;
                    Ok(Command::Plugin(PluginCmd::Untrust { publisher }))
                }
                other => bail!(
                    "unknown plugin command {other:?}; use pack, verify, install, trust or untrust"
                ),
            }
        }
        "daemon" | "up" => Ok(Command::Daemon),
        "pair" => {
            let sub = if args.first().is_some_and(|arg| !arg.starts_with('-')) {
                args.remove(0)
            } else {
                "create".to_string()
            };
            match sub.as_str() {
                "create" | "new" | "code" => {
                    let name = take_flag(&mut args, "--name")?;
                    Ok(Command::Pair(PairCmd::Create { name }))
                }
                "list" | "ls" => {
                    let json = take_switch(&mut args, "--json");
                    Ok(Command::Pair(PairCmd::List { json }))
                }
                "revoke" | "rm" => {
                    let id = args
                        .first()
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("usage: eix pair revoke <client-id>"))?;
                    Ok(Command::Pair(PairCmd::Revoke { id }))
                }
                other => {
                    bail!("usage: eix pair [--name LABEL] | list | revoke <id> (got {other:?})")
                }
            }
        }
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
            session.mode.as_str(),
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
    worktree: Option<bool>,
    mode: Option<String>,
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
    let mode = mode.or(defaults.mode);
    let mut client = cli.connect().await?;
    let session = create_session(&mut client, name, cwd, model, worktree, mode).await?;
    println!(
        "{} {}  {}  [{}]",
        session.status.symbol(),
        session.name,
        short_id(&session.id),
        session.mode.as_str()
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
                session = create_session(&mut client, None, None, None, None, None).await?;
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

async fn set_mode(cli: &Cli, session: Option<String>, mode: String, temporary: bool) -> Result<()> {
    let mut client = cli.connect().await?;
    let session = resolve_session(&mut client, session.as_deref(), false).await?;
    let result: SessionResult = serde_json::from_value(
        client
            .request(
                "session.mode",
                json!({
                    "session_id": session.id,
                    "mode": mode,
                    "temporary": temporary,
                }),
            )
            .await?,
    )?;
    println!(
        "{} is now {} ({})",
        result.session.name,
        result.session.mode.as_str(),
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

async fn answer(
    cli: &Cli,
    session: Option<String>,
    option: Option<usize>,
    answers: std::collections::BTreeMap<String, Vec<String>>,
) -> Result<()> {
    let mut client = cli.connect().await?;
    let session = resolve_session(&mut client, session.as_deref(), false).await?;
    let result: InputResult = serde_json::from_value(
        client
            .request(
                "input.respond",
                json!({
                    "session_id": session.id,
                    "input_id": "",
                    "answers": answers,
                    "option_index": option.map(|option| option - 1),
                }),
            )
            .await?,
    )?;
    if result.accepted {
        match option {
            Some(option) => println!("answered {} (option {option})", session.name),
            None => println!("answered {}", session.name),
        }
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
                "mode": session.mode.as_str(),
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
                    "mode": session.mode.as_str(),
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
        println!("{}  {}", format_ts(item.ts), preview);
    }
    Ok(())
}

async fn ready(cli: &Cli, session: Option<String>) -> Result<()> {
    let mut client = cli.connect().await?;
    let target = match session {
        Some(needle) => resolve_session(&mut client, Some(&needle), false).await?,
        None => current_worktree_session(&mut client).await?,
    };
    let state: IntegrationState = serde_json::from_value(
        client
            .request("session.ready", json!({ "session_id": target.id }))
            .await?,
    )?;
    println!(
        "{} ready · {} · base {}",
        state.status.symbol(),
        truncate(&target.name, 28),
        state.base_branch.as_deref().unwrap_or("auto")
    );
    Ok(())
}

/// Resolve the session that owns the current directory. An explicit session id
/// is handled by the caller; this never falls back to an unrelated session.
async fn current_worktree_session(client: &mut Client) -> Result<Session> {
    let sessions = list_sessions(client).await?;
    let cwd = std::env::current_dir()
        .context("cannot determine current directory")?
        .canonicalize()
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_default());
    // Prefer the deepest session workspace containing the caller, so running
    // from a subdirectory of a worktree still finds that worktree.
    if let Some(session) = sessions
        .iter()
        .filter(|session| {
            !session.is_agent() && session.has_worktree() && cwd.starts_with(&session.cwd)
        })
        .max_by_key(|session| session.cwd.components().count())
    {
        return Ok(session.clone());
    }
    bail!(
        "no session is bound to {}; run `eix ready --session ID`",
        cwd.display()
    )
}

async fn integrate(cli: &Cli, action: String, session: Option<String>) -> Result<()> {
    let mut client = cli.connect().await?;
    match action.as_str() {
        "status" => {
            let filter = match session {
                Some(needle) => Some(resolve_session(&mut client, Some(&needle), false).await?.id),
                None => None,
            };
            let result: IntegrationListResult =
                serde_json::from_value(client.request("integration.list", json!({})).await?)?;
            let integrations: Vec<_> = result
                .integrations
                .into_iter()
                .filter(|state| {
                    filter
                        .as_deref()
                        .is_none_or(|session_id| state.session_id == session_id)
                })
                .collect();
            if integrations.is_empty() {
                println!("no worktree integrations");
                return Ok(());
            }
            let sessions = list_sessions(&mut client).await?;
            for state in integrations {
                let name = sessions
                    .iter()
                    .find(|session| session.id == state.session_id)
                    .map(|session| session.name.as_str())
                    .unwrap_or("-");
                let detail = state
                    .error
                    .as_deref()
                    .map(|error| format!(" · {}", truncate(&error.replace('\n', " "), 80)))
                    .unwrap_or_default();
                println!(
                    "{} {:<28} {:<8} {:<9}{}",
                    state.status.symbol(),
                    truncate(name, 28),
                    short_id(&state.session_id),
                    state.status.as_str(),
                    detail
                );
            }
            Ok(())
        }
        "continue" | "retry" => {
            let target = match session {
                Some(needle) => resolve_session(&mut client, Some(&needle), false).await?,
                None => current_worktree_session(&mut client).await?,
            };
            let state: IntegrationState = serde_json::from_value(
                client
                    .request("integration.continue", json!({ "session_id": target.id }))
                    .await?,
            )?;
            println!(
                "{} requeued · {}",
                state.status.symbol(),
                state.status.as_str()
            );
            Ok(())
        }
        _ => unreachable!(),
    }
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

/// Package, verify, install and trust plugins. Installation never edits the
/// config; it prints the exact `[plugins.<name>]` snippet to add.
async fn plugin_command(_cli: &Cli, cmd: PluginCmd) -> Result<()> {
    let config_dir = core_config::config_dir();
    let plugins_root = config_dir.join("plugins");
    let trust_path = config_dir.join("plugin-trust.toml");
    match cmd {
        PluginCmd::Pack { directory } => {
            let entries = eix_core::pack_plugin(&directory)
                .with_context(|| format!("cannot pack {}", directory.display()))?;
            println!(
                "packed {} file(s); wrote {}",
                entries.len(),
                directory
                    .join(eix_core::plugin_package::DEFAULT_CHECKSUMS)
                    .display()
            );
            println!("sign the checksum file and write the base64 signature to eix-plugin.sig");
            Ok(())
        }
        PluginCmd::Verify {
            directory,
            allow_unsigned,
        } => {
            let manifest = eix_core::PluginManifest::load(&directory.join("eix-plugin.toml"))?;
            let trust = eix_core::PluginTrustStore::load(&trust_path)?;
            eix_core::verify_plugin(&directory, &manifest, &trust, allow_unsigned)?;
            println!("verified {} ({})", manifest.name, manifest.version);
            Ok(())
        }
        PluginCmd::Install {
            directory,
            allow_unsigned,
            force,
        } => {
            let trust = eix_core::PluginTrustStore::load(&trust_path)?;
            let installed =
                eix_core::install_plugin(&directory, &plugins_root, &trust, allow_unsigned, force)?;
            println!(
                "installed {} ({}) to {}",
                installed.name,
                installed.manifest.version,
                installed.path.display()
            );
            println!();
            println!("Add this to your global config and reload the daemon:");
            println!();
            println!("[plugins.{}]", installed.name);
            println!(
                "manifest = {:?}",
                installed.path.join("eix-plugin.toml").display().to_string()
            );
            Ok(())
        }
        PluginCmd::Trust {
            publisher,
            public_key,
        } => {
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(public_key.trim())
                .context("public key must be base64")?;
            if decoded.len() != 32 {
                bail!("an Ed25519 public key is 32 bytes, got {}", decoded.len());
            }
            let mut trust = eix_core::PluginTrustStore::load(&trust_path)?;
            trust.insert(publisher.clone(), public_key.trim().to_string());
            trust.save(&trust_path)?;
            println!("trusted publisher {publisher} ({})", trust_path.display());
            Ok(())
        }
        PluginCmd::Untrust { publisher } => {
            let mut trust = eix_core::PluginTrustStore::load(&trust_path)?;
            let removed = trust.keys.remove(&publisher).is_some();
            trust.save(&trust_path)?;
            println!(
                "{} publisher {publisher}",
                if removed { "removed" } else { "unknown" }
            );
            Ok(())
        }
    }
}

async fn mcp_command(cli: &Cli, cmd: McpCmd) -> Result<()> {
    let store = eixd::oauth::Store::new(cli.data_dir.join("oauth").join("mcp"));
    match cmd {
        McpCmd::Auth { server, no_browser } => {
            let config = Config::load(cli.config_path.as_deref())?;
            let spec = config.mcp.servers.get(&server).with_context(|| {
                format!("mcp server {server:?} is not configured under mcp.servers")
            })?;
            if !spec.is_remote() {
                bail!(
                    "mcp server {server:?} uses stdio; OAuth applies to remote servers with `url`"
                );
            }
            let token = store
                .authorize(&server, spec, eixd::oauth::AuthOptions { no_browser })
                .await?;
            println!(
                "authorized {server}; token saved to {}",
                store.token_path(&server).display()
            );
            if token.refresh_token.is_none() {
                println!(
                    "note: no refresh token was issued; rerun `eix mcp auth {server}` when it expires"
                );
            }
            reload_mcp(cli).await;
            Ok(())
        }
        McpCmd::Logout { server } => {
            store.remove(&server)?;
            println!("removed the stored OAuth token for {server}");
            reload_mcp(cli).await;
            Ok(())
        }
    }
}

/// Store an API key or run OAuth for one provider, and inspect credentials.
/// The daemon reads the credential files on demand, so no reload is required.
async fn auth_command(cli: &Cli, cmd: AuthCmd) -> Result<()> {
    let store = eixd::auth::Store::under_data_dir(&cli.data_dir);
    let config = Config::load(cli.config_path.as_deref())?;
    match cmd {
        AuthCmd::Login {
            provider,
            api_key,
            stdin,
            no_browser,
        } => {
            let spec = config.providers.get(&provider).with_context(|| {
                format!("provider {provider:?} is not configured under [providers]")
            })?;
            let key = match api_key {
                Some(key) => Some(key),
                None if stdin || !io::stdin().is_terminal() => Some(read_stdin_secret()?),
                None if spec.oauth.is_some() => None,
                None => Some(prompt_secret(&format!("API key for {provider}: "))?),
            };
            if let Some(key) = key {
                store.save_api_key(&provider, &key)?;
                println!(
                    "saved the API key for {provider} to {}",
                    store.token_path(&provider).display()
                );
                return Ok(());
            }
            let oauth = spec.oauth.as_ref().with_context(|| {
                format!(
                    "provider {provider:?} has no [providers.{provider}.oauth] block; \
                     pass --api-key or --stdin"
                )
            })?;
            let credential = store.authorize_oauth(&provider, oauth, no_browser).await?;
            println!(
                "authorized {provider} ({}); saved to {}",
                credential.summary(),
                store.token_path(&provider).display()
            );
            if credential.refresh_token.is_none() {
                println!(
                    "note: no refresh token was issued; run `eix auth login {provider}` again \
                     when it expires"
                );
            }
            Ok(())
        }
        AuthCmd::List { json } => {
            let mut rows = Vec::new();
            for (key, spec) in &config.providers {
                let (authenticated, source) = credential_source(&store, key, spec);
                rows.push(json!({
                    "provider": key,
                    "name": spec.name.as_deref().unwrap_or(key),
                    "kind": spec.kind.label(),
                    "model": spec.model.clone(),
                    "default": spec.default,
                    "authenticated": authenticated,
                    "source": source,
                }));
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&rows)?);
                return Ok(());
            }
            println!(
                "{:<16} {:<10} {:<9} {:<28} AUTH",
                "PROVIDER", "KIND", "DEFAULT", "MODEL"
            );
            for row in &rows {
                println!(
                    "{:<16} {:<10} {:<9} {:<28} {}",
                    row["provider"].as_str().unwrap_or_default(),
                    row["kind"].as_str().unwrap_or_default(),
                    if row["default"].as_bool().unwrap_or(false) {
                        "yes"
                    } else {
                        "-"
                    },
                    truncate(row["model"].as_str().unwrap_or_default(), 28),
                    row["source"].as_str().unwrap_or_default(),
                );
            }
            Ok(())
        }
        AuthCmd::Logout { provider } => {
            if store.remove(&provider)? {
                println!("removed the stored credential for {provider}");
            } else {
                println!("no stored credential for {provider}");
            }
            Ok(())
        }
    }
}

/// Same precedence as the daemon: inline key, environment, then the store.
fn credential_source(
    store: &eixd::auth::Store,
    key: &str,
    spec: &eix_core::ProviderConfig,
) -> (bool, String) {
    if spec
        .api_key
        .as_deref()
        .is_some_and(|key| !key.trim().is_empty())
    {
        return (true, "config".to_string());
    }
    if !spec.api_key_env.trim().is_empty()
        && std::env::var(spec.api_key_env.trim())
            .ok()
            .is_some_and(|value| !value.trim().is_empty())
    {
        return (true, format!("env:{}", spec.api_key_env.trim()));
    }
    match store.load(key) {
        Ok(Some(credential)) if credential.available() => (
            true,
            match credential.kind {
                eixd::auth::CredentialKind::ApiKey => "store:api-key",
                eixd::auth::CredentialKind::OAuth => "store:oauth",
            }
            .to_string(),
        ),
        Ok(Some(credential)) => (false, format!("store:{} (unusable)", credential.summary())),
        Ok(None) => (false, "missing".to_string()),
        Err(err) => (false, format!("store error: {err}")),
    }
}

fn read_stdin_secret() -> Result<String> {
    let mut text = String::new();
    io::stdin()
        .read_to_string(&mut text)
        .context("cannot read the credential from stdin")?;
    let text = text.trim().to_string();
    if text.is_empty() {
        bail!("no credential was provided on stdin");
    }
    Ok(text)
}

fn prompt_secret(prompt: &str) -> Result<String> {
    print!("{prompt}");
    io::stdout().flush().ok();
    #[cfg(unix)]
    let _echo = EchoGuard::disable();
    let mut line = String::new();
    let read = io::stdin().read_line(&mut line);
    #[cfg(unix)]
    drop(_echo);
    println!();
    read.context("cannot read the credential")?;
    let line = line.trim().to_string();
    if line.is_empty() {
        bail!("no credential was provided");
    }
    Ok(line)
}

/// Restore terminal echo when `eix auth login` reads an API key interactively.
#[cfg(unix)]
struct EchoGuard {
    original: libc::termios,
    active: bool,
}

#[cfg(unix)]
impl EchoGuard {
    fn disable() -> Self {
        // SAFETY: fd 0 is the terminal we just prompted on; `termios` is a
        // plain C struct and `tcgetattr`/`tcsetattr` only touch it.
        unsafe {
            let mut term = std::mem::zeroed::<libc::termios>();
            if libc::tcgetattr(0, &mut term) != 0 {
                return Self {
                    original: term,
                    active: false,
                };
            }
            let original = term;
            term.c_lflag &= !libc::ECHO;
            Self {
                original,
                active: libc::tcsetattr(0, libc::TCSANOW, &term) == 0,
            }
        }
    }
}

#[cfg(unix)]
impl Drop for EchoGuard {
    fn drop(&mut self) {
        if self.active {
            // SAFETY: restoring the exact settings captured in `disable`.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &self.original);
            }
        }
    }
}

/// Inspect or switch the daemon's runtime default model. Switching does not
/// persist to config; sessions with a pinned model keep their choice.
async fn model_command(cli: &Cli, cmd: ModelCmd) -> Result<()> {
    let mut client = cli.connect().await?;
    match cmd {
        ModelCmd::Show { json, all } => {
            let value = client.request("model.list", Value::Null).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&value)?);
                return Ok(());
            }
            let result: eix_core::ModelListResult = serde_json::from_value(value)?;
            println!("default  {}", result.default_model);
            if !result.providers.is_empty() {
                println!();
                println!("{:<16} {:<10} AUTH", "PROVIDER", "KIND");
                for provider in &result.providers {
                    println!(
                        "{:<16} {:<10} {}{}",
                        provider.key,
                        provider.kind,
                        if provider.authenticated {
                            ""
                        } else {
                            "missing · "
                        },
                        provider.source,
                    );
                }
            }
            if all {
                println!();
                println!("{:<44} {:<16} CONTEXT", "MODEL", "PROVIDER");
                for model in &result.models {
                    println!(
                        "{:<44} {:<16} {}",
                        truncate(&model.id, 44),
                        truncate(&model.provider, 16),
                        model
                            .context_tokens
                            .map(|tokens| tokens.to_string())
                            .unwrap_or_else(|| "-".to_string()),
                    );
                }
            }
            Ok(())
        }
        ModelCmd::Use { model } => {
            let value = client
                .request("model.use", json!({ "model": model }))
                .await?;
            let result: eix_core::ModelUseResult = serde_json::from_value(value)?;
            println!("default model: {}", result.model);
            Ok(())
        }
    }
}

/// Ask an already-running daemon to reconnect MCP servers. Auth works without
/// a daemon, so a missing daemon is not an error.
async fn reload_mcp(cli: &Cli) {
    if let Ok(mut client) = Client::connect(&cli.endpoint).await {
        let _ = client.request("mcp.reload", json!({})).await;
    }
}

/// Create, list and revoke paired remote clients. Pairing codes are minted by
/// the local daemon and redeemed over HTTP (or another transport) for a durable
/// token; only the token's hash is stored.
async fn pair_command(cli: &Cli, cmd: PairCmd) -> Result<()> {
    let mut client = cli.connect().await?;
    match cmd {
        PairCmd::Create { name } => {
            let params = match name {
                Some(name) => json!({ "name": name }),
                None => json!({}),
            };
            let result = client.request("pair.create", params).await?;
            let code = result["code"].as_str().unwrap_or_default();
            let expires_at = result["expires_at"].as_i64().unwrap_or_default();
            let expires = chrono::DateTime::from_timestamp_millis(expires_at)
                .map(|dt| {
                    dt.with_timezone(&chrono::Local)
                        .format("%Y-%m-%d %H:%M:%S")
                        .to_string()
                })
                .unwrap_or_else(|| expires_at.to_string());
            println!("pairing code: {code}");
            println!("expires: {expires} (5 minutes, single use)");
            match result["http"].as_str() {
                Some(url) => println!(
                    "redeem:\n  curl -s {url} -H 'content-type: application/json' \\\n    -d '{{\"code\":\"{code}\"}}'"
                ),
                None => println!(
                    "enable `server.http` to redeem this code over HTTP; the code can also be \
                     submitted to POST /pair on any HTTP listener"
                ),
            }
            Ok(())
        }
        PairCmd::List { json } => {
            let result = client.request("pair.list", json!({})).await?;
            let clients = result["clients"].as_array().cloned().unwrap_or_default();
            if json {
                println!("{}", serde_json::to_string_pretty(&clients)?);
                return Ok(());
            }
            if clients.is_empty() {
                println!("no paired clients");
                return Ok(());
            }
            println!(
                "{:<16} {:<24} {:<11} {:<11} STATUS",
                "ID", "NAME", "CREATED", "LAST USED"
            );
            for entry in clients {
                let last_used = entry["last_used_at"]
                    .as_i64()
                    .map(format_ts)
                    .unwrap_or_else(|| "-".to_string());
                println!(
                    "{:<16} {:<24} {:<11} {:<11} {}",
                    entry["id"].as_str().unwrap_or_default(),
                    truncate(entry["name"].as_str().unwrap_or_default(), 24),
                    format_ts(entry["created_at"].as_i64().unwrap_or_default()),
                    last_used,
                    if entry["revoked"].as_bool().unwrap_or(false) {
                        "revoked"
                    } else {
                        "active"
                    },
                );
            }
            Ok(())
        }
        PairCmd::Revoke { id } => {
            let result = client.request("pair.revoke", json!({ "id": id })).await?;
            if result["revoked"].as_bool().unwrap_or(false) {
                println!("revoked {id}");
            } else {
                bail!("no active paired client with id {id}");
            }
            Ok(())
        }
    }
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
                core_sandbox::status()
            );
            println!("input    native (built-in Vim editor)");
            println!(
                "snapshot {}",
                if config.tools.snapshots {
                    "on (before apply_patch/exec_command)"
                } else {
                    "off"
                }
            );
            println!(
                "rules    {} · network {}",
                config.tools.rules.len(),
                if config.tools.sandbox_network {
                    "allowed"
                } else {
                    "blocked"
                }
            );
            println!("classify auto in auto mode · session model");
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
            for warning in &config.security_warnings {
                println!("warning  {warning}");
            }
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
    worktree: Option<bool>,
    mode: Option<String>,
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
                    "mode": mode,
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
        return create_session(client, None, None, None, None, None).await;
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
            EventKind::Integration => {
                if let Some(text) = event.payload.get("text").and_then(Value::as_str) {
                    println!("integration> {text}");
                }
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
                    let classifier = payload
                        .classifier
                        .as_ref()
                        .map(|item| {
                            format!(
                                " · classify {} {:.2}{}{}",
                                item.decision,
                                item.confidence,
                                if item.cached { " cached" } else { "" },
                                if item.reason.is_empty() {
                                    String::new()
                                } else {
                                    format!(" ({})", item.reason)
                                }
                            )
                        })
                        .unwrap_or_default();
                    println!(
                        "perm> {} {} · risk {} · {}{}{}",
                        payload.tool,
                        payload.action,
                        payload.risk,
                        payload.rule_source,
                        payload
                            .rule
                            .as_deref()
                            .map(|rule| format!(" · rule {rule}"))
                            .unwrap_or_default(),
                        classifier
                    );
                }
            }
            EventKind::Mode => {
                if let Ok(payload) = serde_json::from_value::<ModePayload>(event.payload.clone()) {
                    println!(
                        "mode> {} -> {}{}",
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
                        "ask> {} question(s) (answer in the TUI, or `eix answer <id>=<value>`)",
                        payload.questions.len()
                    );
                    for question in &payload.questions {
                        println!("  [{}] {}", question.id, question.header);
                        println!("    {}", question.question);
                        for (index, option) in question.options.iter().enumerate() {
                            let recommended = if index == 0 { " (recommended)" } else { "" };
                            match option.description.as_deref() {
                                Some(description) if !description.is_empty() => println!(
                                    "    {}. {} — {}{recommended}",
                                    index + 1,
                                    option.label,
                                    description
                                ),
                                _ => {
                                    println!("    {}. {}{recommended}", index + 1, option.label)
                                }
                            }
                        }
                    }
                }
            }
            EventKind::InputAnswered => {
                if let Ok(payload) =
                    serde_json::from_value::<InputAnsweredPayload>(event.payload.clone())
                {
                    let answers = payload
                        .answers
                        .iter()
                        .map(|(id, values)| format!("{id}={}", values.join(", ")))
                        .collect::<Vec<_>>()
                        .join(" ");
                    println!("ask> answered: {answers}");
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
        "apply_patch" => {
            let patch = string("patchText");
            let target = patch
                .lines()
                .find_map(|line| {
                    line.strip_prefix("*** Add File: ")
                        .or_else(|| line.strip_prefix("*** Update File: "))
                        .or_else(|| line.strip_prefix("*** Delete File: "))
                        .map(str::trim)
                })
                .unwrap_or_default();
            if target.is_empty() {
                "apply_patch".to_string()
            } else {
                format!("apply_patch {target}")
            }
        }
        "exec_command" => {
            let command = arguments
                .get("cmd")
                .and_then(Value::as_str)
                .unwrap_or_else(|| string("command"));
            format!("{tool}: {command}")
        }
        "write_stdin" => format!("write_stdin {}", string("session_id")),
        "view_image" => format!("view_image {}", string("path")),
        "web_fetch" | "web_search" => {
            let detail = string("url");
            format!("{tool} {}", if detail.is_empty() { "." } else { detail })
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
    new [--name N] [--cwd P] [--model M] [--worktree | --shared] [--mode MODE]
                               create a session (default: share the current workspace;
                               --worktree creates .worktrees/<name> immediately)
    mode MODE [--session ID] [--temporary]
                               set plan / build / auto / full for a session
    log [--session ID]         print the persisted transcript
    rename [--session ID] NAME
    cancel [--session ID]      cancel the running turn
    approve [--session ID] [--hunks 0,2] [--reason TEXT]
                               approve the pending tool call (all or selected hunks)
    deny [--session ID] [--reason TEXT]
                               deny the pending tool call, optionally with a reason
    answer [--session ID] <option> | <id>=<value>...
                               answer pending question(s); a bare number picks
                               an option for a single-question request
    approvals [--json]         list pending approvals across sessions
    status [--json]            daemon and session summary (`--json` for dwmblocks)
    pick [--query Q] [--limit N] [--json]
                               print sessions for rofi/dmenu/fzf (id<TAB>name<TAB>status<TAB>cwd)
    ps [--session ID]          list background processes
    tail PROCESS               print a process's log tail
    kill PROCESS               stop a background process
    fork [--session ID] [--at SEQ] [--name NAME] [--shared]
                               copy history into a new session (default: same files;
                               worktree.auto enables a new git worktree)
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
    ready [--session ID]       mark the current worktree ready for rebase/test/merge
    integrate [status | continue SESSION]
                               show the integration queue or resume a paused session
    mcp auth <server> [--no-browser]
                               authorize a remote MCP server with OAuth (PKCE)
    mcp logout <server>        forget a remote MCP server's OAuth token
    plugin pack [DIR]          write eix-plugin.sums for a plugin directory
    plugin verify [DIR] [--allow-unsigned]
                               verify checksums and the detached signature
    plugin install DIR [--force] [--allow-unsigned]
                               verify and install under ~/.config/eix/plugins
    plugin trust PUBLISHER KEY trust a base64 Ed25519 publisher key
    plugin untrust PUBLISHER   remove a trusted publisher key
    auth login PROVIDER [--api-key KEY | --stdin] [--no-browser]
                               store an API key or run the provider's OAuth flow
    auth list [--json]         show providers and where each credential comes from
    auth logout PROVIDER       remove a stored provider credential
    model [show] [--all] [--json]
                               show the daemon default and provider auth state
    model use PROVIDER/MODEL   switch the daemon default at runtime
    model default              restore the configured default model
    pair [--name LABEL]        mint a one-time code for a new remote client
    pair list [--json]         list paired clients
    pair revoke <id>           revoke a paired client
    acp                        run the Agent Client Protocol adapter on stdio
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
    eix mode plan --session 4f2a9c11
    eix ps
    eix tail 4f2a9c11
    eix fork --at 12
    eix rewind --at 12
    eix worktree list
    eix worktree remove 4f2a9c11 --force
    eix ready --session 4f2a9c11
    eix integrate status
    eix integrate continue 4f2a9c11
    eix plugin pack examples/plugins/echo
    eix plugin install examples/plugins/echo --allow-unsigned
    eix --tcp 127.0.0.1:7777 --token \"$EIX_TOKEN\" ls"
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
    fn mcp_auth_takes_a_server_and_never_defaults_it() {
        match parse_command(
            "mcp".to_string(),
            vec![
                "auth".to_string(),
                "docs".to_string(),
                "--no-browser".to_string(),
            ],
        )
        .unwrap()
        {
            Command::Mcp(McpCmd::Auth { server, no_browser }) => {
                assert_eq!(server, "docs");
                assert!(no_browser);
            }
            _ => panic!("expected mcp auth"),
        }
        assert!(parse_command("mcp".to_string(), vec!["auth".to_string()]).is_err());
        assert!(parse_command("mcp".to_string(), Vec::new()).is_err());
        match parse_command(
            "mcp".to_string(),
            vec!["logout".to_string(), "docs".to_string()],
        )
        .unwrap()
        {
            Command::Mcp(McpCmd::Logout { server }) => assert_eq!(server, "docs"),
            _ => panic!("expected mcp logout"),
        }
    }

    #[test]
    fn auth_parses_login_list_and_logout() {
        match parse_command(
            "auth".to_string(),
            vec![
                "login".to_string(),
                "openai".to_string(),
                "--stdin".to_string(),
            ],
        )
        .unwrap()
        {
            Command::Auth(AuthCmd::Login {
                provider,
                api_key,
                stdin,
                no_browser,
            }) => {
                assert_eq!(provider, "openai");
                assert!(api_key.is_none());
                assert!(stdin);
                assert!(!no_browser);
            }
            _ => panic!("expected auth login"),
        }
        match parse_command(
            "auth".to_string(),
            vec![
                "login".to_string(),
                "claude".to_string(),
                "--api-key".to_string(),
                "sk-test".to_string(),
                "--no-browser".to_string(),
            ],
        )
        .unwrap()
        {
            Command::Auth(AuthCmd::Login {
                provider, api_key, ..
            }) => {
                assert_eq!(provider, "claude");
                assert_eq!(api_key.as_deref(), Some("sk-test"));
            }
            _ => panic!("expected auth login --api-key"),
        }
        // Bare `eix auth openai` is shorthand for login.
        match parse_command("auth".to_string(), vec!["openai".to_string()]).unwrap() {
            Command::Auth(AuthCmd::Login { provider, .. }) => assert_eq!(provider, "openai"),
            _ => panic!("expected auth login shorthand"),
        }
        match parse_command(
            "auth".to_string(),
            vec!["list".to_string(), "--json".to_string()],
        )
        .unwrap()
        {
            Command::Auth(AuthCmd::List { json }) => assert!(json),
            _ => panic!("expected auth list"),
        }
        match parse_command(
            "auth".to_string(),
            vec!["logout".to_string(), "openai".to_string()],
        )
        .unwrap()
        {
            Command::Auth(AuthCmd::Logout { provider }) => assert_eq!(provider, "openai"),
            _ => panic!("expected auth logout"),
        }
        assert!(parse_command("auth".to_string(), vec!["login".to_string()]).is_err());
    }

    #[test]
    fn model_parses_show_and_runtime_switch() {
        match parse_command("model".to_string(), Vec::new()).unwrap() {
            Command::Model(ModelCmd::Show { json, all }) => {
                assert!(!json);
                assert!(!all);
            }
            _ => panic!("expected model show"),
        }
        match parse_command(
            "model".to_string(),
            vec![
                "show".to_string(),
                "--all".to_string(),
                "--json".to_string(),
            ],
        )
        .unwrap()
        {
            Command::Model(ModelCmd::Show { json, all }) => {
                assert!(json);
                assert!(all);
            }
            _ => panic!("expected model show --all"),
        }
        match parse_command(
            "model".to_string(),
            vec!["use".to_string(), "openai/gpt-5".to_string()],
        )
        .unwrap()
        {
            Command::Model(ModelCmd::Use { model }) => {
                assert_eq!(model.as_deref(), Some("openai/gpt-5"))
            }
            _ => panic!("expected model use"),
        }
        match parse_command("model".to_string(), vec!["openai/gpt-5".to_string()]).unwrap() {
            Command::Model(ModelCmd::Use { model }) => {
                assert_eq!(model.as_deref(), Some("openai/gpt-5"))
            }
            _ => panic!("expected model shorthand"),
        }
        match parse_command("model".to_string(), vec!["default".to_string()]).unwrap() {
            Command::Model(ModelCmd::Use { model }) => assert!(model.is_none()),
            _ => panic!("expected model default"),
        }
    }

    #[test]
    fn pair_defaults_to_create_and_parses_management_subcommands() {
        match parse_command("pair".to_string(), Vec::new()).unwrap() {
            Command::Pair(PairCmd::Create { name }) => assert!(name.is_none()),
            _ => panic!("expected pair create"),
        }
        match parse_command(
            "pair".to_string(),
            vec!["--name".to_string(), "laptop".to_string()],
        )
        .unwrap()
        {
            Command::Pair(PairCmd::Create { name }) => assert_eq!(name.as_deref(), Some("laptop")),
            _ => panic!("expected pair create with a name"),
        }
        match parse_command("pair".to_string(), vec!["list".to_string()]).unwrap() {
            Command::Pair(PairCmd::List { json }) => assert!(!json),
            _ => panic!("expected pair list"),
        }
        match parse_command(
            "pair".to_string(),
            vec!["list".to_string(), "--json".to_string()],
        )
        .unwrap()
        {
            Command::Pair(PairCmd::List { json }) => assert!(json),
            _ => panic!("expected pair list --json"),
        }
        match parse_command(
            "pair".to_string(),
            vec!["revoke".to_string(), "ct_123".to_string()],
        )
        .unwrap()
        {
            Command::Pair(PairCmd::Revoke { id }) => assert_eq!(id, "ct_123"),
            _ => panic!("expected pair revoke"),
        }
        assert!(parse_command("pair".to_string(), vec!["revoke".to_string()]).is_err());
        assert!(parse_command("pair".to_string(), vec!["nope".to_string()]).is_err());
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
