# ADR-013: Pricing table sourced from vendored LiteLLM snapshot, static-first

**Status**: Accepted
**Date**: 2026-08-15

## Context

No provider (Anthropic, OpenAI) publishes a machine-readable pricing
endpoint (`research/build-vs-buy.md` §2, `research/features.md` §2 — both
confirmed via direct web research, not assumption). The requirements'
"optional best-effort live lookup against provider APIs" therefore has
nothing to call on the provider side. LiteLLM's
`model_prices_and_context_window.json` (MIT-licensed, outside its
`enterprise/` tree) is the closest thing to an authoritative, continuously
updated source (`research/stack.md` §2, `research/build-vs-buy.md` §2).

## Decision

- **Amendment (2026-08-15, repair iteration 1):** the in-memory type is
  `ModelPrice { input_usd_per_token: f64, output_usd_per_token: f64 }` —
  the unit is in the field name, not left implicit. LiteLLM's source JSON
  (`model_prices_and_context_window.json`) is per-token
  (`input_cost_per_token`), not per-million as an earlier draft of the
  implementation plan assumed; the vendoring/loading code must not rescale
  it. `CostTracker` prices each record once, at write time, using this
  per-token type directly (`tokens as f64 * input_usd_per_token`), and a
  golden-value test pins this: `input_usd_per_token = 0.000003` over `8_000`
  tokens must equal `0.024`.
- Vendor a filtered snapshot (Anthropic + OpenAI models only) of LiteLLM's
  JSON as `src/cost_metrics/pricing_default.json`, checked into the repo as
  a fixture, loaded at startup as the static/default pricing table.
- User-supplied config overrides (by model name) take precedence over the
  vendored table; both are merged into one `PricingTable` at construction.
- No inline network call to any provider "pricing API" — none exists to
  call. An optional background task may periodically re-fetch LiteLLM's raw
  JSON over HTTPS (`reqwest`, already a dependency) and atomically swap the
  in-memory table via `tokio::sync::watch::Sender::send`/`Receiver`
  (**repair iteration 1**: `arc_swap` is not a dependency in this repo and
  is not being added for this one swap site; `tokio::sync::watch` gives the
  same atomic-pointer-swap behavior using an already-present crate), never
  blocking `SessionCompactionPipeline::apply`. This is scoped as a stretch
  task (Epic 1.4) — the static/config table is the load-bearing path per the
  requirements' constraint that this feature must degrade cleanly.
- A model absent from the merged table resolves to `cost_usd: None`
  ("pricing_unavailable"), never `$0`, per `research/features.md` §5 pitfall
  7.
- Every response that used the static (non-refreshed, or refreshed-but-stale
  beyond a configurable staleness window) table sets `pricing_source:
  "static"` vs `"live"` in the report and logs once per fallback event
  (satisfies requirements.md's Observability Requirements).

## Alternatives rejected

- **Scrape Anthropic/OpenAI's HTML pricing pages directly** — rejected;
  brittle, no versioning, explicitly against "machine-readable" spirit
  (`research/build-vs-buy.md` §2).
- **Hand-maintain the pricing table from scratch** — rejected; redundant
  with an actively-updated MIT source community-maintained specifically for
  this purpose.
- **Call an unofficial third-party pricing aggregator (OpenRouter, aipricing.guru)** —
  rejected; no more authoritative than LiteLLM's file and less
  established/version-controlled (`research/stack.md` §2).

## Consequences

- Pricing accuracy is bounded by how often the vendored snapshot is
  refreshed (manual sync script or the optional background task) — this is
  an accepted, documented staleness risk, not a silent one, because every
  dollar figure carries its `pricing_source` provenance.
- Adding a new model to the table (or fixing a wrong price) is a config
  change or a snapshot refresh, not a code change requiring redeploy.
