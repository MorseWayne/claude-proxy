# Thinking 渲染兼容修复设计

Date: 2026-05-19

## 背景

Claude Code 只把 Anthropic Messages 流中的结构化 thinking block 当作 thinking 渲染：`content_block_start.content_block.type = "thinking"` 后接 `content_block_delta.delta.type = "thinking_delta"`。如果上游把推理内容作为普通 `text_delta` 返回，即使文本里有 `[thinking]...[/thinking]` 标记，Claude Code 也会把它显示在当前会话正文中。

GitNexus 对 `claude-code` 的分析确认：`text_delta` 会进入 `streamingText`；`thinking_delta` 不进入普通文本显示。`claude-proxy` 当前仅处理 typed reasoning 字段：Chat Completions 的 `reasoning_content`、Responses 的 `response.reasoning_*` 事件；普通 content/output_text 中的 tagged thinking 仍会作为 text 输出。

## 目标

- 在 OpenAI-compatible 输出转换层统一识别 tagged thinking，避免 thinking 标记泄漏到 Claude Code 可见正文。
- 保持 typed reasoning 字段优先，不改变 Anthropic passthrough。
- 覆盖 Chat Completions 与 Responses 的 streaming 和 non-streaming 路径，包括 OpenAI、ChatGPT、Copilot 复用路径。
- 通过共享状态机避免两套转换器行为漂移。

## 非目标

- 不修改 Claude Code 源码。
- 不改变 Anthropic 原生 provider 的转发语义。
- 不尝试语义判断普通文本是否“像思考”；只识别明确标记，降低误伤。

## 设计

新增 provider 内部共享 tagged-thinking splitter。它接收上游普通文本片段，输出 `Text(String)` 或 `Thinking(String)` 片段；调用方负责把片段映射为 Anthropic SSE block。

识别标记：

- `[thinking]` / `[/thinking]`
- `<thinking>` / `</thinking>`

行为：

- 标记本身不输出。
- 标记内文本输出为 thinking 片段。
- 标记外文本保持 text 片段。
- 支持标记跨 chunk 拆分。
- 支持单个 delta 内多次 text/thinking 切换。
- 响应结束时如果仍处于 thinking 状态，由转换器正常关闭 thinking block。
- 未出现明确 opening tag 的文本保持原样；孤立 closing tag 不触发隐藏。

接入点：

- Chat Completions streaming：`StreamConverter.process_chunk` 对 `choice.delta.content` 使用 splitter；`reasoning_content` 仍直接输出 typed thinking。
- Chat Completions non-streaming：`message.content` 先经 splitter，再输出 text/thinking blocks。
- Responses streaming：`response.output_text.delta` 使用 splitter；`response.reasoning_summary_text.delta` / `response.reasoning_text.delta` 仍直接输出 typed thinking。
- Responses non-streaming：`output_text` / `refusal` 经过 splitter； reasoning item 仍直接输出 typed thinking。

事件顺序约束：

- text 片段只能进入 `content_block_start(type=text)` + `text_delta`。
- thinking 片段只能进入 `content_block_start(type=thinking)` + `thinking_delta`。
- block 类型切换前必须发送 `content_block_stop`。
- tool/function block 开始前必须关闭已打开的 text/thinking block。

## 验证计划

- Chat Completions 单测：
  - `reasoning_content` 仍输出 thinking。
  - `content` 中完整 `[thinking]...[/thinking]` 输出 thinking。
  - tag 跨 chunk 拆分仍输出 thinking。
  - thinking 后正文重新输出 text。
  - 无标记普通文本不变。
- Responses 单测：
  - typed reasoning event 仍输出 thinking。
  - `response.output_text.delta` 中 tagged thinking 输出 thinking。
  - tag 跨 chunk、多段切换正常。
  - 无标记普通文本不变。
- Non-streaming 单测：
  - Chat Completions `message.content` tagged thinking。
  - Responses `output_text` tagged thinking。
- 工作区验证：
  - `cargo fmt --check`
  - provider 目标测试
  - `cargo test -p claude-proxy-providers`
  - `cargo test` 如时间允许
  - `cargo clippy -- -D warnings`
  - `gitnexus_detect_changes(scope=all)`

## 风险与缓解

- 风险：隐藏用户本来想显示的示例标签。缓解：只识别明确 opening tag；没有 opening tag 的普通文本不变。
- 风险：流式 tag 跨 chunk 解析导致延迟少量文本。缓解：只缓存最长可能 tag 前缀，未匹配时及时释放为 text。
- 风险：Responses 主路径影响面高。缓解：仅在 `output_text.delta` / non-streaming text 输出分流，不改变 function/tool/reasoning typed event 行为，并补主路径回归测试。
