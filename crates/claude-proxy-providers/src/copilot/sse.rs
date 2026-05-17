use claude_proxy_core::SseEvent;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::http::fmt_reqwest_err;
use crate::provider::ProviderError;

pub(super) fn stream_anthropic_sse_response(
    response: reqwest::Response,
) -> BoxStream<'static, Result<SseEvent, ProviderError>> {
    let (tx, rx) = mpsc::channel::<Result<SseEvent, ProviderError>>(64);

    tokio::spawn(async move {
        let mut buffer = String::new();
        let mut byte_stream = response.bytes_stream();

        while let Some(chunk_result) = byte_stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    buffer.push_str(&String::from_utf8_lossy(&chunk));
                    while let Some(pos) = buffer.find("\n\n") {
                        let event_text = buffer[..pos].to_string();
                        buffer = buffer[pos + 2..].to_string();
                        if let Some(event) = parse_anthropic_sse_text(&event_text)
                            && tx.send(Ok(event)).await.is_err()
                        {
                            return;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx
                        .send(Err(ProviderError::Network(fmt_reqwest_err(&e))))
                        .await;
                    return;
                }
            }
        }

        if let Some(event) = parse_anthropic_sse_text(&buffer) {
            let _ = tx.send(Ok(event)).await;
        }
    });

    Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
}

#[cfg(test)]
fn parse_anthropic_sse(bytes: &[u8]) -> SseEvent {
    let text = String::from_utf8_lossy(bytes);
    parse_anthropic_sse_text(&text).unwrap_or_else(|| SseEvent {
        event: String::new(),
        data: Value::Null,
    })
}

fn parse_anthropic_sse_text(text: &str) -> Option<SseEvent> {
    if text.trim().is_empty() {
        return None;
    }

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

    Some(SseEvent {
        event: event_type,
        data,
    })
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

    #[test]
    fn parse_anthropic_sse_text_ignores_incomplete_empty_frame() {
        assert!(parse_anthropic_sse_text("").is_none());
        assert!(parse_anthropic_sse_text("\n").is_none());
    }

    #[test]
    fn parse_anthropic_sse_text_reads_single_frame_from_buffered_stream() {
        let event = parse_anthropic_sse_text(
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\"}",
        )
        .expect("event");

        assert_eq!(event.event, "content_block_delta");
        assert_eq!(event.data["type"], "content_block_delta");
    }
}
