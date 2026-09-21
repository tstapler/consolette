# Implementation Plan: auto-model-family

**Feature**: Synthetic family aliases (`auto-coding` free-only + `auto-coding-paid` opt-in paid) resolved per-session to the best real model ID by local error-rate then latency. Stickiness: STICKY-PER-SESSION (pick sticks per session, re-evaluates on member cooldown/exclusion event or every K=50 family resolutions, implementer-tunable).
**Date**: 2026-09-12
**Status**: Ready for implementation
**ADRs**:
- ADR-001 Resolution site A+C (FamilyTable + pre-select resolution in dispatch)
- ADR-002 Per-(upstream, model) stats keying
- ADR-003 FamilyRuntime owned by MetricsCollector; FamilyTable immutable on Router

## Creative Pass (Step 0.5 — alternatives explored)

Three high-level approaches were considered before committing:

1. **A+C — Alias table + resolution step in `Router::dispatch`** (CHOSEN). Strength: single choke point both entrypoints share; strategy stays pure/health-blind, cooldown/failover/session-pin semantics untouched. Weakness: touches the hot dispatch path, so per-request overhead must stay negligible.
2. **Stats-aware `RoutingStrategy` decorator** (REJECTED). Strength: smallest conceptual diff — reuses the existing strategy seam with zero dispatch-loop edits. Weakness: breaks ADR-003's documented pure/health-blind contract by injecting metrics+health into `select()`, and duplicates the health pre-filter.
3. **Delegate to OpenRouter `auto` router** (REJECTED for default; viable only as bounded paid-alias backing). Strength: zero bespoke ranking code; OpenRouter absorbs delist/rotate churn. Weakness: converts the hard free-only invariant into someone-else's config-you-must-get-right, and the pick+reason live on OpenRouter's side so the local "live pick + why" dashboard cannot be served from local stats.

## Domain Glossary

| Term | Definition | Notes |
|---|---|---|
| FamilyAlias | Synthetic model ID a client sends (e.g. `auto-coding`) that names a family instead of a real model. | Matches `body["model"]` verbatim in dispatch. |
| FamilyTable | Config-built map from FamilyAlias to its ordered FamilyMember list + flags. | Built in `from_config`, held as `Arc` on Router; immutable config, rebuilt on hot-swap. |
| FamilyRuntime | Mutable per-family runtime: stats map, TTL denylist, snapshots, counters, probe/hysteresis state. | Owned by MetricsCollector (survives hot-swap); keyed by `(upstream_name, model_id)`. |
| FamilyMember | One resolvable candidate: `(upstream_name, model_id)` pair in config order. | Config order is the cold-start default. |
| FamilyResolver | Pure ranking function: orders healthy FamilyMembers by decayed error-rate, then latency. | Takes a stats snapshot; no I/O, unit-testable. |
| MemberStats | Decayed per-member counters: quality-error EWMA/window + latency via composed `DurationHistogram` + sample count. | Keyed per (upstream_name, model_id); see ADR-002/ADR-003. |
| QualityError | A failure that measures model health: 5xx/transient/timeout; excludes auth, validation, 429. | 429 is backpressure, not quality (pitfalls §6). |
| BackpressureSignal | A 429/rate-limit outcome: triggers cooldown exclusion, never penalizes MemberStats error-rate. | Consumed via HealthRegistry pre-filter. |
| ResolutionSnapshot | Last-resolution record per alias: chosen member + error-rate/latency figures + timestamp + previous pick. | Served to dashboard + `/metrics`. |
| ResolutionCounter | Per-alias `/metrics` counters: resolutions total + fallback-to-default events. | Audits paid-leak guard + all-down bypass. |
| FreeGuard | Config validation rule: a free family rejects non-`:free` (or paid-priced) member IDs unless `allow_paid`. | Enforced at load AND in `post_route` (fail-closed on pricing-unknown non-`:free` IDs; pricing source: vendored `PricingTable::load_default` snapshot). |
| SessionPin | An entry in SessionOverrideStore binding a session to one upstream + optional model. | Always wins over family resolution; threaded through the OpenAI adapter (see Epic 4). |
| ColdStartDefault | Config-order first healthy member served when no member has minimum samples. | Flagged "cold" on dashboard. |
| HysteresisMargin | Minimum delta (error pp / latency %) a challenger must beat the incumbent by to dethrone it. | Prevents flapping. |
| ExplorationProbe | Periodic Nth-request (or X%) routing to a non-pick member so demoted models can recover rank. | Requires bounded windows to move the needle; targets exclude denylisted/cooled members. |
| ExclusionReason | Why a member was ranked out: `cooldown`, `excluded:404`, `excluded:auth-scope`, `cold`. | Shown greyed on dashboard. |
| SafetyNetBypass | Named event when all members are unavailable via cooldown/empty-pool: cooldown bypassed, least-bad member served, banner logged. | Never overrides 429-driven cooldowns to retry a rate-limited upstream, and never serves paid from the free alias. |

## Pattern Decisions

| Component | Pattern Chosen | Source | Alternative Rejected | Reason |
|---|---|---|---|---|
| Membership source | Alias/indirection table in config (`[[model_families]]`) | PoEAA Metadata Mapping; arch research §2-A | Smuggling families alongside Config / per-route ad-hoc keys | Must survive `POST /api/route` hot-swap (validate + rebuild); versionable in conf.d |
| Route gating | Additive opt-in `family: Option<String>` on `Route`; dispatch expands the alias only when the active route names it | Requirements §Risk Control (rollback must be route-side) | Unconditional alias match on `body["model"]` | Unconditional match makes route-swap rollback a no-op while the client still sends the alias |
| Resolution site | Resolution step inside `Router::dispatch` (A+C), after pins, before health-filter | GoF Strategy (keep strategies closed); arch research §2-C; ADR-001 | Stats-aware strategy decorator (B) | Decorator breaks ADR-003 purity and duplicates health filter |
| Family strategy | Family route must be `fallback`; alias requests force `FallbackStrategy` ordering | `WeightedStrategy::select` samples randomly, voiding rank/hysteresis/probe | Leaving strategy unconstrained | Ranked-order expansion assumes first-healthy-wins |
| Entrypoint handling | Single choke point in dispatch; no per-entrypoint rewrite | DRY; arch research §2-D | Rewrite `body["model"]` in chat_completions/messages handlers | Duplicates per entrypoint, bypasses health/cooldown/session context |
| Ranker shape | Pure function over (members, stats snapshot) → ordered list | Type-driven design (parse-don't-validate: resolve alias to ranked type once); ADR-001 | Stateful resolver holding MetricsCollector + HealthRegistry refs | Keeps ranker unit-testable without I/O; health stays a pre-filter |
| Stats storage | `FamilyRuntime` owned by `MetricsCollector` (reachable from dispatch via `self.metrics`): `DashMap<(UpstreamName, ModelId), MemberStats>` + TTL denylist + snapshots + counters + probe/hysteresis state; immutable `FamilyTable` stays Router-owned | PoEAA Identity Map (composite key); ADR-002/ADR-003 | Re-keying existing `ProxyMetrics::upstreams` to per-model; Router-owned stats keyed by upstream index | Breaks dashboard compat; Router-owned state is wiped on every `post_route` rebuild and indices go stale on reorder |
| Error→stats classification | `Timeout`/`Upstream{5xx}` → quality error-bit + latency sample; 429/`RateLimited` → backpressure only (cooldown exclusion, never error-rate); auth/validation → ignored entirely; `ModelUnsupported` → denylist feed, not error-rate | `ProviderError` predicates (`is_rate_limited`, `is_validation`, `is_auth`, `is_transient`) in `src/providers/mod.rs` | Single "failure" bucket | Conflates backpressure/client-errors with model quality; client 400s would quarantine healthy members |
| Decay | Windowed deque via composed `DurationHistogram` (implementer default); EWMA only if the spike's recover-from-demotion test justifies it | stack.md §3; pitfalls §3 | Ranking on lifetime counters verbatim | Lifetime counters never forget; once-bad stays penalized forever |
| Session precedence | Pins-first: pinned single-candidate path bypasses family expansion | Existing `effective_candidates` contract in `src/routing/router.rs` | Family-first with pin as tiebreak | Pin means "use this," not "prefer this" |
| Session key (opencode path) | Session key threaded through the OpenAI adapter: `translate_openai_to_anthropic` carries `metadata` through (or `chat_completions.rs` extracts pre-translate and re-attaches post-translate) | `extract_session_id` reads `body.metadata.user_id`; translation currently drops it | Post-translation extraction only | Pins would stay dead on `/v1/chat/completions`, the named opencode consumer |
| Paid isolation | Explicit per-alias allowlists + FreeGuard validation at load AND in `post_route` (`ConfigError::PaidMemberInFreeFamily`); pricing-unknown non-`:free` IDs fail closed in free families | Type-driven design (illegal states unrepresentable at config type) | Pattern-derived membership at request time (`:free` suffix sniffing); load-time-only guard | Suffix is convention, not billing boundary; `post_route` bypasses load-time checks |
| Safety net | Named all-down bypass (log + banner + counter) scoped to cooldown/empty-pool only, free alias never escalates to paid | LiteLLM bypass-log-line precedent (ux.md §1) | Silent fallback to any available model incl. paid; bypass overriding 429 cooldowns or 404/auth failures | Violates no-new-spend constraint invisibly; retrying a rate-limited upstream hammers backpressure; all-404 ends in the validation immediate-return anyway |
| Denylist scope | Per-member TTL denylist (default TTL 1h) in `FamilyRuntime`, orthogonal to per-upstream-index `HealthRegistry` cooldown | 404 delists one model, 429 cools the shared upstream | Single shared exclusion keyed by upstream | A 429 on member A would wrongly exclude healthy member B on the same upstream |

Tension resolved (stack.md vs architecture.md): stack.md §4 suggested a new stats-ranked `RoutingStrategy` impl; architecture.md §2-B rejects a stats-aware strategy decorator as breaking ADR-003 purity and recommends FamilyTable/FamilyResolver consulted in dispatch. **Decision: architecture.md wins — adopt A+C per ADR-001.** The `RoutingStrategy` trait stays pure and health-blind; family ranking is a pre-select resolution step that returns an *ordered* member list the existing loop/strategy then iterates. No new `RoutingStrategy` impl is added.

## Tech Debt Disposition

| Area | Existing Issue | Disposition | Justification |
|---|---|---|---|
| `Router::dispatch` choke point in `src/routing/router.rs` | God-loop owns read→pins→filter→select→send→record; family adds one more step | Isolate via seam | New `src/routing/family.rs` (FamilyTable + FamilyResolver) + `src/metrics/member_stats.rs`; dispatch gains ~10 lines calling the seam; `Router` gains table via `with_family_table` builder (no `Router::new` signature churn); no Router refactor first (arch §5) |
| `/metrics` keyed by upstream name in `src/metrics/counters.rs` | No per-model dimension; lifetime-only | Isolate via seam | Parallel map in `FamilyRuntime` (ADR-002/ADR-003); existing map untouched; Extend-as-is NOT claimed — the change adds a dimension rather than deepening the violation |
| `DurationHistogram` windowing in `src/metrics/histogram.rs` | Only windowing precedent; latency means elsewhere are lifetime | Reuse by composition | `MemberStats` composes `DurationHistogram` per member (no shape copying); reuses tested windowing/percentile code |
| `config/load.rs` conf.d Vec merge | Must confirm `families: Vec` layers like routes/upstreams | Verify in Epic 1 task | Single-fragment convention (`20-family.toml`); figment array-replace clobbers split fragments (same pre-existing footgun as `[[upstreams]]`) — document, two-file test |
| Only-first-route-honored in `Router::from_config` | Family entry must be reachable regardless of route order | Isolate via seam | Alias expansion gated on the active route's opt-in `family` field, independent of route order (arch §3); no multi-route refactor in this feature |

## Migration Plan

No schema/data migration: stats are in-memory only. Restart-amnesia handling (pitfalls §9):

- Accept amnesia explicitly: restarts reset rankings to ColdStartDefault (config order).
- Dashboard always shows sample counts + stats-window age so "best with n=3 since restart 10 min ago" reads as low-confidence.
- Optional follow-up (out of scope for this plan): persist compact EWMA snapshots to disk on interval/shutdown.

## Observability Plan

- **Logs**: per-resolution `tracing::info!` (sampled or pick-change-only to avoid log spam): `alias= chosen= runner_up= margin_err_pp= margin_lat_pct= samples= reason=`; SafetyNetBypass logs a WARN with alias + served member + time; pick-change events logged so flapping is visible; every manual `POST /api/route` swap logged with timestamp (swap count is the success-metric leading indicator).
- **Metrics (`/metrics` JSON)**: new `family` section: per alias `{ current_pick, error_rate, latency_p50_ms, samples, window_age_s, last_change_at, previous_pick, members: [{model, error_rate, latency_p50_ms, samples, status}] }` + `resolutions_total` + `fallback_to_default_total` per alias (ResolutionCounter). Existing `providers`/`provider_latency` sections unchanged.
- **Dashboard**: one family card per alias at top of page (above stat-cards): alias → current real model ID (copy-pasteable text, server-rendered, JS text-swap polling ≤30s) + error% / p50 + last-change + previous pick + window age; ranked member table (error-rate, latency, status label `active|cooldown|excluded:<reason>|cold`); safety-net banner when bypass fires; paid card visually distinct (`may spend` + `paid resolutions: N`); link card to `GET /api/route`.
- **Alerts**: none (single user, no oncall); dashboard edge banners are the alerting surface.

## Risk Control

- **Feature flag/gating**: family ships as an opt-in route entry (`family: Option<String>` on `Route`, additive + `deny_unknown_fields`-safe): dispatch expands the alias ONLY when the active route names it — pinned entries remain the working default until the family card proves stable. No config flag needed beyond the route field + alias present in `[[model_families]]`; absent field or absent alias = zero behavior change.
- **Rollback**: existing `POST /api/route` hot-swap back to a route without the `family` field — instant, no restart (requirements §Risk Control). Because expansion is route-gated, swapping the route truly restores pins even while clients still send `auto-coding` (an un-gated alias would leak verbatim upstream instead). Proven by a hot-swap test: family route → pinned route → outgoing body carries the pin again.
- **Hot-swap state semantics**: `FamilyRuntime` (stats, 1h-TTL denylist, snapshots, counters, probe/hysteresis state) lives in `MetricsCollector` and survives `post_route` rebuilds (same carry-across precedent as `with_session_overrides`); immutable `FamilyTable` is rebuilt from config. Denylist and counters therefore persist across the blessed rollback path; only membership/flags reset to the newly-loaded config.
- **Staged rollout**: (1) free alias with 2 members, dashboard card on, pins still default — enable only after the shared-upstream-429 + concurrency-cap tests are green (pre-mortem FM2); (2) point opencode `consolette` provider at alias for one session; (3) all sessions; (4) paid opt-in alias last, with FreeGuard tests green.

## Unresolved Questions

- [x] **Alias naming + family count** — RESOLVED-CONFIRMED: `auto-coding` (free) + `auto-coding-paid` (paid opt-in). Epic 1 Story 1.1 and Epic 6 Story 6.1 proceed on these strings. Owner: Tyler (decided).
- [ ] **Stat decay design (EWMA α≈0.2–0.3 vs 15-min window)** — Epic 2 Story 2.1 implementer default (decided-with-default, not blocking): windowed deque via composed `DurationHistogram` unless the spike's recover-from-demotion test justifies EWMA. Owner: implementer spike (criterion: recover-from-demotion test + 50-line budget).
- [x] **Session-vs-request granularity (stickiness)** — DECIDED: STICKY-PER-SESSION. Family pick sticks per session with periodic re-evaluation (re-evaluate on member cooldown/exclusion event or every K family resolutions, K default 50, implementer-tunable). Epic 4 Story 4.2 implements auto-stickiness via a SessionOverrideStore-compatible mechanism; explicit pin/move still available. Owner: Tyler (decided).
- [x] **Hysteresis margins + exploration budget values** — RESOLVED-CONFIRMED: challenger must beat incumbent by err delta >2pp AND latency >10%; exploration probe every 25th request; per-member in-flight concurrency cap 2–4. Implementer-tunable within those defaults. Owner: Tyler (decided).
- [x] **Minimum-sample threshold before stats count** — RESOLVED-CONFIRMED: N≥20 with Wilson-interval non-overlap gate (or err delta >5pp at n<30); below threshold → config order flagged `cold`. Owner: Tyler (decided).
- Resolved pre-implementation: FreeGuard pricing-unknown rule (fail-closed for non-`:free` IDs absent from the vendored `PricingTable::load_default` snapshot; `:free`-suffixed unknown IDs accepted since rotation mints new `:free` IDs); denylist TTL default 1h; stats key `(upstream_name, model_id)`; SafetyNetBypass scoped to cooldown/empty-pool only.

## Dependency Visualization

```
E1 (config schema + validation)
 └─> E2 (per-model decayed stats) ──> E3 (dispatch resolution)
 │        └─> E5a (/metrics family section) ──> E5b (dashboard card)
 └─> E3 ──> E4 (session pin/move) ──> E6 (opencode IDs + docs)
 └─> E1 (FreeGuard) ──> E7 (paid opt-in alias)
E3 ──> E5a (ResolutionSnapshot source)

Legend: E1 must land before E2/E3/E7; E2 before E3; E3 before E4; E5a before E5b; E6 after E3+E4.
```

---

## Phase 1 — Membership & Trust Foundation (Epics 1–2)

### Epic 1 — Config schema + validation for families (free-only guard)

**Story 1.1** — As Tyler, I want to declare families in conf.d TOML, so that membership is versioned and hot-swappable.

Acceptance Criteria:
- AC1: Given a `20-family.toml` with `[[model_families]] alias="auto-coding"`, when the proxy loads conf.d, then `Config.families` contains the alias with members in declared order.
  - Files: `src/config/schema.rs`, `src/config/load.rs`
- AC2: Given a family member referencing an unknown upstream, when `POST /api/route` validates, then it rejects with the upstream name in the error.
  - Files: `src/entrypoint/api.rs` (`post_route`), `src/config/validate.rs` (`validate_references`)
- AC3: Given the active route has no `family` field and the client sends `model="auto-coding"`, when dispatch runs, then no family expansion happens (alias leaks verbatim per existing pin semantics); given the route sets `family="auto-coding"`, when dispatch runs with the same body, then the alias resolves to a ranked member.
  - Files: `src/config/schema.rs` (`Route.family`), `src/routing/router.rs` (dispatch gate)
- AC4: Given a family route active and a `POST /api/route` hot-swap back to a pinned route, when the client still sends `model="auto-coding"`, then the outgoing body carries the pin's model ID again (rollback truly restores pins).
  - Files: `src/entrypoint/api.rs` (`post_route`), `src/routing/router.rs`

Tasks:
1. Add `FamilyMember{upstream,model}` + `ModelFamily{alias,members,allow_paid}` structs + `families: Vec` on Config with deny_unknown_fields (~5 min; `src/config/schema.rs`).
2. Add additive `family: Option<String>` on `Route` (`#[serde(default)]`, `deny_unknown_fields`-safe) (~3 min; `src/config/schema.rs`).
3. Verify conf.d Vec merge layers fragments; single-fragment convention (`20-family.toml`) + two-file clobber test documenting figment array-replace (~5 min; `src/config/load.rs`, `tests/` or inline).
4. Extend `validate_references` to cover family member upstream names (~3 min; `src/config/validate.rs`).
5. Build `FamilyTable` in `Router::from_config` (NOT `RuntimeOverrides::apply`, which only replaces routes — families ride conf.d and rebuild every `post_route`) + rebuild-path test: families survive hot-swap via config reload; scope-clear denylist entries whose `(upstream, model)` left the rebuilt FamilyTable (drop them) (~5 min; `src/routing/router.rs`, `src/routing/family.rs`).
6. Route-gate + rollback tests: un-gated alias untouched; gated alias resolves; hot-swap to pinned route restores pins (~5 min; `src/routing/router.rs`, `src/entrypoint/api.rs`).

**Story 1.2** — As Tyler, I want the free family to provably reject paid IDs, so that default routing never spends money.

Acceptance Criteria:
- AC1: Given a free family (`allow_paid=false`) listing `anthropic/claude-paid-model`, when config loads, then validation fails naming the offending member.
  - Files: `src/config/schema.rs`, `src/config/validate.rs`
- AC2: Given the paid alias with `allow_paid=true` listing the same ID, when config loads, then it succeeds.
  - Files: `src/config/schema.rs`
- AC3: Given a free family listing a rotated unknown ID `cohere/new-model:free` (absent from the vendored `PricingTable::load_default` snapshot), when config loads, then it succeeds (`:free` suffix = fail-open for rotation); given a free family listing unknown `anthropic/mystery-model` (no `:free` suffix, pricing `None`), when config loads, then it fails closed.
  - Files: `src/config/validate.rs`, `src/cost_metrics/pricing.rs` (`PricingTable::price_for`)
- AC4: Given a `POST /api/route` whose candidate config injects a paid member ID into the free family, when posted, then it is rejected with 400 naming the alias + member (mirrors `post_route_rejects_unknown_upstream`).
  - Files: `src/entrypoint/api.rs` (`post_route`), `src/config/mod.rs` (`ConfigError::PaidMemberInFreeFamily`)

Tasks:
1. Implement FreeGuard: free family rejects IDs not ending `:free` unless the vendored pricing snapshot (`PricingTable::load_default`, read synchronously) says free; pricing-unknown non-`:free` IDs fail closed in free families, accepted in `allow_paid` families (~5 min; `src/config/schema.rs`, `src/config/validate.rs`, `src/cost_metrics/pricing.rs`).
2. Add `ConfigError::PaidMemberInFreeFamily{alias, member}` variant + invoke FreeGuard inside `post_route` alongside `validate_references` (~3 min; `src/config/mod.rs`, `src/config/validate.rs`, `src/entrypoint/api.rs`).
3. Unit + hot-swap tests: paid-in-free rejected (load + `post_route`), paid-in-paid accepted, `:free` accepted, unknown-`:free` accepted (rotation), unknown non-`:free` rejected in free / accepted in paid (~5 min).

### Epic 2 — Per-model decayed stats dimension

**Story 2.1** — As the resolver, I want decayed per-member error-rate + latency, so that a recovered model can outrank its history.

Acceptance Criteria:
- AC1: Given member `cohere/north-mini-code:free` with 10 recent successes after 50 old errors, when ranking reads MemberStats, then its error-rate reflects mostly the recent window (old errors decayed).
  - Files: `src/metrics/member_stats.rs`
- AC2: Given 429 responses recorded, when reading the quality error-rate, then 429s are excluded from it (backpressure never penalizes error-rate; per-class write table: `Timeout`/`Upstream{5xx}` → error-bit + latency sample; 429 → neither error-bit nor latency blame, only cooldown; auth/validation → ignored; `ModelUnsupported` → denylist feed only).
  - Files: `src/metrics/member_stats.rs`, `src/providers/mod.rs` (predicates `is_rate_limited`, `is_validation`, `is_auth`, `is_transient`)
- AC3: Given a `POST /api/route` hot-swap, when the router rebuilds, then `FamilyRuntime` stats/denylist/snapshots/counters/probe state are intact (owned by `MetricsCollector`, keyed by `(upstream_name, model_id)` — never positional index), while `FamilyTable` rebuilds from the new config.
  - Files: `src/metrics/mod.rs` (`MetricsCollector`), `src/routing/family.rs`

Tasks:
1. Scaffold `src/metrics/member_stats.rs`: `MemberStats` type (latency via composed `DurationHistogram`, not a copied shape) + `MemberStatsMap = DashMap<(String,String), MemberStats>` keyed by (upstream_name, model_id) + record fn (~5 min).
2. Implement decay per chosen design (windowed deque default; EWMA if spike wins) for error bit + latency (~5 min).
3. Implement the per-class write table via existing `ProviderError` predicates: Timeout/Upstream{5xx} → error-bit + latency; 429 → cooldown only; auth/validation → drop; ModelUnsupported → denylist feed (~3 min; `src/providers/mod.rs` predicates reused).
4. Own `FamilyRuntime` by `MetricsCollector` (stats map, 1h-TTL denylist, snapshots, counters, probe/hysteresis state); dispatch reaches it via `self.metrics` so `post_route` rebuilds preserve learning (~5 min; `src/metrics/mod.rs`, `src/routing/router.rs`).
5. Unit tests: decay-forgives, 429-excluded, auth-excluded, hot-swap-carry-over, reorder-stable keying (~5 min).

**Story 2.2** — As the resolver, I want stats fed from the dispatch attempt path with the resolved model ID, so that members sharing one upstream get separate buckets.

Acceptance Criteria:
- AC1: Given two family members via upstream `openrouter` (`model-a:free`, `model-b:free`), when 3 requests serve a and 1 serves b, then the map holds two buckets with counts 3 and 1.
  - Files: `src/routing/router.rs`, `src/metrics/member_stats.rs`
- AC2: Given an existing dashboard `/metrics` consumer, when the new dimension records, then `providers`/`provider_latency` sections are byte-identical in shape.
  - Files: `src/metrics/counters.rs`

Tasks:
1. Thread resolved model ID into the `record_attempt` call site in `src/routing/router.rs` (all error-class arms of the dispatch loop) → dual-write to MemberStatsMap keyed by (upstream_name, resolved model_id) (~5 min; `src/routing/router.rs`).
2. Minimum-sample threshold (CONFIRMED): below N=20 stats report `cold` (unknown ≠ perfect); promotion above threshold additionally requires Wilson-interval non-overlap (or err delta >5pp at n<30) so one stray 500 can't flip the pick (pre-mortem FM1) (~5 min; `src/metrics/member_stats.rs`).
3. Regression test: existing per-upstream counters unchanged (~3 min).

---

## Phase 2 — Resolution & Session Dynamics (Epics 3–4)

### Epic 3 — Family resolution in dispatch (pins-first, pre-dispatch exclusion, 429-as-backpressure)

**Story 3.1** — As Tyler, I want alias requests resolved to the lowest-error then lowest-latency healthy member, so that degradation is absorbed automatically.

Acceptance Criteria:
- AC1: Given alias `auto-coding` with members A (err 12.5% n=32) and B (err 0% n=30) both healthy on a `fallback` family route, when dispatch runs, then outgoing body model is B's ID.
  - Files: `src/routing/family.rs`, `src/routing/router.rs` (dispatch resolution step)
- AC2: Given no stats (cold), when dispatch runs, then outgoing model is the config-order first healthy member and snapshot flags `cold`.
  - Files: `src/routing/family.rs`, `src/routing/router.rs`
- AC3: Given two family members sharing upstream `openrouter` (`model-a:free` errors transiently, `model-b:free` healthy), when one request dispatches, then both members are tried in that request (attempt-tracking is per-(upstream, model), not per upstream index, so the second same-index member is not filtered by `already_tried`).
  - Files: `src/routing/router.rs` (dispatch loop `already_tried`/`strategy.select` interaction)
- AC4: Given a family route with `weighted` strategy, when config loads, then validation rejects (or dispatch forces `FallbackStrategy` for alias requests) — ranked order is meaningless under `WeightedStrategy::select` random sampling.
  - Files: `src/config/validate.rs`, `src/routing/router.rs`

Tasks:
1. Scaffold `src/routing/family.rs`: `FamilyTable::from_config` + `FamilyResolver::rank(alias, healthy_set, snapshot)` pure fn (~5 min).
2. Wire `Router::from_config` to build `Arc<FamilyTable>`; expose a `with_family_table` builder (empty-table default) so `Router::new` test call sites don't churn (~5 min; `src/routing/router.rs`, `src/entrypoint/observability.rs` harness).
3. Insert resolution in `dispatch` after `effective_candidates`, before health-filter, gated on the active route's `family` field: alias match → ranked member list → existing loop iterates (~5 min; `src/routing/router.rs`).
4. Key family-expanded attempt-tracking per-(upstream, model) so same-upstream members are each addressable in one request (~3 min; `src/routing/router.rs`).
5. Record ResolutionSnapshot (ArcSwap publish) + ResolutionCounter increments (~3 min).
6. Tests: ranked-pick, cold-default, same-upstream failover (A fails → B tried same request), weighted-family rejected/forced-fallback (~5 min).

**Story 3.2** — As Tyler, I want dead/delisted IDs excluded before dispatch, so that a repeat request to a 404 member never fails the whole request (first request after a fresh delist still fails — then feeds the denylist).

Accepted limitation (documented in user docs): providers map all 4xx including 404 to `ProviderError::Validation`, and dispatch returns immediately on `is_validation` with no failover — so request N hitting a newly-delisted member fails; requests N+1.. skip it via the denylist until the 1h TTL expires.
Acceptance Criteria:
- AC1: Given member `gone/model:free` denylisted (prior 404) and member B healthy, when dispatch runs, then the outgoing model is B and no request is sent to the dead ID.
  - Files: `src/routing/family.rs`, `src/routing/router.rs` (dispatch resolution step)
- AC2: Given member A newly delisted (no denylist entry yet), when the first request dispatches to A and gets `Validation(_, 404)`, then that request fails AND A is denylisted; when the next request dispatches, then the outgoing model is B.
  - Files: `src/routing/family.rs`, `src/routing/router.rs`
- AC3: Given a `Validation(_, 400)` (client-caused) on member A, when dispatch handles it, then A is NOT denylisted — only `Validation(_, 404)` feeds the denylist (404-vs-400 discrimination on the `Validation(String, u16)` status).
  - Files: `src/routing/family.rs`, `src/providers/mod.rs` (`is_validation`)
- AC4: Given all members excluded by 404-denylist, when dispatch runs, then SafetyNetBypass does NOT fabricate a success: the validation error surfaces (bypass covers cooldown/empty-pool only, never 404/auth/validation); given all members unavailable via cooldown/empty-pool, when dispatch runs, then bypass serves least-bad + increments fallback counter + logs WARN, preferring a non-429-cooled member and never a paid ID from the free alias.
  - Files: `src/routing/family.rs`, `src/routing/router.rs`
- AC5: Given member A 429-cooled (per-upstream-index `HealthRegistry` cooldown on shared upstream `openrouter`) and member B on the same upstream healthy per-member, when dispatch runs, then B is still eligible via the per-member denylist path (cooldown excludes the index; denylist excludes the member) — and SafetyNetBypass never overrides the 429-driven cooldown to retry the rate-limited upstream.
  - Files: `src/routing/health.rs` (`HealthRegistry`), `src/routing/family.rs`

Tasks:
1. Per-member hard-failure denylist with 1h TTL in `FamilyRuntime` (404 → exclude; writer discriminates `Validation(_, 404)` only; `ModelUnsupported` also feeds it); on every `post_route` rebuild, drop denylist entries whose `(upstream, model)` left the rebuilt FamilyTable (~5 min; `src/routing/family.rs`).
2. SafetyNetBypass scoped to cooldown/empty-pool: least-bad + WARN + `fallback_to_default_total++`, never paid-from-free, never overriding 429 cooldowns, never masking 404/auth/validation (~3 min).
3. Tests A — denylist basics: first-request-fails-then-denylisted, 404-excluded-pre-dispatch, 400-never-quarantines (~4 min).
4. Tests B — bypass + paid isolation: cooldown-bypass, 404-all-down-still-errors, free-never-escalates (~4 min).
5. Tests C — shared-upstream-429 gate: B stays eligible while A 429-cools on shared upstream `openrouter`; bypass doesn't hammer A (~4 min).

**Story 3.3** — As Tyler, I want no flapping and no winner-takes-all herd, so that picks stay calm under burst traffic. Confirmed values: hysteresis err delta >2pp / latency >10%; probe every 25th request; per-member concurrency cap 2–4.

Acceptance Criteria:
- AC1: Given incumbent A (p50 2.0s) and challenger B (p50 1.9s, 5% better), when HysteresisMargin requires err >2pp and latency >10%, then the pick stays A.
  - Files: `src/routing/family.rs`
- AC2: Given 100 family requests with probe-every-25th, when counted, then ≥3 go to a non-pick member and each probe resolution logs `reason=probe` with the probed member ID.
  - Files: `src/routing/family.rs`
- AC3: Given a burst of 10 parallel family requests with per-member cap 4, when dispatched, then no member exceeds 4 in-flight (overflow routes to the sibling).
  - Files: `src/routing/family.rs`, `src/routing/router.rs`
- AC4: Given the family resolution seam wired in dispatch, when the resolution-overhead micro-benchmark runs vs the static-pin baseline at family sizes ≤8, then p99 overhead is ≤1ms (budget met before rollout).
  - Files: `src/routing/family.rs` (rank fn bench), `src/routing/router.rs` (dispatch seam)

Tasks:
1. HysteresisMargin: challenger must beat incumbent by err >2pp + latency >10% (~3 min).
2. ExplorationProbe: every 25th request routes to best non-pick EXCLUDING denylisted/cooled members for sampling; probe resolutions tagged `reason=probe` in logs (~3 min).
3. Pick-change event log line (alias, from→to, margins) (~2 min).
4. Per-member in-flight concurrency cap (default 2–4) so bursts spread instead of herding onto the pick (pre-mortem FM2); family route stays disabled until this + the shared-upstream-429 test are green (~5 min).
5. Perf-budget task (C11): measure family resolution overhead vs static-pin baseline (rank micro-bench + dispatch-seam timing), record the measured numbers and the budget in the Epic 5 rollout note; family route stays disabled until the budget is met (~5 min).

### Epic 4 — Session pin/move dynamics (STICKY-PER-SESSION decided)

**Story 4.1** — As Tyler, I want existing session pins to beat family resolution, so that a pinned debugging session never surprises me.

Acceptance Criteria:
- AC1: Given session `s1` pinned to upstream `openrouter` model `model-a:free` and alias `auto-coding` would pick B, when `s1` requests via `/v1/messages` with `metadata.user_id="s1"`, then outgoing model is `model-a:free`.
  - Files: `src/routing/router.rs` (`effective_candidates`)
- AC2: Given `DELETE /api/sessions/s1/route` clears the pin, when `s1` requests again via `/v1/messages` with `metadata.user_id="s1"`, then family resolution applies (resolves to current pick B, then sticks for `s1`).
  - Files: `src/routing/router.rs`, `src/entrypoint/api.rs`
- AC3: Given session `s1` pinned to `model-a:free` and an opencode-shaped `POST /v1/chat/completions` carrying the session key (`metadata.user_id="s1"` or the OpenAI-native `user` field), when dispatched, then outgoing model is `model-a:free` — pins apply on the opencode path, not just `/v1/messages`.
  - Files: `src/entrypoint/chat_completions.rs`, `src/providers/mod.rs` (`translate_openai_to_anthropic`), `src/routing/session_overrides.rs` (`extract_session_id`)

Tasks:
1. Assert pins-first order in dispatch (pinned single-candidate bypasses family expansion) + regression test (~3 min; `src/routing/router.rs` `effective_candidates` + test).
2. Thread the session key through the OpenAI adapter: carry `metadata` through `translate_openai_to_anthropic` (or extract pre-translate in `chat_completions.rs` and re-attach post-translate before dispatch) so `extract_session_id` sees it (~5 min; `src/providers/mod.rs`, `src/entrypoint/chat_completions.rs`).
3. Opencode-shaped session-pin integration test: pinned session via `/v1/chat/completions` sticks to the pin; clear-pin → family resumes (~5 min; `src/entrypoint/chat_completions.rs` tests).
4. Verify clear-pin → family-resumes path via existing session endpoints (~3 min).

**Story 4.2** — As Tyler, I want the family pick to stick per session with periodic re-evaluation, so that a conversation never jumps models mid-stream but still follows degradation.

Acceptance Criteria:
- AC1: Given session `s2` with no pin requesting alias `auto-coding` (current pick A), when `s2` requests again 10 times with no cooldown/exclusion event and fewer than K=50 family resolutions elapsed, then all 10 outgoing models are A (sticky-per-session).
  - Files: `src/routing/router.rs`, `src/routing/session_overrides.rs` (SessionOverrideStore-compatible auto-stickiness mechanism)
- AC2: Given session `s2` stuck to A and member A hits a cooldown/exclusion event (or K=50 family resolutions elapse), when `s2` requests again, then the session re-resolves to the current ranked pick (e.g. B) and sticks to B.
  - Files: `src/routing/family.rs`, `src/routing/session_overrides.rs`
- AC3: Given session `s2` auto-stuck to A and member B degraded, when `POST /api/sessions/s2/route {upstream:"openrouter", model:"model-a:free"}` pins explicitly, then `s2` stays on A while other sessions still resolve dynamically; given `GET /api/sessions` lists pins, when `s2` is pinned to a family member, then the entry shows the member model ID verbatim.
  - Files: `src/entrypoint/api.rs`, `src/routing/session_overrides.rs`

Tasks:
1. Implement auto-stickiness via a SessionOverrideStore-compatible mechanism: first family resolution for a session records the pick; later requests reuse it until re-evaluation triggers (member cooldown/exclusion event or every K=50 family resolutions); explicit pin/move still overrides auto-stick (~8 min; `src/routing/session_overrides.rs`, `src/routing/router.rs`) + integration test sticky-then-reevaluates.
2. Document + test explicit pin-to-member flow (endpoint exists) — add integration test (~5 min).
3. Dashboard sessions view: SKIP in favor of the family-card pinned-session count + `GET /api/sessions` link (Epic 5 Story 5.2 covers it); record the skip explicitly (~2 min).

---

## Phase 3 — Visibility & Rollout (Epics 5–7)

### Epic 5 — Dashboard family card + /metrics additions

**Story 5.1** — As Tyler, I want `/metrics` to expose per-alias resolution state, so that the dashboard and audits read one source.

Acceptance Criteria:
- AC1: Given 3 resolutions of `auto-coding` (picks A,A,B), when `GET /metrics`, then `family["auto-coding"].resolutions_total=3`, `current_pick="model-b:free"`, `previous_pick="model-a:free"`.
  - Files: `src/entrypoint/observability.rs`, `src/metrics/mod.rs`
- AC2: Given zero resolutions yet, when `GET /metrics`, then `family` section shows alias with `cold` status, not an error.
  - Files: `src/entrypoint/observability.rs`
- AC3: Given a configured family, when `GET /api/route`, then family entries are marked unambiguously vs pinned entries (alias, members, current pick).
  - Files: `src/entrypoint/api.rs`

Tasks:
1. Add `family` section to `/metrics` JSON read from `FamilyRuntime` snapshots/counters via `EntrypointState.metrics` (NOT the rebuilt Router) (~5 min; `src/entrypoint/observability.rs`, `src/metrics/mod.rs`).
2. Tests for counters + cold-shape (~3 min).

**Story 5.2** — As Tyler, I want a family card atop the dashboard, so that I can trust the automation at a glance.

Acceptance Criteria:
- AC1: Given alias `auto-coding` currently on `model-b:free` (err 0%, p50 1.8s, changed 5 min ago from A), when `GET /dashboard`, then the card shows alias → model ID text + `0% / 1.8s p50` + `last change 5m ago (prev model-a:free)`.
  - Files: `src/dashboard.rs`
- AC2: Given CDN-blocked (no Chart.js), when loading the dashboard, then the card still renders pick + two numbers as plain HTML.
  - Files: `src/dashboard.rs`
- AC3: Given 2 sessions auto-stuck/pinned to family members, when `GET /dashboard`, then the family card shows `pinned sessions: 2` + a link to `GET /api/sessions`.
  - Files: `src/dashboard.rs`

Tasks:
1. Server-render family card HTML at top (above stat-cards) + ranked member table with status labels; every status dot paired with a text label (grayscale-legible, no color-only); card shows pinned-session count + link to `GET /api/sessions` (Epic 4.2 sessions-view skip decision) with `aria-live="polite"` on the pick line for 30s polling (~5 min; `src/dashboard.rs`).
2. JS text-swap polling ≤30s for pick line; no animation reset (~3 min).
3. Safety-net banner + distinct paid-card style + link to `GET /api/route` (~3 min).
4. Edge states: cold-start copy, all-down banner, delisted greyed rows, tie/last-change line, window-age display (~5 min).

### Epic 6 — opencode provider family IDs + docs

**Story 6.1** — As an opencode user, I want the `consolette` provider to offer the family IDs, so that I can select `auto-coding` like a model.

Acceptance Criteria:
- AC1: Given opencode lists `consolette` provider models, when queried, then `auto-coding` (and paid alias) appear with the family label.
  - Files: opencode provider config (repo-external `consolette` provider family definition) + `GET /api/models` passthrough check
- AC2: Given a chat completion with `model="auto-coding"`, when dispatched, then the alias never leaks to OpenRouter verbatim (dispatch resolution overwrites it with the resolved real ID — the alias passes *through* `translate_openai_to_anthropic` into dispatch; test the overwrite, not a pre-translate interception).
  - Files: `src/entrypoint/chat_completions.rs`, `src/routing/router.rs`
- AC3: Given Epic 4 Story 4.1 AC3 green, when opencode traffic sends `model="auto-coding"` with a session key, then session pins apply to `/v1/chat/completions` traffic (Epic 4 must pass for the opencode path, not just `/v1/messages`).
  - Files: `src/entrypoint/chat_completions.rs`, `src/routing/session_overrides.rs`

Tasks:
1. Add family IDs to opencode `consolette` provider family + verify `list_models` surfaces them (~5 min).
2. Assert alias-overwrite-in-dispatch test (no verbatim leak): translated body still carries the alias into dispatch, resolution overwrites it (~3 min).
3. Docs: family copy (`auto-coding (free pool, least-errors first)`), sticky-per-session note (sticks per session, re-evaluates on cooldown/exclusion event or every K=50 family resolutions), first-request-after-delist failure note (Story 3.2 limitation), rollback via POST /api/route with exact curl + route name (~5 min; docs file per repo convention).

### Epic 7 — Paid opt-in alias

**Story 7.1** — As Tyler, I want a separate paid alias, so that I can opt into paid models without risking the free default.

Acceptance Criteria:
- AC1: Given `auto-coding-paid` with `allow_paid=true` and member `anthropic/claude-x`, when it resolves, then outgoing model may be the paid ID and `paid resolutions` counter increments.
  - Files: `src/config/schema.rs`, `src/routing/family.rs`
- AC2: Given the free alias, when all free members are down, then it NEVER resolves to a paid ID (serves least-bad free + safety-net banner).
  - Files: `src/routing/family.rs`

Tasks:
1. Paid alias config + optional OpenRouter `auto` backing (bounded: allowed_models + max_price, cost_tier=low) — config-only (~5 min).
2. Isolation tests: free↔paid stats tables don't leak; per-alias counters separate (~5 min).
3. Paid card distinct style + `paid resolutions: N` display (covered in 5.2; verify here) (~2 min).
