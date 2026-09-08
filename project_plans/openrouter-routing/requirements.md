# Requirements: openrouter-routing

**Date**: 2026-09-05
**Type**: feature addition
**Complexity**: 3 — system design

## Problem Statement
Consolette can currently route requests to Anthropic, Bedrock, a generic
OpenAI-compatible endpoint, and Gemini, but has no way to reach OpenRouter's
catalog of free-tier models, and no routing strategy that picks among several
candidate models by anything other than static weight or config order
(`FallbackStrategy`, `WeightedStrategy` — `src/routing/strategy.rs`). For
Tyler, this means he can't automatically spread load across OpenRouter's free
models or steer away from ones that are currently slow, erroring, or weak at
code generation — he'd have to watch OpenRouter's dashboard and edit config by
hand.

## Baseline
Today, using a free OpenRouter model means manually configuring it as a
generic `openai`-kind upstream (or not using OpenRouter at all), picking one
model, and manually swapping the config when that model degrades, gets
deprecated, or hits its rate limit. There is no per-model health/quality
signal driving that choice — only the existing binary cooldown
(`HealthRegistry`) and static `weight`.

## Users / Consumers
Tyler, running consolette as his own local Anthropic-compatible proxy in
front of Claude Code / other Anthropic-API clients, configuring an
`openrouter` upstream in his own `conf.d/*.toml`.

## Success Metrics
- An `openrouter` upstream kind can be configured and successfully proxies
  requests to OpenRouter's OpenAI-compatible Chat Completions API.
- Free models available on OpenRouter (price = 0 in its `/models` listing)
  are discovered automatically rather than hand-listed one by one in config.
- A new routing strategy selects among the discovered free models using a
  composite score of (a) rolling per-model latency, (b) rolling per-model
  error rate, and (c) a coding-benchmark rank/score — measurably shifting
  traffic away from a model as its live latency/error signal worsens,
  without requiring a config edit.
- When every free model in the pool is unavailable (cooling down /
  rate-limited), the request fails clearly (`ProviderError::Exhausted`)
  rather than silently falling back to a paid upstream.

## Appetite
Large (3–6 weeks)
*(Scope must fit the appetite. If it doesn't fit, cut scope — do not move the deadline.)*

## Constraints
- No specific deadline.
- Must not silently spend money: staying within the configured free-model
  pool is a hard behavioral requirement, not a preference (see Success
  Metrics and Scope). This guarantee is provably enforced by two mechanisms
  that don't depend on any open question: (a) list-membership filtering
  against the auto-discovered free-model list, and (b) a per-dispatch
  recheck of the *specific selected model's* cached price, not just its id
  presence — together these close the "stale list" / "cache never
  populated" failure modes entirely, not just bound them. It is further
  hardened by (c) a post-hoc cost-field backstop. As of 2026-09-08
  (`sdd:6-verify`), OpenRouter's public docs confirm the needed field exists
  (`GET /api/v1/generation?id=<id>` → `total_cost`), so (c) is implementable
  — but it is not yet implemented (needs a live API key to verify
  end-to-end, plus a check-every-request-vs-sampled design decision; see
  `implementation/plan.md`'s Unresolved Questions). Until that follow-up
  ships, mechanisms (a)+(b) still hold, but the residual free→paid mid-TTL
  exposure remains bounded-but-not-closed (up to ~15 minutes / ~300 requests
  at 20 req/min, per ADR-001), and shipping in the meantime without (c)
  requires Tyler's explicit sign-off as an accepted interim risk
  (`implementation/plan.md` Risk Control) — it is a human decision, not an
  engineering guarantee, until (c) lands. This caveat scopes *how* the
  requirement is provably closed; it does not weaken the requirement itself.
- Must follow the existing `deny_unknown_fields` / `SecretRef`-based config
  conventions in `src/config/schema.rs`, and the existing `Provider` trait
  contract in `src/providers/mod.rs` (no bespoke dispatch path).

## Non-functional Requirements
- **Performance SLO**: not specified beyond "don't hammer OpenRouter's
  `/models` endpoint on every request" — addressed by the model-list cache
  (see Scope).
- **Scalability**: single-user local proxy scale; not applicable.
- **Security classification**: internal (Tyler's own OpenRouter API key,
  handled the same way existing upstream secrets are — via `SecretRef`).
- **Data residency**: no special requirements.

## Scope

### In Scope
- New `UpstreamKind::Openrouter` + `OpenrouterProvider` implementing the
  `Provider` trait (OpenAI-compatible request/response, OpenRouter's
  recommended headers, auth via existing `SecretRef`/`AuthMethod`
  machinery).
- Free-model auto-discovery via OpenRouter's `/models` endpoint (models
  whose pricing is 0), replacing hand-listing each free model in config.
- A cache for that model list: refreshed on a multi-hour TTL, invalidated
  early on an upstream error signal that suggests staleness (e.g. a model-id
  the upstream rejects). Exact TTL value and which errors trigger
  invalidation are informed by Phase 2 research into OpenRouter's actual
  rate limits and how often its free-model catalog changes.
- A new `RoutingStrategy` (per ADR-003's existing strategy abstraction) that
  scores each health-filtered candidate using: rolling per-candidate
  latency, rolling per-candidate error rate, and a coding-benchmark
  rank/score, then selects among them. This requires extending today's
  global-only latency/error tracking (`DurationHistogram`, `ErrorTracker`)
  to per-candidate granularity.
- A coding-benchmark ranking table: a static, checked-in default (seeded
  from a public coding leaderboard such as aider's polyglot benchmark or
  LiveBench), with a documented path for Tyler to override or refresh it.
  Per Phase 3/4's design decision (`implementation/plan.md` Task 4.1.1d):
  "documented path" means editing the table's source (a doc-commented Rust
  const) and rebuilding consolette — a deliberate, accepted simplification,
  not an implied config-file/runtime override mechanism. A live-fetched or
  `conf.d`-mergeable override table was considered and rejected for scope
  reasons (Large appetite already fully allocated across 9 epics — see
  Rabbit Holes).
- Exhaustion behavior: when no candidate in the free-model pool is
  available, the route fails (`ProviderError::Exhausted`) — it does not
  fall back to a paid upstream. (A user who wants a paid fallback can still
  configure one as a separate route today; this feature does not build
  that fallback automatically.)
- Observability: per-candidate score components (latency, error rate, bench
  rank) and model-list cache state (age, last refresh, last invalidation
  reason) are exposed through the existing metrics/dashboard surface
  (`MetricsCollector::to_metrics_json`), not just internal state.

### Out of Scope
- Automatically scraping/crawling third-party coding leaderboards on a
  schedule — the default ranking table is static; a live-fetched leaderboard
  is explicitly not required (Tyler chose a static-default + manual/override
  path over building a scraper).
- Automatic fallback from the free-model pool to a paid upstream.
- Any change to non-OpenRouter providers (Anthropic, Bedrock, generic
  OpenAI, Gemini) or to `FallbackStrategy`/`WeightedStrategy`'s existing
  behavior.
- Tool-use/vision content translation beyond what the existing
  `OpenaiProvider` already supports for OpenAI-compatible upstreams (no new
  translation capability is required specifically for OpenRouter).
- UI/dashboard redesign — new data is exposed through the existing
  `to_metrics_json` surface, not a new dashboard page.

## Rabbit Holes
- **Scoring formula design.** Combining three heterogeneous signals
  (latency in ms, error rate as a fraction, a benchmark rank/score on some
  other scale) into one selection score is an open design problem, not a
  known formula — likely to need normalization and tunable weights. Phase 3
  should fix a concrete, simple formula rather than building a general
  configurable weighting system.
- **Per-candidate metrics granularity.** `DurationHistogram`/`ErrorTracker`
  are currently global singletons; making them (or an equivalent) per
  upstream-index/model is a structural change touching `HealthRegistry`'s
  neighborhood — scope this to exactly what the new strategy needs, not a
  general per-upstream-metrics refactor.
- **OpenRouter rate-limit/ToS specifics for free models.** Free-model rate
  limits, and whether they differ by model or are account-wide, are not yet
  confirmed — needed to size the cooldown duration and the exhaustion
  behavior correctly. Flagged for Phase 2 research.
- **Model-list cache invalidation trigger.** "Invalidate on an error that
  suggests staleness" needs a concrete definition (e.g. a specific
  `ProviderError::Validation`/404-style model-not-found response) — don't
  let this expand into general cross-upstream cache-invalidation
  infrastructure.

## Alternatives Considered
- Reusing the existing generic `UpstreamKind::Openai { base_url }` pointed
  at OpenRouter, with each free model hand-listed as its own upstream/route
  entry. Rejected: no automatic free-model discovery, and stale config
  as OpenRouter's free-model lineup changes — the whole point of this
  feature is to stop doing that by hand.
- Live-fetching a coding-benchmark leaderboard automatically. Rejected for
  v1: adds a scraping/parsing dependency on a third-party site's format for
  a score that changes infrequently; a static, occasionally-refreshed table
  is simpler and was Tyler's preference (index: "some combination" — static
  default with a manual/override path, not an automated scraper).

## Feasibility Risks
- OpenRouter's actual free-model rate limits and how often its `/models`
  catalog changes are unconfirmed — affects both the cache TTL and the
  cooldown-duration tuning for the health registry. (Phase 2 research item.)
- OpenRouter-specific response fields/quirks (e.g. its own
  generation-stats endpoint, provider-routing metadata in responses) are
  unresearched — affects how precisely per-candidate latency can be
  measured (round-trip from consolette vs. OpenRouter-reported).
- No prior art in this codebase for per-candidate (as opposed to global)
  metrics — the extension point in `HealthRegistry`/`DurationHistogram`
  needs a design pass, not just a copy-paste.

## Observability Requirements
- Per-candidate score inputs (rolling latency, rolling error rate, bench
  rank) and the resulting composite score, exposed via the existing
  `MetricsCollector`/dashboard JSON surface.
- Model-list cache state (last refresh time, cached model count, last
  invalidation reason) exposed the same way.
- Standard per-request logging (existing `RequestDetail` path) covers
  request-level detail; no new alerting/oncall condition is required for a
  single-user local proxy.

## Risk Control
- Opt-in by construction: nothing changes for existing routes/upstreams
  until Tyler adds an `openrouter`-kind upstream and a route that selects
  the new scoring strategy — no feature flag needed beyond that config
  choice.
- Rollback: remove the `openrouter` upstream/route from config; no
  migration or data to roll back.

## Open Questions
- What should the composite scoring formula be, precisely (normalization,
  relative weights of latency/error-rate/bench-rank)? → Phase 3.
- What are OpenRouter's actual free-model rate limits, and do they vary
  per model? → Phase 2 research.
- How often does OpenRouter's free-model catalog actually change, to
  calibrate the "couple of hours" cache TTL and error-triggered
  invalidation condition precisely? → Phase 2 research.
- Exact default contents/source of the coding-benchmark ranking table
  (which public leaderboard, which models) → Phase 2 research / Phase 3.
- Precise definition of "an error that suggests the model list is stale"
  for early cache invalidation → Phase 3.
