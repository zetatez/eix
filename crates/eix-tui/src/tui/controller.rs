//! Owns the daemon connection and serializes TUI commands into requests.
//!
//! The UI never blocks on the network: it sends [`Command`]s and receives
//! [`Event`]s. Requests and notifications share one connection, so a request
//! can be in flight while deltas keep arriving.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use eix_core::{
    ApprovalDecision, ApprovalResult, AttachResult, CancelResult, HelloResult, Notification,
    Session, SessionList, SessionResult, SubmitResult,
};
use serde_json::{json, Value};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use super::net::{NetEvent, Requester};
use crate::client::ensure_running;

pub enum Command {
    Hello,
    List,
    Attach(String),
    Create,
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
    },
}

impl Command {
    fn label(&self) -> &'static str {
        match self {
            Self::Hello => "connect",
            Self::List => "list",
            Self::Attach(_) => "attach",
            Self::Create => "create",
            Self::Submit { .. } => "submit",
            Self::Cancel(_) => "cancel",
            Self::Approval { .. } => "approval",
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
    Approval { approval_id: String, accepted: bool },
    Notification(Notification),
    Error { context: String, message: String },
    Closed(String),
}

pub fn spawn(
    socket: PathBuf,
    data_dir: PathBuf,
    config: Option<PathBuf>,
) -> (UnboundedSender<Command>, UnboundedReceiver<Event>) {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (ev_tx, ev_rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        if let Err(err) = run(socket, data_dir, config, cmd_rx, ev_tx.clone()).await {
            let _ = ev_tx.send(Event::Error {
                context: "connect".into(),
                message: format!("{err:#}"),
            });
        }
    });
    (cmd_tx, ev_rx)
}

async fn run(
    socket: PathBuf,
    data_dir: PathBuf,
    config: Option<PathBuf>,
    mut cmd_rx: UnboundedReceiver<Command>,
    ev_tx: UnboundedSender<Event>,
) -> Result<()> {
    let cwd = std::env::current_dir().context("cannot determine current directory")?;
    ensure_running(&socket, &data_dir, config.as_deref()).await?;
    let (requester, mut net_events) = Requester::connect(&socket).await?;

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
        Command::Create => {
            let value = requester
                .request(
                    "session.create",
                    json!({
                        "cwd": cwd,
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
        } => {
            let value = requester
                .request(
                    "approval.respond",
                    json!({
                        "session_id": session_id,
                        "approval_id": approval_id,
                        "decision": decision,
                        "hunks": hunks,
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
    }
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).context("unexpected daemon response")
}

fn send(events: &UnboundedSender<Event>, event: Event) -> Result<()> {
    events.send(event).map_err(|_| anyhow::anyhow!("ui closed"))
}
