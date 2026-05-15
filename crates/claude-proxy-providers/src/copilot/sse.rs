use claude_proxy_core::SseEvent;
use serde_json::Value;

pub(super) fn parse_anthropic_sse(bytes: &[u8]) -> SseEvent {
    let text = String::from_utf8_lossy(bytes);
    let mut event_type = String::new();
    let mut data = Value::Null;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("event: ") {
            event_type = rest.trim().to_string();
        } else if let Some(rest) = line
            .strip_prefix("data: ")
            .or_else(|| line.strip_prefix("data:"))
            && let Ok(parsed) = serde_json::from_str::<Value>(rest.trim())
        {
            data = parsed;
        }
    }

    SseEvent {
        event: event_type,
        data,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_anthropic_sse_accepts_data_without_space() {
        let event = parse_anthropic_sse(
            br#"event: message_delta
data:{"type":"message_delta"}"#,
        );

        assert_eq!(event.event, "message_delta");
        assert_eq!(event.data["type"], "message_delta");
    }
}
