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

use crate::provider::{Provider, ProviderError};

pub struct AnthropicProvider {
    id: String,
    client: Client,
    base_url: String,
}

impl AnthropicProvider {
    pub fn new(
        id: &str,
        api_key: &str,
        base_url: &str,
        proxy: &str,
        connect_timeout: u64,
        read_timeout: u64,
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

        let client = builder
            .build()
            .map_err(|e| ProviderError::Network(format!("failed to build HTTP client: {e}")))?;

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

        // Serialize request as-is (passthrough)
        let body = serde_json::to_value(&request)
            .map_err(|e| ProviderError::Network(format!("failed to serialize request: {e}")))?;

        debug!("Anthropic request to {url}");

        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout
                } else {
                    ProviderError::Network(e.to_string())
                }
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body_text = response.text().await.unwrap_or_default();
            return Err(match status {
                401 => ProviderError::Authentication(body_text),
                429 => ProviderError::RateLimited,
                404 => ProviderError::ModelNotFound(body_text),
                _ => ProviderError::UpstreamError {
                    status,
                    body: body_text,
                },
            });
        }

        if request.stream {
            let stream = response.bytes_stream().map(|chunk| {
                chunk
                    .map(|bytes| parse_anthropic_sse(&bytes))
                    .map_err(|e| ProviderError::Network(e.to_string()))
            });
            Ok(Box::pin(stream))
        } else {
            let body = response
                .text()
                .await
                .map_err(|e| ProviderError::Network(e.to_string()))?;
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
            ModelInfo {
                model_id: "claude-opus-4-20250514".to_string(),
                supports_thinking: Some(true),
            },
            ModelInfo {
                model_id: "claude-sonnet-4-20250514".to_string(),
                supports_thinking: Some(true),
            },
            ModelInfo {
                model_id: "claude-3-5-haiku-20241022".to_string(),
                supports_thinking: Some(false),
            },
        ])
    }
}

/// Parse raw bytes from an Anthropic SSE stream.
fn parse_anthropic_sse(bytes: &[u8]) -> SseEvent {
    let text = String::from_utf8_lossy(bytes);
    let mut event_type = String::new();
    let mut data = Value::Null;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("event: ") {
            event_type = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("data: ")
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
