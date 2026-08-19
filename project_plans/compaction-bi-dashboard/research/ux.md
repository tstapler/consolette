# UX Research: compaction-bi-dashboard

Scope: UX patterns and decisions for the sortable/filterable session-comparison table served by `consolette serve-cost`, per `project_plans/compaction-bi-dashboard/requirements.md`. No code exists yet for this page (`src/cost_metrics/server.rs` currently exposes only `GET /v1/cost/{session_key}` returning JSON — no HTML route). This document is desk research (no external web fetch performed) grounded in the requirements doc and repo conventions; recommendations, not a spec.

## 1. Comparable UX patterns (dependency-light sortable/filterable tables)

Products in this shape (cost-explorer-style tools, log viewers, admin/ops dashboards — e.g. AWS Cost Explorer's tabular view, `htop`/`k9s`-style column sorting translated to web, simple Grafana table panels) converge on a small set of patterns that work without a table library:

- **Click-header-to-sort, three-state cycle**: ascending → descending → (optionally) unsorted, single active sort column at a time. Multi-column sort is a power-user feature most single-operator tools skip — not worth the complexity here (out of scope per the requirements' "no BI framework" constraint).
- **Sort indicator as a glyph + `aria-sort`**, not color alone (color-only fails colorblind users and is the cheapest accessibility miss to avoid — see §3).
- **Filter row is separate from the header row**, not inline in `<th>` — mixing click-to-sort and text-input targets in the same cell creates mis-click hit-testing pain in vanilla JS (no library to manage event delegation cleanly). A dedicated filter row (text input for path substring, `<select>` dropdown for compaction-status) directly below the header is the standard pattern in tools like this.
- **Client-side filter/sort over a single fetched JSON payload** is the right call here, not server-side pagination — 1,135 rows is well within what a browser can hold and re-sort/filter instantly in memory; server-side sort/filter would add API surface for no real benefit at this scale (matches the requirements' single-fetch, no-websocket constraint).
- **Sticky header** (`position: sticky; top: 0`) so column identity survives scrolling through 1,135 rows — pure CSS, zero JS cost, high payoff at this row count.
- **Row-count / filtered-count readout** ("showing 42 of 1,135") near the filter controls — cheap, and is the first thing an operator checks to confirm a filter did something.
- **Numeric right-alignment, text left-alignment** — standard data-table convention, makes magnitude scanning down a column trivial without any JS.
- **Monospace or tabular-nums for numeric columns** (`font-variant-numeric: tabular-nums`) so digits align vertically across rows — a one-line CSS win that meaningfully improves scannability of a cost/token column.

None of this requires DataTables.js/AG-Grid/etc.: sort is `Array.prototype.sort` on the in-memory JSON array plus a re-render of `<tbody>` innerHTML or a diffed re-render; filter is an `Array.prototype.filter` predicate combining the text substring and dropdown state, re-run on `input`/`change` events.

## 2. User mental model

**Default sort order**: Tyler's actual question per the Problem Statement is "is consolette's compaction pulling its weight compared to what Claude Code already does for free?" — the highest-value default sort is **descending by consolette-tokens-saved** (or consolette-dollars-saved), because that surfaces the sessions where consolette's compaction did the most work — evidence for "is this worth running." An alternative worth considering: default sort by **native-vs-consolette delta** (sessions where consolette saved meaningfully more than native, or vice versa) — this more directly answers the comparative question, but is a derived/computed column and slightly less legible than a raw metric on first render. Recommend: default to consolette-tokens-saved descending, with the delta available as a sortable column, not the default sort — simpler mental model to land on first paint, and the delta is one click away.

**Above-the-fold columns** (in priority order, matching the "decide if compaction is worth it" job):
1. Session identifier (path/project substring — needed to locate the file if follow-up is needed)
2. Native compaction: tokens saved, cost saved
3. Consolette compaction: tokens saved, cost saved
4. Compaction status (native / consolette / both / neither) — this is the filter dimension too, so showing it as a column reinforces what the dropdown filter means
5. Chain coverage indicator (full / partial) — must be visible, not buried, per the Rabbit Holes section: partial-chain numbers must never look complete
6. Session size (total tokens or message count) — secondary, useful for sanity-checking whether "0 saved" means "no compaction happened" vs. "tiny session, nothing to compact"

**Tokens vs. dollars vs. "which system did more" — three distinct mental models**:
- **Tokens saved** is the mechanistically primary number — it's what both native and consolette actually report/measure directly (native's `preTokens`/`postTokens`; consolette's own counterfactual estimate). Dollars are a derived, pricing-table-dependent view of the same fact.
- **Dollars saved** is the number Tyler will actually care about when deciding "is this worth the engineering effort" — it converts an abstract token count into the "is this worth it" currency. But per the Feasibility Risks section, native's token counts are Claude Code's own accounting while consolette's use `TiktokenEstimator` — the *same* pricing table converts both to dollars, but the token counts feeding it come from different accounting methods. This must be surfaced (see labeling in §3/§4 below), not silently blended.
- **"Which system did more"** is inherently a *comparison/delta* concept, not a raw metric on either side — it should be its own column (e.g. `consolette_saved − native_saved`, or a qualitative badge like "consolette-only" / "native-only" / "both, consolette larger" / "both, native larger" / "neither") rather than something the user is expected to compute by eyeballing two adjacent columns across a wide table. A derived delta column is cheap to compute client-side once the JSON is fetched and meaningfully reduces cognitive load versus mental subtraction across 1,135 rows.

## 3. Accessibility (proportionate — single-operator local tool, not enterprise-grade)

Cheap wins worth doing (all low-cost, no library needed):
- **`<table>` with real `<th scope="col">`** for headers — screen-reader/keyboard semantics come for free from correct HTML; do not build a sortable "table" out of `<div>`s.
- **`aria-sort="ascending"|"descending"|"none"`** on the currently-sorted `<th>`, updated on click — this is the single highest-value ARIA attribute for this feature and costs one line of JS per sort action.
- **Sort control as a real `<button>` inside the `<th>`**, not a bare `<span>` with a click handler — gives free keyboard operability (Tab + Enter/Space) and a natural focus target, versus reimplementing keydown handling for a non-interactive element.
- **Visible focus outline retained** (do not `outline: none` the sort buttons) — this is the most common accessibility regression in custom dashboards and the cheapest to just not introduce.
- **Sort indicator uses both an icon/glyph and text/aria-sort**, not color alone (see §1) — one extra character (▲/▼) in the button label.
- **Filter inputs get `<label>`s** (can be visually hidden via `.sr-only` if a compact layout is wanted) — screen readers otherwise announce a bare `<input>` with no name.
- **Live region for the filtered-row-count readout** (`aria-live="polite"` on the "showing N of 1,135" text) is a nice-to-have but not essential for a single-operator tool who can see the screen — skip unless trivial to add, since the requirements explicitly call for right-sizing rigor to a local operator tool, not enterprise-grade compliance.

Not worth doing at this scope: full WCAG AA color-contrast audit tooling, screen-reader user testing, RTL support, or a live-region announcement for every sort/filter change (would be noisy for the one person using this).

## 4. Error / edge-case UX

Per the Rabbit Holes and Observability Requirements sections, the aggregation is best-effort and must never silently misrepresent partial data. States to design for:

- **No compaction of either kind** (native or consolette): show explicit "—" or "none" in both metric columns rather than "$0.00" / "0 tokens" — a real zero (compaction ran but saved nothing) and "never compacted" are different facts and must not collapse to the same rendered value. This also matters for the compaction-status filter dropdown ("neither" must be a selectable, meaningful category per the requirements' explicit status enum).
- **Session that failed to parse**: per Observability Requirements, this is skip-and-log server-side, not abort-the-whole-scan. The dashboard should still surface that some sessions were skipped — a small "N sessions failed to parse (see server log)" note near the table, rather than the row silently vanishing with no trace that anything was omitted. This preserves the "don't mistake absence for a clean zero" principle from the Feasibility Risks.
- **Partial chain coverage**: must be visibly flagged per-row (a badge/icon in a "coverage" column, e.g. "partial" vs. "full"), not just present in the raw JSON — the requirements are explicit that partial-chain numbers must never be presented as if complete. A muted-color badge or a "(partial)" suffix directly on the affected row's numbers is cheaper and more visible than a separate legend the user has to cross-reference.
- **In-progress scan** (1,135 files, nonzero time): the Success Metrics only require "doesn't hang the browser tab," not a progress bar. Minimum viable: a loading state ("Scanning session history…") shown while the fetch is in flight, replacing an indefinite blank page. If Phase 2's timing check finds the scan takes long enough to feel stuck (multi-second), a simple indeterminate spinner/text is sufficient — a real progress bar would require streaming/chunked responses, which is out of scope (no websockets/polling per Out of Scope). If a manual-refresh-with-cache design is chosen (per Open Questions), the same loading state applies to the refresh action, with the previous table left visible (not blanked) until the refresh completes, so a slow refresh doesn't look like data loss.
- **Fetch/aggregation error** (e.g. `~/.claude/projects/` unreadable, 500 from the endpoint): show an explicit error message with the raw error text, not a silently empty table — an empty table and "endpoint failed" must look different to the operator, or Tyler will misread "no rows" as "no compaction happened anywhere," which is a wrong and consequential conclusion given the requirements' emphasis on not letting failures masquerade as clean data.
- **Empty result set** (filters exclude everything): distinct "no sessions match your filters" message, separate from the load-error and true-zero-data states above — three visually distinct states (loading / error / empty-after-filter) prevents them from being confused with each other.
- **Estimated vs. exact labeling**: per Feasibility Risks and Observability Requirements, native `preTokens`/`postTokens` are Claude-Code-reported (exact) while consolette's counterfactual is `TiktokenEstimator`-derived (estimated) — mirror the existing `CostReport` pattern (`pricing_source`/`counterfactual_source`) with a small "exact"/"est." marker per relevant cell or column header, so a reader doesn't assume both numbers carry the same precision. This is a correctness-of-perception issue, not just polish — conflating the two is exactly the failure mode called out in Feasibility Risks.

## 5. Jobs-to-be-done

- **Functional job**: decide whether running consolette's own compaction is worth the engineering/maintenance cost, given that Claude Code's native auto-compaction may already capture most of the available savings for free. The table's job is to make that a five-second visual scan (sort by consolette-saved, sort by delta, eyeball how often consolette's compaction actually beats native) rather than a manual per-file `rg` exercise, which is literally today's baseline per the Baseline section.
- **Emotional job**: confidence that the numbers are measured, not guessed — this is why the exact/estimated labeling (§4) and the "skip-and-log, don't silently drop or misrepresent" handling of parse failures and partial chains (§4) matter as much as the sort/filter mechanics. A BI-style table that *looks* authoritative but quietly blends estimated and exact numbers, or silently omits failed sessions, would produce false confidence — worse than the manual-`rg` baseline it replaces, because the manual process at least forces Tyler to look at each row's actual JSON.
- **Social/professional job**: this table is the artifact Tyler (or a future reader of the code/commit history) will point to when justifying a compaction-tuning decision later — "I checked, and consolette's compaction only wins on N% of sessions once native compaction is accounted for" is a defensible, re-derivable claim only if the underlying data's provenance (exact vs. estimated, full vs. partial coverage, which sessions were skipped) is visible in the same view that produced the number, not buried in server logs someone would have to go dig up. This is the same "evidence a reader can check" bar the repo's own CLAUDE.md holds prose claims to, applied to a UI instead of a doc.

## Recommendations summary (non-binding, for Phase 3 planning)

1. Native `<table>`, sticky header, separate filter row below headers, single-column click-to-sort with `aria-sort` + button-based sort controls.
2. Default sort: consolette-tokens-saved descending; provide a computed delta column, sortable, not default.
3. Columns above the fold: session id/path, native saved (tokens/$), consolette saved (tokens/$), status badge, coverage badge, session size.
4. Explicit distinct rendering for: true zero, never-compacted, partial-coverage, parse-failure-count, load-error, empty-after-filter, loading — do not collapse any of these into a blank cell or blank table.
5. Per-cell or per-column exact/estimated marker mirroring `CostReport`'s existing `pricing_source`/`counterfactual_source` convention.
6. Client-side sort/filter over one fetched JSON array — no server-side pagination, no websockets, matching the Out of Scope constraints.
