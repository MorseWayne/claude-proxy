use std::collections::HashMap;

use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Value, json};

/// Anthropic Messages API request.
#[derive(Debug, Clone, Serialize)]
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

#[derive(Deserialize)]
struct RawMessagesRequest {
    model: String,
    system: Option<SystemPrompt>,
    messages: Vec<RawMessage>,
    max_tokens: Option<u32>,
    temperature: Option<f32>,
    top_p: Option<f32>,
    top_k: Option<u32>,
    stop_sequences: Option<Vec<String>>,
    #[serde(default)]
    stream: bool,
    tools: Option<Vec<Tool>>,
    tool_choice: Option<Value>,
    thinking: Option<ThinkingConfig>,
    metadata: Option<Value>,
    #[serde(flatten)]
    extra: HashMap<String, Value>,
}

#[derive(Deserialize)]
struct RawMessage {
    role: String,
    content: MessageContent,
}

impl<'de> Deserialize<'de> for MessagesRequest {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawMessagesRequest::deserialize(deserializer)?;
        normalize_messages_request(raw).map_err(de::Error::custom)
    }
}

fn normalize_messages_request(raw: RawMessagesRequest) -> Result<MessagesRequest, String> {
    let mut system = raw.system;
    let mut messages = Vec::with_capacity(raw.messages.len());

    for (index, raw_message) in raw.messages.into_iter().enumerate() {
        match raw_message.role.as_str() {
            "user" => messages.push(Message {
                role: Role::User,
                content: raw_message.content,
            }),
            "assistant" => messages.push(Message {
                role: Role::Assistant,
                content: raw_message.content,
            }),
            "system" => {
                if let Some(inline_system) = system_prompt_from_message_content(raw_message.content)
                {
                    append_system_prompt(&mut system, inline_system);
                }
            }
            other => {
                return Err(format!(
                    "messages[{index}].role: unknown variant `{other}`, expected `user`, `assistant`, or `system`"
                ));
            }
        }
    }

    Ok(MessagesRequest {
        model: raw.model,
        system,
        messages,
        max_tokens: raw.max_tokens,
        temperature: raw.temperature,
        top_p: raw.top_p,
        top_k: raw.top_k,
        stop_sequences: raw.stop_sequences,
        stream: raw.stream,
        tools: raw.tools,
        tool_choice: raw.tool_choice,
        thinking: raw.thinking,
        metadata: raw.metadata,
        extra: raw.extra,
    })
}

fn system_prompt_from_message_content(content: MessageContent) -> Option<SystemPrompt> {
    match content {
        MessageContent::Text(text) if text.is_empty() => None,
        MessageContent::Text(text) => Some(SystemPrompt::Text(text)),
        MessageContent::Blocks(blocks) if blocks.is_empty() => None,
        MessageContent::Blocks(blocks) => Some(SystemPrompt::Blocks(blocks)),
    }
}

fn append_system_prompt(system: &mut Option<SystemPrompt>, incoming: SystemPrompt) {
    *system = Some(match (system.take(), incoming) {
        (None, incoming) => incoming,
        (Some(SystemPrompt::Text(mut existing)), SystemPrompt::Text(incoming)) => {
            if existing.is_empty() {
                SystemPrompt::Text(incoming)
            } else if incoming.is_empty() {
                SystemPrompt::Text(existing)
            } else {
                existing.push_str("\n\n");
                existing.push_str(&incoming);
                SystemPrompt::Text(existing)
            }
        }
        (Some(SystemPrompt::Text(existing)), SystemPrompt::Blocks(mut incoming)) => {
            let mut blocks = Vec::with_capacity(incoming.len() + usize::from(!existing.is_empty()));
            if !existing.is_empty() {
                blocks.push(Content::Text { text: existing });
            }
            blocks.append(&mut incoming);
            SystemPrompt::Blocks(blocks)
        }
        (Some(SystemPrompt::Blocks(mut existing)), SystemPrompt::Text(incoming)) => {
            if !incoming.is_empty() {
                existing.push(Content::Text { text: incoming });
            }
            SystemPrompt::Blocks(existing)
        }
        (Some(SystemPrompt::Blocks(mut existing)), SystemPrompt::Blocks(mut incoming)) => {
            existing.append(&mut incoming);
            SystemPrompt::Blocks(existing)
        }
    });
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

/// Capability support state for a model feature or modality.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityState {
    Supported,
    Unsupported,
    #[default]
    Unknown,
}

impl CapabilityState {
    pub fn from_bool(value: Option<bool>) -> Self {
        match value {
            Some(true) => Self::Supported,
            Some(false) => Self::Unsupported,
            None => Self::Unknown,
        }
    }

    pub fn is_supported(self) -> bool {
        self == Self::Supported
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointCapabilities {
    #[serde(default)]
    pub anthropic_messages: CapabilityState,
    #[serde(default)]
    pub openai_chat_completions: CapabilityState,
    #[serde(default)]
    pub openai_responses: CapabilityState,
}

impl Default for EndpointCapabilities {
    fn default() -> Self {
        Self {
            anthropic_messages: CapabilityState::Unknown,
            openai_chat_completions: CapabilityState::Unknown,
            openai_responses: CapabilityState::Unknown,
        }
    }
}

impl EndpointCapabilities {
    pub fn from_paths(paths: &[String]) -> Self {
        Self {
            anthropic_messages: path_state(paths, "/v1/messages"),
            openai_chat_completions: path_state(paths, "/chat/completions"),
            openai_responses: path_state(paths, "/responses"),
        }
    }

    pub fn supported_paths(&self) -> Vec<String> {
        let mut paths = Vec::new();
        if self.anthropic_messages.is_supported() {
            paths.push("/v1/messages".to_string());
        }
        if self.openai_chat_completions.is_supported() {
            paths.push("/chat/completions".to_string());
        }
        if self.openai_responses.is_supported() {
            paths.push("/responses".to_string());
        }
        paths
    }
}

fn path_state(paths: &[String], path: &str) -> CapabilityState {
    if paths.iter().any(|candidate| candidate == path) {
        CapabilityState::Supported
    } else {
        CapabilityState::Unsupported
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModalityCapabilities {
    #[serde(default)]
    pub input: InputModalities,
    #[serde(default)]
    pub output: OutputModalities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputModalities {
    #[serde(default = "supported_capability")]
    pub text: CapabilityState,
    #[serde(default)]
    pub image: CapabilityState,
    #[serde(default)]
    pub document: CapabilityState,
    #[serde(default = "unsupported_capability")]
    pub audio: CapabilityState,
    #[serde(default = "unsupported_capability")]
    pub video: CapabilityState,
}

impl Default for InputModalities {
    fn default() -> Self {
        Self {
            text: CapabilityState::Supported,
            image: CapabilityState::Unknown,
            document: CapabilityState::Unknown,
            audio: CapabilityState::Unsupported,
            video: CapabilityState::Unsupported,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputModalities {
    #[serde(default = "supported_capability")]
    pub text: CapabilityState,
    #[serde(default = "unsupported_capability")]
    pub image: CapabilityState,
    #[serde(default = "unsupported_capability")]
    pub audio: CapabilityState,
}

impl Default for OutputModalities {
    fn default() -> Self {
        Self {
            text: CapabilityState::Supported,
            image: CapabilityState::Unsupported,
            audio: CapabilityState::Unsupported,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FeatureCapabilities {
    #[serde(default)]
    pub streaming: CapabilityState,
    #[serde(default)]
    pub system_prompt: CapabilityState,
    #[serde(default)]
    pub tools: CapabilityState,
    #[serde(default)]
    pub tool_choice: CapabilityState,
    #[serde(default)]
    pub thinking: CapabilityState,
    #[serde(default)]
    pub adaptive_thinking: CapabilityState,
    #[serde(default)]
    pub reasoning_effort: CapabilityState,
    #[serde(default)]
    pub prompt_cache: CapabilityState,
    #[serde(default)]
    pub sampling: CapabilityState,
    #[serde(default)]
    pub stop_sequences: CapabilityState,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QualityGateCapabilities {
    #[serde(default)]
    pub tool_search: ToolSearchCapability,
    #[serde(default)]
    pub fine_grained_tool_streaming: CapabilityState,
    #[serde(default)]
    pub prompt_cache: PromptCacheCapability,
    #[serde(default)]
    pub context_management: ContextManagementCapability,
    #[serde(default)]
    pub interleaved_thinking: CapabilityState,
    #[serde(default)]
    pub max_effort: CapabilityState,
    #[serde(default)]
    pub structured_outputs: CapabilityState,
    #[serde(default)]
    pub strict_tools: CapabilityState,
    #[serde(default)]
    pub token_efficient_tools: CapabilityState,
    #[serde(default)]
    pub fast_mode: CapabilityState,
    #[serde(default)]
    pub token_counting: TokenCountingCapability,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityGateHeaderKind {
    #[default]
    None,
    #[serde(rename = "anthropic_1p")]
    Anthropic1p,
    #[serde(rename = "anthropic_3p")]
    Anthropic3p,
    ExtraBody,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QualityGateBetaLocation {
    #[default]
    None,
    Header,
    ExtraBody,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ToolSearchCapability {
    #[serde(default)]
    pub state: CapabilityState,
    #[serde(default)]
    pub header_kind: QualityGateHeaderKind,
    #[serde(default)]
    pub beta_location: QualityGateBetaLocation,
}

impl ToolSearchCapability {
    pub fn supported(
        header_kind: QualityGateHeaderKind,
        beta_location: QualityGateBetaLocation,
    ) -> Self {
        Self {
            state: CapabilityState::Supported,
            header_kind,
            beta_location,
        }
    }

    pub fn unsupported() -> Self {
        Self {
            state: CapabilityState::Unsupported,
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptCacheScope {
    #[default]
    Unknown,
    None,
    Basic,
    GlobalScope,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct PromptCacheCapability {
    #[serde(default)]
    pub state: CapabilityState,
    #[serde(default)]
    pub scope: PromptCacheScope,
}

impl PromptCacheCapability {
    pub fn basic() -> Self {
        Self {
            state: CapabilityState::Supported,
            scope: PromptCacheScope::Basic,
        }
    }

    pub fn unsupported() -> Self {
        Self {
            state: CapabilityState::Unsupported,
            scope: PromptCacheScope::None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ContextManagementCapability {
    #[serde(default)]
    pub thinking_preservation: CapabilityState,
    #[serde(default)]
    pub tool_result_clearing: CapabilityState,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenCountingMode {
    #[default]
    Unknown,
    None,
    Rough,
    Native,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct TokenCountingCapability {
    #[serde(default)]
    pub mode: TokenCountingMode,
}

impl TokenCountingCapability {
    pub fn none() -> Self {
        Self {
            mode: TokenCountingMode::None,
        }
    }

    pub fn rough() -> Self {
        Self {
            mode: TokenCountingMode::Rough,
        }
    }

    pub fn native() -> Self {
        Self {
            mode: TokenCountingMode::Native,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_thinking_budget: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_thinking_budget: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning_effort_levels: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCapabilities {
    #[serde(default)]
    pub endpoints: EndpointCapabilities,
    #[serde(default)]
    pub modalities: ModalityCapabilities,
    #[serde(default)]
    pub features: FeatureCapabilities,
    #[serde(default)]
    pub limits: ModelLimits,
    #[serde(default)]
    pub quality: QualityGateCapabilities,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supported_parameters: Vec<String>,
}

fn supported_capability() -> CapabilityState {
    CapabilityState::Supported
}

fn unsupported_capability() -> CapabilityState {
    CapabilityState::Unsupported
}

/// Information about an available model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelInfo {
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_chat_default: Option<bool>,
    #[serde(default)]
    pub capabilities: ModelCapabilities,
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
    fn messages_request_normalizes_inline_system_message() {
        let parsed: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-opus-4-8",
            "system": "Base instructions.",
            "messages": [
                {"role": "user", "content": "Hi"},
                {"role": "system", "content": "Use strict typing from now on."},
                {"role": "assistant", "content": "Understood."}
            ]
        }))
        .unwrap();

        assert_eq!(parsed.messages.len(), 2);
        assert_eq!(parsed.messages[0].role, Role::User);
        assert_eq!(parsed.messages[1].role, Role::Assistant);
        assert!(matches!(
            parsed.system,
            Some(SystemPrompt::Text(text))
                if text == "Base instructions.\n\nUse strict typing from now on."
        ));
    }

    #[test]
    fn messages_request_normalizes_inline_system_blocks() {
        let parsed: MessagesRequest = serde_json::from_value(json!({
            "model": "claude-opus-4-8",
            "system": "Base instructions.",
            "messages": [
                {"role": "user", "content": "Hi"},
                {"role": "system", "content": [
                    {"type": "text", "text": "Use strict typing from now on."}
                ]}
            ]
        }))
        .unwrap();

        let Some(SystemPrompt::Blocks(blocks)) = parsed.system else {
            panic!("expected merged system blocks");
        };
        assert_eq!(blocks.len(), 2);
        assert!(matches!(&blocks[0], Content::Text { text } if text == "Base instructions."));
        assert!(
            matches!(&blocks[1], Content::Text { text } if text == "Use strict typing from now on.")
        );
    }

    #[test]
    fn messages_request_rejects_unknown_message_role() {
        let err = serde_json::from_value::<MessagesRequest>(json!({
            "model": "claude-sonnet-4-20250514",
            "messages": [{"role": "developer", "content": "Hi"}]
        }))
        .unwrap_err();

        assert!(err.to_string().contains("unknown variant `developer`"));
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
    fn model_capabilities_serialize_canonical_shape() {
        let info = ModelInfo {
            model_id: "gpt-5.5".to_string(),
            vendor: Some("openai".to_string()),
            is_chat_default: None,
            capabilities: ModelCapabilities {
                endpoints: EndpointCapabilities {
                    anthropic_messages: CapabilityState::Unsupported,
                    openai_chat_completions: CapabilityState::Supported,
                    openai_responses: CapabilityState::Supported,
                },
                modalities: ModalityCapabilities {
                    input: InputModalities {
                        image: CapabilityState::Unknown,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                features: FeatureCapabilities {
                    streaming: CapabilityState::Supported,
                    tools: CapabilityState::Supported,
                    thinking: CapabilityState::Supported,
                    reasoning_effort: CapabilityState::Supported,
                    ..Default::default()
                },
                limits: ModelLimits {
                    context_window: Some(400_000),
                    max_output_tokens: Some(128_000),
                    reasoning_effort_levels: vec!["low".to_string(), "high".to_string()],
                    ..Default::default()
                },
                quality: QualityGateCapabilities {
                    tool_search: ToolSearchCapability::supported(
                        QualityGateHeaderKind::Anthropic1p,
                        QualityGateBetaLocation::Header,
                    ),
                    prompt_cache: PromptCacheCapability::basic(),
                    context_management: ContextManagementCapability {
                        thinking_preservation: CapabilityState::Supported,
                        tool_result_clearing: CapabilityState::Unsupported,
                    },
                    structured_outputs: CapabilityState::Supported,
                    token_counting: TokenCountingCapability::native(),
                    ..Default::default()
                },
                supported_parameters: vec!["messages".to_string(), "tools".to_string()],
            },
        };

        let value = serde_json::to_value(&info).unwrap();
        assert_eq!(
            value["capabilities"]["endpoints"]["openai_responses"],
            "supported"
        );
        assert_eq!(
            value["capabilities"]["modalities"]["input"]["image"],
            "unknown"
        );
        assert_eq!(value["capabilities"]["limits"]["context_window"], 400_000);
        assert_eq!(
            value["capabilities"]["quality"]["tool_search"]["header_kind"],
            "anthropic_1p"
        );
        assert_eq!(
            value["capabilities"]["quality"]["prompt_cache"]["scope"],
            "basic"
        );
        assert_eq!(
            value["capabilities"]["quality"]["token_counting"]["mode"],
            "native"
        );
        assert_eq!(value["capabilities"]["supported_parameters"][1], "tools");

        let parsed: ModelInfo = serde_json::from_value(value).unwrap();
        assert!(
            parsed
                .capabilities
                .endpoints
                .openai_responses
                .is_supported()
        );
        assert_eq!(parsed.capabilities.limits.max_output_tokens, Some(128_000));
        assert_eq!(
            parsed.capabilities.quality.prompt_cache.scope,
            PromptCacheScope::Basic
        );
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
