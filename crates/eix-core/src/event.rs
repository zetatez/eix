//! The append-only event log is the source of truth for a session's history.
//! Everything a frontend shows (transcript, approvals, patches, status) is a
//! projection of these events.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{SessionId, SessionStatus, ToolCall, Usage};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    UserMessage,
    AssistantMessage,
    ToolCall,
    ToolResult,
    PatchProposed,
    PatchApplied,
    PatchRejected,
    PatchConflict,
    ApprovalRequired,
    /// Pre-execution snapshot of what a mutating tool is about to touch.
    Snapshot,
    StatusChanged,
    ContextCompacted,
    Error,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UserMessage => "user_message",
            Self::AssistantMessage => "assistant_message",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
            Self::PatchProposed => "patch_proposed",
            Self::PatchApplied => "patch_applied",
            Self::PatchRejected => "patch_rejected",
            Self::PatchConflict => "patch_conflict",
            Self::ApprovalRequired => "approval_required",
            Self::Snapshot => "snapshot",
            Self::StatusChanged => "status_changed",
            Self::ContextCompacted => "context_compacted",
            Self::Error => "error",
        }
    }

    /// Kinds that become provider chat messages when rebuilding context.
    pub fn is_message(self) -> bool {
        matches!(self, Self::UserMessage | Self::AssistantMessage)
    }
}

impl std::str::FromStr for EventKind {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "user_message" => Self::UserMessage,
            "assistant_message" => Self::AssistantMessage,
            "tool_call" => Self::ToolCall,
            "tool_result" => Self::ToolResult,
            "patch_proposed" => Self::PatchProposed,
            "patch_applied" => Self::PatchApplied,
            "patch_rejected" => Self::PatchRejected,
            "patch_conflict" => Self::PatchConflict,
            "approval_required" => Self::ApprovalRequired,
            "snapshot" => Self::Snapshot,
            "status_changed" => Self::StatusChanged,
            "context_compacted" => Self::ContextCompacted,
            "error" => Self::Error,
            _ => return Err(()),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub session_id: SessionId,
    pub seq: u64,
    pub ts: i64,
    pub kind: EventKind,
    pub payload: Value,
}

impl Event {
    pub fn new(
        session_id: impl Into<SessionId>,
        seq: u64,
        kind: EventKind,
        payload: Value,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            seq,
            ts: crate::now_ms(),
            kind,
            payload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserMessagePayload {
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessagePayload {
    pub text: String,
    pub model: String,
    /// True when the turn was cancelled and only a partial answer was kept.
    #[serde(default)]
    pub cancelled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// Tool calls requested alongside this assistant message. The matching
    /// `tool_call` / `tool_result` events carry the same call ids.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallPayload {
    pub call_id: String,
    pub tool: String,
    pub arguments: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultPayload {
    pub call_id: String,
    pub tool: String,
    pub ok: bool,
    /// The user denied the call; `output` explains why.
    #[serde(default)]
    pub denied: bool,
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// One selectable hunk of a proposed write, including its rendered lines.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HunkInfo {
    pub index: usize,
    pub header: String,
    pub lines: Vec<String>,
}

/// A write that has been staged in the session's shadow workspace. The bytes
/// live under `<data>/staging/<session>/<patch_id>.json`; this event carries the
/// reviewable diff and hunk preview.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchProposedPayload {
    pub patch_id: String,
    pub call_id: String,
    pub tool: String,
    /// Workspace-relative path, for display.
    pub path: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hunks: Vec<HunkInfo>,
}

/// Outcome of applying, rejecting or conflicting a staged patch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchResultPayload {
    pub patch_id: String,
    pub call_id: String,
    pub path: String,
    /// True when the change landed on disk.
    #[serde(default)]
    pub applied: bool,
    /// True when the file changed after the patch was staged.
    #[serde(default)]
    pub conflict: bool,
    /// Hunk indices that were applied (empty means the whole change).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hunks: Vec<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRequiredPayload {
    pub approval_id: String,
    pub call_id: String,
    pub tool: String,
    pub summary: String,
    pub arguments: Value,
    /// Unified diff preview for file-writing tools. Absent for tools that
    /// cannot be previewed, such as shell commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// Per-hunk preview for file-writing tools, used for hunk-level approval.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hunks: Vec<HunkInfo>,
}

/// Metadata for a pre-execution snapshot. The bytes live in
/// `<data>/snapshots/<session>/<snapshot_id>/`; this event only records that
/// the copy happened (or why it did not).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotPayload {
    pub snapshot_id: String,
    pub call_id: String,
    pub tool: String,
    /// `file` when a single write target was copied, `tree` for a workspace
    /// snapshot taken before a shell command.
    pub scope: String,
    pub ok: bool,
    #[serde(default)]
    pub files: usize,
    #[serde(default)]
    pub bytes: u64,
    /// Files that could not be copied (too large, over budget, symlinks, ...).
    #[serde(default)]
    pub skipped: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusChangedPayload {
    pub from: SessionStatus,
    pub to: SessionStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub message: String,
}

/// Build a `serde_json::Value` for an event payload.
pub fn payload<T: Serialize>(value: &T) -> Value {
    serde_json::to_value(value).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_round_trips() {
        let all = [
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::ToolCall,
            EventKind::ToolResult,
            EventKind::PatchProposed,
            EventKind::PatchApplied,
            EventKind::PatchRejected,
            EventKind::PatchConflict,
            EventKind::ApprovalRequired,
            EventKind::Snapshot,
            EventKind::StatusChanged,
            EventKind::ContextCompacted,
            EventKind::Error,
        ];
        for kind in all {
            assert_eq!(kind.as_str().parse::<EventKind>(), Ok(kind));
        }
        assert!("nope".parse::<EventKind>().is_err());
    }

    #[test]
    fn message_payloads() {
        let user = payload(&UserMessagePayload { text: "hi".into() });
        assert_eq!(user["text"], "hi");

        let assistant = payload(&AssistantMessagePayload {
            text: "yo".into(),
            model: "m".into(),
            cancelled: false,
            usage: None,
            tool_calls: Vec::new(),
        });
        assert_eq!(assistant["text"], "yo");
        assert!(assistant.get("usage").is_none());
        assert!(assistant.get("tool_calls").is_none());

        let call = payload(&ToolCallPayload {
            call_id: "call_1".into(),
            tool: "read_file".into(),
            arguments: serde_json::json!({"path": "a.rs"}),
        });
        assert_eq!(call["tool"], "read_file");

        let result = payload(&ToolResultPayload {
            call_id: "call_1".into(),
            tool: "read_file".into(),
            ok: true,
            denied: false,
            output: "hi".into(),
            duration_ms: None,
        });
        assert_eq!(result["ok"], true);
        assert!(result.get("duration_ms").is_none());
    }
}
