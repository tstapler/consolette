# Research: HTTP entrypoint stack

## Existing dependency versions (Cargo.toml, verified by Read)

All required deps are already present — no new crate is needed for this feature.

| Crate | Version | Features |
|---|---|---|
| `axum` | `0.8` | `macros` |
| `axum-extra` | `0.10` | `typed-header` |
| `tower` | `0.5` | — |
| `tower-http` | `0.7` | `trace`, `timeout` |
| `tokio` | `1` | `full` |
| `tokio-stream` | `0.1` | `net` |
| `tokio-util` | `0.7` | `rt` |
| `reqwest` | `0.12` | `json`, `stream`, `rustls-tls` |
| `eventsource-stream` | `0.2` | — (already a dep, but see note below — not actually needed on the server-emitting side) |
| `bytes` / `futures-core` / `futures-util` | `1` / `0.3` / `0.3` | — |
| `serde_json` | `1` | — |
| `tracing` | `0.1` | (with `tracing-subscriber` `0.3` env-filter) |
| `http` | `1` | — |

`axum` does *not* need an `"sse"` Cargo feature flag — `axum::response::sse` ships in the base crate; the feature-gating that used to exist pre-1.0 axum is gone by 0.8.

## Existing in-repo axum precedent (`src/cost_metrics/server.rs`)

This is the pattern to imitate, not axum's generic docs — it's already the house style:

- `Router::new().route("/v1/cost/{session_key}", get(handler))` — axum 0.8's `{param}` path syntax (not `:param`, which 0.8 removed).
- Bind via `TcpListener::bind(addr).await?` then `axum::serve(listener, router).await?` — the plain 0.8 serve loop, no manual `hyper::Server` wiring.
- Tests bind `"127.0.0.1:0"` (OS-assigned port) for real-socket integration tests, and separately use `tower::ServiceExt::oneshot` against the `Router` directly for route-shape tests that don't need a real socket. Reuse both patterns for the new entrypoint's tests (real end-to-end streaming test needs a real bind; translation-shape tests can use `oneshot`).
- `axum::extract::State` for shared handler state (here: `Router` — the ADR-003 dispatch router, `CostTracker`, config) — construct once, clone-cheap (`Arc`-wrapped) into the `Router` (axum's, not ADR-003's — name collision to watch when writing code, they must not be confused in imports).

## SSE / streaming handler: manual `Body::from_stream`, not `axum::response::sse::Sse`

This is the one place the generic 2026 axum guidance (see Sources) diverges from what this codebase needs, and the divergence matters:

- `axum::response::sse::Sse<S>` expects a `Stream<Item = Result<Event, E>>` — i.e., you build `axum::response::sse::Event` values (`.data(...)`, `.event(...)`, `.id(...)`) and Sse re-serializes them into `data: ...\n\n` wire format itself.
- **`ProviderResponse::Stream` is already raw upstream SSE bytes**, not decoded events. Confirmed by reading `AnthropicProvider::send` (`src/providers/anthropic.rs:576-586`): for the streaming case it calls `self.send_streaming_request(...)` (a `reqwest::Response`) and does `response.bytes_stream().map(|r| r.map_err(anyhow::Error::from))` — i.e. raw, arbitrarily-chunked bytes straight off the wire from Anthropic's `text/event-stream` response, already in `data: {...}\n\n` framing. There is no intermediate `Event` struct at all.
- Feeding that through `axum::sse::Sse` would require parsing the incoming `data: ...` lines back into `Event`s just so axum can re-serialize them into the same bytes — pure waste, and a place to introduce framing bugs (arbitrary TCP chunk boundaries don't align with SSE event boundaries, so naive line-splitting would break multi-chunk events).
- **Correct pattern for this codebase**: pass the `Stream<Item = Result<Bytes, anyhow::Error>>` straight through as the response body via `axum::body::Body::from_stream(stream)`, and set `Content-Type: text/event-stream` (plus `Cache-Control: no-cache`, `Connection: keep-alive` if not already set by the upstream response/tower-http) explicitly on the `Response` builder. This is a byte-passthrough proxy, not an SSE *producer* — axum's `Sse` type is for the latter.
  ```rust
  let stream = provider_stream.map(|r| r.map_err(std::io::Error::other));
  Response::builder()
      .status(StatusCode::OK)
      .header(CONTENT_TYPE, "text/event-stream")
      .header(CACHE_CONTROL, "no-cache")
      .body(Body::from_stream(stream))?
  ```
  Note `Body::from_stream` wants `Stream<Item = Result<Bytes, E>>` where `E: Into<Box<dyn StdError + Send + Sync>>` — `anyhow::Error` doesn't impl `std::error::Error` compatibly with `Box<dyn Error>` conversion directly in all axum versions, so map errors through `std::io::Error::other(e)` (or `anyhow::Error -> Box<dyn Error + Send + Sync>` via `.into()`) at the boundary. Confirm the exact bound needed against the pinned axum 0.8.x point release when implementing (`cargo doc --open -p axum` or docs.rs pinned to the `Cargo.lock` version).
- `eventsource-stream = "0.2"` (already a dep) is for *parsing* an incoming SSE stream into events — likely present for provider-side consumption of upstream SSE (e.g. if any provider needs to inspect/transform events rather than blind-forward, such as usage extraction from a streaming Anthropic response for cost tracking, per the requirements doc's "cost-tracking on partial/failed streams" rabbit hole). It is not needed for the pure proxy-and-forward path but will likely be needed for the mid-stream usage-extraction / partial-cost-tracking logic called out in the requirements doc's Rabbit Holes section — check whether `record_actual_usage_from_anthropic_response` needs a fully-materialized JSON `Value` (it does, per its signature taking `&Value`) or can work incrementally; if incremental, `eventsource_stream::Eventsource` (a `futures_util::Stream` extension trait) is the idiomatic way to fold the raw byte stream into typed SSE events for extracting the final `message_stop`/usage event before re-emitting the same raw bytes downstream (tee: forward raw bytes to the client while separately scanning for the usage payload).

## OpenAI-compatible streaming shape

The requirements doc says translation goes through the existing `translate_openai_to_anthropic` / `translate_anthropic_to_openai` functions (`src/providers/mod.rs`). Those are almost certainly value-to-value (`serde_json::Value -> serde_json::Value`) translators for the *non-streaming* shape (confirmed present as unit-tested functions per `src/providers/mod.rs:296+`, e.g. `translate_and_record`). For **streaming** OpenAI compat, there is no existing per-chunk translator visible in the grep — this is a real gap the plan needs to size: OpenAI's streaming chunk shape (`data: {"object":"chat.completion.chunk",...}`) differs from Anthropic's SSE event types (`message_start`, `content_block_delta`, `message_stop`, etc.), so a per-event translator (not just per-request) will need to be written, likely also using `eventsource_stream` to decode Anthropic's SSE into typed events, translate each into an OpenAI chunk, and re-serialize. This isn't a "new crate" question, it's new logic — flag for the planning phase.

## Graceful shutdown / connection handling / loopback bind

No existing graceful-shutdown code in the repo (checked: no hits for `graceful`, `with_graceful_shutdown`, `CancellationToken` used this way, `ctrl_c` outside `cmdcrush`'s OTel shutdown). Current idiomatic axum 0.8 + tokio pattern (`axum::serve` returns a builder with `.with_graceful_shutdown(fut)`):

```rust
let listener = TcpListener::bind(("127.0.0.1", config.port)).await?;
axum::serve(listener, router)
    .with_graceful_shutdown(shutdown_signal())
    .await?;

async fn shutdown_signal() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.expect("ctrl_c handler") };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}
```
This is the standard shape used across the axum ecosystem for 0.7/0.8 (unchanged between those versions) — `axum::serve`'s `.with_graceful_shutdown()` waits for in-flight connections to finish (including in-flight SSE streams) before returning, which matters here because a mid-stream shutdown should let the current response finish rather than cut the client off; `tokio::signal::unix::SignalKind::terminate` needs the `signal` feature of `tokio`, already covered by `features = ["full"]`.

**Loopback-only bind**: `Constraints` in the requirements doc pin this to `Config.port` on `127.0.0.1` (matching `src/cost_metrics/server.rs:227`'s existing `TcpListener::bind(addr)` where `addr` is built from a `127.0.0.1` literal). No `0.0.0.0` fallback, no config surface for bind address — construct the `SocketAddr` directly as `(Ipv4Addr::LOCALHOST, config.port).into()` or the string form `format!("127.0.0.1:{port}")`; do not accept a bind-address override from CLI/config/env, per the explicit constraint.

**Connection handling**: `tower-http = { version = "0.7", features = ["trace", "timeout"] }` is already a dependency — layer `tower_http::trace::TraceLayer::new_for_http()` for the requirements doc's "Standard request logging (method, path, upstream selected, status, latency)" observability requirement, and `tower_http::timeout::TimeoutLayer` for request timeouts, both applied via `.layer(...)` on the axum `Router` — but a blanket `TimeoutLayer` must NOT wrap the SSE/streaming routes with a short timeout, since a long-lived stream is expected to run past any reasonable fixed timeout; scope it to non-streaming routes or use a generous ceiling only meant to catch hangs, not bound stream duration.

## Anthropic-native vs. OpenAI-compat routing

Two separate axum routes on the same `Router` (axum's), both funneling into the one ADR-003 `Router::dispatch` per the requirements doc's explicit constraint ("no per-request route selection logic beyond that"):

- `POST /v1/messages` — Anthropic-native, body passed to `dispatch` close to as-is (map query/header `stream` flag from the JSON body's `"stream": true/false` field, matching Anthropic's own API contract, not a query param).
- `POST /v1/chat/completions` — OpenAI-compat path (exact path is an explicit Open Question in the requirements doc, but `/v1/chat/completions` is the de facto standard every OpenAI-compatible proxy in this space uses; the requirements doc defers final confirmation to Phase 2 research) — body run through `translate_openai_to_anthropic` before `dispatch`, response run through `translate_anthropic_to_openai` (non-streaming) or the not-yet-written per-chunk translator (streaming) before returning to the client.

Both handlers share one `axum::extract::State<AppState>` where `AppState` wraps (at minimum) the ADR-003 `Router` and the `CostTracker`, `Arc`-wrapped for cheap cloning into each request's task, matching the `State` pattern already used in `src/cost_metrics/server.rs:22`.

## Streaming failover boundary (ADR-003 rabbit hole)

Per the requirements doc: failover across upstreams is only valid **before** the first byte is flushed to the client. Mechanically, this means: call `Router::dispatch(...).await` to get a `ProviderResponse` (this await already contains all of ADR-003's cross-upstream retry logic per `src/routing/router.rs:72-117` — the loop runs entirely before `dispatch` returns), and only *after* getting back `ProviderResponse::Stream(s)` do you hand `s` to `Body::from_stream` and return the axum `Response`. Because `dispatch`'s retry loop already completes before any bytes reach the HTTP response body, the "point of no return" is naturally the `dispatch(...).await` return, not something the handler needs to reimplement — the handler just must not attempt to retry once it has a `ProviderResponse::Stream` in hand. A mid-stream error inside the `Stream` itself must surface as an in-band SSE `event: error` entry (map the stream's `Result::Err` arm to synthesize an SSE error frame rather than truncating silently or trying to swap providers), per the requirements doc.

## Sources
- [axum::response::sse docs (docs.rs, current)](https://docs.rs/axum/latest/axum/response/sse/index.html)
- [axum sse.rs source](https://docs.rs/axum/latest/src/axum/response/sse.rs.html)
- WebSearch: "axum 0.8 SSE streaming handler Body::from_stream best practice 2026" — confirms `Sse` vs. `Body::from_stream` split (event-producer vs. byte-passthrough), and that axum 0.8's SSE surface is unchanged from 0.7.x.
- In-repo: `src/cost_metrics/server.rs` (existing axum 0.8 Router/State/TcpListener/axum::serve pattern, and `oneshot`-based route tests) — read directly, not web-sourced.
- In-repo: `src/providers/anthropic.rs:564-587` (`Provider::send` impl showing `ProviderResponse::Stream` is raw `reqwest::bytes_stream()` output, not parsed `Event`s).
- In-repo: `src/routing/router.rs:62-117` (`Router::dispatch` — confirms all cross-upstream retry happens inside the `.await`, before a `ProviderResponse` is returned, which is what makes the streaming failover boundary trivial to respect from the handler side).
- In-repo: `Cargo.toml` (dependency versions, read directly).
