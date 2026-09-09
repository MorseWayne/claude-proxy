# Reviewed Upstream Baseline

- Codex repository: `https://github.com/openai/codex.git`
- Reviewed commit: `8e6a44b428e31f91b21edc97904fcdf4f0931ade` (`Fix the worktrees experimental feature test fixture (#42682)`)
- Synchronized at: `2026-09-04 15:58:17 +08:00`
- Reviewed for claude-proxy: `v4.0.1`
- Review scope: native Responses request/response passthrough, ChatGPT/Codex request normalization, Responses Lite invariants, model capability declarations, and Structured Outputs verification.

## Known State at This Baseline

- claude-proxy exposes native `/v1/responses` only for OpenAI and ChatGPT provider types.
- The downstream endpoint is streaming-only and stateless.
- OpenAI requests preserve standard Responses fields and unknown future fields.
- ChatGPT normalizes string input, removes its unsupported `max_output_tokens`, and applies Responses Lite invariants.
- Structured Outputs were verified end to end against the ChatGPT backend with schema-valid JSON output.

## Updating This File

After reviewing a newer Codex revision, replace the commit and date and summarize the actual reviewed scope. Record unresolved relevant changes instead of advancing the baseline past them silently.
