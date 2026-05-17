//! OpenAI-compatible Chat Completions response conversion.
//!
//! Converts OpenAI-format chat completion responses into Anthropic SSE events.

use std::collections::HashMap;

use claude_proxy_core::SseEvent;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::http::{fmt_reqwest_err, next_upstream_stream_item};
use crate::provider::ProviderError;
use crate::tool_args::sanitize_tool_arguments;

// --- SSE Conversion ---

/// Parsed OpenAI streaming chunk.
#[derive(Debug)]
struct OpenAiChunk {
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
pub(crate) fn stream_openai_response(
    response: reqwest::Response,
) -> BoxStream<'static, Result<SseEvent, ProviderError>> {
    let (tx, rx) = mpsc::channel::<Result<SseEvent, ProviderError>>(64);

    tokio::spawn(async move {
        let mut converter = StreamConverter::new();
        let mut buffer = String::new();
        let mut byte_stream = response.bytes_stream();

        loop {
            let chunk_result = match next_upstream_stream_item(byte_stream.next()).await {
                Ok(Some(chunk_result)) => chunk_result,
                Ok(None) => break,
                Err(error) => {
                    let _ = tx.send(Err(error)).await;
                    return;
                }
            };

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
struct StreamConverter {
    message_id: String,
    model: String,
    content_blocks: Vec<ContentBlockState>,
    current_text_index: Option<u32>,
    current_thinking_index: Option<u32>,
    tool_call_indices: HashMap<u32, u32>, // tool_call.index -> content_block index
    tool_call_names: HashMap<u32, String>,
    tool_argument_buffers: HashMap<u32, String>,
    tool_argument_emitted: HashMap<u32, String>,
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
            tool_call_indices: HashMap::new(),
            tool_call_names: HashMap::new(),
            tool_argument_buffers: HashMap::new(),
            tool_argument_emitted: HashMap::new(),
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
                    if let Some(tool_name) = tc.function.name.as_ref() {
                        self.tool_call_names.insert(tc.index, tool_name.clone());
                    }

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
                        let tool_name = self
                            .tool_call_names
                            .get(&tc.index)
                            .cloned()
                            .unwrap_or_default();
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
                        self.tool_argument_buffers
                            .entry(tc.index)
                            .or_default()
                            .push_str(&tc.function.arguments);
                        self.emit_tool_arguments(tc.index, block_idx, true, &mut events);
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
                let tool_blocks: Vec<(u32, u32)> = self
                    .tool_call_indices
                    .iter()
                    .map(|(&tool_index, &block_index)| (tool_index, block_index))
                    .collect();
                for (tool_index, idx) in &tool_blocks {
                    self.emit_tool_arguments(*tool_index, *idx, false, &mut events);
                }
                for (_, idx) in tool_blocks {
                    events.push(SseEvent {
                        event: "content_block_stop".to_string(),
                        data: json!({"type": "content_block_stop", "index": idx}),
                    });
                }
                self.tool_call_indices.clear();
                self.tool_call_names.clear();
                self.tool_argument_buffers.clear();
                self.tool_argument_emitted.clear();

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

    fn emit_tool_arguments(
        &mut self,
        tool_index: u32,
        block_idx: u32,
        require_valid_json: bool,
        events: &mut Vec<SseEvent>,
    ) {
        let Some(arguments) = self.tool_argument_buffers.get(&tool_index).cloned() else {
            return;
        };
        if arguments.is_empty() {
            return;
        }
        if require_valid_json && serde_json::from_str::<Value>(&arguments).is_err() {
            return;
        }

        let tool_name = self
            .tool_call_names
            .get(&tool_index)
            .map(String::as_str)
            .unwrap_or_default();
        let sanitized = sanitize_tool_arguments(tool_name, &arguments).unwrap_or(arguments);
        let previous = self
            .tool_argument_emitted
            .get(&tool_index)
            .map(String::as_str)
            .unwrap_or("");
        if sanitized == previous {
            return;
        }

        let delta = if previous.is_empty() {
            sanitized.as_str()
        } else if let Some(delta) = sanitized.strip_prefix(previous) {
            delta
        } else {
            sanitized.as_str()
        };
        if !delta.is_empty() {
            events.push(SseEvent {
                event: "content_block_delta".to_string(),
                data: json!({
                    "type": "content_block_delta",
                    "index": block_idx,
                    "delta": {
                        "type": "input_json_delta",
                        "partial_json": delta
                    }
                }),
            });
        }
        self.tool_argument_emitted.insert(tool_index, sanitized);
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
        let tool_blocks: Vec<(u32, u32)> = self
            .tool_call_indices
            .iter()
            .map(|(&tool_index, &block_index)| (tool_index, block_index))
            .collect();
        for (tool_index, idx) in &tool_blocks {
            self.emit_tool_arguments(*tool_index, *idx, false, &mut events);
        }
        for (_, idx) in tool_blocks {
            events.push(SseEvent {
                event: "content_block_stop".to_string(),
                data: json!({"type": "content_block_stop", "index": idx}),
            });
        }
        self.tool_call_indices.clear();
        self.tool_call_names.clear();
        self.tool_argument_buffers.clear();
        self.tool_argument_emitted.clear();

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
pub(crate) fn convert_non_streaming_response(data: &Value) -> Vec<SseEvent> {
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
                let arguments = sanitize_tool_arguments(&tool_name, arguments)
                    .unwrap_or_else(|| arguments.to_string());
                let input: Value = serde_json::from_str(&arguments).unwrap_or(json!({}));

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
    fn test_stream_converter_sanitizes_split_read_arguments() {
        let path = temp_read_fixture(1_113);
        let mut converter = StreamConverter::new();
        let first = OpenAiChunk {
            id: "test".to_string(),
            model: "gpt-4.1".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                delta: OpenAiDelta {
                    role: Some("assistant".to_string()),
                    content: None,
                    reasoning_content: None,
                    tool_calls: Some(vec![OpenAiToolCall {
                        index: 0,
                        id: Some("call_1".to_string()),
                        function: OpenAiFunction {
                            name: Some("Read".to_string()),
                            arguments: format!(
                                "{{\"file_path\":\"{}\",\"offset\":",
                                path.to_string_lossy()
                            ),
                        },
                    }]),
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let events = converter.process_chunk(&first);
        assert!(events.iter().all(|event| {
            event.event != "content_block_delta"
                || event.data["delta"]["type"] != "input_json_delta"
        }));

        let second = OpenAiChunk {
            id: "test".to_string(),
            model: "gpt-4.1".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                delta: OpenAiDelta {
                    role: None,
                    content: None,
                    reasoning_content: None,
                    tool_calls: Some(vec![OpenAiToolCall {
                        index: 0,
                        id: None,
                        function: OpenAiFunction {
                            name: None,
                            arguments: "5206854,\"limit\":5}".to_string(),
                        },
                    }]),
                },
                finish_reason: None,
            }],
            usage: None,
        };
        let events = converter.process_chunk(&second);
        let arguments = events
            .iter()
            .find_map(|event| {
                (event.event == "content_block_delta"
                    && event.data["delta"]["type"] == "input_json_delta")
                    .then(|| event.data["delta"]["partial_json"].as_str())
                    .flatten()
            })
            .expect("tool arguments");
        let input: Value = serde_json::from_str(arguments).expect("valid arguments");
        assert_eq!(input["offset"], 520);
        assert_eq!(input["limit"], 5);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_non_streaming_response_sanitizes_read_arguments() {
        let path = temp_read_fixture(1_113);
        let events = convert_non_streaming_response(&json!({
            "model": "gpt-4.1",
            "usage": {"prompt_tokens": 10, "completion_tokens": 2},
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "tool_calls": [{
                        "id": "call_1",
                        "function": {
                            "name": "Read",
                            "arguments": json!({
                                "file_path": path.to_string_lossy(),
                                "offset": 5_206_854_u64,
                                "limit": 5
                            }).to_string()
                        }
                    }]
                }
            }]
        }));
        let input = events
            .iter()
            .find_map(|event| {
                (event.event == "content_block_start"
                    && event.data["content_block"]["type"] == "tool_use")
                    .then_some(&event.data["content_block"]["input"])
            })
            .expect("tool input");

        assert_eq!(input["offset"], 520);
        assert_eq!(input["limit"], 5);
        let _ = std::fs::remove_file(path);
    }

    fn temp_read_fixture(lines: usize) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "claude-proxy-openai-read-fixture-{}-{}.txt",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let body = (1..=lines)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        std::fs::write(&path, body).expect("write read fixture");
        path
    }
}
