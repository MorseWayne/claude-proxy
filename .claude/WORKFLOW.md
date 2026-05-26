# 工作流台账（Workflow Ledger）

用于记录 Claude Code 开发工作的轻量级里程碑台账，便于跨会话恢复和追踪。

## Active（进行中）

### WF-2026-05-25-001 — ChatGPT/Codex provider modernization
Status: In Progress
Level: 3
Started: 2026-05-25
Last updated: 2026-05-26
Current phase: Release prep for v1.3.0

Intent:
- Modernize ChatGPT/OpenAI provider integration using lessons from `/home/wayne/source/open/pi/packages/ai`: accurate ChatGPT/Codex capabilities, richer Responses options, safer prompt cache keys, usage accuracy, WebSocket transport with SSE fallback, and continuation/delta input.

Current todo:
- [x] Finalize design scope and implementation sequence before code changes.
- [x] Review the written design spec and address any issues.
- [x] Wait for user review of the committed design spec.
- [x] Run GitNexus impact analysis before editing each target symbol.
- [x] Implement Phase 1 foundation: model/request/cache/usage correctness plus focused tests.
- [x] Validate Phase 1 with fmt and targeted provider tests before moving to WebSocket work.
- [x] Review Phase 1 diff and apply any reviewer fixes before starting WebSocket work.
- [x] Commit Phase 1 and refresh GitNexus index.
- [x] Implement Phase 2 WebSocket transport with SSE fallback.
- [x] Review Phase 2 diff and address reviewer blockers.
- [x] Final focused reviewer pass found no remaining blockers.
- [x] Commit Phase 2 and refresh GitNexus metadata.
- [x] Plan Phase 3 continuation/delta-input support.
- [x] Implement Phase 3 continuation/delta-input support.
- [x] Validate/review Phase 3.
- [x] Commit Phase 3 and refresh GitNexus metadata.
- [x] Decide whether to add non-blocking continuation hardening tests.
- [x] Implement continuation hardening tests for busy overlap, abort invalidation, and function-call/tool-result delta.
- [x] Validate continuation hardening tests.
- [x] Commit continuation hardening tests and refresh GitNexus metadata.
- [x] Remove untracked review report artifacts.
- [ ] Prepare v1.3.0 release metadata and tag.

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

Prerequisites:
- User has asked to start/continue implementation from the approved spec.

Resume next:
- Validate release metadata, commit the v1.3.0 prep, and create the release tag if approved.

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
