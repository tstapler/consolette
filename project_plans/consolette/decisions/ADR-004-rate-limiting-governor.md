# ADR-004: Per-Upstream Rate Limiting — `governor` Direct Limiters behind `AdmissionControl`

**Status**: Accepted
**Date**: 2026-07-17

## Context

FR-4 requires per-upstream RPM and/or TPM limits, **independent per upstream**
(throttling A must not throttle B), configurable in `20-ratelimit.toml`. On breach,
behavior is per-upstream: **shed** (treat like a cooldown so routing falls
through/redistributes — default) or **queue/delay** up to a bounded wait. TPM uses
the request's estimated token count (reuse `tiktoken-rs`, already in the tree).
Decisions must be visible in `/metrics` + `/dashboard`. Rate limiting must compose
with FR-3 routing without the strategy knowing it exists.

## Decision

Use **`governor = "0.10"`** (0.10.4, MIT, GCRA — a smooth continuous leaky-bucket
equivalent, lock-free `AtomicU64` state, first-class variable-cost `check_n` /
`until_n_ready` for TPM).

- **Direct limiter per upstream per dimension — NOT governor's keyed limiter.**
  A keyed limiter shares **one `Quota` across all keys**, but our upstreams have
  *different* limits. Store `DashMap<UpstreamName, UpstreamLimiter>`; each
  `UpstreamLimiter` owns up to two `Direct` limiters: `rpm` (cost 1 via `check`)
  and `tpm` (cost = estimated tokens via `check_n(NonZeroU32)`).
- **Two dimensions: admit only if both admit.** Check the dimension more likely to
  deny first (TPM for large requests) to minimize cross-dimension accounting drift.
- **Single integration point: `AdmissionControl::admit()` called POST-selection.**
  The router calls `admit` once, on the upstream `strategy.select()` returned, before
  the provider call — NOT as an `Availability` predicate (governor commits state on a
  successful check, so it must run at most once per attempt). This is the only
  rate-limit seam.
- **`on_breach = "shed"` (default)** → `check`/`check_n` returns `Err` →
  `Admit::Shed`. The router adds the shed upstream to `already_tried` and **re-selects
  from the remaining healthy pool** (the same re-select loop that handles a 429 —
  rejection sampling, fine for 2–5 upstreams). No separate routing code path.
  **`on_breach = "delay"`** → `tokio::time::timeout(max_delay, until_n_ready(...))` →
  `Admit::Delayed(d)` on success, `Admit::Shed` on timeout or `InsufficientCapacity`.
- **`AdmissionControl` trait** so the router depends on an interface, not governor:
  `#[async_trait] async fn admit(&self, upstream: &str, est_tokens: u32) -> Admit`.
- **Testability:** `UpstreamLimiter` is generic over `governor::clock::Clock`; tests
  use `FakeRelativeClock` for deterministic RPM/TPM assertions.
- **Token estimation is gated:** the router computes the tiktoken estimate only when a
  selectable upstream actually has a TPM limiter — no tiktoken cost when TPM is
  unconfigured.
- **Config = table-of-tables** `[ratelimit.upstreams.<name>]` + `[ratelimit.defaults]`
  (NOT array-of-tables): tables deep-merge by key across lexical `conf.d` files
  (ADR-001), so a later file can override one upstream's `tpm` and leave the rest.
  Omitted `rpm`/`tpm` = that dimension unlimited; an upstream absent from the table
  = no limiter built (unlimited). `on_breach`/`max_delay_ms` fall back to
  `[ratelimit.defaults]` then built-in defaults (`shed`, `2000`).
- **`InsufficientCapacity` (request tokens > TPM burst) → treated as shed.** Default
  `Quota::per_minute(tpm)` sets burst == tpm (a full minute's budget in one
  request); document that `tpm` must be ≥ the largest expected single request.
- **Metrics:** a per-upstream `rate_limits: DashMap<String, UpstreamRateMetrics>`
  (allowed/shed/delayed/tokens_charged/delay_ms_sum) rather than more hardcoded
  per-provider fields; exposed under a `"ratelimit"` key in `to_metrics_json`.

## Alternatives Considered

| Option | Rejected because |
|--------|-----------------|
| governor **keyed** limiter (`DefaultKeyedRateLimiter`) | Shares one `Quota` across all keys — cannot express different RPM/TPM per upstream. We reproduce keyed ergonomics with `DashMap<name, UpstreamLimiter>`, each with its own quotas. |
| Hand-rolled token bucket | Reimplements GCRA, variable-cost charging, and burst semantics that governor provides lock-free and battle-tested (`tower_governor` builds on it). |
| Array-of-tables `[[ratelimit]]` | Arrays *replace* on deep-merge; a later conf.d file would clobber the whole list. Table-of-tables merges by key. |
| Atomic two-dimension check | governor has no atomic multi-dimension check; strict accounting via `NotUntil` peek is over-engineering for a single-tenant proxy. Accept minor drift; order TPM-first. |
| Reuse the 300s health cooldown for local limiting | A proactive RPM/token-bucket breach should exclude an upstream only for the current attempt (via the `already_tried` re-select), not trip a 300s 429-style cooldown. The two mechanisms stay separate: health cooldown (ADR-003) vs. post-selection admission (here). |
| Make rate limiting an `Availability` predicate | Rejected — a predicate can be polled 0..N times per attempt; governor commits on check, so that would corrupt the bucket. One post-selection `admit()` call instead. |

## Consequences

- New dep: **`governor = "0.10"`**; reuse existing `tiktoken-rs = "0.5"` for the
  token estimate (same tokenizer as compression / `count_tokens`).
- **`dashmap` bumped 5 → 6** — governor 0.10 pulls `dashmap ^6.1`; done as a
  standalone, tree-wide early task (plan Story 1.0, inventorying `cache.rs`,
  `slots.rs`, `memory/`, `metrics/`, `mcp-proxy`) BEFORE routing/rate-limit work
  depends on it, so the tree always compiles.
- `NonZeroU32` guards: `est_tokens == 0` (empty/parse-fail) skips the TPM check.
- The router estimates tokens once per request (only when a TPM limiter applies) and
  calls `admit` post-selection on the chosen upstream; `Shed` → `already_tried` +
  re-select, `Delayed` dispatches after the wait.
- **Accepted trade-off — TPM/RPM double-charge on failover:** because `admit` charges
  the chosen candidate and the request may then shed/fail over to another upstream,
  the first (rejected) candidate keeps its charge. For a single-tenant personal proxy
  this over-count is negligible and preferable to the complexity of a pre-commit peek;
  documented rather than engineered around.
- TPM is **approximate** — `tiktoken-rs` (OpenAI BPE) applied to
  Anthropic/Bedrock/gateway models is an estimate; acceptable as a personal
  guardrail, documented as such.
- Dynamic reload (deferred): direct limiters have no live quota mutation; on config
  change, rebuild the affected `UpstreamLimiter` and `insert` it into the `DashMap`.
</content>
