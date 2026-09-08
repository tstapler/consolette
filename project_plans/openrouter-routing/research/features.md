# Research: Feature Landscape — OpenRouter Provider + Score-Based Free-Model Strategy

Agent 2 (Features), SDD Phase 2, `openrouter-routing`.

## 1. Existing prior art in this codebase

### `RoutingStrategy` trait and the two current implementations (`src/routing/strategy.rs`)
- `RoutingStrategy::select(&self, healthy: &[UpstreamRef]) -> Option<UpstreamRef>` is
  **pure and health-blind** by design (ADR-003,
  `project_plans/consolette/decisions/ADR-003-routing-strategy-trait.md`): it never
  sees cooldown state, only the slice of candidates `Router` has already filtered
  through `HealthRegistry::is_available`. A new scoring strategy slots in exactly
  the same way — implement `select` over `&[UpstreamRef]`, nothing else — and stays
  ignorant of cooldown, admission control, and the retry loop.
- `FallbackStrategy` = `healthy.first().cloned()`. `WeightedStrategy` uses
  `rand::distributions::WeightedIndex` over `UpstreamRef.weight`, with a `.max(1)`
  guard so an all-zero-weight slice still selects uniformly instead of panicking.
  Both are ~30 lines — the bar for "reasonably simple" in this codebase is low; a
  composite-score strategy should stay in that spirit (a `score()` per candidate,
  then max-or-weighted-sample), not grow into a generic pluggable-weights engine
  (explicitly flagged as a rabbit hole in requirements.md).
- `UpstreamRef` currently carries `index`, `name`, `weight`, `model: Option<String>`.
  For OpenRouter, each *free model* is a routing candidate, not each upstream — so
  either `UpstreamRef.model` becomes the per-candidate discriminator (multiple
  `UpstreamRef`s sharing one upstream `index`, differing only in `model`), or a new
  candidate-shape is needed. This directly affects `HealthRegistry`/`DurationHistogram`
  keying (see §2) since both are keyed by upstream `index` today, not `(index, model)`.
- `Router::from_config` (`src/routing/router.rs:191-194`) is a `match route.strategy`
  over a config `Strategy` enum (`Strategy::Fallback | Strategy::Weighted`,
  `src/config/schema.rs:142-145`) that constructs the right `Arc<dyn RoutingStrategy>`.
  A new strategy needs a new `Strategy::ScoredFree` (or similar) variant here, matching
  the existing `deny_unknown_fields` config discipline.

### `HealthRegistry` (`src/routing/health.rs`)
- Per-upstream cooldown, keyed by `usize` index into `Config.upstreams`, backed by
  `DashMap<usize, ProviderState>` with a TOCTOU-safe check-and-clear
  (`is_available` atomically clears an expired cooldown via `DashMap::get_mut`'s
  exclusive per-key guard). Hard rule documented in the module doc comment: never
  hold a `DashMap` guard across `.await` — all ops here are sync `Instant` math.
- `can_cooldown: DashMap<usize, bool>` lets an upstream opt out of cooldown entirely
  (used for Bedrock). Not directly relevant to OpenRouter free models (they should
  cool down), but shows the registry already supports per-upstream behavioral
  variance, which is the right place to look when the free-model pool needs
  different cooldown durations per model or per error class.
- **Keying gap for this feature**: `HealthRegistry` is keyed by upstream `index`,
  which today means one upstream = one provider = one cooldown slot. OpenRouter
  needs cooldown *per free model*, all sharing one upstream/provider/index. This is
  called out explicitly in requirements.md's Rabbit Holes and Feasibility Risks as
  the single biggest structural change: `HealthRegistry` (and the metrics types
  below) need a `(upstream_index, model_id)` composite key, or an equivalent
  per-model sub-registry scoped under the OpenRouter upstream's index. Either path
  touches `HealthRegistry`'s public API (`trip`, `is_available`, `remaining_secs`,
  `set_can_cooldown`) and every call site in `router.rs`.

### `DurationHistogram` / `ErrorTracker` (`src/metrics/histogram.rs`, `src/metrics/error_tracker.rs`)
- `DurationHistogram` is a single global rolling-window (`VecDeque<(Instant, u64)>`,
  default 15 min) with `record()`, `percentiles()` (p50/p95/p99), `requests_per_minute()`,
  and chart-data builders. It is a **global singleton** today (one instance in
  `MetricsCollector`, `src/metrics/mod.rs`), not per-upstream or per-model — this is
  exactly the gap requirements.md's Rabbit Hole #2 names ("Per-candidate metrics
  granularity"). The rolling-window/trim-on-every-op design is simple and directly
  reusable per-model: a `HashMap<String, DurationHistogram>` keyed by model id (or a
  small `DashMap` to match `HealthRegistry`'s concurrency style) gives per-model p50
  latency with no new algorithm, just a new keying layer.
- `ErrorTracker` (`src/metrics/error_tracker.rs`) is fingerprint/dedup-oriented (ports
  the legacy Python proxy's `error_tracker.py`): it normalizes messages, computes a
  `sha256(provider:operation:error_type:normalized_msg)` fingerprint, and keeps a
  100-entry ring buffer of `ErrorRecord`s plus an `AggregatedError` view
  (`count`, `first_seen`, `last_seen`) via `push()`/`get_recent()`/`get_summary()`.
  It already carries a `model: String` field on `ErrorRecord`
  (`src/metrics/error_tracker.rs:110`), so per-model *error occurrence* data already
  exists in the ring buffer — but there's no per-model **rolling error rate**
  (successes vs. failures over a window) computed from it today; the strategy needs
  a new, simpler counter (e.g. a per-model rolling `(attempts, failures)` pair over
  the same kind of time window `DurationHistogram` already uses), not a reuse of
  `ErrorTracker`'s fingerprinting machinery, which solves a different problem
  (dashboard dedup display, not rate-based scoring).
- Both need the same treatment: **generalize from global to per-model**, scoped
  tightly to what the new strategy needs (per requirements.md's explicit scope
  warning — not a general per-upstream-metrics refactor).

### `Router::dispatch` retry/error-classification loop (`src/routing/router.rs:251-369`)
- The loop already does exactly the shape this feature needs for exhaustion: filter
  `candidates` by `!already_tried.contains(&idx) && health.is_available(idx)`, call
  `strategy.select(&healthy)`, and on `None` (empty pool) `break` out to
  `Err(last_error.unwrap_or(ProviderError::Exhausted))` (line 368). **This is already
  the fail-closed behavior the requirement demands** — no fallback to any candidate
  outside the route's configured upstream list happens structurally, because
  `dispatch` only ever draws from `self.candidates`/`effective_candidates`. As long
  as a route configured for the free-model pool doesn't also list a paid upstream as
  a candidate, exhaustion already produces `ProviderError::Exhausted` for free. The
  design risk is entirely in config/candidate construction (making sure the
  discovered free-model list *is* `candidates`, and a paid upstream is never mixed
  into the same route), not in the dispatch loop itself.
- Error-class handling per attempt: `is_validation()`/`is_auth()` → return
  immediately, no failover; `is_rate_limited()` → `health.trip(idx, retry_after)`
  then continue; `is_response_shape_mismatch()` → trip with a longer override
  duration (see Gemini's `DRIFT_COOLDOWN_SECS` pattern) then continue; anything else
  → continue without tripping. **The "model disappeared from OpenRouter's catalog"
  case doesn't fit any existing arm** — OpenRouter returns a 400/404-shaped error for
  an unknown model id, which today would fall into the generic `Err(e) =>` catch-all
  (record + continue, no cooldown trip) unless explicitly classified as
  validation/auth. This needs a design decision in Phase 3: should "model not found"
  both (a) trip a cooldown for that model and (b) trigger early model-list cache
  invalidation (per requirements.md's cache-invalidation scope)? The `is_response_shape_mismatch`
  + `DRIFT_COOLDOWN_SECS` pattern in Gemini's provider is the closest existing
  precedent for "provider-specific error → longer cooldown."

### `SessionOverrideStore` / session pinning (`src/routing/session_overrides.rs`)
- Already ships exactly the "force/pin a specific model for testing" capability
  Tyler is likely to want for the new strategy, at a different layer: a session
  (keyed by the Anthropic Messages API's `metadata.user_id`) can be pinned via
  `POST /api/sessions/{id}/route` to one upstream + optional model override, which
  `Router::effective_candidates` (`router.rs:219-237`) substitutes wholesale for the
  route's normal candidate list before the strategy ever runs. This is strong
  precedent that pinning is a *router-level*, not *strategy-level*, concern — the
  new scoring strategy doesn't need its own pin mechanism; it just needs to keep
  working correctly when `effective_candidates` hands it a single already-pinned
  `UpstreamRef` (i.e., `select` over a 1-element slice should trivially return that
  element, which both existing strategies already satisfy).
- Gap: `SessionOverride.upstream` pins to an *upstream name*, and this feature's
  candidates are all one upstream (`openrouter`) with different `model` values —
  session pinning already supports pinning `model` too (`SessionOverride.model`), so
  "pin session X to free model `deepseek/deepseek-chat:free`" is already
  expressible with zero changes to `session_overrides.rs`. Good example of an
  unstated need that's already solved by existing infrastructure.

### `OpenaiProvider` (`src/providers/openai.rs`)
- `OpenrouterProvider` (per requirements.md's Scope) is explicitly meant to reuse
  the `Provider` trait contract, and `OpenaiProvider` is the closest existing
  shape: `base_url` + `Content-Type` + auth-header injection via
  `anthropic::apply_auth_headers` (shared helper, not Anthropic-specific despite the
  module name), a `fetch_models()` hitting `GET /v1/models` already returning raw
  `Value` (line 167), and both `send_request`/`send_streaming_request` posting to
  `POST {base_url}/v1/chat/completions`. This means the "OpenAI-compatible Chat
  Completions" transport work for OpenRouter is close to zero-effort reuse — the
  real net-new work is (a) OpenRouter-recommended extra headers (`HTTP-Referer`,
  `X-Title` — confirm exact names against OpenRouter's docs in Phase 2/3), (b)
  parsing `/v1/models`' `pricing.prompt`/`pricing.completion` fields to filter
  free (`== "0"`) models, since `fetch_models` today returns the raw body
  uninterpreted, and (c) the model-list cache + TTL/invalidation logic, which has no
  existing precedent anywhere in the codebase (nothing currently caches a
  provider's model list — every other provider's model set is config-static).

### `MetricsCollector::to_metrics_json` (`src/metrics/mod.rs:343-392`)
- Assembles counters, histogram percentiles, RPM/lag chart data, recent
  requests/errors, and a timestamp into one JSON blob; `HealthRegistry` cooldown
  state is merged in separately by the HTTP handler
  (`observability::get_metrics`), not by `MetricsCollector` itself — an explicit
  separation-of-concerns precedent worth following for the new per-model score data
  (compute it in `Router`/the strategy, merge it into the JSON at the handler layer,
  keep `MetricsCollector` itself free-model-agnostic).

## 2. Industry landscape: how other LLM gateways/routers pick a model

- **LiteLLM Router** (`docs.litellm.ai/docs/routing`) ships several selectable
  strategies, most relevantly `latency-based-routing`: a `LowestLatencyLoggingHandler`
  tracks a moving average of latency per deployment — time-to-first-token for
  streaming, total response time for non-streaming — and routes to the
  lowest-average deployment. Cooldowns are per-deployment (`CooldownCache`), not
  per model-group: a deployment crossing a failure-count threshold
  (`allowed_fails`) is pulled from the pool for `cooldown_time` while healthy peers
  keep serving, then rejoins automatically — structurally identical to this
  codebase's `HealthRegistry.trip()`/expiry-based reinstatement. LiteLLM also
  supports `usage-based-routing` (weight by remaining TPM/RPM budget) and
  `cost-based-routing`, but does **not** appear to combine latency + error-rate +
  a static quality/benchmark score into one composite — it treats these as
  separate selectable strategies, not one scoring formula. That's a point in favor
  of keeping this feature's composite-score design simple and bespoke rather than
  hunting for an existing formula to copy.
- **OpenRouter's own provider-routing** (`openrouter.ai/docs/guides/routing/provider-selection`)
  exposes a `provider.sort` request field with three modes — `throughput`,
  `price`, `latency` — plus `preferred_max_latency`/`preferred_min_throughput`
  thresholds computed over **p50/p75/p90/p99 percentiles on a rolling 5-minute
  window**. This routes across *providers serving the same model*, a different axis
  than this feature (routing across *different free models*), but the percentile-
  over-a-rolling-window shape is exactly what `DurationHistogram` already computes,
  reinforcing that per-model p50/p95 from a generalized `DurationHistogram` is a
  reasonable, precedented latency signal to feed the composite score (rather than a
  raw mean, which is more outlier-sensitive).
- **Portkey Gateway** treats model/provider selection as a config-level graph of
  `loadbalance` (weighted, similar to `WeightedStrategy` here), `fallback`
  (ordered, similar to `FallbackStrategy`), and `conditional routing` (route by
  request metadata) — all statically configured, no runtime health/latency-driven
  scoring built into the open-source gateway core. This suggests Tyler's
  requirement (live latency + error-rate + static bench score combined into one
  selection score) is somewhat ahead of what these mainstream gateways ship
  out-of-the-box for free-tier-style pools — there's no drop-in formula to borrow;
  Phase 3's "fix a concrete, simple formula" instruction is the right call, not an
  under-specification.
- **Coding-benchmark tables**: requirements.md already names aider's polyglot
  benchmark and LiveBench as candidate static sources — both publish stable
  numeric scores per model id, but model ids in those tables (e.g.
  `deepseek/deepseek-r1`) will not exactly match OpenRouter's `:free`-suffixed ids
  (e.g. `deepseek/deepseek-r1:free`) or OpenRouter's own naming
  (`openai/gpt-oss-20b:free` vs. a benchmark's `gpt-oss-20b`). The static table's
  keys need a normalization/matching step against OpenRouter's `/models` ids, and
  a defined behavior for "bench table has no entry for this free model" (a neutral
  default rank, not a crash or a zero-score death spiral — see §3).

## 3. Edge cases and failure modes to design for

1. **Free model disappears from OpenRouter's catalog mid-session.** The model-list
   cache (TTL-based per requirements.md) means a candidate the strategy selects can
   be stale by the time the request reaches OpenRouter. OpenRouter's response for
   an unknown/removed model id is the concrete signal Phase 3 needs to define (see
   §1's `Router::dispatch` gap) — it should both fail this attempt without a paid
   fallback (candidates never include a paid upstream — see §1) and ideally
   invalidate the model-list cache early so the *next* request doesn't retry the
   same dead id from a fresh `already_tried`-reset dispatch call.
2. **A model in the static bench-ranking table no longer exists on OpenRouter, or
   vice versa (a live free model has no bench entry).** Both directions need a
   defined default: a currently-free model absent from the bench table should get
   a neutral/median rank (not zero, which would starve it of traffic forever) or a
   configurable "unranked" fallback; a bench-table entry for a model no longer free
   should simply be inert (never matched against a live candidate) — no error, no
   special handling, since the join is candidate-driven, not table-driven.
3. **Cold start: zero latency/error samples for a model.** `DurationHistogram::percentiles()`
   already returns `(0, 0, 0)` on an empty window, and a from-scratch per-model
   error-rate counter would similarly be `0/0`. A composite score built naively on
   "0ms latency, 0% error rate" would make an untested model look *artificially
   best* (fastest, safest) — likely a bug attractor. The formula needs an explicit
   cold-start policy: either exclude models below a minimum sample count from the
   latency/error terms (falling back to bench-rank alone until warmed up), or seed
   new models with a neutral/pessimistic prior. This is worth flagging explicitly
   for Phase 3's scoring-formula design since it's exactly the kind of bug that
   won't show up until the pool has been running a while.
4. **Streaming vs. non-streaming needing the same scoring.** `Router::dispatch`
   currently measures `duration_ms` as wall-clock from `attempt_started` to
   `provider.send()` returning (line 320) — for a stream, that's roughly
   time-to-first-byte/headers, not full-response time (the doc comment at line
   321-324 says as much: "First-byte time isn't separately measured here"). Both
   code paths already funnel through the same `record_attempt`/histogram call, so
   per-model latency recording is already stream-agnostic at the instrumentation
   layer — no special-casing needed for this feature beyond generalizing
   `DurationHistogram` itself to be per-model.
5. **Account-wide (not per-model) rate limiting.** Per Phase 2 web research
   (OpenRouter's published free-tier limits, current as of Sept 2026): 20
   requests/minute always, plus 50/day unfunded or 1,000/day once $10+ has ever
   been spent — and these limits read as **account-wide across the whole free-model
   pool**, not per individual model. If that's confirmed, cooling down model A on a
   429 and immediately trying model B is likely to also 429, since both draw from
   the same account-level bucket — a structurally different failure mode than the
   existing per-upstream 429 handling (which assumes each upstream/index has an
   independent rate limit). This may argue for either (a) a shared cooldown across
   the whole OpenRouter pool on a 429 rather than per-model, or (b) leaning on the
   existing `AdmissionControl`/local rate limiter (ADR-004) ahead of ever hitting
   OpenRouter, rather than relying on reactive per-model cooldown alone. Flagged as
   a concrete Phase 2→3 handoff item since requirements.md's own Feasibility Risks
   list this as unconfirmed.
6. **Free-model catalog churn frequency vs. cache TTL.** Search results (buldrr.com,
   accessed Sept 2026) describe the free lineup as rotating over months (DeepSeek/
   Mistral's free variants disappeared, newer providers' free models appeared) —
   this is consistent with (not proof of) a multi-hour TTL being safe, but the
   *early-invalidation-on-error* path (§1, §3.1) matters more than TTL tuning for
   catching an intra-day removal.
7. **All free models below a minimum health/score threshold vs. all literally
   cooled down.** The requirement's exhaustion behavior (`ProviderError::Exhausted`)
   is naturally satisfied when `HealthRegistry.is_available` returns false for
   every candidate (empty `healthy` slice, `strategy.select` returns `None`,
   `dispatch` breaks to the `Err` at line 368) — no new logic needed here beyond
   correctly wiring per-model health keys (§1). Worth confirming in Phase 3 that
   the composite-score strategy never needs to *itself* decide "everything scored
   too low, refuse" — availability filtering, not scoring, is what should produce
   exhaustion, keeping `select`'s pure/health-blind contract intact.
8. **`/v1/models` fetch itself failing (network error, auth error, malformed
   body) at cache-refresh time**, independent of any inference request. Needs a
   defined behavior: serve the last-known-good cached list past its TTL (soft
   fail) versus a hard failure that surfaces as `ProviderError::Exhausted`
   immediately. Nothing in the existing codebase caches a fetched list with a
   refresh failure mode to copy from — this is genuinely new ground for Phase 3.

## 4. Tyler's likely unstated needs

- **Visibility into *why* a model was chosen.** The Observability Requirements
  section already asks for per-candidate score components exposed via
  `to_metrics_json`, but the natural companion — surfaced per *request*, not just
  as a snapshot of current scores — is worth calling out: `RequestDetail`
  (mentioned at `router.rs:271`, the existing per-request logging path) could carry
  which candidate was selected and its score components at selection time, so a
  request that got routed to a weak/high-latency model is diagnosable after the
  fact from `GET /requests/{id}`, not just from the live dashboard snapshot. This
  extends existing infrastructure (`RequestDetail`) rather than adding new
  surface area, in keeping with the Out-of-Scope note against a UI redesign.
- **Forcing/pinning a specific free model for testing** — already solved by
  `SessionOverrideStore` (§1): Tyler can already pin a session to
  `{upstream: "openrouter", model: "some/model:free"}` today, no new mechanism
  needed. Worth confirming in Phase 3 that this path is explicitly tested against
  the new strategy (pin → single-candidate slice → strategy trivially returns it),
  since it's an existing feature this project must not regress, not a new one to
  build.
- **A way to see the current effective free-model pool and its cache state**
  (which models are in it, when it was last refreshed, why it was last
  invalidated) independent of triggering a live request — already captured in
  requirements.md's Observability Requirements, but the CLI/dashboard-affordance
  question ("is there a `GET` endpoint or CLI subcommand for this, or only the
  dashboard JSON blob") is unresolved and worth a concrete answer in Phase 3 given
  Tyler's local-proxy, single-user workflow (he's likely to `curl`/inspect this
  ad hoc more often than open a dashboard tab).
- **A manual override/refresh path for the bench-ranking table** is explicitly
  in scope per requirements.md ("a documented path for Tyler to override or
  refresh it"), but the *mechanism* is deferred to Phase 3. Given this is a
  config-driven codebase throughout (`SecretRef`, `deny_unknown_fields`,
  `conf.d/*.toml` layering), the path of least surprise is a checked-in default
  table plus an optional `conf.d` override table merged the same way other
  layered config already works — not a new subsystem.
- **Confidence that a rarely-tried model doesn't get stuck at the bottom forever.**
  Related to the cold-start edge case (§3.3): if the composite score purely
  reflects rolling latency/error-rate, a model that's simply never been selected
  recently (rolling window empty) either looks best (dangerous, §3.3) or worst
  (if a pessimistic default is chosen) — the latter risks a model that's actually
  fine getting permanently starved because it never gets picked to prove itself,
  unless the formula deliberately reserves some sampling/exploration budget for
  low-sample models. This is very close to a classic explore/exploit tension and
  worth naming explicitly for Phase 3 even if the fix is simple (e.g. a small
  epsilon-random slot, or excluding low-sample models from the *penalty* terms
  rather than the selection entirely).

## Sources
- Codebase: `src/routing/strategy.rs`, `src/routing/health.rs`, `src/routing/router.rs`,
  `src/routing/session_overrides.rs`, `src/metrics/histogram.rs`,
  `src/metrics/error_tracker.rs`, `src/metrics/mod.rs`, `src/providers/openai.rs`,
  `src/config/schema.rs`, `project_plans/consolette/decisions/ADR-003-routing-strategy-trait.md`.
- [Router - Load Balancing | liteLLM](https://docs.litellm.ai/docs/routing)
- [Routing Strategies | BerriAI/litellm | DeepWiki](https://deepwiki.com/BerriAI/litellm/2.3.1-routing-strategies)
- [OpenRouter Provider Routing / Provider Selection](https://openrouter.ai/docs/guides/routing/provider-selection)
- [How OpenRouter Model Routing Works — OpenRouter Blog](https://openrouter.ai/blog/insights/model-routing/)
- [How to Evaluate LLM Provider Performance — OpenRouter Blog](https://openrouter.ai/blog/insights/evaluate-llm-provider-performance/)
- [Load Balancing - Portkey Docs](https://portkey.ai/docs/product/ai-gateway/load-balancing)
- [Conditional Routing | Portkey Docs](https://docs1.portkey.ai/docs/product/ai-gateway/conditional-routing)
- [OpenRouter Free Tier 2026: Rate Limits, Models, BYOK - Dmytro Klymentiev](https://klymentiev.com/blog/openrouter-free-tier)
- [OpenRouter Free Models List 2026: All 27+ Models Ranked & Tested](https://buldrr.com/openrouter-free-models-list-2026-all-27-models-ranked-tested/)
- [OpenRouter Free API & Models 2026: Limits, Keys & Tips](https://buldrr.com/openrouter-free-api-keys-free-models-simple-guide/)

Note: free-tier rate-limit and catalog-churn figures above come from third-party
aggregator sites (accessed Sept 2026), not OpenRouter's own primary docs directly
fetched in this pass — flagged UNVERIFIED against the primary source; Phase 2's
dedicated OpenRouter-specifics research task should confirm against
`openrouter.ai/docs` directly before this becomes a load-bearing design input
(cooldown duration, shared-vs-per-model rate limiting).
