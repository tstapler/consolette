# Research: Technology Stack — openrouter-routing

Agent 1 (Stack), SDD Phase 2. Scope: which crates/versions/patterns this
feature needs, and whether anything not already in `Cargo.toml` is justified.

## Bottom line

No new dependency is needed. Everything the feature requires — HTTP client,
TTL cache, per-candidate rolling stats, static benchmark table — is already
in `Cargo.toml` and already has an established idiom elsewhere in this
codebase. The right move is to copy those idioms, not add a crate.

## HTTP client to OpenRouter's OpenAI-compatible API

**Use `reqwest` 0.12 directly, following `src/providers/openai.rs`'s existing
shape — do not add `openrouter_api`, `openrouter-rs`, or `openrouter-sdk`.**

- OpenRouter's `/api/v1/chat/completions` is OpenAI-compatible, and
  `OpenaiProvider` (`src/providers/openai.rs:1-102`) already implements
  exactly this shape against an arbitrary `base_url`: two `reqwest::Client`s
  (pooled for normal requests, `pool_max_idle_per_host(0)` for SSE per
  ADR-004), `eventsource_stream::Eventsource` for streaming, and
  `anthropic::apply_auth_headers` for the config-driven `SecretRef`/`AuthMethod`
  auth path. `OpenrouterProvider` should be structured the same way — likely
  thin enough to delegate most of `send()` to shared helpers rather than
  duplicate `OpenaiProvider` wholesale, but that's a Phase 3 design call, not
  a stack question.
- Three third-party OpenRouter Rust SDKs exist on crates.io
  (`openrouter_api` 0.7, `openrouter-rs`, `openrouter-sdk`), found via
  `cargo search openrouter`. None are worth adding:
  - They'd introduce a second HTTP-client abstraction alongside the
    hand-rolled `reqwest` pattern already used for every other provider
    (Anthropic, Bedrock, generic OpenAI, Gemini) — inconsistent with
    "no bespoke dispatch path" (requirements.md constraint) and with how
    this codebase treats provider SDKs generally: it doesn't use the AWS
    Rust SDK's higher-level Bedrock convenience wrappers either, it uses
    `aws-sdk-bedrockruntime`/`aws-sdk-bedrock` directly for the same reason.
  - This feature needs two things a generic SDK doesn't give for free: (a)
    OpenRouter's own headers (`HTTP-Referer`, `X-Title` — OpenRouter's
    documented attribution headers), trivially added as two more
    `HeaderMap` inserts in the existing `build_headers` pattern, and (b) a
    `/models` listing filtered to `pricing.prompt == "0"` /
    `pricing.completion == "0"`, which is just a `serde_json` parse of a
    documented, stable response shape — not something a wrapper crate saves
    meaningfully over.
  - None of the three have enough adoption/maintenance signal (no crate here
    has been vetted against this repo's supply-chain bar) to justify the
    dependency-count and audit cost for what's ~150 lines of code reusing
    patterns already in the tree.
- **Headers**: OpenRouter recommends (not strictly required) `HTTP-Referer`
  and `X-Title` on requests for attribution/leaderboard-ranking purposes.
  Add these as static or config-derived header inserts in
  `OpenrouterProvider::build_headers`, mirroring `OpenaiProvider`'s existing
  method.
- **Auth**: `Authorization: Bearer <OPENROUTER_API_KEY>` — already exactly
  what `AuthMethod::Bearer { token: SecretRef }` supports
  (`src/config/schema.rs:78-90`). No new auth variant needed.

## Free-model discovery (`/models` endpoint)

- `GET https://openrouter.ai/api/v1/models` returns all models with a
  `pricing` object per model; free models have `pricing.prompt == "0"` and
  `pricing.completion == "0"` (both string-typed cents-per-token fields in
  OpenRouter's schema) and/or a `:free` suffix on the model `id`
  (e.g. `deepseek/deepseek-chat-v3.1:free`). Filtering client-side on the
  pricing fields is more robust than string-matching the `:free` suffix,
  since OpenRouter's `id` convention isn't guaranteed to stay a suffix
  forever — Phase 3 should decide the exact filter predicate.
- No new dependency: parse the response with `serde_json::Value` the same
  way `providers/mod.rs::ModelInfo` and `list_models()` already do
  elsewhere (`src/providers/mod.rs:139-179`), or add a small
  `#[derive(Deserialize)]` struct scoped to the fields actually used (id,
  pricing.prompt, pricing.completion, owned_by) — both patterns already
  exist in this codebase for different upstreams; either is idiomatic, no
  crate needed either way.

## TTL-based model-list cache: mutex+Instant, not `moka`/`dashmap`

**Recommendation: a simple `Mutex<Option<(Instant, Vec<ModelInfo>)>>` (or
equivalent single-slot struct), matching `src/routing/health.rs` and
`src/metrics/histogram.rs`'s existing style — not `moka`.**

Reasoning:
- `moka::future::Cache` (already a dependency, used in
  `src/cost_metrics/store.rs`, `src/memory/store.rs`,
  `src/session_compaction/session_state.rs`, `src/compression/rewind.rs`) is
  this codebase's established idiom for **multi-key, per-entry-TTL** caches —
  e.g. `Cache::builder().max_capacity(1000).time_to_live(Duration::from_hours(1))`
  keyed by session/request id. That shape doesn't fit here: the OpenRouter
  model list is **one value** (the whole free-model list), refreshed on one
  shared TTL, with an *early* invalidation trigger driven by an application
  event (an upstream 400/404 that suggests a stale model id) rather than
  time alone. `moka` has no first-class "invalidate this one entry from
  outside the read path based on business logic" primitive that's simpler
  than just holding the value directly — you'd end up wrapping it in a
  single-key `Cache` for no benefit over a plain mutex.
- `dashmap` (already a dependency, used exactly this way for per-index state
  in `HealthRegistry`) is built for **keyed, sharded concurrent maps**. A
  single cached list has no key to shard on, so `DashMap<(), _>` would be an
  odd fit — a plain `Mutex`/`RwLock` around one value is simpler and is what
  `DurationHistogram` (`Mutex<VecDeque<...>>`) and `HealthRegistry`'s
  cooldown-`Instant` comparisons already do for exactly this "protect one
  small piece of shared state with sync primitives, no async cache
  semantics" shape.
- Concrete shape, following `health.rs`'s TOCTOU-safe pattern
  (`DashMap::get_mut`'s exclusive guard) adapted to a `Mutex`:
  ```rust
  struct ModelListCache {
      state: Mutex<CacheState>,
      ttl: Duration,
  }
  struct CacheState {
      models: Vec<ModelInfo>,
      fetched_at: Instant,
      last_invalidation_reason: Option<&'static str>,
  }
  ```
  `get_or_refresh()` takes the lock, checks `fetched_at.elapsed() < ttl`,
  and either returns the cached `Vec` or drops the lock, does the `reqwest`
  call, and re-locks to store the result — same "sync check, async work
  outside the lock" shape `ExecCredentialCache`
  (`src/auth/exec.rs`, `dashmap`-backed) already uses for exec-auth caching,
  which is the closest existing analog to "cache an expensive async fetch
  behind a TTL, keyed lookup optional."
  - **Never hold a `std::sync::Mutex` guard across `.await`** — same hard
    rule `health.rs`'s doc comment states for `DashMap`. Take the lock only
    for the synchronous read-or-decide-refresh check and the final
    write-back; the `reqwest` call itself happens with the lock released.
  - `tokio::sync::Mutex` is an alternative if the refresh path needs to hold
    a lock across an await (e.g. to serialize concurrent refreshes so two
    simultaneous cache-miss requests don't both hit `/models`); given this
    is a single-user local proxy (per requirements.md's stated scale), a
    double-fetch race on cold cache is a correctness non-issue, not a
    perf one — a plain `std::sync::Mutex` with the "lose the race, fetch
    twice, last write wins" behavior is simpler and adequate. Don't add
    `tokio::sync::Mutex` complexity unless Phase 3 decides the double-fetch
    is worth preventing.
- Early invalidation ("an error that suggests the model list is stale"):
  a plain method like `invalidate(reason: &'static str)` that zeroes
  `fetched_at` (or sets a `force_refresh` bool) under the same lock — no new
  crate machinery, just one more method on the same struct. Exposing
  `last_invalidation_reason` in `CacheState` directly satisfies the
  observability requirement ("last invalidation reason" in
  `to_metrics_json`) without inventing a separate event log.

## Per-candidate rolling latency/error-rate: extend the existing counters, don't invent new metrics infra

The requirements' "extending today's global-only latency/error tracking to
per-candidate granularity" already has a direct precedent in this codebase,
just not yet applied to per-*model* (as opposed to per-*upstream*) keys:

- `ProxyMetrics::upstreams: DashMap<String, UpstreamCounters>`
  (`src/metrics/counters.rs:44-46`) already keys per-upstream atomic
  counters (`requests`, `success`, `errors`, `duration_sum_ms`,
  `duration_count`, `last_error_kind`) by upstream name in a `DashMap`. The
  new scoring strategy needs the same shape keyed by **candidate model id**
  instead of (or nested under) upstream name — e.g.
  `DashMap<String, UpstreamCounters>` keyed by OpenRouter model id, or a
  `DashMap<String, ModelCandidateStats>` purpose-built for the three score
  inputs (rolling latency, rolling error rate, static bench rank) rather than
  reusing `UpstreamCounters` verbatim (it carries fields — `first_byte_*`,
  duration buckets — the strategy doesn't need).
- `DurationHistogram` (`src/metrics/histogram.rs`, `Mutex<VecDeque<(Instant,
  u64)>>`, 15-minute rolling window, `percentiles()`) is the existing
  *rolling-window* (not lifetime-average) latency primitive. For "rolling
  per-candidate latency," the natural move is one `DurationHistogram` per
  candidate model, held in a `DashMap<String, DurationHistogram>` (same
  per-key-sharded-map idiom as `HealthRegistry`/`ProxyMetrics::upstreams`) —
  reuse the existing type, don't reinvent a rolling window.
- Rolling **error rate** has no existing rolling-window primitive
  (`ErrorTracker` in `src/metrics/error_tracker.rs` is a fingerprint-dedup
  ring buffer for the dashboard's error list, a different purpose). The
  simplest option consistent with `DurationHistogram`'s existing shape is a
  parallel `Mutex<VecDeque<(Instant, bool)>>` (success/failure per request)
  with the same window-trim-on-record logic, exposing an
  `error_rate() -> f64` the same way `percentiles()` exposes latency
  quantiles — small, and directly mirrors code already in the tree rather
  than pulling in a stats crate.
- No new dependency required for any of this: `dashmap` 6 and
  `std::sync::Mutex`/`std::time::Instant` (already used identically in
  `health.rs` and `histogram.rs`) cover it fully.

## Static coding-benchmark table

- No crate needed. A static table (candidate model id → benchmark
  rank/score) is a plain Rust data structure, not a caching or HTTP concern.
  Two idiomatic options, either fitting this codebase's existing
  conventions:
  1. A `const`/`static` array of `(&'static str, f64)` compiled into the
     binary (simplest; matches the "static, checked-in default" requirement
     literally) — no `include_str!`/build-script precedent was found
     elsewhere in `src/` for this kind of thing, so a plain Rust literal
     table is the path of least novelty.
  2. A TOML section under the existing `figment`/`toml` config stack
     (already dependencies) if Tyler's override path (requirements.md:
     "documented path for Tyler to override or refresh it") should be
     editable without a rebuild — e.g. `[coding_benchmarks]` mapping model
     id substrings/patterns to a score, parsed with the same
     `deny_unknown_fields` `serde` conventions as the rest of
     `src/config/schema.rs`.
  - Which of these two Phase 3 picks is explicitly out of scope for this
    research doc (requirements.md Open Questions: "exact refresh mechanism
    ... is a Phase 3 design decision"). Either way, no new dependency is
    implicated — `serde`/`toml`/`figment` already cover option 2, and option
    1 needs nothing beyond the standard library.
- Benchmark source candidates (both public, static snapshots — not fetched
  live, per requirements' explicit out-of-scope on scraping): aider's
  polyglot leaderboard (aider.chat/docs/leaderboards) and LiveBench
  (livebench.ai) were the two named in requirements.md as examples. Picking
  the exact seed data and refresh cadence is a Phase 3/content decision, not
  a stack decision.

## OpenRouter free-tier rate limits (feasibility input, confirmed via web search)

Relevant to sizing the `HealthRegistry` cooldown duration and the
`ProviderError::Exhausted` behavior for the free-model pool:

- Free (`:free`-suffixed) models are capped at **20 requests/minute**
  account-wide (not obviously per-model — OpenRouter's docs describe it as
  applying to the free-model pool as a whole).
- Daily cap: **50 free-model requests/day** for accounts with less than
  $10 lifetime credit purchased; **1,000/day** once $10+ has been purchased
  at any point (permanent upgrade, not a subscription).
- Practical implication for Phase 3: the per-model cooldown on a 429 from a
  free model should probably be longer than the existing generic default,
  since burning through the 20/min or 50-or-1000/day cap quickly puts every
  free model in the pool into cooldown simultaneously, which is exactly the
  `ProviderError::Exhausted` case the requirements call out as
  correct-by-design (not a bug to route around) — but the exact cooldown
  number is a Phase 3 tuning call, not a stack question.
- Sources: OpenRouter's own rate-limit docs as summarized by
  https://ask-coreai.com/blog/openrouter-rate-limits-explained-how-to-avoid
  and https://klymentiev.com/blog/openrouter-free-tier (both accessed
  2026-09-05 via WebSearch; treat as a starting point to verify against
  OpenRouter's official docs directly before hard-coding a cooldown value in
  Phase 3, since third-party summaries of a vendor's rate limits can lag
  reality).

## Config schema additions (no new dependency)

- `UpstreamKind::Openrouter` as a new enum variant in
  `src/config/schema.rs:107-123`, sibling to `Openai { base_url: String }`
  — likely no fields needed beyond what `Openai` has (OpenRouter's base URL
  is fixed: `https://openrouter.ai/api/v1`), or possibly a
  `base_url: Option<String>` defaulting to that constant if Tyler ever wants
  to point at a self-hosted OpenRouter-compatible gateway — Phase 3 call.
- `Strategy` enum (`src/config/schema.rs:140-145`, currently `Fallback` |
  `Weighted`) needs a third variant (e.g. `Scored`) for the new
  `RoutingStrategy` impl, wired the same way `Route::strategy` already
  dispatches to `FallbackStrategy`/`WeightedStrategy`
  (`src/routing/strategy.rs`).
- All of this reuses `serde`/`toml`/`figment`, already dependencies; no
  schema-validation crate (e.g. `validator`, `garde`) is used elsewhere in
  this codebase for config, and none should be introduced here.

## Summary of dependency decisions

| Need | Considered | Decision | Why |
|---|---|---|---|
| HTTP client to OpenRouter | `openrouter_api`, `openrouter-rs`, `openrouter-sdk`, raw `reqwest` | **raw `reqwest`** (already a dep) | Matches `OpenaiProvider`'s existing pattern; avoids a second HTTP abstraction; OpenRouter is just OpenAI-compatible + 2 extra headers |
| Model-list TTL cache | `moka`, `dashmap`, plain `Mutex`+`Instant` | **plain `Mutex`+`Instant`** | Single-value cache with app-driven early invalidation, not a multi-key per-entry-TTL cache; matches `health.rs`/`histogram.rs` style |
| Per-candidate rolling latency | new stats crate, `DurationHistogram` per key | **`DashMap<String, DurationHistogram>`** (both already deps) | Reuses existing rolling-window primitive verbatim, keyed like `ProxyMetrics::upstreams` already is |
| Per-candidate rolling error rate | new stats crate, parallel `Mutex<VecDeque<..>>` | **parallel `Mutex<VecDeque<(Instant,bool)>>`** (std only) | Mirrors `DurationHistogram`'s exact shape; no existing rolling error-rate primitive to reuse, but the pattern is a one-file port |
| Static benchmark table | new leaderboard-scraping crate, const table, TOML config | **const table or TOML via existing `figment`/`toml`** | Static-by-design per requirements; no scraping; Phase 3 picks the exact shape |
| Config schema | new validation crate | **existing `serde`/`figment`/`toml`** | No precedent for a schema-validation crate in this repo |

**Net new `Cargo.toml` entries: zero.**
