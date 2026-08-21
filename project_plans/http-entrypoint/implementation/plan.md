# Implementation Plan: http-entrypoint

**Feature**: Bind a real, loopback-only HTTP server to `Config.port` (default 47000) that serves an Anthropic-native `POST /v1/messages` endpoint and an OpenAI-compatible `POST /v1/chat/completions` endpoint, both dispatching through the existing `Router::dispatch`, with `CostTracker` wired at the dispatch seam and full streaming (SSE) support for both wire formats.
**Date**: 2026-08-19
**Status**: Draft
**ADRs**:
- [ADR-016: Per-request SessionKey/RequestId synthesis and record_pending-first sequencing](../decisions/ADR-016-session-key-synthesis-and-record-pending-sequencing.md)
- [ADR-017: Manual Body::from_stream over axum::response::sse::Sse](../decisions/ADR-017-manual-body-from-stream-over-axum-sse.md)
- [ADR-018: Streaming failover point-of-no-return and mid-stream error framing](../decisions/ADR-018-streaming-failover-boundary-and-mid-stream-error-framing.md)

---

## Step 0.5 — Alternatives Considered

1. **Thin `src/entrypoint/` module mirroring `src/cost_metrics/server.rs`'s State/router-builder/`serve()` shape** (one `EntrypointState` struct built once, an `entrypoint_router(state) -> axum::Router` builder function, a `serve_entrypoint(port: u16) -> anyhow::Result<()>` function, and separate handler files per wire format). *Strength*: directly mirrors an existing, working, tested pattern in this same codebase (`src/cost_metrics/server.rs`'s `cost_router`/`serve_cost`/`CostServerState`), so there is no new architectural idiom for a maintainer to learn. *Weakness*: introduces one more top-level module and a small amount of boilerplate (a state struct, a router-builder function) that a single-file approach would avoid. **Chosen.**
2. **Fold the HTTP server directly into `src/main.rs`'s `run()` function** (no new module; handlers as free functions or closures in `main.rs`). *Strength*: fewer files, no new module to wire into `src/lib.rs`. *Weakness*: violates this repo's own stated architecture note ("keep transport code thin; put real logic where it's independently testable") by putting non-trivial routing/streaming/error-mapping logic in the CLI entry point, and makes the streaming/translation logic unreachable from `cargo test`'s unit-test tree without spinning up the whole binary. **Rejected** — recorded in the Pattern Decisions table below.
3. **A single combined handler function for both `/v1/messages` and `/v1/chat/completions`**, branching internally on the request path to decide which translation/error-mapping rules to apply. *Strength*: one code path to keep the two response formats "in sync" if they drift. *Weakness*: the two endpoints have genuinely different request/response schemas, different error envelope shapes (Anthropic's `{"type":"error","error":{...}}` vs. OpenAI's `{"error":{...}}`), and different streaming frame formats — a combined handler would need an internal `if openai_mode` branch through nearly every line, which is harder to read and test than two small handlers sharing extracted helpers. **Rejected** — recorded in the Pattern Decisions table below.

## Step 1 — System Type

This is an **HTTP proxy/gateway server**: a thin transport/adapter layer sitting in front of an already-built, already-tested dispatch core (`Router::dispatch`). At the handler level it is Transaction-Script-shaped (PoEAA) — each handler is a short, linear sequence of steps (parse request → synthesize cost-tracking identifiers → dispatch → translate response → write it out) with no handler-local domain model of its own. The "real" domain logic (routing strategy, health, admission control, cost accounting) already lives below this layer and is reused, not reimplemented.

## Domain Glossary

| Term | Definition | Notes |
|---|---|---|
| `Router::from_config` | New associated function, `fn from_config(config: &Config) -> anyhow::Result<Router>`, that assembles a fully-wired `Router` (candidates, providers, strategy, health registry, admission control) from a loaded `Config`. Does not exist yet; first-class deliverable of Phase 1. | Factory Method (GoF) — see Pattern Decisions. |
| `DispatchRouter` | Import alias (`use crate::routing::router::Router as DispatchRouter;`) used inside `src/entrypoint/` to disambiguate consolette's own `Router` from `axum::Router`, which is used unaliased throughout the module — matches the convention already used (implicitly) in `src/cost_metrics/server.rs`, which never imports the dispatch `Router` and so never needed this alias. | Naming/import convention, not a separate ADR. |
| `EntrypointState` | New `Clone` struct bundling everything a handler needs: `dispatch_router: Arc<DispatchRouter>`, `cost_tracker: Arc<CostTracker>`. Passed to axum via `.with_state(state)`, analogous to `CostServerState` in `src/cost_metrics/server.rs`. | Facade-like state bundle — see Pattern Decisions. |
| `entrypoint_router` | New function `fn entrypoint_router(state: EntrypointState) -> axum::Router` that builds the axum `Router` with both routes registered and `tower_http::trace::TraceLayer` applied. | Mirrors `cost_router` in `src/cost_metrics/server.rs`. |
| `serve_entrypoint` | New async function `async fn serve_entrypoint(port: u16, state: EntrypointState) -> anyhow::Result<()>` that binds `SocketAddr::from(([127, 0, 0, 1], port))`, calls `axum::serve(listener, router).with_graceful_shutdown(shutdown_signal())`. Modeled line-for-line on `serve_cost` (`src/cost_metrics/server.rs:218-231`). | The hardcoded `127, 0, 0, 1` octets are what make the loopback-only NFR structurally true — `Config` has no bind-address field to override, only `port`. |
| `shutdown_signal` | New async function that awaits either a Ctrl-C (`tokio::signal::ctrl_c()`) or a SIGTERM (`tokio::signal::unix::signal(SignalKind::terminate())`) future via `tokio::select!`, returning once either fires. No precedent exists yet anywhere in this codebase; new for this feature. | Standard axum 0.8 graceful-shutdown idiom. |
| `post_v1_messages` | New handler, `async fn post_v1_messages(State(state): State<EntrypointState>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response`, backing `POST /v1/messages` (Anthropic-native). | `src/entrypoint/messages.rs`. |
| `post_v1_chat_completions` | New handler, same signature shape, backing `POST /v1/chat/completions` (OpenAI-compatible). | `src/entrypoint/chat_completions.rs`. |
| `CostTrackingStream` | New `Stream` adapter/wrapper type that sits between a `ProviderResponse::Stream`'s inner stream and the outgoing `Body::from_stream`, scanning passing SSE bytes for `event: message_delta`/`event: message_stop` frames to extract usage (via the existing `pub(crate) extract_usage`) without altering the bytes, then calling `CostTracker::record_actual_usage` once usage is found (or `record_request_failed` if the stream ends without one). | Decorator (GoF) — see Pattern Decisions. Lives in `src/entrypoint/cost_tee.rs`. |
| `OpenAiStreamTranslator` | New stateful per-chunk stream adapter, `src/entrypoint/openai_stream.rs`, that consumes the (already cost-teed) raw Anthropic SSE byte stream via `eventsource_stream::Eventsource::eventsource()`, and for each parsed Anthropic SSE `Event` emits zero or more OpenAI-shaped `data: {"id":...,"object":"chat.completion.chunk",...}\n\n` byte chunks, plus a final `data: [DONE]\n\n` sentinel once `event: message_stop` is observed. No existing per-chunk streaming translator exists in this codebase — the existing `translate_anthropic_to_openai`/`translate_openai_to_anthropic` (`src/providers/mod.rs:132,243`) only handle whole (non-streaming) JSON bodies. | Adapter (GoF) — see Pattern Decisions. |
| `map_provider_error_anthropic` | New free function, `fn map_provider_error_anthropic(err: &ProviderError) -> (StatusCode, serde_json::Value)`, mapping each `ProviderError` variant to Anthropic's `{"type":"error","error":{"type":"<error_type>","message":"..."}}` envelope and matching status (400/401/403/404/429/500/529) per `ux.md`. | `src/entrypoint/errors.rs`. |
| `map_provider_error_openai` | New free function, same signature shape, mapping to OpenAI's `{"error":{"message":"...","type":"...","param":null,"code":null}}` envelope. | `src/entrypoint/errors.rs`. |
| `ProviderError::Exhausted` (new variant) | New unit variant added to the existing `ProviderError` enum (`src/providers/mod.rs:32-46`), returned from `Router::dispatch`'s exhaustion fallback (`src/routing/router.rs:110-114`) in place of the previously-reused `ProviderError::Upstream{status: 503, ..}` shape, so "all candidates unhealthy/exhausted" is a distinct, matchable case from a genuine upstream-returned 503. | Added by Story 2.1.3, Task 2.1.3.1 — a real `src/providers/mod.rs` + `src/routing/router.rs` code change, not just an entrypoint-layer addition. |
| `SessionKey` (reused) | Existing type, `src/session_compaction/session_state.rs`, `SessionKey(pub String)`, `SessionKey::new(id: impl Into<String>) -> Self`. The entrypoint synthesizes one fresh `SessionKey` per inbound HTTP request (`format!("http:{}", Uuid::new_v4())`), not reused across requests. | See ADR-016. |
| `RequestId` (reused) | Existing type, `src/cost_metrics/types.rs`, `RequestId(pub Uuid)`, `RequestId::new() -> Self`. One synthesized per inbound HTTP request. | See ADR-016. |
| `CompactionTier::Off` (reused) | Existing enum variant, `src/session_compaction/tiered.rs`, doc-commented "Below the micro threshold: no compaction runs." Reused by the entrypoint to mean "no compaction tier applies; this is a live proxied HTTP request," a deliberate semantic reuse across two unrelated features. | See ADR-016. Flagged explicitly rather than silently relied upon. |
| `record_pending`-first sequencing (reused pattern) | The entrypoint must call `CostTracker::record_pending(&session_key, request_id, CompactionTier::Off).await` before calling `Router::dispatch`, because `record_actual_usage`/`record_request_failed` use the store's non-creating `get()` and will silently no-op if the session was never first created via `record_pending`'s creating `get_or_init()`. | See ADR-016. Load-bearing; enforced by one shared helper (Story 2.1.2) rather than duplicated per-handler. |
| `translate_and_record` (reused, newly-called) | Existing function, `src/providers/mod.rs:335-366`, doc-commented "has no caller in this codebase's live request path today." The OpenAI-compat non-streaming success path (Story 3.1.1) is its first real caller. | Closes an existing TODO. |
| `extract_usage` (reused) | Existing `pub(crate)` function, `src/providers/mod.rs:311`, `fn extract_usage(anthropic: &serde_json::Value) -> Option<(u64, u64)>` returning `(prompt_tokens, completion_tokens)`. Reused directly by `CostTrackingStream` rather than reimplementing usage extraction. | `pub(crate)` visibility confirmed accessible from `src/entrypoint/`. |
| `UpstreamKind::Openai` gap | Existing config schema variant (`src/config/schema.rs`) with **no corresponding `Provider` implementation** anywhere in `src/providers/` (only `anthropic.rs`, `bedrock.rs` exist). `Router::from_config` must `anyhow::bail!` with a named error if a config references this kind, rather than silently skipping it or panicking. | Newly-identified gap this project surfaces; fixed by an explicit bail, not by implementing an OpenAI-upstream `Provider` (out of scope). |
| Loopback-only bind (NFR) | `serve_entrypoint` binds `SocketAddr::from(([127, 0, 0, 1], port))` — a hardcoded array literal, never parsed from a configurable bind-address string. `Config` has no such field. This makes "no remote-bind configurability" true by construction, not by convention. | Verified by an integration test (Story 1.4.2) that asserts a connection from a non-loopback-simulated source is impossible by construction (the socket simply never listens on any other interface). |

## Pattern Decisions

| Component | Pattern Chosen | Source | Alternative Rejected | Reason |
|---|---|---|---|---|
| Overall entrypoint module structure | Thin adapter module (`src/entrypoint/`) mirroring `src/cost_metrics/server.rs`'s State/router-builder/`serve()` shape | This codebase's own existing convention (not an external text) | Folding logic into `src/main.rs` directly | Violates this repo's architecture note ("keep transport code thin... put real logic where it's independently testable"); handlers folded into `main.rs` are unreachable by `cargo test`'s unit tree without spawning the whole binary. |
| `Router::from_config` | Factory Method | GoF | A `RouterBuilder` type with chained `.with_upstream(...)` calls | Over-engineered for a function called exactly once (at startup, from `Command::Run`) with a single, fully-known input (`&Config`); a plain associated function is simpler and just as testable. |
| OpenAI streaming translation | Adapter | GoF | Strategy (pluggable translation "policies" selected at runtime) | There is exactly one direction to translate (Anthropic SSE → OpenAI SSE) for this feature's scope; a `Strategy` implies multiple interchangeable algorithms, which does not exist here and would add an unused trait indirection. |
| Error-envelope mapping | Two paired free functions (`map_provider_error_anthropic`, `map_provider_error_openai`) | Function-based mapping, not a formal GoF pattern | A single `impl IntoResponse for ProviderError` | `ProviderError` needs two *different* envelope shapes depending on which endpoint is handling it — a single `IntoResponse` impl cannot know which wire format the caller wants without an extra parameter, which defeats the point of the trait; two named functions called explicitly from each handler are clearer. |
| Cost-tracking mid-stream tee | Decorator | GoF | A separate consumer task fed by a broadcast channel from the main stream | A `Stream`-wrapping Decorator keeps the tee in the same task as the response body being written (no extra task, no channel backpressure/lag to reason about) and composes directly with `Body::from_stream`; a separate task would need its own error/shutdown handling for no benefit since the tee only reads, never blocks the passthrough. |
| `EntrypointState` | Facade-like state bundle (plain struct of `Arc`s) | PoEAA (Registry-adjacent; effectively a small Facade over the two collaborators a handler needs) | Separate axum `State<Arc<DispatchRouter>>` and `State<Arc<CostTracker>>` extractors on every handler | axum supports multiple `State` extractors via `FromRef`, but every handler in this module needs both collaborators together; one bundled struct is simpler to construct once at startup and pass to `entrypoint_router`, matching `CostServerState`'s existing precedent. |
| `CompactionTier::Off` reuse for HTTP requests | Type-driven reuse of an existing variant, documented explicitly (ADR-016) | Type-driven design (reuse over premature widening) | A new `RequestKind` field/variant added to `CostRecord`/`CompactionTier` | Widens a schema shared with the already-shipped session-compaction feature for the sole benefit of one new caller; `Off` already carries the correct meaning without a schema change, so the simpler reuse is chosen and the reuse itself is called out explicitly (glossary + ADR-016) so it cannot be mistaken for an oversight. |

### Technology Validation

No new Cargo dependency is required. `axum 0.8`, `axum-extra 0.10`, `tower-http 0.7` (`trace`, `timeout` features), `eventsource-stream 0.2`, and `tokio 1` (`full`) are already present in `Cargo.toml` and already exercised by `src/cost_metrics/server.rs`'s existing test suite. The one dependency with a pre-1.0 version number, `eventsource-stream 0.2`, is an **inherited** risk (already a dependency before this project began) rather than a new one introduced by this plan — noted here per Step 3's validation requirement, not re-litigated as a new choice.

## Migration Plan

Not applicable. This feature adds new capability (a previously-stubbed `Command::Run` becomes a real server) rather than migrating existing data, schema, or running infrastructure. No backward-compatibility shim is needed since `Command::Run` currently does nothing but print a summary line and exit.

## Observability Plan

- **Logs**: `tracing::info!(%addr, "consolette http-entrypoint listening")` on bind (mirroring `serve_cost`'s existing log line); `tracing::info!` per request in `Router::from_config`'s selected-upstream path (already logged inside `Router::dispatch`, confirmed pre-existing); `tracing::warn!` when `Router::from_config` finds more than one `Route` in `config.routes` and uses only the first (Story 1.1.2); `tracing::error!` on any `ProviderError` before it is mapped to an HTTP error response (Story 2.1.3/3.1.2); `tower_http::trace::TraceLayer::new_for_http()` applied to the whole `entrypoint_router` for per-request method/path/status/latency spans (Story 1.4.2).
- **Metrics**: none new for this feature — `CostTracker`'s existing reporting (`report_for_session`, already exposed via `serve-cost`'s `/v1/cost/{session_key}`) is the only metrics surface, and every entrypoint request already feeds it via `record_pending`/`record_actual_usage`/`record_request_failed` (ADR-016). No new metrics endpoint is in scope.
- **Alerts**: none — this is a local, single-user, loopback-only process; there is no fleet-level alerting surface for a per-developer-machine binary.

## Risk Control

- **Feature flag**: none, per requirements.md's explicit constraint — this ships directly as the new behavior of `Command::Run`.
- **Rollback procedure**: `git revert` the merge commit; `Command::Run` returns to its previous stub behavior (print config summary, exit) with no persistent state to clean up, since the entrypoint holds no on-disk state of its own beyond what `Config`/`CostTracker` already manage.
- **Staged rollout**: none — single local binary, no fleet, no canary population. Verification is `cargo test` (including the new integration-style tests against the actually-bound server) passing locally and in CI before merge, per requirements.md's Success Metrics.

## Unresolved Questions

- [ ] Should `Router::from_config`'s "more than one `Route` configured" case (Story 1.1.2) log a warning and use the first route, or `anyhow::bail!`? This plan chooses **warn and use the first** (least-surprising for a user who added a second route for future use), but this is a judgment call the feature owner (Tyler) should confirm before Phase 1 ships, since requirements.md states "exactly one default `Route`" is the expected shape without specifying what to do if config declares more.
- [ ] The exact OpenAI error `type`/`code` string values for non-4xx-standard cases (e.g. total-upstream-outage) are inferred from `ux.md`'s general guidance ("503/429-with-retry, convention varies by cause") rather than pinned to one exact string — Story 3.1.2's task list should be reviewed against a real OpenAI client SDK's actual exception-mapping table before merge, owner: whoever implements Phase 3.

## Dependency Visualization

```
Phase 1: Router::from_config + EntrypointState + Command::Run wiring
   |
   |-- Epic 1.1 Router::from_config ------------------\
   |-- Epic 1.2 EntrypointState/serve_entrypoint       |--> required by both Phase 2 and Phase 3
   |-- Epic 1.3 Wire Command::Run                      |
   |-- Epic 1.4 Observability + loopback bind test ----/
   |
   v
Phase 2: Anthropic-native /v1/messages          Phase 3: OpenAI-compatible /v1/chat/completions
   |-- Epic 2.1 non-streaming                        |-- Epic 3.1 non-streaming
   |-- Epic 2.2 streaming (CostTrackingStream,        |-- Epic 3.2 streaming (OpenAiStreamTranslator,
   |            Body::from_stream, error framing)     |            reuses CostTrackingStream + error framing)
   |                                                   |
   \-------------------------- both feed ---------------------------/
                                   |
                                   v
                  Phase 4: Integration tests & hardening
                     |-- Epic 4.1 bound-server end-to-end tests
                     |-- Epic 4.2 three-way cost-tracking coverage
                     \-- Epic 4.3 final gate (fmt/clippy/test)
```

---

## Phase 1: Router::from_config + EntrypointState + Command::Run wiring

### Epic 1.1: `Router::from_config`

**Goal**: Give `Router` a first-class, fully-tested `from_config(&Config) -> anyhow::Result<Router>` associated function that assembles every collaborator (`UpstreamRef` candidates, `Provider` instances, `RoutingStrategy`, `HealthRegistry`, `AdmissionControl`) from a loaded `Config`, handling the Anthropic/Bedrock constructor asymmetry and the `UpstreamKind::Openai` gap explicitly.

#### Story 1.1.1: Build per-upstream `Provider` instances and candidate list

**As a** developer running `consolette run`,
**I want** every `Upstream` in my config turned into a live `Provider` (or a clear startup error if it can't be),
**so that** dispatch has real providers to call instead of a stub.

**Acceptance Criteria**:
- Given a `Config` whose `upstreams` contains one `Upstream{name:"anthropic", kind:UpstreamKind::Anthropic, ...}`, when `Router::from_config(&config)` runs, then it constructs an `AnthropicProvider` via `AnthropicProvider::new(Arc::new(upstream.clone()), resolver, exec_cache, config.request_timeout)` and returns `Ok(Router)` containing it at the same index as the upstream in `config.upstreams`.
  - Example: `config.upstreams[0].name == "anthropic"` (as in `Config::default()`) produces `providers[0].name() == "anthropic"`.
- Given a `Config` whose `upstreams` contains one `Upstream{name:"bedrock", kind:UpstreamKind::Bedrock{..}, ...}`, when `Router::from_config(&config)` runs, then it constructs a `BedrockProvider` via `BedrockProvider::new(Arc::new(upstream.clone())).await` (async, infallible) at the matching index.
  - Example: `config.upstreams[1].name == "bedrock"` (as in `Config::default()`) produces `providers[1].name() == "bedrock"`.
- Given a `Config` whose `upstreams` contains an `Upstream{kind: UpstreamKind::Openai{..}, ..}`, when `Router::from_config(&config)` runs, then it returns `Err` whose message contains the upstream's name and the literal substring `"has no Provider implementation"`.
  - Example: `Upstream{name:"my-openai-upstream", kind:UpstreamKind::Openai{base_url:"https://example.invalid".into()}, auth:None}` produces `Err(e)` where `e.to_string() == "upstream \"my-openai-upstream\": UpstreamKind::Openai has no Provider implementation yet"`.
- Given an `Upstream` whose `kind` is `UpstreamKind::Bedrock`, when `Router::from_config` builds its candidate `UpstreamRef`, then it calls `health.set_can_cooldown(idx, false)` for that index before returning, per ADR-003's "Bedrock never cools down" rule (`src/routing/health.rs` module doc).
  - Example: for `Config::default()`, index `1` (the `"bedrock"` upstream) has `health.set_can_cooldown` called with `(1, false)`; index `0` (`"anthropic"`) does not.

**Files**: `src/routing/router.rs`

##### Task 1.1.1.1 (~4 min): Add `Router::from_config` skeleton and `SystemSecretResolver`/`ExecCredentialCache` construction
- Add `impl Router { pub fn from_config(config: &Config) -> anyhow::Result<Router> { ... } }` (sync signature; async only where a constructor requires it — see Task 1.1.1.3) to `src/routing/router.rs`.
- Inside, construct `let resolver: Arc<dyn SecretResolver + Send + Sync> = Arc::new(SystemSecretResolver);` and `let exec_cache = Arc::new(ExecCredentialCache::new());` once, shared across all `Anthropic`-kind upstreams.
- Add necessary `use` statements (`crate::auth::{SecretResolver, SystemSecretResolver}`, `crate::auth::exec::ExecCredentialCache`, `crate::providers::{anthropic::AnthropicProvider, bedrock::BedrockProvider}`).
- Files: `src/routing/router.rs`

##### Task 1.1.1.2 (~5 min): Loop over `config.upstreams`, construct `AnthropicProvider` per Anthropic-kind entry
- For each `(idx, upstream)` in `config.upstreams.iter().enumerate()`, match on `upstream.kind`; for `UpstreamKind::Anthropic`, call `AnthropicProvider::new(Arc::new(upstream.clone()), Arc::clone(&resolver), Arc::clone(&exec_cache), config.request_timeout)?` and push `Arc::new(provider) as Arc<dyn Provider>` into a `Vec<Arc<dyn Provider>>` at the matching index.
- Files: `src/routing/router.rs`

##### Task 1.1.1.3 (~4 min): Make `from_config` async; construct `BedrockProvider` per Bedrock-kind entry
- Change `Router::from_config` to `pub async fn from_config(config: &Config) -> anyhow::Result<Router>` (required since `BedrockProvider::new` is async).
- For `UpstreamKind::Bedrock { .. }`, call `BedrockProvider::new(Arc::new(upstream.clone())).await` (infallible) and push it at the matching index.
- Files: `src/routing/router.rs`

##### Task 1.1.1.4 (~3 min): Bail explicitly on `UpstreamKind::Openai`
- For `UpstreamKind::Openai { .. }`, return `anyhow::bail!("upstream \"{}\": UpstreamKind::Openai has no Provider implementation yet", upstream.name)`.
- Files: `src/routing/router.rs`

##### Task 1.1.1.5 (~4 min): Build `UpstreamRef` candidates and wire `HealthRegistry::set_can_cooldown`
- Construct `let health = Arc::new(HealthRegistry::new(config.cooldown_seconds));`.
- For each Bedrock-kind index, call `health.set_can_cooldown(idx, false);` immediately after health is constructed and before `Router::new` is called.
- Files: `src/routing/router.rs`

---

#### Story 1.1.2: Resolve the default `Route`, `RoutingStrategy`, and `AdmissionControl`, and construct the `Router`

**As a** developer running `consolette run`,
**I want** `Router::from_config` to pick the configured route/strategy and finish assembling a working `Router`,
**so that** `serve_entrypoint` has a real, dispatch-ready `Router` to hand to `EntrypointState`.

**Acceptance Criteria**:
- Given a `Config` with an empty `routes` vec, when `Router::from_config(&config)` runs, then it returns `Err` whose message contains `"no routes configured"`.
  - Example: `Config { routes: vec![], ..Config::default() }` produces an `Err` (not a panic).
- Given a `Config` with more than one entry in `routes`, when `Router::from_config(&config)` runs, then it logs `tracing::warn!` naming the ignored routes and proceeds using `routes[0]`.
  - Example: `Config { routes: vec![route_a, route_b], .. }` uses `route_a` and emits one `tracing::warn!` mentioning `route_b.name`.
- Given `Config::default()` (one route `"default"`, `Strategy::Fallback`, referencing `"anthropic"` then `"bedrock"`), when `Router::from_config(&config)` runs, then the returned `Router`'s candidates are `[UpstreamRef{index:0,name:"anthropic",weight:1.0}, UpstreamRef{index:1,name:"bedrock",weight:1.0}]` (weight defaulted since `RouteUpstreamRef.weight` is `None`) and its strategy is a `FallbackStrategy`.
  - Example: dispatching with these candidates and both providers healthy selects `"anthropic"` first, per `FallbackStrategy::select`'s existing (already-tested) behavior.
- Given `Strategy::Weighted` on the selected route, when `Router::from_config` builds the strategy, then it constructs `Arc::new(WeightedStrategy)` (unit struct, no arguments) rather than `Arc::new(FallbackStrategy)`.
  - Example: `Route { strategy: Strategy::Weighted, .. }` produces a `Router` whose `strategy` field's `select` behavior matches `WeightedStrategy`'s existing (already-tested) weighted-selection logic, not `FallbackStrategy`'s first-healthy behavior.

**Files**: `src/routing/router.rs`

##### Task 1.1.2.1 (~3 min): Select the route (bail if empty, warn if multiple)
- After the provider-construction loop (Story 1.1.1), add: `let route = config.routes.first().ok_or_else(|| anyhow::anyhow!("no routes configured"))?;` and, if `config.routes.len() > 1`, `tracing::warn!(ignored = ?config.routes[1..].iter().map(|r| &r.name).collect::<Vec<_>>(), "multiple routes configured; using the first");`.
- Files: `src/routing/router.rs`

##### Task 1.1.2.2 (~4 min): Resolve `route.upstreams` (`Vec<RouteUpstreamRef>`) into `Vec<UpstreamRef>` candidates
- For each `RouteUpstreamRef { name, weight }` in `route.upstreams`, find its index in `config.upstreams` by matching `name` (bail with a clear message, e.g. `"route \"{}\" references unknown upstream \"{}\""`, if not found), and build `UpstreamRef { index, name: name.clone(), weight: weight.unwrap_or(1.0) }`.
- Files: `src/routing/router.rs`

##### Task 1.1.2.3 (~3 min): Map `route.strategy` (`Strategy` enum) to a `RoutingStrategy` impl
- `match route.strategy { Strategy::Fallback => Arc::new(FallbackStrategy) as Arc<dyn RoutingStrategy>, Strategy::Weighted => Arc::new(WeightedStrategy) as Arc<dyn RoutingStrategy> }`.
- Files: `src/routing/router.rs`

##### Task 1.1.2.4 (~3 min): Construct `AdmissionControl` from `config.ratelimit` and call `Router::new`
- `let admission = Arc::new(RateLimiters::new(&config.ratelimit)) as Arc<dyn AdmissionControl>;`.
- Return `Ok(Router::new(candidates, providers, strategy, health, admission))`.
- Files: `src/routing/router.rs`

##### Task 1.1.2.5 (~5 min): Unit tests for `Router::from_config`
- Add `#[tokio::test]` functions covering: `Config::default()` succeeds and produces 2 candidates in order; an `Openai`-kind upstream produces the expected `Err` message substring; an empty `routes` vec produces the `"no routes configured"` `Err`; a multi-route config uses the first route (assert via the resulting candidate list matching route 0's upstreams, not route 1's).
- Files: `src/routing/router.rs` (`#[cfg(test)] mod tests`)

---

### Epic 1.2: `EntrypointState`, `entrypoint_router`, `serve_entrypoint`

**Goal**: New `src/entrypoint/mod.rs` providing the state bundle, router builder, and bind/serve function, modeled directly on `src/cost_metrics/server.rs`'s `CostServerState`/`cost_router`/`serve_cost` shape.

#### Story 1.2.1: `EntrypointState` and its constructor

**As a** developer running `consolette run`,
**I want** one small struct holding the dispatch router and cost tracker,
**so that** every handler can reach both without threading two separate `State` extractors everywhere.

**Acceptance Criteria**:
- Given a loaded `Config`, when `EntrypointState::build(config: &Config).await` runs, then it returns `Ok(EntrypointState { dispatch_router: Arc<DispatchRouter>, cost_tracker: Arc<CostTracker> })` where `dispatch_router` was built via `Router::from_config(config).await?` and `cost_tracker` was built via `CostTracker::new(PricingTable::load_default()).await`.
  - Example: `EntrypointState::build(&Config::default()).await` succeeds (since `Config::default()`'s upstreams are both constructible) and yields a state whose `cost_tracker.report_for_session(&SessionKey::new("nonexistent"))` returns the tracker's existing not-found error (proving it's a live, queryable tracker).
- Given `EntrypointState`, when cloned, then the clone shares the same underlying `Router`/`CostTracker` instances (via `Arc::clone`, not deep copies).
  - Example: two clones of one `EntrypointState`, each calling `cost_tracker.record_pending(...)` for the same `SessionKey`, both observe the same session entry in `report_for_session`.

**Files**: `src/entrypoint/mod.rs`

##### Task 1.2.1.1 (~3 min): Define `EntrypointState`
- `#[derive(Clone)] pub struct EntrypointState { pub dispatch_router: Arc<DispatchRouter>, pub cost_tracker: Arc<CostTracker> }` with `use crate::routing::router::Router as DispatchRouter;` and `use crate::cost_metrics::tracker::CostTracker;`.
- Files: `src/entrypoint/mod.rs`

##### Task 1.2.1.2 (~4 min): Implement `EntrypointState::build`
- `pub async fn build(config: &Config) -> anyhow::Result<Self> { let dispatch_router = Arc::new(DispatchRouter::from_config(config).await?); let cost_tracker = Arc::new(CostTracker::new(PricingTable::load_default()).await); Ok(Self { dispatch_router, cost_tracker }) }`.
- Files: `src/entrypoint/mod.rs`

##### Task 1.2.1.3 (~3 min): Unit test for `EntrypointState::build`
- `#[tokio::test]` asserting `EntrypointState::build(&Config::default()).await.is_ok()`.
- Files: `src/entrypoint/mod.rs` (`#[cfg(test)] mod tests`)

---

#### Story 1.2.2: `entrypoint_router` and `serve_entrypoint`

**As a** developer running `consolette run`,
**I want** a function that builds the axum `Router` and one that binds+serves it on loopback,
**so that** `Command::Run` has a single call to make.

**Acceptance Criteria**:
- Given an `EntrypointState`, when `entrypoint_router(state)` runs, then it returns an `axum::Router` with routes `POST /v1/messages` and `POST /v1/chat/completions` registered and `tower_http::trace::TraceLayer::new_for_http()` applied as a layer.
  - Example: a `axum::body::Body` request built via `Request::post("/v1/messages")` against the router (using `tower::ServiceExt::oneshot` in a test) reaches `post_v1_messages`, not a 404.
- Given a `port` and an `EntrypointState`, when `serve_entrypoint(port, state).await` runs, then it binds `SocketAddr::from(([127, 0, 0, 1], port))`, logs `tracing::info!(%addr, "consolette http-entrypoint listening")`, and serves until `shutdown_signal()` resolves.
  - Example: calling `serve_entrypoint(0, state)` (port 0 requests an OS-assigned ephemeral port in tests, matching `cost_metrics/server.rs`'s existing test convention) followed by an immediate SIGINT-simulated shutdown returns `Ok(())` rather than hanging.

**Files**: `src/entrypoint/mod.rs`

##### Task 1.2.2.1 (~4 min): Implement `entrypoint_router`
- `pub fn entrypoint_router(state: EntrypointState) -> axum::Router { axum::Router::new().route("/v1/messages", axum::routing::post(crate::entrypoint::messages::post_v1_messages)).route("/v1/chat/completions", axum::routing::post(crate::entrypoint::chat_completions::post_v1_chat_completions)).layer(tower_http::trace::TraceLayer::new_for_http()).with_state(state) }`.
- Files: `src/entrypoint/mod.rs`

##### Task 1.2.2.2 (~5 min): Implement `serve_entrypoint`
- `pub async fn serve_entrypoint(port: u16, state: EntrypointState) -> anyhow::Result<()> { let addr: SocketAddr = ([127, 0, 0, 1], port).into(); let listener = tokio::net::TcpListener::bind(addr).await?; tracing::info!(%addr, "consolette http-entrypoint listening"); let router = entrypoint_router(state); axum::serve(listener, router).with_graceful_shutdown(shutdown_signal()).await?; Ok(()) }` — signature and body modeled directly on `serve_cost` (`src/cost_metrics/server.rs:218-231`).
- Files: `src/entrypoint/mod.rs`

##### Task 1.2.2.3 (~3 min): Router-wiring unit test
- `#[tokio::test]` building `entrypoint_router` over a test `EntrypointState` and asserting (via `tower::ServiceExt::oneshot`) that `POST /v1/messages` and `POST /v1/chat/completions` do not 404 (a 4xx from validation is acceptable at this stage; a 404 is not).
- Files: `src/entrypoint/mod.rs` (`#[cfg(test)] mod tests`)

---

#### Story 1.2.3: Graceful shutdown

**As a** developer running `consolette run` in a terminal,
**I want** Ctrl-C or SIGTERM to stop the server cleanly,
**so that** I can stop the process the same way I'd stop any other local dev server.

**Acceptance Criteria**:
- Given a running `serve_entrypoint` future, when SIGINT (Ctrl-C) is delivered, then `shutdown_signal()` resolves and `axum::serve(...).with_graceful_shutdown(...)` completes, returning `Ok(())` from `serve_entrypoint`.
  - Example: in a test, spawning `serve_entrypoint` in a `tokio::spawn`, then invoking the test-only shutdown trigger (see Task 1.2.3.2) causes the spawned task to complete within a bounded timeout (e.g. `tokio::time::timeout(Duration::from_secs(2), handle)`).
- Given a running `serve_entrypoint` future, when SIGTERM is delivered, then `shutdown_signal()` also resolves (both signals are treated identically).
  - Example: `tokio::signal::unix::signal(SignalKind::terminate())` firing has the same observable effect as `tokio::signal::ctrl_c()` firing.

**Files**: `src/entrypoint/mod.rs`

##### Task 1.2.3.1 (~4 min): Implement `shutdown_signal`
- `async fn shutdown_signal() { let ctrl_c = async { tokio::signal::ctrl_c().await.expect("install Ctrl+C handler"); }; #[cfg(unix)] let terminate = async { tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).expect("install SIGTERM handler").recv().await; }; #[cfg(not(unix))] let terminate = std::future::pending::<()>(); tokio::select! { () = ctrl_c => {}, () = terminate => {} } }`.
- Files: `src/entrypoint/mod.rs`

##### Task 1.2.3.2 (~4 min): Test-only shutdown trigger for the graceful-shutdown test
- For testability without sending real OS signals, extract a `serve_entrypoint_with_shutdown(port, state, shutdown: impl Future<Output=()> + Send + 'static)` (production `serve_entrypoint` calls it with `shutdown_signal()`; the test calls it with a `tokio::sync::oneshot::Receiver` future it controls directly).
- Files: `src/entrypoint/mod.rs`

##### Task 1.2.3.3 (~3 min): Graceful-shutdown unit test
- `#[tokio::test]`: spawn `serve_entrypoint_with_shutdown(0, state, rx_future)`, send on the paired `oneshot::Sender`, assert the spawned task completes within `tokio::time::timeout(Duration::from_secs(2), ...)`.
- Files: `src/entrypoint/mod.rs` (`#[cfg(test)] mod tests`)

---

### Epic 1.3: Wire `Command::Run`

**Goal**: Replace the current `run()` stub in `src/main.rs` with a real call into `serve_entrypoint`, and register the new module in `src/lib.rs`.

#### Story 1.3.1: `main.rs` and `lib.rs` wiring

**As a** developer,
**I want** `consolette run` to actually bind and serve traffic,
**so that** the feature is reachable from the CLI, not just from tests.

**Acceptance Criteria**:
- Given a valid `~/.config/consolette` config directory, when a user runs `consolette run`, then the process loads config via `config::load(&config_dir())?`, builds an `EntrypointState`, and calls `serve_entrypoint(config.port, state).await`, blocking until shutdown.
  - Example: running the binary with `CLAUDE_CODE_OAUTH_TOKEN` set (satisfying `Config::default()`'s default `anthropic` upstream's auth) and no config file present binds port `47000` and logs the `"consolette http-entrypoint listening"` line.
- Given `src/lib.rs`'s existing alphabetical `pub mod` list, when the new module is added, then `pub mod entrypoint;` appears between `pub mod dashboard;` and `pub mod learn;`, preserving alphabetical order.
  - Example: `grep -n "^pub mod" src/lib.rs` shows `dashboard`, `entrypoint`, `learn` in that order.

**Files**: `src/main.rs`, `src/lib.rs`

##### Task 1.3.1.1 (~2 min): Insert `pub mod entrypoint;` in `src/lib.rs`
- Add `pub mod entrypoint;` alphabetically between the existing `pub mod dashboard;` and `pub mod learn;` lines.
- Files: `src/lib.rs`

##### Task 1.3.1.2 (~4 min): Rewrite `run()` in `src/main.rs`
- Change `fn run() -> anyhow::Result<()>` to `async fn run() -> anyhow::Result<()>`, keep the existing `config::load` call and summary `println!`, then add `let state = consolette::entrypoint::EntrypointState::build(&config).await?; consolette::entrypoint::serve_entrypoint(config.port, state).await`.
- Update the match arm `Command::Run => run(),` to `Command::Run => run().await,`.
- Files: `src/main.rs`

##### Task 1.3.1.3 (~3 min): `#![allow(dead_code)]` cleanup on `src/auth/mod.rs`
- Remove the module-level `#![allow(dead_code)]` and its doc comment ("Nothing outside tests calls this yet...") from `src/auth/mod.rs`, now that `Router::from_config` (Task 1.1.1.1) is a real, non-test caller of `SecretResolver`/`SystemSecretResolver`.
- Run `cargo check` to confirm no other dead-code warnings surface from this removal.
- Files: `src/auth/mod.rs`

---

### Epic 1.4: Observability wiring and loopback-bind verification

**Goal**: Confirm (via a real test, not inspection) that the server only ever listens on loopback, and that request tracing is wired.

#### Story 1.4.1: Route-selection tracing in `Router::from_config`/`dispatch`

**As an** operator reading `consolette run`'s logs,
**I want** to see which route/strategy was selected at startup,
**so that** I can confirm my config was interpreted the way I expected.

**Acceptance Criteria**:
- Given `Router::from_config` selects `route.name == "default"` with `Strategy::Fallback`, when it returns, then it has logged `tracing::info!(route = "default", strategy = "fallback", candidates = 2, "router assembled from config")` (or equivalent structured fields) before returning `Ok`.
  - Example: running with `RUST_LOG=consolette=info` and `Config::default()` prints a line containing `route="default"` and `candidates=2`.

**Files**: `src/routing/router.rs`

##### Task 1.4.1.1 (~3 min): Add the startup-summary `tracing::info!` call
- Immediately before `Router::from_config`'s final `Ok(Router::new(...))`, add the structured `tracing::info!` call summarizing route name, strategy, and candidate count.
- Files: `src/routing/router.rs`

---

#### Story 1.4.2: Loopback-only bind test and `TraceLayer`

**As a** security-conscious user,
**I want** proof the server never listens on a non-loopback interface,
**so that** I can trust the "no remote-bind configurability" claim without reading the source myself.

**Acceptance Criteria**:
- Given `serve_entrypoint(0, state)` is spawned in a test, when the test inspects `listener.local_addr()` (captured before the listener is moved into `axum::serve`), then the address's IP is exactly `127.0.0.1` regardless of `port`.
  - Example: `local_addr.ip() == std::net::Ipv4Addr::LOCALHOST` for `port == 0` and, separately, for `port == 47000`.
- Given `entrypoint_router`, when built, then it has a `TraceLayer` applied, verified by asserting a request produces a `tracing`-captured span (using `tracing_test` conventions already absent from this repo — instead, assert indirectly via response headers/behavior unaffected, and treat the `TraceLayer::new_for_http()` call itself, present in Task 1.2.2.1, as the acceptance mechanism, confirmed by code review rather than a runtime assertion, since asserting on `tracing` output requires a subscriber-capturing harness this repo does not yet have).
  - Example: `entrypoint_router` compiles and the constructed `axum::Router`'s `.layer(...)` chain includes `TraceLayer::new_for_http()` per Task 1.2.2.1's source.

**Files**: `src/entrypoint/mod.rs`

##### Task 1.4.2.1 (~4 min): Loopback-bind integration test
- Add a test that calls `tokio::net::TcpListener::bind(([127,0,0,1], 0)).await` directly (mirroring `serve_entrypoint`'s own binding line) to prove the bind succeeds and `local_addr().ip()` is loopback, then separately spins up `serve_entrypoint_with_shutdown` on port `0` via `tokio::spawn` and confirms a plain TCP connect to `127.0.0.1:<assigned-port>` succeeds (using a raw `tokio::net::TcpStream::connect`), proving the real server socket is reachable only via loopback addressing.
- Files: `src/entrypoint/mod.rs` (`#[cfg(test)] mod tests`)

---

## Phase 2: Anthropic-native `POST /v1/messages`

### Epic 2.1: Non-streaming path

**Goal**: A working, cost-tracked, error-mapped non-streaming `POST /v1/messages` handler.

#### Story 2.1.1: `post_v1_messages` handler — happy path dispatch

**As a** Claude Code user pointing at `http://127.0.0.1:47000`,
**I want** `POST /v1/messages` with `"stream": false` to return a real Anthropic-shaped response,
**so that** my existing Anthropic-compatible client works unmodified.

**Acceptance Criteria**:
- Given a request body `{"model":"claude-sonnet-4-5","max_tokens":100,"messages":[{"role":"user","content":"hi"}],"stream":false}` and headers including `content-type: application/json`, when `post_v1_messages` runs, then it calls `state.dispatch_router.dispatch(body, headers, false, est_tokens)` (with `est_tokens` computed via a simple heuristic, e.g. body-length-based, since no tokenizer dependency is in scope) and, on `Ok(ProviderResponse::Full(json))`, returns `(StatusCode::OK, Json(json))`.
  - Example: a `Router` wired to a `Provider` test double returning `ProviderResponse::Full(serde_json::json!({"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"hello"}]}))` causes `post_v1_messages` to return HTTP 200 with that exact JSON body.

**Files**: `src/entrypoint/messages.rs`

##### Task 2.1.1.1 (~4 min): Define `post_v1_messages` signature and happy-path branch
- `pub async fn post_v1_messages(State(state): State<EntrypointState>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response`. Extract `let stream = body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);`.
- Files: `src/entrypoint/messages.rs`

##### Task 2.1.1.2 (~3 min): Estimate `est_tokens` and call dispatch (non-streaming branch only for now)
- Add a small local helper `fn estimate_tokens(body: &serde_json::Value) -> u32` (e.g. `body.to_string().len() as u32 / 4`, a rough char-count heuristic — documented as a heuristic, not a real tokenizer) and call it before dispatch.
- Files: `src/entrypoint/messages.rs`

---

#### Story 2.1.2: Cost tracking on the non-streaming path (all three outcomes)

**As the** feature owner,
**I want** every `/v1/messages` request to be recorded in `CostTracker` regardless of outcome,
**so that** cost reporting is never silently incomplete.

**Acceptance Criteria**:
- Given any inbound request, when `post_v1_messages` begins handling it, then it synthesizes `session_key = SessionKey::new(format!("http:{}", Uuid::new_v4()))` and `request_id = RequestId::new()`, and calls `state.cost_tracker.record_pending(&session_key, request_id, CompactionTier::Off).await` **before** calling `dispatch` (per ADR-016).
  - Example: after a request completes (success or failure), `state.cost_tracker.report_for_session(&session_key).await` returns `Ok` (proves the session was created), never `Err(SessionNotFound)`.
- Given dispatch returns `Ok(ProviderResponse::Full(json))`, when the handler proceeds, then it calls `record_actual_usage_from_anthropic_response(&state.cost_tracker, &session_key, request_id, model, &json).await`.
  - Example: a `json` body containing `"usage": {"input_tokens": 10, "output_tokens": 5}` results in `report_for_session(&session_key)` reflecting those token counts with `TokenSource::Exact`.
- Given dispatch returns `Err(provider_error)`, when the handler proceeds, then it calls `state.cost_tracker.record_request_failed(&session_key, request_id).await` before mapping the error to a response.
  - Example: a `ProviderError::Timeout` results in a call to `record_request_failed`, and `report_for_session` shows zero actual usage for that request (not a hang or a panic).

**Files**: `src/entrypoint/messages.rs`, `src/entrypoint/cost_tee.rs` (shared helper)

##### Task 2.1.2.1 (~4 min): Extract a shared `begin_cost_tracking` helper
- In `src/entrypoint/cost_tee.rs`, add `pub async fn begin_cost_tracking(tracker: &CostTracker) -> (SessionKey, RequestId) { let session_key = SessionKey::new(format!("http:{}", uuid::Uuid::new_v4())); let request_id = RequestId::new(); tracker.record_pending(&session_key, request_id, CompactionTier::Off).await; (session_key, request_id) }` — one shared call site so every handler (Stories 2.1.2, 2.2.1, 3.1.1, 3.2.1) gets the ADR-016 sequencing by construction.
- Files: `src/entrypoint/cost_tee.rs`

##### Task 2.1.2.2 (~4 min): Wire success/failure recording into `post_v1_messages`
- Call `begin_cost_tracking` before `dispatch`; on `Ok(ProviderResponse::Full(json))` call `record_actual_usage_from_anthropic_response`; on `Err(e)` call `state.cost_tracker.record_request_failed(&session_key, request_id).await` then proceed to error mapping (Story 2.1.3).
- Files: `src/entrypoint/messages.rs`

##### Task 2.1.2.3 (~4 min): Unit tests for both outcomes
- `#[tokio::test]`s covering: successful dispatch results in `report_for_session` showing `TokenSource::Exact` usage; failed dispatch results in `report_for_session` succeeding (session exists) with no usage recorded (using a `Provider` test double that returns `Err(ProviderError::Timeout)`).
- Files: `src/entrypoint/messages.rs` (`#[cfg(test)] mod tests`)

---

#### Story 2.1.3: Anthropic error envelope mapping

**As a** client of `/v1/messages` using the real Anthropic SDK,
**I want** errors shaped exactly like Anthropic's real API errors,
**so that** my SDK raises the correct typed exception instead of a generic parse failure.

**Acceptance Criteria**:
- Given `ProviderError::Validation(msg, 400)`, when `map_provider_error_anthropic` runs, then it returns `(StatusCode::BAD_REQUEST, json!({"type":"error","error":{"type":"invalid_request_error","message":msg}}))`.
  - Example: `ProviderError::Validation("messages: field required".into(), 400)` maps to HTTP 400 with `error.type == "invalid_request_error"` and `error.message == "messages: field required"`.
- Given `ProviderError::Auth(msg)`, when mapped, then it returns `(StatusCode::UNAUTHORIZED, json!({"type":"error","error":{"type":"authentication_error","message":msg}}))`.
  - Example: `ProviderError::Auth("invalid API key".into())` maps to HTTP 401, `error.type == "authentication_error"`.
- Given `ProviderError::RateLimited` or `RateLimitedWithRetry{retry_after}`, when mapped, then it returns HTTP 429 with `error.type == "rate_limit_error"`, and for `RateLimitedWithRetry`, the response additionally carries a `Retry-After: <retry_after>` header.
  - Example: `ProviderError::RateLimitedWithRetry{retry_after: 30}` produces HTTP 429 with header `retry-after: 30` and body `error.type == "rate_limit_error"`.
- Given `ProviderError::Timeout` or `ProviderError::Exhausted` (a new unit variant added by Task 2.1.3.1, returned by `Router::dispatch`'s fallback specifically when all candidates are unhealthy/exhausted — distinct from a genuine upstream-returned 503), when mapped, then it returns `(StatusCode::from_u16(529).unwrap(), json!({"type":"error","error":{"type":"overloaded_error","message":"..."}}))` per `ux.md`'s guidance that total-outage should read as `529 overloaded_error`, not a generic 500.
  - Example: `ProviderError::Exhausted` maps to HTTP 529, `error.type == "overloaded_error"`; `ProviderError::Timeout` maps the same way.
- Given `ProviderError::Upstream{status, body}` for an otherwise-unmapped status — including a genuine upstream-returned `503`, which stays a plain `Upstream{503, body}` rather than being conflated with exhausted failover now that `Router::dispatch` returns the dedicated `ProviderError::Exhausted` variant for the latter (Task 2.1.3.1) — when mapped, then it returns `(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), json!({"type":"error","error":{"type":"api_error","message":body}}))`.
  - Example: `ProviderError::Upstream{status:404, body:"model not found".into()}` maps to HTTP 404, `error.type == "api_error"`; `ProviderError::Upstream{status:503, body:"upstream returned 503".into()}` maps to HTTP 503 with `error.type == "api_error"` — correctly distinguishable from the 529/`overloaded_error` exhausted-failover case above because the two conditions are now different enum variants, not the same `Upstream{503,..}` shape.

**Files**: `src/entrypoint/errors.rs` (error mapping); `src/providers/mod.rs` and `src/routing/router.rs` (Task 2.1.3.1's new `ProviderError::Exhausted` variant and its `Router::dispatch` return site)

##### Task 2.1.3.1 (~5 min): Add `ProviderError::Exhausted` and return it from `Router::dispatch`'s exhaustion fallback
- `ProviderError` (`src/providers/mod.rs:32-46`) gains a new unit variant, e.g. `#[error("all upstream candidates exhausted")] Exhausted`, alongside the existing seven variants. `Router::dispatch`'s final fallback (`src/routing/router.rs:110-114`), currently `Err(last_error.unwrap_or(ProviderError::Upstream{status: 503, body: "no healthy upstreams available".to_string()}))`, changes to `Err(last_error.unwrap_or(ProviderError::Exhausted))` — this is the only return site that needs updating (confirmed by reading `Router::dispatch` in full: `last_error` is otherwise always a real `ProviderError` returned by a provider's `send`, never synthesized elsewhere). This makes "all candidates exhausted" distinguishable from a genuine upstream 503, which the two previously shared no way to tell apart.
- Files: `src/providers/mod.rs`, `src/routing/router.rs`

##### Task 2.1.3.2 (~5 min): Implement `map_provider_error_anthropic`
- Full `match` over all eight `ProviderError` variants per the acceptance criteria above, including the `ModelUnsupported(model)` variant mapping to `(StatusCode::NOT_FOUND, json!({"type":"error","error":{"type":"not_found_error","message":format!("model not found: {model}")}}))` and the new `Exhausted` variant mapping to `529`/`overloaded_error` alongside `Timeout`.
- Files: `src/entrypoint/errors.rs`

##### Task 2.1.3.3 (~3 min): Wire error mapping into `post_v1_messages`
- On `Err(e)` from dispatch (after `record_request_failed`, per Story 2.1.2), call `map_provider_error_anthropic(&e)` and build the `Response` (status + `Retry-After` header when applicable + JSON body).
- Files: `src/entrypoint/messages.rs`

##### Task 2.1.3.4 (~4 min): Unit tests for `map_provider_error_anthropic`
- One `#[test]` per `ProviderError` variant (including `Exhausted`) asserting exact status code and `error.type` string, per the acceptance criteria table above.
- Files: `src/entrypoint/errors.rs` (`#[cfg(test)] mod tests`)

---

### Epic 2.2: Streaming path

**Goal**: `POST /v1/messages` with `"stream": true` returns a real, byte-for-byte-passthrough SSE stream, cost-tracked mid-stream, with correct in-band error framing on a mid-stream cut.

#### Story 2.2.1: `CostTrackingStream` — mid-stream usage tee

**As the** feature owner,
**I want** streaming responses to still produce accurate cost records,
**so that** streaming and non-streaming requests are equally accounted for.

**Acceptance Criteria**:
- Given a raw SSE byte stream containing an `event: message_delta\ndata: {"usage":{"output_tokens":42}}\n\n` frame followed by `event: message_stop\ndata: {}\n\n`, when wrapped in `CostTrackingStream::new(inner, tracker, session_key, request_id, model)`, then every byte chunk from `inner` is yielded unchanged to the consumer, and once the `message_stop` frame is observed, `tracker.record_actual_usage` is called with the usage extracted via `extract_usage` applied to a reconstructed JSON view of the observed `message_delta`/`message_start` fields.
  - Example: consuming the wrapped stream to completion and then calling `tracker.report_for_session(&session_key)` shows `output_tokens == 42`.
- Given the inner stream ends (returns `None`) without ever yielding a `message_stop` frame (a mid-stream cut), when `CostTrackingStream` observes end-of-stream, then it calls `tracker.record_request_failed(&session_key, request_id).await` if no usage was ever extracted, or `record_actual_usage` with `TokenSource::Estimated` using the last-seen partial usage if any was observed before the cut.
  - Example: an inner stream that yields only `event: message_start\n...\n\n` then ends abruptly results in a call to `record_request_failed` (no `message_delta` was ever seen, so no partial usage exists).

**Files**: `src/entrypoint/cost_tee.rs`

##### Task 2.2.1.1 (~5 min): Define `CostTrackingStream` struct and `Stream` impl skeleton
- `pub struct CostTrackingStream<S> { inner: S, tracker: Arc<CostTracker>, session_key: SessionKey, request_id: RequestId, model: String, buf: Vec<u8>, last_usage: Option<(u64,u64)>, done: bool }` implementing `Stream<Item = Result<Bytes, anyhow::Error>>` by delegating `poll_next` to `inner` and appending yielded bytes to `buf` for frame scanning.
- Files: `src/entrypoint/cost_tee.rs`

##### Task 2.2.1.2 (~5 min): Frame-scan for `message_delta`/`message_stop` and extract usage
- On each poll that yields bytes, scan `buf` for complete `\n\n`-terminated SSE frames; for a frame whose `event:` line is `message_delta`, parse its `data:` line as JSON and call `extract_usage` on a `{"usage": ...}`-shaped wrapper, storing the result in `last_usage`; on a `message_stop` frame, call `tracker.record_actual_usage(...)` (or `record_request_failed` if `last_usage` is `None`) and set `done = true`.
- Files: `src/entrypoint/cost_tee.rs`

##### Task 2.2.1.3 (~4 min): Handle end-of-stream without `message_stop` (mid-stream cut)
- When `inner.poll_next` returns `Poll::Ready(None)` and `done` is still `false`, call `record_actual_usage` with `TokenSource::Estimated` (if `last_usage.is_some()`) or `record_request_failed` (otherwise) before propagating `None` to the consumer.
- Files: `src/entrypoint/cost_tee.rs`

##### Task 2.2.1.4 (~4 min): Unit tests for `CostTrackingStream`
- Tests covering: full stream with `message_stop` records exact usage; stream ending after `message_start` only (no `message_delta`) records failure; stream ending after `message_delta` but before `message_stop` records estimated usage. Use `futures::stream::iter` over pre-built `Bytes` chunks as the inner stream.
- Files: `src/entrypoint/cost_tee.rs` (`#[cfg(test)] mod tests`)

---

#### Story 2.2.2: Streaming handler wiring via `Body::from_stream`

**As a** Claude Code user with `"stream": true`,
**I want** to receive the same SSE bytes Anthropic's real API would send,
**so that** my client's SSE parser works unmodified.

**Acceptance Criteria**:
- Given `dispatch` returns `Ok(ProviderResponse::Stream(s))`, when `post_v1_messages` builds its response, then it wraps `s` in `CostTrackingStream::new(...)`, then in `axum::body::Body::from_stream(...)`, and returns a `Response` with status 200, `Content-Type: text/event-stream`, `Cache-Control: no-cache`, `Connection: keep-alive` (per ADR-017).
  - Example: a test `Provider` returning `ProviderResponse::Stream` over three pre-built SSE frames results in an HTTP response whose body, read to completion, equals the concatenation of those three frames byte-for-byte.

**Files**: `src/entrypoint/messages.rs`

##### Task 2.2.2.1 (~4 min): Branch `post_v1_messages` on `ProviderResponse::Stream`
- Extend the `match` from Task 2.1.1.1 to handle `Ok(ProviderResponse::Stream(s))`: wrap in `CostTrackingStream::new(s, Arc::clone(&state.cost_tracker), session_key, request_id, model)`.
- Files: `src/entrypoint/messages.rs`

##### Task 2.2.2.2 (~4 min): Build the SSE `Response` via `Body::from_stream`
- `Response::builder().status(StatusCode::OK).header(CONTENT_TYPE, "text/event-stream").header(CACHE_CONTROL, "no-cache").header(CONNECTION, "keep-alive").body(Body::from_stream(tee)).unwrap()`.
- Files: `src/entrypoint/messages.rs`

##### Task 2.2.2.3 (~4 min): Integration test for byte-for-byte passthrough
- Spin up `entrypoint_router` over a `tower::ServiceExt::oneshot` request (or a bound test server per Story 1.4.2's pattern) with a `Provider` test double streaming fixed SSE bytes, and assert the response body's bytes exactly match the input (minus no alteration expected).
- Files: `src/entrypoint/messages.rs` (`#[cfg(test)] mod tests`)

---

#### Story 2.2.3: Mid-stream error framing (ADR-018)

**As a** streaming client mid-response,
**I want** a clean in-band error signal if the upstream connection drops,
**so that** my client's stream loop terminates deliberately instead of hanging or retrying blindly.

**Acceptance Criteria**:
- Given the inner provider stream yields `Err(e)` partway through (after at least one successful frame), when the passthrough stream reaches that item, then it emits `event: error\ndata: {"type":"error","error":{"type":"api_error","message":"upstream stream interrupted"}}\n\n` as the final byte chunk and then ends the stream (yields `None` afterward), rather than propagating the `Err` to `Body::from_stream` (which would abort the HTTP response mid-body).
  - Example: an inner stream `[Ok(frame1), Err(anyhow!("connection reset"))]` results in a client-observed body of `frame1` followed by the literal `event: error\ndata: {"type":"error","error":{"type":"api_error","message":"upstream stream interrupted"}}\n\n`, with the HTTP response completing normally (not aborted).
- Given a mid-stream cut occurs, when it does, then no second call to `Router::dispatch` is made for the same client request (per ADR-018's point-of-no-return rule).
  - Example: a `Router`/`Provider` test double that counts `dispatch` invocations shows exactly 1 call total for a request whose stream is cut mid-way.

**Files**: `src/entrypoint/cost_tee.rs` (error-framing adapter co-located with the cost tee, since both wrap the same stream)

##### Task 2.2.3.1 (~5 min): Add error-to-frame conversion inside the `CostTrackingStream` poll loop
- When `inner.poll_next` yields `Poll::Ready(Some(Err(e)))`, log `tracing::warn!(error = %e, "upstream stream interrupted mid-response")`, synthesize the `event: error\n...\n\n` `Bytes` chunk as this poll's `Ok` output, set `done = true` (suppressing further polls of `inner`), and (per Task 2.2.1.3's logic) record cost with whatever `last_usage` was captured so far.
- Files: `src/entrypoint/cost_tee.rs`

##### Task 2.2.3.2 (~4 min): Unit test for mid-stream error framing
- Test with an inner stream `futures::stream::iter([Ok(frame1_bytes), Err(anyhow::anyhow!("connection reset"))])`; assert the wrapped stream yields `frame1_bytes` then the exact synthesized error frame, then ends.
- Files: `src/entrypoint/cost_tee.rs` (`#[cfg(test)] mod tests`)

---

## Phase 3: OpenAI-compatible `POST /v1/chat/completions`

### Epic 3.1: Non-streaming path

**Goal**: A working, cost-tracked, error-mapped non-streaming `POST /v1/chat/completions` handler, reusing the existing translation functions.

#### Story 3.1.1: `post_v1_chat_completions` handler — happy path via `translate_and_record`

**As a** developer using an OpenAI-compatible client library pointed at `http://127.0.0.1:47000/v1`,
**I want** `POST /v1/chat/completions` to accept OpenAI-shaped requests and return OpenAI-shaped responses,
**so that** my existing OpenAI client works unmodified against a non-OpenAI backend.

**Acceptance Criteria**:
- Given an OpenAI-shaped request body `{"model":"claude-sonnet-4-5","messages":[{"role":"user","content":"hi"}],"stream":false}`, when `post_v1_chat_completions` runs, then it calls `translate_openai_to_anthropic(&body)` to build the dispatch body, calls `state.dispatch_router.dispatch(anthropic_body, headers, false, est_tokens)`, and on `Ok(ProviderResponse::Full(anthropic_json))` calls `translate_and_record(&state.cost_tracker, &session_key, request_id, &anthropic_json).await` (per `src/providers/mod.rs:335-366` — the real signature takes exactly these four arguments, no `model` parameter; it pulls `model` out of `anthropic_json.get("model")` internally, records cost as a side effect during the call, and returns the OpenAI-shaped body directly as its `serde_json::Value` return value) to obtain `openai_json`, returning `(StatusCode::OK, Json(openai_json))`.
  - Example: an `anthropic_json` of `{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"hello"}],"usage":{"input_tokens":10,"output_tokens":5}}` results in an OpenAI-shaped response containing `"object":"chat.completion"` and `choices[0].message.content == "hello"`, per `translate_anthropic_to_openai`'s existing (already-tested) shape.
- Given this is `translate_and_record`'s first real caller in the codebase's live request path, when the handler calls it, then the `record_pending`-first sequencing (Story 2.1.2's shared `begin_cost_tracking` helper) still applies — `translate_and_record` itself calls `record_actual_usage`, which requires the session to already exist.
  - Example: `begin_cost_tracking` is called before `dispatch`, exactly as in `post_v1_messages`, confirmed by both handlers sharing the same helper function.

**Files**: `src/entrypoint/chat_completions.rs`

##### Task 3.1.1.1 (~4 min): Define `post_v1_chat_completions` signature and translate-in step
- `pub async fn post_v1_chat_completions(State(state): State<EntrypointState>, headers: HeaderMap, Json(body): Json<serde_json::Value>) -> Response`. Extract `model` and `stream` fields from `body` before translating; call `translate_openai_to_anthropic(&body)`.
- Files: `src/entrypoint/chat_completions.rs`

##### Task 3.1.1.2 (~4 min): Wire `begin_cost_tracking` + dispatch + `translate_and_record` for the non-streaming success path
- Call `begin_cost_tracking(&state.cost_tracker).await`, then `dispatch`, then on `Ok(ProviderResponse::Full(json))` call `let openai_json = translate_and_record(&state.cost_tracker, &session_key, request_id, &json).await;` (four arguments — no `model` parameter; `translate_and_record` extracts `model` from `json` internally and returns the OpenAI-shaped body directly, having already recorded cost as a side effect) and return `(StatusCode::OK, Json(openai_json))`.
- Files: `src/entrypoint/chat_completions.rs`

##### Task 3.1.1.3 (~4 min): Unit test for the non-streaming success path
- `#[tokio::test]` with a `Provider` test double returning a fixed Anthropic-shaped `Full` response; assert the handler's response is OpenAI-shaped (`object == "chat.completion"`) and that `report_for_session` shows recorded usage.
- Files: `src/entrypoint/chat_completions.rs` (`#[cfg(test)] mod tests`)

---

#### Story 3.1.2: OpenAI error envelope mapping

**As a** client of `/v1/chat/completions` using the real OpenAI SDK,
**I want** errors shaped exactly like OpenAI's real API errors,
**so that** my SDK's error handling works unmodified.

**Acceptance Criteria**:
- Given `ProviderError::Validation(msg, 400)`, when `map_provider_error_openai` runs, then it returns `(StatusCode::BAD_REQUEST, json!({"error":{"message":msg,"type":"invalid_request_error","param":null,"code":null}}))`.
  - Example: `ProviderError::Validation("model is required".into(), 400)` maps to HTTP 400, `error.type == "invalid_request_error"`, `error.message == "model is required"`.
- Given `ProviderError::RateLimited`/`RateLimitedWithRetry{retry_after}`, when mapped, then it returns HTTP 429 with `error.type == "rate_limit_error"` (mirroring OpenAI's own `rate_limit_error` type string) and a `Retry-After` header when `retry_after` is present.
  - Example: `ProviderError::RateLimited` maps to HTTP 429, `error.type == "rate_limit_error"`, no `Retry-After` header (since no duration is known).
- Given `ProviderError::Timeout` or `ProviderError::Exhausted` (the dedicated variant added by Task 2.1.3.1, distinct from a genuine upstream 503 — see Story 2.1.3), when mapped, then it returns `(StatusCode::SERVICE_UNAVAILABLE, json!({"error":{"message":"...","type":"server_error","param":null,"code":null}}))` per `ux.md`'s note that OpenAI convention for total outage varies by cause but is not a bare 500.
  - Example: `ProviderError::Exhausted` maps to HTTP 503, `error.type == "server_error"`; `ProviderError::Timeout` maps the same way.
- Given `ProviderError::Upstream{status, body}` for an otherwise-unmapped status — including a genuine upstream-returned `503`, which is a plain `Upstream{503, body}` and not conflated with `ProviderError::Exhausted` — when mapped, then it returns `(StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR), json!({"error":{"message":body,"type":"server_error","param":null,"code":null}}))`. This happens to produce the same HTTP 503 as the `Exhausted` case above for this one status value, but via a different variant/branch — no ambiguity exists at the `match` level even though the two statuses coincide.
  - Example: `ProviderError::Upstream{status:404, body:"model not found".into()}` maps to HTTP 404, `error.type == "server_error"`.

**Files**: `src/entrypoint/errors.rs` (relies on the `ProviderError::Exhausted` variant added in Story 2.1.3, Task 2.1.3.1 — no additional `src/providers`/`src/routing` change needed here)

##### Task 3.1.2.1 (~5 min): Implement `map_provider_error_openai`
- Full `match` over all eight `ProviderError` variants (including `Exhausted`) analogous to Task 2.1.3.2 but producing OpenAI's envelope shape and status mapping.
- Files: `src/entrypoint/errors.rs`

##### Task 3.1.2.2 (~3 min): Wire error mapping into `post_v1_chat_completions`
- On `Err(e)` (after `record_request_failed`), call `map_provider_error_openai(&e)` and build the response.
- Files: `src/entrypoint/chat_completions.rs`

##### Task 3.1.2.3 (~4 min): Unit tests for `map_provider_error_openai`
- One `#[test]` per `ProviderError` variant (including `Exhausted`), mirroring Task 2.1.3.4's structure for the OpenAI envelope shape.
- Files: `src/entrypoint/errors.rs` (`#[cfg(test)] mod tests`)

---

### Epic 3.2: Streaming path

**Goal**: `POST /v1/chat/completions` with `"stream": true` returns OpenAI-shaped `chat.completion.chunk` SSE frames translated live from the Anthropic upstream stream.

#### Story 3.2.1: `OpenAiStreamTranslator`

**As a** developer using an OpenAI-compatible streaming client,
**I want** streamed responses translated into OpenAI's `chat.completion.chunk` frame format live,
**so that** my client's streaming parser (built for OpenAI's format) works unmodified against consolette.

**Acceptance Criteria**:
- Given a raw Anthropic SSE byte stream containing `event: content_block_delta\ndata: {"delta":{"type":"text_delta","text":"Hi"}}\n\n`, when parsed by `OpenAiStreamTranslator` (via `eventsource_stream::Eventsource::eventsource()`), then it emits one `data: {"id":"...","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"Hi"},"finish_reason":null}]}\n\n` chunk.
  - Example: given that exact input frame and a synthesized `id` (e.g. `"chatcmpl-<uuid>"`, generated once per response), the emitted chunk's `choices[0].delta.content == "Hi"`.
- Given an Anthropic SSE frame `event: message_stop\ndata: {}\n\n`, when observed, then `OpenAiStreamTranslator` emits a final `chat.completion.chunk` with `choices[0].finish_reason == "stop"` followed by the literal sentinel `data: [DONE]\n\n`.
  - Example: consuming the translator to completion over a full Anthropic SSE transcript ends with exactly `data: [DONE]\n\n` as the last chunk.
- Given an Anthropic SSE `ping` event or any other event type not carrying translatable content (`message_start`, `content_block_start`, `content_block_stop`, `message_delta` without a stop reason), when observed, then `OpenAiStreamTranslator` emits zero chunks for that event (silently consumes it), never propagating an empty/malformed OpenAI chunk.
  - Example: a `event: ping\ndata: {}\n\n` frame produces no output bytes from the translator.

**Files**: `src/entrypoint/openai_stream.rs`

##### Task 3.2.1.1 (~5 min): Define `OpenAiStreamTranslator` struct and `eventsource()` wiring
- `pub struct OpenAiStreamTranslator<S> { inner: eventsource_stream::EventStream<S>, id: String, model: String, done: bool }` with a constructor `OpenAiStreamTranslator::new(inner: S, model: String) -> Self` calling `inner.eventsource()` (via the `Eventsource` trait) and generating `id = format!("chatcmpl-{}", uuid::Uuid::new_v4())`.
- Files: `src/entrypoint/openai_stream.rs`

##### Task 3.2.1.2 (~5 min): Implement per-event-type translation for `content_block_delta`
- In the `Stream` impl's `poll_next`, on each parsed `eventsource_stream::Event`, match `event.event.as_str()` against `"content_block_delta"`; parse `event.data` as JSON, extract `delta.text`, and build the OpenAI chunk JSON described in the acceptance criteria, serialized as `Bytes::from(format!("data: {}\n\n", chunk_json))`.
- Files: `src/entrypoint/openai_stream.rs`

##### Task 3.2.1.3 (~4 min): Implement `message_stop` → final chunk + `[DONE]` sentinel
- On `event.event == "message_stop"`, emit the finish-reason chunk described above, set an internal flag to emit `data: [DONE]\n\n` on the *next* poll, then set `done = true` after that.
- Files: `src/entrypoint/openai_stream.rs`

##### Task 3.2.1.4 (~3 min): Silently consume non-content event types
- For `"ping"`, `"message_start"`, `"content_block_start"`, `"content_block_stop"`, and `"message_delta"` frames without a terminal stop reason, loop to the next inner item instead of yielding a chunk (i.e., recurse/loop within `poll_next` rather than returning `Poll::Ready(Some(empty_chunk))`).
- Files: `src/entrypoint/openai_stream.rs`

##### Task 3.2.1.5 (~5 min): Unit tests for `OpenAiStreamTranslator`
- Tests covering: a `content_block_delta` frame produces the expected chunk; a `message_stop` frame produces the finish-reason chunk followed by `[DONE]`; a `ping` frame produces no output; a full multi-frame transcript (start → several deltas → stop) produces the expected ordered sequence of chunks ending in `[DONE]`.
- Files: `src/entrypoint/openai_stream.rs` (`#[cfg(test)] mod tests`)

---

#### Story 3.2.2: Streaming handler wiring for `/v1/chat/completions`

**As a** developer using an OpenAI-compatible streaming client,
**I want** the full pipeline (dispatch → cost tee → OpenAI translation → SSE response) wired end-to-end,
**so that** streaming works exactly like the non-streaming path, just incrementally.

**Acceptance Criteria**:
- Given `dispatch` returns `Ok(ProviderResponse::Stream(s))` for a `stream: true` OpenAI-shaped request, when `post_v1_chat_completions` builds its response, then it wraps `s` in `CostTrackingStream::new(...)` (reused unchanged from Story 2.2.1 — the tee operates on raw Anthropic SSE bytes regardless of which endpoint requested them), then wraps that in `OpenAiStreamTranslator::new(...)`, then in `Body::from_stream(...)`, returning a `Response` with the same SSE headers as Story 2.2.2.
  - Example: a test `Provider` streaming a fixed Anthropic SSE transcript results in an HTTP response whose body is the OpenAI-translated chunk sequence ending in `data: [DONE]\n\n`, while `report_for_session` simultaneously reflects the usage the underlying `CostTrackingStream` extracted from the same raw bytes.
- Given a mid-stream cut on this path, when it occurs, then the client sees an OpenAI-shaped analog of ADR-018's error framing: a final chunk with `choices[0].finish_reason == "stop"` (the `OpenAiStreamTranslator` cannot emit a `text/event-stream`-native `event: error` field the way the Anthropic-native path can, since OpenAI's chunk format carries no separate error-event type — the interruption is signaled by immediate stream termination via `finish_reason` and `[DONE]`, matching how OpenAI clients already detect an unexpectedly-short stream), followed by `data: [DONE]\n\n`.
  - Example: an inner stream that errors mid-way (Story 2.2.3's synthesized `event: error` frame arrives at the `OpenAiStreamTranslator`'s input) results in the translator recognizing the synthesized `error` event type and emitting the finish-reason chunk + `[DONE]` rather than propagating a translation panic or an empty chunk.

**Files**: `src/entrypoint/chat_completions.rs`, `src/entrypoint/openai_stream.rs`

##### Task 3.2.2.1 (~4 min): Branch `post_v1_chat_completions` on `ProviderResponse::Stream`
- Extend the `match` from Task 3.1.1.1 to handle `Ok(ProviderResponse::Stream(s))`: build `CostTrackingStream::new(s, ...)` then `OpenAiStreamTranslator::new(tee, model)`.
- Files: `src/entrypoint/chat_completions.rs`

##### Task 3.2.2.2 (~3 min): Build the SSE `Response` for the translated stream
- Same header set as Task 2.2.2.2, wrapping `Body::from_stream(translator)`.
- Files: `src/entrypoint/chat_completions.rs`

##### Task 3.2.2.3 (~4 min): Handle the synthesized `event: error` frame in `OpenAiStreamTranslator`
- Add a match arm for `event.event == "error"` (the frame type `CostTrackingStream`/ADR-018 synthesizes on a mid-stream cut) that emits the finish-reason chunk + `[DONE]` sentinel, same as `message_stop`'s handling.
- Files: `src/entrypoint/openai_stream.rs`

##### Task 3.2.2.4 (~4 min): Integration test for the full streaming pipeline
- Test with a `Provider` test double streaming a multi-frame Anthropic transcript (including a forced mid-stream `Err`); assert the final client-visible body is a well-formed OpenAI chunk sequence ending in `[DONE]`, and that `report_for_session` shows the expected partial/estimated usage.
- Files: `src/entrypoint/chat_completions.rs` (`#[cfg(test)] mod tests`)

---

## Phase 4: Integration tests & hardening

### Epic 4.1: End-to-end bound-server tests

**Goal**: Tests that exercise the actually-bound server (real `TcpListener`, real HTTP client), not just in-process `tower::ServiceExt::oneshot` calls, per requirements.md's Success Metrics and the `cost_metrics/server.rs` precedent.

#### Story 4.1.1: Bound-server tests for both endpoints, streaming and non-streaming

**As the** feature owner,
**I want** at least one test per endpoint/mode that binds a real socket and makes a real HTTP request,
**so that** `cargo test` proves the feature works as a real server, not just as wired-together functions.

**Acceptance Criteria**:
- Given `serve_entrypoint_with_shutdown(0, state, shutdown_rx)` spawned via `tokio::spawn` (per Task 1.2.3.2's testable variant) with the assigned ephemeral port read back via `listener.local_addr()` (captured before the listener moves into `axum::serve`, per the `cost_metrics/server.rs` precedent), when a real `reqwest::Client` (or `hyper`, whichever the repo's existing dev-dependencies provide — confirm via `Cargo.toml`) issues `POST http://127.0.0.1:<port>/v1/messages` with `"stream": false`, then it receives a real HTTP 200 response with an Anthropic-shaped JSON body.
  - Example: against a test double `Provider`, the response's `content[0].text` matches the double's fixture text exactly.
- Given the same bound server, when a real HTTP client issues `POST http://127.0.0.1:<port>/v1/messages` with `"stream": true` and reads the response body as a byte stream, then it receives the exact SSE frames the test double `Provider` produced.
  - Example: the client-observed SSE stream, split on `\n\n`, matches the fixture frame list exactly.
- Given the same bound server, when a real HTTP client issues `POST http://127.0.0.1:<port>/v1/chat/completions` with `"stream": false` and `"stream": true` respectively, then it receives OpenAI-shaped JSON and OpenAI-shaped chunk sequences respectively, per Phase 3's handlers.
  - Example: the non-streaming response has `"object": "chat.completion"`; the streaming response ends in `data: [DONE]\n\n`.

**Files**: `src/entrypoint/mod.rs` (or a new `tests/entrypoint_integration.rs` if the repo's existing test layout favors a top-level `tests/` directory for cross-module integration tests — confirm convention via `ls tests/` before choosing)

##### Task 4.1.1.1 (~4 min): Confirm existing integration-test layout convention
- Check whether the repo already has a `tests/` directory (`ls tests/` or equivalent) and whether `cost_metrics/server.rs`'s own bound-server tests live inside `#[cfg(test)] mod tests` in the same file or in a separate `tests/` crate; follow whichever convention already exists for consistency.
- Files: none (research task; determines the file path used by the remaining tasks in this story)

##### Task 4.1.1.2 (~5 min): Bound-server test for `/v1/messages` non-streaming
- Implement the test described in the first acceptance criterion above, using a `Provider` test double injected via a test-only `EntrypointState` constructor (or by constructing `EntrypointState` fields directly, bypassing `EntrypointState::build`, since that test double is not a real `AnthropicProvider`/`BedrockProvider`).
- Files: per Task 4.1.1.1's determined location

##### Task 4.1.1.3 (~5 min): Bound-server test for `/v1/messages` streaming
- Implement the second acceptance criterion's test.
- Files: per Task 4.1.1.1's determined location

##### Task 4.1.1.4 (~5 min): Bound-server tests for `/v1/chat/completions` (both modes)
- Implement the third acceptance criterion's two tests.
- Files: per Task 4.1.1.1's determined location

---

### Epic 4.2: Three-way cost-tracking coverage

**Goal**: Explicit tests for all three cost-tracking outcomes named in `pitfalls.md` — clean success, pre-first-byte failure, mid-stream cut — across both endpoints, closing the loop opened in Stories 2.1.2/2.2.1/2.2.3/3.1.1/3.2.2.

#### Story 4.2.1: Consolidated three-way coverage test matrix

**As the** feature owner,
**I want** one clear, named test per (endpoint × outcome) cell,
**so that** the hardest-to-get-right part of this feature (per requirements.md's Rabbit Holes) has explicit, individually-readable proof of correctness rather than relying on incidental coverage from other tests.

**Acceptance Criteria**:
- Given the six cells (2 endpoints × 3 outcomes), when `cargo test` runs, then each cell has at least one test whose name makes the cell being tested unambiguous (e.g. `messages_streaming_mid_stream_cut_records_estimated_usage`, `chat_completions_non_streaming_pre_first_byte_failure_records_failed`).
  - Example: `cargo test entrypoint::` output lists at least 6 distinct test names matching this pattern, and all pass.

**Files**: `src/entrypoint/messages.rs`, `src/entrypoint/chat_completions.rs` (`#[cfg(test)] mod tests` in each; most cells are likely already covered by Stories 2.1.2/2.2.1/2.2.3/3.1.1/3.2.2's own tests — this story's job is to audit and fill any gap, not necessarily write six new tests from scratch)

##### Task 4.2.1.1 (~4 min): Audit existing tests against the six-cell matrix
- Cross-reference the tests already written in Tasks 2.1.2.3, 2.2.1.4, 2.2.3.2, 3.1.1.3, 3.2.2.4 against the six cells; list which cells are already covered and which are not.
- Files: none (audit task)

##### Task 4.2.1.2 (~5 min): Write any missing cell's test
- For whichever cell(s) Task 4.2.1.1 finds uncovered (most likely `chat_completions` pre-first-byte-failure, since Story 3.1.1/3.1.2 focus on success/error-mapping rather than the cost-tracking angle specifically), add the missing test asserting `record_request_failed` was called (via `report_for_session` showing a session with no recorded usage).
- Files: `src/entrypoint/chat_completions.rs` (`#[cfg(test)] mod tests`) or wherever Task 4.2.1.1 identifies the gap

---

### Epic 4.3: Final gate

**Goal**: `cargo fmt`, `cargo clippy`, and `cargo test` all pass clean, confirming the feature is ready to ship per requirements.md's Success Metrics ("no feature flag... ship directly").

#### Story 4.3.1: Full workspace check

**As the** feature owner,
**I want** the standard CI gate to pass locally before merge,
**so that** the PR doesn't surface avoidable review comments about formatting/lint/test failures.

**Acceptance Criteria**:
- Given the complete diff from Phases 1-4, when `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, and `cargo test` are run, then all three exit 0.
  - Example: `cargo test 2>&1 | tail -5` shows a line matching `test result: ok. \d+ passed; 0 failed`.

**Files**: (no new files; verification only)

##### Task 4.3.1.1 (~5 min): Run and fix `cargo fmt --check`
- Run `cargo fmt --check`; if it fails, run `cargo fmt` and re-diff to confirm only formatting changed.
- Files: any file `cargo fmt` reformats

##### Task 4.3.1.2 (~5 min): Run and fix `cargo clippy --all-targets -- -D warnings`
- Run the lint; address each warning individually (no blanket `#[allow]` unless a specific lint is a known false positive, matching this repo's existing `#[allow(clippy::expect_used)]`-style precedent of narrowly-scoped allows only).
- Files: wherever clippy flags issues

##### Task 4.3.1.3 (~5 min): Run `cargo test` and confirm full pass
- Run `cargo test`; confirm the full suite (existing + all new tests from Phases 1-4) passes with zero failures.
- Files: none (verification only)

---
