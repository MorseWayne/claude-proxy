# Changelog

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
