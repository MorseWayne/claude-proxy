use serde_json::{Value, json};

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
