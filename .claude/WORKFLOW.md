# Workflow Ledger

A lightweight milestone ledger for Claude Code development work.

## Active

### WF-2026-05-17-001 — ChatGPT tool parameter sanitization

Status: In Progress
Level: 2
Current phase: Phase 1 — Implement and validate
Started: 2026-05-17
Updated: 2026-05-17
Goal: Prevent ChatGPT provider tool calls from leaking invalid empty optional parameters such as `Read.pages: ""`, strengthen default instructions, and validate with provider tests.
Decisions:
- Use deterministic sanitizer for empty optional-like tool parameters before forwarding schemas/body where feasible.
- Strengthen ChatGPT fallback instructions; keep existing user/system instructions authoritative.

#### Phase 1 — Implement and validate
Status: In Progress
Depends on:
- GitNexus LOW impact analysis for `build_chatgpt_responses_body` and `convert_tool`.
Tasks:
- [x] Run GitNexus impact analysis before editing.
- [x] Add ChatGPT default tool-use instructions.
- [x] Sanitize empty string optional tool parameters, prioritizing `pages`.
- [x] Add regression tests.
- [x] Run formatting, provider tests, and full validation as practical.

Acceptance / Review:
- Review: Strengthened ChatGPT fallback instructions and added streaming/non-streaming Responses tool argument sanitizer that recursively removes empty string fields before emitting Anthropic tool inputs.
- Validation: `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test -p claude-proxy-providers`, and `cargo test` passed.
- GitNexus: Pre-edit impacts were LOW for `build_chatgpt_responses_body`, `convert_tool`, and `convert_function_call`; final `detect_changes` reported LOW risk with no affected execution flows.
- Tests: Added regression coverage for stream and non-stream Responses tool calls with empty `pages`/nested empty fields.
- Gaps: None.

Resume next: Commit the validated changes and update the GitNexus index.


## Backlog / Future

- [ ] Consider whether OpenAI/Copilot Responses paths need provider-specific handling if their upstreams start requiring `instructions`.

## Completed

### WF-2026-05-16-001 — ChatGPT responses default instructions

Status: Done
Completed: 2026-05-16
Level: 2
Commits:

- 83be0f9 Fix ChatGPT responses instructions fallback

Acceptance summary:

- Review: Added `build_chatgpt_responses_body` so only ChatGPT Responses requests get fallback instructions and existing system instructions are preserved.
- Validation: `cargo fmt`, `cargo clippy -- -D warnings`, `cargo test -p claude-proxy-providers`, and `cargo test` passed.
- GitNexus: Initial `impact` on `ChatGptProvider.chat` returned LOW risk; final `detect_changes` reported HIGH due to touched `chatgpt.rs` and related test flows, reviewed as expected. `npx gitnexus analyze` updated the index to 1,584 nodes / 3,814 edges / 139 flows.
- Tests: Added coverage for missing ChatGPT instructions, preserving existing system instructions, and fast-intent body generation.
- Gaps: None.
