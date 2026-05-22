# Changelog

## v1.1.2 - 2026-05-21

### Changed

- ChatGPT/Codex and Copilot `/responses` requests now omit `max_output_tokens` by default so only public OpenAI Responses requests keep that field.
- Removed the ChatGPT/Codex retry fallback for upstream `Unsupported parameter: max_output_tokens` responses; output limits are now handled by not sending that unsupported field on backends that do not use it.

## v1.1.1 - 2026-05-21

### Fixed

- ChatGPT/Codex requests now retry once without `max_output_tokens` when the upstream backend responds with `Unsupported parameter: max_output_tokens`. Backends that support the field still receive it on the first request, preserving output-budget control where available.
- ChatGPT upstream error mapping now reads top-level `detail` messages in addition to OpenAI-style `error.message`, so unsupported-parameter failures surface as clearer client errors if retry fallback cannot recover.

## v1.1.0 - 2026-05-21

This release focuses on ChatGPT/Codex compatibility, output-budget resilience, and safer production diagnostics. It is backward compatible with v1.0.5.

### Added

- ChatGPT/Codex request metadata parity for `/responses` requests: Codex-style `originator`, `User-Agent`, `session-id`, `thread-id`, `x-codex-window-id`, optional `ChatGPT-Account-Id`, and native `client_metadata`.
- ChatGPT requests now default to Codex-style `originator` and `User-Agent`, using local `codex --version` when available and a nonzero fallback otherwise; `originator` and `user_agent` remain available as explicit overrides under `[providers.<name>.chatgpt]`.
- Responses custom tool-call parity: `custom_tool_call` output items and `response.custom_tool_call_input.delta` / `.done` stream events are converted into Anthropic-compatible `tool_use` events.
- Native-shape ChatGPT/Codex fixtures for request bodies, headers, successful SSE, incomplete SSE, failed SSE, rate-limit SSE, function tool-call SSE, and custom tool-call SSE.
- Prompt-content-free ChatGPT upstream observability logs for request identity, upstream request/response ids, upstream model headers, terminal SSE stop reasons, rate-limit summaries, request body size, and requested/effective output-token budgets.
- Advanced Codex parity decision notes for turn-state replay, Responses WebSocket transport, FedRAMP/residency routing, and account-specific routing.

### Changed

- Fresh oversized tool results are now bounded with head/tail retention before being forwarded to Responses, reducing the chance that one large tool output breaks a Claude Code turn.
- ChatGPT Responses `max_output_tokens` is clamped against known model output limits, including the common `gpt-5.4-mini` 16,384-token ceiling.
- ChatGPT upstream output-limit failures are normalized into clearer Anthropic-compatible `max_tokens` guidance while preserving the correct 400/413 response classes.
- Workspace package metadata now points to the actual repository, `https://github.com/MorseWayne/claude-proxy`.

### Notes

- Responses WebSocket transport is intentionally not enabled in this release. The audited Codex implementation treats WebSocket as a separate provider capability, so this release keeps the HTTP SSE path stable.
- No private FedRAMP or residency headers are invented. Residency routing will be added only after an upstream contract is known.
