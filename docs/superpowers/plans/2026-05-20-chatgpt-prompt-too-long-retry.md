# ChatGPT Prompt Too Long Retry Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make ChatGPT provider compact requests recover from upstream `Prompt is too long` errors by dropping oldest retry-safe context and retrying.

**Architecture:** Keep generic Responses conversion unchanged. Add ChatGPT-specific prompt-too-long detection and request-body shrinking around `ChatGptProvider::chat`, modeled after Claude Code and Codex: normal request first, reactive bounded retry only after PTL errors.

**Tech Stack:** Rust, `serde_json::Value`, existing `reqwest` provider path, existing `cargo test -p claude-proxy-providers chatgpt::tests::...` tests.

---

### Task 1: Pure Retry Helpers

**Files:**
- Modify: `crates/claude-proxy-providers/src/chatgpt.rs`

- [ ] **Step 1: Write failing tests**

Add unit tests for PTL detection, token-gap parsing, oldest-group deletion, tool call/output pairing, assistant-first marker insertion, and oversized single text fallback.

- [ ] **Step 2: Run tests to verify failure**

Run: `cargo test -p claude-proxy-providers chatgpt::tests::prompt_too_long -- --nocapture`
Expected: FAIL because helpers do not exist yet.

- [ ] **Step 3: Implement helpers**

Add private helpers in `chatgpt.rs` for:
- `is_prompt_too_long_error`
- `prompt_too_long_token_gap`
- `shrink_prompt_too_long_body`
- `retry_groups_for_responses_input`
- head/tail text fallback truncation.

- [ ] **Step 4: Run helper tests**

Run: `cargo test -p claude-proxy-providers chatgpt::tests::prompt_too_long -- --nocapture`
Expected: PASS.

### Task 2: Provider Retry Loop

**Files:**
- Modify: `crates/claude-proxy-providers/src/chatgpt.rs`

- [ ] **Step 1: Write failing provider test**

Add a local HTTP server test where the first ChatGPT `/responses` call returns a PTL error and the second returns a successful stream. Assert two requests were sent and the second body has fewer oldest input items.

- [ ] **Step 2: Run test to verify failure**

Run: `cargo test -p claude-proxy-providers chatgpt::tests::chatgpt_retries_prompt_too_long_with_shrunk_body -- --nocapture`
Expected: FAIL because `chat` does not retry PTL yet.

- [ ] **Step 3: Implement retry loop**

Move non-success response handling in `ChatGptProvider::chat` behind a bounded loop. Keep auth refresh behavior unchanged. On PTL, shrink the JSON body and retry up to three times; otherwise map upstream response exactly as before.

- [ ] **Step 4: Run targeted tests**

Run:
- `cargo test -p claude-proxy-providers chatgpt::tests::prompt_too_long -- --nocapture`
- `cargo test -p claude-proxy-providers chatgpt::tests::chatgpt_retries_prompt_too_long_with_shrunk_body -- --nocapture`

Expected: PASS.

### Task 3: Verification

- [ ] Run `cargo fmt --check`.
- [ ] Run `cargo test -p claude-proxy-providers chatgpt::tests:: -- --nocapture`.
- [ ] Run `cargo test -p claude-proxy-providers`.
- [ ] Run `gitnexus_detect_changes(scope=all)` and confirm affected flows match ChatGPT provider retry behavior.
