//! OpenAI-compatible streaming provider.
//!
//! One adapter covers OpenAI, Ollama, vLLM, OpenRouter, LM Studio and friends:
//! POST `/chat/completions` with `stream: true` and parse SSE `data:` lines.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use eix_core::redact::SecretRedactor;
use eix_core::{
    ChatMessage, Config, FunctionCall, ImageAttachment, ModelRoute, ProviderKind, Role, ToolCall,
    ToolDefinition, Usage,
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
                // Gateways often wrap an upstream 4xx in a 502. Those requests
                // are deterministic (bad schema, missing key, unknown model),
                // so backing off just burns the retry budget.
                if looks_like_deterministic_client_error(&body)
                    || matches!(*status, 400 | 401 | 403 | 404 | 422)
                {
                    return RetryClass::Fatal;
                }
                if *status == 429 {
                    return RetryClass::RateLimited;
                }
                if matches!(*status, 408 | 409 | 425) || (500..=599).contains(status) {
                    return RetryClass::Transient;
                }
                // Some providers report overload/rate-limit errors inside a
                // 200 SSE payload. Treat those as transient too. This is the
                // common "stream interrupted" / "overloaded" path.
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

/// Parse an OpenAI-compatible usage object, including the nested cache and
/// reasoning details newer providers expose.
fn usage_from_openai(value: &Value) -> Option<Usage> {
    let prompt = value
        .get("prompt_tokens")
        .or_else(|| value.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let completion = value
        .get("completion_tokens")
        .or_else(|| value.get("output_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let total = value
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| prompt.saturating_add(completion));
    let cache_read = value
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(Value::as_u64)
        .or_else(|| value.get("cache_read_input_tokens").and_then(Value::as_u64))
        .unwrap_or_default();
    let cache_creation = value
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let reasoning = value
        .pointer("/completion_tokens_details/reasoning_tokens")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    (prompt > 0 || completion > 0 || total > 0).then_some(Usage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_creation,
        reasoning_tokens: reasoning,
    })
}

#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub text: String,
    /// Streamed chain-of-thought (`reasoning_content` / `reasoning`), when the
    /// provider exposes it. The daemon may replay it on routes that require
    /// thinking blocks for multi-turn tool use.
    pub reasoning: String,
    /// Opaque provider signature for the reasoning block (Anthropic/DeepSeek
    /// thinking). Replayed verbatim when the route supports it.
    pub reasoning_signature: Option<String>,
    pub model: String,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    /// Images returned by the provider: inline base64 or a remote URL.
    pub images: Vec<ImageAttachment>,
}

/// Bound provider images so one response cannot pin unbounded memory or bloat
/// the event log. Prompt attachments use `[images]` limits; provider output has
/// no config of its own.
const MAX_IMAGE_PARTS: usize = 8;
const MAX_INLINE_IMAGE_BASE64: usize = 24 * 1024 * 1024;

fn clip_images(mut images: Vec<ImageAttachment>) -> Vec<ImageAttachment> {
    images.truncate(MAX_IMAGE_PARTS);
    images
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
    pub async fn stream_chat<F, G>(
        &self,
        messages: &[ChatMessage],
        route: &ModelRoute,
        tools: &[ToolDefinition],
        cancel: &AtomicBool,
        mut on_delta: F,
        mut on_tool_call: G,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(StreamDelta<'_>) + Send,
        G: FnMut(ToolCall) + Send,
    {
        let mut redactor = SecretRedactor::new(self.redaction_salt.clone());
        let sanitized = redactor.redact_messages(messages);
        // Tool-call arguments are restored here, so the prefetch callback sees
        // the same bytes the caller will execute later.
        let mut emit_tool_call = |call: ToolCall| {
            let mut call = call;
            if restore_tool_arguments(&call.function.name) {
                call.function.arguments = redactor.restore_json(&call.function.arguments);
            }
            on_tool_call(call);
        };
        let mut result = match route.kind {
            ProviderKind::OpenAi => {
                self.stream_openai(
                    &sanitized,
                    route,
                    tools,
                    cancel,
                    &mut on_delta,
                    &mut emit_tool_call,
                )
                .await
            }
            ProviderKind::Anthropic | ProviderKind::DeepSeek => {
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

    async fn stream_openai<F, G>(
        &self,
        messages: &[ChatMessage],
        route: &ModelRoute,
        tools: &[ToolDefinition],
        cancel: &AtomicBool,
        mut on_delta: F,
        on_tool_call: &mut G,
    ) -> Result<Completion, ProviderError>
    where
        F: FnMut(StreamDelta<'_>) + Send,
        G: FnMut(ToolCall),
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
            // Ask OpenAI-compatible servers to include the final usage chunk.
            // Without it many gateways only report tokens for some steps, which
            // makes cache-hit and token accounting impossible.
            "stream_options": {"include_usage": true},
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
                if apply_data_with_tools(&data, &mut acc, &mut on_delta, on_tool_call)? {
                    return Ok(acc.finish());
                }
            }
        }
        for data in decoder.finish() {
            if apply_data_with_tools(&data, &mut acc, &mut on_delta, on_tool_call)? {
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
        let message = value.pointer("/choices/0/message");
        let content = message
            .and_then(|message| extract_content(message.get("content")))
            .unwrap_or_default();
        let images = message.map(openai_images).unwrap_or_default();
        let reasoning = message.and_then(extract_reasoning).unwrap_or_default();
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
            reasoning_signature: None,
            model: value
                .get("model")
                .and_then(Value::as_str)
                .unwrap_or(model)
                .to_string(),
            usage: value.get("usage").and_then(usage_from_openai),
            finish_reason: value
                .pointer("/choices/0/finish_reason")
                .and_then(Value::as_str)
                .map(str::to_string),
            tool_calls,
            images,
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
        let body = anthropic_body(messages, route, model, tools, self.max_output_tokens)?;
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
        (ProviderKind::Anthropic | ProviderKind::DeepSeek, Secret::ApiKey(key)) => request
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
    images: Vec<ImageAttachment>,
    /// Number of leading tool calls already handed to the prefetch callback.
    emitted: usize,
}

#[derive(Default, Clone)]
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
            images: Vec::new(),
            emitted: 0,
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
            reasoning_signature: None,
            model: self.model,
            usage: self.usage,
            finish_reason: self.finish_reason,
            tool_calls,
            images: clip_images(self.images),
        }
    }
}

// ---------------------------------------------------------------------------
// Anthropic Messages API
// ---------------------------------------------------------------------------

/// Convert the provider-agnostic transcript into an Anthropic Messages body.
fn anthropic_body(
    messages: &[ChatMessage],
    route: &ModelRoute,
    model: &str,
    tools: &[ToolDefinition],
    max_tokens: u32,
) -> Result<Value, ProviderError> {
    let mut system = Vec::new();
    let mut wire: Vec<Value> = Vec::new();
    let mut in_history = false;
    for message in messages {
        match message.role {
            Role::System => {
                if message.content.trim().is_empty() {
                    continue;
                }
                // DeepSeek accepts a later system message as an in-place
                // prompt/tool update. Keeping it at its original position
                // preserves the preceding cache prefix.
                if route.kind.deepseek_messages() && in_history {
                    push_anthropic(
                        &mut wire,
                        "system",
                        vec![json!({"type": "text", "text": message.content})],
                    );
                } else {
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
                if route.kind.reasoning_replay()
                    && (message.reasoning_signature.is_some() || route.kind.deepseek_messages())
                {
                    if let Some(reasoning) = message
                        .reasoning
                        .as_deref()
                        .filter(|text| !text.trim().is_empty())
                    {
                        let mut thinking = json!({"type": "thinking", "thinking": reasoning});
                        if let Some(signature) = message.reasoning_signature.as_deref() {
                            thinking["signature"] = json!(signature);
                        }
                        blocks.push(thinking);
                    }
                }
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
        if message.role != Role::System {
            in_history = true;
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
    if route.kind.deepseek_messages() {
        if let Some(effort) = route.reasoning_effort.as_deref() {
            if effort == "off" {
                body["thinking"] = json!({"type": "disabled"});
            } else {
                let effort = match effort {
                    "low" | "high" | "max" => effort,
                    // eix's generic default is `medium`; DeepSeek's closest
                    // quality-neutral level is `high`.
                    _ => "high",
                };
                body["thinking"] = json!({"type": "enabled"});
                body["output_config"] = json!({"effort": effort});
            }
        }
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
    reasoning_signature: String,
    model: String,
    input_tokens: u64,
    output_tokens: u64,
    cache_read_tokens: u64,
    cache_creation_tokens: u64,
    finish_reason: Option<String>,
    saw_event: bool,
    done: bool,
    tool_calls: Vec<PartialToolCall>,
    images: Vec<ImageAttachment>,
}

impl AnthropicAccumulator {
    fn new(model: &str) -> Self {
        Self {
            text: String::new(),
            reasoning: String::new(),
            reasoning_signature: String::new(),
            model: model.to_string(),
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            finish_reason: None,
            saw_event: false,
            done: false,
            tool_calls: Vec::new(),
            images: Vec::new(),
        }
    }

    fn set_usage(&mut self, usage: &Value) {
        if let Some(tokens) = usage.get("input_tokens").and_then(Value::as_u64) {
            self.input_tokens = tokens;
        }
        if let Some(tokens) = usage.get("output_tokens").and_then(Value::as_u64) {
            self.output_tokens = tokens;
        }
        if let Some(tokens) = usage.get("cache_read_input_tokens").and_then(Value::as_u64) {
            self.cache_read_tokens = tokens;
        }
        if let Some(tokens) = usage
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
        {
            self.cache_creation_tokens = tokens;
        }
    }

    fn finish(self) -> Completion {
        let tool_calls = finish_tool_calls(self.tool_calls);
        let usage = (self.input_tokens > 0
            || self.output_tokens > 0
            || self.cache_read_tokens > 0
            || self.cache_creation_tokens > 0)
            .then_some(Usage {
                prompt_tokens: self
                    .input_tokens
                    .saturating_add(self.cache_read_tokens)
                    .saturating_add(self.cache_creation_tokens),
                completion_tokens: self.output_tokens,
                total_tokens: self
                    .input_tokens
                    .saturating_add(self.cache_read_tokens)
                    .saturating_add(self.cache_creation_tokens)
                    .saturating_add(self.output_tokens),
                cache_read_tokens: self.cache_read_tokens,
                cache_creation_tokens: self.cache_creation_tokens,
                reasoning_tokens: 0,
            });
        Completion {
            text: self.text,
            reasoning: self.reasoning,
            reasoning_signature: (!self.reasoning_signature.is_empty())
                .then_some(self.reasoning_signature),
            model: self.model,
            usage,
            finish_reason: self.finish_reason,
            tool_calls,
            images: clip_images(self.images),
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
            } else if let Some(image) = anthropic_image(block) {
                acc.images.push(image);
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
                "signature_delta" => {
                    if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                        acc.reasoning_signature.push_str(signature);
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
                if let Some(signature) = block.get("signature").and_then(Value::as_str) {
                    acc.reasoning_signature.push_str(signature);
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
            "image" => {
                if let Some(image) = anthropic_image(block) {
                    acc.images.push(image);
                }
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
    images: Vec<ImageAttachment>,
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
            images: Vec::new(),
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
        let cache_read = usage
            .get("cachedContentTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        let reasoning = usage
            .get("thoughtsTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or_default();
        if prompt > 0 || completion > 0 || total > 0 {
            self.usage = Some(Usage {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: total,
                cache_read_tokens: cache_read,
                cache_creation_tokens: 0,
                reasoning_tokens: reasoning,
            });
        }
    }

    fn finish(self) -> Completion {
        Completion {
            text: self.text,
            reasoning: self.reasoning,
            reasoning_signature: None,
            model: self.model,
            usage: self.usage,
            finish_reason: self.finish_reason,
            tool_calls: finish_tool_calls(self.tool_calls),
            images: clip_images(self.images),
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
            if let Some(image) = gemini_image(part) {
                acc.images.push(image);
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
/// Test-only: production goes through [`apply_data_with_tools`].
#[cfg(test)]
fn apply_data<F>(data: &str, acc: &mut Accumulator, on_delta: &mut F) -> Result<bool, ProviderError>
where
    F: FnMut(StreamDelta<'_>),
{
    apply_data_with_tools(data, acc, on_delta, &mut |_| {})
}

/// Like [`apply_data`], but hands tool calls whose arguments are complete to
/// `on_tool_call` before the stream ends. The callback may start read-only
/// work early; it must be cheap because it runs inside the SSE loop.
fn apply_data_with_tools<F, G>(
    data: &str,
    acc: &mut Accumulator,
    on_delta: &mut F,
    on_tool_call: &mut G,
) -> Result<bool, ProviderError>
where
    F: FnMut(StreamDelta<'_>),
    G: FnMut(ToolCall),
{
    let data = data.trim();
    if data.is_empty() {
        return Ok(false);
    }
    if data == "[DONE]" {
        emit_remaining_tool_calls(acc, on_tool_call);
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
        let update = usage_from_openai(usage);
        if let Some(update) = update {
            match &mut acc.usage {
                Some(existing) => existing.merge(&update),
                None => acc.usage = Some(update),
            }
        }
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
        if let Some(delta) = choice.get("delta") {
            push_openai_images(&mut acc.images, delta);
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
                // A new index means every lower one has all of its argument
                // fragments; hand those to the prefetch callback now.
                emit_tool_calls_before(acc, index, on_tool_call);
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
            // Arguments are complete once a finish reason arrives.
            emit_remaining_tool_calls(acc, on_tool_call);
        }
    }
    Ok(false)
}

/// Convert one accumulated slot into a [`ToolCall`], using the same fallbacks
/// as [`finish_tool_calls`].
fn tool_call_from_partial(index: usize, call: PartialToolCall) -> ToolCall {
    ToolCall {
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
    }
}

/// Emit tool calls below `index` whose name is known. Stops at the first slot
/// still waiting for its name; the next higher index (or the end of the
/// stream) will retry it.
fn emit_tool_calls_before<G>(acc: &mut Accumulator, index: usize, on_tool_call: &mut G)
where
    G: FnMut(ToolCall),
{
    while acc.emitted < index.min(acc.tool_calls.len()) {
        let slot = acc.emitted;
        if acc.tool_calls[slot].name.is_empty() {
            break;
        }
        acc.emitted += 1;
        let call = acc.tool_calls[slot].clone();
        on_tool_call(tool_call_from_partial(slot, call));
    }
}

fn emit_remaining_tool_calls<G>(acc: &mut Accumulator, on_tool_call: &mut G)
where
    G: FnMut(ToolCall),
{
    let len = acc.tool_calls.len();
    emit_tool_calls_before(acc, len, on_tool_call);
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

/// Collect image parts from an OpenAI-style message/delta. Both `content`
/// parts and the non-standard `images` array are accepted.
fn push_openai_images(out: &mut Vec<ImageAttachment>, message: &Value) {
    for key in ["content", "images"] {
        let Some(parts) = message.get(key).and_then(Value::as_array) else {
            continue;
        };
        for part in parts {
            if let Some(image) = image_attachment(part) {
                out.push(image);
            }
        }
    }
}

fn openai_images(message: &Value) -> Vec<ImageAttachment> {
    let mut images = Vec::new();
    push_openai_images(&mut images, message);
    clip_images(images)
}

/// Normalize the image shapes used by OpenAI-compatible endpoints, Anthropic
/// content blocks and Gemini parts.
fn image_attachment(value: &Value) -> Option<ImageAttachment> {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !kind.is_empty() && !kind.to_ascii_lowercase().contains("image") {
        return None;
    }
    if let Some(url) = value
        .pointer("/image_url/url")
        .and_then(Value::as_str)
        .or_else(|| value.get("image_url").and_then(Value::as_str))
    {
        return image_from_url(url);
    }
    if let Some(image) = anthropic_image(value) {
        return Some(image);
    }
    if let Some(image) = gemini_image(value) {
        return Some(image);
    }
    if let Some(url) = value.get("url").and_then(Value::as_str) {
        return image_from_url(url);
    }
    if let Some(data) = value.get("b64_json").and_then(Value::as_str) {
        return inline_image("image/png", data);
    }
    if let Some(data) = value.get("data").and_then(Value::as_str) {
        let mime = value
            .get("mime_type")
            .or_else(|| value.get("mimeType"))
            .and_then(Value::as_str)
            .unwrap_or("image/png");
        if mime.starts_with("image/") {
            return inline_image(mime, data);
        }
    }
    None
}

fn anthropic_image(value: &Value) -> Option<ImageAttachment> {
    let source = value.get("source")?;
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => inline_image(
            source
                .get("media_type")
                .and_then(Value::as_str)
                .unwrap_or("image/png"),
            source.get("data").and_then(Value::as_str)?,
        ),
        Some("url") => image_from_url(source.get("url").and_then(Value::as_str)?),
        _ => None,
    }
}

fn gemini_image(value: &Value) -> Option<ImageAttachment> {
    if let Some(inline) = value.get("inlineData").or_else(|| value.get("inline_data")) {
        return inline_image(
            inline
                .get("mimeType")
                .or_else(|| inline.get("mime_type"))
                .and_then(Value::as_str)
                .unwrap_or("image/png"),
            inline.get("data").and_then(Value::as_str)?,
        );
    }
    let file = value.get("fileData").or_else(|| value.get("file_data"))?;
    image_from_url(
        file.get("fileUri")
            .or_else(|| file.get("file_uri"))
            .and_then(Value::as_str)?,
    )
}

fn image_from_url(url: &str) -> Option<ImageAttachment> {
    if let Some(rest) = url.strip_prefix("data:") {
        let (meta, payload) = rest.split_once(',')?;
        let mime = meta.split(';').next().unwrap_or("image/png");
        if !mime.starts_with("image/") {
            return None;
        }
        if meta
            .split(';')
            .any(|part| part.eq_ignore_ascii_case("base64"))
        {
            return inline_image(mime, payload);
        }
        // Keep a non-base64 data URL for the TUI's percent decoder.
        if url.len() > MAX_INLINE_IMAGE_BASE64 {
            return None;
        }
        return Some(ImageAttachment {
            path: url.to_string(),
            mime: mime.to_string(),
            data: String::new(),
            blob: None,
            bytes: None,
        });
    }
    (url.starts_with("http://") || url.starts_with("https://")).then(|| ImageAttachment {
        path: url.to_string(),
        mime: String::new(),
        data: String::new(),
        blob: None,
        bytes: None,
    })
}

fn inline_image(mime: &str, data: &str) -> Option<ImageAttachment> {
    let data = data.trim();
    if data.is_empty() || !mime.starts_with("image/") || data.len() > MAX_INLINE_IMAGE_BASE64 {
        return None;
    }
    Some(ImageAttachment {
        path: String::new(),
        mime: mime.to_string(),
        data: data.to_string(),
        blob: None,
        // Base64 is 4/3 of the binary size; an exact count is not needed for
        // display, and this keeps the event compact.
        bytes: Some(data.len() as u64 * 3 / 4),
    })
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
    if let Some(value) = headers
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
    {
        if let Ok(secs) = value.parse::<f64>() {
            if secs.is_finite() && secs >= 0.0 {
                return Some(secs.ceil() as u64);
            }
        }
        // RFC 7231 allows an HTTP-date instead of a delta. Respect it so a
        // provider that says "retry after 12:00:05" is not retried too early.
        for parsed in [
            chrono::DateTime::parse_from_rfc2822(value),
            chrono::DateTime::parse_from_rfc3339(value),
        ] {
            if let Ok(date) = parsed {
                let secs = date
                    .signed_duration_since(chrono::Utc::now())
                    .num_seconds()
                    .max(0);
                return Some(secs as u64);
            }
        }
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

/// Errors that cannot improve by waiting: the request itself, the key or the
/// selected model is wrong. Gateways frequently wrap these in a 5xx status.
fn looks_like_deterministic_client_error(body: &str) -> bool {
    [
        "invalid_request_error",
        "invalid request",
        "invalid_params",
        "invalid params",
        "reasoning_content",
        "didn't provide an api key",
        "no api key",
        "invalid api key",
        "incorrect api key",
        "unauthorized",
        "forbidden",
        "no available channel",
        "channel has no enabled key",
        "no enabled key",
        "model not found",
        "unsupported model",
        "unsupported parameter",
        "missing required",
        "malformed request",
        "bad request",
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
        "deadline exceeded",
        "capacity",
        "server error",
        "server_error",
        "internal error",
        "service unavailable",
        "unavailable",
        "bad gateway",
        "gateway timeout",
        "interrupted",
        "connection reset",
        "connection aborted",
        "connection closed",
        "connection refused",
        "reset by peer",
        "broken pipe",
        "unexpected eof",
        "unexpected end",
        "stream error",
        "network error",
        "network errors",
        "socket hang up",
        "econnreset",
        "econnrefused",
        "etimedout",
        "resource_exhausted",
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
    fn emits_complete_tool_calls_before_the_stream_ends() {
        let mut acc = Accumulator::new("m");
        let mut on_delta = |_: StreamDelta<'_>| {};
        let mut emitted = Vec::new();

        // Call 0 is complete once index 1 appears.
        let first = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"exec_command","arguments":"{\"cmd\":\"ls\"}"}},{"index":1,"id":"b","function":{"name":"web_fetch","arguments":"{\"url\":\"x\"}"}}]}}]}"#;
        apply_data_with_tools(first, &mut acc, &mut on_delta, &mut |call: ToolCall| {
            emitted.push(call.function.name);
        })
        .unwrap();
        assert_eq!(emitted, vec!["exec_command"]);

        // The finish reason completes the trailing call and keeps both calls
        // in the returned completion.
        let last = r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#;
        apply_data_with_tools(last, &mut acc, &mut on_delta, &mut |call: ToolCall| {
            emitted.push(call.function.name);
        })
        .unwrap();
        assert_eq!(emitted, vec!["exec_command", "web_fetch"]);

        let done = acc.finish();
        assert_eq!(done.tool_calls.len(), 2);
        assert_eq!(done.tool_calls[0].id, "a");
        assert_eq!(done.tool_calls[1].function.name, "web_fetch");
        assert_eq!(done.tool_calls[1].function.arguments, r#"{"url":"x"}"#);
    }

    #[test]
    fn emits_remaining_tool_calls_on_done() {
        let mut acc = Accumulator::new("m");
        let mut on_delta = |_: StreamDelta<'_>| {};
        let mut emitted = 0;
        let chunk = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"web_search","arguments":"{}"}}]}}]}"#;
        apply_data_with_tools(chunk, &mut acc, &mut on_delta, &mut |_: ToolCall| {
            emitted += 1;
        })
        .unwrap();
        assert_eq!(emitted, 0);
        assert!(
            apply_data_with_tools("[DONE]", &mut acc, &mut on_delta, &mut |_: ToolCall| {
                emitted += 1
            })
            .unwrap()
        );
        assert_eq!(emitted, 1);
        assert_eq!(acc.finish().tool_calls.len(), 1);
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
    fn retries_interrupted_streams_and_gateway_failures() {
        for body in [
            "upstream stream interrupted",
            "connection reset by peer",
            "unexpected EOF from upstream",
            "network errors, please retry shortly",
        ] {
            let err = ProviderError::Api {
                status: 200,
                body: body.into(),
                retry_after_secs: None,
            };
            assert_eq!(err.retry_class(), RetryClass::Transient, "{body}");
        }
    }

    #[test]
    fn does_not_retry_deterministic_errors_wrapped_in_5xx() {
        for body in [
            r#"channel opencode: channel has no enabled key"#,
            r#"upstream status 400: {"error":{"type":"invalid_request_error","message":"The reasoning_content in the thinking mode must be passed back"}}"#,
            r#"no available channel for this model"#,
        ] {
            let err = ProviderError::Api {
                status: 502,
                body: body.into(),
                retry_after_secs: None,
            };
            assert_eq!(err.retry_class(), RetryClass::Fatal, "{body}");
        }
    }

    #[test]
    fn parses_retry_after_http_date() {
        let date = chrono::Utc::now() + chrono::Duration::seconds(30);
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            date.to_rfc2822().parse().unwrap(),
        );
        let secs = parse_retry_after(&headers).expect("retry-after date");
        assert!((25..=30).contains(&secs), "{secs}");
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

    fn test_route(kind: ProviderKind, model: &str) -> ModelRoute {
        ModelRoute {
            provider: "test".into(),
            kind,
            model: model.into(),
            endpoint: String::new(),
            api_key: None,
            reasoning_effort: Some("high".into()),
            input_cost: None,
            output_cost: None,
            cache_read_cost: None,
            context_tokens: None,
        }
    }

    #[test]
    fn parses_openai_cache_and_reasoning_usage() {
        let usage = usage_from_openai(&json!({
            "prompt_tokens": 100,
            "completion_tokens": 40,
            "total_tokens": 140,
            "prompt_tokens_details": {"cached_tokens": 80},
            "completion_tokens_details": {"reasoning_tokens": 12}
        }))
        .unwrap();
        assert_eq!(usage.prompt_tokens, 100);
        assert_eq!(usage.cache_read_tokens, 80);
        assert_eq!(usage.uncached_prompt_tokens(), 20);
        assert_eq!(usage.reasoning_tokens, 12);
        assert_eq!(usage.total_tokens, 140);
    }

    #[test]
    fn deepseek_body_replays_signed_thinking_and_in_history_system_updates() {
        let route = test_route(ProviderKind::DeepSeek, "deepseek-v4-flash");
        let messages = vec![
            ChatMessage::system("initial prompt"),
            ChatMessage::user("do it"),
            ChatMessage::assistant_with_reasoning(
                "calling",
                None,
                Some("I should read the file".into()),
                Some("sig-1".into()),
            ),
            ChatMessage::system("new instruction"),
        ];
        let body = anthropic_body(&messages, &route, "deepseek-v4-flash", &[], 4096).unwrap();
        assert_eq!(body["system"], "initial prompt");
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["output_config"]["effort"], "high");
        let assistant = body["messages"][1]["content"].as_array().unwrap();
        assert_eq!(assistant[0]["type"], "thinking");
        assert_eq!(assistant[0]["signature"], "sig-1");
        let last = body["messages"].as_array().unwrap().last().unwrap();
        assert_eq!(last["role"], "system");
        assert_eq!(last["content"][0]["text"], "new instruction");
    }

    #[test]
    fn anthropic_body_does_not_send_deepseek_fields() {
        let route = test_route(ProviderKind::Anthropic, "claude-sonnet");
        let body = anthropic_body(
            &[ChatMessage::user("hi")],
            &route,
            "claude-sonnet",
            &[],
            4096,
        )
        .unwrap();
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
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
        let route = ModelRoute {
            provider: "test".into(),
            kind: ProviderKind::Anthropic,
            model: "claude-sonnet".into(),
            endpoint: String::new(),
            api_key: None,
            reasoning_effort: None,
            input_cost: None,
            output_cost: None,
            cache_read_cost: None,
            context_tokens: None,
        };
        let body = anthropic_body(
            &messages,
            &route,
            "claude-sonnet",
            &[tool_definition()],
            4096,
        )
        .unwrap();
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
    fn collects_provider_image_parts() {
        // OpenAI-compatible non-streaming content array.
        let message = json!({
            "content": [
                {"type": "text", "text": "here"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,QUJD"}},
                {"type": "image_url", "image_url": {"url": "https://example.test/a.png"}}
            ]
        });
        assert_eq!(
            extract_content(message.get("content")).as_deref(),
            Some("here")
        );
        let images = openai_images(&message);
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].mime, "image/png");
        assert_eq!(images[0].data, "QUJD");
        assert_eq!(images[1].path, "https://example.test/a.png");
        assert!(images[1].data.is_empty());

        // OpenAI streaming delta carrying the generated image JSON.
        let mut acc = Accumulator::new("m");
        let mut on_delta = |_: StreamDelta<'_>| {};
        apply_data_with_tools(
            r#"{"choices":[{"delta":{"images":[{"b64_json":"QUJD"}]}}]}"#,
            &mut acc,
            &mut on_delta,
            &mut |_| {},
        )
        .unwrap();
        let completion = acc.finish();
        assert_eq!(completion.images.len(), 1);
        assert_eq!(completion.images[0].data, "QUJD");

        let many = (0..MAX_IMAGE_PARTS + 2)
            .map(|_| ImageAttachment {
                path: String::new(),
                mime: "image/png".into(),
                data: "QUJD".into(),
                blob: None,
                bytes: None,
            })
            .collect();
        assert_eq!(clip_images(many).len(), MAX_IMAGE_PARTS);
        assert!(inline_image("image/png", &"A".repeat(MAX_INLINE_IMAGE_BASE64 + 1)).is_none());
    }

    #[test]
    fn applies_anthropic_image_blocks() {
        let mut acc = AnthropicAccumulator::new("m");
        let mut on_delta = |_: StreamDelta<'_>| {};
        apply_anthropic_message(
            &json!({
                "model": "claude",
                "content": [
                    {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "QUJD"}},
                    {"type": "image", "source": {"type": "url", "url": "https://example.test/a.jpg"}}
                ],
                "stop_reason": "end_turn"
            }),
            &mut acc,
            &mut on_delta,
        )
        .unwrap();
        // Streaming `content_block_start` uses the same shape.
        apply_anthropic_event(
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"image","source":{"type":"base64","media_type":"image/png","data":"REVG"}}}"#,
            &mut acc,
            &mut on_delta,
        )
        .unwrap();
        let completion = acc.finish();
        assert_eq!(completion.images.len(), 3);
        assert_eq!(completion.images[0].mime, "image/jpeg");
        assert_eq!(completion.images[0].data, "QUJD");
        assert_eq!(completion.images[1].path, "https://example.test/a.jpg");
        assert_eq!(completion.images[2].data, "REVG");
    }

    #[test]
    fn applies_gemini_image_parts() {
        let mut acc = GeminiAccumulator::new("m");
        let mut on_delta = |_: StreamDelta<'_>| {};
        apply_gemini_event(
            r#"{"candidates":[{"content":{"parts":[
                {"inlineData":{"mimeType":"image/png","data":"QUJD"}},
                {"fileData":{"mimeType":"image/jpeg","fileUri":"https://example.test/a.jpg"}},
                {"text":"done"}
            ]}}]}"#,
            &mut acc,
            &mut on_delta,
        )
        .unwrap();
        let completion = acc.finish();
        assert_eq!(completion.text, "done");
        assert_eq!(completion.images.len(), 2);
        assert_eq!(completion.images[0].data, "QUJD");
        assert_eq!(completion.images[1].path, "https://example.test/a.jpg");
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
