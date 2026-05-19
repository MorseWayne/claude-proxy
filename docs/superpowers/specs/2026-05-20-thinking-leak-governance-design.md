# thinking 文本泄漏治理设计

## 背景

`claude-proxy` 在多个 provider 转换层之间使用 Anthropic Messages/SSE 作为统一内部格式。当前代码里结构化 reasoning 路径和普通文本降级路径并存：

- 正确路径：provider-native reasoning 会映射为 Anthropic `thinking_delta`。
- 风险路径：部分请求转换会把 `Content::Thinking` 序列化为普通文本 `[thinking]...[/thinking]`，上游一旦复述该文本，响应转换器会把它当作 `text_delta` 输出。

这会导致思考内容进入用户可见正文。本设计目标是彻底治理该类泄漏，同时保留结构化 `thinking_delta` 行为。

## 目标

- 普通正文 SSE 不再出现 `[thinking]`、`[/thinking]` 或 marker 内内容。
- provider-native reasoning 继续通过结构化 `thinking_delta` 输出。
- 请求转换不再主动把 `Content::Thinking` 降级为普通 text。
- 统计与测试不再把 `[thinking]` 哨兵当作正常协议表示。
- streaming chunk 拆分场景有回归测试覆盖。

## 非目标

- 不改 `claude-proxy-core` 的 `Content::Thinking` 数据模型。
- 不改 Anthropic passthrough 行为。
- 不改 server non-stream reconstruct 逻辑。
- 不引入配置开关或 legacy 模式。

## 推荐方案

采用安全优先分层治理：

1. 先在普通输出文本边界加 streaming-safe sanitizer，阻断已存在或上游复述的 marker 泄漏。
2. 再清理请求侧源头，停止生成 `[thinking]` 普通文本。
3. 最后清理统计和测试中的哨兵依赖。

## 架构设计

新增一个 provider 内部共享的 `thinking_sanitizer` 小模块，职责仅限于处理 assistant/output 普通文本。

模块边界：

- 输入：普通可见文本片段。
- 输出：可见文本片段，可能为空。
- 状态：记录是否处于 hidden thinking marker 内，以及为跨 chunk marker 匹配保留的有限前缀缓冲。
- 不处理：结构化 `thinking_delta`、`reasoning_content`、`Content::Thinking` 数据模型。

安全策略：

- `[thinking]...[/thinking]` 内部内容丢弃。
- marker 外内容继续输出。
- 未闭合 `[thinking]` 到流结束时丢弃。
- 嵌套 marker 以安全优先处理：进入 hidden 状态后直到第一个 `[/thinking]` 才恢复可见输出。
- 不记录 hidden 内容；如需观测，只统计 dropped byte count。

## 数据流

### 请求侧

- `Content::Thinking` 继续作为结构化内部类型存在。
- OpenAI Chat Completions 正规路径继续映射为 `reasoning_content`。
- Responses 和 Copilot Chat Completions 不再把 `Content::Thinking` 序列化为 `[thinking]` 普通 text；没有 provider-native replay 能力时省略历史 thinking。
- `should_include_encrypted_reasoning` 可以继续根据历史 thinking 判断是否请求 encrypted reasoning include，但不能把明文 thinking 放入 prompt。

### 响应侧

- `reasoning_content`、`response.reasoning_text.delta`、`response.reasoning_summary_text.delta` 继续输出 `thinking_delta`。
- `response.output_text.delta`、`response.refusal.delta`、`message.content`、Chat Completions `delta.content` 先经过 sanitizer，再输出 `text_delta`。
- sanitizer 返回空字符串时不发空 `text_delta`；调用点应尽量在打开 text block 前完成净化，避免生成只包含 hidden marker 的空文本块。

## 修改边界

主要修改 `claude-proxy-providers`：

- `crates/claude-proxy-providers/src/responses.rs`
  - `ResponsesStreamConverter` 持有 sanitizer 状态。
  - `response.output_text.delta` / `response.refusal.delta` / `response.content_part.added` 普通文本先净化。
  - `NonStreamingResponsesConverter::add_text_block` 对完整文本净化。
  - `append_message_items` 不再将 `Content::Thinking` 转为 `[thinking]` text。
  - 移除 `should_truncate_text_item` 对 `[thinking]` 的特殊豁免。
- `crates/claude-proxy-providers/src/chat_completions.rs`
  - `StreamConverter` 持有 sanitizer 状态并净化 `delta.content`。
  - 非流式 `message.content` 净化后输出。
  - 保持 `reasoning_content` → `thinking_delta` 不变。
- `crates/claude-proxy-providers/src/copilot/chat_completions.rs`
  - 移除 `Content::Thinking` → `[thinking]` text 的转换。
- `crates/claude-proxy-providers/src/openai_compat.rs`
  - 不再用 `[thinking]` 作为正常 thinking 统计依据。

不修改：

- `crates/claude-proxy-core/src/types.rs`
- `crates/claude-proxy-providers/src/anthropic.rs`
- `crates/claude-proxy-server/src/non_stream.rs`

## 错误处理

sanitizer 是纯文本状态机，不返回 runtime error。异常输入按安全优先处理：

- 不完整 start marker：暂存有限前缀，直到可判定。
- 未闭合 marker：丢弃 hidden 缓冲。
- marker 内异常内容：全部丢弃。
- 输出为空：调用方不发 `text_delta`。

## 测试计划

### Responses

- streaming 单 chunk marker 被净化。
- streaming marker 跨 chunk 拆分不泄漏。
- marker 前后 visible text 保留。
- 未闭合 marker 不输出 hidden 内容。
- `response.reasoning_text.delta` 仍输出 `thinking_delta`。
- non-streaming `output_text` 中 marker 被净化。
- `convert_to_responses` 不再生成 `[thinking]` text。

### Chat Completions

- streaming `delta.content` marker 被净化。
- streaming split marker 被净化。
- non-streaming `message.content` marker 被净化。
- `reasoning_content` 仍输出 `thinking_delta`。

### 请求转换与统计

- `copilot::convert_to_openai_chat` 不再生成 `[thinking]` text。
- `openai::convert_request` 的 `reasoning_content` 行为保持不变。
- request observability 不再把 `[thinking]` 当正常 thinking 统计。

## 验证

执行顺序：

1. `cargo fmt --check`
2. 目标测试：
   - `cargo test -p claude-proxy-providers responses::tests::`
   - `cargo test -p claude-proxy-providers chat_completions::tests::`
   - `cargo test -p claude-proxy-providers copilot::chat_completions::tests::`
   - `cargo test -p claude-proxy-providers openai_compat::tests::`
3. `cargo test -p claude-proxy-providers`
4. `cargo test`
5. `cargo clippy -- -D warnings`
6. `gitnexus_detect_changes(scope=all)`
7. 提交后运行 `npx gitnexus analyze`

## 风险与缓解

- `append_message_items` 影响面为 CRITICAL，先做输出 sanitizer，再清理源头。
- `process_chunk` 影响面为 MEDIUM，新增 focused regression tests 覆盖 OpenAI/Copilot streaming。
- 不修改结构化 `thinking_delta` 路径，避免误伤合法 reasoning 输出。
- 不引入配置开关，避免扩大行为矩阵。
