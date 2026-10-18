//! Wire protocol between `eixd` and frontends.
//!
//! Transport is newline-delimited JSON over a Unix socket. A client sends
//! `Request` objects with a monotonic `id`; the daemon answers with `Response`
//! objects and pushes `Notification`s at any time. Notifications carry the
//! structured events described in `DESIGN.md`, never pre-rendered text.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Event, Session, SessionId};

/// Bumped whenever a change breaks older clients.
pub const PROTOCOL_VERSION: u32 = 1;

/// Where a frontend connects. The Unix socket is the default and is protected
/// by filesystem permissions; TCP is opt-in and requires a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Endpoint {
    Unix(PathBuf),
    Tcp { addr: String, token: Option<String> },
}

impl Endpoint {
    pub fn new(socket: impl Into<PathBuf>, tcp: Option<String>, token: Option<String>) -> Self {
        match tcp
            .map(|addr| addr.trim().to_string())
            .filter(|addr| !addr.is_empty())
        {
            Some(addr) => Self::Tcp { addr, token },
            None => Self::Unix(socket.into()),
        }
    }

    pub fn is_tcp(&self) -> bool {
        matches!(self, Self::Tcp { .. })
    }

    pub fn token(&self) -> Option<&str> {
        match self {
            Self::Tcp { token, .. } => token.as_deref(),
            Self::Unix(_) => None,
        }
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unix(path) => write!(f, "{}", path.display()),
            Self::Tcp { addr, .. } => write!(f, "tcp://{addr}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

impl Request {
    pub fn new(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self {
            id,
            method: method.into(),
            params,
        }
    }

    pub fn parse(line: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(line)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorObject {
    pub code: String,
    pub message: String,
}

impl ErrorObject {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub id: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorObject>,
}

impl Response {
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
            error: Some(ErrorObject::new(code, message)),
        }
    }

    pub fn is_error(&self) -> bool {
        self.error.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub method: String,
    pub params: Value,
}

impl Notification {
    pub fn new<T: Serialize>(method: impl Into<String>, params: T) -> Self {
        Self {
            method: method.into(),
            params: serde_json::to_value(params).unwrap_or(Value::Null),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Response(Response),
    Notification(Notification),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CreateSessionParams {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub model: Option<String>,
    /// Create `.worktrees/<name>` and run the session there. Requires `cwd`
    /// (or the daemon's cwd) to be inside a git work tree.
    #[serde(default)]
    pub worktree: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkParams {
    pub session_id: SessionId,
    /// Copy events with `seq < at_seq`. `None` copies the whole history.
    #[serde(default)]
    pub at_seq: Option<u64>,
    /// Defaults to `<root name>~N`.
    #[serde(default)]
    pub name: Option<String>,
    /// `None` = auto: give the fork its own worktree when the source lives in a
    /// git work tree. `Some(false)` shares the source workspace.
    #[serde(default)]
    pub worktree: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindParams {
    pub session_id: SessionId,
    /// Restore the workspace to just before this sequence (exclusive). `None`
    /// means "now", which only forks and leaves the workspace alone.
    #[serde(default)]
    pub at_seq: Option<u64>,
    #[serde(default)]
    pub name: Option<String>,
}

/// Result of `session.rewind`. `restored` is false when at least one snapshot
/// could not be applied; `warnings` explains why, including files whose bytes
/// were never captured.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindResult {
    pub session: Session,
    pub restored: bool,
    pub snapshots: usize,
    pub files_restored: usize,
    pub files_removed: usize,
    pub files_missing: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeRemoveParams {
    pub session_id: SessionId,
    /// Pass `--force` to `git worktree remove` (discards uncommitted files).
    #[serde(default)]
    pub force: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeRemoveResult {
    pub removed: bool,
    pub message: String,
}

/// First request on a TCP connection. Unix sockets are already protected by
/// filesystem permissions and skip this step.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthParams {
    pub token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthResult {
    pub authenticated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRef {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitParams {
    pub session_id: SessionId,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RenameParams {
    pub session_id: SessionId,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Approve,
    Deny,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRespondParams {
    pub session_id: SessionId,
    pub approval_id: String,
    pub decision: ApprovalDecision,
    #[serde(default)]
    pub reason: Option<String>,
    /// Indices of hunks to apply when approving a write. Omitted or empty
    /// means the whole change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<usize>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResult {
    pub accepted: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApprovalListParams {
    /// Restrict to one session; `None` lists every session with a pending item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

/// One pending approval, either a blocking tool call or a staged write.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalInfo {
    pub session_id: SessionId,
    pub session_name: String,
    pub approval_id: String,
    /// `tool` for a blocking tool call, `patch` for a staged write.
    pub kind: String,
    pub tool: String,
    pub summary: String,
    #[serde(default)]
    pub created_ms: i64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApprovalListResult {
    pub approvals: Vec<ApprovalInfo>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubscribeParams {
    /// Sessions this connection wants events for. `None` (or an omitted
    /// field) means every session; an empty list silences broadcasts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_ids: Option<Vec<SessionId>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubscribeResult {
    /// `None` = all sessions; otherwise the ids the connection subscribed to.
    #[serde(default)]
    pub session_ids: Option<Vec<SessionId>>,
}

/// `draft.get` reads the stored prompt; `draft.set` writes it. A `None` or
/// empty `text` clears the draft.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftParams {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DraftResult {
    pub session_id: SessionId,
    pub text: String,
    #[serde(default)]
    pub updated_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchApplyParams {
    pub session_id: SessionId,
    pub patch_id: String,
    /// Indices of hunks to apply. Omitted or empty means the whole change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<usize>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchRejectParams {
    pub session_id: SessionId,
    pub patch_id: String,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchResult {
    pub accepted: bool,
    #[serde(default)]
    pub applied: bool,
    #[serde(default)]
    pub conflict: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchGetParams {
    pub session_id: SessionId,
    pub patch_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchGetResult {
    pub session_id: SessionId,
    pub patch_id: String,
    /// Workspace-relative path shown in the UI.
    pub path: String,
    /// The proposed file content, for editing before approval.
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PatchReviseParams {
    pub session_id: SessionId,
    pub patch_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsParams {
    pub session_id: SessionId,
    #[serde(default)]
    pub after_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextLedgerParams {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextDropParams {
    pub session_id: SessionId,
    pub entry: String,
    /// `None` means drop; `Some(false)` restores the entry.
    #[serde(default)]
    pub dropped: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextPinParams {
    pub session_id: SessionId,
    pub entry: String,
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCompactParams {
    pub session_id: SessionId,
    /// Inclusive range of source event sequence numbers.
    pub from_seq: u64,
    pub to_seq: u64,
    /// Replacement text. A mechanical note is generated when omitted.
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessListParams {
    /// `None` lists processes across every session.
    #[serde(default)]
    pub session_id: Option<SessionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessOutputParams {
    pub process_id: String,
    #[serde(default)]
    pub max_bytes: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessKillParams {
    pub process_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionList {
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResult {
    pub session: Session,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachResult {
    pub session: Session,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitResult {
    pub session: Session,
    pub turn_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelResult {
    pub cancelled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsResult {
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloResult {
    pub protocol: u32,
    pub version: String,
    pub pid: u32,
    pub model: String,
    pub base_url: String,
    pub sessions: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_round_trips() {
        let req = Request::new(7, "session.create", json!({"name": "auth"}));
        let line = serde_json::to_string(&req).unwrap();
        let back = Request::parse(&line).unwrap();
        assert_eq!(back.id, 7);
        assert_eq!(back.method, "session.create");
        assert_eq!(back.params["name"], "auth");
    }

    #[test]
    fn response_and_notification_are_distinguishable() {
        let ok = ServerMessage::Response(Response::ok(1, json!({"x": 1})));
        let line = serde_json::to_string(&ok).unwrap();
        let back: ServerMessage = serde_json::from_str(&line).unwrap();
        assert!(matches!(back, ServerMessage::Response(r) if r.result.is_some()));

        let note =
            ServerMessage::Notification(Notification::new("session.updated", json!({"id": "s"})));
        let line = serde_json::to_string(&note).unwrap();
        let back: ServerMessage = serde_json::from_str(&line).unwrap();
        assert!(matches!(back, ServerMessage::Notification(n) if n.method == "session.updated"));
    }

    #[test]
    fn defaults_are_lenient() {
        let req: Request = serde_json::from_str(r#"{"id":1,"method":"ping"}"#).unwrap();
        assert_eq!(req.params, Value::Null);
        let create: CreateSessionParams = serde_json::from_str("{}").unwrap();
        assert!(create.name.is_none());
    }
}
