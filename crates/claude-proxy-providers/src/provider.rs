use async_trait::async_trait;
use claude_proxy_core::{MessagesRequest, ModelInfo, SseEvent};
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
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
    pub balance: Option<i64>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitSource {
    #[default]
    UsageEndpoint,
    ResponseHeaders,
}

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

    /// List available models from this provider.
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;

    /// Return provider account quota/rate-limit snapshots when available.
    async fn rate_limit_snapshots(&self) -> Result<Vec<RateLimitSnapshot>, ProviderError> {
        Ok(Vec::new())
    }
}
