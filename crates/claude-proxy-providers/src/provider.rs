use async_trait::async_trait;
use claude_proxy_core::{MessagesRequest, ModelInfo, SseEvent};
use futures::stream::BoxStream;
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
}
