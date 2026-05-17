//! Provider trait and implementations for upstream API adapters.

pub mod anthropic;
mod chat_completions;
pub mod chatgpt;
pub mod copilot;
pub mod http;
pub mod openai;
pub mod provider;
pub(crate) mod responses;
mod tool_args;

pub use http::{apply_extra_ca_certs, fmt_err_chain, fmt_reqwest_err};
pub use provider::{Provider, ProviderError};

use std::sync::Arc;

use claude_proxy_config::Settings;
use claude_proxy_config::settings::{ProviderConfig, ProviderType};

/// Create a provider instance from config.
pub async fn create_provider(
    provider_id: &str,
    config: &ProviderConfig,
    settings: &Settings,
) -> Result<Arc<dyn Provider>, ProviderError> {
    let pt = config
        .provider_type
        .clone()
        .unwrap_or_else(|| ProviderType::parse(provider_id));

    match pt {
        ProviderType::OpenAI => Ok(Arc::new(openai::OpenAiProvider::new(
            provider_id,
            &config.api_key,
            &config.base_url,
            &config.proxy,
            settings.http.connect_timeout,
            settings.http.read_timeout,
            &settings.http.extra_ca_certs,
        )?)),
        ProviderType::Anthropic => Ok(Arc::new(anthropic::AnthropicProvider::new(
            provider_id,
            &config.api_key,
            &config.base_url,
            &config.proxy,
            settings.http.connect_timeout,
            settings.http.read_timeout,
            &settings.http.extra_ca_certs,
        )?)),
        ProviderType::CustomAnthropic(_) => Ok(Arc::new(anthropic::AnthropicProvider::new(
            provider_id,
            &config.api_key,
            &config.base_url,
            &config.proxy,
            settings.http.connect_timeout,
            settings.http.read_timeout,
            &settings.http.extra_ca_certs,
        )?)),
        ProviderType::Copilot => Ok(Arc::new(
            copilot::CopilotProvider::new(provider_id, config, settings).await?,
        )),
        ProviderType::ChatGPT => Ok(Arc::new(
            chatgpt::ChatGptProvider::new(provider_id, &config.base_url, &config.proxy, settings)
                .await?,
        )),
        ProviderType::OpenRouter | ProviderType::Google | ProviderType::Custom(_) => {
            Ok(Arc::new(openai::OpenAiProvider::new(
                provider_id,
                &config.api_key,
                &config.base_url,
                &config.proxy,
                settings.http.connect_timeout,
                settings.http.read_timeout,
                &settings.http.extra_ca_certs,
            )?))
        }
    }
}
