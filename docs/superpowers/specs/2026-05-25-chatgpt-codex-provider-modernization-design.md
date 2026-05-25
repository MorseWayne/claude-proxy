# ChatGPT/Codex Provider Modernization Design

Date: 2026-05-25
Status: Approved for planning
Scope: `claude-proxy` ChatGPT/OpenAI provider layer

## Goal

Modernize the ChatGPT/Codex provider path using the strongest patterns observed in `/home/wayne/source/open/pi/packages/ai`, while preserving the existing stable SSE path as a fallback.

The target outcome is a better long-session ChatGPT/Codex experience: accurate model capability contracts, richer Responses request options, safer prompt cache keys, correct usage accounting, WebSocket transport, SSE fallback, and strict continuation/delta-input support.

## Chosen approach

Implement the full capability set in phases rather than one large rewrite.

This keeps the final design ambitious while making each phase reviewable and testable:

1. Foundation: model/request/cache/usage correctness.
2. WebSocket transport with SSE fallback.
3. WebSocket continuation using `previous_response_id` and delta input.
4. Full validation, GitNexus review, commit, and index refresh.

## Architecture

Split ChatGPT/Codex responsibilities into focused modules over time:

- `chatgpt/request.rs` builds Codex Responses request bodies and applies provider-specific options.
- `chatgpt/transport.rs` abstracts SSE and WebSocket transports behind one stream-producing interface.
- `chatgpt/session.rs` owns per-session WebSocket connection state, busy state, idle expiry, and continuation state.
- `chatgpt/models.rs` owns ChatGPT/Codex-specific `ModelInfo` and capability contracts.
- `chatgpt.rs` remains the provider orchestrator for auth, token refresh, retry, rate-limit cache, and transport selection.

The outer `/v1/messages` server and provider registry API should remain unchanged.

## Data flow

1. `/v1/messages` resolves the model and invokes `ChatGptProvider.chat_with_observer()` as today.
2. The provider gets an existing ChatGPT token or refreshes it on 401.
3. The request passes through OpenAI intent handling.
4. ChatGPT options are resolved from request metadata, request `extra`, and provider runtime config.
5. The Codex request body is built with:
   - `store: false`
   - `stream: true`
   - `instructions`
   - `text.verbosity`
   - `include: ["reasoning.encrypted_content"]` when appropriate
   - `prompt_cache_key`
   - `tool_choice`
   - `parallel_tool_calls`
   - `service_tier`
   - `reasoning.effort` and `reasoning.summary`
6. Transport selection chooses `sse`, `websocket`, or `auto`.
7. Both transports feed the existing Responses-to-Anthropic SSE conversion layer.
8. Provider observer metadata continues to update usage, upstream model, request id, and stop reason.

## Transport behavior

### SSE

SSE remains the stable fallback path. It keeps existing behavior:

- POST to ChatGPT Codex Responses endpoint.
- prompt-too-long shrink retry.
- 401 refresh and retry.
- response header and stream event rate-limit snapshots.
- provider observer metadata.

### WebSocket

WebSocket is the preferred long-session transport.

It should:

- Connect to the Codex Responses endpoint using a `ws`/`wss` URL.
- Send ChatGPT token and account id headers.
- Use the current OpenAI beta WebSocket header required by Codex Responses.
- Send `response.create` payloads.
- Parse WebSocket messages into the same Responses event stream consumed by the existing converter.
- Close or return cached connections based on session state.

### Auto fallback

In `auto` mode:

- Try WebSocket first.
- If WebSocket fails before the first upstream event, record diagnostics and fallback to SSE.
- If WebSocket fails after the first upstream event, return a stream error and do not replay via SSE.

This avoids duplicate model output.

## Continuation

Continuation is enabled only for WebSocket sessions with a stable session/thread key.

Each reusable session may cache:

- last full request body
- last upstream response id
- assistant response items produced by the previous turn

A later request may use continuation only when:

- the non-input parts of the request body match the cached body
- the new input starts with previous input plus previous assistant response items
- the cached response id is available
- the session is not concurrently busy

When these checks pass, send:

- `previous_response_id`
- only the delta input items

When any check fails, send the full input. This fallback is silent and expected.

Continuation state must be cleared on transport errors, aborts, protocol errors, and request shape mismatch that indicates unsafe reuse.

## Prompt cache key policy

Prompt cache keys must not be provider-instance-global.

Priority:

1. Explicit request metadata/extra prompt cache key.
2. Stable client session/thread id.
3. No key or a request-scoped generated key, depending on final compatibility constraints.

Keys are clamped to OpenAI's 64-character prompt cache key limit.

The implementation should log the key source, not the key value.

## Model capability contract

ChatGPT/Codex models should have dedicated `ModelInfo` rather than blindly reusing generic OpenAI metadata.

The ChatGPT/Codex contract must reflect the actual request builder:

- Responses endpoint support.
- Streaming support.
- Reasoning support and supported reasoning effort levels.
- Image input only for models that support it.
- Service tier and verbosity options where supported.
- Stop sequences and max output token behavior must not be advertised if the ChatGPT request builder deliberately omits them.

This avoids client-visible capability drift.

## Usage and cost accounting

Usage extraction should treat OpenAI cached input tokens as a subset of input tokens.

When upstream returns cached-token details:

- non-cached input = input tokens minus cached tokens
- cache read = cached tokens
- output = output tokens
- total should match upstream total when available

Service tier may adjust estimated cost when the model metadata supports pricing multipliers.

## Error handling

- Authentication failures still trigger token refresh and token clearing where appropriate.
- Prompt-too-long shrink retry remains available on the SSE path and any HTTP request path.
- WebSocket startup/transport failures follow the fallback rules above.
- Continuation mismatch is not an error; it falls back to full input.
- Usage-limit and rate-limit responses should produce actionable error messages when upstream provides plan/reset details.
- Malformed upstream JSON/SSE/WebSocket payloads should become explicit provider/protocol errors, not empty successful streams.

## Observability

Add structured, prompt-content-free fields where relevant:

- selected transport
- fallback transport
- WebSocket reused
- continuation used
- continuation delta item count
- prompt cache key source
- upstream response id
- retry-after milliseconds
- rate-limit summary

Diagnostics should help debug transport and continuation choices without logging prompt or response content.

## Test plan

### Phase 1 tests

- ChatGPT/Codex `ModelInfo` capability assertions.
- Request body tests for verbosity, service tier, reasoning summary, tools, and prompt cache key source/length.
- Usage extraction tests for cached-token accounting.
- Existing ChatGPT SSE and prompt-too-long tests remain passing.

### Phase 2 tests

- WebSocket success fixture.
- WebSocket first-event-before-failure starts stream normally.
- WebSocket failure before first event falls back to SSE in auto mode.
- WebSocket failure after first event does not fall back.
- Abort closes or releases resources.
- Reusable session connection is reused; busy session uses a temporary connection.

### Phase 3 tests

- Continuation sends `previous_response_id` and delta input on a safe prefix match.
- Body option changes disable continuation.
- Prefix mismatch disables continuation.
- Missing session disables continuation.
- Transport/protocol failure clears continuation state.

### Final validation

Run:

- `cargo fmt --check`
- targeted provider tests
- `cargo test -p claude-proxy-providers chatgpt`
- `cargo test -p claude-proxy-providers responses`
- `cargo test -p claude-proxy-providers`
- `cargo test`
- `cargo clippy -- -D warnings`
- `git diff --check`
- GitNexus `detect_changes`

After committing, run `npx gitnexus analyze`.

## GitNexus and commit requirements

Before editing each target symbol, run GitNexus impact analysis and warn if risk is HIGH or CRITICAL.

Before commit, run GitNexus detect changes and confirm affected symbols/flows match this design.

Commit the completed code changes and refresh the GitNexus index after the commit.
