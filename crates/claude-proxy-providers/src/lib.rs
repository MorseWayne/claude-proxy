//! Provider trait and implementations for upstream API adapters.

pub mod anthropic;
pub mod openai;
pub mod provider;

pub use provider::{Provider, ProviderError};

use claude_proxy_config::Settings;
use claude_proxy_config::settings::ProviderConfig;

/// Create a provider instance from config.
pub fn create_provider(
    provider_id: &str,
    config: &ProviderConfig,
    settings: &Settings,
) -> Result<Box<dyn Provider>, ProviderError> {
    match provider_id {
        "openai" => Ok(Box::new(openai::OpenAiProvider::new(
            provider_id,
            &config.api_key,
            &config.base_url,
            &config.proxy,
            settings.http.connect_timeout,
            settings.http.read_timeout,
        )?)),
        "anthropic" => Ok(Box::new(anthropic::AnthropicProvider::new(
            provider_id,
            &config.api_key,
            &config.base_url,
            &config.proxy,
            settings.http.connect_timeout,
            settings.http.read_timeout,
        )?)),
        _ => Ok(Box::new(openai::OpenAiProvider::new(
            provider_id,
            &config.api_key,
            &config.base_url,
            &config.proxy,
            settings.http.connect_timeout,
            settings.http.read_timeout,
        )?)),
    }
}
