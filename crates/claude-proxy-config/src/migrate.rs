use std::collections::HashMap;
use std::fs;
use std::path::Path;

use tracing::info;

use crate::error::ConfigError;
use crate::settings::{ModelConfig, ProviderConfig, ServerConfig, Settings};

/// Migrate a legacy `.env` file to a `Settings` struct.
///
/// Returns `None` if the file doesn't exist or can't be read.
pub fn try_migrate_env(env_path: &Path) -> Result<Option<Settings>, ConfigError> {
    if !env_path.exists() {
        return Ok(None);
    }

    let content = fs::read_to_string(env_path).map_err(|e| ConfigError::Read {
        path: env_path.to_path_buf(),
        source: e,
    })?;

    let env = parse_dotenv(&content);

    let settings = build_settings_from_env(&env);

    info!("Migrated legacy .env config from {}", env_path.display());

    Ok(Some(settings))
}

/// Check whether a migration is needed (legacy .env exists, TOML doesn't).
pub fn needs_migration() -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let legacy = dirs::config_dir()
        .map(|p| p.join("claude-proxy").join(".env"))
        .filter(|p| p.exists())?;

    let toml_path = Settings::config_file_path().filter(|p| !p.exists())?;

    Some((legacy, toml_path))
}

/// Perform automatic migration if needed. Returns loaded Settings.
pub fn auto_migrate() -> Result<Option<Settings>, ConfigError> {
    let Some((env_path, toml_path)) = needs_migration() else {
        return Ok(None);
    };

    let settings = try_migrate_env(&env_path)?.ok_or_else(|| {
        ConfigError::Migration(format!(
            "failed to read legacy .env at {}",
            env_path.display()
        ))
    })?;

    // Write the TOML config
    if let Some(parent) = toml_path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            ConfigError::Migration(format!(
                "failed to create config dir {}: {e}",
                parent.display()
            ))
        })?;
    }

    let toml_content = settings.to_toml();
    fs::write(&toml_path, &toml_content).map_err(|e| {
        ConfigError::Migration(format!(
            "failed to write config to {}: {e}",
            toml_path.display()
        ))
    })?;

    info!(
        "Auto-migrated .env → TOML: {} → {}",
        env_path.display(),
        toml_path.display()
    );

    Ok(Some(settings))
}

/// Parse a dotenv file into a key-value map.
fn parse_dotenv(content: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim().to_string();
            let value = value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string();
            map.insert(key, value);
        }
    }
    map
}

/// Build a Settings struct from parsed .env key-value pairs.
fn build_settings_from_env(env: &HashMap<String, String>) -> Settings {
    let openai_key = env.get("OPENAI_API_KEY").cloned().unwrap_or_default();
    let openai_base = env
        .get("OPENAI_BASE_URL")
        .cloned()
        .unwrap_or_else(|| "https://api.openai.com/v1".to_string());
    let openai_proxy = env.get("OPENAI_PROXY").cloned().unwrap_or_default();

    let anthropic_key = env
        .get("ANTHROPIC_UPSTREAM_API_KEY")
        .cloned()
        .unwrap_or_default();
    let anthropic_base = env
        .get("ANTHROPIC_UPSTREAM_BASE_URL")
        .cloned()
        .unwrap_or_else(|| "https://api.anthropic.com".to_string());
    let anthropic_proxy = env
        .get("ANTHROPIC_UPSTREAM_PROXY")
        .cloned()
        .unwrap_or_default();

    let mut providers = HashMap::new();
    if !openai_key.is_empty() {
        providers.insert(
            "openai".to_string(),
            ProviderConfig {
                api_key: openai_key,
                base_url: openai_base,
                proxy: openai_proxy,
            },
        );
    }
    if !anthropic_key.is_empty() {
        providers.insert(
            "anthropic".to_string(),
            ProviderConfig {
                api_key: anthropic_key,
                base_url: anthropic_base,
                proxy: anthropic_proxy,
            },
        );
    }

    let model = ModelConfig {
        default: env
            .get("MODEL")
            .cloned()
            .unwrap_or_else(|| "openai/gpt-4.1".to_string()),
        opus: env.get("MODEL_OPUS").cloned().filter(|s| !s.is_empty()),
        sonnet: env.get("MODEL_SONNET").cloned().filter(|s| !s.is_empty()),
        haiku: env.get("MODEL_HAIKU").cloned().filter(|s| !s.is_empty()),
    };

    let server = ServerConfig {
        host: env
            .get("HOST")
            .cloned()
            .unwrap_or_else(|| "0.0.0.0".to_string()),
        port: env.get("PORT").and_then(|s| s.parse().ok()).unwrap_or(8082),
        auth_token: env
            .get("ANTHROPIC_AUTH_TOKEN")
            .cloned()
            .unwrap_or_else(|| "freecc".to_string()),
    };

    let limits = crate::settings::LimitsConfig {
        rate_limit: env
            .get("PROVIDER_RATE_LIMIT")
            .and_then(|s| s.parse().ok())
            .unwrap_or(40),
        rate_window: env
            .get("PROVIDER_RATE_WINDOW")
            .and_then(|s| s.parse().ok())
            .unwrap_or(60),
        max_concurrency: env
            .get("PROVIDER_MAX_CONCURRENCY")
            .and_then(|s| s.parse().ok())
            .unwrap_or(5),
    };

    let http = crate::settings::HttpConfig {
        read_timeout: env
            .get("HTTP_READ_TIMEOUT")
            .and_then(|s| s.parse().ok())
            .unwrap_or(300),
        write_timeout: env
            .get("HTTP_WRITE_TIMEOUT")
            .and_then(|s| s.parse().ok())
            .unwrap_or(60),
        connect_timeout: env
            .get("HTTP_CONNECT_TIMEOUT")
            .and_then(|s| s.parse().ok())
            .unwrap_or(60),
    };

    Settings {
        providers,
        model,
        server,
        admin: crate::settings::AdminConfig { auth_token: None },
        limits,
        http,
        log: crate::settings::LogConfig::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dotenv() {
        let content = r#"
# Comment
OPENAI_API_KEY="sk-test"
MODEL=openai/gpt-4.1
PORT=9090
"#;
        let env = parse_dotenv(content);
        assert_eq!(env.get("OPENAI_API_KEY").unwrap(), "sk-test");
        assert_eq!(env.get("MODEL").unwrap(), "openai/gpt-4.1");
        assert_eq!(env.get("PORT").unwrap(), "9090");
    }

    #[test]
    fn test_build_settings_from_env() {
        let mut env = HashMap::new();
        env.insert("OPENAI_API_KEY".to_string(), "sk-test".to_string());
        env.insert("MODEL".to_string(), "openai/gpt-4.1".to_string());
        env.insert("ANTHROPIC_AUTH_TOKEN".to_string(), "my-token".to_string());

        let settings = build_settings_from_env(&env);
        assert_eq!(settings.server.auth_token, "my-token");
        assert_eq!(settings.model.default, "openai/gpt-4.1");
        assert!(settings.providers.contains_key("openai"));
    }
}
