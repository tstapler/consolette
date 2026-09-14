# Build vs. Buy — auto-model-family alias resolution

**Date**: 2026-09-12 · **Scope**: `project_plans/auto-model-family/requirements.md`
**Question**: for per-request alias→model resolution (error-rate then latency) inside consolette's `Router`, what do we build, borrow, or delegate?

## Context (verified in-repo)

- `Router` owns the dispatch loop shared by every strategy (`src/routing/router.rs:1`), consulting `SessionOverrideStore` first (`router.rs:211`), then `strategy.select(&healthy)` over a health-filtered slice (`router.rs:288`).
- Strategies are pure and health-blind by design (`src/routing/strategy.rs:1-6`): `FallbackStrategy` (first-healthy-wins) and `WeightedStrategy` (static `weight` via `WeightedIndex`, proportional redistribution falls out when cooled-down peers are absent — `strategy.rs:40-42`). Neither reads error-rate/latency.
- Cooldown-only health lives in `HealthRegistry` (`src/routing/health.rs:1`, `health.rs:32`), keyed by upstream index — the same key `UpstreamRef.index` carries (`strategy.rs:11-22`).
- `MetricsCollector` is wired into `Router` (`router.rs:41`) but the requirements' rabbit holes already flag the two gaps: lifetime counters never forget, and `/metrics` is keyed by upstream *name*, not model ID (`requirements.md:73-76`). Per-model ranking needs a new stats dimension + decay.
- Feasibility risk that constrains every option: auth/validation errors return immediately with no failover, so a delisted (404) family member must be excluded *before* dispatch (`requirements.md:89`).

## Option 1 — Adopt a Rust load-balancing crate (tower-balance / tower p2c + PeakEWMA)

**What it is**: `tower 0.5.3` ships `balance::p2c` ("Power of Two Random Choices": sample two ready services, pick least-loaded) plus `load::{PeakEwma, PendingRequests, Constant}` estimators. `PeakEwma` is a moving average of peak latency. The old split crates (`tower-balance 0.3.0` etc.) are ~6 years stale; the live code is the `balance`/`load` features inside `tower 0.5.x`.

**Pros**
- P2C + PeakEWMA is exactly the right shape for "latency-aware spread with inexact measurements" and is battle-tested (Finagle heritage).
- Would remove any hand-rolled weighted-sampling math.

**Cons**
- Wrong signal: P2C optimizes *load* (latency/concurrency of interchangeable replicas), not *quality* (per-model error-rate ranking with error-rate-first ordering). There is no error-rate input to `Load`.
- Wrong unit: tower balances across `Service` instances behind a `Discover` stream; consolette's choice is alias→model-ID string resolution *before* dispatch, on a 2–3 member family, single-user volume. Adopting tower means reshaping `Router`/`RoutingStrategy` (pure fn over a slice) into tower `Service`+`Discover` plumbing for no ranking benefit.
- No decay-for-errors, no per-model-ID stats, no "why" explanation, no delisted-ID exclusion — all still bespoke.
- New dependency + trait-surface churn for a 2-candidate pick.

**Verdict: Not recommended.** Reference the P2C/PeakEWMA *idea* (see Option 3), do not adopt the crate. The impedance mismatch (load-spread vs. error-ranked model pick) exceeds the value.

## Option 2 — Delegate to OpenRouter's built-in auto-router (`openrouter/auto`)

**What it is (current as of Aug 2026)**: `openrouter/auto` classifies each prompt into ~30 task types and picks a model from community-spend signals (NotDiamond predecessor replaced ~2026-08-10; stable slug `openrouter/auto`, early channel `openrouter/auto-beta`). No per-request surcharge — you pay the picked model's rate — with `cost_tier` (low→max), `allowed_models`, and `provider.max_price` guards. `openrouter/free` (not `auto:free`) is the zero-cost pool. Response `model` field reports the concrete model that ran.

**Pros**
- Zero bespoke ranking code; satisfies "zero manual swaps" in the narrow sense — OpenRouter absorbs delist/rotate churn.
- Quality-aware (task classification) rather than just health-aware; frontier quality available at `cost_tier=max`.

**Cons**
- **Fails the free-only default constraint.** Without tight `allowed_models`/`max_price` config a simple prompt can land on an expensive model; the requirements mandate family membership stays within configured models with no new paid spend by default (`requirements.md:45`). Delegation converts a hard invariant into a config-you-must-get-right on someone else's router (wrong plugin-ID config is silently ignored — known pitfall).
- **Control/observability loss**: pick + reason live on OpenRouter's side; dashboard "live pick + why" becomes polling the activity/generation endpoints after the fact instead of reading local stats. Local error-rate/latency signals (the ranking the requirements specify) go unused.
- Single-vendor lock-in for the core routing decision; local-stats work (the durable asset for any future multi-provider family) never gets built.
- Still needs consolette-side membership config to bound cost — so it doesn't even eliminate config work, it just moves it.

**Verdict: Viable (as opt-in fallback, not the default).** Keep as the documented alternative / possible backing for the opt-in *paid* alias, tightly bounded (`allowed_models` + `max_price`, `cost_tier=low`). Do not use for the default free-only family — it violates cost control and the local-stats ranking requirement.

## Option 3 — Bespoke EWMA/decay stats vs. extending in-repo `MetricsCollector`

**What it is**: small per-model-ID rolling stats (success/error counters + latency EWMA with time- or count-based decay) feeding a deterministic ranker: lowest error-rate first, then lowest latency EWMA, ties → config order (cold-start default per `requirements.md:93`).

**Pros**
- Only approach that directly implements the specified ranking (error-rate *then* latency) on the required dimension (model ID, not upstream name).
- Single-user, 2–3-member family: O(n) scan per request is negligible vs. static-pin lookup (meets the perf SLO trivially); no concurrency scale concerns.
- Decay is ~20 lines (e.g. EWMA α on each observation, or periodic halving) — this is not "building a metrics system," it's two rolling numbers per member. The "reckless bespoke stats" fear applies to distributed multi-tenant systems, not here.
- Keeps the "why" local: dashboard can show the two numbers that drove the pick with zero extra plumbing.

**Cons**
- Must add the model-ID stats dimension and decay that `MetricsCollector` lacks today (real work, in the hot path's data plane — needs care to keep transport thin per AGENTS.md).
- Cold-start and delisted-ID exclusion need explicit design (deterministic config-order default; pre-dispatch liveness check since failover won't save a 404).

**Verdict: Recommended.** Extend `MetricsCollector` (or a small sibling store it owns) with per-model EWMA/decayed error-rate + latency; keep the ranker a pure function for testability. Borrow the PeakEWMA/decay *concept* from tower/LiteLLM, write the ~50 lines yourself.

## Option 4 — Fork/adapt in-repo pieces (`WeightedStrategy`, `HealthRegistry`, `SessionOverrideStore`)

**What fits**:
- `RoutingStrategy` trait (`strategy.rs:25-27`) — the correct seam. A new `FamilyRankingStrategy` (or alias-resolution step before `select`) slots in with zero dispatch-loop changes; pure-fn shape keeps it independently testable (AGENTS.md convention).
- `HealthRegistry` cooldown — reuse as the exclusion mechanism for erroring/delisted members pre-dispatch; ranking then orders survivors. Do not overload cooldown with ranking — keep health-blind strategy separation (ADR-003).
- `SessionOverrideStore` (`session_overrides.rs:43`) — consult order already established (session pin → strategy); family alias must sit *below* explicit session pins, preserving current precedence.
- `WeightedStrategy` — do **not** adapt: static weights spread load but never re-rank on degradation (already listed as a rejected alternative in `requirements.md:83`).

**Pros**: smallest diff; preserves ADR-003 separation; rollback stays the existing `POST /api/route` hot-swap (`requirements.md:103`); ships as opt-in route entry alongside pinned entries.

**Cons**: none structural — but these pieces supply *plumbing*, not the ranking signal. Option 4 without Option 3 is a hollow alias (resolution with nothing to rank on).

**Verdict: Recommended (as the integration path for Option 3).** Build the decayed per-model stats (Option 3) and expose them through a new strategy/resolution step on the existing `RoutingStrategy` + `HealthRegistry` + `SessionOverrideStore` seams. LiteLLM's router (Python, deployment/ FB-aware) is reference-only — not adoptable in a Rust single-crate binary; skim its decay/fallback-test design for ideas, do not port it.

## Bottom line

| Option | Verdict |
|---|---|
| tower p2c/EWMA crates | Not recommended (wrong signal, wrong unit) |
| OpenRouter `auto` | Viable as bounded opt-in (paid alias), not the default |
| Bespoke decayed stats on `MetricsCollector` | **Recommended (the ranker)** |
| Adapt in-repo strategy/health/session seams | **Recommended (the integration path)** |

Build a tiny deterministic ranker on existing seams; delegate nothing on the default path.
