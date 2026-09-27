# Research Findings: Dynamic Angular SPA Stack & Rust Embedding (Axum 0.8)

**Date**: 2026-09-27  
**Project**: `consolette` Web UI Redesign  
**Document**: `project_plans/webui-redesign/research/stack.md`  

---

## 1. Executive Summary & Context

The goal of the `webui-redesign` initiative is to replace `consolette`'s legacy inline HTML dashboard (`src/dashboard.rs`) with a modern, reactive, multi-page Angular 19+ Single Page Application (SPA). To maintain `consolette`'s core architectural principle—a single, zero-dependency binary distribution—the compiled frontend static assets must be embedded directly inside the Rust executable and served via Axum 0.8.

This document presents a technical evaluation across three primary dimensions:
1. **Rust Asset Embedding & Axum 0.8 Integration**: Comparing `rust-embed`, `tower-http` (`ServeDir`/`ServeFile`), `include_dir`, and `include_str!`.
2. **Frontend Framework & Reactive Tech Stack**: Evaluating Angular 19+ standalone components, Signals, RxJS/SSE streaming, charting solutions (Chart.js vs Apache ECharts vs ngx-charts), and Tailwind CSS v4.
3. **Build & CI Integration**: Designing build pipelines (`build.rs`, Makefile, npm, CI) that respect the single-crate constraint while preventing developer friction.

---

## 2. Asset Embedding Options in Rust (Axum 0.8 Context)

### 2.1 Crate Evaluation Matrix

| Criterion | `rust-embed` | `tower-http` (`ServeDir`) | `include_dir` | `include_str!` / `include_bytes!` |
| :--- | :--- | :--- | :--- | :--- |
| **Embedding Mechanism** | Compile-time macro `#[derive(RustEmbed)]` | Runtime filesystem read | Compile-time `include_dir!` macro | Direct literal inclusion in source |
| **Single Binary Delivery** | ✅ Yes (assets baked into `.rodata`) | ❌ No (requires external `dist/` folder on disk at runtime) | ✅ Yes (baked into `.rodata`) | ✅ Yes |
| **Multi-File SPA Support** | ✅ Excellent (`folder = "..."`) | ✅ Excellent | ✅ Good | ❌ Unusable for multi-bundle Angular builds |
| **SPA Fallback Handling** | ✅ Easy via custom Axum handler | ✅ Native via `.fallback()` | ⚠️ Manual lookup required | ❌ N/A |
| **MIME / ETag / Cache Support** | ✅ Built-in hash metadata + `mime_guess` | ✅ Native header handling | ⚠️ Manual header construction | ❌ Manual |
| **Binary Footprint** | ~Zero runtime overhead (`&'static [u8]`) | Zero compile footprint | ~Zero runtime overhead | Direct string allocation |

### 2.2 Deep-Dive & Recommendation

1. **`tower-http` (`ServeDir` / `ServeFile`)**:
   - *Pros*: Out-of-the-box support for HTTP range requests, ETag matching, and SPA fallbacks (`ServeDir::new("dist").fallback(ServeFile::new("dist/index.html"))`).
   - *Cons*: **Violates single-binary constraint.** `ServeDir` attempts to open files from the host operating system's filesystem at runtime. If the binary is moved or executed in a container without the web asset directory, static asset requests fail with 404/500 errors.
2. **`include_dir`**:
   - *Pros*: Clean macro-based filesystem representation.
   - *Cons*: Lacks web-centric metadata helpers (hashes, content-type mapping). Requires writing boilerplate code for HTTP response generation and SPA fallbacks.
3. **`rust-embed` (Recommended)**:
   - *Pros*: Purpose-built for web asset embedding in Rust web servers. Supports folder macro compilation, file metadata (SHA-256 hashes for ETags), and slice references (`Cow<'static, [u8]>`).
   - *Integration with Axum 0.8*: Works seamlessly with Axum 0.8 route handlers. A custom SPA fallback handler returns the exact embedded file if present, or serves `index.html` for unknown subroutes (`/dashboard/sessions`, `/dashboard/benchmarks`, `/dashboard/config`).

### 2.3 Axum 0.8 SPA Fallback Handler Pattern

Axum 0.8 updated route matching syntax (using `{path}` or `{*path}` pattern matchers). Below is the recommended implementation pattern for embedding the Angular `dist/` output using `rust-embed`:

```rust
use axum::{
    body::Body,
    http::{header, HeaderValue, StatusCode, Uri},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "ui/dist/consolette/browser"]
struct WebAssets;

pub fn router() -> Router {
    Router::new()
        // Serve static assets and SPA routes under /dashboard
        .route("/dashboard", get(serve_spa_index))
        .route("/dashboard/", get(serve_spa_index))
        .route("/dashboard/{*path}", get(serve_embedded_asset))
}

async fn serve_spa_index() -> impl IntoResponse {
    serve_asset_or_fallback("index.html")
}

async fn serve_embedded_asset(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches("/dashboard/");
    serve_asset_or_fallback(path)
}

fn serve_asset_or_fallback(path: &str) -> Response {
    // 1. Try exact path match
    if let Some(content) = WebAssets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        let mut response = Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime.as_ref());

        // Immutable caching for hashed Angular bundles, no-cache for index.html
        if path == "index.html" {
            response = response.header(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate");
        } else {
            response = response.header(header::CACHE_CONTROL, "public, max-age=31536000, immutable");
        }

        if let Ok(etag) = HeaderValue::from_str(&format!("\"{}\"", hex::encode(content.metadata.sha256_hash()))) {
            response = response.header(header::ETAG, etag);
        }

        return response.body(Body::from(content.data)).unwrap_or_else(|_| internal_error());
    }

    // 2. SPA Fallback: If path has no extension (e.g. /dashboard/sessions), serve index.html
    if !path.contains('.') {
        if let Some(index) = WebAssets::get("index.html") {
            return Response::builder()
                .status(StatusCode::OK)
                .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
                .header(header::CACHE_CONTROL, "no-cache, no-store, must-revalidate")
                .body(Body::from(index.data))
                .unwrap_or_else(|_| internal_error());
        }
    }

    // 3. Asset not found
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::from("404 Not Found"))
        .unwrap_or_else(|_| internal_error())
}

fn internal_error() -> Response {
    Response::builder()
        .status(StatusCode::INTERNAL_SERVER_ERROR)
        .body(Body::from("500 Internal Server Error"))
        .unwrap_or_default()
}
```

---

## 3. Angular 19+ Frontend Tech Stack

### 3.1 Framework Architecture & Modern Patterns
- **Standalone Components**: Angular 19 standardizes standalone components (`standalone: true` default). `NgModules` are eliminated. Applications are bootstrapped via `bootstrapApplication(AppComponent, appConfig)`.
- **Router & HTTP Configuration**: Configured cleanly in `app.config.ts`:
  ```typescript
  export const appConfig: ApplicationConfig = {
    providers: [
      provideRouter(routes, withComponentInputBinding()),
      provideHttpClient(withFetch()),
    ]
  };
  ```

### 3.2 Reactive State Management with Signals
Angular 19 Signals replace verbose RxJS store boilerplate for UI component state:
- `signal<T>()`: Holds state for real-time RPM, active providers, and session filters.
- `computed()`: Automatically derives metrics (e.g., overall token compression percentage, average TTFT across upstreams).
- `effect()`: Synchronizes state with persistent local storage settings (e.g., dark mode toggle, auto-scroll preferences).

```typescript
@Component({
  selector: 'app-metrics-overview',
  standalone: true,
  template: `
    <div class="stat-card">
      <h3>Compression Ratio</h3>
      <p class="value">{{ compressionRatio() }}%</p>
    </div>
  `
})
export class MetricsOverviewComponent {
  readonly tokensBefore = signal(1250000);
  readonly tokensAfter = signal(350000);

  readonly compressionRatio = computed(() => {
    const before = this.tokensBefore();
    if (!before) return 0;
    return (((before - this.tokensAfter()) / before) * 100).toFixed(1);
  });
}
```

### 3.3 RxJS / SSE Real-time Event Streaming
Axum 0.8 will expose an SSE stream at `/v1/dashboard/events`. To handle high-frequency events without memory degradation:
1. **RxJS `Observable` Wrapper**: Wraps native `EventSource` with automatic retry/reconnect strategies.
2. **Buffer Capping**: Caps live event logs in Signals (`events.update(list => [newEvent, ...list].slice(0, 500))`) to ensure zero browser memory leaks over multi-day proxy runs.

```typescript
@Injectable({ providedIn: 'root' })
export class EventStreamService {
  private readonly eventsSignal = signal<DashboardEvent[]>([]);
  readonly events = this.eventsSignal.asReadonly();

  connect(url: string): Observable<DashboardEvent> {
    return new Observable<DashboardEvent>(observer => {
      const eventSource = new EventSource(url);
      eventSource.onmessage = (event) => {
        const parsed: DashboardEvent = JSON.parse(event.data);
        observer.next(parsed);
        this.eventsSignal.update(current => [parsed, ...current].slice(0, 500));
      };
      eventSource.onerror = (err) => observer.error(err);
      return () => eventSource.close();
    }).pipe(
      retry({ delay: 3000 }) // Auto-reconnect after 3s on SSE disconnect
    );
  }
}
```

### 3.4 Charting Library Comparison Matrix

| Library | Rendering Engine | Angular 19 Compatibility | Real-Time Streaming Performance | Bundle Size (Gzipped) | Dark Mode Styling | Recommendation |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **Chart.js v4** (`ng2-charts` / canvas) | HTML5 Canvas | ✅ Excellent | ✅ High (smooth canvas redraws, 60 FPS) | ~60 KB | ✅ Simple option overrides | **Recommended for Metrics & Lag** |
| **Apache ECharts** (`ngx-echarts`) | Canvas & SVG | ✅ Excellent | ✅ Moderate-High (supports complex series) | ~350 KB | ✅ Built-in dark themes | **Recommended for Benchmarks & Heatmaps** |
| **ngx-charts** | SVG (D3 Primitives) | ⚠️ Mixed | ❌ Low (high DOM node recreation on fast updates) | ~180 KB | ⚠️ Requires SVG custom CSS | ❌ Not Recommended |

**Decision**: Use **Chart.js** for high-frequency time-series (RPM, Event Loop Lag, Duration distribution) to maximize rendering performance and minimize binary bundle size. Use **Apache ECharts** if scatter-plot model benchmarking or heatmaps are introduced.

### 3.5 Styling Framework: Tailwind CSS v4
Tailwind CSS v4 simplifies configuration by adopting CSS-first imports in `src/styles.css`:
```css
@import "tailwindcss";

@layer base {
  body {
    @apply bg-neutral-950 text-neutral-100 font-sans antialiased;
  }
}
```
Eliminates `tailwind.config.js` and provides instant build compilation via Lightning CSS integrated into Angular CLI.

---

## 4. Build & CI Integration Strategy

### 4.1 Development Workflow vs Production Build
- **Local Development**:
  - Developers run `ng serve` in `ui/` (starts Angular dev server at `http://localhost:4200`).
  - `ui/proxy.conf.json` proxies API requests (`/v1/*`, `/metrics`, `/errors/*`) to the backend Axum process running on `http://localhost:8080`.
- **Production Build**:
  - `ng build --configuration production` compiles Angular assets to `ui/dist/consolette/browser/`.
  - `rust-embed` bakes these assets into the `consolette` binary.

### 4.2 Cargo `build.rs` & Fallback Strategy
To prevent `cargo build` failures when Node.js is not installed (e.g. minimal CI environments, standard Rust crates audit):
1. **Fallback Dummy Asset Generator**: `build.rs` checks if `ui/dist/consolette/browser/index.html` exists.
2. If missing and Node.js is unavailable, `build.rs` emits a minimal static fallback `index.html` into `ui/dist/consolette/browser/` so macro compilation succeeds without error.
3. If Node.js and npm are present and `CONSOLETTE_BUILD_WEB=1` is set, `build.rs` triggers `npm run build` automatically.

```rust
// build.rs
use std::path::Path;
use std::fs;

fn main() {
    println!("cargo:rerun-if-changed=ui/src");
    println!("cargo:rerun-if-changed=ui/package.json");

    let dist_dir = Path::new("ui/dist/consolette/browser");
    let index_html = dist_dir.join("index.html");

    if !index_html.exists() {
        println!("cargo:warning=Angular build output not found at {:?}. Generating fallback stub.", index_html);
        fs::create_dir_all(dist_dir).expect("Failed to create fallback asset dir");
        fs::write(
            &index_html,
            "<!DOCTYPE html><html><body><h1>Consolette Dashboard</h1><p>Web UI build output missing. Run 'npm run build' in ui/ directory.</p></body></html>"
        ).expect("Failed to write stub index.html");
    }
}
```

### 4.3 CI & Release Workflow
In `.github/workflows/ci.yml`:
1. **Node setup step**: `actions/setup-node@v4` with Node 20 LTS.
2. **Frontend build step**: `cd ui && npm ci && npm run build`.
3. **Rust compilation step**: `cargo build --release` (embeds full Angular SPA).
4. **Release distribution**: `cargo-dist` packages the single executable containing all static assets, zero external tarball dependencies.

---

## 5. Architectural Recommendations & Next Steps

1. **Asset Embedding**: Adopt `rust-embed` with a custom Axum 0.8 SPA wildcard route handler (`/dashboard/{*path}`) supporting SHA-256 ETags and immutable cache-control headers.
2. **Frontend Stack**: Initialize Angular 19 SPA inside `ui/` using Standalone Components, Signals for local state, RxJS for SSE event streaming with 500-item buffer caps, Tailwind CSS v4, and Chart.js.
3. **Build Pipeline**: Implement `build.rs` fallback stubs alongside Makefile / npm build targets to preserve single-crate developer ergonomics and CI compatibility.
