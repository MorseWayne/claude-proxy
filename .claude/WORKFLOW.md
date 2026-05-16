# 工作流台账（Workflow Ledger）

用于记录 Claude Code 开发工作的轻量级里程碑台账，便于跨会话恢复和追踪。

## Active（进行中）

None（无）。

## Backlog / Future（待办 / 未来）

- [ ] 如果 OpenAI/Copilot Responses 上游开始强制要求 `instructions`，再评估是否需要 provider-specific 处理。
- [ ] 在后续独立重构中考虑抽取 provider-neutral 的 Responses 转换模块；不阻塞 v0.3.4。

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