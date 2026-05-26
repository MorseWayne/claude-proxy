use crate::provider::ProviderError;
use claude_proxy_config::settings::ReasoningMarkerMode;
use claude_proxy_core::{MessagesRequest, SseEvent};
use futures::stream::BoxStream;
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct CodexRequestContext<'a> {
    pub installation_id: Option<&'a str>,
    pub service_tier: Option<&'a str>,
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

pub(super) fn stream_response_with_marker_mode<F>(
    response: reqwest::Response,
    marker_mode: ReasoningMarkerMode,
    on_event: F,
) -> BoxStream<'static, Result<SseEvent, ProviderError>>
where
    F: Fn(&Value) + Send + Sync + 'static,
{
    crate::responses::stream_responses_response_with_marker_mode_and_observer(
        response,
        marker_mode,
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
}

pub(super) fn build_body_with_context(
    request: &MessagesRequest,
    default_instructions: &str,
    context: CodexRequestContext<'_>,
) -> Value {
    let mut body = crate::responses::convert_to_responses(request);
    if let Some(object) = body.as_object_mut() {
        object.remove("stop");
        object.remove("max_output_tokens");
        object.insert("stream".to_string(), json!(true));
        apply_codex_defaults(object);
        apply_codex_request_options(object, request, context);
        apply_codex_reasoning_defaults(object, request);
        let missing_instructions = object
            .get("instructions")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty);
        if missing_instructions {
            object.insert("instructions".to_string(), json!(default_instructions));
        }
        apply_codex_metadata(object, request, context);
    }
    body
}

fn apply_codex_defaults(body: &mut Map<String, Value>) {
    body.entry("tools".to_string()).or_insert_with(|| json!([]));
    body.entry("include".to_string())
        .or_insert_with(|| json!([]));
    body.entry("tool_choice".to_string())
        .or_insert_with(|| json!("auto"));

    let has_tools = body
        .get("tools")
        .and_then(Value::as_array)
        .is_some_and(|tools| !tools.is_empty());
    body.entry("parallel_tool_calls".to_string())
        .or_insert_with(|| json!(has_tools));
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

    if let Some(value) = request.extra.get("parallel_tool_calls")
        && value.is_boolean()
    {
        body.insert("parallel_tool_calls".to_string(), value.clone());
    }

    if let Some(verbosity) = codex_responses_verbosity(request) {
        body.insert("text".to_string(), json!({ "verbosity": verbosity }));
    }
}

fn apply_codex_reasoning_defaults(body: &mut Map<String, Value>, request: &MessagesRequest) {
    if request.extra.contains_key("reasoning") {
        return;
    }
    let Some(reasoning) = body.get_mut("reasoning").and_then(Value::as_object_mut) else {
        return;
    };
    if reasoning.get("summary").and_then(Value::as_str) == Some("detailed") {
        reasoning.insert("summary".to_string(), json!("auto"));
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

    let mut client_metadata = Map::new();

    if let Some(installation_id) = context
        .installation_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        client_metadata.insert(
            "x-codex-installation-id".to_string(),
            json!(installation_id),
        );
    }

    if !client_metadata.is_empty() {
        body.insert(
            "client_metadata".to_string(),
            Value::Object(client_metadata),
        );
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
            stable_client_conversation_id(request).map(|value| PromptCacheKey {
                value: clamp_prompt_cache_key(value),
                source: PromptCacheKeySource::StableClientConversation,
            })
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
