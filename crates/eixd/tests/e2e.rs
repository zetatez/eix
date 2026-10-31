//! End-to-end test: real Unix socket, real SQLite, streaming mock provider.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use eix_core::{
    AgentRole, ApprovalResult, AttachResult, Config, DoctorReport, EventKind, EventsResult,
    FormatterConfig, HelloResult, ModelConfig, PermissionAction, PermissionRule, ProviderConfig,
    Request, Response, ServerMessage, SessionMode, SessionResult, SubmitResult, UndoResult,
    WorkspaceDiffResult, PROTOCOL_VERSION,
};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, UnixStream};
use tokio::sync::{mpsc, oneshot, Mutex};

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

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
async fn assistant_images_are_persisted_from_provider_parts() -> anyhow::Result<()> {
    let dir = temp_dir("assistant-image");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(image_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-image".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "image", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "draw it"}),
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
    let assistant = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::AssistantMessage)
        .expect("assistant event");
    assert_eq!(assistant.payload["text"], "done");
    assert_eq!(assistant.payload["images"][0]["mime"], "image/png");
    assert_eq!(assistant.payload["images"][0]["data"], "QUJD");

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

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
async fn a_prompt_submitted_while_busy_is_queued_and_started() -> anyhow::Result<()> {
    let dir = temp_dir("queue");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    // Delay every answer so the second submit reliably races the first turn.
    let mock = tokio::spawn(mock_provider_delayed(
        listener,
        Arc::clone(&bodies),
        Duration::from_millis(300),
    ));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "queue", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let first: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "first"}),
        )
        .await?;
    assert!(!first.queued);
    assert!(!first.turn_id.is_empty());

    let second: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "second"}),
        )
        .await?;
    assert!(second.queued, "the second prompt should be queued");
    assert!(
        second.turn_id.is_empty(),
        "a queued prompt has no turn yet: {}",
        second.turn_id
    );

    // The queued prompt steers the running turn instead of waiting for a new
    // one: the turn folds it in at the next step and answers both together.
    let mut started = Vec::new();
    let mut completed = 0;
    let mut saw_queued = false;
    let mut saw_steered = false;
    while completed < 1 {
        let notification = client.next_notification().await?;
        match notification.method.as_str() {
            "prompt.queued" => {
                saw_queued = true;
                assert_eq!(notification.params["steering"], true);
            }
            "prompt.steered" => {
                saw_steered = true;
                assert_eq!(notification.params["count"], 1);
            }
            "turn.started" => started.push(
                notification.params["turn_id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            ),
            "turn.completed" => completed += 1,
            _ => {}
        }
    }
    assert!(saw_queued, "prompt.queued was not emitted");
    assert!(saw_steered, "prompt.steered was not emitted");
    assert_eq!(started.len(), 1, "{started:?}");

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let users = attached
        .events
        .iter()
        .filter(|event| event.kind == EventKind::UserMessage)
        .count();
    let answers = attached
        .events
        .iter()
        .filter(|event| event.kind == EventKind::AssistantMessage)
        .count();
    assert_eq!(users, 2, "both prompts are recorded");
    assert!(answers >= 2, "both turns produced an answer");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn cancelling_a_session_drops_its_queued_prompts_and_broadcasts() -> anyhow::Result<()> {
    let dir = temp_dir("queue-cancel");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider_delayed(
        listener,
        Arc::clone(&bodies),
        Duration::from_millis(600),
    ));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "queue-cancel", "cwd": dir}),
        )
        .await?;
    let session_id = created.session.id.clone();

    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "first"}),
        )
        .await?;
    let second: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "second"}),
        )
        .await?;
    assert!(second.queued, "the second prompt should be queued");
    while client.next_notification().await?.method != "prompt.queued" {}

    client
        .request::<Value>("prompt.cancel", json!({"session_id": session_id}))
        .await?;
    let mut dropped = None;
    loop {
        let notification = client.next_notification().await?;
        match notification.method.as_str() {
            "prompt.cancelled" => {
                dropped = notification.params["dropped"].as_u64();
            }
            "turn.completed" => break,
            _ => {}
        }
    }
    assert_eq!(dropped, Some(1), "the queued prompt was dropped");

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let users = attached
        .events
        .iter()
        .filter(|event| event.kind == EventKind::UserMessage)
        .count();
    assert_eq!(users, 1, "the dropped prompt never started");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn auto_names_an_unnamed_session_after_two_turns() -> anyhow::Result<()> {
    let dir = temp_dir("auto-name");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    let original_name = created.session.name.clone();
    assert!(!original_name.is_empty());

    for text in ["first request", "second request"] {
        client
            .request::<SubmitResult>(
                "prompt.submit",
                json!({"session_id": session_id, "text": text}),
            )
            .await?;
        wait_turn(&mut client).await?;
    }

    let mut renamed = None;
    for _ in 0..100 {
        let attached: AttachResult = client
            .request("session.attach", json!({"session_id": session_id}))
            .await?;
        if attached.session.name == "Hello From Mock" {
            renamed = Some(attached.session);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let session = renamed.expect("the second turn should generate a session name");
    assert_ne!(session.name, original_name);
    assert!(
        session
            .name
            .chars()
            .all(|ch| ch.is_alphanumeric() || ch == ' '),
        "{}",
        session.name
    );

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.provider_runtime.retry_attempts = 3;
    config.provider_runtime.retry_base_ms = 1;
    config.provider_runtime.retry_max_secs = 1;

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.provider_runtime.retry_attempts = 2;
    config.provider_runtime.retry_base_ms = 500;
    config.provider_runtime.retry_max_secs = 30;

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.provider_runtime.retry_attempts = 2;
    config.provider_runtime.retry_base_ms = 1;
    config.provider_runtime.retry_max_secs = 1;

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.provider_runtime.retry_attempts = 3;
    config.provider_runtime.retry_base_ms = 1;
    config.provider_runtime.retry_max_secs = 1;

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

/// Answers every request with the same tool call, for a model stuck in a loop.
async fn repeating_tool_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
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
            // Identical arguments and a fresh call id every round, which is
            // what a model that ignores the previous result produces.
            let chunk = json!({
                "model": "mock-tools",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": format!("call_{}", bodies.lock().await.len()),
                            "type": "function",
                            "function": {
                                "name": "exec_command",
                                "arguments": "{\"cmd\":\"cat notes.txt\"}",
                            },
                        }]
                    },
                    "finish_reason": "tool_calls",
                }],
            });
            write_sse(&mut socket, &format!("data: {chunk}\n\ndata: [DONE]\n\n")).await;
        });
    }
}

/// Answers the first `rounds` requests with distinct read-only calls (so the
/// doomloop guard does not fire), then returns a final text answer.
async fn progressing_tool_provider(
    listener: TcpListener,
    bodies: Arc<Mutex<Vec<String>>>,
    rounds: usize,
) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let bodies = Arc::clone(&bodies);
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            let round = {
                let mut bodies = bodies.lock().await;
                bodies.push(body);
                bodies.len() - 1
            };
            let chunk = if round < rounds {
                json!({
                    "model": "mock-tools",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": format!("call_{round}"),
                                "type": "function",
                                "function": {
                                    "name": "exec_command",
                                    "arguments": format!("{{\"cmd\":\"ls d{round}\"}}"),
                                },
                            }]
                        },
                        "finish_reason": "tool_calls",
                    }],
                })
            } else {
                json!({
                    "model": "mock-tools",
                    "choices": [{
                        "delta": {"content": "finished"},
                        "finish_reason": "stop",
                    }],
                })
            };
            write_sse(&mut socket, &format!("data: {chunk}\n\ndata: [DONE]\n\n")).await;
        });
    }
}

/// Answers each round with a *different* read of the same file, so the
/// identical-batch guard never fires while the inspection guard should.
async fn rereading_tool_provider(
    listener: TcpListener,
    bodies: Arc<Mutex<Vec<String>>>,
    rounds: usize,
) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let bodies = Arc::clone(&bodies);
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            let round = {
                let mut bodies = bodies.lock().await;
                bodies.push(body);
                bodies.len() - 1
            };
            let chunk = if round < rounds {
                json!({
                    "model": "mock-tools",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": format!("call_{round}"),
                                "type": "function",
                                "function": {
                                    "name": "exec_command",
                                    "arguments": format!(
                                        "{{\"cmd\":\"sed -n '{}p' notes.txt\"}}",
                                        round + 1
                                    ),
                                },
                            }]
                        },
                        "finish_reason": "tool_calls",
                    }],
                })
            } else {
                json!({
                    "model": "mock-tools",
                    "choices": [{
                        "delta": {"content": "finished"},
                        "finish_reason": "stop",
                    }],
                })
            };
            write_sse(&mut socket, &format!("data: {chunk}\n\ndata: [DONE]\n\n")).await;
        });
    }
}

/// There is no fixed round cap: the model keeps going past the old 64-round
/// limit and ends the turn by returning a normal answer.
#[tokio::test]
async fn daemon_has_no_fixed_tool_round_cap() -> anyhow::Result<()> {
    let dir = temp_dir("unlimited");
    std::fs::create_dir_all(&dir)?;
    const ROUNDS: usize = 70;
    for round in 0..ROUNDS {
        std::fs::create_dir_all(dir.join(format!("d{round}")))?;
    }

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(progressing_tool_provider(
        listener,
        Arc::clone(&bodies),
        ROUNDS,
    ));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.tools.sandbox = eix_core::SandboxMode::Off;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "unlimited", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "list every directory"}),
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
    };

    assert!(
        completed["error"].is_null(),
        "unexpected error: {}",
        completed["error"]
    );
    // One request per tool round plus the final answer request.
    assert_eq!(bodies.lock().await.len(), ROUNDS + 1);

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

/// A model that keeps sending the same tool call must be stopped by the
/// doomloop guard, not allowed to spend the whole round budget on one request.
#[tokio::test]
async fn daemon_stops_a_repeating_tool_batch() -> anyhow::Result<()> {
    let dir = temp_dir("doomloop");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("notes.txt"), "unchanged\n")?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(repeating_tool_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.tools.sandbox = eix_core::SandboxMode::Off;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "doomloop", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "read the notes"}),
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
    };

    let error = completed["error"].as_str().unwrap_or_default();
    assert!(error.contains("identical"), "unexpected error: {error}");
    // Five identical batches: the guard fired long before the 40-round budget.
    assert_eq!(bodies.lock().await.len(), 5);

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn daemon_reminds_a_model_that_keeps_rereading_the_same_file() -> anyhow::Result<()> {
    let dir = temp_dir("reread");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("notes.txt"), "1\n2\n3\n4\n5\n6\n")?;

    const ROUNDS: usize = 6;
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(rereading_tool_provider(
        listener,
        Arc::clone(&bodies),
        ROUNDS,
    ));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.tools.sandbox = eix_core::SandboxMode::Off;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "reread", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "edit the notes"}),
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
    };

    assert!(
        completed["error"].is_null(),
        "unexpected error: {}",
        completed["error"]
    );
    let bodies = bodies.lock().await;
    assert_eq!(bodies.len(), ROUNDS + 1);
    // The third read of the same file trips the guard, and the reminder stays
    // in the next requests while the model keeps re-reading.
    assert!(
        bodies[3].contains("Action check"),
        "third read did not warn: {}",
        bodies[3]
    );
    assert!(bodies[3].contains("notes.txt"), "{}", bodies[3]);
    assert!(
        bodies[ROUNDS - 1].contains("Action check"),
        "reminder did not persist: {}",
        bodies[ROUNDS - 1]
    );

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn context_info_reports_the_token_ledger() -> anyhow::Result<()> {
    let dir = temp_dir("context-info");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "context", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "first question"}),
        )
        .await?;
    wait_turn(&mut client).await?;

    let info: eix_core::ContextLedgerResult = client
        .request("context.info", json!({"session_id": session_id}))
        .await?;
    assert!(info.total_tokens > 0);
    assert!(info.budget_tokens > 0);
    assert!(!info.sectors.is_empty());
    assert!(info.entries.iter().any(|entry| entry.kind == "user"));

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "clear", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    // A workspace memory file is part of the context `/clear` promises to drop.
    let memory = eixd::memory::MemoryStore::new(dir.join("data").join("memory"));
    let memory_path = memory.path_for(&dir);
    std::fs::create_dir_all(memory_path.parent().unwrap())?;
    std::fs::write(&memory_path, "remember this")?;

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
        !cleared
            .entries
            .iter()
            .any(|entry| entry.kind == "user" || entry.kind == "assistant"),
        "clear removes the conversation from the ledger: {:?}",
        cleared.entries
    );
    assert!(
        !memory_path.exists(),
        "clear removes the workspace memory file"
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
    // Nothing from the previous conversation survives, not even a marker.
    assert!(!attached
        .events
        .iter()
        .any(|event| event.kind == EventKind::ContextCleared));
    assert!(!attached.events.iter().any(|event| {
        event.kind == EventKind::UserMessage && event.payload["text"] == "first question"
    }));

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

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
    // ask the provider. The RPC only acknowledges the request; the summary is
    // appended when the background task finishes.
    let accepted: eix_core::ContextCompactResult = client
        .request(
            "context.compact",
            json!({
                "session_id": session_id,
                "generate": true,
                // `/compact <notes>` steers the summary instead of replacing
                // it with literal text.
                "guidance": "keep the database migration details",
            }),
        )
        .await?;
    assert!(accepted.accepted);
    let compacted = wait_notification(&mut client, "context.compacted").await?;
    assert_eq!(compacted["session_id"], session_id);
    assert_eq!(compacted["auto"], false);

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
    assert!(
        summary_request["messages"]
            .to_string()
            .contains("keep the database migration details"),
        "guidance steers the summary: {}",
        summary_request["messages"]
    );
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
async fn context_compact_is_accepted_while_a_turn_is_running() -> anyhow::Result<()> {
    let dir = temp_dir("compact-busy");
    std::fs::create_dir_all(&dir)?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider_delayed(
        listener,
        Arc::new(Mutex::new(Vec::<String>::new())),
        Duration::from_millis(500),
    ));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "compact-busy", "cwd": dir}),
        )
        .await?;
    let session_id = created.session.id.clone();

    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "long discussion"}),
        )
        .await?;

    // The turn is still waiting on the provider. Compaction must accept the
    // request rather than returning `busy`; the summary runs in the
    // background.
    let accepted: eix_core::ContextCompactResult = client
        .request(
            "context.compact",
            json!({"session_id": session_id, "generate": true}),
        )
        .await?;
    assert!(accepted.accepted);

    let mut completed = false;
    let mut compacted = false;
    while !(completed && compacted) {
        let notification = client.next_notification().await?;
        match notification.method.as_str() {
            "turn.completed" => {
                assert_eq!(notification.params["turn_id"], submitted.turn_id);
                assert!(notification.params["error"].is_null());
                completed = true;
            }
            "context.compacted" => compacted = true,
            _ => {}
        }
    }

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

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
        "---\ndescription: Review changes\nrole: both\n---\n\nFind bugs.\n",
    )?;
    std::fs::write(
        dir.join(".eix/commands/review.md"),
        "---\ndescription: Review a path\nagent: reviewer\n---\n\nReview $ARGUMENTS carefully.\n",
    )?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider(listener, Arc::new(Mutex::new(Vec::new()))));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
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
        ProviderConfig {
            name: Some("Remote".to_string()),
            base_url: format!("http://{addr}/v1"),
            ..ProviderConfig::default()
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
    assert_eq!(models.models[2].id, "openai/mock-auto");
    assert_eq!(models.models[3].id, "openai/mock-other");
    assert_eq!(models.models[3].context_tokens, Some(12_345));
    assert_eq!(models.models[4].id, "remote/mock-auto");
    assert_eq!(models.models[4].provider, "Remote");
    assert_eq!(models.models[5].id, "remote/mock-other");
    assert_eq!(models.default_model, "mock-model");
    assert!(models
        .providers
        .iter()
        .any(|provider| provider.key == "openai"));

    // `model.use` changes the daemon fallback without touching the session or
    // the config; `default` restores the configured value.
    let switched: eix_core::ModelUseResult = client
        .request("model.use", json!({"model": "mock/second"}))
        .await?;
    assert_eq!(switched.model, "mock/second");
    let after_use: eix_core::ModelListResult = client.request("model.list", Value::Null).await?;
    assert_eq!(after_use.default_model, "mock/second");
    let restored: eix_core::ModelUseResult = client
        .request("model.use", json!({"model": "default"}))
        .await?;
    assert_eq!(restored.model, "mock-model");
    let unknown = client
        .request_error("model.use", json!({"model": "nope/missing"}))
        .await?;
    assert_eq!(unknown, "invalid_params");

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
    assert!(agents
        .agents
        .iter()
        .all(|agent| agent.name != "build" && agent.name != "plan"));
    let reviewer = agents
        .agents
        .iter()
        .find(|agent| agent.name == "reviewer")
        .expect("reviewer agent");
    assert_eq!(reviewer.role, AgentRole::Both);
    assert_eq!(reviewer.prompt, "Find bugs.");

    let selected: SessionResult = client
        .request(
            "session.agent",
            json!({"session_id": session_id, "agent": "reviewer"}),
        )
        .await?;
    assert_eq!(selected.session.agent.as_deref(), Some("reviewer"));

    // Mode is independent of the selected agent.
    let planned: SessionResult = client
        .request(
            "session.mode",
            json!({"session_id": session_id, "mode": "plan"}),
        )
        .await?;
    assert_eq!(planned.session.mode, SessionMode::Plan);
    assert_eq!(planned.session.agent.as_deref(), Some("reviewer"));
    let automated: SessionResult = client
        .request(
            "session.mode",
            json!({"session_id": session_id, "mode": "auto"}),
        )
        .await?;
    assert_eq!(automated.session.mode, SessionMode::Auto);

    let code = client
        .request_error(
            "session.agent",
            json!({"session_id": session_id, "agent": "missing"}),
        )
        .await?;
    assert_eq!(code, "invalid_params");

    let commands = eix_core::load_commands(&dir, &eix_core::CompatConfig::default());
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
async fn stored_api_key_authorizes_provider_requests() -> anyhow::Result<()> {
    let dir = temp_dir("auth-store");
    std::fs::create_dir_all(&dir)?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let headers = Arc::new(Mutex::new(Vec::new()));
    let mock = tokio::spawn(header_capturing_provider(listener, Arc::clone(&headers)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".into();
    config.default_provider_mut().api_key = None;
    config.default_provider_mut().api_key_env = String::new();

    // Written before the daemon starts; `eix auth login` uses the same path.
    let data = dir.join("data");
    eixd::auth::Store::under_data_dir(&data).save_api_key("openai", "sk-from-store")?;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), data));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "auth", "cwd": dir}))
        .await?;
    let _: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": created.session.id, "text": "hi"}),
        )
        .await?;
    assert_eq!(wait_turn(&mut client).await?, "ok");

    let captured = headers.lock().await;
    assert_eq!(captured.len(), 1, "{captured:?}");
    assert!(
        captured[0]
            .to_ascii_lowercase()
            .contains("authorization: bearer sk-from-store"),
        "{}",
        captured[0]
    );
    drop(captured);

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.mcp.servers.insert(
        "mock".to_string(),
        eix_core::McpServerConfig {
            command: "python3".to_string(),
            args: vec![script.to_string_lossy().into_owned()],
            ..eix_core::McpServerConfig::default()
        },
    );
    // `mcp.set_enabled` persists the flag; keep it inside the test directory
    // instead of the developer's real config file.
    config.explicit_path = Some(dir.join("config.toml"));

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
    assert!(mcp.servers[0].enabled);
    assert!(mcp.servers[0].error.is_none(), "{:?}", mcp.servers[0].error);
    assert_eq!(mcp.servers[0].tools, 1);
    assert_eq!(mcp.servers[0].resources, 1);
    assert_eq!(mcp.servers[0].prompts, 1);

    // Disabling drops the live client and persists `enabled = false`.
    let toggled: Value = client
        .request("mcp.set_enabled", json!({"name": "mock", "enabled": false}))
        .await?;
    assert_eq!(toggled["enabled"], false);
    let mcp: eix_core::McpListResult = client.request("mcp.list", Value::Null).await?;
    assert!(!mcp.servers[0].enabled);
    assert_eq!(mcp.servers[0].tools, 0);
    let saved = std::fs::read_to_string(dir.join("config.toml"))?;
    assert!(
        saved.contains("enabled = false"),
        "toggle was not persisted: {saved}"
    );

    // Re-enabling spawns the server again on the next listing.
    client
        .request::<Value>("mcp.set_enabled", json!({"name": "mock", "enabled": true}))
        .await?;
    let mcp: eix_core::McpListResult = client.request("mcp.list", Value::Null).await?;
    assert!(mcp.servers[0].enabled);
    assert_eq!(mcp.servers[0].tools, 1);
    assert!(mcp.servers[0].error.is_none(), "{:?}", mcp.servers[0].error);

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
async fn plugin_tools_and_commands_are_hosted() -> anyhow::Result<()> {
    if std::process::Command::new("python3")
        .arg("--version")
        .output()
        .is_err()
    {
        return Ok(()); // no Python: nothing to run the plugin with
    }
    let dir = temp_dir("plugin");
    std::fs::create_dir_all(&dir)?;
    let script = dir.join("plugin.py");
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
            "name": "mock",
            "version": "0.1.0",
            "api_version": 2,
            "features": ["content_blocks", "cancel"],
            "tools": [{
                "name": "echo",
                "description": "Echo text",
                "input_schema": {"type": "object",
                                 "properties": {"text": {"type": "string"}},
                                 "required": ["text"]},
                "read_only": True}],
            "commands": [{
                "name": "review",
                "description": "Review a path",
                "args": "<path>",
                "prompt": "Review $ARGUMENTS"}],
            "hooks": [{
                "point": "tool.after",
                "priority": 10,
                "timeout_ms": 0,
                "on_error": "ignore"}],
            "events": []}})
    elif method == "tools/call":
        text = msg["params"]["arguments"].get("text", "")
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {
            "ok": True,
            "content": [{"type": "text", "text": "plugin:" + text}]}})
    elif method == "commands/run":
        arguments = msg["params"].get("arguments", "")
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {
            "ok": True,
            "output": "words:" + str(len(arguments.split()))}})
    elif method == "hooks/run":
        output = msg["params"].get("output", "")
        send({"jsonrpc": "2.0", "id": msg["id"], "result": {
            "output": output + "|hooked"}})
    else:
        send({"jsonrpc": "2.0", "id": msg.get("id"),
              "error": {"code": -32601, "message": "unknown method"}})
"#,
    )?;
    std::fs::write(
        dir.join("eix-plugin.toml"),
        r#"schema = 2
name = "mock"
version = "0.1.0"
api_version = 2
description = "Mock plugin"
capabilities = ["tools", "commands", "hooks"]
settings = "settings.schema.json"

[components.server]
command = "python3"
entry = "plugin.py"

[[contributes.commands]]
name = "triage"
description = "Triage an issue"
args = "<issue>"
prompt = "Triage $ARGUMENTS"

[[contributes.commands]]
name = "count"
description = "Count words"
args = "<text>"
action = "count"
"#,
    )?;
    std::fs::write(
        dir.join("settings.schema.json"),
        r#"{
          "type": "object",
          "properties": {
            "project": {"type": "string", "title": "Project"}
          }
        }"#,
    )?;

    std::fs::create_dir_all(dir.join("prompts"))?;
    std::fs::write(
        dir.join("prompts/eix-plugin.toml"),
        r#"schema = 2
name = "prompts"
version = "0.2.0"
api_version = 2
description = "Declarative commands only"
capabilities = ["commands"]

[[contributes.commands]]
name = "triage"
description = "Triage an issue"
args = "<issue>"
prompt = "Triage $ARGUMENTS"
"#,
    )?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(plugin_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.load_cwd = dir.clone();
    config.provider_runtime.timeout_secs = 30;
    config.plugins.insert(
        "mock".to_string(),
        eix_core::PluginConfig {
            // The manifest supplies the process command and the declarative
            // `triage`/`count` commands; the plugin server supplies `echo` and
            // `review`.
            manifest: Some(std::path::PathBuf::from("eix-plugin.toml")),
            settings: std::collections::BTreeMap::from([(
                "project".to_string(),
                Value::String("acme".into()),
            )]),
            ..eix_core::PluginConfig::default()
        },
    );
    config.plugins.insert(
        "prompts".to_string(),
        eix_core::PluginConfig {
            // No server component: this package is declarative only and must
            // not spawn a process.
            manifest: Some(std::path::PathBuf::from("prompts/eix-plugin.toml")),
            ..eix_core::PluginConfig::default()
        },
    );

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;

    let list: eix_core::PluginListResult = client.request("plugin.list", Value::Null).await?;
    assert_eq!(list.plugins.len(), 2);
    let mock_info = list
        .plugins
        .iter()
        .find(|plugin| plugin.name == "mock")
        .expect("mock plugin");
    assert_eq!(mock_info.state, eix_core::PluginState::Ready);
    assert_eq!(mock_info.version, "0.1.0");
    assert_eq!(mock_info.tools, 1);
    assert_eq!(mock_info.commands, 3);
    assert_eq!(mock_info.hooks, 1);
    assert!(mock_info.error.is_none());
    let prompts = list
        .plugins
        .iter()
        .find(|plugin| plugin.name == "prompts")
        .expect("declarative plugin");
    assert_eq!(prompts.state, eix_core::PluginState::Ready);
    assert_eq!(prompts.command, "");
    assert_eq!(prompts.tools, 0);
    assert_eq!(prompts.commands, 1);
    assert!(prompts.error.is_none());

    let commands: eix_core::PluginCommandList =
        client.request("plugin.commands", Value::Null).await?;
    assert_eq!(commands.commands.len(), 4);
    let review = commands
        .commands
        .iter()
        .find(|command| command.name == "plugin/mock/review")
        .expect("runtime command");
    assert_eq!(review.prompt, "Review $ARGUMENTS");
    let triage = commands
        .commands
        .iter()
        .find(|command| command.name == "plugin/mock/triage")
        .expect("declarative command");
    assert_eq!(triage.prompt, "Triage $ARGUMENTS");
    let count = commands
        .commands
        .iter()
        .find(|command| command.name == "plugin/mock/count")
        .expect("declarative action command");
    assert!(count.action);
    let prompts_triage = commands
        .commands
        .iter()
        .find(|command| command.name == "plugin/prompts/triage")
        .expect("declarative-only command");
    assert_eq!(prompts_triage.prompt, "Triage $ARGUMENTS");

    let created: SessionResult = client
        .request("session.create", json!({"name": "plugin", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    // Action commands call `commands/run` and return text without a model turn.
    let action: eix_core::PluginCommandRunResult = client
        .request(
            "plugin.command.run",
            json!({
                "name": "plugin/mock/count",
                "arguments": "one two three",
                "session_id": session_id,
            }),
        )
        .await?;
    assert!(action.ok);
    assert_eq!(action.output.as_deref(), Some("words:3"));
    assert!(action.prompt.is_none());

    // Settings expose the manifest schema and the configured values.
    let settings: eix_core::PluginSettingsResult = client
        .request("plugin.settings", json!({"name": "mock"}))
        .await?;
    assert_eq!(settings.plugin, "mock");
    assert_eq!(
        settings.schema.as_ref().unwrap()["properties"]["project"]["title"],
        "Project"
    );
    assert_eq!(settings.values["project"], "acme");

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
    assert_eq!(approval["tool"], "plugin__mock__echo");
    let approval_id = approval["approval_id"].as_str().unwrap().to_string();
    let answered: ApprovalResult = client
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
    let result = attached
        .events
        .iter()
        .find(|event| {
            event.kind == EventKind::ToolResult && event.payload["tool"] == "plugin__mock__echo"
        })
        .expect("plugin tool result");
    assert!(
        result.payload["output"]
            .as_str()
            .unwrap()
            .contains("plugin:hi"),
        "{}",
        result.payload["output"]
    );
    // `tool.after` hooks can rewrite the recorded output.
    assert_eq!(
        result.payload["output"].as_str().unwrap(),
        "plugin:hi|hooked",
        "{}",
        result.payload["output"]
    );
    let snapshot = attached
        .events
        .iter()
        .find(|event| {
            event.kind == EventKind::Snapshot && event.payload["tool"] == "plugin__mock__echo"
        })
        .expect("plugin call snapshot");
    assert_eq!(snapshot.payload["scope"], "tree");
    assert_eq!(snapshot.payload["ok"], true);

    let bodies = bodies.lock().await;
    assert_eq!(bodies.len(), 3);
    let first: Value = serde_json::from_str(&bodies[0])?;
    let advertised = |body: &Value, name: &str| {
        body["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|tool| tool["function"]["name"] == name))
    };
    // The base request only carries `tool_search`; the plugin schema arrives
    // after the search result and is then used for the actual call.
    assert!(advertised(&first, "tool_search"), "{}", first["tools"]);
    assert!(!advertised(&first, "plugin__mock__echo"));
    let second: Value = serde_json::from_str(&bodies[1])?;
    assert!(
        advertised(&second, "plugin__mock__echo"),
        "{}",
        second["tools"]
    );
    drop(bodies);

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
#[ignore = "needs `cargo build -p eix-plugin-hello-dylib`"]
async fn dylib_plugin_is_hosted_through_the_manager() -> anyhow::Result<()> {
    let library = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/libeix_plugin_hello_dylib.so");
    if !library.exists() {
        eprintln!("skipping: {} does not exist", library.display());
        return Ok(());
    }

    let dir = temp_dir("dylib");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(plugin_provider_for(
        listener,
        Arc::clone(&bodies),
        "plugin__hello-dylib__echo",
    ));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.load_cwd = dir.clone();
    config.provider_runtime.timeout_secs = 30;
    config.plugins.insert(
        "hello-dylib".to_string(),
        eix_core::PluginConfig {
            library: Some(library.clone()),
            capabilities: vec![
                eix_core::PluginCapability::Tools,
                eix_core::PluginCapability::Commands,
            ],
            ..eix_core::PluginConfig::default()
        },
    );

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;

    let list: eix_core::PluginListResult = client.request("plugin.list", Value::Null).await?;
    assert_eq!(list.plugins.len(), 1);
    let info = &list.plugins[0];
    assert_eq!(info.name, "hello-dylib");
    assert_eq!(info.state, eix_core::PluginState::Ready);
    assert_eq!(info.tools, 1);
    assert_eq!(info.commands, 1);
    assert!(info.command.ends_with("libeix_plugin_hello_dylib.so"));
    assert!(info.error.is_none());

    let commands: eix_core::PluginCommandList =
        client.request("plugin.commands", Value::Null).await?;
    let sync = commands
        .commands
        .iter()
        .find(|command| command.name == "plugin/hello-dylib/sync")
        .expect("action command from describe");
    assert!(sync.action);

    let created: SessionResult = client
        .request("session.create", json!({"name": "dylib", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let action: eix_core::PluginCommandRunResult = client
        .request(
            "plugin.command.run",
            json!({
                "name": "plugin/hello-dylib/sync",
                "arguments": "--full",
                "session_id": session_id,
            }),
        )
        .await?;
    assert!(action.ok);
    assert_eq!(action.output.as_deref(), Some("synced --full"));

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
    assert_eq!(approval["tool"], "plugin__hello-dylib__echo");
    let approval_id = approval["approval_id"].as_str().unwrap().to_string();
    let answered: ApprovalResult = client
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
    let result = attached
        .events
        .iter()
        .find(|event| {
            event.kind == EventKind::ToolResult
                && event.payload["tool"] == "plugin__hello-dylib__echo"
        })
        .expect("dylib tool result");
    assert!(
        result.payload["output"].as_str().unwrap().contains("hi"),
        "{}",
        result.payload["output"]
    );

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn exec_command_timeout_kills_the_process() -> anyhow::Result<()> {
    let dir = temp_dir("exec-timeout");
    std::fs::create_dir_all(&dir)?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(timeout_provider(listener));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.tools.sandbox = eix_core::SandboxMode::Off;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    // `sleep` is not provably read-only, so use full mode to skip the approval
    // card and exercise the timeout itself.
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "timeout", "cwd": dir, "mode": "full"}),
        )
        .await?;
    let session_id = created.session.id.clone();
    let started = std::time::Instant::now();
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "run it"}),
        )
        .await?;
    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            break;
        }
    }
    // `timeout_ms` is a hard limit: the 30s sleep must not run to completion.
    assert!(
        started.elapsed() < std::time::Duration::from_secs(20),
        "timeout did not interrupt the command"
    );

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let result = attached
        .events
        .iter()
        .find(|event| {
            event.kind == EventKind::ToolResult && event.payload["tool"] == "exec_command"
        })
        .expect("exec_command result");
    assert_eq!(result.payload["ok"], false);
    let output = result.payload["output"].as_str().expect("output");
    assert!(output.contains("command timed out after 300ms"), "{output}");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

/// Answers the first request with a `sleep` that outlives `timeout_ms`, then
/// stops after the tool result.
async fn timeout_provider(listener: TcpListener) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let has_tool_result = value["messages"]
                .as_array()
                .map(|messages| messages.iter().any(|message| message["role"] == "tool"))
                .unwrap_or(false);
            let events = if has_tool_result {
                let chunk = json!({
                    "model": "mock-timeout",
                    "choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}],
                });
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            } else {
                tool_call_event(
                    json!({"cmd": "sleep 30", "timeout_ms": 300, "yield_time_ms": 250}),
                    "exec_command",
                    "call_timeout",
                )
            };
            write_sse(&mut socket, &events).await;
        });
    }
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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

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

/// Wait for a specific bus notification, discarding unrelated ones.
async fn wait_notification(client: &mut TestClient, method: &str) -> anyhow::Result<Value> {
    loop {
        let notification = client.next_notification().await?;
        if notification.method == method {
            return Ok(notification.params);
        }
    }
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

#[tokio::test]
async fn cancel_keeps_the_partial_answer_and_returns_to_idle() -> anyhow::Result<()> {
    let dir = temp_dir("cancel");
    std::fs::create_dir_all(&dir)?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(slow_provider(listener));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

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

#[allow(clippy::field_reassign_with_default)]
#[tokio::test]
async fn auto_applied_write_records_its_diff() -> anyhow::Result<()> {
    let dir = temp_dir("auto-diff");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("out.txt"), "before")?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(tool_provider(listener, Arc::clone(&bodies)));

    // Defaults apply in-workspace edits immediately, so this write never
    // produces a `patch_proposed` event. The formatter changes the bytes after
    // the write; the recorded diff must show the formatted result.
    let mut config = Config::default();
    config.formatter = FormatterConfig {
        command: Some("sed -i 's/hello/HELLO/'".into()),
        extensions: vec!["txt".into()],
        timeout_secs: 30,
    }
    .into();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "auto-diff", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "write out.txt"}),
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
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, "HELLO");

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert!(
        !attached
            .events
            .iter()
            .any(|event| event.kind == EventKind::PatchProposed),
        "default writes apply without staging"
    );
    let applied = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::PatchApplied)
        .expect("auto patch event");
    assert_eq!(applied.payload["path"], "out.txt");
    let diff = applied.payload["diff"].as_str().expect("auto patch diff");
    assert!(diff.contains("+++ b/out.txt"), "diff was: {diff}");
    assert!(diff.contains("+HELLO"), "diff was: {diff}");
    assert!(!diff.contains("+hello"), "diff was: {diff}");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn plan_mode_refuses_writes_and_mode_is_logged() -> anyhow::Result<()> {
    let dir = temp_dir("read-only");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("out.txt"), "before")?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(tool_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;

    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "read-only", "cwd": dir, "mode": "plan"}),
        )
        .await?;
    let session_id = created.session.id.clone();
    assert_eq!(created.session.mode, SessionMode::Plan);

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
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, "before");
    // Plan mode keeps the Build tool catalog so the provider cache survives
    // mode switches. Mutating calls are denied by policy, not by hiding
    // schemas.
    let first: Value = serde_json::from_str(&bodies.lock().await[0])?;
    assert!(
        first["tools"].as_array().is_some_and(|tools| tools
            .iter()
            .any(|tool| tool["function"]["name"] == "apply_patch")),
        "read-only tools were: {}",
        first["tools"]
    );
    assert!(
        first["tools"].as_array().is_some_and(|tools| tools
            .iter()
            .any(|tool| tool["function"]["name"] == "exec_command")),
        "exec_command should remain available for read-only inspection"
    );

    // Persisted change: row + mode event.
    let changed: SessionResult = client
        .request(
            "session.mode",
            json!({"session_id": session_id, "mode": "build"}),
        )
        .await?;
    assert_eq!(changed.session.mode, SessionMode::Build);
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let event = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::Mode)
        .expect("mode event");
    assert_eq!(event.payload["from"], "plan");
    assert_eq!(event.payload["to"], "build");
    assert_eq!(event.payload["temporary"], false);

    // Temporary override applies immediately and is visible on reads...
    let temporary: SessionResult = client
        .request(
            "session.mode",
            json!({
                "session_id": session_id,
                "mode": "full",
                "temporary": true,
            }),
        )
        .await?;
    assert_eq!(temporary.session.mode, SessionMode::Full);
    let listed: eix_core::SessionList = client.request("session.list", Value::Null).await?;
    let row = listed
        .sessions
        .iter()
        .find(|session| session.id == session_id)
        .expect("session row");
    assert_eq!(row.mode, SessionMode::Full);
    // ...and is announced as temporary in the event log.
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let temp_event = attached
        .events
        .iter()
        .rev()
        .find(|event| event.kind == EventKind::Mode)
        .expect("temporary mode event");
    assert_eq!(temp_event.payload["to"], "full");
    assert_eq!(temp_event.payload["temporary"], true);

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
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
    assert_eq!(approval["tool"], "exec_command");
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
    assert_eq!(snapshot.payload["tool"], "exec_command");
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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
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
            && event.payload["tool"] == "exec_command"
            && event.payload["ok"] == true
    }));
    assert!(
        !attached.events.iter().any(|event| {
            event.kind == EventKind::Snapshot && event.payload["tool"] == "exec_command"
        }),
        "read-only commands must not capture a workspace snapshot"
    );
    let write_call = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::ToolCall && event.payload["tool"] == "apply_patch")
        .expect("apply_patch call");
    assert!(
        write_call.payload["arguments"]["patchText"]
            .as_str()
            .unwrap_or_default()
            .contains("+hunter2"),
        "arguments were: {}",
        write_call.payload["arguments"]
    );

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.tools.sandbox = eix_core::SandboxMode::Off;
    config.tools.rules = vec![PermissionRule {
        tool: "exec_command".to_string(),
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
    assert_eq!(snapshot.payload["tool"], "exec_command");
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
async fn auto_classifier_allows_and_caches_safe_tool_calls() -> anyhow::Result<()> {
    let dir = temp_dir("classifier-auto");
    std::fs::create_dir_all(&dir)?;
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "eix@example.test"]);
    git(&["config", "user.name", "eix"]);

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(classifier_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.tools.sandbox = eix_core::SandboxMode::Off;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "classifier", "cwd": dir, "mode": "auto"}),
        )
        .await?;
    let session_id = created.session.id.clone();
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "commit the current work"}),
        )
        .await?;

    let mut prompted = false;
    let completed = loop {
        let notification = client.next_notification().await?;
        if notification.method == "tool.approval_required" {
            prompted = true;
        }
        if notification.method == "turn.completed"
            && notification.params["session_id"] == session_id.as_str()
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            break notification.params.clone();
        }
    };
    assert!(!prompted, "classifier should avoid the approval prompt");
    assert!(completed["error"].is_null(), "{completed}");

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let permissions: Vec<_> = attached
        .events
        .iter()
        .filter(|event| event.kind == EventKind::PermissionEvaluated)
        .filter_map(|event| event.payload.get("classifier"))
        .collect();
    assert_eq!(permissions.len(), 2, "{permissions:?}");
    assert_eq!(permissions[0]["decision"], "allow");
    assert_eq!(permissions[0]["cached"], false);
    assert_eq!(permissions[1]["decision"], "allow");
    assert_eq!(permissions[1]["cached"], true);

    let results: Vec<_> = attached
        .events
        .iter()
        .filter(|event| event.kind == EventKind::ToolResult)
        .collect();
    assert_eq!(results.len(), 2, "{results:?}");
    assert!(results.iter().all(|event| event.payload["ok"] == true));

    let output = std::process::Command::new("git")
        .args(["log", "--format=%s"])
        .current_dir(&dir)
        .output()?;
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("auto-classified"),
        "the classifier-approved commit did not run"
    );

    let bodies = bodies.lock().await;
    let classifier_requests = bodies
        .iter()
        .filter(|body| body.contains("eix's execution classifier"))
        .count();
    assert_eq!(
        classifier_requests, 1,
        "the cached second call re-ran the classifier"
    );

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn auto_classifier_invalid_output_falls_back_to_approval() -> anyhow::Result<()> {
    let dir = temp_dir("classifier-fallback");
    std::fs::create_dir_all(&dir)?;
    let git = |args: &[&str]| {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .status()
            .expect("run git");
        assert!(status.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "eix@example.test"]);
    git(&["config", "user.name", "eix"]);

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(invalid_classifier_provider(listener));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.tools.sandbox = eix_core::SandboxMode::Off;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "classifier-fallback", "cwd": dir, "mode": "auto"}),
        )
        .await?;
    let session_id = created.session.id.clone();
    let _: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "commit the current work"}),
        )
        .await?;

    let approval = loop {
        let notification = client.next_notification().await?;
        if notification.method == "tool.approval_required" {
            break notification.params.clone();
        }
    };
    assert_eq!(approval["tool"], "exec_command");
    assert!(
        approval["reasons"]
            .as_array()
            .is_some_and(|reasons| reasons.iter().any(|reason| reason
                .as_str()
                .is_some_and(|reason| reason.contains("classifier returned no JSON")))),
        "approval should explain the classifier fallback: {approval}"
    );
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

    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed" {
            assert!(notification.params["error"].is_null());
            break;
        }
    }

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let classifier = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::PermissionEvaluated)
        .and_then(|event| event.payload.get("classifier"))
        .expect("classifier audit");
    assert_eq!(classifier["decision"], "ask");
    assert!(classifier["reason"]
        .as_str()
        .unwrap()
        .contains("no JSON verdict"));

    let output = std::process::Command::new("git")
        .args(["log", "--format=%s"])
        .current_dir(&dir)
        .output()?;
    assert!(String::from_utf8_lossy(&output.stdout).contains("fallback-approved"));

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
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
            json!({"name": "ro", "cwd": dir, "mode": "plan"}),
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
async fn attention_runs_the_notification_command() -> anyhow::Result<()> {
    let dir = temp_dir("notify");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("out.txt"), "before")?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(tool_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.tools.auto_apply_workspace_edits = false;
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
async fn background_process_streams_output_and_can_be_killed() -> anyhow::Result<()> {
    let dir = temp_dir("process");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(start_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.tools.sandbox = eix_core::SandboxMode::Off;

    let socket = dir.join("eix.sock");
    let data_dir = dir.join("data");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), data_dir.clone()));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "process", "cwd": dir, "mode": "full"}),
        )
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
        let started = tools[0]["content"].as_str().unwrap();
        assert!(started.contains("e2e-process"), "output was: {started:?}");
        assert!(started.contains("still running"), "output was: {started:?}");
        assert!(
            started.contains("Check again in"),
            "the poll hint should schedule the next check: {started:?}"
        );
        let output = tools[1]["content"].as_str().unwrap();
        assert!(output.contains("still running"), "output was: {output:?}");
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
async fn transient_web_fetch_failure_is_retried() -> anyhow::Result<()> {
    let dir = temp_dir("tool-retry");
    std::fs::create_dir_all(&dir)?;

    // The first fetch gets a retryable 503; the second succeeds.
    let flaky = TcpListener::bind("127.0.0.1:0").await?;
    let flaky_addr = flaky.local_addr()?;
    let hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hits_server = Arc::clone(&hits);
    let flaky_task = tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = flaky.accept().await else {
                return;
            };
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            let hit = hits_server.fetch_add(1, Ordering::SeqCst);
            let (status, body) = if hit == 0 {
                ("503 Service Unavailable", "busy")
            } else {
                ("200 OK", "recovered")
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let url = format!("http://{flaky_addr}/data");
    let provider = tokio::spawn({
        let bodies = Arc::clone(&bodies);
        async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
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
                    let arguments = json!({"url": url}).to_string();
                    let chunk = json!({
                        "model": "mock-tools",
                        "choices": [{
                            "delta": {
                                "tool_calls": [{
                                    "index": 0,
                                    "id": "call_1",
                                    "type": "function",
                                    "function": {"name": "web_fetch", "arguments": arguments},
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
            }
        }
    });

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.tools.sandbox = eix_core::SandboxMode::Off;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "retry", "cwd": dir}))
        .await?;
    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": created.session.id, "text": "fetch it"}),
        )
        .await?;
    assert_eq!(wait_turn(&mut client).await?, "done");
    assert_eq!(
        hits.load(Ordering::SeqCst),
        2,
        "the transient failure should be retried exactly once"
    );
    let saw_recovered = bodies
        .lock()
        .await
        .iter()
        .any(|body| body.contains("recovered"));
    assert!(
        saw_recovered,
        "the successful retry must reach the model as the tool result"
    );

    daemon.abort();
    provider.abort();
    flaky_task.abort();
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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

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
async fn recovers_sessions_left_running() -> anyhow::Result<()> {
    let dir = temp_dir("recover");
    let data_dir = dir.join("data");
    std::fs::create_dir_all(&data_dir)?;

    {
        let store = eixd::store::Store::open(&data_dir.join("eix.sqlite3"))?;
        let session = eix_core::Session::new("stale", "stale session", "/tmp");
        store.create_session(&session)?;
        store.set_status("stale", eix_core::SessionStatus::Running)?;
        let waiting = eix_core::Session::new("waiting", "waiting session", "/tmp");
        store.create_session(&waiting)?;
        store.set_status("waiting", eix_core::SessionStatus::AwaitingApproval)?;
    }

    let mut config = Config::default();
    config.default_provider_mut().base_url = "http://127.0.0.1:1/v1".to_string();
    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), data_dir));

    let mut client = connect(&socket).await?;
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": "stale"}))
        .await?;
    assert_eq!(attached.session.status, eix_core::SessionStatus::Paused);
    let error = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::Error)
        .expect("interrupted session error event");
    // A turn that was actually running can be continued by the TUI, and the
    // paused status lets a fresh client discover it from the session list.
    assert_eq!(error.payload["interrupted"], true);
    assert!(attached
        .events
        .iter()
        .any(|e| { e.kind == EventKind::StatusChanged && e.payload["to"] == "paused" }));

    // A turn waiting on the user cannot be continued without bypassing that
    // decision, so it gets a plain (non-resumable) error marker.
    let waiting: AttachResult = client
        .request("session.attach", json!({"session_id": "waiting"}))
        .await?;
    assert_eq!(waiting.session.status, eix_core::SessionStatus::Error);
    let error = waiting
        .events
        .iter()
        .find(|event| event.kind == EventKind::Error)
        .expect("waiting session error event");
    assert_eq!(error.payload["interrupted"], false);

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    // Build applies low/medium in-workspace edits immediately, so the snapshot
    // and the write land in one turn.

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
    let output = std::process::Command::new("git")
        .args(["-c", "user.email=eix@example.com", "-c", "user.name=eix"])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("run git");
    // `-c` only covers this invocation, but the daemon shells out to git on its
    // own (`worktree::commit_all`), so the identity has to be persisted into the
    // repo config for those commits to succeed. Worktrees share the repo config.
    if args.first() == Some(&"init") && output.status.success() {
        for (key, value) in [("user.email", "eix@example.com"), ("user.name", "eix")] {
            std::process::Command::new("git")
                .args(["config", key, value])
                .current_dir(dir)
                .output()
                .expect("git config");
        }
    }
    output
}

fn git_stdout(dir: &Path, args: &[&str]) -> String {
    let output = git(dir, args);
    assert!(
        output.status.success(),
        "git {}: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
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
    config.worktree.auto = true;
    config.default_provider_mut().base_url = "http://127.0.0.1:1/v1".to_string();
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

#[tokio::test]
async fn worktree_create_moves_an_existing_session_once() -> anyhow::Result<()> {
    let dir = temp_dir("worktree-attach");
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
    let base = git_stdout(&dir, &["symbolic-ref", "--short", "HEAD"]);

    let mut config = Config::default();
    config.default_provider_mut().base_url = "http://127.0.0.1:1/v1".to_string();
    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "shared", "cwd": dir}))
        .await?;
    assert!(!created.session.has_worktree());

    let moved: SessionResult = client
        .request(
            "session.worktree.create",
            json!({"session_id": created.session.id, "name": "moved"}),
        )
        .await?;
    let worktree = moved.session.worktree.clone().expect("worktree path");
    assert_eq!(moved.session.id, created.session.id);
    assert_eq!(moved.session.name, "shared");
    assert_eq!(moved.session.cwd, worktree);
    assert!(worktree.starts_with(repo.join(".worktrees")));
    assert!(worktree.is_dir());

    let integrations: eix_core::IntegrationListResult = client
        .request(
            "integration.list",
            json!({"session_id": created.session.id}),
        )
        .await?;
    let state = integrations
        .integrations
        .first()
        .expect("integration state");
    assert_eq!(state.base_branch.as_deref(), Some(base.as_str()));
    assert!(!state.worktree_disabled);

    // The daemon rejects a second worktree instead of nesting one inside the
    // first; the TUI surfaces the same condition as a top-right warning.
    assert_eq!(
        client
            .request_error(
                "session.worktree.create",
                json!({"session_id": created.session.id}),
            )
            .await?,
        "invalid_params"
    );
    assert_eq!(std::fs::read_dir(repo.join(".worktrees"))?.count(), 1);

    let got: SessionResult = client
        .request("session.get", json!({"session_id": created.session.id}))
        .await?;
    assert_eq!(got.session.cwd, worktree);

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn integration_rebases_tests_and_fast_forwards_the_base_branch() -> anyhow::Result<()> {
    let root = temp_dir("integration-queue");
    let dir = root.join("repo");
    std::fs::create_dir_all(&dir)?;
    let init = git(&dir, &["init", "-q", "-b", "main"]);
    assert!(
        init.status.success(),
        "git init: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    std::fs::write(dir.join("README.md"), "base\n")?;
    assert!(git(&dir, &["add", "README.md"]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "init"]).status.success());
    let remote = root.join("remote.git");
    assert!(
        git(&root, &["init", "--bare", "-q", remote.to_str().unwrap()])
            .status
            .success()
    );
    assert!(
        git(&dir, &["remote", "add", "origin", remote.to_str().unwrap()])
            .status
            .success()
    );
    let push = git(&dir, &["push", "-u", "origin", "main"]);
    assert!(
        push.status.success(),
        "git push: {}",
        String::from_utf8_lossy(&push.stderr)
    );
    let repo = dir.canonicalize()?;

    let mut config = Config::default();
    config.worktree.auto = false;
    config.worktree.auto = true;
    config.worktree.base = "main".into();
    config.integration.auto_ready = false;
    config.integration.pull_base = true;
    config.integration.test = "echo unit-tests-passed".into();
    config.integration.timeout_secs = 30;
    let socket = root.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), root.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "feature", "cwd": dir, "worktree": true}),
        )
        .await?;
    let session = created.session;
    let worktree = session.worktree.clone().expect("worktree");
    // Leave the change uncommitted: ready should commit it automatically with
    // the default integration.auto_commit = true.
    std::fs::write(worktree.join("feature.txt"), "from worktree\n")?;

    let ready: eix_core::IntegrationState = client
        .request("session.ready", json!({"session_id": session.id}))
        .await?;
    assert_eq!(ready.status, eix_core::IntegrationStatus::Ready);

    let mut merged = false;
    for _ in 0..40 {
        let list: eix_core::IntegrationListResult = client
            .request("integration.list", json!({"session_id": session.id}))
            .await?;
        let state = list.integrations.first().expect("integration state");
        if state.status == eix_core::IntegrationStatus::Merged {
            merged = true;
            break;
        }
        if state.status == eix_core::IntegrationStatus::Retrying {
            anyhow::bail!("integration retried: {:?}", state.error);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(merged, "integration did not merge");
    assert_eq!(
        std::fs::read_to_string(repo.join("feature.txt"))?,
        "from worktree\n"
    );
    assert!(worktree.is_dir());

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session.id}))
        .await?;
    let notes: Vec<_> = attached
        .events
        .iter()
        .filter(|event| event.kind == EventKind::Integration)
        .filter_map(|event| event.payload.get("text").and_then(|text| text.as_str()))
        .collect();
    assert!(
        notes.iter().any(|note| note.contains("queued")),
        "missing queued note: {notes:?}"
    );
    assert!(
        notes.iter().any(|note| note.contains("merged")),
        "missing merged note: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|note| note.contains("$ git fetch --prune origin")),
        "missing fetch command: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|note| note.contains("$ git pull --ff-only")),
        "missing pull command: {notes:?}"
    );
    assert!(
        notes.iter().any(|note| note.contains("$ git rebase main")),
        "missing rebase command: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|note| note.contains("$ echo unit-tests-passed")
                && note.contains("unit-tests-passed")),
        "missing test command/output: {notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|note| note.contains("$ git merge --ff-only")),
        "missing merge command: {notes:?}"
    );

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&root).await;
    Ok(())
}

#[tokio::test]
async fn ready_recovers_a_stale_rebase_and_records_a_backup() -> anyhow::Result<()> {
    let root = temp_dir("integration-stale-rebase");
    let dir = root.join("repo");
    std::fs::create_dir_all(&dir)?;
    assert!(git(&dir, &["init", "-q", "-b", "main"]).status.success());
    std::fs::write(dir.join("file.txt"), "base\n")?;
    assert!(git(&dir, &["add", "file.txt"]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "base"]).status.success());

    let mut config = Config::default();
    config.worktree.auto = false;
    config.worktree.base = "main".into();
    config.integration.auto_ready = false;
    config.integration.pull_base = false;
    config.integration.conflict_repair = false;
    config.integration.pause_after = 1;
    config.integration.test = "true".into();
    config.integration.timeout_secs = 30;
    let socket = root.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), root.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "stale", "cwd": dir, "worktree": true}),
        )
        .await?;
    let session = created.session;
    let worktree = session.worktree.clone().expect("worktree");

    std::fs::write(worktree.join("file.txt"), "feature\n")?;
    assert!(git(&worktree, &["add", "file.txt"]).status.success());
    assert!(git(&worktree, &["commit", "-q", "-m", "feature"])
        .status
        .success());

    std::fs::write(dir.join("file.txt"), "main\n")?;
    assert!(git(&dir, &["add", "file.txt"]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "main"]).status.success());

    let rebase = git(&worktree, &["rebase", "main"]);
    assert!(
        !rebase.status.success(),
        "rebase should conflict: {}",
        String::from_utf8_lossy(&rebase.stderr)
    );
    assert!(!git(&worktree, &["ls-files", "-u"]).stdout.is_empty());

    // A user/CLI ready must not be blocked by the stale rebase. It should
    // preserve it and queue the worktree.
    let ready: eix_core::IntegrationState = client
        .request("session.ready", json!({"session_id": session.id}))
        .await?;
    assert_eq!(ready.status, eix_core::IntegrationStatus::Ready);

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session.id}))
        .await?;
    let notes: Vec<_> = attached
        .events
        .iter()
        .filter(|event| event.kind == EventKind::Integration)
        .filter_map(|event| event.payload.get("text").and_then(|text| text.as_str()))
        .collect();
    assert!(
        notes
            .iter()
            .any(|note| note.contains("integration recovered")),
        "missing recovery note: {notes:?}"
    );
    assert!(
        notes.iter().any(|note| note.contains("backup:")),
        "missing recovery backup: {notes:?}"
    );

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&root).await;
    Ok(())
}

#[tokio::test]
async fn integration_merges_into_the_branch_the_session_started_from() -> anyhow::Result<()> {
    let root = temp_dir("integration-feature-base");
    let dir = root.join("repo");
    std::fs::create_dir_all(&dir)?;
    assert!(git(&dir, &["init", "-q", "-b", "main"]).status.success());
    std::fs::write(dir.join("README.md"), "base\n")?;
    assert!(git(&dir, &["add", "README.md"]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "init"]).status.success());
    assert!(git(&dir, &["checkout", "-q", "-b", "feature"])
        .status
        .success());
    std::fs::write(dir.join("feature-base.txt"), "feature\n")?;
    assert!(git(&dir, &["add", "feature-base.txt"]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "feature base"])
        .status
        .success());

    let main_before = git_stdout(&dir, &["rev-parse", "main"]);
    let feature_before = git_stdout(&dir, &["rev-parse", "feature"]);
    let repo = dir.canonicalize()?;

    let mut config = Config::default();
    config.worktree.auto = false;
    config.worktree.auto = true;
    config.worktree.base = "auto".into();
    config.integration.auto_ready = false;
    config.integration.pull_base = false;
    config.integration.test = "true".into();
    config.integration.timeout_secs = 30;
    let socket = root.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), root.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "feature-work", "cwd": dir, "worktree": true}),
        )
        .await?;
    let session = created.session;
    let worktree = session.worktree.clone().expect("worktree");
    assert_eq!(
        git_stdout(&worktree, &["rev-parse", "HEAD"]),
        feature_before,
        "worktree should start from the current feature branch"
    );

    let list: eix_core::IntegrationListResult = client
        .request("integration.list", json!({"session_id": session.id}))
        .await?;
    let state = list.integrations.first().expect("integration state");
    assert_eq!(state.base_branch.as_deref(), Some("feature"));

    std::fs::write(worktree.join("session.txt"), "session\n")?;
    let ready: eix_core::IntegrationState = client
        .request("session.ready", json!({"session_id": session.id}))
        .await?;
    assert_eq!(ready.status, eix_core::IntegrationStatus::Ready);

    let mut merged = false;
    for _ in 0..40 {
        let list: eix_core::IntegrationListResult = client
            .request("integration.list", json!({"session_id": session.id}))
            .await?;
        let state = list.integrations.first().expect("integration state");
        if state.status == eix_core::IntegrationStatus::Merged {
            merged = true;
            break;
        }
        if state.status == eix_core::IntegrationStatus::Retrying {
            anyhow::bail!("integration retried: {:?}", state.error);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(merged, "integration did not merge");

    let session_head = git_stdout(&worktree, &["rev-parse", "HEAD"]);
    assert_eq!(git_stdout(&repo, &["rev-parse", "feature"]), session_head);
    assert_eq!(git_stdout(&repo, &["rev-parse", "main"]), main_before);
    assert_ne!(git_stdout(&repo, &["rev-parse", "feature"]), feature_before);
    assert_eq!(
        std::fs::read_to_string(repo.join("session.txt"))?,
        "session\n"
    );
    assert_eq!(
        std::fs::read_to_string(repo.join("feature-base.txt"))?,
        "feature\n"
    );

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&root).await;
    Ok(())
}

#[tokio::test]
async fn integration_merges_into_a_linked_starting_worktree() -> anyhow::Result<()> {
    let root = temp_dir("integration-linked-base");
    let dir = root.join("repo");
    std::fs::create_dir_all(&dir)?;
    assert!(git(&dir, &["init", "-q", "-b", "main"]).status.success());
    std::fs::write(dir.join("README.md"), "base\n")?;
    assert!(git(&dir, &["add", "README.md"]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "init"]).status.success());

    let feature = root.join("feature-wt");
    let add = git(
        &dir,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature",
            feature.to_str().expect("feature path"),
        ],
    );
    assert!(
        add.status.success(),
        "git worktree add: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    std::fs::write(feature.join("feature-base.txt"), "feature\n")?;
    assert!(git(&feature, &["add", "feature-base.txt"]).status.success());
    assert!(git(&feature, &["commit", "-q", "-m", "feature base"])
        .status
        .success());

    let main_before = git_stdout(&dir, &["rev-parse", "main"]);
    let repo = dir.canonicalize()?;
    let feature = feature.canonicalize()?;

    let mut config = Config::default();
    config.worktree.auto = false;
    config.worktree.base = "auto".into();
    config.integration.auto_ready = false;
    config.integration.pull_base = false;
    config.integration.test = "true".into();
    config.integration.timeout_secs = 30;
    let socket = root.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), root.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "linked-work", "cwd": feature, "worktree": true}),
        )
        .await?;
    let session = created.session;
    let worktree = session.worktree.clone().expect("worktree");
    let list: eix_core::IntegrationListResult = client
        .request("integration.list", json!({"session_id": session.id}))
        .await?;
    let state = list.integrations.first().expect("integration state");
    assert_eq!(state.base_branch.as_deref(), Some("feature"));

    std::fs::write(worktree.join("session.txt"), "session\n")?;
    let ready: eix_core::IntegrationState = client
        .request("session.ready", json!({"session_id": session.id}))
        .await?;
    assert_eq!(ready.status, eix_core::IntegrationStatus::Ready);

    let mut merged = false;
    for _ in 0..40 {
        let list: eix_core::IntegrationListResult = client
            .request("integration.list", json!({"session_id": session.id}))
            .await?;
        let state = list.integrations.first().expect("integration state");
        if state.status == eix_core::IntegrationStatus::Merged {
            merged = true;
            break;
        }
        if state.status == eix_core::IntegrationStatus::Retrying {
            anyhow::bail!("integration retried: {:?}", state.error);
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(merged, "integration did not merge");

    let session_head = git_stdout(&worktree, &["rev-parse", "HEAD"]);
    assert_eq!(
        git_stdout(&feature, &["rev-parse", "feature"]),
        session_head
    );
    assert_eq!(git_stdout(&repo, &["rev-parse", "main"]), main_before);
    assert_eq!(
        std::fs::read_to_string(feature.join("session.txt"))?,
        "session\n"
    );
    assert!(!repo.join("session.txt").exists());

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&root).await;
    Ok(())
}

#[tokio::test]
async fn integration_missing_test_command_hands_off_to_the_agent() -> anyhow::Result<()> {
    let root = temp_dir("integration-no-test");
    let dir = root.join("repo");
    std::fs::create_dir_all(&dir)?;
    assert!(git(&dir, &["init", "-q", "-b", "main"]).status.success());
    std::fs::write(dir.join("README.md"), "base\n")?;
    assert!(git(&dir, &["add", "README.md"]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "init"]).status.success());

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.worktree.auto = false;
    config.worktree.base = "main".into();
    config.integration.auto_ready = false;
    config.integration.pull_base = false;
    config.integration.test = "auto".into();
    config.integration.conflict_repair = true;
    config.integration.notify_after = 0;
    config.integration.timeout_secs = 30;
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    let socket = root.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), root.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "no-test", "cwd": dir, "worktree": true}),
        )
        .await?;
    let session = created.session;
    let worktree = session.worktree.clone().expect("worktree");
    std::fs::write(worktree.join("change.txt"), "change\n")?;

    let ready: eix_core::IntegrationState = client
        .request("session.ready", json!({"session_id": session.id}))
        .await?;
    assert_eq!(ready.status, eix_core::IntegrationStatus::Ready);

    let mut repairing = None;
    for _ in 0..60 {
        let list: eix_core::IntegrationListResult = client
            .request("integration.list", json!({"session_id": session.id}))
            .await?;
        let state = list.integrations.first().expect("integration state");
        if state.status == eix_core::IntegrationStatus::Repairing {
            repairing = Some(state.clone());
            break;
        }
        if state.status == eix_core::IntegrationStatus::Retrying {
            anyhow::bail!(
                "missing test command should hand off, not retry: {:?}",
                state.error
            );
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let repairing = repairing.expect("integration should ask the agent to repair");
    assert!(
        repairing
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("test command"),
        "unexpected error: {:?}",
        repairing.error
    );

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session.id}))
        .await?;
    assert!(
        attached.events.iter().any(|event| {
            event.kind == EventKind::UserMessage
                && event
                    .payload
                    .get("text")
                    .and_then(|text| text.as_str())
                    .is_some_and(|text| text.contains("no test command configured"))
        }),
        "the repair prompt was not injected: {:?}",
        attached.events
    );
    assert!(
        attached.events.iter().any(|event| {
            event.kind == EventKind::Integration
                && event
                    .payload
                    .get("status")
                    .and_then(|status| status.as_str())
                    == Some("repairing")
        }),
        "missing repairing note: {:?}",
        attached.events
    );

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&root).await;
    Ok(())
}

#[tokio::test]
async fn integration_pending_repair_resumes_when_idle() -> anyhow::Result<()> {
    let root = temp_dir("integration-resume-repair");
    let dir = root.join("repo");
    std::fs::create_dir_all(&dir)?;
    assert!(git(&dir, &["init", "-q", "-b", "main"]).status.success());
    std::fs::write(dir.join(".gitignore"), ".worktrees/\n")?;
    assert!(git(&dir, &["add", ".gitignore"]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "init"]).status.success());

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.worktree.auto = false;
    config.worktree.base = "main".into();
    config.integration.auto_ready = false;
    config.integration.pull_base = false;
    config.integration.test = "auto".into();
    config.integration.conflict_repair = true;
    config.integration.notify_after = 0;
    config.integration.timeout_secs = 30;
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    let data = root.join("data");
    let socket = root.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), data.clone()));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "resume-repair", "cwd": dir, "worktree": true}),
        )
        .await?;
    let session = created.session;

    // Simulate a deterministic failure that arrived while the agent's turn
    // was still running: the queue records a pending marker instead of
    // injecting a second turn. The idle scanner must resume it.
    {
        let store = eixd::store::Store::open(&data.join("eix.sqlite3"))?;
        let mut state = store
            .get_integration(&session.id)?
            .expect("integration state");
        state.status = eix_core::IntegrationStatus::Repairing;
        state.stage = Some("repair pending: test command missing".into());
        state.error = Some(format!(
            "no test command configured for {}",
            session.cwd.display()
        ));
        state.updated_at = eix_core::now_ms();
        store.save_integration(&state)?;
    }

    let mut saw_prompt = false;
    for _ in 0..60 {
        let attached: AttachResult = client
            .request("session.attach", json!({"session_id": session.id}))
            .await?;
        if attached.events.iter().any(|event| {
            event.kind == EventKind::UserMessage
                && event
                    .payload
                    .get("text")
                    .and_then(|text| text.as_str())
                    .is_some_and(|text| text.contains("no test command configured"))
        }) {
            saw_prompt = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(saw_prompt, "pending repair prompt was never resumed");

    let list: eix_core::IntegrationListResult = client
        .request("integration.list", json!({"session_id": session.id}))
        .await?;
    let state = list.integrations.first().expect("integration state");
    assert_eq!(state.status, eix_core::IntegrationStatus::Repairing);
    assert_eq!(state.stage.as_deref(), Some("test command missing"));

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&root).await;
    Ok(())
}

#[tokio::test]
async fn integration_failure_retries_and_continue_requeues() -> anyhow::Result<()> {
    let root = temp_dir("integration-retry");
    let dir = root.join("repo");
    std::fs::create_dir_all(&dir)?;
    let init = git(&dir, &["init", "-q", "-b", "main"]);
    assert!(init.status.success());
    std::fs::write(dir.join(".gitignore"), ".worktrees/\n")?;
    assert!(git(&dir, &["add", ".gitignore"]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "init"]).status.success());

    let mut config = Config::default();
    config.worktree.auto = false;
    config.worktree.base = "main".into();
    config.integration.auto_ready = false;
    config.integration.pull_base = false;
    config.integration.test = "false".into();
    config.integration.timeout_secs = 30;
    config.integration.conflict_repair = false;
    config.integration.pause_after = 1;
    config.integration.notify_after = 0;
    let socket = root.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), root.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "broken", "cwd": dir, "worktree": true}),
        )
        .await?;
    let session = created.session;
    let worktree = session.worktree.clone().expect("worktree");
    std::fs::write(worktree.join("change.txt"), "change\n")?;
    assert!(git(&worktree, &["add", "change.txt"]).status.success());
    assert!(git(&worktree, &["commit", "-q", "-m", "change"])
        .status
        .success());

    let ready: eix_core::IntegrationState = client
        .request("session.ready", json!({"session_id": session.id}))
        .await?;
    assert_eq!(ready.status, eix_core::IntegrationStatus::Ready);

    let mut failed = None;
    for _ in 0..60 {
        let list: eix_core::IntegrationListResult = client
            .request("integration.list", json!({"session_id": session.id}))
            .await?;
        let state = list.integrations.first().expect("integration state");
        if state.status == eix_core::IntegrationStatus::Retrying && state.attempts > 0 {
            failed = Some(state.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let failed = failed.expect("integration should retry after a failing test");
    assert!(
        failed.next_retry_at.is_some(),
        "paused retry has a due time"
    );
    assert!(
        failed.error.as_deref().unwrap_or_default().contains("test"),
        "unexpected error: {:?}",
        failed.error
    );
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session.id}))
        .await?;
    assert!(
        attached.events.iter().any(|event| {
            event.kind == EventKind::Integration
                && event
                    .payload
                    .get("status")
                    .and_then(|status| status.as_str())
                    == Some("retrying")
        }),
        "missing retrying note: {:?}",
        attached.events
    );

    let continued: eix_core::IntegrationState = client
        .request("integration.continue", json!({"session_id": session.id}))
        .await?;
    assert_eq!(continued.status, eix_core::IntegrationStatus::Ready);
    assert_eq!(continued.attempts, 0);
    assert!(continued.next_retry_at.is_none());
    assert!(continued.error.is_none());

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&root).await;
    Ok(())
}

#[tokio::test]
async fn integration_conflict_retries_when_repair_is_disabled() -> anyhow::Result<()> {
    let root = temp_dir("integration-conflict");
    let dir = root.join("repo");
    std::fs::create_dir_all(&dir)?;
    assert!(git(&dir, &["init", "-q", "-b", "main"]).status.success());
    std::fs::write(dir.join("shared.txt"), "base\n")?;
    assert!(git(&dir, &["add", "shared.txt"]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "init"]).status.success());

    let mut config = Config::default();
    config.worktree.auto = false;
    config.worktree.base = "main".into();
    config.integration.auto_ready = false;
    config.integration.pull_base = false;
    config.integration.test = "true".into();
    config.integration.conflict_repair = false;
    let socket = root.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), root.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "conflict", "cwd": dir, "worktree": true}),
        )
        .await?;
    let session = created.session;
    let worktree = session.worktree.clone().expect("worktree");
    std::fs::write(worktree.join("shared.txt"), "feature\n")?;
    assert!(git(&worktree, &["add", "shared.txt"]).status.success());
    assert!(git(&worktree, &["commit", "-q", "-m", "feature"])
        .status
        .success());
    // Move main so the rebase must conflict.
    std::fs::write(dir.join("shared.txt"), "base changed\n")?;
    assert!(git(&dir, &["add", "shared.txt"]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "base change"])
        .status
        .success());

    let ready: eix_core::IntegrationState = client
        .request("session.ready", json!({"session_id": session.id}))
        .await?;
    assert_eq!(ready.status, eix_core::IntegrationStatus::Ready);

    let mut failed = None;
    for _ in 0..60 {
        let list: eix_core::IntegrationListResult = client
            .request("integration.list", json!({"session_id": session.id}))
            .await?;
        let state = list.integrations.first().expect("integration state");
        if state.status == eix_core::IntegrationStatus::Retrying && state.attempts > 0 {
            failed = Some(state.clone());
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let failed = failed.expect("integration should retry after a rebase conflict");
    assert!(
        failed
            .error
            .as_deref()
            .unwrap_or_default()
            .to_lowercase()
            .contains("conflict"),
        "unexpected error: {:?}",
        failed.error
    );

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&root).await;
    Ok(())
}

#[tokio::test]
async fn first_prompt_creates_an_english_named_worktree() -> anyhow::Result<()> {
    let root = temp_dir("auto-worktree");
    let dir = root.join("repo");
    std::fs::create_dir_all(&dir)?;
    let init = git(&dir, &["init", "-q", "-b", "main"]);
    assert!(init.status.success());
    std::fs::write(dir.join(".gitignore"), ".worktrees/\n")?;
    assert!(git(&dir, &["add", ".gitignore"]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "init"]).status.success());

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let mock = tokio::spawn(mock_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.worktree.auto = true;
    config.worktree.base = "main".into();
    config.integration.auto_ready = false;
    let socket = root.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), root.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "auto", "cwd": dir}))
        .await?;
    assert!(!created.session.has_worktree());
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": created.session.id, "text": "fix the login bug"}),
        )
        .await?;
    let updated: SessionResult = client
        .request("session.get", json!({"session_id": created.session.id}))
        .await?;
    let worktree = updated.session.worktree.clone().expect("auto worktree");
    assert_eq!(
        worktree.file_name().and_then(|name| name.to_str()),
        Some("hello-from-mock")
    );
    assert!(worktree.is_dir());

    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            break;
        }
    }

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&root).await;
    Ok(())
}

#[tokio::test]
async fn lazy_worktree_uses_the_branch_from_session_creation() -> anyhow::Result<()> {
    let root = temp_dir("auto-worktree-branch");
    let dir = root.join("repo");
    std::fs::create_dir_all(&dir)?;
    assert!(git(&dir, &["init", "-q", "-b", "main"]).status.success());
    std::fs::write(dir.join(".gitignore"), ".worktrees/\n")?;
    assert!(git(&dir, &["add", ".gitignore"]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "init"]).status.success());
    assert!(git(&dir, &["checkout", "-q", "-b", "feature"])
        .status
        .success());
    std::fs::write(dir.join("feature.txt"), "feature\n")?;
    assert!(git(&dir, &["add", "feature.txt"]).status.success());
    assert!(git(&dir, &["commit", "-q", "-m", "feature"])
        .status
        .success());
    let feature_head = git_stdout(&dir, &["rev-parse", "HEAD"]);

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let mock = tokio::spawn(mock_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.worktree.auto = true;
    config.worktree.base = "auto".into();
    config.integration.auto_ready = false;
    let socket = root.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), root.join("data")));

    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "auto-branch", "cwd": dir}))
        .await?;
    assert!(!created.session.has_worktree());
    let list: eix_core::IntegrationListResult = client
        .request(
            "integration.list",
            json!({"session_id": created.session.id}),
        )
        .await?;
    let state = list.integrations.first().expect("integration state");
    assert_eq!(state.base_branch.as_deref(), Some("feature"));

    // Switch the starting checkout away before the lazy worktree is created.
    // The stored branch from session creation must still win.
    assert!(git(&dir, &["checkout", "-q", "main"]).status.success());
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": created.session.id, "text": "fix the login bug"}),
        )
        .await?;
    let updated: SessionResult = client
        .request("session.get", json!({"session_id": created.session.id}))
        .await?;
    let worktree = updated.session.worktree.clone().expect("auto worktree");
    assert_eq!(git_stdout(&worktree, &["rev-parse", "HEAD"]), feature_head);
    assert_eq!(
        std::fs::read_to_string(worktree.join("feature.txt"))?,
        "feature\n"
    );

    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["turn_id"] == submitted.turn_id.as_str()
        {
            break;
        }
    }

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&root).await;
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
    let probe = std::net::TcpListener::bind("127.0.0.1:0")?;
    let http_port = probe.local_addr()?.port();
    drop(probe);

    let mut config = Config::default();
    config.default_provider_mut().base_url = "http://127.0.0.1:1/v1".to_string();
    config.server.tcp = Some(addr.clone());
    config.server.http = Some(format!("127.0.0.1:{http_port}"));
    config.server.token = Some("s3cret".to_string());
    config.load_cwd = dir.clone();
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

    // A token minted by pairing works over TCP too, and revoking it is
    // immediate for the next connection.
    let base = format!("http://127.0.0.1:{http_port}");
    let http = reqwest::Client::new();
    let mut http_ready = false;
    for _ in 0..100 {
        if let Ok(response) = http.get(format!("{base}/health")).send().await {
            if response.status().is_success() {
                http_ready = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(http_ready, "http listener did not start");
    let pairing: Value = unix
        .request("pair.create", json!({"name": "tcp-laptop"}))
        .await?;
    let code = pairing["code"].as_str().expect("pairing code").to_string();
    let paired: Value = http
        .post(format!("{base}/pair"))
        .json(&json!({"code": code}))
        .send()
        .await?
        .json()
        .await?;
    let paired_token = paired["token"].as_str().expect("paired token").to_string();
    let mut raw = RawTcp::connect(&addr).await?;
    let auth = raw
        .send("auth", json!({"token": paired_token}))
        .await?
        .expect("response");
    assert_eq!(
        auth.result.expect("auth result")["authenticated"],
        true,
        "a paired token must authenticate over TCP"
    );
    let pong = raw.send("ping", Value::Null).await?.expect("response");
    assert_eq!(pong.result.expect("ping result")["pong"], true);

    let client_id = paired["client"]["id"]
        .as_str()
        .expect("client id")
        .to_string();
    let revoked: Value = unix
        .request("pair.revoke", json!({"id": client_id}))
        .await?;
    assert_eq!(revoked["revoked"], true);
    let mut raw = RawTcp::connect(&addr).await?;
    let denied = raw
        .send("auth", json!({"token": paired_token}))
        .await?
        .expect("response");
    assert_eq!(denied.error.expect("revoked token").code, "unauthorized");

    // The right token unlocks the normal protocol.
    let mut raw = RawTcp::connect(&addr).await?;
    let auth = raw
        .send("auth", json!({"token": "s3cret"}))
        .await?
        .expect("response");
    assert_eq!(auth.result.expect("auth result")["authenticated"], true);
    let pong = raw.send("ping", Value::Null).await?.expect("response");
    assert_eq!(pong.result.expect("ping result")["pong"], true);

    // The token does not grant local-only execution or a way to escape the
    // daemon's workspace through full mode or an absolute cwd.
    for (method, params) in [
        (
            "shell.run",
            json!({"session_id": "unknown", "command": "printf tcp-pwn"}),
        ),
        (
            "session.create",
            json!({"name": "full", "cwd": dir.clone(), "mode": "full"}),
        ),
        ("session.create", json!({"name": "escape", "cwd": "/"})),
    ] {
        let denied = raw.send(method, params).await?.expect("response");
        assert_eq!(
            denied.error.expect("forbidden error").code,
            "forbidden",
            "{method} was not denied"
        );
    }

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
async fn remote_worktree_cannot_escape_the_workspace() -> anyhow::Result<()> {
    let root = temp_dir("remote-worktree");
    let repo = root.join("repo");
    let workspace = repo.join("workspace");
    std::fs::create_dir_all(&workspace)?;
    assert!(git(&repo, &["init", "-q", "-b", "main"]).status.success());
    std::fs::write(repo.join("README.md"), "base\n")?;
    assert!(git(&repo, &["add", "README.md"]).status.success());
    assert!(git(&repo, &["commit", "-q", "-m", "init"]).status.success());

    let probe = std::net::TcpListener::bind("127.0.0.1:0")?;
    let addr = probe.local_addr()?.to_string();
    drop(probe);

    let mut config = Config::default();
    config.server.tcp = Some(addr.clone());
    config.server.token = Some("worktree-secret".to_string());
    config.load_cwd = workspace.clone();
    let socket = root.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket, root.join("data")));

    let mut raw = RawTcp::connect(&addr).await?;
    raw.send("auth", json!({"token": "worktree-secret"}))
        .await?
        .expect("response");
    // The workspace is a subdirectory of the repository, so a worktree would
    // be created outside the directory remote clients may use.
    let denied = raw
        .send(
            "session.create",
            json!({"name": "escape", "cwd": workspace, "worktree": true}),
        )
        .await?
        .expect("response");
    assert_eq!(denied.error.expect("forbidden error").code, "forbidden");

    let sessions: eix_core::SessionList = serde_json::from_value(
        raw.send("session.list", Value::Null)
            .await?
            .expect("response")
            .result
            .expect("session list"),
    )?;
    assert!(sessions.sessions.is_empty());

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&root).await;
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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

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
    assert!(output.starts_with("<subagent_result agent=\""), "{output}");
    assert!(output.contains("status=\"ok\""), "{output}");
    assert!(output.ends_with("</subagent_result>"), "{output}");
    assert!(output.contains("child answer"), "{output}");

    // The same batch also read the workspace, exercising the read + sub-agent
    // fast path.
    let listed = parent_events
        .events
        .iter()
        .find(|event| {
            event.kind == EventKind::ToolResult && event.payload["tool"] == "exec_command"
        })
        .expect("exec_command result");
    assert_eq!(listed.payload["ok"], true);

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn subagents_start_after_writes_and_overlap_trailing_reads() -> anyhow::Result<()> {
    let dir = temp_dir("agent-early");
    std::fs::create_dir_all(&dir)?;

    let slow = TcpListener::bind("127.0.0.1:0").await?;
    let slow_addr = slow.local_addr()?;
    let (read_started_tx, mut read_started_rx) = mpsc::channel(1);
    let (release_tx, release_rx) = oneshot::channel::<()>();
    let slow_http = tokio::spawn(async move {
        let Ok((mut socket, _)) = slow.accept().await else {
            return;
        };
        let mut buf = [0u8; 2048];
        let _ = socket.read(&mut buf).await;
        let _ = read_started_tx.send(()).await;
        let _ = release_rx.await;
        let body = "slow body";
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = socket.write_all(response.as_bytes()).await;
    });

    let child_started = Arc::new(AtomicBool::new(false));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(early_agent_provider(
        listener,
        format!("http://{slow_addr}/slow"),
        Arc::clone(&child_started),
    ));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-agent-early".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;
    config.context.auto_compact = false;

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
            json!({"session_id": parent, "text": "write, inspect, and verify"}),
        )
        .await?;

    let read_started = tokio::time::timeout(Duration::from_secs(10), read_started_rx.recv()).await;
    if !matches!(read_started, Ok(Some(()))) {
        let _ = release_tx.send(());
        daemon.abort();
        mock.abort();
        slow_http.abort();
        anyhow::bail!("the trailing read never started");
    }
    let child_ok = tokio::time::timeout(Duration::from_secs(5), async {
        while !child_started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    // Let the blocked read finish even when the assertion below will fail.
    let _ = release_tx.send(());

    let mut text = String::new();
    let completed = loop {
        let notification = client.next_notification().await?;
        match notification.method.as_str() {
            "message.delta" if notification.params["session_id"] == parent.as_str() => {
                if let Some(delta) = notification.params["delta"].as_str() {
                    text.push_str(delta);
                }
            }
            "turn.completed"
                if notification.params["session_id"] == parent.as_str()
                    && notification.params["turn_id"] == submitted.turn_id.as_str() =>
            {
                break notification.params.clone();
            }
            _ => {}
        }
    };

    assert!(completed["error"].is_null(), "{completed}");
    assert_eq!(text, "parent done");
    assert!(
        child_ok,
        "the child should start after the write and overlap the trailing read"
    );
    assert_eq!(std::fs::read_to_string(dir.join("early.txt"))?, "written\n");

    daemon.abort();
    mock.abort();
    slow_http.abort();
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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

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

    // The parent's `turn.completed` can be observed just before the child has
    // persisted its cancellation, so poll briefly instead of racing it.
    let mut attached = client
        .request::<AttachResult>("session.attach", json!({"session_id": child}))
        .await?;
    for _ in 0..100 {
        if attached
            .events
            .iter()
            .any(|event| event.kind == EventKind::AssistantMessage)
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        attached = client
            .request::<AttachResult>("session.attach", json!({"session_id": child}))
            .await?;
    }
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
async fn request_user_input_blocks_the_turn_until_the_user_answers() -> anyhow::Result<()> {
    let dir = temp_dir("ask");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(ask_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

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
                let questions = notification.params["questions"]
                    .as_array()
                    .expect("questions");
                assert_eq!(questions.len(), 2);
                assert_eq!(questions[0]["question"], "Which database should we use?");
                let options = questions[0]["options"].as_array().expect("options");
                assert_eq!(options.len(), 3);
                assert_eq!(options[0]["label"], "SQLite");
                assert_eq!(questions[1]["id"], "migration");
                let input_id = notification.params["input_id"].clone();
                // A partial answer is rejected and keeps the request pending.
                let partial: eix_core::InputResult = client
                    .request(
                        "input.respond",
                        json!({
                            "session_id": session_id,
                            "input_id": input_id,
                            "answers": { "database": ["Postgres"] },
                        }),
                    )
                    .await?;
                assert!(!partial.accepted);
                let result: eix_core::InputResult = client
                    .request(
                        "input.respond",
                        json!({
                            "session_id": session_id,
                            "input_id": input_id,
                            "answers": {
                                "database": ["Postgres"],
                                "migration": ["Yes"],
                            },
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
    assert_eq!(
        asked.payload["questions"][0]["question"],
        "Which database should we use?"
    );
    assert_eq!(
        asked.payload["questions"][0]["options"][2]["label"],
        "MySQL"
    );
    let answered = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::InputAnswered)
        .expect("answer event");
    assert_eq!(answered.payload["answers"]["database"][0], "Postgres");
    assert_eq!(answered.payload["answers"]["migration"][0], "Yes");
    let result = attached
        .events
        .iter()
        .find(|event| {
            event.kind == EventKind::ToolResult && event.payload["tool"] == "request_user_input"
        })
        .expect("request_user_input result");
    assert_eq!(result.payload["ok"], true);
    let output = result.payload["output"].as_str().expect("output");
    assert!(output.contains("User answered"), "{output}");
    assert!(output.contains("Postgres"), "{output}");
    assert!(output.contains("migration"), "{output}");

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
async fn picker_query_lists_workspace_files() -> anyhow::Result<()> {
    let dir = temp_dir("picker-files");
    let workspace = dir.join("workspace");
    std::fs::create_dir_all(workspace.join("src"))?;
    std::fs::create_dir_all(workspace.join(".git"))?;
    std::fs::create_dir_all(workspace.join("target/debug"))?;
    std::fs::write(workspace.join("src/main.rs"), "fn main() {}")?;
    std::fs::write(workspace.join("README.md"), "# eix")?;
    std::fs::write(workspace.join("shot.png"), b"png")?;
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
    assert_eq!(values, vec!["README.md", "shot.png", "src/main.rs"]);

    let filtered: eix_core::PickerResult = client
        .request(
            "picker.query",
            json!({"kind": "files", "session_id": session_id, "query": "main"}),
        )
        .await?;
    assert_eq!(filtered.items.len(), 1);
    assert_eq!(filtered.items[0].value, "src/main.rs");

    let images: eix_core::PickerResult = client
        .request(
            "picker.query",
            json!({"kind": "images", "session_id": session_id}),
        )
        .await?;
    assert_eq!(images.kind, eix_core::KIND_IMAGES);
    let values: Vec<&str> = images
        .items
        .iter()
        .map(|item| item.value.as_str())
        .collect();
    assert_eq!(values, vec!["shot.png"]);
    assert!(images.items[0].detail.as_deref().unwrap().ends_with(" B"));

    let preview: eix_core::PickerPreviewResult = client
        .request(
            "picker.preview",
            json!({
                "kind": "files",
                "session_id": session_id,
                "value": "src/main.rs",
            }),
        )
        .await?;
    assert_eq!(preview.kind, eix_core::KIND_FILES);
    assert_eq!(preview.value, "src/main.rs");
    assert!(preview.text.contains("fn main() {}"), "{}", preview.text);

    let grep: eix_core::PickerResult = client
        .request(
            "picker.query",
            json!({"kind": "grep", "session_id": session_id, "query": "MAIN"}),
        )
        .await?;
    assert_eq!(grep.kind, eix_core::KIND_GREP);
    assert_eq!(grep.items.len(), 1);
    assert_eq!(grep.items[0].value, "src/main.rs:1");
    assert!(grep.items[0].detail.as_deref().unwrap().contains("fn main"));

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

    let combined: eix_core::PickerResult = client
        .request(
            "picker.query",
            json!({"kind": "git", "session_id": session_id}),
        )
        .await?;
    assert_eq!(combined.kind, eix_core::KIND_GIT);
    assert!(
        combined.items.iter().any(|item| {
            item.value == linked.to_string_lossy().as_ref()
                && item.detail.as_deref() == Some("worktree · eix/linked")
        }),
        "linked worktree missing from combined picker: {combined:?}"
    );
    assert!(
        combined
            .items
            .iter()
            .any(|item| item.value == "feature" && item.detail.as_deref() == Some("branch · init")),
        "branch missing from combined picker: {combined:?}"
    );

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
    mock_provider_delayed(listener, bodies, Duration::ZERO).await
}

/// Like `mock_provider`, but waits before answering a chat request so a test
/// can submit the next prompt while a turn is still running.
async fn mock_provider_delayed(
    listener: TcpListener,
    bodies: Arc<Mutex<Vec<String>>>,
    delay: Duration,
) {
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

            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
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

/// Reply with an OpenAI-style content array that carries an image part.
async fn image_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
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
            let events = concat!(
                "data: {\"model\":\"mock-image\",\"choices\":[{\"delta\":{\"content\":[{\"type\":\"image_url\",\"image_url\":{\"url\":\"data:image/png;base64,QUJD\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"done\"},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            write_sse(&mut socket, events).await;
        });
    }
}

/// Like `mock_provider`, but records the raw request headers so tests can
/// assert on provider authentication.
async fn header_capturing_provider(listener: TcpListener, headers: Arc<Mutex<Vec<String>>>) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let headers = Arc::clone(&headers);
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
            let raw = String::from_utf8_lossy(&buf[..header_end]).to_string();
            let lower = raw.to_ascii_lowercase();
            let length: usize = lower
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
            headers.lock().await.push(raw);
            let events = concat!(
                "data: {\"model\":\"mock-1\",\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                "data: [DONE]\n\n",
            );
            write_sse(&mut socket, events).await;
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
                let added: String = content.lines().map(|line| format!("+{line}\n")).collect();
                let patch = format!(
                    "*** Begin Patch\n*** Update File: out.txt\n@@\n-before\n{added}*** End Patch\n"
                );
                let arguments = json!({"patchText": patch}).to_string();
                let chunk = json!({
                    "model": "mock-tools",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_1",
                                "type": "function",
                                "function": {"name": "apply_patch", "arguments": arguments},
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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;
    config.compations.claude_code = true;

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
    assert!(bodies[0].contains("- deploy"), "{}", bodies[0]);
    assert!(bodies[0].contains("- audit"));
    assert!(
        !bodies[0].contains("<allowed_tools>"),
        "tool grants belong to the use_skill result, not the catalog"
    );
    assert!(bodies[1].contains("Run the deploy checklist before shipping."));
    assert!(bodies[1].contains("# skill: deploy"));

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
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
        .find(|event| event.payload["tool"] == "apply_patch")
        .expect("blocked patch result");
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
                    "apply_patch",
                    json!({
                        "patchText": "*** Begin Patch\n*** Add File: x.txt\n+blocked\n*** End Patch\n"
                    })
                    .to_string(),
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
async fn update_plan_tool_updates_the_session_task_list() -> anyhow::Result<()> {
    let dir = temp_dir("todos");
    std::fs::create_dir_all(&dir)?;
    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(plan_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
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
    // A plan update is not a denial: transcripts must not mark it as one.
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let plan_result = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::ToolResult && event.payload["tool"] == "update_plan")
        .expect("update_plan result");
    assert_eq!(plan_result.payload["ok"], true);
    assert_eq!(plan_result.payload["denied"], false);
    let bodies = bodies.lock().await;
    assert!(bodies[0].contains("\"name\":\"update_plan\""));
    assert!(bodies[1].contains("[>] write the fix"));

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

async fn plan_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
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
                    "explanation": "two steps",
                    "plan": [
                        {"step": "inspect the code", "status": "completed"},
                        {"step": "write the fix", "status": "in_progress"}
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
                                "function": {"name": "update_plan", "arguments": arguments},
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

async fn large_output_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
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
                    "model": "mock-large",
                    "choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}],
                })
            } else {
                let arguments =
                    json!({"cmd": "cat big.txt", "max_output_tokens": 100_000}).to_string();
                json!({
                    "model": "mock-large",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_big",
                                "type": "function",
                                "function": {"name": "exec_command", "arguments": arguments},
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
async fn pressure_prunes_tool_output_before_summarizing() -> anyhow::Result<()> {
    let dir = temp_dir("prune-first");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("big.txt"), "0123456789abcdef\n".repeat(25_000))?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(large_output_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-large".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;
    // The raw result is huge, so the ordinary build truncates it to 20k tokens.
    config.tools.max_output_bytes = 400_000;
    config.context.tool_output_tokens = 20_000;
    // Pressure is certain before pruning and impossible after it: pruning
    // leaves ~5k tokens of tool output against a 12.8k threshold.
    config.context.budget_tokens = 16_000;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "prune", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "read the file"}),
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
    assert!(
        !attached
            .events
            .iter()
            .any(|event| event.kind == EventKind::ContextCompacted),
        "pruning should have relieved the pressure without a summary"
    );

    let bodies = bodies.lock().await;
    assert_eq!(bodies.len(), 2, "a prune must not add a summary request");
    let second: Value = serde_json::from_str(&bodies[1])?;
    let tool = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "tool")
        .expect("tool message");
    let content = tool["content"].as_str().unwrap();
    assert!(content.contains("truncated"), "{content}");
    // The unpruned view is ~80KB; the pruned one is ~20KB plus a marker.
    assert!(
        content.len() < 30_000,
        "tool output was not pruned: {} chars",
        content.len()
    );

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

async fn verbose_summary_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
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
            let summarizing = value["messages"]
                .as_array()
                .map(|messages| {
                    messages.iter().any(|message| {
                        message["content"]
                            .as_str()
                            .is_some_and(|text| text.contains("Summarize the conversation"))
                    })
                })
                .unwrap_or(false);
            // A summary as big as the history it should replace.
            let content = if summarizing {
                "the same details again ".repeat(5_000)
            } else {
                "ok".to_string()
            };
            let chunk = json!({
                "model": "mock-verbose",
                "choices": [{"delta": {"content": content}, "finish_reason": "stop"}],
            });
            let events = format!("data: {chunk}\n\ndata: [DONE]\n\n");
            write_sse(&mut socket, &events).await;
        });
    }
}

#[tokio::test]
async fn a_summary_that_does_not_shrink_history_is_rejected() -> anyhow::Result<()> {
    let dir = temp_dir("no-shrink");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(verbose_summary_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-verbose".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;
    // Below the budget, so trimming keeps the history intact, but far enough
    // over the threshold that compaction is attempted on every round.
    config.context.budget_tokens = 8_000;
    config.context.auto_compact_threshold = 0.5;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "no-shrink", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let padding = "build the thing carefully ".repeat(120);
    for text in [padding.clone(), padding.clone(), padding] {
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
    assert!(
        !attached
            .events
            .iter()
            .any(|event| event.kind == EventKind::ContextCompacted),
        "a non-shrinking summary must not be committed"
    );
    let bodies = bodies.lock().await;
    assert!(
        bodies
            .iter()
            .any(|body| body.contains("Summarize the conversation")),
        "the summary was never requested"
    );

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
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

    let mut compacted = None;
    for text in ["one", "two", "three"] {
        let submitted: SubmitResult = client
            .request(
                "prompt.submit",
                json!({"session_id": session_id, "text": text}),
            )
            .await?;
        loop {
            let notification = client.next_notification().await?;
            if notification.method == "context.compacted" {
                compacted = Some(notification.params.clone());
            }
            if notification.method == "turn.completed"
                && notification.params["turn_id"] == submitted.turn_id.as_str()
            {
                assert!(notification.params["error"].is_null());
                break;
            }
        }
    }
    // The summary runs beside the turn, so it may finish after the last
    // `turn.completed`. Wait for the completion notification instead of
    // assuming it already landed.
    while compacted.is_none() {
        let notification = client.next_notification().await?;
        if notification.method == "context.compacted" {
            compacted = Some(notification.params);
        }
    }
    let compacted = compacted.expect("auto compaction notification");
    assert_eq!(compacted["auto"], true);
    assert_eq!(compacted["messages"], 2);

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let event = attached
        .events
        .iter()
        .find(|event| event.kind == EventKind::ContextCompacted)
        .expect("auto compaction event");
    // The stored summary carries an explicit continuation prefix so the next
    // model does not restart completed work.
    let summary_text = event.payload["summary"].as_str().expect("summary");
    assert!(
        summary_text.contains("Another model started this task"),
        "{summary_text}"
    );
    assert!(summary_text.ends_with("Hello from mock"), "{summary_text}");

    // The next turn rebuilds its ledger from the event log and must load the
    // summary that the previous turn queued in the background.
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
            assert!(notification.params["error"].is_null());
            break;
        }
    }

    let bodies = bodies.lock().await;
    let parsed: Vec<Value> = bodies
        .iter()
        .map(|body| serde_json::from_str(body))
        .collect::<Result<_, _>>()?;
    let summary = parsed
        .iter()
        .find(|body| {
            body["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["content"]
                        .as_str()
                        .unwrap_or_default()
                        .contains("Summarize the conversation")
                })
            })
        })
        .expect("summary request");
    assert!(summary["messages"].to_string().contains("one"));
    let fourth = parsed
        .iter()
        .find(|body| {
            body["messages"].as_array().is_some_and(|messages| {
                messages.iter().any(|message| {
                    message["role"] == "user"
                        && message["content"]
                            .as_str()
                            .unwrap_or_default()
                            .contains("four")
                })
            })
        })
        .expect("fourth turn request");
    assert!(fourth["messages"]
        .as_array()
        .unwrap()
        .iter()
        .any(|message| {
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
                .and_then(|messages| {
                    // Volatile sections (`<environment>`, `<tasks>`, …) are
                    // appended as user messages after history, so look past
                    // them when deciding whether the last real step was a tool
                    // result.
                    messages.iter().rev().find(|message| {
                        let content = message["content"].as_str().unwrap_or_default();
                        message["role"] != "user"
                            || !["<environment>", "<tasks>", "<context>", "<directives>"]
                                .iter()
                                .any(|tag| content.starts_with(tag))
                    })
                })
                .map(|message| message["role"] == "tool")
                .unwrap_or(false);
            let events = if last_is_tool {
                let chunk = json!({
                    "model": "mock-tools",
                    "choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}],
                });
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            } else {
                let again = value["messages"].as_array().is_some_and(|messages| {
                    messages.iter().any(|message| {
                        message["role"] == "user"
                            && message["content"]
                                .as_str()
                                .is_some_and(|text| text.contains("write it again"))
                    })
                });
                let patch = if again {
                    "*** Begin Patch\n*** Update File: out.txt\n@@\n-hello\n+again\n*** End Patch\n"
                } else {
                    "*** Begin Patch\n*** Update File: out.txt\n@@\n-before\n+hello\n*** End Patch\n"
                };
                let arguments = json!({"patchText": patch}).to_string();
                let chunk = json!({
                    "model": "mock-tools",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_1",
                                "type": "function",
                                "function": {"name": "apply_patch", "arguments": arguments},
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
        "[providers.openai]\ndefault = true\nbase_url = \"http://127.0.0.1:9/v1\"\nmodel = \"first-model\"\napi_key = \"test\"\n",
    )?;

    let mut config = Config::default();
    config.default_provider_mut().base_url = "http://127.0.0.1:9/v1".into();
    config.default_provider_mut().model = "first-model".into();
    config.default_provider_mut().api_key = Some("test".into());
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
        "[providers.openai]\ndefault = true\nbase_url = \"http://127.0.0.1:9/v1\"\nmodel = \"second-model\"\napi_key = \"test\"\n",
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
async fn subagent_concurrency_can_change_at_runtime() -> anyhow::Result<()> {
    let dir = temp_dir("subagent-concurrency");
    std::fs::create_dir_all(&dir)?;

    let mut config = Config::default();
    config.default_provider_mut().base_url = "http://127.0.0.1:9/v1".into();
    config.default_provider_mut().model = "test-model".into();
    config.default_provider_mut().api_key = Some("test".into());
    config.tools.max_parallel_subagents = 8;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;

    let hello: HelloResult = client.request("hello", Value::Null).await?;
    assert_eq!(hello.max_parallel_subagents, 8);

    let updated: Value = client
        .request("config.subagents", json!({ "max_parallel_subagents": 64 }))
        .await?;
    assert_eq!(updated["max_parallel_subagents"], 64);
    let hello: HelloResult = client.request("hello", Value::Null).await?;
    assert_eq!(hello.max_parallel_subagents, 64);

    for value in [0, 1, 3, 6, 12, 48, 100, 512] {
        assert_eq!(
            client
                .request_error(
                    "config.subagents",
                    json!({ "max_parallel_subagents": value }),
                )
                .await?,
            "invalid_params",
            "{value}"
        );
    }

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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

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
        .all(|item| item.kind == "user_message"));

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
async fn unused_sessions_can_be_discarded_without_losing_work() -> anyhow::Result<()> {
    let dir = temp_dir("delete-empty");
    std::fs::create_dir_all(&dir)?;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(
        Config::default(),
        socket.clone(),
        dir.join("data"),
    ));
    let mut client = connect(&socket).await?;

    let empty: SessionResult = client
        .request("session.create", json!({"name": "empty", "cwd": dir}))
        .await?;
    let drafted: SessionResult = client
        .request("session.create", json!({"name": "drafted", "cwd": dir}))
        .await?;

    client
        .request::<eix_core::DraftResult>(
            "draft.set",
            json!({"session_id": drafted.session.id, "text": "keep me"}),
        )
        .await?;
    let kept: Value = client
        .request(
            "session.delete_if_empty",
            json!({"session_id": drafted.session.id}),
        )
        .await?;
    assert_eq!(kept["deleted"], false);

    let discarded: Value = client
        .request(
            "session.delete_if_empty",
            json!({"session_id": empty.session.id}),
        )
        .await?;
    assert_eq!(discarded["deleted"], true);
    let list: eix_core::SessionList = client.request("session.list", Value::Null).await?;
    assert!(!list.sessions.iter().any(|s| s.id == empty.session.id));

    // Clearing the draft makes that session discardable too.
    client
        .request::<eix_core::DraftResult>(
            "draft.set",
            json!({"session_id": drafted.session.id, "text": ""}),
        )
        .await?;
    let discarded: Value = client
        .request(
            "session.delete_if_empty",
            json!({"session_id": drafted.session.id}),
        )
        .await?;
    assert_eq!(discarded["deleted"], true);

    daemon.abort();
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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
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
    // Remote callers are contained by the directory the config was loaded
    // from, just as they are for a daemon started with `Config::load`.
    config.load_cwd = dir.clone();
    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut local = connect(&socket).await?;

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

    // Remote clients cannot execute local shell commands, enable full mode, or
    // point a session outside the daemon's workspace.
    for (id, method, params) in [
        (
            4,
            "shell.run",
            json!({"session_id": target.clone(), "command": "printf remote-pwn"}),
        ),
        (
            5,
            "session.mode",
            json!({"session_id": target.clone(), "mode": "full"}),
        ),
        (
            6,
            "session.create",
            json!({"name": "full", "cwd": dir.clone(), "mode": "full"}),
        ),
        (7, "session.create", json!({"name": "escape", "cwd": "/"})),
        (
            8,
            "session.cd",
            json!({"session_id": target.clone(), "path": "/"}),
        ),
    ] {
        let response = http
            .post(format!("{base}/rpc"))
            .bearer_auth("http-secret")
            .json(&json!({"id": id, "method": method, "params": params}))
            .send()
            .await?;
        assert_eq!(response.status(), 200);
        let response: Value = response.json().await?;
        assert_eq!(
            response["error"]["code"], "forbidden",
            "{method} was not denied: {response}"
        );
    }

    // A local full session cannot be used as a remote full session: importing,
    // forking, or rewinding it is rejected as well.
    let local_session: SessionResult = local
        .request(
            "session.create",
            json!({"name": "local-full", "cwd": dir.clone()}),
        )
        .await?;
    local
        .request::<Value>(
            "session.mode",
            json!({"session_id": local_session.session.id, "mode": "full"}),
        )
        .await?;
    let exported: Value = local
        .request(
            "session.export",
            json!({"session_id": local_session.session.id}),
        )
        .await?;
    let export_json = exported["json"].as_str().expect("export json").to_string();
    for (id, method, params) in [
        (
            9,
            "session.import",
            json!({"json": export_json, "cwd": dir.clone()}),
        ),
        (
            10,
            "session.fork",
            json!({"session_id": local_session.session.id}),
        ),
        (
            11,
            "session.rewind",
            json!({"session_id": local_session.session.id}),
        ),
        (
            12,
            "prompt.submit",
            json!({"session_id": local_session.session.id, "text": "remote full"}),
        ),
        (
            14,
            "session.ready",
            json!({"session_id": local_session.session.id}),
        ),
        (
            15,
            "integration.continue",
            json!({"session_id": local_session.session.id}),
        ),
    ] {
        let response = http
            .post(format!("{base}/rpc"))
            .bearer_auth("http-secret")
            .json(&json!({"id": id, "method": method, "params": params}))
            .send()
            .await?;
        assert_eq!(response.status(), 200);
        let response: Value = response.json().await?;
        assert_eq!(
            response["error"]["code"], "forbidden",
            "{method} was not denied: {response}"
        );
    }

    // A remote client must not be able to run a turn in a local session whose
    // working directory is outside the exposed workspace.
    let outside_session: SessionResult = local
        .request(
            "session.create",
            json!({"name": "local-outside", "cwd": "/"}),
        )
        .await?;
    for (id, method, params) in [
        (
            13,
            "prompt.submit",
            json!({"session_id": outside_session.session.id, "text": "escape"}),
        ),
        (
            16,
            "session.ready",
            json!({"session_id": outside_session.session.id}),
        ),
        (
            17,
            "integration.continue",
            json!({"session_id": outside_session.session.id}),
        ),
    ] {
        let response = http
            .post(format!("{base}/rpc"))
            .bearer_auth("http-secret")
            .json(&json!({"id": id, "method": method, "params": params}))
            .send()
            .await?;
        assert_eq!(response.status(), 200);
        let response: Value = response.json().await?;
        assert_eq!(
            response["error"]["code"], "forbidden",
            "{method} escaped the workspace: {response}"
        );
    }

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

    // The OpenAPI document is public and describes the pairing route too.
    let openapi = http.get(format!("{base}/openapi.json")).send().await?;
    assert_eq!(openapi.status(), 200);
    let openapi: Value = openapi.json().await?;
    assert_eq!(openapi["openapi"], "3.1.0");
    assert_eq!(openapi["info"]["version"], env!("CARGO_PKG_VERSION"));
    for path in ["/health", "/stats", "/rpc", "/events", "/pair"] {
        assert!(
            openapi["paths"].get(path).is_some(),
            "openapi is missing {path}"
        );
    }

    // Pairing: the local owner mints a one-time code; the client redeems it
    // over unauthenticated HTTP for a durable token.
    let pairing: Value = local
        .request("pair.create", json!({"name": "laptop"}))
        .await?;
    let code = pairing["code"].as_str().expect("pairing code").to_string();
    assert_eq!(code.len(), 19);
    assert!(pairing["http"]
        .as_str()
        .unwrap_or_default()
        .ends_with("/pair"));

    // Codes are case- and dash-insensitive.
    let loose = code.replace('-', "").to_uppercase();
    let exchanged = http
        .post(format!("{base}/pair"))
        .json(&json!({"code": loose}))
        .send()
        .await?;
    assert_eq!(exchanged.status(), 200, "pairing failed");
    let exchanged: Value = exchanged.json().await?;
    let paired = exchanged["token"]
        .as_str()
        .expect("paired token")
        .to_string();
    assert_eq!(exchanged["client"]["name"], "laptop");
    assert!(exchanged["client"]["id"]
        .as_str()
        .unwrap_or_default()
        .starts_with("ct_"));

    // The minted token opens the same remote surface as the configured one.
    let stats = http
        .get(format!("{base}/stats"))
        .header("x-eix-token", &paired)
        .send()
        .await?;
    assert_eq!(stats.status(), 200);
    let hello = http
        .post(format!("{base}/rpc"))
        .bearer_auth(&paired)
        .json(&json!({"id": 90, "method": "hello", "params": null}))
        .send()
        .await?;
    assert_eq!(hello.status(), 200);
    let hello: Value = hello.json().await?;
    assert_eq!(hello["result"]["protocol"], PROTOCOL_VERSION);

    // A paired client cannot mint or revoke other clients.
    let denied = http
        .post(format!("{base}/rpc"))
        .bearer_auth(&paired)
        .json(&json!({"id": 91, "method": "pair.create", "params": {}}))
        .send()
        .await?;
    let denied: Value = denied.json().await?;
    assert_eq!(denied["error"]["code"], "forbidden");

    let listed: Value = local.request("pair.list", json!({})).await?;
    let clients = listed["clients"].as_array().expect("client list");
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0]["name"], "laptop");
    assert_eq!(clients[0]["revoked"], false);
    assert!(clients[0]["last_used_at"].as_i64().is_some());
    let client_id = clients[0]["id"].as_str().expect("client id").to_string();

    // Codes are single use.
    let reused = http
        .post(format!("{base}/pair"))
        .json(&json!({"code": code}))
        .send()
        .await?;
    assert_eq!(reused.status(), 401);

    // Revocation takes effect immediately for both HTTP and the token check.
    let revoked: Value = local
        .request("pair.revoke", json!({"id": client_id}))
        .await?;
    assert_eq!(revoked["revoked"], true);
    let after = http
        .get(format!("{base}/stats"))
        .bearer_auth(&paired)
        .send()
        .await?;
    assert_eq!(after.status(), 401);
    assert_eq!(
        http.get(format!("{base}/stats"))
            .bearer_auth("http-secret")
            .send()
            .await?
            .status(),
        200,
        "revoking one client must not disable the configured token"
    );

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
            && event.payload["tool"] == "exec_command"
            && event.payload["arguments"]["command"] == "printf shell-ok"));
    assert!(attached
        .events
        .iter()
        .any(|event| event.kind == EventKind::ToolResult
            && event.payload["tool"] == "exec_command"
            && event.payload["ok"] == true));

    let readonly: SessionResult = client
        .request(
            "session.create",
            json!({"name": "readonly", "cwd": dir, "mode": "plan"}),
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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

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
async fn rewind_to_any_turn_respects_the_scope() -> anyhow::Result<()> {
    let dir = temp_dir("rewind-scope");
    std::fs::create_dir_all(&dir)?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(mock_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "rewind", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let mut user_seqs = Vec::new();
    for text in ["first turn", "second turn"] {
        let submitted: SubmitResult = client
            .request(
                "prompt.submit",
                json!({"session_id": session_id, "text": text}),
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
        let user = attached
            .events
            .iter()
            .filter(|event| event.kind == EventKind::UserMessage)
            .find(|event| event.payload["text"] == text)
            .expect("user event");
        user_seqs.push(user.seq);
    }

    // Conversation-only hides the second turn but leaves every event on disk.
    let rewound: Value = client
        .request(
            "session.rewind_to",
            json!({
                "session_id": session_id,
                "at_seq": user_seqs[1],
                "scope": "conversation",
            }),
        )
        .await?;
    assert_eq!(rewound["at_seq"], user_seqs[1]);
    assert!(rewound["changed"].as_u64().unwrap_or(0) >= 2);
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert!(
        attached
            .events
            .iter()
            .any(|event| event.kind == EventKind::UserMessage
                && event.payload["text"] == "first turn")
    );
    assert!(!attached.events.iter().any(
        |event| event.kind == EventKind::UserMessage && event.payload["text"] == "second turn"
    ));

    // Redo restores that range.
    let redone: Value = client
        .request("session.redo", json!({"session_id": session_id}))
        .await?;
    assert!(redone["changed"].as_u64().unwrap_or(0) >= 2);
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert!(attached.events.iter().any(
        |event| event.kind == EventKind::UserMessage && event.payload["text"] == "second turn"
    ));

    // Files-only keeps the conversation visible and changes no events.
    let rewound: Value = client
        .request(
            "session.rewind_to",
            json!({
                "session_id": session_id,
                "at_seq": user_seqs[0],
                "scope": "files",
            }),
        )
        .await?;
    assert_eq!(rewound["changed"], 0);
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert!(attached.events.iter().any(
        |event| event.kind == EventKind::UserMessage && event.payload["text"] == "second turn"
    ));

    // Both hides the first turn and everything after it.
    let rewound: Value = client
        .request(
            "session.rewind_to",
            json!({
                "session_id": session_id,
                "at_seq": user_seqs[0],
                "scope": "both",
            }),
        )
        .await?;
    assert!(rewound["changed"].as_u64().unwrap_or(0) >= 2);
    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    assert!(!attached
        .events
        .iter()
        .any(|event| event.kind == EventKind::UserMessage));

    // An assistant event is not a checkpoint.
    assert!(client
        .request::<Value>(
            "session.rewind_to",
            json!({
                "session_id": session_id,
                "at_seq": user_seqs[0] + 1,
                "scope": "conversation",
            }),
        )
        .await
        .is_err());

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

#[tokio::test]
async fn undo_and_redo_restore_workspace_files() -> anyhow::Result<()> {
    let root = temp_dir("undo-files");
    let dir = root.join("workspace");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("out.txt"), "before")?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(tool_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

    let socket = root.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), root.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "undo-files", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    client
        .request::<SubmitResult>(
            "prompt.submit",
            json!({"session_id": session_id, "text": "write out.txt"}),
        )
        .await?;
    assert_eq!(wait_turn(&mut client).await?, "done");
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, "hello");

    let undone: UndoResult = client
        .request("session.undo", json!({"session_id": session_id}))
        .await?;
    assert!(undone.changed >= 2, "{undone:?}");
    assert!(undone.files_restored >= 1, "{undone:?}");
    assert!(undone.warnings.is_empty(), "{undone:?}");
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, "before");

    let redone: UndoResult = client
        .request("session.redo", json!({"session_id": session_id}))
        .await?;
    assert!(redone.changed >= 2, "{redone:?}");
    assert!(redone.files_restored >= 1, "{redone:?}");
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, "hello");

    // Undoing again rolls the files back once more; the redo snapshot is
    // consumed, so a second redo has nothing left to replay.
    let undone: UndoResult = client
        .request("session.undo", json!({"session_id": session_id}))
        .await?;
    assert!(undone.files_restored >= 1, "{undone:?}");
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, "before");

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&root).await;
    Ok(())
}

#[tokio::test]
async fn workspace_diff_and_doctor_report_the_daemon_view() -> anyhow::Result<()> {
    let root = temp_dir("diff-doctor");
    let dir = root.join("workspace");
    std::fs::create_dir_all(&dir)?;
    assert!(git(&dir, &["init", "-q"]).status.success());
    std::fs::write(dir.join("tracked.txt"), "old\n")?;
    assert!(git(&dir, &["add", "."]).status.success());
    assert!(git(&dir, &["commit", "-qm", "init"]).status.success());
    std::fs::write(dir.join("tracked.txt"), "new\n")?;
    std::fs::write(dir.join("untracked.txt"), "fresh\n")?;

    let socket = root.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(
        Config::default(),
        socket.clone(),
        root.join("data"),
    ));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request("session.create", json!({"name": "diff", "cwd": dir}))
        .await?;
    let session_id = created.session.id.clone();

    let diff: WorkspaceDiffResult = client
        .request("session.diff", json!({"session_id": session_id}))
        .await?;
    assert_eq!(diff.files, 1, "{}", diff.diff);
    assert!(diff.diff.contains("tracked.txt"), "{}", diff.diff);
    assert!(diff.diff.contains("-old"), "{}", diff.diff);
    assert!(diff.diff.contains("+new"), "{}", diff.diff);
    assert!(!diff.truncated);
    assert!(
        diff.notes.iter().any(|note| note.contains("untracked.txt")),
        "{:?}",
        diff.notes
    );

    // The staged view compares the index to HEAD, so the unstaged edit and the
    // untracked file are both absent.
    let staged: WorkspaceDiffResult = client
        .request(
            "session.diff",
            json!({"session_id": session_id, "staged": true}),
        )
        .await?;
    assert_eq!(staged.files, 0);
    assert!(staged.diff.trim().is_empty(), "{}", staged.diff);
    assert!(staged.notes.is_empty(), "{:?}", staged.notes);

    let doctor: DoctorReport = client.request("doctor.report", Value::Null).await?;
    assert!(
        doctor
            .lines
            .iter()
            .any(|line| line.starts_with("socket") && line.contains("eix.sock")),
        "{:?}",
        doctor.lines
    );
    assert!(
        doctor.lines.iter().any(|line| line.starts_with("data")),
        "{:?}",
        doctor.lines
    );
    assert!(
        doctor
            .lines
            .iter()
            .any(|line| line.starts_with("daemon") && line.contains("protocol")),
        "{:?}",
        doctor.lines
    );

    daemon.abort();
    let _ = tokio::fs::remove_dir_all(&root).await;
    Ok(())
}

#[tokio::test]
async fn always_allow_skips_later_patches_for_the_same_path() -> anyhow::Result<()> {
    let dir = temp_dir("always");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("out.txt"), "before")?;

    let bodies = Arc::new(Mutex::new(Vec::<String>::new()));
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(repeat_tool_provider(listener, Arc::clone(&bodies)));

    let mut config = Config::default();
    config.tools.auto_apply_workspace_edits = false;
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-model".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;

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
        if notification.method == "tool.approval_required" {
            break notification.params.clone();
        }
    };
    let approval_id = proposed["approval_id"].as_str().unwrap().to_string();
    let answered: eix_core::ApprovalResult = client
        .request(
            "approval.respond",
            json!({
                "session_id": session_id,
                "approval_id": approval_id,
                "decision": "always",
            }),
        )
        .await?;
    assert!(answered.accepted);
    loop {
        let notification = client.next_notification().await?;
        if notification.method == "turn.completed"
            && notification.params["turn_id"] == first.turn_id.as_str()
        {
            assert!(notification.params["error"].is_null());
            break;
        }
    }
    assert_eq!(std::fs::read_to_string(dir.join("out.txt"))?, "hello");

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
            .is_some_and(|rule| rule.starts_with("always-apply_patch-")),
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

/// A provider that asks for the mock plugin's `echo` tool, then answers.
async fn plugin_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
    plugin_provider_for(listener, bodies, "plugin__mock__echo").await;
}

async fn plugin_provider_for(
    listener: TcpListener,
    bodies: Arc<Mutex<Vec<String>>>,
    plugin_tool: &'static str,
) {
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
            // Plugin tools are deferred: round one searches, round two calls
            // the discovered tool, round three stops.
            let tool_results = value["messages"]
                .as_array()
                .map(|messages| {
                    messages
                        .iter()
                        .filter(|message| message["role"] == "tool")
                        .count()
                })
                .unwrap_or(0);
            if tool_results >= 2 {
                let chunk = json!({
                    "model": "mock-plugin",
                    "choices": [{"delta": {"content": "done"}, "finish_reason": "stop"}],
                });
                write_sse(&mut socket, &format!("data: {chunk}\n\ndata: [DONE]\n\n")).await;
                return;
            }
            let (id, name, arguments) = if tool_results == 0 {
                ("call_search", "tool_search", json!({"query": "echo text"}))
            } else {
                ("call_plugin_0", plugin_tool, json!({"text": "hi"}))
            };
            let chunk = json!({
                "model": "mock-plugin",
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": arguments.to_string(),
                            },
                        }]
                    },
                    "finish_reason": "tool_calls",
                }],
            });
            write_sse(&mut socket, &format!("data: {chunk}\n\ndata: [DONE]\n\n")).await;
        });
    }
}

/// Parent asks for a write, a child, and a slow read in one batch. The child
/// marks itself started as soon as its provider request arrives.
async fn early_agent_provider(
    listener: TcpListener,
    read_url: String,
    child_started: Arc<AtomicBool>,
) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let read_url = read_url.clone();
        let child_started = Arc::clone(&child_started);
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let tool_messages = value["messages"]
                .as_array()
                .map(|messages| {
                    messages
                        .iter()
                        .filter(|message| message["role"] == "tool")
                        .count()
                })
                .unwrap_or(0);
            let can_spawn = value["tools"]
                .as_array()
                .map(|tools| {
                    tools
                        .iter()
                        .any(|tool| tool["function"]["name"] == "spawn_agent")
                })
                .unwrap_or(false);

            let chunk = if can_spawn && tool_messages == 0 {
                let write = json!({
                    "patchText": "*** Begin Patch\n*** Add File: early.txt\n+written\n*** End Patch\n"
                })
                .to_string();
                let spawn = json!({"prompt": "investigate", "name": "scout"}).to_string();
                let fetch = json!({"url": read_url}).to_string();
                json!({
                    "model": "mock-agent-early",
                    "choices": [{
                        "delta": {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "call_write",
                                    "type": "function",
                                    "function": {"name": "apply_patch", "arguments": write},
                                },
                                {
                                    "index": 1,
                                    "id": "call_agent",
                                    "type": "function",
                                    "function": {"name": "spawn_agent", "arguments": spawn},
                                },
                                {
                                    "index": 2,
                                    "id": "call_read",
                                    "type": "function",
                                    "function": {"name": "web_fetch", "arguments": fetch},
                                }
                            ]
                        },
                        "finish_reason": "tool_calls",
                    }],
                })
            } else if can_spawn {
                json!({
                    "model": "mock-agent-early",
                    "choices": [{"delta": {"content": "parent done"}, "finish_reason": "stop"}],
                })
            } else {
                child_started.store(true, Ordering::SeqCst);
                json!({
                    "model": "mock-agent-early",
                    "choices": [{"delta": {"content": "child answer"}, "finish_reason": "stop"}],
                })
            };
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
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "call_agent",
                                    "type": "function",
                                    "function": {"name": "spawn_agent", "arguments": arguments},
                                },
                                {
                                    "index": 1,
                                    "id": "call_list",
                                    "type": "function",
                                    "function": {"name": "exec_command", "arguments": "{\"cmd\":\"ls\"}"},
                                },
                            ]
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
                0 => tool_call_event(json!({"cmd": "cat secret.txt"}), "exec_command", "call_1"),
                1 => {
                    let placeholder = first_placeholder(&value).unwrap_or_default();
                    let patch = format!(
                        "*** Begin Patch\n*** Add File: out.txt\n+{placeholder}\n*** End Patch\n"
                    );
                    tool_call_event(json!({"patchText": patch}), "apply_patch", "call_2")
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
                let arguments = json!({"cmd": "sed -i 's/before/after/' src/keep.txt"}).to_string();
                let chunk = json!({
                    "model": "mock-tools",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_1",
                                "type": "function",
                                "function": {"name": "exec_command", "arguments": arguments},
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

/// A provider that asks for the same allowed command twice, then answers.
/// Classifier requests are answered with a confident allow; the daemon should
/// cache that verdict and not ask the provider a second time.
async fn classifier_provider(listener: TcpListener, bodies: Arc<Mutex<Vec<String>>>) {
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

            let system = value["messages"]
                .as_array()
                .and_then(|messages| messages.first())
                .and_then(|message| message["content"].as_str())
                .unwrap_or_default();
            if system.contains("eix's execution classifier") {
                let chunk = json!({
                    "model": "mock-classifier",
                    "choices": [{
                        "delta": {"content": "{\"decision\":\"allow\",\"confidence\":0.97,\"reason\":\"the user asked to commit\"}"},
                        "finish_reason": "stop",
                    }],
                });
                write_sse(&mut socket, &format!("data: {chunk}\n\ndata: [DONE]\n\n")).await;
                return;
            }

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
                0 | 1 => {
                    let arguments =
                        json!({"cmd": "git commit --allow-empty -m auto-classified"}).to_string();
                    let chunk = json!({
                        "model": "mock-tools",
                        "choices": [{
                            "delta": {
                                "tool_calls": [{
                                    "index": 0,
                                    "id": format!("call_{}", tool_results + 1),
                                    "type": "function",
                                    "function": {"name": "exec_command", "arguments": arguments},
                                }]
                            },
                            "finish_reason": "tool_calls",
                        }],
                    });
                    format!("data: {chunk}\n\ndata: [DONE]\n\n")
                }
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

/// A provider whose classifier returns invalid output. The daemon must keep
/// the call behind the normal approval prompt.
async fn invalid_classifier_provider(listener: TcpListener) {
    loop {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let Some(body) = read_http_body(&mut socket).await else {
                return;
            };
            let value: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let system = value["messages"]
                .as_array()
                .and_then(|messages| messages.first())
                .and_then(|message| message["content"].as_str())
                .unwrap_or_default();
            if system.contains("eix's execution classifier") {
                let chunk = json!({
                    "model": "mock-classifier",
                    "choices": [{
                        "delta": {"content": "not a json verdict"},
                        "finish_reason": "stop",
                    }],
                });
                write_sse(&mut socket, &format!("data: {chunk}\n\ndata: [DONE]\n\n")).await;
                return;
            }

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
                    json!({"cmd": "git commit --allow-empty -m fallback-approved"}).to_string();
                let chunk = json!({
                    "model": "mock-tools",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_fallback",
                                "type": "function",
                                "function": {"name": "exec_command", "arguments": arguments},
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

/// A provider for the `request_user_input` test: the first round asks a
/// question, the second (after the tool result) stops with "done".
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
                    "questions": [
                        {
                            "id": "database",
                            "header": "Database",
                            "question": "Which database should we use?",
                            "options": [
                                {"label": "SQLite", "description": "Embedded, no setup"},
                                {"label": "Postgres", "description": "Server, better concurrency"},
                                {"label": "MySQL", "description": "Widely deployed"}
                            ]
                        },
                        {
                            "id": "migration",
                            "header": "Migration",
                            "question": "Migrate the existing data?",
                            "options": [
                                {"label": "Yes", "description": "Run the migration"},
                                {"label": "No", "description": "Start empty"}
                            ]
                        }
                    ]
                })
                .to_string();
                tool_call_events("call_ask", "request_user_input", &arguments)
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
                let arguments = json!({
                    "cmd": "echo e2e-process && sleep 30",
                    "yield_time_ms": 200
                })
                .to_string();
                let chunk = json!({
                    "model": "mock-tools",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_1",
                                "type": "function",
                                "function": {"name": "exec_command", "arguments": arguments},
                            }]
                        },
                        "finish_reason": "tool_calls",
                    }],
                });
                format!("data: {chunk}\n\ndata: [DONE]\n\n")
            } else if tool_results == 1 {
                // The first result names the still-running session; poll it.
                let session = value["messages"]
                    .as_array()
                    .and_then(|messages| {
                        messages
                            .iter()
                            .rev()
                            .find(|message| message["role"] == "tool")
                    })
                    .and_then(|message| message["content"].as_str())
                    .and_then(|text| text.split_once("session "))
                    .map(|(_, rest)| rest.split([' ', ';']).next().unwrap_or_default())
                    .unwrap_or_default()
                    .to_string();
                let arguments =
                    json!({"session_id": session, "chars": "", "yield_time_ms": 200}).to_string();
                let chunk = json!({
                    "model": "mock-tools",
                    "choices": [{
                        "delta": {
                            "tool_calls": [{
                                "index": 0,
                                "id": "call_2",
                                "type": "function",
                                "function": {"name": "write_stdin", "arguments": arguments},
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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-memory".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
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
                                    "function": {"name": "exec_command", "arguments": "{\"cmd\":\"cat a.txt\"}"},
                                },
                                {
                                    "index": 1,
                                    "id": "read_b",
                                    "type": "function",
                                    "function": {"name": "exec_command", "arguments": "{\"cmd\":\"cat b.txt\"}"},
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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-parallel".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
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

/// Provider that asks for a secret `cat` and an ordinary one in the same
/// batch. The secret read must still stop for approval instead of joining the
/// concurrent read segment.
async fn parallel_secret_read_provider(listener: TcpListener) {
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
            let has_tool_results = messages.iter().any(|message| message["role"] == "tool");
            let chunk = if has_tool_results {
                json!({
                    "model": "mock-parallel-secret",
                    "choices": [{"delta": {"content": "read ok"}, "finish_reason": "stop"}],
                })
            } else {
                json!({
                    "model": "mock-parallel-secret",
                    "choices": [{
                        "delta": {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "read_secret",
                                    "type": "function",
                                    "function": {"name": "exec_command", "arguments": "{\"cmd\":\"cat .env\"}"},
                                },
                                {
                                    "index": 1,
                                    "id": "read_plain",
                                    "type": "function",
                                    "function": {"name": "exec_command", "arguments": "{\"cmd\":\"cat a.txt\"}"},
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
async fn parallel_read_batch_keeps_secret_paths_behind_approval() -> anyhow::Result<()> {
    let dir = temp_dir("parallel-secret-read");
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(".env"), "PASSWORD=hunter2\n")?;
    std::fs::write(dir.join("a.txt"), "ordinary\n")?;

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(parallel_secret_read_provider(listener));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-parallel-secret".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;
    config.context.auto_compact = false;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "parallel-secret-read", "cwd": dir}),
        )
        .await?;
    let session_id = created.session.id.clone();
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": session_id, "text": "read both files"}),
        )
        .await?;
    let turn_id = submitted.turn_id.clone();

    let approval = match tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let notification = client.next_notification().await?;
            if notification.method == "tool.approval_required"
                && notification.params["session_id"] == session_id.as_str()
                && notification.params["turn_id"] == turn_id.as_str()
            {
                return Ok::<Value, anyhow::Error>(notification.params.clone());
            }
        }
    })
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            daemon.abort();
            mock.abort();
            anyhow::bail!("secret read did not request approval");
        }
    };
    assert_eq!(approval["tool"], "exec_command");
    assert_eq!(approval["arguments"]["cmd"], "cat .env");
    let approval_id = approval["approval_id"]
        .as_str()
        .expect("approval id")
        .to_string();
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

    let completed = match tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let notification = client.next_notification().await?;
            if notification.method == "turn.completed"
                && notification.params["session_id"] == session_id.as_str()
                && notification.params["turn_id"] == turn_id.as_str()
            {
                return Ok::<Value, anyhow::Error>(notification.params.clone());
            }
        }
    })
    .await
    {
        Ok(result) => result?,
        Err(_) => {
            daemon.abort();
            mock.abort();
            anyhow::bail!("turn did not finish after approving the secret read");
        }
    };
    assert!(completed["error"].is_null(), "{completed}");

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
        .await?;
    let reads: Vec<_> = attached
        .events
        .iter()
        .filter(|event| {
            event.kind == EventKind::ToolResult && event.payload["tool"] == "exec_command"
        })
        .collect();
    assert_eq!(reads.len(), 2);
    assert!(reads.iter().any(|event| event.payload["output"]
        .as_str()
        .unwrap_or_default()
        .contains("PASSWORD=hunter2")));

    daemon.abort();
    mock.abort();
    let _ = tokio::fs::remove_dir_all(&dir).await;
    Ok(())
}

/// Provider that asks for two independent read-only commands in one batch.
/// Both `cat` commands read from FIFOs, so the test can prove they were
/// started concurrently before either could finish.
async fn parallel_command_provider(listener: TcpListener) {
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
                    "model": "mock-parallel-command",
                    "choices": [{"delta": {"content": answer}, "finish_reason": "stop"}],
                })
            } else {
                let a = json!({"cmd": "cat a.fifo"}).to_string();
                let b = json!({"cmd": "cat b.fifo"}).to_string();
                json!({
                    "model": "mock-parallel-command",
                    "choices": [{
                        "delta": {
                            "tool_calls": [
                                {
                                    "index": 0,
                                    "id": "command_a",
                                    "type": "function",
                                    "function": {"name": "exec_command", "arguments": a},
                                },
                                {
                                    "index": 1,
                                    "id": "command_b",
                                    "type": "function",
                                    "function": {"name": "exec_command", "arguments": b},
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
async fn read_only_commands_run_concurrently_and_keep_call_order() -> anyhow::Result<()> {
    let dir = temp_dir("parallel-command");
    std::fs::create_dir_all(&dir)?;
    let fifo_a = dir.join("a.fifo");
    let fifo_b = dir.join("b.fifo");
    let status = std::process::Command::new("mkfifo")
        .arg(&fifo_a)
        .arg(&fifo_b)
        .status()?;
    anyhow::ensure!(status.success(), "mkfifo failed");

    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock = tokio::spawn(parallel_command_provider(listener));

    let mut config = Config::default();
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-parallel-command".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
    config.context.instructions_files = Vec::new();
    config.context.workspace_summary = false;
    config.context.auto_compact = false;
    config.tools.sandbox = eix_core::SandboxMode::Off;

    let socket = dir.join("eix.sock");
    let daemon = tokio::spawn(eixd::run(config, socket.clone(), dir.join("data")));
    let mut client = connect(&socket).await?;
    let created: SessionResult = client
        .request(
            "session.create",
            json!({"name": "parallel-command", "cwd": dir}),
        )
        .await?;
    let submitted: SubmitResult = client
        .request(
            "prompt.submit",
            json!({"session_id": created.session.id, "text": "read both fifos"}),
        )
        .await?;

    let session_id = created.session.id.clone();
    let turn_id = submitted.turn_id.clone();
    let outcome = tokio::time::timeout(Duration::from_secs(10), async {
        let mut started = 0usize;
        loop {
            let notification = client.next_notification().await?;
            match notification.method.as_str() {
                "tool.started" if notification.params["session_id"] == session_id.as_str() => {
                    started += 1;
                    if started == 2 {
                        let a = fifo_a.clone();
                        let b = fifo_b.clone();
                        tokio::spawn(async move {
                            let _ = tokio::fs::write(a, b"AAA\n").await;
                        });
                        tokio::spawn(async move {
                            let _ = tokio::fs::write(b, b"BBB\n").await;
                        });
                    }
                }
                "turn.completed"
                    if notification.params["session_id"] == session_id.as_str()
                        && notification.params["turn_id"] == turn_id.as_str() =>
                {
                    anyhow::ensure!(
                        started >= 2,
                        "both read-only commands should start before either finishes"
                    );
                    return Ok::<Value, anyhow::Error>(notification.params.clone());
                }
                _ => {}
            }
        }
    })
    .await;

    let completed = match outcome {
        Ok(result) => result?,
        Err(_) => {
            daemon.abort();
            mock.abort();
            anyhow::bail!(
                "timed out waiting for both exec_command calls; they likely ran sequentially"
            );
        }
    };
    assert!(completed["error"].is_null(), "{completed}");

    let attached: AttachResult = client
        .request("session.attach", json!({"session_id": session_id}))
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
    config.default_provider_mut().base_url = format!("http://{addr}/v1");
    config.default_provider_mut().model = "mock-recovery".to_string();
    config.default_provider_mut().api_key = None;
    config.provider_runtime.timeout_secs = 30;
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
