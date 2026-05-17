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

use std::path::Path;
use std::time::Duration;

use crate::provider::ProviderError;
use futures::StreamExt;
use reqwest::StatusCode;
use serde_json::Value;
use tokio::time::sleep;

const MAX_UPSTREAM_ERROR_BODY_BYTES: usize = 64 * 1024;
const MAX_SEND_ATTEMPTS: usize = 3;
const BASE_RETRY_DELAY: Duration = Duration::from_millis(200);
const MAX_RETRY_AFTER_DELAY: Duration = Duration::from_secs(5);

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

pub async fn send_upstream_request(
    request: reqwest::RequestBuilder,
) -> Result<reqwest::Response, ProviderError> {
    let mut attempt = 0;
    loop {
        attempt += 1;
        let Some(next_request) = request.try_clone() else {
            return send_once(request).await;
        };

        let response = send_once(next_request).await;
        if attempt >= MAX_SEND_ATTEMPTS || !should_retry_result(&response) {
            return response;
        }

        sleep(retry_delay(attempt, response.as_ref().ok())).await;
    }
}

async fn send_once(request: reqwest::RequestBuilder) -> Result<reqwest::Response, ProviderError> {
    request.send().await.map_err(|e| {
        if e.is_timeout() {
            ProviderError::Timeout
        } else {
            ProviderError::Network(fmt_reqwest_err(&e))
        }
    })
}

fn should_retry_result(response: &Result<reqwest::Response, ProviderError>) -> bool {
    match response {
        Ok(response) => is_retryable_status(response.status()),
        Err(ProviderError::Timeout) | Err(ProviderError::Network(_)) => true,
        Err(_) => false,
    }
}

fn is_retryable_status(status: StatusCode) -> bool {
    status == StatusCode::REQUEST_TIMEOUT
        || status == StatusCode::CONFLICT
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn retry_delay(attempt: usize, response: Option<&reqwest::Response>) -> Duration {
    response
        .and_then(retry_after_delay)
        .unwrap_or_else(|| BASE_RETRY_DELAY * attempt as u32)
}

fn retry_after_delay_secs(value: &str) -> Option<Duration> {
    value
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .map(|delay| delay.min(MAX_RETRY_AFTER_DELAY))
}

fn retry_after_delay(response: &reqwest::Response) -> Option<Duration> {
    response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(retry_after_delay_secs)
}

pub async fn map_upstream_response(response: reqwest::Response) -> ProviderError {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let body = read_limited_response_text(response, MAX_UPSTREAM_ERROR_BODY_BYTES).await;
    let message = extract_upstream_error_message(&body);

    match status {
        400 => ProviderError::InvalidRequest(message),
        401 => ProviderError::Authentication(message),
        404 => ProviderError::ModelNotFound(message),
        413 => ProviderError::RequestTooLarge(message),
        429 => ProviderError::RateLimited { retry_after },
        503 | 529 => ProviderError::Overloaded {
            message,
            retry_after,
        },
        _ => ProviderError::UpstreamError { status, body },
    }
}

async fn read_limited_response_text(response: reqwest::Response, limit: usize) -> String {
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    let mut truncated = false;

    while let Some(chunk) = stream.next().await {
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
    }

    #[test]
    fn timeout_and_network_errors_are_retryable() {
        assert!(should_retry_result(&Err(ProviderError::Timeout)));
        assert!(should_retry_result(&Err(ProviderError::Network(
            "connection closed".to_string()
        ))));
        assert!(!should_retry_result(&Err(ProviderError::InvalidRequest(
            "bad request".to_string()
        ))));
    }
}
