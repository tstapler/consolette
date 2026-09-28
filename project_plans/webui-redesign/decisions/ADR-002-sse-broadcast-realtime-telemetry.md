# ADR-002: Server-Sent Events (SSE) via `tokio::sync::broadcast` for Real-Time Telemetry Streaming

**Status**: Accepted  
**Date**: 2026-09-27  
**Project**: webui-redesign  

## Context

Consolette's legacy dashboard relied on fixed 30s/60s REST polling to fetch proxy metrics. To support live RPM gauges, real-time token compression metrics, continuous request trace logging, and instant provider health state transitions, the `webui-redesign` requires sub-50ms real-time telemetry streaming from the Axum backend to browser clients.

High request volumes and potential network drops require a streaming architecture that avoids thread saturation, handles broadcast backpressure cleanly, and prevents zombie tasks or dropped connections across firewalls.

## Decision

We choose Server-Sent Events (SSE) exposed at `GET /v1/dashboard/events` using `axum::response::sse::{Sse, Event}` backed by a central `tokio::sync::broadcast::channel<DashboardEvent>(1024)` in `EntrypointState`.

The implementation will:
1. Define a strongly-typed `DashboardEvent` enum (`MetricsTick`, `RequestTrace`, `ErrorLogged`, `ConfigChanged`).
2. Enforce a 15-second keep-alive ping interval (`KeepAlive::new().interval(Duration::from_secs(15)).text("ping")`) to prevent socket drops over quiet connections.
3. Catch `RecvError::Lagged(skipped_count)` in the stream transformer to emit a non-fatal `system_lag` event rather than dropping the client connection or panicking.
4. Aggregate high-frequency metrics into 250ms ticker snapshots (`MetricsTick`) to decouple raw request volume from SSE event frame generation.

## Alternatives Considered

- **REST Polling (Fixed 30s or High-Frequency 1s)**: Rejected due to significant HTTP header overhead, latency, and unnecessary endpoint execution under high request volume.
- **WebSockets (`axum::extract::ws`)**: Rejected because telemetry flow is 100% server-to-client unidirectional. WebSockets add unnecessary stateful handshake complexity, firewall/proxy traversal issues, custom frame parsing, and manual heartbeat management compared to native HTTP SSE.

## Rationale

SSE operates natively over HTTP/1.1 and HTTP/2, integrates directly with standard browser `EventSource` APIs and RxJS observables, automatically handles connection retries, and imposes minimal network overhead. Combining SSE with a 1024-capacity `tokio::sync::broadcast` channel enables efficient multi-subscriber fan-out to concurrent admin tabs without duplicating metrics collection work.

## Consequences

**Positive:**
- Sub-50ms telemetry update latency with low CPU and network overhead.
- Native browser reconnect handling (`EventSource`) without custom protocol framing.
- 15s keep-alive pings prevent silent connection drops across reverse proxies and firewalls.
- Non-fatal handling of `RecvError::Lagged` protects streams during temporary client backpressure.

**Negative / Risks:**
- Unbounded streaming tasks could persist if socket drops go undetected (mitigated by 15s keep-alive pings).
- High request rates (>5,000 req/s) could overwhelm broadcast channels if unthrottled (mitigated by 250ms metrics tick aggregation).

**Follow-up work:**
- Implement `DashboardEvent` enum and Axum SSE handler in `src/entrypoint/events.rs`.
- Wire `MetricsCollector` to push events to the broadcast channel.

## Related

- Requirements: `project_plans/webui-redesign/requirements.md`
- Research: `project_plans/webui-redesign/research/architecture.md`
- Research: `project_plans/webui-redesign/research/pitfalls.md`
