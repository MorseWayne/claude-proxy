use std::collections::HashMap;

use claude_proxy_core::{Content, MessageContent, MessagesRequest};
use sha2::{Digest, Sha256};

pub(super) const IDENTIFIER_MAX_BYTES: usize = 64;

#[derive(Debug, Clone, Default)]
pub struct ResponsesCorrelation {
    tool_names: IdentityMap,
    tool_call_ids: IdentityMap,
}

#[derive(Debug, Clone, Default)]
struct IdentityMap {
    to_upstream: HashMap<String, String>,
    from_upstream: HashMap<String, String>,
}

impl IdentityMap {
    fn insert(&mut self, original: &str, namespace: &str) {
        if self.to_upstream.contains_key(original) {
            return;
        }
        let mut upstream = bounded_identifier(original, namespace);
        let mut collision = 0_u32;
        while self
            .from_upstream
            .get(&upstream)
            .is_some_and(|existing| existing != original)
        {
            collision += 1;
            upstream = hashed_identifier(original, &format!("{namespace}-{collision}"));
        }
        self.to_upstream
            .insert(original.to_string(), upstream.clone());
        self.from_upstream.insert(upstream, original.to_string());
    }

    fn to_upstream<'a>(&'a self, value: &'a str) -> &'a str {
        self.to_upstream
            .get(value)
            .map(String::as_str)
            .unwrap_or(value)
    }

    fn restore_original<'a>(&'a self, value: &'a str) -> &'a str {
        self.from_upstream
            .get(value)
            .map(String::as_str)
            .unwrap_or(value)
    }
}

impl ResponsesCorrelation {
    pub fn from_request(request: &MessagesRequest) -> Self {
        let mut correlation = Self::default();
        if let Some(tools) = &request.tools {
            for tool in tools {
                correlation.tool_names.insert(&tool.name, "tool");
            }
        }
        for message in &request.messages {
            let MessageContent::Blocks(blocks) = &message.content else {
                continue;
            };
            for block in blocks {
                match block {
                    Content::ToolUse { id, name, .. } | Content::ServerToolUse { id, name, .. } => {
                        correlation.tool_names.insert(name, "tool");
                        correlation.tool_call_ids.insert(id, "call");
                    }
                    Content::ToolResult { tool_use_id, .. } => {
                        correlation.tool_call_ids.insert(tool_use_id, "call");
                    }
                    _ => {}
                }
            }
        }
        correlation
    }

    pub(super) fn upstream_tool_name<'a>(&'a self, name: &'a str) -> &'a str {
        self.tool_names.to_upstream(name)
    }

    pub(super) fn anthropic_tool_name<'a>(&'a self, name: &'a str) -> &'a str {
        self.tool_names.restore_original(name)
    }

    pub(super) fn upstream_call_id<'a>(&'a self, id: &'a str) -> &'a str {
        self.tool_call_ids.to_upstream(id)
    }

    pub(super) fn anthropic_call_id<'a>(&'a self, id: &'a str) -> &'a str {
        self.tool_call_ids.restore_original(id)
    }
}

fn bounded_identifier(value: &str, namespace: &str) -> String {
    let valid = !value.is_empty()
        && value.len() <= IDENTIFIER_MAX_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
    if valid {
        return value.to_string();
    }

    hashed_identifier(value, namespace)
}

fn hashed_identifier(value: &str, namespace: &str) -> String {
    let mut readable = value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if readable.is_empty() {
        readable.push_str(namespace);
    }
    let digest = Sha256::digest(format!("{namespace}\0{value}").as_bytes());
    let suffix = digest[..8]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let prefix_budget = IDENTIFIER_MAX_BYTES - suffix.len() - 2;
    readable.truncate(readable.floor_char_boundary(prefix_budget));
    format!("{readable}__{suffix}")
}
