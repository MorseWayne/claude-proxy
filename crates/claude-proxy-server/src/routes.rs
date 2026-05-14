use std::convert::Infallible;

use axum::Json;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use claude_proxy_core::*;
use claude_proxy_providers::provider::ProviderError;
use futures::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;
use tracing::{error, info, warn};

use crate::app::{AppState, TokenUsage};

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
            state.metrics.record_error();
            state
                .metrics
                .record_latency(start.elapsed().as_millis() as u64);
            return error_response(
                StatusCode::UNAUTHORIZED,
                &ErrorResponse::authentication("invalid API key"),
            );
        }
    }

    // Concurrency limiting
    let _permit = match tokio::time::timeout(
        Duration::from_secs(10),
        state.concurrency_semaphore.acquire(),
    )
    .await
    {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => {
            error!("Semaphore closed unexpectedly");
            state.metrics.record_error();
            state
                .metrics
                .record_latency(start.elapsed().as_millis() as u64);
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &ErrorResponse::api_error("service unavailable"),
            );
        }
        Err(_) => {
            warn!("Concurrency limit reached, request timed out");
            state.metrics.record_error();
            state
                .metrics
                .record_latency(start.elapsed().as_millis() as u64);
            return error_response(
                StatusCode::SERVICE_UNAVAILABLE,
                &ErrorResponse::api_error("too many concurrent requests"),
            );
        }
    };

    let (_model_ref, provider_id, upstream_model) = {
        let settings = state.settings.read().await;
        let model_ref = settings.resolve_model(&request.model).to_string();
        let provider_id = claude_proxy_config::Settings::parse_provider_id(&model_ref).to_string();
        let upstream_model =
            claude_proxy_config::Settings::parse_model_name(&model_ref).to_string();
        (model_ref, provider_id, upstream_model)
    };

    info!(
        "Request: model={} → {}/{}",
        request.model, provider_id, upstream_model
    );

    // Build upstream request with resolved model name
    let mut upstream_request = request.clone();
    upstream_request.model = upstream_model;

    // Get or create provider
    let mut registry = state.provider_registry.write().await;
    let settings = state.settings.read().await;
    let provider = match registry.get_or_create(&provider_id, &settings).await {
        Ok(p) => p,
        Err(e) => {
            error!("Provider error: {e}");
            state.metrics.record_error();
            state
                .metrics
                .record_latency(start.elapsed().as_millis() as u64);
            return error_response(
                StatusCode::NOT_FOUND,
                &ErrorResponse::not_found(&format!("provider not available: {e}")),
            );
        }
    };

    // Call provider
    match provider.chat(upstream_request).await {
        Ok(mut stream) => {
            if request.stream {
                // Streaming SSE response
                let (sender, body) = tokio::sync::mpsc::channel::<Result<Vec<u8>, Infallible>>(64);

                let metrics = state.metrics.clone();
                let model_name = request.model.clone();
                tokio::spawn(async move {
                    let task_start = std::time::Instant::now();
                    let mut usage = TokenUsage::default();
                    let mut had_error = false;
                    while let Some(event_result) = stream.next().await {
                        match event_result {
                            Ok(event) => {
                                // Extract token usage from streaming events
                                extract_usage_from_event(&event.data, &mut usage);
                                let sse_text = format_sse_event(&event);
                                if sender.send(Ok(sse_text.into_bytes())).await.is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                had_error = true;
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
                    // Record token usage after stream completes (persisted)
                    let latency_ms = task_start.elapsed().as_millis() as u64;
                    metrics
                        .record_completed_request(&model_name, &usage, had_error, latency_ms)
                        .await;
                });

                state
                    .metrics
                    .record_latency(start.elapsed().as_millis() as u64);
                let stream_body = tokio_stream::wrappers::ReceiverStream::new(body);
                Response::builder()
                    .status(200)
                    .header("content-type", "text/event-stream")
                    .header("cache-control", "no-cache")
                    .header("connection", "keep-alive")
                    .body(Body::from_stream(stream_body))
                    .unwrap()
            } else {
                // Non-streaming: collect all events and return final message
                let mut events = Vec::new();
                while let Some(event_result) = stream.next().await {
                    match event_result {
                        Ok(event) => events.push(event),
                        Err(e) => {
                            return error_response(
                                StatusCode::BAD_GATEWAY,
                                &ErrorResponse::api_error(&e.to_string()),
                            );
                        }
                    }
                }
                let response_data = events
                    .last()
                    .map(|e| e.data.clone())
                    .unwrap_or(json!({"error": "no response from provider"}));

                // Extract token usage from all events (message_start has input_tokens,
                // message_delta has output_tokens; message_stop has none)
                let mut usage = TokenUsage::default();
                for event in &events {
                    extract_usage_from_event(&event.data, &mut usage);
                }
                let latency_ms = start.elapsed().as_millis() as u64;
                state
                    .metrics
                    .record_completed_request(&request.model, &usage, false, latency_ms)
                    .await;

                state.metrics.record_latency(latency_ms);
                Json(response_data).into_response()
            }
        }
        Err(e) => {
            error!("Provider error: {e}");
            state.metrics.record_error();
            let latency_ms = start.elapsed().as_millis() as u64;
            state.metrics.record_latency(latency_ms);
            state
                .metrics
                .record_completed_request(&request.model, &TokenUsage::default(), true, latency_ms)
                .await;
            provider_error_to_response(&e)
        }
    }
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
        ProviderError::ModelNotFound(msg) => {
            error_response(StatusCode::NOT_FOUND, &ErrorResponse::not_found(msg))
        }
        ProviderError::Timeout => error_response(
            StatusCode::GATEWAY_TIMEOUT,
            &ErrorResponse::timeout("upstream request timed out"),
        ),
        ProviderError::UpstreamError { status, body } => {
            error!("Upstream error (HTTP {status}): {body}");
            error_response(
                StatusCode::BAD_GATEWAY,
                &ErrorResponse::api_error("upstream unavailable"),
            )
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
