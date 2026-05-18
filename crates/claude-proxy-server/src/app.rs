use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use claude_proxy_config::Settings;
use claude_proxy_config::settings::LimitsConfig;
use claude_proxy_core::ModelInfo;
use claude_proxy_providers::provider::Provider;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, Semaphore};

use crate::middleware::{RateLimitConfig, RateLimitRuntime};
use crate::persistence::{MetricsStore, StoredTotals};

const DEFAULT_MODEL_CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_PROVIDER_HEALTH_ERROR_LEN: usize = 2048;

/// Token usage breakdown for a single request.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// Accumulated usage metrics for a grouping key.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UsageMetrics {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl UsageMetrics {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    fn add_usage(&mut self, usage: &TokenUsage) {
        self.requests += 1;
        self.input_tokens += usage.input_tokens;
        self.output_tokens += usage.output_tokens;
        self.cache_creation_input_tokens += usage.cache_creation_input_tokens;
        self.cache_read_input_tokens += usage.cache_read_input_tokens;
    }
}

pub type ModelMetrics = UsageMetrics;

/// Request metrics counters.
pub struct Metrics {
    pub requests_total: AtomicU64,
    pub errors_total: AtomicU64,
    pub latency_sum_ms: AtomicU64,
    pub latency_count: AtomicU64,
    /// Per-model token usage metrics (current session).
    pub model_metrics: Mutex<HashMap<String, UsageMetrics>>,
    /// Per-provider token usage metrics (current session).
    pub provider_metrics: Mutex<HashMap<String, UsageMetrics>>,
    /// Per-initiator token usage metrics (current session).
    pub initiator_metrics: Mutex<HashMap<String, UsageMetrics>>,
    /// Persistent store for all-time metrics.
    store: Option<Arc<MetricsStore>>,
    /// All-time totals loaded from store at startup.
    stored_totals: Mutex<StoredTotals>,
}

impl Metrics {
    pub fn new(store: Option<Arc<MetricsStore>>) -> Self {
        Self {
            requests_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            latency_sum_ms: AtomicU64::new(0),
            latency_count: AtomicU64::new(0),
            model_metrics: Mutex::new(HashMap::new()),
            provider_metrics: Mutex::new(HashMap::new()),
            initiator_metrics: Mutex::new(HashMap::new()),
            store,
            stored_totals: Mutex::new(StoredTotals::default()),
        }
    }

    pub fn record_request(&self) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_error(&self) {
        self.errors_total.fetch_add(1, Ordering::Relaxed);
    }

    pub fn record_latency(&self, ms: u64) {
        self.latency_sum_ms.fetch_add(ms, Ordering::Relaxed);
        self.latency_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Record token usage for a model (in-memory).
    pub async fn record_token_usage(&self, model: &str, usage: &TokenUsage) {
        let mut map = self.model_metrics.lock().await;
        map.entry(model.to_string()).or_default().add_usage(usage);
    }

    async fn record_usage_dimensions(
        &self,
        provider: &str,
        initiator: &str,
        model: &str,
        usage: &TokenUsage,
    ) {
        self.record_token_usage(model, usage).await;
        self.provider_metrics
            .lock()
            .await
            .entry(provider.to_string())
            .or_default()
            .add_usage(usage);
        self.initiator_metrics
            .lock()
            .await
            .entry(initiator.to_string())
            .or_default()
            .add_usage(usage);
    }

    /// Record a completed request with usage, persisting to store if available.
    pub async fn record_completed_request(
        &self,
        provider: &str,
        initiator: &str,
        model: &str,
        usage: &TokenUsage,
        is_error: bool,
        latency_ms: u64,
    ) {
        self.record_usage_dimensions(provider, initiator, model, usage)
            .await;
        if let Some(ref store) = self.store {
            store.record_usage(provider, initiator, model, usage, is_error, latency_ms);
        }
    }

    /// Load stored totals from persistence (called once at startup).
    pub async fn load_stored_totals(&self) {
        if let Some(ref store) = self.store {
            let totals = store.load_totals().await;
            *self.stored_totals.lock().await = totals;
        }
    }

    pub async fn to_json(&self) -> serde_json::Value {
        let requests = self.requests_total.load(Ordering::Relaxed);
        let errors = self.errors_total.load(Ordering::Relaxed);
        let latency_sum = self.latency_sum_ms.load(Ordering::Relaxed);
        let latency_count = self.latency_count.load(Ordering::Relaxed);
        let avg_latency = latency_sum.checked_div(latency_count).unwrap_or(0);

        let model_metrics = self.model_metrics.lock().await;
        let models: serde_json::Value = serde_json::to_value(&*model_metrics).unwrap_or_default();
        let provider_metrics = self.provider_metrics.lock().await;
        let providers: serde_json::Value =
            serde_json::to_value(&*provider_metrics).unwrap_or_default();
        let initiator_metrics = self.initiator_metrics.lock().await;
        let initiators: serde_json::Value =
            serde_json::to_value(&*initiator_metrics).unwrap_or_default();

        let stored = self.stored_totals.lock().await;
        let stored_models: serde_json::Value =
            serde_json::to_value(&stored.model_metrics).unwrap_or_default();
        let stored_providers: serde_json::Value =
            serde_json::to_value(&stored.provider_metrics).unwrap_or_default();
        let stored_initiators: serde_json::Value =
            serde_json::to_value(&stored.initiator_metrics).unwrap_or_default();

        json!({
            "requests_total": requests,
            "errors_total": errors,
            "avg_latency_ms": avg_latency,
            "models": models,
            "providers": providers,
            "initiators": initiators,
            "stored": {
                "requests_total": stored.requests_total,
                "errors_total": stored.errors_total,
                "avg_latency_ms": stored.latency_sum_ms.checked_div(stored.latency_count).unwrap_or(0),
                "models": stored_models,
                "providers": stored_providers,
                "initiators": stored_initiators,
            },
        })
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new(None)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderHealthStatus {
    #[default]
    Unknown,
    Healthy,
    Unhealthy,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderHealth {
    pub status: ProviderHealthStatus,
    pub last_ok_unix_secs: Option<u64>,
    pub last_error_unix_secs: Option<u64>,
    pub last_error: Option<String>,
}

impl ProviderHealth {
    fn mark_success(&mut self, timestamp: u64) {
        self.status = ProviderHealthStatus::Healthy;
        self.last_ok_unix_secs = Some(timestamp);
    }

    fn mark_error(&mut self, timestamp: u64, error: &str) {
        self.status = ProviderHealthStatus::Unhealthy;
        self.last_error_unix_secs = Some(timestamp);
        self.last_error = Some(truncate_provider_error(error));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_proxy_config::settings::{
        AdminConfig, HttpConfig, LogConfig, ModelAliasConfig, ModelConfig, ProviderConfig,
        ProviderType, ServerConfig,
    };

    fn settings_with_limits(
        max_concurrency: u32,
        provider_max_concurrency: u32,
        rate_limit: u32,
        rate_window: u32,
    ) -> Settings {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                api_key: "test-key".to_string(),
                base_url: "http://127.0.0.1:9".to_string(),
                proxy: String::new(),
                provider_type: Some(ProviderType::OpenAI),
                copilot: None,
            },
        );

        Settings {
            providers,
            model: ModelConfig {
                default: ModelAliasConfig::new("openai/gpt-4"),
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
            limits: LimitsConfig {
                rate_limit,
                rate_window,
                max_concurrency,
                provider_max_concurrency,
                model_cache_ttl_seconds: DEFAULT_MODEL_CACHE_TTL.as_secs(),
            },
            http: HttpConfig::default(),
            log: LogConfig::default(),
        }
    }

    #[test]
    fn total_tokens_excludes_cache_tokens() {
        let metrics = ModelMetrics {
            requests: 1,
            input_tokens: 100,
            output_tokens: 25,
            cache_creation_input_tokens: 40,
            cache_read_input_tokens: 60,
        };

        assert_eq!(metrics.total_tokens(), 125);
    }

    #[tokio::test]
    async fn completed_request_records_provider_and_initiator_metrics() {
        let metrics = Metrics::default();
        let usage = TokenUsage {
            input_tokens: 11,
            output_tokens: 7,
            cache_creation_input_tokens: 3,
            cache_read_input_tokens: 2,
        };

        metrics
            .record_completed_request("chatgpt", "agent", "gpt-5.5", &usage, false, 123)
            .await;
        let data = metrics.to_json().await;

        assert_eq!(data["models"]["gpt-5.5"]["requests"], 1);
        assert_eq!(data["models"]["gpt-5.5"]["input_tokens"], 11);
        assert_eq!(data["providers"]["chatgpt"]["output_tokens"], 7);
        assert_eq!(
            data["initiators"]["agent"]["cache_creation_input_tokens"],
            3
        );
        assert_eq!(data["stored"]["providers"].as_object().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn provider_health_tracks_recent_error_and_recovery() {
        let state = AppState::new(settings_with_limits(1, 1, 10, 60), None);

        let snapshot = state
            .provider_health_snapshot(vec!["openai".to_string()])
            .await;
        assert_eq!(snapshot["openai"].status, ProviderHealthStatus::Unknown);

        state
            .record_provider_error("openai", "upstream timeout")
            .await;
        let snapshot = state
            .provider_health_snapshot(vec!["openai".to_string()])
            .await;
        assert_eq!(snapshot["openai"].status, ProviderHealthStatus::Unhealthy);
        assert_eq!(
            snapshot["openai"].last_error.as_deref(),
            Some("upstream timeout")
        );

        state.record_provider_success("openai").await;
        let snapshot = state
            .provider_health_snapshot(vec!["openai".to_string()])
            .await;
        assert_eq!(snapshot["openai"].status, ProviderHealthStatus::Healthy);
        assert_eq!(
            snapshot["openai"].last_error.as_deref(),
            Some("upstream timeout")
        );
        assert!(snapshot["openai"].last_ok_unix_secs.is_some());
    }

    #[test]
    fn provider_registry_exports_model_capabilities() {
        let mut registry = ProviderRegistry::new();
        registry.cache_models(
            "chatgpt",
            vec![claude_proxy_core::ModelInfo {
                model_id: "gpt-5.5".to_string(),
                supports_thinking: Some(true),
                vendor: Some("openai".to_string()),
                max_output_tokens: Some(128_000),
                context_window: Some(400_000),
                supported_endpoints: vec!["/responses".to_string()],
                is_chat_default: None,
                supports_vision: Some(true),
                supports_adaptive_thinking: Some(false),
                min_thinking_budget: Some(1024),
                max_thinking_budget: Some(32_000),
                reasoning_effort_levels: vec!["low".to_string(), "high".to_string()],
            }],
        );

        let capabilities = registry.model_capabilities();

        assert_eq!(capabilities["chatgpt/gpt-5.5"]["provider"], "chatgpt");
        assert_eq!(capabilities["chatgpt/gpt-5.5"]["model"], "gpt-5.5");
        assert_eq!(
            capabilities["chatgpt/gpt-5.5"]["max_output_tokens"],
            128_000
        );
        assert_eq!(capabilities["chatgpt/gpt-5.5"]["context_window"], 400_000);
        assert_eq!(
            capabilities["chatgpt/gpt-5.5"]["supported_endpoints"][0],
            "/responses"
        );
        assert_eq!(
            capabilities["chatgpt/gpt-5.5"]["reasoning_effort_levels"][1],
            "high"
        );
    }

    #[test]
    fn provider_registry_treats_expired_models_as_stale() {
        let mut registry = ProviderRegistry::new();
        registry.cache_models_at(
            "openai",
            vec![ModelInfo {
                model_id: "stale-model".to_string(),
                supports_thinking: None,
                vendor: None,
                max_output_tokens: None,
                context_window: None,
                supported_endpoints: Vec::new(),
                is_chat_default: None,
                supports_vision: None,
                supports_adaptive_thinking: None,
                min_thinking_budget: None,
                max_thinking_budget: None,
                reasoning_effort_levels: Vec::new(),
            }],
            Instant::now() - DEFAULT_MODEL_CACHE_TTL - Duration::from_secs(1),
        );

        assert!(registry.cached_models("openai").is_none());
        assert_eq!(registry.all_cached_models().len(), 1);
    }

    #[test]
    fn provider_registry_uses_configured_model_cache_ttl() {
        let mut registry = ProviderRegistry::with_model_cache_ttl(Duration::from_secs(10));
        registry.cache_models_at(
            "openai",
            vec![ModelInfo {
                model_id: "still-fresh".to_string(),
                supports_thinking: None,
                vendor: None,
                max_output_tokens: None,
                context_window: None,
                supported_endpoints: Vec::new(),
                is_chat_default: None,
                supports_vision: None,
                supports_adaptive_thinking: None,
                min_thinking_budget: None,
                max_thinking_budget: None,
                reasoning_effort_levels: Vec::new(),
            }],
            Instant::now() - Duration::from_secs(6),
        );

        assert!(registry.cached_models("openai").is_some());

        registry.set_model_cache_ttl(Duration::from_secs(5));

        assert!(registry.cached_models("openai").is_none());
    }

    #[tokio::test]
    async fn apply_settings_refreshes_runtime_limits() {
        let state = AppState::new(settings_with_limits(1, 1, 1, 60), None);
        let original_semaphore = state.concurrency_semaphore.read().await.clone();
        let _permit = original_semaphore.clone().try_acquire_owned().unwrap();
        assert_eq!(
            state.concurrency_semaphore.read().await.available_permits(),
            0
        );

        state
            .provider_concurrency_semaphores
            .lock()
            .await
            .insert("openai".to_string(), Arc::new(Semaphore::new(1)));
        state
            .model_refresh_locks
            .lock()
            .await
            .insert("openai".to_string(), Arc::new(Mutex::new(())));

        state
            .apply_settings(settings_with_limits(3, 2, 2, 60))
            .await;

        assert_eq!(
            state.concurrency_semaphore.read().await.available_permits(),
            3
        );
        assert!(
            state
                .provider_concurrency_semaphores
                .lock()
                .await
                .is_empty()
        );
        assert!(state.provider_creation_locks.lock().await.is_empty());
        assert!(state.model_refresh_locks.lock().await.is_empty());
    }

    #[tokio::test]
    async fn get_or_create_provider_reuses_cached_provider() {
        let state = AppState::new(settings_with_limits(1, 1, 10, 60), None);

        let first = state.get_or_create_provider("openai").await.unwrap();
        let second = state.get_or_create_provider("openai").await.unwrap();

        assert!(Arc::ptr_eq(&first, &second));
    }

    #[tokio::test]
    async fn get_or_refresh_models_reuses_cached_models() {
        let state = AppState::new(settings_with_limits(1, 1, 10, 60), None);
        state.provider_registry.write().await.cache_models(
            "openai",
            vec![ModelInfo {
                model_id: "gpt-4.1".to_string(),
                supports_thinking: None,
                vendor: None,
                max_output_tokens: None,
                context_window: None,
                supported_endpoints: Vec::new(),
                is_chat_default: None,
                supports_vision: None,
                supports_adaptive_thinking: None,
                min_thinking_budget: None,
                max_thinking_budget: None,
                reasoning_effort_levels: Vec::new(),
            }],
        );

        let models = state.get_or_refresh_models("openai").await.unwrap();

        assert_eq!(models.len(), 1);
        assert_eq!(models[0].model_id, "gpt-4.1");
    }

    #[tokio::test]
    async fn get_or_refresh_models_refreshes_expired_cache() {
        let mut settings = settings_with_limits(1, 1, 10, 60);
        settings.providers.clear();
        settings.providers.insert(
            "anthropic".to_string(),
            ProviderConfig {
                api_key: "test-key".to_string(),
                base_url: String::new(),
                proxy: String::new(),
                provider_type: Some(ProviderType::Anthropic),
                copilot: None,
            },
        );
        let state = AppState::new(settings, None);
        state.provider_registry.write().await.cache_models_at(
            "anthropic",
            vec![ModelInfo {
                model_id: "stale-model".to_string(),
                supports_thinking: None,
                vendor: None,
                max_output_tokens: None,
                context_window: None,
                supported_endpoints: Vec::new(),
                is_chat_default: None,
                supports_vision: None,
                supports_adaptive_thinking: None,
                min_thinking_budget: None,
                max_thinking_budget: None,
                reasoning_effort_levels: Vec::new(),
            }],
            Instant::now() - DEFAULT_MODEL_CACHE_TTL - Duration::from_secs(1),
        );

        let models = state.get_or_refresh_models("anthropic").await.unwrap();

        assert!(
            models
                .iter()
                .any(|model| model.model_id == "claude-sonnet-4-20250514")
        );
        assert!(!models.iter().any(|model| model.model_id == "stale-model"));
        assert!(
            state
                .provider_registry
                .read()
                .await
                .cached_models("anthropic")
                .is_some()
        );
    }
}

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<RwLock<Settings>>,
    pub provider_registry: Arc<RwLock<ProviderRegistry>>,
    provider_creation_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    model_refresh_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    pub concurrency_semaphore: Arc<RwLock<Arc<Semaphore>>>,
    pub provider_concurrency_semaphores: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    pub rate_limit_runtime: Arc<RateLimitRuntime>,
    pub metrics: Arc<Metrics>,
    pub provider_health: Arc<Mutex<HashMap<String, ProviderHealth>>>,
    /// Inflight request deduplication: maps request hash → broadcast sender.
    /// Multiple identical concurrent requests share one upstream call.
    pub inflight: Arc<Mutex<HashMap<u64, tokio::sync::broadcast::Sender<InflightEvent>>>>,
}

/// An event in the inflight broadcast channel.
#[derive(Debug, Clone)]
pub enum InflightEvent {
    /// A successful SSE event from the provider stream.
    Event(claude_proxy_core::SseEvent),
    /// The stream completed (no more events).
    Done,
    /// An error occurred during streaming.
    Error(String),
}

/// Registry of provider instances and cached model lists.
pub struct ProviderRegistry {
    providers: std::collections::HashMap<String, Arc<dyn Provider>>,
    model_cache: std::collections::HashMap<String, ModelCacheEntry>,
    model_cache_ttl: Duration,
}

struct ModelCacheEntry {
    models: Vec<claude_proxy_core::ModelInfo>,
    cached_at: Instant,
}

impl ModelCacheEntry {
    fn new(models: Vec<claude_proxy_core::ModelInfo>) -> Self {
        Self {
            models,
            cached_at: Instant::now(),
        }
    }

    #[cfg(test)]
    fn with_timestamp(models: Vec<claude_proxy_core::ModelInfo>, cached_at: Instant) -> Self {
        Self { models, cached_at }
    }

    fn is_fresh_at(&self, now: Instant, ttl: Duration) -> bool {
        now.saturating_duration_since(self.cached_at) < ttl
    }
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::with_model_cache_ttl(DEFAULT_MODEL_CACHE_TTL)
    }

    pub fn with_model_cache_ttl(model_cache_ttl: Duration) -> Self {
        Self {
            providers: std::collections::HashMap::new(),
            model_cache: std::collections::HashMap::new(),
            model_cache_ttl: sanitize_model_cache_ttl(model_cache_ttl),
        }
    }

    pub fn set_model_cache_ttl(&mut self, model_cache_ttl: Duration) {
        self.model_cache_ttl = sanitize_model_cache_ttl(model_cache_ttl);
    }

    pub fn get(&self, provider_id: &str) -> Option<Arc<dyn Provider>> {
        self.providers.get(provider_id).cloned()
    }

    pub fn insert_if_absent(
        &mut self,
        provider_id: &str,
        provider: Arc<dyn Provider>,
    ) -> Arc<dyn Provider> {
        self.providers
            .entry(provider_id.to_string())
            .or_insert(provider)
            .clone()
    }

    /// Cache model list for a provider.
    pub fn cache_models(&mut self, provider_id: &str, models: Vec<claude_proxy_core::ModelInfo>) {
        self.model_cache
            .insert(provider_id.to_string(), ModelCacheEntry::new(models));
    }

    #[cfg(test)]
    fn cache_models_at(
        &mut self,
        provider_id: &str,
        models: Vec<claude_proxy_core::ModelInfo>,
        cached_at: Instant,
    ) {
        self.model_cache.insert(
            provider_id.to_string(),
            ModelCacheEntry::with_timestamp(models, cached_at),
        );
    }

    /// Get cached models for a provider.
    pub fn cached_models(&self, provider_id: &str) -> Option<&Vec<claude_proxy_core::ModelInfo>> {
        self.cached_models_at(provider_id, Instant::now())
    }

    fn cached_models_at(
        &self,
        provider_id: &str,
        now: Instant,
    ) -> Option<&Vec<claude_proxy_core::ModelInfo>> {
        self.model_cache
            .get(provider_id)
            .filter(|entry| entry.is_fresh_at(now, self.model_cache_ttl))
            .map(|entry| &entry.models)
    }

    /// Get all cached models across all providers.
    pub fn all_cached_models(&self) -> Vec<claude_proxy_core::ModelInfo> {
        self.model_cache
            .values()
            .flat_map(|entry| entry.models.iter())
            .cloned()
            .collect()
    }

    pub fn model_capabilities(&self) -> Value {
        let capabilities = self
            .model_cache
            .iter()
            .flat_map(|(provider_id, entry)| {
                entry.models.iter().map(move |model| {
                    (
                        format!("{provider_id}/{}", model.model_id),
                        json!({
                            "provider": provider_id,
                            "model": model.model_id,
                            "vendor": model.vendor,
                            "max_output_tokens": model.max_output_tokens,
                            "context_window": model.context_window,
                            "supported_endpoints": model.supported_endpoints,
                            "supports_thinking": model.supports_thinking,
                            "supports_vision": model.supports_vision,
                            "supports_adaptive_thinking": model.supports_adaptive_thinking,
                            "min_thinking_budget": model.min_thinking_budget,
                            "max_thinking_budget": model.max_thinking_budget,
                            "reasoning_effort_levels": model.reasoning_effort_levels,
                            "is_chat_default": model.is_chat_default,
                        }),
                    )
                })
            })
            .collect::<serde_json::Map<_, _>>();
        Value::Object(capabilities)
    }

    /// Clear all providers (on config reload).
    pub fn clear(&mut self) {
        self.providers.clear();
        self.model_cache.clear();
    }
}

impl AppState {
    pub fn new(settings: Settings, store: Option<Arc<MetricsStore>>) -> Self {
        let limits = settings.limits.clone();
        Self {
            settings: Arc::new(RwLock::new(settings)),
            provider_registry: Arc::new(RwLock::new(ProviderRegistry::with_model_cache_ttl(
                model_cache_ttl_from_limits(&limits),
            ))),
            provider_creation_locks: Arc::new(Mutex::new(HashMap::new())),
            model_refresh_locks: Arc::new(Mutex::new(HashMap::new())),
            concurrency_semaphore: Arc::new(RwLock::new(Arc::new(Semaphore::new(
                limits.max_concurrency as usize,
            )))),
            provider_concurrency_semaphores: Arc::new(Mutex::new(HashMap::new())),
            rate_limit_runtime: Arc::new(RateLimitRuntime::new(RateLimitConfig {
                max_requests: limits.rate_limit,
                per_seconds: limits.rate_window,
            })),
            metrics: Arc::new(Metrics::new(store)),
            provider_health: Arc::new(Mutex::new(HashMap::new())),
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn apply_settings(&self, settings: Settings) {
        let limits = settings.limits.clone();
        *self.settings.write().await = settings;
        self.provider_registry.write().await.clear();
        self.provider_creation_locks.lock().await.clear();
        self.model_refresh_locks.lock().await.clear();
        self.provider_health.lock().await.clear();
        self.apply_limits(&limits).await;
    }

    async fn apply_limits(&self, limits: &LimitsConfig) {
        self.rate_limit_runtime.update(RateLimitConfig {
            max_requests: limits.rate_limit,
            per_seconds: limits.rate_window,
        });
        self.provider_registry
            .write()
            .await
            .set_model_cache_ttl(model_cache_ttl_from_limits(limits));
        *self.concurrency_semaphore.write().await =
            Arc::new(Semaphore::new(limits.max_concurrency as usize));
        self.provider_concurrency_semaphores.lock().await.clear();
    }

    pub async fn get_or_create_provider(
        &self,
        provider_id: &str,
    ) -> Result<Arc<dyn Provider>, String> {
        if let Some(provider) = self.provider_registry.read().await.get(provider_id) {
            return Ok(provider);
        }

        let creation_lock = self.provider_creation_lock(provider_id).await;
        let _creation_guard = creation_lock.lock().await;

        if let Some(provider) = self.provider_registry.read().await.get(provider_id) {
            return Ok(provider);
        }

        let settings = self.settings.read().await.clone();
        let provider = match create_provider_from_settings(provider_id, &settings).await {
            Ok(provider) => provider,
            Err(error) => {
                self.record_provider_error(provider_id, &error).await;
                return Err(error);
            }
        };

        Ok(self
            .provider_registry
            .write()
            .await
            .insert_if_absent(provider_id, provider))
    }

    pub async fn get_or_refresh_models(&self, provider_id: &str) -> Result<Vec<ModelInfo>, String> {
        if let Some(models) = self
            .provider_registry
            .read()
            .await
            .cached_models(provider_id)
            .cloned()
        {
            return Ok(models);
        }

        let refresh_lock = self.model_refresh_lock(provider_id).await;
        let _refresh_guard = refresh_lock.lock().await;

        if let Some(models) = self
            .provider_registry
            .read()
            .await
            .cached_models(provider_id)
            .cloned()
        {
            return Ok(models);
        }

        let provider = self.get_or_create_provider(provider_id).await?;
        let models = match provider.list_models().await {
            Ok(models) => {
                self.record_provider_success(provider_id).await;
                models
            }
            Err(error) => {
                let message =
                    format!("failed to refresh model list for provider '{provider_id}': {error}");
                self.record_provider_error(provider_id, &message).await;
                return Err(message);
            }
        };

        self.provider_registry
            .write()
            .await
            .cache_models(provider_id, models.clone());

        Ok(models)
    }

    async fn provider_creation_lock(&self, provider_id: &str) -> Arc<Mutex<()>> {
        self.provider_creation_locks
            .lock()
            .await
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    async fn model_refresh_lock(&self, provider_id: &str) -> Arc<Mutex<()>> {
        self.model_refresh_locks
            .lock()
            .await
            .entry(provider_id.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub async fn record_provider_success(&self, provider_id: &str) {
        let now = unix_timestamp_secs();
        self.provider_health
            .lock()
            .await
            .entry(provider_id.to_string())
            .or_default()
            .mark_success(now);
    }

    pub async fn record_provider_error(&self, provider_id: &str, error: &str) {
        let now = unix_timestamp_secs();
        self.provider_health
            .lock()
            .await
            .entry(provider_id.to_string())
            .or_default()
            .mark_error(now, error);
    }

    pub async fn provider_health_snapshot(
        &self,
        provider_ids: impl IntoIterator<Item = String>,
    ) -> HashMap<String, ProviderHealth> {
        let mut snapshot = self.provider_health.lock().await.clone();
        for provider_id in provider_ids {
            snapshot.entry(provider_id).or_default();
        }
        snapshot
    }
}

fn model_cache_ttl_from_limits(limits: &LimitsConfig) -> Duration {
    Duration::from_secs(limits.model_cache_ttl_seconds.max(1))
}

fn sanitize_model_cache_ttl(ttl: Duration) -> Duration {
    ttl.max(Duration::from_secs(1))
}

fn unix_timestamp_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn truncate_provider_error(error: &str) -> String {
    if error.len() <= MAX_PROVIDER_HEALTH_ERROR_LEN {
        return error.to_string();
    }

    let mut truncated = error
        .chars()
        .take(MAX_PROVIDER_HEALTH_ERROR_LEN)
        .collect::<String>();
    truncated.push_str("...");
    truncated
}

async fn create_provider_from_settings(
    provider_id: &str,
    settings: &Settings,
) -> Result<Arc<dyn Provider>, String> {
    let provider_config = settings
        .providers
        .get(provider_id)
        .ok_or_else(|| format!("provider '{provider_id}' not configured"))?;

    claude_proxy_providers::create_provider(provider_id, provider_config, settings)
        .await
        .map_err(|e| format!("failed to create provider '{provider_id}': {e}"))
}
