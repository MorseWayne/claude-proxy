# 工作流台账（Workflow Ledger）

用于记录 Claude Code 开发工作的轻量级里程碑台账，便于跨会话恢复和追踪。

## Active（进行中）

### WF-2026-05-17-003 — ChatGPT/Codex Responses 优化

Status: In Progress（进行中）
Level: 3
Started: 2026-05-17
Updated: 2026-05-17
Current phase: Phase 2 — 分离共享 Responses 模块边界
Goal: 通过稳定 Codex/ChatGPT 共享 Responses 转换路径中的 tool schema、流式 function arguments、stop reason 和后续 metrics 准确性，深度优化 ChatGPT provider 行为。

Decisions（决策）:
- 归类为 Level 3，因为该工作会触及 ChatGPT 与 Copilot 共用的 Responses converter 中的 streaming/tool-call 行为。
- 先稳定行为修复，再考虑模块抽取；避免把功能修复和结构重构混在同一阶段。
- 当前 GitNexus 核实结果显示：`detect_changes` 风险为 medium，`ResponsesStreamConverter.process_event` 上游影响为 MEDIUM，直接消费者主要限于 `stream_responses_response` 和 converter 测试。
- Claude Code 历史源码支持“先缓冲 streamed tool input，再 normalize”的策略；本地日志也确认反复出现 `Read.pages: ""` 失败和 split tool arguments 场景。

#### Phase 1 — 稳定当前 converter 修复
Status: Done（已完成）
Depends on（依赖）:
- None（无）
Tasks（任务）:
- [x] 审查当前 provider 改动，重点确认 schema normalization、argument buffering、sanitize 行为和 stop reason 处理。
- [x] 运行聚焦 provider 验证：`cargo test -p claude-proxy-providers`。
- [x] 如果聚焦测试通过，运行完整 workspace 验证。
- [x] 验证后重新运行 GitNexus `detect_changes`，确认影响范围仍符合预期。
- [x] 确认已提交的行为修复并记录提交信息。

Acceptance / Review（验收 / 复核）:
- Review: 已确认 `cb66177 Prepare v0.3.4 release fixes` 包含 `Cargo.toml`、`chatgpt.rs`、`copilot/responses.rs`，覆盖 tool schema normalization、split function arguments buffering/sanitization、empty completed output 的 `tool_use` stop reason，以及 ChatGPT/Codex fixture 测试。
- Validation: `cargo fmt --check`、`cargo test -p claude-proxy-providers`、`cargo test`、`cargo clippy -- -D warnings` 均通过。
- GitNexus: 推进前 `detect_changes` 为 medium，变更集中在 Responses converter；最终复核时 provider 代码已在 HEAD，剩余工作树仅有 workflow/AGENTS/CLAUDE 文档变更，`detect_changes` 为 low 且无 affected processes。
- Tests: provider crate 62/62 通过；完整 workspace 测试全部通过。
- Gaps: 代码修复已提交但 GitNexus 元数据文档和 workflow ledger 仍有未提交更新；Phase 2/3 仍待决策和实施。

#### Phase 2 — 分离共享 Responses 模块边界
Status: Pending（待处理）
Depends on（依赖）:
- Phase 1
Tasks（任务）:
- [ ] 在行为修复提交后，决定是否将共享 Responses 代码从 `copilot::responses` 抽出。
- [ ] 如果需要抽取，在不改变行为的前提下迁移 request、stream、non-stream、sanitize 职责。
- [ ] 抽取后验证 ChatGPT 与 Copilot 路径。

#### Phase 3 — 提升 usage 与 metrics 准确性
Status: Pending（待处理）
Depends on（依赖）:
- Phase 1
Tasks（任务）:
- [ ] 审计最终 Responses usage 是否正确传播到 `message_delta.usage`。
- [ ] 确认 server metrics 优先采用 completed/delta 事件中的最终 usage。
- [ ] 如果仍有缺口，为 streaming input/output token 捕获添加回归测试。

Discovered tasks（发现的后续任务）:
- 当前 ChatGPT/Codex 兼容性修复稳定后，考虑抽取 provider-neutral 的 Responses 转换模块。
- 将 OpenAI/Responses streaming token accounting 审计作为独立于 tool-call 兼容性工作的后续任务。

Resume next（下次继续）: 审查现有未提交 provider 改动，运行聚焦 provider 测试，然后重新运行 GitNexus change detection，再决定提交或调整实现。

### WF-2026-05-17-002 — v0.3.4 发布

Status: In Progress（进行中）
Level: 2
Started: 2026-05-17
Updated: 2026-05-17
Current phase: Phase 2 — 发布与推送
Goal: 准备并发布 v0.3.4，包含待处理的 provider 修复和版本号更新。

Decisions（决策）:
- 发布版本为 v0.3.4，因为最新已有 tag 是 v0.3.3。
- 验证通过后，将现有 provider 改动纳入本次发布。

#### Phase 1 — 发布准备
Status: Done（已完成）
Depends on（依赖）:
- None（无）
Tasks（任务）:
- [x] 确认当前 git 状态和最新 tag。
- [x] 审查待处理的 provider 改动。
- [x] 将 workspace package version 提升到 0.3.4。
- [x] 运行发布验证。

Acceptance / Review（验收 / 复核）:
- Review: 确认 v0.3.4 包含 Responses tool schema normalization、split function arguments buffering/sanitization、empty completed output tool_use stop reason 和版本号更新。
- Validation: `./target/release/claude-proxy --version` 输出 `claude-proxy 0.3.4`；`v0.3.4` tag 尚不存在。
- GitNexus: 文件级 impact 对 `chatgpt.rs` 和 `copilot/responses.rs` 为 LOW；最终 `detect_changes` 为 medium，主要变更集中在 `ResponsesStreamConverter`、`normalize_tool_schema` 和相关测试。
- Tests: `cargo fmt --check`、`cargo test -p claude-proxy-providers`、`cargo test`、`cargo clippy -- -D warnings`、`cargo build --release` 均通过。
- Gaps: 尚需提交、打 tag、更新 GitNexus 索引并推送。

Resume next（下次继续）: 提交 GitNexus/ledger 元数据，打 v0.3.4 tag，推送 commits 与 tag。

## Backlog / Future（待办 / 未来）

- [ ] 如果 OpenAI/Copilot Responses 上游开始强制要求 `instructions`，再评估是否需要 provider-specific 处理。

## Completed（已完成）

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
