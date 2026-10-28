//! Out-of-process plugin host.
//!
//! One child process per enabled server plugin. The wire format is JSON-RPC
//! 2.0 over newline-delimited JSON, with a protocol version independent from
//! the daemon's own RPC ([`eix_core::plugin::PLUGIN_API_VERSION`]).
//!
//! The host enforces the capability grant from config and builds a single
//! registry of tools, commands and event subscriptions. Declarative plugins
//! (manifest commands only) never spawn a process.

mod client;
mod host_api;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use eix_core::plugin::{
    valid_plugin_name, valid_tool_name, ManifestCommand, PluginCapability, PluginCommand,
    PluginCommandInfo, PluginCommandList, PluginConfig, PluginInfo, PluginInitializeResult,
    PluginListResult, PluginManifest, PluginState, PluginTool, PluginToolCallParams,
    PluginToolCallResult, PLUGIN_COMMAND_PREFIX, PLUGIN_TOOL_PREFIX,
};
use eix_core::redact::redact;
use eix_core::{Config, ToolDefinition};
use serde_json::Value;
use tokio::sync::{mpsc, Mutex};

use crate::store::Store;
use crate::tools::ToolOutcome;

use client::PluginClient;

const DEFAULT_CAPABILITIES: &[PluginCapability] =
    &[PluginCapability::Tools, PluginCapability::Commands];
const MAX_PLUGIN_OUTPUT: usize = 1_000_000;
const MAX_TOOL_DESCRIPTION: usize = 2048;
const MAX_COMMAND_PROMPT: usize = 256 * 1024;
const MAX_EVENT_PATTERN: usize = 128;

#[derive(Debug, Clone)]
struct PluginEvent {
    method: String,
    params: Value,
}

/// Host-side view of one advertised tool.
#[derive(Debug, Clone)]
struct ToolRef {
    exposed: String,
    plugin: String,
    tool: String,
    description: String,
    schema: Value,
}

/// Host-side view of one command, runtime or declarative.
#[derive(Debug, Clone)]
struct CommandRef {
    plugin: String,
    exposed: String,
    args: String,
    description: String,
    prompt: String,
}

/// Everything needed to start one plugin.
struct PluginSpec {
    name: String,
    version: String,
    description: String,
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    cwd: PathBuf,
    data_dir: PathBuf,
    timeout: Duration,
    capabilities: Vec<PluginCapability>,
    allowed_tools: Vec<String>,
    denied_tools: Vec<String>,
    settings: BTreeMap<String, Value>,
    has_server: bool,
    declarative_commands: Vec<ManifestCommand>,
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

struct PluginStatus {
    name: String,
    version: String,
    description: String,
    enabled: bool,
    command: String,
    state: PluginState,
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
            version: self.version.clone(),
            description: self.description.clone(),
            enabled: self.enabled,
            command: self.command.clone(),
            state: self.state,
            tools: self.tools,
            commands: self.commands,
            events: self.events.clone(),
            capabilities: self.capabilities.clone(),
            error: self.error.clone(),
        }
    }
}

/// The daemon's plugin registry.
pub struct PluginManager {
    store: Arc<Store>,
    data_dir: PathBuf,
    clients: Arc<Mutex<BTreeMap<String, Arc<PluginClient>>>>,
    /// Event patterns per running plugin, copied from `initialize`.
    subscriptions: Arc<Mutex<BTreeMap<String, Vec<String>>>>,
    tools: Mutex<Vec<ToolRef>>,
    commands: Mutex<Vec<CommandRef>>,
    statuses: Mutex<BTreeMap<String, PluginStatus>>,
    event_tx: mpsc::Sender<PluginEvent>,
    /// False when no running plugin subscribed to any event. `publish` is
    /// called for every daemon notification, so this keeps the common case
    /// cheap.
    has_event_subscribers: AtomicBool,
}

impl PluginManager {
    pub fn new(store: Arc<Store>, data_dir: PathBuf) -> Arc<Self> {
        // Bounded so a plugin event storm cannot grow memory without bound.
        // `publish` drops events under backpressure; events are best-effort
        // notifications, not durable state.
        let (event_tx, mut event_rx) = mpsc::channel::<PluginEvent>(1024);
        let clients: Arc<Mutex<BTreeMap<String, Arc<PluginClient>>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let subscriptions = Arc::new(Mutex::new(BTreeMap::<String, Vec<String>>::new()));
        let event_clients = Arc::clone(&clients);
        let event_subscriptions = Arc::clone(&subscriptions);
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let targets: Vec<_> = {
                    let clients = event_clients.lock().await;
                    let subscriptions = event_subscriptions.lock().await;
                    clients
                        .iter()
                        .filter(|(name, client)| {
                            !client.is_closed()
                                && subscriptions.get(*name).is_some_and(|patterns| {
                                    patterns
                                        .iter()
                                        .any(|pattern| wildcard(pattern, &event.method))
                                })
                        })
                        .map(|(_, client)| Arc::clone(client))
                        .collect()
                };
                for client in targets {
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
            subscriptions,
            tools: Mutex::new(Vec::new()),
            commands: Mutex::new(Vec::new()),
            statuses: Mutex::new(BTreeMap::new()),
            event_tx,
            has_event_subscribers: AtomicBool::new(false),
        })
    }

    /// Stop every plugin and start the enabled set from `config`.
    pub async fn load(&self, config: &Config) {
        self.has_event_subscribers.store(false, Ordering::Relaxed);
        self.stop_all().await;
        self.subscriptions.lock().await.clear();
        self.tools.lock().await.clear();
        self.commands.lock().await.clear();
        self.statuses.lock().await.clear();

        for (name, plugin) in &config.plugins {
            self.statuses.lock().await.insert(
                name.clone(),
                PluginStatus {
                    name: name.clone(),
                    version: String::new(),
                    description: String::new(),
                    enabled: plugin.enabled,
                    command: plugin.command.clone(),
                    state: if plugin.enabled {
                        PluginState::Starting
                    } else {
                        PluginState::Disabled
                    },
                    tools: 0,
                    commands: 0,
                    events: Vec::new(),
                    capabilities: plugin.capabilities.clone(),
                    error: None,
                },
            );
            if !plugin.enabled {
                continue;
            }
            match resolve_spec(name, plugin, &config.load_cwd, &self.data_dir) {
                Ok(spec) => {
                    if let Err(err) = self.start_one(spec).await {
                        self.mark_failed(name, plugin, &err).await;
                    }
                }
                Err(err) => self.mark_failed(name, plugin, &err).await,
            }
        }
    }

    async fn start_one(&self, spec: PluginSpec) -> Result<()> {
        let declarative = self.declarative_commands(&spec)?;
        if !spec.has_server {
            self.commit_declarative(&spec, declarative).await?;
            return Ok(());
        }

        let client = PluginClient::spawn(&spec, Arc::clone(&self.store))
            .await
            .with_context(|| format!("plugin {} failed to start", spec.name))?;
        let init = match client.initialize(&spec, &spec.cwd).await {
            Ok(init) => init,
            Err(err) => {
                client.shutdown().await;
                return Err(err);
            }
        };
        match self
            .commit_runtime(&spec, &client, &init, declarative)
            .await
        {
            Ok(()) => Ok(()),
            Err(err) => {
                client.shutdown().await;
                Err(err)
            }
        }
    }

    /// Commands declared in the manifest, validated against the capability
    /// grant and the registry's namespace rules.
    fn declarative_commands(&self, spec: &PluginSpec) -> Result<Vec<CommandRef>> {
        if spec.declarative_commands.is_empty() {
            return Ok(Vec::new());
        }
        if !spec.capabilities.contains(&PluginCapability::Commands) {
            bail!(
                "plugin {} declares commands but was not granted `commands`",
                spec.name
            );
        }
        let mut out = Vec::new();
        for command in &spec.declarative_commands {
            validate_command_name(&command.name)?;
            if command.prompt.trim().is_empty() {
                bail!(
                    "plugin {} command {:?} has an empty prompt",
                    spec.name,
                    command.name
                );
            }
            if command.prompt.len() > MAX_COMMAND_PROMPT {
                bail!(
                    "plugin {} command {:?} prompt is too long",
                    spec.name,
                    command.name
                );
            }
            out.push(CommandRef {
                plugin: spec.name.clone(),
                exposed: format!("{PLUGIN_COMMAND_PREFIX}{}/{}", spec.name, command.name),
                args: command.args.clone(),
                description: command.description.clone(),
                prompt: command.prompt.clone(),
            });
        }
        Ok(out)
    }

    async fn commit_declarative(&self, spec: &PluginSpec, commands: Vec<CommandRef>) -> Result<()> {
        self.check_command_conflicts(&commands).await?;
        let count = commands.len();
        self.commands.lock().await.extend(commands);
        self.statuses.lock().await.insert(
            spec.name.clone(),
            PluginStatus {
                name: spec.name.clone(),
                version: spec.version.clone(),
                description: spec.description.clone(),
                enabled: true,
                command: String::new(),
                state: PluginState::Ready,
                tools: 0,
                commands: count,
                events: Vec::new(),
                capabilities: spec.capabilities.clone(),
                error: None,
            },
        );
        Ok(())
    }

    async fn commit_runtime(
        &self,
        spec: &PluginSpec,
        client: &Arc<PluginClient>,
        init: &PluginInitializeResult,
        declarative: Vec<CommandRef>,
    ) -> Result<()> {
        let capabilities: BTreeSet<PluginCapability> = spec.capabilities.iter().copied().collect();

        if !init.hooks.is_empty() {
            bail!(
                "plugin {} declares hooks, which are not implemented in this build",
                spec.name
            );
        }
        if !init.ui.is_empty() {
            bail!(
                "plugin {} declares UI contributions, which are not implemented in this build",
                spec.name
            );
        }
        if !init.rpc.is_empty() {
            bail!(
                "plugin {} declares RPC methods, which are not implemented in this build",
                spec.name
            );
        }
        if !init.tools.is_empty() && !capabilities.contains(&PluginCapability::Tools) {
            bail!(
                "plugin {} advertises {} tool(s) but was not granted `tools`",
                spec.name,
                init.tools.len()
            );
        }
        if !init.commands.is_empty() && !capabilities.contains(&PluginCapability::Commands) {
            bail!(
                "plugin {} advertises {} command(s) but was not granted `commands`",
                spec.name,
                init.commands.len()
            );
        }
        if !init.events.is_empty() && !capabilities.contains(&PluginCapability::Events) {
            bail!(
                "plugin {} subscribes to events but was not granted `events`",
                spec.name
            );
        }

        let events = validate_events(spec, &init.events)?;
        let tools = validate_tools(spec, &init.tools)?;
        let mut commands = declarative;
        commands.extend(validate_commands(spec, &init.commands)?);

        self.check_tool_conflicts(&tools).await?;
        self.check_command_conflicts(&commands).await?;

        let tool_count = tools.len();
        let command_count = commands.len();
        self.tools.lock().await.extend(tools);
        self.commands.lock().await.extend(commands);
        self.clients
            .lock()
            .await
            .insert(spec.name.clone(), Arc::clone(client));
        {
            let mut subscriptions = self.subscriptions.lock().await;
            if events.is_empty() {
                subscriptions.remove(&spec.name);
            } else {
                subscriptions.insert(spec.name.clone(), events.clone());
            }
            self.has_event_subscribers
                .store(!subscriptions.is_empty(), Ordering::Relaxed);
        }
        self.statuses.lock().await.insert(
            spec.name.clone(),
            PluginStatus {
                name: spec.name.clone(),
                version: spec.version.clone(),
                description: spec.description.clone(),
                enabled: true,
                command: spec.command.clone(),
                state: PluginState::Ready,
                tools: tool_count,
                commands: command_count,
                events,
                capabilities: spec.capabilities.clone(),
                error: None,
            },
        );
        Ok(())
    }

    async fn check_tool_conflicts(&self, tools: &[ToolRef]) -> Result<()> {
        let existing = self.tools.lock().await;
        let mut seen = BTreeSet::new();
        for tool in tools {
            if existing
                .iter()
                .any(|existing| existing.exposed == tool.exposed)
                || !seen.insert(tool.exposed.clone())
            {
                bail!("duplicate plugin tool {:?}", tool.exposed);
            }
        }
        Ok(())
    }

    async fn check_command_conflicts(&self, commands: &[CommandRef]) -> Result<()> {
        let existing = self.commands.lock().await;
        let mut seen = BTreeSet::new();
        for command in commands {
            if existing
                .iter()
                .any(|existing| existing.exposed == command.exposed)
                || !seen.insert(command.exposed.clone())
            {
                bail!("duplicate plugin command {:?}", command.exposed);
            }
        }
        Ok(())
    }

    async fn mark_failed(&self, name: &str, plugin: &PluginConfig, err: &anyhow::Error) {
        let message = redact(&format!("{err:#}"));
        eprintln!("eixd: plugin {name}: {message}");
        if let Some(status) = self.statuses.lock().await.get_mut(name) {
            status.state = PluginState::Failed;
            status.error = Some(message);
            status.tools = 0;
            status.commands = 0;
        } else {
            self.statuses.lock().await.insert(
                name.to_string(),
                PluginStatus {
                    name: name.to_string(),
                    version: String::new(),
                    description: String::new(),
                    enabled: plugin.enabled,
                    command: plugin.command.clone(),
                    state: PluginState::Failed,
                    tools: 0,
                    commands: 0,
                    events: Vec::new(),
                    capabilities: plugin.capabilities.clone(),
                    error: Some(message),
                },
            );
        }
    }

    /// Detect children that exited without a request in flight.
    async fn refresh(&self) {
        let clients = { self.clients.lock().await.clone() };
        for (name, client) in clients {
            if let Some(reason) = client.exit_reason().await {
                self.drop_client(&name, &client, Some(reason)).await;
            }
        }
    }

    async fn drop_client(
        &self,
        name: &str,
        client: &Arc<PluginClient>,
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
        client.shutdown().await;
        self.tools.lock().await.retain(|tool| tool.plugin != name);
        self.commands
            .lock()
            .await
            .retain(|command| command.plugin != name);
        {
            let mut subscriptions = self.subscriptions.lock().await;
            subscriptions.remove(name);
            self.has_event_subscribers
                .store(!subscriptions.is_empty(), Ordering::Relaxed);
        }
        if let Some(status) = self.statuses.lock().await.get_mut(name) {
            status.state = PluginState::Failed;
            status.error = error.map(|error| redact(&error));
            status.tools = 0;
            status.commands = 0;
            status.events.clear();
        }
        true
    }

    /// Stop every running process before the registry is rebuilt.
    async fn stop_all(&self) {
        let clients: Vec<_> = {
            let mut clients = self.clients.lock().await;
            std::mem::take(&mut *clients).into_values().collect()
        };
        for client in clients {
            client.shutdown().await;
        }
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
            workspace: Some(cwd.to_path_buf()),
            cwd: Some(cwd.to_path_buf()),
        };
        let result = client
            .request(
                "tools/call",
                serde_json::to_value(params).unwrap_or(Value::Null),
            )
            .await;
        let duration_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(value) => match serde_json::from_value::<PluginToolCallResult>(value) {
                Ok(result) => ToolOutcome {
                    ok: result.ok && !result.is_error,
                    output: cap(&result.to_text(), max_output_bytes.min(MAX_PLUGIN_OUTPUT)),
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
                // A per-call timeout cancels the request but leaves the
                // plugin running; only a closed connection removes it.
                if let Some(reason) = client.exit_reason().await {
                    self.drop_client(&tool.plugin, &client, Some(reason)).await;
                }
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
        let old = self.clients.lock().await.remove(name);
        if let Some(client) = old {
            client.shutdown().await;
        }
        self.subscriptions.lock().await.remove(name);
        {
            let subscriptions = self.subscriptions.lock().await;
            self.has_event_subscribers
                .store(!subscriptions.is_empty(), Ordering::Relaxed);
        }
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
                    version: String::new(),
                    description: String::new(),
                    enabled: false,
                    command: plugin.command.clone(),
                    state: PluginState::Disabled,
                    tools: 0,
                    commands: 0,
                    events: Vec::new(),
                    capabilities: plugin.capabilities.clone(),
                    error: None,
                },
            );
            return Ok(());
        }
        let spec = match resolve_spec(name, plugin, &config.load_cwd, &self.data_dir) {
            Ok(spec) => spec,
            Err(err) => {
                self.mark_failed(name, plugin, &err).await;
                return Err(anyhow::anyhow!(redact(&format!("{err:#}"))));
            }
        };
        if let Err(err) = self.start_one(spec).await {
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
        self.stop_all().await;
        self.subscriptions.lock().await.clear();
        self.tools.lock().await.clear();
        self.commands.lock().await.clear();
        let mut statuses = self.statuses.lock().await;
        for status in statuses.values_mut() {
            status.state = PluginState::Stopped;
            status.tools = 0;
            status.commands = 0;
            status.events.clear();
        }
    }
}

fn resolve_spec(
    name: &str,
    plugin: &PluginConfig,
    cwd: &Path,
    data_dir: &Path,
) -> Result<PluginSpec> {
    if !valid_plugin_name(name) {
        bail!("invalid plugin name {name:?}");
    }
    let cwd = if cwd.as_os_str().is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        cwd.to_path_buf()
    };
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
        if manifest.name != name {
            bail!(
                "plugin {name} manifest {} declares name {:?}",
                path.display(),
                manifest.name
            );
        }
        if manifest.components.ui.is_some() {
            bail!(
                "plugin {name} manifest {} declares components.ui, which is not implemented in this build",
                path.display()
            );
        }
    }

    let (command, args) = if !plugin.command.trim().is_empty() {
        (plugin.command.clone(), plugin.args.clone())
    } else if let Some((path, manifest)) = &manifest {
        let base = path.parent().unwrap_or(&cwd);
        manifest.server_command(base).unwrap_or_default()
    } else {
        bail!("plugin {name} needs `command` or `manifest`");
    };
    let has_server = !command.trim().is_empty();

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

    let declarative_commands = manifest
        .as_ref()
        .map(|(_, manifest)| manifest.contributes.commands.clone())
        .unwrap_or_default();
    let version = manifest
        .as_ref()
        .map(|(_, manifest)| manifest.version.clone())
        .unwrap_or_default();
    let description = manifest
        .as_ref()
        .map(|(_, manifest)| manifest.description.clone())
        .unwrap_or_default();

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
        .unwrap_or_else(|| cwd.clone());

    Ok(PluginSpec {
        name: name.to_string(),
        version,
        description,
        command,
        args,
        env: plugin.env.clone(),
        cwd: plugin_cwd,
        data_dir: data_dir.join("plugins").join(name),
        timeout: Duration::from_secs(plugin.timeout_secs.max(1)),
        capabilities,
        allowed_tools: plugin.allowed_tools.clone(),
        denied_tools: plugin.denied_tools.clone(),
        settings: plugin.settings.clone(),
        has_server,
        declarative_commands,
    })
}

fn validate_tools(spec: &PluginSpec, tools: &[PluginTool]) -> Result<Vec<ToolRef>> {
    let mut out = Vec::new();
    for tool in tools {
        validate_tool(tool)?;
        if !spec.allows_tool(&tool.name) {
            continue;
        }
        out.push(ToolRef {
            exposed: format!("{PLUGIN_TOOL_PREFIX}{}__{}", spec.name, tool.name),
            plugin: spec.name.clone(),
            tool: tool.name.clone(),
            description: tool.description.clone(),
            schema: tool.input_schema.clone(),
        });
    }
    Ok(out)
}

fn validate_tool(tool: &PluginTool) -> Result<()> {
    if !valid_tool_name(&tool.name) {
        bail!(
            "invalid tool name {:?}: use letters, digits, '.', '_' or '-', without '__'",
            tool.name
        );
    }
    if tool.description.len() > MAX_TOOL_DESCRIPTION {
        bail!("tool {:?} description is too long", tool.name);
    }
    if !tool.input_schema.is_object() {
        bail!("tool {:?} input_schema must be a JSON object", tool.name);
    }
    Ok(())
}

fn validate_commands(spec: &PluginSpec, commands: &[PluginCommand]) -> Result<Vec<CommandRef>> {
    let mut out = Vec::new();
    for command in commands {
        validate_command_name(&command.name)?;
        let prompt = command
            .prompt
            .as_deref()
            .filter(|prompt| !prompt.trim().is_empty())
            .with_context(|| {
                format!(
                    "plugin {} command {:?} needs a non-empty prompt",
                    spec.name, command.name
                )
            })?;
        if prompt.len() > MAX_COMMAND_PROMPT {
            bail!(
                "plugin {} command {:?} prompt is too long",
                spec.name,
                command.name
            );
        }
        out.push(CommandRef {
            plugin: spec.name.clone(),
            exposed: format!("{PLUGIN_COMMAND_PREFIX}{}/{}", spec.name, command.name),
            args: command.args.clone(),
            description: command.description.clone(),
            prompt: prompt.to_string(),
        });
    }
    Ok(out)
}

fn validate_command_name(name: &str) -> Result<()> {
    if !valid_tool_name(name) {
        bail!(
            "invalid command name {:?}: use letters, digits, '.', '_' or '-', without '__'",
            name
        );
    }
    Ok(())
}

fn validate_events(spec: &PluginSpec, events: &[String]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for pattern in events {
        let pattern = pattern.trim();
        if pattern.is_empty() || pattern.len() > MAX_EVENT_PATTERN {
            bail!(
                "plugin {} event pattern must be 1..={} bytes",
                spec.name,
                MAX_EVENT_PATTERN
            );
        }
        if pattern.chars().any(char::is_whitespace) {
            bail!(
                "plugin {} event pattern {pattern:?} must not contain whitespace",
                spec.name
            );
        }
        if !pattern
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-' | '*' | '?' | '/'))
        {
            bail!(
                "plugin {} event pattern {pattern:?} contains unsupported characters",
                spec.name
            );
        }
        out.push(pattern.to_string());
    }
    out.sort();
    out.dedup();
    Ok(out)
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
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(name: &str) -> PluginSpec {
        PluginSpec {
            name: name.to_string(),
            version: "1.0.0".into(),
            description: String::new(),
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            cwd: PathBuf::from("/tmp"),
            data_dir: PathBuf::from("/tmp/plugins").join(name),
            timeout: Duration::from_secs(1),
            capabilities: vec![
                PluginCapability::Tools,
                PluginCapability::Commands,
                PluginCapability::Events,
            ],
            allowed_tools: Vec::new(),
            denied_tools: Vec::new(),
            settings: BTreeMap::new(),
            has_server: false,
            declarative_commands: Vec::new(),
        }
    }

    #[test]
    fn event_patterns_are_validated() {
        assert!(validate_events(&spec("a"), &["session.*".into()]).is_ok());
        assert!(validate_events(&spec("a"), &["".into()]).is_err());
        assert!(validate_events(&spec("a"), &["session event".into()]).is_err());
        assert!(validate_events(&spec("a"), &["session!".into()]).is_err());
    }

    #[test]
    fn event_patterns_match_only_subscribed_methods() {
        assert!(wildcard("session.*", "session.created"));
        assert!(wildcard("session.*", "session.updated"));
        assert!(!wildcard("session.*", "tool.completed"));
        assert!(wildcard("tool.?ompleted", "tool.completed"));
        assert!(!wildcard("tool.?ompleted", "tool.completed.extra"));
    }

    #[test]
    fn command_names_are_namespaced_and_validated() {
        assert!(validate_command_name("review").is_ok());
        assert!(validate_command_name("a__b").is_err());
        assert!(validate_command_name("../x").is_err());
    }

    #[test]
    fn duplicate_plugin_tools_are_rejected() {
        let tools = vec![
            PluginTool {
                name: "one".into(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
                read_only: false,
            },
            PluginTool {
                name: "one".into(),
                description: String::new(),
                input_schema: json!({"type": "object"}),
                read_only: false,
            },
        ];
        let refs = validate_tools(&spec("acme"), &tools).unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let error = runtime.block_on(async {
            let manager = PluginManager::new(
                Arc::new(crate::store::Store::in_memory().unwrap()),
                PathBuf::from("/tmp"),
            );
            manager.tools.lock().await.extend(refs.clone());
            manager.check_tool_conflicts(&refs).await
        });
        assert!(error.is_err());
    }
}
