# Adversarial Review: openrouter-routing

**Date**: 2026-09-07 (re-review of 2026-09-05 blockers, after repair pass)
**Verdict**: CONCERNS

## Re-Review of Previously-Blocked Items

All four items previously marked BLOCKED are now RESOLVED in the current
plan.md. One of the fixes surfaces a new, narrower money-safety gap, logged
below as an additional Concern rather than a blocker (see Concerns list).

- [x] **RESOLVED — `expand_candidates` session-pin passthrough.** Story
  4.2.4 / Task 4.2.4a now partitions candidates on `(index ==
  self.openrouter_index) && model.is_none()`, not on index alone. Any
  same-index candidate whose `.model` is already `Some(..)` (i.e., a
  `SessionOverrideStore` pin already applied by `effective_candidates`
  before `expand_candidates` runs) passes through unchanged, plan.md:1378-1396.
  A regression test is explicitly named in Task 4.2.4c: "pin a session to
  one specific free model ... assert `expand_candidates` returns exactly
  that one candidate unchanged against a warm 5-model snapshot, and assert
  the same against a cold (`None`) snapshot" (plan.md:1433-1441). The
  cold-cache case is also covered as its own acceptance criterion
  (plan.md:1403-1414): a pinned candidate is *not* dropped when
  `snapshot() == None`, unlike an unpinned openrouter candidate (which is
  dropped entirely in that case — the pre-existing money-safety rule for
  the unpinned path is preserved).
  - **Gap found in this re-review, logged as a new Concern (not a
    blocker):** the cold-cache passthrough for a *pinned* candidate means
    that specific dispatch bypasses `send()`'s per-dispatch price recheck
    (Task 2.1.2c only rechecks `Some(list)` snapshots — "a `None` snapshot
    ... falls through and lets the real API call be the source of truth,"
    plan.md:772-773) and reaches OpenRouter's real API with zero local
    price verification. If a pinned model has gone free→paid and the cache
    happens to be cold at that exact moment (e.g., mid-self-heal after the
    Blocker-3/4 fix's own invalidation), the request dispatches unverified,
    relying solely on Story 1.2.4's post-hoc cost check as backstop — which
    is itself contingent on an unconfirmed OpenRouter API field. See new
    Concern below.

- [x] **RESOLVED — `cached_count == 1` special case.** Task 2.1.3b now
  explicitly branches `cached_count == 1` to the real-invalidation path
  unconditionally, bypassing the unsatisfiable `distinct_failed <
  cached_count` general rule for that case (plan.md:880-889). The
  acceptance criteria and Task 2.1.3c's test list both name this case
  explicitly (plan.md:820-836, 898-906), with the rationale for accepting
  the "cheap false-invalidate on a genuine data-policy toggle when the pool
  has shrunk to 1" tradeoff spelled out (plan.md:826-830). No new concern
  found here.

- [x] **RESOLVED — immediate on-demand refetch on real invalidation.**
  Task 2.1.3d adds `trigger_immediate_refresh()`, called from the
  real-invalidation branch of `record_not_found_and_maybe_invalidate`
  (plan.md:890-892), spawning a one-shot refresh rather than waiting on the
  5-minute periodic tick. A single-flight guard
  (`refresh_in_flight: AtomicBool`, compare-exchange) prevents concurrent
  on-demand triggers from stampeding, with an explicit test asserting the
  mock `/models` endpoint sees exactly 1 call when 2 real-invalidations
  fire back-to-back (plan.md:861-870, 924-931). A separate test asserts
  self-heal happens "well under 5 minutes" rather than waiting for the
  periodic tick (plan.md:857-860).
  - **Checked for a new stampede/deadlock risk, per this re-review's
    brief:** none found in the described design — the `AtomicBool` guard is
    reset on every exit path (success, refresh error, or `Weak` upgrade
    failure), and nothing holds a lock across an `.await`. One minor,
    non-blocking observation: the single-flight guard only serializes
    on-demand triggers against *each other* — the independent periodic
    background-refresh task (Task 2.1.2b) does not check
    `refresh_in_flight` before calling `refresh()`, so an on-demand trigger
    landing at the same moment as a periodic tick could still produce 2
    concurrent `/models` calls (not the many-request stampede
    `pitfalls.md` §4 warns about, just an occasional redundant one — added
    to Minors below rather than raised as a Concern).

- [x] **RESOLVED — money-safety per-dispatch price re-verification.** Task
  2.1.2c now has `send()` look up the *specific selected model's*
  `FreeModelEntry` in the cache snapshot and reject
  (`ProviderError::ModelUnsupported`, no network call) if no entry matches
  **or** a matching entry's price is nonzero — a genuine per-model check,
  not the old list-membership-only check (plan.md:756-774). This
  literally satisfies the item as framed ("real per-dispatch price
  re-verification for the specific selected model, not just
  list-membership").
  - **Important scope caveat, worth stating plainly so this isn't
    over-read:** this recheck verifies the *specific selected model's*
    price against the *same cached data* that already passed the filter at
    last refresh — it cannot detect a live price change that happened
    *after* that refresh (a stale-but-not-yet-expired cache entry still
    reports the old, free price). The plan itself is honest about this: it
    labels Task 2.1.2c "defense-in-depth against a future filter bug ...
    not by itself a fix for the free→paid-mid-TTL gap" (plan.md:767-771),
    and identifies Story 1.2.4's post-hoc nonzero-cost detection as "the
    actual backstop for the free→paid-mid-TTL case" (plan.md:139-143).
    That backstop is post-hoc (the one triggering request is still billed
    before being caught) and is explicitly contingent on Task 1.2.4a's
    unconfirmed research spike — if OpenRouter's API turns out not to
    expose a per-request cost field, Story 1.2.4 ships as a documented
    no-op and the residual bounded-but-not-closed exposure is called out as
    requiring Tyler's explicit sign-off, not silent absorption
    (plan.md:144-151, 177-185). Given the plan (a) does the maximum
    structurally-available mitigation, (b) is explicit about what that
    mitigation does and doesn't catch, and (c) routes the unresolved case
    to an explicit human sign-off rather than papering over it, this
    counts as resolved for planning purposes — the residual risk is
    disclosed, not hidden.

## Concerns

- [x] **RESOLVED — `ModelStats` GC is now an explicit no-op on `None` snapshot.** Story 4.2.4's Acceptance Criteria and Task 4.2.4b (plan.md) now state explicitly that `expand_candidates` only calls `model_stats.retain(..)` inside the `Some(snapshot)` branch — a cold cache leaves `model_stats` completely untouched, with a Given-When-Then acceptance criterion and a dedicated test in Task 4.2.4c (populate 3 models' stats, force `snapshot() == None`, assert all 3 survive one `expand_candidates` call). This is exactly this Concern's recommended fix.

- [x] **RESOLVED (verified in repair-loop re-check) — integration test for "every free model unavailable → `ProviderError::Exhausted`, no fallback to paid".** Task 4.3.1f (plan.md:1627-1644) now adds `dispatch_should_return_exhausted_when_all_free_model_candidates_are_cooling_down`: a real `Router::dispatch` integration test building a warm 3-model snapshot, tripping `HealthRegistry`'s cooldown for the shared upstream index, and asserting `dispatch()` itself (not `select()` in isolation) returns `Err(ProviderError::Exhausted)` with zero requests recorded against any other upstream. This is the exact end-to-end test this Concern asked for.

- [x] **RESOLVED — mixed `openrouter_scored` + paid-upstream route now has a config-time guard.** Task 4.3.1g (plan.md) extends Story 4.3.1's validation pass: a route whose `strategy == Strategy::OpenrouterScored` that also lists any non-`openrouter`-kind upstream now fails `from_config` with an error naming the route and the offending paid upstream, with a concrete Given-When-Then (Story 4.3.1's Acceptance Criteria) and a dedicated unit test. Story 4.2.4's own "mixed route" test remains a pure-function `expand_candidates` robustness test (that config shape now can never reach `Router::dispatch` in practice, since Task 4.3.1g rejects it at load time).

- [x] **RESOLVED (option taken: amend the docs, not build a config subsystem) — bench-table override path is now explicit.** Task 4.1.1d (plan.md) adds a doc comment on `BENCH_TABLE` stating the override path is edit-and-rebuild; requirements.md's Scope bullet is amended to say the same explicitly, with the `conf.d`-mergeable-override alternative recorded as considered and rejected for scope reasons (Large appetite already fully allocated across 9 epics). This is this Concern's second recommended option, taken deliberately rather than left implicit.

- [x] **RESOLVED — `/models` refresh-failure behavior is now an explicit acceptance criterion + test.** Story 2.1.1's Acceptance Criteria (plan.md) now states the exact "serve stale, don't hard-fail" contract this Concern asked for verbatim ("a failed `refresh()` leaves a previously-populated, not-yet-expired cache entry untouched" / "on an already-empty/expired cache leaves `snapshot() == None`"), with Given-When-Thens and a dedicated Task 2.1.1d covering both cases against a mock error response.

- [ ] **Free-tier tool-call incompatibility (`research/pitfalls.md` §2) has no dedicated handling or observability distinction.** A subset of free models reliably 404 on tool-use requests while serving plain chat fine; the plan's generic error-kind/rolling-error-rate machinery will eventually down-rank them but with no way for Tyler to distinguish "this model is bad" from "this model doesn't support tools and Claude Code always sends tools" in the dashboard/log output — muddying exactly the "why was this picked/deprioritized" auditability `research/ux.md` calls a minimum bar. — **Recommendation**: at minimum, note this as a known rough edge in the observability plan (e.g. surface the `error_kind` distribution alongside `error_rate` in `observability_snapshot()`, which the plan doesn't currently include), or accept as out of scope for v1 and say so explicitly in plan.md.

- [x] **RESOLVED (option (a) taken) — session-pin cold-cache price-recheck bypass is now an explicit, logged tradeoff.** Story 2.1.2's Acceptance Criteria and Task 2.1.2c (plan.md) now state explicitly that this fallthrough is only reachable for a pinned dispatch (an unpinned candidate is already dropped by `expand_candidates` on a cold cache), and require `send()` to emit `tracing::warn!(model, ..)` naming the model whenever it forwards a request with no cache snapshot to verify against — with a Given-When-Then acceptance criterion and a dedicated test in Task 2.1.2d. The narrow exposure itself is unchanged (still contingent on Story 1.2.4's post-hoc cost check as ultimate backstop, same as the money-safety item above), but it is no longer silent — this is option (a) of this Concern's recommendation, made concrete rather than left to prose.

## Minors

- `moka::sync::Cache`'s internal implementation is crossbeam-based with its own background housekeeping (distinct from the five existing `future::Cache`/Tokio-integrated usages) — worth a one-line note in code (beyond the ADR cross-reference already planned) that this is a deliberately different runtime model, so a future contributor doesn't "fix" the inconsistency by converting it back to `future::Cache` without re-reading ADR-001.
- `OpenrouterProvider::send()`'s money-saving pre-flight cache-membership check (Task 2.1.2c) blurs Epic 1.2's stated "thin, mechanical provider" boundary by giving the provider cache-awareness beyond simple request forwarding — justified by the money-safety rationale, but worth a one-line comment at the call site pointing back to that rationale so it doesn't read as scope creep on a later read.
- No single test demonstrates the full feedback loop implied by the Success Metric "measurably shifting traffic away from a model as its live latency/error signal worsens" end-to-end (record several bad outcomes via `record_outcome`, then assert a subsequent `select()` shifts away) — the pieces are each unit-tested (scoring purity, epsilon-greedy distribution, stats-feed) but never chained together in one test.
- ADR-001/Task 2.1.1a correctly flags `moka::sync::Cache`'s exact 0.12.16 API surface as an open, blocking-but-owned unresolved question — good practice, just confirm it's actually resolved before Story 2.1.1 code is written, not discovered mid-implementation.
- New (2026-09-07 re-review): the on-demand refresh's single-flight guard (Task 2.1.3d's `refresh_in_flight` `AtomicBool`) only serializes on-demand triggers against each other — the independent periodic background-refresh task (Task 2.1.2b) doesn't check or set that same flag, so an on-demand trigger landing at the same moment as a periodic tick could still fire 2 concurrent `/models` calls. Harmless (moka's `insert` is idempotent, and it's at most a 2x overlap, not the many-request stampede `pitfalls.md` §4 warns about), but worth having both refresh paths share one guard for cleanliness if it's a small change during implementation.
