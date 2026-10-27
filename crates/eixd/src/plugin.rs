//! Out-of-process plugin host.
//!
//! One child process per enabled server plugin. The wire format is
//! newline-delimited JSON, matching the daemon's own framing but with a
//! separate [`eix_core::plugin::PLUGIN_API_VERSION`]. Plugins advertise tools,
//! prompt commands and event subscriptions during `initialize`.
//!
//! The host enforces the capability grant from config. A plugin cannot add a
//! tool unless `tools` was granted, cannot subscribe to events without
//! `events`, and cannot touch storage without `storage`. This is a host API
//! boundary, not an OS sandbox: the child still runs as the daemon user.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use eix_core::plugin::{
    valid_plugin_name, valid_tool_name, PluginCapability, PluginCommand, PluginCommandInfo,
    PluginCommandList, PluginConfig, PluginInfo, PluginInitializeParams, PluginInitializeResult,
    PluginListResult, PluginManifest, PluginNotification, PluginRequest, PluginResponse,
    PluginTool, PluginToolCallParams, PluginToolCallResult, PLUGIN_API_VERSION,
    PLUGIN_COMMAND_PREFIX, PLUGIN_TOOL_PREFIX,
};
use eix_core::redact::redact;
use eix_core::{Config, ToolDefinition};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{mpsc, Mutex};

use crate::store::Store;
use crate::tools::ToolOutcome;

const DEFAULT_CAPABILITIES: &[PluginCapability] =
    &[PluginCapability::Tools, PluginCapability::Commands];
const MAX_PLUGIN_OUTPUT: usize = 1_000_000;
const MAX_STORAGE_KEY: usize = 512;
const MAX_STORAGE_VALUE: usize = 1_000_000;

#[derive(Debug, Clone)]
struct PluginEvent {
    method: String,
    params: Value,
}

/// Host-side view of one advertised tool.
#[derive(Debug, Clone)]
struct PluginToolRef {
    exposed: String,
    plugin: String,
    tool: String,
    description: String,
    schema: Value,
}

#[derive(Debug, Clone)]
struct PluginCommandRef {
    exposed: String,
    plugin: String,
    args: String,
    description: String,
    prompt: String,
}

#[derive(Debug, Clone)]
struct PluginStatus {
    name: String,
    enabled: bool,
    command: String,
    state: String,
    tools: usize,
    commands: usize,
    events: Vec<String>,
    capabilities: Vec<PluginCapability>,
    error: Option<String>,
}

impl PluginStatus {
    fn info(&self) -> PluginInfo {
        PluginInfo {
            name: self.name.clone(),
            enabled: self.enabled,
            command: self.command.clone(),
            state: self.state.clone(),
            tools: self.tools,
            commands: self.commands,
            events: self.events.clone(),
            capabilities: self.capabilities.clone(),
            error: self.error.clone(),
        }
    }
}

struct PluginSpec {
    name: String,
    version: String,
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: PathBuf,
    timeout: Duration,
    capabilities: Vec<PluginCapability>,
    allowed_tools: Vec<String>,
    denied_tools: Vec<String>,
}

impl PluginSpec {
    fn allows_tool(&self, name: &str) -> bool {
        if self
            .denied_tools
            .iter()
            .any(|pattern| wildcard(pattern, name))
        {
            return false;
        }
        self.allowed_tools.is_empty()
            || self
                .allowed_tools
                .iter()
                .any(|pattern| wildcard(pattern, name))
    }
}

/// One initialized plugin and its stdio channels.
struct PluginClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
    timeout: Duration,
    name: String,
    version: String,
    capabilities: BTreeSet<PluginCapability>,
    events: Vec<String>,
    workspace: PathBuf,
    store: Arc<Store>,
}

impl PluginClient {
    fn new(
        child: Child,
        stdin: ChildStdin,
        stdout: ChildStdout,
        spec: &PluginSpec,
        store: Arc<Store>,
        workspace: PathBuf,
    ) -> Self {
        Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
            timeout: spec.timeout,
            name: spec.name.clone(),
            version: spec.version.clone(),
            capabilities: spec.capabilities.iter().copied().collect(),
            events: Vec::new(),
            workspace,
            store,
        }
    }

    async fn write<T: Serialize>(&mut self, value: &T) -> Result<()> {
        let mut line = serde_json::to_string(value)?;
        line.push('\n');
        let timeout = self.timeout;
        let write = async {
            self.stdin.write_all(line.as_bytes()).await?;
            self.stdin.flush().await?;
            Ok::<(), std::io::Error>(())
        };
        tokio::time::timeout(timeout, write)
            .await
            .with_context(|| format!("plugin {}: write timed out", self.name))??;
        Ok(())
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&PluginRequest::new(id, method, params)).await?;

        let mut line = String::new();
        loop {
            line.clear();
            let read = tokio::time::timeout(self.timeout, self.stdout.read_line(&mut line))
                .await
                .with_context(|| format!("plugin {}: {method} timed out", self.name))??;
            if read == 0 {
                bail!("plugin {} closed stdout", self.name);
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(trimmed) {
                Ok(value) => value,
                Err(err) => {
                    // A plugin that logs to stdout is buggy, but one bad line
                    // should not take down the daemon. Keep waiting for the
                    // response and surface the line in the daemon log.
                    eprintln!(
                        "eixd: plugin {}: ignored non-JSON stdout: {} ({err})",
                        self.name,
                        redact(&cap(trimmed, 4096))
                    );
                    continue;
                }
            };
            if value.get("method").and_then(Value::as_str).is_some() {
                let Some(request_id) = value.get("id").and_then(Value::as_u64) else {
                    // Plugin notification. The host currently has no
                    // notification methods, so ignore it.
                    continue;
                };
                let host_method = value
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let host_params = value.get("params").cloned().unwrap_or(Value::Null);
                self.handle_host_request(request_id, &host_method, host_params)
                    .await?;
                continue;
            }
            if value.get("id").and_then(Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = value.get("error") {
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| error.as_str().unwrap_or("unknown plugin error"));
                bail!("plugin {}: {method} failed: {message}", self.name);
            }
            return Ok(value.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    async fn initialize(
        &mut self,
        params: PluginInitializeParams,
    ) -> Result<PluginInitializeResult> {
        let value = self
            .request("initialize", serde_json::to_value(params)?)
            .await?;
        let result: PluginInitializeResult = serde_json::from_value(value)
            .with_context(|| format!("plugin {}: invalid initialize result", self.name))?;
        if result.api_version != PLUGIN_API_VERSION {
            bail!(
                "plugin {}: API version {} is not supported (host expects {})",
                self.name,
                result.api_version,
                PLUGIN_API_VERSION
            );
        }
        if !result.name.is_empty() && result.name != self.name {
            bail!(
                "plugin {}: initialize returned name {:?}",
                self.name,
                result.name
            );
        }
        if !result.version.is_empty() {
            self.version = result.version.clone();
        }
        Ok(result)
    }

    async fn handle_host_request(&mut self, id: u64, method: &str, params: Value) -> Result<()> {
        let response = match method {
            "host.log" => {
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
                PluginResponse::ok(id, Value::Null)
            }
            "host.info" => PluginResponse::ok(
                id,
                json!({
                    "name": self.name,
                    "version": self.version,
                    "api_version": PLUGIN_API_VERSION,
                    "capabilities": self.capabilities,
                    "workspace": self.workspace,
                }),
            ),
            "host.ping" => PluginResponse::ok(id, json!({"pong": true})),
            "host.storage.get" => match self.storage_get(&params) {
                Ok(value) => PluginResponse::ok(id, json!({ "value": value })),
                Err(err) => PluginResponse::err(id, "storage_error", format!("{err:#}")),
            },
            "host.storage.set" => match self.storage_set(&params) {
                Ok(()) => PluginResponse::ok(id, json!({"ok": true})),
                Err(err) => PluginResponse::err(id, "storage_error", format!("{err:#}")),
            },
            "host.storage.delete" => match self.storage_delete(&params) {
                Ok(deleted) => PluginResponse::ok(id, json!({"deleted": deleted})),
                Err(err) => PluginResponse::err(id, "storage_error", format!("{err:#}")),
            },
            "host.storage.list" => match self.storage_list(&params) {
                Ok(items) => PluginResponse::ok(
                    id,
                    json!({
                        "items": items
                            .into_iter()
                            .map(|(key, value)| json!({"key": key, "value": value}))
                            .collect::<Vec<_>>()
                    }),
                ),
                Err(err) => PluginResponse::err(id, "storage_error", format!("{err:#}")),
            },
            other => {
                PluginResponse::err(id, "unsupported", format!("unknown host method {other:?}"))
            }
        };
        self.write(&response).await
    }

    fn require_storage(&self) -> Result<()> {
        if !self.capabilities.contains(&PluginCapability::Storage) {
            bail!(
                "plugin {} was not granted the storage capability",
                self.name
            );
        }
        Ok(())
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
                Ok(format!("workspace:{}", workspace.display()))
            }
            other => bail!("unknown storage scope {other:?}; use global or workspace"),
        }
    }

    fn storage_get(&self, params: &Value) -> Result<Option<String>> {
        self.require_storage()?;
        let key = storage_key(params)?;
        let scope = self.storage_scope(params)?;
        self.store.plugin_kv_get(&self.name, &scope, &key)
    }

    fn storage_set(&self, params: &Value) -> Result<()> {
        self.require_storage()?;
        let key = storage_key(params)?;
        let value = params
            .get("value")
            .and_then(Value::as_str)
            .context("host.storage.set needs a string `value`")?;
        if value.len() > MAX_STORAGE_VALUE {
            bail!("storage value exceeds {MAX_STORAGE_VALUE} bytes");
        }
        let scope = self.storage_scope(params)?;
        self.store.plugin_kv_set(&self.name, &scope, &key, value)
    }

    fn storage_delete(&self, params: &Value) -> Result<bool> {
        self.require_storage()?;
        let key = storage_key(params)?;
        let scope = self.storage_scope(params)?;
        self.store.plugin_kv_delete(&self.name, &scope, &key)
    }

    fn storage_list(&self, params: &Value) -> Result<Vec<(String, String)>> {
        self.require_storage()?;
        let scope = self.storage_scope(params)?;
        self.store.plugin_kv_list(&self.name, &scope)
    }

    async fn notify_event(&mut self, method: &str, params: &Value) -> Result<()> {
        if !self
            .events
            .iter()
            .any(|pattern| event_matches(pattern, method))
        {
            return Ok(());
        }
        self.write(&PluginNotification::new(method, params.clone()))
            .await
    }

    /// Return a redacted reason when the child is no longer running. `None`
    /// means it is still alive, so callers can keep using it.
    fn exit_reason(&mut self) -> Option<String> {
        match self.child.try_wait() {
            Ok(Some(status)) => Some(format!("plugin exited unexpectedly ({status})")),
            Ok(None) => None,
            Err(err) => Some(format!("plugin process check failed: {err}")),
        }
    }

    /// Best-effort graceful stop. A plugin that ignores the notification is
    /// killed when the client is dropped. The short timeout keeps daemon
    /// shutdown responsive even when a plugin is stuck.
    async fn shutdown(&mut self) {
        let notification = PluginNotification::new("shutdown", json!({}));
        let _ = tokio::time::timeout(Duration::from_millis(500), self.write(&notification)).await;
        let _ = tokio::time::timeout(Duration::from_millis(500), self.child.wait()).await;
    }
}

/// The daemon's plugin registry. Cloned `Arc`s of individual clients let tool
/// calls run concurrently while the map stays mutable for reloads.
pub struct PluginManager {
    store: Arc<Store>,
    data_dir: PathBuf,
    clients: Arc<Mutex<BTreeMap<String, Arc<Mutex<PluginClient>>>>>,
    tools: Mutex<Vec<PluginToolRef>>,
    commands: Mutex<Vec<PluginCommandRef>>,
    statuses: Mutex<BTreeMap<String, PluginStatus>>,
    event_tx: mpsc::Sender<PluginEvent>,
    /// False when no running plugin subscribed to any event. `publish` is
    /// called for every daemon notification, so this keeps the common case
    /// (no plugins, or plugins that only expose tools) from cloning payloads
    /// and waking the delivery task.
    has_event_subscribers: AtomicBool,
}

impl PluginManager {
    pub fn new(store: Arc<Store>, data_dir: PathBuf) -> Arc<Self> {
        // Bounded so a plugin event storm cannot grow memory without bound.
        // `publish` drops events under backpressure; events are notifications,
        // not durable state, and a slow plugin must not stall the daemon.
        let (event_tx, mut event_rx) = mpsc::channel::<PluginEvent>(1024);
        let clients: Arc<Mutex<BTreeMap<String, Arc<Mutex<PluginClient>>>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let event_clients = Arc::clone(&clients);
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let targets: Vec<_> = {
                    let clients = event_clients.lock().await;
                    clients.values().cloned().collect()
                };
                for client in targets {
                    // Never wait for a busy plugin: a tool call in flight must
                    // not delay event delivery to every other plugin. Events
                    // are best-effort, so dropping one is preferable to
                    // head-of-line blocking.
                    let Ok(mut client) = client.try_lock() else {
                        continue;
                    };
                    if let Err(err) = client.notify_event(&event.method, &event.params).await {
                        eprintln!(
                            "eixd: plugin {} event {} failed: {}",
                            client.name,
                            event.method,
                            redact(&format!("{err:#}"))
                        );
                    }
                }
            }
        });
        Arc::new(Self {
            store,
            data_dir,
            clients,
            tools: Mutex::new(Vec::new()),
            commands: Mutex::new(Vec::new()),
            statuses: Mutex::new(BTreeMap::new()),
            event_tx,
            has_event_subscribers: AtomicBool::new(false),
        })
    }

    /// Stop every plugin and start the enabled set from `config`. Safe to call
    /// on hot reload; a broken plugin is reported and skipped, never fatal.
    pub async fn load(&self, config: &Config) {
        self.has_event_subscribers.store(false, Ordering::Relaxed);
        self.clients.lock().await.clear();
        self.tools.lock().await.clear();
        self.commands.lock().await.clear();
        self.statuses.lock().await.clear();

        for (name, plugin) in &config.plugins {
            let status = PluginStatus {
                name: name.clone(),
                enabled: plugin.enabled,
                command: plugin.command.clone(),
                state: if plugin.enabled {
                    "starting"
                } else {
                    "stopped"
                }
                .to_string(),
                tools: 0,
                commands: 0,
                events: Vec::new(),
                capabilities: plugin.capabilities.clone(),
                error: None,
            };
            self.statuses.lock().await.insert(name.clone(), status);
            if !plugin.enabled {
                continue;
            }
            match resolve_spec(name, plugin, &config.load_cwd) {
                Ok(spec) => {
                    if let Err(err) = self.start_one(&spec, &config.load_cwd).await {
                        self.mark_failed(name, plugin, &err).await;
                    }
                }
                Err(err) => self.mark_failed(name, plugin, &err).await,
            }
        }
    }

    async fn start_one(&self, spec: &PluginSpec, workspace: &Path) -> Result<()> {
        let workspace = if workspace.as_os_str().is_empty() {
            spec.cwd.as_path()
        } else {
            workspace
        };
        // Give each plugin a dedicated scratch directory. Do not hand the
        // plugin the daemon's data directory (which contains the SQLite DB).
        let plugin_data_dir = self.data_dir.join("plugins").join(&spec.name);
        std::fs::create_dir_all(&plugin_data_dir)
            .with_context(|| format!("cannot create {}", plugin_data_dir.display()))?;
        let mut command = Command::new(&spec.command);
        command
            .args(&spec.args)
            .envs(&spec.env)
            .env("EIX_PLUGIN_NAME", &spec.name)
            .env("EIX_PLUGIN_API_VERSION", PLUGIN_API_VERSION.to_string())
            .env("EIX_PLUGIN_WORKSPACE", workspace)
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

        let mut client = PluginClient::new(
            child,
            stdin,
            stdout,
            spec,
            Arc::clone(&self.store),
            workspace.to_path_buf(),
        );
        let init = client
            .initialize(PluginInitializeParams {
                protocol_version: PLUGIN_API_VERSION,
                plugin: eix_core::plugin::PluginIdentity {
                    name: spec.name.clone(),
                    version: spec.version.clone(),
                },
                host: eix_core::plugin::PluginIdentity {
                    name: "eixd".to_string(),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
                capabilities: spec.capabilities.clone(),
                workspace: Some(workspace.to_path_buf()),
                config: json!({
                    "data_dir": plugin_data_dir,
                }),
            })
            .await
            .with_context(|| format!("plugin {} failed to initialize", spec.name))?;

        let capabilities: BTreeSet<PluginCapability> = spec.capabilities.iter().copied().collect();
        let tools = self.validate_tools(spec, &capabilities, &init)?;
        let commands = validate_commands(spec, &capabilities, &init)?;
        let events = validate_events(spec, &capabilities, &init)?;
        client.events = events.clone();

        {
            let tool_map = self.tools.lock().await;
            let mut seen = BTreeSet::new();
            for tool in &tools {
                if tool_map
                    .iter()
                    .any(|existing| existing.exposed == tool.exposed)
                    || !seen.insert(tool.exposed.clone())
                {
                    bail!(
                        "plugin {} advertises duplicate tool {:?}",
                        spec.name,
                        tool.exposed
                    );
                }
            }
        }
        let client = Arc::new(Mutex::new(client));
        self.tools.lock().await.extend(tools.iter().cloned());
        self.commands.lock().await.extend(commands.iter().cloned());
        self.clients.lock().await.insert(spec.name.clone(), client);
        if !events.is_empty() {
            self.has_event_subscribers.store(true, Ordering::Relaxed);
        }
        self.statuses.lock().await.insert(
            spec.name.clone(),
            PluginStatus {
                name: spec.name.clone(),
                enabled: true,
                command: spec.command.clone(),
                state: "running".to_string(),
                tools: tools.len(),
                commands: commands.len(),
                events,
                capabilities: spec.capabilities.clone(),
                error: None,
            },
        );
        Ok(())
    }

    fn validate_tools(
        &self,
        spec: &PluginSpec,
        capabilities: &BTreeSet<PluginCapability>,
        init: &PluginInitializeResult,
    ) -> Result<Vec<PluginToolRef>> {
        if !init.tools.is_empty() && !capabilities.contains(&PluginCapability::Tools) {
            bail!(
                "plugin {} advertises {} tool(s) but was not granted `tools`",
                spec.name,
                init.tools.len()
            );
        }
        let mut out = Vec::new();
        for tool in &init.tools {
            validate_tool(tool)?;
            if !spec.allows_tool(&tool.name) {
                continue;
            }
            out.push(PluginToolRef {
                exposed: format!("{PLUGIN_TOOL_PREFIX}{}__{}", spec.name, tool.name),
                plugin: spec.name.clone(),
                tool: tool.name.clone(),
                description: tool.description.clone(),
                schema: tool.input_schema.clone(),
            });
        }
        Ok(out)
    }

    async fn mark_failed(&self, name: &str, plugin: &PluginConfig, err: &anyhow::Error) {
        let message = redact(&format!("{err:#}"));
        eprintln!("eixd: plugin {name}: {message}");
        self.statuses.lock().await.insert(
            name.to_string(),
            PluginStatus {
                name: name.to_string(),
                enabled: plugin.enabled,
                command: plugin.command.clone(),
                state: "failed".to_string(),
                tools: 0,
                commands: 0,
                events: Vec::new(),
                capabilities: plugin.capabilities.clone(),
                error: Some(message),
            },
        );
    }

    /// Detect children that exited without a tool call in flight. This keeps
    /// `plugin.list`, command reloads, and tool definitions from advertising a
    /// dead process. Busy clients are skipped: their request will notice the
    /// broken pipe and fail on its own.
    async fn refresh(&self) {
        let clients = { self.clients.lock().await.clone() };
        let mut dead = Vec::new();
        for (name, client) in clients {
            let reason = match client.try_lock() {
                Ok(mut client) => client.exit_reason(),
                Err(_) => None,
            };
            if let Some(reason) = reason {
                dead.push((name, client, reason));
            }
        }
        for (name, client, reason) in dead {
            self.drop_client(&name, &client, Some(reason)).await;
        }
    }

    /// Remove `client` only if it is still the registered client for `name`.
    /// A concurrent restart may already have installed a replacement; in that
    /// case the old client's failure must not remove the new one or its tools.
    async fn drop_client(
        &self,
        name: &str,
        client: &Arc<Mutex<PluginClient>>,
        error: Option<String>,
    ) -> bool {
        let removed = {
            let mut clients = self.clients.lock().await;
            match clients.get(name) {
                Some(current) if Arc::ptr_eq(current, client) => {
                    clients.remove(name);
                    true
                }
                _ => false,
            }
        };
        if !removed {
            return false;
        }
        self.tools.lock().await.retain(|tool| tool.plugin != name);
        self.commands
            .lock()
            .await
            .retain(|command| command.plugin != name);
        if let Some(error) = error {
            if let Some(status) = self.statuses.lock().await.get_mut(name) {
                status.state = "failed".to_string();
                status.error = Some(redact(&error));
                status.tools = 0;
                status.commands = 0;
            }
        }
        true
    }

    pub async fn list(&self) -> PluginListResult {
        self.refresh().await;
        let statuses = self.statuses.lock().await;
        PluginListResult {
            plugins: statuses.values().map(PluginStatus::info).collect(),
        }
    }

    pub async fn tool_definitions(&self) -> Vec<ToolDefinition> {
        self.refresh().await;
        self.tools
            .lock()
            .await
            .iter()
            .map(|tool| {
                ToolDefinition::function(
                    tool.exposed.clone(),
                    tool.description.clone(),
                    tool.schema.clone(),
                )
            })
            .collect()
    }

    pub async fn knows(&self, exposed: &str) -> bool {
        self.tools
            .lock()
            .await
            .iter()
            .any(|tool| tool.exposed == exposed)
    }

    pub async fn commands(&self) -> PluginCommandList {
        self.refresh().await;
        let commands = self.commands.lock().await;
        PluginCommandList {
            commands: commands
                .iter()
                .map(|command| PluginCommandInfo {
                    plugin: command.plugin.clone(),
                    name: command.exposed.clone(),
                    args: command.args.clone(),
                    description: command.description.clone(),
                    prompt: command.prompt.clone(),
                })
                .collect(),
        }
    }

    pub async fn call(
        &self,
        exposed: &str,
        args: &Value,
        session_id: &str,
        cwd: &Path,
        max_output_bytes: usize,
    ) -> ToolOutcome {
        let started = Instant::now();
        let Some(tool) = self
            .tools
            .lock()
            .await
            .iter()
            .find(|tool| tool.exposed == exposed)
            .cloned()
        else {
            return ToolOutcome {
                ok: false,
                output: format!("unknown plugin tool {exposed:?}"),
                duration_ms: started.elapsed().as_millis() as u64,
            };
        };
        let Some(client) = self.clients.lock().await.get(&tool.plugin).cloned() else {
            return ToolOutcome {
                ok: false,
                output: format!("plugin {} is not running", tool.plugin),
                duration_ms: started.elapsed().as_millis() as u64,
            };
        };
        let params = PluginToolCallParams {
            tool: tool.tool,
            arguments: args.clone(),
            session_id: Some(session_id.to_string()),
            cwd: Some(cwd.to_path_buf()),
        };
        let result = {
            let mut client = client.lock().await;
            client
                .request(
                    "tools/call",
                    serde_json::to_value(params).unwrap_or(Value::Null),
                )
                .await
        };
        let duration_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(value) => match serde_json::from_value::<PluginToolCallResult>(value) {
                Ok(result) => ToolOutcome {
                    ok: result.ok,
                    output: cap(&result.output, max_output_bytes.min(MAX_PLUGIN_OUTPUT)),
                    duration_ms,
                },
                Err(err) => ToolOutcome {
                    ok: false,
                    output: format!(
                        "plugin {} returned an invalid tool result: {err}",
                        tool.plugin
                    ),
                    duration_ms,
                },
            },
            Err(err) => {
                let message = redact(&format!("plugin {} failed: {err:#}", tool.plugin));
                // A crashed or unresponsive child is unusable for later
                // calls. Drop it and its advertised contributions so the next
                // plugin.restart can recover cleanly and plugin.list is honest.
                self.drop_client(&tool.plugin, &client, Some(message.clone()))
                    .await;
                ToolOutcome {
                    ok: false,
                    output: message,
                    duration_ms,
                }
            }
        }
    }

    pub async fn restart(&self, name: &str, config: &Config) -> Result<()> {
        let Some(plugin) = config.plugins.get(name) else {
            bail!("unknown plugin {name:?}");
        };
        self.clients.lock().await.remove(name);
        self.tools.lock().await.retain(|tool| tool.plugin != name);
        self.commands
            .lock()
            .await
            .retain(|command| command.plugin != name);
        if !plugin.enabled {
            self.statuses.lock().await.insert(
                name.to_string(),
                PluginStatus {
                    name: name.to_string(),
                    enabled: false,
                    command: plugin.command.clone(),
                    state: "stopped".to_string(),
                    tools: 0,
                    commands: 0,
                    events: Vec::new(),
                    capabilities: plugin.capabilities.clone(),
                    error: None,
                },
            );
            return Ok(());
        }
        let spec = match resolve_spec(name, plugin, &config.load_cwd) {
            Ok(spec) => spec,
            Err(err) => {
                self.mark_failed(name, plugin, &err).await;
                return Err(anyhow::anyhow!(redact(&format!("{err:#}"))));
            }
        };
        if let Err(err) = self.start_one(&spec, &config.load_cwd).await {
            self.mark_failed(name, plugin, &err).await;
            return Err(anyhow::anyhow!(redact(&format!("{err:#}"))));
        }
        Ok(())
    }

    /// Fire-and-forget event delivery. A slow or crashed plugin must never
    /// block the daemon's turn loop or event bus.
    pub fn publish(&self, method: &str, params: &Value) {
        if !self.has_event_subscribers.load(Ordering::Relaxed) {
            return;
        }
        let _ = self.event_tx.try_send(PluginEvent {
            method: method.to_string(),
            params: params.clone(),
        });
    }

    pub async fn shutdown(&self) {
        self.has_event_subscribers.store(false, Ordering::Relaxed);
        // Give idle plugins a chance to flush their own state. A busy client
        // is skipped; dropping the client kills it, which is acceptable during
        // daemon shutdown and avoids waiting for a long tool timeout.
        let clients: Vec<_> = {
            let clients = self.clients.lock().await;
            clients.values().cloned().collect()
        };
        for client in clients {
            if let Ok(mut client) = client.try_lock() {
                client.shutdown().await;
            }
        }
        self.clients.lock().await.clear();
        self.tools.lock().await.clear();
        self.commands.lock().await.clear();
        let mut statuses = self.statuses.lock().await;
        for status in statuses.values_mut() {
            status.state = "stopped".to_string();
        }
    }
}

fn resolve_spec(name: &str, plugin: &PluginConfig, cwd: &Path) -> Result<PluginSpec> {
    if !valid_plugin_name(name) {
        bail!("invalid plugin name {name:?}");
    }
    // `Config::default()` has no load cwd (tests and embedded callers). Use
    // the process directory in that case instead of spawning with `cwd = ""`.
    let cwd = if cwd.as_os_str().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        cwd.to_path_buf()
    };
    let cwd = cwd.as_path();
    let manifest = plugin
        .manifest
        .as_ref()
        .map(|path| {
            let path = if path.is_absolute() {
                path.clone()
            } else {
                cwd.join(path)
            };
            PluginManifest::load(&path).map(|manifest| (path, manifest))
        })
        .transpose()?;
    if let Some((path, manifest)) = &manifest {
        if manifest.server.is_none() {
            bail!(
                "plugin {name} manifest {} has no [server] component",
                path.display()
            );
        }
    }

    let (command, args) = if !plugin.command.trim().is_empty() {
        (plugin.command.clone(), plugin.args.clone())
    } else if let Some((path, manifest)) = &manifest {
        let base = path.parent().unwrap_or(cwd);
        manifest
            .server_command(base)
            .with_context(|| format!("plugin {name} manifest has no [server] component"))?
    } else {
        bail!("plugin {name} needs `command` or `manifest`");
    };

    let requested = manifest
        .as_ref()
        .map(|(_, manifest)| manifest.capabilities.clone())
        .unwrap_or_default();
    let mut capabilities = if plugin.capabilities.is_empty() {
        if requested.is_empty() {
            DEFAULT_CAPABILITIES.to_vec()
        } else {
            requested.clone()
        }
    } else {
        plugin.capabilities.clone()
    };
    if manifest.is_some() {
        let before = capabilities.len();
        capabilities.retain(|capability| requested.contains(capability));
        if capabilities.len() < before {
            eprintln!("eixd: plugin {name}: ignored capabilities not requested by the manifest");
        }
    }
    capabilities.sort();
    capabilities.dedup();

    let plugin_cwd = plugin
        .cwd
        .as_ref()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                cwd.join(path)
            }
        })
        .unwrap_or_else(|| cwd.to_path_buf());
    let version = manifest
        .as_ref()
        .map(|(_, manifest)| manifest.version.clone())
        .unwrap_or_default();
    Ok(PluginSpec {
        name: name.to_string(),
        version,
        command,
        args,
        env: plugin.env.clone(),
        cwd: plugin_cwd,
        timeout: Duration::from_secs(plugin.timeout_secs.max(1)),
        capabilities,
        allowed_tools: plugin.allowed_tools.clone(),
        denied_tools: plugin.denied_tools.clone(),
    })
}

fn validate_tool(tool: &PluginTool) -> Result<()> {
    if !valid_tool_name(&tool.name) {
        bail!(
            "invalid tool name {:?}: use letters, digits, '.', '_' or '-', without '__'",
            tool.name
        );
    }
    if tool.description.len() > 2048 {
        bail!("tool {:?} description is too long", tool.name);
    }
    if !tool.input_schema.is_object() {
        bail!("tool {:?} input_schema must be a JSON object", tool.name);
    }
    Ok(())
}

fn validate_commands(
    spec: &PluginSpec,
    capabilities: &BTreeSet<PluginCapability>,
    init: &PluginInitializeResult,
) -> Result<Vec<PluginCommandRef>> {
    if !init.commands.is_empty() && !capabilities.contains(&PluginCapability::Commands) {
        bail!(
            "plugin {} advertises {} command(s) but was not granted `commands`",
            spec.name,
            init.commands.len()
        );
    }
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for command in &init.commands {
        validate_command(command)?;
        if !seen.insert(command.name.as_str()) {
            bail!(
                "plugin {} advertises duplicate command {:?}",
                spec.name,
                command.name
            );
        }
        let Some(prompt) = command
            .prompt
            .as_deref()
            .map(str::trim)
            .filter(|prompt| !prompt.is_empty())
        else {
            eprintln!(
                "eixd: plugin {}: command {:?} has no prompt template; action commands are not enabled yet",
                spec.name, command.name
            );
            continue;
        };
        out.push(PluginCommandRef {
            exposed: format!("{PLUGIN_COMMAND_PREFIX}{}/{}", spec.name, command.name),
            plugin: spec.name.clone(),
            args: command.args.clone(),
            description: command.description.clone(),
            prompt: prompt.to_string(),
        });
    }
    Ok(out)
}

fn validate_events(
    spec: &PluginSpec,
    capabilities: &BTreeSet<PluginCapability>,
    init: &PluginInitializeResult,
) -> Result<Vec<String>> {
    if init.events.is_empty() {
        return Ok(Vec::new());
    }
    if !capabilities.contains(&PluginCapability::Events) {
        bail!(
            "plugin {} subscribes to events but was not granted `events`",
            spec.name
        );
    }
    for event in &init.events {
        if event.is_empty()
            || !event
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '*'))
        {
            bail!("plugin {}: invalid event pattern {event:?}", spec.name);
        }
    }
    Ok(init.events.clone())
}

fn validate_command(command: &PluginCommand) -> Result<()> {
    if command.name.is_empty()
        || command.name.starts_with('/')
        || command.name.ends_with('/')
        || command
            .name
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
        || command.name.chars().any(|ch| {
            ch.is_whitespace()
                || !(ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '/'))
        })
    {
        bail!("invalid plugin command name {:?}", command.name);
    }
    Ok(())
}

fn storage_key(params: &Value) -> Result<String> {
    let key = params
        .get("key")
        .and_then(Value::as_str)
        .context("storage method needs a string `key`")?;
    if key.is_empty() || key.len() > MAX_STORAGE_KEY {
        bail!("storage key must be 1..={MAX_STORAGE_KEY} bytes");
    }
    Ok(key.to_string())
}

fn event_matches(pattern: &str, method: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    match pattern.strip_suffix('*') {
        Some(prefix) => method.starts_with(prefix),
        None => pattern == method,
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

fn cap(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let end = text
        .char_indices()
        .take_while(|(index, _)| *index < max)
        .last()
        .map(|(index, ch)| index + ch.len_utf8())
        .unwrap_or(0);
    format!("{}… [truncated]", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_patterns_are_prefix_scoped() {
        assert!(event_matches("*", "session.event"));
        assert!(event_matches("session.*", "session.event"));
        assert!(!event_matches("session.*", "tool.completed"));
        assert!(event_matches("tool.completed", "tool.completed"));
    }

    #[test]
    fn plugin_commands_are_namespaced() {
        let spec = PluginSpec {
            name: "acme".into(),
            version: "0.1.0".into(),
            command: "true".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: PathBuf::from("/tmp"),
            timeout: Duration::from_secs(1),
            capabilities: vec![PluginCapability::Commands],
            allowed_tools: Vec::new(),
            denied_tools: Vec::new(),
        };
        let init = PluginInitializeResult {
            name: "acme".into(),
            commands: vec![PluginCommand {
                name: "review".into(),
                description: "review".into(),
                args: String::new(),
                prompt: Some("review $ARGUMENTS".into()),
            }],
            ..Default::default()
        };
        let capabilities = spec.capabilities.iter().copied().collect();
        let commands = validate_commands(&spec, &capabilities, &init).unwrap();
        assert_eq!(commands[0].exposed, "plugin/acme/review");
    }

    #[tokio::test]
    async fn plugin_can_use_host_storage_during_a_tool_call() -> anyhow::Result<()> {
        if std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_err()
        {
            return Ok(());
        }
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "eix-plugin-host-test-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir)?;
        let script = dir.join("plugin.py");
        std::fs::write(
            &script,
            r#"import sys, json
def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n"); sys.stdout.flush()
def host(method, params):
    send({"id": 900, "method": method, "params": params})
    while True:
        line = sys.stdin.readline()
        if not line: raise RuntimeError("host closed")
        value = json.loads(line)
        if value.get("id") == 900 and "method" not in value:
            if "error" in value: raise RuntimeError(value["error"]["message"])
            return value.get("result")
for line in sys.stdin:
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        send({"id": msg["id"], "result": {
            "name": "kv", "version": "0.1.0", "api_version": 1,
            "tools": [
                {"name": "put", "description": "put", "input_schema": {"type": "object"},
                 "read_only": False},
                {"name": "get", "description": "get", "input_schema": {"type": "object"},
                 "read_only": True}
            ], "commands": [], "events": []}})
    elif method == "tools/call":
        name = msg["params"]["tool"]; args = msg["params"]["arguments"]
        if name == "put":
            host("host.storage.set", {"scope": "global", "key": args["key"],
                                      "value": args["value"]})
            send({"id": msg["id"], "result": {"ok": True, "output": "stored"}})
        else:
            result = host("host.storage.get", {"scope": "global", "key": args["key"]})
            send({"id": msg["id"], "result": {"ok": True,
                  "output": str(result.get("value"))}})
"#,
        )?;

        let mut config = Config {
            load_cwd: dir.clone(),
            ..Config::default()
        };
        config.plugins.insert(
            "kv".to_string(),
            PluginConfig {
                command: "python3".to_string(),
                args: vec![script.to_string_lossy().into_owned()],
                capabilities: vec![PluginCapability::Tools, PluginCapability::Storage],
                timeout_secs: 5,
                ..PluginConfig::default()
            },
        );
        let store = Arc::new(Store::in_memory()?);
        let manager = PluginManager::new(Arc::clone(&store), dir.join("data"));
        manager.load(&config).await;
        let list = manager.list().await;
        assert_eq!(list.plugins[0].state, "running", "{:?}", list.plugins[0]);
        let put = manager
            .call(
                "plugin__kv__put",
                &json!({"key": "answer", "value": "42"}),
                "s",
                &dir,
                10_000,
            )
            .await;
        assert!(put.ok, "{}", put.output);
        let get = manager
            .call(
                "plugin__kv__get",
                &json!({"key": "answer"}),
                "s",
                &dir,
                10_000,
            )
            .await;
        assert!(get.ok, "{}", get.output);
        assert_eq!(get.output, "42");
        assert_eq!(
            store.plugin_kv_get("kv", "global", "answer")?.as_deref(),
            Some("42")
        );
        manager.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[tokio::test]
    async fn ungranted_tool_is_rejected() {
        let spec = PluginSpec {
            name: "acme".into(),
            version: "0.1.0".into(),
            command: "true".into(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: PathBuf::from("/tmp"),
            timeout: Duration::from_secs(1),
            capabilities: vec![PluginCapability::Commands],
            allowed_tools: Vec::new(),
            denied_tools: Vec::new(),
        };
        let init = PluginInitializeResult {
            name: "acme".into(),
            tools: vec![PluginTool {
                name: "search".into(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
                read_only: true,
            }],
            ..Default::default()
        };
        let capabilities = spec.capabilities.iter().copied().collect();
        let manager =
            PluginManager::new(Arc::new(Store::in_memory().unwrap()), PathBuf::from("/tmp"));
        assert!(manager.validate_tools(&spec, &capabilities, &init).is_err());
    }

    fn python3_available() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
    }

    #[tokio::test]
    async fn crashed_plugin_is_marked_failed_and_loses_its_tools() -> anyhow::Result<()> {
        if !python3_available() {
            return Ok(());
        }
        let dir = temp_dir("eix-plugin-crash-test");
        std::fs::create_dir_all(&dir)?;
        let script = dir.join("crash.py");
        std::fs::write(
            &script,
            r#"import sys, json
line = sys.stdin.readline()
message = json.loads(line)
if message.get("method") == "initialize":
    sys.stdout.write(json.dumps({
        "id": message["id"],
        "result": {
            "name": "crash", "version": "0.1.0", "api_version": 1,
            "tools": [{"name": "boom", "description": "boom",
                       "input_schema": {"type": "object"}}],
            "commands": [], "events": []
        }
    }) + "\n")
    sys.stdout.flush()
# Exit without waiting for another request.
"#,
        )?;

        let mut config = Config {
            load_cwd: dir.clone(),
            ..Config::default()
        };
        config.plugins.insert(
            "crash".to_string(),
            PluginConfig {
                command: "python3".to_string(),
                args: vec![script.to_string_lossy().into_owned()],
                capabilities: vec![PluginCapability::Tools],
                timeout_secs: 5,
                ..PluginConfig::default()
            },
        );
        let manager = PluginManager::new(Arc::new(Store::in_memory()?), dir.join("data"));
        manager.load(&config).await;

        let mut last = manager.list().await;
        for _ in 0..100 {
            if last
                .plugins
                .first()
                .is_some_and(|plugin| plugin.state == "failed")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
            last = manager.list().await;
        }
        let plugin = last.plugins.first().expect("plugin status");
        assert_eq!(plugin.state, "failed", "{plugin:?}");
        assert_eq!(plugin.tools, 0);
        assert_eq!(plugin.commands, 0);
        assert!(
            plugin
                .error
                .as_deref()
                .is_some_and(|error| error.contains("exited")),
            "{plugin:?}"
        );
        assert!(!manager.knows("plugin__crash__boom").await);
        manager.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[tokio::test]
    async fn failed_tool_call_drops_the_plugin_and_its_tools() -> anyhow::Result<()> {
        if !python3_available() {
            return Ok(());
        }
        let dir = temp_dir("eix-plugin-tool-failure-test");
        std::fs::create_dir_all(&dir)?;
        let script = dir.join("fail.py");
        std::fs::write(
            &script,
            r#"import sys, json
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    if method == "initialize":
        sys.stdout.write(json.dumps({
            "id": message["id"],
            "result": {
                "name": "badtool", "version": "0.1.0", "api_version": 1,
                "tools": [{"name": "fail", "description": "fail",
                           "input_schema": {"type": "object"}}],
                "commands": [], "events": []
            }
        }) + "\n")
        sys.stdout.flush()
    elif method == "tools/call":
        sys.stdout.write(json.dumps({
            "id": message["id"],
            "error": {"message": "intentional failure"}
        }) + "\n")
        sys.stdout.flush()
    elif method == "shutdown":
        break
"#,
        )?;

        let mut config = Config {
            load_cwd: dir.clone(),
            ..Config::default()
        };
        config.plugins.insert(
            "badtool".to_string(),
            PluginConfig {
                command: "python3".to_string(),
                args: vec![script.to_string_lossy().into_owned()],
                capabilities: vec![PluginCapability::Tools],
                timeout_secs: 5,
                ..PluginConfig::default()
            },
        );
        let manager = PluginManager::new(Arc::new(Store::in_memory()?), dir.join("data"));
        manager.load(&config).await;
        let outcome = manager
            .call(
                "plugin__badtool__fail",
                &serde_json::json!({}),
                "session",
                &dir,
                1024,
            )
            .await;
        assert!(!outcome.ok);
        assert!(
            outcome.output.contains("intentional failure"),
            "{}",
            outcome.output
        );
        let listed = manager.list().await;
        assert_eq!(listed.plugins[0].state, "failed");
        assert_eq!(listed.plugins[0].tools, 0);
        assert!(manager.tool_definitions().await.is_empty());
        assert!(!manager.knows("plugin__badtool__fail").await);
        manager.shutdown().await;
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }

    #[tokio::test]
    async fn shutdown_notifies_idle_plugins() -> anyhow::Result<()> {
        if !python3_available() {
            return Ok(());
        }
        let dir = temp_dir("eix-plugin-shutdown-test");
        std::fs::create_dir_all(&dir)?;
        let script = dir.join("shutdown.py");
        let marker = dir.join("stopped");
        std::fs::write(
            &script,
            r#"import sys, json, pathlib
marker = sys.argv[1]
for line in sys.stdin:
    message = json.loads(line)
    method = message.get("method")
    if method == "initialize":
        sys.stdout.write(json.dumps({
            "id": message["id"],
            "result": {
                "name": "bye", "version": "0.1.0", "api_version": 1,
                "tools": [], "commands": [], "events": []
            }
        }) + "\n")
        sys.stdout.flush()
    elif method == "shutdown":
        pathlib.Path(marker).write_text("bye")
        break
"#,
        )?;

        let mut config = Config {
            load_cwd: dir.clone(),
            ..Config::default()
        };
        config.plugins.insert(
            "bye".to_string(),
            PluginConfig {
                command: "python3".to_string(),
                args: vec![
                    script.to_string_lossy().into_owned(),
                    marker.to_string_lossy().into_owned(),
                ],
                timeout_secs: 5,
                ..PluginConfig::default()
            },
        );
        let manager = PluginManager::new(Arc::new(Store::in_memory()?), dir.join("data"));
        manager.load(&config).await;
        assert_eq!(
            manager.list().await.plugins[0].state,
            "running",
            "plugin should start"
        );

        manager.shutdown().await;
        assert!(marker.exists(), "plugin did not observe shutdown");
        assert_eq!(manager.list().await.plugins[0].state, "stopped");
        let _ = std::fs::remove_dir_all(&dir);
        Ok(())
    }
}
