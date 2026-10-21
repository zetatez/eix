//! Configuration and XDG paths.
//!
//! The config is deliberately small: a single OpenAI-compatible provider plus a
//! few runtime knobs. Environment variables override the file so ad-hoc use
//! (`EIX_MODEL=... eix chat`) does not require editing anything.

use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid config {path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("invalid json config {path}: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub provider: ProviderConfig,
    pub tools: ToolsConfig,
    pub notifications: NotificationsConfig,
    pub context: ContextConfig,
    pub server: ServerConfig,
    /// Additional OpenAI-compatible models. The default `[provider]` entry is
    /// always available and appears first in `/models`.
    pub models: Vec<ModelConfig>,
    /// Model Context Protocol servers (stdio transport).
    pub mcp: McpConfig,
    /// Prompt image limits and blob retention.
    pub images: ImagesConfig,
    /// Language servers used for diagnostics after edits.
    pub lsp: LspConfig,
    /// Optional post-write formatter.
    pub formatter: FormatterConfig,
    /// Optional TUI palette overrides. Empty values keep the built-in colors.
    pub theme: ThemeConfig,
    /// Transcript-mode key overrides (`action = "C-x"`). Empty keeps defaults.
    pub keys: std::collections::BTreeMap<String, String>,
    /// Explicit `--config` path; when set, project files are not merged.
    #[serde(skip)]
    pub explicit_path: Option<PathBuf>,
    /// Directory the project layers were resolved from (reload uses it).
    #[serde(skip)]
    pub load_cwd: PathBuf,
    /// Every candidate file, including ones that did not exist at load time.
    /// The daemon watches their mtimes for hot reload.
    #[serde(skip)]
    pub watch_paths: Vec<PathBuf>,
}

/// Optional TCP listener for remote attach. Off by default: the Unix socket is
/// protected by filesystem permissions, a TCP port is not.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Address to listen on, e.g. `127.0.0.1:7420`. Empty disables TCP.
    pub tcp: Option<String>,
    /// HTTP JSON-RPC / SSE address, e.g. `127.0.0.1:7421`. Empty disables HTTP.
    pub http: Option<String>,
    /// Shared secret every TCP/HTTP client must present. Required whenever
    /// `tcp` or `http` is set.
    pub token: Option<String>,
}

/// External alerts when a session starts needing attention (approval, error).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NotificationsConfig {
    /// Shell command run on attention. Empty disables it. The daemon exposes
    /// `EIX_SESSION_ID`, `EIX_SESSION_NAME`, `EIX_SESSION_CWD`, and
    /// `EIX_STATUS` (`awaiting_approval` / `error`).
    pub command: Option<String>,
    /// Ring the terminal bell in `eix tui` when a background pane needs
    /// attention.
    pub bell: bool,
    /// Send OSC 9/777 desktop notifications and update the terminal title.
    pub osc: bool,
    /// Minimum seconds between notifications for the same session.
    pub cooldown_secs: u64,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            command: None,
            bell: false,
            osc: true,
            cooldown_secs: 10,
        }
    }
}

/// What the daemon does when a mutating tool is requested.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolPolicy {
    /// Ask the user for every mutating tool call (default).
    #[default]
    Ask,
    /// Run mutating tools without asking (dangerous; opt in explicitly).
    Auto,
    /// Read-only: refuse mutating tools without asking.
    ReadOnly,
}

impl ToolPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
            Self::ReadOnly => "read-only",
        }
    }
}

/// How `run_command` is isolated from the host filesystem.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// Use bubblewrap when it is installed, otherwise run directly.
    #[default]
    Auto,
    /// Require bubblewrap; fail the command when it is missing.
    Bwrap,
    /// Run without a sandbox.
    Off,
}

impl SandboxMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Bwrap => "bwrap",
            Self::Off => "off",
        }
    }
}

/// One `[[tools.auto_approve]]` rule. Rules only relax the `ask` policy: the
/// global `read-only` policy and a session's `read-only` level still win, and
/// `full` sessions never consult them.
///
/// All constraints that are present must match (logical AND). A rule with only
/// `tool` matches every call of that tool, including `"*"` for any mutating
/// tool.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AutoApproveRule {
    /// Tool name (`write_file`, `run_command`, `start_command`, ...) or `"*"`.
    pub tool: String,
    /// Workspace-relative glob matched against the `path` argument. `*` stops
    /// at `/`, `**` crosses directories, `?` matches one character.
    pub path: Option<String>,
    /// Prefix of the `command` argument (after leading whitespace).
    pub command_prefix: Option<String>,
    /// Regular expression matched against the whole `command` argument.
    pub command_regex: Option<String>,
    /// Exact JSON arguments signature. Used by session-scoped "Always allow"
    /// rules for tools without a path/command.
    pub signature: Option<String>,
}

impl AutoApproveRule {
    /// True when this rule covers `tool` with the given call arguments.
    pub fn matches(&self, tool: &str, args: &Value) -> bool {
        if self.tool != "*" && self.tool != tool {
            return false;
        }
        if let Some(glob) = self.path.as_deref() {
            let Some(path) = args
                .get("path")
                .and_then(Value::as_str)
                .and_then(normalize_workspace_path)
            else {
                return false;
            };
            if !glob_match(glob, &path) {
                return false;
            }
        }
        if let Some(prefix) = self.command_prefix.as_deref() {
            let Some(command) = args.get("command").and_then(Value::as_str) else {
                return false;
            };
            if !command.trim_start().starts_with(prefix) {
                return false;
            }
        }
        if let Some(pattern) = self.command_regex.as_deref() {
            let Some(command) = args.get("command").and_then(Value::as_str) else {
                return false;
            };
            let Ok(regex) = Regex::new(pattern) else {
                return false;
            };
            if !regex.is_match(command) {
                return false;
            }
        }
        if let Some(signature) = self.signature.as_deref() {
            if serde_json::to_string(args).ok().as_deref() != Some(signature) {
                return false;
            }
        }
        true
    }
}

/// What an ordered `[[permissions]]` rule decides. `ask` falls through to the
/// normal approval card; rules are evaluated top-down and the first match wins.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAction {
    Allow,
    #[default]
    Ask,
    Deny,
}

/// One ordered permission rule. Matching fields work exactly like
/// [`AutoApproveRule`]: every constraint that is present must match.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionRule {
    /// Tool name (`write_file`, `run_command`, `mcp__*`, ...) or `"*"`.
    pub tool: String,
    pub path: Option<String>,
    pub command_prefix: Option<String>,
    pub command_regex: Option<String>,
    #[serde(default)]
    pub action: PermissionAction,
}

impl PermissionRule {
    pub fn matches(&self, tool: &str, args: &Value) -> bool {
        AutoApproveRule {
            tool: self.tool.clone(),
            path: self.path.clone(),
            command_prefix: self.command_prefix.clone(),
            command_regex: self.command_regex.clone(),
            signature: None,
        }
        .matches(tool, args)
    }
}

/// Normalize a tool `path` argument to a workspace-relative path, or `None`
/// when it is absolute or escapes the workspace. This is what keeps a
/// `tests/**` rule from matching `tests/../src/main.rs`.
fn normalize_workspace_path(path: &str) -> Option<String> {
    if Path::new(path).is_absolute() {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    let normalized = path.replace('\\', "/");
    for part in normalized.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// Minimal glob matcher for approval rules: `*` (not crossing `/`), `**`
/// (crossing `/`, including `**/` for zero directories) and `?`.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let mut memo = vec![vec![None; text.len() + 1]; pattern.len() + 1];

    fn walk(
        pattern: &[char],
        text: &[char],
        pi: usize,
        ti: usize,
        memo: &mut [Vec<Option<bool>>],
    ) -> bool {
        if let Some(result) = memo[pi][ti] {
            return result;
        }
        let result = if pi == pattern.len() {
            ti == text.len()
        } else if pattern[pi] == '*' {
            let double = pattern.get(pi + 1) == Some(&'*');
            if double {
                let next = pi + 2;
                // `**/` also matches zero directories.
                if pattern.get(next) == Some(&'/') && walk(pattern, text, next + 1, ti, memo) {
                    true
                } else {
                    (ti..=text.len()).any(|skip| walk(pattern, text, next, skip, memo))
                }
            } else {
                let next = pi + 1;
                let mut end = ti;
                while end < text.len() && text[end] != '/' {
                    end += 1;
                }
                (ti..=end).any(|skip| walk(pattern, text, next, skip, memo))
            }
        } else if ti == text.len() {
            false
        } else if pattern[pi] == '?' {
            text[ti] != '/' && walk(pattern, text, pi + 1, ti + 1, memo)
        } else {
            pattern[pi] == text[ti] && walk(pattern, text, pi + 1, ti + 1, memo)
        };
        memo[pi][ti] = Some(result);
        result
    }

    walk(&pattern, &text, 0, 0, &mut memo)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    pub policy: ToolPolicy,
    /// Tool output is truncated to this many bytes before it enters the log.
    pub max_output_bytes: usize,
    /// Wall-clock limit for `run_command`.
    pub command_timeout_secs: u64,
    /// Filesystem isolation for `run_command`.
    pub sandbox: SandboxMode,
    /// Allow network access inside the sandbox.
    pub sandbox_network: bool,
    /// Mount the home directory read-only so toolchain caches stay usable.
    pub sandbox_read_home: bool,
    /// Copy what a mutating tool can change before it runs.
    pub snapshots: bool,
    /// Rules that skip the approval prompt for matching mutating tool calls.
    pub auto_approve: Vec<AutoApproveRule>,
    /// Ordered allow/ask/deny rules. The first match wins, before
    /// `auto_approve` (which is treated as an appended `allow` rule).
    pub permissions: Vec<PermissionRule>,
    /// Absolute paths (or `~`-prefixed) that file tools may touch outside the
    /// session workspace. A plain path allows its whole subtree.
    pub external_directory: Vec<String>,
}

impl ToolsConfig {
    /// First rule that covers this call, if any. Only meaningful under the
    /// `ask` policy; callers must still enforce `read-only` and sub-agent
    /// restrictions before consulting it.
    pub fn auto_approval(&self, tool: &str, args: &Value) -> Option<&AutoApproveRule> {
        self.auto_approve
            .iter()
            .find(|rule| rule.matches(tool, args))
    }

    /// Ordered permission decision. `[[permissions]]` entries are checked in
    /// order; legacy `[[tools.auto_approve]]` entries then act as `allow`.
    pub fn permission_for(&self, tool: &str, args: &Value) -> PermissionAction {
        for rule in &self.permissions {
            if rule.matches(tool, args) {
                return rule.action;
            }
        }
        if self.auto_approval(tool, args).is_some() {
            return PermissionAction::Allow;
        }
        PermissionAction::Ask
    }

    /// Whether `path` is covered by `tools.external_directory`.
    pub fn external_path_allowed(&self, path: &Path) -> bool {
        let path = path.to_string_lossy().replace('\\', "/");
        self.external_directory.iter().any(|pattern| {
            let pattern = expand_tilde(pattern);
            if pattern.contains(['*', '?', '[']) {
                glob_match(&pattern, &path)
            } else {
                let prefix = pattern.trim_end_matches('/');
                path == prefix || path.starts_with(&format!("{prefix}/"))
            }
        })
    }
}

fn expand_tilde(pattern: &str) -> String {
    if pattern == "~" {
        return env::var("HOME").unwrap_or_else(|_| "~".to_string());
    }
    if let Some(rest) = pattern.strip_prefix("~/") {
        if let Ok(home) = env::var("HOME") {
            return format!("{}/{}", home.trim_end_matches('/'), rest);
        }
    }
    pattern.to_string()
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            policy: ToolPolicy::Ask,
            max_output_bytes: 20_000,
            command_timeout_secs: 60,
            sandbox: SandboxMode::Auto,
            sandbox_network: true,
            sandbox_read_home: true,
            snapshots: true,
            auto_approve: Vec::new(),
            permissions: Vec::new(),
            external_directory: Vec::new(),
        }
    }
}

/// How the daemon assembles the provider context.
/// TUI colors. Values are color names (`cyan`, `darkgray`, `lightblue`),
/// `#rrggbb`, or a 0-255 ANSI index; unknown values are ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// Built-in palette name (`github-dark`, `github-light`, `gruvbox-dark`,
    /// `solarized-dark`, `nord`, `dracula`, …). Explicit fields below override
    /// individual slots of the preset; without a preset the terminal palette is
    /// used (transparent background).
    pub preset: Option<String>,
    pub normal: Option<String>,
    pub comment: Option<String>,
    pub string: Option<String>,
    pub function: Option<String>,
    pub error: Option<String>,
    pub diff_add: Option<String>,
    pub diff_delete: Option<String>,
    pub panel: Option<String>,
}

/// A built-in `[theme] preset` palette. `None` slots mean "use the color the
/// consumer would use without any theme at all".
pub struct ThemePreset {
    pub name: &'static str,
    pub description: &'static str,
    pub background: Option<&'static str>,
    pub normal: Option<&'static str>,
    pub comment: Option<&'static str>,
    pub string: Option<&'static str>,
    pub function: Option<&'static str>,
    pub error: Option<&'static str>,
    pub diff_add: Option<&'static str>,
    pub diff_delete: Option<&'static str>,
    pub panel: Option<&'static str>,
}

impl ThemePreset {
    pub fn slot(&self, name: &str) -> Option<&'static str> {
        match name {
            "background" => self.background,
            "normal" => self.normal,
            "comment" => self.comment,
            "string" => self.string,
            "function" => self.function,
            "error" => self.error,
            "diff_add" => self.diff_add,
            "diff_delete" => self.diff_delete,
            "panel" => self.panel,
            _ => None,
        }
    }
}

/// Every built-in preset, in picker order. Aliases resolve through
/// [`theme_preset`].
pub const THEME_PRESETS: [ThemePreset; 8] = [
    ThemePreset {
        name: "github-dark",
        description: "GitHub Dark / Primer",
        background: Some("#0d1117"),
        normal: Some("#e6edf3"),
        comment: Some("#8b949e"),
        string: Some("#a5d6ff"),
        function: Some("#d2a8ff"),
        error: Some("#ff7b72"),
        diff_add: Some("#3fb950"),
        diff_delete: Some("#f85149"),
        panel: Some("#161b22"),
    },
    ThemePreset {
        name: "github-light",
        description: "GitHub Light / Primer",
        background: Some("#ffffff"),
        normal: Some("#1f2328"),
        comment: Some("#656d76"),
        string: Some("#032f62"),
        function: Some("#8250df"),
        error: Some("#cf222e"),
        diff_add: Some("#1a7f37"),
        diff_delete: Some("#cf222e"),
        panel: Some("#f6f8fa"),
    },
    ThemePreset {
        name: "gruvbox-dark",
        description: "Gruvbox Dark",
        background: Some("#282828"),
        normal: Some("#ebdbb2"),
        comment: Some("#928374"),
        string: Some("#b8bb26"),
        function: Some("#fabd2f"),
        error: Some("#fb4934"),
        diff_add: Some("#98971a"),
        diff_delete: Some("#cc241d"),
        panel: Some("#3c3836"),
    },
    ThemePreset {
        name: "solarized-dark",
        description: "Solarized Dark",
        background: Some("#002b36"),
        normal: Some("#839496"),
        comment: Some("#586e75"),
        string: Some("#2aa198"),
        function: Some("#268bd2"),
        error: Some("#dc322f"),
        diff_add: Some("#859900"),
        diff_delete: Some("#dc322f"),
        panel: Some("#073642"),
    },
    ThemePreset {
        name: "solarized-light",
        description: "Solarized Light",
        background: Some("#fdf6e3"),
        normal: Some("#657b83"),
        comment: Some("#93a1a1"),
        string: Some("#2aa198"),
        function: Some("#268bd2"),
        error: Some("#dc322f"),
        diff_add: Some("#859900"),
        diff_delete: Some("#dc322f"),
        panel: Some("#eee8d5"),
    },
    ThemePreset {
        name: "nord",
        description: "Nord",
        background: Some("#2e3440"),
        normal: Some("#d8dee9"),
        comment: Some("#616e88"),
        string: Some("#a3be8c"),
        function: Some("#88c0d0"),
        error: Some("#bf616a"),
        diff_add: Some("#a3be8c"),
        diff_delete: Some("#bf616a"),
        panel: Some("#3b4252"),
    },
    ThemePreset {
        name: "dracula",
        description: "Dracula",
        background: Some("#282a36"),
        normal: Some("#f8f8f2"),
        comment: Some("#6272a4"),
        string: Some("#f1fa8c"),
        function: Some("#50fa7b"),
        error: Some("#ff5555"),
        diff_add: Some("#50fa7b"),
        diff_delete: Some("#ff5555"),
        panel: Some("#44475a"),
    },
    ThemePreset {
        name: "catppuccin-mocha",
        description: "Catppuccin Mocha",
        background: Some("#1e1e2e"),
        normal: Some("#cdd6f4"),
        comment: Some("#6c7086"),
        string: Some("#a6e3a1"),
        function: Some("#89b4fa"),
        error: Some("#f38ba8"),
        diff_add: Some("#a6e3a1"),
        diff_delete: Some("#f38ba8"),
        panel: Some("#313244"),
    },
];

/// Look up a preset by name, accepting case/`-`/`_` variations plus a few
/// aliases (`github-dark-default`, `mocha`, …).
pub fn theme_preset(name: &str) -> Option<&'static ThemePreset> {
    let normalized = name.trim().to_ascii_lowercase().replace(['_', ' '], "-");
    let canonical = match normalized.as_str() {
        "github-dark-default" | "github" | "primer-dark" => "github-dark",
        "github-light-default" | "primer-light" => "github-light",
        "gruvbox" | "gruvbox-dark-hard" | "gruvbox-dark-medium" => "gruvbox-dark",
        "solarized" | "solarized-dark-high-contrast" => "solarized-dark",
        "solarized-light-high-contrast" => "solarized-light",
        "catppuccin" | "catppuccin-mocha-dark" | "mocha" => "catppuccin-mocha",
        other => other,
    };
    THEME_PRESETS.iter().find(|preset| preset.name == canonical)
}

/// Run a command after a file tool writes real content. `$1` is appended to
/// `command` as the file path. Staged writes are formatted when they are
/// applied, not while they sit in the shadow workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FormatterConfig {
    /// Shell command, e.g. `rustfmt --edition 2021`. `None` disables it.
    pub command: Option<String>,
    /// Only format these extensions (`["rs", "toml"]`); empty means all files.
    pub extensions: Vec<String>,
    pub timeout_secs: u64,
}

impl Default for FormatterConfig {
    fn default() -> Self {
        Self {
            command: None,
            extensions: Vec::new(),
            timeout_secs: 30,
        }
    }
}

impl FormatterConfig {
    pub fn applies_to(&self, path: &Path) -> bool {
        let Some(command) = self.command.as_deref() else {
            return false;
        };
        if command.trim().is_empty() {
            return false;
        }
        if self.extensions.is_empty() {
            return true;
        }
        let Some(extension) = path.extension().and_then(|ext| ext.to_str()) else {
            return false;
        };
        self.extensions.iter().any(|candidate| {
            candidate
                .trim()
                .trim_start_matches('.')
                .eq_ignore_ascii_case(extension)
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    /// Soft ceiling for one request, used to trim old tool output and history.
    pub budget_tokens: usize,
    /// Workspace-relative files folded into the `instructions` sector. A
    /// missing file is skipped. `AGENTS.md` is read from the eix config
    /// directory as well, when present.
    pub instructions_files: Vec<String>,
    /// Include a compact workspace summary (top-level entries, git branch and
    /// status) in the request.
    pub workspace_summary: bool,
    /// Summarize old history automatically before a turn would overflow
    /// `budget_tokens`.
    pub auto_compact: bool,
    /// Fraction of `budget_tokens` that triggers auto-compaction (0, 1].
    pub auto_compact_threshold: f64,
    /// Number of newest user turns to keep verbatim.
    pub auto_compact_keep_turns: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            budget_tokens: 32_000,
            instructions_files: vec!["AGENTS.md".to_string()],
            workspace_summary: true,
            auto_compact: true,
            auto_compact_threshold: 0.8,
            auto_compact_keep_turns: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Base URL of an OpenAI-compatible API, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    pub model: String,
    /// Inline key. Prefer `api_key_env` so keys never live in the repo/config.
    pub api_key: Option<String>,
    pub api_key_env: String,
    /// Overall request timeout. Streaming responses are bounded by this.
    pub timeout_secs: u64,
    /// Optional replacement for the built-in system prompt.
    pub system_prompt: Option<String>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o-mini".to_string(),
            api_key: None,
            api_key_env: "OPENAI_API_KEY".to_string(),
            timeout_secs: 300,
            system_prompt: None,
        }
    }
}

/// One additional model exposed by `/models`. Each entry may point at its own
/// OpenAI-compatible endpoint, so one installation can mix providers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    /// Selection id shown in `/models`, e.g. `openai/gpt-4o`.
    pub id: String,
    /// API model id. Defaults to the part after the first `/` in `id`, which
    /// makes `openai/gpt-4o` work against an OpenAI-compatible endpoint.
    pub model: Option<String>,
    /// Friendly name; defaults to `id`.
    pub name: Option<String>,
    pub base_url: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    /// USD per million input tokens.
    pub input_cost: Option<f64>,
    /// USD per million output tokens.
    pub output_cost: Option<f64>,
    /// Advertised context window, when known.
    pub context_tokens: Option<usize>,
    /// Variants accepted as `id#variant` (for example `high`, `medium`, `low`).
    pub variants: Vec<String>,
}

/// Model Context Protocol servers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// Servers keyed by display name, e.g. `[mcp.servers.files]`.
    pub servers: BTreeMap<String, McpServerConfig>,
}

/// One stdio MCP server. `command` is spawned directly (no shell), so put any
/// arguments in `args`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub command: String,
    pub args: Vec<String>,
    /// Extra environment variables for the child process.
    pub env: BTreeMap<String, String>,
    /// Set false to keep the entry but ignore it.
    pub enabled: bool,
    /// Per-request timeout for `tools/list` and `tools/call`.
    pub timeout_secs: u64,
    /// Only expose matching MCP tool names. Empty means every tool. Supports
    /// `*` and `?` wildcards.
    pub allowed_tools: Vec<String>,
    /// Hide matching MCP tool names. Deny wins over `allowed_tools`.
    pub denied_tools: Vec<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            enabled: true,
            timeout_secs: 30,
            allowed_tools: Vec::new(),
            denied_tools: Vec::new(),
        }
    }
}

impl McpServerConfig {
    /// Whether one tool from this server may be exposed to the model.
    pub fn allows_tool(&self, name: &str) -> bool {
        if self
            .denied_tools
            .iter()
            .any(|pattern| tool_pattern(pattern, name))
        {
            return false;
        }
        self.allowed_tools.is_empty()
            || self
                .allowed_tools
                .iter()
                .any(|pattern| tool_pattern(pattern, name))
    }
}

/// Tool names are not paths, so `*` and `?` match every character (including
/// `/`) rather than stopping at a directory separator.
fn tool_pattern(pattern: &str, text: &str) -> bool {
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

/// Limits for images attached to prompts. Bytes are content-addressed under
/// `<data>/blobs`, so the event log only stores a small reference.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ImagesConfig {
    /// Maximum images in one prompt.
    pub max_prompt_images: usize,
    /// Maximum size of one image.
    pub max_image_bytes: u64,
    /// Delete orphaned blobs older than this many days. Referenced blobs are
    /// never removed by retention. 0 disables age-based cleanup.
    pub retention_days: u64,
    /// Optional cap for the blob directory. When exceeded, the oldest
    /// unreferenced blobs are removed first. 0 means no cap.
    pub max_store_bytes: u64,
}

impl Default for ImagesConfig {
    fn default() -> Self {
        Self {
            max_prompt_images: 4,
            max_image_bytes: 8 * 1024 * 1024,
            retention_days: 30,
            max_store_bytes: 2 * 1024 * 1024 * 1024,
        }
    }
}

/// Language servers keyed by display name, e.g. `[lsp.servers.rust]`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LspConfig {
    pub servers: BTreeMap<String, LspServerConfig>,
}

/// One stdio language server. `command` is spawned directly (no shell).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LspServerConfig {
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// File extensions (without the dot) this server owns, e.g. `["rs"]`.
    pub extensions: Vec<String>,
    /// LSP language id; defaults from the extension.
    pub language_id: Option<String>,
    /// Extra JSON merged into `initializationOptions`.
    pub initialization_options: Option<serde_json::Value>,
    pub enabled: bool,
    /// Timeout for `initialize` and for one diagnostics collection.
    pub timeout_secs: u64,
}

impl Default for LspServerConfig {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: Vec::new(),
            env: BTreeMap::new(),
            extensions: Vec::new(),
            language_id: None,
            initialization_options: None,
            enabled: true,
            timeout_secs: 10,
        }
    }
}

/// A model as shown to frontends. Contains no secrets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: String,
    pub default: bool,
    pub variants: Vec<String>,
    pub input_cost: Option<f64>,
    pub output_cost: Option<f64>,
    pub context_tokens: Option<usize>,
}

/// Everything the provider needs for one request, resolved from the selected
/// model id.
#[derive(Debug, Clone)]
pub struct ModelRoute {
    pub model: String,
    pub endpoint: String,
    pub api_key: Option<String>,
    pub reasoning_effort: Option<String>,
    pub input_cost: Option<f64>,
    pub output_cost: Option<f64>,
}

impl ModelRoute {
    /// Estimated USD cost for one completion, when pricing is configured.
    pub fn cost(&self, prompt_tokens: u64, completion_tokens: u64) -> Option<f64> {
        let input = self.input_cost?;
        let output = self.output_cost?;
        Some(
            (prompt_tokens as f64 / 1_000_000.0) * input
                + (completion_tokens as f64 / 1_000_000.0) * output,
        )
    }
}

impl Config {
    /// Load `path`, or the default config path when `None`, then apply env
    /// overrides. A missing file is not an error: defaults are valid.
    ///
    /// With `None`, project layers from the process cwd are merged on top of
    /// the global file. `Some(path)` stays authoritative: use it for
    /// `--config` / `EIX_CONFIG` and reproducible runs.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::load_for(path, &cwd)
    }

    /// Like [`load`](Self::load), but resolves relative project layers from
    /// `cwd`. Global config first, then ancestor `.eix/config.toml` files
    /// outermost to nearest; nearer files win. Tables merge, arrays replace.
    pub fn load_for(path: Option<&Path>, cwd: &Path) -> Result<Self, ConfigError> {
        let mut explicit_path = None;
        let mut watch_paths = Vec::new();
        let mut merged = toml::Value::Table(toml::map::Map::new());

        // `--config` and EIX_CONFIG are authoritative; do not mix project
        // layers into a deliberately reproducible configuration.
        let explicit = path
            .map(Path::to_path_buf)
            .or_else(|| env_var("EIX_CONFIG").map(PathBuf::from));

        if let Some(path) = explicit {
            explicit_path = Some(path.clone());
            watch_paths.push(path.clone());
            if path.exists() {
                merge_toml(&mut merged, read_layer(&path)?);
            }
        } else {
            let global = default_config_path();
            watch_paths.push(global.clone());
            if global.exists() {
                merge_toml(&mut merged, read_layer(&global)?);
            }
            let mut dirs: Vec<PathBuf> = cwd.ancestors().map(Path::to_path_buf).collect();
            dirs.reverse();
            for dir in dirs {
                let candidate = dir.join(".eix").join("config.toml");
                if candidate.exists() {
                    merge_toml(&mut merged, read_layer(&candidate)?);
                }
                watch_paths.push(candidate);
            }
        }

        let mut config: Self = merged.try_into().map_err(|source| ConfigError::Parse {
            path: explicit_path.clone().unwrap_or_else(default_config_path),
            source,
        })?;
        config.explicit_path = explicit_path;
        config.load_cwd = cwd.to_path_buf();
        config.watch_paths = watch_paths;
        config.apply_env();
        config.validate()?;
        Ok(config)
    }

    /// Re-read the files this config came from. Used by hot reload.
    pub fn reload(&self) -> Result<Self, ConfigError> {
        Self::load_for(self.explicit_path.as_deref(), &self.load_cwd)
    }

    fn apply_env(&mut self) {
        if let Some(value) = env_var("EIX_BASE_URL") {
            self.provider.base_url = value;
        }
        if let Some(value) = env_var("EIX_MODEL") {
            self.provider.model = value;
        }
        if let Some(value) = env_var("EIX_API_KEY") {
            self.provider.api_key = Some(value);
        }
        if let Some(value) = env_var("EIX_API_KEY_ENV") {
            self.provider.api_key_env = value;
        }
        if let Some(value) = env_var("EIX_SYSTEM_PROMPT") {
            self.provider.system_prompt = Some(value);
        }
        if let Some(value) = env_var("EIX_TIMEOUT_SECS").and_then(|v| v.parse().ok()) {
            self.provider.timeout_secs = value;
        }
        if let Some(value) = env_var("EIX_TOOL_POLICY") {
            match value.as_str() {
                "ask" => self.tools.policy = ToolPolicy::Ask,
                "auto" => self.tools.policy = ToolPolicy::Auto,
                "read-only" | "readonly" => self.tools.policy = ToolPolicy::ReadOnly,
                _ => {}
            }
        }
        if let Some(value) = env_var("EIX_TOOL_MAX_OUTPUT").and_then(|v| v.parse().ok()) {
            self.tools.max_output_bytes = value;
        }
        if let Some(value) = env_var("EIX_TOOL_TIMEOUT_SECS").and_then(|v| v.parse().ok()) {
            self.tools.command_timeout_secs = value;
        }
        if let Some(value) = env_var("EIX_TOOL_SANDBOX") {
            match value.as_str() {
                "auto" => self.tools.sandbox = SandboxMode::Auto,
                "bwrap" => self.tools.sandbox = SandboxMode::Bwrap,
                "off" => self.tools.sandbox = SandboxMode::Off,
                _ => {}
            }
        }
        if let Some(value) = env_var("EIX_TOOL_SANDBOX_NETWORK") {
            self.tools.sandbox_network = matches!(value.as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(value) = env_var("EIX_TOOL_SANDBOX_READ_HOME") {
            self.tools.sandbox_read_home = matches!(value.as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(value) = env_var("EIX_TOOL_SNAPSHOTS") {
            self.tools.snapshots = matches!(value.as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(value) = env_var("EIX_NOTIFY_COMMAND") {
            self.notifications.command = Some(value);
        }
        if let Some(value) = env_var("EIX_NOTIFY_COOLDOWN_SECS").and_then(|v| v.parse().ok()) {
            self.notifications.cooldown_secs = value;
        }
        if let Some(value) = env_var("EIX_NOTIFY_BELL") {
            self.notifications.bell = matches!(value.as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(value) = env_var("EIX_CONTEXT_BUDGET").and_then(|v| v.parse().ok()) {
            self.context.budget_tokens = value;
        }
        if let Some(value) = env_var("EIX_CONTEXT_WORKSPACE") {
            self.context.workspace_summary = matches!(value.as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(value) = env_var("EIX_CONTEXT_AUTO_COMPACT") {
            self.context.auto_compact = matches!(value.as_str(), "1" | "true" | "yes" | "on");
        }
        if let Some(value) =
            env_var("EIX_CONTEXT_AUTO_COMPACT_THRESHOLD").and_then(|v| v.parse().ok())
        {
            self.context.auto_compact_threshold = value;
        }
        if let Some(value) = env_var("EIX_TCP") {
            self.server.tcp = Some(value);
        }
        if let Some(value) = env_var("EIX_TCP_TOKEN") {
            self.server.token = Some(value);
        }
        if let Some(value) = env_var("EIX_HTTP") {
            self.server.http = Some(value);
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let provider = &self.provider;
        if provider.base_url.trim().is_empty() {
            return Err(ConfigError::Invalid("provider.base_url is empty".into()));
        }
        if !(provider.base_url.starts_with("http://") || provider.base_url.starts_with("https://"))
        {
            return Err(ConfigError::Invalid(format!(
                "provider.base_url must start with http:// or https:// (got {:?})",
                provider.base_url
            )));
        }
        if provider.model.trim().is_empty() {
            return Err(ConfigError::Invalid("provider.model is empty".into()));
        }
        if provider.timeout_secs == 0 {
            return Err(ConfigError::Invalid(
                "provider.timeout_secs must be > 0".into(),
            ));
        }
        if self.tools.max_output_bytes == 0 {
            return Err(ConfigError::Invalid(
                "tools.max_output_bytes must be > 0".into(),
            ));
        }
        if self.tools.command_timeout_secs == 0 {
            return Err(ConfigError::Invalid(
                "tools.command_timeout_secs must be > 0".into(),
            ));
        }
        if self
            .formatter
            .command
            .as_deref()
            .is_some_and(|command| command.trim().is_empty())
        {
            return Err(ConfigError::Invalid(
                "formatter.command must not be empty".into(),
            ));
        }
        if self.formatter.timeout_secs == 0 {
            return Err(ConfigError::Invalid(
                "formatter.timeout_secs must be > 0".into(),
            ));
        }
        if let Some(preset) = self.theme.preset.as_deref() {
            if theme_preset(preset).is_none() {
                let names: Vec<&str> = THEME_PRESETS.iter().map(|item| item.name).collect();
                return Err(ConfigError::Invalid(format!(
                    "theme.preset {:?} is unknown (available: {})",
                    preset,
                    names.join(", ")
                )));
            }
        }
        for (index, rule) in self.tools.auto_approve.iter().enumerate() {
            if rule.tool.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "tools.auto_approve[{index}].tool is required (use \"*\" to match any tool)"
                )));
            }
            if rule
                .path
                .as_deref()
                .is_some_and(|path| path.trim().is_empty())
            {
                return Err(ConfigError::Invalid(format!(
                    "tools.auto_approve[{index}].path is empty"
                )));
            }
            if rule
                .command_prefix
                .as_deref()
                .is_some_and(|prefix| prefix.is_empty())
            {
                return Err(ConfigError::Invalid(format!(
                    "tools.auto_approve[{index}].command_prefix is empty"
                )));
            }
            if let Some(pattern) = rule.command_regex.as_deref() {
                Regex::new(pattern).map_err(|err| {
                    ConfigError::Invalid(format!(
                        "tools.auto_approve[{index}].command_regex is not a valid regex: {err}"
                    ))
                })?;
            }
        }
        for (index, rule) in self.tools.permissions.iter().enumerate() {
            if rule.tool.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "tools.permissions[{index}].tool is required (use \"*\" to match any tool)"
                )));
            }
            if rule
                .path
                .as_deref()
                .is_some_and(|path| path.trim().is_empty())
            {
                return Err(ConfigError::Invalid(format!(
                    "tools.permissions[{index}].path is empty"
                )));
            }
            if rule
                .command_prefix
                .as_deref()
                .is_some_and(|prefix| prefix.is_empty())
            {
                return Err(ConfigError::Invalid(format!(
                    "tools.permissions[{index}].command_prefix is empty"
                )));
            }
            if let Some(pattern) = rule.command_regex.as_deref() {
                Regex::new(pattern).map_err(|err| {
                    ConfigError::Invalid(format!(
                        "tools.permissions[{index}].command_regex is not a valid regex: {err}"
                    ))
                })?;
            }
        }
        for (index, path) in self.tools.external_directory.iter().enumerate() {
            if path.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "tools.external_directory[{index}] is empty"
                )));
            }
        }
        for (name, server) in &self.lsp.servers {
            if !server.enabled {
                continue;
            }
            if server.command.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "lsp.servers.{name}.command is empty"
                )));
            }
            if server.extensions.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "lsp.servers.{name}.extensions is empty"
                )));
            }
        }
        for (action, key) in &self.keys {
            if action.trim().is_empty() {
                return Err(ConfigError::Invalid("keys contains an empty action".into()));
            }
            if key.trim().is_empty() {
                return Err(ConfigError::Invalid(format!("keys.{action} is empty")));
            }
        }
        for (index, model) in self.models.iter().enumerate() {
            if model.id.trim().is_empty() {
                return Err(ConfigError::Invalid(format!("models[{index}].id is empty")));
            }
            if self
                .models
                .iter()
                .enumerate()
                .any(|(other, item)| other != index && item.id.trim() == model.id.trim())
            {
                return Err(ConfigError::Invalid(format!(
                    "models[{index}].id {:?} is duplicated",
                    model.id
                )));
            }
            let base = model.base_url.trim();
            if !base.is_empty() && !(base.starts_with("http://") || base.starts_with("https://")) {
                return Err(ConfigError::Invalid(format!(
                    "models[{index}].base_url must start with http:// or https:// (got {:?})",
                    model.base_url
                )));
            }
            if model.input_cost.is_some_and(|cost| cost < 0.0)
                || model.output_cost.is_some_and(|cost| cost < 0.0)
            {
                return Err(ConfigError::Invalid(format!(
                    "models[{index}] cost must not be negative"
                )));
            }
        }
        for (name, server) in &self.mcp.servers {
            if !server.enabled {
                continue;
            }
            if server.command.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "mcp.servers.{name}.command is empty"
                )));
            }
            if server.timeout_secs == 0 {
                return Err(ConfigError::Invalid(format!(
                    "mcp.servers.{name}.timeout_secs must be > 0"
                )));
            }
            if server
                .allowed_tools
                .iter()
                .chain(&server.denied_tools)
                .any(|pattern| pattern.trim().is_empty())
            {
                return Err(ConfigError::Invalid(format!(
                    "mcp.servers.{name} has an empty tool pattern"
                )));
            }
        }
        if self.images.max_prompt_images == 0 {
            return Err(ConfigError::Invalid(
                "images.max_prompt_images must be > 0".into(),
            ));
        }
        if self.images.max_image_bytes == 0 {
            return Err(ConfigError::Invalid(
                "images.max_image_bytes must be > 0".into(),
            ));
        }
        if self.context.budget_tokens == 0 {
            return Err(ConfigError::Invalid(
                "context.budget_tokens must be > 0".into(),
            ));
        }
        if !(self.context.auto_compact_threshold > 0.0
            && self.context.auto_compact_threshold <= 1.0)
        {
            return Err(ConfigError::Invalid(
                "context.auto_compact_threshold must be in (0, 1]".into(),
            ));
        }
        if self.context.auto_compact_keep_turns == 0 {
            return Err(ConfigError::Invalid(
                "context.auto_compact_keep_turns must be > 0".into(),
            ));
        }
        let token_missing = self
            .server
            .token
            .as_deref()
            .map(str::trim)
            .is_none_or(str::is_empty);
        if let Some(tcp) = self
            .server
            .tcp
            .as_deref()
            .map(str::trim)
            .filter(|tcp| !tcp.is_empty())
        {
            if token_missing {
                return Err(ConfigError::Invalid(format!(
                    "server.tcp is {tcp:?} but server.token is empty; TCP requires a token"
                )));
            }
        }
        if let Some(http) = self
            .server
            .http
            .as_deref()
            .map(str::trim)
            .filter(|http| !http.is_empty())
        {
            if token_missing {
                return Err(ConfigError::Invalid(format!(
                    "server.http is {http:?} but server.token is empty; HTTP requires a token"
                )));
            }
        }
        Ok(())
    }

    /// Models shown by `/models`: the default provider first, then configured
    /// entries. Duplicate ids keep the first occurrence.
    pub fn models(&self) -> Vec<ModelInfo> {
        let mut out = Vec::new();
        let default_id = self.provider.model.trim().to_string();
        if !default_id.is_empty() {
            out.push(ModelInfo {
                name: default_id.clone(),
                id: default_id.clone(),
                provider: default_id
                    .split_once('/')
                    .map(|(provider, _)| provider.to_string())
                    .unwrap_or_else(|| "default".to_string()),
                default: true,
                variants: Vec::new(),
                input_cost: None,
                output_cost: None,
                context_tokens: None,
            });
        }
        for model in &self.models {
            let id = model.id.trim();
            if id.is_empty() || out.iter().any(|info| info.id == id) {
                continue;
            }
            out.push(ModelInfo {
                id: id.to_string(),
                name: model
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(id)
                    .to_string(),
                provider: id
                    .split_once('/')
                    .map(|(provider, _)| provider.to_string())
                    .unwrap_or_else(|| "custom".to_string()),
                default: false,
                variants: model.variants.clone(),
                input_cost: model.input_cost,
                output_cost: model.output_cost,
                context_tokens: model.context_tokens,
            });
        }
        out
    }

    /// Resolve a selection id (`provider/model`, optionally `#variant`) into
    /// the endpoint, key and API model to use.
    pub fn route(&self, selection: &str) -> ModelRoute {
        let selection = selection.trim();
        let (base_id, variant) = match selection.rsplit_once('#') {
            Some((id, variant)) if !variant.trim().is_empty() => {
                (id.trim(), Some(variant.trim().to_string()))
            }
            _ => (selection, None),
        };
        let configured = self.models.iter().find(|model| model.id.trim() == base_id);

        let (base_url, api_key, default_model, input_cost, output_cost) = match configured {
            Some(model) => (
                if model.base_url.trim().is_empty() {
                    self.provider.base_url.clone()
                } else {
                    model.base_url.clone()
                },
                model
                    .api_key
                    .as_deref()
                    .map(str::trim)
                    .filter(|key| !key.is_empty())
                    .map(str::to_string)
                    .or_else(|| model.api_key_env.as_deref().and_then(env_var))
                    .or_else(|| self.api_key()),
                model
                    .model
                    .as_deref()
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                    .map(str::to_string)
                    .or_else(|| base_id.split_once('/').map(|(_, model)| model.to_string()))
                    .unwrap_or_else(|| base_id.to_string()),
                model.input_cost,
                model.output_cost,
            ),
            None => (
                self.provider.base_url.clone(),
                self.api_key(),
                if base_id.is_empty() {
                    self.provider.model.clone()
                } else {
                    base_id.to_string()
                },
                None,
                None,
            ),
        };

        ModelRoute {
            model: default_model,
            endpoint: chat_endpoint(&base_url),
            api_key,
            // Most OpenAI-compatible servers accept this for reasoning models;
            // it is ignored by the others.
            reasoning_effort: variant,
            input_cost,
            output_cost,
        }
    }

    /// Resolve the API key: inline first, then the named environment variable.
    /// An empty value is treated as absent (local endpoints often need none).
    pub fn api_key(&self) -> Option<String> {
        if let Some(key) = self
            .provider
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
        {
            return Some(key.to_string());
        }
        env_var(&self.provider.api_key_env)
    }

    /// Full chat-completions URL. Accepts either a base (`.../v1`) or an
    /// explicit endpoint (`.../v1/chat/completions`).
    pub fn chat_endpoint(&self) -> String {
        chat_endpoint(&self.provider.base_url)
    }
}

fn chat_endpoint(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    }
}

fn env_var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Read one TOML layer after expanding `{env:NAME}` / `{file:path}`.
fn read_layer(path: &Path) -> Result<toml::Value, ConfigError> {
    let text = fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let text = interpolate(&text, path.parent().unwrap_or(Path::new(".")))?;
    if text.trim().is_empty() {
        return Ok(toml::Value::Table(toml::map::Map::new()));
    }
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.to_path_buf(),
        source,
    })
}

/// Deep-merge TOML values: tables merge key-by-key, everything else (including
/// arrays) is replaced by the higher-priority layer.
fn merge_toml(base: &mut toml::Value, overlay: toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(base), toml::Value::Table(overlay)) => {
            for (key, value) in overlay {
                match base.get_mut(&key) {
                    Some(existing) => merge_toml(existing, value),
                    None => {
                        base.insert(key, value);
                    }
                }
            }
        }
        (base, overlay) => *base = overlay,
    }
}

/// Expand the two config placeholders. Unknown braces (for example a regex
/// quantifier) are copied verbatim.
fn interpolate(text: &str, base_dir: &Path) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let tail = &rest[open..];
        if let Some(after) = tail.strip_prefix("{env:") {
            if let Some(end) = after.find('}') {
                let name = &after[..end];
                let value = env::var(name).map_err(|_| {
                    ConfigError::Invalid(format!("config references unset env var {name:?}"))
                })?;
                out.push_str(&value);
                rest = &after[end + 1..];
                continue;
            }
        } else if let Some(after) = tail.strip_prefix("{file:") {
            if let Some(end) = after.find('}') {
                let rel = after[..end].trim();
                let path = base_dir.join(rel);
                let value = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
                    path: path.clone(),
                    source,
                })?;
                out.push_str(value.trim_end_matches(['\n', '\r']));
                rest = &after[end + 1..];
                continue;
            }
        }
        out.push('{');
        rest = &tail[1..];
    }
    out.push_str(rest);
    Ok(out)
}

pub fn default_config_path() -> PathBuf {
    if let Some(path) = env_var("EIX_CONFIG") {
        return PathBuf::from(path);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("eix")
        .join("config.toml")
}

/// Project-local CLI defaults. This is separate from `config.toml` because it
/// describes how the CLI should *start* a session, not how the daemon behaves.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CliConfig {
    pub model: Option<String>,
    pub agent: Option<String>,
    /// `read-only`, `workspace-write` or `full`.
    pub permission: Option<String>,
}

/// Nearest `.eix/cli.json` or `cli.json` from `cwd` upwards. `EIX_CLI_CONFIG`
/// pins one file instead. Missing files are not an error.
pub fn load_cli_config(cwd: &Path) -> Result<Option<CliConfig>, ConfigError> {
    let candidates: Vec<PathBuf> = match env_var("EIX_CLI_CONFIG") {
        Some(path) => vec![PathBuf::from(path)],
        None => {
            let mut paths = Vec::new();
            for dir in cwd.ancestors() {
                paths.push(dir.join(".eix").join("cli.json"));
                paths.push(dir.join("cli.json"));
            }
            paths
        }
    };
    for path in candidates {
        if !path.exists() {
            continue;
        }
        let text = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
            path: path.clone(),
            source,
        })?;
        let text = interpolate(&text, path.parent().unwrap_or(Path::new(".")))?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        let config: CliConfig =
            serde_json::from_str(&text).map_err(|source| ConfigError::Json {
                path: path.clone(),
                source,
            })?;
        if let Some(model) = config.model.as_deref() {
            if model.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "{}: model must not be empty",
                    path.display()
                )));
            }
        }
        if let Some(permission) = config.permission.as_deref() {
            if !matches!(
                permission,
                "read-only" | "readonly" | "workspace-write" | "workspace_write" | "full"
            ) {
                return Err(ConfigError::Invalid(format!(
                    "{}: permission must be read-only, workspace-write or full",
                    path.display()
                )));
            }
        }
        return Ok(Some(config));
    }
    Ok(None)
}

pub fn data_dir() -> PathBuf {
    if let Some(path) = env_var("EIX_DATA_DIR") {
        return PathBuf::from(path);
    }
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("eix")
}

pub fn db_path() -> PathBuf {
    data_dir().join("eix.sqlite3")
}

pub fn daemon_log_path() -> PathBuf {
    data_dir().join("eixd.log")
}

/// Prefer the per-user runtime dir (tmpfs), which is the right lifetime for a
/// socket. Fall back to a user-specific path in the temp dir.
pub fn socket_path() -> PathBuf {
    if let Some(path) = env_var("EIX_SOCKET") {
        return PathBuf::from(path);
    }
    if let Some(runtime) = env_var("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime).join("eix.sock");
    }
    let user = env_var("USER").unwrap_or_else(|| "eix".to_string());
    env::temp_dir().join(format!("eix-{user}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn endpoints() {
        let mut config = Config::default();
        config.provider.base_url = "http://localhost:11434/v1/".into();
        assert_eq!(
            config.chat_endpoint(),
            "http://localhost:11434/v1/chat/completions"
        );

        config.provider.base_url = "http://localhost:1234/v1/chat/completions".into();
        assert_eq!(
            config.chat_endpoint(),
            "http://localhost:1234/v1/chat/completions"
        );
    }

    #[test]
    fn invalid_base_url_is_rejected() {
        let mut config = Config::default();
        config.provider.base_url = "localhost:11434".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn default_model_is_not_empty() {
        assert!(!Config::default().provider.model.is_empty());
    }

    #[test]
    fn retired_neutral_theme_presets_are_gone() {
        assert!(theme_preset("dark").is_none());
        assert!(theme_preset("light").is_none());
        assert!(theme_preset("github-dark").is_some());
        assert!(Config::default().theme.preset.is_none());
    }

    #[test]
    fn model_catalog_puts_default_first_and_skips_duplicates() {
        let config = Config {
            provider: ProviderConfig {
                model: "local/llama".into(),
                ..ProviderConfig::default()
            },
            models: vec![
                ModelConfig {
                    id: "local/llama".into(),
                    ..ModelConfig::default()
                },
                ModelConfig {
                    id: "openai/gpt-5".into(),
                    name: Some("GPT-5".into()),
                    variants: vec!["high".into(), "low".into()],
                    input_cost: Some(1.25),
                    output_cost: Some(10.0),
                    ..ModelConfig::default()
                },
            ],
            ..Config::default()
        };

        let models = config.models();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "local/llama");
        assert!(models[0].default);
        assert_eq!(models[0].provider, "local");
        assert_eq!(models[1].name, "GPT-5");
        assert_eq!(models[1].provider, "openai");
        assert_eq!(models[1].variants, vec!["high", "low"]);
    }

    #[test]
    fn route_uses_per_model_endpoint_and_strips_provider_prefix() {
        let config = Config {
            provider: ProviderConfig {
                api_key: Some("default-key".into()),
                ..ProviderConfig::default()
            },
            models: vec![ModelConfig {
                id: "other/gpt-5".into(),
                base_url: "http://127.0.0.1:9/v1".into(),
                api_key: Some("other-key".into()),
                input_cost: Some(2.0),
                output_cost: Some(8.0),
                ..ModelConfig::default()
            }],
            ..Config::default()
        };

        let route = config.route("other/gpt-5#high");
        assert_eq!(route.model, "gpt-5");
        assert_eq!(route.endpoint, "http://127.0.0.1:9/v1/chat/completions");
        assert_eq!(route.api_key.as_deref(), Some("other-key"));
        assert_eq!(route.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(route.cost(1_000_000, 500_000), Some(6.0));

        // Unknown ids fall back to the default endpoint and key verbatim.
        let raw = config.route("openrouter/some-model");
        assert_eq!(raw.model, "openrouter/some-model");
        assert_eq!(raw.endpoint, config.chat_endpoint());
        assert_eq!(raw.api_key.as_deref(), Some("default-key"));
    }

    #[test]
    fn ordered_permission_rules_first_match_wins() {
        let parsed: Config = toml::from_str(
            r#"
[[tools.permissions]]
tool = "run_command"
command_prefix = "cargo "
action = "allow"

[[tools.permissions]]
tool = "run_command"
command_regex = "^rm "
action = "deny"

[[tools.permissions]]
tool = "write_file"
path = "src/**"
action = "ask"
"#,
        )
        .expect("parse permissions");
        parsed.validate().expect("valid permissions");

        assert_eq!(
            parsed
                .tools
                .permission_for("run_command", &json!({"command": "cargo test"})),
            PermissionAction::Allow
        );
        assert_eq!(
            parsed
                .tools
                .permission_for("run_command", &json!({"command": "rm -rf /"})),
            PermissionAction::Deny
        );
        assert_eq!(
            parsed
                .tools
                .permission_for("write_file", &json!({"path": "src/lib.rs"})),
            PermissionAction::Ask
        );
        // No rule matches: the default is still a prompt.
        assert_eq!(
            parsed
                .tools
                .permission_for("write_file", &json!({"path": "docs/readme.md"})),
            PermissionAction::Ask
        );
        // Legacy auto_approve entries behave as appended allow rules.
        let mut legacy = Config::default();
        legacy.tools.auto_approve.push(AutoApproveRule {
            tool: "write_file".into(),
            path: Some("docs/**".into()),
            ..AutoApproveRule::default()
        });
        assert_eq!(
            legacy
                .tools
                .permission_for("write_file", &json!({"path": "docs/a.md"})),
            PermissionAction::Allow
        );
    }

    #[test]
    fn external_directory_matches_paths_and_subtrees() {
        let mut config = Config::default();
        config.tools.external_directory = vec![
            "/tmp/eix-ext".into(),
            "/var/cache/eix/**".into(),
            "~/scratch".into(),
        ];
        assert!(config
            .tools
            .external_path_allowed(Path::new("/tmp/eix-ext/a.txt")));
        assert!(config
            .tools
            .external_path_allowed(Path::new("/tmp/eix-ext")));
        assert!(!config
            .tools
            .external_path_allowed(Path::new("/tmp/eix-extras/a.txt")));
        assert!(config
            .tools
            .external_path_allowed(Path::new("/var/cache/eix/deep/file")));
        assert!(!config
            .tools
            .external_path_allowed(Path::new("/var/cache/other/file")));
        if let Ok(home) = env::var("HOME") {
            assert!(config
                .tools
                .external_path_allowed(&PathBuf::from(home).join("scratch/x")));
        }
    }

    #[test]
    fn project_layers_merge_from_outermost_to_nearest() {
        let root = config_temp_dir("layers");
        let nested = root.join("repo").join("crate");
        std::fs::create_dir_all(nested.join(".eix")).unwrap();
        std::fs::create_dir_all(root.join(".eix")).unwrap();
        std::fs::create_dir_all(root.join("repo").join(".eix")).unwrap();
        std::fs::write(
            root.join(".eix/config.toml"),
            "[context]\nbudget_tokens = 1111\n[tools]\nmax_output_bytes = 1111\n",
        )
        .unwrap();
        std::fs::write(
            root.join("repo/.eix/config.toml"),
            "[context]\nbudget_tokens = 2222\n",
        )
        .unwrap();
        std::fs::write(
            nested.join(".eix/config.toml"),
            "[tools]\nmax_output_bytes = 3333\n",
        )
        .unwrap();

        let config = Config::load_for(None, &nested).expect("layered config");
        assert_eq!(config.context.budget_tokens, 2222);
        assert_eq!(config.tools.max_output_bytes, 3333);
        assert!(config
            .watch_paths
            .contains(&nested.join(".eix/config.toml")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn config_placeholders_expand_env_and_file_values() {
        let dir = config_temp_dir("placeholders");
        std::fs::write(dir.join("key.txt"), "secret-from-file\n").unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[provider]\napi_key = \"{file:key.txt}\"\nbase_url = \"{env:EIX_TEST_BASE_URL}\"\n",
        )
        .unwrap();
        // The base URL placeholder points at an existing variable so the test
        // does not mutate process-wide env vars.
        let previous = std::env::var("EIX_TEST_BASE_URL").ok();
        std::env::set_var("EIX_TEST_BASE_URL", "http://127.0.0.1:9/v1");
        let config = Config::load_for(Some(&path), &dir).expect("interpolated config");
        assert_eq!(config.provider.api_key.as_deref(), Some("secret-from-file"));
        assert_eq!(config.provider.base_url, "http://127.0.0.1:9/v1");
        match previous {
            Some(value) => std::env::set_var("EIX_TEST_BASE_URL", value),
            None => std::env::remove_var("EIX_TEST_BASE_URL"),
        }

        // Unknown braces (a regex quantifier) are left alone.
        let text = interpolate("^a{2}$ {file:key.txt}", &dir).unwrap();
        assert_eq!(text, "^a{2}$ secret-from-file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_replaces_arrays_but_merges_tables() {
        let mut base: toml::Value = toml::from_str("[a]\nx = 1\ny = 2\nvals = [1, 2]\n").unwrap();
        let overlay: toml::Value = toml::from_str("[a]\ny = 3\nvals = [9]\n").unwrap();
        merge_toml(&mut base, overlay);
        let a = base.get("a").unwrap();
        assert_eq!(a.get("x").unwrap().as_integer(), Some(1));
        assert_eq!(a.get("y").unwrap().as_integer(), Some(3));
        assert_eq!(a.get("vals").unwrap().as_array().unwrap().len(), 1);
    }

    #[test]
    fn cli_json_uses_the_nearest_file_and_validates_permission() {
        let root = config_temp_dir("cli-json");
        std::fs::create_dir_all(root.join(".eix")).unwrap();
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            root.join(".eix/cli.json"),
            r#"{"model":"outer/model","agent":"plan","permission":"read-only"}"#,
        )
        .unwrap();
        let defaults = load_cli_config(&nested).unwrap().expect("cli config");
        assert_eq!(defaults.model.as_deref(), Some("outer/model"));
        assert_eq!(defaults.agent.as_deref(), Some("plan"));
        assert_eq!(defaults.permission.as_deref(), Some("read-only"));

        std::fs::write(nested.join("cli.json"), r#"{"model":"inner/model"}"#).unwrap();
        let defaults = load_cli_config(&nested).unwrap().expect("inner cli config");
        assert_eq!(defaults.model.as_deref(), Some("inner/model"));
        assert!(defaults.agent.is_none());

        std::fs::write(nested.join("cli.json"), r#"{"permission":"everything"}"#).unwrap();
        assert!(load_cli_config(&nested).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    fn config_temp_dir(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("eix-config-{label}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn invalid_models_are_rejected() {
        let duplicate = Config {
            models: vec![
                ModelConfig {
                    id: "a/b".into(),
                    ..ModelConfig::default()
                },
                ModelConfig {
                    id: "a/b".into(),
                    ..ModelConfig::default()
                },
            ],
            ..Config::default()
        };
        assert!(duplicate.validate().is_err());

        let bad_url = Config {
            models: vec![ModelConfig {
                id: "a/b".into(),
                base_url: "ftp://example.test".into(),
                ..ModelConfig::default()
            }],
            ..Config::default()
        };
        assert!(bad_url.validate().is_err());

        let negative = Config {
            models: vec![ModelConfig {
                id: "a/b".into(),
                input_cost: Some(-1.0),
                ..ModelConfig::default()
            }],
            ..Config::default()
        };
        assert!(negative.validate().is_err());
    }

    #[test]
    fn tools_sandbox_defaults_and_parses() {
        let default = Config::default();
        assert_eq!(default.tools.sandbox, SandboxMode::Auto);
        assert!(default.tools.sandbox_network);
        assert!(default.tools.sandbox_read_home);
        assert!(default.tools.snapshots);

        let parsed: Config = toml::from_str(
            "[tools]\nsandbox = \"off\"\nsandbox_network = false\nsandbox_read_home = false\nsnapshots = false\n",
        )
        .expect("tools config");
        assert_eq!(parsed.tools.sandbox, SandboxMode::Off);
        assert!(!parsed.tools.sandbox_network);
        assert!(!parsed.tools.sandbox_read_home);
        assert!(!parsed.tools.snapshots);
    }

    #[test]
    fn notifications_parse_and_default_to_off() {
        let default = Config::default();
        assert!(default.notifications.command.is_none());
        assert!(!default.notifications.bell);
        assert_eq!(default.notifications.cooldown_secs, 10);

        let parsed: Config = toml::from_str(
            "[notifications]\ncommand = \"notify-send eix\"\nbell = true\ncooldown_secs = 3\n",
        )
        .expect("notifications config");
        assert_eq!(
            parsed.notifications.command.as_deref(),
            Some("notify-send eix")
        );
        assert!(parsed.notifications.bell);
        assert_eq!(parsed.notifications.cooldown_secs, 3);
    }

    #[test]
    fn auto_approve_rules_parse_and_match() {
        let parsed: Config = toml::from_str(
            r#"
            [tools]
            policy = "ask"

            [[tools.auto_approve]]
            tool = "run_command"
            command_prefix = "cargo test"

            [[tools.auto_approve]]
            tool = "write_file"
            path = "tests/**"

            [[tools.auto_approve]]
            tool = "run_command"
            command_regex = "^cargo (check|clippy)\\b"
            "#,
        )
        .expect("auto approve config");
        assert_eq!(parsed.tools.auto_approve.len(), 3);
        parsed.validate().expect("rules are valid");

        let command = json!({"command": "cargo test --workspace"});
        assert!(parsed
            .tools
            .auto_approval("run_command", &command)
            .is_some());
        assert!(parsed
            .tools
            .auto_approval("start_command", &command)
            .is_none());

        let write = json!({"path": "./tests/unit/a.rs", "content": "x"});
        assert!(parsed.tools.auto_approval("write_file", &write).is_some());
        let escape = json!({"path": "tests/../src/main.rs", "content": "x"});
        assert!(parsed.tools.auto_approval("write_file", &escape).is_none());
        let outside = json!({"path": "src/main.rs", "content": "x"});
        assert!(parsed.tools.auto_approval("write_file", &outside).is_none());
        // A missing `path` argument cannot satisfy a path rule.
        assert!(parsed
            .tools
            .auto_approval("write_file", &json!({}))
            .is_none());

        let check = json!({"command": "cargo clippy --workspace"});
        assert!(parsed.tools.auto_approval("run_command", &check).is_some());
        assert!(parsed
            .tools
            .auto_approval("run_command", &json!({"command": "cargo run"}))
            .is_none());
    }

    #[test]
    fn auto_approve_rejects_invalid_rules() {
        let invalid: Config = toml::from_str(
            "[[tools.auto_approve]]\ntool = \"run_command\"\ncommand_regex = \"(\"\n",
        )
        .expect("parse first");
        assert!(invalid.validate().is_err());

        let empty: Config =
            toml::from_str("[[tools.auto_approve]]\ntool = \"\"\n").expect("parse first");
        assert!(empty.validate().is_err());

        let prefix: Config = toml::from_str(
            "[[tools.auto_approve]]\ntool = \"run_command\"\ncommand_prefix = \"\"\n",
        )
        .expect("parse first");
        assert!(prefix.validate().is_err());
    }

    #[test]
    fn parses_lsp_and_key_overrides() {
        let config: Config = toml::from_str(
            r#"
[lsp.servers.rust]
command = "rust-analyzer"
extensions = ["rs"]
initialization_options = { checkOnSave = true }

[keys]
sessions = "C-t"
help = "F2"
"#,
        )
        .expect("parse");
        config.validate().expect("valid");
        let rust = config.lsp.servers.get("rust").expect("server");
        assert_eq!(rust.command, "rust-analyzer");
        assert_eq!(rust.extensions, vec!["rs"]);
        assert_eq!(
            rust.initialization_options.as_ref().unwrap()["checkOnSave"],
            true
        );
        assert_eq!(config.keys.get("sessions").unwrap(), "C-t");
    }

    #[test]
    fn glob_matching_respects_directories() {
        assert!(glob_match("tests/**", "tests/unit/a.rs"));
        assert!(glob_match("tests/**", "tests/a.rs"));
        assert!(!glob_match("tests/*", "tests/unit/a.rs"));
        assert!(glob_match("tests/*.rs", "tests/a.rs"));
        assert!(glob_match("**/*.rs", "a.rs"));
        assert!(glob_match("**/*.rs", "src/a.rs"));
        assert!(glob_match("src/?.rs", "src/a.rs"));
        assert!(!glob_match("src/?.rs", "src/ab.rs"));
        assert!(!glob_match("*", "a/b"));
        assert!(glob_match("**", "a/b"));
        assert!(glob_match("a/**/b", "a/b"));
        assert!(glob_match("a/**/b", "a/x/y/b"));
    }

    #[test]
    fn http_and_tcp_endpoints_require_a_token() {
        let mut config = Config::default();
        config.server.http = Some("127.0.0.1:0".into());
        assert!(config.validate().is_err());
        config.server.token = Some("secret".into());
        config.validate().unwrap();
        config.server.tcp = Some("127.0.0.1:0".into());
        config.server.token = None;
        assert!(config.validate().is_err());
    }

    #[test]
    fn mcp_tool_filters_match_slashes_and_deny_wins() {
        let mut server = McpServerConfig {
            allowed_tools: vec!["read*".into()],
            denied_tools: vec!["read_secret".into()],
            ..McpServerConfig::default()
        };
        assert!(server.allows_tool("read_file"));
        assert!(server.allows_tool("read/file"));
        assert!(!server.allows_tool("write_file"));
        assert!(!server.allows_tool("read_secret"));
        server.allowed_tools.clear();
        assert!(server.allows_tool("write_file"));
        assert!(!server.allows_tool("read_secret"));
    }

    #[test]
    fn wildcard_rule_matches_any_mutating_tool() {
        let rule = AutoApproveRule {
            tool: "*".to_string(),
            ..AutoApproveRule::default()
        };
        assert!(rule.matches("write_file", &json!({"path": "a"})));
        assert!(rule.matches("run_command", &json!({"command": "ls"})));
    }
}
