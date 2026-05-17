use claude_proxy_core::{MessagesRequest, ModelInfo, ThinkingConfig};
use serde_json::Value;

pub(super) fn apply_model_limits(
    request: &mut MessagesRequest,
    model_info: Option<&ModelInfo>,
    configured_max_thinking_tokens: u32,
) {
    let Some(model_info) = model_info else {
        return;
    };

    if let Some(model_max) = model_info.max_output_tokens {
        request.max_tokens = Some(
            request
                .max_tokens
                .map_or(model_max, |max| max.min(model_max)),
        );
    }

    if request.thinking.is_none() && model_can_think(model_info) {
        request.thinking = Some(ThinkingConfig {
            r#type: Some(
                if model_info.supports_adaptive_thinking == Some(true) {
                    "adaptive"
                } else {
                    "enabled"
                }
                .to_string(),
            ),
            budget_tokens: if model_info.supports_adaptive_thinking == Some(true) {
                None
            } else {
                compute_thinking_budget(
                    model_info.min_thinking_budget,
                    model_info.max_thinking_budget,
                    request.max_tokens.or(model_info.max_output_tokens),
                    configured_max_thinking_tokens,
                )
            },
        });
    }
}

pub(super) fn should_use_interleaved_thinking_beta(model_info: Option<&ModelInfo>) -> bool {
    model_info.is_some_and(|model| {
        model.supports_adaptive_thinking != Some(true) && model_can_think(model)
    })
}

pub(super) fn copilot_messages_effort(
    request: &MessagesRequest,
    model_info: Option<&ModelInfo>,
) -> Option<String> {
    let requested_effort = if let Some(effort) = request
        .extra
        .get("output_config")
        .and_then(|v| v.get("effort"))
        .and_then(Value::as_str)
        .or_else(|| {
            request
                .extra
                .get("reasoning_effort")
                .and_then(Value::as_str)
        }) {
        effort.to_string()
    } else {
        let has_thinking = request.thinking.is_some() || model_info.is_some_and(model_can_think);
        if !has_thinking {
            return None;
        }

        request
            .thinking
            .as_ref()
            .and_then(|thinking| thinking.budget_tokens)
            .map(thinking_budget_to_effort)
            .unwrap_or_else(|| "medium".to_string())
    };

    Some(select_supported_reasoning_effort(
        &requested_effort,
        model_info,
    ))
}

fn compute_thinking_budget(
    min_thinking_budget: Option<u32>,
    max_thinking_budget: Option<u32>,
    max_output_tokens: Option<u32>,
    configured_max_thinking_tokens: u32,
) -> Option<u32> {
    let available = max_output_tokens.unwrap_or(configured_max_thinking_tokens);
    if available < 2 {
        return None;
    }

    let hard_upper = available.saturating_sub(1);
    let upper = max_thinking_budget
        .unwrap_or(configured_max_thinking_tokens)
        .min(configured_max_thinking_tokens)
        .min(hard_upper);
    if upper == 0 {
        return None;
    }

    let lower = min_thinking_budget.unwrap_or(1024).min(upper);
    Some((available / 2).clamp(lower, upper))
}

fn model_can_think(model_info: &ModelInfo) -> bool {
    model_info.supports_thinking == Some(true)
        || model_info.supports_adaptive_thinking == Some(true)
        || model_info.max_thinking_budget.is_some()
}

fn thinking_budget_to_effort(budget_tokens: u32) -> String {
    match budget_tokens {
        0..=2048 => "low",
        2049..=8192 => "medium",
        _ => "high",
    }
    .to_string()
}

fn select_supported_reasoning_effort(
    requested_effort: &str,
    model_info: Option<&ModelInfo>,
) -> String {
    let Some(model_info) = model_info else {
        return requested_effort.to_string();
    };

    let supported = &model_info.reasoning_effort_levels;
    if supported.is_empty() || supported.iter().any(|level| level == requested_effort) {
        return requested_effort.to_string();
    }

    if supported.iter().any(|level| level == "medium") {
        return "medium".to_string();
    }

    supported
        .first()
        .cloned()
        .unwrap_or_else(|| requested_effort.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use claude_proxy_core::Message;
    use std::collections::HashMap;

    fn model() -> ModelInfo {
        ModelInfo {
            model_id: "claude-opus-4.7".to_string(),
            supports_thinking: Some(true),
            vendor: Some("anthropic".to_string()),
            max_output_tokens: Some(8192),
            context_window: None,
            supported_endpoints: vec!["/v1/messages".to_string()],
            is_chat_default: None,
            supports_vision: None,
            supports_adaptive_thinking: Some(true),
            min_thinking_budget: Some(1024),
            max_thinking_budget: Some(4096),
            reasoning_effort_levels: vec!["low".to_string(), "medium".to_string()],
        }
    }

    fn request(model: &ModelInfo) -> MessagesRequest {
        MessagesRequest {
            model: model.model_id.clone(),
            system: None,
            messages: Vec::<Message>::new(),
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
    fn apply_model_limits_clamps_and_adds_thinking() {
        let model = ModelInfo {
            model_id: "claude-sonnet-4".to_string(),
            supports_thinking: Some(true),
            vendor: Some("anthropic".to_string()),
            max_output_tokens: Some(4096),
            context_window: None,
            supported_endpoints: vec!["/v1/messages".to_string()],
            is_chat_default: None,
            supports_vision: None,
            supports_adaptive_thinking: Some(false),
            min_thinking_budget: Some(1024),
            max_thinking_budget: Some(2048),
            reasoning_effort_levels: Vec::new(),
        };
        let mut request = request(&model);

        apply_model_limits(&mut request, Some(&model), 16_000);

        assert_eq!(request.max_tokens, Some(4096));
        let thinking = request.thinking.expect("thinking inserted");
        assert_eq!(thinking.r#type.as_deref(), Some("enabled"));
        assert_eq!(thinking.budget_tokens, Some(2048));
    }

    #[test]
    fn apply_model_limits_uses_adaptive_thinking_without_budget() {
        let model = model();
        let mut request = request(&model);

        apply_model_limits(&mut request, Some(&model), 16_000);

        let thinking = request.thinking.expect("thinking inserted");
        assert_eq!(thinking.r#type.as_deref(), Some("adaptive"));
        assert_eq!(thinking.budget_tokens, None);
        assert!(!should_use_interleaved_thinking_beta(Some(&model)));
    }

    #[test]
    fn messages_effort_clamps_to_supported_model_levels() {
        let mut model = model();
        model.min_thinking_budget = None;
        model.max_thinking_budget = None;
        model.reasoning_effort_levels = vec!["medium".to_string()];

        let mut request = request(&model);
        request.thinking = Some(ThinkingConfig {
            r#type: Some("adaptive".to_string()),
            budget_tokens: None,
        });
        request.extra = HashMap::from([(
            "output_config".to_string(),
            serde_json::json!({"effort": "high"}),
        )]);

        assert_eq!(
            copilot_messages_effort(&request, Some(&model)).as_deref(),
            Some("medium")
        );

        request.extra.clear();
        request.thinking = Some(ThinkingConfig {
            r#type: Some("enabled".to_string()),
            budget_tokens: Some(12_000),
        });

        assert_eq!(
            copilot_messages_effort(&request, Some(&model)).as_deref(),
            Some("medium")
        );
    }
}
