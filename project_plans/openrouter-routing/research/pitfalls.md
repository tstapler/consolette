# Research: Pitfalls & Risks — openrouter-routing

Agent 4 (Pitfalls), SDD Phase 2. Answers the two Open Questions in
`requirements.md` about OpenRouter's actual free-model rate limits and
catalog churn, plus general risk areas for the three sub-features (new
upstream, model-list cache, composite-score strategy).

## 1. OpenRouter free-tier models — behavior specific to this codebase's plan

### Rate limits (answers Open Question: "actual free-model rate limits")

VERIFIED against OpenRouter's own docs/support articles (Sep 2026):

- **Per-minute**: every model whose ID ends in `:free` is capped at **20
  requests/minute**, regardless of account status.
- **Per-day**: account-wide (not per-model) —
  - **50 requests/day** if lifetime purchased credits are under $10.
  - **1,000 requests/day** once $10+ has ever been purchased (stays
    elevated even if balance later drops toward $0).
- Older "200 requests/day" figures circulating in blog posts/tutorials are
  outdated; several 2026 secondary sources still repeat the stale number —
  don't trust anything but `openrouter.ai/docs/faq` /
  `openrouter.zendesk.com` at implementation time.
- These caps are **account-wide across all `:free` models**, not
  per-model — hammering one free model and failing over to another in the
  pool does not raise the ceiling. This directly affects sizing: a
  composite-score strategy that spreads load across N free models still
  shares one 20-rpm / 50-or-1000-rpd budget. Cooldown-on-429 is necessary
  but the aggregate ceiling means the whole pool can go into simultaneous
  exhaustion under sustained load, which is exactly the `ProviderError::Exhausted`
  path the requirements already call for — good, but size expectations
  accordingly (this is a single-user local proxy, so 20 rpm is likely fine,
  but a burst of Claude Code retries could still trip it).

  Sources: [OpenRouter FAQ](https://openrouter.ai/docs/faq),
  [OpenRouter Rate Limits – What You Need to Know](https://openrouter.zendesk.com/hc/en-us/articles/39501163636379-OpenRouter-Rate-Limits-What-You-Need-to-Know),
  [OpenRouter Free Tier 2026: Rate Limits, Models, BYOK](https://klymentiev.com/blog/openrouter-free-tier).

### Catalog churn (answers Open Question: "how often does the catalog change")

VERIFIED/INFERRED from OpenRouter's changelog and multiple independent
community reports (Sep 2026):

- Free models are added, removed, silently re-versioned, or have their
  `:free` tag stripped **without notice** and on no fixed cadence — reports
  describe this as "constant" churn, not a stable multi-week cycle.
  Concrete recent examples: `deepseek-chat-v3:free` → replaced by
  `deepseek-chat-v3-0324:free`; `moonshotai/kimi-k2.6` lost its `:free` tag;
  `deepseek/deepseek-v4-flash:free` lost free status; six older models
  (Llama 3.x, Hermes 3, Qwen3 Coder, Dolphin Mistral, Tencent Hy3) were
  deprecated within the same week in July 2026.
- Practical implication for this feature: **treat `:free` model IDs as a
  volatile pattern, never a stable identifier to persist beyond the cache
  TTL.** Do not let any part of the design (bench-rank table, dashboard
  history, logs) key long-lived state off a specific free model ID without
  expecting it to vanish. This directly informs the multi-hour TTL choice
  in Scope: given churn is reported as happening on the order of
  days-to-weeks per individual model (not hours), a multi-hour TTL is
  reasonable for freshness, but the **error-triggered early invalidation
  path is the one that actually matters day to day** — a model can vanish
  at any moment inside the TTL window.
- Recommended concrete invalidation trigger (resolves the "precise
  definition" open question in part): a `404`/`ProviderError::ModelUnsupported`-style
  response from OpenRouter's chat completions endpoint for a model ID that
  is currently in the cached free-list should invalidate the whole cached
  list (not just that entry) and force a refetch on the *next* selection
  attempt, since the same event that killed one model's `:free` status
  often coincides with a broader catalog refresh upstream.

  Sources: [OpenRouter API Changelog](https://openrouter.ai/docs/changelog),
  [Free LLMs on OpenRouter Keep Going 404. I Fixed It With 120 Lines of Python](https://dev.to/josh_green_dev/free-llms-on-openrouter-keep-going-404-i-fixed-it-with-120-lines-of-python-43i1),
  [OpenRouter FREE Models gist](https://gist.github.com/rlnorthcutt/e6f392cd1ffb1339cc42dfb024c3cf7f).

### A 404 that is NOT catalog staleness — the data-policy trap

A distinct, commonly-reported 404 on **every** free model simultaneously —
`"No endpoints available matching your guardrail restrictions and data
policy"` — is caused by the account's Settings → Privacy data-policy
toggle, not by the model actually vanishing. OpenRouter's free endpoints
require opting in to prompts/completions being usable for model
improvement; if that toggle is off, every `:free` request 404s regardless
of catalog freshness.

**Pitfall for this feature**: if the error-triggered cache-invalidation
rule treats *any* 404 as "model went stale, evict and refetch," a
misconfigured/changed OpenRouter account setting will cause an
invalidate-refetch-fail loop against every model in the pool, burning
`/models` calls and API calls in a tight retry cycle while never actually
fixing anything (the refetched list looks fine; every model still 404s).
The invalidation condition should distinguish "this specific model id is
gone from `/models`" (real staleness — refetch is the fix) from "every
candidate just failed with the same error" (likely an account/policy/auth
problem — refetching won't help, this should surface as a clear error
rather than loop).

Source: [Why do all free models return a 404: "No endpoints available matching your guardrail restrictions and data policy"?](https://openrouter.zendesk.com/hc/en-us/articles/51690904755227-Why-do-all-free-models-return-a-404-No-endpoints-available-matching-your-guardrail-restrictions-and-data-policy)

### Provider-routing behind a single model ID

OpenRouter itself load-balances a single model ID across multiple backing
inference providers (by default weighted toward cheapest-reliable; `sort`
param can bias toward throughput). This means:

- **Latency and error-rate samples for "one model" are actually a mixture
  across however many providers OpenRouter routed to**, each with
  different latency/availability/regional characteristics. A rolling
  latency/error histogram keyed only by model ID (as this feature's Scope
  implies — "per upstream-index/model") will show noisier, more
  bimodal-looking distributions than a single dedicated backend would,
  purely from OpenRouter's own provider mixing — not because the model
  itself got worse. Don't over-react to variance here as if it were a
  quality signal on the model; it may just be provider-mix noise.
  OpenRouter's own "Exacto" endpoints exist specifically because raw
  per-model quality varies by which provider serves a given call.
- No action item beyond documentation: the rolling latency/error tracking
  this feature adds will absorb this variance automatically (that's what
  rolling stats are for) — flagging it so Phase 3's scoring-formula design
  doesn't try to explain away noise that's actually provider-mix, and so a
  low sample-count window isn't over-trusted (see composite-scoring
  section below).

  Sources: [Provider Variance: Introducing Exacto](https://openrouter.ai/blog/announcements/provider-variance-introducing-exacto/),
  [How OpenRouter Model Routing Works](https://openrouter.ai/blog/insights/model-routing/),
  [Provider Routing docs](https://openrouter.ai/docs/guides/routing/provider-selection).

## 2. "price == 0" as a proxy for "free and usable"

Known false positives/negatives when driving model selection purely off
`/models` pricing data:

- **False positive — temporarily free / promotional pricing.** A model can
  show `pricing.prompt == "0"` during a launch promotion and later revert
  to paid. Auto-discovery must re-check pricing on every cache refresh
  (which the plan already does), but a **stale cached list inside the TTL
  window could keep routing to a model that has since started charging**,
  directly violating the "must not silently spend money" hard constraint
  in Constraints. This is the single highest-severity pitfall in the whole
  feature: the multi-hour TTL is explicitly a place where a price change
  could slip through undetected, because a price change alone usually
  doesn't produce an error response (the request would just succeed and
  bill). Mitigation to consider in Phase 3: treat a shrinking TTL or a
  belt-and-suspenders re-check (e.g. verify pricing==0 for the specific
  model actually selected, not just at list-refresh time, before or after
  dispatch) as part of the design, not just "wait for the next scheduled
  refresh."
- **False positive — capacity-limited to the point of unusability.** Being
  priced at 0 says nothing about whether the model currently has usable
  capacity; a model can be simultaneously "free" and "throttled/queued/
  erroring under load" (see §1 capacity section) — this is exactly what
  the rolling error-rate/latency component of the composite score is
  designed to catch, so this is a correctly-scoped mitigation already in
  the plan, not a gap — but it means the score, not the discovery step,
  is the layer doing this filtering. Discovery alone (price==0) should not
  be treated as an availability signal.
- **False negative — free but not tagged/priced as free in a way the
  filter catches.** OpenRouter has both individually-free models (their ID
  ends `:free`) and an aggregate `openrouter/free` "auto-router" that
  itself picks a free model in the background — pricing-field shape or
  presence of a `:free` suffix should be treated as OpenRouter's
  documented convention, but the filter should key off the actual
  `pricing.prompt`/`pricing.completion` fields (as Scope specifies), not
  string-match `:free`, since suffix conventions are that vendor's naming,
  not a contract.
- **Moderation-flagged / tool-calling-incompatible variants.** Free-tier
  endpoints commonly don't support tool/function calling and 404 on tool
  requests even though the model is validly free and otherwise healthy.
  If consolette (via Claude Code) sends tool-use requests through an
  OpenRouter free-model route, a chunk of the "healthy, free" candidate
  pool may reliably fail every tool-call request while succeeding on plain
  chat — which the rolling error-rate signal will eventually detect and
  down-rank, but only after burning failed requests each time it's tried.
  Worth a note in the eventual doc/config: free-tier + tool-use is a known
  rough edge independent of this feature's own correctness.

  Sources: [OpenRouter free tier models fail with tool use when `:free` suffix is present](https://github.com/block/goose/issues/3054),
  [OpenRouter free models fail with HTTP 404: tool calling not supported on :free tier](https://github.com/NousResearch/hermes-agent/issues/49983).

## 3. Composite scoring (latency + error rate + static bench rank)

General, well-documented failure modes for this class of scorer, mapped to
this feature's specific inputs:

- **Unnormalized units dominating the score.** Latency in raw milliseconds
  (tens to thousands), error rate as a 0.0–1.0 fraction, and a bench
  rank/score on yet another arbitrary scale (e.g. an aider-polyglot
  percentage or a LiveBench 0–100 score) cannot be linearly combined
  without normalization — whichever term happens to have the largest
  numeric range will dominate regardless of intended weight. This is
  flagged explicitly as a Rabbit Hole in requirements.md already; the
  research-level pitfall to add is that the *direction* of normalization
  matters too (lower latency = better, lower error rate = better, higher
  bench rank = better) — a naive "normalize each to [0,1] and sum" is easy
  to get backwards on one term (e.g. accidentally rewarding higher
  latency) and that class of bug is easy to miss in review because the
  code still runs and still "selects something."
- **Cold-start / zero-sample bias.** A candidate with zero recorded
  requests has an error rate of 0/0. If that's coded as "0.0 (no errors)"
  it looks *better* than every model with real traffic and any nonzero
  error rate, so a brand-new or rarely-hit free model would look
  artificially "perfect" and get over-selected — starving the
  well-behaved incumbents of traffic precisely because they've been
  battle-tested and have accumulated a few real errors. Likewise a
  zero-sample latency (no rolling samples yet) has nothing to average —
  whatever default is chosen (0ms, some ceiling, skip-latency-term-entirely)
  directly determines whether new/rarely-used models are favored or
  penalized, and either default needs to be a deliberate Phase 3 choice,
  not whatever falls out of `Option::unwrap_or_default()`. This is the
  single most common bug class in composite health/quality scorers.
- **Oscillation/thrashing between similarly-scored candidates.** If two
  candidates have near-identical composite scores and the strategy always
  picks the strict max, small amounts of sampling noise (one slow request)
  can flip the "winner" on every selection, producing traffic that
  ping-pongs between two models request-to-request rather than settling.
  Combined with a rolling window that recomputes on every request (not
  just periodically), this can also mean the *scores themselves* are
  computed from a mix of very-recent and stale samples inconsistently
  across models with different traffic volumes (a model that's picked
  often has a fresher rolling window than one picked rarely, biasing
  comparisons further). Consider whether Phase 3 wants a
  minimum-sample-count gate, a small hysteresis/stickiness term, or
  weighted-random selection proportional to score (like `WeightedStrategy`
  already does) rather than strict argmax, to avoid visible thrashing.
- **Bench rank staleness vs. live signal freshness mismatch.** The bench
  rank is static/checked-in (updated rarely, per Scope), while
  latency/error-rate are continuously rolling. A model whose live
  performance has degraded sharply (e.g. it got capacity-throttled by
  OpenRouter today) still carries its old, good bench rank — the composite
  needs the live terms to be able to actually override a good bench rank
  when live signal is bad enough, i.e. the weighting can't let the static
  term dominate so much that live degradation is invisible in practice.
  Worth a concrete acceptance check in Phase 4: "a model with a great bench
  rank but 100% live error rate must not be selected over a
  worse-benched-but-healthy model."

## 4. Caching a remote list with multi-hour TTL + error-triggered invalidation

- **Cache stampede / thundering herd on invalidation.** If the model list
  is stored as a single shared cache entry and an error triggers
  invalidation, every in-flight or immediately-following request that
  needs the free-model list will see "cache empty/expired" simultaneously
  and can all fire a concurrent `/models` refetch. For a single-user local
  proxy this is low-severity in absolute terms (no real herd of clients),
  but Claude Code itself can have several requests in flight
  concurrently, so a naive "if stale, refetch inline" implementation
  without a single-flight/refresh-lock could still fire N redundant
  OpenRouter `/models` calls for one invalidation event. A standard fix is
  a single-flight refresh (one refetch in progress; concurrent callers
  await the same in-flight future or serve stale-while-revalidate) rather
  than each caller independently deciding to refetch.
- **Stale-model-id requests returning confusing errors to the end
  client.** If invalidation is lazy (only triggered by the *next* request
  hitting the stale model), the request that discovers the staleness is
  the one that eats the failure — from Claude Code's perspective this
  looks like an ordinary upstream error on an otherwise normal request,
  not a "the model list needed a refresh" condition. Whatever error
  surfaces here should map to the existing `ProviderError` taxonomy in a
  way that's distinguishable in logs/dashboard from a genuine model
  failure (e.g. `ProviderError::ModelUnsupported` vs. a `Validation`), so
  Tyler isn't left debugging "why did OpenRouter reject this model" when
  the real cause was "the cache hadn't refreshed yet." Given the
  Observability Requirements already call for exposing "last invalidation
  reason," make sure that reason distinguishes "evicted due to
  model-not-found" from "evicted due to TTL expiry" from "evicted due to
  data-policy/account-wide error" (see §1) so the dashboard can actually
  explain a burst of failures after the fact.
- **TTL interacting with cooldown duration.** If the model-list cache TTL
  and the `HealthRegistry` cooldown duration are tuned independently, a
  model could be evicted from the free-model list (cache TTL expired,
  refetch shows it's no longer free/gone) while still sitting in an active
  `HealthRegistry` cooldown from an earlier failure, or vice versa —
  cooldown expires and the strategy tries to re-select a model that the
  now-refreshed list has already dropped. Both codepaths need to agree on
  "is this model index still a valid candidate at all," not just "is it
  healthy" — likely means candidate construction should re-derive from the
  freshest cached list on every selection rather than caching a
  long-lived `Vec<UpstreamRef>` that can drift out of sync with the model
  list cache.

## 5. Rust-specific concurrency pitfalls for new per-candidate shared state

Grounded in the actual code in this repo:

- **`src/routing/health.rs`'s documented hard rule** (lines 8–10): "never
  hold a `DashMap` guard across `.await`... this holds by construction as
  long as no `.await` is added inside these methods." `HealthRegistry`
  currently gets away with pure-sync `Instant` comparisons. The instant
  per-candidate latency/error-rate state is added (per the Scope item
  extending `DurationHistogram`/`ErrorTracker` to per-candidate
  granularity, likely via a `DashMap<usize, DurationHistogram>` or
  similar), **every new method must stay just as strictly synchronous** —
  recording a sample or reading percentiles must not become an `async fn`
  or call anything that awaits while a `DashMap::get`/`get_mut` guard is
  live. The existing histogram/error-tracker internals already only ever
  do `Mutex::lock()` + synchronous work, so wrapping them per-candidate
  behind `DashMap<usize, Arc<DurationHistogram>>` (entry lookup, then drop
  the DashMap guard before touching the inner `Mutex`, or clone the `Arc`
  out of the guard first) preserves the existing hard rule — but it's an
  easy rule to violate by accident if a future refactor adds any async
  work (e.g. "record to a metrics backend over HTTP") inside what looks
  like the same call site.
- **Double-locking / lock-ordering risk from nesting two lock types.** A
  per-candidate structure combining a `DashMap` (for the per-index
  registry) with an inner `Mutex` (for each candidate's histogram, mirroring
  `DurationHistogram`'s existing design) means a hot path can end up
  holding a DashMap shard lock while also acquiring the inner Mutex. This
  is fine as long as **only one lock is held at a time** (get the DashMap
  entry, extract/clone the `Arc<Mutex<..>>` or `Arc<DurationHistogram>`,
  drop the DashMap guard, *then* lock the Mutex) — but if a future author
  reads `DashMap::get_mut` and inlines Mutex work inside that closure
  without noticing the nested-lock hazard, it's a straightforward
  deadlock/contention bug to introduce, and DashMap's per-shard internal
  locking makes it easy to not notice in testing (single-threaded tests
  won't reveal shard-lock contention).
- **Poisoning strategy must stay consistent.** `DurationHistogram` and
  (presumably) `ErrorTracker` both use
  `.lock().unwrap_or_else(std::sync::PoisonError::into_inner)` to recover
  from poisoned mutexes rather than propagating a panic. Any new
  per-candidate wrapper needs the same recovery discipline — a
  newly-introduced `.lock().unwrap()` on one code path while the rest of
  the codebase uses `PoisonError::into_inner` would make one panicking
  candidate's recording thread poison that candidate's lock and then panic
  every subsequent reader/writer for that specific model, silently
  removing just that one candidate's health signal from the composite
  score (it would look "stuck" rather than erroring loudly).
- **Cardinality/lifetime mismatch between `DashMap<usize, _>` keyed by
  upstream index and a dynamically-discovered, churning model list.**
  `HealthRegistry` keys cooldown state by a *static* `Config.upstreams`
  index, which works because upstreams are fixed at config-load time. The
  new free-model pool is **dynamic** — discovered at runtime, and models
  drop in/out on cache refresh/invalidation (§1, §4). If per-candidate
  latency/error state reuses the same "key by index into a Vec" pattern
  but the Vec is rebuilt on every cache refresh, indices are not stable
  across refreshes — model at index 3 today may be a completely different
  model after the next `/models` fetch reorders or shrinks the list,
  silently mixing one model's historical latency/error samples into
  another model's score. The per-candidate map almost certainly needs to
  be keyed by a stable identifier (the OpenRouter model ID string, or a
  hash of it) rather than a positional index, unlike the existing
  `HealthRegistry` — and needs an eviction/GC policy for entries whose
  model has permanently left the catalog (otherwise the per-candidate
  `DashMap` grows unbounded across every free model OpenRouter has ever
  listed over the process's lifetime, since nothing currently removes
  entries — `HealthRegistry` never removes entries either, but its
  cardinality is bounded by static config, which won't hold for a churning
  discovered list).

## Summary of Open-Question answers (for requirements.md cross-reference)

| Open Question | Answer |
|---|---|
| Actual free-model rate limits, per-model or account-wide? | 20 req/min per `:free` model; 50/day (or 1,000/day after $10 lifetime spend) **account-wide**, not per-model. |
| How often does the free catalog change, for TTL calibration? | No fixed cadence — described as "constant"; concrete individual-model churn observed on a days-to-weeks cadence, but the account-wide "every free model 404s" failure mode is unrelated to catalog change and needs separate handling (§1 data-policy trap). A multi-hour TTL plus fast error-triggered invalidation (on a specific-model-not-found signal, not any-error) is reasonable given this. |
