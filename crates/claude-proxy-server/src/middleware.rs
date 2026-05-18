//! Rate limiting middleware using governor.

use std::future::Future;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, Request, Response, StatusCode};
use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use tower::{Layer, Service};

type Governor = RateLimiter<GovernorKey, DefaultKeyedStateStore<GovernorKey>, DefaultClock>;

/// Key for rate limiting: uses x-api-key, Bearer auth, or "anonymous" if absent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GovernorKey(String);

impl GovernorKey {
    fn from_request<B>(req: &Request<B>) -> Self {
        let key = req
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .filter(|v| !v.is_empty())
            .or_else(|| {
                req.headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|auth| auth.strip_prefix("Bearer "))
                    .filter(|v| !v.is_empty())
            })
            .unwrap_or("anonymous")
            .to_string();
        Self(key)
    }
}

/// Rate limit configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimitConfig {
    pub max_requests: u32,
    pub per_seconds: u32,
}

/// Runtime rate limit state that can be refreshed after config reload.
pub struct RateLimitRuntime {
    governor: RwLock<Arc<Governor>>,
    retry_after_secs: AtomicU64,
}

impl RateLimitRuntime {
    pub fn new(config: RateLimitConfig) -> Self {
        let (governor, retry_after_secs) = build_governor(config);
        Self {
            governor: RwLock::new(Arc::new(governor)),
            retry_after_secs: AtomicU64::new(retry_after_secs),
        }
    }

    pub fn update(&self, config: RateLimitConfig) {
        let (governor, retry_after_secs) = build_governor(config);
        *self.governor.write().unwrap() = Arc::new(governor);
        self.retry_after_secs
            .store(retry_after_secs, Ordering::Relaxed);
    }

    fn check_key(&self, key: &GovernorKey) -> bool {
        let governor = Arc::clone(&self.governor.read().unwrap());
        governor.check_key(key).is_ok()
    }

    fn retry_after_secs(&self) -> u64 {
        self.retry_after_secs.load(Ordering::Relaxed)
    }
}

fn build_governor(config: RateLimitConfig) -> (Governor, u64) {
    let max_requests = config.max_requests.max(1);
    let max = NonZeroU32::new(max_requests).unwrap();
    let quota = if config.per_seconds <= 1 {
        Quota::per_second(max)
    } else {
        Quota::with_period(Duration::from_secs_f64(
            config.per_seconds as f64 / max_requests as f64,
        ))
        .unwrap()
        .allow_burst(max)
    };
    let retry_after_secs = (config.per_seconds as f64 / max_requests as f64).ceil() as u64;
    (RateLimiter::keyed(quota), retry_after_secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;

    #[test]
    fn governor_key_uses_bearer_token_without_api_key() {
        let request = Request::builder()
            .header("authorization", "Bearer bearer-token")
            .body(())
            .unwrap();

        assert_eq!(
            GovernorKey::from_request(&request),
            GovernorKey("bearer-token".to_string())
        );
    }

    #[test]
    fn governor_key_prefers_x_api_key_over_bearer_token() {
        let request = Request::builder()
            .header("x-api-key", "api-key")
            .header("authorization", "Bearer bearer-token")
            .body(())
            .unwrap();

        assert_eq!(
            GovernorKey::from_request(&request),
            GovernorKey("api-key".to_string())
        );
    }

    #[test]
    fn rate_limit_runtime_update_replaces_limiter() {
        let runtime = RateLimitRuntime::new(RateLimitConfig {
            max_requests: 1,
            per_seconds: 60,
        });
        let key = GovernorKey("client".to_string());

        assert!(runtime.check_key(&key));
        assert!(!runtime.check_key(&key));

        runtime.update(RateLimitConfig {
            max_requests: 2,
            per_seconds: 60,
        });

        assert!(runtime.check_key(&key));
        assert!(runtime.check_key(&key));
        assert!(!runtime.check_key(&key));
        assert_eq!(runtime.retry_after_secs(), 30);
    }
}

/// A Tower layer that enforces rate limits.
#[derive(Clone)]
pub struct RateLimitLayer {
    runtime: Arc<RateLimitRuntime>,
}

impl RateLimitLayer {
    pub fn new(runtime: Arc<RateLimitRuntime>) -> Self {
        Self { runtime }
    }
}

impl<S> Layer<S> for RateLimitLayer {
    type Service = RateLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimitService {
            inner,
            runtime: self.runtime.clone(),
        }
    }
}

/// Rate limiting service wrapper.
#[derive(Clone)]
pub struct RateLimitService<S> {
    inner: S,
    runtime: Arc<RateLimitRuntime>,
}

impl<S, B> Service<Request<B>> for RateLimitService<S>
where
    S: Service<Request<B>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    B: Send + 'static,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<B>) -> Self::Future {
        let key = GovernorKey::from_request(&req);

        if self.runtime.check_key(&key) {
            // Allowed — forward to inner service
            Box::pin(self.inner.call(req))
        } else {
            // Rate limited
            let retry_after = self.runtime.retry_after_secs().max(1).to_string();
            let body = serde_json::json!({
                "type": "error",
                "error": {
                    "type": "rate_limit_error",
                    "message": "rate limit exceeded"
                }
            });
            let response = Response::builder()
                .status(StatusCode::TOO_MANY_REQUESTS)
                .header("content-type", "application/json")
                .header(
                    "retry-after",
                    HeaderValue::from_str(&retry_after).unwrap_or(HeaderValue::from_static("1")),
                )
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap();
            Box::pin(async move { Ok(response) })
        }
    }
}
