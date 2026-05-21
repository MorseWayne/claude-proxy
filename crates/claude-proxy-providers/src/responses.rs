use claude_proxy_config::settings::ReasoningMarkerMode;
use claude_proxy_core::*;
use futures::StreamExt;
use futures::stream::BoxStream;
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::http::{fmt_reqwest_err, next_upstream_stream_item};
use crate::openai_compat::{
    default_adaptive_reasoning_effort, supports_reasoning_summary, supports_sampling_parameters,
    thinking_budget_to_reasoning_effort,
};
use crate::provider::ProviderError;
use crate::reasoning_markers::{ReasoningTextSplitter, TextSegment, split_text};
use crate::sse::{SseDecoder, is_sse_done, parse_sse_json_value};
use crate::tool_args::sanitize_tool_arguments;
use crate::tool_choice::normalize_for_responses;

const RECENT_TOOL_OUTPUTS_TO_KEEP: usize = 12;
const MAX_HISTORICAL_TOOL_OUTPUT_BYTES: usize = 4096;
const MAX_CURRENT_TOOL_OUTPUT_BYTES: usize = 128 * 1024;
const RECENT_TEXT_ITEMS_TO_KEEP: usize = 12;
const MAX_HISTORICAL_TEXT_BYTES: usize = 32 * 1024;
const SMALL_HISTORY_PAYLOAD_BUDGET_BYTES: usize = 256 * 1024;
const DEFAULT_HISTORY_PAYLOAD_BUDGET_BYTES: usize = 512 * 1024;
const LARGE_HISTORY_PAYLOAD_BUDGET_BYTES: usize = 1024 * 1024;
const MAX_MALFORMED_STREAM_PREVIEW_BYTES: usize = 1024;

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

#[derive(Debug)]
enum ResponsesMessagePart {
    Text(String),
    Input(Value),
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
    if supports_sampling_parameters(req) {
        if let Some(temperature) = req.temperature {
            body["temperature"] = json!(temperature);
        }
        if let Some(top_p) = req.top_p {
            body["top_p"] = json!(top_p);
        }
    }
    if let Some(stop) = &req.stop_sequences {
        body["stop"] = json!(stop);
    }
    if let Some(tools) = &req.tools {
        body["tools"] = json!(tools.iter().map(convert_tool).collect::<Vec<_>>());
    }
    if let Some(tool_choice) = &req.tool_choice {
        body["tool_choice"] = normalize_for_responses(tool_choice);
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
            Content::Unknown(_) => {}
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
            let mut parts = Vec::new();
            for block in blocks {
                match block {
                    Content::Text { text } => parts.push(ResponsesMessagePart::Text(text.clone())),
                    Content::Thinking { .. } => {}
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
                        flush_message_parts(
                            input,
                            &msg.role,
                            &mut parts,
                            is_current_message,
                            compression,
                        );
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
                        flush_message_parts(
                            input,
                            &msg.role,
                            &mut parts,
                            is_current_message,
                            compression,
                        );
                        let output = tool_result_output(
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
                    Content::Unknown(value) => {
                        parts.push(content_part_from_unknown(value));
                    }
                }
            }
            flush_message_parts(
                input,
                &msg.role,
                &mut parts,
                is_current_message,
                compression,
            );
        }
    }
}

fn flush_message_parts(
    input: &mut Vec<Value>,
    role: &Role,
    parts: &mut Vec<ResponsesMessagePart>,
    is_current_message: bool,
    compression: &mut HistoryCompressionState,
) {
    if parts.is_empty() {
        return;
    }

    let has_input = parts
        .iter()
        .any(|part| matches!(part, ResponsesMessagePart::Input(_)));

    if !has_input {
        let text = parts
            .iter()
            .filter_map(|part| match part {
                ResponsesMessagePart::Text(text) => Some(text.as_str()),
                ResponsesMessagePart::Input(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        input.push(message_item(
            role,
            compressed_text_item(&text, is_current_message, compression),
        ));
        parts.clear();
        return;
    }

    let content = parts
        .iter()
        .filter_map(|part| match part {
            ResponsesMessagePart::Text(text) if text.is_empty() => None,
            ResponsesMessagePart::Text(text) => Some(json!({
                "type": "input_text",
                "text": compressed_text_item(text, is_current_message, compression),
            })),
            ResponsesMessagePart::Input(value) => Some(value.clone()),
        })
        .collect::<Vec<_>>();

    if !content.is_empty() {
        input.push(message_content_item(role, content));
    }
    parts.clear();
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
    if is_current_message || compression.text_items_to_consider == 0 {
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

fn message_content_item(role: &Role, content: Vec<Value>) -> Value {
    let role = match role {
        Role::User => "user",
        Role::Assistant => "assistant",
    };
    json!({
        "role": role,
        "content": content,
    })
}

fn content_part_from_unknown(value: &Value) -> ResponsesMessagePart {
    if let Some(part) = content_part_from_value(value) {
        return part;
    }
    ResponsesMessagePart::Text(value.to_string())
}

fn content_part_from_value(value: &Value) -> Option<ResponsesMessagePart> {
    match value.get("type").and_then(Value::as_str) {
        Some("text") => value
            .get("text")
            .and_then(Value::as_str)
            .map(|text| ResponsesMessagePart::Text(text.to_string())),
        Some("image") | Some("input_image") | Some("image_url") => {
            unknown_image_url(value).map(input_image_part)
        }
        Some("document") | Some("input_file") | Some("file") => {
            input_file_part(value).map(ResponsesMessagePart::Input)
        }
        _ => None,
    }
}

fn input_image_part(image_url: String) -> ResponsesMessagePart {
    ResponsesMessagePart::Input(json!({
        "type": "input_image",
        "image_url": image_url,
    }))
}

fn unknown_image_url(value: &Value) -> Option<String> {
    match value.get("type").and_then(Value::as_str) {
        Some("image") => image_source_url(value.get("source")?),
        Some("input_image") => image_url_string(value.get("image_url")?),
        Some("image_url") => image_url_string(value.get("image_url")?),
        _ => None,
    }
}

fn image_url_string(value: &Value) -> Option<String> {
    match value {
        Value::String(url) => Some(url.clone()),
        Value::Object(image_url) => image_url
            .get("url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        _ => None,
    }
}

fn image_source_url(source: &Value) -> Option<String> {
    match source.get("type").and_then(Value::as_str) {
        Some("base64") => {
            let media_type = string_field(source, &["media_type", "mediaType"])?;
            let data = source.get("data").and_then(Value::as_str)?;
            (!media_type.is_empty() && !data.is_empty())
                .then(|| format!("data:{media_type};base64,{data}"))
        }
        Some("url") => source
            .get("url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        _ => None,
    }
}

fn input_file_part(value: &Value) -> Option<Value> {
    match value.get("type").and_then(Value::as_str) {
        Some("input_file") => normalize_input_file_part(value),
        Some("file") => value
            .get("file")
            .and_then(normalize_input_file_part)
            .or_else(|| normalize_input_file_part(value)),
        Some("document") => {
            let source = value.get("source")?;
            let mut part = match source.get("type").and_then(Value::as_str) {
                Some("base64") => {
                    let data = source.get("data").and_then(Value::as_str)?;
                    (!data.is_empty()).then(|| {
                        json!({
                            "type": "input_file",
                            "file_data": data,
                        })
                    })?
                }
                Some("url") => {
                    let url = source.get("url").and_then(Value::as_str)?;
                    (!url.is_empty()).then(|| {
                        json!({
                            "type": "input_file",
                            "file_url": url,
                        })
                    })?
                }
                Some("file") => {
                    let file_id = string_field(source, &["file_id", "fileId", "id"])?;
                    (!file_id.is_empty()).then(|| {
                        json!({
                            "type": "input_file",
                            "file_id": file_id,
                        })
                    })?
                }
                _ => return None,
            };
            if let Some(filename) = string_field(value, &["filename", "name", "title"])
                .or_else(|| string_field(source, &["filename", "name", "title"]))
            {
                part["filename"] = json!(filename);
            }
            Some(part)
        }
        _ => None,
    }
}

fn normalize_input_file_part(value: &Value) -> Option<Value> {
    let mut part = json!({"type": "input_file"});
    let mut has_payload = false;
    for field in ["file_data", "file_id", "file_url", "filename"] {
        if let Some(text) = value.get(field).and_then(Value::as_str)
            && !text.is_empty()
        {
            part[field] = json!(text);
            if field != "filename" {
                has_payload = true;
            }
        }
    }
    has_payload.then_some(part)
}

fn string_field<'a>(value: &'a Value, fields: &[&str]) -> Option<&'a str> {
    fields
        .iter()
        .find_map(|field| value.get(*field).and_then(Value::as_str))
}

fn tool_result_text(
    content: &Option<Value>,
    is_error: Option<bool>,
    truncate_if_large: bool,
) -> String {
    let text = if is_error == Some(true) {
        tool_result_error_text(content)
    } else {
        match content {
            Some(Value::String(text)) => text.clone(),
            Some(value) => value.to_string(),
            None => String::new(),
        }
    };
    let text = if truncate_if_large && text.len() > MAX_HISTORICAL_TOOL_OUTPUT_BYTES {
        historical_tool_result_truncation_marker(text.len())
    } else if text.len() > MAX_CURRENT_TOOL_OUTPUT_BYTES {
        truncated_current_tool_result_text(&text)
    } else {
        text
    };
    if is_error == Some(true) {
        format!("ERROR: {text}")
    } else {
        text
    }
}

fn tool_result_error_text(content: &Option<Value>) -> String {
    match content {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(tool_result_error_text_item)
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::String(text)) => text.clone(),
        Some(value) => value.to_string(),
        None => String::new(),
    }
}

fn historical_tool_result_truncation_marker(original_bytes: usize) -> String {
    format!(
        "[tool output truncated: original_bytes={original_bytes}, max_historical_tool_output_bytes={MAX_HISTORICAL_TOOL_OUTPUT_BYTES}]"
    )
}

fn truncated_current_tool_result_text(text: &str) -> String {
    let marker = format!(
        "[tool output truncated: original_bytes={}, max_current_tool_output_bytes={}]\n",
        text.len(),
        MAX_CURRENT_TOOL_OUTPUT_BYTES
    );
    let omitted_marker = "\n[... middle of tool output omitted ...]\n";
    let visible_budget = MAX_CURRENT_TOOL_OUTPUT_BYTES
        .saturating_sub(marker.len())
        .saturating_sub(omitted_marker.len());
    let head_budget = visible_budget * 3 / 4;
    let tail_budget = visible_budget.saturating_sub(head_budget);
    let head_end = floor_char_boundary(text, head_budget);
    let tail_start = ceil_char_boundary(text, text.len().saturating_sub(tail_budget));

    format!(
        "{}{}{}{}",
        marker,
        &text[..head_end],
        omitted_marker,
        &text[tail_start..]
    )
}

fn floor_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while !text.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(text: &str, mut index: usize) -> usize {
    index = index.min(text.len());
    while index < text.len() && !text.is_char_boundary(index) {
        index += 1;
    }
    index
}

fn tool_result_error_text_item(value: &Value) -> Option<String> {
    match value.get("type").and_then(Value::as_str) {
        Some("text") => value
            .get("text")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        Some("image") | Some("input_image") | Some("image_url") => {
            Some("[image omitted from error tool result]".to_string())
        }
        Some("document") | Some("input_file") | Some("file") => {
            Some("[document omitted from error tool result]".to_string())
        }
        Some(_) => Some(value.to_string()),
        None => None,
    }
}

fn tool_result_output(
    content: &Option<Value>,
    is_error: Option<bool>,
    truncate_if_large: bool,
) -> Value {
    let should_truncate_current =
        !truncate_if_large && raw_tool_result_text_len(content) > MAX_CURRENT_TOOL_OUTPUT_BYTES;
    if is_error == Some(true) || truncate_if_large || should_truncate_current {
        return json!(tool_result_text(content, is_error, truncate_if_large));
    }

    match content {
        Some(Value::Array(items)) => {
            if !items
                .iter()
                .all(|item| item.get("type").and_then(Value::as_str).is_some())
            {
                return json!(tool_result_text(content, is_error, truncate_if_large));
            }
            let parts = items
                .iter()
                .map(tool_result_content_part)
                .filter_map(responses_message_part_to_value)
                .collect::<Vec<_>>();
            if parts.is_empty() {
                json!("")
            } else {
                json!(parts)
            }
        }
        _ => json!(tool_result_text(content, is_error, truncate_if_large)),
    }
}

fn tool_result_content_part(value: &Value) -> ResponsesMessagePart {
    content_part_from_value(value).unwrap_or_else(|| ResponsesMessagePart::Text(value.to_string()))
}

fn responses_message_part_to_value(part: ResponsesMessagePart) -> Option<Value> {
    match part {
        ResponsesMessagePart::Text(text) if text.is_empty() => None,
        ResponsesMessagePart::Text(text) => Some(json!({
            "type": "input_text",
            "text": text,
        })),
        ResponsesMessagePart::Input(value) => Some(value),
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

fn convert_reasoning(req: &MessagesRequest) -> Option<Value> {
    if let Some(reasoning) = req.extra.get("reasoning") {
        return Some(reasoning_for_model(req, reasoning.clone()));
    }
    if let Some(effort) = req.extra.get("reasoning_effort").and_then(Value::as_str) {
        return Some(reasoning_effort_for_model(req, effort));
    }
    let thinking = req.thinking.as_ref()?;
    if thinking.r#type.as_deref() == Some("disabled") {
        return Some(json!({"effort": "none"}));
    }
    if let Some(budget_tokens) = thinking.budget_tokens {
        let effort = thinking_budget_to_reasoning_effort(budget_tokens, &req.model);
        return Some(reasoning_effort_for_model(req, effort));
    }
    if thinking.r#type.as_deref() == Some("adaptive") {
        let effort = default_adaptive_reasoning_effort(&req.model);
        return Some(reasoning_effort_for_model(req, effort));
    }
    if thinking.r#type.as_deref() == Some("enabled") {
        return Some(reasoning_effort_for_model(req, "medium"));
    }
    None
}

fn reasoning_effort_for_model(req: &MessagesRequest, effort: &str) -> Value {
    if effort == "none" {
        return json!({"effort": "none"});
    }

    let reasoning = json!({"effort": effort, "summary": "detailed"});
    reasoning_for_model(req, reasoning)
}

fn reasoning_for_model(req: &MessagesRequest, mut reasoning: Value) -> Value {
    if !supports_reasoning_summary(&req.model)
        && let Some(object) = reasoning.as_object_mut()
    {
        object.remove("summary");
    }
    reasoning
}

#[cfg(test)]
pub fn stream_responses_response(
    response: reqwest::Response,
) -> BoxStream<'static, Result<SseEvent, ProviderError>> {
    stream_responses_response_with_observer(response, |_| {})
}

pub fn stream_responses_response_with_marker_mode(
    response: reqwest::Response,
    marker_mode: ReasoningMarkerMode,
) -> BoxStream<'static, Result<SseEvent, ProviderError>> {
    stream_responses_response_with_marker_mode_and_observer(response, marker_mode, |_| {})
}

#[cfg(test)]
pub fn stream_responses_response_with_observer<F>(
    response: reqwest::Response,
    on_event: F,
) -> BoxStream<'static, Result<SseEvent, ProviderError>>
where
    F: Fn(&Value) + Send + Sync + 'static,
{
    stream_responses_response_with_marker_mode_and_observer(
        response,
        ReasoningMarkerMode::Strict,
        on_event,
    )
}

pub fn stream_responses_response_with_marker_mode_and_observer<F>(
    response: reqwest::Response,
    marker_mode: ReasoningMarkerMode,
    on_event: F,
) -> BoxStream<'static, Result<SseEvent, ProviderError>>
where
    F: Fn(&Value) + Send + Sync + 'static,
{
    let (tx, rx) = mpsc::channel::<Result<SseEvent, ProviderError>>(64);
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("unknown")
        .to_string();
    let on_event = Arc::new(on_event);

    tokio::spawn(async move {
        let mut converter = ResponsesStreamConverter::with_marker_mode(marker_mode);
        let mut decoder = SseDecoder::new();
        let mut byte_stream = response.bytes_stream();
        let mut preview = Vec::new();
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
                    append_stream_preview(&mut preview, &chunk);
                    decoder.push(&chunk);
                    while let Some(event) = decoder.next_frame() {
                        if is_sse_done(&event) {
                            saw_done = true;
                            continue;
                        }
                        if let Some(value) = parse_sse_json(&event) {
                            on_event(&value);
                            for event in converter.process_event(&value) {
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
                    if let Some(event) = decoder.finish() {
                        if is_sse_done(&event) {
                            saw_done = true;
                        } else if let Some(value) = parse_sse_json(&event) {
                            on_event(&value);
                            for event in converter.process_event(&value) {
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

        if let Some(event) = decoder.finish()
            && !is_sse_done(&event)
            && let Some(value) = parse_sse_json(&event)
        {
            on_event(&value);
            for event in converter.process_event(&value) {
                if tx.send(Ok(event)).await.is_err() {
                    return;
                }
            }
        }

        if !converter.started {
            let _ = tx
                .send(Err(malformed_responses_stream_error(
                    status,
                    &content_type,
                    &preview,
                )))
                .await;
            return;
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
    parse_sse_json_value(text)
}

fn append_stream_preview(preview: &mut Vec<u8>, chunk: &[u8]) {
    let remaining = MAX_MALFORMED_STREAM_PREVIEW_BYTES.saturating_sub(preview.len());
    if remaining > 0 {
        preview.extend_from_slice(&chunk[..remaining.min(chunk.len())]);
    }
}

fn malformed_responses_stream_error(
    status: u16,
    content_type: &str,
    preview: &[u8],
) -> ProviderError {
    let mut message = format!(
        "empty or malformed Responses API stream from upstream (content-type: {content_type})"
    );
    let preview = String::from_utf8_lossy(preview);
    let preview = preview.trim();
    if preview.is_empty() {
        message.push_str("; response body was empty");
    } else {
        message.push_str("; response preview: ");
        message.push_str(preview);
    }
    message.push_str("; check for a proxy or gateway intercepting the request");

    ProviderError::UpstreamError {
        status,
        body: message,
    }
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
    reasoning_text: ReasoningTextSplitter,
    reasoning_text_key: Option<(u64, u64)>,
    function_blocks: HashMap<u64, u32>,
    function_names: HashMap<u64, String>,
    function_call_ids: HashMap<u64, String>,
    function_argument_buffers: HashMap<u64, String>,
    function_argument_emitted: HashMap<u64, String>,
    custom_tool_input_open: HashSet<u64>,
    custom_tool_input_emitted: HashSet<u64>,
    saw_function_call: bool,
    input_tokens: u32,
    output_tokens: u32,
    stopped: bool,
}

impl ResponsesStreamConverter {
    #[cfg(test)]
    fn new() -> Self {
        Self::with_marker_mode(ReasoningMarkerMode::Strict)
    }

    fn with_marker_mode(marker_mode: ReasoningMarkerMode) -> Self {
        Self {
            message_id: format!("msg_{}", uuid::Uuid::new_v4().to_string().replace('-', "")),
            reasoning_text: ReasoningTextSplitter::new(marker_mode),
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
                    self.emit_text_stream(output_index, content_index, delta, &mut events);
                }
            }
            "response.refusal.delta" => {
                self.ensure_started(event.get("response").unwrap_or(event), &mut events);
                let output_index = event["output_index"].as_u64().unwrap_or(0);
                let content_index = event["content_index"].as_u64().unwrap_or(0);
                let delta = event["delta"].as_str().unwrap_or_default();
                if !delta.is_empty() {
                    self.emit_text_stream(output_index, content_index, delta, &mut events);
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                self.ensure_started(event.get("response").unwrap_or(event), &mut events);
                self.flush_reasoning_text(&mut events);
                let delta = event["delta"].as_str().unwrap_or_default();
                if !delta.is_empty() {
                    let idx = self.ensure_thinking_block(&mut events);
                    events.push(content_delta(idx, "thinking_delta", "thinking", delta));
                }
            }
            "response.function_call_arguments.delta" => {
                self.ensure_started(event.get("response").unwrap_or(event), &mut events);
                self.flush_reasoning_text(&mut events);
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
                self.flush_reasoning_text(&mut events);
                self.handle_function_call_arguments_done(event, &mut events);
            }
            "response.custom_tool_call_input.delta" => {
                self.ensure_started(event.get("response").unwrap_or(event), &mut events);
                self.flush_reasoning_text(&mut events);
                self.handle_custom_tool_call_input_delta(event, &mut events);
            }
            "response.custom_tool_call_input.done" => {
                self.ensure_started(event.get("response").unwrap_or(event), &mut events);
                self.flush_reasoning_text(&mut events);
                self.handle_custom_tool_call_input_done(event, &mut events);
            }
            "response.output_item.done" => {
                self.flush_reasoning_text(&mut events);
                self.handle_output_item_done(event, &mut events);
            }
            "response.completed" | "response.incomplete" | "response.failed" => {
                if let Some(response) = event.get("response") {
                    self.ensure_started(response, &mut events);
                    self.set_usage(response);
                    self.flush_reasoning_text(&mut events);
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
            self.flush_reasoning_text(&mut events);
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
        if is_responses_tool_call_type(item["type"].as_str()) {
            self.flush_reasoning_text(events);
            self.saw_function_call = true;
            self.remember_tool_call_metadata(output_index, event);
            self.ensure_function_block(output_index, event, events);
        }
    }

    fn handle_content_part_added(&mut self, event: &Value, events: &mut Vec<SseEvent>) {
        let output_index = event["output_index"].as_u64().unwrap_or(0);
        let content_index = event["content_index"].as_u64().unwrap_or(0);
        let part = &event["part"];
        match part["type"].as_str() {
            Some("output_text") => {
                if let Some(text) = part["text"].as_str()
                    && !text.is_empty()
                {
                    self.emit_text_stream(output_index, content_index, text, events);
                }
            }
            Some("refusal") => {
                if let Some(text) = part["refusal"].as_str()
                    && !text.is_empty()
                {
                    self.emit_text_stream(output_index, content_index, text, events);
                }
            }
            _ => {}
        }
    }

    fn emit_text_delta(
        &mut self,
        output_index: u64,
        content_index: u64,
        text: &str,
        events: &mut Vec<SseEvent>,
    ) {
        if text.is_empty() {
            return;
        }

        let idx = self.ensure_text_block(output_index, content_index, events);
        events.push(content_delta(idx, "text_delta", "text", text));
    }

    fn handle_output_item_done(&mut self, event: &Value, events: &mut Vec<SseEvent>) {
        let output_index = event["output_index"].as_u64().unwrap_or(0);
        let item = &event["item"];
        match item["type"].as_str() {
            Some("function_call") => {
                self.saw_function_call = true;
                self.remember_tool_call_metadata(output_index, event);
                let idx = self.ensure_function_block(output_index, event, events);
                let arguments = item["arguments"].as_str();
                self.emit_function_arguments(output_index, idx, arguments, false, events);
                self.close_function_block(output_index, idx, events);
            }
            Some("custom_tool_call") => {
                self.saw_function_call = true;
                self.remember_tool_call_metadata(output_index, event);
                let idx = self.ensure_function_block(output_index, event, events);
                self.emit_custom_tool_input_done(output_index, idx, item["input"].as_str(), events);
                self.close_function_block(output_index, idx, events);
            }
            _ => {}
        }
    }

    fn handle_function_call_arguments_done(&mut self, event: &Value, events: &mut Vec<SseEvent>) {
        let output_index = event["output_index"].as_u64().unwrap_or(0);
        self.saw_function_call = true;
        self.remember_tool_call_metadata(output_index, event);

        let idx = self.ensure_function_block(output_index, event, events);
        self.emit_function_arguments(
            output_index,
            idx,
            event["arguments"].as_str(),
            false,
            events,
        );
    }

    fn handle_custom_tool_call_input_delta(&mut self, event: &Value, events: &mut Vec<SseEvent>) {
        let output_index = event["output_index"].as_u64().unwrap_or(0);
        let delta = event["delta"].as_str().unwrap_or_default();
        if delta.is_empty() {
            return;
        }

        self.saw_function_call = true;
        self.remember_tool_call_metadata(output_index, event);
        let idx = self.ensure_function_block(output_index, event, events);
        self.emit_custom_tool_input_delta(output_index, idx, delta, events);
    }

    fn handle_custom_tool_call_input_done(&mut self, event: &Value, events: &mut Vec<SseEvent>) {
        let output_index = event["output_index"].as_u64().unwrap_or(0);
        self.saw_function_call = true;
        self.remember_tool_call_metadata(output_index, event);
        let idx = self.ensure_function_block(output_index, event, events);
        self.emit_custom_tool_input_done(output_index, idx, event["input"].as_str(), events);
    }

    fn remember_tool_call_metadata(&mut self, output_index: u64, event: &Value) {
        let item = &event["item"];
        if let Some(name) = item["name"].as_str().or_else(|| event["name"].as_str()) {
            self.function_names.insert(output_index, name.to_string());
        }
        if let Some(call_id) = item["call_id"]
            .as_str()
            .or_else(|| event["call_id"].as_str())
            .or_else(|| item["id"].as_str())
            .or_else(|| event["item_id"].as_str())
        {
            self.function_call_ids
                .insert(output_index, call_id.to_string());
        }
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

    fn emit_custom_tool_input_delta(
        &mut self,
        output_index: u64,
        idx: u32,
        delta: &str,
        events: &mut Vec<SseEvent>,
    ) {
        if self.custom_tool_input_open.insert(output_index) {
            Self::push_function_arguments_delta(idx, "{\"input\":\"", events);
        }
        self.custom_tool_input_emitted.insert(output_index);
        Self::push_function_arguments_delta(idx, &json_string_fragment(delta), events);
    }

    fn emit_custom_tool_input_done(
        &mut self,
        output_index: u64,
        idx: u32,
        input: Option<&str>,
        events: &mut Vec<SseEvent>,
    ) {
        if self.close_custom_tool_input_if_open(output_index, idx, events) {
            return;
        }
        if self.custom_tool_input_emitted.contains(&output_index) {
            return;
        }
        let Some(input) = input else {
            return;
        };
        let encoded = serde_json::to_string(input).unwrap_or_else(|_| "\"\"".to_string());
        Self::push_function_arguments_delta(idx, &format!("{{\"input\":{encoded}}}"), events);
        self.custom_tool_input_emitted.insert(output_index);
    }

    fn close_custom_tool_input_if_open(
        &mut self,
        output_index: u64,
        idx: u32,
        events: &mut Vec<SseEvent>,
    ) -> bool {
        if !self.custom_tool_input_open.remove(&output_index) {
            return false;
        }
        Self::push_function_arguments_delta(idx, "\"}", events);
        self.custom_tool_input_emitted.insert(output_index);
        true
    }

    fn emit_function_arguments(
        &mut self,
        output_index: u64,
        idx: u32,
        final_arguments: Option<&str>,
        require_valid_json: bool,
        events: &mut Vec<SseEvent>,
    ) {
        let Some(arguments) = final_arguments
            .filter(|arguments| !arguments.is_empty())
            .or_else(|| {
                self.function_argument_buffers
                    .get(&output_index)
                    .map(String::as_str)
            })
        else {
            return;
        };
        if arguments.is_empty() {
            return;
        }

        if require_valid_json && serde_json::from_str::<Value>(arguments).is_err() {
            return;
        }

        let sanitized = self
            .function_names
            .get(&output_index)
            .and_then(|name| sanitize_tool_arguments(name, arguments))
            .map(Cow::Owned)
            .unwrap_or(Cow::Borrowed(arguments));
        let previous = self
            .function_argument_emitted
            .get(&output_index)
            .map(String::as_str)
            .unwrap_or("");
        if sanitized.as_ref() == previous {
            return;
        }
        if let Some(delta) = sanitized.strip_prefix(previous) {
            Self::push_function_arguments_delta(idx, delta, events);
            self.function_argument_emitted
                .insert(output_index, sanitized.into_owned());
        } else if previous.is_empty() {
            Self::push_function_arguments_delta(idx, sanitized.as_ref(), events);
            self.function_argument_emitted
                .insert(output_index, sanitized.into_owned());
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

    fn emit_text_stream(
        &mut self,
        output_index: u64,
        content_index: u64,
        text: &str,
        events: &mut Vec<SseEvent>,
    ) {
        let key = (output_index, content_index);
        if self.reasoning_text_key != Some(key) {
            self.flush_reasoning_text(events);
            self.reasoning_text_key = Some(key);
        }
        for segment in self.reasoning_text.push(text) {
            self.emit_text_segment(key, segment, events);
        }
    }

    fn flush_reasoning_text(&mut self, events: &mut Vec<SseEvent>) {
        let key = self.reasoning_text_key.take();
        for segment in self.reasoning_text.finish() {
            if let Some(key) = key {
                self.emit_text_segment(key, segment, events);
            } else if let TextSegment::Reasoning(thinking) = segment {
                self.emit_thinking_content(&thinking, events);
            }
        }
    }

    fn emit_text_segment(
        &mut self,
        (output_index, content_index): (u64, u64),
        segment: TextSegment,
        events: &mut Vec<SseEvent>,
    ) {
        match segment {
            TextSegment::Text(text) => {
                self.emit_text_delta(output_index, content_index, &text, events);
            }
            TextSegment::Reasoning(thinking) => {
                self.emit_thinking_content(&thinking, events);
            }
        }
    }

    fn emit_thinking_content(&mut self, thinking: &str, events: &mut Vec<SseEvent>) {
        if thinking.is_empty() {
            return;
        }
        let idx = self.ensure_thinking_block(events);
        events.push(content_delta(idx, "thinking_delta", "thinking", thinking));
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

        self.flush_reasoning_text(events);
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
                OpenBlock::Text(idx) => {
                    self.output_blocks.retain(|_, block_idx| *block_idx != idx);
                    idx
                }
                OpenBlock::Thinking(idx) => idx,
            };
            events.push(block_stop(idx));
        }
    }

    fn stop_response(&mut self, response: &Value, events: &mut Vec<SseEvent>) {
        if self.stopped {
            return;
        }
        self.flush_reasoning_text(events);
        self.close_open_block(events);
        self.close_function_blocks(events);
        let reason = response_stop_reason(response, self.saw_function_call);
        self.stop_with_reason(reason, events);
    }

    fn close_function_blocks(&mut self, events: &mut Vec<SseEvent>) {
        let blocks = std::mem::take(&mut self.function_blocks);
        for (output_index, idx) in blocks {
            self.close_custom_tool_input_if_open(output_index, idx, events);
            self.emit_function_arguments(output_index, idx, None, false, events);
            events.push(block_stop(idx));
        }
        self.function_argument_buffers.clear();
        self.function_argument_emitted.clear();
        self.custom_tool_input_open.clear();
        self.custom_tool_input_emitted.clear();
    }

    fn close_function_block(&mut self, output_index: u64, idx: u32, events: &mut Vec<SseEvent>) {
        self.close_custom_tool_input_if_open(output_index, idx, events);
        events.push(block_stop(idx));
        self.function_blocks.remove(&output_index);
        self.function_argument_buffers.remove(&output_index);
        self.function_argument_emitted.remove(&output_index);
        self.custom_tool_input_open.remove(&output_index);
        self.custom_tool_input_emitted.remove(&output_index);
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
                .any(|item| is_responses_tool_call_type(item["type"].as_str()))
        })
    {
        "tool_use"
    } else {
        "end_turn"
    }
}

fn is_responses_tool_call_type(item_type: Option<&str>) -> bool {
    matches!(item_type, Some("function_call") | Some("custom_tool_call"))
}

fn json_string_fragment(text: &str) -> String {
    let encoded = serde_json::to_string(text).unwrap_or_else(|_| "\"\"".to_string());
    encoded
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .unwrap_or_default()
        .to_string()
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

#[cfg(test)]
pub fn convert_non_streaming_response(data: &Value) -> Vec<SseEvent> {
    convert_non_streaming_response_with_marker_mode(data, ReasoningMarkerMode::Strict)
}

pub fn convert_non_streaming_response_with_marker_mode(
    data: &Value,
    marker_mode: ReasoningMarkerMode,
) -> Vec<SseEvent> {
    let mut converter = NonStreamingResponsesConverter::new(data, marker_mode);
    converter.convert()
}

struct NonStreamingResponsesConverter<'a> {
    data: &'a Value,
    events: Vec<SseEvent>,
    next_block_index: u32,
    input_tokens: u32,
    output_tokens: u32,
    marker_mode: ReasoningMarkerMode,
}

impl<'a> NonStreamingResponsesConverter<'a> {
    fn new(data: &'a Value, marker_mode: ReasoningMarkerMode) -> Self {
        let usage = &data["usage"];
        Self {
            data,
            events: Vec::new(),
            next_block_index: 0,
            input_tokens: usage["input_tokens"].as_u64().unwrap_or(0) as u32,
            output_tokens: usage["output_tokens"].as_u64().unwrap_or(0) as u32,
            marker_mode,
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
                    Some("custom_tool_call") => self.convert_custom_tool_call(item),
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
                    .and_then(|name| sanitize_tool_arguments(name, args))
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

    fn convert_custom_tool_call(&mut self, item: &Value) {
        let idx = self.next_block_index;
        self.next_block_index += 1;
        let input = item["input"]
            .as_str()
            .map(|input| json!({ "input": input }))
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
        for segment in split_text(text, self.marker_mode) {
            match segment {
                TextSegment::Text(text) => self.add_plain_text_block(&text),
                TextSegment::Reasoning(thinking) => self.add_thinking_block(&thinking),
            }
        }
    }

    fn add_plain_text_block(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }

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
    use std::path::PathBuf;

    use super::*;
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

    fn thinking_deltas(events: &[SseEvent]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| {
                (event.event == "content_block_delta"
                    && event.data["delta"]["type"] == "thinking_delta")
                    .then(|| event.data["delta"]["thinking"].as_str())
                    .flatten()
                    .map(ToOwned::to_owned)
            })
            .collect()
    }

    async fn response_from_body(content_type: &str, body: &str) -> reqwest::Response {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let content_type = content_type.to_string();
        let body = body.to_string();

        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
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

    #[test]
    fn test_parse_sse_json_accepts_data_without_space() {
        let value = parse_sse_json(r#"data:{"type":"response.output_text.delta"}"#).unwrap();
        assert_eq!(value["type"], "response.output_text.delta");
    }

    #[tokio::test]
    async fn test_stream_response_reports_malformed_http_200_body() {
        let response =
            response_from_body("text/html", "<html><title>login required</title></html>").await;
        let mut stream = stream_responses_response(response);

        let error = stream
            .next()
            .await
            .expect("malformed stream error")
            .expect_err("malformed HTTP 200 body should not finish empty");

        match error {
            ProviderError::UpstreamError { status, body } => {
                assert_eq!(status, 200);
                assert!(body.contains("empty or malformed Responses API stream"));
                assert!(body.contains("content-type: text/html"));
                assert!(body.contains("login required"));
                assert!(body.contains("check for a proxy or gateway"));
            }
            other => panic!("unexpected error: {other}"),
        }
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn test_stream_response_ignores_trailing_chunk_eof_after_completed_event() {
        let body = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        );
        let response = response_from_unterminated_chunked_body("text/event-stream", body).await;
        let mut stream = stream_responses_response(response);
        let mut events = Vec::new();

        while let Some(item) = stream.next().await {
            events.push(item.expect("completed stream should ignore trailing chunk EOF"));
        }

        assert!(events.iter().any(|event| event.event == "message_stop"));
        assert!(
            events
                .iter()
                .any(|event| event.data["delta"]["stop_reason"] == "end_turn")
        );
    }

    #[tokio::test]
    async fn test_stream_response_ignores_chunk_eof_after_undelimited_completed_event() {
        let body = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"hello\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}",
        );
        let response = response_from_unterminated_chunked_body("text/event-stream", body).await;
        let mut stream = stream_responses_response(response);
        let mut events = Vec::new();

        while let Some(item) = stream.next().await {
            events.push(item.expect("terminal frame should be flushed before chunk EOF"));
        }

        assert!(events.iter().any(|event| event.event == "message_stop"));
        assert!(
            events
                .iter()
                .any(|event| event.data["delta"]["stop_reason"] == "end_turn")
        );
    }

    #[tokio::test]
    async fn test_stream_response_ignores_chunk_eof_after_done_marker() {
        let body = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":0,\"content_index\":0,\"delta\":\"hello\"}\n\n",
            "data: [DONE]\n\n",
        );
        let response = response_from_unterminated_chunked_body("text/event-stream", body).await;
        let mut stream = stream_responses_response(response);
        let mut events = Vec::new();

        while let Some(item) = stream.next().await {
            events.push(item.expect("done marker should make chunk EOF terminal"));
        }

        assert!(events.iter().any(|event| event.event == "message_stop"));
    }

    #[tokio::test]
    async fn test_stream_response_observer_sees_raw_events() {
        let body = concat!(
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"codex.rate_limits\",\"plan_type\":\"plus\",\"rate_limits\":{\"primary\":{\"used_percent\":55}}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n",
        );
        let response = response_from_body("text/event-stream", body).await;
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let observed_for_hook = std::sync::Arc::clone(&observed);
        let mut stream = stream_responses_response_with_observer(response, move |event| {
            observed_for_hook
                .lock()
                .unwrap()
                .push(event["type"].clone());
        });

        while let Some(item) = stream.next().await {
            item.expect("stream should convert regular Responses events");
        }

        let observed = observed.lock().unwrap();
        assert!(observed.iter().any(|kind| kind == "codex.rate_limits"));
    }

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
    fn test_convert_to_responses_omits_sampling_for_reasoning_requests() {
        let req = MessagesRequest {
            model: "gpt-5.5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("Hello".to_string()),
            }],
            max_tokens: None,
            temperature: Some(0.2),
            top_p: Some(0.9),
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

        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
    }

    #[test]
    fn test_convert_to_responses_keeps_sampling_for_non_reasoning_requests() {
        let req = MessagesRequest {
            model: "gpt-4.1".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("Hello".to_string()),
            }],
            max_tokens: None,
            temperature: Some(0.2),
            top_p: Some(0.9),
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

        assert!((body["temperature"].as_f64().unwrap() - 0.2).abs() < 1e-6);
        assert!((body["top_p"].as_f64().unwrap() - 0.9).abs() < 1e-6);
    }

    #[test]
    fn test_convert_to_responses_maps_anthropic_image_blocks() {
        let req = MessagesRequest {
            model: "gpt-5.3-codex".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![
                    Content::Text {
                        text: "What is in this image?".to_string(),
                    },
                    Content::Unknown(json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": "iVBORw0KGgo="
                        }
                    })),
                ]),
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

        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(
            body["input"][0]["content"][0]["text"],
            "What is in this image?"
        );
        assert_eq!(body["input"][0]["content"][1]["type"], "input_image");
        assert_eq!(
            body["input"][0]["content"][1]["image_url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
    }

    #[test]
    fn test_convert_to_responses_accepts_camel_case_image_media_type() {
        let req = MessagesRequest {
            model: "gpt-5.3-codex".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![Content::Unknown(json!({
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "mediaType": "image/jpeg",
                        "data": "/9j/4AAQSkZJRg=="
                    }
                }))]),
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

        assert_eq!(body["input"][0]["content"][0]["type"], "input_image");
        assert_eq!(
            body["input"][0]["content"][0]["image_url"],
            "data:image/jpeg;base64,/9j/4AAQSkZJRg=="
        );
    }

    #[test]
    fn test_convert_to_responses_maps_anthropic_document_blocks() {
        let req = MessagesRequest {
            model: "gpt-5.3-codex".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![
                    Content::Text {
                        text: "Summarize this PDF.".to_string(),
                    },
                    Content::Unknown(json!({
                        "type": "document",
                        "title": "report.pdf",
                        "source": {
                            "type": "base64",
                            "media_type": "application/pdf",
                            "data": "JVBERi0x"
                        }
                    })),
                ]),
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

        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][1]["type"], "input_file");
        assert_eq!(body["input"][0]["content"][1]["file_data"], "JVBERi0x");
        assert_eq!(body["input"][0]["content"][1]["filename"], "report.pdf");
    }

    #[test]
    fn test_convert_to_responses_maps_tool_result_image_content() {
        let req = MessagesRequest {
            model: "gpt-5.3-codex".to_string(),
            system: None,
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: MessageContent::Blocks(vec![Content::ToolUse {
                        id: "call_1".to_string(),
                        name: "screenshot".to_string(),
                        input: json!({}),
                    }]),
                },
                Message {
                    role: Role::User,
                    content: MessageContent::Blocks(vec![Content::ToolResult {
                        tool_use_id: "call_1".to_string(),
                        content: Some(json!([
                            {"type": "text", "text": "Captured screenshot."},
                            {
                                "type": "image",
                                "source": {
                                    "type": "base64",
                                    "media_type": "image/png",
                                    "data": "iVBORw0KGgo="
                                }
                            }
                        ])),
                        is_error: None,
                    }]),
                },
            ],
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

        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][1]["output"][0]["type"], "input_text");
        assert_eq!(
            body["input"][1]["output"][0]["text"],
            "Captured screenshot."
        );
        assert_eq!(body["input"][1]["output"][1]["type"], "input_image");
        assert_eq!(
            body["input"][1]["output"][1]["image_url"],
            "data:image/png;base64,iVBORw0KGgo="
        );
    }

    #[test]
    fn test_convert_to_responses_preserves_json_array_tool_result_as_text() {
        let req = MessagesRequest {
            model: "gpt-5.3-codex".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![Content::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    content: Some(json!([{"path": "README.md"}])),
                    is_error: None,
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

        assert_eq!(body["input"][0]["output"], r#"[{"path":"README.md"}]"#);
    }

    #[test]
    fn test_convert_to_responses_keeps_error_tool_result_text_only() {
        let req = MessagesRequest {
            model: "gpt-5.3-codex".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![Content::ToolResult {
                    tool_use_id: "call_1".to_string(),
                    content: Some(json!([
                        {"type": "text", "text": "Permission denied."},
                        {
                            "type": "image",
                            "source": {
                                "type": "base64",
                                "media_type": "image/png",
                                "data": "iVBORw0KGgo="
                            }
                        }
                    ])),
                    is_error: Some(true),
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

        assert_eq!(
            body["input"][0]["output"],
            "ERROR: Permission denied.\n[image omitted from error tool result]"
        );
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
    fn test_convert_to_responses_normalizes_named_tool_choice() {
        let req = MessagesRequest {
            model: "gpt-5".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Text("Search docs".to_string()),
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: None,
            tool_choice: Some(json!({"type": "tool", "name": "WebSearch"})),
            thinking: None,
            metadata: None,
            extra: HashMap::new(),
        };

        let body = convert_to_responses(&req);

        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "name": "WebSearch"})
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
    fn test_convert_to_responses_omits_reasoning_summary_for_codex_spark() {
        let req = MessagesRequest {
            model: "gpt-5.3-codex-spark".to_string(),
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
            thinking: Some(ThinkingConfig {
                r#type: Some("enabled".to_string()),
                budget_tokens: None,
            }),
            metadata: None,
            extra: HashMap::new(),
        };

        let body = convert_to_responses(&req);

        assert_eq!(body["reasoning"], json!({"effort": "medium"}));
    }

    #[test]
    fn test_convert_to_responses_maps_thinking_budget_to_reasoning_effort() {
        for (budget_tokens, expected_effort) in [
            (2048, "low"),
            (8192, "medium"),
            (16_384, "high"),
            (16_385, "xhigh"),
        ] {
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
                thinking: Some(ThinkingConfig {
                    r#type: Some("enabled".to_string()),
                    budget_tokens: Some(budget_tokens),
                }),
                metadata: None,
                extra: HashMap::new(),
            };

            let body = convert_to_responses(&req);

            assert_eq!(body["reasoning"]["effort"], expected_effort);
            assert_eq!(body["reasoning"]["summary"], "detailed");
        }
    }

    #[test]
    fn test_convert_to_responses_downgrades_xhigh_budget_without_model_support() {
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
            thinking: Some(ThinkingConfig {
                r#type: Some("enabled".to_string()),
                budget_tokens: Some(31_999),
            }),
            metadata: None,
            extra: HashMap::new(),
        };

        let body = convert_to_responses(&req);

        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "detailed");
    }

    #[test]
    fn test_convert_to_responses_maps_adaptive_thinking_without_budget_to_high_reasoning_effort() {
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
            thinking: Some(ThinkingConfig {
                r#type: Some("adaptive".to_string()),
                budget_tokens: None,
            }),
            metadata: None,
            extra: HashMap::new(),
        };

        let body = convert_to_responses(&req);

        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["reasoning"]["summary"], "detailed");
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

        assert!(
            input[0]["output"]
                .as_str()
                .unwrap()
                .starts_with("ERROR: [tool output truncated:")
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
    fn test_convert_to_responses_truncates_current_oversized_tool_output_with_head_and_tail() {
        let large_output = format!(
            "{}TAIL_SENTINEL",
            "x".repeat(MAX_CURRENT_TOOL_OUTPUT_BYTES * 2)
        );
        let req = MessagesRequest {
            model: "gpt-5.4-mini".to_string(),
            system: None,
            messages: vec![Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![Content::ToolResult {
                    tool_use_id: "current_call".to_string(),
                    content: Some(Value::String(large_output)),
                    is_error: None,
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
        let output = body["input"][0]["output"]
            .as_str()
            .expect("tool output string");

        assert!(output.starts_with("[tool output truncated:"));
        assert!(output.contains("max_current_tool_output_bytes="));
        assert!(output.contains("[... middle of tool output omitted ...]"));
        assert!(output.ends_with("TAIL_SENTINEL"));
        assert!(output.len() <= MAX_CURRENT_TOOL_OUTPUT_BYTES);
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
    fn test_convert_to_responses_omits_history_thinking_text_items() {
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
        let serialized = serde_json::to_string(input).expect("input json");

        assert!(!serialized.contains("[thinking]"));
        assert!(!serialized.contains(&large_text));
        assert_eq!(input.last().unwrap()["content"], "current");
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
    fn test_stream_converter_maps_tagged_output_text_to_thinking() {
        let mut converter =
            ResponsesStreamConverter::with_marker_mode(ReasoningMarkerMode::LegacyTags);
        let mut events = Vec::new();

        events.extend(converter.process_event(&json!({
            "type": "response.created",
            "response": {"id": "resp_1", "model": "gpt-5", "usage": null}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "content_index": 0,
            "delta": "hello [thinking]plan[/thinking] world"
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "model": "gpt-5",
                "status": "completed",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }
        })));

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
    fn test_stream_converter_preserves_tagged_output_text_by_default() {
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
            "delta": "use `<analysis>...</analysis>` here"
        })));

        assert_eq!(
            text_deltas(&events),
            vec!["use `<analysis>...</analysis>` here"]
        );
        assert!(
            text_and_thinking_deltas(&events)
                .iter()
                .all(|(kind, _)| kind == "text_delta")
        );
    }

    #[test]
    fn test_stream_converter_maps_split_tagged_output_text_to_thinking() {
        let mut converter =
            ResponsesStreamConverter::with_marker_mode(ReasoningMarkerMode::LegacyTags);
        let mut events = Vec::new();

        events.extend(converter.process_event(&json!({
            "type": "response.created",
            "response": {"id": "resp_1", "model": "gpt-5", "usage": null}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "content_index": 0,
            "delta": "hello [thin"
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "content_index": 0,
            "delta": "king]plan[/thin"
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "content_index": 0,
            "delta": "king] world"
        })));

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
    fn test_stream_converter_drops_unclosed_thinking_marker_in_output_text() {
        let mut converter =
            ResponsesStreamConverter::with_marker_mode(ReasoningMarkerMode::LegacyTags);
        let mut events = Vec::new();

        events.extend(converter.process_event(&json!({
            "type": "response.created",
            "response": {"id": "resp_1", "model": "gpt-5", "usage": null}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.output_text.delta",
            "output_index": 0,
            "content_index": 0,
            "delta": "visible [thinking]secret"
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.completed",
            "response": {
                "id": "resp_1",
                "model": "gpt-5",
                "status": "completed",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }
        })));

        assert_eq!(text_deltas(&events), vec!["visible "]);
        assert!(!text_deltas(&events).join("").contains("secret"));
        assert!(thinking_deltas(&events).is_empty());
    }

    #[test]
    fn test_stream_converter_preserves_typed_reasoning_delta() {
        let mut converter = ResponsesStreamConverter::new();
        let mut events = Vec::new();

        events.extend(converter.process_event(&json!({
            "type": "response.created",
            "response": {"id": "resp_1", "model": "gpt-5", "usage": null}
        })));
        events.extend(converter.process_event(&json!({
            "type": "response.reasoning_text.delta",
            "delta": "typed thought"
        })));

        assert_eq!(
            text_and_thinking_deltas(&events),
            vec![("thinking_delta".to_string(), "typed thought".to_string())]
        );
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
    fn test_stream_converter_maps_custom_tool_call_input_delta() {
        let mut converter = ResponsesStreamConverter::new();
        let fixture = [
            json!({
                "type": "response.created",
                "response": {"id": "resp_custom_1", "model": "gpt-5.3-codex", "usage": null}
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "type": "custom_tool_call",
                    "id": "ctc_1",
                    "call_id": "call_custom_1",
                    "name": "python"
                }
            }),
            json!({
                "type": "response.custom_tool_call_input.delta",
                "output_index": 0,
                "item_id": "ctc_1",
                "delta": "print(\""
            }),
            json!({
                "type": "response.custom_tool_call_input.delta",
                "output_index": 0,
                "item_id": "ctc_1",
                "delta": "hi\")\n"
            }),
            json!({
                "type": "response.custom_tool_call_input.done",
                "output_index": 0,
                "item_id": "ctc_1",
                "input": "print(\"hi\")\n"
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_custom_1",
                    "model": "gpt-5.3-codex",
                    "status": "completed",
                    "output": [{"type": "custom_tool_call"}],
                    "usage": {"input_tokens": 10, "output_tokens": 5}
                }
            }),
        ];

        let events = fixture
            .iter()
            .flat_map(|event| converter.process_event(event))
            .collect::<Vec<_>>();
        let tool_block = events
            .iter()
            .find(|event| {
                event.event == "content_block_start"
                    && event.data["content_block"]["type"] == "tool_use"
            })
            .expect("tool block");
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

        assert_eq!(tool_block.data["content_block"]["id"], "call_custom_1");
        assert_eq!(tool_block.data["content_block"]["name"], "python");
        assert_eq!(
            serde_json::from_str::<Value>(&arguments).expect("valid arguments"),
            json!({"input": "print(\"hi\")\n"})
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

        let events =
            convert_non_streaming_response_with_marker_mode(&data, ReasoningMarkerMode::LegacyTags);

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
    fn test_non_streaming_response_maps_custom_tool_call() {
        let data = json!({
            "id": "resp_1",
            "model": "gpt-5.3-codex",
            "status": "completed",
            "output": [
                {
                    "type": "custom_tool_call",
                    "call_id": "call_custom_1",
                    "name": "python",
                    "input": "print(\"hi\")\n"
                }
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });

        let events = convert_non_streaming_response(&data);
        let tool_block = events
            .iter()
            .find(|event| {
                event.event == "content_block_start"
                    && event.data["content_block"]["type"] == "tool_use"
            })
            .expect("tool block");
        let stop = events
            .iter()
            .find(|event| event.event == "message_delta")
            .expect("message_delta");

        assert_eq!(tool_block.data["content_block"]["id"], "call_custom_1");
        assert_eq!(tool_block.data["content_block"]["name"], "python");
        assert_eq!(
            tool_block.data["content_block"]["input"],
            json!({"input": "print(\"hi\")\n"})
        );
        assert_eq!(stop.data["delta"]["stop_reason"], "tool_use");
    }

    #[test]
    fn test_non_streaming_response_maps_tagged_output_text_to_thinking() {
        let data = json!({
            "id": "resp_1",
            "model": "gpt-5",
            "status": "completed",
            "output": [
                {
                    "type": "message",
                    "content": [{"type": "output_text", "text": "hello [thinking]plan[/thinking] world"}]
                }
            ],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });

        let events =
            convert_non_streaming_response_with_marker_mode(&data, ReasoningMarkerMode::LegacyTags);

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
    fn test_non_streaming_response_drops_unclosed_thinking_marker_in_output_text() {
        let data = json!({
            "id": "resp_1",
            "model": "gpt-5",
            "status": "completed",
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "visible [thinking]secret"}]
            }],
            "usage": {"input_tokens": 10, "output_tokens": 5}
        });

        let events =
            convert_non_streaming_response_with_marker_mode(&data, ReasoningMarkerMode::LegacyTags);

        assert_eq!(text_deltas(&events), vec!["visible "]);
        assert!(!text_deltas(&events).join("").contains("secret"));
        assert!(thinking_deltas(&events).is_empty());
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
    fn test_read_sanitizer_recovers_concatenated_large_offset() {
        let path = temp_read_fixture(1_113);
        let sanitized = sanitize_tool_arguments(
            "Read",
            &json!({
                "file_path": path.to_string_lossy(),
                "offset": 5_206_854_u64,
                "limit": 5
            })
            .to_string(),
        )
        .expect("sanitized read arguments");
        let input: Value = serde_json::from_str(&sanitized).expect("valid json");

        assert_eq!(input["offset"], 520);
        assert_eq!(input["limit"], 5);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn test_read_sanitizer_removes_absurd_offset_when_file_is_unavailable() {
        let sanitized = sanitize_tool_arguments(
            "Read",
            &json!({
                "file_path": "missing-routes.rs",
                "offset": 5_206_854_u64,
                "limit": 5
            })
            .to_string(),
        )
        .expect("sanitized read arguments");
        let input: Value = serde_json::from_str(&sanitized).expect("valid json");

        assert!(input.get("offset").is_none());
        assert_eq!(input["limit"], 5);
    }

    #[test]
    fn test_stream_converter_sanitizes_read_large_offset_before_tool_use() {
        let path = temp_read_fixture(1_113);
        let mut converter = ResponsesStreamConverter::new();
        let mut events = Vec::new();
        let arguments = json!({
            "file_path": path.to_string_lossy(),
            "offset": 5_206_854_u64,
            "limit": 5
        })
        .to_string();

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
            "arguments": arguments
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
        let input: Value = serde_json::from_str(arguments).expect("valid arguments");
        assert_eq!(input["offset"], 520);
        assert_eq!(input["limit"], 5);
        let _ = std::fs::remove_file(path);
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
            sanitize_tool_arguments("Other", "{\"pages\":\"\",\"value\":\"\"}"),
            None
        );
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

    fn temp_read_fixture(lines: usize) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "claude-proxy-read-fixture-{}-{}.txt",
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
