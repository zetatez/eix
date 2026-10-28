//! Plugin manifest and protocol types.
//!
//! Plugins are out-of-process programs. `eixd` starts one child per enabled
//! plugin, speaks newline-delimited JSON-RPC 2.0 over stdin/stdout, and only
//! exposes host capabilities granted in configuration. The process boundary
//! keeps a crashing plugin away from sessions, approvals and the event log; it
//! is not an OS sandbox, so a plugin still runs with the daemon user's
//! permissions.
//!
//! This module is the wire contract. The daemon host lives in
//! `eixd/src/plugin/`; the design notes live in `docs/plugins.md`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped when the handshake or wire messages become incompatible. The host
/// only starts plugins that advertise the same major version.
pub const PLUGIN_API_VERSION: u32 = 2;

/// Every model-visible plugin tool starts with this prefix. Exposed names are
/// `plugin__<plugin>__<tool>`; the host keeps the real mapping.
pub const PLUGIN_TOOL_PREFIX: &str = "plugin__";

/// Plugin commands are namespaced so a package cannot silently replace a
/// built-in `/command` or a user Markdown command.
pub const PLUGIN_COMMAND_PREFIX: &str = "plugin/";

/// The only JSON-RPC version the plugin protocol accepts.
pub const JSONRPC_VERSION: &str = "2.0";

/// A capability a plugin can request. The daemon grants a subset from config;
/// a contribution is rejected unless its capability was granted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginCapability {
    /// Add model tools.
    Tools,
    /// Add prompt/action commands.
    Commands,
    /// Subscribe to daemon notifications.
    Events,
    /// Use the host-managed global/workspace KV store.
    Storage,
    /// Run in the hook pipeline.
    Hooks,
    /// Expose plugin RPC methods to other plugins.
    Rpc,
    /// Contribute TUI views, pickers and status fragments.
    Ui,
    /// Make outbound network requests through the host.
    Network,
    /// Read workspace files through the host.
    WorkspaceRead,
    /// Write workspace files through the host.
    WorkspaceWrite,
    /// Read host secrets.
    Secrets,
    /// Register background jobs.
    Jobs,
    /// Read session metadata.
    SessionsRead,
}

impl PluginCapability {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::Commands => "commands",
            Self::Events => "events",
            Self::Storage => "storage",
            Self::Hooks => "hooks",
            Self::Rpc => "rpc",
            Self::Ui => "ui",
            Self::Network => "network",
            Self::WorkspaceRead => "workspace_read",
            Self::WorkspaceWrite => "workspace_write",
            Self::Secrets => "secrets",
            Self::Jobs => "jobs",
            Self::SessionsRead => "sessions_read",
        }
    }
}

/// One process entry point in a manifest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginComponent {
    /// Program to execute. Relative paths resolve against the manifest
    /// directory when the host loads a manifest.
    pub command: String,
    pub args: Vec<String>,
    /// Optional script/binary path. Kept separate from `command` so a
    /// manifest can say `command = "python3"` + `entry = "plugin.py"`.
    pub entry: Option<PathBuf>,
}

/// Components shipped by one package. A package can be declarative only, or
/// add a server process, a UI process, or both.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginComponents {
    pub server: Option<PluginComponent>,
    pub ui: Option<PluginComponent>,
}

/// A prompt command declared directly in the manifest. Declarative commands
/// do not need a server process.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ManifestCommand {
    pub name: String,
    pub description: String,
    /// Hint shown next to the command in the menu, e.g. `[query]`.
    pub args: String,
    /// Prompt template expanded by the frontend with `$ARGUMENTS` / `$1..$9`.
    /// Exactly one of `prompt` and `action` must be set.
    pub prompt: String,
    /// Runtime action handled by the plugin server (`commands/run`). The
    /// plugin may return a prompt to submit or plain output to display.
    pub action: Option<String>,
}

/// Static contributions declared in the manifest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginContributions {
    pub commands: Vec<ManifestCommand>,
}

/// `eix-plugin.toml` next to a plugin's entry point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    /// Manifest schema. Only `2` is supported; v1 manifests are rejected.
    #[serde(default = "default_schema")]
    pub schema: u32,
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    /// Major protocol version the package implements.
    #[serde(default = "default_api_version")]
    pub api_version: u32,
    /// Advisory minimum host version. The host records it but does not
    /// enforce it yet.
    #[serde(default)]
    pub min_host_version: Option<String>,
    /// Capabilities the package would like to use. The host still applies the
    /// narrower grant from config.
    #[serde(default)]
    pub capabilities: Vec<PluginCapability>,
    #[serde(default)]
    pub components: PluginComponents,
    #[serde(default)]
    pub contributes: PluginContributions,
    /// Optional JSON Schema for plugin settings, rendered by the TUI.
    #[serde(default)]
    pub settings: Option<PathBuf>,
    /// Optional distribution metadata: publisher, detached signature and the
    /// checksum file. Install/verify is implemented in `plugin_package`.
    #[serde(default)]
    pub package: Option<PluginPackage>,
}

/// Distribution metadata for a plugin directory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginPackage {
    /// Trust-store key name used to verify the detached signature.
    pub publisher: Option<String>,
    /// Detached Ed25519 signature over the checksum file. Defaults to
    /// `eix-plugin.sig`.
    pub signature: Option<PathBuf>,
    /// Checksum file, relative to the manifest. Defaults to
    /// `eix-plugin.sums`.
    pub checksums: Option<PathBuf>,
}

fn default_schema() -> u32 {
    2
}

fn default_api_version() -> u32 {
    PLUGIN_API_VERSION
}

impl Default for PluginManifest {
    fn default() -> Self {
        Self {
            schema: default_schema(),
            name: String::new(),
            version: String::new(),
            description: String::new(),
            api_version: PLUGIN_API_VERSION,
            min_host_version: None,
            capabilities: Vec::new(),
            components: PluginComponents::default(),
            contributes: PluginContributions::default(),
            settings: None,
            package: None,
        }
    }
}

impl PluginManifest {
    /// Read and validate a manifest. Relative paths inside the manifest are
    /// not resolved here; the host resolves them against `path.parent()`.
    pub fn load(path: &Path) -> Result<Self, PluginError> {
        let text = std::fs::read_to_string(path).map_err(|source| PluginError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let manifest: Self = toml::from_str(&text).map_err(|source| PluginError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        manifest
            .validate()
            .map_err(|message| PluginError::Invalid(format!("{}: {message}", path.display())))?;
        Ok(manifest)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != 2 {
            return Err(format!(
                "unsupported manifest schema {}; only schema 2 is supported",
                self.schema
            ));
        }
        if self.name.trim().is_empty() {
            return Err("plugin name is empty".into());
        }
        if !valid_plugin_name(&self.name) {
            return Err(format!(
                "plugin name {:?} must contain only letters, digits, '.', '_' or '-', \
                 without '__', and must not be '.' or '..'",
                self.name
            ));
        }
        if self.api_version != PLUGIN_API_VERSION {
            return Err(format!(
                "plugin API version {} is not supported (host expects {})",
                self.api_version, PLUGIN_API_VERSION
            ));
        }
        if self.components.server.is_none()
            && self.components.ui.is_none()
            && self.contributes.commands.is_empty()
        {
            return Err("manifest needs a server component, a ui component, or a command".into());
        }
        for (label, component) in [
            ("components.server", &self.components.server),
            ("components.ui", &self.components.ui),
        ] {
            if let Some(component) = component {
                if component.command.trim().is_empty() && component.entry.is_none() {
                    return Err(format!("[{label}] needs `command` or `entry`"));
                }
            }
        }
        for command in &self.contributes.commands {
            if !valid_tool_name(&command.name) {
                return Err(format!(
                    "declarative command name {:?} must contain only letters, digits, '.', '_' or '-', without '__'",
                    command.name
                ));
            }
            let has_prompt = !command.prompt.trim().is_empty();
            let has_action = command
                .action
                .as_deref()
                .is_some_and(|action| !action.trim().is_empty());
            if has_prompt == has_action {
                return Err(format!(
                    "declarative command {:?} needs exactly one of `prompt` or `action`",
                    command.name
                ));
            }
            if has_action && self.components.server.is_none() {
                return Err(format!(
                    "declarative command {:?} uses `action` but the manifest has no server component",
                    command.name
                ));
            }
            if let Some(action) = &command.action {
                if has_action && !valid_tool_name(action.trim()) {
                    return Err(format!(
                        "declarative command {:?} action name {:?} must contain only letters, digits, '.', '_' or '-', without '__'",
                        command.name, action
                    ));
                }
            }
        }
        if let Some(settings) = &self.settings {
            if settings.as_os_str().is_empty() {
                return Err("settings schema path is empty".into());
            }
            validate_relative_path(settings, "settings schema")?;
        }
        if let Some(package) = &self.package {
            if let Some(checksums) = &package.checksums {
                validate_relative_path(checksums, "package.checksums")?;
            }
            if let Some(signature) = &package.signature {
                validate_relative_path(signature, "package.signature")?;
            }
            if let Some(publisher) = &package.publisher {
                if publisher.trim().is_empty() || publisher.len() > 128 {
                    return Err("package.publisher must be 1..=128 bytes".into());
                }
            }
        }
        Ok(())
    }

    /// Resolve the server command. `base` is the manifest's directory.
    pub fn server_command(&self, base: &Path) -> Option<(String, Vec<String>)> {
        self.components
            .server
            .as_ref()
            .map(|component| resolve_component(component, base))
    }

    /// Resolve the UI command. `base` is the manifest's directory.
    pub fn ui_command(&self, base: &Path) -> Option<(String, Vec<String>)> {
        self.components
            .ui
            .as_ref()
            .map(|component| resolve_component(component, base))
    }
}

fn resolve_component(component: &PluginComponent, base: &Path) -> (String, Vec<String>) {
    let entry = component.entry.as_ref().map(|entry| {
        if entry.is_absolute() {
            entry.clone()
        } else {
            base.join(entry)
        }
    });
    if component.command.trim().is_empty() {
        // `entry` alone is the program. Do not pass it as its own first
        // argument. Validation rejects a missing entry.
        return (
            entry
                .map(|entry| entry.to_string_lossy().to_string())
                .unwrap_or_default(),
            component.args.clone(),
        );
    }
    let mut args = component.args.clone();
    if let Some(entry) = &entry {
        args.push(entry.to_string_lossy().to_string());
    }
    // A bare command (`python3`) is looked up on PATH. A command containing a
    // path component (`./bin/server`) is relative to the manifest, matching
    // how `entry` is resolved.
    let command_path = Path::new(&component.command);
    let command = if !command_path.is_absolute()
        && command_path
            .parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty())
    {
        base.join(&component.command).to_string_lossy().to_string()
    } else {
        component.command.clone()
    };
    (command, args)
}

/// One configured plugin. Mirrors `[mcp.servers.<name>]` so users have one
/// mental model for child-process extensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginConfig {
    /// Program to execute. When empty and `manifest` is set, the manifest's
    /// server component supplies it.
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Set false to keep the entry but ignore it.
    pub enabled: bool,
    /// Startup and per-call timeout.
    pub timeout_secs: u64,
    /// Working directory for the child. Defaults to the config load cwd.
    pub cwd: Option<PathBuf>,
    /// Optional `eix-plugin.toml`. Relative paths resolve from the config cwd.
    pub manifest: Option<PathBuf>,
    /// Capabilities granted to this plugin. Empty means "the manifest's
    /// requested capabilities". Config can narrow, but never widen, a
    /// manifest's request.
    pub capabilities: Vec<PluginCapability>,
    /// Only expose matching plugin tool names. Empty means every tool.
    pub allowed_tools: Vec<String>,
    /// Hide matching plugin tool names. Deny wins.
    pub denied_tools: Vec<String>,
    /// Free-form settings passed to `initialize` and exposed to the plugin.
    pub settings: BTreeMap<String, Value>,
    /// Host names or wildcard patterns (`*.example.com`) the plugin may reach
    /// through `host.net.request`. Empty denies every request.
    pub network_allow: Vec<String>,
    /// Environment variable names the plugin may read through
    /// `host.secrets.get`. Empty denies every secret.
    pub secrets: Vec<String>,
    /// Other plugins this plugin may call through `host.rpc.call`. Empty denies
    /// every call; `"*"` allows every RPC-capable plugin.
    pub rpc_allow: Vec<String>,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            enabled: true,
            timeout_secs: 30,
            cwd: None,
            manifest: None,
            capabilities: Vec::new(),
            allowed_tools: Vec::new(),
            denied_tools: Vec::new(),
            settings: BTreeMap::new(),
            network_allow: Vec::new(),
            secrets: Vec::new(),
            rpc_allow: Vec::new(),
        }
    }
}

impl PluginConfig {
    /// Whether one tool advertised by this plugin may be exposed to the model.
    pub fn allows_tool(&self, name: &str) -> bool {
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

    /// Whether `host.net.request` may reach `host`. Checks are case
    /// insensitive and support a leading `*.` wildcard only.
    pub fn allows_host(&self, host: &str) -> bool {
        let host = host.trim().trim_end_matches('.').to_ascii_lowercase();
        !host.is_empty()
            && self.network_allow.iter().any(|pattern| {
                let pattern = pattern.trim().to_ascii_lowercase();
                if let Some(suffix) = pattern.strip_prefix("*.") {
                    host.len() > suffix.len()
                        && host.ends_with(suffix)
                        && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
                } else {
                    pattern == host
                }
            })
    }

    /// Whether the plugin may read this environment variable.
    pub fn allows_secret(&self, name: &str) -> bool {
        self.secrets.iter().any(|allowed| allowed == name)
    }

    /// Whether this plugin may call RPC methods exported by `target`.
    pub fn allows_rpc_target(&self, target: &str) -> bool {
        self.rpc_allow
            .iter()
            .any(|allowed| allowed == "*" || (allowed != "*" && wildcard(allowed, target)))
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

/// A JSON-RPC 2.0 request. The same shape travels in both directions; the
/// host dispatches incoming requests whose method starts with `host.`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    #[serde(default = "jsonrpc_version")]
    pub jsonrpc: String,
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl RpcRequest {
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            method: method.into(),
            params,
        }
    }
}

/// A JSON-RPC 2.0 response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcResponse {
    #[serde(default = "jsonrpc_version")]
    pub jsonrpc: String,
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl RpcResponse {
    pub fn ok<T: Serialize>(id: u64, result: T) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: Some(serde_json::to_value(result).unwrap_or(Value::Null)),
            error: None,
        }
    }

    pub fn err(id: u64, code: i64, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
                data: None,
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// A one-way JSON-RPC notification. Host event delivery, progress and
/// cancellation use this shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcNotification {
    #[serde(default = "jsonrpc_version")]
    pub jsonrpc: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl RpcNotification {
    pub fn new<T: Serialize>(method: impl Into<String>, params: T) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            method: method.into(),
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        }
    }
}

fn jsonrpc_version() -> String {
    JSONRPC_VERSION.to_string()
}

/// Error codes used by the host. Plugin-defined errors may use other values.
pub mod error_code {
    pub const INVALID_PARAMS: i64 = -32602;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INTERNAL: i64 = -32603;
    pub const UNAUTHORIZED: i64 = -32001;
    pub const UNSUPPORTED: i64 = -32002;
    pub const TIMEOUT: i64 = -32003;
    pub const CANCELLED: i64 = -32004;
    pub const PLUGIN_FAILED: i64 = -32005;
}

/// Handshake parameters sent to a server plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInitializeParams {
    pub protocol_version: u32,
    pub api_version: u32,
    pub plugin: PluginIdentity,
    pub host: PluginIdentity,
    /// Host features the plugin may use, e.g. `content_blocks`, `cancel`.
    pub features: Vec<String>,
    pub capabilities: Vec<PluginCapability>,
    #[serde(default)]
    pub workspace: Option<PathBuf>,
    /// Host-provided config: data directory, settings, and future scoped
    /// grants. Treat it as read-only.
    #[serde(default)]
    pub config: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginIdentity {
    pub name: String,
    pub version: String,
}

/// What a plugin advertises after `initialize`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginInitializeResult {
    pub name: String,
    pub version: String,
    /// Must match [`PLUGIN_API_VERSION`].
    pub api_version: u32,
    /// Feature subset the plugin understands.
    pub features: Vec<String>,
    pub tools: Vec<PluginTool>,
    pub commands: Vec<PluginCommand>,
    /// Daemon notification methods, e.g. `session.*`, `tool.completed`.
    pub events: Vec<String>,
    /// Hook points the plugin wants to run. Requires the `hooks` capability.
    pub hooks: Vec<PluginHook>,
    /// Static UI surfaces. Requires the `ui` capability.
    pub ui: Vec<PluginUi>,
    /// RPC methods other plugins may call. Requires the `rpc` capability.
    pub rpc: Vec<PluginRpc>,
}

/// Hook points understood by the host. Unknown points are rejected so a typo
/// cannot silently disable a security-relevant hook.
pub const PLUGIN_HOOK_POINTS: &[&str] = &["tool.before", "tool.after", "prompt.build", "turn.end"];

/// UI surfaces the host knows how to render. `panel` and `status` are text
/// surfaces; `picker` is reserved for a later milestone.
pub const PLUGIN_UI_SURFACES: &[&str] = &["panel", "status", "picker"];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginHook {
    /// One of [`PLUGIN_HOOK_POINTS`].
    pub point: String,
    /// Lower runs first. Ties break on plugin name.
    pub priority: i32,
    /// Per-hook timeout. `0` uses the plugin's configured timeout.
    pub timeout_ms: u64,
    /// `allow` (keep going), `deny` (reject the tool), or `ignore` (keep the
    /// original value and continue).
    pub on_error: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginUi {
    /// One of [`PLUGIN_UI_SURFACES`].
    pub surface: String,
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginRpc {
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// One model tool from a plugin. Names are plugin-local; the host exposes them
/// as `plugin__<plugin>__<name>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginTool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "empty_schema")]
    pub input_schema: Value,
    /// Advisory only. The host still treats every plugin call as high risk and
    /// requires approval unless the user explicitly trusts read-only tools.
    #[serde(default)]
    pub read_only: bool,
}

fn empty_schema() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

/// One prompt command advertised at runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub args: String,
    #[serde(default)]
    pub prompt: Option<String>,
    /// Runtime action name passed back to `commands/run`. Mutually exclusive
    /// with `prompt`.
    #[serde(default)]
    pub action: Option<String>,
}

/// `plugin.commands` result row, with the host-side namespace applied.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCommandInfo {
    pub plugin: String,
    /// Exposed name, e.g. `plugin/acme/review`.
    pub name: String,
    #[serde(default)]
    pub args: String,
    pub description: String,
    pub prompt: String,
    /// True when the frontend must call `plugin.command.run` instead of
    /// submitting `prompt` directly.
    #[serde(default)]
    pub action: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginCommandList {
    pub commands: Vec<PluginCommandInfo>,
}

/// `plugin.command.run` parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCommandRunParams {
    /// Exposed command name, e.g. `plugin/acme/review`.
    pub name: String,
    #[serde(default)]
    pub arguments: String,
    pub session_id: String,
}

/// `plugin.command.run` result. `prompt` is submitted as a user message;
/// otherwise `output` is shown to the user.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginCommandRunResult {
    #[serde(default = "default_true")]
    pub ok: bool,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
}

/// `plugin.settings` result. `values` mirrors `[plugins.<name>.settings]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginSettingsResult {
    pub plugin: String,
    #[serde(default)]
    pub schema: Option<Value>,
    #[serde(default)]
    pub values: Value,
}

/// Lifecycle state exposed by `plugin.list`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginState {
    Disabled,
    Starting,
    Ready,
    Degraded,
    Failed,
    Stopped,
}

/// `plugin.list` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    pub enabled: bool,
    #[serde(default)]
    pub command: String,
    pub state: PluginState,
    #[serde(default)]
    pub tools: usize,
    #[serde(default)]
    pub commands: usize,
    #[serde(default)]
    pub hooks: usize,
    #[serde(default)]
    pub ui: usize,
    #[serde(default)]
    pub rpc: usize,
    #[serde(default)]
    pub events: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<PluginCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginListResult {
    pub plugins: Vec<PluginInfo>,
}

/// `tools/call` parameters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginToolCallParams {
    /// Plugin-local tool name.
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub workspace: Option<PathBuf>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

/// Rich tool output. Large binary payloads should be stored through
/// `host.blob.put` and referenced by handle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text {
        text: String,
    },
    Image {
        mime: String,
        blob: String,
    },
    Resource {
        uri: String,
        #[serde(default)]
        title: Option<String>,
        #[serde(default)]
        mime: Option<String>,
    },
    Diff {
        path: String,
        patch: String,
    },
    Json {
        value: Value,
    },
}

/// `tools/call` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginToolCallResult {
    #[serde(default = "default_true")]
    pub ok: bool,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured: Option<Value>,
    #[serde(default)]
    pub is_error: bool,
}

impl PluginToolCallResult {
    /// Flatten the result for the current text-only transcript. Non-text
    /// blocks become a short placeholder until the TUI renders them.
    pub fn to_text(&self) -> String {
        let mut out = Vec::new();
        for block in &self.content {
            match block {
                ContentBlock::Text { text } => out.push(text.clone()),
                ContentBlock::Image { mime, .. } => out.push(format!("[image {mime}]")),
                ContentBlock::Resource { uri, title, .. } => out.push(match title {
                    Some(title) => format!("[resource {title} <{uri}>]"),
                    None => format!("[resource <{uri}>]"),
                }),
                ContentBlock::Diff { path, .. } => out.push(format!("[diff {path}]")),
                ContentBlock::Json { value } => {
                    out.push(serde_json::to_string(value).unwrap_or_default())
                }
            }
        }
        if let Some(structured) = &self.structured {
            if out.is_empty() {
                out.push(serde_json::to_string(structured).unwrap_or_default());
            }
        }
        out.join("\n")
    }
}

fn default_true() -> bool {
    true
}

/// Storage scope accepted by `host.storage.*`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PluginStorageScope {
    Global,
    Workspace,
}

impl PluginStorageScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Workspace => "workspace",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("cannot read plugin file {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid plugin manifest {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

/// Tool/plugin names are used in JSON-RPC and exposed tool names. Keep the
/// grammar strict so a name cannot be confused with the `__` separator.
pub fn valid_plugin_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains("__")
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

pub fn valid_tool_name(name: &str) -> bool {
    valid_plugin_name(name) && !name.contains("__")
}

/// Hook points are a closed set; a typo would otherwise become a silent no-op.
pub fn valid_hook_point(point: &str) -> bool {
    PLUGIN_HOOK_POINTS.contains(&point)
}

pub fn valid_ui_surface(surface: &str) -> bool {
    PLUGIN_UI_SURFACES.contains(&surface)
}

fn validate_relative_path(path: &Path, label: &str) -> Result<(), String> {
    if path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(format!("{label} must be a relative path without `..`"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_v2_parses_and_resolves_components() {
        let manifest: PluginManifest = toml::from_str(
            r#"
            schema = 2
            name = "acme"
            version = "0.1.0"
            api_version = 2
            capabilities = ["tools", "commands"]

            [components.server]
            command = "python3"
            entry = "plugin.py"

            [[contributes.commands]]
            name = "review"
            description = "review a file"
            prompt = "Review $ARGUMENTS"
            "#,
        )
        .unwrap();
        manifest.validate().unwrap();
        assert_eq!(manifest.name, "acme");
        let (command, args) = manifest.server_command(Path::new("/tmp/acme")).unwrap();
        assert_eq!(command, "python3");
        assert_eq!(args, vec!["/tmp/acme/plugin.py"]);
        assert_eq!(manifest.contributes.commands[0].name, "review");
    }

    #[test]
    fn declarative_only_manifest_is_valid() {
        let manifest: PluginManifest = toml::from_str(
            r#"
            schema = 2
            name = "prompts"
            api_version = 2

            [[contributes.commands]]
            name = "triage"
            prompt = "Triage the issue"
            "#,
        )
        .unwrap();
        manifest.validate().unwrap();
    }

    #[test]
    fn v1_manifest_is_rejected() {
        let manifest: PluginManifest = toml::from_str(
            r#"
            name = "old"
            api_version = 1
            [server]
            command = "true"
            "#,
        )
        .unwrap();
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn entry_only_component_does_not_repeat_the_entry_as_an_arg() {
        let manifest = PluginManifest {
            name: "entry".into(),
            components: PluginComponents {
                server: Some(PluginComponent {
                    command: String::new(),
                    args: vec!["--flag".into()],
                    entry: Some(PathBuf::from("plugin.py")),
                }),
                ui: None,
            },
            ..Default::default()
        };
        let (command, args) = manifest.server_command(Path::new("/tmp/entry")).unwrap();
        assert_eq!(command, "/tmp/entry/plugin.py");
        assert_eq!(args, vec!["--flag"]);
    }

    #[test]
    fn plugin_names_cannot_be_path_segments() {
        assert!(valid_plugin_name("acme"));
        assert!(valid_plugin_name("com.acme"));
        assert!(!valid_plugin_name("."));
        assert!(!valid_plugin_name(".."));
        assert!(!valid_plugin_name("../escape"));
        assert!(!valid_plugin_name("a/b"));
        assert!(!valid_plugin_name("a__b"));
    }

    #[test]
    fn relative_server_command_resolves_against_the_manifest_dir() {
        let manifest = PluginManifest {
            name: "relative".into(),
            components: PluginComponents {
                server: Some(PluginComponent {
                    command: "./bin/plugin".into(),
                    args: Vec::new(),
                    entry: None,
                }),
                ui: None,
            },
            ..Default::default()
        };
        let (command, args) = manifest.server_command(Path::new("/tmp/relative")).unwrap();
        assert_eq!(
            PathBuf::from(command),
            Path::new("/tmp/relative").join("./bin/plugin")
        );
        assert!(args.is_empty());
    }

    #[test]
    fn manifest_rejects_wrong_api_version() {
        let manifest = PluginManifest {
            name: "old".into(),
            api_version: 1,
            components: PluginComponents {
                server: Some(PluginComponent {
                    command: "true".into(),
                    ..Default::default()
                }),
                ui: None,
            },
            ..Default::default()
        };
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn tool_filter_is_deny_first() {
        let config = PluginConfig {
            allowed_tools: vec!["issue_*".into()],
            denied_tools: vec!["issue_delete".into()],
            ..Default::default()
        };
        assert!(config.allows_tool("issue_search"));
        assert!(!config.allows_tool("issue_delete"));
        assert!(!config.allows_tool("deploy"));
    }

    #[test]
    fn protocol_response_round_trips() {
        let line =
            serde_json::to_string(&RpcResponse::ok(1, serde_json::json!({"ok": true}))).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["jsonrpc"], JSONRPC_VERSION);
        assert_eq!(value["id"], 1);
        assert_eq!(value["result"]["ok"], true);
        assert!(value.get("error").is_none());
    }

    #[test]
    fn tool_result_flattens_rich_blocks() {
        let result = PluginToolCallResult {
            ok: true,
            content: vec![
                ContentBlock::Text {
                    text: "hello".into(),
                },
                ContentBlock::Diff {
                    path: "a.rs".into(),
                    patch: "@@".into(),
                },
            ],
            structured: None,
            is_error: false,
        };
        assert_eq!(result.to_text(), "hello\n[diff a.rs]");
    }

    #[test]
    fn action_commands_need_a_server_and_exactly_one_body() {
        let mut manifest: PluginManifest = toml::from_str(
            r#"
            schema = 2
            name = "acme"
            api_version = 2

            [[contributes.commands]]
            name = "sync"
            action = "sync"
            "#,
        )
        .unwrap();
        assert!(manifest.validate().is_err());

        manifest.components.server = Some(PluginComponent {
            command: "python3".into(),
            ..Default::default()
        });
        manifest.validate().unwrap();

        manifest.contributes.commands[0].prompt = "also a prompt".into();
        assert!(manifest.validate().is_err());
    }

    #[test]
    fn hook_and_ui_names_are_closed_sets() {
        assert!(valid_hook_point("tool.before"));
        assert!(valid_hook_point("turn.end"));
        assert!(!valid_hook_point("tool.exec"));
        assert!(valid_ui_surface("panel"));
        assert!(!valid_ui_surface("pane"));
    }

    #[test]
    fn allowlists_default_to_deny_and_scope_rpc_targets() {
        let mut config = PluginConfig::default();
        assert!(!config.allows_host("api.example.com"));
        assert!(!config.allows_secret("TOKEN"));
        assert!(!config.allows_rpc_target("other"));
        config.network_allow = vec!["*.example.com".into()];
        config.secrets = vec!["TOKEN".into()];
        config.rpc_allow = vec!["other*".into()];
        assert!(config.allows_host("api.example.com"));
        assert!(!config.allows_host("example.com"));
        assert!(config.allows_secret("TOKEN"));
        assert!(config.allows_rpc_target("other-plugin"));
        assert!(!config.allows_rpc_target("acme"));
    }

    #[test]
    fn package_paths_must_stay_inside_the_package() {
        let mut manifest = PluginManifest {
            name: "acme".into(),
            components: PluginComponents {
                server: Some(PluginComponent {
                    command: "true".into(),
                    ..Default::default()
                }),
                ui: None,
            },
            ..Default::default()
        };
        manifest.package = Some(PluginPackage {
            publisher: Some("acme".into()),
            signature: Some(PathBuf::from("../escape.sig")),
            checksums: Some(PathBuf::from("eix-plugin.sums")),
        });
        assert!(manifest.validate().is_err());
        manifest.package.as_mut().unwrap().signature = Some(PathBuf::from("eix-plugin.sig"));
        manifest.settings = Some(PathBuf::from("/etc/settings.json"));
        assert!(manifest.validate().is_err());
    }
}
