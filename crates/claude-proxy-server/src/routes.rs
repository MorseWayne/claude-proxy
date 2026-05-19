use std::convert::Infallible;
use std::hash::Hasher;
use std::io;
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use claude_proxy_config::settings::{ModelReasoningEffort, ProviderType};
use claude_proxy_core::*;
use claude_proxy_providers::openai_request_log_info;
use claude_proxy_providers::provider::{Provider, ProviderError};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, broadcast};
use tracing::{debug, error, info, warn};

use crate::app::{AppState, InflightEvent, TokenUsage};

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

    let metrics_context = RequestMetricsContext {
        provider_id: resolved.provider_id.clone(),
        model: resolved.request.model.clone(),
        initiator: resolved.initiator,
    };

    // Call provider (registry lock is no longer held)
    match provider.chat(resolved.request).await {
        Ok(stream) => {
            if request.stream {
                stream_leader_response(
                    &state,
                    &metrics_context,
                    request_hash,
                    broadcast_tx,
                    stream,
                    StreamPermits {
                        _request: request_permit,
                        _provider: Some(provider_permit),
                    },
                    start,
                )
                .await
            } else {
                collect_leader_response(
                    &state,
                    &metrics_context,
                    request_hash,
                    broadcast_tx,
                    stream,
                    StreamPermits {
                        _request: request_permit,
                        _provider: Some(provider_permit),
                    },
                    start,
                )
                .await
            }
        }
        Err(e) => {
            handle_provider_error(
                &state,
                &metrics_context,
                request_hash,
                broadcast_tx,
                e,
                start,
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

#[derive(Clone)]
struct RequestMetricsContext {
    provider_id: String,
    model: String,
    initiator: &'static str,
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
    let provider_type = settings
        .providers
        .get(&provider_id)
        .map(|config| config.resolve_type(&provider_id))
        .unwrap_or_else(|| ProviderType::parse(&provider_id));
    let initiator = resolve_request_initiator(&settings, &provider_id, request);

    let mut request = request.clone();
    request.model = upstream_model.clone();
    apply_alias_reasoning_effort(&mut request, resolved_model.reasoning_effort);

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
) -> Response {
    let (tx, body) = tokio::sync::mpsc::channel::<Result<Vec<u8>, Infallible>>(64);
    tokio::spawn(async move {
        let _permits = permits;
        loop {
            match receiver.recv().await {
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
    request_hash: u64,
    broadcast_tx: broadcast::Sender<InflightEvent>,
    mut stream: BoxStream<'static, Result<SseEvent, ProviderError>>,
    permits: StreamPermits,
    start: std::time::Instant,
) -> Response {
    let (sender, body) = tokio::sync::mpsc::channel::<Result<Vec<u8>, Infallible>>(64);

    let metrics = state.metrics.clone();
    let health_state = state.clone();
    let provider_id = request.provider_id.clone();
    let initiator = request.initiator;
    let model_name = request.model.clone();
    let inflight_map = state.inflight.clone();
    tokio::spawn(async move {
        let _permits = permits;
        let task_start = std::time::Instant::now();
        let mut usage = TokenUsage::default();
        let mut had_error = false;
        let mut last_error = None;
        let mut leader_tx_open = true;
        while let Some(event_result) = stream.next().await {
            match event_result {
                Ok(event) => {
                    extract_usage_from_event(&event.data, &mut usage);
                    let sse_text = leader_tx_open.then(|| format_sse_event(&event));
                    let _ = broadcast_tx.send(InflightEvent::Event(event));
                    if let Some(sse_text) = sse_text
                        && sender.send(Ok(sse_text.into_bytes())).await.is_err()
                    {
                        leader_tx_open = false;
                    }
                    if !leader_tx_open && broadcast_tx.receiver_count() == 0 {
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
        let _ = broadcast_tx.send(InflightEvent::Done);
        inflight_map.lock().await.remove(&request_hash);
        let latency_ms = task_start.elapsed().as_millis() as u64;
        if let Some(error) = last_error {
            health_state
                .record_provider_error(&provider_id, &error)
                .await;
        } else {
            health_state.record_provider_success(&provider_id).await;
        }
        metrics
            .record_completed_request(
                &provider_id,
                initiator,
                &model_name,
                &usage,
                had_error,
                latency_ms,
            )
            .await;
    });

    state
        .metrics
        .record_latency(start.elapsed().as_millis() as u64);
    sse_body_response(body)
}

async fn collect_leader_response(
    state: &AppState,
    request: &RequestMetricsContext,
    request_hash: u64,
    broadcast_tx: broadcast::Sender<InflightEvent>,
    mut stream: BoxStream<'static, Result<SseEvent, ProviderError>>,
    _permits: StreamPermits,
    start: std::time::Instant,
) -> Response {
    let mut events = Vec::new();
    let mut usage = TokenUsage::default();
    while let Some(event_result) = stream.next().await {
        match event_result {
            Ok(event) => {
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
                    .record_provider_error(&request.provider_id, &error_message)
                    .await;
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
    state
        .metrics
        .record_completed_request(
            &request.provider_id,
            request.initiator,
            &request.model,
            &usage,
            false,
            latency_ms,
        )
        .await;

    state.record_provider_success(&request.provider_id).await;
    state.metrics.record_latency(latency_ms);
    Json(response_data).into_response()
}

async fn handle_provider_error(
    state: &AppState,
    request: &RequestMetricsContext,
    request_hash: u64,
    broadcast_tx: broadcast::Sender<InflightEvent>,
    error: ProviderError,
    start: std::time::Instant,
) -> Response {
    let _ = broadcast_tx.send(InflightEvent::Error(error.to_string()));
    let _ = broadcast_tx.send(InflightEvent::Done);
    state.inflight.lock().await.remove(&request_hash);

    error!("Provider error: {error}");
    state
        .record_provider_error(&request.provider_id, &error.to_string())
        .await;
    state.metrics.record_error();
    let latency_ms = start.elapsed().as_millis() as u64;
    state.metrics.record_latency(latency_ms);
    state
        .metrics
        .record_completed_request(
            &request.provider_id,
            request.initiator,
            &request.model,
            &TokenUsage::default(),
            true,
            latency_ms,
        )
        .await;
    provider_error_to_response(&error)
}

fn sse_body_response(body: tokio::sync::mpsc::Receiver<Result<Vec<u8>, Infallible>>) -> Response {
    let stream_body = tokio_stream::wrappers::ReceiverStream::new(body);
    Response::builder()
        .status(200)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("connection", "keep-alive")
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
                "supports_thinking": m.supports_thinking,
                "vendor": m.vendor,
                "max_output_tokens": m.max_output_tokens,
                "context_window": m.context_window,
                "supported_endpoints": m.supported_endpoints,
                "is_chat_default": m.is_chat_default,
                "supports_vision": m.supports_vision,
                "supports_adaptive_thinking": m.supports_adaptive_thinking,
                "min_thinking_budget": m.min_thinking_budget,
                "max_thinking_budget": m.max_thinking_budget,
                "reasoning_effort_levels": m.reasoning_effort_levels,
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

    let model_capabilities = state.provider_registry.read().await.model_capabilities();
    let provider_rate_limits = provider_rate_limit_snapshots(&state, rate_limit_provider_ids).await;
    let provider_health = state.provider_health_snapshot(provider_ids).await;
    let mut metrics = state.metrics.to_json().await;
    if let Some(object) = metrics.as_object_mut() {
        object.insert("model_capabilities".to_string(), model_capabilities);
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

fn provider_error_to_response(error: &ProviderError) -> Response {
    match error {
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
    }
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
        ProviderConfig, ProviderType, ServerConfig,
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
            },
            admin: AdminConfig { auth_token: None },
            limits: LimitsConfig::default(),
            http: HttpConfig::default(),
            log: LogConfig::default(),
        }
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
    fn request_intent_reads_metadata_intent() {
        let mut request = request_with_system(None);
        request.metadata = Some(json!({"intent": "deep_think"}));

        assert_eq!(request_intent(&request), Some("deep_think"));
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
            0xdead_beef,
            broadcast_tx,
            tokio_stream::wrappers::ReceiverStream::new(event_rx).boxed(),
            StreamPermits {
                _request: request_permit,
                _provider: None,
            },
            std::time::Instant::now(),
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

    #[tokio::test]
    async fn admin_metrics_includes_model_capabilities() {
        let settings = settings_with_provider(ProviderType::ChatGPT);
        let state = AppState::new(settings, None);
        state.provider_registry.write().await.cache_models(
            "chatgpt",
            vec![ModelInfo {
                model_id: "gpt-5.5".to_string(),
                supports_thinking: Some(true),
                vendor: Some("openai".to_string()),
                max_output_tokens: Some(128_000),
                context_window: Some(400_000),
                supported_endpoints: vec!["/responses".to_string()],
                is_chat_default: None,
                supports_vision: None,
                supports_adaptive_thinking: None,
                min_thinking_budget: None,
                max_thinking_budget: None,
                reasoning_effort_levels: vec!["low".to_string(), "high".to_string()],
            }],
        );
        state
            .record_provider_error("chatgpt", "token refresh failed")
            .await;

        let response = admin_metrics(State(state), HeaderMap::new()).await;
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();

        assert_eq!(
            body["model_capabilities"]["chatgpt/gpt-5.5"]["max_output_tokens"],
            128_000
        );
        assert_eq!(
            body["model_capabilities"]["chatgpt/gpt-5.5"]["context_window"],
            400_000
        );
        assert_eq!(
            body["model_capabilities"]["chatgpt/gpt-5.5"]["supported_endpoints"][0],
            "/responses"
        );
        assert_eq!(body["provider_health"]["chatgpt"]["status"], "unhealthy");
        assert_eq!(
            body["provider_health"]["chatgpt"]["last_error"],
            "token refresh failed"
        );
    }
}
