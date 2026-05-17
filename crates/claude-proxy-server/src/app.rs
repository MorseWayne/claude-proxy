use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use claude_proxy_config::Settings;
use claude_proxy_providers::provider::Provider;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, Semaphore};

use crate::persistence::{MetricsStore, StoredTotals};

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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            capabilities["chatgpt/gpt-5.5"]["supported_endpoints"][0],
            "/responses"
        );
        assert_eq!(
            capabilities["chatgpt/gpt-5.5"]["reasoning_effort_levels"][1],
            "high"
        );
    }
}

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<RwLock<Settings>>,
    pub provider_registry: Arc<RwLock<ProviderRegistry>>,
    pub concurrency_semaphore: Arc<Semaphore>,
    pub provider_concurrency_semaphores: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    pub metrics: Arc<Metrics>,
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
    model_cache: std::collections::HashMap<String, Vec<claude_proxy_core::ModelInfo>>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self {
            providers: std::collections::HashMap::new(),
            model_cache: std::collections::HashMap::new(),
        }
    }

    /// Get or create a provider for the given ID.
    /// Returns an Arc clone so the caller can use the provider without holding the registry lock.
    pub async fn get_or_create(
        &mut self,
        provider_id: &str,
        settings: &Settings,
    ) -> Result<Arc<dyn Provider>, String> {
        if !self.providers.contains_key(provider_id) {
            let provider_config = settings
                .providers
                .get(provider_id)
                .ok_or_else(|| format!("provider '{provider_id}' not configured"))?;

            let provider: Arc<dyn Provider> =
                claude_proxy_providers::create_provider(provider_id, provider_config, settings)
                    .await
                    .map_err(|e| format!("failed to create provider '{provider_id}': {e}"))?;

            self.providers.insert(provider_id.to_string(), provider);
        }

        Ok(Arc::clone(self.providers.get(provider_id).unwrap()))
    }

    /// Cache model list for a provider.
    pub fn cache_models(&mut self, provider_id: &str, models: Vec<claude_proxy_core::ModelInfo>) {
        self.model_cache.insert(provider_id.to_string(), models);
    }

    /// Get cached models for a provider.
    pub fn cached_models(&self, provider_id: &str) -> Option<&Vec<claude_proxy_core::ModelInfo>> {
        self.model_cache.get(provider_id)
    }

    /// Get all cached models across all providers.
    pub fn all_cached_models(&self) -> Vec<claude_proxy_core::ModelInfo> {
        self.model_cache.values().flatten().cloned().collect()
    }

    pub fn model_capabilities(&self) -> Value {
        let capabilities = self
            .model_cache
            .iter()
            .flat_map(|(provider_id, models)| {
                models.iter().map(move |model| {
                    (
                        format!("{provider_id}/{}", model.model_id),
                        json!({
                            "provider": provider_id,
                            "model": model.model_id,
                            "vendor": model.vendor,
                            "max_output_tokens": model.max_output_tokens,
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
        let max_concurrency = settings.limits.max_concurrency as usize;
        Self {
            settings: Arc::new(RwLock::new(settings)),
            provider_registry: Arc::new(RwLock::new(ProviderRegistry::new())),
            concurrency_semaphore: Arc::new(Semaphore::new(max_concurrency)),
            provider_concurrency_semaphores: Arc::new(Mutex::new(HashMap::new())),
            metrics: Arc::new(Metrics::new(store)),
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}
