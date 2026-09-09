---
name: codex-upstream-alignment
description: Sync the local openai/codex source checkout, analyze upstream changes, and determine or implement what claude-proxy must follow across endpoints, wire protocols, models, tools, auth, and streaming behavior. Use when updating `/home/wayne/source/open/codex`, comparing Codex revisions, or planning claude-proxy alignment from a Codex delta; do not use for generic API compatibility work without an upstream Codex change.
---

# Codex Upstream Alignment

Synchronize openai/codex safely, reduce its changes to the subset relevant to claude-proxy, and produce an evidence-backed follow-up plan or implementation.

## Repositories and Baseline

- Upstream checkout: `/home/wayne/source/open/codex`.
- Target repository: `/home/wayne/source/open/claude-proxy`.
- Read [references/upstream-baseline.md](references/upstream-baseline.md) before computing the upstream range.
- If the requested paths differ or do not exist, discover the actual checkouts before proceeding.

The baseline means “last Codex revision whose relevant behavior was reviewed,” not merely “last fetched revision.” Update it only after the analysis is complete and any requested alignment has been verified.

## 1. Synchronize Codex Without Losing Local Work

When the user asks to update or sync Codex:

1. Read the upstream repository's `AGENTS.md` and relevant nested instructions.
2. Record the current branch, `HEAD`, tracking branch, remotes, and `git status --short`.
3. Fetch the requested remote, then determine whether the working branch can fast-forward.
4. Use a fast-forward-only update when it preserves existing local changes.

Never reset, discard, overwrite, or silently stash upstream-checkout changes. If the checkout is dirty, compare locally modified paths with paths changed by the incoming range. If they overlap, or the branch has diverged, stop the update and explain the conflict. Analysis can continue against the fetched remote ref or an isolated temporary worktree when that stays within the user's request.

Record `before_sha`, `after_sha`, and the exact analyzed range. Do not treat a successful fetch as a completed working-tree update.

## 2. Analyze the Upstream Delta

Start from the recorded baseline, not from an arbitrary release date. Use commit history and file-level diff statistics to narrow the range before reading individual implementations.

Prioritize changes that can alter claude-proxy's observable contract:

- Responses, compact, models, Realtime/WebSocket, and other endpoint request builders.
- Request fields, headers, query parameters, content encoding, authentication, and routing metadata.
- Input/output item variants, tools, Structured Outputs, compaction, continuation, and state handling.
- SSE/WebSocket lifecycle events, response headers, usage accounting, terminal states, and error mapping.
- Model catalog fields, feature flags, context limits, reasoning levels, service tiers, and capability gates.
- Retry, timeout, fallback, caching, and connection reuse semantics.

Relevant upstream entry points commonly include `codex-rs/codex-api`, `codex-rs/core`, `codex-rs/protocol`, `codex-rs/models-manager`, and transport/client crates. Search the actual changed tree rather than assuming these paths remain stable.

Do not propose proxy work for UI-only, sandbox-only, local persistence, or app-server changes unless they modify the wire behavior claude-proxy consumes or exposes.

## 3. Map Each Change to claude-proxy

Use CodeGraph first when available for unfamiliar request and response flows. Trace from the downstream handler through model resolution and provider selection to the upstream request, then back through event decoding and downstream encoding.

Classify each relevant upstream change as:

- **Already aligned**: show the proxy symbols or tests that cover it.
- **Gap**: identify the exact request, response, capability, or operational mismatch.
- **Not applicable**: explain why Codex behavior does not belong in this proxy.
- **Needs live verification**: static code does not establish a private backend contract.

Keep three contracts distinct: public OpenAI behavior, claude-proxy's intentionally exposed subset, and provider-specific behavior such as the ChatGPT/Codex backend. A model appearing in `/v1/models` proves visibility, not full endpoint compatibility.

Present the result as a compact matrix containing upstream commit/file evidence, wire-level change, current proxy behavior, status, recommended action, compatibility impact, and verification needed. Mark inference separately from observed behavior.

## 4. Design and Implement the Follow-up

When implementation is requested:

- Put proxy-wide validation in the downstream handler and provider-only adaptation in the provider adapter.
- Preserve unknown native fields and events unless there is a concrete safety or contract reason to reject them.
- Keep capability declarations synchronized with effective downstream behavior; do not advertise an internal provider path as a native public endpoint.
- Prefer the smallest coherent change that covers request shaping, response/event handling, capabilities, tests, and documentation together.
- If the user explicitly allows breaking changes, prefer one clear endpoint contract over layered legacy fallbacks. Otherwise state compatibility risks before changing public behavior.

If the delta touches Responses, Structured Outputs, ChatGPT request normalization, or `/v1/models` capability declarations, read [references/responses-v4.0.1.md](references/responses-v4.0.1.md) before editing. Treat it as prior evidence to revalidate, not a permanent upstream specification.

## 5. Verify Proportionally

Test the smallest changed layer first, then the full affected flow. For claude-proxy release-quality verification, normally run:

```bash
cargo fmt --all -- --check
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
git diff --check
```

Use a real end-to-end provider check only when requested or when private upstream behavior cannot be established statically. Obtain authorization for network calls, use an isolated temporary configuration and port, never print credentials, and clean up copied credentials and response artifacts.

Update `references/upstream-baseline.md` with the reviewed Codex commit, date, proxy version or commit, reviewed scope, and remaining known gaps after verification succeeds. Do not advance the baseline over unreviewed relevant changes.

## Mutation and Release Boundaries

Syncing Codex does not authorize modifying or pushing the Codex repository. Analysis does not authorize editing claude-proxy unless the user asks for implementation.

Do not commit, push, tag, publish, restart services, or monitor CI unless requested. When release work is requested, preserve unrelated files, stage only the scoped changes, verify commit and tag identity after pushing, and stop without checking CI when the user says not to track it.
