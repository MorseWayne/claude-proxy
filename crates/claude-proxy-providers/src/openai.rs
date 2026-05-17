//! OpenAI-compatible provider adapter.
//!
//! Converts Anthropic Messages API requests to OpenAI ChatCompletion format
//! and converts streaming responses back to Anthropic SSE format.

mod request;

use std::time::Duration;

use async_trait::async_trait;
use claude_proxy_core::*;
use futures::stream::BoxStream;
use reqwest::Client;
use serde_json::Value;

use crate::http::{apply_extra_ca_certs, fmt_reqwest_err, map_upstream_response};
use crate::openai_compat::{
    apply_openai_intent, log_request_observability, openai_model_info, prefers_responses,
    supports_responses,
};
use crate::provider::{Provider, ProviderError};

pub struct OpenAiProvider {
    id: String,
    client: Client,
    base_url: String,
}

fn merge_model_info(mut upstream: ModelInfo) -> ModelInfo {
    let known = openai_model_info(&upstream.model_id);
    if upstream.supports_thinking.is_none() {
        upstream.supports_thinking = known.supports_thinking;
    }
    if upstream.vendor.is_none() {
        upstream.vendor = known.vendor;
    }
    if upstream.max_output_tokens.is_none() {
        upstream.max_output_tokens = known.max_output_tokens;
    }
    if upstream.supported_endpoints.is_empty() || supports_responses(&upstream.model_id) {
        upstream.supported_endpoints = known.supported_endpoints;
    }
    if upstream.reasoning_effort_levels.is_empty() {
        upstream.reasoning_effort_levels = known.reasoning_effort_levels;
    }
    upstream
}

impl OpenAiProvider {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: &str,
        api_key: &str,
        base_url: &str,
        proxy: &str,
        connect_timeout: u64,
        read_timeout: u64,
        extra_ca_certs: &[String],
    ) -> Result<Self, ProviderError> {
        let mut builder = Client::builder()
            .connect_timeout(Duration::from_secs(connect_timeout))
            .read_timeout(Duration::from_secs(read_timeout))
            .default_headers({
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert(
                    "Authorization",
                    format!("Bearer {api_key}")
                        .parse()
                        .map_err(|e| ProviderError::Network(format!("invalid auth header: {e}")))?,
                );
                headers
            });

        if !proxy.is_empty() {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy)
                    .map_err(|e| ProviderError::Network(format!("invalid proxy: {e}")))?,
            );
        }

        builder = apply_extra_ca_certs(builder, extra_ca_certs)?;

        let client = builder.build().map_err(|e| {
            ProviderError::Network(format!(
                "failed to build HTTP client: {}",
                fmt_reqwest_err(&e)
            ))
        })?;

        Ok(Self {
            id: id.to_string(),
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    async fn chat_via_completions(
        &self,
        request: MessagesRequest,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let body = request::convert_request(&request);
        let url = format!("{}/chat/completions", self.base_url);

        log_request_observability("openai", "/chat/completions", &body);

        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout
                } else {
                    ProviderError::Network(fmt_reqwest_err(&e))
                }
            })?;

        if !response.status().is_success() {
            return Err(map_upstream_response(response).await);
        }

        if request.stream {
            Ok(crate::chat_completions::stream_openai_response(response))
        } else {
            let body = response
                .text()
                .await
                .map_err(|e| ProviderError::Network(fmt_reqwest_err(&e)))?;
            let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let events = crate::chat_completions::convert_non_streaming_response(&data);
            let stream = futures::stream::iter(events.into_iter().map(Ok));
            Ok(Box::pin(stream))
        }
    }

    async fn chat_via_responses(
        &self,
        request: MessagesRequest,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let body = crate::responses::convert_to_responses(&request);
        let url = format!("{}/responses", self.base_url);

        log_request_observability("openai", "/responses", &body);

        let response = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout
                } else {
                    ProviderError::Network(fmt_reqwest_err(&e))
                }
            })?;

        if !response.status().is_success() {
            return Err(map_upstream_response(response).await);
        }

        if request.stream {
            Ok(crate::responses::stream_responses_response(response))
        } else {
            let body = response
                .text()
                .await
                .map_err(|e| ProviderError::Network(fmt_reqwest_err(&e)))?;
            let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let events = crate::responses::convert_non_streaming_response(&data);
            let stream = futures::stream::iter(events.into_iter().map(Ok));
            Ok(Box::pin(stream))
        }
    }
}

#[async_trait]
impl Provider for OpenAiProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn chat(
        &self,
        request: MessagesRequest,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let request = apply_openai_intent(request);
        if prefers_responses(&request.model) {
            self.chat_via_responses(request).await
        } else {
            self.chat_via_completions(request).await
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let url = format!("{}/models", self.base_url);
        let response = self.client.get(&url).send().await.map_err(|e| {
            if e.is_timeout() {
                ProviderError::Timeout
            } else {
                ProviderError::Network(fmt_reqwest_err(&e))
            }
        })?;

        if !response.status().is_success() {
            return Err(map_upstream_response(response).await);
        }

        let data: Value = response
            .json()
            .await
            .map_err(|e| ProviderError::Network(format!("failed to parse models response: {e}")))?;

        let models = data["data"]
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter_map(|m| {
                m["id"].as_str().map(|id| {
                    merge_model_info(ModelInfo {
                        model_id: id.to_string(),
                        supports_thinking: None,
                        vendor: Some("openai".to_string()),
                        max_output_tokens: None,
                        supported_endpoints: vec!["/chat/completions".to_string()],
                        is_chat_default: None,
                        supports_vision: None,
                        supports_adaptive_thinking: None,
                        min_thinking_budget: None,
                        max_thinking_budget: None,
                        reasoning_effort_levels: Vec::new(),
                    })
                })
            })
            .collect();

        Ok(models)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_model_info_adds_known_responses_endpoint() {
        let info = merge_model_info(ModelInfo {
            model_id: "gpt-5.5".to_string(),
            supports_thinking: None,
            vendor: Some("openai".to_string()),
            max_output_tokens: None,
            supported_endpoints: vec!["/chat/completions".to_string()],
            is_chat_default: None,
            supports_vision: None,
            supports_adaptive_thinking: None,
            min_thinking_budget: None,
            max_thinking_budget: None,
            reasoning_effort_levels: Vec::new(),
        });

        assert_eq!(
            info.supported_endpoints,
            vec!["/chat/completions", "/responses"]
        );
    }
}
