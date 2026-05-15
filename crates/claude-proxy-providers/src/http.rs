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

use crate::provider::ProviderError;
use serde_json::Value;

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

pub async fn map_upstream_response(response: reqwest::Response) -> ProviderError {
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let body = response.text().await.unwrap_or_default();
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
