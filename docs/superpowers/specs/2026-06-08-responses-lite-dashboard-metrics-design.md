# Responses Lite dashboard metrics follow-up

Date: 2026-06-08

## Goal

Make the Responses Lite and continuation savings data added in the previous slice visible in admin metrics and the TUI dashboard, without changing request routing, provider behavior, or tool conversion.

## Approved scope

Implement a low-risk observability display slice:

1. Extend request observability summary aggregation with low-cardinality counters for Responses Lite, WebSocket transport, and continuation usage.
2. Expose the new counters through `/admin/metrics` alongside existing observability summary fields.
3. Parse the new fields in the TUI metrics contract.
4. Render a compact dashboard status line/card showing continuation saved bytes and key counts.

## Non-goals

- Do not implement Responses Lite standalone tool conversion.
- Do not change ChatGPT/Codex request payload behavior.
- Do not alter context-limit preflight, compaction, or tool-result truncation policy.
- Do not add a new TUI page or trend chart.
- Do not remove or rename existing `/admin/metrics` JSON keys.

## Design

### 1. Summary aggregation

`RequestObservabilitySummary` already aggregates latency, idle gaps, prompt-too-long retries, and continuation saved bytes. Add these counters:

- `responses_lite_requests`: number of observability events where `responses_lite == Some(true)`.
- `websocket_requests`: number of observability events where `transport == "websocket"`.
- `continuation_used_requests`: number of observability events where `continuation_used == Some(true)`.

These fields are additive and default to `0`, so old in-memory values and old persisted rows remain compatible.

For stored metrics, update the SQLite summary query to compute the same counters from `request_observability_events`:

```sql
SUM(CASE WHEN responses_lite = 1 THEN 1 ELSE 0 END)
SUM(CASE WHEN transport = 'websocket' THEN 1 ELSE 0 END)
SUM(CASE WHEN continuation_used = 1 THEN 1 ELSE 0 END)
```

The query must continue working after the previous migration has added `responses_lite` and `continuation_saved_bytes` columns. Legacy databases are already migrated during metrics store initialization.

### 2. Admin metrics contract

`/admin/metrics` should continue returning the existing shape:

```json
{
  "observability": {
    "summary": { ... },
    "recent": [ ... ],
    "stored": { "summary": { ... }, "recent": [ ... ] }
  }
}
```

Only add new fields under existing summary objects. Do not introduce a new top-level section.

Expected new fields:

```json
{
  "continuation_saved_bytes": 123456,
  "responses_lite_requests": 42,
  "websocket_requests": 40,
  "continuation_used_requests": 31
}
```

### 3. TUI parsing

Update the TUI metrics data model and parser to read the new observability summary fields. Missing fields should parse as zero to remain compatible with older running servers.

If the TUI currently parses only a subset of `observability.summary`, keep this change scoped to the existing metrics parsing layer instead of introducing a separate API client.

### 4. Dashboard rendering

Add one compact observability row/card to the existing Dashboard. It should fit the current dashboard layout and avoid a new page.

Suggested labels:

- `Resp Lite`: count of Responses Lite requests.
- `WS`: count of WebSocket requests.
- `Cont`: count of continuation-used requests.
- `Saved`: human-readable `continuation_saved_bytes`.

Use existing formatting helpers if available. Otherwise add a small helper for binary byte units:

- `<1024`: `N B`
- `<1024^2`: `N.N KiB`
- otherwise: `N.N MiB`

The dashboard should prefer stored totals when the existing dashboard already displays stored/all-time figures, and should keep session values visible where that is the current convention. If only one compact value can be shown, show stored total first because continuation savings is cumulative.

### 5. Testing strategy

Add focused tests:

- Server summary aggregation counts Responses Lite, WebSocket, continuation-used, and saved bytes.
- Persistence stored summary loads the same counters from SQLite rows.
- TUI metrics parser defaults missing fields to zero.
- TUI metrics parser reads the new fields when present.
- Dashboard rendering includes the saved-bytes text or metrics row in a stable unit-testable way.

## Risks

- TUI layout can become crowded. Keep the display to one compact row/card and use short labels.
- Stored and session totals can be confusing if shown side by side. Follow the dashboard's existing convention and label totals clearly.
- Admin API consumers may rely on existing JSON. Add fields only; do not rename or move existing keys.

## Future work

- Add miss-reason aggregation for `continuation_disabled_reason` if dashboard diagnosis needs more detail.
- Add Responses Lite standalone tool conversion after the transport and observability layers are stable.
- Add trend windows only if users need time-based diagnostics beyond all-time/session totals.
