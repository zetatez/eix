//! Core types shared by the eix daemon and every frontend.
//!
//! The daemon is the source of truth for sessions and their append-only event
//! log. Frontends (CLI/TUI today, Neovim later) only render state and send
//! commands over the protocol in [`protocol`].

pub mod config;
pub mod context;
pub mod event;
pub mod message;
pub mod nvim_plugin;
pub mod picker;
pub mod process;
pub mod protocol;
pub mod redact;
pub mod sandbox;
pub mod search;
pub mod session;

pub use config::{
    AutoApproveRule, Config, ConfigError, ContextConfig, InputBackend, InputConfig,
    NotificationsConfig, ProviderConfig, SandboxMode, ServerConfig, ToolPolicy, ToolsConfig,
};
pub use context::{ContextEntryInfo, ContextLedgerResult, ContextSectorInfo, Sector};
pub use event::{
    payload, ApprovalRequiredPayload, AssistantMessagePayload, ContextCompactedPayload,
    ContextDropPayload, ContextPinPayload, ErrorPayload, Event, EventKind, HunkInfo,
    PatchProposedPayload, PatchResultPayload, PermissionPayload, ProcessExitedPayload,
    ProcessStartedPayload, SnapshotPayload, StatusChangedPayload, ToolCallPayload,
    ToolResultPayload, UserMessagePayload,
};
pub use message::{
    ChatMessage, FunctionCall, FunctionDefinition, Role, ToolCall, ToolDefinition, Usage,
};
pub use picker::{
    is_data_kind, PickerItem, PickerParams, PickerResult, DATA_KINDS, KIND_BRANCHES, KIND_CONTEXT,
    KIND_FILES, KIND_WORKTREES,
};
pub use process::{ProcessInfo, ProcessKillResult, ProcessList, ProcessOutput, ProcessStatus};
pub use protocol::{
    ApprovalDecision, ApprovalInfo, ApprovalListParams, ApprovalListResult, ApprovalRespondParams,
    ApprovalResult, AttachResult, AuthParams, AuthResult, CancelResult, ContextCompactParams,
    ContextDropParams, ContextLedgerParams, ContextPinParams, CreateSessionParams, DraftParams,
    DraftResult, Endpoint, EventsParams, EventsResult, ForkParams, HelloResult, Notification,
    PatchApplyParams, PatchGetParams, PatchGetResult, PatchRejectParams, PatchResult,
    PatchReviseParams, PermissionParams, ProcessKillParams, ProcessListParams, ProcessOutputParams,
    RenameParams, Request, Response, RewindParams, RewindResult, ServerMessage, SessionList,
    SessionRef, SessionResult, SubmitParams, SubmitResult, SubscribeParams, SubscribeResult,
    WorktreeRemoveParams, WorktreeRemoveResult, PROTOCOL_VERSION,
};
pub use search::{SearchHit, SearchParams, SearchResult};
pub use session::{PermissionLevel, Session, SessionId, SessionOrigin, SessionStatus};

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch. All persisted timestamps use this.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
