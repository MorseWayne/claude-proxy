# ChatGPT continuation, context-limit, and metrics follow-up

Date: 2026-06-02

## Goal

Improve ChatGPT/Codex reliability and observability after log and metrics review without moving conversation compaction into `claude-proxy`.

## Approved scope

1. Extend stale WebSocket continuation fallback for `previous_response_id` failures that occur after non-content upstream prelude events but before any downstream item is emitted.
2. Handle large-context failures with a combined policy: normalize real upstream `context_length_exceeded` errors and locally simulate an upstream-style context-limit error only for clearly oversized requests.
3. Add request observability / metrics fields for transport, continuation, fallback, and upstream error details.

## Non-goals

- Do not proactively compress, shrink, drop, or truncate conversation history in `claude-proxy`.
- Do not replay a stream once any downstream SSE item has been emitted.
- Do not add broad routing or model-selection changes in this slice.

## Design

### 1. Stale continuation fallback

The existing WebSocket startup fallback already treats stale `previous_response_id` as replay-safe when a continuation was used and the first upstream event fails. Extend the same safe-replay rule into the WebSocket stream reader for prelude-only failures.

Fallback is allowed only when all are true:

- The request used a cached continuation.
- The upstream error is `previous_response_not_found` / `Previous response ... not found`.
- The provider has not yielded any Anthropic `SseEvent` to the server stream yet. This is the authoritative downstream-emission marker; upstream JSON prelude events such as `codex.rate_limits` do not count unless they are converted into a downstream `SseEvent`.

When allowed, clear volatile WebSocket and continuation state, close the rejected WebSocket connection, and let Auto transport retry using SSE with the original full body. If any downstream `SseEvent` has already been yielded, keep current non-replay error behavior.

### 2. Large-context handling

Use the combined policy approved by the user:

- Real upstream `context_length_exceeded` errors continue to be detected and normalized.
- Local preflight only triggers for clearly oversized ChatGPT requests.
- The local preflight response should mimic an upstream ChatGPT Responses API context-limit error closely enough for Claude Code to trigger its own compact flow.

The local simulated error should include:

- `type: "error"`
- `error.type: "invalid_request_error"`
- `error.code: "context_length_exceeded"`
- `error.param: "input"`
- a message saying the input exceeds the model context window

The threshold must be deterministic and conservative:

- Scope: ChatGPT Responses requests only.
- Metric: serialized upstream Responses request body bytes before send.
- Primary threshold: derive from the provider model capability `context_window` when the upstream model is known, using the same rough byte/token estimate used by existing ChatGPT request-size warnings.
- Fallback threshold: if model capability data is unavailable, use `700 KiB` body bytes. Current metrics show `>=600KB` requests have a high error rate, while many `300-600KB` requests succeed; 700 KiB avoids blocking the successful middle bucket for unknown models.
- Boundary behavior: `body_bytes >= threshold` triggers the local simulated error; smaller requests continue to upstream.

The simulated response must use the same normalization path as a real upstream `context_length_exceeded` body so Claude Code sees equivalent error semantics.

### 3. Metrics and observability

Add low-cardinality fields to request observability so diagnostics no longer require log scraping:

- `transport`
- `websocket_reused`
- `continuation_used`
- `continuation_disabled_reason`
- `continuation_fallback_used`
- `fallback_reason`
- `upstream_error_status`
- `upstream_error_code`
- `upstream_error_message_class`
- `request_body_bytes`
- `upstream_send_body_bytes`

Persist the fields with SQLite `ALTER TABLE ... ADD COLUMN` migrations for existing databases. New columns should be nullable or have safe defaults so old rows and old DB files remain readable. `/admin/metrics` can evolve by adding fields to recent/stored observability rows while preserving existing keys. Summary aggregation can remain minimal in this slice.

## Validation plan

- WebSocket stale continuation after a non-content prelude event falls back to SSE before the provider yields any downstream `SseEvent`.
- WebSocket stale continuation after a downstream `SseEvent` has been yielded does not replay.
- Local ChatGPT request with serialized upstream body bytes `>=` the model-derived threshold returns a context-limit error without calling upstream.
- Local ChatGPT request below the model-derived threshold continues to call upstream.
- Local ChatGPT request for an unknown model falls back to the `700 KiB` threshold.
- Real upstream `context_length_exceeded` remains normalized through the same error semantics as the local simulated error.
- New observability fields are recorded in memory, persisted to SQLite, returned by `/admin/metrics`, and load correctly from a legacy DB that lacks the new columns.

## Risks

- Local oversize thresholds can be too aggressive and trigger compact early. Keep the threshold conservative and ChatGPT-only.
- Replay after downstream output would duplicate user-visible output. The fallback gate must explicitly require no downstream emission.
- Metrics schema changes must be backward-compatible with existing SQLite databases.
