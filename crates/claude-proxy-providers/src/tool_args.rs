use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tracing::debug;

const MAX_READ_OFFSET_WITHOUT_LINE_COUNT: u64 = 1_000_000;

#[derive(Debug, PartialEq, Eq)]
struct ToolArgumentDiagnostic {
    field: &'static str,
    sanitization: &'static str,
    original_len: usize,
    sanitized_len: usize,
}

pub(crate) fn sanitize_tool_arguments(tool_name: &str, arguments: &str) -> Option<String> {
    let mut diagnostics = Vec::new();
    let sanitized =
        sanitize_tool_arguments_with_diagnostics(tool_name, arguments, &mut diagnostics);
    for diagnostic in diagnostics {
        debug!(
            tool_name,
            field = diagnostic.field,
            sanitization = diagnostic.sanitization,
            original_len = diagnostic.original_len,
            sanitized_len = diagnostic.sanitized_len,
            "Sanitized tool arguments"
        );
    }
    sanitized
}

fn sanitize_tool_arguments_with_diagnostics(
    tool_name: &str,
    arguments: &str,
    diagnostics: &mut Vec<ToolArgumentDiagnostic>,
) -> Option<String> {
    if tool_name != "Read" {
        return None;
    }

    let mut input = serde_json::from_str::<Value>(arguments).ok()?;
    let object = input.as_object_mut()?;
    let mut changed = false;
    if matches!(object.get("pages"), Some(Value::String(pages)) if pages.is_empty()) {
        diagnostics.push(ToolArgumentDiagnostic {
            field: "pages",
            sanitization: "remove_empty_string",
            original_len: 0,
            sanitized_len: 0,
        });
        object.remove("pages");
        changed = true;
    }

    changed |= sanitize_read_line_window(object, diagnostics);

    changed
        .then(|| serde_json::to_string(&input).ok())
        .flatten()
}

fn sanitize_read_line_window(
    object: &mut serde_json::Map<String, Value>,
    diagnostics: &mut Vec<ToolArgumentDiagnostic>,
) -> bool {
    let Some(offset) = numeric_object_field(object, "offset") else {
        return false;
    };

    let mut changed = false;
    if offset == 0 {
        diagnostics.push(ToolArgumentDiagnostic {
            field: "offset",
            sanitization: "raise_zero_to_one",
            original_len: offset.to_string().len(),
            sanitized_len: 1,
        });
        object.insert("offset".to_string(), json!(1));
        changed = true;
    }

    let offset = offset.max(1);
    let limit = numeric_object_field(object, "limit").filter(|limit| *limit > 0);
    let Some(file_path) = object.get("file_path").and_then(Value::as_str) else {
        return changed;
    };
    let Some(line_count) = read_line_count(file_path) else {
        if offset > MAX_READ_OFFSET_WITHOUT_LINE_COUNT {
            diagnostics.push(ToolArgumentDiagnostic {
                field: "offset",
                sanitization: "remove_unverifiable_large_offset",
                original_len: offset.to_string().len(),
                sanitized_len: 0,
            });
            object.remove("offset");
            changed = true;
        }
        return changed;
    };

    if offset <= line_count {
        return changed;
    }

    let recovered_offset = recover_concatenated_offset(offset, line_count);
    let corrected_offset = recovered_offset.unwrap_or_else(|| {
        let limit = limit.unwrap_or(1);
        line_count.saturating_sub(limit.saturating_sub(1)).max(1)
    });
    diagnostics.push(ToolArgumentDiagnostic {
        field: "offset",
        sanitization: if recovered_offset.is_some() {
            "recover_concatenated_offset"
        } else {
            "clamp_to_file_window"
        },
        original_len: offset.to_string().len(),
        sanitized_len: corrected_offset.to_string().len(),
    });
    object.insert("offset".to_string(), json!(corrected_offset));
    changed = true;

    if let Some(limit) = limit {
        let max_limit = line_count
            .saturating_sub(corrected_offset)
            .saturating_add(1);
        if limit > max_limit {
            diagnostics.push(ToolArgumentDiagnostic {
                field: "limit",
                sanitization: "clamp_to_file_window",
                original_len: limit.to_string().len(),
                sanitized_len: max_limit.max(1).to_string().len(),
            });
            object.insert("limit".to_string(), json!(max_limit.max(1)));
        }
    }

    changed
}

fn numeric_object_field(object: &serde_json::Map<String, Value>, key: &str) -> Option<u64> {
    object
        .get(key)
        .and_then(|value| value.as_u64().or_else(|| value.as_str()?.parse().ok()))
}

fn recover_concatenated_offset(offset: u64, line_count: u64) -> Option<u64> {
    let digits = offset.to_string();
    (1..digits.len()).rev().find_map(|prefix_len| {
        let prefix = digits[..prefix_len].parse::<u64>().ok()?;
        (prefix > 0 && prefix <= line_count).then_some(prefix)
    })
}

fn read_line_count(file_path: &str) -> Option<u64> {
    let path = resolve_read_path(file_path)?;
    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    Some(reader.lines().count() as u64)
}

fn resolve_read_path(file_path: &str) -> Option<PathBuf> {
    let path = Path::new(file_path);
    if path.is_absolute() {
        return path.exists().then(|| path.to_path_buf());
    }

    let path = std::env::current_dir().ok()?.join(path);
    path.exists().then_some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_sanitizer_recovers_concatenated_large_offset() {
        let path = temp_read_fixture(1_113);
        let sanitized = sanitize_tool_arguments(
            "Read",
            &json!({
                "file_path": path.to_string_lossy(),
                "offset": 5_206_854_u64,
                "limit": 5
            })
            .to_string(),
        )
        .expect("sanitized read arguments");
        let input: Value = serde_json::from_str(&sanitized).expect("valid json");

        assert_eq!(input["offset"], 520);
        assert_eq!(input["limit"], 5);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_sanitizer_removes_absurd_offset_when_file_is_unavailable() {
        let sanitized = sanitize_tool_arguments(
            "Read",
            &json!({
                "file_path": "missing-routes.rs",
                "offset": 5_206_854_u64,
                "limit": 5
            })
            .to_string(),
        )
        .expect("sanitized read arguments");
        let input: Value = serde_json::from_str(&sanitized).expect("valid json");

        assert!(input.get("offset").is_none());
        assert_eq!(input["limit"], 5);
    }

    #[test]
    fn non_read_tool_pages_empty_string_is_preserved() {
        assert_eq!(
            sanitize_tool_arguments("Other", "{\"pages\":\"\",\"value\":\"\"}"),
            None
        );
    }

    #[test]
    fn read_sanitizer_reports_pii_safe_diagnostics() {
        let path = temp_read_fixture(1_113);
        let mut diagnostics = Vec::new();
        let sanitized = sanitize_tool_arguments_with_diagnostics(
            "Read",
            &json!({
                "file_path": path.to_string_lossy(),
                "pages": "",
                "offset": 5_206_854_u64,
                "limit": 5
            })
            .to_string(),
            &mut diagnostics,
        )
        .expect("sanitized read arguments");
        let input: Value = serde_json::from_str(&sanitized).expect("valid json");

        assert!(input.get("pages").is_none());
        assert_eq!(input["offset"], 520);
        assert_eq!(
            diagnostics,
            vec![
                ToolArgumentDiagnostic {
                    field: "pages",
                    sanitization: "remove_empty_string",
                    original_len: 0,
                    sanitized_len: 0,
                },
                ToolArgumentDiagnostic {
                    field: "offset",
                    sanitization: "recover_concatenated_offset",
                    original_len: 7,
                    sanitized_len: 3,
                },
            ]
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn read_sanitizer_reports_removed_unverifiable_offset() {
        let mut diagnostics = Vec::new();
        let sanitized = sanitize_tool_arguments_with_diagnostics(
            "Read",
            &json!({
                "file_path": "missing-routes.rs",
                "offset": 5_206_854_u64,
                "limit": 5
            })
            .to_string(),
            &mut diagnostics,
        )
        .expect("sanitized read arguments");
        let input: Value = serde_json::from_str(&sanitized).expect("valid json");

        assert!(input.get("offset").is_none());
        assert_eq!(
            diagnostics,
            vec![ToolArgumentDiagnostic {
                field: "offset",
                sanitization: "remove_unverifiable_large_offset",
                original_len: 7,
                sanitized_len: 0,
            }]
        );
    }

    fn temp_read_fixture(lines: usize) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "claude-proxy-read-fixture-{}-{}.txt",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let body = (1..=lines)
            .map(|line| format!("line {line}\n"))
            .collect::<String>();
        std::fs::write(&path, body).expect("write read fixture");
        path
    }
}
