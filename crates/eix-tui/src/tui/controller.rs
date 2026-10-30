//! Owns the daemon connection and serializes TUI commands into requests.
//!
//! The UI never blocks on the network: it sends [`Command`]s and receives
//! [`Event`]s. Requests and notifications share one connection, so a request
//! can be in flight while deltas keep arriving.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use eix_core::{
    ApprovalDecision, ApprovalResult, AttachResult, CancelResult, Endpoint, EventsResult,
    HelloResult, InputResult, IntegrationListResult, IntegrationState, Notification,
    ProcessKillResult, ProcessList, ProcessOutput, Session, SessionList, SessionResult,
    SubmitResult,
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

#[allow(clippy::enum_variant_names)]
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
        mode: Option<String>,
    },
    /// Move an existing session into a new worktree.
    CreateWorktree {
        session_id: String,
        name: Option<String>,
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
    /// Change the session's execution mode.
    SetMode {
        session_id: String,
        mode: eix_core::SessionMode,
        temporary: bool,
    },
    /// Pin the session to one of the configured models.
    SetModel {
        session_id: String,
        model: Option<String>,
    },
    /// Pin the model used by this session's Plan-mode turns.
    SetPlanModel {
        session_id: String,
        model: Option<String>,
    },
    /// Switch the daemon's fallback model for sessions without a pin.
    UseModel {
        model: Option<String>,
    },
    /// Select the session's primary agent.
    SetAgent {
        session_id: String,
        agent: Option<String>,
    },
    /// Change the daemon's runtime `spawn_agent` concurrency limit.
    SetSubagentConcurrency {
        value: usize,
    },
    /// List configured models plus each provider's auto-fetched catalog.
    ListModels,
    /// List agents visible from a session's workspace.
    ListAgents {
        session_id: String,
    },
    /// List configured MCP servers and their tool counts.
    ListMcp,
    /// Enable or disable one configured MCP server and refresh the list.
    SetMcpEnabled {
        name: String,
        enabled: bool,
    },
    /// List prompt commands contributed by server plugins.
    ListPluginCommands,
    /// List plugins and optionally load one plugin's settings schema.
    ListPlugins {
        name: Option<String>,
    },
    /// Refresh only the failed/degraded counts used by the statusline.
    PluginHealth,
    /// Collect `plugin.doctor` diagnostics.
    PluginDoctor {
        name: Option<String>,
    },
    /// Fetch buffered plugin logs.
    PluginLogs {
        name: Option<String>,
    },
    /// Start, stop, restart, enable, disable or uninstall one plugin.
    PluginAction {
        action: String,
        name: String,
    },
    /// Merge values into `[plugins.<name>.settings]`.
    PluginSettingsSet {
        name: String,
        values: Value,
    },
    /// Ask a plugin to render one of its declared surfaces.
    PluginUiRender {
        plugin: String,
        surface: String,
        id: String,
    },
    /// Forward a rendered UI action back to the plugin.
    PluginUiAction {
        plugin: String,
        id: String,
        action: String,
        values: Value,
    },
    /// Query a plugin-backed picker provider.
    PluginPick {
        plugin: String,
        id: String,
        query: String,
    },
    /// Run an action command exported by a plugin.
    RunPluginCommand {
        session_id: String,
        name: String,
        arguments: String,
    },
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
    /// Roll back to just before a visible user turn.
    Rewind {
        session_id: String,
        at_seq: u64,
        scope: eix_core::RewindScope,
    },
    /// Read the session's context token ledger for `/context`.
    ContextInfo {
        session_id: String,
        /// Include the per-entry breakdown, not just the sector table.
        all: bool,
    },
    /// Show the repository diff for the session workspace.
    WorkspaceDiff {
        session_id: String,
        staged: bool,
    },
    /// Collect daemon diagnostics for `/doctor`.
    Doctor,
    /// Rename a session.
    RenameSession {
        session_id: String,
        name: String,
    },
    /// Read or update the persistent session goal.
    Goal {
        session_id: String,
        text: Option<String>,
        done: Option<bool>,
        clear: bool,
    },
    /// List loop/once prompts for a session (or all sessions).
    ListSchedules {
        session_id: Option<String>,
    },
    /// Add a loop or one-shot prompt.
    AddSchedule {
        session_id: String,
        kind: String,
        prompt: String,
        interval_secs: u64,
        at: Option<i64>,
    },
    /// Remove one schedule or every schedule for a session.
    RemoveSchedules {
        id: Option<String>,
        session_id: Option<String>,
        kind: Option<String>,
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
        answers: std::collections::BTreeMap<String, Vec<String>>,
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
        /// Replace the focused pane with the fork instead of opening a new one.
        branch: bool,
    },
    PickerData {
        session_id: String,
        /// Daemon provider (`files`, `grep`, `git`, ...).
        kind: String,
        /// Live provider query; `None` keeps the provider's default listing.
        query: Option<String>,
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
    /// Start microphone dictation for a session. Starting again replaces a
    /// running capture.
    VoiceStart {
        session_id: Option<String>,
        /// Transcribe this audio file instead of reading the microphone.
        file: Option<PathBuf>,
    },
    /// Stop dictation and close any open utterance.
    VoiceStop,
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
            Self::CreateWorktree { .. } => "worktree",
            Self::Ready { .. } => "ready",
            Self::SetCwd { .. } => "cd",
            Self::ClearContext { .. } => "clear",
            Self::CompactContext { .. } => "compact",
            Self::SetMode { .. } => "mode",
            Self::SetModel { .. } => "model",
            Self::SetPlanModel { .. } => "model plan",
            Self::UseModel { .. } => "model default",
            Self::SetAgent { .. } => "agent",
            Self::SetSubagentConcurrency { .. } => "subagents",
            Self::ListModels | Self::ListAgents { .. } => "catalog",
            Self::ListMcp => "mcp",
            Self::SetMcpEnabled { .. } => "mcp toggle",
            Self::ListPluginCommands => "plugin commands",
            Self::ListPlugins { .. } => "plugins",
            Self::PluginHealth => "plugin health",
            Self::PluginDoctor { .. } => "plugin doctor",
            Self::PluginLogs { .. } => "plugin logs",
            Self::PluginAction { .. } => "plugin action",
            Self::PluginSettingsSet { .. } => "plugin settings",
            Self::PluginUiRender { .. } => "plugin ui",
            Self::PluginUiAction { .. } => "plugin ui action",
            Self::PluginPick { .. } => "plugin pick",
            Self::RunPluginCommand { .. } => "plugin command",
            Self::ReloadConfig => "reload",
            Self::ArchiveSession { .. } => "archive",
            Self::DeleteSession { .. } => "delete",
            Self::DeleteSessionIfEmpty { .. } => "delete",
            Self::ListTodos { .. } => "todos",
            Self::RunShell { .. } => "shell",
            Self::Undo { redo: false, .. } => "undo",
            Self::Undo { redo: true, .. } => "redo",
            Self::Rewind { .. } => "rewind",
            Self::ContextInfo { .. } => "context",
            Self::WorkspaceDiff { .. } => "diff",
            Self::Doctor => "doctor",
            Self::RenameSession { .. } => "rename",
            Self::Goal { .. } => "goal",
            Self::ListSchedules { .. }
            | Self::AddSchedule { .. }
            | Self::RemoveSchedules { .. } => "schedule",
            Self::Btw { .. } => "btw",
            Self::BtwCancel { .. } => "btw",
            Self::Submit { .. } => "submit",
            Self::Cancel(_) => "cancel",
            Self::Approval { .. } => "approval",
            Self::Input { .. } => "answer",
            Self::ProcessList { .. } => "processes",
            Self::ProcessOutput { .. } => "process output",
            Self::ProcessKill { .. } => "kill process",
            Self::Fork { .. } => "fork",
            Self::PickerData { .. } => "picker",
            Self::PickerPreview { .. } => "picker preview",
            Self::DraftGet { .. } => "draft",
            Self::DraftSet { .. } => "draft",
            Self::PatchGet { .. } => "patch",
            Self::PatchRevise { .. } => "patch",
            Self::VoiceStart { .. } => "voice",
            Self::VoiceStop => "voice",
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
    /// A session's cwd or mode changed; the view replays it.
    Updated(Session),
    /// `/clear` succeeded: the daemon deleted the session's history, so the
    /// view must drop its cached transcript too.
    Reset {
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
    /// A fork that replaced the focused pane (`/branch`).
    Branched(Session),
    ContextInfo {
        session_id: String,
        all: bool,
        info: eix_core::ContextLedgerResult,
    },
    PickerData {
        kind: String,
        items: Vec<eix_core::PickerItem>,
        query: Option<String>,
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
        default_model: String,
    },
    /// The daemon's runtime default model changed.
    DefaultModel(String),
    Agents(Vec<eix_core::AgentSpec>),
    Mcp(Vec<eix_core::McpServerInfo>),
    PluginCommands(Vec<eix_core::PluginCommandInfo>),
    Plugins {
        list: eix_core::PluginListResult,
        settings: Option<eix_core::PluginSettingsResult>,
        surfaces: Vec<eix_core::PluginUiSurface>,
    },
    PluginHealth {
        total: usize,
        failed: Vec<String>,
        degraded: Vec<String>,
    },
    PluginDoctor(eix_core::PluginDoctorResult),
    PluginLogs(eix_core::PluginLogsResult),
    PluginActionDone {
        action: String,
        name: String,
        ok: bool,
        message: String,
    },
    PluginUiPanel {
        plugin: String,
        id: String,
        surface: String,
        title: String,
        result: eix_core::PluginUiRenderResult,
    },
    PluginUiActionDone {
        plugin: String,
        id: String,
        action: String,
        result: eix_core::PluginUiActionResult,
    },
    PluginPicked {
        plugin: String,
        id: String,
        query: String,
        items: Vec<eix_core::UiPickerItem>,
    },
    PluginCommandResult {
        session_id: String,
        result: eix_core::PluginCommandRunResult,
    },
    SubagentConcurrency {
        value: usize,
    },
    ConfigReloaded {
        paths: Vec<String>,
        max_parallel_subagents: usize,
    },
    Archived(Session),
    Deleted(String),
    Rewritten {
        session_id: String,
        note: String,
    },
    WorkspaceDiff {
        result: eix_core::WorkspaceDiffResult,
        /// The request compared the index to HEAD instead of the working tree.
        staged: bool,
    },
    Doctor(Vec<String>),
    Goal(Option<eix_core::Goal>),
    Schedules {
        session_id: Option<String>,
        schedules: Vec<eix_core::Schedule>,
    },
    ScheduleAdded(eix_core::Schedule),
    SchedulesRemoved {
        removed: usize,
    },
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
    Voice(super::voice::VoiceEvent),
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
    // Capture lives in the client, so dictation is driven here rather than by
    // the request dispatcher: it owns a long-lived task and a stop handle.
    let capture = eix_core::Config::load(config.as_deref())
        .unwrap_or_default()
        .voice
        .capture;
    let mut stop: Option<tokio::sync::oneshot::Sender<()>> = None;

    loop {
        tokio::select! {
            command = cmd_rx.recv() => {
                let Some(command) = command else { break };
                match command {
                    Command::VoiceStart { session_id, file } => {
                        // Replacing a running capture is the natural meaning of
                        // pressing the dictation key twice.
                        if let Some(previous) = stop.take() {
                            let _ = previous.send(());
                        }
                        let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
                        stop = Some(stop_tx);
                        let (voice_tx, mut voice_rx) =
                            mpsc::unbounded_channel::<super::voice::VoiceEvent>();
                        let requester = requester.clone();
                        let config = capture.clone();
                        tokio::spawn(super::voice::dictate(
                            requester, config, session_id, file, stop_rx, voice_tx,
                        ));
                        let events = ev_tx.clone();
                        tokio::spawn(async move {
                            while let Some(event) = voice_rx.recv().await {
                                let _ = events.send(Event::Voice(event));
                            }
                        });
                        continue;
                    }
                    Command::VoiceStop => {
                        if let Some(previous) = stop.take() {
                            let _ = previous.send(());
                        }
                        continue;
                    }
                    command => {
                let label = command.label();
                if let Err(err) = dispatch(command, &requester, &ev_tx, &cwd).await {
                    let _ = ev_tx.send(Event::Error {
                        context: label.to_string(),
                        message: format!("{err:#}"),
                    });
                }
                    }
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
            mode,
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
                        "mode": mode,
                    }),
                )
                .await?;
            let result: SessionResult = decode(value)?;
            send(events, Event::Created(result.session))
        }
        Command::CreateWorktree { session_id, name } => {
            let value = requester
                .request(
                    "session.worktree.create",
                    json!({ "session_id": session_id, "name": name }),
                )
                .await?;
            let result: SessionResult = decode(value)?;
            send(events, Event::Updated(result.session))
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
            send(events, Event::Reset { session_id })
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
        Command::SetMode {
            session_id,
            mode,
            temporary,
        } => {
            let value = requester
                .request(
                    "session.mode",
                    json!({
                        "session_id": session_id,
                        "mode": mode,
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
        Command::SetPlanModel { session_id, model } => {
            let value = requester
                .request(
                    "session.plan_model",
                    json!({ "session_id": session_id, "model": model }),
                )
                .await?;
            let result: SessionResult = decode(value)?;
            send(events, Event::Updated(result.session))
        }
        Command::UseModel { model } => {
            let value = requester
                .request("model.use", json!({ "model": model }))
                .await?;
            let result: eix_core::ModelUseResult = decode(value)?;
            send(events, Event::DefaultModel(result.model))
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
        Command::SetSubagentConcurrency { value } => {
            let value = requester
                .request(
                    "config.subagents",
                    json!({ "max_parallel_subagents": value }),
                )
                .await?;
            let result: eix_core::SubagentConcurrencyResult = decode(value)?;
            send(
                events,
                Event::SubagentConcurrency {
                    value: result.max_parallel_subagents,
                },
            )
        }
        Command::ListModels => {
            let value = requester.request("model.list", Value::Null).await?;
            let result: eix_core::ModelListResult = decode(value)?;
            send(
                events,
                Event::Models {
                    models: result.models,
                    warnings: result.warnings,
                    default_model: result.default_model,
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
        Command::SetMcpEnabled { name, enabled } => {
            requester
                .request(
                    "mcp.set_enabled",
                    json!({ "name": name, "enabled": enabled }),
                )
                .await?;
            // Re-list so tool counts and startup errors reflect the new state
            // instead of only flipping a local flag.
            let value = requester.request("mcp.list", Value::Null).await?;
            let result: eix_core::McpListResult = decode(value)?;
            send(events, Event::Mcp(result.servers))
        }
        Command::ListPluginCommands => {
            let value = requester.request("plugin.commands", Value::Null).await?;
            let result: eix_core::PluginCommandList = decode(value)?;
            send(events, Event::PluginCommands(result.commands))
        }
        Command::ListPlugins { name } => {
            let value = requester.request("plugin.list", Value::Null).await?;
            let list: eix_core::PluginListResult = decode(value)?;
            let surfaces = match requester.request("plugin.ui.list", Value::Null).await {
                Ok(value) => decode::<eix_core::PluginUiListResult>(value)?.surfaces,
                // Older daemons do not expose the surface list; the plugin
                // panels themselves still work through `plugin.ui.open`.
                Err(_) => Vec::new(),
            };
            let settings = match name {
                Some(name) => {
                    let value = requester
                        .request("plugin.settings", json!({ "name": name }))
                        .await?;
                    Some(decode(value)?)
                }
                None => None,
            };
            send(
                events,
                Event::Plugins {
                    list,
                    settings,
                    surfaces,
                },
            )
        }
        Command::PluginHealth => {
            let value = requester.request("plugin.list", Value::Null).await?;
            let list: eix_core::PluginListResult = decode(value)?;
            let mut failed = Vec::new();
            let mut degraded = Vec::new();
            for plugin in &list.plugins {
                match plugin.state {
                    eix_core::PluginState::Failed => failed.push(plugin.name.clone()),
                    eix_core::PluginState::Degraded => degraded.push(plugin.name.clone()),
                    _ => {}
                }
            }
            send(
                events,
                Event::PluginHealth {
                    total: list.plugins.len(),
                    failed,
                    degraded,
                },
            )
        }
        Command::PluginDoctor { name } => {
            let value = requester.request("plugin.doctor", Value::Null).await?;
            let mut result: eix_core::PluginDoctorResult = decode(value)?;
            if let Some(name) = name {
                result.plugins.retain(|entry| entry.name == name);
            }
            send(events, Event::PluginDoctor(result))
        }
        Command::PluginLogs { name } => {
            let value = requester
                .request("plugin.logs", json!({ "name": name, "limit": 200 }))
                .await?;
            let result: eix_core::PluginLogsResult = decode(value)?;
            send(events, Event::PluginLogs(result))
        }
        Command::PluginAction { action, name } => {
            let method = format!("plugin.{action}");
            let result = requester.request(&method, json!({ "name": name })).await;
            let (ok, message) = match result {
                Ok(value) => (true, value.to_string()),
                Err(err) => (false, format!("{err:#}")),
            };
            send(
                events,
                Event::PluginActionDone {
                    action,
                    name,
                    ok,
                    message,
                },
            )
        }
        Command::PluginSettingsSet { name, values } => {
            let result = requester
                .request(
                    "plugin.settings.set",
                    json!({ "name": name, "values": values }),
                )
                .await;
            let (ok, message) = match result {
                Ok(_) => (true, "settings updated".to_string()),
                Err(err) => (false, format!("{err:#}")),
            };
            send(
                events,
                Event::PluginActionDone {
                    action: "settings".into(),
                    name,
                    ok,
                    message,
                },
            )
        }
        Command::PluginUiRender {
            plugin,
            surface,
            id,
        } => {
            let value = requester
                .request(
                    "plugin.ui.render",
                    json!({ "plugin": plugin, "surface": surface, "id": id }),
                )
                .await?;
            let result: eix_core::PluginUiRenderResult = decode(value)?;
            let title = if result.title.is_empty() {
                id.clone()
            } else {
                result.title.clone()
            };
            send(
                events,
                Event::PluginUiPanel {
                    plugin,
                    id,
                    surface,
                    title,
                    result,
                },
            )
        }
        Command::PluginUiAction {
            plugin,
            id,
            action,
            values,
        } => {
            let value = requester
                .request(
                    "plugin.ui.action",
                    json!({
                        "plugin": plugin,
                        "id": id,
                        "action": action,
                        "values": values,
                    }),
                )
                .await?;
            let result: eix_core::PluginUiActionResult = decode(value)?;
            send(
                events,
                Event::PluginUiActionDone {
                    plugin,
                    id,
                    action,
                    result,
                },
            )
        }
        Command::PluginPick { plugin, id, query } => {
            let value = requester
                .request(
                    "plugin.ui.pick",
                    json!({ "plugin": plugin, "id": id, "query": query }),
                )
                .await?;
            let result: eix_core::PluginUiPickResult = decode(value)?;
            send(
                events,
                Event::PluginPicked {
                    plugin,
                    id,
                    query,
                    items: result.items,
                },
            )
        }
        Command::RunPluginCommand {
            session_id,
            name,
            arguments,
        } => {
            let value = requester
                .request(
                    "plugin.command.run",
                    json!({
                        "name": name,
                        "arguments": arguments,
                        "session_id": session_id,
                    }),
                )
                .await?;
            let result: eix_core::PluginCommandRunResult = decode(value)?;
            send(events, Event::PluginCommandResult { session_id, result })
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
            let value = requester
                .request(method, json!({ "session_id": session_id }))
                .await?;
            let result: eix_core::UndoResult = decode(value)?;
            let mut note = if redo {
                format!("{} event(s) restored", result.changed)
            } else {
                format!("{} event(s) hidden", result.changed)
            };
            if result.files_restored > 0 || result.files_removed > 0 {
                note.push_str(&format!(
                    " · {} file(s) restored, {} removed",
                    result.files_restored, result.files_removed
                ));
            }
            if let Some(warning) = result.warnings.first() {
                note.push_str(&format!(" · {warning}"));
            }
            send(events, Event::Rewritten { session_id, note })
        }
        Command::Rewind {
            session_id,
            at_seq,
            scope,
        } => {
            let value = requester
                .request(
                    "session.rewind_to",
                    json!({ "session_id": session_id, "at_seq": at_seq, "scope": scope }),
                )
                .await?;
            let result: eix_core::RewindToResult = decode(value)?;
            let mut note = format!("rewound to e{}", result.at_seq);
            if scope.restores_conversation() {
                note.push_str(&format!(" · {} event(s) hidden", result.changed));
            }
            if scope.restores_files() && (result.files_restored > 0 || result.files_removed > 0) {
                note.push_str(&format!(
                    " · {} file(s) restored, {} removed",
                    result.files_restored, result.files_removed
                ));
            }
            if let Some(warning) = result.warnings.first() {
                note.push_str(&format!(" · {warning}"));
            }
            send(events, Event::Rewritten { session_id, note })
        }
        Command::ContextInfo { session_id, all } => {
            let value = requester
                .request("context.info", json!({ "session_id": session_id }))
                .await?;
            let info: eix_core::ContextLedgerResult = decode(value)?;
            send(
                events,
                Event::ContextInfo {
                    session_id,
                    all,
                    info,
                },
            )
        }
        Command::WorkspaceDiff { session_id, staged } => {
            let value = requester
                .request(
                    "session.diff",
                    json!({ "session_id": session_id, "staged": staged }),
                )
                .await?;
            let result: eix_core::WorkspaceDiffResult = decode(value)?;
            send(events, Event::WorkspaceDiff { result, staged })
        }
        Command::Doctor => {
            let value = requester.request("doctor.report", Value::Null).await?;
            let report: eix_core::DoctorReport = decode(value)?;
            send(events, Event::Doctor(report.lines))
        }
        Command::RenameSession { session_id, name } => {
            let value = requester
                .request(
                    "session.rename",
                    json!({ "session_id": session_id, "name": name }),
                )
                .await?;
            let result: eix_core::SessionResult = decode(value)?;
            send(events, Event::Updated(result.session))
        }
        Command::Goal {
            session_id,
            text,
            done,
            clear,
        } => {
            let value = requester
                .request(
                    "session.goal",
                    json!({
                        "session_id": session_id,
                        "text": text,
                        "done": done,
                        "clear": clear,
                    }),
                )
                .await?;
            let result: eix_core::GoalResult = decode(value)?;
            send(events, Event::Goal(result.goal))
        }
        Command::ListSchedules { session_id } => {
            let value = requester
                .request("schedule.list", json!({ "session_id": session_id }))
                .await?;
            let result: eix_core::ScheduleListResult = decode(value)?;
            send(
                events,
                Event::Schedules {
                    session_id,
                    schedules: result.schedules,
                },
            )
        }
        Command::AddSchedule {
            session_id,
            kind,
            prompt,
            interval_secs,
            at,
        } => {
            let value = requester
                .request(
                    "schedule.add",
                    json!({
                        "session_id": session_id,
                        "kind": kind,
                        "prompt": prompt,
                        "interval_secs": interval_secs,
                        "at": at,
                    }),
                )
                .await?;
            let schedule: eix_core::Schedule = decode(value)?;
            send(events, Event::ScheduleAdded(schedule))
        }
        Command::RemoveSchedules {
            id,
            session_id,
            kind,
        } => {
            let value = requester
                .request(
                    "schedule.remove",
                    json!({ "id": id, "session_id": session_id, "kind": kind }),
                )
                .await?;
            let result: eix_core::ScheduleRemoveResult = decode(value)?;
            send(
                events,
                Event::SchedulesRemoved {
                    removed: result.removed,
                },
            )
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
            let max_parallel_subagents = value["max_parallel_subagents"]
                .as_u64()
                .map(|value| value as usize)
                .unwrap_or(eix_core::DEFAULT_MAX_PARALLEL_SUBAGENTS);
            let plugin_commands = requester
                .request("plugin.commands", Value::Null)
                .await
                .ok()
                .and_then(|value| decode::<eix_core::PluginCommandList>(value).ok())
                .map(|result| result.commands)
                .unwrap_or_default();
            send(
                events,
                Event::ConfigReloaded {
                    paths,
                    max_parallel_subagents,
                },
            )?;
            send(events, Event::PluginCommands(plugin_commands))
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
            answers,
        } => {
            let value = requester
                .request(
                    "input.respond",
                    json!({
                        "session_id": session_id,
                        "input_id": input_id,
                        "answers": answers,
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
            branch,
        } => {
            let value = requester
                .request(
                    "session.fork",
                    json!({ "session_id": session_id, "at_seq": at_seq, "name": name }),
                )
                .await?;
            let result: SessionResult = decode(value)?;
            if branch {
                send(events, Event::Branched(result.session))
            } else {
                send(events, Event::Forked(result.session))
            }
        }
        Command::PickerData {
            session_id,
            kind,
            query,
        } => {
            let value = requester
                .request(
                    "picker.query",
                    json!({
                        "kind": kind,
                        "session_id": session_id,
                        "query": query,
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
                    query,
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
        // Dictation is driven by `run` directly: it owns the capture task and
        // its stop handle, so there is nothing to dispatch here.
        Command::VoiceStart { .. } | Command::VoiceStop => Ok(()),
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).context("unexpected daemon response")
}

fn send(events: &UnboundedSender<Event>, event: Event) -> Result<()> {
    events.send(event).map_err(|_| anyhow::anyhow!("ui closed"))
}
