//! Plugin manifest and protocol types.
//!
//! Plugins are out-of-process programs. `eixd` starts one child per enabled
//! plugin, speaks newline-delimited JSON over stdin/stdout, and only exposes
//! host capabilities that were granted in configuration. The process boundary
//! keeps a crashing plugin away from sessions, approvals and the event log;
//! it is not an OS sandbox, so a plugin still runs with the daemon user's
//! permissions.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Bumped when the plugin handshake or wire messages become incompatible.
pub const PLUGIN_API_VERSION: u32 = 1;

/// Every model-visible plugin tool starts with this prefix. Exposed names are
/// `plugin__<plugin>__<tool>`; the host keeps the real mapping.
pub const PLUGIN_TOOL_PREFIX: &str = "plugin__";

/// Plugin commands are namespaced so a package cannot silently replace a
/// built-in `/command` or a user Markdown command.
pub const PLUGIN_COMMAND_PREFIX: &str = "plugin/";

/// A capability a plugin can request. The daemon grants a subset from config;
/// a contribution is ignored unless its capability was granted.
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
    /// Run in the hook pipeline (reserved for the next phase).
    Hooks,
    /// Expose plugin RPC methods to other plugins (reserved).
    Rpc,
    /// Contribute TUI widgets/commands (reserved for the TUI host).
    Ui,
    /// Make outbound network requests through the host (reserved).
    Network,
    /// Read workspace files through the host (reserved).
    WorkspaceRead,
    /// Write workspace files through the host (reserved).
    WorkspaceWrite,
    /// Read host secrets (reserved).
    Secrets,
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
        }
    }
}

/// One process entry point in a manifest. A package can expose a server
/// component, a UI component, or both.
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

/// `eix-plugin.toml` next to a plugin's entry point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_api_version")]
    pub api_version: u32,
    /// Capabilities the package would like to use. The host still applies the
    /// narrower grant from config.
    #[serde(default)]
    pub capabilities: Vec<PluginCapability>,
    #[serde(default)]
    pub server: Option<PluginComponent>,
    #[serde(default)]
    pub ui: Option<PluginComponent>,
}

fn default_api_version() -> u32 {
    PLUGIN_API_VERSION
}

impl Default for PluginManifest {
    fn default() -> Self {
        Self {
            name: String::new(),
            version: String::new(),
            description: String::new(),
            api_version: PLUGIN_API_VERSION,
            capabilities: Vec::new(),
            server: None,
            ui: None,
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
        if self.server.is_none() && self.ui.is_none() {
            return Err("manifest needs a [server] or [ui] component".into());
        }
        for (label, component) in [("server", &self.server), ("ui", &self.ui)] {
            if let Some(component) = component {
                if component.command.trim().is_empty() && component.entry.is_none() {
                    return Err(format!("[{label}] needs `command` or `entry`"));
                }
            }
        }
        Ok(())
    }

    /// Resolve the server command. `base` is the manifest's directory.
    pub fn server_command(&self, base: &Path) -> Option<(String, Vec<String>)> {
        let component = self.server.as_ref()?;
        Some(resolve_component(component, base))
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
    // how `entry` is resolved. Without this, it would silently resolve against
    // the daemon's cwd instead.
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

/// One configured server plugin. Mirrors `[mcp.servers.<name>]` so users have
/// one mental model for child-process extensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PluginConfig {
    /// Program to execute. When empty and `manifest` is set, the manifest's
    /// `[server]` component supplies it.
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
    /// requested capabilities", or tools + commands when there is no manifest.
    /// Config can narrow, but never widen, a manifest's request.
    pub capabilities: Vec<PluginCapability>,
    /// Only expose matching plugin tool names. Empty means every tool.
    pub allowed_tools: Vec<String>,
    /// Hide matching plugin tool names. Deny wins.
    pub denied_tools: Vec<String>,
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

/// A request in either direction. Host -> plugin uses this shape; plugin ->
/// host uses the same shape for `host.*` methods.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRequest {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl PluginRequest {
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self {
            id,
            method: method.into(),
            params,
        }
    }
}

/// A successful or failed response to a [`PluginRequest`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginResponse {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<PluginErrorObject>,
}

impl PluginResponse {
    pub fn ok<T: Serialize>(id: u64, result: T) -> Self {
        Self {
            id,
            result: Some(serde_json::to_value(result).unwrap_or(Value::Null)),
            error: None,
        }
    }

    pub fn err(id: u64, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id,
            result: None,
            error: Some(PluginErrorObject {
                code: code.into(),
                message: message.into(),
            }),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginErrorObject {
    pub code: String,
    pub message: String,
}

/// A one-way message. Host event delivery and plugin log forwarding use this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginNotification {
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl PluginNotification {
    pub fn new<T: Serialize>(method: impl Into<String>, params: T) -> Self {
        Self {
            method: method.into(),
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        }
    }
}

/// Handshake parameters sent to a server plugin.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInitializeParams {
    pub protocol_version: u32,
    pub plugin: PluginIdentity,
    pub host: PluginIdentity,
    pub capabilities: Vec<PluginCapability>,
    #[serde(default)]
    pub workspace: Option<PathBuf>,
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
    /// Must match [`PLUGIN_API_VERSION`]; omitted by older test plugins.
    #[serde(default = "default_api_version")]
    pub api_version: u32,
    pub tools: Vec<PluginTool>,
    pub commands: Vec<PluginCommand>,
    /// Daemon notification methods, e.g. `session.*`, `tool.completed`.
    /// Empty means no event delivery.
    pub events: Vec<String>,
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
    /// requires approval.
    #[serde(default)]
    pub read_only: bool,
}

fn empty_schema() -> Value {
    serde_json::json!({"type": "object", "properties": {}})
}

/// One prompt command. `prompt` is a template expanded by the frontend using
/// the same `$ARGUMENTS` / `$1..$9` rules as Markdown commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCommand {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub args: String,
    #[serde(default)]
    pub prompt: Option<String>,
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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PluginCommandList {
    pub commands: Vec<PluginCommandInfo>,
}

/// `plugin.list` row.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub name: String,
    pub enabled: bool,
    #[serde(default)]
    pub command: String,
    /// `starting`, `running`, `failed` or `stopped`.
    pub state: String,
    #[serde(default)]
    pub tools: usize,
    #[serde(default)]
    pub commands: usize,
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
    pub cwd: Option<PathBuf>,
}

/// `tools/call` result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginToolCallResult {
    #[serde(default = "default_true")]
    pub ok: bool,
    #[serde(default)]
    pub output: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_and_resolves_server_entry() {
        let manifest: PluginManifest = toml::from_str(
            r#"
            name = "acme"
            version = "0.1.0"
            api_version = 1
            capabilities = ["tools", "commands"]

            [server]
            command = "python3"
            entry = "plugin.py"
            "#,
        )
        .unwrap();
        manifest.validate().unwrap();
        assert_eq!(manifest.name, "acme");
        assert_eq!(
            manifest.capabilities,
            vec![PluginCapability::Tools, PluginCapability::Commands]
        );
        let (command, args) = manifest.server_command(Path::new("/tmp/acme")).unwrap();
        assert_eq!(command, "python3");
        assert_eq!(args, vec!["/tmp/acme/plugin.py"]);
    }

    #[test]
    fn entry_only_component_does_not_repeat_the_entry_as_an_arg() {
        let manifest = PluginManifest {
            name: "entry".into(),
            server: Some(PluginComponent {
                command: String::new(),
                args: vec!["--flag".into()],
                entry: Some(PathBuf::from("plugin.py")),
            }),
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
            server: Some(PluginComponent {
                command: "./bin/plugin".into(),
                args: Vec::new(),
                entry: None,
            }),
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
            api_version: 0,
            server: Some(PluginComponent {
                command: "true".into(),
                ..Default::default()
            }),
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
            serde_json::to_string(&PluginResponse::ok(1, serde_json::json!({"ok": true}))).unwrap();
        let value: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["id"], 1);
        assert_eq!(value["result"]["ok"], true);
        assert!(value.get("error").is_none());
    }
}
