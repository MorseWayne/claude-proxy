use claude_proxy_core::ModelInfo;
use serde_json::Value;
use tracing::warn;

pub(super) fn parse_copilot_model(model: &Value) -> Option<ModelInfo> {
    if !(model["model_picker_enabled"].as_bool() == Some(true)
        || model["capabilities"]["embeddings"].as_str().is_some())
    {
        return None;
    }

    let Some(model_id) = model["id"].as_str().filter(|id| !id.is_empty()) else {
        warn!("Skipping Copilot model without an id: {model:?}");
        return None;
    };

    let capabilities = &model["capabilities"];
    let limits = &capabilities["limits"];
    let billing = &model["billing"];
    let supports = &capabilities["supports"];

    Some(ModelInfo {
        model_id: model_id.to_string(),
        supports_thinking: model["supports_thinking"]
            .as_bool()
            .or_else(|| capabilities["supports_thinking"].as_bool())
            .or_else(|| supports["thinking"].as_bool()),
        vendor: model["vendor"]
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| model["vendor"].as_str())
            .map(|s| s.to_ascii_lowercase()),
        max_output_tokens: limits["max_output_tokens"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok()),
        context_window: limits["max_context_window_tokens"]
            .as_u64()
            .or_else(|| limits["context_window"].as_u64())
            .or_else(|| capabilities["context_window"].as_u64())
            .and_then(|n| u32::try_from(n).ok()),
        supported_endpoints: parse_supported_endpoints(&model["supported_endpoints"]),
        is_chat_default: model["is_chat_default"].as_bool(),
        supports_vision: supports["vision"]
            .as_bool()
            .or_else(|| capabilities["supports_vision"].as_bool()),
        supports_adaptive_thinking: model["supports_adaptive_thinking"]
            .as_bool()
            .or_else(|| capabilities["supports_adaptive_thinking"].as_bool())
            .or_else(|| supports["adaptive_thinking"].as_bool()),
        min_thinking_budget: model["min_thinking_budget"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .or_else(|| {
                supports["min_thinking_budget"]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
            }),
        max_thinking_budget: model["max_thinking_budget"]
            .as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .or_else(|| {
                supports["max_thinking_budget"]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
            })
            .or_else(|| {
                billing["max_thinking_budget"]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok())
            }),
        reasoning_effort_levels: supports["reasoning_effort"]
            .as_array()
            .map(|levels| {
                levels
                    .iter()
                    .filter_map(|level| level.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

pub(super) fn supports_responses_only(endpoints: &[String]) -> bool {
    endpoints.iter().any(|e| e == "/responses")
        && !endpoints.iter().any(|e| e == "/chat/completions")
}

fn parse_supported_endpoints(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|endpoints| {
            endpoints
                .iter()
                .filter_map(|endpoint| {
                    endpoint
                        .as_str()
                        .or_else(|| endpoint.get("path").and_then(Value::as_str))
                        .or_else(|| endpoint.get("url").and_then(Value::as_str))
                        .or_else(|| endpoint.get("endpoint").and_then(Value::as_str))
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_copilot_model_extracts_capabilities() {
        let raw = serde_json::json!({
            "id": "claude-sonnet-4",
            "vendor": {"name": "Anthropic"},
            "is_chat_default": true,
            "model_picker_enabled": true,
            "supports_thinking": true,
            "supports_adaptive_thinking": false,
            "min_thinking_budget": 1024,
            "max_thinking_budget": 8192,
            "supported_endpoints": ["/v1/messages", {"path": "/chat/completions"}],
            "capabilities": {
                "limits": {"max_output_tokens": 16384, "max_context_window_tokens": 200000},
                "supports": {
                    "vision": true,
                    "adaptive_thinking": true,
                    "min_thinking_budget": 2048,
                    "max_thinking_budget": 12000,
                    "reasoning_effort": ["low", "medium", "high", "xhigh"]
                }
            }
        });

        let model = parse_copilot_model(&raw).expect("valid model");
        assert_eq!(model.model_id, "claude-sonnet-4");
        assert_eq!(model.vendor.as_deref(), Some("anthropic"));
        assert_eq!(model.max_output_tokens, Some(16384));
        assert_eq!(model.context_window, Some(200000));
        assert_eq!(
            model.supported_endpoints,
            vec!["/v1/messages", "/chat/completions"]
        );
        assert_eq!(model.is_chat_default, Some(true));
        assert_eq!(model.supports_vision, Some(true));
        assert_eq!(model.supports_thinking, Some(true));
        assert_eq!(model.supports_adaptive_thinking, Some(false));
        assert_eq!(model.min_thinking_budget, Some(1024));
        assert_eq!(model.max_thinking_budget, Some(8192));
        assert_eq!(
            model.reasoning_effort_levels,
            vec!["low", "medium", "high", "xhigh"]
        );
    }

    #[test]
    fn responses_route_only_when_chat_completions_absent() {
        assert!(supports_responses_only(&["/responses".to_string()]));
        assert!(!supports_responses_only(&[
            "/responses".to_string(),
            "/chat/completions".to_string()
        ]));
        assert!(!supports_responses_only(&["/v1/messages".to_string()]));
    }
}
