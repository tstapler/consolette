# ADR-001: Rust-Embed & Custom Axum 0.8 SPA Asset Bundling

**Status**: Accepted  
**Date**: 2026-09-27  
**Project**: webui-redesign  

## Context

Consolette's existing web UI (`src/dashboard.rs`) serves a single static HTML/JS string (`DASHBOARD_HTML`) reliant on external CDN dependencies (`cdn.jsdelivr.net`). The `webui-redesign` initiative replaces this legacy dashboard with an embedded, dynamic Angular 19 Single Page Application (SPA).

A core architectural principle of Consolette is a single, zero-dependency, self-contained binary distribution—requiring all frontend static assets (JS, CSS, HTML, fonts, and chart libraries) to be compiled into the binary without requiring Node.js or separate static asset directories on disk at runtime. Furthermore, client-side HTML5 `pushState` routing (`/dashboard/overview`, `/dashboard/sessions`, `/dashboard/benchmarks`, `/dashboard/config`) requires server-side fallback logic to serve `index.html` for client-handled paths while returning standard HTTP 404s for missing static files.

## Decision

We will compile the Angular 19 `dist/` production assets directly into binary `.rodata` using the `rust-embed` crate (`#[derive(RustEmbed)]`) and serve them via a custom Axum 0.8 router handler (`/dashboard/{*path}`).

The custom fallback handler will:
1. Attempt exact path lookups in embedded assets (`WebAssets::get(path)`).
2. Infer MIME types using `mime_guess` and set HTTP headers.
3. Enforce caching policies (`Cache-Control: no-cache` for `index.html`, `Cache-Control: public, max-age=31536000, immutable` for hashed static assets, and SHA-256 ETags).
4. Perform HTML5 SPA fallback to `index.html` for any request path without a file extension (e.g. `/dashboard/sessions`), enabling client-side Angular Router navigation.

## Alternatives Considered

- **`tower-http` (`ServeDir` / `ServeFile`)**: Rejected because `ServeDir` reads static assets from the physical host filesystem at runtime. This violates the single-binary zero-external-dependency requirement—if the binary is relocated or run in a minimal container without the assets folder present on disk, static requests fail with 404/500 errors.
- **`include_dir`**: Rejected due to the absence of web-centric metadata helpers (MIME type resolution, SHA-256 hash generation for ETags), requiring significant custom header construction boilerplate.
- **`include_str!` / `include_bytes!`**: Rejected as unmaintainable for multi-file hashed Angular production builds containing dozens of static chunks and assets.

## Rationale

`rust-embed` bakes target directories into read-only executable memory (`&'static [u8]`) at compile time, providing zero runtime allocation overhead while exposing SHA-256 file metadata. Integrating `rust-embed` with a custom Axum 0.8 wildcard route handler allows seamless client-side SPA navigation, strict cache management, and air-gapped execution.

## Consequences

**Positive:**
- 100% self-contained single-binary delivery with zero runtime Node.js or disk directory requirements.
- Air-gapped offline operation with zero CDN dependencies.
- Native HTML5 `pushState` SPA route navigation and HTTP cache control headers.

**Negative / Risks:**
- Increases compiled Rust binary footprint by the size of the production Angular build (~1-2MB).
- Developers and CI pipelines must compile Angular assets prior to running `cargo build` (mitigated by a `build.rs` fallback stub generator that creates a placeholder `index.html` if dist is absent).

**Follow-up work:**
- Create `build.rs` fallback stub generator in the crate root to prevent `cargo build` failures when Node.js is unavailable.
- Register `rust-embed` handler in Axum 0.8 entrypoint router.

## Related

- Requirements: `project_plans/webui-redesign/requirements.md`
- Research: `project_plans/webui-redesign/research/stack.md`
- Research: `project_plans/webui-redesign/research/pitfalls.md`
