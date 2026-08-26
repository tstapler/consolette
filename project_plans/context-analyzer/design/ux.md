# UX Design: context-analyzer

SDD Phase 3. Builds on `project_plans/context-analyzer/research/ux.md` (interaction
patterns to borrow, vocabulary, accessibility bar, error/empty states — not
re-derived here) and maps onto the story/task breakdown in
`project_plans/context-analyzer/implementation/plan.md`. Wireframes reuse
`src/cost_metrics/dashboard.html`'s theme-aware CSS-custom-property pattern and
`aria-live` banner state machine (`dashboard.html:8-36`, `:129`, `:187-220`) rather
than `src/dashboard.rs`'s fixed-dark styling, per the research recommendation.

## Surface inventory

| # | Surface | Route | Interactive? | Plan.md stories |
|---|---------|-------|:---:|---|
| 1 | Single-session dashboard | `/dashboard/context?session={id}` | Yes | 1.4.2, 1.4.3, 2.3.2, 5.2.2, 5.3.1 |
| 2 | Cross-session analytics view | `/dashboard/context/sessions` | Yes | 2.1.2, 3.2.2 |
| 3 | Message/turn inspector | embedded panel within Surface 1 | Yes | 2.2.2 |
| — | Cross-dashboard nav links | header of Surfaces 1, 2, and `/dashboard` | Yes (trivial) | 1.4.4b |
| 4 | `consolette context-tracker up` | CLI | No | 4.1.2 |
| 5 | `consolette context-tracker down` | CLI | No | 4.1.3 |
| 6 | `consolette context-hook <event>` | CLI (fire-and-forget, Claude-Code-invoked) | No | 4.2.1 |

Nav links get one line in each surface's flow rather than a standalone
treatment — they're a single `<a>` per direction with one behavior (navigate,
preserving the other dashboard's own state independently).

---

## Surface 1: Single-session dashboard

### Wireframe

```
┌────────────────────────────────────────────────────────────────────────┐
│ consolette · context dashboard          [Session Cost ↗]  [Sessions ↗] │  nav: /dashboard, Surface 2
├────────────────────────────────────────────────────────────────────────┤
│ Session: [ 2026-08-20_14-03_frontend-refactor ▾ ]     source: Claude Code│  picker — swaps data, no reload
│ ⚠ 3 of 214 transcript lines could not be parsed — data may be incomplete│  only if parse_failure_count > 0
├────────────────────────────────────────────────────────────────────────┤
│ CONTEXT GROWTH                                     [200K][500K][700K][1M]│  toggle <button>s, aria-pressed
│  1M ┤                                                     ┈┈┈┈┈┈┈┈┈┈┈┈  │  dashed budget line (on toggle)
│     │                                           ╱▔▔▔▔▔▔▔▔▔▔             │
│ 700K┤▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓ danger band  │  red-shaded, above threshold
│     │                                    ╱                              │
│ 500K┤┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈┈╱                   ⊙native@55 │  dotted autocompact marker
│     │                          ╱‾‾‾‾‾                                   │
│   0 ┤________________________╱_________________________________________│
│     0        10        20        30        40        50      turns    │
│ ▸ View growth as text (peak, threshold crossings, autocompact)          │  <details> — never chart-only
├────────────────────────────────────────────────────────────────────────┤
│ TURN   ◀  [═══════●══════════════════════]  ▶     turn 32 / 58   ▶ play│  native <input type=range>
├────────────────────────────────────────────────────────────────────────┤
│ COMPOSITION — turn 32   [Corroborated ⓘ]      │ tokens        share     │  usage-provenance badge — see below
│  ▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓░░░░▒▒▒▒                    │ Tool I/O  (▓) 12,000 75%│  donut/stacked bar, hatched
│  [donut/stacked bar]                          │ Conversation(░)3,000 19%│  — never chart-only
│                                                │ System    (▒)    500  6%│
│                                                │ [View full turn →]     │  opens Surface 3
├────────────────────────────────────────────────────────────────────────┤
│ CACHE READ / CREATE CHURN                                               │
│  cache_read_input_tokens      ▇▇▇▇▇▇▇▇▇▇▇▇▇▇                          │  literal API field names
│  cache_creation_input_tokens  ▇                                        │  as labels, not "churn score"
│  (per-turn bars, turns 0..58)                                          │
└────────────────────────────────────────────────────────────────────────┘
```

The `[Corroborated ⓘ]` badge next to the composition header is a small
data-provenance indicator, not a new surface: it reports whether this
session's usage figures agree between the transcript parse and consolette's
existing proxy-captured usage, per requirements.md's cross-check bullet and
the `TranscriptOnly | Corroborated | Diverged` state Epic 5.2 (plan.md)
computes. Values: `Corroborated` (proxy and transcript agree), `Diverged`
(proxy saw different totals — transcript still wins as primary per the
requirements decision), or `Transcript only` (session predates proxy
capture, or wasn't proxied). Clicking/focusing the ⓘ expands a one-line
tooltip explaining the specific numbers, e.g. "Proxy saw 3 fewer input
tokens than the transcript for this session."

The collapsed `▸ View growth as text` `<details>` below the growth chart
satisfies the doc's "never chart-only" convention already applied to the
composition and scatter panels: expanding it renders a plain-text/table
summary — e.g. "peak: 720K tokens at turn 14; crossed 500K at turn 9;
native autocompact at turn 55" — computed from the same payload as the
SVG, not a separate fetch.

### Interaction flow

1. **Entry.** Tyler arrives via the `/dashboard` nav link, a Surface 2 row/point
   click (`?session={id}` pre-set), or a typed/bookmarked URL. If no
   `?session=` is present, the dashboard defaults to the most-recently-modified
   session — the common case ("what did I just run") needs zero clicks.
2. **Load.** `aria-live="polite"` banner shows "Loading session data..." while
   `GET /v1/context/sessions` (for the picker) and the selected session's
   `composition`/`growth`/`cache-churn` routes are fetched. This is a
   point-in-time snapshot of the store's last-ingested state as of page
   load — v1 does no live tailing/websocket push (batch/on-demand rescan
   per the architecture research); reloading the page is how Tyler sees
   data ingested since the last load.
3. **Initial render.** Growth chart renders unscaled (no threshold toggled);
   the scrubber defaults to the turn with the largest single-turn token delta
   (the "top growth turn"), not turn 0 — this puts Success Metric #1's answer
   on screen before any interaction, matching the reference tool's
   scrubber/playback default per `research/ux.md` §1.
4. **Threshold toggle.** Clicking "500K" draws a dashed line + danger band and
   sets `aria-pressed="true"` on that button; toggles are independent, so
   multiple thresholds can be shown at once. Clicking again removes it.
5. **Scrub.** Dragging the range input, using arrow keys, or clicking a point
   directly on the growth chart moves the selected turn; the composition panel
   and the churn chart's highlighted turn update in place — no reload, per the
   Chrome DevTools "select a point, everything below filters" pattern
   (`research/ux.md` §1).
6. **Playback.** "▶ play" auto-advances the scrubber one turn at a time
   (pause/stop by clicking again or pressing space when the control has
   focus).
7. **Inspect.** Clicking "View full turn →" (or the autocompact marker, or a
   growth-chart point) opens Surface 3 for that turn.
8. **Switch session.** Selecting a different entry in the picker re-fetches
   that session's data in place; the growth-chart pan/zoom state is not
   preserved across a session switch (it is preserved across turn-scrub
   within the same session).
9. **Leave.** "Session Cost ↗" / "Sessions ↗" navigate to `/dashboard` or
   Surface 2 respectively.
10. **Check provenance.** Clicking/focusing the `[Corroborated ⓘ]` badge
    (see above) expands its one-line tooltip in place; it never blocks or
    requires action — informational only, like the `parse_failure_count`
    banner.

### Error / edge-case handling

| Condition | What Tyler sees | Exit path |
|---|---|---|
| Session has a `SessionRow` but zero `ApiCallRow`s yet (Story 1.4.3) | Chart region shows "No calls recorded for this session yet," scoped to that panel — not a blank SVG or full-page error | Picker/nav stay usable; switch session or wait for next rescan |
| Unknown/deleted `?session=` id (404 from `/v1/context/sessions/{id}/*`) | Page-level banner: "Session not found." | Link back to the Sessions list (Surface 2) |
| Fetch/network failure | `fetch-error` banner: "Failed to load session data" (+ reason if present) | "Retry" button re-issues the fetch; nav links remain clickable |
| `parse_failure_count > 0` | Persistent (non-dismissible) banner naming the exact count: "N of M transcript lines could not be parsed — data on this page may be incomplete," extending the whole-corpus convention at `dashboard.html:358-366` one level down | None needed — informational, not blocking |
| Codex-source session (no compaction/subagent tracking) | Compaction-specific chart regions show a styled "n/a — not tracked for Codex" label, never a bare `0`/`—` | N/A — expected state, not an error |
| Turn-content fetch fails inside the inspector | See Surface 3 | See Surface 3 |

---

## Surface 2: Cross-session analytics view

### Wireframe

```
┌────────────────────────────────────────────────────────────────────────┐
│ consolette · context — Sessions        [Single Session ↗] [Session Cost ↗]│
├────────────────────────────────────────────────────────────────────────┤
│ ⚠ 1 session failed to parse and is not shown                            │  only if parse failures exist
├────────────────────────────────────────────────────────────────────────┤
│  COST/CALL vs PEAK CONTEXT                                              │
│  $                                                                       │
│  │      ●CC                          ●CC  ← hover: id, cost, peak       │
│  │  ●Codex          ●CC                                                 │
│  │                ●CC                                                   │
│  └───────────────────────────────────────── peak context tokens         │
├────────────────────────────────────────────────────────────────────────┤
│  COST/CALL OVER TIME                        (sessions, oldest → newest) │  line chart, Story 2.1.2c
│  $                                                                       │
│  │              ╱╲                                                      │
│  │        ╱╲   ╱  ╲    ╱‾‾                                             │
│  │  ╱‾‾‾‾╱  ╲_╱    ╲__╱                                                │
│  └───────────────────────────────────────── session start time          │
│  up 3.1x over 12 sessions ($0.014/call → $0.043/call, first → last)     │  text summary — never chart-only
├────────────────────────────────────────────────────────────────────────┤
│ Session ▾ │Source│ Cost/call ▾ │ Peak ctx ▾ │ Calls │Coverage│Compaction│
│ abc123    │ CC   │ $0.042      │ 850,000    │ 62    │ 100%   │3 native  │  ← row click navigates
│ def456    │[Codex]│ $0.018     │ 210,000    │ 40    │  —     │  n/a     │    styled badge, not "0"
│ ...                                                                      │
└────────────────────────────────────────────────────────────────────────┘
```

### Interaction flow

1. **Entry.** Nav link from Surface 1 or `/dashboard`, or direct URL.
2. **Load.** `GET /v1/context/sessions/summary`; loading banner while pending.
3. **Render.** Table sorts by "Modified"-equivalent by default; scatter plots
   every session as a point, source-badged (color + label, not color alone);
   the "Cost/Call Over Time" line chart renders from the same payload,
   ordered by `started_at`, with no separate fetch or interaction required —
   the trend is visible on first render, not behind a toggle. A plain-text
   summary (e.g. "up 3.1x over 12 sessions") renders alongside the chart
   from the same payload, satisfying the doc's "never chart-only"
   convention for this chart too.
4. **Sort.** Clicking a column header re-sorts (click again to reverse),
   reusing `dashboard.html`'s existing sort-button/arrow-indicator convention
   (`dashboard.html:95-107`).
5. **Drill down.** Clicking a table row or a scatter point navigates to
   Surface 1 at `/dashboard/context?session={id}` — the Honeycomb/Datadog
   "click a point to pivot to the record" pattern (`research/ux.md` §1).
   Surface 1 opens pre-scrubbed to that session's top-growth turn (same
   default as Surface 1 flow step 3), so the drill-down answers "why" without
   a second search. Table rows are keyboard-activatable (Enter/Space) to
   the same standard as sort headers, per UAC 18b.
6. **Hover.** Hovering a scatter point shows a tooltip with the exact session
   id/cost/peak — numeric fallback alongside the chart, never chart-only.

### Error / edge-case handling

| Condition | What Tyler sees | Exit path |
|---|---|---|
| Zero sessions ingested yet | Empty-corpus banner with a concrete next step: "No sessions ingested yet. Run `consolette context-tracker up` to start capturing sessions, or wait for the next rescan." | Instruction is itself the exit path — actionable, not a bare "no data" |
| Some sessions fail to parse | Row-count readout appends "(N sessions failed to parse and are not shown)," reusing the existing `parseFailureCount` convention (`dashboard.html:358-366`) | Table still usable for the sessions that did parse |
| Fetch/network failure | `fetch-error` banner + "Retry" | Retry button; nav links remain live |
| Codex row's compaction/subagent columns | Styled "n/a" badge, distinct from a numeric `0` | N/A — expected, not an error |
| Fewer than 2 sessions ingested | Trend chart region shows "Not enough sessions yet for a trend" instead of a single-point or empty line | N/A — expected state; table/scatter (if 1 session exists) remain usable |

---

## Surface 3: Message / turn inspector

Embedded, not a modal dialog — an inline collapsible panel beneath Surface
1's composition panel, so scrubbing to another turn while the inspector is
open doesn't require re-navigating a popup stack.

### Wireframe

```
┌────────────────────────────────────────────────────────────────────────┐
│ TURN 32 — full content                                      [Collapse ▲]│
├────────────────────────────────────────────────────────────────────────┤
│ ▸ user_row           "explain the failing test in..."                   │  click to expand
│ ▾ assistant_rows (2)                                                     │  expanded
│     "I'll check the test file first."                                   │
│     tool_use: Read(path="tests/foo_test.rs")                            │
│ ▸ tool_rows (1)       tool_result: 14,200 chars (collapsed by default)  │  large blobs start collapsed
└────────────────────────────────────────────────────────────────────────┘
```

### Interaction flow

1. **Open.** Clicking "View full turn →" in the composition panel, a
   growth-chart point, or an autocompact marker opens/expands the panel for
   the currently-scrubbed turn; lazily fetches
   `GET /v1/context/sessions/{id}/turns/{turn_index}`.
2. **Expand/collapse rows.** Each of `user_row`/`assistant_rows`/`tool_rows`
   is its own collapsible section (native `<details>`/`<summary>` or
   equivalent keyboard-operable toggle), starting collapsed for large blobs
   (a "14,200 chars" summary line lets Tyler decide before rendering a huge
   tool result inline).
3. **Re-scrub while open.** Moving the Surface 1 scrubber while the inspector
   is open re-fetches and re-renders the panel for the new turn in place,
   rather than closing it — avoids Tyler having to reopen the inspector for
   every adjacent turn while scanning a "top growth turns" range.
4. **Close.** "Collapse ▲" hides the panel; the composition panel and chart
   remain as they were.

### Error / edge-case handling

| Condition | What Tyler sees | Exit path |
|---|---|---|
| Turn-content fetch fails | Inline "Could not load turn content" message with a "Retry" link, scoped to the panel | Retry link; closing the panel is always available regardless of fetch state |
| Turn has no tool rows / no assistant rows | The empty section is omitted entirely (not shown as an empty collapsible with nothing inside) | N/A — not an error |

---

## Non-interactive surfaces (condensed)

### `consolette context-tracker up`

```
$ consolette context-tracker up
Backing up ~/.claude/settings.json → ~/.claude/settings.json.bak.1755974400
Installing consolette context hooks...
  PostToolUse: appended (1 existing hook preserved)
  SessionStart: appended (0 existing hooks)
  ... (10 events total)
Done. 10 hook events wired. Existing hooks untouched.
Run `consolette context-tracker down` to remove.

$ consolette context-tracker up   # re-run, idempotent
consolette context hooks already installed (10/10). No changes made.
```

- Exits `0` on both a fresh install and an idempotent no-op re-run (Story
  4.1.2 AC).
- Always prints the backup file path it just wrote, so Tyler can locate it
  without reading source or `settings.json` itself.
- Names "appended" vs. "already present" per hook event, so a re-run's output
  is legible rather than a repeat of the same lines.
- Never prints the contents of `settings.json` (only the hook-event keys it
  touched) — unrelated keys (`env`, `permissions`) may carry secrets.
- On a write failure (unwritable file, disk full), prints a specific error to
  stderr, exits non-zero, and leaves `settings.json` byte-for-byte untouched
  (atomic tmp-write + rename per Story 4.1.1).

### `consolette context-tracker down`

```
$ consolette context-tracker down
Removing consolette context hooks from ~/.claude/settings.json...
  PostToolUse: removed consolette entry (1 other hook preserved)
  ... (10 events total)
Done. Backup at ~/.claude/settings.json.bak.1755974400 is untouched
(not restored — only consolette's own entries were removed).

$ consolette context-tracker down   # re-run, nothing installed
No consolette context hooks found. Nothing to do.
```

- No-op case exits `0`, does not error, and does not rewrite the file (Story
  4.1.3 AC).
- Explicitly states it does **not** restore from backup — prevents Tyler from
  assuming `down` reverts to the pre-`up` snapshot, which would silently
  discard any hooks he added manually after `up` ran.
- Names exactly which hook events were touched, mirroring `up`'s output shape.

### `consolette context-hook <event>`

Fire-and-forget, invoked by Claude Code itself on the hook's hot path — not
something Tyler types interactively, so its "UX" is entirely about not being
noticed.

```
$ echo '{"session_id":"...","tool_name":"Bash"}' | consolette context-hook PostToolUse
$ echo $?
0
# (no stdout)

$ echo '' | consolette context-hook PostToolUse   # malformed/empty payload
$ echo $?
0
# stderr/tracing only: WARN ... empty/malformed stdin payload for PostToolUse, skipping insert
```

- Always exits `0`, valid or malformed payload — must never block or fail the
  user's actual tool call (Story 4.2.1 AC).
- Zero stdout on success; a hook writing to stdout risks being interpreted by
  Claude Code's own hook protocol.
- Warnings go to stderr/`tracing` only.
- Completes within Claude Code's hook timeout budget — contingent on the
  still-open verification flagged in `implementation/plan.md`'s Unresolved
  Questions ("exact hook-timeout budget... not independently verified").

---

## UX acceptance criteria

**Task completion**

1. From `/dashboard`, Tyler reaches the context-growth chart for his most
   recent session in ≤2 clicks (nav link → default-selected session).
2. Tyler identifies the top token-cost-contributor category for any turn in
   ≤1 interaction (scrub — no separate "load composition" step).
3. Tyler identifies the turn where a session crossed a chosen budget in ≤2
   clicks (threshold toggle; the crossing point is already visible inside
   the danger band).
4. Tyler goes from "which session is expensive" (Surface 2) to "why"
   (Surface 1, pre-scrubbed to the peak turn) in exactly 1 click.
5. Tyler inspects a turn's full raw content in ≤2 clicks from Surface 1
   (select turn, click "View full turn").
6. Tyler sorts the cross-session table by any column in 1 click, and
   reverses that sort in a second click on the same header.
7. Tyler switches which session Surface 1 shows in 1 selection, without a
   full page reload.

**Error states — no dead ends**

8. Zero-calls-yet session: chart region shows "No calls recorded for this
   session yet," scoped to that panel, not a blank canvas or full-page error.
9. Unknown/deleted session id: "Session not found" plus a link back to
   Surface 2 — never a blank page or uncaught error.
10. Fetch/network failure: banner reads "Failed to load session data" and
    offers a "Retry" action; nav links remain operable regardless.
11. Partial ingestion (`parse_failure_count > 0`): a persistent banner names
    the exact count, so a partial session is never visually indistinguishable
    from a complete one.
12. Cross-session view with zero sessions: banner names a concrete next step
    (`consolette context-tracker up`, or wait for rescan), not a bare "no data."
13. Codex-source session: compaction/subagent fields show a distinctly
    styled "n/a" badge, never a bare `0` or `—`.
14. Inspector turn-content fetch failure: inline "Could not load turn
    content" with a retry link, scoped to the panel — the rest of the
    dashboard stays intact.
15. Every state above offers at least one visible, clickable way forward
    (retry, nav link, or a concrete instruction) — none is a text-only dead
    end.

**Accessibility**

16. Every chart region exposes an `aria-live="polite"` status element that
    announces loading/error/empty transitions without moving focus.
17. The turn scrubber is a native `<input type="range">`; full turn-by-turn
    navigation works with arrow keys alone, no drag required.
18. Budget-threshold toggles, sort headers, and "View full turn"/"Retry"
    affordances are real `<button>`/`<a>` elements with visible
    `:focus-visible` outlines — no click-only `<div>`s.
18b. Cross-session table rows (Surface 2 drill-down) are keyboard-activatable
    to the same standard: either a real `<button>`/`<a>` wraps the row's
    content, or — where a real interactive element can't wrap a `<tr>` —
    `tabindex="0"` + `role="button"` plus Enter/Space handling, with a
    visible `:focus-visible` outline matching UAC 18.
19. No dashboard concept (composition category, source badge, compaction
    "n/a") is encoded by color alone — each pairs a hatch/pattern or text
    label with its color, satisfying ≥4.5:1 contrast independent of hue.
20. Budget/danger-band lines maintain sufficient contrast against the chart
    background in both light and dark themes.
21. The dashboard renders correctly under light, dark, and unset OS theme
    (CSS-custom-property pattern, not a fixed-dark stylesheet).

**CLI**

22. `context-tracker up`/`down` always print which hook events were touched
    and where the backup file is — Tyler never has to open `settings.json`
    to know what changed.
23. `context-tracker down` explicitly states it does not restore from
    backup, preventing a false assumption that would clobber hooks added
    after `up`.
24. `context-hook <event>` produces zero stdout on success and always exits
    `0` — a malformed payload never blocks or errors the user's real tool
    call.

**Trend visibility**

25. Tyler reads whether cost/call is rising, falling, or flat across his
    recent sessions directly from Surface 2's trend chart, in 0 clicks
    (rendered on page load alongside the scatter, no extra fetch or toggle);
    the chart is never the only source of that answer — a plain-text summary
    (e.g. "up 3.1x over 12 sessions") renders alongside it from the same
    payload.

**Data provenance & chart fallback**

26. Surface 1's composition panel shows a `Corroborated` / `Diverged` /
    `Transcript only` provenance badge for the session's usage figures
    (requirements.md's cross-check bullet, plan.md Epic 5.2), with a
    one-line tooltip on click/focus — Tyler never has to guess whether a
    number came from the transcript alone or was cross-checked against
    proxy data.
27. The context-growth chart (Surface 1) has an adjacent, expandable
    plain-text/table summary (peak value, threshold-crossing turn,
    autocompact turn) computed from the same payload as the SVG — the
    chart is never the sole source of that data, matching the composition
    and scatter panels' existing convention.

---

## Summary

- **6 surfaces designed**: 3 interactive (single-session dashboard,
  cross-session analytics, message/turn inspector) with full wireframe +
  flow + error-state treatment, 3 non-interactive (CLI: `context-tracker
  up`, `context-tracker down`, `context-hook <event>`) with condensed
  sample-output + acceptance-criteria treatment.
- **28 UX acceptance criteria**: 7 task-completion, 8 error/no-dead-end, 7
  accessibility (includes 18b), 3 CLI, 1 trend visibility, 2 data
  provenance/chart fallback.
