# Requirements: webui-redesign

**Date**: 2026-09-27
**Type**: feature addition / web ui redesign
**Complexity**: 3 — system design

## Problem Statement
The current web UI in Consolette is a single-file static HTML/JS string (`DASHBOARD_HTML` in `src/dashboard.rs`) that relies on periodic REST polling (30s/60s intervals). It lacks interactive proxy management, real-time event streaming, session replay capabilities, model benchmarking, and a dynamic modern UI. Proxy operators need a high-performance, real-time, interactive management suite embedded directly within the single Consolette Rust binary.

## Baseline
- `GET /dashboard` serves an inlined raw HTML/CSS/JS string (Chart.js via CDN).
- Data updating depends on fixed JS `setInterval` polling to `/metrics` and `/errors/summary`.
- No interactive controls: operators cannot adjust routing rules, fallback policies, or rate limits via the UI.
- No session replay or detailed payload comparison interface.
- External runtime dependence on CDN scripts (`cdn.jsdelivr.net`) for charting.

## Users / Consumers
- Consolette proxy administrators and developers.
- AI system operators monitoring LLM usage, token compression, provider health, latency, and cost metrics.

## Success Metrics
- **Binary Embedding**: 100% self-contained delivery — single Rust executable containing the compiled Angular SPA frontend without requiring Node.js at runtime.
- **Real-Time Streaming**: Live event streaming via Server-Sent Events (SSE) or WebSockets in Axum, replacing fixed 30s polling.
- **Feature Completeness**: Multi-page Angular dashboard featuring:
  1. Live Metrics & Health Dashboard (real-time RPM, latency, token compression ratio, provider cooldown states).
  2. Interactive Session Replay & Payload Inspector (inspect original vs compressed payloads, diff viewer, step-by-step trace).
  3. Live Model Benchmarking & Upstream Analytics (TTFT, latency percentiles, error rate comparisons across Anthropic, Bedrock, OpenAI upstreams).
  4. Dynamic Proxy Config Editor (interactive UI for viewing and tweaking provider priorities, rate limit thresholds, and fallback rules).
- **Zero CDN Dependencies**: All static assets (JS, CSS, fonts, charts) packaged into the binary.
- **Performance**: Sub-500ms initial page load, <100ms real-time event push latency.

## Appetite
Large (3–4 weeks budget) — Full multi-page Angular dashboard suite with deep analytics, session replay, model benchmarking, and live config editor.

## Constraints
- Single crate / single binary deployment rule (`consolette` binary).
- Must use Axum 0.8 for routing and serving embedded assets.
- Must follow project standards: Rust 1.74+ clippy, zero unwrap/expect in non-test production code, clean single-crate build.
- Rust + Angular web app stack alignment (per `code-new-project` conventions).

## Non-functional Requirements
- **Performance SLO**: UI rendering at 60 FPS for dynamic charts; backend SSE/WS broadcast latency < 50ms.
- **Scalability**: Capable of handling high-throughput request streams without browser memory leaks or Axum thread exhaustion.
- **Security classification**: Internal / local administration tool; secure headers and CORS protection.
- **Offline / Self-contained**: Operates cleanly in air-gapped environments.

## Scope
### In Scope
- Angular SPA setup (Standalone Components, Signals, Router, Tailwind CSS, Chart.js / ngx-charts bundled).
- Rust build integration (`rust-embed` or `include_dir` to bundle Angular `dist/` build output).
- Axum web server integration:
  - Route handler for static embedded assets (`/dashboard/*`, `/assets/*`).
  - Real-time event broadcast stream endpoint (`/v1/dashboard/events` SSE/WebSocket channel).
  - Config management REST/RPC endpoints (`GET/PUT /v1/dashboard/config`).
  - Session replay & payload API endpoints (`GET /v1/dashboard/sessions`, `GET /requests/{id}`).
  - Model benchmarking telemetry API (`GET /v1/dashboard/benchmark`).
- Multi-page UI sections:
  - **Overview**: Real-time stats, provider health badges, token savings counters, lag meters.
  - **Session Replay**: Searchable session log with interactive diff inspector (Original vs Compressed).
  - **Model Benchmarks**: Comparative charts for TTFT, throughput, cost, and latency across providers.
  - **Config Editor**: Visual manager for LLM routing rules, fallback chains, and provider keys.

### Out of Scope
- Multi-tenant cloud SaaS authentication (Kratos/Ory), keeping this strictly for local/binary administration.
- Database migration (persisting metrics in existing SQLite / in-memory store).

## Rabbit Holes
- Direct compilation of Angular during every `cargo build` in CI if Node/npm is not installed. *(Mitigation: build Angular assets to a `dist/` directory, commit compiled web assets or provide a fallback build script so cargo build always succeeds).*
- Axum 0.8 compatibility with static file serving and SSE streaming. *(Mitigation: verify Axum 0.8 `ServeDir` / `rust-embed` integration and `Sse` response types).*

## Alternatives Considered
- **React / Vue / Yew / Leptos**: Evaluated against the standard `code-new-project` stack. Angular was chosen for consistency with team conventions, built-in router, strong TypeScript typing, and Signals state management.
- **WASM frontend (Leptos/Yew)**: Rejected due to larger binary footprint and slower iteration speed compared to standard Angular build embedded into Rust.

## Feasibility Risks
- Memory retention in Angular when subscribing to continuous SSE request streams. (Mitigation: cap rolling event buffer size in client state).
- Axum 0.8 breaking changes in state handling or router merging.

## Observability Requirements
- SSE stream connection counters and disconnect metrics.
- Embedded asset load error logging in Axum tracing subscriber.

## Risk Control
- Feature flag / CLI flag (`--web-ui=legacy|angular` or `/dashboard` route fallback) during initial rollout.
- Graceful degradation to basic metrics if SSE disconnects.

## Open Questions
- What embedding crate fits best with Axum 0.8 (`rust-embed` vs `tower-http` `ServeDir`)?
- Should the Angular build step be driven by a Makefile / Lefthook / cargo build script?
