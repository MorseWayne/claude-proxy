use claude_proxy_core::MessagesRequest;
use serde_json::{Value, json};

pub(crate) fn normalize_for_anthropic_messages(request: &mut MessagesRequest) {
    let Some(value) = request.extra.remove("parallel_tool_calls") else {
        return;
    };
    let Some(parallel_tool_calls) = value.as_bool() else {
        request
            .extra
            .insert("parallel_tool_calls".to_string(), value);
        return;
    };

    if parallel_tool_calls || request.tools.as_ref().is_none_or(Vec::is_empty) {
        return;
    }

    let tool_choice = request
        .tool_choice
        .get_or_insert_with(|| json!({"type": "auto"}));
    let Some(tool_choice) = tool_choice.as_object_mut() else {
        request
            .extra
            .insert("parallel_tool_calls".to_string(), json!(false));
        return;
    };
    tool_choice.insert("disable_parallel_tool_use".to_string(), json!(true));
}

pub(crate) fn normalize_for_chat_completions(tool_choice: &Value) -> Value {
    normalize_tool_choice(
        tool_choice,
        |name| json!({"type": "function", "function": {"name": name}}),
    )
}

pub(crate) fn normalize_for_responses(tool_choice: &Value) -> Value {
    normalize_tool_choice(
        tool_choice,
        |name| json!({"type": "function", "name": name}),
    )
}

fn normalize_tool_choice(
    tool_choice: &Value,
    function_choice: impl FnOnce(&str) -> Value,
) -> Value {
    if let Some(choice_type) = tool_choice.get("type").and_then(Value::as_str) {
        match choice_type {
            "auto" => return json!("auto"),
            "none" => return json!("none"),
            "any" => return json!("required"),
            "tool" => {
                if let Some(name) = tool_choice.get("name").and_then(Value::as_str) {
                    return function_choice(name);
                }
            }
            _ => {}
        }
    }
    tool_choice.clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(parallel_tool_calls: bool) -> MessagesRequest {
        serde_json::from_value(json!({
            "model": "claude-test",
            "messages": [{"role": "user", "content": "hello"}],
            "tools": [{
                "name": "lookup",
                "input_schema": {"type": "object"}
            }],
            "parallel_tool_calls": parallel_tool_calls
        }))
        .unwrap()
    }

    #[test]
    fn maps_disabled_parallel_calls_to_anthropic_tool_choice() {
        let mut request = request(false);

        normalize_for_anthropic_messages(&mut request);

        assert!(!request.extra.contains_key("parallel_tool_calls"));
        assert_eq!(
            request.tool_choice,
            Some(json!({
                "type": "auto",
                "disable_parallel_tool_use": true
            }))
        );
    }

    #[test]
    fn augments_existing_anthropic_tool_choice_when_parallel_calls_are_disabled() {
        let mut request = request(false);
        request.tool_choice = Some(json!({"type": "tool", "name": "lookup"}));

        normalize_for_anthropic_messages(&mut request);

        assert_eq!(
            request.tool_choice,
            Some(json!({
                "type": "tool",
                "name": "lookup",
                "disable_parallel_tool_use": true
            }))
        );
    }

    #[test]
    fn removes_enabled_parallel_calls_for_anthropic_default_behavior() {
        let mut request = request(true);

        normalize_for_anthropic_messages(&mut request);

        assert!(!request.extra.contains_key("parallel_tool_calls"));
        assert!(request.tool_choice.is_none());
    }
}
