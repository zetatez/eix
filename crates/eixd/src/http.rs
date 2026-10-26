//! Minimal HTTP transport for the JSON-RPC daemon.
//!
//! The Unix socket remains the trusted default. This module adds an opt-in
//! HTTP surface for tools and SDKs:
//!
//! - `GET /health` (no auth) returns a small liveness document.
//! - `GET /stats` returns the daemon's `stats` RPC result.
//! - `POST /rpc` takes one JSON-RPC request and returns one response.
//! - `GET /events` streams daemon notifications as Server-Sent Events.
//!   `?session_id=<id>` (repeatable or comma-separated) filters session
//!   notifications; global notifications are always delivered.
//!
//! Authentication is a bearer token (`Authorization: Bearer ...` or
//! `X-Eix-Token`). Every route except `/health` and CORS preflight requires it.
//! Responses carry permissive CORS headers so browser tools can use the API.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};
use eix_core::{Notification, Request};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::server::Daemon;

const MAX_BODY: usize = 8 * 1024 * 1024;
const CORS: &str = "Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: authorization, content-type, x-eix-token\r\nAccess-Control-Max-Age: 86400\r\n";

pub(crate) async fn serve(daemon: Arc<Daemon>, listener: TcpListener, token: String) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let daemon = Arc::clone(&daemon);
        let token = token.clone();
        tokio::spawn(async move {
            if let Err(err) = handle(daemon, stream, &token).await {
                eprintln!("http: {err:#}");
            }
        });
    }
}

async fn handle(daemon: Arc<Daemon>, stream: TcpStream, token: &str) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    let mut head = Vec::new();
    loop {
        let mut byte = [0u8; 1];
        if reader.read_exact(&mut byte).await.is_err() {
            return Ok(());
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") || head.ends_with(b"\n\n") {
            break;
        }
        if head.len() > 64 * 1024 {
            return write_status(&mut writer, 431, "request headers too large").await;
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let target = parts.next().unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = path.to_string();
    let query = parse_query(query);
    let mut content_length = 0usize;
    let mut expect_continue = false;
    let mut auth: Option<String> = None;
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "content-length" => content_length = value.parse().unwrap_or(0),
            "expect" if value.eq_ignore_ascii_case("100-continue") => expect_continue = true,
            "authorization" => {
                auth = Some(
                    value
                        .split_once(' ')
                        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
                        .map(|(_, token)| token.trim())
                        .unwrap_or(value)
                        .to_string(),
                );
            }
            "x-eix-token" => auth = Some(value.to_string()),
            _ => {}
        }
    }

    if method == "OPTIONS" {
        return write_empty(&mut writer, 204).await;
    }

    if path == "/health" && method == "GET" {
        return write_json(
            &mut writer,
            200,
            &serde_json::json!({
                "ok": true,
                "version": env!("CARGO_PKG_VERSION"),
            }),
        )
        .await;
    }

    if !auth
        .as_deref()
        .is_some_and(|given| crate::server::secret_eq(given, token))
    {
        return write_json(
            &mut writer,
            401,
            &serde_json::json!({ "error": "unauthorized" }),
        )
        .await;
    }

    if path == "/stats" && method == "GET" {
        return match daemon.stats().await {
            Ok(stats) => write_json(&mut writer, 200, &stats).await,
            Err(err) => {
                eprintln!("http: stats: {err:#}");
                write_json(
                    &mut writer,
                    500,
                    &serde_json::json!({ "error": "cannot collect stats" }),
                )
                .await
            }
        };
    }

    if path == "/events" && method == "GET" {
        let filter = session_filter(&query);
        return events(daemon, writer, filter).await;
    }

    if path != "/rpc" || method != "POST" {
        return write_status(&mut writer, 404, "not found").await;
    }
    if content_length == 0 || content_length > MAX_BODY {
        return write_status(&mut writer, 413, "invalid body size").await;
    }
    if expect_continue {
        writer.write_all(b"HTTP/1.1 100 Continue\r\n\r\n").await?;
        writer.flush().await?;
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).await?;
    let request: Request = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => {
            return write_json(
                &mut writer,
                400,
                &serde_json::json!({ "error": format!("invalid JSON request: {err}") }),
            )
            .await
        }
    };
    let response = daemon.handle_remote(&request).await;
    write_json(&mut writer, 200, &response).await
}

async fn events(
    daemon: Arc<Daemon>,
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    filter: Option<HashSet<String>>,
) -> Result<()> {
    // Subscribe before the client can observe the response headers; otherwise
    // an event emitted right after the connection opens can be missed.
    let mut rx = daemon.bus.subscribe();
    writer
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n{CORS}\r\n"
            )
            .as_bytes(),
        )
        .await?;
    writer.write_all(b": connected\n\n").await?;
    writer.flush().await?;
    loop {
        match rx.recv().await {
            Ok(notification) => {
                if !notification_matches(&notification, filter.as_ref()) {
                    continue;
                }
                write_event(&mut writer, &notification).await?;
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
    Ok(())
}

fn session_filter(query: &BTreeMap<String, Vec<String>>) -> Option<HashSet<String>> {
    if query
        .get("all")
        .is_some_and(|values| values.iter().any(|value| value == "1" || value == "true"))
        || query
            .get("session_id")
            .is_some_and(|values| values.iter().any(|value| value == "all"))
    {
        return None;
    }
    let mut ids = HashSet::new();
    for value in query.get("session_id").into_iter().flatten() {
        for id in value.split(',') {
            let id = id.trim();
            if !id.is_empty() {
                ids.insert(id.to_string());
            }
        }
    }
    (!ids.is_empty()).then_some(ids)
}

fn notification_matches(notification: &Notification, filter: Option<&HashSet<String>>) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    let id = notification
        .params
        .get("session_id")
        .and_then(|id| id.as_str())
        .or_else(|| {
            notification
                .params
                .get("session")
                .and_then(|session| session.get("id"))
                .and_then(|id| id.as_str())
        })
        .or_else(|| {
            // `session.updated` sends the Session object directly.
            (notification.method == "session.updated")
                .then(|| notification.params.get("id").and_then(|id| id.as_str()))
                .flatten()
        });
    match id {
        // Global notifications (config errors, worktree updates, ...) are
        // useful to every consumer and have no session to filter on.
        None => true,
        Some(id) => filter.contains(id),
    }
}

fn parse_query(query: &str) -> BTreeMap<String, Vec<String>> {
    let mut params: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pair in query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        params
            .entry(percent_decode(key))
            .or_default()
            .push(percent_decode(value));
    }
    params
}

fn percent_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => {
                out.push(b' ');
                index += 1;
            }
            b'%' if index + 2 < bytes.len() => {
                let hex = &value[index + 1..index + 3];
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    index += 3;
                } else {
                    out.push(bytes[index]);
                    index += 1;
                }
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn write_event(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    notification: &Notification,
) -> Result<()> {
    let json = serde_json::to_string(notification).context("cannot encode notification")?;
    writer
        .write_all(format!("data: {json}\n\n").as_bytes())
        .await?;
    writer.flush().await?;
    Ok(())
}

async fn write_status(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    status: u16,
    message: &str,
) -> Result<()> {
    write_json(writer, status, &serde_json::json!({ "error": message })).await
}

async fn write_json<T: serde::Serialize>(
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    status: u16,
    value: &T,
) -> Result<()> {
    let body = serde_json::to_vec(value).context("cannot encode response")?;
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{CORS}\r\n",
        body.len()
    );
    writer.write_all(head.as_bytes()).await?;
    writer.write_all(&body).await?;
    writer.flush().await?;
    Ok(())
}

async fn write_empty(writer: &mut tokio::net::tcp::OwnedWriteHalf, status: u16) -> Result<()> {
    let reason = if status == 204 { "No Content" } else { "OK" };
    writer
        .write_all(
            format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n{CORS}\r\n"
            )
            .as_bytes(),
        )
        .await?;
    writer.flush().await?;
    Ok(())
}
