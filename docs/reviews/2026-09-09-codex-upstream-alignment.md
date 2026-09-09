# Codex 上游对齐审阅：2026-09-09

本轮完成增量审阅并实施四项修改：SSE 取消释放、GPT-6 Astra 能力及请求适配、ChatGPT 路由 Cookie、模型缓存身份隔离。原生输入项、响应 ID 和扩展元数据继续透传。完整 workspace 的 602 项测试及 Clippy 已通过；修改纳入 `v4.0.2` 发布。

## 范围和同步状态

| 项目 | 记录 |
| --- | --- |
| claude-proxy | `v4.0.2`，审阅起点为 `ce6ddef5f9a5b89daaf21ad53f4ff514dd2d92a9` |
| 上次审阅 / Codex before_sha | `8e6a44b428e31f91b21edc97904fcdf4f0931ade` |
| 本次获取并审阅的 origin/main | `0d46c252b3f29f10bacf0ef58a17a1aa5d17ead3` |
| 精确审阅范围 | `8e6a44b428e31f91b21edc97904fcdf4f0931ade..0d46c252b3f29f10bacf0ef58a17a1aa5d17ead3` |
| 增量规模 | 231 个提交；1,346 个文件；97,990 行新增、22,579 行删除 |
| Codex after_sha | `0d46c252b3f29f10bacf0ef58a17a1aa5d17ead3`，已完成强制同步 |
| 分支与远端 | `main` 跟踪 `origin/main`；`https://github.com/openai/codex.git` |
| 本地修改处理 | Codex 的 `AGENTS.md` 末尾空行改动已覆盖，工作区干净；代理原有未跟踪文件 `.cargo/`、`.claude/settings.json` 保留 |

用户明确要求以远端为准强制更新本地，[技能](../../.agents/skills/codex-upstream-alignment/SKILL.md) 已相应修改。重新执行 `git fetch origin` 后，使用 `git reset --hard 0d46c252b3f29f10bacf0ef58a17a1aa5d17ead3` 和 `git clean -fd` 完成同步；核对 `HEAD` 与远端一致，`git status --short` 为空。

同步覆盖了本地 `AGENTS.md` 的末尾空行，应用上游删除两条过时 app-server README 指引的变更；没有未跟踪文件需要清理。Codex 工作区现已与本报告审阅提交一致。强制同步策略仅用于 Codex 上游参考仓库。

## 契约边界

- **公开 OpenAI API**：支持普通响应和 SSE 流式响应；GPT-6 Astra 的模型页声明 Responses、Chat Completions 和推理能力。[流式指南](https://developers.openai.com/api/docs/guides/streaming-responses)、[GPT-6 Astra](https://developers.openai.com/api/docs/models/gpt-6-astra)。
- **代理当前子集**：`/v1/responses` 仅接受 `stream=true`，强制 `store=false`，拒绝非空的 `previous_response_id` / `conversation`；只路由到 OpenAI、ChatGPT。GET WebSocket 探测返回 426。这是现有公开边界，不因本轮审阅扩大。依据：[入口校验](../../crates/claude-proxy-server/src/openai.rs)、[路由](../../crates/claude-proxy-server/src/routes.rs)。
- **ChatGPT/Codex 后端**：字符串输入转为列表，去掉 `max_output_tokens`；Lite 模式强制 `reasoning.context=all_turns`、`parallel_tool_calls=false`。这些仍沿用 v4.0.1 的既有验证，本轮没有重新调用真实后端。公开参数限制不能用这些私有适配规则替代。

## 增量对齐矩阵

状态中的“已对齐”表示代理代码和所列测试覆盖了相关传输行为，不代表对每个真实模型重新验证过功能支持。

| 上游提交 / 文件依据 | 线上协议或行为变化 | 代理当前行为 | 状态 | 建议动作 | 兼容性影响 | 验证要求 |
| --- | --- | --- | --- | --- | --- | --- |
| [56a8470aa](https://github.com/openai/codex/commit/56a8470aa)、[31ccaf40c](https://github.com/openai/codex/commit/31ccaf40c)、[35d9e4bc4](https://github.com/openai/codex/commit/35d9e4bc4)；`core/src/session/reasoning_effort.rs`、`compact_remote_request.rs` | 通过 `configuration_update` 记录有效 effort；请求级 effort 保持初始值；成功压缩后重新建立基线 | 原生路径保留整个 `input` 数组和请求级 reasoning；已有 fixture 明确包含配置项、`compaction_trigger` | 已对齐；特定模型的实际效果待实测 | 保留项顺序及两个 effort 层次；无需把 Codex 会话状态搬进代理 | 清除配置项、由配置项反写请求级 effort 会改变含义与缓存前缀；配置的模型别名仍按既有策略覆盖请求级 effort | 已通过原生输入、压缩集成测试；后续可补 low→high→low 跨请求断言 |
| [dee21ec1b](https://github.com/openai/codex/commit/dee21ec1b)；`codex-api/src/common.rs`、`sse/responses.rs`、`core/src/client.rs` | 从 `response.created.response.id` 获取父响应标识；普通请求可带 `client_metadata.guardian_credits_requested`，Guardian 请求带 `parent_response_id` | 原生 SSE 保留真实 ID；body 扩展字段保留；没有 Guardian 专用下游路由 | 已对齐：普通 Responses 透传；不适用：Guardian 执行流程 | 保持 ID 和元数据；无需实现审批执行器或已被替换的 ticket 协议 | 响应 ID 保留不等于代理支持 `previous_response_id` 有状态续接 | 现有未知项 / 事件覆盖，加专项 ID 与 Guardian 元数据断言即可；额度语义需后端验证 |
| [d05e6d5f4](https://github.com/openai/codex/commit/d05e6d5f4)；`core/src/responses_metadata.rs`、`turn_metadata.rs` | 独立任务和 memory 请求有稳定 `turn_id` / `root_turn_id`；memory 请求仍省略会话和线程身份 | 保留 `client_metadata` 和 `x-codex-turn-metadata`；ChatGPT 请求构造器在缺失时仍补运行时 session/thread headers | 已对齐：身份字段透传；待实测：memory 请求省略语义 | 不重写客户提供的 turn/root；若支持 detached memory 直连，再验证补充 header 的影响 | 普通聊天不受影响；不能据此声称完整 memory 后端兼容 | header 过滤测试已通过；memory 专用请求暂无真实后端复测 |
| [7769bccbb](https://github.com/openai/codex/commit/7769bccbb)；`codex-api/src/sse/responses.rs` | SSE 消费者关闭时立即停止上游轮询 | provider 监听消费者关闭；server 监听主请求及 follower 退出 | 已修复 | 保留取消回归，继续允许其他 follower 消费 | 最后一个消费者退出时释放连接和许可；不依赖下次 heartbeat | 空闲 TCP EOF、主请求断开后 follower 持续接收、最后一个 follower 退出均通过测试 |
| [a97cf1b72](https://github.com/openai/codex/commit/a97cf1b72)、[6af345407](https://github.com/openai/codex/commit/6af345407)；`models-manager/models.json` | `gpt-6-astra.visibility` 从 hide 改为 list；新增实验上下文能力 | Astra 的 OpenAI 目录声明 Responses、推理、模态和限制；Messages 使用 Responses，保留 verbosity | 已修复 | 仅匹配已核对的精确模型名 | 模型发现恢复完整信息；Astra 不再声明会被转换路径舍弃的 sampling/stop 控制 | /v1/models→Messages→mock Responses 全链路及 reasoning 别名测试通过 |
| 同上；`models-manager/models.json`、代理 `chatgpt.rs` | ChatGPT 在线目录可以显示 Astra | 在线目录继续动态映射；新增 Astra 离线回退，默认 272K context、Lite、priority 和 ultra 别名支持 | 已修复 | 以 Codex 0d46c252b 的默认目录字段作为回退依据 | 使用 272K 默认窗口，不自动启用可选 872K 最大窗口，不复制公开 API 的 1.05M 限制 | 在线目录 fixture、离线 capability 和 Lite/effort 规范化测试通过；真实账户访问未验证 |
| [f31bd3adf](https://github.com/openai/codex/commit/f31bd3adf)、[f046cf35d](https://github.com/openai/codex/commit/f046cf35d)；`model-provider/src/models_identity.rs`、`models-manager/src/cache.rs` / `manager.rs` | 缓存绑定 provider、有效路由、账号/用户/计划或 API 凭证；发布结果与续 TTL 时再次校验身份 | server 绑定 provider 实例和身份；ChatGPT 使用可用 account/user/email/plan 与额外路由 headers 的摘要；持久化格式升级到 v2 | 已修复 | 保持已知账号令牌轮换复用，拒绝旧身份/旧实例的刷新结果 | 旧能力缓存变为 miss；不修改 token 文件格式；不完整身份保守随凭证变化失效 | 身份切换、套餐变化、路由 headers、同身份令牌轮换、刷新竞态、URL path 大小写和旧缓存拒绝测试通过 |
| [94e4b3d0b](https://github.com/openai/codex/commit/94e4b3d0b)；`http-client/src/chatgpt_cloudflare_cookies.rs` | ChatGPT HTTP 客户端保存并按作用域重放 `__oailb` 基础设施路由 Cookie | ChatGPT 客户端单独保存 __oailb；限定已知 HTTPS ChatGPT host，并使用 Jar 验证 domain/path/Secure/expiry | 已实现；线上收益待实测 | 保持只有该上游路由 Cookie 可存储，保留下游凭证过滤 | 各客户端独立；自定义或不受信任 host 不存储 Cookie；真实路由收益不由 mock 推断 | 接受/重放、过期删除、跨 host/path 和不可信来源拒绝测试通过 |
| [4e48cd02d](https://github.com/openai/codex/commit/4e48cd02d)、[6af345407](https://github.com/openai/codex/commit/6af345407)、[8e694e955](https://github.com/openai/codex/commit/8e694e955)；`protocol/src/openai_models.rs` | 模型目录增加 Guardian 策略和 experimental context；bundled catalog 移除大部分 base instructions | 代理解析所需能力并忽略额外目录字段；使用自己的模型 API，不执行 Codex 会话策略 | 不适用：客户端策略；目录解析已兼容未知字段 | 不把这些字段伪装成代理实现的能力 | 原始 Codex `/models` 与代理 `/v1/models` 不是同一完整 schema | 模型反序列化 / 映射测试；新增原始目录 fixture 可防回归 |
| [7769bccbb](https://github.com/openai/codex/commit/7769bccbb)；`ext/guardian-v2/.../connection_pool.rs` | Guardian 分类优先用健康 WS，无可用连接时走 HTTP，并在后台补池 | 原生下游 Responses 使用 SSE；ChatGPT Messages 的内部 WS 路径另有重试与冷却 | 不适用：Guardian 池；通用取消问题见上 | 不因内部分类器变化增加公开 WS 端点或改变现有回退规则 | 内部支持 WS 不代表下游支持 WS | 已通过 GET `/v1/responses` 返回 426 的测试 |
| [929389f59](https://github.com/openai/codex/commit/929389f59)、[b01c3986f](https://github.com/openai/codex/commit/b01c3986f)、[86b1b359c](https://github.com/openai/codex/commit/86b1b359c)、[f326857cf](https://github.com/openai/codex/commit/f326857cf) | 图片 generation ID、WebRTC 语音、本机用户验证、staging 登录配置 | 代理没有对应图片生成、Realtime、app-server 用户验证端点 | 不适用 | 不扩展当前产品范围 | 用户输入的图片和 Responses 中未知 item 的透传仍按现有路径处理 | 本轮不需为这些客户端功能添加代理测试 |
| [0d46c252b](https://github.com/openai/codex/commit/0d46c252b)、[32351a7b1](https://github.com/openai/codex/commit/32351a7b1)、[6d377e96e](https://github.com/openai/codex/commit/6d377e96e) | 工具执行元数据封装和在线工具目录刷新；最终 request body 继续携带工具定义与输出扩展字段 | 原生 body 使用 JSON Value 保留工具、结果和未知字段，不缓存 Codex 工具目录 | 已对齐：body 透传；不适用：客户端刷新机制 | 保留现有未知项覆盖，不实现 MCP 工具运行时 | Messages 翻译路径不是任意 Codex item 的无损接口 | `native_codex_input`、未知字段和 custom tool 测试已通过 |

## 实施范围与兼容性

1. **取消传播**：`responses.rs` 的后台读取监听 `tx.closed()`；`routes.rs` 立即识别下游关闭，并在主请求关闭后等待最后一个 follower 退出。主请求断开不再取消仍有消费者的共享流。
2. **Astra**：OpenAI 精确模型声明 1,050,000 context、128,000 output 和 low/medium/high/xhigh/max；Messages 模式适配 verbosity 与 reasoning 别名。ChatGPT 使用 Codex 目录中的 272K 默认窗口及 Lite 配置，`ultra` 仍作为代理的委派选择映射到后端 `max`，保留原有工具门控。
3. **路由 Cookie**：在 ChatGPT HTTP client 安装每客户端独立的 `__oailb` Jar，限制为 `chatgpt.com` / `chat.openai.com` 的 HTTPS 请求。新增 reqwest cookies feature；Cookie 解析和作用域由标准库依赖负责，业务模块不手写解析器。
4. **缓存身份**：`Provider::model_cache_identity` 为 server 提供同步身份快照；缓存的读取、枚举和发布均检查当前 provider 实例及身份。ChatGPT 身份从已有 access token 中可用的 account/user/email/plan 推导，缺少稳定信息时使用不落明文的凭证摘要，并加入额外请求 headers。Warmup 复用相同的缓存入口。持久化能力缓存升级 v2，旧条目重新获取，URL 规范化保留路径大小写。

鉴权令牌的存储格式不变；此处解码 JWT 只用于缓存分区，不构成令牌验证。当前 provider 的 token 刷新和重新创建会影响缓存身份，未新增对外部 token 文件的热监听。遇到刷新期间身份或 provider 改变，会放弃结果并允许后续调用重新获取；不会将旧目录发布给新身份。

仍需真实后端验证的范围包括 Astra 账户可用性、Cookie 的线上亲和性收益以及 detached memory 请求省略 session/thread header 的语义。Guardian 执行器、Realtime 和图片生成端点继续不属于本轮实现范围。

公开指南还要求配置更新项按原始位置重放，避免相邻更新，并限定所支持的模型和模式；这些约束应与本轮 Codex feature gating 分开理解。代理宜保留数据，由对应上游执行模型语义校验，不能从一次 mock 透传成功推断模型接受所有配置组合。[推理更新指南](https://developers.openai.com/api/docs/guides/reasoning#change-reasoning-mid-conversation)

## 验证与证据

最终检查全部通过：

```text
cargo fmt --all -- --check
cargo test --locked --workspace --all-targets -- --quiet
  CLI: 43 passed; config: 30 passed; core: 11 passed
  providers: 389 passed; server unit: 110 passed; integration: 19 passed
  total: 602 passed
cargo clippy --locked --workspace --all-targets -- -D warnings
git diff --check
```

完整测试首次在沙箱内因禁止监听本机端口而失败；在具备本机端口权限的执行环境重跑后全部通过。提供商验证使用本机 mock 与虚拟凭证，没有调用真实服务。新增测试包括：

- `dropping_responses_consumer_closes_idle_upstream_socket`：首事件后保持上游空闲，drop 消费者后在 2 秒测试上限内观察 TCP EOF。
- `responses_disconnect_releases_idle_stream_after_last_consumer` / `responses_follower_disconnect_unsubscribes_without_heartbeat`：关闭最后消费者后释放流、订阅及并发许可；既有 follower 持续消费和背压测试同时通过。
- `astra_model_catalog_and_messages_use_responses_capabilities` / `astra_messages_normalize_reasoning_aliases_without_losing_controls`：核对实际目录返回、Messages 路由、推理参数与其他 controls。
- `astra_chatgpt_catalog_and_lite_normalization_follow_codex`：分别验证 ChatGPT 回退与在线目录，不套用公开 API 限制。
- Cookie 模块的作用域、失效与客户端隔离测试。
- `model_catalog_rejects_response_after_auth_identity_changes` / `model_cache_rejects_refresh_finishing_after_identity_or_provider_change`：确定性挂起刷新后改变身份或替换 provider，证明旧结果被拒绝；另有所有缓存视图失效、令牌轮换、套餐/headers 变更及旧格式缓存测试。

初次审阅的两个临时探针曾确认取消后 750ms 连接仍存活、Astra 目录仅声明 Chat Completions。这些修复前观察现由上述长期回归测试覆盖。

主要代理证据：

- [输入项校验](../../crates/claude-proxy-server/src/openai.rs)、[原生路由及 header 过滤](../../crates/claude-proxy-server/src/routes.rs)、[SSE 编码](../../crates/claude-proxy-server/src/downstream.rs)。
- [共享 Responses 解码和后台任务](../../crates/claude-proxy-providers/src/responses.rs)、[上游超时](../../crates/claude-proxy-providers/src/http.rs)。
- [OpenAI 能力与模型分支](../../crates/claude-proxy-providers/src/openai_compat.rs)、[OpenAI provider](../../crates/claude-proxy-providers/src/openai.rs)。
- [ChatGPT 目录、请求规范化、HTTP client](../../crates/claude-proxy-providers/src/chatgpt.rs)、[持久化 context cache](../../crates/claude-proxy-providers/src/chatgpt/capability_cache.rs)、[服务端模型缓存](../../crates/claude-proxy-server/src/app.rs)。
- [集成 fixture 与 Responses 断言](../../crates/claude-proxy-server/tests/integration.rs)。

本次已筛查全部提交标题和文件增量，深入审阅可能影响代理的 API、模型、请求构造、SSE、认证缓存和传输变化；TUI、沙箱、持久化、构建及本地工具执行变化按传输影响排除。Codex 审阅基线与上游工作区均为 `0d46c252b`；代理侧四项修改已实施，真实后端验证边界单独保留。
