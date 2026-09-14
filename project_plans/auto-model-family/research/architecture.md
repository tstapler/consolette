# Architecture research: auto-model-family

Date: 2026-09-12 · Agent 3 — Architecture · RESEARCH ONLY, no code changed.
Sources: `src/routing/router.rs`, `src/routing/strategy.rs`, `src/routing/health.rs`,
`src/routing/session_overrides.rs`, `src/config/schema.rs`, `src/metrics/counters.rs`,
`src/metrics/mod.rs`, `src/entrypoint/api.rs`. Line refs below.

## 1. Current architecture (what exists)

- **Dispatch loop owns everything per-request** (`router.rs:251-367` `Router::dispatch`):
  `body["model"]` read (260-264) → `effective_candidates()` (267) → health pre-filter
  (282-286) → `strategy.select(&healthy)` (288) → per-candidate `model` override
  stamped onto the outgoing body (307-314) → `provider.send` (316) → `record_attempt`
  (318/336/340/352/360).
- **Strategy is deliberately pure and health-blind** (ADR-003; `strategy.rs:1-6`,
  trait at 25-27: `select(&self, healthy: &[UpstreamRef]) -> Option<UpstreamRef>`).
  `UpstreamRef` (`strategy.rs:17-22`) carries `{index, name, weight, model}` where
  `model` is the pinned override from `RouteUpstreamRef.model` (`schema.rs:149-158`).
- **Health is a separate seam** (`health.rs:32` `HealthRegistry`, keyed by upstream
  *index*; cooldown tripped in dispatch on rate-limit/shape-mismatch only).
  Validation/auth errors **return immediately with no failover** (`router.rs:335-338`) —
  matches requirements "Feasibility Risks": a delisted family member must be excluded
  *before* dispatch, failover will not save it.
- **Session pins short-circuit routing** (`router.rs:219-237` `effective_candidates`):
  a pin replaces the whole candidate list with one `UpstreamRef`. Pins are carried
  across hot-swaps via shared `Arc<SessionOverrideStore>` (`router.rs:120-123`,
  `api.rs:129`).
- **Stats are keyed by upstream *name*, not model** (`counters.rs:44-46`
  `upstreams: DashMap<String, UpstreamCounters>`; `record_attempt` at
  `router.rs:405-438` records by `chosen.name`; the `model: &str` arg is only fed to
  `error_tracker.push`, never to counters). Per-model error-rate/latency ranking
  has **no storage dimension today** — the requirements "Rabbit Holes" call this out
  correctly. `RequestDetail` ring buffer (`metrics/mod.rs:32-56`) has per-request
  `model`+`provider`+timings, usable for display but not as a ranking source without
  new aggregation/windowing.
- **Hot-swap path** (`api.rs:97-133` `post_route`): load conf.d → apply
  `RuntimeOverrides{route}` → `validate_references` → persist
  `runtime-overrides.toml` → `Router::from_config` rebuild → reattach session
  overrides → `ArcSwap` store. Anything the family feature adds to `Config` must
  survive this path (overrides apply + validation + rebuild) or a hot-swap wipes it.
- **Assembly lives in `Router::from_config`** (`router.rs:135-208`): builds one
  `Provider` per upstream, maps `route.upstreams` → `Vec<UpstreamRef>`, picks
  `Fallback`/`Weighted` strategy. Note it only honors the **first** route (163-168).

## 2. Patterns evaluated

| Option | Fit | Verdict |
|---|---|---|
| **A. Alias/indirection table in config** (`[[model_families]]` or equivalent: alias → member `(upstream, model)` list + `allow_paid` flag) | Matches "synthetic family alias" requirement directly; TOML-native; versionable in conf.d; hot-swap compatible if threaded through `Config` | **Adopt** as the membership source |
| **B. Decorator around `RoutingStrategy`** (e.g. `StatsRankedStrategy` wrapping `FallbackStrategy`, holding `Arc<MetricsCollector>`) | Reuses the strategy seam, but breaks its documented contract: strategies are pure/health-blind (`strategy.rs:1-6`). Injecting metrics+health into `select()` couples ranking to a sync pure fn and duplicates the health filter | Reject as primary; acceptable only as a thin ordering wrapper *after* resolution, not as the resolver |
| **C. Resolution step inside `Router::dispatch`** (family alias → ranked member list, then existing loop/strategy proceeds) | Keeps strategy pure, health filtering unchanged, failover/cooldown/admission/session-pin semantics untouched; single choke point both entrypoints (`/v1/messages`, `/v1/chat/completions` → `dispatch`) share | **Adopt** as the resolution site |
| **D. Resolution at the entrypoint** (rewrite `body["model"]` in `chat_completions.rs`/`messages.rs` before `dispatch`) | Would need duplicating in every entrypoint, bypasses health/cooldown/failover context, fights session overrides | Reject |

Recommended composition: **A + C** — a `FamilyResolver` (or `FamilyTable`) struct built
at config-load time, consulted at the top of `dispatch` (right after
`effective_candidates`, before the health-filter loop). When `body["model"]` matches a
family alias, the resolver returns an *ordered* member list (ranked by stats among
healthy members, config order on cold start / ties); the existing loop then iterates
it with the same per-attempt `record_attempt`/cooldown/admission behavior. Session pin
still wins: if `effective_candidates` returned a single pinned ref, skip family
expansion.

## 3. Integration points (concrete)

- **`Router::from_config` (`router.rs:135-208`)** — construct the family table here
  from the new config section and pass `Arc<FamilyTable>` (+ stats handle) into
  `Router::new`. Needs a `Router` field + constructor param; `new` is also called by
  tests/observability harness (`router.rs` tests, `entrypoint/observability.rs:203-222`),
  so default to an empty table there.
- **`SessionOverrideStore` (`router.rs:219-237`)** — order: pins first, family second.
  Pinned single-candidate path bypasses family expansion (a pin means "use this").
- **`HealthRegistry`** — resolver must rank over the *healthy* subset (or consult
  `is_available`/`remaining_secs` when ordering), because validation/auth failures
  don't fail over (`router.rs:335-338`). Dead/delisted IDs excluded pre-dispatch.
- **`MetricsCollector` / `ProxyMetrics::upstreams`** — needs a new per-model (or
  per `(upstream, model)`) stats dimension alongside the existing per-upstream-name
  map; see §4. Also add per-alias counters (`resolutions`, `fallback-to-default`)
  per requirements Observability.
- **`POST /api/route` hot-swap (`api.rs:97-133`)** — keeps working *iff* (a) the
  family section is part of `Config` loaded from conf.d (not smuggled alongside),
  (b) `RuntimeOverrides::apply` + `validate_references` are extended to cover it
  (validate family member upstream names exist), and (c) `from_config` rebuilds the
  resolver. Rollback (POST a pinned route) works unchanged; shipping the family as an
  opt-in route entry preserves the pinned fallback during rollout (requirements Risk
  Control). Note: with only-first-route-honored (163-168), the family entry must be
  reachable — either as the first route during trial or via model-alias matching
  independent of route order (prefer the latter to avoid the multi-route warning trap).
- **Config schema (`schema.rs`)** — new structs, e.g. `ModelFamily { alias, members:
  Vec<FamilyMember{upstream, model}>, allow_paid: bool }` on `Config` as
  `families: Vec<…>`; `deny_unknown_fields` requires explicit fields. Check
  `config/load.rs` conf.d merge semantics for `Vec` fields (routes/upstreams
  precedent) so a `20-family.toml` fragment layers instead of replacing. Membership
  constraint "default family = free only; separate opt-in alias allows paid" is a
  validation rule on this table (reject paid model IDs in the free family unless
  `allow_paid`), not router logic.
- **Entrypoints** — no per-entrypoint change needed under option C; opencode
  `consolette` provider family IDs are client-side config advertising the alias
  strings (e.g. `auto-coding`, paid opt-in alias). Dashboard reads live pick + the
  two ranking signals — needs a "last resolution" snapshot exposed from the resolver
  (alias → chosen member + error-rate/latency figures), plus `/metrics` per-alias
  counters; keep the "why" to exactly those two signals per requirements scope.

## 4. Data flow and consistency (per-model stats gap)

Current flow keys everything by upstream name; two OpenRouter upstreams pinned to two
different `:free` models still aggregate stats *per upstream entry*, and a single
upstream serving multiple family models would conflate them entirely. Required change:

- Add a parallel stats map keyed by model ID (or `(upstream_name, model_id)` if the
  same model can appear behind two upstreams — recommend the pair key, display grouped
  by model). Feed it from the existing `record_attempt` call site, which already has
  both `chosen.name` and the effective `model` string.
- Ranking reads (error-rate, then latency) snapshot these atomics per pick; writes are
  lock-free atomics as today (`counters.rs:6-8` pattern). Negligible per-request
  overhead: one extra `DashMap` lookup/update on the attempt path + one ranked-order
  computation over a handful of members at dispatch start.
- Consistency notes for Phase 3: lifetime counters never decay (requirements Rabbit
  Hole) — design aging/windowing explicitly or a once-bad model is penalized forever;
  cold start (no stats) falls back to config order deterministically; delisted IDs
  need exclusion independent of stats (see feasibility risk on 404-fast-fail).

## 5. Disposition for Phase 3

**Isolate via seam.** ADR-003/ADR-004 boundaries (pure strategy, index-keyed health,
name-keyed metrics, assembly-in-`from_config`) are intact and worth preserving — put
family membership (`FamilyTable` from config) and ranking (`FamilyResolver` over a new
per-model stats dimension) in new modules behind the existing `dispatch` choke point,
rather than refactoring `Router`/`RoutingStrategy` first or stuffing stats-awareness
into the pure strategy trait.
