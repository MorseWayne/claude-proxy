use claude_proxy_core::{MessagesRequest, SseEvent};
use futures::stream::BoxStream;
use serde_json::{Value, json};

use crate::provider::ProviderError;

pub(super) fn stream_response(
    response: reqwest::Response,
) -> BoxStream<'static, Result<SseEvent, ProviderError>> {
    crate::responses::stream_responses_response(response)
}

pub(super) fn build_body(request: &MessagesRequest, default_instructions: &str) -> Value {
    let mut body = crate::responses::convert_to_responses(request);
    if let Some(object) = body.as_object_mut() {
        object.remove("max_output_tokens");
        object.insert("stream".to_string(), json!(true));
        let missing_instructions = object
            .get("instructions")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty);
        if missing_instructions {
            object.insert("instructions".to_string(), json!(default_instructions));
        }
    }
    body
}
