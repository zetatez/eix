//! End-to-end test: real Unix socket, real SQLite, streaming mock provider.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eix_core::{
    AttachResult, Config, EventKind, EventsResult, HelloResult, ModelConfig, NamedProviderConfig,
    PermissionAction, PermissionLevel, PermissionRule, Request, Response, SearchResult,
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
    drop(bodies);

    // Long sessions open at their tail; older pages are fetched with
    // `before_seq` using the first loaded sequence.
    let tail: AttachResult = client
        .request(
            "session.attach",
            json!({"session_id": session_id, "tail": 2}),
        )
        .await?;
    assert!(tail.has_more);
    assert_eq!(tail.events.len(), 2);
    assert!(tail
        .events
        .iter()
        .any(|event| event.kind == EventKind::AssistantMessage));
    let page: EventsResult = client
        .request(
            "session.events",
            json!({
                "session_id": session_id,
                "before_seq": tail.events[0].seq,
                "limit": 10,
            }),
        )
        .await?;
    assert!(page
        .events
        .iter()
        .all(|event| event.seq < tail.events[0].seq));
    assert!(page
        .events
        .iter()
        .any(|event| event.kind == EventKind::UserMessage));
    assert!(!page.has_more);

    assert!(data_dir.join("eix.sqlite3").exists());

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn disconnecting_a_client_does_not_cancel_the_turn() -> anyhow::Result<()> {
    let dir = temp_dir("client-disconnect");
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

    let mut first = connect(&socket).await?;
    let created: SessionResult = first
        .request("session.create", json!({"name": "disconnect", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    let submitted: SubmitResult = first
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "keep going"}),
        )
        .await?;
    assert!(!submitted.turn_id.is_empty());

    // Wait until the provider has started streaming, then close the client
    // without cancelling the turn.
    loop {
        let notification = first.next_notification().await?;
        if notification.method == "message.delta"
            && notification.params["session_id"] == session_id.as_str()
        {
            break;
        }
    }
    drop(first);
    tokio::time::sleep(Duration::from_millis(100)).await;

    let mut second = connect(&socket).await?;
    let mut attached = None;
    for _ in 0..100 {
        let current: AttachResult = second
            .request("session.attach", json!({"session_id": session_id}))
            .await?;
        if current.session.status == eix_core::SessionStatus::Idle {
            attached = Some(current);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let attached = attached.expect("the turn should finish after the client disconnects");
    let assistant = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::AssistantMessage)
        .expect("assistant message persisted");
    let text = assistant.payload["text"].as_str().unwrap_or_default();
    assert!(text.starts_with("part"), "{text}");
    assert_eq!(attached.session.status, eix_core::SessionStatus::Idle);

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn daemon_retries_transient_provider_failures() -> anyhow::Result<()> {
    let dir = temp_dir("retry");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(flaky_provider(listener, 2, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    config.provider.retry_attempts = 3;
    config.provider.retry_base_ms = 1;
    config.provider.retry_max_secs = 1;

    let socket = dir.join("eix.sock");
    let data_dir = dir.join("data");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), data_dir));

    let mut client = connect(&socket).await?;
    let _: HelloResult = client.request("hello", Value::Null).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "retry", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "retry please"}),
        )
        .await?;

    let mut text = String::new();
    let mut retries = 0;
    let completed = loop {
        let notification = client.next_notification().await?;
        match notification.method.as_str() {
            "message.delta"
                if notification.params["session_id"] == session_id.as_str()
                    && notification.params["turn_id"] == submitted.turn_id.as_str() =>
            {
                text.push_str(notification.params["delta"].as_str().unwrap_or_default());
            }
            "turn.retrying" => {
                assert_eq!(notification.params["session_id"], session_id);
                assert_eq!(notification.params["turn_id"], submitted.turn_id);
                assert!(notification.params["attempt"].as_u64().unwrap() >= 2);
                assert_eq!(notification.params["kind"], "transient");
                retries += 1;
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

    assert_eq!(retries, 2);
    assert_eq!(text, "recovered");
    assert!(completed["error"].is_null());
    assert_eq!(bodies.lock().await.len(), 3);

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn daemon_honours_rate_limit_retry_after() -> anyhow::Result<()> {
    let dir = temp_dir("retry-rate-limit");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(rate_limit_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    config.provider.retry_attempts = 2;
    config.provider.retry_base_ms = 500;
    config.provider.retry_max_secs = 30;

    let socket = dir.join("eix.sock");
    let data_dir = dir.join("data");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), data_dir));

    let mut client = connect(&socket).await?;
    let _: HelloResult = client.request("hello", Value::Null).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "rate-limit", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "hello"}),
        )
        .await?;

    let mut text = String::new();
    let mut retries = 0;
    let completed = loop {
        let notification = client.next_notification().await?;
        match notification.method.as_str() {
            "message.delta" => text.push_str(notification.params["delta"].as_str().unwrap()),
            "turn.retrying" => {
                assert_eq!(notification.params["kind"], "rate_limited");
                assert!(
                    notification.params["delay_ms"].as_u64().unwrap() >= 900,
                    "Retry-After should be honoured: {}",
                    notification.params["delay_ms"]
                );
                retries += 1;
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

    assert_eq!(retries, 1);
    assert_eq!(text, "recovered");
    assert!(completed["error"].is_null());
    assert_eq!(bodies.lock().await.len(), 2);

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn daemon_discards_partial_output_before_retrying() -> anyhow::Result<()> {
    let dir = temp_dir("retry-partial");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(partial_failure_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    config.provider.retry_attempts = 2;
    config.provider.retry_base_ms = 1;
    config.provider.retry_max_secs = 1;

    let socket = dir.join("eix.sock");
    let data_dir = dir.join("data");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), data_dir));

    let mut client = connect(&socket).await?;
    let _: HelloResult = client.request("hello", Value::Null).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "partial", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "hello"}),
        )
        .await?;

    let mut text = String::new();
    let mut resets = 0;
    let completed = loop {
        let notification = client.next_notification().await?;
        match notification.method.as_str() {
            "message.delta" => text.push_str(notification.params["delta"].as_str().unwrap()),
            "message.reset" => {
                resets += 1;
                text.clear();
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

    assert_eq!(resets, 1);
    assert_eq!(text, "recovered");
    assert!(completed["error"].is_null());
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let assistant = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::AssistantMessage)
        .expect("assistant event");
    assert_eq!(assistant.payload["text"], "recovered");
    assert!(!assistant.payload["text"]
        .as_str()
        .unwrap()
        .contains("partial"));

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn daemon_does_not_retry_an_exhausted_quota() -> anyhow::Result<()> {
    let dir = temp_dir("quota");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(quota_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    config.provider.retry_attempts = 3;
    config.provider.retry_base_ms = 1;
    config.provider.retry_max_secs = 1;

    let socket = dir.join("eix.sock");
    let data_dir = dir.join("data");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), data_dir));

    let mut client = connect(&socket).await?;
    let _: HelloResult = client.request("hello", Value::Null).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "quota", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "hello"}),
        )
        .await?;

    let completed = loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["session_id"] == session_id.as_str()
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            break notification.params.clone();
        }
        assert_ne!(notification.method, "turn.retrying");
    };

    let error = completed["error"].as_str().unwrap_or_default();
    assert!(error.contains("quota"), "{error}");
    assert_eq!(bodies.lock().await.len(), 1);

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

#[tokio::test]
async fn context_clear_starts_a_fresh_context() -> anyhow::Result<()> {
    let dir = temp_dir("clear");
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
        .request("session.create", json!({"name": "clear", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "first question"}),
        )
        .await?;
    wait_turn(&mut client).await?;

    let cleared: eix_core::ContextLedgerResult = client
        .request("context.clear", json!({"session_id": session_id}))
        .await?;
    assert!(
        cleared
            .entries
            .iter()
            .any(|entry| entry.id.starts_with('e') && entry.dropped),
        "earlier messages are marked dropped"
    );

    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "second question"}),
        )
        .await?;
    wait_turn(&mut client).await?;

    let bodies = bodies.lock().await;
    let second: Value = serde_json::from_str(&bodies[1])?;
    let second_text = second["messages"].to_string();
    assert!(
        !second_text.contains("first question"),
        "the cleared prompt is not resent: {second_text}"
    );
    assert!(
        !second_text.contains("Hello from mock"),
        "the cleared answer is not resent: {second_text}"
    );
    assert!(second_text.contains("second question"));

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert!(attached
        .events
        .iter()
        .any(|event| event.kind == EventKind::ContextCleared));
    // The log keeps the earlier turns for the transcript.
    assert!(attached
        .events
        .iter()
        .any(|event| event.kind == EventKind::UserMessage
            && event.payload["text"] == "first question"));

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn context_compact_can_generate_a_summary() -> anyhow::Result<()> {
    let dir = temp_dir("compact");
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
        .request("session.create", json!({"name": "compact", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "long discussion"}),
        )
        .await?;
    wait_turn(&mut client).await?;

    // No from/to: compact everything that is still in the context. No summary:
    // ask the provider.
    let compacted: eix_core::ContextLedgerResult = client
        .request(
            "context.compact",
            json!({"session_id": session_id, "generate": true}),
        )
        .await?;
    assert!(compacted
        .entries
        .iter()
        .any(|entry| entry.id.starts_with('c')));

    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "follow up"}),
        )
        .await?;
    wait_turn(&mut client).await?;

    let bodies = bodies.lock().await;
    // bodies[0] = main turn, bodies[1] = summarization, bodies[2] = follow up.
    let summary_request: Value = serde_json::from_str(&bodies[1])?;
    assert!(summary_request["messages"]
        .to_string()
        .contains("Summarize the conversation"));
    let follow_up: Value = serde_json::from_str(&bodies[2])?;
    let text = follow_up["messages"].to_string();
    assert!(
        text.contains("Hello from mock"),
        "summary is in context: {text}"
    );
    assert!(text.contains("follow up"));
    assert!(
        !text.contains("long discussion"),
        "old turn was compacted: {text}"
    );

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn session_cd_moves_the_workspace() -> anyhow::Result<()> {
    let dir = temp_dir("cd");
    std::fs::create_dir_all(dir.join("sub"))?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider(listener, Arc::new(Mutex::new(Vec::new()))));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "cd", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let updated: SessionResult = client
        .request(
            "session.cd",
            json!({"session_id": session_id, "path": "sub"}),
        )
        .await?;
    let expected = dir.join("sub").canonicalize()?;
    assert_eq!(updated.session.cwd, expected);

    let code = client
        .request_error(
            "session.cd",
            json!({"session_id": session_id, "path": "missing-dir"}),
        )
        .await?;
    assert_eq!(code, "invalid_params");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn models_agents_and_custom_commands_round_trip() -> anyhow::Result<()> {
    let dir = temp_dir("models-agents");
    std::fs::create_dir_all(dir.join(".eix/agents"))?;
    std::fs::create_dir_all(dir.join(".eix/commands"))?;
    std::fs::write(
        dir.join(".eix/agents/reviewer.md"),
        "---\ndescription: Review changes\nmode: all\npermission: read-only\n---\n\nFind bugs.\n",
    )?;
    std::fs::write(
        dir.join(".eix/commands/review.md"),
        "---\ndescription: Review a path\nagent: reviewer\n---\n\nReview $ARGUMENTS carefully.\n",
    )?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider(listener, Arc::new(Mutex::new(Vec::new()))));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.models = vec![ModelConfig {
        id: "mock/second".to_string(),
        name: Some("Second".to_string()),
        base_url: format!("http://{addr}/v1"),
        input_cost: Some(1.0),
        output_cost: Some(2.0),
        variants: vec!["high".to_string()],
        ..ModelConfig::default()
    }];
    config.providers.insert(
        "remote".to_string(),
        NamedProviderConfig {
            name: Some("Remote".to_string()),
            base_url: format!("http://{addr}/v1"),
            ..NamedProviderConfig::default()
        },
    );

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "catalog", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let models: eix_core::ModelListResult = client.request("model.list", Value::Null).await?;
    assert!(models.warnings.is_empty(), "{:?}", models.warnings);
    assert_eq!(models.models.len(), 6);
    assert_eq!(models.models[0].id, "mock-model");
    assert!(models.models[0].default);
    assert_eq!(models.models[1].id, "mock/second");
    assert_eq!(models.models[1].variants, vec!["high"]);
    // Both the default provider and the named provider expose the mock's
    // `/models` catalog, with provider-qualified selection ids.
    assert_eq!(models.models[2].id, "default/mock-auto");
    assert_eq!(models.models[3].id, "default/mock-other");
    assert_eq!(models.models[3].context_tokens, Some(12_345));
    assert_eq!(models.models[4].id, "remote/mock-auto");
    assert_eq!(models.models[4].provider, "Remote");
    assert_eq!(models.models[5].id, "remote/mock-other");

    let pinned: SessionResult = client
        .request(
            "session.model",
            json!({"session_id": session_id, "model": "mock/second#high"}),
        )
        .await?;
    assert_eq!(pinned.session.model.as_deref(), Some("mock/second#high"));
    let cleared: SessionResult = client
        .request(
            "session.model",
            json!({"session_id": session_id, "model": null}),
        )
        .await?;
    assert!(cleared.session.model.is_none());

    let agents: eix_core::AgentListResult = client
        .request("agent.list", json!({"session_id": session_id}))
        .await?;
    assert!(agents.agents.iter().any(|agent| agent.name == "build"));
    let reviewer = agents
        .agents
        .iter()
        .find(|agent| agent.name == "reviewer")
        .expect("reviewer agent");
    assert_eq!(reviewer.permission, Some(PermissionLevel::ReadOnly));
    assert_eq!(reviewer.prompt, "Find bugs.");

    let selected: SessionResult = client
        .request(
            "session.agent",
            json!({"session_id": session_id, "agent": "reviewer"}),
        )
        .await?;
    assert_eq!(selected.session.agent.as_deref(), Some("reviewer"));

    let code = client
        .request_error(
            "session.agent",
            json!({"session_id": session_id, "agent": "missing"}),
        )
        .await?;
    assert_eq!(code, "invalid_params");

    let commands = eix_core::load_commands(&dir);
    let review = commands
        .iter()
        .find(|command| command.name == "review")
        .expect("review command");
    assert_eq!(review.agent.as_deref(), Some("reviewer"));
    assert_eq!(
        eix_core::expand_command(&review.template, "src/lib.rs"),
        "Review src/lib.rs carefully."
    );

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn mcp_tools_are_listed_and_require_approval() -> anyhow::Result<()> {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        return Ok(()); // no Python: nothing to run the stdio server with
    }
    let dir = temp_dir("mcp");
    std::fs::create_dir_all(&dir)?;
    let script = dir.join("server.py");
    std::fs::write(
        &script,
        r#"import sys, json

def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method = msg.get("method")
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock", "version": "0"}}})
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"tools": [{
            "name": "echo",
            "description": "Echo text",
            "inputSchema": {"type": "object",
                            "properties": {"text": {"type": "string"}},
                            "required": ["text"]}}]}})
    elif method == "tools/call":
        text = msg["params"]["arguments"].get("text", "")
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {
            "content": [{"type": "text", "text": "echo:" + text}]}})
    elif method == "resources/list":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"resources": [{
            "uri": "file:///note", "name": "note",
            "description": "A note", "mimeType": "text/plain"}]}})
    elif method == "resources/read":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"contents": [{
            "uri": "file:///note", "mimeType": "text/plain",
            "text": "note body"}]}})
    elif method == "prompts/list":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {"prompts": [{
            "name": "review", "description": "Review a path",
            "arguments": [{"name": "path", "required": True}]}]}})
    elif method == "prompts/get":
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {
            "description": "Review prompt",
            "messages": [{"role": "user", "content": {
                "type": "text", "text": "review this"}}]}})
    else:
        send({"jsonrpc": "2.0", "id": msg.get("id"),
              "error": {"code": -32601, "message": "unknown method"}})
"#,
    )?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mcp_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    config.mcp.servers.insert(
        "mock".to_string(),
        eix_core::McpServerConfig {
            command: "python3".to_string(),
            args: vec![script.to_string_lossy().into_owned()],
            ..eix_core::McpServerConfig::default()
        },
    );

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;

    let created: SessionResult = client
        .request("session.create", json!({"name": "mcp", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let mcp: eix_core::McpListResult = client.request("mcp.list", Value::Null).await?;
    assert_eq!(mcp.servers.len(), 1);
    assert_eq!(mcp.servers[0].name, "mock");
    assert!(mcp.servers[0].error.is_none(), "{:?}", mcp.servers[0].error);
    assert_eq!(mcp.servers[0].tools, 1);
    assert_eq!(mcp.servers[0].resources, 1);
    assert_eq!(mcp.servers[0].prompts, 1);

    let _: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "echo something"}),
        )
        .await?;

    let approval = loop {
        let notification = client.next_notification().await?;
        if notification.method == "tool.approval_required" {
            break notification.params.clone();
        }
    };
    assert_eq!(approval["tool"], "mcp__mock__echo");
    assert!(approval["summary"]
        .as_str()
        .unwrap()
        .contains("mcp__mock__echo"));
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
    let tool_result = |name: &str| {
        attached
            .events
            .iter()
            .find(|event| event.kind == EventKind::ToolResult && event.payload["tool"] == name)
            .unwrap_or_else(|| panic!("missing tool result for {name}"))
    };
    let echo = tool_result("mcp__mock__echo");
    assert!(
        echo.payload["output"].as_str().unwrap().contains("echo:hi"),
        "{}",
        echo.payload["output"]
    );
    assert!(tool_result("mcp_list_resources").payload["output"]
        .as_str()
        .unwrap()
        .contains("file:///note"));
    assert!(tool_result("mcp_read_resource").payload["output"]
        .as_str()
        .unwrap()
        .contains("note body"));
    assert!(tool_result("mcp_list_prompts").payload["output"]
        .as_str()
        .unwrap()
        .contains("review"));
    assert!(tool_result("mcp_get_prompt").payload["output"]
        .as_str()
        .unwrap()
        .contains("review this"));

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn btw_answers_without_touching_the_log() -> anyhow::Result<()> {
    let dir = temp_dir("btw");
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
        .request("session.create", json!({"name": "btw", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "main question"}),
        )
        .await?;
    wait_turn(&mut client).await?;

    let before: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let events_before = before.events.len();

    let result: eix_core::BtwResult = client
        .request(
            "session.btw",
            json!({"session_id": session_id, "question": "quick side question"}),
        )
        .await?;
    assert!(!result.btw_id.is_empty());

    let completed = loop {
        let notification = client.next_notification().await?;
        if notification.method == "btw.completed" {
            break notification.params.clone();
        }
    };
    assert_eq!(completed["error"], Value::Null);
    assert_eq!(completed["text"], "Hello from mock");

    let after: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert_eq!(after.events.len(), events_before, "btw adds no events");

    let bodies = bodies.lock().await;
    let btw: Value = serde_json::from_str(&bodies[1])?;
    let text = btw["messages"].to_string();
    assert!(text.contains("quick side question"));
    assert!(
        !text.contains("main question"),
        "the side question is answered out of context: {text}"
    );

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
    // The provider never got a directly mutating tool in its advertised
    // list. `run_command` stays visible so harmless commands like `git
    // status` work; the permission engine rejects anything that writes.
    let first: Value = serde_json::from_str(&bodies.lock().await[0])?;
    assert!(
        first["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().all(|tool| {
                !matches!(
                    tool["function"]["name"].as_str(),
                    Some("write_file" | "edit" | "patch" | "start_command")
                )
            })),
        "read-only tools were: {}",
        first["tools"]
    );
    assert!(
        first["tools"].as_array().is_some_and(|tools| tools
            .iter()
            .any(|tool| tool["function"]["name"] == "run_command")),
        "run_command should remain available for read-only inspection"
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
async fn read_only_commands_run_without_approval_and_secrets_stay_local() -> anyhow::Result<()> {
    let dir = temp_dir("readonly-redact");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(
        dir.join("secret.txt"),
        "PASSWORD=hunter2\nOPENAI_API_KEY=sk-live-e2e-secret-1234567890\n",
    )?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(secret_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    config.tools.sandbox = eix_core::SandboxMode::Off;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "redact", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "inspect the secret file"}),
        )
        .await?;

    let mut prompted = false;
    let completed = loop {
        let notification = client.next_notification().await?;
        match notification.method.as_str() {
            "tool.approval_required" => prompted = true,
            "turn.completed"
                if notification.params["session_id"] == session_id.as_str()
                    && notification.params["turn_id"] == submitted.turn_id.as_str() =>
            {
                break notification.params.clone();
            }
            _ => {}
        }
    };
    assert!(!prompted, "`cat` must run without an approval prompt");
    assert!(completed["error"].is_null(), "{completed}");

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let assistant = attached
        .events
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::AssistantMessage)
        .expect("assistant message");
    // The model only saw a placeholder and echoed it; the daemon restored the
    // original before persisting/displaying the assistant message.
    assert_eq!(assistant.payload["text"], "captured hunter2");
    assert!(attached.events.iter().any(|event| {
        event.kind == EventKind::ToolResult
            && event.payload["tool"] == "run_command"
            && event.payload["ok"] == true
    }));
    assert!(
        !attached.events.iter().any(|event| {
            event.kind == EventKind::Snapshot && event.payload["tool"] == "run_command"
        }),
        "read-only commands must not capture a workspace snapshot"
    );
    let write_call = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::ToolCall && event.payload["tool"] == "write_file")
        .expect("write_file call");
    assert_eq!(write_call.payload["arguments"]["content"], "hunter2");
    assert_eq!(write_call.payload["arguments"]["path"], "out.txt");

    let bodies = bodies.lock().await;
    assert!(bodies.len() >= 3, "expected three provider requests");
    for (index, body) in bodies.iter().enumerate() {
        assert!(
            !body.contains("hunter2"),
            "request {index} leaked the password: {body}"
        );
        assert!(
            !body.contains("sk-live-e2e-secret-1234567890"),
            "request {index} leaked the API key: {body}"
        );
    }
    assert!(
        bodies[1].contains("{{eix_secret_"),
        "the tool result should contain a placeholder: {}",
        bodies[1]
    );
    drop(bodies);

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn permission_rule_skips_the_prompt() -> anyhow::Result<()> {
    let dir = temp_dir("allow-rule");
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
    config.tools.sandbox = eix_core::SandboxMode::Off;
    config.tools.rules = vec![PermissionRule {
        tool: "run_command".to_string(),
        command: Some(vec!["sed".to_string()]),
        action: PermissionAction::Allow,
        ..Default::default()
    }];

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "auto", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let _: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "run it"}),
        )
        .await?;

    let mut prompted = false;
    let completed = loop {
        let notification = client.next_notification().await?;
        if notification.method == "tool.approval_required" {
            prompted = true;
        }
        if notification.method == "turn.completed" {
            break notification.params.clone();
        }
    };
    assert!(!prompted, "an auto-approved command must not prompt");
    assert!(completed["error"].is_null());

    // The command really ran, and it was still snapshotted first.
    assert_eq!(std::fs::read_to_string(dir.join("src/keep.txt"))?, "after");
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let snapshot = attached
        .events
        .iter()
        .find(|e| e.kind == EventKind::Snapshot)
        .expect("auto-approved runs are snapshotted");
    assert_eq!(snapshot.payload["tool"], "run_command");
    let result = attached
        .events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::ToolResult)
        .expect("tool result event");
    assert_eq!(result.payload["denied"], false);
    assert_eq!(result.payload["ok"], true);

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn permission_rule_cannot_bypass_a_read_only_session() -> anyhow::Result<()> {
    let dir = temp_dir("allow-rule-readonly");
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
    config.tools.sandbox = eix_core::SandboxMode::Off;
    // A catch-all rule must still lose to the session's read-only level.
    config.tools.rules = vec![PermissionRule {
        tool: "*".to_string(),
        action: PermissionAction::Allow,
        ..Default::default()
    }];

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "ro", "cwd": dir, "permission": "read-only"}),
        )
        .await?;
    let session_id = created.session.id.clone();

    let _: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "run it"}),
        )
        .await?;

    let mut prompted = false;
    loop {
        let notification = client.next_notification().await?;
        if notification.method == "tool.approval_required" {
            prompted = true;
        }
        if notification.method == "turn.completed" {
            break;
        }
    }
    assert!(!prompted, "read-only must refuse, not prompt");
    assert_eq!(std::fs::read_to_string(dir.join("src/keep.txt"))?, "before");

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let result = attached
        .events
        .iter()
        .rev()
        .find(|e| e.kind == EventKind::ToolResult)
        .expect("tool result event");
    assert_eq!(result.payload["denied"], true);

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
        results[1].payload["output"], "1: staged\n",
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
    config.tools.approval = eix_core::ApprovalMode::Auto;
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
    config.tools.approval = eix_core::ApprovalMode::Auto;

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
async fn cancelling_the_parent_stops_a_running_sub_agent() -> anyhow::Result<()> {
    let dir = temp_dir("agent-cancel");
    std::fs::create_dir_all(&dir)?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(hanging_agent_provider(listener));

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
    let parent = created.session.id.clone();
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": parent, "text": "investigate"}),
        )
        .await?;

    // Wait until the child exists and its request is hanging.
    let child = loop {
        let notification = client.next_notification().await?;
        if notification.method == "agent.spawned" {
            break notification.params["agent_id"]
                .as_str()
                .expect("agent id")
                .to_string();
        }
    };
    let cancelled: eix_core::CancelResult = client
        .request("prompt.cancel", json!({"session_id": parent}))
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
        .request("session.attach", json!({"session_id": child}))
        .await?;
    assert_eq!(attached.session.status, eix_core::SessionStatus::Idle);
    let assistant = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::AssistantMessage)
        .unwrap_or_else(|| panic!("child cancellation event: {:?}", attached.events));
    assert_eq!(assistant.payload["cancelled"], true);

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn ask_user_blocks_the_turn_until_the_user_answers() -> anyhow::Result<()> {
    let dir = temp_dir("ask");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(ask_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "ask", "cwd": dir}))
        .await?;
    let session_id = created.session.id;
    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "pick a database"}),
        )
        .await?;

    let mut saw_question = false;
    let mut text = String::new();
    loop {
        let notification = client.next_notification().await?;
        match notification.method.as_str() {
            "input.required" if notification.params["session_id"] == session_id.as_str() => {
                assert_eq!(
                    notification.params["question"],
                    "Which database should we use?"
                );
                let options = notification.params["options"].as_array().expect("options");
                assert_eq!(options.len(), 3);
                assert_eq!(options[0]["label"], "SQLite");
                let result: eix_core::InputResult = client
                    .request(
                        "input.respond",
                        json!({
                            "session_id": session_id,
                            "input_id": notification.params["input_id"],
                            "option_index": 1,
                        }),
                    )
                    .await?;
                assert!(result.accepted);
                saw_question = true;
            }
            "message.delta" if notification.params["session_id"] == session_id.as_str() => {
                if let Some(delta) = notification.params["delta"].as_str() {
                    text.push_str(delta);
                }
            }
            "turn.completed" if notification.params["session_id"] == session_id.as_str() => {
                assert!(notification.params["error"].is_null());
                break;
            }
            _ => {}
        }
    }
    assert!(saw_question, "the daemon never asked");
    assert_eq!(text, "done");

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert_eq!(attached.session.status, eix_core::SessionStatus::Idle);
    let asked = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::InputRequired)
        .expect("question event");
    assert_eq!(asked.payload["question"], "Which database should we use?");
    assert_eq!(asked.payload["options"][2]["label"], "MySQL");
    let answered = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::InputAnswered)
        .expect("answer event");
    assert_eq!(answered.payload["option_index"], 1);
    assert_eq!(
        answered.payload["answer"],
        "Postgres — Server, better concurrency"
    );
    let result = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::ToolResult && event.payload["tool"] == "ask_user")
        .expect("ask_user result");
    assert_eq!(result.payload["ok"], true);
    let output = result.payload["output"].as_str().expect("output");
    assert!(output.contains("User selected option 2 of 3"), "{output}");
    assert!(output.contains("Postgres"), "{output}");

    // The selected label reached the provider as the tool result.
    let bodies = bodies.lock().await;
    assert_eq!(bodies.len(), 2);
    let second: Value = serde_json::from_str(&bodies[1])?;
    let tool_message = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("tool message");
    assert!(tool_message["content"]
        .as_str()
        .unwrap()
        .contains("Postgres"));

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

#[tokio::test]
async fn picker_query_lists_workspace_files() -> anyhow::Result<()> {
    let dir = temp_dir("picker-files");
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(workspace.join("src"))?;
    std::fs::create_dir_all(workspace.join(".git"))?;
    std::fs::create_dir_all(workspace.join("target/debug"))?;
    std::fs::write(workspace.join("src/main.rs"), "fn main() {}")?;
    std::fs::write(workspace.join("README.md"), "# eix")?;
    std::fs::write(workspace.join(".git/config"), "secret")?;
    std::fs::write(workspace.join("target/debug/eix"), "binary")?;

    let config = Config::default();
    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "picker", "cwd": workspace}),
        )
        .await?;
    let session_id = created.session.id.clone();

    let result: eix_core::PickerResult = client
        .request(
            "picker.query",
            json!({"kind": "files", "session_id": session_id}),
        )
        .await?;
    assert_eq!(result.kind, eix_core::KIND_FILES);
    let values: Vec<&str> = result
        .items
        .iter()
        .map(|item| item.value.as_str())
        .collect();
    assert_eq!(values, vec!["README.md", "src/main.rs"]);

    let filtered: eix_core::PickerResult = client
        .request(
            "picker.query",
            json!({"kind": "files", "session_id": session_id, "query": "main"}),
        )
        .await?;
    assert_eq!(filtered.items.len(), 1);
    assert_eq!(filtered.items[0].value, "src/main.rs");

    // The session must belong to the daemon, and the kind must be known.
    assert_eq!(
        client
            .request_error(
                "picker.query",
                json!({"kind": "files", "session_id": "missing"}),
            )
            .await?,
        "not_found"
    );
    assert_eq!(
        client
            .request_error(
                "picker.query",
                json!({"kind": "nope", "session_id": session_id})
            )
            .await?,
        "invalid_params"
    );

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn picker_query_lists_git_worktrees_and_branches() -> anyhow::Result<()> {
    let dir = temp_dir("picker-git");
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(&workspace)?;
    let init = git(&workspace, &["init", "-q"]);
    assert!(
        init.status.success(),
        "git init: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    let commit = git(&workspace, &["commit", "-q", "--allow-empty", "-m", "init"]);
    assert!(
        commit.status.success(),
        "git commit: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
    assert!(git(&workspace, &["branch", "feature"]).status.success());
    let linked = workspace.join("linked");
    let add = git(
        &workspace,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "eix/linked",
            linked.to_str().unwrap(),
        ],
    );
    assert!(
        add.status.success(),
        "git worktree add: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let config = Config::default();
    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "git", "cwd": workspace}))
        .await?;
    let session_id = created.session.id.clone();

    let trees: eix_core::PickerResult = client
        .request(
            "picker.query",
            json!({"kind": "worktrees", "session_id": session_id}),
        )
        .await?;
    assert_eq!(trees.kind, eix_core::KIND_WORKTREES);
    assert!(
        trees
            .items
            .iter()
            .any(|item| item.detail.as_deref() == Some("eix/linked")),
        "linked worktree missing: {trees:?}"
    );
    assert!(trees.items.iter().all(|item| item.preview.is_some()));

    let branches: eix_core::PickerResult = client
        .request(
            "picker.query",
            json!({"kind": "branches", "session_id": session_id, "query": "feat"}),
        )
        .await?;
    assert_eq!(branches.kind, eix_core::KIND_BRANCHES);
    assert_eq!(branches.items.len(), 1);
    assert_eq!(branches.items[0].value, "feature");

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn picker_query_lists_context_entries() -> anyhow::Result<()> {
    let dir = temp_dir("picker-context");
    std::fs::create_dir_all(&dir)?;
    let config = Config::default();
    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "context", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let result: eix_core::PickerResult = client
        .request(
            "picker.query",
            json!({"kind": "context", "session_id": session_id}),
        )
        .await?;
    assert_eq!(result.kind, eix_core::KIND_CONTEXT);
    assert!(
        result.items.iter().any(|item| item.value == "system"),
        "system entry missing: {result:?}"
    );
    assert!(
        result.items.iter().any(|item| item.value == "workspace"),
        "workspace entry missing: {result:?}"
    );
    // Rows carry the ledger detail so the picker can preview them.
    assert!(result.items.iter().all(|item| item.preview.is_some()));

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
            if headers.starts_with("get /v1/models") || headers.starts_with("get /models") {
                let body =
                    r#"{"data":[{"id":"mock-auto"},{"id":"mock-other","context_length":12345}]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
                return;
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

async fn flaky_provider(listener: TcpListener, failures: usize, bodies: Arc<Mutex<Vec<String>>>) {
    let remaining = Arc::new(Mutex::new(failures));
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let bodies = Arc::clone(&bodies);
        let remaining = Arc::clone(&remaining);
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            bodies.lock().await.push(body);
            let fail = {
                let mut remaining = remaining.lock().await;
                if *remaining > 0 {
                    *remaining -= 1;
                    true
                } else {
                    false
                }
            };
            if fail {
                write_http_error(
                    &mut socket,
                    "503 Service Unavailable",
                    r#"{"error":{"message":"temporarily overloaded"}}"#,
                )
                .await;
                return;
            }
            let events = concat!(
                "data: {\"model\":\"mock-retry\",\"choices\":[{\"delta\":{\"content\":\"recovered\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            write_sse(&mut socket, events).await;
        });
    }
}

async fn rate_limit_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
    let first = Arc::new(Mutex::new(true));
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let bodies = Arc::clone(&bodies);
        let first = Arc::clone(&first);
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            bodies.lock().await.push(body);
            let first = {
                let mut first = first.lock().await;
                let value = *first;
                *first = false;
                value
            };
            if first {
                let body = r#"{"error":{"message":"rate limit exceeded"}}"#;
                let response = format!(
                    "HTTP/1.1 429 Too Many Requests\r\nContent-Type: application/json\r\nRetry-After: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
                return;
            }
            let events = concat!(
                "data: {\"model\":\"mock-retry\",\"choices\":[{\"delta\":{\"content\":\"recovered\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            write_sse(&mut socket, events).await;
        });
    }
}

async fn partial_failure_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
    let first = Arc::new(Mutex::new(true));
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let bodies = Arc::clone(&bodies);
        let first = Arc::clone(&first);
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            bodies.lock().await.push(body);
            let first = {
                let mut first = first.lock().await;
                let value = *first;
                *first = false;
                value
            };
            if first {
                // Advertise more bytes than we send, then close: the partial
                // delta reaches the client before the transport fails.
                let events = "data: {\"choices\":[{\"delta\":{\"content\":\"partial\"}}]}\n\n";
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{events}",
                    events.len() + 1024
                );
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.flush().await;
                return;
            }
            let events = concat!(
                "data: {\"model\":\"mock-retry\",\"choices\":[{\"delta\":{\"content\":\"recovered\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            write_sse(&mut socket, events).await;
        });
    }
}

async fn quota_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let bodies = Arc::clone(&bodies);
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            bodies.lock().await.push(body);
            write_http_error(
                &mut socket,
                "429 Too Many Requests",
                r#"{"error":{"type":"insufficient_quota","message":"You exceeded your current quota"}}"#,
            )
            .await;
        });
    }
}

async fn write_http_error(socket: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = socket.write_all(response.as_bytes()).await;
    let _ = socket.flush().await;
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

#[tokio::test]
async fn skills_are_listed_in_the_prompt_and_loadable_with_use_skill() -> anyhow::Result<()> {
    let dir = temp_dir("skills");
    std::fs::create_dir_all(dir.join(".eix/skills/deploy"))?;
    std::fs::write(
        dir.join(".eix/skills/deploy/SKILL.md"),
        "---\ndescription: deploy checklist\n---\nRun the deploy checklist before shipping.\n",
    )?;
    std::fs::create_dir_all(dir.join(".claude/skills/audit"))?;
    std::fs::write(
        dir.join(".claude/skills/audit/SKILL.md"),
        "---\ndescription: Claude-compatible audit\nallowed-tools: read_file\n---\nAudit the workspace.\n",
    )?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(skill_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "skills", "cwd": dir}))
        .await?;
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": created.session.id, "text": "deploy please"}),
        )
        .await?;
    let mut text = String::new();
    loop {
        let notification = client.next_notification().await?;
        match notification.method.as_str() {
            "message.delta" => {
                text.push_str(notification.params["delta"].as_str().unwrap_or_default())
            }
            "turn.completed"
                if notification.params["session_id"] == created.session.id.as_str()
                    && notification.params["turn_id"] == submitted.turn_id.as_str() =>
            {
                break;
            }
            _ => {}
        }
    }
    assert_eq!(text, "used skill");
    let bodies = bodies.lock().await;
    assert!(bodies.len() >= 2);
    assert!(bodies[0].contains("<available_skills>"));
    assert!(bodies[0].contains("deploy checklist"));
    assert!(bodies[0].contains("Claude-compatible audit"));
    assert!(bodies[0].contains("<allowed_tools>read_file</allowed_tools>"));
    assert!(bodies[1].contains("Run the deploy checklist before shipping."));

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn skills_expand_arguments_and_enforce_allowed_tools() -> anyhow::Result<()> {
    let dir = temp_dir("skill-guard");
    std::fs::create_dir_all(dir.join(".eix/skills/locked"))?;
    std::fs::write(
        dir.join(".eix/skills/locked/SKILL.md"),
        "---\ndescription: locked workflow\nallowed-tools: read_file\n---\nDo $ARGUMENTS ($1 / $2).\n",
    )?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(skill_guard_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "guard", "cwd": dir}))
        .await?;
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": created.session.id, "text": "run the locked workflow"}),
        )
        .await?;
    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["session_id"] == created.session.id.as_str()
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            break;
        }
    }

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": created.session.id}))
        .await?;
    let results: Vec<_> = attached
        .events
        .iter()
        .filter(|event| event.kind == EventKind::ToolResult)
        .collect();
    let skill = results
        .iter()
        .find(|event| event.payload["tool"] == "use_skill")
        .expect("use_skill result");
    let output = skill.payload["output"].as_str().unwrap_or_default();
    assert!(output.contains("Do alpha beta (alpha / beta)."), "{output}");
    assert!(output.contains("Allowed tools"), "{output}");
    let write = results
        .iter()
        .find(|event| event.payload["tool"] == "write_file")
        .expect("blocked write result");
    assert_eq!(write.payload["ok"], false);
    assert_eq!(write.payload["denied"], true);
    assert!(
        write.payload["output"]
            .as_str()
            .unwrap_or_default()
            .contains("does not allow"),
        "{}",
        write.payload["output"]
    );
    assert!(!dir.join("x.txt").exists(), "the write must not run");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

async fn skill_guard_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
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
            let tool_count = value["messages"]
                .as_array()
                .map(|messages| {
                    messages
                        .iter()
                        .filter(|message| message["role"] == "tool")
                        .count()
                })
                .unwrap_or(0);
            let (name, arguments, text) = match tool_count {
                0 => (
                    "use_skill",
                    json!({"name": "locked", "arguments": "alpha beta"}).to_string(),
                    String::new(),
                ),
                1 => (
                    "write_file",
                    json!({"path": "x.txt", "content": "blocked"}).to_string(),
                    String::new(),
                ),
                _ => ("", String::new(), "done".to_string()),
            };
            if tool_count >= 2 {
                let chunk = json!({
                    "model": "mock-skill",
                    "choices": [{"delta": {"content": text}, "finish_reason": "stop"}],
                });
                write_sse(&mut socket, &format!("data: {chunk}\n\ndata: [DONE]\n\n")).await;
                return;
            }
            let chunk = json!({
                "model": "mock-skill",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": format!("call_{tool_count}"),
                            "type": "function",
                            "function": {"name": name, "arguments": arguments},
                        }]
                    },
                    "finish_reason": "tool_calls",
                }],
            });
            write_sse(&mut socket, &format!("data: {chunk}\n\ndata: [DONE]\n\n")).await;
        });
    }
}

async fn skill_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
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
            let chunk = if has_tool_result {
                json!({
                    "model": "mock-skill",
                    "choices": [{"delta": {"content": "used skill"}, "finish_reason": "stop"}],
                })
            } else {
                json!({
                    "model": "mock-skill",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_skill",
                                "type": "function",
                                "function": {"name": "use_skill", "arguments": "{\"name\":\"deploy\"}"},
                            }]
                        },
                        "finish_reason": "tool_calls",
                    }],
                })
            };
            let events = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            write_sse(&mut socket, &events).await;
        });
    }
}

#[tokio::test]
async fn todo_tool_updates_the_session_task_list() -> anyhow::Result<()> {
    let dir = temp_dir("todos");
    std::fs::create_dir_all(&dir)?;
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(todo_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "todos", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "plan the work"}),
        )
        .await?;
    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["session_id"] == session_id.as_str()
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            break;
        }
    }

    let todos: eix_core::TodoList = client
        .request("session.todos", json!({"session_id": session_id}))
        .await?;
    assert_eq!(todos.items.len(), 2);
    assert_eq!(todos.items[0].content, "inspect the code");
    assert_eq!(todos.items[0].status, "completed");
    assert_eq!(todos.items[1].status, "in_progress");
    let bodies = bodies.lock().await;
    assert!(bodies[0].contains("\"name\":\"todo\""));
    assert!(bodies[1].contains("[>] write the fix"));

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

async fn todo_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
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
            let chunk = if has_tool_result {
                json!({
                    "model": "mock-todo",
                    "choices": [{"delta": {"content": "planned"}, "finish_reason": "stop"}],
                })
            } else {
                let arguments = json!({
                    "todos": [
                        {"content": "inspect the code", "status": "completed"},
                        {"content": "write the fix", "status": "in_progress"}
                    ]
                })
                .to_string();
                json!({
                    "model": "mock-todo",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_todo",
                                "type": "function",
                                "function": {"name": "todo", "arguments": arguments},
                            }]
                        },
                        "finish_reason": "tool_calls",
                    }],
                })
            };
            let events = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            write_sse(&mut socket, &events).await;
        });
    }
}

#[tokio::test]
async fn auto_compaction_summarizes_old_turns_before_overflow() -> anyhow::Result<()> {
    let dir = temp_dir("auto-compact");
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
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;
    config.context.budget_tokens = 100_000;
    // Force the path without depending on how many tokens the prompt has.
    config.context.auto_compact_threshold = 0.0001;
    config.context.auto_compact_keep_turns = 2;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "compact", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    for text in ["one", "two", "three"] {
        let submitted: SubmitResult = client
            .request(
                "prompt.submit",
                json!({"session_id": session_id, "text": text}),
            )
            .await?;
        loop {
            let notification = client.next_notification().await?;
            if notification.method == "turn.completed"
                && notification.params["turn_id"] == submitted.turn_id.as_str()
            {
                assert!(notification.params["error"].is_null());
                break;
            }
        }
    }

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let compacted = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::ContextCompacted)
        .expect("auto compaction event");
    assert_eq!(compacted.payload["messages"], 2);
    assert_eq!(compacted.payload["summary"], "Hello from mock");

    let bodies = bodies.lock().await;
    assert_eq!(bodies.len(), 4, "three turns plus one summary");
    let summary: Value = serde_json::from_str(&bodies[2])?;
    assert!(summary["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| message["content"]
            .as_str()
            .unwrap_or_default()
            .contains("Summarize the conversation")));
    let third: Value = serde_json::from_str(&bodies[3])?;
    assert!(third["messages"].as_array().unwrap().iter().any(|message| {
        message["role"] == "system"
            && message["content"]
                .as_str()
                .unwrap_or_default()
                .contains("[compacted 2 messages")
    }));

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

/// Like [`tool_provider`], but keys off the *last* message so a second user
/// turn in the same session calls the tool again.
async fn repeat_tool_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
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
            let last_is_tool = value["messages"]
                .as_array()
                .and_then(|messages| messages.last())
                .map(|message| message["role"] == "tool")
                .unwrap_or(false);
            let events = if last_is_tool {
                let chunk = json!({
                    "model": "mock-tools",
                    "choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}],
                });
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            } else {
                let arguments = json!({"path": "out.txt", "content": "hello"}).to_string();
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

#[tokio::test]
async fn config_reload_switches_the_default_model() -> anyhow::Result<()> {
    let dir = temp_dir("reload");
    std::fs::create_dir_all(&dir)?;
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        "[provider]\nbase_url = \"http://127.0.0.1:9/v1\"\nmodel = \"first-model\"\n\
         api_key = \"test\"\ntimeout_secs = 5\n",
    )?;

    let mut config = Config::default();
    config.provider.base_url = "http://127.0.0.1:9/v1".into();
    config.provider.model = "first-model".into();
    config.provider.api_key = Some("test".into());
    // Mark the file as the authoritative source so `reload` reads it again.
    config.explicit_path = Some(config_path.clone());
    config.load_cwd = dir.clone();
    config.watch_paths = vec![config_path.clone()];

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "reload", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    let before: eix_core::ModelListResult = client.request("model.list", Value::Null).await?;
    assert_eq!(before.models[0].id, "first-model");

    std::fs::write(
        &config_path,
        "[provider]\nbase_url = \"http://127.0.0.1:9/v1\"\nmodel = \"second-model\"\n\
         api_key = \"test\"\ntimeout_secs = 5\n",
    )?;
    let reloaded: Value = client.request("config.reload", Value::Null).await?;
    assert_eq!(reloaded["reloaded"], true);
    let after: eix_core::ModelListResult = client.request("model.list", Value::Null).await?;
    assert_eq!(after.models[0].id, "second-model");
    // The session still exists; only new turns use the new default.
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert_eq!(attached.session.id, session_id);

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn sessions_can_be_exported_imported_archived_and_deleted() -> anyhow::Result<()> {
    let dir = temp_dir("session-management");
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
        .request("session.create", json!({"name": "original", "cwd": dir}))
        .await?;
    let original_id = created.session.id.clone();
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": original_id, "text": "say hi"}),
        )
        .await?;
    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["session_id"] == original_id.as_str()
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            break;
        }
    }

    let timeline: eix_core::TimelineResult = client
        .request("session.timeline", json!({"session_id": original_id}))
        .await?;
    assert!(timeline
        .items
        .iter()
        .any(|item| item.kind == "user_message"));
    assert!(timeline
        .items
        .iter()
        .any(|item| item.kind == "assistant_message" && item.preview.contains("Hello")));

    let exported: eix_core::SessionExportResult = client
        .request("session.export", json!({"session_id": original_id}))
        .await?;
    let document: eix_core::SessionExport = serde_json::from_str(&exported.json)?;
    assert_eq!(document.version, eix_core::SESSION_EXPORT_VERSION);
    assert!(!document.events.is_empty());

    let imported: SessionResult = client
        .request(
            "session.import",
            json!({"json": exported.json, "name": "imported", "cwd": dir}),
        )
        .await?;
    assert_ne!(imported.session.id, original_id);
    assert_eq!(imported.session.name, "imported");
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": imported.session.id}))
        .await?;
    assert_eq!(attached.events.len(), document.events.len());
    assert!(attached
        .events
        .iter()
        .any(|event| event.kind == EventKind::AssistantMessage
            && event.payload["text"] == "Hello from mock"));

    let archived: SessionResult = client
        .request(
            "session.archive",
            json!({"session_id": original_id, "archived": true}),
        )
        .await?;
    assert!(archived.session.archived);
    let active: eix_core::SessionList = client.request("session.list", Value::Null).await?;
    assert!(!active.sessions.iter().any(|s| s.id == original_id));
    let all: eix_core::SessionList = client
        .request("session.list", json!({"include_archived": true}))
        .await?;
    assert!(all.sessions.iter().any(|s| s.id == original_id));

    let deleted: Value = client
        .request("session.delete", json!({"session_id": imported.session.id}))
        .await?;
    assert_eq!(deleted["deleted"], true);
    assert!(client
        .request::<AttachResult>("session.attach", json!({"session_id": imported.session.id}),)
        .await
        .is_err());
    let remaining: eix_core::SessionList = client
        .request("session.list", json!({"include_archived": true}))
        .await?;
    assert!(remaining
        .sessions
        .iter()
        .all(|s| s.id != imported.session.id));

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn image_attachments_are_stored_and_sent_as_content_parts() -> anyhow::Result<()> {
    let dir = temp_dir("images");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("pixel.png"), b"\x89PNG\r\n\x1a\nnot-a-real-png")?;
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;
    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "images", "cwd": dir}))
        .await?;
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({
                "session_id": created.session.id,
                "text": "what is this?",
                "images": ["pixel.png"],
            }),
        )
        .await?;
    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["session_id"] == created.session.id.as_str()
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            break;
        }
    }

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": created.session.id}))
        .await?;
    let user = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::UserMessage)
        .expect("user event");
    assert_eq!(user.payload["images"][0]["path"], "pixel.png");
    assert_eq!(user.payload["images"][0]["mime"], "image/png");
    let blob = user.payload["images"][0]["blob"]
        .as_str()
        .expect("event references a blob");
    assert_eq!(blob.len(), 64);
    assert!(user.payload["images"][0]["data"]
        .as_str()
        .unwrap_or_default()
        .is_empty());
    assert!(dir.join("data/blobs").join(&blob[..2]).join(blob).is_file());

    // Exports hydrate blob bytes so another machine can import the session.
    let exported: eix_core::SessionExportResult = client
        .request("session.export", json!({"session_id": created.session.id}))
        .await?;
    let document: eix_core::SessionExport = serde_json::from_str(&exported.json)?;
    let exported_image = document
        .events
        .iter()
        .find(|event| event.kind == EventKind::UserMessage)
        .and_then(|event| event.payload["images"][0].as_object())
        .expect("exported image");
    assert!(!exported_image["data"]
        .as_str()
        .unwrap_or_default()
        .is_empty());

    // Import must not depend on the exporting machine's blob directory. Remove
    // the local blob first, then import the hydrated export and check that the
    // bytes were written back under the same content hash.
    let blob_path = dir.join("data/blobs").join(&blob[..2]).join(blob);
    std::fs::remove_file(&blob_path)?;
    let imported: SessionResult = client
        .request(
            "session.import",
            json!({"json": exported.json, "name": "imported-images"}),
        )
        .await?;
    assert_ne!(imported.session.id, created.session.id);
    assert!(blob_path.is_file(), "import did not restore the image blob");
    let imported_attach: AttachResult = client
        .request("session.attach", json!({"session_id": imported.session.id}))
        .await?;
    let imported_user = imported_attach
        .events
        .iter()
        .find(|event| event.kind == EventKind::UserMessage)
        .expect("imported user event");
    assert_eq!(imported_user.payload["images"][0]["blob"], blob);
    assert!(imported_user.payload["images"][0]["data"]
        .as_str()
        .unwrap_or_default()
        .is_empty());

    let bodies = bodies.lock().await;
    let body: Value = serde_json::from_str(&bodies[0])?;
    let content = &body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap()["content"];
    assert!(content.is_array());
    assert_eq!(content[0]["type"], "text");
    assert_eq!(content[1]["type"], "image_url");
    assert!(content[1]["image_url"]["url"]
        .as_str()
        .unwrap()
        .starts_with("data:image/png;base64,"));

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn http_transport_serves_health_rpc_and_server_sent_events() -> anyhow::Result<()> {
    let dir = temp_dir("http");
    std::fs::create_dir_all(&dir)?;
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.local_addr()?.port()
    };
    let mut config = Config::default();
    config.server.http = Some(format!("127.0.0.1:{port}"));
    config.server.token = Some("http-secret".to_string());
    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let _client = connect(&socket).await?;

    let base = format!("http://127.0.0.1:{port}");
    let http = reqwest::Client::new();
    let mut health = None;
    for _ in 0..100 {
        if let Ok(response) = http.get(format!("{base}/health")).send().await {
            if response.status().is_success() {
                health = Some(response.json::<Value>().await?);
                break;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let health = health.expect("http server did not start");
    assert_eq!(health["ok"], true);

    let preflight = http
        .request(reqwest::Method::OPTIONS, format!("{base}/rpc"))
        .send()
        .await?;
    assert_eq!(preflight.status(), 204);
    assert_eq!(
        preflight
            .headers()
            .get("access-control-allow-origin")
            .and_then(|value| value.to_str().ok()),
        Some("*")
    );

    let stats = http
        .get(format!("{base}/stats"))
        .bearer_auth("http-secret")
        .send()
        .await?;
    assert_eq!(stats.status(), 200);
    let stats: Value = stats.json().await?;
    assert!(stats["db_bytes"].as_u64().unwrap_or_default() > 0);

    let sdk = eix_sdk::Client::new(&base, "http-secret");
    assert_eq!(sdk.health().await?.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(sdk.stats().await?.version, env!("CARGO_PKG_VERSION"));

    let unauthorized = http
        .post(format!("{base}/rpc"))
        .json(&json!({"id": 1, "method": "hello", "params": null}))
        .send()
        .await?;
    assert_eq!(unauthorized.status(), 401);

    let response = http
        .post(format!("{base}/rpc"))
        .bearer_auth("http-secret")
        .json(&json!({"id": 2, "method": "hello", "params": null}))
        .send()
        .await?;
    assert_eq!(response.status(), 200);
    let response: Value = response.json().await?;
    assert_eq!(response["id"], 2);
    assert_eq!(response["result"]["protocol"], PROTOCOL_VERSION);

    let mut events = http
        .get(format!("{base}/events"))
        .bearer_auth("http-secret")
        .send()
        .await?
        .bytes_stream();
    use futures_util::StreamExt;
    // Headers are flushed before the handler subscribes to the daemon bus.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let created = http
        .post(format!("{base}/rpc"))
        .bearer_auth("http-secret")
        .json(&json!({
            "id": 3,
            "method": "session.create",
            "params": {"name": "http", "cwd": dir},
        }))
        .send()
        .await?;
    assert_eq!(created.status(), 200);
    let created: Value = created.json().await?;
    let target = created["result"]["session"]["id"]
        .as_str()
        .expect("created session id")
        .to_string();
    let mut seen = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
    while tokio::time::Instant::now() < deadline {
        let next = tokio::time::timeout(std::time::Duration::from_millis(500), events.next()).await;
        let Ok(Some(Ok(chunk))) = next else { continue };
        seen.push_str(&String::from_utf8_lossy(&chunk));
        if seen.contains("session.updated") {
            break;
        }
    }
    assert!(seen.contains("session.updated"), "no SSE event: {seen}");

    // The SDK uses the same HTTP surface. Exercise typed create/stats plus a
    // session-filtered event stream: another session's updates must not leak.
    let mut filtered = sdk.events(&[target.as_str()]).await?;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let other = sdk
        .create_session(&eix_core::CreateSessionParams {
            name: Some("other".into()),
            cwd: Some(dir.clone()),
            ..Default::default()
        })
        .await?;
    sdk.request(
        "session.rename",
        json!({"session_id": target.clone(), "name": "renamed"}),
    )
    .await?;

    let mut saw_other = false;
    let mut saw_target = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline && !saw_target {
        let next = tokio::time::timeout(Duration::from_millis(500), filtered.next()).await;
        let Ok(Some(Ok(notification))) = next else {
            continue;
        };
        if notification.params.to_string().contains(&other.session.id) {
            saw_other = true;
        }
        if notification.method == "session.updated"
            && notification.params["id"].as_str() == Some(target.as_str())
        {
            saw_target = true;
        }
    }
    assert!(
        saw_target,
        "filtered stream did not deliver the target session"
    );
    assert!(!saw_other, "filtered stream leaked another session");
    assert!(sdk
        .sessions(true)
        .await?
        .sessions
        .iter()
        .any(|session| session.id == target));
    assert!(sdk.stats().await?.sessions >= 2);

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn user_shell_commands_are_recorded_and_read_only_sessions_only_allow_safe_commands(
) -> anyhow::Result<()> {
    let dir = temp_dir("shell-run");
    std::fs::create_dir_all(&dir)?;
    let mut config = Config::default();
    config.tools.snapshots = false;
    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;

    let created: SessionResult = client
        .request("session.create", json!({"name": "shell", "cwd": dir}))
        .await?;
    let result: eix_core::ShellRunResult = client
        .request(
            "shell.run",
            json!({"session_id": created.session.id, "command": "printf shell-ok"}),
        )
        .await?;
    assert!(result.ok, "{}", result.output);
    assert!(result.output.contains("shell-ok"));

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": created.session.id}))
        .await?;
    assert!(attached
        .events
        .iter()
        .any(|event| event.kind == EventKind::ToolCall
            && event.payload["tool"] == "run_command"
            && event.payload["arguments"]["command"] == "printf shell-ok"));
    assert!(attached
        .events
        .iter()
        .any(|event| event.kind == EventKind::ToolResult
            && event.payload["tool"] == "run_command"
            && event.payload["ok"] == true));

    let readonly: SessionResult = client
        .request(
            "session.create",
            json!({"name": "readonly", "cwd": dir, "permission": "read-only"}),
        )
        .await?;
    let allowed: eix_core::ShellRunResult = client
        .request(
            "shell.run",
            json!({"session_id": readonly.session.id, "command": "printf safe"}),
        )
        .await?;
    assert!(allowed.ok, "{}", allowed.output);
    assert!(allowed.output.contains("safe"));

    assert!(client
        .request::<eix_core::ShellRunResult>(
            "shell.run",
            json!({
                "session_id": readonly.session.id,
                "command": "printf nope > blocked.txt",
            }),
        )
        .await
        .is_err());
    assert!(!dir.join("blocked.txt").exists());

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn undo_and_redo_toggle_the_newest_turn() -> anyhow::Result<()> {
    let dir = temp_dir("undo");
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
        .request("session.create", json!({"name": "undo", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "remember this"}),
        )
        .await?;
    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["session_id"] == session_id.as_str()
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            break;
        }
    }

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let before = attached.events.len();
    assert!(before >= 2);

    let undone: Value = client
        .request("session.undo", json!({"session_id": session_id}))
        .await?;
    assert!(undone["changed"].as_u64().unwrap_or(0) >= 2);
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert!(!attached
        .events
        .iter()
        .any(|event| event.kind == EventKind::UserMessage));
    assert!(!attached
        .events
        .iter()
        .any(|event| event.kind == EventKind::AssistantMessage));

    let redone: Value = client
        .request("session.redo", json!({"session_id": session_id}))
        .await?;
    assert!(redone["changed"].as_u64().unwrap_or(0) >= 2);
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert_eq!(attached.events.len(), before);
    assert!(attached
        .events
        .iter()
        .any(|event| event.kind == EventKind::UserMessage
            && event.payload["text"] == "remember this"));

    client
        .request::<Value>("session.undo", json!({"session_id": session_id}))
        .await?;
    assert!(client
        .request::<Value>("session.undo", json!({"session_id": session_id}))
        .await
        .is_err());

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn always_allow_skips_later_staged_writes_for_the_same_path() -> anyhow::Result<()> {
    let dir = temp_dir("always");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(repeat_tool_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-model".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "always", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let first: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "write it"}),
        )
        .await?;
    let proposed = loop {
        let notification = client.next_notification().await?;
        if notification.method == "patch.proposed" {
            break notification.params.clone();
        }
    };
    let patch_id = proposed["patch_id"].as_str().unwrap().to_string();
    let answered: eix_core::ApprovalResult = client
        .request(
            "approval.respond",
            json!({
                "session_id": session_id,
                "approval_id": patch_id,
                "decision": "always",
            }),
        )
        .await?;
    assert!(answered.accepted);
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, "hello");
    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["turn_id"] == first.turn_id.as_str()
        {
            assert!(notification.params["error"].is_null());
            break;
        }
    }

    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "write it again"}),
        )
        .await?;
    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            assert!(notification.params["error"].is_null());
            break;
        }
    }

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let applied: Vec<_> = attached
        .events
        .iter()
        .filter(|event| event.kind == EventKind::PatchApplied)
        .collect();
    assert_eq!(applied.len(), 2, "both writes were applied");
    assert!(
        applied[1].payload["message"]
            .as_str()
            .is_some_and(|message| message.contains("rule")),
        "write was not marked as rule-approved: {}",
        applied[1].payload
    );
    let audits: Vec<_> = attached
        .events
        .iter()
        .filter(|event| event.kind == EventKind::PermissionEvaluated)
        .collect();
    assert!(
        audits.iter().any(|event| event.payload["action"] == "ask"),
        "missing ask audit: {audits:?}"
    );
    let allowed = audits
        .iter()
        .find(|event| event.payload["action"] == "allow")
        .expect("session rule should produce an allow audit");
    assert_eq!(allowed.payload["rule_source"], "session");
    assert!(
        allowed.payload["rule"]
            .as_str()
            .is_some_and(|rule| rule.starts_with("always-write_file-")),
        "unexpected rule audit: {}",
        allowed.payload
    );

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

/// A provider that asks for the `mock` MCP server's `echo` tool, then answers.
async fn mcp_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
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
            let tool_count = value["messages"]
                .as_array()
                .map(|messages| {
                    messages
                        .iter()
                        .filter(|message| message["role"] == "tool")
                        .count()
                })
                .unwrap_or(0);
            if tool_count >= 5 {
                let chunk = json!({
                    "model": "mock-tools",
                    "choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}],
                });
                write_sse(&mut socket, &format!("data: {chunk}\n\ndata: [DONE]\n\n")).await;
                return;
            }
            let (name, arguments) = match tool_count {
                0 => ("mcp__mock__echo", json!({"text": "hi"}).to_string()),
                1 => ("mcp_list_resources", json!({"server": "mock"}).to_string()),
                2 => (
                    "mcp_read_resource",
                    json!({"server": "mock", "uri": "file:///note"}).to_string(),
                ),
                3 => ("mcp_list_prompts", json!({"server": "mock"}).to_string()),
                _ => (
                    "mcp_get_prompt",
                    json!({"server": "mock", "name": "review",
                           "arguments": {"path": "src"}})
                    .to_string(),
                ),
            };
            let chunk = json!({
                "model": "mock-tools",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": format!("call_mcp_{tool_count}"),
                            "type": "function",
                            "function": {"name": name, "arguments": arguments},
                        }]
                    },
                    "finish_reason": "tool_calls",
                }],
            });
            write_sse(&mut socket, &format!("data: {chunk}\n\ndata: [DONE]\n\n")).await;
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

/// Asks the parent to spawn one child, then never answers the child's request.
/// The test cancels the parent and expects the daemon to close the child's
/// provider call promptly instead of waiting for the HTTP timeout.
async fn hanging_agent_provider(listener: TcpListener) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let can_spawn = value["tools"]
                .as_array()
                .map(|tools| {
                    tools
                        .iter()
                        .any(|tool| tool["function"]["name"] == "spawn_agent")
                })
                .unwrap_or(false);
            if can_spawn {
                write_sse(
                    &mut socket,
                    &tool_call_event(
                        json!({"prompt": "investigate", "name": "scout"}),
                        "spawn_agent",
                        "call_agent",
                    ),
                )
                .await;
            } else {
                // Send one delta, then hold the connection open. Cancellation
                // must end the stream and persist the partial answer.
                use tokio::io::AsyncWriteExt;
                let chunk = json!({
                    "model": "mock-agent",
                    "choices": [{"delta": {"content": "child partial"}, "finish_reason": null}],
                });
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {chunk}\n\n"
                );
                if socket.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
                let _ = socket.flush().await;
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        });
    }
}

/// Reads a file containing secrets, then asks the daemon to write a value that
/// the daemon must restore from a provider placeholder.
async fn secret_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
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
            let events = match tool_results {
                0 => tool_call_event(
                    json!({"command": "cat secret.txt"}),
                    "run_command",
                    "call_1",
                ),
                1 => {
                    let placeholder = first_placeholder(&value).unwrap_or_default();
                    tool_call_event(
                        json!({"path": "out.txt", "content": placeholder}),
                        "write_file",
                        "call_2",
                    )
                }
                _ => {
                    let placeholder = first_placeholder(&value).unwrap_or_default();
                    let chunk = json!({
                        "model": "mock-secret",
                        "choices": [{"delta": {"content": format!("captured {placeholder}")}, "finish_reason": "stop"}],
                    });
                    format!("data: {chunk}\n\ndata: [DONE]\n\n")
                }
            };
            write_sse(&mut socket, &events).await;
        });
    }
}

fn tool_call_event(arguments: Value, tool: &str, call_id: &str) -> String {
    let chunk = json!({
        "model": "mock-secret",
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": call_id,
                    "type": "function",
                    "function": {"name": tool, "arguments": arguments.to_string()},
                }]
            },
            "finish_reason": "tool_calls",
        }],
    });
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

fn first_placeholder(value: &Value) -> Option<String> {
    let text = serde_json::to_string(value).ok()?;
    let start = text.find("{{eix_secret_")?;
    let rest = &text[start..];
    let end = rest.find("}}")? + 2;
    Some(rest[..end].to_string())
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
                let arguments =
                    json!({"command": "sed -i 's/before/after/' src/keep.txt"}).to_string();
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

/// A provider for the `ask_user` test: the first round asks a question, the
/// second (after the tool result) stops with "done".
async fn ask_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
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
                    "model": "mock-ask",
                    "choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}],
                });
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            } else {
                let arguments = json!({
                    "question": "Which database should we use?",
                    "options": [
                        {"label": "SQLite", "description": "Embedded, no setup"},
                        {"label": "Postgres", "description": "Server, better concurrency"},
                        {"label": "MySQL", "description": "Widely deployed"}
                    ]
                })
                .to_string();
                tool_call_events("call_ask", "ask_user", &arguments)
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

/// Provider for the memory test: persist a note on the first call, then answer.
/// A second session sees the note in its first request and reports it.
async fn memory_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
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
            let messages = value["messages"].as_array().cloned().unwrap_or_default();
            let last_is_tool = messages
                .last()
                .map(|message| message["role"] == "tool")
                .unwrap_or(false);
            let has_memory = messages.iter().any(|message| {
                message["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("prefers tabs")
            });
            let chunk = if last_is_tool {
                json!({
                    "model": "mock-memory",
                    "choices": [{"delta": {"content": "remembered"}, "finish_reason": "stop"}],
                })
            } else if has_memory {
                json!({
                    "model": "mock-memory",
                    "choices": [{"delta": {"content": "memory visible"}, "finish_reason": "stop"}],
                })
            } else {
                let arguments = json!({"action": "add", "text": "prefers tabs"}).to_string();
                json!({
                    "model": "mock-memory",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "mem_1",
                                "type": "function",
                                "function": {"name": "memory", "arguments": arguments},
                            }]
                        },
                        "finish_reason": "tool_calls",
                    }],
                })
            };
            let events = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            write_sse(&mut socket, &events).await;
        });
    }
}

#[tokio::test]
async fn memory_tool_persists_across_sessions() -> anyhow::Result<()> {
    let dir = temp_dir("memory");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(memory_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-memory".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;
    config.context.auto_compact = false;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;

    let created: SessionResult = client
        .request("session.create", json!({"name": "memory-one", "cwd": dir}))
        .await?;
    let first: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": created.session.id, "text": "remember my preference"}),
        )
        .await?;
    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["turn_id"] == first.turn_id.as_str()
        {
            assert!(notification.params["error"].is_null());
            break;
        }
    }
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": created.session.id}))
        .await?;
    assert!(attached
        .events
        .iter()
        .any(|event| { event.kind == EventKind::ToolCall && event.payload["tool"] == "memory" }));

    let second: SessionResult = client
        .request("session.create", json!({"name": "memory-two", "cwd": dir}))
        .await?;
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": second.session.id, "text": "what do you remember?"}),
        )
        .await?;
    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            assert!(notification.params["error"].is_null());
            break;
        }
    }
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": second.session.id}))
        .await?;
    let answer = attached
        .events
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::AssistantMessage)
        .map(|event| {
            event.payload["text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .unwrap_or_default();
    assert_eq!(answer, "memory visible");

    let bodies = bodies.lock().await;
    assert!(bodies
        .iter()
        .any(|body| { body.contains("Project memory") && body.contains("prefers tabs") }));

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

/// Provider that asks for two independent file reads in one batch. The daemon
/// should run them concurrently and still append the tool results in call order.
async fn parallel_read_provider(listener: TcpListener) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let messages = value["messages"].as_array().cloned().unwrap_or_default();
            let tool_messages: Vec<_> = messages
                .iter()
                .filter(|message| message["role"] == "tool")
                .collect();
            let chunk = if tool_messages.len() >= 2 {
                let first = tool_messages[tool_messages.len() - 2]["content"]
                    .as_str()
                    .unwrap_or_default();
                let second = tool_messages[tool_messages.len() - 1]["content"]
                    .as_str()
                    .unwrap_or_default();
                let answer = if first.contains("AAA") && second.contains("BBB") {
                    "order ok"
                } else {
                    "wrong order"
                };
                json!({
                    "model": "mock-parallel",
                    "choices": [{"delta": {"content": answer}, "finish_reason": "stop"}],
                })
            } else {
                json!({
                    "model": "mock-parallel",
                    "choices": [{
                        "delta": {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "read_a",
                                    "type": "function",
                                    "function": {"name": "read_file", "arguments": "{\"path\":\"a.txt\"}"},
                                },
                                {
                                    "index": 1,
                                    "id": "read_b",
                                    "type": "function",
                                    "function": {"name": "read_file", "arguments": "{\"path\":\"b.txt\"}"},
                                }
                            ]
                        },
                        "finish_reason": "tool_calls",
                    }],
                })
            };
            let events = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            write_sse(&mut socket, &events).await;
        });
    }
}

#[tokio::test]
async fn read_only_tool_batch_keeps_call_order() -> anyhow::Result<()> {
    let dir = temp_dir("parallel-read");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("a.txt"), "AAA\n")?;
    std::fs::write(dir.join("b.txt"), "BBB\n")?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(parallel_read_provider(listener));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-parallel".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;
    config.context.auto_compact = false;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "parallel", "cwd": dir}))
        .await?;
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": created.session.id, "text": "read both files"}),
        )
        .await?;
    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            assert!(notification.params["error"].is_null());
            break;
        }
    }
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": created.session.id}))
        .await?;
    let answer = attached
        .events
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::AssistantMessage)
        .map(|event| {
            event.payload["text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .unwrap_or_default();
    assert_eq!(answer, "order ok");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

/// Provider used by the context-window recovery test. `fail_next` makes the
/// next request return a real 400 context-length error. After the summary
/// request, the retried turn answers `recovered`.
async fn recovery_provider(
    listener: TcpListener,
    fail_next: Arc<AtomicBool>,
    seen_summary: Arc<AtomicBool>,
) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let fail_next = Arc::clone(&fail_next);
        let seen_summary = Arc::clone(&seen_summary);
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            if fail_next.swap(false, Ordering::SeqCst) {
                let error = r#"{"error":{"code":"context_length_exceeded","message":"prompt is too long"}}"#;
                write_http_error(&mut socket, "400 Bad Request", error).await;
                return;
            }
            let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let messages = value["messages"].as_array().cloned().unwrap_or_default();
            let is_summary = messages.iter().any(|message| {
                message["content"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("Summarize the conversation")
            });
            let content = if is_summary {
                seen_summary.store(true, Ordering::SeqCst);
                "summary text"
            } else if seen_summary.load(Ordering::SeqCst) {
                "recovered"
            } else {
                "ok"
            };
            let chunk = json!({
                "model": "mock-recovery",
                "choices": [{"delta": {"content": content}, "finish_reason": "stop"}],
            });
            let events = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            write_sse(&mut socket, &events).await;
        });
    }
}

#[tokio::test]
async fn context_length_error_forces_compaction_and_retries() -> anyhow::Result<()> {
    let dir = temp_dir("context-recovery");
    std::fs::create_dir_all(&dir)?;

    let fail_next = Arc::new(AtomicBool::new(false));
    let seen_summary = Arc::new(AtomicBool::new(false));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(recovery_provider(
        listener,
        Arc::clone(&fail_next),
        Arc::clone(&seen_summary),
    ));

    let mut config = Config::default();
    config.provider.base_url = format!("http://{addr}/v1");
    config.provider.model = "mock-recovery".to_string();
    config.provider.api_key = None;
    config.provider.timeout_secs = 30;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;
    config.context.budget_tokens = 100_000;
    config.context.auto_compact = true;
    config.context.auto_compact_threshold = 0.99;
    config.context.auto_compact_keep_turns = 2;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "recovery", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    for text in ["one", "two", "three"] {
        let submitted: SubmitResult = client
            .request(
                "prompt.submit",
                json!({"session_id": session_id, "text": text}),
            )
            .await?;
        loop {
            let notification = client.next_notification().await?;
            if notification.method == "turn.completed"
                && notification.params["turn_id"] == submitted.turn_id.as_str()
            {
                assert!(notification.params["error"].is_null());
                break;
            }
        }
    }

    fail_next.store(true, Ordering::SeqCst);
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "four"}),
        )
        .await?;
    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            assert!(
                notification.params["error"].is_null(),
                "{}",
                notification.params["error"]
            );
            break;
        }
    }

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert!(attached
        .events
        .iter()
        .any(|event| event.kind == EventKind::ContextCompacted));
    let answer = attached
        .events
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::AssistantMessage)
        .map(|event| {
            event.payload["text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .unwrap_or_default();
    assert_eq!(answer, "recovered");
    assert!(seen_summary.load(Ordering::SeqCst));

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}
