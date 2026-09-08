# UX Research: openrouter-routing (Agent 5)

Scope per requirements.md: single-user local proxy, config-driven, existing
metrics/dashboard JSON surface only (`to_metrics_json`) — no new dashboard
page (explicit Out of Scope). This doc informs what shape that JSON/log
surface should take, not a UI design.

## 1. Comparable UX/config patterns

**LiteLLM** (proxy/router in front of many LLM providers, closest analog to
consolette):
- Router-level `fallbacks`/`retry` config is declarative like consolette's
  `FallbackStrategy`, but LiteLLM's "Router" cost/latency-based routing
  (`routing_strategy: "latency-based-routing"` / `"cost-based-routing"`)
  exposes *no* per-decision breakdown in the response by default — the
  chosen model shows up as the model field in the response, and the
  reasoning is visible only via `litellm.set_verbose` debug logs or the
  optional Prometheus/callback metrics (`litellm_deployment_latency`,
  `litellm_deployment_failure_responses` gauges scraped per
  deployment/model-id label). Works well: the metric is a plain
  Prometheus-style gauge keyed by deployment id, so "why was X picked" is
  answerable by diffing gauges across deployments — no special "decision
  log" object needed. Weak point widely reported in GitHub issues: users
  frequently ask "why did it pick model Y" with no direct answer short of
  turning on verbose logging.
- Their `model_list` config with per-model `litellm_params` (weight, RPM,
  TPM) mirrors consolette's `UpstreamRef`/`weight` — the operator edits a
  static list; live health state lives in memory/redis, not config.

**Portkey** (AI gateway, has an explicit "Conditional Routing" / load
balancing UX): its dashboard surfaces a per-request trace showing which
provider/model in a fallback chain was actually hit and why (latency,
error, or explicit condition), rendered as a linear trace list ("Provider 1
failed → Provider 2 succeeded"). This is the most directly transferable
pattern for consolette: a small, ordered "why" trail per request rather
than a live score dashboard. Portkey's virtue is that the trace is
attached to the *request*, not a separate global panel — matches
consolette's existing `RequestDetail`/`recent_requests` pattern already in
`to_metrics_json`.

**OpenRouter's own app** (openrouter.ai/models, the "Auto Router" and
per-model stats pages): shows per-model uptime/latency/throughput publicly
per model, and its own auto-routing (`openrouter/auto`) returns the
actually-used model in the response `model` field and in the
generation-stats endpoint (`/generation?id=...`) — but does not expose the
scoring internals, only the final pick. Operators trust it because (a) the
chosen model is always visible after the fact in the response, not hidden,
and (b) OpenRouter's public per-model status page lets you sanity-check a
suspicious pick against known model health independently. Lesson: minimum
bar is "which model actually served this request must be visible/logged,"
not "how was it scored."

**Common thread across all three**: none of them build a bespoke
"scoring visualizer" UI. They all rely on (a) the response/log always
naming the model actually used, and (b) a metrics endpoint (Prometheus- or
JSON-style) that a technically fluent operator can query ad hoc. None
target a non-technical audience.

## 2. Tyler's mental model / expectations

Given the existing dashboard pattern in this repo (`src/dashboard.rs`,
`src/context_forensics/dashboard.html`, `src/cost_metrics/dashboard.html`)
— all single-page, poll-driven, `stat-card` + chart-container grids reading
from a JSON blob refreshed on an interval — the operating assumption is:
Tyler is comfortable reading structured JSON and a browser dashboard, and
he built this exact pattern three times already for logs/cost/context. He
is not asking for a new visual affordance; he explicitly scoped this
feature to reuse `to_metrics_json` and ruled out a new dashboard page.

What he'd expect for this specific strategy, inferred from the
Observability Requirements section he already wrote himself
(requirements.md lines 96-100, 163-168):
- Per-candidate score **inputs** (rolling latency, rolling error rate,
  bench rank) and the resulting **composite score**, one row per
  candidate model — this is explicit in his own requirements, so the
  minimum bar is already decided: it's not optional "nice to have,"
  it's a stated success metric ("Observability Requirements").
- Model-list cache state (age, last refresh, last invalidation reason) —
  also explicit.
- He did not ask for a live "current pick vs runner-up" comparison UI, a
  historical score-over-time chart, or alerting — those would be
  gold-plating beyond what he scoped. The existing dashboards in this repo
  favor small `stat-card`s and one flat JSON blob over drill-down UIs;
  matching that grain (a `openrouter_candidates: [...]` array of flat
  objects, similar to how `cooldowns` is merged in by the HTTP handler per
  `src/metrics/mod.rs:386-389`) is the pattern-consistent choice, not a new
  visualization idiom.
- Because this is single-user and config-driven, log-level visibility
  (structured log line per dispatch decision, e.g. `tracing::info!(chosen =
  %model, score, latency_ms, error_rate, bench_rank, ...)`) is *also*
  wanted, not instead of the dashboard field — the existing
  `RequestDetail`/per-request logging path is the audit trail he'd reach
  for first when debugging "why did it pick that model" for one specific
  past request, while the dashboard JSON is for "what's the pool's current
  state" at a glance. Both are cheap (a struct field + a json! block) and
  match precedent (`ErrorTracker`/`recent_errors` already logs
  per-error-kind detail per request).

## 3. Accessibility

Not applicable. This is a JSON metrics field consumed by a single
technical operator and an existing internal-only dashboard page (no new
page per Scope); no screen-reader/WCAG/keyboard-navigation surface is
introduced. Any HTML changes needed to render the new fields in the
existing dashboards should keep using the current dashboard's existing
markup conventions (plain divs/text), nothing new to audit here.

## 4. Error states needing graceful handling

Two distinct failure modes, and they must be **visually/structurally
distinguishable** from each other and from ordinary upstream errors,
because the operator (Tyler) needs to tell "expected, by-design fail-closed"
apart from "something is broken and needs a config fix":

- **Free-model pool exhausted (by design, per requirements.md's explicit
  fail-closed decision).** This is not a bug — it's the intended behavior
  when every candidate is cooling down/rate-limited. The existing
  `ProviderError::Exhausted` variant (`src/providers/mod.rs:54-55`,
  `kind_label() == "exhausted"`) already exists and is reused, not a new
  error type — consistent with "no bespoke dispatch path" constraint. UX
  expectation: the error surfaced to the client (Claude Code / whatever
  called the proxy) should be a clean, recognizable failure (e.g. 503 with
  a message naming it as pool exhaustion, not a raw passthrough of the
  last candidate's raw error body) so Tyler doesn't have to guess from a
  stack trace. It should also increment/appear in the same per-upstream
  error-kind attribution the dashboard already shows
  (`UpstreamCounters::last_error_kind`, referenced in
  `src/providers/mod.rs:118`) so a spike in "exhausted" is visible at a
  glance in the existing dashboard rather than requiring a log dive.
- **Config/data error — the bench table references a model OpenRouter no
  longer serves.** This is a *configuration drift* problem, categorically
  different from pool exhaustion: it means the static bench-rank table
  (Scope: "static, checked-in/config-editable default") has gone stale
  relative to OpenRouter's live catalog. Graceful handling: this should
  never hard-fail the route — a bench-table entry for a retired model
  should just mean that model can't be scored/selected (falls out of the
  candidate pool silently, the same way a model-list-fetch simply excludes
  it), while the mismatch itself is logged once (e.g. `tracing::warn!` on
  first sight of an unscored discovered model) and reflected in the
  dashboard's cache/bench-table state block so Tyler can decide to refresh
  the table — not surfaced to the request path as a client-visible error.
  This mirrors the existing model-list cache invalidation design already
  scoped (Scope: "invalidated early on an upstream error signal that
  suggests staleness") — the operator-facing signal is a dashboard/log
  note, not a request failure, because a single unranked model isn't
  supposed to break the whole route.

The key UX principle from both: **fail closed for money-safety
(exhaustion), fail soft for data-staleness (unranked model)** — and make
sure the dashboard's existing error-kind/cache-state fields let Tyler tell
which one happened after the fact without reading source.

## 5. Job-to-be-done

Functional job: **stop manually watching OpenRouter's model list/dashboard
and hand-editing config** every time a free model degrades, gets
deprecated, or hits its rate limit (requirements.md Problem Statement,
verbatim). The job is delegation of a chore he was doing by hand, not
"get better model quality" as an end in itself — the requirements are
explicit that money-safety (never silently paying) trumps optimality.

Emotional job: reduce a background nagging worry ("is the model I hard-
coded three weeks ago still any good, or silently degraded/gone?") without
introducing a new worry ("is the auto-picker secretly costing me money or
silently picking something terrible?"). This is the classic JTBD tension
for any automation feature — Tyler is trading manual control for
automation, and the price of that trade is that he must still be able to
audit the automation cheaply, or the anxiety just moves rather than
disappears.

**Minimum visibility requirement this implies**: for Tyler to actually
stop babysitting (the job succeeds) rather than just add a second thing to
babysit, three things must be true, and all three are already captured in
requirements.md's Observability Requirements — this section just confirms
they're necessary, not just nice-to-have:
1. He can see, per request or in aggregate, *which* model was actually
   used — without this, "automatic routing" is a black box he can't trust
   enough to stop checking manually (matches OpenRouter's own app pattern
   in section 1 — always name the model that served the request).
2. He can see *why*, at least at the level of the three score components
   (latency, error rate, bench rank) already scoped — enough to sanity
   check a surprising pick ("why is it using the worst-ranked model?" →
   "because the better-ranked ones are all cooling down") without needing
   a live debugger.
3. Exhaustion and staleness are distinguishable failure modes (section 4)
   — so when something does go wrong, the first five seconds of looking at
   the dashboard/logs tells him whether it's "working as designed, pool's
   just dry right now" vs. "the bench table needs a refresh," rather than
   both looking like generic errors he has to dig into by hand — which
   would recreate exactly the babysitting he was trying to eliminate.

No additional visibility beyond what requirements.md already scopes is
needed to satisfy the job — a live score-history chart, alerting, or a new
dashboard page would over-serve a single-user tool whose operator already
reads raw JSON and logs comfortably (evidenced by three existing
dashboards built the same minimal way in this repo).
