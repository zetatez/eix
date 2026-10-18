//! Unix-socket JSON-RPC server and the agent turn loop.
//!
//! A turn is: append the user event -> mark running -> stream provider deltas ->
//! append the assistant event -> mark idle. All state changes are written to
//! SQLite before they are broadcast, so a frontend can always recover by
//! re-attaching and replaying the event log.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use eix_core::{
    now_ms, payload, ApprovalDecision, ApprovalInfo, ApprovalListParams, ApprovalListResult,
    ApprovalRequiredPayload, ApprovalRespondParams, ApprovalResult, AttachResult, AuthParams,
    CancelResult, ChatMessage, Config, ContextCompactParams, ContextCompactedPayload,
    ContextDropParams, ContextDropPayload, ContextLedgerParams, ContextPinParams,
    ContextPinPayload, CreateSessionParams, DraftParams, DraftResult, ErrorPayload, Event,
    EventKind, EventsParams, EventsResult, ForkParams, HelloResult, Notification, PatchApplyParams,
    PatchGetParams, PatchGetResult, PatchProposedPayload, PatchRejectParams, PatchResult,
    PatchResultPayload, PatchReviseParams, PermissionLevel, PermissionParams, PermissionPayload,
    ProcessKillParams, ProcessKillResult, ProcessList, ProcessListParams, ProcessOutputParams,
    RenameParams, Request, Response, RewindParams, RewindResult, SearchParams, SearchResult,
    ServerMessage, Session, SessionId, SessionList, SessionOrigin, SessionRef, SessionResult,
    SessionStatus, SnapshotPayload, StatusChangedPayload, SubmitParams, SubmitResult,
    SubscribeParams, SubscribeResult, ToolCall, ToolCallPayload, ToolDefinition, ToolPolicy,
    ToolResultPayload, UserMessagePayload, WorktreeRemoveParams, WorktreeRemoveResult,
    PROTOCOL_VERSION,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex, RwLock};
use uuid::Uuid;

use crate::context::Ledger;
use crate::process::ProcessRegistry;
use crate::provider::{Provider, ProviderError};
use crate::snapshot::{Scope, Snapshotter};
use crate::staging::{StagedPatch, Staging};
use crate::store::Store;
use crate::tools;
use eix_core::redact::redact;

pub async fn run(config: Config, socket: PathBuf, data_dir: PathBuf) -> Result<()> {
    fs::create_dir_all(&data_dir)
        .with_context(|| format!("cannot create data dir {}", data_dir.display()))?;
    Store::set_permissions(&data_dir)?;

    let db_path = data_dir.join("eix.sqlite3");
    let store = Arc::new(Store::open(&db_path)?);
    Store::set_permissions(&db_path)?;
    let provider = Arc::new(Provider::new(&config).map_err(|e| anyhow::anyhow!("{e}"))?);
    let (bus, _) = broadcast::channel(2048);
    let snapshots = Snapshotter::new(data_dir.join("snapshots"));
    let staging = Staging::new(data_dir.join("staging"));
    let processes = Arc::new(ProcessRegistry::new(
        Arc::clone(&store),
        bus.clone(),
        data_dir.clone(),
    ));
    let daemon = Arc::new(Daemon {
        store,
        provider,
        config: Arc::new(config),
        bus,
        snapshots,
        staging,
        processes,
        turns: Mutex::new(HashMap::new()),
        approvals: Mutex::new(HashMap::new()),
        notified: std::sync::Mutex::new(HashMap::new()),
        temporary_permissions: std::sync::Mutex::new(HashMap::new()),
    });
    let restored = daemon.staging.load();
    if restored > 0 {
        eprintln!("eixd: restored {restored} staged patch(es)");
    }
    daemon.recover().await;

    prepare_socket(&socket)?;
    let listener =
        UnixListener::bind(&socket).with_context(|| format!("cannot bind {}", socket.display()))?;
    Store::set_permissions(&socket)?;

    eprintln!(
        "eixd {} listening on {}",
        env!("CARGO_PKG_VERSION"),
        socket.display()
    );
    eprintln!("  model  {}", daemon.config.provider.model);
    eprintln!("  api    {}", redact(&daemon.config.provider.base_url));
    eprintln!("  data   {}", db_path.display());

    // Optional remote attach. Off unless configured, because a TCP port does
    // not have the Unix socket's filesystem permissions; every connection must
    // authenticate with `server.token` before any other request is answered.
    let tcp_addr = daemon
        .config
        .server
        .tcp
        .as_deref()
        .map(str::trim)
        .filter(|addr| !addr.is_empty())
        .map(str::to_string);
    if let Some(addr) = tcp_addr {
        let token = daemon.config.server.token.clone().unwrap_or_default();
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

struct Daemon {
    store: Arc<Store>,
    provider: Arc<Provider>,
    config: Arc<Config>,
    bus: broadcast::Sender<Notification>,
    /// Pre-execution snapshots for mutating tools.
    snapshots: Snapshotter,
    /// Staged `write_file` proposals, the session shadow workspace.
    staging: Staging,
    /// Background processes started with `start_command`.
    processes: Arc<ProcessRegistry>,
    /// In-flight turns, used for busy checks and cancellation.
    turns: Mutex<HashMap<SessionId, Arc<AtomicBool>>>,
    /// Tool calls blocked on a user decision, keyed by session.
    approvals: Mutex<HashMap<SessionId, PendingApproval>>,
    /// Last attention notification per session, for the cooldown.
    notified: std::sync::Mutex<HashMap<SessionId, Instant>>,
    /// In-memory permission overrides set with `temporary: true`. They last
    /// until the daemon restarts or the level is changed again.
    temporary_permissions: std::sync::Mutex<HashMap<SessionId, PermissionLevel>>,
}

struct PendingApproval {
    approval_id: String,
    tool: String,
    summary: String,
    created_ms: i64,
    responder: oneshot::Sender<ApprovalReply>,
}

#[derive(Debug, Clone)]
struct ApprovalReply {
    decision: ApprovalDecision,
    reason: Option<String>,
    /// Write plan captured when the diff was shown (write_file only).
    plan: Option<tools::WritePlan>,
    /// Hunk indices the user selected; `None` means the whole change.
    hunks: Option<Vec<usize>>,
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

    /// A daemon restart cannot resume an in-flight HTTP stream. Mark those
    /// sessions errored instead of pretending they are still running.
    async fn recover(&self) {
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

    async fn handle(self: &Arc<Self>, request: &Request) -> Response {
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
                model: self.provider.model().to_string(),
                base_url: self.config.provider.base_url.clone(),
                sessions: self.store.session_count()?,
            })?),
            "ping" => Ok(json!({ "pong": true, "ts": now_ms() })),
            "session.list" => {
                let mut sessions = self.store.list_sessions()?;
                self.effective_permissions(&mut sessions);
                Ok(serde_json::to_value(SessionList { sessions })?)
            }
            "session.create" => {
                let params: CreateSessionParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(SessionResult {
                    session: self.create_session(params).await?,
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
                let created = if want_worktree {
                    Some(crate::worktree::create(&source.cwd, &name).await?)
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
            "session.get" => {
                let params: SessionRef = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(SessionResult {
                    session: self.require_session(&params.session_id)?,
                })?)
            }
            "session.attach" => {
                let params: SessionRef = parse_params(request.params.clone())?;
                let session = self.require_session(&params.session_id)?;
                let events = self.store.events(&params.session_id)?;
                Ok(serde_json::to_value(AttachResult { session, events })?)
            }
            "session.events" => {
                let params: EventsParams = parse_params(request.params.clone())?;
                self.require_session(&params.session_id)?;
                Ok(serde_json::to_value(EventsResult {
                    events: self
                        .store
                        .events_after(&params.session_id, params.after_seq)?,
                })?)
            }
            "context.ledger" => {
                let params: ContextLedgerParams = parse_params(request.params.clone())?;
                let ledger = self.build_ledger(&params.session_id).await?;
                Ok(serde_json::to_value(ledger.info())?)
            }
            "context.drop" => {
                let params: ContextDropParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(self.context_drop(params).await?)?)
            }
            "context.pin" => {
                let params: ContextPinParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(self.context_pin(params).await?)?)
            }
            "context.compact" => {
                let params: ContextCompactParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(self.context_compact(params).await?)?)
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
                    });
                }
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
            "patch.apply" => {
                let params: PatchApplyParams = parse_params(request.params.clone())?;
                let result = self
                    .decide_patch(
                        &params.session_id,
                        &params.patch_id,
                        ApprovalDecision::Approve,
                        params.hunks,
                        None,
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
                    &self.config,
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
        } = params;
        {
            let mut approvals = self.approvals.lock().await;
            if let Some(pending) = approvals.remove(&session_id) {
                if !approval_id.is_empty() && pending.approval_id != approval_id {
                    // Stale card: keep waiting for the current approval.
                    approvals.insert(session_id, pending);
                    return Ok(ApprovalResult { accepted: false });
                }
                let _ = pending.responder.send(ApprovalReply {
                    decision,
                    reason,
                    plan: None,
                    hunks,
                });
                return Ok(ApprovalResult { accepted: true });
            }
        }
        // No blocking tool call: the id must belong to a staged patch.
        let result = self
            .decide_patch(&session_id, &approval_id, decision, hunks, reason)
            .await?;
        Ok(ApprovalResult {
            accepted: result.accepted,
        })
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
        if params.worktree.unwrap_or(false) {
            let created = crate::worktree::create(&session.cwd, &session.name).await?;
            session.cwd = created.path.clone();
            session.worktree = Some(created.path);
        }
        self.store.create_session(&session)?;
        self.emit_session(&session);
        Ok(session)
    }

    /// The level that actually applies right now: a temporary in-memory
    /// override wins over the persisted session row.
    fn session_permission(&self, session: &Session) -> PermissionLevel {
        self.temporary_permissions
            .lock()
            .ok()
            .and_then(|overrides| overrides.get(&session.id).copied())
            .unwrap_or(session.permission)
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

        if !self.config.tools.snapshots {
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
        let session = self.require_session(&params.session_id)?;
        if session.status.is_busy() {
            return Err(rpc(
                "busy",
                format!("session {} is {}", session.id, session.status.as_str()),
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
        let text = params.text.trim();
        if text.is_empty() {
            return Err(rpc("invalid_params", "prompt must not be empty"));
        }

        let cancel = {
            let mut turns = self.turns.lock().await;
            if turns.contains_key(&params.session_id) {
                return Err(rpc(
                    "busy",
                    format!("session {} is already running", params.session_id),
                ));
            }
            let flag = Arc::new(AtomicBool::new(false));
            turns.insert(params.session_id.clone(), Arc::clone(&flag));
            flag
        };

        let session_id = params.session_id;
        let setup = self
            .submit_setup(session_id.clone(), text.to_string())
            .await;
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
                "model": session.model.clone().unwrap_or_else(|| self.provider.model().to_string()),
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

    async fn submit_setup(&self, session_id: SessionId, text: String) -> Result<(Session, String)> {
        let session = self.require_session(&session_id)?;
        if session.status.is_busy() {
            return Err(rpc(
                "busy",
                format!("session {} is {}", session.id, session.status.as_str()),
            ));
        }

        // First prompt names an unnamed session; keep it short and one-line.
        let mut session = session;
        if needs_auto_name(&session) {
            let name = auto_name(&text);
            if let Some(renamed) = self.store.rename_session(&session.id, &name)? {
                session = renamed;
                self.emit_session(&session);
            }
        }

        let event = self.store.append_event(
            &session.id,
            EventKind::UserMessage,
            payload(&UserMessagePayload { text }),
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
        let Some(command) = self
            .config
            .notifications
            .command
            .as_deref()
            .map(str::trim)
            .filter(|command| !command.is_empty())
            .map(str::to_string)
        else {
            return;
        };
        let cooldown = Duration::from_secs(self.config.notifications.cooldown_secs);
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
        let definitions = match self.require_session(&session_id) {
            Ok(session) if self.session_permission(&session) == PermissionLevel::ReadOnly => {
                tools::definitions()
                    .into_iter()
                    .filter(|tool| !tools::requires_approval(&tool.function.name))
                    .collect()
            }
            _ => tools::definitions(),
        };
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
    }

    async fn run_turn_inner(
        self: &Arc<Self>,
        session_id: &str,
        turn_id: &str,
        cancel: &AtomicBool,
        allowed_tools: &[ToolDefinition],
    ) -> Result<()> {
        let session = self.require_session(session_id)?;
        let model = session
            .model
            .clone()
            .unwrap_or_else(|| self.provider.model().to_string());
        let ledger = self.build_ledger(session_id).await?;
        let mut messages = ledger.messages();
        self.emit_context(session_id, &ledger);
        let tools = allowed_tools;

        for round in 0..=tools::MAX_TOOL_ROUNDS {
            if cancel.load(Ordering::SeqCst) {
                self.finish_cancelled(session_id, turn_id, "", &model)?;
                return Ok(());
            }

            let bus = self.bus.clone();
            let sid = session_id.to_string();
            let tid = turn_id.to_string();
            let mut streamed = String::new();
            let outcome = self
                .provider
                .stream_chat(&messages, &model, tools, cancel, |delta| {
                    streamed.push_str(delta);
                    let _ = bus.send(Notification::new(
                        "message.delta",
                        json!({
                            "session_id": sid,
                            "turn_id": tid,
                            "delta": delta,
                        }),
                    ));
                })
                .await;

            if cancel.load(Ordering::SeqCst) {
                self.finish_cancelled(session_id, turn_id, &streamed, &model)?;
                return Ok(());
            }

            let completion = match outcome {
                Ok(completion) => completion,
                Err(ProviderError::Cancelled) => {
                    self.finish_cancelled(session_id, turn_id, &streamed, &model)?;
                    return Ok(());
                }
                Err(err) => return Err(anyhow::anyhow!("{err}")),
            };

            if completion.tool_calls.is_empty() {
                if completion.text.trim().is_empty() {
                    return Err(anyhow::anyhow!(
                        "model returned an empty response (finish_reason: {:?})",
                        completion.finish_reason
                    ));
                }
                let event = self.store.append_event(
                    session_id,
                    EventKind::AssistantMessage,
                    payload(&eix_core::AssistantMessagePayload {
                        text: completion.text,
                        model: completion.model,
                        cancelled: false,
                        usage: completion.usage,
                        tool_calls: Vec::new(),
                    }),
                )?;
                self.emit_event(&event);
                self.transition(session_id, SessionStatus::Idle)?;
                self.emit_value(
                    "turn.completed",
                    json!({
                        "session_id": session_id,
                        "turn_id": turn_id,
                        "cancelled": false,
                        "error": Value::Null,
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
                self.finish_cancelled(session_id, turn_id, "", &model)?;
                return Ok(());
            }

            if round == tools::MAX_TOOL_ROUNDS {
                bail!(
                    "tool loop exceeded {} rounds without a final answer",
                    tools::MAX_TOOL_ROUNDS
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
        cancel: &AtomicBool,
    ) -> Result<()> {
        let mut agents: Vec<(String, tokio::task::JoinHandle<Result<String>>)> = Vec::new();
        for call in calls {
            if call.function.name == "spawn_agent" {
                let daemon = Arc::clone(self);
                let call = call.clone();
                let call_id = call.id.clone();
                let sid = session_id.to_string();
                let tid = turn_id.to_string();
                let handle =
                    tokio::spawn(async move { daemon.spawn_agent(&sid, &tid, &call).await });
                agents.push((call_id, handle));
            } else {
                self.run_tool_call(session_id, turn_id, call, messages, cancel)
                    .await?;
            }
            if cancel.load(Ordering::SeqCst) {
                break;
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
    ) -> futures_util::future::BoxFuture<'a, Result<String>> {
        Box::pin(async move { self.spawn_agent_outer(parent_id, parent_turn, call).await })
    }

    async fn spawn_agent_outer(
        self: &Arc<Self>,
        parent_id: &str,
        parent_turn: &str,
        call: &ToolCall,
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
        let result = self.spawn_agent_inner(&parent, parent_turn, &args).await;
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
        args: &Value,
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

        let name = self.agent_name(&parent.name, label);
        let mut child = Session::new(Uuid::new_v4().to_string(), name.clone(), parent.cwd.clone());
        child.model = parent.model.clone();
        child.parent_id = Some(parent.id.clone());
        child.origin = SessionOrigin::Agent;
        self.store.create_session(&child)?;
        self.emit_session(&child);

        let cancel = Arc::new(AtomicBool::new(false));
        self.turns
            .lock()
            .await
            .insert(child.id.clone(), Arc::clone(&cancel));
        self.emit_value(
            "agent.spawned",
            json!({
                "session_id": parent.id,
                "agent_id": child.id,
                "name": child.name,
                "parent_turn_id": parent_turn,
            }),
        );

        let run = self.run_child_turn(&child, prompt).await;
        self.turns.lock().await.remove(&child.id);

        let text = run?;
        let trimmed = text.trim();
        let body = if trimmed.is_empty() {
            "(sub-agent returned no text)".to_string()
        } else {
            cap_text(trimmed, self.config.tools.max_output_bytes)
        };
        self.emit_value(
            "agent.completed",
            json!({
                "session_id": parent.id,
                "agent_id": child.id,
                "name": child.name,
                "ok": true,
            }),
        );
        Ok(format!(
            "sub-agent {} ({}) finished:\n{}",
            child.name,
            short_id(&child.id),
            body
        ))
    }

    /// Run one bounded, read-only turn for a sub-agent and return its answer.
    async fn run_child_turn(self: &Arc<Self>, child: &Session, prompt: &str) -> Result<String> {
        let event = self.store.append_event(
            &child.id,
            EventKind::UserMessage,
            payload(&UserMessagePayload {
                text: prompt.to_string(),
            }),
        )?;
        self.emit_event(&event);
        self.transition(&child.id, SessionStatus::Running)?;

        let turn_id = Uuid::new_v4().to_string();
        let cancel = AtomicBool::new(false);
        let readonly = tools::readonly_definitions();
        self.emit_value(
            "turn.started",
            json!({
                "session_id": child.id,
                "turn_id": turn_id,
                "model": child
                    .model
                    .clone()
                    .unwrap_or_else(|| self.provider.model().to_string()),
            }),
        );
        if let Err(err) = self
            .run_turn_inner(&child.id, &turn_id, &cancel, &readonly)
            .await
        {
            self.fail_turn(&child.id, &turn_id, &format!("{err:#}"));
            return Err(err);
        }

        let events = self.store.events(&child.id)?;
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

        if !tools::is_known(tool) {
            let output = format!("unknown tool {tool:?}");
            self.record_tool_result(session_id, call, false, false, &output, 0)?;
            messages.push(ChatMessage::tool(call.id.clone(), output));
            return Ok(());
        }

        // Sub-agents inherit the parent's context, not its write access. Even
        // if the model ignores the read-only tool list, mutating tools are
        // refused here so a child can never change the shared workspace.
        let session = self.require_session(session_id)?;
        let permission = self.session_permission(&session);
        if session.origin == SessionOrigin::Agent && tools::requires_approval(tool) {
            let output = "sub-agents are read-only; mutating tools are not available".to_string();
            self.record_tool_result(session_id, call, false, true, &output, 0)?;
            messages.push(ChatMessage::tool(call.id.clone(), output));
            return Ok(());
        }

        // The session-level level can only narrow what the global policy
        // allows. `read-only` sessions never stage, ask, or auto-run a write.
        if permission == PermissionLevel::ReadOnly && tools::requires_approval(tool) {
            let output = "session permission is read-only; mutating tools are disabled".to_string();
            self.record_tool_result(session_id, call, false, true, &output, 0)?;
            messages.push(ChatMessage::tool(call.id.clone(), output));
            return Ok(());
        }

        // Rules only relax the default `ask` policy. `auto` needs no help,
        // `read-only` must stay a hard cap, and `full` sessions already skip
        // the approval prompt.
        let auto_approve = self.config.tools.policy == ToolPolicy::Ask
            && permission != PermissionLevel::Full
            && self.config.tools.auto_approval(tool, &args).is_some();

        // Staged writes: under `ask` (the default) a `write_file` proposal goes
        // into the session's shadow workspace and returns at once. The user
        // applies or rejects it later with `patch.apply` / `patch.reject`.
        // Reads and later writes see the staged contents. `full` sessions and
        // auto-approved calls skip the review step and write directly.
        if tool == "write_file"
            && self.config.tools.policy == ToolPolicy::Ask
            && permission != PermissionLevel::Full
            && !auto_approve
        {
            return self
                .stage_write(session_id, turn_id, call, &args, messages)
                .await;
        }

        let mut denial: Option<String> = None;
        let mut plan: Option<tools::WritePlan> = None;
        let mut selected_hunks: Option<Vec<usize>> = None;
        if tools::requires_approval(tool) {
            match self.config.tools.policy {
                ToolPolicy::Auto => {}
                ToolPolicy::ReadOnly => {
                    denial =
                        Some("tool policy is read-only; mutating tools are disabled".to_string())
                }
                ToolPolicy::Ask if permission == PermissionLevel::Full => {}
                ToolPolicy::Ask if auto_approve => {}
                ToolPolicy::Ask => {
                    // `write_file` returned above; only blocking tools land here.
                    let reply = self
                        .wait_for_approval(session_id, turn_id, call, tool, &args, cancel)
                        .await?;
                    selected_hunks = reply.hunks;
                    plan = reply.plan;
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
            }
        }

        let denied = denial.is_some();
        let snapshot_warning = if denied {
            None
        } else {
            self.capture_snapshot(session_id, tool, call, &args, plan.as_ref())
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
                    None => {
                        let session = self.require_session(session_id)?;
                        match self
                            .read_with_staging(session_id, tool, &args, &session.cwd)
                            .await
                        {
                            Some(outcome) => outcome,
                            None => tools::execute(tool, &args, &session.cwd, &self.config).await,
                        }
                    }
                };
                (outcome.ok, outcome.output, outcome.duration_ms)
            }
        };
        if let Some(warning) = snapshot_warning {
            output.push_str(&format!("\n[snapshot failed: {warning}]"));
        }
        if ok && tool == "write_file" {
            // Direct writes bypass the review step: `auto`, `full`, or a
            // matching `[[tools.auto_approve]]` rule.
            let reason = if auto_approve {
                "auto-approval rule"
            } else if permission == PermissionLevel::Full {
                "session permission full"
            } else {
                "tool policy auto"
            };
            self.record_auto_patch(session_id, call, &args, reason)?;
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

    /// Stage a `write_file` proposal in the session shadow workspace and return
    /// immediately. The turn is not blocked: the user applies or rejects the
    /// patch later, and reads see the staged bytes in the meantime.
    async fn stage_write(
        &self,
        session_id: &str,
        turn_id: &str,
        call: &ToolCall,
        args: &Value,
        messages: &mut Vec<ChatMessage>,
    ) -> Result<()> {
        let session = self.require_session(session_id)?;
        let cwd = session.cwd.clone();
        let Some(path) = tools::write_target(args, &cwd) else {
            let output = "write_file needs a path inside the workspace".to_string();
            self.record_tool_result(session_id, call, false, false, &output, 0)?;
            self.emit_value(
                "tool.completed",
                json!({
                    "session_id": session_id,
                    "turn_id": turn_id,
                    "call_id": call.id,
                    "tool": "write_file",
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
            "write_file",
            args,
            &cwd,
            &self.config,
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
                        "tool": "write_file",
                        "ok": false,
                        "denied": false,
                        "duration_ms": 0,
                    }),
                );
                messages.push(ChatMessage::tool(call.id.clone(), output));
                return Ok(());
            }
        };
        let summary = tools::summarize("write_file", args);
        let hunks = plan.hunk_infos();
        let patch = StagedPatch::new(
            session_id,
            &call.id,
            "write_file",
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
                tool: "write_file".to_string(),
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
                "tool": "write_file",
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
                "tool": "write_file",
                "ok": true,
                "denied": false,
                "duration_ms": 0,
            }),
        );
        messages.push(ChatMessage::tool(call.id.clone(), output));
        Ok(())
    }

    /// Flag a session with a pending patch so background panes and external
    /// notifications can surface it. The stored status stays `running`.
    /// Rebuild the context ledger for a session. Used by reads, by the
    /// `context.*` requests, and at the start of every turn.
    async fn build_ledger(&self, session_id: &str) -> Result<Ledger> {
        let session = self.require_session(session_id)?;
        let events = self.store.events(session_id)?;
        let permission = self.session_permission(&session);
        Ok(Ledger::build(
            &self.config,
            system_prompt(&self.config, &session, permission),
            &session.cwd,
            &events,
        )
        .await)
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

    async fn context_drop(
        &self,
        params: ContextDropParams,
    ) -> Result<eix_core::ContextLedgerResult> {
        let ledger = self.build_ledger(&params.session_id).await?;
        if !ledger.has_entry(&params.entry) {
            return Err(rpc(
                "not_found",
                format!("unknown context entry {:?}", params.entry),
            ));
        }
        if !ledger.is_editable(&params.entry) {
            return Err(rpc(
                "invalid_params",
                format!("context entry {:?} cannot be edited", params.entry),
            ));
        }
        let dropped = params.dropped.unwrap_or(true);
        let event = self.store.append_event(
            &params.session_id,
            EventKind::ContextDropped,
            payload(&ContextDropPayload {
                entry: params.entry,
                dropped,
            }),
        )?;
        self.emit_event(&event);
        let ledger = self.build_ledger(&params.session_id).await?;
        self.emit_context(&params.session_id, &ledger);
        Ok(ledger.info())
    }

    async fn context_pin(&self, params: ContextPinParams) -> Result<eix_core::ContextLedgerResult> {
        let ledger = self.build_ledger(&params.session_id).await?;
        if !ledger.has_entry(&params.entry) {
            return Err(rpc(
                "not_found",
                format!("unknown context entry {:?}", params.entry),
            ));
        }
        if !ledger.is_editable(&params.entry) {
            return Err(rpc(
                "invalid_params",
                format!("context entry {:?} cannot be pinned", params.entry),
            ));
        }
        let event = self.store.append_event(
            &params.session_id,
            EventKind::ContextPinned,
            payload(&ContextPinPayload {
                entry: params.entry,
                pinned: params.pinned,
            }),
        )?;
        self.emit_event(&event);
        let ledger = self.build_ledger(&params.session_id).await?;
        self.emit_context(&params.session_id, &ledger);
        Ok(ledger.info())
    }

    async fn context_compact(
        &self,
        params: ContextCompactParams,
    ) -> Result<eix_core::ContextLedgerResult> {
        if params.from_seq == 0 || params.to_seq < params.from_seq {
            return Err(rpc(
                "invalid_params",
                "context.compact needs 0 < from_seq <= to_seq",
            ));
        }
        let ledger = self.build_ledger(&params.session_id).await?;
        if ledger.overlaps_compaction(params.from_seq, params.to_seq) {
            return Err(rpc(
                "invalid_params",
                "compaction range overlaps an existing summary",
            ));
        }
        let (messages, tokens) = ledger.range_stats(params.from_seq, params.to_seq);
        if messages == 0 {
            return Err(rpc("invalid_params", "no messages in the requested range"));
        }
        let summary = params
            .summary
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty())
            .unwrap_or_else(|| {
                "(the user compacted these messages; their text remains in the event log, \
                 not in this request)"
                    .to_string()
            });
        let event = self.store.append_event(
            &params.session_id,
            EventKind::ContextCompacted,
            payload(&ContextCompactedPayload {
                from_seq: params.from_seq,
                to_seq: params.to_seq,
                messages,
                tokens,
                summary,
            }),
        )?;
        self.emit_event(&event);
        self.emit_value(
            "context.compacted",
            json!({
                "session_id": params.session_id,
                "from_seq": params.from_seq,
                "to_seq": params.to_seq,
                "messages": messages,
                "tokens": tokens,
            }),
        );
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

    /// Record a write that was committed to disk without review (`auto`
    /// policy, `full` permission, or an `auto_approve` rule).
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
                let path = tools::write_target(args, cwd)?;
                let content = self.staging.latest_content(session_id, &path)?;
                Some(tools::ToolOutcome {
                    ok: true,
                    output: tools::cap_output(content, self.config.tools.max_output_bytes),
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
        let dir = tools::resolve_path(cwd, raw).ok()?;
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
            output: tools::cap_output(text, self.config.tools.max_output_bytes),
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
            &self.config,
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
        let (snapshot_id, snapshot_warning) = if self.config.tools.snapshots {
            let result = self
                .snapshots
                .capture_file(session_id, "write_file", &patch.call_id, &cwd, &patch.path)
                .await;
            self.record_snapshot(
                session_id,
                "write_file",
                &patch.call_id,
                Scope::File,
                result,
            )
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
        let message = match snapshot_warning {
            Some(warning) => format!("{}\n[snapshot failed: {warning}]", outcome.output),
            None => outcome.output.clone(),
        };
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
        let plan = match tools::command_plan(command, cwd, &session.cwd, &self.config) {
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

    /// Copy what a mutating tool is about to change and record the result.
    /// A failed snapshot never blocks the call; the tool output gets a short
    /// note so the lost rollback safety is visible.
    async fn capture_snapshot(
        &self,
        session_id: &str,
        tool: &str,
        call: &ToolCall,
        args: &Value,
        plan: Option<&tools::WritePlan>,
    ) -> Result<Option<String>> {
        if !tools::requires_approval(tool) || !self.config.tools.snapshots {
            return Ok(None);
        }
        let cwd = self.require_session(session_id)?.cwd;
        let scope = if tool == "write_file" {
            Scope::File
        } else {
            Scope::Tree
        };
        let result = match tool {
            "write_file" => {
                let path = plan
                    .map(|plan| plan.path().to_path_buf())
                    .or_else(|| tools::write_target(args, &cwd));
                match path {
                    Some(path) => {
                        self.snapshots
                            .capture_file(session_id, tool, &call.id, &cwd, &path)
                            .await
                    }
                    None => Err(anyhow::anyhow!("cannot resolve the write target")),
                }
            }
            "run_command" => {
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

    /// Publish an approval request and block until the user answers or the
    /// turn is cancelled.
    async fn wait_for_approval(
        &self,
        session_id: &str,
        turn_id: &str,
        call: &ToolCall,
        tool: &str,
        args: &Value,
        cancel: &AtomicBool,
    ) -> Result<ApprovalReply> {
        if cancel.load(Ordering::SeqCst) {
            return Ok(ApprovalReply {
                decision: ApprovalDecision::Deny,
                reason: Some("turn cancelled".to_string()),
                plan: None,
                hunks: None,
            });
        }

        let approval_id = Uuid::new_v4().to_string();
        let summary = tools::summarize(tool, args);
        let cwd = self.require_session(session_id)?.cwd;
        let plan = tools::write_plan(tool, args, &cwd, &self.config).await;
        let diff = plan.as_ref().map(|plan| plan.diff().to_string());
        let hunks = plan
            .as_ref()
            .map(|plan| plan.hunk_infos())
            .unwrap_or_default();
        let (responder, receiver) = oneshot::channel();
        self.approvals.lock().await.insert(
            session_id.to_string(),
            PendingApproval {
                approval_id: approval_id.clone(),
                tool: tool.to_string(),
                summary: summary.clone(),
                created_ms: now_ms(),
                responder,
            },
        );

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
            }),
        );
        self.transition(session_id, SessionStatus::AwaitingApproval)?;

        let mut reply = tokio::select! {
            reply = receiver => reply.unwrap_or(ApprovalReply {
                decision: ApprovalDecision::Deny,
                reason: Some("approval channel closed".to_string()),
                plan: None,
                hunks: None,
            }),
            _ = wait_for_cancel(cancel) => ApprovalReply {
                decision: ApprovalDecision::Deny,
                reason: Some("turn cancelled".to_string()),
                plan: None,
                hunks: None,
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
        model: &str,
    ) -> Result<()> {
        if !streamed.trim().is_empty() {
            let event = self.store.append_event(
                session_id,
                EventKind::AssistantMessage,
                payload(&eix_core::AssistantMessagePayload {
                    text: streamed.to_string(),
                    model: model.to_string(),
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
fn secret_eq(a: &str, b: &str) -> bool {
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

fn default_name(cwd: &Path) -> String {
    cwd.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("session")
        .to_string()
}

fn needs_auto_name(session: &Session) -> bool {
    let name = session.name.trim();
    name.is_empty() || name == "session" || name == default_name(&session.cwd)
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

fn system_prompt(config: &Config, session: &Session, permission: PermissionLevel) -> String {
    if let Some(prompt) = &config.provider.system_prompt {
        return prompt.clone();
    }
    let permission = match permission {
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
         Do not claim to have run commands or changed files unless you actually did.{permission}",
        session.cwd.display()
    )
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
    fn default_names_fall_back_to_session() {
        assert_eq!(default_name(Path::new("/")), "session");
        assert_eq!(default_name(Path::new("/home/me/eix")), "eix");
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
}
