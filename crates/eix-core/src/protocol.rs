//! Wire protocol between `eixd` and frontends.
//!
//! Transport is newline-delimited JSON over a Unix socket. A client sends
//! `Request` objects with a monotonic `id`; the daemon answers with `Response`
//! objects and pushes `Notification`s at any time. Notifications carry the
//! structured events described in `DESIGN.md`, never pre-rendered text.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Event, IntegrationState, PermissionLevel, Session, SessionId};

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
    /// Per-session tool permission level. Defaults to `workspace-write`.
    #[serde(default)]
    pub permission: Option<PermissionLevel>,
    /// Primary agent name; defaults to `build`.
    #[serde(default)]
    pub agent: Option<String>,
}

/// `session.permission`: change a session's tool permission level.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PermissionParams {
    pub session_id: SessionId,
    pub permission: PermissionLevel,
    /// Hold the new level in memory only; it is not persisted and reverts on
    /// daemon restart. Recorded as a `permission` event either way.
    #[serde(default)]
    pub temporary: bool,
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

/// `session.attach`: replay a session. When `tail` is set only the newest
/// events are returned, so very long sessions open quickly; older pages can be
/// fetched with `session.events` and `before_seq`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachParams {
    pub session_id: SessionId,
    #[serde(default)]
    pub tail: Option<usize>,
}

/// `session.ready`: request integration for a session worktree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionReadyParams {
    pub session_id: SessionId,
    /// Commit dirty worktree changes with an eix-generated message before
    /// queuing. Defaults to `integration.auto_commit`.
    #[serde(default)]
    pub auto_commit: Option<bool>,
}

/// `integration.list`: current queue state, optionally for one session.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntegrationListParams {
    #[serde(default)]
    pub session_id: Option<SessionId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationListResult {
    pub integrations: Vec<IntegrationState>,
}

/// `integration.continue`: clear failure counters and requeue a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IntegrationContinueParams {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitParams {
    pub session_id: SessionId,
    pub text: String,
    /// Paths to images attached to this prompt (relative to the session cwd or
    /// inside an allowed `external_directory`).
    #[serde(default)]
    pub images: Vec<String>,
}

/// `session.cd`: move a session to another working directory.
/// `session.model`: pin a session to one of the models from `model.list`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionModelParams {
    pub session_id: SessionId,
    /// Empty or omitted restores the daemon default.
    #[serde(default)]
    pub model: Option<String>,
}

/// `session.agent`: choose the session's primary agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAgentParams {
    pub session_id: SessionId,
    /// Empty or omitted restores the built-in default.
    #[serde(default)]
    pub agent: Option<String>,
}

/// `config.subagents`: change the runtime concurrency limit for `spawn_agent`.
/// Config reload or a daemon restart restores `[tools] max_parallel_subagents`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentConcurrencyParams {
    pub max_parallel_subagents: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubagentConcurrencyResult {
    pub max_parallel_subagents: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelListResult {
    pub models: Vec<crate::ModelInfo>,
    /// Providers whose automatic catalog fetch failed. Configured models are
    /// still present in `models`.
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentListResult {
    pub agents: Vec<crate::AgentSpec>,
}

/// `mcp.list`: configured MCP servers and how many tools each exposes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpListResult {
    pub servers: Vec<McpServerInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerInfo {
    pub name: String,
    pub enabled: bool,
    pub command: String,
    pub tools: usize,
    #[serde(default)]
    pub resources: usize,
    #[serde(default)]
    pub prompts: usize,
    #[serde(default)]
    pub error: Option<String>,
}

/// `stats`: a compact daemon health/capacity snapshot for dashboards and the
/// HTTP `GET /stats` endpoint.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StatsResult {
    pub version: String,
    pub uptime_secs: u64,
    pub sessions: usize,
    pub archived_sessions: usize,
    pub running_sessions: usize,
    pub events: usize,
    pub db_bytes: u64,
    pub blob_count: usize,
    pub blob_bytes: u64,
    pub skills: usize,
    pub mcp_servers: usize,
    pub default_model: String,
}

/// One diagnostic row from `lsp.workspace`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspDiagnosticInfo {
    /// Absolute path.
    pub path: PathBuf,
    pub line: u32,
    pub character: u32,
    pub severity: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LspWorkspaceResult {
    pub diagnostics: Vec<LspDiagnosticInfo>,
    /// Files that were opened for diagnostics.
    pub files: usize,
    /// Name of the language servers that ran.
    pub servers: Vec<String>,
    /// Files skipped because `max_files` was reached.
    pub skipped: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LspWorkspaceParams {
    pub session_id: SessionId,
    #[serde(default)]
    pub max_files: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionCdParams {
    pub session_id: SessionId,
    pub path: String,
}

/// `context.clear`: make the model start from a clean context. Earlier events
/// stay in the log (and the transcript); they are simply not sent again.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextClearParams {
    pub session_id: SessionId,
}

/// `session.btw`: a one-off side question, answered in a throwaway context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtwParams {
    pub session_id: SessionId,
    pub question: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtwResult {
    pub btw_id: String,
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
    /// Approve this call and remember a matching rule. `scope` selects where
    /// the rule is stored.
    Always,
}

/// How long an "Always" choice lasts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalScope {
    /// Do not remember anything.
    Once,
    /// Keep the rule in memory for this session (default).
    #[default]
    Session,
    /// Request project scope. Project config may only tighten permissions, so
    /// an `allow` rule is kept for the current session instead of persisted.
    Project,
    /// Append a rule to the user's global config.
    User,
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
    /// Where an `Always` decision is remembered. Ignored for other decisions.
    #[serde(default)]
    pub scope: ApprovalScope,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResult {
    pub accepted: bool,
}

/// Answer the model's `ask_user` question. `input_id` may be empty to target
/// the session's current question.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputRespondParams {
    pub session_id: SessionId,
    #[serde(default)]
    pub input_id: String,
    /// Zero-based index into the question's option list.
    pub option_index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputResult {
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
    /// Return events older than this sequence. Used for paging history.
    #[serde(default)]
    pub before_seq: Option<u64>,
    /// Maximum number of events to return for a paged query.
    #[serde(default)]
    pub limit: Option<usize>,
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
    /// Inclusive range of source event sequence numbers. Both `None` means
    /// "every message currently in the context".
    #[serde(default)]
    pub from_seq: Option<u64>,
    #[serde(default)]
    pub to_seq: Option<u64>,
    /// Replacement text. When omitted, a mechanical note is generated (or the
    /// model writes one when `generate` is set).
    #[serde(default)]
    pub summary: Option<String>,
    /// Ask the provider for a real summary of the range instead of the
    /// mechanical placeholder. Used by the `/compact` command.
    #[serde(default)]
    pub generate: bool,
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
pub struct SessionArchiveParams {
    pub session_id: SessionId,
    pub archived: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionExportResult {
    /// A self-contained JSON document accepted by `session.import`.
    pub json: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionImportParams {
    pub json: String,
    /// Overrides the exported name.
    #[serde(default)]
    pub name: Option<String>,
    /// Overrides the exported working directory.
    #[serde(default)]
    pub cwd: Option<PathBuf>,
}

/// One row of `session.timeline`: a user or assistant event plus a short
/// preview, intended for navigation rather than full rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineItem {
    pub seq: u64,
    pub ts: i64,
    pub kind: String,
    pub preview: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellRunParams {
    pub session_id: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellRunResult {
    pub ok: bool,
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimelineResult {
    pub items: Vec<TimelineItem>,
}

/// One model-managed task. `status` is `pending`, `in_progress`, `completed`
/// or `cancelled`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    #[serde(default = "default_todo_status")]
    pub status: String,
}

fn default_todo_status() -> String {
    "pending".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TodoList {
    pub items: Vec<TodoItem>,
}

/// Versioned export envelope. Kept in the protocol module so CLI/TUI clients
/// can parse it without a second schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionExport {
    pub version: u32,
    pub session: Session,
    pub events: Vec<Event>,
}

pub const SESSION_EXPORT_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResult {
    pub session: Session,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachResult {
    pub session: Session,
    pub events: Vec<Event>,
    /// True when older events exist before the returned tail.
    #[serde(default)]
    pub has_more: bool,
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
    /// True when another older page exists before the returned events.
    #[serde(default)]
    pub has_more: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloResult {
    pub protocol: u32,
    pub version: String,
    pub pid: u32,
    pub model: String,
    pub base_url: String,
    pub sessions: usize,
    /// Runtime concurrency cap for `spawn_agent`, so the TUI can show the
    /// current value without an extra request.
    #[serde(default = "default_max_parallel_subagents")]
    pub max_parallel_subagents: usize,
}

fn default_max_parallel_subagents() -> usize {
    crate::DEFAULT_MAX_PARALLEL_SUBAGENTS
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

    #[test]
    fn hello_defaults_subagent_concurrency_for_older_daemons() {
        let hello: HelloResult = serde_json::from_value(json!({
            "protocol": PROTOCOL_VERSION,
            "version": "0.0.0",
            "pid": 1,
            "model": "test",
            "base_url": "http://127.0.0.1:9/v1",
            "sessions": 0,
        }))
        .unwrap();
        assert_eq!(
            hello.max_parallel_subagents,
            crate::DEFAULT_MAX_PARALLEL_SUBAGENTS
        );
    }
}
