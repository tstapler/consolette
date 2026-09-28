# Technical Pitfalls, Performance Traps & Security Risks: WebUI Redesign

**Date**: 2026-09-27  
**Target Architecture**: Axum 0.8 (Rust) + Angular 18 (TypeScript) embedded SPA  
**Scope**: Consolette proxy server (`/home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette`)  

---

## 1. Axum 0.8 Integration, Stream Leaks & High-Throughput Performance

### 1.1 Axum 0.8 Breaking Changes & Static Asset Delivery
- **Router Merging & State Discipline**: Axum 0.8 enforces strict type checks on shared state across sub-routers. In Axum 0.8, combining routers via `.nest()` or `.merge()` requires matching state types (`EntrypointState`). Using `axum::extract::State` without consistent scoping across static asset endpoints and SSE streams will result in compilation failures.
- **Embedded Static Assets (`rust-embed` vs `tower-http`)**: 
  - Serving compiled Angular `dist/` assets embedded via `rust-embed` (or `include_dir!`) requires a fallback handler for HTML5 `pushState` routing (`/dashboard/overview`, `/dashboard/sessions`, `/dashboard/benchmarks`, `/dashboard/config`).
  - Unlike `tower-http::services::ServeDir` which operates on physical disk directories, `rust-embed` requires custom Axum handler wrappers to inspect requested paths, resolve MIME types via `mime_guess`, set headers (`Cache-Control`, `Content-Type`), and serve `index.html` on 404s for client-side router navigation.
  - Failure to set proper caching headers (`Cache-Control: no-cache` for `index.html` and `Cache-Control: public, max-age=31536000, immutable` for hashed JS/CSS chunks) will lead to stale UI bundles being served across updates.

### 1.2 SSE Connection Leaks & TCP Keep-Alive
- **Unbounded Async Task Retention**: SSE endpoints (`GET /v1/dashboard/events`) return long-lived `Sse<impl Stream<Item = Result<Event, Infallible>>>` responses. If a browser tab is closed or a network drop occurs, TCP connection termination may not be immediately detected by Tokio if the task is blocked waiting on an idle channel receiver.
- **Missing Keep-Alive Pings**: Without active SSE keep-alive comments (`: keep-alive\n\n`), intermediate reverse proxies, firewalls, or OS socket state machines may drop quiet connections after 60-120 seconds, or retain zombie server tasks indefinitely.
- **Mitigation**: Configure `axum::response::sse::Sse::keep_alive()` explicitly:
  ```rust
  Sse::new(event_stream)
      .keep_alive(
          axum::response::sse::KeepAlive::new()
              .interval(Duration::from_secs(15))
              .text("keep-alive")
      )
  ```

### 1.3 Broadcast Channel Lag (`tokio::sync::broadcast::error::RecvError::Lagged`)
- **Buffer Overflow on Slow Clients**: Real-time event streaming utilizes `tokio::sync::broadcast::channel::<DashboardEvent>(capacity)`. When high-throughput proxy traffic generates events faster than a slow SSE subscriber (or degraded client connection) can consume them, the broadcast channel buffer overflows.
- **Uncaught `RecvError::Lagged`**: Standard stream adapters (such as `tokio_stream::wrappers::BroadcastStream`) yield `Err(BroadcastStreamRecvError::Lagged(n))` when messages are skipped. Calling `.unwrap()` or failing to handle `RecvError::Lagged` in the Axum stream handler will terminate the SSE stream with an unhandled panic or silent disconnect.
- **Mitigation**:
  1. Catch `RecvError::Lagged(skipped_count)` in the stream transformer and emit a non-fatal telemetry event (`event: system_lag`, data: `{"skipped": n}`).
  2. Implement per-client bounded `tokio::sync::mpsc::channel` queues fed by a central broadcast forwarder that drops oldest non-critical metrics events when backpressure builds up.

### 1.4 Memory Consumption Under High Request Rates
- **Per-Request Event Allocation Overhead**: Allocating and serializing `DashboardEvent` JSON objects for every single proxied LLM request under heavy loads (e.g. >5,000 req/sec) causes high GC/allocator churn in the Rust runtime and saturates network interfaces serving multiple open SSE tabs.
- **Mitigation**: 
  - Decouple request execution from event broadcasting using a 250ms–500ms ticker task that aggregates metrics (RPM, average TTFT, error rates, token counts) before broadcasting summary snapshots to clients.
  - Maintain raw individual request details in fixed-capacity in-memory ring buffers (`VecDeque` / `dashmap`) for session replay queries rather than pushing entire request payloads down the SSE pipe.

---

## 2. Angular Client-Side Memory Leaks & Data Buffer Management

### 2.1 Unclosed `EventSource` Connections
- **Service & Component Lifetime Misalignment**: Creating an `EventSource('/v1/dashboard/events')` inside Angular components or root singleton services without explicit teardown logic leaves TCP sockets open when navigating across routes.
- **Duplicate Connection Inflation**: Re-initializing subscriptions on route change without closing existing `EventSource` instances causes connection pooling exhaustion in browsers (HTTP/1.1 enforces a limit of 6 connection sockets per domain).
- **Mitigation**:
  - Encapsulate SSE management in a dedicated RxJS/Signal service using `DestroyRef` or `takeUntilDestroyed`:
    ```typescript
    @Injectable({ providedIn: 'root' })
    export class SseService {
      private destroyRef = inject(DestroyRef);
      
      connect(): Observable<DashboardEvent> {
        return new Observable<DashboardEvent>(observer => {
          const es = new EventSource('/v1/dashboard/events');
          es.onmessage = ev => observer.next(JSON.parse(ev.data));
          es.onerror = err => observer.error(err);
          return () => es.close(); // Guarantees socket closure on unsubscription
        });
      }
    }
    ```

### 2.2 Unbounded Time-Series Data Buffers
- **Memory Accumulation in Signals & RxJS State**: Continuously appending incoming time-series metrics (RPM, latency histogram points, loop lag) into Angular Signals or RxJS `BehaviorSubject` arrays without size limits leads to unbounded memory growth.
- **Browser Tab Crashing**: Operating a dashboard tab continuously over several days can accumulate millions of array elements, consuming multiple gigabytes of heap memory and triggering browser tab crashes or severe garbage collection pauses.
- **Mitigation**: Enforce fixed rolling window capacity for all time-series arrays:
  ```typescript
  // Slice to keep only the last N data points (e.g., 60 minutes of 5s ticks)
  this.metricsHistory.update(list => [...list, newMetric].slice(-720));
  ```

### 2.3 Chart.js & DOM Element Retainers
- **Orphaned Canvas Chart Instances**: Initializing Chart.js instances inside Angular component `ngOnInit` / `afterNextRender` without destroying them in `ngOnDestroy` retains references to DOM canvas nodes, event listeners, and animation loop callbacks.
- **Mitigation**: Store active `Chart` instances and invoke `.destroy()` explicitly during component teardown:
  ```typescript
  ngOnDestroy() {
    this.chartInstance?.destroy();
  }
  ```

---

## 3. Build Pipeline, Headless CI & Asset Synchronization

### 3.1 Headless CI Build Failures (Missing Node.js/npm)
- **Rust Compiler Dependency on Node.js**: If `build.rs` or `cargo build` unconditionally triggers `npm run build` or `ng build`, CI pipelines lacking Node.js/npm (such as standard `dtolnay/rust-toolchain` GitHub Actions jobs or `cargo-dist` cross-compilation containers) will crash during compilation.
- **Missing Asset Directory Compilation Errors**: Using `rust_embed!` or `include_dir!` targeting `src/webui/dist` will result in hard Rust compilation failures if the `dist` directory does not exist prior to invoking `cargo build`.
- **Mitigation**:
  1. Commit a minimal fallback static `index.html` asset bundle (or stub `dist` directory structure) in git.
  2. Implement conditional execution in `build.rs` that checks for `node` availability and only executes `npm build` if the source TS files are modified and Node environment is present.
  3. Update `.github/workflows/ci.yml` to include `actions/setup-node@v4` prior to `cargo test` / `cargo build` steps.

### 3.2 Asset Drift & Version Mismatch
- **Stale Embedded Assets**: Developers modifying TypeScript files in `src/webui` without rebuilding Angular before running `cargo build` will generate binary artifacts containing stale frontend logic.
- **Release Verification Failure**: Discrepancies between the compiled TS assets and the embedded Rust binary can bypass CI test suites if unit tests only exercise Rust API endpoints without asserting asset checksums or embedding integrity.
- **Mitigation**:
  - Add a pre-commit check (`lefthook.yml`) or CI step (`npm run build && git diff --exit-code src/webui/dist`) to enforce that committed frontend build artifacts match TypeScript sources.
  - Expose build metadata (`git commit hash`, `build timestamp`, `UI version`) via `GET /v1/dashboard/version` to detect runtime drift in deployed environments.

---

## 4. Security Risks in Dynamic Proxy Config Editing

### 4.1 Unauthorized Configuration Mutations & Bind Scope
- **Unauthenticated Control Plane**: Routes like `PUT /api/route`, `POST /v1/dashboard/config`, and `POST /session/policy` allow mutating core LLM routing strategies, provider weights, fallback chains, and rate limit thresholds.
- **Over-Exposed Network Binding**: If Consolette is launched with `--host 0.0.0.0` or deployed on a shared network without authentication, unauthorized network actors can reconfigure routing targets, disable rate limiters, or redirect traffic.
- **Mitigation**:
  - Enforce loopback-only binding (`127.0.0.1`) by default for admin and management endpoints.
  - Require an administrative bearer token header (`X-Consolette-Auth` or `Authorization: Bearer <token>`) for all state-modifying config APIs if binding to non-loopback interfaces.

### 4.2 Cross-Site Request Forgery (CSRF) & CORS Exploitation
- **Cross-Origin Configuration Hijacking**: If an admin accesses a malicious web page in a browser while Consolette is running at `http://localhost:8080`, malicious scripts can execute cross-origin `POST` or `PUT` requests to `/api/route` unless CORS and CSRF protections are enforced.
- **Mitigation**:
  - Apply strict CORS middleware (`tower_http::cors::CorsLayer`) allowing only trusted origin domains.
  - Enforce custom header requirements (`X-Requested-With: Fetch` or `Content-Type: application/json`) on all state-changing POST/PUT handlers to trigger browser CORS preflight checks and block simple HTML form CSRF submissions.

### 4.3 Input Validation Gaps, Arbitrary `base_url` & SSRF Risks
- **Server-Side Request Forgery (SSRF)**: Allowing dynamic updates to provider `base_url` fields via API endpoints enables attackers to redirect outbound LLM proxy traffic to internal network endpoints (`http://169.254.169.254/latest/meta-data/` or internal admin interfaces). Outbound requests will leak authorization headers (`x-api-key`, `Authorization`) to arbitrary third-party endpoints.
- **Plain-Text Key Leakage**: Returning raw provider configuration objects via `GET /api/route` or `GET /v1/dashboard/config` risks leaking sensitive upstream API keys (Anthropic, Bedrock, OpenAI) in API JSON responses and browser console logs.
- **Mitigation**:
  1. Validate provider `base_url` fields against an allowlist of HTTPS schemes and hostname domain patterns, explicitly rejecting private IPv4/IPv6 address ranges (`127.0.0.0/8`, `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`, `169.254.0.0/16`).
  2. Mask API keys in all JSON response serialization schemas (e.g. `"api_key": "sk-ant-...****"`).

### 4.4 Race Conditions & Concurrency Risks During Hot-Swaps
- **ArcSwap State Inconsistency**: Hot-swapping the `DispatchRouter` via `ArcSwap::store()` while requests are executing in parallel requires ensuring atomic updates across `SessionOverrideStore`, `CapabilityCache`, and rate-limiting states.
- **Panic Hazards in Override Validation**: Passing invalid TOML/JSON payloads to `RuntimeOverrides::apply()` that trip `unwrap()` or `expect()` calls in non-test production code violates project safety invariants and can crash the main server process.
- **Mitigation**:
  - Enforce zero `unwrap()` / `expect()` in configuration update paths; return clean `400 Bad Request` JSON error responses on validation failures.
  - Validate candidate routes using `validate_references()` and `validate_model_selectors()` before committing updates to disk or hot-swapping the live router instance.
