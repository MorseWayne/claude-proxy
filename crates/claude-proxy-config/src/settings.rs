use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ConfigError;

/// Top-level configuration loaded from TOML.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,

    #[serde(default)]
    pub model: ModelConfig,

    #[serde(default)]
    pub server: ServerConfig,

    #[serde(default)]
    pub admin: AdminConfig,

    #[serde(default)]
    pub limits: LimitsConfig,

    #[serde(default)]
    pub http: HttpConfig,

    #[serde(default)]
    pub log: LogConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    #[serde(default = "default_empty")]
    pub api_key: String,
    #[serde(default = "default_empty")]
    pub base_url: String,
    #[serde(default = "default_empty")]
    pub proxy: String,
    /// Known provider type. Inferred from the provider key if absent in old configs.
    #[serde(default)]
    pub provider_type: Option<ProviderType>,
    /// Copilot-specific OAuth and optimization configuration.
    #[serde(default)]
    pub copilot: Option<CopilotProviderConfig>,
}

impl ProviderConfig {
    /// Resolve the provider type, falling back to inference from the provider ID.
    pub fn resolve_type(&self, provider_id: &str) -> ProviderType {
        self.provider_type
            .clone()
            .unwrap_or_else(|| ProviderType::parse(provider_id))
    }

    /// Whether this provider uses OAuth (not API key).
    pub fn uses_oauth(&self, provider_id: &str) -> bool {
        self.resolve_type(provider_id).auth_method() == AuthMethod::OAuth
    }
}

/// Copilot provider configuration (OAuth + premium optimizations).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CopilotProviderConfig {
    /// OAuth application to use: "vscode" (default) or "opencode".
    #[serde(default = "default_oauth_app")]
    pub oauth_app: String,
    /// Model used for warmup requests (no tools, no beta header).
    #[serde(default = "default_small_model")]
    pub small_model: String,
    /// Maximum thinking tokens for requests.
    #[serde(default = "default_max_thinking_tokens")]
    pub max_thinking_tokens: u32,
    /// Enable warmup detection (route tool-less requests to small model).
    #[serde(default = "default_true")]
    pub enable_warmup: bool,
    /// Enable tool_result merging to reduce premium request count.
    #[serde(default = "default_true")]
    pub enable_tool_result_merge: bool,
    /// Enable compact/auto-continue request detection.
    #[serde(default = "default_true")]
    pub enable_compact_detection: bool,
    /// Enable subagent marker detection (x-initiator: agent).
    #[serde(default = "default_true")]
    pub enable_agent_marking: bool,
}

fn default_oauth_app() -> String {
    "vscode".to_string()
}
fn default_small_model() -> String {
    "gpt-5-mini".to_string()
}
fn default_max_thinking_tokens() -> u32 {
    16000
}
fn default_true() -> bool {
    true
}

/// Authentication method for a provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AuthMethod {
    #[serde(rename = "api_key")]
    ApiKey,
    #[serde(rename = "oauth")]
    OAuth,
}

/// Known provider type. Informs base URL, auth method, and provider implementation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProviderType {
    OpenAI,
    Anthropic,
    Copilot,
    OpenRouter,
    Google,
    /// Custom OpenAI-compatible endpoint.
    Custom(String),
    /// Custom Anthropic-compatible endpoint.
    CustomAnthropic(String),
}

impl std::str::FromStr for ProviderType {
    type Err = std::convert::Infallible;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "openai" => ProviderType::OpenAI,
            "anthropic" => ProviderType::Anthropic,
            "copilot" | "github-copilot" => ProviderType::Copilot,
            "openrouter" => ProviderType::OpenRouter,
            "google" => ProviderType::Google,
            "custom-anthropic" => ProviderType::CustomAnthropic(String::new()),
            _ => ProviderType::Custom(s.to_string()),
        })
    }
}

impl ProviderType {
    /// Parse from a string identifier (infallible alias for FromStr).
    pub fn parse(s: &str) -> Self {
        s.parse().unwrap()
    }

    /// Return the canonical string identifier.
    pub fn as_str(&self) -> &str {
        match self {
            ProviderType::OpenAI => "openai",
            ProviderType::Anthropic => "anthropic",
            ProviderType::Copilot => "copilot",
            ProviderType::OpenRouter => "openrouter",
            ProviderType::Google => "google",
            ProviderType::Custom(s) => s.as_str(),
            ProviderType::CustomAnthropic(s) => s.as_str(),
        }
    }

    /// Human-readable display name for UI.
    pub fn display_name(&self) -> &str {
        match self {
            ProviderType::OpenAI => "OpenAI",
            ProviderType::Anthropic => "Anthropic",
            ProviderType::Copilot => "GitHub Copilot",
            ProviderType::OpenRouter => "OpenRouter",
            ProviderType::Google => "Google",
            ProviderType::Custom(_) => "Custom (OpenAI-compatible)",
            ProviderType::CustomAnthropic(_) => "Custom (Anthropic-compatible)",
        }
    }

    /// Default API base URL for this provider type.
    pub fn default_base_url(&self) -> &str {
        match self {
            ProviderType::OpenAI => "https://api.openai.com/v1",
            ProviderType::Anthropic => "https://api.anthropic.com",
            ProviderType::Copilot => "https://api.githubcopilot.com",
            ProviderType::OpenRouter => "https://openrouter.ai/api/v1",
            ProviderType::Google => "https://generativelanguage.googleapis.com/v1beta",
            ProviderType::Custom(_) => "",
            ProviderType::CustomAnthropic(_) => "",
        }
    }

    /// How this provider authenticates.
    pub fn auth_method(&self) -> AuthMethod {
        match self {
            ProviderType::Copilot => AuthMethod::OAuth,
            _ => AuthMethod::ApiKey,
        }
    }

    /// Whether this provider requires an API key (not OAuth).
    pub fn needs_api_key(&self) -> bool {
        matches!(self.auth_method(), AuthMethod::ApiKey)
    }

    /// Default model name to use as fallback when switching providers.
    pub fn default_model_name(&self) -> &str {
        match self {
            ProviderType::Copilot => "gpt-5",
            _ => "",
        }
    }

    /// All known provider types for selection UIs.
    pub fn known_types() -> Vec<ProviderType> {
        vec![
            ProviderType::OpenAI,
            ProviderType::Anthropic,
            ProviderType::Copilot,
            ProviderType::OpenRouter,
            ProviderType::Google,
            ProviderType::Custom(String::new()),
            ProviderType::CustomAnthropic(String::new()),
        ]
    }
}

impl Serialize for ProviderType {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            ProviderType::Custom(s) => serializer.serialize_str(&format!("custom:{s}")),
            ProviderType::CustomAnthropic(s) => {
                serializer.serialize_str(&format!("custom-anthropic:{s}"))
            }
            _ => serializer.serialize_str(self.as_str()),
        }
    }
}

impl<'de> Deserialize<'de> for ProviderType {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        if let Some(name) = s.strip_prefix("custom-anthropic:") {
            Ok(ProviderType::CustomAnthropic(name.to_string()))
        } else if let Some(name) = s.strip_prefix("custom:anthropic:") {
            Ok(ProviderType::CustomAnthropic(name.to_string()))
        } else if let Some(name) = s.strip_prefix("custom:") {
            Ok(ProviderType::Custom(name.to_string()))
        } else {
            Ok(ProviderType::parse(&s))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default = "default_model")]
    pub default: String,
    pub opus: Option<String>,
    pub sonnet: Option<String>,
    pub haiku: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_auth_token")]
    pub auth_token: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdminConfig {
    pub auth_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitsConfig {
    #[serde(default = "default_rate_limit")]
    pub rate_limit: u32,
    #[serde(default = "default_rate_window")]
    pub rate_window: u32,
    #[serde(default = "default_max_concurrency")]
    pub max_concurrency: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpConfig {
    #[serde(default = "default_read_timeout")]
    pub read_timeout: u64,
    #[serde(default = "default_write_timeout")]
    pub write_timeout: u64,
    #[serde(default = "default_connect_timeout")]
    pub connect_timeout: u64,
    /// Extra CA certificate files (PEM, single cert or bundle) to trust in
    /// addition to the built-in webpki Mozilla roots. Required for corporate
    /// networks that perform TLS MITM (Fortinet, Zscaler, Bluecoat, ...) —
    /// without this, every outbound HTTPS call fails with `error sending
    /// request` even though `curl` works, because the rustls feature this
    /// crate uses (`rustls-tls`) does not consult the system trust store.
    ///
    /// Example: `extra_ca_certs = ["/etc/ssl/certs/ca-certificates.crt"]`
    /// or point at the specific corporate root, e.g.
    /// `["/etc/ssl/certs/FG201ETK19909125.pem"]`.
    #[serde(default)]
    pub extra_ca_certs: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
    /// Path to log file. Defaults to config_dir/claude-proxy.log if not set.
    #[serde(default)]
    pub file: Option<String>,
    /// Also emit logs to stderr (true for non-daemon server, false for TUI/daemon).
    #[serde(default = "default_true")]
    pub with_stdout: bool,
    #[serde(default)]
    pub raw_api_payloads: bool,
    #[serde(default)]
    pub raw_sse_events: bool,
}

// --- Defaults ---

fn default_empty() -> String {
    String::new()
}
fn default_model() -> String {
    "openai/gpt-4.1".to_string()
}
fn default_host() -> String {
    "127.0.0.1".to_string()
}
fn default_port() -> u16 {
    8082
}
fn default_auth_token() -> String {
    "freecc".to_string()
}
fn default_rate_limit() -> u32 {
    40
}
fn default_rate_window() -> u32 {
    60
}
fn default_max_concurrency() -> u32 {
    5
}
fn default_read_timeout() -> u64 {
    300
}
fn default_write_timeout() -> u64 {
    60
}
fn default_connect_timeout() -> u64 {
    60
}
fn default_log_level() -> String {
    "info".to_string()
}

impl Default for ModelConfig {
    fn default() -> Self {
        Self {
            default: default_model(),
            opus: None,
            sonnet: None,
            haiku: None,
        }
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            host: default_host(),
            port: default_port(),
            auth_token: default_auth_token(),
        }
    }
}

impl Default for LimitsConfig {
    fn default() -> Self {
        Self {
            rate_limit: default_rate_limit(),
            rate_window: default_rate_window(),
            max_concurrency: default_max_concurrency(),
        }
    }
}

impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            read_timeout: default_read_timeout(),
            write_timeout: default_write_timeout(),
            connect_timeout: default_connect_timeout(),
            extra_ca_certs: Vec::new(),
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
            file: None,
            with_stdout: true,
            raw_api_payloads: false,
            raw_sse_events: false,
        }
    }
}

impl Settings {
    /// Load settings from a TOML file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path).map_err(|e| ConfigError::Read {
            path: path.to_path_buf(),
            source: e,
        })?;
        Self::from_toml(&content, path)
    }

    /// Parse settings from a TOML string.
    pub fn from_toml(content: &str, path: &Path) -> Result<Self, ConfigError> {
        let mut settings: Self = toml::from_str(content).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        settings.infer_provider_types();
        Ok(settings)
    }

    /// Fill in missing provider_type fields from the HashMap key.
    pub fn infer_provider_types(&mut self) {
        for (id, config) in self.providers.iter_mut() {
            if config.provider_type.is_none() {
                config.provider_type = Some(ProviderType::parse(id));
            }
        }
    }

    /// Serialize settings to a TOML string.
    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).expect("Settings serialization should not fail")
    }

    /// Resolve a Claude model name to a `provider_id/upstream_model` string.
    pub fn resolve_model(&self, claude_model: &str) -> String {
        let lower = claude_model.to_lowercase();
        if lower.contains("opus")
            && let Some(ref m) = self.model.opus
        {
            return m.clone();
        } else if lower.contains("haiku")
            && let Some(ref m) = self.model.haiku
        {
            return m.clone();
        } else if lower.contains("sonnet")
            && let Some(ref m) = self.model.sonnet
        {
            return m.clone();
        }
        if claude_model.contains('/') {
            return claude_model.to_string();
        }
        let provider = Self::parse_provider_id(&self.model.default);
        format!("{}/{}", provider, claude_model)
    }

    /// Extract the provider ID from a `provider_id/model` string (first `/` only).
    pub fn parse_provider_id(model_ref: &str) -> &str {
        model_ref
            .find('/')
            .map(|i| &model_ref[..i])
            .unwrap_or(model_ref)
    }

    /// Extract the upstream model name from a `provider_id/model` string (first `/` only).
    pub fn parse_model_name(model_ref: &str) -> &str {
        model_ref
            .find('/')
            .map(|i| &model_ref[i + 1..])
            .unwrap_or(model_ref)
    }

    /// Get the admin auth token, falling back to server.auth_token.
    pub fn admin_auth_token(&self) -> &str {
        self.admin
            .auth_token
            .as_deref()
            .unwrap_or(&self.server.auth_token)
    }

    /// Get the config file path for this platform.
    pub fn config_dir() -> Option<PathBuf> {
        dirs::config_dir().map(|p| p.join("claude-proxy"))
    }

    /// Get the full config file path.
    pub fn config_file_path() -> Option<PathBuf> {
        Self::config_dir().map(|p| p.join("config.toml"))
    }

    /// Validate the settings, returning errors for critical issues.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.server.port == 0 {
            return Err(ConfigError::Validation(
                "server.port must be non-zero".to_string(),
            ));
        }
        if self.limits.rate_limit == 0 {
            return Err(ConfigError::Validation(
                "limits.rate_limit must be > 0".to_string(),
            ));
        }
        if self.limits.max_concurrency == 0 {
            return Err(ConfigError::Validation(
                "limits.max_concurrency must be > 0".to_string(),
            ));
        }
        // Validate model format: must contain provider_id/model_name
        let models = [
            ("model.default", Some(self.model.default.as_str())),
            ("model.opus", self.model.opus.as_deref()),
            ("model.sonnet", self.model.sonnet.as_deref()),
            ("model.haiku", self.model.haiku.as_deref()),
        ];
        for (field, value) in models {
            if let Some(v) = value
                && !v.contains('/')
            {
                return Err(ConfigError::Validation(format!(
                    "{field} must be in 'provider_id/model_name' format, got: {v}"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_provider_id() {
        assert_eq!(Settings::parse_provider_id("openai/gpt-4.1"), "openai");
        assert_eq!(
            Settings::parse_provider_id("anthropic/claude-opus-4-20250514"),
            "anthropic"
        );
    }

    #[test]
    fn test_parse_model_name() {
        assert_eq!(Settings::parse_model_name("openai/gpt-4.1"), "gpt-4.1");
        assert_eq!(
            Settings::parse_model_name("anthropic/claude-opus-4-20250514"),
            "claude-opus-4-20250514"
        );
    }

    #[test]
    fn test_resolve_model() {
        let settings = Settings {
            model: ModelConfig {
                default: "openai/gpt-4.1".to_string(),
                opus: Some("anthropic/claude-opus-4-20250514".to_string()),
                sonnet: None,
                haiku: Some("openai/gpt-4.1-mini".to_string()),
            },
            ..Default::default()
        };
        assert_eq!(
            settings.resolve_model("claude-opus-4-20250514"),
            "anthropic/claude-opus-4-20250514"
        );
        assert_eq!(
            settings.resolve_model("claude-3-5-haiku-latest"),
            "openai/gpt-4.1-mini"
        );
        assert_eq!(
            settings.resolve_model("claude-sonnet-4-20250514"),
            "openai/claude-sonnet-4-20250514"
        );
        assert_eq!(
            settings.resolve_model("deepseek-v4-flash"),
            "openai/deepseek-v4-flash"
        );
        assert_eq!(
            settings.resolve_model("opencode-go/deepseek-v4-pro"),
            "opencode-go/deepseek-v4-pro"
        );
    }

    #[test]
    fn test_from_toml() {
        let toml = r#"
[model]
default = "openai/gpt-4.1"

[server]
host = "0.0.0.0"
port = 9090
auth_token = "test-token"
"#;
        let settings = Settings::from_toml(toml, Path::new("test.toml")).unwrap();
        assert_eq!(settings.server.port, 9090);
        assert_eq!(settings.server.auth_token, "test-token");
    }

    #[test]
    fn test_provider_type_parse() {
        assert_eq!(ProviderType::parse("openai"), ProviderType::OpenAI);
        assert_eq!(ProviderType::parse("anthropic"), ProviderType::Anthropic);
        assert_eq!(ProviderType::parse("copilot"), ProviderType::Copilot);
        assert_eq!(ProviderType::parse("github-copilot"), ProviderType::Copilot);
        assert_eq!(ProviderType::parse("openrouter"), ProviderType::OpenRouter);
        assert_eq!(ProviderType::parse("google"), ProviderType::Google);
        assert!(matches!(
            ProviderType::parse("custom-anthropic"),
            ProviderType::CustomAnthropic(s) if s.is_empty()
        ));
        assert!(matches!(
            ProviderType::parse("my-custom"),
            ProviderType::Custom(s) if s == "my-custom"
        ));
    }

    #[test]
    fn test_provider_type_as_str() {
        assert_eq!(ProviderType::OpenAI.as_str(), "openai");
        assert_eq!(ProviderType::Anthropic.as_str(), "anthropic");
        assert_eq!(ProviderType::Copilot.as_str(), "copilot");
        assert_eq!(ProviderType::Custom("foo".into()).as_str(), "foo");
        assert_eq!(ProviderType::CustomAnthropic("bar".into()).as_str(), "bar");
    }

    #[test]
    fn test_provider_type_default_base_url() {
        assert!(
            ProviderType::OpenAI
                .default_base_url()
                .contains("openai.com")
        );
        assert!(
            ProviderType::Anthropic
                .default_base_url()
                .contains("anthropic.com")
        );
        assert!(
            ProviderType::Copilot
                .default_base_url()
                .contains("githubcopilot.com")
        );
        assert!(
            ProviderType::OpenRouter
                .default_base_url()
                .contains("openrouter.ai")
        );
        assert_eq!(ProviderType::Custom("x".into()).default_base_url(), "");
        assert_eq!(
            ProviderType::CustomAnthropic("x".into()).default_base_url(),
            ""
        );
    }

    #[test]
    fn test_provider_type_auth_method() {
        assert_eq!(ProviderType::OpenAI.auth_method(), AuthMethod::ApiKey);
        assert_eq!(ProviderType::Copilot.auth_method(), AuthMethod::OAuth);
        assert!(ProviderType::OpenAI.needs_api_key());
        assert!(!ProviderType::Copilot.needs_api_key());
    }

    #[test]
    fn test_provider_type_serde_roundtrip() {
        let cases = vec![
            ProviderType::OpenAI,
            ProviderType::Anthropic,
            ProviderType::Copilot,
            ProviderType::OpenRouter,
            ProviderType::Google,
            ProviderType::Custom("my-provider".into()),
            ProviderType::CustomAnthropic("my-anthropic".into()),
        ];
        for pt in cases {
            let json = serde_json::to_string(&pt).unwrap();
            let back: ProviderType = serde_json::from_str(&json).unwrap();
            assert_eq!(pt, back, "roundtrip failed for {pt:?} (json: {json})");
        }
    }

    #[test]
    fn test_infer_provider_types() {
        let toml = r#"
[providers.openai]
api_key = "sk-test"

[providers.copilot]
base_url = "https://api.githubcopilot.com"

[providers.custom-thing]
api_key = "custom-key"
base_url = "https://custom.example.com"
"#;
        let settings = Settings::from_toml(toml, Path::new("test.toml")).unwrap();
        assert_eq!(
            settings.providers.get("openai").unwrap().provider_type,
            Some(ProviderType::OpenAI)
        );
        assert_eq!(
            settings.providers.get("copilot").unwrap().provider_type,
            Some(ProviderType::Copilot)
        );
        assert!(matches!(
            settings.providers.get("custom-thing").unwrap().provider_type,
            Some(ProviderType::Custom(ref s)) if s == "custom-thing"
        ));
    }

    #[test]
    fn test_custom_anthropic_provider_type_deserialize() {
        let pt: ProviderType = serde_json::from_str("\"custom-anthropic:my-claude\"").unwrap();
        assert!(matches!(
            pt,
            ProviderType::CustomAnthropic(ref s) if s == "my-claude"
        ));
        assert_eq!(
            serde_json::to_string(&pt).unwrap(),
            "\"custom-anthropic:my-claude\""
        );
    }

    #[test]
    fn test_known_types_count() {
        let types = ProviderType::known_types();
        assert_eq!(types.len(), 7);
    }
}
