use std::collections::BTreeMap;

use claude_proxy_core::SseEvent;
use serde_json::Value;

pub(crate) fn response_from_events(events: &[SseEvent]) -> Option<Value> {
    let mut message = None;
    let mut content = BTreeMap::new();
    let mut partial_input_json = BTreeMap::<u32, String>::new();

    for event in events {
        let data = &event.data;
        match data.get("type").and_then(Value::as_str) {
            Some("message") => return Some(data.clone()),
            Some("message_start") => {
                message = data.get("message").cloned();
                if let Some(items) = message
                    .as_ref()
                    .and_then(|value| value.get("content"))
                    .and_then(Value::as_array)
                {
                    content.extend(
                        items
                            .iter()
                            .cloned()
                            .enumerate()
                            .map(|(idx, item)| (idx as u32, item)),
                    );
                }
            }
            Some("content_block_start") => {
                if let (Some(index), Some(block)) = (event_index(data), data.get("content_block")) {
                    content.insert(index, block.clone());
                }
            }
            Some("content_block_delta") => {
                apply_content_delta(data, &mut content, &mut partial_input_json);
            }
            Some("content_block_stop") => {
                apply_content_block_stop(data, &mut content, &mut partial_input_json);
            }
            Some("message_delta") => {
                if let Some(message) = message.as_mut() {
                    apply_message_delta(data, message);
                }
            }
            _ => {}
        }
    }

    let mut message = message?;
    if let Some(object) = message.as_object_mut() {
        object.insert(
            "content".to_string(),
            Value::Array(content.into_values().collect()),
        );
    }
    Some(message)
}

fn event_index(data: &Value) -> Option<u32> {
    data.get("index")
        .and_then(Value::as_u64)
        .and_then(|index| u32::try_from(index).ok())
}

fn apply_content_delta(
    data: &Value,
    content: &mut BTreeMap<u32, Value>,
    partial_input_json: &mut BTreeMap<u32, String>,
) {
    let Some(index) = event_index(data) else {
        return;
    };
    let delta = &data["delta"];
    match delta.get("type").and_then(Value::as_str) {
        Some("text_delta") => {
            if let Some(text) = delta.get("text").and_then(Value::as_str)
                && let Some(block) = content.get_mut(&index)
            {
                append_string_field(block, "text", text);
            }
        }
        Some("thinking_delta") => {
            if let Some(thinking) = delta.get("thinking").and_then(Value::as_str)
                && let Some(block) = content.get_mut(&index)
            {
                append_string_field(block, "thinking", thinking);
            }
        }
        Some("signature_delta") => {
            if let Some(signature) = delta.get("signature").and_then(Value::as_str)
                && let Some(block) = content.get_mut(&index).and_then(Value::as_object_mut)
            {
                block.insert(
                    "signature".to_string(),
                    Value::String(signature.to_string()),
                );
            }
        }
        Some("input_json_delta") => {
            if let Some(partial_json) = delta.get("partial_json").and_then(Value::as_str) {
                partial_input_json
                    .entry(index)
                    .or_default()
                    .push_str(partial_json);
            }
        }
        _ => {}
    }
}

fn apply_content_block_stop(
    data: &Value,
    content: &mut BTreeMap<u32, Value>,
    partial_input_json: &mut BTreeMap<u32, String>,
) {
    let Some(index) = event_index(data) else {
        return;
    };
    let Some(arguments) = partial_input_json.remove(&index) else {
        return;
    };
    let Ok(input) = serde_json::from_str::<Value>(&arguments) else {
        return;
    };
    if let Some(block) = content.get_mut(&index).and_then(Value::as_object_mut) {
        block.insert("input".to_string(), input);
    }
}

fn append_string_field(block: &mut Value, field: &str, fragment: &str) {
    if let Some(object) = block.as_object_mut() {
        let mut value = object
            .get(field)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        value.push_str(fragment);
        object.insert(field.to_string(), Value::String(value));
    }
}

fn apply_message_delta(data: &Value, message: &mut Value) {
    let Some(object) = message.as_object_mut() else {
        return;
    };

    if let Some(delta) = data.get("delta") {
        if let Some(stop_reason) = delta.get("stop_reason") {
            object.insert("stop_reason".to_string(), stop_reason.clone());
        }
        if let Some(stop_sequence) = delta.get("stop_sequence") {
            object.insert("stop_sequence".to_string(), stop_sequence.clone());
        }
    }

    if let Some(usage) = data.get("usage") {
        if let (Some(existing), Some(update)) = (
            object.get_mut("usage").and_then(Value::as_object_mut),
            usage.as_object(),
        ) {
            for (key, value) in update {
                existing.insert(key.clone(), value.clone());
            }
        } else {
            object.insert("usage".to_string(), usage.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use claude_proxy_core::SseEvent;
    use serde_json::json;

    use super::*;

    #[test]
    fn reconstructs_message_from_sse_events() {
        let events = vec![
            SseEvent {
                event: "message_start".to_string(),
                data: json!({
                    "type": "message_start",
                    "message": {
                        "id": "msg_1",
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": "gpt-4.1",
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": {"input_tokens": 12, "output_tokens": 0}
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
                    "delta": {"type": "text_delta", "text": "Hello"}
                }),
            },
            SseEvent {
                event: "content_block_delta".to_string(),
                data: json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": " world"}
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
                    "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                    "usage": {"output_tokens": 5}
                }),
            },
            SseEvent {
                event: "message_stop".to_string(),
                data: json!({"type": "message_stop"}),
            },
        ];

        let response = response_from_events(&events).unwrap();
        assert_eq!(response["type"], "message");
        assert_eq!(response["content"][0]["type"], "text");
        assert_eq!(response["content"][0]["text"], "Hello world");
        assert_eq!(response["stop_reason"], "end_turn");
        assert_eq!(response["usage"]["input_tokens"], 12);
        assert_eq!(response["usage"]["output_tokens"], 5);
    }

    #[test]
    fn keeps_direct_message_event() {
        let events = vec![SseEvent {
            event: "message".to_string(),
            data: json!({
                "id": "msg_direct",
                "type": "message",
                "role": "assistant",
                "content": [{"type": "text", "text": "hi"}],
                "model": "claude",
                "stop_reason": "end_turn",
                "stop_sequence": null,
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }),
        }];

        let response = response_from_events(&events).unwrap();
        assert_eq!(response["id"], "msg_direct");
        assert_eq!(response["content"][0]["text"], "hi");
    }
}
