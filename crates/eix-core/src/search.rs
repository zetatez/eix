//! Full-text search types shared by the daemon and frontends.
//!
//! The index covers user and assistant messages across every session. The
//! daemon decides between FTS5 (trigram, 3+ characters) and a substring scan
//! for short queries, so callers only ever send raw text.

use serde::{Deserialize, Serialize};

use crate::session::SessionId;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchParams {
    pub query: String,
    /// Restrict to one session. `None` searches every session.
    #[serde(default)]
    pub session_id: Option<SessionId>,
    /// Requested hit count; the daemon clamps it to `1..=100`.
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchHit {
    pub session_id: SessionId,
    pub session_name: String,
    pub seq: u64,
    /// `user` or `assistant`.
    pub role: String,
    pub ts: i64,
    /// Text around the first match, trimmed with `…` on either side.
    pub snippet: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchResult {
    pub query: String,
    pub hits: Vec<SearchHit>,
}
