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
  - `None`
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

旧字符串反序列化为 `{ name = <string>, reasoning_effort = None }`。保存配置时可以输出新的结构化形态；这是可接受的格式迁移，因为旧输入仍兼容。

## 请求解析与优先级

新增一个解析结果类型，例如 `ResolvedModelAlias`：

- `model_ref: String`
- `reasoning_effort: Option<ModelReasoningEffort>`

`Settings::resolve_model()` 可继续保留字符串返回值以降低调用方破坏面；新增方法负责返回结构化解析结果。server 请求路径使用结构化解析结果：

1. 如果请求模型本身包含 `/`，视为直接 `provider/model`，不应用别名推理强度。
2. 否则按现有 alias 规则匹配 `opus`、`haiku`、`sonnet`，并补充 `reasoning` 别名匹配；未命中时沿用 default provider + 请求模型名。
3. 将 `model_ref` 拆为 `provider_id` 和 `upstream_model`。
4. 如果请求没有显式 `reasoning`、`reasoning_effort` 或 `thinking`，并且别名配置的 `reasoning_effort` 是固定值，则注入到请求 `extra["reasoning_effort"]`。
5. 如果别名配置是 `Default` 或 `None`，不注入字段，继续让现有 provider intent 逻辑决定是否设置 effort。

固定值映射：

- `None` → `"none"`
- `Low` → `"low"`
- `Medium` → `"medium"`
- `High` → `"high"`
- `XHigh` → `"xhigh"`

注意：配置枚举中的 `None` 是显式关闭/最低 reasoning 的用户配置值，对应字符串 `none`；Rust `Option::None` 表示未配置。

## TUI 与 CLI 体验

TUI Model 页面从“每个别名单行模型字符串”扩展为“别名名 + model name + reasoning effort”的编辑能力。最小实现可采用每个别名两行：

- `Default Model`
- `Default Reasoning Effort`
- `Reasoning Model`
- `Reasoning Effort`
- `Opus Alias`
- `Opus Reasoning Effort`
- `Sonnet Alias`
- `Sonnet Reasoning Effort`
- `Haiku Alias`
- `Haiku Reasoning Effort`

空 effort 表示未配置；可输入值为 `default`、`none`、`low`、`medium`、`high`、`xhigh`。TUI 保存时写回结构化配置。

CLI `provider switch`、provider add 时选择默认模型，只更新 `model.default.name`，保留或清空 `model.default.reasoning_effort` 的行为应明确采用“保留已有 effort”。首次创建默认配置时 effort 为空。

Claude Code env 同步仍只同步模型名相关环境变量，不同步 reasoning effort，因为 Claude Code 环境变量目前只表达模型选择。

## 验证计划

- config 单元测试：
  - 新结构化 TOML 可反序列化。
  - 旧字符串 TOML 可反序列化为结构化 alias。
  - `to_toml()` 输出结构化字段。
  - validation 校验所有 alias 的 `name` 必须是 `provider_id/model_name`。
  - invalid `reasoning_effort` 被拒绝。
- server/request 单元测试：
  - alias 固定 effort 在无显式请求字段时注入。
  - 请求显式 `reasoning_effort`、`reasoning`、`thinking` 优先。
  - `default` 与未配置都不注入，保留现有 intent 推导。
  - 直接 `provider/model` 不应用 alias effort。
- TUI 单元测试：
  - Model 页面编辑模型名与 effort 字段后正确写入 settings。
  - 空 effort 清空配置。
  - invalid effort 显示错误且不保存。
- 工作区验证：
  - `cargo fmt --check`
  - `cargo test -p claude-proxy-config`
  - 相关 server/TUI 目标测试
  - 必要时 `cargo test`
  - `cargo clippy -- -D warnings`

## 风险与缓解

- 风险：结构化 TOML 可能破坏旧配置读取。缓解：为 alias 字段实现兼容反序列化，并加旧格式测试。
- 风险：`None` 作为枚举值和 `Option::None` 语义混淆。缓解：代码中使用清晰命名，例如 `ModelReasoningEffort::NoReasoning` 或 `Disabled`，序列化仍为 `"none"`。
- 风险：server 解析调用方过多。缓解：保留 `resolve_model()`，新增结构化方法供请求路径使用。
- 风险：TUI 页面行数增加后布局拥挤。缓解：先采用简单可滚动/现有 field rows 机制；若现有页面不支持滚动，再拆成当前选中别名的 name/effort 两个字段。
