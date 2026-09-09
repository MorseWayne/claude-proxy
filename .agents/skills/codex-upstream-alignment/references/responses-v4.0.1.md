# Responses Alignment Evidence from v4.0.1

Read this reference only when the upstream delta affects Responses, ChatGPT/Codex transport, Structured Outputs, model capabilities, or related tests.

## Contract Layers

Keep these separate during analysis:

1. The current official OpenAI Responses API contract.
2. The narrower downstream contract deliberately exposed by claude-proxy.
3. The observed private ChatGPT/Codex backend contract.

At the reviewed baseline, claude-proxy required `stream: true`, forced `store: false`, rejected stateful fields, and accepted string or item-array input. Native Responses routing was limited to OpenAI and ChatGPT providers.

## Provider Differences Observed

- OpenAI accepted standard `input`, `text.format`, and `max_output_tokens` behavior through native passthrough.
- The ChatGPT/Codex backend required list input; claude-proxy normalized public string input to a user item.
- The backend rejected `max_output_tokens`; claude-proxy removed it while retaining the requested value only for observability. This does not enforce an output limit.
- Responses Lite required `reasoning.context=all_turns` and `parallel_tool_calls=false`; claude-proxy applied both only when Lite was effective.
- `text.format.type=json_schema` succeeded end to end after isolating those unrelated request-shape failures.

These are dated observations. Compare new Codex builders, fixtures, model metadata, and live behavior before keeping or removing a workaround.

## Capability Shape

The v4.0.1 model contract added provider identity, a routable `qualified_id`, and endpoint-specific `capabilities.responses` fields:

```json
{
  "provider": "chatgpt",
  "qualified_id": "chatgpt/gpt-5.6-terra",
  "capabilities": {
    "responses": {
      "streaming": "required",
      "stateful": "unsupported",
      "storage": "unsupported",
      "input_formats": ["string", "items"],
      "structured_outputs": "supported",
      "unsupported_parameters": ["max_output_tokens"]
    }
  }
}
```

Update capability metadata whenever effective downstream behavior changes. Do not infer parameter support from the coarse endpoint flag.

## Verification Pattern

Use deterministic coverage before live checks:

- Handler tests for input shapes, streaming, and state rejection.
- OpenAI mock integration tests proving JSON Schema, standard parameters, unknown items, and headers remain intact.
- ChatGPT normalization tests for string input, item preservation, unsupported parameters, Lite invariants, and Structured Outputs preservation.
- Model-list tests proving provider-specific declarations.

For a live Structured Outputs check, verify all of the following:

- `response.created` or `response.completed` echoes `text.format.type=json_schema`.
- Streamed output reconstructs schema-valid JSON.
- A terminal `response.completed` arrives without `response.failed`.

When diagnosing a new backend rejection, begin with a known-minimal request and add one field at a time. HTTP 200 alone is not sufficient evidence of Structured Outputs support.
