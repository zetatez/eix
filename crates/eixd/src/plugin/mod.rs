//! Out-of-process plugin host.
//!
//! One child process per enabled server plugin. The wire format is JSON-RPC
//! 2.0 over newline-delimited JSON, with a protocol version independent from
//! the daemon's own RPC ([`eix_core::plugin::PLUGIN_API_VERSION`]).
//!
//! The host enforces the capability grant from config and builds a single
//! registry of tools, commands and event subscriptions. Declarative plugins
//! (manifest commands only) never spawn a process.

mod backend;
mod client;
mod dylib;
mod host_api;
mod logs;
mod process;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use eix_core::plugin::{
    valid_hook_point, valid_plugin_name, valid_tool_name, valid_ui_surface, ManifestCommand,
    PluginCapability, PluginCommand, PluginCommandInfo, PluginCommandList, PluginCommandRunResult,
    PluginConfig, PluginDoctorCheck, PluginDoctorEntry, PluginDoctorResult, PluginHook, PluginInfo,
    PluginInitializeResult, PluginListResult, PluginLogsParams, PluginLogsResult, PluginManifest,
    PluginRpc, PluginSettingsResult, PluginState, PluginTool, PluginToolCallParams,
    PluginToolCallResult, PluginUi, PluginUiActionParams, PluginUiActionResult, PluginUiListResult,
    PluginUiPickParams, PluginUiPickResult, PluginUiRenderParams, PluginUiRenderResult,
    PluginUiSurface, PLUGIN_COMMAND_PREFIX, PLUGIN_TOOL_PREFIX,
};
use eix_core::redact::redact;
use eix_core::{Config, Notification, ToolDefinition};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc, Mutex};

use crate::store::Store;
use crate::tools::ToolOutcome;

use backend::ExtensionBackend;
use host_api::RpcCallRequest;

const DEFAULT_CAPABILITIES: &[PluginCapability] =
    &[PluginCapability::Tools, PluginCapability::Commands];
const MAX_PLUGIN_OUTPUT: usize = 1_000_000;
const MAX_TOOL_DESCRIPTION: usize = 2048;
const MAX_COMMAND_PROMPT: usize = 256 * 1024;
const MAX_EVENT_PATTERN: usize = 128;
const MAX_SETTINGS_SCHEMA: usize = 256 * 1024;
/// Consecutive backend failures before `plugin.list` reports `degraded`.
/// A plugin that returns a normal tool `is_error` result is not counted: only
/// transport/protocol failures mean the backend itself is unhealthy.
const DEGRADED_AFTER_FAILURES: u32 = 3;

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
    /// `Some(action)` for commands that call `commands/run` instead of
    /// submitting a prompt.
    action: Option<String>,
}

/// One registered hook callback.
#[derive(Debug, Clone)]
struct HookRef {
    plugin: String,
    point: String,
    priority: i32,
    timeout: Option<Duration>,
    on_error: String,
}

/// One static UI surface declared at initialize.
#[derive(Debug, Clone)]
struct UiRef {
    plugin: String,
    surface: String,
    id: String,
    title: String,
}

/// Everything needed to start one plugin.
#[derive(Clone)]
struct PluginSpec {
    name: String,
    version: String,
    description: String,
    command: String,
    args: Vec<String>,
    /// Native library when this package uses `components.dylib` or
    /// `plugins.<name>.library`. Mutually exclusive with `command`.
    library: Option<PathBuf>,
    env: BTreeMap<String, String>,
    cwd: PathBuf,
    data_dir: PathBuf,
    timeout: Duration,
    capabilities: Vec<PluginCapability>,
    allowed_tools: Vec<String>,
    denied_tools: Vec<String>,
    settings: BTreeMap<String, Value>,
    settings_schema: Option<Value>,
    sandbox: bool,
    network_allow: Vec<String>,
    secrets: Vec<String>,
    rpc_allow: Vec<String>,
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

/// Crash-restart bookkeeping for one plugin that has been started.
#[derive(Debug, Clone, Default)]
struct Lifecycle {
    /// Consecutive automatic restarts since the last successful start.
    attempts: u32,
    /// When the supervisor may try again.
    next_start: Option<Instant>,
    /// Set when backoff is exhausted; only an explicit start clears it.
    gave_up: bool,
}

/// Automatic restarts are capped so a plugin that never initializes cannot
/// spin forever.
const MAX_RESTART_ATTEMPTS: u32 = 6;
const MAX_RESTART_BACKOFF: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct PluginStatus {
    name: String,
    version: String,
    description: String,
    enabled: bool,
    command: String,
    state: PluginState,
    tools: usize,
    commands: usize,
    hooks: usize,
    ui: usize,
    rpc: usize,
    events: Vec<String>,
    capabilities: Vec<PluginCapability>,
    /// `process`, `dylib` or `declarative`.
    backend: String,
    /// Consecutive backend failures; reset by a successful call.
    failures: u32,
    error: Option<String>,
    settings_schema: Option<Value>,
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
            hooks: self.hooks,
            ui: self.ui,
            rpc: self.rpc,
            events: self.events.clone(),
            capabilities: self.capabilities.clone(),
            failures: self.failures,
            error: self.error.clone(),
        }
    }
}

/// The daemon's plugin registry.
pub struct PluginManager {
    store: Arc<Store>,
    data_dir: PathBuf,
    /// One backend per running plugin instance. Process and dylib plugins
    /// share this map and the same registry.
    backends: Arc<Mutex<BTreeMap<String, Arc<dyn ExtensionBackend>>>>,
    /// Event patterns per running plugin, copied from `initialize`.
    subscriptions: Arc<Mutex<BTreeMap<String, Vec<String>>>>,
    tools: Mutex<Vec<ToolRef>>,
    commands: Mutex<Vec<CommandRef>>,
    hooks: Mutex<Vec<HookRef>>,
    ui: Mutex<Vec<UiRef>>,
    /// RPC methods per plugin, copied from `initialize`.
    rpc_methods: Arc<Mutex<BTreeMap<String, BTreeSet<String>>>>,
    statuses: Mutex<BTreeMap<String, PluginStatus>>,
    /// Log ring buffer shared with process, dylib and host API paths.
    logs: Arc<logs::PluginLogs>,
    /// Resolved specs of configured plugins, kept so the supervisor can
    /// restart a crashed backend without re-reading the config.
    specs: Mutex<BTreeMap<String, PluginSpec>>,
    /// Crash-restart state for plugins that have been started at least once.
    lifecycle: Mutex<BTreeMap<String, Lifecycle>>,
    /// Serialized `PluginConfig` from the last `load`, used to restart only
    /// the entries whose configuration changed.
    fingerprints: Mutex<BTreeMap<String, String>>,
    /// Set during shutdown so the supervisor does not restart anything.
    shutting_down: AtomicBool,
    event_tx: mpsc::Sender<PluginEvent>,
    rpc_tx: mpsc::Sender<RpcCallRequest>,
    bus: broadcast::Sender<Notification>,
    /// False when no running plugin subscribed to any event. `publish` is
    /// called for every daemon notification, so this keeps the common case
    /// cheap.
    has_event_subscribers: AtomicBool,
    has_hooks: AtomicBool,
}

impl PluginManager {
    pub fn new(
        store: Arc<Store>,
        data_dir: PathBuf,
        bus: broadcast::Sender<Notification>,
    ) -> Arc<Self> {
        // Bounded so a plugin event storm cannot grow memory without bound.
        // `publish` drops events under backpressure; events are best-effort
        // notifications, not durable state.
        let (event_tx, mut event_rx) = mpsc::channel::<PluginEvent>(1024);
        let backends: Arc<Mutex<BTreeMap<String, Arc<dyn ExtensionBackend>>>> =
            Arc::new(Mutex::new(BTreeMap::new()));
        let subscriptions = Arc::new(Mutex::new(BTreeMap::<String, Vec<String>>::new()));
        let event_backends = Arc::clone(&backends);
        let event_subscriptions = Arc::clone(&subscriptions);
        tokio::spawn(async move {
            while let Some(event) = event_rx.recv().await {
                let targets: Vec<_> = {
                    let backends = event_backends.lock().await;
                    let subscriptions = event_subscriptions.lock().await;
                    backends
                        .iter()
                        .filter(|(name, backend)| {
                            !backend.is_closed()
                                && subscriptions.get(*name).is_some_and(|patterns| {
                                    patterns
                                        .iter()
                                        .any(|pattern| wildcard(pattern, &event.method))
                                })
                        })
                        .map(|(_, backend)| Arc::clone(backend))
                        .collect()
                };
                for backend in targets {
                    if let Err(err) = backend.notify(&event.method, event.params.clone()).await {
                        eprintln!(
                            "eixd: plugin {} event {} failed: {}",
                            backend.name(),
                            event.method,
                            redact(&format!("{err:#}"))
                        );
                    }
                }
            }
        });

        let (rpc_tx, mut rpc_rx) = mpsc::channel::<RpcCallRequest>(64);
        let rpc_backends = Arc::clone(&backends);
        let rpc_methods = Arc::new(Mutex::new(BTreeMap::<String, BTreeSet<String>>::new()));
        let router_methods = Arc::clone(&rpc_methods);
        tokio::spawn(async move {
            while let Some(request) = rpc_rx.recv().await {
                let result = route_rpc(&request, &rpc_backends, &router_methods).await;
                let _ = request.reply.send(result);
            }
        });

        let log_buffer = Arc::new(logs::PluginLogs::new());
        let manager = Arc::new(Self {
            store,
            data_dir,
            backends,
            subscriptions,
            tools: Mutex::new(Vec::new()),
            commands: Mutex::new(Vec::new()),
            hooks: Mutex::new(Vec::new()),
            ui: Mutex::new(Vec::new()),
            rpc_methods,
            statuses: Mutex::new(BTreeMap::new()),
            logs: Arc::clone(&log_buffer),
            specs: Mutex::new(BTreeMap::new()),
            lifecycle: Mutex::new(BTreeMap::new()),
            fingerprints: Mutex::new(BTreeMap::new()),
            shutting_down: AtomicBool::new(false),
            event_tx,
            rpc_tx,
            bus,
            has_event_subscribers: AtomicBool::new(false),
            has_hooks: AtomicBool::new(false),
        });

        // A crashed process or dylib backend is detected here rather than on
        // the next RPC. Backoff keeps a plugin that dies during initialize
        // from respawning in a tight loop.
        let weak = Arc::downgrade(&manager);
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                let Some(manager) = weak.upgrade() else {
                    break;
                };
                manager.supervise().await;
            }
        });

        manager
    }

    /// Start enabled plugins, restarting only entries whose configuration
    /// changed since the previous load.
    pub async fn load(&self, config: &Config) {
        self.shutting_down.store(false, Ordering::Relaxed);
        let desired: BTreeMap<String, PluginConfig> = config.plugins.clone();

        // Drop plugins that disappeared from the config or were disabled.
        let running: Vec<String> = {
            let specs = self.specs.lock().await;
            specs.keys().cloned().collect()
        };
        for name in running {
            let keep = desired.get(&name).is_some_and(|plugin| plugin.enabled);
            if !keep {
                self.stop_one(&name, PluginState::Stopped).await;
                self.specs.lock().await.remove(&name);
                self.lifecycle.lock().await.remove(&name);
                self.fingerprints.lock().await.remove(&name);
            }
        }
        self.statuses
            .lock()
            .await
            .retain(|name, _| desired.contains_key(name));

        for (name, plugin) in &desired {
            let fingerprint = plugin_fingerprint(plugin);
            let unchanged = self
                .fingerprints
                .lock()
                .await
                .get(name)
                .is_some_and(|known| known == &fingerprint);
            let known = self.specs.lock().await.contains_key(name);
            if !plugin.enabled {
                self.set_disabled(name, plugin).await;
                continue;
            }
            // Keep an unchanged plugin exactly as it is: running, failed (no
            // retry loop on reload) or waiting for a lazy start.
            if unchanged && known {
                if let Some(status) = self.statuses.lock().await.get_mut(name) {
                    if status.state == PluginState::Disabled {
                        status.state = PluginState::Stopped;
                        status.enabled = true;
                    }
                }
                continue;
            }

            self.stop_one(name, PluginState::Stopped).await;
            match resolve_spec(name, plugin, &config.load_cwd, &self.data_dir) {
                Ok(spec) => {
                    self.specs.lock().await.insert(name.clone(), spec.clone());
                    self.fingerprints
                        .lock()
                        .await
                        .insert(name.clone(), fingerprint);
                    if plugin.lazy {
                        self.insert_lazy_status(&spec).await;
                    } else {
                        self.lifecycle
                            .lock()
                            .await
                            .insert(name.clone(), Lifecycle::default());
                        if let Err(err) = self.start_one(&spec).await {
                            self.mark_failed(name, plugin, &err).await;
                        }
                    }
                }
                Err(err) => {
                    self.insert_unresolved_status(name, plugin).await;
                    self.mark_failed(name, plugin, &err).await;
                }
            }
        }
    }

    async fn insert_lazy_status(&self, spec: &PluginSpec) {
        if let Some(status) = self.statuses.lock().await.get_mut(&spec.name) {
            status.state = PluginState::Stopped;
            status.enabled = true;
            status.error = Some("lazy: not started yet".into());
            return;
        }
        self.statuses.lock().await.insert(
            spec.name.clone(),
            PluginStatus {
                name: spec.name.clone(),
                version: spec.version.clone(),
                description: spec.description.clone(),
                enabled: true,
                command: spec.command.clone(),
                state: PluginState::Stopped,
                tools: 0,
                commands: 0,
                hooks: 0,
                ui: 0,
                rpc: 0,
                events: Vec::new(),
                capabilities: spec.capabilities.clone(),
                backend: backend_kind(spec),
                failures: 0,
                error: Some("lazy: not started yet".into()),
                settings_schema: spec.settings_schema.clone(),
            },
        );
    }

    async fn insert_unresolved_status(&self, name: &str, plugin: &PluginConfig) {
        self.statuses.lock().await.insert(
            name.to_string(),
            PluginStatus {
                name: name.to_string(),
                version: String::new(),
                description: String::new(),
                enabled: plugin.enabled,
                command: plugin.command.clone(),
                state: PluginState::Starting,
                tools: 0,
                commands: 0,
                hooks: 0,
                ui: 0,
                rpc: 0,
                events: Vec::new(),
                capabilities: plugin.capabilities.clone(),
                backend: if plugin.library.is_some() {
                    "dylib".into()
                } else {
                    "process".into()
                },
                failures: 0,
                error: None,
                settings_schema: None,
            },
        );
    }

    async fn set_disabled(&self, name: &str, plugin: &PluginConfig) {
        self.stop_one(name, PluginState::Disabled).await;
        self.specs.lock().await.remove(name);
        self.lifecycle.lock().await.remove(name);
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
                hooks: 0,
                ui: 0,
                rpc: 0,
                events: Vec::new(),
                capabilities: plugin.capabilities.clone(),
                backend: "disabled".into(),
                failures: 0,
                error: None,
                settings_schema: None,
            },
        );
    }

    async fn start_one(&self, spec: &PluginSpec) -> Result<()> {
        let declarative = self.declarative_commands(spec)?;
        if !spec.has_server {
            self.commit_declarative(spec, declarative).await?;
            return Ok(());
        }

        let backend = self.start_backend(spec).await?;
        let init = match backend.initialize(spec).await {
            Ok(init) => init,
            Err(err) => {
                backend.shutdown().await;
                return Err(err);
            }
        };
        match self
            .commit_runtime(spec, &backend, &init, declarative)
            .await
        {
            Ok(()) => Ok(()),
            Err(err) => {
                backend.shutdown().await;
                Err(err)
            }
        }
    }

    /// Build the backend selected by the spec: a native library or a child
    /// process. Both expose the same contribution protocol.
    async fn start_backend(&self, spec: &PluginSpec) -> Result<Arc<dyn ExtensionBackend>> {
        if let Some(library) = &spec.library {
            let backend = dylib::DylibBackend::load(
                library,
                spec,
                Arc::clone(&self.store),
                self.bus.clone(),
                Arc::clone(&self.logs),
            )
            .await
            .with_context(|| format!("plugin {} failed to start", spec.name))?;
            return Ok(backend);
        }
        let backend = process::ProcessBackend::spawn(
            spec,
            Arc::clone(&self.store),
            self.bus.clone(),
            self.rpc_tx.clone(),
            Arc::clone(&self.logs),
        )
        .await
        .with_context(|| format!("plugin {} failed to start", spec.name))?;
        Ok(backend)
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
            let action = command
                .action
                .as_deref()
                .map(str::trim)
                .filter(|action| !action.is_empty());
            let has_prompt = !command.prompt.trim().is_empty();
            if has_prompt == action.is_some() {
                bail!(
                    "plugin {} command {:?} needs exactly one of `prompt` or `action`",
                    spec.name,
                    command.name
                );
            }
            if let Some(action) = action {
                validate_rpc_name(action)?;
                if !spec.has_server {
                    bail!(
                        "plugin {} command {:?} uses an action but has no server",
                        spec.name,
                        command.name
                    );
                }
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
                action: action.map(str::to_string),
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
                hooks: 0,
                ui: 0,
                rpc: 0,
                events: Vec::new(),
                capabilities: spec.capabilities.clone(),
                backend: "declarative".into(),
                failures: 0,
                error: None,
                settings_schema: spec.settings_schema.clone(),
            },
        );
        Ok(())
    }

    async fn commit_runtime(
        &self,
        spec: &PluginSpec,
        backend: &Arc<dyn ExtensionBackend>,
        init: &PluginInitializeResult,
        declarative: Vec<CommandRef>,
    ) -> Result<()> {
        let capabilities: BTreeSet<PluginCapability> = spec.capabilities.iter().copied().collect();

        if !init.hooks.is_empty() && !capabilities.contains(&PluginCapability::Hooks) {
            bail!(
                "plugin {} declares {} hook(s) but was not granted `hooks`",
                spec.name,
                init.hooks.len()
            );
        }
        if !init.ui.is_empty() && !capabilities.contains(&PluginCapability::Ui) {
            bail!(
                "plugin {} declares UI contributions but was not granted `ui`",
                spec.name
            );
        }
        if !init.rpc.is_empty() && !capabilities.contains(&PluginCapability::Rpc) {
            bail!(
                "plugin {} declares RPC methods but was not granted `rpc`",
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
        let hooks = validate_hooks(spec, &init.hooks)?;
        let ui = validate_ui(spec, &init.ui)?;
        let rpc = validate_rpc(spec, &init.rpc)?;
        let mut commands = declarative;
        commands.extend(validate_commands(spec, &init.commands)?);

        self.check_tool_conflicts(&tools).await?;
        self.check_command_conflicts(&commands).await?;

        let tool_count = tools.len();
        let command_count = commands.len();
        let hook_count = hooks.len();
        let ui_count = ui.len();
        let rpc_count = rpc.len();
        self.tools.lock().await.extend(tools);
        self.commands.lock().await.extend(commands);
        self.hooks.lock().await.extend(hooks);
        self.ui.lock().await.extend(ui);
        self.rpc_methods.lock().await.insert(spec.name.clone(), rpc);
        self.backends
            .lock()
            .await
            .insert(spec.name.clone(), Arc::clone(backend));
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
        self.refresh_has_hooks().await;
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
                hooks: hook_count,
                ui: ui_count,
                rpc: rpc_count,
                events,
                capabilities: spec.capabilities.clone(),
                backend: backend_kind(spec),
                failures: 0,
                error: None,
                settings_schema: spec.settings_schema.clone(),
            },
        );
        Ok(())
    }

    async fn refresh_has_hooks(&self) {
        let has_hooks = !self.hooks.lock().await.is_empty();
        self.has_hooks.store(has_hooks, Ordering::Relaxed);
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
        self.logs.push(name, "error", &message);
        if let Some(status) = self.statuses.lock().await.get_mut(name) {
            status.state = PluginState::Failed;
            status.error = Some(message);
            status.tools = 0;
            status.commands = 0;
            status.failures = 0;
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
                    hooks: 0,
                    ui: 0,
                    rpc: 0,
                    events: Vec::new(),
                    capabilities: plugin.capabilities.clone(),
                    backend: if plugin.library.is_some() {
                        "dylib".into()
                    } else {
                        "process".into()
                    },
                    failures: 0,
                    error: Some(message),
                    settings_schema: None,
                },
            );
        }
    }

    /// Track consecutive backend failures and move a plugin between `ready`
    /// and `degraded`. Never overrides `failed`, `stopped` or `disabled`.
    async fn record_result(&self, name: &str, ok: bool, error: Option<&str>) {
        let mut statuses = self.statuses.lock().await;
        let Some(status) = statuses.get_mut(name) else {
            return;
        };
        if ok {
            status.failures = 0;
            let recovered = status.state == PluginState::Degraded;
            if recovered {
                status.state = PluginState::Ready;
                status.error = None;
            }
            drop(statuses);
            if recovered {
                self.logs
                    .push(name, "info", "recovered from degraded state");
            }
            return;
        }
        status.failures = status.failures.saturating_add(1);
        if matches!(status.state, PluginState::Ready | PluginState::Degraded)
            && status.failures >= DEGRADED_AFTER_FAILURES
        {
            status.state = PluginState::Degraded;
            let message = cap(error.unwrap_or("repeated backend failures"), 4096);
            status.error = Some(message.clone());
            drop(statuses);
            self.logs.push(name, "warn", &message);
        }
    }

    /// Detect backends that stopped without a request in flight.
    async fn refresh(&self) {
        let backends = { self.backends.lock().await.clone() };
        for (name, backend) in backends {
            if let Some(reason) = backend.exit_reason().await {
                self.drop_backend(&name, &backend, Some(reason)).await;
            }
        }
    }

    async fn drop_backend(
        &self,
        name: &str,
        backend: &Arc<dyn ExtensionBackend>,
        error: Option<String>,
    ) -> bool {
        let removed = {
            let mut backends = self.backends.lock().await;
            match backends.get(name) {
                Some(current) if Arc::ptr_eq(current, backend) => {
                    backends.remove(name);
                    true
                }
                _ => false,
            }
        };
        if !removed {
            return false;
        }
        backend.shutdown().await;
        self.clear_contributions(name).await;
        let message = error
            .as_deref()
            .map(redact)
            .unwrap_or_else(|| "plugin stopped unexpectedly".to_string());
        self.logs.push(name, "error", &message);
        if let Some(status) = self.statuses.lock().await.get_mut(name) {
            status.state = PluginState::Failed;
            status.error = Some(message);
            status.tools = 0;
            status.commands = 0;
            status.hooks = 0;
            status.ui = 0;
            status.rpc = 0;
            status.failures = 0;
            status.events.clear();
        }
        self.schedule_restart(name).await;
        true
    }

    /// Remove every registry entry owned by `name`. Shared by a crash, a
    /// graceful stop and a reload that only restarts one plugin.
    async fn clear_contributions(&self, name: &str) {
        self.tools.lock().await.retain(|tool| tool.plugin != name);
        self.commands
            .lock()
            .await
            .retain(|command| command.plugin != name);
        self.hooks.lock().await.retain(|hook| hook.plugin != name);
        self.ui.lock().await.retain(|ui| ui.plugin != name);
        self.rpc_methods.lock().await.remove(name);
        self.refresh_has_hooks().await;
        let mut subscriptions = self.subscriptions.lock().await;
        subscriptions.remove(name);
        self.has_event_subscribers
            .store(!subscriptions.is_empty(), Ordering::Relaxed);
    }

    /// Stop one plugin without treating it as a crash.
    async fn stop_one(&self, name: &str, state: PluginState) -> bool {
        let backend = self.backends.lock().await.remove(name);
        let Some(backend) = backend else {
            return false;
        };
        backend.shutdown().await;
        self.clear_contributions(name).await;
        if let Some(status) = self.statuses.lock().await.get_mut(name) {
            status.state = state;
            status.tools = 0;
            status.commands = 0;
            status.hooks = 0;
            status.ui = 0;
            status.rpc = 0;
            status.failures = 0;
            status.events.clear();
            if state == PluginState::Stopped {
                status.error = None;
            }
        }
        true
    }

    /// Schedule a restart with 1s/2s/4s/8s/16s/30s backoff.
    async fn schedule_restart(&self, name: &str) {
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        let Some(spec) = self.specs.lock().await.get(name).cloned() else {
            return;
        };
        if !spec.has_server {
            return;
        }
        let mut lifecycle = self.lifecycle.lock().await;
        let entry = lifecycle.entry(name.to_string()).or_default();
        if entry.gave_up {
            return;
        }
        entry.attempts = entry.attempts.saturating_add(1);
        if entry.attempts > MAX_RESTART_ATTEMPTS {
            entry.gave_up = true;
            entry.next_start = None;
            drop(lifecycle);
            self.logs.push(
                name,
                "error",
                "crash-restart budget exhausted; use /plugins restart to retry",
            );
            return;
        }
        let delay =
            Duration::from_secs(1u64 << (entry.attempts - 1).min(4)).min(MAX_RESTART_BACKOFF);
        entry.next_start = Some(Instant::now() + delay);
        drop(lifecycle);
        self.logs
            .push(name, "warn", &format!("restarting in {}s", delay.as_secs()));
    }

    /// Background supervisor: detect dead backends and retry due restarts.
    async fn supervise(&self) {
        if self.shutting_down.load(Ordering::Relaxed) {
            return;
        }
        self.refresh().await;
        let due: Vec<PluginSpec> = {
            let mut lifecycle = self.lifecycle.lock().await;
            let now = Instant::now();
            let mut due = Vec::new();
            for (name, entry) in lifecycle.iter_mut() {
                if entry.gave_up || entry.next_start.is_none_or(|at| at > now) {
                    continue;
                }
                if !self.specs.lock().await.contains_key(name) {
                    continue;
                }
                entry.next_start = None;
                if let Some(spec) = self.specs.lock().await.get(name) {
                    due.push(spec.clone());
                }
            }
            due
        };
        for spec in due {
            // A plugin may have been started explicitly while the supervisor
            // was waiting.
            if self.backends.lock().await.contains_key(&spec.name) {
                continue;
            }
            self.logs.push(&spec.name, "info", "restarting after crash");
            match self.start_one(&spec).await {
                Ok(()) => {
                    let mut lifecycle = self.lifecycle.lock().await;
                    lifecycle.insert(spec.name.clone(), Lifecycle::default());
                    drop(lifecycle);
                    self.logs.push(&spec.name, "info", "restarted");
                }
                Err(err) => {
                    let message = redact(&format!("{err:#}"));
                    self.logs
                        .push(&spec.name, "error", &format!("restart failed: {message}"));
                    self.mark_failed(&spec.name, &PluginConfig::default(), &err)
                        .await;
                    self.schedule_restart(&spec.name).await;
                }
            }
        }
    }

    /// Stop every running backend before the registry is rebuilt.
    async fn stop_all(&self) {
        let backends: Vec<_> = {
            let mut backends = self.backends.lock().await;
            std::mem::take(&mut *backends).into_values().collect()
        };
        for backend in backends {
            backend.shutdown().await;
        }
    }

    pub async fn list(&self) -> PluginListResult {
        self.refresh().await;
        let statuses = self.statuses.lock().await;
        PluginListResult {
            plugins: statuses.values().map(PluginStatus::info).collect(),
        }
    }

    /// Fetch ring-buffer logs. `since` is the `next` value from a previous
    /// call.
    pub fn logs(&self, params: &PluginLogsParams) -> PluginLogsResult {
        self.logs
            .snapshot(params.name.as_deref(), params.since, params.limit)
    }

    pub fn clear_logs(&self, name: Option<&str>) {
        self.logs.clear(name);
    }

    /// Per-plugin diagnostics for `plugin.doctor`.
    pub async fn doctor(&self) -> PluginDoctorResult {
        self.refresh().await;
        let statuses = self.statuses.lock().await.clone();
        let mut plugins = Vec::with_capacity(statuses.len());
        for status in statuses.into_values() {
            let mut checks = Vec::new();
            checks.push(PluginDoctorCheck {
                name: "enabled".into(),
                ok: status.enabled,
                detail: if status.enabled {
                    "plugin is enabled".into()
                } else {
                    "plugin is disabled in config".into()
                },
            });
            checks.push(PluginDoctorCheck {
                name: "state".into(),
                ok: matches!(status.state, PluginState::Ready | PluginState::Stopped),
                detail: format!("state is {:?}", status.state).to_lowercase(),
            });
            checks.push(PluginDoctorCheck {
                name: "backend".into(),
                ok: status.backend != "disabled",
                detail: format!("{} backend", status.backend),
            });
            checks.push(PluginDoctorCheck {
                name: "failures".into(),
                ok: status.failures < DEGRADED_AFTER_FAILURES,
                detail: format!("{} consecutive failure(s)", status.failures),
            });
            let errors = self.logs.error_count(&status.name);
            checks.push(PluginDoctorCheck {
                name: "logs".into(),
                ok: errors == 0,
                detail: format!("{errors} error line(s) buffered"),
            });
            if let Some(schema) = &status.settings_schema {
                let valid = schema.get("type").and_then(Value::as_str) == Some("object");
                checks.push(PluginDoctorCheck {
                    name: "settings_schema".into(),
                    ok: valid,
                    detail: if valid {
                        "settings schema is an object".into()
                    } else {
                        "settings schema is not a JSON object schema".into()
                    },
                });
            }
            let last_log = self.logs.last(&status.name).map(|entry| entry.message);
            plugins.push(PluginDoctorEntry {
                name: status.name.clone(),
                state: status.state,
                enabled: status.enabled,
                version: status.version.clone(),
                backend: status.backend.clone(),
                failures: status.failures,
                error: status.error.clone(),
                tools: status.tools,
                commands: status.commands,
                hooks: status.hooks,
                ui: status.ui,
                rpc: status.rpc,
                events: status.events.clone(),
                capabilities: status.capabilities.clone(),
                log_lines: self.logs.len(&status.name),
                last_log,
                checks,
            });
        }
        PluginDoctorResult { plugins }
    }

    /// Start one configured plugin by name. Used by `plugin.start` and the
    /// TUI after `plugin.enable`.
    pub async fn start(&self, name: &str, config: &Config) -> Result<()> {
        let plugin = config
            .plugins
            .get(name)
            .with_context(|| format!("unknown plugin {name:?}"))?;
        if !plugin.enabled {
            bail!("plugin {name:?} is disabled");
        }
        if self.backends.lock().await.contains_key(name) {
            return Ok(());
        }
        let spec = resolve_spec(name, plugin, &config.load_cwd, &self.data_dir)?;
        self.specs
            .lock()
            .await
            .insert(name.to_string(), spec.clone());
        self.fingerprints
            .lock()
            .await
            .insert(name.to_string(), plugin_fingerprint(plugin));
        self.lifecycle
            .lock()
            .await
            .insert(name.to_string(), Lifecycle::default());
        self.statuses.lock().await.remove(name);
        match self.start_one(&spec).await {
            Ok(()) => {
                self.logs.push(name, "info", "started");
                Ok(())
            }
            Err(err) => {
                self.insert_unresolved_status(name, plugin).await;
                self.mark_failed(name, plugin, &err).await;
                Err(err)
            }
        }
    }

    /// Stop one plugin without disabling it in the config.
    pub async fn stop(&self, name: &str) -> Result<()> {
        self.lifecycle.lock().await.remove(name);
        if !self.stop_one(name, PluginState::Stopped).await {
            if let Some(status) = self.statuses.lock().await.get_mut(name) {
                status.state = PluginState::Stopped;
                status.tools = 0;
                status.commands = 0;
                status.hooks = 0;
                status.ui = 0;
                status.rpc = 0;
                status.events.clear();
            }
        }
        self.logs.push(name, "info", "stopped");
        Ok(())
    }

    /// Stop one plugin and mark it disabled.
    pub async fn disable(&self, name: &str, plugin: &PluginConfig) {
        self.lifecycle.lock().await.remove(name);
        self.specs.lock().await.remove(name);
        self.set_disabled(name, plugin).await;
        self.logs.push(name, "info", "disabled");
    }

    /// Drop all state for a plugin that is being removed. The caller removes
    /// it from the config and deletes the installed files.
    pub async fn forget(&self, name: &str) {
        self.stop(&name).await.ok();
        self.specs.lock().await.remove(name);
        self.lifecycle.lock().await.remove(name);
        self.fingerprints.lock().await.remove(name);
        self.statuses.lock().await.remove(name);
        self.logs.clear(Some(name));
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
                    action: command.action.is_some(),
                })
                .collect(),
        }
    }

    /// Declared text surfaces. The TUI uses these to label plugin panels and
    /// to decide whether a `plugin.ui.open` event is expected.
    pub async fn ui_surfaces(&self) -> PluginUiListResult {
        self.refresh().await;
        PluginUiListResult {
            surfaces: self
                .ui
                .lock()
                .await
                .iter()
                .map(|surface| PluginUiSurface {
                    plugin: surface.plugin.clone(),
                    surface: surface.surface.clone(),
                    id: surface.id.clone(),
                    title: surface.title.clone(),
                })
                .collect(),
        }
    }

    /// Ask a plugin for the current body of one of its declared surfaces.
    pub async fn render_ui(&self, params: &PluginUiRenderParams) -> Result<PluginUiRenderResult> {
        if !valid_ui_surface(&params.surface) {
            bail!("unknown UI surface {:?}", params.surface);
        }
        let backend = self
            .backends
            .lock()
            .await
            .get(&params.plugin)
            .cloned()
            .with_context(|| format!("plugin {} is not running", params.plugin))?;
        let value = backend
            .request(
                "ui/render",
                serde_json::json!({"surface": params.surface, "id": params.id}),
            )
            .await?;
        let result: PluginUiRenderResult =
            serde_json::from_value(value).context("invalid ui/render result")?;
        if let Some(node) = &result.node {
            node.validate().map_err(|err| anyhow::anyhow!(err))?;
        }
        if result.body.len() > MAX_PLUGIN_OUTPUT {
            bail!("ui/render body is too large");
        }
        self.record_result(&params.plugin, true, None).await;
        Ok(result)
    }

    /// Forward a button/form action to the owning plugin.
    pub async fn ui_action(&self, params: &PluginUiActionParams) -> Result<PluginUiActionResult> {
        let backend = self
            .backends
            .lock()
            .await
            .get(&params.plugin)
            .cloned()
            .with_context(|| format!("plugin {} is not running", params.plugin))?;
        let value = backend
            .request(
                "ui/action",
                serde_json::json!({
                    "id": params.id,
                    "action": params.action,
                    "values": params.values,
                }),
            )
            .await;
        let value = match value {
            Ok(value) => value,
            Err(err) => {
                self.record_result(&params.plugin, false, Some(&format!("{err:#}")))
                    .await;
                return Err(err);
            }
        };
        let result: PluginUiActionResult =
            serde_json::from_value(value).context("invalid ui/action result")?;
        if let Some(node) = &result.node {
            node.validate().map_err(|err| anyhow::anyhow!(err))?;
        }
        if result
            .body
            .as_ref()
            .is_some_and(|body| body.len() > MAX_PLUGIN_OUTPUT)
        {
            bail!("ui/action body is too large");
        }
        self.record_result(&params.plugin, true, None).await;
        Ok(result)
    }

    /// Ask a plugin picker provider for its rows.
    pub async fn ui_pick(&self, params: &PluginUiPickParams) -> Result<PluginUiPickResult> {
        let backend = self
            .backends
            .lock()
            .await
            .get(&params.plugin)
            .cloned()
            .with_context(|| format!("plugin {} is not running", params.plugin))?;
        let value = backend
            .request(
                "ui/pick",
                serde_json::json!({"id": params.id, "query": params.query}),
            )
            .await?;
        let result: PluginUiPickResult =
            serde_json::from_value(value).context("invalid ui/pick result")?;
        if result.items.len() > 1000 {
            bail!("ui/pick returned more than 1000 items");
        }
        for item in &result.items {
            if item.id.is_empty() || item.id.len() > 128 {
                bail!("ui/pick item id must be 1..=128 bytes");
            }
        }
        self.record_result(&params.plugin, true, None).await;
        Ok(result)
    }

    pub async fn run_command(
        &self,
        name: &str,
        arguments: &str,
        session_id: &str,
        cwd: &Path,
    ) -> Result<PluginCommandRunResult> {
        let command = self
            .commands
            .lock()
            .await
            .iter()
            .find(|command| command.exposed == name)
            .cloned()
            .with_context(|| format!("unknown plugin command {name:?}"))?;
        let action = command
            .action
            .clone()
            .with_context(|| format!("plugin command {name:?} submits a prompt directly"))?;
        let backend = self
            .backends
            .lock()
            .await
            .get(&command.plugin)
            .cloned()
            .with_context(|| format!("plugin {} is not running", command.plugin))?;
        let value = match backend
            .request(
                "commands/run",
                serde_json::json!({
                    "command": action,
                    "arguments": arguments,
                    "session_id": session_id,
                    "workspace": cwd,
                    "cwd": cwd,
                }),
            )
            .await
        {
            Ok(value) => {
                self.record_result(&command.plugin, true, None).await;
                value
            }
            Err(err) => {
                let message = redact(&format!("{err:#}"));
                self.record_result(&command.plugin, false, Some(&message))
                    .await;
                return Err(err)
                    .with_context(|| format!("plugin {} command {name} failed", command.plugin));
            }
        };
        let mut result: PluginCommandRunResult = match serde_json::from_value(value) {
            Ok(result) => result,
            Err(err) => {
                self.record_result(&command.plugin, false, Some("invalid command result"))
                    .await;
                return Err(err).context("invalid plugin command result");
            }
        };
        if let Some(prompt) = &result.prompt {
            if prompt.len() > MAX_COMMAND_PROMPT {
                bail!("plugin command {name:?} returned an oversized prompt");
            }
        }
        if let Some(output) = &result.output {
            if output.len() > MAX_PLUGIN_OUTPUT {
                bail!("plugin command {name:?} returned oversized output");
            }
        }
        if !result.ok {
            result.ok = false;
        }
        Ok(result)
    }

    pub async fn settings(&self, name: &str, config: &Config) -> Result<PluginSettingsResult> {
        let status = self
            .statuses
            .lock()
            .await
            .get(name)
            .cloned()
            .with_context(|| format!("unknown plugin {name:?}"))?;
        let plugin = config
            .plugins
            .get(name)
            .with_context(|| format!("unknown plugin {name:?}"))?;
        Ok(PluginSettingsResult {
            plugin: name.to_string(),
            schema: status.settings_schema.clone(),
            values: serde_json::to_value(&plugin.settings).unwrap_or(Value::Null),
        })
    }

    /// Apply a shallow settings update, persist it and notify the running
    /// plugin. `null` removes a key; validation uses the manifest schema.
    pub async fn set_settings(
        &self,
        name: &str,
        updates: &Value,
        config: &Config,
    ) -> Result<PluginSettingsResult> {
        let status = self
            .statuses
            .lock()
            .await
            .get(name)
            .cloned()
            .with_context(|| format!("unknown plugin {name:?}"))?;
        let plugin = config
            .plugins
            .get(name)
            .with_context(|| format!("unknown plugin {name:?}"))?;
        let updates = updates
            .as_object()
            .context("plugin.settings.set needs an object `values`")?;
        let mut merged = plugin.settings.clone();
        for (key, value) in updates {
            if value.is_null() {
                merged.remove(key);
            } else {
                merged.insert(key.clone(), value.clone());
            }
        }
        let values = serde_json::to_value(&merged)?;
        if let Some(schema) = &status.settings_schema {
            let values_for_validation = values.clone();
            eix_core::plugin::validate_plugin_settings(schema, &values_for_validation)
                .map_err(|err| anyhow::anyhow!("invalid settings for {name}: {err}"))?;
        }
        let path = config.write_plugin_settings(name, &merged)?;

        if let Some(backend) = self.backends.lock().await.get(name).cloned() {
            let notification = serde_json::json!({
                "plugin": name,
                "settings": values,
                "path": path,
            });
            if let Err(err) = backend.notify("config.changed", notification).await {
                // Persisting succeeded; a plugin that cannot take the update
                // will pick it up on the next restart.
                eprintln!(
                    "eixd: plugin {name} config.changed failed: {}",
                    redact(&format!("{err:#}"))
                );
            }
        }
        Ok(PluginSettingsResult {
            plugin: name.to_string(),
            schema: status.settings_schema,
            values,
        })
    }

    /// Run `tool.before` hooks. Returns a denial reason when a hook vetoes the
    /// call. Hooks can never approve a tool that normal policy asks about.
    pub async fn run_tool_before(
        &self,
        session_id: &str,
        turn_id: &str,
        tool: &str,
        arguments: &Value,
        cwd: &Path,
    ) -> Option<String> {
        if !self.has_hooks.load(Ordering::Relaxed) {
            return None;
        }
        let hooks = self.hook_refs("tool.before").await;
        for hook in hooks {
            let Some(backend) = self.hook_backend(&hook).await else {
                continue;
            };
            let request = serde_json::json!({
                "point": "tool.before",
                "session_id": session_id,
                "turn_id": turn_id,
                "tool": tool,
                "arguments": arguments,
                "workspace": cwd,
                "cwd": cwd,
            });
            match call_hook(&backend, &hook, request).await {
                Ok(value) => {
                    self.record_result(&hook.plugin, true, None).await;
                    let deny = value.get("action").and_then(Value::as_str) == Some("deny")
                        || value.get("decision").and_then(Value::as_str) == Some("deny")
                        || value.get("deny").and_then(Value::as_bool) == Some(true)
                        || value.get("allow").and_then(Value::as_bool) == Some(false);
                    if deny {
                        let reason = value
                            .get("reason")
                            .and_then(Value::as_str)
                            .filter(|reason| !reason.trim().is_empty())
                            .unwrap_or("denied by plugin hook");
                        return Some(cap(reason, 4096));
                    }
                }
                Err(err) => {
                    let message = redact(&format!("{err:#}"));
                    eprintln!(
                        "eixd: plugin {} hook tool.before failed: {message}",
                        hook.plugin
                    );
                    self.record_result(&hook.plugin, false, Some(&message))
                        .await;
                    if hook.on_error == "deny" {
                        return Some(format!("plugin {} hook failed", hook.plugin));
                    }
                }
            }
        }
        None
    }

    /// Run `tool.after` hooks. The last non-empty `output` replacement wins.
    #[allow(clippy::too_many_arguments)]
    pub async fn run_tool_after(
        &self,
        session_id: &str,
        turn_id: &str,
        tool: &str,
        ok: bool,
        denied: bool,
        output: &str,
        cwd: &Path,
    ) -> Option<String> {
        if !self.has_hooks.load(Ordering::Relaxed) {
            return None;
        }
        let hooks = self.hook_refs("tool.after").await;
        let mut current = output.to_string();
        let mut replaced = false;
        for hook in hooks {
            let Some(backend) = self.hook_backend(&hook).await else {
                continue;
            };
            let request = serde_json::json!({
                "point": "tool.after",
                "session_id": session_id,
                "turn_id": turn_id,
                "tool": tool,
                "ok": ok,
                "denied": denied,
                "output": current,
                "workspace": cwd,
                "cwd": cwd,
            });
            match call_hook(&backend, &hook, request).await {
                Ok(value) => {
                    self.record_result(&hook.plugin, true, None).await;
                    if let Some(output) = value.get("output").and_then(Value::as_str) {
                        current = cap(output, MAX_PLUGIN_OUTPUT);
                        replaced = true;
                    }
                }
                Err(err) => {
                    let message = redact(&format!("{err:#}"));
                    eprintln!(
                        "eixd: plugin {} hook tool.after failed: {message}",
                        hook.plugin
                    );
                    self.record_result(&hook.plugin, false, Some(&message))
                        .await;
                }
            }
        }
        replaced.then_some(current)
    }

    /// Run `prompt.build` hooks sequentially. Each hook may return `prompt`;
    /// the value is size-checked before the next hook sees it.
    pub async fn run_prompt_build(&self, session_id: &str, prompt: &str) -> String {
        if !self.has_hooks.load(Ordering::Relaxed) {
            return prompt.to_string();
        }
        let mut current = prompt.to_string();
        for hook in self.hook_refs("prompt.build").await {
            let Some(backend) = self.hook_backend(&hook).await else {
                continue;
            };
            let request = serde_json::json!({
                "point": "prompt.build",
                "session_id": session_id,
                "prompt": current,
            });
            match call_hook(&backend, &hook, request).await {
                Ok(value) => {
                    self.record_result(&hook.plugin, true, None).await;
                    if let Some(prompt) = value.get("prompt").and_then(Value::as_str) {
                        if prompt.trim().is_empty() {
                            continue;
                        }
                        if prompt.len() > MAX_COMMAND_PROMPT {
                            eprintln!(
                                "eixd: plugin {} hook prompt.build returned an oversized prompt",
                                hook.plugin
                            );
                            continue;
                        }
                        current = prompt.to_string();
                    }
                }
                Err(err) => {
                    let message = redact(&format!("{err:#}"));
                    eprintln!(
                        "eixd: plugin {} hook prompt.build failed: {message}",
                        hook.plugin
                    );
                    self.record_result(&hook.plugin, false, Some(&message))
                        .await;
                }
            }
        }
        current
    }

    /// Run `turn.stop` hooks before a turn is allowed to finish. A hook that
    /// returns `{"block": true, "reason": "..."}` keeps the turn alive: the
    /// daemon feeds the reason back to the model and samples again. This is
    /// for team policies such as "tests must pass before you stop".
    pub async fn run_turn_stop(
        &self,
        session_id: &str,
        turn_id: &str,
        message: &str,
        cwd: &Path,
    ) -> Option<String> {
        if !self.has_hooks.load(Ordering::Relaxed) {
            return None;
        }
        for hook in self.hook_refs("turn.stop").await {
            let Some(backend) = self.hook_backend(&hook).await else {
                continue;
            };
            let request = serde_json::json!({
                "point": "turn.stop",
                "session_id": session_id,
                "turn_id": turn_id,
                "message": message,
                "workspace": cwd,
                "cwd": cwd,
            });
            match call_hook(&backend, &hook, request).await {
                Ok(value) => {
                    self.record_result(&hook.plugin, true, None).await;
                    if let Some(reason) = turn_stop_reason(&value) {
                        return Some(cap(&reason, 4096));
                    }
                }
                Err(err) => {
                    let message = redact(&format!("{err:#}"));
                    eprintln!(
                        "eixd: plugin {} hook turn.stop failed: {message}",
                        hook.plugin
                    );
                    self.record_result(&hook.plugin, false, Some(&message))
                        .await;
                    if hook.on_error == "deny" {
                        return Some(format!("plugin {} hook failed", hook.plugin));
                    }
                }
            }
        }
        None
    }

    /// Fire-and-forget `turn.end` notification. Turn completion must not wait
    /// on plugin bookkeeping.
    pub fn run_turn_end(
        self: &Arc<Self>,
        session_id: &str,
        turn_id: &str,
        ok: bool,
        error: Option<String>,
    ) {
        if !self.has_hooks.load(Ordering::Relaxed) {
            return;
        }
        let manager = Arc::clone(self);
        let session_id = session_id.to_string();
        let turn_id = turn_id.to_string();
        tokio::spawn(async move {
            for hook in manager.hook_refs("turn.end").await {
                let Some(backend) = manager.hook_backend(&hook).await else {
                    continue;
                };
                let request = serde_json::json!({
                    "point": "turn.end",
                    "session_id": session_id,
                    "turn_id": turn_id,
                    "ok": ok,
                    "error": error,
                });
                match call_hook(&backend, &hook, request).await {
                    Ok(_) => manager.record_result(&hook.plugin, true, None).await,
                    Err(err) => {
                        let message = redact(&format!("{err:#}"));
                        eprintln!(
                            "eixd: plugin {} hook turn.end failed: {message}",
                            hook.plugin
                        );
                        manager
                            .record_result(&hook.plugin, false, Some(&message))
                            .await;
                    }
                }
            }
        });
    }

    async fn hook_refs(&self, point: &str) -> Vec<HookRef> {
        let mut hooks: Vec<_> = self
            .hooks
            .lock()
            .await
            .iter()
            .filter(|hook| hook.point == point)
            .cloned()
            .collect();
        hooks.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| left.plugin.cmp(&right.plugin))
        });
        hooks
    }

    async fn hook_backend(&self, hook: &HookRef) -> Option<Arc<dyn ExtensionBackend>> {
        let backend = self.backends.lock().await.get(&hook.plugin).cloned()?;
        (!backend.is_closed()).then_some(backend)
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
        let Some(backend) = self.backends.lock().await.get(&tool.plugin).cloned() else {
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
        let result = backend
            .request(
                "tools/call",
                serde_json::to_value(params).unwrap_or(Value::Null),
            )
            .await;
        let duration_ms = started.elapsed().as_millis() as u64;
        match result {
            Ok(value) => match serde_json::from_value::<PluginToolCallResult>(value) {
                Ok(result) => {
                    self.record_result(&tool.plugin, true, None).await;
                    ToolOutcome {
                        ok: result.ok && !result.is_error,
                        output: cap(&result.to_text(), max_output_bytes.min(MAX_PLUGIN_OUTPUT)),
                        duration_ms,
                    }
                }
                Err(err) => {
                    let message = format!(
                        "plugin {} returned an invalid tool result: {err}",
                        tool.plugin
                    );
                    self.record_result(&tool.plugin, false, Some(&message))
                        .await;
                    ToolOutcome {
                        ok: false,
                        output: message,
                        duration_ms,
                    }
                }
            },
            Err(err) => {
                let message = redact(&format!("plugin {} failed: {err:#}", tool.plugin));
                self.record_result(&tool.plugin, false, Some(&message))
                    .await;
                // A per-call timeout cancels the request but leaves the
                // plugin running; only a closed backend removes it.
                if let Some(reason) = backend.exit_reason().await {
                    self.drop_backend(&tool.plugin, &backend, Some(reason))
                        .await;
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
        self.stop_one(name, PluginState::Stopped).await;
        self.lifecycle.lock().await.remove(name);
        self.statuses.lock().await.remove(name);
        if !plugin.enabled {
            self.set_disabled(name, plugin).await;
            return Ok(());
        }
        let spec = match resolve_spec(name, plugin, &config.load_cwd, &self.data_dir) {
            Ok(spec) => spec,
            Err(err) => {
                self.mark_failed(name, plugin, &err).await;
                return Err(anyhow::anyhow!(redact(&format!("{err:#}"))));
            }
        };
        self.specs
            .lock()
            .await
            .insert(name.to_string(), spec.clone());
        self.fingerprints
            .lock()
            .await
            .insert(name.to_string(), plugin_fingerprint(plugin));
        self.lifecycle
            .lock()
            .await
            .insert(name.to_string(), Lifecycle::default());
        if let Err(err) = self.start_one(&spec).await {
            self.insert_unresolved_status(name, plugin).await;
            self.mark_failed(name, plugin, &err).await;
            return Err(anyhow::anyhow!(redact(&format!("{err:#}"))));
        }
        self.logs.push(name, "info", "restarted on request");
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
        self.shutting_down.store(true, Ordering::Relaxed);
        self.has_event_subscribers.store(false, Ordering::Relaxed);
        self.has_hooks.store(false, Ordering::Relaxed);
        self.lifecycle.lock().await.clear();
        self.stop_all().await;
        self.subscriptions.lock().await.clear();
        self.tools.lock().await.clear();
        self.commands.lock().await.clear();
        self.hooks.lock().await.clear();
        self.ui.lock().await.clear();
        self.rpc_methods.lock().await.clear();
        let mut statuses = self.statuses.lock().await;
        for status in statuses.values_mut() {
            status.state = PluginState::Stopped;
            status.tools = 0;
            status.commands = 0;
            status.hooks = 0;
            status.ui = 0;
            status.rpc = 0;
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
        // Packages that declare `[package]` are verified even without
        // `require_signature`; the option only changes whether an unsigned
        // package is accepted.
        if plugin.require_signature || manifest.package.is_some() {
            let trust_path = eix_core::config::config_dir().join("plugin-trust.toml");
            let trust = eix_core::PluginTrustStore::load(&trust_path)
                .with_context(|| format!("cannot read {}", trust_path.display()))?;
            let directory = path.parent().unwrap_or(&cwd);
            eix_core::verify_plugin(directory, manifest, &trust, !plugin.require_signature)
                .with_context(|| format!("plugin {name} failed signature verification"))?;
        }
        // `components.ui` is a packaging declaration. Text surfaces arrive
        // through `initialize.ui` and `host.ui.*`; the child process itself
        // still runs as a server component with the `ui` capability.
        if manifest.components.ui.is_some()
            && manifest.components.server.is_none()
            && manifest.components.dylib.is_none()
        {
            eprintln!(
                "eixd: plugin {name}: components.ui is declared but only server components start in this build"
            );
        }
    }
    let settings_schema = manifest
        .as_ref()
        .map(|(path, manifest)| load_settings_schema(path, manifest))
        .transpose()?
        .flatten();

    // A native library can come from the config key or the manifest. An
    // explicit `library` wins, but declaring both a dylib and a process is a
    // configuration error rather than a silent choice.
    let library = match &plugin.library {
        Some(path) => Some(if path.is_absolute() {
            path.clone()
        } else {
            cwd.join(path)
        }),
        None => manifest
            .as_ref()
            .and_then(|(path, manifest)| manifest.dylib_path(path.parent().unwrap_or(&cwd))),
    };
    let process_command = if !plugin.command.trim().is_empty() {
        Some((plugin.command.clone(), plugin.args.clone()))
    } else {
        manifest
            .as_ref()
            .and_then(|(path, manifest)| manifest.server_command(path.parent().unwrap_or(&cwd)))
    };
    if library.is_some() && process_command.is_some() {
        bail!("plugin {name} declares both a dylib library and a process command");
    }
    if library.is_none() && process_command.is_none() && manifest.is_none() {
        bail!("plugin {name} needs `command`, `library` or `manifest`");
    }
    let (command, args, has_server) = if let Some(library) = &library {
        (library.display().to_string(), Vec::new(), true)
    } else if let Some((command, args)) = process_command {
        let has_server = !command.trim().is_empty();
        (command, args, has_server)
    } else {
        (String::new(), Vec::new(), false)
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
        library,
        env: plugin.env.clone(),
        cwd: plugin_cwd,
        data_dir: data_dir.join("plugins").join(name),
        timeout: Duration::from_secs(plugin.timeout_secs.max(1)),
        capabilities,
        allowed_tools: plugin.allowed_tools.clone(),
        denied_tools: plugin.denied_tools.clone(),
        settings: plugin.settings.clone(),
        settings_schema,
        sandbox: plugin.sandbox,
        network_allow: plugin.network_allow.clone(),
        secrets: plugin.secrets.clone(),
        rpc_allow: plugin.rpc_allow.clone(),
        has_server,
        declarative_commands,
    })
}

/// Load and validate the JSON Schema referenced by the manifest. The file must
/// stay inside the package directory; a plugin cannot point at arbitrary host
/// files.
fn load_settings_schema(manifest_path: &Path, manifest: &PluginManifest) -> Result<Option<Value>> {
    let Some(relative) = &manifest.settings else {
        return Ok(None);
    };
    let base = manifest_path.parent().unwrap_or(Path::new("."));
    let base = base
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", base.display()))?;
    let path = if relative.is_absolute() {
        relative.clone()
    } else {
        base.join(relative)
    };
    let path = path
        .canonicalize()
        .with_context(|| format!("cannot resolve settings schema {}", path.display()))?;
    if !path.starts_with(&base) {
        bail!(
            "settings schema {} is outside the plugin directory",
            path.display()
        );
    }
    let meta = std::fs::metadata(&path)
        .with_context(|| format!("cannot stat settings schema {}", path.display()))?;
    if meta.len() as usize > MAX_SETTINGS_SCHEMA {
        bail!("settings schema exceeds {MAX_SETTINGS_SCHEMA} bytes");
    }
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("cannot read settings schema {}", path.display()))?;
    let schema: Value = serde_json::from_str(&text)
        .with_context(|| format!("invalid settings schema {}", path.display()))?;
    if !schema.is_object() {
        bail!("settings schema {} must be a JSON object", path.display());
    }
    if schema.get("type").and_then(Value::as_str) != Some("object") {
        bail!(
            "settings schema {} must declare \"type\": \"object\"",
            path.display()
        );
    }
    Ok(Some(schema))
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
            .filter(|prompt| !prompt.trim().is_empty());
        let action = command
            .action
            .as_deref()
            .map(str::trim)
            .filter(|action| !action.is_empty());
        if prompt.is_some() == action.is_some() {
            bail!(
                "plugin {} command {:?} needs exactly one of `prompt` or `action`",
                spec.name,
                command.name
            );
        }
        if let Some(action) = action {
            validate_rpc_name(action)?;
            if !spec.has_server {
                bail!(
                    "plugin {} command {:?} uses an action but has no server",
                    spec.name,
                    command.name
                );
            }
        }
        let prompt = prompt.unwrap_or_default();
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
            action: action.map(str::to_string),
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

fn validate_rpc_name(name: &str) -> Result<()> {
    if !valid_tool_name(name) {
        bail!(
            "invalid RPC method name {:?}: use letters, digits, '.', '_' or '-', without '__'",
            name
        );
    }
    Ok(())
}

fn validate_hooks(spec: &PluginSpec, hooks: &[PluginHook]) -> Result<Vec<HookRef>> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for hook in hooks {
        if !valid_hook_point(&hook.point) {
            bail!(
                "plugin {} hook {:?} is not a supported hook point",
                spec.name,
                hook.point
            );
        }
        if !seen.insert(hook.point.clone()) {
            bail!(
                "plugin {} registers hook {:?} more than once",
                spec.name,
                hook.point
            );
        }
        let on_error = hook.on_error.trim();
        let on_error = if on_error.is_empty() {
            "allow"
        } else {
            on_error
        };
        if !matches!(on_error, "allow" | "deny" | "ignore") {
            bail!(
                "plugin {} hook {:?} on_error must be allow, deny or ignore",
                spec.name,
                hook.point
            );
        }
        if hook.timeout_ms > 300_000 {
            bail!(
                "plugin {} hook {:?} timeout_ms must be <= 300000",
                spec.name,
                hook.point
            );
        }
        out.push(HookRef {
            plugin: spec.name.clone(),
            point: hook.point.clone(),
            priority: hook.priority,
            timeout: (hook.timeout_ms > 0).then(|| Duration::from_millis(hook.timeout_ms)),
            on_error: on_error.to_string(),
        });
    }
    Ok(out)
}

fn validate_ui(spec: &PluginSpec, ui: &[PluginUi]) -> Result<Vec<UiRef>> {
    let mut out = Vec::new();
    let mut seen = BTreeSet::new();
    for surface in ui {
        if !valid_ui_surface(&surface.surface) {
            bail!(
                "plugin {} UI surface {:?} is not supported",
                spec.name,
                surface.surface
            );
        }
        if surface.id.trim().is_empty() || surface.id.len() > 128 {
            bail!("plugin {} UI surface id must be 1..=128 bytes", spec.name);
        }
        if !seen.insert(surface.id.clone()) {
            bail!(
                "plugin {} declares UI surface {:?} more than once",
                spec.name,
                surface.id
            );
        }
        out.push(UiRef {
            plugin: spec.name.clone(),
            surface: surface.surface.clone(),
            id: surface.id.clone(),
            title: surface.title.clone(),
        });
    }
    Ok(out)
}

fn validate_rpc(spec: &PluginSpec, rpc: &[PluginRpc]) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for method in rpc {
        validate_rpc_name(&method.name)?;
        if !out.insert(method.name.clone()) {
            bail!(
                "plugin {} declares RPC method {:?} more than once",
                spec.name,
                method.name
            );
        }
    }
    Ok(out)
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

/// Call one hook with its declared timeout. The plugin's own request timeout
/// still applies when the hook does not set one.
async fn call_hook(
    backend: &Arc<dyn ExtensionBackend>,
    hook: &HookRef,
    params: Value,
) -> Result<Value> {
    match hook.timeout {
        Some(timeout) => tokio::time::timeout(timeout, backend.request("hooks/run", params))
            .await
            .with_context(|| {
                format!(
                    "plugin {} hook {} timed out after {}ms",
                    hook.plugin,
                    hook.point,
                    timeout.as_millis()
                )
            })?,
        None => backend.request("hooks/run", params).await,
    }
}

fn backend_kind(spec: &PluginSpec) -> String {
    if spec.library.is_some() {
        "dylib".into()
    } else {
        "process".into()
    }
}

/// Stable comparison key for "did this plugin's config change". Serializing
/// is cheap next to spawning a process and avoids deriving `PartialEq` on
/// every nested config type.
fn plugin_fingerprint(plugin: &PluginConfig) -> String {
    serde_json::to_string(plugin).unwrap_or_default()
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

/// Route one plugin-to-plugin call. The target must be running and must have
/// advertised the method; the caller's allowlist is checked by `HostContext`.
async fn route_rpc(
    request: &RpcCallRequest,
    backends: &Arc<Mutex<BTreeMap<String, Arc<dyn ExtensionBackend>>>>,
    methods: &Arc<Mutex<BTreeMap<String, BTreeSet<String>>>>,
) -> std::result::Result<Value, String> {
    let exported = methods
        .lock()
        .await
        .get(&request.target)
        .is_some_and(|methods| methods.contains(&request.method));
    if !exported {
        return Err(format!(
            "plugin {} does not export RPC method {:?}",
            request.target, request.method
        ));
    }
    let backend = backends
        .lock()
        .await
        .get(&request.target)
        .cloned()
        .ok_or_else(|| format!("plugin {} is not running", request.target))?;
    if backend.is_closed() {
        return Err(format!("plugin {} is not running", request.target));
    }
    backend
        .request(
            "rpc.call",
            serde_json::json!({
                "method": request.method,
                "arguments": request.params,
                "caller": request.from,
                "depth": request.depth,
            }),
        )
        .await
        .map_err(|err| redact(&format!("{err:#}")))
}

/// Parse a `turn.stop` hook response. A block may use `block`, `action` or
/// `decision` so plugins can share the `tool.before` vocabulary.
fn turn_stop_reason(value: &Value) -> Option<String> {
    let block = value.get("block").and_then(Value::as_bool) == Some(true)
        || value.get("action").and_then(Value::as_str) == Some("block")
        || value.get("decision").and_then(Value::as_str) == Some("block");
    block.then(|| {
        value
            .get("reason")
            .and_then(Value::as_str)
            .filter(|reason| !reason.trim().is_empty())
            .unwrap_or("a turn.stop hook blocked the turn")
            .to_string()
    })
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
            library: None,
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
            settings_schema: None,
            sandbox: false,
            network_allow: Vec::new(),
            secrets: Vec::new(),
            rpc_allow: Vec::new(),
            has_server: false,
            declarative_commands: Vec::new(),
        }
    }

    #[test]
    fn turn_stop_blocks_on_any_decision_spelling() {
        assert_eq!(
            turn_stop_reason(&json!({"block": true, "reason": "tests failing"})),
            Some("tests failing".to_string())
        );
        assert_eq!(
            turn_stop_reason(&json!({"action": "block"})),
            Some("a turn.stop hook blocked the turn".to_string())
        );
        assert_eq!(
            turn_stop_reason(&json!({"decision": "block", "reason": " "})),
            Some("a turn.stop hook blocked the turn".to_string())
        );
        assert_eq!(turn_stop_reason(&json!({"block": false})), None);
        assert_eq!(turn_stop_reason(&json!({"action": "allow"})), None);
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
                tokio::sync::broadcast::channel(8).0,
            );
            manager.tools.lock().await.extend(refs.clone());
            manager.check_tool_conflicts(&refs).await
        });
        assert!(error.is_err());
    }

    #[test]
    fn hooks_are_validated_and_deduplicated() {
        let hook = PluginHook {
            point: "tool.before".into(),
            priority: 5,
            timeout_ms: 500,
            on_error: "deny".into(),
        };
        let hooks = validate_hooks(&spec("acme"), std::slice::from_ref(&hook)).unwrap();
        assert_eq!(hooks[0].priority, 5);
        assert_eq!(hooks[0].on_error, "deny");

        let mut invalid = hook.clone();
        invalid.point = "tool.unknown".into();
        assert!(validate_hooks(&spec("acme"), &[invalid]).is_err());

        let mut invalid = hook.clone();
        invalid.on_error = "explode".into();
        assert!(validate_hooks(&spec("acme"), &[invalid]).is_err());

        assert!(validate_hooks(&spec("acme"), &[hook.clone(), hook]).is_err());
    }

    #[test]
    fn rpc_methods_are_validated_and_deduplicated() {
        let rpc = PluginRpc {
            name: "issues.search".into(),
            description: String::new(),
        };
        let methods = validate_rpc(&spec("acme"), std::slice::from_ref(&rpc)).unwrap();
        assert!(methods.contains("issues.search"));
        assert!(validate_rpc(&spec("acme"), &[rpc.clone(), rpc]).is_err());
        assert!(validate_rpc(
            &spec("acme"),
            &[PluginRpc {
                name: "../escape".into(),
                description: String::new(),
            }]
        )
        .is_err());
    }

    #[test]
    fn action_commands_require_a_server() {
        let command = PluginCommand {
            name: "sync".into(),
            description: String::new(),
            args: String::new(),
            prompt: None,
            action: Some("sync".into()),
        };
        assert!(validate_commands(&spec("acme"), std::slice::from_ref(&command)).is_err());

        let mut server = spec("acme");
        server.has_server = true;
        let refs = validate_commands(&server, std::slice::from_ref(&command)).unwrap();
        assert_eq!(refs[0].action.as_deref(), Some("sync"));

        let both = PluginCommand {
            prompt: Some("prompt".into()),
            ..command.clone()
        };
        assert!(validate_commands(&server, &[both]).is_err());
    }

    #[test]
    fn declared_ui_surfaces_are_listed_with_their_kind() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let manager = PluginManager::new(
                Arc::new(crate::store::Store::in_memory().unwrap()),
                PathBuf::from("/tmp"),
                tokio::sync::broadcast::channel(8).0,
            );
            manager.ui.lock().await.push(UiRef {
                plugin: "acme".into(),
                surface: "picker".into(),
                id: "issues".into(),
                title: "Issues".into(),
            });
            let list = manager.ui_surfaces().await;
            assert_eq!(list.surfaces.len(), 1);
            assert_eq!(list.surfaces[0].plugin, "acme");
            assert_eq!(list.surfaces[0].surface, "picker");
            assert_eq!(list.surfaces[0].id, "issues");
            assert_eq!(list.surfaces[0].title, "Issues");
        });
    }

    #[test]
    fn repeated_failures_mark_a_plugin_degraded_and_recover() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let manager = PluginManager::new(
                Arc::new(crate::store::Store::in_memory().unwrap()),
                PathBuf::from("/tmp"),
                tokio::sync::broadcast::channel(8).0,
            );
            manager.statuses.lock().await.insert(
                "acme".to_string(),
                PluginStatus {
                    name: "acme".into(),
                    version: String::new(),
                    description: String::new(),
                    enabled: true,
                    command: String::new(),
                    state: PluginState::Ready,
                    tools: 0,
                    commands: 0,
                    hooks: 0,
                    ui: 0,
                    rpc: 0,
                    events: Vec::new(),
                    capabilities: Vec::new(),
                    backend: "process".into(),
                    failures: 0,
                    error: None,
                    settings_schema: None,
                },
            );

            manager.record_result("acme", false, Some("timeout")).await;
            manager.record_result("acme", false, Some("timeout")).await;
            let status = manager.statuses.lock().await["acme"].clone();
            assert_eq!(status.state, PluginState::Ready);
            assert_eq!(status.failures, 2);

            manager.record_result("acme", false, Some("timeout")).await;
            let status = manager.statuses.lock().await["acme"].clone();
            assert_eq!(status.state, PluginState::Degraded);
            assert_eq!(status.failures, 3);
            assert_eq!(status.error.as_deref(), Some("timeout"));
            assert_eq!(manager.list().await.plugins[0].failures, 3);

            manager.record_result("acme", true, None).await;
            let status = manager.statuses.lock().await["acme"].clone();
            assert_eq!(status.state, PluginState::Ready);
            assert_eq!(status.failures, 0);
            assert!(status.error.is_none());
        });
    }

    #[test]
    fn doctor_surfaces_recent_errors_and_log_tail() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let manager = PluginManager::new(
                Arc::new(crate::store::Store::in_memory().unwrap()),
                PathBuf::from("/tmp"),
                tokio::sync::broadcast::channel(8).0,
            );
            manager.statuses.lock().await.insert(
                "acme".to_string(),
                PluginStatus {
                    name: "acme".into(),
                    version: "1.0.0".into(),
                    description: String::new(),
                    enabled: true,
                    command: "python3".into(),
                    state: PluginState::Ready,
                    tools: 1,
                    commands: 0,
                    hooks: 0,
                    ui: 0,
                    rpc: 0,
                    events: Vec::new(),
                    capabilities: Vec::new(),
                    backend: "process".into(),
                    failures: 0,
                    error: None,
                    settings_schema: None,
                },
            );
            manager.logs.push("acme", "error", "boom");
            manager.logs.push("acme", "info", "still here");

            let doctor = manager.doctor().await;
            assert_eq!(doctor.plugins.len(), 1);
            let entry = &doctor.plugins[0];
            assert_eq!(entry.backend, "process");
            assert_eq!(entry.log_lines, 2);
            assert_eq!(entry.last_log.as_deref(), Some("still here"));
            let logs = entry
                .checks
                .iter()
                .find(|check| check.name == "logs")
                .unwrap();
            assert!(!logs.ok);
            assert!(logs.detail.contains("1 error"));

            let page = manager.logs(&eix_core::plugin::PluginLogsParams {
                name: Some("acme".into()),
                ..Default::default()
            });
            assert_eq!(page.entries.len(), 2);
            assert_eq!(page.entries[0].message, "boom");
        });
    }

    #[test]
    fn settings_updates_validate_and_persist() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let dir = std::env::temp_dir().join(format!(
                "eix-plugin-settings-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("config.toml");
            std::fs::write(
                &path,
                "[plugins.acme]\ncommand = \"python3\"\n\n[plugins.acme.settings]\nrepo = \"old\"\n",
            )
            .unwrap();
            let config = Config::load_for(Some(&path), &dir).unwrap();

            let manager = PluginManager::new(
                Arc::new(crate::store::Store::in_memory().unwrap()),
                dir.clone(),
                tokio::sync::broadcast::channel(8).0,
            );
            manager.statuses.lock().await.insert(
                "acme".to_string(),
                PluginStatus {
                    name: "acme".into(),
                    version: String::new(),
                    description: String::new(),
                    enabled: true,
                    command: String::new(),
                    state: PluginState::Ready,
                    tools: 0,
                    commands: 0,
                    hooks: 0,
                    ui: 0,
                    rpc: 0,
                    events: Vec::new(),
                    capabilities: Vec::new(),
                    backend: "process".into(),
                    failures: 0,
                    error: None,
                    settings_schema: Some(serde_json::json!({
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["repo"],
                        "properties": {
                            "repo": {"type": "string"},
                            "auto_sync": {"type": "boolean"}
                        }
                    })),
                },
            );

            let result = manager
                .set_settings(
                    "acme",
                    &serde_json::json!({"repo": "acme", "auto_sync": true}),
                    &config,
                )
                .await
                .expect("valid settings");
            assert_eq!(result.values["repo"], "acme");
            assert_eq!(result.values["auto_sync"], true);
            assert_eq!(result.schema.as_ref().unwrap()["type"], "object");

            // Rejected before touching the file.
            assert!(manager
                .set_settings("acme", &serde_json::json!({"auto_sync": "yes"}), &config)
                .await
                .is_err());
            let reloaded = Config::load_for(Some(&path), &dir).unwrap();
            assert_eq!(reloaded.plugins["acme"].settings["auto_sync"], true);

            // `null` removes a key and the table is rewritten in place. The
            // daemon mirrors the write in memory; reload to do the same here.
            let config = reloaded;
            manager
                .set_settings("acme", &serde_json::json!({"auto_sync": null}), &config)
                .await
                .expect("remove setting");
            let reloaded = Config::load_for(Some(&path), &dir).unwrap();
            assert_eq!(reloaded.plugins["acme"].settings["repo"], "acme");
            assert!(!reloaded.plugins["acme"].settings.contains_key("auto_sync"));
            let _ = std::fs::remove_dir_all(&dir);
        });
    }
}
