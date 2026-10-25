//! SQLite-backed session registry and append-only event log.
//!
//! For Phase 0 the log is the database: messages are projected from
//! `user_message` / `assistant_message` events, so fork can copy a prefix
//! without re-parsing events.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use eix_core::{
    now_ms, Event, EventKind, IntegrationState, IntegrationStatus, Session, SessionMode,
    SessionStatus,
};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

/// Per-plugin storage is bounded so a buggy or untrusted plugin cannot fill
/// the daemon's database. Row and byte limits are enforced by `plugin_kv_set`.
pub const MAX_PLUGIN_STORAGE_ROWS: i64 = 10_000;
pub const MAX_PLUGIN_STORAGE_BYTES: i64 = 16 * 1024 * 1024;

pub struct Store {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Default)]
pub struct StoreStats {
    pub sessions: usize,
    pub archived_sessions: usize,
    pub running_sessions: usize,
    pub events: usize,
    pub db_bytes: u64,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create data dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open database {}", path.display()))?;
        Self::init(conn)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id         TEXT PRIMARY KEY,
                name       TEXT NOT NULL,
                cwd        TEXT NOT NULL,
                status     TEXT NOT NULL,
                model      TEXT,
                parent_id  TEXT,
                forked_from_seq INTEGER,
                worktree   TEXT,
                origin     TEXT NOT NULL DEFAULT 'user',
                mode       TEXT NOT NULL DEFAULT 'build',
                agent      TEXT,
                archived   INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS events (
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                seq        INTEGER NOT NULL,
                ts         INTEGER NOT NULL,
                kind       TEXT NOT NULL,
                payload    TEXT NOT NULL,
                undone     INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (session_id, seq)
            );

            CREATE INDEX IF NOT EXISTS events_session_seq ON events(session_id, seq);

            -- Unsent prompt text, so a reconnecting frontend can restore it.
            CREATE TABLE IF NOT EXISTS drafts (
                session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                text       TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );

            -- One row per session worktree. The queue is deliberately separate
            -- from turns so a failed merge cannot make a session look busy.
            CREATE TABLE IF NOT EXISTS session_integrations (
                session_id       TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                status           TEXT NOT NULL DEFAULT 'pending',
                stage            TEXT,
                base_branch      TEXT,
                error            TEXT,
                log_path         TEXT,
                ready_at         INTEGER,
                next_retry_at    INTEGER,
                attempts         INTEGER NOT NULL DEFAULT 0,
                merged_commit    TEXT,
                worktree_disabled INTEGER NOT NULL DEFAULT 0,
                updated_at       INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS session_integrations_due
                ON session_integrations(status, next_retry_at, ready_at);

            -- One-off schema migrations. `sessions.mode` must not be
            -- re-derived from the old permission column on every startup.
            CREATE TABLE IF NOT EXISTS schema_meta (
                key   TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );

            -- Plugin-scoped key/value storage. `scope` is `global` or
            -- `workspace:<absolute path>`; plugins never see another plugin's
            -- rows because every query includes the plugin name.
            CREATE TABLE IF NOT EXISTS plugin_kv (
                plugin     TEXT NOT NULL,
                scope      TEXT NOT NULL,
                key        TEXT NOT NULL,
                value      TEXT NOT NULL,
                updated_at INTEGER NOT NULL,
                PRIMARY KEY (plugin, scope, key)
            );
            "#,
        )?;
        // Databases created before fork/rewind lack the lineage columns.
        add_column(&conn, "sessions", "parent_id", "TEXT")?;
        add_column(&conn, "sessions", "forked_from_seq", "INTEGER")?;
        add_column(&conn, "sessions", "worktree", "TEXT")?;
        add_column(&conn, "sessions", "origin", "TEXT NOT NULL DEFAULT 'user'")?;
        add_column(&conn, "sessions", "mode", "TEXT NOT NULL DEFAULT 'build'")?;
        add_column(&conn, "sessions", "agent", "TEXT")?;
        add_column(&conn, "sessions", "archived", "INTEGER NOT NULL DEFAULT 0")?;
        add_column(&conn, "events", "undone", "INTEGER NOT NULL DEFAULT 0")?;
        migrate_session_modes(&conn)?;
        drop_legacy_search_index(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn create_session(&self, session: &Session) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "INSERT INTO sessions
                (id, name, cwd, status, model, parent_id, forked_from_seq, worktree, origin,
                 mode, agent, archived, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                session.id,
                session.name,
                session.cwd.to_string_lossy(),
                session.status.as_str(),
                session.model,
                session.parent_id,
                session.forked_from_seq.map(|seq| seq as i64),
                session.worktree.as_ref().map(|path| path.to_string_lossy()),
                session.origin.as_str(),
                session.mode.as_str(),
                session.agent,
                session.archived as i64,
                session.created_at,
                session.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        self.list_sessions_including(false)
    }

    /// `include_archived = true` is used by name generation and explicit
    /// archived-session pickers; the default session list hides them.
    pub fn list_sessions_including(&self, include_archived: bool) -> Result<Vec<Session>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let sql = if include_archived {
            "SELECT id, name, cwd, status, model, created_at, updated_at,
                    parent_id, forked_from_seq, worktree, origin, mode, agent, archived
             FROM sessions ORDER BY updated_at DESC, created_at DESC"
        } else {
            "SELECT id, name, cwd, status, model, created_at, updated_at,
                    parent_id, forked_from_seq, worktree, origin, mode, agent, archived
             FROM sessions WHERE archived = 0 ORDER BY updated_at DESC, created_at DESC"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([], row_to_session)?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        Ok(sessions)
    }

    pub fn get_session(&self, id: &str) -> Result<Option<Session>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        Ok(conn
            .query_row(
                "SELECT id, name, cwd, status, model, created_at, updated_at,
                        parent_id, forked_from_seq, worktree, origin, mode, agent, archived
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    /// Change a session's persisted execution mode. Returns the updated row,
    /// or `None` when the session does not exist.
    pub fn set_session_mode(&self, id: &str, mode: SessionMode) -> Result<Option<Session>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let changed = conn.execute(
            "UPDATE sessions SET mode = ?1, updated_at = ?2 WHERE id = ?3",
            params![mode.as_str(), now_ms(), id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(conn
            .query_row(
                "SELECT id, name, cwd, status, model, created_at, updated_at,
                        parent_id, forked_from_seq, worktree, origin, mode, agent, archived
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    /// Change a session's pinned model. `None` restores the daemon default.
    pub fn set_session_model(&self, id: &str, model: Option<&str>) -> Result<Option<Session>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let changed = conn.execute(
            "UPDATE sessions SET model = ?1, updated_at = ?2 WHERE id = ?3",
            params![model, now_ms(), id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(conn
            .query_row(
                "SELECT id, name, cwd, status, model, created_at, updated_at,
                        parent_id, forked_from_seq, worktree, origin, mode, agent, archived
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    /// Change a session's primary agent. `None` restores the built-in default.
    pub fn set_session_agent(&self, id: &str, agent: Option<&str>) -> Result<Option<Session>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let changed = conn.execute(
            "UPDATE sessions SET agent = ?1, updated_at = ?2 WHERE id = ?3",
            params![agent, now_ms(), id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(conn
            .query_row(
                "SELECT id, name, cwd, status, model, created_at, updated_at,
                        parent_id, forked_from_seq, worktree, origin, mode, agent, archived
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    /// Hide or restore a session without deleting its event log.
    pub fn set_session_archived(&self, id: &str, archived: bool) -> Result<Option<Session>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let changed = conn.execute(
            "UPDATE sessions SET archived = ?1, updated_at = ?2 WHERE id = ?3",
            params![archived as i64, now_ms(), id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(conn
            .query_row(
                "SELECT id, name, cwd, status, model, created_at, updated_at,
                        parent_id, forked_from_seq, worktree, origin, mode, agent, archived
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    /// Delete a session only when it has no events, no non-empty draft, and no
    /// worktree. The checks and delete run in one statement so a concurrent
    /// prompt or draft save cannot be lost. Undone events still count as input.
    pub fn delete_session_if_unused(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let changed = conn.execute(
            "DELETE FROM sessions
             WHERE id = ?1
               AND worktree IS NULL
               AND NOT EXISTS (
                   SELECT 1 FROM events WHERE events.session_id = ?1
               )
               AND NOT EXISTS (
                   SELECT 1 FROM drafts
                   WHERE drafts.session_id = ?1 AND trim(drafts.text) <> ''
               )",
            params![id],
        )?;
        if changed > 0 {
            let _ = conn.execute(
                "DELETE FROM session_integrations WHERE session_id = ?1",
                params![id],
            );
        }
        Ok(changed > 0)
    }

    /// Delete a session and (via `ON DELETE CASCADE`) its events and draft.
    /// Returns false when the session did not exist.
    pub fn delete_session(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let _ = conn.execute(
            "DELETE FROM session_integrations WHERE session_id = ?1",
            params![id],
        );
        let changed = conn.execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(changed > 0)
    }

    /// Insert a previously exported event without re-numbering it. Export and
    /// import preserve sequence numbers so compaction ranges keep working.
    pub fn insert_event(&self, event: &Event) -> Result<()> {
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction()?;
        let payload_text = serde_json::to_string(&event.payload)?;
        tx.execute(
            "INSERT INTO events (session_id, seq, ts, kind, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                event.session_id,
                event.seq as i64,
                event.ts,
                event.kind.as_str(),
                payload_text
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Hide the newest user turn (the last `user_message` and everything after
    /// it) from `events()` and the transcript. Each undo gets its own marker so
    /// repeated undo/redo behaves like a stack.
    pub fn undo_last_turn(&self, id: &str) -> Result<Option<usize>> {
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction()?;
        let cutoff: Option<i64> = tx
            .query_row(
                "SELECT seq FROM events
                 WHERE session_id = ?1 AND undone = 0 AND kind = 'user_message'
                 ORDER BY seq DESC LIMIT 1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        let Some(cutoff) = cutoff else {
            return Ok(None);
        };
        let round: i64 = tx.query_row(
            "SELECT COALESCE(MAX(undone), 0) + 1 FROM events WHERE session_id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        let count = tx.execute(
            "UPDATE events SET undone = ?1
             WHERE session_id = ?2 AND seq >= ?3 AND undone = 0",
            params![round, id, cutoff],
        )?;
        tx.execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
            params![now_ms(), id],
        )?;
        tx.commit()?;
        Ok(Some(count))
    }

    /// Restore the most recent undo batch.
    pub fn redo_last_undo(&self, id: &str) -> Result<Option<usize>> {
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction()?;
        let round: Option<i64> = tx.query_row(
            "SELECT MAX(undone) FROM events WHERE session_id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        let Some(round) = round.filter(|round| *round > 0) else {
            return Ok(None);
        };
        let count = tx.execute(
            "UPDATE events SET undone = 0 WHERE session_id = ?1 AND undone = ?2",
            params![id, round],
        )?;
        tx.execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
            params![now_ms(), id],
        )?;
        tx.commit()?;
        Ok(Some(count))
    }

    /// Highest event sequence in a session, or `None` when the session has no
    /// events (or does not exist).
    pub fn max_seq(&self, id: &str) -> Result<u64> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let max: i64 = conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        Ok(max as u64)
    }

    /// Point a session at a different cwd (used when a worktree is removed) and
    /// optionally clear its worktree marker.
    pub fn set_session_cwd(
        &self,
        id: &str,
        cwd: &Path,
        worktree: Option<&Path>,
    ) -> Result<Option<Session>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let changed = conn.execute(
            "UPDATE sessions SET cwd = ?1, worktree = ?2, updated_at = ?3 WHERE id = ?4",
            params![
                cwd.to_string_lossy(),
                worktree.map(|path| path.to_string_lossy()),
                now_ms(),
                id,
            ],
        )?;
        drop(conn);
        if changed == 0 {
            return Ok(None);
        }
        self.get_session(id)
    }

    pub fn session_count(&self) -> Result<usize> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Whether the session has at least one user turn. Used by the idle
    /// auto-ready scanner to avoid queuing a pristine session.
    pub fn has_user_message(&self, session_id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE session_id = ?1 AND kind = 'user_message'",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Unsent prompt for a session; `(text, updated_at)` with empty defaults.
    pub fn get_draft(&self, session_id: &str) -> Result<(String, i64)> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        Ok(conn
            .query_row(
                "SELECT text, updated_at FROM drafts WHERE session_id = ?1",
                params![session_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .unwrap_or_default())
    }

    /// Store a draft, or delete it when `text` is empty. Returns the new
    /// `updated_at` (0 for a deletion).
    pub fn set_draft(&self, session_id: &str, text: &str) -> Result<i64> {
        let now = now_ms();
        let conn = self.conn.lock().expect("store mutex poisoned");
        if text.is_empty() {
            conn.execute(
                "DELETE FROM drafts WHERE session_id = ?1",
                params![session_id],
            )?;
            return Ok(0);
        }
        conn.execute(
            "INSERT INTO drafts (session_id, text, updated_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET text = excluded.text, updated_at = excluded.updated_at",
            params![session_id, text, now],
        )?;
        Ok(now)
    }

    /// Copy the prefix `seq < at_seq` of `source_id` (all of it when `at_seq`
    /// is `None`) into a new, idle session. Files are not copied: `cwd` defaults
    /// to the source's workspace, and `worktree` records an optional git
    /// worktree the caller created for the fork.
    ///
    /// Returns `None` when the source session does not exist. `at_seq` must be
    /// in `1..=max_seq + 1`.
    pub fn fork_session(
        &self,
        source_id: &str,
        at_seq: Option<u64>,
        name: &str,
        cwd: Option<&Path>,
        worktree: Option<&Path>,
    ) -> Result<Option<Session>> {
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction()?;
        let source: Option<(String, Option<String>, String, Option<String>)> = tx
            .query_row(
                "SELECT cwd, model, mode, agent FROM sessions WHERE id = ?1",
                params![source_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((source_cwd, model, mode, agent)) = source else {
            return Ok(None);
        };
        let max_seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE session_id = ?1",
            params![source_id],
            |row| row.get(0),
        )?;
        // Compare in u64: casting a huge `at_seq` to i64 can wrap negative.
        let max_seq = max_seq as u64;
        let upto = match at_seq {
            Some(at) if at >= 1 && at <= max_seq + 1 => at,
            Some(_) => bail!("at_seq must be between 1 and {}", max_seq + 1),
            // The cut is exclusive, so "everything" is one past the last seq.
            None => max_seq + 1,
        };
        let upto = upto as i64;

        let cwd = cwd
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from(source_cwd));
        let mut session = Session::new(uuid::Uuid::new_v4().to_string(), name, cwd);
        session.model = model;
        session.agent = agent;
        session.parent_id = Some(source_id.to_string());
        session.forked_from_seq = Some(upto as u64);
        session.worktree = worktree.map(Path::to_path_buf);
        session.origin = eix_core::SessionOrigin::Fork;
        session.archived = false;
        session.mode = mode.parse().unwrap_or_default();
        tx.execute(
            "INSERT INTO sessions
                (id, name, cwd, status, model, parent_id, forked_from_seq, worktree, origin,
                 mode, agent, archived, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                session.id,
                session.name,
                session.cwd.to_string_lossy(),
                session.status.as_str(),
                session.model,
                session.parent_id,
                session.forked_from_seq.map(|seq| seq as i64),
                session.worktree.as_ref().map(|path| path.to_string_lossy()),
                session.origin.as_str(),
                session.mode.as_str(),
                session.agent,
                session.archived as i64,
                session.created_at,
                session.updated_at,
            ],
        )?;
        // Sequence numbers are preserved: the fork keeps the source's prefix
        // exactly, so a ledger rebuilt from the copy matches.
        tx.execute(
            "INSERT INTO events (session_id, seq, ts, kind, payload)
             SELECT ?1, seq, ts, kind, payload FROM events
             WHERE session_id = ?2 AND seq < ?3",
            params![session.id, source_id, upto],
        )?;
        tx.commit()?;
        Ok(Some(session))
    }

    pub fn rename_session(&self, id: &str, name: &str) -> Result<Option<Session>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let changed = conn.execute(
            "UPDATE sessions SET name = ?1, updated_at = ?2 WHERE id = ?3",
            params![name, now_ms(), id],
        )?;
        drop(conn);
        if changed == 0 {
            return Ok(None);
        }
        self.get_session(id)
    }

    pub fn set_status(&self, id: &str, status: SessionStatus) -> Result<Option<Session>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let changed = conn.execute(
            "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3",
            params![status.as_str(), now_ms(), id],
        )?;
        drop(conn);
        if changed == 0 {
            return Ok(None);
        }
        self.get_session(id)
    }

    /// Append one event, assigning the next per-session sequence number. Also
    /// bumps `sessions.updated_at` so list ordering reflects activity.
    pub fn append_event(&self, id: &str, kind: EventKind, payload: Value) -> Result<Event> {
        let mut conn = self.conn.lock().expect("store mutex poisoned");
        let tx = conn.transaction()?;
        let seq: i64 = tx.query_row(
            "SELECT COALESCE(MAX(seq), 0) + 1 FROM events WHERE session_id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        let ts = now_ms();
        let payload_text = serde_json::to_string(&payload)?;
        tx.execute(
            "INSERT INTO events (session_id, seq, ts, kind, payload)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, seq, ts, kind.as_str(), payload_text],
        )?;
        tx.execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
            params![ts, id],
        )?;
        tx.commit()?;
        Ok(Event {
            session_id: id.to_string(),
            seq: seq as u64,
            ts,
            kind,
            payload,
        })
    }

    pub fn events(&self, id: &str) -> Result<Vec<Event>> {
        self.events_after(id, 0)
    }

    /// Async wrapper for full-log reads. `events()` can scan a large session;
    /// keeping it off the runtime thread avoids stalling unrelated sessions.
    pub async fn events_async(self: &Arc<Self>, id: &str) -> Result<Vec<Event>> {
        let store = Arc::clone(self);
        let id = id.to_string();
        tokio::task::spawn_blocking(move || store.events(&id))
            .await
            .context("event query task failed")?
    }

    /// Async wrapper for incremental event reads used by the turn loop.
    pub async fn events_after_async(
        self: &Arc<Self>,
        id: &str,
        after_seq: u64,
    ) -> Result<Vec<Event>> {
        let store = Arc::clone(self);
        let id = id.to_string();
        tokio::task::spawn_blocking(move || store.events_after(&id, after_seq))
            .await
            .context("event query task failed")?
    }

    pub fn events_after(&self, id: &str, after_seq: u64) -> Result<Vec<Event>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT seq, ts, kind, payload FROM events
             WHERE session_id = ?1 AND seq > ?2 AND undone = 0 ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![id, after_seq], |row| {
            let kind: String = row.get(2)?;
            let payload: String = row.get(3)?;
            Ok(Event {
                session_id: id.to_string(),
                seq: row.get::<_, i64>(0)? as u64,
                ts: row.get(1)?,
                kind: kind.parse::<EventKind>().map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        format!("unknown event kind {kind:?}").into(),
                    )
                })?,
                payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
            })
        })?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }

    /// Fetch up to `limit` events older than `before_seq`, in ascending order.
    /// The boolean is true when still older events exist.
    pub fn events_before(
        &self,
        id: &str,
        before_seq: u64,
        limit: usize,
    ) -> Result<(Vec<Event>, bool)> {
        let limit = limit.max(1);
        let fetch = limit.saturating_add(1).min(i64::MAX as usize) as i64;
        let before_seq = before_seq.min(i64::MAX as u64) as i64;
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT seq, ts, kind, payload FROM events
             WHERE session_id = ?1 AND seq < ?2 AND undone = 0
             ORDER BY seq DESC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![id, before_seq, fetch], |row| {
            let kind: String = row.get(2)?;
            let payload: String = row.get(3)?;
            Ok(Event {
                session_id: id.to_string(),
                seq: row.get::<_, i64>(0)? as u64,
                ts: row.get(1)?,
                kind: kind.parse::<EventKind>().map_err(|_| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        format!("unknown event kind {kind:?}").into(),
                    )
                })?,
                payload: serde_json::from_str(&payload).unwrap_or(Value::Null),
            })
        })?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        let has_more = events.len() > limit;
        events.truncate(limit);
        events.reverse();
        Ok((events, has_more))
    }

    /// Sessions that were mid-turn when the daemon died. They cannot be resumed
    /// honestly, so the caller marks them as errored.
    pub fn running_sessions(&self) -> Result<Vec<Session>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, name, cwd, status, model, created_at, updated_at,
                    parent_id, forked_from_seq, worktree, origin, mode, agent, archived
             FROM sessions WHERE status IN ('running', 'awaiting_approval', 'awaiting_input')",
        )?;
        let rows = stmt.query_map([], row_to_session)?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        Ok(sessions)
    }

    pub fn stats(&self) -> Result<StoreStats> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let sessions: i64 =
            conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        let archived: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE archived != 0",
            [],
            |row| row.get(0),
        )?;
        let running: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sessions WHERE status IN ('running', 'awaiting_approval', 'awaiting_input')",
            [],
            |row| row.get(0),
        )?;
        let events: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))?;
        let pages: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        Ok(StoreStats {
            sessions: sessions.max(0) as usize,
            archived_sessions: archived.max(0) as usize,
            running_sessions: running.max(0) as usize,
            events: events.max(0) as usize,
            db_bytes: (pages.max(0) as u64).saturating_mul(page_size.max(0) as u64),
        })
    }

    /// Blob ids referenced by any stored user message, including undone
    /// events (redo can bring them back).
    pub fn referenced_blob_ids(&self) -> Result<HashSet<String>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT payload FROM events
             WHERE kind = 'user_message' AND payload LIKE '%\"blob\"%'",
        )?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut ids = HashSet::new();
        for row in rows {
            let payload: Value = serde_json::from_str(&row?).unwrap_or(Value::Null);
            let Some(images) = payload.get("images").and_then(Value::as_array) else {
                continue;
            };
            for image in images {
                if let Some(blob) = image.get("blob").and_then(Value::as_str) {
                    ids.insert(blob.to_string());
                }
            }
        }
        Ok(ids)
    }

    /// Read one plugin storage value. Missing keys are `None`, not an error.
    pub fn plugin_kv_get(&self, plugin: &str, scope: &str, key: &str) -> Result<Option<String>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        Ok(conn
            .query_row(
                "SELECT value FROM plugin_kv
                 WHERE plugin = ?1 AND scope = ?2 AND key = ?3",
                params![plugin, scope, key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn plugin_kv_set(&self, plugin: &str, scope: &str, key: &str, value: &str) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let existing_bytes: Option<i64> = conn
            .query_row(
                "SELECT LENGTH(CAST(value AS BLOB)) FROM plugin_kv
                 WHERE plugin = ?1 AND scope = ?2 AND key = ?3",
                params![plugin, scope, key],
                |row| row.get(0),
            )
            .optional()?;
        let (rows, bytes): (i64, i64) = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(LENGTH(CAST(value AS BLOB))), 0)
             FROM plugin_kv WHERE plugin = ?1",
            params![plugin],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let next_rows = rows + i64::from(existing_bytes.is_none());
        let next_bytes = bytes - existing_bytes.unwrap_or(0) + value.len() as i64;
        if next_rows > MAX_PLUGIN_STORAGE_ROWS {
            bail!(
                "plugin {plugin} storage exceeds {MAX_PLUGIN_STORAGE_ROWS} rows; \
                 delete keys to free space"
            );
        }
        if next_bytes > MAX_PLUGIN_STORAGE_BYTES {
            bail!(
                "plugin {plugin} storage exceeds {MAX_PLUGIN_STORAGE_BYTES} bytes; \
                 delete keys to free space"
            );
        }
        conn.execute(
            "INSERT INTO plugin_kv (plugin, scope, key, value, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(plugin, scope, key)
             DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![plugin, scope, key, value, now_ms()],
        )?;
        Ok(())
    }

    pub fn plugin_kv_delete(&self, plugin: &str, scope: &str, key: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let changed = conn.execute(
            "DELETE FROM plugin_kv WHERE plugin = ?1 AND scope = ?2 AND key = ?3",
            params![plugin, scope, key],
        )?;
        Ok(changed > 0)
    }

    /// List keys (and values) for one plugin/scope. Values are included so a
    /// plugin can restore a small cursor cache without N round trips.
    pub fn plugin_kv_list(&self, plugin: &str, scope: &str) -> Result<Vec<(String, String)>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT key, value FROM plugin_kv
             WHERE plugin = ?1 AND scope = ?2 ORDER BY key",
        )?;
        let rows = stmt.query_map(params![plugin, scope], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn set_permissions(path: &Path) -> Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if path.exists() {
                let mode = if path.is_dir() { 0o700 } else { 0o600 };
                fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
            }
        }
        Ok(())
    }
}

impl Store {
    /// Ensure a queue row exists for a session. Existing state is kept.
    pub fn ensure_integration(
        &self,
        session_id: &str,
        base_branch: Option<&str>,
        worktree_disabled: bool,
    ) -> Result<IntegrationState> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let now = now_ms();
        conn.execute(
            "INSERT INTO session_integrations
                (session_id, status, stage, base_branch, worktree_disabled, updated_at)
             VALUES (?1, 'pending', NULL, ?2, ?3, ?4)
             ON CONFLICT(session_id) DO UPDATE SET
                base_branch = COALESCE(excluded.base_branch, session_integrations.base_branch),
                worktree_disabled = excluded.worktree_disabled,
                updated_at = excluded.updated_at",
            params![session_id, base_branch, worktree_disabled as i64, now],
        )?;
        integration_row(&conn, session_id)?
            .ok_or_else(|| anyhow::anyhow!("integration row missing"))
    }

    pub fn get_integration(&self, session_id: &str) -> Result<Option<IntegrationState>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        integration_row(&conn, session_id)
    }

    pub fn list_integrations(&self) -> Result<Vec<IntegrationState>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT session_id, status, stage, base_branch, error, log_path, ready_at,
                    next_retry_at, attempts, merged_commit, worktree_disabled, updated_at
             FROM session_integrations
             ORDER BY updated_at DESC",
        )?;
        let rows = stmt.query_map([], row_to_integration)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn save_integration(&self, state: &IntegrationState) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "INSERT INTO session_integrations
                (session_id, status, stage, base_branch, error, log_path, ready_at,
                 next_retry_at, attempts, merged_commit, worktree_disabled, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(session_id) DO UPDATE SET
                status = excluded.status,
                stage = excluded.stage,
                base_branch = excluded.base_branch,
                error = excluded.error,
                log_path = excluded.log_path,
                ready_at = excluded.ready_at,
                next_retry_at = excluded.next_retry_at,
                attempts = excluded.attempts,
                merged_commit = excluded.merged_commit,
                worktree_disabled = excluded.worktree_disabled,
                updated_at = excluded.updated_at",
            params![
                state.session_id,
                state.status.as_str(),
                state.stage,
                state.base_branch,
                state.error,
                state.log_path.as_ref().map(|path| path.to_string_lossy()),
                state.ready_at,
                state.next_retry_at,
                state.attempts as i64,
                state.merged_commit,
                state.worktree_disabled as i64,
                state.updated_at,
            ],
        )?;
        Ok(())
    }

    /// Next queue item whose retry time has arrived. Ready items have no
    /// `next_retry_at` and are always eligible.
    pub fn next_integration(&self, now: i64) -> Result<Option<IntegrationState>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        Ok(conn
            .query_row(
                "SELECT session_id, status, stage, base_branch, error, log_path, ready_at,
                        next_retry_at, attempts, merged_commit, worktree_disabled, updated_at
                 FROM session_integrations
                 WHERE status IN ('ready', 'retrying')
                   AND (next_retry_at IS NULL OR next_retry_at <= ?1)
                 ORDER BY COALESCE(ready_at, updated_at), updated_at
                 LIMIT 1",
                params![now],
                row_to_integration,
            )
            .optional()?)
    }

    pub fn delete_integration(&self, session_id: &str) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "DELETE FROM session_integrations WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }
}

fn integration_row(conn: &Connection, session_id: &str) -> Result<Option<IntegrationState>> {
    Ok(conn
        .query_row(
            "SELECT session_id, status, stage, base_branch, error, log_path, ready_at,
                    next_retry_at, attempts, merged_commit, worktree_disabled, updated_at
             FROM session_integrations WHERE session_id = ?1",
            params![session_id],
            row_to_integration,
        )
        .optional()?)
}

fn row_to_integration(row: &Row<'_>) -> rusqlite::Result<IntegrationState> {
    let status_text: String = row.get(1)?;
    let status = status_text.parse::<IntegrationStatus>().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            1,
            rusqlite::types::Type::Text,
            format!("unknown integration status {status_text:?}").into(),
        )
    })?;
    Ok(IntegrationState {
        session_id: row.get(0)?,
        status,
        stage: row.get(2)?,
        base_branch: row.get(3)?,
        error: row.get(4)?,
        log_path: row.get::<_, Option<String>>(5)?.map(PathBuf::from),
        ready_at: row.get(6)?,
        next_retry_at: row.get(7)?,
        attempts: row.get::<_, i64>(8)? as u32,
        merged_commit: row.get(9)?,
        worktree_disabled: row.get::<_, i64>(10)? != 0,
        updated_at: row.get(11)?,
    })
}

fn row_to_session(row: &Row<'_>) -> rusqlite::Result<Session> {
    let status_text: String = row.get(3)?;
    let status = status_text.parse::<SessionStatus>().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            format!("unknown session status {status_text:?}").into(),
        )
    })?;
    Ok(Session {
        id: row.get(0)?,
        name: row.get(1)?,
        cwd: row.get::<_, String>(2)?.into(),
        status,
        model: row.get(4)?,
        created_at: row.get(5)?,
        updated_at: row.get(6)?,
        parent_id: row.get(7)?,
        forked_from_seq: row.get::<_, Option<i64>>(8)?.map(|seq| seq as u64),
        worktree: row.get::<_, Option<String>>(9)?.map(PathBuf::from),
        origin: row
            .get::<_, Option<String>>(10)?
            .and_then(|value| value.parse().ok())
            .unwrap_or_default(),
        mode: row
            .get::<_, Option<String>>(11)?
            .and_then(|value| value.parse().ok())
            .unwrap_or_default(),
        agent: row.get(12)?,
        archived: row.get::<_, i64>(13)? != 0,
    })
}

/// Remove the search index from databases created before the feature was
/// deleted. `schema_meta` records the cleanup so the drops only run once.
fn drop_legacy_search_index(conn: &Connection) -> Result<()> {
    let removed: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key = 'search.index.v1'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if removed.is_some() {
        return Ok(());
    }
    // Triggers first: they reference the FTS table, which must go before the
    // content table. `IF EXISTS` keeps this safe for databases that never had
    // the index (including brand-new ones).
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS search_docs_ai;
         DROP TRIGGER IF EXISTS search_docs_ad;
         DROP TRIGGER IF EXISTS search_docs_au;
         DROP TABLE IF EXISTS search_fts;
         DROP TABLE IF EXISTS search_docs;",
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO schema_meta (key, value) VALUES ('search.index.v1', '1')",
        [],
    )?;
    Ok(())
}

/// Add a column to an existing table (SQLite has no `ADD COLUMN IF NOT
/// EXISTS`). Used for migrations that must not break existing databases.
fn add_column(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    if !table_has_column(conn, table, column)? {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
            [],
        )?;
    }
    Ok(())
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?;
    Ok(columns.iter().any(|name| name == column))
}

/// Move old `(permission, agent)` databases onto the single session mode. The
/// built-in plan/build agents no longer exist, so rows carrying those names are
/// reduced to their mode. Custom agent names are preserved. A marker keeps the
/// mapping from running twice if an old database still has the permission
/// column (we intentionally leave unused columns in place).
fn migrate_session_modes(conn: &Connection) -> Result<()> {
    let migrated: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key = 'sessions.mode.v1'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if migrated.is_some() {
        return Ok(());
    }
    if table_has_column(conn, "sessions", "permission")? {
        conn.execute(
            "UPDATE sessions SET mode = CASE
                 WHEN agent = 'plan'
                   OR permission IN ('read-only', 'readonly', 'read_only') THEN 'plan'
                 WHEN permission IN ('full', 'bypass') THEN 'bypass'
                 ELSE 'build'
             END",
            [],
        )?;
    } else {
        // A very old database may predate the permission column. The agent name
        // still carries the read-only/planning intent.
        conn.execute("UPDATE sessions SET mode = 'plan' WHERE agent = 'plan'", [])?;
        conn.execute(
            "UPDATE sessions SET mode = 'build' WHERE agent = 'build'",
            [],
        )?;
    }
    // Plan/build moved from agent names to modes. A custom agent with one of
    // those names is no longer selectable either, so drop the stale reference
    // instead of silently re-binding it to a future custom file.
    conn.execute(
        "UPDATE sessions SET agent = NULL WHERE agent IN ('plan', 'build')",
        [],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO schema_meta (key, value) VALUES ('sessions.mode.v1', '1')",
        [],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> Session {
        Session::new(id, "test", "/tmp")
    }

    #[test]
    fn mode_round_trips_and_fork_inherits_it() {
        let store = Store::in_memory().unwrap();
        let mut parent = session("s1");
        parent.mode = SessionMode::Bypass;
        store.create_session(&parent).unwrap();
        assert_eq!(
            store.get_session("s1").unwrap().unwrap().mode,
            SessionMode::Bypass
        );

        let updated = store
            .set_session_mode("s1", SessionMode::Plan)
            .unwrap()
            .expect("updated row");
        assert_eq!(updated.mode, SessionMode::Plan);

        let forked = store
            .fork_session("s1", None, "fork", None, None)
            .unwrap()
            .expect("fork");
        assert_eq!(forked.mode, SessionMode::Plan);

        assert!(store
            .set_session_mode("missing", SessionMode::Auto)
            .unwrap()
            .is_none());
    }

    #[test]
    fn old_permission_and_agent_columns_migrate_to_mode() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE sessions (
                id         TEXT PRIMARY KEY,
                name       TEXT NOT NULL,
                cwd        TEXT NOT NULL,
                status     TEXT NOT NULL,
                model      TEXT,
                parent_id  TEXT,
                forked_from_seq INTEGER,
                worktree   TEXT,
                origin     TEXT NOT NULL DEFAULT 'user',
                permission TEXT NOT NULL DEFAULT 'workspace-write',
                agent      TEXT,
                archived   INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );
            INSERT INTO sessions
                (id, name, cwd, status, permission, agent, created_at, updated_at)
            VALUES
                ('ro',      'ro',      '/tmp', 'idle', 'read-only',      NULL,       1, 1),
                ('plan',    'plan',    '/tmp', 'idle', 'workspace-write', 'plan',    1, 1),
                ('custom',  'custom',  '/tmp', 'idle', 'workspace-write', 'reviewer', 1, 1),
                ('full',    'full',    '/tmp', 'idle', 'full',            NULL,       1, 1),
                ('build',   'build',   '/tmp', 'idle', 'workspace-write', 'build',   1, 1);
            "#,
        )
        .unwrap();
        let store = Store::init(conn).unwrap();
        let mut rows: Vec<_> = store.list_sessions().unwrap();
        rows.sort_by(|left, right| left.id.cmp(&right.id));
        let by_id = |id: &str| rows.iter().find(|row| row.id == id).unwrap();

        assert_eq!(by_id("ro").mode, SessionMode::Plan);
        assert_eq!(by_id("plan").mode, SessionMode::Plan);
        assert_eq!(by_id("plan").agent, None);
        assert_eq!(by_id("build").mode, SessionMode::Build);
        assert_eq!(by_id("build").agent, None);
        assert_eq!(by_id("custom").mode, SessionMode::Build);
        assert_eq!(by_id("custom").agent.as_deref(), Some("reviewer"));
        assert_eq!(by_id("full").mode, SessionMode::Bypass);

        // The migration marker stops a later startup from overwriting an
        // explicit user mode change with the old permission column.
        {
            let conn = store.conn.lock().unwrap();
            conn.execute("UPDATE sessions SET mode = 'build' WHERE id = 'ro'", [])
                .unwrap();
            migrate_session_modes(&conn).unwrap();
            let mode: String = conn
                .query_row("SELECT mode FROM sessions WHERE id = 'ro'", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(mode, "build");
        }
    }

    #[test]
    fn legacy_search_index_is_dropped_once() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE search_docs (
                doc_id     INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                seq        INTEGER NOT NULL,
                role       TEXT NOT NULL,
                ts         INTEGER NOT NULL,
                text       TEXT NOT NULL,
                UNIQUE (session_id, seq)
            );
            CREATE VIRTUAL TABLE search_fts USING fts5(
                text,
                content='search_docs',
                content_rowid='doc_id',
                tokenize='trigram'
            );
            CREATE TRIGGER search_docs_ai AFTER INSERT ON search_docs BEGIN
                INSERT INTO search_fts(rowid, text) VALUES (new.doc_id, new.text);
            END;
            CREATE TRIGGER search_docs_ad AFTER DELETE ON search_docs BEGIN
                INSERT INTO search_fts(search_fts, rowid, text)
                VALUES ('delete', old.doc_id, old.text);
            END;
            CREATE TRIGGER search_docs_au AFTER UPDATE ON search_docs BEGIN
                INSERT INTO search_fts(search_fts, rowid, text)
                VALUES ('delete', old.doc_id, old.text);
                INSERT INTO search_fts(rowid, text) VALUES (new.doc_id, new.text);
            END;
            INSERT INTO search_docs (session_id, seq, role, ts, text)
            VALUES ('s1', 1, 'user', 1, 'legacy');
            "#,
        )
        .unwrap();

        let store = Store::init(conn).unwrap();
        let conn = store.conn.lock().unwrap();
        let remaining: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name LIKE 'search%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining, 0);
        let marker: String = conn
            .query_row(
                "SELECT value FROM schema_meta WHERE key = 'search.index.v1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(marker, "1");

        // Idempotent on later startups.
        drop_legacy_search_index(&conn).unwrap();
    }

    #[test]
    fn sessions_and_events_round_trip() {
        let store = Store::in_memory().unwrap();
        store.create_session(&session("s1")).unwrap();
        assert_eq!(store.session_count().unwrap(), 1);

        store
            .append_event(
                "s1",
                EventKind::UserMessage,
                serde_json::json!({"text": "hi"}),
            )
            .unwrap();
        let event = store
            .append_event(
                "s1",
                EventKind::AssistantMessage,
                serde_json::json!({"text": "hello", "model": "m", "cancelled": false}),
            )
            .unwrap();
        assert_eq!(event.seq, 2);

        let events = store.events("s1").unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].payload["text"], "hi");
        assert_eq!(events[1].payload["text"], "hello");
    }

    #[test]
    fn unused_sessions_are_only_deleted_without_events_drafts_or_worktrees() {
        let store = Store::in_memory().unwrap();
        store.create_session(&session("empty")).unwrap();
        assert!(store.delete_session_if_unused("empty").unwrap());
        assert!(store.get_session("empty").unwrap().is_none());
        assert!(!store.delete_session_if_unused("missing").unwrap());

        store.create_session(&session("drafted")).unwrap();
        store.set_draft("drafted", "draft").unwrap();
        assert!(!store.delete_session_if_unused("drafted").unwrap());
        store.set_draft("drafted", "   ").unwrap();
        assert!(store.delete_session_if_unused("drafted").unwrap());

        store.create_session(&session("used")).unwrap();
        store
            .append_event(
                "used",
                EventKind::UserMessage,
                serde_json::json!({"text": "hi"}),
            )
            .unwrap();
        assert!(!store.delete_session_if_unused("used").unwrap());

        let mut worktree = session("worktree");
        worktree.worktree = Some(PathBuf::from("/tmp/worktree"));
        store.create_session(&worktree).unwrap();
        assert!(!store.delete_session_if_unused("worktree").unwrap());
    }

    #[test]
    fn status_and_rename() {
        let store = Store::in_memory().unwrap();
        store.create_session(&session("s1")).unwrap();
        let updated = store
            .set_status("s1", SessionStatus::Running)
            .unwrap()
            .unwrap();
        assert_eq!(updated.status, SessionStatus::Running);

        let renamed = store.rename_session("s1", "auth").unwrap().unwrap();
        assert_eq!(renamed.name, "auth");
        assert!(store.get_session("nope").unwrap().is_none());
    }

    #[test]
    fn events_after_seq() {
        let store = Store::in_memory().unwrap();
        store.create_session(&session("s1")).unwrap();
        for i in 0..3 {
            store
                .append_event(
                    "s1",
                    EventKind::UserMessage,
                    serde_json::json!({"text": format!("m{i}")}),
                )
                .unwrap();
        }
        assert_eq!(store.events_after("s1", 1).unwrap().len(), 2);
        assert_eq!(store.events_after("s1", 3).unwrap().len(), 0);
    }

    #[test]
    fn events_before_seq_pages_backwards() {
        let store = Store::in_memory().unwrap();
        store.create_session(&session("s1")).unwrap();
        for i in 0..5 {
            store
                .append_event(
                    "s1",
                    EventKind::UserMessage,
                    serde_json::json!({"text": format!("m{i}")}),
                )
                .unwrap();
        }
        let (page, has_more) = store.events_before("s1", 5, 2).unwrap();
        assert_eq!(
            page.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert!(has_more);

        let (page, has_more) = store.events_before("s1", 3, 2).unwrap();
        assert_eq!(
            page.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(!has_more);
    }

    #[test]
    fn fork_copies_only_the_prefix_and_records_lineage() {
        let store = Store::in_memory().unwrap();
        let mut source = session("s1");
        source.model = Some("m".into());
        source.status = SessionStatus::Running;
        store.create_session(&source).unwrap();
        for i in 0..3 {
            store
                .append_event(
                    "s1",
                    EventKind::UserMessage,
                    serde_json::json!({"text": format!("m{i}")}),
                )
                .unwrap();
        }

        let fork = store
            .fork_session("s1", Some(2), "test~2", None, None)
            .unwrap()
            .unwrap();
        assert_eq!(fork.name, "test~2");
        assert_eq!(fork.parent_id.as_deref(), Some("s1"));
        assert_eq!(fork.forked_from_seq, Some(2));
        assert_eq!(fork.model.as_deref(), Some("m"));
        assert_eq!(fork.status, SessionStatus::Idle);
        assert_eq!(fork.cwd, source.cwd);
        assert_ne!(fork.id, source.id);

        // seq < 2, with the original sequence number preserved.
        let events = store.events(&fork.id).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].seq, 1);
        assert_eq!(events[0].payload["text"], "m0");
        // The source is untouched, and a full copy keeps everything.
        assert_eq!(store.events("s1").unwrap().len(), 3);
        let all = store
            .fork_session("s1", None, "test~3", None, None)
            .unwrap()
            .unwrap();
        assert_eq!(all.forked_from_seq, Some(4));
        assert_eq!(store.events(&all.id).unwrap().len(), 3);

        // `max + 1` is the "copy everything" form used by rewind-at-end.
        let edge = store
            .fork_session("s1", Some(4), "test~4", None, None)
            .unwrap()
            .unwrap();
        assert_eq!(store.events(&edge.id).unwrap().len(), 3);
        assert!(store
            .fork_session("nope", None, "x", None, None)
            .unwrap()
            .is_none());
        assert!(store.fork_session("s1", Some(9), "x", None, None).is_err());
        assert!(store
            .fork_session("s1", Some(u64::MAX), "x", None, None)
            .is_err());
        assert!(store.fork_session("s1", Some(0), "x", None, None).is_err());
    }

    #[test]
    fn integration_rows_round_trip_and_respect_retry_times() {
        let store = Store::in_memory().unwrap();
        store.create_session(&session("s1")).unwrap();
        assert!(!store.has_user_message("s1").unwrap());
        store
            .append_event(
                "s1",
                EventKind::UserMessage,
                serde_json::json!({"text": "work"}),
            )
            .unwrap();
        assert!(store.has_user_message("s1").unwrap());

        let state = store
            .ensure_integration("s1", Some("master"), false)
            .unwrap();
        assert_eq!(state.session_id, "s1");
        assert_eq!(state.status, IntegrationStatus::Pending);
        assert_eq!(state.base_branch.as_deref(), Some("master"));
        assert!(!state.worktree_disabled);
        assert!(store.next_integration(now_ms()).unwrap().is_none());

        let mut ready = state.clone();
        ready.status = IntegrationStatus::Ready;
        ready.ready_at = Some(now_ms());
        store.save_integration(&ready).unwrap();
        let due = store.next_integration(now_ms()).unwrap().unwrap();
        assert_eq!(due.status, IntegrationStatus::Ready);

        let mut retry = ready;
        retry.status = IntegrationStatus::Retrying;
        retry.next_retry_at = Some(now_ms() + 60_000);
        store.save_integration(&retry).unwrap();
        assert!(store.next_integration(now_ms()).unwrap().is_none());
        assert_eq!(
            store
                .next_integration(now_ms() + 60_001)
                .unwrap()
                .unwrap()
                .status,
            IntegrationStatus::Retrying
        );

        let listed = store.list_integrations().unwrap();
        assert_eq!(listed.len(), 1);
        store.delete_integration("s1").unwrap();
        assert!(store.get_integration("s1").unwrap().is_none());
    }

    #[test]
    fn plugin_storage_is_scoped_and_quota_limited() {
        let store = Store::in_memory().unwrap();
        store.plugin_kv_set("alpha", "global", "k", "v").unwrap();
        assert_eq!(
            store
                .plugin_kv_get("alpha", "global", "k")
                .unwrap()
                .as_deref(),
            Some("v")
        );
        assert_eq!(
            store
                .plugin_kv_get("alpha", "workspace:/tmp/x", "k")
                .unwrap(),
            None
        );
        assert_eq!(store.plugin_kv_get("beta", "global", "k").unwrap(), None);

        // Fill the row quota directly so the test does not depend on thousands
        // of individual round trips.
        {
            let mut conn = store.conn.lock().expect("store mutex poisoned");
            let tx = conn.transaction().unwrap();
            for index in 0..MAX_PLUGIN_STORAGE_ROWS {
                tx.execute(
                    "INSERT INTO plugin_kv (plugin, scope, key, value, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    rusqlite::params!["rows", "global", format!("k{index}"), "x", now_ms()],
                )
                .unwrap();
            }
            tx.commit().unwrap();
        }
        let error = store
            .plugin_kv_set("rows", "global", "overflow", "x")
            .unwrap_err();
        assert!(error.to_string().contains("rows"), "{error}");
        store.plugin_kv_delete("rows", "global", "k0").unwrap();
        store
            .plugin_kv_set("rows", "global", "overflow", "x")
            .expect("deleting a row frees the quota");

        // The value limit is 1 MB, so 16 chunks fit just under 16 MiB.
        let chunk = "x".repeat(1_000_000);
        for index in 0..16 {
            store
                .plugin_kv_set("bytes", "global", &format!("k{index}"), &chunk)
                .unwrap();
        }
        let error = store
            .plugin_kv_set("bytes", "global", "k16", &chunk)
            .unwrap_err();
        assert!(error.to_string().contains("bytes"), "{error}");
        // Replacing an existing key does not consume another row and keeps the
        // same byte count, so it remains allowed at the limit.
        store
            .plugin_kv_set("bytes", "global", "k0", &chunk)
            .expect("same-size update stays within quota");
    }
}
