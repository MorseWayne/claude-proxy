use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use claude_proxy_config::Settings;
use claude_proxy_config::settings::LimitsConfig;
use claude_proxy_core::ModelInfo;
use claude_proxy_providers::provider::{Provider, UpstreamErrorMetadata};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock, Semaphore};

use crate::middleware::{RateLimitConfig, RateLimitRuntime};
use crate::persistence::{CompletedUsageRecord, MetricsStore, StoredTotals};

const DEFAULT_MODEL_CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const MAX_PROVIDER_HEALTH_ERROR_LEN: usize = 2048;
const RECENT_OBSERVABILITY_LIMIT: usize = 20;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestPayloadStats {
    pub messages: u64,
    pub content_blocks: u64,
    pub tool_results: u64,
    pub text_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestObservabilityEvent {
    pub request_id: String,
    pub provider: String,
    pub initiator: String,
    pub model: String,
    pub stream: bool,
    pub is_error: bool,
    pub terminal_reason: String,
    pub total_latency_ms: u64,
    pub provider_setup_ms: u64,
    pub upstream_connect_ms: u64,
    pub stream_duration_ms: u64,
    pub first_event_ms: Option<u64>,
    pub last_event_gap_ms: u64,
    pub max_event_gap_ms: u64,
    pub idle_gap_count: u64,
    pub event_count: u64,
    pub prompt_too_long_retries: u64,
    pub prompt_too_long_original_body_bytes: u64,
    pub prompt_too_long_shrunk_body_bytes: u64,
    pub prompt_too_long_dropped_items: u64,
    pub request_messages: u64,
    pub request_content_blocks: u64,
    pub request_tool_results: u64,
    pub request_text_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestObservabilitySummary {
    pub requests: u64,
    pub errors: u64,
    pub avg_total_latency_ms: u64,
    pub avg_upstream_connect_ms: u64,
    pub max_event_gap_ms: u64,
    pub idle_gap_count: u64,
    pub prompt_too_long_retries: u64,
}

impl RequestObservabilitySummary {
    fn add_event(&mut self, event: &RequestObservabilityEvent) {
        self.requests += 1;
        self.errors += event.is_error as u64;
        self.avg_total_latency_ms += event.total_latency_ms;
        self.avg_upstream_connect_ms += event.upstream_connect_ms;
        self.max_event_gap_ms = self.max_event_gap_ms.max(event.max_event_gap_ms);
        self.idle_gap_count += event.idle_gap_count;
        self.prompt_too_long_retries += event.prompt_too_long_retries;
    }

    pub(crate) fn finalize(&mut self) {
        if self.requests == 0 {
            return;
        }
        self.avg_total_latency_ms /= self.requests;
        self.avg_upstream_connect_ms /= self.requests;
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestObservabilitySnapshot {
    pub summary: RequestObservabilitySummary,
    pub recent: Vec<RequestObservabilityEvent>,
    pub stored: Option<RequestObservabilityStored>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestObservabilityStored {
    pub summary: RequestObservabilitySummary,
    pub recent: Vec<RequestObservabilityEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActiveStreamSnapshot {
    pub request_id: String,
    pub provider: String,
    pub initiator: String,
    pub model: String,
    pub age_ms: u64,
    pub idle_ms: u64,
    pub event_count: u64,
    pub last_event_type: String,
    pub tool_use_pending: bool,
}

#[derive(Debug, Clone)]
struct ActiveStreamState {
    provider: String,
    initiator: String,
    model: String,
    started_at: Instant,
    last_event_at: Instant,
    event_count: u64,
    last_event_type: String,
    tool_use_pending: bool,
}

impl ActiveStreamState {
    fn snapshot(&self, request_id: &str, now: Instant) -> ActiveStreamSnapshot {
        ActiveStreamSnapshot {
            request_id: request_id.to_string(),
            provider: self.provider.clone(),
            initiator: self.initiator.clone(),
            model: self.model.clone(),
            age_ms: now.saturating_duration_since(self.started_at).as_millis() as u64,
            idle_ms: now
                .saturating_duration_since(self.last_event_at)
                .as_millis() as u64,
            event_count: self.event_count,
            last_event_type: self.last_event_type.clone(),
            tool_use_pending: self.tool_use_pending,
        }
    }
}

#[derive(Debug, Default)]
pub struct RequestObservabilityMetrics {
    summary: RequestObservabilitySummary,
    recent: VecDeque<RequestObservabilityEvent>,
}

impl RequestObservabilityMetrics {
    fn add_event(&mut self, event: RequestObservabilityEvent) {
        self.summary.add_event(&event);
        if self.recent.len() >= RECENT_OBSERVABILITY_LIMIT {
            self.recent.pop_front();
        }
        self.recent.push_back(event);
    }

    fn snapshot(&self) -> RequestObservabilitySnapshot {
        let mut summary = self.summary.clone();
        summary.finalize();
        RequestObservabilitySnapshot {
            summary,
            recent: self.recent.iter().cloned().collect(),
            stored: None,
        }
    }
}

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
        self.input_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
            + self.output_tokens
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
        self.input_tokens
            + self.cache_creation_input_tokens
            + self.cache_read_input_tokens
            + self.output_tokens
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

#[derive(Debug, Default)]
pub struct MetricsDimensions {
    pub models: HashMap<String, UsageMetrics>,
    pub providers: HashMap<String, UsageMetrics>,
    pub initiators: HashMap<String, UsageMetrics>,
}

impl MetricsDimensions {
    fn add_usage(&mut self, provider: &str, initiator: &str, model: &str, usage: &TokenUsage) {
        self.models
            .entry(model.to_string())
            .or_default()
            .add_usage(usage);
        self.providers
            .entry(provider.to_string())
            .or_default()
            .add_usage(usage);
        self.initiators
            .entry(initiator.to_string())
            .or_default()
            .add_usage(usage);
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ErrorDiagnostics {
    pub errors: u64,
    pub terminal_reasons: HashMap<String, u64>,
    pub error_kinds: HashMap<String, u64>,
}

impl ErrorDiagnostics {
    pub fn record(&mut self, is_error: bool, terminal_reason: &str, error_kind: &str) {
        if !is_error {
            return;
        }
        self.errors += 1;
        if !terminal_reason.is_empty() {
            *self
                .terminal_reasons
                .entry(terminal_reason.to_string())
                .or_default() += 1;
        }
        if !error_kind.is_empty() {
            *self.error_kinds.entry(error_kind.to_string()).or_default() += 1;
        }
    }
}

/// Request metrics counters.
pub struct Metrics {
    pub requests_total: AtomicU64,
    pub errors_total: AtomicU64,
    pub latency_sum_ms: AtomicU64,
    pub latency_count: AtomicU64,
    /// Token usage metrics for the current session.
    pub usage_metrics: Mutex<MetricsDimensions>,
    pub observability_metrics: Mutex<RequestObservabilityMetrics>,
    pub error_diagnostics: Mutex<ErrorDiagnostics>,
    active_streams: Mutex<HashMap<String, ActiveStreamState>>,
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
            usage_metrics: Mutex::new(MetricsDimensions::default()),
            observability_metrics: Mutex::new(RequestObservabilityMetrics::default()),
            error_diagnostics: Mutex::new(ErrorDiagnostics::default()),
            active_streams: Mutex::new(HashMap::new()),
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
        let mut dimensions = self.usage_metrics.lock().await;
        dimensions
            .models
            .entry(model.to_string())
            .or_default()
            .add_usage(usage);
    }

    async fn record_usage_dimensions(
        &self,
        provider: &str,
        initiator: &str,
        model: &str,
        usage: &TokenUsage,
    ) {
        self.usage_metrics
            .lock()
            .await
            .add_usage(provider, initiator, model, usage);
    }

    /// Record a completed request with usage, persisting to store if available.
    pub async fn record_completed_request(&self, record: CompletedUsageRecord<'_>) {
        self.record_usage_dimensions(
            record.provider,
            record.initiator,
            record.model,
            record.usage,
        )
        .await;
        self.error_diagnostics.lock().await.record(
            record.is_error,
            record.terminal_reason,
            record.error_kind,
        );
        if let Some(ref store) = self.store {
            store.record_usage(record);
        }
    }

    /// Load stored totals from persistence (called once at startup).
    pub async fn load_stored_totals(&self) {
        if let Some(ref store) = self.store {
            let totals = store.load_totals().await;
            *self.stored_totals.lock().await = totals;
        }
    }

    pub async fn record_observability(&self, event: RequestObservabilityEvent, persist: bool) {
        self.observability_metrics
            .lock()
            .await
            .add_event(event.clone());
        if persist && let Some(ref store) = self.store {
            store.record_observability(event);
        }
    }

    pub async fn load_stored_observability(&self) -> Option<RequestObservabilityStored> {
        match &self.store {
            Some(store) => Some(store.load_observability().await),
            None => None,
        }
    }

    pub async fn register_active_stream(
        &self,
        request_id: String,
        provider: String,
        initiator: String,
        model: String,
    ) {
        let now = Instant::now();
        self.active_streams.lock().await.insert(
            request_id,
            ActiveStreamState {
                provider,
                initiator,
                model,
                started_at: now,
                last_event_at: now,
                event_count: 0,
                last_event_type: "registered".to_string(),
                tool_use_pending: false,
            },
        );
    }

    pub async fn update_active_stream(
        &self,
        request_id: &str,
        event_type: String,
        tool_use_pending: bool,
    ) {
        if let Some(stream) = self.active_streams.lock().await.get_mut(request_id) {
            stream.last_event_at = Instant::now();
            stream.event_count += 1;
            stream.last_event_type = event_type;
            stream.tool_use_pending = tool_use_pending;
        }
    }

    pub async fn remove_active_stream(&self, request_id: &str) {
        self.active_streams.lock().await.remove(request_id);
    }

    async fn active_stream_snapshots(&self) -> Vec<ActiveStreamSnapshot> {
        let now = Instant::now();
        self.active_streams
            .lock()
            .await
            .iter()
            .map(|(request_id, stream)| stream.snapshot(request_id, now))
            .collect()
    }

    pub async fn to_json(&self) -> serde_json::Value {
        let requests = self.requests_total.load(Ordering::Relaxed);
        let errors = self.errors_total.load(Ordering::Relaxed);
        let latency_sum = self.latency_sum_ms.load(Ordering::Relaxed);
        let latency_count = self.latency_count.load(Ordering::Relaxed);
        let avg_latency = latency_sum.checked_div(latency_count).unwrap_or(0);

        let usage_metrics = self.usage_metrics.lock().await;
        let models: serde_json::Value =
            serde_json::to_value(&usage_metrics.models).unwrap_or_default();
        let providers: serde_json::Value =
            serde_json::to_value(&usage_metrics.providers).unwrap_or_default();
        let initiators: serde_json::Value =
            serde_json::to_value(&usage_metrics.initiators).unwrap_or_default();
        drop(usage_metrics);

        let diagnostics: serde_json::Value =
            serde_json::to_value(self.error_diagnostics.lock().await.clone()).unwrap_or_default();

        let stored = self.stored_totals.lock().await;
        let stored_requests_total = stored.requests_total;
        let stored_errors_total = stored.errors_total;
        let stored_avg_latency_ms = stored
            .latency_sum_ms
            .checked_div(stored.latency_count)
            .unwrap_or(0);
        let stored_models: serde_json::Value =
            serde_json::to_value(&stored.model_metrics).unwrap_or_default();
        let stored_providers: serde_json::Value =
            serde_json::to_value(&stored.provider_metrics).unwrap_or_default();
        let stored_initiators: serde_json::Value =
            serde_json::to_value(&stored.initiator_metrics).unwrap_or_default();
        let stored_diagnostics: serde_json::Value =
            serde_json::to_value(&stored.error_diagnostics).unwrap_or_default();
        drop(stored);

        let mut observability = self.observability_metrics.lock().await.snapshot();
        observability.stored = self.load_stored_observability().await;
        let active_streams = self.active_stream_snapshots().await;
        let active_stream_count = active_streams.len();

        json!({
            "requests_total": requests,
            "errors_total": errors,
            "avg_latency_ms": avg_latency,
            "models": models,
            "providers": providers,
            "initiators": initiators,
            "diagnostics": diagnostics,
            "observability": serde_json::to_value(observability).unwrap_or_default(),
            "active_streams": {
                "count": active_stream_count,
                "streams": active_streams,
            },
            "stored": {
                "requests_total": stored_requests_total,
                "errors_total": stored_errors_total,
                "avg_latency_ms": stored_avg_latency_ms,
                "models": stored_models,
                "providers": stored_providers,
                "initiators": stored_initiators,
                "diagnostics": stored_diagnostics,
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
    #[serde(default)]
    pub last_error_metadata: Option<UpstreamErrorMetadata>,
}

impl ProviderHealth {
    fn mark_success(&mut self, timestamp: u64) {
        self.status = ProviderHealthStatus::Healthy;
        self.last_ok_unix_secs = Some(timestamp);
    }

    fn mark_error(&mut self, timestamp: u64, error: &str, metadata: Option<UpstreamErrorMetadata>) {
        self.status = ProviderHealthStatus::Unhealthy;
        self.last_error_unix_secs = Some(timestamp);
        self.last_error = Some(truncate_provider_error(error));
        self.last_error_metadata = metadata;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_proxy_config::settings::{
        AdminConfig, HttpConfig, LogConfig, ModelAliasConfig, ModelConfig, ObservabilityConfig,
        ProviderConfig, ProviderType, ServerConfig,
    };
    use claude_proxy_core::{
        CapabilityState, EndpointCapabilities, FeatureCapabilities, InputModalities,
        ModalityCapabilities, ModelCapabilities, ModelLimits,
    };

    fn model_capability_fixture(model_id: &str) -> ModelInfo {
        ModelInfo {
            model_id: model_id.to_string(),
            vendor: Some("openai".to_string()),
            is_chat_default: None,
            capabilities: ModelCapabilities {
                endpoints: EndpointCapabilities {
                    anthropic_messages: CapabilityState::Unsupported,
                    openai_chat_completions: CapabilityState::Unsupported,
                    openai_responses: CapabilityState::Supported,
                },
                modalities: ModalityCapabilities {
                    input: InputModalities {
                        image: CapabilityState::Supported,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                features: FeatureCapabilities {
                    thinking: CapabilityState::Supported,
                    adaptive_thinking: CapabilityState::Unsupported,
                    reasoning_effort: CapabilityState::Supported,
                    ..Default::default()
                },
                limits: ModelLimits {
                    max_output_tokens: Some(128_000),
                    context_window: Some(400_000),
                    min_thinking_budget: Some(1024),
                    max_thinking_budget: Some(32_000),
                    reasoning_effort_levels: vec!["low".to_string(), "high".to_string()],
                },
                supported_parameters: vec!["messages".to_string(), "thinking".to_string()],
            },
        }
    }

    fn basic_model(model_id: &str) -> ModelInfo {
        ModelInfo {
            model_id: model_id.to_string(),
            vendor: None,
            is_chat_default: None,
            capabilities: ModelCapabilities::default(),
        }
    }

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
                chatgpt: None,
                runtime: Default::default(),
                reasoning_markers: Default::default(),
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
                ..ServerConfig::default()
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
            observability: ObservabilityConfig::default(),
        }
    }

    #[test]
    fn total_tokens_includes_cache_token_components() {
        let metrics = ModelMetrics {
            requests: 1,
            input_tokens: 100,
            output_tokens: 25,
            cache_creation_input_tokens: 40,
            cache_read_input_tokens: 60,
        };

        assert_eq!(metrics.total_tokens(), 225);
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
            .record_completed_request(CompletedUsageRecord {
                provider: "chatgpt",
                initiator: "agent",
                model: "gpt-5.5",
                usage: &usage,
                is_error: true,
                latency_ms: 123,
                terminal_reason: "stream_error",
                error_kind: "stream",
            })
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
        assert_eq!(data["diagnostics"]["errors"], 1);
        assert_eq!(data["diagnostics"]["terminal_reasons"]["stream_error"], 1);
        assert_eq!(data["diagnostics"]["error_kinds"]["stream"], 1);
    }

    #[tokio::test]
    async fn metrics_snapshot_reports_active_streams_without_prompt_content() {
        let metrics = Metrics::default();
        metrics
            .register_active_stream(
                "req-1".to_string(),
                "chatgpt".to_string(),
                "user".to_string(),
                "gpt-5.5".to_string(),
            )
            .await;
        metrics
            .update_active_stream("req-1", "content_block_start".to_string(), true)
            .await;

        let data = metrics.to_json().await;
        assert_eq!(data["active_streams"]["count"], 1);
        let stream = &data["active_streams"]["streams"][0];
        assert_eq!(stream["request_id"], "req-1");
        assert_eq!(stream["provider"], "chatgpt");
        assert_eq!(stream["model"], "gpt-5.5");
        assert_eq!(stream["last_event_type"], "content_block_start");
        assert_eq!(stream["tool_use_pending"], true);
        assert!(stream.get("prompt").is_none());

        metrics.remove_active_stream("req-1").await;
        let data = metrics.to_json().await;
        assert_eq!(data["active_streams"]["count"], 0);
    }

    #[tokio::test]
    async fn metrics_snapshot_handles_concurrent_completed_requests() {
        let metrics = Arc::new(Metrics::default());
        let usage = TokenUsage {
            input_tokens: 2,
            output_tokens: 3,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        };

        let mut tasks = Vec::new();
        for _ in 0..32 {
            let metrics = metrics.clone();
            let usage = usage.clone();
            tasks.push(tokio::spawn(async move {
                metrics
                    .record_completed_request(CompletedUsageRecord {
                        provider: "openai",
                        initiator: "user",
                        model: "gpt-4.1",
                        usage: &usage,
                        is_error: false,
                        latency_ms: 10,
                        terminal_reason: "completed",
                        error_kind: "",
                    })
                    .await;
            }));
        }

        let snapshot_task = {
            let metrics = metrics.clone();
            tokio::spawn(async move { metrics.to_json().await })
        };

        for task in tasks {
            task.await.unwrap();
        }
        let _ = snapshot_task.await.unwrap();
        let data = metrics.to_json().await;

        assert_eq!(data["models"]["gpt-4.1"]["requests"], 32);
        assert_eq!(data["providers"]["openai"]["input_tokens"], 64);
        assert_eq!(data["initiators"]["user"]["output_tokens"], 96);
    }

    #[tokio::test]
    async fn observability_summary_and_recent_are_reported() {
        let metrics = Metrics::default();

        metrics
            .record_observability(
                RequestObservabilityEvent {
                    request_id: "first".to_string(),
                    provider: "chatgpt".to_string(),
                    initiator: "user".to_string(),
                    model: "gpt-5.5".to_string(),
                    stream: true,
                    is_error: false,
                    terminal_reason: "completed".to_string(),
                    total_latency_ms: 100,
                    provider_setup_ms: 10,
                    upstream_connect_ms: 20,
                    stream_duration_ms: 70,
                    first_event_ms: Some(30),
                    last_event_gap_ms: 5,
                    max_event_gap_ms: 15,
                    idle_gap_count: 0,
                    event_count: 3,
                    prompt_too_long_retries: 1,
                    prompt_too_long_original_body_bytes: 200,
                    prompt_too_long_shrunk_body_bytes: 120,
                    prompt_too_long_dropped_items: 2,
                    request_messages: 2,
                    request_content_blocks: 3,
                    request_tool_results: 1,
                    request_text_bytes: 42,
                },
                false,
            )
            .await;
        metrics
            .record_observability(
                RequestObservabilityEvent {
                    request_id: "second".to_string(),
                    provider: "chatgpt".to_string(),
                    initiator: "agent".to_string(),
                    model: "gpt-5.5".to_string(),
                    stream: false,
                    is_error: true,
                    terminal_reason: "provider_error".to_string(),
                    total_latency_ms: 300,
                    provider_setup_ms: 10,
                    upstream_connect_ms: 40,
                    stream_duration_ms: 0,
                    first_event_ms: None,
                    last_event_gap_ms: 0,
                    max_event_gap_ms: 0,
                    idle_gap_count: 1,
                    event_count: 0,
                    prompt_too_long_retries: 0,
                    prompt_too_long_original_body_bytes: 0,
                    prompt_too_long_shrunk_body_bytes: 0,
                    prompt_too_long_dropped_items: 0,
                    request_messages: 1,
                    request_content_blocks: 1,
                    request_tool_results: 0,
                    request_text_bytes: 10,
                },
                false,
            )
            .await;

        let data = metrics.to_json().await;

        assert_eq!(data["observability"]["summary"]["requests"], 2);
        assert_eq!(data["observability"]["summary"]["errors"], 1);
        assert_eq!(
            data["observability"]["summary"]["avg_total_latency_ms"],
            200
        );
        assert_eq!(
            data["observability"]["summary"]["avg_upstream_connect_ms"],
            30
        );
        assert_eq!(data["observability"]["summary"]["max_event_gap_ms"], 15);
        assert_eq!(data["observability"]["summary"]["idle_gap_count"], 1);
        assert_eq!(
            data["observability"]["summary"]["prompt_too_long_retries"],
            1
        );
        assert_eq!(data["observability"]["recent"].as_array().unwrap().len(), 2);
        assert!(data["observability"]["stored"].is_null());
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
        assert!(snapshot["openai"].last_error_metadata.is_none());

        state
            .record_provider_error_with_metadata(
                "openai",
                "rate limited",
                Some(UpstreamErrorMetadata {
                    status: 429,
                    retry_after: Some(5),
                    request_id: Some("req_123".to_string()),
                    message: Some("slow down".to_string()),
                    body_preview: None,
                    headers: Vec::new(),
                }),
            )
            .await;
        let snapshot = state
            .provider_health_snapshot(vec!["openai".to_string()])
            .await;
        let metadata = snapshot["openai"].last_error_metadata.as_ref().unwrap();
        assert_eq!(metadata.status, 429);
        assert_eq!(metadata.retry_after, Some(5));
        assert_eq!(metadata.request_id.as_deref(), Some("req_123"));

        state.record_provider_success("openai").await;
        let snapshot = state
            .provider_health_snapshot(vec!["openai".to_string()])
            .await;
        assert_eq!(snapshot["openai"].status, ProviderHealthStatus::Healthy);
        assert_eq!(
            snapshot["openai"].last_error.as_deref(),
            Some("rate limited")
        );
        assert!(snapshot["openai"].last_ok_unix_secs.is_some());
    }

    #[test]
    fn provider_registry_exports_model_capabilities() {
        let mut registry = ProviderRegistry::new();
        registry.cache_models("chatgpt", vec![model_capability_fixture("gpt-5.5")]);

        let capabilities = registry.model_capabilities();

        assert_eq!(capabilities["chatgpt/gpt-5.5"]["provider"], "chatgpt");
        assert_eq!(capabilities["chatgpt/gpt-5.5"]["model"], "gpt-5.5");
        assert_eq!(
            capabilities["chatgpt/gpt-5.5"]["capabilities"]["limits"]["max_output_tokens"],
            128_000
        );
        assert_eq!(
            capabilities["chatgpt/gpt-5.5"]["capabilities"]["limits"]["context_window"],
            400_000
        );
        assert_eq!(
            capabilities["chatgpt/gpt-5.5"]["capabilities"]["endpoints"]["openai_responses"],
            "supported"
        );
        assert_eq!(
            capabilities["chatgpt/gpt-5.5"]["capabilities"]["limits"]["reasoning_effort_levels"][1],
            "high"
        );
    }

    #[test]
    fn provider_registry_exports_model_cache_status() {
        let mut registry = ProviderRegistry::with_model_cache_ttl(Duration::from_secs(60));
        let cached_at = Instant::now() - Duration::from_secs(20);
        registry.cache_models_at(
            "chatgpt",
            vec![basic_model("gpt-5.5"), basic_model("gpt-5-mini")],
            cached_at,
        );

        let statuses = registry.model_cache_status_at(
            &["chatgpt".to_string()],
            cached_at + Duration::from_secs(20),
        );

        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].provider, "chatgpt");
        assert!(statuses[0].cached);
        assert_eq!(statuses[0].model_count, 2);
        assert!(statuses[0].fresh);
        assert_eq!(statuses[0].age_secs, Some(20));
        assert_eq!(statuses[0].ttl_secs, 60);
        assert_eq!(statuses[0].expires_in_secs, Some(40));
    }

    #[test]
    fn provider_registry_marks_stale_model_cache_status() {
        let mut registry = ProviderRegistry::with_model_cache_ttl(Duration::from_secs(60));
        let cached_at = Instant::now() - Duration::from_secs(90);
        registry.cache_models_at("openai", vec![basic_model("stale-model")], cached_at);

        let statuses = registry
            .model_cache_status_at(&["openai".to_string()], cached_at + Duration::from_secs(90));

        assert_eq!(statuses[0].provider, "openai");
        assert!(statuses[0].cached);
        assert!(!statuses[0].fresh);
        assert_eq!(statuses[0].age_secs, Some(90));
        assert_eq!(statuses[0].ttl_secs, 60);
        assert_eq!(statuses[0].expires_in_secs, Some(0));
    }

    #[test]
    fn provider_registry_reports_uncached_model_cache_status() {
        let registry = ProviderRegistry::with_model_cache_ttl(Duration::from_secs(60));

        let statuses = registry.model_cache_status_at(&["openai".to_string()], Instant::now());

        assert_eq!(statuses[0].provider, "openai");
        assert!(!statuses[0].cached);
        assert_eq!(statuses[0].model_count, 0);
        assert!(!statuses[0].fresh);
        assert_eq!(statuses[0].age_secs, None);
        assert_eq!(statuses[0].ttl_secs, 60);
        assert_eq!(statuses[0].expires_in_secs, None);
    }

    #[test]
    fn provider_registry_treats_expired_models_as_stale() {
        let mut registry = ProviderRegistry::new();
        registry.cache_models_at(
            "openai",
            vec![basic_model("stale-model")],
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
            vec![basic_model("still-fresh")],
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
        state
            .provider_registry
            .write()
            .await
            .cache_models("openai", vec![basic_model("gpt-4.1")]);

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
                chatgpt: None,
                runtime: Default::default(),
                reasoning_markers: Default::default(),
            },
        );
        let state = AppState::new(settings, None);
        state.provider_registry.write().await.cache_models_at(
            "anthropic",
            vec![basic_model("stale-model")],
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelCacheStatus {
    pub provider: String,
    pub cached: bool,
    pub model_count: usize,
    pub fresh: bool,
    pub age_secs: Option<u64>,
    pub ttl_secs: u64,
    pub expires_in_secs: Option<u64>,
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
        self.age_at(now) < ttl
    }

    fn age_at(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.cached_at)
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

    pub fn model_cache_status(&self, provider_ids: &[String]) -> Vec<ModelCacheStatus> {
        self.model_cache_status_at(provider_ids, Instant::now())
    }

    fn model_cache_status_at(
        &self,
        provider_ids: &[String],
        now: Instant,
    ) -> Vec<ModelCacheStatus> {
        let ttl = self.model_cache_ttl;
        let mut statuses = provider_ids
            .iter()
            .map(|provider_id| {
                let Some(entry) = self.model_cache.get(provider_id) else {
                    return ModelCacheStatus {
                        provider: provider_id.clone(),
                        cached: false,
                        model_count: 0,
                        fresh: false,
                        age_secs: None,
                        ttl_secs: ttl.as_secs(),
                        expires_in_secs: None,
                    };
                };

                let age = entry.age_at(now);
                let fresh = entry.is_fresh_at(now, ttl);
                ModelCacheStatus {
                    provider: provider_id.clone(),
                    cached: true,
                    model_count: entry.models.len(),
                    fresh,
                    age_secs: Some(age.as_secs()),
                    ttl_secs: ttl.as_secs(),
                    expires_in_secs: Some(ttl.saturating_sub(age).as_secs()),
                }
            })
            .collect::<Vec<_>>();
        statuses.sort_by(|a, b| a.provider.cmp(&b.provider));
        statuses
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
                            "is_chat_default": model.is_chat_default,
                            "supported_endpoints": model.capabilities.endpoints.supported_paths(),
                            "capabilities": model.capabilities,
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

        self.fetch_and_cache_models(provider_id).await
    }

    pub async fn refresh_models(&self, provider_id: &str) -> Result<Vec<ModelInfo>, String> {
        let refresh_lock = self.model_refresh_lock(provider_id).await;
        let _refresh_guard = refresh_lock.lock().await;

        self.fetch_and_cache_models(provider_id).await
    }

    async fn fetch_and_cache_models(&self, provider_id: &str) -> Result<Vec<ModelInfo>, String> {
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
        self.record_provider_error_with_metadata(provider_id, error, None)
            .await;
    }

    pub async fn record_provider_error_with_metadata(
        &self,
        provider_id: &str,
        error: &str,
        metadata: Option<UpstreamErrorMetadata>,
    ) {
        let now = unix_timestamp_secs();
        self.provider_health
            .lock()
            .await
            .entry(provider_id.to_string())
            .or_default()
            .mark_error(now, error, metadata);
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
