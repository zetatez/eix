//! Sessions are persistent daemon-side entities. Frontends attach to them and
//! render their event log; they never own session state.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::now_ms;

pub type SessionId = String;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Idle,
    Running,
    AwaitingApproval,
    AwaitingInput,
    Error,
    Paused,
}

impl SessionStatus {
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Idle => "○",
            Self::Running => "●",
            Self::AwaitingApproval => "⚠",
            Self::AwaitingInput => "◐",
            Self::Error => "✗",
            Self::Paused => "◌",
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::AwaitingApproval => "awaiting_approval",
            Self::AwaitingInput => "awaiting_input",
            Self::Error => "error",
            Self::Paused => "paused",
        }
    }

    pub fn needs_attention(self) -> bool {
        matches!(
            self,
            Self::AwaitingApproval | Self::AwaitingInput | Self::Error
        )
    }

    pub fn is_busy(self) -> bool {
        matches!(
            self,
            Self::Running | Self::AwaitingApproval | Self::AwaitingInput
        )
    }
}

impl std::str::FromStr for SessionStatus {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "idle" => Ok(Self::Idle),
            "running" => Ok(Self::Running),
            "awaiting_approval" => Ok(Self::AwaitingApproval),
            "awaiting_input" => Ok(Self::AwaitingInput),
            "error" => Ok(Self::Error),
            "paused" => Ok(Self::Paused),
            _ => Err(()),
        }
    }
}

/// Where a session came from. Lets the UI mark forks and sub-agents without
/// guessing from the name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionOrigin {
    /// Created directly by the user.
    #[default]
    User,
    /// Branch copied from another session (`session.fork` / `session.rewind`).
    Fork,
    /// Child session started by the `spawn_agent` tool.
    Agent,
}

impl SessionOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Fork => "fork",
            Self::Agent => "agent",
        }
    }
}

impl std::str::FromStr for SessionOrigin {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "user" => Ok(Self::User),
            "fork" => Ok(Self::Fork),
            "agent" => Ok(Self::Agent),
            _ => Err(()),
        }
    }
}

/// The execution mode of a session.
///
/// The mode is the single source of truth for what a session may do. Tools,
/// risk analysis and explicit rules can tighten a decision but never widen the
/// mode's capability ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    /// Hard read-only: no writes and only read-only shell commands.
    Plan,
    /// Workspace-write with static risk analysis. Risky calls ask by default.
    #[default]
    Build,
    /// Workspace-write. Unruled default `ask` results go through the
    /// intent-aware classifier; only a confident, in-scope `allow` runs without
    /// prompting. It does not grant Bypass-level external path writes.
    Auto,
    /// Full/bypass behavior: unruled asks are allowed, external writes are
    /// permitted (except secret-path and catastrophic hard denies), and build
    /// commands fall back to unsandboxed execution when no sandbox is
    /// available.
    Bypass,
}

impl SessionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Build => "build",
            Self::Auto => "auto",
            Self::Bypass => "bypass",
        }
    }

    /// Display label used by the TUI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Plan => "Plan",
            Self::Build => "Build",
            Self::Auto => "Auto",
            Self::Bypass => "Bypass",
        }
    }

    /// Hard read-only mode.
    pub fn is_read_only(self) -> bool {
        matches!(self, Self::Plan)
    }

    /// True for modes that may mutate the workspace.
    pub fn can_write(self) -> bool {
        !self.is_read_only()
    }

    /// True when unruled default `ask` calls run without prompting. `Auto`
    /// deliberately does not qualify: it sends those calls through the
    /// classifier instead of blanket-allowing them.
    pub fn allows_unruled_asks(self) -> bool {
        matches!(self, Self::Bypass)
    }

    /// True when unruled default `ask` calls should be sent to the classifier.
    pub fn uses_classifier(self) -> bool {
        matches!(self, Self::Auto)
    }

    /// True when file tools may leave the workspace without an extra guard.
    pub fn allows_external_writes(self) -> bool {
        matches!(self, Self::Bypass)
    }

    /// True when a build command may run without a sandbox.
    pub fn allows_unsandboxed_exec(self) -> bool {
        matches!(self, Self::Bypass)
    }

    /// True for modes that automatically apply workspace edits.
    pub fn applies_workspace_edits(self) -> bool {
        matches!(self, Self::Build | Self::Auto)
    }
}

impl std::str::FromStr for SessionMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "plan" => Ok(Self::Plan),
            "build" => Ok(Self::Build),
            "auto" => Ok(Self::Auto),
            "bypass" => Ok(Self::Bypass),
            _ => Err(()),
        }
    }
}

/// Integration state for a session worktree. Kept separate from the turn
/// status so a failed merge never makes the session look busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrationStatus {
    /// The session is still being worked on.
    #[default]
    Pending,
    /// Explicitly ready, or auto-ready after idling.
    Ready,
    /// Pulling the base branch and updating remote refs.
    Syncing,
    /// Rebasing the session branch onto the base branch.
    Rebasing,
    /// An agent turn is resolving conflicts or a failing test.
    Repairing,
    /// Running the project test command in the rebased worktree.
    Testing,
    /// Fast-forwarding the base branch.
    Merging,
    /// The branch is part of the base branch.
    Merged,
    /// A transient failure; the queue will try again.
    Retrying,
}

impl IntegrationStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Ready => "ready",
            Self::Syncing => "syncing",
            Self::Rebasing => "rebasing",
            Self::Repairing => "repairing",
            Self::Testing => "testing",
            Self::Merging => "merging",
            Self::Merged => "merged",
            Self::Retrying => "retrying",
        }
    }

    pub fn symbol(self) -> &'static str {
        match self {
            Self::Pending => "·",
            Self::Ready => "→",
            Self::Syncing => "⇣",
            Self::Rebasing => "↻",
            Self::Repairing => "✎",
            Self::Testing => "✓",
            Self::Merging => "⇢",
            Self::Merged => "●",
            Self::Retrying => "…",
        }
    }

    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Syncing | Self::Rebasing | Self::Repairing | Self::Testing | Self::Merging
        )
    }
}

impl std::str::FromStr for IntegrationStatus {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "ready" => Ok(Self::Ready),
            "syncing" => Ok(Self::Syncing),
            "rebasing" => Ok(Self::Rebasing),
            "repairing" => Ok(Self::Repairing),
            "testing" => Ok(Self::Testing),
            "merging" => Ok(Self::Merging),
            "merged" => Ok(Self::Merged),
            "retrying" => Ok(Self::Retrying),
            _ => Err(()),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntegrationState {
    pub session_id: SessionId,
    pub status: IntegrationStatus,
    #[serde(default)]
    pub stage: Option<String>,
    #[serde(default)]
    pub base_branch: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub log_path: Option<PathBuf>,
    #[serde(default)]
    pub ready_at: Option<i64>,
    #[serde(default)]
    pub next_retry_at: Option<i64>,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default)]
    pub merged_commit: Option<String>,
    #[serde(default)]
    pub worktree_disabled: bool,
    pub updated_at: i64,
}

impl IntegrationState {
    pub fn new(session_id: impl Into<SessionId>) -> Self {
        Self {
            session_id: session_id.into(),
            status: IntegrationStatus::Pending,
            stage: None,
            base_branch: None,
            error: None,
            log_path: None,
            ready_at: None,
            next_retry_at: None,
            attempts: 0,
            merged_commit: None,
            worktree_disabled: false,
            updated_at: now_ms(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub name: String,
    pub cwd: PathBuf,
    pub status: SessionStatus,
    /// Model pinned to this session. `None` means "use the daemon default".
    #[serde(default)]
    pub model: Option<String>,
    /// Session this one was forked from (or the parent of a sub-agent).
    #[serde(default)]
    pub parent_id: Option<String>,
    /// Source sequence the fork copied up to (exclusive). `None` for a root.
    #[serde(default)]
    pub forked_from_seq: Option<u64>,
    /// Git worktree bound to this session, when one was created for it.
    #[serde(default)]
    pub worktree: Option<PathBuf>,
    /// How the session came to exist (`user` / `fork` / `agent`).
    #[serde(default)]
    pub origin: SessionOrigin,
    /// Session-level execution mode. This is the single capability ceiling.
    #[serde(default)]
    pub mode: SessionMode,
    /// Custom primary agent name (Markdown-defined). `None` uses the default
    /// prompt. Agents no longer carry a mode or permission of their own.
    #[serde(default)]
    pub agent: Option<String>,
    /// Hidden from the default session list, events kept.
    #[serde(default)]
    pub archived: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Session {
    pub fn new(id: impl Into<SessionId>, name: impl Into<String>, cwd: impl Into<PathBuf>) -> Self {
        let now = now_ms();
        Self {
            id: id.into(),
            name: name.into(),
            cwd: cwd.into(),
            status: SessionStatus::Idle,
            model: None,
            parent_id: None,
            forked_from_seq: None,
            worktree: None,
            origin: SessionOrigin::User,
            mode: SessionMode::default(),
            agent: None,
            archived: false,
            created_at: now,
            updated_at: now,
        }
    }

    /// True when the session runs in its own git worktree.
    pub fn has_worktree(&self) -> bool {
        self.worktree.is_some()
    }

    /// Whether this session is bound to the given working directory. Paths are
    /// canonicalized when possible so symlinked launch directories still match.
    pub fn cwd_matches(&self, cwd: &Path) -> bool {
        fn normalize(path: &Path) -> PathBuf {
            path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
        }
        normalize(&self.cwd) == normalize(cwd)
    }

    pub fn is_agent(&self) -> bool {
        self.origin == SessionOrigin::Agent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attention_states() {
        assert!(SessionStatus::AwaitingApproval.needs_attention());
        assert!(SessionStatus::AwaitingInput.needs_attention());
        assert!(!SessionStatus::Running.needs_attention());
        assert!(!SessionStatus::Idle.needs_attention());
    }

    #[test]
    fn symbols_are_distinct() {
        let all = [
            SessionStatus::Idle,
            SessionStatus::Running,
            SessionStatus::AwaitingApproval,
            SessionStatus::AwaitingInput,
            SessionStatus::Error,
            SessionStatus::Paused,
        ];
        let mut symbols: Vec<_> = all.iter().map(|s| s.symbol()).collect();
        symbols.sort();
        symbols.dedup();
        assert_eq!(symbols.len(), all.len());
    }

    #[test]
    fn status_round_trips_through_str() {
        let all = [
            SessionStatus::Idle,
            SessionStatus::Running,
            SessionStatus::AwaitingApproval,
            SessionStatus::AwaitingInput,
            SessionStatus::Error,
            SessionStatus::Paused,
        ];
        for status in all {
            assert_eq!(status.as_str().parse::<SessionStatus>(), Ok(status));
        }
    }

    #[test]
    fn session_modes_round_trip_and_define_capabilities() {
        let all = [
            SessionMode::Plan,
            SessionMode::Build,
            SessionMode::Auto,
            SessionMode::Bypass,
        ];
        for mode in all {
            assert_eq!(mode.as_str().parse::<SessionMode>(), Ok(mode));
        }
        assert!("workspace-write".parse::<SessionMode>().is_err());
        assert!("nonsense".parse::<SessionMode>().is_err());
        assert!(SessionMode::Plan.is_read_only());
        assert!(!SessionMode::Plan.can_write());
        assert!(SessionMode::Build.can_write());
        assert!(!SessionMode::Build.allows_unruled_asks());
        assert!(!SessionMode::Auto.allows_unruled_asks());
        assert!(SessionMode::Auto.uses_classifier());
        assert!(!SessionMode::Plan.uses_classifier());
        assert!(!SessionMode::Build.uses_classifier());
        assert!(SessionMode::Bypass.allows_unruled_asks());
        assert!(!SessionMode::Bypass.uses_classifier());
        assert!(!SessionMode::Auto.allows_external_writes());
        assert!(SessionMode::Bypass.allows_external_writes());
        assert!(SessionMode::Build.applies_workspace_edits());
        assert!(SessionMode::Auto.applies_workspace_edits());
        assert!(!SessionMode::Plan.applies_workspace_edits());
    }

    #[test]
    fn session_mode_serde_uses_plain_mode_names() {
        assert_eq!(
            serde_json::to_string(&SessionMode::Bypass).unwrap(),
            "\"bypass\""
        );
        assert_eq!(
            serde_json::from_str::<SessionMode>("\"plan\"").unwrap(),
            SessionMode::Plan
        );
        assert_eq!(SessionMode::default(), SessionMode::Build);
    }

    #[test]
    fn session_matches_its_working_directory() {
        let cwd = std::env::current_dir().expect("cwd");
        let session = Session::new("s1", "here", cwd.clone());
        assert!(session.cwd_matches(&cwd));
        assert!(!session.cwd_matches(&std::env::temp_dir().join("eix-other-cwd")));
    }
}
