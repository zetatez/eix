//! Wire protocol between `eixd` and frontends.
//!
//! Transport is newline-delimited JSON over a Unix socket. A client sends
//! `Request` objects with a monotonic `id`; the daemon answers with `Response`
//! objects and pushes `Notification`s at any time. Notifications carry the
//! structured events described in `DESIGN.md`, never pre-rendered text.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{Event, Session, SessionId};

/// Bumped whenever a change breaks older clients.
pub const PROTOCOL_VERSION: u32 = 1;

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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionRef {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubmitParams {
    pub session_id: SessionId,
    pub text: String,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalResult {
    pub accepted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventsParams {
    pub session_id: SessionId,
    #[serde(default)]
    pub after_seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionList {
    pub sessions: Vec<Session>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionResult {
    pub session: Session,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttachResult {
    pub session: Session,
    pub events: Vec<Event>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HelloResult {
    pub protocol: u32,
    pub version: String,
    pub pid: u32,
    pub model: String,
    pub base_url: String,
    pub sessions: usize,
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
}
