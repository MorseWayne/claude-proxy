use std::collections::HashMap;

use claude_proxy_core::*;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::http::fmt_reqwest_err;
use crate::provider::ProviderError;

const RECENT_TOOL_OUTPUTS_TO_KEEP: usize = 12;
const MAX_HISTORICAL_TOOL_OUTPUT_BYTES: usize = 4096;
const RECENT_TEXT_ITEMS_TO_KEEP: usize = 12;
const MAX_HISTORICAL_TEXT_BYTES: usize = 32 * 1024;
const SMALL_HISTORY_PAYLOAD_BUDGET_BYTES: usize = 256 * 1024;
const DEFAULT_HISTORY_PAYLOAD_BUDGET_BYTES: usize = 512 * 1024;
const LARGE_HISTORY_PAYLOAD_BUDGET_BYTES: usize = 1024 * 1024;

#[derive(Debug, Default)]
struct HistoryCompressionStats {
    text_items: usize,
    text_bytes: usize,
    tool_outputs: usize,
    tool_output_bytes: usize,
}

#[derive(Debug)]
struct HistoryCompressionState {
    text_items_to_consider: usize,
    tool_outputs_to_consider: usize,
    excess_bytes: usize,
}

pub fn convert_to_responses(req: &MessagesRequest) -> Value {
    let mut input = Vec::new();
    let current_message_index = req.messages.len().saturating_sub(1);
    let historical_stats = req
        .messages
        .iter()
        .take(current_message_index)
        .map(message_compression_stats)
        .fold(HistoryCompressionStats::default(), |mut total, stats| {
            total.text_items += stats.text_items;
            total.text_bytes += stats.text_bytes;
            total.tool_outputs += stats.tool_outputs;
            total.tool_output_bytes += stats.tool_output_bytes;
            total
        });
    let historical_payload_bytes = historical_stats.text_bytes + historical_stats.tool_output_bytes;
    let mut compression = HistoryCompressionState {
        text_items_to_consider: historical_stats
            .text_items
            .saturating_sub(RECENT_TEXT_ITEMS_TO_KEEP),
        tool_outputs_to_consider: historical_stats
            .tool_outputs
            .saturating_sub(RECENT_TOOL_OUTPUTS_TO_KEEP),
        excess_bytes: historical_payload_bytes
            .saturating_sub(history_payload_budget_bytes(&req.model)),
    };

    for (index, msg) in req.messages.iter().enumerate() {
        append_message_items(
            &mut input,
            msg,
            index == current_message_index,
            &mut compression,
        );
    }

    let mut body = json!({
        "model": req.model,
        "input": input,
        "stream": req.stream,
        "store": false,
    });

    if req.tools.as_ref().is_some_and(|tools| !tools.is_empty()) {
        body["parallel_tool_calls"] = json!(true);
    }

    if should_include_encrypted_reasoning(req) {
        body["include"] = json!(["reasoning.encrypted_content"]);
    }

    if let Some(instructions) = system_to_text(&req.system) {
        body["instructions"] = json!(instructions);
    }
    if let Some(max_tokens) = req.max_tokens {
        body["max_output_tokens"] = json!(max_tokens);
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
    if let Some(tools) = &req.tools {
        body["tools"] = json!(tools.iter().map(convert_tool).collect::<Vec<_>>());
    }
    if let Some(tool_choice) = &req.tool_choice {
        body["tool_choice"] = normalize_tool_choice(tool_choice);
    }
    if let Some(reasoning) = convert_reasoning(req) {
        body["reasoning"] = reasoning;
    }
    body
}

fn system_to_text(system: &Option<SystemPrompt>) -> Option<String> {
    match system {
        Some(SystemPrompt::Text(text)) if !text.is_empty() => Some(text.clone()),
        Some(SystemPrompt::Blocks(blocks)) => {
            let text = blocks
                .iter()
                .filter_map(|block| match block {
                    Content::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn should_include_encrypted_reasoning(req: &MessagesRequest) -> bool {
    if req.extra.contains_key("reasoning") {
        return true;
    }
    if req
        .extra
        .get("reasoning_effort")
        .and_then(Value::as_str)
        .is_some_and(|effort| effort != "none")
    {
        return true;
    }
    if req
        .thinking
        .as_ref()
        .is_some_and(|thinking| thinking.r#type.as_deref() != Some("disabled"))
    {
        return true;
    }
    req.messages.iter().any(|message| match &message.content {
        MessageContent::Text(_) => false,
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .any(|block| matches!(block, Content::Thinking { .. })),
    })
}

fn history_payload_budget_bytes(model: &str) -> usize {
    let model = model.to_ascii_lowercase();
    if model.contains("mini") || model.contains("small") || model.contains("flash") {
        SMALL_HISTORY_PAYLOAD_BUDGET_BYTES
    } else if model.contains("gpt-5") || model.contains("o3") || model.contains("o4") {
        LARGE_HISTORY_PAYLOAD_BUDGET_BYTES
    } else {
        DEFAULT_HISTORY_PAYLOAD_BUDGET_BYTES
    }
}

fn message_compression_stats(message: &Message) -> HistoryCompressionStats {
    match &message.content {
        MessageContent::Text(text) => HistoryCompressionStats {
            text_items: 1,
            text_bytes: text.len(),
            ..Default::default()
        },
        MessageContent::Blocks(blocks) => block_compression_stats(blocks),
    }
}

fn block_compression_stats(blocks: &[Content]) -> HistoryCompressionStats {
    let mut stats = HistoryCompressionStats::default();
    let mut text_bytes = 0;
    let mut has_pending_text = false;
    let mut has_pending_thinking = false;

    for block in blocks {
        match block {
            Content::Text { text } => {
                has_pending_text = true;
                text_bytes += text.len();
            }
            Content::Thinking { .. } => {
                has_pending_text = true;
                has_pending_thinking = true;
            }
            Content::ToolUse { .. } | Content::ServerToolUse { .. } => {
                add_pending_text_stats(
                    &mut stats,
                    &mut text_bytes,
                    &mut has_pending_text,
                    &mut has_pending_thinking,
                );
            }
            Content::ToolResult { content, .. } => {
                add_pending_text_stats(
                    &mut stats,
                    &mut text_bytes,
                    &mut has_pending_text,
                    &mut has_pending_thinking,
                );
                stats.tool_outputs += 1;
                stats.tool_output_bytes += raw_tool_result_text_len(content);
            }
            Content::Unknown => {}
        }
    }

    add_pending_text_stats(
        &mut stats,
        &mut text_bytes,
        &mut has_pending_text,
        &mut has_pending_thinking,
    );
    stats
}

fn add_pending_text_stats(
    stats: &mut HistoryCompressionStats,
    text_bytes: &mut usize,
    has_pending_text: &mut bool,
    has_pending_thinking: &mut bool,
) {
    if *has_pending_text && !*has_pending_thinking {
        stats.text_items += 1;
        stats.text_bytes += *text_bytes;
    }
    *text_bytes = 0;
    *has_pending_text = false;
    *has_pending_thinking = false;
}

fn raw_tool_result_text_len(content: &Option<Value>) -> usize {
    match content {
        Some(Value::String(text)) => text.len(),
        Some(value) => value.to_string().len(),
        None => 0,
    }
}

fn append_message_items(
    input: &mut Vec<Value>,
    msg: &Message,
    is_current_message: bool,
    compression: &mut HistoryCompressionState,
) {
    match &msg.content {
        MessageContent::Text(text) => {
            input.push(message_item(
                &msg.role,
                compressed_text_item(text, is_current_message, compression),
            ));
        }
        MessageContent::Blocks(blocks) => {
            let mut text_parts = Vec::new();
            for block in blocks {
                match block {
                    Content::Text { text } => text_parts.push(text.clone()),
                    Content::Thinking { thinking, .. } => {
                        text_parts.push(format!("[thinking]\n{thinking}\n[/thinking]"));
                    }
                    Content::ToolUse {
                        id,
                        name,
                        input: args,
                    }
                    | Content::ServerToolUse {
                        id,
                        name,
                        input: args,
                    } => {
                        if !text_parts.is_empty() {
                            input.push(message_item(
                                &msg.role,
                                compressed_text_item(
                                    &text_parts.join("\n"),
                                    is_current_message,
                                    compression,
                                ),
                            ));
                            text_parts.clear();
                        }
                        input.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": serde_json::to_string(args).unwrap_or_default(),
                        }));
                    }
                    Content::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        if !text_parts.is_empty() {
                            input.push(message_item(
                                &msg.role,
                                compressed_text_item(
                                    &text_parts.join("\n"),
                                    is_current_message,
                                    compression,
                                ),
                            ));
                            text_parts.clear();
                        }
                        let output = tool_result_text(
                            content,
                            *is_error,
                            should_truncate_tool_output(content, is_current_message, compression),
                        );
                        input.push(json!({
                            "type": "function_call_output",
                            "call_id": tool_use_id,
                            "output": output,
                        }));
                    }
                    Content::Unknown => {}
                }
            }
            if !text_parts.is_empty() {
                input.push(message_item(
                    &msg.role,
                    compressed_text_item(&text_parts.join("\n"), is_current_message, compression),
                ));
            }
        }
    }
}

fn compressed_text_item(
    text: &str,
    is_current_message: bool,
    compression: &mut HistoryCompressionState,
) -> String {
    text_item_text(
        text,
        should_truncate_text_item(text, is_current_message, compression),
    )
}

fn should_truncate_tool_output(
    content: &Option<Value>,
    is_current_message: bool,
    compression: &mut HistoryCompressionState,
) -> bool {
    if is_current_message || compression.tool_outputs_to_consider == 0 {
        return false;
    }
    compression.tool_outputs_to_consider -= 1;
    let original_bytes = raw_tool_result_text_len(content);
    if compression.excess_bytes == 0 || original_bytes <= MAX_HISTORICAL_TOOL_OUTPUT_BYTES {
        return false;
    }
    compression.excess_bytes = compression
        .excess_bytes
        .saturating_sub(truncated_tool_output_bytes_saved(original_bytes));
    true
}

fn should_truncate_text_item(
    text: &str,
    is_current_message: bool,
    compression: &mut HistoryCompressionState,
) -> bool {
    if is_current_message || compression.text_items_to_consider == 0 || text.contains("[thinking]")
    {
        return false;
    }
    compression.text_items_to_consider -= 1;
    if compression.excess_bytes == 0 || text.len() <= MAX_HISTORICAL_TEXT_BYTES {
        return false;
    }
    compression.excess_bytes = compression
        .excess_bytes
        .saturating_sub(truncated_text_bytes_saved(text.len()));
    true
}

fn truncated_tool_output_bytes_saved(original_bytes: usize) -> usize {
    original_bytes.saturating_sub(
        format!(
            "[tool output truncated: original_bytes={original_bytes}, max_historical_tool_output_bytes={MAX_HISTORICAL_TOOL_OUTPUT_BYTES}]"
        )
        .len(),
    )
}

fn truncated_text_bytes_saved(original_bytes: usize) -> usize {
    original_bytes.saturating_sub(
        format!(
            "[text content truncated: original_bytes={original_bytes}, max_historical_text_bytes={MAX_HISTORICAL_TEXT_BYTES}]"
        )
        .len(),
    )
}

fn text_item_text(text: &str, truncate_if_large: bool) -> String {
    if truncate_if_large && text.len() > MAX_HISTORICAL_TEXT_BYTES {
        format!(
            "[text content truncated: original_bytes={}, max_historical_text_bytes={}]",
            text.len(),
            MAX_HISTORICAL_TEXT_BYTES
        )
    } else {
        text.to_string()
    }
}

fn message_item(role: &Role, text: String) -> Value {
    let role = match role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    json!({
        "role": role,
        "content": text,
    })
}

fn tool_result_text(
    content: &Option<Value>,
    is_error: Option<bool>,
    truncate_if_large: bool,
) -> String {
    let text = match content {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    };
    let text = if truncate_if_large && text.len() > MAX_HISTORICAL_TOOL_OUTPUT_BYTES {
        format!(
            "[tool output truncated: original_bytes={}, max_historical_tool_output_bytes={}]",
            text.len(),
            MAX_HISTORICAL_TOOL_OUTPUT_BYTES
        )
    } else {
        text
    };
    if is_error == Some(true) {
        format!("ERROR: {text}")
    } else {
        text
    }
}

fn convert_tool(tool: &Tool) -> Value {
    let mut value = json!({
        "type": "function",
        "name": tool.name,
        "parameters": normalize_tool_schema(&tool.input_schema),
    });

    if let Some(description) = &tool.description {
        value["description"] = json!(description);
    }

    value
}

fn normalize_tool_schema(schema: &Value) -> Value {
    let Some(object) = schema.as_object() else {
        return json!({"type": "object", "properties": {}});
    };

    if object
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|schema_type| schema_type != "object")
    {
        return json!({"type": "object", "properties": {}});
    }

    let mut normalized = object.clone();
    normalized.insert("type".to_string(), json!("object"));
    if !normalized
        .get("properties")
        .is_some_and(|properties| properties.is_object())
    {
        normalized.insert("properties".to_string(), json!({}));
    }

    if let Some(required) = normalized.get("required")
        && !required
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string))
    {
        normalized.remove("required");
    }

    Value::Object(normalized)
}

fn normalize_tool_choice(tool_choice: &Value) -> Value {
    if let Some(choice_type) = tool_choice.get("type").and_then(Value::as_str) {
        match choice_type {
            "auto" => return json!("auto"),
            "none" => return json!("none"),
            "any" => return json!("required"),
            "tool" => {
                if let Some(name) = tool_choice.get("name").and_then(Value::as_str) {
                    return json!({"type": "function", "name": name});
                }
            }
            _ => {}
        }
    }
    tool_choice.clone()
}

fn convert_reasoning(req: &MessagesRequest) -> Option<Value> {
    if let Some(reasoning) = req.extra.get("reasoning") {
        return Some(reasoning.clone());
    }
    if let Some(effort) = req.extra.get("reasoning_effort").and_then(Value::as_str) {
        if effort == "none" {
            return Some(json!({"effort": "none"}));
        }
        return Some(json!({"effort": effort, "summary": "detailed"}));
    }
    let thinking = req.thinking.as_ref()?;
    if thinking.r#type.as_deref() == Some("disabled") {
        return Some(json!({"effort": "none"}));
    }
    if matches!(thinking.r#type.as_deref(), Some("enabled" | "adaptive"))
        || thinking.budget_tokens.is_some()
    {
        return Some(json!({"effort": "medium", "summary": "detailed"}));
    }
    None
}

pub fn stream_responses_response(
    response: reqwest::Response,
) -> BoxStream<'static, Result<SseEvent, ProviderError>> {
    let (tx, rx) = mpsc::channel::<Result<SseEvent, ProviderError>>(64);

    tokio::spawn(async move {
        let mut converter = ResponsesStreamConverter::new();
        let mut buffer = String::new();
        let mut byte_stream = response.bytes_stream();

        while let Some(chunk_result) = byte_stream.next().await {
            match chunk_result {
                Ok(chunk) => {
                    buffer.push_str(&String::from_utf8_lossy(&chunk));
                    while let Some(pos) = buffer.find("\n\n") {
                        let event = buffer[..pos].to_string();
                        buffer = buffer[pos + 2..].to_string();

                        if let Some(value) = parse_sse_json(&event) {
                            for event in converter.process_event(&value) {
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

    Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx))
}

fn parse_sse_json(text: &str) -> Option<Value> {
    let mut data = String::new();
    for line in text.lines() {
        if let Some(rest) = line
            .strip_prefix("data: ")
            .or_else(|| line.strip_prefix("data:"))
        {
            let rest = rest.trim();
            if rest == "[DONE]" {
                return None;
            }
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest);
        }
    }
    serde_json::from_str(&data).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenBlock {
    Text(u32),
    Thinking(u32),
}

#[derive(Default)]
struct ResponsesStreamConverter {
    message_id: String,
    model: String,
    started: bool,
    next_block_index: u32,
    open_block: Option<OpenBlock>,
    output_blocks: HashMap<(u64, u64), u32>,
    function_blocks: HashMap<u64, u32>,
    function_names: HashMap<u64, String>,
    function_call_ids: HashMap<u64, String>,
    function_argument_buffers: HashMap<u64, String>,
    function_argument_emitted: HashMap<u64, String>,
    saw_function_call: bool,
    input_tokens: u32,
    output_tokens: u32,
    stopped: bool,
}

impl ResponsesStreamConverter {
    fn new() -> Self {
        Self {
            message_id: format!("msg_{}", uuid::Uuid::new_v4().to_string().replace('-', "")),
            ..Default::default()
        }
    }

    fn process_event(&mut self, event: &Value) -> Vec<SseEvent> {
        let mut events = Vec::new();
        let event_type = event["type"].as_str().unwrap_or_default();

        match event_type {
            "response.created" | "response.in_progress" => {
                if let Some(response) = event.get("response") {
                    self.ensure_started(response, &mut events);
                }
            }
            "response.output_item.added" => {
                self.ensure_started(event.get("response").unwrap_or(event), &mut events);
                self.handle_output_item_added(event, &mut events);
            }
            "response.content_part.added" => {
                self.ensure_started(event.get("response").unwrap_or(event), &mut events);
                self.handle_content_part_added(event, &mut events);
            }
            "response.output_text.delta" => {
                self.ensure_started(event.get("response").unwrap_or(event), &mut events);
                let output_index = event["output_index"].as_u64().unwrap_or(0);
                let content_index = event["content_index"].as_u64().unwrap_or(0);
                let delta = event["delta"].as_str().unwrap_or_default();
                if !delta.is_empty() {
                    let idx = self.ensure_text_block(output_index, content_index, &mut events);
                    events.push(content_delta(idx, "text_delta", "text", delta));
                }
            }
            "response.refusal.delta" => {
                self.ensure_started(event.get("response").unwrap_or(event), &mut events);
                let output_index = event["output_index"].as_u64().unwrap_or(0);
                let content_index = event["content_index"].as_u64().unwrap_or(0);
                let delta = event["delta"].as_str().unwrap_or_default();
                if !delta.is_empty() {
                    let idx = self.ensure_text_block(output_index, content_index, &mut events);
                    events.push(content_delta(idx, "text_delta", "text", delta));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.ensure_started(event.get("response").unwrap_or(event), &mut events);
                let delta = event["delta"].as_str().unwrap_or_default();
                if !delta.is_empty() {
                    let idx = self.ensure_thinking_block(&mut events);
                    events.push(content_delta(idx, "thinking_delta", "thinking", delta));
                }
            }
            "response.function_call_arguments.delta" => {
                self.ensure_started(event.get("response").unwrap_or(event), &mut events);
                let output_index = event["output_index"].as_u64().unwrap_or(0);
                let delta = event["delta"].as_str().unwrap_or_default();
                if !delta.is_empty() {
                    self.ensure_function_block(output_index, event, &mut events);
                    self.function_argument_buffers
                        .entry(output_index)
                        .or_default()
                        .push_str(delta);
                    self.emit_parseable_function_arguments(output_index, event, &mut events);
                }
            }
            "response.function_call_arguments.done" => {
                self.ensure_started(event.get("response").unwrap_or(event), &mut events);
                self.handle_function_call_arguments_done(event, &mut events);
            }
            "response.output_item.done" => {
                self.handle_output_item_done(event, &mut events);
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                if let Some(response) = event.get("response") {
                    self.ensure_started(response, &mut events);
                    self.set_usage(response);
                    self.stop_response(response, &mut events);
                }
            }
            _ => {}
        }

        events
    }

    fn finish(&mut self) -> Vec<SseEvent> {
        let mut events = Vec::new();
        if self.started && !self.stopped {
            self.close_open_block(&mut events);
            self.stop_with_reason("end_turn", &mut events);
        }
        events
    }

    fn ensure_started(&mut self, response: &Value, events: &mut Vec<SseEvent>) {
        if self.started {
            return;
        }

        self.model = response["model"].as_str().unwrap_or("unknown").to_string();
        if let Some(id) = response["id"].as_str() {
            self.message_id = id.replace("resp_", "msg_");
        }
        self.set_usage(response);

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

    fn handle_output_item_added(&mut self, event: &Value, events: &mut Vec<SseEvent>) {
        let output_index = event["output_index"].as_u64().unwrap_or(0);
        let item = &event["item"];
        if item["type"].as_str() == Some("function_call") {
            self.saw_function_call = true;
            if let Some(name) = item["name"].as_str() {
                self.function_names.insert(output_index, name.to_string());
            }
            if let Some(call_id) = item["call_id"].as_str().or_else(|| item["id"].as_str()) {
                self.function_call_ids
                    .insert(output_index, call_id.to_string());
            }
            self.ensure_function_block(output_index, event, events);
        }
    }

    fn handle_content_part_added(&mut self, event: &Value, events: &mut Vec<SseEvent>) {
        let output_index = event["output_index"].as_u64().unwrap_or(0);
        let content_index = event["content_index"].as_u64().unwrap_or(0);
        let part = &event["part"];
        match part["type"].as_str() {
            Some("output_text") => {
                let idx = self.ensure_text_block(output_index, content_index, events);
                if let Some(text) = part["text"].as_str()
                    && !text.is_empty()
                {
                    events.push(content_delta(idx, "text_delta", "text", text));
                }
            }
            Some("refusal") => {
                let idx = self.ensure_text_block(output_index, content_index, events);
                if let Some(text) = part["refusal"].as_str()
                    && !text.is_empty()
                {
                    events.push(content_delta(idx, "text_delta", "text", text));
                }
            }
            _ => {}
        }
    }

    fn handle_output_item_done(&mut self, event: &Value, events: &mut Vec<SseEvent>) {
        let output_index = event["output_index"].as_u64().unwrap_or(0);
        let item = &event["item"];
        if item["type"].as_str() == Some("function_call") {
            self.saw_function_call = true;
            let idx = self.ensure_function_block(output_index, event, events);
            let arguments = item["arguments"].as_str();
            self.emit_function_arguments(output_index, idx, arguments, false, events);
            events.push(block_stop(idx));
            self.function_blocks.remove(&output_index);
            self.function_argument_buffers.remove(&output_index);
            self.function_argument_emitted.remove(&output_index);
        }
    }

    fn handle_function_call_arguments_done(&mut self, event: &Value, events: &mut Vec<SseEvent>) {
        let output_index = event["output_index"].as_u64().unwrap_or(0);
        self.saw_function_call = true;
        if let Some(name) = event["name"].as_str() {
            self.function_names.insert(output_index, name.to_string());
        }
        if let Some(call_id) = event["call_id"]
            .as_str()
            .or_else(|| event["item_id"].as_str())
        {
            self.function_call_ids
                .insert(output_index, call_id.to_string());
        }

        let idx = self.ensure_function_block(output_index, event, events);
        self.emit_function_arguments(
            output_index,
            idx,
            event["arguments"].as_str(),
            false,
            events,
        );
    }

    fn emit_parseable_function_arguments(
        &mut self,
        output_index: u64,
        event: &Value,
        events: &mut Vec<SseEvent>,
    ) {
        let Some(arguments) = self.function_argument_buffers.get(&output_index) else {
            return;
        };
        if serde_json::from_str::<Value>(arguments).is_err() {
            return;
        }
        let idx = self.ensure_function_block(output_index, event, events);
        self.emit_function_arguments(output_index, idx, None, true, events);
    }

    fn emit_function_arguments(
        &mut self,
        output_index: u64,
        idx: u32,
        final_arguments: Option<&str>,
        require_valid_json: bool,
        events: &mut Vec<SseEvent>,
    ) {
        let arguments = final_arguments
            .filter(|arguments| !arguments.is_empty())
            .map(str::to_string)
            .or_else(|| self.function_argument_buffers.get(&output_index).cloned())
            .unwrap_or_default();
        if arguments.is_empty() {
            return;
        }

        if require_valid_json && serde_json::from_str::<Value>(&arguments).is_err() {
            return;
        }

        let sanitized = self
            .function_names
            .get(&output_index)
            .and_then(|name| sanitize_read_empty_pages(name, &arguments))
            .unwrap_or(arguments);
        let previous = self
            .function_argument_emitted
            .get(&output_index)
            .map(String::as_str)
            .unwrap_or("");
        if sanitized == previous {
            return;
        }
        if let Some(delta) = sanitized.strip_prefix(previous) {
            Self::push_function_arguments_delta(idx, delta, events);
            self.function_argument_emitted
                .insert(output_index, sanitized);
        } else if previous.is_empty() {
            Self::push_function_arguments_delta(idx, &sanitized, events);
            self.function_argument_emitted
                .insert(output_index, sanitized);
        }
    }

    fn push_function_arguments_delta(idx: u32, arguments: &str, events: &mut Vec<SseEvent>) {
        events.push(SseEvent {
            event: "content_block_delta".to_string(),
            data: json!({
                "type": "content_block_delta",
                "index": idx,
                "delta": {"type": "input_json_delta", "partial_json": arguments}
            }),
        });
    }

    fn ensure_text_block(
        &mut self,
        output_index: u64,
        content_index: u64,
        events: &mut Vec<SseEvent>,
    ) -> u32 {
        if let Some(&idx) = self.output_blocks.get(&(output_index, content_index)) {
            if self.open_block != Some(OpenBlock::Text(idx)) {
                self.close_open_block(events);
                self.open_block = Some(OpenBlock::Text(idx));
            }
            return idx;
        }

        self.close_open_block(events);
        let idx = self.next_block_index;
        self.next_block_index += 1;
        self.output_blocks
            .insert((output_index, content_index), idx);
        self.open_block = Some(OpenBlock::Text(idx));
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
        if let Some(OpenBlock::Thinking(idx)) = self.open_block {
            return idx;
        }

        self.close_open_block(events);
        let idx = self.next_block_index;
        self.next_block_index += 1;
        self.open_block = Some(OpenBlock::Thinking(idx));
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

    fn ensure_function_block(
        &mut self,
        output_index: u64,
        event: &Value,
        events: &mut Vec<SseEvent>,
    ) -> u32 {
        if let Some(&idx) = self.function_blocks.get(&output_index) {
            return idx;
        }

        self.close_open_block(events);
        let item = &event["item"];
        let idx = self.next_block_index;
        self.next_block_index += 1;
        self.function_blocks.insert(output_index, idx);
        let call_id = item["call_id"]
            .as_str()
            .or_else(|| item["id"].as_str())
            .or_else(|| event["call_id"].as_str())
            .or_else(|| event["item_id"].as_str())
            .map(str::to_string)
            .or_else(|| self.function_call_ids.get(&output_index).cloned())
            .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4()));
        let name = item["name"]
            .as_str()
            .or_else(|| event["name"].as_str())
            .map(str::to_string)
            .or_else(|| self.function_names.get(&output_index).cloned())
            .unwrap_or_default();
        events.push(SseEvent {
            event: "content_block_start".to_string(),
            data: json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {
                    "type": "tool_use",
                    "id": call_id,
                    "name": name,
                    "input": {}
                }
            }),
        });
        idx
    }

    fn close_open_block(&mut self, events: &mut Vec<SseEvent>) {
        if let Some(block) = self.open_block.take() {
            let idx = match block {
                OpenBlock::Text(idx) | OpenBlock::Thinking(idx) => idx,
            };
            events.push(block_stop(idx));
        }
    }

    fn stop_response(&mut self, response: &Value, events: &mut Vec<SseEvent>) {
        if self.stopped {
            return;
        }
        self.close_open_block(events);
        self.close_function_blocks(events);
        let reason = response_stop_reason(response, self.saw_function_call);
        self.stop_with_reason(reason, events);
    }

    fn close_function_blocks(&mut self, events: &mut Vec<SseEvent>) {
        let blocks = std::mem::take(&mut self.function_blocks);
        for (output_index, idx) in blocks {
            self.emit_function_arguments(output_index, idx, None, false, events);
            events.push(block_stop(idx));
        }
        self.function_argument_buffers.clear();
        self.function_argument_emitted.clear();
    }

    fn stop_with_reason(&mut self, reason: &str, events: &mut Vec<SseEvent>) {
        if self.stopped {
            return;
        }
        events.push(SseEvent {
            event: "message_delta".to_string(),
            data: json!({
                "type": "message_delta",
                "delta": {"stop_reason": reason, "stop_sequence": null},
                "usage": {"input_tokens": self.input_tokens, "output_tokens": self.output_tokens}
            }),
        });
        events.push(SseEvent {
            event: "message_stop".to_string(),
            data: json!({"type": "message_stop"}),
        });
        self.stopped = true;
    }

    fn set_usage(&mut self, response: &Value) {
        if let Some(usage) = response.get("usage") {
            self.input_tokens = usage["input_tokens"].as_u64().unwrap_or(0) as u32;
            self.output_tokens = usage["output_tokens"].as_u64().unwrap_or(0) as u32;
        }
    }
}

fn response_stop_reason(response: &Value, saw_function_call: bool) -> &'static str {
    if let Some(reason) = response["incomplete_details"]["reason"].as_str() {
        return match reason {
            "max_output_tokens" => "max_tokens",
            "content_filter" | "content_policy_violation" => "refusal",
            _ => "end_turn",
        };
    }

    if response["status"].as_str() == Some("failed") {
        return "error";
    }

    if saw_function_call
        || response["output"].as_array().is_some_and(|items| {
            items
                .iter()
                .any(|item| item["type"].as_str() == Some("function_call"))
        })
    {
        "tool_use"
    } else {
        "end_turn"
    }
}

fn content_delta(index: u32, delta_type: &str, key: &str, value: &str) -> SseEvent {
    SseEvent {
        event: "content_block_delta".to_string(),
        data: json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": delta_type, key: value}
        }),
    }
}

fn block_stop(index: u32) -> SseEvent {
    SseEvent {
        event: "content_block_stop".to_string(),
        data: json!({"type": "content_block_stop", "index": index}),
    }
}

fn sanitize_read_empty_pages(tool_name: &str, arguments: &str) -> Option<String> {
    if tool_name != "Read" {
        return None;
    }

    let mut input = serde_json::from_str::<Value>(arguments).ok()?;
    let object = input.as_object_mut()?;
    if matches!(object.get("pages"), Some(Value::String(pages)) if pages.is_empty()) {
        object.remove("pages");
        return serde_json::to_string(&input).ok();
    }

    None
}

pub fn convert_non_streaming_response(data: &Value) -> Vec<SseEvent> {
    let mut converter = NonStreamingResponsesConverter::new(data);
    converter.convert()
}

struct NonStreamingResponsesConverter<'a> {
    data: &'a Value,
    events: Vec<SseEvent>,
    next_block_index: u32,
    input_tokens: u32,
    output_tokens: u32,
}

impl<'a> NonStreamingResponsesConverter<'a> {
    fn new(data: &'a Value) -> Self {
        let usage = &data["usage"];
        Self {
            data,
            events: Vec::new(),
            next_block_index: 0,
            input_tokens: usage["input_tokens"].as_u64().unwrap_or(0) as u32,
            output_tokens: usage["output_tokens"].as_u64().unwrap_or(0) as u32,
        }
    }

    fn convert(&mut self) -> Vec<SseEvent> {
        self.events.push(SseEvent {
            event: "message_start".to_string(),
            data: json!({
                "type": "message_start",
                "message": {
                    "id": self.data["id"].as_str().unwrap_or("msg_response"),
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": self.data["model"].as_str().unwrap_or("unknown"),
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": self.input_tokens, "output_tokens": 0}
                }
            }),
        });

        if let Some(output) = self.data["output"].as_array() {
            for item in output {
                match item["type"].as_str() {
                    Some("message") => self.convert_message(item),
                    Some("function_call") => self.convert_function_call(item),
                    Some("reasoning") => self.convert_reasoning_item(item),
                    _ => {}
                }
            }
        }

        self.events.push(SseEvent {
            event: "message_delta".to_string(),
            data: json!({
                "type": "message_delta",
                "delta": {"stop_reason": response_stop_reason(self.data, false), "stop_sequence": null},
                "usage": {"input_tokens": self.input_tokens, "output_tokens": self.output_tokens}
            }),
        });
        self.events.push(SseEvent {
            event: "message_stop".to_string(),
            data: json!({"type": "message_stop"}),
        });

        std::mem::take(&mut self.events)
    }

    fn convert_message(&mut self, item: &Value) {
        if let Some(content) = item["content"].as_array() {
            for part in content {
                match part["type"].as_str() {
                    Some("output_text") => {
                        if let Some(text) = part["text"].as_str()
                            && !text.is_empty()
                        {
                            self.add_text_block(text);
                        }
                    }
                    Some("refusal") => {
                        if let Some(text) = part["refusal"].as_str()
                            && !text.is_empty()
                        {
                            self.add_text_block(text);
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    fn convert_function_call(&mut self, item: &Value) {
        let idx = self.next_block_index;
        self.next_block_index += 1;
        let input = item["arguments"]
            .as_str()
            .and_then(|args| {
                let arguments = item["name"]
                    .as_str()
                    .and_then(|name| sanitize_read_empty_pages(name, args))
                    .unwrap_or_else(|| args.to_string());
                serde_json::from_str::<Value>(&arguments).ok()
            })
            .unwrap_or_else(|| json!({}));
        self.events.push(SseEvent {
            event: "content_block_start".to_string(),
            data: json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {
                    "type": "tool_use",
                    "id": item["call_id"].as_str().or_else(|| item["id"].as_str()).unwrap_or("call_unknown"),
                    "name": item["name"].as_str().unwrap_or(""),
                    "input": input
                }
            }),
        });
        self.events.push(block_stop(idx));
    }

    fn convert_reasoning_item(&mut self, item: &Value) {
        let mut summaries = Vec::new();
        if let Some(summary) = item["summary"].as_array() {
            for part in summary {
                if let Some(text) = part["text"].as_str() {
                    summaries.push(text);
                }
            }
        }
        if !summaries.is_empty() {
            self.add_thinking_block(&summaries.join("\n"));
        }
    }

    fn add_text_block(&mut self, text: &str) {
        let idx = self.next_block_index;
        self.next_block_index += 1;
        self.events.push(SseEvent {
            event: "content_block_start".to_string(),
            data: json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {"type": "text", "text": ""}
            }),
        });
        self.events
            .push(content_delta(idx, "text_delta", "text", text));
        self.events.push(block_stop(idx));
    }

    fn add_thinking_block(&mut self, text: &str) {
        let idx = self.next_block_index;
        self.next_block_index += 1;
        self.events.push(SseEvent {
            event: "content_block_start".to_string(),
            data: json!({
                "type": "content_block_start",
                "index": idx,
                "content_block": {"type": "thinking", "thinking": ""}
            }),
        });
        self.events
            .push(content_delta(idx, "thinking_delta", "thinking", text));
        self.events.push(block_stop(idx));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_to_responses_maps_tools_and_outputs() {
        let req = MessagesRequest {
            model: "gpt-5".to_string(),
            system: Some(SystemPrompt::Text("Be useful.".to_string())),
            messages: vec![
                Message {
                    role: Role::User,
                    content: MessageContent::Text("Weather?".to_string()),
                },
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(vec![Content::ToolUse {
                        id: "call_1".to_string(),
                        name: "weather".to_string(),
                        input: json!({"city": "Shanghai"}),
                    }]),
                },
                Message {
                    role: Role::User,
                    content: MessageContent::Blocks(vec![Content::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        content: Some(Value::String("sunny".to_string())),
                        is_error: None,
                    }]),
                },
            ],
            max_tokens: Some(1024),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: Some(vec![Tool {
                name: "weather".to_string(),
                description: Some("Get weather".to_string()),
                input_schema: json!({"type": "object"}),
            }]),
            tool_choice: None,
            thinking: Some(ThinkingConfig {
                r#type: Some("enabled".to_string()),
                budget_tokens: None,
            }),
            metadata: Some(json!({"user_id": "client-user"})),
            extra: HashMap::new(),
        };

        let body = convert_to_responses(&req);

        assert_eq!(body["instructions"], "Be useful.");
        assert_eq!(body["max_output_tokens"], 1024);
        assert_eq!(body["store"], false);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "weather");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["reasoning"]["summary"], "detailed");
        assert!(body.get("metadata").is_none());
    }

    #[test]
    fn test_convert_to_responses_normalizes_tool_schemas() {
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
            tools: Some(vec![
                Tool {
                    name: "empty".to_string(),
                    description: None,
                    input_schema: json!({}),
                },
                Tool {
                    name: "bad_required".to_string(),
                    description: None,
                    input_schema: json!({
                        "type": "object",
                        "properties": {"path": {"type": "string"}},
                        "required": "path"
                    }),
                },
                Tool {
                    name: "non_object".to_string(),
                    description: None,
                    input_schema: json!({"type": "string"}),
                },
            ]),
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: HashMap::new(),
        };

        let body = convert_to_responses(&req);

        assert_eq!(
            body["tools"][0]["parameters"],
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            body["tools"][1]["parameters"],
            json!({"type": "object", "properties": {"path": {"type": "string"}}})
        );
        assert_eq!(
            body["tools"][2]["parameters"],
            json!({"type": "object", "properties": {}})
        );
    }

    #[test]
    fn test_convert_to_responses_omits_include_without_reasoning() {
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
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: HashMap::new(),
        };

        let body = convert_to_responses(&req);

        assert!(body.get("include").is_none());
        assert!(body.get("parallel_tool_calls").is_none());
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn test_convert_to_responses_omits_reasoning_summary_for_none_effort() {
        let mut extra = HashMap::new();
        extra.insert("reasoning_effort".to_string(), json!("none"));
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
            tools: None,
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra,
        };

        let body = convert_to_responses(&req);

        assert!(body.get("include").is_none());
        assert_eq!(body["reasoning"], json!({"effort": "none"}));
    }

    #[test]
    fn test_convert_to_responses_includes_reasoning_for_history_thinking() {
        let req = MessagesRequest {
            model: "gpt-5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![Content::Thinking {
                    thinking: "prior thought".to_string(),
                    signature: None,
                }]),
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
            metadata: None,
            extra: HashMap::new(),
        };

        let body = convert_to_responses(&req);

        assert_eq!(body["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn test_convert_to_responses_truncates_old_large_tool_outputs() {
        let large_output = "x".repeat(MAX_HISTORICAL_TEXT_BYTES * 2);
        let mut messages = Vec::new();
        for index in 0..(RECENT_TOOL_OUTPUTS_TO_KEEP + 2) {
            messages.push(Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![Content::ToolResult {
                    tool_use_id: format!("call_{index}"),
                    content: Some(Value::String(large_output.clone())),
                    is_error: if index == 0 { Some(true) } else { None },
                }]),
            });
        }
        messages.push(Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![Content::ToolResult {
                tool_use_id: "current_call".to_string(),
                content: Some(Value::String(large_output.clone())),
                is_error: None,
            }]),
        });
        let req = MessagesRequest {
            model: "gpt-5.4-mini".to_string(),
            system: None,
            messages,
            max_tokens: None,
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

        let body = convert_to_responses(&req);
        let input = body["input"].as_array().expect("input items");

        assert_eq!(
            input[0]["output"]
                .as_str()
                .unwrap()
                .starts_with("ERROR: [tool output truncated:"),
            true
        );
        assert!(
            input[1]["output"]
                .as_str()
                .unwrap()
                .starts_with("[tool output truncated:")
        );
        assert_eq!(input[2]["output"], large_output);
        assert_eq!(input.last().unwrap()["output"], large_output);
    }

    #[test]
    fn test_convert_to_responses_truncates_old_large_text_items() {
        let large_text = "x".repeat(MAX_HISTORICAL_TEXT_BYTES * 2);
        let mut messages = Vec::new();
        for _ in 0..(RECENT_TEXT_ITEMS_TO_KEEP + 2) {
            messages.push(Message {
                role: Role::User,
                content: MessageContent::Text(large_text.clone()),
            });
        }
        messages.push(Message {
            role: Role::User,
            content: MessageContent::Text(large_text.clone()),
        });
        let req = MessagesRequest {
            model: "gpt-5.4-mini".to_string(),
            system: None,
            messages,
            max_tokens: None,
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

        let body = convert_to_responses(&req);
        let input = body["input"].as_array().expect("input items");

        assert!(
            input[0]["content"]
                .as_str()
                .unwrap()
                .starts_with("[text content truncated:")
        );
        assert!(
            input[1]["content"]
                .as_str()
                .unwrap()
                .starts_with("[text content truncated:")
        );
        assert_eq!(input[2]["content"], large_text);
        assert_eq!(input.last().unwrap()["content"], large_text);
    }

    #[test]
    fn test_convert_to_responses_preserves_large_model_history_within_budget() {
        let large_text = "x".repeat(MAX_HISTORICAL_TEXT_BYTES * 2);
        let mut messages = Vec::new();
        for _ in 0..(RECENT_TEXT_ITEMS_TO_KEEP + 2) {
            messages.push(Message {
                role: Role::User,
                content: MessageContent::Text(large_text.clone()),
            });
        }
        messages.push(Message {
            role: Role::User,
            content: MessageContent::Text("current".to_string()),
        });
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages,
            max_tokens: None,
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

        let body = convert_to_responses(&req);
        let input = body["input"].as_array().expect("input items");

        assert_eq!(input[0]["content"], large_text);
        assert_eq!(input[1]["content"], large_text);
    }

    #[test]
    fn test_convert_to_responses_does_not_truncate_thinking_text_items() {
        let large_text = "x".repeat(MAX_HISTORICAL_TEXT_BYTES * 2);
        let mut messages = Vec::new();
        for _ in 0..(RECENT_TEXT_ITEMS_TO_KEEP + 20) {
            messages.push(Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![Content::Thinking {
                    thinking: large_text.clone(),
                    signature: None,
                }]),
            });
        }
        messages.push(Message {
            role: Role::User,
            content: MessageContent::Text("current".to_string()),
        });
        let req = MessagesRequest {
            model: "gpt-5.4-mini".to_string(),
            system: None,
            messages,
            max_tokens: None,
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

        let body = convert_to_responses(&req);
        let input = body["input"].as_array().expect("input items");

        assert!(input[0]["content"].as_str().unwrap().contains("[thinking]"));
        assert!(
            !input[0]["content"]
                .as_str()
                .unwrap()
                .contains("text content truncated")
        );
    }

    #[test]
    fn test_tool_result_text_prefixes_errors() {
        let text = tool_result_text(
            &Some(Value::String("failed".to_string())),
            Some(true),
            false,
        );

        assert_eq!(text, "ERROR: failed");
    }

    #[test]
    fn test_stream_converter_maps_text_function_and_incomplete() {
        let mut converter = ResponsesStreamConverter::new();
        let mut events = Vec::new();

        events.extend(converter.process_event(&json!({
            "type": "response.created",
            "response": {"id": "resp_1", "model": "gpt-5", "usage": null}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "content_index": 0,
            "delta": "hello"
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.output_item.added",
            "output_index": 1,
            "item": {"type": "function_call", "call_id": "call_1", "name": "weather"}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 1,
            "delta": "{\"city\":\"Shanghai\"}"
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "model": "gpt-5",
                "status": "completed",
                "output": [{"type": "function_call"}],
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }
        })));

        assert!(events.iter().any(|event| event.event == "message_start"));
        assert!(events.iter().any(|event| {
            event.event == "content_block_delta"
                && event.data["delta"]["type"] == "input_json_delta"
        }));
        let stop = events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta");
        assert_eq!(stop.data["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn test_stream_converter_uses_done_function_arguments_without_delta() {
        let mut converter = ResponsesStreamConverter::new();
        let mut events = Vec::new();

        events.extend(converter.process_event(&json!({
            "type": "response.created",
            "response": {"id": "resp_1", "model": "gpt-5", "usage": null}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "function_call", "call_id": "call_1", "name": "Edit"}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.function_call_arguments.done",
            "output_index": 0,
            "call_id": "call_1",
            "name": "Edit",
            "arguments": "{\"file_path\":\"src/main.rs\",\"old_string\":\"old\",\"new_string\":\"new\"}"
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "call_id": "call_1",
                "name": "Edit"
            }
        })));

        assert!(events.iter().any(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "tool_use"
                && event.data["content_block"]["name"] == "Edit"
                && event.data["content_block"]["id"] == "call_1"
        }));
        assert!(events.iter().any(|event| {
            event.event == "content_block_delta"
                && event.data["delta"]["type"] == "input_json_delta"
                && event.data["delta"]["partial_json"]
                    == "{\"file_path\":\"src/main.rs\",\"old_string\":\"old\",\"new_string\":\"new\"}"
        }));
    }

    #[test]
    fn test_stream_converter_marks_tool_use_when_completed_output_is_empty() {
        let mut converter = ResponsesStreamConverter::new();
        let mut events = Vec::new();

        events.extend(converter.process_event(&json!({
            "type": "response.created",
            "response": {"id": "resp_1", "model": "gpt-5", "usage": null}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Read", "arguments": ""}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "item_id": "fc_1",
            "delta": "{\"file_path\":\"src/main.rs\"}"
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.output_item.done",
            "output_index": 0,
            "item": {
                "type": "function_call",
                "id": "fc_1",
                "call_id": "call_1",
                "name": "Read",
                "arguments": "{\"file_path\":\"src/main.rs\"}"
            }
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "model": "gpt-5",
                "status": "completed",
                "output": [],
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }
        })));

        let stop = events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta");
        assert_eq!(stop.data["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn test_stream_converter_sanitizes_split_read_arguments_on_done() {
        let mut converter = ResponsesStreamConverter::new();
        let mut events = Vec::new();

        events.extend(converter.process_event(&json!({
            "type": "response.created",
            "response": {"id": "resp_1", "model": "gpt-5", "usage": null}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Read", "arguments": ""}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "item_id": "fc_1",
            "delta": "{\"file_path\":\"src/main.rs\","
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "item_id": "fc_1",
            "delta": "\"pages\":\"\"}"
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.function_call_arguments.done",
            "output_index": 0,
            "item_id": "fc_1",
            "name": "Read",
            "arguments": "{\"file_path\":\"src/main.rs\",\"pages\":\"\"}"
        })));

        let arguments = events
            .iter()
            .filter_map(|event| {
                (event.event == "content_block_delta"
                    && event.data["delta"]["type"] == "input_json_delta")
                    .then(|| event.data["delta"]["partial_json"].as_str())
                    .flatten()
            })
            .collect::<String>();
        let arguments: Value = serde_json::from_str(&arguments).expect("valid arguments");
        assert_eq!(arguments, json!({"file_path": "src/main.rs"}));
    }

    #[test]
    fn test_stream_converter_incrementally_sanitizes_when_json_completes() {
        let mut converter = ResponsesStreamConverter::new();

        let mut events = converter.process_event(&json!({
            "type": "response.created",
            "response": {"id": "resp_1", "model": "gpt-5", "usage": null}
        }));
        events.extend(converter.process_event(&json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "Read", "arguments": ""}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "item_id": "fc_1",
            "delta": "{\"file_path\":\"src/main.rs\","
        })));

        assert!(!events.iter().any(|event| {
            event.event == "content_block_delta"
                && event.data["delta"]["type"] == "input_json_delta"
        }));

        let events = converter.process_event(&json!({
            "type": "response.function_call_arguments.delta",
            "output_index": 0,
            "item_id": "fc_1",
            "delta": "\"pages\":\"\"}"
        }));
        let arguments = events
            .iter()
            .filter_map(|event| {
                (event.event == "content_block_delta"
                    && event.data["delta"]["type"] == "input_json_delta")
                    .then(|| event.data["delta"]["partial_json"].as_str())
                    .flatten()
            })
            .collect::<String>();
        let arguments: Value = serde_json::from_str(&arguments).expect("valid arguments");
        assert_eq!(arguments, json!({"file_path": "src/main.rs"}));
    }

    #[test]
    fn test_stream_converter_chatgpt_codex_tool_fixture() {
        let mut converter = ResponsesStreamConverter::new();
        let fixture = [
            json!({
                "type": "response.created",
                "response": {"id": "resp_chatgpt_1", "model": "gpt-5.3-codex", "usage": null}
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "id": "fc_chatgpt_1",
                    "call_id": "call_chatgpt_1",
                    "name": "Bash",
                    "arguments": ""
                }
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "item_id": "fc_chatgpt_1",
                "delta": "{\"command\":\"git"
            }),
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": 0,
                "item_id": "fc_chatgpt_1",
                "delta": " status --short\",\"description\":\"\"}"
            }),
            json!({
                "type": "response.function_call_arguments.done",
                "output_index": 0,
                "item_id": "fc_chatgpt_1",
                "name": "Bash",
                "arguments": "{\"command\":\"git status --short\",\"description\":\"\"}"
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": {
                    "type": "function_call",
                    "id": "fc_chatgpt_1",
                    "call_id": "call_chatgpt_1",
                    "name": "Bash",
                    "arguments": "{\"command\":\"git status --short\",\"description\":\"\"}"
                }
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_chatgpt_1",
                    "model": "gpt-5.3-codex",
                    "status": "completed",
                    "output": [],
                    "usage": {"input_tokens": 10, "output_tokens": 5}
                }
            }),
        ];

        let events = fixture
            .iter()
            .flat_map(|event| converter.process_event(event))
            .collect::<Vec<_>>();
        let arguments = events
            .iter()
            .filter_map(|event| {
                (event.event == "content_block_delta"
                    && event.data["delta"]["type"] == "input_json_delta")
                    .then(|| event.data["delta"]["partial_json"].as_str())
                    .flatten()
            })
            .collect::<String>();
        let stop = events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta");

        assert_eq!(
            serde_json::from_str::<Value>(&arguments).expect("valid arguments"),
            json!({"command": "git status --short", "description": ""})
        );
        assert_eq!(stop.data["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn test_non_streaming_response_maps_text_and_function() {
        let data = json!({
            "id": "resp_1",
            "model": "gpt-5",
            "status": "completed",
            "output": [
                {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "hello"}]
                },
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "weather",
                    "arguments": "{\"city\":\"Shanghai\"}"
                }
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });

        let events = convert_non_streaming_response(&data);

        assert_eq!(
            events.first().map(|event| event.event.as_str()),
            Some("message_start")
        );
        assert!(events.iter().any(|event| {
            event.event == "content_block_start"
                && event.data["content_block"]["type"] == "tool_use"
        }));
        assert_eq!(
            events
                .iter()
                .find(|event| event.event == "message_delta")
                .unwrap()
                .data["delta"]["stop_reason"],
            "tool_use"
        );
    }

    #[test]
    fn test_stream_converter_sanitizes_read_empty_pages_from_done_arguments() {
        let mut converter = ResponsesStreamConverter::new();
        let mut events = Vec::new();

        events.extend(converter.process_event(&json!({
            "type": "response.created",
            "response": {"id": "resp_1", "model": "gpt-5", "usage": null}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "function_call", "call_id": "call_1", "name": "Read"}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.function_call_arguments.done",
            "output_index": 0,
            "call_id": "call_1",
            "name": "Read",
            "arguments": "{\"file_path\":\"src/main.rs\",\"pages\":\"\"}"
        })));

        let arguments = events
            .iter()
            .find_map(|event| {
                (event.event == "content_block_delta"
                    && event.data["delta"]["type"] == "input_json_delta")
                    .then(|| event.data["delta"]["partial_json"].as_str())
                    .flatten()
            })
            .expect("tool arguments");
        let arguments: Value = serde_json::from_str(arguments).expect("valid arguments");
        assert_eq!(arguments, json!({"file_path": "src/main.rs"}));
    }

    #[test]
    fn test_stream_converter_preserves_bash_command_and_empty_strings() {
        let mut converter = ResponsesStreamConverter::new();
        let mut events = Vec::new();

        events.extend(converter.process_event(&json!({
            "type": "response.created",
            "response": {"id": "resp_1", "model": "gpt-5", "usage": null}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {"type": "function_call", "call_id": "call_1", "name": "Bash"}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.function_call_arguments.done",
            "output_index": 0,
            "call_id": "call_1",
            "name": "Bash",
            "arguments": "{\"command\":\"git status --short\",\"description\":\"\",\"run_in_background\":false}"
        })));

        let arguments = events
            .iter()
            .find_map(|event| {
                (event.event == "content_block_delta"
                    && event.data["delta"]["type"] == "input_json_delta")
                    .then(|| event.data["delta"]["partial_json"].as_str())
                    .flatten()
            })
            .expect("tool arguments");
        assert_eq!(
            arguments,
            "{\"command\":\"git status --short\",\"description\":\"\",\"run_in_background\":false}"
        );
    }

    #[test]
    fn test_non_streaming_response_sanitizes_read_empty_pages_only() {
        let data = json!({
            "id": "resp_1",
            "model": "gpt-5",
            "status": "completed",
            "output": [
                {
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "Read",
                    "arguments": "{\"file_path\":\"src/main.rs\",\"pages\":\"\"}"
                },
                {
                    "type": "function_call",
                    "call_id": "call_2",
                    "name": "Bash",
                    "arguments": "{\"command\":\"git status --short\",\"description\":\"\"}"
                }
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });

        let events = convert_non_streaming_response(&data);
        let inputs = events
            .iter()
            .filter_map(|event| {
                (event.event == "content_block_start"
                    && event.data["content_block"]["type"] == "tool_use")
                    .then_some(&event.data["content_block"]["input"])
            })
            .collect::<Vec<_>>();

        assert_eq!(inputs[0], &json!({"file_path": "src/main.rs"}));
        assert_eq!(
            inputs[1],
            &json!({"command": "git status --short", "description": ""})
        );
    }

    #[test]
    fn test_non_read_tool_pages_empty_string_is_preserved() {
        assert_eq!(
            sanitize_read_empty_pages("Other", "{\"pages\":\"\",\"value\":\"\"}"),
            None
        );
    }
}
