//! ChatGPT account provider adapter.
//!
//! Uses the same OpenAI Auth device flow and Codex Responses endpoint that
//! opencode uses for ChatGPT Pro/Plus authentication.

mod auth;
pub mod capability_cache;
mod responses;
mod transport;

use std::collections::{BTreeSet, HashMap};
use std::fs;
#[cfg(not(test))]
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use claude_proxy_config::{
    Settings,
    settings::{
        ChatGptModelCapabilityOverride, ChatGptProviderConfig, ChatGptTransport,
        ClaudeCodeContextMode, DEFAULT_CHATGPT_ORIGINATOR, DEFAULT_CHATGPT_USER_AGENT,
        ProviderConfig, ProviderRuntimeConfig, ProviderType, ReasoningMarkerMode,
        ResponsesLiteMode,
    },
};
use claude_proxy_core::*;
use futures::{StreamExt, stream::BoxStream};
use reqwest::{
    Client, Response, StatusCode,
    header::{HeaderMap, HeaderValue},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

use crate::http::{
    UpstreamRequestPolicy, apply_extra_ca_certs, apply_runtime_request_config, fmt_reqwest_err,
    is_non_retryable_rate_limit_error_body, is_non_retryable_rate_limit_headers,
    map_upstream_response, read_upstream_response_json, read_upstream_response_text,
    send_upstream_request_with_policy, upstream_error_metadata_from_parts,
};
use crate::openai_compat::{
    CompactRequestKind, apply_openai_intent, classify_compact_request_body,
    log_compact_request_observability, log_request_observability,
};
use crate::provider::{
    NativeResponsesRequest, NativeResponsesResponse, Provider, ProviderError, ProviderEvent,
    ProviderRequestMetadata, ProviderRequestObserver, ProviderRequestObserverEvent,
    ProviderRequestObserverEventKind, RateLimitCredits, RateLimitSnapshot, RateLimitSource,
    RateLimitWindow,
};
use crate::reasoning_markers::marker_mode_from_request;
use tracing::{info, warn};

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const DEFAULT_CHATGPT_INSTRUCTIONS: &str = "Follow the user's instructions.";
const CHATGPT_SEND_MAX_ATTEMPTS: usize = 2;
const CHATGPT_CONNECTION_MAX_ATTEMPTS: usize = 4;
const CHATGPT_CONNECTION_BASE_RETRY_DELAY: Duration = Duration::from_secs(5);
const CHATGPT_CONNECTION_MAX_RETRY_DELAY: Duration = Duration::from_secs(60);
const CHATGPT_USAGE_FETCH_INTERVAL: Duration = Duration::from_secs(60);
const CHATGPT_REQUEST_WARNING_RATIO: usize = 80;
const CHATGPT_BYTES_PER_ESTIMATED_TOKEN: usize = 4;
const CHATGPT_TOOL_SCHEMA_BUDGET_BYTES: usize = 256 * 1024;
const CHATGPT_SYNTHETIC_SESSION_HASH_BYTES: usize = 16;
const CHATGPT_SSE_REQUEST_ZSTD_LEVEL: i32 = 3;
pub const CLAUDE_CODE_CONTEXT_1M_BETA: &str = "context-1m-2025-08-07";
pub const VIRTUAL_CONTEXT_1M_EXTRA_KEY: &str = "_claude_proxy_virtual_context_1m";
pub const CLAUDE_CODE_DEFAULT_CONTEXT_WINDOW: u32 = 200_000;
const CHATGPT_GPT_56_DEFAULT_CONTEXT_WINDOW: u32 = 872_000;
const CLAUDE_CODE_COMPACT_SUMMARY_OUTPUT_RESERVE: u32 = 20_000;
const CLAUDE_CODE_AUTO_COMPACT_BUFFER: u32 = 13_000;
const CHATGPT_MEDIA_ESTIMATED_TOKENS: u64 = 2_000;
const CHATGPT_WEBSOCKET_SERVER_ERROR_COOLDOWN_SECS: u64 = 120;
const CHATGPT_WEBSOCKET_STARTUP_FAILURE_COOLDOWN_SECS: u64 = 120;
const CODEX_FAST_SERVICE_TIER: &str = "priority";
const X_OPENAI_INTERNAL_CODEX_RESPONSES_LITE_HEADER: &str =
    "x-openai-internal-codex-responses-lite";
const PROACTIVE_MULTI_AGENT_INSTRUCTIONS: &str = "Proactive multi-agent delegation is active. Any earlier instruction requiring an explicit user request before spawning sub-agents no longer applies. Use sub-agents when parallel work would materially improve speed or quality. This mode remains active until a later multi-agent mode developer message changes it.";

#[derive(Debug, Deserialize)]
struct UsagePayload {
    #[serde(default)]
    plan_type: Option<String>,
    #[serde(default)]
    rate_limit_reached_type: Option<RateLimitReachedPayload>,
    #[serde(default)]
    rate_limit: Option<RateLimitWindowPayload>,
    #[serde(default)]
    credits: Option<CreditsPayload>,
    #[serde(default)]
    spend_control: Option<SpendControlPayload>,
    #[serde(default, alias = "spendControlReached")]
    spend_control_reached: Option<bool>,
    #[serde(default)]
    additional_rate_limits: Option<Vec<AdditionalRateLimitPayload>>,
}

#[derive(Debug, Deserialize)]
struct SpendControlPayload {
    #[serde(default)]
    reached: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RateLimitReachedPayload {
    #[serde(default)]
    #[serde(alias = "type")]
    kind: Option<String>,
}

#[derive(Debug, Deserialize)]
struct AdditionalRateLimitPayload {
    metered_feature: String,
    #[serde(default)]
    limit_name: Option<String>,
    #[serde(default)]
    rate_limit: Option<RateLimitWindowPayload>,
}

#[derive(Debug, Deserialize)]
struct RateLimitWindowPayload {
    #[serde(default)]
    #[serde(alias = "primary_window")]
    primary: Option<RateLimitBucketPayload>,
    #[serde(default)]
    #[serde(alias = "secondary_window")]
    secondary: Option<RateLimitBucketPayload>,
}

#[derive(Debug, Deserialize)]
struct RateLimitBucketPayload {
    used_percent: f64,
    #[serde(default)]
    window_minutes: Option<u64>,
    #[serde(default)]
    limit_window_seconds: Option<u64>,
    #[serde(default)]
    reset_at: Option<serde_json::Value>,
    #[serde(default)]
    resets_at: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
struct CreditsPayload {
    #[serde(default)]
    has_credits: Option<bool>,
    #[serde(default)]
    unlimited: Option<bool>,
    #[serde(default)]
    balance: Option<serde_json::Value>,
}

struct CachedRateLimits {
    snapshots: Vec<RateLimitSnapshot>,
    fetched_at: Option<Instant>,
    hard_stop_generation: u64,
}

#[derive(Debug, Deserialize)]
struct ChatGptModelsPayload {
    models: Vec<ChatGptRemoteModel>,
}

const fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct ChatGptRemoteModel {
    slug: String,
    #[serde(default)]
    supported_reasoning_levels: Vec<ChatGptRemoteReasoningLevel>,
    #[serde(default)]
    context_window: Option<i64>,
    #[serde(default)]
    max_context_window: Option<i64>,
    #[serde(default)]
    input_modalities: Vec<String>,
    #[serde(default)]
    use_responses_lite: bool,
    #[serde(default = "default_true")]
    supports_reasoning_summary_parameter: bool,
    #[serde(default = "default_true")]
    supports_parallel_tool_calls: bool,
    #[serde(default)]
    auto_compact_token_limit: Option<i64>,
    #[serde(default)]
    effective_context_window_percent: Option<i64>,
    #[serde(default)]
    #[serde(rename = "default_reasoning_level")]
    _default_reasoning_level: Option<String>,
    #[serde(default)]
    #[serde(rename = "default_reasoning_summary")]
    _default_reasoning_summary: Option<String>,
    #[serde(default)]
    #[serde(rename = "support_verbosity")]
    _support_verbosity: Option<bool>,
    #[serde(default)]
    #[serde(rename = "default_verbosity")]
    _default_verbosity: Option<String>,
    #[serde(default)]
    visibility: Option<String>,
    #[serde(default)]
    priority: i32,
    #[serde(default)]
    service_tiers: Option<Vec<ChatGptRemoteServiceTier>>,
}

#[derive(Debug, Deserialize)]
struct ChatGptRemoteReasoningLevel {
    effort: String,
}

#[derive(Debug, Deserialize)]
struct ChatGptRemoteServiceTier {
    id: String,
}

#[derive(Debug, Clone)]
struct ChatGptCatalogModel {
    info: ModelInfo,
    responses_lite: bool,
    supports_reasoning_summary_parameter: bool,
    supports_parallel_tool_calls: bool,
    auto_compact_token_limit: Option<u32>,
    effective_context_window_percent: Option<u32>,
    visibility: Option<String>,
    priority: i32,
    service_tiers: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ChatGptOutputTokenBudget {
    requested: Option<u64>,
    effective: Option<u64>,
}

#[derive(Debug, Clone, Copy)]
struct ChatGptSseRequestContext {
    compact_request: bool,
    request_id: u64,
    budget: ChatGptOutputTokenBudget,
    responses_lite: ResponsesLiteDecision,
}

#[derive(Debug)]
struct ChatGptSseRequestBody {
    bytes: Vec<u8>,
    original_len: usize,
    content_encoding: Option<&'static str>,
}

#[derive(Debug, Clone)]
struct ChatGptRequestHeaders {
    originator: HeaderValue,
    user_agent: HeaderValue,
}

#[derive(Debug, Clone)]
pub(super) struct ChatGptRuntimeIds {
    pub(super) session_id: String,
    pub(super) thread_id: String,
    pub(super) window_id: String,
}

impl ChatGptRuntimeIds {
    fn new() -> Self {
        Self {
            session_id: chatgpt_runtime_id(),
            thread_id: chatgpt_runtime_id(),
            window_id: chatgpt_runtime_id(),
        }
    }
}

#[derive(Default)]
struct ChatGptWebSocketStats {
    attempts: AtomicU64,
    successes: AtomicU64,
    fallbacks: AtomicU64,
    failures: AtomicU64,
    connections_created: AtomicU64,
    connections_reused: AtomicU64,
}

#[derive(Debug, Clone, Copy)]
struct ChatGptWebSocketStatsSnapshot {
    attempts: u64,
    successes: u64,
    fallbacks: u64,
    failures: u64,
    connections_created: u64,
    connections_reused: u64,
}

impl ChatGptWebSocketStats {
    fn snapshot(&self) -> ChatGptWebSocketStatsSnapshot {
        ChatGptWebSocketStatsSnapshot {
            attempts: self.attempts.load(Ordering::Relaxed),
            successes: self.successes.load(Ordering::Relaxed),
            fallbacks: self.fallbacks.load(Ordering::Relaxed),
            failures: self.failures.load(Ordering::Relaxed),
            connections_created: self.connections_created.load(Ordering::Relaxed),
            connections_reused: self.connections_reused.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ResponsesLiteDecision {
    enabled: bool,
    source: ResponsesLiteDecisionSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponsesLiteDecisionSource {
    ForcedOn,
    ForcedOff,
    OverrideEnabled,
    OverrideDisabled,
    ModelCapability,
    UnknownModel,
}

impl ResponsesLiteDecision {
    fn disabled(source: ResponsesLiteDecisionSource) -> Self {
        Self {
            enabled: false,
            source,
        }
    }

    fn enabled(source: ResponsesLiteDecisionSource) -> Self {
        Self {
            enabled: true,
            source,
        }
    }

    pub(super) fn is_enabled(self) -> bool {
        self.enabled
    }

    fn source_str(self) -> &'static str {
        match self.source {
            ResponsesLiteDecisionSource::ForcedOn => "forced_on",
            ResponsesLiteDecisionSource::ForcedOff => "forced_off",
            ResponsesLiteDecisionSource::OverrideEnabled => "override_enabled",
            ResponsesLiteDecisionSource::OverrideDisabled => "override_disabled",
            ResponsesLiteDecisionSource::ModelCapability => "model_capability",
            ResponsesLiteDecisionSource::UnknownModel => "unknown_model",
        }
    }
}

#[derive(Clone)]
struct ChatGptPreparedRequest {
    body: Value,
    responses_correlation: crate::responses::ResponsesCorrelation,
    marker_mode: ReasoningMarkerMode,
    compact_request: bool,
    request_id: u64,
    output_token_budget: ChatGptOutputTokenBudget,
    stable_client_conversation_id: Option<String>,
    responses_lite: ResponsesLiteDecision,
    observer: Option<ProviderRequestObserver>,
    pending_context_usage: Option<PendingContextUsage>,
}

struct ChatGptSseStreamContext {
    marker_mode: ReasoningMarkerMode,
    request_id: u64,
    compact_request: bool,
    observer: Option<ProviderRequestObserver>,
    pending_context_usage: Option<PendingContextUsage>,
    responses_correlation: crate::responses::ResponsesCorrelation,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ContextUsageKey {
    provider_id: String,
    account_hash: String,
    model: String,
    stable_client_conversation_id: String,
}

#[derive(Debug, Clone)]
struct ContextUsageBaseline {
    static_body: Value,
    context_items: Vec<Value>,
    total_tokens: u64,
    updated_at: Instant,
}

#[derive(Debug, Clone)]
struct PendingContextUsage {
    key: ContextUsageKey,
    static_body: Value,
    full_input: Vec<Value>,
    compact_kind: CompactRequestKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContextEstimatorSource {
    UsagePlusDelta,
    FullRough,
}

impl ContextEstimatorSource {
    fn as_str(self) -> &'static str {
        match self {
            Self::UsagePlusDelta => "usage_plus_delta",
            Self::FullRough => "full_rough",
        }
    }
}

#[derive(Debug, Clone)]
struct VirtualContextEstimate {
    estimated_tokens: u64,
    safe_input_limit: u32,
    model_context_window: u32,
    estimator_source: ContextEstimatorSource,
    compact_kind: CompactRequestKind,
    compressible_history: bool,
    pending_usage: Option<PendingContextUsage>,
}

struct ChatGptUpstreamEventContext {
    request_id: u64,
    compact_request: bool,
    transport: &'static str,
    first_upstream_event_seen: Arc<AtomicBool>,
    thinking_diagnostics: Arc<ChatGptThinkingDiagnostics>,
    stream_started_at: Instant,
    observer: Option<ProviderRequestObserver>,
    pending_context_usage: Option<PendingContextUsage>,
}

struct ChatGptUpstreamEventHandlerState {
    request_id: u64,
    compact_request: bool,
    transport: &'static str,
    first_upstream_event_seen: Arc<AtomicBool>,
    thinking_diagnostics: Arc<ChatGptThinkingDiagnostics>,
    stream_started_at: Instant,
    observer: Option<ProviderRequestObserver>,
    cache: Arc<Mutex<CachedRateLimits>>,
    provider_id: String,
    runtime_ids: Arc<RwLock<ChatGptRuntimeIds>>,
    websocket_sse_cooldown_until_secs: Arc<AtomicU64>,
    context_usage: Arc<StdMutex<HashMap<ContextUsageKey, ContextUsageBaseline>>>,
    pending_context_usage: Option<PendingContextUsage>,
}

impl ChatGptUpstreamEventHandlerState {
    fn handle(&self, event: &Value) {
        let event_type = event
            .get("type")
            .and_then(|value| value.as_str())
            .unwrap_or("unknown");

        self.log_first_event(event_type);
        self.record_reasoning_delta(event_type, event);
        self.cache_stream_rate_limit(event);
        self.notify_observer(event);
        self.record_upstream_context_overflow(event);
        self.record_context_usage(event);
        self.log_terminal_event(event);
    }

    fn record_context_usage(&self, event: &Value) {
        let Some(pending) = self.pending_context_usage.as_ref() else {
            return;
        };
        let event_type = event.get("type").and_then(Value::as_str);
        if !matches!(
            event_type,
            Some("response.completed" | "response.incomplete")
        ) {
            return;
        }
        let response = event.get("response").unwrap_or(event);
        let Some(input_tokens) = response
            .pointer("/usage/input_tokens")
            .and_then(Value::as_u64)
        else {
            return;
        };
        let output_tokens = response
            .pointer("/usage/output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        let mut cache = self
            .context_usage
            .lock()
            .expect("ChatGPT context usage cache lock poisoned");
        if pending.compact_kind == CompactRequestKind::SummaryGeneration {
            cache.remove(&pending.key);
            return;
        }
        let mut context_items = pending.full_input.clone();
        if let Some(output) = response.get("output").and_then(Value::as_array) {
            context_items.extend(output.iter().cloned());
        }
        cache.insert(
            pending.key.clone(),
            ContextUsageBaseline {
                static_body: pending.static_body.clone(),
                context_items,
                total_tokens: input_tokens.saturating_add(output_tokens),
                updated_at: Instant::now(),
            },
        );
    }

    fn record_upstream_context_overflow(&self, event: &Value) {
        let body = event.to_string();
        if !is_prompt_too_long_error(StatusCode::OK, &body) {
            return;
        }
        notify_request_metadata_observer(
            self.observer.as_ref(),
            ProviderRequestMetadata {
                context_upstream_overflow: Some(true),
                ..ProviderRequestMetadata::default()
            },
        );
    }

    fn log_first_event(&self, event_type: &str) {
        if !self.first_upstream_event_seen.swap(true, Ordering::Relaxed) {
            info!(
                request_id = self.request_id,
                compact_request = self.compact_request,
                transport = self.transport,
                elapsed_ms = elapsed_millis(self.stream_started_at),
                event_type,
                "ChatGPT upstream first event received"
            );
        }
    }

    fn record_reasoning_delta(&self, event_type: &str, event: &Value) {
        if !is_chatgpt_reasoning_delta_event(event_type) {
            return;
        }

        let delta_bytes = chatgpt_sse_delta_len(event) as u64;
        let count = self
            .thinking_diagnostics
            .upstream_reasoning_delta_events
            .fetch_add(1, Ordering::Relaxed)
            + 1;
        self.thinking_diagnostics
            .upstream_reasoning_delta_bytes
            .fetch_add(delta_bytes, Ordering::Relaxed);
        if !self
            .thinking_diagnostics
            .first_upstream_reasoning_logged
            .swap(true, Ordering::Relaxed)
        {
            info!(
                request_id = self.request_id,
                compact_request = self.compact_request,
                transport = self.transport,
                elapsed_ms = elapsed_millis(self.stream_started_at),
                event_type,
                upstream_reasoning_delta_events = count,
                upstream_reasoning_delta_bytes = delta_bytes,
                "ChatGPT upstream reasoning delta observed"
            );
        }
    }

    fn cache_stream_rate_limit(&self, event: &Value) {
        let Some(snapshot) =
            rate_limit_snapshot_from_sse_event(&self.provider_id, event, unix_timestamp_secs())
        else {
            return;
        };

        log_rate_limit_summary(
            self.request_id,
            self.compact_request,
            "stream_event",
            std::slice::from_ref(&snapshot),
        );
        let cache = Arc::clone(&self.cache);
        tokio::spawn(async move {
            cache_rate_limits_into(&cache, vec![snapshot]).await;
        });
    }

    fn notify_observer(&self, event: &Value) {
        if let Some(observer) = self.observer.as_ref() {
            crate::responses::notify_stream_metadata(Some(observer), event);
        }
    }

    fn log_terminal_event(&self, event: &Value) {
        let Some(stop_reason) = chatgpt_sse_stop_reason(event) else {
            return;
        };

        info!(
            request_id = self.request_id,
            compact_request = self.compact_request,
            transport = self.transport,
            upstream_stop_reason = stop_reason,
            upstream_response_status = chatgpt_sse_response_status(event).unwrap_or(""),
            upstream_error_code = chatgpt_sse_error_code(event).unwrap_or(""),
            upstream_error_message = chatgpt_sse_error_message(event).unwrap_or(""),
            upstream_model = chatgpt_sse_model(event).unwrap_or("unknown"),
            upstream_response_id = chatgpt_sse_response_id(event).unwrap_or(""),
            upstream_reasoning_delta_events = self
                .thinking_diagnostics
                .upstream_reasoning_delta_events
                .load(Ordering::Relaxed),
            upstream_reasoning_delta_bytes = self
                .thinking_diagnostics
                .upstream_reasoning_delta_bytes
                .load(Ordering::Relaxed),
            downstream_thinking_delta_events = self
                .thinking_diagnostics
                .downstream_thinking_delta_events
                .load(Ordering::Relaxed),
            downstream_thinking_delta_bytes = self
                .thinking_diagnostics
                .downstream_thinking_delta_bytes
                .load(Ordering::Relaxed),
            "ChatGPT upstream terminal event received"
        );
        if chatgpt_event_is_server_error(event) {
            rotate_chatgpt_runtime_ids_after_server_error(
                &self.runtime_ids,
                self.request_id,
                self.transport,
            );
            ChatGptProvider::activate_websocket_sse_cooldown(
                &self.websocket_sse_cooldown_until_secs,
                self.request_id,
                self.transport,
            );
        }
    }
}

pub use auth::{ChatGptAuth, ChatGptToken, DeviceCodeInfo};

pub struct ChatGptProvider {
    id: String,
    base_url: String,
    http_client: Client,
    endpoint: String,
    models_endpoint: String,
    usage_endpoint: String,
    installation_id: String,
    runtime_ids: Arc<RwLock<ChatGptRuntimeIds>>,
    request_headers: ChatGptRequestHeaders,
    request_policy: UpstreamRequestPolicy,
    runtime: ProviderRuntimeConfig,
    chatgpt_config: ChatGptProviderConfig,
    proxy: Option<String>,
    extra_ca_certs: Vec<String>,
    transport: ChatGptTransport,
    websocket_sse_cooldown_until_secs: Arc<AtomicU64>,
    websocket_stats: ChatGptWebSocketStats,
    websocket_session: Arc<Mutex<transport::ChatGptWebSocketSession>>,
    auth: Arc<ChatGptAuth>,
    remote_models: Arc<RwLock<HashMap<String, ChatGptCatalogModel>>>,
    cached_rate_limits: Arc<Mutex<CachedRateLimits>>,
    context_usage: Arc<StdMutex<HashMap<ContextUsageKey, ContextUsageBaseline>>>,
    payload_limits: crate::http::ResponsePayloadLimits,
}

impl ChatGptProvider {
    pub async fn new(
        id: &str,
        config: &ProviderConfig,
        settings: &Settings,
    ) -> Result<Self, ProviderError> {
        let http_client = build_http_client(&config.proxy, settings)?;
        let auth =
            ChatGptAuth::new(http_client.clone(), settings.http.max_response_body_bytes).await?;
        let chatgpt_config = config.chatgpt.clone().unwrap_or_default();
        let transport = chatgpt_config.transport;
        let base_url = normalized_codex_base_url(&config.base_url);

        Ok(Self {
            id: id.to_string(),
            base_url,
            http_client,
            endpoint: codex_responses_endpoint(&config.base_url),
            models_endpoint: codex_models_endpoint(&config.base_url),
            usage_endpoint: codex_usage_endpoint(&config.base_url),
            installation_id: chatgpt_installation_id(),
            runtime_ids: Arc::new(RwLock::new(ChatGptRuntimeIds::new())),
            request_headers: chatgpt_request_headers(&chatgpt_config)?,
            request_policy: chatgpt_upstream_request_policy(&config.runtime),
            runtime: config.runtime.clone(),
            chatgpt_config,
            proxy: (!config.proxy.trim().is_empty()).then(|| config.proxy.clone()),
            extra_ca_certs: settings.http.extra_ca_certs.clone(),
            transport,
            websocket_sse_cooldown_until_secs: Arc::new(AtomicU64::new(0)),
            websocket_stats: ChatGptWebSocketStats::default(),
            websocket_session: Arc::new(Mutex::new(transport::ChatGptWebSocketSession::new())),
            auth,
            remote_models: Arc::new(RwLock::new(HashMap::new())),
            cached_rate_limits: Arc::new(Mutex::new(CachedRateLimits {
                snapshots: Vec::new(),
                fetched_at: None,
                hard_stop_generation: 0,
            })),
            context_usage: Arc::new(StdMutex::new(HashMap::new())),
            payload_limits: crate::http::ResponsePayloadLimits::from_settings(settings),
        })
    }

    pub(super) fn runtime_ids_snapshot(&self) -> ChatGptRuntimeIds {
        self.runtime_ids
            .read()
            .expect("ChatGPT runtime ids lock poisoned")
            .clone()
    }

    pub(super) fn runtime_ids_handle(&self) -> Arc<RwLock<ChatGptRuntimeIds>> {
        Arc::clone(&self.runtime_ids)
    }

    pub(super) fn websocket_sse_cooldown_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.websocket_sse_cooldown_until_secs)
    }

    pub(super) fn responses_lite_decision(&self, model: &str) -> ResponsesLiteDecision {
        match self.chatgpt_config.responses_lite {
            ResponsesLiteMode::On => {
                ResponsesLiteDecision::enabled(ResponsesLiteDecisionSource::ForcedOn)
            }
            ResponsesLiteMode::Off => {
                ResponsesLiteDecision::disabled(ResponsesLiteDecisionSource::ForcedOff)
            }
            ResponsesLiteMode::Auto => self.auto_responses_lite_decision(model),
        }
    }

    fn auto_responses_lite_decision(&self, model: &str) -> ResponsesLiteDecision {
        let normalized_model = normalize_chatgpt_model_id(model);
        if let Some(override_value) = self
            .chatgpt_config
            .model_capabilities
            .get(normalized_model)
            .and_then(|capability| capability.responses_lite)
        {
            return if override_value {
                ResponsesLiteDecision::enabled(ResponsesLiteDecisionSource::OverrideEnabled)
            } else {
                ResponsesLiteDecision::disabled(ResponsesLiteDecisionSource::OverrideDisabled)
            };
        }

        if let Some(model) = self
            .remote_models
            .read()
            .expect("ChatGPT remote models lock poisoned")
            .get(normalized_model)
        {
            return if model.responses_lite {
                ResponsesLiteDecision::enabled(ResponsesLiteDecisionSource::ModelCapability)
            } else {
                ResponsesLiteDecision::disabled(ResponsesLiteDecisionSource::UnknownModel)
            };
        }

        if chatgpt_model_supports_responses_lite(normalized_model) {
            ResponsesLiteDecision::enabled(ResponsesLiteDecisionSource::ModelCapability)
        } else {
            ResponsesLiteDecision::disabled(ResponsesLiteDecisionSource::UnknownModel)
        }
    }

    fn codex_service_tier(&self, model: &str) -> Option<&str> {
        let requested = effective_codex_service_tier(
            self.runtime.openai.service_tier.as_deref(),
            self.chatgpt_config.fast_mode,
        )?;
        let model = normalize_chatgpt_model_id(model);
        let remote_support = self
            .remote_models
            .read()
            .expect("ChatGPT remote models lock poisoned")
            .get(model)
            .and_then(|model| model.service_tiers.as_ref())
            .map(|tiers| tiers.iter().any(|tier| tier == requested));
        let supported = remote_support.or_else(|| {
            CHATGPT_MODEL_SPECS
                .iter()
                .find(|spec| spec.model_id == model)
                .map(|spec| spec.service_tiers.contains(&requested))
        });

        if supported == Some(false) {
            warn!(
                model,
                service_tier = requested,
                "configured ChatGPT service tier is unsupported by model catalog; omitting"
            );
            None
        } else {
            Some(requested)
        }
    }

    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        let model = normalize_chatgpt_model_id(model);
        self.remote_models
            .read()
            .expect("ChatGPT remote models lock poisoned")
            .get(model)
            .map(|model| model.info.clone())
            .or_else(|| chatgpt_model_info(model, &self.chatgpt_config))
    }

    fn supports_reasoning_summary_parameter(&self, model: &str) -> bool {
        let model = normalize_chatgpt_model_id(model);
        self.remote_models
            .read()
            .expect("ChatGPT remote models lock poisoned")
            .get(model)
            .map(|model| model.supports_reasoning_summary_parameter)
            .unwrap_or(true)
    }

    fn supports_parallel_tool_calls(&self, model: &str) -> bool {
        let model = normalize_chatgpt_model_id(model);
        self.remote_models
            .read()
            .expect("ChatGPT remote models lock poisoned")
            .get(model)
            .map(|model| model.supports_parallel_tool_calls)
            .unwrap_or(true)
    }

    fn model_catalog_safe_input_limit(
        &self,
        model: &str,
        model_context_window: u32,
    ) -> Option<u32> {
        let model = normalize_chatgpt_model_id(model);
        let models = self
            .remote_models
            .read()
            .expect("ChatGPT remote models lock poisoned");
        let model = models.get(model)?;
        let ninety_percent = model_context_window.saturating_mul(9) / 10;
        let auto_compact = model
            .auto_compact_token_limit
            .map(|limit| limit.min(ninety_percent));
        let effective = model.effective_context_window_percent.map(|percent| {
            model_context_window
                .saturating_mul(percent)
                .checked_div(100)
                .unwrap_or_default()
        });
        auto_compact.into_iter().chain(effective).min()
    }

    fn virtual_context_estimate(
        &self,
        body: &Value,
        token: &ChatGptToken,
        stable_client_conversation_id: Option<&str>,
        model_context_window: u32,
        compact_kind: CompactRequestKind,
    ) -> VirtualContextEstimate {
        let static_body = context_static_body(body);
        let full_input = body
            .get("input")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let compressible_history = compact_kind == CompactRequestKind::CompactedContinuation
            || input_has_compressible_history(&full_input);
        let key = stable_client_conversation_id
            .zip(capability_cache::account_hash(token.account_id.as_deref()))
            .map(
                |(stable_client_conversation_id, account_hash)| ContextUsageKey {
                    provider_id: self.id.clone(),
                    account_hash,
                    model: body
                        .get("model")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown")
                        .to_string(),
                    stable_client_conversation_id: stable_client_conversation_id.to_string(),
                },
            );

        let mut source = ContextEstimatorSource::FullRough;
        let mut estimated_tokens = estimate_context_value_tokens(body);
        if let Some(key) = key.as_ref() {
            let mut cache = self
                .context_usage
                .lock()
                .expect("ChatGPT context usage cache lock poisoned");
            cache.retain(|_, baseline| baseline.updated_at.elapsed() <= Duration::from_secs(3600));
            if compact_kind != CompactRequestKind::None {
                cache.remove(key);
            } else if let Some(baseline) = cache.get(key)
                && baseline.static_body == static_body
                && full_input.starts_with(&baseline.context_items)
            {
                estimated_tokens = baseline.total_tokens.saturating_add(
                    full_input[baseline.context_items.len()..]
                        .iter()
                        .map(estimate_context_value_tokens)
                        .sum(),
                );
                source = ContextEstimatorSource::UsagePlusDelta;
            }
        }

        let reserve = match compact_kind {
            CompactRequestKind::SummaryGeneration => CLAUDE_CODE_COMPACT_SUMMARY_OUTPUT_RESERVE,
            CompactRequestKind::CompactedContinuation => {
                CLAUDE_CODE_COMPACT_SUMMARY_OUTPUT_RESERVE + CLAUDE_CODE_AUTO_COMPACT_BUFFER
            }
            CompactRequestKind::None if compressible_history => {
                CLAUDE_CODE_COMPACT_SUMMARY_OUTPUT_RESERVE + CLAUDE_CODE_AUTO_COMPACT_BUFFER
            }
            CompactRequestKind::None => CLAUDE_CODE_COMPACT_SUMMARY_OUTPUT_RESERVE,
        };
        let safe_input_limit = model_context_window.saturating_sub(reserve).min(
            self.model_catalog_safe_input_limit(
                body.get("model")
                    .and_then(Value::as_str)
                    .unwrap_or_default(),
                model_context_window,
            )
            .unwrap_or(u32::MAX),
        );
        let pending_usage = key.map(|key| PendingContextUsage {
            key,
            static_body,
            full_input,
            compact_kind,
        });
        VirtualContextEstimate {
            estimated_tokens,
            safe_input_limit,
            model_context_window,
            estimator_source: source,
            compact_kind,
            compressible_history,
            pending_usage,
        }
    }

    async fn fetch_remote_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let token = self.auth.get_existing_token().await?;
        let client_version =
            local_codex_cli_version().unwrap_or_else(|| env!("CARGO_PKG_VERSION").to_string());
        let mut request_builder = self
            .http_client
            .get(&self.models_endpoint)
            .query(&[("client_version", client_version.as_str())])
            .bearer_auth(&token.access_token)
            .header("Accept", "application/json")
            .header("originator", self.request_headers.originator.clone())
            .header("User-Agent", self.request_headers.user_agent.clone());
        if let Some(account_id) = token.account_id.as_deref() {
            request_builder = request_builder.header("ChatGPT-Account-Id", account_id);
        }

        let request_builder = apply_runtime_request_config(request_builder, &self.runtime)?;
        let response =
            send_upstream_request_with_policy(request_builder, self.request_policy).await?;
        if !response.status().is_success() {
            return Err(map_upstream_response(response).await);
        }
        let payload: ChatGptModelsPayload = read_upstream_response_json(
            response,
            self.payload_limits.max_response_body_bytes,
            "invalid ChatGPT models response",
        )
        .await?;
        if payload.models.is_empty() {
            return Err(ProviderError::UpstreamError {
                status: 502,
                body: "ChatGPT models response contained no models".to_string(),
            });
        }

        let mut catalog = payload
            .models
            .into_iter()
            .map(|model| chatgpt_catalog_model_from_remote(model, &self.chatgpt_config))
            .collect::<Vec<_>>();
        catalog.sort_by_key(|model| model.priority);

        let mut cached = HashMap::with_capacity(catalog.len());
        for model in &catalog {
            cached.insert(model.info.model_id.clone(), model.clone());
        }
        *self
            .remote_models
            .write()
            .expect("ChatGPT remote models lock poisoned") = cached;

        let mut visible = catalog
            .iter()
            .filter(|model| {
                model
                    .visibility
                    .as_deref()
                    .is_none_or(|visibility| visibility.eq_ignore_ascii_case("list"))
            })
            .map(|model| model.info.clone())
            .collect::<Vec<_>>();
        append_configured_chatgpt_models(
            &mut visible,
            &self.chatgpt_config,
            catalog.iter().map(|model| model.info.model_id.as_str()),
        );
        if let Some(model) = visible.first_mut() {
            model.is_chat_default = Some(true);
        }
        if let Some(account_hash) = capability_cache::account_hash(token.account_id.as_deref()) {
            let cached_models = catalog
                .iter()
                .map(|model| model.info.clone())
                .chain(visible.iter().cloned())
                .collect::<Vec<_>>();
            capability_cache::store_model_capabilities(
                &self.id,
                &self.base_url,
                &account_hash,
                &cached_models,
            );
        }
        Ok(visible)
    }

    pub(super) fn activate_websocket_sse_cooldown(
        cooldown_until_secs: &Arc<AtomicU64>,
        request_id: u64,
        transport: &'static str,
    ) {
        Self::activate_websocket_sse_cooldown_for(
            cooldown_until_secs,
            request_id,
            transport,
            CHATGPT_WEBSOCKET_SERVER_ERROR_COOLDOWN_SECS,
            "upstream server_error",
        );
    }

    fn activate_websocket_sse_cooldown_for(
        cooldown_until_secs: &Arc<AtomicU64>,
        request_id: u64,
        transport: &'static str,
        cooldown_secs: u64,
        reason: &'static str,
    ) -> u64 {
        let cooldown_until = unix_timestamp_secs().saturating_add(cooldown_secs);
        cooldown_until_secs.store(cooldown_until, Ordering::Relaxed);
        warn!(
            request_id,
            transport,
            cooldown_secs,
            cooldown_until,
            reason,
            "ChatGPT websocket temporarily disabled"
        );
        cooldown_until
    }

    #[cfg(test)]
    async fn send_responses_request(
        &self,
        body: &Value,
        token: &ChatGptToken,
        context: ChatGptSseRequestContext,
        prompt_too_long_attempt: usize,
    ) -> Result<Response, ProviderError> {
        self.send_responses_request_with_headers(
            body,
            token,
            context,
            prompt_too_long_attempt,
            None,
        )
        .await
    }

    async fn send_responses_request_with_headers(
        &self,
        body: &Value,
        token: &ChatGptToken,
        context: ChatGptSseRequestContext,
        prompt_too_long_attempt: usize,
        forwarded_headers: Option<&HeaderMap>,
    ) -> Result<Response, ProviderError> {
        let ChatGptSseRequestContext {
            compact_request,
            request_id,
            budget,
            responses_lite,
        } = context;
        let request_body = prepare_chatgpt_sse_request_body(body)?;
        let body_bytes = request_body.original_len;
        let body_wire_bytes = request_body.bytes.len();
        let body_content_encoding = request_body.content_encoding.unwrap_or("identity");
        let model = body
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let model_context_window = self
            .model_info(model)
            .and_then(|info| info.capabilities.limits.context_window);
        warn_if_request_nears_context_window(
            request_id,
            compact_request,
            prompt_too_long_attempt,
            model,
            model_context_window,
            body_bytes,
        );
        let started_at = Instant::now();
        let runtime_ids = self.runtime_ids_snapshot();
        let session_id = responses::codex_client_metadata_value(body, "session_id")
            .unwrap_or(&runtime_ids.session_id);
        let thread_id = responses::codex_client_metadata_value(body, "thread_id")
            .unwrap_or(&runtime_ids.thread_id);
        let window_id = responses::codex_client_metadata_value(body, "x-codex-window-id")
            .unwrap_or(&runtime_ids.window_id);
        let client_request_id = chatgpt_runtime_id();
        info!(
            request_id,
            compact_request,
            prompt_too_long_attempt,
            model,
            body_bytes,
            body_wire_bytes,
            body_content_encoding,
            upstream_request_id = %client_request_id,
            session_id,
            thread_id,
            window_id,
            requested_output_tokens = budget.requested.unwrap_or(0),
            requested_output_tokens_present = budget.requested.is_some(),
            effective_output_tokens = budget.effective.unwrap_or(0),
            effective_output_tokens_present = budget.effective.is_some(),
            final_reasoning = %chatgpt_reasoning_log_value(body),
            responses_lite = responses_lite.is_enabled(),
            responses_lite_source = responses_lite.source_str(),
            endpoint = %self.endpoint,
            "ChatGPT upstream request started"
        );

        let mut request_builder = self
            .http_client
            .post(&self.endpoint)
            .bearer_auth(&token.access_token)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .header("originator", self.request_headers.originator.clone())
            .header("User-Agent", self.request_headers.user_agent.clone())
            .header("x-client-request-id", client_request_id)
            .header("session-id", session_id)
            .header("thread-id", thread_id)
            .header("x-codex-window-id", window_id);

        if let Some(headers) = forwarded_headers {
            request_builder = request_builder.headers(headers.clone());
        }

        if let Some(routing_hint) = responses::codex_routing_hint(body) {
            let routing_hint = HeaderValue::from_str(&routing_hint).map_err(|error| {
                ProviderError::InvalidRequest(format!(
                    "invalid ChatGPT x-codex-routing-hint header value: {error}"
                ))
            })?;
            request_builder = request_builder.header("x-codex-routing-hint", routing_hint);
        }
        if let Some(turn_metadata) = responses::codex_turn_metadata(body) {
            let turn_metadata = HeaderValue::from_str(turn_metadata).map_err(|error| {
                ProviderError::InvalidRequest(format!(
                    "invalid ChatGPT x-codex-turn-metadata header value: {error}"
                ))
            })?;
            request_builder = request_builder.header("x-codex-turn-metadata", turn_metadata);
        }

        if responses_lite.is_enabled() {
            request_builder =
                request_builder.header(X_OPENAI_INTERNAL_CODEX_RESPONSES_LITE_HEADER, "true");
        }

        if let Some(account_id) = token.account_id.as_deref() {
            request_builder = request_builder.header("ChatGPT-Account-Id", account_id);
        }
        if let Some(content_encoding) = request_body.content_encoding {
            request_builder = request_builder.header("Content-Encoding", content_encoding);
        }

        let request_builder = apply_runtime_request_config(request_builder, &self.runtime)?;
        let result = send_upstream_request_with_policy(
            request_builder.body(request_body.bytes),
            self.request_policy,
        )
        .await;

        match &result {
            Ok(response) => {
                let upstream_response_id = upstream_request_id_from_headers(response.headers());
                let upstream_model_header = upstream_model_from_headers(response.headers());
                info!(
                    request_id,
                    compact_request,
                    prompt_too_long_attempt,
                    status = response.status().as_u16(),
                    upstream_request_id = upstream_response_id.as_deref().unwrap_or(""),
                    upstream_model_header = upstream_model_header.as_deref().unwrap_or(""),
                    elapsed_ms = elapsed_millis(started_at),
                    "ChatGPT upstream response headers received"
                );
            }
            Err(error) => {
                warn!(
                    request_id,
                    compact_request,
                    prompt_too_long_attempt,
                    elapsed_ms = elapsed_millis(started_at),
                    error = %error,
                    "ChatGPT upstream request failed before response headers"
                );
            }
        }

        result
    }

    async fn send_responses_request_with_prompt_too_long_retry(
        &self,
        body: &mut Value,
        token: &ChatGptToken,
        context: ChatGptSseRequestContext,
        observer: Option<&ProviderRequestObserver>,
    ) -> Result<Response, ProviderError> {
        self.send_responses_request_with_prompt_too_long_retry_and_headers(
            body, token, context, observer, None,
        )
        .await
    }

    async fn send_responses_request_with_prompt_too_long_retry_and_headers(
        &self,
        body: &mut Value,
        token: &ChatGptToken,
        context: ChatGptSseRequestContext,
        observer: Option<&ProviderRequestObserver>,
        forwarded_headers: Option<&HeaderMap>,
    ) -> Result<Response, ProviderError> {
        validate_chatgpt_tool_schema_budget(body)?;
        let body_bytes = json_len(body);
        notify_request_metadata_observer(
            observer,
            ProviderRequestMetadata {
                transport: Some("sse".to_string()),
                responses_lite: Some(context.responses_lite.is_enabled()),
                request_body_bytes: Some(body_bytes as u64),
                upstream_send_body_bytes: Some(body_bytes as u64),
                ..ProviderRequestMetadata::default()
            },
        );
        let current_budget = ChatGptOutputTokenBudget {
            requested: context.budget.requested,
            effective: body.get("max_output_tokens").and_then(Value::as_u64),
        };
        let response = self
            .send_responses_request_with_headers(
                body,
                token,
                ChatGptSseRequestContext {
                    budget: current_budget,
                    ..context
                },
                0,
                forwarded_headers,
            )
            .await?;
        let status = response.status();
        if status.is_success() || status == StatusCode::UNAUTHORIZED {
            if context.compact_request {
                info!(
                    request_id = context.request_id,
                    status = status.as_u16(),
                    prompt_too_long_retry_triggered = false,
                    prompt_too_long_retries = 0,
                    "Compact request prompt-too-long retry result"
                );
            }
            return Ok(response);
        }

        if !is_prompt_too_long_candidate_status(status) {
            return Err(map_upstream_response(response).await);
        }

        let headers = response.headers().clone();
        let error_body =
            read_upstream_response_text(response, self.payload_limits.max_response_body_bytes)
                .await?;
        if is_prompt_too_long_error(status, &error_body) {
            notify_request_metadata_observer(
                observer,
                ProviderRequestMetadata {
                    context_upstream_overflow: Some(true),
                    ..ProviderRequestMetadata::default()
                },
            );
        }
        Err(map_chatgpt_error_status_body_with_headers(
            status, &headers, error_body,
        ))
    }

    async fn chat_prepared_with_token(
        &self,
        prepared: ChatGptPreparedRequest,
        token: ChatGptToken,
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        match self.effective_transport() {
            ChatGptTransport::Sse => self.chat_via_sse_with_auth_retry(prepared, token).await,
            ChatGptTransport::Websocket => self
                .chat_via_websocket_with_auth_retry(prepared, token)
                .await
                .map_err(|error| error.error),
            ChatGptTransport::Auto => {
                match self
                    .chat_via_websocket_with_auth_retry(prepared.clone(), token.clone())
                    .await
                {
                    Ok(stream) => Ok(stream),
                    Err(error) if error.fallback_allowed => {
                        self.websocket_stats
                            .fallbacks
                            .fetch_add(1, Ordering::Relaxed);
                        let cooldown_until = Self::activate_websocket_sse_cooldown_for(
                            &self.websocket_sse_cooldown_until_secs,
                            prepared.request_id,
                            "websocket",
                            CHATGPT_WEBSOCKET_STARTUP_FAILURE_COOLDOWN_SECS,
                            "startup failure before first event",
                        );
                        let stats = self.websocket_stats.snapshot();
                        warn!(
                            request_id = prepared.request_id,
                            compact_request = prepared.compact_request,
                            selected_transport = "websocket",
                            fallback_transport = "sse",
                            websocket_failure_phase = error.phase.as_str(),
                            cooldown_secs = CHATGPT_WEBSOCKET_STARTUP_FAILURE_COOLDOWN_SECS,
                            cooldown_until,
                            websocket_attempts = stats.attempts,
                            websocket_successes = stats.successes,
                            websocket_failures = stats.failures,
                            websocket_fallbacks = stats.fallbacks,
                            error = %error.error,
                            "ChatGPT websocket startup failed before first event; falling back to SSE"
                        );
                        let fallback_reason = chatgpt_websocket_fallback_reason(&error);
                        notify_request_metadata_observer(
                            prepared.observer.as_ref(),
                            ProviderRequestMetadata {
                                continuation_fallback_used: Some(true),
                                fallback_reason: Some(fallback_reason.to_string()),
                                ..ProviderRequestMetadata::default()
                            },
                        );
                        self.chat_via_sse_with_auth_retry(prepared, token).await
                    }
                    Err(error) => Err(error.error),
                }
            }
        }
    }

    fn effective_transport(&self) -> ChatGptTransport {
        match self.transport {
            ChatGptTransport::Auto
                if self
                    .websocket_sse_cooldown_until_secs
                    .load(Ordering::Relaxed)
                    > unix_timestamp_secs() =>
            {
                ChatGptTransport::Sse
            }
            transport => transport,
        }
    }

    async fn chat_via_sse_with_auth_retry(
        &self,
        prepared: ChatGptPreparedRequest,
        token: ChatGptToken,
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        let ChatGptPreparedRequest {
            mut body,
            responses_correlation,
            marker_mode,
            compact_request,
            request_id,
            output_token_budget,
            observer,
            responses_lite,
            pending_context_usage,
            ..
        } = prepared;
        let mut response = self
            .send_responses_request_with_prompt_too_long_retry(
                &mut body,
                &token,
                ChatGptSseRequestContext {
                    compact_request,
                    request_id,
                    budget: output_token_budget,
                    responses_lite,
                },
                observer.as_ref(),
            )
            .await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            let refreshed = match self.auth.force_refresh_token().await {
                Ok(token) => token,
                Err(error) => {
                    if error.is_authentication() {
                        self.auth.clear_token().await;
                    }
                    return Err(error);
                }
            };
            response = self
                .send_responses_request_with_prompt_too_long_retry(
                    &mut body,
                    &refreshed,
                    ChatGptSseRequestContext {
                        compact_request,
                        request_id,
                        budget: output_token_budget,
                        responses_lite,
                    },
                    observer.as_ref(),
                )
                .await?;
            if response.status() == StatusCode::UNAUTHORIZED {
                self.auth.clear_token().await;
            }
        }

        if !response.status().is_success() {
            return Err(map_upstream_response(response).await);
        }

        Ok(self
            .stream_sse_response(
                response,
                ChatGptSseStreamContext {
                    marker_mode,
                    request_id,
                    compact_request,
                    observer,
                    pending_context_usage,
                    responses_correlation,
                },
            )
            .await)
    }

    async fn chat_via_websocket_with_auth_retry(
        &self,
        prepared: ChatGptPreparedRequest,
        mut token: ChatGptToken,
    ) -> Result<
        BoxStream<'static, Result<ProviderEvent, ProviderError>>,
        transport::ChatGptWebSocketStartError,
    > {
        let mut authentication_retried = false;
        let mut stale_continuation_retried = false;

        loop {
            match self.start_websocket_stream(prepared.clone(), &token).await {
                Ok(stream) => return Ok(stream),
                Err(error)
                    if !stale_continuation_retried
                        && error.fallback_allowed
                        && transport::provider_error_is_previous_response_not_found(
                            &error.error,
                        ) =>
                {
                    stale_continuation_retried = true;
                }
                Err(error) if !authentication_retried && error.error.is_authentication() => {
                    authentication_retried = true;
                    token = match self.auth.force_refresh_token().await {
                        Ok(token) => token,
                        Err(error) => {
                            if error.is_authentication() {
                                self.auth.clear_token().await;
                            }
                            return Err(transport::ChatGptWebSocketStartError {
                                error,
                                fallback_allowed: false,
                                phase: transport::ChatGptWebSocketPhase::Protocol,
                            });
                        }
                    };
                }
                Err(mut error) => {
                    if error.error.is_authentication() {
                        self.auth.clear_token().await;
                    }
                    if stale_continuation_retried
                        && transport::provider_error_is_previous_response_not_found(&error.error)
                    {
                        error.fallback_allowed = true;
                    }
                    return Err(error);
                }
            }
        }
    }

    async fn start_websocket_stream(
        &self,
        prepared: ChatGptPreparedRequest,
        token: &ChatGptToken,
    ) -> Result<
        BoxStream<'static, Result<ProviderEvent, ProviderError>>,
        transport::ChatGptWebSocketStartError,
    > {
        let ChatGptPreparedRequest {
            body,
            responses_correlation,
            marker_mode,
            compact_request,
            request_id,
            observer,
            stable_client_conversation_id,
            responses_lite,
            pending_context_usage,
            ..
        } = prepared;
        let final_reasoning = chatgpt_reasoning_log_value(&body);
        self.websocket_stats
            .attempts
            .fetch_add(1, Ordering::Relaxed);
        let websocket_prewarmed = if self.chatgpt_config.websocket_prewarm {
            transport::prewarm_websocket(
                self,
                &body,
                token,
                stable_client_conversation_id.as_deref(),
                request_id,
                responses_lite,
            )
            .await
            .inspect_err(|_| {
                self.websocket_stats
                    .failures
                    .fetch_add(1, Ordering::Relaxed);
            })?
        } else {
            false
        };
        let first_upstream_event_seen = Arc::new(AtomicBool::new(false));
        let thinking_diagnostics = Arc::new(ChatGptThinkingDiagnostics::default());
        let stream_started_at = Instant::now();
        let metadata_observer = observer.clone();
        let on_event = self.upstream_event_handler(ChatGptUpstreamEventContext {
            request_id,
            compact_request,
            transport: "websocket",
            first_upstream_event_seen: Arc::clone(&first_upstream_event_seen),
            thinking_diagnostics: Arc::clone(&thinking_diagnostics),
            stream_started_at,
            observer,
            pending_context_usage,
        });
        let websocket_start = transport::open_websocket_stream(
            self,
            body,
            token,
            transport::ChatGptWebSocketRequestContext {
                marker_mode,
                stable_client_conversation_id: stable_client_conversation_id.as_deref(),
                request_id,
                responses_lite,
            },
            on_event,
            responses_correlation,
        )
        .await
        .inspect_err(|_| {
            self.websocket_stats
                .failures
                .fetch_add(1, Ordering::Relaxed);
        })?;
        notify_request_metadata_observer(
            metadata_observer.as_ref(),
            ProviderRequestMetadata {
                transport: Some("websocket".to_string()),
                responses_lite: Some(responses_lite.is_enabled()),
                websocket_reused: Some(websocket_start.websocket_reused),
                continuation_used: Some(websocket_start.continuation_used),
                continuation_disabled_reason: Some(
                    websocket_start.continuation_disabled_reason.to_string(),
                ),
                request_body_bytes: Some(websocket_start.request_body_bytes as u64),
                upstream_send_body_bytes: Some(websocket_start.upstream_send_body_bytes as u64),
                ..ProviderRequestMetadata::default()
            },
        );
        let reused = websocket_start.websocket_reused;
        let stream = websocket_start.stream;
        self.websocket_stats
            .successes
            .fetch_add(1, Ordering::Relaxed);
        if reused {
            self.websocket_stats
                .connections_reused
                .fetch_add(1, Ordering::Relaxed);
        } else {
            self.websocket_stats
                .connections_created
                .fetch_add(1, Ordering::Relaxed);
        }
        let stats = self.websocket_stats.snapshot();
        info!(
            request_id,
            compact_request,
            selected_transport = "websocket",
            final_reasoning = %final_reasoning,
            websocket_reused = reused,
            websocket_prewarmed,
            websocket_attempts = stats.attempts,
            websocket_successes = stats.successes,
            websocket_failures = stats.failures,
            websocket_fallbacks = stats.fallbacks,
            websocket_connections_created = stats.connections_created,
            websocket_connections_reused = stats.connections_reused,
            "ChatGPT websocket stream selected"
        );
        Ok(wrap_chatgpt_stream_logging(
            stream,
            request_id,
            compact_request,
            "websocket",
            stream_started_at,
            first_upstream_event_seen,
            thinking_diagnostics,
        ))
    }

    async fn stream_sse_response(
        &self,
        response: Response,
        context: ChatGptSseStreamContext,
    ) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
        let ChatGptSseStreamContext {
            marker_mode,
            request_id,
            compact_request,
            observer,
            pending_context_usage,
            responses_correlation,
        } = context;
        let header_snapshots =
            rate_limit_snapshots_from_headers(&self.id, response.headers(), unix_timestamp_secs());
        log_rate_limit_summary(
            request_id,
            compact_request,
            "response_headers",
            &header_snapshots,
        );
        self.cache_rate_limits(header_snapshots).await;

        let first_upstream_event_seen = Arc::new(AtomicBool::new(false));
        let thinking_diagnostics = Arc::new(ChatGptThinkingDiagnostics::default());
        let stream_started_at = Instant::now();
        let on_event = self.upstream_event_handler(ChatGptUpstreamEventContext {
            request_id,
            compact_request,
            transport: "sse",
            first_upstream_event_seen: Arc::clone(&first_upstream_event_seen),
            thinking_diagnostics: Arc::clone(&thinking_diagnostics),
            stream_started_at,
            observer,
            pending_context_usage,
        });
        let stream = responses::stream_response_with_marker_mode_and_context(
            response,
            marker_mode,
            responses_correlation,
            self.payload_limits.max_sse_frame_bytes,
            on_event,
        );
        wrap_chatgpt_stream_logging(
            stream,
            request_id,
            compact_request,
            "sse",
            stream_started_at,
            first_upstream_event_seen,
            thinking_diagnostics,
        )
    }

    fn upstream_event_handler(
        &self,
        context: ChatGptUpstreamEventContext,
    ) -> impl Fn(&Value) + Send + Sync + 'static {
        let ChatGptUpstreamEventContext {
            request_id,
            compact_request,
            transport,
            first_upstream_event_seen,
            thinking_diagnostics,
            stream_started_at,
            observer,
            pending_context_usage,
        } = context;
        let state = ChatGptUpstreamEventHandlerState {
            request_id,
            compact_request,
            transport,
            first_upstream_event_seen,
            thinking_diagnostics,
            stream_started_at,
            observer,
            cache: Arc::clone(&self.cached_rate_limits),
            provider_id: self.id.clone(),
            runtime_ids: self.runtime_ids_handle(),
            websocket_sse_cooldown_until_secs: self.websocket_sse_cooldown_handle(),
            context_usage: Arc::clone(&self.context_usage),
            pending_context_usage,
        };
        move |event| state.handle(event)
    }

    async fn fetch_usage_rate_limits(&self) -> Result<Vec<RateLimitSnapshot>, ProviderError> {
        let token = self.auth.get_existing_token().await?;
        let mut request_builder = self
            .http_client
            .get(&self.usage_endpoint)
            .bearer_auth(&token.access_token)
            .header("User-Agent", self.request_headers.user_agent.clone());

        if let Some(account_id) = token.account_id.as_deref() {
            request_builder = request_builder.header("ChatGPT-Account-Id", account_id);
        }

        let request_builder = apply_runtime_request_config(request_builder, &self.runtime)?;
        let response =
            send_upstream_request_with_policy(request_builder, self.request_policy).await?;
        if !response.status().is_success() {
            return Err(map_upstream_response(response).await);
        }

        let payload = response.json::<UsagePayload>().await.map_err(|error| {
            ProviderError::UpstreamError {
                status: 200,
                body: format!("invalid ChatGPT usage response: {error}"),
            }
        })?;
        Ok(rate_limit_snapshots_from_usage_payload(
            &self.id,
            payload,
            unix_timestamp_secs(),
        ))
    }

    async fn cache_rate_limits(&self, snapshots: Vec<RateLimitSnapshot>) {
        cache_rate_limits_into(&self.cached_rate_limits, snapshots).await;
    }

    async fn rate_limit_hard_stop_generation(&self) -> u64 {
        self.cached_rate_limits.lock().await.hard_stop_generation
    }

    async fn cache_rate_limits_if_generation(
        &self,
        snapshots: Vec<RateLimitSnapshot>,
        expected_generation: u64,
    ) -> bool {
        cache_rate_limits_if_generation_into(
            &self.cached_rate_limits,
            snapshots,
            Some(expected_generation),
        )
        .await
    }

    async fn cached_rate_limits(&self) -> Vec<RateLimitSnapshot> {
        self.cached_rate_limits.lock().await.snapshots.clone()
    }

    async fn fresh_cached_rate_limits(&self) -> Option<Vec<RateLimitSnapshot>> {
        let cached = self.cached_rate_limits.lock().await;
        cached
            .fetched_at
            .filter(|fetched_at| fetched_at.elapsed() < CHATGPT_USAGE_FETCH_INTERVAL)
            .map(|_| cached.snapshots.clone())
            .filter(|snapshots| !snapshots.is_empty())
    }
}

async fn cache_rate_limits_into(
    cache: &Arc<Mutex<CachedRateLimits>>,
    snapshots: Vec<RateLimitSnapshot>,
) {
    let _ = cache_rate_limits_if_generation_into(cache, snapshots, None).await;
}

async fn cache_rate_limits_if_generation_into(
    cache: &Arc<Mutex<CachedRateLimits>>,
    snapshots: Vec<RateLimitSnapshot>,
    expected_generation: Option<u64>,
) -> bool {
    if snapshots.is_empty() {
        return true;
    }

    let mut cached = cache.lock().await;
    if expected_generation.is_some_and(|expected| expected != cached.hard_stop_generation) {
        return false;
    }
    let mut observed_hard_stop = false;
    for snapshot in snapshots {
        if let Some(existing) = cached.snapshots.iter_mut().find(|existing| {
            rate_limit_snapshot_key(existing) == rate_limit_snapshot_key(&snapshot)
        }) {
            let merged = merge_rate_limit_snapshot(existing.clone(), snapshot);
            observed_hard_stop |= rate_limit_snapshot_is_workspace_hard_stop(&merged);
            *existing = merged;
        } else {
            observed_hard_stop |= rate_limit_snapshot_is_workspace_hard_stop(&snapshot);
            cached.snapshots.push(snapshot);
        }
    }
    if observed_hard_stop {
        cached.hard_stop_generation = cached.hard_stop_generation.wrapping_add(1);
    }
    cached.fetched_at = Some(Instant::now());
    true
}

fn rate_limit_snapshot_key(snapshot: &RateLimitSnapshot) -> String {
    snapshot
        .feature
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or("codex")
        .to_string()
}

fn rate_limit_snapshot_is_workspace_hard_stop(snapshot: &RateLimitSnapshot) -> bool {
    if snapshot.spend_control_reached == Some(true) {
        return true;
    }
    matches!(
        snapshot.rate_limit_reached_type.as_deref(),
        Some(
            "workspace_owner_credits_depleted"
                | "workspace_member_credits_depleted"
                | "workspace_owner_usage_limit_reached"
                | "workspace_member_usage_limit_reached"
        )
    )
}

fn merge_rate_limit_snapshot(
    previous: RateLimitSnapshot,
    update: RateLimitSnapshot,
) -> RateLimitSnapshot {
    RateLimitSnapshot {
        provider_id: update.provider_id,
        feature: update.feature.or(previous.feature),
        limit_name: update.limit_name.or(previous.limit_name),
        primary: update.primary.or(previous.primary),
        secondary: update.secondary.or(previous.secondary),
        credits: update.credits.or(previous.credits),
        spend_control_reached: update
            .spend_control_reached
            .or(previous.spend_control_reached),
        plan_type: update.plan_type.or(previous.plan_type),
        rate_limit_reached_type: update
            .rate_limit_reached_type
            .or(previous.rate_limit_reached_type),
        source: update.source,
        updated_at_unix_secs: update.updated_at_unix_secs,
    }
}

fn header_value_from_any(headers: &HeaderMap, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

fn upstream_request_id_from_headers(headers: &HeaderMap) -> Option<String> {
    header_value_from_any(
        headers,
        &[
            "x-request-id",
            "x-openai-request-id",
            "openai-request-id",
            "cf-ray",
        ],
    )
}

fn upstream_model_from_headers(headers: &HeaderMap) -> Option<String> {
    header_value_from_any(
        headers,
        &["openai-model", "x-openai-model", "x-model", "model"],
    )
}

fn chatgpt_reasoning_log_value(body: &Value) -> String {
    body.get("reasoning")
        .map(Value::to_string)
        .unwrap_or_else(|| "null".to_string())
}

fn chatgpt_output_token_budget(
    request: &MessagesRequest,
    body: &Value,
) -> ChatGptOutputTokenBudget {
    ChatGptOutputTokenBudget {
        requested: request.max_tokens.map(u64::from),
        effective: body.get("max_output_tokens").and_then(Value::as_u64),
    }
}

fn log_rate_limit_summary(
    request_id: u64,
    compact_request: bool,
    source: &str,
    snapshots: &[RateLimitSnapshot],
) {
    if snapshots.is_empty() {
        return;
    }
    let summary = rate_limit_summary(snapshots);
    info!(
        request_id,
        compact_request,
        source,
        rate_limit_summary = %summary,
        "ChatGPT upstream rate-limit summary observed"
    );
}

fn rate_limit_summary(snapshots: &[RateLimitSnapshot]) -> String {
    snapshots
        .iter()
        .map(rate_limit_snapshot_summary)
        .collect::<Vec<_>>()
        .join(";")
}

fn rate_limit_snapshot_summary(snapshot: &RateLimitSnapshot) -> String {
    let label = snapshot
        .limit_name
        .as_deref()
        .or(snapshot.feature.as_deref())
        .filter(|value| !value.is_empty())
        .unwrap_or("codex");
    let mut parts = Vec::new();
    if let Some(plan_type) = snapshot.plan_type.as_deref() {
        parts.push(format!("plan={plan_type}"));
    }
    if let Some(primary) = snapshot.primary.as_ref() {
        parts.push(format_rate_limit_window_summary("primary", primary));
    }
    if let Some(secondary) = snapshot.secondary.as_ref() {
        parts.push(format_rate_limit_window_summary("secondary", secondary));
    }
    if let Some(credits) = snapshot.credits.as_ref()
        && let Some(balance) = credits.balance.as_deref()
    {
        parts.push(format!("credits={balance}"));
    }
    if let Some(kind) = snapshot.rate_limit_reached_type.as_deref() {
        parts.push(format!("reached={kind}"));
    }
    if let Some(reached) = snapshot.spend_control_reached {
        parts.push(format!("spend_control_reached={reached}"));
    }

    if parts.is_empty() {
        label.to_string()
    } else {
        format!("{label}:{}", parts.join(","))
    }
}

fn format_rate_limit_window_summary(label: &str, window: &RateLimitWindow) -> String {
    let mut summary = format!("{label}={:.1}%", window.used_percent);
    if let Some(minutes) = window.window_minutes {
        summary.push_str(&format!("/{minutes}m"));
    }
    summary
}

fn chatgpt_sse_stop_reason(event: &Value) -> Option<&'static str> {
    match event.get("type").and_then(Value::as_str)? {
        "response.completed" | "response.incomplete" | "response.failed" => {}
        _ => return None,
    }
    let response = event.get("response").unwrap_or(event);
    if let Some(reason) = response["incomplete_details"]["reason"].as_str() {
        return Some(match reason {
            "max_output_tokens" => "max_tokens",
            "content_filter" | "content_policy_violation" => "refusal",
            _ => "end_turn",
        });
    }
    if response["status"].as_str() == Some("failed") {
        return Some("error");
    }
    if response["output"].as_array().is_some_and(|items| {
        items.iter().any(|item| {
            matches!(
                item["type"].as_str(),
                Some("function_call" | "custom_tool_call")
            )
        })
    }) {
        Some("tool_use")
    } else {
        Some("end_turn")
    }
}

fn chatgpt_sse_response_status(event: &Value) -> Option<&str> {
    event
        .get("response")
        .unwrap_or(event)
        .get("status")
        .and_then(Value::as_str)
}

fn chatgpt_sse_error_code(event: &Value) -> Option<&str> {
    let response = event.get("response").unwrap_or(event);
    response["error"]["code"]
        .as_str()
        .or_else(|| response["error"]["type"].as_str())
}

fn chatgpt_sse_error_message(event: &Value) -> Option<&str> {
    event.get("response").unwrap_or(event)["error"]["message"].as_str()
}

pub(super) fn chatgpt_event_is_server_error(event: &Value) -> bool {
    chatgpt_sse_error_code(event) == Some("server_error")
}

pub(super) fn provider_error_is_chatgpt_server_error(error: &ProviderError) -> bool {
    let ProviderError::UpstreamError { body, .. } = error.without_upstream_metadata() else {
        return false;
    };
    serde_json::from_str::<Value>(body)
        .ok()
        .is_some_and(|event| chatgpt_event_is_server_error(&event))
}

pub(super) fn rotate_chatgpt_runtime_ids_after_server_error(
    runtime_ids: &Arc<RwLock<ChatGptRuntimeIds>>,
    request_id: u64,
    transport: &'static str,
) {
    let mut ids = runtime_ids
        .write()
        .expect("ChatGPT runtime ids lock poisoned");
    let previous_session_id = ids.session_id.clone();
    let previous_thread_id = ids.thread_id.clone();
    let previous_window_id = ids.window_id.clone();
    *ids = ChatGptRuntimeIds::new();
    warn!(
        request_id,
        transport,
        previous_session_id,
        previous_thread_id,
        previous_window_id,
        new_session_id = %ids.session_id,
        new_thread_id = %ids.thread_id,
        new_window_id = %ids.window_id,
        "ChatGPT runtime ids rotated after upstream server_error"
    );
}

fn chatgpt_sse_model(event: &Value) -> Option<&str> {
    event
        .get("response")
        .unwrap_or(event)
        .get("model")
        .and_then(Value::as_str)
}

fn chatgpt_sse_response_id(event: &Value) -> Option<&str> {
    event
        .get("response")
        .unwrap_or(event)
        .get("id")
        .and_then(Value::as_str)
}

fn effective_codex_service_tier(
    runtime_service_tier: Option<&str>,
    fast_mode: bool,
) -> Option<&str> {
    runtime_service_tier.or(fast_mode.then_some(CODEX_FAST_SERVICE_TIER))
}

#[async_trait]
impl Provider for ChatGptProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn chat(
        &self,
        request: MessagesRequest,
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        self.chat_with_observer(request, None).await
    }

    async fn chat_with_observer(
        &self,
        mut request: MessagesRequest,
        observer: Option<ProviderRequestObserver>,
    ) -> Result<BoxStream<'static, Result<ProviderEvent, ProviderError>>, ProviderError> {
        let token = self.auth.get_existing_token().await?;
        let virtual_context_requested = request
            .extra
            .remove(VIRTUAL_CONTEXT_1M_EXTRA_KEY)
            .and_then(|value| value.as_bool())
            .unwrap_or(false);
        let mut request = apply_openai_intent(request);
        let requested_ultra = normalize_chatgpt_56_reasoning(&mut request);
        let model_info = self.model_info(&request.model);
        let model_context = model_info
            .as_ref()
            .filter(|model| model.model_id.starts_with("gpt-5.6"));
        let ultra_supported = model_info.as_ref().is_some_and(|model| {
            model
                .capabilities
                .limits
                .reasoning_effort_levels
                .iter()
                .any(|effort| effort == "ultra")
        });
        let proactive_multi_agent = if requested_ultra
            && ultra_supported
            && request_has_delegation_tool(&request)
        {
            Some(PROACTIVE_MULTI_AGENT_INSTRUCTIONS)
        } else {
            if requested_ultra && !ultra_supported {
                warn!(
                    model = %request.model,
                    "ChatGPT ultra effort is unavailable for this model; sending max effort"
                );
            } else if requested_ultra {
                warn!(
                    model = %request.model,
                    "ChatGPT ultra effort requested without a delegation tool; sending max effort without proactive multi-agent instructions"
                );
            }
            None
        };
        let (request, synthetic_stable_client_conversation_id) =
            ensure_chatgpt_stable_client_conversation_id(request);
        let marker_mode = marker_mode_from_request(&request);
        let prompt_cache_key_source = responses::prompt_cache_key_source(&request);
        let stable_client_conversation_id =
            responses::stable_client_conversation_id_for_continuation(&request);
        let responses_lite = self.responses_lite_decision(&request.model);
        let supports_reasoning_summary_parameter =
            self.supports_reasoning_summary_parameter(&request.model);
        let supports_parallel_tool_calls = self.supports_parallel_tool_calls(&request.model);
        let runtime_ids = self.runtime_ids_snapshot();
        let turn_id = chatgpt_runtime_id();
        let body = responses::build_body_with_context(
            &request,
            DEFAULT_CHATGPT_INSTRUCTIONS,
            responses::CodexRequestContext {
                installation_id: Some(&self.installation_id),
                session_id: Some(&runtime_ids.session_id),
                thread_id: Some(&runtime_ids.thread_id),
                turn_id: Some(&turn_id),
                window_id: Some(&runtime_ids.window_id),
                service_tier: self.codex_service_tier(&request.model),
                standalone_tools: self.chatgpt_config.standalone_tools,
                responses_lite: responses_lite.is_enabled(),
                model: model_context,
                additional_instructions: proactive_multi_agent,
                supports_reasoning_summary_parameter,
                supports_parallel_tool_calls,
            },
        )?;
        let request_id = next_chatgpt_request_id();
        validate_chatgpt_tool_schema_budget(&body)?;
        let output_token_budget = chatgpt_output_token_budget(&request, &body);
        info!(
            request_id,
            prompt_cache_key_source = prompt_cache_key_source.as_str(),
            prompt_cache_key_present = body.get("prompt_cache_key").is_some(),
            stable_client_conversation_id_present = stable_client_conversation_id.is_some(),
            synthetic_stable_client_conversation_id,
            responses_lite = responses_lite.is_enabled(),
            responses_lite_source = responses_lite.source_str(),
            "ChatGPT prompt cache key policy applied"
        );
        notify_request_metadata_observer(
            observer.as_ref(),
            ProviderRequestMetadata {
                prompt_cache_key_present: Some(body.get("prompt_cache_key").is_some()),
                prompt_cache_key_source: Some(prompt_cache_key_source.as_str().to_string()),
                stable_client_conversation_id_present: Some(
                    stable_client_conversation_id.is_some(),
                ),
                synthetic_stable_client_conversation_id: Some(
                    synthetic_stable_client_conversation_id,
                ),
                ..ProviderRequestMetadata::default()
            },
        );
        log_request_observability("chatgpt", "/responses", &body, Some(request_id));
        let compact_kind = classify_compact_request_body(&body);
        let compact_request = compact_kind != CompactRequestKind::None;
        log_compact_request_observability("chatgpt", "/responses", &body, compact_request);

        let virtual_context = virtual_context_requested
            && model_info
                .as_ref()
                .and_then(|model| model.capabilities.limits.context_window)
                .is_some_and(|window| window > CLAUDE_CODE_DEFAULT_CONTEXT_WINDOW);
        let context_estimate = virtual_context.then(|| {
            self.virtual_context_estimate(
                &body,
                &token,
                stable_client_conversation_id.as_deref(),
                model_info
                    .as_ref()
                    .and_then(|model| model.capabilities.limits.context_window)
                    .expect("virtual context requires a known context window"),
                compact_kind,
            )
        });
        notify_request_metadata_observer(
            observer.as_ref(),
            ProviderRequestMetadata {
                virtual_context_1m: Some(virtual_context),
                context_estimated_tokens: context_estimate
                    .as_ref()
                    .map(|estimate| estimate.estimated_tokens),
                context_safe_input_limit: context_estimate
                    .as_ref()
                    .map(|estimate| u64::from(estimate.safe_input_limit)),
                context_model_window: context_estimate
                    .as_ref()
                    .map(|estimate| u64::from(estimate.model_context_window)),
                context_estimator_source: context_estimate
                    .as_ref()
                    .map(|estimate| estimate.estimator_source.as_str().to_string()),
                context_compact_kind: context_estimate
                    .as_ref()
                    .map(|estimate| estimate.compact_kind.as_str().to_string()),
                context_compressible_history: context_estimate
                    .as_ref()
                    .map(|estimate| estimate.compressible_history),
                context_local_blocked: context_estimate.as_ref().map(|estimate| {
                    estimate.estimated_tokens > u64::from(estimate.safe_input_limit)
                }),
                ..ProviderRequestMetadata::default()
            },
        );
        if let Some(estimate) = context_estimate.as_ref() {
            info!(
                request_id,
                model = %request.model,
                virtual_context_1m = true,
                estimated_tokens = estimate.estimated_tokens,
                safe_input_limit = estimate.safe_input_limit,
                model_context_window = estimate.model_context_window,
                estimator_source = estimate.estimator_source.as_str(),
                compact_kind = ?estimate.compact_kind,
                compressible_history = estimate.compressible_history,
                "ChatGPT virtual context preflight evaluated"
            );
            if let Some(error) = virtual_context_limit_error(estimate) {
                warn!(
                    request_id,
                    model = %request.model,
                    estimated_tokens = estimate.estimated_tokens,
                    safe_input_limit = estimate.safe_input_limit,
                    model_context_window = estimate.model_context_window,
                    estimator_source = estimate.estimator_source.as_str(),
                    compact_kind = ?estimate.compact_kind,
                    compressible_history = estimate.compressible_history,
                    "ChatGPT virtual context request blocked before upstream send"
                );
                return Err(error);
            }
        }

        let stream = self
            .chat_prepared_with_token(
                ChatGptPreparedRequest {
                    body,
                    responses_correlation: crate::responses::ResponsesCorrelation::from_request(
                        &request,
                    ),
                    marker_mode,
                    compact_request,
                    request_id,
                    output_token_budget,
                    stable_client_conversation_id,
                    responses_lite,
                    observer,
                    pending_context_usage: context_estimate
                        .and_then(|estimate| estimate.pending_usage),
                },
                token,
            )
            .await?;
        Ok(stream)
    }

    async fn responses(
        &self,
        request: NativeResponsesRequest,
        observer: Option<ProviderRequestObserver>,
    ) -> Result<NativeResponsesResponse, ProviderError> {
        let NativeResponsesRequest {
            mut body,
            headers: forwarded_headers,
        } = request;
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .ok_or_else(|| ProviderError::InvalidRequest("model is required".to_string()))?
            .to_string();
        if body.get("service_tier").is_none()
            && let Some(service_tier) = self.codex_service_tier(&model)
            && let Some(object) = body.as_object_mut()
        {
            object.insert(
                "service_tier".to_string(),
                Value::String(service_tier.to_string()),
            );
        }
        let responses_lite = self.responses_lite_decision(&model);
        let output_token_budget =
            normalize_chatgpt_native_responses_body(&mut body, responses_lite.is_enabled());
        normalize_chatgpt_native_56_reasoning(&mut body, &model);
        let compact_request = classify_compact_request_body(&body) != CompactRequestKind::None;
        let request_id = next_chatgpt_request_id();
        validate_chatgpt_tool_schema_budget(&body)?;
        log_request_observability("chatgpt", "/responses", &body, Some(request_id));
        log_compact_request_observability("chatgpt", "/responses", &body, compact_request);

        let context = ChatGptSseRequestContext {
            compact_request,
            request_id,
            budget: output_token_budget,
            responses_lite,
        };
        let mut token = self.auth.get_existing_token().await?;
        let mut response = self
            .send_responses_request_with_prompt_too_long_retry_and_headers(
                &mut body,
                &token,
                context,
                observer.as_ref(),
                Some(&forwarded_headers),
            )
            .await?;
        if response.status() == StatusCode::UNAUTHORIZED {
            token = self.auth.force_refresh_token().await?;
            response = self
                .send_responses_request_with_prompt_too_long_retry_and_headers(
                    &mut body,
                    &token,
                    context,
                    observer.as_ref(),
                    Some(&forwarded_headers),
                )
                .await?;
            if response.status() == StatusCode::UNAUTHORIZED {
                self.auth.clear_token().await;
            }
        }
        if !response.status().is_success() {
            return Err(map_upstream_response(response).await);
        }

        let headers = response.headers().clone();
        let header_snapshots =
            rate_limit_snapshots_from_headers(&self.id, &headers, unix_timestamp_secs());
        self.cache_rate_limits(header_snapshots).await;
        let stream = crate::responses::stream_native_responses_response_with_provider_observer(
            response,
            observer,
            self.payload_limits.max_sse_frame_bytes,
        );
        Ok(NativeResponsesResponse { headers, stream })
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        match self.fetch_remote_models().await {
            Ok(models) => Ok(models),
            Err(error) => {
                warn!(
                    provider = %self.id,
                    endpoint = %self.models_endpoint,
                    %error,
                    "ChatGPT online model catalog unavailable; using built-in fallback"
                );
                Ok(chatgpt_models(&self.chatgpt_config))
            }
        }
    }

    async fn rate_limit_snapshots(&self) -> Result<Vec<RateLimitSnapshot>, ProviderError> {
        if let Some(snapshots) = self.fresh_cached_rate_limits().await {
            return Ok(snapshots);
        }

        let hard_stop_generation = self.rate_limit_hard_stop_generation().await;
        match self.fetch_usage_rate_limits().await {
            Ok(snapshots) => {
                let _ = self
                    .cache_rate_limits_if_generation(snapshots, hard_stop_generation)
                    .await;
                Ok(self.cached_rate_limits().await)
            }
            Err(error) if error.is_authentication() => Ok(self.cached_rate_limits().await),
            Err(error) => {
                let cached = self.cached_rate_limits().await;
                if cached.is_empty() {
                    Err(error)
                } else {
                    Ok(cached)
                }
            }
        }
    }
}

#[derive(Default)]
struct ChatGptThinkingDiagnostics {
    upstream_reasoning_delta_events: AtomicU64,
    upstream_reasoning_delta_bytes: AtomicU64,
    downstream_thinking_delta_events: AtomicU64,
    downstream_thinking_delta_bytes: AtomicU64,
    first_upstream_reasoning_logged: AtomicBool,
    first_downstream_thinking_logged: AtomicBool,
    summary_logged: AtomicBool,
}

fn wrap_chatgpt_stream_logging(
    stream: BoxStream<'static, Result<ProviderEvent, ProviderError>>,
    request_id: u64,
    compact_request: bool,
    transport: &'static str,
    stream_started_at: Instant,
    first_upstream_event_seen: Arc<AtomicBool>,
    thinking_diagnostics: Arc<ChatGptThinkingDiagnostics>,
) -> BoxStream<'static, Result<ProviderEvent, ProviderError>> {
    let first_stream_item_seen = Arc::new(AtomicBool::new(false));
    let first_stream_item_seen_for_map = Arc::clone(&first_stream_item_seen);
    Box::pin(stream.map(move |result| {
        let result = result.map_err(map_chatgpt_stream_error);
        if let Ok(event) = &result {
            if let Some(delta_bytes) = downstream_thinking_delta_len(event) {
                let count = thinking_diagnostics
                    .downstream_thinking_delta_events
                    .fetch_add(1, Ordering::Relaxed)
                    + 1;
                thinking_diagnostics
                    .downstream_thinking_delta_bytes
                    .fetch_add(delta_bytes as u64, Ordering::Relaxed);
                if !thinking_diagnostics
                    .first_downstream_thinking_logged
                    .swap(true, Ordering::Relaxed)
                {
                    info!(
                        request_id,
                        compact_request,
                        transport,
                        elapsed_ms = elapsed_millis(stream_started_at),
                        downstream_thinking_delta_events = count,
                        downstream_thinking_delta_bytes = delta_bytes,
                        upstream_reasoning_delta_events = thinking_diagnostics
                            .upstream_reasoning_delta_events
                            .load(Ordering::Relaxed),
                        upstream_reasoning_delta_bytes = thinking_diagnostics
                            .upstream_reasoning_delta_bytes
                            .load(Ordering::Relaxed),
                        "ChatGPT downstream thinking delta emitted"
                    );
                }
            }
            if sse_event_finishes_message(event) {
                log_chatgpt_thinking_diagnostics(
                    &thinking_diagnostics,
                    request_id,
                    compact_request,
                    transport,
                    stream_started_at,
                    "message_stop",
                );
            }
        }
        if !first_stream_item_seen_for_map.swap(true, Ordering::Relaxed) {
            match &result {
                Ok(event) => {
                    info!(
                        request_id,
                        compact_request,
                        transport,
                        elapsed_ms = elapsed_millis(stream_started_at),
                        event = %provider_event_type(event),
                        "ChatGPT first downstream stream item emitted"
                    );
                }
                Err(error) => {
                    warn!(
                        request_id,
                        compact_request,
                        transport,
                        elapsed_ms = elapsed_millis(stream_started_at),
                        error = %error,
                        first_upstream_event_seen = first_upstream_event_seen.load(Ordering::Relaxed),
                        "ChatGPT stream failed before first downstream item"
                    );
                    log_chatgpt_thinking_diagnostics(
                        &thinking_diagnostics,
                        request_id,
                        compact_request,
                        transport,
                        stream_started_at,
                        "stream_error_before_first_item",
                    );
                }
            }
        }
        if result.is_err() {
            log_chatgpt_thinking_diagnostics(
                &thinking_diagnostics,
                request_id,
                compact_request,
                transport,
                stream_started_at,
                "stream_error",
            );
        }
        result
    }))
}

fn map_chatgpt_stream_error(error: ProviderError) -> ProviderError {
    match error {
        ProviderError::UpstreamError { status, body } => {
            let status_code =
                StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
            map_chatgpt_error_status_body_with_headers(status_code, &HeaderMap::new(), body)
        }
        other => other,
    }
}

fn log_chatgpt_thinking_diagnostics(
    diagnostics: &ChatGptThinkingDiagnostics,
    request_id: u64,
    compact_request: bool,
    transport: &'static str,
    stream_started_at: Instant,
    terminal_reason: &'static str,
) {
    if diagnostics.summary_logged.swap(true, Ordering::Relaxed) {
        return;
    }
    info!(
        request_id,
        compact_request,
        transport,
        terminal_reason,
        elapsed_ms = elapsed_millis(stream_started_at),
        upstream_reasoning_delta_events = diagnostics
            .upstream_reasoning_delta_events
            .load(Ordering::Relaxed),
        upstream_reasoning_delta_bytes = diagnostics
            .upstream_reasoning_delta_bytes
            .load(Ordering::Relaxed),
        downstream_thinking_delta_events = diagnostics
            .downstream_thinking_delta_events
            .load(Ordering::Relaxed),
        downstream_thinking_delta_bytes = diagnostics
            .downstream_thinking_delta_bytes
            .load(Ordering::Relaxed),
        "ChatGPT thinking stream diagnostics"
    );
}

fn is_chatgpt_reasoning_delta_event(event_type: &str) -> bool {
    matches!(
        event_type,
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta"
    )
}

fn chatgpt_sse_delta_len(event: &Value) -> usize {
    event
        .get("delta")
        .and_then(Value::as_str)
        .map_or(0, str::len)
}

fn downstream_thinking_delta_len(event: &ProviderEvent) -> Option<usize> {
    event.normalized_events().iter().find_map(|event| {
        (event.event == "content_block_delta"
            && event.data["delta"]["type"].as_str() == Some("thinking_delta"))
        .then(|| event.data["delta"]["thinking"].as_str().map_or(0, str::len))
    })
}

fn sse_event_finishes_message(event: &ProviderEvent) -> bool {
    event.normalized_events().iter().any(|event| {
        event.event == "message_stop" || event.data["type"].as_str() == Some("message_stop")
    })
}

fn provider_event_type(event: &ProviderEvent) -> &str {
    if let Some(crate::provider::NativeProviderEvent::OpenAiResponses(event)) = event.native_event()
    {
        return event
            .data
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
    }
    event
        .normalized_events()
        .last()
        .map(|event| event.event.as_str())
        .unwrap_or("unknown")
}

fn chatgpt_request_headers(
    config: &ChatGptProviderConfig,
) -> Result<ChatGptRequestHeaders, ProviderError> {
    Ok(ChatGptRequestHeaders {
        originator: chatgpt_header_value(
            "originator",
            &config.originator,
            DEFAULT_CHATGPT_ORIGINATOR,
        )?,
        user_agent: chatgpt_header_value(
            "User-Agent",
            &config.user_agent,
            default_chatgpt_user_agent(),
        )?,
    })
}

fn chatgpt_header_value(
    header_name: &str,
    configured_value: &str,
    default_value: &str,
) -> Result<HeaderValue, ProviderError> {
    let value = configured_value.trim();
    let value = if value.is_empty() {
        default_value
    } else {
        value
    };

    HeaderValue::from_str(value).map_err(|error| {
        ProviderError::InvalidRequest(format!(
            "invalid ChatGPT {header_name} header value: {error}"
        ))
    })
}

fn default_chatgpt_user_agent() -> &'static str {
    static DEFAULT_USER_AGENT: OnceLock<String> = OnceLock::new();
    DEFAULT_USER_AGENT
        .get_or_init(resolve_default_chatgpt_user_agent)
        .as_str()
}

fn resolve_default_chatgpt_user_agent() -> String {
    local_codex_cli_version()
        .map(|version| format!("codex_cli_rs/{version} (claude-proxy)"))
        .unwrap_or_else(|| DEFAULT_CHATGPT_USER_AGENT.to_string())
}

#[cfg(not(test))]
fn local_codex_cli_version() -> Option<String> {
    let output = Command::new("codex").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    parse_codex_cli_version(stdout.as_ref()).or_else(|| parse_codex_cli_version(stderr.as_ref()))
}

#[cfg(test)]
fn local_codex_cli_version() -> Option<String> {
    None
}

fn parse_codex_cli_version(output: &str) -> Option<String> {
    output.split_whitespace().find_map(|token| {
        let token = token.trim_matches(|ch: char| matches!(ch, '(' | ')' | ',' | ';'));
        let token = token.strip_prefix('v').unwrap_or(token);
        is_version_token(token).then(|| token.to_string())
    })
}

fn is_version_token(token: &str) -> bool {
    token.contains('.')
        && token.chars().next().is_some_and(|ch| ch.is_ascii_digit())
        && token
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '+'))
}

fn build_http_client(proxy: &str, settings: &Settings) -> Result<Client, ProviderError> {
    let mut builder = Client::builder()
        .connect_timeout(Duration::from_secs(settings.http.connect_timeout))
        .read_timeout(Duration::from_secs(settings.http.read_timeout));

    if !proxy.is_empty() {
        builder = builder.proxy(
            reqwest::Proxy::all(proxy)
                .map_err(|e| ProviderError::Network(format!("invalid proxy: {e}")))?,
        );
    }

    builder = apply_extra_ca_certs(builder, &settings.http.extra_ca_certs)?;

    builder.build().map_err(|e| {
        ProviderError::Network(format!(
            "failed to build HTTP client: {}",
            fmt_reqwest_err(&e)
        ))
    })
}

fn chatgpt_upstream_request_policy(runtime: &ProviderRuntimeConfig) -> UpstreamRequestPolicy {
    UpstreamRequestPolicy {
        max_attempts: CHATGPT_SEND_MAX_ATTEMPTS,
        attempt_timeout: None,
        connection_max_attempts: CHATGPT_CONNECTION_MAX_ATTEMPTS,
        connection_base_retry_delay: CHATGPT_CONNECTION_BASE_RETRY_DELAY,
        connection_max_retry_delay: CHATGPT_CONNECTION_MAX_RETRY_DELAY,
        retry_rate_limits: false,
        ..UpstreamRequestPolicy::default()
    }
    .with_runtime_config(runtime)
}

fn codex_responses_endpoint(base_url: &str) -> String {
    let base = normalized_codex_base_url(base_url);

    if base.ends_with("/responses") {
        base
    } else {
        format!("{base}/responses")
    }
}

fn codex_models_endpoint(base_url: &str) -> String {
    let base = normalized_codex_base_url(base_url);
    let base = base.strip_suffix("/responses").unwrap_or(&base);
    format!("{base}/models")
}

fn codex_usage_endpoint(base_url: &str) -> String {
    let base = normalized_codex_base_url(base_url);
    let base = base.strip_suffix("/responses").unwrap_or(&base);

    if base.ends_with("/api/codex") {
        format!("{base}/usage")
    } else if let Some(chatgpt_base) = base.strip_suffix("/codex") {
        format!("{chatgpt_base}/wham/usage")
    } else {
        format!("{base}/wham/usage")
    }
}

fn normalized_codex_base_url(base_url: &str) -> String {
    let mut base = if base_url.trim().is_empty() {
        DEFAULT_CODEX_BASE_URL.to_string()
    } else {
        base_url.trim().trim_end_matches('/').to_string()
    };

    if (base.starts_with("https://chatgpt.com") || base.starts_with("https://chat.openai.com"))
        && !base.contains("/backend-api")
        && !base.contains("/api/codex")
    {
        base.push_str("/backend-api");
    }

    if base.ends_with("/backend-api") {
        base.push_str("/codex");
    }

    base
}

fn chatgpt_installation_id() -> String {
    let id = chatgpt_runtime_id();
    let Some(path) = Settings::config_dir().map(|dir| dir.join("chatgpt").join("installation_id"))
    else {
        return id;
    };

    if let Ok(existing) = fs::read_to_string(&path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return existing.to_string();
        }
    }

    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(path, &id);
    id
}

pub(super) fn chatgpt_runtime_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn rate_limit_snapshots_from_usage_payload(
    provider_id: &str,
    payload: UsagePayload,
    updated_at_unix_secs: u64,
) -> Vec<RateLimitSnapshot> {
    let plan_type = payload.plan_type;
    let reached_type = payload.rate_limit_reached_type.and_then(|value| value.kind);
    let spend_control_reached = payload
        .spend_control
        .and_then(|spend_control| spend_control.reached)
        .or(payload.spend_control_reached);
    let mut snapshots = vec![RateLimitSnapshot {
        provider_id: provider_id.to_string(),
        feature: Some("codex".to_string()),
        limit_name: None,
        primary: payload
            .rate_limit
            .as_ref()
            .and_then(|rate_limit| rate_limit.primary.as_ref())
            .map(rate_limit_window_from_payload),
        secondary: payload
            .rate_limit
            .as_ref()
            .and_then(|rate_limit| rate_limit.secondary.as_ref())
            .map(rate_limit_window_from_payload),
        credits: payload.credits.as_ref().map(credits_from_payload),
        spend_control_reached,
        plan_type: plan_type.clone(),
        rate_limit_reached_type: reached_type,
        source: RateLimitSource::UsageEndpoint,
        updated_at_unix_secs,
    }];

    snapshots.extend(
        payload
            .additional_rate_limits
            .unwrap_or_default()
            .into_iter()
            .map(|additional| RateLimitSnapshot {
                provider_id: provider_id.to_string(),
                feature: Some(additional.metered_feature),
                limit_name: additional.limit_name,
                primary: additional
                    .rate_limit
                    .as_ref()
                    .and_then(|rate_limit| rate_limit.primary.as_ref())
                    .map(rate_limit_window_from_payload),
                secondary: additional
                    .rate_limit
                    .as_ref()
                    .and_then(|rate_limit| rate_limit.secondary.as_ref())
                    .map(rate_limit_window_from_payload),
                credits: None,
                spend_control_reached: None,
                plan_type: plan_type.clone(),
                rate_limit_reached_type: None,
                source: RateLimitSource::UsageEndpoint,
                updated_at_unix_secs,
            }),
    );
    snapshots
        .into_iter()
        .filter(has_rate_limit_snapshot_data)
        .collect()
}

fn rate_limit_window_from_payload(payload: &RateLimitBucketPayload) -> RateLimitWindow {
    RateLimitWindow {
        used_percent: payload.used_percent,
        window_minutes: payload.window_minutes.or_else(|| {
            payload
                .limit_window_seconds
                .map(window_minutes_from_seconds)
        }),
        reset_at_unix_secs: payload
            .reset_at
            .as_ref()
            .or(payload.resets_at.as_ref())
            .and_then(parse_timestamp_value),
    }
}

fn credits_from_payload(payload: &CreditsPayload) -> RateLimitCredits {
    RateLimitCredits {
        has_credits: payload.has_credits,
        unlimited: payload.unlimited,
        balance: payload.balance.as_ref().and_then(balance_value_to_string),
    }
}

fn rate_limit_snapshot_from_sse_event(
    provider_id: &str,
    event: &Value,
    updated_at_unix_secs: u64,
) -> Option<RateLimitSnapshot> {
    if event.get("type").and_then(Value::as_str) != Some("codex.rate_limits") {
        return None;
    }

    let rate_limits = event
        .get("rate_limits")
        .cloned()
        .and_then(|value| serde_json::from_value::<RateLimitWindowPayload>(value).ok());
    let credits = event
        .get("credits")
        .cloned()
        .and_then(|value| serde_json::from_value::<CreditsPayload>(value).ok());
    let feature = event
        .get("metered_limit_name")
        .or_else(|| event.get("limit_name"))
        .and_then(Value::as_str)
        .map(normalize_limit_id)
        .unwrap_or_else(|| "codex".to_string());
    let spend_control_reached = event
        .get("spend_control_reached")
        .or_else(|| event.get("spendControlReached"))
        .and_then(Value::as_bool)
        .or_else(|| {
            event
                .get("spend_control")
                .or_else(|| event.get("spendControl"))
                .and_then(|value| value.get("reached"))
                .and_then(Value::as_bool)
        });
    let rate_limit_reached_type = event
        .get("rate_limit_reached_type")
        .or_else(|| event.get("rateLimitReachedType"))
        .and_then(|value| {
            value
                .as_str()
                .or_else(|| value.get("type").and_then(Value::as_str))
        })
        .map(str::to_string);

    Some(RateLimitSnapshot {
        provider_id: provider_id.to_string(),
        feature: Some(feature),
        limit_name: event
            .get("limit_name")
            .and_then(Value::as_str)
            .map(str::to_string),
        primary: rate_limits
            .as_ref()
            .and_then(|rate_limit| rate_limit.primary.as_ref())
            .map(rate_limit_window_from_payload),
        secondary: rate_limits
            .as_ref()
            .and_then(|rate_limit| rate_limit.secondary.as_ref())
            .map(rate_limit_window_from_payload),
        credits: credits.as_ref().map(credits_from_payload),
        spend_control_reached,
        plan_type: event
            .get("plan_type")
            .and_then(Value::as_str)
            .map(str::to_string),
        rate_limit_reached_type,
        source: RateLimitSource::StreamEvent,
        updated_at_unix_secs,
    })
}

fn rate_limit_snapshots_from_headers(
    provider_id: &str,
    headers: &HeaderMap,
    updated_at_unix_secs: u64,
) -> Vec<RateLimitSnapshot> {
    let mut limit_ids = BTreeSet::from(["codex".to_string()]);
    for name in headers.keys() {
        if let Some(limit_id) = header_limit_id(name.as_str()) {
            limit_ids.insert(limit_id);
        }
    }

    limit_ids
        .into_iter()
        .filter_map(|limit_id| {
            let prefix = format!("x-{limit_id}");
            let feature = normalize_limit_id(&limit_id);
            let snapshot = RateLimitSnapshot {
                provider_id: provider_id.to_string(),
                feature: Some(feature),
                limit_name: header_string(headers, &format!("{prefix}-limit-name")),
                primary: rate_limit_window_from_headers(headers, &prefix, "primary"),
                secondary: rate_limit_window_from_headers(headers, &prefix, "secondary"),
                credits: credits_from_headers(headers, &prefix),
                spend_control_reached: None,
                plan_type: None,
                rate_limit_reached_type: None,
                source: RateLimitSource::ResponseHeaders,
                updated_at_unix_secs,
            };
            has_rate_limit_snapshot_data(&snapshot).then_some(snapshot)
        })
        .collect()
}

fn header_limit_id(name: &str) -> Option<String> {
    let name = name.to_ascii_lowercase();
    let rest = name.strip_prefix("x-")?;
    for marker in ["-primary-", "-secondary-", "-limit-name"] {
        if let Some((limit_id, _)) = rest.split_once(marker) {
            return Some(limit_id.to_string());
        }
        if let Some(limit_id) = rest.strip_suffix(marker) {
            return Some(limit_id.to_string());
        }
    }
    None
}

fn rate_limit_window_from_headers(
    headers: &HeaderMap,
    prefix: &str,
    window: &str,
) -> Option<RateLimitWindow> {
    let used_percent = header_f64(headers, &format!("{prefix}-{window}-used-percent"))?;
    Some(RateLimitWindow {
        used_percent,
        window_minutes: header_u64(headers, &format!("{prefix}-{window}-window-minutes")),
        reset_at_unix_secs: header_timestamp(headers, &format!("{prefix}-{window}-reset-at")),
    })
}

fn credits_from_headers(headers: &HeaderMap, prefix: &str) -> Option<RateLimitCredits> {
    let credits = RateLimitCredits {
        has_credits: header_bool(headers, &format!("{prefix}-credits-has-credits")),
        unlimited: header_bool(headers, &format!("{prefix}-credits-unlimited")),
        balance: header_string(headers, &format!("{prefix}-credits-balance")),
    };
    (credits.has_credits.is_some() || credits.unlimited.is_some() || credits.balance.is_some())
        .then_some(credits)
}

fn has_rate_limit_snapshot_data(snapshot: &RateLimitSnapshot) -> bool {
    snapshot.primary.is_some()
        || snapshot.secondary.is_some()
        || snapshot.credits.is_some()
        || snapshot.spend_control_reached.is_some()
        || snapshot.plan_type.is_some()
        || snapshot.rate_limit_reached_type.is_some()
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn header_f64(headers: &HeaderMap, name: &str) -> Option<f64> {
    header_string(headers, name)?.parse().ok()
}

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    header_string(headers, name)?.parse().ok()
}

fn header_bool(headers: &HeaderMap, name: &str) -> Option<bool> {
    match header_string(headers, name)?.to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

fn header_timestamp(headers: &HeaderMap, name: &str) -> Option<u64> {
    header_string(headers, name).and_then(|value| parse_timestamp_str(&value))
}

fn parse_timestamp_value(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|v| u64::try_from(v).ok()))
        .or_else(|| value.as_str().and_then(parse_timestamp_str))
}

fn parse_timestamp_str(value: &str) -> Option<u64> {
    if let Ok(timestamp) = value.parse::<u64>() {
        return Some(timestamp);
    }

    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .and_then(|dt| u64::try_from(dt.timestamp()).ok())
}

fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn next_chatgpt_request_id() -> u64 {
    static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);
    NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed)
}

fn elapsed_millis(started_at: Instant) -> u64 {
    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn window_minutes_from_seconds(seconds: u64) -> u64 {
    seconds.saturating_add(59) / 60
}

fn balance_value_to_string(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.trim().to_string()).filter(|value| !value.is_empty()),
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    }
}

fn normalize_limit_id(value: &str) -> String {
    value.trim().to_ascii_lowercase().replace('-', "_")
}

#[cfg(test)]
fn build_chatgpt_responses_body(request: &MessagesRequest) -> Value {
    build_chatgpt_responses_body_with_context(request, None)
}

#[cfg(test)]
fn build_chatgpt_responses_body_with_context(
    request: &MessagesRequest,
    installation_id: Option<&str>,
) -> Value {
    responses::build_body(request, DEFAULT_CHATGPT_INSTRUCTIONS, installation_id)
}

#[cfg(test)]
fn build_chatgpt_responses_lite_body(request: &MessagesRequest) -> Value {
    build_chatgpt_responses_body_with_codex_context(
        request,
        responses::CodexRequestContext {
            responses_lite: true,
            ..responses::CodexRequestContext::default()
        },
    )
}

#[cfg(test)]
fn build_chatgpt_responses_body_with_codex_context(
    request: &MessagesRequest,
    context: responses::CodexRequestContext<'_>,
) -> Value {
    responses::build_body_with_context(request, DEFAULT_CHATGPT_INSTRUCTIONS, context)
        .expect("test request should convert to a valid ChatGPT Responses body")
}

fn ensure_chatgpt_stable_client_conversation_id(
    mut request: MessagesRequest,
) -> (MessagesRequest, bool) {
    if responses::stable_client_conversation_id_for_continuation(&request).is_some() {
        return (request, false);
    }

    let Some(session_id) = synthetic_chatgpt_client_session_id(&request) else {
        return (request, false);
    };
    request
        .extra
        .insert("client_session_id".to_string(), Value::String(session_id));
    (request, true)
}

fn synthetic_chatgpt_client_session_id(request: &MessagesRequest) -> Option<String> {
    let first_user_message = request
        .messages
        .iter()
        .find(|message| message.role == Role::User)?;
    let mut hasher = Sha256::new();
    hasher.update(b"claude-proxy-chatgpt-synthetic-session-v1");
    update_synthetic_session_hash(&mut hasher, &request.model);
    update_synthetic_session_hash(&mut hasher, &request.system);
    update_synthetic_session_hash(&mut hasher, first_user_message);
    let digest = hasher.finalize();
    Some(format!(
        "cp-synth-{}",
        hex_prefix(&digest, CHATGPT_SYNTHETIC_SESSION_HASH_BYTES)
    ))
}

fn update_synthetic_session_hash<T: Serialize>(hasher: &mut Sha256, value: &T) {
    match serde_json::to_vec(value) {
        Ok(bytes) => {
            hasher.update((bytes.len() as u64).to_be_bytes());
            hasher.update(bytes);
        }
        Err(_) => hasher.update(b"<json-error>"),
    }
}

fn hex_prefix(bytes: &[u8], len: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(len * 2);
    for byte in bytes.iter().take(len) {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

fn is_prompt_too_long_error(status: StatusCode, body: &str) -> bool {
    let message = chatgpt_error_message_from_body(body);
    if looks_like_output_limit_error(&message) {
        return false;
    }

    if serde_json::from_str::<Value>(body).is_ok_and(|value| {
        value
            .pointer("/error/code")
            .and_then(Value::as_str)
            .is_some_and(|code| code == "context_length_exceeded")
    }) {
        return true;
    }

    matches!(
        status,
        StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE | StatusCode::UNPROCESSABLE_ENTITY
    ) && body.to_ascii_lowercase().contains("prompt is too long")
}

fn is_prompt_too_long_candidate_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE | StatusCode::UNPROCESSABLE_ENTITY
    )
}

#[cfg(test)]
fn map_chatgpt_error_status_body(status: StatusCode, body: String) -> ProviderError {
    map_chatgpt_error_status_body_with_headers(status, &HeaderMap::new(), body)
}

fn map_chatgpt_error_status_body_with_headers(
    status: StatusCode,
    headers: &HeaderMap,
    body: String,
) -> ProviderError {
    let mut message = chatgpt_error_message_from_body(&body);
    if let Some(output_limit_message) = chatgpt_output_limit_error_message(status, &message) {
        message = output_limit_message;
    }
    let prompt_too_long = is_prompt_too_long_error(status, &body);
    if prompt_too_long
        && !message
            .to_ascii_lowercase()
            .starts_with("prompt is too long")
    {
        message = format!("Prompt is too long: {message}");
    }

    let metadata =
        upstream_error_metadata_from_parts(status.as_u16(), headers, &body, message.clone());
    let error = if prompt_too_long {
        ProviderError::InvalidRequest(message)
    } else {
        match status {
            StatusCode::BAD_REQUEST => ProviderError::InvalidRequest(message),
            StatusCode::UNAUTHORIZED => ProviderError::Authentication(message),
            StatusCode::NOT_FOUND => ProviderError::ModelNotFound(message),
            StatusCode::PAYLOAD_TOO_LARGE => ProviderError::RequestTooLarge(message),
            StatusCode::TOO_MANY_REQUESTS
                if is_non_retryable_rate_limit_headers(headers)
                    || is_non_retryable_rate_limit_error_body(&body) =>
            {
                ProviderError::InvalidRequest(message)
            }
            StatusCode::TOO_MANY_REQUESTS => ProviderError::RateLimited {
                retry_after: metadata.retry_after,
            },
            status if status.is_server_error() => ProviderError::Overloaded {
                message,
                retry_after: metadata.retry_after,
            },
            status => ProviderError::UpstreamError {
                status: status.as_u16(),
                body,
            },
        }
    };
    error.with_upstream_metadata(metadata)
}

fn chatgpt_error_message_from_body(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            ["/error/message", "/detail", "/message"]
                .iter()
                .find_map(|pointer| value.pointer(pointer).and_then(chatgpt_error_message_value))
        })
        .unwrap_or_else(|| body.to_string())
}

fn chatgpt_error_message_value(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(ToOwned::to_owned)
        .or_else(|| value.is_null().then(String::new))
        .filter(|message| !message.is_empty())
}

fn chatgpt_output_limit_error_message(status: StatusCode, message: &str) -> Option<String> {
    if !matches!(
        status,
        StatusCode::BAD_REQUEST | StatusCode::PAYLOAD_TOO_LARGE | StatusCode::UNPROCESSABLE_ENTITY
    ) || !looks_like_output_limit_error(message)
    {
        return None;
    }

    Some(
        "requested max_tokens exceeds the upstream model output limit; lower max_tokens or choose a model with a larger output budget".to_string(),
    )
}

fn looks_like_output_limit_error(message: &str) -> bool {
    let normalized = message.to_ascii_lowercase().replace(['-', '_'], " ");
    let mentions_output_budget = [
        "max output tokens",
        "max tokens",
        "output tokens",
        "output token",
        "output limit",
        "maximum output",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));
    let mentions_limit = [
        "limit",
        "maximum",
        "exceed",
        "exceeds",
        "too high",
        "too large",
        "greater than",
        "less than or equal",
        "at most",
    ]
    .iter()
    .any(|needle| normalized.contains(needle));

    mentions_output_budget && mentions_limit
}

fn chatgpt_websocket_fallback_reason(
    error: &transport::ChatGptWebSocketStartError,
) -> &'static str {
    let message = error.error.to_string().to_ascii_lowercase();
    if message.contains("previous response") && message.contains("not found") {
        return "previous_response_not_found";
    }

    match error.phase {
        transport::ChatGptWebSocketPhase::Connect
        | transport::ChatGptWebSocketPhase::ProxyConnect => "websocket_connect_failed",
        transport::ChatGptWebSocketPhase::Send => "websocket_send_failed",
        transport::ChatGptWebSocketPhase::FirstEvent => "websocket_first_event_failed",
        transport::ChatGptWebSocketPhase::AfterFirstEvent
        | transport::ChatGptWebSocketPhase::Protocol => "websocket_startup_failure",
    }
}

fn notify_request_metadata_observer(
    observer: Option<&ProviderRequestObserver>,
    request_metadata: ProviderRequestMetadata,
) {
    let Some(observer) = observer else {
        return;
    };
    observer(ProviderRequestObserverEvent {
        event: ProviderRequestObserverEventKind::RequestMetadata,
        request_metadata: Some(request_metadata),
        ..ProviderRequestObserverEvent::default()
    });
}

fn validate_chatgpt_tool_schema_budget(body: &Value) -> Result<(), ProviderError> {
    let (tools_count, tools_schema_bytes) = chatgpt_tool_schema_stats(body);
    if tools_schema_bytes <= CHATGPT_TOOL_SCHEMA_BUDGET_BYTES {
        return Ok(());
    }

    Err(ProviderError::InvalidRequest(format!(
        "ChatGPT upstream tool schema payload is too large ({tools_schema_bytes} bytes across {tools_count} tools; limit {CHATGPT_TOOL_SCHEMA_BUDGET_BYTES} bytes). Enable Claude Code ToolSearch or reduce MCP tools before retrying."
    )))
}

fn chatgpt_tool_schema_stats(body: &Value) -> (usize, usize) {
    let Some(tools) = body.get("tools").and_then(Value::as_array) else {
        return (0, 0);
    };
    (
        tools.len(),
        serde_json::to_vec(tools).map_or(0, |bytes| bytes.len()),
    )
}

fn json_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
}

fn context_static_body(body: &Value) -> Value {
    let mut body = body.clone();
    if let Some(object) = body.as_object_mut() {
        for key in [
            "input",
            "max_output_tokens",
            "previous_response_id",
            "prompt_cache_key",
            "stream",
        ] {
            object.remove(key);
        }
    }
    body
}

fn input_has_compressible_history(input: &[Value]) -> bool {
    input.iter().any(|item| {
        item.get("role").and_then(Value::as_str) == Some("assistant")
            || matches!(
                item.get("type").and_then(Value::as_str),
                Some("function_call" | "custom_tool_call")
            )
    })
}

fn estimate_context_value_tokens(value: &Value) -> u64 {
    match value {
        Value::Null | Value::Bool(_) => 0,
        Value::Number(_) => 1,
        Value::String(text) => {
            if text.starts_with("data:") && text.contains(";base64,") {
                CHATGPT_MEDIA_ESTIMATED_TOKENS
            } else {
                (text.chars().count() as u64).div_ceil(4)
            }
        }
        Value::Array(items) => items
            .iter()
            .map(estimate_context_value_tokens)
            .sum::<u64>()
            .saturating_add(items.len() as u64),
        Value::Object(object) => {
            if matches!(
                object.get("type").and_then(Value::as_str),
                Some("image" | "input_image" | "document" | "input_file")
            ) {
                return CHATGPT_MEDIA_ESTIMATED_TOKENS;
            }
            object.iter().fold(0u64, |total, (key, value)| {
                total
                    .saturating_add((key.chars().count() as u64).div_ceil(4))
                    .saturating_add(estimate_context_value_tokens(value))
                    .saturating_add(1)
            })
        }
    }
}

fn virtual_context_limit_error(estimate: &VirtualContextEstimate) -> Option<ProviderError> {
    (estimate.estimated_tokens > u64::from(estimate.safe_input_limit)).then(|| {
        ProviderError::InvalidRequest(format!(
            "Prompt is too long: {} tokens > {} maximum safe input (model context window: {}; local estimate based on {})",
            estimate.estimated_tokens,
            estimate.safe_input_limit,
            estimate.model_context_window,
            estimate.estimator_source.as_str(),
        ))
    })
}

fn prepare_chatgpt_sse_request_body(body: &Value) -> Result<ChatGptSseRequestBody, ProviderError> {
    let json_bytes = serde_json::to_vec(body).map_err(|error| {
        ProviderError::InvalidRequest(format!("invalid ChatGPT request body: {error}"))
    })?;
    Ok(chatgpt_sse_request_body_from_json(json_bytes, |bytes| {
        zstd::bulk::compress(bytes, CHATGPT_SSE_REQUEST_ZSTD_LEVEL)
    }))
}

fn chatgpt_sse_request_body_from_json<E>(
    json_bytes: Vec<u8>,
    compress: impl FnOnce(&[u8]) -> Result<Vec<u8>, E>,
) -> ChatGptSseRequestBody
where
    E: std::fmt::Display,
{
    let original_len = json_bytes.len();
    match compress(&json_bytes) {
        Ok(bytes) => ChatGptSseRequestBody {
            bytes,
            original_len,
            content_encoding: Some("zstd"),
        },
        Err(error) => {
            warn!(error = %error, "ChatGPT SSE request zstd compression failed; sending JSON body");
            ChatGptSseRequestBody {
                bytes: json_bytes,
                original_len,
                content_encoding: None,
            }
        }
    }
}

#[cfg(test)]
fn chatgpt_request_warning_threshold(model: &str, config: &ChatGptProviderConfig) -> Option<usize> {
    let context_window = chatgpt_model_info(model, config)?
        .capabilities
        .limits
        .context_window?;
    chatgpt_request_warning_threshold_for_window(context_window)
}

fn chatgpt_request_warning_threshold_for_window(context_window: u32) -> Option<usize> {
    let context_window = context_window as usize;
    Some(
        context_window
            .saturating_mul(CHATGPT_REQUEST_WARNING_RATIO)
            .saturating_div(100)
            .saturating_mul(CHATGPT_BYTES_PER_ESTIMATED_TOKEN),
    )
}

#[cfg(test)]
fn request_size_warning(
    model: &str,
    config: &ChatGptProviderConfig,
    body_bytes: usize,
) -> Option<(usize, usize)> {
    let threshold_bytes = chatgpt_request_warning_threshold(model, config)?;
    (body_bytes >= threshold_bytes).then_some((
        threshold_bytes,
        body_bytes / CHATGPT_BYTES_PER_ESTIMATED_TOKEN,
    ))
}

fn request_size_warning_for_window(
    context_window: u32,
    body_bytes: usize,
) -> Option<(usize, usize)> {
    let threshold_bytes = chatgpt_request_warning_threshold_for_window(context_window)?;
    (body_bytes >= threshold_bytes).then_some((
        threshold_bytes,
        body_bytes / CHATGPT_BYTES_PER_ESTIMATED_TOKEN,
    ))
}

fn warn_if_request_nears_context_window(
    request_id: u64,
    compact_request: bool,
    prompt_too_long_attempt: usize,
    model: &str,
    context_window: Option<u32>,
    body_bytes: usize,
) {
    let Some(context_window) = context_window else {
        return;
    };
    let Some((threshold_bytes, estimated_tokens)) =
        request_size_warning_for_window(context_window, body_bytes)
    else {
        return;
    };
    warn!(
        request_id,
        compact_request,
        prompt_too_long_attempt,
        model,
        body_bytes,
        threshold_bytes,
        estimated_tokens,
        warning_ratio = CHATGPT_REQUEST_WARNING_RATIO,
        "ChatGPT request is approaching the model context window"
    );
}

#[derive(Debug, Clone, Copy)]
struct ChatGptModelSpec {
    model_id: &'static str,
    context_window: u32,
    image_input: bool,
    responses_lite: bool,
    reasoning_efforts: &'static [&'static str],
    service_tiers: &'static [&'static str],
}

const CHATGPT_MODEL_SPECS: &[ChatGptModelSpec] = &[
    ChatGptModelSpec {
        model_id: "gpt-5.6-sol",
        context_window: CHATGPT_GPT_56_DEFAULT_CONTEXT_WINDOW,
        image_input: true,
        responses_lite: true,
        reasoning_efforts: &["low", "medium", "high", "xhigh", "max", "ultra"],
        service_tiers: &["priority"],
    },
    ChatGptModelSpec {
        model_id: "gpt-5.6-terra",
        context_window: CHATGPT_GPT_56_DEFAULT_CONTEXT_WINDOW,
        image_input: true,
        responses_lite: true,
        reasoning_efforts: &["low", "medium", "high", "xhigh", "max", "ultra"],
        service_tiers: &["priority"],
    },
    ChatGptModelSpec {
        model_id: "gpt-5.6-luna",
        context_window: CHATGPT_GPT_56_DEFAULT_CONTEXT_WINDOW,
        image_input: true,
        responses_lite: true,
        reasoning_efforts: &["low", "medium", "high", "xhigh", "max"],
        service_tiers: &["priority"],
    },
];

fn chatgpt_models(config: &ChatGptProviderConfig) -> Vec<ModelInfo> {
    let mut models = CHATGPT_MODEL_SPECS
        .iter()
        .copied()
        .map(|spec| {
            let capability = config.model_capabilities.get(spec.model_id);
            chatgpt_model_info_from_spec(spec, capability)
        })
        .collect::<Vec<_>>();

    append_configured_chatgpt_models(
        &mut models,
        config,
        CHATGPT_MODEL_SPECS.iter().map(|spec| spec.model_id),
    );

    if let Some(model) = models.first_mut() {
        model.is_chat_default = Some(true);
    }

    models
}

fn append_configured_chatgpt_models<'a>(
    models: &mut Vec<ModelInfo>,
    config: &ChatGptProviderConfig,
    known_ids: impl IntoIterator<Item = &'a str>,
) {
    let known_ids = known_ids.into_iter().collect::<BTreeSet<_>>();
    let mut configured_models = config
        .model_capabilities
        .iter()
        .filter(|(model_id, _)| !known_ids.contains(model_id.as_str()))
        .collect::<Vec<_>>();
    configured_models.sort_by_key(|(model_id, _)| *model_id);
    models.extend(
        configured_models
            .into_iter()
            .map(|(model_id, capability)| chatgpt_model_info_from_capability(model_id, capability)),
    );
}

fn chatgpt_catalog_model_from_remote(
    model: ChatGptRemoteModel,
    config: &ChatGptProviderConfig,
) -> ChatGptCatalogModel {
    let capability = config.model_capabilities.get(&model.slug);
    let context_window = capability
        .and_then(|capability| capability.context_window)
        .or_else(|| chatgpt_remote_context_window(&model));
    let image_input = capability
        .and_then(|capability| capability.image_input)
        .unwrap_or_else(|| {
            model
                .input_modalities
                .iter()
                .any(|modality| modality.eq_ignore_ascii_case("image"))
        });
    let reasoning_efforts = configured_reasoning_effort_levels(capability).unwrap_or_else(|| {
        model
            .supported_reasoning_levels
            .iter()
            .map(|level| level.effort.trim())
            .filter(|effort| !effort.is_empty())
            .map(str::to_string)
            .collect()
    });
    let responses_lite = capability
        .and_then(|capability| capability.responses_lite)
        .unwrap_or(model.use_responses_lite);
    let supports_reasoning_summary_parameter = model.supports_reasoning_summary_parameter;
    let supports_parallel_tool_calls = model.supports_parallel_tool_calls;
    let auto_compact_token_limit = model
        .auto_compact_token_limit
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0);
    let effective_context_window_percent = model
        .effective_context_window_percent
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| (1..=100).contains(value));
    let service_tiers = model.service_tiers.map(|tiers| {
        tiers
            .into_iter()
            .map(|tier| tier.id.trim().to_string())
            .filter(|tier| !tier.is_empty())
            .collect()
    });

    ChatGptCatalogModel {
        info: chatgpt_model_info_from_parts(
            &model.slug,
            context_window,
            image_input,
            reasoning_efforts,
        ),
        responses_lite,
        supports_reasoning_summary_parameter,
        supports_parallel_tool_calls,
        auto_compact_token_limit,
        effective_context_window_percent,
        visibility: model.visibility,
        priority: model.priority,
        service_tiers,
    }
}

fn chatgpt_remote_context_window(model: &ChatGptRemoteModel) -> Option<u32> {
    // The Codex catalog exposes the currently active 272K window separately from
    // the GPT-5.6 model's supported 872K maximum. The proxy's default is the
    // larger supported window; explicit model_capabilities overrides still win.
    let windows = if model.slug.starts_with("gpt-5.6") {
        [model.max_context_window, model.context_window]
    } else {
        [model.context_window, model.max_context_window]
    };
    windows
        .into_iter()
        .flatten()
        .find_map(|window| u32::try_from(window).ok())
}

fn chatgpt_model_info(model_id: &str, config: &ChatGptProviderConfig) -> Option<ModelInfo> {
    let model_id = normalize_chatgpt_model_id(model_id);
    CHATGPT_MODEL_SPECS
        .iter()
        .copied()
        .find(|spec| spec.model_id == model_id)
        .map(|spec| {
            let capability = config.model_capabilities.get(spec.model_id);
            chatgpt_model_info_from_spec(spec, capability)
        })
        .or_else(|| {
            config
                .model_capabilities
                .get(model_id)
                .map(|capability| chatgpt_model_info_from_capability(model_id, capability))
        })
}

pub fn configured_chatgpt_context_window(
    settings: &Settings,
    provider_id: &str,
    model_id: &str,
) -> Option<u32> {
    let provider = settings.providers.get(provider_id)?;
    if provider.resolve_type(provider_id) != ProviderType::ChatGPT {
        return None;
    }
    let config = provider.chatgpt.as_ref();
    if let Some(context_window) = config
        .and_then(|config| config.model_capabilities.get(model_id))
        .and_then(|capability| capability.context_window)
    {
        return Some(context_window);
    }
    if let Some(account_hash) = capability_cache::current_account_hash()
        && let Some(context_window) = capability_cache::cached_context_window(
            provider_id,
            &normalized_codex_base_url(&provider.base_url),
            &account_hash,
            model_id,
        )
    {
        return Some(context_window);
    }
    chatgpt_model_info(model_id, &config.cloned().unwrap_or_default())
        .and_then(|model| model.capabilities.limits.context_window)
}

pub fn claude_code_projected_model(settings: &Settings, model_ref: &str) -> String {
    let model_ref = model_ref.trim();
    let bare_model_ref = model_ref.trim_end_matches("[1m]");
    let Some((provider_id, model_id)) = bare_model_ref.split_once('/') else {
        return model_ref.to_string();
    };
    let Some(provider) = settings.providers.get(provider_id) else {
        return model_ref.to_string();
    };
    if provider.resolve_type(provider_id) != ProviderType::ChatGPT {
        return model_ref.to_string();
    }
    if provider
        .chatgpt
        .as_ref()
        .is_some_and(|config| config.claude_code_context == ClaudeCodeContextMode::Standard)
    {
        return bare_model_ref.to_string();
    }
    if configured_chatgpt_context_window(settings, provider_id, model_id)
        .is_some_and(|window| window > CLAUDE_CODE_DEFAULT_CONTEXT_WINDOW)
    {
        format!("{bare_model_ref}[1m]")
    } else {
        bare_model_ref.to_string()
    }
}

fn normalize_chatgpt_model_id(model: &str) -> &str {
    model.rsplit('/').next().unwrap_or(model)
}

fn normalize_chatgpt_56_reasoning(request: &mut MessagesRequest) -> bool {
    if !normalize_chatgpt_model_id(&request.model).starts_with("gpt-5.6") {
        return false;
    }

    let mut mapped = false;
    if let Some(reasoning) = request
        .extra
        .get_mut("reasoning")
        .and_then(Value::as_object_mut)
        && let Some(effort) = reasoning.get("effort").and_then(Value::as_str)
    {
        match effort {
            "none" | "minimal" => {
                reasoning.insert("effort".to_string(), Value::String("low".to_string()));
            }
            "ultra" => {
                reasoning.insert("effort".to_string(), Value::String("max".to_string()));
                mapped = true;
            }
            _ => {}
        }
    }
    if let Some(effort) = request
        .extra
        .get("reasoning_effort")
        .and_then(Value::as_str)
    {
        match effort {
            "none" | "minimal" => {
                request.extra.insert(
                    "reasoning_effort".to_string(),
                    Value::String("low".to_string()),
                );
            }
            "ultra" => {
                request.extra.insert(
                    "reasoning_effort".to_string(),
                    Value::String("max".to_string()),
                );
                mapped = true;
            }
            _ => {}
        }
    }
    if request
        .thinking
        .as_ref()
        .and_then(|thinking| thinking.r#type.as_deref())
        == Some("disabled")
    {
        request.thinking = None;
        request.extra.insert(
            "reasoning_effort".to_string(),
            Value::String("low".to_string()),
        );
    }
    mapped
}

fn normalize_chatgpt_native_56_reasoning(body: &mut Value, model: &str) {
    if !normalize_chatgpt_model_id(model).starts_with("gpt-5.6") {
        return;
    }
    let Some(effort) = body
        .get_mut("reasoning")
        .and_then(Value::as_object_mut)
        .and_then(|reasoning| reasoning.get_mut("effort"))
    else {
        return;
    };
    match effort.as_str() {
        Some("none" | "minimal") => *effort = Value::String("low".to_string()),
        Some("ultra") => *effort = Value::String("max".to_string()),
        _ => {}
    }
}

fn normalize_chatgpt_native_responses_body(
    body: &mut Value,
    responses_lite: bool,
) -> ChatGptOutputTokenBudget {
    let requested = body.get("max_output_tokens").and_then(Value::as_u64);
    let Some(object) = body.as_object_mut() else {
        return ChatGptOutputTokenBudget {
            requested,
            effective: requested,
        };
    };

    if let Some(input) = object
        .get("input")
        .and_then(Value::as_str)
        .map(str::to_string)
    {
        object.insert(
            "input".to_string(),
            serde_json::json!([{"role": "user", "content": input}]),
        );
    }

    // The ChatGPT Codex backend rejects this public Responses API parameter.
    // Retain it as an observed request budget, but do not send an ineffective
    // field upstream. The limitation is advertised in /v1/models.
    object.remove("max_output_tokens");

    if responses_lite {
        object.insert("parallel_tool_calls".to_string(), Value::Bool(false));
        let reasoning = object
            .entry("reasoning".to_string())
            .or_insert_with(|| serde_json::json!({}));
        if !reasoning.is_object() {
            *reasoning = serde_json::json!({});
        }
        if let Some(reasoning) = reasoning.as_object_mut() {
            reasoning.insert(
                "context".to_string(),
                Value::String("all_turns".to_string()),
            );
        }
    }

    ChatGptOutputTokenBudget {
        requested,
        effective: None,
    }
}

fn request_has_delegation_tool(request: &MessagesRequest) -> bool {
    request.tools.as_ref().is_some_and(|tools| {
        tools.iter().any(|tool| {
            let name = tool.name.to_ascii_lowercase();
            matches!(
                name.as_str(),
                "agent" | "delegate" | "spawn_agent" | "spawn_agents" | "subagent" | "task"
            ) || name.contains("spawn_agent")
                || name.contains("subagent")
                || name.contains("delegate")
        })
    })
}

fn chatgpt_model_supports_responses_lite(model_id: &str) -> bool {
    CHATGPT_MODEL_SPECS
        .iter()
        .any(|spec| spec.model_id == model_id && spec.responses_lite)
}

fn chatgpt_model_info_from_spec(
    spec: ChatGptModelSpec,
    capability: Option<&ChatGptModelCapabilityOverride>,
) -> ModelInfo {
    let context_window = capability
        .and_then(|capability| capability.context_window)
        .unwrap_or(spec.context_window);
    let image_input = capability
        .and_then(|capability| capability.image_input)
        .unwrap_or(spec.image_input);
    let reasoning_efforts = configured_reasoning_effort_levels(capability).unwrap_or_else(|| {
        spec.reasoning_efforts
            .iter()
            .map(|effort| (*effort).to_string())
            .collect()
    });

    chatgpt_model_info_from_parts(
        spec.model_id,
        Some(context_window),
        image_input,
        reasoning_efforts,
    )
}

fn chatgpt_model_info_from_capability(
    model_id: &str,
    capability: &ChatGptModelCapabilityOverride,
) -> ModelInfo {
    chatgpt_model_info_from_parts(
        model_id,
        capability.context_window,
        capability.image_input.unwrap_or(false),
        configured_reasoning_effort_levels(Some(capability))
            .unwrap_or_else(default_reasoning_efforts),
    )
}

fn configured_reasoning_effort_levels(
    capability: Option<&ChatGptModelCapabilityOverride>,
) -> Option<Vec<String>> {
    let levels = capability?
        .reasoning_effort_levels
        .as_ref()?
        .iter()
        .map(|level| level.trim())
        .filter(|level| !level.is_empty())
        .map(str::to_string)
        .collect::<Vec<_>>();
    (!levels.is_empty()).then_some(levels)
}

fn default_reasoning_efforts() -> Vec<String> {
    ["minimal", "low", "medium", "high", "xhigh", "max"]
        .into_iter()
        .map(str::to_string)
        .collect()
}

fn chatgpt_model_info_from_parts(
    model_id: &str,
    context_window: Option<u32>,
    image_input: bool,
    reasoning_efforts: Vec<String>,
) -> ModelInfo {
    ModelInfo {
        model_id: model_id.to_string(),
        vendor: Some("openai".to_string()),
        is_chat_default: None,
        capabilities: ModelCapabilities {
            endpoints: EndpointCapabilities {
                openai_responses: CapabilityState::Supported,
                openai_chat_completions: CapabilityState::Unsupported,
                anthropic_messages: CapabilityState::Unknown,
            },
            modalities: ModalityCapabilities {
                input: InputModalities {
                    text: CapabilityState::Supported,
                    image: CapabilityState::from_bool(Some(image_input)),
                    document: CapabilityState::Unknown,
                    audio: CapabilityState::Unsupported,
                    video: CapabilityState::Unsupported,
                },
                output: OutputModalities {
                    text: CapabilityState::Supported,
                    image: CapabilityState::Unsupported,
                    audio: CapabilityState::Unsupported,
                },
            },
            features: FeatureCapabilities {
                streaming: CapabilityState::Supported,
                system_prompt: CapabilityState::Supported,
                tools: CapabilityState::Supported,
                tool_choice: CapabilityState::Supported,
                thinking: CapabilityState::Supported,
                adaptive_thinking: CapabilityState::Supported,
                reasoning_effort: CapabilityState::Supported,
                prompt_cache: CapabilityState::Supported,
                sampling: CapabilityState::Unknown,
                stop_sequences: CapabilityState::Unknown,
            },
            limits: ModelLimits {
                context_window,
                max_output_tokens: None,
                min_thinking_budget: None,
                max_thinking_budget: None,
                reasoning_effort_levels: reasoning_efforts,
            },
            quality: QualityGateCapabilities {
                tool_search: ToolSearchCapability::unsupported(),
                prompt_cache: PromptCacheCapability::basic(),
                max_effort: CapabilityState::Supported,
                structured_outputs: CapabilityState::Supported,
                fast_mode: CapabilityState::Supported,
                token_counting: TokenCountingCapability::rough(),
                ..Default::default()
            },
            responses: Some(ResponsesCapabilities {
                unsupported_parameters: vec!["max_output_tokens".to_string()],
                ..ResponsesCapabilities::streaming_stateless(CapabilityState::Supported)
            }),
            supported_parameters: vec![
                "system".to_string(),
                "messages".to_string(),
                "stream".to_string(),
                "tools".to_string(),
                "tool_choice".to_string(),
                "thinking".to_string(),
                "reasoning_effort".to_string(),
                "prompt_cache_key".to_string(),
                "prompt_cache_options".to_string(),
                "safety_identifier".to_string(),
                "parallel_tool_calls".to_string(),
                "service_tier".to_string(),
                "verbosity".to_string(),
            ],
        },
    }
}

#[cfg(test)]
#[allow(
    clippy::result_large_err,
    reason = "tungstenite's handshake callback fixes the large HTTP response as its error type"
)]
mod tests {
    use super::*;
    use futures::SinkExt;
    use serde_json::json;
    use std::env;
    use std::ffi::OsString;
    use std::sync::Mutex as StdMutex;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio_tungstenite::accept_hdr_async;
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    use tokio_tungstenite::tungstenite::handshake::server::{
        Request as WsServerRequest, Response as WsServerResponse,
    };

    static CHATGPT_WEBSOCKET_PROXY_ENV_LOCK: tokio::sync::Mutex<()> =
        tokio::sync::Mutex::const_new(());

    #[derive(Debug)]
    struct EnvVarGuard {
        key: &'static str,
        original: Option<OsString>,
    }

    impl EnvVarGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = env::var_os(key);
            unsafe { env::set_var(key, value) };
            Self { key, original }
        }

        fn remove(key: &'static str) -> Self {
            let original = env::var_os(key);
            unsafe { env::remove_var(key) };
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.original {
                unsafe { env::set_var(self.key, value) };
            } else {
                unsafe { env::remove_var(self.key) };
            }
        }
    }

    #[test]
    fn codex_fast_mode_uses_priority_service_tier() {
        assert_eq!(effective_codex_service_tier(None, true), Some("priority"));
    }

    #[test]
    fn codex_fast_mode_is_disabled_by_default() {
        assert_eq!(effective_codex_service_tier(None, false), None);
    }

    #[test]
    fn runtime_service_tier_overrides_codex_fast_mode() {
        assert_eq!(
            effective_codex_service_tier(Some("flex"), true),
            Some("flex")
        );
    }

    #[tokio::test]
    async fn codex_service_tier_filters_known_unsupported_models() {
        let mut provider = test_chatgpt_provider("http://127.0.0.1:1/responses".to_string()).await;
        provider.chatgpt_config.fast_mode = true;
        provider
            .remote_models
            .write()
            .expect("ChatGPT remote models lock poisoned")
            .insert(
                "remote-no-priority".to_string(),
                ChatGptCatalogModel {
                    info: chatgpt_model_info_from_parts(
                        "remote-no-priority",
                        Some(272_000),
                        true,
                        Vec::new(),
                    ),
                    responses_lite: true,
                    supports_reasoning_summary_parameter: true,
                    supports_parallel_tool_calls: true,
                    auto_compact_token_limit: None,
                    effective_context_window_percent: None,
                    visibility: None,
                    priority: 0,
                    service_tiers: Some(Vec::new()),
                },
            );

        assert_eq!(provider.codex_service_tier("gpt-5.6-sol"), Some("priority"));
        assert_eq!(provider.codex_service_tier("remote-no-priority"), None);
        assert_eq!(
            provider.codex_service_tier("custom-model"),
            Some("priority")
        );
    }

    #[test]
    fn builds_default_codex_responses_endpoint() {
        assert_eq!(
            codex_responses_endpoint(""),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            codex_responses_endpoint("https://chatgpt.com"),
            "https://chatgpt.com/backend-api/codex/responses"
        );
        assert_eq!(
            codex_responses_endpoint("https://chat.openai.com/"),
            "https://chat.openai.com/backend-api/codex/responses"
        );
        assert_eq!(
            codex_responses_endpoint("https://example.test/base"),
            "https://example.test/base/responses"
        );
        assert_eq!(
            codex_responses_endpoint("https://example.test/base/responses"),
            "https://example.test/base/responses"
        );
    }

    #[test]
    fn builds_chatgpt_usage_endpoint() {
        assert_eq!(
            codex_usage_endpoint(""),
            "https://chatgpt.com/backend-api/wham/usage"
        );
        assert_eq!(
            codex_usage_endpoint("https://chatgpt.com"),
            "https://chatgpt.com/backend-api/wham/usage"
        );
        assert_eq!(
            codex_usage_endpoint("https://chat.openai.com/"),
            "https://chat.openai.com/backend-api/wham/usage"
        );
        assert_eq!(
            codex_usage_endpoint("https://chatgpt.com/backend-api/codex/responses"),
            "https://chatgpt.com/backend-api/wham/usage"
        );
        assert_eq!(
            codex_usage_endpoint("https://chatgpt.com/api/codex"),
            "https://chatgpt.com/api/codex/usage"
        );
    }

    #[test]
    fn parses_usage_payload_rate_limits() {
        let payload: UsagePayload = serde_json::from_value(json!({
            "plan_type": "plus",
            "rate_limit": {
                "primary": {
                    "used_percent": 12.5,
                    "window_minutes": 300,
                    "reset_at": 1800000000
                },
                "secondary": {
                    "used_percent": 50.0,
                    "window_minutes": 10080,
                    "reset_at": "2027-01-15T08:00:00Z"
                }
            },
            "credits": {
                "has_credits": true,
                "unlimited": false,
                "balance": 42
            },
            "additional_rate_limits": [{
                "metered_feature": "agent",
                "limit_name": "Agent",
                "rate_limit": {
                    "primary": { "used_percent": 8.0 }
                }
            }]
        }))
        .unwrap();

        let snapshots = rate_limit_snapshots_from_usage_payload("chatgpt", payload, 123);

        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].provider_id, "chatgpt");
        assert_eq!(snapshots[0].feature.as_deref(), Some("codex"));
        assert_eq!(snapshots[0].plan_type.as_deref(), Some("plus"));
        assert_eq!(snapshots[0].primary.as_ref().unwrap().used_percent, 12.5);
        assert_eq!(
            snapshots[0].credits.as_ref().unwrap().balance.as_deref(),
            Some("42")
        );
        assert_eq!(snapshots[1].feature.as_deref(), Some("agent"));
        assert_eq!(snapshots[1].limit_name.as_deref(), Some("Agent"));
    }

    #[test]
    fn parses_official_codex_usage_payload_rate_limits() {
        let payload: UsagePayload = serde_json::from_value(json!({
            "plan_type": "pro",
            "rate_limit": {
                "allowed": true,
                "limit_reached": false,
                "primary_window": {
                    "used_percent": 42,
                    "limit_window_seconds": 3600,
                    "reset_after_seconds": 120,
                    "reset_at": 1735689720
                },
                "secondary_window": {
                    "used_percent": 5,
                    "limit_window_seconds": 86400,
                    "reset_after_seconds": 43200,
                    "reset_at": 1735693200
                }
            },
            "rate_limit_reached_type": {
                "type": "workspace_member_usage_limit_reached"
            },
            "spend_control": {
                "reached": true
            },
            "credits": {
                "has_credits": true,
                "unlimited": false,
                "balance": "9.99"
            },
            "additional_rate_limits": [{
                "limit_name": "codex_other",
                "metered_feature": "codex_other",
                "rate_limit": {
                    "allowed": true,
                    "limit_reached": false,
                    "primary_window": {
                        "used_percent": 88,
                        "limit_window_seconds": 1800,
                        "reset_after_seconds": 600,
                        "reset_at": 1735693200
                    }
                }
            }]
        }))
        .unwrap();

        let snapshots = rate_limit_snapshots_from_usage_payload("chatgpt", payload, 123);

        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].feature.as_deref(), Some("codex"));
        assert_eq!(snapshots[0].plan_type.as_deref(), Some("pro"));
        assert_eq!(
            snapshots[0].rate_limit_reached_type.as_deref(),
            Some("workspace_member_usage_limit_reached")
        );
        assert_eq!(snapshots[0].spend_control_reached, Some(true));
        assert_eq!(snapshots[0].primary.as_ref().unwrap().used_percent, 42.0);
        assert_eq!(
            snapshots[0].primary.as_ref().unwrap().window_minutes,
            Some(60)
        );
        assert_eq!(
            snapshots[0].secondary.as_ref().unwrap().window_minutes,
            Some(1440)
        );
        assert_eq!(
            snapshots[0].credits.as_ref().unwrap().balance.as_deref(),
            Some("9.99")
        );
        assert_eq!(snapshots[1].feature.as_deref(), Some("codex_other"));
        assert_eq!(snapshots[1].limit_name.as_deref(), Some("codex_other"));
        assert_eq!(snapshots[1].spend_control_reached, None);
        assert_eq!(
            snapshots[1].primary.as_ref().unwrap().window_minutes,
            Some(30)
        );
    }

    #[test]
    fn parses_response_header_rate_limits() {
        let mut headers = HeaderMap::new();
        headers.insert("x-codex-primary-used-percent", "40".parse().unwrap());
        headers.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
        headers.insert("x-codex-secondary-used-percent", "75".parse().unwrap());
        headers.insert("x-codex-credits-has-credits", "true".parse().unwrap());
        headers.insert("x-codex-credits-unlimited", "false".parse().unwrap());
        headers.insert("x-codex-credits-balance", "7.50".parse().unwrap());
        headers.insert(
            "x-codex-other-primary-used-percent",
            "12.5".parse().unwrap(),
        );
        headers.insert("x-agent-primary-used-percent", "9.5".parse().unwrap());
        headers.insert("x-agent-limit-name", "Agent".parse().unwrap());

        let snapshots = rate_limit_snapshots_from_headers("chatgpt", &headers, 456);

        assert_eq!(snapshots.len(), 3);
        assert_eq!(snapshots[0].feature.as_deref(), Some("agent"));
        assert_eq!(snapshots[0].limit_name.as_deref(), Some("Agent"));
        assert_eq!(snapshots[0].primary.as_ref().unwrap().used_percent, 9.5);
        assert_eq!(snapshots[1].feature.as_deref(), Some("codex"));
        assert_eq!(snapshots[1].secondary.as_ref().unwrap().used_percent, 75.0);
        assert_eq!(
            snapshots[1].credits.as_ref().unwrap().balance.as_deref(),
            Some("7.50")
        );
        assert_eq!(snapshots[2].feature.as_deref(), Some("codex_other"));
        assert_eq!(snapshots[2].primary.as_ref().unwrap().used_percent, 12.5);
    }

    #[test]
    fn chatgpt_observability_extracts_upstream_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", "req_123".parse().unwrap());
        headers.insert("openai-model", "gpt-5.3-codex".parse().unwrap());

        assert_eq!(
            upstream_request_id_from_headers(&headers).as_deref(),
            Some("req_123")
        );
        assert_eq!(
            upstream_model_from_headers(&headers).as_deref(),
            Some("gpt-5.3-codex")
        );
    }

    #[test]
    fn chatgpt_observability_formats_rate_limit_summary() {
        let mut headers = HeaderMap::new();
        headers.insert("x-codex-primary-used-percent", "40".parse().unwrap());
        headers.insert("x-codex-primary-window-minutes", "300".parse().unwrap());
        headers.insert("x-codex-credits-balance", "7.50".parse().unwrap());
        headers.insert("x-agent-primary-used-percent", "9.5".parse().unwrap());
        headers.insert("x-agent-limit-name", "Agent".parse().unwrap());

        let snapshots = rate_limit_snapshots_from_headers("chatgpt", &headers, 456);
        let summary = rate_limit_summary(&snapshots);

        assert!(summary.contains("Agent:primary=9.5%"));
        assert!(summary.contains("codex:primary=40.0%/300m,credits=7.50"));
    }

    #[test]
    fn parses_codex_rate_limit_sse_event() {
        let snapshot = rate_limit_snapshot_from_sse_event(
            "chatgpt",
            &json!({
                "type": "codex.rate_limits",
                "plan_type": "plus",
                "rate_limits": {
                    "primary": {
                        "used_percent": 61.5,
                        "window_minutes": 300,
                        "reset_at": 1800000000
                    }
                },
                "credits": {
                    "has_credits": true,
                    "unlimited": false,
                    "balance": "2.25"
                },
                "spendControlReached": true,
                "rateLimitReachedType": {
                    "type": "workspace_member_usage_limit_reached"
                },
                "metered_limit_name": "codex_other"
            }),
            999,
        )
        .expect("codex.rate_limits event should parse");

        assert_eq!(snapshot.provider_id, "chatgpt");
        assert_eq!(snapshot.feature.as_deref(), Some("codex_other"));
        assert_eq!(snapshot.plan_type.as_deref(), Some("plus"));
        assert_eq!(snapshot.primary.as_ref().unwrap().used_percent, 61.5);
        assert_eq!(
            snapshot.credits.as_ref().unwrap().balance.as_deref(),
            Some("2.25")
        );
        assert_eq!(snapshot.spend_control_reached, Some(true));
        assert_eq!(
            snapshot.rate_limit_reached_type.as_deref(),
            Some("workspace_member_usage_limit_reached")
        );
        assert_eq!(snapshot.source, RateLimitSource::StreamEvent);
    }

    #[test]
    fn sparse_rate_limit_merge_preserves_spend_control_state() {
        let merged = merge_rate_limit_snapshot(
            RateLimitSnapshot {
                provider_id: "chatgpt".to_string(),
                feature: Some("codex".to_string()),
                spend_control_reached: Some(true),
                source: RateLimitSource::StreamEvent,
                updated_at_unix_secs: 1,
                ..Default::default()
            },
            RateLimitSnapshot {
                provider_id: "chatgpt".to_string(),
                feature: Some("codex".to_string()),
                source: RateLimitSource::ResponseHeaders,
                updated_at_unix_secs: 2,
                ..Default::default()
            },
        );

        assert_eq!(merged.spend_control_reached, Some(true));
    }

    #[tokio::test]
    async fn stale_usage_refresh_does_not_overwrite_newer_hard_stop() {
        let cache = Arc::new(Mutex::new(CachedRateLimits {
            snapshots: Vec::new(),
            fetched_at: None,
            hard_stop_generation: 0,
        }));
        let expected_generation = cache.lock().await.hard_stop_generation;

        cache_rate_limits_into(
            &cache,
            vec![RateLimitSnapshot {
                provider_id: "chatgpt".to_string(),
                feature: Some("codex".to_string()),
                spend_control_reached: Some(true),
                source: RateLimitSource::StreamEvent,
                updated_at_unix_secs: 2,
                ..Default::default()
            }],
        )
        .await;
        cache_rate_limits_into(
            &cache,
            vec![RateLimitSnapshot {
                provider_id: "chatgpt".to_string(),
                feature: Some("codex".to_string()),
                spend_control_reached: Some(true),
                source: RateLimitSource::StreamEvent,
                updated_at_unix_secs: 3,
                ..Default::default()
            }],
        )
        .await;

        let accepted = cache_rate_limits_if_generation_into(
            &cache,
            vec![RateLimitSnapshot {
                provider_id: "chatgpt".to_string(),
                feature: Some("codex".to_string()),
                spend_control_reached: Some(false),
                source: RateLimitSource::UsageEndpoint,
                updated_at_unix_secs: 1,
                ..Default::default()
            }],
            Some(expected_generation),
        )
        .await;

        assert!(!accepted);
        let cached = cache.lock().await;
        assert_eq!(cached.hard_stop_generation, 2);
        assert_eq!(cached.snapshots[0].spend_control_reached, Some(true));
    }

    #[test]
    fn parses_native_codex_rate_limit_sse_fixture() {
        let fixture = include_str!("../tests/fixtures/chatgpt_codex/stream_rate_limit.sse");
        let event = chatgpt_codex_fixture_sse_events(fixture)
            .into_iter()
            .find(|event| event["type"] == "codex.rate_limits")
            .expect("codex.rate_limits fixture event");

        let snapshot =
            rate_limit_snapshot_from_sse_event("chatgpt", &event, 999).expect("snapshot");

        assert_eq!(snapshot.provider_id, "chatgpt");
        assert_eq!(snapshot.feature.as_deref(), Some("codex"));
        assert_eq!(snapshot.plan_type.as_deref(), Some("plus"));
        assert_eq!(snapshot.primary.as_ref().unwrap().used_percent, 55.5);
        assert_eq!(snapshot.primary.as_ref().unwrap().window_minutes, Some(300));
        assert_eq!(
            snapshot.secondary.as_ref().unwrap().window_minutes,
            Some(10080)
        );
        assert_eq!(
            snapshot.credits.as_ref().unwrap().balance.as_deref(),
            Some("3.25")
        );
        assert_eq!(snapshot.source, RateLimitSource::StreamEvent);
    }

    #[test]
    fn chatgpt_observability_derives_terminal_sse_stop_reason() {
        assert_eq!(
            chatgpt_sse_stop_reason(&json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_1",
                    "model": "gpt-5.3-codex",
                    "status": "completed",
                    "output": [{"type": "message"}]
                }
            })),
            Some("end_turn")
        );
        assert_eq!(
            chatgpt_sse_stop_reason(&json!({
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "output": [{"type": "custom_tool_call"}]
                }
            })),
            Some("tool_use")
        );
        assert_eq!(
            chatgpt_sse_stop_reason(&json!({
                "type": "response.incomplete",
                "response": {
                    "status": "incomplete",
                    "incomplete_details": {"reason": "max_output_tokens"}
                }
            })),
            Some("max_tokens")
        );
        let failed = json!({
            "type": "response.failed",
            "response": {
                "id": "resp_failed",
                "status": "failed",
                "error": {
                    "code": "server_error",
                    "message": "internal error"
                }
            }
        });
        assert_eq!(chatgpt_sse_stop_reason(&failed), Some("error"));
        assert_eq!(chatgpt_sse_response_status(&failed), Some("failed"));
        assert_eq!(chatgpt_sse_error_code(&failed), Some("server_error"));
        assert_eq!(chatgpt_sse_error_message(&failed), Some("internal error"));
        assert!(chatgpt_event_is_server_error(&failed));
        assert!(provider_error_is_chatgpt_server_error(
            &ProviderError::UpstreamError {
                status: 200,
                body: failed.to_string()
            }
        ));
    }

    #[tokio::test]
    async fn chatgpt_stream_context_overflow_notifies_request_observer() {
        let provider = test_chatgpt_provider("http://127.0.0.1:1/responses".to_string()).await;
        let captured = Arc::new(StdMutex::new(Vec::new()));
        let observer_events = Arc::clone(&captured);
        let observer: ProviderRequestObserver = Arc::new(move |event| {
            observer_events.lock().unwrap().push(event);
        });
        let handler = provider.upstream_event_handler(ChatGptUpstreamEventContext {
            request_id: 1,
            compact_request: false,
            transport: "sse",
            first_upstream_event_seen: Arc::new(AtomicBool::new(false)),
            thinking_diagnostics: Arc::new(ChatGptThinkingDiagnostics::default()),
            stream_started_at: Instant::now(),
            observer: Some(observer),
            pending_context_usage: None,
        });

        handler(&json!({
            "type": "error",
            "error": {
                "code": "context_length_exceeded",
                "message": "context limit"
            }
        }));

        assert!(captured.lock().unwrap().iter().any(|event| {
            event
                .request_metadata
                .as_ref()
                .is_some_and(|metadata| metadata.context_upstream_overflow == Some(true))
        }));
    }

    #[test]
    fn chatgpt_server_error_rotates_runtime_ids() {
        let runtime_ids = Arc::new(RwLock::new(ChatGptRuntimeIds {
            session_id: "session-test".to_string(),
            thread_id: "thread-test".to_string(),
            window_id: "window-test".to_string(),
        }));

        rotate_chatgpt_runtime_ids_after_server_error(&runtime_ids, 1, "sse");

        let rotated = runtime_ids.read().unwrap();
        assert_ne!(rotated.session_id, "session-test");
        assert_ne!(rotated.thread_id, "thread-test");
        assert_ne!(rotated.window_id, "window-test");
    }

    #[test]
    fn chatgpt_sse_request_body_uses_zstd_when_available() {
        let json_bytes = br#"{"model":"gpt-5.5","stream":true}"#.to_vec();
        let body = chatgpt_sse_request_body_from_json(json_bytes.clone(), |bytes| {
            zstd::bulk::compress(bytes, CHATGPT_SSE_REQUEST_ZSTD_LEVEL)
        });

        assert_eq!(body.content_encoding, Some("zstd"));
        assert_eq!(body.original_len, json_bytes.len());
        let decoded = zstd::stream::decode_all(body.bytes.as_slice()).unwrap();
        assert_eq!(decoded, json_bytes);
    }

    #[test]
    fn chatgpt_sse_request_body_falls_back_when_zstd_fails() {
        let json_bytes = br#"{"model":"gpt-5.5","stream":true}"#.to_vec();
        let body = chatgpt_sse_request_body_from_json(json_bytes.clone(), |_| Err("boom"));

        assert_eq!(body.content_encoding, None);
        assert_eq!(body.original_len, json_bytes.len());
        assert_eq!(body.bytes, json_bytes);
    }

    #[tokio::test]
    async fn chatgpt_auto_transport_uses_sse_during_server_error_cooldown() {
        let mut provider = test_chatgpt_provider("http://127.0.0.1/responses".to_string()).await;
        provider.transport = ChatGptTransport::Auto;
        assert_eq!(provider.effective_transport(), ChatGptTransport::Auto);

        ChatGptProvider::activate_websocket_sse_cooldown(
            &provider.websocket_sse_cooldown_until_secs,
            1,
            "websocket",
        );

        assert_eq!(provider.effective_transport(), ChatGptTransport::Sse);
    }

    #[test]
    fn chatgpt_request_size_warning_uses_model_context_metadata() {
        let config = ChatGptProviderConfig::default();
        let threshold = chatgpt_request_warning_threshold("gpt-5.6-sol", &config).unwrap();

        assert_eq!(
            threshold,
            872_000 * CHATGPT_BYTES_PER_ESTIMATED_TOKEN * 80 / 100
        );
        assert!(request_size_warning("gpt-5.6-sol", &config, threshold - 1).is_none());
        assert_eq!(
            request_size_warning("gpt-5.6-sol", &config, threshold),
            Some((threshold, threshold / CHATGPT_BYTES_PER_ESTIMATED_TOKEN))
        );
        assert!(request_size_warning("unknown-model", &config, threshold).is_none());
    }

    #[tokio::test]
    async fn chatgpt_responses_lite_decision_uses_policy_and_capabilities() {
        let provider = test_chatgpt_provider("http://127.0.0.1/responses".to_string()).await;

        let gpt55 = provider.responses_lite_decision("gpt-5.5");
        assert!(!gpt55.is_enabled());
        assert_eq!(gpt55.source_str(), "unknown_model");

        let routed_gpt55 = provider.responses_lite_decision("chatgpt/gpt-5.5");
        assert!(!routed_gpt55.is_enabled());
        assert_eq!(routed_gpt55.source_str(), "unknown_model");

        let gpt54 = provider.responses_lite_decision("gpt-5.4");
        assert!(!gpt54.is_enabled());
        assert_eq!(gpt54.source_str(), "unknown_model");

        let sol = provider.responses_lite_decision("chatgpt/gpt-5.6-sol");
        assert!(sol.is_enabled());
        assert_eq!(sol.source_str(), "model_capability");

        let spark = provider.responses_lite_decision("gpt-5.3-codex-spark");
        assert!(!spark.is_enabled());
        assert_eq!(spark.source_str(), "unknown_model");

        let unknown = provider.responses_lite_decision("unknown-model");
        assert!(!unknown.is_enabled());
        assert_eq!(unknown.source_str(), "unknown_model");
    }

    #[tokio::test]
    async fn chatgpt_responses_lite_decision_respects_forced_modes_and_overrides() {
        let mut provider = test_chatgpt_provider("http://127.0.0.1/responses".to_string()).await;

        provider.chatgpt_config.responses_lite = ResponsesLiteMode::On;
        let forced_on = provider.responses_lite_decision("gpt-5.3-codex-spark");
        assert!(forced_on.is_enabled());
        assert_eq!(forced_on.source_str(), "forced_on");

        provider.chatgpt_config.responses_lite = ResponsesLiteMode::Off;
        let forced_off = provider.responses_lite_decision("gpt-5.5");
        assert!(!forced_off.is_enabled());
        assert_eq!(forced_off.source_str(), "forced_off");

        provider.chatgpt_config.responses_lite = ResponsesLiteMode::Auto;
        provider.chatgpt_config.model_capabilities.insert(
            "gpt-5.3-codex-spark".to_string(),
            claude_proxy_config::settings::ChatGptModelCapabilityOverride {
                responses_lite: Some(true),
                ..Default::default()
            },
        );
        let override_enabled = provider.responses_lite_decision("gpt-5.3-codex-spark");
        assert!(override_enabled.is_enabled());
        assert_eq!(override_enabled.source_str(), "override_enabled");

        provider.chatgpt_config.model_capabilities.insert(
            "gpt-5.5".to_string(),
            claude_proxy_config::settings::ChatGptModelCapabilityOverride {
                responses_lite: Some(false),
                ..Default::default()
            },
        );
        let override_disabled = provider.responses_lite_decision("gpt-5.5");
        assert!(!override_disabled.is_enabled());
        assert_eq!(override_disabled.source_str(), "override_disabled");
    }

    #[test]
    fn chatgpt_models_use_dedicated_codex_capability_contract() {
        let models = chatgpt_models(&ChatGptProviderConfig::default());
        let ids = models
            .iter()
            .map(|model| model.model_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"]);
        assert!(chatgpt_model_info("gpt-5.4", &ChatGptProviderConfig::default()).is_none());
        assert!(chatgpt_model_info("gpt-5.4-mini", &ChatGptProviderConfig::default()).is_none());

        let sol = models
            .iter()
            .find(|model| model.model_id == "gpt-5.6-sol")
            .expect("gpt-5.6-sol model");
        let terra = models
            .iter()
            .find(|model| model.model_id == "gpt-5.6-terra")
            .expect("gpt-5.6-terra model");
        let luna = models
            .iter()
            .find(|model| model.model_id == "gpt-5.6-luna")
            .expect("gpt-5.6-luna model");
        for model in [sol, terra, luna] {
            assert_eq!(
                model.capabilities.limits.context_window,
                Some(CHATGPT_GPT_56_DEFAULT_CONTEXT_WINDOW)
            );
            assert!(model.capabilities.modalities.input.image.is_supported());
            let responses = model
                .capabilities
                .responses
                .as_ref()
                .expect("Responses capability details");
            assert_eq!(responses.streaming, ResponsesStreamingMode::Required);
            assert_eq!(responses.stateful, CapabilityState::Unsupported);
            assert_eq!(responses.storage, CapabilityState::Unsupported);
            assert_eq!(
                responses.input_formats,
                vec![ResponsesInputFormat::String, ResponsesInputFormat::Items]
            );
            assert_eq!(responses.unsupported_parameters, vec!["max_output_tokens"]);
        }
        assert_eq!(
            sol.capabilities.limits.reasoning_effort_levels,
            vec!["low", "medium", "high", "xhigh", "max", "ultra"]
        );
        assert_eq!(
            terra.capabilities.limits.reasoning_effort_levels,
            vec!["low", "medium", "high", "xhigh", "max", "ultra"]
        );
        assert_eq!(
            luna.capabilities.limits.reasoning_effort_levels,
            vec!["low", "medium", "high", "xhigh", "max"]
        );
    }

    #[test]
    fn chatgpt_models_apply_configured_capability_overrides() {
        let mut config = ChatGptProviderConfig::default();
        config.model_capabilities.insert(
            "gpt-5.5".to_string(),
            ChatGptModelCapabilityOverride {
                context_window: Some(400_000),
                image_input: Some(false),
                reasoning_effort_levels: Some(vec![
                    "minimal".to_string(),
                    "medium".to_string(),
                    "xhigh".to_string(),
                ]),
                ..Default::default()
            },
        );
        config.model_capabilities.insert(
            "gpt-5.6-codex".to_string(),
            ChatGptModelCapabilityOverride {
                context_window: Some(512_000),
                image_input: Some(true),
                reasoning_effort_levels: Some(vec!["low".to_string(), "ultra".to_string()]),
                responses_lite: Some(true),
            },
        );

        let models = chatgpt_models(&config);
        let gpt55 = models
            .iter()
            .find(|model| model.model_id == "gpt-5.5")
            .expect("overridden gpt-5.5 model");
        assert_eq!(gpt55.capabilities.limits.context_window, Some(400_000));
        assert_eq!(
            gpt55.capabilities.modalities.input.image,
            CapabilityState::Unsupported
        );
        assert_eq!(
            gpt55.capabilities.limits.reasoning_effort_levels,
            vec![
                "minimal".to_string(),
                "medium".to_string(),
                "xhigh".to_string()
            ]
        );

        let new_model = models
            .iter()
            .find(|model| model.model_id == "gpt-5.6-codex")
            .expect("configured custom model");
        assert_eq!(new_model.capabilities.limits.context_window, Some(512_000));
        assert!(new_model.capabilities.modalities.input.image.is_supported());
        assert_eq!(
            new_model.capabilities.limits.reasoning_effort_levels,
            vec!["low".to_string(), "ultra".to_string()]
        );
    }

    #[test]
    fn remote_chatgpt_catalog_uses_codex_metadata_and_config_overrides() {
        let remote: ChatGptRemoteModel = serde_json::from_value(json!({
            "slug": "gpt-5.6-sol",
            "supported_reasoning_levels": [
                {"effort": "low"},
                {"effort": "medium"},
                {"effort": "ultra"}
            ],
            "context_window": 372000,
            "max_context_window": 400000,
            "input_modalities": ["text", "image"],
            "use_responses_lite": true,
            "supports_parallel_tool_calls": false,
            "auto_compact_token_limit": 320000,
            "effective_context_window_percent": 85,
            "default_reasoning_level": "medium",
            "default_reasoning_summary": "auto",
            "support_verbosity": true,
            "default_verbosity": "medium",
            "visibility": "list",
            "priority": 1
        }))
        .unwrap();
        let mut config = ChatGptProviderConfig::default();
        config.model_capabilities.insert(
            "gpt-5.6-sol".to_string(),
            ChatGptModelCapabilityOverride {
                context_window: Some(500_000),
                image_input: Some(false),
                reasoning_effort_levels: Some(vec!["high".to_string(), "ultra".to_string()]),
                responses_lite: Some(false),
            },
        );

        let catalog = chatgpt_catalog_model_from_remote(remote, &config);

        assert_eq!(catalog.info.model_id, "gpt-5.6-sol");
        assert_eq!(
            catalog.info.capabilities.limits.context_window,
            Some(500_000)
        );
        assert_eq!(
            catalog.info.capabilities.modalities.input.image,
            CapabilityState::Unsupported
        );
        assert_eq!(
            catalog.info.capabilities.limits.reasoning_effort_levels,
            vec!["high", "ultra"]
        );
        assert!(!catalog.responses_lite);
        assert!(!catalog.supports_parallel_tool_calls);
        assert_eq!(catalog.auto_compact_token_limit, Some(320_000));
        assert_eq!(catalog.effective_context_window_percent, Some(85));
        assert_eq!(catalog.visibility.as_deref(), Some("list"));
        assert_eq!(catalog.priority, 1);
    }

    #[test]
    fn remote_gpt_56_catalog_defaults_to_supported_max_context_window() {
        let remote: ChatGptRemoteModel = serde_json::from_value(json!({
            "slug": "gpt-5.6-sol",
            "context_window": 272000,
            "max_context_window": 872000
        }))
        .unwrap();

        let catalog = chatgpt_catalog_model_from_remote(remote, &ChatGptProviderConfig::default());

        assert_eq!(
            catalog.info.capabilities.limits.context_window,
            Some(CHATGPT_GPT_56_DEFAULT_CONTEXT_WINDOW)
        );

        let remote: ChatGptRemoteModel = serde_json::from_value(json!({
            "slug": "gpt-5.5",
            "context_window": 272000,
            "max_context_window": 872000
        }))
        .unwrap();
        let catalog = chatgpt_catalog_model_from_remote(remote, &ChatGptProviderConfig::default());
        assert_eq!(
            catalog.info.capabilities.limits.context_window,
            Some(272_000)
        );
    }

    #[test]
    fn remote_chatgpt_catalog_preserves_service_tier_capability_state() {
        let catalog = |service_tiers: Option<Value>| {
            let mut value = json!({"slug": "gpt-test"});
            if let Some(service_tiers) = service_tiers {
                value["service_tiers"] = service_tiers;
            }
            let remote: ChatGptRemoteModel = serde_json::from_value(value).unwrap();
            chatgpt_catalog_model_from_remote(remote, &ChatGptProviderConfig::default())
        };

        assert_eq!(catalog(None).service_tiers, None);
        assert_eq!(catalog(Some(json!([]))).service_tiers, Some(Vec::new()));
        assert_eq!(
            catalog(Some(json!([{"id": "priority", "name": "Fast"}]))).service_tiers,
            Some(vec!["priority".to_string()])
        );
    }

    #[test]
    fn remote_chatgpt_catalog_preserves_reasoning_summary_capability() {
        let catalog = |supports_reasoning_summary_parameter: Option<bool>| {
            let mut value = json!({"slug": "gpt-test"});
            if let Some(supported) = supports_reasoning_summary_parameter {
                value["supports_reasoning_summary_parameter"] = json!(supported);
            }
            let remote: ChatGptRemoteModel = serde_json::from_value(value).unwrap();
            chatgpt_catalog_model_from_remote(remote, &ChatGptProviderConfig::default())
        };

        assert!(catalog(None).supports_reasoning_summary_parameter);
        assert!(catalog(Some(true)).supports_reasoning_summary_parameter);
        assert!(!catalog(Some(false)).supports_reasoning_summary_parameter);
    }

    #[test]
    fn chatgpt_ultra_maps_to_max_and_enables_proactive_instructions_with_delegate_tool() {
        let mut request = MessagesRequest {
            model: "gpt-5.6-sol".to_string(),
            system: None,
            messages: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: Some(vec![Tool {
                name: "spawn_agent".to_string(),
                description: None,
                input_schema: json!({"type": "object"}),
                extra: Default::default(),
            }]),
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: HashMap::from([("reasoning_effort".to_string(), json!("ultra"))]),
        };

        assert!(normalize_chatgpt_56_reasoning(&mut request));
        assert!(request_has_delegation_tool(&request));
        assert_eq!(request.extra["reasoning_effort"], "max");

        let model = chatgpt_model_info("gpt-5.6-sol", &ChatGptProviderConfig::default()).unwrap();
        let body = build_chatgpt_responses_body_with_codex_context(
            &request,
            responses::CodexRequestContext {
                model: Some(&model),
                additional_instructions: Some(PROACTIVE_MULTI_AGENT_INSTRUCTIONS),
                ..Default::default()
            },
        );
        assert_eq!(body["reasoning"]["effort"], "max");
        assert!(
            body["instructions"]
                .as_str()
                .unwrap()
                .contains("Proactive multi-agent delegation is active")
        );
    }

    #[test]
    fn chatgpt_ultra_without_delegate_or_model_support_stays_max_only() {
        let mut request = MessagesRequest {
            model: "gpt-5.6-luna".to_string(),
            system: None,
            messages: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: HashMap::from([("reasoning".to_string(), json!({"effort": "ultra"}))]),
        };

        assert!(normalize_chatgpt_56_reasoning(&mut request));
        assert!(!request_has_delegation_tool(&request));
        assert_eq!(request.extra["reasoning"]["effort"], "max");
        let luna = chatgpt_model_info("gpt-5.6-luna", &ChatGptProviderConfig::default()).unwrap();
        assert!(
            !luna
                .capabilities
                .limits
                .reasoning_effort_levels
                .contains(&"ultra".to_string())
        );
    }

    #[test]
    fn chatgpt_56_maps_unsupported_none_minimal_and_disabled_to_low() {
        let mut request = MessagesRequest {
            model: "gpt-5.6-terra".to_string(),
            system: None,
            messages: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: Some(ThinkingConfig {
                r#type: Some("disabled".to_string()),
                budget_tokens: None,
            }),
            metadata: None,
            extra: HashMap::from([("reasoning".to_string(), json!({"effort": "minimal"}))]),
        };

        assert!(!normalize_chatgpt_56_reasoning(&mut request));
        assert_eq!(request.extra["reasoning"]["effort"], "low");
        assert_eq!(request.extra["reasoning_effort"], "low");
        assert!(request.thinking.is_none());
    }

    #[test]
    fn chatgpt_native_56_normalizes_only_legacy_effort_values() {
        for (input, expected) in [
            ("minimal", "low"),
            ("none", "low"),
            ("ultra", "max"),
            ("disabled", "disabled"),
            ("high", "high"),
        ] {
            let mut body = json!({"reasoning": {"effort": input, "summary": "auto"}});
            normalize_chatgpt_native_56_reasoning(&mut body, "gpt-5.6-sol");
            assert_eq!(body["reasoning"]["effort"], expected);
            assert_eq!(body["reasoning"]["summary"], "auto");
        }
    }

    #[test]
    fn chatgpt_native_responses_normalizes_public_api_shape_for_lite_backend() {
        let mut body = serde_json::json!({
            "model": "gpt-5.6-terra",
            "input": "Return JSON",
            "stream": true,
            "max_output_tokens": 2_000,
            "parallel_tool_calls": true,
            "reasoning": {"effort": "high", "context": "previous_response"},
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "result",
                    "schema": {"type": "object"}
                }
            }
        });

        let budget = normalize_chatgpt_native_responses_body(&mut body, true);

        assert_eq!(
            budget,
            ChatGptOutputTokenBudget {
                requested: Some(2_000),
                effective: None,
            }
        );
        assert_eq!(
            body["input"],
            json!([{"role": "user", "content": "Return JSON"}])
        );
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["context"], "all_turns");
        assert_eq!(body["text"]["format"]["type"], "json_schema");
    }

    #[test]
    fn chatgpt_native_responses_preserves_item_input_and_non_lite_fields() {
        let input = json!([
            {"type": "additional_tools", "role": "developer", "tools": []},
            {"role": "user", "content": "hello"}
        ]);
        let mut body = json!({
            "model": "gpt-5.5",
            "input": input,
            "stream": true,
            "parallel_tool_calls": true,
            "reasoning": {"context": "previous_response"}
        });

        let budget = normalize_chatgpt_native_responses_body(&mut body, false);

        assert_eq!(budget, ChatGptOutputTokenBudget::default());
        assert_eq!(body["input"], input);
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["reasoning"]["context"], "previous_response");
    }

    #[test]
    fn codex_models_endpoint_tracks_custom_codex_base() {
        assert_eq!(
            codex_models_endpoint("https://chatgpt.com/backend-api/codex/responses"),
            "https://chatgpt.com/backend-api/codex/models"
        );
        assert_eq!(
            codex_models_endpoint("http://127.0.0.1:8080/api/codex"),
            "http://127.0.0.1:8080/api/codex/models"
        );
    }

    #[test]
    fn chatgpt_request_policy_uses_http_client_timeout_by_default() {
        let policy = chatgpt_upstream_request_policy(&ProviderRuntimeConfig::default());

        assert_eq!(policy.max_attempts, 2);
        assert_eq!(policy.attempt_timeout, None);
        assert!(!policy.retry_rate_limits);
    }

    #[test]
    fn chatgpt_request_policy_allows_runtime_overrides() {
        let runtime = ProviderRuntimeConfig {
            retry: claude_proxy_config::settings::ProviderRetryConfig {
                max_attempts: Some(4),
                ..Default::default()
            },
            request: claude_proxy_config::settings::ProviderRequestConfig {
                attempt_timeout_seconds: Some(20),
                ..Default::default()
            },
            ..Default::default()
        };
        let policy = chatgpt_upstream_request_policy(&runtime);

        assert_eq!(policy.max_attempts, 4);
        assert_eq!(policy.connection_max_attempts, 4);
        assert_eq!(policy.attempt_timeout, Some(Duration::from_secs(20)));
        assert!(!policy.retry_rate_limits);
    }

    #[test]
    fn chatgpt_request_headers_use_configured_values_and_default_empty_values() {
        let config = claude_proxy_config::settings::ChatGptProviderConfig {
            originator: "codex_cli".to_string(),
            user_agent: "CodexCLI/1.2.3".to_string(),
            ..Default::default()
        };

        let headers = chatgpt_request_headers(&config).unwrap();
        assert_eq!(headers.originator.to_str().unwrap(), "codex_cli");
        assert_eq!(headers.user_agent.to_str().unwrap(), "CodexCLI/1.2.3");

        let config = claude_proxy_config::settings::ChatGptProviderConfig {
            originator: "  ".to_string(),
            user_agent: "\t".to_string(),
            ..Default::default()
        };

        let headers = chatgpt_request_headers(&config).unwrap();
        assert_eq!(headers.originator.to_str().unwrap(), "codex_cli_rs");
        assert_eq!(
            headers.user_agent.to_str().unwrap(),
            "codex_cli_rs/1.0.0 (claude-proxy)"
        );
    }

    #[test]
    fn parses_local_codex_cli_version_output() {
        assert_eq!(
            parse_codex_cli_version("codex-cli 0.130.0"),
            Some("0.130.0".to_string())
        );
        assert_eq!(
            parse_codex_cli_version("codex v0.130.0"),
            Some("0.130.0".to_string())
        );
        assert_eq!(parse_codex_cli_version("codex-cli dev"), None);
    }

    #[test]
    fn chatgpt_request_headers_match_native_codex_fixture() {
        let expected: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/chatgpt_codex/native_request_headers.json"
        ))
        .expect("valid native headers fixture");
        let config = claude_proxy_config::settings::ChatGptProviderConfig::default();

        let headers = chatgpt_request_headers(&config).unwrap();
        let actual = json!({
            "originator": headers.originator.to_str().unwrap(),
            "user_agent": headers.user_agent.to_str().unwrap(),
        });

        assert_eq!(actual, expected);
    }

    #[test]
    fn chatgpt_responses_body_adds_default_instructions() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: Some(4096),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: false,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_body(&req);

        assert_eq!(body["instructions"], DEFAULT_CHATGPT_INSTRUCTIONS);
        assert_eq!(body["stream"], true);
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn chatgpt_responses_body_adds_codex_request_defaults() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: false,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_lite_body(&req);

        assert!(body.get("tools").is_none());
        assert!(body.get("instructions").is_none());
        assert_eq!(body["input"][0]["type"], "additional_tools");
        assert_eq!(body["input"][0]["tools"], json!([]));
        assert_eq!(
            body["input"][1]["content"][0]["text"],
            DEFAULT_CHATGPT_INSTRUCTIONS
        );
        assert_eq!(body["input"][1]["role"], "developer");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["reasoning"], json!({"context": "all_turns"}));
    }

    #[test]
    fn chatgpt_responses_body_keeps_non_lite_parallel_tool_defaults() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: false,
            tools: Some(vec![Tool {
                name: "Read".to_string(),
                description: None,
                input_schema: json!({"type": "object", "properties": {}}),
                extra: Default::default(),
            }]),
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_body_with_codex_context(
            &req,
            responses::CodexRequestContext {
                responses_lite: false,
                ..responses::CodexRequestContext::default()
            },
        );

        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["reasoning"], json!({}));
    }

    #[test]
    fn chatgpt_responses_body_honors_reasoning_summary_capability() {
        let mut extra = HashMap::new();
        extra.insert(
            "reasoning".to_string(),
            json!({"effort": "medium", "summary": "detailed"}),
        );
        let req = MessagesRequest {
            model: "gpt-5.3-codex-spark".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: false,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra,
        };

        let supported = build_chatgpt_responses_body_with_codex_context(
            &req,
            responses::CodexRequestContext {
                supports_reasoning_summary_parameter: true,
                ..responses::CodexRequestContext::default()
            },
        );
        let unsupported = build_chatgpt_responses_body_with_codex_context(
            &req,
            responses::CodexRequestContext {
                supports_reasoning_summary_parameter: false,
                ..responses::CodexRequestContext::default()
            },
        );

        assert_eq!(
            supported["reasoning"],
            json!({"effort": "medium", "summary": "detailed"})
        );
        assert_eq!(unsupported["reasoning"], json!({"effort": "medium"}));
    }

    #[test]
    fn chatgpt_responses_body_omits_unsupported_stop_parameter() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: Some(vec!["</stop>".to_string()]),
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_body(&req);

        assert!(body.get("stop").is_none());
    }

    #[test]
    fn chatgpt_responses_body_adds_codex_metadata_from_stable_sources() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: Some(json!({
                "prompt_cache_key": "thread-123",
                "client_metadata": {
                    "x-codex-window-id": "window-123",
                    "x-client-only": "ignored"
                }
            })),
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_body_with_context(&req, Some("install-123"));

        assert_eq!(body["prompt_cache_key"], "thread-123");
        let client_metadata = body["client_metadata"].as_object().unwrap();
        assert_eq!(
            client_metadata.get("x-codex-installation-id"),
            Some(&json!("install-123"))
        );
        assert_eq!(client_metadata.len(), 2);
        let turn_metadata: Value =
            serde_json::from_str(client_metadata["x-codex-turn-metadata"].as_str().unwrap())
                .unwrap();
        assert_eq!(turn_metadata["request_kind"], "turn");
    }

    #[test]
    fn chatgpt_responses_body_adds_codex_request_options() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("parallel_tool_calls".to_string(), json!(false));
        extra.insert("verbosity".to_string(), json!("high"));
        extra.insert("service_tier".to_string(), json!("priority"));
        extra.insert(
            "stream_options".to_string(),
            json!({"reasoning_summary_delivery": "sequential_cutoff"}),
        );
        extra.insert(
            "client_metadata".to_string(),
            json!({
                "x-codex-turn-metadata": "{\"turn_id\":\"turn-1\"}",
                "x-codex-installation-id": "client-value"
            }),
        );
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: Some(vec![Tool {
                name: "Read".to_string(),
                description: None,
                input_schema: json!({"type": "object", "properties": {}}),
                extra: Default::default(),
            }]),
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra,
        };

        let body = build_chatgpt_responses_body_with_codex_context(
            &req,
            responses::CodexRequestContext {
                installation_id: Some("proxy-installation"),
                service_tier: Some("flex"),
                standalone_tools: true,
                responses_lite: true,
                model: None,
                additional_instructions: None,
                supports_reasoning_summary_parameter: true,
                ..responses::CodexRequestContext::default()
            },
        );

        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["text"], json!({"verbosity": "high"}));
        assert_eq!(
            body["stream_options"],
            json!({"reasoning_summary_delivery": "sequential_cutoff"})
        );
        let turn_metadata: Value = serde_json::from_str(
            body["client_metadata"]["x-codex-turn-metadata"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(turn_metadata["installation_id"], "proxy-installation");
        assert_eq!(turn_metadata["turn_id"], "turn-1");
        assert_eq!(turn_metadata["request_kind"], "turn");
        assert_eq!(
            body["client_metadata"]["x-codex-installation-id"],
            "proxy-installation"
        );
    }

    #[test]
    fn chatgpt_responses_body_uses_stable_prompt_cache_sources() {
        let long_key = "界".repeat(70);
        let expected_key = "界".repeat(64);
        let mut extra = std::collections::HashMap::new();
        extra.insert("prompt_cache_key".to_string(), json!(long_key));
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: Some(json!({"prompt_cache_key": "metadata-key"})),
            extra,
        };

        let body = build_chatgpt_responses_body(&req);

        assert_eq!(body["prompt_cache_key"], expected_key);
        assert_eq!(
            responses::prompt_cache_key_source(&req),
            responses::PromptCacheKeySource::Explicit
        );
    }

    #[test]
    fn chatgpt_responses_body_uses_stable_conversation_id_as_prompt_cache_key() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: Some(json!({"conversation_id": "conversation-123"})),
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_body(&req);

        assert_eq!(body["prompt_cache_key"], "conversation-123");
        assert_eq!(
            responses::prompt_cache_key_source(&req),
            responses::PromptCacheKeySource::StableClientConversation
        );
    }

    #[test]
    fn chatgpt_prompt_cache_prefers_session_without_changing_continuation_scope() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: Some(json!({
                "thread_id": "thread-123",
                "session_id": "session-456"
            })),
            extra: Default::default(),
        };

        assert_eq!(
            build_chatgpt_responses_body(&req)["prompt_cache_key"],
            "session-456"
        );
        assert_eq!(
            responses::stable_client_conversation_id_for_continuation(&req).as_deref(),
            Some("thread-123")
        );
    }

    #[test]
    fn chatgpt_continuation_stable_conversation_id_excludes_explicit_prompt_cache_key() {
        let mut explicit_extra = std::collections::HashMap::new();
        explicit_extra.insert("prompt_cache_key".to_string(), json!("cache-only"));
        let explicit = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: explicit_extra,
        };
        assert_eq!(
            responses::stable_client_conversation_id_for_continuation(&explicit),
            None
        );

        let stable = MessagesRequest {
            metadata: Some(json!({"conversation_id": "conversation-123"})),
            ..explicit
        };
        assert_eq!(
            responses::stable_client_conversation_id_for_continuation(&stable).as_deref(),
            Some("conversation-123")
        );
    }

    #[test]
    fn chatgpt_synthesizes_stable_conversation_id_when_missing() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: Some(SystemPrompt::Text("system".to_string())),
            messages: vec![
                Message {
                    role: Role::User,
                    content: MessageContent::Text("first task".to_string()),
                },
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Text("working".to_string()),
                },
            ],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: Default::default(),
        };

        let (req, synthesized) = ensure_chatgpt_stable_client_conversation_id(req);
        let session_id = req
            .extra
            .get("client_session_id")
            .and_then(Value::as_str)
            .expect("synthetic session id");

        assert!(synthesized);
        assert!(session_id.starts_with("cp-synth-"));
        assert_eq!(session_id.len(), "cp-synth-".len() + 32);
        assert_eq!(
            responses::stable_client_conversation_id_for_continuation(&req).as_deref(),
            Some(session_id)
        );
        assert_eq!(
            build_chatgpt_responses_body(&req)["prompt_cache_key"],
            session_id
        );
    }

    #[test]
    fn chatgpt_preserves_existing_stable_conversation_id() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("client_session_id".to_string(), json!("explicit-session"));
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("first task".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra,
        };

        let (req, synthesized) = ensure_chatgpt_stable_client_conversation_id(req);

        assert!(!synthesized);
        assert_eq!(
            req.extra.get("client_session_id").and_then(Value::as_str),
            Some("explicit-session")
        );
    }

    #[test]
    fn chatgpt_responses_body_adds_codex_runtime_context() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_body_with_codex_context(
            &req,
            responses::CodexRequestContext {
                installation_id: Some("install-123"),
                service_tier: Some("priority"),
                standalone_tools: true,
                responses_lite: true,
                model: None,
                additional_instructions: None,
                supports_reasoning_summary_parameter: true,
                ..responses::CodexRequestContext::default()
            },
        );

        assert!(body.get("prompt_cache_key").is_none());
        assert_eq!(body["service_tier"], "priority");
        let client_metadata = body["client_metadata"].as_object().unwrap();
        assert_eq!(
            client_metadata.get("x-codex-installation-id"),
            Some(&json!("install-123"))
        );
        assert_eq!(client_metadata.len(), 2);
    }

    #[test]
    fn chatgpt_responses_body_converts_bash_to_custom_tool_by_default() {
        let req = MessagesRequest {
            model: "gpt-5.3-codex".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("List changed files.".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: Some(vec![
                Tool {
                    name: "Bash".to_string(),
                    description: Some("Run a shell command".to_string()),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "command": {"type": "string"},
                            "description": {"type": "string"}
                        },
                        "required": ["command"]
                    }),
                    extra: Default::default(),
                },
                Tool {
                    name: "Read".to_string(),
                    description: Some("Read a file".to_string()),
                    input_schema: json!({
                        "type": "object",
                        "properties": {"file_path": {"type": "string"}},
                        "required": ["file_path"]
                    }),
                    extra: Default::default(),
                },
            ]),
            tool_choice: Some(json!({"type": "tool", "name": "Bash"})),
            thinking: None,
            metadata: None,
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_body(&req);

        assert_eq!(body["tools"][0]["type"], "custom");
        assert_eq!(body["tools"][0]["name"], "Bash");
        assert_eq!(body["tools"][0]["format"]["syntax"], "lark");
        assert_eq!(body["tools"][1]["type"], "function");
        assert_eq!(body["tools"][1]["name"], "Read");
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn chatgpt_responses_body_can_disable_standalone_tools() {
        let req = MessagesRequest {
            model: "gpt-5.3-codex".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("List changed files.".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: Some(vec![Tool {
                name: "Bash".to_string(),
                description: Some("Run a shell command".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "description": {"type": "string"}
                    },
                    "required": ["command"]
                }),
                extra: Default::default(),
            }]),
            tool_choice: Some(json!({"type": "tool", "name": "Bash"})),
            thinking: None,
            metadata: None,
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_body_with_codex_context(
            &req,
            responses::CodexRequestContext {
                standalone_tools: false,
                ..responses::CodexRequestContext::default()
            },
        );

        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "Bash");
        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "name": "Bash"})
        );
    }

    #[test]
    fn chatgpt_responses_body_matches_native_codex_fixture_shape() {
        let expected: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/chatgpt_codex/native_request_body.json"
        ))
        .expect("valid native body fixture");
        let req = MessagesRequest {
            model: "gpt-5.3-codex".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("List changed files.".to_string()),
            }],
            max_tokens: Some(4096),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: false,
            tools: Some(vec![Tool {
                name: "Bash".to_string(),
                description: Some("Run a shell command".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"},
                        "description": {"type": "string"}
                    },
                    "required": ["command"]
                }),
                extra: Default::default(),
            }]),
            tool_choice: None,
            thinking: None,
            metadata: Some(json!({
                "prompt_cache_key": "thread-fixture",
                "client_metadata": {
                    "x-codex-window-id": "window-from-request"
                }
            })),
            extra: Default::default(),
        };

        let actual = build_chatgpt_responses_body_with_codex_context(
            &req,
            responses::CodexRequestContext {
                installation_id: Some("install-fixture"),
                service_tier: None,
                standalone_tools: true,
                responses_lite: true,
                model: None,
                additional_instructions: None,
                supports_reasoning_summary_parameter: true,
                ..responses::CodexRequestContext::default()
            },
        );

        assert_eq!(actual, expected);
    }

    #[test]
    fn chatgpt_responses_body_preserves_system_instructions() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: Some(SystemPrompt::Text("Use terse answers.".to_string())),
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_body(&req);

        assert_eq!(body["instructions"], "Use terse answers.");
    }

    #[test]
    fn chatgpt_responses_body_normalizes_tool_schema_for_codex() {
        let req = MessagesRequest {
            model: "gpt-5.3-codex".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("read the file".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: Some(vec![Tool {
                name: "Read".to_string(),
                description: Some("Read a file".to_string()),
                input_schema: json!({
                    "type": "object",
                    "properties": {"file_path": {"type": "string"}},
                    "required": "file_path"
                }),
                extra: Default::default(),
            }]),
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_lite_body(&req);

        let tool = &body["input"][0]["tools"][0]["tools"][0];
        assert_eq!(body["input"][0]["tools"][0]["type"], "namespace");
        assert_eq!(body["input"][0]["tools"][0]["name"], "functions");
        assert_eq!(tool["type"], "function");
        assert_eq!(tool["name"], "Read");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(
            tool["parameters"],
            json!({
                "type": "object",
                "properties": {"file_path": {"type": "string"}}
            })
        );
    }

    #[test]
    fn chatgpt_responses_body_preserves_tool_history_shape() {
        let req = MessagesRequest {
            model: "gpt-5.3-codex".to_string(),
            system: None,
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(vec![Content::ToolUse {
                        id: "call_1".to_string(),
                        name: "Read".to_string(),
                        input: json!({"file_path": "README.md"}),
                    }]),
                },
                Message {
                    role: Role::User,
                    content: MessageContent::Blocks(vec![Content::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        content: Some(Value::String("done".to_string())),
                        is_error: None,
                    }]),
                },
            ],
            max_tokens: Some(4096),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: false,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_body(&req);

        assert_eq!(body["stream"], true);
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][0]["call_id"], "call_1");
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][1]["call_id"], "call_1");
        assert_eq!(body["input"][1]["output"], "done");
    }

    #[test]
    fn chatgpt_responses_body_preserves_explicit_reasoning_effort_summary() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("reasoning_effort".to_string(), json!("xhigh"));
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
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
            extra,
        };

        let body = build_chatgpt_responses_lite_body(&req);

        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["reasoning"]["summary"], "detailed");
        assert_eq!(body["reasoning"]["context"], "all_turns");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn chatgpt_responses_body_defaults_adaptive_reasoning_summary_to_auto() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: Some(4096),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: Some(ThinkingConfig {
                r#type: Some("adaptive".to_string()),
                budget_tokens: None,
            }),
            metadata: None,
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_lite_body(&req);

        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["reasoning"]["context"], "all_turns");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn chatgpt_responses_body_forces_reasoning_context_for_explicit_reasoning() {
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "reasoning".to_string(),
            json!({"effort": "high", "summary": "auto", "context": "previous_response"}),
        );
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
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
            extra,
        };

        let body = build_chatgpt_responses_lite_body(&req);

        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["reasoning"]["context"], "all_turns");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn chatgpt_responses_body_keeps_non_lite_reasoning_context() {
        let mut extra = std::collections::HashMap::new();
        extra.insert(
            "reasoning".to_string(),
            json!({"effort": "high", "summary": "auto", "context": "previous_response"}),
        );
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
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
            extra,
        };

        let body = build_chatgpt_responses_body(&req);

        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "auto");
        assert_eq!(body["reasoning"]["context"], "previous_response");
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    }

    #[test]
    fn chatgpt_intent_fast_affects_responses_body() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: Some(4096),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: Some(json!({"intent": "fast"})),
            extra: Default::default(),
        };

        let req = apply_openai_intent(req);
        let body = build_chatgpt_responses_lite_body(&req);

        assert_eq!(body["model"], "gpt-5.4-mini");
        assert_eq!(
            body["input"][1]["content"][0]["text"],
            DEFAULT_CHATGPT_INSTRUCTIONS
        );
        assert_eq!(body["reasoning"]["effort"], "none");
        assert_eq!(body["reasoning"]["context"], "all_turns");
        assert!(body["reasoning"].get("summary").is_none());
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn chatgpt_responses_body_omits_max_output_tokens_for_codex_backend() {
        let req = MessagesRequest {
            model: "gpt-5.4-mini".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
            }],
            max_tokens: Some(128_000),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_body(&req);

        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(
            chatgpt_output_token_budget(&req, &body),
            ChatGptOutputTokenBudget {
                requested: Some(128_000),
                effective: None
            }
        );
    }

    #[tokio::test]
    async fn chatgpt_virtual_context_preflight_blocks_before_upstream_call() {
        let (endpoint, requests) = capture_once_server().await;
        let provider = test_chatgpt_provider(endpoint).await;
        let token = ChatGptToken {
            access_token: "access".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: i64::MAX,
            account_id: Some("account".to_string()),
        };
        let body = json!({
            "model": "gpt-5.6-luna",
            "input": [
                {"role": "user", "content": "old"},
                {"role": "assistant", "content": "prior"},
                {"role": "user", "content": "x".repeat(1_400_000)}
            ],
            "stream": true
        });

        let estimate = provider.virtual_context_estimate(
            &body,
            &token,
            Some("session-context"),
            372_000,
            CompactRequestKind::None,
        );
        assert_eq!(estimate.safe_input_limit, 339_000);
        assert!(estimate.compressible_history);
        assert_eq!(estimate.estimator_source, ContextEstimatorSource::FullRough);
        let error = virtual_context_limit_error(&estimate)
            .expect("oversized virtual context should be blocked");

        match error.without_upstream_metadata() {
            ProviderError::InvalidRequest(message) => {
                assert!(message.starts_with("Prompt is too long:"));
                assert!(message.contains("339000 maximum safe input"));
                assert!(message.contains("model context window: 372000"));
            }
            other => panic!("unexpected error: {other}"),
        }
        assert!(requests.lock().await.is_empty());
    }

    #[tokio::test]
    async fn chatgpt_virtual_context_thresholds_follow_compaction_state() {
        let provider = test_chatgpt_provider("http://127.0.0.1:1/responses".to_string()).await;
        let token = chatgpt_test_token();
        let history = json!({
            "model": "gpt-5.6-sol",
            "input": [
                {"role": "user", "content": "question"},
                {"role": "assistant", "content": "answer"},
                {"role": "user", "content": "follow-up"}
            ]
        });
        let single_turn = json!({
            "model": "gpt-5.6-sol",
            "input": [{"role": "user", "content": "question"}]
        });

        let ordinary = provider.virtual_context_estimate(
            &history,
            &token,
            Some("threshold-session"),
            372_000,
            CompactRequestKind::None,
        );
        let summary = provider.virtual_context_estimate(
            &history,
            &token,
            Some("threshold-session"),
            372_000,
            CompactRequestKind::SummaryGeneration,
        );
        let continuation = provider.virtual_context_estimate(
            &single_turn,
            &token,
            Some("threshold-session"),
            372_000,
            CompactRequestKind::CompactedContinuation,
        );
        let no_history = provider.virtual_context_estimate(
            &single_turn,
            &token,
            Some("threshold-session"),
            372_000,
            CompactRequestKind::None,
        );

        assert_eq!(ordinary.safe_input_limit, 339_000);
        assert_eq!(summary.safe_input_limit, 352_000);
        assert_eq!(continuation.safe_input_limit, 339_000);
        assert_eq!(no_history.safe_input_limit, 352_000);
    }

    #[test]
    fn chatgpt_context_estimator_uses_fixed_media_cost() {
        let small = json!({"type": "input_image", "image_url": "data:image/png;base64,a"});
        let large = json!({
            "type": "input_image",
            "image_url": format!("data:image/png;base64,{}", "a".repeat(1_000_000))
        });
        assert_eq!(
            estimate_context_value_tokens(&small),
            CHATGPT_MEDIA_ESTIMATED_TOKENS
        );
        assert_eq!(
            estimate_context_value_tokens(&large),
            CHATGPT_MEDIA_ESTIMATED_TOKENS
        );
    }

    #[tokio::test]
    async fn chatgpt_context_estimator_reuses_usage_only_for_matching_prefix() {
        let provider = test_chatgpt_provider("http://127.0.0.1:1/responses".to_string()).await;
        let token = chatgpt_test_token();
        let first = json!({
            "model": "gpt-5.6-terra",
            "input": [{"role": "user", "content": "first"}]
        });
        let second = json!({
            "model": "gpt-5.6-terra",
            "input": [
                {"role": "user", "content": "first"},
                {"role": "user", "content": "second"}
            ]
        });
        let key = ContextUsageKey {
            provider_id: "chatgpt".to_string(),
            account_hash: capability_cache::account_hash(token.account_id.as_deref()).unwrap(),
            model: "gpt-5.6-terra".to_string(),
            stable_client_conversation_id: "usage-session".to_string(),
        };
        provider.context_usage.lock().unwrap().insert(
            key,
            ContextUsageBaseline {
                static_body: context_static_body(&first),
                context_items: first["input"].as_array().unwrap().clone(),
                total_tokens: 1_000,
                updated_at: Instant::now(),
            },
        );

        let matching = provider.virtual_context_estimate(
            &second,
            &token,
            Some("usage-session"),
            372_000,
            CompactRequestKind::None,
        );
        assert_eq!(
            matching.estimator_source,
            ContextEstimatorSource::UsagePlusDelta
        );
        assert!(matching.estimated_tokens >= 1_000);

        let different_session = provider.virtual_context_estimate(
            &second,
            &token,
            Some("other-session"),
            372_000,
            CompactRequestKind::None,
        );
        assert_eq!(
            different_session.estimator_source,
            ContextEstimatorSource::FullRough
        );
    }

    #[tokio::test]
    async fn chatgpt_send_responses_request_adds_codex_session_headers() {
        let (endpoint, requests) = capture_once_server().await;
        let provider = test_chatgpt_provider(endpoint).await;
        let token = ChatGptToken {
            access_token: "access".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: i64::MAX,
            account_id: Some("account".to_string()),
        };
        let body = json!({
            "model": "gpt-5.3-codex",
            "input": [{"role": "user", "content": "hi"}],
            "stream": true,
            "service_tier": "priority",
            "client_metadata": {
                "x-codex-turn-metadata": "{\"request_kind\":\"turn\",\"turn_id\":\"turn-test\"}"
            }
        });

        let response = provider
            .send_responses_request(
                &body,
                &token,
                ChatGptSseRequestContext {
                    compact_request: false,
                    request_id: 1,
                    budget: ChatGptOutputTokenBudget {
                        requested: Some(4096),
                        effective: body.get("max_output_tokens").and_then(Value::as_u64),
                    },
                    responses_lite: ResponsesLiteDecision::enabled(
                        ResponsesLiteDecisionSource::ForcedOn,
                    ),
                },
                0,
            )
            .await
            .expect("request should succeed");

        assert!(response.status().is_success());
        let requests = requests.lock().await;
        let headers = requests[0].headers.to_ascii_lowercase();
        assert!(headers.contains("accept: text/event-stream"));
        assert!(headers.contains("authorization: bearer access"));
        assert!(headers.contains("content-encoding: zstd"));
        assert!(headers.contains("chatgpt-account-id: account"));
        assert!(headers.contains("x-client-request-id: "));
        assert!(!headers.contains("x-client-request-id: thread-test"));
        assert!(headers.contains("session-id: session-test"));
        assert!(headers.contains("thread-id: thread-test"));
        assert!(headers.contains("x-codex-window-id: window-test"));
        assert!(headers.contains("x-codex-routing-hint: model=gpt-5.3-codex;tier=priority"));
        assert!(headers.contains(
            "x-codex-turn-metadata: {\"request_kind\":\"turn\",\"turn_id\":\"turn-test\"}"
        ));
        assert!(headers.contains("x-openai-internal-codex-responses-lite: true"));
        let request_body = request_body_json(&requests[0]);
        assert_eq!(request_body["model"], "gpt-5.3-codex");
    }

    #[tokio::test]
    async fn chatgpt_native_responses_send_preserves_items_and_filtered_headers() {
        let (endpoint, requests) = capture_once_server().await;
        let provider = test_chatgpt_provider(endpoint).await;
        let token = chatgpt_test_token();
        let mut body = json!({
            "model": "gpt-5.6-sol",
            "stream": true,
            "store": false,
            "future_option": {"enabled": true},
            "input": [
                {"id": "at-stable", "type": "additional_tools", "role": "developer", "tools": []},
                {"type": "configuration_update", "reasoning": {"effort": "high"}},
                {"type": "function_call_output", "name": "notifications", "namespace": "slack", "output": "ready"},
                {"type": "compaction_trigger"}
            ]
        });
        let mut forwarded_headers = HeaderMap::new();
        forwarded_headers.insert("x-codex-turn-state", "turn-state-1".parse().unwrap());
        forwarded_headers.insert(
            "x-codex-beta-features",
            "remote_compaction_v2".parse().unwrap(),
        );

        let response = provider
            .send_responses_request_with_prompt_too_long_retry_and_headers(
                &mut body,
                &token,
                ChatGptSseRequestContext {
                    compact_request: true,
                    request_id: 2,
                    budget: ChatGptOutputTokenBudget::default(),
                    responses_lite: ResponsesLiteDecision::enabled(
                        ResponsesLiteDecisionSource::ForcedOn,
                    ),
                },
                None,
                Some(&forwarded_headers),
            )
            .await
            .expect("native request should succeed");

        assert!(response.status().is_success());
        let requests = requests.lock().await;
        let captured = &requests[0];
        let captured_headers = captured.headers.to_ascii_lowercase();
        assert!(captured_headers.contains("x-codex-turn-state: turn-state-1"));
        assert!(captured_headers.contains("x-codex-beta-features: remote_compaction_v2"));
        let captured_body = request_body_json(captured);
        assert_eq!(captured_body, body);
    }

    #[tokio::test]
    async fn chatgpt_send_responses_request_omits_responses_lite_header_when_disabled() {
        let (endpoint, requests) = capture_once_server().await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.chatgpt_config.responses_lite = ResponsesLiteMode::Off;
        let token = ChatGptToken {
            access_token: "access".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: i64::MAX,
            account_id: Some("account".to_string()),
        };
        let body = json!({
            "model": "gpt-5.3-codex",
            "input": [{"role": "user", "content": "hi"}],
            "stream": true
        });

        provider
            .send_responses_request(
                &body,
                &token,
                ChatGptSseRequestContext {
                    compact_request: false,
                    request_id: 1,
                    budget: ChatGptOutputTokenBudget::default(),
                    responses_lite: ResponsesLiteDecision::disabled(
                        ResponsesLiteDecisionSource::ForcedOff,
                    ),
                },
                0,
            )
            .await
            .expect("request should succeed");

        let requests = requests.lock().await;
        let headers = requests[0].headers.to_ascii_lowercase();
        assert!(!headers.contains("x-openai-internal-codex-responses-lite"));
    }

    #[tokio::test]
    async fn chatgpt_websocket_success_streams_response_events() {
        let (endpoint, requests, handshakes) = websocket_events_server(vec![
            websocket_response_created("resp-ws-1"),
            websocket_response_completed("resp-ws-1"),
        ])
        .await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;
        let stream = provider
            .chat_prepared_with_token(chatgpt_test_prepared_request(1), chatgpt_test_token())
            .await
            .expect("websocket stream should start");

        let events = collect_stream_results(stream).await;
        assert!(events.iter().all(Result::is_ok));
        let event_names = events
            .iter()
            .map(|event| event.as_ref().unwrap().event.as_str())
            .collect::<Vec<_>>();
        assert!(event_names.contains(&"message_start"));
        assert!(event_names.contains(&"message_stop"));

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["type"], "response.create");
        assert_eq!(requests[0]["model"], "gpt-5.3-codex");
        assert_eq!(
            requests[0]["client_metadata"]["ws_request_header_x_openai_internal_codex_responses_lite"],
            "true"
        );
        let handshakes = handshakes.lock().unwrap();
        assert_eq!(handshakes.len(), 1);
        assert_eq!(
            handshakes[0].header("openai-beta").as_deref(),
            Some("responses_websockets=2026-02-06")
        );
        assert_eq!(
            handshakes[0].header("authorization").as_deref(),
            Some("Bearer access")
        );
        assert_eq!(
            handshakes[0].header("x-codex-routing-hint").as_deref(),
            Some("model=gpt-5.3-codex")
        );
        assert!(handshakes[0].header("x-codex-turn-metadata").is_some());
    }

    #[tokio::test]
    async fn chatgpt_websocket_completion_records_context_usage_baseline() {
        let (endpoint, _, _) = websocket_events_server(vec![
            websocket_response_created("resp-ws-context"),
            websocket_response_completed("resp-ws-context"),
        ])
        .await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;
        let token = chatgpt_test_token();
        let mut prepared = chatgpt_test_prepared_request(103);
        let pending = test_pending_context_usage(&provider, &prepared, &token, "ws-context");
        let key = pending.key.clone();
        prepared.pending_context_usage = Some(pending);

        let stream = provider
            .chat_prepared_with_token(prepared, token)
            .await
            .expect("websocket stream should start");
        assert!(
            collect_stream_results(stream)
                .await
                .iter()
                .all(Result::is_ok)
        );

        let cache = provider.context_usage.lock().unwrap();
        let baseline = cache.get(&key).expect("usage baseline");
        assert_eq!(baseline.total_tokens, 3);
        assert_eq!(baseline.context_items.len(), 1);
    }

    #[tokio::test]
    async fn chatgpt_websocket_prewarm_sends_generate_false_and_reuses_warm_response() {
        let (endpoint, requests, handshakes) = websocket_sequence_server(vec![
            vec![websocket_response_completed("warm-1")],
            vec![
                websocket_response_created("resp-ws-1"),
                websocket_response_completed("resp-ws-1"),
            ],
        ])
        .await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;
        provider.chatgpt_config.websocket_prewarm = true;

        let stream = provider
            .chat_prepared_with_token(chatgpt_test_prepared_request(101), chatgpt_test_token())
            .await
            .expect("websocket stream should start after prewarm");
        let events = collect_stream_results(stream).await;
        assert!(events.iter().all(Result::is_ok));

        assert_eq!(handshakes.lock().unwrap().len(), 1);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["type"], "response.create");
        assert_eq!(requests[0]["generate"], false);
        assert!(requests[0].get("previous_response_id").is_none());
        assert_eq!(requests[0]["input"].as_array().map(Vec::len), Some(1));
        assert_eq!(requests[1]["previous_response_id"], "warm-1");
        assert_eq!(requests[1]["input"], json!([]));
        assert!(requests[1].get("generate").is_none());
    }

    #[tokio::test]
    async fn chatgpt_websocket_continuation_sends_previous_response_id_and_delta_input() {
        let first_body = chatgpt_websocket_test_body();
        let second_delta = json!({"role": "user", "content": "next"});
        let mut second_body = chatgpt_websocket_test_body();
        second_body["input"] = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            second_delta.clone()
        ]);

        let (endpoint, requests, handshakes) = websocket_sequence_server(vec![
            vec![
                websocket_response_created("resp-ws-1"),
                websocket_response_completed_with_output(
                    "resp-ws-1",
                    json!([{
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "hello"}]
                    }]),
                ),
            ],
            vec![
                websocket_response_created("resp-ws-2"),
                websocket_response_completed("resp-ws-2"),
            ],
        ])
        .await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;

        for (request_id, body) in [(8, first_body), (9, second_body)] {
            let stream = provider
                .chat_prepared_with_token(
                    chatgpt_test_prepared_request_with_body(
                        request_id,
                        body,
                        Some("conversation-continuation"),
                    ),
                    chatgpt_test_token(),
                )
                .await
                .expect("websocket stream should start");
            let events = collect_stream_results(stream).await;
            assert!(events.iter().all(Result::is_ok));
        }

        assert_eq!(handshakes.lock().unwrap().len(), 1);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[0].get("previous_response_id").is_none());
        assert_eq!(requests[0]["input"].as_array().map(Vec::len), Some(1));
        assert_eq!(requests[1]["previous_response_id"], "resp-ws-1");
        assert_eq!(requests[1]["input"], json!([second_delta]));
    }

    #[tokio::test]
    async fn chatgpt_auto_transport_falls_back_to_sse_when_prewarm_fails_before_real_request() {
        let (endpoint, websocket_requests, sse_requests) =
            websocket_prewarm_failure_then_sse_server().await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Auto;
        provider.chatgpt_config.websocket_prewarm = true;

        let stream = provider
            .chat_prepared_with_token(chatgpt_test_prepared_request(102), chatgpt_test_token())
            .await
            .expect("auto transport should fall back to SSE after prewarm failure");
        let events = collect_stream_results(stream).await;
        assert!(events.iter().all(Result::is_ok));
        assert_eq!(provider.effective_transport(), ChatGptTransport::Sse);

        {
            let websocket_requests = websocket_requests.lock().unwrap();
            assert_eq!(websocket_requests.len(), 1);
            assert_eq!(websocket_requests[0]["generate"], false);
        }
        let sse_requests = sse_requests.lock().await;
        assert_eq!(sse_requests.len(), 1);
        let sse_body = request_body_json(&sse_requests[0]);
        assert!(sse_body.get("previous_response_id").is_none());
        assert!(sse_body.get("generate").is_none());
    }

    #[tokio::test]
    async fn chatgpt_auto_transport_falls_back_to_sse_when_continuation_response_id_is_stale() {
        let second_delta = json!({"role": "user", "content": "next"});
        let mut second_body = chatgpt_websocket_test_body();
        second_body["input"] = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            second_delta.clone()
        ]);
        let (endpoint, websocket_requests, sse_requests) =
            websocket_stale_continuation_then_sse_server().await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Auto;

        let first = provider
            .chat_prepared_with_token(
                chatgpt_test_prepared_request_with_body(
                    90,
                    chatgpt_websocket_test_body(),
                    Some("conversation-stale-response"),
                ),
                chatgpt_test_token(),
            )
            .await
            .expect("first websocket stream should start");
        assert!(
            collect_stream_results(first)
                .await
                .iter()
                .all(Result::is_ok)
        );

        let second = provider
            .chat_prepared_with_token(
                chatgpt_test_prepared_request_with_body(
                    91,
                    second_body.clone(),
                    Some("conversation-stale-response"),
                ),
                chatgpt_test_token(),
            )
            .await
            .expect("stale continuation should fall back to SSE");
        assert!(
            collect_stream_results(second)
                .await
                .iter()
                .all(Result::is_ok)
        );
        assert_eq!(provider.effective_transport(), ChatGptTransport::Sse);

        provider
            .websocket_sse_cooldown_until_secs
            .store(0, Ordering::Relaxed);
        let third = provider
            .chat_prepared_with_token(
                chatgpt_test_prepared_request_with_body(
                    92,
                    second_body,
                    Some("conversation-stale-response"),
                ),
                chatgpt_test_token(),
            )
            .await
            .expect("cleared continuation state should allow websocket retry");
        assert!(
            collect_stream_results(third)
                .await
                .iter()
                .all(Result::is_ok)
        );

        {
            let websocket_requests = websocket_requests.lock().unwrap();
            assert_eq!(websocket_requests.len(), 4);
            assert!(websocket_requests[0].get("previous_response_id").is_none());
            assert_eq!(websocket_requests[1]["previous_response_id"], "resp-ws-1");
            assert_eq!(websocket_requests[1]["input"], json!([second_delta]));
            assert!(websocket_requests[2].get("previous_response_id").is_none());
            assert_eq!(
                websocket_requests[2]["input"].as_array().map(Vec::len),
                Some(3)
            );
            assert!(websocket_requests[3].get("previous_response_id").is_none());
            assert_eq!(
                websocket_requests[3]["input"].as_array().map(Vec::len),
                Some(3)
            );
        }

        let sse_requests = sse_requests.lock().await;
        assert_eq!(sse_requests.len(), 1);
        let sse_body = request_body_json(&sse_requests[0]);
        assert!(sse_body.get("previous_response_id").is_none());
        assert_eq!(sse_body["input"].as_array().map(Vec::len), Some(3));

        let stats = provider.websocket_stats.snapshot();
        assert_eq!(stats.fallbacks, 1);
    }

    #[tokio::test]
    async fn chatgpt_websocket_continuation_function_call_delta_sends_tool_result_only() {
        let function_call = json!({
            "type": "function_call",
            "call_id": "call-1",
            "name": "Read",
            "arguments": "{\"file\":\"a.txt\"}"
        });
        let tool_result = json!({
            "type": "function_call_output",
            "call_id": "call-1",
            "output": "file contents"
        });
        let mut second_body = chatgpt_websocket_test_body();
        second_body["input"] = json!([
            {"role": "user", "content": "hi"},
            function_call.clone(),
            tool_result.clone()
        ]);

        let (endpoint, requests, _handshakes) = websocket_sequence_server(vec![
            vec![
                websocket_response_created("resp-ws-1"),
                websocket_response_completed_with_output("resp-ws-1", json!([function_call])),
            ],
            vec![
                websocket_response_created("resp-ws-2"),
                websocket_response_completed("resp-ws-2"),
            ],
        ])
        .await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;

        for (request_id, body) in [(30, chatgpt_websocket_test_body()), (31, second_body)] {
            let stream = provider
                .chat_prepared_with_token(
                    chatgpt_test_prepared_request_with_body(
                        request_id,
                        body,
                        Some("conversation-function-call"),
                    ),
                    chatgpt_test_token(),
                )
                .await
                .expect("websocket stream should start");
            let events = collect_stream_results(stream).await;
            assert!(events.iter().all(Result::is_ok));
        }

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1]["previous_response_id"], "resp-ws-1");
        assert_eq!(requests[1]["input"], json!([tool_result]));
    }

    #[tokio::test]
    async fn chatgpt_websocket_continuation_requires_stable_conversation_id() {
        let mut second_body = chatgpt_websocket_test_body();
        second_body["input"] = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "next"}
        ]);
        let (endpoint, requests, _handshakes) = websocket_sequence_server(vec![
            vec![
                websocket_response_created("resp-ws-1"),
                websocket_response_completed_with_output(
                    "resp-ws-1",
                    json!([{
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "hello"}]
                    }]),
                ),
            ],
            vec![
                websocket_response_created("resp-ws-2"),
                websocket_response_completed("resp-ws-2"),
            ],
        ])
        .await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;

        for (request_id, body) in [(10, chatgpt_websocket_test_body()), (11, second_body)] {
            let stream = provider
                .chat_prepared_with_token(
                    chatgpt_test_prepared_request_with_body(request_id, body, None),
                    chatgpt_test_token(),
                )
                .await
                .expect("websocket stream should start");
            let events = collect_stream_results(stream).await;
            assert!(events.iter().all(Result::is_ok));
        }

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].get("previous_response_id").is_none());
        assert_eq!(requests[1]["input"].as_array().map(Vec::len), Some(3));
    }

    #[tokio::test]
    async fn chatgpt_websocket_continuation_prefix_mismatch_sends_full_input() {
        let mut second_body = chatgpt_websocket_test_body();
        second_body["input"] = json!([
            {"role": "user", "content": "different"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "next"}
        ]);
        let (endpoint, requests, _handshakes) = websocket_sequence_server(vec![
            vec![
                websocket_response_created("resp-ws-1"),
                websocket_response_completed_with_output(
                    "resp-ws-1",
                    json!([{
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "hello"}]
                    }]),
                ),
            ],
            vec![
                websocket_response_created("resp-ws-2"),
                websocket_response_completed("resp-ws-2"),
            ],
        ])
        .await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;

        for (request_id, body) in [(12, chatgpt_websocket_test_body()), (13, second_body)] {
            let stream = provider
                .chat_prepared_with_token(
                    chatgpt_test_prepared_request_with_body(
                        request_id,
                        body,
                        Some("conversation-prefix-mismatch"),
                    ),
                    chatgpt_test_token(),
                )
                .await
                .expect("websocket stream should start");
            let events = collect_stream_results(stream).await;
            assert!(events.iter().all(Result::is_ok));
        }

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].get("previous_response_id").is_none());
        assert_eq!(requests[1]["input"].as_array().map(Vec::len), Some(3));
    }

    #[tokio::test]
    async fn chatgpt_websocket_continuation_account_mismatch_sends_full_input() {
        let mut second_body = chatgpt_websocket_test_body();
        second_body["input"] = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "next"}
        ]);
        let (endpoint, requests, handshakes) = websocket_one_request_per_connection_server(vec![
            vec![
                websocket_response_created("resp-ws-1"),
                websocket_response_completed_with_output(
                    "resp-ws-1",
                    json!([{
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "hello"}]
                    }]),
                ),
            ],
            vec![
                websocket_response_created("resp-ws-2"),
                websocket_response_completed("resp-ws-2"),
            ],
        ])
        .await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;

        let first = provider
            .chat_prepared_with_token(
                chatgpt_test_prepared_request_with_body(
                    14,
                    chatgpt_websocket_test_body(),
                    Some("conversation-account-mismatch"),
                ),
                chatgpt_test_token(),
            )
            .await
            .expect("first websocket stream should start");
        assert!(
            collect_stream_results(first)
                .await
                .iter()
                .all(Result::is_ok)
        );

        let mut second_token = chatgpt_test_token();
        second_token.account_id = Some("other-account".to_string());
        let second = provider
            .chat_prepared_with_token(
                chatgpt_test_prepared_request_with_body(
                    15,
                    second_body,
                    Some("conversation-account-mismatch"),
                ),
                second_token,
            )
            .await
            .expect("second websocket stream should start");
        assert!(
            collect_stream_results(second)
                .await
                .iter()
                .all(Result::is_ok)
        );

        assert_eq!(handshakes.lock().unwrap().len(), 2);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].get("previous_response_id").is_none());
        assert_eq!(requests[1]["input"].as_array().map(Vec::len), Some(3));
    }

    #[tokio::test]
    async fn chatgpt_websocket_continuation_body_mismatch_sends_full_input() {
        let mut second_body = chatgpt_websocket_test_body();
        second_body["service_tier"] = json!("priority");
        second_body["input"] = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "next"}
        ]);
        let (endpoint, requests, _handshakes) = websocket_sequence_server(vec![
            vec![
                websocket_response_created("resp-ws-1"),
                websocket_response_completed_with_output(
                    "resp-ws-1",
                    json!([{
                        "type": "message",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "hello"}]
                    }]),
                ),
            ],
            vec![
                websocket_response_created("resp-ws-2"),
                websocket_response_completed("resp-ws-2"),
            ],
        ])
        .await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;

        for (request_id, body) in [(12, chatgpt_websocket_test_body()), (13, second_body)] {
            let stream = provider
                .chat_prepared_with_token(
                    chatgpt_test_prepared_request_with_body(
                        request_id,
                        body,
                        Some("conversation-body-mismatch"),
                    ),
                    chatgpt_test_token(),
                )
                .await
                .expect("websocket stream should start");
            let events = collect_stream_results(stream).await;
            assert!(events.iter().all(Result::is_ok));
        }

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].get("previous_response_id").is_none());
        assert_eq!(requests[1]["service_tier"], "priority");
        assert_eq!(requests[1]["input"].as_array().map(Vec::len), Some(3));
    }

    #[tokio::test]
    async fn chatgpt_websocket_continuation_terminal_failure_clears_state() {
        let mut second_body = chatgpt_websocket_test_body();
        second_body["input"] = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "next"}
        ]);
        let (endpoint, requests, _handshakes) = websocket_sequence_server(vec![
            vec![
                websocket_response_created("resp-ws-1"),
                websocket_response_failed("resp-ws-1"),
            ],
            vec![
                websocket_response_created("resp-ws-2"),
                websocket_response_completed("resp-ws-2"),
            ],
        ])
        .await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;

        for (index, (request_id, body)) in [(14, chatgpt_websocket_test_body()), (15, second_body)]
            .into_iter()
            .enumerate()
        {
            let stream = provider
                .chat_prepared_with_token(
                    chatgpt_test_prepared_request_with_body(
                        request_id,
                        body,
                        Some("conversation-failed"),
                    ),
                    chatgpt_test_token(),
                )
                .await
                .expect("websocket stream should start");
            let events = collect_stream_results(stream).await;
            if index == 0 {
                assert!(events.iter().any(Result::is_err));
            } else {
                assert!(events.iter().all(Result::is_ok));
            }
        }

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].get("previous_response_id").is_none());
        assert_eq!(requests[1]["input"].as_array().map(Vec::len), Some(3));
    }

    #[tokio::test]
    async fn chatgpt_websocket_continuation_abort_clears_state() {
        let mut second_body = chatgpt_websocket_test_body();
        second_body["input"] = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "abort me"}
        ]);
        let mut third_body = chatgpt_websocket_test_body();
        third_body["input"] = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "after abort"}
        ]);
        let (endpoint, requests, close_rx) =
            websocket_abort_continuation_invalidation_server().await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;

        let first = provider
            .chat_prepared_with_token(
                chatgpt_test_prepared_request_with_body(
                    40,
                    chatgpt_websocket_test_body(),
                    Some("conversation-abort-invalidation"),
                ),
                chatgpt_test_token(),
            )
            .await
            .expect("first websocket stream should start");
        assert!(
            collect_stream_results(first)
                .await
                .iter()
                .all(Result::is_ok)
        );

        let mut second = provider
            .chat_prepared_with_token(
                chatgpt_test_prepared_request_with_body(
                    41,
                    second_body,
                    Some("conversation-abort-invalidation"),
                ),
                chatgpt_test_token(),
            )
            .await
            .expect("second websocket stream should start");
        let first_event = second
            .next()
            .await
            .expect("first downstream event")
            .expect("message_start should be ok");
        assert_eq!(first_event.normalized_events()[0].event, "message_start");
        drop(second);
        tokio::time::timeout(Duration::from_secs(2), close_rx)
            .await
            .expect("websocket should close after downstream abort")
            .expect("close notification should be sent");

        let third = provider
            .chat_prepared_with_token(
                chatgpt_test_prepared_request_with_body(
                    42,
                    third_body,
                    Some("conversation-abort-invalidation"),
                ),
                chatgpt_test_token(),
            )
            .await
            .expect("third websocket stream should start");
        assert!(
            collect_stream_results(third)
                .await
                .iter()
                .all(Result::is_ok)
        );

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1]["previous_response_id"], "resp-ws-1");
        assert!(requests[2].get("previous_response_id").is_none());
        assert_eq!(requests[2]["input"].as_array().map(Vec::len), Some(3));
    }

    #[tokio::test]
    async fn chatgpt_websocket_continuation_busy_request_invalidates_in_flight_state() {
        let mut second_body = chatgpt_websocket_test_body();
        second_body["input"] = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "busy"}
        ]);
        let mut third_body = chatgpt_websocket_test_body();
        third_body["input"] = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "overlap"}
        ]);
        let mut fourth_body = chatgpt_websocket_test_body();
        fourth_body["input"] = json!([
            {"role": "user", "content": "hi"},
            {"role": "assistant", "content": "hello"},
            {"role": "user", "content": "busy"},
            {"role": "user", "content": "after overlap"}
        ]);
        let (endpoint, requests, complete_second_tx) =
            websocket_busy_continuation_invalidation_server().await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;

        let first = provider
            .chat_prepared_with_token(
                chatgpt_test_prepared_request_with_body(
                    50,
                    chatgpt_websocket_test_body(),
                    Some("conversation-busy"),
                ),
                chatgpt_test_token(),
            )
            .await
            .expect("first websocket stream should start");
        assert!(
            collect_stream_results(first)
                .await
                .iter()
                .all(Result::is_ok)
        );

        let mut second = provider
            .chat_prepared_with_token(
                chatgpt_test_prepared_request_with_body(51, second_body, Some("conversation-busy")),
                chatgpt_test_token(),
            )
            .await
            .expect("second websocket stream should start");
        let first_event = second
            .next()
            .await
            .expect("first downstream event")
            .expect("message_start should be ok");
        assert_eq!(first_event.normalized_events()[0].event, "message_start");

        let third = provider
            .chat_prepared_with_token(
                chatgpt_test_prepared_request_with_body(52, third_body, Some("conversation-busy")),
                chatgpt_test_token(),
            )
            .await
            .expect("third websocket stream should start");
        assert!(
            collect_stream_results(third)
                .await
                .iter()
                .all(Result::is_ok)
        );

        complete_second_tx
            .send(())
            .expect("second completion signal should send");
        assert!(
            collect_stream_results(second)
                .await
                .iter()
                .all(Result::is_ok)
        );

        let fourth = provider
            .chat_prepared_with_token(
                chatgpt_test_prepared_request_with_body(53, fourth_body, Some("conversation-busy")),
                chatgpt_test_token(),
            )
            .await
            .expect("fourth websocket stream should start");
        assert!(
            collect_stream_results(fourth)
                .await
                .iter()
                .all(Result::is_ok)
        );

        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert!(requests[0].get("previous_response_id").is_none());
        assert_eq!(requests[1]["previous_response_id"], "resp-ws-1");
        assert!(requests[2].get("previous_response_id").is_none());
        assert_eq!(requests[2]["input"].as_array().map(Vec::len), Some(3));
        assert!(requests[3].get("previous_response_id").is_none());
        assert_eq!(requests[3]["input"].as_array().map(Vec::len), Some(4));
    }

    #[tokio::test]
    async fn chatgpt_websocket_uses_configured_http_proxy() {
        let (endpoint, proxy_url, connect_requests) = websocket_proxy_server(vec![vec![
            websocket_response_created("resp-ws-proxy"),
            websocket_response_completed("resp-ws-proxy"),
        ]])
        .await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;
        provider.proxy = Some(proxy_url);

        let stream = provider
            .chat_prepared_with_token(chatgpt_test_prepared_request(7), chatgpt_test_token())
            .await
            .expect("websocket stream should start through proxy");
        let events = collect_stream_results(stream).await;
        assert!(events.iter().all(Result::is_ok));

        let connect_requests = connect_requests.lock().await;
        assert_eq!(connect_requests.len(), 1);
        assert!(
            connect_requests[0]
                .headers
                .starts_with("CONNECT chatgpt.test:80 HTTP/1.1")
        );
    }

    #[tokio::test]
    async fn chatgpt_websocket_uses_env_https_proxy_when_provider_proxy_missing() {
        let _env_lock = CHATGPT_WEBSOCKET_PROXY_ENV_LOCK.lock().await;
        let _https_proxy = EnvVarGuard::remove("HTTPS_PROXY");
        let _https_proxy_lower = EnvVarGuard::remove("https_proxy");
        let _all_proxy = EnvVarGuard::remove("ALL_PROXY");
        let _all_proxy_lower = EnvVarGuard::remove("all_proxy");
        let _no_proxy = EnvVarGuard::remove("NO_PROXY");
        let _no_proxy_lower = EnvVarGuard::remove("no_proxy");
        let (endpoint, proxy_url, connect_requests) = websocket_proxy_server(vec![vec![
            websocket_response_created("resp-ws-env-proxy"),
            websocket_response_completed("resp-ws-env-proxy"),
        ]])
        .await;
        let _env_proxy = EnvVarGuard::set("HTTPS_PROXY", &proxy_url);
        let _loopback_no_proxy = EnvVarGuard::set("NO_PROXY", "127.0.0.1,localhost");
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;

        let stream = provider
            .chat_prepared_with_token(chatgpt_test_prepared_request(80), chatgpt_test_token())
            .await
            .expect("websocket stream should start through env proxy");
        let events = collect_stream_results(stream).await;
        assert!(events.iter().all(Result::is_ok));

        let connect_requests = connect_requests.lock().await;
        assert_eq!(connect_requests.len(), 1);
        assert!(
            connect_requests[0]
                .headers
                .starts_with("CONNECT chatgpt.test:80 HTTP/1.1")
        );
    }

    #[tokio::test]
    async fn chatgpt_websocket_provider_proxy_overrides_env_proxy() {
        let _env_lock = CHATGPT_WEBSOCKET_PROXY_ENV_LOCK.lock().await;
        let _https_proxy = EnvVarGuard::remove("HTTPS_PROXY");
        let _https_proxy_lower = EnvVarGuard::remove("https_proxy");
        let _all_proxy = EnvVarGuard::remove("ALL_PROXY");
        let _all_proxy_lower = EnvVarGuard::remove("all_proxy");
        let _no_proxy = EnvVarGuard::remove("NO_PROXY");
        let _no_proxy_lower = EnvVarGuard::remove("no_proxy");
        let (endpoint, provider_proxy_url, provider_connect_requests) =
            websocket_proxy_server(vec![vec![
                websocket_response_created("resp-ws-provider-proxy"),
                websocket_response_completed("resp-ws-provider-proxy"),
            ]])
            .await;
        let (_unused_endpoint, env_proxy_url, env_connect_requests) =
            websocket_proxy_server(vec![]).await;
        let _env_proxy = EnvVarGuard::set("HTTPS_PROXY", &env_proxy_url);
        let _loopback_no_proxy = EnvVarGuard::set("NO_PROXY", "127.0.0.1,localhost");
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;
        provider.proxy = Some(provider_proxy_url);

        let stream = provider
            .chat_prepared_with_token(chatgpt_test_prepared_request(81), chatgpt_test_token())
            .await
            .expect("websocket stream should start through provider proxy");
        let events = collect_stream_results(stream).await;
        assert!(events.iter().all(Result::is_ok));

        assert_eq!(provider_connect_requests.lock().await.len(), 1);
        assert_eq!(env_connect_requests.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn chatgpt_websocket_no_proxy_bypasses_env_proxy() {
        let _env_lock = CHATGPT_WEBSOCKET_PROXY_ENV_LOCK.lock().await;
        let _https_proxy = EnvVarGuard::remove("HTTPS_PROXY");
        let _https_proxy_lower = EnvVarGuard::remove("https_proxy");
        let _all_proxy = EnvVarGuard::remove("ALL_PROXY");
        let _all_proxy_lower = EnvVarGuard::remove("all_proxy");
        let _no_proxy = EnvVarGuard::remove("NO_PROXY");
        let _no_proxy_lower = EnvVarGuard::remove("no_proxy");
        let (endpoint, requests, _handshakes) = websocket_events_server(vec![
            websocket_response_created("resp-ws-no-proxy"),
            websocket_response_completed("resp-ws-no-proxy"),
        ])
        .await;
        let (_unused_endpoint, env_proxy_url, env_connect_requests) =
            websocket_proxy_server(vec![]).await;
        let _env_proxy = EnvVarGuard::set("HTTPS_PROXY", &env_proxy_url);
        let _no_proxy = EnvVarGuard::set("NO_PROXY", "127.0.0.1");
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;

        let stream = provider
            .chat_prepared_with_token(chatgpt_test_prepared_request(82), chatgpt_test_token())
            .await
            .expect("websocket stream should bypass env proxy");
        let events = collect_stream_results(stream).await;
        assert!(events.iter().all(Result::is_ok));

        assert_eq!(requests.lock().unwrap().len(), 1);
        assert_eq!(env_connect_requests.lock().await.len(), 0);
    }

    #[tokio::test]
    async fn chatgpt_auto_transport_falls_back_to_sse_before_first_websocket_event() {
        let (endpoint, requests) = websocket_upgrade_required_then_sse_server().await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Auto;
        let stream = provider
            .chat_prepared_with_token(chatgpt_test_prepared_request(2), chatgpt_test_token())
            .await
            .expect("auto transport should fall back to SSE");

        let events = collect_stream_results(stream).await;
        assert!(events.iter().all(Result::is_ok));
        assert_eq!(provider.effective_transport(), ChatGptTransport::Sse);

        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert!(requests[0].headers.starts_with("GET "));
        assert!(requests[1].headers.starts_with("POST "));
    }

    #[tokio::test]
    async fn chatgpt_sse_fallback_completion_records_context_usage_baseline() {
        let (endpoint, _) = websocket_upgrade_required_then_sse_server().await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Auto;
        let token = chatgpt_test_token();
        let mut prepared = chatgpt_test_prepared_request(104);
        let pending = test_pending_context_usage(&provider, &prepared, &token, "sse-context");
        let key = pending.key.clone();
        prepared.pending_context_usage = Some(pending);

        let stream = provider
            .chat_prepared_with_token(prepared, token)
            .await
            .expect("auto transport should fall back to SSE");
        assert!(
            collect_stream_results(stream)
                .await
                .iter()
                .all(Result::is_ok)
        );

        let cache = provider.context_usage.lock().unwrap();
        assert_eq!(
            cache.get(&key).map(|baseline| baseline.total_tokens),
            Some(3)
        );
    }

    #[tokio::test]
    async fn chatgpt_auto_transport_retries_websocket_after_startup_cooldown_expires() {
        let (endpoint, requests) = websocket_fallback_cooldown_retry_server().await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Auto;

        let first = provider
            .chat_prepared_with_token(chatgpt_test_prepared_request(70), chatgpt_test_token())
            .await
            .expect("first auto request should fall back to SSE");
        assert!(
            collect_stream_results(first)
                .await
                .iter()
                .all(Result::is_ok)
        );
        assert_eq!(provider.effective_transport(), ChatGptTransport::Sse);

        let second = provider
            .chat_prepared_with_token(chatgpt_test_prepared_request(71), chatgpt_test_token())
            .await
            .expect("cooldown request should use SSE directly");
        assert!(
            collect_stream_results(second)
                .await
                .iter()
                .all(Result::is_ok)
        );

        provider
            .websocket_sse_cooldown_until_secs
            .store(0, Ordering::Relaxed);
        let third = provider
            .chat_prepared_with_token(chatgpt_test_prepared_request(72), chatgpt_test_token())
            .await
            .expect("expired cooldown should retry websocket");
        assert!(
            collect_stream_results(third)
                .await
                .iter()
                .all(Result::is_ok)
        );

        let stats = provider.websocket_stats.snapshot();
        assert_eq!(stats.attempts, 2);
        assert_eq!(stats.successes, 1);
        assert_eq!(stats.failures, 1);
        assert_eq!(stats.fallbacks, 1);
        assert_eq!(stats.connections_created, 1);
        assert_eq!(stats.connections_reused, 0);

        let requests = requests.lock().await;
        assert_eq!(requests.len(), 4);
        assert!(requests[0].headers.starts_with("GET "));
        assert!(requests[1].headers.starts_with("POST "));
        assert!(requests[2].headers.starts_with("POST "));
        assert!(requests[3].headers.starts_with("GET "));
    }

    #[tokio::test]
    async fn chatgpt_auto_transport_does_not_fallback_after_first_websocket_event() {
        let (endpoint, requests, _handshakes) =
            websocket_events_server(vec![websocket_response_created("resp-ws-close")]).await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Auto;
        let stream = provider
            .chat_prepared_with_token(chatgpt_test_prepared_request(3), chatgpt_test_token())
            .await
            .expect("websocket stream should start after first event");

        let events = collect_stream_results(stream).await;
        assert!(events.iter().any(Result::is_err));
        assert_eq!(provider.effective_transport(), ChatGptTransport::Auto);
        assert_eq!(requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn chatgpt_websocket_abort_closes_upstream_connection() {
        let (endpoint, close_rx) = websocket_hanging_after_created_server().await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;
        let mut stream = provider
            .chat_prepared_with_token(chatgpt_test_prepared_request(6), chatgpt_test_token())
            .await
            .expect("websocket stream should start");

        let first = stream
            .next()
            .await
            .expect("first downstream event")
            .expect("message_start should be ok");
        assert_eq!(first.normalized_events()[0].event, "message_start");
        drop(stream);

        tokio::time::timeout(Duration::from_secs(2), close_rx)
            .await
            .expect("websocket connection should close promptly after downstream abort")
            .expect("close notification should be sent");
    }

    #[tokio::test]
    async fn chatgpt_websocket_reuses_completed_connection() {
        let (endpoint, requests, handshakes) = websocket_sequence_server(vec![
            vec![
                websocket_response_created("resp-ws-1"),
                websocket_response_completed("resp-ws-1"),
            ],
            vec![
                websocket_response_created("resp-ws-2"),
                websocket_response_completed("resp-ws-2"),
            ],
        ])
        .await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;

        for request_id in [4, 5] {
            let stream = provider
                .chat_prepared_with_token(
                    chatgpt_test_prepared_request(request_id),
                    chatgpt_test_token(),
                )
                .await
                .expect("websocket stream should start");
            let events = collect_stream_results(stream).await;
            assert!(events.iter().all(Result::is_ok));
        }

        assert_eq!(handshakes.lock().unwrap().len(), 1);
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn chatgpt_websocket_does_not_reuse_completed_connection_across_models() {
        let (endpoint, requests, handshakes) = websocket_one_request_per_connection_server(vec![
            vec![
                websocket_response_created("resp-ws-1"),
                websocket_response_completed("resp-ws-1"),
            ],
            vec![
                websocket_response_created("resp-ws-2"),
                websocket_response_completed("resp-ws-2"),
            ],
        ])
        .await;
        let mut provider = test_chatgpt_provider(endpoint).await;
        provider.transport = ChatGptTransport::Websocket;

        let first = provider
            .chat_prepared_with_token(chatgpt_test_prepared_request(4), chatgpt_test_token())
            .await
            .expect("first websocket stream should start");
        assert!(
            collect_stream_results(first)
                .await
                .iter()
                .all(Result::is_ok)
        );

        let mut second_body = chatgpt_websocket_test_body();
        second_body["model"] = json!("gpt-5.5");
        let second = provider
            .chat_prepared_with_token(
                chatgpt_test_prepared_request_with_body(5, second_body, Some("conversation-test")),
                chatgpt_test_token(),
            )
            .await
            .expect("second websocket stream should start");
        assert!(
            collect_stream_results(second)
                .await
                .iter()
                .all(Result::is_ok)
        );

        assert_eq!(handshakes.lock().unwrap().len(), 2);
        assert_eq!(requests.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn chatgpt_upstream_context_length_error_returns_request_too_large_without_retry() {
        let (endpoint, requests) = prompt_too_long_error_server().await;
        let provider = test_chatgpt_provider(endpoint).await;
        let token = ChatGptToken {
            access_token: "access".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: i64::MAX,
            account_id: Some("account".to_string()),
        };
        let mut body = json!({
            "model": "gpt-5.3-codex",
            "input": [
                {"role": "user", "content": "old"},
                {"role": "user", "content": "current"}
            ],
            "stream": true
        });

        let error = provider
            .send_responses_request_with_prompt_too_long_retry(
                &mut body,
                &token,
                ChatGptSseRequestContext {
                    compact_request: false,
                    request_id: 1,
                    budget: ChatGptOutputTokenBudget::default(),
                    responses_lite: ResponsesLiteDecision::disabled(
                        ResponsesLiteDecisionSource::UnknownModel,
                    ),
                },
                None,
            )
            .await
            .expect_err("upstream context-limit error should not be retried");

        match error.without_upstream_metadata() {
            ProviderError::InvalidRequest(message) => {
                assert_eq!(message, "Prompt is too long: context limit")
            }
            other => panic!("unexpected error: {other}"),
        }
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0]["input"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn prompt_too_long_error_detection_accepts_text_and_context_code() {
        assert!(is_prompt_too_long_error(
            StatusCode::BAD_REQUEST,
            "Prompt is too long: 137500 tokens > 135000 maximum"
        ));
        assert!(is_prompt_too_long_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            r#"{"error":{"code":"context_length_exceeded","message":"context limit"}}"#
        ));
        assert!(!is_prompt_too_long_error(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"code":"invalid_request","message":"bad tool schema"}}"#
        ));
    }

    #[test]
    fn context_length_errors_map_to_anthropic_invalid_request_even_with_http_200() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"Your input exceeds the context window of this model."}}"#;
        let error = map_chatgpt_error_status_body(StatusCode::OK, body.to_string());

        match error.without_upstream_metadata() {
            ProviderError::InvalidRequest(message) => {
                assert!(message.starts_with("Prompt is too long:"));
                assert!(message.contains("exceeds the context window"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn chatgpt_stream_context_length_error_maps_to_anthropic_invalid_request() {
        let body = r#"{"type":"error","error":{"code":"context_length_exceeded","message":"context limit"}}"#;
        let error = map_chatgpt_stream_error(ProviderError::UpstreamError {
            status: 200,
            body: body.to_string(),
        });

        match error.without_upstream_metadata() {
            ProviderError::InvalidRequest(message) => {
                assert_eq!(message, "Prompt is too long: context limit")
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn output_limit_errors_map_to_clear_anthropic_invalid_request() {
        let error = map_chatgpt_error_status_body(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"max_output_tokens is too high. Maximum supported value is 16384"}}"#.to_string(),
        );

        match error.without_upstream_metadata() {
            ProviderError::InvalidRequest(message) => {
                assert_eq!(
                    message,
                    "requested max_tokens exceeds the upstream model output limit; lower max_tokens or choose a model with a larger output budget"
                );
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn combined_input_and_max_tokens_overflow_is_not_normalized_as_prompt_too_long() {
        let error = map_chatgpt_error_status_body(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"code":"context_length_exceeded","message":"input length plus max_tokens exceeds the model context limit"}}"#.to_string(),
        );

        match error.without_upstream_metadata() {
            ProviderError::InvalidRequest(message) => {
                assert!(!message.starts_with("Prompt is too long:"));
                assert!(message.contains("max_tokens exceeds"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn output_limit_errors_preserve_payload_too_large_status() {
        let error = map_chatgpt_error_status_body(
            StatusCode::PAYLOAD_TOO_LARGE,
            "requested output tokens exceed the model limit".to_string(),
        );

        match error.without_upstream_metadata() {
            ProviderError::RequestTooLarge(message) => {
                assert!(message.contains("max_tokens exceeds"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn output_limit_error_detection_ignores_unrelated_bad_requests() {
        let error = map_chatgpt_error_status_body(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"bad tool schema"}}"#.to_string(),
        );

        match error.without_upstream_metadata() {
            ProviderError::InvalidRequest(message) => {
                assert_eq!(message, "bad tool schema");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn chatgpt_error_mapping_reads_detail_body() {
        let body = r#"{"detail":"Unsupported parameter: max_output_tokens"}"#;
        let error = map_chatgpt_error_status_body(StatusCode::BAD_REQUEST, body.to_string());
        match error.without_upstream_metadata() {
            ProviderError::InvalidRequest(message) => {
                assert_eq!(message, "Unsupported parameter: max_output_tokens");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn chatgpt_quota_body_rate_limit_maps_to_invalid_request() {
        let body = r#"{"error":{"code":"insufficient_quota","message":"quota exhausted"}}"#;
        let error = map_chatgpt_error_status_body(StatusCode::TOO_MANY_REQUESTS, body.to_string());

        match error.without_upstream_metadata() {
            ProviderError::InvalidRequest(message) => {
                assert_eq!(message, "quota exhausted");
            }
            other => panic!("unexpected error: {other}"),
        }

        let metadata = error.upstream_metadata().unwrap();
        assert_eq!(metadata.status, 429);
        assert_eq!(metadata.body_preview.as_deref(), Some(body));
    }

    #[test]
    fn chatgpt_quota_header_rate_limit_maps_to_invalid_request() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after-ms", "1500".parse().unwrap());
        headers.insert("x-request-id", "req_quota".parse().unwrap());
        headers.insert("x-ratelimit-reason", "billing".parse().unwrap());
        let body = r#"{"error":{"message":"billing issue"}}"#;
        let error = map_chatgpt_error_status_body_with_headers(
            StatusCode::TOO_MANY_REQUESTS,
            &headers,
            body.to_string(),
        );

        match error.without_upstream_metadata() {
            ProviderError::InvalidRequest(message) => {
                assert_eq!(message, "billing issue");
            }
            other => panic!("unexpected error: {other}"),
        }

        let metadata = error.upstream_metadata().unwrap();
        assert_eq!(metadata.retry_after, Some(2));
        assert_eq!(metadata.request_id.as_deref(), Some("req_quota"));
        assert!(
            metadata
                .headers
                .iter()
                .any(|header| { header.name == "x-ratelimit-reason" && header.value == "billing" })
        );
    }

    #[test]
    fn chatgpt_ordinary_rate_limit_remains_rate_limited() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", "4".parse().unwrap());
        let body = r#"{"error":{"message":"slow down"}}"#;
        let error = map_chatgpt_error_status_body_with_headers(
            StatusCode::TOO_MANY_REQUESTS,
            &headers,
            body.to_string(),
        );

        match error.without_upstream_metadata() {
            ProviderError::RateLimited { retry_after } => {
                assert_eq!(*retry_after, Some(4));
            }
            other => panic!("unexpected error: {other}"),
        }

        assert_eq!(error.upstream_metadata().unwrap().retry_after, Some(4));
    }

    #[test]
    fn chatgpt_tool_schema_budget_rejects_oversized_tool_catalog() {
        let body = json!({
            "model": "gpt-5.5",
            "input": [{"role": "user", "content": "hi"}],
            "tools": [{
                "type": "function",
                "name": "huge_tool",
                "description": "x".repeat(CHATGPT_TOOL_SCHEMA_BUDGET_BYTES + 1),
                "parameters": {"type": "object"}
            }]
        });

        let error = validate_chatgpt_tool_schema_budget(&body).unwrap_err();

        assert!(matches!(error, ProviderError::InvalidRequest(_)));
        assert!(error.to_string().contains("ToolSearch"));
    }

    fn chatgpt_test_token() -> ChatGptToken {
        ChatGptToken {
            access_token: "access".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: i64::MAX,
            account_id: Some("account".to_string()),
        }
    }

    fn chatgpt_websocket_test_body() -> Value {
        json!({
            "model": "gpt-5.3-codex",
            "instructions": "Follow the user's instructions.",
            "input": [{"role": "user", "content": "hi"}],
            "tools": [],
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "store": false,
            "stream": true,
            "include": [],
            "client_metadata": {
                "x-codex-turn-metadata": "{\"request_kind\":\"turn\",\"turn_id\":\"turn-test\"}"
            }
        })
    }

    fn chatgpt_test_prepared_request(request_id: u64) -> ChatGptPreparedRequest {
        chatgpt_test_prepared_request_with_body(
            request_id,
            chatgpt_websocket_test_body(),
            Some("conversation-test"),
        )
    }

    fn chatgpt_test_prepared_request_with_body(
        request_id: u64,
        body: Value,
        stable_client_conversation_id: Option<&str>,
    ) -> ChatGptPreparedRequest {
        ChatGptPreparedRequest {
            body,
            responses_correlation: crate::responses::ResponsesCorrelation::default(),
            marker_mode: ReasoningMarkerMode::Strict,
            compact_request: false,
            request_id,
            output_token_budget: ChatGptOutputTokenBudget::default(),
            stable_client_conversation_id: stable_client_conversation_id.map(ToOwned::to_owned),
            responses_lite: ResponsesLiteDecision::enabled(ResponsesLiteDecisionSource::ForcedOn),
            observer: None,
            pending_context_usage: None,
        }
    }

    fn test_pending_context_usage(
        provider: &ChatGptProvider,
        prepared: &ChatGptPreparedRequest,
        token: &ChatGptToken,
        session_id: &str,
    ) -> PendingContextUsage {
        let full_input = prepared.body["input"].as_array().unwrap().clone();
        PendingContextUsage {
            key: ContextUsageKey {
                provider_id: provider.id.clone(),
                account_hash: capability_cache::account_hash(token.account_id.as_deref()).unwrap(),
                model: prepared.body["model"].as_str().unwrap().to_string(),
                stable_client_conversation_id: session_id.to_string(),
            },
            static_body: context_static_body(&prepared.body),
            full_input,
            compact_kind: CompactRequestKind::None,
        }
    }

    fn websocket_response_created(id: &str) -> Value {
        json!({
            "type": "response.created",
            "response": {
                "id": id,
                "model": "gpt-5.3-codex",
                "status": "in_progress",
                "output": []
            }
        })
    }

    fn websocket_response_completed(id: &str) -> Value {
        websocket_response_completed_with_output(id, json!([]))
    }

    fn websocket_response_completed_with_output(id: &str, output: Value) -> Value {
        json!({
            "type": "response.completed",
            "response": {
                "id": id,
                "model": "gpt-5.3-codex",
                "status": "completed",
                "output": output,
                "usage": {
                    "input_tokens": 1,
                    "output_tokens": 2,
                    "total_tokens": 3
                }
            }
        })
    }

    fn websocket_response_failed(id: &str) -> Value {
        json!({
            "type": "response.failed",
            "response": {
                "id": id,
                "model": "gpt-5.3-codex",
                "status": "failed",
                "output": [],
                "error": {"message": "failed"},
                "usage": {
                    "input_tokens": 1,
                    "output_tokens": 0,
                    "total_tokens": 1
                }
            }
        })
    }

    async fn collect_stream_results(
        mut stream: BoxStream<'static, Result<ProviderEvent, ProviderError>>,
    ) -> Vec<Result<SseEvent, ProviderError>> {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            match event {
                Ok(event) => events.extend(event.into_normalized_events().into_iter().map(Ok)),
                Err(error) => events.push(Err(error)),
            }
        }
        events
    }

    #[derive(Debug)]
    struct CapturedWsHandshake {
        headers: Vec<(String, String)>,
    }

    impl CapturedWsHandshake {
        fn header(&self, name: &str) -> Option<String> {
            self.headers
                .iter()
                .find(|(header, _)| header.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.clone())
        }
    }

    async fn websocket_events_server(
        events: Vec<Value>,
    ) -> (
        String,
        Arc<StdMutex<Vec<Value>>>,
        Arc<StdMutex<Vec<CapturedWsHandshake>>>,
    ) {
        websocket_sequence_server(vec![events]).await
    }

    async fn websocket_one_request_per_connection_server(
        responses: Vec<Vec<Value>>,
    ) -> (
        String,
        Arc<StdMutex<Vec<Value>>>,
        Arc<StdMutex<Vec<CapturedWsHandshake>>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let handshakes = Arc::new(StdMutex::new(Vec::new()));
        let captured_requests = Arc::clone(&requests);
        let captured_handshakes = Arc::clone(&handshakes);

        tokio::spawn(async move {
            for events in responses {
                let (socket, _) = listener.accept().await.unwrap();
                let handshakes = Arc::clone(&captured_handshakes);
                let mut websocket = accept_hdr_async(
                    socket,
                    move |request: &WsServerRequest, response: WsServerResponse| {
                        handshakes.lock().unwrap().push(CapturedWsHandshake {
                            headers: request
                                .headers()
                                .iter()
                                .map(|(name, value)| {
                                    (
                                        name.as_str().to_string(),
                                        value.to_str().unwrap_or_default().to_string(),
                                    )
                                })
                                .collect(),
                        });
                        Ok(response)
                    },
                )
                .await
                .unwrap();

                if let Some(Ok(WsMessage::Text(text))) = websocket.next().await {
                    captured_requests
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str(&text).unwrap());
                }
                for event in events {
                    websocket
                        .send(WsMessage::Text(event.to_string().into()))
                        .await
                        .unwrap();
                }
                let _ = websocket.close(None).await;
            }
        });

        (format!("http://{addr}/responses"), requests, handshakes)
    }

    async fn websocket_sequence_server(
        responses: Vec<Vec<Value>>,
    ) -> (
        String,
        Arc<StdMutex<Vec<Value>>>,
        Arc<StdMutex<Vec<CapturedWsHandshake>>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let handshakes = Arc::new(StdMutex::new(Vec::new()));
        let captured_requests = Arc::clone(&requests);
        let captured_handshakes = Arc::clone(&handshakes);

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let handshakes = Arc::clone(&captured_handshakes);
            let mut websocket = accept_hdr_async(
                socket,
                move |request: &WsServerRequest, response: WsServerResponse| {
                    handshakes.lock().unwrap().push(CapturedWsHandshake {
                        headers: request
                            .headers()
                            .iter()
                            .map(|(name, value)| {
                                (
                                    name.as_str().to_string(),
                                    value.to_str().unwrap_or_default().to_string(),
                                )
                            })
                            .collect(),
                    });
                    Ok(response)
                },
            )
            .await
            .unwrap();

            for events in responses {
                if let Some(Ok(WsMessage::Text(text))) = websocket.next().await {
                    captured_requests
                        .lock()
                        .unwrap()
                        .push(serde_json::from_str(&text).unwrap());
                }
                for event in events {
                    websocket
                        .send(WsMessage::Text(event.to_string().into()))
                        .await
                        .unwrap();
                }
            }
            let _ = websocket.close(None).await;
        });

        (format!("http://{addr}/responses"), requests, handshakes)
    }

    async fn websocket_prewarm_failure_then_sse_server() -> (
        String,
        Arc<StdMutex<Vec<Value>>>,
        Arc<Mutex<Vec<CapturedHttpRequest>>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let websocket_requests = Arc::new(StdMutex::new(Vec::new()));
        let sse_requests = Arc::new(Mutex::new(Vec::new()));
        let captured_websocket_requests = Arc::clone(&websocket_requests);
        let captured_sse_requests = Arc::clone(&sse_requests);

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_hdr_async(
                socket,
                |_request: &WsServerRequest, response: WsServerResponse| Ok(response),
            )
            .await
            .unwrap();

            if let Some(Ok(WsMessage::Text(text))) = websocket.next().await {
                captured_websocket_requests
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&text).unwrap());
            }
            websocket
                .send(WsMessage::Text(
                    websocket_response_failed("warm-failed").to_string().into(),
                ))
                .await
                .unwrap();
            let _ = websocket.close(None).await;

            let (mut socket, _) = listener.accept().await.unwrap();
            let sse_request = read_http_request_allow_empty_body(&mut socket).await;
            captured_sse_requests.lock().await.push(sse_request);
            let response_body = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                websocket_response_created("resp-sse-prewarm-fallback"),
                websocket_response_completed("resp-sse-prewarm-fallback")
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                response_body.len(),
                response_body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        (
            format!("http://{addr}/responses"),
            websocket_requests,
            sse_requests,
        )
    }

    async fn websocket_stale_continuation_then_sse_server() -> (
        String,
        Arc<StdMutex<Vec<Value>>>,
        Arc<Mutex<Vec<CapturedHttpRequest>>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let websocket_requests = Arc::new(StdMutex::new(Vec::new()));
        let sse_requests = Arc::new(Mutex::new(Vec::new()));
        let captured_websocket_requests = Arc::clone(&websocket_requests);
        let captured_sse_requests = Arc::clone(&sse_requests);

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_hdr_async(
                socket,
                |_request: &WsServerRequest, response: WsServerResponse| Ok(response),
            )
            .await
            .unwrap();

            if let Some(Ok(WsMessage::Text(text))) = websocket.next().await {
                captured_websocket_requests
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&text).unwrap());
            }
            websocket
                .send(WsMessage::Text(
                    websocket_response_created("resp-ws-1").to_string().into(),
                ))
                .await
                .unwrap();
            websocket
                .send(WsMessage::Text(
                    websocket_response_completed_with_output(
                        "resp-ws-1",
                        json!([{
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": "hello"}]
                        }]),
                    )
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();

            if let Some(Ok(WsMessage::Text(text))) = websocket.next().await {
                captured_websocket_requests
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&text).unwrap());
            }
            websocket
                .send(WsMessage::Text(
                    json!({
                        "type": "codex.rate_limits",
                        "plan_type": "plus",
                        "rate_limits": {"primary": {"used_percent": 1}}
                    })
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();
            let stale_error = json!({
                "type": "error",
                "status": 400,
                "error": {
                    "type": "invalid_request_error",
                    "message": "Previous response with id 'resp-ws-1' not found."
                }
            });
            websocket
                .send(WsMessage::Text(stale_error.to_string().into()))
                .await
                .unwrap();
            let _ = websocket.close(None).await;

            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_hdr_async(
                socket,
                |_request: &WsServerRequest, response: WsServerResponse| Ok(response),
            )
            .await
            .unwrap();
            if let Some(Ok(WsMessage::Text(text))) = websocket.next().await {
                captured_websocket_requests
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&text).unwrap());
            }
            websocket
                .send(WsMessage::Text(stale_error.to_string().into()))
                .await
                .unwrap();
            let _ = websocket.close(None).await;

            let (mut socket, _) = listener.accept().await.unwrap();
            let sse_request = read_http_request_allow_empty_body(&mut socket).await;
            captured_sse_requests.lock().await.push(sse_request);
            let response_body = format!(
                "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                websocket_response_created("resp-sse-stale"),
                websocket_response_completed("resp-sse-stale")
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();

            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_hdr_async(
                socket,
                |_request: &WsServerRequest, response: WsServerResponse| Ok(response),
            )
            .await
            .unwrap();
            if let Some(Ok(WsMessage::Text(text))) = websocket.next().await {
                captured_websocket_requests
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&text).unwrap());
            }
            websocket
                .send(WsMessage::Text(
                    websocket_response_created("resp-ws-3").to_string().into(),
                ))
                .await
                .unwrap();
            websocket
                .send(WsMessage::Text(
                    websocket_response_completed("resp-ws-3").to_string().into(),
                ))
                .await
                .unwrap();
            let _ = websocket.close(None).await;
        });

        (
            format!("http://{addr}/responses"),
            websocket_requests,
            sse_requests,
        )
    }

    async fn websocket_abort_continuation_invalidation_server()
    -> (String, Arc<StdMutex<Vec<Value>>>, oneshot::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let captured_requests = Arc::clone(&requests);
        let (close_tx, close_rx) = oneshot::channel();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_hdr_async(
                socket,
                |_request: &WsServerRequest, response: WsServerResponse| Ok(response),
            )
            .await
            .unwrap();

            if let Some(Ok(WsMessage::Text(text))) = websocket.next().await {
                captured_requests
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&text).unwrap());
            }
            websocket
                .send(WsMessage::Text(
                    websocket_response_created("resp-ws-1").to_string().into(),
                ))
                .await
                .unwrap();
            websocket
                .send(WsMessage::Text(
                    websocket_response_completed_with_output(
                        "resp-ws-1",
                        json!([{
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": "hello"}]
                        }]),
                    )
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();

            if let Some(Ok(WsMessage::Text(text))) = websocket.next().await {
                captured_requests
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&text).unwrap());
            }
            websocket
                .send(WsMessage::Text(
                    websocket_response_created("resp-ws-abort")
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();
            let _ = websocket.next().await;
            let _ = close_tx.send(());

            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_hdr_async(
                socket,
                |_request: &WsServerRequest, response: WsServerResponse| Ok(response),
            )
            .await
            .unwrap();
            if let Some(Ok(WsMessage::Text(text))) = websocket.next().await {
                captured_requests
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&text).unwrap());
            }
            websocket
                .send(WsMessage::Text(
                    websocket_response_created("resp-ws-3").to_string().into(),
                ))
                .await
                .unwrap();
            websocket
                .send(WsMessage::Text(
                    websocket_response_completed("resp-ws-3").to_string().into(),
                ))
                .await
                .unwrap();
            let _ = websocket.close(None).await;
        });

        (format!("http://{addr}/responses"), requests, close_rx)
    }

    async fn websocket_busy_continuation_invalidation_server()
    -> (String, Arc<StdMutex<Vec<Value>>>, oneshot::Sender<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(StdMutex::new(Vec::new()));
        let captured_requests = Arc::clone(&requests);
        let (complete_second_tx, complete_second_rx) = oneshot::channel();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_hdr_async(
                socket,
                |_request: &WsServerRequest, response: WsServerResponse| Ok(response),
            )
            .await
            .unwrap();

            if let Some(Ok(WsMessage::Text(text))) = websocket.next().await {
                captured_requests
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&text).unwrap());
            }
            websocket
                .send(WsMessage::Text(
                    websocket_response_created("resp-ws-1").to_string().into(),
                ))
                .await
                .unwrap();
            websocket
                .send(WsMessage::Text(
                    websocket_response_completed_with_output(
                        "resp-ws-1",
                        json!([{
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": "hello"}]
                        }]),
                    )
                    .to_string()
                    .into(),
                ))
                .await
                .unwrap();

            if let Some(Ok(WsMessage::Text(text))) = websocket.next().await {
                captured_requests
                    .lock()
                    .unwrap()
                    .push(serde_json::from_str(&text).unwrap());
            }
            websocket
                .send(WsMessage::Text(
                    websocket_response_created("resp-ws-2").to_string().into(),
                ))
                .await
                .unwrap();

            let captured_requests_for_second_connection = Arc::clone(&captured_requests);
            let second_connection = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                let mut websocket = accept_hdr_async(
                    socket,
                    |_request: &WsServerRequest, response: WsServerResponse| Ok(response),
                )
                .await
                .unwrap();
                for response_id in ["resp-ws-3", "resp-ws-4"] {
                    if let Some(Ok(WsMessage::Text(text))) = websocket.next().await {
                        captured_requests_for_second_connection
                            .lock()
                            .unwrap()
                            .push(serde_json::from_str(&text).unwrap());
                    }
                    websocket
                        .send(WsMessage::Text(
                            websocket_response_created(response_id).to_string().into(),
                        ))
                        .await
                        .unwrap();
                    websocket
                        .send(WsMessage::Text(
                            websocket_response_completed(response_id).to_string().into(),
                        ))
                        .await
                        .unwrap();
                }
                let _ = websocket.close(None).await;
            });

            let _ = complete_second_rx.await;
            websocket
                .send(WsMessage::Text(
                    websocket_response_completed("resp-ws-2").to_string().into(),
                ))
                .await
                .unwrap();
            let _ = second_connection.await;
        });

        (
            format!("http://{addr}/responses"),
            requests,
            complete_second_tx,
        )
    }

    async fn websocket_proxy_server(
        responses: Vec<Vec<Value>>,
    ) -> (String, String, Arc<Mutex<Vec<CapturedHttpRequest>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connect_requests = Arc::new(Mutex::new(Vec::new()));
        let captured_connect_requests = Arc::clone(&connect_requests);

        tokio::spawn(async move {
            for events in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_http_request_allow_empty_body(&mut socket).await;
                captured_connect_requests.lock().await.push(request);
                socket
                    .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                    .await
                    .unwrap();

                let mut websocket = accept_hdr_async(
                    socket,
                    |_request: &WsServerRequest, response: WsServerResponse| Ok(response),
                )
                .await
                .unwrap();
                let _ = websocket.next().await;
                for event in events {
                    websocket
                        .send(WsMessage::Text(event.to_string().into()))
                        .await
                        .unwrap();
                }
                let _ = websocket.close(None).await;
            }
        });

        (
            "http://chatgpt.test/responses".to_string(),
            format!("http://{addr}"),
            connect_requests,
        )
    }

    async fn websocket_hanging_after_created_server() -> (String, oneshot::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (close_tx, close_rx) = oneshot::channel();

        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut websocket = accept_hdr_async(
                socket,
                |_request: &WsServerRequest, response: WsServerResponse| Ok(response),
            )
            .await
            .unwrap();

            let _ = websocket.next().await;
            websocket
                .send(WsMessage::Text(
                    websocket_response_created("resp-ws-abort")
                        .to_string()
                        .into(),
                ))
                .await
                .unwrap();

            let _ = websocket.next().await;
            let _ = close_tx.send(());
        });

        (format!("http://{addr}/responses"), close_rx)
    }

    async fn websocket_upgrade_required_then_sse_server()
    -> (String, Arc<Mutex<Vec<CapturedHttpRequest>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured_requests = Arc::clone(&requests);

        tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_http_request_allow_empty_body(&mut socket).await;
                captured_requests.lock().await.push(CapturedHttpRequest {
                    headers: request.headers.clone(),
                    body: request.body.clone(),
                });

                if attempt == 0 {
                    let response = "HTTP/1.1 426 Upgrade Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                    socket.write_all(response.as_bytes()).await.unwrap();
                } else {
                    let response_body = format!(
                        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                        websocket_response_created("resp-sse-1"),
                        websocket_response_completed("resp-sse-1")
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                        response_body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                }
            }
        });

        (format!("http://{addr}/responses"), requests)
    }

    async fn websocket_fallback_cooldown_retry_server()
    -> (String, Arc<Mutex<Vec<CapturedHttpRequest>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured_requests = Arc::clone(&requests);

        tokio::spawn(async move {
            for attempt in 0..4 {
                let (mut socket, _) = listener.accept().await.unwrap();

                match attempt {
                    0 => {
                        let request = read_http_request_allow_empty_body(&mut socket).await;
                        captured_requests.lock().await.push(CapturedHttpRequest {
                            headers: request.headers.clone(),
                            body: request.body.clone(),
                        });
                        let response = "HTTP/1.1 426 Upgrade Required\r\ncontent-length: 0\r\nconnection: close\r\n\r\n";
                        socket.write_all(response.as_bytes()).await.unwrap();
                    }
                    1 | 2 => {
                        let request = read_http_request_allow_empty_body(&mut socket).await;
                        captured_requests.lock().await.push(CapturedHttpRequest {
                            headers: request.headers.clone(),
                            body: request.body.clone(),
                        });
                        let response_body = format!(
                            "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
                            websocket_response_created("resp-sse-cooldown"),
                            websocket_response_completed("resp-sse-cooldown")
                        );
                        let response = format!(
                            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                            response_body.len()
                        );
                        socket.write_all(response.as_bytes()).await.unwrap();
                    }
                    3 => {
                        let captured_handshake =
                            Arc::new(StdMutex::new(None::<CapturedHttpRequest>));
                        let handshake_slot = Arc::clone(&captured_handshake);
                        let mut websocket = accept_hdr_async(
                            socket,
                            move |request: &WsServerRequest, response: WsServerResponse| {
                                let mut headers = format!("GET {} HTTP/1.1\r\n", request.uri());
                                for (name, value) in request.headers() {
                                    headers.push_str(name.as_str());
                                    headers.push_str(": ");
                                    headers.push_str(value.to_str().unwrap_or_default());
                                    headers.push_str("\r\n");
                                }
                                headers.push_str("\r\n");
                                *handshake_slot.lock().unwrap() = Some(CapturedHttpRequest {
                                    headers,
                                    body: Vec::new(),
                                });
                                Ok(response)
                            },
                        )
                        .await
                        .unwrap();
                        let handshake = captured_handshake.lock().unwrap().take().unwrap();
                        captured_requests.lock().await.push(handshake);
                        let _ = websocket.next().await;
                        websocket
                            .send(WsMessage::Text(
                                websocket_response_created("resp-ws-retry")
                                    .to_string()
                                    .into(),
                            ))
                            .await
                            .unwrap();
                        websocket
                            .send(WsMessage::Text(
                                websocket_response_completed("resp-ws-retry")
                                    .to_string()
                                    .into(),
                            ))
                            .await
                            .unwrap();
                        let _ = websocket.close(None).await;
                    }
                    _ => unreachable!(),
                }
            }
        });

        (format!("http://{addr}/responses"), requests)
    }

    async fn read_http_request_allow_empty_body(
        socket: &mut tokio::net::TcpStream,
    ) -> CapturedHttpRequest {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let read = socket.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(header_end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                let body_start = header_end + 4;
                let headers = std::str::from_utf8(&buffer[..body_start]).unwrap_or_default();
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                if buffer.len() >= body_start + content_length {
                    return CapturedHttpRequest {
                        headers: String::from_utf8_lossy(&buffer[..body_start]).to_string(),
                        body: buffer[body_start..body_start + content_length].to_vec(),
                    };
                }
            }
        }
        CapturedHttpRequest {
            headers: String::new(),
            body: Vec::new(),
        }
    }

    fn chatgpt_codex_fixture_sse_events(fixture: &str) -> Vec<Value> {
        fixture
            .split("\n\n")
            .filter_map(|frame| {
                let data = frame
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .map(str::trim_start)
                    .collect::<Vec<_>>()
                    .join("\n");
                if data.is_empty() || data == "[DONE]" {
                    return None;
                }
                Some(serde_json::from_str(&data).expect("valid SSE fixture JSON"))
            })
            .collect()
    }

    async fn test_chatgpt_provider(endpoint: String) -> ChatGptProvider {
        ChatGptProvider {
            id: "chatgpt".to_string(),
            base_url: endpoint.clone(),
            http_client: Client::new(),
            endpoint,
            models_endpoint: "http://127.0.0.1/models".to_string(),
            usage_endpoint: "http://127.0.0.1/usage".to_string(),
            installation_id: "install-test".to_string(),
            runtime_ids: Arc::new(RwLock::new(ChatGptRuntimeIds {
                session_id: "session-test".to_string(),
                thread_id: "thread-test".to_string(),
                window_id: "window-test".to_string(),
            })),
            request_headers: ChatGptRequestHeaders {
                originator: HeaderValue::from_static("opencode"),
                user_agent: HeaderValue::from_static("opencode/claude-proxy-test"),
            },
            request_policy: chatgpt_upstream_request_policy(&ProviderRuntimeConfig::default()),
            runtime: ProviderRuntimeConfig::default(),
            chatgpt_config: ChatGptProviderConfig::default(),
            proxy: None,
            extra_ca_certs: Vec::new(),
            transport: ChatGptTransport::Sse,
            websocket_sse_cooldown_until_secs: Arc::new(AtomicU64::new(0)),
            websocket_stats: ChatGptWebSocketStats::default(),
            websocket_session: Arc::new(Mutex::new(transport::ChatGptWebSocketSession::new())),
            auth: ChatGptAuth::new(Client::new(), 1024 * 1024).await.unwrap(),
            payload_limits: crate::http::ResponsePayloadLimits {
                max_response_body_bytes: 1024 * 1024,
                max_sse_frame_bytes: 1024 * 1024,
            },
            remote_models: Arc::new(RwLock::new(HashMap::new())),
            cached_rate_limits: Arc::new(Mutex::new(CachedRateLimits {
                snapshots: Vec::new(),
                fetched_at: None,
                hard_stop_generation: 0,
            })),
            context_usage: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    #[derive(Debug)]
    struct CapturedHttpRequest {
        headers: String,
        body: Vec<u8>,
    }

    fn request_body_json(request: &CapturedHttpRequest) -> Value {
        let body = if request
            .headers
            .to_ascii_lowercase()
            .contains("content-encoding: zstd")
        {
            zstd::stream::decode_all(request.body.as_slice()).unwrap()
        } else {
            request.body.clone()
        };
        serde_json::from_slice(&body).unwrap()
    }

    async fn capture_once_server() -> (String, Arc<Mutex<Vec<CapturedHttpRequest>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured_requests = Arc::clone(&requests);

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            captured_requests.lock().await.push(request);

            let response_body = r#"{"ok":true}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        (format!("http://{addr}/responses"), requests)
    }

    async fn prompt_too_long_error_server() -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured_requests = Arc::clone(&requests);

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_http_request(&mut socket).await;
            captured_requests
                .lock()
                .await
                .push(request_body_json(&request));

            let response_body =
                r#"{"error":{"code":"context_length_exceeded","message":"context limit"}}"#;
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        (format!("http://{addr}/responses"), requests)
    }

    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> CapturedHttpRequest {
        let mut buffer = Vec::new();
        let mut chunk = [0_u8; 1024];
        loop {
            let read = socket.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some((body_start, content_length)) = http_body_start_and_len(&buffer)
                && buffer.len() >= body_start + content_length
            {
                return CapturedHttpRequest {
                    headers: String::from_utf8_lossy(&buffer[..body_start]).to_string(),
                    body: buffer[body_start..body_start + content_length].to_vec(),
                };
            }
        }
        CapturedHttpRequest {
            headers: String::new(),
            body: Vec::new(),
        }
    }

    fn http_body_start_and_len(buffer: &[u8]) -> Option<(usize, usize)> {
        let header_end = buffer.windows(4).position(|window| window == b"\r\n\r\n")? + 4;
        let headers = std::str::from_utf8(&buffer[..header_end]).ok()?;
        let content_length = headers.lines().find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())
                .flatten()
        })?;
        Some((header_end, content_length))
    }
}
