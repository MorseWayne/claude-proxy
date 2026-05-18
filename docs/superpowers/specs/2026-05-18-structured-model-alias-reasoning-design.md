# 结构化模型别名推理强度设计

Date: 2026-05-18

## 背景

当前 `[model]` 配置使用扁平字符串表示模型别名：`default` 为必填 `provider_id/model_name`，`reasoning`、`opus`、`sonnet`、`haiku` 为可选字符串。请求进入 server 后通过 `Settings::resolve_model()` 把 Claude 侧模型名解析成上游 `provider/model`，再由 provider 层根据请求中的 `thinking`、`reasoning`、`reasoning_effort` 或 intent 自动推导 OpenAI-compatible `reasoning_effort`。

新需求是：设置模型别名时可以同时设置推理强度；每个别名独立配置；也可以不设置，或者显式设置为默认策略。

## 目标

- `default`、`reasoning`、`opus`、`sonnet`、`haiku` 每个模型别名都能携带可选 `reasoning_effort`。
- 支持结构化 TOML 配置，表达模型名与推理强度的组合。
- 保持旧扁平字符串配置可读，避免现有用户升级后配置失效。
- 明确定义 `unset` 与显式 `default` 的差异：两者都不强制注入固定 effort，显式 `default` 只用于配置可见性与用户意图表达。
- 请求显式字段优先于模型别名配置。

## 非目标

- 不改变 provider 返回的模型能力元数据结构。
- 不改变 Anthropic 原生请求字段语义。
- 不为直接请求 `provider/model` 增加隐式别名配置。
- 不重新设计 intent 到 reasoning effort 的现有推导规则。

## 配置设计

目标 TOML 形态：

```toml
[model.default]
name = "openai/gpt-5"
reasoning_effort = "default"

[model.reasoning]
name = "openai/gpt-5"
reasoning_effort = "high"

[model.opus]
name = "copilot/claude-opus-4-7"

[model.sonnet]
name = "anthropic/claude-sonnet-4-6"
reasoning_effort = "default"

[model.haiku]
name = "openai/gpt-5.4-mini"
reasoning_effort = "none"
```

结构：

- `ModelAliasConfig`
  - `name: String`
  - `reasoning_effort: Option<ModelReasoningEffort>`
- `ModelReasoningEffort`
  - `Default`
  - `Disabled`（序列化为 `"none"`）
  - `Low`
  - `Medium`
  - `High`
  - `XHigh`
- `ModelConfig`
  - `default: ModelAliasConfig`
  - `reasoning: Option<ModelAliasConfig>`
  - `opus: Option<ModelAliasConfig>`
  - `sonnet: Option<ModelAliasConfig>`
  - `haiku: Option<ModelAliasConfig>`

兼容读取旧配置：

```toml
[model]
default = "openai/gpt-4.1"
reasoning = "openai/gpt-5"
opus = "anthropic/claude-opus-4-7"
```

旧字符串反序列化为 `{ name = <string>, reasoning_effort = None }`。保存配置时可以输出新的结构化形态；这是可接受的格式迁移，因为旧输入仍兼容。同一个 alias 不支持同时出现旧字符串值和同名子表，例如 `default = "..."` 与 `[model.default]` 并存应按 TOML 解析错误处理。不同 alias 可以混用新旧格式，以便用户逐步迁移。

## 请求解析与优先级

新增一个解析结果类型，例如 `ResolvedModelAlias`：

- `model_ref: String`
- `reasoning_effort: Option<ModelReasoningEffort>`

`Settings::resolve_model()` 可继续保留字符串返回值以降低调用方破坏面；新增方法负责返回结构化解析结果。server 请求路径使用结构化解析结果：

1. 如果请求模型本身包含 `/`，视为直接 `provider/model`，不应用别名推理强度。
2. 否则按 alias 规则解析：
   - 请求模型名包含 `opus` 时优先使用 `model.opus`。
   - 请求模型名包含 `haiku` 时使用 `model.haiku`。
   - 请求模型名包含 `sonnet` 时使用 `model.sonnet`。
   - 请求模型名包含 `reasoning`，或请求 `metadata.intent` 为 `deep_think` / `reasoning`，并且未命中上面的家族 alias 时，使用 `model.reasoning`。
   - 未命中任何 alias 时，沿用 `model.default.name` 的 provider + 请求模型名；这条路径没有 alias-specific reasoning effort。
3. 将 `model_ref` 拆为 `provider_id` 和 `upstream_model`。
4. 如果请求没有显式 `reasoning`、`reasoning_effort` 或 `thinking`，并且别名配置的 `reasoning_effort` 是固定值，则注入到请求 `extra["reasoning_effort"]`。
5. 如果别名配置是 `Default` 或字段缺失（Rust `Option::None`），不注入字段，继续让现有 provider intent 逻辑决定是否设置 effort。

固定值映射：

- `Disabled` → `"none"`
- `Low` → `"low"`
- `Medium` → `"medium"`
- `High` → `"high"`
- `XHigh` → `"xhigh"`

注意：配置枚举中的 `Disabled` 是显式关闭/最低 reasoning 的用户配置值，对应字符串 `none`，会注入 `reasoning_effort = "none"`；Rust `Option::None` 表示未配置，不注入字段。`Default` 是显式记录“使用默认策略”，行为与未配置相同，但会在结构化配置中保留用户意图。

## TUI 与 CLI 体验

TUI Model 页面从“每个别名单行模型字符串”扩展为两列配置表：第一列编辑模型名，第二列编辑对应的推理强度。行仍按 alias 组织，避免把每个 alias 展开成多行。

| Alias | Model | Reasoning Effort |
| --- | --- | --- |
| Default | `model.default.name` | `model.default.reasoning_effort` |
| Reasoning | `model.reasoning.name` | `model.reasoning.reasoning_effort` |
| Opus | `model.opus.name` | `model.opus.reasoning_effort` |
| Sonnet | `model.sonnet.name` | `model.sonnet.reasoning_effort` |
| Haiku | `model.haiku.name` | `model.haiku.reasoning_effort` |

键盘交互沿用现有 Model 页编辑方式：上下选择 alias 行，左右切换 Model / Reasoning Effort 列，Enter 编辑当前单元格。

空 effort 表示未配置；可输入值为 `default`、`none`、`low`、`medium`、`high`、`xhigh`。TUI 保存时写回结构化配置。

CLI `provider switch`、provider add 时选择默认模型，只更新 `model.default.name`，保留或清空 `model.default.reasoning_effort` 的行为应明确采用“保留已有 effort”。首次创建默认配置时 effort 为空。

Claude Code env 同步仍只同步模型名相关环境变量，不同步 reasoning effort，因为 Claude Code 环境变量目前只表达模型选择。

## 验证计划

- config 单元测试：
  - 新结构化 TOML 可反序列化。
  - 旧字符串 TOML 可反序列化为结构化 alias。
  - 不同 alias 新旧格式混用可反序列化；同一个 alias 的字符串值与子表并存按 TOML 错误处理。
  - `to_toml()` 输出结构化字段。
  - validation 校验所有 alias 的 `name` 必须是 `provider_id/model_name`。
  - invalid `reasoning_effort` 被拒绝。
- server/request 单元测试：
  - alias 固定 effort 在无显式请求字段时注入。
  - alias `reasoning_effort = "none"` 注入 `reasoning_effort = "none"`。
  - 请求显式 `reasoning_effort`、`reasoning`、`thinking` 优先。
  - `default` 与未配置都不注入，保留现有 intent 推导。
  - 直接 `provider/model` 不应用 alias effort。
  - 旧格式配置读取后可完成解析并代理请求。
  - `model.reasoning` 在请求模型名或 intent 命中 reasoning 规则时生效，且不覆盖 opus/haiku/sonnet 家族 alias。
- TUI 单元测试：
  - Model 页面编辑模型名与 effort 字段后正确写入 settings。
  - 空 effort 清空配置。
  - 清空可选 alias 的 model name 会删除对应 alias section，而不是保留空 name。
  - invalid effort 显示错误且不保存。
  - `provider switch` 和 provider add 更新默认模型名时保留已有 `model.default.reasoning_effort`。
- 工作区验证：
  - `cargo fmt --check`
  - `cargo test -p claude-proxy-config`
  - 相关 server/TUI 目标测试
  - 必要时 `cargo test`
  - `cargo clippy -- -D warnings`

## 风险与缓解

- 风险：结构化 TOML 可能破坏旧配置读取。缓解：为 alias 字段实现兼容反序列化，并加旧格式测试。
- 风险：显式关闭和未配置语义混淆。缓解：代码中使用 `ModelReasoningEffort::Disabled` 表示序列化字符串 `"none"`，并保留 `Option::None` 专门表示未配置。
- 风险：`Default` 与未配置运行时行为相同，可能让用户困惑。缓解：文档和 TUI copy 中说明 `default` 是显式记录“使用默认策略”，用于可见配置与人工审阅；空值表示未表达偏好。
- 风险：server 解析调用方过多。缓解：保留 `resolve_model()`，新增结构化方法供请求路径使用。
- 风险：TUI 页面列宽不足或现有组件不支持表格单元格编辑。缓解：仍保持 alias 行 + Model / Reasoning Effort 左右并列；必要时用当前选中 alias 的 inline detail panel 展示两个并列字段，不退回每个 alias 上下拆成两行。
