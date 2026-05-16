# Workflow Ledger

A lightweight milestone ledger for Claude Code development work.

## Active

None.

## Backlog / Future

- [ ] Consider whether OpenAI/Copilot Responses paths need provider-specific handling if their upstreams start requiring `instructions`.

## Completed

### WF-2026-05-17-001 — ChatGPT Read pages argument sanitization

Status: Done
Completed: 2026-05-17
Level: 2
Commits:

- Pending

Acceptance summary:

- Review: Added a conservative Responses argument sanitizer that only removes top-level `pages: ""` for `Read` tool calls when the argument JSON is complete and parseable; non-Read tools and other empty strings are preserved.
- Validation: `cargo fmt --check`, `cargo test -p claude-proxy-providers`, `cargo clippy -- -D warnings`, and `cargo test` passed.
- GitNexus: Impact checks for `handle_function_call_arguments_done`, `handle_output_item_done`, and `convert_function_call` were LOW risk; `detect_changes` reported LOW risk with no affected processes.
- Tests: Added regression coverage for `Read.pages: ""` removal, Bash command preservation, non-streaming sanitization, and non-Read empty string preservation.
- Gaps: None.

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
