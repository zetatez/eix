//! Minimal HTTP transport for the JSON-RPC daemon.
//!
//! The Unix socket remains the trusted default. This module adds an opt-in
//! HTTP surface for tools and SDKs:
//!
//! - `GET /health` (no auth) returns a small liveness document.
//! - `GET /openapi.json` (no auth) describes this surface as OpenAPI 3.1.
//! - `POST /pair` (no auth, but needs a one-time code) mints a client token.
//! - `GET /stats` returns the daemon's `stats` RPC result.
//! - `POST /rpc` takes one JSON-RPC request and returns one response.
//! - `GET /events` streams daemon notifications as Server-Sent Events.
//!   `?session_id=<id>` (repeatable or comma-separated) filters session
//!   notifications; global notifications are always delivered.
//!
//! Authentication is a bearer token (`Authorization: Bearer ...` or
//! `X-Eix-Token`): either the configured `server.token` or a token minted by
//! pairing. Every route except `/health`, `/openapi.json`, `/pair` and CORS
//! preflight requires it. Responses carry permissive CORS headers so browser
//! tools can use the API.

use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use eix_core::{Notification, Request};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::time::timeout;

use crate::server::Daemon;

const MAX_HEAD: usize = 64 * 1024;
const MAX_BODY: usize = 8 * 1024 * 1024;
const MAX_CONNECTIONS: usize = 128;
const READ_TIMEOUT: Duration = Duration::from_secs(15);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const CORS: &str = "Access-Control-Allow-Origin: *\r\nAccess-Control-Allow-Methods: GET, POST, OPTIONS\r\nAccess-Control-Allow-Headers: authorization, content-type, x-eix-token\r\nAccess-Control-Max-Age: 86400\r\n";

pub(crate) async fn serve(daemon: Arc<Daemon>, listener: TcpListener, token: String) {
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let Ok(permit) = Arc::clone(&connections).acquire_owned().await else {
            break;
        };
        let Ok((stream, _)) = listener.accept().await else {
            break;
        };
        let daemon = Arc::clone(&daemon);
        let token = token.clone();
        tokio::spawn(async move {
            let _permit = permit;
            if let Err(err) = handle(daemon, stream, &token).await {
                eprintln!("http: {err:#}");
            }
        });
    }
}

async fn handle(daemon: Arc<Daemon>, stream: TcpStream, token: &str) -> Result<()> {
    let (mut reader, mut writer) = stream.into_split();
    // Read in chunks, but keep any bytes after the header terminator: they are
    // the start of the body and must not be discarded.
    let mut raw = Vec::new();
    let body_prefix;
    let mut chunk = [0u8; 4096];
    loop {
        let read = timeout(READ_TIMEOUT, reader.read(&mut chunk)).await;
        let n = match read {
            Ok(Ok(n)) => n,
            Ok(Err(_)) | Err(_) => return Ok(()),
        };
        if n == 0 {
            return Ok(());
        }
        raw.extend_from_slice(&chunk[..n]);
        if let Some(end) = header_end(&raw) {
            if end > MAX_HEAD {
                return write_status(&mut writer, 431, "request headers too large").await;
            }
            body_prefix = raw.split_off(end);
            break;
        }
        if raw.len() > MAX_HEAD {
            return write_status(&mut writer, 431, "request headers too large").await;
        }
    }
    let head = String::from_utf8_lossy(&raw).into_owned();
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
    let mut chunked = false;
    let mut auth: Option<String> = None;
    for line in lines {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        match key.as_str() {
            "content-length" => content_length = value.parse().unwrap_or(0),
            "transfer-encoding" => chunked = value.eq_ignore_ascii_case("chunked"),
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

    if path == "/openapi.json" && method == "GET" {
        return write_json(&mut writer, 200, &openapi_document()).await;
    }

    // Pairing is deliberately unauthenticated: the one-time code *is* the
    // credential. Read the body before any auth check so a client never has to
    // send its token here.
    if path == "/pair" && method == "POST" {
        let Some(body) = read_body(
            &mut reader,
            &mut writer,
            &body_prefix,
            content_length,
            expect_continue,
        )
        .await?
        else {
            return Ok(());
        };
        let params: Value = match serde_json::from_slice(&body) {
            Ok(params) => params,
            Err(err) => {
                return write_json(
                    &mut writer,
                    400,
                    &serde_json::json!({ "error": format!("invalid JSON body: {err}") }),
                )
                .await
            }
        };
        let code = params
            .get("code")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        return match daemon.pair_client(code, name) {
            Ok((paired, client)) => {
                write_json(
                    &mut writer,
                    200,
                    &serde_json::json!({ "token": paired, "client": client }),
                )
                .await
            }
            Err(err) => {
                let (status, code) = pair_error(&err);
                eprintln!("http: pair rejected: {err:#}");
                write_json(
                    &mut writer,
                    status,
                    &serde_json::json!({ "error": code, "message": format!("{err:#}") }),
                )
                .await
            }
        };
    }

    if !auth
        .as_deref()
        .is_some_and(|given| daemon.accepts_token(given, Some(token)))
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

    // One POST per audio chunk. Streaming the body lets a capture loop send
    // 100 ms as soon as it has them instead of buffering the utterance.
    if method == "POST" {
        if let Some(utterance_id) = path
            .strip_prefix("/asr/")
            .and_then(|rest| rest.strip_suffix("/pcm"))
            .filter(|id| !id.is_empty() && !id.contains('/'))
        {
            return upload_pcm(
                daemon,
                reader,
                writer,
                body_prefix,
                content_length,
                chunked,
                utterance_id,
            )
            .await;
        }
    }

    if path != "/rpc" || method != "POST" {
        return write_status(&mut writer, 404, "not found").await;
    }
    let Some(body) = read_body(
        &mut reader,
        &mut writer,
        &body_prefix,
        content_length,
        expect_continue,
    )
    .await?
    else {
        return Ok(());
    };
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

/// Append PCM16 to one utterance. Accepts both `Content-Length` bodies and
/// `Transfer-Encoding: chunked`, because a capture loop does not know the
/// utterance length up front.
async fn upload_pcm(
    daemon: Arc<Daemon>,
    mut reader: tokio::net::tcp::OwnedReadHalf,
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    body_prefix: Vec<u8>,
    content_length: usize,
    chunked: bool,
    utterance_id: &str,
) -> Result<()> {
    if !chunked && (content_length == 0 || content_length > MAX_BODY) {
        return write_status(&mut writer, 413, "invalid body size").await;
    }
    let mut total = 0u64;

    let result: Result<()> = if chunked {
        let mut buf = body_prefix;
        let mut pos = 0usize;
        loop {
            let line = match take_line(&mut reader, &mut buf, &mut pos).await? {
                Some(line) => line,
                None => break,
            };
            let size = line
                .split(';')
                .next()
                .map(str::trim)
                .and_then(|hex| usize::from_str_radix(hex, 16).ok())
                .ok_or_else(|| anyhow::anyhow!("invalid chunk size"))?;
            if size == 0 {
                break;
            }
            if total as usize + size > MAX_BODY {
                return write_status(&mut writer, 413, "audio exceeds the body limit").await;
            }
            let pcm = take_bytes(&mut reader, &mut buf, &mut pos, size).await?;
            if let Err(err) = push_pcm(&daemon, utterance_id, pcm, &mut total).await {
                return write_json(&mut writer, 400, &serde_json::json!({ "error": err })).await;
            }
        }
        Ok(())
    } else {
        let mut remaining = content_length;
        let mut buf = body_prefix;
        if buf.len() > remaining {
            buf.truncate(remaining);
        }
        loop {
            if !buf.is_empty() {
                let pcm = std::mem::take(&mut buf);
                if let Err(err) = push_pcm(&daemon, utterance_id, pcm, &mut total).await {
                    return write_json(&mut writer, 400, &serde_json::json!({ "error": err }))
                        .await;
                }
            }
            if remaining == 0 {
                break;
            }
            let mut chunk = vec![0u8; remaining.min(64 * 1024)];
            timeout(READ_TIMEOUT, reader.read_exact(&mut chunk))
                .await
                .context("body read timed out")??;
            remaining -= chunk.len();
            buf = chunk;
        }
        Ok(())
    };
    result?;
    write_json(
        &mut writer,
        200,
        &serde_json::json!({ "utterance_id": utterance_id, "bytes": total }),
    )
    .await
}

async fn push_pcm(
    daemon: &Daemon,
    utterance_id: &str,
    pcm: Vec<u8>,
    total: &mut u64,
) -> std::result::Result<(), String> {
    let bytes = daemon
        .asr
        .push(utterance_id, pcm)
        .await
        .map_err(|err| err.message)?;
    *total = bytes;
    Ok(())
}

/// Read up to the next CRLF, refilling from the socket as needed.
async fn take_line(
    reader: &mut tokio::net::tcp::OwnedReadHalf,
    buf: &mut Vec<u8>,
    pos: &mut usize,
) -> Result<Option<String>> {
    loop {
        if let Some(index) = buf[*pos..].windows(2).position(|w| w == b"\r\n") {
            let line = String::from_utf8_lossy(&buf[*pos..*pos + index]).into_owned();
            *pos += index + 2;
            return Ok(Some(line));
        }
        if buf.len() > MAX_BODY + MAX_HEAD {
            return Err(anyhow::anyhow!("chunked framing exceeds the limit"));
        }
        let mut chunk = [0u8; 16 * 1024];
        let read = timeout(READ_TIMEOUT, reader.read(&mut chunk)).await;
        let n = match read {
            Ok(Ok(n)) => n,
            Ok(Err(_)) | Err(_) => return Ok(None),
        };
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn take_bytes(
    reader: &mut tokio::net::tcp::OwnedReadHalf,
    buf: &mut Vec<u8>,
    pos: &mut usize,
    size: usize,
) -> Result<Vec<u8>> {
    while buf.len() - *pos < size + 2 {
        let mut chunk = [0u8; 64 * 1024];
        let n = timeout(READ_TIMEOUT, reader.read(&mut chunk))
            .await
            .context("chunk read timed out")??;
        if n == 0 {
            return Err(anyhow::anyhow!("upload ended mid-chunk"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let pcm = buf[*pos..*pos + size].to_vec();
    *pos += size + 2;
    // Drop consumed framing so a long utterance cannot grow without bound.
    if *pos > 64 * 1024 {
        buf.drain(..*pos);
        *pos = 0;
    }
    Ok(pcm)
}

/// Read the request body, or write a 413 and return `None` when the declared
/// size is missing or too large.
async fn read_body(
    reader: &mut tokio::net::tcp::OwnedReadHalf,
    writer: &mut tokio::net::tcp::OwnedWriteHalf,
    body_prefix: &[u8],
    content_length: usize,
    expect_continue: bool,
) -> Result<Option<Vec<u8>>> {
    if content_length == 0 || content_length > MAX_BODY {
        write_status(writer, 413, "invalid body size").await?;
        return Ok(None);
    }
    if expect_continue {
        write_bytes(writer, b"HTTP/1.1 100 Continue\r\n\r\n").await?;
    }
    let mut body = vec![0u8; content_length];
    let prefix_len = body_prefix.len().min(content_length);
    body[..prefix_len].copy_from_slice(&body_prefix[..prefix_len]);
    if prefix_len < content_length {
        timeout(READ_TIMEOUT, reader.read_exact(&mut body[prefix_len..]))
            .await
            .context("body read timed out")??;
    }
    Ok(Some(body))
}

/// Map a pairing failure to an HTTP status. Anything we do not recognise is a
/// server-side problem, so the message is logged but not echoed.
fn pair_error(err: &anyhow::Error) -> (u16, &'static str) {
    match err.downcast_ref::<crate::server::RpcError>() {
        Some(rpc) if rpc.code == "rate_limited" => (429, "rate_limited"),
        Some(rpc) if rpc.code == "unauthorized" => (401, "unauthorized"),
        _ => (500, "internal"),
    }
}

/// Byte offset just past the first HTTP header terminator in `buf`.
fn header_end(buf: &[u8]) -> Option<usize> {
    let crlf = buf
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 4);
    let lf = buf
        .windows(2)
        .position(|window| window == b"\n\n")
        .map(|index| index + 2);
    match (crlf, lf) {
        (Some(crlf), Some(lf)) => Some(crlf.min(lf)),
        (Some(end), None) | (None, Some(end)) => Some(end),
        (None, None) => None,
    }
}

async fn write_bytes(writer: &mut tokio::net::tcp::OwnedWriteHalf, bytes: &[u8]) -> Result<()> {
    timeout(WRITE_TIMEOUT, writer.write_all(bytes))
        .await
        .context("write timed out")??;
    timeout(WRITE_TIMEOUT, writer.flush())
        .await
        .context("flush timed out")??;
    Ok(())
}

async fn events(
    daemon: Arc<Daemon>,
    mut writer: tokio::net::tcp::OwnedWriteHalf,
    filter: Option<HashSet<String>>,
) -> Result<()> {
    // Subscribe before the client can observe the response headers; otherwise
    // an event emitted right after the connection opens can be missed.
    let mut rx = daemon.bus.subscribe();
    write_bytes(
        &mut writer,
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n{CORS}\r\n"
        )
        .as_bytes(),
    )
    .await?;
    write_bytes(&mut writer, b": connected\n\n").await?;
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
    // Encode the whole SSE frame into one buffer: the previous version built a
    // JSON String and then formatted it into another String, doubling the
    // allocation for every streamed token.
    let mut frame = Vec::with_capacity(256);
    frame.extend_from_slice(b"data: ");
    serde_json::to_writer(&mut frame, notification).context("cannot encode notification")?;
    frame.extend_from_slice(b"\n\n");
    write_bytes(writer, &frame).await
}

/// Well-known code for the HTTP surface. Generated by hand so the document
/// stays dependency-free; keep it in sync with the routes above.
fn openapi_document() -> Value {
    let secured = json!([{ "bearer": [] }, { "eixToken": [] }]);
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "eix daemon HTTP API",
            "version": env!("CARGO_PKG_VERSION"),
            "description": "JSON-RPC over HTTP plus SSE notifications. \
                `POST /rpc` carries the same methods as the Unix socket protocol. \
                Authenticate with `Authorization: Bearer <token>` or `X-Eix-Token`.",
        },
        "servers": [{ "url": "/" }],
        "components": {
            "securitySchemes": {
                "bearer": { "type": "http", "scheme": "bearer" },
                "eixToken": { "type": "apiKey", "in": "header", "name": "X-Eix-Token" }
            },
            "schemas": {
                "Error": {
                    "type": "object",
                    "required": ["error"],
                    "properties": {
                        "error": { "type": "string" },
                        "message": { "type": "string" }
                    }
                },
                "Health": {
                    "type": "object",
                    "required": ["ok", "version"],
                    "properties": {
                        "ok": { "type": "boolean" },
                        "version": { "type": "string" }
                    }
                },
                "PairRequest": {
                    "type": "object",
                    "required": ["code"],
                    "properties": {
                        "code": {
                            "type": "string",
                            "description": "One-time code from `eix pair`. Dashes and case are ignored."
                        },
                        "name": {
                            "type": "string",
                            "description": "Label stored with the token (default: `paired client`)."
                        }
                    }
                },
                "ClientToken": {
                    "type": "object",
                    "required": ["id", "name", "created_at", "revoked"],
                    "properties": {
                        "id": { "type": "string" },
                        "name": { "type": "string" },
                        "created_at": { "type": "integer" },
                        "last_used_at": { "type": ["integer", "null"] },
                        "revoked": { "type": "boolean" }
                    }
                },
                "PairResponse": {
                    "type": "object",
                    "required": ["token", "client"],
                    "properties": {
                        "token": {
                            "type": "string",
                            "description": "Shown once; the daemon stores only its SHA-256 hash."
                        },
                        "client": { "$ref": "#/components/schemas/ClientToken" }
                    }
                },
                "RpcRequest": {
                    "type": "object",
                    "required": ["id", "method"],
                    "properties": {
                        "id": { "type": "integer" },
                        "method": { "type": "string" },
                        "params": {}
                    }
                },
                "RpcResponse": {
                    "type": "object",
                    "required": ["id"],
                    "properties": {
                        "id": { "type": "integer" },
                        "result": {},
                        "error": {
                            "type": "object",
                            "required": ["code", "message"],
                            "properties": {
                                "code": { "type": "string" },
                                "message": { "type": "string" }
                            }
                        }
                    }
                },
                "Stats": { "type": "object", "additionalProperties": true },
                "Notification": {
                    "type": "object",
                    "required": ["method"],
                    "properties": {
                        "method": { "type": "string" },
                        "params": {}
                    }
                }
            }
        },
        "paths": {
            "/health": {
                "get": {
                    "summary": "Liveness probe",
                    "security": [],
                    "responses": {
                        "200": {
                            "description": "Daemon is up",
                            "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Health" } } }
                        }
                    }
                }
            },
            "/openapi.json": {
                "get": {
                    "summary": "This document",
                    "security": [],
                    "responses": { "200": { "description": "OpenAPI 3.1 document" } }
                }
            },
            "/pair": {
                "post": {
                    "summary": "Redeem a one-time pairing code for a durable token",
                    "security": [],
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/PairRequest" } }
                        }
                    },
                    "responses": {
                        "200": {
                            "description": "Token minted",
                            "content": { "application/json": { "schema": { "$ref": "#/components/schemas/PairResponse" } } }
                        },
                        "400": { "description": "Malformed body", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } },
                        "401": { "description": "Invalid or expired code", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } },
                        "429": { "description": "Too many attempts in the current window", "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Error" } } } }
                    }
                }
            },
            "/stats": {
                "get": {
                    "summary": "Daemon statistics",
                    "security": secured,
                    "responses": {
                        "200": {
                            "description": "Stats document",
                            "content": { "application/json": { "schema": { "$ref": "#/components/schemas/Stats" } } }
                        },
                        "401": { "description": "Missing or invalid token" }
                    }
                }
            },
            "/rpc": {
                "post": {
                    "summary": "One JSON-RPC request",
                    "security": secured,
                    "requestBody": {
                        "required": true,
                        "content": {
                            "application/json": { "schema": { "$ref": "#/components/schemas/RpcRequest" } }
                        }
                    },
                    "responses": {
                        "200": {
                            "description": "JSON-RPC response",
                            "content": { "application/json": { "schema": { "$ref": "#/components/schemas/RpcResponse" } } }
                        },
                        "400": { "description": "Malformed JSON" },
                        "401": { "description": "Missing or invalid token" }
                    }
                }
            },
            "/events": {
                "get": {
                    "summary": "Server-Sent Events stream of daemon notifications",
                    "security": secured,
                    "parameters": [
                        {
                            "name": "session_id",
                            "in": "query",
                            "required": false,
                            "description": "Repeatable or comma-separated session filter. Global notifications are always delivered.",
                            "schema": { "type": "string" }
                        },
                        {
                            "name": "all",
                            "in": "query",
                            "required": false,
                            "description": "`1` disables filtering.",
                            "schema": { "type": "string" }
                        }
                    ],
                    "responses": {
                        "200": {
                            "description": "SSE stream; each frame's data is a Notification",
                            "content": { "text/event-stream": { "schema": { "$ref": "#/components/schemas/Notification" } } }
                        },
                        "401": { "description": "Missing or invalid token" }
                    }
                }
            }
        }
    })
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
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n{CORS}\r\n",
        body.len()
    );
    let mut response = head.into_bytes();
    response.extend_from_slice(&body);
    write_bytes(writer, &response).await
}

async fn write_empty(writer: &mut tokio::net::tcp::OwnedWriteHalf, status: u16) -> Result<()> {
    let reason = if status == 204 { "No Content" } else { "OK" };
    write_bytes(
        writer,
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Length: 0\r\nConnection: close\r\n{CORS}\r\n"
        )
        .as_bytes(),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openapi_document_describes_every_route() {
        let doc = openapi_document();
        assert_eq!(doc["openapi"], "3.1.0");
        for path in [
            "/health",
            "/openapi.json",
            "/pair",
            "/stats",
            "/rpc",
            "/events",
        ] {
            assert!(doc["paths"].get(path).is_some(), "missing {path}");
        }
        // The unauthenticated routes must not inherit the global requirement.
        for path in ["/health", "/openapi.json", "/pair"] {
            let method = if path == "/pair" { "post" } else { "get" };
            assert_eq!(doc["paths"][path][method]["security"], json!([]));
        }
        assert!(doc["components"]["securitySchemes"]["bearer"].is_object());
        assert_eq!(
            doc["paths"]["/rpc"]["post"]["security"],
            json!([{ "bearer": [] }, { "eixToken": [] }])
        );
        // Every $ref must resolve, otherwise generated clients break.
        let mut refs = Vec::new();
        collect_refs(&doc, &mut refs);
        for reference in refs {
            let Some(name) = reference.strip_prefix("#/components/schemas/") else {
                panic!("unsupported $ref {reference}");
            };
            assert!(
                doc["components"]["schemas"].get(name).is_some(),
                "dangling $ref {reference}"
            );
        }
    }

    #[test]
    fn pair_errors_map_to_http_status() {
        let limited = anyhow::Error::new(crate::server::RpcError {
            code: "rate_limited".into(),
            message: "slow down".into(),
        });
        assert_eq!(pair_error(&limited).0, 429);
        let unauthorized = anyhow::Error::new(crate::server::RpcError {
            code: "unauthorized".into(),
            message: "bad code".into(),
        });
        assert_eq!(pair_error(&unauthorized).0, 401);
        assert_eq!(pair_error(&anyhow::anyhow!("database is on fire")).0, 500);
    }

    fn collect_refs(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    if key == "$ref" {
                        if let Some(reference) = child.as_str() {
                            out.push(reference.to_string());
                        }
                    } else {
                        collect_refs(child, out);
                    }
                }
            }
            Value::Array(items) => {
                for item in items {
                    collect_refs(item, out);
                }
            }
            _ => {}
        }
    }
}
