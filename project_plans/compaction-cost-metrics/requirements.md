# Requirements: compaction-cost-metrics

**Date**: 2026-08-15
**Type**: feature addition
**Complexity**: 3 — system design

## Problem Statement
Consolette's `session_compaction` pipeline (`SessionCompactionPipeline::apply`, `src/session_compaction/mod.rs`) already decides how aggressively to compact a session's message history, but nothing measures the effect of that decision. There is no way to answer "did compacting this session actually save tokens and money, and how much, compared to letting the context window grow uncompacted?" — which is the question the user needs answered to tune compaction tier thresholds (`TierThresholds`) with real evidence instead of guesswork.

## Baseline
Today, token accounting is global and cost-blind:
- `src/metrics/counters.rs`'s `ProxyMetrics` tracks `tokens_before`/`tokens_after`/`requests_compressed` as global `AtomicU64` counters exposed under `/metrics`'s `"compression"` key — not scoped per session, so two different sessions' compaction behavior can't be compared.
- `src/metrics/mod.rs`'s `RequestDetail` ring buffer holds only the last 100 requests, still not session-scoped.
- `src/providers/mod.rs:267-275` already extracts exact `usage.input_tokens`/`output_tokens` from real Anthropic responses, but only uses them to translate response shape to OpenAI format — they're discarded, never fed into any metric.
- No pricing table (model → $/token) exists anywhere in the repo.
- `CompactionReport` (`src/session_compaction/hooks.rs:20-24`) carries `tier`, `tool_result_stats`, and `summarizer_stats` — no token or size fields at all.

The only way to answer the cost question today is to manually read raw request logs and do the arithmetic by hand, per request, with no cost conversion.

## Users / Consumers
The proxy operator (the user, running consolette locally/self-hosted) — this is an operational/tuning tool, not an end-user-facing feature. Consumed via a new CLI subcommand and a new API endpoint, both exposed by the existing `clap`-based CLI and the existing HTTP server (`src/main.rs`'s `mcp`/proxy binary surface).

## Success Metrics
- For a given session key, the operator can retrieve: total tokens actually sent (post-compaction, from real `usage.input_tokens`), estimated tokens that would have been sent without compaction (counterfactual, tokenizer-estimated), and the estimated dollar difference between the two — via both the CLI subcommand and the API endpoint, and the two surfaces agree (same underlying aggregation).
- Given two synthetic sessions in a test — one held at `CompactionTier::Full` for its whole history, one held at `CompactionTier::Off` — the tool reports a materially lower cumulative token/cost figure for the `Full` session, proving the comparison is directionally trustworthy end to end.

## Appetite
Large (3–6 weeks) — revised up from Medium after Phase 2 research found no live HTTP server exists in this codebase to attach the new endpoint to (see Open Questions); standing up a minimal server bootstrap is now in scope alongside the original cost-metrics work.

## Constraints
- No new persistent datastore: reuse the `moka`-cache-per-`SessionKey` pattern already established by `SessionStateStore` (`src/session_compaction/session_state.rs`) rather than introducing a database.
- Must not add per-request latency on the hot compaction path if the provider-pricing lookup requires a network call — that lookup must be async/cached/background-refreshed, never inline in `SessionCompactionPipeline::apply`.
- Explicitly out of scope: deriving `SessionKey` from real client traffic (the unresolved spike documented in `session_compaction::session_state`'s module docs and the design doc's "The real gap" section). This feature works within a single already-keyed session; it does not solve session identity.

## Non-functional Requirements
- **Performance SLO**: `SessionCompactionPipeline::apply` must not regress in the existing benchmarks/tests beyond noise — token accounting is bookkeeping added to a hot path, not itself a hot computation.
- **Scalability**: same order of magnitude as `SessionStateStore` today (max 1000 sessions, 1hr TTL) — no new scaling requirement.
- **Security classification**: internal/operator-only tooling; the API endpoint has no new external-facing attack surface beyond what the existing `/metrics`-style endpoints already expose.
- **Data residency**: not applicable — local/self-hosted proxy, no new data leaves the process except the (cached) provider-pricing lookup calls.

## Scope
### In Scope
- Extend `CompactionReport` with per-`apply()` token accounting: actual tokens used (from real `usage.input_tokens`/`output_tokens` where the request has completed and that data is available) and an estimated counterfactual (what the request would have cost uncompacted), using a real tokenizer (tiktoken-based, per the user's explicit choice) rather than a char-count heuristic — building out the estimator already anticipated (but not implemented) in `src/ratelimit/mod.rs`'s `est_tokens` doc comment and `project_plans/consolette/implementation/plan.md` task 4.3.1.
- A session-scoped aggregator (new — nothing today groups metrics by `SessionKey`) accumulating cumulative actual vs. counterfactual tokens per session over its lifetime, reusing the `moka` cache pattern from `SessionStateStore`.
- A pricing table converting tokens → dollars: a small built-in default table (Anthropic + OpenAI models, since `providers/mod.rs` already translates between both response shapes), user-overridable via config, with an optional best-effort live lookup against provider APIs if a machine-readable pricing endpoint genuinely exists for a given provider (see Feasibility Risks — this is unconfirmed and must degrade cleanly to the static table if it doesn't).
- A new CLI subcommand that prints the per-session actual-vs-counterfactual tokens/cost comparison.
- A minimal HTTP server bootstrap (none exists today — see Open Questions) plus a new API endpoint on it exposing the same aggregated data as JSON. Scoped to the smallest server needed to host this one route; wiring up `providers`/`routing` into a full live proxy server is explicitly not part of this feature (see Out of Scope).

### Out of Scope
- Wiring `providers`/`routing`/`session_compaction` into a full live proxy server — the minimal server bootstrap added to scope above exists only to host the new cost-metrics route, not to make consolette a running proxy.
- Deriving/discovering `SessionKey` from real client request traffic (separate, already-flagged spike).
- Any UI/dashboard beyond the CLI output and raw JSON endpoint.
- Historical persistence beyond the session's cache TTL (1hr, matching `SessionStateStore`) — this is live operational tooling, not a long-term analytics store.
- Multi-tenant/multi-user cost attribution — single-operator tool.

## Rabbit Holes
- **Provider pricing endpoints may not exist.** Neither Anthropic nor OpenAI is known to publish a stable, machine-readable per-token pricing API — their pricing lives on marketing pages, which are not a contract to scrape. Phase 2 research must confirm or refute this before Phase 3 plans around it; if no such endpoint exists, the live-lookup piece degrades to "static table only," which the user already accepted as a fallback.
- **tiktoken doesn't understand Anthropic's tokenizer.** tiktoken is OpenAI's BPE vocabulary; Anthropic's actual tokenizer is different and not public. A tiktoken-based estimate of an Anthropic request's counterfactual token count will be an approximation, not exact — this needs to be stated plainly in the output (e.g., a `counterfactual_is_estimated: true` flag) so the user doesn't mistake it for the same precision as the real `usage.input_tokens` figure.
- **Reconstructing "what would have been sent without compaction"** requires knowing the *pre-compaction* message array at the moment `apply()` ran, not just its size — `SessionCompactionPipeline::apply` already receives `messages: &Value` before compaction, so this is available in-process, but plumbing it through to the aggregator without cloning large payloads on every request needs care.

## Alternatives Considered
- Extending the existing global `ProxyMetrics` counters to be session-keyed instead of building a new aggregator — rejected because `ProxyMetrics` is a flat set of `AtomicU64`s with no keying concept, and retrofitting keying onto it is a larger, riskier change than adding a parallel session-scoped structure alongside it.
- Using the character-count heuristic already in `src/bin/mcp-proxy`'s `estimate_token_count` instead of a real tokenizer — rejected per the user's explicit "need a real tokenizer" answer.

## Feasibility Risks
- Provider pricing-lookup endpoints may not exist in a usable form (see Rabbit Holes) — the static/configurable table must work standalone as the load-bearing path, with live lookup as a pure enhancement.
- tiktoken crate choice/licensing/maintenance in Rust needs a quick research check (which crate, active maintenance, MSRV compatibility with this crate's toolchain).
- Anthropic-tokenizer accuracy gap (tiktoken approximates it) could make the estimated counterfactual numbers misleading if not clearly labeled as estimates.

## Observability Requirements
Standard request logging is not sufficient here since this *is* the observability feature. In scope:
- A log line (or metric) emitted whenever the live pricing lookup falls back to the static table (so silent staleness is visible).
- The new API endpoint's response should include, per session, both the actual and counterfactual figures plus which token-count source was used for each (`exact` vs. `estimated`) so the operator can judge confidence at a glance.

## Risk Control
Low risk / additive only — this is new instrumentation and two new read-only surfaces (CLI + endpoint); it does not change `SessionCompactionPipeline::apply`'s existing compaction behavior or outputs, only adds accounting alongside them. No feature flag needed; rollback is deleting/reverting the new module if it proves wrong.

## Open Questions
*(all resolved by Phase 2 research — see `research/` for full findings)*
- ~~Does Anthropic or OpenAI expose any machine-readable pricing data at all~~ — **Resolved: no.** Neither provider publishes one; sync the static table from LiteLLM's MIT-licensed `model_prices_and_context_window.json` instead of hand-maintaining it, with the config-overridable table as the load-bearing fallback (see `research/build-vs-buy.md`, `research/stack.md`).
- ~~Which Rust tiktoken crate, and what encoding per model?~~ — **Resolved, with a correction to the original premise.** `tiktoken-rs` (already an unused dependency) covers the OpenAI-side counterfactual. For the Anthropic side, use Anthropic's real `POST /v1/messages/count_tokens` endpoint instead of tiktoken — Anthropic's own docs confirm tiktoken undercounts Claude tokens by 15–30%+, so this is more accurate than the tiktoken-only approach requirements.md assumed (see `research/build-vs-buy.md`). The `counterfactual_is_estimated` flag mitigation from Rabbit Holes still applies to the OpenAI/tiktoken path.
- ~~What HTTP path/verb convention should the new endpoint follow to stay consistent with the existing `/metrics` endpoint?~~ — **Resolved, and it changes scope.** There is no live HTTP server in this codebase today (no `axum::serve` call anywhere; `src/main.rs` never assembles `providers`/`routing`/`session_compaction` into a running server) — `/metrics` as referenced in the Baseline is not a running endpoint to match. Decision (user, 2026-08-15): plan a minimal server bootstrap as part of this feature rather than scoping the HTTP surface out. **Appetite and Scope below are updated accordingly — Phase 3 should size this as the bigger of the two epics.**
- ~~Should the CLI subcommand and the HTTP endpoint share one Rust function?~~ — **Resolved: yes.** New top-level `src/cost_metrics/` module (sibling to `metrics`/`ratelimit`), with a shared `CostTracker::report_for_session()` function consumed by both the CLI subcommand and a router-agnostic HTTP handler on the `State<Arc<T>>` pattern already used by `src/memory/mod.rs`'s `MemoryAppState` (see `research/architecture.md`).
- **Known limitation, not resolved by this release**: nothing wires real proxy traffic (`providers`/`routing`) into `SessionCompactionPipeline::apply()` — that wiring is explicitly Out of Scope above. So this feature's Success Metrics are validated only against the synthetic Full-vs-Off test harness (plan.md Epic 4.1); `serve-cost`'s tracker will not accumulate real production data until a separate, tracked follow-up wires live traffic into `apply()`. See plan.md's "Known Limitation" section for detail.
