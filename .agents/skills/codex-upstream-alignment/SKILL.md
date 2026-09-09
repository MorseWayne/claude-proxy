---
name: codex-upstream-alignment
description: Force-sync the local openai/codex reference checkout from its authoritative remote, analyze upstream changes, and determine or implement what claude-proxy must follow across endpoints, wire protocols, models, tools, auth, and streaming behavior. Use when updating `/home/wayne/source/open/codex`, comparing Codex revisions, or planning claude-proxy alignment from a Codex delta; do not use for generic API compatibility work without an upstream Codex change.
---

# Codex Upstream Alignment

Synchronize openai/codex with the remote as the source of truth, reduce its changes to the subset relevant to claude-proxy, and produce an evidence-backed follow-up plan or implementation.

## Repositories and Baseline

- Upstream checkout: `/home/wayne/source/open/codex`.
- Target repository: `/home/wayne/source/open/claude-proxy`.
- Read [references/upstream-baseline.md](references/upstream-baseline.md) before computing the upstream range.
- If the requested paths differ or do not exist, discover the actual checkouts before proceeding.

The baseline means “last Codex revision whose relevant behavior was reviewed,” not merely “last fetched revision.” Update it only after the analysis is complete and any requested alignment has been verified.

## 1. Force-Synchronize Codex from the Remote

Treat the Codex checkout as an upstream reference copy. A request to synchronize it authorizes replacing local tracked changes and local-only commits on the synchronized branch with the selected remote revision, including changes to `AGENTS.md`. Do not stop or ask again because paths overlap, the checkout is dirty, or the branch has diverged. Do not stash or merge local changes back afterward.

When the user asks to update or sync Codex, or invokes this skill to start the alignment workflow:

1. Read the upstream repository's `AGENTS.md` and relevant nested instructions.
2. Record the current branch, `HEAD`, tracking branch, remotes, and `git status --short`.
3. Use the explicitly requested remote branch, otherwise the configured tracking branch (default `origin/main`). Fetch that remote and resolve the fetched branch to an exact commit. If fetching fails or the target cannot be resolved, stop synchronization rather than resetting to a stale or guessed ref.
4. Confirm the command targets the Codex checkout, then run `git reset --hard <fetched_sha>` to replace the index, tracked files, and current branch tip with that revision.
5. Remove untracked files and directories in this reference checkout with `git clean -fd`. Leave ignored build caches alone; removing ignored files requires an explicit request.
6. Verify `HEAD` equals the fetched commit and `git status --short` is clean. Read any changed upstream instructions before continuing the review.

This replacement policy applies only to the identified Codex reference checkout. Preserve local work in claude-proxy and other repositories. Analysis-only requests that explicitly exclude synchronization must not reset or clean the checkout. Tool or filesystem permission requirements still apply; this policy removes the skill's extra confirmation gate.

Record `before_sha`, `after_sha`, the fetched target, and the local changes replaced. Keep the synchronization revision separate from the reviewed baseline and record the exact analyzed range. Do not treat a successful fetch as a completed working-tree update.

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

Syncing Codex authorizes the local replacement described in section 1, not authoring upstream changes or pushing to its remote. Analysis does not authorize editing claude-proxy runtime code unless the user asks for implementation; maintain the review report and baseline as part of the alignment workflow.

Do not commit, push, tag, publish, restart services, or monitor CI unless requested. When release work is requested, preserve unrelated files, stage only the scoped changes, verify commit and tag identity after pushing, and stop without checking CI when the user says not to track it.
