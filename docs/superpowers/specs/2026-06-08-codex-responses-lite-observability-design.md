# Codex Responses Lite compatibility and observability follow-up

Date: 2026-06-08

## Goal

Keep `claude-proxy` aligned with Codex's recent Responses Lite and WebSocket protocol changes while adding enough observability to measure whether the proxy is reducing repeated upstream payloads.

## Approved scope

Implement a conservative first slice:

1. Add Responses Lite transport markers for ChatGPT/Codex HTTP and WebSocket requests.
2. Record whether a request used Responses Lite in provider/server observability.
3. Record `continuation_saved_bytes` so continuation benefits can be measured without log scraping.
4. Make file logs ANSI-free so local log analysis is reliable.

## Non-goals

- Do not redesign full tool conversion in this slice.
- Do not switch hosted web/image tools to standalone tools yet.
- Do not change model routing or default model selection.
- Do not add dashboard/TUI visualizations yet.
- Do not introduce aggressive context shrinking, compaction, or tool-result truncation.

## Design

### 1. Responses Lite transport markers

Codex now sends a Responses Lite marker when the selected model uses Responses Lite. `claude-proxy` should mirror this for ChatGPT/Codex upstream calls.

For HTTP Responses requests, add this header only when the request is known to be Responses Lite:

```text
x-openai-internal-codex-responses-lite: true
```

For WebSocket `response.create`, add the equivalent metadata entry:

```text
ws_request_header_x_openai_internal_codex_responses_lite=true
```

The marker should be derived from existing model/provider capability data where possible. If the code does not already expose a dedicated boolean, add a narrowly scoped helper in the ChatGPT provider layer rather than threading a broad new capability through unrelated providers.

The default behavior for non-ChatGPT and non-Codex-compatible requests must remain unchanged.

### 2. Responses Lite observability

Provider request metadata should carry an optional low-cardinality boolean-like value for Responses Lite usage. The server request observer then persists it to request observability rows.

The field should be backward-compatible:

- SQLite migration uses `ALTER TABLE ... ADD COLUMN`.
- Existing rows can be `NULL` or `0`.
- Admin metrics should preserve existing keys and add the new field only where the current response shape already exposes request observability details.

Suggested persisted column:

```text
responses_lite INTEGER NULL
```

Use `1` for true, `0` for false when known, and `NULL` when unavailable.

### 3. Continuation saved bytes

`claude-proxy` already records logical request body bytes and actual upstream send body bytes. Add a derived metric:

```text
continuation_saved_bytes = max(request_body_bytes - upstream_send_body_bytes, 0)
```

Record it on completed request observability events. This keeps the metric deterministic and avoids relying on transport-specific logs.

Expected interpretation:

- `0`: no savings, no continuation, or full body was sent.
- Positive value: continuation/delta input reduced upstream payload size.

This field should also be backward-compatible through SQLite migration.

Suggested persisted column:

```text
continuation_saved_bytes INTEGER NOT NULL DEFAULT 0
```

### 4. ANSI-free file logs

The current local log files contain ANSI control sequences in at least one output path, which makes scripts and SQL-style parsing noisier. Adjust tracing initialization so file log layers disable ANSI/color output.

Terminal output can keep colors if the existing configuration supports it. The change should be scoped to file logging only.

### 5. Testing strategy

Add focused tests that cover the new behavior without requiring live ChatGPT access:

- HTTP ChatGPT/Codex Responses Lite requests include `x-openai-internal-codex-responses-lite: true`.
- Non-Responses-Lite requests do not include the header.
- WebSocket `response.create` metadata contains `ws_request_header_x_openai_internal_codex_responses_lite=true` when enabled.
- Request observer computes `continuation_saved_bytes` as a saturating positive difference.
- SQLite migration preserves legacy databases and writes/loads `responses_lite` plus `continuation_saved_bytes`.
- File logging configuration disables ANSI for file layers if that initialization path is testable in isolation.

## Risks

- The Responses Lite capability source may not yet be explicit in `claude-proxy`. Keep detection narrow and test-covered to avoid enabling the marker for unrelated models.
- WebSocket metadata shape is protocol-sensitive. Add tests around the serialized `response.create` payload instead of relying only on helper-level tests.
- Metrics schema changes must remain compatible with existing user databases.
- ANSI-free logging should not remove terminal color unless the current logging architecture makes per-layer control impossible.

## Future work

- Implement full Responses Lite standalone tool conversion for web search and image generation.
- Expose continuation saved bytes and Responses Lite usage in TUI/admin dashboards.
- Add higher-level aggregation for continuation hit rate, miss reasons, and saved bytes.
- Revisit context-limit preflight using the new saved-bytes and Responses Lite metrics.
