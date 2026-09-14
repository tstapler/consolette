# ADR-002: Per-(upstream, model) stats keying alongside existing per-name map

**Date**: 2026-09-12 · **Status**: Accepted

## Context

`ProxyMetrics::upstreams` is `DashMap<String, UpstreamCounters>` keyed by upstream name (counters.rs:46) with lifetime sums. A family resolves multiple model IDs through shared OpenRouter upstreams — one bucket cannot rank members. requirements.md and pitfalls §11 flag this gap.

## Decision

Add a parallel `DashMap<(UpstreamIdx, ModelId), MemberStats>` (display grouped by model) fed from the existing `record_attempt` call site, which already has both `chosen.name` and the effective model string. Existing per-name map and `/metrics` `providers`/`provider_latency` sections are untouched (dashboard compat). `UpstreamIdx` reuses the `UpstreamRef.index ↔ HealthRegistry` identity convention; the model half disambiguates members sharing an upstream.

## Consequences

- One extra DashMap lookup/update on the attempt path (atomics only, no guard across await).
- Decay/windowing lives in the new dimension; lifetime counters stay as-is.
- Restart amnesia accepted explicitly (see plan Migration section).

## Alternatives rejected

- Re-keying the existing map to per-model: breaks dashboard compat and conflates upstream health with model quality.
- Deriving ranking from the `RequestDetail` ring buffer: display-grade, unbounded aggregation per pick, no decay semantics.
