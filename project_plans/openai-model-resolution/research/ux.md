# Research: UX (operator-facing observability for model resolution)

Scope: this feature has no end-user UI. The only real UX surface is the
operator glancing at `src/dashboard.rs` or polling `/metrics`
(`src/metrics/counters.rs`) who needs to answer, without grepping logs:
"is resolution healthy, what did it pick, and did it just run out of
candidates?"

## 1. Comparable patterns (feature-flag dashboards, deploy tools, LiteLLM)

- **LiteLLM's admin/proxy UI** surfaces fallback as a *counter + reason*
  pair on the model list ("fallback used: N times", with the triggering
  error class), not a live play-by-play. It does not show a per-request
  waterfall of which candidate was tried; it shows the *steady-state
  outcome* (which model is currently serving traffic for an alias) plus a
  cumulative fallback count operators check when latency/cost looks off.
  The lesson: a fallback happening once in a while is normal and shouldn't
  demand attention; only a *sustained* pattern (rising fallback rate, or
  zero healthy candidates) should visually escalate.
- **Feature-flag dashboards (LaunchDarkly-style) and deploy tools
  (Argo Rollouts, Spinnaker canary)** universally separate two states that
  are easy to conflate: "the automated system took an action" (informational,
  low-noise — a badge/tooltip, not a color change) vs. "the automated system
  has no good option left" (alerting — a distinct color, ideally red, and
  it must stay visible until acknowledged/resolved, not auto-clear on the
  next poll). Argo Rollouts' canary status specifically distinguishes
  "Progressing" (normal, expected transient state) from "Degraded" (needs a
  human) with different icons/colors — never the same visual weight.
- **Anti-pattern to avoid (alert fatigue)**: don't fire a dashboard-red
  state or a counter increment styled as an error on every single fallback
  hop within a healthy family — that's expected behavior for an opt-in
  resolving upstream and, if styled as an error, teaches the operator to
  ignore the color. Reserve the loud state for "family exhausted" (zero
  working candidates), matching this repo's own existing convention (see
  §4): `errors_total`/`err_*` counters and the red `status-auth-required`
  class are reserved for things a human must act on, while cooldowns
  (`status-cooldown`, amber) are self-recovering and don't page anyone.

## 2. Operator mental model: "why did my request just go to gpt-5 instead of gpt-5.3-codex?"

An operator's actual question decomposes into three, in this order of
urgency:
1. **"What is currently being used for this upstream, right now?"** —
   this needs to be answered at a glance, no click, same place they already
   check upstream health (the status bar at the top of the dashboard).
2. **"Is that a demotion (fallback happened) or the top pick?"** — one
   more piece of at-a-glance state: current candidate vs. best-known
   candidate for the family. If they're the same, no drama. If different,
   that's the signal worth a distinct visual treatment (not necessarily
   alarming, but distinguishable — e.g. a small "fallback active" tag).
3. **"Why — what failed, and when?"** — this is the one thing that's fine
   to push one click deep (a tooltip, an expandable row, or the existing
   `/metrics` JSON), because it's diagnostic detail, not a glance-level
   fact. It should include: which candidate(s) were tried and rejected,
   the classified failure reason (deprecated / wrong-endpoint /
   transient — see Rabbit Holes in requirements.md), and last-attempt
   timestamp. This maps directly onto the per-resolution-attempt counter
   already planned in Observability Requirements — the dashboard just
   needs to render the *latest* attempt per family/upstream, not a full
   history (a full history belongs in `/metrics` JSON or logs, not the
   glanceable card).

Net: operators expect the answer to "why X instead of Y" to live exactly
one level below where they'd notice something changed — not buried in
`journalctl`/log lines they have to correlate by timestamp.

## 3. Error/state taxonomy for the dashboard

Three states worth distinguishing, mapped to visual weight:

| State | Meaning | Suggested treatment |
|---|---|---|
| **Healthy — resolved to newest** | Cached choice is the newest available candidate in the family. | No special indicator; same as any other healthy upstream (green `status-active`). |
| **Healthy — resolved to fallback** | Cached choice is *not* the newest candidate (a newer one exists but is unreachable/deprecated), but requests are succeeding. | Distinguishable but calm — a neutral/informational badge (e.g. small amber dot or a "using fallback: gpt-5" label), analogous to `status-cooldown`'s amber, *not* red. This is "working as designed," and requirements.md explicitly says this must not look like a failure — it's the fix working, not the fix failing. |
| **Exhausted — no working candidate** | Every candidate in the family failed (all deprecated/unreachable/wrong-endpoint). This is the "silent failure" risk requirements.md flags directly (Observability Requirements: "the 'all fallbacks dead' signal an operator needs to notice before assuming the feature 'just works'"). | Loud and sticky — reuse the red `status-auth-required`-style treatment (a new class, e.g. `status-resolution-exhausted`), and it must **not** self-clear until a candidate actually starts working again (same self-healing rule already documented for `last_error_kind` at [src/metrics/counters.rs:24-30](https://github.com/tstapler/consolette/blob/main/src/metrics/counters.rs#L24-L30) — clear on success, never on a blind timer). Requests through that upstream are presumably now failing outright, so this state should coincide with the upstream's own error counters climbing — the dashboard shouldn't need a separate "is anything wrong" heuristic, just a specific-enough label so the operator doesn't have to guess between "auth broke" and "every model in this family died."|

Distinguishing "exhausted" from "healthy but not on the newest model" is the
single most important UX decision in this feature — collapsing them into one
color (e.g. any non-default pick shown as amber) either causes alert fatigue
(if amber also fires for the common, harmless fallback case) or hides a real
outage (if exhaustion looks the same as a harmless fallback).

## 4. Existing dashboard pattern to extend (do not invent a new one)

There is **no existing "family card"** in `src/dashboard.rs` today —
`src/routing/capability.rs` and the auto-model-family feature
(`project_plans/auto-model-family/requirements.md`, "Dashboard shows the
live pick and why") appear to have shipped the routing/capability-eval
logic (see recent commit `92f5d67 fix(routing): capability eval stays
undecided when no probe answered`) but not yet a dashboard surface for it —
grepping `src/dashboard.rs` for `family`/`capability`/`undecided` returns
nothing. So this project isn't extending a family card; it's extending the
one per-upstream status pattern that *does* already exist and already
solves an analogous problem.

**The pattern to reuse**: the provider status bar built client-side in
`loadMetrics()` at [src/dashboard.rs:358-376](https://github.com/tstapler/consolette/blob/main/src/dashboard.rs#L358-L376),
driven by `ProxyMetrics::last_error_kind` (a per-upstream, self-healing,
typed classification set via `set_last_error_kind` —
[src/metrics/counters.rs:206-215](https://github.com/tstapler/consolette/blob/main/src/metrics/counters.rs#L206-L215),
never guessed from error text per the doc comment at
[src/metrics/counters.rs:24-30](https://github.com/tstapler/consolette/blob/main/src/metrics/counters.rs#L24-L30)).
Today it renders one of four CSS classes per upstream — `status-active`
(green), `status-cooldown` (amber, self-recovering, shows remaining
seconds), `status-auth-required` (red, "needs re-auth"), `status-schema-drift`
(purple, "code fix needed") — each with a short human-readable suffix
appended to the upstream's display name
([src/dashboard.rs:364-375](https://github.com/tstapler/consolette/blob/main/src/dashboard.rs#L364-L375)).
This is exactly the "glance → one distinguishable state → optional deeper
detail" shape described in §2 above, and it already encodes the
"informational vs. actionable, self-clearing vs. sticky" split from §3.

**Concrete extension shape** (for planning, not final design): add the
resolved-model state as two more pieces of per-upstream data threaded
through the same `/metrics` JSON `providers[name]` object that already
carries `last_error_kind` — e.g. `resolved_model` (the currently cached
candidate) and a boolean/enum `resolution_state` (`newest` / `fallback` /
`exhausted`) — and extend the existing `cls` ternary in
[src/dashboard.rs:365-368](https://github.com/tstapler/consolette/blob/main/src/dashboard.rs#L365-L368)
with one more branch (`status-resolution-exhausted`, red) and one
low-noise addition to the label suffix for the fallback case (e.g. append
" (using gpt-5, newest unavailable)" the same way cooldown appends its
remaining-seconds suffix at
[src/dashboard.rs:369-372](https://github.com/tstapler/consolette/blob/main/src/dashboard.rs#L369-L372)).
This keeps model-resolution status inside the widget operators already
check for "is my upstream OK," rather than adding a new section they have
to learn to look at — consistent with the Observability Requirements'
framing of this as "consistent with `src/dashboard.rs`'s existing
per-upstream health display," not a net-new UI concept.

The per-resolution-attempt counter itself (success/failure, candidate,
family) belongs in `/metrics` JSON as detail data (§2 point 3) — a compact
rollup (current pick + state enum) is what the dashboard status bar should
render; the full attempt history is for `/metrics` or logs, not the glance
surface.
