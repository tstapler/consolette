# Implementation Plan: compaction-cost-metrics

**Feature**: Per-session actual-vs-counterfactual token/cost accounting for `SessionCompactionPipeline`, exposed via a new CLI subcommand and a new minimal HTTP JSON endpoint, so the operator can tune `TierThresholds` with real evidence.
**Date**: 2026-08-15
**Status**: Ready for implementation (repair iteration 1)
**ADRs**: [ADR-012](../decisions/ADR-012-cost-metrics-reconciliation-and-server-bootstrap.md) (reconciliation model + minimal HTTP server bootstrap; amended — see Amendment section), [ADR-013](../decisions/ADR-013-pricing-table-source.md) (pricing table source)

## Known Limitation: no live traffic feeds the pipeline

`SessionCompactionPipeline::apply()` is called only from `#[cfg(test)]` code and from `serve-cost`'s Epic 4.1 synthetic test harness — nothing in this plan wires `providers`/`routing` into a live proxy that feeds real request messages into `apply()` (that wiring is explicitly out of scope, matching requirements.md's Out of Scope section). Consequently, this release's success metric ("actual vs. counterfactual tokens/cost" per requirements.md's Success Metrics) is validated **only** via the synthetic Full-vs-Off comparison in Epic 4.1. In production, an operator running `consolette serve-cost` will not see `cost-report`/`/v1/cost/{session_key}` accumulate real data until a separate, out-of-scope effort wires real proxy traffic into `SessionCompactionPipeline::apply()`. This is a scope boundary, not a defect to fix here — see requirements.md's Open Questions for the tracked follow-up.

## Rework Notes (repair iteration 1)

This plan failed both `architecture-review.md` and `adversarial-review.md` on first pass. All 12 deduped blockers are addressed below; re-reviewers should check each item against the referenced section, not against this summary.

| # | Blocker | Fix location |
|---|---------|--------------|
| 1 | No live process wires the tracker into anything the CLI/HTTP can read (arch B1, adv B1) | `serve-cost` now owns `SessionCompactionPipeline` + `CostTrackingHook` + the HTTP route; `cost-report` CLI is now a `reqwest` HTTP client of that server. Epics 2.3, 3.1, 3.2 rewritten. ADR-012 amended below. |
| 2 | `post_compact_with_messages` has no `SessionKey` (arch B2) | `CompactHooks::post_compact` signature itself changed to take `&PostCompactContext`; no second method. Epic 2.1 rewritten. Step 0.5 item 2 rationale corrected. |
| 3 | Fire-and-forget spawn + no-op-on-missing-row loses actuals (arch B3, adv B2) | Split write: synchronous `Pending` row insert in the hook; async `record_counterfactual` fills the estimate later; `record_actual_usage` is now an upsert. Epic 2.1/1.3 rewritten; adverse-ordering test added. |
| 4 | `counterfactual − actual` is not a valid subtraction (arch B4) | `tokens_saved` now compares two same-estimator estimates (pre- vs. post-compaction `messages`); `actual_tokens` (from `usage.*`) reported separately, used only for the dollar figure. Epic 1.3.3/2.1 rewritten. |
| 5 | Cumulative aggregation unsound: pending/abandoned inflate savings; cost fields never written; per-token/per-million unit mismatch (arch B5, adv B5) | `TierTotals` now folds in reconciled rows only, prices at write time under the same lock, `report_for_session` is a pure read. `ModelPrice` renamed to carry `_usd_per_token` unit in the field name; golden-value test added. |
| 6 | `get_or_default` copied verbatim is a TOCTOU race (adv B3) | Store uses `cache.get_with(...)` atomic initialization. Epic 1.3.1 rewritten. |
| 7 | `record_actual_usage` resurrects evicted sessions (adv B4) | Non-creating `get()` added; only `record_pending` (plus the adverse-ordering branch of `record_actual_usage`) may create a row. New eviction tests in Epic 1.3.2 (write path) and Epic 4.2 (read path). |
| 8 | `AnthropicCountTokensEstimator` unimplementable, contradicts ADR-012 (adv B6) | Auth/version headers plumbed via `providers::anthropic`; ADR-012 amended to state the counterfactual is a network call; bounded concurrency (`Semaphore`) added; 429 → typed error, no retry. |
| 9 | Epic 2.2 depends on a zero-caller function; retry double-counting unspecified (adv B7) | Story 2.2.1 now states the call site is unreachable in this codebase (unit-test-only) and drops the dead wrapper; `record_actual_usage` specified idempotent per `request_id`; pending-row age-out sweep added. |
| 10 | `CompactionTier` isn't `Hash`; `HashMap` won't compile (arch nitpick, adv M1) | Switched to a 4-element array indexed by tier (both reviews' preferred fix). |
| 11 | No HTTP-mocking dev-dependency; `arc_swap` undeclared (arch C11, adv C12) | Hand-rolled axum test server (no `wiremock`); `tokio::sync::watch` replaces `arc_swap` in the stretch story. |
| 12 | `CompactionReport::default()` would stop equalling itself (arch C10, adv M4) | `RequestId::default()` is `Uuid::nil()`; `apply()` generates the real id explicitly. |

Also applied (cheap, verified concerns/nitpicks): `tests/toml_parity.rs` convention hedge removed; `async-trait` hedge removed (already a direct dep); "As a `CompactionTracker` (sic...)" typo fixed; server binds `127.0.0.1` by default with `--port` read from existing config; route moved to `/v1/cost/{session_key}`; output tokens now extracted and priced alongside input tokens.

## ADR-012 Amendment (2026-08-15, repair iteration 1)

Two corrections to ADR-012's "Decision" section, both required by review blockers:

1. **Ownership**: `consolette serve-cost` is now the *only* process that constructs a `SessionCompactionPipeline`. It registers `CostTrackingHook` on that pipeline and hosts the `GET /v1/cost/{session_key}` route against the same `Arc<CostTracker>`. `consolette cost-report <key>` no longer constructs its own `CostTracker` — it is a `reqwest` HTTP client against `serve-cost`'s route, so the "two surfaces agree" claim is now structurally true (both paths terminate in the same in-process `report_for_session` call) rather than true only inside a hand-populated test.
2. **Counterfactual is a network call, not "no network"**: ADR-012 previously stated `post_compact` writes `counterfactual` "synchronously (no network — tiktoken-rs only)". That was wrong for the Anthropic path, which per ADR (Open Questions, requirements.md) uses the real `POST /v1/messages/count_tokens` API. The corrected model (see Epic 2.1): the hook writes a `Pending` row **synchronously** with `counterfactual: None`; the estimator (tiktoken, synchronous/local, or Anthropic `count_tokens`, async/network) fills `counterfactual` via a separate `record_counterfactual` call, spawned off the hot path. `apply()` itself never awaits network I/O — the NFR ("must not add per-request latency") is preserved by this split, not by pretending the Anthropic call doesn't exist.

Alternatives-rejected section is otherwise unchanged; the "restructure `post_compact` to fire after the response" rejection still stands (see Step 0.5 item 2 below for the corrected rationale for *why* it's rejected).

---

## Step 0.5 — Alternatives considered

1. **Extend global `ProxyMetrics` (`src/metrics/counters.rs`) to be session-keyed.** Strength: zero new module, reuses an already-wired `/metrics`-shaped JSON builder. Weakness: `ProxyMetrics` is a flat `AtomicU64` struct with no keying concept at all — retrofitting a `HashMap`/`moka` key onto every counter is a larger, riskier refactor of code with existing callers than adding a parallel structure. **Rejected** (matches requirements.md's own "Alternatives Considered").
2. **Restructure `CompactHooks::post_compact` to fire after the provider response, so one function sees both counterfactual and actual.** Strength: would eliminate the need for a `RequestId` correlation mechanism entirely — one call site, one write. Weakness: **corrected rationale (repair iteration 1)** — the previous version of this plan claimed this would break `PlanReinjection`/`SkillReinjection` state timing; that claim is VERIFIED FALSE. `grep -rn "impl CompactHooks for" src` shows all four implementors live in `#[cfg(test)]` modules, and reinjection (`reinject_if_missing`) is called inline inside `apply()` (`src/session_compaction/mod.rs`), not through `CompactHooks` at all — so there is no production consumer whose timing this would disturb. The real reason to reject this alternative is simpler and still holds: firing `post_compact` after the provider response would couple `apply()`'s hot, synchronous compaction path to network latency (the response may never arrive, may be retried, or may take seconds), violating the NFR that cost accounting is bookkeeping, not a hot computation. It would also require `apply()` itself to await the provider round trip, which is a much larger structural change than adding one parameter to the existing seam. **Rejected**, on this corrected basis, recorded in [ADR-012](../decisions/ADR-012-cost-metrics-reconciliation-and-server-bootstrap.md).
3. **Two independent writes into one `(SessionKey, RequestId)`-keyed record inside a new `CostTracker`, reconciled lazily on read.** Strength: keeps `apply()`'s existing synchronous, network-free contract untouched; naturally produces the `exact`/`estimated`/`pending` provenance states the requirements already want, as a side effect of the data model rather than a separate mechanism. Weakness: requires threading a new `RequestId` through two previously-disjoint code paths (`session_compaction` and `providers`), which is genuine, real plumbing work with no prior art in this codebase. **Chosen** — the plumbing cost is bounded and one-time; it is the only option that doesn't compromise an existing tested contract.

This is a **system design** problem (new bounded-context module + two new read surfaces + cross-module plumbing), not a simple CRUD or scripting task, per requirements.md's own "Complexity: 3 — system design" classification.

---

## Domain Glossary

| Term | Definition | Notes |
|------|-----------|-------|
| `RequestId` | Unique identifier for one `SessionCompactionPipeline::apply()` invocation / one upstream provider round trip. | Newtype wrapping `uuid::Uuid` (already a dependency, used in `main.rs`'s `compact_session_command`). **`Default` is `Uuid::nil()`** (not `Uuid::new_v4()`), so `CompactionReport::default() == CompactionReport::default()` stays true; the real id is generated explicitly with `RequestId::new()` at the one call site inside `apply()`, never via `Default` (repair iteration 1, was arch C10/adv M4). The one new correlation primitive this feature introduces (ADR-012). |
| `TokenCount` | A count of LLM tokens, always paired with its `TokenSource`. | Newtype wrapping `u64`; never a bare `u64` in `cost_metrics` public types, so call sites can't accidentally compare/sum an exact count against an estimated one without going through explicit conversion. |
| `TokenSource` | Sum type: `Exact` (from real provider `usage.*`) or `Estimated { via: EstimatorKind }`. | Sealed enum; every `TokenCount` in a report carries one. Satisfies requirements.md's "exact vs. estimated" observability requirement structurally, not by convention. |
| `EstimatorKind` | Sum type: `TiktokenCl100k`, `TiktokenO200k`, or `AnthropicCountTokensApi`. | Records *which* estimator produced an `Estimated` `TokenCount`, so a UI/log can distinguish "tiktoken approximation" from "Anthropic's own exact pre-flight count" (build-vs-buy.md: the Anthropic counterfactual is not a tiktoken approximation — it's a real `count_tokens` API call, more accurate than `usage` would suggest calling it "estimated" flatly). |
| `CostAmountUsd` | A dollar amount, or `None` if the model's price is unknown. | Newtype wrapping `Option<f64>` semantics via `Option<CostAmountUsd(f64)>` at call sites — never defaults to `0.0` for a missing price (research/features.md pitfall 7). |
| `ReconciliationStatus` | Sum type: `Pending` (counterfactual recorded, actual not yet arrived), `Reconciled` (both sides recorded), `Abandoned` (request failed/timed out, counterfactual will never be joined). | Per-`(SessionKey, RequestId)` row state. Distinguishes "no data yet" from "compaction ran but saved nothing" per ux.md's error-states table. |
| `CostRecord` | One row: `(request_id: RequestId, tier: Option<CompactionTier>, counterfactual_est: Option<TokenCount>, compacted_est: Option<TokenCount>, actual_tokens: Option<TokenCount>, model: Option<String>, status: ReconciliationStatus, recorded_at: DateTime<Utc>, cost: Option<CostAmountUsd>)`. | Stored in a bounded per-session ring (capacity 200, ADR-012). **repair iteration 1**: split the old single `counterfactual`/`actual` pair into `counterfactual_est` (pre-compaction messages, estimator) and `compacted_est` (post-compaction `apply()` output, *same* estimator) so `tokens_saved` is a valid same-units subtraction; `actual_tokens` (from real `usage.*`, `TokenSource::Exact`) is kept separately for the dollar figure and as a sanity check, never as the minuend (was arch B4). `cost` is priced once, at write time (was arch B5.2). |
| `SessionCostState` | Per-`SessionKey` aggregate: a bounded `VecDeque<CostRecord>` plus always-correct running totals, kept **only over `Reconciled` rows** (folded in exactly once, at the moment a row's status transitions to `Reconciled` — `Pending`/`Abandoned` rows contribute nothing to the totals, so they can never inflate "savings"; was arch B5.1/adv B4-variant), broken down **by `CompactionTier`** using a **4-element array indexed by tier** (`[TierTotals; 4]`, not a `HashMap<CompactionTier, TierTotals>` — `CompactionTier` isn't `Hash` and a fixed array makes "every tier present" a total function with no `Option` on lookup; was arch nitpick/adv M1), so ring eviction of old rows never corrupts the cumulative numbers. | Lives inside `Arc<RwLock<SessionCostState>>`, mirroring `SessionState`'s shape exactly. |
| `TierTotals` | Per-tier reconciled aggregate: `{ counterfactual_tokens: u64, compacted_tokens: u64, actual_tokens: u64, cost_counterfactual: Option<CostAmountUsd>, cost_actual: Option<CostAmountUsd> }`. | Folded in under the same write lock that flips a row to `Reconciled`, so `report_for_session` never touches `PricingTable` — it is a pure read of already-priced totals (was arch B5.2). |
| `CostTracker` | The public API of `src/cost_metrics`: owns the `moka::future::Cache<SessionKey, Arc<RwLock<SessionCostState>>>`, the `PricingTable`, and the estimator adapters. Exposes `record_pending` (was `record_estimate` — renamed to reflect it writes the `Pending` row, not the estimate itself), `record_counterfactual`, `record_actual_usage`, `record_request_failed`, `get` (non-creating), `report_for_session`. | Held as `Arc<CostTracker>`, constructed once inside the `serve-cost` process (repair iteration 1, see Blocker 1 note below), shared by the `CostTrackingHook`, the provider-response call site, and the axum `State`. The CLI (`cost-report`) no longer holds one — it is an HTTP client of `serve-cost` instead. |
| `CostTrackingHook` | A `CompactHooks` implementor whose `post_compact(&self, ctx: &PostCompactContext<'_>)` synchronously calls `CostTracker::record_pending` (row insert, `counterfactual_est: None`), then spawns a task that calls the appropriate `TokenEstimator` on both `ctx.pre_compaction_messages` and `ctx.report`'s post-compaction output and writes both results back via `record_counterfactual`. | The only new code that touches the `CompactHooks` seam; registered via `SessionCompactionPipeline::register_hook`, never edits `apply()`'s body directly. Requires `CompactHooks::post_compact`'s signature itself to change (see `PostCompactContext` below) — was arch B2. |
| `PostCompactContext<'a>` | New struct passed to `CompactHooks::post_compact` in place of the current `(&SessionState, &CompactionReport)` pair: `{ session_key: &'a SessionKey, session: &'a SessionState, pre_compaction_messages: &'a Value, report: &'a CompactionReport }`. | `apply()` has all four values in scope already (confirmed in `src/session_compaction/mod.rs`); no second hook method, no parallel signature — `post_compact`'s one signature changes. Was arch B2; the previous draft's `post_compact_with_messages` two-method design is dropped. |
| `PricingTable` | Model name → `ModelPrice { input_usd_per_token: f64, output_usd_per_token: f64 }` lookup, merged from the vendored LiteLLM snapshot and optional user config overrides. | See ADR-013 (amended). **repair iteration 1**: field names carry the per-token unit explicitly — LiteLLM's source JSON is per-token, not per-million; the previous draft's `price_per_million` naming invited a 1,000,000x unit-conversion bug (was arch B5.2/adv B5). `pricing_source: PricingSource` (`Static`/`Live`) is attached to every `CostAmountUsd` computed from it. **Pre-mortem P1 #2 (Task 1.4.1e)**: `price_for` does a bare string match against LiteLLM's keys; it must be verified against the real, possibly-aliased/Bedrock-qualified model-name strings `src/providers/anthropic.rs`'s `normalize_model_name` and `src/providers/bedrock.rs`'s alias tables actually produce, with a normalization/alias layer added if a mismatch is found — not assumed to line up because fixture-string tests pass. |
| `TokenEstimator` | Trait: `async fn estimate(&self, model: &str, messages: &Value) -> Result<TokenCount, EstimatorError>` (`Estimated` source). | Two implementors: `TiktokenEstimator` (OpenAI-routed models, local/free, synchronous under the hood) and `AnthropicCountTokensEstimator` (calls `POST /v1/messages/count_tokens`, real network call — see Epic 1.2.2 and ADR-012 Amendment — bounded by a `Semaphore`, `EstimatorError::RateLimited` on 429 with no retry). `CostTracker` picks the implementor per request's model/provider. Returns `Result`, not a bare `TokenCount`, so a failed Anthropic call can mark the row `Abandoned` instead of silently writing a wrong number (repair iteration 1; was adv B6). |
| `CostReport` | `serde::Serialize` struct returned by `report_for_session` — the one shared shape both the CLI (`serde_json::to_string_pretty`, now rendering the HTTP response body rather than an in-process call) and the axum handler (`Json(report)`) render, so the two surfaces structurally cannot disagree (repair iteration 1: this is now true because both are literally the same process/route, not merely the same function signature — see Blocker 1 / Epic 2.3/3.1). | Contains per-tier and cumulative breakdowns, `ReconciliationStatus` counts, `pricing_source`, and per-figure provenance (`actual_source`/`counterfactual_source: Option<TokenSource>`, Task 1.3.3a). |
| `SessionNotFound` | A `CostTracker::report_for_session` outcome distinct from "session found, zero savings." | Maps to CLI non-zero exit / stderr message (surfaced from the HTTP client on a `404`) and API `404 {"error":"session_not_found"}` per ux.md's error-states table — never collapsed into a zeroed report. |

---

## Pattern Decisions

| Component | Pattern Chosen | Source | Alternative Rejected | Reason |
|-----------|---------------|--------|---------------------|--------|
| `CostTracker` | Service Layer (PoEAA) — one coarse-grained façade (`record_pending`/`record_counterfactual`/`record_actual_usage`/`report_for_session`) over the cache + pricing + estimators | Fowler | Transaction Script per call site | Multiple collaborators (cache, pricing table, two estimators) and cross-cutting invariants (never double-count, never lose an increment) need a single owning object, not ad hoc functions scattered across `session_compaction`/`providers`. |
| `SessionCostState` store | Repository-like cache-per-key (mirrors `SessionStateStore`'s shape, **not** its `get`-then-`insert` initialization) | PoEAA / existing codebase precedent | (a) A relational/SQL-backed repository; (b) copying `SessionStateStore::get_or_default`'s `get`-then-`insert` pattern verbatim | (a) Constraint explicitly forbids a new persistent datastore; `moka::future::Cache<SessionKey, Arc<RwLock<T>>>` is already the proven in-process pattern for this exact shape. (b) **repair iteration 1**: `get_or_default`'s two-step `get` then `insert` is a TOCTOU race under concurrent first-access to the same fresh key — two callers can both miss the `get`, both construct a fresh entry, and one write clobbers the other (was adv B3, confirmed against `src/session_compaction/session_state.rs`). This store instead uses `moka`'s atomic `cache.get_with(key.clone(), async { ... }).await`, which guarantees the initializer runs exactly once per key even under concurrent callers. Additionally, only the code path that inserts the first `Pending` row (`CostTrackingHook::post_compact`) may create an entry; `record_actual_usage`/`record_request_failed` use a non-creating `get()` and return `SessionNotFound` if the key is absent — including after eviction — rather than silently resurrecting an empty entry (was adv B4). |
| Cost accounting integration point | Strategy/Observer-ish plugin via existing `CompactHooks` trait | GoF (Observer) | Editing `apply()`'s body directly | `CompactHooks` is exactly the extension seam architecture.md identifies for this; editing `apply()` risks the existing tested compaction behavior and duplicates a pattern that already exists. |
| Token estimation per provider | Strategy (GoF) — `TokenEstimator` trait, two implementors selected by model/provider | GoF | One estimator function with an `if provider == "anthropic"` branch | Two genuinely different algorithms (local BPE vs. remote exact API) with different latency/cost/reliability profiles; Strategy keeps them independently testable and swappable (e.g. mocking the Anthropic API call in tests) without branching logic scattered through `CostTracker`. |
| `RequestId`, `TokenCount`, `CostAmountUsd` | Newtypes (type-driven design) | type-driven-design | Raw `Uuid`/`u64`/`f64`/`Option<f64>` | Prevents mixing an actual `TokenCount` with an estimated one at the type level, and prevents a missing price silently behaving like a raw `f64` `0.0` (research/features.md pitfall 7). |
| `TokenSource`, `ReconciliationStatus`, `EstimatorKind` | Sum types / exhaustive enums (type-driven design) | type-driven-design | `bool is_estimated` flags / string tags (`"exact"`/`"estimated"`) | Exhaustive `match` in report-rendering code forces every new state (e.g. adding a third estimator) to be handled everywhere it matters, at compile time, rather than a string comparison that can silently fall through. |
| Pricing table lookup + optional live refresh | Adapter (GoF) over "LiteLLM JSON fetch" behind a `PricingSource` trait, static table as the default `Adapter` impl | GoF | Direct `reqwest` calls inline in `CostTracker` | Isolates the third-party (LiteLLM GitHub JSON) interface so the background-refresh task and its failure/timeout handling live in one small module (`pricing.rs`), not scattered through the tracker; matches the existing `Provider` trait's own use of Adapter for upstream APIs (`src/providers/mod.rs`). |
| CLI subcommand / HTTP handler | `serve-cost` owns the one in-process `report_for_session` caller (the axum handler); `cost-report` is a thin `reqwest` HTTP client of that route, not a second in-process caller | PoEAA (Remote Facade) | (a) Duplicating aggregation logic in both `main.rs` and the axum handler; (b) (repair iteration 1, rejected) giving `cost-report` its own in-process `CostTracker` that happens to share the same construction code as `serve-cost` | Requirements' explicit success metric: "the two surfaces agree (same underlying aggregation)". Option (b) was the plan's original design and is exactly what arch B1/adv B1 flagged: nothing then wires `SessionCompactionPipeline` + `CostTrackingHook` into any live process the CLI or HTTP surface can observe, so "the two surfaces agree" would hold only in a test that constructs both by hand, never against real compaction activity. Making `cost-report` an HTTP client of `serve-cost` means there is exactly one `CostTracker` instance in existence at runtime, and both surfaces are provably the same read. |

---

## Migration Plan

*(Omitted — no schema or persisted-data changes; `src/cost_metrics/pricing_default.json` is a new static fixture checked into the repo, not a migration.)*

## Observability Plan

- **Logs**: `tracing::warn!` once per pricing-table fallback event (live refresh failed/timed out, falling back to static table) — `src/cost_metrics/pricing.rs`. `tracing::debug!` at `CostTrackingHook::post_compact` entry/exit (session key, tier, counterfactual token count). `tracing::error!` (with `request_id`, `session_key`, error context) on every `record_request_failed` call site.
- **Metrics**: `cost_metrics_pricing_fallback_total` (counter, incremented on every static-table fallback) — reuses the `ProxyMetrics`-style `AtomicU64` shape but lives in `src/cost_metrics`, kept structurally separate from `ProxyMetrics` per pitfalls.md's "global counters cross-contaminate session-scoped ones" guidance. `cost_metrics_reconciliation_pending_total` (gauge-ish counter, current count of `Pending` rows across all sessions) surfaced in `CostReport` for spot-checking (ux.md job 2).
- **Alerts**: no new alerts required — this is internal/operator-pull tooling (requirements.md Risk Control), not a paged surface.

## Risk Control

- **Feature flag**: not gated — additive, read-only instrumentation per requirements.md's Risk Control section; the new `CostTrackingHook` is registered explicitly by the caller that assembles `SessionCompactionPipeline`, so simply not registering it is the "off switch" during rollout if ever needed.
- **Rollback procedure**: standard revert via PR close + revert commit — no schema, no external side effects beyond the vendored JSON fixture and (optional) background refresh task, both inert if the module is reverted.
- **Staged rollout**: full rollout on merge — single-operator local tool, no user cohort to stage against.

## Unresolved Questions

- [x] **Resolved (repair iteration 1)**: bind address/port for the minimal HTTP server bootstrap — was mis-referenced as "blocks Story 2.1.2" (no such story exists; the actual blocked story is 2.3.1). Resolved: default bind is `127.0.0.1:8787` (loopback-only, matching this feature's operator-only/internal security classification), overridable via `--port` on `consolette serve-cost`, which itself falls back to a `[cost_metrics] port` key in the existing config file rather than being a config-blind flag. See Epic 2.3.
- [ ] Whether `tiktoken-rs` should be bumped past `0.5.9` before first real use (research/stack.md flags MSRV 1.85+ on newer releases) — blocks Task 1.2.1a — owner: implementer, verify `rustc --version` against MSRV before bumping; pin at `0.5` and defer the bump to a follow-up if MSRV doesn't clear.

## Dependency Visualization

```
Phase 1: cost_metrics core (types, pricing, estimators, tracker)
  Epic 1.1 Domain types ──────────────┐
  Epic 1.2 Estimators (tiktoken +     │
           Anthropic count_tokens) ───┼──> Epic 1.3 CostTracker + store ──> Epic 1.4 Pricing table (ADR-013)
                                      │                    │
Phase 2: Integration                 │                    │
  Epic 2.1 CompactHooks integration <─┘                    │
  (PostCompactContext signature change, │                  │
   CostTrackingHook, RequestId          <────────────────┘
   plumbing into apply())
        │
        ├──> Epic 2.2 Provider-response integration (record_actual_usage
        │            call site in providers/mod.rs, RequestId threading;
        │            marks translate_anthropic_to_openai call site as
        │            unreachable-today/unit-test-only, repair iter. 1)
        │
        └──> Epic 2.3 Minimal HTTP server bootstrap (ADR-012, amended):
                    `serve-cost` constructs SessionCompactionPipeline +
                    CostTrackingHook + the HTTP route together — the only
                    process that owns a live CostTracker
                    │
Phase 3: Surfaces    │
  Epic 3.1 CLI subcommand (`cost-report`) — reqwest HTTP client of
           `serve-cost`'s route, NOT its own CostTracker (repair iter. 1) <┘
  Epic 3.2 HTTP JSON endpoint /v1/cost/{session_key} (part of Epic 2.3's
           server; reuses Epic 1.3's report_for_session)
        │
Phase 4: Verification
  Epic 4.1 Full-vs-Off synthetic-session end-to-end test (requirements.md success metric)
  Epic 4.2 Concurrency/reconciliation edge-case tests (pitfalls.md failure modes;
           adds: adverse-ordering test, eviction-then-write test, idempotent
           double-record test — repair iteration 1)
```

---

## Phase 1: `cost_metrics` core

### Epic 1.1: Domain types and module skeleton
**Goal**: Stand up `src/cost_metrics/` with all newtypes/sum types from the Domain Glossary, wired into `src/lib.rs`, with no behavior yet.

#### Story 1.1.1: Create the module and its core types
**As a** developer extending this feature, **I want** `src/cost_metrics::types` to define every Domain Glossary type before any logic is written, **so that** the rest of the feature has one consistent vocabulary instead of ad hoc `u64`/`bool`/`String` fields invented per-file.
**Acceptance Criteria**:
- `RequestId`, `TokenCount`, `TokenSource`, `EstimatorKind`, `CostAmountUsd`, `ReconciliationStatus` compile, derive `Debug, Clone, PartialEq, Eq` (`Copy` where field sizes allow), and `Serialize`/`Deserialize` where they appear in `CostReport`.
  - *Given* the `cost_metrics::types` module, *When* `TokenSource::Estimated { via: EstimatorKind::TiktokenCl100k }` is serialized with `serde_json::to_string`, *Then* the output is `{"Estimated":{"via":"TiktokenCl100k"}}` (or an equivalent explicit tagged shape — not silently flattened to a bare string).
**Files**: `src/cost_metrics/mod.rs`, `src/cost_metrics/types.rs`, `src/lib.rs`

##### Task 1.1.1a: Add `pub mod cost_metrics;` and create `types.rs` skeleton (~3 min)
- Add `pub mod cost_metrics;` to `src/lib.rs` (alongside existing `pub mod metrics;`, `pub mod ratelimit;`).
- Create `src/cost_metrics/mod.rs` with `pub mod types;` and a module doc comment stating scope (mirrors `session_compaction/mod.rs`'s doc-comment style).
- Files: `src/lib.rs`, `src/cost_metrics/mod.rs`

##### Task 1.1.1b: Define `RequestId` and `TokenCount` newtypes (~4 min)
- `pub struct RequestId(pub uuid::Uuid);` with `RequestId::new() -> Self` (wraps `Uuid::new_v4()`) and an explicit `impl Default for RequestId { fn default() -> Self { RequestId(Uuid::nil()) } }` — **repair iteration 1**: do not derive `Default` (that would call `Uuid::new_v4()` via `Uuid`'s own default-adjacent convention only if hand-rolled wrong; the point is any `Default` impl here must be `nil()`, not a fresh random id) so that `CompactionReport::default() == CompactionReport::default()` stays true once `CompactionReport` gains a `request_id: RequestId` field (was arch C10/adv M4). Real ids are always constructed via `RequestId::new()`, called exactly once inside `apply()` (Epic 2.1).
- `pub struct TokenCount { pub value: u64, pub source: TokenSource }`.
- Files: `src/cost_metrics/types.rs`

##### Task 1.1.1c: Define `TokenSource`, `EstimatorKind`, `ReconciliationStatus` sum types (~4 min)
- `#[derive(Serialize, Deserialize)] pub enum TokenSource { Exact, Estimated { via: EstimatorKind } }`.
- `pub enum EstimatorKind { TiktokenCl100k, TiktokenO200k, AnthropicCountTokensApi }`.
- `pub enum ReconciliationStatus { Pending, Reconciled, Abandoned }`.
- Files: `src/cost_metrics/types.rs`

##### Task 1.1.1d: Define `CostAmountUsd` and `PricingSource` (~3 min)
- `pub struct CostAmountUsd(pub f64);` plus a free function `fn cost_for_tokens(tokens: &TokenCount, usd_per_token: f64) -> CostAmountUsd` (`tokens.value as f64 * usd_per_token`). **repair iteration 1**: parameter is named and typed as a per-token rate, not `price_per_million` — LiteLLM's source data (`input_cost_per_token` in `model_prices_and_context_window.json`) is already per-token; the previous draft's `price_per_million` name/shape would have required (and risked getting wrong) a x1,000,000 conversion that doesn't belong anywhere in this pipeline (was arch B5.2/adv B5).
- `pub enum PricingSource { Static, Live }`.
- Files: `src/cost_metrics/types.rs`

##### Task 1.1.1e: Unit test serde round-trip for every sum type (~5 min)
- One `#[test]` per enum (`TokenSource`, `EstimatorKind`, `ReconciliationStatus`, `PricingSource`) asserting `serde_json::to_string` then `from_str` round-trips to the original value.
- Files: `src/cost_metrics/types.rs` (`#[cfg(test)] mod tests`)

---

### Epic 1.2: Token estimators
**Goal**: Implement `TokenEstimator` (Strategy) with a tiktoken-backed OpenAI implementor and an Anthropic `count_tokens`-API-backed implementor.

#### Story 1.2.1: `TiktokenEstimator` for OpenAI-routed counterfactuals
**As a** `CostTracker`, **I want** a local, free, synchronous token estimate for OpenAI-model messages, **so that** the OpenAI-side counterfactual never depends on a network call.
**Acceptance Criteria**:
- Given a `serde_json::Value` messages array containing only text-content blocks, calling the estimator with model `"gpt-4o"` returns a `TokenCount` with `source: TokenSource::Estimated { via: EstimatorKind::TiktokenO200k }` whose `value` matches `tiktoken_rs::o200k_base_singleton().encode_with_special_tokens(text).len()` for the concatenated text.
  - *Given* `messages = [{"role":"user","content":"hello world"}]` and `model = "gpt-4o"`, *When* `TiktokenEstimator::estimate` runs, *Then* `value` equals the `o200k_base` token count of `"hello world"` (2, per tiktoken's public playground for that string) and `source.via == EstimatorKind::TiktokenO200k`.
- Non-text blocks (`tool_use`, `tool_result`, image) are handled per an explicit per-block-type strategy, not naive `to_string()`-and-tokenize: `tool_use`/`tool_result` serialize their text payload only (not JSON punctuation) and images are excluded with a per-call `truncated_content: bool` flag set `true`.
  - *Given* `messages` containing one `{"type":"image", "source": {...}}` content block, *When* `TiktokenEstimator::estimate` runs, *Then* the returned estimate excludes the image's byte size from the token count and the accompanying `EstimateMeta.truncated_content` is `true`.
**Files**: `src/cost_metrics/estimator.rs`

##### Task 1.2.1a: Define `TokenEstimator` trait and `EstimateMeta` (~4 min)
- `#[async_trait::async_trait] pub trait TokenEstimator: Send + Sync { async fn estimate(&self, model: &str, messages: &Value) -> Result<(TokenCount, EstimateMeta), EstimatorError>; }`. `async-trait = "0.1"` is already a direct dependency (`Cargo.toml`) — **repair iteration 1**: no conditional-add hedge needed, just use it.
- `pub struct EstimateMeta { pub truncated_content: bool }`.
- Files: `src/cost_metrics/estimator.rs`

##### Task 1.2.1b: Implement text-block extraction shared by both estimators (~5 min)
- `fn extract_estimable_text(messages: &Value) -> (String, bool)` returning concatenated text from `text`/`tool_use.input`/`tool_result.content` blocks and a `saw_non_text_block` flag (images, unknown block types).
- Files: `src/cost_metrics/estimator.rs`

##### Task 1.2.1c: Implement `TiktokenEstimator` (~5 min)
- Pick `cl100k_base_singleton()` or `o200k_base_singleton()` via a small `fn encoding_for_model(model: &str) -> Encoding` (default to `o200k_base` for unknown models per tiktoken-rs's own `get_bpe_from_model` fallback behavior).
- Files: `src/cost_metrics/estimator.rs`

##### Task 1.2.1d: Unit tests for `TiktokenEstimator` (~5 min)
- Text-only message array test (exact count assertion above).
- Image-block test (`truncated_content: true`, image excluded from count).
- `tool_result` block test (text payload counted, JSON structure not counted).
- Files: `src/cost_metrics/estimator.rs` (`#[cfg(test)]`)

#### Story 1.2.2: `AnthropicCountTokensEstimator` for the Anthropic-side counterfactual
**As a** `CostTracker`, **I want** to call Anthropic's real `POST /v1/messages/count_tokens` for the Anthropic counterfactual, **so that** the estimate is exact (per Anthropic's own guidance that tiktoken undercounts Claude tokens 15-30%+), not a tiktoken approximation.

**repair iteration 1 (was adv B6 — this story was previously unimplementable as specified: no auth/version headers, contradicted ADR-012's "no network" claim, no rate-limit/concurrency budget)**: three concrete fixes below plus ADR-012's Amendment section correct the contradiction with ADR-012 by stating plainly that this is a real network call.

**Acceptance Criteria**:
- Given a mocked HTTP response `{"input_tokens": 512}` from `count_tokens`, the estimator returns `TokenCount { value: 512, source: TokenSource::Estimated { via: EstimatorKind::AnthropicCountTokensApi } }` — labeled `Estimated` (it's a hypothetical, never-sent request) but sourced from the exact API, distinct in `EstimatorKind` from a tiktoken guess.
  - *Given* a mock server returning `200 {"input_tokens": 512}` for `POST /v1/messages/count_tokens`, *When* `AnthropicCountTokensEstimator::estimate(model="claude-sonnet-5", messages=...)` runs, *Then* the result is `Ok((TokenCount { value: 512, source: TokenSource::Estimated { via: EstimatorKind::AnthropicCountTokensApi } }, _))`.
- The request carries real auth: `x-api-key` (resolved via the same secret-resolution path `src/providers/anthropic.rs` already uses for live Anthropic calls — do not invent a second credential-lookup mechanism) and `anthropic-version` headers, matching what a real `count_tokens` call requires.
  - *Given* the estimator is constructed with the same credential source as `src/providers/anthropic.rs`, *When* a request is built, *Then* the mock server observes both headers present and non-empty (test asserts on `wiremock`/hand-rolled-server-captured request headers — see Task 1.2.2c on the mocking-crate decision).
- Concurrent estimator calls are bounded — a `tokio::sync::Semaphore` (or `JoinSet` with a fixed cap) limits in-flight `count_tokens` requests to a configured maximum (default small, e.g. 4), so a burst of compactions cannot open unbounded outbound connections to Anthropic.
  - *Given* 20 concurrent `estimate` calls against a mock server that only accepts `N` connections at a time, *When* all 20 are issued via the semaphore-guarded estimator, *Then* no more than `N` are in flight simultaneously (test observes the mock server's concurrent-request high-water mark).
- A `429` response is a typed error with **no retry** — the estimator does not itself retry rate-limited counterfactual requests (retries belong to the caller's decision, if any, not baked into this estimator).
  - *Given* a mock server returning `429`, *When* `estimate` runs, *Then* it returns `Err(EstimatorError::RateLimited)` after exactly one attempt (test asserts the mock server received exactly 1 request).
- On other network failure or non-200/429 response, the estimator returns `Err` (not a silently wrong `0` or a panic) so `CostTracker` can mark that row `Abandoned` rather than recording a corrupting zero.
  - *Given* a mock server returning `500`, *When* `estimate` runs, *Then* it returns `Err(EstimatorError::UpstreamFailure(_))`.
- This estimator is called from `CostTrackingHook::post_compact` (Epic 2.1) via a spawned task, never `.await`ed inline inside `apply()` — see Epic 2.1's synchronous-insert/async-fill split (Blocker 3), which replaces the previous draft's plain fire-and-forget `tokio::spawn` with a design where the *row* always exists synchronously and only the *counterfactual value* arrives asynchronously.
**Files**: `src/cost_metrics/estimator.rs`

##### Task 1.2.2a: Implement `AnthropicCountTokensEstimator` struct + `estimate` (~6 min)
- Holds a `reqwest::Client`, base URL (configurable for tests), and a credential resolver reused from `src/providers/anthropic.rs` (do not duplicate secret-lookup logic — extract/expose the existing function if it's currently private, as a small refactor of that module). Builds the `POST /v1/messages/count_tokens` body from `extract_estimable_text` output reshaped into a minimal Anthropic messages array, and sets `x-api-key` + `anthropic-version` headers on every request.
- Files: `src/cost_metrics/estimator.rs`, `src/providers/anthropic.rs` (expose credential resolver if private)

##### Task 1.2.2b: Define `EstimatorError`, timeout, and bounded concurrency (~5 min)
- `pub enum EstimatorError { UpstreamFailure(String), Timeout, RateLimited }`. Add a request timeout (e.g. 2s) via `reqwest::Client::builder().timeout(...)`. Add `pub struct BoundedEstimator<E: TokenEstimator> { inner: E, permits: Arc<tokio::sync::Semaphore> }` wrapping any `TokenEstimator` with a concurrency cap, acquiring a permit before delegating to `inner.estimate(...)`.
- A `429` maps to `Err(EstimatorError::RateLimited)` with no internal retry loop.
- Files: `src/cost_metrics/estimator.rs`

##### Task 1.2.2c: Decide and implement the HTTP-mocking approach (~5 min)
**repair iteration 1 (was arch C11/adv C12)**: `Cargo.toml`'s `[dev-dependencies]` currently contains only `tempfile = "3"` — no `wiremock`. Decision: hand-roll a small local test server using `axum`/`tokio` (already direct dependencies) rather than add `wiremock` as a new dev-dependency — roughly 20 lines (bind `TcpListener` on `127.0.0.1:0`, one route returning a canned/`Arc<Mutex<VecDeque<Response>>>`-driven body, spawned in the test, torn down on drop), no new supply-chain surface. This decision is load-bearing for every test task in Epics 1.2 and 4 that needs an HTTP double — do not introduce `wiremock` in any of them.
- Files: `src/cost_metrics/test_support.rs` (new, shared hand-rolled mock server helper), referenced from `#[cfg(test)]` modules in `estimator.rs` and elsewhere.

##### Task 1.2.2d: Unit tests with the hand-rolled mock server (~6 min)
- Header-presence test, bounded-concurrency high-water-mark test, `429`-no-retry test, `500`-error test, happy-path test (per the Acceptance Criteria above).
- Files: `src/cost_metrics/estimator.rs` (`#[cfg(test)]`)

---

### Epic 1.3: `CostTracker` and session-scoped store
**Goal**: The `moka`-backed `SessionCostState` store plus `CostTracker`'s public API (`record_pending`, `record_counterfactual`, `record_actual_usage`, `record_request_failed`, `get`, `report_for_session`), all logic unit-testable without any HTTP/CLI surface.

#### Story 1.3.1: `SessionCostState` store — atomic initialization, no TOCTOU race
**As a** `CostTracker`, **I want** a `Cache<SessionKey, Arc<RwLock<SessionCostState>>>` that initializes each key exactly once even under concurrent first access, **so that** two callers racing to create the same fresh session's state can never clobber each other.

**repair iteration 1 (was adv B3)**: the original story instructed copying `SessionStateStore::get_or_default`'s `get`-then-`insert` pattern verbatim. That pattern is confirmed (by reading `src/session_compaction/session_state.rs`) to be a real TOCTOU race: two concurrent callers can both miss the `cache.get(key)` check, both construct a fresh `Arc<RwLock<SessionCostState>>`, and whichever `cache.insert` runs second silently discards the first caller's handle — any mutation made through the discarded `Arc` before the second insert is invisible to everyone who reads the surviving one. This is copied nowhere in the new store.

**Acceptance Criteria**:
- `SessionCostStore::get_or_init` uses `moka`'s atomic `cache.get_with(key, init)` so the initializer runs exactly once per key even when many callers race on the same fresh key, and all callers receive the same `Arc`.
  - *Given* a fresh `SessionCostStore`, *When* 20 concurrent tasks call `get_or_init(&SessionKey::new("s1"))` simultaneously (via `tokio::spawn` + `join_all`), *Then* all 20 returned `Arc`s point at the same underlying `RwLock<SessionCostState>` (verified by pointer equality, `Arc::ptr_eq`), and a counter inside a custom init closure proves the closure ran exactly once.
- Different keys get independent state.
  - *Given* a fresh `SessionCostStore`, *When* `get_or_init` is called for `"s1"` and `"s2"`, *Then* mutating the `"s1"` handle does not affect `"s2"`'s.
- A **non-creating** `get(&SessionKey) -> Option<Arc<RwLock<SessionCostState>>>` is also exposed and used by every call site that must not resurrect an evicted or never-seen session (`record_actual_usage`, `record_request_failed`, `record_counterfactual`, `report_for_session`); only the `CostTrackingHook`'s initial row-insert path may call `get_or_init`.
  - *Given* a `SessionCostStore` that has never seen `SessionKey::new("ghost")`, *When* `get(&SessionKey::new("ghost"))` is called, *Then* it returns `None` (no entry is created as a side effect of calling `get`).
- `max_capacity(1000)`, `time_to_live(Duration::from_hours(1))` — identical to `SessionStateStore` per the NFR "same order of magnitude."
**Files**: `src/cost_metrics/store.rs`

##### Task 1.3.1a: Define `SessionCostState`, `CostRecord`, `TierTotals` (~5 min)
- `SessionCostState { records: VecDeque<CostRecord>, totals_by_tier: [TierTotals; 4] }` — **repair iteration 1 (was arch nitpick/adv M1)**: a 4-element array indexed by `CompactionTier as usize` (or a small `fn tier_index(tier: CompactionTier) -> usize`), not `HashMap<CompactionTier, TierTotals>`. `CompactionTier` does not derive `Hash` today (verified against `src/session_compaction/hooks.rs`'s tier enum), so the `HashMap` form as originally specified would not compile; a fixed-size array is also strictly better here per both reviews — "every tier present" becomes a totality guarantee with no `Option` on lookup, and no `Hash`/`Eq` derive needs to be added to a type this plan doesn't own the primary definition of.
- `CostRecord { request_id: RequestId, tier: Option<CompactionTier>, counterfactual_est: Option<TokenCount>, compacted_est: Option<TokenCount>, actual_tokens: Option<TokenCount>, model: Option<String>, status: ReconciliationStatus, recorded_at: DateTime<Utc>, cost: Option<CostAmountUsd> }` (bounded ring, capacity constant `MAX_RECORDS_PER_SESSION: usize = 200`).
- `TierTotals { counterfactual_tokens: u64, compacted_tokens: u64, actual_tokens: u64, cost_counterfactual: Option<CostAmountUsd>, cost_actual: Option<CostAmountUsd> }` — **repair iteration 1 (was arch B5.1)**: these fields are folded in only when a record's status transitions to `Reconciled` (Task 1.3.2c), never while `Pending` or `Abandoned`. The previous draft declared `cumulative_cost_*` fields but no task ever wrote them (was arch B5.2) and separately let `Pending`/`Abandoned` rows contribute to the running total forever, permanently inflating "savings" for any session with in-flight or failed requests.
- Files: `src/cost_metrics/store.rs`

##### Task 1.3.1b: Implement `SessionCostStore` (moka `get_with` + non-creating `get`) (~6 min)
- `pub async fn get_or_init(&self, key: &SessionKey) -> Arc<RwLock<SessionCostState>> { self.cache.get_with(key.clone(), async { Arc::new(RwLock::new(SessionCostState::default())) }).await }` — **repair iteration 1**: do not copy `SessionStateStore::get_or_default`'s two-step `get`-then-`insert` body; `get_with` is moka's documented atomic-initialization API for exactly this race.
- `pub async fn get(&self, key: &SessionKey) -> Option<Arc<RwLock<SessionCostState>>> { self.cache.get(key).await }`.
- Files: `src/cost_metrics/store.rs`

##### Task 1.3.1c: Implement bounded-ring insert (`push_record`) with eviction (~4 min)
- `fn push_record(&mut self, record: CostRecord)` — if `records.len() >= MAX_RECORDS_PER_SESSION`, `pop_front()` before pushing. This method only ever manages the ring; it does **not** touch `totals_by_tier` (that only happens on the `Reconciled` transition, Task 1.3.2c) — so an evicted `Pending`/`Abandoned` row was never counted in the first place and eviction cannot desync totals from history.
- Files: `src/cost_metrics/store.rs`

##### Task 1.3.1d: Unit tests: atomic init under concurrency, independence, non-creating `get`, capacity eviction (~7 min)
- The 20-concurrent-callers atomic-init test from the Acceptance Criteria above (this is the load-bearing regression test for Blocker 6 — it must be run under `cargo test` with `--test-threads` > 1 or an explicit `tokio::spawn` fan-out to actually exercise the race).
- Independent-keys test.
- `get()` returns `None` for an unseen key and does not create an entry (assert a subsequent `cache.iter().count()` is unchanged).
- Insert `MAX_RECORDS_PER_SESSION + 1` records, assert `records.len() == MAX_RECORDS_PER_SESSION` and `totals_by_tier` reflects only the `Reconciled` subset actually folded in (not "all inserted records," correcting the previous draft's assumption).
- Files: `src/cost_metrics/store.rs` (`#[cfg(test)]`)

#### Story 1.3.2: `CostTracker` write path — synchronous pending insert, async counterfactual fill, upsert actuals, reconciled-only totals
**As a** `CostTracker`, **I want** the pending row to always exist synchronously and every subsequent write to be idempotent/upserting, **so that** concurrent requests to the same session never lose an increment and an adverse arrival order never silently drops an actual (pitfalls.md's TOCTOU race; arch B3/B4/B5; adv B2/B4/B7).

**repair iteration 1** replaces the previous draft's design (fire-and-forget `tokio::spawn` writing both `counterfactual` and treating a missing `Pending` row as a silent no-op) with:

1. `record_pending(session_key, request_id, tier)` — called **synchronously** from `CostTrackingHook::post_compact` (Epic 2.1), inserts a `CostRecord` with `counterfactual_est: None`, `compacted_est: None`, `actual_tokens: None`, `status: Pending`. Uses `get_or_init` (may create the session's entry). This call never touches a network estimator and never races with anything else, so "the row exists" is an invariant from the moment `post_compact` returns, not a hope.
2. `record_counterfactual(session_key, request_id, counterfactual_est, compacted_est)` — called from the spawned estimator task once both the pre- and post-compaction estimates are in; finds the row by `request_id` and fills the two fields. Uses non-creating `get`; if the row is missing (evicted before the estimator returned — bounded by ring capacity, not silent forever-loss) it logs `tracing::warn!` and returns `Err(CostTrackerError::RecordNotFound)` rather than fabricating one.
3. `record_actual_usage(session_key, request_id, actual)` is now an **upsert**: if a matching row exists (the common case — `record_pending` already ran), it sets `actual_tokens` and, if `counterfactual_est`/`compacted_est` are already populated, flips `status` to `Reconciled` and folds the record into `totals_by_tier` **once**, under the same write-lock acquisition. If no matching row exists yet — the adverse ordering where the actual usage arrives before `record_pending`'s row, or after ring eviction — it **creates** a new row with `actual_tokens` populated and `status: Pending` (waiting on the counterfactual side), rather than silently discarding the actual (was arch B3, "guarantees silent loss of actuals"). Uses non-creating `get` first, `get_or_init` only in this specific "create with actual already populated" branch — **not** a general-purpose creating path (this is the one specified exception; it still never resurrects a *session* that was never seen at all, only backfills a *record* whose two writes arrived out of order within a session already known to the store, since `record_actual_usage` is only ever called for a session that reached a real provider round trip).
4. `record_request_failed(session_key, request_id)` flips a `Pending` row's `status` to `Abandoned` (contributes nothing to `totals_by_tier`, and — critically — nothing already folded in is un-folded, since `Abandoned` rows by construction never reached `Reconciled`).
5. A **non-creating `get(session_key) -> Option<Arc<RwLock<SessionCostState>>>`** on the store (Task 1.3.1b) backs `record_actual_usage`/`record_request_failed`'s normal path and `report_for_session`; only `record_pending` and `record_actual_usage`'s adverse-ordering branch may create (was adv B4 — "resurrect an evicted session as an empty entry instead of returning not-found").
6. `record_actual_usage` is **idempotent per `request_id`**: calling it twice for the same `request_id` (e.g. a provider retry loop double-recording) replaces the stored `actual_tokens`/`cost`, never adds to `totals_by_tier` a second time (was adv B7's retry-double-counting concern — see Epic 2.2 for where this matters).

**Acceptance Criteria**:
- *Given* an empty store, *When* `record_pending(session_key, request_id, tier=Full)` is called, *Then* exactly one `CostRecord` with `status: Pending`, `counterfactual_est: None` exists — no network call, no `.await` on anything but the write lock.
- *Given* the `Pending` row from above, *When* `record_counterfactual(session_key, request_id, counterfactual_est=41200, compacted_est=9000)` then `record_actual_usage(session_key, request_id, actual=8600)` are called in that order, *Then* the row's `status == Reconciled` and `totals_by_tier[Full]` reflects exactly one folded record (`counterfactual_tokens: 41200, compacted_tokens: 9000, actual_tokens: 8600`).
- **Adverse-ordering test (new, repair iteration 1)**: *Given* an empty store (no `record_pending` call has happened yet — e.g. the hook's synchronous insert raced with an unusually fast provider response), *When* `record_actual_usage(session_key, request_id, actual=8600)` is called first, *Then* it does not error and does not silently drop the value — a `Pending` row is created with `actual_tokens: Some(8600)`; *When* `record_counterfactual` then arrives for the same `request_id`, *Then* the row reconciles normally and folds into `totals_by_tier` exactly once.
- *Given* a `Reconciled` row already folded into `totals_by_tier`, *When* `record_actual_usage` is called again for the same `request_id` with a different `actual` value (simulating a provider retry), *Then* `totals_by_tier`'s `actual_tokens` reflects only the latest value (replaced, not summed).
- `record_request_failed` on a `Pending` row sets `status: Abandoned` and leaves `totals_by_tier` completely unchanged.
- **`SessionNotFound` on genuine absence (was adv B4, distinct from the adverse-ordering case above)**: *Given* a store where `SessionKey::new("s1")` was created via `record_pending`, then the entry is evicted (force via `cache.invalidate(key)` in the test, simulating TTL expiry), *When* `record_actual_usage(session_key, request_id, actual)` is called, *Then* it returns `Err(CostTrackerError::SessionNotFound)` — it does **not** silently create a fresh empty session and return `Ok(())`.
**Files**: `src/cost_metrics/tracker.rs`

##### Task 1.3.2a: Implement `CostTracker` struct and constructor (~3 min)
- `pub struct CostTracker { store: SessionCostStore, pricing: PricingTable, tiktoken: TiktokenEstimator, anthropic: BoundedEstimator<AnthropicCountTokensEstimator> }`.
- `pub async fn new(pricing: PricingTable) -> Self`.
- Files: `src/cost_metrics/tracker.rs`

##### Task 1.3.2b: Implement `record_pending` (synchronous, creating) (~4 min)
- Files: `src/cost_metrics/tracker.rs`

##### Task 1.3.2c: Implement `record_counterfactual` (non-creating `get`, single write-lock, folds nothing itself) (~5 min)
- Only sets `counterfactual_est`/`compacted_est`; the `Reconciled` transition and totals-folding happen in whichever of `record_counterfactual`/`record_actual_usage` runs *second* (both fields plus `actual_tokens` present is the fold trigger) — implement the fold as one shared private helper called from both write paths so there is exactly one place that appends to `totals_by_tier`.
- Files: `src/cost_metrics/tracker.rs`

##### Task 1.3.2d: Implement `record_actual_usage` (upsert, idempotent-per-`request_id`, adverse-ordering create branch) (~6 min)
- Files: `src/cost_metrics/tracker.rs`

##### Task 1.3.2e: Implement `record_request_failed` (~3 min)
- Files: `src/cost_metrics/tracker.rs`

##### Task 1.3.2f: Concurrency test — interleaved writes on one session (~6 min)
- Spawn N=20 concurrent `record_pending` calls (via `tokio::spawn` + `join_all`) on the same `SessionKey` with distinct `RequestId`s, assert all 20 rows exist and no row's data was clobbered by another (catches the read-modify-write race pitfalls.md warns about).
- Files: `src/cost_metrics/tracker.rs` (`#[cfg(test)]`)

##### Task 1.3.2g: Eviction-then-write regression test (~4 min)
- The `SessionNotFound`-on-genuine-eviction acceptance criterion above, as a standalone test (was adv B4's explicitly requested missing test): `record_pending` → force-evict via `cache.invalidate` → `record_actual_usage` → assert `Err(SessionNotFound)`, not `Ok(())` with a fabricated zeroed session.
- Files: `src/cost_metrics/tracker.rs` (`#[cfg(test)]`)

#### Story 1.3.3: `report_for_session` — the one shared aggregation function, pure read, same-estimator `tokens_saved`
**As a** CLI subcommand and an HTTP handler, **I want** one function returning one `CostReport` type, computed with a same-estimator-on-both-sides `tokens_saved`, **so that** the two surfaces cannot structurally disagree and the "savings" figure is a valid subtraction.

**repair iteration 1 (was arch B4)**: the previous draft defined `tokens_saved = counterfactual - actual`, subtracting a token count produced by an *estimator* (tiktoken or Anthropic `count_tokens`, run before compaction) from a token count reported by the *provider's real `usage.*`* (after compaction, after the model actually ran) — these measure different things (an estimate of the uncompacted input vs. the provider's accounting of the compacted input-plus-whatever-else the provider bills as "input"), so the subtraction is not unit-comparable and, per adv B4/arch B4, cannot even reliably stay non-negative. The corrected definition: `tokens_saved = counterfactual_est − compacted_est`, where **both sides come from the same `TokenEstimator` call**, one run against the pre-compaction `messages` array, the other against `apply()`'s post-compaction `out` array (Epic 2.1). This is a valid subtraction by construction (same units, same estimator, same message-extraction logic) and is exactly `0` at `CompactionTier::Off` because `out == messages` there — no separate special-casing needed. `actual_tokens` (from real `usage.input_tokens` + `usage.output_tokens`, `TokenSource::Exact`) is reported **separately**, used for the dollar figure (`actual_tokens` is what the operator is actually billed for) and as a sanity check that `compacted_est` is in the right ballpark — never as an operand of `tokens_saved`.

**Acceptance Criteria**:
- For a session with no `SessionCostState` entry at all (never seen by `record_pending`), `report_for_session` returns `Err(CostReportError::SessionNotFound)` — distinct from a zeroed report (ux.md error-states table row 1). Uses the store's non-creating `get`, never `get_or_init` (a report read must not create session state as a side effect).
  - *Given* a `CostTracker` with an empty store, *When* `report_for_session(&SessionKey::new("ghost"))` is called, *Then* it returns `Err(CostReportError::SessionNotFound)`.
- For a session with only `Pending` records (compacted, no response completed yet), the report's `actual_tokens` is `None` and `tokens_saved`/`estimated_cost_saved_usd` are `None` (not `0`) — ux.md's "counterfactual-but-no-actual" row.
  - *Given* a session with exactly one `Pending` `CostRecord` (`counterfactual_est = Some(41200)`, `actual_tokens = None`), *When* `report_for_session` is called, *Then* `CostReport.actual_tokens == None` and `CostReport.tokens_saved == None` (the `Pending` row contributes nothing to `totals_by_tier` per Story 1.3.2, so there is nothing to report yet).
- For a session with only `Reconciled` `CompactionTier::Off` records (nothing was elided), `counterfactual_est == compacted_est` and `tokens_saved == Some(0)` by construction (same messages, same estimator, no subtraction special-case required) — not an error, not an omitted session.
  - *Given* a `Reconciled` record with `tier: Off`, `counterfactual_est = 10000`, `compacted_est = 10000`, *When* `report_for_session` is called, *Then* `tokens_saved == Some(0)`.
- Report includes a **per-tier breakdown** (`by_tier: [TierBreakdown; 4]` or `Vec<TierBreakdown>` built from the fixed-size array), directly addressing the user's stated `TierThresholds`-tuning motivation (features.md §6 "unstated need").
  - *Given* a session with `Reconciled` records under both `Auto` (`counterfactual_est=10000, compacted_est=8000`) and `Full` (`counterfactual_est=12000, compacted_est=4000`), *When* `report_for_session` is called, *Then* `CostReport.by_tier` shows `Auto` `tokens_saved: Some(2000)` and `Full` `tokens_saved: Some(8000)`, plus a cumulative total `tokens_saved: Some(10000)`.
- **`report_for_session` performs zero `PricingTable` lookups** — every `CostAmountUsd` it returns was already computed and folded into `TierTotals.cost_counterfactual`/`cost_actual` at write time (Story 1.3.2's fold step), under the same write lock that flipped the row to `Reconciled`. This is the fix for arch B5.2's "report-time single-model pricing breaks under multi-model sessions" — pricing happens once, per-record, at the model that record actually used, not once-for-the-whole-report against whichever model happens to be looked up last.
  - *Given* a session with two `Reconciled` records under different models (`claude-sonnet-5` and `gpt-4o`), *When* `report_for_session` is called, *Then* the returned `cost_actual`/`cost_counterfactual` figures are the sum of each record's own already-priced `cost`, not a single price applied to the combined token count.
- **Golden-value pricing test (new, repair iteration 1, was arch B5.2/adv B5)**: *Given* `ModelPrice { input_usd_per_token: 0.000003, output_usd_per_token: ... }` and a `Reconciled` record with `actual_tokens = 8000` (input), *When* the record is priced at write time, *Then* `cost.0 == 0.024` exactly (`8000.0 * 0.000003`) — this pins the unit as per-token, not per-million, so a future refactor that reintroduces a x1,000,000 error fails a test immediately instead of silently producing a wrong dollar figure.
- Every `CostAmountUsd`-bearing field is `None` (not `$0.00`) when the record's model has no entry in `PricingTable` (features.md pitfall 7).
- `actual_tokens` in the report includes **both** `usage.input_tokens` and `usage.output_tokens` (repair iteration 1 cheap-fix — `src/providers/mod.rs`'s `translate_anthropic_to_openai` already extracts `completion_tokens` from `usage.output_tokens`; the previous draft's report only surfaced the input side, making the dollar figure silently input-only without saying so).
**Files**: `src/cost_metrics/tracker.rs`, `src/cost_metrics/report.rs`

##### Task 1.3.3a: Define `CostReport`, `TierBreakdown`, `CostReportError` (~5 min)
- `CostReport { session_key: String, actual_tokens: Option<u64>, actual_source: Option<TokenSource>, counterfactual_tokens: Option<u64>, counterfactual_source: Option<TokenSource>, compacted_tokens: Option<u64>, tokens_saved: Option<u64>, estimated_cost_saved_usd: Option<f64>, actual_cost_usd: Option<f64>, pricing_source: PricingSource, pending_count: usize, abandoned_count: usize, by_tier: Vec<TierBreakdown> }` — all `#[derive(Serialize)]`. Field renamed from the previous `counterfactual_source`/single-`actual_tokens`-implicitly-input-only shape to make the input+output inclusion explicit. **repair iteration 1 addendum (Phase 4 cross-artifact review)**: `actual_source`/`counterfactual_source` restore per-figure provenance (requirements.md Observability Requirements: report "which token-count source was used for each"), dropped from an earlier draft's flat shape; both are `None` exactly when their paired `_tokens` field is `None` (e.g. still `Pending`). `compacted_est`'s source is definitionally identical to `counterfactual_est`'s (same `TokenEstimator` call, Story 1.3.3) so no separate `compacted_source` field is needed.
- Files: `src/cost_metrics/report.rs`

##### Task 1.3.3b: Implement `report_for_session` as a pure read over `totals_by_tier` (~5 min)
- Reads `totals_by_tier` (already reconciled-only, already priced), sums across the 4-element array for cumulative figures, builds `CostReport`. Performs **no** `PricingTable` access — that is a compile-time-checkable invariant this task should preserve by not even holding a `&PricingTable` reference in `report_for_session`'s signature.
- Files: `src/cost_metrics/tracker.rs`

##### Task 1.3.3c: Unit tests for the four Given-When-Then scenarios above (~5 min)
- One test per acceptance criterion bullet.
- Files: `src/cost_metrics/tracker.rs` (`#[cfg(test)]`)

---

### Epic 1.4: Pricing table (ADR-013)
**Goal**: Static/config-overridable `PricingTable`, vendored LiteLLM snapshot, optional background live-refresh — never inline in the hot path.

#### Story 1.4.1: Static table from vendored LiteLLM snapshot + config overrides
**As a** `CostTracker`, **I want** model prices loaded from a checked-in JSON fixture merged with optional user config, **so that** dollar figures work standalone with no network dependency (requirements.md's "load-bearing fallback").
**Acceptance Criteria**:
- `PricingTable::load_default()` parses `src/cost_metrics/pricing_default.json` and returns prices for at least `claude-sonnet-5`/`claude-opus-4-5` and `gpt-4o` (whatever the vendored snapshot's filtered model set includes at implementation time).
  - *Given* `pricing_default.json` contains `{"claude-sonnet-5": {"input_cost_per_token": 0.000003, "output_cost_per_token": 0.000015}}`, *When* `PricingTable::load_default().price_for("claude-sonnet-5")` is called, *Then* it returns `Some(ModelPrice{input: 0.000003, output: 0.000015})`.
- `PricingTable::merge_overrides(&mut self, overrides: HashMap<String, ModelPrice>)` — an override for a model already in the default table replaces it; a model not present is added.
  - *Given* the default table above and `overrides = {"claude-sonnet-5": ModelPrice{input: 0.000004, output: 0.000015}}`, *When* `merge_overrides` runs, *Then* `price_for("claude-sonnet-5").input == 0.000004`.
- `price_for` on a model absent from both returns `None`, never a default/zero price.
**Files**: `src/cost_metrics/pricing.rs`, `src/cost_metrics/pricing_default.json`

##### Task 1.4.1a: Vendor a filtered LiteLLM snapshot (~5 min)
- Fetch `model_prices_and_context_window.json` from `github.com/BerriAI/litellm`, filter to Anthropic + OpenAI model entries consolette actually routes to (per `src/providers/`'s known model list), write to `src/cost_metrics/pricing_default.json`. Add a comment/header noting the source URL and fetch date for future re-sync.
- Files: `src/cost_metrics/pricing_default.json`

##### Task 1.4.1b: Implement `ModelPrice`, `PricingTable::load_default`, `price_for` (~5 min)
- Files: `src/cost_metrics/pricing.rs`

##### Task 1.4.1c: Implement `merge_overrides` (~3 min)
- Files: `src/cost_metrics/pricing.rs`

##### Task 1.4.1d: Unit tests for load/override/miss (~4 min)
- Files: `src/cost_metrics/pricing.rs` (`#[cfg(test)]`)

##### Task 1.4.1e: Test `price_for` against the real model-name strings `src/providers/` emits on live requests (~5 min)
**Pre-mortem P1 #2**: every test above uses hand-picked fixture strings; nothing proves `price_for` resolves against what `src/providers/` actually produces. `src/providers/anthropic.rs`'s `normalize_model_name` strips Bedrock prefix/suffix (`us.anthropic.claude-3-5-sonnet-20241022-v2:0` → `claude-3-5-sonnet-20241022`, confirmed by its own unit tests at `src/providers/anthropic.rs:565-576`), `src/providers/bedrock.rs` maintains its own alias tables mapping short names (`claude-sonnet-4-6`, `claude-opus-4-6`) to Bedrock-qualified ids (`us.anthropic.claude-sonnet-4-6`, `us.anthropic.claude-opus-4-6-v1`) and back, and `src/providers/mod.rs:138` falls back to the literal default `claude-3-haiku-20240307` when no model is specified. Any of these could legitimately miss the LiteLLM snapshot's keys.
- For each distinct model-name string these call sites can emit (enumerate via `grep -rn "normalize_model_name\|claude-\|gpt-" src/providers/*.rs`, not re-typed from memory), assert `PricingTable::load_default().price_for(name)` returns `Some(_)`.
  - *Given* `normalize_model_name("us.anthropic.claude-3-5-sonnet-20241022-v2:0")` (→ `"claude-3-5-sonnet-20241022"`), *When* `price_for` is called with that normalized output, *Then* it returns `Some(_)`, not `None`.
  - *Given* the `mod.rs:138` fallback string `"claude-3-haiku-20240307"`, *When* `price_for` is called with it, *Then* it returns `Some(_)`.
- If any of these real strings return `None` against the vendored snapshot, add an alias/normalization layer to `pricing.rs` (e.g. a `normalize_for_pricing(model: &str) -> &str` pass applied before the `HashMap` lookup, or an explicit alias table merged at `load_default()` time) so `price_for` resolves them — do not silently leave the test red or hand-edit the fixture strings elsewhere in the plan to match.
- Files: `src/cost_metrics/pricing.rs` (`#[cfg(test)]`, references `src/providers/anthropic.rs`, `src/providers/bedrock.rs`, `src/providers/mod.rs`)

#### Story 1.4.2 (stretch): Optional background live-refresh from LiteLLM's raw JSON
**As an** operator, **I want** the pricing table to optionally refresh itself from LiteLLM's live GitHub JSON on an interval, **so that** prices don't silently go stale forever without at least the option of staying current.
**Acceptance Criteria**:
- **repair iteration 1 (was arch/adv C12)**: `arc_swap` is not a dependency in this repo and is not being added for this one swap site — use `tokio::sync::watch::Sender<Arc<PricingTable>>`/`Receiver`, an already-present crate that gives the same atomic-pointer-swap behavior (`Sender::send(Arc::new(new_table))`, readers hold a cloned `Receiver` and call `.borrow().clone()`).
- A `tokio::spawn`ed background task fetches LiteLLM's raw JSON on a configurable interval (default e.g. 24h) and, on success, atomically swaps the `tokio::sync::watch`-held `Arc<PricingTable>` that pricing-at-write-time (Story 1.3.2's fold step) reads synchronously — note this is read when a record is priced, not inside `report_for_session` (Story 1.3.3, pure read).
  - *Given* the background task's first fetch succeeds with a table containing a new model not in the static snapshot, *When* `report_for_session` next runs, *Then* `pricing_source == PricingSource::Live` for that computation.
- On fetch failure (network error, non-200, malformed JSON), the task logs a `tracing::warn!`, increments `cost_metrics_pricing_fallback_total`, and leaves the current table (static or last-successful-live) in place — never panics, never blocks any `apply()` caller.
  - *Given* the background task's fetch returns `500`, *When* the interval fires, *Then* `PricingTable` is unchanged and one `tracing::warn!` log line and one metric increment are observed.
**Files**: `src/cost_metrics/pricing.rs`

##### Task 1.4.2a: Implement `spawn_pricing_refresh_task` (~5 min)
- Files: `src/cost_metrics/pricing.rs`

##### Task 1.4.2b: Implement fetch-failure fallback + metric/log (~4 min)
- Files: `src/cost_metrics/pricing.rs`

##### Task 1.4.2c: Test: failed fetch leaves table unchanged and logs (~4 min)
- Mock the LiteLLM URL failing; assert table pointer/content unchanged.
- Files: `src/cost_metrics/pricing.rs` (`#[cfg(test)]`)

---

## Phase 2: Integration plumbing

### Epic 2.1: `CompactHooks` integration — `CostTrackingHook`, `RequestId`, and the `post_compact` signature change
**Goal**: Wire the counterfactual-recording side through `CompactHooks`, changing `post_compact`'s actual signature (not adding a second method) so the hook has everything it needs in one call.

**repair iteration 1 (was arch B2)**: the previous draft proposed adding a *second* trait method, `post_compact_with_messages`, defaulted to a no-op, to smuggle `messages` in alongside the existing `post_compact(&self, session: &SessionState, report: &CompactionReport)`. That's unnecessary indirection: `apply()` (in `src/session_compaction/mod.rs`) already has `session_key`, `session`, the pre-compaction `messages`, and the constructed `report` all in scope at the single call site where hooks run today. The fix is to change `post_compact`'s signature directly to take one context struct carrying all four, and update the trait's existing implementors (confirmed via `grep -rn "impl CompactHooks for" src` — every implementor is a `#[cfg(test)]` struct; there is no production consumer whose call timing this could break, correcting the previous draft's now-deleted false claim about `PlanReinjection`/`SkillReinjection`, which are called inline in `apply()`, not through `CompactHooks`, per Step 0.5's corrected rationale above).

#### Story 2.1.1: `post_compact` takes a `PostCompactContext`; `CostTrackingHook` synchronously inserts `Pending`, asynchronously fills the counterfactual
**As a** `SessionCompactionPipeline`, **I want** cost accounting to happen entirely through the existing `CompactHooks` seam, with the hook's signature carrying everything it needs, **so that** `apply()`'s tested compaction logic gains one parameter object and one hook registration, nothing more.
**Acceptance Criteria**:
- `CompactHooks::post_compact`'s signature is changed to `fn post_compact(&self, ctx: &PostCompactContext<'_>)`, where `PostCompactContext<'a> { session_key: &'a SessionKey, session: &'a SessionState, pre_compaction_messages: &'a Value, report: &'a CompactionReport }`. All `#[cfg(test)]` implementors in the codebase are updated to the new signature in the same change (there are no production implementors to migrate).
  - *Given* the updated trait, *When* `cargo build` runs, *Then* every existing `#[cfg(test)]` `impl CompactHooks for ...` in the repo compiles against the new signature (enumerate them via `grep -rn "impl CompactHooks for" src` and update each).
- `CostTrackingHook::post_compact` synchronously (no `.await` on anything but an in-memory write lock) calls `CostTracker::record_pending(ctx.session_key, ctx.report.request_id, ctx.report.tier)`, inserting a `Pending` row with no token counts yet — this call happens before `post_compact` returns, so "the row exists" is an invariant of `apply()` having run, not a race with a spawned task.
  - *Given* a `SessionCompactionPipeline` with one registered `CostTrackingHook`, *When* `apply(&session_key, &messages, 0.95)` runs (pressure selecting `CompactionTier::Full`) and returns, *Then* `CostTracker::get(&session_key)` (called immediately, no `.await` on anything but the tracker call itself) shows exactly one `Pending` `CostRecord` under `CompactionTier::Full` — this must hold even if the estimator never completes.
- `CostTrackingHook::post_compact` then spawns a `tokio::task` (not awaited inline) that runs the configured `TokenEstimator` twice — once on `ctx.pre_compaction_messages`, once on `ctx.report`'s post-compaction output (`out`, per Task 2.1.1c) — and, on success, calls `CostTracker::record_counterfactual(ctx.session_key, ctx.report.request_id, counterfactual_est, compacted_est)`; on `Err(EstimatorError)`, calls `CostTracker::record_request_failed` instead so the row does not stay `Pending` forever on an estimator failure.
  - *Given* an `AnthropicCountTokensEstimator` mock configured with an artificial 500ms delay, *When* `apply()` runs with the hook registered, *Then* `apply()` returns in under 50ms (the delay is not on its critical path) — measured via a `tokio::time::pause()`-based or wall-clock-bounded test.
  - *Given* the estimator mock returns `Err(EstimatorError::RateLimited)`, *When* the spawned task runs to completion, *Then* the row's `status` becomes `Abandoned`, not left `Pending` indefinitely.
**Files**: `src/session_compaction/hooks.rs` (signature change), `src/session_compaction/mod.rs` (thread `PostCompactContext` at the one call site, generate `RequestId`), `src/cost_metrics/hook.rs` (new)

##### Task 2.1.1a: Add `request_id: RequestId` to `CompactionReport`; fix `RequestId`'s `Default` to `Uuid::nil()` (~5 min)
- Add `pub request_id: RequestId` to `CompactionReport`. **repair iteration 1 (was arch C10/adv M4)**: `RequestId`'s `Default` impl is explicitly `RequestId(Uuid::nil())`, not `Uuid::new_v4()` — a fresh-random-UUID default would make two separately-constructed `CompactionReport::default()` values compare unequal, silently breaking the struct's derived `#[derive(PartialEq, Eq)]` reflexivity (`Default::default() == Default::default()` must hold for any type deriving `Eq` that relies on structural equality in tests). `RequestId::new()` (real, `Uuid::new_v4()`-backed) is generated exactly once, explicitly, at the top of `apply()` — never via `Default::default()` outside test fixtures.
- `apply()` (in `src/session_compaction/mod.rs`) generates one `RequestId::new()` at the top of the function and threads it into the constructed `CompactionReport` and into the new `PostCompactContext` (Task 2.1.1b) — this and Task 2.1.1b are the only required edits to `apply()`'s body, additive only, no existing field/branch touched.
- Files: `src/session_compaction/hooks.rs`, `src/session_compaction/mod.rs`

##### Task 2.1.1b: Change `CompactHooks::post_compact` signature to `PostCompactContext<'_>`; update `apply()`'s one call site (~6 min)
- Define `pub struct PostCompactContext<'a> { pub session_key: &'a SessionKey, pub session: &'a SessionState, pub pre_compaction_messages: &'a Value, pub report: &'a CompactionReport }` in `src/session_compaction/hooks.rs`.
- Change the trait method to `fn post_compact(&self, ctx: &PostCompactContext<'_>)`.
- At `apply()`'s existing `self.hooks.run_post_compact(&state, &report)` call site, construct one `PostCompactContext` from the four values already in scope (`session_key`, `state`/`session`, the original pre-compaction `messages` reference, `&report`) and pass `&ctx` through to every registered hook.
- Update every `#[cfg(test)]` `impl CompactHooks for ...` in the repo to the new signature (mechanical, per the `grep` inventory in the Story's acceptance criteria).
- Files: `src/session_compaction/hooks.rs`, `src/session_compaction/mod.rs`

##### Task 2.1.1c: Implement `CostTrackingHook` (sync `record_pending` + spawned same-estimator counterfactual fill) (~7 min)
- `pub struct CostTrackingHook { tracker: Arc<CostTracker>, estimator_for: fn(&str) -> EstimatorKind }` (or equivalent model-to-estimator selection — reuses whatever `EstimatorKind` dispatch Epic 1.2 already defined).
- `post_compact(&self, ctx: &PostCompactContext<'_>)`: (1) synchronously calls `self.tracker.record_pending(ctx.session_key, ctx.report.request_id, ctx.report.tier)`; (2) clones the `Arc<CostTracker>`, `ctx.session_key`, `ctx.report.request_id`, `ctx.pre_compaction_messages` (cheap `Arc`/`Value` clone — the "without cloning large payloads" concern from the prior draft is addressed by cloning once here rather than growing `CompactionReport` with duplicated message data), and the post-compaction `out` messages (from `ctx.report`, per whatever field already carries the compacted output — verify against `CompactionReport`'s actual definition in `src/session_compaction/hooks.rs` before implementing), and spawns a `tokio::task` that runs the estimator on both arrays and calls `record_counterfactual` (success) or `record_request_failed` (estimator error) — log+drop the `JoinHandle` per existing async-fire-and-forget patterns in this codebase (check `src/` for an existing `JoinSet` precedent first; use one if it exists, otherwise a bare dropped `JoinHandle` with a `tracing::error!` on `Err` is sufficient here since failure already has a defined outcome, `Abandoned`).
- Files: `src/cost_metrics/hook.rs`

##### Task 2.1.1d: Wire `SessionCompactionPipeline::apply` to construct `PostCompactContext` and register `CostTrackingHook` (~4 min)
- Covered by Task 2.1.1b's `apply()` edit; this task is registering `CostTrackingHook` itself into the pipeline's hook list at construction time in `serve-cost` (Epic 2.3) — no separate `apply()` change beyond Task 2.1.1b.
- Files: `src/session_compaction/mod.rs`, `src/cost_metrics/server.rs`

##### Task 2.1.1e: Integration test: registered hook produces a `Pending` record synchronously after `apply()` returns, before the spawned task completes (~6 min)
- Use a slow/never-resolving mock estimator to prove the `Pending` row exists the instant `apply()` returns, independent of estimator completion (this is the load-bearing test for the synchronous-insert half of Blocker 3).
- Files: `src/cost_metrics/hook.rs` (`#[cfg(test)]`) or `src/session_compaction/mod.rs`'s existing test module

##### Task 2.1.1f: Test: `apply()`'s wall-clock time is unaffected by estimator latency (~5 min)
- Files: `src/cost_metrics/hook.rs` (`#[cfg(test)]`)

##### Task 2.1.1g: Test: estimator failure transitions the row to `Abandoned`, not left `Pending` (~4 min)
- Files: `src/cost_metrics/hook.rs` (`#[cfg(test)]`)

---

### Epic 2.2: Provider-response integration — `record_actual_usage` call site, scoped to what's actually reachable
**Goal**: Thread `SessionKey`/`RequestId` into the one place real `usage.input_tokens`/`output_tokens` exist and call `CostTracker::record_actual_usage`, stated honestly against what's actually wired up in this codebase today.

**repair iteration 1 (was adv B7)**: verified via `grep -rn "translate_anthropic_to_openai" src` that this function has **zero callers anywhere in the codebase** — it is exercised only by its own unit test. That means, as written, the entire actual-usage/reconciliation half of this epic would attach to a function `RequestId` never actually crosses at runtime; describing it as "the real call site" without qualification is misleading. This story now states that explicitly and narrows scope to what a re-reviewer can actually verify: the wrapper is built and unit-tested against `translate_anthropic_to_openai` (since that's where `usage.*` extraction already lives and is already unit-tested, and duplicating the parsing logic elsewhere would be worse), but it is documented as **not reachable from any live request path in this codebase today** — wiring it into a real dispatch loop is out of scope for this feature (same scope boundary ADR-012's Amendment already accepts for `serve-cost`-only pipeline construction). Retry double-counting (adv B7's other concern) is addressed by making `record_actual_usage` idempotent per `request_id` (already specified in Story 1.3.2 — replace, never add) plus a bounded pending-row age-out sweep so orphans from a future live dispatch loop's retries don't accumulate unbounded.

#### Story 2.2.1: `record_actual_usage_from_anthropic_response` wrapper around the existing (currently uncalled) usage-extraction path
**As a** `CostTracker`, **I want** the real `usage.*` values already extracted in `src/providers/mod.rs` fed into `record_actual_usage`, **so that** the actual side of the comparison is populated from ground truth wherever this call path is eventually wired into a live request flow, with correct behavior specified and tested now rather than assumed later.
**Acceptance Criteria**:
- **Explicit reachability statement (repair iteration 1)**: `translate_anthropic_to_openai` has no production caller in this codebase as of this plan (`grep -rn "translate_anthropic_to_openai" src` returns only its definition and its own unit test). The wrapper built in this story is unit-tested directly, not verified end-to-end through a live dispatch path, because no such path exists yet. This is recorded here so a re-reviewer checking "is `RequestId` actually threaded through a real execution boundary" gets a true answer (no) instead of an implied one.
- A new function `pub fn record_actual_usage_from_anthropic_response(tracker: &CostTracker, session_key: &SessionKey, request_id: RequestId, model: &str, anthropic_response: &Value)` extracts `usage.input_tokens` **and** `usage.output_tokens` (repair iteration 1 cheap fix: the previous draft's helper returned only input; both are now extracted and both are priced, or the report explicitly labels a figure input-only if a future model's response lacks `output_tokens` — never silently input-only without saying so) via a shared helper, and calls `tracker.record_actual_usage(...)` with `TokenSource::Exact`.
  - *Given* an Anthropic response `{"usage": {"input_tokens": 12400, "output_tokens": 300}, "model": "claude-sonnet-5", ...}` and a prior `Pending` `CostRecord` for the same `(session_key, request_id)`, *When* `record_actual_usage_from_anthropic_response` is called, *Then* that record's `actual_tokens == Some(TokenCount{12700, Exact})` (input + output combined for the billed-token figure) and `status` transitions toward `Reconciled` once `counterfactual_est`/`compacted_est` are also present.
- **Idempotency test (was adv B7)**: *Given* the same `(session_key, request_id)` reconciled once already, *When* `record_actual_usage_from_anthropic_response` is called a second time with a different `usage` value (simulating a provider-layer retry re-delivering the same logical request), *Then* the stored `actual_tokens`/`cost` reflect only the second call's value, and `totals_by_tier` is not double-folded (this is Story 1.3.2's idempotency guarantee, exercised here through this specific call path).
- This call site is additive to `translate_anthropic_to_openai`'s caller (not inside the pure function itself, preserving its existing "pure, stateless, unit-tested" contract) — a new thin wrapper function calls both `translate_anthropic_to_openai` and the new usage-reporting function, and remains, like its dependency, uncalled from a live path.
**Files**: `src/providers/mod.rs`, `src/cost_metrics/mod.rs`

##### Task 2.2.1a: Extract shared `usage.*` parsing into a small helper, both input and output tokens (~4 min)
- `fn extract_usage(anthropic: &Value) -> Option<(u64, u64)>` returning `(input_tokens, output_tokens)`, used by both `translate_anthropic_to_openai` and the new reporting wrapper — one parsing site, not two.
- Files: `src/providers/mod.rs`

##### Task 2.2.1b: Implement `record_actual_usage_from_anthropic_response` wrapper (~5 min)
- Files: `src/cost_metrics/mod.rs`

##### Task 2.2.1c: Implement the additive wrapper function that calls both `translate_anthropic_to_openai` and the new reporter (~5 min)
- e.g. `pub fn translate_and_record(tracker: &CostTracker, session_key: &SessionKey, request_id: RequestId, anthropic: &Value) -> Value` — keeps `translate_anthropic_to_openai` itself untouched and still callable standalone by its existing unit tests. Doc-comment explicitly notes this wrapper has no caller yet (see Story's reachability statement).
- Files: `src/providers/mod.rs`

##### Task 2.2.1d: Unit tests: successful reconciliation (input+output), idempotent double-record, and adverse-ordering (actual arrives before pending) (~7 min)
- Covers the idempotency AC above plus Story 1.3.2's adverse-ordering create-branch, exercised through this specific wrapper.
- Files: `src/providers/mod.rs` (`#[cfg(test)]`) or `src/cost_metrics/mod.rs`

#### Story 2.2.2: Failure-path cleanup — `record_request_failed`, plus a read-time age-out sweep for orphaned `Pending` rows
**As a** `CostTracker`, **I want** every place a request can fail/time out after `apply()` recorded a counterfactual to call `record_request_failed`, and any row still `Pending` well past a reasonable age to age out on its own, **so that** no `Pending` record is orphaned forever even without a live error-handling call site (pitfalls.md's memory-leak/never-reconciles failure mode; adv B7's "unspecified retry/orphan" concern).
**Acceptance Criteria**:
- Since no live provider-dispatch error path exists in this codebase yet (`Router::dispatch`/`Provider::send` aren't wired into a running server, confirmed above), this story documents and stubs the error-path contract via a test-only harness: a `#[test]` simulating "estimate recorded, then failure signaled" asserts the record transitions to `Abandoned`, and a code comment at the (future) real provider-dispatch error-handling site marks it as "MUST call `record_request_failed` here" for whoever wires up the live proxy later.
  - *Given* a `Pending` `CostRecord`, *When* `record_request_failed(session_key, request_id)` is called (simulating an upstream 5xx/timeout), *Then* `status == Abandoned` and the record is excluded from `totals_by_tier` sums in a subsequent `report_for_session` call, while still counted in `CostReport.abandoned_count`.
- **Read-time age-out sweep (new, repair iteration 1, was adv B7's "orphaned rows unbounded without a live error path")**: since no live dispatch loop exists to reliably call `record_request_failed` on every failure today, `SessionCostStore` (or `CostTracker`) sweeps `Pending` rows older than a configurable `PENDING_MAX_AGE` (default e.g. 10 minutes) to `Abandoned` lazily, on the next read that touches that session's state (`get`, `report_for_session`, or the next write) — no separate background timer task, keeping the "no new persistent datastore/background daemon beyond the one optional pricing-refresh task" scope boundary intact.
  - *Given* a `Pending` `CostRecord` with `recorded_at` more than `PENDING_MAX_AGE` in the past, *When* `report_for_session` (or any other read/write touching that session) is next called, *Then* that row's `status` is `Abandoned` and it's excluded from `totals_by_tier` and reflected in `abandoned_count`.
**Files**: `src/cost_metrics/tracker.rs`, `src/cost_metrics/store.rs`, `src/providers/mod.rs` (doc comment marker only, since no real error call site exists yet)

##### Task 2.2.2a: Test: `record_request_failed` marks `Abandoned`, excluded from totals, counted in `abandoned_count` (~5 min)
- Files: `src/cost_metrics/tracker.rs` (`#[cfg(test)]`)

##### Task 2.2.2b: Implement the read-time `PENDING_MAX_AGE` sweep (~5 min)
- Files: `src/cost_metrics/store.rs`

##### Task 2.2.2c: Test: a stale `Pending` row ages out to `Abandoned` on the next read (~4 min)
- Files: `src/cost_metrics/tracker.rs` (`#[cfg(test)]`)

##### Task 2.2.2d: Add a `// TODO(cost-metrics):` doc-comment marker at the nearest existing error-handling seam in `providers`/`routing` for future wiring (~2 min)
- Files: `src/providers/mod.rs` or `src/routing/router.rs` (wherever `Provider::send`'s `Result::Err` branch is handled, if one exists yet; otherwise on the trait definition itself)

---

### Epic 2.3: `serve-cost` — the single stateful process owning pipeline + tracker + HTTP route (ADR-012, Amendment)
**Goal**: `consolette serve-cost` is the one live process that constructs `SessionCompactionPipeline` with `CostTrackingHook` registered, owns the one `Arc<CostTracker>`, and hosts the one HTTP route against it — resolving Blocker 1 ("no live process wires `SessionCompactionPipeline` into anything the CLI/HTTP surfaces can read") by making this the single place both surfaces ultimately read from.

**repair iteration 1 (was arch B1/adv B1)**: the previous draft's Epic 2.3 stood up a route-only axum server, and Epic 3.1 (CLI) separately constructed its own in-process `CostTracker`. Neither one ever constructed a `SessionCompactionPipeline` with `CostTrackingHook` registered, so in a real deployment there was no live process where compaction actually happened *and* cost got tracked *and* either the CLI or the route could read it back — the two surfaces would only ever agree in a hand-populated unit test. The Remote Facade pattern below fixes this: `serve-cost` becomes that one live process, and the CLI (Epic 3.1) becomes a thin `reqwest` client of its route rather than an independent constructor of equivalent state.

#### Story 2.3.1: `serve-cost` constructs `SessionCompactionPipeline` + `CostTrackingHook` + the `/v1/cost/{session_key}` route together, sharing one `Arc<CostTracker>`
**As an** operator, **I want** one command that both makes compaction cost-tracked and exposes the resulting data over HTTP, **so that** running `serve-cost` is sufficient — by itself, structurally — for both the CLI and any HTTP client to read real data.
**Acceptance Criteria**:
- `consolette serve-cost` (clap derive, mirrors `main.rs`'s existing `Command` enum pattern) constructs one `Arc<CostTracker>`, builds a `SessionCompactionPipeline` with a `CostTrackingHook { tracker: Arc::clone(&cost_tracker) }` registered, and builds a `Router::new().route("/v1/cost/{session_key}", get(handler_cost_report)).with_state(Arc::clone(&cost_tracker))` — the pipeline and the route share the identical `Arc<CostTracker>` instance, not two separately-constructed trackers.
  - *Given* the binary run as `consolette serve-cost --port 8787`, *When* a `GET http://127.0.0.1:8787/v1/cost/nonexistent` request is made, *Then* the response is `404` with body `{"error":"session_not_found","session_key":"nonexistent"}` (per ux.md's error-states table). Route is versioned (`/v1/...`, repair iteration 1 cheap fix — was unversioned `/cost/...`).
- Binds `127.0.0.1` by default (loopback-only, repair iteration 1 cheap fix — matches the security classification of an operator-only diagnostic tool, not a public service), with `--port` as a CLI override; if `--port` is not given, the port is read from consolette's existing config file (whatever config-loading mechanism `main.rs` already uses for other subcommands — reuse it, do not add a second, config-blind flag-only path) before falling back to a hardcoded default (`8787`).
  - *Given* no `--port` flag and a config file specifying a cost-server port, *When* `consolette serve-cost` starts, *Then* it binds that configured port, not the hardcoded default.
- The server registers exactly one route group (`/v1/cost/*`) — no `providers`/`routing` wiring, per the explicit out-of-scope decision; this process is explicitly the *only* place in the codebase that constructs a `SessionCompactionPipeline` with `CostTrackingHook` registered (documented in ADR-012's Amendment and its accepted-scope-boundary consequence).
**Files**: `src/main.rs`, `src/cost_metrics/server.rs` (new)

##### Task 2.3.1a: Add `ServeCost { port: Option<u16> }` variant to `main.rs`'s `Command` enum (~3 min)
- `Option<u16>` (not a bare `u16`) so "not given" is distinguishable from "given," enabling the config-fallback precedence in the Story's ACs.
- Files: `src/main.rs`

##### Task 2.3.1b: Implement `serve_cost_command(port_override: Option<u16>) -> anyhow::Result<()>` (~7 min)
- Resolves the port (`port_override` → config → `8787` default), constructs `PricingTable::load_default()`, the shared `Arc<CostTracker>`, the `SessionCompactionPipeline` with `CostTrackingHook` registered, and the `Router`; binds `127.0.0.1:{port}` and calls `axum::serve`.
- Files: `src/main.rs`, `src/cost_metrics/server.rs`

##### Task 2.3.1c: Wire the new `Command::ServeCost` match arm in `main()` (~2 min)
- Files: `src/main.rs`

##### Task 2.3.1d: Smoke test: server starts, responds to `/v1/cost/{unknown}` with 404 (~5 min)
- Bind to port `0` (OS-assigned) in the test, use `reqwest` to hit the actual returned address — an integration-style test, not a unit test of the handler function in isolation (that's Epic 3.2's job).
- Files: `src/cost_metrics/server.rs` (`#[cfg(test)]`)

##### Task 2.3.1e: Integration test: a real `apply()` call through this process's pipeline is later readable via the `/v1/cost/{session_key}` route (~6 min)
- **New, repair iteration 1** — this is the direct regression guard for Blocker 1: construct `serve-cost`'s pipeline+tracker+route exactly as `serve_cost_command` does, drive one `apply()` call through the pipeline, synthetically reconcile it, and assert the HTTP route (via axum's test utilities, no real network bind needed) returns that data. Proves the "one process, one tracker" structural property end to end, not just that the two halves compile against the same type.
- Files: `src/cost_metrics/server.rs` (`#[cfg(test)]`)

---

## Phase 3: Operator-facing surfaces

### Epic 3.1: CLI subcommand — a thin `reqwest` client of `serve-cost`'s route
**Goal**: `consolette cost-report <session-key>` prints the same `CostReport` the HTTP route returns, fetched over HTTP, not recomputed in-process.

**repair iteration 1 (was arch B1/adv B1)**: the previous draft had `cost-report` construct its own `CostTracker` in-process, independent of whatever `serve-cost` was doing. Since no other process feeds that in-process tracker any real data (compaction only happens inside `serve-cost`'s pipeline, per Epic 2.3), a standalone-constructing `cost-report` could only ever report on sessions it fabricated for its own test — never a real one. The fix, per ADR-012's Amendment: `cost-report` is a `reqwest` HTTP client (`reqwest` is already a direct dependency, confirmed against `Cargo.toml`) of `serve-cost`'s `GET /v1/cost/{session_key}` route. This makes "the CLI and HTTP surfaces agree" a structural property (one server, one tracker, one response body parsed two ways) rather than two independent implementations that happen to use the same struct definition.

#### Story 3.1.1: `cost-report` subcommand fetches from a running `serve-cost` and formats the response
**As an** operator, **I want** `consolette cost-report <session>` to print actual/counterfactual/savings for a session by asking the running `serve-cost` process, **so that** I see the same data the HTTP route would return, without needing to construct any tracking state myself.
**Acceptance Criteria**:
- `consolette cost-report s1 --server http://127.0.0.1:8787` (or a config-resolved default matching `serve-cost`'s own default bind/port, so the flag is optional in the common case) performs `GET {server}/v1/cost/s1` via `reqwest::blocking` or an `async fn main` (whichever matches this binary's existing `main()` shape — check `src/main.rs` before choosing), deserializes the JSON body into `CostReport`, and prints it via the same table/JSON formatters as before.
  - *Given* a `serve-cost` instance running and reachable, holding a `Reconciled`, `Full`-tier session with `actual_tokens=4000, counterfactual_tokens=12000` (and a resolvable price), *When* `cost-report s1` runs, *Then* stdout contains a line equivalent to `"tokens_saved: 8,000 (66.7%)"` and the process exits `0`.
- `consolette cost-report s1 --json` prints `serde_json::to_string_pretty(&report)` where `report` is the exact `CostReport` deserialized from the HTTP response body (mirrors `src/bin/mcp-proxy/cli.rs:181`'s house style) — satisfying "CLI and API share one serialization type" as a runtime fact (same bytes over the wire, parsed once), not merely a compile-time one.
  - *Given* the same server state, *When* `cost-report s1 --json` runs, *Then* stdout is valid JSON parseable back into a `CostReport` with `tokens_saved == Some(8000)`.
- `consolette cost-report ghost` (a `404` from the route, per Epic 2.3's not-found response shape) exits non-zero with stderr `error: no session found for key "ghost"` — never a zeroed table.
  - *Given* the server returns `404 {"error":"session_not_found","session_key":"ghost"}`, *When* `cost-report ghost` runs, *Then* the process exit code is non-zero and stderr contains `no session found for key "ghost"`.
- `consolette cost-report s1` when `serve-cost` isn't running (connection refused) exits non-zero with a clear "could not reach cost server at {addr}, is `consolette serve-cost` running?" message — distinguishing "server unreachable" from "session not found" (both are error states, but they mean different things to the operator: the *tool* isn't up, versus the *session* doesn't exist).
  - *Given* no server listening on the resolved address, *When* `cost-report s1` runs, *Then* the process exit code is non-zero and stderr mentions the server being unreachable, distinctly worded from the session-not-found message above.
**Files**: `src/main.rs`, `src/cost_metrics/cli_format.rs` (new), `src/cost_metrics/client.rs` (new — the `reqwest` client wrapper)

##### Task 3.1.1a: Add `CostReport { session: String, json: bool, server: Option<String> }` variant to `Command` (~3 min)
- Files: `src/main.rs`

##### Task 3.1.1b: Implement `fetch_cost_report(server: &str, session: &str) -> Result<CostReport, CostClientError>` (the `reqwest` client) (~6 min)
- `CostClientError { Unreachable(reqwest::Error), NotFound { session_key: String }, Other(reqwest::Error) }` — distinguishes the three outcomes the CLI's ACs above depend on (connection failure vs. `404` vs. any other non-2xx/deserialization failure).
- Files: `src/cost_metrics/client.rs`

##### Task 3.1.1c: Implement `cost_report_command` handler dispatching to `fetch_cost_report` (~4 min)
- On `Err(NotFound {..})`: `eprintln!("error: no session found for key ...")` + exit `1`. On `Err(Unreachable(_))`: `eprintln!("error: could not reach cost server at ..., is `consolette serve-cost` running?")` + exit `1`. On `Ok(report)`: dispatch to table or JSON formatter.
- Files: `src/main.rs`

##### Task 3.1.1d: Implement `format_cost_report_table(&CostReport) -> String` (hand-formatted, aligned) (~5 min)
- Files: `src/cost_metrics/cli_format.rs`

##### Task 3.1.1e: Implement `format_cost_report_json(&CostReport) -> String` (thin `serde_json::to_string_pretty` wrapper) (~2 min)
- Files: `src/cost_metrics/cli_format.rs`

##### Task 3.1.1f: Unit tests for table formatting (percentage math, `$` formatting, missing-price `None` rendering as `"unavailable"` not `$0.00`) (~5 min)
- Files: `src/cost_metrics/cli_format.rs` (`#[cfg(test)]`)

##### Task 3.1.1g: Client tests against the hand-rolled mock HTTP server (Task 1.2.2c's `test_support.rs`) covering all three `CostClientError` outcomes (~6 min)
- Reuses the same hand-rolled axum mock server infrastructure from Story 1.2.2 rather than introducing a second HTTP-mocking approach (keeps Blocker 11's "no `wiremock`" decision consistent across the whole plan).
- Files: `src/cost_metrics/client.rs` (`#[cfg(test)]`)

---

### Epic 3.2: HTTP JSON endpoint
**Goal**: `GET /v1/cost/{session_key}` on the Epic 2.3 server, returning the shared `CostReport` as JSON — this is now the *only* source of a `CostReport`; the CLI (Epic 3.1) is a client of it, not an independent producer.

#### Story 3.2.1: `handler_cost_report`
**As an** operator (or a script), **I want** `GET /v1/cost/{session_key}` to return `CostReport` as JSON, **so that** the CLI (via `reqwest`) and any other client agree by construction — there is exactly one place `report_for_session` is called against live data.
**Acceptance Criteria**:
- `handler_cost_report(State(tracker): State<Arc<CostTracker>>, Path(session_key): Path<String>) -> impl IntoResponse` calls `tracker.report_for_session(&SessionKey::new(session_key))` and returns `(StatusCode::OK, Json(report))` on `Ok`, `(StatusCode::NOT_FOUND, Json(json!({"error":"session_not_found","session_key": session_key})))` on `Err(SessionNotFound)` — mirrors `src/memory/mod.rs::handler_memory_get`'s exact not-found response shape/pattern.
  - *Given* a `CostTracker` with the `Reconciled` `Full`-tier session from Epic 3.1's example, *When* `GET /v1/cost/s1` is requested, *Then* the response is `200` with JSON body containing `"tokens_saved":8000`.
  - *Given* the same tracker, *When* `GET /v1/cost/ghost` is requested, *Then* the response is `404` with JSON body `{"error":"session_not_found","session_key":"ghost"}`.
**Files**: `src/cost_metrics/server.rs`

##### Task 3.2.1a: Implement `handler_cost_report` (~5 min)
- Files: `src/cost_metrics/server.rs`

##### Task 3.2.1b: Register the route in Epic 2.3's `Router` (if not already done in Task 2.3.1b — verify) (~2 min)
- Files: `src/cost_metrics/server.rs`

##### Task 3.2.1c: Handler-level unit test using axum's `oneshot`/test utilities (not a full server bind) (~5 min)
- Files: `src/cost_metrics/server.rs` (`#[cfg(test)]`)

##### Task 3.2.1d: Integration test: CLI's `reqwest` client and the axum handler, hit against the same running server, return byte-identical JSON (~5 min)
- **repair iteration 1**: since the CLI is now a `reqwest` client of this route (Epic 3.1) rather than an independent `report_for_session` caller, "the two surfaces agree" is now a structural property, not something a test needs to prove by comparing two separately-computed values. This test instead guards against a *future* regression reintroducing an independent CLI-side computation: bind the real server (Task 2.3.1e's harness), call `fetch_cost_report` (Task 3.1.1b) against it, and separately issue a raw `GET` with `reqwest`, and assert both deserialize to identical `CostReport` values — proving the CLI client isn't doing any extra transformation beyond deserialization.
- Files: `src/cost_metrics/server.rs` (`#[cfg(test)]`) or a new `tests/cost_metrics_integration.rs`

---

## Phase 4: End-to-end verification

### Epic 4.1: Full-vs-Off synthetic-session success metric
**Goal**: Directly prove requirements.md's stated success metric end to end.

#### Story 4.1.1: Two synthetic sessions, `Full` vs `Off`, materially different token savings and cost
**As the** feature's acceptance test, **I want** one test driving `SessionCompactionPipeline::apply` across a realistic multi-turn history at `Full` tier and another at `Off` tier, then reading both sessions' `CostReport`s, **so that** the comparison is proven directionally trustworthy end to end, not just unit-tested piecewise.

**repair iteration 1 (was arch B4)**: acceptance criteria updated to use the corrected `tokens_saved = counterfactual_est − compacted_est` definition (both sides same-estimator) rather than the previous `counterfactual − actual` mixed-methods subtraction; `Off`-tier is now asserted to be *exactly* `0`, not just "lower," since `Off` means `out == messages` by construction.
**Acceptance Criteria**:
- Given the same growing multi-turn message history (e.g. 20 turns, escalating pressure) applied once with `pressure_pct` fixed to always select `CompactionTier::Full` and once fixed to always select `CompactionTier::Off`, with each `apply()` call's `RequestId` synthetically reconciled via `record_actual_usage` using deterministic fake `usage.input_tokens`/`usage.output_tokens` proportional to the (possibly-compacted) message size actually sent — the `Full` session's `report_for_session().tokens_saved` is materially greater than zero (assert a meaningful margin, e.g. at least 30% of `counterfactual_tokens`), while the `Off` session's `tokens_saved` is exactly `Some(0)`.
  - *Given* `session_full` processed at `pressure_pct=0.95` for 20 turns and `session_off` processed at `pressure_pct=0.10` for the identical 20 turns, *When* both sessions' `report_for_session` is called, *Then* `session_full.tokens_saved.unwrap() as f64 >= session_full.counterfactual_tokens.unwrap() as f64 * 0.3` and `session_off.tokens_saved == Some(0)`.
- `session_full.actual_tokens` (from the synthetically-reported `usage.*`, ground truth) is also materially lower than `session_off.actual_tokens`, as a sanity cross-check that the same-estimator `tokens_saved` figure and the real billed-token figure point the same direction (they are not required to match numerically — different measurement methods — only to agree in sign/magnitude order).
  - *Given* the same two sessions, *When* both reports are read, *Then* `session_full.actual_tokens.unwrap() < session_off.actual_tokens.unwrap()`.
**Files**: `tests/cost_metrics_end_to_end.rs` (new — `tests/` is an established integration-test location in this repo already, per `tests/toml_parity.rs`; repair iteration 1 removes the previous draft's hedge about checking whether the convention exists)

##### Task 4.1.1a: Build the synthetic 20-turn message-history fixture with escalating size (~5 min)
- Reuse/adapt any existing test fixture generator from `src/session_compaction/mod.rs`'s own test module if one exists (check before writing a new one).
- Files: `tests/cost_metrics_end_to_end.rs`

##### Task 4.1.1b: Drive `apply()` + `record_actual_usage` for the `Full`-tier session (~5 min)
- Files: `tests/cost_metrics_end_to_end.rs`

##### Task 4.1.1c: Drive `apply()` + `record_actual_usage` for the `Off`-tier session (~5 min)
- Files: `tests/cost_metrics_end_to_end.rs`

##### Task 4.1.1d: Assert the material-difference comparison and print both reports on failure for debuggability (~4 min)
- Files: `tests/cost_metrics_end_to_end.rs`

### Epic 4.2: Concurrency and reconciliation edge cases
**Goal**: Regression-guard every failure mode pitfalls.md flagged, beyond the unit tests already embedded in Phase 1/2 stories.

#### Story 4.2.1: TTL eviction and not-found vs. zero-savings distinguishability
**As an** operator, **I want** an expired/evicted session to read distinctly from a zero-savings session, **so that** I never misinterpret silent data loss as "compaction saved nothing" (ux.md's core distinguishability requirement).
**Acceptance Criteria**:
- A session whose `moka` cache entry has been evicted (simulated via a `SessionCostStore` built with a near-zero TTL in the test, then waiting past it) returns `Err(SessionNotFound)` from `report_for_session` — identical error shape to a session that was never seen at all, both distinct from a `Reconciled`/`Off`-tier `tokens_saved: Some(0)` report.
  - *Given* a `SessionCostStore` with `time_to_live(Duration::from_millis(50))`, a `record_pending` call, then a 100ms sleep, *When* `report_for_session` is called, *Then* it returns `Err(SessionNotFound)`, matching the never-seen case's return type exactly.

**repair iteration 1**: this is a store-level regression guard on the *read* path (`report_for_session`) distinct from Task 1.3.2g's eviction test, which guards the *write* path (`record_actual_usage` on an evicted key must not fabricate a zeroed row). Not a duplicate — kept as separate acceptance criteria on separate call paths.
**Files**: `src/cost_metrics/tracker.rs` (`#[cfg(test)]`)

##### Task 4.2.1a: Add a `SessionCostStore::new_with_ttl` test-only constructor (~3 min)
- Files: `src/cost_metrics/store.rs`

##### Task 4.2.1b: Write the TTL-eviction test above (~5 min)
- Files: `src/cost_metrics/tracker.rs` (`#[cfg(test)]`)

##### Task 4.2.1c: Write a companion test asserting `Off`-tier zero-savings returns `Ok(report)` with `tokens_saved: Some(0)`, not `Err` (contrast case, guards against future regressions conflating the two) (~4 min)
- Files: `src/cost_metrics/tracker.rs` (`#[cfg(test)]`)

---

## Summary of scope guardrails for the implementer

- Do not wire `providers`/`routing`/`session_compaction` into a live end-to-end proxy — Epic 2.3's server hosts exactly one route.
- Do not derive `SessionKey` from real client traffic — remains a caller-supplied opaque key throughout.
- Do not let any `CostAmountUsd`-bearing field default to `$0.00` when pricing is unknown — always `None`/"unavailable".
- Do not store per-request history unbounded — the `MAX_RECORDS_PER_SESSION` ring plus per-tier running totals is the only allowed shape.
- Do not compute the Anthropic-side counterfactual with tiktoken — use `AnthropicCountTokensEstimator` exclusively for Anthropic-model sessions; tiktoken is OpenAI-side only.
- **(repair iteration 1)** `serve-cost` is the *only* process allowed to construct a `SessionCompactionPipeline` with `CostTrackingHook` registered (ADR-012 Amendment 1). Do not add a second construction site (e.g. inside the CLI, or a test-only "just for this command" pipeline) — that reintroduces the exact "nothing wires the tracker into anything observable" defect this repair fixed.
- **(repair iteration 1)** `consolette cost-report` must remain a `reqwest` HTTP client of `serve-cost`'s `/v1/cost/{session_key}` route and must never construct its own in-process `CostTracker`. If a future change needs the CLI to work without a running `serve-cost` process, that is a new design decision requiring its own ADR — not a silent reversion inside this feature's implementation.
