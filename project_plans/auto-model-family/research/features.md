# Research: Features — auto-model-family

**Date**: 2026-09-12 · **Agent 2 (Features)** · RESEARCH-ONLY, no code changed

## 1. In-codebase precedents

### 1a. `RouteUpstreamRef.model` pin — the thing being replaced
- `src/config/schema.rs:149-158` — `RouteUpstreamRef { name, weight, model }`; `model` overrides the outgoing body `"model"` when that upstream is selected. Today one entry = one hardcoded ID (e.g. `cohere/north-mini-code:free`). A family alias needs either a new variant (e.g. `models: [...]` / `family: ...` alongside `model`) or a synthetic `UpstreamRef` resolved to a real model at dispatch time. `deny_unknown_fields` means any new key must be schema-added.

### 1b. Strategies are pure + health-blind — natural insertion point
- `src/routing/strategy.rs:1-64` — `RoutingStrategy::select(healthy: &[UpstreamRef]) -> Option<UpstreamRef>`; `FallbackStrategy` = first-healthy (config order), `WeightedStrategy` = OpenRouter-style weighted pick with proportional redistribution when a peer is absent. Strategies never see cooldown state; `Router` pre-filters. A family resolver fits as (a) a new `RoutingStrategy` (e.g. `FamilyStrategy` ranking by stats), or (b) a pre-selection rewrite of `model` on the candidate, leaving existing strategies untouched.

### 1c. `Router` dispatch loop + error-class branching
- `src/routing/router.rs:1-10` (doc comment, ADR-003) — validation and auth errors return immediately (no failover); rate-limit trips cooldown and continues; other transient errors continue without cooldown; same-upstream retries stay inside the provider. `effective_candidates` (`router.rs:219-236`) consults `SessionOverrideStore` before strategy — session pins narrow to one `UpstreamRef` (with optional model override) and survive `POST /api/route` hot-swaps via a shared `Arc`.
- Implication: family resolution happens **before** dispatch (pick a real model ID, then dispatch), not as failover — matches the requirements' feasibility risk that 404/auth fail fast and can't be caught by failover.

### 1d. `HealthRegistry` cooldown (active/passive-adjacent)
- `src/routing/health.rs:32-84` — per-upstream-index `Cooldown { until }`, `trip()` with optional `Retry-After` override, `can_cooldown=false` escape hatch (Bedrock never cools down). Only rate-limit trips it today; error-rate/latency ranking for families is additive, not a replacement.

### 1e. `session_overrides.rs` per-session pins
- `src/routing/session_overrides.rs:1-45` — in-memory `session_id -> { upstream, model }`, keyed off `metadata.user_id` verbatim, fails safe to normal routing when absent. Precedent for sticky per-session behavior — directly relevant to the "flapping" edge case (see §3): OpenRouter's auto-router pins model+provider per conversation for cache warmth; consolette could optionally pin family pick per session.

### 1f. Metrics keyed by upstream name, not model
- `src/metrics/counters.rs:131,218` + `to_json_*per_upstream*` tests — per-upstream `DashMap`, one entry per upstream actually tried; `src/metrics/mod.rs:142` `MetricsCollector`. Requirements rabbit hole confirmed: lifetime counters keyed by upstream *name*. Family ranking needs a **per-(alias,member-model) dimension** that doesn't exist yet, plus decay (lifetime counters never forget). Lag/histogram (`DurationHistogram`, lag-chart buckets in `mod.rs:274-336`) is the closest existing windowing precedent.

### 1g. `ProviderError` taxonomy
- `src/providers/mod.rs:36-42,94-105` — `RateLimited{,WithRetry}`, `Auth`, `Validation(msg,status)`, `is_retryable` / validation+auth fail fast. Family design must decide which signals feed error-rate (5xx/timeout/rate-limit yes; 401/auth and 400/validation no — they indicate config/client, not model health) and must exclude delisted (404) members pre-dispatch.

## 2. Industry landscape

| System | Mechanism | Relevance to consolette |
|---|---|---|
| **OpenRouter auto-router** (`openrouter/auto`, `auto-beta`, NotDiamond → own task-type rankings) | Classifies prompt → ranks candidates by trailing-7d community spend per task; `cost_quality_tradeoff` / `cost_tier` dial; `allowed_models` wildcards; **pins model+provider per conversation** (implicit fingerprint or explicit `session_id`) for cache warmth; graceful degrade to default set | Closest analog. Consolette differs deliberately: rank by **local** error-rate/latency, not global spend; free-only default ≈ `cost_tier: low` + `allowed_models` allowlist; should copy session stickiness + never-fail-routing-infra patterns |
| **OpenRouter provider routing** (default) | Deprioritize providers with outages in last 30s → inverse-square price weighting among stable → rest as fallbacks; `order/only/ignore/sort/max_price/allow_fallbacks` | Precedent for the exact ranking shape (health filter → weighted pick) and for `:nitro`/`:floor` suffixes (latency vs price sort) — family "free vs paid alias" mirrors this split |
| **OpenRouter two-layer failover** | Provider-layer (same model, auto, on by default) vs model-layer (`models` array, opt-in, priority order, reliable floor last, walks once) | Consolette's family *is* the model layer; keep provider failover (cooldown) underneath. Copy "floor model last" and "walk once" rules |
| **LiteLLM Router** | 6 strategies (weighted, latency-based, rate-limit-aware, least-busy, lowest-cost, custom Python) + `fallbacks` / `context_window_fallbacks` / `content_policy_fallbacks` maps, `complexity_router_config` tiers, per-key/team overrides | Menu of ranking signals consolette is deliberately scoping down to two (error-rate, latency). `context_window_fallbacks` is a failure mode consolette should handle: cheap-model context overflow should escalate, not count as model "error" |
| **Classic LB active/passive + cooldown** | Health checks remove bad backends; passive = standby until active fails | Consolette's cooldown is active/passive-lite; family ranking generalizes from binary in/out to ordered preference |
| **Factory Router (coding agents)** | Picks efficient model per session, escalates on struggle (~20-25% spend cut claimed) | Validates the "cheap default + escalate" shape; escalation trigger design is the hard part |

Key takeaway: nobody ranks purely on local error+latency — market signals (spend share) or static tiers dominate. Local-stats ranking is simpler and private, but cold-start and low-traffic staleness are known weaknesses industry solves with global priors or static defaults.

## 3. Edge cases & failure modes the design must handle

1. **Cold start, no stats** — deterministic default (config order), per requirements. Must also define: do early samples immediately re-rank (flappy) or require N minimum samples?
2. **All members degraded** — need a floor: pick least-bad (never refuse to route), surface "all degraded" on dashboard; routing infra hiccup must never itself 500 (OpenRouter auto-router rule).
3. **Delisted / 404 model IDs** — fail fast (Validation, no failover per `router.rs` doc), so resolution must exclude dead IDs **pre-dispatch** via catalog refresh independent of stats (requirements feasibility risk). Distinguish 404-model-not-found (exclude member) from 400-validation (client bug — don't penalize model).
4. **Auth errors bypass failover** — 401/403 return immediately; a bad key on one upstream must not poison that model's stats nor trigger family-wide churn. Attribute auth failures to upstream config, not member quality.
5. **Flapping between models** — per-request re-rank on noisy latency can oscillate and defeat prompt caching. Mitigations: hysteresis/margin (only switch if challenger beats incumbent by X), sticky-per-session pin (precedent: `session_overrides.rs`; OpenRouter pins per conversation), minimum dwell time.
6. **Stat staleness / never-forget** — lifetime counters permanently penalize a once-bad model. Needs windowed/decayed counters (EWMA or sliding window); existing lag-bucket code is the only windowing precedent.
7. **Upstream-name vs model-ID keying** — two family members behind the same OpenRouter upstream share today's per-upstream stats; per-member attribution requires plumbing model ID through the dispatch→metrics path.
8. **Context-length overflow vs true error** — escalate to larger-context member, don't just mark error (LiteLLM `context_window_fallbacks` pattern).
9. **Rate-limit (429) handling** — already trips cooldown; family ranker must respect cooldown pre-filter and not double-count 429 as both cooldown + error-rate penalty (or define that it does, explicitly).
10. **Free-model rotation** — OpenRouter retires `:free` IDs; membership refresh cadence + dashboard "member delisted" signal needed, else family silently shrinks to one.
11. **Paid-leak guardrail** — default alias must provably never resolve to a paid ID (allowlist enforcement + test); paid opt-in alias is a separate ID.
12. **Dashboard "why" sprawl** — keep to the two signals (error-rate, latency) per requirements; show pick + figures + sample counts.
13. **Session vs request granularity** — opencode/Claude Code agent loops benefit from per-session consistency (cache warmth); stateless per-request is simpler. Open question for Phase 3.

## 4. Unstated needs (beyond explicit requirements)

- **Per-session stickiness** for agent loops (cache warmth, behavior consistency) — neither required nor excluded; industry treats it as essential.
- **Manual pin escape hatch per family member**, not just whole-route rollback — operator will want "exclude this member for now" without rewriting the route.
- **Member health visibility before failure** — dashboard should show all members ranked, not just the winner, so degradation is visible while still absorbed.
- **Cost attribution per pick** — `/metrics` per-alias counters are required; per-member spend/usage joins with existing `cost_metrics/` would close the loop on "no new paid spend."
- **Replayability/debuggability** — log pick + runner-up + margins per request (or sampled) so a bad month-end can be audited against "zero manual swaps."
- **Naming/family-count decision** (`auto-coding` only vs reasoning/chat too + paid alias name) — flagged as open question; opencode provider family IDs must match whatever is chosen.
- **Build-vs-delegate hedge** — OpenRouter auto-router as upstream-of-last-resort / floor member keeps the "local stats" scope while answering the delegate-vs-build question cheaply.
