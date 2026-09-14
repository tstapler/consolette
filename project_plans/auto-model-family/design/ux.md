# UX Design: auto-model-family

**Date:** 2026-09-12 · **Status:** design-only, no code changed
**Sources:** `project_plans/auto-model-family/requirements.md`,
`project_plans/auto-model-family/research/ux.md`, `src/dashboard.rs`
**Placement:** family card at top of `GET /dashboard`, above the existing
stat-cards (`src/dashboard.rs:105-134`).

## 1. Surfaces

### Interactive (full design below)

| # | Surface | Location | Actions |
|---|---------|----------|---------|
| S1 | Family card | Dashboard top, above stat-cards | Glance (read pick + why); no editing on card |
| S2 | Member table | Inside family card, below pick line | Read rank + status; copy model ID |
| S3 | Safety-net banner | Top of family card (conditional) | Read bypass notice; follow rollback link |

Dashboard does not edit routes. Editing stays `POST /api/route`; the card
links to `GET /api/route`.

### Non-interactive (condensed)

**N1 — family block in route TOML (`conf.d/`).** Representative sample:

```toml
[[model_families]]
alias = "auto-coding"
allow_paid = false

[[model_families.members]]
upstream = "openrouter-code"
model = "cohere/north-mini-code:free"

[[model_families.members]]
upstream = "openrouter-code-fallback"
model = "poolside/laguna-s-2.1:free"

# Opt-in gate on the active route: the alias expands only when the route names it.
[[routes]]
name = "default"
strategy = "fallback"
family = "auto-coding"
```

- Alias is synthetic (never sent upstream); members stay within configured models.
- Free-only default, enforced at load AND hot-swap; paid members require the separate opt-in alias.
- Member order is the deterministic cold-start pick order.
- Unknown upstream or paid-in-free member fails config load (fail-closed, never fatal to other families); a freshly delisted ID still fails its first request (no failover on 404), then lands on the 1h denylist — repeat requests are protected.

**N2 — `/metrics` JSON additions.** Sample:

```json
{ "family": { "auto-coding": {
  "current_pick": "cohere/north-mini-code:free",
  "previous_pick": "qwen/qwen3-coder:free",
  "last_change_at": "2026-09-12T10:04:00Z",
  "window_age_s": 900,
  "resolutions_total": 412, "fallback_to_default_total": 3,
  "members": [
    {"model": "cohere/north-mini-code:free", "error_rate": 0.4, "latency_p50_ms": 2100, "samples": 412, "status": "active"}
  ] } } }
```

- Per-alias `resolutions_total` and `fallback_to_default_total` counters present.
- `current_pick` is the real model ID that served/will serve; copy-pasteable string.
- `last_change_at` + `previous_pick` present so a switch is answerable in seconds.
- Stats window/age exposed (`window_age_s` field) so stale penalties are visible.

**N3 — opencode `consolette` provider IDs.** Sample: model ID `auto-coding`.

- Family alias appears alongside pinned models, labeled free pool.
- Paid alias (if added) labeled distinctly (e.g. `auto-coding-paid — may spend`).
- Selecting the alias needs no opencode restart/reconfig when members change.
- Rollback = `POST /api/route` hot-swap to the pinned route (no restart); reselecting the pin in opencode works as a secondary path.

**N4 — CLI / API outputs.** Sample: `GET /api/route` response includes the
family entry with alias, members, and current pick.

- Route output marks family entries vs. pinned entries unambiguously.
- Request logs show `family= pick=<real-id>` (LiteLLM-style triple); probe resolutions add `reason=probe`.
- Rollback path (`POST /api/route` hot-swap to pinned route) documented on card.

## 2. Dashboard card design

### 2.0 Name mapping (ux label → plan type)

| Card / docs label | Plan type | Notes |
|---|---|---|
| `auto-coding` | `FamilyAlias` | Synthetic ID the client sends (free pool; confirmed) |
| `auto-coding-paid` | `FamilyAlias` | Synthetic ID the client sends (paid opt-in; confirmed) |
| pick / current real model | `current_pick` (`ResolutionSnapshot`) | Never the alias |
| err % / p50 | `MemberStats` (decayed) + `samples` | `cold` below 20 samples (N=20 confirmed + Wilson gate) |
| banner | `SafetyNetBypass` event | Cooldown/empty-pool only |
| excluded rows | `ExclusionReason` (`cooldown`, `excluded:404`, `cold`) | 1h TTL on 404 denylist |
| cold-start label | `ColdStartDefault` | Config-order first healthy |
| paid card counters | `ResolutionCounter` (per-alias) | Free/paid tables never leak |
| log triple | `family=<alias> pick=<real-id>` | LiteLLM-style attribution; probe resolutions log `reason=probe` |

### 2.1 ASCII wireframe (normal state)

```
+-- FAMILY: auto-coding (free pool, least-errors first) -------- [via GET /api/route] --+
| NOW SERVING (copy-pasteable): cohere/north-mini-code:free                            |
| why: err 0.4% (412 req) · p50 2.1s · window: last 500 req · window age 900s         |
| last change: 10:04 (prev: qwen/qwen3-coder:free)                                     |
| stable: challenger needs err >2pp AND p50 >10% to dethrone (hysteresis)              |
| pinned sessions: 2 (see GET /api/sessions)                                          |
| +------------------OWA------+--------+---------+----------------------------+        |
| | MEMBER (ranked)            | ERR %  | P50     | STATUS                     |        |
| +----------------------------+--------+---------+----------------------------+        |
| | cohere/north-mini-code:... | 0.4%   | 2.1s    | (•) active — serving       |        |
| | qwen/qwen3-coder:free      | 1.9%   | 2.6s    | (•) active                 |        |
| | x/ai-model:free            | —      | —       | (•) excluded: 404 ...      |        |
| +----------------------------+--------+---------+----------------------------+        |
| Session pins take precedence; pick sticks per session, re-evaluates on              |
| cooldown/exclusion event or every 50 family resolutions.                             |
+-------------------------------------------------------------------------------------+
```

Paid alias (only if configured) renders as a second card with a distinct
border + `PAID — may spend` label and a `paid resolutions: N` line.
Never merged into the free card.

Safety-net banner (conditional, top of card, red border, text — not color-only).
Copy-pasteable rollback with exact route name `default-pinned`:

```
!! All auto-model-family members unhealthy — bypassed cooldown, served <model-id> at <time>.
   Rollback (copy-paste): curl -X POST http://localhost:PORT/api/route -H 'Content-Type: application/json' -d '{"name":"default-pinned"}'
   Next retry on healthy member immediately; bypass clears on next healthy resolution.
```

### 2.2 Interaction flow: glance → drill → rollback

1. **Glance (≤5s):** pick line + why line answer "which real model, and why?"
   without scrolling. Poll updates text in place (no animation reset), 30s
   cadence matching existing `loadMetrics()`.
2. **Drill:** member table (ranked, max 2 signal columns: err %, p50) shows
   which member is next-best and which rows are excluded + why
   (`active | cooldown Ns | excluded:<reason> | cold-start-default`).
   Model IDs are selectable text for paste into `POST /api/route` or catalog
   search.
3. **Rollback link:** card header links to `GET /api/route`; banner names the
   exact rollback (`POST /api/route` hot-swap to pinned route, no restart).
   No editing UI on the dashboard — link only, per LiteLLM lesson.

### 2.3 Error / edge states (all six)

| State | Card copy (exact or template) | Exit path |
|---|---|---|
| Cold start | `Cold start — serving config-order default (<model-id>) until 20 requests accumulate.` Status label `cold-start-default`. | Resolves itself after 20 requests; no action needed. Table shows `—` (never `0%`) for members with no data. |
| All-down bypass | Banner above (2.1). Card border red + `BYPASSED` text label. | Banner names served ID + time + copy-pasteable rollback curl; clears automatically on next healthy resolution. Never serves paid from the free alias. |
| Paid-selected | Separate card style: `auto-coding-paid — PAID, may spend` + `paid resolutions: N`. | Switch opencode model back to `auto-coding` or a pin; counter confirms spend stopped. |
| Delisted member | Greyed row: `excluded: 404 (denylisted 1h, last hit <time>)`. Pick line appends `dead IDs excluded before dispatch`. First request after a fresh delist still fails once (404 → immediate validation error, no failover), then feeds the denylist. | Refresh membership via conf.d at leisure; repeat requests protected, but request N fails — plan for one failure per rotation. |
| Tie / flap | `last change: <time> (previous: <model-id>)` always visible; card one-liner `stable: challenger needs err >2pp AND p50 >10% to dethrone (hysteresis)`; probe resolutions tagged `probe: <model-id>` on the pick line and `reason=probe` in logs. | No user action; timestamp proves stability. If flapping persists, session-pin via existing pin mechanism. |
| Stale stats | `stats window: last 500 reqs, age 900s / since <time>` line on card from day one. | Stale penalty is recognizable as old window; window/decay tuning is Phase 3, display slot is now. |

Progressive enhancement: card is server-rendered HTML; JS polling only swaps
values. CDN-blocked (Chart.js fails) still shows pick + two numbers.
Polling (30s cadence): pick line uses `aria-live="polite"` so screen-reader
users get one calm announcement per poll, never interrupted mid-read.

## 3. UX acceptance criteria (human-testable)

Bar: no color-only status (dot + text label always), works CDN-blocked,
IDs copy-pasteable, every state has an exit path, no dead ends.

1. **Glance (3 steps):** open `GET /dashboard` → family card is above
   stat-cards → read pick + `err %` + `p50` within 5s. Pass: no scroll needed.
2. **Drill (3 steps):** read member table → rows sorted by rank with status
   labels (`active/cooldown/excluded:<reason>`) → copy a member ID as text.
   Pass: IDs selectable, excluded rows greyed *and* labeled.
3. **Rollback link (2 steps):** click card's `GET /api/route` link →
   `POST /api/route` pinned route restores traffic without restart
   (banner shows copy-pasteable curl with exact route name `default-pinned`).
   Pass: round trip works; card reflects pinned state after.
4. **Cold start (2 steps):** fresh stats → card shows
   `Cold start — serving config-order default (<model-id>)`. Pass: explicit
   label, not `0%`/empty table; clears after 20 requests.
5. **All-down bypass (3 steps):** force all members unhealthy → banner
   `All auto-coding members unhealthy — bypassed cooldown and served
   <model-id> at <time>` appears with red border + text (not color-only) →
   follow rollback hint. Pass: free alias never serves a paid ID.
6. **Paid-selected (2 steps):** select paid alias → distinct `PAID — may
   spend` card + `paid resolutions: N` increments. Pass: visually distinct
   from free card; counter visible.
7. **Delisted member (2 steps):** delist one member → greyed row
   `excluded: 404 (denylisted 1h, last hit <time>)`, pick line notes
   `dead IDs excluded before dispatch` + first-request-fails-once note. Pass: repeat traffic unaffected (request N fails, N+1 skips).
8. **Tie/flap (2 steps):** force near-tie → `last change` + `previous pick`
   visible, pick stable across polls (hysteresis err >2pp AND p50 >10%); probe
   resolutions tagged `probe: <model-id>`. Pass: no oscillation
   per 30s poll.
9. **Stale stats (1 step):** card shows `stats window: last 500 reqs, age 900s /
   since <time>`. Pass: age of signal always visible.
10. **CDN-blocked (2 steps):** block `cdn.jsdelivr.net` → reload dashboard →
    family card text (pick + err % + p50) fully readable with charts broken.
    Pass: no JS-chart dependency for card content.
11. **No color-only (1 step):** inspect each status row with color ignored
    (grayscale/devtools). Pass: every dot paired with a text label.
12. **Pinned sessions (2 steps):** with 2 sessions stuck/pinned → card shows
    `pinned sessions: 2` + link to `GET /api/sessions` → follow link shows
    per-session member IDs. Pass: per-session serving model answerable from
    card (Epic 4.2 sessions-view skipped in favor of this).

Count: 12 criteria (8 interactive incl. a11y bar, 4 edge/state coverage
folded into the six states above).
