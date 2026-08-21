# Pitfalls Research: http-entrypoint

Scope: risks specific to wiring `axum`/`tokio` streaming proxy endpoints on top of
`Router::dispatch` (ADR-003), exec-based credential injection (ADR-007,
`src/auth/exec.rs`), and Anthropic/OpenAI schema translation
(`src/providers/mod.rs`).

## 1. Streaming/SSE proxy pitfalls (axum + tokio)

- **Point-of-no-return ambiguity for failover.** ADR-003 says failover is only
  legal pre-first-byte. The actual boundary in code is "before the first
  chunk is written to the client's response body," which is *not* the same
  moment as "before the first chunk is read from upstream." A naive
  implementation that treats "received first SSE event from upstream" as the
  cutover point can still fail over correctly, but one that starts writing to
  the axum response `Sender`/body stream before fully validating the first
  event (e.g. before confirming it isn't an upstream-side `error` SSE event)
  will have already committed the client to a broken stream. Design so the
  "have we flushed anything yet" flag is set exactly at the `body.send()`/
  `poll_next` call that hands bytes to axum, not at the point bytes leave the
  provider's HTTP client.
- **First-event buffering necessarily adds latency.** If the design buffers
  the first upstream SSE event before flushing (to make the failover decision
  cleanly), that must be scoped to *one event*, not implemented as "buffer
  until upstream sends a fixed size" — Anthropic first events can be small
  (`message_start`) and the buffer must be event-delimited (`\n\n`), not
  byte-count delimited, or it will corrupt/merge frames under partial reads.
- **Client-disconnect handling.** Axum drops the response future when the
  client disconnects, but the upstream request future (reqwest/hyper stream)
  keeps running unless explicitly tied to the same task via a `select!` on a
  cancellation signal or `Body::from_stream` drop propagation. Without an
  explicit check, a client that closes its TCP connection mid-response leaves
  an orphaned upstream stream consuming bandwidth/cost until upstream itself
  times out — this directly undermines cost tracking, since tokens will
  continue to accrue for a request nobody is reading. Verify `axum::body::Body::from_stream`'s
  drop behavior propagates a cancel to the underlying `reqwest::Response`
  byte stream (it generally does via `Stream::drop`, but only if the stream
  chain has no `.boxed()`/buffering adapter that detaches cancellation,
  e.g. `tokio::sync::mpsc` bridging without dropping the sender on cancel).
- **Timeout/keepalive interaction with `tower_http::timeout::TimeoutLayer`.**
  `tower-http` (already a dependency, feature `timeout`) applies one timeout
  to the whole request-response cycle by default. For a streaming SSE
  response that can legitimately run for minutes, a blanket per-request
  timeout applied at the tower layer will kill in-flight streams. This needs
  to be scoped to the "time to first byte" phase only (i.e., not applied via
  the global layer, or applied with a much longer budget that only exists to
  catch a truly hung upstream) — otherwise ordinary long completions get
  killed mid-stream and the mid-stream-cut cost-tracking path (flagged in
  requirements) gets exercised constantly by an operational bug rather than
  a real upstream failure.
- **Backpressure and unbounded buffering.** If translation/relaying is
  implemented as "read entire upstream stream into memory, then re-emit,"
  large streaming responses (long completions, especially with big tool-use
  blocks) will balloon memory. Prefer a straight `Stream -> Stream` pipeline
  (map/relay per SSE frame) over collect-then-replay. Also watch for
  `axum::body::Body::from_stream` requiring `Result<Bytes, E>` items where
  `E: Into<axum::BoxError>` — a stream that never yields an `Err` variant
  (infallible) still needs the type parameter satisfied, a common compile
  friction point that tempts people into `unwrap()`-ing errors into panics
  instead of an in-band SSE `error` event.
- **Keep-alive / idle SSE comments.** Anthropic's own streaming API and most
  reverse proxies/load balancers expect periodic `: keep-alive\n\n` comment
  lines (or equivalent) on idle SSE connections to prevent intermediate
  proxies/L7 LBs from closing idle connections. Since this server is
  loopback-only (no LB in front), this is lower risk than in a public
  deployment, but any client-side idle-read timeout (e.g. an OpenAI SDK's
  default read timeout) can still fire if upstream pauses between events
  longer than the client's timeout — worth explicitly deciding whether to
  synthesize keep-alive comments or document that clients must set generous
  read timeouts.
- **Header/trailer requirements for SSE over axum.** Missing
  `Content-Type: text/event-stream`, `Cache-Control: no-cache`, or
  `X-Accel-Buffering: no`-equivalent hints causes some HTTP client libraries
  (and `curl` output buffering under certain terminal conditions) to appear
  to hang or buffer full responses before delivering — easy to mistake for a
  server bug when it's a missing header. `axum-extra`'s `Sse` response type
  handles the framing but the crate's default keep-alive interval and content
  type still need verification against what Anthropic-API clients expect.

## 2. Anthropic <-> OpenAI schema translation pitfalls

`src/providers/mod.rs:132` (`translate_openai_to_anthropic`) and `:243`
(`translate_anthropic_to_openai`) are unit-tested in isolation
(confirmed — see the file's own comment at line 293-296 noting
`translate_anthropic_to_openai` has "no production caller in this codebase
as of this story"). Wiring them to real dispatch is exactly where the
requirements doc flags edge cases surfacing for the first time:

- **Tool calls / function calling shape mismatch.** OpenAI's
  `tool_calls` array (with `id`, `type: "function"`, `function.name`,
  `function.arguments` as a *JSON string*) vs. Anthropic's `tool_use`
  content blocks (`input` as a native JSON object, not a stringified one).
  A translation bug here is silent — it produces syntactically valid JSON
  that a real LLM client then rejects or mishandles ("arguments" being
  double-encoded, or an object being sent where a JSON string is expected).
  This class of bug will not show up in unit tests that construct requests
  by hand matching the code's own assumptions; it needs an end-to-end
  round-trip test with a real tool-calling transcript.
- **Multi-block content translation.** Anthropic content is always an array
  of typed blocks (`text`, `tool_use`, `tool_result`, `image`, `thinking`,
  etc.); OpenAI's `content` field is either a bare string or (for vision) an
  array of `{type, text|image_url}`. A translator that assumes "one text
  block per message" (a common initial implementation shortcut) will silently
  drop or concatenate-incorrectly any message with multiple content blocks —
  this is especially likely to bite multi-turn tool-result exchanges, where
  Anthropic emits a `tool_result` block that must map to an OpenAI `tool`
  role message, not inline content.
- **Stop-reason vocabulary mismatch.** Anthropic: `end_turn`, `max_tokens`,
  `stop_sequence`, `tool_use`. OpenAI: `stop`, `length`, `tool_calls`,
  `content_filter`, `function_call` (deprecated). There is no injective
  mapping — `tool_use` and `tool_calls` line up, but Anthropic has no
  equivalent of `content_filter` and OpenAI has no equivalent of
  `stop_sequence` (which sequence stopped it gets lost in translation unless
  explicitly carried in a vendor extension field). Decide up front which
  direction is lossy and document it, rather than discovering it because a
  client's finish-reason branching breaks.
- **`n` (multiple completions) parameter.** OpenAI's chat API supports
  `n > 1` (multiple choices per request); Anthropic's Messages API has no
  such parameter. If the OpenAI-compatible endpoint doesn't explicitly
  reject `n > 1` (or emulate it via multiple sequential/parallel dispatches),
  a client requesting `n=3` will silently get a single choice back in an
  array of size 1, and code iterating `choices[1]`/`choices[2]` will panic or
  misbehave downstream — this should fail fast with a clear 400 rather than
  degrade silently.
- **System message placement.** OpenAI puts system content inline as a
  `{role: "system"}` message in the `messages` array; Anthropic hoists it out
  to a top-level `system` field (string or content-block array, and callers
  using prompt caching put `cache_control` on it — relevant since this repo
  already has `src/system_prompt/cache_aligner.rs`). A translator that only
  looks at the first message for `role == "system"` will mishandle multiple
  system messages or a system message that isn't first.
- **Token/usage field shape differences propagating into `CostTracker`.**
  Anthropic reports `input_tokens`/`output_tokens` (plus cache read/write
  token fields); OpenAI reports `prompt_tokens`/`completion_tokens`/
  `total_tokens`. If the OpenAI-compatible path's translated usage numbers
  are computed by re-deriving from the *already-translated* Anthropic-shaped
  usage rather than from the raw upstream response, cache-token accounting
  (a first-class concept in this codebase per `system_prompt/cache_aligner.rs`
  and the existing cost_metrics work) can silently vanish for OpenAI-API
  clients even though it's tracked correctly for native Anthropic clients.
- **Streaming-specific translation is a second, harder translation surface.**
  The two functions found (`translate_openai_to_anthropic`,
  `translate_anthropic_to_openai`) operate on full JSON values — i.e. they
  translate *non-streaming* bodies. Streaming SSE translation (Anthropic's
  `message_start`/`content_block_delta`/`message_delta`/`message_stop` event
  sequence vs. OpenAI's `chat.completion.chunk` deltas) is a materially
  different, stateful translation problem (has to track content-block
  indices, accumulate partial JSON for tool-call argument deltas, and decide
  when to emit a synthetic final chunk) that these two functions do not
  cover at all. Confirm during planning whether streaming OpenAI-compatible
  translation exists anywhere, or whether it needs to be designed as a new
  piece of stateful code, not just "call the existing translator per chunk."

## 3. axum 0.7 -> 0.8 API-surface gotchas

The crate is pinned to `axum = "0.8"` (`Cargo.toml`); most tutorials, Stack
Overflow answers, and even some LLM training data default to 0.6/0.7 idioms.
Known-breaking surface between 0.7 and 0.8 to watch for:

- **Path parameter syntax changed** from `/:id` to `/{id}` (and wildcard
  `/*rest` to `/{*rest}`). Any route registered with the old `:param` syntax
  either fails to compile against 0.8's `axum::routing::path` matcher or (in
  some transitional versions) panics at router-build time with a message
  about invalid path syntax — easy to hit if copy-pasting route definitions
  from older examples or from `matchit`-based tutorials.
- **`Handler`/extractor trait bound changes and `FromRequest`/`FromRequestParts`
  churn** across 0.7 -> 0.8 occasionally require re-deriving custom
  extractors (relevant if the entrypoint adds a custom extractor for e.g.
  pulling the resolved route/model out of request state). Check the axum
  0.8 CHANGELOG for the specific version pinned in `Cargo.lock`, not just
  "0.8" from `Cargo.toml`, since 0.8.x point releases also moved things.
  Not verified against this repo's exact `Cargo.lock`-pinned patch version
  in this research pass — needs a compile check during planning/implementation, not an
  assumption.
- **`axum-extra`'s `Sse` type and `typed-header` feature** (both present in
  `Cargo.toml`) need version-matching against the pinned `axum-extra = "0.10"`
  — `axum-extra` versions track axum major versions loosely; mixing an
  `axum-extra` built against a different `axum` minor than the one resolved
  in `Cargo.lock` produces trait-mismatch compile errors that look like
  unrelated type errors (a very common axum ecosystem support-forum
  question). Run `cargo tree -i axum` once wiring starts to confirm a single
  resolved `axum` version across `axum`, `axum-extra`, and `tower-http`.
- **`Router::with_state` / state type inference** got stricter in later 0.7/
  0.8 releases — a `Router<S>` that isn't turned into `Router<()>` via
  `.with_state(...)` before being passed to `axum::serve` won't compile, and
  the resulting error message points at `axum::serve`'s bound rather than at
  the missing `.with_state()` call, which is a common source of confusing
  "the trait bound is not satisfied" errors for people used to older axum.
- **`axum::serve` replaced `Server::bind(...).serve(...)`.** 0.8 (like late
  0.7) requires binding a `tokio::net::TcpListener` yourself and passing it
  to `axum::serve(listener, app)`, which changes how graceful shutdown and
  loopback-only enforcement are wired — worth explicitly binding
  `127.0.0.1:<port>` (not `0.0.0.0`) at the `TcpListener::bind` call site
  and adding a test that asserts the bind fails/refuses on a non-loopback
  interface, since a config-driven port with no interface field is an easy
  place to accidentally widen the bind address later (e.g. if someone later
  adds an "allow remote access" toggle without noticing the constraint was
  previously structural, not configured).

## 4. Credential/header-leakage risks (ADR-007 exec credential helper)

`src/auth/exec.rs` runs an external command per upstream and caches the
resulting `HeaderMap` (`ExecAuthCache`, `:37-43`), converting helper-returned
`(name, value)` string pairs into `http::HeaderValue`s (`:254-262`). Once
those headers are attached to an outbound provider request, they become an
ambient secret that every logging/error/tracing layer touching that request
can accidentally surface:

- **`tower_http::trace::TraceLayer`** (feature already enabled in
  `Cargo.toml`) logs request/response metadata by default at `DEBUG`/`TRACE`
  levels; its default `on_request`/`on_response` callbacks do not log headers
  unless customized, but any customization added later (e.g. "log all
  headers for debugging a routing issue") would trivially leak `Authorization`
  or provider-specific auth headers (e.g. `x-api-key`) into log output. This
  needs an explicit denylist/redaction if header logging is ever added, and a
  test asserting it.
- **`ProviderError::Upstream { status, body }`** (`src/providers/mod.rs`)
  carries the raw upstream response body. If an upstream ever echoes request
  headers back in an error body (some APIs do this for malformed-request
  diagnostics), that body — which flows into `ProviderError`'s `Display` impl
  and potentially into client-visible error responses or cost-tracking
  records — could leak credential material. The HTTP entrypoint's
  error-to-client-response mapping must not pass upstream error bodies
  through verbatim without at least confirming upstream never echoes
  auth headers, or must scrub before forwarding.
- **Panics with `Debug`-derived context.** Any `.unwrap()`/`.expect()` on a
  `Result` that contains a `HeaderMap` or `HeaderValue` in its `Debug` output
  (e.g. an error type that derives `Debug` and stores headers) will print
  the raw header value — including secrets — to stderr/panic logs if it ever
  panics inside a request-handling task. Since this is new server code
  (a fresh dispatch seam), any `unwrap()` reachable from a request handler
  is a direct incident risk, not just a crash risk — prefer `Result`
  propagation with a type that explicitly redacts headers in its `Debug`/
  `Display` impls over deriving `Debug` on anything holding a `HeaderMap`.
- **Per-request header injection point must not be visible to clients.**
  ADR-007's headers are meant to be injected server-side into the *upstream*
  request. The HTTP entrypoint must ensure the axum extractor/handler for
  the client-facing request never accidentally merges the client's own
  incoming headers with the exec-helper-injected ones in a way that either
  (a) forwards the client's own `Authorization` header upstream unexpectedly,
  or (b) reflects the injected upstream credential back to the client (e.g.
  via a naive "echo all headers back for debugging" middleware, or via
  `tower_http::trace` in `TRACE`-with-headers mode). This is a "explicitly
  design against" item: the client-request header set and the
  upstream-request header set should be two distinct `HeaderMap`s that never
  get merged wholesale.
- **Cache-hit path (`:111-129`) returns a cloned `HeaderMap` on every
  dispatch** — under concurrent request load (multiple simultaneous client
  requests routed through the same upstream), this is a hot, high-frequency
  clone of secret material. Not a correctness bug, but worth confirming the
  cache's `Mutex`/`RwLock` (not yet located in this pass) isn't held across
  an `.await` in the exec-invocation path in a way that would need
  `tokio::sync::Mutex` rather than `std::sync::Mutex` — mixing them wrong is
  a classic async-Rust deadlock source, distinct from the leak risk but
  adjacent code to double-check.

## 5. Cost-tracking coverage gaps (flagged directly in requirements)

Three code paths must each independently record correctly, and it's easy to
implement only the first:

1. **Clean success** — full response or full SSE stream completes normally.
   Straightforward; likely already covered by `translate_and_record`
   (`src/providers/mod.rs:335`) for the non-streaming case.
2. **Pre-first-byte failure** — `Router::dispatch` exhausts failover options
   before any bytes reach the client. Needs a cost-tracking hook on the
   *final* failure after all retries, not per-attempt (or usage would be
   double-counted/wrongly zeroed across retries) — worth confirming whether
   `Router::dispatch` (`src/routing/router.rs:62`) exposes per-attempt vs.
   final-outcome hooks.
3. **Mid-stream cut** — upstream fails after some SSE events have already
   flushed to the client. This is the hardest: partial token usage has to be
   estimated or read from whatever partial `message_delta`/`usage` events did
   arrive before the cut, since the final `message_stop` (which normally
   carries authoritative usage) never arrives. A naive implementation that
   only records cost on seeing a clean terminal event will silently record
   **zero cost for partially-consumed, non-free upstream calls** — an actual
   billing-accuracy bug, not just a metrics gap. This should be designed
   explicitly (e.g. periodic/best-effort usage accumulation from
   intermediate `message_delta` events) rather than deferred.

## Sources

- Requirements doc (`project_plans/http-entrypoint/requirements.md`) and its
  own cited TODO in `src/providers/mod.rs`'s `translate_and_record`.
- Direct inspection: `src/providers/mod.rs` (translation functions, lines
  132, 243, 293-296, 335), `src/auth/exec.rs` (header cache/injection, lines
  19, 37-43, 90-129, 195-265), `src/routing/router.rs:62` (`dispatch` entry
  point), `Cargo.toml` (axum 0.8, axum-extra 0.10, tower-http 0.7 with
  `trace`/`timeout` features, tokio "full").
- General knowledge of axum 0.7->0.8 migration notes (path syntax
  `:param` -> `{param}`, `axum::serve` replacing `Server::bind`, state-type
  inference tightening) and common Rust async-streaming-proxy pitfalls
  (backpressure via unbounded buffering, cancellation-on-drop semantics,
  `tower_http::TimeoutLayer` scope) — general web/training knowledge, not
  pinned to a specific external doc; verify exact behavior against the
  `Cargo.lock`-resolved versions during implementation rather than relying on
  this summary.
- General knowledge of Anthropic Messages API vs. OpenAI Chat Completions
  API schema differences (content blocks vs. flat content, tool_use vs.
  tool_calls, stop_reason vocabularies, system message placement, `n`
  parameter, streaming event models) — general/training knowledge; the
  authoritative source for exact current field shapes is each vendor's own
  API reference, not re-verified against live docs in this research pass.
