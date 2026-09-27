# ADR-003: Angular 19 Standalone Components with Signals and Bounded Rolling Window Event Buffers

**Status**: Accepted  
**Date**: 2026-09-27  
**Project**: webui-redesign  

## Context

Continuous streaming of real-time telemetry events (`MetricsTick`, `RequestTrace`, `ErrorLogged`) into client-side JavaScript risks accumulating unbounded memory over time, leading to browser tab freezing, garbage collection pauses, or tab crashes during long-running proxy operations.

Additionally, the frontend requires a clean, modern reactive state architecture without unnecessary boilerplate libraries or legacy component module structures.

## Decision

We will implement the frontend using Angular 19 Standalone Components (`standalone: true`) and Angular Signals (`signal()`, `computed()`, `effect()`) for reactive state management.

To guarantee zero browser memory leaks over multi-day operations:
1. All continuous streaming event arrays in frontend services (`MetricsStore`, `SessionStore`, `BenchmarkStore`) will enforce fixed rolling-window capacity caps.
2. Live request trace streams will be capped at 500 items (`traces.update(list => [newTrace, ...list].slice(0, 500))`).
3. Historical time-series metric buffers will be capped at 720 items (`metricsHistory.update(list => [...list, newMetric].slice(-720))`).
4. RxJS SSE observables will encapsulate connection teardown using `DestroyRef` / `takeUntilDestroyed` to ensure socket closure on component unmount.
5. All charting components (Chart.js canvas instances) will explicitly call `.destroy()` on teardown.

## Alternatives Considered

- **NgModules + RxJS BehaviorSubjects (Legacy Angular)**: Rejected in favor of Angular 19 Standalone Components and Signals, which eliminate module boilerplate and provide fine-grained DOM updates.
- **Unbounded Client-Side State Accumulation**: Rejected because appending events without capacity caps guarantees browser heap memory exhaustion over extended streaming sessions.
- **NgRx / Redux Store**: Rejected as over-engineered for an embedded single-binary dashboard; Angular Signals provide built-in reactivity with zero external state library dependencies.

## Rationale

Angular 19 Signals provide fine-grained reactivity, triggering DOM updates only for visual components that directly depend on changed metrics signals. Bounded array capacity limits (`.slice(-500)` / `.slice(-720)`) cap browser memory consumption to a predictable, small allocation footprint regardless of how long the dashboard tab remains active.

## Consequences

**Positive:**
- Guaranteed zero memory growth and leak prevention over continuous long-term streaming runs.
- 60 FPS UI rendering for real-time charts and KPI cards.
- Clean component architecture using Angular 19 Standalone Components and standalone router configuration.
- 100% offline self-contained delivery (Chart.js compiled directly into Angular output without CDN dependencies).

**Negative / Risks:**
- In-memory event history is limited to the rolling window cap (older historical items must be queried on-demand via backend REST APIs).

**Follow-up work:**
- Implement `SseService` with proper teardown logic.
- Build `MetricsStore`, `SessionStore`, `BenchmarkStore`, and `ConfigStore` services with Signal buffer caps.

## Related

- Requirements: `project_plans/webui-redesign/requirements.md`
- Research: `project_plans/webui-redesign/research/stack.md`
- Research: `project_plans/webui-redesign/research/pitfalls.md`
