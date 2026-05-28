//! Anthropic Messages provider adapter.
//!
//! Mostly passthrough — rewrites auth token and base URL.

use std::time::Duration;

use async_trait::async_trait;
use claude_proxy_core::*;
use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::Client;
use serde_json::Value;
use tracing::debug;

use crate::http::{
    apply_extra_ca_certs, fmt_reqwest_err, map_upstream_response, next_upstream_stream_item,
    read_upstream_response_text, send_upstream_request,
};
use crate::provider::{Provider, ProviderError};
use crate::sse::{SseDecoder, parse_sse_frame};

pub struct AnthropicProvider {
    id: String,
    client: Client,
    base_url: String,
}

impl AnthropicProvider {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        api_key: &str,
        base_url: &str,
        proxy: &str,
        connect_timeout: u64,
        read_timeout: u64,
        extra_ca_certs: &[String],
    ) -> Result<Self, ProviderError> {
        let mut builder = Client::builder()
            .connect_timeout(Duration::from_secs(connect_timeout))
            .read_timeout(Duration::from_secs(read_timeout))
            .default_headers({
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert(
                    "x-api-key",
                    api_key.parse().map_err(|e| {
                        ProviderError::Network(format!("invalid api-key header: {e}"))
                    })?,
                );
                headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
                headers
            });

        if !proxy.is_empty() {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy)
                    .map_err(|e| ProviderError::Network(format!("invalid proxy: {e}")))?,
            );
        }

        builder = apply_extra_ca_certs(builder, extra_ca_certs)?;

        let client = builder.build().map_err(|e| {
            ProviderError::Network(format!(
                "failed to build HTTP client: {}",
                fmt_reqwest_err(&e)
            ))
        })?;

        Ok(Self {
            id: id.to_string(),
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn chat(
        &self,
        request: MessagesRequest,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let url = format!("{}/v1/messages", self.base_url);

        // Serialize request and inject cache_control for prompt caching
        let mut request = request;
        sanitize_anthropic_history(&mut request);
        let mut body = serde_json::to_value(&request)
            .map_err(|e| ProviderError::Network(format!("failed to serialize request: {e}")))?;
        inject_cache_control(&mut body);

        debug!("Anthropic request to {url}");

        let response = send_upstream_request(self.client.post(&url).json(&body)).await?;

        if !response.status().is_success() {
            return Err(map_upstream_response(response).await);
        }

        if request.stream {
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<SseEvent, ProviderError>>(64);
            tokio::spawn(async move {
                let mut decoder = SseDecoder::new();
                let mut byte_stream = response.bytes_stream();
                loop {
                    let chunk_result = match next_upstream_stream_item(byte_stream.next()).await {
                        Ok(Some(chunk_result)) => chunk_result,
                        Ok(None) => break,
                        Err(error) => {
                            let _ = tx.send(Err(error)).await;
                            return;
                        }
                    };

                    match chunk_result {
                        Ok(bytes) => {
                            decoder.push(&bytes);
                            while let Some(frame) = decoder.next_frame() {
                                let Some(event) = parse_anthropic_sse_frame(&frame) else {
                                    continue;
                                };
                                if tx.send(Ok(event)).await.is_err() {
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

                if let Some(frame) = decoder.finish()
                    && let Some(event) = parse_anthropic_sse_frame(&frame)
                {
                    let _ = tx.send(Ok(event)).await;
                }
            });
            Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
        } else {
            let body = read_upstream_response_text(response).await?;
            let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let event = SseEvent {
                event: "message".to_string(),
                data,
            };
            let stream = futures::stream::iter(vec![Ok(event)]);
            Ok(Box::pin(stream))
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        // Anthropic doesn't have a standard /models endpoint.
        // Return well-known Claude models.
        Ok(vec![
            anthropic_model("claude-opus-4-20250514", CapabilityState::Supported),
            anthropic_model("claude-sonnet-4-20250514", CapabilityState::Supported),
            anthropic_model("claude-3-5-haiku-20241022", CapabilityState::Unsupported),
        ])
    }
}

fn anthropic_model(model_id: &str, thinking: CapabilityState) -> ModelInfo {
    ModelInfo {
        model_id: model_id.to_string(),
        vendor: Some("anthropic".to_string()),
        is_chat_default: None,
        capabilities: ModelCapabilities {
            endpoints: EndpointCapabilities {
                anthropic_messages: CapabilityState::Supported,
                openai_chat_completions: CapabilityState::Unsupported,
                openai_responses: CapabilityState::Unsupported,
            },
            modalities: ModalityCapabilities {
                input: InputModalities {
                    image: CapabilityState::Supported,
                    ..Default::default()
                },
                ..Default::default()
            },
            features: FeatureCapabilities {
                streaming: CapabilityState::Supported,
                system_prompt: CapabilityState::Supported,
                tools: CapabilityState::Supported,
                tool_choice: CapabilityState::Supported,
                thinking,
                prompt_cache: CapabilityState::Supported,
                sampling: CapabilityState::Supported,
                stop_sequences: CapabilityState::Supported,
                ..Default::default()
            },
            quality: QualityGateCapabilities {
                tool_search: ToolSearchCapability::supported(
                    QualityGateHeaderKind::Anthropic1p,
                    QualityGateBetaLocation::Header,
                ),
                fine_grained_tool_streaming: CapabilityState::Unknown,
                prompt_cache: PromptCacheCapability::basic(),
                interleaved_thinking: thinking,
                token_counting: TokenCountingCapability::none(),
                ..Default::default()
            },
            supported_parameters: vec![
                "system".to_string(),
                "messages".to_string(),
                "max_tokens".to_string(),
                "stream".to_string(),
                "tools".to_string(),
                "tool_choice".to_string(),
                "thinking".to_string(),
                "temperature".to_string(),
                "top_p".to_string(),
                "top_k".to_string(),
                "stop_sequences".to_string(),
            ],
            ..Default::default()
        },
    }
}

fn sanitize_anthropic_history(request: &mut MessagesRequest) {
    let mut messages = Vec::with_capacity(request.messages.len());

    for mut message in request.messages.drain(..) {
        if sanitize_message(&mut message) {
            messages.push(message);
        }
    }

    request.messages = messages;
}

fn sanitize_message(message: &mut Message) -> bool {
    match &mut message.content {
        MessageContent::Text(text) => {
            trim_text(text);
            !text.is_empty()
        }
        MessageContent::Blocks(blocks) => {
            blocks.retain_mut(keep_content_block);
            !blocks.is_empty()
        }
    }
}

fn keep_content_block(block: &mut Content) -> bool {
    match block {
        Content::Text { text } => {
            trim_text(text);
            !text.is_empty()
        }
        Content::Thinking {
            thinking,
            signature,
        } => !thinking.trim().is_empty() && signature.as_ref().is_some_and(|s| !s.is_empty()),
        Content::ToolUse { .. }
        | Content::ToolResult { .. }
        | Content::ServerToolUse { .. }
        | Content::Unknown(_) => true,
    }
}

fn trim_text(text: &mut String) {
    if text.chars().last().is_some_and(char::is_whitespace) {
        text.truncate(text.trim_end().len());
    }
}

/// Parse a complete Anthropic SSE frame.
fn parse_anthropic_sse_frame(text: &str) -> Option<SseEvent> {
    let frame = parse_sse_frame(text)?;
    let data = serde_json::from_str::<Value>(frame.data.trim()).unwrap_or(Value::Null);

    Some(SseEvent {
        event: frame.event.unwrap_or_default(),
        data,
    })
}

/// Inject `cache_control: {"type": "ephemeral"}` into the request body to enable
/// Anthropic's prompt caching. Marks (up to 4 breakpoints, the API max):
///   1. Last system block
///   2. Last tool definition
///   3. Latest user message (most valuable during tool-use loops)
///
/// If the request already has cache_control annotations from the client, those
/// count toward the cap so we don't exceed 4 total.
fn inject_cache_control(body: &mut Value) {
    let cache_control = serde_json::json!({"type": "ephemeral"});

    // Count existing cache_control annotations to respect the 4-breakpoint cap.
    let existing = count_existing_cache_controls(body);
    let mut budget = 4u32.saturating_sub(existing);

    // 1. Inject on the last system prompt block.
    if budget > 0 && body.get("system").is_some() {
        let system = body.get_mut("system").unwrap();
        match system {
            Value::String(text) => {
                let block = serde_json::json!([{
                    "type": "text",
                    "text": text.clone(),
                    "cache_control": cache_control.clone()
                }]);
                *system = block;
                budget -= 1;
            }
            Value::Array(blocks) => {
                if let Some(last) = blocks.last_mut()
                    && let Value::Object(obj) = last
                {
                    obj.insert("cache_control".to_string(), cache_control.clone());
                    budget -= 1;
                }
            }
            _ => {}
        }
    }

    // 2. Inject on the last tool definition.
    if budget > 0
        && let Some(Value::Array(tools)) = body.get_mut("tools")
        && let Some(last_tool) = tools.last_mut()
        && let Value::Object(obj) = last_tool
    {
        obj.insert("cache_control".to_string(), cache_control.clone());
        budget -= 1;
    }

    // 3. Inject on the latest user message (most impactful during tool-use loops).
    if budget > 0
        && let Some(Value::Array(messages)) = body.get_mut("messages")
        && let Some(last_user) = messages
            .iter_mut()
            .rev()
            .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
    {
        match last_user.get_mut("content") {
            Some(Value::Array(blocks)) => {
                if let Some(last_block) = blocks.last_mut()
                    && let Value::Object(obj) = last_block
                {
                    obj.insert("cache_control".to_string(), cache_control.clone());
                }
            }
            Some(Value::String(text)) => {
                let block = serde_json::json!([{
                    "type": "text",
                    "text": text.clone(),
                    "cache_control": cache_control.clone()
                }]);
                *last_user.get_mut("content").unwrap() = block;
            }
            _ => {}
        }
    }
}

/// Count existing `cache_control` annotations in the request body.
fn count_existing_cache_controls(body: &Value) -> u32 {
    let mut count = 0u32;

    // Check system blocks
    if let Some(Value::Array(blocks)) = body.get("system") {
        for block in blocks {
            if block.get("cache_control").is_some() {
                count += 1;
            }
        }
    }

    // Check tool definitions
    if let Some(Value::Array(tools)) = body.get("tools") {
        for tool in tools {
            if tool.get("cache_control").is_some() {
                count += 1;
            }
        }
    }

    // Check message content blocks
    if let Some(Value::Array(messages)) = body.get("messages") {
        for msg in messages {
            if let Some(Value::Array(blocks)) = msg.get("content") {
                for block in blocks {
                    if block.get("cache_control").is_some() {
                        count += 1;
                    }
                }
            }
        }
    }

    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn base_request(messages: Vec<Message>) -> MessagesRequest {
        MessagesRequest {
            model: "claude-sonnet-4".to_string(),
            system: None,
            messages,
            max_tokens: Some(4096),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: HashMap::new(),
        }
    }

    async fn retry_then_success_server() -> (String, Arc<AtomicUsize>, tokio::task::JoinHandle<()>)
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let attempts = Arc::new(AtomicUsize::new(0));
        let server_attempts = attempts.clone();

        let handle = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 4096];
                let _ = socket.read(&mut request).await.unwrap();
                let attempt = server_attempts.fetch_add(1, Ordering::SeqCst);
                let (status, body) = if attempt == 0 {
                    (
                        "500 Internal Server Error",
                        r#"{"error":{"message":"temporary upstream failure"}}"#,
                    )
                } else {
                    (
                        "200 OK",
                        r#"{"id":"msg_test","type":"message","role":"assistant","content":[],"model":"claude-sonnet-4","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#,
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        (format!("http://{addr}"), attempts, handle)
    }

    #[test]
    fn anthropic_sse_decoder_waits_for_complete_frames() {
        let mut decoder = SseDecoder::new();
        decoder.push(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\"");

        assert!(decoder.next_frame().is_none());

        decoder.push(b"}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");

        let first = parse_anthropic_sse_frame(&decoder.next_frame().unwrap()).unwrap();
        assert_eq!(first.event, "content_block_delta");
        assert_eq!(first.data["type"], "content_block_delta");

        let second = parse_anthropic_sse_frame(&decoder.next_frame().unwrap()).unwrap();
        assert_eq!(second.event, "message_stop");
        assert_eq!(second.data["type"], "message_stop");
        assert!(decoder.next_frame().is_none());
    }

    #[test]
    fn sanitize_history_strips_unsigned_thinking_and_empty_messages() {
        let mut request = base_request(vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![
                    Content::Thinking {
                        thinking: "completed".to_string(),
                        signature: None,
                    },
                    Content::Text {
                        text: "answer  \n".to_string(),
                    },
                ]),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![Content::Thinking {
                    thinking: String::new(),
                    signature: Some("sig".to_string()),
                }]),
            },
        ]);

        sanitize_anthropic_history(&mut request);

        assert_eq!(request.messages.len(), 1);
        match &request.messages[0].content {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                assert!(matches!(&blocks[0], Content::Text { text } if text == "answer"));
            }
            MessageContent::Text(_) => panic!("expected content blocks"),
        }
    }

    #[test]
    fn sanitize_history_preserves_signed_thinking() {
        let mut request = base_request(vec![Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![Content::Thinking {
                thinking: "completed".to_string(),
                signature: Some("signature".to_string()),
            }]),
        }]);

        sanitize_anthropic_history(&mut request);

        match &request.messages[0].content {
            MessageContent::Blocks(blocks) => assert!(matches!(
                &blocks[0],
                Content::Thinking {
                    thinking,
                    signature: Some(signature),
                } if thinking == "completed" && signature == "signature"
            )),
            MessageContent::Text(_) => panic!("expected content blocks"),
        }
    }

    #[tokio::test]
    async fn anthropic_chat_retries_transient_upstream_status() {
        let (base_url, attempts, server) = retry_then_success_server().await;
        let provider =
            AnthropicProvider::new("anthropic", "test-key", &base_url, "", 5, 5, &[]).unwrap();
        let mut request = base_request(vec![Message {
            role: Role::User,
            content: MessageContent::Text("hello".to_string()),
        }]);
        request.stream = false;

        let mut stream = provider.chat(request).await.unwrap();
        let event = stream.next().await.unwrap().unwrap();

        assert_eq!(event.event, "message");
        assert_eq!(event.data["id"], "msg_test");
        assert!(stream.next().await.is_none());
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        server.await.unwrap();
    }
}
