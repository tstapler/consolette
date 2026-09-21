# Research: Pitfalls of Stats-Driven Model Routing (Agent 4)

**Date**: 2026-09-12
**Scope**: synthetic family alias (e.g. `auto-coding`) resolved per-request to best real model ID, ranked by local error-rate then latency from `/metrics`.
**Method**: codebase orientation only (no code changes). Key sources: `src/routing/router.rs:1-9` (error-class branching), `src/metrics/counters.rs:46,129-185` (per-upstream DashMap, lifetime AtomicU64, mean-only latency), `src/routing/health.rs` (cooldown registry), requirements in `project_plans/auto-model-family/requirements.md`.

## 1. Thundering herd onto the "best" model

Greedy argmin(error-rate, latency) sends 100% of family traffic to the current winner. For a single-user proxy this still matters because opencode/Claude Code burst parallel requests (multi-file edits, background tasks), and the winner is typically a free-tier model with the tightest rate limit — the herd converts a latency advantage into 429s within seconds.

What to design against: never pure argmin. Minimum options: epsilon-greedy / weighted sampling (send N% to runner-up), or sticky-pick with periodic re-evaluation rather than per-request re-ranking. Note the existing `WeightedStrategy` (`src/routing/strategy.rs`) already spreads load statically — the family resolver should reuse that shape, not replace it with winner-takes-all.

## 2. Oscillation / flapping between two near-tied models

When two models have nearly identical stats, tiny noise flips the pick every request. Each flip changes connection reuse, prompt-cache warmth, and first-byte latency, making both look worse and amplifying the flip. Dashboard "why" becomes unreadable noise.

What to design against: hysteresis — require a margin (e.g. error-rate delta > X pp, or latency delta > Y%) before dethroning the incumbent; re-resolve on a timer or after K requests, not on every request; log pick changes as events so flapping is visible.

## 3. Stale stats: lifetime counters never decay (VERIFIED)

`/metrics` counters are monotonic lifetime `AtomicU64`s with no timestamp, no window, no decay (`counters.rs:90-126` construction, `record_request` only ever `fetch_add`). A model that was bad for a week in August outvotes a model that recovered yesterday, forever. Conversely a model that was great for months coasts on reputation long after it degrades — the exact failure mode the feature exists to fix reacts slowly.

What to design against: Phase 3 must define an explicit aging policy (sliding window, EWMA, or periodic reset) as a new stats dimension — it cannot reuse `/metrics` counters verbatim. Also decide whether decay applies symmetrically (forgive old errors AND forget old successes).

## 4. Cold start: no stats, first pick is a guess (VERIFIED as known risk)

A fresh family member (newly added free model ID after OpenRouter rotation) has zero samples. Argmin over empty stats either divides by zero, always prefers the untested model (0 errors / 0 requests = 0% error rate — the most dangerous default), or never tries it (starvation, §8).

What to design against: deterministic default from config order (already the stated plan in requirements §Feasibility Risks); minimum-sample threshold before a model's stats count (below threshold → rank by config order, flagged as "cold" on dashboard); treat 0-request models as unknown, never as perfect.

## 5. Simpson's-paradox-style aggregation errors

`record_request` folds every attempt into one per-upstream bucket (`counters.rs:148-164`): successes, errors, `duration_sum_ms`/`duration_count` (mean only). At minimum three confounders hide in that single mean:

- **Request mix**: short chat completions vs. long tool-heavy agentic loops have 10-100x latency differences. A model that happened to serve mostly short requests looks "faster."
- **Time-of-day / upstream congestion**: free-tier latency varies by hour; lifetime means blend peak and off-peak.
- **Error-type blending**: `record_error_kind` (`counters.rs:202+`) classifies timeout/auth/rate-limit/validation globally, while per-upstream buckets only count success/error. A model with frequent 429s (capacity signal, transient) ranks identically to one with frequent 500s (quality signal) — yet the correct response differs (back off vs. abandon).

What to design against: rank on per-error-class rates if possible, or at minimum exclude rate-limit responses from the "quality" error rate (they measure popularity, not brokenness); consider latency medians/tails over means; segment by recency (§3) which partially de-confound time.

## 6. Free-tier rate limits punish the winner (feedback loop)

This is the loop that ties §1 + §5 together and is the highest-probability failure for the default free-only family: best model → most traffic → hits free-tier 429 first → 429s recorded as errors → error-rate spikes → resolver demotes it → traffic shifts to runner-up → runner-up hits its 429 → oscillation between exhausted models, each accumulating error counts that persist forever (§3). Note the router already has a rate-limit cooldown path (`router.rs:5-6`, `health.rs`), but cooldown is per-upstream-index while family resolution is per-model-ID — the two layers must agree on identity or cooldown won't protect the demoted winner.

What to design against: treat 429/rate-limit as a *backpressure* signal (temporary exclusion + cooldown), not a *quality* signal (permanent error-rate penalty); exclude cooldown-covered models from ranking before scoring; consider concurrency caps per family member.

## 7. Router returns immediately on auth/validation errors — no failover (VERIFIED)

`router.rs:1-9` + `entrypoint/observability.rs:348`: validation and auth errors propagate immediately with no failover. A delisted/rotated model ID (OpenRouter returns 404, typically classified as validation/auth-side, not transient) fails the whole request instead of falling through to the next family member. So failover *cannot* compensate for stale family membership — resolution must exclude dead IDs *before* dispatch (liveness check against OpenRouter catalog or recent hard-failure exclusion), exactly as requirements §Feasibility Risks states.

Related: a single poisoned member (e.g. permanently 401 due to a bad key scope on one upstream) poisons every resolution that picks it, with no in-request recovery.

What to design against: pre-dispatch filtering (membership refresh + hard-failure denylist with TTL); decide whether family resolution needs its own try-next loop *inside* the resolver for the validation-error class, since the router won't do it.

## 8. Exploration starvation: losers never get sampled, winners never get challenged

Any deterministic ranking creates a one-way ratchet: the demoted model gets zero new traffic, so its stats freeze at the moment of demotion and it can never recover its rank even if the upstream fixes itself. Combined with §3 (no decay), demotion is effectively permanent.

What to design against: guaranteed exploration budget (e.g. every Nth request or X% probes the non-pick); successful probes must actually move the needle, which requires bounded windows (§3) — exploration without decay is wasted traffic.

## 9. In-memory DashMap stats lost on restart (VERIFIED structure, INFERRED lifecycle)

Per-upstream stats live in `DashMap<String, UpstreamCounters>` (`counters.rs:46`) populated by `record_request`; there is no persistence path in the metrics module (contrast `context_forensics/store.rs`, `cost_metrics/store.rs` which have stores). Every proxy restart wipes learned rankings → post-restart cold start on all members (§4) → first requests after deploy always take the config-order default, and a restart-heavy week replays the same bad picks.

What to design against: decide explicitly — accept amnesia (document that restarts reset ranking, keep cold-start default sane) or persist compact snapshots (EWMA state or windowed counts) to disk on interval/shutdown. At minimum, dashboard should show sample counts so "best with n=3 since restart 10 min ago" is visibly low-confidence.

## 10. Single-user low sample sizes make error-rate noisy (structural)

Single-user volume means tens of requests/day, not thousands. Error-rate differences of 1-2 requests (e.g. 1/8 = 12.5% vs. 0/10 = 0%) flip rankings on pure noise. Latency means over n<10 are dominated by single outliers (one 60s tail event in `duration_gt60s` bucket poisons the mean for days).

What to design against: minimum-sample thresholds + confidence display (Wilson interval or even just "n=" next to every figure); prefer latency median or trimmed mean over the current sum/count mean; shrink small-sample estimates toward the family mean; dashboard must show the raw counts behind the pick, not just the verdict.

## 11. Stats keyed by upstream *name*, not model ID (VERIFIED gap)

`upstreams: DashMap<String, UpstreamCounters>` is keyed by the config upstream name (`counters.rs:46`, `record_request(upstream: &str, ...)`), and `RouteUpstreamRef` pins one model per entry — but a family alias resolves *multiple model IDs through shared OpenRouter upstreams*. Two family members served via the same `openrouter` upstream entry would share one stats bucket, making per-model ranking impossible without a new dimension.

What to design against: new per-(upstream, model) stats key (or per-model side table) recorded at dispatch time with the resolved model ID; backfill/migration story for existing lifetime counters; `/metrics` shape change and dashboard update.

## 12. Cost leakage: the paid opt-in alias must not contaminate the free default

Requirements specify two aliases (free-only default, paid opt-in). Pitfalls: a shared ranking table lets paid-model success stats leak into free-family decisions or vice versa; a misconfigured family membership (paid model ID in the free list) silently spends money with no guardrail, violating the "no new paid spend by default" constraint; OpenRouter `:free` suffix is a naming convention, not an enforced billing boundary — a rotated ID without the suffix may bill.

What to design against: family membership as explicit allowlists per alias (never pattern-derived at request time); a billing guard (e.g. free alias rejects non-`:free` IDs, or cross-checks the pricing table in `cost_metrics/pricing.rs`); per-alias resolution counters (`/metrics`: resolutions, fallback-to-default) so leakage is auditable.

## Design-against checklist (summary for Phase 3)

1. No pure argmin — weighted/epsilon sampling + hysteresis + re-resolve cadence.
2. Bounded stat windows or EWMA; never rank on lifetime counters directly.
3. Cold-start rule: config order + minimum-sample threshold; unknown ≠ perfect.
4. Separate 429/backpressure (temporary exclusion) from quality errors (ranking penalty).
5. Pre-dispatch membership filtering; do not rely on router failover for dead IDs.
6. Exploration budget so demoted models can recover.
7. Per-(upstream, model) stats dimension; upstream-name keying is insufficient.
8. Restart story: accept amnesia explicitly or persist snapshots; always show sample counts.
9. Free/paid alias isolation with an enforced billing guard.
10. Dashboard "why" limited to the two signals + n + recency, per requirements scope cap.
