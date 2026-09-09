use async_trait::async_trait;
use claude_proxy_core::{MessagesRequest, ModelInfo, SseEvent};
use futures::stream::BoxStream;
use reqwest::header::HeaderMap;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::io::{self, Write};
use std::mem::size_of;
use std::sync::Arc;
use thiserror::Error;

/// A provider response event with a protocol-neutral normalized view and an
/// optional lossless source-protocol view.
///
/// Normalized events are Anthropic Messages events today because they are the
/// common denominator consumed by all existing downstream adapters. Native
/// events preserve richer upstream semantics so a matching downstream can
/// encode them without a lossy round trip through that common denominator.
#[derive(Debug, Clone)]
pub struct ProviderEvent {
    normalized: Vec<SseEvent>,
    native: Option<NativeProviderEvent>,
}

#[derive(Debug, Clone)]
pub enum NativeProviderEvent {
    OpenAiResponses(SseEvent),
}

impl ProviderEvent {
    pub fn normalized(events: Vec<SseEvent>) -> Self {
        Self {
            normalized: events,
            native: None,
        }
    }

    pub fn openai_responses(native: SseEvent, normalized: Vec<SseEvent>) -> Self {
        Self {
            normalized,
            native: Some(NativeProviderEvent::OpenAiResponses(native)),
        }
    }

    pub fn normalized_events(&self) -> &[SseEvent] {
        &self.normalized
    }

    pub fn into_normalized_events(self) -> Vec<SseEvent> {
        self.normalized
    }

    pub fn native_event(&self) -> Option<&NativeProviderEvent> {
        self.native.as_ref()
    }

    pub fn into_parts(self) -> (Vec<SseEvent>, Option<NativeProviderEvent>) {
        (self.normalized, self.native)
    }

    /// Estimate the logical bytes retained by this owned event without
    /// allocating a second serialized payload.
    pub fn retained_size_bytes(&self) -> Option<u64> {
        let mut size = u64::try_from(size_of::<Self>()).ok()?;
        for event in &self.normalized {
            size = size.checked_add(sse_event_retained_size(event)?)?;
        }
        if let Some(NativeProviderEvent::OpenAiResponses(event)) = &self.native {
            size = size.checked_add(sse_event_retained_size(event)?)?;
        }
        Some(size)
    }
}

fn sse_event_retained_size(event: &SseEvent) -> Option<u64> {
    let fixed = u64::try_from(size_of::<SseEvent>()).ok()?;
    let event_name = u64::try_from(event.event.len()).ok()?;
    let mut writer = CountingWriter::default();
    serde_json::to_writer(&mut writer, &event.data).ok()?;
    fixed.checked_add(event_name)?.checked_add(writer.bytes)
}

#[derive(Default)]
struct CountingWriter {
    bytes: u64,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let length = u64::try_from(buffer.len()).map_err(|_| io::Error::other("size overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(length)
            .ok_or_else(|| io::Error::other("size overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl From<SseEvent> for ProviderEvent {
    fn from(event: SseEvent) -> Self {
        Self::normalized(vec![event])
    }
}

#[cfg(test)]
mod provider_event_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn retained_size_counts_normalized_and_native_payloads() {
        let normalized = SseEvent {
            event: "message_delta".to_string(),
            data: json!({"delta": {"text": "hello"}}),
        };
        let normalized_only = ProviderEvent::normalized(vec![normalized.clone()]);
        let with_native = ProviderEvent::openai_responses(
            SseEvent {
                event: "response.output_text.delta".to_string(),
                data: json!({"delta": "hello"}),
            },
            vec![normalized],
        );

        assert!(normalized_only.retained_size_bytes().unwrap() > 0);
        assert!(
            with_native.retained_size_bytes().unwrap()
                > normalized_only.retained_size_bytes().unwrap()
        );
    }
}

#[derive(Debug, Clone, Error)]
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

    #[error("upstream response too large: {0}")]
    ResponseTooLarge(String),

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
    #[serde(default)]
    pub spend_control_reached: Option<bool>,
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
    #[serde(default)]
    pub reasoning_output_tokens: u64,
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
    pub prompt_cache_key_present: Option<bool>,
    #[serde(default)]
    pub prompt_cache_key_source: Option<String>,
    #[serde(default)]
    pub stable_client_conversation_id_present: Option<bool>,
    #[serde(default)]
    pub synthetic_stable_client_conversation_id: Option<bool>,
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
    #[serde(default)]
    pub virtual_context_1m: Option<bool>,
    #[serde(default)]
    pub context_estimated_tokens: Option<u64>,
    #[serde(default)]
    pub context_safe_input_limit: Option<u64>,
    #[serde(default)]
    pub context_model_window: Option<u64>,
    #[serde(default)]
    pub context_estimator_source: Option<String>,
    #[serde(default)]
    pub context_compact_kind: Option<String>,
    #[serde(default)]
    pub context_compressible_history: Option<bool>,
    #[serde(default)]
    pub context_local_blocked: Option<bool>,
    #[serde(default)]
    pub context_upstream_overflow: Option<bool>,
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

/// A validated, stateless OpenAI Responses request whose item ordering and
/// provider-native fields must be preserved.
#[derive(Debug, Clone)]
pub struct NativeResponsesRequest {
    pub body: Value,
    /// A server-filtered set of Codex protocol headers. Authentication and
    /// provider-owned routing headers are never included here.
    pub headers: HeaderMap,
}

/// An upstream Responses stream together with the response headers that are
/// safe candidates for downstream projection.
pub struct NativeResponsesResponse {
    pub headers: HeaderMap,
    pub stream: BoxStream<'static, Result<ProviderEvent, ProviderError>>,
}

/// Trait implemented by upstream provider adapters.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Provider identifier (e.g., "openai", "anthropic").
    fn id(&self) -> &str;

    /// Send a chat request and return a stream of normalized provider events.
    async fn chat(
        &self,
        request: MessagesRequest,
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError>;

    async fn chat_with_observer(
        &self,
        request: MessagesRequest,
        _observer: Option<ProviderRequestObserver>,
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        self.chat(request).await
    }

    /// Send an already-canonical OpenAI Responses request without translating
    /// its input items through the Anthropic Messages representation.
    async fn responses(
        &self,
        _request: NativeResponsesRequest,
        _observer: Option<ProviderRequestObserver>,
    ) -> Result<NativeResponsesResponse, ProviderError> {
        Err(ProviderError::InvalidRequest(
            "selected provider does not support native Responses requests".to_string(),
        ))
    }

    /// List available models from this provider.
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;

    /// Opaque authentication scope for model catalogs within this provider instance.
    /// Providers with mutable credentials must change it when the catalog owner
    /// or entitlement changes. None disables reuse while identity is unavailable.
    fn model_cache_identity(&self) -> Option<String> {
        Some(self.id().to_string())
    }

    /// Return provider account quota/rate-limit snapshots when available.
    async fn rate_limit_snapshots(&self) -> Result<Vec<RateLimitSnapshot>, ProviderError> {
        Ok(Vec::new())
    }
}
