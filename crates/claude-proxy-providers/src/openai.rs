//! OpenAI-compatible provider adapter.
//!
//! Converts Anthropic Messages API requests to OpenAI ChatCompletion format
//! and converts streaming responses back to Anthropic SSE format.

mod request;

use std::time::Duration;

use async_trait::async_trait;
use claude_proxy_core::*;
use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::{Level, debug, enabled};

use crate::http::{apply_extra_ca_certs, fmt_reqwest_err, map_upstream_response};
use crate::provider::{Provider, ProviderError};

pub struct OpenAiProvider {
    id: String,
    client: Client,
    base_url: String,
}

const REASONING_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];

fn intent(req: &MessagesRequest) -> Option<&str> {
    req.metadata
        .as_ref()
        .and_then(|metadata| metadata.get("intent"))
        .and_then(Value::as_str)
}

pub(crate) fn apply_openai_intent(mut request: MessagesRequest) -> MessagesRequest {
    let intent = intent(&request).map(str::to_string);
    if let Some(fast_model) = intent
        .as_deref()
        .and_then(|intent| fast_model_for(intent, &request.model))
    {
        request.model = fast_model.to_string();
    }
    apply_reasoning_effort(&mut request, intent.as_deref());
    request
}

fn fast_model_for(intent: &str, model: &str) -> Option<&'static str> {
    if !matches!(intent, "fast" | "quick_reply" | "summarization") {
        return None;
    }
    if model.starts_with("gpt-5.5") || model.starts_with("gpt-5.4") || model.starts_with("gpt-5") {
        Some("gpt-5.4-mini")
    } else {
        None
    }
}

fn apply_reasoning_effort(request: &mut MessagesRequest, intent: Option<&str>) {
    if request.extra.contains_key("reasoning")
        || request.extra.contains_key("reasoning_effort")
        || request.thinking.is_some()
    {
        return;
    }

    let effort = match intent {
        Some("fast" | "quick_reply" | "summarization") => Some("none"),
        Some("deep_think" | "reasoning") => highest_reasoning_effort(&request.model),
        Some("tool_use" | "agent") if supports_reasoning_effort(&request.model, "medium") => {
            Some("medium")
        }
        _ => None,
    };

    if let Some(effort) = effort {
        request
            .extra
            .insert("reasoning_effort".to_string(), json!(effort));
    }
}

fn highest_reasoning_effort(model: &str) -> Option<&'static str> {
    if supports_reasoning_effort(model, "xhigh") {
        Some("xhigh")
    } else if supports_reasoning_effort(model, "high") {
        Some("high")
    } else {
        None
    }
}

fn supports_reasoning_effort(model: &str, effort: &str) -> bool {
    model_reasoning_efforts(model).contains(&effort)
}

fn model_reasoning_efforts(model: &str) -> Vec<&'static str> {
    if is_reasoning_model(model) || model.starts_with("gpt-5") {
        REASONING_EFFORTS.to_vec()
    } else {
        Vec::new()
    }
}

fn is_reasoning_model(model: &str) -> bool {
    model.starts_with("o1") || model.starts_with("o3") || model.starts_with("o4")
}

fn supports_responses(model: &str) -> bool {
    model.starts_with("gpt-5") || is_reasoning_model(model)
}

fn is_codex_model(model: &str) -> bool {
    model.contains("codex")
}

fn supported_endpoints_for(model: &str) -> Vec<String> {
    if supports_responses(model) {
        if is_codex_model(model) {
            vec!["/responses".to_string()]
        } else {
            vec!["/chat/completions".to_string(), "/responses".to_string()]
        }
    } else {
        vec!["/chat/completions".to_string()]
    }
}

fn prefers_responses(model: &str) -> bool {
    supports_responses(model)
}

pub(crate) fn openai_model_info(model_id: &str) -> ModelInfo {
    let reasoning_efforts = model_reasoning_efforts(model_id)
        .into_iter()
        .map(str::to_string)
        .collect::<Vec<_>>();
    let supported_endpoints = supported_endpoints_for(model_id);

    ModelInfo {
        model_id: model_id.to_string(),
        supports_thinking: (!reasoning_efforts.is_empty()).then_some(true),
        vendor: Some("openai".to_string()),
        max_output_tokens: if model_id.starts_with("gpt-5.5") {
            Some(128_000)
        } else if model_id.contains("mini") {
            Some(16_384)
        } else {
            None
        },
        supported_endpoints,
        is_chat_default: None,
        supports_vision: None,
        supports_adaptive_thinking: None,
        min_thinking_budget: None,
        max_thinking_budget: None,
        reasoning_effort_levels: reasoning_efforts,
    }
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

        debug!(
            "OpenAI request: model={} stream={} messages={}",
            body.get("model").and_then(|v| v.as_str()).unwrap_or("?"),
            request.stream,
            body.get("messages")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0)
        );

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
            Ok(stream_openai_response(response))
        } else {
            let body = response
                .text()
                .await
                .map_err(|e| ProviderError::Network(fmt_reqwest_err(&e)))?;
            let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let events = convert_non_streaming_response(&data);
            let stream = futures::stream::iter(events.into_iter().map(Ok));
            Ok(Box::pin(stream))
        }
    }

    async fn chat_via_responses(
        &self,
        request: MessagesRequest,
    ) -> Result<BoxStream<'static, Result<SseEvent, ProviderError>>, ProviderError> {
        let body = crate::copilot::responses::convert_to_responses(&request);
        let url = format!("{}/responses", self.base_url);

        debug!(
            "OpenAI responses request: model={} stream={} input_items={}",
            body.get("model").and_then(|v| v.as_str()).unwrap_or("?"),
            request.stream,
            body.get("input")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0)
        );

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
            Ok(crate::copilot::responses::stream_responses_response(
                response,
            ))
        } else {
            let body = response
                .text()
                .await
                .map_err(|e| ProviderError::Network(fmt_reqwest_err(&e)))?;
            let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let events = crate::copilot::responses::convert_non_streaming_response(&data);
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

// --- SSE Conversion ---

/// Parsed OpenAI streaming chunk.
#[derive(Debug)]
pub struct OpenAiChunk {
    #[allow(dead_code)]
    id: String,
    model: String,
    choices: Vec<OpenAiChoice>,
    usage: Option<OpenAiUsage>,
}

#[derive(Debug)]
struct OpenAiUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
}

#[derive(Debug)]
struct OpenAiChoice {
    #[allow(dead_code)]
    index: u32,
    delta: OpenAiDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Default)]
struct OpenAiDelta {
    #[allow(dead_code)]
    role: Option<String>,
    content: Option<String>,
    reasoning_content: Option<String>,
    tool_calls: Option<Vec<OpenAiToolCall>>,
}

#[derive(Debug, Clone)]
struct OpenAiToolCall {
    index: u32,
    id: Option<String>,
    function: OpenAiFunction,
}

#[derive(Debug, Clone)]
struct OpenAiFunction {
    name: Option<String>,
    arguments: String,
}

/// Spawn a task that parses an OpenAI-format SSE byte stream and converts to Anthropic SSE events.
/// Returns a pinned BoxStream. Used by both OpenAI and Copilot providers.
pub fn stream_openai_response(
    response: reqwest::Response,
) -> BoxStream<'static, Result<SseEvent, ProviderError>> {
    let (tx, rx) = mpsc::channel::<Result<SseEvent, ProviderError>>(64);

    tokio::spawn(async move {
        let mut converter = StreamConverter::new();
        let mut buffer = String::new();
        let mut byte_stream = response.bytes_stream();

        while let Some(chunk_result) = byte_stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    buffer.push_str(&String::from_utf8_lossy(&chunk));

                    while let Some(pos) = buffer.find("\n\n") {
                        let event_str = buffer[..pos].to_string();
                        buffer = buffer[pos + 2..].to_string();

                        if let Some(openai_chunk) = parse_openai_chunk(&event_str) {
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
    Box::pin(stream)
}

/// Stateful converter from OpenAI streaming chunks to Anthropic SSE events.
pub struct StreamConverter {
    message_id: String,
    model: String,
    content_blocks: Vec<ContentBlockState>,
    current_text_index: Option<u32>,
    current_thinking_index: Option<u32>,
    tool_call_indices: std::collections::HashMap<u32, u32>, // tool_call.index -> content_block index
    started: bool,
    input_tokens: u32,
    output_tokens: u32,
}

#[derive(Debug)]
struct ContentBlockState {
    #[allow(dead_code)]
    block_type: String,
    #[allow(dead_code)]
    index: u32,
}

impl Default for StreamConverter {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamConverter {
    pub fn new() -> Self {
        Self {
            message_id: format!("msg_{}", uuid::Uuid::new_v4().to_string().replace('-', "")),
            model: String::new(),
            content_blocks: Vec::new(),
            current_text_index: None,
            current_thinking_index: None,
            tool_call_indices: std::collections::HashMap::new(),
            started: false,
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    pub fn process_chunk(&mut self, chunk: &OpenAiChunk) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // Extract usage if present in this chunk (OpenAI sends it in the final chunk)
        if let Some(ref usage) = chunk.usage {
            self.input_tokens = usage.prompt_tokens;
            self.output_tokens = usage.completion_tokens;
        }

        if !self.started {
            self.model = chunk.model.clone();
            events.push(SseEvent {
                event: "message_start".to_string(),
                data: json!({
                    "type": "message_start",
                    "message": {
                        "id": self.message_id,
                        "type": "message",
                        "role": "assistant",
                        "content": [],
                        "model": self.model,
                        "stop_reason": null,
                        "stop_sequence": null,
                        "usage": {"input_tokens": self.input_tokens, "output_tokens": 0}
                    }
                }),
            });
            self.started = true;
        }

        for choice in &chunk.choices {
            // Handle reasoning/thinking content (DeepSeek-style)
            if let Some(ref reasoning) = choice.delta.reasoning_content
                && !reasoning.is_empty()
            {
                if self.current_thinking_index.is_none() {
                    let idx = self.content_blocks.len() as u32;
                    self.current_thinking_index = Some(idx);
                    self.content_blocks.push(ContentBlockState {
                        block_type: "thinking".to_string(),
                        index: idx,
                    });
                    events.push(SseEvent {
                        event: "content_block_start".to_string(),
                        data: json!({
                            "type": "content_block_start",
                            "index": idx,
                            "content_block": {"type": "thinking", "thinking": ""}
                        }),
                    });
                }
                let idx = self.current_thinking_index.unwrap();
                events.push(SseEvent {
                    event: "content_block_delta".to_string(),
                    data: json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": {"type": "thinking_delta", "thinking": reasoning}
                    }),
                });
            }

            // Handle text content
            if let Some(ref content) = choice.delta.content
                && !content.is_empty()
            {
                // Close thinking block if open
                if self.current_thinking_index.is_some() {
                    let idx = self.current_thinking_index.take().unwrap();
                    events.push(SseEvent {
                        event: "content_block_stop".to_string(),
                        data: json!({"type": "content_block_stop", "index": idx}),
                    });
                }

                if self.current_text_index.is_none() {
                    let idx = self.content_blocks.len() as u32;
                    self.current_text_index = Some(idx);
                    self.content_blocks.push(ContentBlockState {
                        block_type: "text".to_string(),
                        index: idx,
                    });
                    events.push(SseEvent {
                        event: "content_block_start".to_string(),
                        data: json!({
                            "type": "content_block_start",
                            "index": idx,
                            "content_block": {"type": "text", "text": ""}
                        }),
                    });
                }
                let idx = self.current_text_index.unwrap();
                events.push(SseEvent {
                    event: "content_block_delta".to_string(),
                    data: json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": {"type": "text_delta", "text": content}
                    }),
                });
            }

            // Handle tool calls
            if let Some(ref tool_calls) = choice.delta.tool_calls {
                for tc in tool_calls {
                    // Close text/thinking blocks if open
                    if self.current_text_index.is_some() {
                        let idx = self.current_text_index.take().unwrap();
                        events.push(SseEvent {
                            event: "content_block_stop".to_string(),
                            data: json!({"type": "content_block_stop", "index": idx}),
                        });
                    }
                    if self.current_thinking_index.is_some() {
                        let idx = self.current_thinking_index.take().unwrap();
                        events.push(SseEvent {
                            event: "content_block_stop".to_string(),
                            data: json!({"type": "content_block_stop", "index": idx}),
                        });
                    }

                    let block_idx = if let Some(&idx) = self.tool_call_indices.get(&tc.index) {
                        idx
                    } else {
                        let idx = self.content_blocks.len() as u32;
                        self.tool_call_indices.insert(tc.index, idx);
                        self.content_blocks.push(ContentBlockState {
                            block_type: "tool_use".to_string(),
                            index: idx,
                        });

                        // Emit content_block_start with tool_use
                        let tool_id = tc
                            .id
                            .clone()
                            .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4()));
                        let tool_name = tc.function.name.clone().unwrap_or_default();
                        events.push(SseEvent {
                            event: "content_block_start".to_string(),
                            data: json!({
                                "type": "content_block_start",
                                "index": idx,
                                "content_block": {
                                    "type": "tool_use",
                                    "id": tool_id,
                                    "name": tool_name,
                                    "input": {}
                                }
                            }),
                        });
                        idx
                    };

                    // Emit argument deltas
                    if !tc.function.arguments.is_empty() {
                        events.push(SseEvent {
                            event: "content_block_delta".to_string(),
                            data: json!({
                                "type": "content_block_delta",
                                "index": block_idx,
                                "delta": {
                                    "type": "input_json_delta",
                                    "partial_json": tc.function.arguments
                                }
                            }),
                        });
                    }
                }
            }

            // Handle finish_reason
            if let Some(ref reason) = choice.finish_reason {
                // Close any open blocks
                if self.current_text_index.is_some() {
                    let idx = self.current_text_index.take().unwrap();
                    events.push(SseEvent {
                        event: "content_block_stop".to_string(),
                        data: json!({"type": "content_block_stop", "index": idx}),
                    });
                }
                if self.current_thinking_index.is_some() {
                    let idx = self.current_thinking_index.take().unwrap();
                    events.push(SseEvent {
                        event: "content_block_stop".to_string(),
                        data: json!({"type": "content_block_stop", "index": idx}),
                    });
                }
                for &idx in self.tool_call_indices.values() {
                    events.push(SseEvent {
                        event: "content_block_stop".to_string(),
                        data: json!({"type": "content_block_stop", "index": idx}),
                    });
                }
                self.tool_call_indices.clear();

                let stop_reason = match reason.as_str() {
                    "stop" => "end_turn",
                    "length" => "max_tokens",
                    "tool_calls" => "tool_use",
                    other => other,
                };

                events.push(SseEvent {
                    event: "message_delta".to_string(),
                    data: json!({
                        "type": "message_delta",
                        "delta": {
                            "stop_reason": stop_reason,
                            "stop_sequence": null
                        },
                        "usage": {"output_tokens": self.output_tokens, "input_tokens": self.input_tokens}
                    }),
                });

                events.push(SseEvent {
                    event: "message_stop".to_string(),
                    data: json!({"type": "message_stop"}),
                });
            }
        }

        events
    }

    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();

        // Close any remaining open blocks
        if self.current_text_index.is_some() {
            let idx = self.current_text_index.take().unwrap();
            events.push(SseEvent {
                event: "content_block_stop".to_string(),
                data: json!({"type": "content_block_stop", "index": idx}),
            });
        }
        if self.current_thinking_index.is_some() {
            let idx = self.current_thinking_index.take().unwrap();
            events.push(SseEvent {
                event: "content_block_stop".to_string(),
                data: json!({"type": "content_block_stop", "index": idx}),
            });
        }

        // If we never got a finish_reason, send message_stop
        if self.started {
            events.push(SseEvent {
                event: "message_delta".to_string(),
                data: json!({
                    "type": "message_delta",
                    "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                    "usage": {"output_tokens": self.output_tokens, "input_tokens": self.input_tokens}
                }),
            });
            events.push(SseEvent {
                event: "message_stop".to_string(),
                data: json!({"type": "message_stop"}),
            });
        }

        events
    }
}

/// Parse raw SSE text into an OpenAI chunk.
pub fn parse_openai_chunk(text: &str) -> Option<OpenAiChunk> {
    let mut data_str = None;

    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("data: ") {
            let trimmed = rest.trim();
            if trimmed == "[DONE]" {
                return None;
            }
            data_str = Some(trimmed.to_string());
        }
    }

    let data: Value = serde_json::from_str(&data_str?).ok()?;

    let id = data["id"].as_str().unwrap_or("").to_string();
    let model = data["model"].as_str().unwrap_or("").to_string();

    let choices = data["choices"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[])
        .iter()
        .map(|c| {
            let index = c["index"].as_u64().unwrap_or(0) as u32;
            let delta = &c["delta"];

            let role = delta["role"].as_str().map(|s| s.to_string());
            let content = delta["content"].as_str().map(|s| s.to_string());
            let reasoning_content = delta["reasoning_content"].as_str().map(|s| s.to_string());

            let tool_calls = delta["tool_calls"].as_array().map(|arr| {
                arr.iter()
                    .map(|tc| {
                        let idx = tc["index"].as_u64().unwrap_or(0) as u32;
                        let id = tc["id"].as_str().map(|s| s.to_string());
                        let name = tc["function"]["name"].as_str().map(|s| s.to_string());
                        let arguments = tc["function"]["arguments"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();

                        OpenAiToolCall {
                            index: idx,
                            id,
                            function: OpenAiFunction { name, arguments },
                        }
                    })
                    .collect()
            });

            let finish_reason = c["finish_reason"].as_str().map(|s| s.to_string());

            OpenAiChoice {
                index,
                delta: OpenAiDelta {
                    role,
                    content,
                    reasoning_content,
                    tool_calls,
                },
                finish_reason,
            }
        })
        .collect();

    let usage = data.get("usage").map(|u| OpenAiUsage {
        prompt_tokens: u["prompt_tokens"].as_u64().unwrap_or(0) as u32,
        completion_tokens: u["completion_tokens"].as_u64().unwrap_or(0) as u32,
    });

    Some(OpenAiChunk {
        id,
        model,
        choices,
        usage,
    })
}

/// Convert a non-streaming OpenAI response to Anthropic format.
pub fn convert_non_streaming_response(data: &Value) -> Vec<SseEvent> {
    let message_id = format!("msg_{}", uuid::Uuid::new_v4().to_string().replace('-', ""));
    let model = data["model"].as_str().unwrap_or("unknown").to_string();

    let mut events = Vec::new();

    events.push(SseEvent {
        event: "message_start".to_string(),
        data: json!({
            "type": "message_start",
            "message": {
                "id": message_id,
                "type": "message",
                "role": "assistant",
                "content": [],
                "model": model,
                "stop_reason": null,
                "stop_sequence": null,
                "usage": {
                    "input_tokens": data["usage"]["prompt_tokens"],
                    "output_tokens": 0
                }
            }
        }),
    });

    let mut block_index = 0u32;

    if let Some(choices) = data["choices"].as_array()
        && let Some(choice) = choices.first()
    {
        let message = &choice["message"];

        // Handle reasoning content
        if let Some(reasoning) = message["reasoning_content"].as_str()
            && !reasoning.is_empty()
        {
            events.push(SseEvent {
                event: "content_block_start".to_string(),
                data: json!({
                    "type": "content_block_start",
                    "index": block_index,
                    "content_block": {"type": "thinking", "thinking": ""}
                }),
            });
            events.push(SseEvent {
                event: "content_block_delta".to_string(),
                data: json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": {"type": "thinking_delta", "thinking": reasoning}
                }),
            });
            events.push(SseEvent {
                event: "content_block_stop".to_string(),
                data: json!({"type": "content_block_stop", "index": block_index}),
            });
            block_index += 1;
        }

        // Handle text content
        if let Some(content) = message["content"].as_str()
            && !content.is_empty()
        {
            events.push(SseEvent {
                event: "content_block_start".to_string(),
                data: json!({
                    "type": "content_block_start",
                    "index": block_index,
                    "content_block": {"type": "text", "text": ""}
                }),
            });
            events.push(SseEvent {
                event: "content_block_delta".to_string(),
                data: json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": {"type": "text_delta", "text": content}
                }),
            });
            events.push(SseEvent {
                event: "content_block_stop".to_string(),
                data: json!({"type": "content_block_stop", "index": block_index}),
            });
            block_index += 1;
        }

        // Handle tool calls
        if let Some(tool_calls) = message["tool_calls"].as_array() {
            for tc in tool_calls {
                let tool_id = tc["id"].as_str().unwrap_or("call_unknown").to_string();
                let tool_name = tc["function"]["name"].as_str().unwrap_or("").to_string();
                let arguments = tc["function"]["arguments"].as_str().unwrap_or("{}");

                let input: Value = serde_json::from_str(arguments).unwrap_or(json!({}));

                events.push(SseEvent {
                    event: "content_block_start".to_string(),
                    data: json!({
                        "type": "content_block_start",
                        "index": block_index,
                        "content_block": {
                            "type": "tool_use",
                            "id": tool_id,
                            "name": tool_name,
                            "input": input
                        }
                    }),
                });
                events.push(SseEvent {
                    event: "content_block_stop".to_string(),
                    data: json!({"type": "content_block_stop", "index": block_index}),
                });
                block_index += 1;
            }
        }

        // Stop reason
        let finish = choice["finish_reason"].as_str().unwrap_or("stop");
        let stop_reason = match finish {
            "stop" => "end_turn",
            "length" => "max_tokens",
            "tool_calls" => "tool_use",
            other => other,
        };

        events.push(SseEvent {
            event: "message_delta".to_string(),
            data: json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason, "stop_sequence": null},
                "usage": {"output_tokens": data["usage"]["completion_tokens"]}
            }),
        });
    }

    events.push(SseEvent {
        event: "message_stop".to_string(),
        data: json!({"type": "message_stop"}),
    });

    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_openai_chunk_text() {
        let text = r#"data: {"id":"chatcmpl-123","object":"chat.completion.chunk","model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}"#;
        let chunk = parse_openai_chunk(text).unwrap();
        assert_eq!(chunk.id, "chatcmpl-123");
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("Hello"));
    }

    #[test]
    fn test_parse_openai_chunk_done() {
        let text = "data: [DONE]";
        assert!(parse_openai_chunk(text).is_none());
    }

    #[test]
    fn test_stream_converter_text() {
        let mut converter = StreamConverter::new();

        let chunk = OpenAiChunk {
            id: "test".to_string(),
            model: "gpt-4".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                delta: OpenAiDelta {
                    role: Some("assistant".to_string()),
                    content: None,
                    reasoning_content: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let events = converter.process_chunk(&chunk);
        assert_eq!(events.len(), 1); // message_start
        assert_eq!(events[0].event, "message_start");

        let chunk2 = OpenAiChunk {
            id: "test".to_string(),
            model: "gpt-4".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                delta: OpenAiDelta {
                    role: None,
                    content: Some("Hello world".to_string()),
                    reasoning_content: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let events = converter.process_chunk(&chunk2);
        assert_eq!(events.len(), 2); // content_block_start + content_block_delta
        assert_eq!(events[0].event, "content_block_start");
        assert_eq!(events[1].event, "content_block_delta");
    }

    #[test]
    fn test_stream_converter_thinking() {
        let mut converter = StreamConverter::new();

        let chunk = OpenAiChunk {
            id: "test".to_string(),
            model: "deepseek-r1".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                delta: OpenAiDelta {
                    role: Some("assistant".to_string()),
                    content: None,
                    reasoning_content: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };
        converter.process_chunk(&chunk);

        let chunk2 = OpenAiChunk {
            id: "test".to_string(),
            model: "deepseek-r1".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                delta: OpenAiDelta {
                    role: None,
                    content: None,
                    reasoning_content: Some("Let me think...".to_string()),
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let events = converter.process_chunk(&chunk2);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].event, "content_block_start");
        assert_eq!(events[1].event, "content_block_delta");
    }

    #[test]
    fn known_gpt5_model_prefers_responses() {
        let info = openai_model_info("gpt-5.5");

        assert!(prefers_responses("gpt-5.5"));
        assert_eq!(info.max_output_tokens, Some(128_000));
        assert_eq!(
            info.supported_endpoints,
            vec!["/chat/completions", "/responses"]
        );
        assert_eq!(
            info.reasoning_effort_levels,
            vec!["low", "medium", "high", "xhigh"]
        );
    }

    #[test]
    fn non_reasoning_model_keeps_chat_completions() {
        let info = openai_model_info("gpt-4.1");

        assert!(!prefers_responses("gpt-4.1"));
        assert_eq!(info.supported_endpoints, vec!["/chat/completions"]);
        assert!(info.reasoning_effort_levels.is_empty());
    }

    #[test]
    fn codex_model_uses_responses_endpoint_only() {
        let info = openai_model_info("gpt-5.3-codex");

        assert!(prefers_responses("gpt-5.3-codex"));
        assert_eq!(info.supported_endpoints, vec!["/responses"]);
    }

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

    #[test]
    fn intent_fast_selects_fast_model_and_disables_reasoning() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("hi".to_string()),
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
            metadata: Some(json!({"intent": "fast"})),
            extra: Default::default(),
        };

        let req = apply_openai_intent(req);

        assert_eq!(req.model, "gpt-5.4-mini");
        assert_eq!(
            req.extra.get("reasoning_effort").and_then(Value::as_str),
            Some("none")
        );
    }

    #[test]
    fn intent_deep_think_uses_highest_effort() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("think".to_string()),
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
            metadata: Some(json!({"intent": "deep_think"})),
            extra: Default::default(),
        };

        let req = apply_openai_intent(req);

        assert_eq!(
            req.extra.get("reasoning_effort").and_then(Value::as_str),
            Some("xhigh")
        );
    }
}
