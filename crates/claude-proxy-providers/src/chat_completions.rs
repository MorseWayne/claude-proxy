//! OpenAI-compatible Chat Completions response conversion.
//!
//! Converts OpenAI-format chat completion responses into Anthropic SSE events.

use std::borrow::Cow;
use std::collections::HashMap;

use claude_proxy_config::settings::ReasoningMarkerMode;
use claude_proxy_core::SseEvent;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::http::{fmt_reqwest_err, next_upstream_stream_item};
use crate::provider::{
    ProviderError, ProviderRequestObserver, ProviderRequestObserverEvent,
    ProviderRequestObserverEventKind, ProviderStreamMetadata, ProviderUsageMetadata,
};
use crate::reasoning_markers::{ReasoningTextSplitter, TextSegment, split_text};
use crate::sse::{SseDecoder, is_sse_done, parse_sse_json_value};
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
#[cfg(test)]
pub(crate) fn stream_openai_response(
    response: reqwest::Response,
) -> BoxStream<'static, Result<SseEvent, ProviderError>> {
    stream_openai_response_with_marker_mode(response, ReasoningMarkerMode::Strict)
}

pub(crate) fn stream_openai_response_with_marker_mode(
    response: reqwest::Response,
    marker_mode: ReasoningMarkerMode,
) -> BoxStream<'static, Result<SseEvent, ProviderError>> {
    stream_openai_response_with_marker_mode_and_observer(response, marker_mode, None)
}

pub(crate) fn stream_openai_response_with_marker_mode_and_observer(
    response: reqwest::Response,
    marker_mode: ReasoningMarkerMode,
    observer: Option<ProviderRequestObserver>,
) -> BoxStream<'static, Result<SseEvent, ProviderError>> {
    let (tx, rx) = mpsc::channel::<Result<SseEvent, ProviderError>>(64);

    tokio::spawn(async move {
        let mut converter = StreamConverter::with_marker_mode(marker_mode);
        let observer = observer.as_ref();
        let mut decoder = SseDecoder::new();
        let mut byte_stream = response.bytes_stream();
        let mut saw_done = false;

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
                    decoder.push(&chunk);
                    while let Some(event_str) = decoder.next_frame() {
                        if is_sse_done(&event_str) {
                            saw_done = true;
                            continue;
                        }
                        if let Some(openai_chunk) = parse_openai_chunk(&event_str) {
                            let events = converter.process_chunk(&openai_chunk);
                            notify_stream_metadata(observer, &openai_chunk);
                            for event in events {
                                if tx.send(Ok(event)).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    if converter.stopped || saw_done {
                        break;
                    }
                    if let Some(event_str) = decoder.finish() {
                        if is_sse_done(&event_str) {
                            saw_done = true;
                        } else if let Some(openai_chunk) = parse_openai_chunk(&event_str) {
                            let events = converter.process_chunk(&openai_chunk);
                            notify_stream_metadata(observer, &openai_chunk);
                            for event in events {
                                if tx.send(Ok(event)).await.is_err() {
                                    return;
                                }
                            }
                        }
                    }
                    if converter.stopped || saw_done {
                        break;
                    }
                    let _ = tx
                        .send(Err(ProviderError::Network(fmt_reqwest_err(&e))))
                        .await;
                    return;
                }
            }
        }

        if let Some(event_str) = decoder.finish()
            && !is_sse_done(&event_str)
            && let Some(openai_chunk) = parse_openai_chunk(&event_str)
        {
            notify_stream_metadata(observer, &openai_chunk);
            for event in converter.process_chunk(&openai_chunk) {
                if tx.send(Ok(event)).await.is_err() {
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
    reasoning_text: ReasoningTextSplitter,
    started: bool,
    stopped: bool,
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

fn block_stop(index: u32) -> SseEvent {
    SseEvent {
        event: "content_block_stop".to_string(),
        data: json!({"type": "content_block_stop", "index": index}),
    }
}

fn push_non_streaming_text_block(events: &mut Vec<SseEvent>, index: u32, text: &str) {
    events.push(SseEvent {
        event: "content_block_start".to_string(),
        data: json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "text", "text": ""}
        }),
    });
    events.push(SseEvent {
        event: "content_block_delta".to_string(),
        data: json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "text_delta", "text": text}
        }),
    });
    events.push(block_stop(index));
}

fn push_non_streaming_thinking_block(events: &mut Vec<SseEvent>, index: u32, thinking: &str) {
    events.push(SseEvent {
        event: "content_block_start".to_string(),
        data: json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "thinking", "thinking": ""}
        }),
    });
    events.push(SseEvent {
        event: "content_block_delta".to_string(),
        data: json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "thinking_delta", "thinking": thinking}
        }),
    });
    events.push(block_stop(index));
}

impl Default for StreamConverter {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamConverter {
    pub fn new() -> Self {
        Self::with_marker_mode(ReasoningMarkerMode::Strict)
    }

    pub fn with_marker_mode(marker_mode: ReasoningMarkerMode) -> Self {
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
            reasoning_text: ReasoningTextSplitter::new(marker_mode),
            started: false,
            stopped: false,
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
                self.flush_reasoning_text(&mut events);
                self.emit_thinking_content(reasoning, &mut events);
            }

            // Handle text content
            if let Some(ref content) = choice.delta.content
                && !content.is_empty()
            {
                self.emit_text_stream_content(content, &mut events);
            }

            // Handle tool calls
            if let Some(ref tool_calls) = choice.delta.tool_calls {
                self.flush_reasoning_text(&mut events);
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
                self.flush_reasoning_text(&mut events);
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
                self.stopped = true;

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

    fn emit_text_stream_content(&mut self, content: &str, events: &mut Vec<SseEvent>) {
        for segment in self.reasoning_text.push(content) {
            self.emit_text_segment(segment, events);
        }
    }

    fn flush_reasoning_text(&mut self, events: &mut Vec<SseEvent>) {
        for segment in self.reasoning_text.finish() {
            self.emit_text_segment(segment, events);
        }
    }

    fn emit_text_segment(&mut self, segment: TextSegment, events: &mut Vec<SseEvent>) {
        match segment {
            TextSegment::Text(text) => self.emit_text_content(&text, events),
            TextSegment::Reasoning(thinking) => {
                self.emit_thinking_content(&thinking, events);
            }
        }
    }

    fn emit_text_content(&mut self, content: &str, events: &mut Vec<SseEvent>) {
        if self.current_thinking_index.is_some() {
            let idx = self.current_thinking_index.take().unwrap();
            events.push(block_stop(idx));
        }

        if content.is_empty() {
            return;
        }
        let idx = self.ensure_text_block(events);
        events.push(SseEvent {
            event: "content_block_delta".to_string(),
            data: json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": {"type": "text_delta", "text": content}
            }),
        });
    }

    fn emit_thinking_content(&mut self, thinking: &str, events: &mut Vec<SseEvent>) {
        if thinking.is_empty() {
            return;
        }
        let idx = self.ensure_thinking_block(events);
        events.push(SseEvent {
            event: "content_block_delta".to_string(),
            data: json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": {"type": "thinking_delta", "thinking": thinking}
            }),
        });
    }

    fn ensure_text_block(&mut self, events: &mut Vec<SseEvent>) -> u32 {
        if let Some(idx) = self.current_text_index {
            return idx;
        }
        if let Some(idx) = self.current_thinking_index.take() {
            events.push(block_stop(idx));
        }
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
        idx
    }

    fn ensure_thinking_block(&mut self, events: &mut Vec<SseEvent>) -> u32 {
        if let Some(idx) = self.current_thinking_index {
            return idx;
        }
        if let Some(idx) = self.current_text_index.take() {
            events.push(block_stop(idx));
        }
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
        idx
    }

    fn emit_tool_arguments(
        &mut self,
        tool_index: u32,
        block_idx: u32,
        require_valid_json: bool,
        events: &mut Vec<SseEvent>,
    ) {
        let Some(arguments) = self.tool_argument_buffers.get(&tool_index) else {
            return;
        };
        if arguments.is_empty() {
            return;
        }
        if require_valid_json && serde_json::from_str::<Value>(arguments).is_err() {
            return;
        }

        let tool_name = self
            .tool_call_names
            .get(&tool_index)
            .map(String::as_str)
            .unwrap_or_default();
        let sanitized = sanitize_tool_arguments(tool_name, arguments)
            .map(Cow::Owned)
            .unwrap_or(Cow::Borrowed(arguments.as_str()));
        let previous = self
            .tool_argument_emitted
            .get(&tool_index)
            .map(String::as_str)
            .unwrap_or("");
        if sanitized.as_ref() == previous {
            return;
        }

        let delta = if previous.is_empty() {
            sanitized.as_ref()
        } else if let Some(delta) = sanitized.strip_prefix(previous) {
            delta
        } else {
            sanitized.as_ref()
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
        self.tool_argument_emitted
            .insert(tool_index, sanitized.into_owned());
    }

    pub fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if self.stopped {
            return events;
        }

        self.flush_reasoning_text(&mut events);

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
            self.stopped = true;
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
    let data = parse_sse_json_value(text)?;

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

fn notify_stream_metadata(observer: Option<&ProviderRequestObserver>, chunk: &OpenAiChunk) {
    let Some(observer) = observer else {
        return;
    };
    if chunk.usage.is_none() && chunk.model.is_empty() && chunk.id.is_empty() {
        return;
    }

    let usage = chunk.usage.as_ref().map(|usage| ProviderUsageMetadata {
        input_tokens: usage.prompt_tokens as u64,
        output_tokens: usage.completion_tokens as u64,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
    });
    let stop_reason = chunk
        .choices
        .iter()
        .find_map(|choice| choice.finish_reason.clone());

    observer(ProviderRequestObserverEvent {
        event: ProviderRequestObserverEventKind::StreamMetadata,
        stream_metadata: Some(ProviderStreamMetadata {
            usage,
            model: (!chunk.model.is_empty()).then(|| chunk.model.clone()),
            request_id: (!chunk.id.is_empty()).then(|| chunk.id.clone()),
            stop_reason,
        }),
        ..ProviderRequestObserverEvent::default()
    });
}

/// Convert a non-streaming OpenAI response to Anthropic format.
#[cfg(test)]
pub(crate) fn convert_non_streaming_response(data: &Value) -> Vec<SseEvent> {
    convert_non_streaming_response_with_marker_mode(data, ReasoningMarkerMode::Strict)
}

pub(crate) fn convert_non_streaming_response_with_marker_mode(
    data: &Value,
    marker_mode: ReasoningMarkerMode,
) -> Vec<SseEvent> {
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
            for segment in split_text(content, marker_mode) {
                match segment {
                    TextSegment::Text(text) => {
                        if !text.is_empty() {
                            push_non_streaming_text_block(&mut events, block_index, &text);
                            block_index += 1;
                        }
                    }
                    TextSegment::Reasoning(thinking) => {
                        push_non_streaming_thinking_block(&mut events, block_index, &thinking);
                        block_index += 1;
                    }
                }
            }
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
    use futures::StreamExt;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    fn text_deltas(events: &[SseEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| {
                (event.event == "content_block_delta"
                    && event.data["delta"]["type"] == "text_delta")
                    .then(|| event.data["delta"]["text"].as_str())
                    .flatten()
                    .map(ToOwned::to_owned)
            })
            .collect()
    }

    #[test]
    fn test_parse_openai_chunk_text() {
        let text = r#"data: {"id":"chatcmpl-123","object":"chat.completion.chunk","model":"gpt-4","choices":[{"index":0,"delta":{"role":"assistant","content":"Hello"},"finish_reason":null}]}"#;
        let chunk = parse_openai_chunk(text).unwrap();
        assert_eq!(chunk.id, "chatcmpl-123");
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("Hello"));
    }

    #[test]
    fn test_parse_openai_chunk_accepts_data_without_space() {
        let text = r#"data:{"id":"chatcmpl-123","object":"chat.completion.chunk","model":"gpt-4","choices":[{"index":0,"delta":{"content":"Hello"},"finish_reason":null}]}"#;
        let chunk = parse_openai_chunk(text).unwrap();
        assert_eq!(chunk.id, "chatcmpl-123");
        assert_eq!(chunk.choices[0].delta.content.as_deref(), Some("Hello"));
    }

    #[test]
    fn test_parse_openai_chunk_done() {
        let text = "data: [DONE]";
        assert!(parse_openai_chunk(text).is_none());
    }

    #[tokio::test]
    async fn test_stream_openai_ignores_trailing_chunk_eof_after_finish_reason() {
        let body = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":1}}\n\n",
        );
        let response = response_from_unterminated_chunked_body("text/event-stream", body).await;
        let mut stream = stream_openai_response(response);
        let mut events = Vec::new();

        while let Some(item) = stream.next().await {
            events.push(item.expect("completed stream should ignore trailing chunk EOF"));
        }

        assert!(events.iter().any(|event| event.event == "message_stop"));
    }

    #[tokio::test]
    async fn test_stream_openai_ignores_chunk_eof_after_undelimited_finish_reason() {
        let body = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":1}}",
        );
        let response = response_from_unterminated_chunked_body("text/event-stream", body).await;
        let mut stream = stream_openai_response(response);
        let mut events = Vec::new();

        while let Some(item) = stream.next().await {
            events.push(item.expect("terminal frame should be flushed before chunk EOF"));
        }

        assert!(events.iter().any(|event| event.event == "message_stop"));
    }

    #[tokio::test]
    async fn test_stream_openai_ignores_chunk_eof_after_done_marker() {
        let body = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: [DONE]\n\n",
        );
        let response = response_from_unterminated_chunked_body("text/event-stream", body).await;
        let mut stream = stream_openai_response(response);
        let mut events = Vec::new();

        while let Some(item) = stream.next().await {
            events.push(item.expect("done marker should make chunk EOF terminal"));
        }

        assert!(events.iter().any(|event| event.event == "message_stop"));
    }

    #[tokio::test]
    async fn test_stream_openai_errors_on_midstream_chunk_eof() {
        let body = "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"partial\"},\"finish_reason\":null}]}\n\n";
        let response = response_from_unterminated_chunked_body("text/event-stream", body).await;
        let mut stream = stream_openai_response(response);
        let mut saw_network_error = false;

        while let Some(item) = stream.next().await {
            if matches!(item, Err(ProviderError::Network(_))) {
                saw_network_error = true;
                break;
            }
        }

        assert!(
            saw_network_error,
            "mid-stream chunk EOF must produce an error"
        );
    }

    #[tokio::test]
    async fn test_stream_openai_observer_sees_late_usage_chunk() {
        let body = concat!(
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello\"},\"finish_reason\":null}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: {\"id\":\"chatcmpl-1\",\"object\":\"chat.completion.chunk\",\"model\":\"gpt-4\",\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2}}\n\n",
            "data: [DONE]\n\n",
        );
        let response = response_from_unterminated_chunked_body("text/event-stream", body).await;
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_for_observer = Arc::clone(&observed);
        let observer: ProviderRequestObserver = Arc::new(move |event| {
            observed_for_observer.lock().unwrap().push(event);
        });
        let mut stream = stream_openai_response_with_marker_mode_and_observer(
            response,
            ReasoningMarkerMode::Strict,
            Some(observer),
        );
        let mut events = Vec::new();

        while let Some(item) = stream.next().await {
            events.push(item.expect("stream should complete"));
        }

        assert!(events.iter().any(|event| event.event == "message_stop"));
        let observed = observed.lock().unwrap();
        let usage = observed
            .iter()
            .filter_map(|event| event.stream_metadata.as_ref())
            .filter_map(|metadata| metadata.usage.as_ref())
            .last()
            .expect("late usage metadata should be observed");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 2);
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
    fn test_stream_converter_maps_tagged_thinking_content() {
        let mut converter = StreamConverter::with_marker_mode(ReasoningMarkerMode::LegacyTags);
        converter.process_chunk(&OpenAiChunk {
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
        });

        let events = converter.process_chunk(&OpenAiChunk {
            id: "test".to_string(),
            model: "gpt-4".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                delta: OpenAiDelta {
                    role: None,
                    content: Some("hello [thinking]plan[/thinking] world".to_string()),
                    reasoning_content: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        });

        assert_eq!(
            text_and_thinking_deltas(&events),
            vec![
                ("text_delta".to_string(), "hello ".to_string()),
                ("thinking_delta".to_string(), "plan".to_string()),
                ("text_delta".to_string(), " world".to_string()),
            ]
        );
        assert!(!text_deltas(&events).join("").contains("plan"));
    }

    #[test]
    fn test_stream_converter_preserves_tagged_text_by_default() {
        let mut converter = StreamConverter::new();
        converter.process_chunk(&OpenAiChunk {
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
        });

        let events = converter.process_chunk(&OpenAiChunk {
            id: "test".to_string(),
            model: "gpt-4".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                delta: OpenAiDelta {
                    role: None,
                    content: Some("use `<thinking>...</thinking>` here".to_string()),
                    reasoning_content: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        });

        assert_eq!(
            text_deltas(&events),
            vec!["use `<thinking>...</thinking>` here"]
        );
        assert!(
            text_and_thinking_deltas(&events)
                .iter()
                .all(|(kind, _)| kind == "text_delta")
        );
    }

    #[test]
    fn test_stream_converter_drops_unclosed_thinking_marker_in_text() {
        let mut converter = StreamConverter::with_marker_mode(ReasoningMarkerMode::LegacyTags);
        converter.process_chunk(&OpenAiChunk {
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
        });

        let mut events = converter.process_chunk(&OpenAiChunk {
            id: "test".to_string(),
            model: "gpt-4".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                delta: OpenAiDelta {
                    role: None,
                    content: Some("visible [thinking]secret".to_string()),
                    reasoning_content: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        });
        events.extend(converter.finish());

        assert_eq!(text_deltas(&events), vec!["visible "]);
        assert!(!text_deltas(&events).join("").contains("secret"));
    }

    #[test]
    fn test_stream_converter_sanitizes_split_thinking_markers_in_text() {
        let mut converter = StreamConverter::with_marker_mode(ReasoningMarkerMode::SanitizeOnly);
        converter.process_chunk(&OpenAiChunk {
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
        });

        let mut events = Vec::new();
        for content in ["hello [think", "ing]secret", "[/think", "ing] world"] {
            events.extend(converter.process_chunk(&OpenAiChunk {
                id: "test".to_string(),
                model: "gpt-4".to_string(),
                choices: vec![OpenAiChoice {
                    index: 0,
                    delta: OpenAiDelta {
                        role: None,
                        content: Some(content.to_string()),
                        reasoning_content: None,
                        tool_calls: None,
                    },
                    finish_reason: None,
                }],
                usage: None,
            }));
        }

        assert_eq!(text_deltas(&events), vec!["hello ", " world"]);
        assert!(!text_deltas(&events).join("").contains("secret"));
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
    fn test_stream_converter_maps_split_tagged_thinking_content() {
        let mut converter = StreamConverter::with_marker_mode(ReasoningMarkerMode::LegacyTags);
        converter.process_chunk(&OpenAiChunk {
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
        });

        let mut events = converter.process_chunk(&OpenAiChunk {
            id: "test".to_string(),
            model: "gpt-4".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                delta: OpenAiDelta {
                    role: None,
                    content: Some("hello [thin".to_string()),
                    reasoning_content: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        });
        events.extend(converter.process_chunk(&OpenAiChunk {
            id: "test".to_string(),
            model: "gpt-4".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                delta: OpenAiDelta {
                    role: None,
                    content: Some("king]plan[/thin".to_string()),
                    reasoning_content: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        }));
        events.extend(converter.process_chunk(&OpenAiChunk {
            id: "test".to_string(),
            model: "gpt-4".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                delta: OpenAiDelta {
                    role: None,
                    content: Some("king] world".to_string()),
                    reasoning_content: None,
                    tool_calls: None,
                },
                finish_reason: None,
            }],
            usage: None,
        }));

        let deltas = text_and_thinking_deltas(&events);
        assert_eq!(
            deltas,
            vec![
                ("text_delta".to_string(), "hello ".to_string()),
                ("thinking_delta".to_string(), "plan".to_string()),
                ("text_delta".to_string(), " world".to_string()),
            ]
        );
    }

    #[test]
    fn test_stream_converter_sanitizes_split_read_arguments() {
        let path = temp_read_fixture(1_113);
        let file_path_json = serde_json::to_string(path.to_string_lossy().as_ref()).unwrap();
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
                            arguments: format!("{{\"file_path\":{file_path_json},\"offset\":"),
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
    fn test_stream_converter_does_not_finish_twice_after_finish_reason() {
        let mut converter = StreamConverter::new();
        let events = converter.process_chunk(&OpenAiChunk {
            id: "test".to_string(),
            model: "gpt-4.1".to_string(),
            choices: vec![OpenAiChoice {
                index: 0,
                delta: OpenAiDelta {
                    role: Some("assistant".to_string()),
                    content: Some("Done".to_string()),
                    reasoning_content: None,
                    tool_calls: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(OpenAiUsage {
                prompt_tokens: 11,
                completion_tokens: 3,
            }),
        });

        assert_eq!(
            events
                .iter()
                .filter(|event| event.event == "message_stop")
                .count(),
            1
        );
        assert!(converter.finish().is_empty());
    }

    #[test]
    fn test_non_streaming_response_maps_tagged_thinking_content() {
        let events = convert_non_streaming_response_with_marker_mode(
            &json!({
                "model": "gpt-4.1",
                "usage": {"prompt_tokens": 10, "completion_tokens": 2},
                "choices": [{
                    "finish_reason": "stop",
                    "message": {
                        "content": "hello [thinking]plan[/thinking] world"
                    }
                }]
            }),
            ReasoningMarkerMode::LegacyTags,
        );

        assert_eq!(
            text_and_thinking_deltas(&events),
            vec![
                ("text_delta".to_string(), "hello ".to_string()),
                ("thinking_delta".to_string(), "plan".to_string()),
                ("text_delta".to_string(), " world".to_string()),
            ]
        );
        assert!(!text_deltas(&events).join("").contains("plan"));
    }

    #[test]
    fn test_non_streaming_response_drops_unclosed_thinking_marker_in_text() {
        let events = convert_non_streaming_response_with_marker_mode(
            &json!({
                "model": "gpt-4.1",
                "usage": {"prompt_tokens": 10, "completion_tokens": 2},
                "choices": [{
                    "finish_reason": "stop",
                    "message": {
                        "content": "visible [thinking]secret"
                    }
                }]
            }),
            ReasoningMarkerMode::LegacyTags,
        );

        assert_eq!(text_deltas(&events), vec!["visible "]);
        assert!(!text_deltas(&events).join("").contains("secret"));
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

    async fn response_from_unterminated_chunked_body(
        content_type: &str,
        body: &str,
    ) -> reqwest::Response {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let content_type = content_type.to_string();
        let body = body.to_string();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ntransfer-encoding: chunked\r\nconnection: close\r\n\r\n{:x}\r\n{body}\r\n",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        reqwest::Client::new()
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap()
    }

    fn text_and_thinking_deltas(events: &[SseEvent]) -> Vec<(String, String)> {
        events
            .iter()
            .filter_map(|event| {
                if event.event != "content_block_delta" {
                    return None;
                }
                let delta = &event.data["delta"];
                match delta["type"].as_str()? {
                    "text_delta" => Some((
                        "text_delta".to_string(),
                        delta["text"].as_str()?.to_string(),
                    )),
                    "thinking_delta" => Some((
                        "thinking_delta".to_string(),
                        delta["thinking"].as_str()?.to_string(),
                    )),
                    _ => None,
                }
            })
            .collect()
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
