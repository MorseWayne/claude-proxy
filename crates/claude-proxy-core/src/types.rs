use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Content {
    Text {
        text: String,
    },
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    ToolUse {
        id: String,
        name: String,
        input: Value,
    },
    ToolResult {
        tool_use_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        is_error: Option<bool>,
    },
    ServerToolUse {
        id: String,
        name: String,
        input: Value,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub input_schema: Value,
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
}
