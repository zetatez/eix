//! End-to-end test: real Unix socket, real SQLite, streaming mock provider.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eix_core::{
    AttachResult, Config, EventKind, HelloResult, PermissionLevel, Request, Response, SearchResult,
    ServerMessage, SessionResult, SubmitResult, PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::Mutex;

type ClientReader = BufReader<ReadHalf<UnixStream>>;

#[tokio::test]
async fn daemon_streams_and_persists_a_turn() -> anyhow::Result<()> {
    let dir = temp_dir("turn");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let data_dir = dir.join("data");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), data_dir.clone()));

    let mut client = connect(&socket).await?;
    let hello: HelloResult = client.request("hello", Value::Null).await?;
    assert_eq!(hello.protocol, PROTOCOL_VERSION);
    assert_eq!(hello.model, "mock-model");

    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "e2e", "cwd": dir, "model": "session-model"}),
        )
        .await?;
    let session_id = created.session.id.clone();
    assert_eq!(created.session.name, "e2e");
    assert_eq!(created.session.model.as_deref(), Some("session-model"));

    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "say hi"}),
        )
        .await?;
    assert!(!submitted.turn_id.is_empty());

    let mut text = String::new();
    let completed = loop {
        let notification = client.next_notification().await?;
        match notification.method.as_str() {
            "message.delta" => {
                let delta = notification.params["delta"].as_str().unwrap();
                text.push_str(delta);
            }
            "turn.completed"
                if notification.params["session_id"] == session_id.as_str()
                    && notification.params["turn_id"] == submitted.turn_id.as_str() =>
            {
                break notification.params.clone();
            }
            _ => {}
        }
    };
    assert_eq!(text, "Hello from mock");
    assert!(completed["error"].is_null());

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert_eq!(attached.session.status, eix_core::SessionStatus::Idle);
    let user = attached
        .events
        .iter()
        .find(|e| e.kind == EventKind::UserMessage)
        .expect("user event");
    assert_eq!(user.payload["text"], "say hi");
    let assistant = attached
        .events
        .iter()
        .find(|e| e.kind == EventKind::AssistantMessage)
        .expect("assistant event");
    assert_eq!(assistant.payload["text"], "Hello from mock");
    // The provider's reported model wins over the requested one.
    assert_eq!(assistant.payload["model"], "mock-1");
    assert_eq!(assistant.payload["usage"]["total_tokens"], 5);

    // The provider saw the system + user messages and requested a stream.
    let bodies = bodies.lock().await;
    assert_eq!(bodies.len(), 1);
    let body: Value = serde_json::from_str(&bodies[0])?;
    assert_eq!(body["model"], "session-model");
    assert_eq!(body["stream"], true);
    assert_eq!(body["messages"][0]["role"], "system");
    let user_message = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user")
        .expect("user message");
    assert_eq!(user_message["content"], "say hi");

    assert!(data_dir.join("eix.sqlite3").exists());

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn context_ledger_reports_sectors_and_honours_policies() -> anyhow::Result<()> {
    let dir = temp_dir("context");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("AGENTS.md"), "Always answer in lowercase.\n")?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let data_dir = dir.join("data");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), data_dir.clone()));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "ctx", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "first question"}),
        )
        .await?;
    wait_turn(&mut client).await?;

    let ledger: eix_core::ContextLedgerResult = client
        .request("context.ledger", json!({"session_id": session_id}))
        .await?;
    assert!(ledger.total_tokens > 0);
    assert_eq!(ledger.budget_tokens, 32_000);
    let instructions = ledger
        .entries
        .iter()
        .find(|entry| entry.id == "instructions:AGENTS.md")
        .expect("AGENTS.md in the ledger");
    assert!(instructions.detail.contains("Always answer in lowercase"));
    assert_eq!(instructions.sector, eix_core::Sector::Instructions);
    assert!(ledger.entries.iter().any(|entry| entry.id == "workspace"));
    assert!(ledger
        .sectors
        .iter()
        .find(|sector| sector.sector == eix_core::Sector::History)
        .is_some_and(|sector| sector.entries == 2));

    // The first request carries the system prompt, the instructions and the
    // workspace summary.
    let first: Value = serde_json::from_str(&bodies.lock().await[0])?;
    let first_text = first["messages"].to_string();
    assert!(first_text.contains("Always answer in lowercase"));
    assert!(first_text.contains("workspace:"));
    assert!(first_text.contains("first question"));

    let user = ledger
        .entries
        .iter()
        .find(|entry| entry.kind == "user")
        .expect("user entry")
        .clone();
    let assistant = ledger
        .entries
        .iter()
        .find(|entry| entry.kind == "assistant")
        .expect("assistant entry")
        .clone();
    assert!(user.id.starts_with('e'));
    assert!(assistant.id.starts_with('e'));

    let pinned: eix_core::ContextLedgerResult = client
        .request(
            "context.pin",
            json!({"session_id": session_id, "entry": user.id, "pinned": true}),
        )
        .await?;
    assert!(
        pinned
            .entries
            .iter()
            .find(|entry| entry.id == user.id)
            .unwrap()
            .pinned
    );

    let dropped: eix_core::ContextLedgerResult = client
        .request(
            "context.drop",
            json!({"session_id": session_id, "entry": assistant.id}),
        )
        .await?;
    assert!(
        dropped
            .entries
            .iter()
            .find(|entry| entry.id == assistant.id)
            .unwrap()
            .dropped
    );

    // System entries are not user-editable.
    let refused = client
        .request_error(
            "context.drop",
            json!({"session_id": session_id, "entry": "system"}),
        )
        .await?;
    assert_eq!(refused, "invalid_params");

    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "second question"}),
        )
        .await?;
    wait_turn(&mut client).await?;

    let second: Value = serde_json::from_str(&bodies.lock().await[1])?;
    let second_text = second["messages"].to_string();
    assert!(second_text.contains("Always answer in lowercase"));
    assert!(second_text.contains("first question"));
    assert!(
        !second_text.contains("Hello from mock"),
        "a dropped assistant message must not be sent: {second_text}"
    );

    // Compaction replaces the first exchange with one summary.
    let from = seq_of(&user.id)?;
    let to = seq_of(&assistant.id)?;
    let compacted: eix_core::ContextLedgerResult = client
        .request(
            "context.compact",
            json!({
                "session_id": session_id,
                "from_seq": from,
                "to_seq": to,
                "summary": "we exchanged greetings",
            }),
        )
        .await?;
    assert!(compacted
        .entries
        .iter()
        .any(|entry| entry.id == format!("c{from}-{to}")));
    assert!(
        compacted
            .entries
            .iter()
            .find(|entry| entry.id == user.id)
            .unwrap()
            .compacted
    );

    let overlap = client
        .request_error(
            "context.compact",
            json!({"session_id": session_id, "from_seq": from, "to_seq": from + 1}),
        )
        .await?;
    assert_eq!(overlap, "invalid_params");

    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "third question"}),
        )
        .await?;
    wait_turn(&mut client).await?;
    let third: Value = serde_json::from_str(&bodies.lock().await[2])?;
    let third_text = third["messages"].to_string();
    assert!(third_text.contains("we exchanged greetings"));
    assert!(third_text.contains("[compacted 2 messages"));
    assert!(!third_text.contains("first question"));
    assert!(third_text.contains("second question"));

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert!(attached
        .events
        .iter()
        .any(|event| event.kind == EventKind::ContextPinned));
    assert!(attached
        .events
        .iter()
        .any(|event| event.kind == EventKind::ContextDropped));
    assert!(attached
        .events
        .iter()
        .any(|event| event.kind == EventKind::ContextCompacted));

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

async fn wait_turn(client: &mut TestClient) -> anyhow::Result<String> {
    let mut text = String::new();
    loop {
        let notification = client.next_notification().await?;
        match notification.method.as_str() {
            "message.delta" => {
                if let Some(delta) = notification.params["delta"].as_str() {
                    text.push_str(delta);
                }
            }
            "turn.completed" => {
                if let Some(error) = notification.params["error"].as_str() {
                    anyhow::bail!("turn failed: {error}");
                }
                return Ok(text);
            }
            _ => {}
        }
    }
}

fn seq_of(id: &str) -> anyhow::Result<u64> {
    Ok(id
        .strip_prefix('e')
        .ok_or_else(|| anyhow::anyhow!("not an event entry: {id}"))?
        .parse()?)
}

#[tokio::test]
async fn cancel_keeps_the_partial_answer_and_returns_to_idle() -> anyhow::Result<()> {
    let dir = temp_dir("cancel");
    std::fs::create_dir_all(&dir)?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(slow_provider(listener));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "cancel", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "go"}),
        )
        .await?;

    // Wait until the first token is on the wire, then cancel.
    loop {
        let notification = client.next_notification().await?;
        if notification.method == "message.delta"
            && notification.params["session_id"] == session_id.as_str()
        {
            break;
        }
    }
    let cancelled: eix_core::CancelResult = client
        .request("prompt.cancel", json!({"session_id": session_id}))
        .await?;
    assert!(cancelled.cancelled);

    let completed = loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            break notification.params.clone();
        }
    };
    assert_eq!(completed["cancelled"], true);

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert_eq!(attached.session.status, eix_core::SessionStatus::Idle);
    let assistant = attached
        .events
        .iter()
        .find(|e| e.kind == EventKind::AssistantMessage)
        .expect("partial assistant event");
    assert_eq!(assistant.payload["text"], "part");
    assert_eq!(assistant.payload["cancelled"], true);

    // Nothing is running now, so a second cancel is a no-op.
    let again: eix_core::CancelResult = client
        .request("prompt.cancel", json!({"session_id": session_id}))
        .await?;
    assert!(!again.cancelled);

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn staged_write_is_applied_after_approval() -> anyhow::Result<()> {
    let dir = temp_dir("approve");
    std::fs::create_dir_all(&dir)?;
    // Pre-existing content so the snapshot has something to roll back to.
    std::fs::write(dir.join("out.txt"), "before")?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(tool_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "approve", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "write out.txt"}),
        )
        .await?;
    assert!(!submitted.turn_id.is_empty());

    let patch = loop {
        let notification = client.next_notification().await?;
        if notification.method == "patch.proposed" {
            break notification.params.clone();
        }
    };
    assert_eq!(patch["tool"], "write_file");
    assert_eq!(patch["call_id"], "call_1");
    let diff = patch["diff"].as_str().expect("diff preview");
    assert!(diff.contains("+++ b/out.txt"), "diff was: {diff}");
    assert!(diff.contains("+hello"), "diff was: {diff}");
    let hunks = patch["hunks"].as_array().expect("hunk preview");
    assert_eq!(hunks.len(), 1, "hunks were: {hunks:?}");
    assert_eq!(hunks[0]["index"], 0);
    assert!(
        hunks[0]["lines"]
            .as_array()
            .is_some_and(|lines| lines.iter().any(|line| line == "+hello")),
        "hunk was: {:?}",
        hunks[0]
    );
    let patch_id = patch["patch_id"].as_str().unwrap().to_string();
    // Staging does not touch the workspace.
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, "before");

    let mut saw_attention = false;
    let completed = loop {
        let notification = client.next_notification().await?;
        if notification.method == "session.attention" {
            assert_eq!(notification.params["status"], "awaiting_approval");
            saw_attention = true;
        }
        if notification.method == "turn.completed" {
            break notification.params.clone();
        }
    };
    assert!(saw_attention);
    assert!(completed["error"].is_null());

    let staged: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert_eq!(staged.session.status, eix_core::SessionStatus::Idle);
    assert!(staged
        .events
        .iter()
        .any(|e| e.kind == EventKind::PatchProposed));
    assert!(
        !staged
            .events
            .iter()
            .any(|e| e.kind == EventKind::ApprovalRequired),
        "staged writes do not block on an approval event"
    );
    assert!(
        !staged.events.iter().any(|e| e.kind == EventKind::Snapshot),
        "the snapshot is taken when the patch is applied"
    );
    let result = staged
        .events
        .iter()
        .find(|e| e.kind == EventKind::ToolResult)
        .expect("tool result event");
    assert_eq!(result.payload["ok"], true);
    assert_eq!(result.payload["denied"], false);

    let applied: eix_core::PatchResult = client
        .request(
            "patch.apply",
            json!({"session_id": session_id, "patch_id": patch_id}),
        )
        .await?;
    assert!(applied.accepted);
    assert!(applied.applied);
    assert!(!applied.conflict);
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, "hello");

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert!(attached
        .events
        .iter()
        .any(|e| e.kind == EventKind::PatchApplied));
    let snapshot = attached
        .events
        .iter()
        .find(|e| e.kind == EventKind::Snapshot)
        .expect("snapshot event");
    assert_eq!(snapshot.payload["ok"], true);
    assert_eq!(snapshot.payload["tool"], "write_file");
    assert_eq!(snapshot.payload["scope"], "file");
    assert_eq!(
        snapshot.payload["files"], 1,
        "snapshot was: {}",
        snapshot.payload
    );
    let snapshot_id = snapshot.payload["snapshot_id"].as_str().unwrap();
    let saved = dir
        .join("data/snapshots")
        .join(&session_id)
        .join(snapshot_id)
        .join("files/out.txt");
    assert_eq!(std::fs::read_to_string(&saved)?, "before");
    let assistant = attached
        .events
        .iter()
        .rfind(|e| e.kind == EventKind::AssistantMessage)
        .expect("final assistant event");
    assert_eq!(assistant.payload["text"], "done");

    let bodies = bodies.lock().await;
    assert_eq!(bodies.len(), 2);
    let first: Value = serde_json::from_str(&bodies[0])?;
    assert_eq!(first["tool_choice"], "auto");
    assert_eq!(first["tools"][0]["function"]["name"], "read_file");
    let second: Value = serde_json::from_str(&bodies[1])?;
    let content = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default();
    assert!(
        content.starts_with("staged out.txt as patch "),
        "tool content was: {content}"
    );
    assert!(content.contains("not written yet"), "{content}");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn read_only_session_refuses_writes_and_permission_is_logged() -> anyhow::Result<()> {
    let dir = temp_dir("read-only");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(tool_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;

    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "read-only", "cwd": dir, "permission": "read-only"}),
        )
        .await?;
    let session_id = created.session.id.clone();
    assert_eq!(created.session.permission, PermissionLevel::ReadOnly);

    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "write out.txt"}),
        )
        .await?;
    assert!(!submitted.turn_id.is_empty());

    let completed = loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed" {
            break notification.params.clone();
        }
    };
    assert!(completed["error"].is_null());

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert!(
        !attached
            .events
            .iter()
            .any(|event| event.kind == EventKind::PatchProposed),
        "a read-only session must not stage writes"
    );
    let result = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::ToolResult)
        .expect("tool result event");
    assert_eq!(result.payload["ok"], false);
    assert_eq!(result.payload["denied"], true);
    assert!(
        result.payload["output"]
            .as_str()
            .unwrap_or_default()
            .contains("read-only"),
        "tool output was: {}",
        result.payload
    );
    assert!(!dir.join("out.txt").exists());
    // The provider never got a mutating tool in its advertised list.
    let first: Value = serde_json::from_str(&bodies.lock().await[0])?;
    assert!(
        first["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().all(|tool| {
                !matches!(
                    tool["function"]["name"].as_str(),
                    Some("write_file" | "run_command" | "start_command")
                )
            })),
        "read-only tools were: {}",
        first["tools"]
    );

    // Persisted change: row + permission event.
    let changed: SessionResult = client
        .request(
            "session.permission",
            json!({"session_id": session_id, "permission": "workspace-write"}),
        )
        .await?;
    assert_eq!(changed.session.permission, PermissionLevel::WorkspaceWrite);
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let event = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::Permission)
        .expect("permission event");
    assert_eq!(event.payload["from"], "read-only");
    assert_eq!(event.payload["to"], "workspace-write");
    assert_eq!(event.payload["temporary"], false);

    // Temporary escalation applies immediately and is visible on reads...
    let temporary: SessionResult = client
        .request(
            "session.permission",
            json!({
                "session_id": session_id,
                "permission": "full",
                "temporary": true,
            }),
        )
        .await?;
    assert_eq!(temporary.session.permission, PermissionLevel::Full);
    let listed: eix_core::SessionList = client.request("session.list", Value::Null).await?;
    let row = listed
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .expect("session row");
    assert_eq!(row.permission, PermissionLevel::Full);
    // ...and is announced as temporary in the event log.
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let temp_event = attached
        .events
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::Permission)
        .expect("temporary permission event");
    assert_eq!(temp_event.payload["to"], "full");
    assert_eq!(temp_event.payload["temporary"], true);

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn staged_write_can_be_revised_before_approval() -> anyhow::Result<()> {
    let dir = temp_dir("revise");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("out.txt"), "before")?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(tool_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "revise", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "write out.txt"}),
        )
        .await?;

    let patch = loop {
        let notification = client.next_notification().await?;
        if notification.method == "patch.proposed" {
            break notification.params.clone();
        }
    };
    let patch_id = patch["patch_id"].as_str().unwrap().to_string();

    let staged: eix_core::PatchGetResult = client
        .request(
            "patch.get",
            json!({"session_id": session_id, "patch_id": patch_id}),
        )
        .await?;
    assert_eq!(staged.text, "hello");
    assert_eq!(staged.path, "out.txt");

    let revised: eix_core::PatchResult = client
        .request(
            "patch.revise",
            json!({
                "session_id": session_id,
                "patch_id": patch_id,
                "text": "edited before approval\n",
            }),
        )
        .await?;
    assert!(revised.accepted);
    assert_eq!(revised.message.as_deref(), Some("patch updated"));

    let staged: eix_core::PatchGetResult = client
        .request(
            "patch.get",
            json!({"session_id": session_id, "patch_id": patch_id}),
        )
        .await?;
    assert_eq!(staged.text, "edited before approval\n");
    // Nothing has touched the workspace yet.
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, "before");

    let applied: eix_core::PatchResult = client
        .request(
            "patch.apply",
            json!({"session_id": session_id, "patch_id": patch_id}),
        )
        .await?;
    assert!(applied.applied);
    assert_eq!(
        std::fs::read_to_string(dir.join("out.txt"))?,
        "edited before approval\n"
    );

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn command_snapshots_the_workspace_before_running() -> anyhow::Result<()> {
    let dir = temp_dir("command-snapshot");
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::write(dir.join("src/keep.txt"), "before")?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(command_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    // The snapshot, not the filesystem sandbox, is under test here.
    config.tools.sandbox = eix_core::SandboxMode::Off;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "cmd", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let _: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "run it"}),
        )
        .await?;

    let approval = loop {
        let notification = client.next_notification().await?;
        if notification.method == "tool.approval_required" {
            break notification.params.clone();
        }
    };
    assert_eq!(approval["tool"], "run_command");
    let approval_id = approval["approval_id"].as_str().unwrap().to_string();

    let answered: eix_core::ApprovalResult = client
        .request(
            "approval.respond",
            json!({
                "session_id": session_id,
                "approval_id": approval_id,
                "decision": "approve",
            }),
        )
        .await?;
    assert!(answered.accepted);

    let completed = loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed" {
            break notification.params.clone();
        }
    };
    assert!(completed["error"].is_null());

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let snapshot = attached
        .events
        .iter()
        .find(|e| e.kind == EventKind::Snapshot)
        .expect("snapshot event");
    assert_eq!(snapshot.payload["ok"], true);
    assert_eq!(snapshot.payload["tool"], "run_command");
    assert_eq!(snapshot.payload["scope"], "tree");
    assert!(
        snapshot.payload["files"].as_u64().unwrap_or(0) >= 1,
        "snapshot was: {}",
        snapshot.payload
    );

    let snapshot_id = snapshot.payload["snapshot_id"].as_str().unwrap();
    let saved = dir
        .join("data/snapshots")
        .join(&session_id)
        .join(snapshot_id)
        .join("files/src/keep.txt");
    assert_eq!(std::fs::read_to_string(&saved)?, "before");
    assert_eq!(std::fs::read_to_string(dir.join("src/keep.txt"))?, "after");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn staged_write_applies_only_the_selected_hunks() -> anyhow::Result<()> {
    let dir = temp_dir("hunks");
    std::fs::create_dir_all(&dir)?;
    let original = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\nfourteen\nfifteen\nsixteen\nseventeen\neighteen\nnineteen\ntwenty\n";
    let edited = "one\nTWO\nthree\nfour\nfive\nsix\nseven\neight\nnine\nten\neleven\ntwelve\nthirteen\nfourteen\nfifteen\nsixteen\nseventeen\nEIGHTEEN\nnineteen\ntwenty\n";
    std::fs::write(dir.join("out.txt"), original)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(tool_provider_with_content(
        listener,
        Arc::clone(&bodies),
        edited.to_string(),
    ));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "hunks", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "edit out.txt"}),
        )
        .await?;

    let patch = loop {
        let notification = client.next_notification().await?;
        if notification.method == "patch.proposed" {
            break notification.params.clone();
        }
    };
    let hunks = patch["hunks"].as_array().expect("hunk preview");
    assert_eq!(hunks.len(), 2, "hunks were: {hunks:?}");
    let patch_id = patch["patch_id"].as_str().unwrap().to_string();

    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed" {
            break;
        }
    }

    // Apply only the first hunk; the second edit must not be written.
    let answered: eix_core::PatchResult = client
        .request(
            "patch.apply",
            json!({
                "session_id": session_id,
                "patch_id": patch_id,
                "hunks": [0],
            }),
        )
        .await?;
    assert!(answered.accepted);
    assert!(answered.applied);

    let expected = original.replacen("two\n", "TWO\n", 1);
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, expected);

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let applied = attached
        .events
        .iter()
        .find(|e| e.kind == EventKind::PatchApplied)
        .expect("patch applied event");
    assert_eq!(applied.payload["hunks"], json!([0]));

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn attention_runs_the_notification_command() -> anyhow::Result<()> {
    let dir = temp_dir("notify");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(tool_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    let notified_path = dir.join("notified");
    config.notifications.command = Some(format!(
        "printf '%s %s' \"$EIX_SESSION_NAME\" \"$EIX_STATUS\" > {}",
        notified_path.display()
    ));
    config.notifications.cooldown_secs = 0;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "notify", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "write out.txt"}),
        )
        .await?;

    // The command runs off the event loop, so poll briefly for its output.
    let mut notified = String::new();
    for _ in 0..100 {
        if let Ok(text) = tokio::fs::read_to_string(&notified_path).await {
            notified = text;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(notified, "notify awaiting_approval");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn staged_write_can_be_rejected_and_keeps_the_workspace_untouched() -> anyhow::Result<()> {
    let dir = temp_dir("deny");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(tool_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "deny", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "write out.txt"}),
        )
        .await?;

    let patch = loop {
        let notification = client.next_notification().await?;
        if notification.method == "patch.proposed" {
            break notification.params.clone();
        }
    };
    let patch_id = patch["patch_id"].as_str().unwrap().to_string();

    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed" {
            break;
        }
    }

    let answered: eix_core::PatchResult = client
        .request(
            "patch.reject",
            json!({
                "session_id": session_id,
                "patch_id": patch_id,
                "reason": "not this time",
            }),
        )
        .await?;
    assert!(answered.accepted);
    assert!(!answered.applied);

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let rejected = attached
        .events
        .iter()
        .find(|e| e.kind == EventKind::PatchRejected)
        .expect("patch rejected event");
    assert_eq!(rejected.payload["message"], "not this time");
    assert!(!dir.join("out.txt").exists());

    let bodies = bodies.lock().await;
    let second: Value = serde_json::from_str(&bodies[1])?;
    let content = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["role"] == "tool")
        .and_then(|m| m["content"].as_str())
        .unwrap_or_default();
    assert!(content.contains("staged out.txt"), "{content}");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn staged_write_reports_a_conflict_when_the_file_changed() -> anyhow::Result<()> {
    let dir = temp_dir("conflict");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(tool_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "conflict", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "write out.txt"}),
        )
        .await?;

    let patch = loop {
        let notification = client.next_notification().await?;
        if notification.method == "patch.proposed" {
            break notification.params.clone();
        }
    };
    assert_eq!(patch["tool"], "write_file");
    let patch_id = patch["patch_id"].as_str().unwrap().to_string();

    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed" {
            break;
        }
    }

    // The file changes after the diff was generated but before the decision.
    std::fs::write(dir.join("out.txt"), "external")?;

    let answered: eix_core::PatchResult = client
        .request(
            "patch.apply",
            json!({"session_id": session_id, "patch_id": patch_id}),
        )
        .await?;
    assert!(answered.accepted);
    assert!(!answered.applied);
    assert!(answered.conflict);

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let conflict = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::PatchConflict)
        .expect("patch conflict event");
    let output = conflict.payload["message"].as_str().unwrap_or_default();
    assert!(output.contains("changed after the diff"), "{output}");
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, "external");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn staged_writes_are_visible_to_reads() -> anyhow::Result<()> {
    let dir = temp_dir("shadow");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("out.txt"), "on disk")?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(staging_read_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "shadow", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "write then read"}),
        )
        .await?;

    let completed = loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed" {
            break notification.params.clone();
        }
    };
    assert!(completed["error"].is_null());

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let results: Vec<_> = attached
        .events
        .iter()
        .filter(|event| event.kind == EventKind::ToolResult)
        .collect();
    assert_eq!(results.len(), 2, "events were: {:?}", attached.events);
    assert_eq!(results[1].payload["tool"], "read_file");
    assert_eq!(
        results[1].payload["output"], "staged",
        "reads must see the shadow workspace"
    );
    // The patch is still waiting and the real file is untouched.
    assert!(attached
        .events
        .iter()
        .any(|event| event.kind == EventKind::PatchProposed));
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, "on disk");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn background_process_streams_output_and_can_be_killed() -> anyhow::Result<()> {
    let dir = temp_dir("process");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(start_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    config.tools.policy = eix_core::ToolPolicy::Auto;
    config.tools.sandbox = eix_core::SandboxMode::Off;

    let socket = dir.join("eix.sock");
    let data_dir = dir.join("data");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), data_dir.clone()));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "process", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "start the server"}),
        )
        .await?;
    assert!(!submitted.turn_id.is_empty());
    let answer = wait_turn(&mut client).await?;
    assert_eq!(answer, "done");

    // The model got a process id back, read its output, and then finished.
    {
        let bodies = bodies.lock().await;
        assert_eq!(bodies.len(), 3, "start, read, finish");
        let value: Value = serde_json::from_str(
            bodies
                .iter()
                .find(|body| body.matches("\"role\":\"tool\"").count() >= 2)
                .expect("output round trip"),
        )?;
        let tools: Vec<&Value> = value["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "tool")
            .collect();
        assert!(tools[0]["content"]
            .as_str()
            .unwrap()
            .contains("started process"));
        let output = tools[1]["content"].as_str().unwrap();
        assert!(output.starts_with("process "), "output was: {output:?}");
        assert!(output.contains("running"), "output was: {output:?}");
    }

    let list: eix_core::ProcessList = client
        .request("process.list", json!({"session_id": session_id}))
        .await?;
    assert_eq!(list.processes.len(), 1);
    let process = &list.processes[0];
    assert_eq!(process.status, eix_core::ProcessStatus::Running);
    let process_id = process.id.clone();

    let mut text = String::new();
    for _ in 0..100 {
        let output: eix_core::ProcessOutput = client
            .request("process.output", json!({"process_id": process_id}))
            .await?;
        text = output.text;
        if text.contains("e2e-process") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(text.contains("e2e-process"), "output was: {text:?}");

    let killed: eix_core::ProcessKillResult = client
        .request("process.kill", json!({"process_id": process_id}))
        .await?;
    assert!(killed.killed);

    let mut status = eix_core::ProcessStatus::Running;
    for _ in 0..150 {
        let output: eix_core::ProcessOutput = client
            .request("process.output", json!({"process_id": process_id}))
            .await?;
        status = output.status;
        if !status.is_running() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(status, eix_core::ProcessStatus::Killed);

    let again: eix_core::ProcessKillResult = client
        .request("process.kill", json!({"process_id": process_id}))
        .await?;
    assert!(!again.killed);

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let started = attached
        .events
        .iter()
        .position(|event| event.kind == EventKind::ProcessStarted)
        .expect("process started event");
    let exited = attached
        .events
        .iter()
        .position(|event| event.kind == EventKind::ProcessExited)
        .expect("process exited event");
    assert!(started < exited);

    let log = data_dir
        .join("processes")
        .join(&session_id)
        .join(format!("{process_id}.log"));
    assert!(log.exists(), "log at {}", log.display());
    assert!(tokio::fs::read_to_string(&log)
        .await?
        .contains("e2e-process"));

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn fork_copies_a_prefix_into_a_new_idle_session() -> anyhow::Result<()> {
    let dir = temp_dir("fork");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "forkme", "cwd": dir}))
        .await?;
    let source = created.session;
    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": source.id, "text": "hello"}),
        )
        .await?;
    assert_eq!(wait_turn(&mut client).await?, "Hello from mock");

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": source.id}))
        .await?;
    let assistant_seq = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::AssistantMessage)
        .expect("assistant message")
        .seq;
    let expected: Vec<&eix_core::Event> = attached
        .events
        .iter()
        .filter(|event| event.seq < assistant_seq)
        .collect();
    assert!(!expected.is_empty());

    let forked: SessionResult = client
        .request(
            "session.fork",
            json!({"session_id": source.id, "at_seq": assistant_seq}),
        )
        .await?;
    let fork = forked.session;
    assert_eq!(fork.name, "forkme~2");
    assert_eq!(fork.parent_id.as_deref(), Some(source.id.as_str()));
    assert_eq!(fork.forked_from_seq, Some(assistant_seq));
    assert_eq!(fork.status, eix_core::SessionStatus::Idle);
    assert_eq!(fork.cwd, source.cwd);

    // The copy is exactly the source prefix, sequence numbers included.
    let fork_attached: AttachResult = client
        .request("session.attach", json!({"session_id": fork.id}))
        .await?;
    assert_eq!(fork_attached.events.len(), expected.len());
    for (copied, original) in fork_attached.events.iter().zip(expected.iter()) {
        assert_eq!(copied.seq, original.seq);
        assert_eq!(copied.kind, original.kind);
        assert_eq!(copied.payload, original.payload);
    }

    // The fork's search index covers exactly its prefix.
    let hits: SearchResult = client
        .request(
            "search.query",
            json!({"query": "hello", "session_id": fork.id}),
        )
        .await?;
    assert_eq!(hits.hits.len(), 1);
    assert_eq!(hits.hits[0].seq, 1);
    let hits: SearchResult = client
        .request(
            "search.query",
            json!({"query": "Hello from mock", "session_id": fork.id}),
        )
        .await?;
    assert!(hits.hits.is_empty());

    // A full fork copies the current tail and gets the next suffix.
    let forked_again: SessionResult = client
        .request("session.fork", json!({"session_id": source.id}))
        .await?;
    assert_eq!(forked_again.session.name, "forkme~3");
    assert_eq!(
        forked_again.session.forked_from_seq,
        attached.events.last().map(|event| event.seq + 1)
    );

    // Out-of-range cut points are parameter errors.
    let code = client
        .request_error(
            "session.fork",
            json!({"session_id": source.id, "at_seq": 999}),
        )
        .await?;
    assert_eq!(code, "invalid_params");

    // Forking never mutates the source log.
    let after: AttachResult = client
        .request("session.attach", json!({"session_id": source.id}))
        .await?;
    assert_eq!(after.events.len(), attached.events.len());

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn search_indexes_messages_across_sessions_and_short_queries() -> anyhow::Result<()> {
    let dir = temp_dir("search");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let alpha: SessionResult = client
        .request("session.create", json!({"name": "alpha", "cwd": dir}))
        .await?;
    let beta: SessionResult = client
        .request("session.create", json!({"name": "beta", "cwd": dir}))
        .await?;

    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": alpha.session.id, "text": "中文检索测试"}),
        )
        .await?;
    wait_turn(&mut client).await?;
    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({
                "session_id": beta.session.id,
                "text": "the request timeout is 30 seconds",
            }),
        )
        .await?;
    wait_turn(&mut client).await?;

    // Three or more characters use the FTS5 trigram index.
    let hits: SearchResult = client
        .request("search.query", json!({"query": "timeout"}))
        .await?;
    assert_eq!(hits.hits.len(), 1);
    assert_eq!(hits.hits[0].session_name, "beta");
    assert_eq!(hits.hits[0].role, "user");
    assert!(hits.hits[0].snippet.contains("timeout"));

    // CJK: 4 characters are indexed; 2 characters fall back to a scan.
    let hits: SearchResult = client
        .request("search.query", json!({"query": "中文检索"}))
        .await?;
    assert_eq!(hits.hits.len(), 1);
    assert_eq!(hits.hits[0].session_name, "alpha");
    let hits: SearchResult = client
        .request("search.query", json!({"query": "检索"}))
        .await?;
    assert_eq!(hits.hits.len(), 1);

    // Assistant text is indexed too, and the limit is applied.
    let hits: SearchResult = client
        .request(
            "search.query",
            json!({"query": "Hello from mock", "limit": 1}),
        )
        .await?;
    assert_eq!(hits.hits.len(), 1);
    assert_eq!(hits.hits[0].role, "assistant");

    // Scoping to a session excludes the other one.
    let hits: SearchResult = client
        .request(
            "search.query",
            json!({"query": "timeout", "session_id": alpha.session.id}),
        )
        .await?;
    assert!(hits.hits.is_empty());

    // Blank queries are parameter errors, and searching does not mutate logs.
    let code = client
        .request_error("search.query", json!({"query": "   "}))
        .await?;
    assert_eq!(code, "invalid_params");
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": beta.session.id}))
        .await?;
    assert_eq!(
        attached
            .events
            .iter()
            .filter(|event| event.kind == EventKind::UserMessage)
            .count(),
        1
    );

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn recovers_sessions_left_running() -> anyhow::Result<()> {
    let dir = temp_dir("recover");
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir)?;

    {
        let store = eixd::store::Store::open(&data_dir.join("eix.sqlite3"))?;
        let session = eix_core::Session::new("stale", "stale session", "/tmp");
        store.create_session(&session)?;
        store.set_status("stale", eix_core::SessionStatus::Running)?;
    }

    let mut config = Config::default();
    config.provider.base_url = "http://127.0.0.1:1/v1".to_string();
    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), data_dir));

    let mut client = connect(&socket).await?;
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": "stale"}))
        .await?;
    assert_eq!(attached.session.status, eix_core::SessionStatus::Error);
    assert!(attached.events.iter().any(|e| e.kind == EventKind::Error));
    assert!(attached
        .events
        .iter()
        .any(|e| { e.kind == EventKind::StatusChanged && e.payload["to"] == "error" }));

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn rewind_restores_the_workspace_before_a_write() -> anyhow::Result<()> {
    let dir = temp_dir("rewind");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("out.txt"), "before")?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(tool_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    // Execute the write immediately so the snapshot and the write land in one
    // turn; `ask` would stage it and defer the snapshot until apply.
    config.tools.policy = eix_core::ToolPolicy::Auto;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "rewindme", "cwd": dir}))
        .await?;
    let source = created.session;
    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": source.id, "text": "write out.txt"}),
        )
        .await?;
    assert_eq!(wait_turn(&mut client).await?, "done");
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, "hello");

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": source.id}))
        .await?;
    let tool_seq = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::ToolCall)
        .expect("tool call event")
        .seq;
    let snapshot = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::Snapshot)
        .expect("snapshot event");
    assert_eq!(snapshot.payload["ok"], true);
    assert!(snapshot.seq >= tool_seq);

    let rewound: eix_core::RewindResult = client
        .request(
            "session.rewind",
            json!({"session_id": source.id, "at_seq": tool_seq}),
        )
        .await?;
    assert!(rewound.restored, "warnings: {:?}", rewound.warnings);
    assert!(rewound.snapshots >= 1);
    assert!(rewound.files_restored >= 1, "{rewound:?}");
    assert_eq!(
        rewound.session.parent_id.as_deref(),
        Some(source.id.as_str())
    );
    assert_eq!(rewound.session.origin, eix_core::SessionOrigin::Fork);
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, "before");

    // The new branch stops before the tool call, and the source is untouched.
    let branch: AttachResult = client
        .request("session.attach", json!({"session_id": rewound.session.id}))
        .await?;
    assert!(branch.events.iter().all(|event| event.seq < tool_seq));
    assert_eq!(branch.session.status, eix_core::SessionStatus::Idle);
    let after: AttachResult = client
        .request("session.attach", json!({"session_id": source.id}))
        .await?;
    assert_eq!(after.events.len(), attached.events.len());

    // Rewinding past the end only forks: the workspace is already there.
    let noop: eix_core::RewindResult = client
        .request("session.rewind", json!({"session_id": source.id}))
        .await?;
    assert!(noop.restored);
    assert_eq!(noop.snapshots, 0);
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, "before");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(["-c", "user.email=eix@example.com", "-c", "user.name=eix"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git")
}

#[tokio::test]
async fn worktree_sessions_are_created_forked_and_removed() -> anyhow::Result<()> {
    let dir = temp_dir("worktree");
    std::fs::create_dir_all(&dir)?;
    let init = git(&dir, &["init", "-q"]);
    assert!(
        init.status.success(),
        "git init: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let commit = git(&dir, &["commit", "-q", "--allow-empty", "-m", "init"]);
    assert!(
        commit.status.success(),
        "git commit: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let repo = dir.canonicalize()?;

    let mut config = Config::default();
    config.provider.base_url = "http://127.0.0.1:1/v1".to_string();
    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "feature", "cwd": dir, "worktree": true}),
        )
        .await?;
    let session = created.session;
    let worktree = session.worktree.clone().expect("worktree path");
    assert!(worktree.is_dir());
    assert_eq!(session.cwd, worktree);
    assert!(worktree.starts_with(repo.join(".worktrees")));

    // A fork inside a repo gets its own worktree by default.
    let forked: SessionResult = client
        .request("session.fork", json!({"session_id": session.id}))
        .await?;
    assert!(forked.session.has_worktree());
    assert_eq!(
        forked.session.parent_id.as_deref(),
        Some(session.id.as_str())
    );
    let fork_worktree = forked.session.worktree.clone().expect("fork worktree");
    assert!(fork_worktree.is_dir());
    assert_ne!(fork_worktree, worktree);

    // `worktree: false` shares the source workspace instead.
    let shared: SessionResult = client
        .request(
            "session.fork",
            json!({"session_id": session.id, "worktree": false, "name": "shared"}),
        )
        .await?;
    assert!(!shared.session.has_worktree());
    assert_eq!(shared.session.cwd, worktree);

    // A dirty worktree is kept unless the caller explicitly forces removal.
    std::fs::write(worktree.join("dirty.txt"), "x")?;
    let refused: eix_core::WorktreeRemoveResult = client
        .request("session.worktree.remove", json!({"session_id": session.id}))
        .await?;
    assert!(!refused.removed);
    assert!(refused.message.contains("--force"), "{}", refused.message);

    let removed: eix_core::WorktreeRemoveResult = client
        .request(
            "session.worktree.remove",
            json!({"session_id": session.id, "force": true}),
        )
        .await?;
    assert!(removed.removed, "{}", removed.message);
    assert!(!worktree.exists());
    let after: SessionResult = client
        .request("session.get", json!({"session_id": session.id}))
        .await?;
    assert!(after.session.worktree.is_none());
    assert_eq!(after.session.cwd, repo);

    // Removing one worktree never touches another branch's.
    assert!(fork_worktree.is_dir());
    let _ = client
        .request::<eix_core::WorktreeRemoveResult>(
            "session.worktree.remove",
            json!({"session_id": forked.session.id, "force": true}),
        )
        .await?;

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

/// Newline-JSON client for the TCP listener (no auto-start, no Unix socket).
struct RawTcp {
    reader: BufReader<ReadHalf<tokio::net::TcpStream>>,
    writer: WriteHalf<tokio::net::TcpStream>,
    next_id: u64,
}

impl RawTcp {
    async fn connect(addr: &str) -> anyhow::Result<Self> {
        let mut last = None;
        for _ in 0..100 {
            match tokio::net::TcpStream::connect(addr).await {
                Ok(stream) => {
                    let (read, write) = tokio::io::split(stream);
                    return Ok(Self {
                        reader: BufReader::new(read),
                        writer: write,
                        next_id: 1,
                    });
                }
                Err(err) => {
                    last = Some(err);
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
        Err(anyhow::anyhow!(
            "cannot connect to tcp {addr}: {}",
            last.map(|err| err.to_string()).unwrap_or_default()
        ))
    }

    async fn send(&mut self, method: &str, params: Value) -> anyhow::Result<Option<Response>> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_vec(&Request::new(id, method, params))?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        self.writer.flush().await?;
        let mut buf = String::new();
        if self.reader.read_line(&mut buf).await? == 0 {
            return Ok(None);
        }
        match serde_json::from_str::<ServerMessage>(buf.trim_end())? {
            ServerMessage::Response(response) => Ok(Some(response)),
            ServerMessage::Notification(_) => Ok(None),
        }
    }

    async fn recv_notification(&mut self) -> anyhow::Result<Option<String>> {
        let mut buf = String::new();
        if self.reader.read_line(&mut buf).await? == 0 {
            return Ok(None);
        }
        Ok(
            match serde_json::from_str::<ServerMessage>(buf.trim_end())? {
                ServerMessage::Notification(note) => Some(note.method),
                ServerMessage::Response(_) => None,
            },
        )
    }
}

#[tokio::test]
async fn tcp_attach_requires_the_configured_token() -> anyhow::Result<()> {
    let dir = temp_dir("remote");
    std::fs::create_dir_all(&dir)?;
    // Reserve an ephemeral port, then let the daemon take it.
    let probe = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = probe.local_addr()?.to_string();
    drop(probe);

    let mut config = Config::default();
    config.provider.base_url = "http://127.0.0.1:1/v1".to_string();
    config.server.tcp = Some(addr.clone());
    config.server.token = Some("s3cret".to_string());
    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    // The local Unix socket stays trusted and needs no token.
    let mut unix = connect(&socket).await?;
    let hello: HelloResult = unix.request("hello", Value::Null).await?;
    assert_eq!(hello.protocol, PROTOCOL_VERSION);

    // No auth: the first request is rejected and the connection closes.
    let mut raw = RawTcp::connect(&addr).await?;
    let denied = raw.send("hello", Value::Null).await?.expect("response");
    let error = denied.error.expect("unauthorized error");
    assert_eq!(error.code, "unauthorized");

    // Wrong token is rejected too.
    let mut raw = RawTcp::connect(&addr).await?;
    let denied = raw
        .send("auth", json!({"token": "wrong"}))
        .await?
        .expect("response");
    assert_eq!(denied.error.expect("unauthorized").code, "unauthorized");

    // The right token unlocks the normal protocol.
    let mut raw = RawTcp::connect(&addr).await?;
    let auth = raw
        .send("auth", json!({"token": "s3cret"}))
        .await?
        .expect("response");
    assert_eq!(auth.result.expect("auth result")["authenticated"], true);
    let pong = raw.send("ping", Value::Null).await?.expect("response");
    assert_eq!(pong.result.expect("ping result")["pong"], true);
    let sessions: eix_core::SessionList = serde_json::from_value(
        raw.send("session.list", Value::Null)
            .await?
            .expect("response")
            .result
            .expect("session list"),
    )?;
    assert!(sessions.sessions.is_empty());

    // An authenticated TCP client gets the same event broadcast as a local
    // Unix client.
    unix.request::<SessionResult>(
        "session.create",
        json!({"name": "remote", "cwd": dir.clone()}),
    )
    .await?;
    let seen = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match raw.recv_notification().await? {
                Some(method) if method == "session.updated" => return Ok::<_, anyhow::Error>(true),
                Some(_) => continue,
                None => return Ok(false),
            }
        }
    })
    .await??;
    assert!(
        seen,
        "authenticated TCP client did not receive the session.updated broadcast"
    );

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn session_subscribe_filters_broadcasts_per_connection() -> anyhow::Result<()> {
    let dir = temp_dir("subscribe");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let data_dir = dir.join("data");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), data_dir.clone()));

    // Create the two sessions on one connection, then open a second one that
    // only subscribes to `a` (a fresh connection has no backlog).
    let mut worker = connect(&socket).await?;
    let a: SessionResult = worker
        .request("session.create", json!({"name": "a", "cwd": dir}))
        .await?;
    let b: SessionResult = worker
        .request("session.create", json!({"name": "b", "cwd": dir}))
        .await?;
    let (a, b) = (a.session.id, b.session.id);

    let mut watcher = connect(&socket).await?;
    let subscribed: eix_core::SubscribeResult = watcher
        .request("session.subscribe", json!({"session_ids": [a.clone()]}))
        .await?;
    assert_eq!(
        subscribed.session_ids.as_deref(),
        Some([a.clone()].as_slice())
    );

    // A turn on `b` must not reach the watcher.
    let submitted: SubmitResult = worker
        .request("prompt.submit", json!({"session_id": b, "text": "b turn"}))
        .await?;
    loop {
        let notification = worker.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["session_id"] == b.as_str()
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            break;
        }
    }

    // A turn on `a` does reach it, and nothing from `b` slips through: the
    // broadcast bus is ordered, so `b`'s events would arrive first.
    let submitted: SubmitResult = worker
        .request("prompt.submit", json!({"session_id": a, "text": "a turn"}))
        .await?;
    loop {
        let notification = watcher.next_notification().await?;
        if let Some(session_id) = notification
            .params
            .get("session_id")
            .and_then(Value::as_str)
        {
            assert_eq!(
                session_id,
                a.as_str(),
                "unsubscribed event leaked: {notification:?}"
            );
        }
        if notification.method == "turn.completed"
            && notification.params["session_id"] == a.as_str()
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            break;
        }
    }

    // `null` restores the full stream.
    let all: eix_core::SubscribeResult = watcher
        .request("session.subscribe", json!({"session_ids": null}))
        .await?;
    assert!(all.session_ids.is_none());

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn drafts_round_trip_and_clear() -> anyhow::Result<()> {
    let dir = temp_dir("draft");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;

    let socket = dir.join("eix.sock");
    let data_dir = dir.join("data");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), data_dir.clone()));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "d", "cwd": dir}))
        .await?;
    let id = created.session.id;

    let empty: eix_core::DraftResult = client
        .request("draft.get", json!({"session_id": id}))
        .await?;
    assert_eq!(empty.text, "");
    assert_eq!(empty.updated_at, 0);

    let saved: eix_core::DraftResult = client
        .request(
            "draft.set",
            json!({"session_id": id, "text": "half a thought"}),
        )
        .await?;
    assert_eq!(saved.text, "half a thought");
    assert!(saved.updated_at > 0);

    let loaded: eix_core::DraftResult = client
        .request("draft.get", json!({"session_id": id}))
        .await?;
    assert_eq!(loaded.text, "half a thought");

    // An empty text deletes the draft.
    let cleared: eix_core::DraftResult = client
        .request("draft.set", json!({"session_id": id, "text": ""}))
        .await?;
    assert_eq!(cleared.updated_at, 0);
    let loaded: eix_core::DraftResult = client
        .request("draft.get", json!({"session_id": id}))
        .await?;
    assert_eq!(loaded.text, "");

    // Unknown sessions are rejected.
    let code = client
        .request_error("draft.get", json!({"session_id": "nope"}))
        .await?;
    assert_eq!(code, "not_found");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn spawn_agent_runs_a_readonly_child_and_returns_its_answer() -> anyhow::Result<()> {
    let dir = temp_dir("agent");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(agent_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "parent", "cwd": dir}))
        .await?;
    let parent = created.session;
    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": parent.id, "text": "investigate"}),
        )
        .await?;
    // Sub-agents stream deltas on the same connection, so wait for the
    // parent's completion and ignore the child's.
    let mut text = String::new();
    loop {
        let notification = client.next_notification().await?;
        match notification.method.as_str() {
            "message.delta" if notification.params["session_id"] == parent.id.as_str() => {
                if let Some(delta) = notification.params["delta"].as_str() {
                    text.push_str(delta);
                }
            }
            "turn.completed" if notification.params["session_id"] == parent.id.as_str() => {
                assert!(notification.params["error"].is_null());
                break;
            }
            _ => {}
        }
    }
    assert_eq!(text, "parent done");

    let list: eix_core::SessionList = client.request("session.list", Value::Null).await?;
    let child = list
        .sessions
        .iter()
        .find(|session| session.origin == eix_core::SessionOrigin::Agent)
        .expect("agent child session");
    assert_eq!(child.parent_id.as_deref(), Some(parent.id.as_str()));
    assert_eq!(child.name, "parent/scout-1");
    assert_eq!(child.cwd, parent.cwd);
    assert_eq!(child.model, parent.model);

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": child.id}))
        .await?;
    assert_eq!(attached.session.status, eix_core::SessionStatus::Idle);
    let user = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::UserMessage)
        .expect("child prompt");
    assert_eq!(user.payload["text"], "investigate");
    let assistant = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::AssistantMessage)
        .expect("child answer");
    assert_eq!(assistant.payload["text"], "child answer");

    // The parent receives the child's answer as the tool result.
    let parent_events: AttachResult = client
        .request("session.attach", json!({"session_id": parent.id}))
        .await?;
    let result = parent_events
        .events
        .iter()
        .find(|event| event.kind == EventKind::ToolResult && event.payload["tool"] == "spawn_agent")
        .expect("spawn_agent result");
    assert_eq!(result.payload["ok"], true);
    let output = result.payload["output"].as_str().expect("output");
    assert!(output.contains("child answer"), "{output}");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn nvim_headless_renders_the_transcript() -> anyhow::Result<()> {
    // The Neovim frontend is optional: skip when the binary is missing.
    let has_nvim = std::process::Command::new("nvim")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    if !has_nvim {
        eprintln!("skipping: nvim not found");
        return Ok(());
    }

    let dir = temp_dir("nvim");
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), data_dir.clone()));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "editor", "cwd": dir}))
        .await?;
    let session = created.session;
    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session.id, "text": "say hi"}),
        )
        .await?;
    assert_eq!(wait_turn(&mut client).await?, "Hello from mock");

    let plugin = eix_core::nvim_plugin::write(&data_dir)?;
    // Wait until the rendered buffer actually contains the answer, then dump
    // it. Plain `print` goes to stdout under `--headless`.
    let wait_and_print = concat!(
        "lua local function ready() ",
        "if not (_G.eix_nvim and _G.eix_nvim.buffer) then return false end ",
        "local lines = vim.api.nvim_buf_get_lines(_G.eix_nvim.buffer, 0, -1, false) ",
        "return table.concat(lines, \"\\n\"):find(\"Hello from mock\", 1, true) ~= nil end ",
        "local ok = vim.wait(8000, ready, 50) ",
        "if ok then print(table.concat(vim.api.nvim_buf_get_lines(_G.eix_nvim.buffer, 0, -1, false), \"\\n\")) ",
        "else print(\"TIMEOUT connected=\"..tostring(_G.eix_nvim.connected)..\" attached=\"..tostring(_G.eix_nvim.attached)..\" buffer=\"..tostring(_G.eix_nvim.buffer)) end",
    );
    let plugin_for_nvim = plugin.clone();
    let socket_for_nvim = socket.clone();
    let session_id = session.id.clone();
    // `#[tokio::test]` uses a current-thread runtime: blocking on the child
    // would starve the daemon, so run nvim on the blocking pool and keep
    // polling the daemon while it works.
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new("nvim")
            .args(["--headless", "-u", "NONE", "-n"])
            .arg("--cmd")
            .arg("lua dofile(vim.env.EIX_NVIM_PLUGIN)")
            .arg("-c")
            .arg(format!("Eix attach {session_id}"))
            .arg("-c")
            .arg(wait_and_print)
            .arg("-c")
            .arg("qa!")
            .env("EIX_NVIM_PLUGIN", &plugin_for_nvim)
            .env("EIX_NVIM_SOCKET", &socket_for_nvim)
            .env("EIX_NVIM_SESSION", "")
            .output()
    })
    .await??;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "nvim failed: {stderr}\n{stdout}");
    // Under `--headless`, Lua `print` goes to stderr; accept either stream.
    let shown = format!("{stdout}{stderr}");
    assert!(
        shown.contains("say hi"),
        "transcript missing the prompt:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(
        shown.contains("Hello from mock"),
        "transcript missing the answer:\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

struct TestClient {
    reader: ClientReader,
    writer: WriteHalf<UnixStream>,
    next_id: u64,
    /// Notifications seen while waiting for a response.
    pending: std::collections::VecDeque<eix_core::Notification>,
}

impl TestClient {
    async fn request<T: serde::de::DeserializeOwned>(
        &mut self,
        method: &str,
        params: Value,
    ) -> anyhow::Result<T> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_vec(&Request::new(id, method, params))?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        self.writer.flush().await?;
        loop {
            match read_message(&mut self.reader).await? {
                ServerMessage::Response(Response {
                    id: response_id,
                    result,
                    error,
                }) if response_id == id => {
                    if let Some(error) = error {
                        anyhow::bail!("{}: {}", error.code, error.message);
                    }
                    return Ok(serde_json::from_value(result.unwrap_or(Value::Null))?);
                }
                ServerMessage::Response(other) => {
                    anyhow::bail!("unexpected response id {}", other.id)
                }
                ServerMessage::Notification(notification) => self.pending.push_back(notification),
            }
        }
    }

    async fn next_notification(&mut self) -> anyhow::Result<eix_core::Notification> {
        if let Some(notification) = self.pending.pop_front() {
            return Ok(notification);
        }
        match read_message(&mut self.reader).await? {
            ServerMessage::Notification(notification) => Ok(notification),
            ServerMessage::Response(response) => {
                anyhow::bail!("unexpected response id {}", response.id)
            }
        }
    }

    /// Send a request that is expected to fail and return the error code.
    async fn request_error(&mut self, method: &str, params: Value) -> anyhow::Result<String> {
        let id = self.next_id;
        self.next_id += 1;
        let mut line = serde_json::to_vec(&Request::new(id, method, params))?;
        line.push(b'\n');
        self.writer.write_all(&line).await?;
        self.writer.flush().await?;
        loop {
            match read_message(&mut self.reader).await? {
                ServerMessage::Response(Response {
                    id: response_id,
                    result,
                    error,
                }) if response_id == id => {
                    if let Some(error) = error {
                        return Ok(error.code);
                    }
                    anyhow::bail!("request unexpectedly succeeded: {result:?}");
                }
                ServerMessage::Response(other) => {
                    anyhow::bail!("unexpected response id {}", other.id)
                }
                ServerMessage::Notification(notification) => self.pending.push_back(notification),
            }
        }
    }
}

async fn connect(socket: &Path) -> anyhow::Result<TestClient> {
    let mut last = None;
    for _ in 0..100 {
        match UnixStream::connect(socket).await {
            Ok(stream) => {
                let (read, write) = tokio::io::split(stream);
                return Ok(TestClient {
                    reader: BufReader::new(read),
                    writer: write,
                    next_id: 1,
                    pending: std::collections::VecDeque::new(),
                });
            }
            Err(err) => {
                last = Some(err);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
    anyhow::bail!("cannot connect to daemon: {:?}", last)
}

async fn read_message(reader: &mut ClientReader) -> anyhow::Result<ServerMessage> {
    let mut line = String::new();
    if reader.read_line(&mut line).await? == 0 {
        anyhow::bail!("daemon closed the connection");
    }
    Ok(serde_json::from_str(line.trim_end())?)
}

async fn mock_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let bodies = Arc::clone(&bodies);
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let header_end = loop {
                let mut chunk = [0u8; 1024];
                let Ok(n) = socket.read(&mut chunk).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_header_end(&buf) {
                    break pos;
                }
            };
            let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
            let length: usize = headers
                .lines()
                .find_map(|line| line.strip_prefix("content-length:"))
                .and_then(|value| value.trim().parse().ok())
                .unwrap_or(0);
            while buf.len() < header_end + length {
                let mut chunk = [0u8; 1024];
                let Ok(n) = socket.read(&mut chunk).await else {
                    return;
                };
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let body = String::from_utf8_lossy(&buf[header_end..header_end + length]).to_string();
            bodies.lock().await.push(body);

            let events = concat!(
                "data: {\"model\":\"mock-1\",\"choices\":[{\"delta\":{\"content\":\"Hello \"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"from \"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"mock\"},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":3,\"completion_tokens\":2,\"total_tokens\":5}}\n\n",
                "data: [DONE]\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                events.len(),
                events
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        });
    }
}

async fn slow_provider(listener: TcpListener) {
    let Ok((mut socket, _)) = listener.accept().await else {
        return;
    };

    let mut buf = Vec::new();
    let header_end = loop {
        let mut chunk = [0u8; 1024];
        let Ok(n) = socket.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            return;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let length: usize = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < header_end + length {
        let mut chunk = [0u8; 1024];
        let Ok(n) = socket.read(&mut chunk).await else {
            return;
        };
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }

    let mut events =
        vec!["data: {\"choices\":[{\"delta\":{\"content\":\"part\"}}]}\n\n".to_string()];
    for _ in 0..20 {
        events.push("data: {\"choices\":[{\"delta\":{\"content\":\".\"}}]}\n\n".to_string());
    }
    events.push("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n".to_string());
    events.push("data: [DONE]\n\n".to_string());

    let total: usize = events.iter().map(String::len).sum();
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
    );
    if socket.write_all(header.as_bytes()).await.is_err() {
        return;
    }
    for event in events {
        if socket.write_all(event.as_bytes()).await.is_err() {
            return;
        }
        if socket.flush().await.is_err() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(80)).await;
    }
}

async fn tool_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
    tool_provider_with_content(listener, bodies, "hello".to_string()).await;
}
async fn tool_provider_with_content(
    listener: TcpListener,
    bodies: Arc<Mutex<Vec<String>>>,
    content: String,
) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let bodies = Arc::clone(&bodies);
        let content = content.clone();
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            bodies.lock().await.push(body);

            let has_tool_result = value["messages"]
                .as_array()
                .map(|messages| messages.iter().any(|message| message["role"] == "tool"))
                .unwrap_or(false);
            let events = if has_tool_result {
                let chunk = json!({
                    "model": "mock-tools",
                    "choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}],
                });
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            } else {
                let arguments = json!({"path": "out.txt", "content": content}).to_string();
                let chunk = json!({
                    "model": "mock-tools",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_1",
                                "type": "function",
                                "function": {"name": "write_file", "arguments": arguments},
                            }]
                        },
                        "finish_reason": "tool_calls",
                    }],
                });
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            };
            write_sse(&mut socket, &events).await;
        });
    }
}

/// A provider for the sub-agent test: the parent (whose tool list contains
/// `spawn_agent`) asks for one child; the child (read-only tools) answers;
/// the parent then stops with "parent done".
async fn agent_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let bodies = Arc::clone(&bodies);
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            bodies.lock().await.push(body);

            let has_tool_result = value["messages"]
                .as_array()
                .map(|messages| messages.iter().any(|message| message["role"] == "tool"))
                .unwrap_or(false);
            let can_spawn = value["tools"]
                .as_array()
                .map(|tools| {
                    tools
                        .iter()
                        .any(|tool| tool["function"]["name"] == "spawn_agent")
                })
                .unwrap_or(false);

            let events = if has_tool_result {
                let chunk = json!({
                    "model": "mock-agent",
                    "choices": [{"delta": {"content": "parent done"}, "finish_reason": "stop"}],
                });
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            } else if can_spawn {
                let arguments = json!({"prompt": "investigate", "name": "scout"}).to_string();
                let chunk = json!({
                    "model": "mock-agent",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_agent",
                                "type": "function",
                                "function": {"name": "spawn_agent", "arguments": arguments},
                            }]
                        },
                        "finish_reason": "tool_calls",
                    }],
                });
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            } else {
                let chunk = json!({
                    "model": "mock-agent",
                    "choices": [{"delta": {"content": "child answer"}, "finish_reason": "stop"}],
                });
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            };
            write_sse(&mut socket, &events).await;
        });
    }
}

async fn command_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let bodies = Arc::clone(&bodies);
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            bodies.lock().await.push(body);
            let has_tool_result = value["messages"]
                .as_array()
                .map(|messages| messages.iter().any(|message| message["role"] == "tool"))
                .unwrap_or(false);
            let events = if has_tool_result {
                let chunk = json!({
                    "model": "mock-tools",
                    "choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}],
                });
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            } else {
                let arguments = json!({"command": "printf after > src/keep.txt"}).to_string();
                let chunk = json!({
                    "model": "mock-tools",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_1",
                                "type": "function",
                                "function": {"name": "run_command", "arguments": arguments},
                            }]
                        },
                        "finish_reason": "tool_calls",
                    }],
                });
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            };
            write_sse(&mut socket, &events).await;
        });
    }
}

/// A provider that stages a write, then reads the same file back, then stops.
async fn staging_read_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let bodies = Arc::clone(&bodies);
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            bodies.lock().await.push(body);
            let tool_messages = value["messages"]
                .as_array()
                .map(|messages| {
                    messages
                        .iter()
                        .filter(|message| message["role"] == "tool")
                        .count()
                })
                .unwrap_or(0);
            let events = match tool_messages {
                0 => tool_call_events(
                    "call_1",
                    "write_file",
                    &json!({"path": "out.txt", "content": "staged"}).to_string(),
                ),
                1 => tool_call_events(
                    "call_2",
                    "read_file",
                    &json!({"path": "out.txt"}).to_string(),
                ),
                _ => {
                    let chunk = json!({
                        "model": "mock-tools",
                        "choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}],
                    });
                    format!("data: {chunk}\n\ndata: [DONE]\n\n")
                }
            };
            write_sse(&mut socket, &events).await;
        });
    }
}

fn tool_call_events(call_id: &str, tool: &str, arguments: &str) -> String {
    let chunk = json!({
        "model": "mock-tools",
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": {"name": tool, "arguments": arguments},
                }]
            },
            "finish_reason": "tool_calls",
        }],
    });
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

async fn start_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let bodies = Arc::clone(&bodies);
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            bodies.lock().await.push(body);
            let tool_results = value["messages"]
                .as_array()
                .map(|messages| {
                    messages
                        .iter()
                        .filter(|message| message["role"] == "tool")
                        .count()
                })
                .unwrap_or(0);
            let events = if tool_results == 0 {
                let arguments = json!({"command": "echo e2e-process && sleep 30"}).to_string();
                let chunk = json!({
                    "model": "mock-tools",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_1",
                                "type": "function",
                                "function": {"name": "start_command", "arguments": arguments},
                            }]
                        },
                        "finish_reason": "tool_calls",
                    }],
                });
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            } else if tool_results == 1 {
                // Read the newest process without naming it.
                let arguments = json!({"max_bytes": 200}).to_string();
                let chunk = json!({
                    "model": "mock-tools",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_2",
                                "type": "function",
                                "function": {"name": "process_output", "arguments": arguments},
                            }]
                        },
                        "finish_reason": "tool_calls",
                    }],
                });
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            } else {
                let chunk = json!({
                    "model": "mock-tools",
                    "choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}],
                });
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            };
            write_sse(&mut socket, &events).await;
        });
    }
}

async fn read_http_body(socket: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let header_end = loop {
        let mut chunk = [0u8; 1024];
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_header_end(&buf) {
            break pos;
        }
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_ascii_lowercase();
    let length: usize = headers
        .lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    while buf.len() < header_end + length {
        let mut chunk = [0u8; 1024];
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Some(String::from_utf8_lossy(&buf[header_end..header_end + length]).to_string())
}

async fn write_sse(socket: &mut tokio::net::TcpStream, events: &str) {
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        events.len(),
        events
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.flush().await;
}

fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

fn temp_dir(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("eix-e2e-{label}-{}-{nanos}", std::process::id()))
}
