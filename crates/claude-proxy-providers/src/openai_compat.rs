use claude_proxy_core::*;
use serde_json::{Value, json};
use tracing::{Level, debug, enabled};

const REASONING_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];

fn intent(req: &MessagesRequest) -> Option<&str> {
    req.metadata
        .as_ref()
        .and_then(|metadata| metadata.get("intent"))
        .and_then(Value::as_str)
}

#[derive(Debug, PartialEq, Eq)]
pub struct OpenAiRequestLogInfo {
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub reasoning_source: &'static str,
    pub thinking_type: Option<String>,
    pub thinking_budget_tokens: Option<u32>,
}

pub fn openai_request_log_info(request: &MessagesRequest) -> OpenAiRequestLogInfo {
    let original_intent = intent(request).map(str::to_string);
    let original_reasoning_source = explicit_reasoning_source(request);
    let request = apply_openai_intent(request.clone());
    let reasoning_effort = request_reasoning_effort(&request);
    let reasoning_source = original_reasoning_source
        .or_else(|| thinking_reasoning_source(&request))
        .or_else(|| {
            original_intent.as_deref().and_then(|intent| {
                reasoning_effort
                    .is_some()
                    .then_some(intent_reasoning_source(intent))
                    .flatten()
            })
        })
        .unwrap_or("unspecified");

    OpenAiRequestLogInfo {
        model: request.model,
        reasoning_effort,
        reasoning_source,
        thinking_type: request
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.r#type.clone()),
        thinking_budget_tokens: request
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.budget_tokens),
    }
}

fn explicit_reasoning_source(request: &MessagesRequest) -> Option<&'static str> {
    if request.extra.contains_key("reasoning") {
        Some("explicit:reasoning")
    } else if request.extra.contains_key("reasoning_effort") {
        Some("explicit:reasoning_effort")
    } else {
        None
    }
}

fn thinking_reasoning_source(request: &MessagesRequest) -> Option<&'static str> {
    request.thinking.as_ref().and_then(|thinking| {
        if thinking.r#type.as_deref() == Some("disabled")
            || matches!(thinking.r#type.as_deref(), Some("enabled" | "adaptive"))
            || thinking.budget_tokens.is_some()
        {
            Some("thinking")
        } else {
            None
        }
    })
}

fn intent_reasoning_source(intent: &str) -> Option<&'static str> {
    match intent {
        "fast" => Some("intent:fast"),
        "quick_reply" => Some("intent:quick_reply"),
        "summarization" => Some("intent:summarization"),
        "deep_think" => Some("intent:deep_think"),
        "reasoning" => Some("intent:reasoning"),
        "tool_use" => Some("intent:tool_use"),
        "agent" => Some("intent:agent"),
        _ => None,
    }
}

fn request_reasoning_effort(request: &MessagesRequest) -> Option<String> {
    if let Some(reasoning) = request.extra.get("reasoning") {
        return Some(
            reasoning
                .get("effort")
                .and_then(Value::as_str)
                .unwrap_or("custom")
                .to_string(),
        );
    }
    if let Some(effort) = request
        .extra
        .get("reasoning_effort")
        .and_then(Value::as_str)
    {
        return Some(effort.to_string());
    }
    let thinking = request.thinking.as_ref()?;
    if thinking.r#type.as_deref() == Some("disabled") {
        return Some("none".to_string());
    }
    if matches!(thinking.r#type.as_deref(), Some("enabled" | "adaptive"))
        || thinking.budget_tokens.is_some()
    {
        return Some("medium".to_string());
    }
    None
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

pub(crate) fn supports_responses(model: &str) -> bool {
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

pub(crate) fn prefers_responses(model: &str) -> bool {
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
        context_window: if model_id.starts_with("gpt-5") {
            Some(400_000)
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

#[derive(Debug, Default, PartialEq, Eq)]
struct PayloadBreakdown {
    text_items: usize,
    thinking_items: usize,
    function_call_items: usize,
    function_output_items: usize,
    largest_item_bytes: usize,
    text_bytes: usize,
    thinking_bytes: usize,
    tool_call_bytes: usize,
    tool_output_bytes: usize,
    instructions_bytes: usize,
    truncated_text_items: usize,
    truncated_text_bytes_saved: usize,
    truncated_tool_output_items: usize,
    truncated_tool_output_bytes_saved: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RequestObservability {
    pub model: String,
    pub stream: bool,
    pub input_items: usize,
    pub has_tools: bool,
    pub has_parallel_tool_calls: bool,
    pub has_reasoning: bool,
    pub has_include: bool,
    pub has_instructions: bool,
    pub body_bytes: usize,
    history_payload_budget_bytes: usize,
    history_payload_bytes_after: usize,
    history_payload_bytes_before: usize,
    history_payload_budget_used_per_mille: usize,
    text_items: usize,
    thinking_items: usize,
    function_call_items: usize,
    function_output_items: usize,
    largest_item_bytes: usize,
    text_bytes: usize,
    thinking_bytes: usize,
    tool_call_bytes: usize,
    tool_output_bytes: usize,
    instructions_bytes: usize,
    truncated_text_items: usize,
    truncated_text_bytes_saved: usize,
    truncated_tool_output_items: usize,
    truncated_tool_output_bytes_saved: usize,
}

pub(crate) fn request_observability(body: &Value) -> RequestObservability {
    let breakdown = payload_breakdown(body);
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string();
    let history_payload_bytes_after = breakdown.text_bytes + breakdown.tool_output_bytes;
    let history_payload_bytes_before = history_payload_bytes_after
        + breakdown.truncated_text_bytes_saved
        + breakdown.truncated_tool_output_bytes_saved;
    let history_payload_budget_bytes = history_payload_budget_bytes_for_stats(&model);
    RequestObservability {
        model,
        stream: body.get("stream").and_then(Value::as_bool).unwrap_or(false),
        input_items: body
            .get("input")
            .or_else(|| body.get("messages"))
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        has_tools: body
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty()),
        has_parallel_tool_calls: body.get("parallel_tool_calls").is_some(),
        has_reasoning: body.get("reasoning").is_some(),
        has_include: body.get("include").is_some(),
        has_instructions: body.get("instructions").is_some(),
        body_bytes: serde_json::to_vec(body).map_or(0, |bytes| bytes.len()),
        history_payload_budget_bytes,
        history_payload_bytes_after,
        history_payload_bytes_before,
        history_payload_budget_used_per_mille: history_payload_bytes_after * 1000
            / history_payload_budget_bytes.max(1),
        text_items: breakdown.text_items,
        thinking_items: breakdown.thinking_items,
        function_call_items: breakdown.function_call_items,
        function_output_items: breakdown.function_output_items,
        largest_item_bytes: breakdown.largest_item_bytes,
        text_bytes: breakdown.text_bytes,
        thinking_bytes: breakdown.thinking_bytes,
        tool_call_bytes: breakdown.tool_call_bytes,
        tool_output_bytes: breakdown.tool_output_bytes,
        instructions_bytes: breakdown.instructions_bytes,
        truncated_text_items: breakdown.truncated_text_items,
        truncated_text_bytes_saved: breakdown.truncated_text_bytes_saved,
        truncated_tool_output_items: breakdown.truncated_tool_output_items,
        truncated_tool_output_bytes_saved: breakdown.truncated_tool_output_bytes_saved,
    }
}

fn history_payload_budget_bytes_for_stats(model: &str) -> usize {
    let model = model.to_ascii_lowercase();
    if model.contains("mini") || model.contains("small") || model.contains("flash") {
        256 * 1024
    } else if model.contains("gpt-5") || model.contains("o3") || model.contains("o4") {
        1024 * 1024
    } else {
        512 * 1024
    }
}

fn payload_breakdown(body: &Value) -> PayloadBreakdown {
    let mut breakdown = PayloadBreakdown {
        instructions_bytes: body
            .get("instructions")
            .and_then(Value::as_str)
            .map_or(0, str::len),
        ..Default::default()
    };

    if let Some(items) = body.get("input").and_then(Value::as_array) {
        for item in items {
            add_responses_item_breakdown(item, &mut breakdown);
        }
    }

    if let Some(messages) = body.get("messages").and_then(Value::as_array) {
        for message in messages {
            add_chat_message_breakdown(message, &mut breakdown);
        }
    }

    breakdown
}

fn add_item_size(item: &Value, breakdown: &mut PayloadBreakdown) {
    let bytes = serde_json::to_vec(item).map_or(0, |bytes| bytes.len());
    breakdown.largest_item_bytes = breakdown.largest_item_bytes.max(bytes);
}

fn add_responses_item_breakdown(item: &Value, breakdown: &mut PayloadBreakdown) {
    add_item_size(item, breakdown);
    match item.get("type").and_then(Value::as_str) {
        Some("function_call") => {
            breakdown.function_call_items += 1;
            breakdown.tool_call_bytes += serde_json::to_vec(item).map_or(0, |bytes| bytes.len());
        }
        Some("function_call_output") => {
            breakdown.function_output_items += 1;
            if let Some(output) = item.get("output").and_then(Value::as_str) {
                breakdown.tool_output_bytes += output.len();
                add_truncation_breakdown(output, breakdown);
            }
        }
        _ => add_message_content_breakdown(item, breakdown),
    }
}

fn add_chat_message_breakdown(message: &Value, breakdown: &mut PayloadBreakdown) {
    add_item_size(message, breakdown);
    add_message_content_breakdown(message, breakdown);
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        breakdown.function_call_items += tool_calls.len();
        breakdown.tool_call_bytes += serde_json::to_vec(tool_calls).map_or(0, |bytes| bytes.len());
    }
}

fn add_message_content_breakdown(item: &Value, breakdown: &mut PayloadBreakdown) {
    match item.get("content") {
        Some(Value::String(text)) => add_text_or_thinking_bytes(text, breakdown),
        Some(Value::Array(parts)) => {
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    add_text_or_thinking_bytes(text, breakdown);
                }
            }
        }
        _ => {}
    }
}

fn add_text_or_thinking_bytes(text: &str, breakdown: &mut PayloadBreakdown) {
    add_truncation_breakdown(text, breakdown);
    if text.contains("[thinking]") {
        breakdown.thinking_items += 1;
        breakdown.thinking_bytes += text.len();
    } else {
        breakdown.text_items += 1;
        breakdown.text_bytes += text.len();
    }
}

fn add_truncation_breakdown(text: &str, breakdown: &mut PayloadBreakdown) {
    if text.starts_with("[text content truncated:") {
        breakdown.truncated_text_items += 1;
        if let Some(original_bytes) = truncated_original_bytes(text) {
            breakdown.truncated_text_bytes_saved += original_bytes.saturating_sub(text.len());
        }
    } else if text.starts_with("[tool output truncated:")
        || text.starts_with("ERROR: [tool output truncated:")
    {
        breakdown.truncated_tool_output_items += 1;
        if let Some(original_bytes) = truncated_original_bytes(text) {
            breakdown.truncated_tool_output_bytes_saved +=
                original_bytes.saturating_sub(text.len());
        }
    }
}

fn truncated_original_bytes(text: &str) -> Option<usize> {
    text.split("original_bytes=")
        .nth(1)
        .and_then(|rest| rest.split([',', ']']).next())
        .and_then(|bytes| bytes.parse().ok())
}

pub(crate) fn log_request_observability(provider: &str, endpoint: &str, body: &Value) {
    if !enabled!(Level::DEBUG) {
        return;
    }

    let stats = request_observability(body);
    debug!(
        provider,
        endpoint,
        model = %stats.model,
        stream = stats.stream,
        input_items = stats.input_items,
        has_tools = stats.has_tools,
        has_parallel_tool_calls = stats.has_parallel_tool_calls,
        has_reasoning = stats.has_reasoning,
        has_include = stats.has_include,
        has_instructions = stats.has_instructions,
        body_bytes = stats.body_bytes,
        history_payload_budget_bytes = stats.history_payload_budget_bytes,
        history_payload_bytes_after = stats.history_payload_bytes_after,
        history_payload_bytes_before = stats.history_payload_bytes_before,
        history_payload_budget_used_per_mille = stats.history_payload_budget_used_per_mille,
        text_items = stats.text_items,
        thinking_items = stats.thinking_items,
        function_call_items = stats.function_call_items,
        function_output_items = stats.function_output_items,
        largest_item_bytes = stats.largest_item_bytes,
        text_bytes = stats.text_bytes,
        thinking_bytes = stats.thinking_bytes,
        tool_call_bytes = stats.tool_call_bytes,
        tool_output_bytes = stats.tool_output_bytes,
        instructions_bytes = stats.instructions_bytes,
        truncated_text_items = stats.truncated_text_items,
        truncated_text_bytes_saved = stats.truncated_text_bytes_saved,
        truncated_tool_output_items = stats.truncated_tool_output_items,
        truncated_tool_output_bytes_saved = stats.truncated_tool_output_bytes_saved,
        "Provider request payload stats"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_gpt5_model_prefers_responses() {
        let info = openai_model_info("gpt-5.5");

        assert!(prefers_responses("gpt-5.5"));
        assert_eq!(info.max_output_tokens, Some(128_000));
        assert_eq!(info.context_window, Some(400_000));
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
    fn request_observability_summarizes_responses_payload() {
        let body = json!({
            "model": "gpt-5.4-mini",
            "input": [
                {"role": "user", "content": "secret prompt"},
                {"role": "user", "content": "[text content truncated: original_bytes=1000, max_historical_text_bytes=32768]"},
                {"role": "assistant", "content": "[thinking]\nprivate chain\n[/thinking]"},
                {"type": "function_call", "name": "read", "arguments": "{\"path\":\"README.md\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "[tool output truncated: original_bytes=2000, max_historical_tool_output_bytes=4096]"}
            ],
            "stream": true,
            "tools": [{"type": "function", "name": "read", "parameters": {}}],
            "parallel_tool_calls": true,
            "reasoning": {"effort": "none"},
            "include": ["reasoning.encrypted_content"],
            "instructions": "system text"
        });

        let stats = request_observability(&body);

        assert_eq!(stats.model, "gpt-5.4-mini");
        assert!(stats.stream);
        assert_eq!(stats.input_items, 5);
        assert!(stats.has_tools);
        assert!(stats.has_parallel_tool_calls);
        assert!(stats.has_reasoning);
        assert!(stats.has_include);
        assert!(stats.has_instructions);
        assert!(stats.body_bytes > 0);
        assert_eq!(stats.text_items, 2);
        assert_eq!(stats.thinking_items, 1);
        assert_eq!(stats.function_call_items, 1);
        assert_eq!(stats.function_output_items, 1);
        assert_eq!(
            stats.text_bytes,
            "secret prompt".len()
                + "[text content truncated: original_bytes=1000, max_historical_text_bytes=32768]"
                    .len()
        );
        assert_eq!(
            stats.tool_output_bytes,
            "[tool output truncated: original_bytes=2000, max_historical_tool_output_bytes=4096]"
                .len()
        );
        assert_eq!(stats.truncated_text_items, 1);
        assert_eq!(
            stats.truncated_text_bytes_saved,
            1000 - "[text content truncated: original_bytes=1000, max_historical_text_bytes=32768]"
                .len()
        );
        assert_eq!(stats.truncated_tool_output_items, 1);
        assert_eq!(
            stats.truncated_tool_output_bytes_saved,
            2000 - "[tool output truncated: original_bytes=2000, max_historical_tool_output_bytes=4096]"
                .len()
        );
        assert_eq!(stats.instructions_bytes, "system text".len());
        assert_eq!(stats.history_payload_budget_bytes, 256 * 1024);
        assert_eq!(
            stats.history_payload_bytes_after,
            stats.text_bytes + stats.tool_output_bytes
        );
        assert_eq!(
            stats.history_payload_bytes_before,
            stats.history_payload_bytes_after
                + stats.truncated_text_bytes_saved
                + stats.truncated_tool_output_bytes_saved
        );
        assert_eq!(stats.history_payload_budget_used_per_mille, 0);
        assert!(stats.thinking_bytes > 0);
        assert!(stats.tool_call_bytes > 0);
        assert!(stats.largest_item_bytes > 0);
    }

    #[test]
    fn request_observability_summarizes_chat_completions_payload() {
        let body = json!({
            "model": "gpt-4.1",
            "messages": [{"role": "user", "content": "secret prompt"}],
            "stream": false
        });

        let stats = request_observability(&body);

        assert_eq!(stats.model, "gpt-4.1");
        assert!(!stats.stream);
        assert_eq!(stats.input_items, 1);
        assert!(!stats.has_tools);
        assert!(!stats.has_parallel_tool_calls);
        assert!(!stats.has_reasoning);
        assert!(!stats.has_include);
        assert!(!stats.has_instructions);
        assert!(stats.body_bytes > 0);
        assert_eq!(stats.text_items, 1);
        assert_eq!(stats.text_bytes, "secret prompt".len());
        assert_eq!(stats.thinking_items, 0);
        assert_eq!(stats.function_call_items, 0);
        assert_eq!(stats.function_output_items, 0);
        assert_eq!(stats.tool_output_bytes, 0);
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
    fn request_log_info_reports_intent_reasoning_and_model_rewrite() {
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

        let info = openai_request_log_info(&req);

        assert_eq!(info.model, "gpt-5.4-mini");
        assert_eq!(info.reasoning_effort.as_deref(), Some("none"));
        assert_eq!(info.reasoning_source, "intent:fast");
    }

    #[test]
    fn request_log_info_reports_explicit_reasoning_effort() {
        let mut req = MessagesRequest {
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
            metadata: Some(json!({"intent": "fast"})),
            extra: Default::default(),
        };
        req.extra
            .insert("reasoning_effort".to_string(), json!("high"));

        let info = openai_request_log_info(&req);

        assert_eq!(info.model, "gpt-5.4-mini");
        assert_eq!(info.reasoning_effort.as_deref(), Some("high"));
        assert_eq!(info.reasoning_source, "explicit:reasoning_effort");
    }

    #[test]
    fn request_log_info_reports_thinking_reasoning_effort() {
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
            thinking: Some(ThinkingConfig {
                r#type: Some("enabled".to_string()),
                budget_tokens: Some(4096),
            }),
            metadata: None,
            extra: Default::default(),
        };

        let info = openai_request_log_info(&req);

        assert_eq!(info.model, "gpt-5.5");
        assert_eq!(info.reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(info.reasoning_source, "thinking");
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
