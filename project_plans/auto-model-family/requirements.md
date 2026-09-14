# Requirements: auto-model-family

**Date**: 2026-09-12
**Type**: feature addition
**Complexity**: 3 — system design

## Problem Statement

consolette's route pins each upstream entry to one hardcoded model ID
(`RouteUpstreamRef.model`, e.g. `cohere/north-mini-code:free`). When a pinned
free model degrades (slow, erroring, or delisted by OpenRouter), requests keep
hitting it until someone manually rewrites the route via `POST /api/route`.
The user wants to address a stable synthetic family instead (e.g. opencode
sends `auto-coding`) and have the proxy resolve it to the currently-best real
model ID from its own stats. Confirmed aliases: `auto-coding` (free-only
default) + `auto-coding-paid` (opt-in paid).

## Baseline

`~/.config/consolette/conf.d/10-openrouter.toml`: two OpenRouter upstreams
pinned to two `:free` coding models in `fallback` order, then
anthropic/bedrock. A degraded pin is handled by a manual `POST /api/route`
swap after noticing it on the dashboard or in `/metrics`. Model IDs are
verified live against OpenRouter's catalog at setup time only.

## Users / Consumers

Tyler (sole operator of this loopback proxy) via Claude Code (`/v1/messages`)
and opencode (`/v1/chat/completions` with a `consolette` provider family).

## Success Metrics

Zero manual route swaps for a month: every model degradation inside the
family is absorbed by automatic re-resolution, measured against the current
baseline of manual `POST /api/route` edits. Leading indicator (weekly):
`fallback_to_default_total` stays flat and no manual `POST /api/route` swaps
are logged — both visible on the dashboard card.

## Appetite

Large (3–6 weeks)
*(Scope must fit the appetite. If it doesn't fit, cut scope — do not move the deadline.)*

## Constraints

- Loopback-only proxy posture unchanged: no inbound auth, no new network surface.
- Single user, single machine (cross-machine stat sharing is out of scope).
- No new paid spend by default: family membership stays within configured models.

## Non-functional Requirements

- **Performance SLO**: p99 resolution overhead ≤1ms vs the static-pin baseline at family sizes ≤8, measured by the Story 3.3 AC4 benchmark.
- **Stability**: resolution must not flap on single-user noise — N≥20 samples with Wilson-interval gate before stats override config order (confirmed); hysteresis requires challenger to beat incumbent by err >2pp AND latency >10% (confirmed); ExplorationProbe every 25th request; per-member in-flight cap 2–4; sticky per session with re-evaluation every K=50 family resolutions or on cooldown/exclusion event (confirmed).
- **Offline readability**: the dashboard family card is server-rendered and fully readable with charts/CDN blocked.
- **Scalability**: single-user request volume; not applicable beyond that.
- **Security classification**: internal (API keys remain env-referenced, never inline).
- **Data residency**: no special requirements.

## Scope

### In Scope

- Router resolves a synthetic family alias to a real model ID per request,
  ranked by local stats: error rate first, latency second (both already
  tracked per upstream in `/metrics`).
- opencode `consolette` provider gains the family model IDs.
- Dashboard shows the live pick and why (which stats drove it).
- The default family routes between free models only; a separate opt-in
  alias (different ID) allows paid-model selection.
- Session-aware: existing per-session pins take precedence over family
  resolution, and sessions can be pinned/moved dynamically between family
  members (e.g. pin session X to model Y, move session Z off a degraded
  member) without a route rewrite. Family pick is STICKY-PER-SESSION:
  it sticks per session with periodic re-evaluation (on member
  cooldown/exclusion event or every K family resolutions, K default 50,
  implementer-tunable); explicit pin/move still available.

### Out of Scope

- Sharing stats or picks across machines.
- Changing macOS `install` behavior or the daemon-install plan.

## Rabbit Holes

- Stat windowing/decay: lifetime counters in `/metrics` never forget, so a
  once-bad model stays penalized forever unless Phase 3 designs aging explicitly.
- `/metrics` is keyed by upstream *name*, not model ID — per-model ranking
  needs a stats dimension that does not exist yet.
- Dashboard "why" display can sprawl; keep it to the two ranking signals.

## Alternatives Considered

- OpenRouter's built-in auto-router (delegates the decision; kept as an open question, not chosen).
- Keep manual pins with more fallback entries (zero code, but fails the success metric).
- Static weighted list (spreads load but never re-ranks on degradation).

## Feasibility Risks

- Router returns immediately on auth/validation errors with no failover — a
  delisted (404) family member fails fast instead of falling through, so
  resolution must exclude dead IDs *before* dispatch, not rely on failover.
- Free-model IDs rotate on OpenRouter's side; family membership needs
  refreshing independent of stats.
- Cold start: with no local stats yet, the first pick needs a deterministic
  default (config order).

## Observability Requirements

- Dashboard: current family pick plus the error-rate/latency figures behind it.
- `/metrics`: per-alias counters (resolutions, fallback-to-default events).
- Standard request logging sufficient; no oncall (single user).

## Risk Control

Rollback is the existing `POST /api/route` hot-swap back to the pinned route
— instant, no restart, no redeploy. Resolution should ship as an opt-in route
entry so the pinned entries remain a working fallback during rollout.

## Open Questions

- ~~Alias naming and how many families~~ RESOLVED-CONFIRMED: `auto-coding` (free) + `auto-coding-paid` (paid opt-in).
- Stat decay/window design for the error-rate and latency signals? *(product contract fixed: a member with 10 consecutive successes after any error history must outrank its past within the window; mechanism = implementer spike, windowed-deque default, 50-line budget)*
- ~~Delegate vs. build: use OpenRouter's auto-router?~~ Resolved in Phase 2: build the ranker (OpenRouter `auto` viable only as bounded opt-in paid alias).
