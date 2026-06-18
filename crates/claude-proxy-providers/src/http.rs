//! Shared HTTP client helpers: extra CA cert loading and error-chain
//! formatting.
//!
//! This crate is built with reqwest's `rustls-tls` feature, which means TLS
//! validation uses webpki-roots' embedded Mozilla CA bundle and **never**
//! consults the system trust store. That works fine on the open Internet but
//! breaks in corporate environments that perform TLS interception (Fortinet,
//! Zscaler, Bluecoat, ...): `curl` succeeds because the corporate root is in
//! `/etc/ssl/certs`, but our binary fails with the unhelpful `error sending
//! request for url (...)`.
//!
//! The fix is to let the user point us at one or more PEM files via the
//! `http.extra_ca_certs` setting; we install each cert as an additional root
//! on every reqwest client we build.

use std::future::Future;
use std::path::Path;
use std::time::{Duration, SystemTime};

use crate::provider::{ProviderError, UpstreamErrorHeader, UpstreamErrorMetadata};
use chrono::DateTime;
use claude_proxy_config::settings::{
    ProviderRequestConfig, ProviderRetryConfig, ProviderRuntimeConfig,
};
use futures::StreamExt;
use reqwest::{
    RequestBuilder, StatusCode,
    header::{HeaderName, HeaderValue},
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::time::{sleep, timeout};
use tracing::warn;

const MAX_UPSTREAM_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_UPSTREAM_ERROR_PREVIEW_BYTES: usize = 2048;
const MAX_SAFE_UPSTREAM_HEADER_VALUE_BYTES: usize = 512;
const MAX_SEND_ATTEMPTS: usize = 3;
const BASE_RETRY_DELAY: Duration = Duration::from_millis(200);
const MAX_RETRY_AFTER_DELAY: Duration = Duration::from_secs(5);
const UPSTREAM_STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
const UPSTREAM_ERROR_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(5);
const UPSTREAM_SUCCESS_BODY_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy)]
pub struct UpstreamRequestPolicy {
    pub max_attempts: usize,
    pub attempt_timeout: Option<Duration>,
    pub base_retry_delay: Duration,
    pub retry_network_errors: bool,
    pub retry_timeout_errors: bool,
    pub retry_rate_limits: bool,
    pub retry_transient_statuses: bool,
}

impl Default for UpstreamRequestPolicy {
    fn default() -> Self {
        Self {
            max_attempts: MAX_SEND_ATTEMPTS,
            attempt_timeout: None,
            base_retry_delay: BASE_RETRY_DELAY,
            retry_network_errors: true,
            retry_timeout_errors: true,
            retry_rate_limits: true,
            retry_transient_statuses: true,
        }
    }
}

impl UpstreamRequestPolicy {
    pub fn from_runtime_config(config: &ProviderRuntimeConfig) -> Self {
        Self::default().with_runtime_config(config)
    }

    pub fn with_runtime_config(mut self, config: &ProviderRuntimeConfig) -> Self {
        self.apply_retry_config(&config.retry);
        self.apply_request_config(&config.request);
        self
    }

    fn apply_retry_config(&mut self, retry: &ProviderRetryConfig) {
        if let Some(max_attempts) = retry.max_attempts {
            self.max_attempts = max_attempts;
        }
        if let Some(base_delay_ms) = retry.base_delay_ms {
            self.base_retry_delay = Duration::from_millis(base_delay_ms);
        }
        if let Some(retry_network_errors) = retry.network_errors {
            self.retry_network_errors = retry_network_errors;
        }
        if let Some(retry_timeout_errors) = retry.timeout_errors {
            self.retry_timeout_errors = retry_timeout_errors;
        }
        if let Some(retry_rate_limits) = retry.rate_limits {
            self.retry_rate_limits = retry_rate_limits;
        }
        if let Some(retry_transient_statuses) = retry.transient_statuses {
            self.retry_transient_statuses = retry_transient_statuses;
        }
    }

    fn apply_request_config(&mut self, request: &ProviderRequestConfig) {
        if let Some(attempt_timeout_seconds) = request.attempt_timeout_seconds {
            self.attempt_timeout = Some(Duration::from_secs(attempt_timeout_seconds));
        }
    }

    fn max_attempts(self) -> usize {
        self.max_attempts.max(1)
    }
}

pub fn apply_runtime_request_config(
    mut request: RequestBuilder,
    config: &ProviderRuntimeConfig,
) -> Result<RequestBuilder, ProviderError> {
    for (name, value) in &config.request.extra_headers {
        let header_name = HeaderName::from_bytes(name.as_bytes()).map_err(|error| {
            ProviderError::InvalidRequest(format!(
                "invalid provider runtime header {name}: {error}"
            ))
        })?;
        let header_value = HeaderValue::from_str(value).map_err(|error| {
            ProviderError::InvalidRequest(format!(
                "invalid provider runtime header {name}: {error}"
            ))
        })?;
        request = request.header(header_name, header_value);
    }

    if !config.request.query_params.is_empty() {
        request = request.query(&config.request.query_params);
    }

    Ok(request)
}

/// Walk the `source` chain of an error and produce a `: `-separated string so
/// callers see the real root cause (TLS handshake error, DNS failure, …)
/// instead of just the topmost `reqwest::Error` message. Avoids duplicating
/// the same message twice when the parent error already embeds the source.
pub fn fmt_err_chain(err: &(dyn std::error::Error + 'static)) -> String {
    let mut out = err.to_string();
    let mut current = err.source();
    while let Some(src) = current {
        let msg = src.to_string();
        if !out.contains(&msg) {
            out.push_str(": ");
            out.push_str(&msg);
        }
        current = src.source();
    }
    out
}

/// Convenience wrapper around [`fmt_err_chain`] for `reqwest::Error`, which is
/// what most call sites already have on hand.
pub fn fmt_reqwest_err(err: &reqwest::Error) -> String {
    fmt_err_chain(err)
}

pub async fn next_upstream_stream_item<F, T>(next: F) -> Result<Option<T>, ProviderError>
where
    F: Future<Output = Option<T>>,
{
    next_upstream_stream_item_with_timeout(next, UPSTREAM_STREAM_IDLE_TIMEOUT).await
}

async fn next_upstream_stream_item_with_timeout<F, T>(
    next: F,
    idle_timeout: Duration,
) -> Result<Option<T>, ProviderError>
where
    F: Future<Output = Option<T>>,
{
    timeout(idle_timeout, next)
        .await
        .map_err(|_| ProviderError::Timeout)
}

pub async fn send_upstream_request(
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, ProviderError> {
    send_upstream_request_with_policy(request, UpstreamRequestPolicy::default()).await
}

pub async fn send_upstream_request_with_policy(
    request: reqwest::RequestBuilder,
    policy: UpstreamRequestPolicy,
) -> Result<reqwest::Response, ProviderError> {
    let mut attempt = 0;
    let max_attempts = policy.max_attempts();
    loop {
        attempt += 1;
        let Some(next_request) = request.try_clone() else {
            return send_once(request, policy.attempt_timeout).await;
        };

        let response = send_once(next_request, policy.attempt_timeout).await;
        if attempt >= max_attempts || !should_retry_result(&response, policy) {
            return response;
        }

        warn_retrying_upstream_request(attempt, max_attempts, &response);
        sleep(retry_delay(attempt, response.as_ref().ok(), policy)).await;
    }
}

async fn send_once(
    request: reqwest::RequestBuilder,
    attempt_timeout: Option<Duration>,
) -> Result<reqwest::Response, ProviderError> {
    with_request_timeout(
        async {
            request.send().await.map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout
                } else {
                    ProviderError::Network(fmt_reqwest_err(&e))
                }
            })
        },
        attempt_timeout,
    )
    .await
}

async fn with_request_timeout<F, T>(
    request: F,
    attempt_timeout: Option<Duration>,
) -> Result<T, ProviderError>
where
    F: Future<Output = Result<T, ProviderError>>,
{
    if let Some(attempt_timeout) = attempt_timeout {
        timeout(attempt_timeout, request)
            .await
            .map_err(|_| ProviderError::Timeout)?
    } else {
        request.await
    }
}

pub async fn read_upstream_response_text(
    response: reqwest::Response,
) -> Result<String, ProviderError> {
    read_upstream_response_text_with_timeout(response, UPSTREAM_SUCCESS_BODY_TIMEOUT).await
}

async fn read_upstream_response_text_with_timeout(
    response: reqwest::Response,
    read_timeout: Duration,
) -> Result<String, ProviderError> {
    timeout(read_timeout, response.text())
        .await
        .map_err(|_| ProviderError::Timeout)?
        .map_err(|e| {
            if e.is_timeout() {
                ProviderError::Timeout
            } else {
                ProviderError::Network(fmt_reqwest_err(&e))
            }
        })
}

pub async fn read_upstream_response_json<T>(
    response: reqwest::Response,
    parse_error_context: &str,
) -> Result<T, ProviderError>
where
    T: DeserializeOwned,
{
    let body = read_upstream_response_text(response).await?;
    serde_json::from_str(&body)
        .map_err(|e| ProviderError::Network(format!("{parse_error_context}: {e}")))
}

fn warn_retrying_upstream_request(
    attempt: usize,
    max_attempts: usize,
    response: &Result<reqwest::Response, ProviderError>,
) {
    match response {
        Ok(response) => warn!(
            attempt,
            max_attempts,
            status = response.status().as_u16(),
            "Retrying upstream request after retryable HTTP status"
        ),
        Err(error) => warn!(
            attempt,
            max_attempts,
            error = %error,
            "Retrying upstream request after transient error"
        ),
    }
}

fn should_retry_result(
    response: &Result<reqwest::Response, ProviderError>,
    policy: UpstreamRequestPolicy,
) -> bool {
    match response {
        Ok(response) => should_retry_response(response, policy),
        Err(ProviderError::Timeout) => policy.retry_timeout_errors,
        Err(ProviderError::Network(_)) => policy.retry_network_errors,
        Err(_) => false,
    }
}

fn should_retry_response(response: &reqwest::Response, policy: UpstreamRequestPolicy) -> bool {
    if response.status() == StatusCode::TOO_MANY_REQUESTS {
        return policy.retry_rate_limits && !is_non_retryable_rate_limit_response(response);
    }
    should_retry_status(response.status(), policy)
}

fn should_retry_status(status: StatusCode, policy: UpstreamRequestPolicy) -> bool {
    if status == StatusCode::TOO_MANY_REQUESTS {
        return policy.retry_rate_limits;
    }
    policy.retry_transient_statuses && is_retryable_status(status)
}

fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::CONFLICT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn is_non_retryable_rate_limit_response(response: &reqwest::Response) -> bool {
    header_contains_any(
        response.headers(),
        &[
            "x-openai-error-code",
            "openai-error-code",
            "x-ratelimit-reason",
            "x-rate-limit-reason",
        ],
        &[
            "insufficient_quota",
            "billing",
            "hard_limit",
            "payment_required",
            "quota_exceeded",
        ],
    )
}

fn is_retryable_overload_status(status: u16) -> bool {
    StatusCode::from_u16(status).is_ok_and(|status| {
        status == StatusCode::REQUEST_TIMEOUT
            || status == StatusCode::CONFLICT
            || status.is_server_error()
    })
}

fn retry_delay(
    attempt: usize,
    response: Option<&reqwest::Response>,
    policy: UpstreamRequestPolicy,
) -> Duration {
    response
        .and_then(retry_after_delay)
        .unwrap_or_else(|| policy.base_retry_delay * attempt as u32)
}

fn retry_after_delay_secs(value: &str) -> Option<Duration> {
    retry_after_delay_from_now(value, SystemTime::now())
}

fn retry_after_delay_from_now(value: &str, now: SystemTime) -> Option<Duration> {
    if let Ok(secs) = value.parse::<u64>() {
        return Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER_DELAY));
    }

    let retry_at: SystemTime = DateTime::parse_from_rfc2822(value).ok()?.to_utc().into();
    let delay = retry_at.duration_since(now).unwrap_or(Duration::ZERO);
    Some(delay.min(MAX_RETRY_AFTER_DELAY))
}

fn retry_after_delay(response: &reqwest::Response) -> Option<Duration> {
    retry_after_delay_from_headers(response.headers())
}

fn retry_after_delay_from_headers(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
    headers
        .get("retry-after-ms")
        .and_then(|v| v.to_str().ok())
        .and_then(retry_after_ms_delay)
        .or_else(|| {
            headers
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(retry_after_delay_secs)
        })
}

fn retry_after_ms_delay(value: &str) -> Option<Duration> {
    let ms = value.parse::<u64>().ok()?;
    Some(Duration::from_millis(ms).min(MAX_RETRY_AFTER_DELAY))
}

fn retry_after_seconds_from_headers(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    retry_after_delay_from_headers(headers).map(retry_after_duration_to_seconds)
}

fn retry_after_duration_to_seconds(delay: Duration) -> u64 {
    if delay.is_zero() {
        return 0;
    }
    delay.as_secs() + u64::from(delay.subsec_nanos() > 0)
}

pub async fn map_upstream_response(response: reqwest::Response) -> ProviderError {
    let status = response.status().as_u16();
    let retry_after = retry_after_seconds_from_headers(response.headers());
    let request_id = upstream_request_id_from_headers(response.headers());
    let safe_headers = safe_upstream_error_headers(response.headers());
    let body = read_limited_response_text(response, MAX_UPSTREAM_ERROR_BODY_BYTES).await;
    let message = extract_upstream_error_message(&body);
    let metadata = UpstreamErrorMetadata {
        status,
        retry_after,
        request_id,
        message: Some(message.clone()),
        body_preview: upstream_error_body_preview(&body),
        headers: safe_headers,
    };

    let error = match status {
        400 => ProviderError::InvalidRequest(message),
        401 => ProviderError::Authentication(message),
        404 => ProviderError::ModelNotFound(message),
        413 => ProviderError::RequestTooLarge(message),
        429 => ProviderError::RateLimited { retry_after },
        status if is_retryable_overload_status(status) => ProviderError::Overloaded {
            message,
            retry_after,
        },
        _ => ProviderError::UpstreamError { status, body },
    };
    error.with_upstream_metadata(metadata)
}

pub fn upstream_error_metadata_from_parts(
    status: u16,
    headers: &reqwest::header::HeaderMap,
    body: &str,
    message: String,
) -> UpstreamErrorMetadata {
    UpstreamErrorMetadata {
        status,
        retry_after: retry_after_seconds_from_headers(headers),
        request_id: upstream_request_id_from_headers(headers),
        message: Some(message),
        body_preview: upstream_error_body_preview(body),
        headers: safe_upstream_error_headers(headers),
    }
}

fn upstream_request_id_from_headers(headers: &reqwest::header::HeaderMap) -> Option<String> {
    header_value_from_any(
        headers,
        &[
            "request-id",
            "x-request-id",
            "x-openai-request-id",
            "openai-request-id",
            "x-ms-request-id",
            "cf-ray",
        ],
    )
}

fn safe_upstream_error_headers(headers: &reqwest::header::HeaderMap) -> Vec<UpstreamErrorHeader> {
    [
        "request-id",
        "x-request-id",
        "x-openai-request-id",
        "openai-request-id",
        "x-ms-request-id",
        "cf-ray",
        "retry-after-ms",
        "retry-after",
        "x-openai-error-code",
        "openai-error-code",
        "x-ratelimit-reason",
        "x-rate-limit-reason",
        "x-ratelimit-limit-requests",
        "x-ratelimit-remaining-requests",
        "x-ratelimit-reset-requests",
        "x-ratelimit-limit-tokens",
        "x-ratelimit-remaining-tokens",
        "x-ratelimit-reset-tokens",
    ]
    .into_iter()
    .filter_map(|name| {
        header_value(headers, name).map(|value| UpstreamErrorHeader {
            name: name.to_string(),
            value,
        })
    })
    .collect()
}

fn header_value_from_any(headers: &reqwest::header::HeaderMap, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| header_value(headers, name))
}

fn header_contains_any(
    headers: &reqwest::header::HeaderMap,
    names: &[&str],
    needles: &[&str],
) -> bool {
    names
        .iter()
        .filter_map(|name| header_value(headers, name))
        .any(|value| {
            let value = value.to_ascii_lowercase();
            needles.iter().any(|needle| value.contains(needle))
        })
}

fn header_value(headers: &reqwest::header::HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(truncate_safe_header_value)
}

fn truncate_safe_header_value(value: &str) -> String {
    if value.len() <= MAX_SAFE_UPSTREAM_HEADER_VALUE_BYTES {
        return value.to_string();
    }

    let mut truncated = value
        .chars()
        .take(MAX_SAFE_UPSTREAM_HEADER_VALUE_BYTES)
        .collect::<String>();
    truncated.push_str("...");
    truncated
}

fn upstream_error_body_preview(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    if trimmed.len() <= MAX_UPSTREAM_ERROR_PREVIEW_BYTES {
        return Some(trimmed.to_string());
    }

    let mut preview = trimmed
        .chars()
        .take(MAX_UPSTREAM_ERROR_PREVIEW_BYTES)
        .collect::<String>();
    preview.push_str("...");
    Some(preview)
}

async fn read_limited_response_text(response: reqwest::Response, limit: usize) -> String {
    read_limited_response_text_with_timeout(response, limit, UPSTREAM_ERROR_BODY_IDLE_TIMEOUT).await
}

async fn read_limited_response_text_with_timeout(
    response: reqwest::Response,
    limit: usize,
    idle_timeout: Duration,
) -> String {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    let mut truncated = false;
    let mut timed_out = false;

    loop {
        let next_chunk = match timeout(idle_timeout, stream.next()).await {
            Ok(next_chunk) => next_chunk,
            Err(_) => {
                timed_out = true;
                break;
            }
        };
        let Some(chunk) = next_chunk else {
            break;
        };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => break,
        };
        let remaining = limit.saturating_sub(body.len());
        if remaining == 0 {
            truncated = true;
            break;
        }
        let take = remaining.min(chunk.len());
        body.extend_from_slice(&chunk[..take]);
        if take < chunk.len() {
            truncated = true;
            break;
        }
    }

    let mut text = String::from_utf8_lossy(&body).into_owned();
    if truncated {
        text.push_str("\n[upstream error body truncated]");
    }
    if timed_out {
        text.push_str("\n[upstream error body read timed out]");
    }
    text
}

pub fn extract_upstream_error_message(body: &str) -> String {
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

/// Read every certificate from a PEM file (single cert or bundle) and append
/// them as additional root CAs on the given builder. Returns the updated
/// builder, or a `ProviderError::Network` describing exactly which file
/// failed and why.
pub fn apply_extra_ca_certs(
    mut builder: reqwest::ClientBuilder,
    paths: &[String],
) -> Result<reqwest::ClientBuilder, ProviderError> {
    for path in paths {
        let pem = std::fs::read(Path::new(path)).map_err(|e| {
            ProviderError::Network(format!("failed to read extra CA cert {path}: {e}"))
        })?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| {
            ProviderError::Network(format!(
                "failed to parse extra CA cert {path}: {}",
                fmt_reqwest_err(&e)
            ))
        })?;
        for cert in certs {
            builder = builder.add_root_certificate(cert);
        }
    }
    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_proxy_config::settings::{ProviderRequestConfig, ProviderRetryConfig};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    async fn response_with_stalled_body(body_prefix: &str) -> reqwest::Response {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body_prefix = body_prefix.to_string();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            let headers = format!(
                "HTTP/1.1 500 Internal Server Error\r\ncontent-length: {}\r\nconnection: keep-alive\r\n\r\n",
                body_prefix.len() + 64
            );
            socket.write_all(headers.as_bytes()).await.unwrap();
            socket.write_all(body_prefix.as_bytes()).await.unwrap();
            let _socket = socket;
            std::future::pending::<()>().await;
        });

        reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
    }

    async fn response_with_status_headers_and_body(
        status_line: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> reqwest::Response {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let status_line = status_line.to_string();
        let headers = headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect::<Vec<_>>();
        let body = body.to_string();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            let mut response = format!(
                "HTTP/1.1 {status_line}\r\ncontent-length: {}\r\nconnection: close\r\n",
                body.len()
            );
            for (name, value) in headers {
                response.push_str(&format!("{name}: {value}\r\n"));
            }
            response.push_str("\r\n");
            response.push_str(&body);
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
    }

    #[test]
    fn retryable_statuses_include_transient_failures() {
        assert!(is_retryable_status(StatusCode::REQUEST_TIMEOUT));
        assert!(is_retryable_status(StatusCode::CONFLICT));
        assert!(is_retryable_status(StatusCode::TOO_MANY_REQUESTS));
        assert!(is_retryable_status(StatusCode::INTERNAL_SERVER_ERROR));
        assert!(is_retryable_status(StatusCode::BAD_GATEWAY));
        assert!(is_retryable_status(StatusCode::SERVICE_UNAVAILABLE));
    }

    #[test]
    fn retryable_overload_statuses_match_transient_upstream_failures() {
        for status in [408, 409, 500, 502, 503, 504, 529] {
            assert!(is_retryable_overload_status(status));
        }

        for status in [400, 401, 404, 413, 429] {
            assert!(!is_retryable_overload_status(status));
        }
    }

    #[test]
    fn retryable_statuses_exclude_client_failures() {
        assert!(!is_retryable_status(StatusCode::BAD_REQUEST));
        assert!(!is_retryable_status(StatusCode::UNAUTHORIZED));
        assert!(!is_retryable_status(StatusCode::NOT_FOUND));
        assert!(!is_retryable_status(StatusCode::PAYLOAD_TOO_LARGE));
    }

    #[test]
    fn retry_after_delay_clamps_large_values() {
        assert_eq!(retry_after_delay_secs("1"), Some(Duration::from_secs(1)));
        assert_eq!(retry_after_delay_secs("60"), Some(MAX_RETRY_AFTER_DELAY));
        assert_eq!(retry_after_delay_secs("not-a-number"), None);
        assert_eq!(
            retry_after_ms_delay("1500"),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(retry_after_ms_delay("90000"), Some(MAX_RETRY_AFTER_DELAY));
    }

    #[test]
    fn retry_after_delay_accepts_http_date() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        assert_eq!(
            retry_after_delay_from_now("Tue, 14 Nov 2023 22:13:23 GMT", now),
            Some(Duration::from_secs(3))
        );
        assert_eq!(
            retry_after_delay_from_now("Tue, 14 Nov 2023 22:13:10 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(
            retry_after_delay_from_now("Tue, 14 Nov 2023 22:14:30 GMT", now),
            Some(MAX_RETRY_AFTER_DELAY)
        );
    }

    #[test]
    fn timeout_and_network_errors_are_retryable() {
        let policy = UpstreamRequestPolicy::default();
        assert!(should_retry_result(&Err(ProviderError::Timeout), policy));
        assert!(should_retry_result(
            &Err(ProviderError::Network("connection closed".to_string())),
            policy
        ));
        assert!(!should_retry_result(
            &Err(ProviderError::InvalidRequest("bad request".to_string())),
            policy
        ));
    }

    #[tokio::test]
    async fn retry_after_ms_header_takes_precedence() {
        let response = response_with_status_headers_and_body(
            "429 Too Many Requests",
            &[("retry-after", "4"), ("retry-after-ms", "1500")],
            r#"{"error":{"message":"slow down"}}"#,
        )
        .await;

        assert_eq!(
            retry_after_delay(&response),
            Some(Duration::from_millis(1500))
        );

        let error = map_upstream_response(response).await;
        assert!(matches!(
            error.without_upstream_metadata(),
            ProviderError::RateLimited {
                retry_after: Some(2)
            }
        ));
        assert_eq!(error.upstream_metadata().unwrap().retry_after, Some(2));
    }

    #[tokio::test]
    async fn quota_rate_limit_headers_are_not_retried() {
        let response = response_with_status_headers_and_body(
            "429 Too Many Requests",
            &[("x-openai-error-code", "insufficient_quota")],
            r#"{"error":{"message":"quota exhausted"}}"#,
        )
        .await;

        assert!(!should_retry_result(
            &Ok(response),
            UpstreamRequestPolicy::default()
        ));
    }

    #[tokio::test]
    async fn ordinary_rate_limits_remain_retryable() {
        let response = response_with_status_headers_and_body(
            "429 Too Many Requests",
            &[("retry-after-ms", "250")],
            r#"{"error":{"message":"slow down"}}"#,
        )
        .await;

        assert!(should_retry_result(
            &Ok(response),
            UpstreamRequestPolicy::default()
        ));
    }

    #[tokio::test]
    async fn upstream_stream_item_times_out_when_idle() {
        let result = next_upstream_stream_item_with_timeout(
            std::future::pending::<Option<Result<(), reqwest::Error>>>(),
            Duration::ZERO,
        )
        .await;

        assert!(matches!(result, Err(ProviderError::Timeout)));
    }

    #[tokio::test]
    async fn upstream_error_metadata_keeps_safe_headers_only() {
        let response = response_with_status_headers_and_body(
            "429 Too Many Requests",
            &[
                ("retry-after", "4"),
                ("x-openai-request-id", "req_123"),
                ("authorization", "Bearer secret"),
            ],
            r#"{"error":{"message":"slow down"}}"#,
        )
        .await;

        let error = map_upstream_response(response).await;
        assert!(matches!(
            error.without_upstream_metadata(),
            ProviderError::RateLimited {
                retry_after: Some(4)
            }
        ));
        let metadata = error.upstream_metadata().unwrap();
        assert_eq!(metadata.status, 429);
        assert_eq!(metadata.retry_after, Some(4));
        assert_eq!(metadata.request_id.as_deref(), Some("req_123"));
        assert_eq!(metadata.message.as_deref(), Some("slow down"));
        assert!(
            metadata
                .headers
                .iter()
                .any(|header| header.name == "x-openai-request-id" && header.value == "req_123")
        );
        assert!(
            !metadata
                .headers
                .iter()
                .any(|header| header.name.eq_ignore_ascii_case("authorization"))
        );
    }

    #[tokio::test]
    async fn upstream_error_metadata_truncates_body_preview() {
        let body = "x".repeat(MAX_UPSTREAM_ERROR_PREVIEW_BYTES + 128);
        let response =
            response_with_status_headers_and_body("500 Internal Server Error", &[], &body).await;

        let error = map_upstream_response(response).await;
        let metadata = error.upstream_metadata().unwrap();

        assert_eq!(metadata.status, 500);
        assert!(matches!(
            error.without_upstream_metadata(),
            ProviderError::Overloaded { .. }
        ));
        let preview = metadata.body_preview.as_deref().unwrap();
        assert!(preview.len() < body.len());
        assert!(preview.ends_with("..."));
    }

    #[tokio::test]
    async fn upstream_error_body_read_times_out_when_idle() {
        let response = response_with_stalled_body("partial upstream failure").await;

        let body = read_limited_response_text_with_timeout(
            response,
            MAX_UPSTREAM_ERROR_BODY_BYTES,
            Duration::from_millis(20),
        )
        .await;

        assert!(body.contains("partial upstream failure"));
        assert!(body.contains("[upstream error body read timed out]"));
    }

    #[tokio::test]
    async fn upstream_success_body_read_times_out_when_idle() {
        let response = response_with_stalled_body("partial upstream success").await;

        let result =
            read_upstream_response_text_with_timeout(response, Duration::from_millis(20)).await;

        assert!(matches!(result, Err(ProviderError::Timeout)));
    }

    #[tokio::test]
    async fn request_attempt_times_out_when_policy_budget_expires() {
        let result = with_request_timeout(
            std::future::pending::<Result<(), ProviderError>>(),
            Some(Duration::ZERO),
        )
        .await;

        assert!(matches!(result, Err(ProviderError::Timeout)));
    }

    #[test]
    fn request_policy_uses_one_attempt_when_configured_zero() {
        let policy = UpstreamRequestPolicy {
            max_attempts: 0,
            ..UpstreamRequestPolicy::default()
        };

        assert_eq!(policy.max_attempts(), 1);
    }

    #[test]
    fn runtime_config_overrides_request_policy() {
        let runtime = ProviderRuntimeConfig {
            retry: ProviderRetryConfig {
                max_attempts: Some(4),
                base_delay_ms: Some(50),
                network_errors: Some(false),
                timeout_errors: Some(false),
                rate_limits: Some(false),
                transient_statuses: Some(false),
            },
            request: ProviderRequestConfig {
                attempt_timeout_seconds: Some(30),
                ..Default::default()
            },
            ..Default::default()
        };

        let policy = UpstreamRequestPolicy::from_runtime_config(&runtime);

        assert_eq!(policy.max_attempts, 4);
        assert_eq!(policy.base_retry_delay, Duration::from_millis(50));
        assert_eq!(policy.attempt_timeout, Some(Duration::from_secs(30)));
        assert!(!policy.retry_network_errors);
        assert!(!policy.retry_timeout_errors);
        assert!(!policy.retry_rate_limits);
        assert!(!policy.retry_transient_statuses);
    }

    #[test]
    fn runtime_config_retry_toggles_control_retryable_results() {
        let policy = UpstreamRequestPolicy {
            retry_network_errors: false,
            retry_timeout_errors: false,
            retry_rate_limits: false,
            retry_transient_statuses: false,
            ..UpstreamRequestPolicy::default()
        };

        assert!(!should_retry_result(&Err(ProviderError::Timeout), policy));
        assert!(!should_retry_result(
            &Err(ProviderError::Network("connection closed".to_string())),
            policy
        ));
        assert!(!should_retry_status(StatusCode::TOO_MANY_REQUESTS, policy));
        assert!(!should_retry_status(
            StatusCode::INTERNAL_SERVER_ERROR,
            policy
        ));
    }
}
