//! Rate limiting middleware using governor.

use std::future::Future;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::body::Body;
use axum::http::{HeaderValue, Request, Response, StatusCode};
use governor::clock::DefaultClock;
use governor::state::keyed::DefaultKeyedStateStore;
use governor::{Quota, RateLimiter};
use tower::{Layer, Service};

type Governor = RateLimiter<GovernorKey, DefaultKeyedStateStore<GovernorKey>, DefaultClock>;

/// Key for rate limiting: uses x-api-key header value, or "anonymous" if absent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct GovernorKey(String);

impl GovernorKey {
    fn from_request<B>(req: &Request<B>) -> Self {
        let key = req
            .headers()
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("anonymous")
            .to_string();
        Self(key)
    }
}

/// Rate limit configuration.
#[derive(Clone)]
pub struct RateLimitConfig {
    pub max_requests: u32,
    pub per_seconds: u32,
}

/// A Tower layer that enforces rate limits.
#[derive(Clone)]
pub struct RateLimitLayer {
    governor: Arc<Governor>,
    retry_after_secs: u64,
}

impl RateLimitLayer {
    pub fn new(config: RateLimitConfig) -> Self {
        let max = NonZeroU32::new(config.max_requests.max(1)).unwrap();
        let quota = if config.per_seconds <= 1 {
            Quota::per_second(max)
        } else {
            Quota::with_period(Duration::from_secs_f64(
                config.per_seconds as f64 / config.max_requests as f64,
            ))
            .unwrap()
            .allow_burst(max)
        };
        let governor = Arc::new(RateLimiter::keyed(quota));
        let retry_after_secs =
            (config.per_seconds as f64 / config.max_requests as f64).ceil() as u64;
        Self {
            governor,
            retry_after_secs,
        }
    }
}

impl<S> Layer<S> for RateLimitLayer {
    type Service = RateLimitService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RateLimitService {
            inner,
            governor: self.governor.clone(),
            retry_after_secs: self.retry_after_secs,
        }
    }
}

/// Rate limiting service wrapper.
#[derive(Clone)]
pub struct RateLimitService<S> {
    inner: S,
    governor: Arc<Governor>,
    retry_after_secs: u64,
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

        match self.governor.check_key(&key) {
            Ok(()) => {
                // Allowed — forward to inner service
                Box::pin(self.inner.call(req))
            }
            Err(_) => {
                // Rate limited
                let retry_after = self.retry_after_secs.max(1).to_string();
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
                        HeaderValue::from_str(&retry_after)
                            .unwrap_or(HeaderValue::from_static("1")),
                    )
                    .body(Body::from(serde_json::to_string(&body).unwrap()))
                    .unwrap();
                Box::pin(async move { Ok(response) })
            }
        }
    }
}
