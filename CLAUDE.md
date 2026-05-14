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
