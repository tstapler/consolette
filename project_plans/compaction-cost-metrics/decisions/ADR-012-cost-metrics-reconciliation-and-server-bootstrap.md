# ADR-012: Cost-metrics reconciliation model and minimal HTTP server bootstrap

**Status**: Accepted (amended 2026-08-15, repair iteration 1 — see Amendment section)
**Date**: 2026-08-15

## Context

`compaction-cost-metrics` needs to compare actual tokens used (known only
after a provider responds, in `src/providers/mod.rs`) against a counterfactual
estimate of uncompacted tokens (known only inside
`SessionCompactionPipeline::apply`, before the request is sent). These two
facts are produced in different functions, at different times, with no
existing correlation id between them (see `research/architecture.md` §4). We
also need to decide where the JSON endpoint's HTTP surface lives, since no
live axum server exists anywhere in this codebase today (`research/architecture.md`
§2, confirmed by `grep -rn "axum::serve" src` returning nothing and
`src/main.rs` only wiring `Run`/`Mcp`/`CompactSession`).

## Decision

**Reconciliation**: adopt architecture.md's "option 3, generalized." A new
`RequestId(Uuid)` newtype is generated once per `apply()` call (default
`Uuid::nil()`, so `CompactionReport::default()` stays reflexively `Eq`; the
real id is generated explicitly at the one call site inside `apply()`) and
threaded alongside `SessionKey`. `CostTracker` stores one row per
`(SessionKey, RequestId)` in a bounded per-session ring (capacity 200, oldest
evicted first — matches the "no per-request history growth" pitfall guidance
while still allowing per-tier breakdown). The row is written in two steps,
not two independent optional writes: `CompactHooks::post_compact` inserts the
row **synchronously**, `counterfactual: None`, at hook-fire time (never
skipped, so there is always something to reconcile against); a same-estimator
counterfactual token count is then filled in via `CostTracker::
record_counterfactual(&SessionKey, RequestId, TokenCount)`, called from a
spawned task so the (possibly network-bound, see Amendment below) estimator
never blocks `apply()`. Separately, `CostTracker::
record_actual_usage(&SessionKey, RequestId, ActualUsage)`, inserted at the
call site that already extracts `usage.*` in `src/providers/mod.rs`, writes
`actual` whenever a response completes, and is idempotent per `request_id`
(a second call replaces, never adds — the provider layer's retry loop may
call it more than once). `record_actual_usage`/`record_request_failed` never
create a row that doesn't already exist (see Amendment below); only the
`post_compact` insert may create one. A row with only `counterfactual`
populated is `Pending`; every error/timeout path at the call site must call
`record_request_failed` to mark it `Abandoned` rather than leaving it
`Pending` forever, and a read-time age-out sweep abandons any row still
`Pending` past a configurable age so orphans are bounded even without a live
error path (mitigates the pitfalls.md "orphaned pending entries" failure
mode).

**HTTP server bootstrap**: stand up the smallest possible axum server —
one `Router` with one route (`GET /v1/cost/{session_key}`) and one
`axum::serve(TcpListener, router)` call — behind a new `consolette serve-cost`
subcommand. Per the Amendment below, this process is also the *only* place
that constructs a `SessionCompactionPipeline`; the route and the pipeline
share one `Arc<CostTracker>`. This explicitly does NOT wire
`providers`/`routing` into a full request-serving proxy beyond what
`session_compaction` already needs; it mirrors the already-unwired
`MemoryAppState`/`handler_memory_*` pattern in `src/memory/mod.rs` for the
route itself. Binds `127.0.0.1` by default (loopback-only); the port is read
from the existing config file with a `--port` CLI override, not a
config-blind flag.

## Alternatives rejected

- **Restructure `post_compact` to fire after the response** — rejected. The
  original rationale here (breaking existing `CompactHooks` consumers'
  timing) was checked against the code and found false: no production
  `CompactHooks` implementor exists — `grep -rn "impl CompactHooks for" src`
  returns only `#[cfg(test)]` structs — and reinjection is called inline in
  `apply()`, not through the hook trait. The still-valid reason to reject
  this alternative is that it would couple `apply()`'s hot, synchronous path
  to provider network latency, which the two-step write above avoids without
  restructuring anything.
- **A separate short-TTL staging `moka` cache keyed by `RequestId` alone,
  joined and cleared on arrival** — rejected in favor of storing both sides
  directly in `CostTracker`'s per-session record: avoids a second cache with
  its own eviction-race surface, and naturally satisfies the exact/estimated
  provenance requirement (`Pending` is just an as-yet-unpopulated field, not
  a separate mechanism).
- **Wire the new route into a full `providers`/`routing` proxy server** —
  rejected per explicit user scope decision (2026-08-15): out of scope for
  this feature; tracked as a separate, larger effort.
- **(Rejected, repair iteration 1) Keep `serve-cost` as a route-only process
  and have `consolette cost-report` construct its own in-process
  `CostTracker`** — rejected because nothing then wires
  `SessionCompactionPipeline` + `CostTrackingHook` into any process the CLI
  or HTTP route can observe; the two surfaces would agree only in a
  hand-populated test, never in a real deployment. See Amendment.
- **(Rejected, repair iteration 1) Persist `CostTracker` state to sqlite via
  the existing `rusqlite` dependency** — rejected; the constraint
  (requirements.md) is explicitly no new persistent datastore, and adding
  durable storage does not by itself solve the "nothing wires the pipeline
  into a live process" problem — it only changes where an already-missing
  process's state would live.

## Amendment (2026-08-15, repair iteration 1)

Two corrections, both required to pass architecture/adversarial review:

1. **Ownership.** `consolette serve-cost` is the single stateful process
   that owns a `SessionCompactionPipeline` (with `CostTrackingHook`
   registered) *and* the `GET /v1/cost/{session_key}` route, both against one
   `Arc<CostTracker>`. `consolette cost-report <key>` no longer constructs
   its own `CostTracker`; it is a `reqwest` HTTP client (already a
   dependency) against `serve-cost`'s route. This makes "the CLI and HTTP
   surfaces agree" a structural property (one process, one tracker) instead
   of an assumption resting on both surfaces happening to be told to
   construct equivalent state.
2. **The counterfactual is not always network-free.** This ADR originally
   said `post_compact` writes `counterfactual` "synchronously (no network —
   tiktoken-rs only)". That's wrong for the Anthropic estimator path, which
   calls the real `POST /v1/messages/count_tokens` API (per requirements.md's
   resolved Open Questions — tiktoken undercounts Claude by 15-30%+). The
   corrected model is the two-step write described above: the row insert is
   synchronous and network-free; filling in `counterfactual` is not, and is
   therefore done off the hot path via `record_counterfactual`, with a
   bounded concurrency limit (`tokio::sync::Semaphore`) and no retry on a 429
   (typed `EstimatorError`, surfaced as `Abandoned` on that row rather than
   blocking others).

## Consequences

- `RequestId` becomes the first correlation id in this codebase threaded
  across the compaction/provider-response boundary — future features
  needing the same join (e.g. per-request cost headers) can reuse it.
- The minimal server bootstrap is deliberately throwaway-shaped (one route)
  so it does not accidentally become "the" proxy server by accretion;
  Epic 2 tasks must not add unrelated routes to it.
- Bounded per-session ring (200 rows) means very long sessions lose their
  oldest per-request rows before the 1hr TTL evicts the whole session;
  cumulative running totals (by tier) are kept as separate fields, folded in
  only when a row transitions to `Reconciled` (not while `Pending` or
  `Abandoned`), so eviction of individual rows and orphaned pending rows
  never inflate the cumulative numbers used by the success-metric comparison.
- `serve-cost` being the only process that constructs the pipeline means an
  operator must run it for cost tracking to occur at all; compaction via
  other entry points (e.g. a future embedded use of `SessionCompactionPipeline`
  outside `serve-cost`) will not be cost-tracked unless it also registers
  `CostTrackingHook`. This is an accepted scope boundary, not an oversight —
  wiring cost tracking into every possible pipeline construction site is out
  of scope for this feature.
