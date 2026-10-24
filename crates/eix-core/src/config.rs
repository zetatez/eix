//! Configuration and XDG paths.
//!
//! The config keeps a map of OpenAI-compatible providers; exactly one entry is
//! marked `default = true`. A few runtime knobs live on the default provider.
//! Only the provider entry point (`EIX_BASE_URL`, `EIX_MODEL`, `EIX_API_KEY`)
//! and startup/connection settings are read outside the file; every other knob
//! comes from the config file.

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// OpenAI-compatible providers keyed by name. Exactly one provider is
    /// marked `default = true`; its name is used in model selection ids.
    #[serde(default = "default_openai_providers")]
    pub providers: BTreeMap<String, ProviderConfig>,
    /// Request timeout, retry policy, default context window and system prompt.
    /// These are global runtime knobs, not per-provider settings.
    pub provider_runtime: ProviderRuntimeConfig,
    pub tools: ToolsConfig,
    pub notifications: NotificationsConfig,
    pub context: ContextConfig,
    /// Opt-in discovery of skills, commands, and agents written for other
    /// agent harnesses. Native `.eix` sources are always loaded.
    pub compations: CompatConfig,
    pub server: ServerConfig,
    /// Additional OpenAI-compatible models. The default provider's model is
    /// always available and appears first in `/model`.
    pub models: Vec<ModelConfig>,
    /// Model Context Protocol servers (stdio transport).
    pub mcp: McpConfig,
    /// Prompt image limits and blob retention.
    pub images: ImagesConfig,
    /// Language servers used for diagnostics after edits.
    pub lsp: LspConfig,
    /// Optional post-write formatters. Entries are tried in order and the
    /// first matching extension wins.
    pub formatter: FormattersConfig,
    /// Session worktree defaults.
    pub worktree: WorktreeConfig,
    /// Worktree integration queue defaults.
    pub integration: IntegrationConfig,
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
    /// Security relaxations that were refused from project layers.
    #[serde(skip)]
    pub security_warnings: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            providers: default_openai_providers(),
            provider_runtime: ProviderRuntimeConfig::default(),
            tools: ToolsConfig::default(),
            notifications: NotificationsConfig::default(),
            context: ContextConfig::default(),
            compations: CompatConfig::default(),
            server: ServerConfig::default(),
            models: Vec::new(),
            mcp: McpConfig::default(),
            images: ImagesConfig::default(),
            lsp: LspConfig::default(),
            formatter: FormattersConfig::default(),
            worktree: WorktreeConfig::default(),
            integration: IntegrationConfig::default(),
            theme: ThemeConfig::default(),
            keys: std::collections::BTreeMap::new(),
            explicit_path: None,
            load_cwd: PathBuf::new(),
            watch_paths: Vec::new(),
            security_warnings: Vec::new(),
        }
    }
}

fn default_openai_providers() -> BTreeMap<String, ProviderConfig> {
    BTreeMap::from([("openai".to_string(), ProviderConfig::openai())])
}

/// Per-harness compatibility switches. All are off by default so another
/// agent's global skills/commands do not silently enter eix.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CompatConfig {
    /// Claude Code: `~/.claude` and project `.claude` skills/commands/agents.
    pub claude_code: bool,
    /// OpenCode: `~/.config/opencode` and project `.opencode` skills/commands/agents.
    pub opencode: bool,
    /// Codex CLI: `$CODEX_HOME` and project `.codex` skills, plus prompts.
    pub codex_cli: bool,
    /// Gemini CLI: `~/.gemini` and project `.gemini` skills, TOML commands, agents.
    pub gemini_cli: bool,
    /// Pi: `$PI_CODING_AGENT_DIR` / `.pi` skills and prompts, plus `.agents`.
    pub pi: bool,
}

impl CompatConfig {
    /// Every supported compatibility source (useful for tests and imports).
    pub fn all() -> Self {
        Self {
            claude_code: true,
            opencode: true,
            codex_cli: true,
            gemini_cli: true,
            pi: true,
        }
    }
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

/// Worktree policy for new sessions. `auto` creates a worktree lazily on the
/// first prompt, using an English slug generated from the task text.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorktreeConfig {
    /// Create an isolated worktree for every new user/fork session in a git
    /// repository. Explicit `--shared` sessions opt out.
    pub auto: bool,
    /// Local base branch. `auto` uses the branch checked out in the session's
    /// starting directory; detached HEADs fall back to `origin/HEAD`, then
    /// `main` / `master`, then the repository's current branch. An explicit
    /// branch name overrides the session's current branch.
    pub base: String,
}

impl Default for WorktreeConfig {
    fn default() -> Self {
        Self {
            auto: true,
            base: "auto".into(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AutoReady {
    /// Only an explicit `eix ready` starts integration.
    Manual,
    /// After a successful turn and an idle period, mark the session ready.
    #[default]
    Idle,
}

/// Daemon-side integration queue. One session branch is rebased onto the base
/// branch, tested, then fast-forwarded into the base worktree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct IntegrationConfig {
    pub enabled: bool,
    pub auto_ready: AutoReady,
    /// Seconds of no activity before an idle session is marked ready.
    pub idle_secs: u64,
    /// Commit dirty worktree changes when readiness is requested. Enabled by
    /// default so an explicit `ready` does not fail on the agent's edits.
    pub auto_commit: bool,
    /// Test command run in the rebased worktree. `auto` detects Cargo/npm/Go/
    /// Python projects. An empty value is invalid while integration is enabled.
    pub test: String,
    /// Wall-clock limit for one test run.
    pub timeout_secs: u64,
    /// Fetch and `pull --ff-only` the base branch before each rebase.
    pub pull_base: bool,
    /// Push the base branch after a local fast-forward. Off by default.
    pub push: bool,
    /// Maximum backoff between transient retries.
    pub retry_max_secs: u64,
    /// Notify after this many consecutive failures. 0 disables notifications.
    pub notify_after: u32,
    /// Pause automatic retries after this many failures and wait for
    /// `eix integrate continue`. 0 means never pause.
    pub pause_after: u32,
    /// Let the session agent attempt conflict/test repair.
    pub conflict_repair: bool,
    /// Per-repository overrides, keyed by canonical repository root.
    pub repos: BTreeMap<String, RepoIntegrationConfig>,
}

impl Default for IntegrationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_ready: AutoReady::Idle,
            idle_secs: 300,
            auto_commit: true,
            test: "auto".into(),
            timeout_secs: 1800,
            pull_base: true,
            push: false,
            retry_max_secs: 300,
            notify_after: 3,
            pause_after: 0,
            conflict_repair: true,
            repos: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RepoIntegrationConfig {
    pub base: Option<String>,
    pub test: Option<String>,
    pub timeout_secs: Option<u64>,
    pub pull_base: Option<bool>,
}

impl IntegrationConfig {
    fn repo_override(&self, repo: &Path) -> Option<&RepoIntegrationConfig> {
        let key = repo.to_string_lossy();
        self.repos.get(key.as_ref()).or_else(|| {
            repo.file_name()
                .and_then(|n| self.repos.get(n.to_string_lossy().as_ref()))
        })
    }

    /// Base override for a repository, if configured.
    pub fn repo_base(&self, repo: &Path) -> Option<&str> {
        self.repo_override(repo)
            .and_then(|over| over.base.as_deref())
    }

    pub fn repo_test(&self, repo: &Path) -> &str {
        self.repo_override(repo)
            .and_then(|over| over.test.as_deref())
            .unwrap_or(&self.test)
    }

    pub fn repo_timeout(&self, repo: &Path) -> u64 {
        self.repo_override(repo)
            .and_then(|over| over.timeout_secs)
            .unwrap_or(self.timeout_secs)
    }

    pub fn repo_pull_base(&self, repo: &Path) -> bool {
        self.repo_override(repo)
            .and_then(|over| over.pull_base)
            .unwrap_or(self.pull_base)
    }
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

/// The default approval behavior. Rules are explicit intent and override this
/// mode; the mode only decides what happens to *unruled* `ask` calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// Ask for every unruled call whose risk default is `ask` (default).
    #[default]
    Ask,
    /// Run unruled `ask` calls without prompting. Hard denies still win.
    Auto,
}

impl ApprovalMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Auto => "auto",
        }
    }
}

/// How `run_command` / `start_command` are isolated from the host.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    /// Use bubblewrap when it is available, otherwise run directly.
    #[default]
    Auto,
    /// Require bubblewrap; fail rather than run outside the sandbox.
    Required,
    /// Run without a sandbox.
    Off,
}

impl SandboxMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Required => "required",
            Self::Off => "off",
        }
    }
}

/// What a permission rule says about a matching call.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionAction {
    Allow,
    #[default]
    Ask,
    Deny,
    /// Allow only when a sandbox is active; fall back to `ask` otherwise.
    Sandbox,
}

impl PermissionAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Ask => "ask",
            Self::Deny => "deny",
            Self::Sandbox => "sandbox",
        }
    }
}

/// Where a rule came from. `Builtin` is reserved for risk defaults.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSource {
    Builtin,
    #[default]
    User,
    Project,
    Session,
}

impl RuleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Builtin => "builtin",
            Self::User => "user",
            Self::Project => "project",
            Self::Session => "session",
        }
    }
}

/// A permission rule. All present constraints must match.
///
/// `command` is an argv token prefix. A command with multiple shell segments
/// (for example `cargo test && rm -rf x`) only matches when *every* segment
/// starts with the prefix; this prevents raw-string prefix bypasses.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PermissionRule {
    /// Stable identifier used in approval payloads and audit logs.
    pub id: String,
    /// Tool name (`write_file`, `run_command`, `mcp__*`, ...) or `"*"`.
    pub tool: String,
    /// Workspace-relative glob matched against the `path` argument.
    pub path: Option<String>,
    /// Argv token prefix, e.g. `["git", "status"]`.
    pub command: Option<Vec<String>>,
    /// Escape hatch for unusual cases. Prefer `command` for shell rules.
    pub command_regex: Option<String>,
    /// Exact JSON arguments signature. Used by "Always" rules for tools
    /// without a path or command.
    pub signature: Option<String>,
    #[serde(default)]
    pub action: PermissionAction,
    /// Set by the config loader; never trusted from TOML.
    #[serde(skip)]
    pub source: RuleSource,
}

impl PermissionRule {
    /// True when this rule covers `tool` with the given call arguments.
    pub fn matches(&self, tool: &str, args: &Value) -> bool {
        if self.tool != "*" && self.tool != tool {
            return false;
        }
        if let Some(glob) = self.path.as_deref() {
            if tool == "webfetch" {
                let Some(url) = args.get("url").and_then(Value::as_str) else {
                    return false;
                };
                if !glob_match(glob, url) {
                    return false;
                }
            } else {
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
        }
        if let Some(prefix) = self.command.as_deref() {
            let Some(command) = args.get("command").and_then(Value::as_str) else {
                return false;
            };
            if !command_starts_with(command, prefix, self.action == PermissionAction::Deny) {
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

    /// More constraints means a more specific rule wins when several match.
    pub fn specificity(&self) -> usize {
        usize::from(self.tool != "*")
            + usize::from(self.path.is_some())
            + usize::from(self.command.is_some())
            + usize::from(self.command_regex.is_some())
            + usize::from(self.signature.is_some())
    }
}

fn command_starts_with(command: &str, prefix: &[String], any_segment: bool) -> bool {
    let analysis = crate::risk::analyze_command(command);
    if analysis.segments.is_empty() {
        return false;
    }
    if analysis.dynamic && !any_segment {
        return false;
    }
    // A prefix rule must not silently cover output redirections: allowing
    // `echo hi` must not allow `echo hi > /etc/passwd`.
    if !any_segment && !analysis.redirections.is_empty() {
        return false;
    }
    let matches_prefix = |segment: &crate::risk::CommandSegment| {
        prefix.len() <= segment.argv.len()
            && prefix
                .iter()
                .zip(segment.argv.iter())
                .all(|(expected, actual)| expected == actual)
    };
    if any_segment {
        analysis.segments.iter().any(matches_prefix)
    } else {
        analysis.segments.iter().all(matches_prefix)
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

/// Default number of `spawn_agent` children allowed to run concurrently in one
/// tool batch.
pub const DEFAULT_MAX_PARALLEL_SUBAGENTS: usize = 4;

/// Supported concurrency levels for `spawn_agent`. Keeping this a small,
/// explicit set makes the configured default and runtime override predictable.
pub const SUBAGENT_CONCURRENCY_OPTIONS: [usize; 6] = [1, 2, 4, 8, 16, 32];

/// Whether `value` is a supported `spawn_agent` concurrency level.
pub fn subagent_concurrency_supported(value: usize) -> bool {
    SUBAGENT_CONCURRENCY_OPTIONS.contains(&value)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolsConfig {
    /// What to do with unruled `ask` calls.
    pub approval: ApprovalMode,
    /// Apply low/medium writes inside the workspace immediately instead of
    /// staging them for manual approval. Snapshots and auto-patch records are
    /// still kept. Set to false to review every file edit.
    pub auto_apply_workspace_edits: bool,
    /// Tool output is truncated to this many bytes before it enters the log.
    pub max_output_bytes: usize,
    /// Wall-clock limit for `run_command`.
    pub command_timeout_secs: u64,
    /// Hard cap on provider <-> tool round trips for one turn. The last few
    /// rounds include a "summarize now" reminder, and the final round is sent
    /// without tool definitions so the model must produce an answer.
    pub max_tool_rounds: usize,
    /// Default number of `spawn_agent` children that may run concurrently in
    /// one tool batch. The TUI can override this at runtime; config reload or
    /// a daemon restart restores this default.
    pub max_parallel_subagents: usize,
    /// Filesystem isolation for `run_command` / `start_command`.
    pub sandbox: SandboxMode,
    /// Allow network access inside the sandbox.
    pub sandbox_network: bool,
    /// Paths mounted read-only in the sandbox. `~` is expanded. The whole
    /// home directory is not mounted by default; list toolchain caches here.
    pub sandbox_read_paths: Vec<String>,
    /// Extra paths mounted writable in the sandbox. Relative paths resolve
    /// against the session workspace. The workspace itself is always writable.
    pub sandbox_writable_roots: Vec<String>,
    /// Copy what a mutating tool can change before it runs.
    pub snapshots: bool,
    /// Explicit permission rules. All matching rules are considered; `deny`
    /// wins, then the most specific `ask`/`allow`/`sandbox` rule.
    pub rules: Vec<PermissionRule>,
    /// Absolute paths (or `~`-prefixed) that file tools may touch outside the
    /// session workspace. A plain path allows its whole subtree.
    pub external_directory: Vec<String>,
    /// Optional allowlist for read-only network tools. Empty means `webfetch`
    /// and `websearch` are allowed; a non-empty list restricts them to the
    /// matching hosts.
    pub network_allow: Vec<String>,
}

impl ToolsConfig {
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

    /// Whether `url`'s host is in `network_allow`.
    pub fn network_host_allowed(&self, url: &str) -> bool {
        let Some(host) = url_host(url) else {
            return false;
        };
        let host = host.to_ascii_lowercase();
        self.network_allow.iter().any(|pattern| {
            let pattern = pattern.trim().trim_end_matches('.').to_ascii_lowercase();
            if let Some(suffix) = pattern.strip_prefix("*.") {
                host == suffix || host.ends_with(&format!(".{suffix}"))
            } else {
                host == pattern
            }
        })
    }

    /// All rules that match a call, in config order.
    pub fn matching_rules<'a>(&'a self, tool: &str, args: &Value) -> Vec<&'a PermissionRule> {
        self.rules
            .iter()
            .filter(|rule| rule.matches(tool, args))
            .collect()
    }
}

fn url_host(url: &str) -> Option<&str> {
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit('@').next()?.split(':').next()?;
    (!host.is_empty()).then_some(host)
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
            approval: ApprovalMode::Ask,
            auto_apply_workspace_edits: true,
            max_output_bytes: 20_000,
            command_timeout_secs: 60,
            max_tool_rounds: 64,
            max_parallel_subagents: DEFAULT_MAX_PARALLEL_SUBAGENTS,
            sandbox: SandboxMode::Auto,
            sandbox_network: false,
            sandbox_read_paths: vec![
                "~/.cargo".to_string(),
                "~/.rustup".to_string(),
                "~/.cache".to_string(),
                "~/.npm".to_string(),
            ],
            sandbox_writable_roots: Vec::new(),
            snapshots: true,
            rules: Vec::new(),
            external_directory: Vec::new(),
            network_allow: Vec::new(),
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

/// One or more post-write formatters.
///
/// Accepts either a single `[formatter]` table (legacy) or an ordered array of
/// `[[formatter]]` tables. The first entry whose extensions match a file wins.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(transparent)]
pub struct FormattersConfig(pub Vec<FormatterConfig>);

impl FormattersConfig {
    pub fn matching(&self, path: &Path) -> Option<&FormatterConfig> {
        self.0.iter().find(|formatter| formatter.applies_to(path))
    }
}

impl From<FormatterConfig> for FormattersConfig {
    fn from(formatter: FormatterConfig) -> Self {
        Self(vec![formatter])
    }
}

impl From<Vec<FormatterConfig>> for FormattersConfig {
    fn from(formatters: Vec<FormatterConfig>) -> Self {
        Self(formatters)
    }
}

impl<'de> Deserialize<'de> for FormattersConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum OneOrMany {
            One(FormatterConfig),
            Many(Vec<FormatterConfig>),
        }

        Ok(match OneOrMany::deserialize(deserializer)? {
            OneOrMany::One(formatter) => Self(vec![formatter]),
            OneOrMany::Many(formatters) => Self(formatters),
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
    /// Approximate token budget for the newest conversation kept verbatim
    /// beside the summary. This is the primary retention control, matching
    /// OpenCode's `compaction.keep.tokens`.
    pub auto_compact_keep_tokens: usize,
    /// Minimum number of newest user turns to keep even when their estimated
    /// token cost exceeds `auto_compact_keep_tokens`.
    pub auto_compact_keep_turns: usize,
    /// Maximum estimated tokens kept for one tool result in provider context.
    /// The full output remains in the event log; older/short results are left
    /// untouched. This prevents one noisy command from crowding out the task.
    pub tool_output_tokens: usize,
    /// Hard cap for the tool schemas sent in one request. Built-in schemas are
    /// always kept; MCP tools that would exceed the cap are omitted until the
    /// user narrows them with `mcp.servers.*.allowed_tools`.
    pub max_tool_schema_tokens: usize,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            budget_tokens: 32_000,
            instructions_files: vec!["AGENTS.md".to_string()],
            workspace_summary: true,
            auto_compact: true,
            auto_compact_threshold: 0.8,
            auto_compact_keep_tokens: 8_000,
            auto_compact_keep_turns: 2,
            tool_output_tokens: 4_000,
            max_tool_schema_tokens: 12_000,
        }
    }
}

impl ContextConfig {
    /// Budget actually used for one request. A configured model window always
    /// wins over the static default so a 32k budget cannot be sent to an 8k
    /// model. The reserve leaves room for the completion and provider-side
    /// tokenization differences.
    pub fn effective_budget_tokens(&self, model_context_tokens: Option<usize>) -> usize {
        let Some(window) = model_context_tokens.filter(|tokens| *tokens > 0) else {
            return self.budget_tokens.max(1);
        };
        let reserve = (window / 4).clamp(1_024, 8_000);
        self.budget_tokens
            .min(window.saturating_sub(reserve).max(1))
            .max(1)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Friendly display name; defaults to the table key.
    pub name: Option<String>,
    /// The fallback provider. Exactly one configured provider must set this.
    pub default: bool,
    /// Base URL of an OpenAI-compatible API, e.g. `https://api.openai.com/v1`.
    pub base_url: String,
    /// Default model for this provider. Only the default provider's model is
    /// used as the global fallback; named providers use explicit model ids.
    pub model: String,
    /// Inline key. Prefer `api_key_env` so keys never live in the repo/config.
    pub api_key: Option<String>,
    /// Environment variable holding the key. Empty means no authentication.
    pub api_key_env: String,
}

/// Global provider runtime knobs. Kept separate from endpoint/model tables so
/// every provider shares one timeout, retry policy and prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderRuntimeConfig {
    /// Overall request timeout. Streaming responses are bounded by this.
    pub timeout_secs: u64,
    /// Advertised context window for the default model, when known.
    pub context_tokens: Option<usize>,
    /// Extra attempts after the first one for transient failures.
    pub retry_attempts: u32,
    /// Base delay for jittered exponential backoff between retries.
    pub retry_base_ms: u64,
    /// Upper bound for a single retry delay.
    pub retry_max_secs: u64,
    /// Optional replacement for the built-in system prompt.
    pub system_prompt: Option<String>,
}

impl Default for ProviderRuntimeConfig {
    fn default() -> Self {
        Self {
            timeout_secs: 300,
            context_tokens: None,
            retry_attempts: 8,
            retry_base_ms: 250,
            retry_max_secs: 60,
            system_prompt: None,
        }
    }
}

impl ProviderConfig {
    /// The built-in OpenAI endpoint used when no providers are configured.
    pub fn openai() -> Self {
        Self {
            name: Some("OpenAI".to_string()),
            default: true,
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4o-mini".to_string(),
            api_key_env: "OPENAI_API_KEY".to_string(),
            ..Self::default()
        }
    }

    /// Resolve this provider's API key: inline first, then the named
    /// environment variable. Empty values count as absent.
    pub fn api_key(&self) -> Option<String> {
        if let Some(key) = self
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|key| !key.is_empty())
        {
            return Some(key.to_string());
        }
        let name = self.api_key_env.trim();
        if name.is_empty() {
            None
        } else {
            env_var(name)
        }
    }
}

/// One additional model exposed by `/model`. Each entry may point at its own
/// OpenAI-compatible endpoint, so one installation can mix providers.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelConfig {
    /// Selection id shown in `/model`, e.g. `openai/gpt-4o`.
    pub id: String,
    /// API model id. Defaults to the part after the first `/` in `id`, which
    /// makes `openai/gpt-4o` work against an OpenAI-compatible endpoint.
    pub model: Option<String>,
    /// Friendly name; defaults to `id`.
    pub name: Option<String>,
    /// Named provider from `[providers.*]`. When set, `base_url` is optional
    /// and the provider's endpoint/key are used.
    pub provider: Option<String>,
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
    /// Model id accepted by the provider API, without the `<provider>/`
    /// selection prefix. Frontends show this in
    /// pickers while `id` remains the value used for routing.
    #[serde(default)]
    pub api_model: Option<String>,
    pub name: String,
    pub provider: String,
    pub default: bool,
    pub variants: Vec<String>,
    pub input_cost: Option<f64>,
    pub output_cost: Option<f64>,
    pub context_tokens: Option<usize>,
}

impl ModelInfo {
    /// Id shown in pickers: the raw API id when known, otherwise the full
    /// selection id.
    pub fn display_id(&self) -> &str {
        self.api_model.as_deref().unwrap_or(&self.id)
    }
}

/// One endpoint whose `/models` catalog should be fetched. `default` marks the
/// provider used for fallback model ids.
#[derive(Debug, Clone)]
pub struct ProviderSource {
    pub key: String,
    pub name: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub default: bool,
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
    /// Advertised context window for this model, when known.
    pub context_tokens: Option<usize>,
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
    /// The provider marked `default = true`. Falls back to the first configured
    /// provider so a partially constructed `Config` still behaves sensibly;
    /// loaded configs are validated to have exactly one default.
    pub fn default_provider(&self) -> &ProviderConfig {
        self.providers
            .values()
            .find(|provider| provider.default)
            .or_else(|| self.providers.values().next())
            .expect("config must define at least one provider")
    }

    /// Table key of [`default_provider`](Self::default_provider).
    pub fn default_provider_key(&self) -> &str {
        self.providers
            .iter()
            .find(|(_, provider)| provider.default)
            .or_else(|| self.providers.iter().next())
            .map(|(key, _)| key.as_str())
            .unwrap_or("openai")
    }

    /// Mutable default provider, used by environment overrides and tests.
    pub fn default_provider_mut(&mut self) -> &mut ProviderConfig {
        let key = self
            .providers
            .iter()
            .find(|(_, provider)| provider.default)
            .or_else(|| self.providers.iter().next())
            .map(|(key, _)| key.clone())
            .unwrap_or_else(|| "openai".to_string());
        if !self.providers.contains_key(&key) {
            self.providers.insert(key.clone(), ProviderConfig::openai());
        }
        self.providers
            .get_mut(&key)
            .expect("default provider was just inserted")
    }

    /// The global fallback model, taken from the default provider.
    pub fn default_model(&self) -> &str {
        self.default_provider().model.trim()
    }

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
        let mut rules = Vec::new();
        let mut security_warnings = Vec::new();

        // `--config` and EIX_CONFIG are authoritative; do not mix project
        // layers into a deliberately reproducible configuration.
        let explicit = path
            .map(Path::to_path_buf)
            .or_else(|| env_var("EIX_CONFIG").map(PathBuf::from));

        if let Some(path) = explicit {
            explicit_path = Some(path.clone());
            watch_paths.push(path.clone());
            if path.exists() {
                let mut layer = read_layer(&path)?;
                collect_rules(&mut layer, RuleSource::User, &path, &mut rules)?;
                merge_toml(&mut merged, layer);
            }
        } else {
            let global = default_config_path();
            watch_paths.push(global.clone());
            if global.exists() {
                let mut layer = read_layer(&global)?;
                collect_rules(&mut layer, RuleSource::User, &global, &mut rules)?;
                merge_toml(&mut merged, layer);
            }
            let mut dirs: Vec<PathBuf> = cwd.ancestors().map(Path::to_path_buf).collect();
            dirs.reverse();
            for dir in dirs {
                let candidate = dir.join(".eix").join("config.toml");
                if candidate.exists() {
                    let mut layer = read_layer(&candidate)?;
                    sanitize_project_layer(&mut layer, &mut security_warnings);
                    collect_project_rules(
                        &mut layer,
                        &candidate,
                        &mut rules,
                        &mut security_warnings,
                    )?;
                    merge_toml(&mut merged, layer);
                }
                watch_paths.push(candidate);
            }
        }

        // Migrate the old single `[provider]` table into the unified provider
        // map. New configs should use `[providers.<name>] default = true`.
        if let Some(mut provider) = merged
            .as_table_mut()
            .and_then(|table| table.remove("provider"))
        {
            let Some(provider_table) = provider.as_table_mut() else {
                return Err(ConfigError::Invalid("`provider` must be a table".into()));
            };
            provider_table.insert("default".into(), toml::Value::Boolean(true));
            // Legacy runtime knobs now live in their own [provider_runtime]
            // table instead of being repeated on a provider.
            let table = merged.as_table_mut().expect("merged config is a table");
            let runtime = table
                .entry("provider_runtime")
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            if let Some(runtime) = runtime.as_table_mut() {
                for key in [
                    "timeout_secs",
                    "context_tokens",
                    "retry_attempts",
                    "retry_base_ms",
                    "retry_max_secs",
                    "system_prompt",
                ] {
                    if !runtime.contains_key(key) {
                        if let Some(value) = provider_table.remove(key) {
                            runtime.insert(key.to_string(), value);
                        }
                    }
                }
            }
            let providers = table
                .entry("providers")
                .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
            let Some(providers) = providers.as_table_mut() else {
                return Err(ConfigError::Invalid("`providers` must be a table".into()));
            };
            if providers.contains_key("default") {
                return Err(ConfigError::Invalid(
                    "cannot combine legacy [provider] with [providers.default]".into(),
                ));
            }
            providers.insert("default".into(), provider);
        }
        let mut config: Self = merged.try_into().map_err(|source| ConfigError::Parse {
            path: explicit_path.clone().unwrap_or_else(default_config_path),
            source,
        })?;
        config.tools.rules = rules;
        config.explicit_path = explicit_path;
        config.load_cwd = cwd.to_path_buf();
        config.watch_paths = watch_paths;
        config.security_warnings = security_warnings;
        config.apply_env();
        config.validate()?;
        Ok(config)
    }

    /// Re-read the files this config came from. Used by hot reload.
    pub fn reload(&self) -> Result<Self, ConfigError> {
        Self::load_for(self.explicit_path.as_deref(), &self.load_cwd)
    }

    fn apply_env(&mut self) {
        // Keep only provider entry-point overrides. Every other setting is
        // configured in the config file so a stale shell export cannot silently
        // change daemon behavior.
        let provider = self.default_provider_mut();
        if let Some(value) = env_var("EIX_BASE_URL") {
            provider.base_url = value;
        }
        if let Some(value) = env_var("EIX_MODEL") {
            provider.model = value;
        }
        if let Some(value) = env_var("EIX_API_KEY") {
            provider.api_key = Some(value);
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
        if self.providers.is_empty() {
            return Err(ConfigError::Invalid(
                "providers must contain at least one provider".into(),
            ));
        }
        let defaults = self
            .providers
            .iter()
            .filter(|(_, provider)| provider.default)
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        if defaults.len() != 1 {
            return Err(ConfigError::Invalid(format!(
                "providers must mark exactly one provider with default = true (found {})",
                defaults.len()
            )));
        }
        for (name, provider) in &self.providers {
            if name.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "providers contains an empty name".into(),
                ));
            }
            if name.contains('/') || name.contains('#') || name.chars().any(char::is_whitespace) {
                return Err(ConfigError::Invalid(format!(
                    "providers.{name} must not contain '/', '#' or whitespace"
                )));
            }
            if provider.base_url.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "providers.{name}.base_url is empty"
                )));
            }
            if !(provider.base_url.starts_with("http://")
                || provider.base_url.starts_with("https://"))
            {
                return Err(ConfigError::Invalid(format!(
                    "providers.{name}.base_url must start with http:// or https:// (got {:?})",
                    provider.base_url
                )));
            }
            if provider.default && provider.model.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "providers.{name}.model is empty"
                )));
            }
        }
        if self.provider_runtime.timeout_secs == 0 {
            return Err(ConfigError::Invalid(
                "provider_runtime.timeout_secs must be > 0".into(),
            ));
        }
        if self.provider_runtime.retry_base_ms == 0 {
            return Err(ConfigError::Invalid(
                "provider_runtime.retry_base_ms must be > 0".into(),
            ));
        }
        if self.provider_runtime.retry_max_secs == 0 {
            return Err(ConfigError::Invalid(
                "provider_runtime.retry_max_secs must be > 0".into(),
            ));
        }
        if self.provider_runtime.retry_max_secs.saturating_mul(1_000)
            < self.provider_runtime.retry_base_ms
        {
            return Err(ConfigError::Invalid(
                "provider_runtime.retry_max_secs is shorter than retry_base_ms".into(),
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
        if self.tools.max_tool_rounds == 0 {
            return Err(ConfigError::Invalid(
                "tools.max_tool_rounds must be > 0".into(),
            ));
        }
        if !subagent_concurrency_supported(self.tools.max_parallel_subagents) {
            return Err(ConfigError::Invalid(
                "tools.max_parallel_subagents must be one of 1, 2, 4, 8, 16, 32".into(),
            ));
        }
        if self.integration.enabled {
            if self.integration.idle_secs == 0 && self.integration.auto_ready == AutoReady::Idle {
                return Err(ConfigError::Invalid(
                    "integration.idle_secs must be > 0 when auto_ready = \"idle\"".into(),
                ));
            }
            if self.integration.timeout_secs == 0 {
                return Err(ConfigError::Invalid(
                    "integration.timeout_secs must be > 0".into(),
                ));
            }
            if self.integration.retry_max_secs == 0 {
                return Err(ConfigError::Invalid(
                    "integration.retry_max_secs must be > 0".into(),
                ));
            }
            if self.integration.test.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "integration.test must be \"auto\" or a command".into(),
                ));
            }
        }
        for (repo, over) in &self.integration.repos {
            if repo.trim().is_empty() {
                return Err(ConfigError::Invalid(
                    "integration.repos must use a non-empty repository path".into(),
                ));
            }
            if over
                .base
                .as_deref()
                .is_some_and(|base| base.trim().is_empty())
            {
                return Err(ConfigError::Invalid(format!(
                    "integration.repos[{repo:?}].base must not be empty"
                )));
            }
            if over
                .test
                .as_deref()
                .is_some_and(|test| test.trim().is_empty())
            {
                return Err(ConfigError::Invalid(format!(
                    "integration.repos[{repo:?}].test must not be empty"
                )));
            }
            if over.timeout_secs == Some(0) {
                return Err(ConfigError::Invalid(format!(
                    "integration.repos[{repo:?}].timeout_secs must be > 0"
                )));
            }
        }
        for (index, formatter) in self.formatter.0.iter().enumerate() {
            if formatter
                .command
                .as_deref()
                .is_some_and(|command| command.trim().is_empty())
            {
                return Err(ConfigError::Invalid(format!(
                    "formatter[{index}].command must not be empty"
                )));
            }
            if formatter.timeout_secs == 0 {
                return Err(ConfigError::Invalid(format!(
                    "formatter[{index}].timeout_secs must be > 0"
                )));
            }
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
        for (index, rule) in self.tools.rules.iter().enumerate() {
            if rule.tool.trim().is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "tools.rules[{index}].tool is required (use \"*\" to match any tool)"
                )));
            }
            if rule
                .path
                .as_deref()
                .is_some_and(|path| path.trim().is_empty())
            {
                return Err(ConfigError::Invalid(format!(
                    "tools.rules[{index}].path is empty"
                )));
            }
            if let Some(command) = rule.command.as_ref() {
                if command.is_empty() || command.iter().any(|token| token.is_empty()) {
                    return Err(ConfigError::Invalid(format!(
                        "tools.rules[{index}].command must be a non-empty argv prefix"
                    )));
                }
            }
            if let Some(pattern) = rule.command_regex.as_deref() {
                Regex::new(pattern).map_err(|err| {
                    ConfigError::Invalid(format!(
                        "tools.rules[{index}].command_regex is not a valid regex: {err}"
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
            if let Some(provider) = model
                .provider
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
            {
                if !self.providers.contains_key(provider) {
                    return Err(ConfigError::Invalid(format!(
                        "models[{index}].provider {provider:?} is not configured"
                    )));
                }
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
        if self.context.tool_output_tokens == 0 {
            return Err(ConfigError::Invalid(
                "context.tool_output_tokens must be > 0".into(),
            ));
        }
        if self.context.max_tool_schema_tokens == 0 {
            return Err(ConfigError::Invalid(
                "context.max_tool_schema_tokens must be > 0".into(),
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

    /// Endpoints whose `/models` catalogs are fetched for `/model`.
    pub fn provider_sources(&self) -> Vec<ProviderSource> {
        self.providers
            .iter()
            .map(|(key, provider)| ProviderSource {
                key: key.clone(),
                name: provider
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(key)
                    .to_string(),
                base_url: provider.base_url.clone(),
                api_key: provider.api_key(),
                default: provider.default,
            })
            .collect()
    }

    /// Models shown by `/model`: the default provider's model first, then
    /// configured entries. Duplicate ids keep the first occurrence.
    pub fn models(&self) -> Vec<ModelInfo> {
        let mut out = Vec::new();
        let default_key = self.default_provider_key().to_string();
        let default = self.default_provider();
        let default_id = default.model.trim().to_string();
        if !default_id.is_empty() {
            out.push(ModelInfo {
                name: default_id.clone(),
                id: default_id.clone(),
                api_model: None,
                provider: default_key,
                default: true,
                variants: Vec::new(),
                input_cost: None,
                output_cost: None,
                context_tokens: self.provider_runtime.context_tokens,
            });
        }
        for model in &self.models {
            let id = model.id.trim();
            if id.is_empty() || out.iter().any(|info| info.id == id) {
                continue;
            }
            out.push(ModelInfo {
                id: id.to_string(),
                api_model: model
                    .model
                    .as_deref()
                    .map(str::trim)
                    .map(str::trim)
                    .filter(|api_model| !api_model.is_empty())
                    .map(str::to_string),
                name: model
                    .name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .unwrap_or(id)
                    .to_string(),
                provider: model
                    .provider
                    .as_deref()
                    .map(str::trim)
                    .filter(|provider| !provider.is_empty())
                    .map(str::to_string)
                    .or_else(|| id.split_once('/').map(|(provider, _)| provider.to_string()))
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
        let default = self.default_provider();
        let default_id = default.model.trim();

        let (base_url, api_key, api_model, input_cost, output_cost, provider_context) =
            if let Some(model) = configured {
                let explicit_provider = model
                    .provider
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty());
                let inferred_provider = base_id
                    .split_once('/')
                    .map(|(provider, _)| provider)
                    .filter(|provider| self.providers.contains_key(*provider));
                let provider_key = explicit_provider.or(inferred_provider);
                let provider = provider_key.and_then(|name| self.providers.get(name));
                let base_url = if !model.base_url.trim().is_empty() {
                    model.base_url.clone()
                } else if let Some(provider) = provider {
                    provider.base_url.clone()
                } else {
                    default.base_url.clone()
                };
                let api_key = model
                    .api_key
                    .as_deref()
                    .map(str::trim)
                    .filter(|key| !key.is_empty())
                    .map(str::to_string)
                    .or_else(|| model.api_key_env.as_deref().and_then(env_var))
                    .or_else(|| provider.and_then(ProviderConfig::api_key))
                    .or_else(|| default.api_key());
                let api_model = model
                    .model
                    .as_deref()
                    .map(str::trim)
                    .filter(|model| !model.is_empty())
                    .map(str::to_string)
                    .or_else(|| base_id.split_once('/').map(|(_, model)| model.to_string()))
                    .unwrap_or_else(|| base_id.to_string());
                (
                    base_url,
                    api_key,
                    api_model,
                    model.input_cost,
                    model.output_cost,
                    model.context_tokens,
                )
            } else if !base_id.is_empty() && base_id == default_id {
                (
                    default.base_url.clone(),
                    default.api_key(),
                    base_id.to_string(),
                    None,
                    None,
                    self.provider_runtime.context_tokens,
                )
            } else if let Some((provider_key, model_id)) = base_id.split_once('/') {
                if provider_key == "default" && !self.providers.contains_key("default") {
                    (
                        default.base_url.clone(),
                        default.api_key(),
                        model_id.to_string(),
                        None,
                        None,
                        self.provider_runtime.context_tokens,
                    )
                } else if let Some(provider) = self.providers.get(provider_key) {
                    (
                        provider.base_url.clone(),
                        provider.api_key(),
                        model_id.to_string(),
                        None,
                        None,
                        None,
                    )
                } else {
                    // Unknown ids keep the old behavior: send them verbatim to
                    // the default endpoint.
                    (
                        default.base_url.clone(),
                        default.api_key(),
                        base_id.to_string(),
                        None,
                        None,
                        self.provider_runtime.context_tokens,
                    )
                }
            } else {
                (
                    default.base_url.clone(),
                    default.api_key(),
                    if base_id.is_empty() {
                        default.model.clone()
                    } else {
                        base_id.to_string()
                    },
                    None,
                    None,
                    self.provider_runtime.context_tokens,
                )
            };

        // Reasoning defaults to medium. The TUI uses the reserved `#none`
        // variant to mean "do not send reasoning_effort at all", which keeps
        // the empty Ctrl-T step distinct from the default.
        let reasoning_effort = match variant {
            Some(variant) if variant.eq_ignore_ascii_case("none") => None,
            Some(variant) => Some(variant),
            None => Some("medium".to_string()),
        };

        ModelRoute {
            model: api_model,
            endpoint: chat_endpoint(&base_url),
            api_key,
            // Most OpenAI-compatible servers accept this for reasoning models;
            // it is ignored by the others.
            reasoning_effort,
            input_cost,
            output_cost,
            context_tokens: provider_context,
        }
    }

    /// Resolve the default provider's API key.
    pub fn api_key(&self) -> Option<String> {
        self.default_provider().api_key()
    }

    /// Full chat-completions URL for the default provider. Accepts either a
    /// base (`.../v1`) or an explicit endpoint (`.../v1/chat/completions`).
    pub fn chat_endpoint(&self) -> String {
        chat_endpoint(&self.default_provider().base_url)
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

/// OpenAI-compatible model catalog URL. Accepts the same base forms as
/// [`chat_endpoint`], including an explicit `/chat/completions` endpoint.
pub fn models_endpoint(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    if let Some(base) = base.strip_suffix("/chat/completions") {
        format!("{base}/models")
    } else if base.ends_with("/models") {
        base.to_string()
    } else {
        format!("{base}/models")
    }
}

fn env_var(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Read one TOML layer after expanding `{env:NAME}` / `{file:path}`.
fn collect_rules(
    layer: &mut toml::Value,
    source: RuleSource,
    path: &Path,
    out: &mut Vec<PermissionRule>,
) -> Result<(), ConfigError> {
    let Some(rules) = take_tools_array(layer, "rules") else {
        return Ok(());
    };
    for value in rules {
        let mut rule: PermissionRule = value.try_into().map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        rule.source = source;
        out.push(rule);
    }
    Ok(())
}

/// Project layers may only tighten policy. `allow` rules are downgraded to
/// `ask`; `sandbox`/`ask`/`deny` are portable because they cannot expand the
/// user's authority beyond what the risk engine already permits.
fn collect_project_rules(
    layer: &mut toml::Value,
    path: &Path,
    out: &mut Vec<PermissionRule>,
    warnings: &mut Vec<String>,
) -> Result<(), ConfigError> {
    let Some(rules) = take_tools_array(layer, "rules") else {
        return Ok(());
    };
    for value in rules {
        let mut rule: PermissionRule = value.try_into().map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        if rule.action == PermissionAction::Allow {
            warnings.push(format!(
                "{}: rules[{}].action = \"allow\" is not allowed in project config; downgraded to ask",
                path.display(),
                if rule.id.is_empty() { "?" } else { &rule.id }
            ));
            rule.action = PermissionAction::Ask;
        }
        rule.source = RuleSource::Project;
        out.push(rule);
    }
    Ok(())
}

fn take_tools_array(layer: &mut toml::Value, key: &str) -> Option<Vec<toml::Value>> {
    layer
        .get_mut("tools")
        .and_then(toml::Value::as_table_mut)
        .and_then(|tools| tools.remove(key))
        .and_then(|value| value.as_array().cloned())
}

fn sanitize_project_layer(layer: &mut toml::Value, warnings: &mut Vec<String>) {
    if let Some(tools) = layer.get_mut("tools").and_then(toml::Value::as_table_mut) {
        if let Some(other) = tools.get("approval").and_then(toml::Value::as_str) {
            if other != "ask" {
                let other = other.to_string();
                tools.remove("approval");
                warnings.push(format!(
                    "project config cannot set tools.approval = {other:?}; ignored"
                ));
            }
        }
        if let Some(other) = tools.get("sandbox").and_then(toml::Value::as_str) {
            if other != "required" {
                let other = other.to_string();
                tools.remove("sandbox");
                warnings.push(format!(
                    "project config cannot set tools.sandbox = {other:?}; ignored"
                ));
            }
        }
        if tools.get("sandbox_network").and_then(toml::Value::as_bool) == Some(true) {
            tools.insert("sandbox_network".into(), toml::Value::Boolean(false));
            warnings.push("project config cannot enable sandbox_network; forced false".into());
        }
        for key in [
            "sandbox_read_paths",
            "sandbox_writable_roots",
            "external_directory",
            "network_allow",
        ] {
            if tools.remove(key).is_some() {
                warnings.push(format!("project config cannot add tools.{key}; ignored"));
            }
        }
        if tools.get("snapshots").and_then(toml::Value::as_bool) == Some(false) {
            tools.remove("snapshots");
            warnings.push("project config cannot disable snapshots; ignored".into());
        }
    }

    // Automatic merging and test commands are executable policy. Only the
    // global config (or an explicit trusted layer) may set them.
    for key in ["integration", "worktree"] {
        if layer
            .as_table_mut()
            .is_some_and(|table| table.remove(key).is_some())
        {
            warnings.push(format!(
                "project config cannot set [{key}]; use the global config"
            ));
        }
    }

    // Enabling compatibility loads user-global skills/commands from another
    // agent. A project may turn a switch off, but never on.
    if let Some(compat) = layer
        .get_mut("compations")
        .and_then(toml::Value::as_table_mut)
    {
        for key in ["claude_code", "opencode", "codex_cli", "gemini_cli", "pi"] {
            if compat.get(key).and_then(toml::Value::as_bool) == Some(true) {
                compat.insert(key.into(), toml::Value::Boolean(false));
                warnings.push(format!(
                    "project config cannot enable compations.{key}; forced false"
                ));
            }
        }
    }
}

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

/// Cross-platform config root used by eix's own files.
///
/// eix deliberately keeps its files in `~/.config/eix` on every Unix-like
/// platform, including macOS where `dirs::config_dir()` points at
/// `~/Library/Application Support`. `XDG_CONFIG_HOME` overrides the base
/// directory and `EIX_CONFIG_DIR` overrides the final eix directory.
pub fn config_home() -> PathBuf {
    if let Some(path) = env_var("XDG_CONFIG_HOME") {
        return PathBuf::from(path);
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
}

/// The eix config directory (`~/.config/eix` by default).
pub fn config_dir() -> PathBuf {
    if let Some(path) = env_var("EIX_CONFIG_DIR") {
        return PathBuf::from(path);
    }
    config_home().join("eix")
}

/// Directory for user-installed skills.
pub fn skills_dir() -> PathBuf {
    config_dir().join("skills")
}

/// Directory for user-defined prompt commands.
pub fn cmds_dir() -> PathBuf {
    config_dir().join("cmds")
}

pub fn default_config_path() -> PathBuf {
    if let Some(path) = env_var("EIX_CONFIG") {
        return PathBuf::from(path);
    }
    config_dir().join("config.toml")
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
/// socket. Linux distributions such as Arch normally set `XDG_RUNTIME_DIR`.
/// macOS has no runtime dir, so use the per-user cache dir instead; this keeps
/// the path short enough for the Unix socket limit and avoids sharing `/tmp`
/// with other users. The final fallback is a user-specific temp path.
pub fn socket_path() -> PathBuf {
    if let Some(path) = env_var("EIX_SOCKET") {
        return PathBuf::from(path);
    }
    if let Some(runtime) = dirs::runtime_dir() {
        return runtime.join("eix.sock");
    }
    if let Some(cache) = dirs::cache_dir() {
        return cache.join("eix").join("eix.sock");
    }
    let user = env_var("USER")
        .or_else(|| env_var("USERNAME"))
        .unwrap_or_else(|| "eix".to_string());
    env::temp_dir().join(format!("eix-{user}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn endpoints() {
        let mut config = Config::default();
        config.default_provider_mut().base_url = "http://localhost:11434/v1/".into();
        assert_eq!(
            config.chat_endpoint(),
            "http://localhost:11434/v1/chat/completions"
        );

        config.default_provider_mut().base_url = "http://localhost:1234/v1/chat/completions".into();
        assert_eq!(
            config.chat_endpoint(),
            "http://localhost:1234/v1/chat/completions"
        );
    }

    #[test]
    fn formatter_accepts_single_table_and_array() {
        let single: Config = toml::from_str(
            r#"
[formatter]
command = "rustfmt --edition 2021"
extensions = ["rs"]
"#,
        )
        .unwrap();
        assert_eq!(single.formatter.0.len(), 1);
        assert_eq!(single.formatter.0[0].extensions, vec!["rs"]);

        let many: Config = toml::from_str(
            r#"
[[formatter]]
command = "rustfmt --edition 2021"
extensions = ["rs"]

[[formatter]]
command = "gofmt -w"
extensions = ["go"]
"#,
        )
        .unwrap();
        assert_eq!(many.formatter.0.len(), 2);
        assert_eq!(many.formatter.0[1].command.as_deref(), Some("gofmt -w"));
    }

    #[test]
    fn invalid_base_url_is_rejected() {
        let mut config = Config::default();
        config.default_provider_mut().base_url = "localhost:11434".into();
        assert!(config.validate().is_err());
    }

    #[test]
    fn default_model_is_not_empty() {
        assert!(!Config::default().default_model().is_empty());
    }

    #[test]
    fn provider_runtime_is_configured_separately() {
        let config: Config = toml::from_str(
            r#"
[providers.local]
default = true
base_url = "http://127.0.0.1:11434/v1"
model = "qwen3"

[provider_runtime]
timeout_secs = 42
retry_attempts = 2
retry_base_ms = 10
retry_max_secs = 1
context_tokens = 8192
system_prompt = "custom"
"#,
        )
        .expect("parse");
        config.validate().expect("valid runtime");
        assert_eq!(config.provider_runtime.timeout_secs, 42);
        assert_eq!(config.provider_runtime.retry_attempts, 2);
        assert_eq!(config.provider_runtime.context_tokens, Some(8192));
        assert_eq!(
            config.provider_runtime.system_prompt.as_deref(),
            Some("custom")
        );
    }

    #[test]
    fn provider_retry_defaults_are_bounded() {
        let runtime = Config::default().provider_runtime;
        assert!(runtime.retry_attempts > 0);
        assert!(runtime.retry_base_ms > 0);
        assert!(runtime.retry_max_secs > 0);
        assert!(runtime.retry_max_secs.saturating_mul(1_000) >= runtime.retry_base_ms);
    }

    #[test]
    fn context_compaction_defaults_are_token_first() {
        let context = ContextConfig::default();
        assert!(context.auto_compact);
        assert_eq!(context.auto_compact_keep_tokens, 8_000);
        assert_eq!(context.auto_compact_keep_turns, 2);
    }

    #[test]
    fn effective_budget_reserves_room_for_the_completion() {
        let context = ContextConfig {
            budget_tokens: 32_000,
            ..ContextConfig::default()
        };
        // No known window: keep the configured budget.
        assert_eq!(context.effective_budget_tokens(None), 32_000);
        assert_eq!(context.effective_budget_tokens(Some(0)), 32_000);

        // A high configured budget is clamped below a small real window.
        assert_eq!(context.effective_budget_tokens(Some(8_000)), 6_000);
        // A large window still uses the configured budget.
        assert_eq!(context.effective_budget_tokens(Some(1_000_000)), 32_000);
        // Tiny windows never collapse to zero.
        assert_eq!(context.effective_budget_tokens(Some(1)), 1);
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
            providers: BTreeMap::from([
                (
                    "local".to_string(),
                    ProviderConfig {
                        default: true,
                        base_url: "http://127.0.0.1:11434/v1".into(),
                        model: "local/llama".into(),
                        ..ProviderConfig::default()
                    },
                ),
                (
                    "openai".to_string(),
                    ProviderConfig {
                        base_url: "https://api.openai.com/v1".into(),
                        ..ProviderConfig::default()
                    },
                ),
            ]),
            models: vec![
                ModelConfig {
                    id: "local/llama".into(),
                    ..ModelConfig::default()
                },
                ModelConfig {
                    id: "openai/gpt-5".into(),
                    model: Some("gpt-5".into()),
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
        assert_eq!(models[0].display_id(), "local/llama");
        assert!(models[0].default);
        assert_eq!(models[0].provider, "local");
        assert_eq!(models[1].id, "openai/gpt-5");
        assert_eq!(models[1].api_model.as_deref(), Some("gpt-5"));
        assert_eq!(models[1].display_id(), "gpt-5");
        assert_eq!(models[1].name, "GPT-5");
        assert_eq!(models[1].provider, "openai");
        assert_eq!(models[1].variants, vec!["high", "low"]);
    }

    #[test]
    fn named_providers_route_fetched_model_ids() {
        let mut config: Config = toml::from_str(
            r#"
[providers.openai]
default = true
base_url = "https://default.test/v1"
model = "default-model"
api_key = "default-key"

[providers.openrouter]
name = "OpenRouter"
base_url = "https://openrouter.test/api/v1"
api_key = "router-key"

[providers.local]
base_url = "http://127.0.0.1:11434/v1"
"#,
        )
        .expect("parse");

        let sources = config.provider_sources();
        assert_eq!(sources.len(), 3);
        let openai = sources
            .iter()
            .find(|source| source.key == "openai")
            .unwrap();
        assert!(openai.default);
        assert_eq!(openai.api_key.as_deref(), Some("default-key"));
        let local = sources.iter().find(|source| source.key == "local").unwrap();
        assert!(local.api_key.is_none());
        let openrouter = sources
            .iter()
            .find(|source| source.key == "openrouter")
            .unwrap();
        assert_eq!(openrouter.name, "OpenRouter");
        assert_eq!(openrouter.api_key.as_deref(), Some("router-key"));

        // Fetched catalog ids use `<provider>/<api model>`; the API model may
        // itself contain slashes.
        let route = config.route("openrouter/openai/gpt-5#high");
        assert_eq!(route.model, "openai/gpt-5");
        assert_eq!(
            route.endpoint,
            "https://openrouter.test/api/v1/chat/completions"
        );
        assert_eq!(route.api_key.as_deref(), Some("router-key"));
        assert_eq!(route.reasoning_effort.as_deref(), Some("high"));

        // Unspecified reasoning defaults to medium; the reserved `#none`
        // selection omits reasoning_effort entirely.
        let default_effort = config.route("openrouter/openai/gpt-5");
        assert_eq!(default_effort.reasoning_effort.as_deref(), Some("medium"));
        let no_effort = config.route("openrouter/openai/gpt-5#none");
        assert_eq!(no_effort.reasoning_effort, None);

        let local = config.route("local/llama3");
        assert_eq!(local.model, "llama3");
        assert_eq!(local.endpoint, "http://127.0.0.1:11434/v1/chat/completions");
        assert!(local.api_key.is_none());

        let default_provider = config.route("openai/gpt-5");
        assert_eq!(default_provider.model, "gpt-5");
        assert_eq!(default_provider.endpoint, config.chat_endpoint());
        assert_eq!(default_provider.api_key.as_deref(), Some("default-key"));

        // A configured default model keeps routing to the default provider.
        let default_model = config.route("default-model");
        assert_eq!(default_model.model, "default-model");
        assert_eq!(default_model.endpoint, config.chat_endpoint());

        // Unknown prefixes keep the old fallback behavior.
        let unknown = config.route("other/foo");
        assert_eq!(unknown.model, "other/foo");
        assert_eq!(unknown.endpoint, config.chat_endpoint());

        // Explicit [[models]] entries can reuse a named provider's endpoint.
        config.models.push(ModelConfig {
            id: "openrouter/claude".into(),
            provider: Some("openrouter".into()),
            model: Some("anthropic/claude-sonnet".into()),
            ..ModelConfig::default()
        });
        let explicit = config.route("openrouter/claude");
        assert_eq!(explicit.model, "anthropic/claude-sonnet");
        assert_eq!(
            explicit.endpoint,
            "https://openrouter.test/api/v1/chat/completions"
        );
        assert_eq!(explicit.api_key.as_deref(), Some("router-key"));
    }

    #[test]
    fn model_catalog_endpoint_accepts_base_and_chat_urls() {
        assert_eq!(
            models_endpoint("https://api.openai.com/v1/"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            models_endpoint("https://api.openai.com/v1/chat/completions"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            models_endpoint("https://api.openai.com/v1/models"),
            "https://api.openai.com/v1/models"
        );
    }

    #[test]
    fn invalid_providers_are_rejected() {
        let bad_url = Config {
            providers: BTreeMap::from([(
                "openai".to_string(),
                ProviderConfig {
                    default: true,
                    base_url: "localhost:11434".into(),
                    model: "gpt-4o-mini".into(),
                    ..ProviderConfig::default()
                },
            )]),
            ..Config::default()
        };
        assert!(bad_url.validate().is_err());

        let no_default = Config {
            providers: BTreeMap::from([(
                "openai".to_string(),
                ProviderConfig {
                    base_url: "https://example.test/v1".into(),
                    model: "gpt-4o-mini".into(),
                    ..ProviderConfig::default()
                },
            )]),
            ..Config::default()
        };
        assert!(no_default.validate().is_err());

        let two_defaults = Config {
            providers: BTreeMap::from([
                (
                    "one".to_string(),
                    ProviderConfig {
                        default: true,
                        base_url: "https://one.test/v1".into(),
                        model: "one".into(),
                        ..ProviderConfig::default()
                    },
                ),
                (
                    "two".to_string(),
                    ProviderConfig {
                        default: true,
                        base_url: "https://two.test/v1".into(),
                        model: "two".into(),
                        ..ProviderConfig::default()
                    },
                ),
            ]),
            ..Config::default()
        };
        assert!(two_defaults.validate().is_err());

        let unknown_model_provider = Config {
            models: vec![ModelConfig {
                id: "x/y".into(),
                provider: Some("missing".into()),
                ..ModelConfig::default()
            }],
            ..Config::default()
        };
        assert!(unknown_model_provider.validate().is_err());
    }

    #[test]
    fn legacy_provider_table_is_migrated() {
        let dir = config_temp_dir("legacy-provider");
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[provider]\nbase_url = \"https://legacy.test/v1\"\nmodel = \"legacy-model\"\ntimeout_secs = 42\n",
        )
        .unwrap();
        let config = Config::load_for(Some(&path), &dir).expect("legacy config");
        assert_eq!(config.default_provider_key(), "default");
        assert_eq!(config.default_model(), "legacy-model");
        assert_eq!(config.default_provider().base_url, "https://legacy.test/v1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_provider_name_is_not_reserved() {
        let config: Config = toml::from_str(
            r#"
[providers.local]
default = true
base_url = "http://127.0.0.1:11434/v1"
model = "qwen3"
api_key = "local-key"

[providers.openai]
base_url = "https://api.openai.com/v1"
api_key_env = "OPENAI_API_KEY"
"#,
        )
        .expect("parse");
        config.validate().expect("valid providers");
        assert_eq!(config.default_provider_key(), "local");
        assert_eq!(config.default_model(), "qwen3");
        assert_eq!(config.models()[0].provider, "local");
        assert_eq!(
            config.route("local/qwen3").endpoint,
            "http://127.0.0.1:11434/v1/chat/completions"
        );
        assert_eq!(
            config.route("openai/gpt-4o").endpoint,
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn route_uses_per_model_endpoint_and_strips_provider_prefix() {
        let mut config = Config::default();
        config.default_provider_mut().api_key = Some("default-key".into());
        config.models = vec![ModelConfig {
            id: "other/gpt-5".into(),
            base_url: "http://127.0.0.1:9/v1".into(),
            api_key: Some("other-key".into()),
            input_cost: Some(2.0),
            output_cost: Some(8.0),
            ..ModelConfig::default()
        }];

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
    fn permission_rules_match_and_report_specificity() {
        let parsed: Config = toml::from_str(
            r#"
[[tools.rules]]
id = "allow-cargo-test"
tool = "run_command"
command = ["cargo", "test"]
action = "allow"

[[tools.rules]]
id = "deny-rm"
tool = "run_command"
command_regex = "^rm "
action = "deny"

[[tools.rules]]
id = "ask-src"
tool = "write_file"
path = "src/**"
action = "ask"
"#,
        )
        .expect("parse rules");
        parsed.validate().expect("valid rules");

        let cargo = parsed
            .tools
            .matching_rules("run_command", &json!({"command": "cargo test --workspace"}));
        assert_eq!(cargo.len(), 1);
        assert_eq!(cargo[0].action, PermissionAction::Allow);
        assert_eq!(cargo[0].specificity(), 2);

        let rm = parsed
            .tools
            .matching_rules("run_command", &json!({"command": "rm -rf /"}));
        assert_eq!(
            rm.first().map(|rule| rule.action),
            Some(PermissionAction::Deny)
        );

        let write = parsed
            .tools
            .matching_rules("write_file", &json!({"path": "src/lib.rs"}));
        assert_eq!(
            write.first().map(|rule| rule.action),
            Some(PermissionAction::Ask)
        );

        assert!(parsed
            .tools
            .matching_rules("write_file", &json!({"path": "docs/readme.md"}))
            .is_empty());

        // A multi-segment command cannot satisfy a prefix rule for only one
        // segment. This is the `cargo test && rm -rf /` bypass guard.
        assert!(parsed
            .tools
            .matching_rules("run_command", &json!({"command": "cargo test && rm -rf /"}))
            .is_empty());
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
    fn project_config_cannot_loosen_permissions() {
        let root = config_temp_dir("project-tighten");
        std::fs::create_dir_all(root.join(".eix")).unwrap();
        std::fs::write(
            root.join(".eix/config.toml"),
            r#"
[tools]
approval = "auto"
sandbox = "off"
sandbox_network = true
sandbox_read_paths = ["/"]
sandbox_writable_roots = ["/"]
external_directory = ["/"]
network_allow = ["*"]
snapshots = false

[compations]
claude_code = true
opencode = true
codex_cli = true
gemini_cli = true
pi = true

[[tools.rules]]
id = "repo-allow"
tool = "write_file"
action = "allow"

[[tools.rules]]
id = "repo-shell"
tool = "run_command"
command = ["true"]
action = "allow"
"#,
        )
        .unwrap();

        let config = Config::load_for(None, &root).expect("project config");
        assert_eq!(config.tools.approval, ApprovalMode::Ask);
        assert_eq!(config.tools.sandbox, SandboxMode::Auto);
        assert!(!config.tools.sandbox_network);
        assert!(!config
            .tools
            .sandbox_read_paths
            .iter()
            .any(|path| path == "/"));
        assert!(!config
            .tools
            .sandbox_writable_roots
            .iter()
            .any(|path| path == "/"));
        assert!(config.tools.external_directory.is_empty());
        assert!(config.tools.network_allow.is_empty());
        assert!(config.tools.snapshots);
        assert!(!config.compations.claude_code);
        assert!(!config.compations.opencode);
        assert!(!config.compations.codex_cli);
        assert!(!config.compations.gemini_cli);
        assert!(!config.compations.pi);
        assert!(
            config
                .tools
                .rules
                .iter()
                .all(|rule| rule.source == RuleSource::Project
                    && rule.action == PermissionAction::Ask)
        );
        for expected in [
            "approval",
            "sandbox",
            "sandbox_network",
            "sandbox_read_paths",
            "sandbox_writable_roots",
            "external_directory",
            "network_allow",
            "snapshots",
            "allow",
            "compations.claude_code",
            "compations.opencode",
            "compations.codex_cli",
            "compations.gemini_cli",
            "compations.pi",
        ] {
            assert!(
                config
                    .security_warnings
                    .iter()
                    .any(|warning| warning.contains(expected)),
                "missing warning for {expected}: {:?}",
                config.security_warnings
            );
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn compat_switches_default_off_and_parse() {
        let default = Config::default();
        assert!(!default.compations.claude_code);
        assert!(!default.compations.opencode);
        assert!(!default.compations.codex_cli);
        assert!(!default.compations.gemini_cli);
        assert!(!default.compations.pi);

        let config: Config = toml::from_str(
            r#"
[compations]
claude_code = true
opencode = true
codex_cli = true
gemini_cli = true
pi = true
"#,
        )
        .unwrap();
        assert!(
            config.compations.claude_code
                && config.compations.opencode
                && config.compations.codex_cli
                && config.compations.gemini_cli
                && config.compations.pi
        );
    }

    #[test]
    fn config_placeholders_expand_env_and_file_values() {
        let dir = config_temp_dir("placeholders");
        std::fs::write(dir.join("key.txt"), "secret-from-file\n").unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "[providers.openai]\ndefault = true\nmodel = \"gpt-4o-mini\"\napi_key = \"{file:key.txt}\"\nbase_url = \"{env:EIX_TEST_BASE_URL}\"\n",
        )
        .unwrap();
        // The base URL placeholder points at an existing variable so the test
        // does not mutate process-wide env vars.
        let previous = std::env::var("EIX_TEST_BASE_URL").ok();
        std::env::set_var("EIX_TEST_BASE_URL", "http://127.0.0.1:9/v1");
        let config = Config::load_for(Some(&path), &dir).expect("interpolated config");
        assert_eq!(
            config.default_provider().api_key.as_deref(),
            Some("secret-from-file")
        );
        assert_eq!(config.default_provider().base_url, "http://127.0.0.1:9/v1");
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
        assert!(!default.tools.sandbox_network);
        assert!(default
            .tools
            .sandbox_read_paths
            .iter()
            .any(|path| path == "~/.cargo"));
        assert!(default.tools.snapshots);
        assert_eq!(default.tools.approval, ApprovalMode::Ask);
        assert!(default.tools.auto_apply_workspace_edits);
        assert_eq!(default.tools.max_tool_rounds, 64);
        assert_eq!(default.tools.max_parallel_subagents, 4);
        assert!(default.integration.auto_commit);

        let parsed: Config = toml::from_str(
            r#"
[tools]
approval = "auto"
auto_apply_workspace_edits = false
max_tool_rounds = 96
max_parallel_subagents = 8
sandbox = "required"
sandbox_network = true
sandbox_read_paths = ["~/.cargo"]
sandbox_writable_roots = ["target"]
network_allow = ["example.com"]
snapshots = false
"#,
        )
        .expect("tools config");
        assert_eq!(parsed.tools.approval, ApprovalMode::Auto);
        assert!(!parsed.tools.auto_apply_workspace_edits);
        assert_eq!(parsed.tools.max_tool_rounds, 96);
        assert_eq!(parsed.tools.max_parallel_subagents, 8);
        assert_eq!(parsed.tools.sandbox, SandboxMode::Required);
        assert!(parsed.tools.sandbox_network);
        assert_eq!(parsed.tools.sandbox_read_paths, vec!["~/.cargo"]);
        assert_eq!(parsed.tools.sandbox_writable_roots, vec!["target"]);
        assert!(parsed.tools.network_host_allowed("https://example.com/x"));
        assert!(!parsed.tools.network_host_allowed("https://evil.test/x"));
        assert!(!parsed.tools.snapshots);
    }

    #[test]
    fn tool_round_budget_must_be_positive() {
        let mut config = Config::default();
        config.tools.max_tool_rounds = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn subagent_concurrency_levels_are_explicit() {
        assert_eq!(DEFAULT_MAX_PARALLEL_SUBAGENTS, 4);
        for value in SUBAGENT_CONCURRENCY_OPTIONS {
            assert!(subagent_concurrency_supported(value), "{value}");
        }
        for value in [0, 3, 5, 6, 12, 64] {
            assert!(!subagent_concurrency_supported(value), "{value}");
        }
    }

    #[test]
    fn configured_subagent_concurrency_must_be_a_supported_level() {
        for value in [1, 2, 4, 8, 16, 32] {
            let mut config = Config::default();
            config.tools.max_parallel_subagents = value;
            config
                .validate()
                .unwrap_or_else(|err| panic!("{value}: {err}"));
        }
        for value in [0, 3, 6, 12, 64] {
            let mut config = Config::default();
            config.tools.max_parallel_subagents = value;
            assert!(config.validate().is_err(), "{value}");
        }
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
    fn rules_parse_and_match() {
        let parsed: Config = toml::from_str(
            r#"
            [tools]
            approval = "ask"

            [[tools.rules]]
            id = "cargo-test"
            tool = "run_command"
            command = ["cargo", "test"]
            action = "allow"

            [[tools.rules]]
            id = "tests-writes"
            tool = "write_file"
            path = "tests/**"
            action = "ask"

            [[tools.rules]]
            id = "cargo-check"
            tool = "run_command"
            command_regex = "^cargo (check|clippy)\\b"
            action = "ask"
            "#,
        )
        .expect("rules config");
        assert_eq!(parsed.tools.rules.len(), 3);
        parsed.validate().expect("rules are valid");

        let command = json!({"command": "cargo test --workspace"});
        assert_eq!(
            parsed
                .tools
                .matching_rules("run_command", &command)
                .first()
                .map(|rule| rule.id.as_str()),
            Some("cargo-test")
        );
        assert!(parsed
            .tools
            .matching_rules("start_command", &command)
            .is_empty());

        let write = json!({"path": "./tests/unit/a.rs", "content": "x"});
        assert_eq!(parsed.tools.matching_rules("write_file", &write).len(), 1);
        let escape = json!({"path": "tests/../src/main.rs", "content": "x"});
        assert!(parsed
            .tools
            .matching_rules("write_file", &escape)
            .is_empty());
        let outside = json!({"path": "src/main.rs", "content": "x"});
        assert!(parsed
            .tools
            .matching_rules("write_file", &outside)
            .is_empty());
        assert!(parsed
            .tools
            .matching_rules("write_file", &json!({}))
            .is_empty());

        let check = json!({"command": "cargo clippy --workspace"});
        assert!(parsed.tools.matching_rules("run_command", &check).len() == 1);
        assert!(parsed
            .tools
            .matching_rules("run_command", &json!({"command": "cargo run"}))
            .is_empty());
    }

    #[test]
    fn webfetch_path_rules_match_urls() {
        let parsed: Config = toml::from_str(
            r#"
[[tools.rules]]
id = "docs"
tool = "webfetch"
path = "https://docs.rs/**"
action = "allow"
"#,
        )
        .expect("parse");
        assert_eq!(
            parsed
                .tools
                .matching_rules(
                    "webfetch",
                    &json!({"url": "https://docs.rs/serde/latest/serde/"})
                )
                .len(),
            1
        );
        assert!(parsed
            .tools
            .matching_rules("webfetch", &json!({"url": "https://evil.example/docs.rs"}))
            .is_empty());
    }

    #[test]
    fn rules_reject_invalid_values() {
        let invalid: Config =
            toml::from_str("[[tools.rules]]\ntool = \"run_command\"\ncommand_regex = \"(\"\n")
                .expect("parse first");
        assert!(invalid.validate().is_err());

        let empty: Config = toml::from_str("[[tools.rules]]\ntool = \"\"\n").expect("parse first");
        assert!(empty.validate().is_err());

        let prefix: Config =
            toml::from_str("[[tools.rules]]\ntool = \"run_command\"\ncommand = []\n")
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
    fn wildcard_rule_matches_any_tool() {
        let rule = PermissionRule {
            tool: "*".to_string(),
            ..PermissionRule::default()
        };
        assert!(rule.matches("write_file", &json!({"path": "a"})));
        assert!(rule.matches("run_command", &json!({"command": "ls"})));
        assert_eq!(rule.specificity(), 0);
    }
}
