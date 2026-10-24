//! Owns the daemon connection and serializes TUI commands into requests.
//!
//! The UI never blocks on the network: it sends [`Command`]s and receives
//! [`Event`]s. Requests and notifications share one connection, so a request
//! can be in flight while deltas keep arriving.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use eix_core::{
    ApprovalDecision, ApprovalResult, AttachResult, CancelResult, ContextLedgerResult, Endpoint,
    EventsResult, HelloResult, InputResult, IntegrationListResult, IntegrationState, Notification,
    ProcessKillResult, ProcessList, ProcessOutput, RewindResult, SearchResult, Session,
    SessionList, SessionResult, SubmitResult,
};
use serde_json::{json, Value};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use super::net::{NetEvent, Requester};
use crate::client::ensure_endpoint;

/// Stored events fetched when a pane first attaches. A long session is opened
/// at its tail; older pages are loaded when the user scrolls to the top.
pub const ATTACH_TAIL_EVENTS: usize = 1000;
/// Number of older events requested per history page.
pub const HISTORY_PAGE_EVENTS: usize = 500;

pub enum Command {
    Hello,
    List,
    ListIntegrations,
    Attach(String),
    /// Fetch a page of events older than `before_seq`.
    LoadHistory {
        session_id: String,
        before_seq: u64,
    },
    Create {
        /// `Some(true)` creates a worktree now; `None` lets the daemon create
        /// one lazily from the first prompt; `Some(false)` shares the cwd.
        worktree: Option<bool>,
        /// Working directory; defaults to the TUI's launch directory.
        cwd: Option<PathBuf>,
        /// Optional name; the daemon generates one when omitted.
        name: Option<String>,
        /// Defaults from cli.json (unless overridden on the command line).
        model: Option<String>,
        agent: Option<String>,
        permission: Option<String>,
    },
    /// Mark a worktree session ready for the integration queue.
    Ready {
        session_id: String,
    },
    /// Move a session to another working directory.
    SetCwd {
        session_id: String,
        path: String,
    },
    /// Drop everything before now from the model context.
    ClearContext {
        session_id: String,
    },
    /// Summarize the whole context so the next turn starts small.
    CompactContext {
        session_id: String,
        notes: Option<String>,
    },
    /// Change the session's tool permission level.
    SetPermission {
        session_id: String,
        permission: eix_core::PermissionLevel,
        temporary: bool,
    },
    /// Pin the session to one of the configured models.
    SetModel {
        session_id: String,
        model: Option<String>,
    },
    /// Select the session's primary agent.
    SetAgent {
        session_id: String,
        agent: Option<String>,
    },
    /// List configured models plus each provider's auto-fetched catalog.
    ListModels,
    /// List agents visible from a session's workspace.
    ListAgents {
        session_id: String,
    },
    /// List configured MCP servers and their tool counts.
    ListMcp,
    /// Re-read the daemon config files.
    ReloadConfig,
    /// Hide or restore a session.
    ArchiveSession {
        session_id: String,
        archived: bool,
    },
    /// Delete a session and its event log.
    DeleteSession {
        session_id: String,
    },
    /// Delete a session only when it has no events and no draft. Used when a
    /// pane created for a fresh session is closed without any input.
    DeleteSessionIfEmpty {
        session_id: String,
    },
    /// Fetch recent turns/tool calls for the status line.
    TimelineData {
        session_id: String,
        limit: Option<usize>,
    },
    /// Run a user-typed shell command in the session workspace.
    RunShell {
        session_id: String,
        command: String,
        /// Insert the command's output into the prompt instead of only showing
        /// it in the transcript/status line (`:r!`).
        insert_output: bool,
    },
    /// Read the model-managed task list.
    ListTodos {
        session_id: String,
    },
    /// Hide or restore the newest turn.
    Undo {
        session_id: String,
        redo: bool,
    },
    /// Ask a one-off side question (never written to the session log).
    Btw {
        session_id: String,
        question: String,
    },
    BtwCancel {
        session_id: String,
    },
    Submit {
        session_id: String,
        text: String,
        images: Vec<String>,
    },
    Cancel(String),
    Approval {
        session_id: String,
        approval_id: String,
        decision: ApprovalDecision,
        /// Selected hunk indices; `None` applies the whole change.
        hunks: Option<Vec<usize>>,
        reason: Option<String>,
    },
    /// Answer an `ask_user` question with the option at `option_index`.
    Input {
        session_id: String,
        input_id: String,
        option_index: usize,
    },
    ContextLedger {
        session_id: String,
    },
    ContextOp {
        session_id: String,
        op: ContextOp,
    },
    ProcessList {
        session_id: String,
    },
    ProcessOutput {
        process_id: String,
    },
    ProcessKill {
        process_id: String,
    },
    Fork {
        session_id: String,
        /// Copy history before this sequence; `None` copies everything.
        at_seq: Option<u64>,
        name: Option<String>,
    },
    Rewind {
        session_id: String,
        /// Restore the workspace to just before this sequence.
        at_seq: Option<u64>,
        name: Option<String>,
    },
    Search {
        query: String,
        session_id: Option<String>,
        limit: Option<u32>,
    },
    PickerData {
        session_id: String,
        /// Daemon provider (`files`, `worktrees`, `branches`, `context`).
        kind: String,
    },
    /// Fetch the selected row's content for the picker preview pane.
    PickerPreview {
        /// Preview provider (`files` or `sessions`).
        kind: String,
        /// Workspace context for file previews.
        session_id: Option<String>,
        /// Workspace-relative file path or target session id.
        value: String,
    },
    DraftGet {
        session_id: String,
    },
    DraftSet {
        session_id: String,
        text: String,
    },
    PatchGet {
        session_id: String,
        patch_id: String,
    },
    PatchRevise {
        session_id: String,
        patch_id: String,
        text: String,
    },
}

/// Ledger edits map one-to-one onto `context.*` requests.
pub enum ContextOp {
    Drop { entry: String, dropped: bool },
    Pin { entry: String, pinned: bool },
    Compact { from_seq: u64, to_seq: u64 },
}

impl Command {
    fn label(&self) -> &'static str {
        match self {
            Self::Hello => "connect",
            Self::List => "list",
            Self::ListIntegrations => "integrations",
            Self::Attach(_) => "attach",
            Self::LoadHistory { .. } => "history",
            Self::Create { .. } => "create",
            Self::Ready { .. } => "ready",
            Self::SetCwd { .. } => "cd",
            Self::ClearContext { .. } => "clear",
            Self::CompactContext { .. } => "compact",
            Self::SetPermission { .. } => "permission",
            Self::SetModel { .. } => "model",
            Self::SetAgent { .. } => "agent",
            Self::ListModels | Self::ListAgents { .. } => "catalog",
            Self::ListMcp => "mcp",
            Self::ReloadConfig => "reload",
            Self::ArchiveSession { .. } => "archive",
            Self::DeleteSession { .. } => "delete",
            Self::DeleteSessionIfEmpty { .. } => "delete",
            Self::TimelineData { .. } => "timeline",
            Self::ListTodos { .. } => "todos",
            Self::RunShell { .. } => "shell",
            Self::Undo { redo: false, .. } => "undo",
            Self::Undo { redo: true, .. } => "redo",
            Self::Btw { .. } => "btw",
            Self::BtwCancel { .. } => "btw",
            Self::Submit { .. } => "submit",
            Self::Cancel(_) => "cancel",
            Self::Approval { .. } => "approval",
            Self::Input { .. } => "answer",
            Self::ContextLedger { .. } | Self::ContextOp { .. } => "context",
            Self::ProcessList { .. } => "processes",
            Self::ProcessOutput { .. } => "process output",
            Self::ProcessKill { .. } => "kill process",
            Self::Fork { .. } => "fork",
            Self::Rewind { .. } => "rewind",
            Self::Search { .. } => "search",
            Self::PickerData { .. } => "picker",
            Self::PickerPreview { .. } => "picker preview",
            Self::DraftGet { .. } => "draft",
            Self::DraftSet { .. } => "draft",
            Self::PatchGet { .. } => "patch",
            Self::PatchRevise { .. } => "patch",
        }
    }
}

pub enum Event {
    Hello(HelloResult),
    Sessions(Vec<Session>),
    Integrations(Vec<IntegrationState>),
    Attached(AttachResult),
    /// An older page of stored events, ready to prepend to a pane.
    HistoryPage {
        session_id: String,
        events: Vec<eix_core::Event>,
        has_more: bool,
    },
    Created(Session),
    /// A session's cwd or permission changed; the view replays it.
    Updated(Session),
    ContextCleared {
        session_id: String,
    },
    Compacted {
        session_id: String,
        entries: usize,
        tokens: usize,
    },
    BtwStarted {
        session_id: String,
        btw_id: String,
    },
    BtwCancelled {
        session_id: String,
    },
    Submitted(SubmitResult),
    Cancelled(bool),
    Approval {
        approval_id: String,
        accepted: bool,
    },
    InputResponded {
        accepted: bool,
    },
    Ledger {
        session_id: String,
        result: ContextLedgerResult,
    },
    Processes {
        session_id: String,
        result: ProcessList,
    },
    ProcessOutput {
        result: ProcessOutput,
    },
    ProcessKilled {
        process_id: String,
        killed: bool,
    },
    Forked(Session),
    Rewound(Box<RewindResult>),
    Search(SearchResult),
    PickerData {
        kind: String,
        items: Vec<eix_core::PickerItem>,
    },
    PickerPreview {
        kind: String,
        value: String,
        text: Option<String>,
        error: Option<String>,
    },
    Models {
        models: Vec<eix_core::ModelInfo>,
        warnings: Vec<String>,
    },
    Agents(Vec<eix_core::AgentSpec>),
    Mcp(Vec<eix_core::McpServerInfo>),
    ConfigReloaded {
        paths: Vec<String>,
    },
    Archived(Session),
    Deleted(String),
    Timeline(Vec<eix_core::TimelineItem>),
    Rewritten(String),
    Todos(Vec<eix_core::TodoItem>),
    ShellRan {
        ok: bool,
        output: String,
        insert_output: bool,
    },
    Draft {
        session_id: String,
        text: String,
    },
    PatchEdit {
        session_id: String,
        patch_id: String,
        text: String,
    },
    Notification(Notification),
    Error {
        context: String,
        message: String,
    },
    Closed(String),
}

pub fn spawn(
    endpoint: Endpoint,
    data_dir: PathBuf,
    config: Option<PathBuf>,
) -> (
    UnboundedSender<Command>,
    UnboundedReceiver<Event>,
    tokio::task::JoinHandle<()>,
) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        if let Err(err) = run(endpoint, data_dir, config, cmd_rx, ev_tx.clone()).await {
            let _ = ev_tx.send(Event::Error {
                context: "connect".into(),
                message: format!("{err:#}"),
            });
        }
    });
    (cmd_tx, ev_rx, handle)
}

async fn run(
    endpoint: Endpoint,
    data_dir: PathBuf,
    config: Option<PathBuf>,
    mut cmd_rx: UnboundedReceiver<Command>,
    ev_tx: UnboundedSender<Event>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot determine current directory")?;
    ensure_endpoint(&endpoint, &data_dir, config.as_deref()).await?;
    let (requester, mut net_events) = Requester::connect(&endpoint).await?;

    loop {
        tokio::select! {
            command = cmd_rx.recv() => {
                let Some(command) = command else { break };
                let label = command.label();
                if let Err(err) = dispatch(command, &requester, &ev_tx, &cwd).await {
                    let _ = ev_tx.send(Event::Error {
                        context: label.to_string(),
                        message: format!("{err:#}"),
                    });
                }
            }
            event = net_events.recv() => {
                match event {
                    Some(NetEvent::Notification(notification)) => {
                        let _ = ev_tx.send(Event::Notification(notification));
                    }
                    Some(NetEvent::Closed(reason)) => {
                        let _ = ev_tx.send(Event::Closed(reason));
                        break;
                    }
                    None => break,
                }
            }
            else => break,
        }
    }
    Ok(())
}

async fn dispatch(
    command: Command,
    requester: &Requester,
    events: &UnboundedSender<Event>,
    default_cwd: &Path,
) -> Result<()> {
    match command {
        Command::ListIntegrations => {
            let value = requester.request("integration.list", json!({})).await?;
            let result: IntegrationListResult = decode(value)?;
            send(events, Event::Integrations(result.integrations))
        }
        Command::Hello => {
            let value = requester.request("hello", Value::Null).await?;
            send(events, Event::Hello(decode(value)?))
        }
        Command::List => {
            let value = requester.request("session.list", Value::Null).await?;
            let list: SessionList = decode(value)?;
            send(events, Event::Sessions(list.sessions))
        }
        Command::Attach(session_id) => {
            let value = requester
                .request(
                    "session.attach",
                    json!({
                        "session_id": session_id,
                        "tail": ATTACH_TAIL_EVENTS,
                    }),
                )
                .await?;
            send(events, Event::Attached(decode(value)?))
        }
        Command::LoadHistory {
            session_id,
            before_seq,
        } => {
            let value = requester
                .request(
                    "session.events",
                    json!({
                        "session_id": session_id,
                        "before_seq": before_seq,
                        "limit": HISTORY_PAGE_EVENTS,
                    }),
                )
                .await?;
            let result: EventsResult = decode(value)?;
            send(
                events,
                Event::HistoryPage {
                    session_id,
                    events: result.events,
                    has_more: result.has_more,
                },
            )
        }
        Command::Create {
            worktree,
            cwd,
            name,
            model,
            agent,
            permission,
        } => {
            let cwd = cwd.unwrap_or_else(|| default_cwd.to_path_buf());
            let value = requester
                .request(
                    "session.create",
                    json!({
                        "cwd": cwd,
                        "worktree": worktree,
                        "name": name,
                        "model": model,
                        "agent": agent,
                        "permission": permission,
                    }),
                )
                .await?;
            let result: SessionResult = decode(value)?;
            send(events, Event::Created(result.session))
        }
        Command::Ready { session_id } => {
            let value = requester
                .request("session.ready", json!({ "session_id": session_id }))
                .await?;
            let state: eix_core::IntegrationState = decode(value)?;
            send(
                events,
                Event::Notification(Notification::new("integration.updated", state)),
            )
        }
        Command::SetCwd { session_id, path } => {
            let value = requester
                .request(
                    "session.cd",
                    json!({ "session_id": session_id, "path": path }),
                )
                .await?;
            let result: SessionResult = decode(value)?;
            send(events, Event::Updated(result.session))
        }
        Command::ClearContext { session_id } => {
            let value = requester
                .request("context.clear", json!({ "session_id": session_id }))
                .await?;
            let _: eix_core::ContextLedgerResult = decode(value)?;
            send(events, Event::ContextCleared { session_id })
        }
        Command::CompactContext { session_id, notes } => {
            let value = requester
                .request(
                    "context.compact",
                    json!({
                        "session_id": session_id,
                        "summary": notes,
                        "generate": true,
                    }),
                )
                .await?;
            let ledger: eix_core::ContextLedgerResult = decode(value)?;
            let entries = ledger.entries.len();
            let tokens = ledger.total_tokens;
            send(
                events,
                Event::Compacted {
                    session_id,
                    entries,
                    tokens,
                },
            )
        }
        Command::SetPermission {
            session_id,
            permission,
            temporary,
        } => {
            let value = requester
                .request(
                    "session.permission",
                    json!({
                        "session_id": session_id,
                        "permission": permission,
                        "temporary": temporary,
                    }),
                )
                .await?;
            let result: SessionResult = decode(value)?;
            send(events, Event::Updated(result.session))
        }
        Command::SetModel { session_id, model } => {
            let value = requester
                .request(
                    "session.model",
                    json!({ "session_id": session_id, "model": model }),
                )
                .await?;
            let result: SessionResult = decode(value)?;
            send(events, Event::Updated(result.session))
        }
        Command::SetAgent { session_id, agent } => {
            let value = requester
                .request(
                    "session.agent",
                    json!({ "session_id": session_id, "agent": agent }),
                )
                .await?;
            let result: SessionResult = decode(value)?;
            send(events, Event::Updated(result.session))
        }
        Command::ListModels => {
            let value = requester.request("model.list", Value::Null).await?;
            let result: eix_core::ModelListResult = decode(value)?;
            send(
                events,
                Event::Models {
                    models: result.models,
                    warnings: result.warnings,
                },
            )
        }
        Command::ListAgents { session_id } => {
            let value = requester
                .request("agent.list", json!({ "session_id": session_id }))
                .await?;
            let result: eix_core::AgentListResult = decode(value)?;
            send(events, Event::Agents(result.agents))
        }
        Command::ListMcp => {
            let value = requester.request("mcp.list", Value::Null).await?;
            let result: eix_core::McpListResult = decode(value)?;
            send(events, Event::Mcp(result.servers))
        }
        Command::ArchiveSession {
            session_id,
            archived,
        } => {
            let value = requester
                .request(
                    "session.archive",
                    json!({ "session_id": session_id, "archived": archived }),
                )
                .await?;
            let result: eix_core::SessionResult = decode(value)?;
            send(events, Event::Archived(result.session))
        }
        Command::DeleteSession { session_id } => {
            requester
                .request("session.delete", json!({ "session_id": session_id }))
                .await?;
            send(events, Event::Deleted(session_id))
        }
        Command::DeleteSessionIfEmpty { session_id } => {
            let result = requester
                .request(
                    "session.delete_if_empty",
                    json!({ "session_id": &session_id }),
                )
                .await?;
            if result.get("deleted").and_then(Value::as_bool) == Some(true) {
                send(events, Event::Deleted(session_id))
            } else {
                Ok(())
            }
        }
        Command::TimelineData { session_id, limit } => {
            let value = requester
                .request(
                    "session.timeline",
                    json!({ "session_id": session_id, "limit": limit }),
                )
                .await?;
            let result: eix_core::TimelineResult = decode(value)?;
            send(events, Event::Timeline(result.items))
        }
        Command::RunShell {
            session_id,
            command,
            insert_output,
        } => {
            let value = requester
                .request(
                    "shell.run",
                    json!({ "session_id": session_id, "command": command }),
                )
                .await?;
            let result: eix_core::ShellRunResult = decode(value)?;
            send(
                events,
                Event::ShellRan {
                    ok: result.ok,
                    output: result.output,
                    insert_output,
                },
            )
        }
        Command::ListTodos { session_id } => {
            let value = requester
                .request("session.todos", json!({ "session_id": session_id }))
                .await?;
            let result: eix_core::TodoList = decode(value)?;
            send(events, Event::Todos(result.items))
        }
        Command::Undo { session_id, redo } => {
            let method = if redo { "session.redo" } else { "session.undo" };
            requester
                .request(method, json!({ "session_id": session_id }))
                .await?;
            send(events, Event::Rewritten(session_id))
        }
        Command::ReloadConfig => {
            let value = requester.request("config.reload", Value::Null).await?;
            let paths = value["paths"]
                .as_array()
                .map(|paths| {
                    paths
                        .iter()
                        .filter_map(|path| path.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            send(events, Event::ConfigReloaded { paths })
        }
        Command::Btw {
            session_id,
            question,
        } => {
            let value = requester
                .request(
                    "session.btw",
                    json!({ "session_id": session_id, "question": question }),
                )
                .await?;
            let result: eix_core::BtwResult = decode(value)?;
            send(
                events,
                Event::BtwStarted {
                    session_id,
                    btw_id: result.btw_id,
                },
            )
        }
        Command::BtwCancel { session_id } => {
            requester
                .request("btw.cancel", json!({ "session_id": session_id }))
                .await?;
            send(events, Event::BtwCancelled { session_id })
        }
        Command::Submit {
            session_id,
            text,
            images,
        } => {
            let value = requester
                .request(
                    "prompt.submit",
                    json!({ "session_id": session_id, "text": text, "images": images }),
                )
                .await?;
            send(events, Event::Submitted(decode(value)?))
        }
        Command::Cancel(session_id) => {
            let value = requester
                .request("prompt.cancel", json!({ "session_id": session_id }))
                .await?;
            let result: CancelResult = decode(value)?;
            send(events, Event::Cancelled(result.cancelled))
        }
        Command::Approval {
            session_id,
            approval_id,
            decision,
            hunks,
            reason,
        } => {
            let value = requester
                .request(
                    "approval.respond",
                    json!({
                        "session_id": session_id,
                        "approval_id": approval_id,
                        "decision": decision,
                        "hunks": hunks,
                        "reason": reason,
                    }),
                )
                .await?;
            let result: ApprovalResult = decode(value)?;
            send(
                events,
                Event::Approval {
                    approval_id,
                    accepted: result.accepted,
                },
            )
        }
        Command::Input {
            session_id,
            input_id,
            option_index,
        } => {
            let value = requester
                .request(
                    "input.respond",
                    json!({
                        "session_id": session_id,
                        "input_id": input_id,
                        "option_index": option_index,
                    }),
                )
                .await?;
            let result: InputResult = decode(value)?;
            send(
                events,
                Event::InputResponded {
                    accepted: result.accepted,
                },
            )
        }
        Command::ContextLedger { session_id } => {
            let value = requester
                .request("context.ledger", json!({ "session_id": session_id }))
                .await?;
            send(
                events,
                Event::Ledger {
                    session_id,
                    result: decode(value)?,
                },
            )
        }
        Command::ContextOp { session_id, op } => {
            let (method, params) = match op {
                ContextOp::Drop { entry, dropped } => (
                    "context.drop",
                    json!({ "session_id": session_id, "entry": entry, "dropped": dropped }),
                ),
                ContextOp::Pin { entry, pinned } => (
                    "context.pin",
                    json!({ "session_id": session_id, "entry": entry, "pinned": pinned }),
                ),
                ContextOp::Compact { from_seq, to_seq } => (
                    "context.compact",
                    json!({ "session_id": session_id, "from_seq": from_seq, "to_seq": to_seq }),
                ),
            };
            let value = requester.request(method, params).await?;
            send(
                events,
                Event::Ledger {
                    session_id,
                    result: decode(value)?,
                },
            )
        }
        Command::ProcessList { session_id } => {
            let value = requester
                .request("process.list", json!({ "session_id": session_id }))
                .await?;
            send(
                events,
                Event::Processes {
                    session_id,
                    result: decode(value)?,
                },
            )
        }
        Command::ProcessOutput { process_id } => {
            let value = requester
                .request("process.output", json!({ "process_id": process_id }))
                .await?;
            send(
                events,
                Event::ProcessOutput {
                    result: decode(value)?,
                },
            )
        }
        Command::ProcessKill { process_id } => {
            let value = requester
                .request("process.kill", json!({ "process_id": process_id }))
                .await?;
            let result: ProcessKillResult = decode(value)?;
            send(
                events,
                Event::ProcessKilled {
                    process_id,
                    killed: result.killed,
                },
            )
        }
        Command::Fork {
            session_id,
            at_seq,
            name,
        } => {
            let value = requester
                .request(
                    "session.fork",
                    json!({ "session_id": session_id, "at_seq": at_seq, "name": name }),
                )
                .await?;
            let result: SessionResult = decode(value)?;
            send(events, Event::Forked(result.session))
        }
        Command::Rewind {
            session_id,
            at_seq,
            name,
        } => {
            let value = requester
                .request(
                    "session.rewind",
                    json!({ "session_id": session_id, "at_seq": at_seq, "name": name }),
                )
                .await?;
            let result: RewindResult = decode(value)?;
            send(events, Event::Rewound(Box::new(result)))
        }
        Command::Search {
            query,
            session_id,
            limit,
        } => {
            let value = requester
                .request(
                    "search.query",
                    json!({ "query": query, "session_id": session_id, "limit": limit }),
                )
                .await?;
            let result: SearchResult = decode(value)?;
            send(events, Event::Search(result))
        }
        Command::PickerData { session_id, kind } => {
            let value = requester
                .request(
                    "picker.query",
                    json!({
                        "kind": kind,
                        "session_id": session_id,
                        "limit": 2000
                    }),
                )
                .await?;
            let result: eix_core::PickerResult = decode(value)?;
            send(
                events,
                Event::PickerData {
                    kind: result.kind,
                    items: result.items,
                },
            )
        }
        Command::PickerPreview {
            kind,
            session_id,
            value,
        } => {
            let request = requester
                .request(
                    "picker.preview",
                    json!({
                        "kind": &kind,
                        "session_id": &session_id,
                        "value": &value,
                    }),
                )
                .await;
            match request {
                Ok(response) => {
                    let result: eix_core::PickerPreviewResult = decode(response)?;
                    send(
                        events,
                        Event::PickerPreview {
                            kind: result.kind,
                            value: result.value,
                            text: Some(result.text),
                            error: None,
                        },
                    )
                }
                Err(err) => send(
                    events,
                    Event::PickerPreview {
                        kind,
                        value,
                        text: None,
                        error: Some(format!("{err:#}")),
                    },
                ),
            }
        }
        Command::DraftGet { session_id } => {
            let value = requester
                .request("draft.get", json!({ "session_id": session_id }))
                .await?;
            let result: eix_core::DraftResult = decode(value)?;
            send(
                events,
                Event::Draft {
                    session_id: result.session_id,
                    text: result.text,
                },
            )
        }
        Command::DraftSet { session_id, text } => {
            requester
                .request(
                    "draft.set",
                    json!({ "session_id": session_id, "text": text }),
                )
                .await?;
            Ok(())
        }
        Command::PatchGet {
            session_id,
            patch_id,
        } => {
            let value = requester
                .request(
                    "patch.get",
                    json!({ "session_id": session_id, "patch_id": patch_id }),
                )
                .await?;
            let result: eix_core::PatchGetResult = decode(value)?;
            send(
                events,
                Event::PatchEdit {
                    session_id: result.session_id,
                    patch_id: result.patch_id,
                    text: result.text,
                },
            )
        }
        Command::PatchRevise {
            session_id,
            patch_id,
            text,
        } => {
            requester
                .request(
                    "patch.revise",
                    json!({ "session_id": session_id, "patch_id": patch_id, "text": text }),
                )
                .await?;
            Ok(())
        }
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).context("unexpected daemon response")
}

fn send(events: &UnboundedSender<Event>, event: Event) -> Result<()> {
    events.send(event).map_err(|_| anyhow::anyhow!("ui closed"))
}
