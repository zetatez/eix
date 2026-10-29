//! Picker providers shared by the daemon and every frontend.
//!
//! DESIGN.md §8 describes one unified `query → filter → preview → action`
//! overlay with pluggable providers. The daemon owns the providers that need
//! workspace access; frontends ask for rows with `picker.query`, fetch the
//! selected row's body with `picker.preview`, and add their own fuzzy filtering
//! and actions.
//!
//! Providers today: `files` (workspace paths), `images` (workspace images),
//! `grep` (workspace file contents as `path:line` rows), `git` (worktrees +
//! branches), `worktrees` and `branches` (git), and `context` (the session's
//! ledger).
//! Frontends may also synthesize rows locally (the TUI command palette), so
//! nothing here assumes the daemon sent every item.

use serde::{Deserialize, Serialize};

use crate::session::SessionId;

/// Provider name for workspace files.
pub const KIND_FILES: &str = "files";
/// Provider name for git worktrees of the session's repository.
pub const KIND_WORKTREES: &str = "worktrees";
/// Provider name for git branches of the session's repository.
pub const KIND_BRANCHES: &str = "branches";
/// Provider name for the combined git worktree/branch picker.
pub const KIND_GIT: &str = "git";
/// Provider name for workspace file contents (`path:line` matches).
pub const KIND_GREP: &str = "grep";
/// Provider name for workspace image files (`png`, `jpg`, ...).
pub const KIND_IMAGES: &str = "images";
/// Preview provider name for a session's recent conversation.
pub const KIND_SESSION_PREVIEW: &str = "sessions";

/// Every provider the daemon answers for `picker.query`.
pub const DATA_KINDS: [&str; 6] = [
    KIND_FILES,
    KIND_WORKTREES,
    KIND_BRANCHES,
    KIND_GIT,
    KIND_GREP,
    KIND_IMAGES,
];

/// True when `kind` is a daemon-backed provider.
pub fn is_data_kind(kind: &str) -> bool {
    DATA_KINDS.contains(&kind)
}

/// One row returned by `picker.query`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PickerItem {
    /// What a frontend inserts or acts on. For files: a workspace-relative path.
    pub value: String,
    /// Display label; defaults to `value` when a provider has no nicer form.
    pub label: String,
    /// Secondary text (size, kind, ...), rendered dimmer than the label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// Multi-line preview for the picker's right pane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

impl PickerItem {
    pub fn new(value: impl Into<String>) -> Self {
        let value = value.into();
        Self {
            label: value.clone(),
            value,
            detail: None,
            preview: None,
        }
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_preview(mut self, preview: impl Into<String>) -> Self {
        self.preview = Some(preview.into());
        self
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickerParams {
    /// Provider name; see the `KIND_*` constants.
    pub kind: String,
    /// Workspace to search. Required by providers that read the filesystem.
    #[serde(default)]
    pub session_id: Option<SessionId>,
    /// Provider query: a case-insensitive row filter for files/git, and the
    /// content search pattern for `grep`.
    #[serde(default)]
    pub query: Option<String>,
    /// Requested row count; the daemon clamps it to a sane range.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickerResult {
    pub kind: String,
    pub items: Vec<PickerItem>,
}

/// Request a lazy preview for one picker row. The list query only sends paths
/// and metadata; previews are fetched on selection so large files and long
/// session histories never inflate the initial payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickerPreviewParams {
    /// Preview provider: `files` or [`KIND_SESSION_PREVIEW`].
    pub kind: String,
    /// Workspace context for `files`. Sessions previews use `value` instead.
    #[serde(default)]
    pub session_id: Option<SessionId>,
    /// Row value: workspace-relative file path or target session id.
    pub value: String,
    /// Optional line/event cap for providers that support paging.
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickerPreviewResult {
    pub kind: String,
    pub value: String,
    /// Plain text for the picker's right pane.
    pub text: String,
}
