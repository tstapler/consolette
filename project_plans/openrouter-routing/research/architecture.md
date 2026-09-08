# Research: Architecture — openrouter-routing

Scope: (a) how `OpenrouterProvider` fits the existing `Provider`/`UpstreamKind`
factory pattern, and (b) how a new per-candidate-scoring `RoutingStrategy`
threads rolling latency/error data and a static bench-rank into the existing
health-blind, synchronous `select()` seam. Read in full or by targeted range
at commit `546188c` (current `HEAD`): `src/routing/strategy.rs`,
`src/routing/health.rs` (146 lines, whole file), `src/routing/router.rs`
(`build_providers`, `from_config`, `dispatch`), `src/providers/mod.rs:1-145`,
`src/providers/gemini/mod.rs` (module layout only), `src/metrics/counters.rs`,
`src/metrics/histogram.rs`, `src/metrics/error_tracker.rs`,
`src/config/schema.rs:95-166`. Builds on ADR-003
(`project_plans/consolette/decisions/ADR-003-routing-strategy-trait.md`) and
the gemini-provider architecture research
(`project_plans/gemini-provider/research/architecture.md`), cited by section
below rather than re-derived.

## 1. `OpenrouterProvider` — this is the easy, well-trodden half

The gemini-provider precedent (architecture.md §2, §6) already establishes
the full mechanical checklist for adding a provider, and it transfers
directly:

- **`src/config/schema.rs:107-123`** — `UpstreamKind` is an exhaustive
  `#[serde(tag = "kind")]` enum with `deny_unknown_fields`. Add
  `Openrouter { .. }` as a new struct variant next to `Gemini { project_id:
  String }`. Since OpenRouter is OpenAI-Chat-Completions-compatible (per
  requirements' Alternatives Considered — it was previously reachable as a
  hand-configured `Openai { base_url }`), the variant plausibly needs no
  required field beyond what `SecretRef`/`AuthMethod` already carries — a
  `base_url` default (`https://openrouter.ai/api/v1`) can be a
  `#[serde(default = "...")]` field if Tyler ever wants to point at a proxy,
  otherwise it can be hardcoded in `OpenrouterProvider` the way
  `AnthropicProvider`'s endpoint is (gemini architecture.md §2, citing
  `router.rs`'s `Anthropic` arm needing no `base_url`).
- **Two exhaustive-match call sites** the compiler forces the moment the
  variant exists (gemini architecture.md §2's exact mechanism repeats
  verbatim):
  - `src/routing/router.rs:64-87` (`build_providers`) — add an
    `UpstreamKind::Openrouter { .. } => Arc::new(OpenrouterProvider::new(..))`
    arm, same shape as the `Gemini` arm at router.rs:81-86 (takes
    `Arc::new(upstream.clone())`, `resolver`, `exec_cache`,
    `config.request_timeout`).
  - `src/entrypoint/mod.rs`'s `upstream_kind_label()` — add `"openrouter"`.
    (Not re-read this pass; gemini architecture.md §2 already confirmed this
    is the *only* dashboard-facing kind-name lookup as of the
    `41996c9` generalization commit.)
- **`src/routing/router.rs:144-157`** (`from_config`'s Bedrock
  `can_cooldown = false` list) — Openrouter must **not** be added here, same
  reasoning as Gemini (router.rs:153's comment): it's a real network
  upstream where whole-upstream cooldown is the right response to a genuine
  outage (auth failure, DNS/connect failure). This is orthogonal to the
  new *per-model* soft/hard avoidance mechanism in §3 below — see that
  section for why whole-upstream `HealthRegistry` cooldown and per-model
  scoring solve different problems and both are needed.
- **Module layout precedent**: `src/providers/gemini/` (`mod.rs`, `error.rs`,
  `stream.rs`, `tools.rs`, `translate.rs`, all private `mod` with selective
  `pub(crate) use` re-exports, e.g. `pub(crate) use error::DRIFT_COOLDOWN_SECS`
  at `gemini/mod.rs:35`) is the precedent for `src/providers/openrouter/`
  once it needs more than one file — plausibly `mod.rs` (the `Provider` impl
  + `build_headers`), `models.rs` (the free-model-list cache from §2), and
  reuse of `openai.rs`'s existing request/response translation instead of a
  new `translate.rs`, since OpenRouter's wire format is OpenAI-compatible
  (requirements' Out-of-Scope: "no new translation capability required").
  Concretely this suggests `OpenrouterProvider` can **compose** or
  **delegate to** `OpenaiProvider`'s request-building/response-parsing
  functions rather than reimplementing them — an open question for
  `sdd:3-plan` (wrap `OpenaiProvider` internally vs. hand-copy its
  translation helpers vs. extract them into a shared free function first).
- **Headers/auth**: same pattern as gemini architecture.md §5 —
  `OpenrouterProvider::build_headers` sets `Content-Type` and OpenRouter's
  recommended `HTTP-Referer`/`X-Title` attribution headers (per OpenRouter's
  docs; unresearched here — Phase 2's protocol research owns confirming
  these), then calls the shared `apply_auth_headers` (`anthropic.rs:390`)
  for `Bearer`/`SecretRef`-based auth. **No auth-path changes** — OpenRouter
  auth is a plain bearer API key, simpler than Gemini's `Exec`-wrapper case.
- **`list_models`**: the `Provider` trait already requires this method
  (`src/providers/mod.rs:16`, returns `Vec<ModelInfo>`). `OpenrouterProvider`
  implements it for real (hits `GET /models`, filters `pricing.prompt ==
  "0" && pricing.completion == "0"`) rather than stubbing it the way
  gemini-provider's first milestone did, since free-model discovery *is*
  this feature's core value — but note `list_models()` is a **cold,
  on-demand** trait method (called by e.g. `consolette list-models`), not
  the hot-path structure the new strategy reads per-request. See §2 for why
  the cache needs its own synchronous read path independent of this method.

**Conclusion for §1**: nothing here is architecturally novel relative to the
Gemini precedent; it is direct, mechanical reuse of an already-proven
pattern. The genuinely new ground is §2 and §3.

## 2. Where the free-model-list cache lives, and why it can't be "inside `list_models()`"

`Router` only ever holds `providers: Vec<Arc<dyn Provider>>` and calls
`provider.send(..)` — it has no reason to downcast to a concrete type, and
`Provider`'s only model-listing hook, `list_models()`, is `async` (mod.rs:16)
and meant for cold/explicit calls (CLI `list-models`), not something the
hot dispatch loop or a synchronous `RoutingStrategy::select()` can call.

**The cache must be readable synchronously from inside `select()`**, because
`RoutingStrategy::select(&self, healthy: &[UpstreamRef]) -> Option<UpstreamRef>`
(`strategy.rs:26`) is deliberately non-async and takes no `&Provider`/cache
argument — ADR-003's whole design point is that `select` stays pure and
health-blind so `FallbackStrategy`/`WeightedStrategy` don't need touching.
Two structurally different places the cache's *state* could live, versus
where it's *populated*:

- **Population** (network I/O, TTL refresh, invalidation) naturally lives
  **inside `OpenrouterProvider`** — it already owns the `reqwest::Client`,
  base URL, and auth headers needed to call `GET /models`, so a background
  refresh task or lazy-check-on-call belongs there, not duplicated at router
  level.
- **The synchronously-readable snapshot** the strategy consults, however,
  needs to be reachable from `RoutingStrategy::select()`'s call site — i.e.
  from `Router::dispatch`, not from inside the provider object at all, since
  `select()` never receives a `Provider` reference. This argues for the
  cache being **owned by the new `OpenrouterScoringStrategy` struct itself**
  (constructed with an `Arc` to a shared cache, injected at `Router::from_config`
  build time, mirroring how `strategy.rs`'s existing strategies are
  zero-field unit structs but nothing prevents a new one from holding
  `Arc<..>` fields — `RoutingStrategy` only requires `Send + Sync`, not
  `Default`/zero-sized).
- Concretely: an `Arc<ModelListCache>` (e.g. `ArcSwap<Vec<FreeModelEntry>>`
  or `RwLock<CachedModelList>`, matching the "never hold a guard across
  `.await`" discipline from `health.rs`'s doc comment) is constructed once
  in `OpenrouterProvider::new(..)` **and also handed to** the strategy
  constructor in `Router::from_config` — the same `Arc` shared between the
  provider (which refreshes it, the only place doing the actual `GET
  /models` HTTP call and TTL/invalidation bookkeeping) and the strategy
  (which only ever reads it synchronously in `select()`). This avoids
  inventing a second, router-level cache and avoids the strategy needing
  any way to reach the provider object.
- **Refresh trigger**: a background `tokio::spawn`'d task inside
  `OpenrouterProvider::new` (checked against the multi-hour TTL) is simplest
  and keeps `select()` genuinely non-blocking; a lazy "refresh if stale"
  check triggered from inside `send()` (the async, already-`.await`-capable
  path) is the alternative if a background task is considered too much
  standing infrastructure for a single-user proxy. Either way, **refresh
  itself must not happen inside `select()`** — that would violate ADR-003's
  synchronous-and-pure contract and reintroduce the network-call-per-request
  problem the requirements explicitly want the cache to solve ("don't hammer
  OpenRouter's `/models` endpoint on every request").
- **Early invalidation** ("an error that suggests staleness," requirements'
  Rabbit Holes) is naturally driven from `Router::dispatch`'s existing error
  match arms (router.rs:335-365) — e.g. if `OpenrouterProvider::send` maps a
  "model not found" response to a specific, distinguishable `ProviderError`
  variant/kind, `Router::dispatch`'s catch-all arm (or a new arm) can call
  something like `cache.invalidate()` before the retry loop re-selects. This
  mirrors the *shape* of the existing Gemini "schema drift trips cooldown"
  special-case (router.rs:345-360, gemini architecture.md §3) but targets
  the model-list cache instead of `HealthRegistry` — same
  "provider-specific reaction living in the shared dispatch loop" pattern,
  same tension the Gemini research flagged (shared code touched for one
  provider's quirk). Recommend, as that research did for its analogous gap,
  flagging this explicitly for `sdd:3-plan` rather than deciding the exact
  `ProviderError` plumbing here.

**Where it is *not***: not a field on `Router` itself (`Router` is
kind-agnostic per ADR-003 and the "no bespoke dispatch path" constraint —
adding an `Option<Arc<ModelListCache>>` field to the generic `Router` struct
for one upstream kind would violate that), and not recomputed by calling the
`Provider` trait's `list_models()` per request (that method is async, cold,
and does a real network round-trip unless it *also* consults the same
cache — in fact `list_models()` and the strategy's cache read should
converge on the **same** underlying `ArcSwap`, so `consolette list-models`
and live routing never disagree about what's currently "free").

## 3. Per-candidate latency/error tracking — the real structural gap

### 3.1 What already exists, and why none of it is "per-candidate" in the needed sense

Three existing pieces of metrics/health infrastructure look superficially
reusable but each has a mismatch:

| Existing structure | Keying | Why it doesn't fit as-is |
|---|---|---|
| `HealthRegistry` (`src/routing/health.rs`, `DashMap<usize, ProviderState>`) | upstream **index** (position in `Config.upstreams`) | One `usize` per configured upstream *slot*, not per model. All of OpenRouter's free models share **one** upstream slot/index — `HealthRegistry` has no notion of a candidate finer than "the whole openrouter upstream." |
| `UpstreamCounters` (`src/metrics/counters.rs:16-31`, `DashMap<String, UpstreamCounters>` at `counters.rs:46`) | upstream **name** (`String`) | Same granularity problem as above (one name = one upstream), *and* it's all-time cumulative (`AtomicU64` sums, `counters.rs:277-292`'s `to_json` divides sum/count for an all-time average) — not a rolling window, so it can't answer "how has this model performed **recently**." |
| `DurationHistogram` (`src/metrics/histogram.rs`) | none — **one global instance** (`MetricsCollector.histogram: Arc<DurationHistogram>`, `metrics/mod.rs:144`) | Rolling (15-minute `VecDeque` window, `histogram.rs:1-50`) is exactly the right *mechanism*, but there is only one process-wide instance; it isn't keyed by upstream or model at all today. |

So the requirements' framing ("does `DurationHistogram`/`ErrorTracker` need
per-index generalization, similar to how `HealthRegistry` is already keyed
by upstream index") is half right and half a trap: `HealthRegistry`'s
per-**index** keying is exactly the granularity the new strategy does *not*
want, because index-granularity is "per configured upstream," and this
feature's whole point is sub-selecting among many models living behind a
*single* upstream index. Generalizing `DurationHistogram`/`ErrorTracker` to
be index-keyed like `HealthRegistry` would reproduce the same mismatch one
level down.

### 3.2 Recommended shape: a new, strategy-owned tracker keyed by model id

Per the Rabbit Holes guidance ("scope this to exactly what the new strategy
needs, not a general per-upstream-metrics refactor"), the cleanest fit is a
**new, small, purpose-built structure** — not a generalization of
`HealthRegistry`, `UpstreamCounters`, or the global `DurationHistogram` —
living alongside (constructed by, owned by) the new
`OpenrouterScoringStrategy`:

- **Key**: model id (`String`) is sufficient — `UpstreamRef.index` is
  useless here (it's constant across all OpenRouter candidates), and there
  is exactly one OpenRouter upstream in practice, so no `(index, model)`
  composite is needed unless a future multi-OpenRouter-upstream config is
  anticipated (not a stated requirement — YAGNI).
- **Shape**: reuse `DurationHistogram`'s existing rolling-window mechanism
  verbatim, but hold `DashMap<String, Arc<DurationHistogram>>` (one rolling
  histogram per model id, lazily created via `.entry(id).or_insert_with(..)`
  — the same `DashMap::entry` idiom `UpstreamCounters` already uses at
  `counters.rs:148`) instead of generalizing `DurationHistogram` itself.
  `DurationHistogram` needs **zero code changes** — it's already a
  standalone, cheaply-instantiable struct with no global-singleton
  assumption baked into its own code (the "one instance" fact lives in
  `MetricsCollector`'s field declaration, not in `histogram.rs`).
- **Error rate**: a rolling error-rate needs the same per-model-keyed
  treatment. `ErrorTracker` (`error_tracker.rs`) is shaped for a different
  job (fingerprinted dedup for the dashboard's error-summary view, capped
  ring buffer, not a rate) and reusing it would conflate two purposes.
  Simplest fit: reuse the *same* per-model `DashMap` entry to also record a
  small rolling window of `(Instant, bool)` outcomes (or two rolling
  counters with time-decay) — this can literally be a second small
  `RollingErrorRate` type with the same "trim on read/write" shape as
  `DurationHistogram`, or, if the two signals are always read together, one
  combined `ModelStats { latency: DurationHistogram, outcomes:
  VecDeque<(Instant, bool)> }` struct behind one `Mutex`/`DashMap` entry so
  a `select()` call touches one lock per candidate instead of two.
- **Write path**: `Router::dispatch` (router.rs:316-365) is exactly where
  every attempt's `attempt_started`/outcome is already known — recording
  into the new per-model tracker is a call alongside the existing
  `self.record_attempt(&chosen.name, ..)` (router.rs:318 etc.), gated on
  `chosen.model.is_some()` (or, more precisely, on the strategy having
  vended this candidate) so the other three providers' dispatch path is
  untouched. Concretely this means **either**:
  1. `Router::dispatch` calls a new `RoutingStrategy` method
     (`fn record_outcome(&self, candidate: &UpstreamRef, duration_ms: u64,
     success: bool)`, default no-op) after every attempt, so the write path
     is generic across strategies and `Router` stays strategy-agnostic
     (preferred — keeps the "one dispatch loop, pluggable strategy"
     ADR-003 shape intact, and costs `FallbackStrategy`/`WeightedStrategy`
     nothing since they'd just use the default no-op); or
  2. `Router::dispatch` special-cases "is this the openrouter scoring
     strategy" — rejected, this is exactly the "bespoke dispatch path" the
     requirements rule out.
  Option 1 is a **strict superset extension** of `RoutingStrategy` (default
  method, `Self: Sized` not required), so `strategy.rs`'s existing trait
  and both existing impls compile unchanged — additive, not a breaking
  signature change to `select()` itself, honoring the "keep the requested
  `select(&[UpstreamRef])` signature pure" alternative-rejected-because
  row in ADR-003's table (that row rejected *adding args to `select`*, not
  adding an unrelated second method to the trait).
- **Read path**: `select()` reads the shared `DashMap` synchronously —
  no `.await` anywhere in the read, satisfying `health.rs`'s "never hold a
  `DashMap` guard across `.await`" hard rule by construction (same
  reasoning `HealthRegistry` already relies on). Each candidate's guard is
  acquired, its current `(p50 latency, error rate)` computed, and released
  before moving to the next candidate — no guard is held across the
  scoring loop's iteration boundary, let alone across an `.await`.

### 3.3 Per-model hard exclusion (429s) is a *separate* concern from soft scoring

The requirements need both:

1. **Soft steering** — a model with worse rolling latency/error-rate scores
   lower and is picked less often (§3.2 above covers this; no on/off gate
   needed, the composite score itself does the work).
2. **Hard exclusion** — "When every free model in the pool is unavailable
   (cooling down / rate-limited), the request fails clearly
   (`ProviderError::Exhausted`)" implies a specific model that gets a real
   429 needs to become **unselectable** for some duration, not just
   score-penalized, mirroring what `HealthRegistry` already does at
   whole-upstream granularity.

`HealthRegistry`'s existing `trip`/`is_available` (`health.rs:58-104`) is
**index-keyed** and is applied by `Router::dispatch` as a pre-selection
filter (router.rs:284) *before* `strategy.select` ever runs — the same call
that gates Anthropic/Bedrock/OpenAI/Gemini today. If OpenRouter's per-model
429 also tripped `self.health.trip(chosen.index, ..)` (as
router.rs:339-343's existing rate-limit arm does today for every other
provider), it would cool down the **entire openrouter upstream** — every
free model — for one model's rate limit, directly undermining the "spread
load across free models" goal. This is a genuine architectural fork point
for `sdd:3-plan` to resolve explicitly, with (at least) two viable shapes:

- **(a) A second, sibling registry** — a small new struct with the *same*
  `DashMap`-based trip/is_available mechanism as `HealthRegistry`
  (health.rs's code is short and generic enough to copy the pattern
  cheaply) but keyed by model id instead of upstream index, owned by the
  strategy (or injected into it) rather than by `Router`. `Router::dispatch`
  would need a small, additive change: after choosing `chosen.model`, check
  this second registry (if the candidate came from a strategy that has one)
  before calling `provider.send`, and trip it on a 429 instead of (or in
  addition to) the whole-upstream one. This keeps `HealthRegistry` itself
  untouched (zero risk to the three tested existing routes) at the cost of
  one more `Router::dispatch` branch that is conditionally relevant only to
  candidates carrying a model id.
- **(b) Fold hard exclusion into the score** — instead of a real
  cooldown gate, a model that just 429'd gets its rolling error-rate driven
  to (near) 1.0 for a window, which the composite score naturally treats as
  "never pick this," approximating a cooldown without a second registry.
  Simpler (no new type), but doesn't give a clean, inspectable
  "`retry_after`-respecting" cooldown the way `HealthRegistry::trip` does
  with `override_duration` from a parsed `Retry-After` header — a 429's
  `Retry-After` would just be discarded rather than driving how long the
  model is truly excluded. Given the requirements call out `Retry-After`v
  handling only implicitly ("rate-limited" cooldown, no explicit mention of
  honoring OpenRouter's `Retry-After` for free models), this may be
  acceptable for v1, but it's a real behavioral difference from every other
  provider's rate-limit handling and should be a stated tradeoff, not an
  accident.

Recommend flagging this fork explicitly for `sdd:3-plan` (mirroring how the
Gemini research flagged its own cooldown-gap fork in architecture.md §3
rather than silently picking one path) — it's the single highest-leverage
open design decision in this feature's routing layer.

### 3.4 The `already_tried` / candidate-expansion interaction (a second, related gap)

`Router::dispatch`'s retry loop (router.rs:258, 281-366) tracks
`already_tried: HashSet<usize>` and filters `healthy` by
`!already_tried.contains(&u.index)` (router.rs:284) — **keyed by upstream
index**, on the existing assumption that "one candidate = one upstream
index, tried at most once per dispatch." If OpenRouter's many free models
are represented as multiple `UpstreamRef`s that all share the **same**
`index` (differing only in `.model`) — which they must, since `index` is
also how `Router` looks up `self.providers[chosen.index]`
(router.rs:306) and there is exactly one `OpenrouterProvider` instance —
then the *first* failed attempt against any OpenRouter model inserts that
shared `index` into `already_tried`, and the very next loop iteration's
`healthy` filter drops **every** OpenRouter candidate at once, regardless
of model. This silently defeats "steer away from a slow/erroring model and
try another free model in the same request" — the router would report
`Exhausted` after exactly one failed model attempt, never trying a second
free model in the same dispatch.

This is a **router-level** change needed regardless of which per-model
health design (§3.3a/b) is chosen: `already_tried` needs to become model-
aware, e.g. `HashSet<(usize, Option<String>)>`, with the filter at
router.rs:284 comparing the pair instead of the bare index. This is a small,
additive, and — per a quick read of router.rs's existing tests — probably
low-risk change: `FallbackStrategy`/`WeightedStrategy` candidates all carry
`model: None` (or a fixed per-route pin that never changes across a single
dispatch's retries), so `(idx, None)` is a strict refinement of `idx` for
every existing test case and behavior is unchanged for the three other
providers. This should be scoped and called out explicitly in `sdd:3-plan`
as a `Router::dispatch` change, not left implicit inside the new strategy's
design — it's shared code, but it's a narrow, mechanical, low-risk widening
of a key type, not the kind of general refactor the Rabbit Holes section
warns against.

### 3.5 Where the discovered-model candidate list itself gets built

Tying §2 and §3 together: `Router.candidates: Vec<UpstreamRef>` is built
**once**, statically, in `from_config` (router.rs:170-189) from
`route.upstreams` — one static entry per configured route-upstream line.
For OpenRouter, the *route* config still declares one static line (`{name =
"openrouter", ...}`, no per-model config needed — this is the entire point
of auto-discovery), but at **dispatch time** that one static `UpstreamRef`
needs to expand into N model-carrying candidates before
`strategy.select()` sees them. Given `effective_candidates` already exists
as the per-dispatch candidate-list hook (router.rs:219-237, currently used
for session pins), the natural extension point is: after
`effective_candidates` resolves the (session-pin-aware) static list, a
second, generic expansion step turns each `UpstreamRef` into one-or-more
`UpstreamRef`s by consulting a per-strategy (not per-provider — see §2's
reasoning that the strategy owns the cache handle) hook. Concretely, this
argues for a second default-no-op `RoutingStrategy` method alongside
§3.2's `record_outcome`, e.g. `fn expand_candidates(&self, candidates:
Vec<UpstreamRef>) -> Vec<UpstreamRef> { candidates }` — default identity for
`FallbackStrategy`/`WeightedStrategy`, overridden by
`OpenrouterScoringStrategy` to read its `Arc<ModelListCache>` and fan one
static `openrouter` entry out into one entry per cached free model (each
carrying `model: Some(id)`), which then flows through the existing
health-filter (§3.3) → `strategy.select` (§3.2's scoring) → dispatch →
`record_outcome` pipeline unchanged. This keeps `Router::dispatch`'s
control flow identical in shape to today (filter → select → admit → send →
classify), with two new, additive, default-no-op trait methods as the only
`RoutingStrategy` surface change — no change to `select()`'s existing
signature or its health-blind/pure contract.

## 4. Data-flow summary (per dispatch, for the OpenRouter route)

1. `effective_candidates` resolves session pins (unchanged, router.rs:219).
2. **New**: `strategy.expand_candidates(..)` fans the one static
   `openrouter` `UpstreamRef` out to N per-model `UpstreamRef`s, reading the
   `Arc<ModelListCache>` synchronously (§2, §3.5) — no `.await`, no network
   call, just a snapshot read.
3. `healthy` filter (router.rs:282-286) applies **both**
   `!already_tried.contains(&(idx, model))` (widened per §3.4) and
   `self.health.is_available(idx)` (whole-upstream gate, unchanged) — and,
   per §3.3a if that path is chosen, a second per-model availability check.
4. `strategy.select(&healthy)` (§3.2) reads each candidate's rolling
   latency/error-rate from the per-model `DashMap` plus the static
   bench-rank table, computes the composite score, returns the top (or
   sampled) candidate — synchronous, pure, no shared-mutable-state
   *mutation*, only reads (satisfies ADR-003's "health-blind... no shared
   mutable state access shown in the trait signature" as long as the reads
   stay reads).
5. `provider.send(..)` dispatches (unchanged, §1 — `OpenrouterProvider` is a
   normal `Provider` impl).
6. On return, `Router::dispatch` calls the existing error-class match arms
   (unchanged) **plus**, per §3.2, `strategy.record_outcome(..)` — this is
   the only *write* into per-model rolling state, and it happens after the
   `.await` on `provider.send` has already completed, so no guard is ever
   held across an `.await` (satisfies `health.rs`'s hard rule by the same
   construction it already relies on).
7. Exhaustion: once `already_tried` (now model-aware) covers every
   discovered free model, `healthy` is empty, `strategy.select` returns
   `None`, the loop breaks, and `Err(last_error.unwrap_or(Exhausted))`
   fires exactly as it does today (router.rs:368) — **no code change
   needed** for the "fail clearly, don't fall back to paid" requirement, as
   long as the config-level guarantee holds that the OpenRouter route's
   `upstreams` list contains only OpenRouter-kind entries (a config
   convention, not something the router enforces or needs to enforce here).

## 5. Summary of open architectural questions carried into `sdd:3-plan`

- **§3.3**: per-model hard exclusion on a 429 — a sibling `DashMap`-based
  registry keyed by model id (mirrors `HealthRegistry`'s shape but is a new,
  separate type) vs. folding hard exclusion into the composite score via a
  saturating error-rate. Needs an explicit decision; both are viable, they
  trade off `Retry-After` fidelity against added surface area.
- **§3.4**: `Router::dispatch`'s `already_tried: HashSet<usize>` needs
  widening to `HashSet<(usize, Option<String>)>` — small, additive, and
  appears behavior-preserving for the three existing strategies/four
  existing providers, but is shared code and should be reviewed as its own
  step, not folded silently into the new strategy's implementation.
- **`RoutingStrategy` trait gains two default-no-op methods**
  (`record_outcome`, `expand_candidates`) — additive per ADR-003's own
  precedent for keeping `select()`'s signature untouched; confirm at plan
  time that this doesn't fight `dyn RoutingStrategy`'s object-safety
  (`Self: Sized` must not appear on either).
- **§2**: exact cache data structure (`ArcSwap` vs `RwLock<Vec<..>>`) and
  refresh trigger (background `tokio::spawn` vs. lazy-check-in-`send`) — a
  Phase 3 implementation-detail choice, not an architecture fork.
- **§1**: whether `OpenrouterProvider` delegates to/wraps `OpenaiProvider`'s
  existing request/response translation helpers, or hand-copies them —
  affects `src/providers/openai.rs`'s `pub(crate)`-ness of those helpers but
  not the overall shape.
