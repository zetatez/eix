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

/// What a session is allowed to do to the workspace.
///
/// This is the per-session capability ceiling. Risk rules and the approval
/// mode decide what happens below it; none of them can widen this level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PermissionLevel {
    /// Mutating tools are refused; safe reads and read-only shell commands are
    /// allowed.
    #[serde(rename = "read-only")]
    ReadOnly,
    /// The default: risk rules and `[tools] approval` decide whether a call is
    /// allowed, sandboxed or asked about; paths stay inside the workspace.
    #[default]
    #[serde(rename = "workspace-write")]
    WorkspaceWrite,
    /// Removes the capability ceiling only. Hard denials, explicit rules and
    /// approval mode still apply; the workspace path check remains.
    #[serde(rename = "full")]
    Full,
}

impl PermissionLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::Full => "full",
        }
    }

    /// True for levels that may mutate the workspace.
    pub fn can_write(self) -> bool {
        !matches!(self, Self::ReadOnly)
    }

    /// The stricter of two levels. An agent's read-only restriction always
    /// wins over a more permissive session; it never widens one.
    pub fn restrict(self, other: Self) -> Self {
        match (self, other) {
            (Self::ReadOnly, _) | (_, Self::ReadOnly) => Self::ReadOnly,
            (Self::WorkspaceWrite, _) | (_, Self::WorkspaceWrite) => Self::WorkspaceWrite,
            _ => Self::Full,
        }
    }
}

impl std::str::FromStr for PermissionLevel {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "read-only" | "readonly" | "read_only" => Ok(Self::ReadOnly),
            "workspace-write" | "workspace_write" | "write" | "workspace" => {
                Ok(Self::WorkspaceWrite)
            }
            "full" | "auto" => Ok(Self::Full),
            _ => Err(()),
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
    /// Per-session tool permission level.
    #[serde(default)]
    pub permission: PermissionLevel,
    /// Primary agent name (built-in or Markdown-defined). `None` means the
    /// default `build` behavior.
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
            permission: PermissionLevel::default(),
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
    fn permission_levels_round_trip_and_alias() {
        let all = [
            PermissionLevel::ReadOnly,
            PermissionLevel::WorkspaceWrite,
            PermissionLevel::Full,
        ];
        for level in all {
            assert_eq!(level.as_str().parse::<PermissionLevel>(), Ok(level));
        }
        assert_eq!(
            "readonly".parse::<PermissionLevel>(),
            Ok(PermissionLevel::ReadOnly)
        );
        assert_eq!(
            "workspace-write".parse::<PermissionLevel>(),
            Ok(PermissionLevel::WorkspaceWrite)
        );
        assert!("nonsense".parse::<PermissionLevel>().is_err());
        assert!(!PermissionLevel::ReadOnly.can_write());
        assert!(PermissionLevel::Full.can_write());
    }

    #[test]
    fn session_matches_its_working_directory() {
        let cwd = std::env::current_dir().expect("cwd");
        let session = Session::new("s1", "here", cwd.clone());
        assert!(session.cwd_matches(&cwd));
        assert!(!session.cwd_matches(&std::env::temp_dir().join("eix-other-cwd")));
    }
}
