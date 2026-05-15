use std::collections::{HashMap, HashSet};

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

pub fn convert_to_responses(req: &MessagesRequest) -> Value {
    let mut input = Vec::new();
    let current_message_index = req.messages.len().saturating_sub(1);
    let historical_messages = req.messages.iter().take(current_message_index);
    let historical_tool_outputs = historical_messages
        .clone()
        .map(count_tool_results)
        .sum::<usize>();
    let historical_text_items = historical_messages.map(count_text_items).sum::<usize>();
    let mut historical_tool_outputs_to_truncate =
        historical_tool_outputs.saturating_sub(RECENT_TOOL_OUTPUTS_TO_KEEP);
    let mut historical_text_items_to_truncate =
        historical_text_items.saturating_sub(RECENT_TEXT_ITEMS_TO_KEEP);

    for (index, msg) in req.messages.iter().enumerate() {
        append_message_items(
            &mut input,
            msg,
            index == current_message_index,
            &mut historical_tool_outputs_to_truncate,
            &mut historical_text_items_to_truncate,
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

fn count_tool_results(message: &Message) -> usize {
    match &message.content {
        MessageContent::Text(_) => 0,
        MessageContent::Blocks(blocks) => blocks
            .iter()
            .filter(|block| matches!(block, Content::ToolResult { .. }))
            .count(),
    }
}

fn count_text_items(message: &Message) -> usize {
    match &message.content {
        MessageContent::Text(_) => 1,
        MessageContent::Blocks(blocks) => {
            let mut count = 0;
            let mut has_pending_text = false;
            for block in blocks {
                match block {
                    Content::Text { .. } | Content::Thinking { .. } => has_pending_text = true,
                    Content::ToolUse { .. }
                    | Content::ServerToolUse { .. }
                    | Content::ToolResult { .. } => {
                        if has_pending_text {
                            count += 1;
                            has_pending_text = false;
                        }
                    }
                    Content::Unknown => {}
                }
            }
            count + usize::from(has_pending_text)
        }
    }
}

fn append_message_items(
    input: &mut Vec<Value>,
    msg: &Message,
    is_current_message: bool,
    historical_tool_outputs_to_truncate: &mut usize,
    historical_text_items_to_truncate: &mut usize,
) {
    match &msg.content {
        MessageContent::Text(text) => {
            input.push(message_item(
                &msg.role,
                text_item_text(
                    text,
                    should_truncate_text_item(
                        is_current_message,
                        historical_text_items_to_truncate,
                    ),
                ),
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
                                text_item_text(
                                    &text_parts.join("\n"),
                                    should_truncate_text_item(
                                        is_current_message,
                                        historical_text_items_to_truncate,
                                    ),
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
                                text_item_text(
                                    &text_parts.join("\n"),
                                    should_truncate_text_item(
                                        is_current_message,
                                        historical_text_items_to_truncate,
                                    ),
                                ),
                            ));
                            text_parts.clear();
                        }
                        let output = tool_result_text(
                            content,
                            *is_error,
                            should_truncate_tool_output(
                                is_current_message,
                                historical_tool_outputs_to_truncate,
                            ),
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
                    text_item_text(
                        &text_parts.join("\n"),
                        should_truncate_text_item(
                            is_current_message,
                            historical_text_items_to_truncate,
                        ),
                    ),
                ));
            }
        }
    }
}

fn should_truncate_tool_output(
    is_current_message: bool,
    historical_tool_outputs_to_truncate: &mut usize,
) -> bool {
    if is_current_message || *historical_tool_outputs_to_truncate == 0 {
        return false;
    }
    *historical_tool_outputs_to_truncate -= 1;
    true
}

fn should_truncate_text_item(
    is_current_message: bool,
    historical_text_items_to_truncate: &mut usize,
) -> bool {
    if is_current_message || *historical_text_items_to_truncate == 0 {
        return false;
    }
    *historical_text_items_to_truncate -= 1;
    true
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
        "parameters": tool.input_schema,
    });

    if let Some(description) = &tool.description {
        value["description"] = json!(description);
    }

    value
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
    function_argument_streamed: HashSet<u64>,
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
                    let idx = self.ensure_function_block(output_index, event, &mut events);
                    self.function_argument_streamed.insert(output_index);
                    Self::push_function_arguments_delta(idx, delta, &mut events);
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
            let idx = self.ensure_function_block(output_index, event, events);
            if !self.function_argument_streamed.contains(&output_index)
                && let Some(arguments) = item["arguments"].as_str()
                && !arguments.is_empty()
            {
                Self::push_function_arguments_delta(idx, arguments, events);
                self.function_argument_streamed.insert(output_index);
            }
            events.push(block_stop(idx));
            self.function_blocks.remove(&output_index);
            self.function_argument_streamed.remove(&output_index);
        }
    }

    fn handle_function_call_arguments_done(&mut self, event: &Value, events: &mut Vec<SseEvent>) {
        let output_index = event["output_index"].as_u64().unwrap_or(0);
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
        if self.function_argument_streamed.contains(&output_index) {
            return;
        }

        let arguments = event["arguments"].as_str().unwrap_or_default();
        if !arguments.is_empty() {
            let idx = self.ensure_function_block(output_index, event, events);
            Self::push_function_arguments_delta(idx, arguments, events);
            self.function_argument_streamed.insert(output_index);
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
        let reason = response_stop_reason(response);
        self.stop_with_reason(reason, events);
    }

    fn close_function_blocks(&mut self, events: &mut Vec<SseEvent>) {
        let blocks = std::mem::take(&mut self.function_blocks);
        for idx in blocks.values() {
            events.push(block_stop(*idx));
        }
        self.function_argument_streamed.clear();
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

fn response_stop_reason(response: &Value) -> &'static str {
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

    if response["output"].as_array().is_some_and(|items| {
        items
            .iter()
            .any(|item| item["type"].as_str() == Some("function_call"))
    }) {
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
                "delta": {"stop_reason": response_stop_reason(self.data), "stop_sequence": null},
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
            .and_then(|args| serde_json::from_str::<Value>(args).ok())
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
        let large_output = "x".repeat(MAX_HISTORICAL_TOOL_OUTPUT_BYTES + 1);
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
            model: "gpt-5".to_string(),
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
        let large_text = "x".repeat(MAX_HISTORICAL_TEXT_BYTES + 1);
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
            model: "gpt-5".to_string(),
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
}
