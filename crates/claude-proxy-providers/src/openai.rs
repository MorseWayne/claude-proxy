//! OpenAI-compatible provider adapter.
//!
//! Converts Anthropic Messages API requests to OpenAI ChatCompletion format
//! and converts streaming responses back to Anthropic SSE format.

mod request;

use std::time::Duration;

use async_trait::async_trait;
use claude_proxy_config::settings::ProviderRuntimeConfig;
use claude_proxy_core::*;
use futures::stream::BoxStream;
use reqwest::Client;
use serde_json::{Map, Value, json};

use crate::http::{
    UpstreamRequestPolicy, apply_extra_ca_certs, apply_runtime_request_config, fmt_reqwest_err,
    map_upstream_response, read_upstream_response_json, read_upstream_response_text,
    send_upstream_request_with_policy,
};
use crate::openai_compat::{
    apply_openai_intent, log_request_observability, openai_model_info, prefers_responses,
};
use crate::provider::{Provider, ProviderError, ProviderRequestObserver};
use crate::reasoning_markers::marker_mode_from_request;

pub struct OpenAiProvider {
    id: String,
    client: Client,
    base_url: String,
    request_policy: UpstreamRequestPolicy,
    runtime: ProviderRuntimeConfig,
}

fn merge_model_info(mut upstream: ModelInfo) -> ModelInfo {
    let known = openai_model_info(&upstream.model_id);
    if upstream.vendor.is_none() {
        upstream.vendor = known.vendor;
    }
    upstream.capabilities = known.capabilities;
    upstream
}

fn apply_openai_responses_options(
    body: &mut Value,
    request: &MessagesRequest,
    runtime: &ProviderRuntimeConfig,
) {
    let Some(object) = body.as_object_mut() else {
        return;
    };

    insert_trimmed_string(
        object,
        "service_tier",
        request
            .extra
            .get("service_tier")
            .and_then(Value::as_str)
            .or(runtime.openai.service_tier.as_deref()),
    );
    insert_trimmed_string(
        object,
        "prompt_cache_key",
        request
            .extra
            .get("prompt_cache_key")
            .and_then(Value::as_str)
            .or_else(|| {
                request
                    .metadata
                    .as_ref()
                    .and_then(|metadata| metadata.get("prompt_cache_key"))
                    .and_then(Value::as_str)
            }),
    );
    if let Some(value) = request.extra.get("parallel_tool_calls")
        && value.is_boolean()
    {
        object.insert("parallel_tool_calls".to_string(), value.clone());
    }
    if let Some(verbosity) = openai_responses_verbosity(request) {
        object.insert("text".to_string(), json!({ "verbosity": verbosity }));
    }
}

fn openai_responses_verbosity(request: &MessagesRequest) -> Option<&str> {
    if !request.model.starts_with("gpt-5") {
        return None;
    }

    request
        .extra
        .get("verbosity")
        .and_then(Value::as_str)
        .or_else(|| {
            request
                .extra
                .get("text")
                .and_then(|value| value.get("verbosity"))
                .and_then(Value::as_str)
        })
        .map(str::trim)
        .filter(|value| matches!(*value, "low" | "medium" | "high"))
}

fn insert_trimmed_string(object: &mut Map<String, Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) {
        object
            .entry(key.to_string())
            .or_insert_with(|| value.into());
    }
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
        request_policy: UpstreamRequestPolicy,
        runtime: ProviderRuntimeConfig,
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
            request_policy,
            runtime,
        })
    }

    async fn chat_via_completions(
        &self,
        request: MessagesRequest,
        observer: Option<ProviderRequestObserver>,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let body = request::convert_request(&request);
        let url = format!("{}/chat/completions", self.base_url);

        log_request_observability("openai", "/chat/completions", &body, None);

        let upstream_request =
            apply_runtime_request_config(self.client.post(&url), &self.runtime)?.json(&body);
        let response =
            send_upstream_request_with_policy(upstream_request, self.request_policy).await?;

        if !response.status().is_success() {
            return Err(map_upstream_response(response).await);
        }

        if request.stream {
            Ok(
                crate::chat_completions::stream_openai_response_with_marker_mode_and_observer(
                    response,
                    marker_mode_from_request(&request),
                    observer,
                ),
            )
        } else {
            let body = read_upstream_response_text(response).await?;
            let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let events = crate::chat_completions::convert_non_streaming_response_with_marker_mode(
                &data,
                marker_mode_from_request(&request),
            );
            let stream = futures::stream::iter(events.into_iter().map(Ok));
            Ok(Box::pin(stream))
        }
    }

    fn responses_request_body(&self, request: &MessagesRequest) -> Value {
        let mut body = crate::responses::convert_to_responses(request);
        apply_openai_responses_options(&mut body, request, &self.runtime);
        body
    }

    async fn chat_via_responses(
        &self,
        request: MessagesRequest,
        observer: Option<ProviderRequestObserver>,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let body = self.responses_request_body(&request);
        let url = format!("{}/responses", self.base_url);

        log_request_observability("openai", "/responses", &body, None);

        let upstream_request =
            apply_runtime_request_config(self.client.post(&url), &self.runtime)?.json(&body);
        let response =
            send_upstream_request_with_policy(upstream_request, self.request_policy).await?;

        if !response.status().is_success() {
            return Err(map_upstream_response(response).await);
        }

        if request.stream {
            Ok(
                crate::responses::stream_responses_response_with_marker_mode_and_provider_observer(
                    response,
                    marker_mode_from_request(&request),
                    observer,
                ),
            )
        } else {
            let body = read_upstream_response_text(response).await?;
            let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let events = crate::responses::convert_non_streaming_response_with_marker_mode(
                &data,
                marker_mode_from_request(&request),
            );
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
        self.chat_with_observer(request, None).await
    }

    async fn chat_with_observer(
        &self,
        request: MessagesRequest,
        observer: Option<ProviderRequestObserver>,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let request = apply_openai_intent(request);
        if prefers_responses(&request.model) {
            self.chat_via_responses(request, observer).await
        } else {
            self.chat_via_completions(request, observer).await
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let url = format!("{}/models", self.base_url);
        let upstream_request = apply_runtime_request_config(self.client.get(&url), &self.runtime)?;
        let response =
            send_upstream_request_with_policy(upstream_request, self.request_policy).await?;

        if !response.status().is_success() {
            return Err(map_upstream_response(response).await);
        }

        let data: Value =
            read_upstream_response_json(response, "failed to parse models response").await?;

        let models = data["data"]
            .as_array()
            .map(|a| a.as_slice())
            .unwrap_or(&[])
            .iter()
            .filter_map(|m| {
                m["id"].as_str().map(|id| {
                    merge_model_info(ModelInfo {
                        model_id: id.to_string(),
                        vendor: Some("openai".to_string()),
                        is_chat_default: None,
                        capabilities: ModelCapabilities::default(),
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
            vendor: Some("openai".to_string()),
            is_chat_default: None,
            capabilities: ModelCapabilities::default(),
        });

        assert_eq!(info.capabilities.limits.context_window, Some(400_000));
        assert_eq!(
            info.capabilities.endpoints.supported_paths(),
            vec!["/chat/completions", "/responses"]
        );
    }

    #[test]
    fn openai_responses_body_applies_runtime_and_request_options() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("parallel_tool_calls".to_string(), json!(false));
        extra.insert("verbosity".to_string(), json!("high"));
        extra.insert("service_tier".to_string(), json!("priority"));
        extra.insert("prompt_cache_key".to_string(), json!("request-thread"));
        let runtime = ProviderRuntimeConfig {
            openai: claude_proxy_config::settings::OpenAiRuntimeConfig {
                service_tier: Some("flex".to_string()),
            },
            ..Default::default()
        };
        let provider = OpenAiProvider::new(
            "openai",
            "test-key",
            "http://127.0.0.1:1",
            "",
            1,
            1,
            &[],
            UpstreamRequestPolicy::default(),
            runtime,
        )
        .unwrap();
        let req = MessagesRequest {
            model: "gpt-5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("Hi".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: Some(vec![Tool {
                name: "search".to_string(),
                description: None,
                input_schema: json!({"type": "object"}),
            }]),
            tool_choice: None,
            thinking: None,
            metadata: Some(json!({"prompt_cache_key": "thread-123"})),
            extra,
        };

        let body = provider.responses_request_body(&req);

        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["prompt_cache_key"], "request-thread");
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["text"], json!({"verbosity": "high"}));
    }

    #[test]
    fn openai_responses_body_omits_unknown_request_options() {
        let mut extra = std::collections::HashMap::new();
        extra.insert("parallel_tool_calls".to_string(), json!("false"));
        extra.insert("verbosity".to_string(), json!("verbose"));
        let provider = OpenAiProvider::new(
            "openai",
            "test-key",
            "http://127.0.0.1:1",
            "",
            1,
            1,
            &[],
            UpstreamRequestPolicy::default(),
            ProviderRuntimeConfig::default(),
        )
        .unwrap();
        let req = MessagesRequest {
            model: "gpt-4.1".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("Hi".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: Some(json!({"prompt_cache_key": "   "})),
            extra,
        };

        let body = provider.responses_request_body(&req);

        assert!(body.get("parallel_tool_calls").is_none());
        assert!(body.get("prompt_cache_key").is_none());
        assert!(body.get("text").is_none());
    }
}
