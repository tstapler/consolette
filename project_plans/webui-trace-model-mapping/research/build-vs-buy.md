# Build vs Buy: trace/attempt capture, model-list cache, Angular components

Labels: VERIFIED = file opened/grepped this session; INFERRED = reasoning, not run.

## (a) Trace / attempt-chain capture and display

Facts (VERIFIED):
- `tracing` 0.1 + `tracing-subscriber` are deps (Cargo.toml:56-58) and used for logs.
- `opentelemetry` 0.32 + `opentelemetry_sdk` (metrics feature only) are deps (Cargo.toml:105-106), but the only use is the `cmdcrush` bin (`src/bin/cmdcrush/main.rs:27`). There is no `tracing-opentelemetry`, no OTLP exporter, and no OTel trace feature in the proxy.
- `MetricsCollector` already holds a 100-entry ring buffer `Mutex<VecDeque<RequestDetail>>` (`src/metrics/mod.rs:185`, cap at `:245-251`) with in-place update-by-id (`mutate_request`, `:257-262`). `RequestDetail` (`:32`) already has `selected_model` and `selected_model_was_exploration`, set once per attempt, "last attempt wins" (`:308-309`). No attempt list exists today.

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| A1. Extend `RequestDetail` with `attempts: Vec<Attempt>` (provider, model, status, latency, error kind, reason) pushed via a `mutate_request`-style hook; reuse existing SSE event | Zero new deps; same bounded buffer (cap fits NFR); already the path the UI reads; air-gapped; trivial hot-path cost (one short mutex lock per attempt, same as today) | Bespoke; history limited to 100 (raise cap or add a second ring); no standard trace export | **Recommended** |
| A2. `tracing` spans + custom `Layer` that folds span events into the ring buffer | Spans are natural for nested attempts; `tracing` already a dep, no new crate | Layer plumbing is more code than A1; span fields must be scrubbed for secrets; hot-path span overhead; custom Layer is still bespoke | Viable (only if spans are wanted elsewhere) |
| A3. OpenTelemetry traces + OTLP exporter to Jaeger/Tempo | Standard, rich waterfall UIs externally | Needs new crates (`tracing-opentelemetry`, `opentelemetry-otlp`, sdk `trace` feature); needs a collector (not self-contained); does not feed the embedded SPA | Not recommended |
| A4. Langfuse / Helicone / Phoenix | Polished LLM trace UIs | SaaS or extra services to run; violates air-gapped single-binary constraint; prompt content risk (NFR: no prompt leakage); not in the embedded UI | Not recommended (could stay an optional later export) |

Fork-adapt note: no fork candidate; the existing ring buffer is the adaptation target (A1).

## (b) Upstream model-list discovery and caching

Facts (VERIFIED):
- `Provider::list_models()` is in the trait (`src/providers/mod.rs:181`); implemented for anthropic (`anthropic.rs:574`), openai (`openai/mod.rs:865`), openrouter (`openrouter/mod.rs:656`). `ModelInfo` at `providers/mod.rs:143`. Bedrock/gemini implementations were not checked (INFERRED: may be missing or static; `aws-sdk-bedrock` is a dep, Cargo.toml:82).
- `GET /api/models` (`src/entrypoint/api.rs:54-90`) rebuilds all providers via `build_providers` on every call, calls `list_models` live, returns per-upstream list or `{error}`. It has no cache, no refresh time, and returns only `id` and `owned_by`.
- OpenRouter already has a `ModelListCache` refreshed every 5 minutes by a background task using Weak refs (`openrouter/mod.rs:51`, `:95-97`, `:464-473`) and a raw fetch in `openrouter/models.rs:20`.
- `local_discovery.rs` has `LocalCatalog` + `run_local_discovery(catalog, interval)` (`:226`) polling local backends with size/timeout guards.
- `moka` 0.12 is already a dep (Cargo.toml:88), used in `src/memory/store.rs:8`. `arc-swap` is also a dep (Cargo.toml:~125).
- `routing/bench_table.rs` is a static benchmark table (header, lines 1-40), not discovery. Nothing reusable except as a reminder that it needs manual refresh.

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| B1. New `UpstreamModelCatalog` (`Arc<RwLock<HashMap<upstream, {models, fetched_at, last_error}>>>`), one background refresher over the live `Provider` set, `/api/models` reads the snapshot plus a `?refresh=1` forced path; generalize the `LocalCatalog`/OpenRouter-cache pattern | Meets "last-refresh + error per upstream" directly; no rebuilding providers per request (also removes a latency and credential-resolution cost on each call); no new deps; matches in-repo patterns | Must keep the last good list when a refresh fails (design point); need to tie lifetime to router hot-swap (reuse the Weak pattern) | **Recommended** |
| B2. `moka` TTL cache keyed by upstream | Already a dep; TTL/eviction free | Cannot natively keep stale value plus error state and refresh timestamp; need a wrapper struct anyway; TTL expiry yields misses instead of "stale + error" | Viable |
| B3. Keep live-on-demand `/api/models` as is, just add fields | Smallest change | N calls to upstreams per page load; slow/failing upstream blocks UI; hits rate limits; no refresh age | Viable as step 0 only |
| B4. `cached` crate or similar | Macro convenience | New dep for no gain over B1/B2 | Not recommended |
| B5. Reuse OpenRouter `ModelListCache` for all providers | Already tested | OpenRouter-specific types (pricing/free filters); coupled to its scoring | Not recommended directly; copy pattern |

## (c) Angular table / drawer / waterfall

Facts (VERIFIED, `ui/package.json`): runtime deps are Angular 19 core packages, `chart.js` ^4.5.1, tailwindcss 4, rxjs, zone.js. No component library (no Material/CDK/PrimeNG/ag-grid/d3). Existing components: `recent-trace-table`, `provider-health-grid`, `metric-kpi-card`, `rpm-trend-chart` and `latency-percentile-chart` (chart.js), `session-replay-view`.

| Option | Pros | Cons | Verdict |
|---|---|---|---|
| C1. Extend `recent-trace-table.component.ts` + new standalone drawer component (Tailwind, `@if`/signals) + waterfall as plain flex/CSS bars sized by `latency / total` | Zero new deps; bundle stays small and air-gapped; attempt chains are few rows, so CSS bars are enough; consistent with existing style | Hand-rolled accessibility (focus trap, Esc) | **Recommended** |
| C2. chart.js stacked/floating horizontal bar for waterfall | Already bundled; reuses chart patterns in repo | Poor fit for labeled, clickable rows; tooltip/label work; heavier than CSS | Viable |
| C3. `@angular/cdk` (overlay/table/a11y) | Mature drawer/focus handling | New dep (not in package.json); adds Bazel/lockfile churn | Not recommended unless a11y demands it |
| C4. Material/PrimeNG/ag-grid/d3/vis-timeline | Feature-rich | Large new deps, theming conflicts with Tailwind 4, bundle size | Not recommended |

## Overall
Build all three in-house on existing deps: A1 (extend ring buffer with attempts), B1 (cached catalog with timestamps/errors, patterned on `LocalCatalog` and the OpenRouter cache), C1 (Tailwind drawer plus CSS waterfall). No new crate or npm package is required. Unchecked (INFERRED): bedrock/gemini `list_models` behavior and the exact current SSE JSON casing (root-cause item in requirements).
