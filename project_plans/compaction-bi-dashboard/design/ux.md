# UX Design: compaction-bi-dashboard

Grounded in `project_plans/compaction-bi-dashboard/requirements.md`, `research/ux.md`, and
`implementation/plan.md` Epics 3.1–3.3 (exact JSON shape: `SessionComparisonRow` — `session_path`,
`session_id`, `project`, `size_bytes`, `modified_unix_secs`, `status` (`NativeOnly` /
`ConsoletteOnly` / `Both` / `Neither`), `native_event_count`, `native_tokens_saved: Option<i64>`,
`consolette_tokens_saved: Option<i64>`, `net_advantage_tokens: Option<i64>`,
`no_compaction_total_tokens: u64`, `no_compaction_estimated_cost_usd: Option<f64>`,
`chain_coverage_ratio: f64`; response envelope adds `generated_at`, `parse_failure_count`,
`confidence_legend`). Sorting/filtering is entirely client-side over one fetched JSON array — no
server round-trip per interaction (Out of Scope: no server-side pagination, no websockets).

## Step 1: Surfaces and states

One real interactive surface: **`GET /dashboard`**, the session comparison table. No auth, no
multi-page flow, no modals. States within that one surface:

| State | Trigger |
|---|---|
| Loading | Page just loaded, `fetch('/v1/dashboard/sessions')` in flight |
| Populated | Fetch succeeded, ≥1 row, no active filter narrowing it to zero |
| Fetch/aggregation error | Fetch rejected or non-2xx response |
| Empty (zero sessions ever) | Fetch succeeded, `rows.length === 0`, no filter applied |
| Empty-after-filter | Fetch succeeded with rows, but the active text/status filter matches none |
| Populated with partial data | Fetch succeeded, `parse_failure_count > 0` and/or some rows have `chain_coverage_ratio < 1.0` |

That is 6 states on 1 surface — sortable/filterable interaction is layered on top of the
"Populated" state, not a separate state of its own.

## Step 2: Wireframe

```
┌──────────────────────────────────────────────────────────────────────────────────────────────────┐
│ Consolette — Compaction Cost Dashboard                                                             │
│                                                                                                      │
│  #status-banner  (hidden when populated; shown for loading/error/empty/empty-after-filter states)  │
│  ┌────────────────────────────────────────────────────────────────────────────────────────────┐  │
│  │ ⏳ Scanning session history…                                                                  │  │
│  └────────────────────────────────────────────────────────────────────────────────────────────┘  │
│                                                                                                      │
│  Filter path/project: [________________________]   Status: [ All ▾ ]                               │
│                                                          ├ All                                      │
│                                                          ├ Native only                              │
│                                                          ├ Consolette only                          │
│                                                          ├ Both                                     │
│                                                          └ Neither                                  │
│                                                                                                      │
│  Showing 1,088 of 1,135 sessions   ·   ⚠ 12 sessions failed to parse (see server log)              │
│  ══════════════════════════════════════ sticky ═══════════════════════════════════════════════    │
│ ┌────────┬─────────┬────────┬───────────┬────────────┬────────────┬────────────┬─────────────┐   │
│ │Session▲│ Project │ Status │Native tok. │Consolette  │Net         │No-compact.  │Chain        │   │
│ │[button]│[button] │[button]│saved[btn]  │tok. saved  │advantage   │cost[btn]    │coverage[btn]│   │
│ │        │         │        │(exact)     │[btn](est.) │[btn]       │(est.)       │             │   │
│ ├────────┼─────────┼────────┼───────────┼────────────┼────────────┼────────────┼─────────────┤   │
│ │abc123  │my-repo  │ Both   │   7,800    │   3,200    │   -4,600   │   $0.42     │ full        │   │
│ │def456  │my-repo  │ NativeO│   2,100    │      —     │      —     │   $0.11     │ partial ⚠   │   │
│ │ghi789  │other    │ Neither│      —     │      —     │      —     │   $0.00     │ full        │   │
│ │jkl012  │other    │ Cnsl.O │      —     │   9,400    │   9,400    │   $1.03     │ full        │   │
│ │  ...   │         │        │            │            │            │             │             │   │
│ └────────┴─────────┴────────┴───────────┴────────────┴────────────┴────────────┴─────────────┘   │
│  (session cell links out to raw .jsonl path on click / shows full path as title/tooltip)           │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
```

Column-header key (`aria-sort` lives on each `<button>`'s parent `<th>`):
- **Session** — `session_id`, text sort (`Intl.Collator`); title attribute shows full `session_path`
- **Project** — `project`, text sort
- **Status** — `status` enum, sorts by a fixed rank order (`Both` > `NativeOnly` > `ConsoletteOnly` > `Neither`, or alphabetical — either is defensible; pick alphabetical for predictability)
- **Native tokens saved** — `native_tokens_saved` (nullable numeric), header shows `(exact)` — native-reported
- **Consolette tokens saved** — `consolette_tokens_saved` (nullable numeric), header shows `(est.)`
- **Net advantage** — `net_advantage_tokens` (nullable numeric; only populated when both sides are `Some`) — default sort column, descending
- **No-compaction cost** — `no_compaction_estimated_cost_usd` (nullable numeric), header shows `(est.)`
- **Chain coverage** — `chain_coverage_ratio` (0.0–1.0), rendered as `full` (ratio == 1.0) or `partial (NN%)` with a `⚠` glyph, never color alone

Rendered-value rules for nullable/zero-ish cells (this is the crux of the "don't collapse states"
requirement):
- `native_tokens_saved: None` (native never ran) → render `—` (em dash), not `0`
- `native_tokens_saved: Some(0)` (native ran, saved nothing) → render `0`, distinct from `—`
- Same rule applies to `consolette_tokens_saved` and `net_advantage_tokens`
- `chain_coverage_ratio < 1.0` → `partial (NN%) ⚠` text, never a bare colored dot

## Step 3: Interaction flow

1. **Operator navigates to `http://127.0.0.1:<port>/dashboard`.**
   System: page shell renders instantly (static HTML, `include_str!`-inlined, no network wait for
   the shell itself); `#status-banner` shows "Scanning session history…"; table `<tbody>` is empty.
2. **Fetch resolves.**
   - Success, rows present → banner hidden, `render(rows)` populates `<tbody>` sorted by
     **Net advantage descending** by default (JSON is returned in server-computed order but the
     dashboard establishes its own default sort on first render — sortable rather than fixed,
     but net-advantage-descending most directly answers "is consolette pulling its weight," per
     `research/ux.md` §2's recommendation adapted to name the delta column, not the raw
     consolette-saved column, as default; both are one click apart regardless).
   - Success, zero rows → banner shows "No sessions found under `~/.claude/projects/`." table
     stays empty, no header/filter row hidden (they remain visible so the operator can see the
     page loaded correctly, just found nothing).
   - Failure (network/HTTP error) → banner shows `Failed to load session data: <raw error text>`,
     table stays empty, filter controls disabled (nothing to filter yet).
3. **Operator clicks a column header button (e.g., "Chain coverage").**
   System: in-memory `rows` array re-sorted (ascending on first click for that column); that
   header's `aria-sort` set to `ascending`, all other headers reset to `none`; glyph (▲) shown in
   the button label; `<tbody>` re-rendered via `replaceChildren()`. No network call.
4. **Operator clicks the same header again.**
   System: toggles to `descending` (▼), re-sorts, re-renders. A third click could reset to `none`
   (original fetch order) — optional; not required by the plan, and skipping it keeps the JS
   simpler without harming usability (one un-sort is one refresh away).
5. **Operator types into the path/project text filter.**
   System: on every `input` event, re-runs `Array.prototype.filter` combining the text substring
   (case-insensitive match against `session_path`/`project`) and the current status-dropdown
   value; re-renders `<tbody>`; updates the "Showing N of M" readout. No network call.
6. **Operator picks a value from the Status dropdown (`All` / `Native only` / `Consolette only` /
   `Both` / `Neither`).**
   System: same filter re-run as above, combined with any active text filter (AND, not OR)
   — e.g., typing `my-repo` + selecting `Native only` shows only `my-repo`-path sessions with
   `status === "NativeOnly"`. Sort order chosen in step 3/4 is preserved across filter changes.
7. **Filter narrows the result to zero rows.**
   System: `<tbody>` is empty, banner shows "No sessions match your filters." with the filter
   controls still visible and populated with the user's current input — the way back is simply
   clearing the text field or resetting the dropdown to `All`, both one interaction each.
8. **Operator hovers/clicks the Session cell.**
   System: cell has a `title` attribute with the full `session_path` (visible on hover) so the
   operator can locate the file without a click; per Epic 3.2.2b, cell content is inserted via
   `.textContent`, so a hostile/odd `project`/`session_path` string (e.g. containing HTML-like
   characters from an unusual directory name) renders as literal text, never executes. No live
   "click to open" affordance is required by the plan — `file://` links from a served page are
   unreliable across browsers/OSes and out of scope; the tooltip already answers "where is this
   file."

## Step 4: Error / edge-case table

| Case | Distinguishing signal shown | Way back to a normal view |
|---|---|---|
| Loading | `#status-banner`: "Scanning session history…"; table body empty | N/A — resolves automatically on fetch completion |
| Fetch/aggregation error | `#status-banner`: "Failed to load session data: `<raw error text>`"; table body empty; filter inputs disabled | Reload the page (no in-page retry button required by the plan; simplest correct affordance for a local single-operator tool) |
| Empty result (zero sessions ever) | `#status-banner`: "No sessions found under `~/.claude/projects/`."; filter row still visible but inert (nothing to filter) | N/A — reflects true state; reload after adding sessions |
| Empty-after-filter | `#status-banner`: "No sessions match your filters."; filter controls remain visible with current values intact | Clear the text input and/or reset Status dropdown to `All` — both restore the populated view in one interaction each |
| Some sessions failed to parse | Persistent note near the row-count readout: "⚠ N sessions failed to parse (see server log)" — shown whenever `parse_failure_count > 0`, independent of loading/error/empty state | Not dismissible/actionable in-page (server-log detail is out of scope for the dashboard itself); it coexists with a populated table, it does not block it |
| Partial chain coverage on a row | `partial (NN%) ⚠` in that row's Chain coverage cell (text + glyph, not color alone); the row's other numeric cells remain visible but the coverage flag signals "these numbers may be incomplete" | N/A — informational per-row flag, not a blocking state |
| True-zero savings vs. never-compacted | `native_tokens_saved`/`consolette_tokens_saved` render `0` when `Some(0)` (compaction ran, saved nothing) vs. `—` when `None` (that compaction path never ran on this session) — same rule applied consistently across both metric columns and `net_advantage_tokens` | N/A — both are terminal, correct renderings of distinct facts |

## Step 5: UX acceptance criteria

**Sort / filter mechanics**
1. User can sort the visible table by any of the 8 columns in exactly 1 click (first click = ascending, per column).
2. A second click on the same header toggles to descending in 1 click (2 clicks total for descending from an unsorted table).
3. Only one column shows an active `aria-sort` value at a time; clicking a different header's button resets the previously active header's `aria-sort` to `none` within the same interaction (no stale double-sorted indicator).
4. User can filter to only natively-compacted sessions (`status === NativeOnly`) in ≤2 interactions: (1) open the Status dropdown, (2) select "Native only." No text-input step required for this specific filter.
5. Combining the text filter and the Status dropdown narrows results by logical AND, verified by: filtering text to a known project substring, then selecting a status value, and confirming the row count only decreases or stays equal — never increases — relative to either filter alone.
6. The "Showing N of M" readout updates within the same render pass as any sort or filter action — never stale by one interaction.

**States have no dead ends**
7. Every one of the 6 states in Step 1 renders text distinguishable from every other state — verified by reading the literal banner/table text for each state side by side and confirming no two are identical or ambiguous (e.g., "no sessions found" vs. "no sessions match your filters" must not be the same string).
8. From empty-after-filter, a human can return to the full populated table in ≤2 actions (clear text input; or reset dropdown to "All"; combined worst case, both).
9. From a fetch error, the raw error text is visible in the banner (not just a generic "something went wrong") so the operator can distinguish "server down" from "directory unreadable" from "malformed JSON" without opening devtools.
10. A row's `native_tokens_saved`/`consolette_tokens_saved`/`net_advantage_tokens` never render as `0` and `—` inconsistently for the same underlying `None`/`Some(0)` distinction — spot-check at least one of each across the rendered table.

**Accessibility**
11. Every column-header sort control is a real `<button>` reachable via Tab, in DOM/visual order matching the column order, and activatable via both Enter and Space.
12. `aria-sort` is present on every sortable `<th>` at all times (`none` when inactive), and its value updates synchronously with the click that changes sort — verified via a screen reader or the accessibility tree inspector announcing the new sort direction after a click.
13. Both filter inputs (text field, status `<select>`) have an associated `<label>` (visually hidden is acceptable) — verified by confirming the accessible name of each control is non-empty and describes its purpose ("Filter by session or project path", "Filter by compaction status").
14. No `outline: none`/`outline: 0` is applied to the sort buttons or filter inputs anywhere in the page's CSS — Tab-focus is visibly indicated by the browser's default or an equivalent custom focus ring at all times.
15. Sort direction and chain-coverage partial/full status are each conveyed by text or a glyph (▲/▼, "partial"/"full", ⚠) in addition to any color — verified by viewing the page in grayscale/high-contrast mode and confirming both remain distinguishable.
16. Body text and column headers meet WCAG AA contrast (≥4.5:1) against their background in both the default and any dark-mode styling shipped — verified with a contrast-checker tool against the actual CSS color values.
17. Transcript-derived strings (`session_path`, `project`) are inserted via `.textContent`/`createElement`, never `.innerHTML` or template-string HTML concatenation — verified by code inspection of `dashboard.html`'s `render()` function and by the Epic 3.2.2 Given-When-Then (hostile `project` string renders as literal text, no script executes).

## Summary

- **Surfaces designed**: 1 (`GET /dashboard`).
- **States designed**: 6 (loading, populated, fetch/aggregation error, empty-zero-sessions, empty-after-filter, populated-with-partial-data flags).
- **UX acceptance criteria written**: 17 (6 sort/filter mechanics, 4 no-dead-end/state-distinction, 7 accessibility).
