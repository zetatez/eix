//! OpenAI-compatible streaming provider.
//!
//! One adapter covers OpenAI, Ollama, vLLM, OpenRouter, LM Studio and friends:
//! POST `/chat/completions` with `stream: true` and parse SSE `data:` lines.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use eix_core::redact::SecretRedactor;
use eix_core::{
    ChatMessage, Config, FunctionCall, ModelRoute, ProviderKind, Role, ToolCall, ToolDefinition,
    Usage,
};
use futures_util::StreamExt;
use serde_json::{json, Value};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("request cancelled")]
    Cancelled,
    #[error("provider configuration: {0}")]
    Config(String),
    #[error("provider request failed: {0}")]
    Http(String),
    #[error("provider returned HTTP {status}: {body}")]
    Api {
        status: u16,
        body: String,
        retry_after_secs: Option<u64>,
    },
    #[error("failed to decode provider stream: {0}")]
    Decode(String),
}

/// How the daemon should react to a provider failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryClass {
    /// Worth another attempt with exponential backoff.
    Transient,
    /// The provider asked us to slow down; retry, preferably after
    /// `Retry-After`.
    RateLimited,
    /// The plan/credits are exhausted. Retrying immediately cannot help.
    Quota,
    /// The prompt is larger than the model's context window. The caller must
    /// compact or trim before retrying; a plain backoff would repeat the error.
    ContextLength,
    /// Configuration, authentication, or request errors that need the user.
    Fatal,
}

impl RetryClass {
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Transient | Self::RateLimited)
    }

    /// Stable label used in retry notifications and logs.
    pub fn label(self) -> &'static str {
        match self {
            Self::Transient => "transient",
            Self::RateLimited => "rate_limited",
            Self::Quota => "quota",
            Self::ContextLength => "context_length",
            Self::Fatal => "fatal",
        }
    }

    /// Rate limits need a more patient floor than a dropped connection when
    /// the provider did not send `Retry-After`.
    pub fn backoff_floor(self) -> Duration {
        match self {
            Self::RateLimited => Duration::from_millis(500),
            _ => Duration::ZERO,
        }
    }
}

impl ProviderError {
    pub fn retry_class(&self) -> RetryClass {
        match self {
            Self::Cancelled | Self::Config(_) => RetryClass::Fatal,
            // Transport failures (DNS, connect, reset, timeout) and malformed
            // streams are usually transient.
            Self::Http(_) | Self::Decode(_) => RetryClass::Transient,
            Self::Api { status, body, .. } => {
                let body = body.to_ascii_lowercase();
                if looks_like_quota(&body) || *status == 402 {
                    return RetryClass::Quota;
                }
                if *status == 413 || looks_like_context_length(&body) {
                    return RetryClass::ContextLength;
                }
                if *status == 429 {
                    return RetryClass::RateLimited;
                }
                if matches!(*status, 408 | 409 | 425) || (500..=599).contains(status) {
                    return RetryClass::Transient;
                }
                // Some providers report overload/rate-limit errors inside a
                // 200 SSE payload. Treat those as transient too.
                if *status == 200 && looks_like_transient(&body) {
                    return RetryClass::Transient;
                }
                RetryClass::Fatal
            }
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Api {
                retry_after_secs: Some(secs),
                ..
            } => Some(Duration::from_secs(*secs)),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub text: String,
    /// Streamed chain-of-thought (`reasoning_content` / `reasoning`), when the
    /// provider exposes it. Never fed back as a chat message.
    pub reasoning: String,
    pub model: String,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
    pub tool_calls: Vec<ToolCall>,
}

/// One streamed fragment. Providers like DeepSeek interleave reasoning and
/// answer tokens, so the host needs to tell them apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamDelta<'a> {
    Content(&'a str),
    Reasoning(&'a str),
}

pub struct Provider {
    client: reqwest::Client,
    model: String,
    /// Completion budget for protocols that require one.
    max_output_tokens: u32,
    /// API keys and OAuth tokens stored with `eix auth login`.
    credentials: crate::auth::Store,
    /// Random per-provider salt for reversible secret placeholders. It never
    /// leaves this process; without it the hashed ids cannot be dictionary
    /// attacked for low-entropy passwords.
    redaction_salt: String,
}

impl Provider {
    pub fn new(config: &Config, data_dir: &std::path::Path) -> Result<Self, ProviderError> {
        Self::with_store(config, crate::auth::Store::under_data_dir(data_dir))
    }

    pub fn with_store(
        config: &Config,
        credentials: crate::auth::Store,
    ) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .user_agent(format!("eix/{}", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(config.provider_runtime.timeout_secs))
            .build()
            .map_err(|e| ProviderError::Http(e.to_string()))?;
        Ok(Self {
            client,
            model: config.default_model().to_string(),
            max_output_tokens: config.provider_runtime.max_output_tokens,
            credentials,
            redaction_salt: uuid::Uuid::new_v4().to_string(),
        })
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    /// Stream a completion with reversible secret redaction at the provider
    /// boundary. Outbound messages are scrubbed before serialization, and any
    /// placeholders the model echoes back are restored before the caller sees
    /// the completion. This covers normal turns, `/btw`, and automatic context
    /// summaries because they all go through this method.
    pub async fn stream_chat<F>(
        &self,
        messages: &[ChatMessage],
        route: &ModelRoute,
        tools: &[ToolDefinition],
        cancel: &AtomicBool,
        mut on_delta: F,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(StreamDelta<'_>) + Send,
    {
        let mut redactor = SecretRedactor::new(self.redaction_salt.clone());
        let sanitized = redactor.redact_messages(messages);
        let mut result = match route.kind {
            ProviderKind::OpenAi => {
                self.stream_openai(&sanitized, route, tools, cancel, &mut on_delta)
                    .await
            }
            ProviderKind::Anthropic => {
                self.stream_anthropic(&sanitized, route, tools, cancel, &mut on_delta)
                    .await
            }
            ProviderKind::Gemini => {
                self.stream_gemini(&sanitized, route, tools, cancel, &mut on_delta)
                    .await
            }
        };
        if let Ok(completion) = &mut result {
            completion.text = redactor.restore(&completion.text);
            completion.reasoning = redactor.restore(&completion.reasoning);
            for call in &mut completion.tool_calls {
                if restore_tool_arguments(&call.function.name) {
                    call.function.arguments = redactor.restore_json(&call.function.arguments);
                }
            }
        }
        result
    }

    async fn stream_openai<F>(
        &self,
        messages: &[ChatMessage],
        route: &ModelRoute,
        tools: &[ToolDefinition],
        cancel: &AtomicBool,
        mut on_delta: F,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(StreamDelta<'_>) + Send,
    {
        if cancel.load(Ordering::Relaxed) {
            return Err(ProviderError::Cancelled);
        }

        let model = route.model.as_str();
        let wire_messages: Vec<Value> = messages.iter().map(ChatMessage::to_wire).collect();
        let mut body = json!({
            "model": model,
            "messages": wire_messages,
            "stream": true,
        });
        if let Some(effort) = &route.reasoning_effort {
            body["reasoning_effort"] = json!(effort);
        }
        if !tools.is_empty() {
            body["tools"] = serde_json::to_value(tools)
                .map_err(|e| ProviderError::Config(format!("cannot encode tools: {e}")))?;
            body["tool_choice"] = json!("auto");
        }
        let mut request = self
            .client
            .post(&route.endpoint)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);
        if let Some(secret) = self.auth_secret(route).await {
            request = apply_auth(request, route.kind, secret);
        }
        let response = send_request(request, cancel).await?;

        let is_sse = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("text/event-stream"))
            .unwrap_or(false);

        if !is_sse {
            return self.non_streaming(response, model, cancel, on_delta).await;
        }

        let mut decoder = SseDecoder::new();
        let mut acc = Accumulator::new(model);
        let mut stream = response.bytes_stream();
        loop {
            // A provider can send headers and one event, then stop writing.
            // Poll the flag while waiting for the next chunk so cancel does not
            // depend on the server closing the connection.
            let next = tokio::select! {
                next = stream.next() => next,
                _ = wait_for_cancel(cancel) => return Err(ProviderError::Cancelled),
            };
            let Some(chunk) = next else {
                break;
            };
            if cancel.load(Ordering::Relaxed) {
                return Err(ProviderError::Cancelled);
            }
            let chunk = chunk.map_err(|e| ProviderError::Http(e.to_string()))?;
            for data in decoder.feed(&chunk) {
                if apply_data(&data, &mut acc, &mut on_delta)? {
                    return Ok(acc.finish());
                }
            }
        }
        for data in decoder.finish() {
            if apply_data(&data, &mut acc, &mut on_delta)? {
                return Ok(acc.finish());
            }
        }

        if !acc.saw_event {
            return Err(ProviderError::Decode(
                "stream ended without any events".to_string(),
            ));
        }
        // Reaching this point means no `[DONE]` was seen. A clean EOF without
        // a finish reason usually means the connection dropped mid-answer;
        // treat it as retryable instead of persisting a truncated response.
        if acc.finish_reason.is_none() {
            return Err(ProviderError::Decode(
                "stream ended before a finish_reason or [DONE]".to_string(),
            ));
        }
        Ok(acc.finish())
    }

    async fn non_streaming<F>(
        &self,
        response: reqwest::Response,
        model: &str,
        cancel: &AtomicBool,
        mut on_delta: F,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(StreamDelta<'_>),
    {
        let text = tokio::select! {
            text = response.text() => text.map_err(|e| ProviderError::Http(e.to_string()))?,
            _ = wait_for_cancel(cancel) => return Err(ProviderError::Cancelled),
        };
        let value: Value = serde_json::from_str(&text)
            .map_err(|e| ProviderError::Decode(format!("non-streaming response: {e}")))?;
        if let Some(error) = value.get("error") {
            return Err(ProviderError::Api {
                status: 200,
                body: error_message(error),
                retry_after_secs: None,
            });
        }
        let content = value
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let reasoning = value
            .pointer("/choices/0/message")
            .and_then(extract_reasoning)
            .unwrap_or_default();
        if !reasoning.is_empty() {
            on_delta(StreamDelta::Reasoning(&reasoning));
        }
        if !content.is_empty() {
            on_delta(StreamDelta::Content(&content));
        }
        let tool_calls: Vec<ToolCall> = value
            .pointer("/choices/0/message/tool_calls")
            .and_then(|calls| serde_json::from_value(calls.clone()).ok())
            .unwrap_or_default();
        Ok(Completion {
            text: content,
            reasoning,
            model: value
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(model)
                .to_string(),
            usage: value
                .get("usage")
                .and_then(|u| serde_json::from_value(u.clone()).ok()),
            finish_reason: value
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                .map(str::to_string),
            tool_calls,
        })
    }

    /// Resolve the credential for one request: config/env first, then the
    /// owner-only credential store.
    async fn auth_secret(&self, route: &ModelRoute) -> Option<crate::auth::Secret> {
        if let Some(key) = route
            .api_key
            .as_deref()
            .map(str::trim)
            .filter(|k| !k.is_empty())
        {
            return Some(crate::auth::Secret::ApiKey(key.to_string()));
        }
        self.credentials.secret(&route.provider).await
    }

    async fn stream_anthropic<F>(
        &self,
        messages: &[ChatMessage],
        route: &ModelRoute,
        tools: &[ToolDefinition],
        cancel: &AtomicBool,
        mut on_delta: F,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(StreamDelta<'_>) + Send,
    {
        if cancel.load(Ordering::Relaxed) {
            return Err(ProviderError::Cancelled);
        }
        let model = route.model.as_str();
        let body = anthropic_body(messages, model, tools, self.max_output_tokens)?;
        let mut request = self
            .client
            .post(&route.endpoint)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        if let Some(secret) = self.auth_secret(route).await {
            request = apply_auth(request, route.kind, secret);
        }
        let response = send_request(request, cancel).await?;

        let is_sse = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("text/event-stream"))
            .unwrap_or(false);
        if !is_sse {
            let value = response_json(response, cancel).await?;
            let mut acc = AnthropicAccumulator::new(model);
            apply_anthropic_message(&value, &mut acc, &mut on_delta)?;
            acc.done = true;
            return Ok(acc.finish());
        }

        let mut decoder = SseDecoder::new();
        let mut acc = AnthropicAccumulator::new(model);
        let mut stream = response.bytes_stream();
        loop {
            let next = tokio::select! {
                next = stream.next() => next,
                _ = wait_for_cancel(cancel) => return Err(ProviderError::Cancelled),
            };
            let Some(chunk) = next else {
                break;
            };
            if cancel.load(Ordering::Relaxed) {
                return Err(ProviderError::Cancelled);
            }
            let chunk = chunk.map_err(|e| ProviderError::Http(e.to_string()))?;
            for data in decoder.feed(&chunk) {
                if apply_anthropic_event(&data, &mut acc, &mut on_delta)? {
                    return Ok(acc.finish());
                }
            }
        }
        for data in decoder.finish() {
            if apply_anthropic_event(&data, &mut acc, &mut on_delta)? {
                return Ok(acc.finish());
            }
        }
        if !acc.saw_event {
            return Err(ProviderError::Decode(
                "stream ended without any events".to_string(),
            ));
        }
        if !acc.done && acc.finish_reason.is_none() {
            return Err(ProviderError::Decode(
                "stream ended before a stop reason".to_string(),
            ));
        }
        Ok(acc.finish())
    }

    async fn stream_gemini<F>(
        &self,
        messages: &[ChatMessage],
        route: &ModelRoute,
        tools: &[ToolDefinition],
        cancel: &AtomicBool,
        mut on_delta: F,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(StreamDelta<'_>) + Send,
    {
        if cancel.load(Ordering::Relaxed) {
            return Err(ProviderError::Cancelled);
        }
        let model = route.model.as_str();
        let body = gemini_body(
            messages,
            model,
            tools,
            self.max_output_tokens,
            route.reasoning_effort.as_deref(),
        )?;
        let url = gemini_stream_endpoint(&route.endpoint, model);
        let mut request = self
            .client
            .post(&url)
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(&body);
        if let Some(secret) = self.auth_secret(route).await {
            request = apply_auth(request, route.kind, secret);
        }
        let response = send_request(request, cancel).await?;

        let is_sse = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.contains("text/event-stream"))
            .unwrap_or(false);
        if !is_sse {
            let value = response_json(response, cancel).await?;
            let mut acc = GeminiAccumulator::new(model);
            match &value {
                Value::Array(items) => {
                    for item in items {
                        apply_gemini_event_value(item, &mut acc, &mut on_delta)?;
                    }
                }
                item => apply_gemini_event_value(item, &mut acc, &mut on_delta)?,
            }
            return Ok(acc.finish());
        }

        let mut decoder = SseDecoder::new();
        let mut acc = GeminiAccumulator::new(model);
        let mut stream = response.bytes_stream();
        loop {
            let next = tokio::select! {
                next = stream.next() => next,
                _ = wait_for_cancel(cancel) => return Err(ProviderError::Cancelled),
            };
            let Some(chunk) = next else {
                break;
            };
            if cancel.load(Ordering::Relaxed) {
                return Err(ProviderError::Cancelled);
            }
            let chunk = chunk.map_err(|e| ProviderError::Http(e.to_string()))?;
            for data in decoder.feed(&chunk) {
                if apply_gemini_event(&data, &mut acc, &mut on_delta)? {
                    return Ok(acc.finish());
                }
            }
        }
        for data in decoder.finish() {
            if apply_gemini_event(&data, &mut acc, &mut on_delta)? {
                return Ok(acc.finish());
            }
        }
        if !acc.saw_event {
            return Err(ProviderError::Decode(
                "stream ended without any events".to_string(),
            ));
        }
        if acc.finish_reason.is_none() {
            return Err(ProviderError::Decode(
                "stream ended before a finish reason".to_string(),
            ));
        }
        Ok(acc.finish())
    }
}

/// Send with cancellation and map non-2xx responses to `ProviderError::Api`.
async fn send_request(
    request: reqwest::RequestBuilder,
    cancel: &AtomicBool,
) -> Result<reqwest::Response, ProviderError> {
    // Waiting for the first byte can outlast the user's patience. Poll the
    // turn flag and drop the request future on cancel; reqwest then closes
    // the connection instead of leaving the turn stuck in `sending`.
    let response = tokio::select! {
        result = request.send() => result.map_err(|e| {
            if cancel.load(Ordering::Relaxed) {
                ProviderError::Cancelled
            } else {
                ProviderError::Http(e.to_string())
            }
        })?,
        _ = wait_for_cancel(cancel) => return Err(ProviderError::Cancelled),
    };
    let status = response.status();
    if !status.is_success() {
        let retry_after_secs = parse_retry_after(response.headers());
        let body = response.text().await.unwrap_or_default();
        return Err(ProviderError::Api {
            status: status.as_u16(),
            body: truncate(&body, 2048),
            retry_after_secs,
        });
    }
    Ok(response)
}

/// Attach a provider-specific credential header.
fn apply_auth(
    request: reqwest::RequestBuilder,
    kind: ProviderKind,
    secret: crate::auth::Secret,
) -> reqwest::RequestBuilder {
    use crate::auth::Secret;
    match (kind, secret) {
        (ProviderKind::Anthropic, Secret::ApiKey(key)) => request
            .header("x-api-key", key)
            .header("anthropic-version", "2023-06-01"),
        (ProviderKind::Gemini, Secret::ApiKey(key)) => request.header("x-goog-api-key", key),
        (_, Secret::ApiKey(key)) | (_, Secret::Bearer(key)) => request.bearer_auth(key),
    }
}

async fn response_json(
    response: reqwest::Response,
    cancel: &AtomicBool,
) -> Result<Value, ProviderError> {
    let text = tokio::select! {
        text = response.text() => text.map_err(|e| ProviderError::Http(e.to_string()))?,
        _ = wait_for_cancel(cancel) => return Err(ProviderError::Cancelled),
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|e| ProviderError::Decode(format!("non-streaming response: {e}")))?;
    if let Some(error) = value.get("error") {
        return Err(ProviderError::Api {
            status: 200,
            body: error_message(error),
            retry_after_secs: None,
        });
    }
    Ok(value)
}

struct Accumulator {
    text: String,
    reasoning: String,
    model: String,
    usage: Option<Usage>,
    finish_reason: Option<String>,
    saw_event: bool,
    tool_calls: Vec<PartialToolCall>,
}

#[derive(Default)]
struct PartialToolCall {
    id: String,
    kind: String,
    name: String,
    arguments: String,
}

impl Accumulator {
    fn new(model: &str) -> Self {
        Self {
            text: String::new(),
            reasoning: String::new(),
            model: model.to_string(),
            usage: None,
            finish_reason: None,
            saw_event: false,
            tool_calls: Vec::new(),
        }
    }

    fn finish(self) -> Completion {
        let tool_calls = self
            .tool_calls
            .into_iter()
            .filter(|call| !call.name.is_empty())
            .enumerate()
            .map(|(index, call)| ToolCall {
                id: if call.id.is_empty() {
                    format!("call_{index}")
                } else {
                    call.id
                },
                kind: if call.kind.is_empty() {
                    "function".to_string()
                } else {
                    call.kind
                },
                function: FunctionCall {
                    name: call.name,
                    arguments: call.arguments,
                },
            })
            .collect();
        Completion {
            text: self.text,
            reasoning: self.reasoning,
            model: self.model,
            usage: self.usage,
            finish_reason: self.finish_reason,
            tool_calls,
        }
    }
}

// ---------------------------------------------------------------------------
// Anthropic Messages API
// ---------------------------------------------------------------------------

/// Convert the provider-agnostic transcript into an Anthropic Messages body.
fn anthropic_body(
    messages: &[ChatMessage],
    model: &str,
    tools: &[ToolDefinition],
    max_tokens: u32,
) -> Result<Value, ProviderError> {
    let mut system = Vec::new();
    let mut wire: Vec<Value> = Vec::new();
    for message in messages {
        match message.role {
            Role::System => {
                if !message.content.trim().is_empty() {
                    system.push(message.content.clone());
                }
            }
            Role::User => {
                let mut blocks = Vec::new();
                if !message.content.is_empty() {
                    blocks.push(json!({"type": "text", "text": message.content}));
                }
                for image in &message.images {
                    blocks.push(json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": image.mime,
                            "data": image.data,
                        }
                    }));
                }
                push_anthropic(&mut wire, "user", blocks);
            }
            Role::Assistant => {
                let mut blocks = Vec::new();
                if !message.content.is_empty() {
                    blocks.push(json!({"type": "text", "text": message.content}));
                }
                for call in message.tool_calls.iter().flatten() {
                    let input = serde_json::from_str(&call.function.arguments)
                        .unwrap_or_else(|_| json!({}));
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": call.id,
                        "name": call.function.name,
                        "input": input,
                    }));
                }
                // Anthropic rejects an assistant turn with no blocks.
                if blocks.is_empty() {
                    blocks.push(json!({"type": "text", "text": ""}));
                }
                push_anthropic(&mut wire, "assistant", blocks);
            }
            Role::Tool => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": message.tool_call_id.clone().unwrap_or_default(),
                    "content": message.content,
                });
                push_anthropic(&mut wire, "user", vec![block]);
            }
        }
    }

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": wire,
        "stream": true,
    });
    if !system.is_empty() {
        body["system"] = json!(system.join("\n\n"));
    }
    if !tools.is_empty() {
        body["tools"] = json!(tools
            .iter()
            .map(|tool| json!({
                "name": tool.function.name,
                "description": tool.function.description,
                "input_schema": tool.function.parameters,
            }))
            .collect::<Vec<_>>());
        body["tool_choice"] = json!({"type": "auto"});
    }
    Ok(body)
}

/// Append blocks, merging consecutive messages with the same role. Anthropic
/// requires every `tool_result` for one assistant turn to share a user message.
fn push_anthropic(wire: &mut Vec<Value>, role: &str, blocks: Vec<Value>) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = wire.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(content) = last.get_mut("content").and_then(Value::as_array_mut) {
                content.extend(blocks);
                return;
            }
        }
    }
    wire.push(json!({"role": role, "content": blocks}));
}

struct AnthropicAccumulator {
    text: String,
    reasoning: String,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    finish_reason: Option<String>,
    saw_event: bool,
    done: bool,
    tool_calls: Vec<PartialToolCall>,
}

impl AnthropicAccumulator {
    fn new(model: &str) -> Self {
        Self {
            text: String::new(),
            reasoning: String::new(),
            model: model.to_string(),
            input_tokens: 0,
            output_tokens: 0,
            finish_reason: None,
            saw_event: false,
            done: false,
            tool_calls: Vec::new(),
        }
    }

    fn set_usage(&mut self, usage: &Value) {
        if let Some(tokens) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.input_tokens = tokens;
        }
        if let Some(tokens) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output_tokens = tokens;
        }
    }

    fn finish(self) -> Completion {
        let tool_calls = finish_tool_calls(self.tool_calls);
        let usage = (self.input_tokens > 0 || self.output_tokens > 0).then_some(Usage {
            prompt_tokens: self.input_tokens,
            completion_tokens: self.output_tokens,
            total_tokens: self.input_tokens.saturating_add(self.output_tokens),
        });
        Completion {
            text: self.text,
            reasoning: self.reasoning,
            model: self.model,
            usage,
            finish_reason: self.finish_reason,
            tool_calls,
        }
    }
}

fn anthropic_stop_reason(reason: &str) -> String {
    match reason {
        "end_turn" | "stop_sequence" => "stop".to_string(),
        "max_tokens" => "length".to_string(),
        "tool_use" => "tool_calls".to_string(),
        "refusal" => "content_filter".to_string(),
        other => other.to_string(),
    }
}

fn apply_anthropic_event<F>(
    data: &str,
    acc: &mut AnthropicAccumulator,
    on_delta: &mut F,
) -> Result<bool, ProviderError>
where
    F: FnMut(StreamDelta<'_>),
{
    let data = data.trim();
    if data.is_empty() {
        return Ok(false);
    }
    let value: Value = serde_json::from_str(data)
        .map_err(|e| ProviderError::Decode(format!("{e}: {}", truncate(data, 512))))?;
    acc.saw_event = true;

    match value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "error" => {
            let error = value.get("error").unwrap_or(&value);
            return Err(ProviderError::Api {
                status: 200,
                body: error_message(error),
                retry_after_secs: None,
            });
        }
        "message_start" => {
            if let Some(message) = value.get("message") {
                if let Some(model) = message.get("model").and_then(Value::as_str) {
                    acc.model = model.to_string();
                }
                if let Some(usage) = message.get("usage") {
                    acc.set_usage(usage);
                }
            }
        }
        "content_block_start" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let block = value.get("content_block").unwrap_or(&Value::Null);
            if block.get("type").and_then(Value::as_str) == Some("tool_use") {
                ensure_tool_slot(acc, index);
                let slot = &mut acc.tool_calls[index];
                slot.id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                slot.kind = "function".to_string();
                slot.name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
            }
        }
        "content_block_delta" => {
            let index = value.get("index").and_then(Value::as_u64).unwrap_or(0) as usize;
            let delta = value.get("delta").unwrap_or(&Value::Null);
            match delta
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "text_delta" => {
                    if let Some(text) = delta.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            acc.text.push_str(text);
                            on_delta(StreamDelta::Content(text));
                        }
                    }
                }
                "thinking_delta" => {
                    if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                        if !text.is_empty() {
                            acc.reasoning.push_str(text);
                            on_delta(StreamDelta::Reasoning(text));
                        }
                    }
                }
                "input_json_delta" => {
                    if let Some(partial) = delta.get("partial_json").and_then(Value::as_str) {
                        ensure_tool_slot(acc, index);
                        acc.tool_calls[index].arguments.push_str(partial);
                    }
                }
                _ => {}
            }
        }
        "message_delta" => {
            if let Some(usage) = value.get("usage") {
                acc.set_usage(usage);
            }
            if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                acc.finish_reason = Some(anthropic_stop_reason(reason));
            }
        }
        "message_stop" => {
            acc.done = true;
            return Ok(true);
        }
        _ => {}
    }
    Ok(false)
}

/// Apply a complete (non-streaming) Anthropic response.
fn apply_anthropic_message<F>(
    value: &Value,
    acc: &mut AnthropicAccumulator,
    on_delta: &mut F,
) -> Result<(), ProviderError>
where
    F: FnMut(StreamDelta<'_>),
{
    if let Some(model) = value.get("model").and_then(Value::as_str) {
        acc.model = model.to_string();
    }
    if let Some(usage) = value.get("usage") {
        acc.set_usage(usage);
    }
    for block in value
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "text" => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    acc.text.push_str(text);
                    on_delta(StreamDelta::Content(text));
                }
            }
            "thinking" => {
                if let Some(text) = block.get("thinking").and_then(Value::as_str) {
                    acc.reasoning.push_str(text);
                    on_delta(StreamDelta::Reasoning(text));
                }
            }
            "tool_use" => {
                let index = acc.tool_calls.len();
                ensure_tool_slot(acc, index);
                let slot = &mut acc.tool_calls[index];
                slot.id = block
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                slot.kind = "function".to_string();
                slot.name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                slot.arguments = block
                    .get("input")
                    .map(|input| input.to_string())
                    .unwrap_or_else(|| "{}".to_string());
            }
            _ => {}
        }
    }
    if let Some(reason) = value.get("stop_reason").and_then(Value::as_str) {
        acc.finish_reason = Some(anthropic_stop_reason(reason));
    }
    Ok(())
}

fn ensure_tool_slot(acc: &mut AnthropicAccumulator, index: usize) {
    if acc.tool_calls.len() <= index {
        acc.tool_calls
            .resize_with(index + 1, PartialToolCall::default);
    }
}

// ---------------------------------------------------------------------------
// Gemini generateContent API
// ---------------------------------------------------------------------------

fn gemini_stream_endpoint(base: &str, model: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.contains(":streamGenerateContent") {
        let separator = if base.contains('?') { '&' } else { '?' };
        format!("{base}{separator}alt=sse")
    } else {
        format!("{base}/models/{model}:streamGenerateContent?alt=sse")
    }
}

fn gemini_body(
    messages: &[ChatMessage],
    model: &str,
    tools: &[ToolDefinition],
    max_tokens: u32,
    reasoning_effort: Option<&str>,
) -> Result<Value, ProviderError> {
    let mut system = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    let mut tool_names = std::collections::HashMap::new();
    for message in messages {
        match message.role {
            Role::System => {
                if !message.content.trim().is_empty() {
                    system.push(message.content.clone());
                }
            }
            Role::User => {
                let mut parts = Vec::new();
                if !message.content.is_empty() {
                    parts.push(json!({"text": message.content}));
                }
                for image in &message.images {
                    parts.push(json!({
                        "inlineData": {"mimeType": image.mime, "data": image.data}
                    }));
                }
                push_gemini(&mut contents, "user", parts);
            }
            Role::Assistant => {
                let mut parts = Vec::new();
                if !message.content.is_empty() {
                    parts.push(json!({"text": message.content}));
                }
                for call in message.tool_calls.iter().flatten() {
                    tool_names.insert(call.id.clone(), call.function.name.clone());
                    let args = serde_json::from_str(&call.function.arguments)
                        .unwrap_or_else(|_| json!({"_raw": call.function.arguments}));
                    parts.push(json!({
                        "functionCall": {"name": call.function.name, "args": args}
                    }));
                }
                push_gemini(&mut contents, "model", parts);
            }
            Role::Tool => {
                let name = message
                    .tool_call_id
                    .as_deref()
                    .and_then(|id| tool_names.get(id))
                    .cloned()
                    .unwrap_or_else(|| "tool".to_string());
                let part = json!({
                    "functionResponse": {
                        "name": name,
                        "response": {"result": message.content},
                    }
                });
                push_gemini(&mut contents, "user", vec![part]);
            }
        }
    }

    let mut body = json!({
        "contents": contents,
        "generationConfig": {"maxOutputTokens": max_tokens},
    });
    if !system.is_empty() {
        body["systemInstruction"] = json!({"parts": [{"text": system.join("\n\n")}]});
    }
    if !tools.is_empty() {
        body["tools"] = json!([{
            "functionDeclarations": tools.iter().map(|tool| json!({
                "name": tool.function.name,
                "description": tool.function.description,
                "parameters": gemini_schema(&tool.function.parameters),
            })).collect::<Vec<_>>()
        }]);
    }
    if model.contains("2.5") || model.to_ascii_lowercase().contains("thinking") {
        let budget = match reasoning_effort {
            Some("none") => Some(0),
            Some("low") => Some(1_024),
            Some("high") => Some(8_192),
            Some("medium") | Some(_) => Some(4_096),
            None => None,
        };
        if let Some(budget) = budget {
            body["generationConfig"]["thinkingConfig"] = json!({"thinkingBudget": budget});
        }
    }
    Ok(body)
}

fn push_gemini(contents: &mut Vec<Value>, role: &str, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    if let Some(last) = contents.last_mut() {
        if last.get("role").and_then(Value::as_str) == Some(role) {
            if let Some(existing) = last.get_mut("parts").and_then(Value::as_array_mut) {
                existing.extend(parts);
                return;
            }
        }
    }
    contents.push(json!({"role": role, "parts": parts}));
}

/// Gemini accepts an OpenAPI subset; drop JSON Schema keywords it rejects.
fn gemini_schema(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, value) in map {
                if matches!(
                    key.as_str(),
                    "additionalProperties" | "$schema" | "$defs" | "definitions" | "strict"
                ) {
                    continue;
                }
                out.insert(key.clone(), gemini_schema(value));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(gemini_schema).collect()),
        other => other.clone(),
    }
}

struct GeminiAccumulator {
    text: String,
    reasoning: String,
    model: String,
    usage: Option<Usage>,
    finish_reason: Option<String>,
    saw_event: bool,
    tool_calls: Vec<PartialToolCall>,
}

impl GeminiAccumulator {
    fn new(model: &str) -> Self {
        Self {
            text: String::new(),
            reasoning: String::new(),
            model: model.to_string(),
            usage: None,
            finish_reason: None,
            saw_event: false,
            tool_calls: Vec::new(),
        }
    }

    fn set_usage(&mut self, usage: &Value) {
        let prompt = usage
            .get("promptTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let completion = usage
            .get("candidatesTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let total = usage
            .get("totalTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(prompt.saturating_add(completion));
        if prompt > 0 || completion > 0 || total > 0 {
            self.usage = Some(Usage {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: total,
            });
        }
    }

    fn finish(self) -> Completion {
        Completion {
            text: self.text,
            reasoning: self.reasoning,
            model: self.model,
            usage: self.usage,
            finish_reason: self.finish_reason,
            tool_calls: finish_tool_calls(self.tool_calls),
        }
    }
}

fn gemini_finish_reason(reason: &str) -> String {
    match reason {
        "STOP" => "stop".to_string(),
        "MAX_TOKENS" => "length".to_string(),
        "SAFETY" | "RECITATION" | "BLOCKLIST" | "PROHIBITED_CONTENT" | "SPII" => {
            "content_filter".to_string()
        }
        "MALFORMED_FUNCTION_CALL" => "tool_calls".to_string(),
        other => other.to_ascii_lowercase(),
    }
}

fn apply_gemini_event<F>(
    data: &str,
    acc: &mut GeminiAccumulator,
    on_delta: &mut F,
) -> Result<bool, ProviderError>
where
    F: FnMut(StreamDelta<'_>),
{
    let data = data.trim();
    if data.is_empty() {
        return Ok(false);
    }
    if data == "[DONE]" {
        return Ok(true);
    }
    let value: Value = serde_json::from_str(data)
        .map_err(|e| ProviderError::Decode(format!("{e}: {}", truncate(data, 512))))?;
    apply_gemini_event_value(&value, acc, on_delta)?;
    Ok(false)
}

fn apply_gemini_event_value<F>(
    value: &Value,
    acc: &mut GeminiAccumulator,
    on_delta: &mut F,
) -> Result<(), ProviderError>
where
    F: FnMut(StreamDelta<'_>),
{
    if value.is_null() {
        return Ok(());
    }
    acc.saw_event = true;
    if let Some(error) = value.get("error") {
        return Err(ProviderError::Api {
            status: 200,
            body: error_message(error),
            retry_after_secs: None,
        });
    }
    if let Some(model) = value.get("modelVersion").and_then(Value::as_str) {
        acc.model = model.to_string();
    }
    if let Some(usage) = value.get("usageMetadata") {
        acc.set_usage(usage);
    }
    if let Some(block) = value
        .get("promptFeedback")
        .and_then(|v| v.get("blockReason"))
    {
        acc.finish_reason = Some("content_filter".to_string());
        return Err(ProviderError::Api {
            status: 200,
            body: format!("blocked by provider: {block}"),
            retry_after_secs: None,
        });
    }
    let Some(candidate) = value
        .get("candidates")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
    else {
        return Ok(());
    };
    if let Some(parts) = candidate
        .pointer("/content/parts")
        .and_then(Value::as_array)
    {
        for part in parts {
            if let Some(call) = part.get("functionCall") {
                let name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if name.is_empty() {
                    continue;
                }
                let arguments = call
                    .get("args")
                    .map(|args| args.to_string())
                    .unwrap_or_else(|| "{}".to_string());
                let index = acc.tool_calls.len();
                acc.tool_calls.push(PartialToolCall {
                    id: format!("call_{index}"),
                    kind: "function".to_string(),
                    name,
                    arguments,
                });
            }
            if let Some(text) = part.get("text").and_then(Value::as_str) {
                if text.is_empty() {
                    continue;
                }
                if part
                    .get("thought")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    acc.reasoning.push_str(text);
                    on_delta(StreamDelta::Reasoning(text));
                } else {
                    acc.text.push_str(text);
                    on_delta(StreamDelta::Content(text));
                }
            }
        }
    }
    if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
        acc.finish_reason = Some(gemini_finish_reason(reason));
    }
    Ok(())
}

fn finish_tool_calls(calls: Vec<PartialToolCall>) -> Vec<ToolCall> {
    calls
        .into_iter()
        .filter(|call| !call.name.is_empty())
        .enumerate()
        .map(|(index, call)| ToolCall {
            id: if call.id.is_empty() {
                format!("call_{index}")
            } else {
                call.id
            },
            kind: if call.kind.is_empty() {
                "function".to_string()
            } else {
                call.kind
            },
            function: FunctionCall {
                name: call.name,
                arguments: call.arguments,
            },
        })
        .collect()
}

/// Apply one SSE `data:` payload. Returns `true` when the stream is done.
fn apply_data<F>(data: &str, acc: &mut Accumulator, on_delta: &mut F) -> Result<bool, ProviderError>
where
    F: FnMut(StreamDelta<'_>),
{
    let data = data.trim();
    if data.is_empty() {
        return Ok(false);
    }
    if data == "[DONE]" {
        return Ok(true);
    }

    let value: Value = serde_json::from_str(data)
        .map_err(|e| ProviderError::Decode(format!("{e}: {}", truncate(data, 512))))?;
    acc.saw_event = true;

    if let Some(error) = value.get("error") {
        return Err(ProviderError::Api {
            status: 200,
            body: error_message(error),
            retry_after_secs: None,
        });
    }
    if let Some(model) = value.get("model").and_then(Value::as_str) {
        acc.model = model.to_string();
    }
    if let Some(usage) = value.get("usage").filter(|u| !u.is_null()) {
        acc.usage = serde_json::from_value(usage.clone()).ok();
    }
    if let Some(choice) = value.pointer("/choices/0") {
        if let Some(reasoning) = choice
            .pointer("/delta")
            .and_then(extract_reasoning)
            .filter(|text| !text.is_empty())
        {
            acc.reasoning.push_str(&reasoning);
            on_delta(StreamDelta::Reasoning(&reasoning));
        }
        if let Some(delta) = extract_content(choice.pointer("/delta/content")) {
            if !delta.is_empty() {
                acc.text.push_str(&delta);
                on_delta(StreamDelta::Content(&delta));
            }
        }
        if let Some(calls) = choice
            .pointer("/delta/tool_calls")
            .and_then(Value::as_array)
        {
            for call in calls {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(acc.tool_calls.len() as u64) as usize;
                if acc.tool_calls.len() <= index {
                    acc.tool_calls
                        .resize_with(index + 1, PartialToolCall::default);
                }
                let slot = &mut acc.tool_calls[index];
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    slot.id = id.to_string();
                }
                if let Some(kind) = call.get("type").and_then(Value::as_str) {
                    slot.kind = kind.to_string();
                }
                if let Some(function) = call.get("function") {
                    if let Some(name) = function.get("name").and_then(Value::as_str) {
                        slot.name.push_str(name);
                    }
                    if let Some(arguments) = function.get("arguments").and_then(Value::as_str) {
                        slot.arguments.push_str(arguments);
                    }
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            acc.finish_reason = Some(reason.to_string());
        }
    }
    Ok(false)
}

/// Reasoning-capable providers expose the chain of thought under different
/// keys: DeepSeek uses `reasoning_content`, OpenRouter/vLLM use `reasoning`
/// (sometimes `thinking`). Accept any of them.
fn extract_reasoning(value: &Value) -> Option<String> {
    for key in ["reasoning_content", "reasoning", "thinking"] {
        if let Some(text) = extract_content(value.get(key)) {
            return Some(text);
        }
    }
    None
}

/// OpenAI sends `content` as a string; newer multimodal APIs use an array of
/// `{type, text}` parts. Accept both.
fn extract_content(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                } else if let Some(text) = part.as_str() {
                    out.push_str(text);
                }
            }
            Some(out)
        }
        _ => None,
    }
}

fn error_message(error: &Value) -> String {
    error
        .get("message")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| error.to_string())
}

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &text[..end])
}

fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    if let Some(secs) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|secs| secs.is_finite() && *secs >= 0.0)
    {
        return Some(secs.ceil() as u64);
    }
    headers
        .get("retry-after-ms")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|ms| ms.saturating_add(999) / 1_000)
}

fn looks_like_quota(body: &str) -> bool {
    [
        "insufficient_quota",
        "insufficient quota",
        "exceeded your current quota",
        "quota exceeded",
        "quota has been exceeded",
        "billing",
        "payment required",
        "credit balance",
        "out of credits",
        "no credits",
        "余额不足",
        "配额",
        "套餐",
    ]
    .iter()
    .any(|needle| body.contains(needle))
}

fn looks_like_context_length(body: &str) -> bool {
    [
        "context_length_exceeded",
        "context length exceeded",
        "maximum context length",
        "max context length",
        "context window",
        "prompt is too long",
        "input is too long",
        "too many tokens",
        "request too large",
        "reduce the length of the messages",
        "上下文长度",
        "超出上下文",
    ]
    .iter()
    .any(|needle| body.contains(needle))
}

fn looks_like_transient(body: &str) -> bool {
    [
        "rate limit",
        "rate_limit",
        "too many requests",
        "overloaded",
        "temporarily",
        "try again",
        "timeout",
        "timed out",
        "capacity",
        "server error",
        "service unavailable",
    ]
    .iter()
    .any(|needle| body.contains(needle))
}

/// Incremental Server-Sent Events decoder.
///
/// Handles arbitrary chunk boundaries, CRLF/LF, comments, and multi-line
/// `data:` blocks (joined with `\n`, as the SSE spec requires).
pub struct SseDecoder {
    buf: Vec<u8>,
}

impl SseDecoder {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        let mut out = Vec::new();
        while let Some((end, sep_len)) = find_separator(&self.buf) {
            let block = self.buf[..end].to_vec();
            self.buf.drain(..end + sep_len);
            if let Some(data) = block_data(&block) {
                out.push(data);
            }
        }
        out
    }

    /// Flush a trailing block that was not terminated by a blank line.
    pub fn finish(&mut self) -> Vec<String> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        let block = std::mem::take(&mut self.buf);
        block_data(&block).into_iter().collect()
    }
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new()
    }
}

fn find_separator(buf: &[u8]) -> Option<(usize, usize)> {
    for i in 0..buf.len() {
        if buf[i] == b'\n' && buf.get(i + 1) == Some(&b'\n') {
            return Some((i, 2));
        }
        if buf[i] == b'\r'
            && buf.get(i + 1) == Some(&b'\n')
            && buf.get(i + 2) == Some(&b'\r')
            && buf.get(i + 3) == Some(&b'\n')
        {
            return Some((i, 4));
        }
    }
    None
}

fn block_data(block: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(block);
    let mut data = Vec::new();
    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if let Some(rest) = line.strip_prefix("data:") {
            data.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        }
    }
    if data.is_empty() {
        None
    } else {
        Some(data.join("\n"))
    }
}

async fn wait_for_cancel(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Relaxed) {
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Secret placeholders are restored only for local tools. Network, shell and
/// MCP tool arguments keep the placeholder (or fail closed in the daemon), so a
/// model cannot turn a redacted transcript value back into a live credential in
/// an outbound request.
fn restore_tool_arguments(tool: &str) -> bool {
    matches!(
        tool,
        "apply_patch"
            | "view_image"
            | "lsp_diagnostics"
            | "update_plan"
            | "request_user_input"
            | "tool_search"
            | "memory"
            | "use_skill"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoder_handles_split_chunks_and_crlf() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.feed(b"data: {\"a\":1}").is_empty());
        let out = decoder.feed(b"\r\n\r\ndata: [DONE]\r\n\r\n");
        assert_eq!(out, vec!["{\"a\":1}".to_string(), "[DONE]".to_string()]);
    }

    #[test]
    fn decoder_ignores_comments_and_joins_multiline_data() {
        let mut decoder = SseDecoder::new();
        let out = decoder.feed(b": ping\n\ndata: line1\ndata: line2\n\n");
        assert_eq!(out, vec!["line1\nline2".to_string()]);
    }

    #[test]
    fn decoder_flushes_trailing_block() {
        let mut decoder = SseDecoder::new();
        assert!(decoder.feed(b"data: tail").is_empty());
        assert_eq!(decoder.finish(), vec!["tail".to_string()]);
        assert!(decoder.finish().is_empty());
    }

    #[test]
    fn applies_delta_and_usage() {
        let mut acc = Accumulator::new("fallback");
        let mut seen = String::new();
        let mut on_delta = |delta: StreamDelta<'_>| {
            if let StreamDelta::Content(text) = delta {
                seen.push_str(text);
            }
        };

        let line = r#"{"model":"m1","choices":[{"delta":{"content":"Hel"}}]}"#;
        assert!(!apply_data(line, &mut acc, &mut on_delta).unwrap());
        let line = r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#;
        assert!(!apply_data(line, &mut acc, &mut on_delta).unwrap());

        let done = acc.finish();
        assert_eq!(seen, "Hello");
        assert_eq!(done.text, "Hello");
        assert_eq!(done.model, "m1");
        assert_eq!(done.finish_reason.as_deref(), Some("stop"));
        assert_eq!(done.usage.unwrap().total_tokens, 3);
    }

    #[test]
    fn accumulates_streamed_reasoning() {
        let mut acc = Accumulator::new("m");
        let mut seen = String::new();
        let mut on_delta = |delta: StreamDelta<'_>| {
            if let StreamDelta::Reasoning(text) = delta {
                seen.push_str(text);
            }
        };
        let line = r#"{"choices":[{"delta":{"reasoning_content":"step "}}]}"#;
        apply_data(line, &mut acc, &mut on_delta).unwrap();
        let line = r#"{"choices":[{"delta":{"reasoning":"one","content":"answer"}}]}"#;
        apply_data(line, &mut acc, &mut on_delta).unwrap();
        let done = acc.finish();
        assert_eq!(seen, "step one");
        assert_eq!(done.reasoning, "step one");
        assert_eq!(done.text, "answer");
    }

    #[test]
    fn accumulates_streamed_tool_calls() {
        let mut acc = Accumulator::new("m");
        let mut on_delta = |_: StreamDelta<'_>| {};
        let first = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"exec_command","arguments":"{\"cm"}}]}}]}"#;
        let second = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"d\":\"cat a.rs\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        apply_data(first, &mut acc, &mut on_delta).unwrap();
        apply_data(second, &mut acc, &mut on_delta).unwrap();
        let done = acc.finish();
        assert_eq!(done.tool_calls.len(), 1);
        assert_eq!(done.tool_calls[0].id, "call_a");
        assert_eq!(done.tool_calls[0].function.name, "exec_command");
        assert_eq!(
            done.tool_calls[0].function.arguments,
            r#"{"cmd":"cat a.rs"}"#
        );
        assert_eq!(done.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn assembles_multiple_tool_calls_by_index() {
        let mut acc = Accumulator::new("m");
        let mut on_delta = |_: StreamDelta<'_>| {};
        let chunks = [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"exec_command","arguments":"{}"}},{"index":1,"id":"b","function":{"name":"apply_patch","arguments":"{}"}}]}}]}"#,
        ];
        for chunk in chunks {
            apply_data(chunk, &mut acc, &mut on_delta).unwrap();
        }
        let done = acc.finish();
        assert_eq!(done.tool_calls.len(), 2);
        assert_eq!(done.tool_calls[0].function.name, "exec_command");
        assert_eq!(done.tool_calls[1].function.name, "apply_patch");
    }

    #[test]
    fn accepts_content_parts() {
        let value = json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]);
        assert_eq!(extract_content(Some(&value)).as_deref(), Some("ab"));
    }

    #[test]
    fn surfaces_provider_errors() {
        let mut acc = Accumulator::new("m");
        let mut on_delta = |_: StreamDelta<'_>| {};
        let err = apply_data(
            r#"{"error":{"message":"bad key"}}"#,
            &mut acc,
            &mut on_delta,
        )
        .unwrap_err();
        assert!(err.to_string().contains("bad key"));
    }

    #[test]
    fn classifies_transient_provider_failures() {
        for status in [408, 409, 425, 429, 500, 502, 503, 504] {
            let err = ProviderError::Api {
                status,
                body: "temporary".into(),
                retry_after_secs: None,
            };
            assert!(
                err.retry_class().is_retryable(),
                "status {status} should retry"
            );
        }
        assert!(ProviderError::Http("connection reset".into())
            .retry_class()
            .is_retryable());
        assert!(ProviderError::Decode("truncated stream".into())
            .retry_class()
            .is_retryable());
    }

    #[test]
    fn classifies_context_window_failures_separately() {
        for body in [
            r#"{"error":{"code":"context_length_exceeded","message":"prompt is too long"}}"#,
            "maximum context length is 128000 tokens",
            "too many tokens in request",
        ] {
            let err = ProviderError::Api {
                status: 400,
                body: body.into(),
                retry_after_secs: None,
            };
            assert_eq!(err.retry_class(), RetryClass::ContextLength, "{body}");
            assert!(!err.retry_class().is_retryable());
        }

        // A generic 413 is also recoverable by compacting the prompt.
        let payload = ProviderError::Api {
            status: 413,
            body: "request entity too large".into(),
            retry_after_secs: None,
        };
        assert_eq!(payload.retry_class(), RetryClass::ContextLength);
    }

    #[test]
    fn retry_classes_expose_labels_and_backoff_floors() {
        assert_eq!(RetryClass::Transient.label(), "transient");
        assert_eq!(RetryClass::RateLimited.label(), "rate_limited");
        assert_eq!(
            RetryClass::RateLimited.backoff_floor(),
            Duration::from_millis(500)
        );
        assert_eq!(RetryClass::Transient.backoff_floor(), Duration::ZERO);
    }

    #[test]
    fn does_not_retry_quota_or_auth_failures() {
        for status in [400, 401, 403, 404, 422] {
            let err = ProviderError::Api {
                status,
                body: "bad request".into(),
                retry_after_secs: None,
            };
            assert_eq!(err.retry_class(), RetryClass::Fatal);
        }
        let quota = ProviderError::Api {
            status: 429,
            body: r#"{"error":{"type":"insufficient_quota","message":"You exceeded your current quota"}}"#.into(),
            retry_after_secs: Some(10),
        };
        assert_eq!(quota.retry_class(), RetryClass::Quota);
        assert_eq!(quota.retry_after(), Some(Duration::from_secs(10)));
    }

    #[test]
    fn parses_retry_after_seconds_and_milliseconds() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "3".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(3));

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("retry-after-ms", "1500".parse().unwrap());
        assert_eq!(parse_retry_after(&headers), Some(2));
    }

    #[test]
    fn secrets_are_only_restored_for_local_tool_arguments() {
        for tool in [
            "apply_patch",
            "view_image",
            "lsp_diagnostics",
            "update_plan",
            "request_user_input",
            "tool_search",
            "memory",
            "use_skill",
        ] {
            assert!(restore_tool_arguments(tool), "{tool}");
        }
        for tool in [
            "exec_command",
            "write_stdin",
            "web_fetch",
            "web_search",
            "spawn_agent",
            "mcp__github__create_issue",
        ] {
            assert!(!restore_tool_arguments(tool), "{tool}");
        }
    }

    fn tool_definition() -> ToolDefinition {
        ToolDefinition::function(
            "exec_command",
            "Run a command",
            json!({
                "type": "object",
                "properties": {"cmd": {"type": "string"}},
                "required": ["cmd"],
                "additionalProperties": false,
            }),
        )
    }

    #[test]
    fn anthropic_body_splits_system_and_merges_tool_results() {
        let call = ToolCall {
            id: "toolu_1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "exec_command".into(),
                arguments: r#"{"cmd":"cat a.rs"}"#.into(),
            },
        };
        let messages = vec![
            ChatMessage::system("be brief"),
            ChatMessage::user("read it"),
            ChatMessage::assistant_with_tools("", vec![call]),
            ChatMessage::tool("toolu_1", "contents"),
            ChatMessage::tool("toolu_2", "more"),
        ];
        let body = anthropic_body(&messages, "claude-sonnet", &[tool_definition()], 4096).unwrap();
        assert_eq!(body["system"], "be brief");
        assert_eq!(body["max_tokens"], 4096);
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["messages"][1]["role"], "assistant");
        assert_eq!(
            body["messages"][1]["content"][0]["type"],
            Value::String("tool_use".into())
        );
        assert_eq!(
            body["messages"][1]["content"][0]["input"]["cmd"],
            "cat a.rs"
        );
        // Consecutive tool results become one user message with two blocks.
        let results = body["messages"][2]["content"].as_array().unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 3);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["type"], "tool_result");
        assert_eq!(results[1]["tool_use_id"], "toolu_2");
        assert_eq!(body["tools"][0]["name"], "exec_command");
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn applies_anthropic_stream_events() {
        let mut acc = AnthropicAccumulator::new("fallback");
        let mut seen = String::new();
        let mut on_delta = |delta: StreamDelta<'_>| {
            if let StreamDelta::Content(text) = delta {
                seen.push_str(text);
            }
        };
        let events = [
            r#"{"type":"message_start","message":{"model":"claude-sonnet","usage":{"input_tokens":10,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi"}}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"exec_command","input":{}}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"cmd\":\"cat a.rs\"}"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":7}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let mut done = false;
        for event in events {
            done = apply_anthropic_event(event, &mut acc, &mut on_delta).unwrap();
        }
        assert!(done);
        let completion = acc.finish();
        assert_eq!(seen, "Hi");
        assert_eq!(completion.text, "Hi");
        assert_eq!(completion.model, "claude-sonnet");
        assert_eq!(completion.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(completion.tool_calls.len(), 1);
        assert_eq!(completion.tool_calls[0].id, "toolu_1");
        assert_eq!(
            completion.tool_calls[0].function.arguments,
            r#"{"cmd":"cat a.rs"}"#
        );
        let usage = completion.usage.unwrap();
        assert_eq!(usage.prompt_tokens, 10);
        assert_eq!(usage.completion_tokens, 7);
        assert_eq!(usage.total_tokens, 17);
    }

    #[test]
    fn applies_anthropic_non_streaming_blocks() {
        let mut acc = AnthropicAccumulator::new("m");
        let mut on_delta = |_: StreamDelta<'_>| {};
        apply_anthropic_message(
            &json!({
                "model": "claude",
                "content": [
                    {"type": "thinking", "thinking": "plan"},
                    {"type": "text", "text": "answer"},
                    {"type": "tool_use", "id": "toolu_1", "name": "exec_command", "input": {"cmd": "cat a.rs"}}
                ],
                "stop_reason": "tool_use",
                "usage": {"input_tokens": 2, "output_tokens": 3}
            }),
            &mut acc,
            &mut on_delta,
        )
        .unwrap();
        let completion = acc.finish();
        assert_eq!(completion.reasoning, "plan");
        assert_eq!(completion.text, "answer");
        assert_eq!(completion.tool_calls[0].function.name, "exec_command");
        assert_eq!(completion.usage.unwrap().total_tokens, 5);
    }

    #[test]
    fn gemini_body_converts_roles_tools_and_schema() {
        let call = ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "exec_command".into(),
                arguments: r#"{"cmd":"cat a.rs"}"#.into(),
            },
        };
        let messages = vec![
            ChatMessage::system("be brief"),
            ChatMessage::user("read it"),
            ChatMessage::assistant_with_tools("", vec![call]),
            ChatMessage::tool("call_1", "contents"),
        ];
        let body = gemini_body(
            &messages,
            "gemini-2.5-pro",
            &[tool_definition()],
            2048,
            Some("low"),
        )
        .unwrap();
        assert_eq!(body["systemInstruction"]["parts"][0]["text"], "be brief");
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["contents"][1]["role"], "model");
        assert_eq!(
            body["contents"][1]["parts"][0]["functionCall"]["name"],
            "exec_command"
        );
        assert_eq!(body["contents"][2]["role"], "user");
        assert_eq!(
            body["contents"][2]["parts"][0]["functionResponse"]["name"],
            "exec_command"
        );
        let parameters = &body["tools"][0]["functionDeclarations"][0]["parameters"];
        assert_eq!(parameters["type"], "object");
        assert!(parameters.get("additionalProperties").is_none());
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 2048);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            1024
        );
    }

    #[test]
    fn applies_gemini_stream_events() {
        let mut acc = GeminiAccumulator::new("fallback");
        let mut seen = String::new();
        let mut on_delta = |delta: StreamDelta<'_>| match delta {
            StreamDelta::Content(text) => seen.push_str(text),
            StreamDelta::Reasoning(_) => {}
        };
        let first = r#"{"candidates":[{"content":{"parts":[{"text":"think","thought":true},{"text":"Hel"}]},"index":0}],"modelVersion":"gemini-2.5-pro"}"#;
        let second = r#"{"candidates":[{"content":{"parts":[{"text":"lo"},{"functionCall":{"name":"exec_command","args":{"cmd":"cat a.rs"}}}]},"finishReason":"STOP","index":0}],"usageMetadata":{"promptTokenCount":3,"candidatesTokenCount":4,"totalTokenCount":7}}"#;
        apply_gemini_event(first, &mut acc, &mut on_delta).unwrap();
        apply_gemini_event(second, &mut acc, &mut on_delta).unwrap();
        let completion = acc.finish();
        assert_eq!(seen, "Hello");
        assert_eq!(completion.reasoning, "think");
        assert_eq!(completion.model, "gemini-2.5-pro");
        assert_eq!(completion.finish_reason.as_deref(), Some("stop"));
        assert_eq!(completion.tool_calls.len(), 1);
        assert_eq!(completion.tool_calls[0].function.name, "exec_command");
        assert_eq!(
            completion.tool_calls[0].function.arguments,
            r#"{"cmd":"cat a.rs"}"#
        );
        assert_eq!(completion.usage.unwrap().total_tokens, 7);
    }

    #[test]
    fn gemini_maps_blocked_and_length_finish_reasons() {
        assert_eq!(gemini_finish_reason("MAX_TOKENS"), "length");
        assert_eq!(gemini_finish_reason("SAFETY"), "content_filter");
        assert_eq!(gemini_finish_reason("STOP"), "stop");
    }
}
