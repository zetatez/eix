//! Sessions are persistent daemon-side entities. Frontends attach to them and
//! render their event log; they never own session state.

use std::path::PathBuf;

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
            created_at: now,
            updated_at: now,
        }
    }

    /// True when the session runs in its own git worktree.
    pub fn has_worktree(&self) -> bool {
        self.worktree.is_some()
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
}
