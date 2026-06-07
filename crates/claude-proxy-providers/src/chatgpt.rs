//! ChatGPT account provider adapter.
//!
//! Uses the same OpenAI Auth device flow and Codex Responses endpoint that
//! opencode uses for ChatGPT Pro/Plus authentication.

mod auth;
mod responses;
mod transport;

use std::collections::BTreeSet;
use std::fs;
#[cfg(not(test))]
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use claude_proxy_config::{
    Settings,
    settings::{
        ChatGptProviderConfig, ChatGptTransport, DEFAULT_CHATGPT_ORIGINATOR,
        DEFAULT_CHATGPT_USER_AGENT, ProviderConfig, ProviderRuntimeConfig, ReasoningMarkerMode,
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
    map_upstream_response, read_upstream_response_text, send_upstream_request_with_policy,
    upstream_error_metadata_from_parts,
};
use crate::openai_compat::{
    apply_openai_intent, is_compact_request_body, log_compact_request_observability,
    log_request_observability,
};
use crate::provider::{
    Provider, ProviderError, ProviderRequestMetadata, ProviderRequestObserver,
    ProviderRequestObserverEvent, ProviderRequestObserverEventKind, RateLimitCredits,
    RateLimitSnapshot, RateLimitSource, RateLimitWindow,
};
use crate::reasoning_markers::marker_mode_from_request;
use tracing::{info, warn};

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const DEFAULT_CHATGPT_INSTRUCTIONS: &str = "Follow the user's instructions.";
const CHATGPT_SSE_RESPONSE_HEADER_TIMEOUT: Duration = Duration::from_secs(10);
const CHATGPT_SEND_MAX_ATTEMPTS: usize = 2;
const CHATGPT_USAGE_FETCH_INTERVAL: Duration = Duration::from_secs(60);
const CHATGPT_CONTEXT_LIMIT_FALLBACK_PREFLIGHT_BODY_BYTES: usize = 700 * 1024;
const CHATGPT_REQUEST_WARNING_RATIO: usize = 80;
const CHATGPT_BYTES_PER_ESTIMATED_TOKEN: usize = 4;
const CHATGPT_TOOL_SCHEMA_BUDGET_BYTES: usize = 256 * 1024;
const CHATGPT_SYNTHETIC_SESSION_HASH_BYTES: usize = 16;
const CHATGPT_WEBSOCKET_SERVER_ERROR_COOLDOWN_SECS: u64 = 120;
const CHATGPT_WEBSOCKET_STARTUP_FAILURE_COOLDOWN_SECS: u64 = 120;
const CODEX_FAST_SERVICE_TIER: &str = "priority";

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
    additional_rate_limits: Option<Vec<AdditionalRateLimitPayload>>,
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
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ChatGptOutputTokenBudget {
    requested: Option<u64>,
    effective: Option<u64>,
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

#[derive(Clone)]
struct ChatGptPreparedRequest {
    body: Value,
    marker_mode: ReasoningMarkerMode,
    compact_request: bool,
    request_id: u64,
    output_token_budget: ChatGptOutputTokenBudget,
    stable_client_conversation_id: Option<String>,
    observer: Option<ProviderRequestObserver>,
}

struct ChatGptUpstreamEventContext {
    request_id: u64,
    compact_request: bool,
    transport: &'static str,
    first_upstream_event_seen: Arc<AtomicBool>,
    thinking_diagnostics: Arc<ChatGptThinkingDiagnostics>,
    stream_started_at: Instant,
    observer: Option<ProviderRequestObserver>,
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
        self.log_terminal_event(event);
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
    http_client: Client,
    endpoint: String,
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
    cached_rate_limits: Arc<Mutex<CachedRateLimits>>,
}

impl ChatGptProvider {
    pub async fn new(
        id: &str,
        config: &ProviderConfig,
        settings: &Settings,
    ) -> Result<Self, ProviderError> {
        let http_client = build_http_client(&config.proxy, settings)?;
        let auth = ChatGptAuth::new(http_client.clone()).await?;
        let chatgpt_config = config.chatgpt.clone().unwrap_or_default();
        let transport = chatgpt_config.transport;

        Ok(Self {
            id: id.to_string(),
            http_client,
            endpoint: codex_responses_endpoint(&config.base_url),
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
            cached_rate_limits: Arc::new(Mutex::new(CachedRateLimits {
                snapshots: Vec::new(),
                fetched_at: None,
            })),
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

    fn codex_service_tier(&self) -> Option<&str> {
        effective_codex_service_tier(
            self.runtime.openai.service_tier.as_deref(),
            self.chatgpt_config.fast_mode,
        )
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

    async fn send_responses_request(
        &self,
        body: &Value,
        token: &ChatGptToken,
        compact_request: bool,
        request_id: u64,
        prompt_too_long_attempt: usize,
        budget: ChatGptOutputTokenBudget,
    ) -> Result<Response, ProviderError> {
        let body_bytes = json_len(body);
        let model = body
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        warn_if_request_nears_context_window(
            request_id,
            compact_request,
            prompt_too_long_attempt,
            model,
            body_bytes,
        );
        let started_at = Instant::now();
        let runtime_ids = self.runtime_ids_snapshot();
        let client_request_id = chatgpt_runtime_id();
        info!(
            request_id,
            compact_request,
            prompt_too_long_attempt,
            model,
            body_bytes,
            upstream_request_id = %client_request_id,
            session_id = %runtime_ids.session_id,
            thread_id = %runtime_ids.thread_id,
            window_id = %runtime_ids.window_id,
            requested_output_tokens = budget.requested.unwrap_or(0),
            requested_output_tokens_present = budget.requested.is_some(),
            effective_output_tokens = budget.effective.unwrap_or(0),
            effective_output_tokens_present = budget.effective.is_some(),
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
            .header("session-id", runtime_ids.session_id)
            .header("thread-id", runtime_ids.thread_id)
            .header("x-codex-window-id", runtime_ids.window_id);

        if let Some(account_id) = token.account_id.as_deref() {
            request_builder = request_builder.header("ChatGPT-Account-Id", account_id);
        }

        let request_builder = apply_runtime_request_config(request_builder, &self.runtime)?;
        let result =
            send_upstream_request_with_policy(request_builder.json(body), self.request_policy)
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
        compact_request: bool,
        request_id: u64,
        budget: ChatGptOutputTokenBudget,
        _observer: Option<&ProviderRequestObserver>,
    ) -> Result<Response, ProviderError> {
        validate_chatgpt_tool_schema_budget(body)?;
        let body_bytes = json_len(body);
        let model = body
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        notify_request_metadata_observer(
            _observer,
            ProviderRequestMetadata {
                transport: Some("sse".to_string()),
                request_body_bytes: Some(body_bytes as u64),
                upstream_send_body_bytes: Some(body_bytes as u64),
                ..ProviderRequestMetadata::default()
            },
        );
        let threshold = chatgpt_context_limit_preflight_threshold(model);
        if body_bytes >= threshold.body_bytes {
            warn!(
                request_id,
                compact_request,
                model,
                body_bytes,
                threshold_bytes = threshold.body_bytes,
                threshold_source = threshold.source,
                model_context_window = threshold.context_window.unwrap_or(0),
                "ChatGPT request exceeded local context-limit preflight threshold"
            );
            return Err(simulated_chatgpt_context_limit_error(
                body_bytes,
                threshold.body_bytes,
            ));
        }

        let current_budget = ChatGptOutputTokenBudget {
            requested: budget.requested,
            effective: body.get("max_output_tokens").and_then(Value::as_u64),
        };
        let response = self
            .send_responses_request(body, token, compact_request, request_id, 0, current_budget)
            .await?;
        let status = response.status();
        if status.is_success() || status == StatusCode::UNAUTHORIZED {
            if compact_request {
                info!(
                    request_id,
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
        let error_body = read_upstream_response_text(response).await?;
        Err(map_chatgpt_error_status_body_with_headers(
            status, &headers, error_body,
        ))
    }

    async fn chat_prepared_with_token(
        &self,
        prepared: ChatGptPreparedRequest,
        token: ChatGptToken,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
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
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let ChatGptPreparedRequest {
            mut body,
            marker_mode,
            compact_request,
            request_id,
            output_token_budget,
            observer,
            ..
        } = prepared;
        let mut response = self
            .send_responses_request_with_prompt_too_long_retry(
                &mut body,
                &token,
                compact_request,
                request_id,
                output_token_budget,
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
                    compact_request,
                    request_id,
                    output_token_budget,
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
            .stream_sse_response(response, marker_mode, request_id, compact_request, observer)
            .await)
    }

    async fn chat_via_websocket_with_auth_retry(
        &self,
        prepared: ChatGptPreparedRequest,
        token: ChatGptToken,
    ) -> Result<
        BoxStream<'static, Result<SseEvent, ProviderError>>,
        transport::ChatGptWebSocketStartError,
    > {
        let first = self.start_websocket_stream(prepared.clone(), &token).await;
        match first {
            Ok(stream) => Ok(stream),
            Err(error) if error.error.is_authentication() => {
                let refreshed = match self.auth.force_refresh_token().await {
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
                let retried = self.start_websocket_stream(prepared, &refreshed).await;
                if let Err(error) = &retried
                    && error.error.is_authentication()
                {
                    self.auth.clear_token().await;
                }
                retried
            }
            Err(error) => Err(error),
        }
    }

    async fn start_websocket_stream(
        &self,
        prepared: ChatGptPreparedRequest,
        token: &ChatGptToken,
    ) -> Result<
        BoxStream<'static, Result<SseEvent, ProviderError>>,
        transport::ChatGptWebSocketStartError,
    > {
        let ChatGptPreparedRequest {
            body,
            marker_mode,
            compact_request,
            request_id,
            observer,
            stable_client_conversation_id,
            ..
        } = prepared;
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
        });
        let websocket_start = transport::open_websocket_stream(
            self,
            body,
            token,
            marker_mode,
            stable_client_conversation_id.as_deref(),
            request_id,
            on_event,
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
        marker_mode: claude_proxy_config::settings::ReasoningMarkerMode,
        request_id: u64,
        compact_request: bool,
        observer: Option<ProviderRequestObserver>,
    ) -> BoxStream<'static, Result<SseEvent, ProviderError>> {
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
        });
        let stream = responses::stream_response_with_marker_mode(response, marker_mode, on_event);
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
    if snapshots.is_empty() {
        return;
    }

    let mut cached = cache.lock().await;
    for snapshot in snapshots {
        if let Some(existing) = cached.snapshots.iter_mut().find(|existing| {
            rate_limit_snapshot_key(existing) == rate_limit_snapshot_key(&snapshot)
        }) {
            *existing = merge_rate_limit_snapshot(existing.clone(), snapshot);
        } else {
            cached.snapshots.push(snapshot);
        }
    }
    cached.fetched_at = Some(Instant::now());
}

fn rate_limit_snapshot_key(snapshot: &RateLimitSnapshot) -> String {
    snapshot
        .feature
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or("codex")
        .to_string()
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
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        self.chat_with_observer(request, None).await
    }

    async fn chat_with_observer(
        &self,
        request: MessagesRequest,
        observer: Option<ProviderRequestObserver>,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let token = self.auth.get_existing_token().await?;
        let request = apply_openai_intent(request);
        let (request, synthetic_stable_client_conversation_id) =
            ensure_chatgpt_stable_client_conversation_id(request);
        let marker_mode = marker_mode_from_request(&request);
        let prompt_cache_key_source = responses::prompt_cache_key_source(&request);
        let stable_client_conversation_id =
            responses::stable_client_conversation_id_for_continuation(&request);
        let body = responses::build_body_with_context(
            &request,
            DEFAULT_CHATGPT_INSTRUCTIONS,
            responses::CodexRequestContext {
                installation_id: Some(&self.installation_id),
                service_tier: self.codex_service_tier(),
            },
        );
        let request_id = next_chatgpt_request_id();
        validate_chatgpt_tool_schema_budget(&body)?;
        let output_token_budget = chatgpt_output_token_budget(&request, &body);
        info!(
            request_id,
            prompt_cache_key_source = prompt_cache_key_source.as_str(),
            prompt_cache_key_present = body.get("prompt_cache_key").is_some(),
            stable_client_conversation_id_present = stable_client_conversation_id.is_some(),
            synthetic_stable_client_conversation_id,
            "ChatGPT prompt cache key policy applied"
        );
        log_request_observability("chatgpt", "/responses", &body, Some(request_id));
        let compact_request = is_compact_request_body(&body);
        log_compact_request_observability("chatgpt", "/responses", &body, compact_request);

        self.chat_prepared_with_token(
            ChatGptPreparedRequest {
                body,
                marker_mode,
                compact_request,
                request_id,
                output_token_budget,
                stable_client_conversation_id,
                observer,
            },
            token,
        )
        .await
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(chatgpt_models())
    }

    async fn rate_limit_snapshots(&self) -> Result<Vec<RateLimitSnapshot>, ProviderError> {
        if let Some(snapshots) = self.fresh_cached_rate_limits().await {
            return Ok(snapshots);
        }

        match self.fetch_usage_rate_limits().await {
            Ok(snapshots) => {
                self.cache_rate_limits(snapshots.clone()).await;
                Ok(snapshots)
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
    stream: BoxStream<'static, Result<SseEvent, ProviderError>>,
    request_id: u64,
    compact_request: bool,
    transport: &'static str,
    stream_started_at: Instant,
    first_upstream_event_seen: Arc<AtomicBool>,
    thinking_diagnostics: Arc<ChatGptThinkingDiagnostics>,
) -> BoxStream<'static, Result<SseEvent, ProviderError>> {
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
                        event = %event.event,
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

fn downstream_thinking_delta_len(event: &SseEvent) -> Option<usize> {
    (event.event == "content_block_delta"
        && event.data["delta"]["type"].as_str() == Some("thinking_delta"))
    .then(|| event.data["delta"]["thinking"].as_str().map_or(0, str::len))
}

fn sse_event_finishes_message(event: &SseEvent) -> bool {
    event.event == "message_stop" || event.data["type"].as_str() == Some("message_stop")
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
        attempt_timeout: Some(CHATGPT_SSE_RESPONSE_HEADER_TIMEOUT),
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
        plan_type: event
            .get("plan_type")
            .and_then(Value::as_str)
            .map(str::to_string),
        rate_limit_reached_type: None,
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
fn build_chatgpt_responses_body_with_codex_context(
    request: &MessagesRequest,
    context: responses::CodexRequestContext<'_>,
) -> Value {
    responses::build_body_with_context(request, DEFAULT_CHATGPT_INSTRUCTIONS, context)
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

fn simulated_chatgpt_context_limit_error(
    body_bytes: usize,
    threshold_bytes: usize,
) -> ProviderError {
    let body = serde_json::json!({
        "type": "error",
        "error": {
            "type": "invalid_request_error",
            "code": "context_length_exceeded",
            "param": "input",
            "message": format!(
                "The input exceeds the model context window (serialized request body is {body_bytes} bytes; local preflight threshold is {threshold_bytes} bytes)."
            )
        }
    })
    .to_string();
    map_chatgpt_error_status_body_with_headers(StatusCode::BAD_REQUEST, &HeaderMap::new(), body)
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

    let metadata =
        upstream_error_metadata_from_parts(status.as_u16(), headers, &body, message.clone());
    let error = if is_prompt_too_long_error(status, &body) {
        ProviderError::RequestTooLarge(message)
    } else {
        match status {
            StatusCode::BAD_REQUEST => ProviderError::InvalidRequest(message),
            StatusCode::UNAUTHORIZED => ProviderError::Authentication(message),
            StatusCode::NOT_FOUND => ProviderError::ModelNotFound(message),
            StatusCode::PAYLOAD_TOO_LARGE => ProviderError::RequestTooLarge(message),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChatGptContextLimitPreflightThreshold {
    body_bytes: usize,
    source: &'static str,
    context_window: Option<u32>,
}

fn chatgpt_context_limit_preflight_threshold(model: &str) -> ChatGptContextLimitPreflightThreshold {
    if let Some(context_window) =
        chatgpt_model_info(model).and_then(|info| info.capabilities.limits.context_window)
    {
        return ChatGptContextLimitPreflightThreshold {
            body_bytes: (context_window as usize).saturating_mul(CHATGPT_BYTES_PER_ESTIMATED_TOKEN),
            source: "model_context_window",
            context_window: Some(context_window),
        };
    }

    ChatGptContextLimitPreflightThreshold {
        body_bytes: CHATGPT_CONTEXT_LIMIT_FALLBACK_PREFLIGHT_BODY_BYTES,
        source: "fallback_body_bytes",
        context_window: None,
    }
}

fn chatgpt_request_warning_threshold(model: &str) -> Option<usize> {
    let context_window = chatgpt_model_info(model)?
        .capabilities
        .limits
        .context_window? as usize;
    Some(
        context_window
            .saturating_mul(CHATGPT_REQUEST_WARNING_RATIO)
            .saturating_div(100)
            .saturating_mul(CHATGPT_BYTES_PER_ESTIMATED_TOKEN),
    )
}

fn request_size_warning(model: &str, body_bytes: usize) -> Option<(usize, usize)> {
    let threshold_bytes = chatgpt_request_warning_threshold(model)?;
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
    body_bytes: usize,
) {
    let Some((threshold_bytes, estimated_tokens)) = request_size_warning(model, body_bytes) else {
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
}

const CHATGPT_MODEL_SPECS: &[ChatGptModelSpec] = &[
    ChatGptModelSpec {
        model_id: "gpt-5.5",
        context_window: 272_000,
        image_input: true,
    },
    ChatGptModelSpec {
        model_id: "gpt-5.4",
        context_window: 272_000,
        image_input: true,
    },
    ChatGptModelSpec {
        model_id: "gpt-5.4-mini",
        context_window: 272_000,
        image_input: true,
    },
    ChatGptModelSpec {
        model_id: "gpt-5.3-codex-spark",
        context_window: 128_000,
        image_input: false,
    },
];

fn chatgpt_models() -> Vec<ModelInfo> {
    CHATGPT_MODEL_SPECS
        .iter()
        .copied()
        .map(chatgpt_model_info_from_spec)
        .collect()
}

fn chatgpt_model_info(model_id: &str) -> Option<ModelInfo> {
    CHATGPT_MODEL_SPECS
        .iter()
        .copied()
        .find(|spec| spec.model_id == model_id)
        .map(chatgpt_model_info_from_spec)
}

fn chatgpt_model_info_from_spec(spec: ChatGptModelSpec) -> ModelInfo {
    let reasoning_efforts = ["low", "medium", "high", "xhigh"]
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();

    ModelInfo {
        model_id: spec.model_id.to_string(),
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
                    image: CapabilityState::from_bool(Some(spec.image_input)),
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
                context_window: Some(spec.context_window),
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
            supported_parameters: vec![
                "system".to_string(),
                "messages".to_string(),
                "stream".to_string(),
                "tools".to_string(),
                "tool_choice".to_string(),
                "thinking".to_string(),
                "reasoning_effort".to_string(),
                "prompt_cache_key".to_string(),
                "parallel_tool_calls".to_string(),
                "service_tier".to_string(),
                "verbosity".to_string(),
            ],
        },
    }
}

#[cfg(test)]
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
        assert_eq!(snapshot.source, RateLimitSource::StreamEvent);
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
        let threshold = chatgpt_request_warning_threshold("gpt-5.5").unwrap();

        assert_eq!(
            threshold,
            272_000 * CHATGPT_BYTES_PER_ESTIMATED_TOKEN * 80 / 100
        );
        assert!(request_size_warning("gpt-5.5", threshold - 1).is_none());
        assert_eq!(
            request_size_warning("gpt-5.5", threshold),
            Some((threshold, threshold / CHATGPT_BYTES_PER_ESTIMATED_TOKEN))
        );
        assert!(request_size_warning("unknown-model", threshold).is_none());
    }

    #[test]
    fn chatgpt_models_use_dedicated_codex_capability_contract() {
        let models = chatgpt_models();
        let ids = models
            .iter()
            .map(|model| model.model_id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec!["gpt-5.5", "gpt-5.4", "gpt-5.4-mini", "gpt-5.3-codex-spark",]
        );

        let gpt55 = models
            .iter()
            .find(|model| model.model_id == "gpt-5.5")
            .expect("gpt-5.5 model");

        assert_eq!(gpt55.capabilities.limits.max_output_tokens, None);
        assert_eq!(gpt55.capabilities.limits.context_window, Some(272_000));
        assert!(gpt55.capabilities.endpoints.openai_responses.is_supported());
        assert_eq!(
            gpt55.capabilities.endpoints.openai_chat_completions,
            CapabilityState::Unsupported
        );
        assert!(gpt55.capabilities.modalities.input.image.is_supported());
        assert_eq!(
            gpt55.capabilities.features.stop_sequences,
            CapabilityState::Unknown
        );
        assert_eq!(
            gpt55.capabilities.features.sampling,
            CapabilityState::Unknown
        );
        assert!(
            !gpt55
                .capabilities
                .supported_parameters
                .contains(&"max_tokens".to_string())
        );
        assert!(
            !gpt55
                .capabilities
                .supported_parameters
                .contains(&"stop_sequences".to_string())
        );
        assert!(
            gpt55
                .capabilities
                .supported_parameters
                .contains(&"service_tier".to_string())
        );
        assert_eq!(
            gpt55.capabilities.quality.tool_search.state,
            CapabilityState::Unsupported
        );
        assert_eq!(
            gpt55.capabilities.quality.prompt_cache.scope,
            PromptCacheScope::Basic
        );
        assert!(gpt55.capabilities.quality.structured_outputs.is_supported());
        assert!(gpt55.capabilities.quality.fast_mode.is_supported());
        assert_eq!(
            gpt55.capabilities.quality.token_counting.mode,
            TokenCountingMode::Rough
        );
        assert_eq!(
            gpt55.capabilities.limits.reasoning_effort_levels,
            vec!["low", "medium", "high", "xhigh"]
        );

        let spark = models
            .iter()
            .find(|model| model.model_id == "gpt-5.3-codex-spark")
            .expect("codex spark model");
        assert_eq!(
            spark.capabilities.modalities.input.image,
            CapabilityState::Unsupported
        );
        assert_eq!(spark.capabilities.limits.context_window, Some(128_000));
    }

    #[test]
    fn chatgpt_request_policy_caps_first_response_wait() {
        let policy = chatgpt_upstream_request_policy(&ProviderRuntimeConfig::default());

        assert_eq!(policy.max_attempts, 2);
        assert_eq!(policy.attempt_timeout, Some(Duration::from_secs(10)));
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

        let body = build_chatgpt_responses_body(&req);

        assert_eq!(body["tools"], json!([]));
        assert_eq!(body["include"], json!([]));
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["parallel_tool_calls"], false);
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
        assert_eq!(client_metadata.len(), 1);
    }

    #[test]
    fn chatgpt_responses_body_adds_codex_request_options() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("parallel_tool_calls".to_string(), json!(false));
        extra.insert("verbosity".to_string(), json!("high"));
        extra.insert("service_tier".to_string(), json!("priority"));
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
            }]),
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra,
        };

        let body = build_chatgpt_responses_body_with_codex_context(
            &req,
            responses::CodexRequestContext {
                installation_id: None,
                service_tier: Some("flex"),
            },
        );

        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["text"], json!({"verbosity": "high"}));
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
            },
        );

        assert!(body.get("prompt_cache_key").is_none());
        assert_eq!(body["service_tier"], "priority");
        let client_metadata = body["client_metadata"].as_object().unwrap();
        assert_eq!(
            client_metadata.get("x-codex-installation-id"),
            Some(&json!("install-123"))
        );
        assert_eq!(client_metadata.len(), 1);
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
            }]),
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_body(&req);

        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "Read");
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(
            body["tools"][0]["parameters"],
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
    fn chatgpt_responses_body_defaults_reasoning_summary_to_auto() {
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

        let body = build_chatgpt_responses_body(&req);

        assert_eq!(body["reasoning"]["effort"], "xhigh");
        assert_eq!(body["reasoning"]["summary"], "auto");
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
        let body = build_chatgpt_responses_body(&req);

        assert_eq!(body["model"], "gpt-5.4-mini");
        assert_eq!(body["instructions"], DEFAULT_CHATGPT_INSTRUCTIONS);
        assert_eq!(body["reasoning"]["effort"], "none");
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

    #[test]
    fn context_limit_preflight_threshold_uses_model_capability_then_fallback() {
        let gpt55 = chatgpt_context_limit_preflight_threshold("gpt-5.5");
        assert_eq!(
            gpt55.body_bytes,
            272_000 * CHATGPT_BYTES_PER_ESTIMATED_TOKEN
        );
        assert_eq!(gpt55.source, "model_context_window");
        assert_eq!(gpt55.context_window, Some(272_000));

        let spark = chatgpt_context_limit_preflight_threshold("gpt-5.3-codex-spark");
        assert_eq!(
            spark.body_bytes,
            128_000 * CHATGPT_BYTES_PER_ESTIMATED_TOKEN
        );
        assert_eq!(spark.source, "model_context_window");
        assert_eq!(spark.context_window, Some(128_000));

        let unknown = chatgpt_context_limit_preflight_threshold("unknown-model");
        assert_eq!(
            unknown.body_bytes,
            CHATGPT_CONTEXT_LIMIT_FALLBACK_PREFLIGHT_BODY_BYTES
        );
        assert_eq!(unknown.source, "fallback_body_bytes");
        assert_eq!(unknown.context_window, None);
    }

    #[tokio::test]
    async fn chatgpt_local_context_limit_preflight_returns_request_too_large_without_upstream_call()
    {
        let (endpoint, requests) = capture_once_server().await;
        let provider = test_chatgpt_provider(endpoint).await;
        let token = ChatGptToken {
            access_token: "access".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at: i64::MAX,
            account_id: Some("account".to_string()),
        };
        let mut body = json!({
            "model": "gpt-5.3-codex-spark",
            "input": [{"role": "user", "content": "x".repeat(CHATGPT_CONTEXT_LIMIT_FALLBACK_PREFLIGHT_BODY_BYTES)}],
            "stream": true
        });

        let error = provider
            .send_responses_request_with_prompt_too_long_retry(
                &mut body,
                &token,
                false,
                1,
                ChatGptOutputTokenBudget::default(),
                None,
            )
            .await
            .expect_err("oversized body should return a context-limit error");

        match error.without_upstream_metadata() {
            ProviderError::RequestTooLarge(message) => {
                assert!(message.contains("exceeds the model context window"));
            }
            other => panic!("unexpected error: {other}"),
        }
        assert_eq!(
            error.upstream_metadata().map(|metadata| metadata.status),
            Some(400)
        );
        assert!(requests.lock().await.is_empty());
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
            "stream": true
        });

        let response = provider
            .send_responses_request(
                &body,
                &token,
                false,
                1,
                0,
                ChatGptOutputTokenBudget {
                    requested: Some(4096),
                    effective: body.get("max_output_tokens").and_then(Value::as_u64),
                },
            )
            .await
            .expect("request should succeed");

        assert!(response.status().is_success());
        let requests = requests.lock().await;
        let headers = requests[0].headers.to_ascii_lowercase();
        assert!(headers.contains("accept: text/event-stream"));
        assert!(headers.contains("authorization: bearer access"));
        assert!(headers.contains("chatgpt-account-id: account"));
        assert!(headers.contains("x-client-request-id: "));
        assert!(!headers.contains("x-client-request-id: thread-test"));
        assert!(headers.contains("session-id: session-test"));
        assert!(headers.contains("thread-id: thread-test"));
        assert!(headers.contains("x-codex-window-id: window-test"));
        let request_body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(request_body["model"], "gpt-5.3-codex");
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

        let websocket_requests = websocket_requests.lock().unwrap();
        assert_eq!(websocket_requests.len(), 1);
        assert_eq!(websocket_requests[0]["generate"], false);
        let sse_requests = sse_requests.lock().await;
        assert_eq!(sse_requests.len(), 1);
        let sse_body: Value = serde_json::from_slice(&sse_requests[0].body).unwrap();
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

        let websocket_requests = websocket_requests.lock().unwrap();
        assert_eq!(websocket_requests.len(), 3);
        assert!(websocket_requests[0].get("previous_response_id").is_none());
        assert_eq!(websocket_requests[1]["previous_response_id"], "resp-ws-1");
        assert_eq!(websocket_requests[1]["input"], json!([second_delta]));
        assert!(websocket_requests[2].get("previous_response_id").is_none());
        assert_eq!(
            websocket_requests[2]["input"].as_array().map(Vec::len),
            Some(3)
        );

        let sse_requests = sse_requests.lock().await;
        assert_eq!(sse_requests.len(), 1);
        let sse_body: Value = serde_json::from_slice(&sse_requests[0].body).unwrap();
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

        for (request_id, body) in [(14, chatgpt_websocket_test_body()), (15, second_body)] {
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
            assert!(events.iter().all(Result::is_ok));
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
        assert_eq!(first_event.event, "message_start");
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
        assert_eq!(first_event.event, "message_start");

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
        assert_eq!(first.event, "message_start");
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
                false,
                1,
                ChatGptOutputTokenBudget::default(),
                None,
            )
            .await
            .expect_err("upstream context-limit error should not be retried");

        match error.without_upstream_metadata() {
            ProviderError::RequestTooLarge(message) => assert_eq!(message, "context limit"),
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
    fn context_length_errors_map_to_request_too_large_even_with_http_200() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","code":"context_length_exceeded","message":"Your input exceeds the context window of this model."}}"#;
        let error = map_chatgpt_error_status_body(StatusCode::OK, body.to_string());

        match error.without_upstream_metadata() {
            ProviderError::RequestTooLarge(message) => {
                assert!(message.contains("exceeds the context window"));
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn chatgpt_stream_context_length_error_maps_to_request_too_large() {
        let body = r#"{"type":"error","error":{"code":"context_length_exceeded","message":"context limit"}}"#;
        let error = map_chatgpt_stream_error(ProviderError::UpstreamError {
            status: 200,
            body: body.to_string(),
        });

        match error.without_upstream_metadata() {
            ProviderError::RequestTooLarge(message) => assert_eq!(message, "context limit"),
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
            "include": []
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
            marker_mode: ReasoningMarkerMode::Strict,
            compact_request: false,
            request_id,
            output_token_budget: ChatGptOutputTokenBudget::default(),
            stable_client_conversation_id: stable_client_conversation_id.map(ToOwned::to_owned),
            observer: None,
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
        mut stream: BoxStream<'static, Result<SseEvent, ProviderError>>,
    ) -> Vec<Result<SseEvent, ProviderError>> {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
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
            http_client: Client::new(),
            endpoint,
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
            auth: ChatGptAuth::new(Client::new()).await.unwrap(),
            cached_rate_limits: Arc::new(Mutex::new(CachedRateLimits {
                snapshots: Vec::new(),
                fetched_at: None,
            })),
        }
    }

    #[derive(Debug)]
    struct CapturedHttpRequest {
        headers: String,
        body: Vec<u8>,
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
            let body = read_http_request_body(&mut socket).await;
            captured_requests
                .lock()
                .await
                .push(serde_json::from_slice(&body).unwrap());

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

    async fn read_http_request_body(socket: &mut tokio::net::TcpStream) -> Vec<u8> {
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
                return buffer[body_start..body_start + content_length].to_vec();
            }
        }
        Vec::new()
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
