//! Agent Client Protocol (ACP) adapter.
//!
//! Editors such as Zed launch `eix acp` and speak JSON-RPC over stdio. The
//! adapter is a thin bridge to the daemon: ACP sessions map one-to-one to eix
//! sessions, prompt turns stream `message.delta` / `reasoning.delta`, and tool
//! approvals become `session/request_permission` calls in the editor. All
//! existing eix features (session persistence, worktrees, MCP, approvals) stay
//! in the daemon.
//!
//! The adapter deliberately keeps editor-specific behaviour out of the daemon
//! protocol: it translates in both directions and advertises only the
//! capabilities it implements (text prompts, embedded text context, load).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use eix_core::{
    ApprovalDecision, ApprovalScope, AssistantMessagePayload, AttachResult, Endpoint, Event,
    EventKind, Session, SessionMode, SessionResult, SubmitResult, ToolCallPayload,
    ToolResultPayload, UserMessagePayload,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::client::ensure_endpoint;
use crate::tui::net::{NetEvent, Requester};

const ACP_VERSION: u64 = 1;

pub async fn run(endpoint: Endpoint, data_dir: PathBuf, config: Option<PathBuf>) -> Result<()> {
    ensure_endpoint(&endpoint, &data_dir, config.as_deref()).await?;
    let (requester, mut daemon_events) = Requester::connect(&endpoint).await?;
    let (out, mut out_rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(message) = out_rx.recv().await {
            let Ok(mut line) = serde_json::to_vec(&message) else {
                continue;
            };
            line.push(b'\n');
            if stdout.write_all(&line).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });

    let mut bridge = Bridge {
        requester,
        out,
        next_id: 1,
        sessions: HashMap::new(),
        active: HashMap::new(),
        pending: HashMap::new(),
        announced: HashSet::new(),
        tool_args: HashMap::new(),
        tool_outputs: HashMap::new(),
    };
    let mut stdin = BufReader::new(tokio::io::stdin()).lines();
    loop {
        tokio::select! {
            line = stdin.next_line() => {
                match line {
                    Ok(Some(line)) if !line.trim().is_empty() => {
                        match serde_json::from_str::<Value>(&line) {
                            Ok(message) => {
                                if let Err(err) = bridge.handle_client(message).await {
                                    eprintln!("eix acp: {err:#}");
                                }
                            }
                            Err(err) => eprintln!("eix acp: invalid JSON-RPC message: {err}"),
                        }
                    }
                    Ok(Some(_)) => {}
                    Ok(None) | Err(_) => break,
                }
            }
            event = daemon_events.recv() => {
                match event {
                    Some(NetEvent::Notification(notification)) => {
                        if let Err(err) = bridge.handle_daemon(notification).await {
                            eprintln!("eix acp: {err:#}");
                        }
                    }
                    Some(NetEvent::Closed(reason)) => {
                        eprintln!("eix acp: daemon connection closed: {reason}");
                        break;
                    }
                    None => break,
                }
            }
        }
    }
    for session_id in bridge.active.keys().cloned().collect::<Vec<_>>() {
        let _ = bridge
            .requester
            .request("prompt.cancel", json!({ "session_id": session_id }))
            .await;
    }
    drop(bridge.out);
    let _ = writer.await;
    Ok(())
}

struct ActiveTurn {
    request_id: Value,
    turn_id: String,
    streamed_message: bool,
    cancelled: bool,
}

enum Pending {
    Approval {
        session_id: String,
        approval_id: String,
    },
    Input {
        session_id: String,
        input_id: String,
        options: HashMap<String, usize>,
    },
}

struct Bridge {
    requester: Requester,
    out: mpsc::UnboundedSender<Value>,
    next_id: u64,
    /// ACP session id (same as the daemon id) to its working directory.
    sessions: HashMap<String, PathBuf>,
    active: HashMap<String, ActiveTurn>,
    pending: HashMap<u64, Pending>,
    announced: HashSet<(String, String)>,
    tool_args: HashMap<(String, String), Value>,
    tool_outputs: HashMap<(String, String), (bool, bool, String)>,
}

impl Bridge {
    async fn handle_client(&mut self, message: Value) -> Result<()> {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let id = message.get("id").cloned();
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        match (method, id) {
            (Some(method), Some(id)) => self.handle_request(id, &method, params).await,
            (Some(method), None) => self.handle_notification(&method, params).await,
            (None, Some(id)) => self.handle_response(id, &message).await,
            _ => Ok(()),
        }
    }

    async fn handle_request(&mut self, id: Value, method: &str, params: Value) -> Result<()> {
        match method {
            "initialize" => {
                self.respond(
                    id,
                    json!({
                        "protocolVersion": ACP_VERSION,
                        "agentCapabilities": {
                            "loadSession": true,
                            "promptCapabilities": {
                                "image": false,
                                "audio": false,
                                "embeddedContext": true
                            },
                            "mcpCapabilities": {"http": false, "sse": false},
                            "sessionCapabilities": {"close": {}}
                        },
                        "agentInfo": {
                            "name": "eix",
                            "title": "eix",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "authMethods": []
                    }),
                );
                Ok(())
            }
            "authenticate" => {
                self.respond(id, json!({}));
                Ok(())
            }
            "session/new" => self.new_session(id, params).await,
            "session/load" => self.load_session(id, params).await,
            "session/prompt" => self.prompt(id, params).await,
            "session/set_mode" => self.set_mode(id, params).await,
            "session/close" => self.close_session(id, params).await,
            "session/list" => {
                // Minimal, stable listing: only sessions created in this
                // adapter process. Editors use it for "resume recent".
                let sessions: Vec<Value> = self
                    .sessions
                    .iter()
                    .map(|(id, cwd)| json!({ "sessionId": id, "cwd": cwd, "title": id }))
                    .collect();
                self.respond(id, json!({ "sessions": sessions }));
                Ok(())
            }
            other => {
                self.respond_error(id, -32601, format!("method not found: {other}"));
                Ok(())
            }
        }
    }

    async fn handle_notification(&mut self, method: &str, params: Value) -> Result<()> {
        if method == "session/cancel" {
            let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
                return Ok(());
            };
            if let Some(turn) = self.active.get_mut(session_id) {
                turn.cancelled = true;
            }
            let _ = self
                .requester
                .request("prompt.cancel", json!({ "session_id": session_id }))
                .await;
            self.deny_pending(session_id).await;
        }
        Ok(())
    }

    async fn handle_response(&mut self, id: Value, message: &Value) -> Result<()> {
        let Some(request_id) = id.as_u64() else {
            return Ok(());
        };
        let Some(pending) = self.pending.remove(&request_id) else {
            return Ok(());
        };
        let outcome = message
            .get("result")
            .and_then(|result| result.get("outcome"))
            .cloned()
            .unwrap_or_else(|| json!({ "outcome": "cancelled" }));
        let selected = outcome
            .get("outcome")
            .and_then(Value::as_str)
            .is_some_and(|value| value == "selected");
        let option = outcome.get("optionId").and_then(Value::as_str);

        match pending {
            Pending::Approval {
                session_id,
                approval_id,
            } => {
                let (decision, scope) = if selected {
                    match option {
                        Some("allow-always") => (ApprovalDecision::Always, ApprovalScope::Session),
                        Some("reject-once") | Some("reject-always") => {
                            (ApprovalDecision::Deny, ApprovalScope::Once)
                        }
                        _ => (ApprovalDecision::Approve, ApprovalScope::Once),
                    }
                } else {
                    (ApprovalDecision::Deny, ApprovalScope::Once)
                };
                let _ = self
                    .requester
                    .request(
                        "approval.respond",
                        json!({
                            "session_id": session_id,
                            "approval_id": approval_id,
                            "decision": decision,
                            "scope": scope,
                            "reason": if matches!(decision, ApprovalDecision::Deny) {
                                Some("rejected in the editor")
                            } else {
                                None
                            }
                        }),
                    )
                    .await;
            }
            Pending::Input {
                session_id,
                input_id,
                options,
            } => {
                if selected {
                    if let Some(index) = option.and_then(|option| options.get(option)) {
                        let _ = self
                            .requester
                            .request(
                                "input.respond",
                                json!({
                                    "session_id": session_id,
                                    "input_id": input_id,
                                    "option_index": index
                                }),
                            )
                            .await;
                    }
                } else {
                    if let Some(turn) = self.active.get_mut(&session_id) {
                        turn.cancelled = true;
                    }
                    let _ = self
                        .requester
                        .request("prompt.cancel", json!({ "session_id": session_id }))
                        .await;
                }
            }
        }
        Ok(())
    }

    async fn new_session(&mut self, id: Value, params: Value) -> Result<()> {
        let Some(cwd) = params.get("cwd").and_then(Value::as_str) else {
            self.respond_error(id, -32602, "session/new requires cwd");
            return Ok(());
        };
        let cwd = PathBuf::from(cwd);
        if !cwd.is_absolute() {
            self.respond_error(id, -32602, "cwd must be an absolute path");
            return Ok(());
        }
        warn_mcp_servers(&params);
        let value = match self
            .requester
            .request(
                "session.create",
                json!({
                    "cwd": cwd,
                    "worktree": false,
                    "name": Value::Null,
                    "model": Value::Null,
                    "agent": Value::Null,
                    "mode": Value::Null
                }),
            )
            .await
        {
            Ok(value) => value,
            Err(err) => {
                self.respond_error(id, -32603, format!("{err:#}"));
                return Ok(());
            }
        };
        let session: Session = serde_json::from_value(
            value
                .get("session")
                .cloned()
                .context("session.create returned no session")?,
        )?;
        self.sessions
            .insert(session.id.clone(), session.cwd.clone());
        self.respond(
            id,
            json!({
                "sessionId": session.id,
                "modes": mode_state(session.mode)
            }),
        );
        Ok(())
    }

    async fn load_session(&mut self, id: Value, params: Value) -> Result<()> {
        let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
            self.respond_error(id, -32602, "session/load requires sessionId");
            return Ok(());
        };
        warn_mcp_servers(&params);
        let value = match self
            .requester
            .request("session.attach", json!({ "session_id": session_id }))
            .await
        {
            Ok(value) => value,
            Err(err) => {
                self.respond_error(id, -32002, format!("{err:#}"));
                return Ok(());
            }
        };
        let attached: AttachResult = serde_json::from_value(value)?;
        self.sessions
            .insert(attached.session.id.clone(), attached.session.cwd.clone());
        for event in attached.events {
            self.replay_event(&attached.session.id, event, &attached.session);
        }
        self.respond(
            id,
            json!({
                "modes": mode_state(attached.session.mode)
            }),
        );
        Ok(())
    }

    async fn prompt(&mut self, id: Value, params: Value) -> Result<()> {
        let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
            self.respond_error(id, -32602, "session/prompt requires sessionId");
            return Ok(());
        };
        if self.active.contains_key(session_id) {
            self.respond_error(id, -32000, "a prompt is already running for this session");
            return Ok(());
        }
        let text = prompt_text(params.get("prompt"));
        if text.trim().is_empty() {
            self.respond_error(id, -32602, "the prompt has no text content");
            return Ok(());
        }
        let value = match self
            .requester
            .request(
                "prompt.submit",
                json!({ "session_id": session_id, "text": text, "images": [] }),
            )
            .await
        {
            Ok(value) => value,
            Err(err) => {
                self.respond_error(id, -32603, format!("{err:#}"));
                return Ok(());
            }
        };
        let submitted: SubmitResult = serde_json::from_value(value)?;
        self.sessions
            .insert(session_id.to_string(), submitted.session.cwd.clone());
        self.active.insert(
            session_id.to_string(),
            ActiveTurn {
                request_id: id,
                turn_id: submitted.turn_id,
                streamed_message: false,
                cancelled: false,
            },
        );
        Ok(())
    }

    async fn set_mode(&mut self, id: Value, params: Value) -> Result<()> {
        let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
            self.respond_error(id, -32602, "session/set_mode requires sessionId");
            return Ok(());
        };
        let Some(mode) = params.get("modeId").and_then(Value::as_str) else {
            self.respond_error(id, -32602, "session/set_mode requires modeId");
            return Ok(());
        };
        let Ok(mode) = mode.parse::<SessionMode>() else {
            self.respond_error(id, -32602, format!("unknown mode: {mode}"));
            return Ok(());
        };
        match self
            .requester
            .request(
                "session.mode",
                json!({ "session_id": session_id, "mode": mode, "temporary": false }),
            )
            .await
        {
            Ok(value) => {
                let result: SessionResult = serde_json::from_value(value)?;
                self.notify(
                    "session/update",
                    json!({
                        "sessionId": session_id,
                        "update": {
                            "sessionUpdate": "current_mode_update",
                            "currentModeId": result.session.mode.as_str()
                        }
                    }),
                );
                self.respond(id, json!({}));
            }
            Err(err) => self.respond_error(id, -32603, format!("{err:#}")),
        }
        Ok(())
    }

    async fn close_session(&mut self, id: Value, params: Value) -> Result<()> {
        let Some(session_id) = params.get("sessionId").and_then(Value::as_str) else {
            self.respond_error(id, -32602, "session/close requires sessionId");
            return Ok(());
        };
        if !self.sessions.contains_key(session_id) {
            // The editor may close a session this adapter never loaded.
            self.respond_error(id, -32002, format!("unknown session: {session_id}"));
            return Ok(());
        }
        let _ = self
            .requester
            .request("prompt.cancel", json!({ "session_id": session_id }))
            .await;
        self.sessions.remove(session_id);
        self.active.remove(session_id);
        self.respond(id, json!({}));
        Ok(())
    }

    async fn handle_daemon(&mut self, notification: eix_core::Notification) -> Result<()> {
        let params = &notification.params;
        let Some(session_id) = params.get("session_id").and_then(Value::as_str) else {
            return Ok(());
        };
        if !self.sessions.contains_key(session_id) {
            return Ok(());
        }
        let turn_id = params.get("turn_id").and_then(Value::as_str);
        match notification.method.as_str() {
            "message.delta" => {
                let Some(delta) = params.get("delta").and_then(Value::as_str) else {
                    return Ok(());
                };
                if !self.turn_matches(session_id, turn_id) {
                    return Ok(());
                }
                if let Some(turn) = self.active.get_mut(session_id) {
                    turn.streamed_message = true;
                }
                self.message_chunk(session_id, "agent_message_chunk", delta);
            }
            "reasoning.delta" => {
                let Some(delta) = params.get("delta").and_then(Value::as_str) else {
                    return Ok(());
                };
                if self.turn_matches(session_id, turn_id) {
                    self.message_chunk(session_id, "agent_thought_chunk", delta);
                }
            }
            "tool.started" => {
                if !self.turn_matches(session_id, turn_id) {
                    return Ok(());
                }
                let Some(call_id) = params.get("call_id").and_then(Value::as_str) else {
                    return Ok(());
                };
                let tool = params.get("tool").and_then(Value::as_str).unwrap_or("tool");
                let key = (session_id.to_string(), call_id.to_string());
                if self.announced.insert(key.clone()) {
                    self.notify(
                        "session/update",
                        json!({
                            "sessionId": session_id,
                            "update": {
                                "sessionUpdate": "tool_call",
                                "toolCallId": call_id,
                                "name": tool,
                                "title": tool,
                                "kind": tool_kind(tool),
                                "status": "in_progress",
                                "rawInput": self.tool_args.get(&key).cloned()
                            }
                        }),
                    );
                } else {
                    self.notify(
                        "session/update",
                        json!({
                            "sessionId": session_id,
                            "update": {
                                "sessionUpdate": "tool_call_update",
                                "toolCallId": call_id,
                                "status": "in_progress"
                            }
                        }),
                    );
                }
            }
            "tool.completed" => {
                if !self.turn_matches(session_id, turn_id) {
                    return Ok(());
                }
                let Some(call_id) = params.get("call_id").and_then(Value::as_str) else {
                    return Ok(());
                };
                let key = (session_id.to_string(), call_id.to_string());
                let (ok, denied, output) = self.tool_outputs.remove(&key).unwrap_or_else(|| {
                    (
                        params.get("ok").and_then(Value::as_bool).unwrap_or(false),
                        params
                            .get("denied")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        String::new(),
                    )
                });
                self.announced.remove(&key);
                let mut update = json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": call_id,
                    "status": if ok { "completed" } else { "failed" }
                });
                if !output.is_empty() {
                    let text = if denied {
                        format!("Denied: {output}")
                    } else {
                        output
                    };
                    update["content"] =
                        json!([{ "type": "content", "content": { "type": "text", "text": text } }]);
                }
                self.notify(
                    "session/update",
                    json!({ "sessionId": session_id, "update": update }),
                );
            }
            "tool.approval_required" => {
                if !self.turn_matches(session_id, turn_id) {
                    return Ok(());
                }
                self.request_approval(session_id, params).await;
            }
            "input.required" => {
                if !self.turn_matches(session_id, turn_id) {
                    return Ok(());
                }
                self.request_input(session_id, params).await;
            }
            "turn.completed" => {
                self.finish_turn(session_id, turn_id, params);
            }
            "session.event" => {
                // The daemon publishes the event itself as the notification
                // params, so `session_id` is at the top level.
                if let Ok(event) = serde_json::from_value::<Event>(params.clone()) {
                    if let Some(cwd) = self.sessions.get(session_id).cloned() {
                        let streamed = self
                            .active
                            .get(session_id)
                            .map(|turn| turn.streamed_message)
                            .unwrap_or(false);
                        self.live_event(session_id, &cwd, event, streamed);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn turn_matches(&self, session_id: &str, turn_id: Option<&str>) -> bool {
        let Some(turn) = self.active.get(session_id) else {
            return false;
        };
        turn_id.is_none_or(|turn_id| turn_id == turn.turn_id)
    }

    async fn request_approval(&mut self, session_id: &str, params: &Value) {
        let Some(call_id) = params.get("call_id").and_then(Value::as_str) else {
            return;
        };
        let Some(approval_id) = params.get("approval_id").and_then(Value::as_str) else {
            return;
        };
        let tool = params.get("tool").and_then(Value::as_str).unwrap_or("tool");
        let summary = params
            .get("summary")
            .and_then(Value::as_str)
            .unwrap_or(tool);
        let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
        let key = (session_id.to_string(), call_id.to_string());
        let announce = self.announced.insert(key.clone());
        self.tool_args.insert(key, arguments.clone());

        let tool_call = json!({
            "toolCallId": call_id,
            "name": tool,
            "title": summary,
            "kind": tool_kind(tool),
            "status": "pending",
            "rawInput": arguments
        });
        if announce {
            let mut update = tool_call.clone();
            update["sessionUpdate"] = Value::String("tool_call".into());
            self.notify(
                "session/update",
                json!({ "sessionId": session_id, "update": update }),
            );
        }

        let mut options = vec![json!({
            "optionId": "allow-once",
            "name": "Allow once",
            "kind": "allow_once"
        })];
        if params
            .get("allow_always")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            options.push(json!({
                "optionId": "allow-always",
                "name": "Always allow this session",
                "kind": "allow_always"
            }));
        }
        options.push(json!({
            "optionId": "reject-once",
            "name": "Reject",
            "kind": "reject_once"
        }));

        let request_id = self.next_id;
        self.next_id += 1;
        self.pending.insert(
            request_id,
            Pending::Approval {
                session_id: session_id.to_string(),
                approval_id: approval_id.to_string(),
            },
        );
        self.request(
            request_id,
            "session/request_permission",
            json!({
                "sessionId": session_id,
                "toolCall": tool_call,
                "options": options
            }),
        );
    }

    async fn request_input(&mut self, session_id: &str, params: &Value) {
        let Some(input_id) = params.get("input_id").and_then(Value::as_str) else {
            return;
        };
        let Some(call_id) = params.get("call_id").and_then(Value::as_str) else {
            return;
        };
        let question = params
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("The agent needs an answer");
        let Some(items) = params.get("options").and_then(Value::as_array) else {
            return;
        };
        if items.is_empty() {
            return;
        }
        let mut options = Vec::new();
        let mut by_id = HashMap::new();
        for (index, item) in items.iter().enumerate() {
            let label = item
                .get("label")
                .and_then(Value::as_str)
                .unwrap_or("Option");
            let description = item.get("description").and_then(Value::as_str);
            let name = match description {
                Some(description) => format!("{label} — {description}"),
                None => label.to_string(),
            };
            let option_id = format!("option-{index}");
            by_id.insert(option_id.clone(), index);
            options.push(json!({
                "optionId": option_id,
                "name": name,
                "kind": "allow_once"
            }));
        }
        options.push(json!({
            "optionId": "reject-once",
            "name": "Cancel the turn",
            "kind": "reject_once"
        }));

        let key = (session_id.to_string(), call_id.to_string());
        if self.announced.insert(key) {
            self.notify(
                "session/update",
                json!({
                    "sessionId": session_id,
                    "update": {
                        "sessionUpdate": "tool_call",
                        "toolCallId": call_id,
                        "name": "ask_user",
                        "title": question,
                        "kind": "other",
                        "status": "pending"
                    }
                }),
            );
        }

        let request_id = self.next_id;
        self.next_id += 1;
        self.pending.insert(
            request_id,
            Pending::Input {
                session_id: session_id.to_string(),
                input_id: input_id.to_string(),
                options: by_id,
            },
        );
        self.request(
            request_id,
            "session/request_permission",
            json!({
                "sessionId": session_id,
                "toolCall": {
                    "toolCallId": call_id,
                    "name": "ask_user",
                    "title": question,
                    "kind": "other",
                    "status": "pending"
                },
                "options": options
            }),
        );
    }

    fn finish_turn(&mut self, session_id: &str, turn_id: Option<&str>, params: &Value) {
        let Some(turn) = self.active.get(session_id) else {
            return;
        };
        if let Some(turn_id) = turn_id {
            if turn_id != turn.turn_id {
                return;
            }
        }
        let turn = self.active.remove(session_id).expect("checked above");
        let error = params.get("error").and_then(Value::as_str);
        if let Some(error) = error.filter(|error| !error.is_empty()) {
            self.message_chunk(
                session_id,
                "agent_message_chunk",
                &format!("eix error: {error}"),
            );
        }
        let cancelled = turn.cancelled
            || params
                .get("cancelled")
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let stop_reason = if cancelled { "cancelled" } else { "end_turn" };
        self.respond(turn.request_id, json!({ "stopReason": stop_reason }));
        // The turn is over; resolve any permission request still open in the
        // editor so it does not wait forever.
        let ids: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending_session(pending) == session_id)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if self.pending.remove(&id).is_some() {
                self.respond(json!(id), json!({ "outcome": { "outcome": "cancelled" } }));
            }
        }
    }

    async fn deny_pending(&mut self, session_id: &str) {
        let ids: Vec<u64> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending_session(pending) == session_id)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(Pending::Approval {
                session_id,
                approval_id,
            }) = self.pending.remove(&id)
            {
                let _ = self
                    .requester
                    .request(
                        "approval.respond",
                        json!({
                            "session_id": session_id,
                            "approval_id": approval_id,
                            "decision": ApprovalDecision::Deny,
                            "scope": ApprovalScope::Once,
                            "reason": "cancelled in the editor"
                        }),
                    )
                    .await;
            }
        }
    }

    fn replay_event(&self, session_id: &str, event: Event, session: &Session) {
        match event.kind {
            EventKind::UserMessage => {
                if let Ok(payload) = serde_json::from_value::<UserMessagePayload>(event.payload) {
                    self.message_chunk(session_id, "user_message_chunk", &payload.text);
                }
            }
            EventKind::AssistantMessage => {
                if let Ok(payload) =
                    serde_json::from_value::<AssistantMessagePayload>(event.payload)
                {
                    if let Some(reasoning) = payload.reasoning.as_deref() {
                        if !reasoning.is_empty() {
                            self.message_chunk(session_id, "agent_thought_chunk", reasoning);
                        }
                    }
                    if !payload.text.is_empty() {
                        self.message_chunk(session_id, "agent_message_chunk", &payload.text);
                    }
                }
            }
            EventKind::ToolCall => {
                if let Ok(payload) = serde_json::from_value::<ToolCallPayload>(event.payload) {
                    self.notify(
                        "session/update",
                        json!({
                            "sessionId": session_id,
                            "update": {
                                "sessionUpdate": "tool_call",
                                "toolCallId": payload.call_id,
                                "name": payload.tool,
                                "title": payload.tool,
                                "kind": tool_kind(&payload.tool),
                                "status": "pending",
                                "rawInput": payload.arguments
                            }
                        }),
                    );
                }
            }
            EventKind::ToolResult => {
                if let Ok(payload) = serde_json::from_value::<ToolResultPayload>(event.payload) {
                    self.tool_result_update(session_id, &payload);
                }
            }
            EventKind::PatchProposed => {
                if let Ok(payload) =
                    serde_json::from_value::<eix_core::PatchProposedPayload>(event.payload)
                {
                    if let Some(diff) = payload.diff.as_deref() {
                        self.notify(
                            "session/update",
                            json!({
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "tool_call_update",
                                    "toolCallId": payload.call_id,
                                    "status": "in_progress",
                                    "content": [{
                                        "type": "content",
                                        "content": {"type": "text", "text": diff}
                                    }]
                                }
                            }),
                        );
                    }
                }
            }
            EventKind::Error => {
                if let Some(message) = event.payload.get("message").and_then(Value::as_str) {
                    self.message_chunk(
                        session_id,
                        "agent_message_chunk",
                        &format!("eix: {message}"),
                    );
                }
            }
            EventKind::Mode => {
                if let Some(mode) = event.payload.get("to").and_then(Value::as_str) {
                    self.notify(
                        "session/update",
                        json!({
                            "sessionId": session_id,
                            "update": {
                                "sessionUpdate": "current_mode_update",
                                "currentModeId": mode
                            }
                        }),
                    );
                }
            }
            _ => {}
        }
        let _ = session;
    }

    fn live_event(&mut self, session_id: &str, _cwd: &Path, event: Event, streamed: bool) {
        match event.kind {
            EventKind::ToolCall => {
                if let Ok(payload) = serde_json::from_value::<ToolCallPayload>(event.payload) {
                    let key = (session_id.to_string(), payload.call_id.clone());
                    self.tool_args
                        .insert(key.clone(), payload.arguments.clone());
                    if self.announced.insert(key) {
                        self.notify(
                            "session/update",
                            json!({
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "tool_call",
                                    "toolCallId": payload.call_id,
                                    "name": payload.tool,
                                    "title": payload.tool,
                                    "kind": tool_kind(&payload.tool),
                                    "status": "pending",
                                    "rawInput": payload.arguments
                                }
                            }),
                        );
                    }
                }
            }
            EventKind::ToolResult => {
                if let Ok(payload) = serde_json::from_value::<ToolResultPayload>(event.payload) {
                    let key = (session_id.to_string(), payload.call_id.clone());
                    self.tool_outputs
                        .insert(key, (payload.ok, payload.denied, payload.output.clone()));
                }
            }
            EventKind::AssistantMessage if !streamed => {
                if let Ok(payload) =
                    serde_json::from_value::<AssistantMessagePayload>(event.payload)
                {
                    if !payload.text.is_empty() {
                        self.message_chunk(session_id, "agent_message_chunk", &payload.text);
                    }
                    if let Some(turn) = self.active.get_mut(session_id) {
                        turn.streamed_message = true;
                    }
                }
            }
            EventKind::PatchProposed => {
                if let Ok(payload) =
                    serde_json::from_value::<eix_core::PatchProposedPayload>(event.payload)
                {
                    if let Some(diff) = payload.diff.as_deref() {
                        self.notify(
                            "session/update",
                            json!({
                                "sessionId": session_id,
                                "update": {
                                    "sessionUpdate": "tool_call_update",
                                    "toolCallId": payload.call_id,
                                    "status": "in_progress",
                                    "content": [{
                                        "type": "content",
                                        "content": {"type": "text", "text": diff}
                                    }]
                                }
                            }),
                        );
                    }
                }
            }
            EventKind::Mode => {
                if let Some(mode) = event.payload.get("to").and_then(Value::as_str) {
                    self.notify(
                        "session/update",
                        json!({
                            "sessionId": session_id,
                            "update": {
                                "sessionUpdate": "current_mode_update",
                                "currentModeId": mode
                            }
                        }),
                    );
                }
            }
            _ => {}
        }
    }

    fn tool_result_update(&self, session_id: &str, payload: &ToolResultPayload) {
        let text = if payload.denied {
            format!("Denied: {}", payload.output)
        } else {
            payload.output.clone()
        };
        self.notify(
            "session/update",
            json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": payload.call_id,
                    "status": if payload.ok { "completed" } else { "failed" },
                    "content": [{
                        "type": "content",
                        "content": {"type": "text", "text": text}
                    }]
                }
            }),
        );
    }

    fn message_chunk(&self, session_id: &str, kind: &str, text: &str) {
        self.notify(
            "session/update",
            json!({
                "sessionId": session_id,
                "update": {
                    "sessionUpdate": kind,
                    "content": { "type": "text", "text": text }
                }
            }),
        );
    }

    fn respond(&self, id: Value, result: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn respond_error(&self, id: Value, code: i64, message: impl Into<String>) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message.into() }
        }));
    }

    fn notify(&self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn request(&self, id: u64, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
    }

    fn send(&self, message: Value) {
        let _ = self.out.send(message);
    }
}

/// Editor-provided MCP servers are not wired into eix sessions yet. Say so
/// once rather than silently dropping their tools.
fn warn_mcp_servers(params: &Value) {
    if params
        .get("mcpServers")
        .and_then(Value::as_array)
        .is_some_and(|servers| !servers.is_empty())
    {
        eprintln!(
            "eix acp: ignoring editor-provided MCP servers; \
             configure them under mcp.servers in the eix config instead"
        );
    }
}

fn pending_session(pending: &Pending) -> &str {
    match pending {
        Pending::Approval { session_id, .. } | Pending::Input { session_id, .. } => session_id,
    }
}

fn prompt_text(prompt: Option<&Value>) -> String {
    let mut parts = Vec::new();
    if let Some(items) = prompt.and_then(Value::as_array) {
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            parts.push(text.to_string());
                        }
                    }
                }
                Some("resource_link") => {
                    let name = item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("resource");
                    let uri = item.get("uri").and_then(Value::as_str).unwrap_or("");
                    parts.push(format!("@{name} ({uri})"));
                }
                Some("resource") => {
                    if let Some(text) = item
                        .get("resource")
                        .and_then(|resource| resource.get("text"))
                        .and_then(Value::as_str)
                    {
                        parts.push(text.to_string());
                    }
                }
                _ => {}
            }
        }
    }
    parts.join("\n\n")
}

fn mode_state(mode: SessionMode) -> Value {
    json!({
        "currentModeId": mode.as_str(),
        "availableModes": [
            {
                "id": "plan",
                "name": "Plan",
                "description": "Read-only; inspect and propose changes"
            },
            {
                "id": "build",
                "name": "Build",
                "description": "Edit the workspace; risky calls ask first"
            },
            {
                "id": "auto",
                "name": "Auto",
                "description": "Let the classifier approve low-risk calls"
            },
            {
                "id": "bypass",
                "name": "Bypass",
                "description": "Skip unruled approvals; hard denies still apply"
            }
        ]
    })
}

fn tool_kind(tool: &str) -> &'static str {
    match tool {
        "read_file" | "list_dir" | "view_image" | "mcp_read_resource" | "mcp_list_resources" => {
            "read"
        }
        "write_file" | "edit" | "patch" | "apply_patch" | "mcp_call_tool" => "edit",
        "grep" | "glob" | "web_search" => "search",
        "exec_command" | "run_command" | "start_command" | "write_stdin" => "execute",
        "webfetch" | "web_fetch" => "fetch",
        "ask_user" => "other",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_text_joins_text_and_resources() {
        let prompt = json!([
            {"type": "text", "text": "hello"},
            {"type": "resource_link", "name": "main.rs", "uri": "file:///tmp/main.rs"},
            {"type": "resource", "resource": {"text": "embedded"}},
            {"type": "image", "data": "..."}
        ]);
        assert_eq!(
            prompt_text(Some(&prompt)),
            "hello\n\n@main.rs (file:///tmp/main.rs)\n\nembedded"
        );
    }

    #[test]
    fn modes_round_trip_through_the_eix_parser() {
        let state = mode_state(SessionMode::Build);
        assert_eq!(state["currentModeId"], "build");
        for mode in state["availableModes"].as_array().unwrap() {
            let id = mode["id"].as_str().unwrap();
            assert!(id.parse::<SessionMode>().is_ok(), "{id}");
        }
    }

    #[test]
    fn tool_kinds_cover_the_common_tools() {
        assert_eq!(tool_kind("read_file"), "read");
        assert_eq!(tool_kind("write_file"), "edit");
        assert_eq!(tool_kind("run_command"), "execute");
        assert_eq!(tool_kind("grep"), "search");
        assert_eq!(tool_kind("webfetch"), "fetch");
    }
}
