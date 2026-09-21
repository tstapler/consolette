# UX Research: auto-model-family

**Date:** 2026-09-12 · **Agent 5 — UX** · **Status:** research-only, no code changed

**Verdict:** There **is** a user-facing surface. `GET /dashboard` (`src/dashboard.rs:1-14`) is a live-updating HTML page (polls `/metrics` every 30s, `/errors/summary` every 60s, Chart.js via CDN), and `GET /` (`src/entrypoint/landing.rs:42-44`) advertises it as "Live-updating monitoring dashboard (charts + recent errors)". The family alias adds at minimum one new dashboard question — "which real model is `auto-coding` right now, and why?" — plus three edge states that need deliberate copy. Everything else (router resolution, opencode provider IDs) is invisible until quality or cost changes, which is exactly when Tyler will come looking.

## 1. Comparable UX patterns

### OpenRouter auto-router (`openrouter/auto`)
- **Mental model sold:** "a market index" — aggregate 7-day spend per task type picks the model; no per-user stats shown at request time (openrouter.ai/docs/guides/routing/routers/auto-router).
- **Where the decision surfaces (three places, all post-hoc):**
  1. Response `model` attribute = the real model that served the request.
  2. Activity dashboard (Aug 2026 launch: spend/tokens/cache-hit/latency per agent, per model, per request; drill into individual generations + Analytics API) — openrouter.ai/blog/announcements/activity-dashboard.
  3. Opt-in router metadata header (`X-OpenRouter-Metadata: enabled` → `openrouter_metadata.pipeline[].data.task_type`, e.g. `code:debugging`); absent when classification unavailable — graceful degradation, never a failure.
- **Controls that work:** `cost_tier` band + `cost_quality_tradeoff` 0–10 slider + `allowed_models`/`excluded_models` wildcard patterns, settable per-request or as account defaults (Settings → Plugins). Sticky sessions keep multi-turn conversations on one model until it stops being a leading choice.
- **Takeaway for consolette:** copy the *post-hoc attribution* pattern (real model ID on every response/activity row + optional "why" metadata), not the market-index ranking. Our ranking signal is local (error-rate → latency per requirements), so the "why" is two numbers, not a task classifier. Also copy the tier-as-band idea: our free-only default vs. paid opt-in alias is exactly a hard cost tier.

### LiteLLM Router / Proxy
- **Strategies that map to ours:** `latency-based-routing` (lowest time-to-first-token, TTL-cached), `simple-shuffle` default, cooldowns per deployment (not per group), `allowed_fails_policy` (N consecutive failures of a given type before cooldown), proactive `background_health_checks` + `enable_health_check_routing` that removes bad deployments *before* a user request lands on them (docs.litellm.ai/docs/proxy/health_check_routing, /docs/routing).
- **Dashboard pattern:** Admin UI → General Settings → Routing Groups: per-model-group strategy picker (e.g. latency-based for `claude-sonnet`, simple-shuffle for cheap models), editable without touching `proxy_config.yaml`; deleting a group falls back to `default` strategy immediately (docs.litellm.ai/docs/proxy/ui/routing_groups).
- **Observability pattern:** every request logs `routing_group= model= strategy=`; docs give the exact `kubectl logs | grep` and Loki LogQL to verify which group/strategy served a request. Cooldown bypass has an explicit safety-net log line ("All deployments in cooldown … bypassing cooldown filter").
- **Takeaways for consolette:**
  1. Show **strategy + group + chosen deployment** on one line per family — the LiteLLM log triple is the cheapest "why" that works.
  2. Per-family strategy (not global) matches our one-free-family + one-paid-family scope.
  3. Name and log the safety net (all-members-down bypass) instead of silently serving something surprising.

### Interaction flows that work (distilled)
1. **At-a-glance card:** family alias → current pick + one-line reason ("lowest error rate, 2.1s p50"). OpenRouter Activity row; LiteLLM routing-group row.
2. **Drill-down table:** per-member error rate / latency / last-picked-at, sorted by rank, with the excluded (cooldown/delisted) rows visibly greyed — LiteLLM cooldown semantics, OpenRouter generation drill-in.
3. **Config without redeploy:** strategy/cost-tier editable from UI, persisted (LiteLLM) or per-request overridable (OpenRouter plugins). Our equivalent is already `POST /api/route` + `GET /api/route` (`src/entrypoint/landing.rs:66-76`); the dashboard only needs to *link* to the active route, not reimplement editing.

## 2. Mental models — what Tyler will expect

- **`auto-coding` = "the free coding model that currently works."** Not "the smartest", not "the cheapest" — *the one that isn't degraded*. Any quality change will be read as "the family switched members", never as "my prompt got worse". The dashboard must therefore answer "did it just switch, and from what to what?" within seconds — a **last-change timestamp + previous pick** matters more than a latency chart.
- **Where he looks when quality changes (predicted order):** (1) opencode output itself → (2) `GET /dashboard` (already the documented habit per requirements baseline: "after noticing it on the dashboard or in `/metrics`") → (3) `/metrics` JSON → (4) `GET /api/route` to confirm pins. So the family card belongs at the **top of the dashboard**, above the existing stat-cards (`src/dashboard.rs:105-119`: Total Requests / Success / Error Rate), not buried under charts.
- **Naming risk (open question in requirements):** `auto-coding` invites the OpenRouter analogy ("market picks best coder"), but ours picks "least-broken free model". If the name overpromises intelligence, the first bad-but-fast pick erodes trust. Mitigation is copy, not code: label it `auto-coding (free pool, least-errors first)` and keep the paid alias visibly distinct (`auto-coding-paid` or similar — TBD in Phase 3).
- **Stickiness expectation:** OpenRouter explicitly documents sticky multi-turn behavior. If our resolution is per-request with no stickiness, a mid-session model swap will feel like a personality change. Either document "may change between requests" on the card or scope stickiness — flag for Phase 3, don't silently pick one.

## 3. Accessibility — minimal bar for a local dev tool

Sole user, loopback-only, desktop browser. No WCAG-audit burden, but four cheap bars apply because this page is read during incidents (tired eyes, fast skimming):

1. **Don't rely on color alone.** Existing dashboard already uses colored dots (`.status-active/.status-cooldown/.status-auth-required`, `src/dashboard.rs:41-44`) — the family card must pair each dot with a text label ("active", "cooldown", "excluded: 404").
2. **Readable without JS charts.** Chart.js is CDN-loaded; if offline/CDN-blocked the page must still show the pick + two numbers as plain HTML (server-render the card, enhance with JS polling — progressive enhancement).
3. **Polling must not steal context.** Existing `setInterval` polling (30s/60s, `src/dashboard.rs:4-5`) is already less disruptive than meta-refresh; keep the family card to text swaps (no animation reset), and keep the 30s cadence or faster only for the pick line — stale "current pick" is worse than stale charts.
4. **Copy-pasteable IDs.** Real model IDs as selectable text (not canvas), so a delisted ID can be pasted into `POST /api/route` rollback or OpenRouter catalog search.

## 4. Error / edge states needing graceful UX

| State | What Tyler sees today (bad) | Graceful UX (proposed) |
|---|---|---|
| **Cold start (stats empty)** | Empty table / 0% rates (`src/dashboard.rs:107-118` defaults) — ambiguous with "everything broken" | Explicit `Cold start — serving config-order default (<model-id>) until N requests accumulate` + show the deterministic default (requirements feasibility risk already mandates config-order default). Distinguish "no data yet" from "all failing". |
| **All family members down / in cooldown** | LiteLLM's trap: silent bypass looks like success | Named safety-net banner: `All auto-coding members unhealthy — bypassed cooldown and served <model-id> at <time>` (copy LiteLLM's log line into the UI) + red card. Never silently serve paid from the free alias to cover the gap (violates "no new paid spend by default"). |
| **Paid alias accidentally selected** | Invisible spend | Paid alias card in a visually distinct style (different border/label, e.g. `auto-coding:paid — may spend`), plus per-alias resolution counters in `/metrics` (already required) surfaced as `paid resolutions: N` on the card. Consider a confirm step in opencode provider labeling, not just dashboard. |
| **Member delisted (404) / excluded pre-dispatch** | Router "fails fast with no failover" (requirements risk) — looks like an outage | Show the member as `excluded: not in catalog (last checked <time>)` greyed row; the pick line says `resolved before dispatch — dead IDs excluded`. This exclusion list is the highest-value "why" content. |
| **Tie / flapping (two members statistically tied)** | Rapid pick oscillation reads as instability | Show `last change: <time> (previous: <model-id>)` + only re-rank on meaningful delta or cooldown event; document the hysteresis rule in one line. Prevents "why did it switch again?" distrust. |
| **Stale stats (lifetime counters never forget)** | Once-bad model penalized forever (requirements rabbit hole) | Show window/age on the card (`stats window: last N reqs / since <time>`) so Tyler can tell a stale penalty from a fresh signal. Window design is Phase 3; the display slot must exist from day one. |

General rule from both comparables: **the "why" is two signals only** (error-rate, latency — requirements rabbit hole warns against sprawl). Every extra column is scope creep; every missing edge label is a support ticket to yourself.

## 5. Job-to-be-done lens — "never touch route config again"

- **Functional JTBD:** When a free model degrades, automatically serve the next-best family member so I never `POST /api/route` again (success metric: zero manual swaps for a month). Dashboard proof = current pick + two driving numbers + last-change line. Nothing more is needed to declare the job done.
- **Emotional JTBD:** *Confidence without vigilance.* Today Tyler babysits pins via dashboard/`/metrics`. The family alias must convert monitoring from "am I about to be paged by my own proxy?" to "I can glance and trust it". That means the card's dominant emotion is **calm**: stable pick, boring reason ("0 errors, fastest of 3"), visible-but-quiet edge banners. Flapping picks or unexplained quality drops destroy this job even if functionally "correct".
- **Social JTBD:** Minimal — single user, no team. The only social surface is future-self / debugging-self: the dashboard + `/metrics` per-alias counters (resolutions, fallback-to-default) are the handoff to the person diagnosing next month's weird output. Optimize for that reader: timestamps, previous-pick, exclusion reasons.
- **If truly no user-facing surface?** Not the case here — rejected. Resolution itself is invisible, but the *trust* in resolution lives entirely in the dashboard card + metrics counters. Ship the alias without the card and the JTBD fails: Tyler will keep manually checking pins because he can't see the automation working.

## Recommendations for Phase 3 (non-binding, UX-only)

1. One family card per alias at top of dashboard: `alias → current real model ID (copy-pasteable) + error% / p50 latency + last-change time + previous pick + stats-window age`.
2. Ranked member table (2 columns + status label): error-rate, latency, `active | cooldown | excluded:<reason> | cold-start-default`.
3. Safety-net banner for all-down bypass; visually distinct paid-alias card; link card to `GET /api/route`.
4. Server-render the card text; JS polling only updates values (CDN-failure-safe, no chart dependency).
5. Keep "why" to the two ranking signals; put window/decay, stickiness, and alias naming in Phase 3 decisions with the open questions in requirements (`requirements.md:107-111`).

## Sources

- Codebase: `project_plans/auto-model-family/requirements.md`; `src/dashboard.rs:1-14,41-44,105-119`; `src/entrypoint/landing.rs:42-44,66-76`.
- OpenRouter: `openrouter.ai/docs/guides/routing/routers/auto-router` (market-index model, `model` attribute, `X-OpenRouter-Metadata` task_type, cost_tier/allowed_models, sticky sessions); `openrouter.ai/blog/announcements/activity-dashboard` (per-agent/model/request Activity); `openrouter.ai/openrouter/auto/status` (routed-model attribution).
- LiteLLM: `docs.litellm.ai/docs/routing` (strategies, cooldowns per deployment); `docs.litellm.ai/docs/proxy/health_check_routing` (proactive removal, `allowed_fails_policy`, bypass log line); `docs.litellm.ai/docs/proxy/ui/routing_groups` (per-group strategy UI, immediate apply, default fallback); `docs.litellm.ai/docs/router_architecture` (`routing_group= model= strategy=` log triple).
