use std::collections::HashMap;

use axum::Json;
use axum::extract::{State, rejection::JsonRejection};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use claude_proxy_core::{
    Content, Message, MessageContent, MessagesRequest, Role, SystemPrompt, Tool,
};
use serde_json::{Map, Value, json};

use crate::app::AppState;
use crate::downstream::{DownstreamProtocol, openai_error_response};

const CHAT_FIELDS: &[&str] = &[
    "model",
    "messages",
    "max_tokens",
    "max_completion_tokens",
    "temperature",
    "top_p",
    "stop",
    "stream",
    "stream_options",
    "tools",
    "tool_choice",
    "parallel_tool_calls",
    "response_format",
    "reasoning_effort",
    "verbosity",
    "service_tier",
    "metadata",
    "prompt_cache_key",
    "safety_identifier",
    "store",
    "n",
    "logprobs",
    "top_logprobs",
    "frequency_penalty",
    "presence_penalty",
    "seed",
    "user",
    "modalities",
    "audio",
    "prediction",
    "web_search_options",
    "functions",
    "function_call",
];

#[derive(Debug)]
struct RequestError {
    message: String,
    param: Option<String>,
}

impl RequestError {
    fn invalid(param: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            param: Some(param.into()),
        }
    }

    fn unsupported(param: impl Into<String>) -> Self {
        let param = param.into();
        Self::invalid(
            param.clone(),
            format!("unsupported OpenAI compatibility field: {param}"),
        )
    }

    fn into_response(self) -> Response {
        openai_error_response(
            StatusCode::BAD_REQUEST,
            &self.message,
            "invalid_request_error",
            None,
            self.param.as_deref(),
        )
    }
}

/// POST /v1/chat/completions — OpenAI Chat Completions compatibility endpoint.
pub async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    let value = match payload {
        Ok(Json(value)) => value,
        Err(error) => return json_rejection_response(error),
    };
    let (request, protocol) = match convert_chat_request(value) {
        Ok(converted) => converted,
        Err(error) => return error.into_response(),
    };
    crate::routes::execute_messages_request(state, headers, request, protocol).await
}

/// POST /v1/responses — native, stateless Codex Responses endpoint.
pub async fn responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    payload: Result<Json<Value>, JsonRejection>,
) -> Response {
    let value = match payload {
        Ok(Json(value)) => value,
        Err(error) => return json_rejection_response(error),
    };
    let (request, model) = match validate_native_responses_request(value) {
        Ok(converted) => converted,
        Err(error) => return error.into_response(),
    };
    crate::routes::execute_responses_request(state, headers, request, model).await
}

/// Codex treats HTTP 426 as an explicit instruction to use Responses over
/// HTTP instead of retrying the optional WebSocket transport.
pub async fn responses_websocket_upgrade_required() -> Response {
    openai_error_response(
        StatusCode::UPGRADE_REQUIRED,
        "Responses WebSocket transport is not supported; retry with HTTPS streaming",
        "invalid_request_error",
        Some("responses_websocket_not_supported"),
        None,
    )
}

fn json_rejection_response(error: JsonRejection) -> Response {
    let status = error.status();
    openai_error_response(
        status,
        &error.body_text(),
        "invalid_request_error",
        Some("invalid_json"),
        None,
    )
}

fn convert_chat_request(
    value: Value,
) -> Result<(MessagesRequest, DownstreamProtocol), RequestError> {
    let object = request_object(value)?;
    reject_unknown_fields(&object, CHAT_FIELDS)?;
    reject_chat_semantic_gaps(&object)?;

    let model = required_string(&object, "model")?;
    let messages_value = object
        .get("messages")
        .ok_or_else(|| RequestError::invalid("messages", "missing required field: messages"))?;
    let messages_array = messages_value
        .as_array()
        .ok_or_else(|| RequestError::invalid("messages", "messages must be an array"))?;
    let (system, messages) = convert_chat_messages(messages_array)?;

    let max_tokens = merge_token_limits(
        optional_u32(&object, "max_tokens")?,
        optional_u32(&object, "max_completion_tokens")?,
    )?;
    let stream = optional_bool(&object, "stream")?.unwrap_or(false);
    let include_usage = chat_include_usage(&object)?;
    let (mut tools, tool_choice) =
        convert_tools_and_choice(object.get("tools"), object.get("tool_choice"))?;
    if object.get("tool_choice").and_then(Value::as_str) == Some("none") {
        tools = None;
    }

    let mut extra = HashMap::new();
    copy_extra(
        &object,
        &mut extra,
        &[
            "reasoning_effort",
            "verbosity",
            "service_tier",
            "prompt_cache_key",
            "safety_identifier",
            "parallel_tool_calls",
        ],
    );

    let request = MessagesRequest {
        model: model.clone(),
        system,
        messages,
        max_tokens,
        temperature: optional_f32(&object, "temperature")?,
        top_p: optional_f32(&object, "top_p")?,
        top_k: None,
        stop_sequences: optional_stop(&object, "stop")?,
        stream,
        tools,
        tool_choice,
        thinking: None,
        metadata: object.get("metadata").filter(|v| !v.is_null()).cloned(),
        extra,
    };
    Ok((
        request,
        DownstreamProtocol::chat_completions(model, include_usage),
    ))
}

fn validate_native_responses_request(value: Value) -> Result<(Value, String), RequestError> {
    let mut object = request_object(value)?;
    let model = required_string(&object, "model")?;
    match object.get("input") {
        None => {
            return Err(RequestError::invalid(
                "input",
                "missing required field: input",
            ));
        }
        Some(Value::String(_) | Value::Array(_)) => {}
        Some(_) => {
            return Err(RequestError::invalid(
                "input",
                "input must be a string or an array of Responses input items",
            ));
        }
    }
    if optional_bool(&object, "stream")? != Some(true) {
        return Err(RequestError::invalid(
            "stream",
            "the native Responses endpoint requires stream=true",
        ));
    }
    for field in ["store", "background"] {
        if optional_bool(&object, field)? == Some(true) {
            return Err(RequestError::unsupported(field));
        }
    }
    object.insert("store".to_string(), Value::Bool(false));
    object.remove("background");
    reject_non_null(&object, "previous_response_id")?;
    reject_non_null(&object, "conversation")?;
    object.remove("previous_response_id");
    object.remove("conversation");
    Ok((Value::Object(object), model))
}

fn request_object(value: Value) -> Result<Map<String, Value>, RequestError> {
    value
        .as_object()
        .cloned()
        .ok_or_else(|| RequestError::invalid("body", "request body must be a JSON object"))
}

fn reject_unknown_fields(
    object: &Map<String, Value>,
    allowed: &[&str],
) -> Result<(), RequestError> {
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(RequestError::unsupported(key));
    }
    Ok(())
}

fn reject_chat_semantic_gaps(object: &Map<String, Value>) -> Result<(), RequestError> {
    reject_true(object, "store")?;
    if let Some(n) = optional_u32(object, "n")?
        && n != 1
    {
        return Err(RequestError::invalid("n", "only n=1 is supported"));
    }
    reject_true(object, "logprobs")?;
    reject_positive_u32(object, "top_logprobs")?;
    reject_nonzero_number(object, "frequency_penalty")?;
    reject_nonzero_number(object, "presence_penalty")?;
    reject_non_null(object, "seed")?;
    reject_non_null(object, "audio")?;
    reject_non_null(object, "prediction")?;
    reject_non_null(object, "web_search_options")?;
    reject_non_null(object, "functions")?;
    reject_non_null(object, "function_call")?;

    if let Some(modalities) = object.get("modalities").filter(|v| !v.is_null()) {
        let text_only = modalities
            .as_array()
            .is_some_and(|values| values.iter().all(|value| value.as_str() == Some("text")));
        if !text_only {
            return Err(RequestError::invalid(
                "modalities",
                "only text output modality is supported",
            ));
        }
    }
    if let Some(format) = object.get("response_format").filter(|v| !v.is_null())
        && format.get("type").and_then(Value::as_str) != Some("text")
    {
        return Err(RequestError::invalid(
            "response_format",
            "structured response formats are not supported by this compatibility endpoint",
        ));
    }
    validate_parallel_tool_calls(object)?;
    Ok(())
}

fn validate_parallel_tool_calls(object: &Map<String, Value>) -> Result<(), RequestError> {
    optional_bool(object, "parallel_tool_calls")?;
    Ok(())
}

fn reject_true(object: &Map<String, Value>, key: &str) -> Result<(), RequestError> {
    if object.get(key).and_then(Value::as_bool) == Some(true) {
        return Err(RequestError::unsupported(key));
    }
    Ok(())
}

fn reject_non_null(object: &Map<String, Value>, key: &str) -> Result<(), RequestError> {
    if object.get(key).is_some_and(|value| !value.is_null()) {
        return Err(RequestError::unsupported(key));
    }
    Ok(())
}

fn reject_positive_u32(object: &Map<String, Value>, key: &str) -> Result<(), RequestError> {
    if optional_u32(object, key)?.unwrap_or(0) > 0 {
        return Err(RequestError::unsupported(key));
    }
    Ok(())
}

fn reject_nonzero_number(object: &Map<String, Value>, key: &str) -> Result<(), RequestError> {
    if let Some(value) = object.get(key).filter(|value| !value.is_null()) {
        let number = value
            .as_f64()
            .ok_or_else(|| RequestError::invalid(key, format!("{key} must be a number")))?;
        if number != 0.0 {
            return Err(RequestError::unsupported(key));
        }
    }
    Ok(())
}

fn required_string(object: &Map<String, Value>, key: &str) -> Result<String, RequestError> {
    object
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| RequestError::invalid(key, format!("{key} must be a non-empty string")))
}

fn optional_string(object: &Map<String, Value>, key: &str) -> Result<Option<String>, RequestError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => Err(RequestError::invalid(
            key,
            format!("{key} must be a string"),
        )),
    }
}

fn optional_bool(object: &Map<String, Value>, key: &str) -> Result<Option<bool>, RequestError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(RequestError::invalid(
            key,
            format!("{key} must be a boolean"),
        )),
    }
}

fn optional_u32(object: &Map<String, Value>, key: &str) -> Result<Option<u32>, RequestError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| {
                RequestError::invalid(key, format!("{key} must be a non-negative 32-bit integer"))
            }),
    }
}

fn optional_f32(object: &Map<String, Value>, key: &str) -> Result<Option<f32>, RequestError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .map(|value| Some(value as f32))
            .ok_or_else(|| RequestError::invalid(key, format!("{key} must be a finite number"))),
    }
}

fn merge_token_limits(
    legacy: Option<u32>,
    current: Option<u32>,
) -> Result<Option<u32>, RequestError> {
    match (legacy, current) {
        (Some(legacy), Some(current)) if legacy != current => Err(RequestError::invalid(
            "max_completion_tokens",
            "max_tokens and max_completion_tokens must match when both are supplied",
        )),
        (_, Some(current)) => Ok(Some(current)),
        (legacy, None) => Ok(legacy),
    }
}

fn optional_stop(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<Vec<String>>, RequestError> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(stop)) => Ok(Some(vec![stop.clone()])),
        Some(Value::Array(stops)) => stops
            .iter()
            .map(|stop| {
                stop.as_str()
                    .map(ToOwned::to_owned)
                    .ok_or_else(|| RequestError::invalid(key, "stop must contain only strings"))
            })
            .collect::<Result<Vec<_>, _>>()
            .map(Some),
        Some(_) => Err(RequestError::invalid(
            key,
            "stop must be a string or an array of strings",
        )),
    }
}

fn chat_include_usage(object: &Map<String, Value>) -> Result<bool, RequestError> {
    match object.get("stream_options") {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Object(options)) => {
            reject_unknown_fields(options, &["include_usage", "include_obfuscation"])?;
            match options.get("include_usage") {
                None | Some(Value::Null) => Ok(false),
                Some(Value::Bool(value)) => Ok(*value),
                Some(_) => Err(RequestError::invalid(
                    "stream_options.include_usage",
                    "stream_options.include_usage must be a boolean",
                )),
            }
        }
        Some(_) => Err(RequestError::invalid(
            "stream_options",
            "stream_options must be an object",
        )),
    }
}

fn copy_extra(object: &Map<String, Value>, extra: &mut HashMap<String, Value>, keys: &[&str]) {
    for key in keys {
        if let Some(value) = object.get(*key).filter(|value| !value.is_null()) {
            extra.insert((*key).to_string(), value.clone());
        }
    }
}

fn convert_chat_messages(
    messages: &[Value],
) -> Result<(Option<SystemPrompt>, Vec<Message>), RequestError> {
    let mut system_parts = Vec::new();
    let mut converted = Vec::new();

    for (index, message) in messages.iter().enumerate() {
        let path = format!("messages[{index}]");
        let object = message
            .as_object()
            .ok_or_else(|| RequestError::invalid(&path, format!("{path} must be an object")))?;
        let role = object.get("role").and_then(Value::as_str).ok_or_else(|| {
            RequestError::invalid(format!("{path}.role"), "message role must be a string")
        })?;

        match role {
            "system" | "developer" => {
                let text = text_only_content(
                    object.get("content").unwrap_or(&Value::Null),
                    &format!("{path}.content"),
                )?;
                if !text.is_empty() {
                    system_parts.push(text);
                }
            }
            "user" => converted.push(Message {
                role: Role::User,
                content: MessageContent::Blocks(chat_content_blocks(
                    object.get("content").unwrap_or(&Value::Null),
                    &format!("{path}.content"),
                    false,
                )?),
            }),
            "assistant" => {
                let mut blocks = Vec::new();
                if let Some(reasoning) = object.get("reasoning_content").and_then(Value::as_str)
                    && !reasoning.is_empty()
                {
                    blocks.push(Content::Thinking {
                        thinking: reasoning.to_string(),
                        signature: None,
                    });
                }
                blocks.extend(chat_content_blocks(
                    object.get("content").unwrap_or(&Value::Null),
                    &format!("{path}.content"),
                    true,
                )?);
                if let Some(tool_calls) = object.get("tool_calls").filter(|value| !value.is_null())
                {
                    blocks.extend(chat_tool_calls(tool_calls, &format!("{path}.tool_calls"))?);
                }
                converted.push(Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(blocks),
                });
            }
            "tool" => {
                let tool_call_id = object
                    .get("tool_call_id")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    .ok_or_else(|| {
                        RequestError::invalid(
                            format!("{path}.tool_call_id"),
                            "tool messages require tool_call_id",
                        )
                    })?;
                let content = tool_result_content(
                    object.get("content").unwrap_or(&Value::Null),
                    &format!("{path}.content"),
                )?;
                converted.push(Message {
                    role: Role::User,
                    content: MessageContent::Blocks(vec![Content::ToolResult {
                        tool_use_id: tool_call_id.to_string(),
                        content,
                        is_error: None,
                    }]),
                });
            }
            _ => {
                return Err(RequestError::invalid(
                    format!("{path}.role"),
                    format!("unsupported message role: {role}"),
                ));
            }
        }
    }

    let system = (!system_parts.is_empty()).then(|| SystemPrompt::Text(system_parts.join("\n\n")));
    Ok((system, converted))
}

fn chat_content_blocks(
    value: &Value,
    path: &str,
    assistant: bool,
) -> Result<Vec<Content>, RequestError> {
    match value {
        Value::Null => Ok(Vec::new()),
        Value::String(text) => Ok(vec![Content::Text { text: text.clone() }]),
        Value::Array(parts) => parts
            .iter()
            .enumerate()
            .map(|(index, part)| {
                let part_path = format!("{path}[{index}]");
                let object = part.as_object().ok_or_else(|| {
                    RequestError::invalid(&part_path, "content part must be an object")
                })?;
                match object.get("type").and_then(Value::as_str) {
                    Some("text") | Some("input_text") | Some("output_text") => Ok(Content::Text {
                        text: object
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    }),
                    Some("refusal") if assistant => Ok(Content::Text {
                        text: object
                            .get("refusal")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string(),
                    }),
                    Some("image_url") if !assistant => {
                        let url = image_url_value(object.get("image_url"), &part_path)?;
                        Ok(image_content(url))
                    }
                    Some(kind) => Err(RequestError::invalid(
                        format!("{part_path}.type"),
                        format!("unsupported content part type: {kind}"),
                    )),
                    None => Err(RequestError::invalid(
                        format!("{part_path}.type"),
                        "content part type is required",
                    )),
                }
            })
            .collect(),
        _ => Err(RequestError::invalid(
            path,
            "content must be a string, array, or null",
        )),
    }
}

fn chat_tool_calls(value: &Value, path: &str) -> Result<Vec<Content>, RequestError> {
    let calls = value
        .as_array()
        .ok_or_else(|| RequestError::invalid(path, "tool_calls must be an array"))?;
    calls
        .iter()
        .enumerate()
        .map(|(index, call)| {
            let call_path = format!("{path}[{index}]");
            let object = call
                .as_object()
                .ok_or_else(|| RequestError::invalid(&call_path, "tool call must be an object"))?;
            if object.get("type").and_then(Value::as_str) != Some("function") {
                return Err(RequestError::invalid(
                    format!("{call_path}.type"),
                    "only function tool calls are supported",
                ));
            }
            let id = object
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    RequestError::invalid(format!("{call_path}.id"), "tool call id is required")
                })?;
            let function = object
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    RequestError::invalid(
                        format!("{call_path}.function"),
                        "tool call function must be an object",
                    )
                })?;
            let name = required_string(function, "name")?;
            let arguments = function
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let input = serde_json::from_str(arguments).map_err(|error| {
                RequestError::invalid(
                    format!("{call_path}.function.arguments"),
                    format!("tool call arguments must be valid JSON: {error}"),
                )
            })?;
            Ok(Content::ToolUse {
                id: id.to_string(),
                name,
                input,
            })
        })
        .collect()
}

fn text_only_content(value: &Value, path: &str) -> Result<String, RequestError> {
    match value {
        Value::Null => Ok(String::new()),
        Value::String(text) => Ok(text.clone()),
        Value::Array(parts) => {
            let mut text = String::new();
            for (index, part) in parts.iter().enumerate() {
                let part_path = format!("{path}[{index}]");
                let object = part.as_object().ok_or_else(|| {
                    RequestError::invalid(&part_path, "content part must be an object")
                })?;
                match object.get("type").and_then(Value::as_str) {
                    Some("text") | Some("input_text") | Some("output_text") => {
                        text.push_str(
                            object
                                .get("text")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        );
                    }
                    Some(kind) => {
                        return Err(RequestError::invalid(
                            format!("{part_path}.type"),
                            format!("system content does not support {kind}"),
                        ));
                    }
                    None => {
                        return Err(RequestError::invalid(
                            format!("{part_path}.type"),
                            "content part type is required",
                        ));
                    }
                }
            }
            Ok(text)
        }
        _ => Err(RequestError::invalid(
            path,
            "content must be a string, array, or null",
        )),
    }
}

fn tool_result_content(value: &Value, path: &str) -> Result<Option<Value>, RequestError> {
    match value {
        Value::Null => Ok(None),
        Value::String(_) => Ok(Some(value.clone())),
        Value::Array(parts) => {
            let mut normalized = Vec::new();
            for (index, part) in parts.iter().enumerate() {
                let object = part.as_object().ok_or_else(|| {
                    RequestError::invalid(
                        format!("{path}[{index}]"),
                        "tool result content part must be an object",
                    )
                })?;
                match object.get("type").and_then(Value::as_str) {
                    Some("text") => normalized.push(json!({
                        "type": "text",
                        "text": object.get("text").and_then(Value::as_str).unwrap_or_default(),
                    })),
                    Some(kind) => {
                        return Err(RequestError::invalid(
                            format!("{path}[{index}].type"),
                            format!("unsupported tool result content type: {kind}"),
                        ));
                    }
                    None => {
                        return Err(RequestError::invalid(
                            format!("{path}[{index}].type"),
                            "tool result content part type is required",
                        ));
                    }
                }
            }
            Ok(Some(Value::Array(normalized)))
        }
        _ => Err(RequestError::invalid(
            path,
            "tool result content must be a string, array, or null",
        )),
    }
}

fn image_url_value(value: Option<&Value>, path: &str) -> Result<String, RequestError> {
    match value {
        Some(Value::String(url)) if !url.is_empty() => Ok(url.clone()),
        Some(Value::Object(image)) => image
            .get("url")
            .and_then(Value::as_str)
            .filter(|url| !url.is_empty())
            .map(ToOwned::to_owned)
            .ok_or_else(|| {
                RequestError::invalid(format!("{path}.image_url"), "image URL is required")
            }),
        _ => Err(RequestError::invalid(
            format!("{path}.image_url"),
            "image_url must be a non-empty string or object with url",
        )),
    }
}

fn image_content(url: String) -> Content {
    if let Some(data_url) = url.strip_prefix("data:")
        && let Some((media_and_encoding, data)) = data_url.split_once(',')
        && let Some(media_type) = media_and_encoding.strip_suffix(";base64")
    {
        return Content::Unknown(json!({
            "type": "image",
            "source": {
                "type": "base64",
                "media_type": media_type,
                "data": data,
            }
        }));
    }
    Content::Unknown(json!({
        "type": "image",
        "source": {
            "type": "url",
            "url": url,
        }
    }))
}

fn convert_tools_and_choice(
    tools: Option<&Value>,
    choice: Option<&Value>,
) -> Result<(Option<Vec<Tool>>, Option<Value>), RequestError> {
    let tools = convert_tools(tools)?;
    let choice = convert_tool_choice(choice)?;
    Ok((tools, choice))
}

fn convert_tools(value: Option<&Value>) -> Result<Option<Vec<Tool>>, RequestError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| RequestError::invalid("tools", "tools must be an array"))?;
    let tools = values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let path = format!("tools[{index}]");
            let object = value
                .as_object()
                .ok_or_else(|| RequestError::invalid(&path, "tool must be an object"))?;
            if object.get("type").and_then(Value::as_str) != Some("function") {
                return Err(RequestError::invalid(
                    format!("{path}.type"),
                    "only function tools are supported",
                ));
            }
            let function = object
                .get("function")
                .and_then(Value::as_object)
                .ok_or_else(|| {
                    RequestError::invalid(
                        format!("{path}.function"),
                        "function tool definition is required",
                    )
                })?;
            if function.get("strict").and_then(Value::as_bool) == Some(true) {
                return Err(RequestError::invalid(
                    format!("{path}.strict"),
                    "strict function schemas are not supported across all providers",
                ));
            }
            Ok(Tool {
                name: required_string(function, "name")?,
                description: optional_string(function, "description")?,
                input_schema: function
                    .get("parameters")
                    .or_else(|| function.get("input_schema"))
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                extra: Map::new(),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok((!tools.is_empty()).then_some(tools))
}

fn convert_tool_choice(value: Option<&Value>) -> Result<Option<Value>, RequestError> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    if let Some(choice) = value.as_str() {
        return match choice {
            "none" => Ok(None),
            "auto" => Ok(Some(json!({"type": "auto"}))),
            "required" | "any" => Ok(Some(json!({"type": "any"}))),
            _ => Err(RequestError::invalid(
                "tool_choice",
                format!("unsupported tool_choice: {choice}"),
            )),
        };
    }

    let object = value.as_object().ok_or_else(|| {
        RequestError::invalid("tool_choice", "tool_choice must be a string or object")
    })?;
    if object.get("type").and_then(Value::as_str) != Some("function") {
        return Err(RequestError::invalid(
            "tool_choice.type",
            "only named function tool_choice objects are supported",
        ));
    }
    let function = object
        .get("function")
        .and_then(Value::as_object)
        .ok_or_else(|| {
            RequestError::invalid(
                "tool_choice.function",
                "tool_choice.function must be an object",
            )
        })?;
    Ok(Some(json!({
        "type": "tool",
        "name": required_string(function, "name")?,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chat_request_with_token_limits(
        max_tokens: Option<u32>,
        max_completion_tokens: Option<u32>,
    ) -> Value {
        let mut request = json!({
            "model": "gpt-test",
            "messages": [{"role": "user", "content": "hello"}]
        });
        let object = request.as_object_mut().unwrap();
        if let Some(value) = max_tokens {
            object.insert("max_tokens".to_string(), json!(value));
        }
        if let Some(value) = max_completion_tokens {
            object.insert("max_completion_tokens".to_string(), json!(value));
        }
        request
    }

    #[test]
    fn reconciles_chat_completion_token_limit_aliases() {
        for (legacy, current, expected) in [
            (Some(4096), None, 4096),
            (None, Some(1200), 1200),
            (Some(16384), Some(16384), 16384),
        ] {
            let (request, _) =
                convert_chat_request(chat_request_with_token_limits(legacy, current)).unwrap();
            assert_eq!(request.max_tokens, Some(expected));
        }

        let error = convert_chat_request(chat_request_with_token_limits(Some(4096), Some(1200)))
            .unwrap_err();
        assert_eq!(error.param.as_deref(), Some("max_completion_tokens"));
        assert_eq!(
            error.message,
            "max_tokens and max_completion_tokens must match when both are supplied"
        );
        assert_eq!(error.into_response().status(), StatusCode::BAD_REQUEST);
    }

    #[test]
    fn converts_chat_messages_tools_and_usage_option() {
        let (request, protocol) = convert_chat_request(json!({
            "model": "gpt-test",
            "messages": [
                {"role": "developer", "content": "Be concise"},
                {"role": "user", "content": "weather?"},
                {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "weather", "arguments": "{\"city\":\"Paris\"}"}
                    }]
                },
                {"role": "tool", "tool_call_id": "call_1", "content": "sunny"}
            ],
            "stream": true,
            "stream_options": {"include_usage": true},
            "tools": [{
                "type": "function",
                "function": {
                    "name": "weather",
                    "parameters": {"type": "object"}
                }
            }]
        }))
        .unwrap();

        assert_eq!(request.messages.len(), 3);
        assert!(matches!(request.system, Some(SystemPrompt::Text(_))));
        assert!(request.tools.is_some());
        assert!(matches!(
            protocol,
            DownstreamProtocol::ChatCompletions(ref options) if options.include_usage
        ));
    }

    #[test]
    fn native_responses_preserves_codex_items_and_unknown_fields() {
        let request = json!({
            "model": "gpt-test",
            "stream": true,
            "future_option": {"enabled": true},
            "input": [
                {
                    "id": "at_stable",
                    "type": "additional_tools",
                    "role": "developer",
                    "tools": [{"type": "function", "name": "weather"}]
                },
                {
                    "type": "configuration_update",
                    "reasoning": {"effort": "high"}
                },
                {
                    "type": "function_call_output",
                    "name": "notifications",
                    "namespace": "slack",
                    "output": "Alice mentioned you"
                },
                {"type": "compaction_trigger"}
            ]
        });
        let (validated, model) = validate_native_responses_request(request.clone()).unwrap();

        assert_eq!(model, "gpt-test");
        assert_eq!(validated["input"], request["input"]);
        assert_eq!(validated["future_option"], request["future_option"]);
        assert_eq!(validated["store"], false);
        assert!(validated.get("background").is_none());
    }

    #[test]
    fn native_responses_requires_streaming() {
        for stream in [None, Some(json!(false)), Some(Value::Null)] {
            let mut request = json!({"model": "gpt-test", "input": "hello"});
            if let Some(stream) = stream {
                request["stream"] = stream;
            }
            let error = validate_native_responses_request(request).unwrap_err();
            assert_eq!(error.param.as_deref(), Some("stream"));
        }
    }

    #[test]
    fn native_responses_accepts_only_standard_input_shapes() {
        for input in [json!(null), json!({"role": "user"}), json!(42)] {
            let error = validate_native_responses_request(json!({
                "model": "gpt-test",
                "input": input,
                "stream": true
            }))
            .unwrap_err();
            assert_eq!(error.param.as_deref(), Some("input"));
            assert!(error.message.contains("string or an array"));
        }

        for input in [
            json!("hello"),
            json!([{"role": "user", "content": "hello"}]),
        ] {
            validate_native_responses_request(json!({
                "model": "gpt-test",
                "input": input,
                "stream": true
            }))
            .unwrap();
        }
    }

    #[test]
    fn rejects_stateful_responses_fields() {
        let error = validate_native_responses_request(json!({
            "model": "gpt-test",
            "input": "hello",
            "stream": true,
            "previous_response_id": "resp_old"
        }))
        .unwrap_err();
        assert_eq!(error.param.as_deref(), Some("previous_response_id"));
    }

    #[test]
    fn accepts_parallel_tool_calls_false_for_openai_compatibility_requests() {
        let (chat_request, _) = convert_chat_request(json!({
            "model": "gpt-test",
            "messages": [{"role": "user", "content": "hello"}],
            "parallel_tool_calls": false
        }))
        .unwrap();
        assert_eq!(
            chat_request.extra.get("parallel_tool_calls"),
            Some(&json!(false))
        );

        let (responses_request, _) = validate_native_responses_request(json!({
            "model": "gpt-test",
            "input": "hello",
            "stream": true,
            "parallel_tool_calls": false
        }))
        .unwrap();
        assert_eq!(
            responses_request.get("parallel_tool_calls"),
            Some(&json!(false))
        );
    }

    #[test]
    fn rejects_non_boolean_parallel_tool_calls() {
        let error = convert_chat_request(json!({
            "model": "gpt-test",
            "messages": [{"role": "user", "content": "hello"}],
            "parallel_tool_calls": "false"
        }))
        .unwrap_err();

        assert_eq!(error.param.as_deref(), Some("parallel_tool_calls"));
        assert_eq!(error.message, "parallel_tool_calls must be a boolean");
    }

    #[test]
    fn normalizes_data_url_images() {
        let (request, _) = convert_chat_request(json!({
            "model": "gpt-test",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image_url",
                    "image_url": {"url": "data:image/png;base64,AAAA"}
                }]
            }]
        }))
        .unwrap();
        let MessageContent::Blocks(blocks) = &request.messages[0].content else {
            panic!("expected blocks");
        };
        let Content::Unknown(image) = &blocks[0] else {
            panic!("expected image");
        };
        assert_eq!(image["source"]["media_type"], "image/png");
    }
}
