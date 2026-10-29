//! Host API served to plugin processes.
//!
//! Every method checks the capability granted in configuration. The host is
//! deliberately conservative: empty allowlists deny, network redirects are
//! not followed, filesystem paths are canonicalized inside the workspace, and
//! plugin blobs live under the plugin's own data directory.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use eix_core::plugin::{
    error_code, PluginCapability, RpcNotification, RpcRequest, RpcResponse, PLUGIN_API_VERSION,
};
use eix_core::redact::redact;
use eix_core::Notification;
use futures_util::StreamExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::{broadcast, mpsc, oneshot, Mutex};

use crate::store::Store;

const MAX_STORAGE_KEY: usize = 512;
const MAX_STORAGE_VALUE: usize = 1_000_000;
const MAX_FS_FILE: usize = 1024 * 1024;
const MAX_FS_LIST: usize = 1000;
const MAX_NET_BODY: usize = 1024 * 1024;
const MAX_NET_RESPONSE: usize = 4 * 1024 * 1024;
const MAX_BLOB: usize = 8 * 1024 * 1024;
const MAX_JOBS: usize = 32;
const MAX_RPC_DEPTH: u32 = 8;
const NET_TIMEOUT: Duration = Duration::from_secs(30);
const RPC_TIMEOUT: Duration = Duration::from_secs(60);

/// One plugin-to-plugin RPC routed through the manager.
pub(super) struct RpcCallRequest {
    pub from: String,
    pub target: String,
    pub method: String,
    pub params: Value,
    pub depth: u32,
    pub reply: oneshot::Sender<Result<Value, String>>,
}

/// Everything a host method needs. Cloned per incoming request.
#[derive(Clone)]
pub struct HostContext {
    name: String,
    capabilities: BTreeSet<PluginCapability>,
    workspace: PathBuf,
    store: Arc<Store>,
    frame_tx: mpsc::Sender<Value>,
    data_dir: PathBuf,
    network_allow: Vec<String>,
    secrets: Vec<String>,
    rpc_allow: Vec<String>,
    env: BTreeMap<String, String>,
    bus: broadcast::Sender<Notification>,
    rpc_tx: mpsc::Sender<RpcCallRequest>,
    jobs: Arc<Mutex<HashMap<String, tokio::task::JoinHandle<()>>>>,
}

impl HostContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: String,
        capabilities: BTreeSet<PluginCapability>,
        workspace: PathBuf,
        store: Arc<Store>,
        frame_tx: mpsc::Sender<Value>,
        data_dir: PathBuf,
        network_allow: Vec<String>,
        secrets: Vec<String>,
        rpc_allow: Vec<String>,
        env: BTreeMap<String, String>,
        bus: broadcast::Sender<Notification>,
        rpc_tx: mpsc::Sender<RpcCallRequest>,
    ) -> Self {
        Self {
            name,
            capabilities,
            workspace,
            store,
            frame_tx,
            data_dir,
            network_allow,
            secrets,
            rpc_allow,
            env,
            bus,
            rpc_tx,
            jobs: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// Handle one `host.*` request and write its response back to the plugin.
    pub async fn handle(&self, request: RpcRequest) {
        let method = request.method.as_str();
        let response = if let Some(capability) = required_capability(method) {
            if !self.capabilities.contains(&capability) {
                RpcResponse::err(
                    request.id,
                    error_code::UNAUTHORIZED,
                    format!(
                        "plugin {} was not granted the {} capability",
                        self.name,
                        capability.as_str()
                    ),
                )
            } else {
                self.execute(&request).await
            }
        } else if !known_host_method(method) {
            RpcResponse::err(
                request.id,
                error_code::METHOD_NOT_FOUND,
                format!("unknown host method {method:?}"),
            )
        } else {
            self.execute(&request).await
        };
        let _ = self
            .frame_tx
            .send(serde_json::to_value(response).unwrap_or(Value::Null))
            .await;
    }

    async fn execute(&self, request: &RpcRequest) -> RpcResponse {
        match self.dispatch(request).await {
            Ok(value) => RpcResponse::ok(request.id, value),
            Err(err) => {
                RpcResponse::err(request.id, error_code::INVALID_PARAMS, format!("{err:#}"))
            }
        }
    }

    async fn dispatch(&self, request: &RpcRequest) -> Result<Value> {
        let method = request.method.as_str();
        let params = &request.params;
        match method {
            "host.log" => {
                self.write_log(params);
                Ok(Value::Null)
            }
            "host.info" => Ok(json!({
                "name": self.name,
                "api_version": PLUGIN_API_VERSION,
                "capabilities": self.capabilities,
                "workspace": self.workspace,
                "network_allow": self.network_allow,
                "secrets": self.secrets,
                "rpc_allow": self.rpc_allow,
            })),
            "host.ping" => Ok(json!({"pong": true})),
            "host.storage.get" => {
                let key = storage_key(params)?;
                let scope = self.storage_scope(params)?;
                Ok(json!({
                    "value": self.store.plugin_kv_get(&self.name, &scope, &key)?
                }))
            }
            "host.storage.set" => {
                let key = storage_key(params)?;
                let value = params
                    .get("value")
                    .and_then(Value::as_str)
                    .context("host.storage.set needs a string `value`")?;
                if value.len() > MAX_STORAGE_VALUE {
                    bail!("storage value exceeds {MAX_STORAGE_VALUE} bytes");
                }
                let scope = self.storage_scope(params)?;
                self.store.plugin_kv_set(&self.name, &scope, &key, value)?;
                Ok(json!({"ok": true}))
            }
            "host.storage.delete" => {
                let key = storage_key(params)?;
                let scope = self.storage_scope(params)?;
                Ok(json!({
                    "deleted": self.store.plugin_kv_delete(&self.name, &scope, &key)?
                }))
            }
            "host.storage.list" => {
                let scope = self.storage_scope(params)?;
                Ok(json!({
                    "items": self
                        .store
                        .plugin_kv_list(&self.name, &scope)?
                        .into_iter()
                        .map(|(key, value)| json!({"key": key, "value": value}))
                        .collect::<Vec<_>>()
                }))
            }
            "host.fs.read" => self.fs_read(params),
            "host.fs.write" => self.fs_write(params),
            "host.fs.list" => self.fs_list(params),
            "host.net.request" => self.net_request(params).await,
            "host.secrets.get" => self.secret_get(params),
            "host.session.get" => self.session_get(params),
            "host.session.list" => self.session_list(params),
            "host.ui.open" => self.ui_event("plugin.ui.open", params),
            "host.ui.update" => self.ui_event("plugin.ui.update", params),
            "host.ui.close" => self.ui_close(params),
            "host.notify" => self.notify(params),
            "host.rpc.call" => self.rpc_call(params).await,
            "host.blob.put" => self.blob_put(params),
            "host.blob.get" => self.blob_get(params),
            "host.job.register" => self.job_register(params).await,
            "host.job.trigger" => self.job_trigger(params).await,
            "host.job.unregister" => self.job_unregister(params).await,
            other => bail!("unknown host method {other:?}"),
        }
    }

    /// Support `host.log` sent as a notification (no response expected).
    pub fn log_notification(&self, value: &Value) {
        let params = value.get("params").cloned().unwrap_or(Value::Null);
        self.write_log(&params);
    }

    fn write_log(&self, params: &Value) {
        let level = params
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("info");
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        eprintln!(
            "eix-plugin[{}] {level}: {}",
            self.name,
            redact(&cap(message, 4096))
        );
    }

    fn storage_scope(&self, params: &Value) -> Result<String> {
        let scope = params
            .get("scope")
            .and_then(Value::as_str)
            .unwrap_or("global");
        match scope {
            "global" => Ok("global".to_string()),
            "workspace" => {
                let workspace = params
                    .get("workspace")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .unwrap_or_else(|| self.workspace.clone());
                if !same_path(&workspace, &self.workspace) {
                    bail!("workspace scope must name the plugin's workspace");
                }
                Ok(format!("workspace:{}", workspace.display()))
            }
            other => bail!("unknown storage scope {other:?}; use global or workspace"),
        }
    }

    // ---- filesystem -----------------------------------------------------

    fn fs_read(&self, params: &Value) -> Result<Value> {
        let path = self.resolve_existing(params, "path")?;
        let meta =
            std::fs::metadata(&path).with_context(|| format!("cannot stat {}", path.display()))?;
        if !meta.is_file() {
            bail!("{} is not a regular file", path.display());
        }
        if meta.len() as usize > MAX_FS_FILE {
            bail!("file exceeds {MAX_FS_FILE} bytes");
        }
        let bytes =
            std::fs::read(&path).with_context(|| format!("cannot read {}", path.display()))?;
        Ok(match String::from_utf8(bytes) {
            Ok(text) => json!({
                "path": self.relative(&path),
                "content": text,
                "encoding": "utf8",
                "bytes": text.len(),
            }),
            Err(err) => {
                let bytes = err.into_bytes();
                json!({
                    "path": self.relative(&path),
                    "content": base64::engine::general_purpose::STANDARD.encode(&bytes),
                    "encoding": "base64",
                    "bytes": bytes.len(),
                })
            }
        })
    }

    fn fs_write(&self, params: &Value) -> Result<Value> {
        let raw = params
            .get("path")
            .and_then(Value::as_str)
            .context("host.fs.write needs a string `path`")?;
        let path = self.resolve_for_write(raw)?;
        let content = params
            .get("content")
            .and_then(Value::as_str)
            .context("host.fs.write needs a string `content`")?;
        let bytes = match params.get("encoding").and_then(Value::as_str) {
            None | Some("utf8") => content.as_bytes().to_vec(),
            Some("base64") => base64::engine::general_purpose::STANDARD
                .decode(content)
                .context("invalid base64 content")?,
            Some(other) => bail!("unknown encoding {other:?}; use utf8 or base64"),
        };
        if bytes.len() > MAX_FS_FILE {
            bail!("content exceeds {MAX_FS_FILE} bytes");
        }
        std::fs::write(&path, &bytes)
            .with_context(|| format!("cannot write {}", path.display()))?;
        Ok(json!({
            "path": self.relative(&path),
            "bytes": bytes.len(),
        }))
    }

    fn fs_list(&self, params: &Value) -> Result<Value> {
        let path = match params.get("path").and_then(Value::as_str) {
            Some(raw) if !raw.trim().is_empty() => self.resolve_existing(params, "path")?,
            _ => self.canonical_workspace()?,
        };
        if !path.is_dir() {
            bail!("{} is not a directory", path.display());
        }
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(&path)
            .with_context(|| format!("cannot list {}", path.display()))?
            .take(MAX_FS_LIST)
        {
            let Ok(entry) = entry else { continue };
            let Ok(meta) = entry.metadata() else {
                // `metadata` follows symlinks; broken links are still listed.
                entries.push(json!({
                    "name": entry.file_name().to_string_lossy(),
                    "kind": "unknown",
                }));
                continue;
            };
            let kind = if meta.is_dir() {
                "dir"
            } else if meta.is_file() {
                "file"
            } else {
                "other"
            };
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "kind": kind,
                "size": meta.len(),
            }));
        }
        entries.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
        Ok(json!({
            "path": self.relative(&path),
            "entries": entries,
        }))
    }

    fn canonical_workspace(&self) -> Result<PathBuf> {
        self.workspace
            .canonicalize()
            .with_context(|| format!("cannot resolve workspace {}", self.workspace.display()))
    }

    fn resolve_existing(&self, params: &Value, key: &str) -> Result<PathBuf> {
        let raw = params
            .get(key)
            .and_then(Value::as_str)
            .with_context(|| format!("host.fs method needs a string `{key}`"))?;
        let path = self.resolve_for_write(raw)?;
        if !path.exists() {
            bail!("{} does not exist", path.display());
        }
        let path = path
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", path.display()))?;
        self.ensure_inside(&path)?;
        Ok(path)
    }

    fn resolve_for_write(&self, raw: &str) -> Result<PathBuf> {
        if raw.trim().is_empty() {
            bail!("path must not be empty");
        }
        let raw_path = Path::new(raw);
        if raw_path
            .components()
            .any(|part| matches!(part, Component::ParentDir))
        {
            bail!("path must not contain `..`");
        }
        let path = if raw_path.is_absolute() {
            raw_path.to_path_buf()
        } else {
            self.workspace.join(raw_path)
        };
        let parent = path.parent().context("path has no parent")?;
        let parent = parent
            .canonicalize()
            .with_context(|| format!("cannot resolve {}", parent.display()))?;
        self.ensure_inside(&parent)?;
        let path = parent.join(path.file_name().context("path has no file name")?);
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if meta.file_type().is_symlink() {
                bail!("refusing to follow symlink {}", path.display());
            }
        }
        Ok(path)
    }

    fn ensure_inside(&self, path: &Path) -> Result<()> {
        let root = self.canonical_workspace()?;
        if !path.starts_with(&root) {
            bail!("path {} is outside the workspace", path.display());
        }
        Ok(())
    }

    fn relative(&self, path: &Path) -> String {
        path.strip_prefix(&self.workspace)
            .unwrap_or(path)
            .display()
            .to_string()
    }

    // ---- network --------------------------------------------------------

    async fn net_request(&self, params: &Value) -> Result<Value> {
        let raw = params
            .get("url")
            .and_then(Value::as_str)
            .context("host.net.request needs a string `url`")?;
        let url = reqwest::Url::parse(raw).context("invalid URL")?;
        if url.scheme() != "https" {
            bail!("only https URLs are allowed");
        }
        if !url.username().is_empty() || url.password().is_some() {
            bail!("URL credentials are not allowed; use host.secrets");
        }
        let host = url.host_str().context("URL has no host")?;
        if !self
            .network_allow
            .iter()
            .any(|pattern| host_matches(pattern, host))
        {
            bail!(
                "host {host:?} is not in plugins.{}.network_allow",
                self.name
            );
        }
        let method = params
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("GET")
            .to_ascii_uppercase();
        let method = match method.as_str() {
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" => {
                reqwest::Method::from_bytes(method.as_bytes()).context("invalid method")?
            }
            other => bail!("method {other:?} is not allowed"),
        };
        let body = params
            .get("body")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if body.len() > MAX_NET_BODY {
            bail!("request body exceeds {MAX_NET_BODY} bytes");
        }
        let timeout = params
            .get("timeout_ms")
            .and_then(Value::as_u64)
            .map(Duration::from_millis)
            .unwrap_or(NET_TIMEOUT)
            .min(NET_TIMEOUT);
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("cannot create HTTP client")?;
        let mut request = client.request(method.clone(), url);
        if let Some(headers) = params.get("headers").and_then(Value::as_object) {
            if headers.len() > 32 {
                bail!("too many request headers");
            }
            for (name, value) in headers {
                let lower = name.to_ascii_lowercase();
                if matches!(
                    lower.as_str(),
                    "host" | "cookie" | "connection" | "content-length" | "transfer-encoding"
                ) {
                    bail!("header {name:?} is not allowed");
                }
                let Some(value) = value.as_str() else {
                    bail!("header {name:?} must be a string");
                };
                request = request.header(name, value);
            }
        }
        if !body.is_empty() {
            request = request.body(body.to_string());
        }
        let response = request.send().await.context("request failed")?;
        let status = response.status();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                (
                    name.as_str().to_string(),
                    value.to_str().unwrap_or_default().to_string(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let content_type = headers
            .get("content-type")
            .cloned()
            .unwrap_or_else(|| "application/octet-stream".into());
        let mut bytes = Vec::new();
        let mut truncated = false;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("cannot read response body")?;
            if bytes.len() + chunk.len() > MAX_NET_RESPONSE {
                let keep = MAX_NET_RESPONSE.saturating_sub(bytes.len());
                bytes.extend_from_slice(&chunk[..keep]);
                truncated = true;
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
        let text_like = content_type.starts_with("text/")
            || content_type.contains("json")
            || content_type.contains("xml")
            || content_type.contains("yaml")
            || content_type.contains("javascript");
        let mut result = json!({
            "status": status.as_u16(),
            "headers": headers,
            "bytes": bytes.len(),
            "truncated": truncated,
        });
        if text_like {
            result["content"] = Value::String(String::from_utf8_lossy(&bytes).to_string());
            result["encoding"] = Value::String("utf8".into());
        } else {
            result["content"] =
                Value::String(base64::engine::general_purpose::STANDARD.encode(&bytes));
            result["encoding"] = Value::String("base64".into());
        }
        Ok(result)
    }

    // ---- secrets --------------------------------------------------------

    fn secret_get(&self, params: &Value) -> Result<Value> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .context("host.secrets.get needs a string `name`")?;
        if !self.secrets.iter().any(|allowed| allowed == name) {
            bail!("secret {name:?} is not in plugins.{}.secrets", self.name);
        }
        let value = self
            .env
            .get(name)
            .cloned()
            .or_else(|| std::env::var(name).ok())
            .with_context(|| format!("secret {name:?} is not set"))?;
        // Never log the value; the audit line only names the variable.
        eprintln!("eix-plugin[{}] secret read: {name}", self.name);
        Ok(json!({"name": name, "value": value}))
    }

    // ---- sessions -------------------------------------------------------

    fn session_get(&self, params: &Value) -> Result<Value> {
        let id = params
            .get("session_id")
            .and_then(Value::as_str)
            .context("host.session.get needs a string `session_id`")?;
        let session = self
            .store
            .get_session(id)?
            .with_context(|| format!("unknown session {id:?}"))?;
        serde_json::to_value(session).context("cannot encode session")
    }

    fn session_list(&self, params: &Value) -> Result<Value> {
        let include_archived = params
            .get("include_archived")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let sessions = self.store.list_sessions_including(include_archived)?;
        let rows = sessions
            .into_iter()
            .take(500)
            .map(|session| {
                json!({
                    "id": session.id,
                    "name": session.name,
                    "cwd": session.cwd,
                    "status": session.status,
                    "origin": session.origin,
                    "mode": session.mode,
                    "archived": session.archived,
                    "updated_at": session.updated_at,
                })
            })
            .collect::<Vec<_>>();
        Ok(json!({"sessions": rows}))
    }

    // ---- UI -------------------------------------------------------------

    fn ui_event(&self, method: &str, params: &Value) -> Result<Value> {
        let id = params
            .get("id")
            .and_then(Value::as_str)
            .context("host.ui method needs a string `id`")?;
        if id.trim().is_empty() || id.len() > 128 {
            bail!("UI id must be 1..=128 bytes");
        }
        let title = params
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let surface = params
            .get("surface")
            .and_then(Value::as_str)
            .unwrap_or("panel");
        if !eix_core::plugin::valid_ui_surface(surface) {
            bail!("unknown UI surface {surface:?}");
        }
        let body = match params.get("body") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(lines)) => lines
                .iter()
                .map(|line| line.as_str().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n"),
            None => String::new(),
            Some(_) => bail!("UI body must be a string or an array of strings"),
        };
        if body.len() > 256 * 1024 {
            bail!("UI body is too large");
        }
        self.emit(
            method,
            json!({
                "plugin": self.name,
                "id": id,
                "surface": surface,
                "title": title,
                "body": body,
            }),
        );
        Ok(json!({"ok": true}))
    }

    fn ui_close(&self, params: &Value) -> Result<Value> {
        let id = params
            .get("id")
            .and_then(Value::as_str)
            .context("host.ui.close needs a string `id`")?;
        self.emit("plugin.ui.close", json!({"plugin": self.name, "id": id}));
        Ok(json!({"ok": true}))
    }

    fn notify(&self, params: &Value) -> Result<Value> {
        let level = params
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("info");
        let message = params
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let message = cap(message, 4096);
        eprintln!("eix-plugin[{}] {level}: {}", self.name, redact(&message));
        self.emit(
            "plugin.notify",
            json!({"plugin": self.name, "level": level, "message": message}),
        );
        Ok(json!({"ok": true}))
    }

    fn emit(&self, method: &str, params: Value) {
        let _ = self.bus.send(Notification::from_value(method, params));
    }

    // ---- plugin RPC -----------------------------------------------------

    async fn rpc_call(&self, params: &Value) -> Result<Value> {
        let target = params
            .get("plugin")
            .or_else(|| params.get("target"))
            .and_then(Value::as_str)
            .context("host.rpc.call needs a string `plugin`")?;
        let method = params
            .get("method")
            .and_then(Value::as_str)
            .context("host.rpc.call needs a string `method`")?;
        if !eix_core::plugin::valid_plugin_name(target) {
            bail!("invalid target plugin {target:?}");
        }
        if target == self.name {
            bail!("a plugin cannot call itself");
        }
        if !eix_core::plugin::valid_tool_name(method) {
            bail!("invalid RPC method {method:?}");
        }
        if !self
            .rpc_allow
            .iter()
            .any(|allowed| allowed == "*" || wildcard(allowed, target))
        {
            bail!(
                "plugin {target:?} is not in plugins.{}.rpc_allow",
                self.name
            );
        }
        let depth = params.get("depth").and_then(Value::as_u64).unwrap_or(0) as u32;
        if depth >= MAX_RPC_DEPTH {
            bail!("RPC call depth limit ({MAX_RPC_DEPTH}) exceeded");
        }
        let (reply, rx) = oneshot::channel();
        self.rpc_tx
            .send(RpcCallRequest {
                from: self.name.clone(),
                target: target.to_string(),
                method: method.to_string(),
                params: params
                    .get("arguments")
                    .or_else(|| params.get("params"))
                    .cloned()
                    .unwrap_or(Value::Null),
                depth: depth + 1,
                reply,
            })
            .await
            .context("plugin RPC router is not running")?;
        match tokio::time::timeout(RPC_TIMEOUT, rx).await {
            Ok(Ok(Ok(value))) => Ok(value),
            Ok(Ok(Err(err))) => bail!(err),
            Ok(Err(_)) => bail!("plugin RPC router dropped the request"),
            Err(_) => bail!("plugin RPC timed out"),
        }
    }

    // ---- blobs ----------------------------------------------------------

    fn blob_put(&self, params: &Value) -> Result<Value> {
        let content = params
            .get("data")
            .or_else(|| params.get("content"))
            .and_then(Value::as_str)
            .context("host.blob.put needs a string `data`")?;
        let bytes = match params.get("encoding").and_then(Value::as_str) {
            None | Some("utf8") => content.as_bytes().to_vec(),
            Some("base64") => base64::engine::general_purpose::STANDARD
                .decode(content)
                .context("invalid base64 data")?,
            Some(other) => bail!("unknown encoding {other:?}; use utf8 or base64"),
        };
        if bytes.len() > MAX_BLOB {
            bail!("blob exceeds {MAX_BLOB} bytes");
        }
        let id = hex_digest(&bytes);
        let path = self.blob_path(&id)?;
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create {}", parent.display()))?;
            }
            let temp = path.with_extension("tmp");
            std::fs::write(&temp, &bytes)
                .with_context(|| format!("cannot write {}", temp.display()))?;
            std::fs::rename(&temp, &path)
                .with_context(|| format!("cannot commit {}", path.display()))?;
        }
        Ok(json!({"id": id, "bytes": bytes.len()}))
    }

    fn blob_get(&self, params: &Value) -> Result<Value> {
        let id = params
            .get("id")
            .and_then(Value::as_str)
            .context("host.blob.get needs a string `id`")?;
        let path = self.blob_path(id)?;
        let meta = std::fs::metadata(&path).with_context(|| format!("unknown blob {id:?}"))?;
        if meta.len() as usize > MAX_BLOB {
            bail!("blob exceeds {MAX_BLOB} bytes");
        }
        let bytes = std::fs::read(&path).with_context(|| format!("cannot read blob {id:?}"))?;
        Ok(json!({
            "id": id,
            "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "encoding": "base64",
            "bytes": bytes.len(),
        }))
    }

    fn blob_path(&self, id: &str) -> Result<PathBuf> {
        if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("blob id must be a 64 character SHA-256 hex digest");
        }
        Ok(self
            .data_dir
            .join("blobs")
            .join(&id[..2])
            .join(id.to_ascii_lowercase()))
    }

    // ---- jobs -----------------------------------------------------------

    async fn job_register(&self, params: &Value) -> Result<Value> {
        let id = params
            .get("id")
            .and_then(Value::as_str)
            .context("host.job.register needs a string `id`")?;
        validate_job_id(id)?;
        let interval = params
            .get("interval_secs")
            .and_then(Value::as_u64)
            .unwrap_or(60)
            .clamp(1, 86_400);
        let payload = params.get("payload").cloned().unwrap_or(Value::Null);
        let name = self.name.clone();
        let frame_tx = self.frame_tx.clone();
        let job_id = id.to_string();
        let handle = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(interval));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // The first tick fires immediately; register should not.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let notification = RpcNotification::new(
                    "job.run",
                    json!({"id": job_id, "plugin": name, "payload": payload}),
                );
                let Ok(frame) = serde_json::to_value(notification) else {
                    break;
                };
                if frame_tx.send(frame).await.is_err() {
                    break;
                }
            }
        });
        let mut jobs = self.jobs.lock().await;
        if !jobs.contains_key(id) && jobs.len() >= MAX_JOBS {
            handle.abort();
            bail!("at most {MAX_JOBS} jobs may be registered");
        }
        if let Some(previous) = jobs.insert(id.to_string(), handle) {
            previous.abort();
        }
        Ok(json!({"id": id, "interval_secs": interval}))
    }

    async fn job_trigger(&self, params: &Value) -> Result<Value> {
        let id = params
            .get("id")
            .and_then(Value::as_str)
            .context("host.job.trigger needs a string `id`")?;
        validate_job_id(id)?;
        if !self.jobs.lock().await.contains_key(id) {
            bail!("unknown job {id:?}");
        }
        let payload = params.get("payload").cloned().unwrap_or(Value::Null);
        let notification = RpcNotification::new(
            "job.run",
            json!({"id": id, "plugin": self.name, "payload": payload, "manual": true}),
        );
        self.frame_tx
            .send(serde_json::to_value(notification)?)
            .await
            .context("plugin is not running")?;
        Ok(json!({"triggered": id}))
    }

    async fn job_unregister(&self, params: &Value) -> Result<Value> {
        let id = params
            .get("id")
            .and_then(Value::as_str)
            .context("host.job.unregister needs a string `id`")?;
        let removed = self.jobs.lock().await.remove(id);
        let had = removed.is_some();
        if let Some(handle) = removed {
            handle.abort();
        }
        Ok(json!({"removed": had}))
    }
}

fn known_host_method(method: &str) -> bool {
    matches!(
        method,
        "host.log"
            | "host.info"
            | "host.ping"
            | "host.storage.get"
            | "host.storage.set"
            | "host.storage.delete"
            | "host.storage.list"
            | "host.fs.read"
            | "host.fs.write"
            | "host.fs.list"
            | "host.net.request"
            | "host.secrets.get"
            | "host.session.get"
            | "host.session.list"
            | "host.ui.open"
            | "host.ui.update"
            | "host.ui.close"
            | "host.notify"
            | "host.rpc.call"
            | "host.blob.put"
            | "host.blob.get"
            | "host.job.register"
            | "host.job.trigger"
            | "host.job.unregister"
    )
}

pub(super) fn required_capability(method: &str) -> Option<PluginCapability> {
    match method {
        "host.storage.get"
        | "host.storage.set"
        | "host.storage.delete"
        | "host.storage.list"
        | "host.blob.put"
        | "host.blob.get" => Some(PluginCapability::Storage),
        "host.fs.read" | "host.fs.list" => Some(PluginCapability::WorkspaceRead),
        "host.fs.write" => Some(PluginCapability::WorkspaceWrite),
        "host.net.request" => Some(PluginCapability::Network),
        "host.secrets.get" => Some(PluginCapability::Secrets),
        "host.session.get" | "host.session.list" => Some(PluginCapability::SessionsRead),
        "host.ui.open" | "host.ui.update" | "host.ui.close" | "host.notify" => {
            Some(PluginCapability::Ui)
        }
        "host.rpc.call" => Some(PluginCapability::Rpc),
        "host.job.register" | "host.job.trigger" | "host.job.unregister" => {
            Some(PluginCapability::Jobs)
        }
        _ => None,
    }
}

fn storage_key(params: &Value) -> Result<String> {
    let key = params
        .get("key")
        .and_then(Value::as_str)
        .context("storage method needs a string `key`")?;
    if key.is_empty() || key.len() > MAX_STORAGE_KEY {
        bail!("storage key must be 1..={MAX_STORAGE_KEY} bytes");
    }
    if key.chars().any(char::is_control) {
        bail!("storage key must not contain control characters");
    }
    Ok(key.to_string())
}

fn validate_job_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 128 {
        bail!("job id must be 1..=128 bytes");
    }
    if id.contains("..") || id.starts_with('/') {
        bail!("job id must not contain `..` or start with `/`");
    }
    if !id
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '/'))
    {
        bail!("job id contains unsupported characters");
    }
    Ok(())
}

fn same_path(left: &Path, right: &Path) -> bool {
    fn normalize(path: &Path) -> PathBuf {
        path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    }
    normalize(left) == normalize(right)
}

fn host_matches(pattern: &str, host: &str) -> bool {
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let pattern = pattern.trim().to_ascii_lowercase();
    if let Some(suffix) = pattern.strip_prefix("*.") {
        host.len() > suffix.len()
            && host.ends_with(suffix)
            && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
    } else {
        pattern == host
    }
}

fn wildcard(pattern: &str, text: &str) -> bool {
    fn walk(pattern: &[u8], text: &[u8]) -> bool {
        match pattern.split_first() {
            None => text.is_empty(),
            Some((b'*', rest)) => {
                walk(rest, text) || (!text.is_empty() && walk(pattern, &text[1..]))
            }
            Some((b'?', rest)) => !text.is_empty() && walk(rest, &text[1..]),
            Some((byte, rest)) => text.first() == Some(byte) && walk(rest, &text[1..]),
        }
    }
    walk(pattern.as_bytes(), text.as_bytes())
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn cap(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_allow_matches_exact_and_wildcard_hosts() {
        assert!(host_matches("api.example.com", "API.example.com"));
        assert!(host_matches("*.example.com", "api.example.com"));
        assert!(!host_matches("*.example.com", "example.com"));
        assert!(!host_matches("api.example.com", "evil.example.com"));
    }

    #[test]
    fn storage_keys_are_bounded() {
        assert!(storage_key(&json!({"key": "a"})).is_ok());
        assert!(storage_key(&json!({"key": ""})).is_err());
        assert!(storage_key(&json!({"key": "a\nb"})).is_err());
        assert!(storage_key(&json!({"key": "a".repeat(MAX_STORAGE_KEY + 1)})).is_err());
        assert!(storage_key(&json!({})).is_err());
    }

    #[test]
    fn job_ids_are_bounded() {
        assert!(validate_job_id("sync.issues").is_ok());
        assert!(validate_job_id("").is_err());
        assert!(validate_job_id("../escape").is_err());
    }

    #[test]
    fn capabilities_are_required_per_method() {
        assert_eq!(
            required_capability("host.fs.write"),
            Some(PluginCapability::WorkspaceWrite)
        );
        assert_eq!(
            required_capability("host.net.request"),
            Some(PluginCapability::Network)
        );
        assert_eq!(required_capability("host.log"), None);
    }

    fn context(capabilities: &[PluginCapability]) -> (HostContext, mpsc::Receiver<Value>) {
        context_with(
            capabilities,
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp/plugins/acme"),
            Vec::new(),
            BTreeMap::new(),
        )
    }

    fn context_with(
        capabilities: &[PluginCapability],
        workspace: PathBuf,
        data_dir: PathBuf,
        secrets: Vec<String>,
        env: BTreeMap<String, String>,
    ) -> (HostContext, mpsc::Receiver<Value>) {
        let (tx, rx) = mpsc::channel(8);
        let (bus, _) = broadcast::channel(8);
        let (rpc_tx, _rpc_rx) = mpsc::channel(8);
        let host = HostContext::new(
            "acme".into(),
            capabilities.iter().copied().collect::<BTreeSet<_>>(),
            workspace,
            Arc::new(Store::in_memory().unwrap()),
            tx,
            data_dir,
            Vec::new(),
            secrets,
            Vec::new(),
            env,
            bus,
            rpc_tx,
        );
        (host, rx)
    }

    async fn call(
        host: &HostContext,
        rx: &mut mpsc::Receiver<Value>,
        method: &str,
        params: Value,
    ) -> RpcResponse {
        host.handle(RpcRequest::new(1, method, params)).await;
        serde_json::from_value(rx.recv().await.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn storage_round_trips_with_the_capability() {
        let (host, mut rx) = context(&[PluginCapability::Storage]);
        let response = call(
            &host,
            &mut rx,
            "host.storage.set",
            json!({"scope": "global", "key": "k", "value": "v"}),
        )
        .await;
        assert!(response.error.is_none());
        let response = call(
            &host,
            &mut rx,
            "host.storage.get",
            json!({"scope": "global", "key": "k"}),
        )
        .await;
        assert_eq!(response.result.unwrap()["value"], "v");
        let response = call(
            &host,
            &mut rx,
            "host.storage.delete",
            json!({"scope": "global", "key": "k"}),
        )
        .await;
        assert_eq!(response.result.unwrap()["deleted"], true);
    }

    #[tokio::test]
    async fn storage_without_the_capability_is_rejected() {
        let (host, mut rx) = context(&[]);
        let response = call(
            &host,
            &mut rx,
            "host.storage.get",
            json!({"scope": "global", "key": "k"}),
        )
        .await;
        assert_eq!(
            response.error.expect("error").code,
            error_code::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn reserved_methods_no_longer_report_unsupported() {
        let (host, mut rx) = context(&[]);
        let response = call(&host, &mut rx, "host.fs.read", json!({})).await;
        assert_eq!(
            response.error.expect("error").code,
            error_code::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn fs_access_is_jailed_to_the_workspace() {
        let root = std::env::temp_dir().join(format!("eix-host-fs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let (tx, mut rx) = mpsc::channel(8);
        let (bus, _) = broadcast::channel(8);
        let (rpc_tx, _rpc_rx) = mpsc::channel(8);
        let host = HostContext::new(
            "acme".into(),
            [
                PluginCapability::WorkspaceRead,
                PluginCapability::WorkspaceWrite,
            ]
            .into_iter()
            .collect(),
            root.clone(),
            Arc::new(Store::in_memory().unwrap()),
            tx,
            root.join("data"),
            Vec::new(),
            Vec::new(),
            Vec::new(),
            BTreeMap::new(),
            bus,
            rpc_tx,
        );
        let response = call(
            &host,
            &mut rx,
            "host.fs.write",
            json!({"path": "notes/a.txt", "content": "hello"}),
        )
        .await;
        assert_eq!(
            response.error.expect("error").code,
            error_code::INVALID_PARAMS
        );
        std::fs::create_dir_all(root.join("notes")).unwrap();
        let response = call(
            &host,
            &mut rx,
            "host.fs.write",
            json!({"path": "notes/a.txt", "content": "hello"}),
        )
        .await;
        assert!(response.error.is_none(), "{:?}", response.error);
        let response = call(
            &host,
            &mut rx,
            "host.fs.read",
            json!({"path": "notes/a.txt"}),
        )
        .await;
        assert_eq!(response.result.unwrap()["content"], "hello");
        let response = call(&host, &mut rx, "host.fs.read", json!({"path": "../escape"})).await;
        assert_eq!(
            response.error.expect("error").code,
            error_code::INVALID_PARAMS
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn secrets_require_an_explicit_allowlist() {
        let env = BTreeMap::from([("ACME_TOKEN".to_string(), "s3cret".to_string())]);
        let (host, mut rx) = context_with(
            &[PluginCapability::Secrets],
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp/plugins/acme"),
            Vec::new(),
            env.clone(),
        );
        let response = call(
            &host,
            &mut rx,
            "host.secrets.get",
            json!({"name": "ACME_TOKEN"}),
        )
        .await;
        assert_eq!(
            response.error.expect("error").code,
            error_code::INVALID_PARAMS
        );

        let (host, mut rx) = context_with(
            &[PluginCapability::Secrets],
            PathBuf::from("/tmp"),
            PathBuf::from("/tmp/plugins/acme"),
            vec!["ACME_TOKEN".into()],
            env,
        );
        let response = call(
            &host,
            &mut rx,
            "host.secrets.get",
            json!({"name": "ACME_TOKEN"}),
        )
        .await;
        assert_eq!(response.result.unwrap()["value"], "s3cret");
    }

    #[tokio::test]
    async fn blobs_are_content_addressed_and_scoped_to_the_plugin() {
        let root = std::env::temp_dir().join(format!("eix-host-blob-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let (host, mut rx) = context_with(
            &[PluginCapability::Storage],
            PathBuf::from("/tmp"),
            root.clone(),
            Vec::new(),
            BTreeMap::new(),
        );
        let response = call(
            &host,
            &mut rx,
            "host.blob.put",
            json!({"data": "hello", "encoding": "utf8"}),
        )
        .await;
        let result = response.result.expect("blob put");
        let id = result["id"].as_str().unwrap().to_string();
        assert_eq!(id.len(), 64);
        assert!(root.join("blobs").join(&id[..2]).join(&id).exists());

        let response = call(&host, &mut rx, "host.blob.get", json!({"id": id})).await;
        assert_eq!(response.result.unwrap()["data"], "aGVsbG8=");

        let response = call(
            &host,
            &mut rx,
            "host.blob.get",
            json!({"id": "../etc/passwd"}),
        )
        .await;
        assert_eq!(
            response.error.expect("error").code,
            error_code::INVALID_PARAMS
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
