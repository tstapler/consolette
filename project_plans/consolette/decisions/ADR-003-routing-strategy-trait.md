# ADR-003: Routing — One `RoutingStrategy` Trait (fallback | weighted) + `HealthRegistry` + `Availability`

**Status**: Accepted
**Date**: 2026-07-17

## Context

FR-3 (CD-3) requires two routing strategies selected per-route: `fallback`
(ordered, current behavior, default) and `weighted` (OpenRouter-style split), with
**429/health cooldown applying to both** (FR-3.4) and both living behind **one
strategy interface**, reusing `fallback.rs` where practical (FR-3.5). All-unhealthy
→ clear 503 (FR-3.6).

Today `FallbackHandler::dispatch` (`src/fallback.rs`) conflates three concerns:
health/cooldown state (single-slot `FallbackState`, primary only), selection policy
(hardcoded primary→fallback), and dispatch/retry orchestration (the error-class
match arms). The ADR-006 `ProviderState`/`try_exit_cooldown` cooldown machine is
sound but single-slot. Rate limiting (FR-4) must compose without the strategy
knowing it exists.

## Decision

Separate the three concerns so **selection becomes pluggable while cooldown state
and the dispatch loop are shared by all strategies** (weighted-router research §1).

- **`RoutingStrategy` trait — pure selection, health-blind:**
  `fn select(&self, healthy: &[UpstreamRef]) -> Option<UpstreamRef>`. It receives
  only the already-filtered healthy candidates. Health is deliberately invisible
  here — that is what lets fallback and weighted share one cooldown implementation.
  - `FallbackStrategy::select` = `healthy.first()` (candidates arrive in config
    order, so "first healthy" reproduces primary→fallback exactly).
  - `WeightedStrategy::select` = `rand::distributions::WeightedIndex` over healthy
    weights (`.max(1)` guard). A cooled-down peer is simply absent, so survivors
    keep their relative proportions == free proportional redistribution.
- **`HealthRegistry` — cooldown factored out**, `DashMap<usize, ProviderState>`
  keyed by upstream index, **reusing the ADR-006 `ProviderState` enum and the
  TOCTOU-safe check-and-clear** (via `DashMap::get_mut` exclusive per-key guard).
  Per-upstream `can_cooldown: bool` preserves "Bedrock never cooled down".
  **Hard rule: never hold a DashMap guard across `.await`** (all health ops are
  pure-sync nanosecond `Instant` comparisons).
- **`Availability` predicate — health/cooldown ONLY:**
  `fn is_available(&self, idx: usize) -> bool`. The router applies it as a
  pre-selection filter to drop cooled-down upstreams before `strategy.select`.
  `HealthRegistry` is its only implementor. **Rate limiting is deliberately NOT an
  `Availability` source** — governor commits limiter state on a successful check, so
  it must run at most once per attempt, *after* a candidate is chosen. Rate limiting
  therefore integrates via `AdmissionControl::admit()` post-selection (ADR-004), not
  here. `Availability` stays a thin, health-only seam (kept as a trait for
  testability and possible future health signals).
- **Shed handling = the re-select loop, not a predicate.** When `admit()` sheds the
  chosen upstream, the router adds it to `already_tried` and re-selects from the
  remaining healthy pool. For 2–5 upstreams this **rejection sampling** is cheap, and
  it is exactly how weighted redistribution around a shed (or 429'd) upstream is
  achieved — one loop handles both.
- **Router owns the dispatch loop**, shrinking the candidate set per attempt
  (`candidates = healthy_subset MINUS already_tried`; `strategy.select(&candidates)`),
  **reusing the existing error-class match arms verbatim** (`is_validation`/`is_auth`
  → no failover; `is_rate_limited` → `health.trip` then continue; transient →
  continue). Same-upstream retries (Bedrock `2^n` backoff) stay **inside the
  provider**; the router only fails over to a *different* upstream.
- **Streaming discipline:** the connect-time retry loop must open a working body
  (or exhaust candidates) **inside `dispatch`** before handing the
  `ProviderResponse::Stream` outward. Cross-upstream failover is only possible
  **before the first byte flushes** — once axum has sent `200 OK` + partial SSE,
  a mid-stream failure can only be an SSE `error` event, never a transparent
  failover (weighted-router research §6). The current providers already return
  `Err` on connect-time status before yielding the stream, so no new hazard.

Selection is `Arc<dyn RoutingStrategy>` (stateless/immutable); the router is
`Arc<Router>` in `AppState`, replacing `Arc<FallbackHandler<…>>` +
`Arc<FallbackState>`.

## Alternatives Considered

| Option | Rejected because |
|--------|-----------------|
| Add `attempt`/health args to `select()` | Leaks health into the strategy, defeating shared cooldown; the router-owned shrinking loop keeps the requested `select(&[UpstreamRef])` signature pure. |
| Fork `fallback.rs` into two handlers | Duplicates the error-arm logic and cooldown; violates FR-3.5 ("reuse where practical"). |
| Keep single-slot `FallbackState` | Cannot model N upstreams; weighted needs per-upstream health. Generalized to `HealthRegistry`. |
| `Arc<RwLock<HashMap<usize,ProviderState>>>` (stay literally on ADR-006) | Correct but coarser: one global lock; tripping one upstream blocks reads of another. `DashMap` per-key sharding is preferred; sync critical sections are nanoseconds. |
| Bake rate limiting into `HealthRegistry` / make it an `Availability` source | Rejected. Couples two mechanisms with different durations (429 = 300s cooldown; RPM refill = seconds) AND breaks governor's commit-on-check contract (a predicate could be polled 0..N times per attempt, corrupting the bucket). Rate limiting is a single post-selection `AdmissionControl::admit()` call instead. |
| Hand-rolled cumulative-weight scan | `WeightedIndex` is clearer and handles the all-zero-weights edge; candidate sets are tiny either way. |

## Consequences

- New dep: **`rand = "0.8"`** (`WeightedIndex` at `rand::distributions`,
  `thread_rng()` — lock-free, per-task). Pin 0.8 deliberately: 0.9 moved these to
  `rand::distr::weighted` / `rand::rng()`.
- **`dashmap` bumped 5 → 6** (also required by governor in ADR-004).
- `Provider` trait and the error-classification contract (`providers/mod.rs`) are
  reused unchanged; providers keep their internal retry/backoff.
- Weighted redistribution is automatic (cooled/limited peers absent from the
  healthy slice); no explicit reweighting math.
- `/metrics` cooldown block generalizes from two hardcoded providers to a
  per-upstream `remaining_secs()` iteration over `HealthRegistry`.
- Existing `fallback.rs` unit tests port to `router.rs` (normal→first,
  429→cooldown+next, 4xx→no-failover, cooldown-skips, weighted-redistribution).
- **Deviation from ADR-006 recorded:** ADR-006 chose `tokio::sync::RwLock<ProviderState>`
  (single slot) to stay async-native. `HealthRegistry` moves to `DashMap<usize,
  ProviderState>` (sync per-key sharding) because it needs N per-upstream slots and
  the critical sections are nanosecond `Instant` comparisons with no contention. The
  ADR-006 atomic check-and-clear semantics are preserved via `DashMap::get_mut`. Hard
  rule: never hold a DashMap guard across `.await`. (The conservative
  `Arc<RwLock<HashMap<..>>>` alternative is noted above and rejected as coarser.)
</content>
