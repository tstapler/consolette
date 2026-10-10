# UX Design: webui-trace-model-mapping

Inputs: `requirements.md`, `research/ux.md` (its repo facts are VERIFIED there; product comparisons are INFERRED). Wireframes are ASCII; widths are indicative.

Operator job: "Why did my request go to that model?" The design answers, in order: what was asked, what ran, why, and whether the target is real.

## 1. Surface inventory

| # | Surface | Type | Priority (cut order: last first) |
|---|---|---|---|
| S1 | Live Request Traces table (Overview) | Interactive | Must |
| S2 | Live-feed status banner + Pause/Resume | Interactive | Must (never cut: reconnecting banner in Story 1.1.4, disconnected banner + Reconnect + lag chip in Story 4.1.5 core, Pause/Resume + status region in Task 4.1.7c). All of Story 4.1.7 (focus ring, contrast, reduced motion, target size) and the UX-04 pinned-model block are also never cut |
| S3 | Inspect drawer (route, attempts, selectable attempt detail) | Interactive | Must |
| S4 | Upstream Models view (new route `/dashboard/models`) | Interactive | Must |
| S5 | Trace filter/search bar | Interactive | Deferred to backlog (plan Story 5.1.4); UX-33 deferred with it |
| S6 | Trace SSE/API contract (JSON sample) | Non-interactive | Must |
| S7 | Model-list refresh log lines | Non-interactive | Must |
| S8 | Exploratory-testing gap report | Non-interactive | Must |

Deep-link from drawer to Config Editor is out of the design (cut first per research). The router has no named-rule concept, so no rule name is shown anywhere.

## 2. Shared vocabulary

- Reason chips (fixed set, text not color; the one vocabulary from `implementation/plan.md` "Route reason and label maps" and ADR-004): wire `routeReason` -> chip `session_pin` -> `pinned`, `strategy` -> `strategy`, `exploration` -> `explore`, `fallback` -> `fallback`, `rate_limit_hold` -> `held`, `default` -> `default`, `cache` -> `cache`, `local` -> `local`; plus the status-driven `cancelled` chip for 499. There is no `alias`, `rule` or `no route` chip. One chip per row; full reason in the drawer.
- Status text = glyph + code + word: `ok 200 OK`, `! 429 Rate limited`, `x 502 Failed`. Sub-label `fell back` (final success on a different upstream than the first attempt) or `retried` (same upstream) when final success followed a failed attempt; derived in the UI from `attempts` and `statusCode`, no wire field.
- Missing value: an en dash whose reason is visible, not only sr-only, **without a tab stop per dash** (a 100-row table would otherwise gain hundreds): the dash has `title="n/a (reason)"` (or "not recorded") for hover; keyboard users read the same reason as visible text in the footer disclosure "What do these dashes and chips mean?" (lists every reason string with its meaning) and in the drawer's "Not recorded" block; screen readers hear the row's n/a reasons through an sr-only description attached to the row's Inspect button (`aria-describedby`). The footer legend reads "- = not applicable; hover a dash or open the legend for the reason". Never `unknown`, `0ms`, or `undefinedms`.
- Chip jargon: every chip has a `title` tooltip in plain language, and a "What do these dashes and chips mean?" disclosure in the table footer lists them (`held` = waited for a rate-limit cooldown, then retried; `default` = no special rule, the default route; `explore` = an occasional exploration pick to test another upstream; `pinned` = a session pinned this conversation to one upstream; `strategy`, `fallback`, `cache`, `local`, `cancelled` likewise; strings are in `implementation/plan.md` "Chip jargon legend").
- Upstream state words: `OK`, `ERROR`, `STALE`, `NOT SUPPORTED`, `LOADING`. Attempt error labels (from the sanitized `errorClass`): `auth failed`, `rate limited`, `overloaded`, `timed out`, `server error`, `unreachable`, `request rejected`, `bad response`, `shed, not tried`, `cancelled`; catalog refresh errors use `auth failed`, `timed out`, `rate limited`, `upstream error`, `not supported`, `failed`.
- Informational text uses neutral-400 or lighter (7.11:1 on neutral-900); neutral-500/600 and slate-500 are not used for information (research/ux.md section 0 contrast results). Minimum 11px for chips and headers.
- Terminology: the table column and the drawer both say "First byte" (time to the first byte of the SSE body, same origin as Latency). "TTFB" and "TTFT" are not used in the UI; first-token time is out of scope.
- Relative times (`3m ago`) always carry the absolute time in `<time datetime>` and `title`.

## 3. S1 Live Request Traces table

### Wireframe (connected, with data)

```
Live Request Traces                          [feed: * Live]  [ Pause ]
caption (sr-only): Live request traces
TIME      REQUEST ID     MODEL ROUTE                               LATENCY*  FIRST BYTE  TOKENS (SAVED)  STATUS            ACTIONS
14:02:11  a1b2c3d4 [cp]  claude-sonnet-4 -> anthropic/sonnet-4-5   480ms     910ms       4,102 (-38%)    ok 200 OK         [Inspect]
                         [strategy]
14:02:09  9f8e7d6c [cp]  gpt-4o -> openrouter/gpt-4o-mini          3650ms    3800ms      1,870           ok 200 OK         [Inspect]
                         [fallback] [x3 attempts]                                                       fell back
14:02:07  77ab01cd [cp]  claude-opus -> bedrock/sonnet-4-5         310ms     --          --              x 502 Failed      [Inspect]
                         [fallback] [x2 attempts]
14:02:05  5c5c5c5c [cp]  anthropic/claude-haiku                    220ms     640ms       900             ok 200 OK         [Inspect]
                         [default]
```

- **Scope note (Tyler, 2026-10-10)**: the wireframes show first-byte and tokens-after values as they will look after the deferred compression/first-byte follow-up (plan Epic 6). In this project every FIRST BYTE cell is `n/a (not streamed)` or `n/a (not measured)` and every tokens-after cell is `n/a (compression off)`; the `pending`, `ended before first byte` and `client closed before first byte` states are follow-up work, not built here.
- Column semantics (revised): `LATENCY*` is time to response headers (footnote), `FIRST BYTE` (footnote "from upstream send") is the first stream byte measured from the same origin as `LATENCY*` (final-attempt send), so it is never smaller than LATENCY*; time spent in earlier failed attempts or holds is in the drawer's `totalMs`. It reads `n/a (...)` per the plan's Column Truth Table for non-streamed, cache-hit, failed and compression-off rows (examples above showing `--` are rendered as an en dash with sr-only reason; until the deferred stream tee ships (not in this project) a streamed row reads `n/a (not measured)`); cache-hit rows show chip `[cache]`, local rows `[local]`, client-closed rows `499 Client closed` + `[cancelled]`.
- `[cp]` copies the full request ID; announces "Copied request ID" via the shared status region **only after the clipboard write succeeds**. Where `navigator.clipboard` is unavailable (non-secure context) it falls back to selecting the text and `execCommand('copy')`; if that fails the status region says "Copy failed - select the ID and copy it manually" and the ID is left selected. Never announces "Copied" on failure.
- Route cell line 1: `requested -> routed` when names differ; only the routed name when equal (provider on the same line, chips on line 2). Truncate with ellipsis; full names in `title` and in the drawer.
- Request ID is the row header (`<th scope="row">`), shows 8 characters; Inspect is the only row activator (no clickable `<tr>`). Inspect has `aria-label="Inspect trace a1b2c3d4"`.
- `x3 attempts` badge appears only when attempts > 1; activating it opens the drawer with the ATTEMPTS heading scrolled into view (focus still goes to the drawer heading).
- The table sits in one **focusable, named horizontal scroll region** (`role="region"`, `aria-label="Live request traces, scroll horizontally"`, `tabindex="0"`, focus ring) so that at 320 px width or 200% zoom the page itself does not scroll sideways (WCAG 1.4.10) and keyboard users can scroll the table.
- Footer (below the table): "Showing latest 100" when full; the n/a legend; the "What do these dashes and chips mean?" disclosure; and, while Epic 2 has not shipped, the coverage note "Rows cover requests that reach routing. Requests rejected before routing, cache hits and multi-round search requests are not shown as one row yet." (removed by Task 2.1.6f).
- Rows keyed by request ID; new rows insert at top without moving a focused row.

### Interaction flow

| Step | User | System |
|---|---|---|
| 1 | Opens `/dashboard/overview` | Skeleton rows (3, `aria-hidden`) + "Connecting to live feed..." until SSE connects |
| 2 | Sends a request through the proxy | Row appears within the SSE SLO (<100ms push), reason chip and status populated |
| 3 | Tabs to a row, presses Enter on Inspect | S3 opens, focus moves to drawer heading |
| 4 | Presses `[cp]` | Full ID to clipboard; "Copied request ID" announced |
| 4a | Tabs into the table (focus inside it) while traffic flows | **Auto-hold (UX-23)**: new rows are held, not inserted, so nothing moves under the focused control; "N new traces waiting" is announced (first immediately, then at most once per second) and a "Show N new" button appears; rows already shown update in place. Focus leaving the table (or "Show N new") inserts the held rows in one step and announces "N new traces" once |
| 5 | Presses Pause | Rows stop inserting; the button label swaps to Resume (a label swap only: **no `aria-pressed`**, so assistive tech does not announce a second "pressed" state); "N new traces waiting" is announced via the status region; Resume flushes them |
| Exit | Navigates to another page / closes drawer | Table state is not lost; SSE service stays subscribed |

### States

| State | Display | Exit path |
|---|---|---|
| Loading (SSE connecting, no data) | 3 skeleton rows + "Connecting to live feed..." | Auto-resolves; if >10s see S2 disconnected |
| Empty (connected, zero traces) | "No requests yet. Point a client at this proxy to see traces." followed by the proxy URL (`<origin>`, the page's own origin) and a "Copy URL" button; never mentions the stream | Send traffic; row appears |
| Populated | As wireframe | -- |
| Stale (reconnecting/disconnected) | Rows kept, dimmed with a color token (never `opacity`) so every text node still meets 4.5:1 (UX-24), banner from S2 | Reconnect button / auto-retry |
| Held (focus inside table, or manual Pause) | Held rows counted in the status region and the "Show N new" button (auto) or the Resume label (manual); manual Pause outlasts a focus-out | Show N new / Resume |
| Error (malformed event) | Row rendered with en dashes for bad fields; row still inspectable; one console warning, no toast spam | Inspect still works |
| Missing field | En dash + sr-only "not recorded" | -- |
| Lagging (`system_lag` event) | Chip "feed lagging - N events skipped" (N = the event's `skipped` count; there is no millisecond value) next to the feed pill | Clears on next event without lag |
| Overflow | Table holds last 100 (ring-buffer size); footer "Showing latest 100" | -- |

## 4. S2 Live-feed status banner + Pause/Resume

```
[ ! Live feed interrupted - reconnecting (attempt 3). Showing last 42 traces from 14:02:11. ]   role="status"
[ x Live feed disconnected. Data below is stale.                         [ Reconnect ]       ]   role="alert"
```

| State | Display | Exit path |
|---|---|---|
| connected | Pill "Live" (dot + text) | -- |
| reconnecting | Amber banner with attempt count and last-trace time | Auto; or user presses Pause (no effect on retry) |
| disconnected | Rose banner, Reconnect button, rows dimmed | Reconnect button; if it fails again banner shows the failure and stays actionable |
| paused (user) | Pill "Paused - N waiting"; button reads Resume | Resume |

Reconnecting and disconnected are textually distinct everywhere the pill appears (Overview and app shell). Pulse/ping animation is `motion-safe:` only.

## 5. S3 Inspect drawer

Non-modal right-side panel (480-640px), `role="dialog"`, `aria-modal="false"`, `aria-labelledby` the trace heading. The table stays visible and usable. At 640 px viewport width or narrower the drawer becomes a full-width sheet (no horizontal scroll down to 320 px, WCAG 1.4.10 reflow; the table is not usable behind it, so Esc/close is always visible). **Focus at <= 640 px**: the sheet is modal (`aria-modal="true"`, the page behind it `inert`), Tab and Shift+Tab cycle inside it, Esc closes it and returns focus to the originating Inspect button (or the table element if that row left the buffer). **Above 640 px** the drawer stays non-modal: Tab from its last control continues to the rest of the page (no trap), and Esc closes it from any control inside. All drawer and table controls have a hit area of at least 24x24 CSS px (WCAG 2.5.8).

### Wireframe

```
+-- Trace 9f8e7d6c-1a2b-... [cp] ---------------------------- [x close] --+
| ok 200 OK (fell back)   14:02:09   Total 4120ms   First byte 3800ms     |
|                                                                         |
| ROUTE                                                                   |
|  Requested   gpt-4o                                                     |
|  Routed      openrouter / gpt-4o-mini                                   |
|  Model source pinned: openrouter's configured model replaces gpt-4o     |
|  Why         fallback - moved to the next candidate after anthropic     |
|              failed (overloaded, HTTP 529)                              |
|                                                                         |
| ATTEMPTS (3)                    0ms   1s    2s    3s    4s              |
|  1 anthropic/sonnet-4-5   x 529 overloaded    310ms |=====|             |
|  2 bedrock/sonnet-4-5     x 429 rate limited  120ms        |==|         |
|  3 openrouter/gpt-4o-mini ok 200             3650ms           |=======| |
|  summary (sr-only): 3 attempts; first two failed; third succeeded.      |
|  Selected: attempt 1 - overloaded - "Upstream overloaded (HTTP 529)"    |
|                                                                         |
| TOKENS   before 1,870 est.   after n/a (compression off)                |
| NOT RECORDED  (omitted when empty)                                      |
|  First byte: n/a (not measured)                                         |
+-------------------------------------------------------------------------+
```

- Attempt list is an ordered list/table; each row is a button (`aria-pressed`) that selects it; bars are `aria-hidden`; start, duration, outcome are text. Outcome glyph set is the one in `trace-timeline.component.ts` (ok, !, x, -). The first failed attempt is selected by default.
- Error detail per attempt is the sanitized `errorClass` label (see the label list in section 2) plus the backend's short fixed-phrase `errorMessage`; the upstream body is never captured, so there is nothing else to show, and never request prompt content or keys.
- **Why block** (all reasons, the same strings as the plan's "Route reason and label maps"): `Model source` is `pinned: <routedProvider>'s configured model replaces the client's <requested>` or `passthrough: the client's model name was sent unchanged to <routedProvider>`; "which pin" is the routed upstream and its model (the wire has no route/rule field, so none is shown). Why text: `session_pin` "pinned to this upstream by the session"; `strategy` "chosen by the <strategy> strategy"; `exploration` "exploration pick"; `fallback` "moved to the next candidate after <first failed attempt>"; `rate_limit_hold` "waited for a rate-limit cooldown, then retried (Nx)"; `default` "default route: no session pin, exploration or strategy decision applied"; `cache` "served from the response cache; no upstream was called"; `local` "served by a local model <routedModel>"; 499 "client closed the connection".
- **Not recorded** block: one visible line per n/a column with its exact reason string (keyboard-visible: this is where the dash reasons live for a keyboard user); omitted when the trace has none.
- Fallback to plain attempt list (no bars) is the agreed degradation if appetite slips.
- No Pipeline or Payload-diff tabs in this drawer (the plan does not embed the pipeline timeline; payload comparison stays in Session Replay).

### Interaction flow

| Step | User | System |
|---|---|---|
| 1 | Activates Inspect (or attempts badge) | Drawer opens; focus to heading; badge variant scrolls to ATTEMPTS |
| 2 | Reads ROUTE | Requested, routed, reason (chip text map in section 2; `strategy` adds the strategy name) |
| 3 | Selects attempt 2 (Enter/Space on its row button) | Detail pane under list shows the error class label and the short redacted message |
| 4 | Presses copy on ID | Copied, announced |
| 5 | Selects another table row's Inspect | Drawer content swaps in place, focus stays on drawer heading, the heading text changes and the status region announces "Showing trace <id8>" |
| Exit | Esc or close button | Drawer closes, focus returns to the originating Inspect button (or to the table if that row left the buffer) |

### States

| State | Display | Exit path |
|---|---|---|
| Evicted (never seen, e.g. deep link `?requestId=gone`) | "This trace is no longer retained (buffer holds the last 100)." | Close |
| Evicted while open | The drawer keeps showing a snapshot of the last data received, with the visible banner "No longer in live buffer - showing the last data received" (status region); focus does not move | Close |
| No attempt data (old event/uninstrumented path) | One implicit attempt row + "Attempt detail not captured" | -- |
| Attempts truncated | "Showing first N of M attempts" | -- |
| Single attempt success | ATTEMPTS shows one row; no fallback wording | -- |
| All attempts failed | Routed shows the last attempted upstream; status shows the real failed code; Why = the derived reason chip text plus "all N attempts failed" | -- |

Attempts are inline in the trace event (capped at 16), so the drawer has no loading or fetch-failed state; an unavailable trace is the Evicted state.

## 6. S4 Upstream Models view

New route `/dashboard/models` with nav entry "Models". Backed by the new `GET /api/model-catalog` (per-upstream state, ages, `refreshing` flag; ADR-003), not the legacy `GET /api/models`. **Opening the page never refreshes**: it shows the cached snapshot with each upstream's age, plus an explicit "Refresh now" button (all) and a Refresh button per upstream (no hidden refresh: a refresh can run credential helpers or paid calls, ADR-003). Refresh uses `POST /api/model-catalog/refresh` (all upstreams) or `POST /api/model-catalog/refresh?upstream=<name>` (one upstream); the POST returns the cached snapshot immediately with `refreshing: true`.

### Wireframe

```
Upstream models   [Refresh now]  [search models...]
-------------------------------------------------------------------------------
! Referenced by config but not listed upstream (1)                    pinned
   openai / gpt-5-turbo   route "default" upstream 0   not listed upstream (as of 12m ago)
-------------------------------------------------------------------------------
[v] anthropic   OK              42 models   refreshed 3m ago             [Refresh]
      claude-sonnet-4-5
      claude-3-opus
[v] bedrock     ERROR + STALE   auth failed (403)  last good 2h ago       [Retry]
      showing cached 31 models from 2h ago
[v] openrouter  OK              310 models  refreshed 12m ago  (paged, search)
[>] gemini      NOT SUPPORTED   upstream has no model-list endpoint
[>] vertex      NOT LOADED      Not loaded yet - press Refresh now          [Refresh]
```

- Provider sections are disclosure buttons with `aria-expanded`; search expands matching sections; large lists paged (200 rows, "Show more") with a visible count.
- Config cross-reference is the pinned warning block only. The block always shows the caveat "Only models pinned on the first route are checked. Models reached through other routes, model-family routes and fallback targets that live outside this route are not checked, so no warning here is not proof that every target exists." (the router uses the first route only and ignores the rest, `select_primary_route`, `src/routing/router/mod.rs:328`). The search input has an accessible name ("Search models"). At 320 px / 200% zoom each upstream section stacks (name, state, Refresh wrap) and long model ids wrap, with no horizontal scroll. Per-model "in config / not in config" markers, "used in rule/alias" annotations and an All / Config-only / Errors filter are backlog, not in this design.
- Stale threshold: 30 minutes in `on_demand` mode and 2 x `interval` in interval mode (ADR-003, `plan.md` Unresolved Questions; Tyler may change it).

### Interaction flow

| Step | User | System |
|---|---|---|
| 1 | Opens Models | One `GET /api/model-catalog` renders the cached snapshot at once with ages; never-refreshed upstreams read "Not loaded yet - press Refresh now"; **no POST is sent**. Help text discloses that refreshing runs each upstream's credential flow and, in one line, that the dashboard is reachable by any local process or page in the browser (loopback bind, no auth) |
| 2 | Types in search | Filters across providers, expands matches, announces "N matches" once typing pauses (status region) |
| 3 | Presses Refresh on one provider | `POST ...refresh?upstream=<name>`; that row's button is disabled with `aria-busy` and "Refreshing..." from the click until a `ModelCatalogChanged` event shows that upstream's `lastAttemptAt` advanced, or a later snapshot reports `refreshing: false` (not until the POST response, which returns at once); then new count/time. If a fan-out is already running the request coalesces onto it and the row says "Refresh already running" |
| 4 | Presses Refresh now (all) | `POST ...refresh`; all buttons busy until each row's `lastAttemptAt` advances or `refreshing` is `false`; each updates when done; partial failure leaves others updated |
| 5 | Reads pinned warning | Sees config entries missing upstream |
| Exit | Navigates away | Refreshes in flight continue server-side; no state lost |

### States

| State | Display | Exit path |
|---|---|---|
| First load, GET in flight | Per-upstream skeleton | Auto |
| Never refreshed | `NOT LOADED` row: "Not loaded yet - press Refresh now"; when all upstreams are in this state the page leads with one prominent Refresh now button | Refresh now |
| OK | Count + relative time | -- |
| Error with cache | `ERROR` + `STALE`, error kind label (auth failed / timed out / rate limited / upstream error), age, cached list visible | Retry |
| Error without cache | `ERROR`, error kind label, Retry; never "0 models" | Retry |
| Not supported | `NOT SUPPORTED` row with explanation | -- |
| Empty list (upstream returned 0, healthy) | "Upstream reports 0 models." (distinct from error) | Refresh |
| No upstreams configured | "No upstreams configured." + hint to the Config Editor page | Open Config Editor |
| Whole endpoint 5xx/offline | Page-level error "Could not load the model catalog (<status or offline>)" with a Retry button that refetches; SSE state independent | Retry |
| Search no match | "No models match 'x'." + Clear search | Clear search |
| Refresh in progress | As flow step 3; a coalesced request shows the row text "Refresh already running" | -- |

## 7. S5 Trace filter/search (deferred)

Deferred to the backlog (plan Story 5.1.4 "trace filter/search"); no story implements it, so UX-33 is deferred with it and the Overview wireframe has no filter control. When it is picked up, the intended shape is a search box over id/model/provider, Status and Reason selects, a `matches N of M` count announced via the status region, and a single Clear action.

## 8. Non-interactive surfaces

### S6 Trace event contract (JSON sample; field names are the plan's wire names)

```json
{"type":"RequestTrace","data":{"id":"9f8e7d6c-1a2b-4c3d-8e9f-0a1b2c3d4e5f","requestId":"9f8e7d6c-1a2b-4c3d-8e9f-0a1b2c3d4e5f",
 "requestedModel":"gpt-4o","routedModel":"gpt-4o-mini","routedModelSource":"pinned","routedProvider":"openrouter",
 "routeReason":"fallback","routeStrategy":null,
 "attempts":[
  {"iteration":0,"upstream":"anthropic","model":"sonnet-4-5","outcome":"upstream","status":529,"latencyMs":310,"errorClass":"overloaded","errorMessage":"Upstream overloaded (HTTP 529)"},
  {"iteration":0,"upstream":"bedrock","model":"sonnet-4-5","outcome":"rate_limited","status":429,"latencyMs":120,"errorClass":"rate_limited","errorMessage":"Upstream rate limit (HTTP 429)"},
  {"iteration":0,"upstream":"openrouter","model":"gpt-4o-mini","outcome":"ok","status":200,"latencyMs":3650,"errorClass":null,"errorMessage":null}],
 "attemptsTruncated":false,"iterations":1,
 "timeToHeadersMs":3650,"totalMs":4120,"firstByteMs":3800,"firstByteState":"measured","traceRevision":1,
 "tokensBefore":1870,"tokensAfter":1870,"compressionStatus":"none","stream":true,"traceKind":"routed","statusCode":200}}
```

Indicative (the `firstByteMs`, `firstByteState`, `traceRevision: 1` and `compressionStatus: "none"` values shown are what the deferred Epic 6 will send; in this project they are `null`, absent, `0` and `"off"`); the pinned source of truth is `ui/src/testing/fixtures/sse/request-trace-failure.json` (plan Story 2.1.1c), which replaces this sample once it exists.

- Every field the table renders is present or explicitly null (no silent omission leading to `unknown`).
- `statusCode` reflects the real final status, not a constant.
- Casing is camelCase (ADR-001); additive fields only (compatibility per requirements Risk Control).
- No prompt content or credentials in any field. Attempt errors are only the closed `errorClass` and the fixed-phrase `errorMessage`, never an upstream body.
- Attempts are inline (`attempts[]`, capped at 16, `attemptsTruncated`); there is no lazy fetch.
- `fell back` / `retried` has no wire field: the UI derives it from `attempts` and `statusCode`.
- `routeRule`, `attemptCount` and `finalStatus` do not exist (the count is `attempts.length`).

### S7 Model-list refresh log lines

```
INFO model_list_refresh upstream=bedrock outcome=ok model_count=31 duration_ms=812
WARN model_list_refresh upstream=bedrock outcome=error kind=auth status=403 duration_ms=240
```

- Exactly one structured line per refresh (INFO on success, WARN on failure, never both) with the field names above (the plan's Observability Plan); no keys, tokens, model ids, message text or URLs with credentials.
- `kind` uses the catalog vocabulary (`auth`, `timeout`, `rate_limited`, `upstream`, `unsupported`, `other`), which the Models view shows as the labels in section 2; attempt `errorClass` values are a separate vocabulary and are not logged here.
- "Trace-emission drops" means ring-buffer evictions (counted, logged at `debug`) and the `system_lag` event, which the UI shows as the skipped-event chip.

### S8 Exploratory gap report

- One entry per finding: page, element, expected vs actual, severity, disposition (fixed / backlog item id).
- Covers all four pages (Overview, Session Replay, Model Benchmarks, Config Editor) with live proxy traffic.
- Every finding has a disposition; none left blank.
- Stubbed or hardcoded UI (for example default pipeline steps) is listed explicitly.

## 9. UX acceptance criteria

Task completion
- UX-01: An operator can see requested model, routed model/provider and reason chip for a trace in the table without opening anything (0 clicks).
- UX-02: An operator can find why a request fell back (failed attempt, error class label, short message, latency) in at most 2 steps from the table: Inspect, read ATTEMPTS (the first failed attempt is selected by default).
- UX-03: An operator can copy a full request ID in 1 action from the table row and 1 from the drawer; "Copied" is announced only on success, and a failed or unavailable clipboard falls back to select-and-copy with a failure message.
- UX-04: An operator can determine whether a model pinned on the primary route is offered upstream in at most 2 steps from the Models page once the catalog has been refreshed (3 steps when it has never been refreshed: open, press Refresh now, read) (open page, read the pinned warning block; the page shows the last cached catalog, so the answer is as fresh as the age shown, and a Refresh now click makes it current). Must; its test and Story 3.1.6 are never cut.
- UX-05: An operator can refresh a single upstream without refreshing others (1 click).

Data correctness in the UI
- UX-06: For live traffic, every row shows a real request ID, latency, tokens and status, plus first-byte time for streamed requests and tokens-after when compression is enabled; every other cell shows an explicit `n/a (reason)` from the Column Truth Table in `implementation/plan.md` (success metric: 100% real-or-explained, from the 0 of 8 baseline); no cell displays `unknown`, `undefined`, `NaN` or a bare `0ms` for an absent value.
- UX-07: Absent values render as an en dash with sr-only text giving the reason (`n/a (not streamed)`, `n/a (compression off)`, `n/a (compression skipped)`, `n/a (compression failed)`, `n/a (client closed before first byte)`, `n/a (served from cache)`, `n/a (no response)`, `n/a (synthesized stream)`, `n/a (cancelled)`, `n/a (not measured)` for a streamed row before first-byte measurement exists); (follow-up, Epic 6: `pending` while a stream's first byte has not arrived, and `n/a (stream ended before first byte)` if the stream ends, errors or is dropped first (a streamed row never stays `pending`); the two terminal no-byte states are distinct: `n/a (stream ended before first byte)` (the upstream stream ended or errored) and `n/a (client closed before first byte)` (the client went away).) A late lower-`traceRevision` event never overwrites a row (applies now, to the Story 4.1.6 backfill merge).
- UX-08: A trace with failed attempts then success shows `fell back` or `retried` in the status cell and `xN attempts` badge; failed final status shows the real code, not 200.
- UX-09: When requested and routed names are identical, the route cell shows one name, not `a -> a`.

States and exits
- UX-10: Empty-and-connected shows "No requests yet..." with the proxy URL; it never blames the stream.
- UX-11: Reconnecting and disconnected are visually and textually distinct, both keep the last rows, and disconnected offers a Reconnect button.
- UX-12: Every error state in S1-S5 names the error and offers at least one action (Retry, Reconnect, Clear, Close); no dead ends.
- UX-13: An evicted trace in the drawer shows "no longer retained (buffer holds the last 100)" and a Close action.
- UX-14: An upstream with an error and a cached list shows STALE, the age, and the cached list; an upstream with an error and no cache never shows "0 models".
- UX-15: A provider without a model-list endpoint shows NOT SUPPORTED with an explanation, distinct from an empty (0 models) healthy result.
- UX-16: A `system_lag` event shows a "feed lagging" chip that clears when lag ends.
- UX-17: Loading states exist for the table (3 skeleton rows until the SSE connects) and for the Models page (one skeleton per upstream while the single GET is in flight; a never-refreshed upstream is the distinct `NOT LOADED` state, not a loading state). The drawer has no loading row: attempts are inline in the event, and a trace that is not in the store is the Evicted state. Each state resolves to data or an error within the app's request timeout.

Accessibility
- UX-18: All interactive controls (Inspect, copy, Pause, Show N new, Reconnect, Refresh, section toggles, model search, attempt rows, chip and dash legend disclosure, drawer close, the table scroll region) are reachable and operable by keyboard alone, with a visible `focus-visible` ring of at least 2 px and 3:1 contrast. Must; never cut.
- UX-19: Each Inspect button has a unique accessible name containing the short request ID.
- UX-20: Opening the drawer moves focus to its heading; Esc closes it and returns focus to the originating Inspect button.
- UX-21: The table has a caption and Request ID cells are row headers; no `aria-live` region wraps the table.
- UX-22: A single polite status region announces throttled summaries ("N new traces", "Copied request ID", and "N matches" for the Models search); only disconnected uses `role="alert"`.
- UX-23: Pause stops row insertion (the visible label swaps Pause/Resume; there is no `aria-pressed`, so state is signalled once) and Resume flushes the waiting rows. **Mechanism**: while keyboard focus is inside the table, new rows are auto-held (updates to shown rows apply in place), "N new traces waiting" is announced (at most once per second) with a "Show N new" button, and the held rows are inserted in one step when focus leaves the table or the button is used; Pause stays manual and outlasts a focus-out. No row moves under a focused element in any case.
- UX-24: All informational text has contrast of at least 4.5:1 against its background (no neutral-500/600 or slate-500 for information; checked by class grep and by a computed-contrast spec), **including rows dimmed in the reconnecting/disconnected/paused states (dimmed with a color token, not `opacity`)**; chips and headers are at least 11px. Must; never cut.
- UX-25: Status, reason, attempt outcome and upstream state are conveyed by text plus glyph, never color alone.
- UX-26: The attempt waterfall is readable by a screen reader through text values and the sr-only summary; bars are `aria-hidden`.
- UX-27: Pulse/ping animations are disabled under `prefers-reduced-motion`. Must; never cut.
- UX-28: Provider sections expose `aria-expanded`; Refresh in progress exposes `aria-busy`.

Non-interactive and process
- UX-29: The trace event contains no prompt content, API keys or upstream error bodies; attempts carry only `errorClass` and a fixed-phrase `errorMessage` (checked with a sentinel secret in an upstream error body, by inspecting a captured event and the drawer for a failed attempt).
- UX-30: Each model-list refresh emits exactly one structured log line (`upstream`, `outcome`, `model_count` or `kind` + `status`, `duration_ms`) and no secrets.
- UX-31: The exploratory report lists every finding with severity and a disposition (fixed or backlog id) across all four pages.
- UX-32: When the 100-entry buffer is full, the table shows only the latest 100 and labels it "Showing latest 100".
- UX-34 (visible explanations, no per-cell tab stops): Every `n/a (reason)` dash shows its reason on hover (`title`); keyboard users read it as visible text in the footer disclosure "What do these dashes and chips mean?" (every reason string) and in the drawer's "Not recorded" block; screen readers get it through a row-level `aria-describedby` on the row's Inspect button; dashes are **not** tab stops. Every chip has a plain-language tooltip and the footer legend lists them.
- UX-35 (no hidden refresh): Opening the Models page sends no POST; upstream refresh (credential flows, paid calls) happens only after a click on a Refresh button.
- UX-36 (reflow and targets): At 640 px or narrower the drawer is a full-width sheet with no horizontal scroll at 320 px, and every interactive control has a hit area of at least 24x24 CSS px. At 320 px (and 200% zoom) the Overview and Models pages have no page-level horizontal scroll: the trace table scrolls inside a focusable, named region and Models rows stack and wrap.
- UX-37 (Recent errors): The Recent errors list reads "No errors since this page loaded." when empty and "Showing latest 20" when it overflows; it is not an `aria-live` region.
- UX-38 (drawer keeps its data): A drawer open on a trace that is then evicted from the 100-row buffer keeps showing the last data received under the banner "No longer in live buffer - showing the last data received"; it does not blank or move focus.
- UX-39 (sheet focus): At 640 px or narrower the drawer sheet is modal (`aria-modal="true"`, page `inert`, Tab cycles inside, Esc closes and restores focus); above 640 px it is non-modal and Tab can leave it.
- UX-40 (forced colors and non-text contrast): Status, chips, the focus ring, glyphs and waterfall bars meet 3:1 non-text contrast (WCAG 1.4.11) and remain visible under forced colors (nothing distinguished by color alone, no `forced-color-adjust: none`); the Models search has an accessible name.
- UX-41 (pinned-warning caveat): The Models pinned-warning block always states that only models pinned on the first route are checked and that other routes, model-family routes and fallback targets outside it are not.
- UX-33 (deferred with S5, no story and no test until the filter bar is built): With filters active, the count "matches N of M" is correct and Clear restores all rows in one action.

## 10. Decided in planning (was: unresolved)

1. "Requested" is the raw client model (ADR-004). There is no `alias` chip; whether the routed model is a pin or a passthrough is `routedModelSource` (`pinned` / `passthrough`).
2. Attempts are inline in the event (capped at 16); no lazy fetch.
3. Stale threshold: 30 min in `on_demand` mode, 2 x interval in interval mode; refresh interval is a product default and applies if Tyler is silent.
4. Field casing is camelCase (ADR-001); a live frame capture (plan Task 1.1.1a) confirms the snake_case baseline.
5. S5 filter/search is deferred (UX-33 with it); the Config Editor deep-link is excluded.
6. Plan-only UI additions with no requirement of their own, kept under the generic "fix highest-value gaps" scope: the "N search rounds" badge and "Round N" grouping in the drawer (emulation loops), the Recent errors list (Story 4.1.5), and the `GET /v1/dashboard/traces` backfill (Story 4.1.6).
