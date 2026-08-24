# UX Research: context-analyzer

Agent 5, SDD Phase 2. Sources: `manavgup/context-analyzer` README
(https://github.com/manavgup/context-analyzer, fetched via
`gh api repos/manavgup/context-analyzer/readme`) and consolette's existing
dashboards — `src/cost_metrics/dashboard.html`, `src/dashboard.rs`,
`src/cost_metrics/server.rs`.

## 1. Comparable UX patterns worth borrowing

**From the reference implementation (manavgup/context-analyzer README,
"Dashboard Features" section):**

- **Budget-threshold toggle buttons (200K/500K/700K/1M) on the growth chart**,
  rendered as a dashed red budget line + orange dotted autocompact line +
  red-shaded danger band above budget. This is the single most
  requirements-critical pattern (Success Metric #2: "identify the
  turn/session where context crossed a chosen budget threshold"). Borrow it
  directly — threshold-as-toggle-button rather than a dropdown/settings
  field keeps the common case (a handful of standard budgets) one click
  away, matching how profiler UIs expose common filters as buttons, not
  forms.
- **Scrubber + playback on "top growth turns"**: lets Tyler step turn-by-turn
  through the session and watch the composition donut/breakdown update in
  sync, rather than only reading a static chart. This is the same
  interaction as a DevTools Performance-panel timeline scrubber or a video
  player's frame-step — familiar, cheap to implement (just an index into
  the turns array driving whichever chart/table is "current"), and it's the
  natural answer to Success Metric #1 (top cost contributor *per turn*).
- **Session dropdown that swaps data without a full page reload** (single-page
  dashboard, not one URL per session) — matches Datadog/Honeycomb trace
  views where switching traces doesn't leave the panel layout. Cheaper than
  a page nav and preserves scroll/zoom state on the chart.
- **Composition donut colocated with a numeric breakdown table**, not the donut
  alone. APM tools learned this the hard way: a pie/donut answers "roughly
  what proportion" at a glance but is bad at precise comparison — always
  pair a chart affordance with the exact numbers next to or on hover, never
  chart-only.
- **`/sessions` cross-session view as a separate route/page** from the
  single-session view, linked by clicking a session in a table
  (drill-down), not by URL param convention alone. Consolette already has
  exactly this shape at `src/cost_metrics/server.rs:197-198` (`/dashboard`
  vs `/v1/dashboard/sessions`) — reuse that split rather than cramming
  cross-session analytics into the same page as the message inspector.

**General precedent (profilers/APM/DevTools), applied to this feature:**

- **Flamegraph convention — area/width encodes magnitude, not just color.**
  The Tool I/O vs Conversation vs System breakdown should be stacked-bar or
  treemap-capable at the per-turn level (not donut-only), so a long session
  with many turns can be scanned as one shape and an expensive turn jumps
  out by width, the way a hot function jumps out in a flamegraph. The
  reference's per-turn donut update-on-scrub is fine for one turn at a
  time; a stacked-area "context growth" chart across the whole session
  (which the reference already has) is what gives the flamegraph-style
  overview.
- **Chrome DevTools Performance panel's "select a range, everything below
  filters to that range" pattern.** Selecting a turn range on the growth
  chart should filter the composition breakdown and message inspector to
  that range — avoids forcing Tyler to scroll a long message list to find
  the turn he already located on the chart.
- **Honeycomb/Datadog's "click a data point to pivot to the underlying
  record"** — clicking a point on the cost/call vs peak-context scatter
  plot (cross-session view) should jump straight to that session's
  single-session dashboard, pre-scrolled/scrubbed to the offending turn if
  feasible. This is the click-through consolette's own table already lacks
  (`dashboard.html`'s comparison table is read-only, no row click) — worth
  adding here since "click any session to drill into its single-session
  view" is explicitly called out in the reference README's Dashboard
  Features list.

## 2. User mental model — vocabulary that will feel native vs. foreign

Tyler built consolette's own compaction pipeline, so he already has fixed
meanings for these terms from the codebase itself — the dashboard should
reuse them verbatim, not invent synonyms:

- **"session"** — already consolette's unit of a transcript
  (`SessionCostState` in `src/cost_metrics/store.rs:90`, keyed by
  `session_key` in the existing `/v1/cost/{session_key}` route). Reuse
  directly; do not introduce "conversation" or "run" as an alternate term
  for the same thing.
- **"turn"** — the reference's per-call/per-turn distinction maps onto a
  concept consolette doesn't currently expose as a first-class type
  (`store.rs` tracks `CostRecord`/`TierTotals` per session, not per turn).
  This is new vocabulary for the *data model*, but not for Tyler's *mental
  model* — he already thinks in turns when reasoning about a Claude Code
  session interactively. Introduce it as a dashboard-level concept without
  worrying that it clashes with anything already named differently in
  consolette.
- **"compaction"** — already a first-class concept
  (`CompactionTier`/`tier_index` in `src/cost_metrics/store.rs:36`, and the
  whole `dashboard.html` table is literally a "native vs. consolette
  compaction" comparison). The context-analyzer feature's autocompact
  threshold line and any compaction-event markers on the growth chart
  should use the *same* tier vocabulary consolette's compaction dashboard
  already uses ("native" = Claude Code's own auto-compact, distinguished
  from anything consolette-driven) rather than inventing a new label like
  "auto-summarize" — a second name for the same event is exactly the kind
  of relearning this question is asking to avoid.
- **"budget"** as a threshold concept (200K/500K/700K/1M) is new vocabulary
  but maps cleanly onto something Tyler already reasons about implicitly
  (context window ceilings) — safe to adopt as-is from the reference.
- **Token composition categories — "Tool I/O / Conversation / System"** — these
  are the reference tool's categories, not consolette's. Consolette's own
  cost/compaction code doesn't currently have an equivalent 3-way split
  (`store.rs` totals are by `CompactionTier`, not by content-source). Two
  options: (a) adopt the reference's 3-way split as-is since it's
  externally legible and matches how Claude Code transcripts are actually
  structured (tool_use/tool_result blocks vs. text vs. system prompt), or
  (b) fold it into consolette's existing tier vocabulary. Recommend (a) —
  composition-by-source and compaction-tier are orthogonal axes (a tool
  result can be pre- or post-compaction), so conflating them would be the
  "relearning a new tool" failure mode, not avoiding it.
- **Cache-read churn** — new to consolette's dashboard vocabulary but not to
  Tyler's mental model; cache read/write token counts are Anthropic API
  primitives consolette already ingests for cost tracking
  (`TokenCount` in `src/cost_metrics/types.rs:40`). Label the chart with the
  literal API field names (cache_read / cache_creation) rather than an
  abstracted "churn score," since Tyler will want to cross-check against
  raw API responses he's already used to reading.

**Net recommendation:** name every dashboard concept after either (a) the
Anthropic API's own token-usage field names, or (b) a term consolette's
codebase already uses (session, compaction, tier) — introduce new
vocabulary only where the reference tool's concept has no consolette
equivalent yet (turn, budget threshold, composition category), and even
then prefer the reference's naming since it's the closest external
precedent and Tyler explicitly wants parity with it.

## 3. Accessibility — minimum bar worth doing cheaply

This is single-user, localhost-only, Tyler-only. Full WCAG conformance
(screen-reader testing, full keyboard-only audit, colorblind simulation
sign-off) is overkill. But a handful of things are near-zero marginal cost
if done from the start and expensive to retrofit once three chart types and
a scrubber exist:

**Worth doing (cheap, and consolette already does most of it):**

- **Never encode Tool I/O / Conversation / System by color alone.** Pair
  each composition category with a distinct pattern or label (hatch fill on
  the stacked bar, or a persistent legend + on-hover numeric label) so it's
  legible even if Tyler's monitor/lighting washes out one of three similar
  hues at a glance. Cheap: this is a chart-config decision made once, not
  ongoing work.
- **Keyboard-operable scrubber.** If the scrubber/playback control is a
  native `<input type="range">` it gets keyboard support (arrow keys) for
  free — use that instead of a custom drag-only div. This is the difference
  between "free" and "a real feature" — pick the native element.
  `dashboard.html`'s sort buttons already establish the pattern of using
  real `<button>` elements with `:focus-visible` outlines
  (`dashboard.html:103-106`) rather than clickable divs — carry that
  forward.
- **Sufficient contrast on budget/danger lines against the chart
  background**, both light and dark. `cost_metrics/dashboard.html` already
  defines a `prefers-color-scheme` dark palette via CSS custom properties
  (`:root` block, `dashboard.html:8-36`); `dashboard.rs`'s Chart.js
  dashboard is hard-coded dark-only (`background: #0a0a0a`,
  `dashboard.rs:26`) with no light variant. For the new dashboard, follow
  `cost_metrics/dashboard.html`'s theme-aware pattern, not `dashboard.rs`'s
  fixed-dark one — Tyler already showed a preference for the theme-aware
  approach in the more recent file.
- **`aria-live` status region for loading/error/empty states** —
  `dashboard.html:129` (`role="status" aria-live="polite"`) already does
  this cheaply for the compaction-comparison table; reuse the same banner
  state-machine pattern (`setBannerState`, `dashboard.html:187-220`) for the
  new dashboard's loading/error/empty states rather than inventing a new
  one.

**Skip (not worth it for a one-user localhost tool):**

- Full screen-reader pass over chart data (e.g., generating an
  accessible data-table fallback for every chart) — the composition
  breakdown table already gives the numeric fallback; don't duplicate it
  for the growth/churn charts too.
- Formal WCAG AA contrast-ratio verification tooling/CI gate — eyeball it
  against both themes once, move on.
- Any multi-user consideration (permissions, user preferences persistence
  across accounts) — genuinely N/A here.

## 4. Error / empty states

Three states this feature specifically needs beyond consolette's existing
`dashboard.html` state machine (`loading` / `fetch-error` / `empty-corpus` /
`empty-after-filter`, `dashboard.html:184-220`):

- **No transcript data yet for a session** (session exists in the
  cross-session table — e.g. because a hook fired — but ingestion hasn't
  parsed any turns/calls yet, or the session is brand new with zero
  calls). Show the single-session dashboard shell with an inline empty
  state in place of the growth chart/composition panel ("No calls recorded
  for this session yet") rather than a blank chart canvas or a full-page
  error — a Chart.js canvas with no data silently renders as an empty box,
  which reads as broken, not empty. Reuse the `empty-corpus` banner pattern
  from `dashboard.html` but scope it to the affected chart region instead
  of the whole page, since the session-picker and page chrome are still
  usable.
- **Partial ingestion failure** (some transcript lines failed to parse —
  e.g. malformed JSONL, a truncated line from a crash mid-write).
  `dashboard.html` already has exactly this pattern for whole *sessions*
  that fail to parse: `state.parseFailureCount`, surfaced as "(N sessions
  failed to parse and are not shown)" appended to the row-count readout
  (`dashboard.html:358-366`), backed by a `parse_failures` array in the
  `/v1/dashboard/sessions` response. Extend the same convention one level
  down: a per-session ingestion-warning banner ("N of M transcript lines
  could not be parsed — data on this page may be incomplete") rather than
  silently dropping the bad lines and letting totals look authoritative
  when they're not. Never let a partial-data session look identical to a
  fully-ingested one in the UI — the whole point of this tool is to trust
  the numbers.
- **Codex CLI and Claude Code sessions coexisting in one table.** The
  reference tool handles this by ingesting Codex sessions "into the same
  dashboard and analytics alongside Claude Code" with the caveat that
  "compaction/subagent depth remains Claude Code-only" (README, "What it
  does"). Recommend: a small source badge/pill in the session table (e.g.
  a colored "CC" / "Codex" tag, matching the badge style
  `dashboard.rs`'s request table already uses for provider labels —
  `provColor`/`error-type` badge classes, `dashboard.rs:454`,
  `dashboard.rs:475-477`) plus graying out or hiding compaction-specific
  columns for Codex rows rather than showing a misleading "0" or "—" that
  reads as "no compaction happened" when the real answer is "not
  applicable / not tracked for this source." Distinguishing by badge (not
  color alone, per §3) also satisfies the composition-category
  color-independence bar for the same reason.

## 5. Jobs-to-be-done

- **Functional job:** "Find what's expensive, at the granularity that lets
  me act on it" — which turn, which call, which content category (tool
  output vs. conversation vs. system) is driving cost or context growth,
  cross-referenced against a budget so Tyler can tell *before* a session
  gets expensive, not just in a post-mortem. The scrubber/threshold-toggle
  UX in §1 is what turns "expensive" from a single aggregate number into
  something actionable turn-by-turn.
- **Emotional job:** "Stop feeling blind about my Claude Code (and Codex)
  costs" — this is explicitly Tyler's framing in the requirements' Problem
  Statement. The dashboard's job is to convert an ambient, low-grade
  anxiety about opaque agent costs into a concrete, checkable fact ("this
  session crossed 500K at turn 40 because of a large tool result") that
  can be acted on or dismissed. This argues for the dashboard defaulting to
  *showing something is fine* as readily as it shows a problem — an
  all-green/no-threshold-crossed session should look calm and
  unremarkable, not like every other chart on the page, so the alarm
  signal (a crossed budget line, a red danger band) actually stands out
  when it matters instead of competing with routine data density.
- **Social job:** none — solo personal tool, no sharing/reporting audience,
  matches the requirements' Constraints section ("Solo, personal use,
  localhost-only, no SLA"). No design effort should go toward
  shareable/exportable views, multi-user framing, or presentation-quality
  polish beyond what Tyler himself needs to read at a glance.
