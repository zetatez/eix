//! Configuration and XDG paths.
//!
//! The config is deliberately small: a single OpenAI-compatible provider plus a
//! few runtime knobs. Environment variables override the file so ad-hoc use
//! (`EIX_MODEL=... eix chat`) does not require editing anything.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
    /// Minimum seconds between notifications for the same session.
    pub cooldown_secs: u64,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            command: None,
            bell: false,
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
        if let Some(value) = env_var("EIX_NOTIFY_COMMAND") {
            self.notifications.command = Some(value);
        }
        if let Some(value) = env_var("EIX_NOTIFY_COOLDOWN_SECS").and_then(|v| v.parse().ok()) {
            self.notifications.cooldown_secs = value;
        }
        if let Some(value) = env_var("EIX_NOTIFY_BELL") {
            self.notifications.bell = matches!(value.as_str(), "1" | "true" | "yes" | "on");
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

        let parsed: Config = toml::from_str(
            "[tools]\nsandbox = \"off\"\nsandbox_network = false\nsandbox_read_home = false\n",
        )
        .expect("tools config");
        assert_eq!(parsed.tools.sandbox, SandboxMode::Off);
        assert!(!parsed.tools.sandbox_network);
        assert!(!parsed.tools.sandbox_read_home);
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
}
