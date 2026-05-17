# 工作流台账（Workflow Ledger）

用于记录 Claude Code 开发工作的轻量级里程碑台账，便于跨会话恢复和追踪。

## Active（进行中）

### WF-2026-05-17-004 — Provider 稳定性优化规划

Status: Active（进行中）
Level: 3
Started: 2026-05-17
Updated: 2026-05-17
Current phase: Phase 1 — Retry / error classification 实施完成

Goal（目标）:

- 基于当前重构后的 OpenAI/ChatGPT/Copilot provider 边界，分阶段提升上游调用稳定性、streaming 鲁棒性、tool 参数诊断和 usage/cost 可观测性。

Decisions（决策）:

- 先推进 provider-level retry / error classification；这是收益最高且边界最清晰的稳定性优化。
- 当前 Phase 1 已实施：共享 HTTP retry helper 已接入 OpenAI、ChatGPT、Copilot 的 chat 与 model listing 请求路径；OAuth/device-flow 轮询请求暂不纳入本阶段。
- Phase 2 继续处理 SSE idle / half-open stream detection，避免 streaming 请求长期挂死。
- Chat Completions finalization 先以测试复核为主，确认是否存在重复 stop / usage 事件后再决定是否改状态机。
- PII-safe diagnostics 与 usage/cost metrics 作为后续增强，不与 retry/SSE 修复混合。
- 当前只做规划入账；实施前仍需按项目规则对拟修改符号运行 GitNexus impact。

#### Phase 1 — Retry / error classification 设计与影响评估
Status: Done
Depends on:
- None
Tasks:
- [x] 确认 OpenAI/ChatGPT/Copilot 当前 HTTP 请求共用边界与可抽取点。
- [x] 对拟修改符号运行 GitNexus upstream impact，若 HIGH/CRITICAL 先向用户确认。
- [x] 设计最小 retry 策略：覆盖 timeout、408、409、429、5xx、`retry-after`，避免 fallback model 和复杂全局策略。
- [x] 明确测试覆盖：可重试错误、不可重试错误、retry-after、最终错误映射。

Acceptance / Review:
- Review: 已在 [http.rs](crates/claude-proxy-providers/src/http.rs) 新增共享 `send_upstream_request` helper，最多 3 次发送可 clone 请求；对 timeout/network、408、409、429、5xx 做 transient retry，并尊重秒数形式 `retry-after`（上限 5 秒）。OpenAI、ChatGPT、Copilot 的 chat 请求路径及 OpenAI/Copilot model listing 已接入；OAuth/device-flow 轮询路径暂不纳入本阶段。
- Validation: `cargo fmt --check`、`cargo test -p claude-proxy-providers http::tests`、`cargo test -p claude-proxy-providers`、`cargo test`、`cargo clippy -- -D warnings` 均通过。
- GitNexus: 实施前 `map_upstream_response`、OpenAI `chat_via_completions` / `chat_via_responses` / `list_models`、ChatGPT `chat`、Copilot `chat_via_messages` / `chat_via_completions` / `chat_via_responses` / `list_models` upstream impact 均为 LOW；实施后 `detect_changes(scope=all)` 为 LOW，changed_count=15，affected_count=0，affected_processes=[]。
- Tests: 新增 HTTP helper 单元测试覆盖 retryable status、non-retryable status、`retry-after` clamp、timeout/network retry 判定；provider crate 77/77、workspace 全量测试通过。
- Gaps: 未覆盖 Anthropic provider 与 ChatGPT/Copilot OAuth/device-code 请求；这些认证/轮询路径有不同节奏与语义，后续如需处理应单独评估。

#### Phase 2 — Streaming idle / half-open detection
Status: Pending
Depends on:
- Phase 1
Tasks:
- [ ] 评估 [chat_completions.rs](crates/claude-proxy-providers/src/chat_completions.rs) 与 [responses.rs](crates/claude-proxy-providers/src/responses.rs) 的 streaming loop 是否需要统一 idle timeout。
- [ ] 设计 provider stream idle timeout 行为，超时返回明确 `ProviderError::Timeout`。
- [ ] 添加 stream 卡住/无 chunk 的回归测试或可执行验证。

#### Phase 3 — Chat Completions finalization parity
Status: Pending
Depends on:
- Phase 2
Tasks:
- [ ] 复核 [chat_completions.rs](crates/claude-proxy-providers/src/chat_completions.rs) 中 `finish_reason` 与 stream EOF 后 `finish()` 的交互。
- [ ] 补充重复 `message_stop` / `message_delta.usage` / tool argument flush 的回归测试。
- [ ] 仅在测试证明存在问题时调整状态机。

#### Phase 4 — Diagnostics 与 metrics 增强
Status: Pending
Depends on:
- Phase 3
Tasks:
- [ ] 在 [tool_args.rs](crates/claude-proxy-providers/src/tool_args.rs) 或调用边界加入 PII-safe 诊断，只记录 tool name、字段名、长度、sanitization 类型。
- [ ] 评估 per-model cost / context window / max output tokens 指标是否应加入 server metrics。
- [ ] 将已完成的 provider-neutral Responses 抽取待办从 Backlog 清理或改写为测试/文档跟进。

Discovered tasks（发现的后续任务）:

- 若上游后续强制要求 Responses `instructions`，再评估 OpenAI/Copilot provider-specific 处理。

Resume next（下次继续）:

- Phase 1 已完成并验证；提交后运行 `npx gitnexus analyze` 刷新索引。下一步进入 Phase 2：评估 [chat_completions.rs](crates/claude-proxy-providers/src/chat_completions.rs) 与 [responses.rs](crates/claude-proxy-providers/src/responses.rs) 的 streaming idle / half-open detection。

## Backlog / Future（待办 / 未来）

- [ ] 如果 OpenAI/Copilot Responses 上游开始强制要求 `instructions`，再评估是否需要 provider-specific 处理。
- [ ] 清理 provider-neutral Responses 抽取相关历史待办：当前 [responses.rs](crates/claude-proxy-providers/src/responses.rs) 已完成解耦，后续只需补测试或文档。

## Completed（已完成）

### WF-2026-05-17-003 — ChatGPT/Codex Responses 优化

Status: Done（已完成）
Completed: 2026-05-17
Level: 3
Commits（提交）:

- cb66177 Prepare v0.3.4 release fixes
- 5e1a146 Fix streaming usage snapshot accounting
- ef6163a Refresh GitNexus index metadata

Acceptance summary（验收摘要）:

- Review: 完成 ChatGPT/Codex Responses 路径优化：稳定 tool schema normalization、split function arguments buffering/sanitization、empty completed output 的 `tool_use` stop reason，并修正 server streaming usage 统计为最终累计快照语义，避免重复 `message_delta.usage` 导致 token 翻倍。
- Validation: `cargo fmt --check`、`cargo test -p claude-proxy-providers`、`cargo test -p claude-proxy-server usage_extraction`、`cargo test -p claude-proxy-server`、`cargo test`、`cargo clippy -- -D warnings` 均通过。
- GitNexus: Phase 1 推进前 `detect_changes` 为 medium，集中在 Responses converter；Phase 2 评估显示 `convert_to_responses` 上游影响为 CRITICAL，因此暂缓抽取；Phase 3 中 `extract_usage_from_event` 上游 impact 为 LOW，变更后 `detect_changes` 为 low 且无 affected processes。最终 `npx gitnexus analyze` 已刷新索引至 1,608 nodes / 3,892 edges / 141 flows。
- Tests: provider crate 62/62 通过；server crate 10/10 与 integration 7/7 通过；完整 workspace 测试全部通过。新增 usage snapshot 回归覆盖重复 `message_delta.usage` 与 cache token snapshot。
- Gaps: provider-neutral Responses 模块抽取保留为后续独立重构；当前 ChatGPT/Codex 行为优化已完成。

### WF-2026-05-17-002 — v0.3.4 发布

Status: Done（已完成）
Completed: 2026-05-17
Level: 2
Commits / Tag（提交 / 标签）:

- cb66177 Prepare v0.3.4 release fixes
- c5d145a Refresh v0.3.4 release metadata
- v0.3.4 tag: 4524ceb（本地 tag 存在，指向 `c5d145a`）

Acceptance summary（验收摘要）:

- Review: 确认 v0.3.4 包含 Responses tool schema normalization、split function arguments buffering/sanitization、empty completed output `tool_use` stop reason、版本号更新和发布元数据刷新。
- Validation: 发布准备阶段已验证 `./target/release/claude-proxy --version` 输出 `claude-proxy 0.3.4`；`cargo fmt --check`、`cargo test -p claude-proxy-providers`、`cargo test`、`cargo clippy -- -D warnings`、`cargo build --release` 均通过。
- GitNexus: 文件级 impact 对 `chatgpt.rs` 和 `copilot/responses.rs` 为 LOW；最终发布准备 `detect_changes` 为 medium，主要变更集中在 `ResponsesStreamConverter`、`normalize_tool_schema` 和相关测试。发布后 GitNexus 元数据已刷新。
- Tests: provider crate 与完整 workspace 验证通过。
- Gaps: 用户确认发布任务已由其他 agent 完成；本 ledger 已据本地 tag 状态补记完成。

### WF-2026-05-17-001 — ChatGPT Read pages 参数清理

Status: Done（已完成）
Completed: 2026-05-17
Level: 2
Commits（提交）:

- a907eb7 Sanitize empty Read pages arguments

Acceptance summary（验收摘要）:

- Review: 添加了保守的 Responses argument sanitizer，仅在 `Read` tool call 的 argument JSON 完整且可解析时移除顶层 `pages: ""`；保留非 Read 工具和其他 empty string。
- Validation: `cargo fmt --check`、`cargo test -p claude-proxy-providers`、`cargo clippy -- -D warnings` 和 `cargo test` 均通过。
- GitNexus: 对 `handle_function_call_arguments_done`、`handle_output_item_done` 和 `convert_function_call` 的 impact 检查为 LOW risk；`detect_changes` 报告 LOW risk，且无 affected processes。
- Tests: 添加了 `Read.pages: ""` 移除、Bash command 保留、non-streaming sanitization、non-Read empty string 保留等回归覆盖。
- Gaps: None（无）。

### WF-2026-05-16-001 — ChatGPT responses 默认 instructions

Status: Done（已完成）
Completed: 2026-05-16
Level: 2
Commits（提交）:

- 83be0f9 Fix ChatGPT responses instructions fallback

Acceptance summary（验收摘要）:

- Review: 添加 `build_chatgpt_responses_body`，确保只有 ChatGPT Responses 请求获得 fallback instructions，同时保留已有 system instructions。
- Validation: `cargo fmt`、`cargo clippy -- -D warnings`、`cargo test -p claude-proxy-providers` 和 `cargo test` 均通过。
- GitNexus: 初始对 `ChatGptProvider.chat` 的 `impact` 为 LOW risk；最终 `detect_changes` 因触及 `chatgpt.rs` 和相关测试流程报告 HIGH，经复核符合预期。`npx gitnexus analyze` 将索引更新为 1,584 nodes / 3,814 edges / 139 flows。
- Tests: 添加了 missing ChatGPT instructions、保留 existing system instructions、fast-intent body generation 的覆盖。
- Gaps: None（无）。