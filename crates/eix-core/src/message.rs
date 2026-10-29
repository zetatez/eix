//! Provider-agnostic chat messages.
//!
//! These serialize directly into the OpenAI "chat completions" wire format, so
//! any OpenAI-compatible endpoint (OpenAI, Ollama, vLLM, OpenRouter, ...) can be
//! driven from the same types.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// JSON-encoded arguments, exactly as the provider sent them.
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_type")]
    pub kind: String,
    pub function: FunctionCall,
}

fn function_type() -> String {
    "function".to_string()
}

/// One image attached to a user message. `data` is base64 (no data-URL
/// prefix); `mime` is `image/png`, `image/jpeg`, ... .
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePart {
    pub mime: String,
    pub data: String,
}

impl ImagePart {
    pub fn data_url(&self) -> String {
        format!("data:{};base64,{}", self.mime, self.data)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Present on assistant messages that requested tool calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// Present on `role: tool` messages, linking the result to its call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Attached images for multimodal user messages.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImagePart>,
    /// Assistant thinking text. Not part of the OpenAI wire shape; adapters
    /// that support signed thinking blocks serialize it explicitly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Opaque signature paired with `reasoning` for providers that verify it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_signature: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::new(Role::User, content)
    }

    pub fn user_with_images(content: impl Into<String>, images: Vec<ImagePart>) -> Self {
        let mut message = Self::new(Role::User, content);
        message.images = images;
        message
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(Role::Assistant, content)
    }

    pub fn assistant_with_tools(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        let mut message = Self::new(Role::Assistant, content);
        message.tool_calls = Some(tool_calls);
        message
    }

    /// Assistant message carrying provider reasoning metadata for replay.
    pub fn assistant_with_reasoning(
        content: impl Into<String>,
        tool_calls: Option<Vec<ToolCall>>,
        reasoning: Option<String>,
        reasoning_signature: Option<String>,
    ) -> Self {
        let mut message = Self::new(Role::Assistant, content);
        message.tool_calls = tool_calls;
        message.reasoning = reasoning;
        message.reasoning_signature = reasoning_signature;
        message
    }

    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        let mut message = Self::new(Role::Tool, content);
        message.tool_call_id = Some(tool_call_id.into());
        message
    }

    pub fn new(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            name: None,
            tool_calls: None,
            tool_call_id: None,
            images: Vec::new(),
            reasoning: None,
            reasoning_signature: None,
        }
    }

    /// OpenAI-compatible wire object. Text-only messages keep `content` as a
    /// string so providers that do not understand content parts keep working;
    /// image messages use the multimodal array form.
    pub fn to_wire(&self) -> Value {
        let content = if self.images.is_empty() {
            json!(self.content)
        } else {
            let mut parts = Vec::new();
            if !self.content.is_empty() {
                parts.push(json!({ "type": "text", "text": self.content }));
            }
            for image in &self.images {
                parts.push(json!({
                    "type": "image_url",
                    "image_url": { "url": image.data_url() },
                }));
            }
            Value::Array(parts)
        };
        let mut value = json!({ "role": self.role, "content": content });
        if let Some(name) = &self.name {
            value["name"] = json!(name);
        }
        if let Some(tool_calls) = &self.tool_calls {
            value["tool_calls"] = serde_json::to_value(tool_calls).unwrap_or(Value::Null);
        }
        if let Some(tool_call_id) = &self.tool_call_id {
            value["tool_call_id"] = json!(tool_call_id);
        }
        value
    }
}

/// OpenAI-compatible function tool definition sent with every request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FunctionDefinition,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

impl ToolDefinition {
    pub fn function(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: serde_json::Value,
    ) -> Self {
        Self {
            kind: "function".to_string(),
            function: FunctionDefinition {
                name: name.into(),
                description: description.into(),
                parameters,
            },
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Total prompt tokens reported by the provider, including cache hits.
    #[serde(default, alias = "prompt_tokens")]
    pub prompt_tokens: u64,
    #[serde(default, alias = "completion_tokens")]
    pub completion_tokens: u64,
    #[serde(default, alias = "total_tokens")]
    pub total_tokens: u64,
    /// Prompt tokens served from the provider's prefix cache (a subset of
    /// `prompt_tokens`). Kept separate so callers can report cache efficiency.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_read_tokens: u64,
    /// Prompt tokens written to the provider's cache this request (a subset of
    /// `prompt_tokens` for providers that bill cache writes separately).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cache_creation_tokens: u64,
    /// Reasoning tokens included in `completion_tokens`, when reported.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub reasoning_tokens: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

impl Usage {
    /// Prompt tokens that were not served from cache. Saturating so malformed
    /// provider metrics can never underflow the estimate.
    pub fn uncached_prompt_tokens(&self) -> u64 {
        self.prompt_tokens
            .saturating_sub(self.cache_read_tokens)
            .saturating_sub(self.cache_creation_tokens)
    }

    /// Merge a later usage snapshot (for example a `message_delta` after
    /// `message_start`) without dropping cache metrics that only one event
    /// carried.
    pub fn merge(&mut self, later: &Usage) {
        if later.prompt_tokens > 0 {
            self.prompt_tokens = later.prompt_tokens;
        }
        if later.completion_tokens > 0 {
            self.completion_tokens = later.completion_tokens;
        }
        if later.total_tokens > 0 {
            self.total_tokens = later.total_tokens;
        }
        if later.cache_read_tokens > 0 {
            self.cache_read_tokens = later.cache_read_tokens;
        }
        if later.cache_creation_tokens > 0 {
            self.cache_creation_tokens = later.cache_creation_tokens;
        }
        if later.reasoning_tokens > 0 {
            self.reasoning_tokens = later.reasoning_tokens;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_to_openai_shape() {
        let msg = ChatMessage::user("hello");
        let value = serde_json::to_value(&msg).unwrap();
        assert_eq!(value["role"], "user");
        assert_eq!(value["content"], "hello");
        assert!(value.get("name").is_none());
        assert!(value.get("tool_calls").is_none());
    }

    #[test]
    fn serializes_tool_call_and_result() {
        let call = ToolCall {
            id: "call_1".into(),
            kind: "function".into(),
            function: FunctionCall {
                name: "exec_command".into(),
                arguments: r#"{"cmd":"cat a.rs"}"#.into(),
            },
        };
        let assistant = ChatMessage::assistant_with_tools("", vec![call]);
        let value = serde_json::to_value(&assistant).unwrap();
        assert_eq!(value["tool_calls"][0]["id"], "call_1");
        assert_eq!(value["tool_calls"][0]["type"], "function");
        assert_eq!(value["tool_calls"][0]["function"]["name"], "exec_command");

        let result = ChatMessage::tool("call_1", "contents");
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["role"], "tool");
        assert_eq!(value["tool_call_id"], "call_1");
    }

    #[test]
    fn image_only_messages_use_content_parts() {
        let message = ChatMessage::user_with_images(
            "",
            vec![ImagePart {
                mime: "image/png".into(),
                data: "aGVsbG8=".into(),
            }],
        );
        let wire = message.to_wire();
        assert_eq!(wire["role"], "user");
        assert_eq!(wire["content"].as_array().unwrap().len(), 1);
        assert_eq!(wire["content"][0]["type"], "image_url");
        assert_eq!(
            wire["content"][0]["image_url"]["url"],
            "data:image/png;base64,aGVsbG8="
        );
    }

    #[test]
    fn usage_parses_openai_fields() {
        let usage: Usage =
            serde_json::from_value(serde_json::json!({"prompt_tokens": 3, "completion_tokens": 5}))
                .unwrap();
        assert_eq!(usage.prompt_tokens, 3);
        assert_eq!(usage.completion_tokens, 5);
        assert_eq!(usage.total_tokens, 0);
    }

    #[test]
    fn usage_keeps_cache_and_reasoning_buckets() {
        let usage: Usage = serde_json::from_value(serde_json::json!({
            "prompt_tokens": 100,
            "completion_tokens": 20,
            "total_tokens": 120,
            "cache_read_tokens": 80,
            "cache_creation_tokens": 10,
            "reasoning_tokens": 5
        }))
        .unwrap();
        assert_eq!(usage.uncached_prompt_tokens(), 10);
        assert_eq!(usage.cache_read_tokens, 80);
        assert_eq!(usage.reasoning_tokens, 5);

        let mut merged = usage.clone();
        merged.merge(&Usage {
            completion_tokens: 25,
            total_tokens: 125,
            ..Usage::default()
        });
        assert_eq!(merged.prompt_tokens, 100);
        assert_eq!(merged.completion_tokens, 25);
        assert_eq!(merged.cache_read_tokens, 80);
    }
}
