//! SQLite-backed session registry and append-only event log.
//!
//! For Phase 0 the log is the database: messages are projected from
//! `user_message` / `assistant_message` events, so rewind/fork/search can be
//! added later without migrating the write path.

use std::fs;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use eix_core::{
    now_ms, AssistantMessagePayload, ChatMessage, Event, EventKind, Session, SessionStatus,
    ToolResultPayload, UserMessagePayload,
};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

pub struct Store {
    conn: Mutex<Connection>,
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
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS events (
                session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                seq        INTEGER NOT NULL,
                ts         INTEGER NOT NULL,
                kind       TEXT NOT NULL,
                payload    TEXT NOT NULL,
                PRIMARY KEY (session_id, seq)
            );

            CREATE INDEX IF NOT EXISTS events_session_seq ON events(session_id, seq);
            "#,
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn create_session(&self, session: &Session) -> Result<()> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        conn.execute(
            "INSERT INTO sessions (id, name, cwd, status, model, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                session.id,
                session.name,
                session.cwd.to_string_lossy(),
                session.status.as_str(),
                session.model,
                session.created_at,
                session.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, name, cwd, status, model, created_at, updated_at
             FROM sessions ORDER BY updated_at DESC, created_at DESC",
        )?;
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
                "SELECT id, name, cwd, status, model, created_at, updated_at
                 FROM sessions WHERE id = ?1",
                params![id],
                row_to_session,
            )
            .optional()?)
    }

    pub fn session_count(&self) -> Result<usize> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        Ok(count as usize)
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

    pub fn events_after(&self, id: &str, after_seq: u64) -> Result<Vec<Event>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT seq, ts, kind, payload FROM events
             WHERE session_id = ?1 AND seq > ?2 ORDER BY seq ASC",
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

    /// Rebuild provider context from the event log. Only complete turns are
    /// included; cancelled partials are still honest assistant messages.
    pub fn messages(&self, id: &str) -> Result<Vec<ChatMessage>> {
        let events = self.events(id)?;
        let mut messages = Vec::new();
        for event in events {
            match event.kind {
                EventKind::UserMessage => {
                    if let Ok(payload) = serde_json::from_value::<UserMessagePayload>(event.payload)
                    {
                        messages.push(ChatMessage::user(payload.text));
                    }
                }
                EventKind::AssistantMessage => {
                    if let Ok(payload) =
                        serde_json::from_value::<AssistantMessagePayload>(event.payload)
                    {
                        if !payload.text.is_empty() || !payload.tool_calls.is_empty() {
                            messages.push(ChatMessage::assistant_with_tools(
                                payload.text,
                                payload.tool_calls,
                            ));
                        }
                    }
                }
                EventKind::ToolResult => {
                    if let Ok(payload) = serde_json::from_value::<ToolResultPayload>(event.payload)
                    {
                        messages.push(ChatMessage::tool(payload.call_id, payload.output));
                    }
                }
                _ => {}
            }
        }
        Ok(messages)
    }

    /// Sessions that were mid-turn when the daemon died. They cannot be resumed
    /// honestly, so the caller marks them as errored.
    pub fn running_sessions(&self) -> Result<Vec<Session>> {
        let conn = self.conn.lock().expect("store mutex poisoned");
        let mut stmt = conn.prepare(
            "SELECT id, name, cwd, status, model, created_at, updated_at
             FROM sessions WHERE status IN ('running', 'awaiting_approval', 'awaiting_input')",
        )?;
        let rows = stmt.query_map([], row_to_session)?;
        let mut sessions = Vec::new();
        for row in rows {
            sessions.push(row?);
        }
        Ok(sessions)
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str) -> Session {
        Session::new(id, "test", "/tmp")
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
        assert_eq!(events[1].payload["text"], "hello");

        let messages = store.messages("s1").unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].content, "hi");
        assert_eq!(messages[1].content, "hello");
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
    fn messages_reconstruct_tool_calls() {
        let store = Store::in_memory().unwrap();
        store.create_session(&session("s1")).unwrap();
        store
            .append_event(
                "s1",
                EventKind::UserMessage,
                serde_json::json!({"text": "read it"}),
            )
            .unwrap();
        store
            .append_event(
                "s1",
                EventKind::AssistantMessage,
                serde_json::json!({
                    "text": "",
                    "model": "m",
                    "cancelled": false,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}
                    }]
                }),
            )
            .unwrap();
        store
            .append_event(
                "s1",
                EventKind::ToolResult,
                serde_json::json!({
                    "call_id": "call_1",
                    "tool": "read_file",
                    "ok": true,
                    "output": "fn main() {}"
                }),
            )
            .unwrap();

        let messages = store.messages("s1").unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[1].role, eix_core::Role::Assistant);
        assert_eq!(
            messages[1].tool_calls.as_ref().unwrap()[0].function.name,
            "read_file"
        );
        assert_eq!(messages[2].role, eix_core::Role::Tool);
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(messages[2].content, "fn main() {}");
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
}
