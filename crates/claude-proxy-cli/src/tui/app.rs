#![allow(dead_code)]
use std::collections::HashMap;

use claude_proxy_config::Settings;
use claude_proxy_config::settings::ProviderType;

// ── Navigation ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavItem {
    Dashboard,
    Providers,
    Server,
    Limits,
    Http,
    Log,
    Model,
    System,
}

impl NavItem {
    pub const ALL: &[NavItem] = &[
        NavItem::Dashboard,
        NavItem::Providers,
        NavItem::Server,
        NavItem::Limits,
        NavItem::Http,
        NavItem::Log,
        NavItem::Model,
        NavItem::System,
    ];

    pub fn name(&self) -> &'static str {
        match self {
            NavItem::Dashboard => "Dashboard",
            NavItem::Providers => "Providers",
            NavItem::Server => "Server",
            NavItem::Limits => "Limits",
            NavItem::Http => "HTTP",
            NavItem::Log => "Logging",
            NavItem::Model => "Model",
            NavItem::System => "System",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            NavItem::Dashboard => "◉",
            NavItem::Providers => "◆",
            NavItem::Server => "⬡",
            NavItem::Limits => "⏻",
            NavItem::Http => "↗",
            NavItem::Log => "☰",
            NavItem::Model => "◇",
            NavItem::System => "⚙",
        }
    }
}

// ── Focus ──

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Nav,
    Content,
    Overlay,
}

// ── Toast ──

#[derive(Debug, Clone)]
pub struct Toast {
    pub message: String,
    pub kind: ToastKind,
    pub remaining_ticks: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

impl Toast {
    pub fn info(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            kind: ToastKind::Info,
            remaining_ticks: 15,
        }
    }
    pub fn success(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            kind: ToastKind::Success,
            remaining_ticks: 15,
        }
    }
    pub fn warning(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            kind: ToastKind::Warning,
            remaining_ticks: 20,
        }
    }
    pub fn error(msg: impl Into<String>) -> Self {
        Self {
            message: msg.into(),
            kind: ToastKind::Error,
            remaining_ticks: 25,
        }
    }
}

// ── Overlay ──

#[derive(Debug, Clone)]
pub enum Overlay {
    Confirm(ConfirmOverlay),
    Input(InputOverlay),
    Picker(PickerOverlay),
    Loading(LoadingOverlay),
    OAuth(OAuthOverlay),
    Help,
}

#[derive(Debug, Clone)]
pub struct ConfirmOverlay {
    pub title: String,
    pub message: String,
    pub kind: ConfirmKind,
}

#[derive(Debug, Clone)]
pub enum ConfirmKind {
    /// [Enter] Yes  [Esc] No
    YesNo { on_yes: ConfirmAction },
    /// [Enter] Save & Quit  [n] Discard  [Esc] Cancel
    DirtyQuit,
    /// [Enter] Close
    Info,
}

#[derive(Debug, Clone)]
pub enum ConfirmAction {
    Quit,
    DeleteProvider(String),
    SaveAndQuit,
}

#[derive(Debug, Clone)]
pub struct PickerOverlay {
    pub title: String,
    pub items: Vec<String>,
    pub selected: usize,
    pub action: PickerAction,
}

#[derive(Debug, Clone)]
pub enum PickerAction {
    SetModelDefault {
        provider_id: String,
    },
    /// Step 1: pick provider → then fetch and show model picker
    PickProviderForModel {
        section: EditableSection,
    },
    /// Step 2: pick model → set the field
    SetModelField {
        provider_id: String,
        section: EditableSection,
    },
    /// Pick a log level value
    SetLogLevel,
    /// Add a new provider of the selected type
    AddProvider,
}

#[derive(Debug, Clone)]
pub struct LoadingOverlay {
    pub title: String,
    pub message: String,
    pub spinner_tick: u64,
}

/// OAuth device code authorization overlay.
#[derive(Debug, Clone)]
pub struct OAuthOverlay {
    pub provider_id: String,
    pub step: OAuthStep,
    pub spinner_tick: u64,
}

#[derive(Debug, Clone)]
pub enum OAuthStep {
    /// Requesting device code from GitHub...
    Requesting,
    /// Show the URL and user code to the user.
    ShowCode { url: String, code: String },
    /// Polling for user authorization.
    Polling,
    /// Authorization succeeded.
    Success,
    /// Authorization failed.
    Failed(String),
}

/// Result from a background OAuth operation.
pub enum OAuthResult {
    CodeInfo {
        url: String,
        code: String,
        device_code: String,
        interval: u64,
    },
    Token(String),
    Error(String),
}

#[derive(Debug, Clone)]
pub struct InputOverlay {
    pub title: String,
    pub prompt: String,
    pub value: String,
    pub cursor: usize,
    /// What to do with the input value on confirm
    pub action: InputAction,
}

impl InputOverlay {
    pub fn new(title: &str, prompt: &str, action: InputAction) -> Self {
        Self {
            title: title.into(),
            prompt: prompt.into(),
            value: String::new(),
            cursor: 0,
            action,
        }
    }

    pub fn with_value(mut self, value: &str) -> Self {
        let len = value.len();
        self.value = value.into();
        self.cursor = len;
        self
    }

    pub fn insert(&mut self, c: char) {
        self.value.insert(self.cursor, c);
        self.cursor += 1;
    }

    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            self.value.remove(self.cursor);
        }
    }
}

#[derive(Debug, Clone)]
pub enum InputAction {
    SetModelDefault {
        provider_id: String,
    },
    EditProviderField {
        provider_id: String,
        field: ProviderField,
        field_index: usize,
    },
    EditSetting {
        section: EditableSection,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderField {
    ApiKey,
    BaseUrl,
    Proxy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditableSection {
    ServerHost,
    ServerPort,
    ServerAuthToken,
    AdminAuthToken,
    RateLimit,
    RateWindow,
    MaxConcurrency,
    HttpReadTimeout,
    HttpWriteTimeout,
    HttpConnectTimeout,
    LogLevel,
    ModelDefault,
    ModelReasoning,
    ModelOpus,
    ModelSonnet,
    ModelHaiku,
}

// ── App State ──

/// Result of a background model fetch.
pub struct FetchResult {
    pub provider_id: String,
    pub models: Result<Vec<String>, String>,
}

/// Result of a background provider health check.
pub struct ProviderCheckResult {
    pub provider_id: String,
    pub result: Result<ProviderCheckOk, String>,
}

#[derive(Debug, Clone)]
pub struct ProviderCheckOk {
    pub message: String,
}

#[derive(Debug, Clone)]
pub enum ProviderCheckStatus {
    Checking,
    Ok(String),
    Warning(String),
    Failed(String),
}

/// Live metrics fetched from a running server.
#[derive(Debug, Clone, Default)]
pub struct LiveMetrics {
    pub requests_total: u64,
    pub errors_total: u64,
    pub avg_latency_ms: u64,
    pub models: Vec<(String, LiveModelMetrics)>,
    pub providers: Vec<(String, LiveModelMetrics)>,
    pub initiators: Vec<(String, LiveModelMetrics)>,
    pub model_capabilities: Vec<(String, ModelCapability)>,
    /// All-time stored totals (persisted across restarts).
    pub stored: Option<StoredMetrics>,
}

#[derive(Debug, Clone, Default)]
pub struct StoredMetrics {
    pub requests_total: u64,
    pub errors_total: u64,
    pub avg_latency_ms: u64,
    pub models: Vec<(String, LiveModelMetrics)>,
    pub providers: Vec<(String, LiveModelMetrics)>,
    pub initiators: Vec<(String, LiveModelMetrics)>,
}

#[derive(Debug, Clone, Default)]
pub struct ModelCapability {
    pub provider: String,
    pub vendor: Option<String>,
    pub max_output_tokens: Option<u64>,
    pub context_window: Option<u64>,
    pub supported_endpoints: Vec<String>,
    pub supports_thinking: Option<bool>,
    pub supports_vision: Option<bool>,
    pub supports_adaptive_thinking: Option<bool>,
    pub reasoning_effort_levels: Vec<String>,
}

/// Per-model metrics for display.
#[derive(Debug, Clone, Default)]
pub struct LiveModelMetrics {
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

impl LiveModelMetrics {
    pub fn total_tokens(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

/// Sub-focus for the Providers page: list vs detail pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderFocus {
    List,
    Detail,
}

pub struct App {
    pub settings: Settings,
    pub nav: NavItem,
    pub focus: Focus,
    pub nav_idx: usize,
    pub content_idx: usize,
    /// Which field is selected inside the provider detail pane.
    pub detail_idx: usize,
    /// Whether the provider page focuses the list or the detail pane.
    pub provider_focus: ProviderFocus,
    pub dirty: bool,
    pub overlay: Option<Overlay>,
    pub toast: Option<Toast>,
    pub should_quit: bool,
    pub tick: u64,
    /// Channel to receive model fetch results from background thread.
    pub fetch_rx: Option<std::sync::mpsc::Receiver<FetchResult>>,
    /// When fetching models for Model page editing — which field to set.
    pub pending_model_section: Option<EditableSection>,
    /// Tokio runtime handle for async provider calls from background threads.
    pub tokio_handle: Option<tokio::runtime::Handle>,
    /// Live metrics fetched from the running server.
    pub live_metrics: Option<LiveMetrics>,
    /// Channel for receiving metrics fetch results from background thread.
    pub metrics_rx: Option<std::sync::mpsc::Receiver<Option<serde_json::Value>>>,
    /// Tick counter for metrics refresh (every N ticks).
    pub metrics_fetch_tick: u64,
    /// Pending provider types for the AddProvider picker.
    pub pending_provider_types: Option<Vec<ProviderType>>,
    /// Channel for OAuth background thread results.
    pub oauth_rx: Option<std::sync::mpsc::Receiver<OAuthResult>>,
    /// The provider ID currently undergoing OAuth.
    pub oauth_pending_id: Option<String>,
    /// Stashed device code info for polling phase.
    pub oauth_device_info: Option<(String, u64)>,
    /// Channel for provider connectivity/auth check results.
    pub provider_check_rx: Option<std::sync::mpsc::Receiver<ProviderCheckResult>>,
    /// Last known provider connectivity/auth status by provider ID.
    pub provider_statuses: HashMap<String, ProviderCheckStatus>,
}

impl App {
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            nav: NavItem::Dashboard,
            focus: Focus::Nav,
            nav_idx: 0,
            content_idx: 0,
            detail_idx: 0,
            provider_focus: ProviderFocus::List,
            dirty: false,
            overlay: None,
            toast: None,
            should_quit: false,
            tick: 0,
            fetch_rx: None,
            pending_model_section: None,
            tokio_handle: tokio::runtime::Handle::try_current().ok(),
            live_metrics: None,
            metrics_rx: None,
            metrics_fetch_tick: 0,
            pending_provider_types: None,
            oauth_rx: None,
            oauth_pending_id: None,
            oauth_device_info: None,
            provider_check_rx: None,
            provider_statuses: HashMap::new(),
        }
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn show_toast(&mut self, toast: Toast) {
        self.toast = Some(toast);
    }

    pub fn item_count(&self) -> usize {
        match self.nav {
            NavItem::Dashboard => 0,
            NavItem::Providers => self.settings.providers.len(),
            NavItem::Server => 4,
            NavItem::Limits => 3,
            NavItem::Http => 3,
            NavItem::Log => 3,
            NavItem::Model => 5,
            NavItem::System => 0, // read-only info page
        }
    }

    /// Number of editable fields in the provider detail pane.
    pub fn provider_detail_field_count(&self) -> usize {
        3 // API Key, Base URL, Proxy
    }

    pub fn clamp_content_idx(&mut self) {
        let count = self.item_count();
        if count > 0 && self.content_idx >= count {
            self.content_idx = count.saturating_sub(1);
        }
        let detail_max = self.provider_detail_field_count();
        if self.detail_idx >= detail_max {
            self.detail_idx = detail_max.saturating_sub(1);
        }
    }
}
