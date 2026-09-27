# Implementation Plan: WebUI Redesign (`webui-redesign`)

**Date**: 2026-09-27  
**Status**: Ready for Implementation (Patched & Remediated)  
**Requirements Reference**: [requirements.md](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/project_plans/webui-redesign/requirements.md)  
**Adversarial Review Reference**: [adversarial-review.md](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/project_plans/webui-redesign/implementation/adversarial-review.md)  
**Research Documents**:
- [stack.md](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/project_plans/webui-redesign/research/stack.md)
- [features.md](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/project_plans/webui-redesign/research/features.md)
- [architecture.md](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/project_plans/webui-redesign/research/architecture.md)
- [pitfalls.md](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/project_plans/webui-redesign/research/pitfalls.md)

---

## Overview

This implementation plan outlines the engineering work required to replace Consolette's legacy single-file inline HTML/JS dashboard (`src/dashboard.rs`) with a modern, embedded Angular 19+ Single-Page Application (SPA) driven by an Axum 0.8 Server-Sent Events (SSE) streaming backend.

The new WebUI delivers real-time metrics streaming, interactive session replay with original vs. compressed prompt diffing, model benchmarking across upstreams, and a dynamic proxy configuration editor with hot-reloading—all packaged inside a single, zero-dependency Rust executable.

```mermaid
flowchart TD
    subgraph Client ["Angular 19 SPA Frontend (ui/)"]
        Nav["Router & Navigation Shell"]
        Overview["Overview View (RPM, Health, SSE Feed)"]
        Replay["Session Replay View (Trace Tree, Diff Inspector)"]
        Bench["Benchmarking View (TTFT, Percentiles)"]
        ConfigUI["Config Editor View (Routing Sliders, Cascades)"]
        Stores["Signal Stores (Metrics, Session, Benchmark, Config)"]
        SSEService["RxJS SseService (/v1/dashboard/events)"]
        
        Nav --> Overview & Replay & Bench & ConfigUI
        Overview & Replay & Bench & ConfigUI --> Stores
        Stores <-- SSEService
    end

    subgraph Backend ["Consolette Rust Binary (Axum 0.8)"]
        Router["Axum Router (entrypoint_router)"]
        AssetHandler["rust-embed Handler (/dashboard/* & /assets/*)"]
        SSEHandler["SSE Telemetry Broadcaster (/v1/dashboard/events)"]
        SessionAPI["Session Replay API (/v1/dashboard/sessions, /requests/{id})"]
        BenchAPI["Benchmarking API (/v1/dashboard/benchmark)"]
        ConfigAPI["Config Management API (/v1/dashboard/config)"]
        EventBus["tokio::sync::broadcast::Sender<DashboardEvent>"]
        
        Router --> AssetHandler & SSEHandler & SessionAPI & BenchAPI & ConfigAPI
        SSEHandler <-- EventBus
    end

    SSEService <== "SSE Stream" ==> SSEHandler
    AssetHandler <== "Embedded SPA Assets" ==> Nav
```

---

## Epic 1: Rust Backend & Axum 0.8 SSE Streaming & Embedded Asset Handler Infrastructure

### Story 1.1: Embedded Static Asset Handler & SPA Fallback Route Integration
Provide zero-dependency static asset embedding inside the Rust binary using `rust-embed` and Axum 0.8 wildcard routing with HTML5 client-side fallback handling.

#### Task 1.1.1: Add `rust-embed`, `arc-swap`, `mime_guess`, `tokio-stream`, and `tower-http` Dependencies
- **Description**: Add required backend dependencies to `Cargo.toml`: `rust-embed` (v8.5), `arc-swap` (v1.7 for atomic router swapping), `mime_guess` (v2.0 for static asset content-type detection), `tokio-stream` (v0.1 for broadcast stream mapping), and `tower-http` (v0.6 with `cors` feature). Define `DashboardAssets` struct in `src/dashboard.rs` pointing to `ui/dist/consolette/browser`.
- **Target Files**:
  - `Cargo.toml`
  - `src/dashboard.rs`
- **Acceptance Criteria**:
  - `Cargo.toml` explicitly specifies `rust-embed = "8.5"`, `arc-swap = "1.7"`, `mime_guess = "2.0"`, `tokio-stream = "0.1"`, and `tower-http = { version = "0.6", features = ["cors"] }`.
  - `DashboardAssets` struct derives `RustEmbed` with `folder = "ui/dist/consolette/browser"`.
  - Rust code compiles cleanly with `cargo check`.
- **Test Instructions**:
  - Run `cargo check` to verify macro parsing and crate dependencies compilation.

#### Task 1.1.2: Implement Axum 0.8 Static Asset & SPA Fallback Handler
- **Description**: Implement `serve_embedded_asset` handler in `src/dashboard.rs`. Inspect URI paths (stripping query parameters and percent-encoded paths via `uri.path()`), infer MIME types via `mime_guess`, attach SHA-256 ETags, set `Cache-Control` headers (`no-cache` for `index.html`, `immutable` for hashed assets), and fall back to serving `index.html` on 404 for non-file SPA routes.
- **Target Files**:
  - `src/dashboard.rs`
- **Acceptance Criteria**:
  - Strips query parameters (e.g. `/dashboard/main.js?v=1.0`) so asset lookups in `DashboardAssets::get()` succeed without 404 errors.
  - Exact file requests (e.g. `/dashboard/main.js`) return file data with appropriate MIME type and ETag headers.
  - Hashed static assets include `Cache-Control: public, max-age=31536000, immutable`.
  - `index.html` includes `Cache-Control: no-cache, no-store, must-revalidate`.
  - Route paths without file extensions (e.g. `/dashboard/sessions`) return `index.html` with status `200 OK`.
- **Test Instructions**:
  - Run unit tests in `src/dashboard.rs` verifying asset lookup, query parameter stripping, header generation, and fallback behavior.

#### Task 1.1.3: Wire Axum Router Static Routes & CORS Middleware in Entrypoint
- **Description**: Register static asset routes (`/dashboard`, `/dashboard/`, `/dashboard/{*path}`) in `entrypoint_router()` in `src/entrypoint/mod.rs`. Attach `tower_http::cors::CorsLayer` middleware to the Axum router restricting allowed origins and methods for security.
- **Target Files**:
  - `src/entrypoint/mod.rs`
- **Acceptance Criteria**:
  - `GET /dashboard` and `GET /dashboard/*` correctly map to `serve_embedded_asset`.
  - Router attaches `tower_http::cors::CorsLayer` restricting unauthorized cross-origin requests.
  - Server boots cleanly without routing conflicts.
- **Test Instructions**:
  - Run `cargo test entrypoint_router` to confirm route registration and CORS middleware attachment.

#### Task 1.1.4: Implement `--web-ui=legacy|angular` CLI Flag Parsing & Route Fallback
- **Description**: Implement CLI flag parsing for `--web-ui=legacy|angular` in `src/config/mod.rs` and `src/bin/consolette.rs`. When set to `--web-ui=legacy`, `entrypoint_router()` routes `/dashboard` to the legacy single-file `DASHBOARD_HTML` handler instead of embedded Angular SPA assets.
- **Target Files**:
  - `src/config/mod.rs`
  - `src/bin/consolette.rs`
  - `src/entrypoint/mod.rs`
- **Acceptance Criteria**:
  - CLI parses `--web-ui` with values `angular` (default) or `legacy`.
  - Setting `--web-ui=legacy` serves the legacy single-file HTML dashboard interface.
  - Unit tests verify CLI argument parsing and conditional route binding.
- **Test Instructions**:
  - Run `cargo test config_web_ui_flag` and `cargo test entrypoint_legacy_fallback`.

---

### Story 1.2: Server-Sent Events (SSE) Telemetry Streaming Engine
Implement a high-throughput, low-latency SSE event stream endpoint yielding strongly-typed Rust JSON telemetry events to connected dashboard clients.

#### Task 1.2.1: Define Strongly-Typed `DashboardEvent` Schema
- **Description**: Create `src/entrypoint/events.rs` defining the `DashboardEvent` enum with variants: `MetricsTick(MetricsTickData)`, `RequestTrace(RequestTraceData)`, `ErrorLogged(ErrorLoggedData)`, and `ConfigChanged(ConfigChangedData)`.
- **Target Files**:
  - `src/entrypoint/events.rs`
- **Acceptance Criteria**:
  - All event payloads implement `serde::Serialize` and `Clone`.
  - Enum uses `#[serde(tag = "type", content = "data")]` for clean TypeScript deserialization.
- **Test Instructions**:
  - Add unit tests in `src/entrypoint/events.rs` verifying JSON serialization outputs for all event types.

#### Task 1.2.2: Add Broadcast Channel to Entrypoint State
- **Description**: Extend `EntrypointState` in `src/entrypoint/mod.rs` to include `tokio::sync::broadcast::Sender<DashboardEvent>` initialized with a capacity of 1024.
- **Target Files**:
  - `src/entrypoint/mod.rs`
- **Acceptance Criteria**:
  - `EntrypointState` contains `event_tx: broadcast::Sender<DashboardEvent>`.
  - `EntrypointState::build()` initializes channel and exposes a subscriber cloning method.
- **Test Instructions**:
  - Run `cargo check` to verify state struct alignment across sub-routers.

#### Task 1.2.3: Implement Axum SSE Handler with Heartbeat & Lag Handling
- **Description**: Implement `GET /v1/dashboard/events` handler in `src/entrypoint/events.rs`. Subscribe to `broadcast::Receiver`, convert events to `axum::response::sse::Event`, attach a 15s keep-alive ping, and catch `RecvError::Lagged(skipped)` by wrapping it in a synthetic warning SSE event (`Event::default().event("system_lag").data(...)`) rather than crashing or terminating the stream.
- **Target Files**:
  - `src/entrypoint/events.rs`
  - `src/entrypoint/mod.rs`
- **Acceptance Criteria**:
  - Handler returns `Sse<impl Stream>` with `Content-Type: text/event-stream`.
  - Keep-alive comments (`: ping`) fire every 15 seconds.
  - Broadcast channel lag yields a synthetic `system_lag` warning event rather than closing the stream.
- **Test Instructions**:
  - Run `cargo test sse_event_stream` to verify channel subscription, heartbeat interval, and lag recovery.

---

### Story 1.3: Metrics & Pipeline Telemetry Event Instrumentation
Connect core execution pipelines to the broadcast channel so proxy activity emits real-time events.

#### Task 1.3.1: Instrument Request Execution Traces
- **Description**: Modify `MetricsCollector` in `src/metrics/mod.rs` and request completion handlers to construct `RequestTraceData` (duration, first byte ms, tokens before/after, compression status) and publish to `event_tx`.
- **Target Files**:
  - `src/metrics/mod.rs`
  - `src/entrypoint/observability.rs`
- **Acceptance Criteria**:
  - Every completed proxy request emits a `DashboardEvent::RequestTrace` event onto the broadcast channel.
- **Test Instructions**:
  - Run `cargo test metrics_trace_broadcast` to verify event dispatch on request completion.

#### Task 1.3.2: Instrument Error Tracker Events
- **Description**: Modify `ErrorTracker` in `src/metrics/error_tracker.rs` to broadcast `DashboardEvent::ErrorLogged` when 429 rate limits, 5xx provider outages, or auth failures are registered.
- **Target Files**:
  - `src/metrics/error_tracker.rs`
- **Acceptance Criteria**:
  - Upstream provider errors instantly emit `ErrorLoggedData` containing provider ID, HTTP status, and error details.
- **Test Instructions**:
  - Add unit test simulating provider rate limit error and assert event emission.

#### Task 1.3.3: Implement 1-Second Background Telemetry Ticker
- **Description**: Create a background Tokio ticker task in `src/entrypoint/mod.rs` that runs every 1,000ms, snapshots metrics counters (RPM, total requests, tokens saved, event loop lag, provider health states), and broadcasts `MetricsTickData`.
- **Target Files**:
  - `src/entrypoint/mod.rs`
- **Acceptance Criteria**:
  - Ticker broadcasts `DashboardEvent::MetricsTick` every 1 second when active.
  - Loop lag calculation accurately measures runtime execution delay.
- **Test Instructions**:
  - Run integration test verifying ticker execution and event output over a 3-second window.

---

## Epic 2: Angular 19 SPA Frontend Project Setup, Signal Stores, and SSE Service

### Story 2.1: Angular 19 Project Bootstrap & UI Configuration
Scaffold a modern, standalone Angular 19 SPA in `ui/` with zero runtime Node/CDN dependencies.

#### Task 2.1.1: Initialize Angular 19 SPA Workspace
- **Description**: Scaffold Angular 19 SPA under `ui/` directory with strict TypeScript, standalone components, `provideRouter(routes, withComponentInputBinding())`, and `provideHttpClient(withFetch())`.
- **Target Files**:
  - `ui/package.json`
  - `ui/angular.json`
  - `ui/tsconfig.json`
  - `ui/src/app/app.config.ts`
  - `ui/src/app/app.routes.ts`
- **Acceptance Criteria**:
  - Project uses Angular 19+ standalone component syntax (no `NgModules`).
  - `npm run build` succeeds and places output in `ui/dist/consolette/browser`.
- **Test Instructions**:
  - Run `npm test` inside `ui/` to confirm clean test runner initialization.

#### Task 2.1.2: Integrate Tailwind CSS v4 & Styling Theme
- **Description**: Configure Tailwind CSS v4 using CSS-first import `@import "tailwindcss";` in `ui/src/styles.css`. Define dark mode color palette (neutral-950 background, cyan accent colors).
- **Target Files**:
  - `ui/src/styles.css`
- **Acceptance Criteria**:
  - Tailwind styles compile without errors during `ng build`.
  - Base dark mode theme applies cleanly across layout elements.
- **Test Instructions**:
  - Run `npm run build` and inspect generated CSS bundle.

#### Task 2.1.3: Install & Bundle Chart.js v4
- **Description**: Install `chart.js` into `ui/package.json`. Configure Angular build to bundle Chart.js directly into the output JS assets for offline execution.
- **Target Files**:
  - `ui/package.json`
  - `ui/angular.json`
- **Acceptance Criteria**:
  - `chart.js` is bundled locally into `dist/browser/main.js`.
  - Zero external CDN script tags (`cdn.jsdelivr.net`) exist in `index.html`.
- **Test Instructions**:
  - Search build output for external URL references to verify offline air-gapped compliance.

---

### Story 2.2: Reactive SSE Client Service & Reconnection Manager
Build an RxJS/Signal-integrated SSE client service with exponential backoff auto-reconnection and explicit teardown.

#### Task 2.2.1: Build `SseService` EventSource Wrapper with Exponential Backoff
- **Description**: Create `SseService` (`ui/src/app/core/services/sse.service.ts`) wrapping native `EventSource('/v1/dashboard/events')` in an RxJS `Observable`. Implement exponential backoff reconnection retry logic starting at 1s up to a maximum of 30s with a maximum retry attempt counter to prevent infinite reconnection loops.
- **Target Files**:
  - `ui/src/app/core/services/sse.service.ts`
- **Acceptance Criteria**:
  - Service exposes `events$: Observable<DashboardEvent>` and connection status signal (`isConnected`).
  - Disconnects trigger exponential backoff reconnection retry logic (1s, 2s, 4s... max 30s) up to max retry attempts.
- **Test Instructions**:
  - Add Jasmine/Karma unit test mocking `EventSource` and verifying exponential backoff retry behavior.

#### Task 2.2.2: Implement `DestroyRef` Socket Teardown
- **Description**: Register `DestroyRef` cleanup callbacks in `SseService` to explicitly invoke `eventSource.close()` on service/component destruction.
- **Target Files**:
  - `ui/src/app/core/services/sse.service.ts`
- **Acceptance Criteria**:
  - Unsubscribing or destroying host views immediately closes the underlying HTTP SSE socket.
- **Test Instructions**:
  - Verify socket termination in unit test during teardown lifecycle.

---

### Story 2.3: Global Angular Signal State Stores
Create modular Signal stores for managing application state with bounded rolling memory limits.

#### Task 2.3.1: Implement `MetricsStore` with Capped Rolling Buffer
- **Description**: Create `MetricsStore` (`ui/src/app/core/stores/metrics.store.ts`) subscribing to `SseService`. Store latest `MetricsTickData` and keep a bounded rolling array of recent `RequestTraceData` prepending new trace events (`[newTrace, ...traces]`) and capping at 100 items (`.slice(0, 100)`).
- **Target Files**:
  - `ui/src/app/core/stores/metrics.store.ts`
- **Acceptance Criteria**:
  - Exposes `metrics`, `recentTraces`, `rpm`, `currentLagMs`, and `isContended` computed signals.
  - Prepending new traces and calling `.slice(0, 100)` guarantees latest 100 traces are retained without memory growth.
- **Test Instructions**:
  - Run store unit test pushing 150 traces and assert array length remains capped at 100 with newest trace at index 0.

#### Task 2.3.2: Implement `SessionStore`, `BenchmarkStore`, and `ConfigStore`
- **Description**: Create dedicated Signal stores for sessions (`session.store.ts`), model benchmarks (`benchmark.store.ts`), and configuration state (`config.store.ts`).
- **Target Files**:
  - `ui/src/app/core/stores/session.store.ts`
  - `ui/src/app/core/stores/benchmark.store.ts`
  - `ui/src/app/core/stores/config.store.ts`
- **Acceptance Criteria**:
  - Stores expose clean read-only Signals and REST fetch/update methods.
- **Test Instructions**:
  - Run store unit tests verifying state updates on HTTP mock responses.

---

### Story 2.4: Core Layout & Navigation Shell
Build the top-level application shell with responsive navigation and real-time connectivity status.

#### Task 2.4.1: Build `AppShellComponent` & Navigation Header
- **Description**: Create `AppShellComponent` (`ui/src/app/core/layout/app-shell.component.ts`) containing top bar (logo, version badge, SSE connection status indicator) and sidebar router links (`Overview`, `Session Replay`, `Model Benchmarks`, `Config Editor`).
- **Target Files**:
  - `ui/src/app/core/layout/app-shell.component.ts`
- **Acceptance Criteria**:
  - Displays live SSE connection status badge (Green = Connected, Yellow = Reconnecting).
  - Sidebar correctly updates active link styling on route changes.
- **Test Instructions**:
  - Run component DOM test checking router link targets and badge state rendering.

---

## Epic 3: Real-Time Metrics & Upstream Health Overview Dashboard

### Story 3.1: Key Performance Indicator (KPI) Metric Cards
Display real-time proxy performance metrics with Signal-driven auto-updating cards.

#### Task 3.1.1: Build `KpiCardComponent` Widget
- **Description**: Create `KpiCardComponent` (`ui/src/app/features/overview/components/kpi-card.component.ts`) accepting title, value, unit, trend delta, and status color inputs.
- **Target Files**:
  - `ui/src/app/features/overview/components/kpi-card.component.ts`
- **Acceptance Criteria**:
  - Formats large numbers cleanly (e.g. 1.2M tokens saved).
  - Highlights positive/negative trends visually.
- **Test Instructions**:
  - Run DOM test verifying input bindings and template rendering.

#### Task 3.1.2: Implement Overview KPI Summary Bar
- **Description**: Create `OverviewComponent` (`ui/src/app/features/overview/overview.component.ts`) rendering 4 core KPI cards: RPM (Requests Per Minute), TPS (Tokens Per Second), Median TTFT, and Overall Token Savings %.
- **Target Files**:
  - `ui/src/app/features/overview/overview.component.ts`
- **Acceptance Criteria**:
  - KPI cards update dynamically as `MetricsStore` receives new `MetricsTick` events.
- **Test Instructions**:
  - Simulate SSE `MetricsTick` event in test and verify text content updates.

---

### Story 3.2: Provider Health & Cooldown Badges
Visualize upstream provider operational status, rate limit cooldowns, and error states.

#### Task 3.2.1: Build `ProviderHealthGridComponent`
- **Description**: Create `ProviderHealthGridComponent` displaying status cards for each provider target (`Anthropic`, `AWS Bedrock`, `OpenAI`).
- **Target Files**:
  - `ui/src/app/features/overview/components/provider-health-grid.component.ts`
- **Acceptance Criteria**:
  - Displays dynamic badges: `Healthy` (Green), `Degraded` (Yellow), `In Cooldown` (Orange), `Auth Error` (Red).
  - Shows countdown timer for providers in cooldown (e.g., `Cooldown: 12s remaining`).
- **Test Instructions**:
  - Run component test with mock provider health data across all 4 status states.

#### Task 3.2.2: Add Cooldown Flash & Status Shift Animations
- **Description**: Add visual transition animations when a provider enters cooldown or recovers to healthy state.
- **Target Files**:
  - `ui/src/app/features/overview/components/provider-health-grid.component.ts`
- **Acceptance Criteria**:
  - Status transitions trigger a subtle highlight animation.
- **Test Instructions**:
  - Verify CSS class application during status changes in test runner.

---

### Story 3.3: High-Frequency Chart.js Telemetry Widgets
Render 60 FPS canvas charts for request volume and event loop lag.

#### Task 3.3.1: Build `RpmTrendChartComponent` with Sliding Window Cap
- **Description**: Create `RpmTrendChartComponent` (`ui/src/app/features/overview/components/rpm-trend-chart.component.ts`) wrapping Chart.js canvas. Render rolling time-series graph of RPM and loop lag (ms). Enforce a sliding window array cap (max 60 data points) using `.shift()` on dataset updates (`if (chart.data.labels.length > 60) { chart.data.labels.shift(); chart.data.datasets.forEach(d => d.data.shift()); }`) to prevent memory growth and canvas render lag.
- **Target Files**:
  - `ui/src/app/features/overview/components/rpm-trend-chart.component.ts`
- **Acceptance Criteria**:
  - Canvas updates smoothly on incoming metrics ticks without redrawing full DOM tree.
  - Sliding window array cap (max 60 data points via `.shift()`) prevents memory accumulation over extended streaming sessions.
  - Dark mode chart grid lines and colors match application theme.
- **Test Instructions**:
  - Test chart initialization and dataset push operations under high tick counts, asserting array length cap at 60.

#### Task 3.3.2: Implement Chart Lifecycle Teardown
- **Description**: Register `ngOnDestroy` in `RpmTrendChartComponent` calling `chartInstance.destroy()`.
- **Target Files**:
  - `ui/src/app/features/overview/components/rpm-trend-chart.component.ts`
- **Acceptance Criteria**:
  - `chartInstance.destroy()` is invoked on component unmount, preventing memory leaks.
- **Test Instructions**:
  - Assert `.destroy()` call count during component teardown in unit test.

---

### Story 3.4: Live Event Trace Feed Widget
Display a live scrolling table of incoming proxy request execution traces.

#### Task 3.4.1: Build `LiveTraceFeedComponent`
- **Description**: Create `LiveTraceFeedComponent` rendering recent request traces from `MetricsStore.recentTraces`.
- **Target Files**:
  - `ui/src/app/features/overview/components/live-trace-feed.component.ts`
- **Acceptance Criteria**:
  - Displays columns: Timestamp, Request ID, Provider/Model, Duration, TTFT, Token Savings %, HTTP Status.
  - Displays colored HTTP status tags (`200 OK` green, `429` orange, `500` red).
- **Test Instructions**:
  - Run component test verifying table row binding against mock trace array.

---

## Epic 4: Session Replay & Interactive Payload Inspector (Original vs Compressed Diff)

### Story 4.1: Session Replay API & In-Memory Payload Retrieval
Extend Rust backend endpoints to serve session transcripts and payload stage snapshots.

#### Task 4.1.1: Implement `GET /v1/dashboard/sessions` REST Handler
- **Description**: Extend `src/entrypoint/api.rs` to expose `GET /v1/dashboard/sessions` with `limit`, `cursor`, and `search` query parameters, returning active session metadata and token savings summaries.
- **Target Files**:
  - `src/entrypoint/api.rs`
- **Acceptance Criteria**:
  - Returns JSON array of session objects containing session ID, turn count, token savings %, last active timestamp, and pin status.
- **Test Instructions**:
  - Add API integration test in `api.rs` asserting 200 OK response and JSON schema match.

#### Task 4.1.2: Upgrade `GET /requests/{id}` for Stage Payload Retrieval
- **Description**: Modify `get_request_body` in `src/entrypoint/observability.rs` to support `?stage=original|compressed`. Fetch compressed payload snapshots from `OmissionCache`/compression logs.
- **Target Files**:
  - `src/entrypoint/observability.rs`
- **Acceptance Criteria**:
  - `GET /requests/{id}?stage=original` returns full original request JSON.
  - `GET /requests/{id}?stage=compressed` returns compressed/modified request JSON.
- **Test Instructions**:
  - Add integration test fetching both stages for a recorded request ID.

---

### Story 4.2: Session Replay & Trace Tree UI
Build interactive Angular components for searching sessions and inspecting execution step timelines.

#### Task 4.2.1: Build `SessionListComponent`
- **Description**: Create `SessionListComponent` (`ui/src/app/features/sessions/session-list.component.ts`) with searchable table, model/provider filters, and pagination.
- **Target Files**:
  - `ui/src/app/features/sessions/session-list.component.ts`
- **Acceptance Criteria**:
  - Filter input filters sessions by ID, provider, or model in real time.
  - Clicking a session selects it for detailed trace viewing.
- **Test Instructions**:
  - Run component test verifying search filter logic and selection event outputs.

#### Task 4.2.2: Build `TraceTimelineComponent`
- **Description**: Create `TraceTimelineComponent` displaying step-by-step pipeline execution timeline (`Ingress` -> `RateLimit` -> `Compression` -> `Router` -> `Provider Dispatch` -> `Egress`).
- **Target Files**:
  - `ui/src/app/features/sessions/components/trace-timeline.component.ts`
- **Acceptance Criteria**:
  - Displays timing duration (ms) spent at each stage.
  - Highlights fallback retries if provider dispatch encountered errors.
- **Test Instructions**:
  - Test timeline node rendering against multi-step trace mock data.

---

### Story 4.3: Side-by-Side Payload Diff Inspector
Build a split-pane visual diff viewer comparing original vs. compressed request payloads.

#### Task 4.3.1: Build `PayloadDiffInspectorComponent`
- **Description**: Create `PayloadDiffInspectorComponent` (`ui/src/app/features/sessions/components/payload-diff-inspector.component.ts`) with side-by-side or unified diff viewing modes for JSON prompts.
- **Target Files**:
  - `ui/src/app/features/sessions/components/payload-diff-inspector.component.ts`
- **Acceptance Criteria**:
  - Highlights added (green), modified (yellow), and omitted/compressed (red) text blocks.
  - Provides toggle between Side-by-Side and Unified diff modes.
- **Test Instructions**:
  - Run component test with sample original and compressed JSON text strings.

#### Task 4.3.2: Add Granular Token Breakdown Metric Bar
- **Description**: Build token breakdown bar component displaying System Tokens, Context/History Tokens, Tokens Saved %, Generation Tokens, and Cached Tokens hit rates.
- **Target Files**:
  - `ui/src/app/features/sessions/components/token-breakdown-bar.component.ts`
- **Acceptance Criteria**:
  - Segmented horizontal progress bar visually displays relative token proportions.
- **Test Instructions**:
  - Verify width calculations and tooltips in component test runner.

---

### Story 4.4: Interactive Request Sandbox & Replay Runner
Provide an inline sandbox allowing operators to re-send modified requests through the proxy.

#### Task 4.4.1: Build `RequestPlaygroundComponent`
- **Description**: Create `RequestPlaygroundComponent` (`ui/src/app/features/sessions/components/request-playground.component.ts`) pre-populating selected request payload into an editable JSON editor with a "Replay Request" trigger button calling `POST /v1/messages`.
- **Target Files**:
  - `ui/src/app/features/sessions/components/request-playground.component.ts`
- **Acceptance Criteria**:
  - Allows editing request payload and parameters (`temperature`, `top_p`).
  - Clicking "Replay Request" sends payload and displays live stream output diff.
- **Test Instructions**:
  - Test payload submission call with HttpClient mock.

---

## Epic 5: Model Benchmarking & Telemetry Analytics Suite

### Story 5.1: Model Benchmarking Telemetry API
Expose comparative performance metrics across models and upstream providers.

#### Task 5.1.1: Implement `GET /v1/dashboard/benchmark` API Endpoint
- **Description**: Implement `GET /v1/dashboard/benchmark` in `src/entrypoint/api.rs`. Aggregate latency percentiles ($p_{50}, p_{90}, p_{95}, p_{99}$) for TTFT and total duration, cross-referencing static benchmark scores (`bench_table.rs`) and composite scores (`openrouter_scoring.rs`).
- **Target Files**:
  - `src/entrypoint/api.rs`
  - `src/routing/bench_table.rs`
  - `src/routing/openrouter_scoring.rs`
- **Acceptance Criteria**:
  - Returns comparative JSON array grouped by provider and model.
  - Includes percentiles, error frequencies, generation speeds, and cost metrics.
- **Test Instructions**:
  - Run unit test in `src/entrypoint/api.rs` verifying percentile calculation logic.

---

### Story 5.2: Latency Percentiles & TTFT Charting
Build multi-series latency distribution charts.

#### Task 5.2.1: Build `LatencyPercentileChartComponent` with Sliding Window Cap & Teardown
- **Description**: Create `LatencyPercentileChartComponent` (`ui/src/app/features/benchmarks/components/latency-percentile-chart.component.ts`) rendering multi-line area charts for $p_{50}$, $p_{90}$, and $p_{99}$ TTFT metrics across upstreams. Enforce a sliding window cap (max 60 data points) using `.shift()` on dataset updates (`if (chart.data.labels.length > 60) { chart.data.labels.shift(); chart.data.datasets.forEach(d => d.data.shift()); }`). Implement `ngOnDestroy` lifecycle hook calling `chartInstance.destroy()` to prevent memory leaks during component teardown or tab navigation.
- **Target Files**:
  - `ui/src/app/features/benchmarks/components/latency-percentile-chart.component.ts`
- **Acceptance Criteria**:
  - Plots comparative latency curves for Anthropic, Bedrock, and OpenAI upstreams.
  - Enforces sliding window array cap (max 60 data points via `.shift()`).
  - Calls `chartInstance.destroy()` in `ngOnDestroy` during component destruction.
- **Test Instructions**:
  - Verify chart dataset binding, array capping at 60 points, and `.destroy()` execution in unit tests.

#### Task 5.2.2: Add Cumulative Distribution Function (CDF) Toggle
- **Description**: Add toggle switch to convert percentile line graph into CDF distribution curve for identifying long-tail latency spikes.
- **Target Files**:
  - `ui/src/app/features/benchmarks/components/latency-percentile-chart.component.ts`
- **Acceptance Criteria**:
  - Toggle dynamically recalculates chart dataset coordinates.
- **Test Instructions**:
  - Assert dataset coordinate recalculation on toggle click.

---

### Story 5.3: Comparative Provider Scorecard & Error Matrix
Display sortable benchmark scorecards and error breakdown tables.

#### Task 5.3.1: Build `BenchmarkTableComponent`
- **Description**: Create `BenchmarkTableComponent` (`ui/src/app/features/benchmarks/components/benchmark-table.component.ts`) displaying sortable columns: Model, Provider, TTFT (ms), Speed (tok/s), Success Rate %, Cost/1k req ($), and Tokens Saved %.
- **Target Files**:
  - `ui/src/app/features/benchmarks/components/benchmark-table.component.ts`
- **Acceptance Criteria**:
  - Clicking header columns sorts rows ascending/descending.
- **Test Instructions**:
  - Test table column sorting logic in unit test.

#### Task 5.3.2: Build `ErrorDistributionMatrixComponent`
- **Description**: Create `ErrorDistributionMatrixComponent` rendering categorised error counts (`429 Rate Limit`, `401 Auth`, `5xx Outage`, `Timeout`) across upstreams.
- **Target Files**:
  - `ui/src/app/features/benchmarks/components/error-distribution-matrix.component.ts`
- **Acceptance Criteria**:
  - Highlights providers experiencing elevated error rates.
- **Test Instructions**:
  - Test matrix rendering against error distribution mock object.

---

## Epic 6: Dynamic Proxy Configuration Manager & Hot-Reloading Engine

### Story 6.1: Dynamic Configuration REST API & Safety Controls
Build secure REST endpoints for fetching and updating runtime routing configurations with synchronized hot-reloading.

#### Task 6.1.1: Implement `GET` and `PUT /v1/dashboard/config` Endpoints with Mutex Synchronization
- **Description**: Implement `GET /v1/dashboard/config` and `PUT /v1/dashboard/config` in `src/entrypoint/api.rs`. Read current routing setup, validate incoming TOML/JSON updates, write to runtime overrides file (`runtime-overrides.toml`), and hot-swap `EntrypointState::dispatch_router` via `ArcSwap::store`. Wrap file persistence and `ArcSwap::store()` calls inside an async `tokio::sync::Mutex` lock to guarantee synchronized memory/disk updates and prevent concurrent write race conditions.
- **Target Files**:
  - `src/entrypoint/api.rs`
  - `src/routing/mod.rs`
- **Acceptance Criteria**:
  - `GET /v1/dashboard/config` returns complete active configuration JSON.
  - `PUT /v1/dashboard/config` validates model selectors and provider references before applying.
  - Disk persistence and `ArcSwap::store()` updates are protected by `tokio::sync::Mutex`, preventing race conditions or disk-memory state drift during concurrent requests.
  - Valid updates hot-swap live router atomically without dropping active connections.
  - Emits `DashboardEvent::ConfigChanged` notification on broadcast channel.
- **Test Instructions**:
  - Run API integration test submitting concurrent valid and invalid config payloads and verifying thread-safe persistence.

#### Task 6.1.2: Implement API Key Preservation & Strict SSRF Safety Controls on Config API
- **Description**: Implement strict security validation and API key protection logic on `PUT /v1/dashboard/config`:
  1. **API Key Preservation**: Mask API key fields in GET responses (`sk-ant-...****`). On `PUT /v1/dashboard/config`, check if incoming API key fields match a masked string pattern (e.g. `sk-...****`). If masked, automatically retain the existing unmasked API key from the active configuration state to prevent key corruption on edit.
  2. **Strict SSRF Validation**: Parse provider `base_url` values using `url::Url` and perform explicit IP range checks. Reject loopback (`127.0.0.0/8`, `::1`), link-local (`169.254.0.0/16`, `fe80::/10`), private CIDRs (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`), local hostnames (`localhost`, `*.internal`), and non-HTTPS schemes.
  3. **Authentication & CSRF**: Enforce loopback-only binding or `X-Consolette-Auth` bearer token header, and require `Content-Type: application/json`.
- **Target Files**:
  - `src/entrypoint/api.rs`
- **Acceptance Criteria**:
  - Submitting a config payload containing masked API keys automatically retains the existing unmasked keys in `runtime-overrides.toml` and memory state.
  - Non-loopback requests without bearer token return `401 Unauthorized`.
  - `base_url` pointing to loopback, link-local, or private IPs (e.g. `169.254.169.254`, `127.0.0.1`, `10.0.0.1`, `::1`) returns `400 Bad Request` SSRF validation failure.
- **Test Instructions**:
  - Run security unit tests in `api.rs` asserting API key preservation, token auth, key masking, and strict SSRF rejection across all blocked CIDR ranges.

---

### Story 6.2: Visual Load Balancing & Routing Weight Sliders
Build interactive UI components for distributing traffic volume across upstreams.

#### Task 6.2.1: Build `RoutingConfigComponent` with Masked API Key Support
- **Description**: Create `RoutingConfigComponent` (`ui/src/app/features/config/components/routing-config.component.ts`) displaying interactive percentage sliders for load balancing volume across providers. Ensure form controls preserve masked API key placeholders without clearing them or marking fields invalid during edits.
- **Target Files**:
  - `ui/src/app/features/config/components/routing-config.component.ts`
- **Acceptance Criteria**:
  - Sliders dynamically enforce total weight sum of 100%.
  - Masked API key strings are passed back intact during config saves so the backend retains active unmasked keys.
- **Test Instructions**:
  - Test slider change handlers, weight normalization math, and masked key form state preservation in unit test.

---

### Story 6.3: Fallback Cascade Diagram & Rate Limit Rule Builder
Build visual controls for failover cascades and rate limit policy forms.

#### Task 6.3.1: Build `FallbackCascadeBuilderComponent`
- **Description**: Create `FallbackCascadeBuilderComponent` (`ui/src/app/features/config/components/fallback-cascade-builder.component.ts`) allowing visual ordering of primary and secondary fallback provider targets on 429/5xx errors.
- **Target Files**:
  - `ui/src/app/features/config/components/fallback-cascade-builder.component.ts`
- **Acceptance Criteria**:
  - Drag-and-drop or step ordering controls adjust fallback cascade sequence.
- **Test Instructions**:
  - Test re-ordering actions in component test runner.

#### Task 6.3.2: Build `RateLimitFormDirectiveComponent`
- **Description**: Create `RateLimitFormDirectiveComponent` providing input fields for RPM, TPM, concurrent request limits, and hourly budget caps ($).
- **Target Files**:
  - `ui/src/app/features/config/components/rate-limit-form.component.ts`
- **Acceptance Criteria**:
  - Validates positive numerical inputs before allowing form submission.
- **Test Instructions**:
  - Test form field validation state bindings.

---

### Story 6.4: Config Validation, Diff Viewer & Hot-Reload Trigger
Provide visual diff comparison and single-click hot reload trigger.

#### Task 6.4.1: Build `ConfigDiffModalComponent`
- **Description**: Create `ConfigDiffModalComponent` (`ui/src/app/features/config/components/config-diff-modal.component.ts`) displaying a visual TOML/JSON diff between active runtime config and draft edits before submitting.
- **Target Files**:
  - `ui/src/app/features/config/components/config-diff-modal.component.ts`
- **Acceptance Criteria**:
  - Displays clean side-by-side diff highlighting modified values.
- **Test Instructions**:
  - Test diff generator with modified config objects.

#### Task 6.4.2: Wire "Apply Config (Hot Reload)" Action
- **Description**: Connect "Apply Config" button to `ConfigStore.applyConfig()`. Display success toast notification or inline validation error banner based on API response.
- **Target Files**:
  - `ui/src/app/features/config/config-editor.component.ts`
- **Acceptance Criteria**:
  - Successful response triggers toast notification and updates active state.
  - Server validation errors display clean banner explaining invalid fields.
- **Test Instructions**:
  - Test success and failure response handling in component test runner.

---

## Epic 7: Integration, Build Automation & End-to-End Verification

### Story 7.1: Cargo `build.rs` Directives, CI Pipeline & `cargo-dist` Release Integration
Automate single-binary embedding, build tracking directives, and release asset compilation.

#### Task 7.1.1: Implement Cargo `build.rs` Asset Generator with `cargo:rerun-if-changed`
- **Description**: Create `build.rs` at crate root. Emit `println!("cargo:rerun-if-changed=ui/dist/consolette/browser");` and `println!("cargo:rerun-if-changed=ui/src");` build directives so Cargo tracks frontend changes. Check if `ui/dist/consolette/browser/index.html` exists; if missing and Node.js is unavailable, write a minimal stub `index.html` file into `ui/dist/consolette/browser/` so `rust-embed` compilation succeeds in clean environments.
- **Target Files**:
  - `build.rs`
- **Acceptance Criteria**:
  - `build.rs` emits `cargo:rerun-if-changed` directives for `ui/dist/consolette/browser` and `ui/src`.
  - Rebuilding frontend assets via `npm run build` forces Cargo to recompile `src/dashboard.rs` instead of reusing cached stub binaries.
  - `cargo build` succeeds cleanly even if `ui/dist/consolette/browser/` was empty or missing.
- **Test Instructions**:
  - Run `cargo check`, touch `ui/src/index.html`, and verify `cargo build` re-executes asset compilation steps.

#### Task 7.1.2: Update GitHub Actions CI Workflow
- **Description**: Update `.github/workflows/ci.yml` to include `actions/setup-node@v4` with Node 20 LTS. Add step `npm ci && npm run build` inside `ui/` prior to running `cargo test` and `cargo build`.
- **Target Files**:
  - `.github/workflows/ci.yml`
- **Acceptance Criteria**:
  - CI pipeline automatically builds Angular SPA assets before Rust compilation.
  - All CI jobs pass cleanly.
- **Test Instructions**:
  - Run local CI simulation check with `cargo test --all-targets`.

#### Task 7.1.3: Configure `cargo-dist` Release Pipeline Asset Compilation
- **Description**: Update release automation configuration (`dist-workspace.toml` and `.github/workflows/release.yml` generation settings) to include Node 20 setup (`actions/setup-node@v4`) and execute `npm ci && npm run build` inside `ui/` prior to `cargo-dist` binary compilation.
- **Target Files**:
  - `dist-workspace.toml`
  - `.github/workflows/release.yml`
- **Acceptance Criteria**:
  - Official release binaries produced by `cargo-dist` build compiled Angular assets beforehand, packaging production WebUI files rather than fallback stubs.
- **Test Instructions**:
  - Validate `cargo-dist` workspace configuration and verify build step order.

---

### Story 7.2: Automated Integration & Verification Testing
Verify end-to-end functionality across backend and frontend layers.

#### Task 7.2.1: Implement Embedded Asset Integration Tests
- **Description**: Create `tests/webui_integration_test.rs` asserting embedded asset handler behavior (`/dashboard`, `/dashboard/main.js`, `/dashboard/sessions`), verifying 200 OK status codes, Content-Type headers, ETags, and SPA fallbacks.
- **Target Files**:
  - `tests/webui_integration_test.rs`
- **Acceptance Criteria**:
  - Tests verify exact file retrieval and SPA HTML5 fallback logic over HTTP.
- **Test Instructions**:
  - Run `cargo test --test webui_integration_test`.

#### Task 7.2.2: Implement SSE Streaming & API Integration Tests
- **Description**: Extend `tests/webui_integration_test.rs` to connect to `GET /v1/dashboard/events`, receive initial keep-alive/metrics tick events, and query REST endpoints (`/v1/dashboard/sessions`, `/v1/dashboard/benchmark`, `/v1/dashboard/config`).
- **Target Files**:
  - `tests/webui_integration_test.rs`
- **Acceptance Criteria**:
  - All REST endpoints return expected JSON schemas and status codes.
  - SSE stream yields valid `event: message` framing.
- **Test Instructions**:
  - Run `cargo test --test webui_integration_test`.

#### Task 7.2.3: Final Executable Build & Full Verification Gate
- **Description**: Execute complete project build (`npm run build` in `ui/` followed by `cargo build --release`). Verify single output executable in `target/release/consolette` contains all embedded web assets and runs without external filesystem dependencies.
- **Target Files**:
  - `target/release/consolette`
- **Acceptance Criteria**:
  - Executable size remains optimal.
  - Executable runs standalone and serves full Angular WebUI at `http://localhost:8080/dashboard`.
- **Test Instructions**:
  - Launch compiled binary, request `/dashboard/overview`, and verify complete SPA HTML delivery.

---

## Implementation Summary & Plan Audit

- **Total Epics**: 7
- **Total Stories**: 24
- **Total Tasks**: 56

### Key Architectural Choices & Flagged Decisions
1. **Asset Embedding (`rust-embed`)**: Selected over `tower-http` (`ServeDir`) to guarantee a 100% self-contained single-binary deployment without runtime disk dependencies.
2. **Real-time Telemetry (Axum 0.8 SSE)**: Server-Sent Events chosen over WebSockets due to 100% unidirectional telemetry push requirements, lighter HTTP header framing, and native browser `EventSource` reconnection.
3. **Frontend Stack (Angular 19 + Signals + Chart.js v4)**: Angular Standalone Components paired with Signals deliver 60 FPS UI rendering. Chart.js bundled locally ensures zero CDN dependencies in air-gapped environments.
4. **CI & Developer Safety (`build.rs` Fallback & `cargo:rerun-if-changed`)**: `build.rs` fallback generator guarantees `cargo build` succeeds cleanly in CI or Rust toolchain environments where Node.js is not present, while `cargo:rerun-if-changed` directives guarantee stale stubs are rebuilt upon frontend edits.
5. **Config Security, SSRF & Key Preservation**: All config mutation endpoints enforce loopback-only binding or bearer token auth, API key masking with unmasked key retention on edits, `tokio::sync::Mutex` persistence synchronization, and strict `base_url` hostname validation to prevent SSRF and credential leaks.
6. **Release Packaging (`cargo-dist` Node Build Step)**: Release workspace tooling is configured to build Angular assets prior to binary compilation, guaranteeing release artifacts contain complete production UI assets.
