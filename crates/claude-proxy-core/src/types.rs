use std::collections::HashMap;

use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

/// Anthropic Messages API request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<SystemPrompt>,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    pub stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<Tool>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_choice: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
    /// Unknown/extra fields preserved and forwarded upstream.
    #[serde(flatten)]
    pub extra: HashMap<String, Value>,
}

/// System prompt: either a plain string or an array of content blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SystemPrompt {
    Text(String),
    Blocks(Vec<Content>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: MessageContent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// Message content: either a single text string or an array of content blocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Blocks(Vec<Content>),
}

#[derive(Debug, Clone)]
pub enum Content {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        content: Option<Value>,
        is_error: Option<bool>,
    },
    ServerToolUse {
        id: String,
        name: String,
        input: Value,
    },
    Unknown(Value),
}

impl Serialize for Content {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Content::Text { text } => json!({
                "type": "text",
                "text": text,
            })
            .serialize(serializer),
            Content::Thinking {
                thinking,
                signature,
            } => {
                let mut value = json!({
                    "type": "thinking",
                    "thinking": thinking,
                });
                if let Some(signature) = signature {
                    value["signature"] = json!(signature);
                }
                value.serialize(serializer)
            }
            Content::ToolUse { id, name, input } => json!({
                "type": "tool_use",
                "id": id,
                "name": name,
                "input": input,
            })
            .serialize(serializer),
            Content::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let mut value = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_use_id,
                });
                if let Some(content) = content {
                    value["content"] = content.clone();
                }
                if let Some(is_error) = is_error {
                    value["is_error"] = json!(is_error);
                }
                value.serialize(serializer)
            }
            Content::ServerToolUse { id, name, input } => json!({
                "type": "server_tool_use",
                "id": id,
                "name": name,
                "input": input,
            })
            .serialize(serializer),
            Content::Unknown(value) => value.serialize(serializer),
        }
    }
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        deserialize_content(value).map_err(de::Error::custom)
    }
}

#[derive(Deserialize)]
struct TextContent {
    text: String,
}

#[derive(Deserialize)]
struct ThinkingContent {
    thinking: String,
    signature: Option<String>,
}

#[derive(Deserialize)]
struct ToolUseContent {
    id: String,
    name: String,
    input: Value,
}

#[derive(Deserialize)]
struct ToolResultContent {
    tool_use_id: String,
    content: Option<Value>,
    is_error: Option<bool>,
}

fn deserialize_content(value: Value) -> Result<Content, String> {
    let Some(content_type) = value.get("type").and_then(Value::as_str) else {
        return Ok(Content::Unknown(value));
    };

    match content_type {
        "text" => serde_json::from_value::<TextContent>(value)
            .map(|content| Content::Text { text: content.text })
            .map_err(|err| format!("invalid text content block: {err}")),
        "thinking" => serde_json::from_value::<ThinkingContent>(value)
            .map(|content| Content::Thinking {
                thinking: content.thinking,
                signature: content.signature,
            })
            .map_err(|err| format!("invalid thinking content block: {err}")),
        "tool_use" => serde_json::from_value::<ToolUseContent>(value)
            .map(|content| Content::ToolUse {
                id: content.id,
                name: content.name,
                input: content.input,
            })
            .map_err(|err| format!("invalid tool_use content block: {err}")),
        "tool_result" => serde_json::from_value::<ToolResultContent>(value)
            .map(|content| Content::ToolResult {
                tool_use_id: content.tool_use_id,
                content: content.content,
                is_error: content.is_error,
            })
            .map_err(|err| format!("invalid tool_result content block: {err}")),
        "server_tool_use" => serde_json::from_value::<ToolUseContent>(value)
            .map(|content| Content::ServerToolUse {
                id: content.id,
                name: content.name,
                input: content.input,
            })
            .map_err(|err| format!("invalid server_tool_use content block: {err}")),
        _ => Ok(Content::Unknown(value)),
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Tool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
}

impl<'de> Deserialize<'de> for Tool {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = Value::deserialize(deserializer)?;
        deserialize_tool(value).map_err(de::Error::custom)
    }
}

fn deserialize_tool(value: Value) -> Result<Tool, String> {
    let Value::Object(mut object) = value else {
        return Err("tool must be an object".to_string());
    };

    let function = object.remove("function");
    let mut function = match function {
        Some(Value::Object(function)) => function,
        Some(_) => return Err("tool.function must be an object".to_string()),
        None => serde_json::Map::new(),
    };

    let name = take_string(&mut function, "name")
        .or_else(|| take_string(&mut object, "name"))
        .ok_or_else(|| "missing field `name`".to_string())?;
    let description = take_string(&mut function, "description")
        .or_else(|| take_string(&mut object, "description"));
    let input_schema = function
        .remove("input_schema")
        .or_else(|| function.remove("parameters"))
        .or_else(|| object.remove("input_schema"))
        .or_else(|| object.remove("parameters"))
        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));

    Ok(Tool {
        name,
        description,
        input_schema,
    })
}

fn take_string(object: &mut serde_json::Map<String, Value>, key: &str) -> Option<String> {
    object
        .remove(key)
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThinkingConfig {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub budget_tokens: Option<u32>,
}

/// A unified SSE event from any provider.
#[derive(Debug, Clone, Serialize)]
pub struct SseEvent {
    pub event: String,
    pub data: Value,
}

/// Information about an available model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_endpoints: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_chat_default: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_vision: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_adaptive_thinking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_thinking_budget: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_thinking_budget: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_effort_levels: Vec<String>,
}

/// Errors from provider interactions.
#[derive(Debug, Clone, Serialize)]
pub struct AnthropicError {
    #[serde(rename = "type")]
    pub error_type: String,
    pub message: String,
}

/// Anthropic-style error response wrapper.
#[derive(Debug, Clone, Serialize)]
pub struct ErrorResponse {
    #[serde(rename = "type")]
    pub r#type: String,
    pub error: AnthropicError,
}

impl ErrorResponse {
    pub fn authentication(message: &str) -> Self {
        Self {
            r#type: "error".to_string(),
            error: AnthropicError {
                error_type: "authentication_error".to_string(),
                message: message.to_string(),
            },
        }
    }

    pub fn rate_limit(message: &str) -> Self {
        Self {
            r#type: "error".to_string(),
            error: AnthropicError {
                error_type: "rate_limit_error".to_string(),
                message: message.to_string(),
            },
        }
    }

    pub fn api_error(message: &str) -> Self {
        Self {
            r#type: "error".to_string(),
            error: AnthropicError {
                error_type: "api_error".to_string(),
                message: message.to_string(),
            },
        }
    }

    pub fn timeout(message: &str) -> Self {
        Self {
            r#type: "error".to_string(),
            error: AnthropicError {
                error_type: "timeout_error".to_string(),
                message: message.to_string(),
            },
        }
    }

    pub fn invalid_request(message: &str) -> Self {
        Self {
            r#type: "error".to_string(),
            error: AnthropicError {
                error_type: "invalid_request_error".to_string(),
                message: message.to_string(),
            },
        }
    }

    pub fn not_found(message: &str) -> Self {
        Self {
            r#type: "error".to_string(),
            error: AnthropicError {
                error_type: "not_found_error".to_string(),
                message: message.to_string(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_messages_request_roundtrip() {
        let req = MessagesRequest {
            model: "claude-sonnet-4-20250514".to_string(),
            system: Some(SystemPrompt::Text("You are helpful.".to_string())),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("Hello".to_string()),
            }],
            max_tokens: Some(1024),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: HashMap::new(),
        };

        let json = serde_json::to_string(&req).unwrap();
        let parsed: MessagesRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.model, "claude-sonnet-4-20250514");
        assert!(parsed.stream);
    }

    #[test]
    fn content_preserves_unknown_block_roundtrip() {
        let raw = json!({
            "type": "mcp_tool_use",
            "id": "mcp_1",
            "name": "fetch_context",
            "input": {"uri": "repo://context"},
            "server_name": "gitnexus"
        });

        let content: Content = serde_json::from_value(raw.clone()).unwrap();
        let Content::Unknown(preserved) = &content else {
            panic!("expected unknown content block");
        };
        assert_eq!(preserved, &raw);
        assert_eq!(serde_json::to_value(content).unwrap(), raw);
    }

    #[test]
    fn tool_deserializes_anthropic_shape() {
        let tool: Tool = serde_json::from_value(json!({
            "name": "read_file",
            "description": "Read a file",
            "input_schema": {
                "type": "object",
                "properties": {"path": {"type": "string"}}
            }
        }))
        .unwrap();

        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.description.as_deref(), Some("Read a file"));
        assert_eq!(tool.input_schema["properties"]["path"]["type"], "string");
    }

    #[test]
    fn tool_deserializes_openai_responses_shape() {
        let tool: Tool = serde_json::from_value(json!({
            "type": "function",
            "name": "read_file",
            "description": "Read a file",
            "parameters": {
                "type": "object",
                "properties": {"path": {"type": "string"}}
            }
        }))
        .unwrap();

        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.description.as_deref(), Some("Read a file"));
        assert_eq!(tool.input_schema["properties"]["path"]["type"], "string");
    }

    #[test]
    fn tool_deserializes_openai_chat_shape() {
        let tool: Tool = serde_json::from_value(json!({
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file",
                "parameters": {
                    "type": "object",
                    "properties": {"path": {"type": "string"}}
                }
            }
        }))
        .unwrap();

        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.description.as_deref(), Some("Read a file"));
        assert_eq!(tool.input_schema["properties"]["path"]["type"], "string");
    }

    #[test]
    fn tool_defaults_missing_schema_to_empty_object() {
        let tool: Tool = serde_json::from_value(json!({
            "type": "function",
            "name": "now"
        }))
        .unwrap();

        assert_eq!(tool.name, "now");
        assert_eq!(
            tool.input_schema,
            json!({"type": "object", "properties": {}})
        );
    }
}
