//! End-to-end ACP test: run `eix acp` against a scripted daemon.
//!
//! The fake daemon speaks the same newline-delimited JSON-RPC as `eixd`, so
//! the test exercises the adapter's real stdio loop, capability negotiation,
//! session mapping, streaming updates, permission round-trips and replay.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eix_core::{Event, EventKind, Notification, Request, Response, Session, SessionMode};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::net::unix::OwnedWriteHalf;
use tokio::net::UnixListener;
use tokio::process::{ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex as AsyncMutex;

#[derive(Default)]
struct Recorded {
    submits: usize,
    approvals: Vec<Value>,
}

#[tokio::test]
async fn acp_drives_sessions_prompts_and_permissions() {
    let dir = temp_dir("eix-acp");
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("eixd.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let recorded = Arc::new(Mutex::new(Recorded::default()));
    let daemon = tokio::spawn(serve(listener, dir.clone(), Arc::clone(&recorded)));

    let mut child = Command::new(env!("CARGO_BIN_EXE_eix"))
        .arg("--socket")
        .arg(&socket)
        .arg("--data-dir")
        .arg(&dir)
        .arg("acp")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("cannot start eix acp");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = BufReader::new(child.stdout.take().unwrap());
    let mut lines = stdout.lines();
    let mut stderr = BufReader::new(child.stderr.take().unwrap()).lines();

    // initialize
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": 1,
                "clientCapabilities": { "fs": { "readTextFile": true, "writeTextFile": true } },
                "clientInfo": { "name": "test", "version": "0" }
            }
        }),
    )
    .await;
    let (result, _) = read_until(&mut lines, &mut stderr, 1).await;
    assert_eq!(result["protocolVersion"], 1);
    assert_eq!(result["agentCapabilities"]["loadSession"], true);
    assert_eq!(
        result["agentCapabilities"]["promptCapabilities"]["embeddedContext"],
        true
    );
    assert_eq!(
        result["agentCapabilities"]["promptCapabilities"]["image"],
        false
    );
    assert!(result["agentCapabilities"]["sessionCapabilities"]["close"].is_object());

    // session/new
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/new",
            "params": { "cwd": dir, "mcpServers": [] }
        }),
    )
    .await;
    let (result, _) = read_until(&mut lines, &mut stderr, 2).await;
    assert_eq!(result["sessionId"], "sess-1");
    assert_eq!(result["modes"]["currentModeId"], "build");
    assert_eq!(
        result["modes"]["availableModes"].as_array().unwrap().len(),
        4
    );

    // session/prompt streams chunks, a tool call, and finishes the turn.
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "session/prompt",
            "params": {
                "sessionId": "sess-1",
                "prompt": [
                    { "type": "text", "text": "hello" },
                    { "type": "resource_link", "name": "main.rs", "uri": "file:///tmp/main.rs" }
                ]
            }
        }),
    )
    .await;
    let (result, updates) = read_until(&mut lines, &mut stderr, 3).await;
    assert_eq!(result["stopReason"], "end_turn");
    assert!(has_update(
        &updates,
        "agent_message_chunk",
        "Hello from eix"
    ));
    assert!(updates.iter().any(|update| {
        update["sessionUpdate"] == "tool_call" && update["toolCallId"] == "call-1"
    }));

    // A second turn asks for permission; the adapter must round-trip it.
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "session/prompt",
            "params": {
                "sessionId": "sess-1",
                "prompt": [{ "type": "text", "text": "run it" }]
            }
        }),
    )
    .await;
    let permission = read_request(&mut lines, &mut stderr, "session/request_permission").await;
    assert_eq!(permission["params"]["sessionId"], "sess-1");
    assert_eq!(permission["params"]["toolCall"]["toolCallId"], "call-2");
    let options = permission["params"]["options"].as_array().unwrap();
    assert!(options
        .iter()
        .any(|option| option["optionId"] == "allow-once"));
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": permission["id"],
            "result": { "outcome": { "outcome": "selected", "optionId": "allow-once" } }
        }),
    )
    .await;
    let (result, _) = read_until(&mut lines, &mut stderr, 4).await;
    assert_eq!(result["stopReason"], "end_turn");

    // session/load replays history as session/update notifications.
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "session/load",
            "params": { "sessionId": "sess-load", "cwd": dir, "mcpServers": [] }
        }),
    )
    .await;
    let (_, updates) = read_until(&mut lines, &mut stderr, 5).await;
    assert!(has_update(
        &updates,
        "user_message_chunk",
        "previous question"
    ));
    assert!(has_update(
        &updates,
        "agent_message_chunk",
        "previous answer"
    ));

    // session/close succeeds for a known session and rejects an unknown one.
    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 6,
            "method": "session/close",
            "params": { "sessionId": "sess-1" }
        }),
    )
    .await;
    let (result, _) = read_until(&mut lines, &mut stderr, 6).await;
    assert_eq!(result, json!({}));

    send(
        &mut stdin,
        json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "session/close",
            "params": { "sessionId": "missing" }
        }),
    )
    .await;
    let error = read_error(&mut lines, &mut stderr, 7).await;
    assert_eq!(error["code"], -32002);

    drop(stdin);
    let status = tokio::time::timeout(Duration::from_secs(10), child.wait())
        .await
        .expect("eix acp did not exit")
        .unwrap();
    assert!(status.success(), "eix acp failed: {status}");

    {
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.submits, 2);
        assert_eq!(recorded.approvals.len(), 1);
        assert_eq!(recorded.approvals[0]["approval_id"], "apr-1");
        assert_eq!(recorded.approvals[0]["decision"], "approve");
        assert_eq!(recorded.approvals[0]["scope"], "once");
    }

    let _ = tokio::time::timeout(Duration::from_secs(5), daemon).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Scripted fake daemon. The first connection is `ensure_endpoint`'s probe.
async fn serve(listener: UnixListener, dir: PathBuf, recorded: Arc<Mutex<Recorded>>) {
    let (probe, _) = listener.accept().await.unwrap();
    drop(probe);
    let (stream, _) = listener.accept().await.unwrap();
    let (read, write) = stream.into_split();
    let write = Arc::new(AsyncMutex::new(write));
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let request = match Request::parse(&line) {
            Ok(request) => request,
            Err(_) => continue,
        };
        match request.method.as_str() {
            "session.create" => {
                let session = session("sess-1", &dir, SessionMode::Build);
                reply(
                    &write,
                    Response::ok(request.id, json!({ "session": session })),
                )
                .await;
            }
            "session.attach" => {
                let session = session("sess-load", &dir, SessionMode::Build);
                let events = vec![
                    Event::new(
                        "sess-load",
                        1,
                        EventKind::UserMessage,
                        json!({ "text": "previous question", "images": [] }),
                    ),
                    Event::new(
                        "sess-load",
                        2,
                        EventKind::AssistantMessage,
                        json!({ "text": "previous answer", "model": "test" }),
                    ),
                ];
                reply(
                    &write,
                    Response::ok(
                        request.id,
                        json!({ "session": session, "events": events, "has_more": false }),
                    ),
                )
                .await;
            }
            "prompt.submit" => {
                let session = session("sess-1", &dir, SessionMode::Build);
                let turn = {
                    let mut recorded = recorded.lock().unwrap();
                    recorded.submits += 1;
                    format!("turn-{}", recorded.submits)
                };
                reply(
                    &write,
                    Response::ok(request.id, json!({ "session": session, "turn_id": turn })),
                )
                .await;
                if turn == "turn-1" {
                    notify(
                        &write,
                        "message.delta",
                        json!({ "session_id": "sess-1", "turn_id": turn, "delta": "Hello from eix" }),
                    )
                    .await;
                    notify(
                        &write,
                        "session.event",
                        serde_json::to_value(Event::new(
                            "sess-1",
                            2,
                            EventKind::ToolCall,
                            json!({
                                "call_id": "call-1",
                                "tool": "exec_command",
                                "arguments": { "cmd": "cat src/main.rs" }
                            }),
                        ))
                        .unwrap(),
                    )
                    .await;
                    notify(
                        &write,
                        "turn.completed",
                        json!({
                            "session_id": "sess-1",
                            "turn_id": turn,
                            "cancelled": false,
                            "error": null
                        }),
                    )
                    .await;
                } else {
                    notify(
                        &write,
                        "tool.approval_required",
                        json!({
                            "session_id": "sess-1",
                            "turn_id": turn,
                            "approval_id": "apr-1",
                            "call_id": "call-2",
                            "tool": "exec_command",
                            "summary": "exec_command: ls",
                            "arguments": { "cmd": "ls" }
                        }),
                    )
                    .await;
                }
            }
            "approval.respond" => {
                recorded
                    .lock()
                    .unwrap()
                    .approvals
                    .push(request.params.clone());
                reply(&write, Response::ok(request.id, json!({ "session": null }))).await;
                notify(
                    &write,
                    "turn.completed",
                    json!({
                        "session_id": "sess-1",
                        "turn_id": "turn-2",
                        "cancelled": false,
                        "error": null
                    }),
                )
                .await;
            }
            "prompt.cancel" => {
                reply(
                    &write,
                    Response::ok(request.id, json!({ "cancelled": true })),
                )
                .await;
            }
            other => {
                reply(
                    &write,
                    Response::err(request.id, "method_not_found", other.to_string()),
                )
                .await;
            }
        }
    }
}

fn session(id: &str, cwd: &Path, mode: SessionMode) -> Value {
    let mut session = Session::new(id, "acp", cwd);
    session.mode = mode;
    serde_json::to_value(session).unwrap()
}

async fn reply(write: &Arc<AsyncMutex<OwnedWriteHalf>>, response: Response) {
    write_line(write, serde_json::to_value(response).unwrap()).await;
}

async fn notify(write: &Arc<AsyncMutex<OwnedWriteHalf>>, method: &str, params: Value) {
    write_line(
        write,
        serde_json::to_value(Notification::from_value(method, params)).unwrap(),
    )
    .await;
}

async fn write_line(write: &Arc<AsyncMutex<OwnedWriteHalf>>, value: Value) {
    let mut line = serde_json::to_vec(&value).unwrap();
    line.push(b'\n');
    let mut guard = write.lock().await;
    guard.write_all(&line).await.unwrap();
    guard.flush().await.unwrap();
}

async fn send(stdin: &mut ChildStdin, value: Value) {
    let mut line = serde_json::to_vec(&value).unwrap();
    line.push(b'\n');
    stdin.write_all(&line).await.unwrap();
    stdin.flush().await.unwrap();
}

/// Read until the response for `id`, returning its result and every update.
async fn read_until(
    lines: &mut Lines<BufReader<ChildStdout>>,
    stderr: &mut Lines<BufReader<tokio::process::ChildStderr>>,
    id: u64,
) -> (Value, Vec<Value>) {
    let mut updates = Vec::new();
    loop {
        let message = next_message(lines, stderr).await;
        if message.get("method").is_some() {
            if let Some(params) = message.get("params") {
                if let Some(update) = params.get("update") {
                    updates.push(update.clone());
                }
                if message["method"] == "session/request_permission" {
                    panic!("unexpected permission request while waiting for id {id}: {message}");
                }
            }
            continue;
        }
        if message["id"] == json!(id) {
            if let Some(error) = message.get("error") {
                panic!("request {id} failed: {error}");
            }
            return (message["result"].clone(), updates);
        }
    }
}

async fn read_request(
    lines: &mut Lines<BufReader<ChildStdout>>,
    stderr: &mut Lines<BufReader<tokio::process::ChildStderr>>,
    method: &str,
) -> Value {
    loop {
        let message = next_message(lines, stderr).await;
        if message["method"] == method {
            return message;
        }
    }
}

async fn read_error(
    lines: &mut Lines<BufReader<ChildStdout>>,
    stderr: &mut Lines<BufReader<tokio::process::ChildStderr>>,
    id: u64,
) -> Value {
    loop {
        let message = next_message(lines, stderr).await;
        if message.get("method").is_none() && message["id"] == json!(id) {
            return message["error"].clone();
        }
    }
}

async fn next_message(
    lines: &mut Lines<BufReader<ChildStdout>>,
    stderr: &mut Lines<BufReader<tokio::process::ChildStderr>>,
) -> Value {
    match tokio::time::timeout(Duration::from_secs(10), lines.next_line()).await {
        Ok(Ok(Some(line))) => serde_json::from_str(&line)
            .unwrap_or_else(|err| panic!("invalid JSON from eix acp: {err}: {line}")),
        Ok(Ok(None)) => {
            let mut log = String::new();
            while let Ok(Some(line)) = stderr.next_line().await {
                log.push_str(&line);
                log.push('\n');
            }
            panic!("eix acp closed stdout unexpectedly:\n{log}");
        }
        Ok(Err(err)) => panic!("cannot read stdout: {err}"),
        Err(_) => panic!("timed out waiting for eix acp"),
    }
}

fn has_update(updates: &[Value], kind: &str, text: &str) -> bool {
    updates
        .iter()
        .any(|update| update["sessionUpdate"] == kind && update["content"]["text"] == text)
}

fn temp_dir(prefix: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("{prefix}-{}-{nanos}", std::process::id()))
}
