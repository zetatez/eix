//! Core types shared by the eix daemon and every frontend.
//!
//! The daemon is the source of truth for sessions and their append-only event
//! log. Frontends (CLI/TUI today, Neovim later) only render state and send
//! commands over the protocol in [`protocol`].

pub mod agents;
pub mod commands;
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
pub mod skills;

pub use agents::{find_agent, load_agents, AgentMode, AgentSpec};
pub use commands::{expand as expand_command, load_commands, CustomCommand};
pub use config::{
    load_cli_config, theme_preset, AutoApproveRule, CliConfig, Config, ConfigError, ContextConfig,
    FormatterConfig, ImagesConfig, LspConfig, LspServerConfig, McpConfig, McpServerConfig,
    ModelConfig, ModelInfo, ModelRoute, NotificationsConfig, PermissionAction, PermissionRule,
    ProviderConfig, SandboxMode, ServerConfig, ThemeConfig, ThemePreset, ToolPolicy, ToolsConfig,
    THEME_PRESETS,
};
pub use context::{ContextEntryInfo, ContextLedgerResult, ContextSectorInfo, Sector};
pub use event::{
    payload, ApprovalRequiredPayload, AssistantMessagePayload, ContextClearedPayload,
    ContextCompactedPayload, ContextDropPayload, ContextPinPayload, ErrorPayload, Event, EventKind,
    HunkInfo, ImageAttachment, InputAnsweredPayload, InputOption, InputRequiredPayload,
    PatchProposedPayload, PatchResultPayload, PermissionPayload, ProcessExitedPayload,
    ProcessStartedPayload, SnapshotPayload, StatusChangedPayload, ToolCallPayload,
    ToolResultPayload, UserMessagePayload,
};
pub use message::{
    ChatMessage, FunctionCall, FunctionDefinition, ImagePart, Role, ToolCall, ToolDefinition, Usage,
};
pub use picker::{
    is_data_kind, PickerItem, PickerParams, PickerResult, DATA_KINDS, KIND_BRANCHES, KIND_CONTEXT,
    KIND_FILES, KIND_WORKTREES,
};
pub use process::{ProcessInfo, ProcessKillResult, ProcessList, ProcessOutput, ProcessStatus};
pub use protocol::{
    AgentListResult, ApprovalDecision, ApprovalInfo, ApprovalListParams, ApprovalListResult,
    ApprovalRespondParams, ApprovalResult, AttachParams, AttachResult, AuthParams, AuthResult,
    BtwParams, BtwResult, CancelResult, ContextClearParams, ContextCompactParams,
    ContextDropParams, ContextLedgerParams, ContextPinParams, CreateSessionParams, DraftParams,
    DraftResult, Endpoint, EventsParams, EventsResult, ForkParams, HelloResult, InputRespondParams,
    InputResult, LspDiagnosticInfo, LspWorkspaceParams, LspWorkspaceResult, McpListResult,
    McpServerInfo, ModelListResult, Notification, PatchApplyParams, PatchGetParams, PatchGetResult,
    PatchRejectParams, PatchResult, PatchReviseParams, PermissionParams, ProcessKillParams,
    ProcessListParams, ProcessOutputParams, RenameParams, Request, Response, RewindParams,
    RewindResult, ServerMessage, SessionAgentParams, SessionArchiveParams, SessionCdParams,
    SessionExport, SessionExportResult, SessionImportParams, SessionList, SessionModelParams,
    SessionRef, SessionResult, ShellRunParams, ShellRunResult, StatsResult, SubmitParams,
    SubmitResult, SubscribeParams, SubscribeResult, TimelineItem, TimelineResult, TodoItem,
    TodoList, WorktreeRemoveParams, WorktreeRemoveResult, PROTOCOL_VERSION, SESSION_EXPORT_VERSION,
};
pub use search::{SearchHit, SearchParams, SearchResult};
pub use session::{PermissionLevel, Session, SessionId, SessionOrigin, SessionStatus};
pub use skills::{available_prompt as available_skills_prompt, find_skill, load_skills, Skill};

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch. All persisted timestamps use this.
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
