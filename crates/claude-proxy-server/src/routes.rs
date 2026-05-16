use std::convert::Infallible;
use std::hash::Hasher;
use std::io;
use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use claude_proxy_config::settings::ProviderType;
use claude_proxy_core::*;
use claude_proxy_providers::provider::{Provider, ProviderError};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use std::collections::hash_map::DefaultHasher;
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;
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

    info!(
        "Request: initiator={} model={} → {}/{}",
        resolved.initiator, request.model, resolved.provider_id, resolved.upstream_model
    );

    // --- Concurrent request deduplication ---
    // Compute a hash of the full request to identify identical inflight requests.
    let request_hash = compute_request_hash(&resolved.request);

    // Check if an identical request is already in flight
    if let Some(receiver) = subscribe_inflight_request(&state, request_hash).await {
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

    // Register this request as inflight (we are the "leader")
    let (broadcast_tx, _) = tokio::sync::broadcast::channel::<InflightEvent>(256);
    {
        let mut inflight = state.inflight.lock().await;
        inflight.insert(request_hash, broadcast_tx.clone());
    }

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

async fn resolve_upstream_request(
    state: &AppState,
    request: &MessagesRequest,
) -> ResolvedUpstreamRequest {
    let settings = state.settings.read().await;
    let model_ref = settings.resolve_model(&request.model).to_string();
    let provider_id = claude_proxy_config::Settings::parse_provider_id(&model_ref).to_string();
    let upstream_model = claude_proxy_config::Settings::parse_model_name(&model_ref).to_string();
    let initiator = resolve_request_initiator(&settings, &provider_id, request);

    let mut request = request.clone();
    request.model = upstream_model.clone();

    ResolvedUpstreamRequest {
        provider_id,
        upstream_model,
        initiator,
        request,
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
    let mut registry = state.provider_registry.write().await;
    let settings = state.settings.read().await;
    registry.get_or_create(provider_id, &settings).await
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
    match tokio::time::timeout(
        Duration::from_secs(10),
        state.concurrency_semaphore.clone().acquire_owned(),
    )
    .await
    {
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

async fn subscribe_inflight_request(
    state: &AppState,
    request_hash: u64,
) -> Option<tokio::sync::broadcast::Receiver<InflightEvent>> {
    let inflight = state.inflight.lock().await;
    let sender = inflight.get(&request_hash)?;
    let receiver = sender.subscribe();
    drop(inflight);

    debug!("Dedup: joining existing inflight request (hash={request_hash:016x})");
    Some(receiver)
}

fn join_inflight_stream(
    mut receiver: tokio::sync::broadcast::Receiver<InflightEvent>,
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
                Ok(InflightEvent::Done) | Err(_) => break,
                Ok(InflightEvent::Error(msg)) => {
                    let error_event = SseEvent {
                        event: "error".to_string(),
                        data: json!({"error": {"type": "api_error", "message": msg}}),
                    };
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
    mut receiver: tokio::sync::broadcast::Receiver<InflightEvent>,
    _request_permit: OwnedSemaphorePermit,
) -> Response {
    let mut last_event = None;
    loop {
        match receiver.recv().await {
            Ok(InflightEvent::Event(event)) => last_event = Some(event),
            Ok(InflightEvent::Done) | Err(_) => break,
            Ok(InflightEvent::Error(msg)) => {
                return error_response(StatusCode::BAD_GATEWAY, &ErrorResponse::api_error(&msg));
            }
        }
    }

    let response_data = last_event
        .map(|e| e.data)
        .unwrap_or(json!({"error": "no response from provider"}));
    Json(response_data).into_response()
}

async fn stream_leader_response(
    state: &AppState,
    request: &RequestMetricsContext,
    request_hash: u64,
    broadcast_tx: tokio::sync::broadcast::Sender<InflightEvent>,
    mut stream: BoxStream<'static, Result<SseEvent, ProviderError>>,
    permits: StreamPermits,
    start: std::time::Instant,
) -> Response {
    let (sender, body) = tokio::sync::mpsc::channel::<Result<Vec<u8>, Infallible>>(64);

    let metrics = state.metrics.clone();
    let provider_id = request.provider_id.clone();
    let initiator = request.initiator;
    let model_name = request.model.clone();
    let inflight_map = state.inflight.clone();
    tokio::spawn(async move {
        let _permits = permits;
        let task_start = std::time::Instant::now();
        let mut usage = TokenUsage::default();
        let mut had_error = false;
        while let Some(event_result) = stream.next().await {
            match event_result {
                Ok(event) => {
                    extract_usage_from_event(&event.data, &mut usage);
                    let sse_text = format_sse_event(&event);
                    let _ = broadcast_tx.send(InflightEvent::Event(event));
                    if sender.send(Ok(sse_text.into_bytes())).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    had_error = true;
                    let _ = broadcast_tx.send(InflightEvent::Error(e.to_string()));
                    let error_event = SseEvent {
                        event: "error".to_string(),
                        data: json!({"error": {"type": "api_error", "message": e.to_string()}}),
                    };
                    let sse_text = format_sse_event(&error_event);
                    let _ = sender.send(Ok(sse_text.into_bytes())).await;
                    break;
                }
            }
        }
        let _ = broadcast_tx.send(InflightEvent::Done);
        inflight_map.lock().await.remove(&request_hash);
        let latency_ms = task_start.elapsed().as_millis() as u64;
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
    broadcast_tx: tokio::sync::broadcast::Sender<InflightEvent>,
    mut stream: BoxStream<'static, Result<SseEvent, ProviderError>>,
    _permits: StreamPermits,
    start: std::time::Instant,
) -> Response {
    let mut last_event = None;
    let mut usage = TokenUsage::default();
    while let Some(event_result) = stream.next().await {
        match event_result {
            Ok(event) => {
                extract_usage_from_event(&event.data, &mut usage);
                let _ = broadcast_tx.send(InflightEvent::Event(event.clone()));
                last_event = Some(event);
            }
            Err(e) => {
                let _ = broadcast_tx.send(InflightEvent::Error(e.to_string()));
                let _ = broadcast_tx.send(InflightEvent::Done);
                state.inflight.lock().await.remove(&request_hash);
                return error_response(
                    StatusCode::BAD_GATEWAY,
                    &ErrorResponse::api_error(&e.to_string()),
                );
            }
        }
    }
    let _ = broadcast_tx.send(InflightEvent::Done);
    state.inflight.lock().await.remove(&request_hash);

    let response_data = last_event
        .map(|e| e.data)
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

    state.metrics.record_latency(latency_ms);
    Json(response_data).into_response()
}

async fn handle_provider_error(
    state: &AppState,
    request: &RequestMetricsContext,
    request_hash: u64,
    broadcast_tx: tokio::sync::broadcast::Sender<InflightEvent>,
    error: ProviderError,
    start: std::time::Instant,
) -> Response {
    let _ = broadcast_tx.send(InflightEvent::Error(error.to_string()));
    let _ = broadcast_tx.send(InflightEvent::Done);
    state.inflight.lock().await.remove(&request_hash);

    error!("Provider error: {error}");
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

            let mut settings = state.settings.write().await;
            *settings = new_settings.clone();
            let mut registry = state.provider_registry.write().await;
            registry.clear();
            state.provider_concurrency_semaphores.lock().await.clear();

            if let Some(path) = claude_proxy_config::Settings::config_file_path()
                && let Err(e) = std::fs::write(&path, new_settings.to_toml())
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
                let mut settings = state.settings.write().await;
                *settings = new_settings;
                let mut registry = state.provider_registry.write().await;
                registry.clear();
                state.provider_concurrency_semaphores.lock().await.clear();
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
    Json(state.metrics.to_json().await).into_response()
}

fn format_sse_event(event: &SseEvent) -> String {
    let data_str = serde_json::to_string(&event.data).unwrap_or_default();
    if event.event.is_empty() {
        format!("data: {data_str}\n\n")
    } else {
        format!("event: {}\ndata: {data_str}\n\n", event.event)
    }
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
        } => {
            let mut response = error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &ErrorResponse::api_error(message),
            );
            if let Some(secs) = retry_after
                && let Ok(header_value) = axum::http::HeaderValue::from_str(&secs.to_string())
            {
                response.headers_mut().insert("retry-after", header_value);
            }
            response
        }
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
        408 => error_response(
            StatusCode::GATEWAY_TIMEOUT,
            &ErrorResponse::timeout(&message),
        ),
        503 | 529 => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            &ErrorResponse::api_error(&message),
        ),
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
/// In streaming, usage comes in:
/// - `message_start` event: `message.usage.input_tokens`
/// - `message_delta` event: `usage.output_tokens`
fn extract_usage_from_event(data: &Value, usage: &mut TokenUsage) {
    // message_start: { "message": { "usage": { "input_tokens": N, ... } } }
    if let Some(message) = data.get("message")
        && let Some(u) = message.get("usage")
    {
        if let Some(v) = u.get("input_tokens").and_then(|v| v.as_u64()) {
            usage.input_tokens += v;
        }
        if let Some(v) = u
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
        {
            usage.cache_creation_input_tokens += v;
        }
        if let Some(v) = u.get("cache_read_input_tokens").and_then(|v| v.as_u64()) {
            usage.cache_read_input_tokens += v;
        }
    }

    // message_delta: { "usage": { "output_tokens": N } }
    if let Some(u) = data.get("usage") {
        if let Some(v) = u.get("output_tokens").and_then(|v| v.as_u64()) {
            usage.output_tokens += v;
        }
        // Also check for input tokens in delta (some providers include them)
        if let Some(v) = u.get("input_tokens").and_then(|v| v.as_u64()) {
            usage.input_tokens += v;
        }
        // Cache tokens at top level (Anthropic non-streaming response format)
        if let Some(v) = u
            .get("cache_creation_input_tokens")
            .and_then(|v| v.as_u64())
        {
            usage.cache_creation_input_tokens += v;
        }
        if let Some(v) = u.get("cache_read_input_tokens").and_then(|v| v.as_u64()) {
            usage.cache_read_input_tokens += v;
        }
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

    use claude_proxy_config::settings::{
        AdminConfig, HttpConfig, LimitsConfig, LogConfig, ModelConfig, ProviderConfig,
        ProviderType, ServerConfig,
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
            },
        );

        claude_proxy_config::Settings {
            providers,
            model: ModelConfig {
                default: "test/model".to_string(),
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
}
