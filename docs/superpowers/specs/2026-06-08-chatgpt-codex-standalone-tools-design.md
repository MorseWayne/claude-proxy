# ChatGPT/Codex standalone tools conversion

Date: 2026-06-08

## Goal

Make ChatGPT/Codex Responses requests use Codex-style standalone/custom tools by default, instead of always representing every Claude Code tool as a generic OpenAI Responses `function` tool.

This is an intentionally bolder follow-up to the Responses Lite transport and dashboard metrics work. The goal is to test closer alignment with native Codex tool protocol while keeping a fast rollback path.

## Approved direction

Implement a thorough ChatGPT/Codex scoped conversion:

1. Add `chatgpt.standalone_tools`, defaulting to `true`.
2. Convert supported Claude Code tools to Codex standalone/custom tool definitions by default for ChatGPT/Codex Responses requests.
3. Preserve function-tool fallback for tools or schemas that cannot be safely mapped.
4. Keep non-ChatGPT providers on the existing function-tool conversion path.
5. Include history, stream-converter, and WebSocket continuation compatibility so default-on does not silently discard tool context.

## Non-goals

- Do not change Anthropic, OpenAI Chat Completions, or Copilot conversion behavior.
- Do not remove the existing function-tool conversion path.
- Do not fail a request only because one tool cannot be converted to standalone/custom form.
- Do not change server routing, auth, rate limiting, or admin metrics in this slice.
- Do not change the downstream Anthropic SSE shape returned to Claude Code.

## Design

### 1. Configuration

Extend `ChatGptProviderConfig` with:

```rust
#[serde(default = "default_true")]
pub standalone_tools: bool,
```

Default behavior:

- `responses_lite = true`
- `standalone_tools = true`

This makes the new conversion path active by default, while preserving a config-level rollback:

```toml
[providers.chatgpt]
standalone_tools = false
```

### 2. Conversion mode boundary

Extend the Responses conversion context with a tool conversion mode:

```rust
pub enum ToolConversionMode {
    Function,
    CodexStandalone,
}

pub struct ConversionContext<'a> {
    pub provider_id: Option<&'a str>,
    pub model: Option<&'a ModelInfo>,
    pub tool_conversion_mode: ToolConversionMode,
}
```

Default mode is `Function`, so existing callers keep current behavior unless they opt in.

The ChatGPT/Codex body builder passes `CodexStandalone` only when `chatgpt.standalone_tools` is enabled. If disabled, it passes `Function` and must produce the existing request shape.

### 3. Tool definition conversion

Current generic Responses conversion emits:

```json
{
  "type": "function",
  "name": "Bash",
  "description": "...",
  "parameters": { ... }
}
```

The new ChatGPT/Codex mode uses a deterministic conversion table. The implementation must not invent ad hoc mappings outside this table. Unknown or unsafe entries fall back to the existing `function` output.

#### Initial conversion table

| Tool category | Candidate names | Standalone/custom request shape | Downstream tool input adapter | Fallback rule |
| --- | --- | --- | --- | --- |
| Shell text tool | `Bash` | Codex custom/freeform text tool, if confirmed accepted by upstream. Expected shape is `type: "custom"`, `name: "Bash"`, `description`, and a text/freeform format field matching Codex/OpenAI Responses custom-tool schema. | Convert returned custom input text to Anthropic `tool_use.input = {"command": <input>, "description": ""}` unless the input is valid JSON containing `command`. | If request-side schema is not confirmed by fixture/test, keep `type: "function"`. |
| Text-search/read/edit tools | `Read`, `Edit`, `Write`, `MultiEdit`, `Glob`, `Grep`, `LS` | Prefer native standalone only when a field-preserving mapping is confirmed. These tools have structured parameters, so function fallback is expected until a safe native schema is proven. | Preserve exact JSON fields expected by Claude Code tools. | Fall back to `type: "function"` if any required field would be renamed, dropped, or made ambiguous. |
| Agent/meta tools | `Task`, `TodoWrite` | Convert only if a native Codex schema preserves the full JSON payload. | Preserve exact JSON payload. | Fall back to `type: "function"`. |
| Web tools | `WebFetch`, `WebSearch` | Convert only if native schema and hosted-tool semantics are proven not to conflict with Claude Code tool execution. | Preserve exact JSON payload. | Fall back to `type: "function"`. |
| Unknown tools | Any other name | None. | Existing function call handling. | Always fall back to `type: "function"`. |

This table intentionally supports a thorough rollout while avoiding lossy mappings. “Thorough” means every common tool is evaluated and tested under this table; it does not mean every tool must be forced into custom form if doing so would break its arguments.

#### Mapping confirmation protocol

A tool may be emitted as standalone/custom only when all confirmation checks pass:

| Check | Accept condition | Reject/fallback condition |
| --- | --- | --- |
| Schema evidence | A focused fixture or unit test asserts the exact request-side JSON shape for this tool. | The shape is inferred only from comments, logs, or response-side events. |
| Field preservation | Every Claude Code required argument is preserved with the same meaning and type, or has a documented reversible adapter. | Any required field is dropped, renamed without adapter, made optional, or converted to lossy freeform text. |
| Tool input adapter | The downstream Anthropic `tool_use.input` produced from upstream `custom_tool_call` exactly matches what Claude Code expects. | The converter would produce generic `{"input": ...}` for a known structured Claude Code tool without an adapter. |
| Mixed-mode behavior | Tests cover this tool alongside at least one fallback function tool. | The tool only works when it is the only tool in the request. |
| Continuation behavior | Continuation prefix extraction either preserves this tool's completed custom call or emits an explicit disabled reason. | Custom calls disappear from continuation state silently. |

Each converted tool must have a named test or fixture documenting the evidence. If any check fails, the implementation must fall back to `type: "function"` for that tool while keeping default-on standalone mode for other tools.

### 4. Tool choice compatibility

`tool_choice` normalization must be deterministic in mixed mode:

- If the selected tool remains a function tool, keep the existing Responses named-function selector.
- If the selected tool is converted to custom/standalone and an accepted selector format is confirmed, emit that selector.
- If no accepted selector format is confirmed for a converted custom tool, degrade to `"auto"` rather than emitting invalid JSON.
- If a request has mixed custom and function tools with no explicit selection, keep `"auto"`.

Required mixed-mode cases:

1. Explicit `tool_choice` points at a converted tool.
2. Explicit `tool_choice` points at a fallback function tool.
3. Explicit `tool_choice` points at an unknown/missing tool.
4. No explicit `tool_choice` and a mixed tool list is present.

### 5. Tool history compatibility

Request history currently serializes assistant tool uses as `function_call` and user tool results as `function_call_output`.

Default-on standalone/custom conversion must not drop historical calls or outputs.

Implementation rules:

- Preserve `tool_use_id`, tool names, arguments, outputs, and `is_error` meaning.
- If the current tool definition is converted to custom/standalone but the historical tool call is already represented as `function_call`, keep it unless upstream rejects the mixed history in a focused test.
- If historical custom calls are introduced, add a name-specific adapter that converts between custom freeform input and Claude Code’s expected JSON input shape.
- Never convert structured tool history into freeform text unless there is a reversible adapter for that tool.

### 6. WebSocket continuation compatibility

This is a hard requirement because default-on standalone/custom tools can otherwise reduce the continuation savings added in earlier slices.

Current transport code must be reviewed and updated as needed:

- `assistant_output_item_to_input_prefix_item` currently supports `function_call` and may ignore `custom_tool_call`.
- `terminal_assistant_output_items` and related continuation prefix extraction must not silently discard reusable custom tool calls.
- If custom tool calls are emitted by upstream, the continuation cache should either:
  1. preserve them as supported assistant output prefix items, or
  2. record a clear disabled/fallback reason and avoid unsafe continuation reuse.

Preferred behavior:

- Add support for preserving `custom_tool_call` output prefix items when they include enough stable identifiers (`call_id`, `name`, and `input` or completed input state).
- Keep `function_call_output` deltas after matching custom/function calls when building continuation deltas.
- Extend canonical-body tests so standalone/custom mode does not fragment continuation cache keys with conversion-only or transport-only fields.

Minimum continuation field contract:

| Field | Required for reuse | Source | Notes |
| --- | --- | --- | --- |
| `type` | Yes | Upstream item | Must remain `custom_tool_call` or the confirmed native equivalent. |
| `call_id` | Yes | Upstream item | Links later tool output to the assistant call. Missing `call_id` disables reuse. |
| `name` | Yes | Upstream item | Selects the reversible tool input adapter. Unknown names may still be preserved only if no adapter is needed. |
| `input` / completed input state | Yes for freeform tools | `response.custom_tool_call_input.done` or completed output item | Streaming deltas must be closed before reuse. Incomplete input disables reuse. |
| output index / item id | No for canonical matching, yes while streaming | Upstream events | Used to assemble input; should not become a stable cache-key-only dependency unless required. |

If a safe custom continuation representation cannot be implemented in the same slice, the implementation must explicitly disable continuation for affected custom-tool requests with a low-cardinality reason rather than silently losing context.

### 7. Stream converter compatibility

The response converter already handles both:

- `function_call`
- `custom_tool_call`

This slice should keep the downstream Anthropic-compatible `tool_use` shape unchanged. If a custom text tool such as `Bash` returns freeform input, the converter must adapt it into the JSON shape expected by Claude Code tools. For `Bash`, that means producing `{"command": ..., "description": ...}` rather than `{"input": ...}` when the tool name is `Bash`.

Existing generic custom-tool behavior can remain for unknown custom tools.

### 8. Rollback behavior

Rollback paths:

1. Runtime config: set `chatgpt.standalone_tools = false`.
2. Code-level: revert the implementation commit.
3. Per-tool fallback: unknown/unsafe tools remain generic `function` tools.
4. Continuation fallback: affected custom-tool requests can opt out with an explicit reason if safe reuse is not possible.

The request builder must not panic or fail when standalone conversion is incomplete.

## Testing strategy

Add focused tests. The following are required, not optional:

### Config and request conversion

- Config parses `standalone_tools` and defaults to `true`.
- ChatGPT body builder defaults to standalone/custom mode.
- `standalone_tools = false` preserves old function-tool shape.
- Supported tools convert to the expected Codex standalone/custom schema.
- Unknown or unsafe tools fall back to `type: "function"`.
- Mixed mode is stable when one tool converts and another falls back.

### Tool choice

- `tool_choice` targeting a converted tool emits the confirmed selector or safely degrades to `"auto"`.
- `tool_choice` targeting a fallback function tool keeps the existing function selector.
- Mixed custom/function tools with no explicit selection keep `"auto"`.

Required behavior matrix:

| Tool list | Explicit choice | Expected output |
| --- | --- | --- |
| Single converted tool | converted tool name | Confirmed custom selector, or `"auto"` if no accepted selector exists. |
| Single fallback function tool | fallback tool name | Existing `{"type":"function","name":...}` Responses selector. |
| Mixed converted + fallback tools | converted tool name | Confirmed custom selector, or `"auto"` if no accepted selector exists. |
| Mixed converted + fallback tools | fallback tool name | Existing function selector for the fallback tool. |
| Mixed converted + fallback tools | unknown/missing tool name | `"auto"` or existing safe fallback behavior; never invalid JSON. |
| Any tool list | no explicit choice | `"auto"`.

### History and stream conversion

- Existing `tool_use` / `tool_result` history remains represented and linked.
- `Bash` custom text input maps back to `tool_use.input.command`.
- Existing stream converter tests still pass for `function_call` and `custom_tool_call` events.
- Unknown custom tools keep generic `{"input": ...}` behavior.

### WebSocket continuation

- Continuation prefix extraction preserves supported `custom_tool_call` assistant outputs, or disables continuation with an explicit reason.
- Mixed function/custom tool output histories keep relevant `function_call_output` / custom output items.
- Canonical body tests cover mode-specific fields and ensure transport-only metadata does not split cache keys.
- A continuation test must cover a custom-tool response followed by a tool-result-only delta.

Validation commands:

```bash
cargo test -p claude-proxy-config chatgpt
cargo test -p claude-proxy-providers --lib chatgpt_responses_body
cargo test -p claude-proxy-providers --lib test_convert_to_responses
cargo test -p claude-proxy-providers --lib custom_tool
cargo test -p claude-proxy-providers --lib continuation
cargo clippy -p claude-proxy-config -p claude-proxy-providers -- -D warnings
cargo fmt --all --check
git diff --check
```

Before committing implementation, run GitNexus impact analysis for the edited symbols and `gitnexus_detect_changes` for the final diff.

## Risks

- Native Codex standalone/custom request schema may be stricter than inferred from response fixtures.
- Tool parameter shape changes can break Claude Code tool calls if a mapping drops or rewrites fields incorrectly.
- Mixed history (`function_call`) plus standalone/current tool definitions may affect upstream behavior or continuation hits.
- Default-on behavior increases rollout risk; the config toggle and per-tool fallback are required safety valves.
- WebSocket continuation may regress if custom output prefix handling is incomplete; this is a required implementation checkpoint.

## Acceptance criteria

The implementation is acceptable only if:

1. Default ChatGPT/Codex config enables standalone tools.
2. Disabling `standalone_tools` restores the old function-tool request shape.
3. At least one confirmed supported tool uses standalone/custom request shape.
4. Unknown/unsafe tools fall back to `function` without request failure.
5. Downstream Anthropic `tool_use` shape remains compatible with Claude Code.
6. Continuation behavior for custom-tool responses is either safely supported or explicitly disabled with a clear reason.
7. Required focused tests, formatting, clippy, and GitNexus checks pass.

## Future work

- Add observability counters for standalone conversion successes/fallbacks per low-cardinality reason.
- Add live upstream soak tests once the request-side schema is implemented.
- Expand native mappings as more Codex fixtures or protocol evidence becomes available.
