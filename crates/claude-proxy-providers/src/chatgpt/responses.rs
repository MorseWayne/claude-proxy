use claude_proxy_config::settings::ReasoningMarkerMode;
use claude_proxy_core::{MessagesRequest, SseEvent};
use futures::stream::BoxStream;
use serde_json::{Map, Value, json};

use crate::provider::ProviderError;

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct CodexRequestContext<'a> {
    pub installation_id: Option<&'a str>,
    pub prompt_cache_key: Option<&'a str>,
    pub window_id: Option<&'a str>,
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
        object.insert("stream".to_string(), json!(true));
        apply_codex_defaults(object);
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

fn apply_codex_metadata(
    body: &mut Map<String, Value>,
    request: &MessagesRequest,
    context: CodexRequestContext<'_>,
) {
    let metadata = request.metadata.as_ref();
    let prompt_cache_key = metadata
        .and_then(|metadata| metadata.get("prompt_cache_key"))
        .and_then(Value::as_str)
        .or(context.prompt_cache_key)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if let Some(prompt_cache_key) = prompt_cache_key {
        body.entry("prompt_cache_key".to_string())
            .or_insert_with(|| json!(prompt_cache_key));
    }

    let mut client_metadata = metadata
        .and_then(|metadata| metadata.get("client_metadata"))
        .and_then(Value::as_object)
        .cloned()
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
    }

    if let Some(window_id) = context
        .window_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        client_metadata
            .entry("x-codex-window-id".to_string())
            .or_insert_with(|| json!(window_id));
    }

    if !client_metadata.is_empty() {
        body.insert(
            "client_metadata".to_string(),
            Value::Object(client_metadata),
        );
    }
}
