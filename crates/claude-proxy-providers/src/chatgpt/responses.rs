use claude_proxy_config::settings::ReasoningMarkerMode;
use claude_proxy_core::{MessagesRequest, SseEvent};
use futures::stream::BoxStream;
use serde_json::{Map, Value, json};

use crate::provider::ProviderError;

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

pub(super) fn build_body(
    request: &MessagesRequest,
    default_instructions: &str,
    installation_id: Option<&str>,
) -> Value {
    let mut body = crate::responses::convert_to_responses(request);
    if let Some(object) = body.as_object_mut() {
        object.remove("stop");
        object.insert("stream".to_string(), json!(true));
        let missing_instructions = object
            .get("instructions")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty);
        if missing_instructions {
            object.insert("instructions".to_string(), json!(default_instructions));
        }
        apply_codex_metadata(object, request, installation_id);
    }
    body
}

fn apply_codex_metadata(
    body: &mut Map<String, Value>,
    request: &MessagesRequest,
    installation_id: Option<&str>,
) {
    let metadata = request.metadata.as_ref();
    if let Some(prompt_cache_key) = metadata
        .and_then(|metadata| metadata.get("prompt_cache_key"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        body.entry("prompt_cache_key".to_string())
            .or_insert_with(|| json!(prompt_cache_key));
    }

    let mut client_metadata = metadata
        .and_then(|metadata| metadata.get("client_metadata"))
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    if let Some(installation_id) = installation_id
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
