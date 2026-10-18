//! Owns the daemon connection and serializes TUI commands into requests.
//!
//! The UI never blocks on the network: it sends [`Command`]s and receives
//! [`Event`]s. Requests and notifications share one connection, so a request
//! can be in flight while deltas keep arriving.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use eix_core::{
    ApprovalDecision, ApprovalResult, AttachResult, CancelResult, ContextLedgerResult, Endpoint,
    HelloResult, Notification, ProcessKillResult, ProcessList, ProcessOutput, RewindResult,
    SearchResult, Session, SessionList, SessionResult, SubmitResult,
};
use serde_json::{json, Value};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use super::net::{NetEvent, Requester};
use crate::client::ensure_endpoint;

pub enum Command {
    Hello,
    List,
    Attach(String),
    Create {
        /// Create the session in its own git worktree.
        worktree: bool,
    },
    Submit {
        session_id: String,
        text: String,
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
            Self::Attach(_) => "attach",
            Self::Create { .. } => "create",
            Self::Submit { .. } => "submit",
            Self::Cancel(_) => "cancel",
            Self::Approval { .. } => "approval",
            Self::ContextLedger { .. } | Self::ContextOp { .. } => "context",
            Self::ProcessList { .. } => "processes",
            Self::ProcessOutput { .. } => "process output",
            Self::ProcessKill { .. } => "kill process",
            Self::Fork { .. } => "fork",
            Self::Rewind { .. } => "rewind",
            Self::Search { .. } => "search",
            Self::PickerData { .. } => "picker",
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
    Attached(AttachResult),
    Created(Session),
    Submitted(SubmitResult),
    Cancelled(bool),
    Approval {
        approval_id: String,
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
) -> (UnboundedSender<Command>, UnboundedReceiver<Event>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        if let Err(err) = run(endpoint, data_dir, config, cmd_rx, ev_tx.clone()).await {
            let _ = ev_tx.send(Event::Error {
                context: "connect".into(),
                message: format!("{err:#}"),
            });
        }
    });
    (cmd_tx, ev_rx)
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
    cwd: &Path,
) -> Result<()> {
    match command {
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
                .request("session.attach", json!({ "session_id": session_id }))
                .await?;
            send(events, Event::Attached(decode(value)?))
        }
        Command::Create { worktree } => {
            let value = requester
                .request(
                    "session.create",
                    json!({
                        "cwd": cwd,
                        "worktree": worktree,
                    }),
                )
                .await?;
            let result: SessionResult = decode(value)?;
            send(events, Event::Created(result.session))
        }
        Command::Submit { session_id, text } => {
            let value = requester
                .request(
                    "prompt.submit",
                    json!({ "session_id": session_id, "text": text }),
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
