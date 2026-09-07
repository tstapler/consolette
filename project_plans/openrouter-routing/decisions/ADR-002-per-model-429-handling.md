# ADR-002: Per-Model 429s Fold Into Rolling Error Rate — No Sibling Hard-Exclusion Registry

**Status**: Accepted
**Date**: 2026-09-05

## Context

`research/architecture.md` flags an explicit fork point: when a specific
free model returns 429, should the design (a) add a second,
`HealthRegistry`-shaped `DashMap`-based registry keyed by model id that
`Router::dispatch` checks in addition to the existing whole-upstream
`HealthRegistry`, preserving exact `Retry-After` fidelity per model; or (b)
fold the 429 into the model's rolling error rate (driving it toward 1.0 for
a window), losing per-model `Retry-After` precision but adding no new
structural type?

`research/pitfalls.md` (Phase 2, resolved) establishes that OpenRouter's
free-model rate limits are **account-wide, not per-model**: 20 req/min and
50-or-1,000/day, shared across every `:free` model, not budgeted separately
per model id.

This matters directly for the fork: `Router::dispatch`'s existing
`is_rate_limited()` arm already calls
`self.health.trip(chosen.index, override_duration)` using the parsed
`Retry-After` (`src/routing/router.rs:339-344`). Every per-model
`UpstreamRef` this feature's `expand_candidates` produces shares the *same*
`chosen.index` (the one `openrouter`-kind upstream's index) — only `.model`
differs. So tripping `chosen.index`'s cooldown on a 429 **already cools down
every free model behind that upstream, with full `Retry-After` fidelity,
using zero new code** — because the underlying constraint really is
account-wide, one whole-upstream cooldown is the semantically correct
representation of it, not an artifact to work around.

## Decision

**Option (b): fold a per-model 429 into that model's rolling error rate.**
No sibling per-model hard-exclusion registry.

`OpenrouterScoringStrategy::record_outcome` (`src/routing/openrouter_scoring.rs`),
on seeing `error_kind == Some("rate_limited")` for a per-model candidate,
pushes `RATE_LIMIT_SYNTHETIC_FAILURES` (= 5) synthetic `false` samples into
that model's `RollingErrorRate` — enough to dominate its rolling window
without waiting for real attempts to accumulate, so the composite score
(ADR-003) immediately deprioritizes it — *deprioritizes*, not hard-excludes:
it can still be re-selected by the strategy's epsilon-greedy exploration
term once the synthetic samples age out of the 15-minute window, which is
the correct recovery behavior for an account-wide limit that resets on its
own schedule rather than a specific model being "bad."

The account-wide `Retry-After` fidelity that fork (a) exists to preserve is
still fully delivered — by the existing whole-upstream `HealthRegistry.trip`
call, unmodified, exactly as it already handles every other provider's
rate limits.

## Alternatives Considered

| Option | Rejected because |
|--------|-------------------|
| (a) Sibling `DashMap<String, ProviderState>`-shaped registry keyed by model id, checked as an extra pre-selection filter alongside `HealthRegistry` | Solves a problem that doesn't match reality: the constraint is account-wide, not per-model, so a per-model cooldown would represent something OpenRouter doesn't actually do. It also adds a new structural type plus a new eviction policy plus a new `Router::dispatch` call site — exactly the "general per-upstream-metrics refactor" `requirements.md`'s Rabbit Holes section warns against, for a fidelity guarantee (`Retry-After`) the existing whole-upstream trip already provides for free given the shared-index fan-out. |
| Do nothing extra — let the rolling error rate accumulate the 429 as one ordinary failure sample, no synthetic weighting | Under a 15-minute rolling window with potentially few real samples for a rarely-picked model, one real failure sample might not move the composite score enough to matter before the next selection, and the account-wide cooldown already blocks re-selection of *every* model for the `Retry-After` duration anyway — so the synthetic weighting's actual job is to keep that model deprioritized a little past the point the whole-upstream cooldown itself lifts, not to react within the current request. Rejected the "no weighting" variant as under-reacting to a real signal for negligible implementation cost saved. |

## Consequences

- No new registry type, no new eviction policy, no new `Router::dispatch`
  call site beyond the already-planned `record_outcome` hook (ADR/plan
  Phase 3).
- `Retry-After` fidelity is preserved via the pre-existing
  `HealthRegistry.trip(chosen.index, override_duration)` path — verified by
  a regression test asserting that a 429 from a per-model `UpstreamRef`
  trips cooldown for every other per-model `UpstreamRef` sharing the same
  index (plan.md Task 4.2.3c).
- If OpenRouter's actual behavior later turns out to include *any*
  genuinely per-model throttling distinct from the account-wide cap (not
  confirmed by Phase 2 research), this decision should be revisited — the
  synthetic-error-rate approach would under-react to a real per-model-only
  signal. Flagged in plan.md's Unresolved Questions as a watch item, not a
  blocker.
