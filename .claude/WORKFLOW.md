# 工作流台账（Workflow Ledger）

用于记录 Claude Code 开发工作的轻量级里程碑台账，便于跨会话恢复和追踪。

## Active（进行中）

### WF-2026-05-17-004 — Provider 稳定性优化规划

Status: Active（进行中）
Level: 3
Started: 2026-05-17
Updated: 2026-05-17
Current phase: Phase 5A — Metrics shape enrichment 已完成

Goal（目标）:

- 基于当前重构后的 OpenAI/ChatGPT/Copilot provider 边界，分阶段提升上游调用稳定性、streaming 鲁棒性、tool 参数诊断和 usage/cost 可观测性。

Decisions（决策）:

- 先推进 provider-level retry / error classification；这是收益最高且边界最清晰的稳定性优化。
- 当前 Phase 1 已实施：共享 HTTP retry helper 已接入 OpenAI、ChatGPT、Copilot 的 chat 与 model listing 请求路径；OAuth/device-flow 轮询请求暂不纳入本阶段。
- Phase 2 已实施：OpenAI Chat Completions 与 Responses streaming loop 现在通过共享 idle timeout helper 读取上游 chunk，半开连接会返回 `ProviderError::Timeout`。
- Phase 3 已实施：Chat Completions `StreamConverter` 收到 `finish_reason` 后会标记 stopped，EOF `finish()` 不再重复发送 `message_delta` / `message_stop`。
- Phase 4 已拆分并先实施 provider 侧 PII-safe tool diagnostics：只记录 tool name、字段名、sanitization 类型与长度，不记录参数内容；usage/cost metrics 因涉及 server schema/API/TUI，延后单独处理。
- Phase 5 规划结论：usage/cost metrics 应先做“模型能力与用量可观测性”而不是价格估算；cost 需要可维护 pricing source，否则只暴露 billable token 维度与模型 context/max output metadata；不保留旧 metrics JSON 兼容，server 与 TUI 同步更新新契约。
- Phase 5A 已实施 server-only metrics shape enrichment：session 与 stored metrics 均新增 provider / initiator 维度聚合；SQLite schema 无需变更。

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
Status: Done
Depends on:
- Phase 1
Tasks:
- [x] 评估 [chat_completions.rs](crates/claude-proxy-providers/src/chat_completions.rs) 与 [responses.rs](crates/claude-proxy-providers/src/responses.rs) 的 streaming loop 是否需要统一 idle timeout。
- [x] 设计 provider stream idle timeout 行为，超时返回明确 `ProviderError::Timeout`。
- [x] 添加 stream 卡住/无 chunk 的回归测试或可执行验证。

Acceptance / Review:
- Review: 已在 [http.rs](crates/claude-proxy-providers/src/http.rs) 增加共享 `next_upstream_stream_item` helper，使用 120 秒 idle timeout 包装上游 `bytes_stream().next()`；[chat_completions.rs](crates/claude-proxy-providers/src/chat_completions.rs) 与 [responses.rs](crates/claude-proxy-providers/src/responses.rs) 的 streaming loop 均已接入，超时会向下游发送 `ProviderError::Timeout` 并结束任务。
- Validation: `cargo fmt --check`、`cargo test -p claude-proxy-providers http::tests`、`cargo test -p claude-proxy-providers`、`cargo test`、`cargo clippy -- -D warnings` 均通过。
- GitNexus: 实施前 `stream_openai_response` 与 `stream_responses_response` upstream impact 均为 LOW；实施后 `detect_changes(scope=all)` 为 LOW，changed_count=5，affected_count=0，affected_processes=[]。
- Tests: 新增 `upstream_stream_item_times_out_when_idle`，覆盖 pending upstream item 在零时长 timeout 下返回 `ProviderError::Timeout`；provider crate 78/78、workspace 全量测试通过。
- Gaps: 当前 idle timeout 固定为 120 秒，未暴露配置项；如后续用户需要可配置化，应作为独立配置变更处理。

#### Phase 3 — Chat Completions finalization parity
Status: Done
Depends on:
- Phase 2
Tasks:
- [x] 复核 [chat_completions.rs](crates/claude-proxy-providers/src/chat_completions.rs) 中 `finish_reason` 与 stream EOF 后 `finish()` 的交互。
- [x] 补充重复 `message_stop` / `message_delta.usage` / tool argument flush 的回归测试。
- [x] 仅在测试证明存在问题时调整状态机。

Acceptance / Review:
- Review: 回归测试先确认 `finish_reason` 后再调用 EOF `finish()` 会重复 finalization；随后在 [chat_completions.rs](crates/claude-proxy-providers/src/chat_completions.rs) 的 `StreamConverter` 增加 `stopped` 状态，`process_chunk` 处理 `finish_reason` 后标记停止，`finish()` 对已停止状态直接返回空事件。
- Validation: `cargo fmt --check`、`cargo test -p claude-proxy-providers test_stream_converter_does_not_finish_twice_after_finish_reason`、`cargo test -p claude-proxy-providers`、`cargo test`、`cargo clippy -- -D warnings` 均通过。
- GitNexus: `stream_openai_response` upstream impact 为 LOW；`process_chunk` upstream impact 为 LOW（4 direct，包括入口和测试）；`finish` upstream impact 为 LOW（1 direct）。实施后 `detect_changes(scope=all)` 为 LOW，changed_count=9，affected_count=0，affected_processes=[]。
- Tests: 新增 `test_stream_converter_does_not_finish_twice_after_finish_reason`，覆盖 `finish_reason` chunk 已产生一次 `message_stop` 后 EOF `finish()` 不再产生事件；provider crate 79/79、workspace 全量测试通过。
- Gaps: 当前只修正 Chat Completions converter；Responses converter 已有 stopped 状态，本阶段无需改动。

#### Phase 4 — PII-safe tool diagnostics
Status: Done
Depends on:
- Phase 3
Tasks:
- [x] 在 [tool_args.rs](crates/claude-proxy-providers/src/tool_args.rs) 加入 PII-safe 诊断，只记录 tool name、字段名、长度、sanitization 类型。
- [x] 评估 per-model cost / context window / max output tokens 指标是否应加入 server metrics。
- [x] 将 usage/cost metrics 拆出为后续独立任务，避免与 provider sanitizer 诊断混合。

Acceptance / Review:
- Review: 已在 [tool_args.rs](crates/claude-proxy-providers/src/tool_args.rs) 为 `Read` argument sanitizer 增加结构化诊断路径；公开 sanitizer 行为保持 `Option<String>` 不变，Chat Completions 与 Responses 调用点无需改动。诊断使用 DEBUG 日志，只记录 `tool_name`、字段名、sanitization 类型、原始/修正后长度，不记录 `file_path` 或 argument 内容。
- Validation: `cargo fmt --check`、`cargo test -p claude-proxy-providers tool_args`、`cargo test -p claude-proxy-providers`、`cargo test`、`cargo clippy -- -D warnings` 均通过。
- GitNexus: Phase 4 探索显示 provider sanitizer 与 server metrics 是两条独立边界；`sanitize_tool_arguments` 与 `sanitize_read_line_window` upstream impact 为 HIGH（共享 Chat Completions/Responses sanitizer 与测试流程），已向用户确认后继续。实施后 `detect_changes(scope=all)` 为 HIGH，changed_count=9，affected_count=8，affected_processes 均集中在 `tool_args.rs` sanitizer 测试/辅助流程，符合预期。
- Tests: provider crate 81/81、workspace 全量测试通过；新增 `read_sanitizer_reports_pii_safe_diagnostics` 与 `read_sanitizer_reports_removed_unverifiable_offset` 覆盖诊断字段、sanitization 类型与长度，不断言敏感参数内容。
- Gaps: usage/cost metrics 未在本阶段实现；因会影响 server metrics schema、admin API 与 TUI 展示，保留为独立后续任务。

#### Phase 5 — Usage/cost metrics 规划
Status: Done
Depends on:
- Phase 4
Tasks:
- [x] 用 GitNexus 复核 server metrics、persistence、admin API 与 TUI dashboard 的执行边界。
- [x] 明确第一步不直接做价格估算，先补齐 per-provider/per-initiator/per-model 的可观测性与模型能力 metadata。
- [x] 拆分实施顺序，降低 schema/API/TUI 同时变更风险。

Planned implementation order:
1. **Phase 5A — Metrics shape enrichment（server-only）**：扩展 `Metrics` / `StoredTotals` 的聚合输出，保留现有 token snapshot 语义，新增 provider+initiator 维度聚合；直接定义新的 `/admin/metrics` JSON shape，不为旧 shape 添加兼容 shim。
2. **Phase 5B — Model capability metadata（provider/server boundary）**：把 provider `ModelInfo` 中已有的 context window、max output tokens、supported endpoints/reasoning metadata 暴露到新的 metrics/admin 响应契约，供 TUI 展示；避免混入 request path 的实时计费逻辑。
3. **Phase 5C — TUI metrics display**：按新 metrics JSON 契约更新 Dashboard 解析与展示 provider/initiator 维度、模型能力列；不支持旧 server 的旧 metrics shape，发现字段缺失时按新契约默认空值处理即可。
4. **Phase 5D — Cost estimate（optional）**：仅在有明确 pricing table 来源和更新策略后实现估算；否则只展示 billable token 分解，避免误导性成本数字。

Acceptance / Review:
- Review: 已确认当前 [app.rs](crates/claude-proxy-server/src/app.rs) 的 `TokenUsage` / `ModelMetrics` 只按 model 聚合 token，`MetricsStore` 只持久化 provider、initiator、model、token、error、latency，`/admin/metrics` 输出 session/stored totals，TUI Dashboard 只解析并合并 model token totals；用户明确要求不考虑兼容，因此后续实施可同步替换 server/TUI 的 metrics 契约。
- Validation: 规划阶段未修改业务代码；读取并复核 [routes.rs](crates/claude-proxy-server/src/routes.rs)、[persistence.rs](crates/claude-proxy-server/src/persistence.rs)、[dashboard.rs](crates/claude-proxy-cli/src/tui/pages/dashboard.rs) 与 [app.rs](crates/claude-proxy-cli/src/tui/app.rs)。
- GitNexus: `record_completed_request`、`MetricsStore.record_usage`、`MetricsStore.load_totals`、`admin_metrics`、`fetch_live_metrics`、`render_model_usage` 是主要边界；server 与 TUI 变更会跨 crate，应分阶段实施。
- Tests: N/A（规划阶段）。
- Gaps: 尚未选择 pricing source；未运行具体符号 impact，实施前仍需对拟修改符号逐一运行 GitNexus upstream impact；无需为旧 metrics JSON shape 设计兼容层。

#### Phase 5A — Metrics shape enrichment（server-only）
Status: Done
Depends on:
- Phase 5
Tasks:
- [x] 对 `record_completed_request`、`MetricsStore.record_usage`、`MetricsStore.load_totals`、`Metrics.to_json` 运行 GitNexus upstream impact。
- [x] 扩展 session metrics 输出，新增 provider / initiator 维度聚合。
- [x] 扩展 stored totals 聚合，按 model / provider / initiator 输出统一 token usage metrics。
- [x] 补充 server metrics shape 与 persistence aggregation 回归测试。

Acceptance / Review:
- Review: 已在 [app.rs](crates/claude-proxy-server/src/app.rs) 将 `ModelMetrics` 泛化为 `UsageMetrics`，保留 `ModelMetrics` alias，并为 session metrics 新增 `providers` 与 `initiators` 输出；已在 [persistence.rs](crates/claude-proxy-server/src/persistence.rs) 复用现有 `usage_events.provider` / `initiator` 字段聚合 stored totals，无需 SQLite schema migration。
- Validation: `cargo fmt --check`、`cargo test -p claude-proxy-server`、`cargo test`、`cargo clippy -- -D warnings` 均通过。
- GitNexus: 实施前 `Metrics.record_completed_request#6`、`MetricsStore.record_usage#6`、`MetricsStore.load_totals#0`、`Metrics.to_json#0` upstream impact 均为 LOW；实施后 `detect_changes(scope=all)` 为 MEDIUM，changed_count=31，affected_count=1，affected_processes=[`Handle_server → Load_stored_totals`]，符合启动加载 stored totals 的预期影响。
- Tests: 新增 `completed_request_records_provider_and_initiator_metrics` 覆盖 session `models` / `providers` / `initiators` JSON；新增 `load_totals_groups_usage_by_model_provider_and_initiator` 覆盖 stored model/provider/initiator 聚合与 error totals。
- Gaps: TUI 仍只解析旧 Dashboard model usage 展示；按规划留到 Phase 5C 与新 metrics contract 同步更新。模型能力 metadata 未实现，留到 Phase 5B。

Discovered tasks（发现的后续任务）:

- 若上游后续强制要求 Responses `instructions`，再评估 OpenAI/Copilot provider-specific 处理。
- usage/cost metrics 增强需单独规划：可能涉及 server metrics schema、admin API 响应、SQLite migration/aggregation 与 TUI 展示。

Resume next（下次继续）:

- Phase 5A 已完成 server-only metrics shape enrichment；下一步建议推进 Phase 5B model capability metadata，或先做 Phase 5C TUI metrics display 以消费新增 provider/initiator 维度。

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