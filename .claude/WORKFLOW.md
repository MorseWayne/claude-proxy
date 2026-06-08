# 工作流台账（Workflow Ledger）

用于记录 Claude Code 开发工作的轻量级里程碑台账，便于跨会话恢复和追踪。

## Active（进行中）

### WF-2026-05-28-003 — v2.0 deep quality/performance audit
Status: In Progress
Level: 2
Started: 2026-05-28
Last updated: 2026-05-28
Current phase: Responses conversion decomposition validated

Intent:
- Deep-dive claude-proxy 2.0 code details and improve code quality, performance, and resource usage with lean, targeted validation.

Current todo:
- [x] Establish clean baseline and validation commands.
- [x] Identify concrete optimization targets from code metrics and code review.
- [x] Review affected call paths before editing any selected symbol.
- [x] Implement the first low-risk/high-value improvement and validate.
- [x] Continue with the next optimization target from clippy/metric hotspots.
- [x] Assess ChatGPT too-many-arguments refactor risk before editing.
- [x] Review accumulated diff and decide next hotspot or commit boundary.
- [x] Decompose ChatGPT upstream event handler into focused responsibilities.
- [x] Decide whether to proceed with CRITICAL-risk Responses conversion decomposition.
- [x] Decompose `responses.rs::append_message_items` into focused block/text/tool helpers.
- [ ] Review Responses decomposition diff and choose next hotspot.

Changes:
- Baseline: `cargo check --workspace` passed; repo root was clean before this audit.
- Hotspot scan found largest runtime areas in ChatGPT/Responses/server routes/TUI and clippy allocation warnings.
- GitNexus impact for `inject_cache_control` was LOW (0 direct callers/processes reported by index).
- Optimized Anthropic cache-control injection to move string prompt content into wrapped text blocks instead of cloning large system/latest-user strings, and added regression coverage for string prompt wrapping.
- Removed five redundant CLI/TUI clones by moving owned provider defaults and OAuth result payloads directly into their consumers.
- Refactored ChatGPT stream callback setup and WebSocket completion storage to use small context structs, removing the remaining clippy `too_many_arguments` warnings without changing transport behavior.
- Further decomposed ChatGPT upstream stream-event handling: introduced `ChatGptUpstreamEventHandlerState` with focused methods for first-event logging, reasoning-delta accounting, rate-limit caching, observer notification, and terminal-event handling. `upstream_event_handler` now only builds state and returns the callback.
- Committed the first cleanup/refactor batch as `12680bd` (`refactor provider cleanup hotspots`).
- Decomposed `responses.rs::append_message_items` into focused helpers: text message append, block iteration, single-block dispatch, function-call append, and function-call-output append. Behavior is intended to be unchanged.

Validation:
- `cargo test -p claude-proxy-providers inject_cache_control` passed.
- `cargo test -p claude-proxy-providers anthropic_` passed.
- `cargo clippy -p claude-proxy-providers --lib -- -W clippy::redundant_clone` no longer reports Anthropic redundant-clone warnings; remaining warnings are pre-existing ChatGPT too-many-arguments.
- `cargo fmt --all --check` and `git diff --check` passed.
- GitNexus detect_changes scope all after Anthropic change: LOW risk, no affected processes.
- GitNexus impact before CLI/TUI edits: `handle_provider` LOW (1 direct caller, 2 affected process roots), `poll_oauth` LOW (1 direct caller, 2 affected process roots).
- `cargo clippy -p claude-proxy-cli --bin claude-proxy -- -W clippy::redundant_clone` no longer reports CLI/TUI redundant-clone warnings; remaining warnings are pre-existing provider ChatGPT too-many-arguments.
- `cargo test -p claude-proxy-cli --bin claude-proxy` passed.
- `cargo fmt --all --check` and `git diff --check` passed after CLI/TUI cleanup.
- GitNexus detect_changes scope all after CLI/TUI cleanup: HIGH due central CLI provider/TUI flows, expected for touched `handle_provider`; changed code is move-only clone removal.
- GitNexus impact before ChatGPT context-struct refactor: `upstream_event_handler` LOW, `open_websocket_stream` LOW (index reported 0 callers/processes for both target lookups).
- `cargo clippy -p claude-proxy-providers --lib -- -W clippy::too_many_arguments` passed with no warnings.
- `cargo test -p claude-proxy-providers chatgpt_` passed.
- `cargo clippy --workspace -- -W clippy::redundant_clone -W clippy::too_many_arguments` passed with no warnings.
- Final `cargo fmt --all --check` and `git diff --check` passed.
- GitNexus detect_changes scope all after first ChatGPT context refactor: CRITICAL due central ChatGPT provider/WebSocket and CLI flows, expected for signature/context refactors; targeted ChatGPT and CLI tests passed.
- GitNexus impact before deeper event-handler decomposition: `upstream_event_handler` LOW.
- `cargo test -p claude-proxy-providers chatgpt_` passed after event-handler state extraction.
- `cargo clippy -p claude-proxy-providers --lib -- -W clippy::too_many_arguments -W clippy::too_many_lines` shows `upstream_event_handler` no longer as too-long; remaining relevant provider warnings are broader transport and `send_responses_request_with_prompt_too_long_retry`.
- Final `cargo fmt --all --check` and `git diff --check` passed.
- Final GitNexus detect_changes scope all before commit `12680bd`: CRITICAL due central ChatGPT provider/WebSocket and CLI flows, expected for touched ChatGPT provider internals plus previous CLI changes.
- GitNexus impact before Responses conversion decomposition: `append_message_items` CRITICAL (27 impacted, 9 affected process roots, direct caller `convert_to_responses`). User acknowledged by asking to continue after commit.
- `cargo test -p claude-proxy-providers --lib test_convert_to_responses` passed (24 tests).
- `cargo test -p claude-proxy-providers --lib chatgpt_responses_body` passed (14 tests).
- `cargo clippy -p claude-proxy-providers --lib -- -W clippy::too_many_lines` shows `append_message_items` no longer among too-long functions; remaining relevant warnings are broader transport/stream functions and stream converter.
- `cargo fmt --all --check` and `git diff --check` passed after Responses decomposition.
- GitNexus detect_changes scope all after Responses decomposition: CRITICAL, expected because the Responses conversion path is central; changed file is only `responses.rs`.

Prerequisites:
- Preserve existing uncommitted/user work; current repo status is clean at claude-proxy root.

Resume next:
- Review the uncommitted `responses.rs` + ledger diff. Next likely hotspots: `responses.rs::flush_message_parts`, `chatgpt.rs::send_responses_request_with_prompt_too_long_retry`, or `chatgpt/transport.rs::open_websocket_stream`; inspect affected call paths before editing.

### WF-2026-05-28-002 — ChatGPT request context efficiency follow-up
Status: In Progress
Level: 2
Started: 2026-05-28
Last updated: 2026-05-28
Current phase: v2.0.0 release metadata prepared

Intent:
- Reduce repeated large ChatGPT upstream request payloads observed in `~/.config/claude-proxy/logs/claude-proxy.log`, prioritizing stable prompt cache / WebSocket continuation keys and better payload observability.

Current todo:
- [x] Analyze live ChatGPT payload logs and identify dominant inefficiencies.
- [x] Review affected ChatGPT request paths before editing.
- [x] Implement stable synthetic conversation id fallback and request-id-aware payload stats.
- [x] Validate with focused tests and `git diff --check`.
- [x] Add safe continuation fallback for synthetic session-id drift observed in live logs.

Changes:
- ChatGPT requests without explicit stable session metadata now synthesize a deterministic `client_session_id` from model, system prompt, and first user message. This makes `prompt_cache_key` and WebSocket continuation keys present instead of `none` / `missing_key` for clients that do not pass session headers.
- Existing explicit conversation/session ids are preserved.
- Provider request payload stats now include optional `request_id` fields; ChatGPT passes the per-request id so payload stats can be correlated with upstream/transport logs.
- Validation passed: `cargo fmt --all --check`, targeted synthetic-session tests, `cargo test -p claude-proxy-providers chatgpt_`, `cargo test -p claude-proxy-providers openai_compat::tests`, `git diff --check`, and GitNexus detect_changes (LOW, no affected processes).
- Live validation after this change showed prompt cache/continuation keys are present, but `continuation_used=false` with `missing_session` across follow-up turns; likely synthetic id drift because the first user message in compacted Claude Code requests is not stable. User approved a safe prefix-match fallback.
- WebSocket continuation fallback now applies only to synthetic `cp-synth-*` ids. On exact-key miss, it searches same provider/account/model/schema cached continuations, ignores only `prompt_cache_key` in canonical body comparison, requires cached full input + assistant output to prefix-match current input, and logs `continuation_synthetic_fallback_used`.
- Second live validation still showed `missing_session`; root cause is likely terminal cache storage being rejected when ChatGPT output contains `reasoning` items. Updated terminal assistant output extraction to skip `reasoning` items while still rejecting unsupported custom tool calls, so successful reasoning responses can populate the continuation cache.
- Third live validation still showed `missing_session`; root cause is likely terminal events that contain response id/status but omit `response.output`. Updated continuation caching to allow empty terminal output, then infer/skip the previous assistant/function-call prefix from the next request before computing delta.
- Final live validation succeeded: follow-up requests showed `continuation_used=true`, `continuation_disabled_reason="none"`, and `continuation_delta_items=1` while full logical input was 71/73 items, confirming upstream WebSocket sends only the delta.
- Added `continuation_send_body_bytes` to the WebSocket continuation decision log so live logs show the actual JSON body size sent after applying continuation/delta input.
- Validation passed after fallback/reasoning/empty-output/log-size fix: `cargo fmt --all --check`, `cargo test -p claude-proxy-providers chatgpt_`, `cargo test -p claude-proxy-providers continuation_`, `git diff --check`, and GitNexus detect_changes (HIGH, expected because `open_websocket_stream` / continuation flow is intentionally affected).
- Overlong context handling now surfaces ChatGPT `context_length_exceeded` SSE/WebSocket error events as `ProviderError::RequestTooLarge`, maps non-stream aggregate failures to HTTP 413 instead of generic 502, and records provider-error/request-too-large metrics. Focused validation passed: targeted ChatGPT mapping tests, Responses SSE error-event test, server request-too-large tests, `cargo test -p claude-proxy-providers --lib chatgpt_`, `cargo test -p claude-proxy-providers --lib responses::tests::test_stream_response`, `cargo test -p claude-proxy-server --lib provider_error`, `cargo fmt --all --check`, `git diff --check`; GitNexus detect_changes reports HIGH expected due central stream/error paths.
- Recent live logs showed 2/18 continuation decisions degraded to `prefix_mismatch`, sending ~690KB full bodies, and one stale WebSocket Broken pipe after ~94s idle. Continuation delta now falls back to inferred assistant-prefix matching even when terminal output was cached but the client omits/reformats assistant output; true divergent prefixes still send full input. WebSocket cached-session idle TTL was reduced from 300s to 60s to avoid reusing stale connections. Validation passed: targeted continuation delta/prefix-mismatch tests, `cargo test -p claude-proxy-providers --lib chatgpt_`, `cargo test -p claude-proxy-providers --lib responses::tests::test_stream_response`, `cargo test -p claude-proxy-server --lib provider_error`, `cargo fmt --all --check`, `git diff --check`; GitNexus detect_changes reports CRITICAL expected due central `open_websocket_stream` plus existing stream/error patches.
- Prepared v2.0.0 release metadata as a major release for ChatGPT overlong-context/continuation reliability: bumped workspace packages and lockfile to 2.0.0 and added CHANGELOG release notes. Validation passed: `cargo metadata --no-deps --format-version 1`, `cargo fmt --all --check`, and `git diff --check`.

Prerequisites:
- User approved applying the recommended optimizations after log analysis showed prompt cache and continuation were always disabled by missing stable conversation ids.

Resume next:
- Commit and tag v2.0.0 release metadata, then push main and tag.

### WF-2026-05-28-001 — Claude Code quality gate capability rollout
Status: In Progress
Level: 2
Started: 2026-05-28
Last updated: 2026-05-28
Current phase: Low-risk capability infrastructure and provider diagnostics

Intent:
- Turn the third-party provider quality-gates research into implementation: add explicit quality gate capability metadata, populate provider/model mappings, expose diagnostics, and begin capability-driven request/conversion safety.

Current todo:
- [x] Add `QualityGateCapabilities` to `ModelCapabilities` with serialization coverage.
- [x] Populate conservative provider mappings for Anthropic, ChatGPT, OpenAI compat, and Copilot.
- [x] Expose diagnostics in model capabilities/admin metrics and TUI.
- [x] Start request feature/conversion groundwork without changing request behavior beyond metadata/validation-safe checks.

Changes:
- GitNexus impact for `ModelCapabilities` is HIGH because the schema is central to provider metadata, server validation fixtures, and CLI/TUI display. Proceeded with a metadata-first, default-safe scope.
- Added `QualityGateCapabilities` plus tool-search, prompt-cache-scope, context-management, token-counting, and related capability enums to the core model capability payload.
- Provider mappings now expose conservative quality gate metadata for Anthropic, ChatGPT, OpenAI compat, and Copilot-discovered models.
- `/model_capabilities`, admin metrics, and TUI parsing/display now surface quality gate diagnostics.
- Request capability detection now covers reasoning effort, prompt cache key, service tier/fast mode, structured outputs, strict tools, token-efficient tools, and context management; explicitly unsupported quality gates are rejected while unknown gates remain allowed.
- Responses conversion now has `ConversionContext` and can clamp reasoning effort from `ModelInfo` metadata when provided.
- Validation passed: `cargo fmt --all --check`, focused core/provider/server/CLI tests, multi-crate `cargo test -p claude-proxy-core -p claude-proxy-providers -p claude-proxy-server -p claude-proxy-cli`, `cargo clippy -p claude-proxy-core -p claude-proxy-providers -p claude-proxy-server -p claude-proxy-cli -- -D warnings`, `git diff --check`, and GitNexus detect_changes (CRITICAL expected due central model schema / Responses conversion / server validation paths).

Prerequisites:
- User asked to implement the first three batches from the quality-gates implementation plan.

Resume next:
- Review the diff, decide whether to keep the unrelated pre-existing docs changes, then commit or continue with deeper capability-driven conversion controls.

### WF-2026-05-25-001 — ChatGPT/Codex provider modernization
Status: In Progress
Level: 3
Started: 2026-05-25
Last updated: 2026-06-07
Current phase: Pi 0.78.1 ChatGPT/Codex model catalog follow-up validated

Intent:
- Modernize ChatGPT/OpenAI provider integration using lessons from `/home/wayne/source/open/pi/packages/ai`: accurate ChatGPT/Codex capabilities, richer Responses options, safer prompt cache keys, usage accuracy, WebSocket transport with SSE fallback, and continuation/delta input.

Current todo:
- [x] Finalize design scope and implementation sequence before code changes.
- [x] Review the written design spec and address any issues.
- [x] Wait for user review of the committed design spec.
- [x] Review affected call paths before editing each target symbol.
- [x] Implement Phase 1 foundation: model/request/cache/usage correctness plus focused tests.
- [x] Validate Phase 1 with fmt and targeted provider tests before moving to WebSocket work.
- [x] Review Phase 1 diff and apply any reviewer fixes before starting WebSocket work.
- [x] Commit Phase 1.
- [x] Implement Phase 2 WebSocket transport with SSE fallback.
- [x] Review Phase 2 diff and address reviewer blockers.
- [x] Final focused reviewer pass found no remaining blockers.
- [x] Commit Phase 2.
- [x] Plan Phase 3 continuation/delta-input support.
- [x] Implement Phase 3 continuation/delta-input support.
- [x] Validate/review Phase 3.
- [x] Commit Phase 3.
- [x] Decide whether to add non-blocking continuation hardening tests.
- [x] Implement continuation hardening tests for busy overlap, abort invalidation, and function-call/tool-result delta.
- [x] Validate continuation hardening tests.
- [x] Commit continuation hardening tests.
- [x] Remove untracked review report artifacts.
- [x] Implement post-validation WebSocket startup fallback tuning: phase-aware diagnostics, short Auto cooldown, and lightweight WebSocket counters.
- [x] Implement ChatGPT WebSocket environment proxy fallback with NO_PROXY bypass.
- [x] Validate ChatGPT WebSocket environment proxy fallback and commit if checks pass.
- [ ] Prepare v1.3.0 release metadata and tag.
- [x] Implement agreed ChatGPT upstream payload optimizations: reuse continuation for SSE/HTTP where safe, proactively compact oversized ChatGPT bodies before send, and add a ChatGPT tool-schema budget guard.
- [x] Prepare v1.3.6 release metadata and changelog for ChatGPT payload optimizations.
- [x] Fix ChatGPT capability metadata so client sampling/stop parameters are tolerated while the provider continues stripping unsupported upstream fields.
- [x] Prepare v1.3.7 reduced-risk release metadata: remove v1.3.6 SSE/HTTP continuation and pre-send compaction, keep tool-schema guard.
- [x] Apply Pi 0.76-aligned ChatGPT follow-ups: Codex Spark 128k context metadata, shorter SSE header timeout, default no ChatGPT 429 retry, and session-id cache-affinity docs.
- [x] Align ChatGPT/Codex selectable model catalog with Pi 0.78.1 `openai-codex` availability.

Changes:
- User approved the "full bold" scope including WebSocket transport and continuation, not just low-risk capability/request fixes.
- Design spec committed in `c569751` at `docs/superpowers/specs/2026-05-25-chatgpt-codex-provider-modernization-design.md`.
- Spec review found missing boundaries for continuation keys, canonical comparison, WebSocket fallback errors, prompt cache/usage defaults, and model matrices; the design was revised to define them.
- Spec review passed after revision; latest design clarification commit is `a39e179`.
- Phase 1 foundation diff now adds dedicated ChatGPT/Codex model capabilities, richer Codex request options, request-scoped/stable prompt cache keys, normalized cached-token usage accounting, and focused tests.
- Validation so far: `cargo fmt --check`, ChatGPT/Responses provider tests, full `cargo test -p claude-proxy-providers`, `cargo test -p claude-proxy-server`, `cargo test -p claude-proxy-cli`, full `cargo test`, `cargo clippy -p claude-proxy-providers -p claude-proxy-server -p claude-proxy-cli -- -D warnings`, full `cargo clippy -- -D warnings`, and `git diff --check` passed.
- Reviewer fanout found no blockers; accepted one small maintainability cleanup to share Responses usage JSON emission.
- GitNexus detect_changes reports CRITICAL because the diff touches central ChatGPT request and Responses stream conversion paths; affected processes align with Phase 1 scope.
- Phase 1 foundation committed, and `npx gitnexus analyze` completed successfully.
- Phase 2 implementation adds configurable ChatGPT transport selection (`sse`/`websocket`/`auto`), a Responses WebSocket transport with `response.create`, OpenAI beta header, account/session headers, startup SSE fallback, post-first-event non-replay behavior, and completed-connection reuse.
- Phase 2 validation so far: `cargo fmt --check`, `cargo test -p claude-proxy-config`, ChatGPT/Responses provider tests, full `cargo test -p claude-proxy-providers`, full `cargo test`, `cargo clippy -- -D warnings`, and `git diff --check` passed.
- Phase 2 reviewer found blockers around abort/drop cleanup and honoring proxy/custom CA settings for WebSocket; fixes now add downstream-drop abort signaling, prompt upstream close on abort, proxy/extra-CA-aware WebSocket connection setup, and regression coverage for abort cleanup plus configured HTTP proxy routing.
- Final focused reviewer pass found no remaining blockers for abort/drop cleanup, proxy/custom-CA support, or connector correctness; noted non-blocking gaps around SOCKS/HTTPS proxy parity, proxy credential coverage, and custom-CA fixture coverage.
- After blocker fixes, validation passed again: `cargo fmt --check`, `cargo test -p claude-proxy-config`, `cargo test -p claude-proxy-providers chatgpt`, `cargo test -p claude-proxy-providers responses`, full `cargo test -p claude-proxy-providers`, full `cargo test`, full `cargo clippy -- -D warnings`, and `git diff --check`.
- GitNexus detect_changes reports HIGH; affected flows are expected ChatGPT provider/test-helper paths plus Responses stream metadata usage extraction.
- Phase 2 committed in `08cf57a` and GitNexus metadata refreshed afterward.
- Phase 3 implementation adds WebSocket-only continuation state keyed by provider/account/model/stable client conversation/schema, canonical non-input body comparison, delta input with `previous_response_id` on safe prefix match, and state cleanup on terminal failure, transport errors, and abort/drop.
- Phase 3 focused/full validation passed so far: `cargo fmt --check`, transport helper tests, ChatGPT WebSocket/auto/chatgpt tests, Responses tests, full `cargo test -p claude-proxy-providers`, full `cargo test`, full `cargo clippy -- -D warnings`, and `git diff --check`.
- GitNexus detect_changes for Phase 3 reports CRITICAL because the diff touches ChatGPT WebSocket transport/session flow and test helpers; affected flows align with planned ChatGPT continuation/WebSocket scope.
- Phase 3 reviewer found no blockers; non-blocking follow-ups are explicit busy/concurrency coverage, abort-state invalidation coverage, and e2e function-call/tool-result continuation coverage.
- Phase 3 committed in `5967589` and GitNexus metadata refreshed afterward.
- Follow-up hardening tests now cover same-key busy overlap invalidating in-flight continuation state, downstream abort clearing continuation state before the next request, and function-call/tool-result continuation sending only the tool-result delta.
- Reviewer found the initial busy-overlap test could pass via prefix mismatch; fixed by making the fourth request extend the stale in-flight transcript so a bad late cache update would send `previous_response_id: resp-ws-2` and fail.
- Hardening validation passed: `cargo fmt --check`, `cargo test -p claude-proxy-providers chatgpt_websocket_continuation`, `cargo test -p claude-proxy-providers chatgpt`, full `cargo test -p claude-proxy-providers`, full `cargo test`, full `cargo clippy -- -D warnings`, and `git diff --check`.
- Continuation hardening tests committed in `26d4893`; GitNexus metadata refreshed in `c0c480b`; final `gitnexus_detect_changes` reported no changes detected.
- Removed untracked `reports/` review/planning artifacts because their useful outcomes are already captured in commits and this ledger.
- Started release prep for v1.3.0 by bumping workspace package version and drafting changelog notes for the ChatGPT/Codex modernization.
- During live ChatGPT/Codex validation, upstream `server_error` reproduced on both WebSocket and SSE; added diagnostics plus a runtime-id recovery fix so SSE requests use per-request `x-client-request-id` and ChatGPT `server_error` rotates session/thread/window IDs and clears WebSocket volatile state.
- Compared `/home/quzhihao/workspace/source/open/pi/packages/ai/src/providers/openai-codex-responses.ts`: its WebSocket cache is session-scoped, marks entries busy, drops cache on errors, and does not use a provider-global reusable connection. Mirrored the safer direction by keying claude-proxy cached WebSocket reuse by provider/account/model/session/thread/window to prevent cross-model/account reuse.
- Applied additional pi-aligned stability tuning: ChatGPT/Codex model context metadata now uses 272k tokens, Codex reasoning summary defaults to `auto` for generated reasoning requests, and WebSocket `server_error` activates a short SSE cooldown so repeated retries do not immediately hit the same WebSocket failure path.
- Committed and pushed the ChatGPT/Codex stability fixes in `6fd905a`.
- Started the next stability round by enriching incoming `/v1/messages` requests with `client_session_id` from safe session headers when the request does not already carry stable session metadata; this lets ChatGPT/Codex derive prompt cache and continuation keys when clients provide a session header, without inventing a provider-global cross-session key.
- Post-validation WebSocket startup fallback tuning replaces provider-wide permanent Auto fallback with a 120s startup-failure SSE cooldown, adds phase-aware WebSocket startup errors, lightweight provider-local WebSocket counters, and prompt-cache/continuation presence logs without logging key values.
- Validation for the WebSocket tuning passed: `cargo fmt --check`, `cargo test -p claude-proxy-providers chatgpt_`, full `cargo clippy -- -D warnings`, and provider-file `git diff --check`.
- GitNexus detect_changes reports CRITICAL because the diff intentionally touches ChatGPT WebSocket transport/session and related test flows; affected processes align with the planned WebSocket fallback/diagnostics scope.
- Follow-up diagnosis found first WebSocket attempts consistently fail in `connect` phase when ChatGPT provider has no explicit proxy but the shell has `HTTPS_PROXY` / `ALL_PROXY`; the WebSocket path only used provider proxy while the HTTP/SSE path can still succeed.
- Implemented WebSocket proxy resolution so explicit provider proxy still wins, otherwise `HTTPS_PROXY` / `https_proxy` / `ALL_PROXY` / `all_proxy` are used unless `NO_PROXY` / `no_proxy` matches the target host; only HTTP proxy URLs are accepted, matching the existing CONNECT tunnel support.
- Added focused tests for env `HTTPS_PROXY` fallback, provider proxy overriding env proxy, and `NO_PROXY` bypass; env-proxy tests set loopback `NO_PROXY` while holding an async lock so concurrent local WebSocket tests are not polluted by process-wide proxy variables.
- Validation for the WebSocket env proxy fallback passed: `cargo fmt --check`, the three new target tests, `cargo test -p claude-proxy-providers chatgpt_`, and `cargo clippy -- -D warnings`.
- GitNexus detect_changes reports HIGH because the diff touches ChatGPT WebSocket connect/proxy paths and related test helpers; affected processes align with the intended WebSocket proxy resolution scope. The report also includes pre-existing `AGENTS.md` / `CLAUDE.md` metadata edits that are excluded from this fix commit.
- Implemented agreed post-v1.3.5 ChatGPT upstream payload optimizations: SSE/HTTP now shares safe continuation state and can send `previous_response_id` plus delta input, oversized ChatGPT request bodies are compacted before first send when they approach the model context window, and oversized tool catalogs fail fast with a ToolSearch hint.
- Validation for the payload optimization diff passed: `cargo fmt --check`, `cargo test -p claude-proxy-providers chatgpt_ -- --nocapture`, full `cargo test -p claude-proxy-providers`, and `git diff --check`.
- GitNexus detect_changes reports CRITICAL because the diff intentionally touches ChatGPT SSE/WebSocket continuation/session handling and prompt-too-long request shrinking; affected processes align with the planned ChatGPT payload optimization scope.
- Fixed compaction compatibility for ChatGPT/Codex models: capability metadata now reports sampling and stop_sequences as Unknown instead of Unsupported, so server capability validation tolerates client-supplied optional parameters that the ChatGPT Responses builder strips before sending upstream. Validation passed: `cargo fmt --all --check`, targeted ChatGPT model capability test, targeted Responses sampling-omission test, and GitNexus detect_changes (LOW).
- User agreed v1.3.6 payload optimizations had low benefit for their complexity; v1.3.7 now removes SSE/HTTP continuation reuse and proactive pre-send compaction, while preserving the safer tool-schema budget guard and the sampling/stop capability compatibility fix. Validation passed: `cargo fmt --all --check`, targeted ChatGPT capability/tool-schema/Responses sampling/transport/continuation tests, `cargo test -p claude-proxy-providers chatgpt_`, `git diff --check`, and GitNexus detect_changes (LOW, no affected processes).
- Applied Pi 0.76-aligned follow-ups after reviewing Pi release notes/source: ChatGPT/Codex Spark now reports 128k context, ChatGPT SSE response header timeout is 10s, ChatGPT default policy no longer retries 429 responses, README documents using Pi `--session-id` for stable cache affinity, and CHANGELOG has an Unreleased entry. Validation passed: `cargo fmt --all --check`, targeted ChatGPT model/policy tests, `cargo test -p claude-proxy-providers chatgpt_`, `git diff --check`, and GitNexus detect_changes (LOW, no affected processes). Pre-edit impact for `chatgpt_upstream_request_policy` was CRITICAL due central ChatGPT path, so the change was scoped to policy defaults and covered by focused tests.
- Pi 0.78.1 local `pi --list-models openai-codex` shows `gpt-5.5`, `gpt-5.4`, `gpt-5.4-mini`, and `gpt-5.3-codex-spark`; it warns that `openai-codex/gpt-5.3-codex` and `openai-codex/gpt-5.2` no longer match. Removed those unavailable models from ChatGPT provider selection and switched ChatGPT defaults/docs to `gpt-5.5`. Validation passed: `cargo fmt --all --check`, targeted ChatGPT model/context tests, `cargo test -p claude-proxy-providers chatgpt_`, `cargo test -p claude-proxy-config`, `git diff --check`; Rust review found no blockers.

Prerequisites:
- User has asked to start/continue implementation from the approved spec.

Resume next:
- Commit release metadata for v2.0.7, merge/push main, then push tag `v2.0.7` to trigger GitHub release workflow.

### WF-2026-05-20-007 — ChatGPT/Codex compatibility follow-ups
Status: Completed
Level: 3
Priority: Continue from Backlog after Codex request metadata baseline
Started: 2026-05-21
Last updated: 2026-05-21
Current phase: Closed

Intent:
- Improve ChatGPT/Codex compatibility beyond the baseline in `73648f4`, starting with output budget governance for oversized Claude Code responses.

Current todo:
- [x] Output budget guardrails: truncate oversized current tool output with head/tail retention, keep historical tool-output compression, and clamp ChatGPT Responses `max_output_tokens` to known model limits.
- [x] Output limit errors: add clearer Anthropic-compatible errors if upstream/client output-limit failures still surface after truncation.
- [x] Codex SSE parity: map `response.custom_tool_call_input.delta` / `.done` and `custom_tool_call` output items into Anthropic `tool_use` events with fixture coverage.
- [x] Compatibility presets: make `codex`, `opencode`, and `anthropic-bridge` request identity defaults explicit for originator, user agent, headers, and body metadata behavior.
- [x] Fixture tests: add snapshot fixtures from real/native Codex request body, headers, successful SSE, incomplete, failed, rate-limit, and tool-call streams.
- [x] Observability: expose upstream request id, model header, stop reason, rate-limit summary, body bytes, and requested/effective output token budget in structured logs or admin metrics without prompt content.
- [x] Advanced Codex parity: evaluate turn-state replay, WebSocket Responses transport, FedRAMP/residency routing headers, and account-specific routing only after the HTTP SSE path is stable.

Changes:
- Baseline completed in `73648f4`: ChatGPT `/responses` now sends Codex-style request defaults, stable runtime metadata, and session/thread/window headers.
- Added provider-neutral current tool-output head/tail truncation at 128 KiB so a fresh oversized Claude Code tool result does not get forwarded to Responses verbatim.
- Added ChatGPT Responses `max_output_tokens` clamping against known OpenAI model metadata, covering the common `gpt-5.4-mini` 16,384-token ceiling case.
- Validation: `cargo fmt --check`, targeted ChatGPT clamp test, targeted current tool-output truncation test, `cargo test -p claude-proxy-providers chatgpt`, `cargo test -p claude-proxy-providers responses::tests::test_convert_to_responses`, and full `cargo test -p claude-proxy-providers` passed.
- GitNexus: `build_body_with_context` / `build_body` impact LOW; generic `tool_result_output` and `convert_to_responses` impact CRITICAL, so the edit was kept to bounded truncation behavior with focused and provider-wide tests.
- Added Responses custom tool-call parity based on OpenAI's current `custom_tool_call` item and `response.custom_tool_call_input.delta` / `.done` streaming events: streaming deltas are escaped into an Anthropic-compatible `tool_use` input object as `{"input": "<freeform text>"}`.
- Validation: `cargo fmt --check`, `git diff --check`, custom-tool stream/non-stream target tests, Responses stream converter tests, non-streaming response tests, and full `cargo test -p claude-proxy-providers` passed.
- GitNexus: `ResponsesStreamConverter::process_event` impact CRITICAL because it is the streaming conversion entrypoint; related custom handling helpers and non-stream converter changes were LOW, and the edit was scoped to new `custom_tool_call` event/item branches.
- Added ChatGPT request identity presets: `opencode` preserves existing defaults; `codex` uses OpenAI Codex source's current `codex_cli_rs` originator; `anthropic-bridge` marks bridge traffic explicitly. Explicit `originator` / `user_agent` overrides still win.
- ChatGPT Responses body metadata now records `x-claude-proxy-identity-preset` in `client_metadata` when the provider path supplies a preset.
- Validation: `cargo fmt --check`, config ChatGPT preset tests, ChatGPT provider header/body tests, `cargo test -p claude-proxy-config`, `cargo test -p claude-proxy-providers chatgpt`, full `cargo test -p claude-proxy-providers`, and `cargo clippy -p claude-proxy-config -p claude-proxy-providers -- -D warnings` passed.
- GitNexus: `ChatGptProviderConfig`, `chatgpt_request_headers`, `build_body_with_context`, `apply_codex_metadata`, `ChatGptProvider::new`, and `ChatGptProvider::chat_with_observer` impact LOW.
- Added sanitized native-shape ChatGPT/Codex fixtures for request body, request identity headers, successful SSE, incomplete SSE, failed SSE, rate-limit SSE, function tool-call SSE, and custom tool-call SSE.
- Validation: `cargo fmt --check`, native Codex fixture target tests, existing ChatGPT Codex tool fixture test, and full `cargo test -p claude-proxy-providers` passed.
- GitNexus: fixture-covered request body/header symbols were LOW; `ResponsesStreamConverter::process_event` impact is CRITICAL as the central Responses streaming conversion entrypoint, so this phase only added fixture coverage and did not change streaming conversion behavior.
- Added prompt-content-free ChatGPT structured logs for request identity, upstream request/response ids, upstream model header, terminal SSE stop reason, response header / stream rate-limit summaries, body bytes, and requested/effective output token budgets.
- Validation: `cargo fmt --check`, ChatGPT observability target tests, output-token clamp budget test, full `cargo test -p claude-proxy-providers`, and `cargo clippy -p claude-proxy-providers -- -D warnings` passed.
- GitNexus: `send_responses_request`, `ChatGptProvider::chat_with_observer`, `rate_limit_snapshots_from_headers`, and `rate_limit_snapshot_from_sse_event` impact LOW.
- Added ChatGPT provider-side output-limit error normalization: upstream `max_output_tokens` / `max_tokens` limit failures now surface as clear Anthropic-compatible `max_tokens` guidance while preserving 400 vs 413 response classes.
- Validation: `cargo fmt --check`, output-limit target tests, prompt-too-long detection regression, full `cargo test -p claude-proxy-providers`, and `cargo clippy -p claude-proxy-providers -- -D warnings` passed.
- GitNexus: `map_chatgpt_error_status_body` impact LOW; generic server error response functions were HIGH, so this phase avoided changing the `/v1/messages` response shell.
- Added Advanced Codex parity decision record in [2026-05-21-chatgpt-advanced-codex-parity.md](../docs/superpowers/plans/2026-05-21-chatgpt-advanced-codex-parity.md): keep HTTP SSE stable; defer Codex App Server turn replay, Responses WebSocket transport, and residency routing until upstream/client requirements are concrete; retain existing automatic `ChatGPT-Account-Id` forwarding.
- Validation: `git diff --check` passed; Rust tests were not run because this closeout only changes docs/workflow notes.

Prerequisites:
- None

Resume next:
- None. Workflow is closed; reopen only if a new upstream Codex transport or lifecycle requirement appears.

## Backlog / Future（待办 / 未来）

- [ ] 如果 OpenAI/Copilot Responses 上游开始强制要求 `instructions`，再评估是否需要 provider-specific 处理。
- [ ] 清理 provider-neutral Responses 抽取相关历史待办：当前 [responses.rs](crates/claude-proxy-providers/src/responses.rs) 已完成解耦，后续只需补测试或文档。

## Completed（已完成）

### WF-2026-06-08-003 — ChatGPT/Codex standalone tools conversion
Completed: 2026-06-08
Level: 3

Close summary:
- Outcome: Added default-on ChatGPT/Codex standalone tools conversion with `chatgpt.standalone_tools` rollback, converts compatible `Bash` tool definitions to Responses `custom` freeform tools, preserves function fallback for structured/unknown tools, adapts custom Bash outputs back to Claude Code `command` input, and preserves custom tool calls in WebSocket continuation prefix handling.
- Validation: Passed focused ChatGPT config/body/custom-tool/continuation/Responses conversion tests, `cargo clippy -p claude-proxy-config -p claude-proxy-providers -- -D warnings`, `cargo fmt --all --check`, `git diff --check`, and GitNexus impact/detect_changes with expected CRITICAL risk on core Responses conversion paths.
- Gaps: Only `Bash` has confirmed request-side `custom` conversion in this slice; other common structured tools intentionally fall back to function tools until field-preserving native schemas are proven.

Archived execution:
- Intent: Default-enable a thorough ChatGPT/Codex standalone/custom tools conversion while preserving config and per-tool fallback to function tools.
- Plan:
  - [done] P1 — Write and review standalone tools design spec.
  - [done] P2 — Add default-on ChatGPT standalone tools config and conversion mode boundary.
  - [done] P3 — Implement standalone/custom tool definition conversion with safe fallback.
  - [done] P4 — Validate tool history, tool_choice, stream converter, and WebSocket continuation compatibility.
  - [done] P5 — Run focused tests, GitNexus checks, and commit implementation.
- Key changes:
  - `chatgpt.standalone_tools` defaults to true and can be set false to restore the old function-tool request shape.
  - ChatGPT/Codex conversion mode is scoped through `ConversionContext`; generic Responses conversion remains function-tool by default.
  - `Bash` tools with a string `command` schema emit Responses `type: custom` with a Lark freeform grammar; `Read` and other structured tools remain function fallback.
  - Stream and non-streaming converters map custom `Bash` input back to `{"command": ..., "description": ""}` while preserving generic `{"input": ...}` for unknown custom tools.
  - WebSocket continuation now preserves supported `custom_tool_call` assistant output prefix items and can infer deltas after custom tool calls.
- Validation:
  - `cargo test -p claude-proxy-config chatgpt`
  - `cargo test -p claude-proxy-providers --lib chatgpt_responses_body`
  - `cargo test -p claude-proxy-providers --lib custom_tool`
  - `cargo test -p claude-proxy-providers --lib continuation`
  - `cargo test -p claude-proxy-providers --lib test_convert_to_responses`
  - `cargo clippy -p claude-proxy-config -p claude-proxy-providers -- -D warnings`
  - `cargo fmt --all --check`, `git diff --check`, GitNexus impact/detect_changes.
- Deferred / gaps:
  - Expand native mappings for `Read`, `Edit`, `Write`, `Grep`, `WebFetch`, and other structured tools after confirmed request-side schemas exist.

### WF-2026-06-08-002 — Responses Lite dashboard metrics follow-up
Completed: 2026-06-08
Level: 2

Close summary:
- Outcome: Added low-cardinality observability summary counters for Responses Lite, WebSocket transport, continuation-used requests, and continuation saved bytes in admin metrics and the TUI dashboard.
- Validation: Passed focused server/TUI observability tests, `cargo fmt --all --check`, `git diff --check`, `cargo clippy -p claude-proxy-server -p claude-proxy-cli -- -D warnings`, and GitNexus detect_changes with expected HIGH impact on dashboard render/metrics parsing flows.
- Gaps: None for the approved dashboard metrics slice.

Archived execution:
- Intent: Surface Responses Lite and continuation savings data already collected by the transport/observability layer through `/admin/metrics` and the TUI dashboard.
- Plan:
  - [done] P1 — Implement admin observability summary counters.
  - [done] P2 — Implement TUI metrics parsing for new summary fields.
  - [done] P3 — Render compact dashboard observability display.
  - [done] P4 — Add focused parser/rendering/server persistence tests.
  - [done] P5 — Validate, run impact/change checks, and commit.
- Key changes:
  - Admin observability summaries now aggregate Responses Lite, WebSocket, continuation-used, and continuation saved-byte totals for session and stored metrics.
  - TUI parsing defaults missing new summary fields to zero for compatibility with older servers.
  - Dashboard prefers stored observability totals when available and shows a compact `Resp Lite` row with WS, continuation, and saved-byte values.
- Validation:
  - Focused server/TUI observability tests, formatting, clippy, diff whitespace check, and GitNexus detect_changes.
- Deferred / gaps:
  - None.

### WF-2026-06-08-001 — Codex Responses Lite compatibility and observability follow-up
Completed: 2026-06-08
Level: 2

Close summary:
- Outcome: Added default-on ChatGPT/Codex Responses Lite transport markers for HTTP and WebSocket request payloads, persisted `responses_lite` plus `continuation_saved_bytes`, and made stderr ANSI conditional on terminal detection while file logs remain ANSI-free.
- Validation: Passed focused config/provider/server/CLI tests, `cargo test -p claude-proxy-providers chatgpt_`, `cargo clippy -p claude-proxy-config -p claude-proxy-providers -p claude-proxy-server -p claude-proxy-cli -- -D warnings`, `cargo fmt --all --check`, `git diff --check`, and GitNexus detect_changes with expected CRITICAL observability/WebSocket blast radius.
- Gaps: Full standalone tools conversion and dashboard/TUI surfacing are deferred follow-ups.

Archived execution:
- Intent: Align ChatGPT/Codex transport with recent Codex Responses Lite protocol markers and add low-risk observability for payload savings/log analysis.
- Plan:
  - [done] P1 — Finalize design/spec approval before implementation.
  - [done] P2 — Implement Responses Lite HTTP/WebSocket markers and observer metadata.
  - [done] P3 — Persist `responses_lite` and `continuation_saved_bytes` observability fields with compatible migrations.
  - [done] P4 — Make file logs ANSI-free / avoid ANSI in non-terminal stderr.
  - [done] P5 — Validate focused tests, format/lint scope, update ledger, and commit.
- Key changes:
  - User selected the conservative batch scope and approved the design spec `docs/superpowers/specs/2026-06-08-codex-responses-lite-observability-design.md`.
  - `chatgpt.responses_lite` defaults to true and controls the HTTP `x-openai-internal-codex-responses-lite` header plus WebSocket `client_metadata.ws_request_header_x_openai_internal_codex_responses_lite` marker.
  - Request observability now carries and persists Responses Lite state and saturating continuation saved bytes.
- Validation:
  - Focused tests covered config parsing, HTTP header on/off, WebSocket metadata, observability summary/persistence/legacy migration, logging tests, provider ChatGPT subset, fmt, clippy, diff check, and GitNexus detect_changes.
- Deferred / gaps:
  - Responses Lite standalone tool conversion and UI/dashboard aggregation remain future work.

### WF-2026-06-02-004 — Native Codex WebSocket prewarm for ChatGPT provider
Status: Completed
Completed: 2026-06-02
Level: 3

Close summary:
- Outcome: Added default-off `chatgpt.websocket_prewarm` support for ChatGPT/Codex providers. When enabled, the WebSocket path sends a native Codex-style `generate=false` warmup request, stores the completed warmup response id in the existing continuation cache, and lets the first matching real request reuse it with `previous_response_id` and empty `input`.
- Validation: Focused config/provider/prewarm/continuation tests passed, along with `cargo fmt --all --check`, `cargo clippy -p claude-proxy-config -p claude-proxy-providers -- -D warnings`, and `git diff --check`.
- Gaps: No live upstream Claude Code soak test was run; the option remains disabled by default.

Archived execution:
- Intent: Implement native Codex-style WebSocket prewarm for the ChatGPT/Codex provider so the first real WebSocket request can start closer to native Codex responsiveness.
- Plan:
  - [done] P1 — Review native Codex prewarm semantics and claude-proxy ChatGPT WebSocket lifecycle.
  - [done] P2 — Add minimal config/state boundaries for enabling WebSocket prewarm without changing default unsafe paths.
  - [done] P3 — Implement warmup request flow using Codex `generate=false` and safe fallback behavior.
  - [done] P4 — Add focused tests for prewarm request shape, completion handling, fallback, and non-WebSocket modes.
  - [done] P5 — Validate focused crates, update ledger checkpoint/closeout, and commit.
- Key changes:
  - Native Codex review confirmed warmup sends WebSocket `response.create` with `generate=false`, waits for `response.completed`, then reuses the warmup response id.
  - The proxy now uses existing WebSocket session and continuation state to cache warmup response ids, ignores transport-only `generate` during canonical comparison, and permits empty delta input only for matching prewarm continuations.
  - Auto transport treats prewarm failure before the real request as replay-safe and falls back to SSE without leaking `previous_response_id` or `generate` into the fallback body.
- Validation:
  - `cargo test -p claude-proxy-config`
  - `cargo test -p claude-proxy-providers --lib chatgpt_`
  - `cargo test -p claude-proxy-providers --lib continuation_`
  - Focused WebSocket prewarm tests, formatting, clippy, and diff whitespace checks.
- Deferred / gaps:
  - Optional live upstream Claude Code soak test with `chatgpt.websocket_prewarm = true`.

### WF-2026-06-02-003 — Codex fast-mode parity for ChatGPT provider
Status: Completed
Completed: 2026-06-02
Level: 3

Close summary:
- Outcome: Added first-class `chatgpt.fast_mode` configuration for ChatGPT/Codex providers. When enabled and no explicit `runtime.openai.service_tier` is set, ChatGPT requests send Codex `service_tier = "priority"`; request-level and runtime service-tier overrides remain higher priority. The TUI provider detail pane now exposes a ChatGPT-only `Codex Fast Mode` toggle, and newly added ChatGPT providers get default ChatGPT config.
- Validation: CodeGraph comparison confirmed native Codex Fast Mode maps to `priority` and also uses WebSocket prewarm. `cargo fmt --check`, `cargo test -p claude-proxy-config`, `cargo test -p claude-proxy-providers chatgpt::tests::`, `cargo test -p claude-proxy-cli tui::tests::`, focused new fast-mode tests, and `cargo clippy -p claude-proxy-config -p claude-proxy-providers -p claude-proxy-cli -- -D warnings` passed.
- Gaps: This reaches request-side service-tier parity but does not implement native Codex WebSocket prewarm (`generate=false`), so first-token latency may still differ from native Codex. No live upstream Claude Code soak test was run.

### WF-2026-06-02-002 — ChatGPT continuation/context/metrics follow-up
Status: Completed
Completed: 2026-06-02
Level: 3

Close summary:
- Outcome: Committed ChatGPT/Codex reliability follow-up in `c880062`: WebSocket Auto fallback now remains replay-safe after non-content prelude events, large-context handling returns upstream-like `context_length_exceeded` instead of proxy-side compression, and request observability now records transport/continuation/fallback/upstream-error/body-byte dimensions with SQLite migration support.
- Validation: Targeted ChatGPT context-limit and WebSocket fallback tests passed, `cargo test -p claude-proxy-providers --lib chatgpt_` passed, `cargo test -p claude-proxy-server --lib` passed, `cargo clippy -p claude-proxy-providers -p claude-proxy-server -- -D warnings` passed, `cargo fmt --all --check` passed, and `git diff --check` passed.
- Gaps: No live upstream Claude Code soak test was run. GitNexus refresh failed during closeout, and the project-level GitNexus requirement has since been removed by user request.

Archived execution:
- Intent: Improve ChatGPT/Codex reliability and observability after log/metrics review while letting Claude Code own compaction.
- Plan:
  - [done] P1 — Confirm minimal design and user-approved behavior boundaries.
  - [done] P2 — Extend WebSocket stale `previous_response_id` fallback after non-content prelude upstream events when no downstream item was emitted.
  - [done] P3 — Replace proxy-side large-context shrinking with an upstream-like `context_length_exceeded` response path that lets Claude Code compact itself.
  - [done] P4 — Add request observability / metrics dimensions for transport, continuation, fallback, and upstream error details.
  - [done] P5 — Validate targeted behavior, commit, and attempt to refresh the code intelligence index.
- Key changes:
  - User rejected proxy-side proactive compression and selected combined policy: normalize real upstream `context_length_exceeded` and locally simulate upstream-like context-limit failures only for clearly oversized ChatGPT requests.
  - User challenged the original 700KiB threshold; implementation now derives the local ChatGPT preflight threshold from `ModelInfo.capabilities.limits.context_window` when available, using 700KiB only as the unknown-model fallback.
  - P4 added provider request metadata events, server-side metadata merging, upstream error classification fields, and legacy-safe SQLite observability columns.
- Validation:
  - Focused provider/server tests, broad ChatGPT provider tests, server library tests, clippy, formatting, and diff whitespace checks passed before commit `c880062`.
- Deferred / gaps:
  - Optional live Claude Code soak test against ChatGPT/Codex upstream.

### WF-2026-06-02-001 — ChatGPT WebSocket continuation concurrency stability
Status: Completed
Completed: 2026-06-02
Level: 3

Intent:
- Investigate recurring ChatGPT `Previous response with id ... not found` 400s under concurrent/continued Claude Code usage, review the proxy concurrency design, and improve stability/performance with narrow validated changes.

Close summary:
- Outcome: WebSocket startup now treats stale ChatGPT continuation `previous_response_id ... not found` errors as safe-to-replay only when a cached continuation was actually used; it clears volatile WebSocket/continuation state, closes the rejected connection, and lets Auto transport retry through SSE with the original full body.
- Validation: CodeGraph impact checked `open_websocket_stream`, `prepare_continuation`, and related WebSocket state before edits. `cargo test -p claude-proxy-providers chatgpt_auto_transport_falls_back_to_sse_when_continuation_response_id_is_stale`, full `cargo test -p claude-proxy-providers` (268 tests), `cargo fmt --check`, and `cargo clippy -p claude-proxy-providers -- -D warnings` passed.
- Gaps: GitNexus MCP tools/resources were not exposed in this session, so GitNexus detect_changes could not be run. No live upstream Claude Code soak test was run.

### WF-2026-05-27-007 — Release v1.3.5
Status: Completed
Completed: 2026-05-27
Level: 2

Intent:
- Publish a new tool version with Claude Code ToolSearch defaults, request payload diagnostics, and streaming responsiveness fixes.

Close summary:
- Outcome: bumped workspace release metadata to v1.3.5 and added changelog notes for ToolSearch defaults, ChatGPT/OpenAI tool-schema diagnostics, Anthropic SSE frame decoding, and SSE anti-buffering headers.
- Validation: `cargo check -p claude-proxy-cli`, `cargo fmt --check`, focused CLI/provider/server tests, full `cargo test`, `git diff --check`, and GitNexus detect_changes passed/run.
- Gaps: GitNexus detect_changes reported HIGH due broad CLI server-start, Anthropic streaming, OpenAI request observability, and SSE response changes; affected flows align with the intended release scope. CI/release workflow status will be verified after pushing the tag.

### WF-2026-05-27-006 — Streaming responsiveness optimizations
Status: Completed
Completed: 2026-05-27
Level: 2

Intent:
- Improve Claude Code streaming responsiveness and correctness by fixing Anthropic SSE frame handling and adding anti-buffering stream response headers.

Close summary:
- Outcome: Anthropic streaming now decodes complete SSE frames before forwarding events, avoiding TCP-chunk boundary bugs; SSE responses now send `cache-control: no-cache, no-transform` and `x-accel-buffering: no` to discourage intermediary buffering.
- Validation: GitNexus impact was LOW for `AnthropicProvider::chat` and HIGH for shared `sse_body_response`; user confirmed proceeding after warning. `cargo fmt --check`, focused Anthropic/header tests, full `cargo test -p claude-proxy-server`, `git diff --check`, and GitNexus detect_changes passed/run.
- Gaps: no real Claude Code streaming soak test was run; `crates/claude-proxy-providers/src/openai_compat.rs` had pre-existing uncommitted edits outside this workflow.

### WF-2026-05-27-005 — Release v1.3.4
Status: Completed
Completed: 2026-05-27
Level: 2

Intent:
- Publish a new tool version for the stream stability diagnostics work without tracking the CI workflow after pushing.

Close summary:
- Outcome: bumped workspace release metadata to v1.3.4 and added changelog notes for stream safeguards and active stream diagnostics; next shell actions publish by pushing main and tag `v1.3.4` without waiting for CI.
- Validation: `cargo check -p claude-proxy-cli`, `cargo fmt --check`, `git diff --check`, and GitNexus detect_changes passed/run; detect_changes was LOW with no changed symbols or affected processes.
- Gaps: CI/release workflow status intentionally not tracked per user request.

### WF-2026-05-27-004 — Stream stability optimization
Status: Completed
Completed: 2026-05-27
Level: 2

Intent:
- Improve Claude Code long-session stability and responsiveness by adding HTTP/SSE safeguards, clearer stream timeout behavior, and consistent provider idle handling without changing the Claude Code ↔ proxy protocol.

Close summary:
- Outcome: committed stream stability diagnostics, adding SSE heartbeat comments, stream idle/overall watchdogs, conservative tool-use terminal timeout, configurable `[server]` safeguard durations, provider idle-timeout consistency for Anthropic/Copilot streams, and prompt-content-free `active_streams` admin diagnostics using random request UUIDs.
- Validation: `cargo fmt --check`, focused config/server tests, `cargo test -p claude-proxy-config`, `cargo test -p claude-proxy-server`, `cargo test -p claude-proxy-cli`, `cargo clippy -p claude-proxy-config -p claude-proxy-server -p claude-proxy-cli -p claude-proxy-providers -- -D warnings`, `git diff --check`, and GitNexus detect_changes passed/run.
- GitNexus: detect_changes reported CRITICAL because the patch intentionally touches broad `ServerConfig`, stream leader/follower, admin metrics, and provider streaming paths; scoped impact for `stream_leader_response` remained LOW and changed processes align with the stability/diagnostics scope.
- Gaps: no real Claude Code long-session soak test was run; optional follow-up is richer admin stuck-stream UI/diagnostics if needed.

### WF-2026-05-27-003 — Remove completions command
Status: Completed
Level: 2
Started: 2026-05-27
Last updated: 2026-05-27
Current phase: Closed

Intent:
- Remove the `claude-proxy completions` CLI command because the user does not want to keep shell-completion generation as a visible tool command.

Close summary:
- Outcome: removed the `completions` subcommand, `clap_complete` dependency, and README shell-completion sections; updated CLI architecture docs and Unreleased changelog.
- Validation: `cargo fmt --check`, `cargo check -p claude-proxy-cli`, `cargo test -p claude-proxy-cli`, `cargo clippy -p claude-proxy-cli -- -D warnings`, `git diff --check`, and GitNexus detect_changes passed.
- Gaps: prepared for v1.3.3 commit/release after user requested publishing without CI tracking.

### WF-2026-05-27-002 — v1.3.2 tool release
Status: Completed
Level: 2
Started: 2026-05-27
Last updated: 2026-05-27
Current phase: Closed

Intent:
- Commit the CLI maintenance commands, bump the tool to v1.3.2, push main, and publish the GitHub release via the tag workflow.

Close summary:
- Outcome: committed `69e6371` (`Prepare v1.3.2 release`), pushed `main`, tagged and pushed `v1.3.2`, and the GitHub release workflow completed successfully.
- Validation: `cargo fmt --check`, `cargo test -p claude-proxy-cli`, `cargo clippy -p claude-proxy-cli -- -D warnings`, CLI help checks, `git diff --check`, and GitNexus detect_changes were run before release.
- Gaps: GitNexus detect_changes reported CRITICAL from broad CLI entrypoint/doc line shifts; scoped impact checks for touched CLI entry symbols were LOW.

### WF-2026-05-27-001 — CLI maintenance commands
Status: Completed
Level: 2
Started: 2026-05-27
Last updated: 2026-05-27
Current phase: Closed

Intent:
- Add CLI maintenance commands to clear local logs/metrics database files and stream logs live in the terminal.

Close summary:
- Outcome: added `claude-proxy clean` for local log/metrics cleanup and `claude-proxy logs` for live log streaming, plus README/README_EN command docs.
- Validation: `cargo fmt --check`, `cargo check -p claude-proxy-cli`, `cargo test -p claude-proxy-cli`, CLI help checks, and a temp-file log streaming smoke test passed.
- Gaps: GitNexus detect_changes reports CRITICAL due broad CLI entrypoint/doc line shifts; scoped impact checks for `Commands`, `main`, `async_main`, and `log_dir` were LOW and changed behavior is limited to new maintenance commands plus skipping self-logging for them.

### WF-2026-05-23-001 — Capability contract optimization
Completed: 2026-05-23
Level: 3

Close summary:
- Outcome: Replaced flat model capability fields with canonical `ModelCapabilities`; providers, `/v1/models`, `/admin/metrics`, TUI parsing, and conservative `/v1/messages` request validation now use the canonical shape.
- Validation: `cargo fmt --check`, targeted crate tests, full `cargo test`, `cargo clippy -- -D warnings`, `git diff --check`, and GitNexus `detect_changes` completed.
- GitNexus: `detect_changes` reported CRITICAL because the change intentionally updates the core `ModelInfo` contract and message validation flow; affected processes matched planned model metadata, admin metrics, TUI, provider metadata, and `/v1/messages` scope.
- Gaps: Did not run a live TUI visual session or real upstream provider requests; validation is unit/integration/static.

### WF-2026-05-20-001 — 解决 main 分支推送冲突

Completed: 2026-05-21
Level: 2

Close summary:

- Outcome: `main` 已完成推送冲突后的收尾，最近两次提交 `73648f4` 与 `3662dae` 已推送，当前 `main...origin/main` 对齐且工作树干净。
- Validation: ChatGPT/Codex metadata baseline 提交前已运行 `cargo fmt --check`、`cargo test -p claude-proxy-providers`、`git diff --check` 和 GitNexus staged `detect_changes`；workflow 提交 detect 为 LOW。
- Gaps: None。

### WF-2026-05-20-006 — Core metrics optimization

Completed: 2026-05-20
Level: 3

Close summary:

- Outcome: 修正 streaming leader latency 在 stream 完成后计入；`/admin/metrics` 增加 session/stored diagnostics 和 observability summary；SQLite usage events 增加 `terminal_reason`/`error_kind` 并保留既有数据；TUI Dashboard 展示 observability/top error，并澄清 admin token fallback。
- Validation: `cargo fmt --check`、`cargo test -p claude-proxy-cli`、`cargo test -p claude-proxy-server`、`cargo test`、`cargo clippy -- -D warnings` 均通过。
- GitNexus: 修改前对 `poll_metrics`、`render_dashboard`、`render_server_page`、`stream_leader_response`、`record_completed_request`、`record_usage` 做 impact；提交前 `detect_changes` 为 CRITICAL，影响集中在计划内 metrics/persistence/routes/TUI render 路径；提交后已运行 `npx gitnexus analyze`。
- Gaps: 未做真实本地 TUI/浏览器端到端可视验证；`.antigravitycli/` 未跟踪目录保留未提交。

### WF-2026-05-20-003 — 长会话观测 metrics

Completed: 2026-05-20
Level: 3

Close summary:

- Outcome: 新增默认开启、可通过配置关闭的 request observability metrics；按请求持久化阶段耗时、上游连接、流式事件间隔、idle gap、prompt-too-long retry 和 payload stats；`/admin/metrics` 输出 summary/recent/stored，且不保存 prompt/response 内容。
- Validation: `cargo check`、`cargo test --workspace --no-run`、目标测试、`cargo fmt --check`、`cargo test`、`cargo clippy -- -D warnings` 均通过。
- GitNexus: 修改前已对核心符号做 impact analysis；提交前 `detect_changes` 为 CRITICAL，影响集中在计划内配置、Provider trait、ChatGPT retry、server metrics/persistence/routes/admin metrics 流程。
- Gaps: 未做真实 Claude Code 长会话端到端观测验证；当前实现覆盖本地单元/集成验证和持久化路径。

### WF-2026-05-20-005 — 安装后启动交互提示

Completed: 2026-05-20
Level: 2

Close summary:

- Outcome: 安装器每次安装前都询问是否继续；如已有服务/进程运行则提示继续会停止它；安装完成后询问是否启动，并在选择启动时追加询问是否后台运行。已有运行状态不再自动恢复。
- Validation: `bash -n install.sh`、`git diff --check -- install.sh install.ps1` 通过；本机无 PowerShell，未执行 `install.ps1` 解析检查；GitNexus 文件级 impact 为 LOW；已提交 `85e7f7d` 并运行 `npx gitnexus analyze`。
- Gaps: 未做真实跨平台安装升级端到端验证；GitNexus compare 受当前 observability 未提交改动影响，不能作为 installer-only 风险读数。

### WF-2026-05-20-004 — 安装脚本升级时恢复已有服务

Completed: 2026-05-20
Level: 2

Close summary:

- Outcome: Unix 安装脚本在已有 daemon 运行时提示确认、停止后覆盖安装并恢复 `server start --daemon`；Windows 安装脚本在已有 claude-proxy 进程时提示确认、停止后覆盖 exe 并重新启动。
- Validation: `bash -n install.sh`、`git diff --check -- install.sh install.ps1` 通过；本机无 PowerShell，未执行 `install.ps1` 解析检查；GitNexus detect_changes 为 low、0 个受影响流程；已提交 `1a13b58` 并运行 `npx gitnexus analyze`。
- Gaps: 未做真实跨平台安装升级端到端验证。

### WF-2026-05-20-002 — 性能热点优化
Completed: 2026-05-20
Level: 3

Close summary:
- Outcome: `SseDecoder` 改为 offset-based 缓冲消费；OpenAI Chat Completions 与 Responses 流式工具/函数参数输出减少全量 clone；SQLite metrics writer 改为有界队列并保持非阻塞 `try_send`；内存 metrics 维度合并为单个锁并保留 `/admin/metrics` JSON shape。
- Validation: `cargo fmt --check`、provider SSE/streaming 参数目标测试、server persistence/metrics 目标测试、`cargo test`、`cargo clippy -- -D warnings` 均通过。
- GitNexus: 修改前 impact 中 `SseDecoder::next_frame` 与 `Metrics::record_completed_request` 为 HIGH；提交前 `detect_changes` 为 HIGH，影响集中在计划内 SSE streaming、metrics persistence/completed request 及相关测试路径。
- Tests: 覆盖 CRLF split/mixed delimiters/partial tail SSE 边界、工具参数 sanitizer 增量输出、metrics persistence totals、并发 completed request snapshot。
- Gaps: 未做真实上游流式端到端请求验证；`/admin/metrics` quota 缓存与 TUI merged row 预计算保留为后续优化。

### WF-2026-05-19-002 — thinking 文本泄漏治理
Completed: 2026-05-20
Level: 3

Close summary:
- Outcome: 新增 `ThinkingSanitizer`，接入 Responses 与 Chat Completions 的普通 text 输出边界；移除请求侧 `[thinking]` 文本降级；结构化 reasoning 继续走 `thinking_delta`。
- Validation: `cargo fmt --check`、目标 provider tests、`cargo test -p claude-proxy-providers`、`cargo test`、`cargo clippy -- -D warnings` 均通过。
- Gaps: 未做真实上游端到端请求验证；本地验证覆盖转换层和 SSE shape。

### WF-2026-05-19-003 — Thinking 渲染异常检修
Completed: 2026-05-19
Level: 2

Close summary:
- Outcome: 新增 provider 内部 tagged-thinking splitter，并在 Chat Completions / Responses 的 streaming 与 non-streaming 普通文本输出路径识别 `[thinking]...[/thinking]` 和 `<thinking>...</thinking>`，转换为 Anthropic `thinking_delta`。
- Validation: `cargo fmt --check`、tagged-thinking 单测、tagged streaming/non-streaming 目标回归、`cargo test -p claude-proxy-providers`、`cargo test`、`cargo clippy -- -D warnings` 均通过。
- Gaps: 未做真实 Claude Code 端到端 UI 验证；本地协议转换和完整 workspace 测试已覆盖事件形状。

### WF-2026-05-19-004 — 流式 chunked EOF 错误处理
Completed: 2026-05-20
Level: 3

Close summary:
- Outcome: 修复 OpenAI Chat Completions 流式响应在终止事件后的尾部 chunked EOF 误报，同时保留中途断流错误语义。
- Validation: 对应提交 `c3354e0 Fix streaming EOF handling` 已包含回归测试与验证。
- Gaps: None。

### WF-2026-05-18-004 — 模型别名推理强度配置
Completed: 2026-05-20
Level: 2

Close summary:
- Outcome: 支持结构化模型别名推理强度，并修正 direct model ref / Claude Code model alias 路由相关语义。
- Validation: 对应提交 `c802943`、`bd04e33`、`6ed92e5` 以及设计文档提交已完成。
- Gaps: None。

### WF-2026-05-18-003 — TUI ChatGPT 额度显示规划
Completed: 2026-05-20
Level: 3

Close summary:
- Outcome: 实现 ChatGPT/Codex quota 获取、metrics 暴露和 TUI Dashboard 独立 quota 卡片。
- Validation: 对应提交 `ed12898`、`cbfdb00`、`36ca923`、`175b368` 已完成相关 provider/server/TUI 测试。
- Gaps: 未做真实账号端到端额度接口验证。

### WF-2026-05-18-002 — Claude onboarding 跳过同步
Completed: 2026-05-20
Level: 2

Close summary:
- Outcome: TUI 保存模型配置并同步 Claude Code 设置时写入 onboarding 完成标记。
- Validation: 对应提交 `7d89680 feat(tui): skip Claude onboarding on model save` 已完成。
- Gaps: None。

### WF-2026-05-18-001 — README ChatGPT TUI 配置文档
Completed: 2026-05-20
Level: 2

Close summary:
- Outcome: README 增加 ChatGPT TUI 配置流程和相关截图说明。
- Validation: 对应提交 `a593571 Document ChatGPT TUI setup flow` 已完成。
- Gaps: 示例截图不是实时账号登录截图。

### WF-2026-05-17-005 — WebSearch tool_choice 兼容修复
Completed: 2026-05-20
Level: 2

Close summary:
- Outcome: 统一 OpenAI/Copilot Chat Completions 与 Responses 的 named tool_choice 归一化语义。
- Validation: 对应提交 `bf7dc93 Normalize OpenAI tool choices for named tools` 已完成。
- Gaps: 未做真实 DeepSeek WebSearch 端到端请求验证。

### WF-2026-05-17-004 — Provider 稳定性优化规划
Completed: 2026-05-20
Level: 3

Close summary:
- Outcome: 完成 provider retry、stream idle timeout、finalization parity、PII-safe tool diagnostics、metrics dimensions、model capability metadata 和 TUI metrics display 分阶段优化。
- Validation: 对应提交包含 `72e697b`、`b50f8a8`、`eb6fec2`、`f2fc29e` 等，相关阶段均已记录测试通过。
- Gaps: usage/cost pricing estimate 仍在 Backlog 中，需明确 pricing source 后另行规划。

### WF-2026-05-19-001 — Thinking budget xhigh 映射优化
Completed: 2026-05-19
Level: 2

Close summary:
- Outcome: `thinking_budget_to_reasoning_effort` 现在按 `0..=2048 low`、`2049..=8192 medium`、`8193..=16384 high`、`16385+ xhigh` 映射，并在目标模型不支持 `xhigh` 时降级为 `high`。
- Validation: `cargo fmt --check`、`cargo test -p claude-proxy-providers`、`cargo test`、`cargo clippy -- -D warnings` 均通过。
- Gaps: 未做真实 ChatGPT 请求端到端验证；本地验证覆盖日志映射函数和 Responses 请求体转换。

### WF-2026-05-17-006 — Model context window metadata
Completed: 2026-05-17
Level: 3

Close summary:
- Outcome: 在 core `ModelInfo`、OpenAI/ChatGPT known model metadata、Copilot model parser、server capability export、TUI capability parser/Dashboard rows 接入 `context_window`。
- Validation: `cargo fmt --check`、相关 crate 测试、完整 `cargo test`、`cargo clippy -- -D warnings` 均通过。
- Gaps: 未做交互式 TUI 视觉验证；Cargo.toml 存在本任务外版本号变更。

### WF-2026-05-17-003 — ChatGPT/Codex Responses 优化
Completed: 2026-05-17
Level: 3

Close summary:
- Outcome: 完成 Responses 路径 tool schema normalization、function arguments buffering/sanitization、empty completed output `tool_use` stop reason，并修正 server streaming usage 累计快照统计。
- Validation: provider/server/workspace 测试与 clippy 均通过。
- Gaps: provider-neutral Responses 模块抽取保留为后续独立重构。

### WF-2026-05-17-002 — v0.3.4 发布
Completed: 2026-05-17
Level: 2

Close summary:
- Outcome: 完成 v0.3.4 发布准备、版本号更新和发布元数据刷新。
- Validation: release build、workspace 测试与 clippy 均通过。
- Gaps: 发布任务已由其他 agent 完成；ledger 据本地 tag 状态补记完成。

### WF-2026-05-17-001 — ChatGPT Read pages 参数清理
Completed: 2026-05-17
Level: 2

Close summary:
- Outcome: 添加保守的 Responses argument sanitizer，仅在 `Read` tool call 的 argument JSON 完整且可解析时移除顶层 `pages: ""`。
- Validation: `cargo fmt --check`、`cargo test -p claude-proxy-providers`、`cargo clippy -- -D warnings` 和 `cargo test` 均通过。
- Gaps: None。

### WF-2026-05-16-001 — ChatGPT responses 默认 instructions
Completed: 2026-05-16
Level: 2

Close summary:
- Outcome: 添加 `build_chatgpt_responses_body`，确保只有 ChatGPT Responses 请求获得 fallback instructions，同时保留已有 system instructions。
- Validation: `cargo fmt`、`cargo clippy -- -D warnings`、`cargo test -p claude-proxy-providers` 和 `cargo test` 均通过。
- Gaps: None。
