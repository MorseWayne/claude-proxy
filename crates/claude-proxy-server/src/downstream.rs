use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::body::Body;
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use claude_proxy_core::{ErrorResponse, SseEvent};
use claude_proxy_providers::provider::{
    NativeProviderEvent, ProviderError, ProviderEvent, UpstreamErrorMetadata,
};
use serde_json::{Map, Value, json};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub(crate) enum DownstreamProtocol {
    Anthropic,
    ChatCompletions(ChatCompletionsOptions),
    Responses(ResponsesOptions),
}

#[derive(Debug, Clone)]
pub(crate) struct ChatCompletionsOptions {
    pub model: String,
    pub include_usage: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct ResponsesOptions {
    pub model: String,
    pub response_fields: Map<String, Value>,
}

impl DownstreamProtocol {
    pub(crate) fn anthropic() -> Self {
        Self::Anthropic
    }

    pub(crate) fn chat_completions(model: impl Into<String>, include_usage: bool) -> Self {
        Self::ChatCompletions(ChatCompletionsOptions {
            model: model.into(),
            include_usage,
        })
    }

    pub(crate) fn responses(model: impl Into<String>, response_fields: Map<String, Value>) -> Self {
        Self::Responses(ResponsesOptions {
            model: model.into(),
            response_fields,
        })
    }

    pub(crate) fn stream_encoder(&self) -> StreamEncoder {
        match self {
            Self::Anthropic => StreamEncoder::Anthropic,
            Self::ChatCompletions(options) => {
                StreamEncoder::Chat(ChatStreamEncoder::new(options.clone()))
            }
            Self::Responses(options) => {
                StreamEncoder::Responses(ResponsesStreamEncoder::new(options.clone()))
            }
        }
    }

    pub(crate) fn non_stream_response(&self, mut events: Vec<ProviderEvent>) -> Response {
        match self {
            Self::Anthropic => {
                let normalized = into_normalized_events(events);
                let response_data = crate::non_stream::response_from_events(&normalized)
                    .or_else(|| normalized.last().map(|event| event.data.clone()))
                    .unwrap_or_else(|| json!({"error": "no response from provider"}));
                Json(response_data).into_response()
            }
            Self::ChatCompletions(options) => {
                if let Some(response) = native_chat_completion_response(&events, options) {
                    return Json(response).into_response();
                }
                let normalized = into_normalized_events(events);
                let Some(message) = crate::non_stream::response_from_events(&normalized) else {
                    return protocol_error_response(
                        self,
                        StatusCode::BAD_GATEWAY,
                        &ErrorResponse::api_error("no response from provider"),
                    );
                };
                Json(chat_completion_response(options, &message)).into_response()
            }
            Self::Responses(options) => {
                if let Some(response) = native_responses_response(&mut events, options) {
                    return Json(response).into_response();
                }
                let normalized = into_normalized_events(events);
                let Some(message) = crate::non_stream::response_from_events(&normalized) else {
                    return protocol_error_response(
                        self,
                        StatusCode::BAD_GATEWAY,
                        &ErrorResponse::api_error("no response from provider"),
                    );
                };
                Json(responses_response(options, &message)).into_response()
            }
        }
    }
}

fn into_normalized_events(events: Vec<ProviderEvent>) -> Vec<SseEvent> {
    events
        .into_iter()
        .flat_map(|event| event.into_normalized_events())
        .collect()
}

pub(crate) enum StreamEncoder {
    Anthropic,
    Chat(ChatStreamEncoder),
    Responses(ResponsesStreamEncoder),
}

impl StreamEncoder {
    pub(crate) fn encode_event(&mut self, event: &ProviderEvent) -> Vec<Vec<u8>> {
        match self {
            Self::Anthropic => event
                .normalized_events()
                .iter()
                .map(format_anthropic_event)
                .collect(),
            Self::Chat(encoder) => event
                .native_event()
                .map(|event| match event {
                    NativeProviderEvent::OpenAiResponses(event) => {
                        encoder.encode_native_event(event)
                    }
                })
                .unwrap_or_else(|| {
                    event
                        .normalized_events()
                        .iter()
                        .flat_map(|event| encoder.encode_event(event))
                        .collect()
                }),
            Self::Responses(encoder) => match event.native_event() {
                Some(NativeProviderEvent::OpenAiResponses(event)) => {
                    encoder.encode_native_event(event)
                }
                None => event
                    .normalized_events()
                    .iter()
                    .flat_map(|event| encoder.encode_event(event))
                    .collect(),
            },
        }
    }

    pub(crate) fn encode_error(&mut self, message: impl Into<String>) -> Vec<Vec<u8>> {
        let message = message.into();
        match self {
            Self::Anthropic => vec![format_anthropic_event(&SseEvent {
                event: "error".to_string(),
                data: json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": message,
                    }
                }),
            })],
            Self::Chat(encoder) => encoder.encode_error(message),
            Self::Responses(encoder) => encoder.encode_error(message),
        }
    }

    pub(crate) fn finish(&mut self) -> Vec<Vec<u8>> {
        match self {
            Self::Anthropic => Vec::new(),
            Self::Chat(encoder) => encoder.finish(),
            Self::Responses(encoder) => encoder.finish(),
        }
    }
}

pub(crate) fn protocol_error_response(
    protocol: &DownstreamProtocol,
    status: StatusCode,
    error: &ErrorResponse,
) -> Response {
    match protocol {
        DownstreamProtocol::Anthropic => {
            let body = serde_json::to_string(error).unwrap_or_default();
            Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
        }
        DownstreamProtocol::ChatCompletions(_) | DownstreamProtocol::Responses(_) => {
            let (error_type, code) = openai_error_mapping(&error.error.error_type);
            openai_error_response(status, &error.error.message, error_type, code, None)
        }
    }
}

pub(crate) fn openai_error_response(
    status: StatusCode,
    message: &str,
    error_type: &str,
    code: Option<&str>,
    param: Option<&str>,
) -> Response {
    Json(json!({
        "error": {
            "message": message,
            "type": error_type,
            "param": param,
            "code": code,
        }
    }))
    .into_response_with_status(status)
}

pub(crate) fn provider_error_response(
    protocol: &DownstreamProtocol,
    error: &ProviderError,
) -> Response {
    if matches!(protocol, DownstreamProtocol::Anthropic) {
        unreachable!("Anthropic provider errors use the established route mapping");
    }

    let metadata = error.upstream_metadata();
    let (status, response_error, retry_after, should_retry) =
        match error.without_upstream_metadata() {
            ProviderError::Authentication(message) => (
                StatusCode::UNAUTHORIZED,
                ErrorResponse::authentication(message),
                None,
                false,
            ),
            ProviderError::RateLimited { retry_after } => (
                StatusCode::TOO_MANY_REQUESTS,
                ErrorResponse::rate_limit("rate limited by upstream"),
                *retry_after,
                true,
            ),
            ProviderError::InvalidRequest(message) => (
                StatusCode::BAD_REQUEST,
                ErrorResponse::invalid_request(message),
                None,
                false,
            ),
            ProviderError::RequestTooLarge(message) => (
                StatusCode::PAYLOAD_TOO_LARGE,
                ErrorResponse::invalid_request(message),
                None,
                false,
            ),
            ProviderError::ResponseTooLarge(message) => (
                StatusCode::BAD_GATEWAY,
                ErrorResponse::api_error(message),
                None,
                false,
            ),
            ProviderError::Overloaded {
                message,
                retry_after,
            } => (
                overloaded_status(),
                overloaded_error(message),
                *retry_after,
                true,
            ),
            ProviderError::ModelNotFound(message) => (
                StatusCode::NOT_FOUND,
                ErrorResponse::not_found(message),
                None,
                false,
            ),
            ProviderError::Timeout => (
                StatusCode::GATEWAY_TIMEOUT,
                ErrorResponse::timeout("upstream request timed out"),
                None,
                true,
            ),
            ProviderError::UpstreamError { status, body } => {
                let message = extract_upstream_error_message(body);
                match *status {
                    400 => (
                        StatusCode::BAD_REQUEST,
                        ErrorResponse::invalid_request(&message),
                        None,
                        false,
                    ),
                    413 => (
                        StatusCode::PAYLOAD_TOO_LARGE,
                        ErrorResponse::invalid_request(&message),
                        None,
                        false,
                    ),
                    status if is_retryable_upstream_error_status(status) => {
                        (overloaded_status(), overloaded_error(&message), None, true)
                    }
                    _ => (
                        StatusCode::BAD_GATEWAY,
                        ErrorResponse::api_error(&message),
                        None,
                        false,
                    ),
                }
            }
            ProviderError::ServiceUnavailable(message) => (
                StatusCode::SERVICE_UNAVAILABLE,
                ErrorResponse::api_error(message),
                None,
                true,
            ),
            ProviderError::Network(message) => (
                StatusCode::BAD_GATEWAY,
                ErrorResponse::api_error(&format!("network error: {message}")),
                None,
                true,
            ),
            ProviderError::WithUpstreamMetadata { .. } => unreachable!(),
        };

    let mut response = protocol_error_response(protocol, status, &response_error);
    if should_retry {
        response
            .headers_mut()
            .insert("x-should-retry", HeaderValue::from_static("true"));
    }
    if let Some(seconds) = retry_after
        && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
    {
        response.headers_mut().insert("retry-after", value);
    }
    attach_upstream_error_headers(&mut response, metadata);
    response
}

trait JsonStatusResponse {
    fn into_response_with_status(self, status: StatusCode) -> Response;
}

impl JsonStatusResponse for Json<Value> {
    fn into_response_with_status(self, status: StatusCode) -> Response {
        (status, self).into_response()
    }
}

fn openai_error_mapping(error_type: &str) -> (&'static str, Option<&'static str>) {
    match error_type {
        "authentication_error" => ("invalid_request_error", Some("invalid_api_key")),
        "rate_limit_error" => ("rate_limit_error", Some("rate_limit_exceeded")),
        "invalid_request_error" => ("invalid_request_error", None),
        "not_found_error" => ("invalid_request_error", Some("model_not_found")),
        "overloaded_error" => ("server_error", Some("server_overloaded")),
        "timeout_error" => ("server_error", Some("upstream_timeout")),
        _ => ("server_error", None),
    }
}

fn overloaded_status() -> StatusCode {
    StatusCode::from_u16(529).unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
}

fn overloaded_error(message: &str) -> ErrorResponse {
    ErrorResponse {
        r#type: "error".to_string(),
        error: claude_proxy_core::AnthropicError {
            error_type: "overloaded_error".to_string(),
            message: message.to_string(),
        },
    }
}

fn is_retryable_upstream_error_status(status: u16) -> bool {
    matches!(status, 408 | 409 | 500..=599)
}

fn extract_upstream_error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| {
            if body.trim().is_empty() {
                "upstream unavailable".to_string()
            } else {
                body.to_string()
            }
        })
}

fn attach_upstream_error_headers(
    response: &mut Response,
    metadata: Option<&UpstreamErrorMetadata>,
) {
    let Some(metadata) = metadata else {
        return;
    };
    if let Ok(value) = HeaderValue::from_str(&metadata.status.to_string()) {
        response.headers_mut().insert("x-upstream-status", value);
    }
    if let Some(request_id) = metadata.request_id.as_deref()
        && let Ok(value) = HeaderValue::from_str(request_id)
    {
        response
            .headers_mut()
            .insert("x-upstream-request-id", value);
    }
    if let Some(seconds) = metadata.retry_after
        && !response.headers().contains_key("retry-after")
        && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
    {
        response.headers_mut().insert("retry-after", value);
    }
}

fn format_anthropic_event(event: &SseEvent) -> Vec<u8> {
    let data = serde_json::to_string(&event.data).unwrap_or_default();
    if event.event.is_empty() {
        format!("data: {data}\n\n").into_bytes()
    } else {
        format!("event: {}\ndata: {data}\n\n", event.event).into_bytes()
    }
}

fn data_frame(value: &Value) -> Vec<u8> {
    format!(
        "data: {}\n\n",
        serde_json::to_string(value).unwrap_or_default()
    )
    .into_bytes()
}

fn responses_frame(event: &str, value: &Value) -> Vec<u8> {
    format!(
        "event: {event}\ndata: {}\n\n",
        serde_json::to_string(value).unwrap_or_default()
    )
    .into_bytes()
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[derive(Debug, Clone, Copy, Default)]
struct Usage {
    input_tokens: u64,
    output_tokens: u64,
    reasoning_tokens: u64,
    cached_tokens: u64,
}

impl Usage {
    fn merge(&mut self, value: &Value) {
        let cache_creation = value["cache_creation_input_tokens"].as_u64().unwrap_or(0);
        let cached = value["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .or_else(|| value["cache_read_input_tokens"].as_u64())
            .unwrap_or(0);
        let input = value["input_tokens"].as_u64().unwrap_or(0);
        let input = if value.get("input_tokens_details").is_some() {
            input
        } else {
            input.saturating_add(cache_creation).saturating_add(cached)
        };
        self.input_tokens = self.input_tokens.max(input);
        self.output_tokens = self
            .output_tokens
            .max(value["output_tokens"].as_u64().unwrap_or(0));
        self.reasoning_tokens = self.reasoning_tokens.max(
            value["output_tokens_details"]["reasoning_tokens"]
                .as_u64()
                .or_else(|| value["reasoning_output_tokens"].as_u64())
                .unwrap_or(0),
        );
        self.cached_tokens = self.cached_tokens.max(cached);
    }

    fn total_input(self) -> u64 {
        self.input_tokens
    }

    fn chat_json(self) -> Value {
        json!({
            "prompt_tokens": self.total_input(),
            "completion_tokens": self.output_tokens,
            "total_tokens": self.total_input().saturating_add(self.output_tokens),
            "prompt_tokens_details": {
                "cached_tokens": self.cached_tokens,
            },
            "completion_tokens_details": {
                "reasoning_tokens": self.reasoning_tokens,
            }
        })
    }

    fn responses_json(self) -> Value {
        json!({
            "input_tokens": self.total_input(),
            "input_tokens_details": {
                "cached_tokens": self.cached_tokens,
            },
            "output_tokens": self.output_tokens,
            "output_tokens_details": {
                "reasoning_tokens": self.reasoning_tokens,
            },
            "total_tokens": self.total_input().saturating_add(self.output_tokens),
        })
    }
}

fn usage_from_message(message: &Value) -> Usage {
    let mut usage = Usage::default();
    usage.merge(&message["usage"]);
    usage
}

fn stop_reason(message: &Value) -> &str {
    message["stop_reason"].as_str().unwrap_or("end_turn")
}

fn chat_finish_reason(reason: &str) -> &str {
    match reason {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        "refusal" => "content_filter",
        "end_turn" | "stop_sequence" => "stop",
        other => other,
    }
}

fn response_output_missing_or_empty(response: &Value) -> bool {
    response
        .get("output")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
}

fn reconstructed_responses_output(events: &[ProviderEvent]) -> Option<Vec<Value>> {
    let normalized = events
        .iter()
        .flat_map(ProviderEvent::normalized_events)
        .cloned()
        .collect::<Vec<_>>();
    let message = crate::non_stream::response_from_events(&normalized)?;
    let output = responses_output_items(&message);
    (!output.is_empty()).then_some(output)
}

fn native_chat_completion_response(
    events: &[ProviderEvent],
    options: &ChatCompletionsOptions,
) -> Option<Value> {
    let response = events.iter().rev().find_map(|event| {
        let NativeProviderEvent::OpenAiResponses(event) = event.native_event()?;
        matches!(
            event.data["type"].as_str(),
            Some("response.completed" | "response.incomplete" | "response.failed")
        )
        .then(|| event.data.get("response"))
        .flatten()
    })?;
    let reconstructed_output = response_output_missing_or_empty(response)
        .then(|| reconstructed_responses_output(events))
        .flatten();
    let output = response["output"]
        .as_array()
        .filter(|output| !output.is_empty())
        .or(reconstructed_output.as_ref());
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();
    for item in output.into_iter().flatten() {
        match item["type"].as_str() {
            Some("message") => {
                for part in item["content"].as_array().into_iter().flatten() {
                    match part["type"].as_str() {
                        Some("output_text") => {
                            text.push_str(part["text"].as_str().unwrap_or_default())
                        }
                        Some("refusal") => {
                            text.push_str(part["refusal"].as_str().unwrap_or_default())
                        }
                        _ => {}
                    }
                }
            }
            Some("reasoning") => {
                for part in item["summary"].as_array().into_iter().flatten() {
                    reasoning.push_str(part["text"].as_str().unwrap_or_default());
                }
                for part in item["content"].as_array().into_iter().flatten() {
                    reasoning.push_str(part["text"].as_str().unwrap_or_default());
                }
            }
            Some("function_call" | "custom_tool_call") => {
                tool_calls.push(json!({
                    "id": item["call_id"].as_str().or_else(|| item["id"].as_str()).unwrap_or_default(),
                    "type": "function",
                    "function": {
                        "name": item["name"].as_str().unwrap_or_default(),
                        "arguments": item["arguments"].as_str()
                            .or_else(|| item["input"].as_str())
                            .unwrap_or("{}"),
                    }
                }));
            }
            Some("web_search_call") => {
                tool_calls.push(json!({
                    "id": item["id"].as_str().unwrap_or_default(),
                    "type": "function",
                    "function": {
                        "name": "web_search",
                        "arguments": json!({
                            "query": item["action"]["query"].as_str()
                                .or_else(|| item["query"].as_str())
                                .unwrap_or_default()
                        }).to_string(),
                    }
                }));
            }
            _ => {}
        }
    }
    let mut assistant = json!({
        "role": "assistant",
        "content": if text.is_empty() && !tool_calls.is_empty() {
            Value::Null
        } else {
            Value::String(text)
        },
    });
    if !reasoning.is_empty() {
        assistant["reasoning_content"] = Value::String(reasoning);
    }
    if !tool_calls.is_empty() {
        assistant["tool_calls"] = Value::Array(tool_calls);
    }
    let finish_reason = if !assistant["tool_calls"].is_null() {
        "tool_calls"
    } else if response["status"].as_str() == Some("incomplete") {
        "length"
    } else {
        "stop"
    };
    let mut usage = Usage::default();
    usage.merge(&response["usage"]);
    Some(json!({
        "id": format!("chatcmpl-{}", compact_uuid()),
        "object": "chat.completion",
        "created": unix_timestamp(),
        "model": options.model,
        "choices": [{
            "index": 0,
            "message": assistant,
            "logprobs": null,
            "finish_reason": finish_reason,
        }],
        "usage": usage.chat_json(),
        "system_fingerprint": null,
    }))
}

fn chat_completion_response(options: &ChatCompletionsOptions, message: &Value) -> Value {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut tool_calls = Vec::new();

    for block in message["content"].as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => text.push_str(block["text"].as_str().unwrap_or_default()),
            Some("thinking") => reasoning.push_str(block["thinking"].as_str().unwrap_or_default()),
            Some("tool_use") | Some("server_tool_use") => {
                tool_calls.push(json!({
                    "id": block["id"].as_str().unwrap_or_default(),
                    "type": "function",
                    "function": {
                        "name": block["name"].as_str().unwrap_or_default(),
                        "arguments": serde_json::to_string(&block["input"])
                            .unwrap_or_else(|_| "{}".to_string()),
                    }
                }));
            }
            _ => {}
        }
    }

    let mut assistant = json!({
        "role": "assistant",
        "content": if text.is_empty() && !tool_calls.is_empty() {
            Value::Null
        } else {
            Value::String(text)
        },
    });
    if !reasoning.is_empty() {
        assistant["reasoning_content"] = Value::String(reasoning);
    }
    if !tool_calls.is_empty() {
        assistant["tool_calls"] = Value::Array(tool_calls);
    }

    json!({
        "id": chat_completion_id(message),
        "object": "chat.completion",
        "created": unix_timestamp(),
        "model": options.model,
        "choices": [{
            "index": 0,
            "message": assistant,
            "logprobs": null,
            "finish_reason": chat_finish_reason(stop_reason(message)),
        }],
        "usage": usage_from_message(message).chat_json(),
        "system_fingerprint": null,
    })
}

fn chat_completion_id(message: &Value) -> String {
    message["id"]
        .as_str()
        .filter(|id| id.starts_with("chatcmpl-"))
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("chatcmpl-{}", compact_uuid()))
}

fn compact_uuid() -> String {
    Uuid::new_v4().simple().to_string()
}

fn native_chat_tool_identity(item: &Value) -> Option<(&str, &str)> {
    match item["type"].as_str()? {
        "function_call" | "custom_tool_call" => Some((
            item["call_id"]
                .as_str()
                .or_else(|| item["id"].as_str())
                .unwrap_or_default(),
            item["name"].as_str().unwrap_or_default(),
        )),
        "web_search_call" => Some((item["id"].as_str().unwrap_or_default(), "web_search")),
        _ => None,
    }
}

pub(crate) struct ChatStreamEncoder {
    options: ChatCompletionsOptions,
    id: String,
    created: u64,
    started: bool,
    finish_sent: bool,
    done: bool,
    finish_reason: String,
    usage: Usage,
    tool_indices: HashMap<u32, u32>,
    native_tool_started: HashSet<u32>,
    native_arguments: HashMap<u32, String>,
    native_argument_emitted: HashMap<u32, usize>,
    next_tool_index: u32,
}

impl ChatStreamEncoder {
    fn new(options: ChatCompletionsOptions) -> Self {
        Self {
            options,
            id: format!("chatcmpl-{}", compact_uuid()),
            created: unix_timestamp(),
            started: false,
            finish_sent: false,
            done: false,
            finish_reason: "stop".to_string(),
            usage: Usage::default(),
            tool_indices: HashMap::new(),
            native_tool_started: HashSet::new(),
            native_arguments: HashMap::new(),
            native_argument_emitted: HashMap::new(),
            next_tool_index: 0,
        }
    }

    fn encode_event(&mut self, event: &SseEvent) -> Vec<Vec<u8>> {
        if self.done {
            return Vec::new();
        }

        match event.data["type"]
            .as_str()
            .or_else(|| (!event.event.is_empty()).then_some(event.event.as_str()))
        {
            Some("message_start") => {
                if let Some(id) = event.data["message"]["id"].as_str()
                    && id.starts_with("chatcmpl-")
                {
                    self.id = id.to_string();
                }
                self.usage.merge(&event.data["message"]["usage"]);
                self.ensure_started()
            }
            Some("content_block_start") => {
                let mut frames = self.ensure_started();
                let block = &event.data["content_block"];
                if matches!(block["type"].as_str(), Some("tool_use" | "server_tool_use")) {
                    let block_index = event.data["index"].as_u64().unwrap_or(0) as u32;
                    let tool_index = self.tool_index(block_index);
                    let arguments = match block.get("input") {
                        Some(Value::Object(object)) if object.is_empty() => String::new(),
                        Some(Value::Null) | None => String::new(),
                        Some(input) => {
                            serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string())
                        }
                    };
                    frames.push(self.chunk(
                        json!({
                            "tool_calls": [{
                                "index": tool_index,
                                "id": block["id"].as_str().unwrap_or_default(),
                                "type": "function",
                                "function": {
                                    "name": block["name"].as_str().unwrap_or_default(),
                                    "arguments": arguments,
                                }
                            }]
                        }),
                        None,
                    ));
                }
                frames
            }
            Some("content_block_delta") => {
                let mut frames = self.ensure_started();
                let delta = &event.data["delta"];
                let frame = match delta["type"].as_str() {
                    Some("text_delta") => Some(self.chunk(json!({"content": delta["text"]}), None)),
                    Some("thinking_delta") => {
                        Some(self.chunk(json!({"reasoning_content": delta["thinking"]}), None))
                    }
                    Some("input_json_delta") => {
                        let block_index = event.data["index"].as_u64().unwrap_or(0) as u32;
                        let tool_index = self.tool_index(block_index);
                        Some(self.chunk(
                            json!({
                                "tool_calls": [{
                                    "index": tool_index,
                                    "function": {
                                        "arguments": delta["partial_json"].as_str().unwrap_or_default(),
                                    }
                                }]
                            }),
                            None,
                        ))
                    }
                    _ => None,
                };
                if let Some(frame) = frame {
                    frames.push(frame);
                }
                frames
            }
            Some("message_delta") => {
                self.usage.merge(&event.data["usage"]);
                self.finish_reason = chat_finish_reason(
                    event.data["delta"]["stop_reason"]
                        .as_str()
                        .unwrap_or("end_turn"),
                )
                .to_string();
                let mut frames = self.ensure_started();
                if !self.finish_sent {
                    self.finish_sent = true;
                    frames.push(self.chunk(json!({}), Some(self.finish_reason.clone())));
                }
                frames
            }
            Some("message_stop") => self.finish(),
            Some("error") => self.encode_error(
                event.data["error"]["message"]
                    .as_str()
                    .unwrap_or("upstream stream error")
                    .to_string(),
            ),
            _ => Vec::new(),
        }
    }

    fn encode_native_event(&mut self, event: &SseEvent) -> Vec<Vec<u8>> {
        if self.done {
            return Vec::new();
        }
        match event.data["type"].as_str().unwrap_or(event.event.as_str()) {
            "response.created" | "response.in_progress" => {
                if let Some(response) = event.data.get("response") {
                    self.usage.merge(&response["usage"]);
                }
                self.ensure_started()
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                let mut frames = self.ensure_started();
                frames.push(self.chunk(
                    json!({"content": event.data["delta"].as_str().unwrap_or_default()}),
                    None,
                ));
                frames
            }
            "response.reasoning_text.delta" | "response.reasoning_summary_text.delta" => {
                let mut frames = self.ensure_started();
                frames.push(self.chunk(
                    json!({
                        "reasoning_content": event.data["delta"].as_str().unwrap_or_default()
                    }),
                    None,
                ));
                frames
            }
            "response.output_item.added" => {
                let item = &event.data["item"];
                let Some((id, name)) = native_chat_tool_identity(item) else {
                    return self.ensure_started();
                };
                if id.is_empty() || name.is_empty() {
                    return self.ensure_started();
                }
                self.finish_reason = "tool_calls".to_string();
                let output_index = event.data["output_index"].as_u64().unwrap_or(0) as u32;
                let tool_index = self.tool_index(output_index);
                self.native_tool_started.insert(output_index);
                let mut frames = self.ensure_started();
                frames.push(self.chunk(
                    json!({
                        "tool_calls": [{
                            "index": tool_index,
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": ""}
                        }]
                    }),
                    None,
                ));
                frames
            }
            "response.function_call_arguments.delta" | "response.custom_tool_call_input.delta" => {
                let output_index = event.data["output_index"].as_u64().unwrap_or(0) as u32;
                let delta = event.data["delta"].as_str().unwrap_or_default();
                self.native_arguments
                    .entry(output_index)
                    .or_default()
                    .push_str(delta);
                self.emit_native_tool_argument_remainder(output_index, "")
            }
            "response.function_call_arguments.done" | "response.custom_tool_call_input.done" => {
                let output_index = event.data["output_index"].as_u64().unwrap_or(0) as u32;
                let complete = event.data["arguments"]
                    .as_str()
                    .or_else(|| event.data["input"].as_str())
                    .unwrap_or_default();
                self.emit_native_tool_argument_remainder(output_index, complete)
            }
            "response.output_item.done" => {
                let item = &event.data["item"];
                let output_index = event.data["output_index"].as_u64().unwrap_or(0) as u32;
                if matches!(
                    item["type"].as_str(),
                    Some("function_call" | "custom_tool_call")
                ) {
                    let mut frames = Vec::new();
                    if !self.native_tool_started.contains(&output_index)
                        && let Some((id, name)) = native_chat_tool_identity(item)
                        && !id.is_empty()
                        && !name.is_empty()
                    {
                        self.finish_reason = "tool_calls".to_string();
                        let tool_index = self.tool_index(output_index);
                        self.native_tool_started.insert(output_index);
                        frames.extend(self.ensure_started());
                        frames.push(self.chunk(
                            json!({
                                "tool_calls": [{
                                    "index": tool_index,
                                    "id": id,
                                    "type": "function",
                                    "function": {"name": name, "arguments": ""}
                                }]
                            }),
                            None,
                        ));
                    }
                    let complete = item["arguments"]
                        .as_str()
                        .or_else(|| item["input"].as_str())
                        .unwrap_or_default();
                    frames.extend(self.emit_native_tool_argument_remainder(output_index, complete));
                    return frames;
                }
                if item["type"].as_str() != Some("web_search_call") {
                    return Vec::new();
                }
                let arguments = json!({
                    "query": item["action"]["query"]
                        .as_str()
                        .or_else(|| item["query"].as_str())
                        .unwrap_or_default()
                })
                .to_string();
                self.emit_native_tool_argument_remainder(output_index, &arguments)
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                let response = &event.data["response"];
                self.usage.merge(&response["usage"]);
                if event.data["type"].as_str() == Some("response.incomplete")
                    || response["status"].as_str() == Some("incomplete")
                {
                    self.finish_reason = "length".to_string();
                }
                self.finish()
            }
            "error" => self.encode_error(
                event.data["message"]
                    .as_str()
                    .unwrap_or("upstream stream error")
                    .to_string(),
            ),
            _ => Vec::new(),
        }
    }

    fn ensure_started(&mut self) -> Vec<Vec<u8>> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        vec![self.chunk(json!({"role": "assistant", "content": ""}), None)]
    }

    fn emit_native_tool_argument_remainder(
        &mut self,
        output_index: u32,
        complete: &str,
    ) -> Vec<Vec<u8>> {
        if !complete.is_empty() {
            let buffered = self.native_arguments.entry(output_index).or_default();
            if buffered != complete {
                let preserves_prefix = complete.starts_with(buffered.as_str());
                *buffered = complete.to_string();
                if !preserves_prefix {
                    self.native_argument_emitted.insert(output_index, 0);
                }
            }
        }
        if !self.native_tool_started.contains(&output_index) {
            return Vec::new();
        }
        let buffered = self
            .native_arguments
            .get(&output_index)
            .cloned()
            .unwrap_or_default();
        let emitted = self
            .native_argument_emitted
            .get(&output_index)
            .copied()
            .unwrap_or(0)
            .min(buffered.len());
        let remainder = &buffered[buffered.floor_char_boundary(emitted)..];
        if remainder.is_empty() {
            return Vec::new();
        }
        let remainder = remainder.to_string();
        self.native_argument_emitted
            .insert(output_index, buffered.len());
        let tool_index = self.tool_index(output_index);
        vec![self.chunk(
            json!({
                "tool_calls": [{
                    "index": tool_index,
                    "function": {"arguments": remainder}
                }]
            }),
            None,
        )]
    }

    fn tool_index(&mut self, block_index: u32) -> u32 {
        if let Some(index) = self.tool_indices.get(&block_index) {
            return *index;
        }
        let index = self.next_tool_index;
        self.next_tool_index = self.next_tool_index.saturating_add(1);
        self.tool_indices.insert(block_index, index);
        index
    }

    fn chunk(&self, delta: Value, finish_reason: Option<String>) -> Vec<u8> {
        data_frame(&json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.options.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "logprobs": null,
                "finish_reason": finish_reason,
            }],
            "system_fingerprint": null,
        }))
    }

    fn encode_error(&mut self, message: String) -> Vec<Vec<u8>> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        vec![
            data_frame(&json!({
                "error": {
                    "message": message,
                    "type": "server_error",
                    "param": null,
                    "code": "upstream_stream_error",
                }
            })),
            b"data: [DONE]\n\n".to_vec(),
        ]
    }

    fn finish(&mut self) -> Vec<Vec<u8>> {
        if self.done {
            return Vec::new();
        }
        let mut frames = self.ensure_started();
        if !self.finish_sent {
            self.finish_sent = true;
            frames.push(self.chunk(json!({}), Some(self.finish_reason.clone())));
        }
        if self.options.include_usage {
            frames.push(data_frame(&json!({
                "id": self.id,
                "object": "chat.completion.chunk",
                "created": self.created,
                "model": self.options.model,
                "choices": [],
                "usage": self.usage.chat_json(),
                "system_fingerprint": null,
            })));
        }
        frames.push(b"data: [DONE]\n\n".to_vec());
        self.done = true;
        frames
    }
}

#[derive(Debug)]
enum ResponseBlock {
    Text {
        output_index: u32,
        item_id: String,
        text: String,
    },
    Reasoning {
        output_index: u32,
        item_id: String,
        text: String,
        signature: Option<String>,
    },
    Tool {
        output_index: u32,
        item_id: String,
        call_id: String,
        name: String,
        initial_arguments: String,
        argument_deltas: String,
    },
}

impl ResponseBlock {
    fn output_index(&self) -> u32 {
        match self {
            Self::Text { output_index, .. }
            | Self::Reasoning { output_index, .. }
            | Self::Tool { output_index, .. } => *output_index,
        }
    }
}

pub(crate) struct ResponsesStreamEncoder {
    options: ResponsesOptions,
    id: String,
    created_at: u64,
    sequence: u64,
    started: bool,
    done: bool,
    native_mode: bool,
    next_output_index: u32,
    blocks: BTreeMap<u32, ResponseBlock>,
    output: BTreeMap<u32, Value>,
    usage: Usage,
    stop_reason: String,
}

impl ResponsesStreamEncoder {
    fn new(options: ResponsesOptions) -> Self {
        Self {
            options,
            id: format!("resp_{}", compact_uuid()),
            created_at: unix_timestamp(),
            sequence: 0,
            started: false,
            done: false,
            native_mode: false,
            next_output_index: 0,
            blocks: BTreeMap::new(),
            output: BTreeMap::new(),
            usage: Usage::default(),
            stop_reason: "end_turn".to_string(),
        }
    }

    fn encode_event(&mut self, event: &SseEvent) -> Vec<Vec<u8>> {
        if self.done {
            return Vec::new();
        }

        let event_type = event.data["type"]
            .as_str()
            .or_else(|| (!event.event.is_empty()).then_some(event.event.as_str()));
        let mut frames = self.ensure_started();

        match event_type {
            Some("message_start") => {
                self.usage.merge(&event.data["message"]["usage"]);
            }
            Some("content_block_start") => {
                let block_index = event.data["index"].as_u64().unwrap_or(0) as u32;
                frames.extend(self.start_block(block_index, &event.data["content_block"]));
            }
            Some("content_block_delta") => {
                let block_index = event.data["index"].as_u64().unwrap_or(0) as u32;
                frames.extend(self.delta_block(block_index, &event.data["delta"]));
            }
            Some("content_block_stop") => {
                let block_index = event.data["index"].as_u64().unwrap_or(0) as u32;
                frames.extend(self.close_block(block_index));
            }
            Some("message_delta") => {
                self.usage.merge(&event.data["usage"]);
                self.stop_reason = event.data["delta"]["stop_reason"]
                    .as_str()
                    .unwrap_or("end_turn")
                    .to_string();
            }
            Some("message_stop") => {
                frames.extend(self.finish());
            }
            Some("error") => {
                frames.extend(
                    self.encode_error(
                        event.data["error"]["message"]
                            .as_str()
                            .unwrap_or("upstream stream error")
                            .to_string(),
                    ),
                );
            }
            _ => {}
        }
        frames
    }

    fn encode_native_event(&mut self, event: &SseEvent) -> Vec<Vec<u8>> {
        if self.done {
            return Vec::new();
        }
        self.native_mode = true;
        let event_name = event
            .data
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or(event.event.as_str());
        if event_name.is_empty() {
            return Vec::new();
        }

        let mut payload = event.data.clone();
        if let Some(sequence) = payload["sequence_number"].as_u64() {
            self.sequence = self.sequence.max(sequence.saturating_add(1));
        } else {
            payload["sequence_number"] = json!(self.sequence);
            self.sequence = self.sequence.saturating_add(1);
        }
        if let Some(response) = payload.get_mut("response") {
            normalize_native_response(response, &self.options);
        }
        if matches!(
            event_name,
            "response.completed" | "response.incomplete" | "response.failed"
        ) {
            self.done = true;
        }
        vec![responses_frame(event_name, &payload)]
    }

    fn ensure_started(&mut self) -> Vec<Vec<u8>> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        let mut response = self.response_object("in_progress", Vec::new());
        response["usage"] = Value::Null;
        vec![
            self.event(
                "response.created",
                json!({
                    "type": "response.created",
                    "response": response,
                }),
            ),
            self.event(
                "response.in_progress",
                json!({
                    "type": "response.in_progress",
                    "response": self.response_object("in_progress", Vec::new()),
                }),
            ),
        ]
    }

    fn start_block(&mut self, block_index: u32, block: &Value) -> Vec<Vec<u8>> {
        if self.blocks.contains_key(&block_index) {
            return Vec::new();
        }

        let output_index = self.allocate_output_index();
        match block["type"].as_str() {
            Some("text") => {
                let item_id = format!("msg_{}_{}", compact_uuid(), output_index);
                self.blocks.insert(
                    block_index,
                    ResponseBlock::Text {
                        output_index,
                        item_id: item_id.clone(),
                        text: block["text"].as_str().unwrap_or_default().to_string(),
                    },
                );
                vec![
                    self.event(
                        "response.output_item.added",
                        json!({
                            "type": "response.output_item.added",
                            "output_index": output_index,
                            "item": {
                                "id": item_id,
                                "type": "message",
                                "status": "in_progress",
                                "role": "assistant",
                                "content": [],
                            }
                        }),
                    ),
                    self.event(
                        "response.content_part.added",
                        json!({
                            "type": "response.content_part.added",
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": 0,
                            "part": {
                                "type": "output_text",
                                "text": "",
                                "annotations": [],
                                "logprobs": [],
                            }
                        }),
                    ),
                ]
            }
            Some("thinking") => {
                let item_id = format!("rs_{}_{}", compact_uuid(), output_index);
                self.blocks.insert(
                    block_index,
                    ResponseBlock::Reasoning {
                        output_index,
                        item_id: item_id.clone(),
                        text: block["thinking"].as_str().unwrap_or_default().to_string(),
                        signature: block["signature"].as_str().map(ToOwned::to_owned),
                    },
                );
                vec![
                    self.event(
                        "response.output_item.added",
                        json!({
                            "type": "response.output_item.added",
                            "output_index": output_index,
                            "item": {
                                "id": item_id,
                                "type": "reasoning",
                                "status": "in_progress",
                                "summary": [],
                            }
                        }),
                    ),
                    self.event(
                        "response.reasoning_summary_part.added",
                        json!({
                            "type": "response.reasoning_summary_part.added",
                            "item_id": item_id,
                            "output_index": output_index,
                            "summary_index": 0,
                            "part": {
                                "type": "summary_text",
                                "text": "",
                            }
                        }),
                    ),
                ]
            }
            Some("tool_use") | Some("server_tool_use") => {
                let call_id = block["id"]
                    .as_str()
                    .filter(|id| !id.is_empty())
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| format!("call_{}", compact_uuid()));
                let item_id = format!("fc_{call_id}");
                let initial_arguments = match block.get("input") {
                    Some(Value::Null) | None => String::new(),
                    Some(Value::Object(object)) if object.is_empty() => String::new(),
                    Some(input) => {
                        serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string())
                    }
                };
                let name = block["name"].as_str().unwrap_or_default().to_string();
                self.blocks.insert(
                    block_index,
                    ResponseBlock::Tool {
                        output_index,
                        item_id: item_id.clone(),
                        call_id: call_id.clone(),
                        name: name.clone(),
                        initial_arguments,
                        argument_deltas: String::new(),
                    },
                );
                vec![self.event(
                    "response.output_item.added",
                    json!({
                        "type": "response.output_item.added",
                        "output_index": output_index,
                        "item": {
                            "id": item_id,
                            "type": "function_call",
                            "status": "in_progress",
                            "arguments": "",
                            "call_id": call_id,
                            "name": name,
                        }
                    }),
                )]
            }
            _ => Vec::new(),
        }
    }

    fn delta_block(&mut self, block_index: u32, delta: &Value) -> Vec<Vec<u8>> {
        if !self.blocks.contains_key(&block_index) {
            let synthetic = match delta["type"].as_str() {
                Some("text_delta") => json!({"type": "text", "text": ""}),
                Some("thinking_delta") | Some("signature_delta") => {
                    json!({"type": "thinking", "thinking": ""})
                }
                Some("input_json_delta") => json!({
                    "type": "tool_use",
                    "id": format!("call_{}", compact_uuid()),
                    "name": "",
                    "input": {},
                }),
                _ => return Vec::new(),
            };
            let _ = self.start_block(block_index, &synthetic);
        }

        let (event_name, payload) = match self.blocks.get_mut(&block_index) {
            Some(ResponseBlock::Text {
                output_index,
                item_id,
                text,
            }) if delta["type"].as_str() == Some("text_delta") => {
                let fragment = delta["text"].as_str().unwrap_or_default();
                text.push_str(fragment);
                (
                    "response.output_text.delta",
                    json!({
                        "type": "response.output_text.delta",
                        "item_id": item_id,
                        "output_index": output_index,
                        "content_index": 0,
                        "delta": fragment,
                        "logprobs": [],
                    }),
                )
            }
            Some(ResponseBlock::Reasoning {
                output_index,
                item_id,
                text,
                ..
            }) if delta["type"].as_str() == Some("thinking_delta") => {
                let fragment = delta["thinking"].as_str().unwrap_or_default();
                text.push_str(fragment);
                (
                    "response.reasoning_summary_text.delta",
                    json!({
                        "type": "response.reasoning_summary_text.delta",
                        "item_id": item_id,
                        "output_index": output_index,
                        "summary_index": 0,
                        "delta": fragment,
                    }),
                )
            }
            Some(ResponseBlock::Reasoning { signature, .. })
                if delta["type"].as_str() == Some("signature_delta") =>
            {
                *signature = delta["signature"].as_str().map(ToOwned::to_owned);
                return Vec::new();
            }
            Some(ResponseBlock::Tool {
                output_index,
                item_id,
                argument_deltas,
                ..
            }) if delta["type"].as_str() == Some("input_json_delta") => {
                let fragment = delta["partial_json"].as_str().unwrap_or_default();
                argument_deltas.push_str(fragment);
                (
                    "response.function_call_arguments.delta",
                    json!({
                        "type": "response.function_call_arguments.delta",
                        "item_id": item_id,
                        "output_index": output_index,
                        "delta": fragment,
                    }),
                )
            }
            _ => return Vec::new(),
        };
        vec![self.event(event_name, payload)]
    }

    fn close_block(&mut self, block_index: u32) -> Vec<Vec<u8>> {
        let Some(block) = self.blocks.remove(&block_index) else {
            return Vec::new();
        };
        let output_index = block.output_index();

        match block {
            ResponseBlock::Text { item_id, text, .. } => {
                let item = json!({
                    "id": item_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{
                        "type": "output_text",
                        "text": text,
                        "annotations": [],
                        "logprobs": [],
                    }],
                });
                self.output.insert(output_index, item.clone());
                vec![
                    self.event(
                        "response.output_text.done",
                        json!({
                            "type": "response.output_text.done",
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": 0,
                            "text": text,
                            "logprobs": [],
                        }),
                    ),
                    self.event(
                        "response.content_part.done",
                        json!({
                            "type": "response.content_part.done",
                            "item_id": item_id,
                            "output_index": output_index,
                            "content_index": 0,
                            "part": item["content"][0],
                        }),
                    ),
                    self.event(
                        "response.output_item.done",
                        json!({
                            "type": "response.output_item.done",
                            "output_index": output_index,
                            "item": item,
                        }),
                    ),
                ]
            }
            ResponseBlock::Reasoning {
                item_id,
                text,
                signature,
                ..
            } => {
                let mut item = json!({
                    "id": item_id,
                    "type": "reasoning",
                    "status": "completed",
                    "summary": [{
                        "type": "summary_text",
                        "text": text,
                    }],
                });
                if let Some(signature) = signature {
                    item["encrypted_content"] = Value::String(signature);
                }
                self.output.insert(output_index, item.clone());
                vec![
                    self.event(
                        "response.reasoning_summary_text.done",
                        json!({
                            "type": "response.reasoning_summary_text.done",
                            "item_id": item_id,
                            "output_index": output_index,
                            "summary_index": 0,
                            "text": text,
                        }),
                    ),
                    self.event(
                        "response.reasoning_summary_part.done",
                        json!({
                            "type": "response.reasoning_summary_part.done",
                            "item_id": item_id,
                            "output_index": output_index,
                            "summary_index": 0,
                            "part": {
                                "type": "summary_text",
                                "text": text,
                            }
                        }),
                    ),
                    self.event(
                        "response.output_item.done",
                        json!({
                            "type": "response.output_item.done",
                            "output_index": output_index,
                            "item": item,
                        }),
                    ),
                ]
            }
            ResponseBlock::Tool {
                item_id,
                call_id,
                name,
                initial_arguments,
                argument_deltas,
                ..
            } => {
                let arguments = if argument_deltas.is_empty() {
                    if initial_arguments.is_empty() {
                        "{}".to_string()
                    } else {
                        initial_arguments
                    }
                } else {
                    argument_deltas
                };
                let item = json!({
                    "id": item_id,
                    "type": "function_call",
                    "status": "completed",
                    "arguments": arguments,
                    "call_id": call_id,
                    "name": name,
                });
                self.output.insert(output_index, item.clone());
                vec![
                    self.event(
                        "response.function_call_arguments.done",
                        json!({
                            "type": "response.function_call_arguments.done",
                            "item_id": item_id,
                            "output_index": output_index,
                            "arguments": arguments,
                        }),
                    ),
                    self.event(
                        "response.output_item.done",
                        json!({
                            "type": "response.output_item.done",
                            "output_index": output_index,
                            "item": item,
                        }),
                    ),
                ]
            }
        }
    }

    fn allocate_output_index(&mut self) -> u32 {
        let index = self.next_output_index;
        self.next_output_index = self.next_output_index.saturating_add(1);
        index
    }

    fn event(&mut self, event: &str, mut payload: Value) -> Vec<u8> {
        payload["sequence_number"] = json!(self.sequence);
        self.sequence = self.sequence.saturating_add(1);
        responses_frame(event, &payload)
    }

    fn response_object(&self, status: &str, output: Vec<Value>) -> Value {
        let mut response = json!({
            "id": self.id,
            "object": "response",
            "created_at": self.created_at,
            "status": status,
            "background": false,
            "error": null,
            "incomplete_details": null,
            "instructions": null,
            "max_output_tokens": null,
            "model": self.options.model,
            "output": output,
            "parallel_tool_calls": true,
            "previous_response_id": null,
            "reasoning": null,
            "store": false,
            "temperature": null,
            "text": {"format": {"type": "text"}},
            "tool_choice": "auto",
            "tools": [],
            "top_p": null,
            "truncation": "disabled",
            "usage": self.usage.responses_json(),
            "metadata": {},
        });
        apply_response_fields(&mut response, &self.options.response_fields);
        response
    }

    fn encode_error(&mut self, message: String) -> Vec<Vec<u8>> {
        if self.done {
            return Vec::new();
        }
        self.done = true;
        vec![self.event(
            "error",
            json!({
                "type": "error",
                "code": "upstream_stream_error",
                "message": message,
                "param": null,
            }),
        )]
    }

    fn finish(&mut self) -> Vec<Vec<u8>> {
        if self.native_mode {
            return Vec::new();
        }
        if self.done {
            return Vec::new();
        }

        let mut frames = self.ensure_started();
        let open_blocks = self.blocks.keys().copied().collect::<Vec<_>>();
        for block_index in open_blocks {
            frames.extend(self.close_block(block_index));
        }

        let (event_name, status, incomplete_details) = match self.stop_reason.as_str() {
            "max_tokens" => (
                "response.incomplete",
                "incomplete",
                json!({"reason": "max_output_tokens"}),
            ),
            "error" => ("response.failed", "failed", Value::Null),
            _ => ("response.completed", "completed", Value::Null),
        };
        let output = self.output.values().cloned().collect::<Vec<_>>();
        let mut response = self.response_object(status, output);
        response["incomplete_details"] = incomplete_details;
        frames.push(self.event(
            event_name,
            json!({
                "type": event_name,
                "response": response,
            }),
        ));
        self.done = true;
        frames
    }
}

fn normalize_native_response(response: &mut Value, options: &ResponsesOptions) {
    response["model"] = Value::String(options.model.clone());
    apply_response_fields(response, &options.response_fields);
}

fn native_responses_response(
    events: &mut Vec<ProviderEvent>,
    options: &ResponsesOptions,
) -> Option<Value> {
    let index = events.iter().rposition(|event| {
        matches!(
            event.native_event(),
            Some(NativeProviderEvent::OpenAiResponses(native))
                if matches!(
                    native.data["type"].as_str(),
                    Some("response.completed" | "response.incomplete" | "response.failed")
                ) && native.data.get("response").is_some()
        )
    })?;
    let reconstructed_output = events[index]
        .native_event()
        .and_then(|native| match native {
            NativeProviderEvent::OpenAiResponses(event) => event.data.get("response"),
        })
        .filter(|response| response_output_missing_or_empty(response))
        .and_then(|_| reconstructed_responses_output(events));
    let (_, native) = events.remove(index).into_parts();
    let Some(NativeProviderEvent::OpenAiResponses(mut event)) = native else {
        return None;
    };
    let mut response = event.data.get_mut("response").map(Value::take)?;
    if let Some(output) = reconstructed_output {
        response["output"] = Value::Array(output);
    }
    normalize_native_response(&mut response, options);
    Some(response)
}

fn responses_response(options: &ResponsesOptions, message: &Value) -> Value {
    let output = responses_output_items(message);
    let reason = stop_reason(message);
    let (status, incomplete_details) = match reason {
        "max_tokens" => ("incomplete", json!({"reason": "max_output_tokens"})),
        "error" => ("failed", Value::Null),
        _ => ("completed", Value::Null),
    };
    let mut response = json!({
        "id": response_id(message),
        "object": "response",
        "created_at": unix_timestamp(),
        "status": status,
        "background": false,
        "error": null,
        "incomplete_details": incomplete_details,
        "instructions": null,
        "max_output_tokens": null,
        "model": options.model,
        "output": output,
        "parallel_tool_calls": true,
        "previous_response_id": null,
        "reasoning": null,
        "store": false,
        "temperature": null,
        "text": {"format": {"type": "text"}},
        "tool_choice": "auto",
        "tools": [],
        "top_p": null,
        "truncation": "disabled",
        "usage": usage_from_message(message).responses_json(),
        "metadata": {},
    });
    apply_response_fields(&mut response, &options.response_fields);
    response
}

fn response_id(message: &Value) -> String {
    message["id"]
        .as_str()
        .filter(|id| id.starts_with("resp_"))
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("resp_{}", compact_uuid()))
}

fn responses_output_items(message: &Value) -> Vec<Value> {
    let mut output = Vec::new();
    let mut pending_text = String::new();

    let flush_text = |output: &mut Vec<Value>, pending_text: &mut String| {
        if pending_text.is_empty() {
            return;
        }
        output.push(json!({
            "id": format!("msg_{}", compact_uuid()),
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{
                "type": "output_text",
                "text": std::mem::take(pending_text),
                "annotations": [],
                "logprobs": [],
            }],
        }));
    };

    for block in message["content"].as_array().into_iter().flatten() {
        match block["type"].as_str() {
            Some("text") => pending_text.push_str(block["text"].as_str().unwrap_or_default()),
            Some("thinking") => {
                flush_text(&mut output, &mut pending_text);
                let mut reasoning = json!({
                    "id": format!("rs_{}", compact_uuid()),
                    "type": "reasoning",
                    "status": "completed",
                    "summary": [{
                        "type": "summary_text",
                        "text": block["thinking"].as_str().unwrap_or_default(),
                    }],
                });
                if let Some(signature) = block["signature"].as_str() {
                    reasoning["encrypted_content"] = Value::String(signature.to_string());
                }
                output.push(reasoning);
            }
            Some("tool_use") | Some("server_tool_use") => {
                flush_text(&mut output, &mut pending_text);
                let call_id = block["id"].as_str().unwrap_or_default();
                output.push(json!({
                    "id": format!("fc_{call_id}"),
                    "type": "function_call",
                    "status": "completed",
                    "arguments": serde_json::to_string(&block["input"])
                        .unwrap_or_else(|_| "{}".to_string()),
                    "call_id": call_id,
                    "name": block["name"].as_str().unwrap_or_default(),
                }));
            }
            _ => {}
        }
    }
    flush_text(&mut output, &mut pending_text);
    output
}

fn apply_response_fields(response: &mut Value, fields: &Map<String, Value>) {
    let Some(object) = response.as_object_mut() else {
        return;
    };
    for (key, value) in fields {
        object.insert(key.clone(), value.clone());
    }
}

#[cfg(test)]
mod tests {
    use claude_proxy_core::SseEvent;
    use serde_json::json;

    use super::*;

    fn text_events() -> Vec<SseEvent> {
        vec![
            SseEvent {
                event: "message_start".to_string(),
                data: json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_test",
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": "upstream-model",
                        "stop_reason": null,
                        "usage": {"input_tokens": 4, "output_tokens": 0}
                    }
                }),
            },
            SseEvent {
                event: "content_block_start".to_string(),
                data: json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {"type": "text", "text": ""}
                }),
            },
            SseEvent {
                event: "content_block_delta".to_string(),
                data: json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": "hello"}
                }),
            },
            SseEvent {
                event: "content_block_stop".to_string(),
                data: json!({"type": "content_block_stop", "index": 0}),
            },
            SseEvent {
                event: "message_delta".to_string(),
                data: json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn"},
                    "usage": {"output_tokens": 2}
                }),
            },
            SseEvent {
                event: "message_stop".to_string(),
                data: json!({"type": "message_stop"}),
            },
        ]
    }

    fn native_text_delta_events_with_empty_terminal_output() -> Vec<ProviderEvent> {
        let mut normalized = text_events();
        let terminal_normalized = normalized.split_off(3);
        vec![
            ProviderEvent::openai_responses(
                SseEvent {
                    event: "response.output_text.delta".to_string(),
                    data: json!({
                        "type": "response.output_text.delta",
                        "output_index": 0,
                        "content_index": 0,
                        "delta": "hello"
                    }),
                },
                normalized,
            ),
            ProviderEvent::openai_responses(
                SseEvent {
                    event: "response.completed".to_string(),
                    data: json!({
                        "type": "response.completed",
                        "response": {
                            "id": "resp_empty_terminal",
                            "object": "response",
                            "model": "upstream-model",
                            "status": "completed",
                            "output": [],
                            "usage": {
                                "input_tokens": 4,
                                "output_tokens": 2,
                                "total_tokens": 6
                            }
                        }
                    }),
                },
                terminal_normalized,
            ),
        ]
    }

    #[test]
    fn chat_stream_uses_requested_model_and_done_marker() {
        let mut encoder =
            DownstreamProtocol::chat_completions("client-model", true).stream_encoder();
        let frames = text_events()
            .iter()
            .flat_map(|event| encoder.encode_event(&event.clone().into()))
            .collect::<Vec<_>>();
        let body = String::from_utf8(frames.concat()).unwrap();

        assert!(body.contains("\"model\":\"client-model\""));
        assert!(body.contains("\"content\":\"hello\""));
        assert!(body.contains("\"prompt_tokens\":4"));
        assert!(body.ends_with("data: [DONE]\n\n"));
    }

    #[test]
    fn responses_stream_emits_stateful_lifecycle() {
        let mut encoder =
            DownstreamProtocol::responses("client-model", Map::new()).stream_encoder();
        let frames = text_events()
            .iter()
            .flat_map(|event| encoder.encode_event(&event.clone().into()))
            .collect::<Vec<_>>();
        let body = String::from_utf8(frames.concat()).unwrap();

        assert!(body.contains("event: response.created"));
        assert!(body.contains("event: response.output_text.delta"));
        assert!(body.contains("event: response.output_item.done"));
        assert!(body.contains("event: response.completed"));
        assert!(body.contains("\"output\":[{\"content\""));
        assert!(!body.contains("[DONE]"));
    }

    #[test]
    fn responses_stream_preserves_native_web_search_events() {
        let mut encoder =
            DownstreamProtocol::responses("client-model", Map::new()).stream_encoder();
        let event = ProviderEvent::openai_responses(
            SseEvent {
                event: "response.output_item.added".to_string(),
                data: json!({
                    "type": "response.output_item.added",
                    "output_index": 2,
                    "sequence_number": 17,
                    "item": {
                        "id": "ws_native",
                        "type": "web_search_call",
                        "status": "in_progress",
                        "action": {"type": "search", "query": "rust"}
                    }
                }),
            },
            vec![SseEvent {
                event: "content_block_start".to_string(),
                data: json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "server_tool_use",
                        "id": "ws_native",
                        "name": "web_search",
                        "input": {"query": "rust"}
                    }
                }),
            }],
        );

        let reasoning = ProviderEvent::openai_responses(
            SseEvent {
                event: "response.reasoning_text.delta".to_string(),
                data: json!({
                    "type": "response.reasoning_text.delta",
                    "item_id": "rs_native",
                    "output_index": 3,
                    "content_index": 0,
                    "delta": "reasoning"
                }),
            },
            Vec::new(),
        );
        let mut frames = encoder.encode_event(&event);
        frames.extend(encoder.encode_event(&reasoning));
        let body = String::from_utf8(frames.concat()).unwrap();
        assert!(body.contains("event: response.output_item.added"));
        assert!(body.contains("\"type\":\"web_search_call\""));
        assert!(body.contains("\"sequence_number\":17"));
        assert!(body.contains("event: response.reasoning_text.delta"));
        assert!(body.contains("\"sequence_number\":18"));
        assert!(!body.contains("\"type\":\"function_call\""));
    }

    #[test]
    fn chat_stream_reads_parallel_native_tool_events_without_anthropic_serialization() {
        let mut encoder =
            DownstreamProtocol::chat_completions("client-model", true).stream_encoder();
        let provider_events = [
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {"type": "function_call", "call_id": "call_a", "name": "first"}
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": {"type": "function_call", "call_id": "call_b", "name": "second"}
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 1,
                "delta": "{\"b\":2}"
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "delta": "{\"a\":1}"
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 2,
                "item": {"type": "function_call", "id": "fc_c"}
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 2,
                "item": {
                    "type": "function_call",
                    "call_id": "call_c",
                    "name": "third",
                    "arguments": "{\"c\":3}"
                }
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "usage": {
                        "input_tokens": 10,
                        "input_tokens_details": {"cached_tokens": 3},
                        "output_tokens": 5,
                        "output_tokens_details": {"reasoning_tokens": 2}
                    }
                }
            }),
        ]
        .map(|data| {
            ProviderEvent::openai_responses(
                SseEvent {
                    event: data["type"].as_str().unwrap().to_string(),
                    data,
                },
                Vec::new(),
            )
        });
        let mut frames = Vec::new();
        for event in &provider_events {
            frames.extend(encoder.encode_event(event));
        }
        frames.extend(encoder.finish());
        let body = String::from_utf8(frames.concat()).unwrap();
        let chunks = body
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .filter(|line| *line != "[DONE]")
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let tool_calls = chunks
            .iter()
            .flat_map(|chunk| {
                chunk["choices"][0]["delta"]["tool_calls"]
                    .as_array()
                    .into_iter()
                    .flatten()
            })
            .collect::<Vec<_>>();

        assert!(tool_calls.iter().any(|call| {
            call["index"] == 0 && call["id"] == "call_a" && call["function"]["name"] == "first"
        }));
        assert!(tool_calls.iter().any(|call| {
            call["index"] == 1 && call["id"] == "call_b" && call["function"]["name"] == "second"
        }));
        assert!(
            tool_calls
                .iter()
                .any(|call| { call["index"] == 1 && call["function"]["arguments"] == "{\"b\":2}" })
        );
        assert!(
            tool_calls
                .iter()
                .any(|call| { call["index"] == 0 && call["function"]["arguments"] == "{\"a\":1}" })
        );
        assert!(tool_calls.iter().any(|call| {
            call["index"] == 2 && call["id"] == "call_c" && call["function"]["name"] == "third"
        }));
        assert!(
            tool_calls
                .iter()
                .any(|call| { call["index"] == 2 && call["function"]["arguments"] == "{\"c\":3}" })
        );
        assert!(body.contains("\"finish_reason\":\"tool_calls\""));
        assert!(body.contains("\"prompt_tokens\":10"));
        assert!(body.contains("\"reasoning_tokens\":2"));
    }

    #[test]
    fn native_non_stream_response_preserves_items_and_applies_client_fields() {
        let output = json!([
            {
                "id": "rs_native",
                "type": "reasoning",
                "status": "completed",
                "summary": [],
                "encrypted_content": "signed"
            },
            {
                "id": "ws_native",
                "type": "web_search_call",
                "status": "completed",
                "action": {
                    "type": "search",
                    "query": "rust",
                    "sources": [{"title": "Rust", "url": "https://rust-lang.org"}]
                }
            },
            {
                "id": "fc_a",
                "type": "function_call",
                "status": "completed",
                "call_id": "call_a",
                "name": "first",
                "arguments": "{}"
            },
            {
                "id": "fc_b",
                "type": "function_call",
                "status": "completed",
                "call_id": "call_b",
                "name": "second",
                "arguments": "{}"
            }
        ]);
        let event = ProviderEvent::openai_responses(
            SseEvent {
                event: "response.completed".to_string(),
                data: json!({
                    "type": "response.completed",
                    "sequence_number": 23,
                    "response": {
                        "id": "resp_native",
                        "object": "response",
                        "model": "upstream-model",
                        "status": "completed",
                        "parallel_tool_calls": true,
                        "output": output,
                        "usage": {"input_tokens": 4, "output_tokens": 8, "total_tokens": 12}
                    }
                }),
            },
            Vec::new(),
        );
        let options = ResponsesOptions {
            model: "client-model".to_string(),
            response_fields: Map::from_iter([(
                "parallel_tool_calls".to_string(),
                Value::Bool(false),
            )]),
        };

        let response = native_responses_response(&mut vec![event], &options).unwrap();
        assert_eq!(response["id"], "resp_native");
        assert_eq!(response["model"], "client-model");
        assert_eq!(response["parallel_tool_calls"], false);
        assert_eq!(response["output"], output);
    }

    #[test]
    fn native_non_stream_response_reconstructs_empty_terminal_output_from_deltas() {
        let mut events = native_text_delta_events_with_empty_terminal_output();
        let options = ResponsesOptions {
            model: "client-model".to_string(),
            response_fields: Map::new(),
        };

        let response = native_responses_response(&mut events, &options).unwrap();

        assert_eq!(response["id"], "resp_empty_terminal");
        assert_eq!(response["model"], "client-model");
        assert_eq!(response["status"], "completed");
        assert_eq!(response["usage"]["input_tokens"], 4);
        assert_eq!(response["output"][0]["type"], "message");
        assert_eq!(response["output"][0]["content"][0]["text"], "hello");
    }

    #[test]
    fn native_non_stream_chat_preserves_parallel_tool_calls_and_reasoning() {
        let event = ProviderEvent::openai_responses(
            SseEvent {
                event: "response.completed".to_string(),
                data: json!({
                    "type": "response.completed",
                    "response": {
                        "status": "completed",
                        "output": [
                            {
                                "type": "reasoning",
                                "summary": [{"type": "summary_text", "text": "summary"}]
                            },
                            {
                                "type": "function_call",
                                "call_id": "call_a",
                                "name": "first",
                                "arguments": "{\"a\":1}"
                            },
                            {
                                "type": "function_call",
                                "call_id": "call_b",
                                "name": "second",
                                "arguments": "{\"b\":2}"
                            }
                        ],
                        "usage": {
                            "input_tokens": 10,
                            "input_tokens_details": {"cached_tokens": 3},
                            "output_tokens": 5,
                            "output_tokens_details": {"reasoning_tokens": 2}
                        }
                    }
                }),
            },
            Vec::new(),
        );
        let response = native_chat_completion_response(
            &[event],
            &ChatCompletionsOptions {
                model: "client-model".to_string(),
                include_usage: false,
            },
        )
        .unwrap();

        assert_eq!(response["choices"][0]["finish_reason"], "tool_calls");
        assert_eq!(
            response["choices"][0]["message"]["reasoning_content"],
            "summary"
        );
        assert_eq!(
            response["choices"][0]["message"]["tool_calls"][0]["id"],
            "call_a"
        );
        assert_eq!(
            response["choices"][0]["message"]["tool_calls"][1]["id"],
            "call_b"
        );
        assert_eq!(response["usage"]["prompt_tokens"], 10);
        assert_eq!(
            response["usage"]["completion_tokens_details"]["reasoning_tokens"],
            2
        );
    }

    #[test]
    fn native_non_stream_chat_reconstructs_empty_terminal_output_from_deltas() {
        let events = native_text_delta_events_with_empty_terminal_output();

        let response = native_chat_completion_response(
            &events,
            &ChatCompletionsOptions {
                model: "client-model".to_string(),
                include_usage: false,
            },
        )
        .unwrap();

        assert_eq!(response["model"], "client-model");
        assert_eq!(response["choices"][0]["message"]["content"], "hello");
        assert_eq!(response["choices"][0]["finish_reason"], "stop");
        assert_eq!(response["usage"]["prompt_tokens"], 4);
        assert_eq!(response["usage"]["completion_tokens"], 2);
    }

    #[test]
    fn non_stream_chat_maps_tools_and_usage() {
        let mut events = text_events();
        events.insert(
            4,
            SseEvent {
                event: "content_block_start".to_string(),
                data: json!({
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": {
                        "type": "tool_use",
                        "id": "call_1",
                        "name": "lookup",
                        "input": {"q": "rust"}
                    }
                }),
            },
        );
        events.insert(
            5,
            SseEvent {
                event: "content_block_stop".to_string(),
                data: json!({"type": "content_block_stop", "index": 1}),
            },
        );

        let message = crate::non_stream::response_from_events(&events).unwrap();
        let response = chat_completion_response(
            &ChatCompletionsOptions {
                model: "client-model".to_string(),
                include_usage: false,
            },
            &message,
        );

        assert_eq!(response["model"], "client-model");
        assert_eq!(
            response["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
            "lookup"
        );
        assert_eq!(response["usage"]["prompt_tokens"], 4);
        assert_eq!(response["usage"]["completion_tokens"], 2);
    }

    #[test]
    fn openai_error_shape_has_no_anthropic_wrapper() {
        let response = protocol_error_response(
            &DownstreamProtocol::chat_completions("model", false),
            StatusCode::UNAUTHORIZED,
            &ErrorResponse::authentication("bad key"),
        );
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }
}
