# ADR-001: SSE contract - backend camelCase plus a shared JSON fixture

**Status**: Accepted (planning, 2026-10-09)
**Deciders**: Phase 3 planning agent; confirm at plan approval

## Context
The Angular UI reads camelCase (`requestId`, `durationMs`, ... in `ui/src/app/core/models/telemetry.models.ts`) while the SSE structs in `src/entrypoint/events.rs:17-55` have no `rename_all` and emit snake_case. `research/architecture.md` recommends backend `#[serde(rename_all = "camelCase")]`; `research/pitfalls.md` recommends a UI-side adapter to avoid any wire change. Both agree the contract must be pinned by a fixture test.

## Evidence (consumers grepped by the planner)
`grep -rn "dashboard/events"` (excluding node_modules/target/dist/project_plans) finds exactly five hits:
- `ui/src/app/core/services/sse.service.ts:32` - the only real consumer.
- `src/entrypoint/events.rs:66` (handler), `src/entrypoint/mod.rs:280` (route), `src/entrypoint/mod.rs:742` (status-only router test).
- `tests/webui_integration_test.rs:102` - asserts `200` and `text/event-stream` only; it never parses a frame.
The only test that pins field names is `request_trace_event_serialization` in `src/entrypoint/events.rs` (asserts `"request_id"`), which is in the same crate and is updated together with the change. REST dashboard types already use camelCase (`SessionSummary`, `BenchmarkItem` at `src/entrypoint/observability.rs:54,159`). No MCP server in `src/` consumes the stream.

## Precondition (hard gate on Task 1.1.2b)
Before the rename merges, Task 1.1.1g runs `grep -rnE "request_id|duration_ms|first_byte_ms|tokens_before|status_code" src tests` (every non-UI consumer; producers such as `src/metrics/` and `src/entrypoint/events.rs` are expected hits) and records the result in `research/spike-notes.md`. At plan revision time the only legacy hit is `src/dashboard.rs:540-546`, which reads `/metrics` `recent_requests` (REST JSON, kept additive by Story 2.1.3), not the SSE stream; `src/mcp_gateway.rs` has no hit (VERIFIED by grep, 2026-10-09; the other `src/` hits are producers or unrelated internals, to be triaged in 1.1.1g). A new hit on the SSE payload blocks 1.1.2b until it is migrated or the dual-emit task is promoted to must-have. CI already builds with `CONSOLETTE_REQUIRE_UI=1` (`.github/workflows/ci.yml:16`); Task 1.1.2f adds a bundle-freshness check so a green Rust test with a stale embedded UI cannot pass.

## Decision
1. Put `#[serde(rename_all = "camelCase")]` on all four SSE data structs (`MetricsTickData`, `RequestTraceData`, `ErrorLoggedData`, `ConfigChangedData`) and on every new nested type (`AttemptTrace`, etc.). The UI models are aligned to the backend shape (the backend is the source of truth; UI field names that have no backend source - e.g. `changedKeys`, `providerId`, `errorDetails` - are renamed or dropped in the UI).
2. No UI adapter layer. No duplicated snake_case keys. Reason: the only consumer is in-repo, an adapter would leave a third naming scheme to maintain, and the REST dashboard already sets the precedent.
3. A **shared fixture** is the anti-drift mechanism: `ui/src/testing/fixtures/sse/*.json` (one file per event type, including a failure trace with attempts). A Rust test serializes representative structs and asserts `serde_json::Value` equality with `include_str!` of the same files; a UI spec loads the same files and feeds them through `SseService`/`MetricsStore` and asserts the rendered view-model. Changing either side without the fixture fails CI on that side.
4. Compatibility plan for the requirement "do not break existing consumers": none exists in-repo; ship a release-note line ("SSE field names are now camelCase"). If an external consumer surfaces before release, add a one-release dual-emit of the legacy snake_case keys for `RequestTraceData` only (precedent: `get_dashboard_config` duplicates keys). This is a revert-sized change, not a design change. It is a **concrete, cut-able task (1.1.2g)**: a `compat_snake_case: bool` config key (default `false`) that makes `RequestTraceData` additionally serialize the legacy snake_case keys; if the task is cut, rollback is revert-only and the legacy-consumer grep below is the only protection.
5. `serde_json::to_string(...).unwrap_or_default()` in `handle_sse_events` stays but a Rust test serializes every event variant so a non-serializable new field cannot become a silent blank frame.

## Consequences
- (+) One naming scheme across REST and SSE; the UI needs no mapping code; the fixture catches drift both ways.
- (-) A wire-breaking change for any unknown external consumer (mitigated by item 4).
- (-) Rust build reads a file under `ui/src/` via `include_str!`; `build.rs` already tracks `ui/src` so touching a fixture triggers a UI rebuild (harmless).

## Alternatives rejected
- UI-side adapter (pitfalls.md): zero wire risk, but only protects hypothetical consumers, adds a mapping layer for 4 event types, and leaves `ErrorLogged`/`ConfigChanged` shapes divergent.
- Dual-emit both casings now: doubles event size on the hot path for no known consumer.
