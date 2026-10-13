# Gap Report: webui-trace-model-mapping

## Epic 1 validation (Task 1.1.4i)

**Status: WAIVED (2026-10-10).** Tyler instructed via /goal on 2026-10-10 ("Finish the rest of the SDD steps and merge the pr") to proceed without the one-day usage validation. The day-of-use validation was NOT performed. The riskiest assumption (operators will actually use Inspect/Models) is therefore still UNVALIDATED and is re-checked in Story 5.1.3 (value-risk usage check). No usage result is recorded or implied below.

Procedure (plan Story 1.1.4, "Epic 1 boundary validation"): after the Epic 1 PR merges and the binary has run on real traffic for one working day, Tyler uses the Live Request Traces table and the existing Inspect link (`/sessions?requestId=`) and records:

| Field | Entry |
|-------|-------|
| Date(s) used | not performed (waived) |
| Days used | not performed (waived) |
| Reached for the table instead of `/metrics` or logs? | not performed (waived) |
| What he missed | not performed (waived) |

The gate would have stopped the plan on a "did not use it" result; with the waiver it did not run.

## Exploratory pass (Story 5.1.3)

Run date: 2026-10-12. Operator-tester: Claude Sonnet 5.5 agent driving a real headless Chrome (no human operator). 

### Environment

- Build: `CONSOLETTE_REQUIRE_UI=1 cargo build` at branch `feat/webui-trace-model-mapping` (HEAD 1baa29e plus three uncommitted working-tree edits in `src/dashboard.rs`, `src/providers/openai/{mod,resolution}.rs` that the build included). Binary `target/debug/consolette run`.
- Isolation: `HOME=/tmp/expl/home` so the config (`/tmp/expl/home/.config/consolette/conf.d/00-expl.toml`) and the response cache (`~/.claude/consolette/response-cache.sqlite`) are throwaway; proxy on `127.0.0.1:19100`.
- Fake upstreams (throwaway Python, `/tmp/expl/fake.py`, not committed): `flaky` (anthropic kind, :19101, 429 on `[flaky]`), `steady` (openai kind, :19102, route pin `gpt-5.1`, lists 67 models), `broken` (openai kind, :19103, `/models` answers 401), a fake LM Studio on :1234 for a real local-discovery model (`local/lm-studio/fake-local`). Route `default` = fallback `[flaky, steady(model=gpt-5.1)]`. `[response_cache] enabled = true`. Request bodies carry a per-run nonce.
- Browser: puppeteer (devDependency of `ui/package.json`) driving `/Applications/Google Chrome.app` headless; viewport 1440x900 unless stated; scripts under `/tmp/expl/*.mjs`.
- No real upstream credentials were used; the real Ollama on :11434 serves no models and was left alone.
- Server-tool request: see "Not exercised" below.

Evidence paths: `P` = `project_plans/webui-trace-model-mapping/evidence/`. Screenshots not copied there are in `/tmp/expl/` (throwaway, not committed); their names are given so the run can be repeated with the same scripts.

### Scenario results (research/features.md section 6)

| # | Scenario | Steps (all against `127.0.0.1:19100`) | Observed | Result |
|---|----------|----------------------------------------|----------|--------|
| 1 | Raw SSE capture | `curl -N /v1/dashboard/events` during traffic | `RequestTrace` is camelCase with `id`, `requestId`, `attempts[]`, `routeReason`, `traceRevision`. `MetricsTick.providerHealth` is still the constant `"healthy"` for all 3 upstreams while `/metrics` showed flaky `half_open`. | pass (trace), fail (health, E-17) |
| 2 | Alias through a pinned route | `claude-sonnet-4` via route `flaky -> steady(model=gpt-5.1)` | Drawer: `Requested claude-sonnet-4 / Routed steady / gpt-5.1`, reason text. Upstream-side capture of the wire model was not taken. | partial (wire model not compared) |
| 3 | 429 then fallback | `[flaky]` marker in body | One row, `200 OK`, `fell back`, chips `fallback`, `x2 attempts`; drawer lists `flaky x 429 rate limited` then `steady / gpt-5.1 ok 200` (`/tmp/expl/b1-drawer-fallback.png`). | pass |
| 4 | Total failure / validation / auth | `[dead]` marker (all upstreams 500); truncated JSON body; body without `messages` | `[dead]`: listed as `500 Server error`, drawer `2 attempts; all failed` (`P`x06 and `/tmp/expl/g1-drawer-total-failure.png`). Truncated JSON: client got 400, **no trace** (E-21). Body without `messages` was forwarded and answered 200 (proxy behavior, not a UI gap; noted only). 401 auth failure: not exercised (the proxy has no inbound auth to fail). | partial |
| 5 | Streaming | 12-chunk SSE, 0.4 s apart (5.5 s wall) | Trace `totalMs` 7, `firstByteMs` null, status 200: durations are header time (E-19). | fail (known, Epic 6) |
| 6 | Session list and replay | requests with and without `metadata.user_id`; open Session Replay | List empty on load although the API returns 2 sessions (E-03); after typing in search the rows appear; click -> `/requests/<sessionId>` 404 (x2); timeline/tokens hardcoded (E-05). | fail |
| 7 | Inspect | click `Inspect` | Opens drawer at `/overview?requestId=...` (the dead end is fixed); Esc closes and returns focus to the button. Header overlap E-01. | pass with defects |
| 8 | Benchmarks empty and with traffic | fresh process, then traffic | Empty: 4 fabricated rows (`claude-3-5-sonnet`...) unmarked (`P`x04). With traffic: real rows but `0.0%` error rate despite a 500, speed `0`, aider `55.0%` fallback, TTFT = duration (E-06). | fail |
| 9 | Config edit | edit key/RPM/base URL, Preview Diff, Apply | See E-11 to E-16. Bearer clobber and silent RPM drop VERIFIED by direct `PUT` on a second throwaway proxy (see below). | fail |
| 10 | `/api/models` | `curl /api/models` with one 401 upstream | 0.03 s, per-upstream map, `broken` carries the raw upstream body in its error string (E-27). UI Models view uses the catalog endpoint instead. Unreachable-upstream timeout not tried. | partial |
| 11 | Refresh mid-traffic, stop and restart proxy | reload page; kill and restart process | Reload: history gone (E-20). Outage: badge stays `Connected` for 30 s, and after the restart no new row arrives until reload, although a raw `EventSource` opened in the same page received events (E-02, `P`x05). | fail |
| 12 | Burst / `system_lag` | 2500 requests, 48 threads, one never-reading SSE client | Server emitted `event: system_lag {"skipped":760}`; 1742 of 2500 traces delivered to that client. UI consumption of `system_lag` not observed (could not induce lag in the browser consumer). | backend pass, UI not exercised |
| 13 | Local model | fake LM Studio on :1234, `local/lm-studio/fake-local` | Row with chip `local`, provider `local`, 200 (`/tmp/expl/a1-overview-traces.png` = `P`x00). | pass |
| 14 | Hot swap in flight | stream in flight, `PUT /v1/dashboard/config {"fallbackCascade":[steady,flaky]}` at +1.5 s | Stream finished normally (`message_stop`), next request served by `steady`. The Config page is not refreshed by the `ConfigChanged` event (code reading, UI not exercised). | pass (backend) |

Also exercised: client abort mid-stream (row shows plain `200 OK`, E-19) and before headers (`499 Client closed`, chip `cancelled`, works: `P`x06); response-cache hit (`200 (cache)`, chip `cache`, drawer `Attempts (0)`); malformed-JSON 400.

### Gap table

Severity: S1 broken, misleading, or blocks a Must a11y item; S2 wrong but workable; S3 polish. "Plan story" is where the fix belongs; "Disposition" is the final outcome from Story 5.1.4 (the 4.1.0 proposals are in the Re-rank section).

| ID | Description | Evidence | Sev | Plan story | Disposition (final, Story 5.1.4) |
|----|-------------|----------|-----|------------|-------------|
| E-01 | Inspect drawer top 64 px (heading, Copy, Close) sits under the sticky app header (header `z-50`, drawer `z-40`, drawer `top-0`); `elementFromPoint` at the Close button returns the header. Keyboard focus lands on the hidden Copy and Close buttons (Tab 1 and 2 at y=20). At 320 px the sheet is full-screen, so no visible close control. Esc still works. | `P`x01, `P`x09, `/tmp/expl/k3.mjs` output | S1 | 4.1.7 (drawer offset) | fixed `71071e3` |
| E-02 | Live stream recovery: header badge stays `Connected` through a 30 s proxy outage; after restart the UI shows no new traces until a reload, while a raw `EventSource` on the same page receives them. Headless Chrome only; not repeated headed. | `P`x05, `/tmp/expl/f2.mjs`, `/tmp/expl/f3.mjs` | S1 | 4.1.5 | fixed `87da0c5`, `56dcd84`, `fff9192` |
| E-03 | Session Replay list is empty on first load although `/v1/dashboard/sessions` returns sessions. Root-cause hypothesis: `filteredSessions` is a `computed()` over a plain `@Input` (`session-list.component.ts:95-103`), so it never recomputes when the input changes; typing in the search box forces it and the rows appear (observed). | `/tmp/expl/d2-sessions-list.png`, `/tmp/expl/d3.mjs` | S1 | 4.1.1 | fixed `4bb1a1b` |
| E-04 | Session rows are not keyboard-focusable (only the scroll region takes focus), so a keyboard-only operator cannot select a session. | `/tmp/expl/k2.mjs sessions` | S1 | 4.1.7 | fixed `71071e3` |
| E-05 | Session Replay fabrications and dead payloads (research gaps 6 and 8, now VERIFIED): `/requests/<sessionId>?stage=original|compressed` both 404; timeline (`Total 203ms`, Compression 14 ms) and token bar (`Prompt 1,250`) are hardcoded constants shown for any session; list shows `SAVED % 100.0` because `tokens_after` is 0. | `P`x03 | S1 | 4.1.1, 4.1.2 | fixed `4bb1a1b`, `2bd33e5`, `8f32dcf` |
| E-06 | Benchmarks (research gap 9, VERIFIED): cold start shows 4 fabricated rows with `0.5%` error and `42` tok/s and no marker; with traffic, error rate is `0.0%` although a 500 occurred, speed `0`, aider falls back to `55.0%`, TTFT equals duration. | `P`x04, `/tmp/expl/d1-bench-traffic.png` | S1 | 4.1.2 | fixed `2bd33e5`, `e511102` |
| E-07 | UX-18 focus ring: every control uses the browser default ring (`#005fcc`, `1px auto` on nav and scroll regions, `2px solid` on buttons) because the `focus-visible:outline` classes name no colour (`recent-trace-table.component.ts:92,104,132,152,169`). Contrast, computed: 3.25:1 on page `#090d16`, 2.44:1 on card `#1e293b`, 2.53:1 on button `#262626`; the plan asks 3:1. | `/tmp/expl/k2.mjs` output, contrast computed with the WCAG formula | S1 | 4.1.7 | fixed `71071e3` |
| E-08 | Config Editor text and number inputs show no focus indicator at all (outline `none`, border colour unchanged on focus). | `/tmp/expl/k3.mjs` (border before/after identical), `/tmp/expl/k2.mjs config` | S1 | 4.1.7 | fixed `71071e3` |
| E-09 | UX-27: 5 `animate-pulse` status dots (header badge, page badge, 3 provider `Active` dots) keep animating under `prefers-reduced-motion: reduce`; the 0.3 s transitions are also unchanged. | `/tmp/expl/r1.mjs` output | S1 | 4.1.7 | fixed `71071e3`, `a976a85` |
| E-10 | UX-18 target size: Config weight sliders are 8 px high; Benchmarks CDF checkbox is 13x13 px (under 24x24). Trace-table and Models buttons pass (24x24 or larger). | `/tmp/expl/k2.mjs config`, `k2.mjs benchmarks` | S1 | 4.1.7 | fixed `71071e3` |
| E-11 | UX-24 contrast: neutral-500 hint text 3.78:1 (`Counts every upstream attempt...`, benchmark percentile sub-values), slate-500 empty-state text 3.76:1, white on the cyan `Apply Config` button 3.62:1. Trace table, drawer and Models text all pass 4.5:1. | `/tmp/expl/ct.mjs` (colours rasterized via canvas to avoid oklch parse errors) | S1 | 4.1.7 | fixed `71071e3` |
| E-12 | M6 keyboard path misses the target: from page top the first row's `Inspect` is the 8th Tab stop (5 nav links, scroll region, Copy, Inspect), 9 key presses with Enter; mouse path is 2 interactions (scroll to the table, click). Plan rule makes a miss S1; judgement call because the plan says the operator works keyboard-first. | M6 section below | S1 | 4.1.0 re-rank (skip-link or table first in order) | fixed `71071e3` (residual: Copy precedes Inspect, see B-12) |
| E-13 | Config `PUT` clobbers Bearer auth (research gap 10, VERIFIED): upstream with `auth.type = "bearer"` received `Authorization: Bearer sk-orig-bearer` before and `x-api-key: sk-new-bearer` with no `Authorization` after a `PUT` that returned 200. | second throwaway proxy, `/tmp/expl/f1.log` header lines | S1 | 4.1.3 | fixed `768718c` |
| E-14 | Config `rateLimits.rpm` edits are dropped silently: `PUT {"rpm":120}` returned 200 and `GET` still returns `"rpm":"shed"`. The editor loads an empty RPM field with a red `Must be a non-negative integer` shown before any edit. | same `PUT`/`GET` pair; `/tmp/expl/e1-config.png` | S1 | 4.1.3 | fixed `768718c` (rate limits now read-only in the editor) |
| E-15 | Config Apply fails for the whole config when any provider has an `http://` base URL: the UI sends `baseUrl` for every provider and one `Scheme must be HTTPS` returns 400 for all. The banner shows Angular's raw `Http failure response for ...: 400 Bad Request`, not the server's reason; clearing a base URL gives `Invalid URL: relative URL without a base`. Workaround: edit the TOML. | `P`x07 | S2 | 4.1.3 | fixed `768718c` |
| E-16 | Config form details: typing into the masked key field appends to the mask (`****sk-rotated-123` is what Preview Diff and the `PUT` carry; server effect not observed because the 400 came first); all weights show `0%` and `Total Weight: 0%` on a fallback route; `flaky` (anthropic kind) loads with the example placeholder URL because `GET` omits its `baseUrl`; Preview Diff compares an active config with duplicate snake_case keys against a draft without them. | `/tmp/expl/e2-config-preview.png` | S2 | 4.1.3 | fixed `768718c` (residual: Preview Diff duplicate snake_case keys, see B-11) |
| E-17 | Provider health is constant: grid and `MetricsTick.providerHealth` say `Active` / `healthy` for all upstreams, including `broken` (models endpoint 401) and `flaky` while `/metrics` reports `half_open`. | `P`x06 (grid), SSE capture, `/metrics` `cooldowns` | S2 | 4.1.4 | fixed `ac45774` |
| E-18 | KPI cards `Tokens per second`, `Median TTFT`, `Token savings` are permanently `0`, `0ms`, `0.0%` (research gaps 11 and 13, still open). | `/tmp/expl/s01-overview.png` | S2 | 4.1.4 and Epic 6 | fixed `ac45774` (cards made honest; real values arrive with Epic 6, see B-02) |
| E-19 | Stream and abort accuracy: a mid-stream client abort is recorded as `200 OK` with no `cancelled` chip (only a pre-header abort gives `499`); a 5.5 s stream shows `totalMs` 7 and drawer `Total 7ms`; `First byte` reads `n/a (not measured)` and never resolves. | `P`x06, `/tmp/expl/sse2.txt` | S2 | Epic 6 (new tasks for abort and total) | backlog B-01 (Med) |
| E-20 | No trace backfill (research gap 16): a reload shows `No requests yet` with requests still in the backend ring buffer. | `/tmp/expl/f3.mjs` (rows after reload) | S2 | 4.1.6 | fixed `afe24c4`, `1f66c99` |
| E-21 | Malformed-JSON 400 produces no trace and no row; the operator cannot see why a client failed. | SSE count 0 for the 400 | S2 | new (1.1 follow-up) | backlog B-03 (Med) |
| E-22 | The trace table is below the fold at 1440x900 (0 rows visible on load; heading at y=855); the page order is 4 KPI cards, health, chart, then traces. | `/tmp/expl/s01-overview.png` | S3 | 4.1.0 | backlog B-04 (Low) |
| E-23 | Drawer wording: cache hit shows `Routed claude-sonnet-4` though no upstream ran; a session-pinned request says `pinned: steady's configured model replaces...` although the model came from the session pin. | `/tmp/expl/m6-cachehit.png`, M6 answers below | S3 | 2.1.x wording | fixed `a73e5b5` |
| E-24 | Reflow: at 320 px the nav is `display:none` with no replacement (also at 640 css px, i.e. 200% zoom of a 1280 px window) and the page scrolls horizontally by 112 px (745 px on Overview); the header badge is clipped. The trace table scrolls inside its own region as intended. | `/tmp/expl/z1.mjs`, `/tmp/expl/z1-table-320.png` | S2 | 4.1.7g | fixed `71071e3`, `d243858` |
| E-25 | Forced-colors: the drawer waterfall bars and status dots disappear (background-based); the text beside them still carries the data. Sidebar is not sticky, so navigation scrolls out of view on the long Overview page. | `P`x08, `P`x06 | S3 | 4.1.7 | fixed `71071e3` |
| E-26 | Legend (`What do these dashes and chips mean?`): term and definition run together with no separator (`n/a (not streamed)the response was not...`). | `/tmp/expl/a2-legend.png` | S3 | 1.1.4 polish | fixed `6419f2d` |
| E-27 | Legacy `GET /api/models` returns the raw upstream error body in `broken` (`validation error: {"error": ...}`); the UI catalog sanitizes (`auth failed (401)`). | `curl` output | S3 | 3.1.2 | backlog B-05 (Low) |
| E-28 | Models page: the pinned-model warning uses neutral styling with a `!` glyph only; three caveat paragraphs sit above the content; the `role=status` live region was empty 2.5 s after a Refresh, so the UX-22 announcement was not observed (could be transient; not confirmed). | `P`x02 | S3 | 3.1.5, 4.1.7 | fixed `6e4d253` |

Not gaps (confirmed working): trace cells are real values or `n/a (reason)`; chips (`fallback`, `x2 attempts`, `cache`, `local`, `cancelled`) and the dash legend render; failures and 499 are listed; Esc closes the drawer and returns focus to the Inspect button; Models states (`NOT LOADED`, `ERROR auth failed (401)` with Retry, `OK n models refreshed Ns ago`), search, and the pinned-model warning block (UX-04) work; Models and trace buttons meet 24x24; contrast passes on the trace table, drawer and Models.

### Re-labelled research claims

| Claim | Result |
|-------|--------|
| PUT auth clobbers Bearer | VERIFIED (E-13) |
| Session Replay payloads 404 | VERIFIED (E-05) |
| Benchmarks seed rows | VERIFIED (E-06) |
| Provider health hardcoded `healthy` | VERIFIED (E-17) |
| `system_lag` emitted to slow consumers | VERIFIED backend (scenario 12); UI handling not observed |
| Inspect dead end | REFUTED (fixed by Epic 2; opens the drawer) |
| Trace columns blank (casing mismatch) | REFUTED (fixed by Epic 1) |

### M6 timed walk-throughs ("why did request X go to model Y", UI only)

Method and limit: a scripted run (`/tmp/expl/m6.mjs`) in headless Chrome on the Overview page with the stream already connected. The seconds below are automation time from the click to the answer being in the DOM, so they are a floor and say nothing about a human reading; no human operator took part. The tester also read the answer from the drawer screenshot (`P`x00 style), where it fits in the first viewport.

| Walk-through | Interactions (mouse) | Interactions (keyboard) | Automation time | Answer the drawer gave |
|--------------|----------------------|--------------------------|-----------------|------------------------|
| Fallback (429 then second upstream) | 2 (scroll to table, click Inspect) | 9 (8 Tab, Enter) | 266 ms | `Routed steady / gpt-5.1`, `Why fallback moved to the next candidate after flaky failed (rate limited, HTTP 429)` |
| Session-pinned (`POST /api/sessions/<id>/route`) | 2 | 9 | 144 ms | `Routed steady / gpt-5.2`, `Why pinned to this upstream by the session` |
| Cache hit | 2 | 9 | 98 ms | `Why cache served from the response cache; no upstream was called` (but `Routed claude-sonnet-4`, E-23) |

Mouse path meets the target (<= 2 interactions); human seconds against the 60 s target are not measured. The keyboard path misses the interaction target (E-12).

### Value-risk usage check (requirements "Value Risks")

Day-of-use validation not performed; no real operator usage evidence. This section does not fill in that field and the Epic 1 waiver above is unchanged. The 15-minute unprompted working session with live traffic was not run: the only user in this pass was an agent that had read the plan, so it cannot say what an unprompted operator reaches for.

Observed value-risk evidence (design and session observations, not usage):

- Discoverability of Inspect: the trace table starts below the fold at 1440x900 (E-22) and each row carries a plain `Inspect` button; the first-load screen above the fold is 4 KPI cards (3 permanently zero, E-18) and a provider grid that always says `Active` (E-17).
- Usefulness once reached: the drawer answered all three M6 questions in one click with the answer on screen (fallback chain, pin, cache), and showed a total failure's attempt list.
- History loss: a freshly opened or reloaded page shows no traces (E-20), so Inspect only helps for requests that arrived while the page was open. This is the largest value risk observed.
- Models: reachable from top-level navigation; shows `NOT LOADED` until Refresh is pressed (on-demand default), after which states and the pinned-model warning were useful and legible.
- Recommendation on cut #3 (waterfall bars) and Models extras: none either way; there is no usage evidence, so the plan default stands. The waterfall bars disappear in forced-colors (E-25) and the text rows carry the same data, which is a small point in favor of cutting them first if time slips.

### Not exercised

- Server-tool (`web_search`) request: the default config reaches the real `stapler-mcp` search backend (`backend_reachable=true`), so it was not run live; covered by `tests/trace_model_mapping_e2e.rs` against a fake backend.
- Real upstream credentials and a real provider: none used (fake upstreams only); credential-flow behavior of Models refresh (SSO prompts) not observed.
- Screen reader pass (VoiceOver/NVDA): not run, no assistive-technology driver in the headless session; only roles and aria attributes were read (drawer `role=dialog aria-modal=false`, `aria-labelledby` the heading; `role=status` region).
- Reduced-motion check of the Chart.js canvas and drawer slide: only CSS animations and transitions were enumerated; canvas animation not measured. The drawer has no CSS transition.
- Live-region and held-update behavior (UX-22/UX-23) under a focused row: not exercised.
- SSE outage behavior in a headed browser, `system_lag` handling in the UI, `ConfigChanged` refresh of the Config page, unreachable-upstream timeout for `/api/models`, wire-model capture at the upstream (scenario 2), three simultaneous tabs: not exercised.
- Contrast of non-text elements (borders, chart lines) and of dimmed rows: not measured.

## Re-rank (Story 4.1.0)

Scored from the Story 5.1.3 table (14 S1, 8 S2, 6 S3). Rule (Tyler, 2026-10-10): S1 and S2 are fixed, S3 goes to backlog unless it is a trivial wording fix; the Must a11y items in 4.1.7 are uncuttable. Every existing Epic 4 story is backed by at least one verified S1/S2 gap, so none is dropped (no plan cut applies).

| Story | Decision | Evidence | Resize / additions |
|-------|----------|----------|--------------------|
| 4.1.1 Session Replay ids / Inspect link | keep, re-size +0.3 d | E-03 (list empty on first load), E-05 (payload 404s) | add E-03 fix (`computed()` over a plain `@Input`) with a regression spec |
| 4.1.2 Remove fabricated data | keep | E-05, E-06 (both S1) | include Session Replay timeline/token-bar constants (E-05) |
| 4.1.3 Config Editor | keep, re-size +0.4 d | E-13, E-14 (S1), E-15, E-16 | add: per-provider baseUrl only when changed, show server error reason (E-15); masked key field must not append to the mask, weights from the route (E-16) |
| 4.1.4 Real health + tick | keep | E-17 (S2), E-18 (S2) | E-18: hide the three permanently-zero KPI cards (or show n/a) instead of 0 |
| 4.1.5 SSE events / reconnect | keep core | E-02 (S1) | E-02 root cause (client never reconnects, badge stays Connected) is the first task |
| 4.1.6 Trace backfill | keep | E-20 (S2) | none |
| 4.1.7 Accessibility | keep in full | E-04, E-07-E-12 (S1), E-24, E-25 | add: Inspect drawer sits under the sticky header (E-01, S1: header z-index vs drawer top offset), session rows keyboard-focusable (E-04), Overview Inspect keyboard path (E-12), reflow nav replacement (E-24), forced-colors bars (E-25), live-region verification (E-28) |

New or deferred items:

| Gap | Disposition |
|-----|-------------|
| E-01 drawer under header (S1) | fixed in 4.1.7 |
| E-19 mid-stream abort recorded as 200, streamed durations (S2) | backlog: needs the deferred Epic 6 stream tee (first-byte/stream-end timing); not implementable honestly without it |
| E-21 malformed-JSON 400 produces no trace (S2) | backlog (follow-up to Story 1.1.3; needs a pre-handler extractor rejection hook) |
| E-22 table below the fold (S3) | backlog (layout decision for Tyler) |
| E-23 drawer wording for cache/pinned (S3) | quick win in 5.1.4b |
| E-26 legend term/definition separator (S3) | quick win in 5.1.4b |
| E-27 legacy `/api/models` leaks raw upstream body (S3) | backlog (migrate callers to the sanitized catalog) |
| E-28 Models warning styling, live region (S3) | verify in 4.1.7c, else backlog |

## Epic 4 outcome (Story 4.1.7 accessibility pass)

Run date: 2026-10-12. Measured in headless Chrome (puppeteer) against `CONSOLETTE_REQUIRE_UI=1 cargo build` of the working tree at `d243858`, fake upstreams as in the exploratory pass, scripts `/tmp/expl/{m2,z1,r1,ct,e28}.mjs` (throwaway). Commits: `6419f2d` E-26, `a73e5b5` E-23, `3fcf348` a11y-checks + Karma builder, `399c240` live region/Pause/hold, `71071e3` the main a11y pass, `6e4d253` E-28, `a976a85` chart motion, `d243858` reflow fix.

| ID | Status | Evidence (before -> after) |
|----|--------|----------------------------|
| E-01 | fixed `71071e3` | Close/Copy/heading hit-tested as the header at y=20 -> drawer at `top-16`; `elementFromPoint` returns the control itself at 1440 and 320 px (y=84), focus lands on the heading at y=84. Spec uses `elementFromPoint` against a replica header. |
| E-04 | fixed `71071e3` | rows had no tab stop -> each row has a `<button>` (130x24, ringed); spec selects a session via keyboard-focusable control. |
| E-07 | fixed `71071e3` | default ring 3.25:1 page / 2.44:1 card / 2.53:1 button -> global `:focus-visible` 2px `#7dd3fc`; 145 sampled tab stops over 5 routes: 0 without ring, min 11.65:1 on page, 8.77:1 on card. |
| E-08 | fixed `71071e3` | Config inputs no indicator -> same ring (38 Config tab stops, 0 without). Inputs also got label `for`/`id` (they had no accessible name). |
| E-09 | fixed `71071e3`, `a976a85` | 5 pulse dots and 0.3 s transitions under `reduce` -> 0 running animations, transitions 1e-05 s. `animate-*` is now `motion-safe:`; latency chart `animation:false` under reduce (unit spec; canvas not sampled in a browser; RPM chart was already `false`). |
| E-10 | fixed `71071e3` | sliders 8 px, checkbox 13x13 -> 24 px; 0 of 145 tab stops under 24x24. |
| E-11 | fixed `71071e3` | neutral-500 3.78, slate-500 3.76, Apply 3.62 -> neutral/slate-400 and cyan-700; page scan of 5 routes finds no text under 4.5:1 except the gradient wordmark (transparent clip text, false positive). Stale rows dimmed by colour token (`.stale-rows`), not opacity. |
| E-12 | fixed `71071e3` (judgement) | 9 key presses -> 5 (skip link, Enter, Copy, Inspect, Enter). Residual: Copy still precedes Inspect; mouse path is still 2 interactions. |
| E-23 | fixed `a73e5b5` | cache hit reads `Answered for <model> (cache hit; no upstream ran)`; `session_pin` source says the model came from the session's pin. |
| E-24 | fixed `71071e3`, `d243858` | 320 px: nav `display:none`, page overflow 745 px (Overview) / 112 px -> menu button with 5-link panel, overflow 0 on Overview, Models, Config, Sessions; 640 px css: 419 px -> 0; header badge right edge 308 px. Root cause of the page-level overflow was absolute `sr-only` spans escaping the unpositioned scroll region (`d243858`). |
| E-25 | fixed `71071e3` | bars/dots vanished under forced-colors -> bordered, `CanvasText` fill under `@media (forced-colors: active)` (screenshot `/tmp/expl/r1-forced-colors-drawer.png` shows bar, dots, ring); sidebar is sticky (top 64 px after a 600 px scroll). Residual: only emulated forced-colors (light) in headless Chrome. |
| E-26 | fixed `6419f2d` | term and definition now separated by ` - `. |
| E-28 | fixed `6e4d253` | VERIFIED defect: status region empty after Refresh. Now `Refreshing models` then `Refresh finished: 2 of 3 upstreams ok` (browser: finished message present 50 ms after click); pinned warning has a visible `Warning` label (spec; the live run had no warnings so the label was not seen in the browser). |

Not done / residual: no screen-reader pass (VoiceOver/NVDA); chart canvas animation under reduce checked by spec only; forced-colors dark scheme and real Windows High Contrast not tried; dimmed-row contrast is spec-measured, not re-sampled in the browser; Karma now runs on `@angular/build:karma` (the webpack builder never applied Tailwind v4, so earlier specs saw unstyled DOM).

## Backlog (Story 5.1.4a)

The stapler-squad MCP was unavailable on 2026-10-12, so this section is the backlog. Severity: High = S1, Med = S2, Low = S3 (Story 4.1.0 scale), or Low/Med/High risk for non-gap items. Per requirements M5 only Low rows may be plain backlog among gaps; E-19 and E-21 are Med and carry an explicit reason (they need work outside this project).

| ID | Title | Sev | Reason | Suggested follow-up |
|----|-------|-----|--------|---------------------|
| B-01 | Mid-stream abort recorded as 200; streamed total and first-byte (E-19) | Med | Needs the stream tee from deferred Epic 6; cannot be reported honestly from header time | Do in Epic 6 Story 6.1.5 plus a new task: record abort status and stream-end time |
| B-02 | Epic 6 as a whole: compression wiring and real first-byte tee (Stories 6.1.1-6.1.6, ADR-005) | Med | Deferred by Tyler 2026-10-10; own PR set and validation. Until then `tokensAfter` is `n/a (compression off)` and first byte is `n/a (not streamed)` / `n/a (not measured)`; KPI cards for tokens/s, TTFT and savings stay hidden or n/a | Re-baseline the 9.9-day estimate and start a follow-up project |
| B-03 | Malformed-JSON 400 produces no trace (E-21) | Med | Axum extractor rejects before the handler, so `TraceScope::begin` never runs | Add a pre-handler rejection hook or custom `Json` extractor that records a 400 trace |
| B-04 | Trace table is below the fold at 1440x900 (E-22) | Low | Layout decision (KPI cards and chart first); skip link mitigates the keyboard path | Tyler decides: move the table above the chart or collapse KPI cards |
| B-05 | Legacy `GET /api/models` returns the raw upstream error body (E-27) | Low | Callers still use the unsanitized path; the catalog endpoint sanitizes | Migrate `/api/models` callers to the catalog and share its error classifier |
| B-06 | First-token time (distinct from first byte) | Low | First byte from Story 6.1.5 is the first stream byte; `message_start` precedes any token | Parse the first content delta in the tee once Epic 6 lands |
| B-07 | Decision R1: no CORS or Origin check on `POST /api/model-catalog/refresh` | Low | Accepted risk (plan Decision R1) | Revisit with B-08 if the dashboard ever binds beyond loopback |
| B-08 | Tighten dashboard CORS policy or add a dashboard token | Med | Bind/auth/CORS stance is documented, not changed in this project | Own ADR: token or same-origin-only policy for `/v1/dashboard/*` and `/api/*` |
| B-09 | Route hot-swap does not mark surviving catalog entries Stale | Low | Hot-swap prunes entries but does not mark the rest Stale when upstream config changes | Mark entries Stale when an upstream's base URL or auth changes, and refresh |
| B-10 | Anthropic 529 is recorded as `rate_limited` / 429 in attempts | Low | Production maps 529 to 429 in the attempt record and the e2e test asserts that | Add an `overloaded` attempt outcome and update the e2e assertion |
| B-11 | Preview Diff shows duplicate snake_case keys in the Config Editor (E-16 residual) | Low | E-16: Preview Diff compares an active config with duplicate snake_case keys against a draft without them | Normalize keys before diffing in `config-editor` |
| B-12 | Copy precedes Inspect in tab order (E-12 residual) | Low | Skip link brought the path to 5 key presses (E-12 fix); the Copy button still comes first in the row | Put Inspect first in the row's tab order via DOM order or roving tabindex |
| B-13 | Screen-reader pass (VoiceOver/NVDA) not performed | Med | No assistive-technology driver in the headless session; only roles and aria attributes were read | Manual VoiceOver pass on Overview, drawer, Models, Config |
| B-14 | Usage validation waived; riskiest assumption unvalidated | Med | Epic 1 day-of-use check waived 2026-10-10; no operator usage evidence exists | Tyler uses the table and drawer for a working day and fills the Epic 1 validation table |
| B-15 | `tests/trace_model_mapping_e2e.rs` writes to the real `~/.claude/consolette/response-cache.sqlite` | Low | The test does not isolate HOME; it nonces bodies so stale entries cannot cause hits (`1baa29e`) | Point the test's response cache at a temp dir |
| B-16 | Karma builder switched to `@angular/build:karma` (`ui/angular.json`) | Low | Checked: `ui/BUILD.bazel` only runs `ng build` through `bazel_ng_build.mjs` (application builder, unchanged) and no Bazel test target references Karma, so Bazel is unaffected today; `@angular/build` is a transitive dependency of `@angular-devkit/build-angular` (lockfile has it). Bazel was not run | If a Bazel UI test target is added, use `@angular/build:karma`; consider declaring `@angular/build` explicitly in `ui/package.json` |
| B-17 | `src/routing/router/mod.rs` is exactly 1130 lines | Low | Zero headroom against its line-cap test; any addition fails it | Extract another submodule before the next router change |
| B-18 | Flaky test `providers::openrouter::cache::tests::snapshot_should_return_none_after_ttl_expires` | Low | Time-based under load | Use a fake clock or `tokio::time::pause` |
| B-19 | Flaky test `routing::model_catalog::refresh_tests::refresh_failure_should_emit_one_warn...` | Low | Fails under load; passes alone | Capture the warn deterministically, not by wall clock |
| B-20 | Flaky test `per_model_retry::dispatch_should_retry_sibling_free_model_after_per_model_rate_limit` (failed once under load) | Low | Timing sensitivity | Use paused time or widen the margin |
| B-21 | 2.5 s real-time hold test has about 100 ms margin | Low | Real-time sleep in the table-hold spec can flake on a loaded CI runner | Use Angular `fakeAsync` / jasmine clock |
| B-22 | Mid-stream failure status | Low | Status stays "at headers" for streams (documented in the Column Truth Table) | Needs Epic 6 tee |
| B-23 | In-progress start event | Low | Rows appear only on completion | Emit a start event once the tee exists |
| B-24 | Compression for `count_tokens` and other endpoints; `session_compaction` / `system_prompt` token savings | Low | Different features from request compression | Separate follow-ups after Epic 6 |
| B-25 | Cost-tracker `RequestId` vs trace `requestId` duality | Low | Two id spaces for one request | Unify when touching the cost tracker |
| B-26 | Pin/unpin UI, per-upstream model pin in Config UI, trace filter/search, deep link from drawer to Config Editor | Low | Feature additions outside this plan's scope | Product backlog |
| B-27 | Merge local discovery into the model catalog | Low | Local models appear only through discovery | Catalog follow-up |
| B-28 | Move `DashboardEvent` / `RequestTraceData` into a dependency-free `events` module | Low | Would cut the `metrics -> entrypoint` edge (architecture review C1c) | Refactor PR |
| B-29 | Flatten `Routed { model, source }` if not done in Epic 2 | Low | Cosmetic API shape | Check when next touching the wire type |
| B-30 | Original-body store has no per-entry size cap | Low | Existing behavior, not changed here | Add a cap and a test |

## Final disposition summary (Story 5.1.4a)

Counts below are derived from the gap table by command, not by hand (see the commit that added this section for the command).

Command: `sed -n '/^### Gap table/,/^Not gaps/p' gap-report.md | grep '^| E-'` filtered with `grep -c`.

| Disposition | Count |
|-------------|-------|
| Gaps total (E-01 to E-28) | 28 (14 S1, 8 S2, 6 S3) |
| fixed (commit SHA in the table) | 24 (14 of 14 S1, 6 of 8 S2, 4 of 6 S3) |
| backlog | 4 (E-19 and E-21 are S2 with the reasons above; E-22 and E-27 are S3) |
| Backlog items in the section above | 30 (B-01 to B-30) |

M5 cutoff: every S1 is fixed; the two S2 backlog rows (E-19, E-21) carry explicit reasons (Epic 6 dependency; pre-handler rejection hook); only S3 rows are plain backlog.

## Final gate (Story 5.1.4c), 2026-10-12

| Check | Result |
|-------|--------|
| `cargo fmt --check` | exit 0, no files flagged |
| `CONSOLETTE_REQUIRE_UI=1 cargo clippy --all-targets -- -D warnings` | clean |
| `CONSOLETTE_REQUIRE_UI=1 cargo test --no-fail-fast` | 1250 passed, 0 failed, 7 ignored |
| `cd ui && npx ng test --watch=false --browsers=ChromeHeadless` | 220 of 220 SUCCESS |
| `make ui-fresh-check` (`tests/ui_bundle_fresh.rs`) | 3 passed |
| `cargo run --bin readme-check` | README CLI reference and config example match |
| `npm run e2e` (Story 5.1.2 smoke, real proxy on :19100, fake upstreams, temp HOME) | SMOKE OK; first row `200 OK`, no `undefined`, drawer shows `Requested claude-sonnet-4`; servers killed, ports 19100-19103 free |

Quick wins (5.1.4b): none beyond E-23 and E-26 (already fixed). E-27 is not cheap: it needs the catalog's error classifier shared with the router path.
