use claude_proxy_core::*;
use serde_json::Value;

use crate::tool_choice::normalize_for_chat_completions;

pub(super) fn convert_to_openai_chat(req: &MessagesRequest) -> Value {
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
        messages.push(serde_json::json!({"role": "system", "content": text}));
    }

    for msg in &req.messages {
        let role = match msg.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        };

        match &msg.content {
            MessageContent::Text(text) => {
                messages.push(serde_json::json!({"role": role, "content": text}));
            }
            MessageContent::Blocks(blocks) => {
                let mut parts: Vec<Value> = Vec::new();
                for block in blocks {
                    match block {
                        Content::Text { text } => {
                            parts.push(serde_json::json!({"type": "text", "text": text}));
                        }
                        Content::Thinking { thinking, .. } => {
                            parts.push(serde_json::json!({
                                "type": "text",
                                "text": format!("[thinking]\n{thinking}\n[/thinking]")
                            }));
                        }
                        Content::ToolUse { id, name, input }
                        | Content::ServerToolUse { id, name, input } => {
                            messages.push(serde_json::json!({
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
                            messages.push(serde_json::json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content_str
                            }));
                            continue;
                        }
                        Content::Unknown(_) => {}
                    }
                }
                if !parts.is_empty() {
                    messages.push(serde_json::json!({"role": role, "content": parts}));
                }
            }
        }
    }

    let mut body = serde_json::json!({
        "model": req.model,
        "messages": messages,
        "stream": req.stream,
    });

    if let Some(max_tokens) = req.max_tokens {
        body["max_tokens"] = serde_json::json!(max_tokens);
    }
    if let Some(temperature) = req.temperature {
        body["temperature"] = serde_json::json!(temperature);
    }
    if let Some(top_p) = req.top_p {
        body["top_p"] = serde_json::json!(top_p);
    }
    if let Some(stop) = &req.stop_sequences {
        body["stop"] = serde_json::json!(stop);
    }
    if let Some(tools) = &req.tools {
        let openai_tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema
                    }
                })
            })
            .collect();
        body["tools"] = serde_json::json!(openai_tools);
    }
    if let Some(tc) = &req.tool_choice {
        body["tool_choice"] = normalize_for_chat_completions(tc);
    }
    if let Some(thinking) = &req.thinking {
        let mut tv = serde_json::Map::new();
        if let Some(ref t) = thinking.r#type {
            tv.insert("type".to_string(), serde_json::json!(t));
        }
        if let Some(bt) = thinking.budget_tokens {
            tv.insert("budget_tokens".to_string(), serde_json::json!(bt));
        }
        body["thinking"] = serde_json::json!(tv);
    }

    body
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    #[test]
    fn convert_to_openai_chat_normalizes_named_tool_choice() {
        let req = MessagesRequest {
            model: "gpt-4.1".to_string(),
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

        let body = convert_to_openai_chat(&req);

        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "function": {"name": "WebSearch"}})
        );
    }
}
