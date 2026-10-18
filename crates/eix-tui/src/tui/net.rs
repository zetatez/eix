//! Split socket connection: one writer task, one reader task.
//!
//! The TUI needs to read notifications continuously while it also sends
//! requests (submit, cancel, attach), so a request/response-only client cannot
//! own the socket. Responses are routed by id; notifications go to the UI.

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use eix_core::{Notification, Request, Response, ServerMessage};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug)]
pub enum NetEvent {
    Notification(Notification),
    Closed(String),
}

#[derive(Clone)]
pub struct Requester {
    out: mpsc::UnboundedSender<Vec<u8>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Response>>>>,
    next_id: Arc<AtomicU64>,
}

impl Requester {
    pub async fn connect(socket: &Path) -> Result<(Self, mpsc::UnboundedReceiver<NetEvent>)> {
        let stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("cannot connect to {}", socket.display()))?;
        let (read, write) = tokio::io::split(stream);

        let (out_tx, out_rx) = mpsc::unbounded_channel();
        tokio::spawn(writer_loop(write, out_rx));

        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Response>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        tokio::spawn(reader_loop(read, Arc::clone(&pending), event_tx));

        Ok((
            Self {
                out: out_tx,
                pending,
                next_id: Arc::new(AtomicU64::new(1)),
            },
            event_rx,
        ))
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending mutex poisoned")
            .insert(id, tx);

        let mut line = serde_json::to_vec(&Request::new(id, method, params))?;
        line.push(b'\n');
        if self.out.send(line).is_err() {
            self.pending
                .lock()
                .expect("pending mutex poisoned")
                .remove(&id);
            bail!("daemon connection closed");
        }

        let response = match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => bail!("daemon connection closed while waiting for {method}"),
            Err(_) => bail!("daemon did not answer {method} within 30s"),
        };
        if let Some(error) = response.error {
            bail!("{}: {}", error.code, error.message);
        }
        Ok(response.result.unwrap_or(Value::Null))
    }
}

async fn writer_loop(mut write: WriteHalf<UnixStream>, mut rx: mpsc::UnboundedReceiver<Vec<u8>>) {
    while let Some(bytes) = rx.recv().await {
        if write.write_all(&bytes).await.is_err() {
            break;
        }
        if write.flush().await.is_err() {
            break;
        }
    }
}

async fn reader_loop(
    read: ReadHalf<UnixStream>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Response>>>>,
    events: mpsc::UnboundedSender<NetEvent>,
) {
    let mut lines = BufReader::new(read).lines();
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => match serde_json::from_str::<ServerMessage>(line.trim_end()) {
                Ok(ServerMessage::Response(response)) => {
                    if let Some(tx) = pending
                        .lock()
                        .expect("pending mutex poisoned")
                        .remove(&response.id)
                    {
                        let _ = tx.send(response);
                    }
                }
                Ok(ServerMessage::Notification(notification)) => {
                    if events.send(NetEvent::Notification(notification)).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = events.send(NetEvent::Closed(format!(
                        "invalid message from daemon: {err}"
                    )));
                    break;
                }
            },
            Ok(None) => {
                let _ = events.send(NetEvent::Closed("daemon closed the connection".into()));
                break;
            }
            Err(err) => {
                let _ = events.send(NetEvent::Closed(format!("read error: {err}")));
                break;
            }
        }
    }
    // Wake every pending request so none hangs forever.
    pending.lock().expect("pending mutex poisoned").clear();
}
