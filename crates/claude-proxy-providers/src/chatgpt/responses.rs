use crate::openai_compat::{CompactRequestKind, classify_compact_request_body};
use crate::provider::{ProviderError, ProviderEvent};
use claude_proxy_config::settings::ReasoningMarkerMode;
use claude_proxy_core::{MessagesRequest, ModelInfo};
use futures::stream::BoxStream;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy)]
pub(super) struct CodexRequestContext<'a> {
    pub installation_id: Option<&'a str>,
    pub session_id: Option<&'a str>,
    pub thread_id: Option<&'a str>,
    pub turn_id: Option<&'a str>,
    pub window_id: Option<&'a str>,
    pub service_tier: Option<&'a str>,
    pub standalone_tools: bool,
    pub responses_lite: bool,
    pub model: Option<&'a ModelInfo>,
    pub additional_instructions: Option<&'a str>,
    pub supports_reasoning_summary_parameter: bool,
    pub supports_parallel_tool_calls: bool,
}

impl Default for CodexRequestContext<'_> {
    fn default() -> Self {
        Self {
            installation_id: None,
            session_id: None,
            thread_id: None,
            turn_id: None,
            window_id: None,
            service_tier: None,
            standalone_tools: true,
            responses_lite: false,
            model: None,
            additional_instructions: None,
            supports_reasoning_summary_parameter: true,
            supports_parallel_tool_calls: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PromptCacheKeySource {
    Explicit,
    StableClientConversation,
    None,
}

impl PromptCacheKeySource {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Explicit => "explicit",
            Self::StableClientConversation => "stable_client_conversation",
            Self::None => "none",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptCacheKey {
    value: String,
    source: PromptCacheKeySource,
}

pub(super) fn stream_response_with_marker_mode_and_context<F>(
    response: reqwest::Response,
    marker_mode: ReasoningMarkerMode,
    correlation: crate::responses::ResponsesCorrelation,
    on_event: F,
) -> BoxStream<'static, Result<ProviderEvent, ProviderError>>
where
    F: Fn(&Value) + Send + Sync + 'static,
{
    crate::responses::stream_responses_response_with_context_and_observer(
        response,
        marker_mode,
        correlation,
        on_event,
    )
}

#[cfg(test)]
pub(super) fn build_body(
    request: &MessagesRequest,
    default_instructions: &str,
    installation_id: Option<&str>,
) -> Value {
    build_body_with_context(
        request,
        default_instructions,
        CodexRequestContext {
            installation_id,
            ..CodexRequestContext::default()
        },
    )
    .expect("test request should convert to a valid Responses body")
}

pub(super) fn build_body_with_context(
    request: &MessagesRequest,
    default_instructions: &str,
    context: CodexRequestContext<'_>,
) -> Result<Value, crate::provider::ProviderError> {
    let mut body = crate::responses::convert_to_responses_with_context(
        request,
        crate::responses::ConversionContext {
            provider_id: Some("chatgpt"),
            model: context.model,
            tool_conversion_mode: if context.standalone_tools {
                crate::responses::ToolConversionMode::CodexStandalone
            } else {
                crate::responses::ToolConversionMode::Function
            },
            supports_reasoning_summary_parameter: Some(
                context.supports_reasoning_summary_parameter,
            ),
        },
    )?;
    if let Some(object) = body.as_object_mut() {
        object.remove("stop");
        object.remove("max_output_tokens");
        object.insert("stream".to_string(), json!(true));
        apply_codex_defaults(object, context);
        apply_codex_request_options(object, request, context);
        apply_codex_reasoning_defaults(object, request, context);
        let missing_instructions = object
            .get("instructions")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty);
        if missing_instructions {
            object.insert("instructions".to_string(), json!(default_instructions));
        }
        if let Some(additional) = context
            .additional_instructions
            .map(str::trim)
            .filter(|instructions| !instructions.is_empty())
        {
            let existing = object
                .get("instructions")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let instructions = if existing.is_empty() {
                additional.to_string()
            } else {
                format!("{existing}\n\n{additional}")
            };
            object.insert("instructions".to_string(), json!(instructions));
        }
        apply_responses_lite_shape(object, context.responses_lite);
        apply_codex_metadata(object, request, context);
    }
    Ok(body)
}

fn apply_codex_defaults(body: &mut Map<String, Value>, context: CodexRequestContext<'_>) {
    body.entry("tools".to_string()).or_insert_with(|| json!([]));
    body.entry("include".to_string())
        .or_insert_with(|| json!([]));
    body.entry("tool_choice".to_string())
        .or_insert_with(|| json!("auto"));

    if context.responses_lite || !context.supports_parallel_tool_calls {
        body.insert("parallel_tool_calls".to_string(), json!(false));
    } else {
        let has_tools = body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        body.entry("parallel_tool_calls".to_string())
            .or_insert_with(|| json!(has_tools));
    }
}

fn apply_codex_request_options(
    body: &mut Map<String, Value>,
    request: &MessagesRequest,
    context: CodexRequestContext<'_>,
) {
    insert_trimmed_string(
        body,
        "service_tier",
        request
            .extra
            .get("service_tier")
            .and_then(Value::as_str)
            .or(context.service_tier),
    );

    insert_trimmed_string(
        body,
        "safety_identifier",
        request
            .extra
            .get("safety_identifier")
            .and_then(Value::as_str),
    );

    if let Some(value) = request.extra.get("prompt_cache_options")
        && value.is_object()
    {
        body.insert("prompt_cache_options".to_string(), value.clone());
    }

    if let Some(value) = request.extra.get("parallel_tool_calls")
        && value.is_boolean()
        && ((!context.responses_lite && context.supports_parallel_tool_calls)
            || value.as_bool() == Some(false))
    {
        body.insert("parallel_tool_calls".to_string(), value.clone());
    }

    if let Some(value) = request.extra.get("stream_options")
        && value.is_object()
    {
        body.insert("stream_options".to_string(), value.clone());
    }

    if let Some(verbosity) = codex_responses_verbosity(request) {
        body.insert("text".to_string(), json!({ "verbosity": verbosity }));
    }
}

fn apply_codex_reasoning_defaults(
    body: &mut Map<String, Value>,
    request: &MessagesRequest,
    context: CodexRequestContext<'_>,
) {
    let has_explicit_reasoning = request.extra.contains_key("reasoning");
    let has_explicit_reasoning_effort = request.extra.contains_key("reasoning_effort");
    body.entry("reasoning".to_string())
        .or_insert_with(|| json!({}));
    let Some(reasoning) = body.get_mut("reasoning") else {
        return;
    };
    if !reasoning.is_object() {
        *reasoning = json!({});
    }
    let Some(reasoning) = reasoning.as_object_mut() else {
        return;
    };
    if context.responses_lite {
        reasoning.insert("context".to_string(), json!("all_turns"));
    }
    if !has_explicit_reasoning
        && !has_explicit_reasoning_effort
        && reasoning.get("summary").and_then(Value::as_str) == Some("detailed")
    {
        reasoning.insert("summary".to_string(), json!("auto"));
    }
    let include = body
        .entry("include".to_string())
        .or_insert_with(|| json!([]));
    if !include.is_array() {
        *include = json!([]);
    }
    if let Some(include) = include.as_array_mut()
        && !include
            .iter()
            .any(|value| value.as_str() == Some("reasoning.encrypted_content"))
    {
        include.push(json!("reasoning.encrypted_content"));
    }
}

fn apply_codex_metadata(
    body: &mut Map<String, Value>,
    request: &MessagesRequest,
    context: CodexRequestContext<'_>,
) {
    if let Some(prompt_cache_key) = resolve_prompt_cache_key(request) {
        body.insert(
            "prompt_cache_key".to_string(),
            json!(prompt_cache_key.value),
        );
    }

    let mut client_metadata: Map<String, Value> = request
        .extra
        .get("client_metadata")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut turn_metadata = client_metadata
        .get("x-codex-turn-metadata")
        .and_then(Value::as_str)
        .and_then(|value| serde_json::from_str::<Value>(value).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default();

    if let Some(installation_id) = context
        .installation_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        client_metadata.insert(
            "x-codex-installation-id".to_string(),
            json!(installation_id),
        );
        turn_metadata.insert("installation_id".to_string(), json!(installation_id));
    }

    for (flat_key, canonical_key, value) in [
        ("session_id", "session_id", context.session_id),
        ("thread_id", "thread_id", context.thread_id),
        ("turn_id", "turn_id", context.turn_id),
        ("x-codex-window-id", "window_id", context.window_id),
    ] {
        if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
            client_metadata.insert(flat_key.to_string(), json!(value));
            turn_metadata.insert(canonical_key.to_string(), json!(value));
        }
    }

    if !client_metadata.is_empty() {
        turn_metadata.insert(
            "request_kind".to_string(),
            json!(codex_request_kind(classify_compact_request_body(
                &Value::Object(body.clone(),)
            ))),
        );
        client_metadata.insert(
            "x-codex-turn-metadata".to_string(),
            Value::String(Value::Object(turn_metadata).to_string()),
        );
    }

    if !client_metadata.is_empty() {
        body.insert(
            "client_metadata".to_string(),
            Value::Object(client_metadata),
        );
    }
}

fn codex_request_kind(kind: CompactRequestKind) -> &'static str {
    match kind {
        CompactRequestKind::None => "turn",
        _ => "compaction",
    }
}

fn apply_responses_lite_shape(body: &mut Map<String, Value>, responses_lite: bool) {
    if !responses_lite {
        return;
    }

    let tools = body
        .remove("tools")
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    let (direct_tools, mut hosted_tools): (Vec<_>, Vec<_>) = tools.into_iter().partition(|tool| {
        matches!(
            tool.get("type").and_then(Value::as_str),
            Some("function" | "custom")
        )
    });
    if !direct_tools.is_empty() {
        hosted_tools.push(json!({
            "type": "namespace",
            "name": "functions",
            "description": "",
            "tools": direct_tools,
        }));
    }

    let mut prefix = vec![json!({
        "type": "additional_tools",
        "role": "developer",
        "tools": hosted_tools,
    })];
    if let Some(instructions) = body
        .remove("instructions")
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .filter(|value| !value.is_empty())
    {
        prefix.push(json!({
            "type": "message",
            "role": "developer",
            "content": [{"type": "input_text", "text": instructions}],
        }));
    }
    let input = body
        .entry("input".to_string())
        .or_insert_with(|| Value::Array(Vec::new()));
    if let Some(items) = input.as_array_mut() {
        prefix.append(items);
        *items = prefix;
    }
}

pub(super) fn codex_turn_metadata(body: &Value) -> Option<&str> {
    body.pointer("/client_metadata/x-codex-turn-metadata")
        .and_then(Value::as_str)
}

pub(super) fn codex_client_metadata_value<'a>(body: &'a Value, key: &str) -> Option<&'a str> {
    body.get("client_metadata")?.get(key)?.as_str()
}

pub(super) fn codex_routing_hint(body: &Value) -> Option<String> {
    let model = body.get("model")?.as_str()?.trim();
    if model.is_empty() {
        return None;
    }
    let tier = body
        .get("service_tier")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    Some(match tier {
        Some(tier) => format!("model={model};tier={tier}"),
        None => format!("model={model}"),
    })
}

pub(super) fn set_codex_request_kind(body: &mut Value, request_kind: &str) {
    let Some(metadata) = body
        .pointer_mut("/client_metadata/x-codex-turn-metadata")
        .and_then(|value| value.as_str())
        .and_then(|value| serde_json::from_str::<Value>(value).ok())
    else {
        return;
    };
    let mut metadata = metadata;
    if let Some(object) = metadata.as_object_mut() {
        object.insert("request_kind".to_string(), json!(request_kind));
    }
    if let Some(slot) = body.pointer_mut("/client_metadata/x-codex-turn-metadata") {
        *slot = Value::String(metadata.to_string());
    }
}

pub(super) fn prompt_cache_key_source(request: &MessagesRequest) -> PromptCacheKeySource {
    resolve_prompt_cache_key(request)
        .map(|key| key.source)
        .unwrap_or(PromptCacheKeySource::None)
}

pub(super) fn stable_client_conversation_id_for_continuation(
    request: &MessagesRequest,
) -> Option<String> {
    stable_client_conversation_id(request).map(ToOwned::to_owned)
}

fn resolve_prompt_cache_key(request: &MessagesRequest) -> Option<PromptCacheKey> {
    trimmed_string(request.extra.get("prompt_cache_key"))
        .or_else(|| metadata_string(request, "prompt_cache_key"))
        .map(|value| PromptCacheKey {
            value: clamp_prompt_cache_key(value),
            source: PromptCacheKeySource::Explicit,
        })
        .or_else(|| {
            stable_prompt_cache_scope_id(request).map(|value| PromptCacheKey {
                value: clamp_prompt_cache_key(value),
                source: PromptCacheKeySource::StableClientConversation,
            })
        })
}

fn stable_prompt_cache_scope_id(request: &MessagesRequest) -> Option<&str> {
    [
        "session_id",
        "client_session_id",
        "x-client-session-id",
        "conversation_id",
        "client_conversation_id",
        "x-client-conversation-id",
        "thread_id",
        "client_thread_id",
        "x-client-thread-id",
    ]
    .into_iter()
    .find_map(|key| {
        metadata_string(request, key).or_else(|| trimmed_string(request.extra.get(key)))
    })
}

fn stable_client_conversation_id(request: &MessagesRequest) -> Option<&str> {
    [
        "conversation_id",
        "thread_id",
        "session_id",
        "client_conversation_id",
        "client_thread_id",
        "client_session_id",
        "x-client-conversation-id",
        "x-client-thread-id",
        "x-client-session-id",
    ]
    .into_iter()
    .find_map(|key| {
        metadata_string(request, key).or_else(|| trimmed_string(request.extra.get(key)))
    })
}

fn metadata_string<'a>(request: &'a MessagesRequest, key: &str) -> Option<&'a str> {
    request
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get(key))
        .and_then(|value| trimmed_string(Some(value)))
}

fn trimmed_string(value: Option<&Value>) -> Option<&str> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn insert_trimmed_string(object: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        object.insert(key.to_string(), json!(value));
    }
}

fn codex_responses_verbosity(request: &MessagesRequest) -> Option<&str> {
    if !request.model.starts_with("gpt-5") {
        return None;
    }

    request
        .extra
        .get("verbosity")
        .and_then(Value::as_str)
        .or_else(|| {
            request
                .extra
                .get("text")
                .and_then(|value| value.get("verbosity"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|value| matches!(*value, "low" | "medium" | "high"))
}

fn clamp_prompt_cache_key(value: &str) -> String {
    value.chars().take(64).collect()
}
