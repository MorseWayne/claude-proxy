use claude_proxy_core::*;
use serde_json::Value;

const COMPACT_SYSTEM_PREFIXES: &[&str] = &[
    "This is a compacted conversation",
    "This is a continuation",
];

/// Result of preprocessing a MessagesRequest for Premium optimization.
#[derive(Debug, Default)]
pub struct PreprocessResult {
    pub is_warmup: bool,
    pub is_compact: bool,
    pub is_subagent: bool,
}

/// Detects compact requests by checking the beginning of the system prompt text.
fn detect_compact(system: &Option<SystemPrompt>) -> bool {
    let text = match system {
        Some(SystemPrompt::Text(s)) => s.as_str(),
        Some(SystemPrompt::Blocks(blocks)) => {
            blocks.first().map_or("", |b| match b {
                Content::Text { text } => text.as_str(),
                _ => "",
            })
        }
        None => return false,
    };

    let start = &text[..text.len().min(100)];
    COMPACT_SYSTEM_PREFIXES
        .iter()
        .any(|prefix| start.starts_with(prefix))
}

/// Detects subagent markers in system reminders (__SUBAGENT_MARKER__ pattern).
fn detect_subagent(system: &Option<SystemPrompt>) -> bool {
    let text = match system {
        Some(SystemPrompt::Text(s)) => s.as_str(),
        Some(SystemPrompt::Blocks(blocks)) => {
            blocks.first().map_or("", |b| match b {
                Content::Text { text } => text.as_str(),
                _ => "",
            })
        }
        None => return false,
    };

    text.contains("__SUBAGENT_MARKER__")
}

/// Detects if the request has an `anthropic-beta` header (via extra fields).
fn has_beta_header(request: &MessagesRequest) -> bool {
    request
        .extra
        .get("anthropic-beta")
        .is_some()
}

/// Apply tool_result merging to the request (mutates in place).
pub fn merge_tool_results_inplace(messages: &mut Vec<Message>) -> bool {
    let mut merged = false;
    let mut new_messages: Vec<Message> = Vec::new();
    let mut i = 0;

    while i < messages.len() {
        let msg = &messages[i];

        if msg.role != Role::User {
            new_messages.push(messages[i].clone());
            i += 1;
            continue;
        }

        if let MessageContent::Blocks(ref blocks) = msg.content {
            let mut new_blocks: Vec<Content> = Vec::new();
            let mut j = 0;

            while j < blocks.len() {
                match &blocks[j] {
                    Content::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } => {
                        // Look ahead for text blocks to merge
                        let mut merged_text = match content {
                            Some(Value::String(s)) => s.clone(),
                            Some(v) => v.to_string(),
                            None => String::new(),
                        };
                        let mut k = j + 1;

                        while k < blocks.len() {
                            if let Content::Text { text } = &blocks[k] {
                                if !merged_text.is_empty() {
                                    merged_text.push('\n');
                                }
                                merged_text.push_str(text);
                                merged = true;
                                k += 1;
                            } else {
                                break;
                            }
                        }

                        let new_content = if merged_text.is_empty() {
                            content.clone()
                        } else {
                            Some(Value::String(merged_text))
                        };

                        new_blocks.push(Content::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: new_content,
                            is_error: *is_error,
                        });
                        j = k;
                    }
                    _ => {
                        new_blocks.push(blocks[j].clone());
                        j += 1;
                    }
                }
            }

            new_messages.push(Message {
                role: msg.role.clone(),
                content: MessageContent::Blocks(new_blocks),
            });
        } else {
            new_messages.push(messages[i].clone());
        }
        i += 1;
    }

    if merged {
        *messages = new_messages;
    }

    merged
}

pub fn preprocess(
    request: &MessagesRequest,
    enable_warmup: bool,
    enable_compact: bool,
    enable_agent_marking: bool,
    _enable_tool_merge: bool,
) -> PreprocessResult {
    let mut result = PreprocessResult::default();

    if enable_compact {
        result.is_compact = detect_compact(&request.system);
    }

    if enable_agent_marking {
        result.is_subagent = detect_subagent(&request.system);
    }

    if enable_warmup
        && !result.is_compact
        && request.tools.as_ref().map_or(true, |t| t.is_empty())
        && !has_beta_header(request)
    {
        result.is_warmup = true;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_compact_positive() {
        let system = Some(SystemPrompt::Text(
            "This is a compacted conversation summary...".to_string(),
        ));
        assert!(detect_compact(&system));
    }

    #[test]
    fn test_detect_compact_negative() {
        let system = Some(SystemPrompt::Text(
            "You are a helpful assistant.".to_string(),
        ));
        assert!(!detect_compact(&system));
    }

    #[test]
    fn test_detect_compact_continuation() {
        let system = Some(SystemPrompt::Text(
            "This is a continuation of the previous conversation...".to_string(),
        ));
        assert!(detect_compact(&system));
    }

    #[test]
    fn test_detect_subagent_positive() {
        let system = Some(SystemPrompt::Text(
            "__SUBAGENT_MARKER__{\"session_id\":\"abc\"} some system text".to_string(),
        ));
        assert!(detect_subagent(&system));
    }

    #[test]
    fn test_detect_subagent_negative() {
        let system = Some(SystemPrompt::Text("Normal system prompt".to_string()));
        assert!(!detect_subagent(&system));
    }

    #[test]
    fn test_warmup_detection() {
        let request = MessagesRequest {
            model: "test".to_string(),
            system: None,
            messages: vec![],
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
            extra: std::collections::HashMap::new(),
        };
        let result = preprocess(&request, true, true, true, true);
        assert!(result.is_warmup);
        assert!(!result.is_compact);
        assert!(!result.is_subagent);
    }

    #[test]
    fn test_no_warmup_with_tools() {
        let request = MessagesRequest {
            model: "test".to_string(),
            system: None,
            messages: vec![],
            max_tokens: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: true,
            tools: Some(vec![Tool {
                name: "test_tool".to_string(),
                description: None,
                input_schema: serde_json::json!({}),
            }]),
            tool_choice: None,
            thinking: None,
            metadata: None,
            extra: std::collections::HashMap::new(),
        };
        let result = preprocess(&request, true, true, true, true);
        assert!(!result.is_warmup);
    }

    #[test]
    fn test_merge_tool_results() {
        let mut messages = vec![Message {
            role: Role::User,
            content: MessageContent::Blocks(vec![
                Content::ToolResult {
                    tool_use_id: "tool_1".to_string(),
                    content: Some(Value::String("result text".to_string())),
                    is_error: None,
                },
                Content::Text {
                    text: "additional info".to_string(),
                },
            ]),
        }];
        let merged = merge_tool_results_inplace(&mut messages);
        assert!(merged);

        if let MessageContent::Blocks(ref blocks) = messages[0].content {
            assert_eq!(blocks.len(), 1);
            if let Content::ToolResult { content, .. } = &blocks[0] {
                assert_eq!(
                    content.as_ref().and_then(|v| v.as_str()),
                    Some("result text\nadditional info")
                );
            } else {
                panic!("Expected ToolResult");
            }
        }
    }
}
