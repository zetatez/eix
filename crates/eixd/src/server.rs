//! Unix-socket JSON-RPC server and the agent turn loop.
//!
//! A turn is: append the user event -> mark running -> stream provider deltas ->
//! append the assistant event -> mark idle. All state changes are written to
//! SQLite before they are broadcast, so a frontend can always recover by
//! re-attaching and replaying the event log.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crate::blob::BlobStore;
use crate::lsp::LspSet;
use crate::memory::MemoryStore;
use anyhow::{bail, Context, Result};
use base64::Engine as _;
use eix_core::{
    now_ms, payload, AgentListResult, ApprovalDecision, ApprovalInfo, ApprovalListParams,
    ApprovalListResult, ApprovalRequiredPayload, ApprovalRespondParams, ApprovalResult,
    ApprovalScope, AttachParams, AttachResult, AuthParams, AutoReady, BtwParams, BtwResult,
    CancelResult, ChatMessage, Config, ContextClearParams, ContextClearedPayload,
    ContextCompactParams, ContextCompactedPayload, CreateSessionParams, DraftParams, DraftResult,
    ErrorPayload, Event, EventKind, EventsParams, EventsResult, ForkParams, FunctionCall,
    HelloResult, InputAnsweredPayload, InputOption, InputRequiredPayload, InputRespondParams,
    InputResult, IntegrationContinueParams, IntegrationListParams, IntegrationListResult,
    IntegrationState, IntegrationStatus, LspWorkspaceParams, McpListResult, McpServerInfo,
    ModelListResult, Notification, PatchApplyParams, PatchGetParams, PatchGetResult,
    PatchProposedPayload, PatchRejectParams, PatchResult, PatchResultPayload, PatchReviseParams,
    PermissionAction, PermissionLevel, PermissionParams, PermissionPayload, PermissionRule,
    PickerParams, PickerPreviewParams, PickerPreviewResult, ProcessKillParams, ProcessKillResult,
    ProcessList, ProcessListParams, ProcessOutputParams, RenameParams, Request, Response,
    RewindParams, RewindResult, RuleSource, SearchParams, SearchResult, ServerMessage, Session,
    SessionAgentParams, SessionArchiveParams, SessionCdParams, SessionExport, SessionExportResult,
    SessionId, SessionImportParams, SessionList, SessionModelParams, SessionOrigin,
    SessionReadyParams, SessionRef, SessionResult, SessionStatus, ShellRunParams, ShellRunResult,
    SnapshotPayload, StatsResult, StatusChangedPayload, SubagentConcurrencyParams, SubmitParams,
    SubmitResult, SubscribeParams, SubscribeResult, TimelineItem, TimelineResult, ToolCall,
    ToolCallPayload, ToolDefinition, ToolResultPayload, UserMessagePayload, WorktreeRemoveParams,
    WorktreeRemoveResult, PROTOCOL_VERSION, SESSION_EXPORT_VERSION,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock, Semaphore};
use uuid::Uuid;

use crate::context::{estimate_request_tokens, estimate_tool_schema_tokens, Ledger, StaticContext};
use crate::permissions::{decide as permission_decide, PermissionDecision};
use crate::process::ProcessRegistry;
use crate::provider::{Provider, ProviderError, RetryClass};
use crate::snapshot::{Scope, Snapshotter};
use crate::staging::{StagedPatch, Staging};
use crate::store::Store;
use crate::tools;
use eix_core::redact::redact;

/// `stage` marker used while the agent's previous turn is still running.
/// The idle scanner later turns this into a real repair prompt.
const REPAIR_PENDING_STAGE: &str = "repair pending: ";

/// Keep integration notes useful without letting a huge test log fill the
/// transcript. The repair prompt still carries its own (larger) failure tail.
const INTEGRATION_LOG_CHARS: usize = 8_000;

fn render_command_log(log: &crate::worktree::CommandLog) -> String {
    let command = format!("$ {}", log.command);
    let output = log.output.trim();
    if output.is_empty() {
        command
    } else {
        format!(
            "{command}\n{}",
            crate::integration::tail(output, INTEGRATION_LOG_CHARS)
        )
    }
}

fn render_command_logs(logs: &[crate::worktree::CommandLog]) -> String {
    if logs.is_empty() {
        return "(no remote commands; using the local base)".to_string();
    }
    logs.iter()
        .map(render_command_log)
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn render_rebase_recovery(recovery: &crate::worktree::RebaseRecovery) -> String {
    let short = |value: &str| value.chars().take(12).collect::<String>();
    let list = |items: &[String]| {
        if items.is_empty() {
            return "none".to_string();
        }
        let max = 8;
        let mut text = items
            .iter()
            .take(max)
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        if items.len() > max {
            text.push_str(&format!(" (+{} more)", items.len() - max));
        }
        text
    };
    let mut text = format!(
        "integration recovered · stale rebase aborted\n\
         worktree: {}\n\
         branch: {}\n\
         onto: {}\n\
         HEAD: {}\n\
         conflicts: {}\n\
         staged: {}\n\
         unstaged: {}\n\
         backup: {}\n\
         backup ref: {}",
        recovery.worktree.display(),
        recovery.branch.as_deref().unwrap_or("(detached)"),
        recovery
            .onto
            .as_deref()
            .map(short)
            .unwrap_or_else(|| "(unknown)".into()),
        short(&recovery.head),
        list(&recovery.conflicts),
        list(&recovery.staged),
        list(&recovery.unstaged),
        recovery.backup_dir.display(),
        recovery.backup_ref.as_deref().unwrap_or("(none)"),
    );
    if let Some(patch) = recovery.backup_patch.as_deref() {
        text.push_str(&format!("\npatch: {}", patch.display()));
    }
    if let Some(files) = recovery.backup_files.as_deref() {
        text.push_str(&format!("\nfiles: {}", files.display()));
    }
    if !recovery.untracked.is_empty() {
        text.push_str(&format!(
            "\nuntracked files kept in place: {}",
            list(&recovery.untracked)
        ));
    }
    if recovery.had_work {
        text.push_str(
            "\n\nAny conflict resolution or staged edits were preserved in the backup above; the worktree was restored to the pre-rebase branch and integration continues without them. Reapply the backup if those edits were meant to land.",
        );
    } else {
        text.push_str("\n\nNo tracked rebase work was present; integration can continue.");
    }
    text
}

pub async fn run(config: Config, socket: PathBuf, data_dir: PathBuf) -> Result<()> {
    fs::create_dir_all(&data_dir)
        .with_context(|| format!("cannot create data dir {}", data_dir.display()))?;
    Store::set_permissions(&data_dir)?;

    let db_path = data_dir.join("eix.sqlite3");
    let store = Arc::new(Store::open(&db_path)?);
    Store::set_permissions(&db_path)?;
    let provider = arc_swap::ArcSwap::from_pointee(
        Provider::new(&config).map_err(|e| anyhow::anyhow!("{e}"))?,
    );
    let (bus, _) = broadcast::channel(2048);
    let snapshots = Snapshotter::new(data_dir.join("snapshots"));
    let staging = Staging::new(data_dir.join("staging"));
    let processes = Arc::new(ProcessRegistry::new(
        Arc::clone(&store),
        bus.clone(),
        data_dir.clone(),
    ));
    let blobs = BlobStore::new(data_dir.join("blobs"), config.images.clone());
    let subagent_concurrency = config.tools.max_parallel_subagents;
    let daemon = Arc::new(Daemon {
        store,
        provider,
        config: arc_swap::ArcSwap::from_pointee(config),
        subagent_concurrency: AtomicUsize::new(subagent_concurrency),
        bus,
        blobs,
        memory: MemoryStore::new(data_dir.join("memory")),
        started: Instant::now(),
        snapshots,
        staging,
        processes,
        mcp: Mutex::new(crate::mcp::McpSet::new()),
        lsp: Mutex::new(crate::lsp::LspSet::new()),
        todos: Mutex::new(HashMap::new()),
        turns: Mutex::new(HashMap::new()),
        worktree_creating: Mutex::new(HashSet::new()),
        integration_lock: Mutex::new(()),
        approvals: Mutex::new(HashMap::new()),
        always: Mutex::new(HashMap::new()),
        inputs: Mutex::new(HashMap::new()),
        btws: Mutex::new(HashMap::new()),
        notified: std::sync::Mutex::new(HashMap::new()),
        temporary_permissions: std::sync::Mutex::new(HashMap::new()),
        skill_tools: Mutex::new(HashMap::new()),
        token_scales: std::sync::Mutex::new(HashMap::new()),
        model_catalog: Mutex::new(ModelCatalogCache::default()),
    });
    let restored = daemon.staging.load();
    if restored > 0 {
        eprintln!("eixd: restored {restored} staged patch(es)");
    }
    daemon.recover().await;
    // One queue owner per daemon. It serializes merges so a session branch is
    // never rebased while another session is updating the base worktree.
    let integration = Arc::clone(&daemon);
    tokio::spawn(async move {
        integration.integration_loop().await;
    });
    if let Ok(report) = daemon.gc_blobs().await {
        if report.removed > 0 {
            eprintln!(
                "eixd: removed {} orphaned image blob(s), {} bytes",
                report.removed, report.removed_bytes
            );
        }
    }
    // Retention and the size cap should also apply to a long-running daemon,
    // not only on restart/session deletion.
    let blob_gc = Arc::clone(&daemon);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(6 * 60 * 60)).await;
            match blob_gc.gc_blobs().await {
                Ok(report) if report.removed > 0 => eprintln!(
                    "eixd: removed {} orphaned image blob(s), {} bytes",
                    report.removed, report.removed_bytes
                ),
                Ok(_) => {}
                Err(err) => eprintln!("eixd: image blob gc failed: {err:#}"),
            }
        }
    });

    // Hot reload: poll the files the config was assembled from. A parse error
    // keeps the old config and surfaces on `config.error`; the next edit
    // retries. Polling (instead of inotify) keeps the dependency footprint
    // small and works on every platform.
    if !daemon.config.load().watch_paths.is_empty() {
        let watcher = Arc::clone(&daemon);
        tokio::spawn(async move {
            let mut last = watcher.config_mtimes();
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                let now = watcher.config_mtimes();
                if now == last {
                    continue;
                }
                match watcher.reload_config().await {
                    Ok(paths) => {
                        watcher.emit_value(
                            "config.reloaded",
                            json!({
                                "paths": paths,
                                "max_parallel_subagents": watcher
                                    .subagent_concurrency
                                    .load(Ordering::Relaxed),
                            }),
                        );
                        eprintln!("eixd: config reloaded");
                    }
                    Err(err) => {
                        watcher.emit_value("config.error", json!({ "error": err.to_string() }));
                        eprintln!("eixd: config reload failed: {err:#}");
                    }
                }
                last = watcher.config_mtimes();
            }
        });
    }

    prepare_socket(&socket)?;
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("cannot bind {}", socket.display()))?;
    Store::set_permissions(&socket)?;

    eprintln!(
        "eixd {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        socket.display()
    );
    eprintln!("  model  {}", daemon.config.load().default_model());
    eprintln!(
        "  api    {}",
        redact(&daemon.config.load().default_provider().base_url)
    );
    eprintln!("  data   {}", db_path.display());

    // Optional HTTP transport. Like TCP it is off unless configured and every
    // route except `/health` requires the shared token.
    let http_addr = daemon
        .config
        .load()
        .server
        .http
        .as_deref()
        .map(str::trim)
        .filter(|addr| !addr.is_empty())
        .map(str::to_string);
    if let Some(addr) = http_addr {
        let token = daemon
            .config
            .load()
            .server
            .token
            .clone()
            .unwrap_or_default();
        if token.trim().is_empty() {
            bail!("server.http is configured but server.token is empty");
        }
        let listener = TcpListener::bind(&addr)
            .await
            .with_context(|| format!("cannot bind http {addr}"))?;
        eprintln!("  http   {} (token required)", listener.local_addr()?);
        let daemon = Arc::clone(&daemon);
        tokio::spawn(async move {
            crate::http::serve(daemon, listener, token).await;
        });
    }

    // Optional remote attach. Off unless configured, because a TCP port does
    // not have the Unix socket's filesystem permissions; every connection must
    // authenticate with `server.token` before any other request is answered.
    let tcp_addr = daemon
        .config
        .load()
        .server
        .tcp
        .as_deref()
        .map(str::trim)
        .filter(|addr| !addr.is_empty())
        .map(str::to_string);
    if let Some(addr) = tcp_addr {
        let token = daemon
            .config
            .load()
            .server
            .token
            .clone()
            .unwrap_or_default();
        if token.trim().is_empty() {
            bail!("server.tcp is configured but server.token is empty");
        }
        let tcp = TcpListener::bind(&addr)
            .await
            .with_context(|| format!("cannot bind tcp {addr}"))?;
        eprintln!("  tcp    {} (token required)", tcp.local_addr()?);
        let daemon = Arc::clone(&daemon);
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = tcp.accept().await else {
                    break;
                };
                let daemon = Arc::clone(&daemon);
                let token = token.clone();
                tokio::spawn(async move {
                    daemon.serve(stream, Transport::Tcp, Some(token)).await;
                });
            }
        });
    }

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accept failed")?;
                let daemon = Arc::clone(&daemon);
                tokio::spawn(async move {
                    daemon.serve(stream, Transport::Unix, None).await;
                });
            }
        }
    }

    let _ = fs::remove_file(&socket);
    // Background processes are children of this daemon; take them with us.
    daemon.processes.kill_all().await;
    Ok(())
}

/// Remove a stale socket, but never steal one from a live daemon.
fn prepare_socket(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("cannot create socket dir {}", parent.display()))?;
        Store::set_permissions(parent)?;
    }
    if path.exists() {
        match std::os::unix::net::UnixStream::connect(path) {
            Ok(_) => bail!(
                "another eixd is already listening at {} (set EIX_SOCKET to use a different one)",
                path.display()
            ),
            Err(_) => {
                fs::remove_file(path)
                    .with_context(|| format!("cannot remove stale socket {}", path.display()))?;
            }
        }
    }
    Ok(())
}

pub(crate) struct Daemon {
    store: Arc<Store>,
    provider: arc_swap::ArcSwap<Provider>,
    config: arc_swap::ArcSwap<Config>,
    /// Runtime cap for concurrent `spawn_agent` calls. Initialized from
    /// `[tools] max_parallel_subagents` and reset by config reload; the TUI can
    /// override it until the next reload or daemon restart.
    subagent_concurrency: AtomicUsize,
    pub(crate) bus: broadcast::Sender<Notification>,
    /// Prompt images, content-addressed on disk.
    blobs: BlobStore,
    /// Cross-session project notes, injected as a fixed context sector.
    memory: MemoryStore,
    started: Instant,
    /// Pre-execution snapshots for mutating tools.
    snapshots: Snapshotter,
    /// Staged `write_file` proposals, the session shadow workspace.
    staging: Staging,
    /// Background processes started with `start_command`.
    processes: Arc<ProcessRegistry>,
    /// Lazily started MCP servers and their tools.
    mcp: Mutex<crate::mcp::McpSet>,
    lsp: Mutex<crate::lsp::LspSet>,
    /// Model-managed task lists, keyed by session. In-memory: a daemon
    /// restart clears them.
    todos: Mutex<HashMap<SessionId, Vec<eix_core::TodoItem>>>,
    /// In-flight turns, used for busy checks and cancellation.
    turns: Mutex<HashMap<SessionId, Arc<AtomicBool>>>,
    /// Sessions whose first prompt is currently creating a worktree, so two
    /// concurrent prompts cannot create two branches.
    worktree_creating: Mutex<HashSet<SessionId>>,
    /// Serializes "queue claims a ready item" against "prompt resets the item
    /// to pending and registers a turn". Without it the rebase can begin in
    /// the gap between those two operations.
    integration_lock: Mutex<()>,
    /// Tool calls blocked on a user decision, keyed by session.
    approvals: Mutex<HashMap<SessionId, PendingApproval>>,
    /// Session-scoped rules created by "Always allow" on the approval card.
    always: Mutex<HashMap<SessionId, Vec<PermissionRule>>>,
    /// `ask_user` questions blocked on an answer, keyed by session.
    inputs: Mutex<HashMap<SessionId, PendingInput>>,
    /// In-flight `/btw` side questions, keyed by session (for cancellation).
    btws: Mutex<HashMap<SessionId, Arc<AtomicBool>>>,
    /// Last attention notification per session, for the cooldown.
    notified: std::sync::Mutex<HashMap<SessionId, Instant>>,
    /// In-memory permission overrides set with `temporary: true`. They last
    /// until the daemon restarts or the level is changed again.
    temporary_permissions: std::sync::Mutex<HashMap<SessionId, PermissionLevel>>,
    /// Tools allowed by skills loaded with `use_skill`, unioned per session.
    /// Cleared when a new user prompt starts.
    skill_tools: Mutex<HashMap<SessionId, Vec<String>>>,
    /// Per-model multiplier that calibrates local token estimates against
    /// provider-reported prompt usage. Starts at 1.0 and is updated with a
    /// bounded EWMA after each completed request.
    token_scales: std::sync::Mutex<HashMap<String, f64>>,
    /// Cached merged model catalog. `/model` may be opened repeatedly; don't
    /// hit every provider on each open.
    model_catalog: Mutex<ModelCatalogCache>,
}

#[derive(Default)]
struct ModelCatalogCache {
    fetched_at: Option<Instant>,
    models: Vec<eix_core::ModelInfo>,
    warnings: Vec<String>,
}

struct PendingApproval {
    approval_id: String,
    tool: String,
    summary: String,
    created_ms: i64,
    /// High/critical risk calls cannot become unlimited "Always" rules.
    allow_always: bool,
    responder: oneshot::Sender<ApprovalReply>,
}

struct PendingInput {
    input_id: String,
    /// Number of valid options, so an out-of-range answer is rejected instead
    /// of silently cancelling the turn.
    options: usize,
    responder: oneshot::Sender<usize>,
}

#[derive(Debug, Clone)]
struct ApprovalReply {
    decision: ApprovalDecision,
    reason: Option<String>,
    /// Write plan captured when the diff was shown (write_file only).
    plan: Option<tools::WritePlan>,
    /// Hunk indices the user selected; `None` means the whole change.
    hunks: Option<Vec<usize>>,
    /// Where an "Always" decision is remembered.
    scope: ApprovalScope,
}

impl Daemon {
    fn emit<T: Serialize>(&self, method: &str, params: &T) {
        let _ = self.bus.send(Notification::new(method, params));
    }

    fn emit_value(&self, method: &str, params: Value) {
        let _ = self.bus.send(Notification::new(method, params));
    }

    fn emit_event(&self, event: &Event) {
        self.emit("session.event", event);
    }

    fn emit_session(&self, session: &Session) {
        self.emit("session.updated", session);
    }

    fn emit_integration(&self, state: &IntegrationState) {
        self.emit("integration.updated", state);
    }

    fn save_integration(&self, state: &IntegrationState) -> Result<()> {
        self.store.save_integration(state)?;
        self.emit_integration(state);
        Ok(())
    }

    /// Persist a human-readable integration milestone in the session log. The
    /// queue is otherwise invisible in the transcript until it injects a
    /// repair turn.
    fn note_integration(&self, session_id: &str, status: IntegrationStatus, text: String) {
        if text.trim().is_empty() {
            return;
        }
        let payload = json!({
            "status": status.as_str(),
            "text": text,
        });
        if let Ok(event) = self
            .store
            .append_event(session_id, EventKind::Integration, payload)
        {
            self.emit_event(&event);
        }
    }

    async fn base_for_repo(&self, repo: &Path) -> Result<String> {
        let config = self.config.load_full();
        let configured = config
            .integration
            .repo_base(repo)
            .unwrap_or(&config.worktree.base)
            .to_string();
        crate::worktree::resolve_base(repo, &configured).await
    }

    /// Base for a new session: the branch checked out in the directory where
    /// the user started it. Explicit `[worktree] base` / per-repo `base`
    /// settings still win; `auto` now means "this branch", not "main/master".
    async fn base_for_session(&self, dir: &Path, repo: &Path) -> Result<String> {
        let config = self.config.load_full();
        let configured = config
            .integration
            .repo_base(repo)
            .unwrap_or(&config.worktree.base)
            .to_string();
        let configured = configured.trim();
        if configured.is_empty() || configured == "auto" {
            if let Some(branch) = crate::worktree::branch(dir).await {
                return Ok(branch);
            }
        }
        crate::worktree::resolve_base(repo, configured).await
    }

    /// Name a worktree from the user's task text. The model call is on the
    /// first prompt's critical path, so provider failure falls back to an
    /// ASCII task slug rather than blocking the turn.
    async fn generate_worktree_name(&self, session: &Session, text: &str) -> String {
        let fallback = crate::integration::fallback_worktree_name(text);
        let config = self.config.load_full();
        let model = session
            .model
            .clone()
            .unwrap_or_else(|| config.default_model().to_string());
        let route = config.route(&model);
        let messages = vec![
            ChatMessage::system(
                "You name git worktrees for coding tasks. Reply with only a short English name. Use 2-5 lowercase ASCII words joined by hyphens, at most 48 characters. No punctuation, quotes, explanation, or non-ASCII characters.",
            ),
            ChatMessage::user(format!(
                "User task:\n\n{}\n\nReturn only the worktree name.",
                cap_text(text.trim(), 4_000)
            )),
        ];
        let cancel = AtomicBool::new(false);
        let provider = self.provider.load_full();
        match provider
            .stream_chat(&messages, &route, &[], &cancel, |_| {})
            .await
        {
            Ok(completion) => {
                let name = crate::integration::clean_worktree_name(&completion.text);
                if name.is_empty() {
                    fallback
                } else {
                    name
                }
            }
            Err(_) => fallback,
        }
    }

    async fn ensure_auto_worktree(&self, session_id: &str, text: &str) -> Result<Session> {
        let session = self.require_session(session_id)?;
        if session.is_agent() || session.has_worktree() {
            return Ok(session);
        }
        if !self.config.load().worktree.auto {
            return Ok(session);
        }
        // Only sessions created with the auto-worktree policy participate.
        // Legacy rows (and imported sessions) have no integration row; moving
        // them into a new worktree on their next prompt would be surprising.
        match self.store.get_integration(session_id)? {
            Some(state) if state.worktree_disabled => return Ok(session),
            Some(_) => {}
            None => return Ok(session),
        }

        // Two prompts submitted at once must not create two branches. Claim
        // the session, then re-read it so a completed first creation wins.
        {
            let mut creating = self.worktree_creating.lock().await;
            if !creating.insert(session.id.clone()) {
                return Err(rpc(
                    "busy",
                    format!("session {} is creating its worktree", session.id),
                ));
            }
        }
        let result = async {
            let session = self.require_session(session_id)?;
            if session.is_agent() || session.has_worktree() {
                return Ok(session);
            }
            let Some(repo) = crate::worktree::main_repo(&session.cwd).await else {
                return Ok(session);
            };
            // Recorded when the session was created, before the user could
            // switch branches. Legacy rows have no stored base, so resolve it
            // at first prompt.
            let base = match self
                .store
                .get_integration(&session.id)?
                .and_then(|state| state.base_branch)
            {
                Some(base) => base,
                None => self.base_for_session(&session.cwd, &repo).await?,
            };
            let name = self.generate_worktree_name(&session, text).await;
            let created =
                crate::worktree::create_from_base(&session.cwd, &name, Some(&base)).await?;
            let cwd = worktree_session_cwd(&repo, &session.cwd, &created.path);
            let updated = self
                .store
                .set_session_cwd(&session.id, &cwd, Some(&created.path))?
                .ok_or_else(|| rpc("not_found", format!("unknown session {}", session.id)))?;
            let state = self
                .store
                .ensure_integration(&session.id, Some(&base), false)?;
            self.save_integration(&state)?;
            self.emit_session(&updated);
            Ok(updated)
        }
        .await;
        self.worktree_creating.lock().await.remove(session_id);
        result
    }

    /// Back up and remove a stale rebase so integration never stays wedged.
    /// Returns `Ok(None)` when there is no rebase to recover.
    async fn recover_stale_rebase(
        &self,
        session: &Session,
        worktree: &Path,
        status: IntegrationStatus,
    ) -> Result<Option<crate::worktree::RebaseRecovery>> {
        let Some(recovery) = crate::worktree::recover_rebase(worktree).await? else {
            return Ok(None);
        };
        self.note_integration(&session.id, status, render_rebase_recovery(&recovery));
        Ok(Some(recovery))
    }

    async fn request_ready(
        &self,
        session_id: &str,
        auto_commit: Option<bool>,
    ) -> Result<IntegrationState> {
        self.request_ready_inner(session_id, auto_commit, false)
            .await
    }

    async fn request_ready_inner(
        &self,
        session_id: &str,
        auto_commit: Option<bool>,
        allow_busy: bool,
    ) -> Result<IntegrationState> {
        let config = self.config.load_full();
        let session = self.require_session(session_id)?;
        if !allow_busy
            && (self.turns.lock().await.contains_key(session_id) || session.status.is_busy())
        {
            return Err(rpc(
                "busy",
                format!("session {} is still running", session.id),
            ));
        }
        let Some(worktree) = session.worktree.clone() else {
            return Err(rpc(
                "invalid_params",
                format!("session {} has no worktree", session.id),
            ));
        };
        // Do not race the queue's ready -> syncing claim. Holding this across
        // the commit below is cheap and keeps a late `ready` from overwriting
        // an active rebase/test state.
        let _claim = self.integration_lock.lock().await;
        let mut state = self
            .store
            .get_integration(session_id)?
            .unwrap_or_else(|| IntegrationState::new(&session.id));
        let previous_status = state.status;
        // Never touch a worktree that the queue is actively rebasing/testing/
        // merging. `Repairing` is different: the agent may have gone idle
        // without finishing, so an explicit ready is allowed to recover it.
        if state.status.is_active() && state.status != IntegrationStatus::Repairing {
            return Err(rpc(
                "busy",
                format!(
                    "session {} integration is {}",
                    session.id,
                    state.status.as_str()
                ),
            ));
        }
        if crate::worktree::in_rebase(&worktree).await {
            // An agent calling the `ready` tool may be in the middle of
            // resolving a conflict. Refuse to throw that work away; send it
            // back to finish or abort explicitly. A user/CLI `/ready` (and an
            // agent call with no tracked work) recovers automatically, with a
            // backup, so a stale rebase can never wedge the queue forever.
            if allow_busy && crate::worktree::rebase_has_work(&worktree).await {
                let conflicts = crate::worktree::conflict_files(&worktree).await;
                let branch = crate::worktree::branch(&worktree)
                    .await
                    .unwrap_or_else(|| "(detached)".into());
                let files = if conflicts.is_empty() {
                    "(none; tracked edits are staged or unstaged)".to_string()
                } else {
                    conflicts.join("\n")
                };
                return Err(rpc(
                    "invalid_params",
                    format!(
                        "a rebase is still in progress in {}\n\nBranch: {branch}\nConflicts:\n{files}\n\n\
                         Resolve the conflicts, stage them with `git add <files>`, then run\n\
                         `GIT_EDITOR=true git rebase --continue` until the rebase finishes.\n\
                         To discard the rebase instead, run `git rebase --abort`. Then call ready again.",
                        worktree.display()
                    ),
                ));
            }
            if let Err(err) = self
                .recover_stale_rebase(&session, &worktree, state.status)
                .await
            {
                return Err(rpc(
                    "invalid_params",
                    format!(
                        "cannot recover the in-progress rebase in {}: {err:#}",
                        worktree.display()
                    ),
                ));
            }
        }
        let base = match state.base_branch.clone() {
            Some(base) => base,
            None => {
                let repo = crate::worktree::main_repo(&worktree)
                    .await
                    .unwrap_or_else(|| session.cwd.clone());
                self.base_for_repo(&repo).await?
            }
        };
        let do_commit = auto_commit.unwrap_or(config.integration.auto_commit);
        if !crate::worktree::is_clean(&worktree).await? {
            if !do_commit {
                return Err(rpc(
                    "invalid_params",
                    "worktree has uncommitted changes; commit them or enable integration.auto_commit",
                ));
            }
            if self.session_permission(&session) == PermissionLevel::ReadOnly {
                return Err(rpc(
                    "permission_denied",
                    "read-only sessions cannot auto-commit worktree changes",
                ));
            }
            let message = format!(
                "eix: {}\n\nEix-Session: {}",
                session.name.trim(),
                session.id
            );
            crate::worktree::commit_all(&worktree, &message).await?;
        }
        state.status = IntegrationStatus::Ready;
        state.stage = None;
        state.base_branch = Some(base);
        state.error = None;
        state.next_retry_at = None;
        state.ready_at = Some(now_ms());
        // Keep the attempt counter across repair/requeue cycles so repeated
        // failures still trigger notifications and `pause_after`. A new user
        // prompt intentionally starts a fresh cycle; `integration.continue`
        // is the explicit reset for a paused queue.
        state.worktree_disabled = false;
        state.updated_at = now_ms();
        self.save_integration(&state)?;
        if previous_status != IntegrationStatus::Ready {
            let base = state.base_branch.as_deref().unwrap_or("auto");
            self.note_integration(
                &session.id,
                IntegrationStatus::Ready,
                format!("integration queued: base {base}"),
            );
        }
        Ok(state)
    }

    fn integration_continue(&self, session_id: &str) -> Result<IntegrationState> {
        let mut state = self.store.get_integration(session_id)?.ok_or_else(|| {
            rpc(
                "not_found",
                format!("no integration state for {session_id}"),
            )
        })?;
        state.status = IntegrationStatus::Ready;
        state.stage = None;
        state.error = None;
        state.next_retry_at = None;
        state.ready_at = Some(now_ms());
        state.attempts = 0;
        state.updated_at = now_ms();
        self.save_integration(&state)?;
        self.note_integration(
            session_id,
            IntegrationStatus::Ready,
            "integration requeued".into(),
        );
        Ok(state)
    }

    /// Called by the queue loop. It never performs a second concurrent merge:
    /// one daemon task owns the queue and processes one repository item at a
    /// time.
    async fn process_next_integration(self: &Arc<Self>) -> Result<()> {
        // Serialize the ready -> syncing decision with prompt submission.
        let claim = self.integration_lock.lock().await;
        let Some(mut state) = self.store.next_integration(now_ms())? else {
            return Ok(());
        };
        let session = self.require_session(&state.session_id)?;
        if session.archived {
            state.status = IntegrationStatus::Pending;
            state.error = Some("session archived".into());
            state.next_retry_at = None;
            state.updated_at = now_ms();
            self.save_integration(&state)?;
            return Ok(());
        }
        if session.status.is_busy() || self.turns.lock().await.contains_key(&session.id) {
            state.next_retry_at = Some(now_ms() + 10_000);
            state.updated_at = now_ms();
            self.save_integration(&state)?;
            return Ok(());
        }
        let Some(worktree) = session.worktree.clone() else {
            state.status = IntegrationStatus::Pending;
            state.error = Some("session has no worktree".into());
            state.updated_at = now_ms();
            self.save_integration(&state)?;
            return Ok(());
        };
        // Block prompt submission from this point on. integrate_one will
        // repeat the save (harmless) and then work outside this lock.
        state.status = IntegrationStatus::Syncing;
        state.stage = Some("base".into());
        state.updated_at = now_ms();
        self.save_integration(&state)?;
        drop(claim);
        let session_id = session.id.clone();
        match self.integrate_one(session, worktree, state).await {
            Ok(()) => Ok(()),
            Err(err) => {
                // Most failures already move the row to `retrying`. This is
                // the last line of defence against an unexpected error leaving
                // an active status that the queue never picks up again.
                if let Some(state) = self.store.get_integration(&session_id)? {
                    if state.status.is_active() {
                        let session = self.require_session(&session_id)?;
                        return self.retry_integration(&session, state, format!("{err:#}"));
                    }
                }
                Err(err)
            }
        }
    }

    async fn integrate_one(
        self: &Arc<Self>,
        session: Session,
        worktree: PathBuf,
        mut state: IntegrationState,
    ) -> Result<()> {
        let config = self.config.load_full();
        let repo = crate::worktree::main_repo(&worktree)
            .await
            .unwrap_or_else(|| session.cwd.clone());
        // A previous attempt (or a manual git command) may have left a rebase
        // behind. The queue owns an idle worktree, so recover it before trying
        // to rebase again; the backup makes this safe even with conflicts.
        if crate::worktree::in_rebase(&worktree).await {
            if let Err(err) = self
                .recover_stale_rebase(&session, &worktree, state.status)
                .await
            {
                return self
                    .start_repair(
                        &session,
                        &state,
                        "rebase recovery failed",
                        &format!("recover stale rebase: {err:#}"),
                    )
                    .await;
            }
        }
        let base = match state.base_branch.clone() {
            Some(base) => base,
            None => match self.base_for_repo(&repo).await {
                Ok(base) => base,
                Err(err) => {
                    return self
                        .start_repair(
                            &session,
                            &state,
                            "base branch unavailable",
                            &format!("{err:#}"),
                        )
                        .await;
                }
            },
        };
        state.base_branch = Some(base.clone());
        // The base branch is the branch the user was on when the session was
        // created. It may live in a linked worktree rather than the main one;
        // find that checkout before syncing/merging.
        let base_worktree = match crate::worktree::worktree_for_branch(&repo, &base).await {
            Some(path) => path,
            None => {
                return self
                    .start_repair(
                        &session,
                        &state,
                        "base branch not checked out",
                        &format!(
                            "base branch {base} is not checked out in any worktree; check it out before ready"
                        ),
                    )
                    .await;
            }
        };
        state.status = IntegrationStatus::Syncing;
        state.stage = Some("base".into());
        state.updated_at = now_ms();
        self.save_integration(&state)?;
        self.note_integration(
            &session.id,
            IntegrationStatus::Syncing,
            format!("integration syncing: base {base}"),
        );

        let (base_head, sync_commands) = if config.integration.repo_pull_base(&repo) {
            match crate::worktree::sync_base(&base_worktree, &base).await {
                Ok(run) => (run.head, run.commands),
                Err(err) => {
                    let detail = format!("base sync: {err:#}");
                    if detail.contains("uncommitted changes") {
                        return self
                            .start_repair(&session, &state, "base worktree dirty", &detail)
                            .await;
                    }
                    return self.retry_integration(&session, state, detail);
                }
            }
        } else {
            match crate::worktree::require_base(&base_worktree, &base).await {
                Ok(head) => (head, Vec::new()),
                Err(err) => {
                    return self
                        .start_repair(
                            &session,
                            &state,
                            "base branch unavailable",
                            &format!("base head: {err:#}"),
                        )
                        .await;
                }
            }
        };
        self.note_integration(
            &session.id,
            IntegrationStatus::Syncing,
            format!(
                "integration synced: base {base}\n{}",
                render_command_logs(&sync_commands)
            ),
        );

        state.status = IntegrationStatus::Rebasing;
        state.stage = Some("rebase".into());
        state.updated_at = now_ms();
        self.save_integration(&state)?;
        self.note_integration(
            &session.id,
            IntegrationStatus::Rebasing,
            format!("integration rebasing: session branch onto {base}"),
        );
        let rebase = match crate::worktree::rebase(&worktree, &base).await {
            Ok(output) => output,
            Err(err) => {
                return self
                    .start_repair(
                        &session,
                        &state,
                        "rebase failed",
                        &format!("rebase: {err:#}"),
                    )
                    .await;
            }
        };
        if !rebase.success {
            let conflicts = crate::worktree::conflict_files(&worktree).await;
            if !conflicts.is_empty() || crate::worktree::in_rebase(&worktree).await {
                let details = format!(
                    "rebase conflict on {} (base {base}).\nConflicting files:\n{}\n\n{}",
                    crate::worktree::branch(&worktree).await.unwrap_or_default(),
                    conflicts.join("\n"),
                    crate::integration::tail(&rebase.combined(), 8_000)
                );
                let log = crate::worktree::CommandLog {
                    command: format!("git rebase {base}"),
                    output: rebase.combined(),
                };
                self.note_integration(
                    &session.id,
                    IntegrationStatus::Rebasing,
                    format!(
                        "integration rebase conflict · {base}\n{}",
                        render_command_log(&log)
                    ),
                );
                if !config.integration.conflict_repair {
                    // Retrying with the conflicted rebase left in place would
                    // just wedge the next attempt. Preserve it and clear it
                    // before backing off.
                    if let Err(err) = self
                        .recover_stale_rebase(&session, &worktree, state.status)
                        .await
                    {
                        return self.retry_integration(
                            &session,
                            state,
                            format!("{details}\nrebase recovery failed: {err:#}"),
                        );
                    }
                    return self.retry_integration(&session, state, details);
                }
                return self
                    .start_repair(&session, &state, "rebase conflict", &details)
                    .await;
            }
            return self
                .start_repair(
                    &session,
                    &state,
                    "rebase failed",
                    &format!("rebase: {}", rebase.combined()),
                )
                .await;
        }
        let log = crate::worktree::CommandLog {
            command: format!("git rebase {base}"),
            output: rebase.combined(),
        };
        self.note_integration(
            &session.id,
            IntegrationStatus::Rebasing,
            format!("integration rebased: {base}\n{}", render_command_log(&log)),
        );
        let head_before_test = match crate::worktree::head(&worktree).await {
            Ok(head) => head,
            Err(err) => {
                return self.retry_integration(&session, state, format!("head: {err:#}"));
            }
        };

        let test = match crate::integration::test_command(
            &worktree,
            Some(&session.cwd),
            config.integration.repo_test(&repo),
        ) {
            Some(test) => test,
            None => {
                let details = format!(
                    "no test command configured for {}\n\n\
                     Set [integration] test or [integration.repos.\"{}\"] in the user config.\n\
                     If the repository keeps multiple projects in subdirectories, ask the user\n\
                     which project/command to use before making changes.",
                    worktree.display(),
                    repo.display()
                );
                return self
                    .start_repair(&session, &state, "test command missing", &details)
                    .await;
            }
        };
        state.status = IntegrationStatus::Testing;
        state.stage = Some("test".into());
        state.updated_at = now_ms();
        self.save_integration(&state)?;
        self.note_integration(
            &session.id,
            IntegrationStatus::Testing,
            format!("integration testing: `{test}`"),
        );
        let run = match crate::integration::run_test(
            &worktree,
            &test,
            config.integration.repo_timeout(&repo),
        )
        .await
        {
            Ok(run) => run,
            Err(err) => {
                let details = format!("test: {err:#}");
                self.note_integration(
                    &session.id,
                    IntegrationStatus::Testing,
                    format!("integration test could not run · `{test}`\n{details}"),
                );
                return self
                    .start_repair(&session, &state, "test command could not run", &details)
                    .await;
            }
        };
        let test_log = crate::worktree::CommandLog {
            command: test.clone(),
            output: run.output.clone(),
        };
        if !run.success {
            let reason = if run.timed_out {
                "integration test timed out"
            } else {
                "integration test failed"
            };
            self.note_integration(
                &session.id,
                IntegrationStatus::Testing,
                format!("{reason} · `{test}`\n{}", render_command_log(&test_log)),
            );
            let details = format!(
                "test command failed: `{test}`\n\n{}",
                crate::integration::tail(&run.output, 8_000)
            );
            return self
                .start_repair(&session, &state, "test failure", &details)
                .await;
        }
        self.note_integration(
            &session.id,
            IntegrationStatus::Testing,
            format!(
                "integration test passed: `{test}`\n{}",
                render_command_log(&test_log)
            ),
        );
        match crate::worktree::head(&worktree).await {
            Ok(after) if after == head_before_test => {}
            Ok(_) => {
                return self
                    .start_repair(
                        &session,
                        &state,
                        "test command changed branch HEAD",
                        "The test command created a commit. Integration refuses to merge a branch\n\
                         that changed while it was being tested.",
                    )
                    .await;
            }
            Err(err) => {
                return self.retry_integration(&session, state, format!("head: {err:#}"));
            }
        }
        if !crate::worktree::is_clean(&worktree).await? {
            return self
                .start_repair(
                    &session,
                    &state,
                    "test command left changes",
                    "The test command left tracked changes in the worktree. Integration refuses\n\
                     to merge files that were not part of the session's commit.",
                )
                .await;
        }

        let current_base = match crate::worktree::require_base(&base_worktree, &base).await {
            Ok(head) => head,
            Err(err) => {
                return self
                    .start_repair(
                        &session,
                        &state,
                        "base branch unavailable",
                        &format!("base head: {err:#}"),
                    )
                    .await;
            }
        };
        if current_base != base_head {
            return self.retry_integration(
                &session,
                state,
                "base branch moved during testing; rebasing again".into(),
            );
        }
        let branch = match crate::worktree::branch(&worktree).await {
            Some(branch) => branch,
            None => {
                return self
                    .start_repair(
                        &session,
                        &state,
                        "worktree has no branch",
                        "The session worktree is detached and cannot be merged.",
                    )
                    .await;
            }
        };
        state.status = IntegrationStatus::Merging;
        state.stage = Some("merge".into());
        state.updated_at = now_ms();
        self.save_integration(&state)?;
        self.note_integration(
            &session.id,
            IntegrationStatus::Merging,
            format!("integration merging · {branch} → {base}"),
        );
        let merge_log = match crate::worktree::merge_ff(&base_worktree, &branch).await {
            Ok(log) => log,
            Err(err) => {
                return self
                    .start_repair(
                        &session,
                        &state,
                        "merge failed",
                        &format!("merge {branch} into {base}: {err:#}"),
                    )
                    .await;
            }
        };
        let merged_commit = crate::worktree::head(&base_worktree).await.ok();
        state.status = IntegrationStatus::Merged;
        state.stage = None;
        state.error = None;
        state.next_retry_at = None;
        state.merged_commit = merged_commit;
        state.updated_at = now_ms();
        self.save_integration(&state)?;
        let commit = state
            .merged_commit
            .as_deref()
            .map(|commit| commit.chars().take(7).collect::<String>())
            .unwrap_or_default();
        let headline = if commit.is_empty() {
            format!("integration merged into {base}")
        } else {
            format!("integration merged into {base} · {commit}")
        };
        self.note_integration(
            &session.id,
            IntegrationStatus::Merged,
            format!("{headline}\n{}", render_command_log(&merge_log)),
        );
        self.emit_value(
            "integration.merged",
            json!({
                "session_id": session.id,
                "base": base,
                "commit": state.merged_commit,
            }),
        );
        if config.integration.push {
            match crate::worktree::push_base(&base_worktree, &base).await {
                Ok(log) => self.note_integration(
                    &session.id,
                    IntegrationStatus::Merged,
                    format!("integration pushed · {base}\n{}", render_command_log(&log)),
                ),
                Err(err) => {
                    self.note_integration(
                        &session.id,
                        IntegrationStatus::Merged,
                        format!("integration push failed · {base}\n{err:#}"),
                    );
                    eprintln!(
                        "eixd: integration push failed: {}",
                        redact(&format!("{err:#}"))
                    );
                }
            }
        }
        Ok(())
    }

    async fn start_repair(
        self: &Arc<Self>,
        session: &Session,
        state: &IntegrationState,
        reason: &str,
        details: &str,
    ) -> Result<()> {
        let config = self.config.load_full();
        if !config.integration.conflict_repair {
            return self.retry_integration(session, state.clone(), details.to_string());
        }
        // A pending repair is resumed by `resume_pending_repairs` after the
        // previous turn finishes. Do not count it as a new failure twice.
        let resuming = state.status == IntegrationStatus::Repairing
            && state
                .stage
                .as_deref()
                .is_some_and(|stage| stage.starts_with(REPAIR_PENDING_STAGE));
        let mut repairing = state.clone();
        repairing.next_retry_at = None;
        if resuming {
            repairing.stage = Some(reason.to_string());
        } else {
            let paused = self.account_failure(&mut repairing, details);
            if paused {
                repairing.status = IntegrationStatus::Retrying;
                repairing.next_retry_at = Some(i64::MAX / 4);
                self.save_integration(&repairing)?;
                self.notify_failure(session, &repairing);
                self.note_integration(
                    &session.id,
                    IntegrationStatus::Retrying,
                    format!(
                        "integration paused after {} attempts · {reason}\nRun `eix integrate continue` to requeue.",
                        repairing.attempts
                    ),
                );
                return Ok(());
            }
            repairing.status = IntegrationStatus::Repairing;
            repairing.stage = Some(reason.to_string());
        }
        if self.turns.lock().await.contains_key(&session.id) || session.status.is_busy() {
            repairing.stage = Some(format!("{REPAIR_PENDING_STAGE}{reason}"));
            self.save_integration(&repairing)?;
            if !resuming {
                self.notify_failure(session, &repairing);
                self.note_integration(
                    &session.id,
                    IntegrationStatus::Repairing,
                    format!("integration repair pending · {reason}"),
                );
            }
            return Ok(());
        }
        let worktree = session
            .worktree
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| session.cwd.display().to_string());
        let prompt = format!(
            "Integration failed while landing this session's worktree.\n\n\
             Reason: {reason}\n\
             Worktree: {worktree}\n\n{details}\n\n\
             Please resolve this autonomously. If a rebase is in progress, edit the\n\
             conflicted files, run `git add <files>`, then `GIT_EDITOR=true git rebase --continue`\n\
             until the rebase finishes. If you cannot resolve it, run `git rebase --abort` and explain.\n\
             If the fix needs a human decision (for example which test command to use, or how\n\
             to clean the base worktree), ask the user with `ask_user` and wait for the answer.\n\
             Do not call `ready` again until something has changed. After resolving, run the\n\
             project tests, then call the `ready` tool (or run `eix ready`) to requeue. Do not\n\
             merge or rebase the base branch yourself.",
        );
        self.save_integration(&repairing)?;
        if !resuming {
            self.notify_failure(session, &repairing);
        }
        self.note_integration(
            &session.id,
            IntegrationStatus::Repairing,
            format!("integration repairing · {reason}"),
        );
        let params = SubmitParams {
            session_id: session.id.clone(),
            text: prompt,
            images: Vec::new(),
        };
        if let Err(err) = self.submit_inner(params, true).await {
            return self.retry_integration(session, repairing, format!("repair turn: {err:#}"));
        }
        Ok(())
    }

    /// Count a conflict/test/transient failure and return whether automatic
    /// repair should pause for `eix integrate continue`.
    fn account_failure(&self, state: &mut IntegrationState, error: &str) -> bool {
        let config = self.config.load_full();
        state.attempts = state.attempts.saturating_add(1);
        state.error = Some(crate::integration::tail(error, 8_000));
        state.updated_at = now_ms();
        if config.integration.pause_after > 0 && state.attempts >= config.integration.pause_after {
            state.error = Some(format!(
                "{} (paused after {} attempts; run `eix integrate continue`)",
                state.error.clone().unwrap_or_default(),
                state.attempts
            ));
            return true;
        }
        false
    }

    /// Emit one attention notification when the configured failure threshold
    /// is crossed. Later retries keep the same status without spamming.
    fn notify_failure(&self, session: &Session, state: &IntegrationState) {
        let config = self.config.load_full();
        if config.integration.notify_after > 0 && state.attempts == config.integration.notify_after
        {
            self.emit_value(
                "integration.attention",
                json!({
                    "session_id": session.id,
                    "session_name": session.name,
                    "attempts": state.attempts,
                    "error": state.error,
                }),
            );
            // Reuse the configured external alert command. Integration
            // failures are not a session status transition, so the normal
            // transition path would never run it.
            self.notify_attention(session, SessionStatus::Error);
        }
    }

    fn retry_integration(
        &self,
        session: &Session,
        mut state: IntegrationState,
        error: String,
    ) -> Result<()> {
        let config = self.config.load_full();
        let previous_status = state.status;
        let previous_error = state.error.clone();
        let paused_by_config = self.account_failure(&mut state, &error);
        // A transient failure is worth a few retries. A deterministic failure
        // (bad test command, dirty base, repeated merge failure) would produce
        // the same result forever, so stop and hand control back to the user.
        let repeated = previous_status == IntegrationStatus::Retrying
            && state.attempts >= 3
            && state.error == previous_error;
        let paused = paused_by_config || repeated;
        state.status = IntegrationStatus::Retrying;
        state.stage = None;
        if paused {
            state.next_retry_at = Some(i64::MAX / 4);
        } else {
            let base = 5u64.saturating_mul(1u64 << state.attempts.saturating_sub(1).min(6));
            let delay = base.min(config.integration.retry_max_secs.max(5));
            state.next_retry_at = Some(now_ms() + (delay as i64 * 1_000));
        }
        self.save_integration(&state)?;
        self.notify_failure(session, &state);
        // Record the first observation of a failure, not every backoff tick.
        if previous_status != IntegrationStatus::Retrying
            || state.error != previous_error
            || repeated
        {
            let detail = crate::integration::tail(state.error.as_deref().unwrap_or(&error), 2_000);
            let text = if paused {
                if repeated {
                    format!(
                        "integration paused · the same failure repeated {} times · {detail}\nFix the cause, then run `eix integrate continue`.",
                        state.attempts
                    )
                } else {
                    format!(
                        "integration paused after {} attempts · {detail}\nRun `eix integrate continue` to requeue.",
                        state.attempts
                    )
                }
            } else {
                format!(
                    "integration retrying (attempt {}) · {detail}",
                    state.attempts
                )
            };
            self.note_integration(&session.id, IntegrationStatus::Retrying, text);
        }
        Ok(())
    }

    /// A deterministic failure can arrive while the agent is still finishing
    /// the turn that called `ready`. In that case `start_repair` records a
    /// pending marker; once the session is idle, inject the repair prompt.
    async fn resume_pending_repairs(self: &Arc<Self>) -> Result<()> {
        let sessions = self.store.list_sessions()?;
        for session in sessions {
            if session.archived || session.is_agent() || !session.has_worktree() {
                continue;
            }
            let Some(state) = self.store.get_integration(&session.id)? else {
                continue;
            };
            if state.status != IntegrationStatus::Repairing || state.worktree_disabled {
                continue;
            }
            let Some(reason) = state
                .stage
                .as_deref()
                .and_then(|stage| stage.strip_prefix(REPAIR_PENDING_STAGE))
            else {
                continue;
            };
            let reason = reason.to_string();
            if reason.trim().is_empty() {
                continue;
            }
            if session.status != SessionStatus::Idle
                || self.turns.lock().await.contains_key(&session.id)
                || self.approvals.lock().await.contains_key(&session.id)
                || self.inputs.lock().await.contains_key(&session.id)
            {
                continue;
            }
            // Do not overwrite a prompt the user is already drafting.
            let (draft, _) = self.store.get_draft(&session.id)?;
            if !draft.trim().is_empty() {
                continue;
            }
            // Let the finishing turn persist its final events first.
            if now_ms().saturating_sub(session.updated_at) < 1_000 {
                continue;
            }
            let details = state.error.clone().unwrap_or_default();
            if let Err(err) = self.start_repair(&session, &state, &reason, &details).await {
                eprintln!(
                    "eixd: cannot resume repair for {}: {}",
                    session.id,
                    redact(&format!("{err:#}"))
                );
            }
        }
        Ok(())
    }

    /// Auto-ready and repair-watchdog. Runs on the queue loop; one pass every
    /// few seconds keeps the daemon simple and restart-safe.
    async fn auto_ready_idle(&self) -> Result<()> {
        let config = self.config.load_full();
        if config.integration.auto_ready != AutoReady::Idle {
            return Ok(());
        }
        let idle_ms = (config.integration.idle_secs.max(1) as i64) * 1_000;
        let sessions = self.store.list_sessions()?;
        for session in sessions {
            if session.archived || session.is_agent() || !session.has_worktree() {
                continue;
            }
            let Some(state) = self.store.get_integration(&session.id)? else {
                continue;
            };
            if state.worktree_disabled {
                continue;
            }
            if !matches!(
                state.status,
                IntegrationStatus::Pending | IntegrationStatus::Repairing
            ) {
                continue;
            }
            if session.status != SessionStatus::Idle
                || self.turns.lock().await.contains_key(&session.id)
                || self.approvals.lock().await.contains_key(&session.id)
                || self.inputs.lock().await.contains_key(&session.id)
            {
                continue;
            }
            let (draft, _) = self.store.get_draft(&session.id)?;
            if !draft.trim().is_empty() {
                continue;
            }
            let idle_needed = if state.status == IntegrationStatus::Repairing {
                idle_ms.min(30_000)
            } else {
                idle_ms
            };
            if now_ms().saturating_sub(session.updated_at) < idle_needed {
                continue;
            }
            if !self.store.has_user_message(&session.id)? {
                continue;
            }
            if state.status == IntegrationStatus::Repairing {
                // A pending marker means the repair prompt was never injected
                // (the previous turn was still running). `resume_pending_repairs`
                // owns that case.
                if state
                    .stage
                    .as_deref()
                    .is_some_and(|stage| stage.starts_with(REPAIR_PENDING_STAGE))
                {
                    continue;
                }
                // A repair turn can end without calling ready, leaving a
                // rebase (and sometimes resolved conflicts) behind. Preserve
                // that state in a backup and abort the rebase so the next
                // ready/queue pass is not wedged. Dirty tracked files still
                // wait for the agent or auto-commit.
                let worktree = session.worktree.as_deref().unwrap_or(&session.cwd);
                if crate::worktree::in_rebase(worktree).await {
                    if let Err(err) = self
                        .recover_stale_rebase(&session, worktree, state.status)
                        .await
                    {
                        eprintln!(
                            "eixd: cannot recover stale rebase for {}: {}",
                            session.id,
                            redact(&format!("{err:#}"))
                        );
                        continue;
                    }
                }
                if !crate::worktree::is_clean(&session.cwd).await? {
                    continue;
                }
                let mut next = state;
                next.status = IntegrationStatus::Retrying;
                next.stage = Some("repair watchdog".into());
                next.error = Some(
                    "repair turn finished without eix ready; paused (run `eix integrate continue` or call ready)"
                        .into(),
                );
                next.next_retry_at = Some(i64::MAX / 4);
                next.updated_at = now_ms();
                self.save_integration(&next)?;
                self.note_integration(
                    &session.id,
                    IntegrationStatus::Retrying,
                    "integration paused · repair turn finished without ready\nFix the cause and call `ready`, or run `eix integrate continue`.".into(),
                );
                continue;
            }
            // A dirty worktree is normal while the agent is still working.
            // Wait for it to become clean unless auto-commit is enabled; the
            // next idle scan will try again.
            if !config.integration.auto_commit && !crate::worktree::is_clean(&session.cwd).await? {
                continue;
            }
            if let Err(err) = self.request_ready(&session.id, None).await {
                eprintln!(
                    "eixd: auto-ready skipped for {}: {}",
                    session.id,
                    redact(&format!("{err:#}"))
                );
            }
        }
        Ok(())
    }

    async fn integration_loop(self: Arc<Self>) {
        let mut tick = 0u64;
        loop {
            if self.config.load().integration.enabled {
                if let Err(err) = self.process_next_integration().await {
                    eprintln!("eixd: integration queue: {}", redact(&format!("{err:#}")));
                }
                // A repair can only start after the turn that called `ready`
                // has finished, even when auto-ready is disabled.
                if let Err(err) = self.resume_pending_repairs().await {
                    eprintln!(
                        "eixd: resume pending repairs: {}",
                        redact(&format!("{err:#}"))
                    );
                }
                // Auto-ready does a git status per idle session; the default
                // idle window is minutes, so a 20-second scan is plenty.
                if tick.is_multiple_of(10) {
                    if let Err(err) = self.auto_ready_idle().await {
                        eprintln!(
                            "eixd: idle integration scan: {}",
                            redact(&format!("{err:#}"))
                        );
                    }
                }
            }
            tick = tick.wrapping_add(1);
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    }

    /// A daemon restart cannot resume an in-flight HTTP stream. Mark those
    /// sessions errored instead of pretending they are still running.
    async fn recover(&self) {
        self.recover_integrations().await;
        let running = match self.store.running_sessions() {
            Ok(sessions) => sessions,
            Err(err) => {
                eprintln!(
                    "eixd: cannot scan running sessions: {}",
                    redact(&format!("{err:#}"))
                );
                return;
            }
        };
        for session in running {
            let message = "daemon restarted while this turn was running".to_string();
            if let Ok(event) = self.store.append_event(
                &session.id,
                EventKind::Error,
                payload(&ErrorPayload {
                    message: message.clone(),
                }),
            ) {
                self.emit_event(&event);
            }
            if let Err(err) = self.transition(&session.id, SessionStatus::Error) {
                eprintln!(
                    "eixd: cannot recover {}: {}",
                    session.id,
                    redact(&format!("{err:#}"))
                );
            }
        }
    }

    /// Active integration rows cannot make progress after a restart: the
    /// worker that owned them is gone. Abort any half-finished rebase (which
    /// restores the pre-rebase branch) and put the item back in the retry
    /// queue; a merge that already landed is a no-op on the next attempt.
    async fn recover_integrations(&self) {
        let states = match self.store.list_integrations() {
            Ok(states) => states,
            Err(err) => {
                eprintln!(
                    "eixd: cannot scan integrations: {}",
                    redact(&format!("{err:#}"))
                );
                return;
            }
        };
        for mut state in states {
            if !state.status.is_active() {
                continue;
            }
            if let Ok(Some(session)) = self.store.get_session(&state.session_id) {
                if let Some(worktree) = session.worktree.as_deref() {
                    if crate::worktree::in_rebase(worktree).await {
                        if let Err(err) = self
                            .recover_stale_rebase(&session, worktree, state.status)
                            .await
                        {
                            eprintln!(
                                "eixd: cannot recover stale rebase for {}: {}",
                                state.session_id,
                                redact(&format!("{err:#}"))
                            );
                        }
                    }
                }
            }
            state.status = IntegrationStatus::Retrying;
            state.stage = None;
            state.error = Some("daemon restarted during integration; retrying".into());
            state.next_retry_at = Some(now_ms());
            state.updated_at = now_ms();
            if let Err(err) = self.save_integration(&state) {
                eprintln!(
                    "eixd: cannot recover integration {}: {}",
                    state.session_id,
                    redact(&format!("{err:#}"))
                );
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Unix,
    Tcp,
}

impl Daemon {
    async fn serve<S>(self: Arc<Self>, stream: S, transport: Transport, token: Option<String>)
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (read, write) = tokio::io::split(stream);
        let (tx, rx) = mpsc::unbounded_channel::<ServerMessage>();
        let writer = tokio::spawn(write_loop(write, rx));

        // A Unix connection is trusted by its 0600 permissions, so events are
        // broadcast to every client. A TCP connection starts with no events at
        // all and only subscribes after a successful `auth`, so nothing leaks
        // before the token is checked.
        // Per-connection event filter; `None` means every session. It is
        // shared with the bus task so `session.subscribe` can narrow it.
        let filter = Arc::new(RwLock::new(None::<HashSet<SessionId>>));
        let mut bus_task = (transport == Transport::Unix)
            .then(|| spawn_bus(self.bus.clone(), tx.clone(), filter.clone()));

        let mut lines = BufReader::new(read).lines();
        // Unix sockets are already protected by filesystem permissions.
        let mut authorized = transport == Transport::Unix;
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(_) => break,
            };
            if line.trim().is_empty() {
                continue;
            }
            let request = match Request::parse(&line) {
                Ok(request) => request,
                Err(err) => {
                    let _ = tx.send(ServerMessage::Response(Response::err(
                        0,
                        "bad_request",
                        format!("invalid JSON request: {err}"),
                    )));
                    continue;
                }
            };
            if !authorized {
                let okay = if request.method == "auth" {
                    serde_json::from_value::<AuthParams>(request.params.clone())
                        .ok()
                        .map(|params| params.token)
                        .as_deref()
                        .zip(token.as_deref())
                        .is_some_and(|(given, expected)| secret_eq(given, expected))
                } else {
                    false
                };
                if !okay {
                    let _ = tx.send(ServerMessage::Response(Response::err(
                        request.id,
                        "unauthorized",
                        "a valid `auth` request with the server token is required",
                    )));
                    break;
                }
                authorized = true;
                // From here on this connection sees the same broadcast as a
                // local client.
                bus_task = Some(spawn_bus(self.bus.clone(), tx.clone(), filter.clone()));
                let _ = tx.send(ServerMessage::Response(Response::ok(
                    request.id,
                    json!({ "authenticated": true }),
                )));
                continue;
            }
            // Connection-local: narrow which sessions this client hears about.
            if request.method == "session.subscribe" {
                let response =
                    match serde_json::from_value::<SubscribeParams>(request.params.clone()) {
                        Ok(params) => {
                            *filter.write().await = params
                                .session_ids
                                .clone()
                                .map(|ids| ids.into_iter().collect::<HashSet<_>>());
                            Response::ok(
                                request.id,
                                json!(SubscribeResult {
                                    session_ids: params.session_ids
                                }),
                            )
                        }
                        Err(err) => Response::err(request.id, "bad_request", err.to_string()),
                    };
                if tx.send(ServerMessage::Response(response)).is_err() {
                    break;
                }
                continue;
            }
            let response = self.handle(&request).await;
            if tx.send(ServerMessage::Response(response)).is_err() {
                break;
            }
        }

        if let Some(task) = bus_task {
            task.abort();
        }
        drop(tx);
        let _ = writer.await;
    }

    pub(crate) async fn handle(self: &Arc<Self>, request: &Request) -> Response {
        match self.dispatch(request).await {
            Ok(result) => Response::ok(request.id, result),
            Err(err) => match err.downcast_ref::<RpcError>() {
                Some(rpc) => Response::err(request.id, rpc.code.clone(), rpc.message.clone()),
                None => Response::err(request.id, "internal", format!("{err:#}")),
            },
        }
    }

    async fn dispatch(self: &Arc<Self>, request: &Request) -> Result<Value> {
        match request.method.as_str() {
            "hello" => Ok(serde_json::to_value(HelloResult {
                protocol: PROTOCOL_VERSION,
                version: env!("CARGO_PKG_VERSION").to_string(),
                pid: std::process::id(),
                model: self.config.load().default_model().to_string(),
                base_url: self.config.load().default_provider().base_url.clone(),
                sessions: self.store.session_count()?,
                max_parallel_subagents: self.subagent_concurrency.load(Ordering::Relaxed),
            })?),
            "ping" => Ok(json!({ "pong": true, "ts": now_ms() })),
            "session.list" => {
                let include_archived = request
                    .params
                    .get("include_archived")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut sessions = self.store.list_sessions_including(include_archived)?;
                self.effective_permissions(&mut sessions);
                Ok(serde_json::to_value(SessionList { sessions })?)
            }
            "session.archive" => {
                let params: SessionArchiveParams = parse_params(request.params.clone())?;
                let session = self
                    .store
                    .set_session_archived(&params.session_id, params.archived)?
                    .ok_or_else(|| rpc("not_found", "unknown session"))?;
                self.emit_value("session.updated", serde_json::to_value(&session)?);
                Ok(serde_json::to_value(SessionResult { session })?)
            }
            "session.delete" => {
                let params: SessionRef = parse_params(request.params.clone())?;
                if self.turns.lock().await.contains_key(&params.session_id) {
                    return Err(rpc("busy", "cannot delete a running session"));
                }
                let deleted = self.store.delete_session(&params.session_id)?;
                if !deleted {
                    return Err(rpc("not_found", "unknown session"));
                }
                self.todos.lock().await.remove(&params.session_id);
                self.emit_value(
                    "session.deleted",
                    json!({ "session_id": params.session_id }),
                );
                let _ = self.gc_blobs().await;
                Ok(json!({ "deleted": true }))
            }
            // Frontends open a session before the first prompt so they can
            // render the composer and bind a daemon-side draft. If the pane is
            // closed without any input, discard that unused row instead of
            // leaving it in the session picker forever.
            "session.delete_if_empty" => {
                let params: SessionRef = parse_params(request.params.clone())?;
                // A running turn may not have persisted its first event yet;
                // never race it with a cleanup delete.
                if self.turns.lock().await.contains_key(&params.session_id) {
                    return Ok(json!({ "deleted": false }));
                }
                let deleted = self.store.delete_session_if_unused(&params.session_id)?;
                if deleted {
                    self.todos.lock().await.remove(&params.session_id);
                    self.emit_value(
                        "session.deleted",
                        json!({ "session_id": params.session_id }),
                    );
                    let _ = self.gc_blobs().await;
                }
                Ok(json!({ "deleted": deleted }))
            }
            "session.undo" | "session.redo" => {
                let params: SessionRef = parse_params(request.params.clone())?;
                if self.turns.lock().await.contains_key(&params.session_id) {
                    return Err(rpc("busy", "cannot undo while a turn is running"));
                }
                let changed = if request.method == "session.undo" {
                    self.store.undo_last_turn(&params.session_id)?
                } else {
                    self.store.redo_last_undo(&params.session_id)?
                };
                match changed {
                    Some(count) => Ok(json!({ "changed": count })),
                    None if request.method == "session.undo" => {
                        Err(rpc("nothing_to_undo", "no user turn to undo"))
                    }
                    None => Err(rpc("nothing_to_redo", "nothing to redo")),
                }
            }
            "shell.run" => {
                let params: ShellRunParams = parse_params(request.params.clone())?;
                let command = params.command.trim();
                if command.is_empty() {
                    return Err(rpc("invalid", "shell command is empty"));
                }
                let session = self.require_session(&params.session_id)?;
                let cwd = session.cwd.clone();
                let permission = self.session_permission(&session);
                let decision = {
                    let config = self.config.load();
                    permission_decide(
                        "run_command",
                        &json!({ "command": command }),
                        &cwd,
                        &config,
                        permission,
                        &[],
                        false,
                    )
                };
                let call_id = format!("shell-{}", Uuid::new_v4());
                self.record_permission(&params.session_id, &call_id, "run_command", &decision)?;
                if decision.action == PermissionAction::Deny {
                    return Err(rpc("permission", decision.reason));
                }
                let mutating = decision.risk.mutates_workspace();
                let arguments = json!({ "command": command });
                let call = ToolCall {
                    id: call_id.clone(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "run_command".to_string(),
                        arguments: arguments.to_string(),
                    },
                };
                let event = self.store.append_event(
                    &params.session_id,
                    EventKind::ToolCall,
                    payload(&ToolCallPayload {
                        call_id: call_id.clone(),
                        tool: "run_command".to_string(),
                        arguments: arguments.clone(),
                    }),
                )?;
                self.emit_event(&event);
                if self.config.load().tools.snapshots && mutating {
                    let result = self
                        .snapshots
                        .capture_tree(&params.session_id, "run_command", &call_id, &cwd)
                        .await;
                    let _ = self
                        .record_snapshot(
                            &params.session_id,
                            "run_command",
                            &call_id,
                            Scope::Tree,
                            result,
                        )
                        .await?;
                }
                let outcome =
                    tools::execute("run_command", &arguments, &cwd, self.config.load().as_ref())
                        .await;
                self.record_tool_result(
                    &params.session_id,
                    &call,
                    outcome.ok,
                    false,
                    &outcome.output,
                    outcome.duration_ms,
                )?;
                Ok(serde_json::to_value(ShellRunResult {
                    ok: outcome.ok,
                    output: outcome.output,
                })?)
            }
            "session.todos" => {
                let params: SessionRef = parse_params(request.params.clone())?;
                let items = self
                    .todos
                    .lock()
                    .await
                    .get(&params.session_id)
                    .cloned()
                    .unwrap_or_default();
                Ok(serde_json::to_value(eix_core::TodoList { items })?)
            }
            "session.export" => {
                let params: SessionRef = parse_params(request.params.clone())?;
                let session = self.require_session(&params.session_id)?;
                let mut events = self.store.events(&params.session_id)?;
                // Exports must remain portable: inline image bytes even when
                // this daemon stores them as blobs.
                for event in &mut events {
                    self.hydrate_event_images(event).await?;
                }
                let document = SessionExport {
                    version: SESSION_EXPORT_VERSION,
                    session,
                    events,
                };
                Ok(serde_json::to_value(SessionExportResult {
                    json: serde_json::to_string_pretty(&document)?,
                })?)
            }
            "session.import" => {
                let params: SessionImportParams = parse_params(request.params.clone())?;
                let document: SessionExport = serde_json::from_str(&params.json)
                    .map_err(|err| rpc("invalid_export", format!("invalid export: {err}")))?;
                if document.version != SESSION_EXPORT_VERSION {
                    return Err(rpc(
                        "invalid_export",
                        format!(
                            "unsupported export version {} (expected {})",
                            document.version, SESSION_EXPORT_VERSION
                        ),
                    ));
                }
                let mut session = document.session;
                session.id = Uuid::new_v4().to_string();
                session.status = SessionStatus::Idle;
                session.parent_id = None;
                session.forked_from_seq = None;
                session.worktree = None;
                session.origin = SessionOrigin::User;
                session.archived = false;
                if let Some(name) = params.name {
                    session.name = name;
                }
                if let Some(cwd) = params.cwd {
                    session.cwd = cwd;
                }
                let now = eix_core::now_ms();
                session.created_at = now;
                session.updated_at = now;
                self.store.create_session(&session)?;
                for mut event in document.events {
                    event.session_id = session.id.clone();
                    // Make imported inline images local blobs. Exports carry
                    // inline bytes precisely so this does not depend on the
                    // exporting machine's blob directory.
                    self.store_event_images(&mut event).await?;
                    self.store.insert_event(&event)?;
                }
                Ok(serde_json::to_value(SessionResult { session })?)
            }
            "session.timeline" => {
                let params: SessionRef = parse_params(request.params.clone())?;
                let limit = request
                    .params
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(100) as usize;
                let events = self.store.events(&params.session_id)?;
                let mut items: Vec<TimelineItem> = events
                    .into_iter()
                    .filter_map(|event| {
                        let preview = timeline_preview(event.kind, &event.payload)?;
                        Some(TimelineItem {
                            seq: event.seq,
                            ts: event.ts,
                            kind: event.kind.as_str().to_string(),
                            preview,
                        })
                    })
                    .collect();
                if items.len() > limit {
                    items.drain(0..items.len() - limit);
                }
                Ok(serde_json::to_value(TimelineResult { items })?)
            }
            "session.create" => {
                let params: CreateSessionParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(SessionResult {
                    session: self.create_session(params).await?,
                })?)
            }
            "model.list" => {
                let (models, warnings) = self.model_catalog().await;
                Ok(serde_json::to_value(ModelListResult { models, warnings })?)
            }
            "config.reload" => {
                let paths = self.reload_config().await?;
                Ok(json!({
                    "reloaded": true,
                    "paths": paths,
                    "max_parallel_subagents": self.subagent_concurrency.load(Ordering::Relaxed),
                }))
            }
            "config.subagents" => {
                let params: SubagentConcurrencyParams = parse_params(request.params.clone())?;
                if !eix_core::subagent_concurrency_supported(params.max_parallel_subagents) {
                    return Err(rpc(
                        "invalid_params",
                        "max_parallel_subagents must be one of 1, 2, 4, 8, 16, 32",
                    ));
                }
                self.subagent_concurrency
                    .store(params.max_parallel_subagents, Ordering::Relaxed);
                Ok(json!({
                    "max_parallel_subagents": params.max_parallel_subagents,
                }))
            }
            "agent.list" => {
                let params: SessionRef = parse_params(request.params.clone())?;
                let cwd = self
                    .require_session(&params.session_id)
                    .map(|session| session.cwd.clone())
                    .unwrap_or_else(|_| {
                        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
                    });
                let config = self.config.load();
                Ok(serde_json::to_value(AgentListResult {
                    agents: eix_core::load_agents(&cwd, &config.compations),
                })?)
            }
            "mcp.list" => {
                let mut mcp = self.mcp.lock().await;
                mcp.ensure_loaded(self.config.load().as_ref()).await;
                mcp.refresh(self.config.load().as_ref()).await;
                let statuses = mcp.status();
                let servers = self
                    .config
                    .load()
                    .mcp
                    .servers
                    .iter()
                    .map(|(name, server)| {
                        let status = statuses.iter().find(|status| &status.name == name);
                        McpServerInfo {
                            name: name.clone(),
                            enabled: server.enabled,
                            command: server.command.clone(),
                            tools: status.map(|status| status.tools).unwrap_or(0),
                            resources: status.map(|status| status.resources).unwrap_or(0),
                            prompts: status.map(|status| status.prompts).unwrap_or(0),
                            error: status.and_then(|status| status.error.clone()),
                        }
                    })
                    .collect();
                Ok(serde_json::to_value(McpListResult { servers })?)
            }
            "stats" => Ok(serde_json::to_value(self.stats().await?)?),
            "lsp.workspace" => {
                let params: LspWorkspaceParams = parse_params(request.params.clone())?;
                let session = self.require_session(&params.session_id)?;
                let config = self.config.load();
                let mut lsp = self.lsp.lock().await;
                let result = lsp
                    .workspace(
                        &config,
                        &session.cwd,
                        params.max_files.unwrap_or(200).clamp(1, 2000),
                    )
                    .await?;
                Ok(serde_json::to_value(result)?)
            }
            "session.model" => {
                let params: SessionModelParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(SessionResult {
                    session: self.set_model(params)?,
                })?)
            }
            "session.agent" => {
                let params: SessionAgentParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(SessionResult {
                    session: self.set_agent(params)?,
                })?)
            }
            "session.fork" => {
                let params: ForkParams = parse_params(request.params.clone())?;
                let source = self.require_session(&params.session_id)?;
                if let Some(at) = params.at_seq {
                    let max = self.store.max_seq(&source.id)?;
                    if at < 1 || at > max + 1 {
                        return Err(rpc(
                            "invalid_params",
                            format!("at_seq must be between 1 and {}", max + 1),
                        ));
                    }
                }
                let name = match params.name {
                    Some(name) => name.trim().to_string(),
                    None => self.fork_name(&source.name),
                };
                if name.is_empty() {
                    return Err(rpc("invalid_params", "name must not be empty"));
                }
                // `None` means auto: a fork gets its own worktree when the
                // source is in a git work tree, so the branches can edit files
                // independently. `Some(false)` opts out and shares `cwd`.
                let want_worktree = match params.worktree {
                    Some(value) => value,
                    None => crate::worktree::repo_root(&source.cwd).await.is_some(),
                };
                let mut base_branch = None;
                let created = if want_worktree {
                    let repo = crate::worktree::main_repo(&source.cwd)
                        .await
                        .or_else(|| Some(source.cwd.clone()))
                        .unwrap_or_else(|| source.cwd.clone());
                    // A fork of a worktree session should branch from that
                    // session's base branch, not from its temporary `eix/...`
                    // branch. A shared source has no temporary branch, so use
                    // the branch the user is on.
                    let stored_base = self
                        .store
                        .get_integration(&source.id)?
                        .and_then(|state| state.base_branch);
                    let base = match stored_base {
                        Some(base) => base,
                        None if source.has_worktree() => self.base_for_repo(&repo).await?,
                        None => self.base_for_session(&source.cwd, &repo).await?,
                    };
                    let created =
                        crate::worktree::create_from_base(&source.cwd, &name, Some(&base)).await?;
                    base_branch = Some(base);
                    Some(created)
                } else {
                    None
                };
                let forked = self.store.fork_session(
                    &source.id,
                    params.at_seq,
                    &name,
                    created.as_ref().map(|created| created.path.as_path()),
                    created.as_ref().map(|created| created.path.as_path()),
                );
                let session = match forked {
                    Ok(Some(session)) => session,
                    Ok(None) => {
                        if let Some(created) = &created {
                            let _ = crate::worktree::remove(&created.path, true).await;
                        }
                        return Err(rpc("not_found", format!("unknown session {}", source.id)));
                    }
                    Err(err) => {
                        if let Some(created) = &created {
                            let _ = crate::worktree::remove(&created.path, true).await;
                        }
                        return Err(err);
                    }
                };
                let integration = self.store.ensure_integration(
                    &session.id,
                    base_branch.as_deref(),
                    !want_worktree,
                )?;
                self.save_integration(&integration)?;
                self.emit_session(&session);
                Ok(serde_json::to_value(SessionResult { session })?)
            }
            "session.rewind" => {
                let params: RewindParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(self.rewind_session(params).await?)?)
            }
            "session.worktree.remove" => {
                let params: WorktreeRemoveParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(self.remove_worktree(params).await?)?)
            }
            "session.ready" => {
                let params: SessionReadyParams = parse_params(request.params.clone())?;
                let state = self
                    .request_ready(&params.session_id, params.auto_commit)
                    .await?;
                Ok(serde_json::to_value(state)?)
            }
            "integration.list" => {
                let params: IntegrationListParams = parse_params(request.params.clone())?;
                let mut integrations = self.store.list_integrations()?;
                if let Some(session_id) = params.session_id.as_deref() {
                    self.require_session(session_id)?;
                    integrations.retain(|state| state.session_id == session_id);
                }
                Ok(serde_json::to_value(IntegrationListResult {
                    integrations,
                })?)
            }
            "integration.continue" => {
                let params: IntegrationContinueParams = parse_params(request.params.clone())?;
                self.require_session(&params.session_id)?;
                let state = self.integration_continue(&params.session_id)?;
                Ok(serde_json::to_value(state)?)
            }
            "session.get" => {
                let params: SessionRef = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(SessionResult {
                    session: self.require_session(&params.session_id)?,
                })?)
            }
            "session.attach" => {
                let params: AttachParams = parse_params(request.params.clone())?;
                let session = self.require_session(&params.session_id)?;
                let (events, has_more) = match params.tail {
                    Some(tail) => self
                        .store
                        .events_before(&params.session_id, u64::MAX, tail)?,
                    None => (self.store.events(&params.session_id)?, false),
                };
                Ok(serde_json::to_value(AttachResult {
                    session,
                    events,
                    has_more,
                })?)
            }
            "session.events" => {
                let params: EventsParams = parse_params(request.params.clone())?;
                self.require_session(&params.session_id)?;
                let (events, has_more) = match params.before_seq {
                    Some(before_seq) => self.store.events_before(
                        &params.session_id,
                        before_seq,
                        params.limit.unwrap_or(usize::MAX),
                    )?,
                    None => {
                        let mut events = self
                            .store
                            .events_after(&params.session_id, params.after_seq)?;
                        let limit = params.limit.map(|limit| limit.max(1));
                        let has_more = limit.is_some_and(|limit| events.len() > limit);
                        if let Some(limit) = limit {
                            events.truncate(limit);
                        }
                        (events, has_more)
                    }
                };
                Ok(serde_json::to_value(EventsResult { events, has_more })?)
            }
            "context.compact" => {
                let params: ContextCompactParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(self.context_compact(params).await?)?)
            }
            "context.clear" => {
                let params: ContextClearParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(self.context_clear(params).await?)?)
            }
            "search.query" => {
                let params: SearchParams = parse_params(request.params.clone())?;
                let query = params.query.trim();
                if query.is_empty() {
                    return Err(rpc("invalid_params", "query must not be empty"));
                }
                let hits = self.store.search(
                    query,
                    params.session_id.as_deref(),
                    params.limit.unwrap_or(20),
                )?;
                Ok(serde_json::to_value(SearchResult {
                    query: query.to_string(),
                    hits,
                })?)
            }
            "picker.query" => {
                let params: PickerParams = parse_params(request.params.clone())?;
                if !eix_core::is_data_kind(&params.kind) {
                    return Err(rpc(
                        "invalid_params",
                        format!("unknown picker kind {:?}", params.kind),
                    ));
                }
                let session_id = params.session_id.as_deref().ok_or_else(|| {
                    rpc(
                        "invalid_params",
                        format!("session_id is required for {}", params.kind),
                    )
                })?;
                let session = self.require_session(session_id)?;
                let result = match params.kind.as_str() {
                    eix_core::KIND_FILES => {
                        crate::picker::files(&session.cwd, params.query.as_deref(), params.limit)
                            .await
                    }
                    eix_core::KIND_WORKTREES => {
                        crate::picker::worktrees(
                            &session.cwd,
                            params.query.as_deref(),
                            params.limit,
                        )
                        .await
                    }
                    eix_core::KIND_BRANCHES => {
                        crate::picker::branches(&session.cwd, params.query.as_deref(), params.limit)
                            .await
                    }
                    _ => unreachable!("validated data kind"),
                };
                Ok(serde_json::to_value(result)?)
            }
            "picker.preview" => {
                let params: PickerPreviewParams = parse_params(request.params.clone())?;
                let preview = match params.kind.as_str() {
                    eix_core::KIND_FILES => {
                        let session_id = params.session_id.as_deref().ok_or_else(|| {
                            rpc("invalid_params", "session_id is required for files")
                        })?;
                        let session = self.require_session(session_id)?;
                        crate::picker::file_preview(
                            &session.cwd,
                            &params.value,
                            self.config.load().as_ref(),
                            params.limit,
                        )
                        .await?
                    }
                    eix_core::KIND_SESSION_PREVIEW => {
                        self.require_session(&params.value)?;
                        // A long turn may end with many tool events. Fetch a
                        // wider window than the rendered message count so the
                        // preview still finds recent user/assistant turns.
                        let wanted = params.limit.unwrap_or(120).clamp(1, 500);
                        let fetch = wanted.saturating_mul(4).min(2_000);
                        let (events, _) =
                            self.store.events_before(&params.value, u64::MAX, fetch)?;
                        crate::picker::session_preview(&params.value, &events, Some(wanted))
                    }
                    other => {
                        return Err(rpc(
                            "invalid_params",
                            format!("unknown preview kind {other:?}"),
                        ))
                    }
                };
                Ok(serde_json::to_value(PickerPreviewResult {
                    kind: preview.kind,
                    value: preview.value,
                    text: preview.text,
                })?)
            }
            "process.list" => {
                let params: ProcessListParams = parse_params(request.params.clone())?;
                if let Some(session_id) = params.session_id.as_deref() {
                    self.require_session(session_id)?;
                }
                Ok(serde_json::to_value(ProcessList {
                    processes: self.processes.list(params.session_id.as_deref()),
                })?)
            }
            "process.output" => {
                let params: ProcessOutputParams = parse_params(request.params.clone())?;
                let max_bytes = params.max_bytes.unwrap_or(16 * 1024).clamp(1, 512 * 1024);
                Ok(serde_json::to_value(
                    self.processes.output(&params.process_id, max_bytes)?,
                )?)
            }
            "process.kill" => {
                let params: ProcessKillParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(ProcessKillResult {
                    killed: self.processes.kill(&params.process_id)?,
                })?)
            }
            "session.rename" => {
                let params: RenameParams = parse_params(request.params.clone())?;
                let name = params.name.trim();
                if name.is_empty() {
                    return Err(rpc("invalid_params", "name must not be empty"));
                }
                let session = self
                    .store
                    .rename_session(&params.session_id, name)?
                    .ok_or_else(|| {
                        rpc(
                            "not_found",
                            format!("unknown session {}", params.session_id),
                        )
                    })?;
                self.emit_session(&session);
                Ok(serde_json::to_value(SessionResult { session })?)
            }
            "session.permission" => {
                let params: PermissionParams = parse_params(request.params.clone())?;
                let session = self.set_permission(params)?;
                Ok(serde_json::to_value(SessionResult { session })?)
            }
            "session.cd" => {
                let params: SessionCdParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(SessionResult {
                    session: self.set_cwd(params)?,
                })?)
            }
            "session.btw" => {
                let params: BtwParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(self.btw(params).await?)?)
            }
            "btw.cancel" => {
                let params: SessionRef = parse_params(request.params.clone())?;
                let flag = self.btws.lock().await.get(&params.session_id).cloned();
                if let Some(flag) = &flag {
                    flag.store(true, Ordering::SeqCst);
                }
                Ok(json!({ "cancelled": flag.is_some() }))
            }
            "draft.get" => {
                let params: DraftParams = parse_params(request.params.clone())?;
                self.require_session(&params.session_id)?;
                let (text, updated_at) = self.store.get_draft(&params.session_id)?;
                Ok(serde_json::to_value(DraftResult {
                    session_id: params.session_id,
                    text,
                    updated_at,
                })?)
            }
            "draft.set" => {
                let params: DraftParams = parse_params(request.params.clone())?;
                self.require_session(&params.session_id)?;
                let text = params.text.unwrap_or_default();
                let updated_at = self.store.set_draft(&params.session_id, &text)?;
                Ok(serde_json::to_value(DraftResult {
                    session_id: params.session_id,
                    text,
                    updated_at,
                })?)
            }
            "prompt.submit" => {
                let params: SubmitParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(self.submit(params).await?)?)
            }
            "prompt.cancel" => {
                let params: SessionRef = parse_params(request.params.clone())?;
                let flag = self.turns.lock().await.get(&params.session_id).cloned();
                let cancelled = flag.is_some();
                if let Some(flag) = flag {
                    flag.store(true, Ordering::SeqCst);
                }
                // Unblock a turn waiting on approval; it will notice the flag.
                if let Some(pending) = self.approvals.lock().await.remove(&params.session_id) {
                    let _ = pending.responder.send(ApprovalReply {
                        decision: ApprovalDecision::Deny,
                        reason: Some("turn cancelled".to_string()),
                        plan: None,
                        hunks: None,
                        scope: ApprovalScope::Once,
                    });
                }
                // Drop a pending question; the waiting `ask_user` sees a closed
                // channel and reports that the user did not answer.
                self.inputs.lock().await.remove(&params.session_id);
                Ok(serde_json::to_value(CancelResult { cancelled })?)
            }
            "approval.list" => {
                let params: ApprovalListParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(self.list_approvals(params).await?)?)
            }
            "approval.respond" => {
                let params: ApprovalRespondParams = parse_params(request.params.clone())?;
                let result = self.respond_approval(params).await?;
                Ok(serde_json::to_value(result)?)
            }
            "input.respond" => {
                let params: InputRespondParams = parse_params(request.params.clone())?;
                let result = self.respond_input(params).await?;
                Ok(serde_json::to_value(result)?)
            }
            "patch.apply" => {
                let params: PatchApplyParams = parse_params(request.params.clone())?;
                let result = self
                    .decide_patch(
                        &params.session_id,
                        &params.patch_id,
                        ApprovalDecision::Approve,
                        params.hunks,
                        None,
                        ApprovalScope::Once,
                    )
                    .await?;
                Ok(serde_json::to_value(result)?)
            }
            "patch.get" => {
                let params: PatchGetParams = parse_params(request.params.clone())?;
                let patch = self
                    .staging
                    .get(&params.session_id, &params.patch_id)
                    .ok_or_else(|| {
                        rpc("not_found", format!("unknown patch {}", params.patch_id))
                    })?;
                Ok(serde_json::to_value(PatchGetResult {
                    session_id: params.session_id,
                    patch_id: params.patch_id,
                    path: patch.label,
                    text: patch.new,
                })?)
            }
            "patch.revise" => {
                let params: PatchReviseParams = parse_params(request.params.clone())?;
                let session = self.require_session(&params.session_id)?;
                let Some(patch) = self.staging.get(&params.session_id, &params.patch_id) else {
                    return Err(rpc(
                        "not_found",
                        format!("unknown patch {}", params.patch_id),
                    ));
                };
                let plan = tools::build_plan(
                    patch.path.clone(),
                    patch.label.clone(),
                    patch.old.clone(),
                    params.text,
                    &self.config.load(),
                );
                let Some(updated) = self.staging.update_content(
                    &params.session_id,
                    &params.patch_id,
                    plan.new_content().to_string(),
                )?
                else {
                    return Err(rpc(
                        "not_found",
                        format!("unknown patch {}", params.patch_id),
                    ));
                };
                let event = self.store.append_event(
                    &params.session_id,
                    EventKind::PatchProposed,
                    payload(&PatchProposedPayload {
                        patch_id: updated.patch_id.clone(),
                        call_id: updated.call_id.clone(),
                        tool: updated.tool.clone(),
                        path: updated.label.clone(),
                        summary: format!("{} (edited)", updated.label),
                        diff: Some(plan.diff().to_string()),
                        hunks: plan.hunk_infos(),
                    }),
                )?;
                self.emit_event(&event);
                self.emit_value(
                    "patch.proposed",
                    json!({
                        "session_id": params.session_id,
                        "patch_id": updated.patch_id,
                        "call_id": updated.call_id,
                        "tool": updated.tool,
                        "path": updated.label,
                        "summary": format!("{} (edited)", updated.label),
                        "diff": plan.diff(),
                        "hunks": plan.hunk_infos(),
                    }),
                );
                self.announce_patch(&session);
                Ok(serde_json::to_value(PatchResult {
                    accepted: true,
                    applied: false,
                    conflict: false,
                    message: Some("patch updated".to_string()),
                })?)
            }
            "patch.reject" => {
                let params: PatchRejectParams = parse_params(request.params.clone())?;
                let result = self
                    .decide_patch(
                        &params.session_id,
                        &params.patch_id,
                        ApprovalDecision::Deny,
                        None,
                        params.reason,
                        ApprovalScope::Once,
                    )
                    .await?;
                Ok(serde_json::to_value(result)?)
            }
            other => Err(rpc("method_not_found", format!("unknown method {other:?}"))),
        }
    }

    async fn list_approvals(&self, params: ApprovalListParams) -> Result<ApprovalListResult> {
        let want = params.session_id.as_deref();
        let mut approvals = Vec::new();
        // Blocking tool calls, one per session.
        let pending = {
            let mut out = Vec::new();
            for (session_id, item) in self.approvals.lock().await.iter() {
                out.push((
                    session_id.clone(),
                    item.approval_id.clone(),
                    item.tool.clone(),
                    item.summary.clone(),
                    item.created_ms,
                ));
            }
            out
        };
        for (session_id, approval_id, tool, summary, created_ms) in pending {
            if want.is_some_and(|want| want != session_id.as_str()) {
                continue;
            }
            let Some(session) = self.store.get_session(&session_id)? else {
                continue;
            };
            approvals.push(ApprovalInfo {
                session_id,
                session_name: session.name,
                approval_id,
                kind: "tool".to_string(),
                tool,
                summary,
                created_ms,
            });
        }
        // Staged writes survive across turns, so list them too.
        for patch in self.staging.all() {
            if want.is_some_and(|want| want != patch.session_id.as_str()) {
                continue;
            }
            let Some(session) = self.store.get_session(&patch.session_id)? else {
                continue;
            };
            approvals.push(ApprovalInfo {
                session_id: patch.session_id,
                session_name: session.name,
                approval_id: patch.patch_id,
                kind: "patch".to_string(),
                tool: patch.tool,
                summary: patch.label,
                created_ms: patch.created_ms,
            });
        }
        approvals.sort_by_key(|item| item.created_ms);
        Ok(ApprovalListResult { approvals })
    }

    async fn respond_approval(&self, params: ApprovalRespondParams) -> Result<ApprovalResult> {
        let ApprovalRespondParams {
            session_id,
            approval_id,
            decision,
            reason,
            hunks,
            scope,
        } = params;
        {
            let mut approvals = self.approvals.lock().await;
            if let Some(pending) = approvals.remove(&session_id) {
                if !approval_id.is_empty() && pending.approval_id != approval_id {
                    // Stale card: keep waiting for the current approval.
                    approvals.insert(session_id, pending);
                    return Ok(ApprovalResult { accepted: false });
                }
                let decision = if decision == ApprovalDecision::Always && !pending.allow_always {
                    ApprovalDecision::Approve
                } else {
                    decision
                };
                let _ = pending.responder.send(ApprovalReply {
                    decision,
                    reason,
                    plan: None,
                    hunks,
                    scope,
                });
                return Ok(ApprovalResult { accepted: true });
            }
        }
        // No blocking tool call: the id must belong to a staged patch.
        let result = self
            .decide_patch(&session_id, &approval_id, decision, hunks, reason, scope)
            .await?;
        Ok(ApprovalResult {
            accepted: result.accepted,
        })
    }

    async fn respond_input(&self, params: InputRespondParams) -> Result<InputResult> {
        let mut inputs = self.inputs.lock().await;
        let Some(pending) = inputs.remove(&params.session_id) else {
            return Ok(InputResult { accepted: false });
        };
        if !params.input_id.is_empty() && pending.input_id != params.input_id {
            // Stale card: keep waiting for the current question.
            inputs.insert(params.session_id, pending);
            return Ok(InputResult { accepted: false });
        }
        if params.option_index >= pending.options {
            inputs.insert(params.session_id, pending);
            return Ok(InputResult { accepted: false });
        }
        let _ = pending.responder.send(params.option_index);
        Ok(InputResult { accepted: true })
    }

    async fn create_session(&self, params: CreateSessionParams) -> Result<Session> {
        let cwd = match params.cwd {
            Some(cwd) => cwd,
            None => std::env::current_dir().context("cannot determine current directory")?,
        };
        let name = params
            .name
            .map(|name| name.trim().to_string())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| default_name(&cwd));
        let mut session = Session::new(Uuid::new_v4().to_string(), name, cwd);
        session.model = params
            .model
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty());
        if let Some(permission) = params.permission {
            session.permission = permission;
        }
        if let Some(agent) = params
            .agent
            .map(|agent| agent.trim().to_string())
            .filter(|agent| !agent.is_empty())
        {
            session.agent = Some(agent);
        }
        let worktree_mode = params.worktree;
        let disabled = match worktree_mode {
            Some(false) => true,
            None => !self.config.load().worktree.auto,
            Some(true) => false,
        };
        let mut base_branch = None;
        if worktree_mode == Some(true) {
            let repo = crate::worktree::main_repo(&session.cwd)
                .await
                .or_else(|| Some(session.cwd.clone()))
                .unwrap_or_else(|| session.cwd.clone());
            let base = self.base_for_session(&session.cwd, &repo).await?;
            let created =
                crate::worktree::create_from_base(&session.cwd, &session.name, Some(&base)).await?;
            session.cwd = created.path.clone();
            session.worktree = Some(created.path);
            base_branch = Some(base);
        } else if !disabled {
            // Lazy worktrees are created on the first prompt, but the base
            // branch must be the one the user was on when the session was
            // created. Failure here is deferred: the first prompt will retry
            // and report a useful error.
            if let Some(repo) = crate::worktree::main_repo(&session.cwd).await {
                base_branch = self.base_for_session(&session.cwd, &repo).await.ok();
            }
        }
        self.store.create_session(&session)?;
        let integration =
            self.store
                .ensure_integration(&session.id, base_branch.as_deref(), disabled)?;
        self.save_integration(&integration)?;
        self.emit_session(&session);
        Ok(session)
    }

    /// The level that actually applies right now: a temporary in-memory
    /// override wins over the persisted session row.
    fn session_permission(&self, session: &Session) -> PermissionLevel {
        let base = self
            .temporary_permissions
            .lock()
            .ok()
            .and_then(|overrides| overrides.get(&session.id).copied())
            .unwrap_or(session.permission);
        let Some(name) = session.agent.as_deref() else {
            return base;
        };
        let config = self.config.load();
        let Some(agent) = eix_core::find_agent(&session.cwd, name, &config.compations) else {
            return base;
        };
        match agent.permission {
            Some(level) => base.restrict(level),
            None => base,
        }
    }

    /// Apply `session.model`. Pinning `None` restores the daemon default.
    fn set_model(&self, params: SessionModelParams) -> Result<Session> {
        let session = self.require_session(&params.session_id)?;
        let model = params
            .model
            .map(|model| model.trim().to_string())
            .filter(|model| !model.is_empty());
        let updated = self
            .store
            .set_session_model(&session.id, model.as_deref())?
            .ok_or_else(|| rpc("not_found", "no such session"))?;
        self.emit_session(&updated);
        Ok(updated)
    }

    /// Apply `session.agent`. Only primary-capable agents may be selected.
    fn set_agent(&self, params: SessionAgentParams) -> Result<Session> {
        let session = self.require_session(&params.session_id)?;
        let agent = params
            .agent
            .map(|agent| agent.trim().to_string())
            .filter(|agent| !agent.is_empty());
        if let Some(name) = &agent {
            let config = self.config.load();
            let Some(spec) = eix_core::find_agent(&session.cwd, name, &config.compations) else {
                return Err(rpc("invalid_params", format!("unknown agent {name:?}")));
            };
            if !spec.mode.can_primary() {
                return Err(rpc(
                    "invalid_params",
                    format!("agent {name:?} is a sub-agent and cannot be primary"),
                ));
            }
        }
        let updated = self
            .store
            .set_session_agent(&session.id, agent.as_deref())?
            .ok_or_else(|| rpc("not_found", "no such session"))?;
        self.emit_session(&updated);
        Ok(updated)
    }

    /// Apply `session.cd`. The new directory becomes the workspace for later
    /// turns; tools and the ledger read it from the session row.
    fn set_cwd(&self, params: SessionCdParams) -> Result<Session> {
        let session = self.require_session(&params.session_id)?;
        if session.status.is_busy() {
            return Err(rpc(
                "busy",
                "wait for the turn to finish before changing cwd",
            ));
        }
        let raw = params.path.trim();
        if raw.is_empty() {
            return Err(rpc("invalid_params", "path must not be empty"));
        }
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let expanded = if raw == "~" {
            home.clone().unwrap_or_else(|| PathBuf::from(raw))
        } else if let Some(rest) = raw.strip_prefix("~/") {
            home.map(|home| home.join(rest))
                .unwrap_or_else(|| PathBuf::from(raw))
        } else {
            PathBuf::from(raw)
        };
        let candidate = if expanded.is_absolute() {
            expanded
        } else {
            session.cwd.join(expanded)
        };
        let cwd = candidate.canonicalize().map_err(|err| {
            rpc(
                "invalid_params",
                format!("cannot open {}: {err}", candidate.display()),
            )
        })?;
        if !cwd.is_dir() {
            return Err(rpc(
                "invalid_params",
                format!("{} is not a directory", cwd.display()),
            ));
        }
        // Keep the worktree marker only while the session stays inside it.
        let worktree = session
            .worktree
            .as_ref()
            .filter(|worktree| cwd.starts_with(worktree))
            .map(PathBuf::as_path);
        let updated = self
            .store
            .set_session_cwd(&session.id, &cwd, worktree)?
            .ok_or_else(|| rpc("not_found", format!("unknown session {}", session.id)))?;
        self.emit_session(&updated);
        Ok(updated)
    }

    /// Start a `/btw` side question. The answer streams as `btw.*`
    /// notifications and never touches the session's event log or ledger.
    async fn btw(self: &Arc<Self>, params: BtwParams) -> Result<BtwResult> {
        let question = params.question.trim().to_string();
        if question.is_empty() {
            return Err(rpc("invalid_params", "question must not be empty"));
        }
        let session = self.require_session(&params.session_id)?;
        let btw_id = Uuid::new_v4().to_string();
        {
            let mut btws = self.btws.lock().await;
            if btws.contains_key(&session.id) {
                return Err(rpc("busy", "a side question is already running"));
            }
            btws.insert(session.id.clone(), Arc::new(AtomicBool::new(false)));
        }
        let daemon = Arc::clone(self);
        let session_id = session.id.clone();
        let id = btw_id.clone();
        let model = session
            .model
            .clone()
            .unwrap_or_else(|| self.config.load().default_model().to_string());
        tokio::spawn(async move {
            daemon.run_btw(session_id, id, question, model).await;
        });
        Ok(BtwResult { btw_id })
    }

    async fn run_btw(
        self: Arc<Self>,
        session_id: String,
        btw_id: String,
        question: String,
        model: String,
    ) {
        let cancel = self
            .btws
            .lock()
            .await
            .get(&session_id)
            .cloned()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        let result = self
            .run_btw_inner(&session_id, &btw_id, &question, &model, &cancel)
            .await;
        match result {
            Ok(text) => self.emit_value(
                "btw.completed",
                json!({
                    "session_id": session_id,
                    "btw_id": btw_id,
                    "text": text,
                    "error": Value::Null,
                }),
            ),
            Err(err) => self.emit_value(
                "btw.completed",
                json!({
                    "session_id": session_id,
                    "btw_id": btw_id,
                    "text": "",
                    "error": format!("{err:#}"),
                }),
            ),
        }
        self.btws.lock().await.remove(&session_id);
    }

    async fn run_btw_inner(
        &self,
        session_id: &str,
        btw_id: &str,
        question: &str,
        model: &str,
        cancel: &AtomicBool,
    ) -> Result<String> {
        let session = self.require_session(session_id)?;
        let permission = self.session_permission(&session);
        let system = format!(
            "{}\n\nThis is a one-off side question from the user. Answer it directly and \
             concisely. It is not part of the main conversation, so do not call tools and do \
             not refer to it as a new task.",
            system_prompt(&self.config.load(), &session, permission)
        );
        let messages = vec![
            ChatMessage::system(system),
            ChatMessage::user(question.to_string()),
        ];
        let bus = self.bus.clone();
        let sid = session_id.to_string();
        let id = btw_id.to_string();
        let mut text = String::new();
        let route = self.config.load().route(model);
        let provider = self.provider.load_full();
        let completion = provider
            .stream_chat(&messages, &route, &[], cancel, |delta| {
                if let crate::provider::StreamDelta::Content(chunk) = delta {
                    text.push_str(chunk);
                    let _ = bus.send(Notification::new(
                        "btw.delta",
                        json!({
                            "session_id": sid,
                            "btw_id": id,
                            "delta": chunk,
                        }),
                    ));
                }
            })
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        if cancel.load(Ordering::SeqCst) {
            bail!("cancelled");
        }
        Ok(if completion.text.trim().is_empty() {
            text
        } else {
            completion.text
        })
    }

    /// Apply `session.permission`. Persisted changes update the row; temporary
    /// ones stay in memory. Both append a `permission` event so the change is
    /// visible in the transcript and audit trail.
    fn set_permission(&self, params: PermissionParams) -> Result<Session> {
        let mut session = self.require_session(&params.session_id)?;
        let from = self.session_permission(&session);
        let to = params.permission;
        if params.temporary {
            if let Ok(mut overrides) = self.temporary_permissions.lock() {
                overrides.insert(session.id.clone(), to);
            }
            // Report/broadcast the effective level; the row keeps the persisted
            // one so a restart reverts the escalation.
            session.permission = to;
        } else {
            if let Ok(mut overrides) = self.temporary_permissions.lock() {
                overrides.remove(&session.id);
            }
            session = self
                .store
                .set_session_permission(&session.id, to)?
                .ok_or_else(|| {
                    rpc(
                        "not_found",
                        format!("unknown session {}", params.session_id),
                    )
                })?;
        }
        if from != to {
            let event = self.store.append_event(
                &session.id,
                EventKind::Permission,
                payload(&PermissionPayload {
                    from,
                    to,
                    temporary: params.temporary,
                }),
            )?;
            self.emit_event(&event);
        }
        self.emit_session(&session);
        Ok(session)
    }

    /// `name~2`, `name~3`, ... The root drops an existing numeric `~N`, so
    /// forking a fork keeps one family instead of nesting suffixes.
    fn fork_name(&self, source_name: &str) -> String {
        let root = match source_name.rsplit_once('~') {
            Some((head, tail))
                if !head.is_empty()
                    && !tail.is_empty()
                    && tail.chars().all(|c| c.is_ascii_digit()) =>
            {
                head
            }
            _ => source_name,
        };
        let taken: HashSet<String> = self
            .store
            .list_sessions()
            .map(|sessions| sessions.into_iter().map(|session| session.name).collect())
            .unwrap_or_default();
        let mut n = 2;
        loop {
            let candidate = format!("{root}~{n}");
            if !taken.contains(&candidate) {
                return candidate;
            }
            n += 1;
        }
    }

    /// Fork the session, then roll the shared workspace back to how it looked
    /// just before the cut. Snapshots are captured before every mutating tool
    /// call, so applying the ones at or after the cut in reverse order leaves
    /// the files as they were before the discarded branch ran.
    ///
    /// Rollback is best-effort: files that were never captured, changes made
    /// outside eix, and failed restores are reported instead of hidden.
    async fn rewind_session(&self, params: RewindParams) -> Result<RewindResult> {
        let source = self.require_session(&params.session_id)?;
        if source.status.is_busy() {
            return Err(rpc(
                "busy",
                format!("session {} is {}", source.id, source.status.as_str()),
            ));
        }
        let max = self.store.max_seq(&source.id)?;
        let upto = match params.at_seq {
            Some(at) if at >= 1 && at <= max + 1 => at,
            Some(_) => {
                return Err(rpc(
                    "invalid_params",
                    format!("at_seq must be between 1 and {}", max + 1),
                ))
            }
            None => max + 1,
        };
        let name = match params.name {
            Some(name) => name.trim().to_string(),
            None => self.fork_name(&source.name),
        };
        if name.is_empty() {
            return Err(rpc("invalid_params", "name must not be empty"));
        }

        // Collect the snapshots taken at or after the cut before forking: they
        // are exactly the mutations the new branch discards.
        let mut snapshots: Vec<(u64, String, usize)> = Vec::new();
        for event in self.store.events(&source.id)? {
            if event.kind != EventKind::Snapshot || event.seq < upto {
                continue;
            }
            let Ok(snapshot) = serde_json::from_value::<SnapshotPayload>(event.payload) else {
                continue;
            };
            if snapshot.ok && !snapshot.snapshot_id.is_empty() {
                snapshots.push((event.seq, snapshot.snapshot_id, snapshot.skipped));
            }
        }
        // Oldest first so the reverse walk applies the earliest snapshot last.
        snapshots.sort_by_key(|(seq, _, _)| *seq);

        let session = self
            .store
            .fork_session(&source.id, Some(upto), &name, None, None)?
            .ok_or_else(|| rpc("not_found", format!("unknown session {}", source.id)))?;
        self.emit_session(&session);

        let mut result = RewindResult {
            session,
            restored: true,
            snapshots: snapshots.len(),
            files_restored: 0,
            files_removed: 0,
            files_missing: 0,
            warnings: Vec::new(),
        };

        if !self.config.load().tools.snapshots {
            result.restored = false;
            result
                .warnings
                .push("tools.snapshots is disabled, so the workspace was not rolled back".into());
            return Ok(result);
        }
        if snapshots.is_empty() {
            result.warnings.push(
                "no snapshots after the cut; files changed outside tool calls were left as they are"
                    .into(),
            );
            return Ok(result);
        }

        let cwd = result.session.cwd.clone();
        let mut failed = 0usize;
        let mut skipped = 0usize;
        for (_, snapshot_id, skipped_files) in snapshots.iter().rev() {
            skipped += skipped_files;
            match self.snapshots.restore(&source.id, snapshot_id, &cwd).await {
                Ok(report) => {
                    result.files_restored += report.restored;
                    result.files_removed += report.removed;
                    result.files_missing += report.missing;
                }
                Err(err) => {
                    failed += 1;
                    result.warnings.push(format!(
                        "snapshot {snapshot_id} could not be restored: {err:#}"
                    ));
                }
            }
        }
        if skipped > 0 {
            result.warnings.push(format!(
                "{skipped} file(s) were too large or excluded and were never captured, so they were not rolled back"
            ));
        }
        if result.files_missing > 0 {
            result.warnings.push(format!(
                "{} pre-existing file(s) could not be put back and may be missing",
                result.files_missing
            ));
        }
        result.warnings.push(
            "only tool-call mutations are snapshotted; changes made outside eix are not detected"
                .into(),
        );
        result.restored = failed == 0 && result.files_missing == 0;
        Ok(result)
    }

    /// Remove the git worktree bound to a session. Without `force` git refuses
    /// a dirty worktree; that refusal is returned as `removed: false` so the
    /// caller can surface it and ask for confirmation.
    async fn remove_worktree(&self, params: WorktreeRemoveParams) -> Result<WorktreeRemoveResult> {
        // Keep the queue from claiming this session between the busy check and
        // the actual `git worktree remove`.
        let _claim = self.integration_lock.lock().await;
        let session = self.require_session(&params.session_id)?;
        if session.status.is_busy() {
            return Err(rpc(
                "busy",
                format!("session {} is {}", session.id, session.status.as_str()),
            ));
        }
        if self
            .store
            .get_integration(&session.id)?
            .is_some_and(|state| state.status.is_active())
        {
            return Err(rpc(
                "busy",
                format!(
                    "session {} integration is active; finish or `eix integrate continue` first",
                    session.id
                ),
            ));
        }
        let Some(path) = session.worktree.clone() else {
            return Err(rpc(
                "invalid_params",
                format!("session {} has no worktree", session.id),
            ));
        };
        // Resolve the main repo before removal: once the directory is gone,
        // `git -C <path>` cannot answer.
        let repo = crate::worktree::main_repo(&path).await;
        match crate::worktree::remove(&path, params.force).await {
            Ok(()) => {
                let fallback = path.parent().map(Path::to_path_buf);
                let cwd = repo.or(fallback).unwrap_or_else(|| path.clone());
                if let Some(updated) = self.store.set_session_cwd(&session.id, &cwd, None)? {
                    self.emit_session(&updated);
                }
                if let Some(mut state) = self.store.get_integration(&session.id)? {
                    state.status = IntegrationStatus::Pending;
                    state.stage = None;
                    state.error = None;
                    state.next_retry_at = None;
                    state.worktree_disabled = true;
                    state.updated_at = now_ms();
                    self.save_integration(&state)?;
                }
                Ok(WorktreeRemoveResult {
                    removed: true,
                    message: format!("removed {}", path.display()),
                })
            }
            Err(err) => Ok(WorktreeRemoveResult {
                removed: false,
                message: format!("{err:#}; re-run with --force to discard uncommitted changes"),
            }),
        }
    }

    async fn submit(self: &Arc<Self>, params: SubmitParams) -> Result<SubmitResult> {
        self.submit_inner(params, false).await
    }

    /// `internal` repair turns keep the integration state in `repairing`
    /// instead of resetting it to `pending` like a normal user prompt.
    async fn submit_inner(
        self: &Arc<Self>,
        params: SubmitParams,
        internal: bool,
    ) -> Result<SubmitResult> {
        let text = params.text.trim().to_string();
        if text.is_empty() && params.images.is_empty() {
            return Err(rpc("invalid_params", "prompt must not be empty"));
        }
        let mut session = self.require_session(&params.session_id)?;
        let images = load_images(
            &session.cwd,
            self.config.load().as_ref(),
            &params.images,
            &self.blobs,
        )
        .await?;

        // Auto worktrees are created lazily: the first prompt is the first
        // point where a meaningful English name can be generated.
        session = self.ensure_auto_worktree(&session.id, &text).await?;

        // The queue takes the same lock while it claims an item. Keep the
        // critical section short: a few queries and the turn registration.
        let integration_guard = if internal {
            None
        } else {
            Some(self.integration_lock.lock().await)
        };
        if !internal {
            if let Some(state) = self.store.get_integration(&session.id)? {
                // `repairing` means the queue is waiting for the agent; a
                // user prompt may take over (the in-flight repair turn is
                // still guarded by the turn lock below).
                if state.status.is_active() && state.status != IntegrationStatus::Repairing {
                    return Err(rpc(
                        "busy",
                        format!(
                            "session {} integration is {}",
                            session.id,
                            state.status.as_str()
                        ),
                    ));
                }
            }
        }

        let cancel = {
            let mut turns = self.turns.lock().await;
            if turns.contains_key(&session.id) {
                return Err(rpc(
                    "busy",
                    format!("session {} is already running", session.id),
                ));
            }
            let flag = Arc::new(AtomicBool::new(false));
            turns.insert(session.id.clone(), Arc::clone(&flag));
            flag
        };

        if !internal {
            if let Some(mut state) = self.store.get_integration(&session.id)? {
                if state.status != IntegrationStatus::Pending {
                    state.status = IntegrationStatus::Pending;
                    state.stage = None;
                    state.next_retry_at = None;
                    state.error = None;
                    state.attempts = 0;
                    state.updated_at = now_ms();
                    if let Err(err) = self.save_integration(&state) {
                        self.turns.lock().await.remove(&session.id);
                        return Err(err);
                    }
                }
            }
        }
        drop(integration_guard);

        let session_id = session.id.clone();
        let setup = self.submit_setup(session_id.clone(), text, images).await;
        let (session, turn_id) = match setup {
            Ok(setup) => setup,
            Err(err) => {
                self.turns.lock().await.remove(&session_id);
                return Err(err);
            }
        };

        self.emit_value(
            "turn.started",
            json!({
                "session_id": session.id.clone(),
                "turn_id": turn_id.clone(),
                "model": session.model.clone().unwrap_or_else(|| self.config.load().default_model().to_string()),
            }),
        );

        let daemon = Arc::clone(self);
        let session_id = session.id.clone();
        let spawned_turn = turn_id.clone();
        tokio::spawn(async move {
            daemon.run_turn(session_id, spawned_turn, cancel).await;
        });

        Ok(SubmitResult { session, turn_id })
    }

    /// Modification times of every config layer, used by the reload watcher.
    fn config_mtimes(&self) -> Vec<(PathBuf, Option<SystemTime>)> {
        self.config
            .load()
            .watch_paths
            .iter()
            .map(|path| {
                let modified = fs::metadata(path).and_then(|meta| meta.modified()).ok();
                (path.clone(), modified)
            })
            .collect()
    }

    /// Merged static catalog plus provider `/models` results. Fetch failures
    /// are warnings, never hard errors: a local or offline provider must not
    /// hide the models that are configured explicitly.
    async fn model_catalog(&self) -> (Vec<eix_core::ModelInfo>, Vec<String>) {
        const CACHE_TTL: Duration = Duration::from_secs(300);
        const FAILED_CACHE_TTL: Duration = Duration::from_secs(30);
        {
            let cache = self.model_catalog.lock().await;
            let ttl = if cache.warnings.is_empty() {
                CACHE_TTL
            } else {
                // Retry a temporarily offline provider sooner than a healthy
                // catalog, without hammering it on every picker open.
                FAILED_CACHE_TTL
            };
            if cache
                .fetched_at
                .is_some_and(|fetched| fetched.elapsed() < ttl)
            {
                return (cache.models.clone(), cache.warnings.clone());
            }
        }

        let config = self.config.load_full();
        let configured = config.models();
        let (fetched, warnings) = crate::model_catalog::fetch_all(&config).await;
        let models = crate::model_catalog::merge(configured, fetched);
        let mut cache = self.model_catalog.lock().await;
        cache.fetched_at = Some(Instant::now());
        cache.models = models.clone();
        cache.warnings = warnings.clone();
        (models, warnings)
    }

    /// Re-read the config layers and rebuild the provider. Existing sessions
    /// keep their model and permission; new tool/context/MCP settings apply to
    /// the next turn. A bad file keeps the previous config in place.
    async fn reload_config(&self) -> Result<Vec<String>> {
        let next = self
            .config
            .load()
            .reload()
            .context("cannot reload config")?;
        let provider = Provider::new(&next).context("cannot rebuild provider")?;
        let paths = next
            .watch_paths
            .iter()
            .map(|path| path.display().to_string())
            .collect();
        let subagent_concurrency = next.tools.max_parallel_subagents;
        self.blobs.set_config(next.images.clone());
        self.provider.store(Arc::new(provider));
        self.config.store(Arc::new(next));
        self.subagent_concurrency
            .store(subagent_concurrency, Ordering::Relaxed);
        *self.model_catalog.lock().await = ModelCatalogCache::default();
        self.mcp.lock().await.clear();
        self.lsp.lock().await.clear();
        Ok(paths)
    }

    fn require_session(&self, id: &str) -> Result<Session> {
        let mut session = self
            .store
            .get_session(id)?
            .ok_or_else(|| rpc("not_found", format!("unknown session {id}")))?;
        // Temporary escalations live only in memory; present them as the
        // session's current level so every RPC sees the effective value.
        if let Some(level) = self
            .temporary_permissions
            .lock()
            .ok()
            .and_then(|overrides| overrides.get(id).copied())
        {
            session.permission = level;
        }
        Ok(session)
    }

    /// Overlay in-memory permission overrides on a batch of rows.
    fn effective_permissions(&self, sessions: &mut [Session]) {
        let Ok(overrides) = self.temporary_permissions.lock() else {
            return;
        };
        for session in sessions {
            if let Some(level) = overrides.get(&session.id) {
                session.permission = *level;
            }
        }
    }

    async fn submit_setup(
        &self,
        session_id: SessionId,
        text: String,
        images: Vec<eix_core::ImageAttachment>,
    ) -> Result<(Session, String)> {
        let session = self.require_session(&session_id)?;
        if session.status.is_busy() {
            return Err(rpc(
                "busy",
                format!("session {} is {}", session.id, session.status.as_str()),
            ));
        }
        // A skill's tool restrictions last for one user turn.
        self.skill_tools.lock().await.remove(&session_id);

        let event = self.store.append_event(
            &session.id,
            EventKind::UserMessage,
            payload(&UserMessagePayload { text, images }),
        )?;
        self.emit_event(&event);

        let session = self.transition(&session.id, SessionStatus::Running)?;
        Ok((session, Uuid::new_v4().to_string()))
    }

    fn transition(&self, id: &str, to: SessionStatus) -> Result<Session> {
        let before = self.require_session(id)?;
        if before.status == to {
            return Ok(before);
        }
        self.store
            .set_status(id, to)?
            .ok_or_else(|| rpc("not_found", format!("unknown session {id}")))?;
        let event = self.store.append_event(
            id,
            EventKind::StatusChanged,
            payload(&StatusChangedPayload {
                from: before.status,
                to,
            }),
        )?;
        self.emit_event(&event);
        let session = self.require_session(id)?;
        self.emit_session(&session);
        if to.needs_attention() {
            self.emit_value(
                "session.attention",
                json!({
                    "session_id": id,
                    "status": to.as_str(),
                }),
            );
            self.notify_attention(&session, to);
        }
        Ok(session)
    }

    /// Run the configured notification command for a session that just started
    /// needing attention. Spawned on its own thread so a slow or hanging
    /// command never blocks the event loop. Failures are logged, not fatal.
    fn notify_attention(&self, session: &Session, status: SessionStatus) {
        let config = self.config.load();
        let Some(command) = config
            .notifications
            .command
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_string)
        else {
            return;
        };
        let cooldown = Duration::from_secs(self.config.load().notifications.cooldown_secs);
        let now = Instant::now();
        {
            let mut notified = self
                .notified
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if notified
                .get(&session.id)
                .is_some_and(|last| now.duration_since(*last) < cooldown)
            {
                return;
            }
            notified.insert(session.id.clone(), now);
        }

        let id = session.id.clone();
        let name = session.name.clone();
        let cwd = session.cwd.clone();
        let status = status.as_str().to_string();
        std::thread::spawn(move || {
            let result = std::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .env("EIX_SESSION_ID", &id)
                .env("EIX_SESSION_NAME", &name)
                .env("EIX_SESSION_CWD", &cwd)
                .env("EIX_STATUS", &status)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .and_then(|mut child| child.wait());
            if let Err(err) = result {
                eprintln!(
                    "eixd: notification command failed: {}",
                    redact(&err.to_string())
                );
            }
        });
    }

    async fn run_turn(
        self: Arc<Self>,
        session_id: SessionId,
        turn_id: String,
        cancel: Arc<AtomicBool>,
    ) {
        // A read-only session is never shown mutating tools, even if the model
        // would otherwise call one; `run_tool_call` enforces it again.
        let read_only = self
            .require_session(&session_id)
            .map(|session| self.session_permission(&session) == PermissionLevel::ReadOnly)
            .unwrap_or(true);
        let sub_agent = self
            .require_session(&session_id)
            .map(|session| session.origin == SessionOrigin::Agent)
            .unwrap_or(true);
        let mut definitions = if read_only || sub_agent {
            tools::definitions()
                .into_iter()
                .filter(|tool| !tools::hard_mutating(&tool.function.name))
                .collect::<Vec<_>>()
        } else {
            tools::definitions()
        };
        // MCP tools always need approval, so a read-only session does not see
        // them. Starting the servers lazily keeps startup cheap. Resource and
        // prompt helpers are also hidden unless at least one server is enabled.
        let mcp_configured = self
            .config
            .load()
            .mcp
            .servers
            .values()
            .any(|server| server.enabled);
        if !mcp_configured || read_only || sub_agent {
            definitions.retain(|tool| !is_mcp_meta(&tool.function.name));
        }
        if !read_only && !sub_agent && mcp_configured {
            let config = self.config.load_full();
            let mcp_tools = {
                let mut mcp = self.mcp.lock().await;
                mcp.ensure_loaded(&config).await;
                mcp.refresh(&config).await;
                mcp.tool_definitions()
            };
            // Tool schemas are paid for on every request. Built-ins always fit;
            // cap dynamic MCP schemas so a large server cannot consume the
            // whole context before the model sees the user's message.
            let cap = config.context.max_tool_schema_tokens;
            let (kept, omitted) =
                cap_tool_schemas(std::mem::take(&mut definitions), mcp_tools, cap);
            definitions = kept;
            if omitted > 0 {
                eprintln!(
                    "eixd: omitted {omitted} MCP tool(s) to keep tool schemas under \
                     {cap} tokens; narrow mcp.servers.*.allowed_tools to expose them"
                );
            }
        }
        if let Err(err) = self
            .run_turn_inner(&session_id, &turn_id, &cancel, &definitions)
            .await
        {
            eprintln!(
                "eixd: turn {turn_id} failed: {}",
                redact(&format!("{err:#}"))
            );
            let message = format!("{err:#}");
            self.fail_turn(&session_id, &turn_id, &message);
        }
        self.approvals.lock().await.remove(&session_id);
        self.turns.lock().await.remove(&session_id);
        // Naming is deliberately off the critical path: the turn has already
        // completed, and a provider hiccup falls back to a local title.
        self.maybe_auto_name_session(&session_id).await;
    }

    async fn run_turn_inner(
        self: &Arc<Self>,
        session_id: &str,
        turn_id: &str,
        cancel: &Arc<AtomicBool>,
        allowed_tools: &[ToolDefinition],
    ) -> Result<()> {
        let session = self.require_session(session_id)?;
        let config = self.config.load_full();
        let model = session
            .model
            .clone()
            .unwrap_or_else(|| config.default_model().to_string());
        let turn_started = Instant::now();
        let static_context = StaticContext::load(&config, &session.cwd).await;
        let mut events = self.store.events_async(session_id).await?;
        let mut last_seq = events.last().map(|event| event.seq).unwrap_or(0);
        let ledger = self
            .build_ledger_from_events(session_id, allowed_tools, &events, &static_context)
            .await?;
        let keep_turns = config.context.auto_compact_keep_turns;
        let (ledger, _) = if config.context.auto_compact && session.origin != SessionOrigin::Agent {
            self.auto_compact_if_needed(
                session_id,
                ledger,
                false,
                keep_turns,
                allowed_tools,
                &mut events,
                &mut last_seq,
                &static_context,
            )
            .await
        } else {
            (ledger, false)
        };
        let mut messages = ledger.messages();
        self.emit_context(session_id, &ledger);
        let tools = allowed_tools;
        let max_tool_rounds = config.tools.max_tool_rounds.max(1);
        let route = config.route(&model);
        let (max_retries, retry_base, retry_max) = (
            config.provider_runtime.retry_attempts,
            Duration::from_millis(config.provider_runtime.retry_base_ms.max(1)),
            Duration::from_secs(config.provider_runtime.retry_max_secs.max(1)),
        );
        // Bound expensive summaries and context-window recoveries so a broken
        // provider/model pair cannot loop forever inside one user turn.
        let mut auto_compactions = 0u32;
        let mut context_recoveries = 0u32;

        for round in 0..=max_tool_rounds {
            if cancel.load(Ordering::SeqCst) {
                self.finish_cancelled(session_id, turn_id, "", "", &model)?;
                return Ok(());
            }

            // Rebuild from the event log after every tool batch. This applies
            // budget trimming to the newly added outputs, picks up todo/memory
            // updates, and lets auto-compaction run mid-turn instead of only
            // before the first request.
            if round > 0 {
                self.refresh_turn_events(session_id, &mut events, &mut last_seq)
                    .await?;
                let refreshed = self
                    .build_ledger_from_events(session_id, allowed_tools, &events, &static_context)
                    .await?;
                let auto_compact = self.config.load().context.auto_compact
                    && session.origin != SessionOrigin::Agent;
                let keep = self.config.load().context.auto_compact_keep_turns;
                let (refreshed, compacted) = if auto_compact && auto_compactions < 2 {
                    self.auto_compact_if_needed(
                        session_id,
                        refreshed,
                        false,
                        keep,
                        allowed_tools,
                        &mut events,
                        &mut last_seq,
                        &static_context,
                    )
                    .await
                } else {
                    (refreshed, false)
                };
                if compacted {
                    auto_compactions += 1;
                }
                messages = refreshed.messages();
                self.emit_context(session_id, &refreshed);
            }

            // Approaching the budget, tell the model to stop exploring and
            // answer. The final round omits tool definitions entirely, so the
            // model cannot spend the whole budget and then fail without a
            // synthesis.
            if let Some(warning) = tool_round_warning(round, max_tool_rounds) {
                messages.push(ChatMessage::system(warning));
            }
            let request_tools = tools_for_round(round, max_tool_rounds, tools);

            let provider = self.provider.load_full();
            let mut attempt = 0u32;
            let completion = loop {
                let bus = self.bus.clone();
                let sid = session_id.to_string();
                let tid = turn_id.to_string();
                let mut streamed = String::new();
                let mut streamed_reasoning = String::new();
                let estimated_prompt = estimate_request_tokens(&messages, request_tools);
                let outcome = provider
                    .stream_chat(&messages, &route, request_tools, cancel.as_ref(), |delta| {
                        match delta {
                            crate::provider::StreamDelta::Content(text) => {
                                streamed.push_str(text);
                                let _ = bus.send(Notification::new(
                                    "message.delta",
                                    json!({
                                        "session_id": sid,
                                        "turn_id": tid,
                                        "delta": text,
                                    }),
                                ));
                            }
                            crate::provider::StreamDelta::Reasoning(text) => {
                                streamed_reasoning.push_str(text);
                                let _ = bus.send(Notification::new(
                                    "reasoning.delta",
                                    json!({
                                        "session_id": sid,
                                        "turn_id": tid,
                                        "delta": text,
                                    }),
                                ));
                            }
                        }
                    })
                    .await;

                if cancel.load(Ordering::SeqCst) {
                    self.finish_cancelled(
                        session_id,
                        turn_id,
                        &streamed,
                        &streamed_reasoning,
                        &model,
                    )?;
                    return Ok(());
                }

                if let Ok(completion) = &outcome {
                    self.observe_prompt_usage(&model, estimated_prompt, completion.usage.as_ref());
                }

                let (reason, delay, kind) = match outcome {
                    Ok(completion)
                        if completion.tool_calls.is_empty()
                            && completion.text.trim().is_empty()
                            && completion.reasoning.trim().is_empty()
                            && attempt < max_retries =>
                    {
                        (
                            "model returned an empty response".to_string(),
                            retry_delay_for_class(
                                retry_base,
                                retry_max,
                                attempt + 1,
                                None,
                                RetryClass::Transient,
                            ),
                            "empty_response",
                        )
                    }
                    Ok(completion) => break completion,
                    Err(ProviderError::Cancelled) => {
                        self.finish_cancelled(
                            session_id,
                            turn_id,
                            &streamed,
                            &streamed_reasoning,
                            &model,
                        )?;
                        return Ok(());
                    }
                    Err(err)
                        if err.retry_class() == RetryClass::ContextLength
                            && context_recoveries < 2 =>
                    {
                        context_recoveries += 1;
                        self.refresh_turn_events(session_id, &mut events, &mut last_seq)
                            .await?;
                        let ledger = self
                            .build_ledger_from_events(
                                session_id,
                                allowed_tools,
                                &events,
                                &static_context,
                            )
                            .await?;
                        // The first recovery keeps the configured recent turns;
                        // the second is deliberately aggressive.
                        let keep = if context_recoveries == 1 {
                            self.config.load().context.auto_compact_keep_turns
                        } else {
                            1
                        };
                        let (ledger, compacted) = self
                            .auto_compact_if_needed(
                                session_id,
                                ledger,
                                true,
                                keep,
                                allowed_tools,
                                &mut events,
                                &mut last_seq,
                                &static_context,
                            )
                            .await;
                        if !compacted {
                            return Err(anyhow::anyhow!("{err}"));
                        }
                        messages = ledger.messages();
                        self.emit_context(session_id, &ledger);
                        if !streamed.is_empty() || !streamed_reasoning.is_empty() {
                            self.emit_value(
                                "message.reset",
                                json!({
                                    "session_id": session_id,
                                    "turn_id": turn_id,
                                }),
                            );
                        }
                        eprintln!(
                            "eixd: turn {turn_id} context window exceeded; compacted and retrying"
                        );
                        continue;
                    }
                    Err(err) if err.retry_class().is_retryable() && attempt < max_retries => {
                        let class = err.retry_class();
                        let delay = retry_delay_for_class(
                            retry_base,
                            retry_max,
                            attempt + 1,
                            err.retry_after(),
                            class,
                        );
                        (err.to_string(), delay, class.label())
                    }
                    Err(err) => {
                        if attempt > 0 {
                            return Err(anyhow::anyhow!("{err} (after {} attempts)", attempt + 1));
                        }
                        return Err(anyhow::anyhow!("{err}"));
                    }
                };

                attempt += 1;
                if !streamed.is_empty() || !streamed_reasoning.is_empty() {
                    // Deltas from the failed attempt are already in the TUI;
                    // clear them before the new attempt streams its own text.
                    self.emit_value(
                        "message.reset",
                        json!({
                            "session_id": session_id,
                            "turn_id": turn_id,
                        }),
                    );
                }
                let reason = truncate_ui(&redact(&reason), 240);
                eprintln!(
                    "eixd: turn {turn_id} retry {attempt}/{max_retries} ({kind}) in {}ms: {reason}",
                    delay.as_millis()
                );
                self.emit_value(
                    "turn.retrying",
                    json!({
                        "session_id": session_id,
                        "turn_id": turn_id,
                        "attempt": attempt + 1,
                        "max_attempts": max_retries.saturating_add(1),
                        "delay_ms": delay.as_millis() as u64,
                        "kind": kind,
                        "reason": reason,
                    }),
                );
                if sleep_cancellable(delay, cancel).await {
                    self.finish_cancelled(session_id, turn_id, "", "", &model)?;
                    return Ok(());
                }
            };

            if completion.tool_calls.is_empty() {
                if completion.text.trim().is_empty() && completion.reasoning.trim().is_empty() {
                    return Err(anyhow::anyhow!(
                        "model returned an empty response after {} attempt(s) (finish_reason: {:?})",
                        attempt + 1,
                        completion.finish_reason
                    ));
                }
                let event = self.store.append_event(
                    session_id,
                    EventKind::AssistantMessage,
                    payload(&eix_core::AssistantMessagePayload {
                        text: completion.text,
                        model: completion.model,
                        reasoning: (!completion.reasoning.is_empty())
                            .then_some(completion.reasoning),
                        cancelled: false,
                        usage: completion.usage.clone(),
                        tool_calls: Vec::new(),
                    }),
                )?;
                self.emit_event(&event);
                self.transition(session_id, SessionStatus::Idle)?;
                let duration_ms = turn_started.elapsed().as_millis() as u64;
                let cost = completion
                    .usage
                    .as_ref()
                    .and_then(|usage| route.cost(usage.prompt_tokens, usage.completion_tokens));
                self.emit_value(
                    "turn.completed",
                    json!({
                        "session_id": session_id,
                        "turn_id": turn_id,
                        "cancelled": false,
                        "error": Value::Null,
                        "usage": completion.usage,
                        "cost": cost,
                        "duration_ms": duration_ms,
                    }),
                );
                return Ok(());
            }

            // The assistant asked for tools; record the message so the context
            // stays honest, then run each call and feed the results back.
            let event = self.store.append_event(
                session_id,
                EventKind::AssistantMessage,
                payload(&eix_core::AssistantMessagePayload {
                    text: completion.text.clone(),
                    model: completion.model.clone(),
                    reasoning: (!completion.reasoning.is_empty())
                        .then_some(completion.reasoning.clone()),
                    cancelled: false,
                    usage: completion.usage.clone(),
                    tool_calls: completion.tool_calls.clone(),
                }),
            )?;
            self.emit_event(&event);
            messages.push(ChatMessage::assistant_with_tools(
                completion.text,
                completion.tool_calls.clone(),
            ));

            self.run_tool_batch(
                session_id,
                turn_id,
                &completion.tool_calls,
                &mut messages,
                cancel,
            )
            .await?;
            if cancel.load(Ordering::SeqCst) {
                self.finish_cancelled(session_id, turn_id, "", "", &model)?;
                return Ok(());
            }

            if round == max_tool_rounds {
                bail!(
                    "tool loop exceeded {} rounds without a final answer; raise [tools] max_tool_rounds if this task legitimately needs more",
                    max_tool_rounds
                );
            }
        }
        Ok(())
    }

    /// Run one assistant tool batch. Regular tools run in call order; every
    /// `spawn_agent` call runs concurrently and its result is appended in call
    /// order, so several sub-agents can work at once without racing files
    /// (they are read-only).
    async fn run_tool_batch(
        self: &Arc<Self>,
        session_id: &str,
        turn_id: &str,
        calls: &[ToolCall],
        messages: &mut Vec<ChatMessage>,
        cancel: &Arc<AtomicBool>,
    ) -> Result<()> {
        // A batch made entirely of read-only tools can run concurrently. This
        // is the common "inspect several files/directories" pattern; keeping
        // the results in call order preserves the provider contract.
        if !calls.is_empty()
            && calls
                .iter()
                .all(|call| tools::parallel_safe(&call.function.name))
        {
            // Bound fan-out: a provider can ask for dozens of reads in one
            // turn, and each one may open files or hit the network.
            const READONLY_CONCURRENCY: usize = 8;
            let limit = Arc::new(Semaphore::new(READONLY_CONCURRENCY));
            let results = futures_util::future::join_all(calls.iter().map(|call| {
                let daemon = Arc::clone(self);
                let limit = Arc::clone(&limit);
                async move {
                    let _permit = limit.acquire_owned().await.expect("semaphore open");
                    daemon
                        .run_readonly_call(session_id, turn_id, call, cancel.as_ref())
                        .await
                }
            }));
            let results = results.await;
            for result in results {
                messages.push(result?);
            }
            return Ok(());
        }

        // Run writes/commands first, then sub-agents. A child is read-only but
        // shares the workspace; starting it before the parent's writes in the
        // same batch would let it observe a half-applied change.
        let mut agent_calls = Vec::new();
        for call in calls {
            if call.function.name == "spawn_agent" {
                agent_calls.push(call.clone());
            } else {
                self.run_tool_call(session_id, turn_id, call, messages, cancel.as_ref())
                    .await?;
            }
            if cancel.load(Ordering::SeqCst) {
                break;
            }
        }

        let agent_concurrency = self.subagent_concurrency.load(Ordering::Relaxed).max(1);
        let agent_limit = Arc::new(Semaphore::new(agent_concurrency));
        let mut agents: Vec<(String, tokio::task::JoinHandle<Result<String>>)> = Vec::new();
        if !cancel.load(Ordering::SeqCst) {
            for call in agent_calls {
                let daemon = Arc::clone(self);
                let call_id = call.id.clone();
                let sid = session_id.to_string();
                let tid = turn_id.to_string();
                let limit = Arc::clone(&agent_limit);
                let parent_cancel = Arc::clone(cancel);
                let handle = tokio::spawn(async move {
                    let _permit = limit.acquire_owned().await.expect("semaphore open");
                    daemon.spawn_agent(&sid, &tid, &call, parent_cancel).await
                });
                agents.push((call_id, handle));
            }
        }
        for (call_id, handle) in agents {
            let output = match handle.await {
                Ok(Ok(output)) => output,
                Ok(Err(err)) => format!("spawn_agent failed: {err:#}"),
                Err(err) => format!("spawn_agent task failed: {err}"),
            };
            // `spawn_agent` records its own tool events; only the message fed
            // back to the provider is added here.
            messages.push(ChatMessage::tool(call_id, output));
        }
        Ok(())
    }

    /// Execute one read-only call without borrowing `messages`, so a whole
    /// batch can be joined concurrently. Event recording and notifications are
    /// the same as the sequential path.
    async fn run_readonly_call(
        &self,
        session_id: &str,
        turn_id: &str,
        call: &ToolCall,
        cancel: &AtomicBool,
    ) -> Result<ChatMessage> {
        let tool = call.function.name.as_str();
        let args: Value = match serde_json::from_str(&call.function.arguments) {
            Ok(args) => args,
            Err(err) => {
                let output = format!("invalid JSON arguments: {err}");
                self.record_tool_result(session_id, call, false, false, &output, 0)?;
                return Ok(ChatMessage::tool(call.id.clone(), output));
            }
        };
        let event = self.store.append_event(
            session_id,
            EventKind::ToolCall,
            payload(&ToolCallPayload {
                call_id: call.id.clone(),
                tool: tool.to_string(),
                arguments: args.clone(),
            }),
        )?;
        self.emit_event(&event);

        if cancel.load(Ordering::SeqCst) {
            let output = "cancelled".to_string();
            self.record_tool_result(session_id, call, false, false, &output, 0)?;
            return Ok(ChatMessage::tool(call.id.clone(), output));
        }
        if !tools::is_known(tool) {
            let output = format!("unknown tool {tool:?}");
            self.record_tool_result(session_id, call, false, false, &output, 0)?;
            return Ok(ChatMessage::tool(call.id.clone(), output));
        }
        if !self.skill_allows(session_id, tool).await {
            let output = format!("the active skill does not allow the {tool} tool");
            self.record_tool_result(session_id, call, false, true, &output, 0)?;
            return Ok(ChatMessage::tool(call.id.clone(), output));
        }

        let session = self.require_session(session_id)?;
        self.emit_value(
            "tool.started",
            json!({
                "session_id": session_id,
                "turn_id": turn_id,
                "call_id": call.id,
                "tool": tool,
            }),
        );
        let outcome = match self
            .read_with_staging(session_id, tool, &args, &session.cwd)
            .await
        {
            Some(outcome) => outcome,
            None => {
                tools::execute_in(
                    tool,
                    &args,
                    &session.cwd,
                    &self.config.load(),
                    Some(cancel),
                    session.worktree.as_deref(),
                )
                .await
            }
        };
        let (ok, output, duration_ms) = (outcome.ok, outcome.output, outcome.duration_ms);
        self.record_tool_result(session_id, call, ok, false, &output, duration_ms)?;
        self.emit_value(
            "tool.completed",
            json!({
                "session_id": session_id,
                "turn_id": turn_id,
                "call_id": call.id,
                "tool": tool,
                "ok": ok,
                "denied": false,
                "duration_ms": duration_ms,
            }),
        );
        Ok(ChatMessage::tool(call.id.clone(), output))
    }

    /// Start one sub-agent and return its final answer as tool output.
    ///
    /// The return type is boxed on purpose: the child turn eventually calls
    /// back into the tool loop, and an explicit `dyn Future + Send` boundary
    /// lets the compiler resolve the recursive auto-trait instead of giving up
    /// with "future is not Send".
    fn spawn_agent<'a>(
        self: &'a Arc<Self>,
        parent_id: &'a str,
        parent_turn: &'a str,
        call: &'a ToolCall,
        parent_cancel: Arc<AtomicBool>,
    ) -> futures_util::future::BoxFuture<'a, Result<String>> {
        Box::pin(async move {
            self.spawn_agent_outer(parent_id, parent_turn, call, parent_cancel)
                .await
        })
    }

    async fn spawn_agent_outer(
        self: &Arc<Self>,
        parent_id: &str,
        parent_turn: &str,
        call: &ToolCall,
        parent_cancel: Arc<AtomicBool>,
    ) -> Result<String> {
        let args: Value = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Null);
        let event = self.store.append_event(
            parent_id,
            EventKind::ToolCall,
            payload(&ToolCallPayload {
                call_id: call.id.clone(),
                tool: "spawn_agent".to_string(),
                arguments: args.clone(),
            }),
        )?;
        self.emit_event(&event);

        let parent = self.require_session(parent_id)?;
        let result = self
            .spawn_agent_inner(&parent, parent_turn, &call.id, &args, parent_cancel)
            .await;
        let (ok, output) = match result {
            Ok(output) => (true, output),
            Err(err) => (false, format!("{err:#}")),
        };
        self.record_tool_result(parent_id, call, ok, false, &output, 0)?;
        self.emit_value(
            "tool.completed",
            json!({
                "session_id": parent_id,
                "turn_id": parent_turn,
                "call_id": call.id,
                "tool": "spawn_agent",
                "ok": ok,
                "denied": false,
                "duration_ms": 0,
            }),
        );
        // A failed sub-agent is reported to the model, not turned into a
        // session error: the parent can retry or answer without it.
        Ok(output)
    }

    async fn spawn_agent_inner(
        self: &Arc<Self>,
        parent: &Session,
        parent_turn: &str,
        parent_call_id: &str,
        args: &Value,
        parent_cancel: Arc<AtomicBool>,
    ) -> Result<String> {
        if parent.origin == SessionOrigin::Agent {
            bail!("sub-agents cannot spawn further sub-agents (depth limit is 1)");
        }
        let prompt = args
            .get("prompt")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
            .ok_or_else(|| anyhow::anyhow!("spawn_agent needs a non-empty `prompt`"))?;
        let label = args
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|label| !label.is_empty());
        let requested_agent = args
            .get("agent")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|agent| !agent.is_empty());
        let agent = match requested_agent {
            Some(name) => {
                let config = self.config.load();
                let Some(spec) = eix_core::find_agent(&parent.cwd, name, &config.compations) else {
                    bail!("unknown sub-agent {name:?}");
                };
                if !spec.mode.can_subagent() {
                    bail!("agent {name:?} cannot be used as a sub-agent");
                }
                Some(spec)
            }
            None => None,
        };

        let name = self.agent_name(
            &parent.name,
            label.or_else(|| agent.as_ref().map(|agent| agent.name.as_str())),
        );
        let mut child = Session::new(Uuid::new_v4().to_string(), name.clone(), parent.cwd.clone());
        child.model = agent
            .as_ref()
            .and_then(|agent| agent.model.clone())
            .or_else(|| parent.model.clone());
        child.agent = agent.as_ref().map(|agent| agent.name.clone());
        child.parent_id = Some(parent.id.clone());
        child.origin = SessionOrigin::Agent;
        self.store.create_session(&child)?;
        self.emit_session(&child);

        let child_cancel = Arc::new(AtomicBool::new(false));
        self.turns
            .lock()
            .await
            .insert(child.id.clone(), Arc::clone(&child_cancel));
        self.emit_value(
            "agent.spawned",
            json!({
                "session_id": parent.id,
                "agent_id": child.id,
                "call_id": parent_call_id,
                "name": child.name,
                "parent_turn_id": parent_turn,
            }),
        );

        // Cancelling the parent must also stop the child. The watcher keeps the
        // child's own `turns` entry authoritative, so `prompt.cancel` on the
        // child and parent cancellation both land on the same flag.
        let watcher = {
            let child_cancel = Arc::clone(&child_cancel);
            let parent_cancel = Arc::clone(&parent_cancel);
            tokio::spawn(async move {
                wait_for_cancel(&parent_cancel).await;
                child_cancel.store(true, Ordering::SeqCst);
            })
        };
        let run = self
            .run_child_turn(&child, prompt, Arc::clone(&child_cancel))
            .await;
        watcher.abort();
        self.turns.lock().await.remove(&child.id);

        match run {
            Ok(text) => {
                let trimmed = text.trim();
                let body = if trimmed.is_empty() {
                    "(sub-agent returned no text)".to_string()
                } else {
                    cap_text(trimmed, self.config.load().tools.max_output_bytes)
                };
                self.emit_value(
                    "agent.completed",
                    json!({
                        "session_id": parent.id,
                        "agent_id": child.id,
                        "call_id": parent_call_id,
                        "name": child.name,
                        "ok": true,
                        "result": body.clone(),
                    }),
                );
                Ok(format!(
                    "sub-agent {} ({}) finished:\n{}",
                    child.name,
                    short_id(&child.id),
                    body
                ))
            }
            Err(err) => {
                let message = format!("{err:#}");
                self.emit_value(
                    "agent.completed",
                    json!({
                        "session_id": parent.id,
                        "agent_id": child.id,
                        "call_id": parent_call_id,
                        "name": child.name,
                        "ok": false,
                        "error": message,
                    }),
                );
                Err(err)
            }
        }
    }

    /// Run one bounded, read-only turn for a sub-agent and return its answer.
    async fn run_child_turn(
        self: &Arc<Self>,
        child: &Session,
        prompt: &str,
        cancel: Arc<AtomicBool>,
    ) -> Result<String> {
        let event = self.store.append_event(
            &child.id,
            EventKind::UserMessage,
            payload(&UserMessagePayload {
                text: prompt.to_string(),
                images: Vec::new(),
            }),
        )?;
        self.emit_event(&event);
        self.transition(&child.id, SessionStatus::Running)?;

        let turn_id = Uuid::new_v4().to_string();
        let readonly = tools::readonly_definitions();
        self.emit_value(
            "turn.started",
            json!({
                "session_id": child.id,
                "turn_id": turn_id,
                "model": child
                    .model
                    .clone()
                    .unwrap_or_else(|| self.config.load().default_model().to_string()),
            }),
        );
        if let Err(err) = self
            .run_turn_inner(&child.id, &turn_id, &cancel, &readonly)
            .await
        {
            self.fail_turn(&child.id, &turn_id, &format!("{err:#}"));
            return Err(err);
        }

        let events = self.store.events_async(&child.id).await?;
        for event in events.iter().rev() {
            if event.kind != EventKind::AssistantMessage {
                continue;
            }
            if let Ok(assistant) =
                serde_json::from_value::<eix_core::AssistantMessagePayload>(event.payload.clone())
            {
                return Ok(assistant.text);
            }
        }
        Ok(String::new())
    }

    /// `<parent>/<label>-N`, never colliding with an existing session name.
    fn agent_name(&self, parent_name: &str, label: Option<&str>) -> String {
        let label = label.unwrap_or("agent");
        let taken: HashSet<String> = self
            .store
            .list_sessions()
            .map(|sessions| sessions.into_iter().map(|session| session.name).collect())
            .unwrap_or_default();
        let mut n = 1;
        loop {
            let candidate = format!("{parent_name}/{label}-{n}");
            if !taken.contains(&candidate) {
                return candidate;
            }
            n += 1;
        }
    }

    /// Record, gate, execute and feed back a single tool call.
    async fn run_tool_call(
        &self,
        session_id: &str,
        turn_id: &str,
        call: &ToolCall,
        messages: &mut Vec<ChatMessage>,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let tool = call.function.name.as_str();
        let args: Value = match serde_json::from_str(&call.function.arguments) {
            Ok(args) => args,
            Err(err) => {
                let output = format!("invalid JSON arguments: {err}");
                self.record_tool_result(session_id, call, false, false, &output, 0)?;
                messages.push(ChatMessage::tool(call.id.clone(), output));
                return Ok(());
            }
        };

        let event = self.store.append_event(
            session_id,
            EventKind::ToolCall,
            payload(&ToolCallPayload {
                call_id: call.id.clone(),
                tool: tool.to_string(),
                arguments: args.clone(),
            }),
        )?;
        self.emit_event(&event);

        let mcp_known = tool.starts_with(crate::mcp::PREFIX) && self.mcp.lock().await.knows(tool);
        if !tools::is_known(tool) && !mcp_known {
            let output = format!("unknown tool {tool:?}");
            self.record_tool_result(session_id, call, false, false, &output, 0)?;
            messages.push(ChatMessage::tool(call.id.clone(), output));
            return Ok(());
        }
        // A loaded skill may restrict the tools available for the rest of the
        // turn. `use_skill` itself is always allowed so another skill can be
        // loaded (their allow-lists are unioned).
        if tool != "use_skill" && !self.skill_allows(session_id, tool).await {
            let output = format!("the active skill does not allow the {tool} tool");
            self.record_tool_result(session_id, call, false, true, &output, 0)?;
            messages.push(ChatMessage::tool(call.id.clone(), output));
            return Ok(());
        }

        // Sub-agents inherit the parent's context, not its write access. Even
        // if the model ignores the read-only tool list, mutating tools are
        // refused here so a child can never change the shared workspace.
        let session = self.require_session(session_id)?;
        let permission = self.session_permission(&session);
        let session_rules = self
            .always
            .lock()
            .await
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        let decision = {
            let config = self.config.load();
            permission_decide(
                tool,
                &args,
                &session.cwd,
                &config,
                permission,
                &session_rules,
                mcp_known,
            )
        };
        self.record_permission(session_id, &call.id, tool, &decision)?;
        // A deny is global and absolute: it must run before the special-case
        // handlers below (todo/memory/ask_user/lsp), which otherwise bypass
        // the approval gate.
        if decision.action == PermissionAction::Deny {
            let output = decision.reason.clone();
            self.record_tool_result(session_id, call, false, true, &output, 0)?;
            self.emit_value(
                "tool.completed",
                json!({
                    "session_id": session_id,
                    "turn_id": turn_id,
                    "call_id": call.id,
                    "tool": tool,
                    "ok": false,
                    "denied": true,
                    "duration_ms": 0,
                }),
            );
            messages.push(ChatMessage::tool(call.id.clone(), output));
            return Ok(());
        }
        if session.origin == SessionOrigin::Agent && decision.action != PermissionAction::Allow {
            let output = if decision.action == PermissionAction::Deny {
                format!("sub-agents are read-only; {}", decision.reason)
            } else {
                "sub-agents are read-only; mutating or interactive tools are not available"
                    .to_string()
            };
            self.record_tool_result(session_id, call, false, true, &output, 0)?;
            messages.push(ChatMessage::tool(call.id.clone(), output));
            return Ok(());
        }

        // Sub-agents have no UI; a question they asked would block the parent
        // turn with nobody to answer it.
        if session.origin == SessionOrigin::Agent && tool == "ask_user" {
            let output = "sub-agents cannot ask the user questions".to_string();
            self.record_tool_result(session_id, call, false, true, &output, 0)?;
            messages.push(ChatMessage::tool(call.id.clone(), output));
            return Ok(());
        }

        // `todo` only changes daemon state (the session task list). It never
        // touches the workspace, so it bypasses the approval card.
        if tool == "todo" {
            let output = match parse_todos(&args) {
                Ok(items) => {
                    let rendered = render_todos(&items);
                    self.todos
                        .lock()
                        .await
                        .insert(session_id.to_string(), items);
                    rendered
                }
                Err(err) => {
                    let output = format!("{err:#}");
                    self.record_tool_result(session_id, call, false, true, &output, 0)?;
                    messages.push(ChatMessage::tool(call.id.clone(), output));
                    return Ok(());
                }
            };
            self.record_tool_result(session_id, call, true, true, &output, 0)?;
            messages.push(ChatMessage::tool(call.id.clone(), output));
            return Ok(());
        }

        // `memory` persists a small note outside the workspace. Like `todo` it
        // changes daemon state rather than files, so it bypasses approval.
        if tool == "memory" {
            let action = args.get("action").and_then(Value::as_str).unwrap_or("add");
            let text = args.get("text").and_then(Value::as_str).unwrap_or("");
            let result = match action {
                "add" if text.trim().is_empty() => {
                    Err(anyhow::anyhow!("memory add needs non-empty `text`"))
                }
                "add" => self.memory.append(&session.cwd, text).await,
                "replace" if text.trim().is_empty() => Err(anyhow::anyhow!(
                    "memory replace needs non-empty `text`; use action=clear to delete"
                )),
                "replace" => self.memory.write(&session.cwd, text).await,
                "clear" => self
                    .memory
                    .clear(&session.cwd)
                    .await
                    .map(|_| "memory cleared".to_string()),
                other => Err(anyhow::anyhow!(
                    "unknown memory action {other:?}; use add, replace or clear"
                )),
            };
            let (ok, output) = match result {
                Ok(text) => (true, text),
                Err(err) => (false, format!("{err:#}")),
            };
            self.record_tool_result(session_id, call, ok, false, &output, 0)?;
            messages.push(ChatMessage::tool(call.id.clone(), output));
            return Ok(());
        }

        // The model can mark its worktree ready without a shell approval. The
        // queue still defers while this turn is running, then rebases/tests.
        if tool == "ready" {
            if session.origin == SessionOrigin::Agent {
                let output = "sub-agents cannot mark a worktree ready".to_string();
                self.record_tool_result(session_id, call, false, true, &output, 0)?;
                messages.push(ChatMessage::tool(call.id.clone(), output));
                return Ok(());
            }
            let output = match self.request_ready_inner(session_id, None, true).await {
                Ok(state) => format!("queued for integration ({})", state.status.as_str()),
                Err(err) => format!("cannot queue for integration: {err:#}"),
            };
            let ok = output.starts_with("queued for integration");
            self.record_tool_result(session_id, call, ok, false, &output, 0)?;
            messages.push(ChatMessage::tool(call.id.clone(), output));
            return Ok(());
        }

        // `ask_user` is interactive rather than mutating: it blocks the turn
        // until the user picks an option (or the turn is cancelled).
        if tool == "ask_user" {
            return self
                .ask_user(session_id, turn_id, call, &args, messages, cancel)
                .await;
        }

        // Language-server diagnostics are read-only and bypass approvals.
        if tool == "lsp_diagnostics" {
            let config = self.config.load();
            let raw = args.get("path").and_then(Value::as_str).unwrap_or_default();
            let output = match tools::resolve_path_in(&session.cwd, raw, &config) {
                Ok(path) if LspSet::handles(&config, &path) => {
                    let mut lsp = self.lsp.lock().await;
                    match lsp.diagnostics(&config, &session.cwd, &path).await {
                        Ok(diagnostics) if diagnostics.is_empty() => {
                            format!("no diagnostics for {}", path.display())
                        }
                        Ok(diagnostics) => diagnostics
                            .iter()
                            .take(50)
                            .map(|diagnostic| diagnostic.label(&path))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        Err(err) => format!("lsp diagnostics failed: {err:#}"),
                    }
                }
                Ok(path) => format!("no language server configured for {}", path.display()),
                Err(err) => format!("{err:#}"),
            };
            self.record_tool_result(session_id, call, true, false, &output, 0)?;
            messages.push(ChatMessage::tool(call.id.clone(), output));
            return Ok(());
        }

        // Low/medium writes under an `ask` decision go into the shadow
        // workspace and return immediately. The user applies or rejects them
        // later. High/critical writes and forced asks block on an approval card
        // so their risk metadata is visible and `Always` can be withheld.
        // Explicit `allow` rules and `approval = "auto"` write directly.
        if decision.action == PermissionAction::Ask
            && !decision.force_ask
            && decision.risk.level <= eix_core::RiskLevel::Medium
            && matches!(tool, "write_file" | "edit")
        {
            return self
                .stage_write(session_id, turn_id, call, &args, messages)
                .await;
        }

        let mut denial: Option<String> = None;
        let mut plan: Option<tools::WritePlan> = None;
        let mut selected_hunks: Option<Vec<usize>> = None;
        if decision.action == PermissionAction::Ask {
            let reply = self
                .wait_for_approval(session_id, turn_id, call, &args, &decision, cancel)
                .await?;
            selected_hunks = reply.hunks;
            plan = reply.plan;
            if reply.decision == ApprovalDecision::Always {
                self.remember_always(session_id, tool, &args, reply.scope)
                    .await;
            }
            if reply.decision == ApprovalDecision::Deny {
                denial = Some(reply.reason.unwrap_or_else(|| "denied by user".to_string()));
            } else if let Some(plan) = &plan {
                // The user reviewed a diff; do not overwrite a file that
                // changed in the meantime.
                if let Err(err) = plan.verify_unchanged().await {
                    denial = Some(format!("{err:#}"));
                }
            }
        }

        let denied = denial.is_some();
        let snapshot_warning = if denied {
            None
        } else {
            self.capture_snapshot(session_id, tool, call, &args, &decision.risk, plan.as_ref())
                .await?
        };
        let (ok, mut output, duration_ms) = match denial {
            Some(reason) => (false, reason, 0),
            None => {
                self.emit_value(
                    "tool.started",
                    json!({
                        "session_id": session_id,
                        "turn_id": turn_id,
                        "call_id": call.id,
                        "tool": tool,
                    }),
                );
                let outcome = match &plan {
                    Some(plan) => tools::apply_plan(plan, selected_hunks.as_deref()).await,
                    None if tool == "start_command" => {
                        self.start_background(session_id, &args).await
                    }
                    None if tool == "process_output" => self.read_process(session_id, &args),
                    None if is_mcp_meta(tool) => self.call_mcp_meta(tool, &args).await,
                    None if mcp_known => self.call_mcp(tool, &args).await,
                    None => {
                        let session = self.require_session(session_id)?;
                        match self
                            .read_with_staging(session_id, tool, &args, &session.cwd)
                            .await
                        {
                            Some(outcome) => outcome,
                            None => {
                                tools::execute_in(
                                    tool,
                                    &args,
                                    &session.cwd,
                                    &self.config.load(),
                                    Some(cancel),
                                    session.worktree.as_deref(),
                                )
                                .await
                            }
                        }
                    }
                };
                (outcome.ok, outcome.output, outcome.duration_ms)
            }
        };
        if ok && tool == "use_skill" {
            self.remember_skill(session_id, &args).await;
        }
        if let Some(warning) = snapshot_warning {
            output.push_str(&format!("\n[snapshot failed: {warning}]"));
        }
        if ok && matches!(tool, "write_file" | "edit" | "patch") {
            let session = self.require_session(session_id)?;
            let config = self.config.load();
            let paths = written_paths(tool, &args, &session.cwd, &config);
            output.push_str(&self.lsp_notes(&config, &session.cwd, &paths).await);
        }
        if ok && matches!(tool, "write_file" | "edit") {
            let reason = decision.reason.clone();
            self.record_auto_patch(session_id, call, &args, &reason)?;
        }

        self.record_tool_result(session_id, call, ok, denied, &output, duration_ms)?;
        self.emit_value(
            "tool.completed",
            json!({
                "session_id": session_id,
                "turn_id": turn_id,
                "call_id": call.id,
                "tool": tool,
                "ok": ok,
                "denied": denied,
                "duration_ms": duration_ms,
            }),
        );
        messages.push(ChatMessage::tool(call.id.clone(), output));
        Ok(())
    }

    /// Stage a `write_file` or `edit` proposal in the session shadow workspace
    /// and return immediately. The turn is not blocked: the user applies or
    /// rejects the patch later, and reads see the staged bytes in the meantime.
    async fn stage_write(
        &self,
        session_id: &str,
        turn_id: &str,
        call: &ToolCall,
        args: &Value,
        messages: &mut Vec<ChatMessage>,
    ) -> Result<()> {
        let tool = call.function.name.as_str();
        let session = self.require_session(session_id)?;
        let cwd = session.cwd.clone();
        let Some(path) = tools::write_target(args, &cwd, self.config.load().as_ref()) else {
            let output = format!("{tool} needs a path inside the workspace");
            self.record_tool_result(session_id, call, false, false, &output, 0)?;
            self.emit_value(
                "tool.completed",
                json!({
                    "session_id": session_id,
                    "turn_id": turn_id,
                    "call_id": call.id,
                    "tool": tool,
                    "ok": false,
                    "denied": false,
                    "duration_ms": 0,
                }),
            );
            messages.push(ChatMessage::tool(call.id.clone(), output));
            return Ok(());
        };
        // Diff against the newest staged contents when the agent writes the
        // same file twice before the user has decided.
        let base = self.staging.latest_content(session_id, &path);
        let plan = match tools::write_plan_with_base(
            tool,
            args,
            &cwd,
            &self.config.load(),
            base.as_deref(),
        )
        .await
        {
            Ok(plan) => plan,
            Err(err) => {
                let output = format!("{err:#}");
                self.record_tool_result(session_id, call, false, false, &output, 0)?;
                self.emit_value(
                    "tool.completed",
                    json!({
                        "session_id": session_id,
                        "turn_id": turn_id,
                        "call_id": call.id,
                        "tool": tool,
                        "ok": false,
                        "denied": false,
                        "duration_ms": 0,
                    }),
                );
                messages.push(ChatMessage::tool(call.id.clone(), output));
                return Ok(());
            }
        };
        let summary = self.tool_summary(tool, args);
        let hunks = plan.hunk_infos();
        let patch = StagedPatch::new(
            session_id,
            &call.id,
            tool,
            plan.path(),
            plan.label(),
            plan.old().to_string(),
            plan.new_content().to_string(),
        );
        self.staging.add(patch.clone())?;
        let event = self.store.append_event(
            session_id,
            EventKind::PatchProposed,
            payload(&PatchProposedPayload {
                patch_id: patch.patch_id.clone(),
                call_id: call.id.clone(),
                tool: tool.to_string(),
                path: patch.label.clone(),
                summary: summary.clone(),
                diff: Some(plan.diff().to_string()),
                hunks: hunks.clone(),
            }),
        )?;
        self.emit_event(&event);
        self.emit_value(
            "patch.proposed",
            json!({
                "session_id": session_id,
                "turn_id": turn_id,
                "patch_id": patch.patch_id,
                "call_id": call.id,
                "tool": tool,
                "path": patch.label,
                "summary": summary,
                "diff": plan.diff(),
                "hunks": hunks,
            }),
        );
        self.announce_patch(&session);

        let output = format!(
            "staged {} as patch {}; the file is not written yet — the user will apply or reject the patch",
            patch.label, patch.patch_id
        );
        self.record_tool_result(session_id, call, true, false, &output, 0)?;
        self.emit_value(
            "tool.completed",
            json!({
                "session_id": session_id,
                "turn_id": turn_id,
                "call_id": call.id,
                "tool": tool,
                "ok": true,
                "denied": false,
                "duration_ms": 0,
            }),
        );
        messages.push(ChatMessage::tool(call.id.clone(), output));
        Ok(())
    }

    /// Aggregate daemon counters for `GET /stats` and the `stats` RPC.
    pub(crate) async fn stats(&self) -> Result<StatsResult> {
        let store = self.store.stats()?;
        let (blob_count, blob_bytes) = self.blobs.stats().await.unwrap_or((0, 0));
        let config = self.config.load();
        Ok(StatsResult {
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_secs: self.started.elapsed().as_secs(),
            sessions: store.sessions,
            archived_sessions: store.archived_sessions,
            running_sessions: store.running_sessions,
            events: store.events,
            db_bytes: store.db_bytes,
            blob_count,
            blob_bytes,
            skills: eix_core::load_skills(&config.load_cwd, &config.compations).len(),
            mcp_servers: config.mcp.servers.len(),
            default_model: config.default_model().to_string(),
        })
    }

    /// Delete image blobs no event references. Referenced images are always
    /// retained so history stays replayable.
    async fn gc_blobs(&self) -> Result<crate::blob::BlobGc> {
        let used = self.store.referenced_blob_ids()?;
        self.blobs.gc(&used, SystemTime::now()).await
    }

    /// Inline blob-backed image bytes before exporting a session.
    async fn hydrate_event_images(&self, event: &mut Event) -> Result<()> {
        if event.kind != EventKind::UserMessage {
            return Ok(());
        }
        let Ok(mut message) = serde_json::from_value::<UserMessagePayload>(event.payload.clone())
        else {
            return Ok(());
        };
        let mut changed = false;
        for image in &mut message.images {
            if !image.data.is_empty() {
                continue;
            }
            let Some(blob) = image.blob.clone() else {
                continue;
            };
            match self.blobs.get(&blob).await {
                Ok(bytes) => {
                    image.data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    image.bytes.get_or_insert(bytes.len() as u64);
                    changed = true;
                }
                Err(err) => {
                    // Portability matters more than a hard failure here: keep
                    // the reference and let the importer report a missing blob.
                    eprintln!("eixd: export: {err:#}");
                }
            }
        }
        if changed {
            event.payload = payload(&message);
        }
        Ok(())
    }

    /// Replace inline image bytes on import with local content-addressed blobs.
    async fn store_event_images(&self, event: &mut Event) -> Result<()> {
        if event.kind != EventKind::UserMessage {
            return Ok(());
        }
        let Ok(mut message) = serde_json::from_value::<UserMessagePayload>(event.payload.clone())
        else {
            return Ok(());
        };
        let mut changed = false;
        for image in &mut message.images {
            if image.data.is_empty() {
                continue;
            }
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&image.data)
                .context("imported image is not valid base64")?;
            image.blob = Some(self.blobs.put(&bytes).await?);
            image.bytes = Some(bytes.len() as u64);
            image.data.clear();
            changed = true;
        }
        if changed {
            event.payload = payload(&message);
        }
        Ok(())
    }

    /// Rebuild the context ledger for a session. Used by reads and the
    /// `context.*` requests, where the exact turn tool list is not available.
    async fn build_ledger(&self, session_id: &str) -> Result<Ledger> {
        let session = self.require_session(session_id)?;
        let tools = self.standard_tool_definitions(&session);
        self.build_ledger_with_tools(session_id, &tools, None).await
    }

    /// The tools a normal turn would see, without starting MCP servers.
    /// Dynamic MCP tool schemas are only known after a turn starts, so ledger
    /// reads outside the turn use the built-in set and MCP meta-tools.
    fn standard_tool_definitions(&self, session: &Session) -> Vec<ToolDefinition> {
        let read_only = self.session_permission(session) == PermissionLevel::ReadOnly;
        let sub_agent = session.origin == SessionOrigin::Agent;
        let mut definitions = if read_only || sub_agent {
            tools::definitions()
                .into_iter()
                .filter(|tool| !tools::hard_mutating(&tool.function.name))
                .collect::<Vec<_>>()
        } else {
            tools::definitions()
        };
        let mcp_configured = self
            .config
            .load()
            .mcp
            .servers
            .values()
            .any(|server| server.enabled);
        if !mcp_configured || read_only || sub_agent {
            definitions.retain(|tool| !is_mcp_meta(&tool.function.name));
        }
        definitions
    }

    /// Rebuild the ledger with the exact tool schemas the request will carry.
    /// When `static_context` is supplied, instructions and the workspace
    /// summary are reused instead of walking the repository again.
    async fn build_ledger_with_tools(
        &self,
        session_id: &str,
        tools: &[ToolDefinition],
        static_context: Option<&StaticContext>,
    ) -> Result<Ledger> {
        let session = self.require_session(session_id)?;
        let events = self.store.events_async(session_id).await?;
        let loaded;
        let static_context = match static_context {
            Some(context) => context,
            None => {
                let config = self.config.load_full();
                loaded = StaticContext::load(&config, &session.cwd).await;
                &loaded
            }
        };
        self.build_ledger_from_events(session_id, tools, &events, static_context)
            .await
    }

    /// Assemble the ledger from an event snapshot. The turn loop keeps this
    /// snapshot and appends only new events after each tool batch.
    async fn build_ledger_from_events(
        &self,
        session_id: &str,
        tools: &[ToolDefinition],
        events: &[Event],
        static_context: &StaticContext,
    ) -> Result<Ledger> {
        let session = self.require_session(session_id)?;
        let config = self.config.load_full();
        let permission = self.session_permission(&session);
        let todos = self
            .todos
            .lock()
            .await
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        let tasks = (!todos.is_empty()).then(|| todo_context(&todos));
        let memory = self.memory.read(&session.cwd).await;
        let model = session
            .model
            .clone()
            .unwrap_or_else(|| config.default_model().to_string());
        let route = config.route(&model);
        let budget = config.context.effective_budget_tokens(route.context_tokens);
        Ok(Ledger::build_with_state_and_static(
            &config,
            system_prompt(&config, &session, permission),
            static_context,
            events,
            Some(&self.blobs),
            memory,
            tasks,
            tools,
            self.token_scale(&model),
            budget,
        )
        .await)
    }

    /// Append events written since the last snapshot. Compaction events and
    /// tool results are picked up here without re-reading the whole log.
    async fn refresh_turn_events(
        &self,
        session_id: &str,
        events: &mut Vec<Event>,
        last_seq: &mut u64,
    ) -> Result<()> {
        let new_events = self.store.events_after_async(session_id, *last_seq).await?;
        if let Some(last) = new_events.last() {
            *last_seq = last.seq;
        }
        events.extend(new_events);
        Ok(())
    }

    fn token_scale(&self, model: &str) -> f64 {
        let scales = match self.token_scales.lock() {
            Ok(scales) => scales,
            Err(poisoned) => poisoned.into_inner(),
        };
        scales.get(model).copied().unwrap_or(1.0)
    }

    /// Feed provider-reported prompt usage back into the estimate. The observed
    /// ratio is clamped and smoothed so one odd provider response cannot make
    /// the next request wildly over- or under-trim.
    fn observe_prompt_usage(&self, model: &str, estimated: usize, usage: Option<&eix_core::Usage>) {
        let Some(actual) = usage
            .map(|usage| usage.prompt_tokens as usize)
            .filter(|tokens| *tokens > 0)
        else {
            return;
        };
        if estimated == 0 {
            return;
        }
        let mut scales = match self.token_scales.lock() {
            Ok(scales) => scales,
            Err(poisoned) => poisoned.into_inner(),
        };
        let scale = scales.entry(model.to_string()).or_insert(1.0);
        *scale = calibrated_scale(*scale, estimated, actual);
    }

    fn emit_context(&self, session_id: &str, ledger: &Ledger) {
        self.emit_value(
            "context.updated",
            json!({
                "session_id": session_id,
                "ledger": ledger.info(),
            }),
        );
    }

    async fn context_compact(
        &self,
        params: ContextCompactParams,
    ) -> Result<eix_core::ContextLedgerResult> {
        let session = self.require_session(&params.session_id)?;
        if session.status.is_busy() {
            return Err(rpc(
                "busy",
                "wait for the turn to finish before compacting the context",
            ));
        }
        let ledger = self.build_ledger(&params.session_id).await?;
        let (from_seq, to_seq) = match (params.from_seq, params.to_seq) {
            (Some(from), Some(to)) => (from, to),
            _ => ledger
                .message_range()
                .ok_or_else(|| rpc("invalid_params", "there is nothing to compact yet"))?,
        };
        if from_seq == 0 || to_seq < from_seq {
            return Err(rpc(
                "invalid_params",
                "context.compact needs 0 < from_seq <= to_seq",
            ));
        }
        if ledger.overlaps_compaction(from_seq, to_seq) {
            return Err(rpc(
                "invalid_params",
                "compaction range overlaps an existing summary",
            ));
        }
        let (messages, tokens) = ledger.range_stats(from_seq, to_seq);
        if messages == 0 {
            return Err(rpc("invalid_params", "no messages in the requested range"));
        }
        let summary = match params
            .summary
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
        {
            Some(text) => text,
            None if params.generate => self.generate_summary(&params.session_id, &ledger).await?,
            None => "(the user compacted these messages; their text remains in the event log, \
                     not in this request)"
                .to_string(),
        };
        let event = self.store.append_event(
            &params.session_id,
            EventKind::ContextCompacted,
            payload(&ContextCompactedPayload {
                from_seq,
                to_seq,
                messages,
                tokens,
                summary,
                rolling: false,
            }),
        )?;
        self.emit_event(&event);
        self.emit_value(
            "context.compacted",
            json!({
                "session_id": params.session_id,
                "from_seq": from_seq,
                "to_seq": to_seq,
                "messages": messages,
                "tokens": tokens,
            }),
        );
        let ledger = self.build_ledger(&params.session_id).await?;
        self.emit_context(&params.session_id, &ledger);
        Ok(ledger.info())
    }

    /// Ask the model to summarize the conversation so far. Used by `/compact`;
    /// the result replaces the history in the provider request.
    async fn generate_summary(&self, session_id: &str, ledger: &Ledger) -> Result<String> {
        self.summarize_messages(session_id, ledger.messages(), None)
            .await
    }

    /// Summarize only the messages that are about to leave the context. When
    /// `previous` is present, produce a rolling handoff that also retains the
    /// earlier summary's decisions and task state.
    async fn summarize_messages(
        &self,
        session_id: &str,
        mut messages: Vec<ChatMessage>,
        previous: Option<&str>,
    ) -> Result<String> {
        let todos = self
            .todos
            .lock()
            .await
            .get(session_id)
            .cloned()
            .unwrap_or_default();
        let mut system = String::from(
            "You are compacting a coding-agent conversation. The summary replaces the older \
             messages, so preserve everything needed to continue the task without re-reading \
             the log. Use these sections:\n\
             1. Goal and constraints (including explicit user preferences)\n\
             2. Decisions and rejected alternatives\n\
             3. Files, symbols, and code changes (exact paths)\n\
             4. Commands and important results/errors\n\
             5. Task state (what is done, in progress, blocked)\n\
             6. Open questions and the next concrete step\n\
             Be concise but specific. Keep exact paths, commands, error text, and identifiers. \
             Prefer task state, decisions, and next steps over narration; this note may be \
             updated by later compactions. Do not invent facts or add commentary.",
        );
        if !todos.is_empty() {
            system.push_str("\n\nCurrent task list (preserve it):\n");
            system.push_str(&todo_context(&todos));
        }
        messages.insert(0, ChatMessage::system(system));
        if let Some(previous) = previous.map(str::trim).filter(|text| !text.is_empty()) {
            messages.insert(
                1,
                ChatMessage::system(format!(
                    "Previous rolling summary. Update it with the new conversation; keep still-valid \
                     goals, decisions, file paths and next steps, and remove stale details:\n\n{}",
                    cap_text(previous, 12_000)
                )),
            );
        }
        messages.push(ChatMessage::user(
            "Summarize the conversation so far for a fresh context window. Write the compacted \
             summary now. Return only the summary.",
        ));
        let config = self.config.load();
        let model = self
            .require_session(session_id)?
            .model
            .unwrap_or_else(|| config.default_model().to_string());
        let cancel = AtomicBool::new(false);
        let route = config.route(&model);
        let provider = self.provider.load_full();
        let completion = provider
            .stream_chat(&messages, &route, &[], &cancel, |_| {})
            .await
            .map_err(|err| rpc("provider", format!("summary failed: {err}")))?;
        let summary = completion.text.trim().to_string();
        if summary.is_empty() {
            return Err(rpc("provider", "the model returned an empty summary"));
        }
        Ok(summary)
    }

    /// Summarize the oldest history when the request is about to cross
    /// `context.auto_compact_threshold` of the budget. `force` bypasses the
    /// threshold after the provider reports a context-length error. Summary
    /// failures are logged and ignored: a provider hiccup must not block the
    /// turn.
    #[allow(clippy::too_many_arguments)]
    async fn auto_compact_if_needed(
        &self,
        session_id: &str,
        ledger: Ledger,
        force: bool,
        keep: usize,
        tools: &[ToolDefinition],
        events: &mut Vec<Event>,
        last_seq: &mut u64,
        static_context: &StaticContext,
    ) -> (Ledger, bool) {
        let config = self.config.load();
        let info = ledger.info();
        let threshold = config.context.auto_compact_threshold;
        if info.budget_tokens == 0 {
            return (ledger, false);
        }
        let over_threshold = (info.total_tokens as f64) > threshold * info.budget_tokens as f64;
        if !force && !over_threshold {
            return (ledger, false);
        }
        // Forced recovery may have only two user turns, in which case the
        // retention window leaves nothing to summarize. Fall back to keeping
        // one turn rather than giving up on the request.
        let range = ledger
            .auto_compact_range_tokens(config.context.auto_compact_keep_tokens, keep.max(1))
            .or_else(|| {
                if force {
                    ledger.auto_compact_range(1)
                } else if over_threshold {
                    // A tiny test budget or a huge token retention window can
                    // make the whole live conversation fit in the window while
                    // still exceeding the request budget. Fall back to the
                    // turn-based minimum so the request can proceed.
                    ledger.auto_compact_range(keep.max(1))
                } else {
                    None
                }
            });
        let Some((from_seq, to_seq)) = range else {
            return (ledger, false);
        };
        if ledger.overlaps_compaction(from_seq, to_seq) {
            return (ledger, false);
        }
        let (messages, tokens) = ledger.range_stats(from_seq, to_seq);
        if messages == 0 {
            return (ledger, false);
        }
        let previous_summary = ledger.latest_summary();
        let summary = match self
            .summarize_messages(
                session_id,
                ledger.messages_in_range(from_seq, to_seq),
                previous_summary.as_deref(),
            )
            .await
        {
            Ok(summary) => summary,
            Err(err) => {
                eprintln!("eixd: auto-compaction skipped: {err:#}");
                return (ledger, false);
            }
        };
        let event = match self.store.append_event(
            session_id,
            EventKind::ContextCompacted,
            payload(&ContextCompactedPayload {
                from_seq,
                to_seq,
                messages,
                tokens,
                summary,
                rolling: true,
            }),
        ) {
            Ok(event) => event,
            Err(err) => {
                eprintln!("eixd: auto-compaction skipped: {err:#}");
                return (ledger, false);
            }
        };
        self.emit_event(&event);
        self.emit_value(
            "context.compacted",
            json!({
                "session_id": session_id,
                "from_seq": from_seq,
                "to_seq": to_seq,
                "messages": messages,
                "tokens": tokens,
                "auto": true,
            }),
        );
        if let Err(err) = self.refresh_turn_events(session_id, events, last_seq).await {
            eprintln!("eixd: auto-compaction event refresh failed: {err:#}");
            return (ledger, true);
        }
        match self
            .build_ledger_from_events(session_id, tools, events, static_context)
            .await
        {
            Ok(ledger) => {
                self.emit_context(session_id, &ledger);
                (ledger, true)
            }
            Err(err) => {
                eprintln!("eixd: auto-compaction reload failed: {err:#}");
                (ledger, true)
            }
        }
    }

    /// `/clear`: draw a line in the event log before which nothing is sent to
    /// the provider again.
    async fn context_clear(
        &self,
        params: ContextClearParams,
    ) -> Result<eix_core::ContextLedgerResult> {
        let session = self.require_session(&params.session_id)?;
        if session.status.is_busy() {
            return Err(rpc(
                "busy",
                "cancel the running turn before clearing the context",
            ));
        }
        let event = self.store.append_event(
            &params.session_id,
            EventKind::ContextCleared,
            payload(&ContextClearedPayload {}),
        )?;
        self.emit_event(&event);
        let ledger = self.build_ledger(&params.session_id).await?;
        self.emit_context(&params.session_id, &ledger);
        Ok(ledger.info())
    }

    fn announce_patch(&self, session: &Session) {
        self.emit_value(
            "session.attention",
            json!({
                "session_id": session.id,
                "status": SessionStatus::AwaitingApproval.as_str(),
            }),
        );
        self.notify_attention(session, SessionStatus::AwaitingApproval);
    }

    /// Record a write that was committed to disk without a review prompt
    /// (`approval = "auto"` or an explicit `allow` rule).
    fn record_auto_patch(
        &self,
        session_id: &str,
        call: &ToolCall,
        args: &Value,
        reason: &str,
    ) -> Result<()> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let patch_id = Uuid::new_v4().to_string();
        let event = self.store.append_event(
            session_id,
            EventKind::PatchApplied,
            payload(&PatchResultPayload {
                patch_id: patch_id.clone(),
                call_id: call.id.clone(),
                path: path.clone(),
                applied: true,
                conflict: false,
                hunks: Vec::new(),
                snapshot_id: None,
                message: Some(reason.to_string()),
            }),
        )?;
        self.emit_event(&event);
        self.emit_value(
            "patch.applied",
            json!({
                "session_id": session_id,
                "patch_id": patch_id,
                "call_id": call.id,
                "path": path,
                "applied": true,
                "auto": true,
            }),
        );
        Ok(())
    }

    /// Serve `read_file` / `list_dir` from the shadow workspace when a pending
    /// patch touches the requested path. `None` falls back to the real tool.
    async fn read_with_staging(
        &self,
        session_id: &str,
        tool: &str,
        args: &Value,
        cwd: &Path,
    ) -> Option<tools::ToolOutcome> {
        match tool {
            "read_file" => {
                let path = tools::write_target(args, cwd, self.config.load().as_ref())?;
                let content = self.staging.latest_content(session_id, &path)?;
                let offset = args
                    .get("offset")
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .max(1) as usize;
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(2000)
                    .max(1) as usize;
                Some(tools::ToolOutcome {
                    ok: true,
                    output: tools::format_read(
                        &content,
                        offset,
                        limit,
                        self.config.load().tools.max_output_bytes,
                    ),
                    duration_ms: 0,
                })
            }
            "list_dir" => self.staged_listing(session_id, args, cwd).await,
            _ => None,
        }
    }

    /// Merge pending staged paths into a directory listing. Files that only
    /// exist in the shadow workspace are shown so the agent can find them.
    async fn staged_listing(
        &self,
        session_id: &str,
        args: &Value,
        cwd: &Path,
    ) -> Option<tools::ToolOutcome> {
        let patches = self.staging.pending(session_id);
        if patches.is_empty() {
            return None;
        }
        let raw = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let dir = tools::resolve_path_in(cwd, raw, self.config.load().as_ref()).ok()?;
        let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut disk_read = false;
        if let Ok(mut entries) = tokio::fs::read_dir(&dir).await {
            disk_read = true;
            while let Ok(Some(entry)) = entries.next_entry().await {
                let name = entry.file_name().to_string_lossy().into_owned();
                let suffix = match entry.file_type().await {
                    Ok(kind) if kind.is_dir() => "/",
                    _ => "",
                };
                names.insert(format!("{name}{suffix}"));
            }
        }
        for patch in &patches {
            let Ok(relative) = patch.path.strip_prefix(&dir) else {
                continue;
            };
            let mut components = relative.components();
            let Some(first) = components.next() else {
                continue;
            };
            let suffix = if components.next().is_some() { "/" } else { "" };
            names.insert(format!("{}{suffix}", first.as_os_str().to_string_lossy()));
        }
        if !disk_read && names.is_empty() {
            // Nothing staged under this path either: let the real tool report
            // the missing directory.
            return None;
        }
        let text = if names.is_empty() {
            "(empty directory)".to_string()
        } else {
            names.into_iter().collect::<Vec<_>>().join("\n")
        };
        Some(tools::ToolOutcome {
            ok: true,
            output: tools::cap_output(text, self.config.load().tools.max_output_bytes),
            duration_ms: 0,
        })
    }

    /// Apply or reject a staged patch. An empty `patch_id` targets the oldest
    /// pending patch in the session (used by `eix approve` / `eix deny`).
    async fn decide_patch(
        &self,
        session_id: &str,
        patch_id: &str,
        decision: ApprovalDecision,
        hunks: Option<Vec<usize>>,
        reason: Option<String>,
        scope: ApprovalScope,
    ) -> Result<PatchResult> {
        let patch = if patch_id.is_empty() {
            self.staging.oldest(session_id)
        } else {
            self.staging.get(session_id, patch_id)
        };
        let Some(patch) = patch else {
            return Ok(PatchResult {
                accepted: false,
                applied: false,
                conflict: false,
                message: Some("no pending patch".to_string()),
            });
        };
        let Some(patch) = self.staging.take(session_id, &patch.patch_id) else {
            return Ok(PatchResult {
                accepted: false,
                applied: false,
                conflict: false,
                message: Some("no pending patch".to_string()),
            });
        };

        if decision == ApprovalDecision::Always {
            self.remember_always(
                session_id,
                &patch.tool,
                &json!({"path": patch.label.clone()}),
                scope,
            )
            .await;
        }

        if decision == ApprovalDecision::Deny {
            self.emit_patch_result(
                session_id,
                EventKind::PatchRejected,
                &patch,
                false,
                false,
                Vec::new(),
                None,
                reason.clone(),
            )?;
            self.emit_value(
                "patch.rejected",
                json!({
                    "session_id": session_id,
                    "patch_id": patch.patch_id,
                    "call_id": patch.call_id,
                    "path": patch.label,
                    "reason": reason,
                }),
            );
            self.refresh_session(session_id);
            return Ok(PatchResult {
                accepted: true,
                applied: false,
                conflict: false,
                message: reason,
            });
        }

        let cwd = self.require_session(session_id)?.cwd;
        let plan = tools::build_plan(
            patch.path.clone(),
            patch.label.clone(),
            patch.old.clone(),
            patch.new.clone(),
            &self.config.load(),
        );
        if let Err(err) = plan.verify_unchanged().await {
            let message = format!("{err:#}");
            self.emit_patch_result(
                session_id,
                EventKind::PatchConflict,
                &patch,
                false,
                true,
                Vec::new(),
                None,
                Some(message.clone()),
            )?;
            self.emit_value(
                "patch.conflict",
                json!({
                    "session_id": session_id,
                    "patch_id": patch.patch_id,
                    "call_id": patch.call_id,
                    "path": patch.label,
                    "message": message,
                }),
            );
            self.refresh_session(session_id);
            return Ok(PatchResult {
                accepted: true,
                applied: false,
                conflict: true,
                message: Some(message),
            });
        }

        // The snapshot now happens as close to the write as possible.
        let (snapshot_id, snapshot_warning) = if self.config.load().tools.snapshots {
            let result = self
                .snapshots
                .capture_file(session_id, &patch.tool, &patch.call_id, &cwd, &patch.path)
                .await;
            self.record_snapshot(session_id, &patch.tool, &patch.call_id, Scope::File, result)
                .await?
        } else {
            (None, None)
        };

        let outcome = tools::apply_plan(&plan, hunks.as_deref()).await;
        if !outcome.ok {
            // Keep the proposal so the user can retry after fixing the problem.
            self.staging.restore(patch.clone())?;
            return Ok(PatchResult {
                accepted: true,
                applied: false,
                conflict: false,
                message: Some(outcome.output),
            });
        }
        let formatted = tools::format_file(&self.config.load(), &cwd, &patch.path).await;
        let mut message = match (snapshot_warning, formatted) {
            (Some(warning), Some(note)) => {
                format!("{}\n[snapshot failed: {warning}]{note}", outcome.output)
            }
            (Some(warning), None) => {
                format!("{}\n[snapshot failed: {warning}]", outcome.output)
            }
            (None, Some(note)) => format!("{}{note}", outcome.output),
            (None, None) => outcome.output.clone(),
        };
        let config = self.config.load();
        message.push_str(
            &self
                .lsp_notes(&config, &cwd, std::slice::from_ref(&patch.path))
                .await,
        );
        let applied_hunks = hunks.unwrap_or_default();
        self.emit_patch_result(
            session_id,
            EventKind::PatchApplied,
            &patch,
            true,
            false,
            applied_hunks.clone(),
            snapshot_id,
            Some(message.clone()),
        )?;
        self.emit_value(
            "patch.applied",
            json!({
                "session_id": session_id,
                "patch_id": patch.patch_id,
                "call_id": patch.call_id,
                "path": patch.label,
                "applied": true,
                "hunks": applied_hunks,
            }),
        );
        self.refresh_session(session_id);
        Ok(PatchResult {
            accepted: true,
            applied: true,
            conflict: false,
            message: Some(message),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn emit_patch_result(
        &self,
        session_id: &str,
        kind: EventKind,
        patch: &StagedPatch,
        applied: bool,
        conflict: bool,
        hunks: Vec<usize>,
        snapshot_id: Option<String>,
        message: Option<String>,
    ) -> Result<()> {
        let event = self.store.append_event(
            session_id,
            kind,
            payload(&PatchResultPayload {
                patch_id: patch.patch_id.clone(),
                call_id: patch.call_id.clone(),
                path: patch.label.clone(),
                applied,
                conflict,
                hunks,
                snapshot_id,
                message,
            }),
        )?;
        self.emit_event(&event);
        Ok(())
    }

    /// Re-broadcast a session after a patch decision so frontends that
    /// optimistically showed an attention badge catch up with the store.
    fn refresh_session(&self, session_id: &str) {
        if let Ok(session) = self.require_session(session_id) {
            self.emit_session(&session);
        }
    }

    /// Start a background process on behalf of `start_command`.
    async fn start_background(&self, session_id: &str, args: &Value) -> tools::ToolOutcome {
        let started = Instant::now();
        let command = args.get("command").and_then(Value::as_str).unwrap_or("");
        if command.trim().is_empty() {
            return tools::ToolOutcome {
                ok: false,
                output: "command must not be empty".to_string(),
                duration_ms: started.elapsed().as_millis() as u64,
            };
        }
        let session = match self.require_session(session_id) {
            Ok(session) => session,
            Err(err) => {
                return tools::ToolOutcome {
                    ok: false,
                    output: format!("{err:#}"),
                    duration_ms: started.elapsed().as_millis() as u64,
                }
            }
        };
        let cwd = args.get("cwd").and_then(Value::as_str);
        let plan = match tools::command_plan(
            command,
            cwd,
            &session.cwd,
            session.worktree.as_deref(),
            &self.config.load(),
        ) {
            Ok(plan) => plan,
            Err(err) => {
                return tools::ToolOutcome {
                    ok: false,
                    output: format!("{err:#}"),
                    duration_ms: started.elapsed().as_millis() as u64,
                }
            }
        };
        match self.processes.start(session_id, command, plan).await {
            Ok(info) => tools::ToolOutcome {
                ok: true,
                output: format!(
                    "started process {} (pid {}) in {}\nlog: {}\nUse process_output to read its output.",
                    info.id, info.pid, info.cwd, info.log
                ),
                duration_ms: started.elapsed().as_millis() as u64,
            },
            Err(err) => tools::ToolOutcome {
                ok: false,
                output: format!("{err:#}"),
                duration_ms: started.elapsed().as_millis() as u64,
            },
        }
    }

    /// `process_output`: read-only tail of a background process.
    fn read_process(&self, session_id: &str, args: &Value) -> tools::ToolOutcome {
        let started = Instant::now();
        let process_id = args
            .get("process_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty());
        let max_bytes = args
            .get("max_bytes")
            .and_then(Value::as_u64)
            .map(|value| value as usize)
            .unwrap_or(8000)
            .clamp(1, 512 * 1024);
        match self
            .processes
            .output_for_session(session_id, process_id, max_bytes)
        {
            Ok(output) => {
                let mut text = format!(
                    "process {} · {} · {} bytes",
                    output.process_id,
                    output.status.as_str(),
                    output.bytes
                );
                if let Some(code) = output.exit_code {
                    text.push_str(&format!(" · exit {code}"));
                }
                if output.truncated {
                    text.push_str(" · tail");
                }
                text.push('\n');
                text.push_str(&output.text);
                tools::ToolOutcome {
                    ok: true,
                    output: text,
                    duration_ms: started.elapsed().as_millis() as u64,
                }
            }
            Err(err) => tools::ToolOutcome {
                ok: false,
                output: format!("{err:#}"),
                duration_ms: started.elapsed().as_millis() as u64,
            },
        }
    }

    /// Approval-card / notification summary. MCP tools get a JSON argument
    /// preview; built-ins keep their tailored one-liners.
    fn tool_summary(&self, tool: &str, args: &Value) -> String {
        if tool.starts_with(crate::mcp::PREFIX) {
            crate::mcp::summarize(tool, args)
        } else {
            tools::summarize(tool, args)
        }
    }

    fn sandbox_status(&self, decision: &PermissionDecision) -> &'static str {
        if decision.action == PermissionAction::Sandbox {
            return "sandbox";
        }
        match self.config.load().tools.sandbox {
            eix_core::SandboxMode::Off => "off",
            eix_core::SandboxMode::Auto | eix_core::SandboxMode::Required
                if !eix_core::sandbox::bwrap_available() =>
            {
                "unavailable"
            }
            eix_core::SandboxMode::Auto => "auto",
            eix_core::SandboxMode::Required => "required",
        }
    }

    fn record_permission(
        &self,
        session_id: &str,
        call_id: &str,
        tool: &str,
        decision: &PermissionDecision,
    ) -> Result<()> {
        let sandbox = self.sandbox_status(decision);
        let event = self.store.append_event(
            session_id,
            EventKind::PermissionEvaluated,
            payload(&eix_core::PermissionEvaluatedPayload {
                call_id: call_id.to_string(),
                tool: tool.to_string(),
                action: decision.action.as_str().to_string(),
                risk: decision.risk.level.as_str().to_string(),
                risk_categories: decision
                    .risk
                    .categories
                    .iter()
                    .map(|category| category.as_str().to_string())
                    .collect(),
                reasons: decision.risk.reasons.clone(),
                rule: decision.rule_id().map(str::to_string),
                rule_source: decision.rule_source().as_str().to_string(),
                sandbox: sandbox.to_string(),
                parsed_command: decision.risk.parsed_command(),
            }),
        )?;
        self.emit_event(&event);
        Ok(())
    }

    /// Turn an approved "Always" into a narrow rule. Session scope is kept in
    /// memory; user scope also persists it, while project scope cannot widen
    /// permissions and therefore stays session-local.
    async fn remember_always(
        &self,
        session_id: &str,
        tool: &str,
        args: &Value,
        scope: ApprovalScope,
    ) {
        if scope == ApprovalScope::Once {
            return;
        }
        let rule = build_always_rule(tool, args, RuleSource::Session);
        // The current session always gets the narrow in-memory rule so the
        // next call is not delayed by a config reload. User scope additionally
        // persists it; project scope cannot grant `allow` (project config may
        // only tighten), so it stays session-local.
        if scope == ApprovalScope::User {
            let cwd = match self.require_session(session_id) {
                Ok(session) => session.cwd,
                Err(_) => {
                    self.always
                        .lock()
                        .await
                        .entry(session_id.to_string())
                        .or_default()
                        .push(rule);
                    return;
                }
            };
            if let Err(err) = self.append_rule(scope, &rule, &cwd) {
                eprintln!(
                    "eixd: could not persist permission rule: {}",
                    redact(&format!("{err:#}"))
                );
            }
        } else if scope == ApprovalScope::Project {
            eprintln!("eixd: project scope cannot grant allow; keeping rule for this session");
        }
        self.always
            .lock()
            .await
            .entry(session_id.to_string())
            .or_default()
            .push(rule);
    }

    fn append_rule(&self, scope: ApprovalScope, rule: &PermissionRule, cwd: &Path) -> Result<()> {
        let path = match scope {
            ApprovalScope::User => eix_core::config::default_config_path(),
            ApprovalScope::Once | ApprovalScope::Session | ApprovalScope::Project => {
                let _ = cwd;
                return Ok(());
            }
        };
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        let mut text = String::new();
        if path.exists() {
            text = fs::read_to_string(&path)
                .with_context(|| format!("cannot read {}", path.display()))?;
            if !text.ends_with('\n') {
                text.push('\n');
            }
        }
        text.push_str("\n[[tools.rules]]\n");
        text.push_str(&format!("id = {}\n", toml_string(&rule.id)));
        text.push_str(&format!("tool = {}\n", toml_string(&rule.tool)));
        if let Some(path) = rule.path.as_deref() {
            text.push_str(&format!("path = {}\n", toml_string(path)));
        }
        if let Some(command) = rule.command.as_ref() {
            text.push_str("command = [");
            text.push_str(
                &command
                    .iter()
                    .map(|token| toml_string(token))
                    .collect::<Vec<_>>()
                    .join(", "),
            );
            text.push_str("]\n");
        }
        if let Some(signature) = rule.signature.as_deref() {
            text.push_str(&format!("signature = {}\n", toml_string(signature)));
        }
        text.push_str(&format!("action = {}\n", toml_string(rule.action.as_str())));
        let mut options = fs::OpenOptions::new();
        options.create(true).append(true);
        use std::io::Write as _;
        let mut file = options
            .open(&path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        file.write_all(text.as_bytes())
            .with_context(|| format!("cannot append {}", path.display()))?;
        Ok(())
    }

    /// Run an MCP tool and cap its output like any other tool result.
    async fn call_mcp(&self, tool: &str, args: &Value) -> tools::ToolOutcome {
        let started = Instant::now();
        let result = self.mcp.lock().await.call(tool, args).await;
        self.mcp_outcome(result, started)
    }

    /// Read-only helpers for MCP resources and prompts.
    async fn call_mcp_meta(&self, tool: &str, args: &Value) -> tools::ToolOutcome {
        let started = Instant::now();
        let mut mcp = self.mcp.lock().await;
        let config = self.config.load();
        mcp.ensure_loaded(&config).await;
        mcp.refresh(&config).await;
        let server = args.get("server").and_then(Value::as_str);
        let result = match tool {
            crate::mcp::LIST_RESOURCES => mcp.list_resources(server),
            crate::mcp::LIST_PROMPTS => mcp.list_prompts(server),
            crate::mcp::READ_RESOURCE => {
                let (server, uri) = match (server, args.get("uri").and_then(Value::as_str)) {
                    (Some(server), Some(uri)) => (server, uri),
                    _ => {
                        return tools::ToolOutcome {
                            ok: false,
                            output: "mcp_read_resource needs `server` and `uri`".to_string(),
                            duration_ms: started.elapsed().as_millis() as u64,
                        }
                    }
                };
                mcp.read_resource(server, uri).await
            }
            crate::mcp::GET_PROMPT => {
                let (server, name) = match (server, args.get("name").and_then(Value::as_str)) {
                    (Some(server), Some(name)) => (server, name),
                    _ => {
                        return tools::ToolOutcome {
                            ok: false,
                            output: "mcp_get_prompt needs `server` and `name`".to_string(),
                            duration_ms: started.elapsed().as_millis() as u64,
                        }
                    }
                };
                let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));
                mcp.get_prompt(server, name, &arguments).await
            }
            _ => Err(anyhow::anyhow!("unknown MCP helper {tool}")),
        };
        self.mcp_outcome(result, started)
    }

    fn mcp_outcome(&self, result: Result<String>, started: Instant) -> tools::ToolOutcome {
        match result {
            Ok(output) => tools::ToolOutcome {
                ok: true,
                output: tools::cap_output(output, self.config.load().tools.max_output_bytes),
                duration_ms: started.elapsed().as_millis() as u64,
            },
            Err(err) => tools::ToolOutcome {
                ok: false,
                output: format!("{err:#}"),
                duration_ms: started.elapsed().as_millis() as u64,
            },
        }
    }

    async fn skill_allows(&self, session_id: &str, tool: &str) -> bool {
        let allowed = self.skill_tools.lock().await;
        let Some(patterns) = allowed.get(session_id) else {
            return true;
        };
        patterns
            .iter()
            .any(|pattern| wildcard_matches(pattern, tool))
    }

    async fn remember_skill(&self, session_id: &str, args: &Value) {
        let Some(name) = args.get("name").and_then(Value::as_str) else {
            return;
        };
        let Ok(session) = self.require_session(session_id) else {
            return;
        };
        let config = self.config.load();
        let Some(skill) = eix_core::find_skill(&session.cwd, name, &config.compations) else {
            return;
        };
        if skill.allowed_tools.is_empty() {
            return;
        }
        let mut active = self.skill_tools.lock().await;
        let entry = active.entry(session_id.to_string()).or_default();
        for pattern in skill.allowed_tools {
            if !entry.contains(&pattern) {
                entry.push(pattern);
            }
        }
    }

    /// Copy what a mutating tool is about to change and record the result.
    /// A failed snapshot never blocks the call; the tool output gets a short
    /// note so the lost rollback safety is visible.
    async fn capture_snapshot(
        &self,
        session_id: &str,
        tool: &str,
        call: &ToolCall,
        args: &Value,
        risk: &eix_core::RiskAssessment,
        plan: Option<&tools::WritePlan>,
    ) -> Result<Option<String>> {
        let is_mcp = tool.starts_with(crate::mcp::PREFIX);
        let mcp_mutating = is_mcp && risk.categories.contains(&eix_core::RiskCategory::McpWrite);
        if !(risk.mutates_workspace() || mcp_mutating) || !self.config.load().tools.snapshots {
            return Ok(None);
        }
        let cwd = self.require_session(session_id)?.cwd;
        let scope = if matches!(tool, "write_file" | "edit") {
            Scope::File
        } else {
            Scope::Tree
        };
        if is_mcp {
            let result = self
                .snapshots
                .capture_tree(session_id, tool, &call.id, &cwd)
                .await;
            let (_snapshot_id, warning) = self
                .record_snapshot(session_id, tool, &call.id, scope, result)
                .await?;
            return Ok(warning);
        }
        let result = match tool {
            "write_file" | "edit" => {
                let path = plan
                    .map(|plan| plan.path().to_path_buf())
                    .or_else(|| tools::write_target(args, &cwd, self.config.load().as_ref()));
                match path {
                    Some(path) => {
                        self.snapshots
                            .capture_file(session_id, tool, &call.id, &cwd, &path)
                            .await
                    }
                    None => Err(anyhow::anyhow!("cannot resolve the write target")),
                }
            }
            "patch" | "run_command" => {
                self.snapshots
                    .capture_tree(session_id, tool, &call.id, &cwd)
                    .await
            }
            _ => return Ok(None),
        };

        let (_snapshot_id, warning) = self
            .record_snapshot(session_id, tool, &call.id, scope, result)
            .await?;
        Ok(warning)
    }

    /// Record a snapshot outcome (success or failure) as an event.
    async fn record_snapshot(
        &self,
        session_id: &str,
        tool: &str,
        call_id: &str,
        scope: Scope,
        result: Result<crate::snapshot::Snapshot>,
    ) -> Result<(Option<String>, Option<String>)> {
        let (event_payload, warning) = match result {
            Ok(snapshot) => (
                SnapshotPayload {
                    snapshot_id: snapshot.id.clone(),
                    call_id: call_id.to_string(),
                    tool: tool.to_string(),
                    scope: snapshot.scope.as_str().to_string(),
                    ok: true,
                    files: snapshot.files,
                    bytes: snapshot.bytes,
                    skipped: snapshot.skipped,
                    error: None,
                },
                None,
            ),
            Err(err) => {
                let message = format!("{err:#}");
                (
                    SnapshotPayload {
                        snapshot_id: String::new(),
                        call_id: call_id.to_string(),
                        tool: tool.to_string(),
                        scope: scope.as_str().to_string(),
                        ok: false,
                        files: 0,
                        bytes: 0,
                        skipped: 0,
                        error: Some(message.clone()),
                    },
                    Some(message),
                )
            }
        };
        let snapshot_id =
            (!event_payload.snapshot_id.is_empty()).then(|| event_payload.snapshot_id.clone());
        let event =
            self.store
                .append_event(session_id, EventKind::Snapshot, payload(&event_payload))?;
        self.emit_event(&event);
        Ok((snapshot_id, warning))
    }

    /// Diagnostics appended to a successful write. Never fails the tool: a
    /// broken language server is a note, not a lost edit.
    async fn lsp_notes(&self, config: &Config, cwd: &Path, paths: &[PathBuf]) -> String {
        let mut notes = String::new();
        let mut lsp = self.lsp.lock().await;
        for path in paths {
            if !LspSet::handles(config, path) {
                continue;
            }
            match lsp.diagnostics(config, cwd, path).await {
                Ok(diagnostics) if !diagnostics.is_empty() => {
                    notes.push_str("\n\nLSP diagnostics:\n");
                    for diagnostic in diagnostics.iter().take(50) {
                        notes.push_str(&diagnostic.label(path));
                        notes.push('\n');
                    }
                }
                Ok(_) => {}
                Err(err) => notes.push_str(&format!("\n[lsp: {err:#}]")),
            }
        }
        notes
    }

    fn record_tool_result(
        &self,
        session_id: &str,
        call: &ToolCall,
        ok: bool,
        denied: bool,
        output: &str,
        duration_ms: u64,
    ) -> Result<()> {
        let event = self.store.append_event(
            session_id,
            EventKind::ToolResult,
            payload(&ToolResultPayload {
                call_id: call.id.clone(),
                tool: call.function.name.clone(),
                ok,
                denied,
                output: output.to_string(),
                duration_ms: (duration_ms > 0).then_some(duration_ms),
            }),
        )?;
        self.emit_event(&event);
        Ok(())
    }

    /// Ask the user a multiple-choice question and block until they answer or
    /// the turn is cancelled. The selected option label becomes the tool output.
    async fn ask_user(
        &self,
        session_id: &str,
        turn_id: &str,
        call: &ToolCall,
        args: &Value,
        messages: &mut Vec<ChatMessage>,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let (question, options): (String, Vec<InputOption>) = match tools::ask_user_args(args) {
            Ok(parsed) => parsed,
            Err(err) => {
                let output = format!("{err:#}");
                self.record_tool_result(session_id, call, false, false, &output, 0)?;
                messages.push(ChatMessage::tool(call.id.clone(), output));
                return Ok(());
            }
        };

        let input_id = Uuid::new_v4().to_string();
        let (responder, receiver) = oneshot::channel();
        self.inputs.lock().await.insert(
            session_id.to_string(),
            PendingInput {
                input_id: input_id.clone(),
                options: options.len(),
                responder,
            },
        );

        let event = self.store.append_event(
            session_id,
            EventKind::InputRequired,
            payload(&InputRequiredPayload {
                input_id: input_id.clone(),
                call_id: call.id.clone(),
                question: question.clone(),
                options: options.clone(),
            }),
        )?;
        self.emit_event(&event);
        self.emit_value(
            "input.required",
            json!({
                "session_id": session_id,
                "turn_id": turn_id,
                "input_id": input_id,
                "call_id": call.id,
                "question": question,
                "options": options,
            }),
        );
        self.transition(session_id, SessionStatus::AwaitingInput)?;

        let answer = tokio::select! {
            answer = receiver => answer.ok(),
            _ = wait_for_cancel(cancel) => None,
        };
        self.inputs.lock().await.remove(session_id);
        if !cancel.load(Ordering::SeqCst) {
            self.transition(session_id, SessionStatus::Running)?;
        }

        let Some(index) = answer.filter(|index| *index < options.len()) else {
            let output = "the user did not answer (turn cancelled)".to_string();
            self.record_tool_result(session_id, call, false, true, &output, 0)?;
            self.emit_value(
                "tool.completed",
                json!({
                    "session_id": session_id,
                    "turn_id": turn_id,
                    "call_id": call.id,
                    "tool": "ask_user",
                    "ok": false,
                    "denied": true,
                    "duration_ms": 0,
                }),
            );
            messages.push(ChatMessage::tool(call.id.clone(), output));
            return Ok(());
        };

        let option = &options[index];
        let answer = match option
            .description
            .as_deref()
            .map(str::trim)
            .filter(|description| !description.is_empty())
        {
            Some(description) => format!("{} — {description}", option.label),
            None => option.label.clone(),
        };
        let event = self.store.append_event(
            session_id,
            EventKind::InputAnswered,
            payload(&InputAnsweredPayload {
                input_id: input_id.clone(),
                call_id: call.id.clone(),
                answer: answer.clone(),
                option_index: index,
            }),
        )?;
        self.emit_event(&event);

        let output = format!(
            "User selected option {} of {}: {answer}",
            index + 1,
            options.len()
        );
        self.record_tool_result(session_id, call, true, false, &output, 0)?;
        self.emit_value(
            "tool.completed",
            json!({
                "session_id": session_id,
                "turn_id": turn_id,
                "call_id": call.id,
                "tool": "ask_user",
                "ok": true,
                "denied": false,
                "duration_ms": 0,
            }),
        );
        messages.push(ChatMessage::tool(call.id.clone(), output));
        Ok(())
    }

    /// Publish an approval request and block until the user answers or the
    /// turn is cancelled.
    async fn wait_for_approval(
        &self,
        session_id: &str,
        turn_id: &str,
        call: &ToolCall,
        args: &Value,
        decision: &PermissionDecision,
        cancel: &AtomicBool,
    ) -> Result<ApprovalReply> {
        let tool = call.function.name.as_str();
        if cancel.load(Ordering::SeqCst) {
            return Ok(ApprovalReply {
                decision: ApprovalDecision::Deny,
                reason: Some("turn cancelled".to_string()),
                plan: None,
                hunks: None,
                scope: ApprovalScope::Once,
            });
        }

        let approval_id = Uuid::new_v4().to_string();
        let summary = self.tool_summary(tool, args);
        let cwd = self.require_session(session_id)?.cwd;
        let plan = tools::write_plan(tool, args, &cwd, &self.config.load()).await;
        let diff = match &plan {
            Some(plan) => Some(plan.diff().to_string()),
            None if tool == "patch" => {
                let patch_text = args
                    .get("patchText")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                tools::patch_preview(patch_text, &cwd, &self.config.load())
                    .await
                    .ok()
            }
            None => None,
        };
        let hunks = plan
            .as_ref()
            .map(|plan| plan.hunk_infos())
            .unwrap_or_default();
        let (responder, receiver) = oneshot::channel();
        let allow_always =
            decision.risk.level <= eix_core::RiskLevel::Medium && !decision.force_ask;
        self.approvals.lock().await.insert(
            session_id.to_string(),
            PendingApproval {
                approval_id: approval_id.clone(),
                tool: tool.to_string(),
                summary: summary.clone(),
                created_ms: now_ms(),
                allow_always,
                responder,
            },
        );

        let risk_categories: Vec<String> = decision
            .risk
            .categories
            .iter()
            .map(|category| category.as_str().to_string())
            .collect();
        let scopes = if allow_always {
            vec![
                "once".to_string(),
                "session".to_string(),
                "user".to_string(),
            ]
        } else {
            vec!["once".to_string()]
        };
        let sandbox = self.sandbox_status(decision);
        let event = self.store.append_event(
            session_id,
            EventKind::ApprovalRequired,
            payload(&ApprovalRequiredPayload {
                approval_id: approval_id.clone(),
                call_id: call.id.clone(),
                tool: tool.to_string(),
                summary: summary.clone(),
                arguments: args.clone(),
                diff: diff.clone(),
                hunks: hunks.clone(),
                risk: decision.risk.level.as_str().to_string(),
                risk_categories: risk_categories.clone(),
                reasons: decision.risk.reasons.clone(),
                rule: decision.rule_id().map(str::to_string),
                rule_source: decision.rule_source().as_str().to_string(),
                sandbox: sandbox.to_string(),
                parsed_command: decision.risk.parsed_command(),
                scopes,
            }),
        )?;
        self.emit_event(&event);
        self.emit_value(
            "tool.approval_required",
            json!({
                "session_id": session_id,
                "turn_id": turn_id,
                "approval_id": approval_id,
                "call_id": call.id,
                "tool": tool,
                "summary": summary,
                "arguments": args,
                "diff": diff,
                "hunks": hunks,
                "risk": decision.risk.level.as_str(),
                "risk_categories": risk_categories,
                "reasons": decision.risk.reasons,
                "rule": decision.rule_id(),
                "rule_source": decision.rule_source().as_str(),
                "sandbox": sandbox,
                "parsed_command": decision.risk.parsed_command(),
                "allow_always": allow_always,
            }),
        );
        self.transition(session_id, SessionStatus::AwaitingApproval)?;

        let mut reply = tokio::select! {
            reply = receiver => reply.unwrap_or(ApprovalReply {
                decision: ApprovalDecision::Deny,
                reason: Some("approval channel closed".to_string()),
                plan: None,
                hunks: None,
                scope: ApprovalScope::Once,
            }),
            _ = wait_for_cancel(cancel) => ApprovalReply {
                decision: ApprovalDecision::Deny,
                reason: Some("turn cancelled".to_string()),
                plan: None,
                hunks: None,
                scope: ApprovalScope::Once,
            },
        };
        reply.plan = plan;

        self.approvals.lock().await.remove(session_id);
        if !cancel.load(Ordering::SeqCst) {
            self.transition(session_id, SessionStatus::Running)?;
        }
        Ok(reply)
    }

    fn finish_cancelled(
        &self,
        session_id: &str,
        turn_id: &str,
        streamed: &str,
        reasoning: &str,
        model: &str,
    ) -> Result<()> {
        if !streamed.trim().is_empty() || !reasoning.trim().is_empty() {
            let event = self.store.append_event(
                session_id,
                EventKind::AssistantMessage,
                payload(&eix_core::AssistantMessagePayload {
                    text: streamed.to_string(),
                    model: model.to_string(),
                    reasoning: (!reasoning.is_empty()).then(|| reasoning.to_string()),
                    cancelled: true,
                    usage: None,
                    tool_calls: Vec::new(),
                }),
            )?;
            self.emit_event(&event);
        }
        self.transition(session_id, SessionStatus::Idle)?;
        self.emit_value(
            "turn.completed",
            json!({
                "session_id": session_id,
                "turn_id": turn_id,
                "cancelled": true,
                "error": Value::Null,
            }),
        );
        Ok(())
    }

    fn fail_turn(&self, session_id: &str, turn_id: &str, message: &str) {
        if let Ok(event) = self.store.append_event(
            session_id,
            EventKind::Error,
            payload(&ErrorPayload {
                message: message.to_string(),
            }),
        ) {
            self.emit_event(&event);
        }
        if let Err(err) = self.transition(session_id, SessionStatus::Error) {
            eprintln!(
                "eixd: cannot mark {session_id} as errored: {}",
                redact(&format!("{err:#}"))
            );
        }
        self.emit_value(
            "turn.completed",
            json!({
                "session_id": session_id,
                "turn_id": turn_id,
                "cancelled": false,
                "error": message,
            }),
        );
    }
}

async fn wait_for_cancel(cancel: &AtomicBool) {
    while !cancel.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn retry_delay(
    base: Duration,
    max: Duration,
    attempt: u32,
    retry_after: Option<Duration>,
) -> Duration {
    if let Some(after) = retry_after {
        return after.clamp(Duration::from_millis(1), max);
    }
    let factor = 1u32 << attempt.saturating_sub(1).min(20);
    base.saturating_mul(factor).min(max)
}

/// Class-aware backoff. `Retry-After` wins when present; otherwise rate
/// limits use a one-second floor and transient failures get jitter so a fleet
/// of agents does not retry in lockstep.
fn retry_delay_for_class(
    base: Duration,
    max: Duration,
    attempt: u32,
    retry_after: Option<Duration>,
    class: RetryClass,
) -> Duration {
    if retry_after.is_some() {
        return retry_delay(base, max, attempt, retry_after);
    }
    let base = base.max(class.backoff_floor());
    jitter(retry_delay(base, max, attempt, None)).min(max)
}

/// Add up to 25% jitter. UUID v4 is already part of the daemon and gives us a
/// cheap source of entropy without another dependency.
fn jitter(delay: Duration) -> Duration {
    let millis = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);
    let span = (millis / 4).max(1);
    let offset = (uuid::Uuid::new_v4().as_u128() as u64) % span;
    delay.saturating_add(Duration::from_millis(offset))
}

/// Blend a new provider observation into the current estimate scale. The
/// ratio is clamped so a single anomalous usage report cannot dominate, and
/// the final scale stays within 0.5x–2x.
fn calibrated_scale(current: f64, estimated: usize, actual: usize) -> f64 {
    if estimated == 0 || actual == 0 {
        return current;
    }
    let observed = (actual as f64 / estimated as f64).clamp(0.5, 2.0);
    (current * 0.7 + observed * 0.3).clamp(0.5, 2.0)
}

/// Sleep, but wake up promptly when the turn is cancelled.
async fn sleep_cancellable(delay: Duration, cancel: &AtomicBool) -> bool {
    let deadline = Instant::now() + delay;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return true;
        }
        let now = Instant::now();
        if now >= deadline {
            return false;
        }
        tokio::time::sleep((deadline - now).min(Duration::from_millis(100))).await;
    }
}

const TOOL_ROUND_WARNING: usize = 8;

/// A system reminder injected near the end of the tool budget. It is not
/// persisted: the next round rebuilds messages from the event log, so the
/// reminder is added again only while it still applies.
fn tool_round_warning(round: usize, max_rounds: usize) -> Option<String> {
    let remaining = max_rounds.saturating_sub(round);
    (remaining <= TOOL_ROUND_WARNING).then(|| {
        format!(
            "Tool budget: {round}/{max_rounds} rounds used, {remaining} left. \
             Stop calling tools and provide your best final answer now. \
             If something is incomplete or unverified, say so explicitly \
             instead of starting more exploration."
        )
    })
}

/// The final round deliberately hides tool definitions. At that point the
/// alternative is a hard failure with no answer, so force a synthesis.
fn tools_for_round(round: usize, max_rounds: usize, tools: &[ToolDefinition]) -> &[ToolDefinition] {
    if round >= max_rounds {
        &[]
    } else {
        tools
    }
}

fn truncate_ui(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

async fn write_loop<W>(mut write: W, mut rx: mpsc::UnboundedReceiver<ServerMessage>)
where
    W: AsyncWrite + Unpin,
{
    while let Some(message) = rx.recv().await {
        let mut bytes = match serde_json::to_vec(&message) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        bytes.push(b'\n');
        if write.write_all(&bytes).await.is_err() {
            break;
        }
        if write.flush().await.is_err() {
            break;
        }
    }
}

/// Forward every daemon event to one connection. Unix clients subscribe at
/// connect time; TCP clients subscribe only after `auth`, so `spawn_bus` is
/// called from the auth branch.
fn spawn_bus(
    bus: broadcast::Sender<Notification>,
    tx: mpsc::UnboundedSender<ServerMessage>,
    filter: Arc<RwLock<Option<HashSet<SessionId>>>>,
) -> tokio::task::JoinHandle<()> {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(notification) => {
                    let allowed = {
                        let filter = filter.read().await;
                        match filter.as_ref() {
                            None => true,
                            Some(ids) => notification_session_id(&notification)
                                .is_none_or(|id| ids.contains(&id)),
                        }
                    };
                    if !allowed {
                        continue;
                    }
                    if tx.send(ServerMessage::Notification(notification)).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    let note = Notification::new("stream.lagged", json!({ "skipped": skipped }));
                    if tx.send(ServerMessage::Notification(note)).is_err() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

/// Session a broadcast belongs to, if any. `session.updated` carries the
/// session itself (`id`); every other event carries `session_id`. Notifications
/// without either (e.g. `stream.lagged`) reach every subscriber.
fn notification_session_id(notification: &Notification) -> Option<SessionId> {
    notification
        .params
        .get("session_id")
        .and_then(Value::as_str)
        .or_else(|| notification.params.get("id").and_then(Value::as_str))
        .map(str::to_string)
}

fn parse_params<T: DeserializeOwned>(params: Value) -> Result<T> {
    serde_json::from_value(params).map_err(|err| rpc("invalid_params", err.to_string()))
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct RpcError {
    code: String,
    message: String,
}

fn rpc(code: impl Into<String>, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(RpcError {
        code: code.into(),
        message: message.into(),
    })
}

/// Compare two secrets without short-circuiting on the first differing byte.
/// This is not a hardened constant-time primitive, but it removes the trivial
/// timing signal of `==` for the remote-attach token.
pub(crate) fn secret_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// First eight bytes of a session id, for human-facing messages.
fn short_id(id: &str) -> &str {
    &id[..id.len().min(8)]
}

/// Truncate tool output without splitting a UTF-8 code point.
fn cap_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated]", &text[..end])
}

/// Validate the `todo` tool payload. The model sends the complete list on
/// every call, so stale items disappear without a separate delete operation.
fn parse_todos(args: &Value) -> Result<Vec<eix_core::TodoItem>> {
    let items = args
        .get("todos")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("todos must be an array"))?;
    let mut parsed = Vec::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let content = item
            .get("content")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|content| !content.is_empty())
            .ok_or_else(|| anyhow::anyhow!("todos[{index}].content must be non-empty"))?;
        let status = item
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        if !matches!(
            status,
            "pending" | "in_progress" | "completed" | "cancelled"
        ) {
            bail!("todos[{index}].status must be pending, in_progress, completed or cancelled");
        }
        parsed.push(eix_core::TodoItem {
            content: content.to_string(),
            status: status.to_string(),
        });
    }
    Ok(parsed)
}

/// Stable task-state block for the context ledger. Unlike the tool result, it
/// is phrased as current state so a compacted conversation still knows what is
/// in progress and what remains.
fn todo_context(items: &[eix_core::TodoItem]) -> String {
    let mut out = String::from("Current task list (keep this up to date):\n");
    for item in items {
        let marker = match item.status.as_str() {
            "completed" => "[x]",
            "in_progress" => "[>]",
            "cancelled" => "[-]",
            _ => "[ ]",
        };
        out.push_str(&format!("{marker} {}\n", item.content));
    }
    out.trim_end().to_string()
}

fn render_todos(items: &[eix_core::TodoItem]) -> String {
    if items.is_empty() {
        return "task list cleared".to_string();
    }
    let mut out = String::from("task list updated:\n");
    for item in items {
        let marker = match item.status.as_str() {
            "completed" => "[x]",
            "in_progress" => "[>]",
            "cancelled" => "[-]",
            _ => "[ ]",
        };
        out.push_str(&format!("{marker} {}\n", item.content));
    }
    out.trim_end().to_string()
}

/// One-line preview for `session.timeline`. Returns `None` for bookkeeping
/// events that are not useful for navigation.
fn timeline_preview(kind: EventKind, payload: &Value) -> Option<String> {
    let text = match kind {
        EventKind::UserMessage | EventKind::AssistantMessage => {
            payload.get("text")?.as_str()?.trim().to_string()
        }
        EventKind::ToolCall => {
            let name = payload
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let summary = payload
                .get("summary")
                .and_then(Value::as_str)
                .unwrap_or_default();
            format!("{name} {summary}").trim().to_string()
        }
        EventKind::Error => payload
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("error")
            .to_string(),
        EventKind::ContextCompacted => "context compacted".to_string(),
        EventKind::Integration => payload.get("text")?.as_str()?.trim().to_string(),
        _ => return None,
    };
    if text.is_empty() {
        None
    } else {
        Some(cap_text(&text, 160))
    }
}

fn default_name(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("session")
        .to_string()
}

/// Keep a session in the same subdirectory when its auto-worktree is created.
/// Starting in `repo/go` must not silently move the agent to the repository
/// root, otherwise it loses the language/tooling context the user already had.
fn worktree_session_cwd(repo: &Path, original: &Path, worktree: &Path) -> PathBuf {
    original
        .strip_prefix(repo)
        .ok()
        .filter(|relative| !relative.as_os_str().is_empty())
        .map(|relative| worktree.join(relative))
        .filter(|path| path.is_dir())
        .unwrap_or_else(|| worktree.to_path_buf())
}

fn needs_auto_name(session: &Session) -> bool {
    let name = session.name.trim();
    name.is_empty() || name == "session" || name == default_name(&session.cwd)
}

fn is_mcp_meta(tool: &str) -> bool {
    matches!(
        tool,
        crate::mcp::LIST_RESOURCES
            | crate::mcp::READ_RESOURCE
            | crate::mcp::LIST_PROMPTS
            | crate::mcp::GET_PROMPT
    )
}

/// Keep built-in schemas and append dynamic MCP schemas while they fit. The
/// caller logs `omitted` so a truncated tool list is never silent.
fn cap_tool_schemas(
    mut builtins: Vec<ToolDefinition>,
    dynamic: Vec<ToolDefinition>,
    cap: usize,
) -> (Vec<ToolDefinition>, usize) {
    let mut used = estimate_tool_schema_tokens(&builtins);
    let mut omitted = 0;
    for tool in dynamic {
        let cost = estimate_tool_schema_tokens(std::slice::from_ref(&tool));
        if used.saturating_add(cost) <= cap {
            used += cost;
            builtins.push(tool);
        } else {
            omitted += 1;
        }
    }
    (builtins, omitted)
}

/// Tiny wildcard matcher for skill tool restrictions (`*` and `?`).
fn wildcard_matches(pattern: &str, text: &str) -> bool {
    fn inner(pattern: &[u8], text: &[u8]) -> bool {
        match pattern.split_first() {
            None => text.is_empty(),
            Some((b'*', rest)) => {
                inner(rest, text) || (!text.is_empty() && inner(pattern, &text[1..]))
            }
            Some((b'?', rest)) => !text.is_empty() && inner(rest, &text[1..]),
            Some((byte, rest)) => text.first() == Some(byte) && inner(rest, &text[1..]),
        }
    }
    inner(pattern.as_bytes(), text.as_bytes())
}

/// Read prompt images. Images are capped so one prompt cannot blow up the
/// event log or the provider request.
async fn load_images(
    cwd: &Path,
    config: &Config,
    paths: &[String],
    blobs: &BlobStore,
) -> Result<Vec<eix_core::ImageAttachment>> {
    if paths.len() > config.images.max_prompt_images {
        bail!(
            "at most {} images can be attached to one prompt",
            config.images.max_prompt_images
        );
    }
    let mut images = Vec::with_capacity(paths.len());
    for raw in paths {
        let path = tools::resolve_path_in(cwd, raw, config)?;
        let meta = tokio::fs::metadata(&path)
            .await
            .with_context(|| format!("cannot read image {raw}"))?;
        if meta.len() > config.images.max_image_bytes {
            bail!(
                "image {raw} is larger than {} MB",
                config.images.max_image_bytes / (1024 * 1024)
            );
        }
        let mime = match path
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
            .as_deref()
        {
            Some("png") => "image/png",
            Some("jpg" | "jpeg") => "image/jpeg",
            Some("gif") => "image/gif",
            Some("webp") => "image/webp",
            _ => bail!("unsupported image type for {raw} (png, jpg, gif, webp)"),
        };
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("cannot read image {raw}"))?;
        let blob = blobs.put(&bytes).await?;
        images.push(eix_core::ImageAttachment {
            path: raw.clone(),
            mime: mime.to_string(),
            data: String::new(),
            blob: Some(blob),
            bytes: Some(bytes.len() as u64),
        });
    }
    Ok(images)
}

/// Files a successful write touched, for post-edit diagnostics.
fn written_paths(tool: &str, args: &Value, cwd: &Path, config: &Config) -> Vec<PathBuf> {
    if matches!(tool, "write_file" | "edit") {
        return tools::write_target(args, cwd, config).into_iter().collect();
    }
    if tool != "patch" {
        return Vec::new();
    }
    args.get("patchText")
        .and_then(Value::as_str)
        .map(|text| {
            text.lines()
                .filter_map(|line| {
                    line.strip_prefix("*** Add File: ")
                        .or_else(|| line.strip_prefix("*** Update File: "))
                })
                .filter_map(|raw| tools::resolve_path_in(cwd, raw.trim(), config).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn build_always_rule(tool: &str, args: &Value, source: RuleSource) -> PermissionRule {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(str::to_string);
    let command = args.get("command").and_then(Value::as_str);
    let (command, signature) = match command {
        Some(command) => {
            let analysis = eix_core::risk::analyze_command(command);
            if analysis.dynamic || analysis.segments.len() != 1 {
                (None, serde_json::to_string(args).ok())
            } else {
                (Some(analysis.segments[0].argv.clone()), None)
            }
        }
        None => (
            None,
            if path.is_none() {
                serde_json::to_string(args).ok()
            } else {
                None
            },
        ),
    };
    PermissionRule {
        id: format!("always-{tool}-{}", now_ms()),
        tool: tool.to_string(),
        path,
        command,
        signature,
        action: PermissionAction::Allow,
        source,
        ..PermissionRule::default()
    }
}

fn toml_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_default()
}

fn auto_name(text: &str) -> String {
    let first = text.lines().next().unwrap_or("").trim();
    let mut name: String = first.chars().take(48).collect();
    if first.chars().count() > 48 {
        name.push('…');
    }
    if name.is_empty() {
        "session".to_string()
    } else {
        name
    }
}

/// Session titles are short labels: keep Unicode letters/digits, separate
/// English words with a single space, and drop other punctuation. Chinese and
/// other non-ASCII runs stay contiguous, so `登录 / 修复` becomes `登录修复`.
fn sanitize_session_name(text: &str) -> String {
    const MAX: usize = 32;
    let mut out = String::new();
    let mut previous_ascii = false;
    let mut has_word = false;
    for word in text.split(|ch: char| !ch.is_alphanumeric()) {
        if word.is_empty() {
            continue;
        }
        let ascii = word.chars().any(|ch| ch.is_ascii_alphanumeric());
        if has_word && (ascii || previous_ascii) {
            out.push(' ');
        }
        // Normalize all-lowercase English words to title case. Preserve mixed
        // casing such as `iOS` and acronyms such as `API`.
        let title_case = word
            .chars()
            .all(|ch| !ch.is_ascii_alphabetic() || ch.is_ascii_lowercase());
        let mut first_alpha = true;
        for ch in word.chars() {
            if title_case && first_alpha && ch.is_ascii_alphabetic() {
                out.push(ch.to_ascii_uppercase());
                first_alpha = false;
            } else {
                if ch.is_ascii_alphabetic() {
                    first_alpha = false;
                }
                out.push(ch);
            }
        }
        previous_ascii = ascii;
        has_word = true;
    }
    let mut name: String = out.chars().take(MAX).collect();
    while name.ends_with(' ') {
        name.pop();
    }
    name
}

/// Models occasionally wrap the answer in prose or quotes despite the prompt.
fn clean_generated_name(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let lower = line.to_ascii_lowercase();
    let line = if lower.starts_with("session name:") {
        line["session name:".len()..].trim()
    } else if lower.starts_with("name:") {
        line["name:".len()..].trim()
    } else if lower.starts_with("title:") {
        line["title:".len()..].trim()
    } else {
        line
    };
    let line = line
        .trim_matches(|ch: char| ch == '"' || ch == '\'' || ch == '`' || ch == '#' || ch == '*')
        .trim();
    sanitize_session_name(line)
}

impl Daemon {
    /// After the second user turn, ask the model for a short title and update
    /// the session row. A manually renamed session is never overwritten.
    async fn maybe_auto_name_session(&self, session_id: &str) {
        let Ok(session) = self.require_session(session_id) else {
            return;
        };
        if session.origin == SessionOrigin::Agent || !needs_auto_name(&session) {
            return;
        }
        let Ok(events) = self.store.events(session_id) else {
            return;
        };
        let mut user_count = 0usize;
        let mut first_user = String::new();
        let mut transcript = String::new();
        for event in &events {
            match event.kind {
                EventKind::UserMessage => {
                    let Ok(message) =
                        serde_json::from_value::<UserMessagePayload>(event.payload.clone())
                    else {
                        continue;
                    };
                    user_count += 1;
                    if user_count > 2 {
                        break;
                    }
                    if first_user.is_empty() {
                        first_user = message.text.clone();
                    }
                    let text = if message.text.trim().is_empty() {
                        "[image]".to_string()
                    } else {
                        cap_text(message.text.trim(), 1_500)
                    };
                    transcript.push_str(&format!("User: {text}\n\n"));
                }
                EventKind::AssistantMessage => {
                    if user_count == 0 || user_count > 2 {
                        continue;
                    }
                    let Ok(message) = serde_json::from_value::<eix_core::AssistantMessagePayload>(
                        event.payload.clone(),
                    ) else {
                        continue;
                    };
                    if message.text.trim().is_empty() {
                        continue;
                    }
                    transcript.push_str(&format!(
                        "Assistant: {}\n\n",
                        cap_text(message.text.trim(), 1_500)
                    ));
                }
                _ => {}
            }
        }
        if user_count < 2 {
            return;
        }
        let fallback = sanitize_session_name(&auto_name(if first_user.trim().is_empty() {
            "session"
        } else {
            &first_user
        }));
        let generated = self
            .generate_session_name(&session, &transcript)
            .await
            .and_then(|raw| {
                let name = clean_generated_name(&raw);
                (!name.is_empty()).then_some(name)
            });
        let name = generated.unwrap_or(fallback);
        let name = if name.is_empty() {
            "session".to_string()
        } else {
            name
        };
        // Re-read after the provider call: a manual rename wins.
        let Ok(latest) = self.require_session(session_id) else {
            return;
        };
        if !needs_auto_name(&latest) {
            return;
        }
        if let Ok(Some(renamed)) = self.store.rename_session(session_id, &name) {
            self.emit_session(&renamed);
        }
    }

    async fn generate_session_name(&self, session: &Session, transcript: &str) -> Option<String> {
        let config = self.config.load_full();
        let model = session
            .model
            .clone()
            .unwrap_or_else(|| config.default_model().to_string());
        let route = config.route(&model);
        let messages = vec![
            ChatMessage::system(
                "You name coding-agent sessions. Read the conversation and reply with only a short session title. Requirements: 2-6 words, at most 32 characters including spaces, use the user's language, separate English words with single spaces, no punctuation, no quotes, no markdown.",
            ),
            ChatMessage::user(format!(
                "Conversation:\n\n{}\nReturn only the session title.",
                cap_text(transcript.trim(), 8_000)
            )),
        ];
        let cancel = AtomicBool::new(false);
        let provider = self.provider.load_full();
        let completion = provider
            .stream_chat(&messages, &route, &[], &cancel, |_| {})
            .await
            .ok()?;
        Some(completion.text)
    }
}

fn system_prompt(config: &Config, session: &Session, permission: PermissionLevel) -> String {
    let mut prompt = if let Some(prompt) = &config.provider_runtime.system_prompt {
        prompt.clone()
    } else {
        let permission_note = match permission {
            PermissionLevel::ReadOnly => {
                "\nThis session is read-only: do not attempt to modify files or run commands."
            }
            PermissionLevel::Full => {
                "\nThis session may modify files without asking. Stay inside the workspace and be careful."
            }
            PermissionLevel::WorkspaceWrite => "",
        };
        format!(
            "You are eix, a coding agent in the user's terminal. Work inside `{}`.\n\
             Be concise, concrete, and honest. If you are unsure, inspect the workspace or ask.\n\
             Do not claim to have run commands or changed files unless you actually did.\n\
             Infer the language and toolchain from the session cwd and nearby files; do not ask \
             which language to use when the working directory already makes it clear.\n\
             For small, self-contained tasks, skip the task list and repository/git metadata \
             checks: implement directly, run one focused verification, and state assumptions.\n\
             For larger multi-step work, keep a short task list with the `todo` tool and update \
             it as work progresses. Inspect the relevant code before editing, make the smallest \
             coherent change, and verify it with focused tests or builds. When a tool fails, \
             read the error and adjust instead of repeating the same call. Use `memory` only for \
             durable workspace facts that will help future sessions.{permission_note}",
            session.cwd.display()
        )
    };
    let agents = eix_core::load_agents(&session.cwd, &config.compations);
    if let Some(name) = session.agent.as_deref() {
        if let Some(agent) = agents.iter().find(|agent| agent.name == name) {
            let instructions = agent.prompt.trim();
            if !instructions.is_empty() {
                prompt.push_str("\n\n");
                prompt.push_str(instructions);
            }
        }
    }
    if session.origin != SessionOrigin::Agent {
        let subagents: Vec<String> = agents
            .iter()
            .filter(|agent| agent.mode.can_subagent() && !agent.description.is_empty())
            .map(|agent| format!("{} ({})", agent.name, agent.description))
            .collect();
        if !subagents.is_empty() {
            prompt
                .push_str("\n\nSub-agents available to `spawn_agent` (pass the name as `agent`): ");
            prompt.push_str(&subagents.join("; "));
            prompt.push('.');
        }
    }
    if session.has_worktree() && config.integration.enabled {
        prompt.push_str(
            "\n\n## Worktree integration\n\
             This session runs in an isolated git worktree. Commit your changes before you finish.\n\
             Run the project tests after your edits. When they pass, call the `ready` tool (or run\n\
             `eix ready`) so the daemon can rebase, test and fast-forward the base branch. Do not\n\
             merge or rebase the base branch yourself. If eix asks you to repair a rebase conflict\n\
             or a failing integration test, resolve it in this worktree, run the tests, then call\n\
             `ready` again.",
        );
    }
    let skills = eix_core::load_skills(&session.cwd, &config.compations);
    prompt.push_str(&eix_core::available_skills_prompt(&skills));
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_names_are_short_and_single_line() {
        assert_eq!(auto_name("fix auth\nmore"), "fix auth");
        let long = "x".repeat(80);
        let name = auto_name(&long);
        assert_eq!(name.chars().count(), 49);
        assert!(name.ends_with('…'));
    }

    #[test]
    fn generated_names_keep_spaces_between_english_words() {
        assert_eq!(sanitize_session_name("fix: auth_bug!"), "Fix Auth Bug");
        assert_eq!(sanitize_session_name("  登录 / 修复  "), "登录修复");
        assert_eq!(sanitize_session_name("Hello, world."), "Hello World");
        assert_eq!(sanitize_session_name("Fix 登录 bug"), "Fix 登录 Bug");
        assert_eq!(sanitize_session_name("iOS 26 build"), "iOS 26 Build");
        assert_eq!(sanitize_session_name("!!!"), "");
        assert_eq!(clean_generated_name("Session name: `FixAuth`"), "FixAuth");
        assert_eq!(
            clean_generated_name("Title: refactor cache"),
            "Refactor Cache"
        );
        let long = sanitize_session_name(&"x".repeat(80));
        assert_eq!(long.chars().count(), 32);
        assert!(long.chars().all(char::is_alphanumeric));
        let words = sanitize_session_name(&"word ".repeat(20));
        assert!(words.chars().count() <= 32);
        assert!(!words.ends_with(' '));
    }

    #[test]
    fn default_names_fall_back_to_session() {
        assert_eq!(default_name(Path::new("/")), "session");
        assert_eq!(default_name(Path::new("/home/me/eix")), "eix");
    }

    #[test]
    fn auto_worktree_keeps_the_original_subdirectory() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let base =
            std::env::temp_dir().join(format!("eix-worktree-cwd-{}-{stamp}", std::process::id()));
        let repo = base.join("repo");
        let subdir = repo.join("go");
        let worktree = base.join("worktree");
        std::fs::create_dir_all(worktree.join("go")).unwrap();
        std::fs::create_dir_all(&subdir).unwrap();

        assert_eq!(
            worktree_session_cwd(&repo, &subdir, &worktree),
            worktree.join("go")
        );
        assert_eq!(worktree_session_cwd(&repo, &repo, &worktree), worktree);
        assert_eq!(
            worktree_session_cwd(&repo, &repo.join("missing"), &worktree),
            worktree
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn cwd_derived_names_are_eligible_for_auto_naming() {
        let mut session = Session::new("id", "eix", "/home/me/eix");
        assert!(needs_auto_name(&session));
        session.name = "auth".to_string();
        assert!(!needs_auto_name(&session));
    }

    #[test]
    fn busy_and_attention_are_distinct() {
        assert!(SessionStatus::Running.is_busy());
        assert!(SessionStatus::AwaitingApproval.is_busy());
        assert!(!SessionStatus::Error.is_busy());
    }

    #[test]
    fn tool_round_budget_warns_and_forces_the_final_answer() {
        let tools = vec![ToolDefinition::function(
            "read_file",
            "read a file",
            json!({ "type": "object" }),
        )];
        assert!(tool_round_warning(0, 64).is_none());
        assert!(tool_round_warning(55, 64).is_none());
        assert!(tool_round_warning(56, 64).is_some());
        assert!(tool_round_warning(64, 64).is_some());

        assert_eq!(tools_for_round(63, 64, &tools).len(), 1);
        assert!(tools_for_round(64, 64, &tools).is_empty());
    }

    #[test]
    fn prompt_usage_calibration_is_smoothed_and_bounded() {
        let close = |a: f64, b: f64| assert!((a - b).abs() < 1e-9, "{a} != {b}");
        close(calibrated_scale(1.0, 1_000, 2_000), 1.3);
        close(calibrated_scale(1.0, 1_000, 0), 1.0);
        close(calibrated_scale(1.0, 0, 2_000), 1.0);
        close(calibrated_scale(1.0, 1_000, 100_000), 1.3);
        close(calibrated_scale(1.9, 1_000, 100_000), 1.93);
        close(calibrated_scale(0.6, 1_000, 1), 0.57);
    }

    #[test]
    fn dynamic_tool_schemas_are_omitted_when_they_exceed_the_cap() {
        let builtin = vec![ToolDefinition::function(
            "read_file",
            "Read a file",
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        )];
        let dynamic = vec![
            ToolDefinition::function(
                "mcp__one__tool",
                "First dynamic tool",
                json!({"type": "object", "properties": {"a": {"type": "string"}}}),
            ),
            ToolDefinition::function(
                "mcp__two__tool",
                "Second dynamic tool",
                json!({"type": "object", "properties": {"b": {"type": "string"}}}),
            ),
        ];
        let builtin_cost = estimate_tool_schema_tokens(&builtin);
        let first_cost = estimate_tool_schema_tokens(&dynamic[..1]);
        let (kept, omitted) =
            cap_tool_schemas(builtin.clone(), dynamic.clone(), builtin_cost + first_cost);
        assert_eq!(kept.len(), 2);
        assert_eq!(omitted, 1);
        // Built-ins survive even when the cap is already exhausted.
        let (kept, omitted) = cap_tool_schemas(builtin, dynamic, 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(omitted, 2);
    }

    #[test]
    fn retry_delay_is_exponential_and_bounded() {
        let base = Duration::from_millis(500);
        let max = Duration::from_secs(5);
        assert_eq!(retry_delay(base, max, 1, None), base);
        assert_eq!(retry_delay(base, max, 2, None), Duration::from_secs(1));
        assert_eq!(retry_delay(base, max, 10, None), max);
        assert_eq!(
            retry_delay(base, max, 1, Some(Duration::from_secs(2))),
            Duration::from_secs(2)
        );
        assert_eq!(
            retry_delay(base, max, 1, Some(Duration::from_secs(60))),
            max
        );
    }

    #[test]
    fn retry_delay_applies_class_specific_backoff() {
        let base = Duration::from_millis(100);
        let max = Duration::from_secs(60);

        // Transient failures start at the configured base with up to 25%
        // jitter.
        let transient = retry_delay_for_class(base, max, 1, None, RetryClass::Transient);
        assert!(transient >= base, "{transient:?}");
        assert!(transient <= Duration::from_millis(125), "{transient:?}");

        // Rate limits without Retry-After use a 500ms floor.
        let rate_limited = retry_delay_for_class(base, max, 1, None, RetryClass::RateLimited);
        assert!(
            rate_limited >= Duration::from_millis(500),
            "{rate_limited:?}"
        );
        assert!(
            rate_limited <= Duration::from_millis(625),
            "{rate_limited:?}"
        );

        // Retry-After is authoritative and capped by retry_max_secs.
        assert_eq!(
            retry_delay_for_class(
                base,
                max,
                1,
                Some(Duration::from_secs(7)),
                RetryClass::RateLimited,
            ),
            Duration::from_secs(7)
        );
        assert_eq!(
            retry_delay_for_class(
                base,
                max,
                1,
                Some(Duration::from_secs(90)),
                RetryClass::RateLimited,
            ),
            max
        );
    }
}
