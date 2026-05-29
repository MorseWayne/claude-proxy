# Changelog

## v2.0.1 - 2026-05-29

### Fixed in v2.0.1

- Fixed pasting into Windows TUI edit inputs, including provider API key/base URL/proxy fields, by enabling bracketed paste handling and inserting paste payloads into the active input overlay.

## v2.0.0 - 2026-05-28

### Fixed in v2.0.0

- Surface ChatGPT `context_length_exceeded` SSE/WebSocket error frames as request-too-large failures, returning HTTP 413 for non-stream aggregate requests instead of retry-amplifying them as generic 502 Bad Gateway responses.
- Treat Responses `type: "error"` stream frames as provider errors so upstream prompt/context failures do not get converted into malformed successful streams.
- Reduce false `prefix_mismatch` WebSocket continuation misses by allowing safe delta inference when clients omit or reformat cached assistant/function-call output while preserving the cached user/tool-input prefix check.
- Shorten cached ChatGPT WebSocket connection idle reuse from 300s to 60s to avoid stale connection reuse and Broken pipe fallback after longer idle gaps.

### Changed in v2.0.0

- ChatGPT continuation diagnostics should now remain near small delta payloads after reconnect/redeploy, with full-history fallback reserved for true prefix/body/account/model mismatches.
- Request observability for overlong ChatGPT prompts records provider/request-too-large failures instead of stream/network failures where the upstream error is classifiable.

## v1.3.10 - 2026-05-28

### Fixed in v1.3.10

- Bound ChatGPT WebSocket continuation state to concrete cached connection IDs to prevent stale `previous_response_id` reuse under concurrent sessions.
- Invalidated continuation state when reused WebSocket connections fail, abort, or cannot be re-cached.
- Disabled delta continuation on new WebSocket connections and improved continuation/cache diagnostics with connection IDs.

### Changed in v1.3.10

- Raised default stream idle timeout from 120s to 300s for long `gpt-5.5` high-reasoning turns.
- Raised default tool-use terminal timeout from 30s to 120s.
- Added ChatGPT upstream/downstream thinking stream diagnostics and request IDs on stream watchdog logs.

## v1.3.9 - 2026-05-28

### Changed in v1.3.9

- Added ChatGPT stable synthetic session fallback so WebSocket continuation and prompt cache keys are present even when clients omit explicit session metadata.
- Improved ChatGPT WebSocket continuation caching for reasoning responses and terminal events without output payloads, allowing follow-up turns to send `previous_response_id` plus delta input instead of full history.
- Added continuation send-body byte diagnostics to show the actual upstream WebSocket payload size after delta compaction.

## v1.3.8 - 2026-05-28

### Changed in v1.3.8

- Updated ChatGPT/Codex Spark metadata to use its observed 128k context window.
- Made ChatGPT SSE response-header waits shorter and disabled default ChatGPT 429 retries to avoid hiding account/quota limit responses behind provider retries.

## v1.3.7 - 2026-05-28

### Changed in v1.3.7

- Removed the ChatGPT SSE/HTTP `previous_response_id` continuation reuse added in v1.3.6; continuation remains limited to the WebSocket transport.
- Removed proactive pre-send ChatGPT request compaction; prompt-too-long shrinking now only happens after an upstream prompt-too-long response.
- Relaxed ChatGPT model capability metadata for sampling and stop-sequence parameters so clients are not rejected before provider-side request normalization.

## v1.3.6 - 2026-05-27

### Added in v1.3.6

- Added a ChatGPT tool-schema budget guard that fails fast with an actionable ToolSearch hint before oversized tool catalogs are sent upstream.

### Changed in v1.3.6

- ChatGPT SSE/HTTP requests can now reuse safe `previous_response_id` continuation state and send only delta input when WebSocket transport is unavailable.
- ChatGPT request bodies that approach the known model context window are proactively compacted before the first upstream send, reducing avoidable prompt-too-long retries.

## v1.3.5 - 2026-05-27

### Added in v1.3.5

- Added prompt-content-free ChatGPT/OpenAI request payload diagnostics for tool count, total tool schema bytes, largest tool schema, and top tool schema contributors.

### Changed in v1.3.5

- Claude Code settings sync now defaults `ENABLE_TOOL_SEARCH=true` for proxy-backed sessions, including during `claude-proxy server start`, while preserving an explicit user override.
- Improved Anthropic streaming correctness by decoding complete SSE frames before forwarding events.
- Added anti-buffering headers to SSE responses to discourage intermediary response buffering.

## v1.3.4 - 2026-05-27

### Added in v1.3.4

- Added configurable SSE stream safeguards for Claude Code long sessions, including heartbeat, idle timeout, overall timeout, and conservative unfinished `tool_use` timeout handling.
- Added prompt-content-free active stream diagnostics in `/admin/metrics` for currently open streams.

### Changed in v1.3.4

- Improved Anthropic and Copilot provider streaming idle handling so stalled upstream streams fail with explicit timeouts instead of hanging indefinitely.

## v1.3.3 - 2026-05-27

### Removed in v1.3.3

- Removed the `claude-proxy completions` command and shell-completion documentation.

## v1.3.2 - 2026-05-27

### Added in v1.3.2

- Added `claude-proxy clean` to remove local log files and the persisted `metrics.db` SQLite database files, with daemon-running protection by default.
- Added `claude-proxy logs` to stream the active or explicitly selected log file in real time from the terminal.

## v1.3.1 - 2026-05-27

### Fixed in v1.3.1

- ChatGPT WebSocket connections now honor `HTTPS_PROXY` / `ALL_PROXY` when no provider proxy is configured, while preserving provider proxy priority and `NO_PROXY` bypass behavior.

## v1.3.0 - 2026-05-26

### Added in v1.3.0

- Added dedicated ChatGPT/Codex model capability metadata and richer Responses request options for Codex-style requests.
- Added configurable ChatGPT transport selection with Responses WebSocket support, automatic SSE fallback before the first upstream event, and completed WebSocket connection reuse.
- Added WebSocket-only ChatGPT continuation support using safe `previous_response_id` delta input when the request body, account, model, and stable client conversation key all match.
- Added continuation regression coverage for prefix/body/account mismatches, terminal failures, downstream aborts, same-key busy overlaps, auto-transport fallback, and function-call/tool-result deltas.

### Changed in v1.3.0

- Improved ChatGPT/Codex prompt cache key generation to avoid unsafe shared thread-id fallbacks and prefer stable request-scoped sources.
- Improved cached-token usage accounting so cache reads and writes are reflected consistently in provider metrics, admin totals, and the TUI.
- Improved ChatGPT WebSocket startup behavior to honor configured HTTP proxies and extra CA certificates where supported.

## v1.2.1 - 2026-05-24

### Added in v1.2.1

- Added canonical model capability contract metadata for internal routing and compatibility validation.

### Changed in v1.2.1

- Updated local tooling ignore rules to keep CI/build outputs out of version control and simplify release preparation.

## v1.2.0 - 2026-05-22

### Added in v1.2.0

- Added provider runtime policy controls for OpenAI/ChatGPT upstream retries, timeouts, extra request configuration, and OpenAI request options while preserving prior defaults.
- Added provider-side streaming metadata capture so OpenAI Chat Completions, OpenAI Responses, and ChatGPT Responses final usage/model/request metadata can be recorded without changing Anthropic-compatible SSE output.
- Added safe upstream diagnostics for provider errors, including upstream status, request id, retry-after, and provider health metadata.
- Added explicit ChatGPT/Copilot provider login flow and non-interactive server-mode auth-needed errors when ChatGPT tokens are missing.
- Added provider-aware model cache status to admin metrics and `POST /admin/models/refresh` for explicit cache refreshes.
- Added OpenAI Responses request alignment for safe `service_tier`, `prompt_cache_key`, boolean `parallel_tool_calls`, and supported `text.verbosity` options.

### Changed in v1.2.0

- Improved OpenAI/ChatGPT streaming token accounting so persisted usage can include final provider usage metadata.
- Improved ChatGPT request-size diagnostics near known context limits without blocking requests solely on estimation.
- Improved non-streaming Responses conversion coverage for reasoning summaries, refusal content, and incomplete `max_tokens` stop reasons.

## v1.1.2 - 2026-05-21

### Changed in v1.1.2

- ChatGPT/Codex and Copilot `/responses` requests now omit `max_output_tokens` by default so only public OpenAI Responses requests keep that field.
- Removed the ChatGPT/Codex retry fallback for upstream `Unsupported parameter: max_output_tokens` responses; output limits are now handled by not sending that unsupported field on backends that do not use it.

## v1.1.1 - 2026-05-21

### Fixed in v1.1.1

- ChatGPT/Codex requests now retry once without `max_output_tokens` when the upstream backend responds with `Unsupported parameter: max_output_tokens`. Backends that support the field still receive it on the first request, preserving output-budget control where available.
- ChatGPT upstream error mapping now reads top-level `detail` messages in addition to OpenAI-style `error.message`, so unsupported-parameter failures surface as clearer client errors if retry fallback cannot recover.

## v1.1.0 - 2026-05-21

This release focuses on ChatGPT/Codex compatibility, output-budget resilience, and safer production diagnostics. It is backward compatible with v1.0.5.

### Added in v1.1.0

- ChatGPT/Codex request metadata parity for `/responses` requests: Codex-style `originator`, `User-Agent`, `session-id`, `thread-id`, `x-codex-window-id`, optional `ChatGPT-Account-Id`, and native `client_metadata`.
- ChatGPT requests now default to Codex-style `originator` and `User-Agent`, using local `codex --version` when available and a nonzero fallback otherwise; `originator` and `user_agent` remain available as explicit overrides under `[providers.<name>.chatgpt]`.
- Responses custom tool-call parity: `custom_tool_call` output items and `response.custom_tool_call_input.delta` / `.done` stream events are converted into Anthropic-compatible `tool_use` events.
- Native-shape ChatGPT/Codex fixtures for request bodies, headers, successful SSE, incomplete SSE, failed SSE, rate-limit SSE, function tool-call SSE, and custom tool-call SSE.
- Prompt-content-free ChatGPT upstream observability logs for request identity, upstream request/response ids, upstream model headers, terminal SSE stop reasons, rate-limit summaries, request body size, and requested/effective output-token budgets.
- Advanced Codex parity decision notes for turn-state replay, Responses WebSocket transport, FedRAMP/residency routing, and account-specific routing.

### Changed in v1.1.0

- Fresh oversized tool results are now bounded with head/tail retention before being forwarded to Responses, reducing the chance that one large tool output breaks a Claude Code turn.
- ChatGPT Responses `max_output_tokens` is clamped against known model output limits, including the common `gpt-5.4-mini` 16,384-token ceiling.
- ChatGPT upstream output-limit failures are normalized into clearer Anthropic-compatible `max_tokens` guidance while preserving the correct 400/413 response classes.
- Workspace package metadata now points to the actual repository, `https://github.com/MorseWayne/claude-proxy`.

### Notes

- Responses WebSocket transport is intentionally not enabled in this release. The audited Codex implementation treats WebSocket as a separate provider capability, so this release keeps the HTTP SSE path stable.
- No private FedRAMP or residency headers are invented. Residency routing will be added only after an upstream contract is known.
