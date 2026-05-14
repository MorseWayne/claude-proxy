//! Provider trait and implementations for upstream API adapters.

pub mod anthropic;
pub mod copilot;
pub mod openai;
pub mod provider;

pub use provider::{Provider, ProviderError};

use claude_proxy_config::Settings;
use claude_proxy_config::settings::{ProviderConfig, ProviderType};

/// Create a provider instance from config.
pub async fn create_provider(
    provider_id: &str,
    config: &ProviderConfig,
    settings: &Settings,
) -> Result<Box<dyn Provider>, ProviderError> {
    let pt = config
        .provider_type
        .clone()
        .unwrap_or_else(|| ProviderType::parse(provider_id));

    match pt {
        ProviderType::OpenAI => Ok(Box::new(openai::OpenAiProvider::new(
            provider_id,
            &config.api_key,
            &config.base_url,
            &config.proxy,
            settings.http.connect_timeout,
            settings.http.read_timeout,
        )?)),
        ProviderType::Anthropic => Ok(Box::new(anthropic::AnthropicProvider::new(
            provider_id,
            &config.api_key,
            &config.base_url,
            &config.proxy,
            settings.http.connect_timeout,
            settings.http.read_timeout,
        )?)),
        ProviderType::Copilot => Ok(Box::new(
            copilot::CopilotProvider::new(provider_id, config, settings).await?,
        )),
        ProviderType::OpenRouter | ProviderType::Google | ProviderType::Custom(_) => {
            Ok(Box::new(openai::OpenAiProvider::new(
                provider_id,
                &config.api_key,
                &config.base_url,
                &config.proxy,
                settings.http.connect_timeout,
                settings.http.read_timeout,
            )?))
        }
    }
}
