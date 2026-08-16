# Adversarial Review: compaction-cost-metrics (re-review, repair iteration 1)

**Verdict**: CONCERNS

Scope: this is a targeted re-review of only the 7 blockers (B1–B7) from the
original `adversarial-review.md`, against `implementation/plan.md`'s repair
iteration 1 and the amended `decisions/ADR-012-*.md` / `decisions/ADR-013-*.md`.
Concerns/Minors from the original review were explicitly out of scope per the
review brief and are not re-litigated here. All claims below are VERIFIED
against plan.md/ADR-012/ADR-013 content and, where noted, against current
`src/` on `main` (e2aae05) via direct `grep`/`Read`, not against the plan's own
"Rework Notes" summary of itself.

---

## B1 — RESOLVED

**Original defect**: three separate processes each constructed their own
`CostTracker`; nothing wired compaction into any process the CLI/HTTP surfaces
could read, so both surfaces would always return "not found" against real
traffic.

**Fix as specified**: Epic 2.3 (plan.md lines 531–564) makes `consolette
serve-cost` the single process that constructs one `Arc<CostTracker>`, builds
a `SessionCompactionPipeline` with `CostTrackingHook` registered against that
same `Arc`, and hosts `GET /v1/cost/{session_key}` against it (Task 2.3.1b).
Epic 3.1 (lines 569–609) turns `cost-report` into a `reqwest` HTTP client of
that route — it no longer constructs a `CostTracker` at all (Task 3.1.1b/c).
ADR-012's Amendment §1 states the same ownership model, and the "Alternatives
rejected" section explicitly rejects the old route-only-server-plus-standalone-CLI
design with the correct reasoning. Task 2.3.1e adds a direct regression test:
construct `serve-cost`'s pipeline+tracker+route exactly as the command does,
drive one real `apply()` through it, and assert the HTTP route reflects it —
this is the right test shape to catch a regression back to two trackers.

This is a real structural fix: there is now exactly one `CostTracker` instance
per running `serve-cost` process, and the CLI is provably reading from it. The
plan's Guardrails section (lines 691–699) also forbids a second construction
site, closing the "someone adds a convenience in-process CLI path later"
regression risk.

**Residual note (not a blocker, in scope for context only)**: Success Metric 1
("the two surfaces agree") is now trivially true by construction rather than
by independent computation — this was the plan's own chosen remediation path
(a) from the original review, so it counts as resolved, not as a new problem.

---

## B2 — RESOLVED

**Original defect**: fire-and-forget `tokio::spawn(record_estimate)` could
complete after `record_actual_usage`, and the no-op-on-missing-row spec
silently dropped the actual.

**Fix as specified**: Story 1.3.2 (lines 296–315) splits the write exactly as
the original remediation demanded. `record_pending` inserts the row
synchronously with no token counts (item 1); the estimator's result is filled
in later via `record_counterfactual` (item 2); `record_actual_usage` is
explicitly an **upsert** (item 3): if no row exists yet, it creates one with
`actual_tokens` populated and `status: Pending`, rather than a no-op. The
"Adverse-ordering test (new, repair iteration 1)" acceptance criterion (line
311) directly exercises the exact interleaving B2 named — actual arrives
before the pending row exists — and asserts the value is not dropped and later
reconciles exactly once. Task 2.1.1c/g also correctly route estimator failure
to `record_request_failed` → `Abandoned`, so a failed estimator doesn't leave
the row `Pending` forever, which was an adjacent risk of the same design.

The remediation is more thorough than the minimum requested (it also handles
idempotent double-delivery, folded into the same fix — see B7 below).

---

## B3 — RESOLVED

**Original defect**: `get_or_default` copied "verbatim" from
`SessionStateStore` used `get`-then-`insert`, a TOCTOU race on concurrent
fresh-key access, and the plan's own 20-concurrent-callers test would trigger
or flake it.

**Verification against actual task text, not just the rework-notes summary**:
Task 1.3.1b (line 280–283) gives the literal implementation:

```
pub async fn get_or_init(&self, key: &SessionKey) -> Arc<RwLock<SessionCostState>> {
    self.cache.get_with(key.clone(), async { Arc::new(RwLock::new(SessionCostState::default())) }).await
}
```

This is `moka::future::Cache::get_with`, which is documented to run the
initializer exactly once per key even under concurrent callers and return the
same `Arc` to every caller (VERIFIED: `moka = { version = "0.12", features =
["future"] }"` in `Cargo.toml:80`, and `get_with` is `future::Cache`'s
documented atomic-init API in that version line). This is a genuine code
change, not a comment claiming the bug is fixed — the actual accessor body
changed from the flagged `get`-then-`insert` shape to `get_with`.

The corresponding regression test, Task 1.3.1d (lines 289–293) and the AC on
line 265–266, retains the 20-concurrent-callers-on-a-fresh-key shape but now
asserts `Arc::ptr_eq` equality across all 20 results and that the init closure
ran exactly once — this is the correct assertion shape for `get_with`'s
guarantee and will pass (not flake) against the `get_with`-based
implementation, resolving the original "the plan ships a test that fails
against its own code" defect.

**Non-creating `get` usage, checked as instructed**: Task 1.3.1b also defines
`pub async fn get(&self, key: &SessionKey) -> Option<Arc<RwLock<SessionCostState>>>`
(non-creating, line 282). Story 1.3.2 explicitly enumerates which calls may
create: only `record_pending` (item 1, "may create") and one specific branch
of `record_actual_usage` (item 3's adverse-ordering create, explicitly scoped:
"not a general-purpose creating path"). `record_counterfactual` (item 2) is
specified to use "non-creating `get`"; `record_request_failed` (item 4) only
flips a status on an existing row (implicitly non-creating, consistent with
Task 1.3.2e's description "Implement `record_request_failed`" with no mention
of `get_or_init`). `report_for_session` (Story 1.3.3, line 349) is explicitly
specified to use "the store's non-creating `get`, never `get_or_init`." I did
not find any task text that has `record_actual_usage`/`record_request_failed`
calling the creating path outside the one explicitly-scoped adverse-ordering
branch — the creating/non-creating split is applied consistently across all
five write/read entry points named in the original blocker.

---

## B4 — RESOLVED

**Original defect**: `record_actual_usage` after TTL eviction would go through
`get_or_default` and resurrect the session as an empty entry, so
`report_for_session` would return `Ok(zeros)` instead of the required
`Err(SessionNotFound)`, contradicting Story 4.2.1.

**Fix as specified**: per B3's analysis above, `record_actual_usage`'s normal
path now uses the non-creating `get` (Story 1.3.2 item 5, line 305: "backs
`record_actual_usage`/`record_request_failed`'s normal path"); the only
creation branch is the narrowly-scoped adverse-ordering one, which the plan
itself distinguishes explicitly from genuine eviction: "it still never
resurrects a *session* that was never seen at all... since `record_actual_usage`
is only ever called for a session that reached a real provider round trip"
(line 303). The missing test the original review asked for now exists twice,
on both sides of the store/tracker boundary:

- Task 1.3.2g (line 339–341): `record_pending` → force-evict via
  `cache.invalidate` → `record_actual_usage` → assert
  `Err(CostTrackerError::SessionNotFound)`, matching the AC on line 314.
- Task 4.2.1a–c (lines 680–687): a store-level TTL-eviction test on the
  *read* path (`report_for_session` after real TTL expiry, not just
  `cache.invalidate`), explicitly distinguished in the plan's own text (line
  677) from Task 1.3.2g's write-path test as "not a duplicate... separate
  acceptance criteria on separate call paths."

Both tests assert the required `Err(SessionNotFound)`, not `Ok(zeros)`. This
directly closes the contradiction with Story 4.2.1's (now Epic 4.2's)
guarantee.

---

## B5 — RESOLVED

**Original defect**: `PricingTable` was specified per-million in the glossary
but the LiteLLM JSON example and store logic implied per-token — a
1,000,000× error risk — with no golden-value dollar test anywhere.

**Fix as specified**: `ModelPrice` is renamed to carry the unit in the field
name — `ModelPrice { input_usd_per_token: f64, output_usd_per_token: f64 }`
(Domain Glossary, line 66; Task 1.1.1d, line 177: "parameter is named and
typed as a per-token rate, not `price_per_million`"). The vendored JSON keeps
LiteLLM's own per-token key names (`input_cost_per_token`,
`output_cost_per_token` — Task 1.4.1a/AC on line 385), so there is no
transform step where a unit bug could be introduced, matching the original
review's own recommended remediation.

**Golden-value test, checked for real numbers (not hidden in a different
task)**: Story 1.3.3's Acceptance Criteria (line 359) states: "Given
`ModelPrice { input_usd_per_token: 0.000003, output_usd_per_token: ... }` and a
`Reconciled` record with `actual_tokens = 8000` (input), When the record is
priced at write time, Then `cost.0 == 0.024` exactly (`8000.0 * 0.000003`)" —
this is a concrete dollar-amount assertion pinned against a real vendored-style
value, and Task 1.3.3c ("Unit tests for the four Given-When-Then scenarios
above") is the task that implements it, directly adjacent to the spec — not
buried elsewhere. ADR-013 was independently checked and states the same fixed
unit and the same `0.000003 × 8000 → cost` golden-value framing (ADR-013 lines
20–27), so the ADR and plan.md are consistent with each other post-fix,
closing the original "two documents disagree" half of B5 as well.

---

## B6 — RESOLVED

**Original defect**: `AnthropicCountTokensEstimator` was unimplementable
(no auth/version headers), contradicted ADR-012's "synchronous, no network"
decision, and had no rate-limit/concurrency budget.

**Auth/headers**: Task 1.2.2a (line 237–239) specifies the estimator "Holds a
`reqwest::Client`, base URL..., and a credential resolver reused from
`src/providers/anthropic.rs` (do not duplicate secret-lookup logic — extract/
expose the existing function if it's currently private)" and "sets `x-api-key`
+ `anthropic-version` headers on every request." Story 1.2.2's AC (line 226)
requires both headers to be present and non-empty, verified against a mock
server. This directly closes the "no credentials" defect and correctly points
at the existing `src/providers/anthropic.rs` mechanism rather than inventing a
second one.

**ADR-012 itself amended, checked directly (not just plan.md)**: ADR-012's
"Amendment (2026-08-15, repair iteration 1)" section, item 2 (lines 109–119),
explicitly states: "This ADR originally said `post_compact` writes
`counterfactual` 'synchronously (no network — tiktoken-rs only)'. That's wrong
for the Anthropic estimator path, which calls the real `POST
/v1/messages/count_tokens` API... The corrected model is the two-step write
described above: the row insert is synchronous and network-free; filling in
`counterfactual` is not..." This is a genuine amendment to the ADR document
itself (not merely plan.md asserting the ADR is fine) — I read ADR-012 in
full and confirmed the "Decision" section's own prose (lines 30–34) now
matches this framing ("a same-estimator counterfactual token count is then
filled in via `CostTracker::record_counterfactual`... called from a spawned
task so the (possibly network-bound, see Amendment below) estimator never
blocks `apply()`"). The two documents (ADR-012, plan.md) no longer contradict
each other.

**Rate-limit/concurrency budget**: Task 1.2.2b (line 242) adds `BoundedEstimator<E>`
wrapping any `TokenEstimator` with a `tokio::sync::Semaphore`-based concurrency
cap, and specifies 429 → `Err(EstimatorError::RateLimited)` with no internal
retry. Story 1.2.2's AC (lines 228–231) has an explicit concurrency
high-water-mark test and a 429-no-retry test asserting exactly one attempt.
`CostTracker`'s field is `anthropic: BoundedEstimator<AnthropicCountTokensEstimator>`
(Task 1.3.2a, line 318), so the bound is applied at the point of use, not just
defined and left unused.

---

## B7 — RESOLVED

**Original defect**: the actual-usage reconciliation half of Epic 2.2 was
dead code (`translate_anthropic_to_openai` has zero callers, VERIFIED again
directly via `grep -rn "translate_anthropic_to_openai" src` → only the
definition at `src/providers/mod.rs:243`, no other call site), no `RequestId`
threaded anywhere real, and retry double-counting via `Router::dispatch`'s
retry loop was entirely unaddressed.

**Fix as specified**: Story 2.2.1 (lines 481–491) now states the
unreachability plainly as an explicit acceptance criterion ("Explicit
reachability statement (repair iteration 1)": "`translate_anthropic_to_openai`
has no production caller in this codebase as of this plan... The wrapper
built in this story is unit-tested directly, not verified end-to-end through a
live dispatch path, because no such path exists yet"). This is the honest
framing the original review asked for (remediation (i)): it doesn't pretend
the call site is live, and it doesn't silently drop the wrapper either — it
keeps the parsing/reporting logic unit-tested where it already lives, which is
a defensible, explicitly-scoped choice rather than a hidden gap.

**Idempotency (retry double-counting)**: Story 1.3.2 item 6 (line 306)
specifies `record_actual_usage` is "idempotent per `request_id`: calling it
twice for the same `request_id`... replaces the stored `actual_tokens`/`cost`,
never adds to `totals_by_tier` a second time" — this is remediation (ii) from
the original review, applied at the one shared implementation (`record_actual_usage`
itself), so it automatically covers `Router::dispatch`'s retry loop whenever
that call site is eventually wired up, not just the currently-dead wrapper.
Story 2.2.1's own AC (line 489) adds an idempotency test through the wrapper
specifically, and Story 1.3.2's AC (line 312) tests it at the tracker level
directly.

**Pending-row age-out (orphan bound)**: Story 2.2.2 (lines 508–515) adds a
read-time `PENDING_MAX_AGE` sweep (default 10 minutes) that abandons stale
`Pending` rows lazily on the next read/write touching that session — this is
remediation (iii) from the original review, and is explicitly justified as
necessary "since no live provider-dispatch error path exists in this codebase
yet." Task 2.2.2d adds a `// TODO(cost-metrics):` marker at the future
real error-handling seam for whoever wires up live dispatch later.

This resolves all three sub-concerns the original blocker raised, and does so
by being honest about what's still not wired to live traffic rather than by
overclaiming reachability.

---

## Overall Verdict

All 7 previously-BLOCKED items are RESOLVED as specified, verified against the
actual task/story text and the actual amended ADR documents (not merely the
plan's own rework-notes description of itself), and cross-checked against
current `src/` where the claims were checkable (e.g. `CompactHooks`
implementors are still test-only, `async-trait`/`moka` versions support the
APIs the plan now calls for, `translate_anthropic_to_openai` still has zero
callers and the plan is honest about that fact rather than papering over it).

No new blocker was introduced by the repair. Verdict is **CONCERNS**, not
CLEAN, because this re-review's scope was limited to the 7 original blockers
per the review brief — the original Concerns/Minors (C1–C12, M1–M8) were
explicitly out of scope here and have not been re-verified against the
rewritten plan; a full pass against those before implementation is still
advisable, particularly C2 (output-token pricing — note the fix for B5
appears to have also resolved this: Story 1.3.3's AC at line 361 now states
`actual_tokens` includes both `usage.input_tokens` and `usage.output_tokens`,
and Story 2.2.1's AC line 487 does the same — but this was not this review's
assigned scope to confirm exhaustively) and C10 (bind address — also appears
addressed at Task 2.3.1b/Story 2.3.1's AC, same caveat).
