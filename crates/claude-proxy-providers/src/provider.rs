use async_trait::async_trait;
use claude_proxy_core::{MessagesRequest, ModelInfo, SseEvent};
use futures::stream::BoxStream;
use serde::{Deserialize, Deserializer, Serialize};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("authentication failed: {0}")]
    Authentication(String),

    #[error("service unavailable: {0}")]
    ServiceUnavailable(String),

    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error("rate limited")]
    RateLimited { retry_after: Option<u64> },

    #[error("invalid request: {0}")]
    InvalidRequest(String),

    #[error("request too large: {0}")]
    RequestTooLarge(String),

    #[error("upstream overloaded: {message}")]
    Overloaded {
        message: String,
        retry_after: Option<u64>,
    },

    #[error("upstream error (HTTP {status}): {body}")]
    UpstreamError { status: u16, body: String },

    #[error("request timed out")]
    Timeout,

    #[error("network error: {0}")]
    Network(String),

    #[error("{source}")]
    WithUpstreamMetadata {
        #[source]
        source: Box<ProviderError>,
        metadata: Box<UpstreamErrorMetadata>,
    },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamErrorMetadata {
    pub status: u16,
    #[serde(default)]
    pub retry_after: Option<u64>,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub body_preview: Option<String>,
    #[serde(default)]
    pub headers: Vec<UpstreamErrorHeader>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpstreamErrorHeader {
    pub name: String,
    pub value: String,
}

impl ProviderError {
    pub fn with_upstream_metadata(self, metadata: UpstreamErrorMetadata) -> Self {
        match self {
            ProviderError::WithUpstreamMetadata { source, .. } => {
                ProviderError::WithUpstreamMetadata {
                    source,
                    metadata: Box::new(metadata),
                }
            }
            source => ProviderError::WithUpstreamMetadata {
                source: Box::new(source),
                metadata: Box::new(metadata),
            },
        }
    }

    pub fn upstream_metadata(&self) -> Option<&UpstreamErrorMetadata> {
        match self {
            ProviderError::WithUpstreamMetadata { metadata, .. } => Some(metadata),
            _ => None,
        }
    }

    pub fn without_upstream_metadata(&self) -> &ProviderError {
        match self {
            ProviderError::WithUpstreamMetadata { source, .. } => {
                source.without_upstream_metadata()
            }
            _ => self,
        }
    }

    pub fn is_authentication(&self) -> bool {
        matches!(
            self.without_upstream_metadata(),
            ProviderError::Authentication(_)
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitSnapshot {
    pub provider_id: String,
    pub feature: Option<String>,
    pub limit_name: Option<String>,
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub credits: Option<RateLimitCredits>,
    pub plan_type: Option<String>,
    pub rate_limit_reached_type: Option<String>,
    pub source: RateLimitSource,
    pub updated_at_unix_secs: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitWindow {
    pub used_percent: f64,
    pub window_minutes: Option<u64>,
    pub reset_at_unix_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RateLimitCredits {
    pub has_credits: Option<bool>,
    pub unlimited: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_optional_string")]
    pub balance: Option<String>,
}

fn deserialize_optional_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value.and_then(|value| match value {
        serde_json::Value::String(value) => {
            Some(value.trim().to_string()).filter(|value| !value.is_empty())
        }
        serde_json::Value::Number(value) => Some(value.to_string()),
        serde_json::Value::Bool(value) => Some(value.to_string()),
        _ => None,
    }))
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitSource {
    #[default]
    UsageEndpoint,
    ResponseHeaders,
    StreamEvent,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUsageMetadata {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderStreamMetadata {
    #[serde(default)]
    pub usage: Option<ProviderUsageMetadata>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub request_id: Option<String>,
    #[serde(default)]
    pub stop_reason: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderRequestMetadata {
    #[serde(default)]
    pub transport: Option<String>,
    #[serde(default)]
    pub responses_lite: Option<bool>,
    #[serde(default)]
    pub websocket_reused: Option<bool>,
    #[serde(default)]
    pub continuation_used: Option<bool>,
    #[serde(default)]
    pub continuation_disabled_reason: Option<String>,
    #[serde(default)]
    pub continuation_fallback_used: Option<bool>,
    #[serde(default)]
    pub fallback_reason: Option<String>,
    #[serde(default)]
    pub request_body_bytes: Option<u64>,
    #[serde(default)]
    pub upstream_send_body_bytes: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProviderRequestObserverEvent {
    pub event: ProviderRequestObserverEventKind,
    #[serde(default)]
    pub prompt_too_long_retries: u64,
    #[serde(default)]
    pub original_body_bytes: u64,
    #[serde(default)]
    pub shrunk_body_bytes: u64,
    #[serde(default)]
    pub dropped_items: u64,
    #[serde(default)]
    pub stream_metadata: Option<ProviderStreamMetadata>,
    #[serde(default)]
    pub request_metadata: Option<ProviderRequestMetadata>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderRequestObserverEventKind {
    #[default]
    PromptTooLongRetry,
    PromptTooLongRetryExhausted,
    PromptTooLongRetryUnshrinkable,
    StreamMetadata,
    RequestMetadata,
}

pub type ProviderRequestObserver = Arc<dyn Fn(ProviderRequestObserverEvent) + Send + Sync>;

/// Trait implemented by upstream provider adapters.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Provider identifier (e.g., "openai", "anthropic").
    fn id(&self) -> &str;

    /// Send a chat request and return a stream of SSE events.
    async fn chat(
        &self,
        request: MessagesRequest,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError>;

    async fn chat_with_observer(
        &self,
        request: MessagesRequest,
        _observer: Option<ProviderRequestObserver>,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        self.chat(request).await
    }

    /// List available models from this provider.
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;

    /// Return provider account quota/rate-limit snapshots when available.
    async fn rate_limit_snapshots(&self) -> Result<Vec<RateLimitSnapshot>, ProviderError> {
        Ok(Vec::new())
    }
}
