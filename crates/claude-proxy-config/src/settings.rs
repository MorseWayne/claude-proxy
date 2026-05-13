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
    /// Copilot-specific OAuth and optimization configuration.
    #[serde(default)]
    pub copilot: Option<CopilotProviderConfig>,
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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    #[serde(default = "default_log_level")]
    pub level: String,
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
        }
    }
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: default_log_level(),
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
        toml::from_str(content).map_err(|e| ConfigError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })
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
}
