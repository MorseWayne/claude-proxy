# 工作流台账（Workflow Ledger）

用于记录 Claude Code 开发工作的轻量级里程碑台账，便于跨会话恢复和追踪。

## Active（进行中）

### WF-2026-05-20-001 — 解决 main 分支推送冲突
Status: In Progress
Level: 2
Priority: Paused while performance optimization commit/index refresh finishes
Started: 2026-05-20
Last updated: 2026-05-20
Current phase: 冲突解决与验证

Intent:
- 解决本地 `main` 与 `origin/main` 分叉导致的推送冲突，并保持现有工作不丢失。

Current todo:
- [x] 检查本地/远端提交差异和未提交 `.claude/WORKFLOW.md` 改动。
- [x] 确认合并语义：同时保留远端 tagged thinking 转换和本地 sanitizer 防泄漏。
- [ ] 运行格式化、provider 测试、GitNexus detect_changes，提交并刷新索引。

Changes:
- 初始状态：`main...origin/main [ahead 2, behind 2]`，未展开 merge conflict，工作区仅 `.claude/WORKFLOW.md` 有本地改动。
- 合并 `origin/main` 后冲突集中在 `.claude/WORKFLOW.md`、`chat_completions.rs`、`responses.rs`、`lib.rs`；用户确认完整 tagged thinking 应转为 `thinking_delta`，未闭合或残留 marker 不应泄漏到普通 text。

Prerequisites:
- None

Resume next:
- 运行格式化和 provider 级测试，修正失败后完成 merge commit。

## Backlog / Future（待办 / 未来）

- [ ] 如果 OpenAI/Copilot Responses 上游开始强制要求 `instructions`，再评估是否需要 provider-specific 处理。
- [ ] 清理 provider-neutral Responses 抽取相关历史待办：当前 [responses.rs](crates/claude-proxy-providers/src/responses.rs) 已完成解耦，后续只需补测试或文档。

## Completed（已完成）

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
