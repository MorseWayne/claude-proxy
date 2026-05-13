use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use claude_proxy_config::Settings;
use claude_proxy_providers::provider::Provider;
use serde_json::json;
use tokio::sync::{RwLock, Semaphore};

/// Request metrics counters.
pub struct Metrics {
    pub requests_total: AtomicU64,
    pub errors_total: AtomicU64,
    pub latency_sum_ms: AtomicU64,
    pub latency_count: AtomicU64,
}

impl Metrics {
    pub fn new() -> Self {
        Self {
            requests_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            latency_sum_ms: AtomicU64::new(0),
            latency_count: AtomicU64::new(0),
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

    pub fn to_json(&self) -> serde_json::Value {
        let requests = self.requests_total.load(Ordering::Relaxed);
        let errors = self.errors_total.load(Ordering::Relaxed);
        let latency_sum = self.latency_sum_ms.load(Ordering::Relaxed);
        let latency_count = self.latency_count.load(Ordering::Relaxed);
        let avg_latency = latency_sum.checked_div(latency_count).unwrap_or(0);

        json!({
            "requests_total": requests,
            "errors_total": errors,
            "avg_latency_ms": avg_latency,
        })
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Shared application state.
#[derive(Clone)]
pub struct AppState {
    pub settings: Arc<RwLock<Settings>>,
    pub provider_registry: Arc<RwLock<ProviderRegistry>>,
    pub concurrency_semaphore: Arc<Semaphore>,
    pub metrics: Arc<Metrics>,
}

/// Registry of provider instances and cached model lists.
pub struct ProviderRegistry {
    providers: std::collections::HashMap<String, Box<dyn Provider>>,
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
    pub async fn get_or_create(
        &mut self,
        provider_id: &str,
        settings: &Settings,
    ) -> Result<&dyn Provider, String> {
        if !self.providers.contains_key(provider_id) {
            let provider_config = settings
                .providers
                .get(provider_id)
                .ok_or_else(|| format!("provider '{provider_id}' not configured"))?;

            let provider: Box<dyn Provider> =
                claude_proxy_providers::create_provider(provider_id, provider_config, settings)
                    .await
                    .map_err(|e| format!("failed to create provider '{provider_id}': {e}"))?;

            self.providers.insert(provider_id.to_string(), provider);
        }

        Ok(self.providers.get(provider_id).unwrap().as_ref())
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

    /// Clear all providers (on config reload).
    pub fn clear(&mut self) {
        self.providers.clear();
        self.model_cache.clear();
    }
}

impl AppState {
    pub fn new(settings: Settings) -> Self {
        let max_concurrency = settings.limits.max_concurrency as usize;
        Self {
            settings: Arc::new(RwLock::new(settings)),
            provider_registry: Arc::new(RwLock::new(ProviderRegistry::new())),
            concurrency_semaphore: Arc::new(Semaphore::new(max_concurrency)),
            metrics: Arc::new(Metrics::new()),
        }
    }
}
