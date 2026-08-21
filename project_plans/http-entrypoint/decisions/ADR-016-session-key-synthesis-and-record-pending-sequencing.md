# ADR-016: Per-request SessionKey/RequestId synthesis, CompactionTier::Off reuse, and record_pending-first sequencing

**Status**: Accepted
**Date**: 2026-08-19
**Related**: [requirements.md](../requirements.md), [ADR-003 (routing strategy)](../../../docs/adr/), `src/cost_metrics/tracker.rs`, `src/cost_metrics/store.rs`, `src/session_compaction/tiered.rs`

## Context

`CostTracker` and its backing `SessionCostStore` were designed for the session-compaction feature, where a `SessionKey` identifies a long-lived Claude Code conversation and `CompactionTier` identifies which compaction tier (if any) ran for a given request within that session. The HTTP entrypoint introduces a second, unrelated caller of `CostTracker`: every inbound `POST /v1/messages` or `POST /v1/chat/completions` request must also be recorded, but an HTTP request arriving at the entrypoint has no existing `SessionKey` — there is no session-compaction session backing it.

Two additional facts from reading `src/cost_metrics/tracker.rs` and `src/cost_metrics/store.rs` directly constrain the design:

1. `CostTracker::record_pending` is the *only* method that calls the store's creating `SessionCostStore::get_or_init`. `CostTracker::record_actual_usage` and `CostTracker::record_request_failed` both call the store's non-creating `SessionCostStore::get`, which returns `None` (a silent no-op) for a session that was never first seen via `record_pending`.
2. `CompactionTier` (`src/session_compaction/tiered.rs`) has no variant that means "this is not a compaction request at all" — its closest fit is `CompactionTier::Off`, doc-commented "Below the micro threshold: no compaction runs."

Without an explicit decision, a future maintainer could plausibly call `record_actual_usage` directly on a freshly-synthesized `SessionKey` without first calling `record_pending`, which would silently no-op for every single HTTP request and produce an empty cost report with no error anywhere.

## Decision

For every inbound HTTP request at the entrypoint:

1. Synthesize `let session_key = SessionKey::new(format!("http:{}", Uuid::new_v4()));` and `let request_id = RequestId::new();` once, before dispatch.
2. Call `tracker.record_pending(&session_key, request_id, CompactionTier::Off).await` **before** calling `Router::dispatch`. This is a hard ordering requirement, not an optimization — it is the only call that creates the session entry in `SessionCostStore`, and every later `record_actual_usage`/`record_request_failed` call for this same `request_id` depends on it having already run.
3. Reuse `CompactionTier::Off` to mean "no compaction tier applies; this is a live proxied HTTP request." This is a deliberate semantic reuse across two unrelated features, not a new enum variant, to avoid widening `CompactionTier`'s scope for a single caller.
4. On dispatch success, call `record_actual_usage_from_anthropic_response` (`src/cost_metrics/mod.rs`) with the (already-translated-to-Anthropic-shape, since `Router::dispatch` speaks Anthropic wire format) response body.
5. On dispatch failure before any bytes are returned to the client, call `tracker.record_request_failed(&session_key, request_id).await`.
6. On a mid-stream cut, record whatever partial usage was accumulated by the `CostTrackingStream` tee (see Story 2.2.1) via `record_actual_usage` with `TokenSource::Estimated`, falling back to `record_request_failed` if no usage frame was ever seen.

Each HTTP request gets its own freshly-synthesized `SessionKey` (not reused across requests) — the entrypoint has no notion of a multi-turn "session" the way session-compaction does; every request stands alone from a cost-tracking perspective.

## Alternatives Considered

1. **Add a new `RequestKind` field to `CostRecord`/`CompactionTier`-adjacent schema to distinguish HTTP-proxied requests from compaction requests explicitly.** Rejected: widens a cost-tracking schema shared with the already-shipped session-compaction feature for the benefit of a single new caller; `CompactionTier::Off` already carries the correct meaning ("no compaction happened") without a schema change, and the reuse is documented here and in the plan's Domain Glossary instead.
2. **Call `record_actual_usage` directly without a prior `record_pending`, relying on it to lazily create the session.** Rejected: `record_actual_usage` deliberately uses the non-creating `store.get()` (confirmed by reading `src/cost_metrics/tracker.rs`), and changing that method's behavior to auto-create would change its contract for the existing session-compaction caller too — out of scope and riskier than sequencing the two calls correctly at the new call site.
3. **Reuse one long-lived `SessionKey` per entrypoint process (not per-request).** Rejected: would aggregate every HTTP request's cost into a single indistinguishable session record, defeating the purpose of `report_for_session` for anyone inspecting per-request cost after the fact.

## Consequences

- Every entrypoint dispatch path (success, pre-first-byte failure, mid-stream cut) must call `record_pending` first; this is enforced by construction if all three paths share one small helper function rather than duplicating the sequencing inline (see plan Story 2.1.2).
- `CompactionTier::Off`'s doc comment should be read as "no compaction tier applies," which now covers two cases (below-threshold compaction, and non-compaction HTTP dispatch) — a future reader of `tiered.rs` alone would not learn this from that file, so the reuse is called out in the plan's Domain Glossary as the canonical pointer back to this ADR.
- `report_for_session` on one of these synthetic per-request `SessionKey`s will always report exactly one request's cost — this is expected, not a bug.
