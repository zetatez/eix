//! Core types shared by the eix daemon and every frontend.
//!
//! The daemon is the source of truth for sessions and their append-only event
//! log. Frontends (CLI/TUI today, Neovim later) only render state and send
//! commands over the protocol in [`protocol`].

pub mod config;
pub mod event;
pub mod message;
pub mod protocol;
pub mod sandbox;
pub mod session;

pub use config::{
    Config, ConfigError, InputBackend, InputConfig, NotificationsConfig, ProviderConfig,
    SandboxMode, ToolPolicy, ToolsConfig,
};
pub use event::{
    payload, ApprovalRequiredPayload, AssistantMessagePayload, ErrorPayload, Event, EventKind,
    HunkInfo, SnapshotPayload, StatusChangedPayload, ToolCallPayload, ToolResultPayload,
    UserMessagePayload,
};
pub use message::{
    ChatMessage, FunctionCall, FunctionDefinition, Role, ToolCall, ToolDefinition, Usage,
};
pub use protocol::{
    ApprovalDecision, ApprovalRespondParams, ApprovalResult, AttachResult, CancelResult,
    CreateSessionParams, EventsParams, EventsResult, HelloResult, Notification, RenameParams,
    Request, Response, ServerMessage, SessionList, SessionRef, SessionResult, SubmitParams,
    SubmitResult, PROTOCOL_VERSION,
};
pub use session::{Session, SessionId, SessionStatus};

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch. All persisted timestamps use this.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
