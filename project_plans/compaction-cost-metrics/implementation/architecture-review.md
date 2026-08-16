# Architecture Review: compaction-cost-metrics (re-review, repair iteration 1)

**Verdict**: CONCERNS

Scope: this is a **re-review of the 5 previously-BLOCKED items only** (B1-B5 from the original review), plus a spot-check for new problems introduced by the repair. Concerns/Nitpicks/praised items from the original review were not re-litigated per the task instructions.

Reviewed: current `implementation/plan.md` (including its "Rework Notes" and "ADR-012 Amendment" sections), `decisions/ADR-012-cost-metrics-reconciliation-and-server-bootstrap.md`, `decisions/ADR-013-pricing-table-source.md`. Code re-read directly (not taken on the rework notes' word): `src/session_compaction/mod.rs`, `src/session_compaction/hooks.rs`.

---

## Blocker re-review

### B1 — No live process wires the tracker to a real surface — **RESOLVED**

Plan now makes `serve-cost` (Epic 2.3) the single process that constructs `SessionCompactionPipeline` with `CostTrackingHook` registered, owning one `Arc<CostTracker>` shared by the pipeline and the axum route. `cost-report` (Epic 3.1) is redefined as a `reqwest` HTTP client of that route — it no longer constructs its own tracker (Task 3.1.1a-g; scope guardrail section explicitly forbids a second construction site). Story 2.3.1e adds a new integration test driving a real `apply()` call through `serve-cost`'s own pipeline and reading it back via the HTTP route — this is the correct regression guard for the original defect (it exercises the actual wiring, not a hand-populated tracker). Story 3.2.1d's "two surfaces agree" test is now framed correctly as a regression guard against a *future* reintroduction of independent computation, not (as before) the only place the claim was true.

This makes the "two surfaces agree" claim structurally true, matching remediation option 1 from the original review.

**Spot-check — new problem?** Making `cost-report` a `reqwest` client of `serve-cost` does create a new runtime dependency (the CLI now does nothing useful unless `serve-cost` is running). This is not a regression, though: per the original B1 finding, standalone `cost-report` never returned real data anyway (guaranteed 404/`SessionNotFound`). The plan states the failure mode honestly and distinctly (Task 3.1.1c: "could not reach cost server ... is `consolette serve-cost` running?" vs. the separate not-found message), so the dependency is disclosed rather than silently assumed. No new blocker.

### B2 — `post_compact` had no `SessionKey` — **RESOLVED**

Plan now changes `CompactHooks::post_compact`'s signature itself (not a second method) to `fn post_compact(&self, ctx: &PostCompactContext<'_>)`, where `PostCompactContext<'a> { session_key, session, pre_compaction_messages, report }` (Task 2.1.1b). Verified directly against `src/session_compaction/mod.rs:150-153`:

```rust
{
    let state = session_lock.read().await;
    self.hooks.run_post_compact(&state, &report);
}
```

At this call site, `session_key` (the function parameter), `messages` (the original pre-compaction parameter — not consumed, since `out = messages.clone()` at line 107), `state` (the read-lock guard, alive for the block), and `report` are all simultaneously in scope. A `PostCompactContext<'a>` constructed and used entirely within this block can unify `'a` to the guard's lifetime (the shortest of the four) without issue, since the struct doesn't escape the block. **The lifetime parameter works as claimed.**

Also verified the rework notes' factual claim underpinning this fix: `grep -rn "impl CompactHooks for" src` — all four implementors (`hooks.rs:75,101,116`, `mod.rs:232`) are `#[cfg(test)]`-only, confirming there is no production call site whose signature-change blast radius exceeds "update four test structs." Step 0.5 item 2's corrected rationale matches this.

### B3 — Fire-and-forget spawn + no-op-on-no-match drops actuals — **RESOLVED**

Plan replaces the single fire-and-forget write with a split: `record_pending` is called **synchronously** inside `post_compact` (before it returns), inserting a `Pending` row with no token counts; the estimator runs in a spawned task and later calls `record_counterfactual`; `record_actual_usage` becomes an **upsert** that creates a `Pending` row (with `actual_tokens` populated) if none exists yet, rather than the previous no-op-and-drop. Story 1.3.2's acceptance criteria include an explicit adverse-ordering test (actual arrives before the pending row) and an idempotency requirement (retry of `record_actual_usage` replaces, never double-adds).

This directly addresses the original race: row existence is now an invariant of `post_compact` returning, not a hope that a spawned task wins a race.

**Spot-check note (not a blocker)**: the plan specifies the Reconciled-transition fold happens in "whichever of `record_counterfactual`/`record_actual_usage` runs second" via one shared helper, and separately requires that a second, idempotent `record_actual_usage` call *not* re-fold into `totals_by_tier`. The plan documents the intended behavior and both are covered by explicit acceptance-criteria tests (Story 1.3.2's third bullet), but the write-up doesn't spell out the specific guard ("only fold if `status != Reconciled` at time of write") that makes double-fold-safety mechanical rather than convention-based. This is an implementation-detail gap, not a design flaw — the AC would fail a shipped implementation that got it wrong — so it's noted for the implementer's attention rather than treated as blocking.

### B4 — Invalid `counterfactual - actual` subtraction — **RESOLVED**

Plan redefines `tokens_saved = counterfactual_est − compacted_est`, where both operands come from the same `TokenEstimator` call — one against pre-compaction `messages`, one against `apply()`'s post-compaction `out` (Story 1.3.3, Task 2.1.1c). This is a same-units, same-methodology subtraction that is exactly `0` at `CompactionTier::Off` by construction (since `out == messages` there — confirmed against the `off_tier_is_a_no_op` test in `src/session_compaction/mod.rs:180-188`, which already asserts `out == messages` at `Off`). `actual_tokens` (real `usage.*`, `TokenSource::Exact`) is kept as a separate reported field used only for the dollar figure and as a sanity cross-check (Epic 4.1's end-to-end test explicitly checks sign-agreement, not numeric equality, between the two). This is exactly the remediation the original review proposed.

### B5 — Unsound cumulative aggregation — **RESOLVED**

Both sub-defects addressed:

1. **Pending/Abandoned inflation**: `TierTotals` is now folded into only at the moment a row transitions to `Reconciled` (Task 1.3.1a/1.3.2c); `Pending` and `Abandoned` rows never contribute, and eviction from the bounded ring (`push_record`, Task 1.3.1c) is explicitly documented as not touching `totals_by_tier`, so an evicted unreconciled row was never counted and can't desync anything.
2. **Unwritten cost fields / report-time single-model pricing**: pricing now happens per-record, under the same write-lock that performs the `Reconciled` fold (Task 1.3.2's fold step), so `report_for_session` is a pure read with "zero `PricingTable` lookups" as an explicit acceptance criterion (Story 1.3.3), and a multi-model session's totals are a sum of already-individually-priced records rather than one report-time lookup — this is the correct fix for the ring-eviction/multi-model problem the original review identified.

**Spot-check — does folding only at `Reconciled` still let old counterfactual estimates count correctly once they do reconcile?** Yes: `record_counterfactual` sets `counterfactual_est`/`compacted_est` on the existing row without folding; the row stays `Pending` until `record_actual_usage` later arrives, at which point the fold-trigger check (both estimate fields + `actual_tokens` present) fires and folds the *already-recorded* counterfactual value in correctly. The one edge case — a row's counterfactual is recorded, but before the actual arrives the row is pushed out of the 200-record ring by newer records in the same session — is explicitly called out in the plan as a bounded, logged (`tracing::warn!`) loss rather than a silent one, which is an accepted and disclosed trade-off, not a re-introduction of the original silent-loss defect.

---

## Overall assessment

All five original blockers are resolved at the plan level, verified against the current plan.md text and cross-checked against the real `src/session_compaction/mod.rs`/`hooks.rs` code rather than the rework notes' own description. No new architecture-level blocker was found in the three spot-check areas requested (reqwest hard dependency, `PostCompactContext` lifetime, `TierTotals` fold-at-`Reconciled` correctness).

One non-blocking implementation-detail gap is noted above (B3's double-fold guard isn't spelled out as a specific check, though it's covered by an acceptance-criteria test that would catch a wrong implementation) — this is why the verdict is **CONCERNS** rather than **CLEAN**, mirroring the original review's severity scale. It does not warrant another repair iteration; it's implementer guidance, not a plan defect.
