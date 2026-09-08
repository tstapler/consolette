# ADR-003: Composite Scoring Formula — Weighted Min-Max Sum + Epsilon-Greedy

**Status**: Accepted
**Date**: 2026-09-05

## Context

`requirements.md`'s Open Questions flags this as "the single most important
decision" in this plan: combine three heterogeneous signals — rolling
latency (ms), rolling error rate (fraction), and a static coding-benchmark
rank (0-100 pass rate) — into one score `OpenrouterScoringStrategy::select`
uses to pick among health-filtered free-model candidates.
`research/build-vs-buy.md` requires the formula to be hand-rolled (no
scoring-library dependency) with explicit normalization and a monotonicity
guarantee: a candidate at the worst end of every signal must never
outscore one at the best end of every signal. `research/features.md` flags
three concrete failure modes to design against: cold-start bias (a model
with zero samples), oscillation between near-tied scores, and an
explore/exploit tension for rarely-picked models.

## Decision

For each per-model candidate in the current health-filtered slice:

1. **Per-signal normalization**, computed fresh over the current candidate
   slice on every `select()` call (no persisted normalization state):
   - Latency: `norm_latency = 1 - (p50_ms - min_p50) / (max_p50 - min_p50)`,
     lower latency scores higher. If `max_p50 == min_p50` (including the
     single-candidate case), `norm_latency = 1.0` for all.
   - Error rate: `norm_error = 1 - (rate - min_rate) / (max_rate - min_rate)`,
     lower error rate scores higher. Same tie guard.
   - Bench rank: `bench_score = BENCH_TABLE[model] / 100.0` — used
     **as-is, not min-max normalized against the candidate slice**. Unlike
     the two live signals, this is a static, absolute quality measure; two
     mediocre candidates in a small pool shouldn't make one look "great"
     relative to the other the way min-max would.
   - **Cold start** (zero samples for a signal): that *signal's* component
     defaults to the neutral midpoint `0.5`, not the best or worst value —
     avoiding both starving a new model (never picked because it's assumed
     worst) and over-favoring one (assumed best). Latency and error-rate
     samples are recorded together per attempt (`record_outcome`), so a
     model is always cold-start for both or neither.
   - **Unranked model** (not in `BENCH_TABLE`): `bench_score = 0.5`
     (neutral), logged once per model id
     (`tracing::warn!(model, "unranked in bench table")`) — a fail-soft
     config-drift condition per `research/ux.md`, never a request-level
     failure.

2. **Weighted sum**:
   `composite = 0.5 * norm_error + 0.3 * norm_latency + 0.2 * bench_score`.
   Weights sum to 1.0, so `composite ∈ [0, 1]`. Error rate is weighted
   highest: on a rate-limited free pool, a failing attempt both wastes a
   scarce request *and* still needs a retry, so it's the most expensive
   signal to get wrong. Latency is second — it's the direct, live
   user-facing cost. Bench rank is third and deliberately the smallest
   weight: it's a coarse, infrequently-refreshed prior that live signals
   should dominate once real samples exist, which cold-start's neutral
   default lets happen gradually rather than all at once.

3. **Selection: epsilon-greedy, ε = 0.1.** 90% of the time, pick the
   candidate with the highest `composite` (ties broken by first-in-list —
   stable, not random, so identical scores don't churn the choice run to
   run). 10% of the time, pick uniformly at random from the healthy slice
   instead of the argmax. This is the concrete, fixed answer to
   `features.md`'s "explore/exploit tension for rarely-picked models": a
   model that's currently deprioritized (e.g. after a synthetic 429 penalty,
   ADR-002) still gets picked roughly 1 time in 10 regardless of its score,
   so it keeps accumulating real samples and can recover once its rolling
   window ages the penalty out — without a general configurable weighting
   system (`requirements.md`'s Rabbit Holes explicitly rules that out for
   v1).

4. **Monotonicity guarantee**: because every component is normalized into
   `[0, 1]` with a consistent "higher is better" direction and the weights
   are non-negative and sum to 1, a candidate at `(norm_latency=0,
   norm_error=0, bench_score=0)` scores exactly `0` and one at `(1, 1, 1)`
   scores exactly `1` — the worst possible candidate can never outscore the
   best possible one. Verified directly by unit tests (plan.md Task
   4.2.5a-c), not just argued algebraically.

## Alternatives Considered

| Option | Rejected because |
|--------|-------------------|
| Z-score normalization instead of min-max | Z-scores are unbounded and can flip sign in ways that break the `[0,1]`-per-component monotonicity guarantee build-vs-buy.md asked for; min-max keeps every component trivially bounded and the "worst never beats best" property provable by construction, not just by convention. |
| Equal weights (1/3 each) | Doesn't reflect that an error costs more than added latency on a request-and-daily-capped free pool (a failed attempt still consumes rate-limit budget *and* needs a retry) — equal weighting would under-react to exactly the signal `requirements.md`'s success metric ("measurably shifting traffic away from a model as its live latency/error signal worsens") cares about most. |
| Pure argmax, no exploration | Starves a deprioritized-but-recovering model forever once its score falls behind, since it would never be picked again to generate the samples that would let its rolling window recover — directly reproduces the cold-start/explore-exploit risk `features.md` flagged. |
| Softmax / Thompson-sampling-style probabilistic selection over all candidates (weighted by score, not just top-1 vs. random) | A more principled explore/exploit approach, but a genuinely more complex mechanism (weighted sampling à la `WeightedStrategy`) for a formula `requirements.md`'s Rabbit Holes says to keep simple and concrete; epsilon-greedy gets the same qualitative "poor performers still recover" property with two fixed constants and no new sampling machinery beyond what `WeightedStrategy` already uses (`rand::thread_rng`). |
| Cold-start defaults to worst-case (assume unproven = bad) or best-case (assume unproven = great) | Both directly reproduce the cold-start bias `features.md` flags — worst-case starves new models of the samples needed to prove themselves; best-case over-floods a genuinely bad new model with traffic before any real signal exists. Neutral `0.5` avoids both failure directions. |

## Consequences

- Constants (`RATE_LIMIT_SYNTHETIC_FAILURES = 5`, `EPSILON_EXPLORATION = 0.1`,
  weights `0.5/0.3/0.2`) are fixed in code
  (`src/routing/openrouter_scoring.rs`), not configurable — matches
  `requirements.md`'s explicit instruction to fix one concrete formula
  rather than build a general weighting system. Revisiting them requires a
  code change, not a config edit.
- `select()` remains pure and sync (ADR-003 of the base `consolette` ADR
  set, `project_plans/consolette/decisions/ADR-003-routing-strategy-trait.md`)
  — the epsilon coin-flip and argmax are both plain in-memory computation,
  no I/O, no lock held across an `.await`.
- Every candidate's score breakdown (not just the winner's) is retained for
  `to_metrics_json`'s `openrouter_scoring` block (plan.md Phase 5) — needed
  for the after-the-fact auditability `research/features.md` calls for.
