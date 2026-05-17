use claude_proxy_core::*;
use serde_json::{Map, Value};

#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct SanitizeStats {
    pub empty_text_blocks: usize,
    pub empty_thinking_blocks: usize,
    pub unsigned_thinking_blocks: usize,
    pub empty_messages: usize,
    pub merged_messages: usize,
}

impl SanitizeStats {
    pub fn changed(&self) -> bool {
        self.empty_text_blocks > 0
            || self.empty_thinking_blocks > 0
            || self.unsigned_thinking_blocks > 0
            || self.empty_messages > 0
            || self.merged_messages > 0
    }
}

pub(super) fn prepare_messages_request(
    request: &mut MessagesRequest,
    effort: Option<&str>,
) -> Result<(Value, SanitizeStats), serde_json::Error> {
    let stats = sanitize_messages(request);
    request.extra.clear();

    let mut body = serde_json::to_value(request)?;
    if let Value::Object(ref mut obj) = body {
        obj.remove("metadata");
        normalize_thinking(obj, effort);
        disable_eager_input_streaming(obj);
    }

    Ok((body, stats))
}

fn sanitize_messages(request: &mut MessagesRequest) -> SanitizeStats {
    let mut stats = SanitizeStats::default();
    let mut messages = Vec::with_capacity(request.messages.len());

    for mut message in request.messages.drain(..) {
        if sanitize_message(&mut message, &mut stats) {
            push_message(&mut messages, message, &mut stats);
        } else {
            stats.empty_messages += 1;
        }
    }

    request.messages = messages;
    stats
}

fn sanitize_message(message: &mut Message, stats: &mut SanitizeStats) -> bool {
    match &mut message.content {
        MessageContent::Text(text) => {
            trim_text(text);
            !text.is_empty()
        }
        MessageContent::Blocks(blocks) => {
            blocks.retain_mut(|block| keep_content_block(block, stats));
            !blocks.is_empty()
        }
    }
}

fn keep_content_block(block: &mut Content, stats: &mut SanitizeStats) -> bool {
    match block {
        Content::Text { text } => {
            trim_text(text);
            if text.is_empty() {
                stats.empty_text_blocks += 1;
                false
            } else {
                true
            }
        }
        Content::Thinking {
            thinking,
            signature,
        } => {
            if thinking.trim().is_empty() {
                stats.empty_thinking_blocks += 1;
                false
            } else if signature.as_ref().is_none_or(String::is_empty) {
                stats.unsigned_thinking_blocks += 1;
                false
            } else {
                true
            }
        }
        Content::ToolUse { .. }
        | Content::ToolResult { .. }
        | Content::ServerToolUse { .. }
        | Content::Unknown(_) => true,
    }
}

fn trim_text(text: &mut String) {
    if text.chars().last().is_some_and(char::is_whitespace) {
        text.truncate(text.trim_end().len());
    }
}

fn push_message(messages: &mut Vec<Message>, message: Message, stats: &mut SanitizeStats) {
    if let Some(last) = messages.last_mut()
        && last.role == message.role
    {
        merge_message_content(&mut last.content, message.content);
        stats.merged_messages += 1;
        return;
    }

    messages.push(message);
}

fn merge_message_content(left: &mut MessageContent, right: MessageContent) {
    match (left, right) {
        (MessageContent::Text(left_text), MessageContent::Text(right_text)) => {
            left_text.push('\n');
            left_text.push_str(&right_text);
        }
        (MessageContent::Blocks(left_blocks), MessageContent::Blocks(right_blocks)) => {
            left_blocks.extend(right_blocks);
        }
        (MessageContent::Blocks(left_blocks), MessageContent::Text(right_text)) => {
            left_blocks.push(Content::Text { text: right_text });
        }
        (left @ MessageContent::Text(_), MessageContent::Blocks(mut right_blocks)) => {
            let MessageContent::Text(left_text) =
                std::mem::replace(left, MessageContent::Blocks(Vec::new()))
            else {
                unreachable!();
            };
            let mut merged = Vec::with_capacity(1 + right_blocks.len());
            merged.push(Content::Text { text: left_text });
            merged.append(&mut right_blocks);
            *left = MessageContent::Blocks(merged);
        }
    }
}

fn normalize_thinking(body: &mut Map<String, Value>, effort: Option<&str>) {
    let needs_output_effort = if let Some(Value::Object(thinking)) = body.get_mut("thinking") {
        match thinking.get("type").and_then(Value::as_str) {
            Some("enabled") | Some("adaptive") => {
                thinking.insert("type".to_string(), Value::String("adaptive".to_string()));
                thinking.remove("budget_tokens");
                true
            }
            _ => false,
        }
    } else {
        false
    };

    if needs_output_effort {
        let effort = effort.unwrap_or("medium");
        let output_config = body
            .entry("output_config".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let Value::Object(config) = output_config {
            config.insert("effort".to_string(), Value::String(effort.to_string()));
        }
    }
}

fn disable_eager_input_streaming(body: &mut Map<String, Value>) {
    if let Some(Value::Array(tools)) = body.get_mut("tools") {
        for tool in tools {
            if let Value::Object(tool_obj) = tool {
                tool_obj.insert("eager_input_streaming".to_string(), Value::Bool(false));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn base_request(messages: Vec<Message>) -> MessagesRequest {
        MessagesRequest {
            model: "claude-opus-4.7".to_string(),
            system: None,
            messages,
            max_tokens: Some(8192),
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
        }
    }

    #[test]
    fn strips_invalid_content_and_omits_empty_messages() {
        let mut request = base_request(vec![
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![
                    Content::Thinking {
                        thinking: "completed".to_string(),
                        signature: None,
                    },
                    Content::Thinking {
                        thinking: " \n\t ".to_string(),
                        signature: Some("sig".to_string()),
                    },
                    Content::Text {
                        text: "final answer  \n".to_string(),
                    },
                ]),
            },
            Message {
                role: Role::Assistant,
                content: MessageContent::Blocks(vec![Content::Thinking {
                    thinking: String::new(),
                    signature: None,
                }]),
            },
        ]);

        let stats = sanitize_messages(&mut request);

        assert_eq!(stats.unsigned_thinking_blocks, 1);
        assert_eq!(stats.empty_thinking_blocks, 2);
        assert_eq!(stats.empty_messages, 1);
        assert_eq!(request.messages.len(), 1);
        match &request.messages[0].content {
            MessageContent::Blocks(blocks) => {
                assert_eq!(blocks.len(), 1);
                assert!(matches!(&blocks[0], Content::Text { text } if text == "final answer"));
            }
            MessageContent::Text(_) => panic!("expected content blocks"),
        }
    }

    #[test]
    fn preserves_signed_thinking_blocks() {
        let mut request = base_request(vec![Message {
            role: Role::Assistant,
            content: MessageContent::Blocks(vec![Content::Thinking {
                thinking: "completed thinking".to_string(),
                signature: Some("valid-signature".to_string()),
            }]),
        }]);

        let stats = sanitize_messages(&mut request);

        assert_eq!(stats, SanitizeStats::default());
        match &request.messages[0].content {
            MessageContent::Blocks(blocks) => assert!(matches!(
                &blocks[0],
                Content::Thinking {
                    thinking,
                    signature: Some(signature),
                } if thinking == "completed thinking" && signature == "valid-signature"
            )),
            MessageContent::Text(_) => panic!("expected content blocks"),
        }
    }

    #[test]
    fn merges_adjacent_messages_after_sanitizing() {
        let mut request = base_request(vec![
            Message {
                role: Role::User,
                content: MessageContent::Text("first".to_string()),
            },
            Message {
                role: Role::User,
                content: MessageContent::Blocks(vec![Content::Text {
                    text: "second".to_string(),
                }]),
            },
        ]);

        let stats = sanitize_messages(&mut request);

        assert_eq!(stats.merged_messages, 1);
        assert_eq!(request.messages.len(), 1);
        match &request.messages[0].content {
            MessageContent::Blocks(blocks) => {
                assert!(matches!(&blocks[0], Content::Text { text } if text == "first"));
                assert!(matches!(&blocks[1], Content::Text { text } if text == "second"));
            }
            MessageContent::Text(_) => panic!("expected merged content blocks"),
        }
    }

    #[test]
    fn prepares_copilot_messages_body() {
        let mut request = base_request(vec![Message {
            role: Role::User,
            content: MessageContent::Text("hello".to_string()),
        }]);
        request.tools = Some(vec![Tool {
            name: "example".to_string(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
        }]);
        request.thinking = Some(ThinkingConfig {
            r#type: Some("enabled".to_string()),
            budget_tokens: Some(8192),
        });
        request.extra = HashMap::from([(
            "output_config".to_string(),
            serde_json::json!({"effort": "high"}),
        )]);
        request.metadata = Some(serde_json::json!({"user_id": "client-user"}));

        let (body, stats) = prepare_messages_request(&mut request, Some("high")).expect("body");

        assert!(!stats.changed());
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert!(body["thinking"].get("budget_tokens").is_none());
        assert_eq!(body["output_config"]["effort"], "high");
        assert_eq!(body["tools"][0]["eager_input_streaming"], false);
        assert!(body.get("tool_streaming").is_none());
        assert!(body.get("metadata").is_none());
        assert!(body.get("extra").is_none());
    }
}
