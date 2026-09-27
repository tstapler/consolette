# Adversarial Review Report: WebUI Redesign (`webui-redesign`)

**Date**: 2026-09-27  
**Target Plan**: [plan.md](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/project_plans/webui-redesign/implementation/plan.md)  
**Reviewer**: Adversarial Reviewer Agent  
**Overall Verdict**: **CLEAN**

---

## 1. Executive Summary & Verdict Rationale

A follow-up adversarial re-evaluation was conducted on the patched implementation plan for `webui-redesign` ([plan.md](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/project_plans/webui-redesign/implementation/plan.md)).

All **6 previously identified blocker issues**—as well as all secondary major and minor architectural findings—have been completely and rigorously remediated in `plan.md`. The implementation plan now meets all security, build system, concurrency, data integrity, and performance standards outlined in [requirements.md](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/project_plans/webui-redesign/requirements.md), architectural research files, and AGENTS conventions.

The overall status is updated from **BLOCKED** to **CLEAN**. The project is approved to proceed to Phase 5 (Implementation).

---

## 2. Detailed Verification Matrix of 6 Previously Identified Blockers

| # | Blocker Issue | Previous Status | Patched Plan Remediation | Current Verdict |
|---|---|---|---|---|
| **1** | **API Key Preservation on PUT config** | **BLOCKER** | **Task 6.1.2** & **Task 6.2.1** specify checking incoming API key fields for masked patterns (`sk-...****`) and automatically retaining the existing unmasked API key from active config state in `runtime-overrides.toml` and memory. Form controls preserve masked placeholders intact without clearing fields. | **RESOLVED** |
| **2** | **`build.rs` `cargo:rerun-if-changed` & `cargo-dist` build steps** | **BLOCKER** | **Task 7.1.1** adds `cargo:rerun-if-changed=ui/dist/consolette/browser` and `cargo:rerun-if-changed=ui/src` to `build.rs`. **Task 7.1.2** & **Task 7.1.3** configure `actions/setup-node@v4` with Node 20 LTS and `npm ci && npm run build` in `.github/workflows/ci.yml`, `dist-workspace.toml`, and release workflows prior to `cargo-dist` compilation. | **RESOLVED** |
| **3** | **Strict SSRF `url::Url` IP validation & CORS middleware** | **BLOCKER** | **Task 1.1.3** registers `tower_http::cors::CorsLayer` on the Axum router. **Task 6.1.2** parses `base_url` as `url::Url` and rejects loopback (`127.0.0.0/8`, `::1`), link-local (`169.254.0.0/16`, `fe80::/10`), private CIDRs (`10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`), local hostnames (`localhost`, `*.internal`), and non-HTTPS schemes. | **RESOLVED** |
| **4** | **Mutex synchronization for config mutations** | **BLOCKER** | **Task 6.1.1** wraps disk persistence (`runtime-overrides.toml`) and `ArcSwap::store()` router updates inside an async `tokio::sync::Mutex` lock to guarantee synchronized memory/disk updates and eliminate concurrent write race conditions. | **RESOLVED** |
| **5** | **Sliding window caps (max 60 points) on Chart.js datasets** | **BLOCKER** | **Task 3.3.1** & **Task 5.2.1** mandate sliding window array caps (`if (chart.data.labels.length > 60) { chart.data.labels.shift(); chart.data.datasets.forEach(d => d.data.shift()); }`) on dataset updates. **Task 5.2.1** explicitly registers `ngOnDestroy` chart destruction (`chartInstance.destroy()`). | **RESOLVED** |
| **6** | **Cargo dependencies & `--web-ui=legacy|angular` flag parsing** | **BLOCKER** | **Task 1.1.1** explicitly declares `rust-embed = "8.5"`, `arc-swap = "1.7"`, `mime_guess = "2.0"`, `tokio-stream = "0.1"`, and `tower-http = { version = "0.6", features = ["cors"] }`. **Task 1.1.4** specifies CLI flag parsing for `--web-ui=legacy|angular` in `src/config/mod.rs`, `src/bin/consolette.rs`, and conditional route binding in `src/entrypoint/mod.rs`. | **RESOLVED** |

---

## 3. Verification of Secondary & Architectural Remediations

In addition to the 6 primary blockers, all secondary findings identified in the previous review have been verified as fully resolved in [plan.md](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/project_plans/webui-redesign/implementation/plan.md):

1. **Asset Query Parameter Stripping (Task 1.1.2)**: `serve_embedded_asset` strips query parameters (e.g. `/dashboard/main.js?v=1.0`) using `uri.path()` before querying `DashboardAssets::get()`, preventing asset lookup 404 failures.
2. **SSE Stream Lag Recovery (Task 1.2.3)**: `GET /v1/dashboard/events` catches `RecvError::Lagged(skipped)` and emits a synthetic warning SSE event (`Event::default().event("system_lag").data(...)`) rather than dropping the connection or panicking.
3. **SSE Client Reconnection Backoff (Task 2.2.1)**: `SseService` implements exponential backoff reconnection retry logic (starting at 1s up to a maximum of 30s with a maximum retry counter) to prevent endless tight reconnection loops.
4. **Signal Store Prepending & Array Capping (Task 2.3.1)**: `MetricsStore` prepends incoming trace events (`[newTrace, ...traces]`) and calls `.slice(0, 100)`, ensuring the latest 100 traces are retained in correct chronological order.
5. **Chart Lifecycle Memory Leak Teardown (Task 3.3.2 & Task 5.2.1)**: All Chart.js components explicitly implement `ngOnDestroy` calling `chartInstance.destroy()`, eliminating canvas renderer memory leaks during tab navigation.

---

## 4. Final Review Verdict & Readiness Confirmation

**Overall Verdict**: **CLEAN**  
**Status**: **Ready for Implementation**

All 6 blocker issues and secondary architectural concerns have been satisfactorily addressed. The implementation plan `project_plans/webui-redesign/implementation/plan.md` is approved for engineering execution via `sdd/5-implement` or subagent-driven development workflows.
