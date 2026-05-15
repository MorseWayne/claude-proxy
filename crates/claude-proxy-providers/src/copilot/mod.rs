pub mod auth;
pub mod headers;
mod preprocess;

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
use tracing::{debug, info};

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

    fn build_headers(&self, token: &str, vision: bool) -> HeaderMap {
        let hb = self.header_builder.try_read().expect("header_builder lock");
        let headers_vec = hb.build_headers(token, None, vision);
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
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let url = format!("{}/v1/messages", self.base_url);
        let vision = Self::has_vision_content(&request.messages);
        let headers = self.build_headers(token, vision);

        let body = serde_json::to_value(&request)
            .map_err(|e| ProviderError::Network(format!("serialize error: {e}")))?;

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
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let url = format!("{}/chat/completions", self.base_url);
        let vision = Self::has_vision_content(&request.messages);
        let headers = self.build_headers(token, vision);
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
            let (tx, rx) = tokio::sync::mpsc::channel::<Result<SseEvent, ProviderError>>(64);

            tokio::spawn(async move {
                let mut converter = super::openai::StreamConverter::new();
                let mut buffer = String::new();
                let mut byte_stream = response.bytes_stream();

                while let Some(chunk_result) = byte_stream.next().await {
                    match chunk_result {
                        Ok(chunk) => {
                            buffer.push_str(&String::from_utf8_lossy(&chunk));
                            while let Some(pos) = buffer.find("\n\n") {
                                let event_str = buffer[..pos].to_string();
                                buffer = buffer[pos + 2..].to_string();

                                if let Some(openai_chunk) =
                                    crate::openai::parse_openai_chunk(&event_str)
                                {
                                    let events = converter.process_chunk(&openai_chunk);
                                    for event in events {
                                        if tx.send(Ok(event)).await.is_err() {
                                            return;
                                        }
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            let _ = tx
                                .send(Err(ProviderError::Network(fmt_reqwest_err(&e))))
                                .await;
                            return;
                        }
                    }
                }

                for event in converter.finish() {
                    if tx.send(Ok(event)).await.is_err() {
                        break;
                    }
                }
            });

            let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
            Ok(Box::pin(stream))
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

        if endpoints.iter().any(|e| e == "/v1/messages") {
            debug!("Using native /v1/messages path for model {}", request.model);
            self.chat_via_messages(request, &token).await
        } else {
            debug!(
                "Falling back to /chat/completions for model {}",
                request.model
            );
            self.chat_via_completions(request, &token).await
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
            .unwrap_or(&vec![])
            .iter()
            .filter(|m| {
                m["model_picker_enabled"].as_bool() == Some(true)
                    || m["capabilities"]["embeddings"].as_str().is_some()
            })
            .map(|m| {
                let model_id = m["id"].as_str().unwrap_or("unknown").to_string();
                let supports_thinking = m["capabilities"]
                    .get("supports_tool_calls")
                    .and_then(|v| v.as_bool());

                let _endpoints: Vec<String> = m["supported_endpoints"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|e| e.as_str().map(|s| s.to_string()))
                            .collect()
                    })
                    .unwrap_or_default();

                ModelInfo {
                    model_id,
                    supports_thinking,
                }
            })
            .collect();

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
        } else if let Some(rest) = line.strip_prefix("data: ")
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
