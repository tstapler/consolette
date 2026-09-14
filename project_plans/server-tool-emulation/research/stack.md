# Research: Stack

**Date**: 2026-09-13 | **Sources**: consolette `Cargo.toml`, `src/` inspection;
stapler-mcp repo (GitHub fetch 2026-09-13 + `crates/core/src/schema.rs`)

## What exists in consolette today

- Single-crate binary, edition 2021. `rmcp = 2.1` with features already
  including **`client`** + `transport-io` + streamable-HTTP client/server.
  A spawned-stdio MCP client needs only `tokio` process spawning (already a
  full dependency) — **no new transport crate required**.
- `tokio = 1` (full), `serde/serde_json`, `reqwest 0.12` (json/stream/
  rustls-tls), `eventsource-stream`, ` governor`, `dashmap`, `moka`,
  `arc-swap`, `backoff`/`tokio-retry`. Everything the loop needs (timeouts,
  retries, caching, concurrency) is on hand.
- Test stack: `cargo test` + `tempfile`; tokio `test-util` for time control.
  No mock-MCP harness exists yet — one will be built from `rmcp` itself
  (a fake `stapler-mcp` server on stdio in tests).
- Clippy pedantic + `unwrap_used`/`expect_used` denied — new code must be
  `Result`-plumbed, no `.unwrap()` outside tests.

## stapler-mcp tooling surface (verified from source)

- Architecture: thin stdio MCP client binary → Unix-socket daemon
  (`~/.stapler-mcp/daemon.sock`, overridable via `STAPLER_MCP_HOME`); daemon
  owns browser pool, HTTP clients, caches. Exactly one daemon machine-wide
  (lockfile-guarded, stale-socket-safe).
- Search tool: **`brave_web_search`** — input `{query: String, count?:
  u32 (default 10, max 20)}` (camelCase on the wire: `query`, `count`);
  output `{results: [{title, url, description}]}`. Stateless HTTP wrapper over
  Brave Search API; `BRAVE_API_KEY` read from **the daemon's environment
  only**; base URL overridable via `BRAVE_API_BASE_URL` (designed for tests).
- Adjacent fetch tools exist (`fetch_page`, `read_website`,
  `download_website`) but are **not** in scope; the emulation needs search
  results only. (A later story could offer URL-fetch enrichment — deferred.)
- Distributions: native binary (`cargo install`) and wasm/Node (zero native
  binary). Consolette needs only the **native binary** as a child process.
- Interop is by `ping`-liveness, never by implementation — consolette should
  likewise treat the child as an opaque MCP server (tool name + JSON shapes
  only).

## Version / compatibility notes

- `rmcp 2.1` client speaking to the stapler-mcp native binary's `rmcp`-based
  stdio server is same-SDK-family — lowest interop risk of any seam.
- stapler-mcp is versioned independently; consolette must tolerate unknown
  extra fields (serde default-tolerant structs) and a missing
  `brave_web_search` tool (degrade, don't crash).
- MSRV: repo pins via `rust-toolchain.toml`; no edition upgrade needed.

## Dependencies needed

- **None new for V1.** `rmcp` client + `tokio::process` cover the recommended
  seam. Optional later: none anticipated.
- Dev-only: a test fake MCP server built on the existing `rmcp` server bits.

## Alternatives considered (detail in `alternatives.md` / `stapler-mcp-seam.md`)

- Shared-crate extraction (`crates/core` as a dependency): rejected — pulls
  schemars/fastembed/browser surface into the proxy, duplicates credential
  handling, couples release trains.
- HTTP seam: rejected — stapler-mcp exposes no HTTP server surface; adding
  one is new attack surface for zero benefit.
