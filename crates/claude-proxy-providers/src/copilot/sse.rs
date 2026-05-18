use claude_proxy_core::SseEvent;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::http::fmt_reqwest_err;
use crate::provider::ProviderError;
use crate::sse::{SseDecoder, parse_sse_frame};

pub(super) fn stream_anthropic_sse_response(
    response: reqwest::Response,
) -> BoxStream<'static, Result<SseEvent, ProviderError>> {
    let (tx, rx) = mpsc::channel::<Result<SseEvent, ProviderError>>(64);

    tokio::spawn(async move {
        let mut decoder = SseDecoder::new();
        let mut byte_stream = response.bytes_stream();

        while let Some(chunk_result) = byte_stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    decoder.push(&chunk);
                    while let Some(event_text) = decoder.next_frame() {
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

        if let Some(event_text) = decoder.finish()
            && let Some(event) = parse_anthropic_sse_text(&event_text)
        {
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
    let frame = parse_sse_frame(text)?;
    let data = serde_json::from_str::<Value>(frame.data.trim()).unwrap_or(Value::Null);

    Some(SseEvent {
        event: frame.event.unwrap_or_default(),
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
