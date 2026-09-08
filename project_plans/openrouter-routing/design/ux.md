# UX Design: openrouter-routing

**Phase**: 3 (design, pre-implementation)
**Inputs**: `../requirements.md`, `../research/ux.md`, `../implementation/plan.md`

## Scope note

This feature has **no interactive screens, modals, or flows**. Per
`requirements.md`'s Scope ("UI/dashboard redesign" is explicitly Out of
Scope) and `research/ux.md` §1–3, its only user-facing surfaces are
config files, a metrics JSON endpoint, structured logs, and client-visible
error bodies. All four are non-interactive. No wireframes or interaction
diagrams are produced — none apply.

**Accessibility: not applicable.** Every surface below is JSON, a log
line, or a TOML file consumed by one technical operator (Tyler) through a
terminal or an existing internal-only dashboard whose markup this feature
doesn't change. There is no screen, so there is no screen-reader, color-
contrast, or keyboard-navigation concern to audit (`research/ux.md` §3).

## Surface inventory

| # | Surface | Where it's defined in the plan |
|---|---------|-------------------------------|
| 1 | Config: `UpstreamKind::Openrouter` + `Strategy::OpenrouterScored` | plan.md Epic 1.1 |
| 2 | Dashboard JSON: `GET /metrics` → `openrouter_scoring` block | plan.md Epic 5.1, Story 5.1.1/5.1.2 |
| 3 | Dashboard JSON: `RequestDetail.selected_model` | plan.md Story 5.1.3 |
| 4 | Structured log: selection-decision line | plan.md Story 5.1.4 |
| 5 | Structured log: unranked-model / suppressed-invalidation warnings | plan.md Task 4.2.1c, 2.1.3b |
| 6 | Client-visible error: pool exhaustion | plan.md (reuses existing `ProviderError::Exhausted` → `src/entrypoint/errors.rs`) |
| 7 | Client-visible error: single stale bench-table entry | plan.md Epic 4.1/4.2 — explicitly **not** a client error (fail-soft) |

4 config/log/metrics surfaces + 2 error-state surfaces (one fail-closed,
one fail-soft) = **6 designed surfaces** (7 rows above, with #6/#7 forming
one paired "error visibility" design problem, per Step 2's instruction to
confirm the two are distinguishable).

---

## 1. Config: new upstream kind + strategy

```toml
# conf.d/openrouter.toml
[[upstreams]]
name = "openrouter"
kind = "openrouter"
auth = { type = "bearer", token = { env = "OPENROUTER_API_KEY" } }

[[routes]]
name = "free-coding"
strategy = "openrouter_scored"
upstreams = [{ name = "openrouter" }]
```

**Acceptance criteria**:
- No new fields to learn beyond `kind = "openrouter"` and
  `strategy = "openrouter_scored"` — auth uses the existing
  `SecretRef`/`AuthMethod` machinery Tyler already knows from every other
  upstream kind (plan.md Story 1.1.1).
- A stray field under `kind = "openrouter"` (e.g. a copy-pasted `base_url`
  from an `openai`-kind block) fails to parse with a `deny_unknown_fields`
  error naming the field — not a silent ignore. Next action is obvious:
  delete the field.
- A route with `strategy = "openrouter_scored"` but no `openrouter`-kind
  upstream in its `upstreams` list fails `Router::from_config` at startup
  with an error naming the route (plan.md Story 4.3.1) — not a runtime
  surprise on the first request. Next action: add the upstream or fix the
  route's `upstreams` list.
- Every existing `conf.d/*.toml` (Anthropic/Bedrock/OpenAI/Gemini
  upstreams, `fallback`/`weighted` strategies) continues to parse and
  behave identically — adding this feature is a zero-risk config change
  for routes that don't opt in.

---

## 2. Dashboard JSON: `openrouter_scoring` block

```json
{
  "openrouter_scoring": {
    "cache": {
      "cached_model_count": 5,
      "age_secs": 812,
      "last_refresh": "2026-09-05T14:02:11Z",
      "last_invalidation_reason": null
    },
    "models": {
      "deepseek/deepseek-chat-v3.1:free": {
        "latency_p50_ms": 1840,
        "error_rate": 0.02,
        "bench_rank": 0.551,
        "composite_score": 0.71,
        "sample_count": 46,
        "explore": false
      },
      "qwen/qwen3-coder:free": {
        "latency_p50_ms": 3120,
        "error_rate": 0.18,
        "bench_rank": null,
        "composite_score": 0.5,
        "sample_count": 3,
        "explore": null
      }
    }
  }
}
```

**Acceptance criteria**:
- Key is present only when the active route's strategy is
  `OpenrouterScoringStrategy`; omitted (not `null`) for
  `fallback`/`weighted` routes — matching every other conditional block
  already in `to_metrics_json` (plan.md Story 5.1.2).
- One object per currently-cached free model, so Tyler can see the entire
  live pool — not just the last-chosen one — at a glance: which models
  exist, which are scoring well, which are cold-starting
  (`sample_count` low) or unranked (`bench_rank: null`).
- `bench_rank: null` (as in `qwen3-coder` above) is visibly distinct from
  a real `0.0` score — an operator scanning the JSON can tell "not in the
  bench table yet" from "ranked worst," which matters because the two
  imply different next actions (refresh `BENCH_TABLE` vs. nothing).
- `explore` (Pre-mortem P2 #2) is `true`/`false` once a model has been
  selected at least once — whichever branch `select()`'s epsilon-greedy
  coin flip actually took for it — and `null` (as in `qwen3-coder` above)
  for a model that's been scored but never yet selected. Distinguishes an
  intentional exploration pick from a genuine scoring anomaly after the
  fact, without re-deriving it from the selection log.
- `cache.last_invalidation_reason` surfaces the exact string from
  `ModelListCache` (`"model_not_found:<id>"` or `"suppressed_systemic_404"`)
  verbatim, so a cache anomaly is diagnosable from this one field without
  grepping logs.
- Field grain matches the existing `cooldowns` block's precedent (flat
  object merged at the top level of the same JSON blob) — no new
  dashboard page, no new visual idiom (`research/ux.md` §2).

---

## 3. Dashboard JSON: `selected_model` on Recent Requests

```json
{
  "recent_requests": [
    {
      "request_id": "req_9f2a",
      "timestamp": "2026-09-05T14:07:33Z",
      "model": "claude-sonnet-4-5",
      "provider": "openrouter",
      "selected_model": "deepseek/deepseek-chat-v3.1:free",
      "selected_model_was_exploration": false,
      "duration_ms": 2140.0
    }
  ]
}
```

**Acceptance criteria**:
- **Operator can determine which model served a given request in ≤5
  seconds**: open the dashboard, find the row by `request_id` or
  timestamp, read `selected_model` — one field, no cross-referencing logs
  or `/metrics` at the exact moment of the request (this is the direct
  fix for the "black box" risk `research/ux.md` §5 flags as the #1
  trust-blocker for automated routing).
- `selected_model` is `null` for every non-model-pinned route
  (`fallback`/`weighted`) — additive field, zero behavior change for
  existing dashboard consumers reading `recent_requests` today.
- `model` (the client-requested model, e.g. what Claude Code asked for)
  and `selected_model` (what actually served it) are both visible in the
  same row, so a mismatch between the two is immediately legible — this
  is the OpenRouter-app pattern research/ux.md §1 calls the minimum bar
  ("which model actually served this request must be visible").
- `selected_model_was_exploration` (Pre-mortem P2 #2) is `true` when that
  pick was an epsilon-greedy exploration choice rather than the
  highest-composite greedy pick, and `null` for every non-model-pinned
  route — same nullability convention as `selected_model` — so a
  surprising-looking selection in this row is distinguishable from a
  genuine scoring failure without cross-referencing the selection log.

---

## 4. Structured log: selection decision

```
DEBUG openrouter candidate selected model="deepseek/deepseek-chat-v3.1:free" norm_latency=0.82 norm_error=0.95 bench_score=0.551 composite=0.71
```

**Acceptance criteria**:
- Exactly one `debug`-level event per `select()` call that picks
  `Some(candidate)` — no event on an empty pool (that path is error state
  #6 below, not a selection).
- Names the chosen model id and all three normalized score components
  plus the composite — enough for Tyler to answer "why did it pick that
  one" (e.g. "the better-ranked ones are cooling down") without a live
  debugger, per `research/ux.md` §5 point 2.
- Emitted via the same `tracing` machinery as every other log line in the
  codebase — no new log format/sink to learn.

---

## 5. Structured log: fail-soft warnings

```
WARN openrouter model missing from BENCH_TABLE, using neutral default model="qwen/qwen3-coder:free"
WARN openrouter suppressed cache invalidation: systemic 404 (data-policy toggle?) distinct_failed=5 cached_count=5
```

**Acceptance criteria**:
- Each distinct condition logs **once per model id** (unranked-model
  warning) or **once per suppression event** (systemic-404 warning), not
  once per request — a busy pool doesn't spam the log for a static
  condition (plan.md Task 4.2.1c).
- Both are `WARN`, not `ERROR` — signaling "notice, no action required
  yet" rather than "something broke," consistent with their fail-soft
  status (see error-state analysis below).
- Message text names the specific model/counts, so the log line alone
  (no source read) tells Tyler what to do next: add the model to
  `BENCH_TABLE`, or ignore the systemic-404 case (it self-resolves, it's
  the account-wide privacy toggle, not real staleness).

---

## 6/7. Client-visible error states — fail-closed vs. fail-soft

This is the surface `research/ux.md` §4 flags as the core finding to
verify: **pool exhaustion must fail closed and be client-visible; a
stale/unranked bench-table entry must fail soft and never reach the
client.** I traced both paths in the actual plan and current code
(`src/entrypoint/errors.rs`, `src/providers/mod.rs`, plan.md Epic 4.1–4.2)
to confirm the design holds.

### 6. Pool exhausted (fail closed, client-visible)

When every free-model candidate is cooling down/rate-limited,
`Router::dispatch` returns `Err(ProviderError::Exhausted)` (plan.md
`src/routing/router.rs:368`, reusing the existing variant — no new error
type). The client sees:

```json
// Anthropic-shaped response, HTTP 529
{
  "type": "error",
  "error": {
    "type": "overloaded_error",
    "message": "all upstream candidates exhausted"
  }
}
```
```json
// OpenAI-shaped response, HTTP 503
{
  "error": {
    "message": "all upstream candidates exhausted",
    "type": "server_error",
    "param": null,
    "code": null
  }
}
```

**Acceptance criteria**:
- The client (Claude Code or any Anthropic-API caller) gets a clean,
  named failure — `"all upstream candidates exhausted"` — not a raw
  passthrough of the last candidate's 429 body. Tyler doesn't have to
  guess from a stack trace what happened (`research/ux.md` §4).
- **Caveat found during verification, not previously called out in
  research/plan**: `ProviderError::Exhausted` and `ProviderError::Timeout`
  currently share the same Anthropic-shaped `(529, "overloaded_error")`
  pair (`src/entrypoint/errors.rs:38-53`) and differ only in `message`
  text (`"request timed out"` vs. `"all upstream candidates exhausted"`).
  They remain **distinguishable by reading the message field**, but not by
  the `type` field or status code alone — an operator (or client-side
  code) branching only on HTTP status/`type` cannot tell them apart. This
  is pre-existing behavior this feature reuses verbatim (Out of Scope:
  "no change to non-OpenRouter providers... or to error path"), so it's
  not a regression to fix here, but it's worth Tyler knowing: **if he ever
  wants to alert/branch on exhaustion specifically, he must match on the
  message string, not the error `type`.**
- This event also increments the same per-upstream `last_error_kind`
  attribution the dashboard already shows (`kind_label() == "exhausted"`,
  `src/providers/mod.rs:133`), so a spike in exhaustion is visible in
  `recent_errors`/upstream counters at a glance, not just in the 503/529
  response body — confirms `research/ux.md`'s requirement that this
  failure mode also shows up in aggregate, not only per-request.
- **No dead end**: the message plus the dashboard's `openrouter_scoring`
  cache/model block together tell Tyler exactly what to check next (are
  all models cooling down? is the cache stale/empty?) — he is not left
  with a bare "exhausted" and no lead.

### 7. Stale bench-table entry / unranked model (fail soft, never client-visible)

When a discovered free model has no `BENCH_TABLE` entry, `bench_score()`
returns `None` and the strategy substitutes the neutral default `0.5`
(plan.md Story 4.2.1, Task 4.2.1c) — the model stays in the candidate
pool and can still be selected. There is **no `ProviderError` variant for
this at all**: it never reaches `Router::dispatch`'s error path, so it
cannot produce a client-visible error body by construction, not just by
convention. The only visible trace is:
- the one-time `WARN` log line (surface #5), and
- `bench_rank: null` in the `openrouter_scoring.models` block (surface
  #2) for that model, persisting for as long as it's unranked (not just
  at the moment of first discovery).

**Acceptance criteria**:
- **A client request that happens to land on an unranked model succeeds
  or fails only for reasons unrelated to its bench-table status** — the
  missing bench entry itself never causes a 4xx/5xx. Verified by walking
  the type: `bench_score: Option<f64>` is consumed only inside the
  scoring formula (Task 4.2.1a), never inside `send()`'s error
  classification (`src/providers/openrouter/mod.rs`) — there is no code
  path from "unranked" to `ProviderError`.
- **Distinguishable from exhaustion at a glance**: exhaustion produces a
  client-visible 503/529 *and* a dashboard error-kind spike; an unranked
  model produces *only* a dashboard `bench_rank: null` + one log line,
  with a normal 200 response. An operator seeing a client error can rule
  out "just an unranked model" immediately, because that condition never
  produces one.
- Similarly, the data-policy-vs-staleness cache logic (plan.md Story
  2.1.3) never surfaces a client error either: a single stale/404'ing
  model is dropped from the pool and retried against a different
  candidate in the same dispatch loop (via the widened `already_tried`,
  Story 3.1.2) rather than failing the request — the request only fails
  with `Exhausted` if *every* candidate is unavailable, which is the
  correct, already-designed boundary between "one model is stale" (soft)
  and "the whole pool is dry" (hard).

---

## UX acceptance criteria (consolidated)

1. **Model attribution**: operator can determine which model served a
   given request in ≤5 seconds from the dashboard's `recent_requests`
   row (`selected_model` field) — no log correlation required.
2. **Score auditability**: operator can see, for every currently-cached
   free model (not just the last pick), its latency/error-rate/bench-rank
   inputs and composite score via `GET /metrics`'s `openrouter_scoring.models`
   block, within one page load / one `curl`.
3. **Decision auditability**: operator can answer "why was model X picked
   for request Y" from the `DEBUG` selection log line alone, without
   re-running the request or attaching a debugger.
4. **Fail-closed error is client-visible and named**: pool exhaustion
   returns a 529/503 whose message explicitly says
   `"all upstream candidates exhausted"` — distinguishable from a generic
   upstream error by message text (see caveat above re: shared `type`
   field with `Timeout`).
5. **Fail-soft is never client-visible**: an unranked model or a single
   stale bench-table/model-list entry never produces a client-facing
   error — confirmed by code-path tracing above, not just by
   design intent.
6. **The two failure modes are distinguishable within 5 seconds of
   looking**: exhaustion shows a client error *and* a dashboard
   error-kind spike; staleness/unranked shows *only* a dashboard
   `bench_rank: null` / cache-state note and a one-time log line, with
   requests otherwise succeeding normally.
7. **No dead ends**: every error/config-mistake surface above (unknown
   config field, missing openrouter upstream on a scored route, pool
   exhaustion) states specifically what's wrong (field name / route name
   / "exhausted") such that the next action is inferable from the message
   text alone, without reading source.
8. **Zero behavior change for non-opted-in config**: every new field
   (`selected_model`, `openrouter_scoring`) is additive/omit-when-absent,
   and every existing `conf.d/*.toml` continues to parse and dispatch
   identically — verified against plan.md's Migration Plan ("no existing
   valid config becomes invalid").
9. **Accessibility**: not applicable — no interactive/visual surface is
   introduced (see Scope note above).
