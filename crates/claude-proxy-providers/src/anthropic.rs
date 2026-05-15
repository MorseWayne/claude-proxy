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

use crate::http::{apply_extra_ca_certs, fmt_reqwest_err};
use crate::provider::{Provider, ProviderError};

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
        let mut body = serde_json::to_value(&request)
            .map_err(|e| ProviderError::Network(format!("failed to serialize request: {e}")))?;
        inject_cache_control(&mut body);

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
                    ProviderError::Network(fmt_reqwest_err(&e))
                }
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let body_text = response.text().await.unwrap_or_default();
            return Err(match status {
                401 => ProviderError::Authentication(body_text),
                429 => ProviderError::RateLimited { retry_after },
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
                    .map_err(|e| ProviderError::Network(fmt_reqwest_err(&e)))
            });
            Ok(Box::pin(stream))
        } else {
            let body = response
                .text()
                .await
                .map_err(|e| ProviderError::Network(fmt_reqwest_err(&e)))?;
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
                vendor: Some("anthropic".to_string()),
                max_output_tokens: None,
                supported_endpoints: vec!["/v1/messages".to_string()],
                is_chat_default: None,
                supports_vision: Some(true),
                supports_adaptive_thinking: None,
                min_thinking_budget: None,
                max_thinking_budget: None,
                reasoning_effort_levels: Vec::new(),
            },
            ModelInfo {
                model_id: "claude-sonnet-4-20250514".to_string(),
                supports_thinking: Some(true),
                vendor: Some("anthropic".to_string()),
                max_output_tokens: None,
                supported_endpoints: vec!["/v1/messages".to_string()],
                is_chat_default: None,
                supports_vision: Some(true),
                supports_adaptive_thinking: None,
                min_thinking_budget: None,
                max_thinking_budget: None,
                reasoning_effort_levels: Vec::new(),
            },
            ModelInfo {
                model_id: "claude-3-5-haiku-20241022".to_string(),
                supports_thinking: Some(false),
                vendor: Some("anthropic".to_string()),
                max_output_tokens: None,
                supported_endpoints: vec!["/v1/messages".to_string()],
                is_chat_default: None,
                supports_vision: Some(true),
                supports_adaptive_thinking: None,
                min_thinking_budget: None,
                max_thinking_budget: None,
                reasoning_effort_levels: Vec::new(),
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
