# ADR-001: Model-List Cache — `moka::sync::Cache`, Not Hand-Rolled, Not `future::Cache`

**Status**: Accepted
**Date**: 2026-09-05

## Context

Two Phase 2 research files disagree. `research/stack.md` recommends a
hand-rolled `Mutex<Option<(Instant, Vec<ModelInfo>)>>` for the free-model-list
cache and argues against `moka`. `research/build-vs-buy.md` found that
`moka` (`moka = { version = "0.12", features = ["future"] }`, `Cargo.toml:86`,
resolved 0.12.16 in `Cargo.lock`) is already a direct dependency and is the
established in-repo idiom for exactly this shape of problem in five other
modules: `src/compression/rewind.rs`, `src/cost_metrics/store.rs`,
`src/memory/dedup.rs`, `src/memory/store.rs`,
`src/session_compaction/session_state.rs`. Per the architecture-review
checklist's "consistency with build-vs-buy decision" criterion,
`build-vs-buy.md` is authoritative here.

A second, narrower question `build-vs-buy.md` didn't resolve: all five
existing usages call `moka::future::Cache`, whose entire API
(`get`, `get_with`, `invalidate`) is `async`. But `research/architecture.md`
establishes that `RoutingStrategy::select`/the new `expand_candidates` method
must stay synchronous and pure (ADR-003), and this plan's per-model
429-handling design (ADR-002) needs `record_outcome` — also synchronous — to
be able to invalidate the cache when it detects the "single model 404ing"
staleness signal. `future::Cache`'s async-only surface can't be called from
either.

## Decision

Use `moka::sync::Cache<(), Arc<Vec<FreeModelEntry>>>` (a single entry, keyed
by the unit key `()`, since there is exactly one free-model list to cache;
`FreeModelEntry { id, price_prompt, price_completion }`, not a bare `String`
— see the Amendment below) — **not** `future::Cache`, and not a hand-rolled
`Mutex<Option<(Instant, ..)>>`.

- `moka::sync::Cache` still gives the two things that made `build-vs-buy.md`
  reject the hand-rolled option: native TTL eviction and a real
  `invalidate()`, with no new dependency.
- Unlike the five precedents, this cache's only *writer* is a single
  background refresh task (`Story 2.1.2`), not many concurrent async callers
  racing to populate it — so `future::Cache`'s single-flight `get_with`
  initializer buys nothing here that a plain background loop doesn't already
  give for free. What this cache needs instead is synchronous `get`
  (from `expand_candidates`) and synchronous `invalidate`
  (from `record_outcome`'s model-not-found handling), which only
  `moka::sync::Cache` provides.
- TTL is set to 15 minutes, not the "couple of hours" `requirements.md`
  floated — see `research/pitfalls.md`'s silent-spend pitfall: a model going
  free→paid mid-TTL produces a successful, billed response, not an error, so
  nothing else in this design catches it. Bounding the TTL to 15 minutes
  bounds that exposure window directly. A background task refreshes every 5
  minutes (well under the TTL), so in normal operation the TTL is a
  backstop, not the primary refresh driver.

## Alternatives Considered

| Option | Rejected because |
|--------|-------------------|
| Hand-rolled `Mutex<Option<(Instant, Vec<ModelInfo>)>>` (`stack.md`) | Reinvents TTL/eviction/single-flight that a dependency already in `Cargo.toml` provides for free, and diverges from the established in-repo idiom used in five other modules for this exact shape of problem. |
| `moka::future::Cache` (matching the five precedents exactly) | Its `get`/`get_with`/`invalidate` are all `async fn`. `RoutingStrategy::select`/`expand_candidates` must stay sync (ADR-003), and `record_outcome`'s model-not-found invalidation path is also sync — an async-only cache API can't be called from either without breaking the trait's sync contract or spawning a detached invalidation task that races the very check it's trying to make authoritative. |
| Keep `future::Cache` for TTL and add a separate `ArcSwap<Vec<String>>` for sync reads | Two sources of truth for the same data (which is current: the moka entry or the ArcSwap snapshot?) with no atomic way to keep them in sync across an invalidate. `moka::sync::Cache` alone already gives sync reads without this duplication. |

## Consequences

- No new *dependency* — but `Cargo.toml:86`'s
  `moka = { version = "0.12", features = ["future"] }` must gain `"sync"`
  alongside `"future"` (`moka`'s `sync` module is gated behind its own
  Cargo feature, separate from `future`, so today's feature list does not
  yet expose it) — see plan.md Task 2.1.1a. This keeps the existing 5
  `future::Cache` usages compiling unchanged; `"sync"` is additive.
- This is the sixth in-repo `moka` usage but the first using
  `moka::sync::Cache` instead of `moka::future::Cache` — worth a one-line
  comment at the definition site (`src/providers/openrouter/cache.rs`)
  pointing future readers at this ADR, so the inconsistency with the other
  five doesn't read as an oversight.
- The background refresh task (Story 2.1.2) and the on-demand one-shot
  refresh triggered by a real invalidation (Story 2.1.3, Amendment below)
  are the only writers; neither may panic mid-refresh in a way that never
  reschedules/completes (a `Weak`-upgrade failure is the intended, sole exit
  condition for the periodic task — see plan.md Task 2.1.2b; the on-demand
  task resets its single-flight guard on every exit path — see Task
  2.1.3d).

## Amendment: Per-Dispatch Money-Safety Backstop (2026-09-05)

**Context.** This ADR's original TTL rationale (above) explicitly
acknowledged, but did not close, a gap: a model going free→paid mid-TTL
produces a successful, billed response — not an error — so nothing in the
original design detects it before the next refresh. Both the
architecture-review and adversarial-review of this plan flagged this as a
BLOCKER against `requirements.md`'s "must not silently spend money" hard
constraint (a bounded exposure window is a materially weaker guarantee than
"must not," and `research/pitfalls.md` §2 had explicitly asked for a
belt-and-suspenders re-check of the *specific selected model's* price, not
just list membership).

**Decision.** Two changes, both additive to the TTL mechanism above (the
TTL is retained as a backstop-of-last-resort, not replaced):

1. The cache's value type is `Vec<FreeModelEntry>` (`{id, price_prompt,
   price_completion}`), not a bare `Vec<String>` of ids.
   `OpenrouterProvider::send()`'s pre-flight check now verifies the
   *specific selected model's* cached price is `(0.0, 0.0)`, not just that
   its id is present in the list (plan.md Task 2.1.2c). This is
   defense-in-depth against a future bug in the free-filter itself, not a
   fix for the mid-TTL window by itself — everything in this list is
   already expected to be free by construction.
2. `OpenrouterProvider::send()` additionally inspects OpenRouter's response
   for a per-request cost/usage field (if the API exposes one — confirming
   this is a research spike, plan.md Task 1.2.4a). A nonzero cost reported
   for a nominally-free-routed request triggers an immediate hard
   `model_cache.invalidate()` plus a `tracing::error!` — this is the actual
   backstop for the mid-TTL window, since it reacts to a specific billed
   request rather than waiting for the next scheduled refresh (plan.md
   Story 1.2.4).

**Consequences.**
- **Update (2026-09-08, `sdd:6-verify`)**: confirmed via OpenRouter's public
  docs (no API key needed) that `GET /api/v1/generation?id=<id>` returns a
  `total_cost` field for any prior generation — mechanism 2 is implementable.
  It remains a documented no-op today (unimplemented, not "not possible")
  pending a live key to verify end-to-end; see plan.md's Unresolved
  Questions and Risk Control for the current status. Until it lands, the
  original 15-minute-TTL-bounded exposure (not eliminated) is the actual
  residual guarantee, and shipping in the meantime must be surfaced to
  Tyler as an explicit interim sign-off decision, not silently accepted.
- Mechanism 1 requires `list_free_models()` to return price alongside id
  (plan.md Story 1.2.2), which is a small, purely additive change to data
  already being parsed off `/models`' response — no new HTTP call.
