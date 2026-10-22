//! SQLite-backed session registry and append-only event log.
//!
//! For Phase 0 the log is the database: messages are projected from
//! `user_message` / `assistant_message` events, so fork can copy a prefix and
//! search can be added later without migrating the write path.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use eix_core::{now_ms, Event, EventKind, PermissionLevel, SearchHit, Session, SessionStatus};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

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
                permission TEXT NOT NULL DEFAULT 'workspace-write',
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

            -- Search index. `search_docs` is the content table (so snippets and
            -- metadata come from normal columns); `search_fts` mirrors its text.
            CREATE TABLE IF NOT EXISTS search_docs (
                doc_id     INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                seq        INTEGER NOT NULL,
                role       TEXT NOT NULL,
                ts         INTEGER NOT NULL,
                text       TEXT NOT NULL,
                UNIQUE (session_id, seq)
            );

            CREATE VIRTUAL TABLE IF NOT EXISTS search_fts USING fts5(
                text,
                content='search_docs',
                content_rowid='doc_id',
                tokenize='trigram'
            );

            CREATE TRIGGER IF NOT EXISTS search_docs_ai AFTER INSERT ON search_docs BEGIN
                INSERT INTO search_fts(rowid, text) VALUES (new.doc_id, new.text);
            END;

            CREATE TRIGGER IF NOT EXISTS search_docs_ad AFTER DELETE ON search_docs BEGIN
                INSERT INTO search_fts(search_fts, rowid, text)
                VALUES ('delete', old.doc_id, old.text);
            END;

            CREATE TRIGGER IF NOT EXISTS search_docs_au AFTER UPDATE ON search_docs BEGIN
                INSERT INTO search_fts(search_fts, rowid, text)
                VALUES ('delete', old.doc_id, old.text);
                INSERT INTO search_fts(rowid, text) VALUES (new.doc_id, new.text);
            END;
            "#,
        )?;
        // Databases created before fork/rewind lack the lineage columns.
        add_column(&conn, "sessions", "parent_id", "TEXT")?;
        add_column(&conn, "sessions", "forked_from_seq", "INTEGER")?;
        add_column(&conn, "sessions", "worktree", "TEXT")?;
        add_column(&conn, "sessions", "origin", "TEXT NOT NULL DEFAULT 'user'")?;
        add_column(
            &conn,
            "sessions",
            "permission",
            "TEXT NOT NULL DEFAULT 'workspace-write'",
        )?;
        add_column(&conn, "sessions", "agent", "TEXT")?;
        add_column(&conn, "sessions", "archived", "INTEGER NOT NULL DEFAULT 0")?;
        add_column(&conn, "events", "undone", "INTEGER NOT NULL DEFAULT 0")?;
        // Index messages written before the search tables existed. New events
        // are indexed by `append_event`; the UNIQUE key makes this a no-op on
        // later starts (only the scan cost remains).
        conn.execute(
            "INSERT OR IGNORE INTO search_docs (session_id, seq, role, ts, text)
             SELECT session_id, seq,
                    CASE kind WHEN 'user_message' THEN 'user' ELSE 'assistant' END,
                    ts,
                    json_extract(payload, '$.text')
             FROM events
             WHERE kind IN ('user_message', 'assistant_message')
               AND COALESCE(json_extract(payload, '$.text'), '') <> ''",
            [],
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn create_session(&self, session: &Session) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "INSERT INTO sessions
                (id, name, cwd, status, model, parent_id, forked_from_seq, worktree, origin,
                 permission, agent, archived, created_at, updated_at)
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
                session.permission.as_str(),
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
                    parent_id, forked_from_seq, worktree, origin, permission, agent, archived
             FROM sessions ORDER BY updated_at DESC, created_at DESC"
        } else {
            "SELECT id, name, cwd, status, model, created_at, updated_at,
                    parent_id, forked_from_seq, worktree, origin, permission, agent, archived
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
                        parent_id, forked_from_seq, worktree, origin, permission, agent, archived
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    /// Change a session's persisted permission level. Returns the updated row,
    /// or `None` when the session does not exist.
    pub fn set_session_permission(
        &self,
        id: &str,
        permission: PermissionLevel,
    ) -> Result<Option<Session>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let changed = conn.execute(
            "UPDATE sessions SET permission = ?1, updated_at = ?2 WHERE id = ?3",
            params![permission.as_str(), now_ms(), id],
        )?;
        if changed == 0 {
            return Ok(None);
        }
        Ok(conn
            .query_row(
                "SELECT id, name, cwd, status, model, created_at, updated_at,
                        parent_id, forked_from_seq, worktree, origin, permission, agent, archived
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
                        parent_id, forked_from_seq, worktree, origin, permission, agent, archived
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
                        parent_id, forked_from_seq, worktree, origin, permission, agent, archived
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
                        parent_id, forked_from_seq, worktree, origin, permission, agent, archived
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    /// Delete a session and (via `ON DELETE CASCADE`) its events, draft and
    /// search rows. Returns false when the session did not exist.
    pub fn delete_session(&self, id: &str) -> Result<bool> {
        let conn = self.conn.lock().expect("store mutex poisoned");
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
        if matches!(
            event.kind,
            EventKind::UserMessage | EventKind::AssistantMessage
        ) {
            let text = event
                .payload
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            if !text.is_empty() {
                let role = if event.kind == EventKind::UserMessage {
                    "user"
                } else {
                    "assistant"
                };
                tx.execute(
                    "INSERT OR IGNORE INTO search_docs (session_id, seq, role, ts, text)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![event.session_id, event.seq as i64, role, event.ts, text],
                )?;
            }
        }
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
        reindex_search(&tx, id)?;
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
        reindex_search(&tx, id)?;
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
                "SELECT cwd, model, permission, agent FROM sessions WHERE id = ?1",
                params![source_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let Some((source_cwd, model, permission, agent)) = source else {
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
        session.permission = permission.parse().unwrap_or_default();
        tx.execute(
            "INSERT INTO sessions
                (id, name, cwd, status, model, parent_id, forked_from_seq, worktree, origin,
                 permission, agent, archived, created_at, updated_at)
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
                session.permission.as_str(),
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
        // Keep the search index in sync without re-parsing events: messages in
        // the prefix are already indexed under the source.
        tx.execute(
            "INSERT OR IGNORE INTO search_docs (session_id, seq, role, ts, text)
             SELECT ?1, seq, role, ts, text FROM search_docs
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
        if matches!(kind, EventKind::UserMessage | EventKind::AssistantMessage) {
            let text = payload
                .get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            if !text.is_empty() {
                let role = if kind == EventKind::UserMessage {
                    "user"
                } else {
                    "assistant"
                };
                tx.execute(
                    "INSERT OR IGNORE INTO search_docs (session_id, seq, role, ts, text)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, seq, role, ts, text],
                )?;
            }
        }
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

    /// Search user and assistant message text across sessions.
    ///
    /// Queries containing a term of three or more characters use the FTS5
    /// trigram index; otherwise (short queries, which Chinese words often are)
    /// this falls back to a case-insensitive substring scan. `limit` is clamped
    /// to `1..=100`.
    pub fn search(
        &self,
        query: &str,
        session_id: Option<&str>,
        limit: u32,
    ) -> Result<Vec<SearchHit>> {
        let query = query.trim();
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let limit = limit.clamp(1, 100) as i64;
        let conn = self.conn.lock().expect("store mutex poisoned");

        let fts_terms: Vec<&str> = query
            .split_whitespace()
            .filter(|term| term.chars().count() >= 3)
            .collect();

        let rows = if fts_terms.is_empty() {
            let mut stmt = conn.prepare(
                "SELECT d.session_id, s.name, d.seq, d.role, d.ts, d.text
                 FROM search_docs d
                 JOIN sessions s ON s.id = d.session_id
                 WHERE instr(lower(d.text), lower(?1)) > 0
                   AND (?2 IS NULL OR d.session_id = ?2)
                 ORDER BY d.ts DESC
                 LIMIT ?3",
            )?;
            let mapped =
                stmt.query_map(params![query, session_id, limit], |row| map_hit(row, query))?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            // Quote every term so user punctuation cannot be read as FTS5
            // syntax; `"a" "b"` is an implicit AND.
            let expression = fts_terms
                .iter()
                .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" ");
            let mut stmt = conn.prepare(
                "SELECT d.session_id, s.name, d.seq, d.role, d.ts, d.text
                 FROM search_fts
                 JOIN search_docs d ON d.doc_id = search_fts.rowid
                 JOIN sessions s ON s.id = d.session_id
                 WHERE search_fts MATCH ?1
                   AND (?2 IS NULL OR d.session_id = ?2)
                 ORDER BY bm25(search_fts), d.ts DESC
                 LIMIT ?3",
            )?;
            let mapped = stmt.query_map(params![expression, session_id, limit], |row| {
                map_hit(row, query)
            })?;
            mapped.collect::<rusqlite::Result<Vec<_>>>()?
        };
        Ok(rows)
    }

    /// Sessions that were mid-turn when the daemon died. They cannot be resumed
    /// honestly, so the caller marks them as errored.
    pub fn running_sessions(&self) -> Result<Vec<Session>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, name, cwd, status, model, created_at, updated_at,
                    parent_id, forked_from_seq, worktree, origin, permission, agent, archived
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

/// Rebuild `search_docs` for one session from the events that are still
/// visible. Undo/redo change visibility, so the FTS index has to follow.
fn reindex_search(tx: &rusqlite::Transaction<'_>, session_id: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM search_docs WHERE session_id = ?1",
        params![session_id],
    )?;
    tx.execute(
        "INSERT INTO search_docs (session_id, seq, role, ts, text)
         SELECT session_id, seq,
                CASE kind WHEN 'user_message' THEN 'user' ELSE 'assistant' END,
                ts,
                json_extract(payload, '$.text')
         FROM events
         WHERE session_id = ?1 AND undone = 0
           AND kind IN ('user_message', 'assistant_message')
           AND COALESCE(json_extract(payload, '$.text'), '') <> ''",
        params![session_id],
    )?;
    Ok(())
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
        permission: row
            .get::<_, Option<String>>(11)?
            .and_then(|value| value.parse().ok())
            .unwrap_or_default(),
        agent: row.get(12)?,
        archived: row.get::<_, i64>(13)? != 0,
    })
}

fn map_hit(row: &Row<'_>, query: &str) -> rusqlite::Result<SearchHit> {
    let text: String = row.get(5)?;
    Ok(SearchHit {
        session_id: row.get(0)?,
        session_name: row.get(1)?,
        seq: row.get::<_, i64>(2)? as u64,
        role: row.get(3)?,
        ts: row.get(4)?,
        snippet: snippet(&text, query),
    })
}

/// A short window around the first match. Character-based so mixed CJK text
/// and case folding cannot produce invalid byte offsets; `…` marks trimmed
/// ends.
fn snippet(text: &str, query: &str) -> String {
    const BEFORE: usize = 40;
    const AFTER: usize = 120;
    const HEAD: usize = 160;
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let hay: Vec<char> = flat.chars().collect();
    let needles: Vec<Vec<char>> = std::iter::once(query.to_string())
        .chain(query.split_whitespace().map(str::to_string))
        .map(|needle| needle.chars().collect())
        .collect();
    let Some((position, length)) = needles
        .iter()
        .find_map(|needle| find_ci(&hay, needle).map(|position| (position, needle.len())))
    else {
        let mut head: String = hay.iter().take(HEAD).collect();
        if hay.len() > HEAD {
            head.push('…');
        }
        return head;
    };
    let start = position.saturating_sub(BEFORE);
    let end = (position + length + AFTER).min(hay.len());
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    out.extend(&hay[start..end]);
    if end < hay.len() {
        out.push('…');
    }
    out
}

/// First index where `needle` occurs, ASCII case-insensitively.
fn find_ci(hay: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&start| {
        hay[start..start + needle.len()]
            .iter()
            .zip(needle)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

/// Add a column to an existing table (SQLite has no `ADD COLUMN IF NOT
/// EXISTS`). Used for migrations that must not break existing databases.
fn add_column(conn: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let columns: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<_>>()?;
    if !columns.iter().any(|name| name == column) {
        conn.execute(
            &format!("ALTER TABLE {table} ADD COLUMN {column} {decl}"),
            [],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> Session {
        Session::new(id, "test", "/tmp")
    }

    #[test]
    fn permission_round_trips_and_fork_inherits_it() {
        let store = Store::in_memory().unwrap();
        let mut parent = session("s1");
        parent.permission = PermissionLevel::ReadOnly;
        store.create_session(&parent).unwrap();
        assert_eq!(
            store.get_session("s1").unwrap().unwrap().permission,
            PermissionLevel::ReadOnly
        );

        let updated = store
            .set_session_permission("s1", PermissionLevel::Full)
            .unwrap()
            .expect("updated row");
        assert_eq!(updated.permission, PermissionLevel::Full);

        let forked = store
            .fork_session("s1", None, "fork", None, None)
            .unwrap()
            .expect("fork");
        assert_eq!(forked.permission, PermissionLevel::Full);

        assert!(store
            .set_session_permission("missing", PermissionLevel::ReadOnly)
            .unwrap()
            .is_none());
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
        // The search index is copied with the prefix, not rebuilt later.
        let hits = store.search("m0", Some(&fork.id), 20).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].seq, 1);
        assert!(store.search("m1", Some(&fork.id), 20).unwrap().is_empty());

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
    fn search_finds_messages_and_falls_back_for_short_queries() {
        let store = Store::in_memory().unwrap();
        store
            .create_session(&Session::new("s1", "alpha", "/tmp"))
            .unwrap();
        store
            .create_session(&Session::new("s2", "beta", "/tmp"))
            .unwrap();
        store
            .append_event(
                "s1",
                EventKind::UserMessage,
                serde_json::json!({"text": "hello world 你好"}),
            )
            .unwrap();
        store
            .append_event(
                "s1",
                EventKind::AssistantMessage,
                serde_json::json!({"text": "the request timeout is 30s"}),
            )
            .unwrap();
        store
            .append_event(
                "s2",
                EventKind::UserMessage,
                serde_json::json!({"text": "中文检索测试"}),
            )
            .unwrap();

        // Three or more characters go through the FTS5 trigram index.
        let hits = store.search("timeout", None, 20).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_name, "alpha");
        assert_eq!(hits[0].role, "assistant");
        assert_eq!(hits[0].seq, 2);
        assert!(hits[0].snippet.contains("timeout"), "{}", hits[0].snippet);

        // Multiple terms are ANDed and matched case-insensitively.
        let hits = store.search("Request TIMEOUT", None, 20).unwrap();
        assert_eq!(hits.len(), 1);

        // Short queries (common for Chinese) fall back to substring scanning.
        let hits = store.search("检索", None, 20).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].session_name, "beta");
        assert_eq!(hits[0].role, "user");
        let hits = store.search("中文检索", None, 20).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(store.search("你好", None, 20).unwrap().len(), 1);

        // Session scoping, limits, and non-matches.
        assert!(store.search("timeout", Some("s2"), 20).unwrap().is_empty());
        assert_eq!(store.search("hello", Some("s1"), 1).unwrap().len(), 1);
        assert!(store.search("missing", None, 20).unwrap().is_empty());
        // Punctuation is literal, not FTS5 syntax; blank queries match nothing.
        assert!(store.search("\"", None, 20).unwrap().is_empty());
        assert!(store.search("   ", None, 20).unwrap().is_empty());

        // The snippet keeps context around the match for long messages.
        let long = "x".repeat(300);
        store
            .append_event(
                "s1",
                EventKind::UserMessage,
                serde_json::json!({"text": format!("{long} needle {long}")}),
            )
            .unwrap();
        let hits = store.search("needle", None, 20).unwrap();
        assert_eq!(hits.len(), 1);
        assert!(hits[0].snippet.starts_with('…'), "{}", hits[0].snippet);
        assert!(hits[0].snippet.ends_with('…'), "{}", hits[0].snippet);
        assert!(hits[0].snippet.contains("needle"));
    }
}
