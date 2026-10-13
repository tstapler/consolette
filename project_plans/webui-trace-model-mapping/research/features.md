# Research: Features + Static Gap Audit (webui-trace-model-mapping)

Date: 2026-10-09. Method: static reading only; nothing was run. Labels: **VERIFIED** = read in source at the cited line (or asserted by an existing unit test); **INFERRED** = reasoned from code, needs the live run. Paths are relative to the repo root. Line numbers are from the working tree at commit 49ec827.

## 1. UI to backend wiring audit

### 1.1 SSE contract (`/v1/dashboard/events`)

Transport is fine. The backend sends every `DashboardEvent` as the default `message` event, with the JSON body `{"type":..,"data":..}` (`src/entrypoint/events.rs:71-75`). `sse.service.ts:65-72` parses `onmessage`. Lag is sent as a named `system_lag` event with `{skipped}` (`events.rs:76-83`), and the UI listens for it by name (`sse.service.ts:75-84`). **Transport: WIRED.** Nothing consumes `system_lag` after parsing: `MetricsStore.handleDashboardEvent` handles only MetricsTick and RequestTrace (`metrics.store.ts:52-58`).

**Root cause of the blank trace columns: VERIFIED.** The Rust structs have no `rename_all`, so they emit snake_case. The test at `events.rs:134` asserts `"request_id":"req_123"`. The UI model reads camelCase.

| Event / field | Backend (file:line) | UI (file:line) | Verdict |
|---|---|---|---|
| RequestTrace.request_id | `events.rs:28` | `requestId?` `telemetry.models.ts:14` | MISMATCHED (case). The row shows `unknown` via `recent-trace-table.component.ts:102`. |
| RequestTrace.id | not sent | `id: string` (required) `telemetry.models.ts:13` | MISSING backend. The UI type claims it is always present. `trackById` falls back to `timestamp-provider` (`recent-trace-table.component.ts:140`), which collides for same-second requests. |
| duration_ms / first_byte_ms | `events.rs:32-33` | `durationMs` / `firstByteMs` `:18-19` | MISMATCHED (case). The cell renders `undefinedms`. |
| tokens_before / tokens_after | `events.rs:34-35` | `tokensBefore` / `tokensAfter` `:20-21` | MISMATCHED (case). |
| compressed: bool | `events.rs:36` | `compressionStatus: string` `:22` | MISMATCHED (name and type). The UI never reads `compressionStatus` in the table. |
| status_code | `events.rs:37` | `statusCode` `:23` | MISMATCHED (case). The value is also hardcoded 200 (`src/metrics/mod.rs:292`). |
| timestamp / provider / model | `events.rs:29-31` | same | WIRED. Timestamp is RFC3339. |
| MetricsTick.rpm | `events.rs:19` | `rpm` | WIRED. The only field that matches. |
| MetricsTick.total_requests | `events.rs:20` | `totalRequests` `:3` | MISMATCHED. The store never uses it. |
| MetricsTick.total_tokens_saved | `events.rs:21` | `tokensSaved` `:4` | MISMATCHED (name and case). The `tokensSaved` computed is always 0 (`metrics.store.ts:35`). |
| MetricsTick.current_lag_ms | `events.rs:22` | `currentLagMs` `:8` | MISMATCHED (case). `isContended` never trips. |
| MetricsTick.provider_health | `events.rs:23`, `entrypoint/mod.rs:208-210` | `providerHealth` `:9` | MISMATCHED (case). The backend also hardcodes `"healthy"` for every upstream. |
| tps, medianTtftMs, tokenSavingsPercent | not sent | `:5-7`; KPI cards `overview-dashboard.component.ts:42-60` | MISSING backend. Two of the four KPI cards are permanently 0. |
| ErrorLogged | `{timestamp, provider, model, error_type, status_code?, message}` `events.rs:41-48` | `{providerId, statusCode, errorDetails, timestamp}` `:26-31` | MISMATCHED (names and shape). Also not consumed by any store (`metrics.store.ts:52-58`). |
| ConfigChanged | `{timestamp, route_name, strategy}` `events.rs:51-55` | `{timestamp, changedKeys?}` `:33-36` | MISMATCHED, and not consumed. The Config page does not refresh when the config changes elsewhere. |

Emission semantics, all VERIFIED:
- `RequestTrace` is emitted only from `update_request_timing` (`metrics/mod.rs:268-304`), which only the success path calls (`routing/router/mod.rs:755` via `record_first_attempt_timing`). Failed, exhausted, validation-error and auth-error requests never produce a trace. That is why the UI can only ever see 200s.
- `first_byte_ms` equals `duration_ms`, and both are time-to-response-headers, not full stream time (`router/mod.rs:1070-1086`, `record_attempt` doc at about `:1000`). "TTFT" and "Duration" are the same number, and streaming durations are understated.
- `tokens_after` is initialised to 0 (`metrics/mod.rs:99`) and nothing in non-test `src/` assigns it. `compressed` is likewise never set. After the casing fix, "Tokens (Saved)" still shows `0` and no savings. `tokens_before` is a pre-dispatch estimate (`est_tokens`, `router/mod.rs:692`).
- Requests whose model resolves in `local_catalog` return early from `dispatch` (`router/mod.rs:677-679`) before `push_request`. They produce no ring-buffer entry, no trace, and nothing in sessions or benchmarks.
- The ticker fires on an interval (`entrypoint/mod.rs:~167-225`). The interval was not read.

### 1.2 Overview page

| Element | Source | Verdict |
|---|---|---|
| Connection badge | `SseService.connectionStatus` | WIRED |
| RPM card, RPM trend chart | `MetricsTick.rpm` | WIRED |
| Tokens/sec card | `tps` | MISSING backend (always 0) |
| Median TTFT card | `medianTtftMs` | MISSING backend. The histogram exists (`MetricsCollector.histogram`), so it is computable. |
| Token Savings card | `tokenSavingsPercent` | MISSING backend (and nothing sets `tokens_after`) |
| Provider health grid | `providerHealth` | MISMATCHED (case) plus hardcoded "healthy" backend. When the input is null the grid shows fake `Anthropic / AWS Bedrock / OpenAI` (`provider-health-grid.component.ts:83-87`), which is misleading. Cooldown seconds and `lastError` are never fed (`providersList` is unused). Real data exists in `/metrics` `cooldowns` (`observability.rs:26`) and `last_error_kind`. |
| Trace table, cols Time, Provider/Model | | WIRED (`model` is the *client-requested* model, see 2) |
| Trace table, cols Request ID, Duration, TTFT, Tokens, Status | | MISMATCHED (see 1.1) |
| Inspect button | navigates to `/sessions?requestId=` (`overview-dashboard.component.ts:100`) | STUBBED. `session-replay-view.component.ts` never reads the query param (grep: no `queryParam` or `ActivatedRoute`). The button is a dead end. |
| Table history | `maxTraces=500` in the UI store (`metrics.store.ts:33`) | Note: the UI store starts empty on page load. There is no backfill from the backend ring buffer (100), so a fresh load shows no history. Neither `/metrics` `recent_requests` nor any `/v1/dashboard/*` route is called to seed it. |

### 1.3 Session Replay

| Element | Source | Verdict |
|---|---|---|
| Session list | `GET /v1/dashboard/sessions` (`observability.rs:72-156`), camelCase `SessionSummary` | WIRED. Fields match `SessionData`. Only sessions with `metadata.user_id` appear. Built from the last 100 requests. `model` is the requested model of the latest request. `limit` and `search` work. |
| Click a session to load payloads | `GET /requests/{sessionId}?stage=...` (`session-replay-view.component.ts:95-103`) | MISMATCHED. The route keys on a *request* id (`observability.rs:310-327`, uuid from `router/mod.rs:691`). The UI passes the *session* id (`session.id`). VERIFIED by reading both sides, so original and compressed payloads should always 404 (live-run check needed). |
| Compressed payload | `get_compressed_body` | The compressed ring buffer is never written in non-test code. Doc at `metrics/mod.rs:188-190` says compression isn't wired into dispatch. Always 404 even with the right id. |
| Pipeline timeline | `currentTimelineSteps` hardcoded (Ingress 2ms, RateLimit 1, Compression 14, Router 4, Dispatch 180, Egress 2) `session-replay-view.component.ts:67-74` | HARDCODED. Looks real but is fake for every session. |
| Token breakdown bar | `promptTokens=1250, responseTokens=450, savedTokens=320` | HARDCODED |
| Pinned badge | `pinned` | WIRED. `/api/sessions/{id}/route` pin and unpin endpoints exist (`mod.rs:334-339`), but the UI has no pin/unpin control. |
| Per-turn list for a session | none | MISSING backend and UI. There is no `/v1/dashboard/sessions/{id}/turns`, so "replay" has nothing to replay. |

### 1.4 Model Benchmarks

`GET /v1/dashboard/benchmark` (`observability.rs:181-299`): camelCase and WIRED to `BenchmarkData`. The numbers are partly fabricated (all VERIFIED):
- `success_rate_percent` is 100.0 and `error_rate` is 0.0 for every row (`:257-258`). Failures are never in the ring buffer.
- `cost_per_1k` is a constant 0.015 (`:259`).
- `speed_tok_sec` falls back to 35.0 when there is no duration (`:238`). Since `tokens_after` is 0, the computed value is also 0.
- `aider_score` falls back to 55.0 for an unknown model (`:243`). It is real for models in `bench_table`.
- When there are no recent requests, **fake seed rows** (claude-3-5-sonnet, gpt-4o, gemini-1.5-pro, deepseek) with fixed numbers are returned (`:265-296`). A cold-start operator sees fabricated data with no marker.
- Grouping is by `(requested model, provider)`. The routed/selected model is ignored, so a pinned OpenRouter pool shows as the client alias. Percentiles use at most the last 100 requests, and TTFT equals duration (see 1.1).
- Real data exists elsewhere but is not exposed here: per-model attempt outcomes (`record_model_attempt`, `router/mod.rs:~1038`) and the `openrouter_scoring` snapshot on `/metrics`.

### 1.5 Config Editor

| Element | Source | Verdict |
|---|---|---|
| Load | `GET /v1/dashboard/config` (`observability.rs:417-487`) | WIRED for providers, fallbackCascade and webUi. API keys are masked. Duplicate snake_case keys are sent alongside the camelCase ones. |
| Provider apiKey, baseUrl, weight | PUT handler `:521-583` | WIRED. baseUrl is only applied for `UpstreamKind::Openai` and must pass HTTPS and not-private validation, so localhost bases are rejected. A masked key is preserved. The PUT always rewrites auth as `Apikey` with header `x-api-key` (`:567-571`), which clobbers Bearer-auth upstreams. INFERRED to be a bug; needs live confirmation. |
| Fallback cascade | PUT handler `:586-617` | WIRED |
| Rate limits (rpm/tpm/concurrent) | GET returns `rateLimits: {rpm: on_breach, maxDelayMs}` (`:466-469`). `rpm` is set from an `on_breach` setting, not an RPM number. | MISMATCHED, and the PUT handler never reads `rateLimits`, so edits are silently dropped. Success is toasted anyway (`config-editor-view.component.ts:~120-125`). |
| Fallback defaults | If `providers` is absent, the editor invents `anthropic`/`openai` (`config-editor-view.component.ts:139-142`) | HARDCODED |
| Persistence | Only the first route is saved, as runtime overrides (`:623-634`). Router is rebuilt and swapped. | WIRED, with the limit that only `routes[0]` is editable. |
| ConfigChanged event | Emitted (`:675-683`) | Unconsumed in the UI |
| Per-model pinning, `model`, `model_family` per upstream | Present in config (`RouteUpstreamRef`) | MISSING in UI. The editor cannot show or edit which model an upstream is pinned to. |

### 1.6 Backend routes with no UI surface

Verified in `src/entrypoint/mod.rs:259-356`:
- `/api/models` (per-upstream model catalog, `api.rs:54`) is **never called by the SPA** (grep for `/api` in `ui/src/app` returned nothing).
- `/api/route` GET/POST, `/api/sessions`, `/api/sessions/{id}/route`, `/session/prune`, `/session/policy`, `/session/prune/stats`, `/errors/summary`, and `/metrics` (including `cooldowns`, `capability`, `response_cache`, `openrouter_scoring`) are likewise unused by the SPA.
- `/errors/summary` plus the `ErrorLogged` event give an error feed that no page shows.
- `/v1/models` (client-facing catalog) is also not used by the SPA.

`/api/models` behaviour (VERIFIED, `api.rs:54-87`):
- It reloads config from disk (not the live router) and **builds all providers on every call**, then calls `list_models()` live and uncached.
- The response is a map `{upstream: [{id, owned_by}] | {error}}`. It has no last-refresh time, no staleness and no timeout. One slow upstream blocks the whole response (sequential `for` loop).
- Every provider implements `list_models` (anthropic `:574`, bedrock `:904`, gemini `:497`, openai `:865`, openrouter `:656`). Whether each returns an error or a static list was not examined. Bedrock's may be a static table; INFERRED, check live.
- The failure of `build_providers` aborts the whole thing with a 500.

## 2. Where the routing data lives in the dispatch path

All VERIFIED, `src/routing/router/mod.rs`:
- **Client-requested model**: `body["model"]` at dispatch entry (`:676-681`, a local variable). It is also captured into `RequestDetail.model` by `from_body` (`metrics/mod.rs:96, 116-121`). So the trace's current `model` is the client-requested name, not the routed one.
- **Resolved/selected model**: `chosen.model` (a pinned model from route config) is written to the outbound body (`:736-743`) and to `RequestDetail.selected_model` (`record_selection :846-857`). It is `None` for non-pinned routes, in which case the model is passed through unchanged. Further rewriting happens *inside providers* and is not visible to the router: Bedrock `to_bedrock_model_id` (`providers/bedrock.rs:296`), OpenAI-family resolution via `MODEL_FAMILY_BODY_KEY` (`router/mod.rs:744-746`, resolution in `providers/openai/resolution.rs`), and Gemini translation. The **final wire model is not captured anywhere today**.
- **Provider/upstream**: `chosen.name` (set via `update_request_timing` on success only).
- **Routing reason**: partly derivable. `chosen.index`, the strategy (`state.server_info.strategy`, as used by `ConfigChanged`), `selected_model_was_exploration` (`record_selection`), session pin (`effective_candidates(session_id)`, `:688`) and the admission shed (`:727-733`). None is stored per request except exploration. The pin source (session pin vs route) is not recorded.
- **Per-attempt chain**: only logged transiently. Each attempt passes through `record_dispatch_outcome` (`:752, 759, 763, 774, 784`) with `chosen`, `attempt_started`, outcome and error. That is the single choke point to append an `Attempt {upstream, model, status/kind, latency_ms}` record. Today `selected_model` and `provider` are overwritten by the *last* attempt (the doc at `:843` says so). The hold-and-retry (`:810-836`), admission Shed (`:729-731`, no attempt recorded at all) and `ProviderError` classes (`providers/mod.rs:36-65`) are all there. `ProviderError::Upstream{status}` gives the HTTP status, but `RateLimited`, `Timeout` and `Exhausted` have no numeric code (need a mapping: 429, 504, 503).
- **Final HTTP status to the client**: `entrypoint/errors.rs` presumably maps errors. It was not read; the error path is a candidate place to emit a failure trace.
- **Streaming**: success is recorded at header time. Stream-completion data (tokens, duration, error mid-stream) is only available in `cost_tee.rs` / `openai_stream.rs` (`CostTrackingStream`, per the `record_attempt` doc). The trace cannot carry final tokens or TTFT without a tee hook. INFERRED; not read in detail.
- **Alias/family**: `model_family` per route upstream (config) is injected as a body key. What counts as "requested" is `body["model"]` as the client sent it. For `/v1/chat/completions` the body is translated to Anthropic shape first, and `model` is preserved (comment at `metrics/mod.rs:79-82`).

## 3. Comparable gateway dashboards (from general knowledge; not freshly researched, treat as INFERRED)

- **LiteLLM** (Admin UI "Logs" / spend logs): columns for time, request id, model requested (`model_group`), actual model (`model`), api_base/provider, tokens (prompt/completion/total), cost, latency, status, user/team/key. A detail drawer shows request/response, metadata, and retry/fallback info (`model_info`, `fallbacks`, "attempted_retries" in headers such as `x-litellm-model-id`, `x-litellm-attempted-fallbacks`). This is the closest analogue to a requested-vs-routed view.
- **Helicone**: Requests table with model, provider, status, latency, TTFT, tokens, cost, custom properties, and session grouping with a session tree and timeline. Fallbacks are shown through the gateway's multi-model routing. Strong precedent for TTFT and session trees.
- **Portkey**: Logs with provider, model, status, latency, cost, "trace id", and a per-request timeline of the **gateway strategy** (fallback/loadbalance/retry) that shows each attempted target and its outcome. The best precedent for an attempt chain.
- **OpenRouter Activity**: per-generation rows with model, provider actually served, tokens (prompt/completion/native), cost, TTFT/throughput ("generation time", "latency"), finish reason, and a "fallback used" marker when a different provider served the request. Also shows a per-provider model-availability page.
- Common pattern worth copying: show requested -> served model as `A -> B` in one cell, with an arrow, a badge when they differ, and an expandable attempt list. Hide the arrow when they are equal.

## 4. Edge cases and unstated needs

1. Requested == routed (no pin): show a single model, no arrow.
2. Non-pinned route: `selected_model` is `None` (`metrics/mod.rs:57-61`), so the UI must fall back to the provider plus the requested model. The wire model is unknown (see 2).
3. Failed requests: no trace today (1.1). A request that exhausts every candidate (503/529) or fails validation (400) must still be listed, with its final status and the attempt chain, otherwise "Status" is always 200.
4. Mid-stream failure after a 200 header: status stays 200. Needs a tee hook or an annotation.
5. Retry that eventually succeeds (e.g. 429 then 200): the final trace should show 200 plus `attempts=2`.
6. Hold-and-retry: up to about 45 s of added latency (`router/mod.rs:805-809`), with no attempt records in between. The UI should be able to show "held N times".
7. Admission Shed: loops with no attempt recorded.
8. Local-catalog requests (1.1): invisible. Decide whether they are traced.
9. Session pin overrides: `/api/sessions/{id}/route` changes the route silently. The trace should say "pinned by session".
10. Config hot-swap mid-request: `ConfigChanged` rebuilds the router. In-flight attempts belong to the old router, and the candidate indexes change. Attempt records should store upstream names, not indexes.
11. Ring buffer cap of 100, shared by bodies (up to 64 MB each, `entrypoint/mod.rs:360`). Adding attempts increases per-entry size only slightly. Do not store bodies in attempts. Security: do not put error bodies into attempts unredacted, since `ProviderError::Upstream.body` may contain prompt fragments or keys (requirement: no leaks). Store the status class and kind label only (`kind_label()`).
12. SSE reconnect/refresh: the UI store starts empty. Add a backfill from the backend buffer (`get_recent_requests`), which also provides the failed-request history once failures are recorded. `EventSource` auto-reconnect is disabled by the UI's own handling (`closeExistingSource` plus manual backoff), and lagged events are dropped silently.
13. Back-compat: `/v1/dashboard/events` has other consumers possibly (MCP). The only other in-repo consumer found is none: the legacy dashboard uses `/metrics` `recent_requests`, not SSE (`dashboard.rs:507-545`). Adding new fields is additive. Casing choice: the REST dashboard endpoints already use camelCase (`SessionSummary`, `BenchmarkItem`), so `#[serde(rename_all="camelCase")]` on the SSE structs is consistent with them. Duplicating snake keys for one release (as `get_dashboard_config` does) is the precedent for compatibility.
14. Model list: upstreams without a list endpoint, auth failure, Bedrock needing a control-plane call, slow upstreams (sequential in `get_models`), large OpenRouter catalogs (hundreds of ids: needs search or filter in the UI), and the stale/never-refreshed state. Requirement is "last-refresh time and error state". The current endpoint has neither. A cache with a background refresh is a new component.
15. Time zones and clocks: the timestamp is RFC3339 UTC and the UI formats it locally. Fine.
16. Accessibility: status is conveyed by colour only in the trace table. Add text. Low.
17. The `unknown` placeholder is also what `request_model_name` returns for a missing model (`metrics/mod.rs:119`), so a literal "unknown" model can appear.

## 5. Ranked gap list

| # | Gap | Severity | Effort | Evidence |
|---|---|---|---|---|
| 1 | SSE field casing mismatch makes id, duration, TTFT, tokens and status blank (add `rename_all=camelCase`, plus `id` and `compressionStatus` or adapt the UI) | High | S | `events.rs:27-38`, `telemetry.models.ts:12-24` |
| 2 | MetricsTick mismatches (`totalRequests`, `tokensSaved`, `currentLagMs`, `providerHealth`) and hardcoded "healthy" health | High | S (casing) / M (real health from `HealthRegistry`) | `events.rs:18-24`, `entrypoint/mod.rs:208-210` |
| 3 | Failures never traced, status hardcoded to 200 | High | M | `metrics/mod.rs:292`, `router/mod.rs:755-786` |
| 4 | No per-attempt chain, retry/fallback invisible, last attempt overwrites provider and selected model | High | M | `router/mod.rs:752-786, 843-857` |
| 5 | Requested vs routed model, routing reason not on the trace (`selected_model` exists but unsent; reason not stored) | High | M | `metrics/mod.rs:62-70`, `events.rs:27` |
| 6 | Session Replay click calls `/requests/{sessionId}`, so payloads always 404 | High | S | `session-replay-view.component.ts:95-103`, `observability.rs:310` |
| 7 | `/api/models` not in UI; no cache, refresh time or timeout, sequential and builds providers per call | High (requirement) | M | `api.rs:54-87` |
| 8 | Hardcoded timeline and token bar in Session Replay | Medium | M (needs per-stage timings) / S (hide or label as sample) | `session-replay-view.component.ts:62-74` |
| 9 | Benchmarks: fabricated success rate, cost and fallback rows; groups by requested model | Medium | M | `observability.rs:257-296` |
| 10 | Config rate-limit editor drops edits silently, GET `rpm` is the wrong value, PUT overwrites auth type with `x-api-key` | Medium | S-M | `observability.rs:466-469, 567-571` |
| 11 | `tokens_after` / `compressed` never populated, so "Tokens (Saved)" and the Token Savings KPI are always empty | Medium | M | `metrics/mod.rs:99, 36` |
| 12 | TTFT equals duration (header time); streaming duration understated | Medium | M (tee hook) | `router/mod.rs:1070-1086` |
| 13 | tps and medianTtft KPIs unbacked | Medium | S | `overview-dashboard.component.ts:42-60` |
| 14 | Inspect button dead end (`requestId` query param ignored) | Medium | S | `overview-dashboard.component.ts:100` |
| 15 | ErrorLogged and ConfigChanged unconsumed, shapes mismatched; no error feed page | Low-Medium | S-M | `events.rs:41-55` |
| 16 | No backfill of traces at page load (empty history on refresh) | Medium | S | `metrics.store.ts` |
| 17 | Local-catalog requests untraced | Low | S | `router/mod.rs:677-679` |
| 18 | Provider grid fake default cards, no cooldown/error data | Low | S | `provider-health-grid.component.ts:83-87` |
| 19 | No pin/unpin UI, no per-upstream model pin in config UI | Low | M | `mod.rs:334-339` |
| 20 | `system_lag` ignored | Low | S | `metrics.store.ts:52-58` |

## 6. Scenarios to exercise in the live run

1. Capture a raw RequestTrace and MetricsTick over SSE (`curl -N http://127.0.0.1:<port>/v1/dashboard/events`) to confirm snake_case, the absence of `id`, and the `healthy` constant.
2. Send a request with a known alias (e.g. a `haiku` name) through a pinned model-family route, then compare the requested `model`, `selected_model` (via `/metrics` `recent_requests`) and the upstream's own log, to find where the wire model diverges.
3. Force a 429 on the first upstream (cooldown or a fake upstream) and confirm: retry, fallback to the second, a single trace, status 200, and the overwritten provider.
4. Force total failure (all upstreams down, 400 validation, 401 auth) and confirm that no trace appears and what status the client sees.
5. Stream a long response and compare reported duration/TTFT against wall-clock.
6. Hit a request without `metadata.user_id`, then one with it, and watch Session Replay list behaviour. Click a session and confirm the 404s on `/requests/{sessionId}`.
7. Click Inspect on a trace. Confirm the navigation and that `/sessions` ignores the param.
8. Open Benchmarks with an empty ring buffer (expect fake seed rows) and with traffic (expect 100% success).
9. Config: edit a rate limit and save (expect a toast but no persisted change), edit a Bearer-auth upstream's key (check for auth-header clobbering), add a localhost base URL (expect 400), reload the page.
10. Call `GET /api/models` with one bad key and one unreachable upstream: measure latency, shape and error rendering. Also time it with all upstreams.
11. Refresh the Overview page mid-traffic to see the history loss, then stop the proxy and restart it to see the SSE reconnect and the badge.
12. Fire a burst of requests (more than 1024 events/sec is unlikely, but test with slow client) to see `system_lag`.
13. Local-catalog model request (if a local backend is configured): confirm it is invisible.
14. Hot-swap a route via the Config page during an in-flight request.
