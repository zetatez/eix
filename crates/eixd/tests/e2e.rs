//! End-to-end test: real Unix socket, real SQLite, streaming mock provider.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eix_core::{
    AttachResult, Config, EventKind, HelloResult, Request, Response, ServerMessage, SessionResult,
    SubmitResult, PROTOCOL_VERSION,
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
    assert_eq!(body["messages"][1]["role"], "user");
    assert_eq!(body["messages"][1]["content"], "say hi");

    assert!(data_dir.join("eix.sqlite3").exists());

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
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
