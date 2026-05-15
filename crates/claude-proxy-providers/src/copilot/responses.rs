use std::collections::{HashMap, HashSet};

use claude_proxy_core::*;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::http::fmt_reqwest_err;
use crate::provider::ProviderError;

pub fn convert_to_responses(req: &MessagesRequest) -> Value {
    let mut input = Vec::new();

    for msg in &req.messages {
        append_message_items(&mut input, msg);
    }

    let mut body = json!({
        "model": req.model,
        "input": input,
        "stream": req.stream,
        "store": false,
        "parallel_tool_calls": true,
    });

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
    if let Some(metadata) = &req.metadata {
        body["metadata"] = metadata.clone();
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

fn append_message_items(input: &mut Vec<Value>, msg: &Message) {
    match &msg.content {
        MessageContent::Text(text) => {
            input.push(message_item(&msg.role, text.clone()));
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
                            input.push(message_item(&msg.role, text_parts.join("\n")));
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
                            input.push(message_item(&msg.role, text_parts.join("\n")));
                            text_parts.clear();
                        }
                        let output = tool_result_text(content, *is_error);
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
                input.push(message_item(&msg.role, text_parts.join("\n")));
            }
        }
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

fn tool_result_text(content: &Option<Value>, is_error: Option<bool>) -> String {
    let text = match content {
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
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
        return Some(json!({"effort": effort}));
    }
    let thinking = req.thinking.as_ref()?;
    if thinking.r#type.as_deref() == Some("disabled") {
        return Some(json!({"effort": "none"}));
    }
    if thinking.r#type.as_deref() == Some("enabled") || thinking.budget_tokens.is_some() {
        return Some(json!({"effort": "medium"}));
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
                    events.push(SseEvent {
                        event: "content_block_delta".to_string(),
                        data: json!({
                            "type": "content_block_delta",
                            "index": idx,
                            "delta": {"type": "input_json_delta", "partial_json": delta}
                        }),
                    });
                }
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
                events.push(SseEvent {
                    event: "content_block_delta".to_string(),
                    data: json!({
                        "type": "content_block_delta",
                        "index": idx,
                        "delta": {"type": "input_json_delta", "partial_json": arguments}
                    }),
                });
            }
            events.push(block_stop(idx));
            self.function_blocks.remove(&output_index);
            self.function_argument_streamed.remove(&output_index);
        }
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
            .map(str::to_string)
            .or_else(|| self.function_call_ids.get(&output_index).cloned())
            .unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4()));
        let name = item["name"]
            .as_str()
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
            metadata: None,
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
        assert_eq!(body["reasoning"]["effort"], "medium");
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
