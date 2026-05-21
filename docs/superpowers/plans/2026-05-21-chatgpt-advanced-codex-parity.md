# ChatGPT Advanced Codex Parity Evaluation

**Date:** 2026-05-21
**Status:** Accepted: keep the HTTP SSE path stable; defer new lifecycle and transport surfaces.

**Scope:** Turn-state replay, Responses WebSocket transport, FedRAMP/residency routing, and account-specific routing for the ChatGPT/Codex compatibility follow-up workflow.

## Baseline

The proxy currently exposes an Anthropic-compatible `/v1/messages` surface and translates requests to ChatGPT's Codex Responses HTTP SSE path. The stable baseline already sends Codex-style request identity and correlation metadata:

- `originator`
- `User-Agent`
- `session-id`
- `thread-id`
- `x-codex-window-id`
- optional `ChatGPT-Account-Id`
- request body `client_metadata`

## Evidence

- OpenAI Codex keeps HTTP Responses and Responses WebSocket as separate provider capabilities. WebSocket support is feature-gated by provider metadata such as `supports_websockets`, and the local mock uses a distinct `wire_api = "responses_websocket"` path.
- Codex App Server v2 thread replay is modeled as lifecycle RPCs such as `thread/resume`, `thread/rollback`, `thread/fork`, and `thread/read`. Persisted `turns` are populated only for those lifecycle responses, not for ordinary Responses streaming events.
- Residency appears as an explicit config requirement field named `enforceResidency`, rather than as a confirmed ChatGPT HTTP header contract for `/backend-api/codex/responses`.
- Account routing is represented by ChatGPT account identifiers in Codex client/server code. This proxy already forwards the account id extracted from ChatGPT auth as `ChatGPT-Account-Id`.

## Decisions

### Turn-State Replay

Do not implement Codex thread replay in this proxy yet.

The proxy does not currently own a Codex App Server v2 thread lifecycle. It bridges stateless Anthropic-style message requests into ChatGPT Responses SSE. Replaying persisted Codex `turns` would require a new durable thread/session model, lifecycle RPC semantics, and lossiness rules for tool executions. That is larger than request metadata parity and should remain separate until a client actually depends on Codex App Server thread replay through this proxy.

### Responses WebSocket

Do not add WebSocket transport now.

Official Codex support treats WebSocket as a separate provider capability, not as a transparent replacement for HTTP SSE. The current proxy path is HTTP SSE and has coverage for native Codex-shaped request bodies and SSE events. A WebSocket implementation should be gated by explicit configuration and real compatibility tests before it changes production routing.

### FedRAMP / Residency Routing

Do not invent private residency headers.

`enforceResidency` is visible as a higher-level config requirement, but there is no confirmed HTTP header contract in the audited ChatGPT Responses path. If residency routing becomes necessary, it should be added as an explicit, allowlisted configuration surface after the upstream contract is known.

### Account-Specific Routing

Keep the existing automatic account-id forwarding.

The current proxy already sends `ChatGPT-Account-Id` when the ChatGPT auth layer exposes an account id. That is the safest current parity point. Multi-account selection or manual account overrides should be added only when there is a concrete user workflow that needs choosing among multiple valid ChatGPT accounts.

## Follow-Up Triggers

- A real Codex client requires App Server v2 lifecycle methods through this proxy.
- OpenAI Codex makes `responses_websocket` mandatory or materially different from HTTP SSE for the target ChatGPT backend.
- The ChatGPT backend documents or returns a concrete residency/FedRAMP routing contract.
- Users need explicit account selection across multiple ChatGPT accounts.
