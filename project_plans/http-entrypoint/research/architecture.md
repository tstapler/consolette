# Research: axum HTTP entrypoint architecture

Scope: how to structure the axum layer on top of `Router::dispatch` (ADR-003), the streaming
first-byte-flush boundary, cost-tracking wiring, and integration points. Infrastructure/CRUD-shaped
proxying — no EventStorming table.

## 1. Module shape: thin handlers, a dedicated entrypoint module

CLAUDE.md's rule ("keep transport code thin; put real logic where it's independently testable")
already has a precedent in this repo: `src/cost_metrics/server.rs` is the model to copy, not
`src/main.rs`. That file:

- Defines an `AppState`-equivalent (`CostServerState`) holding all the `Arc<...>` shared state,
  built by an async `build()`/`build_with_...()` constructor that is unit-testable with no HTTP
  involved (`src/cost_metrics/server.rs:96` `CostServerState::build_with_session_glob`).
- Keeps `axum::Router::new().route(...)` construction and the `Json`/`Path`/`State` extractors as
  the only "transport" code, with handler bodies delegating immediately into the state's methods
  or into free functions in sibling modules (`report.rs`, `client.rs`).
- Exposes a top-level `serve_cost(port)`-shaped function that `main.rs` calls with zero logic of
  its own (`src/main.rs:207` `serve_cost_command`).

Recommendation: add a new module, e.g. `src/entrypoint/mod.rs` (or `src/entrypoint/server.rs` +
`src/entrypoint/handlers.rs`), following this same split:

- **`EntrypointState`** (name TBD) — holds `Arc<Router>` (the ADR-003 router, not axum's),
  `Arc<CostTracker>`, and whatever config/session-key derivation is needed. Built by an async
  constructor from `Config` that is unit-testable independent of a bound socket.
- **Handlers** (`post_v1_messages`, `post_v1_chat_completions`) do exactly three things: parse/
  validate the incoming request into the shape `Router::dispatch` expects, call `dispatch`, and
  translate `ProviderResponse`/`ProviderError` into an axum response. All translation logic
  (OpenAI↔Anthropic) and all cost-recording logic already lives in `src/providers/mod.rs` and
  `src/cost_metrics/*` — handlers call into those, they do not reimplement them.
- A `serve(port)` function mirroring `cost_metrics::server::serve_cost` — binds
  `TcpListener::bind(("127.0.0.1", port))` (loopback per the requirements' NFR/constraint) and
  runs `axum::serve`.
- `main.rs`'s `Command::Run` arm becomes a one-line delegate, exactly like `Command::ServeCost`.

This keeps `Router::dispatch` (the ADR-003 dispatch loop) completely untouched — handlers are a new
caller, not a modification site.

## 2. Missing piece: no `Config -> Router` constructor exists yet

`Router::new(candidates, providers, strategy, health, admission)` (`src/routing/router.rs:36`) is
only ever called from its own test module and nowhere in production code
(`grep -rn "Router::new" src` returns only `routing/router.rs` and test files under
`cost_metrics/`). There is currently **no function that builds a `Router` from `Config`** —
`Config.upstreams: Vec<Upstream>` / `Config.routes: Vec<Route>` (`src/config/schema.rs:264-266`)
are parsed but never turned into `UpstreamRef`s, concrete `Provider`s, a `RoutingStrategy`, a
`HealthRegistry`, or an `AdmissionControl`. This wiring — reading `Config.routes[0]` (the "exactly
one default Route" the requirements scope to), instantiating `AnthropicProvider`/`BedrockProvider`
per `Upstream` (constructors at `src/providers/anthropic.rs:80` and `src/providers/bedrock.rs`),
building `HealthRegistry::new(config.cooldown_seconds)` and `RateLimiters::new(&config.ratelimit)`
(`src/ratelimit/mod.rs:33`) as the `AdmissionControl` impl — is new code this feature must add, most
naturally as a `Router::from_config(&Config) -> anyhow::Result<Router>` (or a free function) in
`src/routing/mod.rs` or the new entrypoint module. It's a first-class deliverable, not incidental
plumbing, and should be planned/tested as such (e.g. what happens with 0 routes, >1 route — out of
scope per requirements but must not panic — and unrecognized `UpstreamKind`).

## 3. Streaming: the first-byte-flush boundary is enforced by `Router::dispatch` itself, not the handler

ADR-003 (`project_plans/consolette/decisions/ADR-003-routing-strategy-trait.md:61-67`) states the
rule precisely: "the connect-time retry loop must open a working body (or exhaust candidates)
*inside* `dispatch` before handing the `ProviderResponse::Stream` outward... The current providers
already return `Err` on connect-time status before yielding the stream, so no new hazard."

This means the "committed vs. still failable" boundary is **not a new state the axum handler has to
track carefully** — it already coincides with `Router::dispatch`'s `Ok`/`Err` return:

- While `dispatch` is looping over candidates, every attempt is a `provider.send(...).await` call.
  A provider's `send` only returns `Ok(ProviderResponse::Stream(...))` once it has validated the
  connect-time response status — a 4xx/5xx from the upstream comes back as `Err`, which is exactly
  what feeds the existing error-class branching (`is_validation`/`is_auth`/`is_rate_limited`/
  transient) and continues the failover loop. So **by the time `dispatch` returns
  `Ok(ProviderResponse::Stream(stream))` to the handler, failover is no longer possible or
  necessary** — the handler is free to immediately write `200 OK` and start forwarding
  `Bytes` chunks.
- The handler's job is therefore simple and matches axum's own streaming idiom: on
  `Ok(ProviderResponse::Stream(s))`, build a response with `axum::body::Body::from_stream(s)` (or
  `axum_extra`'s SSE helper if the Anthropic-native shape needs `event: ...` framing) and return
  it — no buffering of the whole stream, no manual "is this the first byte" tracking.
- **What must NOT happen**: don't call `dispatch` once per byte or retry it after the axum response
  has been constructed. The single `dispatch().await` call is the entire retry boundary; once it
  returns `Ok`, this handler's only remaining job is relaying bytes and (per §4) recording cost
  once the stream ends or errors mid-flight.
- **Mid-stream failure after the boundary**: a transport-level error surfacing while consuming
  `Stream::Item = Result<Bytes, anyhow::Error>` after the response has started flushing cannot
  trigger router failover (ADR-003 is explicit this is impossible once bytes are sent) — it can
  only become an in-band SSE `error` event (Anthropic-native) or a terminated stream (OpenAI-compat,
  which has no error-event convention as rich as Anthropic's `event: error`). This is exactly the
  "cost-tracking on partial/failed streams" rabbit hole the requirements call out — the handler
  needs to wrap the byte-stream in an adapter that (a) forwards bytes/errors to the client
  unchanged and (b) separately observes whether the stream ended cleanly, ended with an upstream
  error mid-flight, or was cut short, so it can call the right `CostTracker` method afterward
  (§4). A `futures::stream::StreamExt::inspect` or a hand-rolled wrapping `Stream` impl that
  captures the last chunk / an `Arc<Mutex<Outcome>>` flag, checked in a `Drop` or after the stream
  is fully polled, is the natural shape — no existing helper for this exists yet in the codebase.

## 4. Cost-tracking wiring: call the existing functions from the new seam, don't duplicate

`translate_and_record` (`src/providers/mod.rs:335`) already does exactly what's needed for the
**OpenAI-compat, non-streaming success path**: it calls
`record_actual_usage_from_anthropic_response` and then `translate_anthropic_to_openai`. Its own
doc comment states this handler-seam is precisely what's missing today. Concretely:

- **OpenAI-compat endpoint, full response, success**: handler calls `dispatch(...)`, gets
  `ProviderResponse::Full(anthropic_json)`, and calls `translate_and_record(tracker, session_key,
  request_id, &anthropic_json)` directly — this is the TODO's call site, verbatim, no new
  wrapper needed.
- **Anthropic-native endpoint, full response, success**: there's no existing `translate_and_record`
  equivalent that skips the OpenAI translation — either add a thin sibling (e.g.
  `record_anthropic_response` calling just `record_actual_usage_from_anthropic_response` without
  the `translate_anthropic_to_openai` call) or call
  `crate::cost_metrics::record_actual_usage_from_anthropic_response` directly from the handler
  before serializing the passthrough JSON. Prefer adding the small sibling function next to
  `translate_and_record` in `providers/mod.rs` so both endpoint handlers share one call site
  rather than the handler reaching two modules deep into `cost_metrics` directly — keeps the
  "thin handler" property.
- **Failure path** (`dispatch` returns `Err(ProviderError)`, before any bytes sent): call
  `tracker.record_request_failed(&session_key, request_id).await` (`src/cost_metrics/tracker.rs:270`)
  — this is the other half of the TODO, and per the doc comment this is "the nearest
  `Provider::send`/`Router::dispatch` error-handling seam," i.e. exactly the `Err` arm of the
  handler's `match dispatch(...).await`.
- **Streaming success/failure**: per §3, wrap the outgoing stream so that when it finishes (end of
  stream, or an `Err(anyhow::Error)` item), the handler calls either
  `record_actual_usage_from_anthropic_response` (if a final `message_stop`/usage event was seen in
  the SSE frames — Anthropic's streaming format emits a final `usage` delta) or
  `record_request_failed` (clean end with no usage data, or a mid-stream error). This is the
  hardest of the three cost-tracking paths and matches the requirements' explicit "Rabbit Hole"
  call-out; it needs its own test coverage for: (a) clean stream with usage, (b) clean stream with
  no parseable usage, (c) mid-stream upstream error.
- **`SessionKey`/`RequestId` provenance**: `record_pending`/`record_actual_usage`/
  `record_request_failed` all key off `(SessionKey, RequestId)` (`src/cost_metrics/tracker.rs:133,
  211, 270`), a model built for `SessionCompactionPipeline`'s Claude-Code-session use case, not a
  generic HTTP proxy request. A live HTTP request to `/v1/messages` has no Claude Code session
  associated with it. Options to resolve, worth flagging for the planning phase:
  1. Synthesize a `SessionKey` per request (e.g. a fresh UUID) with `RequestId::new()` per request,
     giving each proxied call its own single-record "session" — matches
     `record_actual_usage`'s documented tolerance for "no `record_pending` row exists yet" (it
     upserts via `get_or_init`, per `tracker.rs:192-199`), so `record_pending` isn't even a hard
     prerequisite for the success path.
  2. Accept an optional client-supplied session identifier (e.g. a header) to let multiple
     proxied requests be grouped, if that's ever wanted — likely out of scope for this pass since
     the requirements don't call for it.
  Given the requirements' "closing the gap" framing (Success Metrics: "A `CostTracker` record
  exists for a completed request"), option 1 (synthesize per-request) is the minimal correct
  choice for this pass.

## 5. Integration points

- **Router (ADR-003)**: `Router::dispatch(body, headers, stream, est_tokens)`
  (`src/routing/router.rs:62`) is the single call both endpoint handlers make. `est_tokens` needs a
  cheap estimate before dispatch — `TiktokenEstimator` already exists
  (`src/cost_metrics/estimator.rs`, used by `CostTrackingHook`) and is the natural reuse rather than
  inventing a second estimator.
- **Auth / exec.rs (ADR-007)**: no direct integration point for the handler — `ExecCredentialCache`
  is composed into `Provider::send` (concrete `AnthropicProvider`/`BedrockProvider` construction),
  which happens during the `Config -> Router` build step (§2), not per-request. The handler layer
  never touches `auth::exec` directly; this "applies transparently" exactly as the requirements
  state, contingent on the `Router::from_config` step correctly threading `Upstream.auth` into each
  provider's constructor.
- **Config**: `Config.port` (`src/config/schema.rs:243-244`, default 47000 via `default_port()`) is
  the bind port — resolved once at `Command::Run` startup, no per-request config reads.
  `Config.cooldown_seconds` feeds `HealthRegistry::new`, `Config.ratelimit` feeds `RateLimiters::new`
  — both at the same one-time `Router::from_config` step.
- **`HealthRegistry`/`strategy.rs`**: confirmed via full read — both are pure-sync, no `.await`
  anywhere inside `HealthRegistry`'s methods or either `RoutingStrategy` impl
  (`src/routing/health.rs`, `src/routing/strategy.rs`). The "never hold a DashMap guard across
  `.await`" hazard ADR-003 calls out is a constraint on *future* changes to `HealthRegistry`
  internals, not something the new async handler layer needs to actively defend against today —
  `is_available`/`trip`/`remaining_secs` all return before any `.await` point since they take no
  `.await` at all. The handler only ever calls these transitively through `Router::dispatch`, never
  directly.

## Summary of concrete gaps this feature must fill (not yet existing anywhere in the codebase)

1. `Config -> Router` construction (§2) — no such function exists; needs its own tests.
2. `src/entrypoint/` (or similar) module: `EntrypointState`, two handlers, `serve(port)`,
   mirroring `cost_metrics::server.rs`'s shape.
3. A small `record_anthropic_response`-style sibling to `translate_and_record` for the
   Anthropic-native (non-OpenAI-translated) success path (§4).
4. A stream-wrapping adapter that observes terminal outcome (clean+usage / clean+no-usage /
   mid-stream error) to drive the correct `CostTracker` call after forwarding completes (§3, §4) —
   no precedent for this exists in the codebase yet.
5. Per-request `SessionKey`/`RequestId` synthesis for cost-tracking calls (§4) — the tracker's
   session model currently assumes a Claude Code transcript session, not a generic proxied request.
