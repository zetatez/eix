//! Unix-socket JSON-RPC server and the agent turn loop.
//!
//! A turn is: append the user event -> mark running -> stream provider deltas ->
//! append the assistant event -> mark idle. All state changes are written to
//! SQLite before they are broadcast, so a frontend can always recover by
//! re-attaching and replaying the event log.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use eix_core::{
    now_ms, payload, ApprovalDecision, ApprovalRequiredPayload, ApprovalRespondParams,
    ApprovalResult, AttachResult, CancelResult, ChatMessage, Config, CreateSessionParams,
    ErrorPayload, Event, EventKind, EventsParams, EventsResult, HelloResult, Notification,
    RenameParams, Request, Response, ServerMessage, Session, SessionId, SessionList, SessionRef,
    SessionResult, SessionStatus, StatusChangedPayload, SubmitParams, SubmitResult, ToolCall,
    ToolCallPayload, ToolPolicy, ToolResultPayload, UserMessagePayload, PROTOCOL_VERSION,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, WriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};
use uuid::Uuid;

use crate::provider::{Provider, ProviderError};
use crate::store::Store;
use crate::tools;

pub async fn run(config: Config, socket: PathBuf, data_dir: PathBuf) -> Result<()> {
    fs::create_dir_all(&data_dir)
        .with_context(|| format!("cannot create data dir {}", data_dir.display()))?;
    Store::set_permissions(&data_dir)?;

    let db_path = data_dir.join("eix.sqlite3");
    let store = Arc::new(Store::open(&db_path)?);
    Store::set_permissions(&db_path)?;
    let provider = Arc::new(Provider::new(&config).map_err(|e| anyhow::anyhow!("{e}"))?);
    let (bus, _) = broadcast::channel(2048);
    let daemon = Arc::new(Daemon {
        store,
        provider,
        config: Arc::new(config),
        bus,
        turns: Mutex::new(HashMap::new()),
        approvals: Mutex::new(HashMap::new()),
        notified: std::sync::Mutex::new(HashMap::new()),
    });
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
    eprintln!("  api    {}", daemon.config.provider.base_url);
    eprintln!("  data   {}", db_path.display());

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => break,
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accept failed")?;
                let daemon = Arc::clone(&daemon);
                tokio::spawn(async move {
                    daemon.serve(stream).await;
                });
            }
        }
    }

    let _ = fs::remove_file(&socket);
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
    /// In-flight turns, used for busy checks and cancellation.
    turns: Mutex<HashMap<SessionId, Arc<AtomicBool>>>,
    /// Tool calls blocked on a user decision, keyed by session.
    approvals: Mutex<HashMap<SessionId, PendingApproval>>,
    /// Last attention notification per session, for the cooldown.
    notified: std::sync::Mutex<HashMap<SessionId, Instant>>,
}

struct PendingApproval {
    approval_id: String,
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
                eprintln!("eixd: cannot scan running sessions: {err:#}");
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
                eprintln!("eixd: cannot recover {}: {err:#}", session.id);
            }
        }
    }

    async fn serve(self: Arc<Self>, stream: UnixStream) {
        let (read, write) = tokio::io::split(stream);
        let (tx, rx) = mpsc::unbounded_channel::<ServerMessage>();
        let writer = tokio::spawn(write_loop(write, rx));

        let mut bus_rx = self.bus.subscribe();
        let bus_tx = tx.clone();
        // Phase 0 broadcasts every event to every connection; the 0600 socket
        // is the security boundary. Per-connection session filtering belongs
        // with remote attach, where the boundary is no longer the filesystem.
        let bus_task = tokio::spawn(async move {
            loop {
                match bus_rx.recv().await {
                    Ok(notification) => {
                        if bus_tx
                            .send(ServerMessage::Notification(notification))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        let note =
                            Notification::new("stream.lagged", json!({ "skipped": skipped }));
                        if bus_tx.send(ServerMessage::Notification(note)).is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let mut lines = BufReader::new(read).lines();
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
            let response = self.handle(&request).await;
            if tx.send(ServerMessage::Response(response)).is_err() {
                break;
            }
        }

        bus_task.abort();
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
            "session.list" => Ok(serde_json::to_value(SessionList {
                sessions: self.store.list_sessions()?,
            })?),
            "session.create" => {
                let params: CreateSessionParams = parse_params(request.params.clone())?;
                Ok(serde_json::to_value(SessionResult {
                    session: self.create_session(params)?,
                })?)
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
            "approval.respond" => {
                let params: ApprovalRespondParams = parse_params(request.params.clone())?;
                let result = self.respond_approval(params).await?;
                Ok(serde_json::to_value(result)?)
            }
            other => Err(rpc("method_not_found", format!("unknown method {other:?}"))),
        }
    }

    async fn respond_approval(&self, params: ApprovalRespondParams) -> Result<ApprovalResult> {
        let mut approvals = self.approvals.lock().await;
        let Some(pending) = approvals.remove(&params.session_id) else {
            return Ok(ApprovalResult { accepted: false });
        };
        if !params.approval_id.is_empty() && pending.approval_id != params.approval_id {
            // Stale card: keep waiting for the current approval.
            approvals.insert(params.session_id, pending);
            return Ok(ApprovalResult { accepted: false });
        }
        let _ = pending.responder.send(ApprovalReply {
            decision: params.decision,
            reason: params.reason,
            plan: None,
            hunks: params.hunks,
        });
        Ok(ApprovalResult { accepted: true })
    }

    fn create_session(&self, params: CreateSessionParams) -> Result<Session> {
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
        self.store.create_session(&session)?;
        self.emit_session(&session);
        Ok(session)
    }

    fn require_session(&self, id: &str) -> Result<Session> {
        self.store
            .get_session(id)?
            .ok_or_else(|| rpc("not_found", format!("unknown session {id}")))
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
                eprintln!("eixd: notification command failed: {err}");
            }
        });
    }

    async fn run_turn(
        self: Arc<Self>,
        session_id: SessionId,
        turn_id: String,
        cancel: Arc<AtomicBool>,
    ) {
        if let Err(err) = self.run_turn_inner(&session_id, &turn_id, &cancel).await {
            eprintln!("eixd: turn {turn_id} failed: {err:#}");
            let message = format!("{err:#}");
            self.fail_turn(&session_id, &turn_id, &message);
        }
        self.approvals.lock().await.remove(&session_id);
        self.turns.lock().await.remove(&session_id);
    }

    async fn run_turn_inner(
        &self,
        session_id: &str,
        turn_id: &str,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let session = self.require_session(session_id)?;
        let model = session
            .model
            .clone()
            .unwrap_or_else(|| self.provider.model().to_string());
        let mut messages = Vec::new();
        messages.push(ChatMessage::system(system_prompt(&self.config, &session)));
        messages.extend(self.store.messages(session_id)?);
        let tools = tools::definitions();

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
                .stream_chat(&messages, &model, &tools, cancel, |delta| {
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

            for call in &completion.tool_calls {
                self.run_tool_call(session_id, turn_id, call, &mut messages, cancel)
                    .await?;
                if cancel.load(Ordering::SeqCst) {
                    self.finish_cancelled(session_id, turn_id, "", &model)?;
                    return Ok(());
                }
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
                ToolPolicy::Ask => {
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
        let (ok, output, duration_ms) = match denial {
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
                    None => {
                        let session = self.require_session(session_id)?;
                        tools::execute(tool, &args, &session.cwd, &self.config).await
                    }
                };
                (outcome.ok, outcome.output, outcome.duration_ms)
            }
        };

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
            eprintln!("eixd: cannot mark {session_id} as errored: {err:#}");
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

async fn write_loop(
    mut write: WriteHalf<UnixStream>,
    mut rx: mpsc::UnboundedReceiver<ServerMessage>,
) {
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

fn system_prompt(config: &Config, session: &Session) -> String {
    if let Some(prompt) = &config.provider.system_prompt {
        return prompt.clone();
    }
    format!(
        "You are eix, a coding agent in the user's terminal. Work inside `{}`.\n\
         Be concise, concrete, and honest. If you are unsure, inspect the workspace or ask.\n\
         Do not claim to have run commands or changed files unless you actually did.",
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
