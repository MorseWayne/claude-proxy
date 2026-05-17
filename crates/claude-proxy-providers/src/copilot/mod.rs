pub mod auth;
mod chat_completions;
pub mod headers;
mod messages;
mod model;
mod preprocess;
mod sse;
mod thinking;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use claude_proxy_config::settings::{
    CopilotProviderConfig, ProviderConfig as ConfigProviderConfig, Settings,
};
use claude_proxy_core::*;
use futures::stream::BoxStream;
use reqwest::header::HeaderMap;
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::http::{
    apply_extra_ca_certs, fmt_reqwest_err, map_upstream_response, send_upstream_request,
};
use crate::provider::{Provider, ProviderError};

use self::auth::CopilotAuth;
use self::headers::HeaderBuilder;
use self::model::{parse_copilot_model, supports_responses_only};
use self::preprocess::{merge_tool_results_inplace, preprocess};
use self::thinking::{
    apply_model_limits, copilot_messages_effort, should_use_interleaved_thinking_beta,
};

/// Copilot provider — proxies to GitHub Copilot API with VS Code impersonation.
pub struct CopilotProvider {
    id: String,
    http_client: reqwest::Client,
    base_url: String,
    auth: Arc<CopilotAuth>,
    header_builder: RwLock<HeaderBuilder>,
    config: CopilotProviderConfig,
    model_cache: RwLock<Vec<ModelInfo>>,
    model_endpoints: RwLock<HashMap<String, Vec<String>>>,
}

impl CopilotProvider {
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        id: &str,
        config: &ConfigProviderConfig,
        settings: &Settings,
    ) -> Result<Self, ProviderError> {
        let copilot_config = config.copilot.clone().unwrap_or_default();

        let base_url = if config.base_url.is_empty() {
            "https://api.githubcopilot.com".to_string()
        } else {
            config.base_url.trim_end_matches('/').to_string()
        };

        let http_client = Self::build_http_client(&config.proxy, settings)?;
        let auth = CopilotAuth::new(http_client.clone(), &copilot_config.oauth_app).await?;

        Ok(Self {
            id: id.to_string(),
            http_client,
            base_url,
            auth,
            header_builder: RwLock::new(HeaderBuilder::new()),
            config: copilot_config,
            model_cache: RwLock::new(Vec::new()),
            model_endpoints: RwLock::new(HashMap::new()),
        })
    }

    fn build_http_client(
        proxy: &str,
        settings: &Settings,
    ) -> Result<reqwest::Client, ProviderError> {
        let mut builder = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(settings.http.connect_timeout))
            .read_timeout(Duration::from_secs(settings.http.read_timeout));

        if !proxy.is_empty() {
            builder = builder.proxy(
                reqwest::Proxy::all(proxy)
                    .map_err(|e| ProviderError::Network(format!("invalid proxy: {e}")))?,
            );
        }

        builder = apply_extra_ca_certs(builder, &settings.http.extra_ca_certs)?;

        builder.build().map_err(|e| {
            ProviderError::Network(format!(
                "failed to build HTTP client: {}",
                fmt_reqwest_err(&e)
            ))
        })
    }

    fn build_headers(&self, token: &str, vision: bool, initiator: &str) -> HeaderMap {
        let hb = self.header_builder.try_read().expect("header_builder lock");
        let headers_vec = hb.build_headers(token, None, vision, initiator, "conversation-agent");
        let mut map = HeaderMap::new();
        for (k, v) in &headers_vec {
            if let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                && let Ok(value) = reqwest::header::HeaderValue::from_str(v)
            {
                map.insert(name, value);
            }
        }
        map
    }

    async fn get_model_endpoints(&self, model: &str) -> Vec<String> {
        let models = self.model_cache.read().await;
        if let Some(model_info) = models.iter().find(|m| m.model_id == model)
            && !model_info.supported_endpoints.is_empty()
        {
            return model_info.supported_endpoints.clone();
        }
        drop(models);

        let endpoints = self.model_endpoints.read().await;
        if let Some(eps) = endpoints.get(model) {
            return eps.clone();
        }
        drop(endpoints);

        let models = self.model_cache.read().await;
        models
            .iter()
            .find(|m| m.model_id == model)
            .and_then(|m| m.supports_thinking)
            .map_or_else(
                || vec!["/chat/completions".to_string()],
                |_| vec!["/v1/messages".to_string(), "/chat/completions".to_string()],
            )
    }

    async fn get_model_info(&self, model: &str) -> Option<ModelInfo> {
        self.model_cache
            .read()
            .await
            .iter()
            .find(|m| m.model_id == model)
            .cloned()
    }

    fn has_vision_content(messages: &[Message]) -> bool {
        messages.iter().any(|m| match &m.content {
            MessageContent::Blocks(blocks) => {
                blocks.iter().any(|b| matches!(b, Content::Unknown(_)))
            }
            _ => false,
        })
    }

    async fn chat_via_messages(
        &self,
        request: MessagesRequest,
        token: &str,
        initiator: &str,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let url = format!("{}/v1/messages", self.base_url);
        let vision = Self::has_vision_content(&request.messages);
        let model_info = self.get_model_info(&request.model).await;
        let mut headers = self.build_headers(token, vision, initiator);

        if should_use_interleaved_thinking_beta(model_info.as_ref()) {
            headers.insert(
                reqwest::header::HeaderName::from_static("anthropic-beta"),
                reqwest::header::HeaderValue::from_static("interleaved-thinking-2025-05-14"),
            );
        }

        // Serialize and disable tool eager input streaming, which Copilot does not support.
        let mut request = request;
        apply_model_limits(
            &mut request,
            model_info.as_ref(),
            self.config.max_thinking_tokens,
        );
        let output_effort = copilot_messages_effort(&request, model_info.as_ref());
        let (body, sanitize_stats) =
            messages::prepare_messages_request(&mut request, output_effort.as_deref())
                .map_err(|e| ProviderError::Network(format!("serialize error: {e}")))?;
        if sanitize_stats.changed() {
            debug!(
                "Prepared Copilot messages request: removed empty_text_blocks={} empty_thinking_blocks={} unsigned_thinking_blocks={} empty_messages={} merged_messages={}",
                sanitize_stats.empty_text_blocks,
                sanitize_stats.empty_thinking_blocks,
                sanitize_stats.unsigned_thinking_blocks,
                sanitize_stats.empty_messages,
                sanitize_stats.merged_messages
            );
        }

        debug!("Copilot messages API request to {url}");

        let response =
            send_upstream_request(self.http_client.post(&url).headers(headers).json(&body)).await?;

        if !response.status().is_success() {
            return Err(self.map_upstream_error(response).await);
        }

        if request.stream {
            Ok(sse::stream_anthropic_sse_response(response))
        } else {
            let body = response
                .text()
                .await
                .map_err(|e| ProviderError::Network(fmt_reqwest_err(&e)))?;
            let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let event = SseEvent {
                event: "message".to_string(),
                data,
            };
            let stream = futures::stream::iter(vec![Ok(event)]);
            Ok(Box::pin(stream))
        }
    }

    async fn chat_via_completions(
        &self,
        request: MessagesRequest,
        token: &str,
        initiator: &str,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url);
        let vision = Self::has_vision_content(&request.messages);
        let headers = self.build_headers(token, vision, initiator);
        let body = chat_completions::convert_to_openai_chat(&request);

        debug!("Copilot chat completions API request to {url}");

        let response =
            send_upstream_request(self.http_client.post(&url).headers(headers).json(&body)).await?;

        if !response.status().is_success() {
            return Err(self.map_upstream_error(response).await);
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
        token: &str,
        initiator: &str,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let url = format!("{}/responses", self.base_url);
        let vision = Self::has_vision_content(&request.messages);
        let headers = self.build_headers(token, vision, initiator);
        let body = crate::responses::convert_to_responses(&request);

        debug!("Copilot responses API request to {url}");

        let response =
            send_upstream_request(self.http_client.post(&url).headers(headers).json(&body)).await?;

        if !response.status().is_success() {
            return Err(self.map_upstream_error(response).await);
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

    async fn map_upstream_error(&self, response: reqwest::Response) -> ProviderError {
        map_upstream_response(response).await
    }
}

#[async_trait]
impl Provider for CopilotProvider {
    fn id(&self) -> &str {
        &self.id
    }

    async fn chat(
        &self,
        request: MessagesRequest,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let mut request = request;

        // Step 1: Premium optimization preprocessing
        let prep_result = preprocess(
            &request,
            self.config.enable_warmup,
            self.config.enable_compact_detection,
            self.config.enable_agent_marking,
            self.config.enable_tool_result_merge,
        );

        // Step 2: Tool result merging (mutates request)
        if self.config.enable_tool_result_merge {
            merge_tool_results_inplace(&mut request.messages);
        }

        // Step 3: Warmup override — use small model
        if prep_result.is_warmup && !self.config.small_model.is_empty() {
            info!(
                "Warmup detected, routing to small model: {}",
                self.config.small_model
            );
            request.model = self.config.small_model.clone();
        }

        // Step 4: Get auth token
        let token = self.auth.get_token().await?;

        // Step 5: Choose API path based on model capabilities
        let endpoints = self.get_model_endpoints(&request.model).await;
        let initiator = if prep_result.is_subagent {
            "agent"
        } else {
            "user"
        };

        if endpoints.iter().any(|e| e == "/v1/messages") {
            debug!("Using native /v1/messages path for model {}", request.model);
            self.chat_via_messages(request, &token, initiator).await
        } else if self.config.enable_responses_api && supports_responses_only(&endpoints) {
            debug!("Using /responses path for model {}", request.model);
            self.chat_via_responses(request, &token, initiator).await
        } else {
            debug!(
                "Falling back to /chat/completions for model {}",
                request.model
            );
            self.chat_via_completions(request, &token, initiator).await
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let token = self.auth.get_token().await?;

        let hb = self.header_builder.read().await;
        let headers_vec = hb.build_models_headers(&token);
        let mut headers = HeaderMap::new();
        for (k, v) in &headers_vec {
            if let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                && let Ok(value) = reqwest::header::HeaderValue::from_str(v)
            {
                headers.insert(name, value);
            }
        }
        drop(hb);

        let url = format!("{}/models", self.base_url);
        let response = send_upstream_request(self.http_client.get(&url).headers(headers)).await?;

        if !response.status().is_success() {
            return Err(self.map_upstream_error(response).await);
        }

        let data: Value = response
            .json()
            .await
            .map_err(|e| ProviderError::Network(format!("failed to parse models response: {e}")))?;

        let models: Vec<ModelInfo> = data["data"]
            .as_array()
            .map(|items| items.iter().filter_map(parse_copilot_model).collect())
            .unwrap_or_default();

        let mut cache = self.model_cache.write().await;
        *cache = models.clone();

        Ok(models)
    }
}
