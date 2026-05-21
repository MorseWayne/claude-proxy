//! ChatGPT account provider adapter.
//!
//! Uses the same OpenAI Auth device flow and Codex Responses endpoint that
//! opencode uses for ChatGPT Pro/Plus authentication.

mod auth;
mod responses;

use std::collections::BTreeSet;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use claude_proxy_config::{
    Settings,
    settings::{ChatGptIdentityPreset, ChatGptProviderConfig, ProviderConfig},
};
use claude_proxy_core::*;
use futures::{StreamExt, stream::BoxStream};
use reqwest::{
    Client, Response, StatusCode,
    header::{HeaderMap, HeaderValue},
};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::http::{
    UpstreamRequestPolicy, apply_extra_ca_certs, fmt_reqwest_err, map_upstream_response,
    read_upstream_response_text, send_upstream_request_with_policy,
};
use crate::openai_compat::{
    apply_openai_intent, is_compact_request_body, log_compact_request_observability,
    log_request_observability, openai_model_info,
};
use crate::provider::{
    Provider, ProviderError, ProviderRequestObserver, ProviderRequestObserverEvent,
    ProviderRequestObserverEventKind, RateLimitCredits, RateLimitSnapshot, RateLimitSource,
    RateLimitWindow,
};
use crate::reasoning_markers::marker_mode_from_request;
use tracing::{info, warn};

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const DEFAULT_CHATGPT_INSTRUCTIONS: &str = "Follow the user's instructions.";
const CHATGPT_SEND_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(60);
const CHATGPT_SEND_MAX_ATTEMPTS: usize = 2;
const CHATGPT_USAGE_FETCH_INTERVAL: Duration = Duration::from_secs(60);
const CHATGPT_PTL_RETRY_MARKER: &str =
    "[earlier conversation truncated for ChatGPT prompt-too-long retry]";
const CHATGPT_PTL_MAX_RETRIES: usize = 3;
const CHATGPT_PTL_FALLBACK_DROP_DIVISOR: usize = 5;

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
    identity_preset: ChatGptIdentityPreset,
    originator: HeaderValue,
    user_agent: HeaderValue,
}

pub use auth::{ChatGptAuth, ChatGptToken, DeviceCodeInfo};

pub struct ChatGptProvider {
    id: String,
    http_client: Client,
    endpoint: String,
    usage_endpoint: String,
    installation_id: String,
    session_id: String,
    thread_id: String,
    window_id: String,
    request_headers: ChatGptRequestHeaders,
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

        Ok(Self {
            id: id.to_string(),
            http_client,
            endpoint: codex_responses_endpoint(&config.base_url),
            usage_endpoint: codex_usage_endpoint(&config.base_url),
            installation_id: chatgpt_installation_id(),
            session_id: chatgpt_runtime_id(),
            thread_id: chatgpt_runtime_id(),
            window_id: chatgpt_runtime_id(),
            request_headers: chatgpt_request_headers(&chatgpt_config)?,
            auth,
            cached_rate_limits: Arc::new(Mutex::new(CachedRateLimits {
                snapshots: Vec::new(),
                fetched_at: None,
            })),
        })
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
        let started_at = Instant::now();
        info!(
            request_id,
            compact_request,
            prompt_too_long_attempt,
            model,
            body_bytes,
            upstream_request_id = %self.thread_id,
            session_id = %self.session_id,
            thread_id = %self.thread_id,
            window_id = %self.window_id,
            identity_preset = self.request_headers.identity_preset.as_str(),
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
            .header("x-client-request-id", self.thread_id.as_str())
            .header("session-id", self.session_id.as_str())
            .header("thread-id", self.thread_id.as_str())
            .header("x-codex-window-id", self.window_id.as_str());

        if let Some(account_id) = token.account_id.as_deref() {
            request_builder = request_builder.header("ChatGPT-Account-Id", account_id);
        }

        let result = send_upstream_request_with_policy(
            request_builder.json(body),
            chatgpt_upstream_request_policy(),
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
        compact_request: bool,
        request_id: u64,
        budget: ChatGptOutputTokenBudget,
        observer: Option<&ProviderRequestObserver>,
    ) -> Result<Response, ProviderError> {
        let mut prompt_too_long_attempts = 0;

        loop {
            let response = self
                .send_responses_request(
                    body,
                    token,
                    compact_request,
                    request_id,
                    prompt_too_long_attempts,
                    budget,
                )
                .await?;
            let status = response.status();
            if status.is_success() || status == StatusCode::UNAUTHORIZED {
                if compact_request {
                    info!(
                        request_id,
                        status = status.as_u16(),
                        prompt_too_long_retry_triggered = prompt_too_long_attempts > 0,
                        prompt_too_long_retries = prompt_too_long_attempts,
                        "Compact request prompt-too-long retry result"
                    );
                }
                return Ok(response);
            }

            if !is_prompt_too_long_candidate_status(status) {
                return Err(map_upstream_response(response).await);
            }

            let error_body = read_upstream_response_text(response).await?;
            if !is_prompt_too_long_error(status, &error_body) {
                return Err(map_chatgpt_error_status_body(status, error_body));
            }

            if prompt_too_long_attempts >= CHATGPT_PTL_MAX_RETRIES {
                notify_prompt_too_long_observer(
                    observer,
                    ProviderRequestObserverEventKind::PromptTooLongRetryExhausted,
                    prompt_too_long_attempts as u64,
                    None,
                );
                if compact_request {
                    info!(
                        request_id,
                        status = status.as_u16(),
                        prompt_too_long_retry_triggered = true,
                        prompt_too_long_retries = prompt_too_long_attempts,
                        prompt_too_long_retry_exhausted = true,
                        "Compact request prompt-too-long retry result"
                    );
                }
                return Err(map_chatgpt_error_status_body(status, error_body));
            }

            let Some(stats) =
                shrink_prompt_too_long_body(body, prompt_too_long_token_gap(&error_body))
            else {
                notify_prompt_too_long_observer(
                    observer,
                    ProviderRequestObserverEventKind::PromptTooLongRetryUnshrinkable,
                    prompt_too_long_attempts as u64,
                    None,
                );
                if compact_request {
                    info!(
                        request_id,
                        status = status.as_u16(),
                        prompt_too_long_retry_triggered = true,
                        prompt_too_long_retries = prompt_too_long_attempts,
                        prompt_too_long_retry_shrinkable = false,
                        "Compact request prompt-too-long retry result"
                    );
                }
                return Err(map_chatgpt_error_status_body(status, error_body));
            };
            prompt_too_long_attempts += 1;
            notify_prompt_too_long_observer(
                observer,
                ProviderRequestObserverEventKind::PromptTooLongRetry,
                prompt_too_long_attempts as u64,
                Some(&stats),
            );
            warn!(
                request_id,
                attempt = prompt_too_long_attempts,
                compact_request,
                dropped_groups = stats.dropped_groups,
                dropped_items = stats.dropped_items,
                inserted_marker = stats.inserted_marker,
                truncated_text_items = stats.truncated_text_items,
                original_body_bytes = stats.original_body_bytes,
                shrunk_body_bytes = stats.shrunk_body_bytes,
                "Retrying ChatGPT request after prompt-too-long response"
            );
        }
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

        let response =
            send_upstream_request_with_policy(request_builder, chatgpt_upstream_request_policy())
                .await?;
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
        let token = self.auth.get_token().await?;
        let request = apply_openai_intent(request);
        let marker_mode = marker_mode_from_request(&request);
        let mut body = responses::build_body_with_context(
            &request,
            DEFAULT_CHATGPT_INSTRUCTIONS,
            responses::CodexRequestContext {
                installation_id: Some(&self.installation_id),
                prompt_cache_key: Some(&self.thread_id),
                window_id: Some(&self.window_id),
                identity_preset: Some(self.request_headers.identity_preset.as_str()),
            },
        );
        let output_token_budget = chatgpt_output_token_budget(&request, &body);
        let request_id = next_chatgpt_request_id();
        log_request_observability("chatgpt", "/responses", &body);
        let compact_request = is_compact_request_body(&body);
        log_compact_request_observability("chatgpt", "/responses", &body, compact_request);

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
                    if matches!(error, ProviderError::Authentication(_)) {
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

        let header_snapshots =
            rate_limit_snapshots_from_headers(&self.id, response.headers(), unix_timestamp_secs());
        log_rate_limit_summary(
            request_id,
            compact_request,
            "response_headers",
            &header_snapshots,
        );
        self.cache_rate_limits(header_snapshots).await;

        let cache = Arc::clone(&self.cached_rate_limits);
        let provider_id = self.id.clone();
        let first_sse_seen = Arc::new(AtomicBool::new(false));
        let first_sse_seen_for_event = Arc::clone(&first_sse_seen);
        let stream_started_at = Instant::now();
        let stream =
            responses::stream_response_with_marker_mode(response, marker_mode, move |event| {
                if !first_sse_seen_for_event.swap(true, Ordering::Relaxed) {
                    let event_type = event
                        .get("type")
                        .and_then(|value| value.as_str())
                        .unwrap_or("unknown");
                    info!(
                        request_id,
                        compact_request,
                        elapsed_ms = elapsed_millis(stream_started_at),
                        event_type,
                        "ChatGPT upstream first SSE event received"
                    );
                }
                if let Some(snapshot) =
                    rate_limit_snapshot_from_sse_event(&provider_id, event, unix_timestamp_secs())
                {
                    log_rate_limit_summary(
                        request_id,
                        compact_request,
                        "stream_event",
                        std::slice::from_ref(&snapshot),
                    );
                    let cache = Arc::clone(&cache);
                    tokio::spawn(async move {
                        cache_rate_limits_into(&cache, vec![snapshot]).await;
                    });
                }
                if let Some(stop_reason) = chatgpt_sse_stop_reason(event) {
                    info!(
                        request_id,
                        compact_request,
                        upstream_stop_reason = stop_reason,
                        upstream_model = chatgpt_sse_model(event).unwrap_or("unknown"),
                        upstream_response_id = chatgpt_sse_response_id(event).unwrap_or(""),
                        "ChatGPT upstream terminal SSE event received"
                    );
                }
            });
        let first_stream_item_seen = Arc::new(AtomicBool::new(false));
        let first_stream_item_seen_for_map = Arc::clone(&first_stream_item_seen);
        Ok(Box::pin(stream.map(move |result| {
            if !first_stream_item_seen_for_map.swap(true, Ordering::Relaxed) {
                match &result {
                    Ok(event) => {
                        info!(
                            request_id,
                            compact_request,
                            elapsed_ms = elapsed_millis(stream_started_at),
                            event = %event.event,
                            "ChatGPT first downstream stream item emitted"
                        );
                    }
                    Err(error) => {
                        warn!(
                            request_id,
                            compact_request,
                            elapsed_ms = elapsed_millis(stream_started_at),
                            error = %error,
                            first_sse_seen = first_sse_seen.load(Ordering::Relaxed),
                            "ChatGPT stream failed before first downstream item"
                        );
                    }
                }
            }
            result
        })))
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
            Err(ProviderError::Authentication(_)) => Ok(self.cached_rate_limits().await),
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

fn chatgpt_request_headers(
    config: &ChatGptProviderConfig,
) -> Result<ChatGptRequestHeaders, ProviderError> {
    let identity_preset = config.identity_preset;
    Ok(ChatGptRequestHeaders {
        identity_preset,
        originator: chatgpt_header_value(
            "originator",
            &config.originator,
            identity_preset.originator(),
        )?,
        user_agent: chatgpt_header_value(
            "User-Agent",
            &config.user_agent,
            identity_preset.user_agent(),
        )?,
    })
}

fn chatgpt_header_value(
    header_name: &str,
    configured_value: &str,
    default_value: &'static str,
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

fn chatgpt_upstream_request_policy() -> UpstreamRequestPolicy {
    UpstreamRequestPolicy {
        max_attempts: CHATGPT_SEND_MAX_ATTEMPTS,
        attempt_timeout: Some(CHATGPT_SEND_ATTEMPT_TIMEOUT),
    }
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

fn chatgpt_runtime_id() -> String {
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

#[derive(Debug, PartialEq, Eq)]
struct PromptTooLongShrinkStats {
    dropped_groups: usize,
    dropped_items: usize,
    inserted_marker: bool,
    truncated_text_items: usize,
    original_body_bytes: usize,
    shrunk_body_bytes: usize,
}

#[derive(Debug)]
struct RetryGroup {
    end: usize,
    estimated_tokens: usize,
}

#[derive(Debug, Clone, Copy)]
enum TextPath {
    InputContentString(usize),
    InputContentPartText(usize, usize),
    InputOutput(usize),
    Instructions,
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

fn map_chatgpt_error_status_body(status: StatusCode, body: String) -> ProviderError {
    let mut message = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| body.clone());
    if let Some(output_limit_message) = chatgpt_output_limit_error_message(status, &message) {
        message = output_limit_message;
    }

    match status {
        StatusCode::BAD_REQUEST => ProviderError::InvalidRequest(message),
        StatusCode::UNAUTHORIZED => ProviderError::Authentication(message),
        StatusCode::NOT_FOUND => ProviderError::ModelNotFound(message),
        StatusCode::PAYLOAD_TOO_LARGE => ProviderError::RequestTooLarge(message),
        StatusCode::TOO_MANY_REQUESTS => ProviderError::RateLimited { retry_after: None },
        status if status.is_server_error() => ProviderError::Overloaded {
            message,
            retry_after: None,
        },
        status => ProviderError::UpstreamError {
            status: status.as_u16(),
            body,
        },
    }
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

fn notify_prompt_too_long_observer(
    observer: Option<&ProviderRequestObserver>,
    event: ProviderRequestObserverEventKind,
    prompt_too_long_retries: u64,
    stats: Option<&PromptTooLongShrinkStats>,
) {
    let Some(observer) = observer else {
        return;
    };
    observer(ProviderRequestObserverEvent {
        event,
        prompt_too_long_retries,
        original_body_bytes: stats.map_or(0, |stats| stats.original_body_bytes as u64),
        shrunk_body_bytes: stats.map_or(0, |stats| stats.shrunk_body_bytes as u64),
        dropped_items: stats.map_or(0, |stats| stats.dropped_items as u64),
    });
}

fn prompt_too_long_token_gap(body: &str) -> Option<usize> {
    let lower = body.to_ascii_lowercase();
    let start = lower.find("prompt is too long")?;
    let numbers = ascii_numbers(&lower[start..]);
    let actual = numbers.first().copied()?;
    let limit = numbers.get(1).copied()?;
    actual.checked_sub(limit).filter(|gap| *gap > 0)
}

fn ascii_numbers(text: &str) -> Vec<usize> {
    let mut numbers = Vec::new();
    let mut current: Option<usize> = None;
    for byte in text.bytes() {
        if byte.is_ascii_digit() {
            let digit = (byte - b'0') as usize;
            current = Some(
                current
                    .unwrap_or(0)
                    .saturating_mul(10)
                    .saturating_add(digit),
            );
        } else if let Some(value) = current.take() {
            numbers.push(value);
        }
    }
    if let Some(value) = current {
        numbers.push(value);
    }
    numbers
}

fn shrink_prompt_too_long_body(
    body: &mut Value,
    token_gap: Option<usize>,
) -> Option<PromptTooLongShrinkStats> {
    let original_body_bytes = json_len(body);

    if let Some(input) = body.get_mut("input").and_then(Value::as_array_mut) {
        let groups = retry_groups_for_responses_input(input);
        if groups.len() >= 2 {
            let drop_groups = prompt_too_long_drop_group_count(&groups, token_gap);
            let drop_end = groups[drop_groups - 1].end;
            input.drain(0..drop_end);
            let inserted_marker = ensure_retry_input_starts_with_user(input);
            return Some(PromptTooLongShrinkStats {
                dropped_groups: drop_groups,
                dropped_items: drop_end,
                inserted_marker,
                truncated_text_items: 0,
                original_body_bytes,
                shrunk_body_bytes: json_len(body),
            });
        }
    }

    truncate_largest_text_for_retry(body, token_gap).map(|()| PromptTooLongShrinkStats {
        dropped_groups: 0,
        dropped_items: 0,
        inserted_marker: false,
        truncated_text_items: 1,
        original_body_bytes,
        shrunk_body_bytes: json_len(body),
    })
}

fn retry_groups_for_responses_input(input: &[Value]) -> Vec<RetryGroup> {
    let mut groups = Vec::new();
    let mut index = 0;

    while index < input.len() {
        let start = index;
        let mut pending_call_ids = BTreeSet::new();

        if is_function_call(&input[index]) {
            collect_call_id(&input[index], &mut pending_call_ids);
            index += 1;

            while index < input.len() {
                if is_function_call(&input[index]) {
                    collect_call_id(&input[index], &mut pending_call_ids);
                    index += 1;
                    continue;
                }

                if is_function_call_output(&input[index]) {
                    if let Some(call_id) = input[index].get("call_id").and_then(Value::as_str) {
                        pending_call_ids.remove(call_id);
                    }
                    index += 1;
                    if pending_call_ids.is_empty() {
                        break;
                    }
                    continue;
                }

                if pending_call_ids.is_empty() {
                    break;
                }
                index += 1;
            }
        } else {
            index += 1;
        }

        groups.push(RetryGroup {
            end: index,
            estimated_tokens: estimate_retry_group_tokens(&input[start..index]),
        });
    }

    groups
}

fn is_function_call(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("function_call")
}

fn is_function_call_output(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("function_call_output")
}

fn collect_call_id(item: &Value, call_ids: &mut BTreeSet<String>) {
    if let Some(call_id) = item.get("call_id").and_then(Value::as_str) {
        call_ids.insert(call_id.to_string());
    }
}

fn estimate_retry_group_tokens(items: &[Value]) -> usize {
    let bytes = items.iter().map(json_len).sum::<usize>();
    (bytes / 4).max(1)
}

fn prompt_too_long_drop_group_count(groups: &[RetryGroup], token_gap: Option<usize>) -> usize {
    let drop_groups = if let Some(token_gap) = token_gap {
        let mut tokens = 0;
        let mut count = 0;
        for group in groups {
            tokens += group.estimated_tokens;
            count += 1;
            if tokens >= token_gap {
                break;
            }
        }
        count
    } else {
        (groups.len() / CHATGPT_PTL_FALLBACK_DROP_DIVISOR).max(1)
    };

    drop_groups.clamp(1, groups.len().saturating_sub(1))
}

fn ensure_retry_input_starts_with_user(input: &mut Vec<Value>) -> bool {
    if input.first().is_none_or(is_user_message_item) {
        return false;
    }

    input.insert(
        0,
        serde_json::json!({
            "role": "user",
            "content": CHATGPT_PTL_RETRY_MARKER,
        }),
    );
    true
}

fn is_user_message_item(item: &Value) -> bool {
    item.get("role").and_then(Value::as_str) == Some("user")
}

fn truncate_largest_text_for_retry(body: &mut Value, token_gap: Option<usize>) -> Option<()> {
    let path = largest_retry_text_path(body)?;
    let original = retry_text_at_path(body, path)?.to_string();
    let truncated = truncated_retry_text(&original, token_gap)?;
    set_retry_text_at_path(body, path, truncated)?;
    Some(())
}

fn largest_retry_text_path(body: &Value) -> Option<TextPath> {
    let mut largest: Option<(TextPath, usize)> = None;

    if let Some(input) = body.get("input").and_then(Value::as_array) {
        for (item_index, item) in input.iter().enumerate() {
            if let Some(text) = item.get("content").and_then(Value::as_str) {
                update_largest(
                    &mut largest,
                    TextPath::InputContentString(item_index),
                    text.len(),
                );
            }
            if let Some(parts) = item.get("content").and_then(Value::as_array) {
                for (part_index, part) in parts.iter().enumerate() {
                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        update_largest(
                            &mut largest,
                            TextPath::InputContentPartText(item_index, part_index),
                            text.len(),
                        );
                    }
                }
            }
            if let Some(output) = item.get("output").and_then(Value::as_str) {
                update_largest(
                    &mut largest,
                    TextPath::InputOutput(item_index),
                    output.len(),
                );
            }
        }
    }

    largest.map(|(path, _)| path).or_else(|| {
        body.get("instructions")
            .and_then(Value::as_str)
            .map(|_| TextPath::Instructions)
    })
}

fn update_largest(largest: &mut Option<(TextPath, usize)>, path: TextPath, len: usize) {
    if largest.as_ref().is_none_or(|(_, current)| len > *current) {
        *largest = Some((path, len));
    }
}

fn retry_text_at_path(body: &Value, path: TextPath) -> Option<&str> {
    match path {
        TextPath::InputContentString(item_index) => {
            body.get("input")?.get(item_index)?.get("content")?.as_str()
        }
        TextPath::InputContentPartText(item_index, part_index) => body
            .get("input")?
            .get(item_index)?
            .get("content")?
            .get(part_index)?
            .get("text")?
            .as_str(),
        TextPath::InputOutput(item_index) => {
            body.get("input")?.get(item_index)?.get("output")?.as_str()
        }
        TextPath::Instructions => body.get("instructions")?.as_str(),
    }
}

fn set_retry_text_at_path(body: &mut Value, path: TextPath, text: String) -> Option<()> {
    let target = match path {
        TextPath::InputContentString(item_index) => body
            .get_mut("input")?
            .get_mut(item_index)?
            .get_mut("content")?,
        TextPath::InputContentPartText(item_index, part_index) => body
            .get_mut("input")?
            .get_mut(item_index)?
            .get_mut("content")?
            .get_mut(part_index)?
            .get_mut("text")?,
        TextPath::InputOutput(item_index) => body
            .get_mut("input")?
            .get_mut(item_index)?
            .get_mut("output")?,
        TextPath::Instructions => body.get_mut("instructions")?,
    };
    *target = Value::String(text);
    Some(())
}

fn truncated_retry_text(text: &str, token_gap: Option<usize>) -> Option<String> {
    let bytes_to_remove = token_gap
        .map(|gap| gap.saturating_mul(4))
        .unwrap_or_else(|| text.len() / 2)
        .max(1);
    let target_bytes = text.len().saturating_sub(bytes_to_remove);
    if target_bytes >= text.len() {
        return None;
    }

    let marker = format!(
        "[chatgpt prompt truncated for retry: original_bytes={}]",
        text.len()
    );
    let visible_budget = target_bytes.saturating_sub(marker.len());
    let head_budget = visible_budget / 2;
    let tail_budget = visible_budget.saturating_sub(head_budget);
    let head_end = floor_char_boundary(text, head_budget);
    let tail_start = ceil_char_boundary(text, text.len().saturating_sub(tail_budget));

    Some(format!(
        "{}{}{}",
        &text[..head_end],
        marker,
        &text[tail_start..]
    ))
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn json_len(value: &Value) -> usize {
    serde_json::to_vec(value).map_or(0, |bytes| bytes.len())
}

fn chatgpt_models() -> Vec<ModelInfo> {
    [
        "gpt-5.5",
        "gpt-5.4",
        "gpt-5.4-mini",
        "gpt-5.3-codex",
        "gpt-5.3-codex-spark",
        "gpt-5.2",
    ]
    .into_iter()
    .map(|model_id| {
        let mut info = openai_model_info(model_id);
        info.supports_vision = Some(true);
        info
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

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
        assert_eq!(
            chatgpt_sse_stop_reason(&json!({
                "type": "response.failed",
                "response": {"status": "failed"}
            })),
            Some("error")
        );
    }

    #[test]
    fn chatgpt_models_include_reasoning_capabilities() {
        let models = chatgpt_models();
        let gpt55 = models
            .iter()
            .find(|model| model.model_id == "gpt-5.5")
            .expect("gpt-5.5 model");

        assert_eq!(gpt55.max_output_tokens, Some(128_000));
        assert_eq!(gpt55.context_window, Some(400_000));
        assert!(
            gpt55
                .supported_endpoints
                .contains(&"/responses".to_string())
        );
        assert_eq!(gpt55.supports_vision, Some(true));
        assert_eq!(
            gpt55.reasoning_effort_levels,
            vec!["low", "medium", "high", "xhigh"]
        );
    }

    #[test]
    fn chatgpt_request_policy_caps_first_response_wait() {
        let policy = chatgpt_upstream_request_policy();

        assert_eq!(policy.max_attempts, 2);
        assert_eq!(policy.attempt_timeout, Some(Duration::from_secs(60)));
    }

    #[test]
    fn chatgpt_request_headers_use_configured_values_and_default_empty_values() {
        let config = claude_proxy_config::settings::ChatGptProviderConfig {
            originator: "codex_cli".to_string(),
            user_agent: "CodexCLI/1.2.3".to_string(),
            ..Default::default()
        };

        let headers = chatgpt_request_headers(&config).unwrap();
        assert_eq!(headers.identity_preset, ChatGptIdentityPreset::Opencode);
        assert_eq!(headers.originator.to_str().unwrap(), "codex_cli");
        assert_eq!(headers.user_agent.to_str().unwrap(), "CodexCLI/1.2.3");

        let config = claude_proxy_config::settings::ChatGptProviderConfig {
            originator: "  ".to_string(),
            user_agent: "\t".to_string(),
            ..Default::default()
        };

        let headers = chatgpt_request_headers(&config).unwrap();
        assert_eq!(headers.originator.to_str().unwrap(), "opencode");
        assert_eq!(
            headers.user_agent.to_str().unwrap(),
            "opencode/claude-proxy"
        );
    }

    #[test]
    fn chatgpt_request_headers_use_identity_presets() {
        let config = claude_proxy_config::settings::ChatGptProviderConfig {
            identity_preset: ChatGptIdentityPreset::Codex,
            ..Default::default()
        };
        let headers = chatgpt_request_headers(&config).unwrap();
        assert_eq!(headers.identity_preset, ChatGptIdentityPreset::Codex);
        assert_eq!(headers.originator.to_str().unwrap(), "codex_cli_rs");
        assert_eq!(
            headers.user_agent.to_str().unwrap(),
            "codex_cli_rs/0.0.0 (claude-proxy)"
        );

        let config = claude_proxy_config::settings::ChatGptProviderConfig {
            identity_preset: ChatGptIdentityPreset::AnthropicBridge,
            ..Default::default()
        };
        let headers = chatgpt_request_headers(&config).unwrap();
        assert_eq!(headers.originator.to_str().unwrap(), "anthropic-bridge");
        assert_eq!(
            headers.user_agent.to_str().unwrap(),
            "anthropic-bridge/claude-proxy"
        );
    }

    #[test]
    fn chatgpt_request_headers_match_native_codex_fixture() {
        let expected: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/chatgpt_codex/native_request_headers.json"
        ))
        .expect("valid native headers fixture");
        let config = claude_proxy_config::settings::ChatGptProviderConfig {
            identity_preset: ChatGptIdentityPreset::Codex,
            ..Default::default()
        };

        let headers = chatgpt_request_headers(&config).unwrap();
        let actual = json!({
            "identity_preset": headers.identity_preset.as_str(),
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
        assert_eq!(body["max_output_tokens"], 4096);
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
                    "x-codex-window-id": "window-123"
                }
            })),
            extra: Default::default(),
        };

        let body = build_chatgpt_responses_body_with_context(&req, Some("install-123"));

        assert_eq!(body["prompt_cache_key"], "thread-123");
        assert_eq!(
            body["client_metadata"]["x-codex-installation-id"],
            "install-123"
        );
        assert_eq!(body["client_metadata"]["x-codex-window-id"], "window-123");
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
                prompt_cache_key: Some("thread-123"),
                window_id: Some("window-123"),
                identity_preset: Some("codex"),
            },
        );

        assert_eq!(body["prompt_cache_key"], "thread-123");
        assert_eq!(
            body["client_metadata"]["x-codex-installation-id"],
            "install-123"
        );
        assert_eq!(body["client_metadata"]["x-codex-window-id"], "window-123");
        assert_eq!(
            body["client_metadata"]["x-claude-proxy-identity-preset"],
            "codex"
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
                prompt_cache_key: Some("thread-fallback"),
                window_id: Some("window-runtime"),
                identity_preset: Some("codex"),
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
        assert_eq!(body["max_output_tokens"], 4096);
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][0]["call_id"], "call_1");
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][1]["call_id"], "call_1");
        assert_eq!(body["input"][1]["output"], "done");
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
        assert_eq!(body["max_output_tokens"], 4096);
    }

    #[test]
    fn chatgpt_responses_body_clamps_max_output_tokens_to_model_limit() {
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

        assert_eq!(body["max_output_tokens"], 16_384);
        assert_eq!(
            chatgpt_output_token_budget(&req, &body),
            ChatGptOutputTokenBudget {
                requested: Some(128_000),
                effective: Some(16_384)
            }
        );
    }

    #[tokio::test]
    async fn chatgpt_retries_prompt_too_long_with_shrunk_body() {
        let (endpoint, requests) = prompt_too_long_retry_server().await;
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

        let response = provider
            .send_responses_request_with_prompt_too_long_retry(
                &mut body,
                &token,
                false,
                1,
                ChatGptOutputTokenBudget::default(),
                None,
            )
            .await
            .expect("retry should succeed");

        assert!(response.status().is_success());
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0]["input"].as_array().unwrap().len(), 2);
        assert_eq!(requests[1]["input"].as_array().unwrap().len(), 1);
        assert_eq!(requests[1]["input"][0]["content"], "current");
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
        assert!(headers.contains("x-client-request-id: thread-test"));
        assert!(headers.contains("session-id: session-test"));
        assert!(headers.contains("thread-id: thread-test"));
        assert!(headers.contains("x-codex-window-id: window-test"));
        let request_body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(request_body["model"], "gpt-5.3-codex");
    }

    #[tokio::test]
    async fn chatgpt_prompt_too_long_retry_notifies_observer() {
        let (endpoint, _requests) = prompt_too_long_retry_server().await;
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
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observer: ProviderRequestObserver = {
            let observed = observed.clone();
            Arc::new(move |event| observed.lock().unwrap().push(event))
        };

        provider
            .send_responses_request_with_prompt_too_long_retry(
                &mut body,
                &token,
                false,
                1,
                ChatGptOutputTokenBudget::default(),
                Some(&observer),
            )
            .await
            .expect("retry should succeed");

        let observed = observed.lock().unwrap();
        assert_eq!(observed.len(), 1);
        assert_eq!(
            observed[0].event,
            ProviderRequestObserverEventKind::PromptTooLongRetry
        );
        assert_eq!(observed[0].prompt_too_long_retries, 1);
        assert!(observed[0].original_body_bytes > observed[0].shrunk_body_bytes);
        assert_eq!(observed[0].dropped_items, 1);
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
    fn output_limit_errors_map_to_clear_anthropic_invalid_request() {
        let error = map_chatgpt_error_status_body(
            StatusCode::BAD_REQUEST,
            r#"{"error":{"message":"max_output_tokens is too high. Maximum supported value is 16384"}}"#.to_string(),
        );

        match error {
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

        match error {
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

        match error {
            ProviderError::InvalidRequest(message) => {
                assert_eq!(message, "bad tool schema");
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn prompt_too_long_token_gap_parses_actual_and_limit() {
        assert_eq!(
            prompt_too_long_token_gap("Prompt is too long: 137500 tokens > 135000 maximum"),
            Some(2500)
        );
        assert_eq!(
            prompt_too_long_token_gap(
                r#"{"error":{"message":"prompt is too long: 400001 tokens > 400000 maximum"}}"#
            ),
            Some(1)
        );
        assert_eq!(prompt_too_long_token_gap("Prompt is too long"), None);
    }

    #[test]
    fn prompt_too_long_retry_drops_oldest_responses_input_group() {
        let mut body = json!({
            "model": "gpt-5.3-codex",
            "input": [
                {"role": "user", "content": "old"},
                {"role": "user", "content": "current"}
            ],
            "stream": true
        });

        let stats =
            shrink_prompt_too_long_body(&mut body, None).expect("body should shrink by group");
        let input = body["input"].as_array().expect("input");

        assert_eq!(stats.dropped_items, 1);
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"], "current");
    }

    #[test]
    fn prompt_too_long_retry_keeps_function_call_and_output_together() {
        let mut body = json!({
            "model": "gpt-5.3-codex",
            "input": [
                {"type": "function_call", "call_id": "call_old", "name": "Read", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_old", "output": "old output"},
                {"role": "user", "content": "current"}
            ],
            "stream": true
        });

        let stats =
            shrink_prompt_too_long_body(&mut body, None).expect("body should shrink by group");
        let input = body["input"].as_array().expect("input");

        assert_eq!(stats.dropped_items, 2);
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"], "current");
    }

    #[test]
    fn prompt_too_long_retry_inserts_marker_when_retry_would_start_with_assistant() {
        let mut body = json!({
            "model": "gpt-5.3-codex",
            "input": [
                {"role": "user", "content": "old"},
                {"role": "assistant", "content": "assistant starts remaining context"},
                {"role": "user", "content": "current"}
            ],
            "stream": true
        });

        let stats =
            shrink_prompt_too_long_body(&mut body, None).expect("body should shrink by group");
        let input = body["input"].as_array().expect("input");

        assert!(stats.inserted_marker);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"], CHATGPT_PTL_RETRY_MARKER);
        assert_eq!(input[1]["role"], "assistant");
    }

    #[test]
    fn prompt_too_long_retry_truncates_single_oversized_text_when_no_group_can_drop() {
        let huge = "x".repeat(200_000);
        let mut body = json!({
            "model": "gpt-5.3-codex",
            "input": [
                {"role": "user", "content": huge}
            ],
            "stream": true
        });

        let stats = shrink_prompt_too_long_body(&mut body, Some(10_000))
            .expect("single large text should truncate");
        let content = body["input"][0]["content"].as_str().expect("content");

        assert_eq!(stats.dropped_items, 0);
        assert_eq!(stats.truncated_text_items, 1);
        assert!(content.len() < 200_000);
        assert!(content.contains("[chatgpt prompt truncated for retry:"));
    }

    #[test]
    fn prompt_too_long_retry_prefers_truncating_input_over_instructions() {
        let instructions = "i".repeat(250_000);
        let content = "x".repeat(200_000);
        let mut body = json!({
            "model": "gpt-5.3-codex",
            "instructions": instructions,
            "input": [
                {"role": "user", "content": content}
            ],
            "stream": true
        });

        shrink_prompt_too_long_body(&mut body, Some(10_000)).expect("body should shrink");

        assert_eq!(body["instructions"].as_str().unwrap().len(), 250_000);
        assert!(body["input"][0]["content"].as_str().unwrap().len() < 200_000);
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
            session_id: "session-test".to_string(),
            thread_id: "thread-test".to_string(),
            window_id: "window-test".to_string(),
            request_headers: ChatGptRequestHeaders {
                identity_preset: ChatGptIdentityPreset::Opencode,
                originator: HeaderValue::from_static("opencode"),
                user_agent: HeaderValue::from_static("opencode/claude-proxy-test"),
            },
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

    async fn prompt_too_long_retry_server() -> (String, Arc<Mutex<Vec<Value>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured_requests = Arc::clone(&requests);

        tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let body = read_http_request_body(&mut socket).await;
                captured_requests
                    .lock()
                    .await
                    .push(serde_json::from_slice(&body).unwrap());

                let (status, response_body) = if attempt == 0 {
                    (
                        "400 Bad Request",
                        r#"{"error":{"message":"Prompt is too long: 20 tokens > 10 maximum"}}"#,
                    )
                } else {
                    ("200 OK", r#"{"ok":true}"#)
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response_body}",
                    response_body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
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
