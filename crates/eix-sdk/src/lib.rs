//! Typed Rust client for the eix HTTP JSON-RPC API.
//!
//! The daemon's HTTP transport is opt-in (`[server] http` / `EIX_HTTP`) and
//! requires a bearer token. This crate wraps the common calls and turns `/events` into a
//! [`Stream`] of notifications.
//!
//! ```no_run
//! use eix_sdk::Client;
//!
//! # async fn example() -> Result<(), eix_sdk::Error> {
//! let client = Client::new("http://127.0.0.1:7878", std::env::var("EIX_TOKEN").unwrap_or_default());
//! let health = client.health().await?;
//! println!("eix {}", health.version);
//! # Ok(())
//! # }
//! ```

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};

use eix_core::{
    ApprovalRespondParams, ApprovalResult, CancelResult, CreateSessionParams, Notification,
    Request, Response, SessionList, SessionRef, SessionResult, StatsResult, SubmitParams,
    SubmitResult,
};
use futures_util::{stream, Stream, StreamExt};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("http request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("unauthorized: check the daemon token")]
    Unauthorized,
    #[error("http {status}: {body}")]
    Status { status: u16, body: String },
    #[error("rpc {code}: {message}")]
    Rpc { code: String, message: String },
    #[error("event stream ended with an incomplete event")]
    IncompleteEvent,
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Deserialize)]
pub struct Health {
    pub ok: bool,
    pub version: String,
}

/// HTTP client. Clone freely; the id counter and inner reqwest client are
/// shared.
#[derive(Clone)]
pub struct Client {
    base: String,
    token: String,
    http: reqwest::Client,
    next_id: std::sync::Arc<AtomicU64>,
}

impl Client {
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_string(),
            token: token.into(),
            http: reqwest::Client::new(),
            next_id: std::sync::Arc::new(AtomicU64::new(1)),
        }
    }

    pub fn with_http(mut self, http: reqwest::Client) -> Self {
        self.http = http;
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    pub async fn health(&self) -> Result<Health> {
        let response = self
            .http
            .get(format!("{}/health", self.base))
            .send()
            .await?;
        self.decode_response(response).await
    }

    pub async fn stats(&self) -> Result<StatsResult> {
        let response = self
            .http
            .get(format!("{}/stats", self.base))
            .bearer_auth(&self.token)
            .send()
            .await?;
        self.decode_response(response).await
    }

    pub async fn sessions(&self, include_archived: bool) -> Result<SessionList> {
        self.request_typed(
            "session.list",
            serde_json::json!({ "include_archived": include_archived }),
        )
        .await
    }

    pub async fn create_session(&self, params: &CreateSessionParams) -> Result<SessionResult> {
        self.request_typed("session.create", params).await
    }

    pub async fn submit(&self, params: &SubmitParams) -> Result<SubmitResult> {
        self.request_typed("prompt.submit", params).await
    }

    pub async fn cancel(&self, session_id: &str) -> Result<CancelResult> {
        self.request_typed(
            "prompt.cancel",
            SessionRef {
                session_id: session_id.to_string(),
            },
        )
        .await
    }

    pub async fn approve(&self, params: &ApprovalRespondParams) -> Result<ApprovalResult> {
        self.request_typed("approval.respond", params).await
    }

    /// Send one JSON-RPC request and return the raw result value.
    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = Request::new(id, method, params);
        let response = self
            .http
            .post(format!("{}/rpc", self.base))
            .bearer_auth(&self.token)
            .json(&request)
            .send()
            .await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Unauthorized);
        }
        let body: Response = self.decode_response(response).await?;
        if body.id != id {
            return Err(Error::Status {
                status: 200,
                body: format!("response id {} does not match request {id}", body.id),
            });
        }
        if let Some(error) = body.error {
            return Err(Error::Rpc {
                code: error.code,
                message: error.message,
            });
        }
        body.result.ok_or_else(|| Error::Rpc {
            code: "missing_result".into(),
            message: "response has no result".into(),
        })
    }

    /// Typed convenience wrapper around [`request`](Self::request).
    pub async fn request_typed<P: Serialize, T: DeserializeOwned>(
        &self,
        method: &str,
        params: P,
    ) -> Result<T> {
        let value = self.request(method, serde_json::to_value(params)?).await?;
        Ok(serde_json::from_value(value)?)
    }

    /// Stream daemon notifications. With one or more session ids the stream
    /// keeps those sessions plus global notifications; an empty list receives
    /// everything.
    pub async fn events(&self, session_ids: &[&str]) -> Result<EventStream> {
        let mut url = format!("{}/events", self.base);
        if !session_ids.is_empty() {
            url.push('?');
            for (index, id) in session_ids.iter().enumerate() {
                if index > 0 {
                    url.push('&');
                }
                url.push_str("session_id=");
                url.push_str(&percent_encode(id));
            }
        }
        let response = self.http.get(url).bearer_auth(&self.token).send().await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Unauthorized);
        }
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(Error::Status { status, body });
        }
        let bytes = response.bytes_stream();
        // Keep the raw bytes: a multibyte UTF-8 character can be split across
        // two network chunks, and decoding each chunk separately would corrupt
        // it. Events are decoded only once their boundary is complete.
        let parsed = stream::unfold(
            (bytes, Vec::new(), false),
            |(mut bytes, mut buffer, done)| async move {
                if done {
                    return None;
                }
                loop {
                    if let Some((end, width)) = split_event(&buffer) {
                        let event = String::from_utf8_lossy(&buffer[..end]);
                        let parsed = parse_event(&event);
                        drop(event);
                        buffer.drain(..end + width);
                        return match parsed {
                            Ok(Some(notification)) => {
                                Some((Ok(notification), (bytes, buffer, false)))
                            }
                            Ok(None) => continue,
                            Err(err) => Some((Err(err), (bytes, buffer, true))),
                        };
                    }
                    match bytes.next().await {
                        Some(Ok(chunk)) => {
                            buffer.extend_from_slice(&chunk);
                            if buffer.len() > 2 * 1024 * 1024 {
                                return Some((
                                    Err(Error::Status {
                                        status: 200,
                                        body: "SSE buffer exceeded 2 MB".into(),
                                    }),
                                    (bytes, buffer, true),
                                ));
                            }
                        }
                        Some(Err(err)) => {
                            return Some((Err(Error::Http(err)), (bytes, buffer, true)))
                        }
                        None => {
                            return if buffer.iter().all(u8::is_ascii_whitespace) {
                                None
                            } else {
                                Some((Err(Error::IncompleteEvent), (bytes, buffer, true)))
                            }
                        }
                    }
                }
            },
        );
        Ok(EventStream {
            inner: Box::pin(parsed),
        })
    }

    async fn decode_response<T: DeserializeOwned>(&self, response: reqwest::Response) -> Result<T> {
        if response.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::Unauthorized);
        }
        let status = response.status().as_u16();
        let body = response.text().await?;
        if status != 200 {
            return Err(Error::Status { status, body });
        }
        Ok(serde_json::from_str(&body)?)
    }
}

pub struct EventStream {
    inner: Pin<Box<dyn Stream<Item = Result<Notification>> + Send>>,
}

impl Stream for EventStream {
    type Item = Result<Notification>;

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.inner.as_mut().poll_next(cx)
    }
}

impl std::fmt::Debug for EventStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EventStream")
    }
}

/// Locate the end of the next SSE event. Returns the byte offset just before
/// the blank line and the separator width, so the caller can parse the borrowed
/// slice and drain the buffer once instead of copying both halves per event.
fn split_event(buffer: &[u8]) -> Option<(usize, usize)> {
    let mut index = 0;
    while index + 1 < buffer.len() {
        if buffer[index] == b'\n' && buffer[index + 1] == b'\n' {
            return Some((index, 2));
        }
        if index + 3 < buffer.len() && &buffer[index..index + 4] == b"\r\n\r\n" {
            return Some((index, 4));
        }
        index += 1;
    }
    None
}

fn parse_event(event: &str) -> Result<Option<Notification>> {
    let mut data = Vec::new();
    for line in event.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("data:") {
            data.push(value.trim_start());
        }
    }
    if data.is_empty() {
        return Ok(None);
    }
    let json = data.join("\n");
    Ok(Some(serde_json::from_str(&json)?))
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_events_split_and_ignore_comments() {
        let buffer = b": connected\n\ndata: {\"method\":\"x\",\"params\":{}}\n\n";
        let (first, first_width) = split_event(buffer).expect("first event");
        assert!(parse_event(&String::from_utf8_lossy(&buffer[..first]))
            .unwrap()
            .is_none());
        let rest = &buffer[first + first_width..];
        let (second, _) = split_event(rest).expect("second event");
        let notification = parse_event(&String::from_utf8_lossy(&rest[..second]))
            .unwrap()
            .unwrap();
        assert_eq!(notification.method, "x");
    }

    #[test]
    fn query_encoding_is_safe() {
        assert_eq!(percent_encode("a b/c"), "a%20b%2Fc");
    }
}
