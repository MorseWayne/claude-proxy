# Workflow Ledger

A lightweight milestone ledger for Claude Code development work.

## Active

<!-- workflow-ledger:task
id: WF-2026-05-16-001
level: 2
status: In Progress
current_phase: Phase 2 — Implement and validate
updated: 2026-05-16
-->

### WF-2026-05-16-001 — ChatGPT responses default instructions
Status: In Progress
Level: 2
Started: 2026-05-16
Last updated: 2026-05-16
Current phase: Phase 2 — Implement and validate

Goal:
- Prevent Claude Desktop requests routed to `chatgpt/gpt-5.5` from failing upstream with `Instructions are required` when the Anthropic request has no system prompt.

Decisions:
- 2026-05-16 — Only patch the `ChatGptProvider` Responses path with a non-empty fallback instruction to avoid changing shared OpenAI/Copilot Responses conversion semantics.
- 2026-05-16 — Use `Follow the user's instructions.` as the minimal fallback because it satisfies the upstream requirement without adding an assistant persona.

Phases:

#### Phase 1 — Design and impact analysis
Status: Done
Depends on:
- None
Tasks:
- [x] Confirm the failing request path and missing `instructions` condition.
- [x] Choose a scoped provider-specific fix.
- [x] Run GitNexus impact analysis for `ChatGptProvider.chat`.

Acceptance / Review:
- Review: Confirmed `ChatGptProvider.chat` builds a Responses body from `convert_to_responses` before sending upstream.
- Validation: User approved provider-specific fallback design.
- GitNexus: `impact` on `ChatGptProvider.chat` returned LOW risk with 0 direct upstream callers/processes affected.
- Tests: N/A; design phase only.
- Gaps: None.

#### Phase 2 — Implement and validate
Status: In Progress
Depends on:
- Phase 1
Tasks:
- [x] Add ChatGPT-only fallback `instructions` handling.
- [x] Add tests for missing and existing instructions.
- [x] Run provider tests and required validation.
- [ ] Commit and update GitNexus index.

Acceptance / Review:
- Review: Implemented `build_chatgpt_responses_body` so only ChatGPT Responses requests get fallback instructions and existing system instructions are preserved.
- Validation: `cargo fmt`, `cargo clippy -- -D warnings`, and `cargo test` passed.
- GitNexus: `detect_changes` returned HIGH due to touched `chatgpt.rs` and related test processes; reviewed as expected for provider request-body construction and tests.
- Tests: `cargo test -p claude-proxy-providers`; `cargo test`.
- Gaps: Commit and GitNexus index update still pending.

Resume next:
- Commit the two modified files, run `npx gitnexus analyze`, then report commit hash.

## Backlog / Future

- [ ] Consider whether OpenAI/Copilot Responses paths need provider-specific handling if their upstreams start requiring `instructions`.

## Completed

### WF-YYYY-MM-DD-000 — Completed task title
Status: Done
Completed: YYYY-MM-DD
Level: 1
Commits:
- abc1234 commit subject

Acceptance summary:
- Review: Summary of review performed.
- Validation: Summary of validation performed.
- GitNexus: Summary or N/A.
- Tests: Summary or N/A.
- Gaps: Remaining follow-up or none.
