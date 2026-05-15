pub mod auth;
pub mod headers;
mod preprocess;
mod responses;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use claude_proxy_config::settings::{
    CopilotProviderConfig, ProviderConfig as ConfigProviderConfig, Settings,
};
use claude_proxy_core::*;
use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::header::HeaderMap;
use serde_json::Value;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::http::{apply_extra_ca_certs, fmt_reqwest_err};
use crate::provider::{Provider, ProviderError};

use self::auth::CopilotAuth;
use self::headers::HeaderBuilder;
use self::preprocess::{merge_tool_results_inplace, preprocess};

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
        if let Some(model_info) = models.iter().find(|m| m.model_id == model) {
            if !model_info.supported_endpoints.is_empty() {
                return model_info.supported_endpoints.clone();
            }
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
            MessageContent::Blocks(blocks) => blocks.iter().any(|b| matches!(b, Content::Unknown)),
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
        request.extra.clear();
        let mut body = serde_json::to_value(&request)
            .map_err(|e| ProviderError::Network(format!("serialize error: {e}")))?;
        if let Value::Object(ref mut obj) = body {
            normalize_copilot_messages_thinking(obj, output_effort.as_deref());
            disable_eager_input_streaming(obj);
        }

        debug!("Copilot messages API request to {url}");

        let response = self
            .http_client
            .post(&url)
            .headers(headers)
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
            return Err(self.map_upstream_error(response).await);
        }

        if request.stream {
            let stream = response.bytes_stream().map(|chunk| {
                chunk
                    .map(|bytes| parse_anthropic_sse(&bytes))
                    .map_err(|e| ProviderError::Network(fmt_reqwest_err(&e)))
            });
            Ok(Box::pin(stream))
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
        let body = convert_to_openai_chat(&request);

        debug!("Copilot chat completions API request to {url}");

        let response = self
            .http_client
            .post(&url)
            .headers(headers)
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
            return Err(self.map_upstream_error(response).await);
        }

        if request.stream {
            Ok(crate::openai::stream_openai_response(response))
        } else {
            let body = response
                .text()
                .await
                .map_err(|e| ProviderError::Network(fmt_reqwest_err(&e)))?;
            let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let events = crate::openai::convert_non_streaming_response(&data);
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
        let body = responses::convert_to_responses(&request);

        debug!("Copilot responses API request to {url}");

        let response = self
            .http_client
            .post(&url)
            .headers(headers)
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
            return Err(self.map_upstream_error(response).await);
        }

        if request.stream {
            Ok(responses::stream_responses_response(response))
        } else {
            let body = response
                .text()
                .await
                .map_err(|e| ProviderError::Network(fmt_reqwest_err(&e)))?;
            let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let events = responses::convert_non_streaming_response(&data);
            let stream = futures::stream::iter(events.into_iter().map(Ok));
            Ok(Box::pin(stream))
        }
    }

    async fn map_upstream_error(&self, response: reqwest::Response) -> ProviderError {
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let body = response.text().await.unwrap_or_default();
        match status {
            401 => ProviderError::Authentication(body),
            429 => ProviderError::RateLimited { retry_after },
            404 => ProviderError::ModelNotFound(body),
            _ => ProviderError::UpstreamError { status, body },
        }
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
        let response = self
            .http_client
            .get(&url)
            .headers(headers)
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

fn parse_anthropic_sse(bytes: &[u8]) -> SseEvent {
    let text = String::from_utf8_lossy(bytes);
    let mut event_type = String::new();
    let mut data = Value::Null;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("event: ") {
            event_type = rest.trim().to_string();
        } else if let Some(rest) = line
            .strip_prefix("data: ")
            .or_else(|| line.strip_prefix("data:"))
            && let Ok(parsed) = serde_json::from_str::<Value>(rest.trim())
        {
            data = parsed;
        }
    }

    SseEvent {
        event: event_type,
        data,
    }
}

fn parse_copilot_model(model: &Value) -> Option<ModelInfo> {
    if !(model["model_picker_enabled"].as_bool() == Some(true)
        || model["capabilities"]["embeddings"].as_str().is_some())
    {
        return None;
    }

    let Some(model_id) = model["id"].as_str().filter(|id| !id.is_empty()) else {
        warn!("Skipping Copilot model without an id: {model:?}");
        return None;
    };

    let capabilities = &model["capabilities"];
    let limits = &capabilities["limits"];
    let billing = &model["billing"];
    let supports = &capabilities["supports"];

    Some(ModelInfo {
        model_id: model_id.to_string(),
        supports_thinking: model["supports_thinking"]
            .as_bool()
            .or_else(|| capabilities["supports_thinking"].as_bool())
            .or_else(|| supports["thinking"].as_bool()),
        vendor: model["vendor"]
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| model["vendor"].as_str())
            .map(|s| s.to_ascii_lowercase()),
        max_output_tokens: limits["max_output_tokens"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok()),
        supported_endpoints: parse_supported_endpoints(&model["supported_endpoints"]),
        is_chat_default: model["is_chat_default"].as_bool(),
        supports_vision: supports["vision"]
            .as_bool()
            .or_else(|| capabilities["supports_vision"].as_bool()),
        supports_adaptive_thinking: model["supports_adaptive_thinking"]
            .as_bool()
            .or_else(|| capabilities["supports_adaptive_thinking"].as_bool())
            .or_else(|| supports["adaptive_thinking"].as_bool()),
        min_thinking_budget: model["min_thinking_budget"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .or_else(|| {
                supports["min_thinking_budget"]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
            }),
        max_thinking_budget: model["max_thinking_budget"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .or_else(|| {
                supports["max_thinking_budget"]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
            })
            .or_else(|| {
                billing["max_thinking_budget"]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
            }),
        reasoning_effort_levels: supports["reasoning_effort"]
            .as_array()
            .map(|levels| {
                levels
                    .iter()
                    .filter_map(|level| level.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

fn supports_responses_only(endpoints: &[String]) -> bool {
    endpoints.iter().any(|e| e == "/responses")
        && !endpoints.iter().any(|e| e == "/chat/completions")
}

fn parse_supported_endpoints(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|endpoints| {
            endpoints
                .iter()
                .filter_map(|endpoint| {
                    endpoint
                        .as_str()
                        .or_else(|| endpoint.get("path").and_then(Value::as_str))
                        .or_else(|| endpoint.get("url").and_then(Value::as_str))
                        .or_else(|| endpoint.get("endpoint").and_then(Value::as_str))
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn apply_model_limits(
    request: &mut MessagesRequest,
    model_info: Option<&ModelInfo>,
    configured_max_thinking_tokens: u32,
) {
    let Some(model_info) = model_info else {
        return;
    };

    if let Some(model_max) = model_info.max_output_tokens {
        request.max_tokens = Some(
            request
                .max_tokens
                .map_or(model_max, |max| max.min(model_max)),
        );
    }

    if request.thinking.is_none() && model_can_think(model_info) {
        request.thinking = Some(ThinkingConfig {
            r#type: Some(
                if model_info.supports_adaptive_thinking == Some(true) {
                    "adaptive"
                } else {
                    "enabled"
                }
                .to_string(),
            ),
            budget_tokens: if model_info.supports_adaptive_thinking == Some(true) {
                None
            } else {
                compute_thinking_budget(
                    model_info.min_thinking_budget,
                    model_info.max_thinking_budget,
                    request.max_tokens.or(model_info.max_output_tokens),
                    configured_max_thinking_tokens,
                )
            },
        });
    }
}

fn compute_thinking_budget(
    min_thinking_budget: Option<u32>,
    max_thinking_budget: Option<u32>,
    max_output_tokens: Option<u32>,
    configured_max_thinking_tokens: u32,
) -> Option<u32> {
    let available = max_output_tokens.unwrap_or(configured_max_thinking_tokens);
    if available < 2 {
        return None;
    }

    let hard_upper = available.saturating_sub(1);
    let upper = max_thinking_budget
        .unwrap_or(configured_max_thinking_tokens)
        .min(configured_max_thinking_tokens)
        .min(hard_upper);
    if upper == 0 {
        return None;
    }

    let lower = min_thinking_budget.unwrap_or(1024).min(upper);
    Some((available / 2).clamp(lower, upper))
}

fn model_can_think(model_info: &ModelInfo) -> bool {
    model_info.supports_thinking == Some(true)
        || model_info.supports_adaptive_thinking == Some(true)
        || model_info.max_thinking_budget.is_some()
}

fn should_use_interleaved_thinking_beta(model_info: Option<&ModelInfo>) -> bool {
    model_info.is_some_and(|model| {
        model.supports_adaptive_thinking != Some(true) && model_can_think(model)
    })
}

fn disable_eager_input_streaming(body: &mut serde_json::Map<String, Value>) {
    if let Some(Value::Array(tools)) = body.get_mut("tools") {
        for tool in tools {
            if let Value::Object(tool_obj) = tool {
                tool_obj.insert("eager_input_streaming".to_string(), Value::Bool(false));
            }
        }
    }
}

fn copilot_messages_effort(
    request: &MessagesRequest,
    model_info: Option<&ModelInfo>,
) -> Option<String> {
    let requested_effort = if let Some(effort) = request
        .extra
        .get("output_config")
        .and_then(|v| v.get("effort"))
        .and_then(Value::as_str)
        .or_else(|| {
            request
                .extra
                .get("reasoning_effort")
                .and_then(Value::as_str)
        }) {
        effort.to_string()
    } else {
        let has_thinking = request.thinking.is_some() || model_info.is_some_and(model_can_think);
        if !has_thinking {
            return None;
        }

        request
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.budget_tokens)
            .map(thinking_budget_to_effort)
            .unwrap_or_else(|| "medium".to_string())
    };

    Some(select_supported_reasoning_effort(
        &requested_effort,
        model_info,
    ))
}

fn thinking_budget_to_effort(budget_tokens: u32) -> String {
    match budget_tokens {
        0..=2048 => "low",
        2049..=8192 => "medium",
        _ => "high",
    }
    .to_string()
}

fn select_supported_reasoning_effort(
    requested_effort: &str,
    model_info: Option<&ModelInfo>,
) -> String {
    let Some(model_info) = model_info else {
        return requested_effort.to_string();
    };

    let supported = &model_info.reasoning_effort_levels;
    if supported.is_empty() || supported.iter().any(|level| level == requested_effort) {
        return requested_effort.to_string();
    }

    if supported.iter().any(|level| level == "medium") {
        return "medium".to_string();
    }

    supported
        .first()
        .cloned()
        .unwrap_or_else(|| requested_effort.to_string())
}

fn normalize_copilot_messages_thinking(
    body: &mut serde_json::Map<String, Value>,
    effort: Option<&str>,
) {
    let needs_output_effort = if let Some(Value::Object(thinking)) = body.get_mut("thinking") {
        match thinking.get("type").and_then(Value::as_str) {
            Some("enabled") | Some("adaptive") => {
                thinking.insert("type".to_string(), Value::String("adaptive".to_string()));
                thinking.remove("budget_tokens");
                true
            }
            _ => false,
        }
    } else {
        false
    };

    if needs_output_effort {
        let effort = effort.unwrap_or("medium");
        let output_config = body
            .entry("output_config".to_string())
            .or_insert_with(|| Value::Object(serde_json::Map::new()));
        if let Value::Object(config) = output_config {
            config.insert("effort".to_string(), Value::String(effort.to_string()));
        }
    }
}

fn convert_to_openai_chat(req: &MessagesRequest) -> Value {
    let mut messages: Vec<Value> = Vec::new();

    if let Some(ref system) = req.system {
        let text = match system {
            SystemPrompt::Text(s) => s.clone(),
            SystemPrompt::Blocks(blocks) => blocks
                .iter()
                .filter_map(|b| match b {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n"),
        };
        messages.push(serde_json::json!({"role": "system", "content": text}));
    }

    for msg in &req.messages {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };

        match &msg.content {
            MessageContent::Text(text) => {
                messages.push(serde_json::json!({"role": role, "content": text}));
            }
            MessageContent::Blocks(blocks) => {
                let mut parts: Vec<Value> = Vec::new();
                for block in blocks {
                    match block {
                        Content::Text { text } => {
                            parts.push(serde_json::json!({"type": "text", "text": text}));
                        }
                        Content::Thinking { thinking, .. } => {
                            parts.push(serde_json::json!({
                                "type": "text",
                                "text": format!("[thinking]\n{thinking}\n[/thinking]")
                            }));
                        }
                        Content::ToolUse { id, name, input }
                        | Content::ServerToolUse { id, name, input } => {
                            messages.push(serde_json::json!({
                                "role": "assistant",
                                "tool_calls": [{
                                    "id": id,
                                    "type": "function",
                                    "function": {
                                        "name": name,
                                        "arguments": serde_json::to_string(input).unwrap_or_default()
                                    }
                                }]
                            }));
                            continue;
                        }
                        Content::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            let text = match content {
                                Some(Value::String(s)) => s.clone(),
                                Some(v) => v.to_string(),
                                None => String::new(),
                            };
                            let content_str = if *is_error == Some(true) {
                                format!("ERROR: {text}")
                            } else {
                                text
                            };
                            messages.push(serde_json::json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content_str
                            }));
                            continue;
                        }
                        Content::Unknown => {}
                    }
                }
                if !parts.is_empty() {
                    messages.push(serde_json::json!({"role": role, "content": parts}));
                }
            }
        }
    }

    let mut body = serde_json::json!({
        "model": req.model,
        "messages": messages,
        "stream": req.stream,
    });

    if let Some(max_tokens) = req.max_tokens {
        body["max_tokens"] = serde_json::json!(max_tokens);
    }
    if let Some(temperature) = req.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }
    if let Some(top_p) = req.top_p {
        body["top_p"] = serde_json::json!(top_p);
    }
    if let Some(stop) = &req.stop_sequences {
        body["stop"] = serde_json::json!(stop);
    }
    if let Some(tools) = &req.tools {
        let openai_tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema
                    }
                })
            })
            .collect();
        body["tools"] = serde_json::json!(openai_tools);
    }
    if let Some(tc) = &req.tool_choice {
        body["tool_choice"] = tc.clone();
    }
    if let Some(thinking) = &req.thinking {
        let mut tv = serde_json::Map::new();
        if let Some(ref t) = thinking.r#type {
            tv.insert("type".to_string(), serde_json::json!(t));
        }
        if let Some(bt) = thinking.budget_tokens {
            tv.insert("budget_tokens".to_string(), serde_json::json!(bt));
        }
        body["thinking"] = serde_json::json!(tv);
    }

    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_anthropic_sse_accepts_data_without_space() {
        let event = parse_anthropic_sse(
            br#"event: message_delta
data:{"type":"message_delta"}"#,
        );

        assert_eq!(event.event, "message_delta");
        assert_eq!(event.data["type"], "message_delta");
    }

    #[test]
    fn test_parse_copilot_model_extracts_capabilities() {
        let raw = serde_json::json!({
            "id": "claude-sonnet-4",
            "vendor": {"name": "Anthropic"},
            "is_chat_default": true,
            "model_picker_enabled": true,
            "supports_thinking": true,
            "supports_adaptive_thinking": false,
            "min_thinking_budget": 1024,
            "max_thinking_budget": 8192,
            "supported_endpoints": ["/v1/messages", {"path": "/chat/completions"}],
            "capabilities": {
                "limits": {"max_output_tokens": 16384},
                "supports": {
                    "vision": true,
                    "adaptive_thinking": true,
                    "min_thinking_budget": 2048,
                    "max_thinking_budget": 12000,
                    "reasoning_effort": ["low", "medium", "high", "xhigh"]
                }
            }
        });

        let model = parse_copilot_model(&raw).expect("valid model");
        assert_eq!(model.model_id, "claude-sonnet-4");
        assert_eq!(model.vendor.as_deref(), Some("anthropic"));
        assert_eq!(model.max_output_tokens, Some(16384));
        assert_eq!(
            model.supported_endpoints,
            vec!["/v1/messages", "/chat/completions"]
        );
        assert_eq!(model.is_chat_default, Some(true));
        assert_eq!(model.supports_vision, Some(true));
        assert_eq!(model.supports_thinking, Some(true));
        assert_eq!(model.supports_adaptive_thinking, Some(false));
        assert_eq!(model.min_thinking_budget, Some(1024));
        assert_eq!(model.max_thinking_budget, Some(8192));
        assert_eq!(
            model.reasoning_effort_levels,
            vec!["low", "medium", "high", "xhigh"]
        );
    }

    #[test]
    fn test_responses_route_only_when_chat_completions_absent() {
        assert!(supports_responses_only(&["/responses".to_string()]));
        assert!(!supports_responses_only(&[
            "/responses".to_string(),
            "/chat/completions".to_string()
        ]));
        assert!(!supports_responses_only(&["/v1/messages".to_string()]));
    }

    #[test]
    fn test_apply_model_limits_clamps_and_adds_thinking() {
        let model = ModelInfo {
            model_id: "claude-sonnet-4".to_string(),
            supports_thinking: Some(true),
            vendor: Some("anthropic".to_string()),
            max_output_tokens: Some(4096),
            supported_endpoints: vec!["/v1/messages".to_string()],
            is_chat_default: None,
            supports_vision: None,
            supports_adaptive_thinking: Some(false),
            min_thinking_budget: Some(1024),
            max_thinking_budget: Some(2048),
            reasoning_effort_levels: Vec::new(),
        };
        let mut request = MessagesRequest {
            model: model.model_id.clone(),
            system: None,
            messages: vec![],
            max_tokens: Some(8192),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: HashMap::new(),
        };

        apply_model_limits(&mut request, Some(&model), 16_000);

        assert_eq!(request.max_tokens, Some(4096));
        let thinking = request.thinking.expect("thinking inserted");
        assert_eq!(thinking.r#type.as_deref(), Some("enabled"));
        assert_eq!(thinking.budget_tokens, Some(2048));
    }

    #[test]
    fn test_apply_model_limits_uses_adaptive_thinking_without_budget() {
        let model = ModelInfo {
            model_id: "claude-opus-4.7".to_string(),
            supports_thinking: Some(true),
            vendor: Some("anthropic".to_string()),
            max_output_tokens: Some(8192),
            supported_endpoints: vec!["/v1/messages".to_string()],
            is_chat_default: None,
            supports_vision: None,
            supports_adaptive_thinking: Some(true),
            min_thinking_budget: Some(1024),
            max_thinking_budget: Some(4096),
            reasoning_effort_levels: vec!["low".to_string(), "medium".to_string()],
        };
        let mut request = MessagesRequest {
            model: model.model_id.clone(),
            system: None,
            messages: vec![],
            max_tokens: Some(8192),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: HashMap::new(),
        };

        apply_model_limits(&mut request, Some(&model), 16_000);

        let thinking = request.thinking.expect("thinking inserted");
        assert_eq!(thinking.r#type.as_deref(), Some("adaptive"));
        assert_eq!(thinking.budget_tokens, None);
        assert!(!should_use_interleaved_thinking_beta(Some(&model)));
    }

    #[test]
    fn test_copilot_messages_effort_clamps_to_supported_model_levels() {
        let model = ModelInfo {
            model_id: "claude-opus-4.7".to_string(),
            supports_thinking: Some(true),
            vendor: Some("anthropic".to_string()),
            max_output_tokens: Some(8192),
            supported_endpoints: vec!["/v1/messages".to_string()],
            is_chat_default: None,
            supports_vision: None,
            supports_adaptive_thinking: Some(true),
            min_thinking_budget: None,
            max_thinking_budget: None,
            reasoning_effort_levels: vec!["medium".to_string()],
        };
        let mut request = MessagesRequest {
            model: model.model_id.clone(),
            system: None,
            messages: vec![],
            max_tokens: Some(8192),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: None,
            thinking: Some(ThinkingConfig {
                r#type: Some("adaptive".to_string()),
                budget_tokens: None,
            }),
            metadata: None,
            extra: HashMap::from([(
                "output_config".to_string(),
                serde_json::json!({"effort": "high"}),
            )]),
        };

        assert_eq!(
            copilot_messages_effort(&request, Some(&model)).as_deref(),
            Some("medium")
        );

        request.extra.clear();
        request.thinking = Some(ThinkingConfig {
            r#type: Some("enabled".to_string()),
            budget_tokens: Some(12_000),
        });

        assert_eq!(
            copilot_messages_effort(&request, Some(&model)).as_deref(),
            Some("medium")
        );
    }

    #[test]
    fn test_disable_eager_input_streaming_does_not_add_tool_streaming() {
        let mut body = serde_json::json!({
            "model": "claude-sonnet-4",
            "tools": [
                {
                    "name": "example",
                    "input_schema": {"type": "object"}
                }
            ]
        });

        let obj = body.as_object_mut().expect("object body");
        disable_eager_input_streaming(obj);

        assert!(obj.get("tool_streaming").is_none());
        assert_eq!(obj["tools"][0]["eager_input_streaming"], false);
    }

    #[test]
    fn test_normalize_copilot_messages_thinking_uses_adaptive_effort() {
        let mut body = serde_json::json!({
            "model": "claude-opus-4.7",
            "thinking": {
                "type": "enabled",
                "budget_tokens": 8192
            }
        });

        let obj = body.as_object_mut().expect("object body");
        normalize_copilot_messages_thinking(obj, Some("high"));

        assert_eq!(obj["thinking"]["type"], "adaptive");
        assert!(obj["thinking"].get("budget_tokens").is_none());
        assert_eq!(obj["output_config"]["effort"], "high");
    }
}
