//! OpenAI-compatible streaming provider.
//!
//! One adapter covers OpenAI, Ollama, vLLM, OpenRouter, LM Studio and friends:
//! POST `/chat/completions` with `stream: true` and parse SSE `data:` lines.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use eix_core::redact::SecretRedactor;
use eix_core::{ChatMessage, Config, FunctionCall, ModelRoute, ToolCall, ToolDefinition, Usage};
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
                if looks_like_context_length(&body) {
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
    /// Random per-provider salt for reversible secret placeholders. It never
    /// leaves this process; without it the hashed ids cannot be dictionary
    /// attacked for low-entropy passwords.
    redaction_salt: String,
}

impl Provider {
    pub fn new(config: &Config) -> Result<Self, ProviderError> {
        let client = reqwest::Client::builder()
            .user_agent(format!("eix/{}", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .timeout(Duration::from_secs(config.provider.timeout_secs))
            .build()
            .map_err(|e| ProviderError::Http(e.to_string()))?;
        Ok(Self {
            client,
            model: config.provider.model.clone(),
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
        let mut result = self
            .stream_chat_inner(&sanitized, route, tools, cancel, &mut on_delta)
            .await;
        if let Ok(completion) = &mut result {
            completion.text = redactor.restore(&completion.text);
            completion.reasoning = redactor.restore(&completion.reasoning);
            for call in &mut completion.tool_calls {
                call.function.arguments = redactor.restore_json(&call.function.arguments);
            }
        }
        result
    }

    async fn stream_chat_inner<F>(
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
        if let Some(key) = &route.api_key {
            request = request.bearer_auth(key);
        }

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
            _ = async {
                while !cancel.load(Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
            } => return Err(ProviderError::Cancelled),
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
        let first = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_a","type":"function","function":{"name":"read_file","arguments":"{\"pa"}}]}}]}"#;
        let second = r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a.rs\"}"}}]},"finish_reason":"tool_calls"}]}"#;
        apply_data(first, &mut acc, &mut on_delta).unwrap();
        apply_data(second, &mut acc, &mut on_delta).unwrap();
        let done = acc.finish();
        assert_eq!(done.tool_calls.len(), 1);
        assert_eq!(done.tool_calls[0].id, "call_a");
        assert_eq!(done.tool_calls[0].function.name, "read_file");
        assert_eq!(done.tool_calls[0].function.arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(done.finish_reason.as_deref(), Some("tool_calls"));
    }

    #[test]
    fn assembles_multiple_tool_calls_by_index() {
        let mut acc = Accumulator::new("m");
        let mut on_delta = |_: StreamDelta<'_>| {};
        let chunks = [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"a","function":{"name":"read_file","arguments":"{}"}},{"index":1,"id":"b","function":{"name":"list_dir","arguments":"{}"}}]}}]}"#,
        ];
        for chunk in chunks {
            apply_data(chunk, &mut acc, &mut on_delta).unwrap();
        }
        let done = acc.finish();
        assert_eq!(done.tool_calls.len(), 2);
        assert_eq!(done.tool_calls[0].function.name, "read_file");
        assert_eq!(done.tool_calls[1].function.name, "list_dir");
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
}
