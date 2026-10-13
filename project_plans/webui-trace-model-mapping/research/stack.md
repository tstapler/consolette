# Research: Stack (webui-trace-model-mapping)

Labels: VERIFIED = opened/ran in this session; INFERRED = deduced, not executed.

## 1. SSE serde casing strategy

- VERIFIED: `RequestTraceData` and `MetricsTickData` derive `Serialize` with no `rename_all` (`src/entrypoint/events.rs:17-39`), so they emit snake_case. The envelope is `#[serde(tag="type", content="data")]` (`events.rs:~60`) and is serialized with `serde_json::to_string` (`events.rs:~75`).
- VERIFIED: the UI expects camelCase (`ui/src/app/core/models/telemetry.models.ts:1-26`: `requestId`, `durationMs`, `tokensSaved`, `currentLagMs`, `providerHealth`). The mismatch is wider than the trace: `MetricsTickData` sends `total_tokens_saved` and `current_lag_ms`, but the UI reads `tokensSaved` and `currentLagMs`. Tick fields `tps`, `medianTtftMs`, `tokenSavingsPercent` have no backend source.
- VERIFIED: the UI also needs `id` (required) and `compressionStatus: string`. The backend sends neither (`compressed: bool`, `events.rs:34`).
- INFERRED (not run against a live stream; the requirements Open Question remains open): snake_case on the wire is the root cause of the blank cells.
- VERIFIED precedent for camelCase backend-side: `src/entrypoint/observability.rs:54,159` (`SessionSummary`, `BenchmarkItem`) use `#[serde(rename_all = "camelCase")]`, and it is used on the Gemini wire structs. Dashboard REST types are already camelCase; only the SSE structs diverge.
- Recommendation: add `#[serde(rename_all = "camelCase")]` to the SSE data structs (`MetricsTickData`, `RequestTraceData`, `ErrorLoggedData`, `ConfigChangedData`). Add new fields as `Option<_>` with `#[serde(skip_serializing_if = "Option::is_none")]`. The existing `compressed: bool` stays, and the UI derives `compressionStatus` from it, or the backend adds a `compression_status` string.
- Compat for non-UI consumers (INFERRED, not yet grepped): if one exists, emit the old names via `#[serde(alias = ...)]` (only helps deserialization) or duplicate fields with `#[serde(rename = "request_id")]`. Default to no duplicates unless a consumer is found. Check `src/entrypoint/events.rs` tests (~line 119) and the legacy `src/dashboard.rs`.
- Trace emit site: `src/metrics/mod.rs:282-293` hardcodes `status_code: 200`. The trace is built from the `RequestDetail` closure `r`, so `selected_model` and `selected_model_was_exploration` are available there (requirements baseline).
- No new crates are needed. The new structs (`AttemptTrace { provider, model, status, latency_ms, reason }`) derive `Serialize` only.

## 2. Angular patterns (ui/)

- VERIFIED: Angular `^19.2.0`, rxjs `~7.8.0` (`ui/package.json:15,23`). No NgRx or other store library.
- VERIFIED store pattern: `@Injectable({providedIn:'root'})` class with private `signal<T>()`, public `.asReadonly()`, and `computed()` derivations. `MetricsStore` (`ui/src/app/core/stores/metrics.store.ts:14-70`) subscribes to `SseService.events$`, prepends traces, and caps at 500 (`:42`), while the backend ring buffer is 100.
- VERIFIED REST store pattern: `BenchmarkStore` (`ui/src/app/core/stores/benchmark.store.ts`) uses `HttpClient` plus `isLoading`/`error` signals and `catchError(() => of([]))`. A new `ModelsStore` copies this shape: `GET /api/models`, then `signal<Record<provider, {models|error}>>`, plus `lastRefreshed`.
- Needed UI changes (no new deps): extend `RequestTraceData` with `requestedModel?`, `routedModel`, `routingReason?`, `attempts?: AttemptTrace[]`, and drop or derive `id`/`compressionStatus`. Update `recent-trace-table.component.ts` and `session-replay-view.component.ts`, which both reference these fields (grep hit).
- `MetricsStore.handleDashboardEvent` (`:50-56`) handles only `MetricsTick` and `RequestTrace`. `ErrorLogged` and `ConfigChanged` are ignored, a candidate gap for the exploratory report.

## 3. Upstream model lists

- VERIFIED: `trait Provider` (`src/providers/mod.rs:156`) already has `async fn list_models() -> Result<Vec<ModelInfo>, ProviderError>`. `ModelInfo { id, owned_by }` is at `mod.rs:143`. Implementations: `anthropic.rs:574`, `bedrock.rs:904` (control-plane `aws_sdk_bedrock` client, `bedrock.rs:180`), `gemini/mod.rs:497`, `openai/mod.rs:865`. An openrouter implementation was not confirmed in this pass (INFERRED: it has one, since it has a model-list cache).
- VERIFIED: `GET /api/models` (`src/entrypoint/api.rs:54-86`) reloads config and builds fresh providers on every call. It live-queries `list_models()` per upstream, sequentially, with per-upstream error inlined as `{"error": "..."}`. There is no cache, no timestamp, no concurrency, and `owned_by` is snake_case. The response is `{<upstream>: [ {id, owned_by} ] | {error}}`.
- Gaps against requirements: no `last_refresh`, no cached state, no per-request cost control, and a slow upstream blocks the whole response.
- Auth: providers use their own configured auth (VERIFIED by construction via `build_providers`), which satisfies the "existing provider auth" constraint.

## 4. Caching crates already present

- VERIFIED (`Cargo.toml:89`): `moka = 0.12` with features `future` and `sync`. `dashmap 6` is also present (`:70`).
- VERIFIED precedent: `src/providers/openrouter/cache.rs` has `ModelListCache` (moka sync, TTL, background refresh task, chrono `DateTime<Utc>`). Other moka users: `src/memory/store.rs`, `cost_metrics/store.rs`, `compression/rewind.rs`, `session_compaction/session_state.rs`.
- Recommendation: a periodic cached refresh, not on-demand. Use a small `ModelCatalog` holding `DashMap<String, {models, fetched_at, last_error}>`, filled by a `tokio::spawn` interval task (the same pattern as OpenRouter's background refresh) and by a `POST /api/models/refresh` handler. `GET /api/models` then reads the cache and returns `fetchedAt` and `error` per upstream. Use `futures_util::future::join_all` (`futures-util` is already a dep) to refresh providers concurrently, with `tokio::time::timeout`. Moka is optional (a TTL is not needed if the explicit refresh task owns freshness). The structured log is `tracing::info!/warn!` (`tracing = 0.1`).

## 5. Dependencies

None new. `serde`, `serde_json`, `chrono` (used in `openrouter/cache.rs`, so already in `Cargo.toml` outside the excerpt read), `dashmap`, `moka`, `tokio`, `futures-util`, `tracing`, `axum 0.8` and `tokio-stream` all cover the work. Frontend: Angular signals and `HttpClient` only.

## Open / unverified

- Live SSE capture not performed (root cause stays INFERRED).
- Non-UI SSE consumers not grepped.
- OpenRouter `list_models` implementation not opened.
