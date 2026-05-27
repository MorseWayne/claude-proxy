use std::convert::Infallible;
use std::hash::Hasher;
use std::io;
use std::sync::{Arc, Mutex as StdMutex};

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use claude_proxy_config::settings::{
    ModelReasoningEffort, ProviderType, REASONING_MARKER_MODE_EXTRA_KEY, ReasoningMarkerMode,
};
use claude_proxy_core::*;
use claude_proxy_providers::openai_request_log_info;
use claude_proxy_providers::provider::{
    Provider, ProviderError, ProviderRequestObserver, ProviderRequestObserverEvent,
    ProviderRequestObserverEventKind, ProviderUsageMetadata, UpstreamErrorMetadata,
};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, broadcast};
use tokio::time::{Instant as TokioInstant, MissedTickBehavior};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use crate::app::{
    AppState, InflightEvent, RequestObservabilityEvent, RequestPayloadStats, TokenUsage,
};
use crate::persistence::CompletedUsageRecord;

const SSE_HEARTBEAT_FRAME: &[u8] = b": ping\n\n";

fn check_auth(headers: &HeaderMap, auth_token: &str) -> bool {
    if auth_token.is_empty() {
        return true;
    }
    if let Some(key) = headers.get("x-api-key").and_then(|v| v.to_str().ok())
        && key == auth_token
    {
        return true;
    }
    if let Some(auth) = headers.get("authorization").and_then(|v| v.to_str().ok())
        && let Some(token) = auth.strip_prefix("Bearer ")
        && token == auth_token
    {
        return true;
    }
    false
}

fn attach_client_session_metadata(
    mut request: MessagesRequest,
    headers: &HeaderMap,
) -> MessagesRequest {
    if request_has_stable_client_session(&request) {
        return request;
    }
    let Some(session_id) = client_session_id_from_headers(headers) else {
        return request;
    };
    request
        .extra
        .insert("client_session_id".to_string(), json!(session_id));
    request
}

fn request_has_stable_client_session(request: &MessagesRequest) -> bool {
    const KEYS: &[&str] = &[
        "conversation_id",
        "thread_id",
        "session_id",
        "client_conversation_id",
        "client_thread_id",
        "client_session_id",
        "x-client-conversation-id",
        "x-client-thread-id",
        "x-client-session-id",
    ];
    KEYS.iter().any(|key| {
        request.extra.get(*key).and_then(Value::as_str).is_some()
            || request
                .metadata
                .as_ref()
                .and_then(|metadata| metadata.get(*key))
                .and_then(Value::as_str)
                .is_some()
    })
}

fn client_session_id_from_headers(headers: &HeaderMap) -> Option<String> {
    const HEADER_NAMES: &[&str] = &[
        "x-client-conversation-id",
        "x-client-session-id",
        "x-client-thread-id",
        "x-claude-conversation-id",
        "x-claude-session-id",
        "x-claude-thread-id",
        "x-session-id",
        "session-id",
        "session_id",
    ];
    HEADER_NAMES.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(ToOwned::to_owned)
    })
}

/// Check authorization header against admin token.
fn check_admin_auth(headers: &HeaderMap, admin_token: &str) -> bool {
    let auth = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let provided = auth.strip_prefix("Bearer ").unwrap_or(auth);
    provided == admin_token
}

/// GET /health
pub async fn health() -> &'static str {
    "ok"
}

/// POST /v1/messages — Anthropic Messages API proxy
pub async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<MessagesRequest>,
) -> Response {
    let start = std::time::Instant::now();
    let (observability_enabled, observability_idle_gap_ms, stream_config) = {
        let settings = state.settings.read().await;
        (
            settings.observability.enabled,
            settings.observability.idle_gap_ms,
            StreamResponseConfig::from_settings(&settings),
        )
    };
    state.metrics.record_request();

    // Auth check
    {
        let settings = state.settings.read().await;
        if !check_auth(&headers, &settings.server.auth_token) {
            record_request_error(&state, start);
            return error_response(
                StatusCode::UNAUTHORIZED,
                &ErrorResponse::authentication("invalid API key"),
            );
        }
    }

    let request = attach_client_session_metadata(request, &headers);

    // Concurrency limiting
    let request_permit = match acquire_request_permit(&state, start).await {
        Ok(permit) => permit,
        Err(response) => return response,
    };

    let resolved = resolve_upstream_request(&state, &request).await;

    log_resolved_request(&request, &resolved);

    // --- Concurrent request deduplication ---
    // Compute a hash of the full request to identify identical inflight requests.
    let request_hash = compute_request_hash(&resolved.request);

    let broadcast_tx = match register_or_subscribe_inflight_request(&state, request_hash).await {
        InflightRegistration::Leader(sender) => sender,
        InflightRegistration::Follower(receiver) => {
            let response = if request.stream {
                join_inflight_stream(
                    receiver,
                    StreamPermits {
                        _request: request_permit,
                        _provider: None,
                    },
                    stream_config,
                )
            } else {
                join_inflight_non_stream(receiver, request_permit).await
            };
            state
                .metrics
                .record_latency(start.elapsed().as_millis() as u64);
            return response;
        }
    };

    // Get or create provider — lock is released after obtaining the Arc
    let provider = match get_provider(&state, &resolved.provider_id).await {
        Ok(provider) => provider,
        Err(e) => {
            error!("Provider error: {e}");
            let _ = broadcast_tx.send(InflightEvent::Error(e.to_string()));
            let _ = broadcast_tx.send(InflightEvent::Done);
            state.inflight.lock().await.remove(&request_hash);
            record_request_error(&state, start);
            return error_response(
                StatusCode::NOT_FOUND,
                &ErrorResponse::not_found(&format!("provider not available: {e}")),
            );
        }
    };

    if let Err(error) = validate_resolved_request_capabilities(&state, &resolved).await {
        let message = error.error.message.clone();
        let _ = broadcast_tx.send(InflightEvent::Error(message));
        let _ = broadcast_tx.send(InflightEvent::Done);
        state.inflight.lock().await.remove(&request_hash);
        record_request_error(&state, start);
        return error_response(StatusCode::BAD_REQUEST, &error);
    }

    let provider_permit = match acquire_provider_permit(&state, &resolved.provider_id, start).await
    {
        Ok(permit) => permit,
        Err(response) => {
            let msg = "provider concurrency limit reached";
            let _ = broadcast_tx.send(InflightEvent::Error(msg.to_string()));
            let _ = broadcast_tx.send(InflightEvent::Done);
            state.inflight.lock().await.remove(&request_hash);
            return response;
        }
    };

    let provider_setup_ms = start.elapsed().as_millis() as u64;
    let metrics_context = RequestMetricsContext {
        provider_id: resolved.provider_id.clone(),
        model: resolved.request.model.clone(),
        initiator: resolved.initiator,
    };
    let payload_stats = request_payload_stats(&resolved.request);
    let observer_state = Arc::new(StdMutex::new(RequestObserverState::default()));
    let provider_observer = Some(provider_request_observer(observer_state.clone()));
    let upstream_connect_start = std::time::Instant::now();

    // Call provider (registry lock is no longer held)
    match provider
        .chat_with_observer(resolved.request, provider_observer)
        .await
    {
        Ok(stream) => {
            let upstream_connect_ms = upstream_connect_start.elapsed().as_millis() as u64;
            let observability = observability_enabled.then(|| ObservabilityContext {
                request_id: format!("{request_hash:016x}"),
                provider_id: metrics_context.provider_id.clone(),
                initiator: metrics_context.initiator,
                model: metrics_context.model.clone(),
                stream: request.stream,
                start,
                provider_setup_ms,
                upstream_connect_ms,
                idle_gap_ms: observability_idle_gap_ms,
                payload_stats,
                observer_state: observer_state.clone(),
            });
            if request.stream {
                stream_leader_response(
                    &state,
                    &metrics_context,
                    stream,
                    LeaderResponseContext {
                        request_hash,
                        broadcast_tx,
                        permits: StreamPermits {
                            _request: request_permit,
                            _provider: Some(provider_permit),
                        },
                        start,
                        observer_state: observer_state.clone(),
                        observability,
                        stream_config,
                    },
                )
                .await
            } else {
                collect_leader_response(
                    &state,
                    &metrics_context,
                    stream,
                    LeaderResponseContext {
                        request_hash,
                        broadcast_tx,
                        permits: StreamPermits {
                            _request: request_permit,
                            _provider: Some(provider_permit),
                        },
                        start,
                        observer_state: observer_state.clone(),
                        observability,
                        stream_config,
                    },
                )
                .await
            }
        }
        Err(e) => {
            let upstream_connect_ms = upstream_connect_start.elapsed().as_millis() as u64;
            let observability = observability_enabled.then(|| ObservabilityContext {
                request_id: format!("{request_hash:016x}"),
                provider_id: metrics_context.provider_id.clone(),
                initiator: metrics_context.initiator,
                model: metrics_context.model.clone(),
                stream: request.stream,
                start,
                provider_setup_ms,
                upstream_connect_ms,
                idle_gap_ms: observability_idle_gap_ms,
                payload_stats,
                observer_state: observer_state.clone(),
            });
            handle_provider_error(
                &state,
                &metrics_context,
                request_hash,
                broadcast_tx,
                e,
                start,
                observability,
            )
            .await
        }
    }
}

struct ResolvedUpstreamRequest {
    provider_id: String,
    provider_type: ProviderType,
    upstream_model: String,
    initiator: &'static str,
    request: MessagesRequest,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct RequestFeatures {
    streaming: bool,
    system_prompt: bool,
    tools: bool,
    tool_choice: bool,
    thinking: bool,
    vision: bool,
    sampling: bool,
    stop_sequences: bool,
}

impl RequestFeatures {
    fn from_request(request: &MessagesRequest) -> Self {
        Self {
            streaming: request.stream,
            system_prompt: request.system.is_some(),
            tools: request
                .tools
                .as_ref()
                .is_some_and(|tools| !tools.is_empty()),
            tool_choice: request.tool_choice.is_some(),
            thinking: request.thinking.is_some(),
            vision: request_uses_vision(request),
            sampling: request.temperature.is_some()
                || request.top_p.is_some()
                || request.top_k.is_some(),
            stop_sequences: request
                .stop_sequences
                .as_ref()
                .is_some_and(|sequences| !sequences.is_empty()),
        }
    }
}

fn request_uses_vision(request: &MessagesRequest) -> bool {
    request
        .messages
        .iter()
        .any(|message| match &message.content {
            MessageContent::Blocks(blocks) => blocks.iter().any(content_is_image),
            MessageContent::Text(_) => false,
        })
}

fn content_is_image(content: &Content) -> bool {
    match content {
        Content::Unknown(value) => value.get("type").and_then(Value::as_str) == Some("image"),
        _ => false,
    }
}

fn validate_request_capabilities(
    provider_id: &str,
    model: &str,
    capabilities: &ModelCapabilities,
    features: &RequestFeatures,
) -> Result<(), ErrorResponse> {
    reject_unsupported(
        provider_id,
        model,
        "streaming",
        features.streaming,
        capabilities.features.streaming,
    )?;
    reject_unsupported(
        provider_id,
        model,
        "system prompt",
        features.system_prompt,
        capabilities.features.system_prompt,
    )?;
    reject_unsupported(
        provider_id,
        model,
        "tools",
        features.tools,
        capabilities.features.tools,
    )?;
    reject_unsupported(
        provider_id,
        model,
        "tool_choice",
        features.tool_choice,
        capabilities.features.tool_choice,
    )?;
    reject_unsupported(
        provider_id,
        model,
        "thinking",
        features.thinking,
        capabilities.features.thinking,
    )?;
    reject_unsupported(
        provider_id,
        model,
        "vision input",
        features.vision,
        capabilities.modalities.input.image,
    )?;
    reject_unsupported(
        provider_id,
        model,
        "sampling parameters",
        features.sampling,
        capabilities.features.sampling,
    )?;
    reject_unsupported(
        provider_id,
        model,
        "stop_sequences",
        features.stop_sequences,
        capabilities.features.stop_sequences,
    )
}

fn reject_unsupported(
    provider_id: &str,
    model: &str,
    feature: &str,
    requested: bool,
    state: CapabilityState,
) -> Result<(), ErrorResponse> {
    if requested && state == CapabilityState::Unsupported {
        return Err(ErrorResponse::invalid_request(&format!(
            "model {model} via provider {provider_id} does not support {feature}"
        )));
    }
    Ok(())
}

async fn validate_resolved_request_capabilities(
    state: &AppState,
    resolved: &ResolvedUpstreamRequest,
) -> Result<(), ErrorResponse> {
    let features = RequestFeatures::from_request(&resolved.request);
    let registry = state.provider_registry.read().await;
    let Some(model) = registry
        .cached_models(&resolved.provider_id)
        .and_then(|models| {
            models
                .iter()
                .find(|model| model.model_id == resolved.upstream_model)
        })
    else {
        return Ok(());
    };

    validate_request_capabilities(
        &resolved.provider_id,
        &resolved.upstream_model,
        &model.capabilities,
        &features,
    )
}

#[derive(Clone)]
struct RequestMetricsContext {
    provider_id: String,
    model: String,
    initiator: &'static str,
}

#[derive(Clone)]
struct ObservabilityContext {
    request_id: String,
    provider_id: String,
    initiator: &'static str,
    model: String,
    stream: bool,
    start: std::time::Instant,
    provider_setup_ms: u64,
    upstream_connect_ms: u64,
    idle_gap_ms: u64,
    payload_stats: RequestPayloadStats,
    observer_state: Arc<StdMutex<RequestObserverState>>,
}

#[derive(Debug, Clone, Default)]
struct RequestObserverState {
    prompt_too_long_retries: u64,
    prompt_too_long_original_body_bytes: u64,
    prompt_too_long_shrunk_body_bytes: u64,
    prompt_too_long_dropped_items: u64,
    stream_usage: TokenUsage,
}

impl RequestObserverState {
    fn record(&mut self, event: &ProviderRequestObserverEvent) {
        match event.event {
            ProviderRequestObserverEventKind::PromptTooLongRetry
            | ProviderRequestObserverEventKind::PromptTooLongRetryExhausted
            | ProviderRequestObserverEventKind::PromptTooLongRetryUnshrinkable => {
                self.prompt_too_long_retries = self
                    .prompt_too_long_retries
                    .max(event.prompt_too_long_retries);
                self.prompt_too_long_original_body_bytes = self
                    .prompt_too_long_original_body_bytes
                    .max(event.original_body_bytes);
                self.prompt_too_long_shrunk_body_bytes = self
                    .prompt_too_long_shrunk_body_bytes
                    .max(event.shrunk_body_bytes);
                self.prompt_too_long_dropped_items =
                    self.prompt_too_long_dropped_items.max(event.dropped_items);
            }
            ProviderRequestObserverEventKind::StreamMetadata => {
                if let Some(usage) = event
                    .stream_metadata
                    .as_ref()
                    .and_then(|metadata| metadata.usage.as_ref())
                {
                    merge_provider_usage_metadata(usage, &mut self.stream_usage);
                }
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
struct ObservabilityTiming {
    stream_duration_ms: u64,
    first_event_ms: Option<u64>,
    last_event_gap_ms: u64,
    max_event_gap_ms: u64,
    idle_gap_count: u64,
    event_count: u64,
    last_event_at: Option<std::time::Instant>,
}

impl ObservabilityTiming {
    fn record_event(
        &mut self,
        request_start: std::time::Instant,
        event_at: std::time::Instant,
        idle_gap_ms: u64,
    ) {
        self.event_count += 1;
        self.first_event_ms
            .get_or_insert_with(|| event_at.duration_since(request_start).as_millis() as u64);
        if let Some(last_event_at) = self.last_event_at {
            let gap_ms = event_at.duration_since(last_event_at).as_millis() as u64;
            self.last_event_gap_ms = gap_ms;
            self.max_event_gap_ms = self.max_event_gap_ms.max(gap_ms);
            if idle_gap_ms > 0 && gap_ms >= idle_gap_ms {
                self.idle_gap_count += 1;
            }
        }
        self.last_event_at = Some(event_at);
    }
}

fn provider_request_observer(
    state: Arc<StdMutex<RequestObserverState>>,
) -> ProviderRequestObserver {
    Arc::new(move |event| {
        if let Ok(mut state) = state.lock() {
            state.record(&event);
        }
    })
}

fn build_observability_event(
    context: ObservabilityContext,
    timing: ObservabilityTiming,
    is_error: bool,
    terminal_reason: &str,
) -> RequestObservabilityEvent {
    let observer_state = context
        .observer_state
        .lock()
        .map(|state| state.clone())
        .unwrap_or_default();
    RequestObservabilityEvent {
        request_id: context.request_id,
        provider: context.provider_id,
        initiator: context.initiator.to_string(),
        model: context.model,
        stream: context.stream,
        is_error,
        terminal_reason: terminal_reason.to_string(),
        total_latency_ms: context.start.elapsed().as_millis() as u64,
        provider_setup_ms: context.provider_setup_ms,
        upstream_connect_ms: context.upstream_connect_ms,
        stream_duration_ms: timing.stream_duration_ms,
        first_event_ms: timing.first_event_ms,
        last_event_gap_ms: timing.last_event_gap_ms,
        max_event_gap_ms: timing.max_event_gap_ms,
        idle_gap_count: timing.idle_gap_count,
        event_count: timing.event_count,
        prompt_too_long_retries: observer_state.prompt_too_long_retries,
        prompt_too_long_original_body_bytes: observer_state.prompt_too_long_original_body_bytes,
        prompt_too_long_shrunk_body_bytes: observer_state.prompt_too_long_shrunk_body_bytes,
        prompt_too_long_dropped_items: observer_state.prompt_too_long_dropped_items,
        request_messages: context.payload_stats.messages,
        request_content_blocks: context.payload_stats.content_blocks,
        request_tool_results: context.payload_stats.tool_results,
        request_text_bytes: context.payload_stats.text_bytes,
    }
}

fn request_payload_stats(request: &MessagesRequest) -> RequestPayloadStats {
    let mut stats = RequestPayloadStats {
        messages: request.messages.len() as u64,
        ..RequestPayloadStats::default()
    };
    if let Some(system) = &request.system {
        match system {
            SystemPrompt::Text(text) => {
                stats.content_blocks += 1;
                stats.text_bytes += text.len() as u64;
            }
            SystemPrompt::Blocks(blocks) => {
                for block in blocks {
                    add_content_stats(block, &mut stats);
                }
            }
        }
    }
    for message in &request.messages {
        match &message.content {
            MessageContent::Text(text) => {
                stats.content_blocks += 1;
                stats.text_bytes += text.len() as u64;
            }
            MessageContent::Blocks(blocks) => {
                for block in blocks {
                    add_content_stats(block, &mut stats);
                }
            }
        }
    }
    stats
}

fn add_content_stats(content: &Content, stats: &mut RequestPayloadStats) {
    stats.content_blocks += 1;
    match content {
        Content::Text { text } => {
            stats.text_bytes += text.len() as u64;
        }
        Content::Thinking { thinking, .. } => {
            stats.text_bytes += thinking.len() as u64;
        }
        Content::ToolResult { content, .. } => {
            stats.tool_results += 1;
            if let Some(content) = content {
                stats.text_bytes += json_text_bytes(content);
            }
        }
        Content::Unknown(value) => {
            stats.text_bytes += json_text_bytes(value);
        }
        Content::ToolUse { .. } | Content::ServerToolUse { .. } => {}
    }
}

fn json_text_bytes(value: &Value) -> u64 {
    match value {
        Value::String(text) => text.len() as u64,
        Value::Array(values) => values.iter().map(json_text_bytes).sum(),
        Value::Object(object) => object
            .iter()
            .filter(|(key, _)| key.as_str() != "type")
            .map(|(_, value)| json_text_bytes(value))
            .sum(),
        _ => 0,
    }
}

fn log_resolved_request(original_request: &MessagesRequest, resolved: &ResolvedUpstreamRequest) {
    if openai_compatible_reasoning_log(&resolved.provider_type) {
        let info = openai_request_log_info(&resolved.request);
        if let Some(reasoning_effort) = info.reasoning_effort {
            if info.thinking_type.is_some() || info.thinking_budget_tokens.is_some() {
                info!(
                    "Request: initiator={} model={} → {}/{} reasoning_effort={} reasoning_source={} thinking_type={} thinking_budget_tokens={}",
                    resolved.initiator,
                    original_request.model,
                    resolved.provider_id,
                    info.model,
                    reasoning_effort,
                    info.reasoning_source,
                    info.thinking_type.as_deref().unwrap_or("unset"),
                    info.thinking_budget_tokens
                        .map(|tokens| tokens.to_string())
                        .unwrap_or_else(|| "unset".to_string())
                );
                return;
            }
            info!(
                "Request: initiator={} model={} → {}/{} reasoning_effort={} reasoning_source={}",
                resolved.initiator,
                original_request.model,
                resolved.provider_id,
                info.model,
                reasoning_effort,
                info.reasoning_source
            );
            return;
        }
    }

    info!(
        "Request: initiator={} model={} → {}/{}",
        resolved.initiator, original_request.model, resolved.provider_id, resolved.upstream_model
    );
}

fn openai_compatible_reasoning_log(provider_type: &ProviderType) -> bool {
    matches!(
        provider_type,
        ProviderType::OpenAI
            | ProviderType::ChatGPT
            | ProviderType::OpenRouter
            | ProviderType::Google
            | ProviderType::Custom(_)
    )
}

async fn resolve_upstream_request(
    state: &AppState,
    request: &MessagesRequest,
) -> ResolvedUpstreamRequest {
    let settings = state.settings.read().await;
    let intent = request_intent(request);
    let resolved_model = settings.resolve_model_with_intent(&request.model, intent);
    let provider_id = resolved_model.provider_id.clone();
    let upstream_model = resolved_model.upstream_model.clone();
    let provider_config = settings.providers.get(&provider_id);
    let provider_type = provider_config
        .map(|config| config.resolve_type(&provider_id))
        .unwrap_or_else(|| ProviderType::parse(&provider_id));
    let reasoning_marker_mode = resolved_model
        .reasoning_marker_mode
        .or_else(|| provider_config.map(|config| config.reasoning_markers))
        .unwrap_or_default();
    let initiator = resolve_request_initiator(&settings, &provider_id, request);

    let mut request = request.clone();
    request.model = upstream_model.clone();
    apply_alias_reasoning_effort(&mut request, resolved_model.reasoning_effort);
    apply_reasoning_marker_mode(&mut request, &provider_type, reasoning_marker_mode);

    ResolvedUpstreamRequest {
        provider_id,
        provider_type,
        upstream_model,
        initiator,
        request,
    }
}

fn request_intent(request: &MessagesRequest) -> Option<&str> {
    request
        .metadata
        .as_ref()
        .and_then(|metadata| metadata.get("intent"))
        .and_then(Value::as_str)
}

fn apply_alias_reasoning_effort(
    request: &mut MessagesRequest,
    effort: Option<ModelReasoningEffort>,
) {
    if let Some(value) = effort.and_then(ModelReasoningEffort::request_value) {
        request.extra.remove("reasoning");
        request
            .extra
            .insert("reasoning_effort".to_string(), json!(value));
        request.thinking = None;
    }
}

fn apply_reasoning_marker_mode(
    request: &mut MessagesRequest,
    provider_type: &ProviderType,
    mode: ReasoningMarkerMode,
) {
    if !openai_compatible_marker_mode(provider_type) {
        return;
    }
    request.extra.insert(
        REASONING_MARKER_MODE_EXTRA_KEY.to_string(),
        json!(mode.as_config_value()),
    );
}

fn openai_compatible_marker_mode(provider_type: &ProviderType) -> bool {
    matches!(
        provider_type,
        ProviderType::OpenAI
            | ProviderType::Copilot
            | ProviderType::ChatGPT
            | ProviderType::OpenRouter
            | ProviderType::Google
            | ProviderType::Custom(_)
    )
}

fn resolve_request_initiator(
    settings: &claude_proxy_config::Settings,
    provider_id: &str,
    request: &MessagesRequest,
) -> &'static str {
    let agent_marking_enabled = settings
        .providers
        .get(provider_id)
        .map(|config| match config.resolve_type(provider_id) {
            ProviderType::Copilot => config
                .copilot
                .as_ref()
                .is_none_or(|copilot| copilot.enable_agent_marking),
            ProviderType::ChatGPT => true,
            _ => false,
        })
        .unwrap_or(false);

    if agent_marking_enabled && has_subagent_marker(&request.system) {
        "agent"
    } else {
        "user"
    }
}

fn has_subagent_marker(system: &Option<SystemPrompt>) -> bool {
    let text = match system {
        Some(SystemPrompt::Text(text)) => text.as_str(),
        Some(SystemPrompt::Blocks(blocks)) => blocks.first().map_or("", |block| match block {
            Content::Text { text } => text.as_str(),
            _ => "",
        }),
        None => return false,
    };

    text.contains("__SUBAGENT_MARKER__")
}

async fn get_provider(state: &AppState, provider_id: &str) -> Result<Arc<dyn Provider>, String> {
    state.get_or_create_provider(provider_id).await
}

fn record_request_error(state: &AppState, start: std::time::Instant) {
    state.metrics.record_error();
    state
        .metrics
        .record_latency(start.elapsed().as_millis() as u64);
}

struct StreamPermits {
    _request: OwnedSemaphorePermit,
    _provider: Option<OwnedSemaphorePermit>,
}

#[derive(Debug, Clone, Copy)]
struct StreamResponseConfig {
    heartbeat_interval: Duration,
    idle_timeout: Duration,
    overall_timeout: Duration,
    tool_use_terminal_timeout: Duration,
}

impl StreamResponseConfig {
    fn from_settings(settings: &claude_proxy_config::Settings) -> Self {
        Self {
            heartbeat_interval: seconds_duration(settings.server.sse_heartbeat_interval_seconds),
            idle_timeout: seconds_duration(settings.server.stream_idle_timeout_seconds),
            overall_timeout: seconds_duration(settings.server.stream_overall_timeout_seconds),
            tool_use_terminal_timeout: seconds_duration(
                settings.server.tool_use_terminal_timeout_seconds,
            ),
        }
    }
}

fn seconds_duration(seconds: u64) -> Duration {
    Duration::from_secs(seconds.max(1))
}

struct LeaderResponseContext {
    request_hash: u64,
    broadcast_tx: broadcast::Sender<InflightEvent>,
    permits: StreamPermits,
    start: std::time::Instant,
    observer_state: Arc<StdMutex<RequestObserverState>>,
    observability: Option<ObservabilityContext>,
    stream_config: StreamResponseConfig,
}

async fn acquire_request_permit(
    state: &AppState,
    start: std::time::Instant,
) -> Result<OwnedSemaphorePermit, Response> {
    let semaphore = state.concurrency_semaphore.read().await.clone();
    match tokio::time::timeout(Duration::from_secs(10), semaphore.acquire_owned()).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => {
            error!("Semaphore closed unexpectedly");
            record_request_error(state, start);
            Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &ErrorResponse::api_error("service unavailable"),
            ))
        }
        Err(_) => {
            warn!("Concurrency limit reached, request timed out");
            record_request_error(state, start);
            Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &ErrorResponse::api_error("too many concurrent requests"),
            ))
        }
    }
}

async fn acquire_provider_permit(
    state: &AppState,
    provider_id: &str,
    start: std::time::Instant,
) -> Result<OwnedSemaphorePermit, Response> {
    let max_concurrency = state.settings.read().await.limits.provider_max_concurrency as usize;
    let semaphore = {
        let mut semaphores = state.provider_concurrency_semaphores.lock().await;
        semaphores
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Semaphore::new(max_concurrency)))
            .clone()
    };

    match tokio::time::timeout(Duration::from_secs(10), semaphore.acquire_owned()).await {
        Ok(Ok(permit)) => Ok(permit),
        Ok(Err(_)) => {
            error!("Provider semaphore closed unexpectedly");
            record_request_error(state, start);
            Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &ErrorResponse::api_error("service unavailable"),
            ))
        }
        Err(_) => {
            warn!("Provider concurrency limit reached for {provider_id}");
            record_request_error(state, start);
            Err(error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &ErrorResponse::api_error("provider concurrency limit reached"),
            ))
        }
    }
}

enum InflightRegistration {
    Leader(broadcast::Sender<InflightEvent>),
    Follower(broadcast::Receiver<InflightEvent>),
}

async fn register_or_subscribe_inflight_request(
    state: &AppState,
    request_hash: u64,
) -> InflightRegistration {
    let mut inflight = state.inflight.lock().await;
    if let Some(sender) = inflight.get(&request_hash) {
        let receiver = sender.subscribe();
        debug!("Dedup: joining existing inflight request (hash={request_hash:016x})");
        return InflightRegistration::Follower(receiver);
    }

    let (sender, _) = broadcast::channel::<InflightEvent>(256);
    inflight.insert(request_hash, sender.clone());
    debug!("Dedup: registered new inflight request (hash={request_hash:016x})");
    InflightRegistration::Leader(sender)
}

fn join_inflight_stream(
    mut receiver: broadcast::Receiver<InflightEvent>,
    permits: StreamPermits,
    stream_config: StreamResponseConfig,
) -> Response {
    let (tx, body) = tokio::sync::mpsc::channel::<Result<Vec<u8>, Infallible>>(64);
    tokio::spawn(async move {
        let _permits = permits;
        let mut heartbeat = tokio::time::interval(stream_config.heartbeat_interval);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        heartbeat.tick().await;
        loop {
            tokio::select! {
                event = receiver.recv() => {
                    match event {
                        Ok(InflightEvent::Event(event)) => {
                            let sse_text = format_sse_event(&event);
                            if tx.send(Ok(sse_text.into_bytes())).await.is_err() {
                                break;
                            }
                        }
                        Ok(InflightEvent::Done) | Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            let error_event = stream_api_error_event(format!(
                                "duplicate request stream fell behind and missed {skipped} event(s); please retry"
                            ));
                            let _ = tx
                                .send(Ok(format_sse_event(&error_event).into_bytes()))
                                .await;
                            break;
                        }
                        Ok(InflightEvent::Error(msg)) => {
                            let error_event = stream_api_error_event(msg);
                            let _ = tx
                                .send(Ok(format_sse_event(&error_event).into_bytes()))
                                .await;
                            break;
                        }
                    }
                }
                _ = heartbeat.tick() => {
                    if tx.send(Ok(SSE_HEARTBEAT_FRAME.to_vec())).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    sse_body_response(body)
}

async fn join_inflight_non_stream(
    mut receiver: broadcast::Receiver<InflightEvent>,
    _request_permit: OwnedSemaphorePermit,
) -> Response {
    let mut events = Vec::new();
    loop {
        match receiver.recv().await {
            Ok(InflightEvent::Event(event)) => events.push(event),
            Ok(InflightEvent::Done) | Err(broadcast::error::RecvError::Closed) => break,
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    &ErrorResponse::api_error(&format!(
                        "duplicate request stream fell behind and missed {skipped} event(s); please retry"
                    )),
                );
            }
            Ok(InflightEvent::Error(msg)) => {
                return error_response(StatusCode::BAD_GATEWAY, &ErrorResponse::api_error(&msg));
            }
        }
    }

    let response_data = crate::non_stream::response_from_events(&events)
        .or_else(|| events.last().map(|e| e.data.clone()))
        .unwrap_or(json!({"error": "no response from provider"}));
    Json(response_data).into_response()
}

async fn stream_leader_response(
    state: &AppState,
    request: &RequestMetricsContext,
    mut stream: BoxStream<'static, Result<SseEvent, ProviderError>>,
    context: LeaderResponseContext,
) -> Response {
    let LeaderResponseContext {
        request_hash,
        broadcast_tx,
        permits,
        start: _,
        observer_state,
        observability,
        stream_config,
    } = context;
    let (sender, body) = tokio::sync::mpsc::channel::<Result<Vec<u8>, Infallible>>(64);

    let metrics = state.metrics.clone();
    let health_state = state.clone();
    let provider_id = request.provider_id.clone();
    let initiator = request.initiator;
    let model_name = request.model.clone();
    let inflight_map = state.inflight.clone();
    let request_id = Uuid::new_v4().to_string();
    metrics
        .register_active_stream(
            request_id.clone(),
            provider_id.clone(),
            initiator.to_string(),
            model_name.clone(),
        )
        .await;
    tokio::spawn(async move {
        let _permits = permits;
        let task_start = std::time::Instant::now();
        let mut timing = ObservabilityTiming::default();
        let mut usage = TokenUsage::default();
        let mut had_error = false;
        let mut terminal_reason = "completed";
        let mut last_error = None;
        let mut leader_tx_open = true;
        let mut heartbeat = tokio::time::interval(stream_config.heartbeat_interval);
        heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
        heartbeat.tick().await;
        let mut idle_deadline = Box::pin(tokio::time::sleep(stream_config.idle_timeout));
        let mut overall_deadline = Box::pin(tokio::time::sleep(stream_config.overall_timeout));
        let mut tool_use_deadline =
            Box::pin(tokio::time::sleep(stream_config.tool_use_terminal_timeout));
        let mut tool_use_pending = false;

        loop {
            tokio::select! {
                event_result = stream.next() => {
                    let Some(event_result) = event_result else {
                        break;
                    };
                    idle_deadline
                        .as_mut()
                        .reset(TokioInstant::now() + stream_config.idle_timeout);
                    match event_result {
                        Ok(event) => {
                            let now = std::time::Instant::now();
                            if let Some(context) = &observability {
                                timing.record_event(context.start, now, context.idle_gap_ms);
                            }
                            extract_usage_from_event(&event.data, &mut usage);
                            if sse_event_starts_tool_use(&event) {
                                tool_use_pending = true;
                                tool_use_deadline.as_mut().reset(
                                    TokioInstant::now() + stream_config.tool_use_terminal_timeout,
                                );
                            } else if sse_event_finishes_message(&event) {
                                tool_use_pending = false;
                            } else if tool_use_pending {
                                tool_use_deadline.as_mut().reset(
                                    TokioInstant::now() + stream_config.tool_use_terminal_timeout,
                                );
                            }
                            metrics
                                .update_active_stream(
                                    &request_id,
                                    sse_event_type(&event),
                                    tool_use_pending,
                                )
                                .await;
                            let sse_text = leader_tx_open.then(|| format_sse_event(&event));
                            let _ = broadcast_tx.send(InflightEvent::Event(event));
                            if let Some(sse_text) = sse_text
                                && sender.send(Ok(sse_text.into_bytes())).await.is_err()
                            {
                                leader_tx_open = false;
                            }
                            if !leader_tx_open && broadcast_tx.receiver_count() == 0 {
                                terminal_reason = "client_disconnected";
                                break;
                            }
                        }
                        Err(e) => {
                            let error_message = e.to_string();
                            error!(
                                provider_id = %provider_id,
                                model = %model_name,
                                error = %error_message,
                                "stream error from provider"
                            );
                            had_error = true;
                            terminal_reason = "stream_error";
                            last_error = Some(error_message.clone());
                            let _ = broadcast_tx.send(InflightEvent::Error(error_message.clone()));
                            let error_event = stream_api_error_event(error_message);
                            if leader_tx_open {
                                let sse_text = format_sse_event(&error_event);
                                let _ = sender.send(Ok(sse_text.into_bytes())).await;
                            }
                            break;
                        }
                    }
                }
                _ = heartbeat.tick(), if leader_tx_open => {
                    if sender.send(Ok(SSE_HEARTBEAT_FRAME.to_vec())).await.is_err() {
                        leader_tx_open = false;
                    }
                    if !leader_tx_open && broadcast_tx.receiver_count() == 0 {
                        terminal_reason = "client_disconnected";
                        break;
                    }
                }
                _ = &mut idle_deadline => {
                    let error_message = format!(
                        "upstream stream idle for {}s",
                        stream_config.idle_timeout.as_secs()
                    );
                    warn!(
                        provider_id = %provider_id,
                        model = %model_name,
                        timeout_seconds = stream_config.idle_timeout.as_secs(),
                        "stream idle watchdog fired"
                    );
                    had_error = true;
                    terminal_reason = "stream_idle_timeout";
                    last_error = Some(error_message.clone());
                    let _ = broadcast_tx.send(InflightEvent::Error(error_message.clone()));
                    if leader_tx_open {
                        let error_event = stream_api_error_event(error_message);
                        let sse_text = format_sse_event(&error_event);
                        let _ = sender.send(Ok(sse_text.into_bytes())).await;
                    }
                    break;
                }
                _ = &mut tool_use_deadline, if tool_use_pending => {
                    let error_message = format!(
                        "tool_use stream did not reach message_stop within {}",
                        format_timeout_duration(stream_config.tool_use_terminal_timeout)
                    );
                    warn!(
                        provider_id = %provider_id,
                        model = %model_name,
                        timeout_seconds = stream_config.tool_use_terminal_timeout.as_secs(),
                        "tool_use terminal watchdog fired"
                    );
                    had_error = true;
                    terminal_reason = "tool_use_terminal_timeout";
                    last_error = Some(error_message.clone());
                    let _ = broadcast_tx.send(InflightEvent::Error(error_message.clone()));
                    if leader_tx_open {
                        let error_event = stream_api_error_event(error_message);
                        let sse_text = format_sse_event(&error_event);
                        let _ = sender.send(Ok(sse_text.into_bytes())).await;
                    }
                    break;
                }
                _ = &mut overall_deadline => {
                    let error_message = format!(
                        "upstream stream exceeded overall timeout of {}s",
                        stream_config.overall_timeout.as_secs()
                    );
                    warn!(
                        provider_id = %provider_id,
                        model = %model_name,
                        timeout_seconds = stream_config.overall_timeout.as_secs(),
                        "stream overall watchdog fired"
                    );
                    had_error = true;
                    terminal_reason = "stream_overall_timeout";
                    last_error = Some(error_message.clone());
                    let _ = broadcast_tx.send(InflightEvent::Error(error_message.clone()));
                    if leader_tx_open {
                        let error_event = stream_api_error_event(error_message);
                        let sse_text = format_sse_event(&error_event);
                        let _ = sender.send(Ok(sse_text.into_bytes())).await;
                    }
                    break;
                }
            }
        }
        let _ = broadcast_tx.send(InflightEvent::Done);
        inflight_map.lock().await.remove(&request_hash);
        metrics.remove_active_stream(&request_id).await;
        let latency_ms = task_start.elapsed().as_millis() as u64;
        timing.stream_duration_ms = latency_ms;
        if let Some(error) = last_error {
            health_state
                .record_provider_error(&provider_id, &error)
                .await;
        } else {
            health_state.record_provider_success(&provider_id).await;
        }
        merge_observer_usage(&observer_state, &mut usage);
        metrics.record_latency(latency_ms);
        metrics
            .record_completed_request(CompletedUsageRecord {
                provider: &provider_id,
                initiator,
                model: &model_name,
                usage: &usage,
                is_error: had_error,
                latency_ms,
                terminal_reason,
                error_kind: if had_error { "stream" } else { "" },
            })
            .await;
        if let Some(context) = observability {
            metrics
                .record_observability(
                    build_observability_event(context, timing, had_error, terminal_reason),
                    true,
                )
                .await;
        }
    });

    sse_body_response(body)
}

async fn collect_leader_response(
    state: &AppState,
    request: &RequestMetricsContext,
    mut stream: BoxStream<'static, Result<SseEvent, ProviderError>>,
    context: LeaderResponseContext,
) -> Response {
    let LeaderResponseContext {
        request_hash,
        broadcast_tx,
        permits: _permits,
        start,
        observer_state,
        observability,
        stream_config: _,
    } = context;
    let stream_start = std::time::Instant::now();
    let mut timing = ObservabilityTiming::default();
    let mut events = Vec::new();
    let mut usage = TokenUsage::default();
    while let Some(event_result) = stream.next().await {
        match event_result {
            Ok(event) => {
                if let Some(context) = &observability {
                    timing.record_event(
                        context.start,
                        std::time::Instant::now(),
                        context.idle_gap_ms,
                    );
                }
                extract_usage_from_event(&event.data, &mut usage);
                let _ = broadcast_tx.send(InflightEvent::Event(event.clone()));
                events.push(event);
            }
            Err(e) => {
                let error_message = e.to_string();
                let _ = broadcast_tx.send(InflightEvent::Error(error_message.clone()));
                let _ = broadcast_tx.send(InflightEvent::Done);
                state.inflight.lock().await.remove(&request_hash);
                state
                    .record_provider_error_with_metadata(
                        &request.provider_id,
                        &error_message,
                        e.upstream_metadata().cloned(),
                    )
                    .await;
                let latency_ms = start.elapsed().as_millis() as u64;
                state.metrics.record_error();
                state.metrics.record_latency(latency_ms);
                merge_observer_usage(&observer_state, &mut usage);
                state
                    .metrics
                    .record_completed_request(CompletedUsageRecord {
                        provider: &request.provider_id,
                        initiator: request.initiator,
                        model: &request.model,
                        usage: &usage,
                        is_error: true,
                        latency_ms,
                        terminal_reason: "stream_error",
                        error_kind: "stream",
                    })
                    .await;
                if let Some(context) = observability {
                    timing.stream_duration_ms = stream_start.elapsed().as_millis() as u64;
                    state
                        .metrics
                        .record_observability(
                            build_observability_event(context, timing, true, "stream_error"),
                            true,
                        )
                        .await;
                }
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    &ErrorResponse::api_error(&error_message),
                );
            }
        }
    }
    let _ = broadcast_tx.send(InflightEvent::Done);
    state.inflight.lock().await.remove(&request_hash);

    let response_data = crate::non_stream::response_from_events(&events)
        .or_else(|| events.last().map(|e| e.data.clone()))
        .unwrap_or(json!({"error": "no response from provider"}));

    let latency_ms = start.elapsed().as_millis() as u64;
    merge_observer_usage(&observer_state, &mut usage);
    state
        .metrics
        .record_completed_request(CompletedUsageRecord {
            provider: &request.provider_id,
            initiator: request.initiator,
            model: &request.model,
            usage: &usage,
            is_error: false,
            latency_ms,
            terminal_reason: "completed",
            error_kind: "",
        })
        .await;

    state.record_provider_success(&request.provider_id).await;
    state.metrics.record_latency(latency_ms);
    if let Some(context) = observability {
        timing.stream_duration_ms = stream_start.elapsed().as_millis() as u64;
        state
            .metrics
            .record_observability(
                build_observability_event(context, timing, false, "completed"),
                true,
            )
            .await;
    }
    Json(response_data).into_response()
}

async fn handle_provider_error(
    state: &AppState,
    request: &RequestMetricsContext,
    request_hash: u64,
    broadcast_tx: broadcast::Sender<InflightEvent>,
    error: ProviderError,
    start: std::time::Instant,
    observability: Option<ObservabilityContext>,
) -> Response {
    let _ = broadcast_tx.send(InflightEvent::Error(error.to_string()));
    let _ = broadcast_tx.send(InflightEvent::Done);
    state.inflight.lock().await.remove(&request_hash);

    error!("Provider error: {error}");
    state
        .record_provider_error_with_metadata(
            &request.provider_id,
            &error.to_string(),
            error.upstream_metadata().cloned(),
        )
        .await;
    state.metrics.record_error();
    let latency_ms = start.elapsed().as_millis() as u64;
    state.metrics.record_latency(latency_ms);
    state
        .metrics
        .record_completed_request(CompletedUsageRecord {
            provider: &request.provider_id,
            initiator: request.initiator,
            model: &request.model,
            usage: &TokenUsage::default(),
            is_error: true,
            latency_ms,
            terminal_reason: "provider_error",
            error_kind: provider_error_kind(&error),
        })
        .await;
    if let Some(context) = observability {
        state
            .metrics
            .record_observability(
                build_observability_event(
                    context,
                    ObservabilityTiming::default(),
                    true,
                    "provider_error",
                ),
                true,
            )
            .await;
    }
    provider_error_to_response(&error)
}

fn sse_body_response(body: tokio::sync::mpsc::Receiver<Result<Vec<u8>, Infallible>>) -> Response {
    let stream_body = tokio_stream::wrappers::ReceiverStream::new(body);
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache, no-transform")
        .header("connection", "keep-alive")
        .header("x-accel-buffering", "no")
        .body(Body::from_stream(stream_body))
        .unwrap()
}

/// GET /v1/models — list available models
pub async fn list_models(State(state): State<AppState>) -> Json<Value> {
    refresh_missing_model_caches(&state).await;

    let registry = state.provider_registry.read().await;
    let models = registry.all_cached_models();

    let data: Vec<Value> = models
        .iter()
        .map(|m| {
            json!({
                "id": m.model_id,
                "object": "model",
                "vendor": m.vendor,
                "is_chat_default": m.is_chat_default,
                "supported_endpoints": m.capabilities.endpoints.supported_paths(),
                "capabilities": m.capabilities,
            })
        })
        .collect();

    Json(json!({
        "data": data,
        "object": "list"
    }))
}

async fn refresh_missing_model_caches(state: &AppState) {
    let provider_ids: Vec<String> = {
        let settings = state.settings.read().await;
        settings.providers.keys().cloned().collect()
    };

    for provider_id in provider_ids {
        if let Err(error) = state.get_or_refresh_models(&provider_id).await {
            warn!("{error}");
        }
    }
}

/// GET /admin/config — get current config (keys masked)
pub async fn admin_get_config(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let settings = state.settings.read().await;
    if !check_admin_auth(&headers, settings.admin_auth_token()) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            &ErrorResponse::authentication("invalid admin token"),
        );
    }
    let toml_str = settings.to_toml();
    let masked = mask_toml_keys(&toml_str);
    Json(json!({"config": masked})).into_response()
}

/// PUT /admin/config — update config
pub async fn admin_update_config(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(update): Json<Value>,
) -> Response {
    let settings = state.settings.read().await;
    if !check_admin_auth(&headers, settings.admin_auth_token()) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            &ErrorResponse::authentication("invalid admin token"),
        );
    }
    drop(settings);

    let toml_str = match update.get("config").and_then(|c| c.as_str()) {
        Some(s) => s.to_string(),
        None => {
            return error_response(
                StatusCode::BAD_REQUEST,
                &ErrorResponse::invalid_request("expected {\"config\": \"<toml>\"}"),
            );
        }
    };

    match claude_proxy_config::Settings::from_toml(&toml_str, std::path::Path::new("admin-update"))
    {
        Ok(new_settings) => {
            if let Err(e) = new_settings.validate() {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    &ErrorResponse::invalid_request(&format!("validation failed: {e}")),
                );
            }

            let config_toml = new_settings.to_toml();
            state.apply_settings(new_settings).await;

            if let Some(path) = claude_proxy_config::Settings::config_file_path()
                && let Err(e) = std::fs::write(&path, config_toml)
            {
                error!("Failed to write config to disk: {e}");
            }

            info!("Config updated via admin API");
            Json(json!({"status": "ok"})).into_response()
        }
        Err(e) => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorResponse::invalid_request(&format!("invalid config: {e}")),
        ),
    }
}

/// POST /admin/restart — trigger config reload
pub async fn admin_restart(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let settings = state.settings.read().await;
    if !check_admin_auth(&headers, settings.admin_auth_token()) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            &ErrorResponse::authentication("invalid admin token"),
        );
    }
    drop(settings);

    if let Some(path) = claude_proxy_config::Settings::config_file_path() {
        match claude_proxy_config::Settings::load(&path) {
            Ok(new_settings) => {
                if let Err(e) = new_settings.validate() {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        &ErrorResponse::invalid_request(&format!("validation failed: {e}")),
                    );
                }
                state.apply_settings(new_settings).await;
                info!("Config reloaded via admin restart");
                Json(json!({"status": "reloaded"})).into_response()
            }
            Err(e) => {
                error!("Config reload failed: {e}");
                error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    &ErrorResponse::api_error(&format!("reload failed: {e}")),
                )
            }
        }
    } else {
        error_response(
            StatusCode::NOT_FOUND,
            &ErrorResponse::not_found("no config file found"),
        )
    }
}

/// GET /admin/metrics
pub async fn admin_metrics(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let settings = state.settings.read().await;
    if !check_admin_auth(&headers, settings.admin_auth_token()) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            &ErrorResponse::authentication("invalid admin token"),
        );
    }
    let provider_ids = settings.providers.keys().cloned().collect::<Vec<_>>();
    let rate_limit_provider_ids = settings
        .providers
        .iter()
        .filter(|(provider_id, config)| config.resolve_type(provider_id) == ProviderType::ChatGPT)
        .map(|(provider_id, _)| provider_id.clone())
        .collect::<Vec<_>>();
    drop(settings);

    let (model_capabilities, model_cache) = {
        let registry = state.provider_registry.read().await;
        (
            registry.model_capabilities(),
            registry.model_cache_status(&provider_ids),
        )
    };
    let provider_rate_limits = provider_rate_limit_snapshots(&state, rate_limit_provider_ids).await;
    let provider_health = state.provider_health_snapshot(provider_ids).await;
    let mut metrics = state.metrics.to_json().await;
    if let Some(object) = metrics.as_object_mut() {
        object.insert("model_capabilities".to_string(), model_capabilities);
        object.insert(
            "model_cache".to_string(),
            serde_json::to_value(model_cache).unwrap_or_default(),
        );
        object.insert(
            "provider_rate_limits".to_string(),
            serde_json::to_value(provider_rate_limits).unwrap_or_default(),
        );
        object.insert(
            "provider_health".to_string(),
            serde_json::to_value(provider_health).unwrap_or_default(),
        );
    }
    Json(metrics).into_response()
}

/// POST /admin/models/refresh — force refresh model caches for all configured providers
pub async fn admin_refresh_models(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let settings = state.settings.read().await;
    if !check_admin_auth(&headers, settings.admin_auth_token()) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            &ErrorResponse::authentication("invalid admin token"),
        );
    }
    let provider_ids = settings.providers.keys().cloned().collect::<Vec<_>>();
    drop(settings);

    let mut refreshed = serde_json::Map::new();
    let mut errors = serde_json::Map::new();
    for provider_id in &provider_ids {
        match state.refresh_models(provider_id).await {
            Ok(models) => {
                refreshed.insert(provider_id.clone(), json!(models.len()));
            }
            Err(error) => {
                warn!("{error}");
                errors.insert(provider_id.clone(), json!(error));
            }
        }
    }

    let model_cache = state
        .provider_registry
        .read()
        .await
        .model_cache_status(&provider_ids);
    let status = if errors.is_empty() { "ok" } else { "partial" };

    Json(json!({
        "status": status,
        "refreshed": refreshed,
        "errors": errors,
        "model_cache": model_cache,
    }))
    .into_response()
}

async fn provider_rate_limit_snapshots(
    state: &AppState,
    provider_ids: Vec<String>,
) -> std::collections::HashMap<String, Vec<claude_proxy_providers::provider::RateLimitSnapshot>> {
    let mut snapshots = std::collections::HashMap::new();
    for provider_id in provider_ids {
        let Ok(provider) = state.get_or_create_provider(&provider_id).await else {
            continue;
        };
        if let Ok(provider_snapshots) = provider.rate_limit_snapshots().await
            && !provider_snapshots.is_empty()
        {
            snapshots.insert(provider_id, provider_snapshots);
        }
    }
    snapshots
}

fn format_sse_event(event: &SseEvent) -> String {
    let data_str = serde_json::to_string(&event.data).unwrap_or_default();
    if event.event.is_empty() {
        format!("data: {data_str}\n\n")
    } else {
        format!("event: {}\ndata: {data_str}\n\n", event.event)
    }
}

fn stream_api_error_event(message: impl Into<String>) -> SseEvent {
    SseEvent {
        event: "error".to_string(),
        data: json!({
            "type": "error",
            "error": {
                "type": "api_error",
                "message": message.into()
            }
        }),
    }
}

fn sse_event_starts_tool_use(event: &SseEvent) -> bool {
    event.event == "content_block_start"
        && event.data["content_block"]["type"].as_str() == Some("tool_use")
}

fn sse_event_finishes_message(event: &SseEvent) -> bool {
    event.event == "message_stop" || event.data["type"].as_str() == Some("message_stop")
}

fn sse_event_type(event: &SseEvent) -> String {
    event
        .data
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| (!event.event.is_empty()).then_some(event.event.as_str()))
        .unwrap_or("unknown")
        .to_string()
}

fn format_timeout_duration(duration: Duration) -> String {
    if duration.as_millis().is_multiple_of(1000) {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

fn overloaded_status() -> StatusCode {
    StatusCode::from_u16(529).unwrap_or(StatusCode::SERVICE_UNAVAILABLE)
}

fn overloaded_error(message: &str) -> ErrorResponse {
    ErrorResponse {
        r#type: "error".to_string(),
        error: AnthropicError {
            error_type: "overloaded_error".to_string(),
            message: message.to_string(),
        },
    }
}

fn overloaded_response(message: &str, retry_after: Option<u64>) -> Response {
    let mut response = error_response(overloaded_status(), &overloaded_error(message));
    response
        .headers_mut()
        .insert("x-should-retry", HeaderValue::from_static("true"));
    if let Some(secs) = retry_after
        && let Ok(header_value) = HeaderValue::from_str(&secs.to_string())
    {
        response.headers_mut().insert("retry-after", header_value);
    }
    response
}

fn is_retryable_upstream_error_status(status: u16) -> bool {
    matches!(status, 408 | 409 | 500..=599)
}

fn provider_error_kind(error: &ProviderError) -> &'static str {
    match error.without_upstream_metadata() {
        ProviderError::Authentication(_) => "authentication",
        ProviderError::RateLimited { .. } => "rate_limited",
        ProviderError::InvalidRequest(_) => "invalid_request",
        ProviderError::RequestTooLarge(_) => "request_too_large",
        ProviderError::Overloaded { .. } => "overloaded",
        ProviderError::ModelNotFound(_) => "model_not_found",
        ProviderError::Timeout => "timeout",
        ProviderError::UpstreamError { .. } => "upstream",
        ProviderError::ServiceUnavailable(_) => "service_unavailable",
        ProviderError::Network(_) => "network",
        ProviderError::WithUpstreamMetadata { .. } => unreachable!(),
    }
}

fn provider_error_to_response(error: &ProviderError) -> Response {
    let metadata = error.upstream_metadata();
    let mut response = match error.without_upstream_metadata() {
        ProviderError::Authentication(msg) => error_response(
            StatusCode::UNAUTHORIZED,
            &ErrorResponse::authentication(msg),
        ),
        ProviderError::RateLimited { retry_after } => {
            let mut response = error_response(
                StatusCode::TOO_MANY_REQUESTS,
                &ErrorResponse::rate_limit("rate limited by upstream"),
            );
            if let Some(secs) = retry_after
                && let Ok(header_value) = axum::http::HeaderValue::from_str(&secs.to_string())
            {
                response.headers_mut().insert("retry-after", header_value);
            }
            response
        }
        ProviderError::InvalidRequest(msg) => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorResponse::invalid_request(msg),
        ),
        ProviderError::RequestTooLarge(msg) => error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            &ErrorResponse::invalid_request(msg),
        ),
        ProviderError::Overloaded {
            message,
            retry_after,
        } => overloaded_response(message, *retry_after),
        ProviderError::ModelNotFound(msg) => {
            error_response(StatusCode::NOT_FOUND, &ErrorResponse::not_found(msg))
        }
        ProviderError::Timeout => error_response(
            StatusCode::GATEWAY_TIMEOUT,
            &ErrorResponse::timeout("upstream request timed out"),
        ),
        ProviderError::UpstreamError { status, body } => {
            error!("Upstream error (HTTP {status}): {body}");
            upstream_error_to_response(*status, body)
        }
        ProviderError::ServiceUnavailable(msg) => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorResponse::api_error(msg),
        ),
        ProviderError::Network(msg) => error_response(
            StatusCode::BAD_GATEWAY,
            &ErrorResponse::api_error(&format!("network error: {msg}")),
        ),
        ProviderError::WithUpstreamMetadata { .. } => unreachable!(),
    };
    attach_upstream_error_headers(&mut response, metadata);
    response
}

fn upstream_error_to_response(status: u16, body: &str) -> Response {
    let message = extract_upstream_error_message(body);
    match status {
        400 => error_response(
            StatusCode::BAD_REQUEST,
            &ErrorResponse::invalid_request(&message),
        ),
        413 => error_response(
            StatusCode::PAYLOAD_TOO_LARGE,
            &ErrorResponse::invalid_request(&message),
        ),
        status if is_retryable_upstream_error_status(status) => overloaded_response(&message, None),
        _ => error_response(StatusCode::BAD_GATEWAY, &ErrorResponse::api_error(&message)),
    }
}

fn attach_upstream_error_headers(
    response: &mut Response,
    metadata: Option<&UpstreamErrorMetadata>,
) {
    let Some(metadata) = metadata else {
        return;
    };
    if let Ok(header_value) = HeaderValue::from_str(&metadata.status.to_string()) {
        response
            .headers_mut()
            .insert("x-upstream-status", header_value);
    }
    if let Some(request_id) = metadata.request_id.as_deref()
        && let Ok(header_value) = HeaderValue::from_str(request_id)
    {
        response
            .headers_mut()
            .insert("x-upstream-request-id", header_value);
    }
    if let Some(secs) = metadata.retry_after
        && !response.headers().contains_key("retry-after")
        && let Ok(header_value) = HeaderValue::from_str(&secs.to_string())
    {
        response.headers_mut().insert("retry-after", header_value);
    }
}

fn extract_upstream_error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| {
            if body.trim().is_empty() {
                "upstream unavailable".to_string()
            } else {
                body.to_string()
            }
        })
}

fn error_response(status: StatusCode, error: &ErrorResponse) -> Response {
    let body = serde_json::to_string(error).unwrap_or_default();
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap()
}

fn mask_toml_keys(toml_str: &str) -> String {
    let mut result = String::new();
    for line in toml_str.lines() {
        let trimmed = line.trim();
        if (trimmed.starts_with("api_key") || trimmed.starts_with("auth_token"))
            && trimmed.contains('=')
            && let Some((key, _)) = trimmed.split_once('=')
        {
            result.push_str(&format!("{} = \"***\"", key.trim()));
            result.push('\n');
            continue;
        }
        result.push_str(line);
        result.push('\n');
    }
    result
}

/// Extract token usage from a streaming SSE event.
fn extract_usage_from_event(data: &Value, usage: &mut TokenUsage) {
    if let Some(message) = data.get("message")
        && let Some(u) = message.get("usage")
    {
        update_usage_snapshot(u, usage);
    }

    if let Some(u) = data.get("usage") {
        update_usage_snapshot(u, usage);
    }
}

fn update_usage_snapshot(u: &Value, usage: &mut TokenUsage) {
    if let Some(v) = u.get("input_tokens").and_then(|v| v.as_u64()) {
        usage.input_tokens = usage.input_tokens.max(v);
    }
    if let Some(v) = u.get("output_tokens").and_then(|v| v.as_u64()) {
        usage.output_tokens = usage.output_tokens.max(v);
    }
    if let Some(v) = u
        .get("cache_creation_input_tokens")
        .and_then(|v| v.as_u64())
    {
        usage.cache_creation_input_tokens = usage.cache_creation_input_tokens.max(v);
    }
    if let Some(v) = u.get("cache_read_input_tokens").and_then(|v| v.as_u64()) {
        usage.cache_read_input_tokens = usage.cache_read_input_tokens.max(v);
    }
}

fn merge_observer_usage(state: &Arc<StdMutex<RequestObserverState>>, usage: &mut TokenUsage) {
    if let Ok(state) = state.lock() {
        merge_usage_snapshot(&state.stream_usage, usage);
    }
}

fn merge_provider_usage_metadata(provider_usage: &ProviderUsageMetadata, usage: &mut TokenUsage) {
    usage.input_tokens = usage.input_tokens.max(provider_usage.input_tokens);
    usage.output_tokens = usage.output_tokens.max(provider_usage.output_tokens);
    usage.cache_creation_input_tokens = usage
        .cache_creation_input_tokens
        .max(provider_usage.cache_creation_input_tokens);
    usage.cache_read_input_tokens = usage
        .cache_read_input_tokens
        .max(provider_usage.cache_read_input_tokens);
}

fn merge_usage_snapshot(snapshot: &TokenUsage, usage: &mut TokenUsage) {
    usage.input_tokens = usage.input_tokens.max(snapshot.input_tokens);
    usage.output_tokens = usage.output_tokens.max(snapshot.output_tokens);
    usage.cache_creation_input_tokens = usage
        .cache_creation_input_tokens
        .max(snapshot.cache_creation_input_tokens);
    usage.cache_read_input_tokens = usage
        .cache_read_input_tokens
        .max(snapshot.cache_read_input_tokens);
}

/// Compute a hash of the request for deduplication purposes.
/// Two requests with the same model, messages, system, tools, and parameters
/// will produce the same hash.
fn compute_request_hash(request: &MessagesRequest) -> u64 {
    let mut hasher = DefaultHasher::new();
    let mut writer = HashWriter(&mut hasher);
    let _ = serde_json::to_writer(&mut writer, request);
    hasher.finish()
}

struct HashWriter<'a, H>(&'a mut H);

impl<H: Hasher> io::Write for HashWriter<'_, H> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::body::to_bytes;
    use claude_proxy_config::settings::{
        AdminConfig, HttpConfig, LimitsConfig, LogConfig, ModelAliasConfig, ModelConfig,
        ObservabilityConfig, ProviderConfig, ProviderType, ServerConfig,
    };

    use super::*;

    fn settings_with_provider(provider_type: ProviderType) -> claude_proxy_config::Settings {
        let mut providers = HashMap::new();
        providers.insert(
            "test".to_string(),
            ProviderConfig {
                api_key: String::new(),
                base_url: String::new(),
                proxy: String::new(),
                provider_type: Some(provider_type),
                copilot: None,
                chatgpt: None,
                runtime: Default::default(),
                reasoning_markers: Default::default(),
            },
        );

        claude_proxy_config::Settings {
            providers,
            model: ModelConfig {
                default: ModelAliasConfig::new("test/model"),
                reasoning: None,
                opus: None,
                sonnet: None,
                haiku: None,
            },
            server: ServerConfig {
                host: "127.0.0.1".to_string(),
                port: 0,
                auth_token: String::new(),
                ..ServerConfig::default()
            },
            admin: AdminConfig { auth_token: None },
            limits: LimitsConfig::default(),
            http: HttpConfig::default(),
            log: LogConfig::default(),
            observability: ObservabilityConfig::default(),
        }
    }

    fn test_stream_config() -> StreamResponseConfig {
        StreamResponseConfig {
            heartbeat_interval: Duration::from_secs(15),
            idle_timeout: Duration::from_secs(120),
            overall_timeout: Duration::from_secs(600),
            tool_use_terminal_timeout: Duration::from_millis(50),
        }
    }

    #[test]
    fn sse_body_response_disables_intermediary_buffering() {
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let response = sse_body_response(rx);
        let headers = response.headers();

        assert_eq!(headers["content-type"], "text/event-stream");
        assert_eq!(headers["cache-control"], "no-cache, no-transform");
        assert_eq!(headers["connection"], "keep-alive");
        assert_eq!(headers["x-accel-buffering"], "no");
    }

    #[test]
    fn stream_api_error_event_uses_anthropic_error_shape() {
        let event = stream_api_error_event("upstream stream was malformed");

        assert_eq!(event.event, "error");
        assert_eq!(event.data["type"], "error");
        assert_eq!(event.data["error"]["type"], "api_error");
        assert_eq!(
            event.data["error"]["message"],
            "upstream stream was malformed"
        );
    }

    fn model_capability_fixture(model_id: &str) -> ModelInfo {
        ModelInfo {
            model_id: model_id.to_string(),
            vendor: Some("openai".to_string()),
            is_chat_default: None,
            capabilities: ModelCapabilities {
                endpoints: EndpointCapabilities {
                    anthropic_messages: CapabilityState::Unsupported,
                    openai_chat_completions: CapabilityState::Unsupported,
                    openai_responses: CapabilityState::Supported,
                },
                features: FeatureCapabilities {
                    thinking: CapabilityState::Supported,
                    reasoning_effort: CapabilityState::Supported,
                    ..Default::default()
                },
                limits: ModelLimits {
                    max_output_tokens: Some(128_000),
                    context_window: Some(400_000),
                    reasoning_effort_levels: vec!["low".to_string(), "high".to_string()],
                    ..Default::default()
                },
                supported_parameters: vec!["messages".to_string(), "thinking".to_string()],
                ..Default::default()
            },
        }
    }

    fn request_with_system(system: Option<SystemPrompt>) -> MessagesRequest {
        MessagesRequest {
            model: "test/model".to_string(),
            system,
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
            extra: HashMap::new(),
        }
    }

    #[test]
    fn alias_reasoning_effort_injects_fixed_request_value() {
        let mut request = request_with_system(None);

        apply_alias_reasoning_effort(&mut request, Some(ModelReasoningEffort::High));

        assert_eq!(
            request
                .extra
                .get("reasoning_effort")
                .and_then(Value::as_str),
            Some("high")
        );
    }

    #[test]
    fn alias_reasoning_effort_default_preserves_intent_behavior() {
        let mut request = request_with_system(None);

        apply_alias_reasoning_effort(&mut request, Some(ModelReasoningEffort::Auto));

        assert!(!request.extra.contains_key("reasoning_effort"));
    }

    #[test]
    fn alias_reasoning_effort_overrides_request_reasoning_fields() {
        let mut request = request_with_system(None);
        request.thinking = Some(ThinkingConfig {
            r#type: Some("adaptive".to_string()),
            budget_tokens: None,
        });
        request
            .extra
            .insert("reasoning".to_string(), json!({"effort": "low"}));
        request
            .extra
            .insert("reasoning_effort".to_string(), json!("medium"));

        apply_alias_reasoning_effort(&mut request, Some(ModelReasoningEffort::High));

        assert_eq!(
            request
                .extra
                .get("reasoning_effort")
                .and_then(Value::as_str),
            Some("high")
        );
        assert!(!request.extra.contains_key("reasoning"));
        assert!(request.thinking.is_none());
    }

    #[test]
    fn reasoning_marker_mode_is_internal_to_openai_compatible_providers() {
        let mut request = request_with_system(None);

        apply_reasoning_marker_mode(
            &mut request,
            &ProviderType::OpenAI,
            ReasoningMarkerMode::LegacyTags,
        );

        assert_eq!(
            request
                .extra
                .get(REASONING_MARKER_MODE_EXTRA_KEY)
                .and_then(Value::as_str),
            Some("legacy_tags")
        );

        let mut anthropic_request = request_with_system(None);
        apply_reasoning_marker_mode(
            &mut anthropic_request,
            &ProviderType::Anthropic,
            ReasoningMarkerMode::LegacyTags,
        );
        assert!(
            !anthropic_request
                .extra
                .contains_key(REASONING_MARKER_MODE_EXTRA_KEY)
        );
    }

    #[test]
    fn request_intent_reads_metadata_intent() {
        let mut request = request_with_system(None);
        request.metadata = Some(json!({"intent": "deep_think"}));

        assert_eq!(request_intent(&request), Some("deep_think"));
    }

    #[test]
    fn client_session_metadata_uses_safe_headers_when_missing() {
        let request = request_with_system(None);
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-session-id",
            HeaderValue::from_static("session-123"),
        );

        let request = attach_client_session_metadata(request, &headers);

        assert_eq!(
            request
                .extra
                .get("client_session_id")
                .and_then(Value::as_str),
            Some("session-123")
        );
    }

    #[test]
    fn client_session_metadata_preserves_explicit_request_value() {
        let mut request = request_with_system(None);
        request
            .extra
            .insert("client_session_id".to_string(), json!("explicit-session"));
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-session-id",
            HeaderValue::from_static("header-session"),
        );

        let request = attach_client_session_metadata(request, &headers);

        assert_eq!(
            request
                .extra
                .get("client_session_id")
                .and_then(Value::as_str),
            Some("explicit-session")
        );
    }

    #[test]
    fn chatgpt_subagent_marker_resolves_agent_initiator() {
        let settings = settings_with_provider(ProviderType::ChatGPT);
        let request = request_with_system(Some(SystemPrompt::Text(
            "prefix __SUBAGENT_MARKER__ suffix".to_string(),
        )));

        assert_eq!(
            resolve_request_initiator(&settings, "test", &request),
            "agent"
        );
    }

    #[test]
    fn openai_subagent_marker_stays_user_initiator() {
        let settings = settings_with_provider(ProviderType::OpenAI);
        let request = request_with_system(Some(SystemPrompt::Text(
            "prefix __SUBAGENT_MARKER__ suffix".to_string(),
        )));

        assert_eq!(
            resolve_request_initiator(&settings, "test", &request),
            "user"
        );
    }

    #[test]
    fn extracts_anthropic_upstream_error_message() {
        let body = r#"{"type":"error","error":{"type":"invalid_request_error","message":"bad thinking block"}}"#;

        assert_eq!(extract_upstream_error_message(body), "bad thinking block");
    }

    #[test]
    fn upstream_400_maps_to_invalid_request_response() {
        let response = upstream_error_to_response(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"bad request"}}"#,
        );

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn concurrent_inflight_registration_produces_single_leader() {
        let state = AppState::new(settings_with_provider(ProviderType::OpenAI), None);
        let request_hash = 0xfeed_f00d;
        let tasks = (0..32)
            .map(|_| {
                let state = state.clone();
                tokio::spawn(async move {
                    matches!(
                        register_or_subscribe_inflight_request(&state, request_hash).await,
                        InflightRegistration::Leader(_)
                    )
                })
            })
            .collect::<Vec<_>>();

        let results = futures::future::join_all(tasks).await;
        let leader_count = results
            .into_iter()
            .filter(|result| *result.as_ref().unwrap())
            .count();

        assert_eq!(leader_count, 1);
        assert_eq!(state.inflight.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn stream_leader_continues_for_followers_after_leader_disconnect() {
        let state = AppState::new(settings_with_provider(ProviderType::OpenAI), None);
        let request = RequestMetricsContext {
            provider_id: "test".to_string(),
            model: "model".to_string(),
            initiator: "user",
        };
        let (broadcast_tx, mut follower) = broadcast::channel::<InflightEvent>(16);
        let (event_tx, event_rx) = tokio::sync::mpsc::channel::<Result<SseEvent, ProviderError>>(2);
        let request_permit = Arc::new(tokio::sync::Semaphore::new(1))
            .acquire_owned()
            .await
            .unwrap();

        let response = stream_leader_response(
            &state,
            &request,
            tokio_stream::wrappers::ReceiverStream::new(event_rx).boxed(),
            LeaderResponseContext {
                request_hash: 0xdead_beef,
                broadcast_tx,
                permits: StreamPermits {
                    _request: request_permit,
                    _provider: None,
                },
                start: std::time::Instant::now(),
                observer_state: Arc::new(StdMutex::new(RequestObserverState::default())),
                observability: None,
                stream_config: test_stream_config(),
            },
        )
        .await;
        drop(response);

        for text in ["first", "second"] {
            event_tx
                .send(Ok(SseEvent {
                    event: String::new(),
                    data: json!({
                        "type": "content_block_delta",
                        "delta": {"text": text}
                    }),
                }))
                .await
                .unwrap();
        }
        drop(event_tx);

        let mut received = Vec::new();
        loop {
            let event = tokio::time::timeout(Duration::from_secs(1), follower.recv())
                .await
                .unwrap()
                .unwrap();
            match event {
                InflightEvent::Event(event) => {
                    received.push(event.data["delta"]["text"].as_str().unwrap().to_string());
                }
                InflightEvent::Done => break,
                InflightEvent::Error(message) => panic!("unexpected error event: {message}"),
            }
        }

        assert_eq!(received, ["first", "second"]);
    }

    #[tokio::test]
    async fn stream_leader_times_out_unfinished_tool_use() {
        let state = AppState::new(settings_with_provider(ProviderType::OpenAI), None);
        let request = RequestMetricsContext {
            provider_id: "test".to_string(),
            model: "model".to_string(),
            initiator: "user",
        };
        let (broadcast_tx, _follower) = broadcast::channel::<InflightEvent>(16);
        let (event_tx, event_rx) = tokio::sync::mpsc::channel::<Result<SseEvent, ProviderError>>(2);
        let request_permit = Arc::new(tokio::sync::Semaphore::new(1))
            .acquire_owned()
            .await
            .unwrap();

        let response = stream_leader_response(
            &state,
            &request,
            tokio_stream::wrappers::ReceiverStream::new(event_rx).boxed(),
            LeaderResponseContext {
                request_hash: 0xbeef_dead,
                broadcast_tx,
                permits: StreamPermits {
                    _request: request_permit,
                    _provider: None,
                },
                start: std::time::Instant::now(),
                observer_state: Arc::new(StdMutex::new(RequestObserverState::default())),
                observability: None,
                stream_config: test_stream_config(),
            },
        )
        .await;

        event_tx
            .send(Ok(SseEvent {
                event: "content_block_start".to_string(),
                data: json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": "call_1",
                        "name": "Read",
                        "input": {}
                    }
                }),
            }))
            .await
            .unwrap();

        let body = tokio::time::timeout(
            Duration::from_secs(1),
            to_bytes(response.into_body(), usize::MAX),
        )
        .await
        .expect("tool_use watchdog should finish the response")
        .unwrap();
        let body = std::str::from_utf8(&body).unwrap();

        assert!(body.contains("content_block_start"));
        assert!(body.contains("event: error"));
        assert!(body.contains("tool_use stream did not reach message_stop"));
    }

    #[tokio::test]
    async fn inflight_stream_follower_reports_lagged_broadcast() {
        let (broadcast_tx, receiver) = broadcast::channel::<InflightEvent>(1);
        let request_permit = Arc::new(tokio::sync::Semaphore::new(1))
            .acquire_owned()
            .await
            .unwrap();
        for text in ["first", "second"] {
            broadcast_tx
                .send(InflightEvent::Event(SseEvent {
                    event: String::new(),
                    data: json!({
                        "type": "content_block_delta",
                        "delta": {"text": text}
                    }),
                }))
                .unwrap();
        }

        let response = join_inflight_stream(
            receiver,
            StreamPermits {
                _request: request_permit,
                _provider: None,
            },
            test_stream_config(),
        );
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body = std::str::from_utf8(&body).unwrap();

        assert!(body.contains("event: error"));
        assert!(body.contains("missed 1 event"));
    }

    #[tokio::test]
    async fn inflight_non_stream_follower_reports_lagged_broadcast() {
        let (broadcast_tx, receiver) = broadcast::channel::<InflightEvent>(1);
        let request_permit = Arc::new(tokio::sync::Semaphore::new(1))
            .acquire_owned()
            .await
            .unwrap();
        for text in ["first", "second"] {
            broadcast_tx
                .send(InflightEvent::Event(SseEvent {
                    event: String::new(),
                    data: json!({
                        "type": "content_block_delta",
                        "delta": {"text": text}
                    }),
                }))
                .unwrap();
        }

        let response = join_inflight_non_stream(receiver, request_permit).await;

        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("missed 1 event")
        );
    }

    #[tokio::test]
    async fn provider_overloaded_maps_to_retryable_529_response() {
        let response = provider_error_to_response(&ProviderError::Overloaded {
            message: "upstream overloaded".to_string(),
            retry_after: Some(3),
        });

        assert_eq!(response.status().as_u16(), 529);
        assert_eq!(
            response.headers().get("retry-after").unwrap(),
            HeaderValue::from_static("3")
        );
        assert_eq!(
            response.headers().get("x-should-retry").unwrap(),
            HeaderValue::from_static("true")
        );

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "overloaded_error");
        assert_eq!(body["error"]["message"], "upstream overloaded");
    }

    #[tokio::test]
    async fn provider_error_response_exposes_safe_upstream_headers() {
        let response = provider_error_to_response(
            &ProviderError::RateLimited {
                retry_after: Some(5),
            }
            .with_upstream_metadata(UpstreamErrorMetadata {
                status: 429,
                retry_after: Some(5),
                request_id: Some("req_abc".to_string()),
                message: Some("slow down".to_string()),
                body_preview: None,
                headers: Vec::new(),
            }),
        );

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response.headers().get("retry-after").unwrap(),
            HeaderValue::from_static("5")
        );
        assert_eq!(
            response.headers().get("x-upstream-status").unwrap(),
            HeaderValue::from_static("429")
        );
        assert_eq!(
            response.headers().get("x-upstream-request-id").unwrap(),
            HeaderValue::from_static("req_abc")
        );

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "rate_limit_error");
        assert_eq!(body["error"]["message"], "rate limited by upstream");
    }

    #[tokio::test]
    async fn upstream_529_maps_to_overloaded_error_response() {
        let response = upstream_error_to_response(
            529,
            r#"{"error":{"type":"overloaded_error","message":"too busy"}}"#,
        );

        assert_eq!(response.status().as_u16(), 529);
        assert_eq!(
            response.headers().get("x-should-retry").unwrap(),
            HeaderValue::from_static("true")
        );

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["type"], "overloaded_error");
        assert_eq!(body["error"]["message"], "too busy");
    }

    #[tokio::test]
    async fn retryable_upstream_errors_map_to_overloaded_response() {
        for status in [408, 409, 500, 502, 503, 504] {
            let response = upstream_error_to_response(
                status,
                r#"{"error":{"type":"upstream_error","message":"temporary upstream failure"}}"#,
            );

            assert_eq!(response.status().as_u16(), 529);
            assert_eq!(
                response.headers().get("x-should-retry").unwrap(),
                HeaderValue::from_static("true")
            );

            let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
            let body: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["type"], "overloaded_error");
            assert_eq!(body["error"]["message"], "temporary upstream failure");
        }
    }

    #[test]
    fn request_payload_stats_counts_blocks_without_storing_content() {
        let mut request = request_with_system(Some(SystemPrompt::Blocks(vec![Content::Text {
            text: "system".to_string(),
        }])));
        request.messages = vec![Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![
                Content::Text {
                    text: "hello".to_string(),
                },
                Content::ToolResult {
                    tool_use_id: "toolu_1".to_string(),
                    content: Some(json!([{"type": "text", "text": "tool output"}])),
                    is_error: None,
                },
            ]),
        }];

        let stats = request_payload_stats(&request);

        assert_eq!(stats.messages, 1);
        assert_eq!(stats.content_blocks, 3);
        assert_eq!(stats.tool_results, 1);
        assert_eq!(
            stats.text_bytes,
            "system".len() as u64 + "hello".len() as u64 + "tool output".len() as u64
        );
    }

    #[test]
    fn provider_request_observer_keeps_prompt_too_long_max_stats() {
        let state = Arc::new(StdMutex::new(RequestObserverState::default()));
        let observer = provider_request_observer(state.clone());

        observer(ProviderRequestObserverEvent {
            event: ProviderRequestObserverEventKind::PromptTooLongRetry,
            prompt_too_long_retries: 1,
            original_body_bytes: 300,
            shrunk_body_bytes: 200,
            dropped_items: 1,
            ..ProviderRequestObserverEvent::default()
        });
        observer(ProviderRequestObserverEvent {
            event: ProviderRequestObserverEventKind::PromptTooLongRetry,
            prompt_too_long_retries: 2,
            original_body_bytes: 280,
            shrunk_body_bytes: 150,
            dropped_items: 3,
            ..ProviderRequestObserverEvent::default()
        });

        let state = state.lock().unwrap();
        assert_eq!(state.prompt_too_long_retries, 2);
        assert_eq!(state.prompt_too_long_original_body_bytes, 300);
        assert_eq!(state.prompt_too_long_shrunk_body_bytes, 200);
        assert_eq!(state.prompt_too_long_dropped_items, 3);
    }

    #[test]
    fn provider_request_observer_merges_stream_usage_metadata() {
        let state = Arc::new(StdMutex::new(RequestObserverState::default()));
        let observer = provider_request_observer(state.clone());

        observer(ProviderRequestObserverEvent {
            event: ProviderRequestObserverEventKind::StreamMetadata,
            stream_metadata: Some(claude_proxy_providers::provider::ProviderStreamMetadata {
                usage: Some(ProviderUsageMetadata {
                    input_tokens: 42,
                    output_tokens: 9,
                    cache_creation_input_tokens: 3,
                    cache_read_input_tokens: 5,
                }),
                model: Some("gpt-4".to_string()),
                request_id: Some("chatcmpl-1".to_string()),
                stop_reason: Some("stop".to_string()),
            }),
            ..ProviderRequestObserverEvent::default()
        });

        let mut usage = TokenUsage {
            input_tokens: 1,
            output_tokens: 2,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };
        merge_observer_usage(&state, &mut usage);

        assert_eq!(usage.input_tokens, 42);
        assert_eq!(usage.output_tokens, 9);
        assert_eq!(usage.cache_creation_input_tokens, 3);
        assert_eq!(usage.cache_read_input_tokens, 5);
    }

    #[test]
    fn usage_extraction_keeps_final_token_snapshot() {
        let mut usage = TokenUsage::default();
        extract_usage_from_event(
            &json!({
                "type": "message_start",
                "message": {
                    "usage": {
                        "input_tokens": 12,
                        "output_tokens": 0
                    }
                }
            }),
            &mut usage,
        );
        extract_usage_from_event(
            &json!({
                "type": "message_delta",
                "usage": {
                    "input_tokens": 12,
                    "output_tokens": 7
                }
            }),
            &mut usage,
        );
        extract_usage_from_event(
            &json!({
                "type": "message_delta",
                "usage": {
                    "input_tokens": 12,
                    "output_tokens": 7
                }
            }),
            &mut usage,
        );

        assert_eq!(usage.input_tokens, 12);
        assert_eq!(usage.output_tokens, 7);
    }

    #[test]
    fn usage_extraction_keeps_cache_token_snapshot() {
        let mut usage = TokenUsage::default();
        extract_usage_from_event(
            &json!({
                "type": "message_start",
                "message": {
                    "usage": {
                        "input_tokens": 30,
                        "cache_creation_input_tokens": 5,
                        "cache_read_input_tokens": 10
                    }
                }
            }),
            &mut usage,
        );
        extract_usage_from_event(
            &json!({
                "type": "message_delta",
                "usage": {
                    "input_tokens": 30,
                    "output_tokens": 4,
                    "cache_creation_input_tokens": 5,
                    "cache_read_input_tokens": 10
                }
            }),
            &mut usage,
        );

        assert_eq!(usage.input_tokens, 30);
        assert_eq!(usage.output_tokens, 4);
        assert_eq!(usage.cache_creation_input_tokens, 5);
        assert_eq!(usage.cache_read_input_tokens, 10);
    }

    #[test]
    fn request_features_detects_used_capabilities() {
        let mut request = request_with_system(Some(SystemPrompt::Text("system".to_string())));
        request.stream = true;
        request.temperature = Some(0.2);
        request.stop_sequences = Some(vec!["stop".to_string()]);
        request.thinking = Some(ThinkingConfig {
            r#type: Some("enabled".to_string()),
            budget_tokens: Some(1024),
        });
        request.tools = Some(vec![Tool {
            name: "read".to_string(),
            description: None,
            input_schema: json!({"type": "object"}),
        }]);
        request.tool_choice = Some(json!({"type": "auto"}));
        request.messages = vec![Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![Content::Unknown(json!({
                "type": "image",
                "source": {"type": "base64", "media_type": "image/png", "data": "..."}
            }))]),
        }];

        let features = RequestFeatures::from_request(&request);

        assert!(features.streaming);
        assert!(features.system_prompt);
        assert!(features.tools);
        assert!(features.tool_choice);
        assert!(features.thinking);
        assert!(features.vision);
        assert!(features.sampling);
        assert!(features.stop_sequences);
    }

    #[test]
    fn capability_validation_rejects_explicitly_unsupported_features() {
        let capabilities = ModelCapabilities {
            features: FeatureCapabilities {
                tools: CapabilityState::Unsupported,
                ..Default::default()
            },
            ..Default::default()
        };
        let features = RequestFeatures {
            tools: true,
            ..Default::default()
        };

        let error = validate_request_capabilities("openai", "gpt-x", &capabilities, &features)
            .expect_err("unsupported tools should fail");

        assert_eq!(error.error.error_type, "invalid_request_error");
        assert!(error.error.message.contains("does not support tools"));
    }

    #[test]
    fn capability_validation_allows_unknown_features() {
        let capabilities = ModelCapabilities::default();
        let features = RequestFeatures {
            tools: true,
            thinking: true,
            vision: true,
            ..Default::default()
        };

        validate_request_capabilities("custom", "model", &capabilities, &features).unwrap();
    }

    #[tokio::test]
    async fn admin_refresh_models_requires_admin_auth() {
        let mut settings = settings_with_provider(ProviderType::OpenAI);
        settings.admin.auth_token = Some("admin-secret".to_string());
        let state = AppState::new(settings, None);

        let response = admin_refresh_models(State(state), HeaderMap::new()).await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admin_metrics_includes_model_capabilities() {
        let settings = settings_with_provider(ProviderType::ChatGPT);
        let state = AppState::new(settings, None);
        state
            .provider_registry
            .write()
            .await
            .cache_models("test", vec![model_capability_fixture("gpt-5.5")]);
        state
            .record_provider_error("chatgpt", "token refresh failed")
            .await;

        let response = admin_metrics(State(state), HeaderMap::new()).await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            body["model_capabilities"]["test/gpt-5.5"]["capabilities"]["limits"]["max_output_tokens"],
            128_000
        );
        assert_eq!(
            body["model_capabilities"]["test/gpt-5.5"]["capabilities"]["limits"]["context_window"],
            400_000
        );
        assert_eq!(
            body["model_capabilities"]["test/gpt-5.5"]["capabilities"]["endpoints"]["openai_responses"],
            "supported"
        );
        assert_eq!(body["model_cache"][0]["provider"], "test");
        assert_eq!(body["model_cache"][0]["cached"], true);
        assert_eq!(body["model_cache"][0]["model_count"], 1);
        assert_eq!(body["model_cache"][0]["fresh"], true);
        assert!(body["model_cache"][0]["ttl_secs"].as_u64().unwrap() > 0);
        assert!(body["model_cache"][0]["age_secs"].as_u64().is_some());
        assert!(body["model_cache"][0]["expires_in_secs"].as_u64().is_some());
        assert_eq!(body["provider_health"]["chatgpt"]["status"], "unhealthy");
        assert_eq!(
            body["provider_health"]["chatgpt"]["last_error"],
            "token refresh failed"
        );
    }
}
