# Validation Strategy & Test Suite Specification: WebUI Redesign (`webui-redesign`)

**Date**: 2026-09-27  
**Status**: Completed / Ready for Implementation  
**Requirements Reference**: [requirements.md](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/project_plans/webui-redesign/requirements.md)  
**Implementation Plan Reference**: [plan.md](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/project_plans/webui-redesign/implementation/plan.md)  
**Adversarial Review Reference**: [adversarial-review.md](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/project_plans/webui-redesign/implementation/adversarial-review.md)  

---

## Overview

This document defines the comprehensive test validation suite and requirement-to-test traceability matrix for the `webui-redesign` project. It ensures that 100% of functional and non-functional requirements specified in [requirements.md](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/project_plans/webui-redesign/requirements.md) are backed by concrete tasks in [plan.md](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/project_plans/webui-redesign/implementation/plan.md) and covered by explicit unit, integration, end-to-end (E2E), and non-functional performance/security test cases.

---

## Requirement-to-Test Traceability Matrix

| Requirement ID | Requirement Description | Plan Task Mapping | Test Case ID | Test Category | Validation Description |
|---|---|---|---|---|---|
| **REQ-FUNC-01** | Embedded Asset Handler & SPA Fallback Route | Task 1.1.1, Task 1.1.2, Task 1.1.3 | `TC-UNIT-01`, `TC-UNIT-02`, `TC-INT-01`, `TC-INT-02`, `TC-INT-03` | Unit & Integration | Verify query string stripping, MIME type inference, ETag generation, Cache-Control headers, and SPA fallback to `index.html` on extensionless routes. |
| **REQ-FUNC-02** | CLI `--web-ui=legacy|angular` Flag & Fallback | Task 1.1.4 | `TC-UNIT-03`, `TC-INT-04` | Unit & Integration | Verify CLI flag parsing for `--web-ui` and conditional routing to legacy single-file HTML when specified. |
| **REQ-FUNC-03** | Axum SSE Telemetry Streaming Engine | Task 1.2.1, Task 1.2.2, Task 1.2.3 | `TC-UNIT-04`, `TC-INT-05`, `TC-INT-06` | Unit & Integration | Verify `GET /v1/dashboard/events` stream framing, 15s keep-alive comments, and synthetic `system_lag` warning event recovery on broadcast channel lag. |
| **REQ-FUNC-04** | Telemetry Event Instrumentation & Background Ticker | Task 1.3.1, Task 1.3.2, Task 1.3.3 | `TC-UNIT-05`, `TC-UNIT-06`, `TC-INT-07` | Unit & Integration | Assert request execution traces, error tracker events, and 1-second background `MetricsTickData` ticker broadcasts. |
| **REQ-FUNC-05** | Angular 19 SPA Setup & Local Bundling | Task 2.1.1, Task 2.1.2, Task 2.1.3 | `TC-UNIT-07`, `TC-E2E-01` | Unit & E2E | Confirm Angular standalone component setup, Tailwind CSS v4 compilation, and local bundling of Chart.js v4 with zero CDN dependencies. |
| **REQ-FUNC-06** | RxJS SseService Reconnection & Teardown | Task 2.2.1, Task 2.2.2 | `TC-UNIT-08`, `TC-UNIT-09` | Unit | Validate exponential backoff reconnection logic (1s..30s, capped retries) and `DestroyRef` socket teardown on unmount. |
| **REQ-FUNC-07** | Signal State Stores & Bounded Rolling Buffers | Task 2.3.1, Task 2.3.2 | `TC-UNIT-10`, `TC-UNIT-11` | Unit | Assert `MetricsStore` caps trace history at 100 items prepended, and test `SessionStore`, `BenchmarkStore`, `ConfigStore` state updates. |
| **REQ-FUNC-08** | App Shell Header & Connection Indicator | Task 2.4.1 | `TC-UNIT-12` | Unit | Verify top header navigation links, router active styling, and real-time SSE connection status badge rendering (Connected / Reconnecting). |
| **REQ-FUNC-09** | Overview KPI Cards & Provider Health Grid | Task 3.1.1, Task 3.1.2, Task 3.2.1, Task 3.2.2 | `TC-UNIT-13`, `TC-UNIT-14` | Unit | Test real-time RPM, TPS, TTFT, token savings summary rendering, and provider health states (`Healthy`, `Degraded`, `In Cooldown` timer, `Auth Error`). |
| **REQ-FUNC-10** | High-Frequency Chart.js Widgets & Teardown | Task 3.3.1, Task 3.3.2 | `TC-UNIT-15`, `TC-UNIT-16` | Unit | Verify sliding window dataset caps (max 60 data points via `.shift()`) and explicit `ngOnDestroy` execution calling `chartInstance.destroy()`. |
| **REQ-FUNC-11** | Live Scrolling Request Trace Feed Widget | Task 3.4.1 | `TC-UNIT-17` | Unit | Validate live scrolling trace table binding, timestamp ordering, and HTTP status code color tags. |
| **REQ-FUNC-12** | Session Replay REST APIs & Stage Fetching | Task 4.1.1, Task 4.1.2 | `TC-INT-08`, `TC-INT-09` | Integration | Assert `GET /v1/dashboard/sessions` search/pagination parameters and `GET /requests/{id}?stage=original|compressed` payload stage retrieval. |
| **REQ-FUNC-13** | Session List UI & Execution Trace Timeline | Task 4.2.1, Task 4.2.2 | `TC-UNIT-18`, `TC-UNIT-19` | Unit | Test searchable session table filtering and step-by-step pipeline execution timeline node rendering with duration attribution. |
| **REQ-FUNC-14** | Side-by-Side Payload Diff & Token Breakdown | Task 4.3.1, Task 4.3.2 | `TC-UNIT-20`, `TC-UNIT-21` | Unit | Validate split-pane / unified diff generator highlighting added/modified/omitted text blocks and segmented token breakdown bar rendering. |
| **REQ-FUNC-15** | Interactive Request Sandbox & Replay Runner | Task 4.4.1 | `TC-UNIT-22` | Unit | Verify payload editing in playground editor and "Replay Request" action sending `POST /v1/messages`. |
| **REQ-FUNC-16** | Model Benchmarking Telemetry API | Task 5.1.1 | `TC-INT-10` | Integration | Test `GET /v1/dashboard/benchmark` returning TTFT & duration percentiles ($p_{50}, p_{90}, p_{95}, p_{99}$), error rates, static & composite scores. |
| **REQ-FUNC-17** | Latency Percentile Charting & Scorecard | Task 5.2.1, Task 5.2.2, Task 5.3.1, Task 5.3.2 | `TC-UNIT-23`, `TC-UNIT-24`, `TC-UNIT-25` | Unit | Test multi-series percentile line graph, max 60 point capping, `ngOnDestroy` teardown, CDF toggle, sortable benchmark table, and error matrix. |
| **REQ-FUNC-18** | Dynamic Config REST API with Mutex Safety | Task 6.1.1 | `TC-INT-11`, `TC-INT-12`, `TC-INT-13` | Integration | Assert `GET` and `PUT /v1/dashboard/config` thread-safe `tokio::sync::Mutex` disk/memory persistence, `ArcSwap` router hot-swap, and event broadcast. |
| **REQ-FUNC-19** | API Key Preservation & SSRF Safety Controls | Task 6.1.2 | `TC-UNIT-26`, `TC-UNIT-27`, `TC-INT-14` | Unit & Integration | Verify masked key detection (`sk-...****`) retaining active unmasked keys, strict SSRF `url::Url` IP validation blocking forbidden CIDRs, CORS, and bearer token auth. |
| **REQ-FUNC-20** | Visual Routing Config UI & Hot Reloading | Task 6.2.1, Task 6.3.1, Task 6.3.2, Task 6.4.1, Task 6.4.2 | `TC-UNIT-28`, `TC-UNIT-29`, `TC-UNIT-30`, `TC-UNIT-31`, `TC-UNIT-32` | Unit | Test percentage weight sliders (100% total), masked key form preservation, fallback cascade ordering, rate limit forms, TOML diff modal, and hot reload toasts. |
| **REQ-FUNC-21** | Cargo `build.rs` & CI / Release Packaging | Task 7.1.1, Task 7.1.2, Task 7.1.3, Task 7.2.1, Task 7.2.2, Task 7.2.3 | `TC-INT-15`, `TC-E2E-02` | Integration & E2E | Verify `build.rs` `cargo:rerun-if-changed` directives, stub generator fallback, GitHub Actions Node 20 setup, `cargo-dist` release compilation, and single-binary packaging. |
| **REQ-NFR-01** | Performance SLO (60 FPS & <50ms SSE Latency) | Task 1.2.3, Task 3.3.1 | `TC-NFR-01`, `TC-NFR-02` | Non-Functional | Verify backend SSE event broadcast latency < 50ms and frontend 60 FPS rendering under heavy metrics streaming load. |
| **REQ-NFR-02** | Scalability & Memory Retention Hygiene | Task 2.3.1, Task 3.3.2, Task 5.2.1 | `TC-NFR-03`, `TC-NFR-04` | Non-Functional | Assert heap memory remains stable over 10,000 streamed events with zero memory leaks via capped rolling arrays and explicit `ngOnDestroy` chart destruction. |
| **REQ-NFR-03** | Security, CORS & SSRF Protection | Task 1.1.3, Task 6.1.2 | `TC-NFR-05`, `TC-NFR-06` | Security | Assert 100% rejection of SSRF payload attempts targeting private IPs (`169.254.169.254`, `127.0.0.1`, `10.0.0.1`) and verify zero plain-text key leaks in responses. |
| **REQ-NFR-04** | Air-Gapped / Offline Delivery | Task 2.1.3, Task 7.2.3 | `TC-NFR-07` | Non-Functional | Launch compiled single Rust binary in air-gapped environment without internet connectivity and verify 100% WebUI asset and telemetry operation. |

---

## Detailed Test Suite Specifications

### 1. Unit Test Suite (32 Test Cases)

- **Backend Rust Unit Tests (`src/dashboard.rs`, `src/entrypoint/events.rs`, `src/entrypoint/api.rs`)**:
  - `TC-UNIT-01`: Query parameter stripping on embedded asset lookups (`/dashboard/main.js?v=1.0`).
  - `TC-UNIT-02`: Asset response header generation (Cache-Control, Content-Type, SHA-256 ETags).
  - `TC-UNIT-03`: CLI flag parser for `--web-ui=legacy|angular` in `src/config/mod.rs`.
  - `TC-UNIT-04`: `DashboardEvent` enum JSON serialization matching TypeScript type contracts.
  - `TC-UNIT-05`: `MetricsCollector` broadcasting `RequestTrace` event on request completion.
  - `TC-UNIT-06`: `ErrorTracker` broadcasting `ErrorLogged` event on 429/5xx error registration.
  - `TC-UNIT-26`: API key masking pattern matching (`sk-...****`) and retention of active unmasked keys on config PUT.
  - `TC-UNIT-27`: SSRF URL validation rejecting loopback (`127.0.0.1`), link-local (`169.254.169.254`), private CIDRs (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`), and non-HTTPS schemes.

- **Frontend Angular Unit Tests (`ui/src/app/...`)**:
  - `TC-UNIT-07`: Angular standalone app routing and input binding initialization.
  - `TC-UNIT-08`: `SseService` Jasmine/Karma test verifying exponential backoff reconnection strategy.
  - `TC-UNIT-09`: `SseService` `DestroyRef` teardown test asserting `eventSource.close()` invocation.
  - `TC-UNIT-10`: `MetricsStore` rolling array capping at 100 traces with prepended order.
  - `TC-UNIT-11`: `SessionStore`, `BenchmarkStore`, `ConfigStore` Signal state updates.
  - `TC-UNIT-12`: `AppShellComponent` link active states and SSE connection badge rendering.
  - `TC-UNIT-13`: `KpiCardComponent` formatted number rendering and trend indicators.
  - `TC-UNIT-14`: `ProviderHealthGridComponent` rendering `Healthy`, `Degraded`, `In Cooldown` timer, and `Auth Error` badges.
  - `TC-UNIT-15`: `RpmTrendChartComponent` sliding window dataset capping at max 60 data points via `.shift()`.
  - `TC-UNIT-16`: `RpmTrendChartComponent` `ngOnDestroy` hook invoking `chartInstance.destroy()`.
  - `TC-UNIT-17`: `LiveTraceFeedComponent` DOM row binding and status tag styling.
  - `TC-UNIT-18`: `SessionListComponent` real-time search filter and selection event emission.
  - `TC-UNIT-19`: `TraceTimelineComponent` execution step timeline rendering and stage duration display.
  - `TC-UNIT-20`: `PayloadDiffInspectorComponent` split-pane / unified diff generator.
  - `TC-UNIT-21`: `TokenBreakdownBarComponent` token proportion calculation and segment widths.
  - `TC-UNIT-22`: `RequestPlaygroundComponent` JSON payload editing and HttpClient `POST /v1/messages` submission.
  - `TC-UNIT-23`: `LatencyPercentileChartComponent` percentile plotting, max 60 point capping, CDF toggle, and `ngOnDestroy` teardown.
  - `TC-UNIT-24`: `BenchmarkTableComponent` table column sorting.
  - `TC-UNIT-25`: `ErrorDistributionMatrixComponent` error categorization matrix.
  - `TC-UNIT-28`: `RoutingConfigComponent` weight sliders total 100% calculation and masked key preservation.
  - `TC-UNIT-29`: `FallbackCascadeBuilderComponent` failover sequence re-ordering.
  - `TC-UNIT-30`: `RateLimitFormDirectiveComponent` numerical input validation.
  - `TC-UNIT-31`: `ConfigDiffModalComponent` TOML/JSON diff formatting.
  - `TC-UNIT-32`: "Apply Config" action triggering toast notifications and validation banners.

---

### 2. Integration Test Suite (15 Test Cases)

- `TC-INT-01`: `GET /dashboard/main.js` returns HTTP 200 OK with `application/javascript` and SHA-256 ETag.
- `TC-INT-02`: `GET /dashboard/sessions` (SPA fallback route) returns HTTP 200 OK with `index.html`.
- `TC-INT-03`: `GET /dashboard/missing.xyz` returns HTTP 404 Not Found.
- `TC-INT-04`: Booting binary with `--web-ui=legacy` serves legacy single-file `DASHBOARD_HTML`.
- `TC-INT-05`: `GET /v1/dashboard/events` returns `text/event-stream` and 15s keep-alive comments.
- `TC-INT-06`: Broadcast channel lag emits synthetic `system_lag` warning event without closing SSE stream.
- `TC-INT-07`: 1-second background ticker task broadcasts `MetricsTickData` snapshots over 3 seconds.
- `TC-INT-08`: `GET /v1/dashboard/sessions` accepts `limit`, `cursor`, `search` parameters and returns valid session list JSON.
- `TC-INT-09`: `GET /requests/{id}?stage=original` and `?stage=compressed` return corresponding JSON payload snapshots.
- `TC-INT-10`: `GET /v1/dashboard/benchmark` returns latency percentiles ($p_{50}, p_{90}, p_{95}, p_{99}$), static, and composite scores.
- `TC-INT-11`: `GET /v1/dashboard/config` returns active routing configuration JSON.
- `TC-INT-12`: `PUT /v1/dashboard/config` persists updates to disk, hot-swaps `ArcSwap` router, and emits `ConfigChanged` event.
- `TC-INT-13`: Concurrent `PUT /v1/dashboard/config` requests execute safely under `tokio::sync::Mutex` without state drift or race conditions.
- `TC-INT-14`: Non-loopback config mutations without `X-Consolette-Auth` header return HTTP 401 Unauthorized.
- `TC-INT-15`: Cargo `build.rs` emits `cargo:rerun-if-changed` directives and generates stub `index.html` if missing.

---

### 3. End-to-End & Browser / UX Acceptance Test Suite (2 Test Cases)

- `TC-E2E-01`: Air-Gapped Asset Verification: Inspect Angular `dist/browser/` build output to confirm zero external CDN URL references (`cdn.jsdelivr.net`).
- `TC-E2E-02`: Single-Binary Executable Verification: Build release binary (`cargo build --release`), launch executable, and verify complete multi-page Angular SPA delivery at `http://localhost:8080/dashboard` without external disk dependencies.

---

### 4. Non-Functional & Security Test Suite (7 Test Cases)

- `TC-NFR-01`: SSE Event Broadcast Latency: Measure time from request completion to SSE event arrival (< 50ms SLO).
- `TC-NFR-02`: 60 FPS Rendering SLO: Verify smooth 60 FPS canvas rendering during continuous high-frequency metrics ticks.
- `TC-NFR-03`: Long-Running Heap Memory Retention: Stream 10,000 events over 1 hour and assert zero memory growth or leaks in Angular client heap.
- `TC-NFR-04`: Component Teardown Hygiene: Assert all Chart.js instances and SSE sockets are destroyed cleanly when navigating across routes.
- `TC-NFR-05`: SSRF Security Penetration Test: Submit malicious `base_url` payloads (`http://169.254.169.254`, `http://127.0.0.1:22`, `http://10.0.0.1`) to `PUT /v1/dashboard/config` and verify 100% rejection.
- `TC-NFR-06`: API Key Leakage Audit: Verify no plain-text API keys are returned in `GET /v1/dashboard/config` or client console logs.
- `TC-NFR-07`: Air-Gapped Offline Execution: Disconnect internet interface, run `consolette` binary, and verify complete offline dashboard functionality.

---

## Test Inventory Summary

- **Unit Test Cases**: 32
- **Integration Test Cases**: 15
- **E2E / UX Acceptance Test Cases**: 2
- **Non-Functional & Security Test Cases**: 7
- **Total Test Cases**: **56**

---

## Implementation Readiness Gate Evaluation

| # | Readiness Gate Criterion | Required State | Actual State | Verdict |
|---|---|---|---|---|
| **1** | **Requirements Coverage** | 100% of functional & non-functional requirements mapped in `plan.md` & `validation.md` | 25 / 25 Requirements Mapped (100%) | **PASS** |
| **2** | **Plan Completeness** | All Epics/Stories have concrete tasks, acceptance criteria, & test instructions without TODOs | 7 Epics, 24 Stories, 56 Tasks Fully Specified | **PASS** |
| **3** | **Review Resolution** | Adversarial review verdict is CLEAN or CONCERNS (not BLOCKED) | All 6 Blockers Resolved, Verdict is **CLEAN** | **PASS** |
| **4** | **Validation Traceability** | Test suite covers Unit, Integration, E2E, and Non-Functional (Security, Memory, SLO, Air-Gapped) | 56 Explicit Test Cases Defined Across All 4 Tiers | **PASS** |

### Overall Readiness Gate Verdict: **PASS**

The `webui-redesign` project satisfies all quality, coverage, security, architectural, and validation criteria. The project is fully approved to proceed to Phase 5 (`sdd/5-implement`).
