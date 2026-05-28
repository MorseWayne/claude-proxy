# Claude Code Third-Party Provider Quality Gates

Date: 2026-05-28
Status: Research notes for provider design
Scope: `claude-proxy` provider compatibility and Claude Code parity

## Goal

When adding or modernizing a third-party API provider, avoid the trap where the provider can answer requests but silently misses Claude Code optimizations that affect conversation quality, latency, token cost, long-session stability, and tool-use reliability.

These notes summarize provider-specific gates observed in Claude Code. They are intended as a design checklist for `claude-proxy` provider work, especially OpenAI/ChatGPT/Codex-style providers that proxy Anthropic Messages API traffic.

## Provider classification model

Claude Code makes two related but distinct decisions:

1. **API provider family** — `firstParty`, `bedrock`, `vertex`, or `foundry`.
2. **Official Anthropic base URL** — whether `ANTHROPIC_BASE_URL` is unset or points to `api.anthropic.com` (plus staging for Anthropic internal users).

A custom gateway using the Anthropic SDK path may still be `firstParty`, but many official-source optimizations are disabled when `isFirstPartyAnthropicBaseUrl()` is false.

For `claude-proxy`, provider work should avoid a single binary `is_third_party` decision. Prefer explicit capability flags per upstream.

## High-priority gates that affect quality or efficiency

### 1. Tool search / dynamic tool loading

Impact:

- Prevents all MCP and deferrable tool schemas from being loaded into the prompt up front.
- Reduces context pressure, prompt tokens, first-token latency, and model distraction in tool-heavy sessions.

Observed Claude Code gates:

- `ENABLE_TOOL_SEARCH` controls `tst`, `tst-auto`, or `standard` mode.
- Custom `ANTHROPIC_BASE_URL` defaults to disabled unless `ENABLE_TOOL_SEARCH` is explicitly set.
- Bedrock/Vertex use a different tool-search beta header than the official Claude API.
- Bedrock sends selected beta headers via extra body params instead of the normal `betas` field.

Provider requirements:

- Supports `defer_loading` on tool definitions.
- Supports `tool_reference` blocks returned by the tool-search tool.
- Accepts the correct beta/header/body placement for the upstream.
- Preserves discovered tool references across turns and compact boundaries.

`claude-proxy` design implication:

- Add a provider capability like `tool_search = { supported, header_kind, beta_location }` rather than assuming all non-official providers are unsafe.

### 2. Fine-grained tool streaming

Impact:

- Without fine-grained tool streaming, large tool inputs can be buffered until complete before deltas are emitted.
- Claude Code comments call out multi-minute hangs on large tool inputs without this path.

Observed Claude Code gate:

- `eager_input_streaming` is gated to `firstParty && isFirstPartyAnthropicBaseUrl()` even when the env override is set.
- Proxies, Bedrock, and Vertex are blocked because some deployments reject the field.

Provider requirements:

- Upstream accepts the per-tool `eager_input_streaming` field.
- Upstream streams partial tool input JSON safely.

`claude-proxy` design implication:

- Treat this as an explicit provider capability. If supported, forward or synthesize the Anthropic-compatible field; otherwise omit it deliberately.

### 3. Prompt cache scope and global system-prompt cache

Impact:

- Global prompt cache reduces repeated system prompt and stable prefix cost across turns/sessions.
- Missing it can increase latency and token cost in long sessions and subagent-heavy workflows.

Observed Claude Code gates:

- Global prompt cache scope is first-party only.
- Foundry is intentionally excluded despite being close to first-party, because rollout data was first-party-only.
- Default fallback uses ordinary/org-level prompt cache behavior.

Provider requirements:

- Supports Anthropic-style `cache_control`.
- Supports `prompt-caching-scope` semantics if global cache is desired.
- Has stable cache-key isolation so one user/session cannot poison another.

`claude-proxy` design implication:

- Separate `prompt_cache = basic` from `prompt_cache_scope = global`.
- Do not use provider-instance-global cache keys. Prefer explicit client conversation/session keys, or no reusable key.

### 4. Context management and thinking preservation

Impact:

- Affects long-session quality when prior thinking blocks or old tool results accumulate.
- Native `context_management` can preserve useful thinking while clearing expensive history.

Observed Claude Code gates:

- `CONTEXT_MANAGEMENT_BETA_HEADER` is included only where first-party-only betas are allowed.
- Current default allows first-party and Foundry, not Bedrock/Vertex.
- Tool clearing strategies are even more restricted; thinking preservation is broader but still beta-gated.

Provider requirements:

- Supports `context_management` request field.
- Supports required beta/header shape.
- Has equivalent behavior for thinking preservation and tool-result clearing.

`claude-proxy` design implication:

- Capability should distinguish `thinking_preservation` from `tool_result_clearing`.

### 5. Thinking and adaptive thinking support

Impact:

- Directly affects reasoning quality.
- Claude 4.6-class models may rely on adaptive thinking behavior.

Observed Claude Code gates:

- Unknown first-party and Foundry model strings default more optimistically.
- Bedrock/Vertex-style third-party paths default conservatively because model IDs may not follow Anthropic naming.
- `modelSupportsThinking`, `modelSupportsAdaptiveThinking`, and `modelSupportsISP` can all be false for custom model IDs unless explicitly overridden.

Provider requirements:

- Clear per-model capability contract for:
  - `thinking`
  - `adaptive_thinking`
  - `interleaved_thinking`

`claude-proxy` design implication:

- Avoid inferring capabilities only from string contains checks such as `sonnet-4` or `opus-4`.
- Add explicit model metadata or provider config overrides.

### 6. Effort / max effort

Impact:

- Affects reasoning depth, `ultrathink`, and `/effort` behavior.
- Incorrectly disabling effort can lower quality; incorrectly enabling it can cause upstream 400 errors.

Observed Claude Code gates:

- `modelSupportsEffort()` defaults true for unknown first-party models.
- Third-party unknown model strings default false.
- Built-in positive checks are mostly model-name based, e.g. Opus/Sonnet 4.6.

Provider requirements:

- Per-model support for `effort` and `max_effort`.
- Mapping from Claude Code effort levels to upstream-native reasoning controls when upstream is not Anthropic-compatible.

`claude-proxy` design implication:

- Include effort support in `ModelInfo`, not only in provider-level config.

### 7. Structured outputs and strict tool schemas

Impact:

- Affects output-format reliability and strict tool-use validation.
- Can improve tool-call correctness when supported.

Observed Claude Code gates:

- Structured outputs are allowed only for first-party and Foundry by default.
- Bedrock/Vertex are disabled unless future support is added.
- Strict tools are gated by structured-output support and beta availability.

Provider requirements:

- Supports Anthropic `output_config.format` or an equivalent that can be converted safely.
- Supports strict tool schemas without rejecting the request.

`claude-proxy` design implication:

- Model metadata should say whether structured outputs and strict tools are supported independently.

### 8. Token-efficient tool-use format

Impact:

- Reduces output tokens for tool calls.
- Matters in tool-heavy sessions and long-running agent loops.

Observed Claude Code gates:

- Controlled by first-party-only beta behavior and GrowthBook.
- Third-party default is false.
- Mutually exclusive with strict tool behavior in parts of Claude Code.

Provider requirements:

- Supports the token-efficient tool-use beta/header or an equivalent upstream format.

`claude-proxy` design implication:

- Keep this opt-in per provider/model until proven compatible.

### 9. Fast mode

Impact:

- Improves perceived output speed.
- User-visible interaction quality feature.

Observed Claude Code gates:

- Fast mode is rejected early for any provider other than first-party.
- Also depends on billing/overage/org state.

Provider requirements:

- Upstream supports an equivalent service tier or speed mode.
- Clear fallback behavior when upstream rejects the option.

`claude-proxy` design implication:

- If mapping to OpenAI service tier or Codex priority tier, expose it as provider-specific capability instead of pretending it is Anthropic fast mode.

### 10. Default model selection and model fallback

Impact:

- Wrong default model can silently lower quality.
- Third-party supply often lags first-party model launch cadence.

Observed Claude Code behavior:

- First-party default Sonnet may move to the newest Sonnet quickly.
- Third-party default Sonnet can intentionally lag to an older known-available model.
- Legacy Opus remapping is first-party-only because third-party providers may not have the latest model.
- 404/model-not-found paths suggest third-party fallback models.

Provider requirements:

- Explicit model matrix with canonical display name, upstream model id, context window, reasoning support, output support, and fallback.
- No blind reuse of Anthropic first-party aliases for third-party upstreams.

`claude-proxy` design implication:

- Keep provider-specific `ModelInfo` complete and conservative by default.

## Medium-priority gates

### Token counting and context estimates

Impact:

- Drives auto-compact timing, tool-search auto thresholds, context visualization, and oversized-result handling.

Observed Claude Code behavior:

- Bedrock needs a custom token-count path because the Bedrock SDK does not expose the same count-tokens API.
- Vertex filters unsupported beta headers for token counting.
- Failure falls back to rough estimates, which can undercount or overcount.

`claude-proxy` design implication:

- Implement token counting per provider where possible.
- If only rough estimates are available, bias toward safety for large tool results and compaction thresholds.

### Max output token cap and escalation

Impact:

- Affects capacity reservation, latency, cost, and recovery from `max_output_tokens` stops.

Observed Claude Code behavior:

- Some first-party experiments cap default output then retry with a larger cap when needed.
- Third-party default is disabled because it was not validated on Bedrock/Vertex.

`claude-proxy` design implication:

- Do not assume Anthropic first-party max-token heuristics apply to every provider.
- Make default max output and escalation behavior provider/model-specific.

### Model capabilities discovery

Impact:

- Helps identify max input tokens and max output tokens for new models.

Observed Claude Code behavior:

- Model capability fetching is limited to Anthropic-internal first-party official base URL paths.

`claude-proxy` design implication:

- Prefer explicit static model metadata plus optional provider-native discovery.

## Lower-priority UX gates

These do not usually affect core model quality, but they change UX or orchestration behavior:

- Auto mode / transcript classifier is first-party-only for external users.
- Verification-agent prompt guidance defaults false for third-party users.
- Away-summary generation defaults false for third-party users.
- Voice mode and Remote Control require Claude.ai OAuth/subscription and are not relevant to most third-party providers.
- First-party analytics/bootstrap/settings-sync/policy endpoints are disabled for third-party providers.

## Recommended `claude-proxy` provider capability shape

Prefer explicit capability contracts over scattered provider-name checks:

```toml
[providers.example.capabilities]
tool_search = true
tool_search_header = "anthropic_1p" # or "anthropic_3p", "extra_body", "none"
fine_grained_tool_streaming = true
prompt_cache = "basic"              # none | basic | global_scope
context_management = true
thinking = true
adaptive_thinking = true
interleaved_thinking = true
effort = true
max_effort = false
structured_outputs = true
strict_tools = true
token_efficient_tools = false
fast_mode = false
token_counting = "native"           # none | rough | native
```

For model-specific differences, provider-level defaults should be overridden by model metadata:

```toml
[providers.example.models."codex-high"]
upstream = "gpt-5-codex-high"
context_window = 272000
max_output_tokens = 64000
supports_tools = true
supports_tool_search = true
supports_reasoning_effort = true
supports_structured_outputs = true
supports_prompt_cache = true
fallback = "codex-medium"
```

## Implementation checklist for a new provider

Before declaring a provider Claude Code compatible, verify:

- [ ] Tool search works with many MCP tools and does not inline all deferrable schemas.
- [ ] Large tool calls stream incrementally or intentionally disable fine-grained streaming with a known cost.
- [ ] Prompt cache behavior is correct and cache keys are session-safe.
- [ ] Context management is either supported or safely omitted.
- [ ] Thinking/adaptive thinking/interleaved thinking behavior is explicitly modeled.
- [ ] Effort/max-effort maps to upstream semantics or is disabled deliberately.
- [ ] Structured outputs and strict tools are independently tested.
- [ ] Token-efficient tool format is either supported or disabled deliberately.
- [ ] Default model is not lower quality than intended.
- [ ] Third-party model-not-found errors suggest a useful fallback.
- [ ] Token counting is native or conservative enough for auto-compact/tool-search thresholds.
- [ ] Max-output defaults and recovery behavior are provider/model-specific.
- [ ] Provider-specific beta/header/body placement is tested for streaming and non-streaming paths.

## Key takeaway

A provider that only implements request/response conversion may be functionally usable but still materially worse than the official source. The highest-risk silent degradations are dynamic tool loading, fine-grained tool streaming, prompt caching, context management, thinking/effort capability detection, and model default selection.
