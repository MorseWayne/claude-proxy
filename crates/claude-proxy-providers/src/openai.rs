//! OpenAI-compatible provider adapter.
//!
//! Converts Anthropic Messages API requests to OpenAI ChatCompletion format
//! and converts streaming responses back to Anthropic SSE format.

use std::time::Duration;

use async_trait::async_trait;
use claude_proxy_core::*;
use futures::StreamExt;
use futures::stream::BoxStream;
use reqwest::Client;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tracing::debug;

use crate::provider::{Provider, ProviderError};

pub struct OpenAiProvider {
    id: String,
    client: Client,
    base_url: String,
}

impl OpenAiProvider {
    pub fn new(
        id: &str,
        api_key: &str,
        base_url: &str,
        proxy: &str,
        connect_timeout: u64,
        read_timeout: u64,
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

        let client = builder
            .build()
            .map_err(|e| ProviderError::Network(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            id: id.to_string(),
            client,
            base_url: base_url.trim_end_matches('/').to_string(),
        })
    }

    /// Convert an Anthropic MessagesRequest to an OpenAI ChatCompletion request body.
    fn convert_request(&self, req: &MessagesRequest) -> Value {
        let mut messages: Vec<Value> = Vec::new();

        // System prompt
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
            messages.push(json!({"role": "system", "content": text}));
        }

        // Convert messages
        for msg in &req.messages {
            let role = match msg.role {
                Role::User => "user",
                Role::Assistant => "assistant",
            };

            match &msg.content {
                MessageContent::Text(text) => {
                    messages.push(json!({"role": role, "content": text}));
                }
                MessageContent::Blocks(blocks) => {
                    let mut parts: Vec<Value> = Vec::new();
                    for block in blocks {
                        match block {
                            Content::Text { text } => {
                                parts.push(json!({"type": "text", "text": text}));
                            }
                            Content::Thinking { thinking, .. } => {
                                parts.push(json!({"type": "text", "text": format!("[thinking]\n{thinking}\n[/thinking]")}));
                            }
                            Content::ToolUse { id, name, input }
                            | Content::ServerToolUse { id, name, input } => {
                                messages.push(json!({
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
                                messages.push(json!({
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
                        messages.push(json!({"role": role, "content": parts}));
                    }
                }
            }
        }

        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "stream": req.stream,
        });

        if let Some(max_tokens) = req.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }
        if let Some(temperature) = req.temperature {
            body["temperature"] = json!(temperature);
        }
        if let Some(top_p) = req.top_p {
            body["top_p"] = json!(top_p);
        }
        if let Some(stop) = &req.stop_sequences {
            body["stop"] = json!(stop);
        }

        // Convert Anthropic tools to OpenAI format
        if let Some(tools) = &req.tools {
            let openai_tools: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.input_schema
                        }
                    })
                })
                .collect();
            body["tools"] = json!(openai_tools);
        }

        if let Some(tc) = &req.tool_choice {
            body["tool_choice"] = tc.clone();
        }

        if let Some(thinking) = &req.thinking {
            let mut thinking_value = serde_json::Map::new();
            if let Some(ref t) = thinking.r#type {
                thinking_value.insert("type".to_string(), json!(t));
            }
            if let Some(bt) = thinking.budget_tokens {
                thinking_value.insert("budget_tokens".to_string(), json!(bt));
            }
            body["thinking"] = json!(thinking_value);
        }

        body
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
        let body = self.convert_request(&request);
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
                    ProviderError::Network(e.to_string())
                }
            })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body_text = response.text().await.unwrap_or_default();
            return Err(match status {
                401 => ProviderError::Authentication(body_text),
                429 => ProviderError::RateLimited,
                404 => ProviderError::ModelNotFound(body_text),
                _ => ProviderError::UpstreamError {
                    status,
                    body: body_text,
                },
            });
        }

        if request.stream {
            let (tx, rx) = mpsc::channel::<Result<SseEvent, ProviderError>>(64);

            // Spawn a task to parse the SSE stream and convert to Anthropic format
            tokio::spawn(async move {
                let mut converter = StreamConverter::new();
                let mut buffer = String::new();
                let mut byte_stream = response.bytes_stream();

                while let Some(chunk_result) = byte_stream.next().await {
                    match chunk_result {
                        Ok(chunk) => {
                            buffer.push_str(&String::from_utf8_lossy(&chunk));

                            // Process complete SSE events (separated by double newline)
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
                            let _ = tx.send(Err(ProviderError::Network(e.to_string()))).await;
                            return;
                        }
                    }
                }

                // Send any remaining events
                for event in converter.finish() {
                    if tx.send(Ok(event)).await.is_err() {
                        break;
                    }
                }
            });

            let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
            Ok(Box::pin(stream))
        } else {
            // Non-streaming
            let body = response
                .text()
                .await
                .map_err(|e| ProviderError::Network(e.to_string()))?;
            let data: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let events = convert_non_streaming_response(&data);
            let stream = futures::stream::iter(events.into_iter().map(Ok));
            Ok(Box::pin(stream))
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        let url = format!("{}/models", self.base_url);
        let response = self.client.get(&url).send().await.map_err(|e| {
            if e.is_timeout() {
                ProviderError::Timeout
            } else {
                ProviderError::Network(e.to_string())
            }
        })?;

        if !response.status().is_success() {
            let status = response.status().as_u16();
            let body = response.text().await.unwrap_or_default();
            return Err(match status {
                401 => ProviderError::Authentication(body),
                _ => ProviderError::UpstreamError { status, body },
            });
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
                m["id"].as_str().map(|id| ModelInfo {
                    model_id: id.to_string(),
                    supports_thinking: None,
                })
            })
            .collect();

        Ok(models)
    }
}

// --- SSE Conversion ---

/// Parsed OpenAI streaming chunk.
#[derive(Debug)]
struct OpenAiChunk {
    #[allow(dead_code)]
    id: String,
    model: String,
    choices: Vec<OpenAiChoice>,
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

/// Stateful converter from OpenAI streaming chunks to Anthropic SSE events.
struct StreamConverter {
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

impl StreamConverter {
    fn new() -> Self {
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

    fn process_chunk(&mut self, chunk: &OpenAiChunk) -> Vec<SseEvent> {
        let mut events = Vec::new();

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
                        "usage": {"output_tokens": self.output_tokens}
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

    fn finish(&mut self) -> Vec<SseEvent> {
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
                    "usage": {"output_tokens": self.output_tokens}
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
fn parse_openai_chunk(text: &str) -> Option<OpenAiChunk> {
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

    Some(OpenAiChunk { id, model, choices })
}

/// Convert a non-streaming OpenAI response to Anthropic format.
fn convert_non_streaming_response(data: &Value) -> Vec<SseEvent> {
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
        };
        let events = converter.process_chunk(&chunk2);
        assert_eq!(events.len(), 2); // content_block_start + content_block_delta
        assert_eq!(events[0].event, "content_block_start");
        assert_eq!(events[1].event, "content_block_delta");
    }

    #[test]
    fn test_stream_converter_thinking() {
        let mut converter = StreamConverter::new();

        // First chunk with role
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
        };
        converter.process_chunk(&chunk);

        // Reasoning content
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
        };
        let events = converter.process_chunk(&chunk2);
        assert_eq!(events.len(), 2); // content_block_start (thinking) + content_block_delta
        assert_eq!(events[0].event, "content_block_start");
        assert_eq!(events[1].event, "content_block_delta");
    }
}
