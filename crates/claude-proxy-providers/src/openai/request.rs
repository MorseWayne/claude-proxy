use claude_proxy_core::*;
use serde_json::{Value, json};

/// Convert an Anthropic MessagesRequest to an OpenAI ChatCompletion request body.
pub(super) fn convert_request(req: &MessagesRequest) -> Value {
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
        messages.push(json!({"role": "system", "content": text}));
    }

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
                let mut reasoning_parts: Vec<String> = Vec::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                for block in blocks {
                    match block {
                        Content::Text { text } => {
                            parts.push(json!({"type": "text", "text": text}));
                        }
                        Content::Thinking { thinking, .. } => {
                            reasoning_parts.push(thinking.clone());
                        }
                        Content::ToolUse { id, name, input }
                        | Content::ServerToolUse { id, name, input } => {
                            tool_calls.push(json!({
                                "id": id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": serde_json::to_string(input).unwrap_or_default()
                                }
                            }));
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
                        }
                        Content::Unknown => {}
                    }
                }

                if !reasoning_parts.is_empty() || !parts.is_empty() || !tool_calls.is_empty() {
                    let mut msg = json!({"role": role});
                    if !reasoning_parts.is_empty() {
                        msg["reasoning_content"] = json!(reasoning_parts.join("\n"));
                    }
                    if !parts.is_empty() {
                        msg["content"] = json!(parts);
                    }
                    if !tool_calls.is_empty() {
                        msg["tool_calls"] = json!(tool_calls);
                    }
                    messages.push(msg);
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
