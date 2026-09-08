# Implementation Plan: openrouter-routing

**Feature**: `UpstreamKind::Openrouter` + auto-discovered free-model pool +
a new composite-scoring `RoutingStrategy` that live-routes among them by
rolling latency, rolling error rate, and a static coding-benchmark rank.
**Date**: 2026-09-05
**Status**: Ready for implementation
**ADRs**:
- [ADR-001: Model-list cache — `moka::sync::Cache`, not hand-rolled, not `future::Cache`](../decisions/ADR-001-model-list-cache-moka-sync.md)
- [ADR-002: Per-model 429s fold into rolling error rate](../decisions/ADR-002-per-model-429-handling.md)
- [ADR-003: Composite scoring formula — weighted min-max sum + epsilon-greedy](../decisions/ADR-003-composite-scoring-formula.md)

---

## Step 0.5 — Alternatives Explored

1. **Generic `UpstreamKind::Openai { base_url }` + one route-upstream entry
   per free model, hand-listed.** *Strength*: zero new code. *Weakness*: no
   auto-discovery — the entire point of this feature is to stop hand-editing
   config as OpenRouter's free lineup churns (`requirements.md` Alternatives
   Considered). Rejected.
2. **A new `UpstreamKind::Openrouter` + `OpenrouterProvider`, with
   discovery/caching/scoring all built into the provider itself** (no new
   `RoutingStrategy`). *Strength*: fewer moving parts, no `RoutingStrategy`
   trait changes. *Weakness*: violates ADR-003
   (`project_plans/consolette/decisions/ADR-003-routing-strategy-trait.md`)'s
   separation of health/selection/dispatch — selection logic belongs in a
   `RoutingStrategy`, not smuggled into a `Provider::send()`, and it would
   make this feature's scoring untestable in isolation from HTTP mocking.
   Rejected.
3. **`OpenrouterProvider` (thin, mechanical, follows the Gemini precedent)
   + a new `OpenrouterScoringStrategy` behind the existing `RoutingStrategy`
   trait, with two small new shared structures (a per-model stats map, a
   model-list cache) injected into both.** *Strength*: keeps `Provider` and
   `RoutingStrategy` doing exactly what ADR-003 assigns them, reuses
   `DurationHistogram`/`moka`/`rand::thread_rng` verbatim, additive-only
   changes to shared code (`RoutingStrategy` trait, `Router::dispatch`).
   *Weakness*: more files/wiring than option 2. **Chosen** — it's the only
   option that doesn't fight the codebase's existing architecture.

---

## Domain Glossary

| Term | Definition | Notes |
|------|-----------|-------|
| `UpstreamKind::Openrouter` | New config-schema variant (`kind = "openrouter"`) for an upstream that proxies to OpenRouter's OpenAI-compatible API. | `src/config/schema.rs`. No fields — base URL is hardcoded (matches `Anthropic`/`Gemini`'s precedent, unlike `Openai`'s configurable `base_url`). |
| `OpenrouterProvider` | `Provider` impl for OpenRouter: `send()` forwards Chat-Completions-shaped requests, `list_models()` lists all reported models, `list_free_models()` lists only price==0 ones. Owns `model_cache: Arc<ModelListCache>` and is responsible, as an invariant of its own `new()` (not of any particular `Strategy`), for eagerly populating and then background-refreshing it — see Story 2.1.2 and Pattern Decisions "Cache-population lifecycle ownership" (architecture-review Blocker 1 fix). Also performs a per-dispatch price recheck and post-hoc nonzero-cost detection in `send()` — see Story 1.2.4 (architecture/adversarial-review Blocker 2 fix). | `src/providers/openrouter/mod.rs`. |
| `ModelListCache` | Single-entry `moka::sync::Cache<(), Arc<Vec<FreeModelEntry>>>` holding the current free-model list as `(id, price_prompt, price_completion)` entries (not bare ids — see Story 2.1.1, Blocker 2 fix), with TTL, manual `invalidate()`, an on-demand single-flight refetch trigger on real invalidation (Story 2.1.3, Blocker 5 fix), and the data-policy-vs-staleness distinguishing logic. Shared (same `Arc`) between `OpenrouterProvider` and `OpenrouterScoringStrategy`; holds a `Weak<OpenrouterProvider>` (set via `Arc::new_cyclic` in `OpenrouterProvider::new()`) so it can trigger its own refetch without the strategy or router orchestrating it. | `src/providers/openrouter/cache.rs`. See ADR-001. |
| `FreeModelEntry` | `{ id: String, price_prompt: f64, price_completion: f64 }` — one entry of the cached free-model list. Carries price alongside id so `OpenrouterProvider::send()` can verify "this specific selected model is still priced `(0.0, 0.0)`" against the cache snapshot, not just "this id is still present" (architecture-review Blocker 2 / adversarial-review Blocker 2). | `src/providers/openrouter/cache.rs`. |
| free-model list | The `Vec<FreeModelEntry>` of OpenRouter models whose `pricing.prompt`/`pricing.completion` were `"0"` at last cache populate, carrying those price fields forward (not discarded after the filter). | See `FreeModelEntry` above. |
| `RoutingStrategy::expand_candidates` | New default-identity trait method. Runs once per `Router::dispatch` call, before the health filter, transforming the static candidate list. `OpenrouterScoringStrategy` overrides it to fan the one static "openrouter" `UpstreamRef` out into one per currently-cached free model. | `src/routing/strategy.rs`. |
| `RoutingStrategy::record_outcome` | New default-no-op trait method. Called by `Router::dispatch` once per attempt, after `provider.send()`'s `.await` resolves. Feeds `OpenrouterScoringStrategy`'s per-model stats. | `src/routing/strategy.rs`. |
| `RoutingStrategy::observability_snapshot` | New default-`None` trait method returning a strategy-specific JSON blob for `/metrics`. `OpenrouterScoringStrategy` returns model-list cache state + per-model score breakdowns. | `src/routing/strategy.rs`. |
| `ModelStats` | Per-model rolling state: a `DurationHistogram` (latency, reused unmodified) + a `RollingErrorRate` (new, error half). Keyed by model id string in a `DashMap` owned by `OpenrouterScoringStrategy`. | `src/routing/model_stats.rs`. |
| `RollingErrorRate` | New small type: `Mutex<VecDeque<(Instant, bool)>>` over a 15-minute window, mirroring `DurationHistogram`'s shape/`PoisonError` recovery discipline. `error_rate()` returns `Option<f64>` (`None` = cold start, not `0.0`). | `src/routing/model_stats.rs`. |
| `BENCH_TABLE` | Static `&[(&str, f64)]` of `(model id, aider-polyglot pass-rate %)`, hand-copied with a source-URL + retrieval-date comment. | `src/routing/bench_table.rs`. |
| `OpenrouterScoringStrategy` | The new `RoutingStrategy` impl: composite-scores per-model candidates and selects via epsilon-greedy (ADR-003). Owns `model_stats: DashMap<String, ModelStats>`, `model_cache: Arc<ModelListCache>`, `openrouter_index: usize`, `last_scores: DashMap<String, ScoreBreakdown>`. | `src/routing/openrouter_scoring.rs`. |
| `ScoreBreakdown` | Per-model snapshot of the last-computed `{latency_p50_ms, error_rate, bench_rank, composite, sample_count}`, kept for `/metrics` auditability. | `src/routing/openrouter_scoring.rs`. |
| `Strategy::OpenrouterScored` | New config-schema `Strategy` variant (`strategy = "openrouter_scored"`) selecting `OpenrouterScoringStrategy` in `Router::from_config`. | `src/config/schema.rs`. |
| `already_tried` | `Router::dispatch`'s per-dispatch dedup set, widened from `HashSet<usize>` to `HashSet<(usize, Option<String>)>` so a failed attempt against one free model doesn't drop every other free model sharing the same upstream index. | `src/routing/router.rs`. |
| data-policy 404 | An OpenRouter account-wide setting toggle (Settings → Privacy) that makes *every* free model 404 simultaneously — must NOT trigger cache invalidation (refetching won't fix it). Distinguished from genuine catalog staleness (one model 404s) by counting distinct failing model ids within a short window against the cached list size. | See `ModelListCache::record_not_found_and_maybe_invalidate`, Story 2.1.3. |

---

## Pattern Decisions

| Component | Pattern Chosen | Source | Alternative Rejected | Reason |
|-----------|---------------|--------|---------------------|--------|
| Model-list cache | `moka::sync::Cache<(), Arc<Vec<FreeModelEntry>>>` (price carried alongside id — see ADR-001 amendment) | ADR-001 (resolves `stack.md` vs `build-vs-buy.md`) | Hand-rolled `Mutex<Option<(Instant, Vec<ModelInfo>)>>` (`stack.md`); `moka::future::Cache` (matching the 5 other in-repo usages); bare `Vec<String>` of ids (original plan, revised per architecture/adversarial-review Blocker 2) | `build-vs-buy.md` is authoritative per the architecture-review checklist's consistency criterion: `moka` is already a dependency and the established idiom, avoiding reinventing TTL/eviction/single-flight. `sync::Cache` (not `future::Cache`) because `RoutingStrategy::expand_candidates`/`record_outcome` must stay synchronous (ADR-003 of the base consolette ADR set), and only `sync::Cache` exposes non-async `get`/`invalidate`. Bare ids were rejected because they can't support a per-dispatch "is *this* model still actually free" recheck — only "is this id still on the list." |
| Per-model 429 handling | Fold into `RollingErrorRate` via synthetic failure weight | ADR-002 | Sibling `DashMap`-based hard-exclusion registry keyed by model id, mirroring `HealthRegistry` | OpenRouter's free-tier rate limits are account-wide, not per-model (`research/pitfalls.md`); the existing whole-upstream `HealthRegistry.trip(chosen.index, ..)` already cools down every free model with full `Retry-After` fidelity for free, since all per-model `UpstreamRef`s share one index. A sibling registry would solve a problem that doesn't match reality while adding exactly the "general per-upstream-metrics refactor" `requirements.md`'s Rabbit Holes warns against. |
| Composite scoring formula | Weighted min-max sum (`0.5·error + 0.3·latency + 0.2·bench`) + epsilon-greedy (ε=0.1) selection | ADR-003 | Z-score normalization; equal weights; pure argmax (no exploration); softmax/weighted-sampling over all candidates | Min-max keeps every component in `[0,1]` so "worst never beats best" holds by construction, not convention. Error-rate weighted highest because a failing attempt on a rate-capped free pool both wastes budget and needs a retry. Epsilon-greedy is the concrete, fixed answer to the explore/exploit risk `features.md` flags, without building a general configurable weighting system. |
| Selection strategy structure (GoF) | **Strategy pattern**, extending the existing `RoutingStrategy` trait (already Strategy-shaped per ADR-003) with 3 new default-implemented methods | GoF (Strategy) | A special-cased `if let Some(openrouter_strategy) = ...` branch inside `Router::dispatch` | The trait is already the seam for "pluggable selection policy, shared dispatch loop" (ADR-003). Adding default-no-op methods keeps `FallbackStrategy`/`WeightedStrategy` and all their existing tests completely unaffected — verified by Story 3.1.1's regression test, not just assumed. |
| Sharing `ModelListCache` between `OpenrouterProvider` and `OpenrouterScoringStrategy` | `build_providers` additionally returns `HashMap<usize, Arc<OpenrouterProvider>>` (concrete-typed, index-keyed) alongside its existing `Vec<(String, Arc<dyn Provider>)>` | Type-driven design (avoid an unsound "erase early, downcast later" shape) | `dyn Any`-downcasting via a new `Provider::as_any()` trait method | `as_any()` would require touching all 4 existing `Provider` impls plus ~8 test-double impls across `router.rs`/`observability.rs`/`messages.rs`/`chat_completions.rs` — a much larger, unrelated blast radius than widening one function's return type, whose only 2 external call sites (`src/main.rs:198`, `src/entrypoint/api.rs:50`) each need a one-line destructure change. |
| `ModelStats`/`RollingErrorRate` value objects (type-driven design) | Small owned structs, not primitives threaded through function signatures | Type-driven design | Passing `(u64, f64)` tuples around for latency/error-rate | Matches the existing `DurationHistogram` precedent exactly (a struct with `record()`/query methods, not a bag of primitives) — consistency with the codebase's established shape for rolling metrics state. |
| Model id representation (type-driven design) | Plain `String`, no `ModelId` newtype | Type-driven design | `struct ModelId(String)` newtype | The codebase already uses raw `String` for model ids everywhere they cross the `Provider`/`UpstreamRef` boundary (`ModelInfo.id`, `RouteUpstreamRef.model`); a newtype introduced only at this one new boundary would need conversions at every existing call site it touches without enforcing any invariant beyond non-emptiness, which the `/models` parse step already guarantees. |
| `ModelStats` GC/eviction | `DashMap::retain` against the latest cache snapshot's id set, run once per `expand_candidates` call | — | Time-based eviction (drop entries not seen in N hours) | `HealthRegistry` never needs eviction (config-static upstreams are bounded); this feature's model set churns, so it needs *some* policy (`research/pitfalls.md`). Retain-on-refresh is simpler than tracking last-seen timestamps per entry; the accepted tradeoff (losing a model's history if it transiently drops out of one refresh) is minor for a single-user tool and explicitly noted, not hidden. |
| Background refresh task lifecycle | `Weak<ModelListCache>` + `Weak<OpenrouterProvider>`, upgraded each loop iteration; loop exits when either upgrade fails | — | Fire-and-forget `Arc` clones (task runs for process lifetime even after a route hot-swap orphans its `Router`) | A route hot-swap (`api::post_route`) rebuilds the `Router` via `from_config`, which would otherwise spawn a *new* background task on every reload while the old one keeps running forever against an orphaned cache/provider — a real, if slow, resource leak. `Weak` upgrade failure is the standard idiom for "stop when nobody's listening anymore" and costs nothing extra to add. |
| Cache-population lifecycle ownership (architecture-review Blocker 1) | Eager refresh + background-refresh-task spawn are an invariant of `OpenrouterProvider::new()` itself, built via `Arc::new_cyclic` so the cache can hold a `Weak<OpenrouterProvider>` handle — **plus** a symmetric config-validation rule (Story 4.3.1) rejecting any route that references an `openrouter`-kind upstream under a `Strategy` other than `OpenrouterScored` | Type-driven design (make the safety property a fact about construction, not about which match arm runs) | Leaving population solely inside `Router::from_config`'s `OpenrouterScored` match arm (original plan) | The original design made money-safety population conditional on which `Strategy` happened to reference the upstream — an `openrouter`-kind upstream used under `Strategy::Fallback`/`Weighted` never populated its cache, so `OpenrouterProvider::send()`'s documented "`None` snapshot falls through, real API call is the source of truth" behavior silently forwarded an unfiltered model string to OpenRouter. Moving population into the provider's own constructor closes the gap structurally (every `openrouter`-kind upstream is always safe, regardless of strategy); the config-validation rule is defense-in-depth so a `Fallback`/`Weighted` route referencing an `openrouter` upstream is still rejected loudly at load time rather than silently "working" in a way nothing tests for. This also incidentally shrinks `Router::from_config`'s `OpenrouterScored` arm, addressing the architecture review's separate SRP Concern for free. |
| Money-safety per-dispatch backstop (architecture-review Blocker 2 / adversarial-review Blocker 2) | Cache stores `FreeModelEntry{id, price_prompt, price_completion}` tuples, not bare ids; `send()` rechecks the *specific selected model's* price against the cache snapshot (not just membership); a post-hoc nonzero-cost signal in OpenRouter's response (if the API exposes one — Story 1.2.4) hard-invalidates the cache immediately and logs at `error` level | `research/pitfalls.md` §2's explicit "belt-and-suspenders re-check" ask | Relying solely on the 15-minute TTL bound (original plan) | A model going free→paid mid-TTL produces a successful, billed response — no error, so nothing else in the design catches it before this fix. The TTL bound only limits *how long* that exposure lasts, it doesn't detect it happening. The price-tuple recheck is real but limited (it only re-verifies against the same cached data the membership check already trusted); the post-hoc cost-field check is the actual backstop that can catch a bill inside the TTL window, if OpenRouter's API surfaces one — Story 1.2.4's first task is confirming that it does, and plan.md's Risk Control section documents the residual risk explicitly if it doesn't. |

---

## Migration Plan

No data migration. This is an additive, backward-compatible config-schema
change: a new `UpstreamKind::Openrouter` enum variant and a new
`Strategy::OpenrouterScored` enum variant. Existing `conf.d/*.toml` files
that don't reference either variant parse and behave identically to today —
`deny_unknown_fields` on both enums means a config typo still fails loudly,
but no existing valid config becomes invalid. No on-disk state format
changes (the model-list cache and per-model stats are process-memory only,
matching `SessionOverrideStore`'s precedent of "deliberately not persisted
to disk").

## Observability Plan

- **Logs**:
  - `tracing::debug!` at `OpenrouterScoringStrategy::select()` naming the
    chosen model id and its `{norm_latency, norm_error, bench_score,
    composite}` breakdown (Story 5.1.4).
  - `tracing::warn!` once per model id when it's absent from `BENCH_TABLE`
    (Story 4.2.1c) — fail-soft, not a request-level error.
  - `tracing::warn!` when `ModelListCache` suppresses an invalidation
    because the model-not-found signal looks systemic (data-policy 404),
    naming the distinct-failure count vs. cached-model count (Story 2.1.3b).
- **Metrics**: `to_metrics_json`'s new `openrouter_scoring` block
  (Story 5.1.1/5.1.2) — per-model `{latency_p50_ms, error_rate, bench_rank,
  composite_score, sample_count}` plus model-list cache state
  (`{cached_model_count, age_secs, last_refresh, last_invalidation_reason}`).
  `RequestDetail.selected_model` (Story 5.1.3) records which model a given
  request actually dispatched to.
- **Alerts**: no new alerting/oncall condition — single-user local proxy
  (matches `research/ux.md`).

## Risk Control

- **Feature flag**: not gated. Opt-in by construction — nothing changes
  until Tyler adds an `openrouter`-kind upstream and a route using
  `strategy = "openrouter_scored"` in his own `conf.d/*.toml`.
- **Rollback procedure**: remove the `openrouter` upstream and its route
  from config. No migration or persisted data to roll back (see Migration
  Plan). If a bad deploy is already running, reverting the binary is also
  sufficient — no on-disk schema to downgrade.
- **Staged rollout**: full rollout on merge — single-user tool, no cohorts.
- **Money-safety backstop (architecture-review Blocker 2 / adversarial-review
  Blocker 2)**: staying within the free-model pool is layered across three
  mechanisms, not the 15-minute TTL alone:
  1. Config-load-time + construction-time: an `openrouter`-kind upstream can
     only be dispatched to under `Strategy::OpenrouterScored` (Story 4.3.1's
     symmetric validation), and its cache is always populated as an
     invariant of `OpenrouterProvider::new()` (Story 2.1.2) — closes the
     "cache never populated, unfiltered model forwarded" gap entirely, not
     just bounds it.
  2. Per-dispatch: `send()` verifies the *specific selected model's* cached
     price is `(0.0, 0.0)`, not just that its id is present (Task 2.1.2c) —
     defense-in-depth against a future filter bug, not a mid-TTL fix by
     itself.
  3. Post-hoc: if OpenRouter's response exposes a per-request cost/usage
     field, a nonzero value for a nominally-free-routed request hard-
     invalidates the cache immediately and logs at `error` level (Story
     1.2.4) — this is the actual backstop for the free→paid-mid-TTL case
     `research/pitfalls.md` §2 flagged as highest-severity.
  - **Residual risk, explicitly carried, not hidden in ADR prose**: mechanism
    3 depends on OpenRouter's API actually exposing a per-request cost
    field. **Update (2026-09-08, `sdd:6-verify` Layer 4):** confirmed via
    OpenRouter's public docs (no API key needed) —
    `GET /api/v1/generation?id=<id>`
    (<https://openrouter.ai/docs/api/api-reference/generations/get-generation>)
    returns a `total_cost` (USD) field for any prior generation, keyed by the
    `id` already present on every completion response. This is cleaner than
    the `usage: {include: true}` opt-in this plan originally anticipated — no
    request-body change, so no risk of the accounting opt-in itself altering
    billing behavior. **Mechanism 3 is therefore implementable**, but is not
    yet implemented (a real feature addition — an extra HTTP round-trip per
    checked request, a sampling-vs-every-request decision, and end-to-end
    verification against a real key none of this session's environments
    had) — it is a well-scoped follow-up story now, not an open unknown. The
    free→paid-mid-TTL window remains bounded-but-not-closed (≤15 minutes /
    ≤~300 requests at 20 req/min, per ADR-001) **until that follow-up ships**,
    and shipping without it in the meantime still needs Tyler's explicit
    sign-off as an accepted interim risk — see the updated Unresolved
    Question below.

## Unresolved Questions

- [ ] Confirm the exact JSON shape of OpenRouter's `/models` pricing fields
  (expected, per Phase 2 research, to be `pricing.prompt`/`pricing.completion`
  as string `"0"` for free models, with id suffix `:free` as a cross-check)
  against a live `GET https://openrouter.ai/api/v1/models` response — blocks
  Story 1.2.2 — owner: implementer, first sub-task of that story.
- [ ] Confirm the exact JSON shape of OpenRouter's model-not-found error
  response (status code + body fields) so `OpenrouterProvider::send()`'s
  error classification maps it to `ProviderError::ModelUnsupported`
  correctly — blocks Story 1.2.1's error-mapping task — owner: implementer,
  captured against a real request in that task per its acceptance criterion.
- [ ] Verify `moka::sync::Cache`'s exact method names/builder API for 0.12.16
  (`Cache::builder()`, `.max_capacity()`, `.time_to_live()`, `.build()`,
  `.get()`, `.insert()`, `.invalidate()`) against `docs.rs/moka/0.12` or
  `cargo doc -p moka --no-deps`, and add `"sync"` to `Cargo.toml:86`'s
  `moka` feature list (currently `["future"]` only, which doesn't expose
  `moka::sync`) — blocks Story 2.1.1 — owner: implementer, first sub-task
  of that story (Task 2.1.1a).
- [ ] Populate `BENCH_TABLE` with real transcribed rows from
  https://aider.chat/docs/leaderboards/ for the free models actually
  returned by a live `/models` call, with a retrieval-date comment — blocks
  Story 4.1.1 — owner: implementer (this plan intentionally ships that file
  with placeholder/empty rows rather than fabricated numbers).
- [x] ~~Confirm whether OpenRouter's Chat Completions response (or a
  request-time `usage: {include: true}`-style opt-in, or a companion
  `/generation?id=` lookup) exposes a genuine per-request cost/usage
  field~~ — **RESOLVED 2026-09-08** (`sdd:6-verify` Layer 4, via OpenRouter's
  public docs, no API key needed): `GET /api/v1/generation?id=<id>` returns
  `total_cost` (USD) for any prior generation. See Risk Control's updated
  note above. **New follow-up, not yet a story in this plan**: implement
  Story 1.2.4's mechanism 3 for real using this endpoint — needs a live
  OpenRouter API key to verify end-to-end (none available in any environment
  this project has run in so far) and a design decision on check-every-
  request vs. sampled. — owner: Tyler/implementer, next session with a live
  key.
- [ ] Watch item, not a blocker (ADR-002): if OpenRouter turns out to apply
  any genuinely per-model throttling distinct from the account-wide cap,
  ADR-002's "fold into rolling error rate" decision should be revisited —
  no story depends on this; flagged for whoever next touches
  `OpenrouterScoringStrategy::record_outcome`.
- [ ] **Pre-mortem P2 #1 — bench-table coverage-ratio go/no-go check
  (pre-mortem.md Failure Mode #1):** before Phase 4 implementation, compute
  the actual coverage ratio (live free models with a `BENCH_TABLE` entry ÷
  total live free models, from Task 1.2.2a's confirmed list) and record it
  as an explicit go/no-go note in this plan — if it's low, either lower
  `WEIGHT_BENCH` accordingly or add one aggregate startup log line reporting
  the ratio, rather than relying only on the per-model `tracing::warn!`
  (Task 4.2.1c) — blocks Task 4.1.1a (bench-table transcription) — owner:
  implementer, at the same time Task 4.1.1a's leaderboard lookups happen
  (the ratio falls out of that same pass over the confirmed free-model
  list).
- [ ] **Pre-mortem P2 #2 — exploration-vs-misbehavior `explore` flag
  (pre-mortem.md Failure Mode #2):** add an `explore: bool` field to Story
  5.1.4's log line, `RequestDetail` (Story 5.1.3), and
  `observability_snapshot()` (Story 5.1.1) so an epsilon-greedy exploration
  dispatch is distinguishable after the fact from a genuine scoring
  failure — a single boolean, not the "general configurable weighting
  system" ADR-003 already declined to build — blocks Story 4.2.2 (`select()`,
  which knows whether it took the explore or greedy branch) and Story 5.1.1
  /5.1.3/5.1.4 (which surface it) — owner: implementer, added alongside
  Task 4.2.2a's `select()` implementation.
- [ ] **Pre-mortem P2 #3 — ADR-002's account-wide-vs-per-model rate-limit
  assumption confirmation (pre-mortem.md Failure Mode #3):** promote this
  from an unowned watch item to an owned validation task — capture at least
  one live 429 response's headers/body for a single free model under
  sustained load and confirm whether OpenRouter's error signature is
  per-model or account-wide, before or immediately after initial ship. If
  it turns out to be per-model (not account-wide), ADR-002's justification
  for rejecting a sibling per-model rate-limit registry no longer holds —
  blocks Task 4.2.3b (the 429 synthetic-weighting logic that currently
  assumes whole-upstream `HealthRegistry` cooldown is sufficient) — owner:
  implementer, captured as part of Task 4.2.3c's `Retry-After`-fidelity
  regression test's live-traffic follow-up, not left for "whoever next
  touches `record_outcome`."
  `STATUS: not completed during Phase 5 implementation — see ADR-002's
  post-implementation note`

---

## Dependency Visualization

```
Phase 1: Provider Foundation
  Epic 1.1 Config schema ──┐
  Epic 1.2 OpenrouterProvider ──┘
        │
        ▼
Phase 2: Model-List Cache
  Epic 2.1 ModelListCache (moka::sync, TTL, background refresh,
            data-policy-vs-staleness invalidation)
        │
        ├─────────────────────────────┐
        ▼                             ▼
Phase 3: Metrics Infrastructure   Phase 4: Bench Table
  Epic 3.1 RoutingStrategy trait    Epic 4.1 BENCH_TABLE
           additions + already_tried        │
           widening                         │
  Epic 3.2 ModelStats/RollingErrorRate       │
        │                                   │
        └───────────────┬───────────────────┘
                         ▼
              Phase 4 (cont.): Scoring Strategy
                Epic 4.2 OpenrouterScoringStrategy
                         (composite formula, ADR-002/ADR-003,
                          expand_candidates, record_outcome)
                Epic 4.3 Strategy::OpenrouterScored wiring
                         │
                         ▼
              Phase 5: Observability
                Epic 5.1 to_metrics_json + RequestDetail +
                         structured log line
```

Epics 1.1/1.2 can proceed in either order within Phase 1 (config schema has
no code dependency on the provider, but the provider needs the schema
variant to compile) — sequenced 1.1 → 1.2 below for that reason. Phase 3's
two epics are independent of each other and of Phase 4's Epic 4.1; both must
land before Epic 4.2, which is the first piece that needs the trait
additions, `ModelStats`, *and* the bench table simultaneously.

---

## Phase 1: OpenRouter Provider Foundation

### Epic 1.1: Config Schema

**Goal**: `UpstreamKind::Openrouter` and `Strategy::OpenrouterScored` exist,
parse, and round-trip, with zero effect on existing configs.

#### Story 1.1.1: Add `UpstreamKind::Openrouter` and `Strategy::OpenrouterScored`
**As a** Tyler configuring `conf.d/*.toml`, **I want** an `openrouter` kind
and an `openrouter_scored` strategy, **so that** I can declare an OpenRouter
upstream and route to it via the new scoring strategy without any bespoke
config path.

**Acceptance Criteria**:
- A TOML fragment `[[upstreams]]\nname = "openrouter"\nkind = "openrouter"`
  deserializes to `Upstream { kind: UpstreamKind::Openrouter, .. }`.
  - *Given* the TOML fragment above with no other fields, *when* it's parsed
    via `Config`'s existing `figment` loader, *then* deserialization
    succeeds and `upstream.kind == UpstreamKind::Openrouter`.
- An unknown field under `kind = "openrouter"` (e.g. a stray `base_url`)
  fails to parse, matching every other `UpstreamKind` variant's
  `deny_unknown_fields` behavior.
  - *Given* `[[upstreams]]\nname = "openrouter"\nkind = "openrouter"\nbase_url = "https://x"`,
    *when* parsed, *then* deserialization returns an error naming the
    unknown field `base_url`.
- `strategy = "openrouter_scored"` deserializes to `Strategy::OpenrouterScored`.
  - *Given* `[[routes]]\nname = "default"\nstrategy = "openrouter_scored"`,
    *when* parsed, *then* `route.strategy == Strategy::OpenrouterScored`.
- Every existing conf.d fixture/test still parses unchanged (no variant
  reordering breaks existing serialized fixtures, if any exist).

**Files**: `src/config/schema.rs`

##### Task 1.1.1a: Add `UpstreamKind::Openrouter` variant (~2 min)
- In the `UpstreamKind` enum (`src/config/schema.rs:107-123`), add
  `Openrouter,` as a new unit variant (no fields — base URL is hardcoded in
  the provider, matching `Anthropic`).
- Files: `src/config/schema.rs`

##### Task 1.1.1b: Add `Strategy::OpenrouterScored` variant (~2 min)
- In the `Strategy` enum (`src/config/schema.rs:140-145`, currently
  `#[serde(rename_all = "lowercase")]` with `Fallback`/`Weighted`), add
  `#[serde(rename = "openrouter_scored")] OpenrouterScored,`.
- Files: `src/config/schema.rs`

##### Task 1.1.1c: Unit tests for both new variants (~4 min)
- Add `#[test]` cases (near existing `UpstreamKind`/`Strategy`
  (de)serialization tests in `src/config/schema.rs`'s `#[cfg(test)]` module,
  if present, else at the bottom of the file) covering: successful parse of
  `kind = "openrouter"`; rejection of an unknown field under it; successful
  parse of `strategy = "openrouter_scored"`.
- Files: `src/config/schema.rs`

---

### Epic 1.2: `OpenrouterProvider`

**Goal**: A working, mechanical `Provider` impl for OpenRouter — `send()`
forwards Chat-Completions requests, `list_models()`/`list_free_models()` do
real `/models` fetches — wired into `build_providers` and
`upstream_kind_label`, following the Gemini precedent exactly.

#### Story 1.2.1: `OpenrouterProvider::new`/`send()`
**As a** Tyler with an `openrouter`-kind upstream configured, **I want**
requests routed to it to reach OpenRouter's real Chat Completions endpoint
with correct auth and headers, **so that** I get real completions from
OpenRouter's models.

**Acceptance Criteria**:
- `OpenrouterProvider::new` is `async fn new(..) -> anyhow::Result<Arc<Self>>`
  (returns an `Arc`, not a bare `Self` — a deliberate divergence from the
  other providers' constructor shape, required so the struct can be built
  via `Arc::new_cyclic` and hand its own `model_cache` a `Weak<Self>` handle;
  see Story 2.1.2). It builds two `reqwest::Client`s (pooled + streaming),
  matching `OpenaiProvider`/`GeminiProvider`'s two-client shape, **and**
  (per Story 2.1.2's Blocker-1 fix) eagerly populates `model_cache` and
  spawns its background refresh task before returning — cache population is
  an invariant of construction, not of any particular `Strategy` choosing to
  use it.
  - *Given* a `Upstream { name: "openrouter", kind: Openrouter, auth: Some(Bearer{..}) }`,
    *when* `OpenrouterProvider::new(upstream, resolver, exec_cache, timeout).await`
    is called, *then* it returns `Ok(Arc<OpenrouterProvider>)` with both
    clients constructed and `model_cache.snapshot()` already attempted
    (`Some(..)` if OpenRouter was reachable, `None` and logged otherwise —
    see Story 2.1.2's acceptance criteria for the exact contract).
- `send()` POSTs to `https://openrouter.ai/api/v1/chat/completions` with
  `Authorization: Bearer <resolved token>` plus static
  `HTTP-Referer`/`X-Title` headers, forwarding the request body verbatim
  (reusing `OpenaiProvider`'s translation helpers — no new translation
  logic).
  - *Given* a resolved bearer token `"sk-or-v1-test"` and a request body
    `{"model": "deepseek/deepseek-chat-v3.1:free", "messages": [...]}`,
    *when* `send(body, headers, false)` is called against a mock server,
    *then* the outgoing request has `Authorization: Bearer sk-or-v1-test`,
    `HTTP-Referer` and `X-Title` set, and the same JSON body forwarded.
- A model-not-found-shaped OpenRouter error response classifies to
  `ProviderError::ModelUnsupported(model_id)` (reusing the existing enum
  variant, `src/providers/mod.rs:46` — no new variant), **not** a generic
  `ProviderError::Upstream`.
  - *Given* the mock server returns OpenRouter's real model-not-found error
    shape (captured live per this story's Unresolved Question) for model id
    `"foo/bar:free"`, *when* `send()` receives it, *then* it returns
    `Err(ProviderError::ModelUnsupported("foo/bar:free".to_string()))`.
- A rate-limit response (429) classifies to `ProviderError::RateLimitedWithRetry`
  when `Retry-After` is present, else `ProviderError::RateLimited`, matching
  `OpenaiProvider`'s existing status-mapping pattern.
  - *Given* a mock 429 response with header `Retry-After: 20`, *when*
    `send()` receives it, *then* it returns
    `Err(ProviderError::RateLimitedWithRetry { retry_after: 20 })`.

**Files**: `src/providers/openrouter/mod.rs` (new), `src/providers/mod.rs`

##### Task 1.2.1a: Capture OpenRouter's real error-response shapes (~5 min)
- Resolves this story's first Unresolved Question. Issue a live (or
  previously-captured, if Phase 2 research already has one) request to
  `POST https://openrouter.ai/api/v1/chat/completions` with an invalid
  model id and with an exhausted-quota scenario; record the exact JSON
  body shape for each in a code comment at the classification site (Task
  1.2.1d) rather than guessing.
- Files: none (research capture, feeds Task 1.2.1d's comment)

##### Task 1.2.1b: `OpenrouterProvider` struct + `new()` (~4 min)
- Mirror `OpenaiProvider`'s struct shape (`src/providers/openai.rs:38-53`):
  pooled `client`, streaming `stream_client`, `upstream: Arc<Upstream>`,
  `resolver`, `exec_cache`, plus a new `model_cache: Arc<ModelListCache>`
  field. No `base_url` field — hardcode
  `const BASE_URL: &str = "https://openrouter.ai/api/v1";` as a module
  constant instead (unlike `OpenaiProvider`, this upstream kind carries no
  configurable base URL).
- `new()` builds the struct inside `Arc::new_cyclic(|weak_self| { .. })` so
  `model_cache` can be constructed with a `Weak<Self>` back-reference (used
  by Story 2.1.2's background task and Story 2.1.3's on-demand refetch
  trigger), then — after the `Arc::new_cyclic` call returns, since its
  closure must stay sync — awaits the eager refresh and spawns the
  background task per Story 2.1.2, before returning `Ok(provider_arc)`.
- Add `pub mod openrouter;` to `src/providers/mod.rs:9-12`'s module list.
- Files: `src/providers/openrouter/mod.rs`, `src/providers/mod.rs`

##### Task 1.2.1c: `send()` — headers + forward (~4 min)
- Implement `send()` following `OpenaiProvider::send`'s shape
  (`src/providers/openai.rs` around its `send` impl in the
  `#[async_trait] impl Provider for OpenaiProvider` block, `line 304+`):
  build headers via `super::anthropic::apply_auth_headers` (same as
  `OpenaiProvider`/`GeminiProvider`), then additionally insert static
  `HTTP-Referer: https://github.com/tstapler/consolette` and
  `X-Title: consolette` headers, POST to
  `{BASE_URL}/chat/completions`.
- Files: `src/providers/openrouter/mod.rs`

##### Task 1.2.1d: Error classification (~4 min)
- Add a `classify_openrouter_error` helper (mirroring
  `gemini::error::classify_gemini_error`'s status-code branching style,
  `src/providers/gemini/error.rs:47-64`), using Task 1.2.1a's captured
  shapes: 429 → `RateLimited`/`RateLimitedWithRetry`; the model-not-found
  shape → `ModelUnsupported(model_id)`; 401/403 → `Auth`; other 4xx →
  `Validation`; 5xx → `Upstream`.
- Files: `src/providers/openrouter/mod.rs`

##### Task 1.2.1e: Unit tests for `send()`/error classification (~5 min)
- Table-driven tests over a mock HTTP server (matching the existing
  `wiremock`-or-equivalent pattern used in `openai.rs`'s/`gemini/mod.rs`'s
  own test modules) covering the 4 acceptance criteria above.
- Files: `src/providers/openrouter/mod.rs`

#### Story 1.2.2: `list_models()` / `list_free_models()`
**As** `consolette list-models` / the web control panel's `GET /api/models`,
**I want** to see every model OpenRouter reports, **and as** the new scoring
strategy, **I want** only the free ones, **so that** discovery replaces
hand-listing.

**Acceptance Criteria**:
- `list_models()` (the `Provider` trait method) GETs `{BASE_URL}/models` and
  returns every model as `ModelInfo { id, owned_by }`, unfiltered — matching
  `OpenaiProvider::list_models`'s existing shape and behavior exactly
  (same trait contract, all providers listed alongside each other).
  - *Given* a mock `/models` response with 3 models (1 free, 2 paid), *when*
    `list_models()` is called, *then* it returns all 3 as `ModelInfo`.
- `list_free_models()` (a new inherent method, not part of the `Provider`
  trait) GETs the same endpoint and returns one `FreeModelEntry{id,
  price_prompt, price_completion}` per model whose `pricing.prompt == "0"`
  and `pricing.completion == "0"` — carrying the (zero) price values forward
  rather than discarding them after the filter, so downstream consumers
  (`ModelListCache`, `OpenrouterProvider::send()`'s per-dispatch recheck —
  Blocker 2 fix) can verify price directly instead of trusting bare id
  membership.
  - *Given* the same mock response, *when* `list_free_models()` is called,
    *then* it returns `vec![FreeModelEntry { id: "<the one free model's id>".into(), price_prompt: 0.0, price_completion: 0.0 }]`.
- Both methods share the underlying HTTP GET (no duplicated request logic)
  — refactored into one private `fetch_models_raw()` helper, mirroring
  `OpenaiProvider::fetch_models`'s pattern (`src/providers/openai.rs:167-192`).

**Files**: `src/providers/openrouter/mod.rs`, `src/providers/openrouter/models.rs` (new)

##### Task 1.2.2a: Confirm live pricing-field shape (~3 min)
- Resolves this story's Unresolved Question. Issue (or reuse a Phase-2-research-captured)
  live `GET https://openrouter.ai/api/v1/models` response; confirm
  `pricing.prompt`/`pricing.completion` are strings and `"0"` denotes free,
  and that free ids carry a `:free` suffix as a cross-check signal. Record
  the confirmed shape in a doc comment on Task 1.2.2c's filter function.
- Files: none (research capture)

##### Task 1.2.2b: `fetch_models_raw()` shared helper (~3 min)
- Extract the GET-and-parse-JSON logic (mirroring
  `OpenaiProvider::fetch_models`, `src/providers/openai.rs:167-192`) into
  `src/providers/openrouter/models.rs` as `pub(super) async fn
  fetch_models_raw(provider: &OpenrouterProvider) -> Result<Value,
  ProviderError>`.
- Files: `src/providers/openrouter/models.rs`

##### Task 1.2.2c: `list_models()` + `list_free_models()` (~4 min)
- `list_models()` maps every `data[]` entry to `ModelInfo` (mirroring
  `OpenaiProvider::list_models`, `src/providers/openai.rs:340-358`).
  `list_free_models()` additionally filters on
  `entry.pricing.prompt == "0" && entry.pricing.completion == "0"` (per
  Task 1.2.2a's confirmed shape) and returns `Vec<FreeModelEntry>`
  (`{id, price_prompt: 0.0, price_completion: 0.0}` per matching entry —
  parsed from the same `pricing.prompt`/`pricing.completion` fields the
  filter already reads, not re-derived).
- Files: `src/providers/openrouter/models.rs`, `src/providers/openrouter/mod.rs`

##### Task 1.2.2d: Unit tests (~4 min)
- Mock-server tests for both acceptance criteria (mixed free/paid response;
  all-paid response returns empty free list; malformed pricing field is
  treated as not-free, not a parse error — fail-soft per `research/ux.md`).
- Files: `src/providers/openrouter/models.rs`

#### Story 1.2.3: Wire into `build_providers` / `upstream_kind_label`
**As** any code that enumerates upstreams (`consolette list-models`, the web
control panel, `Router::from_config`), **I want** `UpstreamKind::Openrouter`
handled everywhere the other 4 kinds already are, **so that** the new kind
isn't a silent gap.

**Acceptance Criteria**:
- `build_providers` constructs an `Arc<OpenrouterProvider>` for
  `UpstreamKind::Openrouter` and additionally returns a
  `HashMap<usize, Arc<OpenrouterProvider>>` (index → concrete handle) for
  every such upstream, alongside its existing
  `Vec<(String, Arc<dyn Provider>)>` return value.
  - *Given* a `Config` with one `UpstreamKind::Openrouter` upstream at index
    2, *when* `build_providers(&config)` is called, *then* the returned map
    contains `{2: Arc<OpenrouterProvider>}` and the providers vec's index-2
    entry is `("openrouter", Arc<dyn Provider>)` wrapping that same
    provider.
- `src/main.rs:198` and `src/entrypoint/api.rs:50` still compile and behave
  identically, destructuring the new tuple return and ignoring its second
  element.
- `upstream_kind_label` (`src/entrypoint/mod.rs:121-126`) returns
  `"openrouter"` for `UpstreamKind::Openrouter`.
  - *Given* `UpstreamKind::Openrouter`, *when*
    `upstream_kind_label(&kind)` is called, *then* it returns `"openrouter"`.

**Files**: `src/routing/router.rs`, `src/main.rs`, `src/entrypoint/api.rs`, `src/entrypoint/mod.rs`

##### Task 1.2.3a: Widen `build_providers`' return type (~4 min)
- Change `build_providers`'s signature
  (`src/routing/router.rs:58`) to
  `pub async fn build_providers(config: &Config) -> anyhow::Result<(Vec<(String, Arc<dyn Provider>)>, HashMap<usize, Arc<crate::providers::openrouter::OpenrouterProvider>>)>`.
  Add the `UpstreamKind::Openrouter` match arm
  (`src/routing/router.rs:64-87`) calling
  `OpenrouterProvider::new(..).await?` — note this already returns
  `Arc<OpenrouterProvider>` directly (Task 1.2.1b), not a bare `Self`, so no
  extra `Arc::new(..)` wrap is needed — pushing a clone of that `Arc` as
  `Arc<dyn Provider>` into the providers vec and inserting the same `Arc`
  into the new map keyed by the upstream's index. Because `new()` now does
  an eager cache refresh (Story 2.1.2), `build_providers` awaits it for
  every `openrouter`-kind upstream regardless of which `Strategy` any route
  ends up pairing it with — this is the Blocker-1 fix, not incidental.
- Files: `src/routing/router.rs`

##### Task 1.2.3b: Fix up the 3 call sites (~3 min)
- `Router::from_config` (`src/routing/router.rs:139-143`): destructure both
  return values, keep using the providers vec as before, thread the new map
  into strategy construction (used starting Epic 4.3).
- `src/main.rs:198` and `src/entrypoint/api.rs:50`: change
  `let providers = build_providers(&config).await?;` to
  `let (providers, _) = build_providers(&config).await?;`.
- Files: `src/routing/router.rs`, `src/main.rs`, `src/entrypoint/api.rs`

##### Task 1.2.3c: `upstream_kind_label` (~2 min)
- Add `UpstreamKind::Openrouter => "openrouter",` to the match
  (`src/entrypoint/mod.rs:121-126`).
- Files: `src/entrypoint/mod.rs`

##### Task 1.2.3d: Regression + new tests (~4 min)
- Add a `build_providers_should_construct_provider_for_upstream_kind_openrouter`
  test mirroring the existing Gemini one
  (`src/routing/router.rs:1270-1293`). Add
  `upstream_kind_label_should_return_openrouter_for_new_variant` mirroring
  the existing Gemini test (`src/entrypoint/mod.rs:251-259`). Run the full
  existing `router.rs`/`entrypoint/mod.rs` test suites to confirm no
  regression from the signature change.
- Files: `src/routing/router.rs`, `src/entrypoint/mod.rs`

#### Story 1.2.4: Post-hoc nonzero-cost detection (money-safety backstop)
**As** Tyler, **I want** a request that dispatched to a nominally-free model
but actually got billed to be caught and loudly flagged immediately, **so
that** "must not silently spend money" holds even inside the model-list
cache's TTL window — not just after the next refresh (architecture-review
Blocker 2 / adversarial-review Blocker 2: the TTL bounds this exposure, it
doesn't close it).

**Acceptance Criteria**:
- `send()`'s successful-response path inspects OpenRouter's response for a
  per-request cost/usage field (exact field name/location per Task 1.2.4a's
  research spike); when it reports a nonzero cost for a request whose
  `model` was drawn from the free-model cache, `send()` (a) calls
  `model_cache.invalidate()` immediately — not waiting for TTL or the next
  periodic refresh, (b) sets
  `last_invalidation_reason = Some("unexpected_nonzero_cost:<model>")`, and
  (c) emits `tracing::error!(model, cost, "openrouter billed a nominally-free model — cache invalidated")`
  — `error`, not `debug`/`warn`, since this is the one condition in the
  whole feature that means real money may have just been spent.
  - *Given* a request to model `"a/b:free"` (present in the free-model
    cache) whose OpenRouter response reports a nonzero cost, *when* `send()`
    processes the response, *then* `model_cache.snapshot()` afterward is
    `None` and a `tracing::error!` event was emitted naming the model and
    cost.
  - *Given* a response reporting no cost field, or `cost == 0.0`, *when*
    `send()` processes it, *then* no invalidation occurs and no `error!`
    event fires (baseline unaffected).
- If Task 1.2.4a's research spike finds no accessible cost field at all,
  this story ships as a documented no-op (the check exists in code but is
  gated on a field that doesn't exist) — the resulting residual risk is
  Tyler's sign-off decision, not an implementer decision (see plan.md Risk
  Control).

**Files**: `src/providers/openrouter/mod.rs`

##### Task 1.2.4a: Confirm whether a per-request cost field exists (~5 min)
- Resolves this story's Unresolved Question (added to plan.md's Unresolved
  Questions list). Check OpenRouter's real Chat Completions response for a
  cost/usage field (candidates to check: a request-time
  `usage: {include: true}` opt-in surfacing `usage.cost` in the response; a
  separate `GET /api/v1/generation?id=<generation_id>` lookup keyed off the
  response's generation id). Record the confirmed shape — or its absence —
  in a doc comment at Task 1.2.4b's call site.
- Files: none (research capture)

##### Task 1.2.4b: Implement the check + hard-invalidate + `error!` log (~4 min)
- In `send()`'s success path, after receiving OpenRouter's response and
  before returning it: if Task 1.2.4a confirmed an accessible cost field,
  parse it; if nonzero and the dispatched `model` came from
  `model_cache.snapshot()`, call `model_cache.invalidate()`, set
  `last_invalidation_reason`, and log per the acceptance criteria above. If
  no field exists, this becomes a documented `// no accessible per-request
  cost field as of <date>; see plan.md Story 1.2.4 / Risk Control` comment
  rather than a functioning check.
- Files: `src/providers/openrouter/mod.rs`

##### Task 1.2.4c: Unit tests (~4 min)
- Mock-server tests for both acceptance-criteria GWTs above (gated on Task
  1.2.4a's finding — if no cost field exists, this task instead asserts the
  documented no-op doesn't misfire on ordinary responses).
- Files: `src/providers/openrouter/mod.rs`

---

## Phase 2: Model-List Cache

### Epic 2.1: `ModelListCache`

**Goal**: A shared, TTL-bounded, background-refreshed, sync-readable
free-model list, with the money-safety-motivated short TTL and the
data-policy-vs-staleness distinguishing invalidation logic from
`research/pitfalls.md`.

#### Story 2.1.1: `ModelListCache` type
**As** `OpenrouterProvider` and `OpenrouterScoringStrategy`, **I want** a
shared, cheaply-sync-readable snapshot of the current free-model list, **so
that** `expand_candidates` (which must stay sync) can read it without
network I/O on the request path.

**Acceptance Criteria**:
- `ModelListCache::snapshot()` is a plain sync method returning
  `Option<Arc<Vec<FreeModelEntry>>>` (`FreeModelEntry { id: String,
  price_prompt: f64, price_completion: f64 }` — price carried alongside id,
  not a bare `Vec<String>`, per architecture-review Blocker 2 / adversarial-
  review Blocker 2: a per-dispatch price recheck needs the price, not just
  the id, to verify) — `None` before the first successful refresh, `Some(..)`
  after.
  - *Given* a freshly-constructed `ModelListCache` with nothing refreshed
    yet, *when* `snapshot()` is called, *then* it returns `None`.
  - *Given* a `ModelListCache` after `refresh()` populated 2 free models,
    *when* `snapshot()` is called, *then* it returns
    `Some(Arc::new(vec![FreeModelEntry{id:"a/b:free".into(), price_prompt:0.0, price_completion:0.0}, FreeModelEntry{id:"c/d:free".into(), price_prompt:0.0, price_completion:0.0}]))`.
- `refresh(&self, provider: &OpenrouterProvider)` is async, calls
  `provider.list_free_models()` (now returning `Vec<FreeModelEntry>` per
  Task 1.2.2c), and on success both updates the cache entry and records
  `last_refresh = Some(Instant::now())`.
- **`refresh()` failure behavior is explicit, not implicit (adversarial-review
  Concern — "serve stale until TTL" was never a stated acceptance criterion
  or test):** on `Err`, `refresh()` leaves the existing cache entry (if any)
  completely untouched — it neither clears nor replaces it, so a
  previously-populated, not-yet-expired entry keeps serving stale data until
  the TTL naturally expires it. On a cache that was already empty/expired,
  a failed `refresh()` leaves `snapshot() == None`.
  - *Given* a populated, not-yet-expired cache entry, *when* `refresh()` is
    called and `provider.list_free_models()` returns `Err`, *then*
    `snapshot()` afterward still returns the same `Some(..)` value as
    before the failed call (untouched, not cleared).
  - *Given* an empty/already-expired cache (`snapshot() == None`), *when*
    `refresh()` is called and `provider.list_free_models()` returns `Err`,
    *then* `snapshot()` afterward still returns `None`.
- The cache entry expires after 15 minutes (`MODEL_LIST_TTL`), per ADR-001's
  money-safety-bounded TTL — `snapshot()` returns `None` again once expired
  and un-refreshed.
  - *Given* a `ModelListCache` built with a 1ms TTL (test-only constructor)
    and a populated entry, *when* 5ms elapse and `snapshot()` is called,
    *then* it returns `None`.
- `invalidate()` is a plain sync method that clears the entry immediately
  (not waiting for TTL).
  - *Given* a populated cache, *when* `invalidate()` is called then
    `snapshot()` is called, *then* it returns `None`.

**Files**: `src/providers/openrouter/cache.rs` (new), `Cargo.toml`

##### Task 2.1.1a: Enable `moka`'s `sync` feature + confirm its API surface (~3 min)
- `moka`'s `sync` module is gated behind its own Cargo feature, separate
  from `future` — `Cargo.toml:86`'s
  `moka = { version = "0.12", features = ["future"] }` only exposes
  `moka::future`. Add `"sync"` to that feature list (additive; the existing
  5 `future::Cache` usages are unaffected).
- Resolves this story's Unresolved Question. Check `docs.rs/moka/0.12`
  (or `cargo doc -p moka --no-deps`) for `moka::sync::Cache`'s builder and
  method names; confirm `Cache::builder().max_capacity(1).time_to_live(dur).build()`,
  `.get(&key) -> Option<V>`, `.insert(key, value)`, `.invalidate(&key)` all
  exist with those signatures (or note the actual ones if they differ).
- Files: `Cargo.toml` (research capture for the API-surface half)

##### Task 2.1.1b: `ModelListCache` struct + `snapshot()`/`refresh()`/`invalidate()` (~5 min)
- `pub struct FreeModelEntry { pub id: String, pub price_prompt: f64, pub price_completion: f64 }`.
  Per ADR-001 (as amended): `cache: moka::sync::Cache<(), Arc<Vec<FreeModelEntry>>>`,
  `last_refresh: Mutex<Option<Instant>>`,
  `last_invalidation_reason: Mutex<Option<String>>`,
  `recent_not_found: DashMap<String, Instant>` (used starting Story 2.1.3),
  `provider: Weak<OpenrouterProvider>` (set at construction via
  `Arc::new_cyclic` in `OpenrouterProvider::new()`, Task 1.2.1b — used by
  Story 2.1.3's on-demand refetch trigger), `refresh_in_flight: AtomicBool`
  (single-flight guard, also Story 2.1.3).
  `MODEL_LIST_TTL: Duration = Duration::from_mins(15)` as a module
  constant. Add a `#[cfg(test)] new_with_ttl(ttl: Duration)` constructor
  mirroring `SessionCostStore::new_with_ttl`'s precedent
  (`src/cost_metrics/store.rs:165-175`).
- Files: `src/providers/openrouter/cache.rs`

##### Task 2.1.1c: Unit tests (~4 min)
- Cover all 4 acceptance criteria above using `new_with_ttl`.
- Files: `src/providers/openrouter/cache.rs`

##### Task 2.1.1d: `refresh()` failure-behavior tests (~4 min)
- Resolves the adversarial-review Concern that `/models` refresh-failure
  behavior was implicit. `refresh()`'s implementation (Task 2.1.1b) must
  leave `cache`/`last_refresh` unmodified when
  `provider.list_free_models()` returns `Err` (a `match`/`if let Err` guard
  around the update, not a blanket overwrite). Add two tests against a mock
  server returning an error response: one starting from a populated,
  not-yet-expired cache (asserts `snapshot()` is unchanged after the failed
  `refresh()`), one starting from an empty/expired cache (asserts
  `snapshot() == None` after the failed `refresh()`).
- Files: `src/providers/openrouter/cache.rs`

#### Story 2.1.2: Eager startup populate + background refresh, owned by `OpenrouterProvider::new()`, with weak-ref lifecycle
**As** the very first request after startup (or a route hot-swap), **I
want** the free-model list already warm, **so that** it isn't spuriously
`Exhausted` before any refresh has run — **and as** the process, **I want**
old background refresh tasks to stop after a route hot-swap orphans their
`Router`, **so that** they don't accumulate forever — **and as** the
money-safety property this feature depends on, **I want** this population
to happen for *every* `openrouter`-kind upstream `build_providers`
constructs, **so that** it does not depend on which `Strategy` (if any)
later references that upstream (architecture-review Blocker 1: originally
this lived only inside `Router::from_config`'s `Strategy::OpenrouterScored`
match arm, so an `openrouter`-kind upstream used under
`Strategy::Fallback`/`Weighted` never got a populated cache at all, and
`OpenrouterProvider::send()`'s "`None` snapshot falls through to the real
API call" behavior then silently forwarded an unfiltered model string to
OpenRouter — a direct violation of the "must not silently spend money"
constraint).

**Acceptance Criteria**:
- `OpenrouterProvider::new()` (Task 1.2.1b) — not `Router::from_config` —
  awaits one `model_cache.refresh(&self)` call before returning, for every
  `openrouter`-kind upstream `build_providers` constructs, regardless of
  whether any route's `Strategy` ends up referencing it. A refresh failure
  is logged (`tracing::warn!`) but does not fail `new()` (matches
  `BedrockProvider::new`'s existing graceful-construction precedent,
  `src/routing/router.rs:71-73`).
  - *Given* a reachable OpenRouter `/models` endpoint, *when*
    `build_providers` constructs an `OpenrouterProvider` for a config
    upstream, *then* that provider's `model_cache.snapshot()` is already
    `Some(..)` before any request has been dispatched and before
    `Router::from_config` has even inspected which `Strategy` any route
    uses.
  - *Given* an unreachable OpenRouter endpoint, *when* `OpenrouterProvider::new()`
    runs, *then* it still returns `Ok(Arc<OpenrouterProvider>)` (not `Err`),
    with `model_cache.snapshot() == None`.
  - *Given* a config with an `openrouter`-kind upstream referenced only by a
    `Strategy::Fallback` route (no `OpenrouterScored` route anywhere), *when*
    `build_providers` runs, *then* that upstream's `model_cache` is
    populated exactly the same as if an `OpenrouterScored` route existed —
    population is unconditional, not gated on strategy choice. (The
    complementary config-validation rule in Story 4.3.1 additionally
    rejects this specific config at load time as defense-in-depth; this
    acceptance criterion tests that population itself doesn't depend on it.)
- A background `tokio::spawn`ed task, spawned from inside `new()`, refreshes
  every 5 minutes (`MODEL_LIST_REFRESH_INTERVAL`), holding only
  `Weak<ModelListCache>` + `Weak<OpenrouterProvider>`, and exits its loop
  the first time either `.upgrade()` fails.
  - *Given* an `OpenrouterProvider` built via `new()` then dropped (no other
    strong references), *when* the next refresh tick fires, *then* the
    background task's `Weak::upgrade()` calls fail and the task returns
    without panicking or refreshing.
- **Pinned-request cold-cache bypass is logged, not silent (adversarial-review
  Concern, 2026-09-07 re-review):** a session pinned to a specific free model
  (Story 4.2.4's pass-through) survives `expand_candidates` even when
  `model_cache.snapshot() == None`, which means Task 2.1.2c's `send()`-time
  price recheck has nothing to check against and falls through to the real
  API call unverified — the documented "`None` snapshot falls through, real
  API call is source of truth" behavior applies here too, but for this one
  case (a pinned dispatch, not an ordinary scored one) that fallthrough means
  zero local price verification for a request that could have gone
  free→paid. `send()` must emit a `tracing::warn!` naming the model whenever
  it forwards a request with no cache snapshot to verify against, so this
  bypass is visible in logs rather than indistinguishable from the normal,
  verified path.
  - *Given* a request whose `model` was selected via a session pin and
    `model_cache.snapshot() == None` at dispatch time, *when* `send()`
    forwards the request, *then* a `tracing::warn!` event fires naming the
    model and noting the price recheck was bypassed due to a cold cache.

**Files**: `src/providers/openrouter/mod.rs`, `src/providers/openrouter/cache.rs`

##### Task 2.1.2a: `OpenrouterProvider::new()`'s eager refresh (~5 min)
- Inside `new()` (Task 1.2.1b), after the `Arc::new_cyclic` call returns an
  `Arc<Self>` with `model_cache` already constructed (holding its
  `Weak<Self>` back-reference), `.await` one `model_cache.refresh(&self)`
  call, logging on `Err` rather than propagating it. This runs
  unconditionally for every `openrouter`-kind upstream `build_providers`
  constructs (Task 1.2.3a) — it no longer lives inside
  `Router::from_config`'s strategy-selection match at all, which is the
  fix for architecture-review Blocker 1.
- Files: `src/providers/openrouter/mod.rs`

##### Task 2.1.2b: Background refresh task with weak-ref exit (~5 min)
- Immediately after Task 2.1.2a's eager refresh, inside `new()`, spawn:
  ```rust
  let weak_cache = Arc::downgrade(&model_cache);
  let weak_provider = weak_self.clone(); // from the Arc::new_cyclic closure
  tokio::spawn(async move {
      loop {
          tokio::time::sleep(MODEL_LIST_REFRESH_INTERVAL).await;
          let (Some(cache), Some(provider)) = (weak_cache.upgrade(), weak_provider.upgrade()) else {
              break;
          };
          if let Err(e) = cache.refresh(&provider).await {
              tracing::warn!(error = %e, "openrouter model-list background refresh failed");
          }
      }
  });
  ```
- Files: `src/providers/openrouter/mod.rs`

##### Task 2.1.2c: `OpenrouterProvider::model_cache()` accessor + send()-time price recheck (~4 min)
- `OpenrouterProvider` gets a `model_cache: Arc<ModelListCache>` field,
  created in `new()`, and `pub fn model_cache(&self) -> Arc<ModelListCache>`
  (cheap `Arc::clone`). Wire the send()-time consistency check here too:
  before forwarding, if `model_cache.snapshot()` is `Some(list)`, look up
  the `FreeModelEntry` matching the request's `model` field. If none
  matches, **or** a match exists but `price_prompt != 0.0 ||
  price_completion != 0.0`, return
  `Err(ProviderError::ModelUnsupported(model_id))` without making the
  network call (saves a request against the 20rpm budget). This is the
  literal "verify pricing == (0,0) for the specific selected model, not
  just presence" recheck architecture-review Blocker 2 / adversarial-review
  Blocker 2 asked for — it's defense-in-depth against a future bug in
  `list_free_models()`'s own filter (every entry in this list is expected
  to already be free by construction), not by itself a fix for the
  free→paid-mid-TTL gap; that gap's real backstop is Story 1.2.4's post-hoc
  cost check. A `None` snapshot (cold cache) falls through and lets the
  real API call be the source of truth — but (adversarial-review Concern,
  2026-09-07 re-review) that fallthrough is only reachable at all for a
  pinned dispatch (an unpinned candidate at a cold cache is already dropped
  by `expand_candidates`, Story 4.2.4), so reaching this branch means a
  session-pinned request is about to be forwarded with zero local price
  verification. Emit `tracing::warn!(model, "openrouter: dispatching
  session-pinned model with no cache snapshot to verify price against —
  price recheck bypassed")` in this fallthrough branch so the bypass is
  visible in logs, not silent.
- Files: `src/providers/openrouter/mod.rs`

##### Task 2.1.2d: Unit/integration tests (~5 min)
- Eager-refresh-failure-doesn't-fail-`new()` test; weak-ref exit test
  (construct provider, drop all strong refs, advance a short test interval,
  assert no panic / task completes); population-is-unconditional-of-strategy
  test (build a config with an `openrouter`-kind upstream referenced only by
  a `Fallback` route, call `build_providers`, assert the provider's
  `model_cache.snapshot()` is `Some(..)` after construction); send()-time
  price-recheck test (model present in snapshot but with nonzero price →
  `ModelUnsupported` with zero mock-server calls recorded — construct this
  case via the test-only cache constructor, since `list_free_models()`
  itself should never actually produce such an entry); pinned-cold-cache
  `warn!` test (a session-pinned model dispatched against a `None` snapshot
  emits the bypass `tracing::warn!` event naming the model, asserted via a
  test-scoped `tracing` subscriber).
- Files: `src/providers/openrouter/mod.rs`

#### Story 2.1.3: Data-policy-vs-staleness distinguishing invalidation, with immediate self-heal
**As** the model-list cache, **I want** to invalidate-and-refetch when one
specific model 404s (real staleness), but **not** when every candidate
404s at once (the account-wide data-policy toggle), **so that** a policy
change doesn't loop refetching forever while a genuinely stale single entry
still self-heals — **and as** the route depending on this cache, **I want**
a real invalidation to trigger an immediate refetch rather than waiting up
to 5 minutes for the next periodic tick, **so that** one stale model
doesn't take the whole free-model pool to `Exhausted` for minutes at a time
(adversarial-review Blockers 3 and 4: the original minority-vs-systemic
threshold was mathematically unsatisfiable at `cached_count == 1`, and
`invalidate()` had no refetch trigger at all).

**Acceptance Criteria**:
- `record_not_found_and_maybe_invalidate(&self, model: &str)`: when a
  minority of the cached list's models have 404'd within the last 60
  seconds, it invalidates the cache and sets
  `last_invalidation_reason = Some("model_not_found:<model>")`.
  - *Given* a cached list of 5 models and no prior 404s recorded, *when*
    `record_not_found_and_maybe_invalidate("a/b:free")` is called, *then*
    `snapshot()` afterward returns `None` (invalidated) and
    `last_invalidation_reason() == Some("model_not_found:a/b:free")`.
- When *every* model in the cached list has 404'd within the window (the
  data-policy signature), it does **not** invalidate, and instead sets
  `last_invalidation_reason = Some("suppressed_systemic_404")` plus logs a
  `tracing::warn!`.
  - *Given* a cached list of exactly 2 models, *when*
    `record_not_found_and_maybe_invalidate` is called once for each of the
    2 models within the same 60-second window, *then* `snapshot()`
    afterward still returns `Some(..)` (not invalidated) and
    `last_invalidation_reason() == Some("suppressed_systemic_404")`.
- **`cached_count == 1` special case (adversarial-review Blocker 3):** when
  the cached list has exactly 1 model, any 404 against it invalidates —
  it is never classified systemic, even though the general rule
  (`distinct_failed < cached_count`) is unsatisfiable at `cached_count == 1`
  (`1 < 1` is always false). A single-model pool has no "minority vs.
  majority" to distinguish; treating its one failure as staleness lets it
  self-heal, and the cost of occasionally misclassifying a genuine
  data-policy toggle as staleness here is cheap (one extra refetch that
  reconfirms 0 free models within a round-trip) compared to the cost of
  never invalidating (the pool is permanently stuck, since there is no
  future event that would ever reconsider it).
  - *Given* a cached list of exactly 1 model, *when*
    `record_not_found_and_maybe_invalidate` is called for that model,
    *then* `snapshot()` afterward returns `None` (invalidated), not
    `Some(..)`, and `last_invalidation_reason() ==
    Some("model_not_found:<that model>")` — **not**
    `"suppressed_systemic_404"`.
- The 404-tracking window (`NOT_FOUND_WINDOW = 60s`) prunes entries older
  than 60 seconds, so an old, unrelated 404 doesn't count toward "everyone
  just failed."
  - *Given* one model 404'd 90 seconds ago and a different model 404s now,
    out of a cached list of 3, *when*
    `record_not_found_and_maybe_invalidate` runs for the current one,
    *then* only 1 distinct recent failure is counted (the stale one is
    pruned), so it invalidates (1 < 3).
- **Immediate on-demand refetch on real invalidation (adversarial-review
  Blocker 4):** every time this method takes the *real* (non-systemic)
  invalidation branch — including the `cached_count == 1` branch above — it
  additionally triggers a one-shot, fire-and-forget background refresh
  immediately, rather than relying solely on the next periodic 5-minute
  tick (Story 2.1.2). This is what actually makes Story 2.1.3's own framing
  ("a genuinely stale single entry still self-heals") true: without it, a
  `None` snapshot makes `expand_candidates` drop the openrouter candidate
  entirely (Story 4.2.4), so the whole route sits at `Exhausted` until the
  next tick — up to 5 minutes, contradicting `research/ux.md`'s
  fail-closed(money)/fail-soft(staleness) split for what is a pure
  staleness event.
  - *Given* a cache invalidated via the real-staleness branch, *when* a
    short time passes (well under 5 minutes — e.g. the time for one mock-
    server round trip in a test), *then* `snapshot()` returns `Some(..)`
    again, without waiting for the next scheduled periodic tick.
- **Single-flight guard:** if `record_not_found_and_maybe_invalidate` takes
  the real-invalidation branch multiple times in quick succession (e.g.
  several per-model 404s land close together), at most one on-demand
  refresh is in flight at a time — a second trigger while one is already
  running is a no-op, avoiding the refetch stampede `research/pitfalls.md`
  §4 warns about.
  - *Given* two calls to `record_not_found_and_maybe_invalidate` for two
    different models within milliseconds of each other, both taking the
    real-invalidation branch, *when* both trigger a refetch, *then* the
    mock OpenRouter `/models` endpoint records exactly 1 call, not 2.

**Files**: `src/providers/openrouter/cache.rs`

##### Task 2.1.3a: `recent_not_found` tracking + pruning (~4 min)
- `record_not_found_and_maybe_invalidate` inserts `(model, Instant::now())`
  into `recent_not_found`, then `retain`s entries within `NOT_FOUND_WINDOW`
  (60s, module constant).
- Files: `src/providers/openrouter/cache.rs`

##### Task 2.1.3b: Minority-vs-systemic decision + invalidate/suppress, with `cached_count == 1` special case (~5 min)
- Compare `recent_not_found.len()` (distinct failing models) against
  `cached_count = snapshot().map_or(0, |s| s.len())`. Decision:
  - `cached_count == 1`: always take the real-invalidation branch (the
    general minority-vs-systemic comparison is skipped entirely — it's
    unsatisfiable here, not just an edge case of it).
  - `cached_count > 1 && distinct_failed < cached_count`: real-invalidation
    branch.
  - Otherwise (`cached_count == 0`, or `distinct_failed >= cached_count`
    with `cached_count > 1`): suppress-as-systemic branch.
  - Real-invalidation branch: call `self.cache.invalidate(&())`, set
    `last_invalidation_reason = Some(format!("model_not_found:{model}"))`,
    then call `self.trigger_immediate_refresh()` (Task 2.1.3d).
  - Suppress branch: set
    `last_invalidation_reason = Some("suppressed_systemic_404".into())` and
    `tracing::warn!(model, distinct_failed, cached_count, "...")`.
- Files: `src/providers/openrouter/cache.rs`

##### Task 2.1.3c: Unit tests (~5 min)
- Cover the 4 non-refetch acceptance criteria above (minority-invalidates,
  systemic-suppresses, window-pruning), **plus the `cached_count == 1`
  special case explicitly** (previously only `cached_count == 0` was in
  this task's list — the `== 1` case is the one adversarial-review Blocker
  3 found missing), plus a `cached_count == 0` edge case (empty/never-
  populated cache — should neither invalidate nor suppress-log
  meaningfully; just no-op safely, and must not attempt a refetch either).
- Files: `src/providers/openrouter/cache.rs`

##### Task 2.1.3d: On-demand one-shot refresh trigger + single-flight guard (~5 min)
- `ModelListCache::trigger_immediate_refresh(&self)`: if
  `self.refresh_in_flight.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire).is_ok()`,
  `tokio::spawn` a task that upgrades `self.provider` (the `Weak<OpenrouterProvider>`
  set at construction, Task 2.1.1b), calls `refresh(&provider).await` if the
  upgrade succeeds (a no-op, just clear the flag, if it doesn't — the
  provider is gone), and resets `refresh_in_flight` to `false` on every exit
  path (success, refresh error, or upgrade failure). If the
  compare-exchange fails (already in flight), this call is a no-op — that's
  the single-flight guard. This doesn't reuse moka's own single-flight
  machinery (`sync::Cache::get_with`'s blocking single-flight doesn't
  compose with an async HTTP call from a sync context — the same reason
  ADR-001 rejected `future::Cache` for this type); the `AtomicBool` guard is
  the sync-compatible equivalent for this one specific call site.
- Files: `src/providers/openrouter/cache.rs`

##### Task 2.1.3e: On-demand refresh + single-flight tests (~5 min)
- Test 1: invalidate via the real-staleness branch against a mock server,
  then poll/await briefly (well under 5 minutes) and assert
  `snapshot().is_some()` again — demonstrating self-heal without waiting for
  the periodic tick. Test 2: fire two real-invalidation calls back-to-back
  (e.g. from two different model ids) and assert the mock server's
  `/models` call count is exactly 1, not 2, verifying the single-flight
  guard.
- Files: `src/providers/openrouter/cache.rs`

---

## Phase 3: Per-Candidate Metrics Infrastructure

### Epic 3.1: `RoutingStrategy` Trait Additions + `already_tried` Widening

**Goal**: The 3 new additive trait methods exist with safe defaults, proven
not to affect `FallbackStrategy`/`WeightedStrategy`, and `Router::dispatch`
correctly retries a *different* free model instead of giving up after one
model's failure.

#### Story 3.1.1: Add `expand_candidates`/`record_outcome`/`observability_snapshot` to `RoutingStrategy`
**As** the router, **I want** 3 new hooks with safe defaults on
`RoutingStrategy`, **so that** `OpenrouterScoringStrategy` can plug into the
existing dispatch loop without changing `select()`'s signature or affecting
the other two strategies.

**Acceptance Criteria**:
- `FallbackStrategy`/`WeightedStrategy`'s entire existing test suite
  (`src/routing/strategy.rs:66-111`) still passes unmodified after adding
  the 3 default methods.
  - *Given* the existing `fallback_selects_first_healthy` test, *when* run
    after this story's changes, *then* it still passes with no
    modification to the test itself.
- `expand_candidates`'s default is the identity function.
  - *Given* `FallbackStrategy` (which doesn't override it) and a candidate
    vec of 2 `UpstreamRef`s, *when* `.expand_candidates(candidates.clone())`
    is called, *then* it returns the same 2 `UpstreamRef`s unchanged.
- `record_outcome`'s default is a no-op (compiles and does nothing
  observable).
- `observability_snapshot`'s default returns `None`.

**Files**: `src/routing/strategy.rs`

##### Task 3.1.1a: Add the 3 methods to the trait (~4 min)
- In `RoutingStrategy` (`src/routing/strategy.rs:25-27`), add:
  ```rust
  fn expand_candidates(&self, candidates: Vec<UpstreamRef>) -> Vec<UpstreamRef> {
      candidates
  }
  fn record_outcome(
      &self,
      _candidate: &UpstreamRef,
      _duration_ms: u64,
      _success: bool,
      _error_kind: Option<&'static str>,
  ) {
  }
  fn observability_snapshot(&self) -> Option<serde_json::Value> {
      None
  }
  ```
- Files: `src/routing/strategy.rs`

##### Task 3.1.1b: Regression test + identity test (~3 min)
- Run the existing test module unmodified; add one new test asserting
  `expand_candidates`'s identity default on `FallbackStrategy`.
- Files: `src/routing/strategy.rs`

#### Story 3.1.2: Widen `already_tried`; call the 3 new hooks from `Router::dispatch`
**As** a dispatch loop routing among several free models behind one
upstream index, **I want** a failed attempt against one model to still let
the loop try a *different* model, **so that** the pool isn't wrongly
declared `Exhausted` after a single model's failure.

**Acceptance Criteria**:
- `already_tried` is `HashSet<(usize, Option<String>)>`; a failure against
  `(2, Some("a/b:free"))` does not prevent trying `(2, Some("c/d:free"))`
  in the same dispatch loop.
  - *Given* two per-model candidates at the same index 2 (`"a/b:free"` and
    `"c/d:free"`) both healthy, and model `"a/b:free"`'s attempt returns a
    transient error, *when* the loop continues, *then* `"c/d:free"` is
    still selectable (not filtered out by `already_tried`).
- Every existing `already_tried`-dependent test in `src/routing/router.rs`
  (candidates all carrying `model: None`) still passes unmodified — `(idx,
  None)` is a strict refinement of `idx` for those cases.
- **Intentional, disclosed behavior change for non-OpenRouter strategies
  (architecture-review Concern, `research/architecture.md` §3.4):** this
  widening is a real behavior change for `Fallback`/`Weighted` routes too,
  not just an internal detail of the new OpenRouter path.
  `RouteUpstreamRef.model: Option<String>` is a general, already-shipped
  config field usable under any `UpstreamKind` (`src/config/schema.rs:157`);
  two route-upstream entries at the same index with different model pins
  are a legitimate existing config shape. Under the old `HashSet<usize>`, a
  failure on one such pin excluded both from the retry loop; under the
  widened key, they become independently retryable. This is called out here
  explicitly, as a documented and intentional side effect, so it is not
  silently discovered later by a future maintainer.
  - *Given* a `Fallback` (or `Weighted`) route with two `RouteUpstreamRef`s
    both at index 3 — one pinned `model: Some("model-a")`, the other
    `model: Some("model-b")` — and `"model-a"`'s attempt fails, *when* the
    dispatch loop continues, *then* `"model-b"` is still selectable in the
    same dispatch call (not excluded by `already_tried`): a failure on one
    pinned model no longer poisons the other.
- `Router::dispatch` calls `self.strategy.expand_candidates(candidates)`
  once, before the health-filter loop begins.
  - *Given* a route with an `OpenrouterScoringStrategy` and a warm
    `ModelListCache` snapshot of 3 free models, *when* `dispatch()` runs,
    *then* the loop's candidate pool contains 3 per-model `UpstreamRef`s,
    not the 1 static one.
- `Router::dispatch` calls `self.strategy.record_outcome(&chosen,
  duration_ms, success, error_kind)` after every attempt (all 5 existing
  outcome arms), after the `provider.send()` `.await` resolves.
  - *Given* a chosen candidate whose attempt returns
    `Err(ProviderError::ModelUnsupported("x".into()))`, *when* the
    corresponding match arm runs, *then*
    `strategy.record_outcome` is called with `success = false` and
    `error_kind = Some("model_unsupported")`.

**Files**: `src/routing/router.rs`

##### Task 3.1.2a: Widen `already_tried`'s type (~3 min)
- Change `let mut already_tried: HashSet<usize> = HashSet::new();`
  (`src/routing/router.rs:258`) to
  `HashSet<(usize, Option<String>)>`.
- Files: `src/routing/router.rs`

##### Task 3.1.2b: Update the filter + insert call sites (~3 min)
- `src/routing/router.rs:284`:
  `.filter(|u| !already_tried.contains(&(u.index, u.model.clone())) && self.health.is_available(u.index))`.
- `src/routing/router.rs:291`:
  `already_tried.insert((chosen.index, chosen.model.clone()));`.
- Files: `src/routing/router.rs`

##### Task 3.1.2c: Call `expand_candidates` before the loop (~2 min)
- Right after `let candidates = self.effective_candidates(session_id.as_deref());`
  (`src/routing/router.rs:267`), add
  `let candidates = self.strategy.expand_candidates(candidates);`.
- Files: `src/routing/router.rs`

##### Task 3.1.2d: Call `record_outcome` from each outcome arm (~5 min)
- Restructure the `match provider.send(...).await { .. }` block
  (`src/routing/router.rs:316-365`) to bind the result first and compute
  `duration_ms` once, then call `self.strategy.record_outcome(&chosen,
  duration_ms, success, error_kind)` in each of the 5 arms (`Ok`,
  validation/auth, rate-limited, response-shape-mismatch, catch-all `Err`),
  alongside the existing `self.record_attempt(..)` calls. `error_kind` is
  `outcome.as_ref().err().map(ProviderError::kind_label)`.
- Files: `src/routing/router.rs`

##### Task 3.1.2e: Regression + new tests (~5 min)
- Run the full existing `router.rs` test suite (all `already_tried`-adjacent
  tests use `model: None`, so `(idx, None)` behaves identically to `idx`).
  Add a new test for the "different model retried after one model's
  failure" acceptance criterion, and one asserting `record_outcome` is
  invoked with the right `error_kind` per arm (using a strategy test double
  that records calls).
- Files: `src/routing/router.rs`

##### Task 3.1.2f: Non-OpenRouter same-index/different-model-pin regression test (~4 min)
- Resolves the architecture-review Concern flagged against this story
  (`implementation/architecture-review.md`): its recommendation was "add one
  explicit test/acceptance-criterion — same index, two different model
  pins, `Fallback`/`Weighted` strategy: a failure on one no longer poisons
  the other — so the behavior change is a documented, intentional decision
  rather than an implicit side effect." Add
  `dispatch_should_not_poison_sibling_model_pin_at_same_index_for_fallback_strategy`
  (and the `Weighted` equivalent, or one parameterized test covering both)
  using a `Fallback`/`Weighted` route — deliberately *not*
  `OpenrouterScoringStrategy` — with two `RouteUpstreamRef`s at the same
  index differing only in `model`, asserting a failure on one still leaves
  the other selectable within the same `dispatch()` call. The test's doc
  comment states this is an intentional, disclosed side effect of the
  `already_tried` widening (Task 3.1.2a), not a regression — matching
  requirements.md's Out-of-Scope guarantee that `FallbackStrategy`/
  `WeightedStrategy`'s *intended* behavior is unaffected, while making this
  one dedup-granularity change explicit rather than silently absorbed.
- Files: `src/routing/router.rs`

### Epic 3.2: `ModelStats` / `RollingErrorRate`

**Goal**: The per-model rolling latency+error state `OpenrouterScoringStrategy`
needs, built from a tiny additive `DurationHistogram` accessor plus one new
small type mirroring its shape.

#### Story 3.2.1: `DurationHistogram::sample_count()`
**As** the composite scorer, **I want** to distinguish "0 real samples"
(cold start) from "real samples whose value happens to be 0", **so that**
cold-start defaulting (ADR-003) is unambiguous.

**Acceptance Criteria**:
- `sample_count()` returns the number of samples within the rolling window,
  0 for an empty/all-expired histogram.
  - *Given* a fresh `DurationHistogram::with_default_window()`, *when*
    `sample_count()` is called, *then* it returns `0`.
  - *Given* a histogram with 3 `record()` calls within the window, *when*
    `sample_count()` is called, *then* it returns `3`.

**Files**: `src/metrics/histogram.rs`

##### Task 3.2.1a: Add `sample_count()` (~3 min)
- Insert after `percentiles()` (`src/metrics/histogram.rs:90`), mirroring
  its cutoff/lock/filter pattern:
  ```rust
  #[must_use]
  pub fn sample_count(&self) -> usize {
      let now = Instant::now();
      let cutoff = now.checked_sub(self.window).unwrap_or(now);
      let samples = self.samples.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
      samples.iter().filter(|(t, _)| *t >= cutoff).count()
  }
  ```
- Files: `src/metrics/histogram.rs`

##### Task 3.2.1b: Unit test (~2 min)
- Cover both acceptance criteria.
- Files: `src/metrics/histogram.rs`

#### Story 3.2.2: `RollingErrorRate`
**As** the composite scorer, **I want** a per-model rolling error rate with
the same window/recovery discipline as `DurationHistogram`, **so that**
error-rate scoring has the same correctness guarantees latency scoring
already has.

**Acceptance Criteria**:
- `error_rate()` returns `None` when no samples are in the window (cold
  start — distinct from a real `0.0`).
  - *Given* a fresh `RollingErrorRate::with_default_window()`, *when*
    `error_rate()` is called, *then* it returns `None`.
- `error_rate()` returns the correct fraction otherwise.
  - *Given* 3 `record(true)` calls and 1 `record(false)` call, *when*
    `error_rate()` is called, *then* it returns `Some(0.25)`.
- Samples older than the 15-minute window are excluded, matching
  `DurationHistogram`'s window/trim behavior.
- A poisoned mutex recovers via `PoisonError::into_inner`, matching
  `DurationHistogram`'s existing discipline (`research/pitfalls.md`).

**Files**: `src/routing/model_stats.rs` (new)

##### Task 3.2.2a: `RollingErrorRate` struct + `record()`/`error_rate()`/`sample_count()` (~5 min)
- `Mutex<VecDeque<(Instant, bool)>>` + `window: Duration`, mirroring
  `DurationHistogram`'s exact structure (`src/metrics/histogram.rs:13-50`)
  including the trim-on-write and `PoisonError::into_inner` recovery.
- Files: `src/routing/model_stats.rs`

##### Task 3.2.2b: Unit tests (~4 min)
- Cover all 4 acceptance criteria above.
- Files: `src/routing/model_stats.rs`

#### Story 3.2.3: `ModelStats`
**As** `OpenrouterScoringStrategy`, **I want** one struct bundling a
model's latency histogram and error-rate tracker, **so that** its
`DashMap<String, ModelStats>` has one coherent entry per model.

**Acceptance Criteria**:
- `ModelStats::new()` returns a struct with both trackers empty/cold-start.
  - *Given* `ModelStats::new()`, *when* both `.latency.sample_count()` and
    `.errors.error_rate()` are checked, *then* they report `0` and `None`
    respectively.

**Files**: `src/routing/model_stats.rs`

##### Task 3.2.3a: `ModelStats` struct (~2 min)
- `pub struct ModelStats { pub latency: DurationHistogram, pub errors:
  RollingErrorRate }` with `pub fn new() -> Self` using
  `DurationHistogram::with_default_window()` +
  `RollingErrorRate::with_default_window()`.
- Files: `src/routing/model_stats.rs`

##### Task 3.2.3b: Unit test (~2 min)
- Cover the acceptance criterion above.
- Files: `src/routing/model_stats.rs`

---

## Phase 4: Bench Table + Scoring Strategy

### Epic 4.1: Static Bench-Rank Table

#### Story 4.1.1: `BENCH_TABLE`
**As** the composite scorer, **I want** a checked-in, source-attributed
static table of coding-benchmark scores for OpenRouter's free models, **so
that** the bench-rank signal has real data without a live scraper.

**Acceptance Criteria**:
- `bench_score(model_id)` returns `Some(pass_rate / 100.0)` for a model in
  the table, `None` for one that isn't.
  - *Given* a table entry `("deepseek/deepseek-chat-v3.1:free", 55.1)`,
    *when* `bench_score("deepseek/deepseek-chat-v3.1:free")` is called,
    *then* it returns `Some(0.551)`.
  - *Given* a model id not in the table, *when* `bench_score(id)` is
    called, *then* it returns `None`.
- The table's source and retrieval date are recorded in a doc comment, per
  `research/build-vs-buy.md`'s Apache-2.0-attribution requirement.
- The table contains real transcribed values (not fabricated placeholders)
  for at least the free models this plan's implementer confirms are
  actually present in a live OpenRouter `/models` response (Task 1.2.2a),
  cross-referenced against https://aider.chat/docs/leaderboards/.

**Files**: `src/routing/bench_table.rs` (new)

##### Task 4.1.1a: Fetch and transcribe real leaderboard rows (~5 min)
- Resolves this story's Unresolved Question. Using Task 1.2.2a's confirmed
  list of free model ids, look each one up (or its closest named match) on
  https://aider.chat/docs/leaderboards/ and transcribe its pass-rate
  percentage. Do not invent scores for models not found on the leaderboard
  — omit them from the table instead (they'll score the neutral `0.5`
  default per ADR-003, which is the correct fail-soft behavior).
- Files: none (research capture, feeds Task 4.1.1b)

##### Task 4.1.1b: `BENCH_TABLE` + `bench_score()` (~3 min)
- `pub const BENCH_TABLE: &[(&str, f64)] = &[ /* Task 4.1.1a's rows */ ];`
  with the source-URL + retrieval-date doc comment. `pub fn
  bench_score(model_id: &str) -> Option<f64>` does a linear scan (table is
  small, no need for a `HashMap`).
- Files: `src/routing/bench_table.rs`

##### Task 4.1.1c: Unit tests (~2 min)
- Cover both `bench_score` acceptance criteria using 2 known table rows.
- Files: `src/routing/bench_table.rs`

##### Task 4.1.1d: Document the override path as edit-and-rebuild (~2 min)
- Resolves the adversarial-review Concern that requirements.md's in-scope
  "documented path for Tyler to override or refresh it" (the bench table)
  had no corresponding story — Epic 4.1 ships only a hardcoded Rust const,
  with no config-layer override. Deliberate scope decision, not an oversight:
  `research/features.md` §4's `conf.d`-mergeable-override recommendation is
  a new config-parsing subsystem, and this feature's appetite (Large, 9
  epics) is already fully allocated — adding one this late is exactly the
  scope creep `requirements.md`'s Rabbit Holes section warns against. Add a
  doc comment directly above `BENCH_TABLE` (Task 4.1.1b's constant)
  stating: "Override path: edit this table's rows and rebuild consolette.
  There is no runtime/config-file override — see
  project_plans/openrouter-routing/requirements.md Scope for why this is a
  deliberate simplification, not a gap." No test required (a doc-comment
  task); Task 4.1.1c's tests are unaffected.
- Files: `src/routing/bench_table.rs`

### Epic 4.2: `OpenrouterScoringStrategy`

**Goal**: The composite-scoring, epsilon-greedy `RoutingStrategy` impl
(ADR-002, ADR-003), fully unit-tested including the monotonicity guarantee.

#### Story 4.2.1: Score computation
**As** `OpenrouterScoringStrategy::select`, **I want** a pure function
computing each candidate's normalized `ScoreBreakdown`, **so that** the
formula (ADR-003) is independently testable from selection.

**Acceptance Criteria**:
- Min-max normalization for latency/error-rate; absolute `/100.0` for bench
  score; cold-start and unranked-model neutral defaults of `0.5`; weights
  `0.5/0.3/0.2` per ADR-003.
  - *Given* 2 candidates, A (p50=100ms, error_rate=0.0, bench=80.0) and B
    (p50=500ms, error_rate=0.5, bench=40.0), *when* scores are computed,
    *then* A's `composite` is strictly greater than B's.
  - *Given* a candidate with 0 samples in both trackers and no bench-table
    entry, *when* its score is computed, *then*
    `norm_latency == norm_error == bench_score == 0.5` and `composite ==
    0.5`.
  - *Given* a single candidate (min == max for both live signals), *when*
    its score is computed, *then* `norm_latency == norm_error == 1.0`
    (tie-guard, not division by zero / NaN).
- An unranked model logs `tracing::warn!` exactly once per model id (not
  once per `select()` call) — tracked via a `DashSet<String>`-or-equivalent
  "already warned" set.

**Files**: `src/routing/openrouter_scoring.rs` (new)

##### Task 4.2.1a: `ScoreBreakdown` struct + per-candidate normalization (~5 min)
- `pub struct ScoreBreakdown { pub latency_p50_ms: u64, pub error_rate: Option<f64>, pub bench_rank: Option<f64>, pub composite: f64, pub sample_count: usize }`.
  A private `fn normalize(values: &[f64]) -> Vec<f64>` helper implementing
  the min-max-with-tie-guard logic once, reused for both latency and error
  rate.
- Files: `src/routing/openrouter_scoring.rs`

##### Task 4.2.1b: Weighted-sum composite (~3 min)
- `composite = 0.5 * norm_error + 0.3 * norm_latency + 0.2 * bench_score`
  as named module constants (`WEIGHT_ERROR`, `WEIGHT_LATENCY`,
  `WEIGHT_BENCH`), per ADR-003.
- Files: `src/routing/openrouter_scoring.rs`

##### Task 4.2.1c: Unranked-model once-per-model warning (~3 min)
- A `warned_unranked: DashSet<String>` (or `Mutex<HashSet<String>>`) field
  on `OpenrouterScoringStrategy`; log + insert only if not already present.
- Files: `src/routing/openrouter_scoring.rs`

##### Task 4.2.1d: Unit tests (~5 min)
- Cover all acceptance criteria above.
- Files: `src/routing/openrouter_scoring.rs`

#### Story 4.2.2: `select()` — epsilon-greedy
**As** `OpenrouterScoringStrategy`, **I want** to pick the best-scoring
candidate 90% of the time and a uniformly random one 10% of the time, **so
that** deprioritized models still recover (ADR-003).

**Acceptance Criteria**:
- With `healthy` empty, `select()` returns `None`.
- With one candidate, `select()` always returns it (both branches degenerate
  to the same choice).
- Over many trials with 2 differently-scored candidates, the higher-scoring
  one is picked noticeably more often (statistically consistent with
  ε=0.1), and the lower-scoring one is still picked sometimes (not zero
  times in, say, 200 trials).
  - *Given* candidate A (composite=0.9) and B (composite=0.1), *when*
    `select()` is called 200 times, *then* A is picked in roughly 91% ±
    generous statistical margin of trials and B is picked at least once.
- Ties are broken by first-in-list (deterministic), not randomly, when the
  greedy branch is taken.

**Files**: `src/routing/openrouter_scoring.rs`

##### Task 4.2.2a: `select()` impl (~5 min)
- Compute `ScoreBreakdown` for every candidate (Story 4.2.1), store into
  `last_scores` (for Story 5.1.1), then: `if rand::thread_rng().gen::<f64>() < EPSILON_EXPLORATION`
  pick uniformly via `healthy[thread_rng().gen_range(0..healthy.len())]`,
  else pick `healthy.iter().max_by(|a, b| score(a).partial_cmp(&score(b)))`
  (stable first-max, matching `Iterator::max_by`'s documented
  last-max-wins... note: `max_by` returns the *last* max on ties in Rust's
  stdlib — use `.enumerate().fold(..)` or reverse iteration if strict
  first-wins-on-tie is required; confirm against `std` docs during
  implementation and adjust to guarantee first-in-list-wins deterministically).
- Files: `src/routing/openrouter_scoring.rs`

##### Task 4.2.2b: Unit tests (~5 min)
- Cover all acceptance criteria above, including the statistical one (use
  a generous margin / seeded RNG if determinism is needed for CI
  stability).
- Files: `src/routing/openrouter_scoring.rs`

#### Story 4.2.3: `record_outcome()` — stats feed + ADR-002's synthetic 429 weighting + invalidation hook
**As** `OpenrouterScoringStrategy`, **I want** every attempt's outcome fed
into the right model's `ModelStats`, with a 429 additionally weighted per
ADR-002 and a model-not-found additionally triggering the cache's
minority-vs-systemic check, **so that** the composite score and the cache
both react correctly to live signals.

**Acceptance Criteria**:
- A successful attempt records `(duration_ms, true)` into the model's
  `ModelStats`.
  - *Given* an empty `model_stats` map, *when*
    `record_outcome(&candidate_with_model("a/b:free"), 150, true, None)` is
    called, *then* `model_stats["a/b:free"].latency.sample_count() == 1`
    and `.errors.error_rate() == Some(0.0)`.
- A `rate_limited` failure records the real failure plus
  `RATE_LIMIT_SYNTHETIC_FAILURES` (5) additional synthetic failures (per
  ADR-002).
  - *Given* an empty map, *when* `record_outcome(&candidate, 50, false,
    Some("rate_limited"))` is called, *then*
    `model_stats["a/b:free"].errors.sample_count() == 6` and
    `.error_rate() == Some(1.0)`.
- A `model_unsupported` failure calls
  `model_cache.record_not_found_and_maybe_invalidate(model_id)`.
  - *Given* a `ModelListCache` mock/spy, *when* `record_outcome(&candidate,
    50, false, Some("model_unsupported"))` is called, *then* the cache's
    `record_not_found_and_maybe_invalidate` was called with the
    candidate's model id.
- A candidate whose `.index != self.openrouter_index` or `.model == None`
  is ignored (not tracked) — this strategy only tracks its own per-model
  candidates.

**Files**: `src/routing/openrouter_scoring.rs`

##### Task 4.2.3a: Basic stats-feed (~3 min)
- Early-return if `candidate.index != self.openrouter_index || candidate.model.is_none()`.
  Otherwise `self.model_stats.entry(model.clone()).or_insert_with(ModelStats::new)`,
  then `.latency.record(duration_ms)` and `.errors.record(success)`.
- Files: `src/routing/openrouter_scoring.rs`

##### Task 4.2.3b: 429 synthetic weighting + model-not-found hook (~4 min)
- `match error_kind { Some("rate_limited") => for _ in 0..RATE_LIMIT_SYNTHETIC_FAILURES { stats.errors.record(false); }, Some("model_unsupported") => self.model_cache.record_not_found_and_maybe_invalidate(model), _ => {} }`
  inside the `!success` branch.
- Files: `src/routing/openrouter_scoring.rs`

##### Task 4.2.3c: Unit tests, incl. `Retry-After`-fidelity regression (ADR-002) (~5 min)
- Cover all 4 acceptance criteria above. Additionally: an
  integration-style test in `src/routing/router.rs` (or a new
  `openrouter_scoring.rs` test using a real `Router`) asserting that a 429
  from one per-model candidate trips `HealthRegistry` for that shared
  index, making *every* other per-model candidate at that index
  unavailable too — verifying ADR-002's claim that whole-upstream cooldown
  already delivers `Retry-After` fidelity without a sibling registry.
- Files: `src/routing/openrouter_scoring.rs`, `src/routing/router.rs`

#### Story 4.2.4: `expand_candidates()` — fan-out + GC, preserving session pins
**As** `OpenrouterScoringStrategy`, **I want** the one static "openrouter"
candidate replaced by one per currently-cached free model on every
dispatch, with stale `ModelStats` entries dropped, **so that** the dispatch
loop actually has per-model candidates to choose among — **and as** a
session that's pinned to one specific free model via
`SessionOverrideStore`, **I want** `expand_candidates` to leave my pin
alone, **so that** an existing feature (`research/features.md` §1: "an
existing feature this project must not regress") doesn't get silently
overridden back to the full pool on every dispatch (adversarial-review
Blocker 1).

**Acceptance Criteria**:
- A candidate at `self.openrouter_index` **whose `.model` is `None`** is
  replaced by N candidates (same index, weight; `.model = Some(entry.id)`
  for each `FreeModelEntry` in the cache snapshot); other candidates
  (different index, e.g. a mixed fallback route) pass through unchanged.
  - *Given* candidates `[UpstreamRef{index: 2, model: None, ..}]` and a
    cache snapshot of 2 `FreeModelEntry`s (`"a/b:free"`, `"c/d:free"`),
    *when* `expand_candidates` is called, *then* it returns 2
    `UpstreamRef`s, both `index: 2`, with `model` set to each of the two
    ids respectively.
  - *Given* a mixed route `[UpstreamRef{index: 0 (anthropic), ..},
    UpstreamRef{index: 2 (openrouter), model: None, ..}]`, *when*
    `expand_candidates` is called, *then* index 0's candidate is returned
    unchanged and index 2's is fanned out.
- **A candidate at `self.openrouter_index` whose `.model` is already
  `Some(..)` passes through unchanged — it is never replaced by the fanned-
  out list.** This is the fix for adversarial-review Blocker 1:
  `Router::dispatch` calls `effective_candidates(session_id)` — which
  applies `SessionOverrideStore`'s model pin, collapsing the candidate list
  to a single `UpstreamRef{index: openrouter_index, model:
  Some("pinned/model:free")}` — *before* calling `expand_candidates`. The
  original fan-out logic partitioned purely by index and ignored whether
  `.model` was already set, so a session pin was silently destroyed and
  replaced by the full N-model pool on every dispatch. Partitioning must
  additionally check `.model.is_none()` before treating a same-index
  candidate as fan-out input.
  - *Given* a candidate `UpstreamRef{index: 2, model: Some("pinned/x:free")}`
    (as `effective_candidates` would produce for a session pinned via
    `POST /api/sessions/{id}/route`) and a cache snapshot of 5 *different*
    free models, *when* `expand_candidates` is called, *then* the result is
    exactly `[UpstreamRef{index: 2, model: Some("pinned/x:free")}]` — one
    candidate, unchanged — not 5, and not the pin swapped for one of the
    cached 5.
- A `None` cache snapshot (cold cache) produces zero candidates for an
  openrouter-index candidate whose `.model` is `None` (not a panic, not the
  original unexpanded one — an unexpanded static "openrouter" `UpstreamRef`
  with `model: None` would dispatch an unscoped request to OpenRouter with
  whatever `model` the client sent, which isn't guaranteed to be a free
  model — silently defeating the money-safety constraint). A `None`
  snapshot does **not** affect an already-pinned (`model: Some(..)`)
  candidate at that index — the pass-through rule above still applies
  regardless of cache state, since a pin doesn't need the cache to know
  which model to dispatch to.
  - *Given* `model_cache.snapshot() == None`, *when* `expand_candidates` is
    called with an openrouter-index candidate whose `model: None`, *then*
    that candidate is dropped entirely from the result.
  - *Given* `model_cache.snapshot() == None`, *when* `expand_candidates` is
    called with an openrouter-index candidate whose `model:
    Some("pinned/x:free")`, *then* that candidate is still returned
    unchanged (not dropped) — a session pin should still be attempted even
    if the background refresh hasn't warmed the cache yet.
- `self.model_stats` is GC'd via `retain` against the current snapshot's id
  set on every call (per the Pattern Decisions table's eviction policy).
- **GC is skipped entirely when `snapshot() == None` (adversarial-review
  Concern):** a cold cache must never be treated as "the id set is empty" —
  doing so would `retain` against an empty set and wipe every model's
  rolling latency/error history on every transient cache miss, discarding
  the "rolling stats absorb OpenRouter's own provider-mix noise" property
  `research/pitfalls.md` §1 relies on. `expand_candidates` must call
  `self.model_stats.retain(..)` only when `model_cache.snapshot()` is
  `Some(..)`; on `None` it leaves `model_stats` untouched.
  - *Given* a populated `model_stats` map (entries for 3 models) and
    `model_cache.snapshot() == None`, *when* `expand_candidates` is called,
    *then* all 3 `model_stats` entries are still present afterward
    (no eviction occurred).

**Files**: `src/routing/openrouter_scoring.rs`

##### Task 4.2.4a: Fan-out logic, with pinned-candidate pass-through (~5 min)
- Partition `candidates` by `(index == self.openrouter_index) && model.is_none()`
  — **not** by index alone. Candidates matching that predicate get replaced
  with `snapshot().map_or(vec![], |entries| entries.iter().map(|e| UpstreamRef { model: Some(e.id.clone()), ..original.clone() }).collect())`.
  Every other candidate — different index, **or** same index but
  `model.is_some()` (a session pin already applied by
  `effective_candidates`) — passes through unchanged, order preserved.
- Files: `src/routing/openrouter_scoring.rs`

##### Task 4.2.4b: `ModelStats` GC, no-op on cold cache (~3 min)
- `if let Some(snapshot) = self.model_cache.snapshot() { let current_ids: HashSet<_> = snapshot.iter().map(|e| e.id.clone()).collect(); self.model_stats.retain(|id, _| current_ids.contains(id)); }`
  — the `retain` call must live entirely inside the `Some(..)` branch; a
  `None` snapshot performs no `retain` call at all (resolves adversarial-review's
  Concern that a literal `retain` against an empty id set on `None` would
  wipe all per-model rolling history on every transient cache miss).
- Files: `src/routing/openrouter_scoring.rs`

##### Task 4.2.4c: Unit tests, incl. session-pin regression + cold-cache GC no-op (~5 min)
- Cover all acceptance criteria above, plus a GC test (stats entry for a
  model no longer in the snapshot is dropped after one `expand_candidates`
  call) **and** the cold-cache GC no-op test named in the acceptance
  criteria above (populate `model_stats` for 3 models, force
  `model_cache.snapshot() == None`, call `expand_candidates`, assert all 3
  entries survive). **Explicitly add** the session-pin regression test named
  in the acceptance criteria: pin a session to one specific free model
  (construct the candidate the way `effective_candidates` would), assert
  `expand_candidates` returns exactly that one candidate unchanged against
  a warm 5-model snapshot, and assert the same against a cold (`None`)
  snapshot.
- Files: `src/routing/openrouter_scoring.rs`

#### Story 4.2.5: Monotonicity + cold-start property tests
**As** the codebase's future maintainers, **I want** the "worst candidate
never outscores the best candidate" guarantee (ADR-003, `build-vs-buy.md`)
verified by an actual test, not just argued in a comment, **so that** a
future formula tweak can't silently break it.

**Acceptance Criteria**:
- A candidate with worst-possible latency, error rate, AND bench rank never
  scores higher than one with best-possible values on all three, across a
  range of candidate-pool sizes (2, 3, 5 candidates).
  - *Given* candidate WORST (max latency, max error rate, 0.0 bench) and
    candidate BEST (min latency, min error rate, 100.0 bench) among a pool
    of 5, *when* both are scored, *then* `score(WORST) < score(BEST)`
    strictly.
- A cold-start candidate (0 samples, unranked) scores exactly `0.5` in
  isolation, and scores strictly between a WORST and a BEST candidate when
  all three are in the same pool.
- Weights sum to 1.0 (a static assertion / test on the constants
  themselves — catches a future edit that breaks the `[0,1]` bound).

**Files**: `src/routing/openrouter_scoring.rs`

##### Task 4.2.5a: Monotonicity test across pool sizes (~4 min)
- Parameterized test (2/3/5-candidate pools) per the acceptance criterion.
- Files: `src/routing/openrouter_scoring.rs`

##### Task 4.2.5b: Cold-start-in-context test (~3 min)
- Per the second acceptance criterion.
- Files: `src/routing/openrouter_scoring.rs`

##### Task 4.2.5c: Weights-sum-to-1.0 test (~2 min)
- `assert!((WEIGHT_ERROR + WEIGHT_LATENCY + WEIGHT_BENCH - 1.0).abs() < f64::EPSILON)`.
- Files: `src/routing/openrouter_scoring.rs`

### Epic 4.3: Wire `Strategy::OpenrouterScored` into `Router::from_config`

#### Story 4.3.1: Strategy-selection match arm, with symmetric kind-vs-strategy validation
**As** a route configured with `strategy = "openrouter_scored"`, **I want**
`Router::from_config` to actually construct an `OpenrouterScoringStrategy`
for it, **so that** the config variant (Epic 1.1) does something — **and
as** the money-safety property this whole feature depends on, **I want**
`Router::from_config` to reject, in the *other* direction too, any route
that pairs an `openrouter`-kind upstream with a non-`OpenrouterScored`
`Strategy`, **so that** a `Fallback`/`Weighted` route can never dispatch to
an `openrouter`-kind upstream unfiltered (architecture-review Blocker 1's
defense-in-depth half — population itself is now unconditional per Story
2.1.2, but a `Fallback`/`Weighted` route referencing an `openrouter`
upstream is still a config mistake worth rejecting loudly rather than
letting it "work" in an untested way).

**Acceptance Criteria**:
- A route with `strategy = "openrouter_scored"` and an `openrouter`-kind
  upstream builds a `Router` whose strategy is an `OpenrouterScoringStrategy`
  wired to that upstream's index and `ModelListCache`.
  - *Given* a `Config` with an `openrouter`-kind upstream at index 1 and a
    route `{strategy: OpenrouterScored, upstreams: [{name: "openrouter"}]}`,
    *when* `Router::from_config(&config, metrics)` is called, *then* it
    returns `Ok(Router)` and a subsequent `dispatch()` call's candidate
    expansion reflects that upstream's cached free models.
- A route with `strategy = "openrouter_scored"` but **no** `openrouter`-kind
  upstream fails `from_config` with a clear error naming the route.
  - *Given* a route using `OpenrouterScored` whose `upstreams` all
    reference non-`openrouter`-kind upstreams, *when* `from_config` runs,
    *then* it returns `Err` naming the route.
- **Symmetric direction (architecture-review Blocker 1):** a route that
  references an `openrouter`-kind upstream under **any** `Strategy` other
  than `OpenrouterScored` fails `from_config` with a clear error naming both
  the route and the upstream.
  - *Given* a `Config` with an `openrouter`-kind upstream named `"or"` and a
    route `{name: "r1", strategy: Fallback, upstreams: [{name: "or"}]}`,
    *when* `Router::from_config` runs, *then* it returns `Err` naming route
    `"r1"` and upstream `"or"`.
  - *Given* the same upstream referenced instead by a
    `{strategy: Weighted, ..}` route, *when* `Router::from_config` runs,
    *then* it likewise returns `Err`.
  - *Given* a route mixing an `openrouter`-kind upstream with a non-
    `openrouter` upstream under `Strategy::Fallback`, *when*
    `Router::from_config` runs, *then* it still returns `Err` — the
    presence of *any* `openrouter`-kind upstream in a non-`OpenrouterScored`
    route's `upstreams` list is sufficient to reject it, regardless of what
    else is in that list.
- **Reverse mixed-upstream guard (adversarial-review Concern): a route using
  `strategy = "openrouter_scored"` that *also* lists a non-`openrouter`-kind
  (e.g. paid) upstream in its `upstreams` fails `from_config`.** Without
  this, a single misconfigured route listing both an `openrouter` and a
  paid upstream could let `dispatch()` silently fall through to the paid
  upstream once the free pool is `Exhausted` — precisely the "silently
  spend money" outcome this feature exists to prevent, and requirements.md's
  Scope says a paid fallback must be a *separate* route, not a mixed one.
  - *Given* a route `{name: "r2", strategy: OpenrouterScored, upstreams:
    [{name: "or"} (openrouter-kind), {name: "paid-openai"} (openai-kind)]}`,
    *when* `Router::from_config` runs, *then* it returns `Err` naming route
    `"r2"` and upstream `"paid-openai"`.
- **Full pool-exhaustion path, end-to-end (validation.md's "pool-exhaustion
  dispatch" integration test — requirements.md Success Metrics #4 / Scope
  #6):** when every fanned-out free-model candidate is cooling down or
  rate-limited, `Router::dispatch` itself — not just `select()` in
  isolation — returns `Err(ProviderError::Exhausted)`, with no fallback to
  a paid upstream.
  - *Given* a `Router` whose route is wired to an `OpenrouterScoringStrategy`
    over a warm `ModelListCache` snapshot of 3 free models sharing one
    upstream index, and `HealthRegistry` has tripped cooldown for that
    index (e.g. via a prior 429 response with `Retry-After`, so all 3
    per-model candidates are simultaneously unavailable), *when*
    `Router::dispatch(request, session_id)` is called, *then* it returns
    `Err(ProviderError::Exhausted)`, and no request is observed reaching any
    other (paid) upstream.

**Files**: `src/routing/router.rs`

##### Task 4.3.1a: Match arm (~4 min)
- Add to the strategy-selection match (`src/routing/router.rs:191-194`):
  construct `OpenrouterScoringStrategy::new(openrouter_index, model_cache,
  bench_table::BENCH_TABLE)`, obtaining `model_cache` via
  `provider.model_cache()` on the `Arc<OpenrouterProvider>` from the map
  Task 1.2.3a added. Unlike the original plan, this arm no longer performs
  the eager refresh or spawns the background task itself — both now happen
  unconditionally inside `OpenrouterProvider::new()` (Story 2.1.2), so this
  arm just wires the already-warm cache into the strategy.
- Files: `src/routing/router.rs`

##### Task 4.3.1b: No-openrouter-upstream error path (~2 min)
- The `.ok_or_else(...)` locating the route's `openrouter`-kind upstream
  index (needed for Task 4.3.1a's match arm) already covers this — this
  task just confirms/tests the error message names the route.
- Files: `src/routing/router.rs`

##### Task 4.3.1c: Integration test (~5 min)
- End-to-end: build a `Config` with an `openrouter` upstream + a
  `openrouter_scored` route against a mock OpenRouter server, call
  `Router::from_config`, then `dispatch()`, and assert a request reaches
  the mock server for one of the cached free models.
- Files: `src/routing/router.rs`

##### Task 4.3.1d: Symmetric kind-vs-strategy validation (~5 min)
- Add a validation pass in `Router::from_config`, run for **every** route
  before/alongside strategy construction (so it also covers
  `Strategy::Fallback`/`Weighted` routes that never reach the
  `OpenrouterScored` match arm at all): for each route, resolve its
  `upstreams` to their `UpstreamKind`s; if any resolves to
  `UpstreamKind::Openrouter` and `route.strategy != Strategy::OpenrouterScored`,
  return `Err` naming the route and the offending upstream (e.g.
  `anyhow!("route '{route_name}' references openrouter-kind upstream '{upstream_name}' under strategy {strategy:?}; openrouter-kind upstreams may only be used with strategy = \"openrouter_scored\"")`).
  This check is independent of — and runs regardless of — whether the
  `OpenrouterScored` match arm's own reverse-direction check (Task 4.3.1b)
  fires, since a `Fallback`/`Weighted` route never enters that arm.
- Files: `src/routing/router.rs`

##### Task 4.3.1e: Unit tests for the symmetric validation (~4 min)
- Cover all 3 symmetric-direction acceptance-criteria GWTs above
  (`Fallback` + openrouter upstream → `Err`; `Weighted` + openrouter
  upstream → `Err`; mixed-upstream `Fallback` route → `Err`), plus a
  regression check that the existing reverse-direction test (Task 4.3.1b)
  and the two positive-path tests (Task 4.3.1c and this story's first
  acceptance criterion) still pass unmodified.
- Files: `src/routing/router.rs`

##### Task 4.3.1f: Pool-exhaustion dispatch integration test (~5 min)
- Resolves a cross-artifact consistency-check finding: Story 4.2.2's
  `select()` tests (`select_should_return_none_when_no_healthy_candidates`)
  only prove `select()` returns `None` in isolation on an empty slice — they
  never exercise the full `Router::dispatch` path end-to-end, which is what
  requirements.md Success Metrics #4 / Scope #6 actually promises. Add
  `dispatch_should_return_exhausted_when_all_free_model_candidates_are_cooling_down`
  (named to match validation.md's REQ-6 row in its Requirement → Test
  Mapping) as a real `Router::dispatch` integration test: build a `Router`
  wired to an `OpenrouterScoringStrategy` over a warm 3-model
  `ModelListCache` snapshot, trip `HealthRegistry`'s cooldown for the shared
  upstream index (e.g. reusing Task 4.2.3c's rate-limited-response setup, or
  driving one dispatch through a mock 429 with `Retry-After` first), call
  `dispatch()`, and assert it returns `Err(ProviderError::Exhausted)` with
  zero requests recorded against any other upstream. This closes the gap
  between validation.md (which already named/scoped this test) and plan.md
  (which, before this task, never asked for it).
- Files: `src/routing/router.rs`

##### Task 4.3.1g: Reject an `openrouter_scored` route that mixes in a paid upstream (~4 min)
- Resolves the adversarial-review Concern: a route mixing an
  `openrouter_scored`-strategy upstream with a paid upstream in the same
  route had no config-time guard, risking silent paid fallback on
  free-pool exhaustion. Extend Task 4.3.1d's validation pass: for a route
  whose `strategy == Strategy::OpenrouterScored`, if its `upstreams` list
  contains any entry resolving to a `UpstreamKind` other than
  `Openrouter`, return `Err` naming the route and the offending
  non-openrouter upstream, e.g.
  `anyhow!("route '{route_name}' uses strategy = \"openrouter_scored\" but upstream '{upstream_name}' is not openrouter-kind; mixing a scored free-model pool with a paid upstream in one route risks silent paid fallback on exhaustion — configure the paid upstream as a separate route instead")`.
  Add a unit test `from_config_should_reject_openrouter_scored_route_mixing_paid_upstream`
  covering this story's reverse-mixed-upstream acceptance criterion above.
- Files: `src/routing/router.rs`

---

## Phase 5: Observability

### Epic 5.1: `to_metrics_json` + `RequestDetail` + Structured Log Line

#### Story 5.1.1: `OpenrouterScoringStrategy::observability_snapshot()`
**As** the `/metrics` endpoint, **I want** per-model score breakdowns and
model-list cache state, **so that** Tyler can see why the strategy is
routing the way it is without reading logs.

**Acceptance Criteria**:
- Returns `Some(json!({"cache": {...}, "models": {...}}))` with cache state
  `{cached_model_count, age_secs, last_refresh, last_invalidation_reason}`
  and one `models` entry per model in `last_scores`
  (`{latency_p50_ms, error_rate, bench_rank, composite_score,
  sample_count}`).
  - *Given* an `OpenrouterScoringStrategy` after one `select()` call over 2
    candidates, *when* `observability_snapshot()` is called, *then* the
    returned JSON's `models` object has exactly 2 keys matching those 2
    model ids, each with all 5 fields present.

**Files**: `src/routing/openrouter_scoring.rs`

##### Task 5.1.1a: Implement `observability_snapshot()` (~4 min)
- Build the JSON from `self.last_scores` and `self.model_cache`'s state
  accessors (`last_refresh`, `last_invalidation_reason`,
  `snapshot().map_or(0, |s| s.len())`).
- Files: `src/routing/openrouter_scoring.rs`

##### Task 5.1.1b: Unit test (~3 min)
- Cover the acceptance criterion above.
- Files: `src/routing/openrouter_scoring.rs`

#### Story 5.1.2: Merge into `GET /metrics`
**As** `GET /metrics`'s caller (the dashboard), **I want** the
`openrouter_scoring` block merged in exactly like `cooldowns` already is,
**so that** it's visible without a separate endpoint.

**Acceptance Criteria**:
- `GET /metrics`'s response includes `result["openrouter_scoring"]` when
  the active route's strategy is `OpenrouterScoringStrategy`, and omits the
  key entirely otherwise (not `null`).
  - *Given* an active `FallbackStrategy` route, *when* `GET /metrics` is
    called, *then* the response has no `openrouter_scoring` key.
  - *Given* an active `OpenrouterScoringStrategy` route, *when* `GET
    /metrics` is called, *then* the response's `openrouter_scoring` key
    matches `observability_snapshot()`'s output.

**Files**: `src/routing/router.rs`, `src/entrypoint/observability.rs`

##### Task 5.1.2a: `Router::openrouter_scoring_snapshot()` (~2 min)
- `pub fn openrouter_scoring_snapshot(&self) -> Option<serde_json::Value> { self.strategy.observability_snapshot() }`,
  placed near `cooldown_snapshot` (`src/routing/router.rs:384-398`).
- Files: `src/routing/router.rs`

##### Task 5.1.2b: Merge in the HTTP handler (~2 min)
- In `get_metrics` (`src/entrypoint/observability.rs:22-26`):
  ```rust
  if let Some(scoring) = state.dispatch_router.load().openrouter_scoring_snapshot() {
      result["openrouter_scoring"] = scoring;
  }
  ```
- Files: `src/entrypoint/observability.rs`

##### Task 5.1.2c: Unit tests (~4 min)
- Cover both acceptance criteria, mirroring the existing `cooldowns`
  handler test pattern.
- Files: `src/entrypoint/observability.rs`

#### Story 5.1.3: `RequestDetail.selected_model`
**As** the dashboard's "Recent Requests" table, **I want** to see which
specific free model a request actually dispatched to, **so that**
per-request auditability doesn't require cross-referencing logs.

**Acceptance Criteria**:
- `RequestDetail` gains `pub selected_model: Option<String>`, populated
  from `chosen.model.clone()` once a candidate is selected, `None` for
  every existing non-model-pinned route (backward compatible — existing
  `RequestDetail` JSON consumers see one new, additive field).
  - *Given* a dispatch that selects a per-model candidate
    `UpstreamRef{model: Some("a/b:free"), ..}`, *when* the request
    completes, *then* its `RequestDetail.selected_model ==
    Some("a/b:free".to_string())`.
  - *Given* a dispatch on `FallbackStrategy` (candidates always have
    `model: None`), *when* the request completes, *then*
    `RequestDetail.selected_model == None`.

**Files**: `src/metrics/mod.rs`, `src/routing/router.rs`

##### Task 5.1.3a: Add the field (~2 min)
- Add `pub selected_model: Option<String>,` to `RequestDetail`
  (`src/metrics/mod.rs:32-56`), defaulted to `None` in `from_body`
  (`src/metrics/mod.rs:105-123`, since it's not known until after
  selection).
- Files: `src/metrics/mod.rs`

##### Task 5.1.3b: Populate it post-selection (~3 min)
- In `Router::dispatch`, after `let Some(chosen) = self.strategy.select(&healthy) else { break; };`
  (`src/routing/router.rs:288`), if `chosen.model.is_some()`, update the
  ring-buffer `RequestDetail` for `request_id` — mirroring how
  `update_request_timing` already mutates a `RequestDetail` in place by id
  (`src/metrics/mod.rs:189+`); add a small
  `MetricsCollector::set_selected_model(&self, request_id: &str, model:
  Option<String>)` helper analogous to `update_request_timing`'s existing
  pattern, called once per dispatch attempt with the currently-chosen
  model (last attempt wins, matching how `provider` name already gets
  overwritten across retries).
- Files: `src/metrics/mod.rs`, `src/routing/router.rs`

##### Task 5.1.3c: Unit tests (~4 min)
- Cover both acceptance criteria.
- Files: `src/metrics/mod.rs`, `src/routing/router.rs`

#### Story 5.1.4: Structured log line on selection
**As** Tyler reading logs after the fact, **I want** each selection
decision logged with the chosen model and its score breakdown, **so that**
I can audit routing behavior without querying `/metrics` at the exact
moment it happened.

**Acceptance Criteria**:
- `select()` emits exactly one `tracing::debug!` per call (when it selects
  `Some`), naming the model id and its 4 score fields.
  - *Given* a `select()` call that picks model `"a/b:free"` with
    `composite = 0.73`, *when* the call completes, *then* a `debug`-level
    tracing event was emitted with fields including `model = "a/b:free"`
    and `composite = 0.73` (asserted via a test-scoped `tracing` subscriber
    capturing events).

**Files**: `src/routing/openrouter_scoring.rs`

##### Task 5.1.4a: Add the log line (~2 min)
- At the end of `select()`, once the chosen candidate is known:
  `tracing::debug!(model = %chosen_model, norm_latency, norm_error, bench_score, composite, "openrouter candidate selected");`.
- Files: `src/routing/openrouter_scoring.rs`

##### Task 5.1.4b: Unit test (~3 min)
- Use `tracing-test` or an equivalent in-repo pattern (check for existing
  precedent in the codebase's other `tracing`-emitting tests before adding
  a new dev-dependency) to assert the event fires with the right fields.
- Files: `src/routing/openrouter_scoring.rs`
