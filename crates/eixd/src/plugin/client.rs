//! One plugin child process and its JSON-RPC 2.0 connection.
//!
//! Requests are multiplexed: a writer task owns stdin, a reader task owns
//! stdout, and every in-flight request waits on its own oneshot channel.
//! A slow tool call therefore cannot block events, UI updates or another
//! call to the same plugin.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use eix_core::plugin::{
    error_code, PluginIdentity, PluginInitializeParams, PluginInitializeResult, RpcNotification,
    RpcRequest, RpcResponse, PLUGIN_API_VERSION,
};
use eix_core::redact::redact;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot, Mutex, Semaphore};

use crate::store::Store;

use super::host_api::HostContext;
use super::PluginSpec;

/// Outbound frames buffered before a request or event has to wait.
const FRAME_BUFFER: usize = 128;
/// Host requests from one plugin handled concurrently. A plugin that floods
/// the host gets backpressure instead of unbounded tasks.
const MAX_INFLIGHT: usize = 8;
/// Largest protocol line accepted from a plugin. A larger line is treated as
/// a protocol error and the child is stopped.
const MAX_FRAME: usize = 8 * 1024 * 1024;
/// Grace period for `shutdown` before the process is killed.
const SHUTDOWN_GRACE: Duration = Duration::from_millis(500);

/// A started plugin process. `Arc<PluginClient>` is shared by tool calls and
/// event delivery; all mutation goes through the connection's channels.
pub struct PluginClient {
    pub name: String,
    connection: Arc<Connection>,
    child: Arc<Mutex<Child>>,
    closed: Arc<AtomicBool>,
    closed_reason: Arc<Mutex<Option<String>>>,
}

impl PluginClient {
    pub async fn spawn(
        spec: &PluginSpec,
        store: Arc<Store>,
        bus: tokio::sync::broadcast::Sender<eix_core::Notification>,
        rpc_tx: mpsc::Sender<super::host_api::RpcCallRequest>,
    ) -> Result<Arc<Self>> {
        let workspace = spec.cwd.clone();
        let plugin_data_dir = spec.data_dir.clone();
        std::fs::create_dir_all(&plugin_data_dir)
            .with_context(|| format!("cannot create {}", plugin_data_dir.display()))?;

        let mut command = Command::new(&spec.command);
        command
            .args(&spec.args)
            .envs(&spec.env)
            .env("EIX_PLUGIN_NAME", &spec.name)
            .env("EIX_PLUGIN_API_VERSION", PLUGIN_API_VERSION.to_string())
            .env("EIX_PLUGIN_WORKSPACE", &workspace)
            .current_dir(&spec.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .with_context(|| format!("cannot start plugin {} ({})", spec.name, spec.command))?;
        let stdin = child.stdin.take().context("plugin stdin was not piped")?;
        let stdout = child.stdout.take().context("plugin stdout was not piped")?;
        if let Some(stderr) = child.stderr.take() {
            let name = spec.name.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    eprintln!("eix-plugin[{name}]: {}", redact(&line));
                }
            });
        }

        let (frame_tx, frame_rx) = mpsc::channel::<Value>(FRAME_BUFFER);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let closed = Arc::new(AtomicBool::new(false));
        let closed_reason = Arc::new(Mutex::new(None));

        let connection = Arc::new(Connection {
            name: spec.name.clone(),
            timeout: spec.timeout,
            frame_tx: frame_tx.clone(),
            next_id: AtomicU64::new(1),
            pending: Arc::clone(&pending),
            inflight: Arc::new(Semaphore::new(MAX_INFLIGHT)),
            closed: Arc::clone(&closed),
            closed_reason: Arc::clone(&closed_reason),
        });

        let host = HostContext::new(
            spec.name.clone(),
            spec.capabilities.iter().copied().collect(),
            workspace,
            Arc::clone(&store),
            frame_tx.clone(),
            plugin_data_dir,
            spec.network_allow.clone(),
            spec.secrets.clone(),
            spec.rpc_allow.clone(),
            spec.env.clone(),
            bus,
            rpc_tx,
        );

        let writer_connection = Arc::clone(&connection);
        let mut stdin = stdin;
        tokio::spawn(async move {
            writer_connection.write_loop(&mut stdin, frame_rx).await;
        });

        let reader_connection = Arc::clone(&connection);
        let reader_host = host.clone();
        let reader_child = Arc::new(Mutex::new(child));
        let reader_child_handle = Arc::clone(&reader_child);
        let mut stdout = stdout;
        tokio::spawn(async move {
            reader_connection
                .read_loop(&mut stdout, &reader_host, &reader_child_handle)
                .await;
        });

        Ok(Arc::new(Self {
            name: spec.name.clone(),
            connection,
            child: reader_child,
            closed,
            closed_reason,
        }))
    }

    pub async fn initialize(
        &self,
        spec: &PluginSpec,
        workspace: &Path,
    ) -> Result<PluginInitializeResult> {
        let params = PluginInitializeParams {
            protocol_version: PLUGIN_API_VERSION,
            api_version: PLUGIN_API_VERSION,
            plugin: PluginIdentity {
                name: spec.name.clone(),
                version: spec.version.clone(),
            },
            host: PluginIdentity {
                name: "eixd".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
            features: vec![
                "content_blocks".to_string(),
                "cancel".to_string(),
                "hooks".to_string(),
                "ui".to_string(),
                "rpc".to_string(),
                "jobs".to_string(),
                "fs".to_string(),
                "net".to_string(),
                "secrets".to_string(),
                "session_read".to_string(),
                "blobs".to_string(),
                "command_actions".to_string(),
            ],
            capabilities: spec.capabilities.clone(),
            workspace: Some(workspace.to_path_buf()),
            config: json!({
                "data_dir": spec.data_dir,
                "settings": spec.settings,
            }),
        };
        let value = self
            .connection
            .request("initialize", serde_json::to_value(params)?)
            .await
            .context("initialize failed")?;
        let init: PluginInitializeResult =
            serde_json::from_value(value).context("invalid initialize result")?;
        if init.api_version != PLUGIN_API_VERSION {
            bail!(
                "plugin {} speaks API version {}, host expects {}",
                spec.name,
                init.api_version,
                PLUGIN_API_VERSION
            );
        }
        if !init.name.is_empty() && init.name != spec.name {
            bail!(
                "plugin {} initialized as {:?}, which does not match its manifest",
                spec.name,
                init.name
            );
        }
        Ok(init)
    }

    /// Send a request and wait for its response. Times out and asks the plugin
    /// to cancel the call on failure.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.connection.request(method, params).await
    }

    /// Deliver one subscribed daemon notification. Uses `try_send` so a slow
    /// plugin is skipped instead of blocking the event fan-out.
    pub async fn notify_event(&self, method: &str, params: &Value) -> Result<()> {
        let frame = serde_json::to_value(RpcNotification::new(method, params.clone()))?;
        self.connection.try_send(frame)
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Relaxed)
    }

    pub async fn closed_reason(&self) -> Option<String> {
        self.closed_reason.lock().await.clone()
    }

    /// Return a reason when the child is no longer usable.
    pub async fn exit_reason(&self) -> Option<String> {
        if self.is_closed() {
            return Some(
                self.closed_reason()
                    .await
                    .unwrap_or_else(|| "plugin connection closed".into()),
            );
        }
        let mut child = self.child.lock().await;
        match child.try_wait() {
            Ok(Some(status)) => Some(format!("plugin exited unexpectedly ({status})")),
            Ok(None) => None,
            Err(err) => Some(format!("plugin process check failed: {err}")),
        }
    }

    /// Best-effort graceful stop, then kill. A stuck plugin cannot delay
    /// daemon shutdown for more than [`SHUTDOWN_GRACE`].
    pub async fn shutdown(&self) {
        let frame = serde_json::to_value(RpcNotification::new("shutdown", json!({})))
            .unwrap_or(Value::Null);
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, self.connection.send(frame)).await;
        let mut child = self.child.lock().await;
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, child.wait()).await;
        let _ = child.start_kill();
    }
}

/// Shared transport state. Held by the client and both IO tasks.
struct Connection {
    name: String,
    timeout: Duration,
    frame_tx: mpsc::Sender<Value>,
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<RpcResponse>>>>,
    inflight: Arc<Semaphore>,
    closed: Arc<AtomicBool>,
    closed_reason: Arc<Mutex<Option<String>>>,
}

impl Connection {
    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        if self.closed.load(Ordering::Relaxed) {
            bail!("plugin {} is not running", self.name);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        let frame = serde_json::to_value(RpcRequest::new(id, method, params))?;
        let sent = tokio::time::timeout(self.timeout, self.frame_tx.send(frame)).await;
        if !matches!(sent, Ok(Ok(()))) {
            self.pending.lock().await.remove(&id);
            bail!("plugin {} did not accept the request", self.name);
        }
        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(response)) => match response.error {
                Some(error) => bail!("plugin error {}: {}", error.code, error.message),
                None => Ok(response.result.unwrap_or(Value::Null)),
            },
            Ok(Err(_)) => bail!("plugin {} disconnected", self.name),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                let _ = self
                    .frame_tx
                    .try_send(serde_json::to_value(RpcNotification::new(
                        "$/cancel",
                        json!({ "id": id }),
                    ))?);
                bail!(
                    "plugin {} call {method} timed out after {}s",
                    self.name,
                    self.timeout.as_secs()
                )
            }
        }
    }

    async fn send(&self, frame: Value) -> Result<()> {
        self.frame_tx
            .send(frame)
            .await
            .map_err(|_| anyhow::anyhow!("plugin {} is not running", self.name))
    }

    fn try_send(&self, frame: Value) -> Result<()> {
        self.frame_tx
            .try_send(frame)
            .map_err(|_| anyhow::anyhow!("plugin {} is busy or stopped", self.name))
    }

    async fn write_loop(
        &self,
        stdin: &mut tokio::process::ChildStdin,
        mut rx: mpsc::Receiver<Value>,
    ) {
        while let Some(frame) = rx.recv().await {
            let line = match serde_json::to_string(&frame) {
                Ok(line) => line,
                Err(err) => {
                    eprintln!("eixd: plugin {} serialization failed: {err}", self.name);
                    continue;
                }
            };
            if let Err(err) = stdin.write_all(line.as_bytes()).await {
                self.close(&format!("cannot write to plugin {}: {err}", self.name))
                    .await;
                return;
            }
            if let Err(err) = stdin.write_all(b"\n").await {
                self.close(&format!("cannot write to plugin {}: {err}", self.name))
                    .await;
                return;
            }
            if let Err(err) = stdin.flush().await {
                self.close(&format!("cannot flush plugin {}: {err}", self.name))
                    .await;
                return;
            }
        }
    }

    async fn read_loop(
        &self,
        stdout: &mut tokio::process::ChildStdout,
        host: &HostContext,
        child: &Arc<Mutex<Child>>,
    ) {
        let mut lines = BufReader::new(stdout).lines();
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => {
                    self.close(&format!("plugin {} closed stdout", self.name))
                        .await;
                    break;
                }
                Err(err) => {
                    self.close(&format!("cannot read plugin {}: {err}", self.name))
                        .await;
                    break;
                }
            };
            if line.trim().is_empty() {
                continue;
            }
            if line.len() > MAX_FRAME {
                self.close(&format!(
                    "plugin {} sent a {} byte frame (limit {MAX_FRAME})",
                    self.name,
                    line.len()
                ))
                .await;
                break;
            }
            let value: Value = match serde_json::from_str(&line) {
                Ok(value) => value,
                Err(err) => {
                    eprintln!(
                        "eixd: plugin {} sent invalid JSON: {err}: {}",
                        self.name,
                        redact(&line)
                    );
                    continue;
                }
            };
            self.dispatch(value, host).await;
        }
        let mut child = child.lock().await;
        let _ = child.start_kill();
    }

    async fn dispatch(&self, value: Value, host: &HostContext) {
        let method = value
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let id = value.get("id").and_then(Value::as_u64);
        match (method, id) {
            (Some(method), Some(id)) => {
                let params = value.get("params").cloned().unwrap_or(Value::Null);
                let request = RpcRequest {
                    jsonrpc: eix_core::plugin::JSONRPC_VERSION.to_string(),
                    id,
                    method,
                    params,
                };
                let host = host.clone();
                let permit = match Arc::clone(&self.inflight).acquire_owned().await {
                    Ok(permit) => permit,
                    Err(_) => return,
                };
                tokio::spawn(async move {
                    host.handle(request).await;
                    drop(permit);
                });
            }
            (Some(method), None) => {
                if method == "host.log" {
                    host.log_notification(&value);
                }
            }
            (None, Some(id)) => match serde_json::from_value::<RpcResponse>(value) {
                Ok(response) => {
                    if let Some(tx) = self.pending.lock().await.remove(&id) {
                        let _ = tx.send(response);
                    }
                }
                Err(err) => {
                    eprintln!("eixd: plugin {} sent an invalid response: {err}", self.name);
                }
            },
            (None, None) => {}
        }
    }

    async fn close(&self, reason: &str) {
        if self.closed.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.closed_reason.lock().await = Some(redact(reason));
        let mut pending = self.pending.lock().await;
        for (id, tx) in pending.drain() {
            let _ = tx.send(RpcResponse::err(
                id,
                error_code::PLUGIN_FAILED,
                reason.to_string(),
            ));
        }
    }
}
