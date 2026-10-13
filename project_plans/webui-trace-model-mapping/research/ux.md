# UX Research: requested-vs-routed model, attempt chains, upstream models

Labels: VERIFIED = opened/ran in this session; INFERRED = from prior knowledge of the named products, not re-checked online (web access was not used; treat product claims as INFERRED).

## 0. Repo facts (VERIFIED)

- Trace table columns and cells: `ui/src/app/features/overview/components/recent-trace-table.component.ts:25-32` (headers), `:44-49` (provider + one model chip), `:51-56` (duration/TTFT), `:63-69` (status badge), `:72-77` (Inspect button, no `aria-label`, no row context).
- Empty state is one sentence, no distinction between "SSE down" and "no traffic yet": `recent-trace-table.component.ts:80-84`.
- Short ID is cut to 8 chars + "..." with no copy affordance: `:101-104`.
- Status is a colored pill that also contains the numeric code (so not color-only): `:63-69`, `:123-137`. Success is the only state shown; no "retried"/"fell back" state exists in `RequestTraceData` (`ui/src/app/core/models/telemetry.models.ts:12-24`).
- `trace-timeline.component.ts:4-9, 61-69` is a horizontal stepper of pipeline stages with hardcoded default steps (Ingress, RateLimit, ...). It is a pipeline view, not an attempt view; its status icons use glyphs (check, !, x, -) in addition to color (`:44-47`), a good pattern to reuse.
- No `role=`/`aria-` attributes anywhere under `ui/src/app` except `scope="col"` (grep over `*.ts`; the only hits were SSE/status code). No `aria-live`, no focus management.
- SSE state already exists as `'connected' | 'reconnecting' | 'disconnected'` (`core/services/sse.service.ts:5`), rendered as a pill in `app-shell.component.ts:34-46` and `overview-dashboard.component.ts:32-34`. The overview pill treats reconnecting and disconnected identically (amber).
- Theme: `ui/src/styles.css:5-12` defines tokens, but components use Tailwind neutral/slate classes directly rather than the tokens.
- Contrast (computed with a WCAG luminance script against neutral-900 `#171717`, the card background): neutral-400 7.11:1, neutral-500 3.78:1 (FAILS AA for normal text), neutral-600 2.29:1 (fails), cyan-400 9.92, emerald-400 9.33, amber-400 10.74, rose-400 6.66. Slate-500 on `#090d16` = 4.08 (fails). The empty-state text (`text-neutral-500`, `:81`) and the 10px `text-[10px]` table headers on neutral-950 in neutral-400 pass on color but 10px is below comfortable size.

## 1. Comparable patterns

All INFERRED (from product knowledge, not re-fetched).

| Product | Requested vs routed | Reason | Attempt chain | Takeaway |
|---|---|---|---|---|
| LiteLLM proxy (Logs / Request view) | Shows `model` (alias) and the resolved `model_id`/`api_base` in row + detail | Fallback shown via metadata (`fallbacks`, `model_group`), mostly in raw JSON | Retries appear as separate spend-log rows or metadata, not a grouped chain | Raw JSON detail works for power users but chain is hard to see; avoid |
| Helicone | Model column shows resolved model; request body holds the requested one | Little explicit reason | Gateway fallbacks are logged as separate requests linked by session/properties | Linking attempts via a shared ID is cheap; grouping is what is missing |
| Portkey | Logs show config applied, "strategy" (fallback/loadbalance) and per-target attempts | Strategy name + target index | Per-request "trace" lists each attempted target with status | Closest to what we want: strategy label + ordered target list in detail |
| OpenRouter Activity | Row shows the model the user asked for; detail shows provider actually served, with "fallbacks" noted | Provider routing/fallback flagged | Single row, provider listed; failed provider attempts are summarized | Single row per user request with a small "via provider X" is the right density |
| Langfuse | Generation observation shows `model` (as called); nested observations for retries | N/A | Tree/timeline of nested observations with latency bars | Waterfall + nested children is the idiom operators already know |
| Datadog APM waterfall | Span per attempt; error spans flagged red with icon; span tags (`http.status_code`, `retry`) | Tags | Waterfall bars on a shared time axis, parent/child indentation | Gold standard for attempt chains; steal the shared time axis and per-span tag list |

What works and why (INFERRED):
1. One row per client request, attempts nested in detail. Per-attempt rows in the main table bury the question "what happened to my request".
2. Show requested model only when it differs from routed (OpenRouter/Portkey style); identical names collapse to one chip, which keeps the common case quiet.
3. Reason as a short enum chip, details on demand (Portkey strategy). Free text in the cell does not scale.
4. Waterfall with shared time axis makes retry delay and wasted time visible, which is the operator's real cost question.

## 2. Proposed layouts

### 2.1 Trace row (compact)

Columns become: TIME | REQUEST ID (copy) | MODEL ROUTE | DURATION | TTFT | TOKENS (SAVED) | STATUS | ACTIONS.

```
TIME      REQUEST ID    MODEL ROUTE                                      DURATION  TTFT   TOKENS (SAVED)  STATUS        ACTIONS
14:02:11  a1b2c3d4 [c]  claude-sonnet-4  ->  anthropic / sonnet-4-5      2310ms   480ms  4,102 (-38%)    200 OK        [Inspect]
                        [alias]
14:02:09  9f8e7d6c [c]  gpt-4o -> openrouter / gpt-4o-mini               4120ms   910ms  1,870           200 OK        [Inspect]
                        [fallback] [x3 attempts]                                                         retried
14:02:07  77ab01cd [c]  claude-opus -> (none)                              310ms    --    --              502 Failed    [Inspect]
                        [no route] [x2 attempts]                                                         
14:02:05  5c5c5c5c [c]  claude-haiku  (same as requested)                  1100ms   220ms  900             200 OK        [Inspect]
                        anthropic / claude-haiku
```

Rules:
- Cell is two lines: line 1 `requested -> routed` (omit `requested ->` and arrow if equal, show provider on line 2 instead); line 2 holds the chips. Truncate with ellipsis and `title` / tooltip carrying full names; do not truncate in the drawer.
- Reason chip text (not color alone): `alias`, `strategy`, `rule`, `pinned`, `fallback`, `explore`, `default`. Fixed vocabulary, one chip max in the row; the full reason (rule name, pin age) goes in the drawer.
- Attempt badge `x3 attempts` only when attempts > 1. Clicking the badge opens the drawer scrolled to the waterfall.
- Status column text combines code and a word (`200 OK`, `429 Rate limited`, `502 Failed`) plus a glyph; add a `retried`/`fell back` sub-label when final success followed a failed attempt. This needs a final-status distinct from the hardcoded 200 (see requirements.md baseline).
- Missing values render as an en dash with a visually-hidden "not recorded" text, not blank or the string `unknown`, and never `0ms` for absent TTFT (the current `{{ trace.firstByteMs }}ms` at `:55` prints "undefinedms"-style output when the field is missing).

### 2.2 Inspect drawer

Right-side panel (about 480-640px), opened by Inspect, replaces nothing on the page.

```
+-- Trace a1b2c3d4-... [copy] -------------------------- [x close] --+
| 200 OK   14:02:09   Total 4120ms   TTFT 910ms                      |
|                                                                    |
| ROUTE                                                              |
|  Requested   gpt-4o                                                |
|  Routed      openrouter / gpt-4o-mini                              |
|  Why         fallback: primary anthropic failed (529), rule        |
|              "gpt-family" -> next candidate                        |
|                                                                    |
| ATTEMPTS (3)                      0ms    1s     2s     3s     4s   |
|  1 anthropic / sonnet-4-5  [x 529 overloaded]  |=====|             |
|  2 bedrock  / sonnet-4-5   [x 429 throttled]        |==|          |
|  3 openrouter / gpt-4o-mini [ok 200]                   |=========| |
|     backoff 200ms between 1 and 2 shown as hatched gap            |
|                                                                    |
| TOKENS   before 1,870  after 1,870  saved 0                        |
| [ Pipeline timeline ]  [ Payload diff ]   (existing components)    |
+--------------------------------------------------------------------+
```

- Each attempt row: ordinal, provider/model, outcome glyph + code + short error class, bar on a shared time axis (Datadog/Langfuse idiom). Selecting an attempt shows error body (redacted; requirements.md security constraint) below.
- The existing `trace-timeline` stepper (`trace-timeline.component.ts`) and payload diff viewer remain as tabs; the stepper currently uses hardcoded defaults (`:62-69`), so show it only when real step data exists, otherwise hide rather than display fake durations. (VERIFIED defaults exist; INFERRED that no caller supplies real steps - check callers.)
- Attempt cap: ring-buffer bounded; if attempts were truncated show "showing first N of M".

### 2.3 Upstream models view (new route, e.g. `/dashboard/models` or tab on Overview)

```
Upstream models                       [Refresh all]  [search models...]  [Filter: All | Config-only | Errors]
-----------------------------------------------------------------------------------------
anthropic   OK  42 models   refreshed 3m ago                 [Refresh] [v]
  claude-sonnet-4-5      in config   (used by: rule gpt-family, alias sonnet)
  claude-haiku-4-5       in config
  claude-3-opus          not in config
bedrock     ERROR  auth failed (403)  last good 2h ago  STALE   [Retry] [v]
  (showing cached 31 models from 2h ago)
openrouter  OK  310 models   refreshed 12m ago   [Refresh] [v]
gemini      NOT SUPPORTED  upstream has no model-list endpoint   [v]
-----------------------------------------------------------------------------------------
Referenced by config but NOT listed upstream (3)
  ! openai / gpt-5-turbo   referenced in: rule "premium", alias "auto"   -- not offered by openai as of 12m ago
```

- Per-upstream header: name, state word (OK / ERROR / STALE / NOT SUPPORTED / LOADING), model count, relative refresh time with absolute time in `title`/`<time datetime>`, refresh button.
- Collapsible provider sections (disclosure buttons with `aria-expanded`); large lists (OpenRouter) virtualized or paginated with search; search matches across providers and expands matching sections.
- "Config but not upstream" is the highest-value diagnostic here (typo, deprecated model): show as its own pinned warning block and as a `!` marker inline in both places; mark with text "not listed upstream", not color alone.
- Providers with no list API (some, per requirements.md Feasibility Risks) get an explicit NOT SUPPORTED row so absence is not read as empty/broken.
- A stale-but-present list is kept visible with an age warning rather than blanked on error.

## 3. Accessibility

Findings are VERIFIED where cited, otherwise recommendations.

- Contrast: replace `text-neutral-500`/`text-slate-500`/`text-neutral-600` for any informational text (3.78, 4.08, 2.29:1, computed). Use neutral-400 (7.11) minimum; for disabled/decorative only is neutral-500 acceptable. Chips: cyan-400, emerald-400, amber-400, rose-400 all pass on neutral-900 (computed). Chip borders (`border-neutral-700`) are decorative; text carries the meaning, so 3:1 non-text contrast is not required for them, but focus rings must be 3:1.
- Minimum size: `text-[10px]` headers/chips (`:23, :46, :65`) are small; raise to 11-12px for chips.
- Not color-only: status always includes code + word + glyph; reason chips are text; attempt outcome has glyph (reuse the check/!/x/- set from `trace-timeline.component.ts:44-47`); stale/error upstream states carry a word label. Connection pill already has text (`app-shell.component.ts:46`) but the overview pill varies only color (`overview-dashboard.component.ts:32-34`) - add text.
- Table semantics: keep native `<table>`; add `<caption class="sr-only">Live request traces</caption>`. Row header: make Request ID a `<th scope="row">`.
- Live region: do NOT put `aria-live` on the whole table (it would announce every row at proxy-traffic rates). Instead use one visually-hidden `role="status"` (polite) that reports throttled summaries ("12 new traces", "Live updates paused") and `role="alert"` only for SSE disconnected. Provide a Pause/Resume toggle (`aria-pressed`) so rows stop shifting under the cursor/focus; this also serves WCAG 2.2.2 (pause moving content).
- Keyboard: Inspect is a real `<button>` (good), but needs `aria-label="Inspect trace a1b2c3d4"` since eight identical "Inspect" buttons are ambiguous (`:72-77`). Also make the whole row activatable with Enter via the button only (avoid clickable `<tr>`). Provide visible `:focus-visible` ring (Tailwind resets may remove it; check). Keep focus stable when new rows are prepended: key by `trackById` (already used, `:139-141`) and insert without moving the focused row.
- Drawer: `role="dialog"` (non-modal, `aria-modal="false"`) or `complementary` with `aria-labelledby` the trace heading; move focus to the drawer heading on open, Esc closes, return focus to the originating Inspect button; if modal, trap focus. A non-modal drawer lets the user keep the table visible, which suits comparison.
- Waterfall: render as an ordered list/table, with bars as decorative (`aria-hidden`) and the numbers (start, duration, outcome) as text, so screen readers get the same data. Add a visually-hidden summary: "3 attempts; first two failed; third succeeded in 2.1s".
- Reduced motion: `animate-pulse`/`animate-ping` on the connection dot (`app-shell.component.ts:39-40`) should respect `prefers-reduced-motion` (Tailwind `motion-safe:`).
- Truncated model names: expose full name via `title` plus a copy button; `title` alone is not keyboard accessible, so the drawer is the accessible full-detail path.

## 4. Empty / loading / error / stale states

| Surface | State | Display |
|---|---|---|
| Trace table | SSE connecting, no data | Skeleton rows (3) + "Connecting to live feed..." |
| | Connected, zero traces | "No requests yet. Point a client at `http://<host>:<port>` to see traces." (current text at `:81-82` blames the stream; wrong when connected) |
| | Reconnecting | Banner above table: "Live feed interrupted - reconnecting (attempt n). Showing last N traces from HH:MM:SS." Rows kept, dimmed slightly, not removed. `role="status"` |
| | Disconnected (retries exhausted) | Banner `role="alert"`: "Live feed disconnected. Data below is stale." + a Reconnect button. Distinguish from reconnecting (today both are amber on Overview) |
| | `system_lag` event | Small chip "feed lagging (n ms)"; traces may have been dropped |
| | Missing field | En dash + sr-only "not recorded" |
| Drawer | Trace evicted from ring buffer | "This trace is no longer retained (buffer holds last 100)." |
| | No attempt data (old event/ no instrumentation) | Show single implicit attempt and note "Attempt detail not captured" |
| | Payload not captured | Existing payload viewer empty state with reason |
| Upstream models | First load | Per-upstream skeleton + "Loading..." |
| | Upstream error, cached list exists | STALE badge, age, error class (auth/timeout/5xx), list shown |
| | Upstream error, no cache | ERROR state with error class and a Retry button; never show "0 models" |
| | Not supported | NOT SUPPORTED row with explanation |
| | Whole endpoint 5xx/offline | Page-level error with retry; SSE state independent of this |
| | Refresh in progress | Button disabled with `aria-busy`, spinner + text "Refreshing" |

Staleness thresholds are a product decision (suggest warn at 2x the refresh interval); not determined by any repo file.

## 5. JTBD: "Why did my request go to that model?"

Job statement: When a client's request ends up on an unexpected model/provider, I want to see what was asked, what ran, and which rule/decision/failure caused it, so I can fix config or the client without reading logs or code.

Operator steps and what the UI must answer:
1. Locate the request: by time and, secondarily, by requested model or request ID (the client may log the ID). Need: filter/search by requested model, routed model, provider, status; copyable full ID. (Not present today.)
2. Compare asked vs got: requested -> routed in the row (section 2.1).
3. Learn the reason: reason chip, then drawer "Why" line naming the config rule/alias/pin (needs backend to emit a rule identifier; see backend research).
4. If fallback: which attempt failed and why (status/error class), how long each cost, and whether the final answer was from the fallback.
5. Verify the target is real: Upstream models view says whether the routed or configured model is actually offered, and flags config entries missing upstream.
6. Act: link from the drawer reason to the Config Editor at the relevant rule (nice-to-have); copy request ID / cURL-free summary for a bug report.

Priority for the appetite (INFERRED value ranking): 1) fix blank fields; 2) requested->routed + reason chip; 3) attempt waterfall; 4) upstream models view; 5) trace filtering/search; 6) deep-link to Config Editor. Cut order, if scope slips: 6, 5, then waterfall polish (keep a plain attempt list).

## 6. Open questions for planning

- Which name is "requested": raw client model or post-alias? Row design assumes raw, with alias recorded as reason (`alias`) so both are recoverable.
- Does the trace need `attempts[]` inline (event size, 100-entry ring) or fetched lazily on Inspect from a `GET /api/traces/{id}`? UX works with either; lazy fetch needs the drawer loading/evicted states above.
- Are `compressionStatus` and the rest of the camelCase/snake_case contract fixed first? Rendering work is blocked on real data; see requirements.md baseline (INFERRED, unconfirmed).
