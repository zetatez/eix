//! SQLite-backed session registry and append-only event log.
//!
//! For Phase 0 the log is the database: messages are projected from
//! `user_message` / `assistant_message` events, so fork can copy a prefix
//! without re-parsing events.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use eix_core::{
    now_ms, Event, EventKind, Goal, IntegrationState, IntegrationStatus, Schedule, Session,
    SessionMode, SessionStatus,
};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

/// Per-plugin storage is bounded so a buggy or untrusted plugin cannot fill
/// the daemon's database. Row and byte limits are enforced by `plugin_kv_set`.
pub const MAX_PLUGIN_STORAGE_ROWS: i64 = 10_000;
pub const MAX_PLUGIN_STORAGE_BYTES: i64 = 16 * 1024 * 1024;

pub struct Store {
    conn: Mutex<Connection>,
    /// Dedicated read-only connection. WAL lets it observe committed writes
    /// without waiting for the writer lock, so a long SELECT no longer blocks
    /// `append_event` for every other session. `None` for in-memory test
    /// databases, which are private to one connection.
    reader: Option<Mutex<Connection>>,
}

#[derive(Debug, Clone, Default)]
pub struct StoreStats {
    pub sessions: usize,
    pub archived_sessions: usize,
    pub running_sessions: usize,
    pub events: usize,
    pub db_bytes: u64,
}

/// A long-lived credential minted by pairing. Only the hash is stored; the
/// plaintext token is shown once and then lives with the client.
#[derive(Debug, Clone, Serialize)]
pub struct ClientToken {
    pub id: String,
    pub name: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub revoked: bool,
}

impl Store {
    /// Recover the connection after a panic instead of poisoning every later
    /// query. A transaction is rolled back when its guard drops, so the
    /// recovered connection is still usable.
    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Read-only queries. Falls back to the writer for in-memory databases.
    fn read_conn(&self) -> MutexGuard<'_, Connection> {
        match &self.reader {
            Some(reader) => reader
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            None => self.conn(),
        }
    }

    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("cannot create data dir {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("cannot open database {}", path.display()))?;
        let mut store = Self::init(conn)?;
        // Opened after `init` so the schema/WAL settings already exist. Read
        // only: a SELECT that accidentally writes fails loudly instead of
        // silently racing the writer.
        let reader = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .with_context(|| format!("cannot open database for reading {}", path.display()))?;
        reader.busy_timeout(Duration::from_secs(5))?;
        store.reader = Some(Mutex::new(reader));
        Ok(store)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // WAL + NORMAL avoids an fsync per appended event. A power loss can
        // drop the newest transaction, but the database stays consistent and
        // the daemon can rebuild any unflushed runtime state from the log.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS sessions (
                id         TEXT PRIMARY KEY,
                name       TEXT NOT NULL,
                cwd        TEXT NOT NULL,
                status     TEXT NOT NULL,
                model      TEXT,
                plan_model TEXT,
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
            -- Timeline only needs user messages; keep that scan at the tail.
            CREATE INDEX IF NOT EXISTS events_session_kind_seq
                ON events(session_id, kind, seq);

            -- Unsent prompt text, so a reconnecting frontend can restore it.
            CREATE TABLE IF NOT EXISTS drafts (
                session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                text       TEXT NOT NULL,
                updated_at INTEGER NOT NULL
            );

            -- One persistent goal per session. Injected into the context on
            -- every turn until marked done or cleared.
            CREATE TABLE IF NOT EXISTS session_goals (
                session_id TEXT PRIMARY KEY REFERENCES sessions(id) ON DELETE CASCADE,
                text       TEXT NOT NULL,
                done       INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            -- Prompt schedulers owned by the daemon. `loop` repeats every
            -- interval; `once` is removed after it fires.
            CREATE TABLE IF NOT EXISTS session_schedules (
                id            TEXT PRIMARY KEY,
                session_id    TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                kind          TEXT NOT NULL,
                prompt        TEXT NOT NULL,
                interval_secs INTEGER NOT NULL DEFAULT 0,
                next_at       INTEGER NOT NULL,
                created_at    INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS session_schedules_due
                ON session_schedules(next_at);

            -- Snapshot taken immediately before an undo rolls the workspace
            -- back, so redo can put those bytes back.
            CREATE TABLE IF NOT EXISTS undo_snapshots (
                session_id  TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                round       INTEGER NOT NULL,
                snapshot_id TEXT NOT NULL,
                PRIMARY KEY (session_id, round)
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

            -- Paired remote clients. The token itself is never stored, only
            -- its SHA-256 hash; `revoked_at` keeps an audit trail.
            CREATE TABLE IF NOT EXISTS client_tokens (
                id           TEXT PRIMARY KEY,
                name         TEXT NOT NULL,
                token_hash   TEXT NOT NULL UNIQUE,
                created_at   INTEGER NOT NULL,
                last_used_at INTEGER,
                revoked_at   INTEGER
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
        add_column(&conn, "sessions", "plan_model", "TEXT")?;
        add_column(&conn, "events", "undone", "INTEGER NOT NULL DEFAULT 0")?;
        migrate_session_modes(&conn)?;
        migrate_session_mode_names(&conn)?;
        drop_legacy_search_index(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            reader: None,
        })
    }

    pub fn create_session(&self, session: &Session) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO sessions
                (id, name, cwd, status, model, plan_model, parent_id, forked_from_seq, worktree, origin,
                 mode, agent, archived, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                session.id,
                session.name,
                session.cwd.to_string_lossy(),
                session.status.as_str(),
                session.model,
                session.plan_model,
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
        let conn = self.read_conn();
        let sql = if include_archived {
            "SELECT id, name, cwd, status, model, created_at, updated_at,
                    parent_id, forked_from_seq, worktree, origin, mode, agent, archived, plan_model
             FROM sessions ORDER BY updated_at DESC, created_at DESC"
        } else {
            "SELECT id, name, cwd, status, model, created_at, updated_at,
                    parent_id, forked_from_seq, worktree, origin, mode, agent, archived, plan_model
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
        let conn = self.read_conn();
        Ok(conn
            .query_row(
                "SELECT id, name, cwd, status, model, created_at, updated_at,
                        parent_id, forked_from_seq, worktree, origin, mode, agent, archived, plan_model
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    /// Change a session's persisted execution mode. Returns the updated row,
    /// or `None` when the session does not exist.
    pub fn set_session_mode(&self, id: &str, mode: SessionMode) -> Result<Option<Session>> {
        let conn = self.conn();
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
                        parent_id, forked_from_seq, worktree, origin, mode, agent, archived, plan_model
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    /// Change a session's pinned normal model. `None` falls back to the
    /// configured `execute` role, then the daemon default.
    pub fn set_session_model(&self, id: &str, model: Option<&str>) -> Result<Option<Session>> {
        let conn = self.conn();
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
                        parent_id, forked_from_seq, worktree, origin, mode, agent, archived, plan_model
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    /// Change a session's Plan-mode model. `None` falls back to the configured
    /// `plan` role, then the session's normal model.
    pub fn set_session_plan_model(&self, id: &str, model: Option<&str>) -> Result<Option<Session>> {
        let conn = self.conn();
        let changed = conn.execute(
            "UPDATE sessions SET plan_model = ?1, updated_at = ?2 WHERE id = ?3",
            params![model, now_ms(), id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(conn
            .query_row(
                "SELECT id, name, cwd, status, model, created_at, updated_at,
                        parent_id, forked_from_seq, worktree, origin, mode, agent, archived, plan_model
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    /// Change a session's primary agent. `None` restores the built-in default.
    pub fn set_session_agent(&self, id: &str, agent: Option<&str>) -> Result<Option<Session>> {
        let conn = self.conn();
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
                        parent_id, forked_from_seq, worktree, origin, mode, agent, archived, plan_model
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    /// Hide or restore a session without deleting its event log.
    pub fn set_session_archived(&self, id: &str, archived: bool) -> Result<Option<Session>> {
        let conn = self.conn();
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
                        parent_id, forked_from_seq, worktree, origin, mode, agent, archived, plan_model
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
        let conn = self.conn();
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
        let conn = self.conn();
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
        let mut conn = self.conn();
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
        let Some(cutoff) = self.last_visible_user_turn(id)? else {
            return Ok(None);
        };
        let round = self.next_rewind_round(id)?;
        Ok(Some(self.rewind_to(id, cutoff, round)?))
    }

    /// Allocate the next rewind round for a session. Rounds are shared by the
    /// `events.undone` marker and `undo_snapshots`, so a files-only rewind
    /// (which hides no events) still gets a distinct redo slot.
    pub fn next_rewind_round(&self, id: &str) -> Result<i64> {
        let conn = self.read_conn();
        let round: i64 = conn.query_row(
            "SELECT MAX(round) FROM (
                 SELECT COALESCE(MAX(undone), 0) AS round
                 FROM events WHERE session_id = ?1
                 UNION ALL
                 SELECT COALESCE(MAX(round), 0) AS round
                 FROM undo_snapshots WHERE session_id = ?1
             )",
            params![id],
            |row| row.get(0),
        )?;
        Ok(round + 1)
    }

    /// Hide every visible event at or after `cutoff` under `round`.
    pub fn rewind_to(&self, id: &str, cutoff: u64, round: i64) -> Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
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
        Ok(count)
    }

    /// Restore the most recent rewind batch.
    pub fn redo_last_undo(&self, id: &str) -> Result<Option<usize>> {
        let Some(round) = self.last_rewind_round(id)? else {
            return Ok(None);
        };
        Ok(Some(self.redo_round(id, round)?))
    }

    /// Unhide the events hidden under `round`. Files-only rounds have no such
    /// events, so this returns 0 and only the snapshot is restored.
    pub fn redo_round(&self, id: &str, round: i64) -> Result<usize> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let count = tx.execute(
            "UPDATE events SET undone = 0 WHERE session_id = ?1 AND undone = ?2",
            params![id, round],
        )?;
        tx.execute(
            "UPDATE sessions SET updated_at = ?1 WHERE id = ?2",
            params![now_ms(), id],
        )?;
        tx.commit()?;
        Ok(count)
    }

    /// Sequence of the newest visible user message, i.e. the first event an
    /// undo would hide. `None` when there is nothing left to undo.
    pub fn last_visible_user_turn(&self, id: &str) -> Result<Option<u64>> {
        let conn = self.read_conn();
        let seq: Option<i64> = conn
            .query_row(
                "SELECT seq FROM events
                 WHERE session_id = ?1 AND undone = 0 AND kind = 'user_message'
                 ORDER BY seq DESC LIMIT 1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(seq.map(|seq| seq as u64))
    }

    /// Whether `seq` is a visible user message, i.e. a valid rewind target.
    pub fn is_visible_user_turn(&self, id: &str, seq: u64) -> Result<bool> {
        let conn = self.read_conn();
        let found: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM events
                 WHERE session_id = ?1 AND seq = ?2 AND undone = 0
                   AND kind = 'user_message'",
                params![id, seq],
                |row| row.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Newest rewind round, or `None` when nothing has been rewound. Includes
    /// files-only rounds, which have a snapshot row but no hidden events.
    pub fn last_rewind_round(&self, id: &str) -> Result<Option<i64>> {
        let conn = self.read_conn();
        let round: i64 = conn.query_row(
            "SELECT MAX(round) FROM (
                 SELECT COALESCE(MAX(undone), 0) AS round
                 FROM events WHERE session_id = ?1
                 UNION ALL
                 SELECT COALESCE(MAX(round), 0) AS round
                 FROM undo_snapshots WHERE session_id = ?1
             )",
            params![id],
            |row| row.get(0),
        )?;
        Ok((round > 0).then_some(round))
    }

    /// Remember the pre-undo workspace snapshot for a round so redo can put
    /// the files back.
    pub fn record_undo_snapshot(&self, id: &str, round: i64, snapshot_id: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "INSERT OR REPLACE INTO undo_snapshots (session_id, round, snapshot_id)
             VALUES (?1, ?2, ?3)",
            params![id, round, snapshot_id],
        )?;
        Ok(())
    }

    /// Take (and forget) the snapshot recorded for an undo round.
    pub fn take_undo_snapshot(&self, id: &str, round: i64) -> Result<Option<String>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let snapshot: Option<String> = tx
            .query_row(
                "SELECT snapshot_id FROM undo_snapshots
                 WHERE session_id = ?1 AND round = ?2",
                params![id, round],
                |row| row.get(0),
            )
            .optional()?;
        if snapshot.is_some() {
            tx.execute(
                "DELETE FROM undo_snapshots WHERE session_id = ?1 AND round = ?2",
                params![id, round],
            )?;
        }
        tx.commit()?;
        Ok(snapshot)
    }

    pub fn goal(&self, id: &str) -> Result<Option<Goal>> {
        let conn = self.read_conn();
        conn.query_row(
            "SELECT text, done, created_at, updated_at FROM session_goals
             WHERE session_id = ?1",
            params![id],
            |row| {
                Ok(Goal {
                    text: row.get(0)?,
                    done: row.get::<_, i64>(1)? != 0,
                    created_at: row.get(2)?,
                    updated_at: row.get(3)?,
                })
            },
        )
        .optional()
        .map_err(Into::into)
    }

    /// Create or replace the session goal. Replacing reopens a completed goal.
    pub fn set_goal(&self, id: &str, text: &str) -> Result<Option<Goal>> {
        let now = now_ms();
        let conn = self.conn();
        let changed = conn.execute(
            "INSERT INTO session_goals (session_id, text, done, created_at, updated_at)
             VALUES (?1, ?2, 0, ?3, ?3)
             ON CONFLICT(session_id) DO UPDATE SET
                 text = excluded.text,
                 done = 0,
                 updated_at = excluded.updated_at",
            params![id, text, now],
        )?;
        drop(conn);
        if changed == 0 {
            return Ok(None);
        }
        self.goal(id)
    }

    pub fn set_goal_done(&self, id: &str, done: bool) -> Result<Option<Goal>> {
        let conn = self.conn();
        let changed = conn.execute(
            "UPDATE session_goals SET done = ?1, updated_at = ?2 WHERE session_id = ?3",
            params![done as i64, now_ms(), id],
        )?;
        drop(conn);
        if changed == 0 {
            return Ok(None);
        }
        self.goal(id)
    }

    pub fn clear_goal(&self, id: &str) -> Result<bool> {
        let conn = self.conn();
        let changed = conn.execute(
            "DELETE FROM session_goals WHERE session_id = ?1",
            params![id],
        )?;
        Ok(changed > 0)
    }

    pub fn add_schedule(
        &self,
        id: &str,
        session_id: &str,
        kind: &str,
        prompt: &str,
        interval_secs: u64,
        next_at: i64,
    ) -> Result<Option<Schedule>> {
        let created_at = now_ms();
        let conn = self.conn();
        let changed = conn.execute(
            "INSERT INTO session_schedules
                (id, session_id, kind, prompt, interval_secs, next_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                session_id,
                kind,
                prompt,
                interval_secs as i64,
                next_at,
                created_at
            ],
        )?;
        drop(conn);
        if changed == 0 {
            return Ok(None);
        }
        Ok(Some(Schedule {
            id: id.to_string(),
            session_id: session_id.to_string(),
            kind: kind.to_string(),
            prompt: prompt.to_string(),
            interval_secs,
            next_at,
            created_at,
        }))
    }

    pub fn list_schedules(&self, session_id: Option<&str>) -> Result<Vec<Schedule>> {
        let conn = self.read_conn();
        let mut statement = conn.prepare(
            "SELECT id, session_id, kind, prompt, interval_secs, next_at, created_at
             FROM session_schedules
             WHERE (?1 IS NULL OR session_id = ?1)
             ORDER BY next_at, created_at",
        )?;
        let rows = statement.query_map(params![session_id], row_to_schedule)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    /// Schedules whose fire time has passed, skipping archived sessions.
    pub fn due_schedules(&self, now: i64) -> Result<Vec<Schedule>> {
        let conn = self.read_conn();
        let mut statement = conn.prepare(
            "SELECT s.id, s.session_id, s.kind, s.prompt, s.interval_secs, s.next_at,
                    s.created_at
             FROM session_schedules s
             JOIN sessions ON sessions.id = s.session_id
             WHERE s.next_at <= ?1 AND sessions.archived = 0
             ORDER BY s.next_at",
        )?;
        let rows = statement.query_map(params![now], row_to_schedule)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(Into::into)
    }

    pub fn advance_schedule(&self, id: &str, next_at: i64) -> Result<bool> {
        let conn = self.conn();
        let changed = conn.execute(
            "UPDATE session_schedules SET next_at = ?1 WHERE id = ?2",
            params![next_at, id],
        )?;
        Ok(changed > 0)
    }

    pub fn remove_schedule(&self, id: &str) -> Result<bool> {
        let conn = self.conn();
        let changed = conn.execute("DELETE FROM session_schedules WHERE id = ?1", params![id])?;
        Ok(changed > 0)
    }

    pub fn remove_schedules(&self, session_id: &str, kind: Option<&str>) -> Result<usize> {
        let conn = self.conn();
        let changed = conn.execute(
            "DELETE FROM session_schedules
             WHERE session_id = ?1 AND (?2 IS NULL OR kind = ?2)",
            params![session_id, kind],
        )?;
        Ok(changed)
    }

    /// Highest event sequence in a session, or `None` when the session has no
    /// events (or does not exist).
    pub fn max_seq(&self, id: &str) -> Result<u64> {
        let conn = self.read_conn();
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
        let conn = self.conn();
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
        let conn = self.read_conn();
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    /// Whether the session has at least one user turn. Used by the idle
    /// auto-ready scanner to avoid queuing a pristine session.
    pub fn has_user_message(&self, session_id: &str) -> Result<bool> {
        let conn = self.read_conn();
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE session_id = ?1 AND kind = 'user_message'",
            params![session_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Unsent prompt for a session; `(text, updated_at)` with empty defaults.
    pub fn get_draft(&self, session_id: &str) -> Result<(String, i64)> {
        let conn = self.read_conn();
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
        let conn = self.conn();
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
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        struct ForkSource {
            cwd: String,
            model: Option<String>,
            plan_model: Option<String>,
            mode: String,
            agent: Option<String>,
        }
        let source: Option<ForkSource> = tx
            .query_row(
                "SELECT cwd, model, plan_model, mode, agent FROM sessions WHERE id = ?1",
                params![source_id],
                |row| {
                    Ok(ForkSource {
                        cwd: row.get(0)?,
                        model: row.get(1)?,
                        plan_model: row.get(2)?,
                        mode: row.get(3)?,
                        agent: row.get(4)?,
                    })
                },
            )
            .optional()?;
        let Some(ForkSource {
            cwd: source_cwd,
            model,
            plan_model,
            mode,
            agent,
        }) = source
        else {
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
        session.plan_model = plan_model;
        session.agent = agent;
        session.parent_id = Some(source_id.to_string());
        session.forked_from_seq = Some(upto as u64);
        session.worktree = worktree.map(Path::to_path_buf);
        session.origin = eix_core::SessionOrigin::Fork;
        session.archived = false;
        session.mode = mode.parse().unwrap_or_default();
        tx.execute(
            "INSERT INTO sessions
                (id, name, cwd, status, model, plan_model, parent_id, forked_from_seq, worktree, origin,
                 mode, agent, archived, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
            params![
                session.id,
                session.name,
                session.cwd.to_string_lossy(),
                session.status.as_str(),
                session.model,
                session.plan_model,
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
        let conn = self.conn();
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
        let conn = self.conn();
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
        let mut conn = self.conn();
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

    /// Async wrapper for backwards history pages. The TUI asks for these while
    /// scrolling, and a large page would otherwise run on a runtime thread.
    pub async fn events_before_async(
        self: &Arc<Self>,
        id: &str,
        before_seq: u64,
        limit: usize,
    ) -> Result<(Vec<Event>, bool)> {
        let store = Arc::clone(self);
        let id = id.to_string();
        tokio::task::spawn_blocking(move || store.events_before(&id, before_seq, limit))
            .await
            .context("event query task failed")?
    }

    /// Async wrapper for forward history pages.
    pub async fn events_after_page_async(
        self: &Arc<Self>,
        id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<(Vec<Event>, bool)> {
        let store = Arc::clone(self);
        let id = id.to_string();
        tokio::task::spawn_blocking(move || store.events_after_page(&id, after_seq, limit))
            .await
            .context("event query task failed")?
    }

    /// Async wrapper for kind-filtered reads (rewind scans snapshot events).
    pub async fn events_after_kind_async(
        self: &Arc<Self>,
        id: &str,
        after_seq: u64,
        kind: EventKind,
    ) -> Result<Vec<Event>> {
        let store = Arc::clone(self);
        let id = id.to_string();
        tokio::task::spawn_blocking(move || store.events_after_kind(&id, after_seq, kind))
            .await
            .context("event query task failed")?
    }

    pub async fn recent_user_messages_async(
        self: &Arc<Self>,
        id: &str,
        limit: usize,
    ) -> Result<Vec<Event>> {
        let store = Arc::clone(self);
        let id = id.to_string();
        tokio::task::spawn_blocking(move || store.recent_user_messages(&id, limit))
            .await
            .context("event query task failed")?
    }

    /// Async wrapper for daemon counters: the event `COUNT(*)` grows with the
    /// log, and `/stats` must not block a runtime thread.
    pub async fn stats_async(self: &Arc<Self>) -> Result<StoreStats> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.stats())
            .await
            .context("stats query task failed")?
    }

    /// Async wrapper for the blob-reference scan used by GC.
    pub async fn referenced_blob_ids_async(self: &Arc<Self>) -> Result<HashSet<String>> {
        let store = Arc::clone(self);
        tokio::task::spawn_blocking(move || store.referenced_blob_ids())
            .await
            .context("blob reference query task failed")?
    }

    pub fn events_after(&self, id: &str, after_seq: u64) -> Result<Vec<Event>> {
        let conn = self.read_conn();
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

    /// Fetch up to `limit` events newer than `after_seq`, in ascending order.
    /// The boolean is true when newer events were omitted.
    pub fn events_after_page(
        &self,
        id: &str,
        after_seq: u64,
        limit: usize,
    ) -> Result<(Vec<Event>, bool)> {
        let limit = limit.max(1);
        let fetch = limit.saturating_add(1).min(i64::MAX as usize) as i64;
        let after_seq = after_seq.min(i64::MAX as u64) as i64;
        let conn = self.read_conn();
        let mut stmt = conn.prepare(
            "SELECT seq, ts, kind, payload FROM events
             WHERE session_id = ?1 AND seq > ?2 AND undone = 0
             ORDER BY seq ASC LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![id, after_seq, fetch], |row| {
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
        Ok((events, has_more))
    }

    /// All events of one kind newer than `after_seq`. Used by rewind to find
    /// snapshots without scanning unrelated messages and tool output.
    pub fn events_after_kind(
        &self,
        id: &str,
        after_seq: u64,
        kind: EventKind,
    ) -> Result<Vec<Event>> {
        let after_seq = after_seq.min(i64::MAX as u64) as i64;
        let conn = self.read_conn();
        let mut stmt = conn.prepare(
            "SELECT seq, ts, kind, payload FROM events
             WHERE session_id = ?1 AND seq > ?2 AND kind = ?3 AND undone = 0
             ORDER BY seq ASC",
        )?;
        let rows = stmt.query_map(params![id, after_seq, kind.as_str()], |row| {
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
        let conn = self.read_conn();
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

    /// Newest user messages first, capped in SQL. The timeline UI only needs
    /// this event kind; loading the whole session just to filter it is waste.
    pub fn recent_user_messages(&self, id: &str, limit: usize) -> Result<Vec<Event>> {
        let limit = limit.max(1).min(i64::MAX as usize) as i64;
        let conn = self.read_conn();
        let mut stmt = conn.prepare(
            "SELECT seq, ts, kind, payload FROM events
             WHERE session_id = ?1 AND kind = 'user_message' AND undone = 0
             ORDER BY seq DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![id, limit], |row| {
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

    /// Sessions that were mid-turn when the daemon died. They cannot be resumed
    /// honestly, so the caller marks them as errored.
    pub fn running_sessions(&self) -> Result<Vec<Session>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, cwd, status, model, created_at, updated_at,
                    parent_id, forked_from_seq, worktree, origin, mode, agent, archived, plan_model
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
        let conn = self.read_conn();
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
        let conn = self.read_conn();
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
        let conn = self.read_conn();
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
        let conn = self.conn();
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
        let conn = self.conn();
        let changed = conn.execute(
            "DELETE FROM plugin_kv WHERE plugin = ?1 AND scope = ?2 AND key = ?3",
            params![plugin, scope, key],
        )?;
        Ok(changed > 0)
    }

    /// List keys (and values) for one plugin/scope. Values are included so a
    /// plugin can restore a small cursor cache without N round trips.
    pub fn plugin_kv_list(&self, plugin: &str, scope: &str) -> Result<Vec<(String, String)>> {
        let conn = self.read_conn();
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
        let conn = self.conn();
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
        let conn = self.read_conn();
        integration_row(&conn, session_id)
    }

    pub fn list_integrations(&self) -> Result<Vec<IntegrationState>> {
        let conn = self.read_conn();
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
        let conn = self.conn();
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
        let conn = self.read_conn();
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
        let conn = self.conn();
        conn.execute(
            "DELETE FROM session_integrations WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// Register a paired client. `token_hash` is the SHA-256 of the plaintext
    /// token, which the caller shows to the client exactly once.
    pub fn create_client_token(&self, name: &str, token_hash: &str) -> Result<ClientToken> {
        let id = format!("ct_{}", &Uuid::new_v4().simple().to_string()[..12]);
        let now = now_ms();
        self.conn().execute(
            "INSERT INTO client_tokens (id, name, token_hash, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, name, token_hash, now],
        )?;
        Ok(ClientToken {
            id,
            name: name.to_string(),
            created_at: now,
            last_used_at: None,
            revoked: false,
        })
    }

    /// Resolve an active token by hash and record its use. `None` means the
    /// token is unknown or revoked.
    pub fn use_client_token(&self, token_hash: &str) -> Result<Option<ClientToken>> {
        let conn = self.conn();
        let row = conn
            .query_row(
                "SELECT id, name, created_at, last_used_at, revoked_at
                 FROM client_tokens WHERE token_hash = ?1",
                params![token_hash],
                row_to_client_token,
            )
            .optional()?;
        let Some(token) = row else {
            return Ok(None);
        };
        if token.revoked {
            return Ok(None);
        }
        let now = now_ms();
        // HTTP authenticates on every request; do not turn that into a write
        // per request. A minute of granularity is enough for `pair list`.
        let recent = token
            .last_used_at
            .is_some_and(|last| now.saturating_sub(last) < 60_000);
        if !recent {
            conn.execute(
                "UPDATE client_tokens SET last_used_at = ?1 WHERE id = ?2",
                params![now, token.id],
            )?;
        }
        Ok(Some(ClientToken {
            last_used_at: Some(if recent {
                token.last_used_at.unwrap_or(now)
            } else {
                now
            }),
            ..token
        }))
    }

    pub fn list_client_tokens(&self) -> Result<Vec<ClientToken>> {
        let conn = self.conn();
        let mut statement = conn.prepare(
            "SELECT id, name, created_at, last_used_at, revoked_at
             FROM client_tokens ORDER BY created_at DESC",
        )?;
        let rows = statement.query_map([], row_to_client_token)?;
        let mut tokens = Vec::new();
        for row in rows {
            tokens.push(row?);
        }
        Ok(tokens)
    }

    pub fn revoke_client_token(&self, id: &str) -> Result<bool> {
        let changed = self.conn().execute(
            "UPDATE client_tokens SET revoked_at = ?1
             WHERE id = ?2 AND revoked_at IS NULL",
            params![now_ms(), id],
        )?;
        Ok(changed > 0)
    }
}

fn row_to_client_token(row: &Row<'_>) -> rusqlite::Result<ClientToken> {
    Ok(ClientToken {
        id: row.get(0)?,
        name: row.get(1)?,
        created_at: row.get(2)?,
        last_used_at: row.get(3)?,
        revoked: row.get::<_, Option<i64>>(4)?.is_some(),
    })
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

fn row_to_schedule(row: &Row<'_>) -> rusqlite::Result<Schedule> {
    Ok(Schedule {
        id: row.get(0)?,
        session_id: row.get(1)?,
        kind: row.get(2)?,
        prompt: row.get(3)?,
        interval_secs: row.get::<_, i64>(4)?.max(0) as u64,
        next_at: row.get(5)?,
        created_at: row.get(6)?,
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
        plan_model: row.get(14)?,
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
                 WHEN permission IN ('full', 'bypass') THEN 'full'
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

/// `bypass` was renamed to `full` with no parsing alias, so rewrite stored
/// values once. Without this an old session would silently drop to `build`
/// because the mode column falls back to the default when it cannot parse.
fn migrate_session_mode_names(conn: &Connection) -> Result<()> {
    let migrated: Option<String> = conn
        .query_row(
            "SELECT value FROM schema_meta WHERE key = 'sessions.mode.v2'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if migrated.is_some() {
        return Ok(());
    }
    conn.execute(
        "UPDATE sessions SET mode = 'full' WHERE mode = 'bypass'",
        [],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO schema_meta (key, value) VALUES ('sessions.mode.v2', '1')",
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
    fn file_store_reads_committed_writes_through_reader() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("eix-store-reader-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open(&dir.join("eix.sqlite3")).unwrap();
        assert!(store.reader.is_some(), "file stores should use a reader");

        store.create_session(&session("s1")).unwrap();
        store
            .append_event(
                "s1",
                EventKind::UserMessage,
                serde_json::json!({ "text": "hi" }),
            )
            .unwrap();

        // Both go through the read-only connection.
        assert!(store.get_session("s1").unwrap().is_some());
        assert_eq!(store.events("s1").unwrap().len(), 1);

        // A write on the reader must fail rather than race the writer.
        {
            let reader = store.reader.as_ref().unwrap();
            let guard = reader.lock().unwrap();
            assert!(guard.execute("DELETE FROM sessions", []).is_err());
        }

        // The writer remains usable after the reader observed the committed row.
        store
            .append_event(
                "s1",
                EventKind::UserMessage,
                serde_json::json!({ "text": "again" }),
            )
            .unwrap();
        assert_eq!(store.events("s1").unwrap().len(), 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mode_round_trips_and_fork_inherits_it() {
        let store = Store::in_memory().unwrap();
        let mut parent = session("s1");
        parent.mode = SessionMode::Full;
        store.create_session(&parent).unwrap();
        assert_eq!(
            store.get_session("s1").unwrap().unwrap().mode,
            SessionMode::Full
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
    fn plan_model_round_trips_and_fork_inherits_it() {
        let store = Store::in_memory().unwrap();
        let mut parent = session("s1");
        parent.plan_model = Some("local/planner".into());
        store.create_session(&parent).unwrap();
        assert_eq!(
            store
                .get_session("s1")
                .unwrap()
                .unwrap()
                .plan_model
                .as_deref(),
            Some("local/planner")
        );

        let updated = store
            .set_session_plan_model("s1", Some("local/planner#high"))
            .unwrap()
            .expect("updated row");
        assert_eq!(updated.plan_model.as_deref(), Some("local/planner#high"));

        let forked = store
            .fork_session("s1", None, "fork", None, None)
            .unwrap()
            .expect("fork");
        assert_eq!(forked.plan_model.as_deref(), Some("local/planner#high"));

        let cleared = store
            .set_session_plan_model("s1", None)
            .unwrap()
            .expect("cleared row");
        assert_eq!(cleared.plan_model, None);

        assert!(store
            .set_session_plan_model("missing", Some("local/planner"))
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
        assert_eq!(by_id("full").mode, SessionMode::Full);

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
    fn stored_bypass_modes_become_full_once() {
        let store = Store::init(Connection::open_in_memory().unwrap()).unwrap();
        let mut legacy = session("s1");
        legacy.mode = SessionMode::Full;
        store.create_session(&legacy).unwrap();
        {
            let conn = store.conn.lock().unwrap();
            conn.execute("UPDATE sessions SET mode = 'bypass' WHERE id = 's1'", [])
                .unwrap();
            // Pretend the database predates the rename.
            conn.execute("DELETE FROM schema_meta WHERE key = 'sessions.mode.v2'", [])
                .unwrap();
            migrate_session_mode_names(&conn).unwrap();
            let mode: String = conn
                .query_row("SELECT mode FROM sessions WHERE id = 's1'", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(mode, "full");
            // The marker keeps a later startup from touching a newer value.
            conn.execute("UPDATE sessions SET mode = 'plan' WHERE id = 's1'", [])
                .unwrap();
            migrate_session_mode_names(&conn).unwrap();
            let mode: String = conn
                .query_row("SELECT mode FROM sessions WHERE id = 's1'", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(mode, "plan");
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
    fn events_after_page_and_kind_filter_limit_in_sql() {
        let store = Store::in_memory().unwrap();
        store.create_session(&session("s1")).unwrap();
        for kind in [
            EventKind::UserMessage,
            EventKind::Snapshot,
            EventKind::UserMessage,
            EventKind::Snapshot,
            EventKind::UserMessage,
        ] {
            store
                .append_event("s1", kind, serde_json::json!({}))
                .unwrap();
        }

        let (page, has_more) = store.events_after_page("s1", 0, 2).unwrap();
        assert_eq!(
            page.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert!(has_more);

        let (page, has_more) = store.events_after_page("s1", 4, 2).unwrap();
        assert_eq!(
            page.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![5]
        );
        assert!(!has_more);

        let snapshots = store
            .events_after_kind("s1", 0, EventKind::Snapshot)
            .unwrap();
        assert_eq!(
            snapshots.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![2, 4]
        );
        let snapshots = store
            .events_after_kind("s1", 2, EventKind::Snapshot)
            .unwrap();
        assert_eq!(
            snapshots.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![4]
        );
    }

    #[test]
    fn recent_user_messages_filters_and_limits_in_sql() {
        let store = Store::in_memory().unwrap();
        store.create_session(&session("s1")).unwrap();
        for kind in [
            EventKind::UserMessage,
            EventKind::AssistantMessage,
            EventKind::UserMessage,
            EventKind::UserMessage,
        ] {
            store
                .append_event("s1", kind, serde_json::json!({"text": "m"}))
                .unwrap();
        }

        let newest = store.recent_user_messages("s1", 1).unwrap();
        assert_eq!(newest.len(), 1);
        assert_eq!(newest[0].seq, 4);

        store.undo_last_turn("s1").unwrap();
        let visible = store.recent_user_messages("s1", 10).unwrap();
        assert_eq!(
            visible.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![3, 1]
        );

        store.redo_last_undo("s1").unwrap();
        let restored = store.recent_user_messages("s1", 2).unwrap();
        assert_eq!(
            restored.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![4, 3]
        );
    }

    #[test]
    fn rewind_to_any_turn_and_redo_restores_the_range() {
        let store = Store::in_memory().unwrap();
        store.create_session(&session("s1")).unwrap();
        let mut turns = Vec::new();
        for i in 0..3 {
            let user = store
                .append_event(
                    "s1",
                    EventKind::UserMessage,
                    serde_json::json!({"text": format!("m{i}")}),
                )
                .unwrap();
            turns.push(user.seq);
            store
                .append_event(
                    "s1",
                    EventKind::AssistantMessage,
                    serde_json::json!({"text": "a"}),
                )
                .unwrap();
        }
        assert!(store.is_visible_user_turn("s1", turns[1]).unwrap());
        assert!(!store.is_visible_user_turn("s1", turns[1] + 1).unwrap());

        let round = store.next_rewind_round("s1").unwrap();
        assert_eq!(round, 1);
        // The second user turn and everything after it.
        assert_eq!(store.rewind_to("s1", turns[1], round).unwrap(), 4);
        assert_eq!(store.last_rewind_round("s1").unwrap(), Some(1));
        assert!(store.recent_user_messages("s1", 10).unwrap().len() == 1);

        assert_eq!(store.redo_round("s1", round).unwrap(), 4);
        assert_eq!(store.recent_user_messages("s1", 10).unwrap().len(), 3);
        assert_eq!(store.last_rewind_round("s1").unwrap(), None);
        assert_eq!(store.next_rewind_round("s1").unwrap(), 1);
    }

    #[test]
    fn files_only_rewind_rounds_reserve_a_slot_and_can_be_redone() {
        let store = Store::in_memory().unwrap();
        store.create_session(&session("s1")).unwrap();
        let round = store.next_rewind_round("s1").unwrap();
        store.record_undo_snapshot("s1", round, "snap-a").unwrap();
        assert_eq!(store.last_rewind_round("s1").unwrap(), Some(round));
        // A files-only round hides no events, but still advances the counter.
        assert_eq!(store.next_rewind_round("s1").unwrap(), round + 1);
        assert_eq!(store.redo_round("s1", round).unwrap(), 0);
        assert_eq!(
            store.take_undo_snapshot("s1", round).unwrap().as_deref(),
            Some("snap-a")
        );
        assert_eq!(store.last_rewind_round("s1").unwrap(), None);
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
            let mut conn = store.conn();
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

    #[test]
    fn client_tokens_are_hashed_and_revocable() {
        let store = Store::in_memory().unwrap();
        let hash_a = "a".repeat(64);
        let hash_b = "b".repeat(64);

        let first = store.create_client_token("laptop", &hash_a).unwrap();
        let second = store.create_client_token("phone", &hash_b).unwrap();
        assert!(first.id.starts_with("ct_"));
        assert_ne!(first.id, second.id);
        assert_eq!(first.name, "laptop");
        assert!(!first.revoked);
        assert!(first.last_used_at.is_none());

        // The plaintext never touches the DB: unknown hashes do not resolve.
        assert!(store.use_client_token("not-a-hash").unwrap().is_none());
        let used = store.use_client_token(&hash_a).unwrap().unwrap();
        assert_eq!(used.id, first.id);
        assert!(used.last_used_at.is_some());

        let listed = store.list_client_tokens().unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(
            listed.iter().filter(|token| !token.revoked).count(),
            2,
            "new tokens start active"
        );

        assert!(store.revoke_client_token(&first.id).unwrap());
        assert!(
            !store.revoke_client_token(&first.id).unwrap(),
            "revoking twice is a no-op"
        );
        assert!(store.use_client_token(&hash_a).unwrap().is_none());
        assert!(store.use_client_token(&hash_b).unwrap().is_some());
        let listed = store.list_client_tokens().unwrap();
        assert!(
            listed
                .iter()
                .find(|token| token.id == first.id)
                .unwrap()
                .revoked
        );
        // A revoked row keeps its audit trail rather than disappearing.
        assert_eq!(listed.len(), 2);

        // Hash uniqueness is enforced by the schema.
        assert!(store.create_client_token("dupe", &hash_b).is_err());
    }

    #[test]
    fn goals_are_replaced_completed_and_cleared() {
        let store = Store::in_memory().unwrap();
        store.create_session(&session("s1")).unwrap();
        assert!(store.goal("s1").unwrap().is_none());

        let goal = store.set_goal("s1", "ship the release").unwrap().unwrap();
        assert_eq!(goal.text, "ship the release");
        assert!(!goal.done);

        let goal = store.set_goal_done("s1", true).unwrap().unwrap();
        assert!(goal.done);

        // Replacing the text reopens the goal.
        let goal = store.set_goal("s1", "ship it twice").unwrap().unwrap();
        assert!(!goal.done);
        assert_eq!(goal.text, "ship it twice");

        assert!(store.clear_goal("s1").unwrap());
        assert!(store.goal("s1").unwrap().is_none());
        assert!(!store.clear_goal("s1").unwrap());
    }

    #[test]
    fn schedules_round_trip_and_only_due_rows_fire() {
        let store = Store::in_memory().unwrap();
        store.create_session(&session("s1")).unwrap();

        let repeating = store
            .add_schedule("a", "s1", "loop", "check the build", 10, 100)
            .unwrap()
            .unwrap();
        assert_eq!(repeating.interval_secs, 10);
        assert_eq!(store.list_schedules(Some("s1")).unwrap().len(), 1);
        assert!(store.due_schedules(99).unwrap().is_empty());
        assert_eq!(store.due_schedules(100).unwrap().len(), 1);

        store.advance_schedule("a", 200).unwrap();
        store
            .add_schedule("b", "s1", "once", "say hi", 0, 50)
            .unwrap();
        let due = store.due_schedules(150).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].id, "b");

        assert!(store.remove_schedule("b").unwrap());
        assert_eq!(store.remove_schedules("s1", Some("loop")).unwrap(), 1);
        assert!(store.list_schedules(Some("s1")).unwrap().is_empty());
        assert!(!store.remove_schedule("missing").unwrap());
    }

    #[test]
    fn undo_snapshots_are_taken_once_per_round() {
        let store = Store::in_memory().unwrap();
        store.create_session(&session("s1")).unwrap();
        assert!(store.take_undo_snapshot("s1", 1).unwrap().is_none());

        store.record_undo_snapshot("s1", 1, "snap-a").unwrap();
        assert_eq!(
            store.take_undo_snapshot("s1", 1).unwrap().as_deref(),
            Some("snap-a")
        );
        // Taking it forgets the mapping so a second redo cannot replay it.
        assert!(store.take_undo_snapshot("s1", 1).unwrap().is_none());
    }
}
