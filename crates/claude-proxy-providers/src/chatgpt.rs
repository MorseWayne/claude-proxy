//! ChatGPT account provider adapter.
//!
//! Uses the same OpenAI Auth device flow and Codex Responses endpoint that
//! opencode uses for ChatGPT Pro/Plus authentication.

mod auth;
mod responses;

use std::collections::BTreeSet;
use std::fs;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use claude_proxy_config::{
    Settings,
    settings::{
        ChatGptProviderConfig, DEFAULT_CHATGPT_ORIGINATOR, DEFAULT_CHATGPT_USER_AGENT,
        ProviderConfig,
    },
};
use claude_proxy_core::*;
use futures::stream::BoxStream;
use reqwest::{
    Client, Response, StatusCode,
    header::{HeaderMap, HeaderValue},
};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::http::{
    UpstreamRequestPolicy, apply_extra_ca_certs, fmt_reqwest_err, map_upstream_response,
    send_upstream_request_with_policy,
};
use crate::openai_compat::{apply_openai_intent, log_request_observability, openai_model_info};
use crate::provider::{
    Provider, ProviderError, RateLimitCredits, RateLimitSnapshot, RateLimitSource, RateLimitWindow,
};

const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const DEFAULT_CHATGPT_INSTRUCTIONS: &str = "Follow the user's instructions.";
const CHATGPT_SEND_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(60);
const CHATGPT_SEND_MAX_ATTEMPTS: usize = 2;
const CHATGPT_USAGE_FETCH_INTERVAL: Duration = Duration::from_secs(60);

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

#[derive(Debug, Clone)]
struct ChatGptRequestHeaders {
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
    ) -> Result<Response, ProviderError> {
        let mut request_builder = self
            .http_client
            .post(&self.endpoint)
            .bearer_auth(&token.access_token)
            .header("Content-Type", "application/json")
            .header("originator", self.request_headers.originator.clone())
            .header("User-Agent", self.request_headers.user_agent.clone());

        if let Some(account_id) = token.account_id.as_deref() {
            request_builder = request_builder.header("ChatGPT-Account-Id", account_id);
        }

        send_upstream_request_with_policy(
            request_builder.json(body),
            chatgpt_upstream_request_policy(),
        )
        .await
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

#[async_trait]
impl Provider for ChatGptProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn chat(
        &self,
        request: MessagesRequest,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let token = self.auth.get_token().await?;
        let request = apply_openai_intent(request);
        let body = build_chatgpt_responses_body_with_context(&request, Some(&self.installation_id));
        log_request_observability("chatgpt", "/responses", &body);

        let mut response = self.send_responses_request(&body, &token).await?;
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
            response = self.send_responses_request(&body, &refreshed).await?;
            if response.status() == StatusCode::UNAUTHORIZED {
                self.auth.clear_token().await;
            }
        }

        if !response.status().is_success() {
            return Err(map_upstream_response(response).await);
        }

        let header_snapshots =
            rate_limit_snapshots_from_headers(&self.id, response.headers(), unix_timestamp_secs());
        self.cache_rate_limits(header_snapshots).await;

        let cache = Arc::clone(&self.cached_rate_limits);
        let provider_id = self.id.clone();
        Ok(responses::stream_response(response, move |event| {
            if let Some(snapshot) =
                rate_limit_snapshot_from_sse_event(&provider_id, event, unix_timestamp_secs())
            {
                let cache = Arc::clone(&cache);
                tokio::spawn(async move {
                    cache_rate_limits_into(&cache, vec![snapshot]).await;
                });
            }
        }))
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
    Ok(ChatGptRequestHeaders {
        originator: chatgpt_header_value(
            "originator",
            &config.originator,
            DEFAULT_CHATGPT_ORIGINATOR,
        )?,
        user_agent: chatgpt_header_value(
            "User-Agent",
            &config.user_agent,
            DEFAULT_CHATGPT_USER_AGENT,
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
    let id = uuid::Uuid::new_v4().to_string();
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
        source: RateLimitSource::ResponseHeaders,
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
            let snapshot = RateLimitSnapshot {
                provider_id: provider_id.to_string(),
                feature: Some(limit_id.clone()),
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

fn build_chatgpt_responses_body_with_context(
    request: &MessagesRequest,
    installation_id: Option<&str>,
) -> Value {
    responses::build_body(request, DEFAULT_CHATGPT_INSTRUCTIONS, installation_id)
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
        headers.insert("x-agent-primary-used-percent", "9.5".parse().unwrap());
        headers.insert("x-agent-limit-name", "Agent".parse().unwrap());

        let snapshots = rate_limit_snapshots_from_headers("chatgpt", &headers, 456);

        assert_eq!(snapshots.len(), 2);
        assert_eq!(snapshots[0].feature.as_deref(), Some("agent"));
        assert_eq!(snapshots[0].limit_name.as_deref(), Some("Agent"));
        assert_eq!(snapshots[0].primary.as_ref().unwrap().used_percent, 9.5);
        assert_eq!(snapshots[1].feature.as_deref(), Some("codex"));
        assert_eq!(snapshots[1].secondary.as_ref().unwrap().used_percent, 75.0);
        assert_eq!(
            snapshots[1].credits.as_ref().unwrap().balance.as_deref(),
            Some("7.50")
        );
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
        assert_eq!(snapshot.source, RateLimitSource::ResponseHeaders);
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
        };

        let headers = chatgpt_request_headers(&config).unwrap();
        assert_eq!(headers.originator.to_str().unwrap(), "codex_cli");
        assert_eq!(headers.user_agent.to_str().unwrap(), "CodexCLI/1.2.3");

        let config = claude_proxy_config::settings::ChatGptProviderConfig {
            originator: "  ".to_string(),
            user_agent: "\t".to_string(),
        };

        let headers = chatgpt_request_headers(&config).unwrap();
        assert_eq!(headers.originator.to_str().unwrap(), "opencode");
        assert_eq!(
            headers.user_agent.to_str().unwrap(),
            "opencode/claude-proxy"
        );
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
        assert!(body.get("max_output_tokens").is_none());
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
        assert!(body.get("max_output_tokens").is_none());
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
        assert!(body.get("max_output_tokens").is_none());
    }
}
