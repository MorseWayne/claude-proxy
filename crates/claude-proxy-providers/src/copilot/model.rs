use claude_proxy_core::{
    CapabilityState, EndpointCapabilities, FeatureCapabilities, InputModalities,
    ModalityCapabilities, ModelCapabilities, ModelInfo, ModelLimits, PromptCacheCapability,
    QualityGateBetaLocation, QualityGateCapabilities, QualityGateHeaderKind,
    TokenCountingCapability, ToolSearchCapability,
};
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

    let supports_thinking = model["supports_thinking"]
        .as_bool()
        .or_else(|| capabilities["supports_thinking"].as_bool())
        .or_else(|| supports["thinking"].as_bool());
    let supports_vision = supports["vision"]
        .as_bool()
        .or_else(|| capabilities["supports_vision"].as_bool());
    let supports_adaptive_thinking = model["supports_adaptive_thinking"]
        .as_bool()
        .or_else(|| capabilities["supports_adaptive_thinking"].as_bool())
        .or_else(|| supports["adaptive_thinking"].as_bool());
    let supports_tool_search = model["supports_tool_search"]
        .as_bool()
        .or_else(|| capabilities["supports_tool_search"].as_bool())
        .or_else(|| supports["tool_search"].as_bool());
    let supports_structured_outputs = model["supports_structured_outputs"]
        .as_bool()
        .or_else(|| capabilities["supports_structured_outputs"].as_bool())
        .or_else(|| supports["structured_outputs"].as_bool());
    let supports_strict_tools = model["supports_strict_tools"]
        .as_bool()
        .or_else(|| capabilities["supports_strict_tools"].as_bool())
        .or_else(|| supports["strict_tools"].as_bool());
    let min_thinking_budget = model["min_thinking_budget"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
        .or_else(|| {
            supports["min_thinking_budget"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
        });
    let max_thinking_budget = model["max_thinking_budget"]
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
        });
    let reasoning_effort_levels = supports["reasoning_effort"]
        .as_array()
        .map(|levels| {
            levels
                .iter()
                .filter_map(|level| level.as_str().map(str::to_string))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let supported_endpoints = parse_supported_endpoints(&model["supported_endpoints"]);
    let supports_reasoning_effort = !reasoning_effort_levels.is_empty();

    Some(ModelInfo {
        model_id: model_id.to_string(),
        vendor: model["vendor"]
            .get("name")
            .and_then(Value::as_str)
            .or_else(|| model["vendor"].as_str())
            .map(|s| s.to_ascii_lowercase()),
        is_chat_default: model["is_chat_default"].as_bool(),
        capabilities: ModelCapabilities {
            endpoints: EndpointCapabilities::from_paths(&supported_endpoints),
            modalities: ModalityCapabilities {
                input: InputModalities {
                    image: CapabilityState::from_bool(supports_vision),
                    ..Default::default()
                },
                ..Default::default()
            },
            features: FeatureCapabilities {
                streaming: CapabilityState::Supported,
                system_prompt: CapabilityState::Supported,
                tools: CapabilityState::Supported,
                tool_choice: CapabilityState::Supported,
                thinking: CapabilityState::from_bool(supports_thinking),
                adaptive_thinking: CapabilityState::from_bool(supports_adaptive_thinking),
                reasoning_effort: CapabilityState::from_bool(
                    supports_reasoning_effort.then_some(true),
                ),
                sampling: CapabilityState::Supported,
                stop_sequences: CapabilityState::Supported,
                ..Default::default()
            },
            limits: ModelLimits {
                max_output_tokens: limits["max_output_tokens"]
                    .as_u64()
                    .and_then(|n| u32::try_from(n).ok()),
                context_window: limits["max_context_window_tokens"]
                    .as_u64()
                    .or_else(|| limits["context_window"].as_u64())
                    .or_else(|| capabilities["context_window"].as_u64())
                    .and_then(|n| u32::try_from(n).ok()),
                min_thinking_budget,
                max_thinking_budget,
                reasoning_effort_levels,
            },
            quality: QualityGateCapabilities {
                tool_search: match supports_tool_search {
                    Some(true) => ToolSearchCapability::supported(
                        QualityGateHeaderKind::Anthropic3p,
                        QualityGateBetaLocation::Header,
                    ),
                    Some(false) => ToolSearchCapability::unsupported(),
                    None => ToolSearchCapability::default(),
                },
                prompt_cache: PromptCacheCapability::basic(),
                interleaved_thinking: CapabilityState::from_bool(supports_thinking),
                max_effort: CapabilityState::from_bool(supports_reasoning_effort.then_some(true)),
                structured_outputs: CapabilityState::from_bool(supports_structured_outputs),
                strict_tools: CapabilityState::from_bool(supports_strict_tools),
                token_counting: TokenCountingCapability::rough(),
                ..Default::default()
            },
            supported_parameters: copilot_supported_parameters(supports_thinking.is_some()),
        },
    })
}

fn copilot_supported_parameters(include_thinking: bool) -> Vec<String> {
    let mut parameters = vec![
        "system".to_string(),
        "messages".to_string(),
        "max_tokens".to_string(),
        "stream".to_string(),
        "tools".to_string(),
        "tool_choice".to_string(),
        "temperature".to_string(),
        "top_p".to_string(),
        "stop_sequences".to_string(),
    ];
    if include_thinking {
        parameters.push("thinking".to_string());
    }
    parameters
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
    use claude_proxy_core::TokenCountingMode;

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
                    "tool_search": true,
                    "structured_outputs": true,
                    "strict_tools": false,
                    "min_thinking_budget": 2048,
                    "max_thinking_budget": 12000,
                    "reasoning_effort": ["low", "medium", "high", "xhigh"]
                }
            }
        });

        let model = parse_copilot_model(&raw).expect("valid model");
        assert_eq!(model.model_id, "claude-sonnet-4");
        assert_eq!(model.vendor.as_deref(), Some("anthropic"));
        assert_eq!(model.capabilities.limits.max_output_tokens, Some(16384));
        assert_eq!(model.capabilities.limits.context_window, Some(200000));
        assert_eq!(
            model.capabilities.endpoints.supported_paths(),
            vec!["/v1/messages", "/chat/completions"]
        );
        assert_eq!(model.is_chat_default, Some(true));
        assert!(model.capabilities.modalities.input.image.is_supported());
        assert!(model.capabilities.features.thinking.is_supported());
        assert_eq!(
            model.capabilities.features.adaptive_thinking,
            CapabilityState::Unsupported
        );
        assert_eq!(model.capabilities.limits.min_thinking_budget, Some(1024));
        assert_eq!(model.capabilities.limits.max_thinking_budget, Some(8192));
        assert_eq!(
            model.capabilities.limits.reasoning_effort_levels,
            vec!["low", "medium", "high", "xhigh"]
        );
        assert!(model.capabilities.quality.tool_search.state.is_supported());
        assert_eq!(
            model.capabilities.quality.tool_search.header_kind,
            QualityGateHeaderKind::Anthropic3p
        );
        assert!(model.capabilities.quality.structured_outputs.is_supported());
        assert_eq!(
            model.capabilities.quality.strict_tools,
            CapabilityState::Unsupported
        );
        assert_eq!(
            model.capabilities.quality.token_counting.mode,
            TokenCountingMode::Rough
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
