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
    #[serde(default, alias = "prompt_tokens")]
    pub prompt_tokens: u64,
    #[serde(default, alias = "completion_tokens")]
    pub completion_tokens: u64,
    #[serde(default, alias = "total_tokens")]
    pub total_tokens: u64,
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
                name: "read_file".into(),
                arguments: r#"{"path":"a.rs"}"#.into(),
            },
        };
        let assistant = ChatMessage::assistant_with_tools("", vec![call]);
        let value = serde_json::to_value(&assistant).unwrap();
        assert_eq!(value["tool_calls"][0]["id"], "call_1");
        assert_eq!(value["tool_calls"][0]["type"], "function");
        assert_eq!(value["tool_calls"][0]["function"]["name"], "read_file");

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
}
