# Reviewed Upstream Baseline

- Codex repository: `https://github.com/openai/codex.git`
- Reviewed commit: `0d46c252b3f29f10bacf0ef58a17a1aa5d17ead3` (`Encapsulate executed tool call metadata recording (#44002)`)
- Reviewed at: `2026-09-09` (`Asia/Shanghai`)
- Previous reviewed commit: `8e6a44b428e31f91b21edc97904fcdf4f0931ade`
- Analyzed range: `8e6a44b428e31f91b21edc97904fcdf4f0931ade..0d46c252b3f29f10bacf0ef58a17a1aa5d17ead3` (231 commits)
- Reviewed for claude-proxy: `v4.0.2`; review started from commit `ce6ddef5f9a5b89daaf21ad53f4ff514dd2d92a9`
- Review scope: reasoning configuration items and compaction pins, response IDs and Guardian metadata, turn identity, SSE cancellation, Astra model capabilities, model-cache identity, ChatGPT routing cookies, and exclusion of client-only endpoint/runtime changes.
- Review report: [2026-09-09 Codex upstream alignment](../../../../docs/reviews/2026-09-09-codex-upstream-alignment.md).

## Checkout Synchronization

- Fetch completed: `origin/main` is `0d46c252b3f29f10bacf0ef58a17a1aa5d17ead3`.
- Local checkout `before_sha`: `8e6a44b428e31f91b21edc97904fcdf4f0931ade`.
- Local checkout `after_sha`: `0d46c252b3f29f10bacf0ef58a17a1aa5d17ead3`.
- Last completed synchronization: `2026-09-09` (`Asia/Shanghai`).
- Force-synchronized from the authoritative remote with `git reset --hard` and `git clean -fd`, as explicitly requested by the user. The local trailing blank line in `AGENTS.md` was replaced; there were no untracked files to remove. `HEAD` matches the fetched target and the working tree is clean.
- The reviewed revision and synchronized checkout now match. The proxy changes below are included in the `v4.0.2` release.

## Known State at This Baseline

- claude-proxy exposes native `/v1/responses` only for OpenAI and ChatGPT provider types.
- The downstream endpoint is streaming-only and stateless.
- OpenAI requests preserve standard Responses fields and unknown future fields.
- ChatGPT normalizes string input, removes its unsupported `max_output_tokens`, and applies Responses Lite invariants.
- Structured Outputs were verified end to end against the ChatGPT backend with schema-valid JSON output.

The Structured Outputs live verification above is historical v4.0.1 evidence. The 2026-09-09 review ran deterministic tests and local probes, without real provider calls.

## Implemented Alignment (2026-09-09)

- SSE cancellation now closes idle upstream reads when the last consumer exits. Followers retain the shared stream after the leader disconnects; idle follower disconnection releases its subscription and permits.
- OpenAI Astra metadata now declares Responses, reasoning levels, text/image input, a 1,050,000-token context and 128,000-token output limit. Messages routing uses Responses, supports verbosity, and normalizes reasoning aliases. Unsupported sampling/stop controls are no longer advertised for Astra.
- ChatGPT Astra fallback now uses the Codex catalog's 272,000-token default context, Lite mode, priority tier, and reasoning/delegation capabilities. It does not copy the public API context limit or activate the optional 872,000-token expanded window.
- ChatGPT HTTP clients now retain only `__oailb`, per client, on known HTTPS ChatGPT hosts. Cookie scope and expiration are enforced by reqwest's jar; downstream credentials remain filtered.
- Model catalogs are bound to provider instance and authentication identity. ChatGPT identity incorporates available account/user/email/plan claims and configured routing headers, while preserving reuse across known-owner token rotation. Opaque credentials are hashed conservatively. Cached views reject changed identities and refresh results from replaced providers or identities.
- Persistent context capability caches use schema version 2 and the same identity scope plus provider/base/model. Legacy entries become cache misses; URL normalization preserves path case. Token storage format is unchanged.

## Remaining Verification Boundaries

- Real-provider Astra operation, routing-cookie performance benefits, and updated private backend constraints were not tested live. Deterministic fixtures establish request shaping and isolation, not account access or backend entitlement.
- Detached memory identity: native turn/root metadata is retained, but ChatGPT fills missing session/thread headers. Backend expectations for dedicated memory requests remain unverified.

Preserve these items in future reviews even when advancing beyond this revision. Guardian execution, voice/WebRTC, image generation endpoints, and app-server user verification remain outside the proxy's exposed scope.

## Verification for This Review

- `cargo fmt --all -- --check` and `git diff --check` passed.
- `cargo test --locked --workspace --all-targets -- --quiet`: 602 passed (43 CLI, 30 config, 11 core, 389 provider, 110 server unit, 19 server integration tests).
- `cargo clippy --locked --workspace --all-targets -- -D warnings` passed.
- Regression coverage includes idle TCP closure, leader/follower cancellation, Astra catalog and Messages routing, effort aliases, cookie scope/expiration, token/plan/routing identity, legacy cache rejection, and refresh races against identity changes and provider replacement.
- Full tests required execution with local socket permission after the sandbox denied loopback listeners. All provider verification used local mocks and synthetic credentials.
- Prepared for the `v4.0.2` release. No service restarts or live provider tests were performed.

## Updating This File

After reviewing a newer Codex revision, replace the commit and date and summarize the actual reviewed scope. Record unresolved relevant changes instead of advancing the baseline past them silently.
