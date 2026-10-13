# Architecture Research: trace -> model mapping data flow

Labels: **VERIFIED** = I opened the cited lines; **INFERRED** = deduced, not run. Nothing here was executed (no live SSE capture), so the snake_case/camelCase mismatch stays INFERRED until captured. Prior art (not repeated): `project_plans/webui-redesign/research/architecture.md`, ADR-002 (SSE broadcast), ADR-003 (signals bounded store).

## 1. Current data flow

1. **Ingress**: `chat_completions.rs:57` calls `Router::dispatch(anthropic_body, headers, stream, est_tokens)` after translating to Anthropic wire format; `/v1/messages` does the same via `entrypoint/messages/`. VERIFIED (grep). `observability.rs:995,1095` are test callers.
2. **Requested model captured**: `dispatch` reads `body["model"]` into local `model` (`src/routing/router/mod.rs:676-680`), and `RequestDetail::from_body` stores the same string as `RequestDetail.model` (`src/metrics/mod.rs:96`, `request_model_name` `:116`). So **`RequestDetail.model` is already the client-requested name** - VERIFIED. It is pre-alias for upstream-routed requests.
3. **Alias/local catalog**: `local_catalog.resolve(&model)` short-circuits to `dispatch_local` (`router/mod.rs:682`) *before* `push_request`, so local-catalog requests never get a `RequestDetail` or trace - VERIFIED.
4. **Candidate expansion / strategy**: `effective_candidates(session_id)` (session pin, `:600`) -> `strategy.expand_candidates` (`:688`) -> loop: `admitted_candidates` (health) -> `strategy.select` (`:707`). Routing reason inputs (session pin, strategy name, exploration flag, health filtering, rate-limit hold) are all visible *inside this function* but only `selected_model` and `was_exploration` are persisted, via `record_selection` (`:848-857`) - VERIFIED.
5. **Dispatch + retries**: each loop iteration is one attempt: `provider.send` timed by `attempt_started` (`:747-748`); five outcome arms (`:750-787`) each call `record_dispatch_outcome` (`:1042`) which fans out to counters, `error_tracker.push`, strategy stats, and health. Attempts are **not accumulated per request** - the model/provider is overwritten per attempt ("last attempt wins", `:841-847`). VERIFIED.
6. **Trace emission**: only on first success: `record_first_attempt_timing` (`:755`, `:1082`) -> `MetricsCollector::update_request_timing` (`metrics/mod.rs:268`) which builds `RequestTraceData` and `broadcast::send`s it (`:282-303`). Consequences, all VERIFIED:
   - `status_code: 200` hardcoded (`:292`); failed/exhausted requests (`Err` returns `:760`, `:794`) **emit no trace at all**.
   - `duration_ms` = time-to-headers of the successful attempt, `first_byte_ms` set equal to it (`:1077-1081` comment); streaming duration not measured.
   - `tokens_after` and `compressed` are never set in production: only assignment is a test (`observability.rs:792`); `from_body` initialises `0`/`false` (`metrics/mod.rs:99,101`). The "tokens saved" column is therefore 0 regardless of the serde issue (the legacy `dashboard.rs:529` shows the same data). Compression stats exist only as aggregate atomics (`counters/mod.rs:72`).
   - `RequestTraceData.provider` is the *upstream name* of the successful attempt; `model` is the requested name; the routed/selected model is not on the event (`events.rs:27-38`).
7. **Family resolution**: `model_family` is injected into the outbound body (`router/mod.rs:744-746`); the actual model id is resolved *inside the provider* (`providers/openai/resolution.rs` `resolve_family`, TTL cache `:829`) and recorded only into per-family counters (`metrics/counters/resolution.rs:99-107`), keyed by family, not by request. So "routed model id for a family route" is **not available at the router today** - INFERRED from the grep; needs a provider-trait change or a lookup of `resolution_state` at emit time (approximate under concurrency).
8. **SSE**: `handle_sse_events` (`events.rs:67`) subscribes to `state.event_tx`, serialises `DashboardEvent` (`#[serde(tag="type", content="data")]`, `:57-58`) with default field names; lag -> `system_lag` event (`:76`). Other emitters: `ErrorTracker` (`metrics/error_tracker.rs:317`) and the ticker (`entrypoint/mod.rs:201`).
9. **UI**: `SseService` parses `onmessage` and named events (`ui/.../sse.service.ts:65-81`) -> `MetricsStore.pushTrace` (cap via `maxTracesLimit`, `metrics.store.ts:60-64`). UI model expects `requestId`, `durationMs`, `firstByteMs`, `tokensBefore`, `tokensAfter`, `compressionStatus`, `statusCode` and an `id` (`telemetry.models.ts` `RequestTraceData`); backend sends snake_case and `compressed: bool`. `ErrorLoggedData` has the same drift (`providerId`/`errorDetails` vs backend `provider`/`message`). INFERRED (serde default, not captured).
10. **`/api/models`** (`entrypoint/api.rs:54-87`): VERIFIED. Per request it reloads config from disk, calls `build_providers` (constructs every provider, throwaway `ProxyMetrics`), then calls `list_models()` serially on each. Returns `{ "<upstream>": [{id, owned_by}] | {error: "..."} }` - per-upstream error inline, **no cache, no refresh timestamp, no latency/ttl, snake_case `owned_by`, array-or-object union per key**. Nothing in `ui/src` references it (grep of `api/models` empty). `Provider::list_models` exists on the trait (`providers/mod.rs:181`) with impls for anthropic, bedrock, gemini, openai, openrouter - so every adapter already exists; the work is a cache/service around them.

## 2. Where to capture the new fields (minimum hot-path cost)

Principle: the loop in `dispatch` already computes everything; capture into a **local `Vec<AttemptRecord>` on the stack** (no lock per attempt) and hand it over **once** at request end. Today `mutate_request` takes the ring-buffer mutex and linear-scans up to 100 entries per call, three times per attempt (`selected_model`, `was_exploration`, timing) - VERIFIED (`metrics/mod.rs:257-265`, `record_selection`). Replacing them with one terminal write reduces hot-path cost.

| Field | Source | Capture point |
|---|---|---|
| requested model | `body["model"]` | already `RequestDetail.model` (`from_body`); expose on event as `requested_model` |
| routed provider/model | `chosen.name`, `chosen.model`, family-resolved id (INFERRED unavailable) | final `AttemptRecord` where `outcome==Ok`; family-resolved id needs `ProviderResponse` to carry `resolved_model` (small provider change) or defer to a later story |
| routing reason | `session_id` pin (`effective_candidates`), strategy name (`strategy`), `was_exploration`, `hold_retries>0`, `Admit::Shed` | small enum `RouteReason { SessionPin, Strategy(&'static str), Exploration, Fallback, RateLimitHold }` set locally in `dispatch` |
| attempt chain | per-iteration `(index, name, model, started->elapsed, outcome kind)` | `Vec<AttemptRecord>` pushed in each of the 5 outcome arms + the `Shed` branch (`:728-732`) |
| status code | `ProviderError` kind -> HTTP status (see `entrypoint/errors.rs`); success = 200 | terminal write; emit trace on **both** success and exhaustion so failures appear |

Seam: introduce `routing/trace.rs` (`AttemptRecord`, `RouteReason`, `RequestOutcome`) and one `MetricsCollector::finish_request(request_id, RequestOutcome)` that replaces `update_request_timing`+`set_selected_model*`. `RequestDetail` gains `attempts: Vec<AttemptRecord>` (bounded, e.g. max 8; the `dispatch` loop is already bounded by candidates plus `MAX_HOLD_RETRIES`) and `routing_reason`. Memory: 100 entries x <=8 attempts of ~64 B = ~50 KB. No new locks, no new channel.

## 3. Alternatives - recommendations

| Decision | Recommendation | Rationale |
|---|---|---|
| serde camelCase backend vs UI adapts | **Backend: `#[serde(rename_all="camelCase")]` on the new/changed SSE structs, plus `#[serde(alias)]` is not needed on serialise; keep snake_case duplicates only if a non-UI consumer is found.** | The UI is the only in-repo consumer (INFERRED: `dashboard.rs` legacy page uses `/api/...` REST snake_case, not SSE - confirm with `grep "dashboard/events"`); the existing test asserts `"request_id"` (`events.rs:134`) and must change. Fixing one struct is cheaper than adapting every UI model, and camelCase is the TS convention the three UI models already use (`telemetry.models.ts`). Also fix `compressed` -> UI `compressionStatus` by adding `compressionStatus` string server-side (derive from `tokens_after<tokens_before`), keeping `compressed` for compatibility. Capture a real event first to confirm root cause (requirements Open Question 1). |
| ring buffer vs trace store | **Extend the ring buffer (`RequestDetail.attempts`), keep 100 cap; raise to a named const (e.g. 500) only if exploratory testing shows 100 is too small.** | Attempts are tiny and already co-located with `recent_requests`, `original_bodies`, `compressed_bodies` capped in lockstep (`metrics/mod.rs:242-252, 328`). A separate store duplicates eviction logic and adds a join. The live table is SSE-fed (ADR-002/003); the Inspect view can use the SSE event (which carries the attempts) plus the existing ring-buffer REST for backfill. Revisit a persistent store only if historical replay becomes a requirement (not in scope). |
| model list on-demand vs cached | **Cached periodic refresh with manual refresh button** | Today every `GET /api/models` rebuilds all providers from disk and does serial network calls (`api.rs:57-84`); latency = sum of upstreams and N tabs = N fan-outs. Cache gives constant-time UI reads, per-upstream `refreshed_at`/`error`/`stale` (required by success metric 3), and a place for backoff. |

### Model-list service shape (recommended)
- New `src/routing/model_catalog.rs` (or `src/providers/catalog.rs`): `ModelCatalogService { entries: ArcSwap/RwLock<HashMap<String, UpstreamModels>> }` where `UpstreamModels { models: Vec<ModelInfo>, refreshed_at, last_error: Option<String>, last_attempt_at }`.
- Reuses the live `Router.providers` (already authenticated; `build_providers` per call is unnecessary) via a `list_models` fan-out with per-provider timeout (e.g. 10 s) using `join_all`; one task per upstream so a slow Bedrock control-plane call doesn't block others. Refresh on startup, every ~10 min (config), and on `POST /api/models/refresh`; also rebuild on hot-swap in `post_route`.
- Never overwrite last-good models with an error: keep stale models + `last_error` + `stale: true`.
- Expose `GET /api/models` additively: `{ "upstreams": { "<name>": { "models":[...], "refreshedAt", "error", "stale" } } }` under a **new path** (`/api/model-catalog`) or versioned shape, leaving the current response untouched for existing callers (control-panel free-text -> select, per the doc comment at `api.rs:45-47`). Log refresh outcomes via `tracing` (no keys).
- Emit a `ModelCatalogChanged` `DashboardEvent` variant (additive) so the UI doesn't poll. UI: new `ModelCatalogStore` (signal), mirroring `config.store.ts` error-signal pattern.
- Upstreams without a list endpoint: `list_models()` returns `Err` today - map to `error: "unsupported"` distinct from transient errors (INFERRED; check each adapter's behaviour: anthropic `:574`, bedrock `:904`, gemini `:497`, openai `:865`, openrouter `:656`).

## 4. Hotspot / size assessment

Churn is 6-month `git log` file-touch counts (VERIFIED): `entrypoint/mod.rs` 20 commits, `metrics/mod.rs` 9, `entrypoint/api.rs` 9, `router/mod.rs` 7, `events.rs` 2. Sizes (VERIFIED `wc -l`): `router/mod.rs` 1097, `entrypoint/mod.rs` 758, `api.rs` 933, `metrics/mod.rs` 568, `events.rs` 136. `dispatch` alone is ~130 lines with 5 near-duplicate outcome arms (`:750-787`).

| File | Disposition |
|---|---|
| `src/routing/router/mod.rs` (oversized, `dispatch` is the churn centre) | **Isolate via seam**: add `AttemptRecord` + one `finish_request` call in a new `routing/trace.rs`; do not restructure the 1097-line file. At most extract the outcome-arm bookkeeping into the new type. |
| `src/metrics/mod.rs` (568, moderate) | **Extend as-is** (replace `update_request_timing`/`set_selected_model*` by `finish_request`; keep ring buffer). |
| `src/entrypoint/mod.rs` (758, 20 commits - highest churn) | **Extend as-is** - only route registration lines for the catalog endpoint; keep handler logic in a new module. |
| `src/entrypoint/api.rs` (933) | **Isolate via seam**: put catalog handlers in a new `entrypoint/models.rs`; leave `get_models` unchanged for compatibility. |
| `src/entrypoint/events.rs` (136) | **Extend as-is**. |

Hotspot ranking itself was not computed with `code-hotspot-analysis` (only churn x size by eye) - INFERRED ranking.

## 5. Event-Command-Policy: trace / attempt lifecycle

| Event | Command | Policy | Notes |
|---|---|---|---|
| RequestReceived (dispatch entry, after local-catalog check) | `push_request(RequestDetail::from_body)` | Record `requested_model`; allocate empty attempts Vec locally | exists (`router/mod.rs:690-698`) |
| CandidateSelected | push `AttemptRecord{upstream, model, reason}` to local Vec | No lock, no emit | replaces `record_selection` mutex writes |
| AttemptAdmissionShed | mark attempt `Shed`, continue loop | Do not trip cooldown (ADR-004 comment `:722-726`) | currently not recorded |
| AttemptFailed(retryable: rate-limit, shape-mismatch, other) | finalize attempt with kind + elapsed; `record_dispatch_outcome` | Trip health/cooldown per existing policy; continue to next candidate; set reason=Fallback for next | existing arms `:762-786` |
| AttemptFailed(validation/auth) | finalize attempt; end request | Non-retryable: `finish_request(status=4xx)` | `:758-761` |
| AttemptSucceeded | finalize attempt; `finish_request(status=200, chain)` | Single ring-buffer write + one SSE emit | replaces `record_first_attempt_timing` |
| AllCandidatesExhausted | `finish_request(status=502/last error, chain)` | **Emit trace too** (currently silent); also emit `ErrorLogged` as today | `:790-794` |
| TraceEmitted (broadcast) | `event_tx.send(RequestTrace)` | Ignore send error (no subscribers); count drops via lag event + counter; never block | `metrics/mod.rs:296-303` |
| ClientLagged | `system_lag` SSE event | UI may show gap banner / backfill from REST | `events.rs:76` |
| RingBufferEvicted | `pop_back` at 100 | Late `finish_request` becomes no-op - acceptable; log at debug | `metrics/mod.rs:248, 254-256` |
| ModelCatalogRefreshTick / ManualRefresh | refresh each upstream concurrently | Timeout per upstream; keep last-good on error; set `stale` | new |
| ModelCatalogChanged | broadcast additive SSE event | UI store update | new |
| RouteHotSwapped (`POST /api/route`) | rebuild router; trigger catalog refresh | `ConfigChanged` already emitted | `api.rs:134` |

## 6. Risks / open items
- Streaming duration/TTFT are not real today (time-to-headers only); true TTFT needs a stream tee like `CostTrackingStream` (`entrypoint/cost_tee.rs`) - separate, larger story. Decide whether "TTFT" column is relabelled or implemented.
- `tokens_after`/`compressed` never populated: need to find where compression result is available (compression runs outside `dispatch`) and write it to the `RequestDetail` - not located in this research (gap).
- Family-resolved concrete model id is not exposed to the router (section 1, item 7) - INFERRED; confirm in `providers/openai/resolution.rs` before committing to showing it.
- Local-catalog (`dispatch_local`) requests bypass tracing entirely; decide if in scope.
- Compatibility: VERIFIED by `grep -rln "dashboard/events" src ui/src` - only `src/entrypoint/events.rs`, `src/entrypoint/mod.rs` (route + test at `:742`) and `ui/.../sse.service.ts` reference the endpoint, so the Angular UI is the only in-repo consumer; external consumers (e.g. user scripts) remain unknown, so keep the "alias for one release" option only if one surfaces.
