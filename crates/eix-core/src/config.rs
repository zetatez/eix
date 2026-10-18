//! Configuration and XDG paths.
//!
//! The config is deliberately small: a single OpenAI-compatible provider plus a
//! few runtime knobs. Environment variables override the file so ad-hoc use
//! (`EIX_MODEL=... eix chat`) does not require editing anything.

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
}

/// Optional TCP listener for remote attach. Off by default: the Unix socket is
/// protected by filesystem permissions, a TCP port is not.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// Address to listen on, e.g. `127.0.0.1:7420`. Empty disables TCP.
    pub tcp: Option<String>,
    /// Shared secret every TCP client must present (`auth` request). Required
    /// whenever `tcp` is set.
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
        true
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
        }
    }
}

/// How the daemon assembles the provider context.
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
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            budget_tokens: 32_000,
            instructions_files: vec!["AGENTS.md".to_string()],
            workspace_summary: true,
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

impl Config {
    /// Load `path`, or the default config path when `None`, then apply env
    /// overrides. A missing file is not an error: defaults are valid.
    pub fn load(path: Option<&Path>) -> Result<Self, ConfigError> {
        let path = path
            .map(Path::to_path_buf)
            .unwrap_or_else(default_config_path);
        let mut config = if path.exists() {
            let text = fs::read_to_string(&path).map_err(|source| ConfigError::Read {
                path: path.clone(),
                source,
            })?;
            if text.trim().is_empty() {
                Self::default()
            } else {
                toml::from_str(&text).map_err(|source| ConfigError::Parse {
                    path: path.clone(),
                    source,
                })?
            }
        } else {
            Self::default()
        };
        config.apply_env();
        config.validate()?;
        Ok(config)
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
        if let Some(value) = env_var("EIX_TCP") {
            self.server.tcp = Some(value);
        }
        if let Some(value) = env_var("EIX_TCP_TOKEN") {
            self.server.token = Some(value);
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
        if self.context.budget_tokens == 0 {
            return Err(ConfigError::Invalid(
                "context.budget_tokens must be > 0".into(),
            ));
        }
        if let Some(tcp) = self
            .server
            .tcp
            .as_deref()
            .map(str::trim)
            .filter(|tcp| !tcp.is_empty())
        {
            if self
                .server
                .token
                .as_deref()
                .map(str::trim)
                .is_none_or(str::is_empty)
            {
                return Err(ConfigError::Invalid(format!(
                    "server.tcp is {tcp:?} but server.token is empty; TCP requires a token"
                )));
            }
        }
        Ok(())
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
        let base = self.provider.base_url.trim_end_matches('/');
        if base.ends_with("/chat/completions") {
            base.to_string()
        } else {
            format!("{base}/chat/completions")
        }
    }
}

fn env_var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
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
    fn wildcard_rule_matches_any_mutating_tool() {
        let rule = AutoApproveRule {
            tool: "*".to_string(),
            ..AutoApproveRule::default()
        };
        assert!(rule.matches("write_file", &json!({"path": "a"})));
        assert!(rule.matches("run_command", &json!({"command": "ls"})));
    }
}
