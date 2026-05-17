# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build / Test / Lint

```bash
cargo build --release          # Release binary at target/release/claude-proxy
cargo run -- <subcommand>      # Run the CLI
cargo test                     # All tests
cargo test -p claude-proxy-server  # Single crate
cargo clippy -- -D warnings   # Lint (CI gate)
cargo fmt --check              # Format check
```

## Architecture

Rust workspace (edition 2024) — 5 crates:

- **`claude-proxy-core`** — Canonical data model. The Anthropic Messages API types (`MessagesRequest`, `SseEvent`, `Content`, `ErrorResponse`, `Role`, etc.) serve as the interchange format. All providers normalize into these types.

- **`claude-proxy-config`** — TOML config at `~/.config/claude-proxy/config.toml`. `Settings` struct with providers map, model aliases, server/limit/http/log sections. Also handles `.env` → TOML migration.

- **`claude-proxy-providers`** — `Provider` trait (`chat()` returns `BoxStream<SseEvent>`, `list_models()`) + 3 implementations:
  - `AnthropicProvider` — passthrough with auth header rewrite
  - `OpenAiProvider` — `StreamConverter` state machine converts OpenAI Chat Completion ↔ Anthropic SSE. **Note:** usage data arrives in the last streaming chunk, but `message_start` already fired with `input_tokens: 0` — this causes bug where streaming OpenAI input_tokens are never captured.
  - `CopilotProvider` — wraps OpenAI provider with GitHub OAuth, VS Code header emulation, and request preprocessing (warmup → small model, compact detection, tool_result merging, subagent marking)

- **`claude-proxy-server`** — Axum HTTP server. Routes: `/v1/messages` (proxy), `/v1/models`, `/health`, `/admin/metrics|config|restart`. Key internals:
  - `Middleware` — keyed token-bucket rate limiter (governor crate)
  - `Metrics` — atomic counters for requests/errors/latency, per-model `HashMap` for token usage. `record_completed_request()` persists to SQLite.
  - `MetricsStore` — SQLite at `~/.config/claude-proxy/metrics.db`, schema `usage_events` with per-request token counts, errors, latency.
  - Background tasks: config file watcher (debounced), SIGUSR1 reload, model cache warmup at startup.
  - `extract_usage_from_event()` parses token counts from SSE events — relies on `message_start` for input_tokens/cache and `message_delta` for output_tokens.

- **`claude-proxy-cli`** — Single binary `claude-proxy`. Combines all other crates.
  - CLI via `clap`: `provider add|edit|delete|test...`, `config show|edit|validate...`, `server start|stop|restart|status`, `completions`, `tui`
  - TUI via `ratatui`: 8 pages (Dashboard, Providers, Server, Limits, HTTP, Logging, Model, System). Dashboard polls `/admin/metrics` every 5s and merges session + stored totals.

## Data flow (request proxying)

1. `POST /v1/messages` → `routes::messages()`
2. Auth check (x-api-key / Bearer), concurrency semaphore acquire
3. Resolve `model` → provider_id + upstream_model_name via `Settings::resolve_model()`
4. Get or create provider from `ProviderRegistry`
5. `provider.chat(request)` → stream of `SseEvent` (all providers normalize to this)
6. Stream back as SSE to client, extracting token usage from events
7. On completion: `record_completed_request()` → in-memory `Metrics` + SQLite persistence

## Model routing

`Settings::resolve_model(model)` looks up model aliases in `[model]` section (opus/sonnet/haiku), then parses `provider_id/upstream_model` format. Falls back to default provider.

## Known issues (stats accuracy)

- `total_tokens()` double-counts cache tokens (cache is a subset of input_tokens in Anthropic's API)
- OpenAI streaming never captures `input_tokens` (usage arrives after `message_start` is emitted)
- Anthropic non-streaming never captures `cache_*_input_tokens` (extraction path mismatch)

## Project Workflow

- **MUST use GitNexus first for code exploration.** For any code exploration or architecture question, first use GitNexus to analyze the architecture, current execution flows, and potential modification impact before falling back to grep or manual file reads.
- **MUST provide a structured summary after code changes.** After completing user-requested code modifications, summarize changed files, validation performed, GitNexus findings, commit hash, and any follow-up notes.
- **MUST commit completed code changes.** After completing user-requested code modifications and verification, create a git commit for the change unless the user explicitly says not to commit.
- **MUST update the GitNexus index after committing.** After committing completed code changes, run `npx gitnexus analyze` so the code intelligence index reflects the latest repository state.

<!-- gitnexus:start -->
# GitNexus — Code Intelligence

This project is indexed by GitNexus as **claude-proxy** (1713 symbols, 4167 relationships, 150 execution flows). Use the GitNexus MCP tools to understand code, assess impact, and navigate safely.

> If any GitNexus tool warns the index is stale, run `npx gitnexus analyze` in terminal first.

## Always Do

- **MUST run impact analysis before editing any symbol.** Before modifying a function, class, or method, run `gitnexus_impact({target: "symbolName", direction: "upstream"})` and report the blast radius (direct callers, affected processes, risk level) to the user.
- **MUST run `gitnexus_detect_changes()` before committing** to verify your changes only affect expected symbols and execution flows.
- **MUST warn the user** if impact analysis returns HIGH or CRITICAL risk before proceeding with edits.
- When exploring unfamiliar code, use `gitnexus_query({query: "concept"})` to find execution flows instead of grepping. It returns process-grouped results ranked by relevance.
- When you need full context on a specific symbol — callers, callees, which execution flows it participates in — use `gitnexus_context({name: "symbolName"})`.

## Never Do

- NEVER edit a function, class, or method without first running `gitnexus_impact` on it.
- NEVER ignore HIGH or CRITICAL risk warnings from impact analysis.
- NEVER rename symbols with find-and-replace — use `gitnexus_rename` which understands the call graph.
- NEVER commit changes without running `gitnexus_detect_changes()` to check affected scope.

## Resources

| Resource | Use for |
|----------|---------|
| `gitnexus://repo/claude-proxy/context` | Codebase overview, check index freshness |
| `gitnexus://repo/claude-proxy/clusters` | All functional areas |
| `gitnexus://repo/claude-proxy/processes` | All execution flows |
| `gitnexus://repo/claude-proxy/process/{name}` | Step-by-step execution trace |

## CLI

| Task | Read this skill file |
|------|---------------------|
| Understand architecture / "How does X work?" | `.claude/skills/gitnexus/gitnexus-exploring/SKILL.md` |
| Blast radius / "What breaks if I change X?" | `.claude/skills/gitnexus/gitnexus-impact-analysis/SKILL.md` |
| Trace bugs / "Why is X failing?" | `.claude/skills/gitnexus/gitnexus-debugging/SKILL.md` |
| Rename / extract / split / refactor | `.claude/skills/gitnexus/gitnexus-refactoring/SKILL.md` |
| Tools, resources, schema reference | `.claude/skills/gitnexus/gitnexus-guide/SKILL.md` |
| Index, status, clean, wiki CLI commands | `.claude/skills/gitnexus/gitnexus-cli/SKILL.md` |

<!-- gitnexus:end -->


## Workflow Ledger

Use `workflow-ledger` for recoverable development work.

- Classify tasks before executing: Level 0 Q&A, Level 1 lightweight edit, Level 2 standard code work, Level 3 complex work.
- Maintain `.claude/WORKFLOW.md` for Level 2/3 tasks and for any task the user wants tracked across sessions.
- Organize tracked work by phases and subtasks, not a flat checklist.
- Before marking a phase Done, record `Acceptance / Review` with `Review`, `Validation`, `GitNexus`, `Tests`, and `Gaps`; failed validation means the phase stays In Progress or Blocked.
- Record dependencies and discovered future tasks; complete prerequisites before blocked work, and defer non-blocking discoveries to Backlog/Future.
- Use TodoWrite for current-session execution; use `.claude/WORKFLOW.md` for milestone history and resume points.
- Do not create attachments or extra spec files unless Level 3 work genuinely needs them or the user asks.

Do not rationalize skipping the ledger:

- “This is small” still requires Level classification; Level 2/3 work is tracked.
- “I will update it later” is unsafe; update at phase completion, blockers, key decisions, and handoff points.
- TodoWrite is session-local; `.claude/WORKFLOW.md` is the durable recovery state.
- Keep core fields stable so `.claude/bin/workflow-ledger doctor` can check the ledger.
