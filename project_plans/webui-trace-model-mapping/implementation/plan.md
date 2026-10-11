# Implementation Plan: webui-trace-model-mapping

**Feature**: Make the Angular dashboard answer "what did the client ask for, where did Consolette send it, and why?" - real trace fields and status (including failures), requested -> routed model with reason and attempt chain, an upstream model catalog view, plus a systematic gap pass over the four pages. Compression wiring and real first-byte time (Epic 6) are a deferred follow-up, not part of this plan's scope or effort.
**Date**: 2026-10-09 (revised through triad-review repair iteration 1; final-pass triad repair applied 2026-10-10 with Tyler's decisions: Epics 1-5 with no cuts, Epic 6 deferred)
**Status**: Final-pass triad repair applied (2026-10-10). **Tyler's decisions (2026-10-10)**: (a) commit to Epics 1-5 with no cuts; (b) Epic 6 (compression wiring + real first-byte tee) is a deferred follow-up project, kept intact below but outside this plan's scope, effort, schedule and cut order. Effort is now **48.4 engineer-days (~9.7 weeks)**, 3.4 days (~0.7 weeks) over the approved ~9-week (45-day) appetite - see "Effort and Appetite" (single source of truth). Earlier iterations: 1-3 repaired the architecture and adversarial reviews; iteration 4 applied the Phase 4 consistency, pre-mortem and validation repairs; triad repair iteration 1 added the UX, product and engineering triad findings; this final pass adds the real-socket client-abort e2e in week 1-2 (Task 1.1.3k), n/a cells without per-cell tab stops, drawer Why/snapshot/sheet-focus ACs, reflow and extra a11y checks, the Epic 1 validation gate, and the accepted-risk decision R1.
**ADRs**: ADR-001 (SSE contract: backend camelCase + shared fixture), ADR-002 (trace unit, attempt capture, single terminal trace, cancellation), ADR-003 (cached upstream model catalog, on-demand refresh), ADR-004 (requested / routed model definitions), ADR-005 (compression wiring + first-byte stream tee; **deferred follow-up**, not implemented by this plan) - all in `project_plans/webui-trace-model-mapping/decisions/`

Conventions for every task: run UI builds with `CONSOLETTE_REQUIRE_UI=1` (otherwise `build.rs` silently keeps a stale bundle on TS errors); Rust gate is `cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test`; UI gate is `cd ui && npx ng test --watch=false --browsers=ChromeHeadless`; no `unwrap`/`expect` in non-test code; no new crates or npm packages (puppeteer is already a devDependency, `ui/package.json`; no `criterion` - hot-path checks are ratio-based `#[test]`s). TDD: the failing test is written and seen failing before the production change in the same story. Task times in this plan are **ideal engineer-days per story** (TDD, gates and PR hygiene included); the old per-task "~5 min" figures were removed because they were not credible. **Every task is <= 0.5 day (4 h)**; a task that proves larger is split before it starts, and a task listed without a time is <= 0.5 d by construction. CI already sets `CONSOLETTE_REQUIRE_UI=1` (`.github/workflows/ci.yml:16`); Task 1.1.2f adds `make ui-fresh-check` and a CI step that fail on a stale embedded bundle.

---

## Domain Glossary

| Term | Definition | Notes |
|------|-----------|-------|
| `RequestTrace` | The SSE event (`DashboardEvent::RequestTrace`) emitted at the end of a **client request** (success, failure, cache hit, local, or cancelled) | Upserted in UI by `requestId`. In this project each request emits exactly one terminal trace (`traceRevision: 0`); the stream first-byte re-emit (`traceRevision: 1`, ADR-005) is the deferred follow-up |
| Trace unit | **One trace per client request** (one `request_id`), not per `Router::dispatch` call | ADR-002 amendment. Server-tool emulation loops, cache hits and local dispatch each map to exactly one trace |
| `TraceScope` | RAII guard in `src/metrics/trace.rs`: created and **owned by the handler wrapper** (or by the `Router::dispatch` compatibility wrapper) as a plain local; inner functions and the emulation loop only borrow it (`&mut TraceScope`, `ScopedDispatch`), there is no `Arc` of it. Owns `request_id`, the `RequestDetail`, the `AttemptLog`, iteration count; `finish(..)` emits the terminal trace and is the only normal exit; `Drop` without `finish` emits `cancelled` (499) only if the handler has not yet returned, else `unfinished` (500, logged `error!`, counter) - see the finish contract in Story 1.1.3 | Replaces the "wrap dispatch" idea; survives future cancellation (ADR-002) |
| `TraceKind` | `Routed` / `CacheHit` / `Local`; wire `traceKind`: `"routed"`, `"cache_hit"`, `"local"` | Lets UI and error-rate logic treat the three honestly |
| `RequestTraceData` | Rust payload of `RequestTrace` (`src/entrypoint/events.rs`); camelCase on the wire; built in **one** place: `RequestTraceData::from_detail(&RequestDetail)` | ADR-001; the same function serves SSE emit, `finish` and the backfill endpoint (and later the first-byte re-emit). Carries `traceRevision` (0 = terminal at headers/finish; 1 is reserved for the follow-up stream re-emit); the UI upsert ignores a lower revision for the same `requestId` (also applies to backfill, Story 4.1.6) |
| `requestId` | UUID string created by `TraceScope::begin`; UI `id` equals it | Never the placeholder `unknown` |
| `RequestedModel` / `requestedModel` | Raw `body["model"]` exactly as the client sent it, captured in `TraceScope::begin` | ADR-004 |
| `RoutedModel` / `routedModel` | `chosen.model` of the final attempt when the route pins one; else the requested name | ADR-004. Rust side models `routedModel` + source as one enum `Routed { model, source: Pinned \| Passthrough }`, flattened to two wire fields in `events.rs` |
| `RoutedModelSource` | Enum `Pinned` / `Passthrough` (wire `"pinned"`, `"passthrough"`) | Tells UI whether `routedModel` is a rewrite |
| `routedProvider` | Upstream *name* (config key) of the final attempt, never an index; `"local"` for local-catalog dispatch; null for cache hit | Survives config hot-swap |
| `ResolvedModel` (provider-internal) | Concrete model id picked by `providers/openai/resolution.rs` for a family route | Optional `resolvedModel` only if Task 1.1.1c says it is cheap |
| `AttemptRecord` | One try against one candidate: iteration, upstream, model, outcome, error class, status, latency ms | `src/metrics/trace.rs` (no `routing` and no `providers` import); no bodies, no upstream error text (only the closed `AttemptErrorClass`). The scope also holds at most one **pending attempt** (upstream, model, iteration, `Instant`) created before the upstream `.await`; completion moves it into the log, `Drop` of an `Active` scope converts it to `Cancelled` (Story 2.1.2) |
| `AttemptOutcome` | Enum `Ok` / `RateLimited` / `Timeout` / `Auth` / `Validation` / `ShapeMismatch` / `Upstream` / `Shed` / `Cancelled` | The `From<&ProviderError>` impl lives in `src/providers/mod.rs` next to `trace_status`/`kind_label` (keeps `trace.rs` free of a `providers` import). Exhaustive `match`; `RateLimited` and `RateLimitedWithRetry` both map to `RateLimited`; `ModelUnsupported` -> `Validation`; `Exhausted` never recorded as an attempt |
| `AttemptErrorClass` | Closed sanitized enum on a failed attempt: `Auth` / `RateLimited` / `Overloaded` / `Timeout` / `ServerError` / `Unreachable` / `ClientError` / `BadResponse` / `Shed` / `Cancelled`; `None` for an `Ok` attempt. Wire (snake_case): `auth`, `rate_limited`, `overloaded`, `timeout`, `server_error`, `unreachable`, `client_error`, `bad_response`, `shed`, `cancelled` | Derived from `&ProviderError` (plus status) by `From<&ProviderError> for AttemptErrorClass` in `src/providers/mod.rs`, exhaustive `match`: `RateLimited`/`RateLimitedWithRetry` -> `RateLimited`; `Timeout`; `Auth`; `Validation`/`ModelUnsupported` -> `ClientError`; `ResponseShapeMismatch` -> `BadResponse`; `Upstream{status: 0}` -> `Unreachable`; `Upstream{529 or 503}` -> `Overloaded`; other `Upstream{5xx}` and `Exhausted` (defensive) -> `ServerError`; other `Upstream` -> `ClientError`. Never derived from error text |
| `errorMessage` | Short redacted message on a failed `AttemptTrace`: a fixed phrase per `AttemptErrorClass` plus the numeric status, e.g. `Upstream overloaded (HTTP 529)`; built in `events.rs` from class and status only, so it cannot carry upstream text | Same allowlist idea as `CatalogError.message` (Story 3.1.2) |
| `AttemptLog` | Bounded (`MAX_ATTEMPTS = 16`) local `Vec<AttemptRecord>` plus `truncated: bool` and `iterations: u8` | Owned by the `TraceScope`; mutated under `&mut`, no lock |
| `AttemptTrace` | Wire (camelCase) form of `AttemptRecord`: `iteration, upstream, model, outcome, status, latencyMs, errorClass, errorMessage` (the last two null for an `ok` attempt), built in `events.rs` via `From<&AttemptRecord>` | Nested in `RequestTraceData.attempts` |
| `RouteReason` | Enum `SessionPin` / `Strategy` / `Exploration` / `Fallback` / `RateLimitHold` / `Default` / `Cache` / `Local`. **Wire strings (the one vocabulary; ADR-004 item 4 and `ux.md` use the same table, see "Route reason and label maps" below)**: `session_pin`, `strategy`, `exploration`, `fallback`, `rate_limit_hold`, `default`, `cache`, `local`. `Strategy` also sets `routeStrategy` (the `RoutingStrategy::name()` string, e.g. `weighted`); `routeStrategy` is null for every other reason. There is no `alias`, `rule` or `no route` reason, and no `routeRule` field | `Strategy` carries a `&'static str` from the new `RoutingStrategy::name()` (Task 2.1.1d). Derived by `RouteReason::for_request(&RouteInputs)` where `RouteInputs` holds only plain values (`kind`, `session_pinned: bool`, `strategy_name: &'static str`, `exploration: bool`, `hold_retries: u8`, `&[AttemptRecord]`), no router types |
| `RequestOutcome` | Terminal summary passed to `TraceScope::finish`: final status, provider, routed model, source, reason, hold_retries, timings | `src/metrics/trace.rs`; consumed only to populate the `RequestDetail`. Handlers return `HandlerResult { response, outcome }` so every return path carries one (Story 1.1.3) |
| `ScopeState` | `Active` / `Finished` (+ `handler_returned: bool`); `Drop` outcome is `cancelled` if `Active && !handler_returned && !panicking`, else `unfinished` | Distinguishes client cancellation from an internal handler bug (Story 1.1.3) |
| `trace_status` | `ProviderError::trace_status() -> u16`; canonical upstream-class status for the trace | Not necessarily the client-visible code (ADR-002); drawer says so. Cancelled = 499 |
| Status-at-headers | For `stream: true`, `statusCode` is the status when response headers arrived; a later mid-stream failure is not reflected | UI tooltip + glossary note; mid-stream failure stays a backlog item |
| `timeToHeadersMs` | Latency from send of the **final attempt** to its response headers | Replaces the misleading meaning of `durationMs`. Same time origin as `firstByteMs` |
| `totalMs` | Wall time from `TraceScope::begin` to terminal emit (client-visible, spans all emulation-loop iterations) | New in this revision |
| `firstByteMs` | (follow-up, Epic 6) Real time from the **send of the final attempt** (same origin as `timeToHeadersMs`, so `firstByteMs >= timeToHeadersMs` always) to the first byte polled by the stream tee; `Option<u64>` | ADR-005. **In this project always `null`** and the cell reads `n/a (not streamed)` / `n/a (not measured)`; a non-null value, when it arrives, renders as a duration with no table change |
| `firstByteState` | (follow-up, Epic 6) Wire enum for streamed rows: `pending` / `measured` / `ended_empty` / `client_closed`; absent for non-streams | **In this project never sent.** Set-once semantics and the three terminal texts are specified in Story 6.1.5 and ADR-005 item 5 |
| `CompressionOutcome` | (follow-up, Epic 6) `{ status: CompressionStatus, tokens_after: Option<u64> }` computed in the single ingress compression step and recorded with `TraceScope::record_compression(outcome)` | ADR-005. In this project `begin` records `tokens_before` and status `off` and nothing ever changes it |
| `CompressionStatus` | `"off"` (engine disabled by config) / `"none"` / `"skipped"` / `"failed"` / `"compressed"` | Wire enum is defined now (Story 1.1.2) and its n/a texts are owned by Story 1.1.4 (`naReason`); **in this project the backend only ever sends `off`**. `none`/`skipped`/`failed`/`compressed` (meanings in Story 6.1.2) arrive with the follow-up |
| `naReason` | UI pure function `(trace, column) -> NaReason \| null` returning the explicit "n/a (...)" text per the Column Truth Table | Table-driven spec |
| `ModelCatalog` | Cached per-upstream list of advertised models (`src/routing/model_catalog.rs`) | ADR-003 |
| `UpstreamName` | Newtype over `String` for a configured upstream's name, defined in the leaf module `src/metrics/ids.rs` (re-exported) so catalog and trace both import it without depending on each other | Architecture review |
| `UpstreamModels` | Catalog entry: `models`, `totalCount`, `truncated`, `refreshedAt`, `lastAttemptAt`, `refreshing`, `state`, `error` | |
| `CatalogState` | Sum type `Loading` / `Ok` / `Stale` / `Error` / `Unsupported` | Exhaustive in Rust and TS |
| `CatalogRefreshMode` | Config enum `OnDemand` (default) / `Interval { secs }` / `Off` | ADR-003 |
| `CatalogError` | `{ kind: CatalogErrorKind, status: Option<u16>, message: String }`, sanitized | Never `ProviderError::to_string()` |
| `CatalogErrorKind` | `Auth` / `Timeout` / `RateLimited` / `Upstream` / `Unsupported` / `Other` | |
| `ConfigWarning` | `{ upstream, model, referencedBy }` for a model pinned in config but not listed upstream | Only for `Ok`/`Stale` upstreams, never family routes |
| `ModelCatalogChanged` | Additive `DashboardEvent` telling the UI to refetch the catalog | |
| `InspectDrawer` | Non-modal right-side panel showing route, attempts waterfall, tokens for one trace | `role="dialog"`, `aria-modal="false"` |
| `GapReport` | Output of the exploratory pass: gap, severity, disposition (fixed / backlog) | `project_plans/webui-trace-model-mapping/gap-report.md` |

**Glossary term count: 40** (recounted with awk over the table rows after adding `AttemptErrorClass` and `errorMessage`; 38 before this iteration, the consistency review's "37" was a miscount).

Rows marked (follow-up, Epic 6) name wire fields or types that exist in the contract but carry constant or absent values until the deferred compression/first-byte project lands; nothing in Epics 1-5 reads a non-constant value from them.

### Column Truth Table (success-metric honesty)

What each Live Request Traces column shows. "real" = measured value; "n/a (reason)" = an en dash whose reason is **visible**, not only sr-only, and **without per-cell tab stops** (a 100-row table with several dashes per row would add hundreds of tab stops): the dash carries `title="n/a (reason)"` for hover; keyboard users get the same text as visible text in (a) the footer disclosure "What do these dashes and chips mean?" (`aria-expanded`), which lists every reason string with its meaning, and (b) the Inspect drawer's "Not recorded" block for that trace; screen readers get it from a row-level sr-only description (`aria-describedby` on the row's Inspect button, the row's single tab stop) such as "First byte: n/a (not streamed). Tokens after: n/a (compression off)." The footer also carries the one-line legend "- = not applicable; hover a dash or open the legend for the reason". Never `0`, `undefined`, or a blank. Implemented once in the UI `naReason` function and pinned by a table-driven spec (Story 1.1.4, which owns every text in the table including `n/a (compression skipped)` and `n/a (compression failed)`; the deferred Epic 6 adds only the three streamed first-byte state texts, Story 6.1.5e).

| Column | Routed, non-stream | Routed, stream | Cache hit | Local | Failed (no response) | Cancelled |
|--------|--------------------|----------------|-----------|-------|----------------------|-----------|
| Request ID | real | real | real | real | real | real |
| Latency (to headers) | real | real (headers) | real (lookup time) | real | real | real elapsed |
| First byte (from upstream send) | `n/a (not streamed)` | `n/a (not measured)` **in this project** (no `firstByteState` is sent); a non-null `firstByteMs` renders as a duration. Follow-up (Epic 6): `pending` until the tee sees the first byte, then real; `n/a (stream ended before first byte)` if the upstream stream ends or errors first, `n/a (client closed before first byte)` if the client goes away first; `n/a (synthesized stream)` for emulated search | `n/a (served from cache)` | real if `firstByteMs` is non-null, else `n/a (not streamed)` / `n/a (not measured)` | `n/a (no response)` | `n/a (cancelled)` |
| Tokens before | real (estimate, "est.") | real (est.) | real (est.) | real (est.) | real (est.) | real (est.) |
| Tokens after / saved | real est. if `compressionStatus` is `compressed` or `none`; `n/a (compression off)` if `off`; `n/a (compression skipped)` if `skipped`; `n/a (compression failed)` if `failed` | same | `n/a (served from cache)` | same as routed | same as routed | same as routed |
| Status | real (code) | real, tooltip "status at response headers" | `200 (cache)` | real | real | `499 Client closed` |
| Route | requested -> routed + reason chip | same | `cache` chip, no provider | `local` chip | reason + attempt badge | `cancelled` chip |

**This project (Epic 6 deferred) is honest by construction (pre-mortem P1-1, now the shipped state rather than an epic boundary).** The backend sends `compressionStatus: "off"`, `firstByteMs: null` and no `firstByteState`, so a streamed row reads `n/a (not measured)` in the First byte column (the one reason string used only while `firstByteState` is absent on a `stream: true` row) and every tokens-after cell reads `n/a (compression off)`; no false zero. Story 1.1.4 pins both with specs. M1 is met through these defined `n/a` values (requirements M1); they never count as "populated".

**Epic 1 trace-coverage boundary (engineering triad).** Until Story 2.1.6b lands, only `Router::dispatch` (through the compat wrapper) creates scopes; the ingress handlers are untouched (Story 1.1.3). So a project stopped after Epic 1 does **not** trace: requests rejected before dispatch (400 validation, `chat_completions` translation errors), response-cache hits, and local-model requests that bypass `dispatch`; and a server-tool emulation request shows one row per `dispatch` call, not one per client request. The UI states this instead of implying completeness: the table footer shows "Rows cover requests that reach routing. Requests rejected before routing, cache hits and multi-round search requests are not shown as one row yet." (Story 1.1.4, AC "Coverage note"); Task 2.1.6f deletes that note when 2.1.6b lands. No row, not a false row, is the honest state for the untraced paths. (A real-socket client abort is nevertheless traced at this boundary through the compat wrapper and proven by Task 1.1.3k.)

(Follow-up, Epic 6) First byte is measured from the send of the final attempt (same origin as Latency-to-headers), so it is never smaller than the Latency cell; earlier failed attempts and holds appear in `totalMs` and the drawer, not here. It is the first byte of the SSE body (Anthropic `message_start` arrives before the first token). First-*token* time is a backlog item (Story 5.1.4). A streamed row emits at most one re-emit (`traceRevision: 1`): `measured`, `ended_empty` or `client_closed` (never when the slot fires with no snapshot, Story 6.1.5).

### Route reason and label maps (single source for plan, ADR-004 and `design/ux.md`)

`routeReason` chip, one per row. Wire strings are the `RouteReason` values; `cancelled` is a status-driven chip (statusCode 499), not a reason.

| `RouteReason` | Wire `routeReason` | Row chip text | Drawer "Why" text |
|---|---|---|---|
| `SessionPin` | `session_pin` | `pinned` | "pinned to this upstream by the session" |
| `Strategy` | `strategy` | `strategy` | "chosen by the <routeStrategy> strategy" |
| `Exploration` | `exploration` | `explore` | "exploration pick" |
| `Fallback` | `fallback` | `fallback` | "moved to the next candidate after <first failed attempt label>" |
| `RateLimitHold` | `rate_limit_hold` | `held` | "waited for a rate-limit cooldown, then retried (<hold_retries>x)" |
| `Default` | `default` | `default` | "default route: no session pin, exploration or strategy decision applied" |
| `Cache` | `cache` | `cache` | "served from the response cache; no upstream was called" |
| `Local` | `local` | `local` | "served by a local model" |
| (status 499) | n/a | `cancelled` | "client closed the connection" |

**Chip jargon legend (UX-8).** Every chip has a `title` tooltip and a keyboard-reachable legend (a "What do these dashes and chips mean?" disclosure in the table footer, `aria-expanded`) using plain language: `pinned` = "a session pinned this conversation to one upstream"; `strategy` = "the routing strategy picked this upstream (name shown in the drawer)"; `explore` = "an occasional exploration pick to test another upstream"; `fallback` = "an earlier upstream failed, so the next one was tried"; `held` = "waited for a rate-limit cooldown, then retried"; `default` = "no special rule; the default route"; `cache` = "answered from the response cache"; `local` = "answered by a local model"; `cancelled` = "the client closed the connection". The legend strings are the drawer "Why" texts above shortened; one table-driven spec asserts both.

Attempt labels (drawer list and detail pane; `AttemptOutcome` wire strings: `ok`, `rate_limited`, `timeout`, `auth`, `validation`, `shape_mismatch`, `upstream`, `shed`, `cancelled`). The list row shows glyph + status + the `errorClass` label; the detail pane shows the label plus the backend `errorMessage`.

| `errorClass` wire | UI label | Example `errorMessage` |
|---|---|---|
| `auth` | auth failed | Upstream rejected credentials (HTTP 401) |
| `rate_limited` | rate limited | Upstream rate limit (HTTP 429) |
| `overloaded` | overloaded | Upstream overloaded (HTTP 529) |
| `timeout` | timed out | Upstream timed out (HTTP 504) |
| `server_error` | server error | Upstream server error (HTTP 500) |
| `unreachable` | unreachable | Upstream unreachable |
| `client_error` | request rejected | Upstream rejected the request (HTTP 422) |
| `bad_response` | bad response | Upstream response not understood (HTTP 502) |
| `shed` | shed, not tried | Skipped by admission control |
| `cancelled` | cancelled | Client closed the request before the upstream answered |

Catalog refresh errors use `CatalogErrorKind` labels instead (Models view, log lines): `auth` -> "auth failed", `timeout` -> "timed out", `rate_limited` -> "rate limited", `upstream` -> "upstream error", `unsupported` -> "not supported", `other` -> "failed". The two vocabularies are deliberately separate; the log field is `kind` for the catalog and `errorClass` is never logged.

Status sub-label in the status cell, derived in the UI from the trace (no wire field): `fell back` when the final `statusCode < 400`, `attempts.length > 1` and the final attempt's `upstream` differs from the first attempt's; `retried` when `attempts.length > 1` and the upstream is the same (rate-limit hold or same-upstream retry); nothing otherwise.

---

## Pattern Decisions

Step 0.5 creative pass, three approaches considered for the whole feature:
1. *Adapter-and-patch*: UI adapter for snake_case, per-field patches wherever data is missing. Strength: no wire change. Weakness: leaves failures invisible and attempt data nowhere.
2. *Trace scope + terminal event* (chosen): one `metrics/trace.rs` RAII scope per client request, one terminal write, camelCase contract pinned by a shared fixture. Strength: fixes failures, attempts, cancellation, loop/cache/local traffic and hot-path lock cost together. Weakness: touches the `dispatch` hotspot and every ingress handler.
3. *Full trace store / OTel*: separate store or OTLP exporter. Strength: history and standard tooling. Weakness: new deps/services, breaks air-gapped single-binary constraint.

| Component | Pattern Chosen | Source | Alternative Rejected | Reason |
|-----------|---------------|--------|---------------------|--------|
| Attempt capture | Local accumulator owned by the scope, handed over once (Unit-of-Work-like, no DB) | PoEAA (Fowler) | `mutate_request` per attempt | 3 lock+scans per attempt today; one replace per request (plus the `begin` push) |
| Terminal emission | RAII guard (`TraceScope`; `finish` is the only normal exit, `Drop` emits `cancelled` or `unfinished` per `ScopeState`) | Rust idiom (resource-lifecycle) | Code after `.await` in a `dispatch` wrapper | Handler future is dropped on client disconnect; code after `.await` never runs (adversarial review). `Drop` is sync-safe: ring buffer and `event_tx` use `std::sync::Mutex` and `broadcast::send` |
| `dispatch` split | `dispatch` (compat wrapper creating its own scope) + `dispatch_traced(&mut TraceScope, ..)` + `dispatch_attempts` loop body | GoF Template-style | Editing every `return` site / changing 60 call sites | Handlers needing aggregation (emulation loop) share one scope; existing callers and tests keep the old signature |
| `ModelCatalog` | Repository-like in-memory cache + on-demand/optional-interval refresher | PoEAA | Live on-demand `list_models`; unconditional timer | Needs age/error per upstream; refresh can trigger credential helpers/paid calls so it is opt-in/on-demand |
| Provider `list_models` fan-out | Adapter (existing `Provider` trait) over `Router::providers_named()` | GoF | New per-provider HTTP code | Adapters already carry auth |
| `AttemptOutcome`, `RouteReason`, `CatalogState`, `RoutedModelSource`, `CompressionStatus`, `TraceKind` | Sum types, exhaustive `match` | type-driven-design | strings / bool flags | Illegal states unrepresentable |
| `UpstreamName` | Newtype in a leaf module | type-driven-design | raw `String`; newtype inside trace module | Avoids a catalog -> trace dependency |
| `status_code`, `tokensAfter`, `firstByteMs` | `Option<T>` (null = not recorded) | Parse-don't-validate | sentinel `0` | "0ms"/"0 tokens" lied to the operator |
| First-byte measurement (deferred, Epic 6) | Pass-through stream tee hook on existing `CostTrackingStream` (non-blocking, set-once CAS; the slot owns the final `RequestDetail` snapshot for the re-emit) | Decorator | New wrapper stream; parsing SSE | One existing tee already used by all three streaming ingress paths |
| Compression (deferred, Epic 6) | Single fail-open ingress step before dispatch, config-gated; engine runs under `spawn_blocking` + `timeout` with a byte cap as primary guard (timed-out task abandoned, bounded by a semaphore) | Facade | Per-handler inline calls | One call site for `/v1/messages` and `/v1/chat/completions` (translated), emulated search included |
| SSE contract | Shared fixture (consumer-driven contract test) | Test pattern | UI adapter, dual emit | Single consumer; fixture detects drift (ADR-001) |
| Catalog error text | Anti-corruption classifier (`CatalogError`) | DDD | `ProviderError::to_string()` | Upstream bodies can leak secrets/prompts |
| UI stores | Signal stores (`MetricsStore` pattern) | Existing repo ADR-003 | NgRx | No new deps |
| Waterfall / drawer | Tailwind + CSS bars, standalone component | build-vs-buy C1 | chart.js / CDK / PrimeNG | Few rows; zero new deps |

---

## Tech Debt Disposition

Churn data: `git log` counts from the adversarial review at `main` 49ec827 (`src/providers/mod.rs` 22 commits, `src/entrypoint/mod.rs` 20, `src/routing/router.rs` 19); Task 1.1.1f re-runs them once to confirm.

| Area | Existing Issue | Disposition | Justification |
|------|----------------|--------------|----------------|
| `src/routing/router/mod.rs` (1097 lines; `dispatch` ~130 lines, 5 near-duplicate outcome arms) | Churn centre, last-attempt-wins state | **Isolate via seam**: trace types and reason derivation (`RouteReason::for_request`, taking plain inputs) live in `src/metrics/trace.rs`, outcome mapping in `providers/mod.rs`, not the router; router gains only `dispatch_traced`/`dispatch_attempts` extraction and record-push lines. Net line growth target: <= 0 (Task 2.1.2d states it) | New code gets clean boundaries without restructuring the file |
| `src/metrics/mod.rs` (568 lines) | `update_request_timing` hardcodes 200, in-place overwrite helpers, **already imports `entrypoint::events::RequestTraceData`** (`:282`), i.e. metrics -> HTTP-layer edge exists today | **Extend as-is, moving DTO construction out**: `finish`/`record_first_byte` build the DTO only via `RequestTraceData::from_detail` in `events.rs`; the only `entrypoint` touch left in `metrics` is the one `event_tx.send`. Removes three helpers, adds `metrics/trace.rs`, which imports only `metrics` siblings, std and serde (no `routing`, no `providers`: the `From<&ProviderError>` impl lives in `providers/mod.rs`). Not literally a leaf of the crate: `metrics -> providers`/`entrypoint::events` edges and the `providers -> metrics`, `entrypoint -> routing -> metrics` cycles already exist (`counters/mod.rs:220`, `providers/openai/resolution.rs:50-54`); this adds no new edge | Edge is kept, not worsened. Verify-phase backlog: move `DashboardEvent`/`RequestTraceData` into a dependency-free `events` module so `metrics -> entrypoint` can be cut |
| `src/providers/mod.rs` (22 commits) | `ProviderError` classification scattered | **Extend as-is** (`trace_status` and `From<&ProviderError> for AttemptOutcome`, next to `kind_label`) | Single exhaustive `match`, no wildcard |
| `src/entrypoint/mod.rs` (758 lines, 20 commits - highest churn) | Route table growing | **Extend as-is** (route registration, `EntrypointState` fields only); handlers go to `entrypoint/models.rs` (the deferred Epic 6 would add `entrypoint/compress_step.rs`) | No logic added to the hotspot |
| `src/entrypoint/messages/mod.rs`, `chat_completions.rs` | Each handler repeats begin-cost-tracking + dispatch + tee | **Extend as-is**: thin wrapper + `TraceScope::begin` + `HandlerResult` split (Story 2.1.6b); the compression call and tee hook arg are deferred (Epic 6) | Minimal lines per handler |
| `src/entrypoint/api.rs` (933 lines) | `get_models` rebuilds providers per call, serial, untyped errors | **Isolate via seam** (`entrypoint/models.rs` + `ModelCatalog`); legacy handler untouched | Config Editor compatibility; migrate callers later (backlog) |
| `src/entrypoint/events.rs` (136 lines) | No `rename_all`, test pins snake_case | **Extend as-is**; becomes the single DTO builder | Small, stable |
| `src/entrypoint/observability.rs` (43 KB) | Fabricated benchmark rows/constants, dropped `rateLimits` PUT, auth clobber | **Extend as-is** for targeted fixes in Epic 4 | Fixes remove instances of the violation |
| `src/entrypoint/cost_tee.rs` | Tee already wraps all streaming paths; no timing hook | **Untouched in this project.** Deferred follow-up (Epic 6, Story 6.1.5): first-byte hook via a `with_first_byte_hook(Option<_>)` builder, so the existing `CostTrackingStream::new` call sites stay untouched (VERIFIED by grep at `main` 49ec827: 7 in `src` = 3 production sites at `chat_completions.rs:73` and `messages/mod.rs:235,:491` plus 4 in `cost_tee.rs` tests; plus 1 more in `tests/server_tool_emulation.rs:500`) | Reuse over a second wrapper |
| UI `recent-trace-table.component.ts` | Reads fields that never arrive, no a11y | **Refactor-first** within Story 1.1.4 | Everything in Epics 1-2 builds on it |

---

## Migration Plan

No schema or persisted data changes (ring buffer and catalog are in-memory). Contract migration only:
- **Wire change**: SSE structs switch snake_case -> camelCase (ADR-001). UI and Rust change in the same commit series (Story 1.1.2); the shared fixture fails whichever side lags. Later stories add fields only (additive); the wire reserves `compressionStatus`, `firstByteMs`, `firstByteState` and `traceRevision 1` for the deferred Epic 6 and sends `off`, `null`, absent and `0` meanwhile.
- **Behavior change**: none in this project. (The deferred Epic 6 would modify outbound prompts when `compression.enabled` is set, default `false`, ADR-005.)
- **Reversibility**: `git revert` of the Story 1.1.2 commits restores both sides together.
- **Zero-downtime strategy**: n/a (single binary embeds the SPA). Dev-loop hazard (new UI with old binary) is covered by `CONSOLETTE_REQUIRE_UI=1` and the served `main-*.js` hash check (Task 1.1.1e).
- **Rollback procedure**: revert; if an external SSE consumer is reported before release, enable the one-release dual-emit of legacy snake_case keys for `RequestTraceData` only (`compat_snake_case` config key, ADR-001 item 4), a concrete cut-able task (Task 1.1.2g, 0.3 d) rather than an after-the-fact patch. **Hard precondition**: Task 1.1.1g (legacy consumer grep over `src/` and `tests/`, result recorded) must be done and clean before Task 1.1.2b merges.

## Observability Plan
- **Logs** (structured `tracing`, no secrets, no bodies): exactly one line per catalog refresh: `info!` on success (`upstream`, `outcome=ok`, `model_count`, `duration_ms`), `warn!` on failure (`upstream`, `outcome=error`, `kind`, `status`, `duration_ms`; no message text, full error at `debug`); this is the field set `design/ux.md` S7 and UX-30 use (no `age_s`/`cached` fields); `debug` when a `finish` replaces no ring-buffer entry (evicted; the event is still emitted from the scope-local detail); `error!` for every `unfinished` scope (an internal handler bug, never silent). A `broadcast::send` with no receivers is normal and not logged (deliberate reading of "trace-emission drops": ring-buffer evictions are counted and logged at `debug`, slow SSE clients are reported by `system_lag`; `requirements.md` and UX S7 state the same).
- **Metrics** (counters on the existing `/metrics` JSON, location confirmed in Task 2.1.3e): `trace_finish_total{status_class}`, `trace_finish_evicted_total`, `trace_cancelled_total`, `trace_unfinished_total`, `catalog_refresh_coalesced_total`, `model_catalog` section with per-upstream `refreshedAt`, `state`, `errorKind`. (The `first_byte_*` and `compression_*` counters belong to the deferred Epic 6.) Only operation >100 ms is the catalog fan-out (bounded by a 10 s per-upstream timeout, off the request path).
- **Alerts**: no new alerts (local developer proxy).
- **Overhead checks**: Task 2.1.3f (one ring-buffer lock per request), Task 2.1.3g (SSE push latency < 100 ms from `finish` to receiver, covering the requirements SLO), and one **always-on, non-`#[ignore]` coarse regression test** (Task 2.1.3h: generous absolute bound with about 20x headroom over the expected cost, so it catches an accidental O(n^2), a blocking call or a per-attempt lock on every `cargo test` run without flaking) next to the release-only timing test. The compression budget and tee-overhead checks (Tasks 6.1.2d, 6.1.6) belong to the deferred Epic 6.

## Non-Functional Stances
- **Dashboard bind and auth** (VERIFIED in code, unchanged by this project): the server binds `127.0.0.1:{port}` only (`src/entrypoint/mod.rs:385`; `loopback_bind_is_reachable_only_via_localhost` at `:660` pins it), the dashboard routes, `/v1/dashboard/events` and `/requests/{id}` have no authentication, and CORS is `allow_origin(Any)` (`:260-263`). Consequence: any local process and any web page open in the user's browser can read SSE/GET responses and issue `POST /api/model-catalog/refresh`. Stance: loopback is the access boundary; no new auth (out of scope); the new surfaces expose no prompts, keys or upstream bodies (sentinel tests); refresh is bounded by single-flight, a 10 s throttle and the retry floor; the Models help text and config reference state it (Task 3.1.3g). Tightening CORS or adding a bearer token for the dashboard is backlog (Story 5.1.4), not applied.
- **Stored compressed prompt**: deferred with Epic 6 (not part of this project). When built: in memory only, 100 entries in lockstep with the ring buffer, stored only when `compressionStatus == compressed`, each at most `compression.max_body_bytes` (2 MB default; worst case about 200 MB at the default), never serialized into SSE, never written to disk, dropped on eviction (Story 6.1.4). The existing original-body store has the same 100-entry cap but no size cap; that is noted, not changed.
- **Hot path**: the always-on coarse test plus the release-only timing test, as in the Observability Plan.

## Risk Control
- **Feature flag**: trace/catalog changes ungated (additive). The compression flag (`compression.enabled`, ADR-005) belongs to the deferred Epic 6.
- **Rollback procedure**: standard revert via PR close + revert commit; epics are independently revertible (Epic 3 touches no trace code; Epic 4 stories are independent).
- **Staged rollout**: epics land as separate PRs in order 1 -> 2 -> 3 -> (5.1.3 exploratory) -> 4 -> 5 so each is releasable. Inside Epic 1, Story 1.1.3 (the `Router::dispatch` hotspot) is its own PR behind the characterization tests and the compat wrapper, and handler adoption (Story 2.1.6b) is a separate PR; see "Staged landing and gates". **Epic 1 boundary gate (product triad)**: before Epic 2 starts, Task 1.1.4i has Tyler use the fixed table and existing Inspect link for one working day of real traffic and record the result in `gap-report.md`; a "did not use it" result re-scopes Epic 2 before its 11.55 days are spent.
- **Scope cut order and schedule**: see "Effort and Appetite" below (single source of truth). Default: no cuts.
- **Decision R1 (for Tyler; accepted risk until he decides otherwise): cross-origin trigger of `POST /api/model-catalog/refresh`.** VERIFIED in code: CORS allows any origin, method and header (`src/entrypoint/mod.rs:260-263`, `CorsLayer::new().allow_origin(Any)...`) and the server binds `127.0.0.1` (`:385`). So any web page open in Tyler's browser can send the new refresh POST to the loopback port, and the preflight (if any) passes. Consequence: it can start each upstream's credential helper (SSO or AWS-chain prompts) and the listing calls (possibly paid). It cannot read anything new (responses carry no prompts, keys or upstream bodies; sentinel tests). Mitigations already in the plan: per-upstream single-flight (RAII guard), 10 s manual throttle per upstream, 5-minute retry floor for automatic refreshes, auth-failure backoff, bounded 10 s per-upstream timeout, no startup/hot-swap/GET-triggered refresh, and the Models help text that states the exposure (Task 3.1.3g). They bound the rate of triggers, not who can trigger. **Not applied (options for Tyler)**: (a) reject the POST when an `Origin` header is present and is not the dashboard's own origin (about 0.2 d inside Task 3.1.4b, not in the 48.4-day total); (b) tighten CORS to same-origin, which could affect other SSE/API consumers (backlog, Story 5.1.4); (c) a dashboard token (out of scope). **Default if Tyler is silent: accept the risk as documented.** Also listed in "Unresolved Questions".


## Effort and Appetite

The approved appetite is ~9 weeks = 45 engineer-days (requirements.md, extended from 3-6 weeks by Tyler on 2026-10-09). **Tyler decided on 2026-10-10: commit to Epics 1-5 with no cuts; Epic 6 is a deferred follow-up and is excluded from every total, schedule and cut order below.** Estimates are ideal engineer-days per story (1 week = 5 days). Every sum was recomputed after the final-pass triad repair.

| Epic | Stories (days) | Epic total |
|------|----------------|-----------|
| 1 Trace truth | 1.1.1 spikes + legacy-consumer grep 0.65; 1.1.2 contract+fixture, stale-bundle check 0.2, cut-able dual-emit rollback 0.3 = 1.5; 1.1.3 TraceScope terminal trace, finish contract, poison-safe drop, test migration, characterization tests, wrapper-adoption check and real-socket client-abort e2e (Task 1.1.3k, 0.4) 4.9; 1.1.4 table UI (incl. skeleton, empty-state URL, clipboard with fallback, latest-100 footer, visible n/a reasons without per-cell tab stops, coverage note, Epic 1 validation record 0.1) 1.95; 1.1.5 fake upstream harness (delays, retry-after, request recorder) 1.5 | **10.5** |
| 2 Requested -> routed, attempts, Inspect | 2.1.1 types (incl. attempt error class) 0.9; 2.1.2 capture + in-flight attempt 1.7; 2.1.3 finish + single DTO builder + always-on coarse perf test 2.05; 2.1.4 row cell + chip legend 1.2; 2.1.5 drawer (selectable attempt, badge scroll, swap announcement, narrow-viewport sheet with focus containment, Why block, Not-recorded block, evicted snapshot) 3.1 (bars 0.5 of it); 2.1.6 trace units: handler wrapper, emulation loop, cache hit, local, counter semantics, coverage-note removal 2.6 | **11.55** |
| 3 Model catalog | 3.1.1 0.75; 3.1.2 0.5; 3.1.3 refresh + named providers builder + prune + RAII single-flight/retry floor + per-upstream + NFR stance docs 3.5; 3.1.4 endpoints 1.2; 3.1.5 UI (explicit refresh, full state set) 2.8; 3.1.6 config warnings + unchecked-targets caveat 1.1 (never cut: UX-04 is Must) | **9.85** |
| 4 Gap fixes (re-ranked after exploratory pass) | 4.1.0 re-rank 0.5; 4.1.1 1.0; 4.1.2 1.5; 4.1.3 1.5; 4.1.4 1.0; 4.1.5 1.6 (core 0.75 + rest 0.85); 4.1.6 1.0; 4.1.7 a11y 3.1 (7a 0.3, 7b 0.6, 7c 0.6, 7d 0.3, 7e 0.3, 7f 0.7, 7g 0.3; all never cut) | **11.2** |
| 5 Prove it | 5.1.1 e2e (attempt-counter scenarios; client abort moved to 1.1.3k) 1.8; 5.1.2 puppeteer 0.5; 5.1.3 exploratory (incl. M6 timed walk-throughs, the value-risk usage check, keyboard, forced-colors and zoom passes; never cut) 1.5; 5.1.4 report + quick wins 1.5 | **5.3** |
| **Total (Epics 1-5)** | | **48.4 days (~9.7 weeks single worker)** |

Recomputed: 10.5 + 11.55 + 9.85 + 11.2 + 5.3 = 48.4. Reconciliation with the 46.65-day Epics 1-5 figure of the earlier draft: +1.75 = Epic 1 +0.6 (0.4 abort e2e pulled forward from Epic 5, 0.1 Epic 1 validation record, 0.1 n/a-reason rework); Epic 2 +0.45 (drawer Why/Not-recorded block 0.2, evicted snapshot 0.1, narrow-sheet focus containment 0.15); Epic 3 +0.1 (unchecked-targets caveat AC and test); Epic 4 +0.8 (stale-row contrast 0.1, UX-23 auto-hold mechanism 0.2, forced-colors/non-text-contrast/search-name checks 0.2, reflow task 0.3); Epic 5 -0.2 (abort scenario moved to Task 1.1.3k). The task splits requested by the engineering triad (1.1.3d2, 1.1.3e, 3.1.3e, 4.1.2b, 4.1.3) re-slice existing time and change no story total. Epic 6 (9.9 days) is not in any figure above; its own total is kept in its section.

**This is 3.4 days (~0.7 weeks, 7.6%) over the approved ~9-week (45-day) appetite.** (Tyler's 2026-10-10 expectation was about 0.3 weeks over, i.e. the 46.65-day baseline; the final-pass UX, engineering and product findings add 1.75 days. Stated plainly, not absorbed.) Wall-clock with a second worker on Epic 3 is INFERRED at roughly 8-9 weeks; not verified. The cut table below is a contingency, not applied by default.

**Never cut (a11y and Must acceptance criteria)**: Story 4.1.7 in full (UX-18 focus ring and target size, UX-22/UX-23 live region and held updates, UX-24 contrast incl. dimmed rows, UX-27 reduced motion, reflow, forced-colors and non-text-contrast checks), Story 3.1.6 (the UX-04 pinned-model block and its test), and **Story 5.1.3** (the exploratory pass feeds Story 4.1.0, measures M6, performs the value-risk usage check and the manual keyboard, screen-reader, forced-colors and zoom passes, and produces the M1 screenshot; without it the success metrics are unproven). **Why a11y is justified for a single-operator loopback tool (product triad)**: the sole operator is Tyler, who works keyboard-first in a dark theme, so the Must items map to his own use: a visible focus ring and 24x24 targets (UX-18), legible text (UX-24: the repo's neutral-500/600 text fails 4.5:1, computed in `research/ux.md`), no row jumping under focus (UX-23) and no animation under reduced motion (UX-27). The breadth items that are not Must (the extra checks in Task 4.1.7f2, the reflow Task 4.1.7g) stay in the no-cuts baseline but are the first descope candidates if the Must items slip, ahead of any Epic 1-3 story.

**Explicit cut order (contingency only; first dropped first); remaining total after each cut:**

| # | Cut | Saves | Remaining |
|---|-----|-------|-----------|
| - | (full plan, no cuts: the default) | - | 48.4 |
| 1 | Task 1.1.2g dual-emit rollback (`compat_snake_case`); rollback becomes revert-only, the 1.1.1g grep stays the protection | 0.3 | 48.1 |
| 2 | Story 5.1.2 Puppeteer smoke | 0.5 | 47.6 |
| 3 | Waterfall bars (Task 2.1.5c); keep ordered attempt list (also cut first if the Epic 1 validation or the 5.1.3 usage check shows the waterfall is not used) | 0.5 | 47.1 |
| 4 | Story 4.1.6 trace backfill | 1.0 | 46.1 |
| 5 | Story 4.1.5 non-core part (`ConfigChanged` refresh and the Recent errors list with its empty/overflow states). The disconnected banner, Reconnect button and `system_lag` chip (4.1.5 core, UX-11/UX-16) stay | 0.85 | 45.25 |
| 6 | Story 4.1.4 real provider health + tick | 1.0 | 44.25 |
| 7 | Story 4.1.3 Config Editor fixes (hide rate-limit editing instead) | 1.5 | 42.75 |

Never cut: Stories 1.1.1-1.1.5 (Task 1.1.2g aside), 2.1.1-2.1.6 (waterfall bars aside), 3.1.1-3.1.6, 4.1.0-4.1.2, 4.1.5 core, 4.1.7 in full, 5.1.1, 5.1.3-5.1.4. Cuts 1-5 bring the plan to 45.25 days (0.25 over); cuts 1-6 to 44.25 (inside the appetite); cuts 1-7 are the floor at 42.75 days. Each cut lands its items in the gap-report backlog with severity.

**Schedule (single worker, no cuts; cumulative days / weeks at 5 d/wk)**: Epic 1 10.5 d (wk 2.1), then the Epic 1 validation day (calendar time, no engineering effort); Epic 2 -> 22.05 d (wk 4.4); Epic 3 -> 31.9 d (wk 6.4); Story 5.1.3 exploratory pass -> 33.4 d (wk 6.7); Epic 4 incl. re-rank -> 44.6 d (wk 8.9); Stories 5.1.1, 5.1.2, 5.1.4 (1.8 + 0.5 + 1.5 = 3.8 d) -> 48.4 d (wk 9.7). **Parallelism (corrected)**: a second worker can take Epic 3 after Story 1.1.2 for the Rust-independent parts (Stories 3.1.1 and 3.1.2, Tasks 3.1.3a1, 3.1.3a2, 3.1.3d, 3.1.3f). **Task 3.1.3b (named providers: `Router::provider_names`, `with_provider_names`, `providers_named`, the `from_config` call) is sequenced after Task 2.1.2d**: both edit `src/routing/router/mod.rs`, and Task 2.1.2d's "router file does not grow net" line-count AC would otherwise be measured against a moving file and conflict on rebase (if a second worker must start earlier, the stated risk is a manual rebase of about 20 lines in `router/mod.rs` plus a re-baselined line count, not a design change). Everything in Epic 3 that needs `providers_named()` (3.1.3c1, 3.1.3e, 3.1.4, 3.1.5, 3.1.6) therefore waits for 2.1.2d as well; `events.rs` (Task 3.1.3f vs Tasks 2.1.1c and 2.1.3c) is additive on both sides and expected to rebase cleanly. The resulting wall-clock is INFERRED, not measured.


**Staged landing and gates (pre-mortem P1-1 and P1-2)**: each epic is an independently shippable PR whose Column Truth Table state is honest at its own boundary (and, with Epic 6 deferred, at the end of the project too); the epic's PR is not opened until its gates pass. Story 1.1.3 lands as its own PR: it contains the characterization tests (Task 1.1.3i, written and committed first), the `TraceScope`, and the `Router::dispatch` compat wrapper, and it leaves every ingress handler and every `dispatch` caller untouched (handlers adopt `TraceScope` in Story 2.1.6b, its own PR). There is no runtime feature flag on `dispatch` (a dual code path in the routing hotspot would double what the characterization tests must cover); revert of the 1.1.3 PR is the rollback.

## Unresolved Questions
- [ ] Does `ResolvedModel` (family route) reach the router cheaply? - blocks Task 2.1.3d only (optional) - owner: Task 1.1.1c
- [ ] What do Bedrock/Gemini/OpenRouter/Anthropic `list_models` return in practice (error vs static vs live; id shape such as `models/` prefix)? - blocks Story 3.1.6 comparison rules - owner: Task 1.1.1b
- [ ] Can Karma import the shared JSON fixture from `ui/src/testing/fixtures/` under `tsconfig.spec.json`, and does the Bazel BUILD / any packaging glob include `ui/src/testing`? (`Cargo.toml` has no `include`/`exclude`, verified by grep) - blocks Task 1.1.2c - owner: Task 1.1.1e
- [ ] Is a raw live SSE frame snake_case exactly as inferred? - blocks Task 1.1.2a (fixture content) - owner: Task 1.1.1a
- [ ] Rate-limit editor: persist or hide? Default = hide/read-only (Story 4.1.3) - owner: Tyler
- [ ] Catalog refresh default: `on_demand` (**the Models view does not refresh on open**: it shows the cached snapshot with its age and an explicit "Refresh now" button; `GET` never refreshes; the optional `?ifStale=true` POST stays server-side for interval mode and scripts; single-flight, 5-minute retry floor from the last attempt; manual button bypasses the floor; no startup or hot-swap fan-out). Optional `interval` mode min 300 s. Default applies if Tyler is silent - owner: Tyler
- [ ] Staleness threshold for the catalog badge (default 30 min in on-demand mode, 2 x interval in interval mode) - owner: Tyler, default applies if silent
- [x] Appetite and scope: decided by Tyler on 2026-10-10: Epics 1-5 with **no cuts**, Epic 6 deferred to a follow-up project. Default is no cuts; the plan is 48.4 days against the approved 45 (3.4 days, ~0.7 weeks over; see Effort and Appetite). Tyler may still apply cuts 1-6 (44.25 days) if he wants to land inside 45 - owner: Tyler
- [ ] **Decision R1** (accepted risk, see Risk Control): cross-origin trigger of `POST /api/model-catalog/refresh` with CORS `Any` on a loopback port can start credential helpers and paid listing calls. Accept as documented (default if silent), or add the `Origin` check (about 0.2 d in Task 3.1.4b, not in the 48.4 total) - owner: Tyler
- [ ] Epic 1 boundary validation (Task 1.1.4i): does Tyler use the fixed table plus existing Inspect link on one day of real traffic? A "no" re-scopes Epic 2 - owner: Tyler, answer recorded in `gap-report.md`

**Deferred follow-up questions (Epic 6; not blocking this plan)**:
- [x] **Compression default** (`compression.enabled`): decided `false` (ADR-005; compression rewrites prompts and may inject a tool, so it is opt-in). Tyler may flip it when the follow-up lands.
- [ ] **`rewind_retrieve` tool injection**: `engine.rs:513-526` unconditionally adds the tool; a model calling it returns a `tool_use` the client cannot execute. Decision (spike 6.1.1): add an engine option to skip injection, or handle the call in the proxy. Default if undecided: skip injection; the spike must also show the dispatched `tools` is unchanged, the marker text does not reference an absent tool, and `should_emulate` is evaluated on the original body (Story 6.1.1 ACs) - owner: Task 6.1.1
- [ ] Compression defaults `timeout_ms`, `max_body_bytes` (2 MB), `max_concurrent` (4): proposed, final values come from the 6.1.1 baseline - owner: Task 6.1.1

## Dependency Visualization

```
1.1.1 verify spikes ──┬──> 1.1.2 contract+fixture ──> 1.1.3 TraceScope terminal trace ──> 1.1.4 trace table UI
                      │                                      │                                   │
1.1.5 fake upstream ──┤                                      v                                   v
 (week 1, reused by   │                           2.1.1 trace types ──> 2.1.2 capture ──> 2.1.3 finish+DTO ──> 2.1.4 route cell ──> 2.1.5 drawer
  2.1.5e, 3.1.4, 5.x) │                                                                   └──> 2.1.6 trace units (loop/cache/local)
                      │
                      └──(1.1.1b)──> 3.1.1 catalog core ─> 3.1.2 sanitizer ─> 3.1.3 named providers+refresh ─> 3.1.4 endpoints ─> 3.1.5 UI ─> 3.1.6 warnings
                                           (3.1.1, 3.1.2, 3.1.3a/d/f need only 1.1.2; 3.1.3b needs 2.1.2d (same router file); the rest of 3.1.3, 3.1.4-3.1.6 follow 3.1.3b)

5.1.3 exploratory pass (needs Epic 1, 2, 3 built) ──> 4.1.0 re-rank ──> Epic 4 stories (all need 1.1.2; independent of each other except noted)
 4.1.1 replay/inspect link (needs 2.1.5)   4.1.2 remove fabricated data (needs 2.1.6 for error rate)   4.1.3 config editor
 4.1.4 real health+tick (needs 1.1.2)      4.1.5 SSE events/reconnect ──> 4.1.6 trace backfill (needs 2.1.3)   4.1.7 a11y (last; touches all)

Epic 1 extras: 1.1.5a1 (Script::hang) ──> 1.1.3k real-socket client-abort e2e (needs 1.1.3d/e wrapper; asserts 499 + trace_cancelled_total now, the cancelled attempt after 2.1.2b2)
                1.1.4 table ──> 1.1.4i one day of real use by Tyler ──> gate: go / re-scope Epic 2

Epic 6 (compression + first byte): DEFERRED follow-up project, no node above depends on it. Its internal graph (front half needs only 1.1.5; back half needs 2.1.6b and 2.1.3) is kept in its own section.

Epic 5: 5.1.1 Rust e2e (needs 2.1.3, 2.1.6, 3.1.4; the abort scenario already ran in 1.1.3k) ─> 5.1.2 puppeteer smoke ─> 5.1.4 gap report ─> fixes
```

---

## Phase 1: Trace truth (Epic 1)

### Epic 1.1: Fix the trace contract and report real status/failure traces
**Goal**: Every row shows real request id, latency (to headers), tokens and status; failed, exhausted and cancelled requests appear; unknowns render per the Column Truth Table instead of 0 or `undefined`. **Effort: 9.9 days.**

#### Story 1.1.1: Verification spikes and baseline (do first; 0.65 day)
**As a** developer, **I want** the INFERRED claims in research confirmed by running code, **so that** later stories do not build on a guess.
**Acceptance Criteria**:
- A raw SSE `RequestTrace` frame and `MetricsTick` frame are captured from a running binary and attached to the plan folder.
  - *Given* a local `consolette` started with a config containing one fake upstream (from Story 1.1.5, or a throwaway axum fake if 1.1.5 is not yet merged) and `CONSOLETTE_REQUIRE_UI=1`, *When* `curl -N http://127.0.0.1:<port>/v1/dashboard/events` runs while one `POST /v1/messages` with `model: "claude-haiku-4-5"` succeeds, *Then* the saved frame shows keys `request_id`, `duration_ms`, `first_byte_ms`, `status_code: 200` (snake_case confirmed) and no `id` key.
- Each `list_models` implementation's real behavior is recorded.
  - *Given* the five adapters in `src/providers/{anthropic.rs:574,bedrock.rs:904,gemini/mod.rs:497,openai/mod.rs:865,openrouter/mod.rs:656}`, *When* each is read (and live-called where credentials exist), *Then* `research/spike-notes.md` (scratch) lists per adapter: error-vs-list, id shape, size, and whether `list_models` triggers a credential helper/SSO/AWS default chain (input to the refresh-mode decision, ADR-003).
- Family-resolved id availability is decided (yes/no within <= 3 files).
- Status-mapping reuse is confirmed.
  - *Given* `entrypoint/errors.rs` (`map_provider_error_anthropic`/`_openai`), *Then* the note records whether reuse for `trace_status` is possible (expected: no, ingress-specific).
- Legacy consumers of the SSE payload are enumerated before the contract changes (hard precondition for Task 1.1.2b).
  - *Given* `grep -rnE "request_id|duration_ms|first_byte_ms|tokens_before|status_code" src tests` and the `/v1/dashboard/events` consumer grep from ADR-001, *When* the hits are triaged, *Then* `research/spike-notes.md` lists each hit as producer, `/metrics` REST consumer (for example `src/dashboard.rs:540-546`, unaffected because `/metrics` stays additive) or SSE consumer; **zero SSE consumers other than `ui/src/app/core/services/sse.service.ts`** is the pass condition, and a hit on `src/mcp_gateway.rs` or any other SSE reader blocks Task 1.1.2b until migrated.
- Compression absence is re-confirmed (the compression design questions are recorded for the deferred Epic 6 follow-up, Story 6.1.1; nothing here depends on them).
  - *Given* `grep -rnE "CompressionEngine|compress_request" src --include='*.rs'`, *Then* zero hits outside `src/compression/` and `src/bin/` (VERIFIED at plan-revision time: the engine is not constructed anywhere in the server).
**Files**: `project_plans/webui-trace-model-mapping/research/spike-notes.md` (new, scratch), `project_plans/webui-trace-model-mapping/research/sse-frames.txt` (new)

Tasks (each is a sub-hour to half-day step; total 0.65 day):
- **1.1.1a** Capture live SSE frames: `CONSOLETTE_REQUIRE_UI=1 cargo run -- <serve subcommand> --config <tmp config>`; send one request; save `curl -N .../v1/dashboard/events | head -c 4000` to `research/sse-frames.txt`; confirm snake_case, no `id`, `provider_health` all `"healthy"`.
- **1.1.1b** Record `list_models` behavior per adapter in `research/spike-notes.md`; Bedrock comparison key is `normalize_model` (test `normalize_model_strips_region_and_vendor_prefix`); note credential side effects.
- **1.1.1c** Decide `resolvedModel` feasibility: trace `resolve_family` callers in `src/providers/openai/mod.rs` (call near `:705`) and `ProviderResponse` (`src/providers/mod.rs`); yes/no + diff sketch.
- **1.1.1d** Compare `ProviderError` variants (`src/providers/mod.rs:36-65`, including `RateLimitedWithRetry`) to `map_provider_error_*`; note differences (Exhausted: 503 openai vs 529 anthropic).
- **1.1.1e** Karma fixture import + stale-bundle check: add `"resolveJsonModule": true` in `ui/tsconfig.spec.json` if needed; throwaway spec importing a JSON under `ui/src/testing/fixtures/sse/`; `CONSOLETTE_REQUIRE_UI=1 cargo build`; record `main-*.js` hash; confirm debug builds embed vs read from disk (`src/dashboard.rs`); check `BUILD.bazel` globs for `ui/src/testing`.
- **1.1.1g** (0.15 d, **hard precondition of 1.1.2b**) Legacy SSE consumer grep per the AC above; record in `research/spike-notes.md`; if any non-UI SSE consumer exists, promote Task 1.1.2g (dual-emit) from cut-able to must-have.
- **1.1.1f** Hotspot confirmation: `git log --since=6.months --name-only | sort | uniq -c | sort -rn | head`; amend Tech Debt table if ranking differs.

#### Story 1.1.2: One contract, pinned by a shared fixture (ADR-001; 1.5 days: 1.0 core + 0.2 stale-bundle check + 0.3 cut-able dual-emit)
**As an** operator, **I want** the dashboard to read the fields the backend actually sends, **so that** the trace table is populated.
**Acceptance Criteria**:
- All four SSE structs serialize camelCase; `RequestTraceData` has `id` (== `requestId`), `compressionStatus` (`off|none|skipped|failed|compressed`), `stream: bool`, `traceKind`, `tokensAfter: Option`, `firstByteMs: Option`, `firstByteState: Option` (`pending|measured|ended_empty|client_closed`), `traceRevision: u8` (0 or 1), `totalMs: Option`, `statusCode: u16`. In this project the backend only ever sends `compressionStatus: "off"`, `firstByteMs: null`, no `firstByteState` and `traceRevision: 0`; the fields exist so the deferred Epic 6 needs no contract change.
  - *Given* a `RequestTraceData { request_id: "req_123", time_to_headers_ms: 42, first_byte_ms: None, tokens_before: 1870, tokens_after: None, compression_status: Off, stream: false, trace_kind: Routed, status_code: 200, .. }`, *When* serialized inside `DashboardEvent::RequestTrace`, *Then* the JSON equals `ui/src/testing/fixtures/sse/request-trace-success.json` (`{"type":"RequestTrace","data":{"id":"req_123","requestId":"req_123","timeToHeadersMs":42,"firstByteMs":null,"tokensBefore":1870,"tokensAfter":null,"compressionStatus":"off","stream":false,"traceKind":"routed","statusCode":200,...}}`).
- The UI parses the same fixture into a populated view-model.
  - *Given* that fixture fed through `SseService` into `MetricsStore`, *When* `traces()` is read, *Then* the first item has `requestId === "req_123"`, `timeToHeadersMs === 42`, `statusCode === 200` (no `undefined`).
- Upsert never regresses a row.
  - *Given* a stored trace `req_123` with `traceRevision: 1, firstByteMs: 450` and a later-arriving event for `req_123` with `traceRevision: 0, firstByteMs: null`, *When* `MetricsStore.pushTrace` runs, *Then* the stored row is unchanged (equal-or-higher revision wins; the same rule governs the Story 4.1.6 backfill merge).
- UI models for `MetricsTick`, `ErrorLogged`, `ConfigChanged` match the backend names.
  - *Given* fixture `metrics-tick.json` with `totalRequests: 120, totalTokensSaved: 0, currentLagMs: 3.5`, *When* parsed, *Then* `MetricsStore` exposes `totalRequests() === 120` and `isContended()` reads `currentLagMs`.
- Every `DashboardEvent` variant serializes to non-empty JSON.
  - *Given* one instance of each variant, *When* `serde_json::to_string` is called, *Then* each result is non-empty and contains `"type"`.
- A stale embedded UI cannot pass CI (Task 1.1.2f).
  - *Given* `make ui-fresh-check` (builds with `CONSOLETTE_REQUIRE_UI=1`, then runs `tests/ui_bundle_fresh.rs`, which fails if any file in `build.rs`'s input list is newer than the embedded `index.html` or the bundle is the placeholder page), *When* a UI input changes without rebuilding, *Then* the test fails naming the stale input; the CI `test` job (already `CONSOLETTE_REQUIRE_UI: "1"`, `.github/workflows/ci.yml:16`) gets a `ui-fresh-check` step after `cargo test`, so a green Rust fixture test with a red or stale UI cannot merge.
- Dual-emit rollback is a concrete task, not a promise (Task 1.1.2g, cut-able, cut #1).
  - *Given* `compat_snake_case: true` in config, *When* a `RequestTrace` is serialized, *Then* the payload carries the camelCase keys plus the legacy snake_case keys (`request_id`, `duration_ms`, `first_byte_ms`, `tokens_before`, `tokens_after`, `status_code`); *Given* the default `false`, *Then* only camelCase keys appear (fixture-pinned).
- The fixture is reachable by every build path.
  - *Given* `cargo test` and the Bazel spike's file globs, *Then* `include_str!("../../ui/src/testing/fixtures/...")` resolves (Task 1.1.1e finding).
**Files**: `src/entrypoint/events.rs`, `src/metrics/mod.rs`, `ui/src/app/core/models/telemetry.models.ts`, `ui/src/app/core/stores/metrics.store.ts`, `ui/src/testing/fixtures/sse/*.json`, `ui/src/app/core/services/sse.service.spec.ts`, `ui/src/app/core/stores/metrics.store.spec.ts`

Tasks:
- **1.1.2a** (precondition: Task 1.1.1g clean) Write failing Rust fixture test in `src/entrypoint/events.rs` tests: replace `request_trace_event_serialization` with fixture-equality tests (`serde_json::Value` compare), add the all-variants non-empty test, create fixture JSON files (success trace, failure trace with `attempts`, metrics tick, error logged, config changed) from the 1.1.1a frame renamed to camelCase.
- **1.1.2b** Rust struct changes: `#[serde(rename_all = "camelCase")]` on the four structs; add `id`, `compression_status`, `stream`, `trace_kind`, `total_ms`; `first_byte_ms`/`tokens_after` become `Option<u64>`; rename `duration_ms` -> `time_to_headers_ms`; introduce `RequestTraceData::from_detail(&RequestDetail)` as the only constructor (replacing the inline build at `src/metrics/mod.rs:282-293`; still status 200 for now).
- **1.1.2c** Failing UI fixture specs in `sse.service.spec.ts` and `metrics.store.spec.ts`.
- **1.1.2d** Align UI models and store (`telemetry.models.ts`; `MetricsStore.pushTrace` becomes upsert-by-`requestId` with the `traceRevision` rule above, capped at `maxTracesLimit`).
- **1.1.2e** Build-and-see: `CONSOLETTE_REQUIRE_UI=1 cargo build`, run, send one request, confirm `main-*.js` hash differs from 1.1.1e and the row is non-blank; record in PR.
- **1.1.2f** (0.2 d) `make ui-fresh-check` target, `tests/ui_bundle_fresh.rs`, CI step; document in the Makefile header comment.
- **1.1.2g** (0.3 d, **cut-able**) `compat_snake_case` config key and dual-emit serializer for `RequestTraceData` only, with the fixture test for both settings; `docs`: one release-note line. If cut, ADR-001 item 4 reads "revert-only rollback".

#### Story 1.1.3: Terminal trace for every request, surviving cancellation (ADR-002; 4.9 days)
**As an** operator, **I want** failed, exhausted and abandoned requests to appear with a real status, **so that** I can see what went wrong instead of an always-200 table.
**Acceptance Criteria**:
- Pre-step, before any production change (pre-mortem P1-2): a characterization test pins today's `Router::dispatch` behavior.
  - *Given* the fake providers in `src/routing/router/tests.rs` and the scenarios `all candidates ok`, `first upstream 529 then second ok`, `all upstreams rate limited` (cooldown above `MAX_HOLD_WAIT_SECS`), `rate-limit hold then success` (paused time), `validation error`, `session-pinned route`, `exploration pick` and `local-catalog model`, *When* `dispatch` runs against the **unmodified** code, *Then* a new `src/routing/router/tests/characterization.rs` records and asserts, per scenario, the provider chosen (name), `selected_model`/`selected_model_was_exploration` in the ring-buffer entry, number of upstream calls (retries), `hold_retries`, the returned `Ok`/`Err` variant and the `last_error` kind. The file is committed first and green before Task 1.1.3d starts; it is **never edited** in the same PR as the refactor (a changed expectation needs its own reviewed commit with a reason). The tests call the public `dispatch` signature, so they run unchanged through the compat wrapper.
- Wrapper adoption is checked mechanically (pre-mortem P1-2).
  - *Given* `tests/trace_scope_adoption.rs` scanning `src/**/*.rs` with `std::fs`, *When* it runs, *Then* every call to `Router::dispatch`, `dispatch_traced` and every `CostTrackingStream::new` in `src` (the 3 production sites) outside an allowlist (the compat wrapper itself, `#[cfg(test)]` code, `src/bin/`) is inside a function that also constructs or receives a `TraceScope`; adding a new ingress handler or stream site without one fails the test with the file and line. The allowlist is a literal list in the test, so widening it is a visible diff.
- Epic 1 boundary is stated, not hidden (engineering triad 6).
  - *Given* this PR alone, *Then* the story's Notes and the Column Truth Table state which paths have no trace until Story 2.1.6b: 400/translation errors raised in the handlers before `dispatch`, cache hits, local requests that bypass `dispatch`, and the per-round split of an emulation request (one row per `dispatch` call, each carrying its own `requestId`); the UI shows the coverage note from Story 1.1.4 for exactly that reason, and a test asserts that a handler-level 400 produces **no** trace at this boundary (so the note is true) and that Story 2.1.6b flips it.
- Real-socket client abort is proven in week 1-2, not in Epic 5 (engineering triad: ADR-002's load-bearing assumption is that axum drops the handler future when the client disconnects).
  - *Given* the Story 1.1.5 fake upstream with `Script::hang()` (never sends headers) and the real server on an OS-assigned port, *When* a raw test client sends `POST /v1/messages` and aborts the connection after 200 ms, *Then* within 2 s exactly one `RequestTrace` arrives on the SSE stream with `statusCode: 499`, the ring buffer has one entry for it, `trace_cancelled_total == 1` and `trace_unfinished_total == 0` on `/metrics`. This runs through the `Router::dispatch` compat wrapper at the Epic 1 boundary (the handler is untouched, but dropping its future drops the `dispatch` future and the scope inside it). The assertion that the attempts are exactly `[<upstream>/cancelled]` with `latencyMs > 0` is added to the same test when Task 2.1.2b2 lands (Story 2.1.2 owns the pending attempt). If this test fails, ADR-002 is wrong and the design is revisited before Epic 2.
- Staged landing: this story is its own PR and leaves ingress handlers untouched.
  - *Given* the PR diff, *Then* `src/entrypoint/messages/mod.rs`, `chat_completions.rs` and every `CostTrackingStream::new` site (VERIFIED by grep at `main` 49ec827: 7 in `src` = 3 production at `chat_completions.rs:73`, `messages/mod.rs:235` and `:491`, plus 4 in `cost_tee.rs` tests; plus 1 in `tests/server_tool_emulation.rs:500`) are not modified (handler adoption is Story 2.1.6b, a separate PR); every existing `dispatch` caller keeps its signature and gets its trace from the compat wrapper; the pre-existing router tests pass without edits except (1) deleting tests of the removed `set_selected_model*` setters and (2) the `selected_model`/`observability` migrations in Task 1.1.3f, each of which must also pass against the characterization recordings. The per-handler bullets below (400 before dispatch, translation error, emulation round-2 failure) are proven here at the wrapper level with a test-double handler and again per real handler in Story 2.1.6b.
- `ProviderError::trace_status()` maps every variant.
  - *Given* `RateLimited` and `RateLimitedWithRetry{..}` -> 429; `Exhausted` -> 503; `Timeout` -> 504; `Auth(_)` -> 401; `Validation(_, 422)` -> 422; `Upstream{status: 0, ..}` -> 502; `Upstream{status: 529, ..}` -> 529; `ResponseShapeMismatch(_)` -> 502; `ModelUnsupported(_)` -> 404. The test enumerates variants through an exhaustive helper so a new variant fails compilation.
- `AttemptOutcome::from(&ProviderError)` mapping is specified (exhaustive): `RateLimited`/`RateLimitedWithRetry` -> `RateLimited`; `Timeout` -> `Timeout`; `Auth` -> `Auth`; `Validation`/`ModelUnsupported` -> `Validation`; `ResponseShapeMismatch` -> `ShapeMismatch`; `Upstream` -> `Upstream`; `Exhausted` -> unreachable as an attempt (mapped to `Upstream` defensively, covered by test).
- `AttemptOutcome::from(&ProviderError)` lives in `src/providers/mod.rs` (not in `metrics/trace.rs`).
  - *Given* `grep -nE "crate::(providers|routing)" src/metrics/trace.rs`, *Then* no hit.
- A trace is emitted on every terminal path of one `dispatch` (compat wrapper) call.
  - *Given* a `Router` with two fake upstreams both returning `RateLimited` under `#[tokio::test(start_paused = true)]` with the rate-limit cooldown set above `MAX_HOLD_WAIT_SECS` (so `hold_for_rate_limit_cooldown`, `router/mod.rs:707-718`, does not wait ~45 s), *When* `dispatch` is called with `model: "claude-sonnet-4"`, *Then* exactly one `RequestTrace` arrives with `statusCode: 429` (`dispatch` returns `last_error`, here `RateLimited`) and `requestId` equal to the ring-buffer entry id.
  - *Given* one upstream returning `Validation(_, 400)`, *Then* one trace `statusCode: 400` and the caller still receives the `Err`.
  - *Given* one upstream returning Ok, *Then* exactly one trace per client request, `statusCode: 200` (not two).
- Finish contract (type-enforced, ADR-002 item 3): `finish`/`finish_cache_hit`/`finish_error` are the only normal exits. **The wrapper owns the scope as a local variable; nothing else ever owns it.** Handlers are split into a thin wrapper and an inner `async fn(&mut TraceScope, ..) -> HandlerResult { response, outcome }` that only borrows it (no `Arc`, no clone; the emulation loop borrows it through `ScopedDispatch`, Story 2.1.6). The wrapper runs, in this order: `let result = inner(&mut scope, ..).await; scope.finish(result.outcome); scope.mark_handler_returned();` and then returns `result.response`; the scope is dropped after `finish` (state `Finished`, `Drop` emits nothing). If axum drops the wrapper future before `finish`, the inner future and every borrow drop first (locals drop in reverse order), then the scope drops with `handler_returned == false` and emits `cancelled`. Every return path (400 validation, translation error, emulation round-2 failure, a failed `run_emulation_loop`) carries an outcome by type.
  - *Given* a handler that returns a 400 body before dispatch, *Then* one trace with `statusCode: 400`, not 499; same for an emulation round-2 dispatch failure and a `chat_completions` translation error (one test per handler).
- `Drop` distinguishes three cases.
  - *Given* `ScopeState::Finished`, *Then* `Drop` emits nothing.
  - *Given* an unfinished scope whose handler has **not** returned (future dropped by axum), *Then* exactly one `RequestTrace` with `statusCode: 499`, ring-buffer entry updated; `trace_cancelled_total == 1`; if an attempt was in flight (Story 2.1.2 pending attempt) it is recorded as one `cancelled` attempt with its elapsed latency, otherwise the attempt log is unchanged.
  - *Given* an unfinished scope with `handler_returned == true`, or `std::thread::panicking()` (handler panic), *Then* exactly one trace with `statusCode: 500`, `routeReason` unchanged, **no** `cancelled` attempt, `error!` logged, `trace_unfinished_total == 1` and `trace_cancelled_total` unchanged; in `cfg(test)` an extra `debug_assert` hook lets tests assert it is never reached by the real handlers.
- Cancellation emits a terminal trace.
  - *Given* a request in `hold_for_rate_limit_cooldown` (paused time, all candidates cooling down), *When* the `dispatch` future is dropped mid-hold, *Then* the 499 outcome above with the earlier failed attempts unchanged and no `cancelled` attempt (nothing is in flight during a hold sleep).
  - *Given* an upstream that never responds (`Script::hang()` / a never-ready test provider) under paused time, *When* the `dispatch` future is dropped after 5 s of virtual time, *Then* one trace `statusCode: 499` whose attempts are exactly `[upstream/cancelled]` with `latencyMs >= 5000` (Story 2.1.2).
  - *Given* a handler future dropped after `TraceScope::begin` but before any dispatch (for example while awaiting compression), *Then* the same single cancelled trace (no leaked entry without a terminal trace).
- Finishing then dropping never double-emits.
  - *Given* `finish` called then the scope dropped, *Then* exactly one event.
- `Drop` is poison-safe and never panics (a panic in `Drop` during unwinding aborts the process).
  - *Given* the ring-buffer or `event_tx` `std::sync::Mutex` poisoned by a panicking thread, *When* an unfinished scope drops, *Then* `Drop` uses `lock().unwrap_or_else(PoisonError::into_inner)`, still emits, and does not panic.
  - *Given* an inner function that starts the emulation loop (so `ScopedDispatch` borrows the scope) and then returns a failure outcome, *Then* the wrapper's `finish` emits exactly one trace with that status and the loop's attempts, never `unfinished` or `cancelled`; *Given* the wrapper future is dropped while `ScopedDispatch` is mid-dispatch, *Then* exactly one `cancelled` trace is emitted after the borrow has ended (the scope cannot be moved into a spawned task: the borrow is not `'static`).
  - *Given* a scope finished at response headers for a stream, then the consumer drops the body mid-stream, *Then* no second terminal or `cancelled` trace (a stream-level first-byte outcome is the deferred Epic 6's).
  - *Given* an inner function that panics while `ScopedDispatch` holds its `tokio::sync::Mutex<&mut TraceScope>` guard (the mutex wraps the borrow, so the scope's `Drop` takes no lock on it), *Then* the scope's `Drop` runs under `thread::panicking()`, emits one `unfinished` trace and the process is not aborted.
- `status_code` is no longer hardcoded.
  - *Given* `grep -n "status_code: 200" src/metrics/mod.rs`, *Then* no production hit.
- Existing `selected_model` behavior is preserved.
  - *Given* `src/routing/router/tests/selected_model.rs` and `tests/observability.rs`, *When* the setters `set_selected_model*`/`record_selection` are removed, *Then* `RequestDetail.selected_model` and `selected_model_was_exploration` are still populated by `TraceScope::finish` and those tests pass after being migrated (assert against the ring-buffer entry written by `finish`).
- Streaming status caveat.
  - *Given* `stream: true` and `Ok(response)` at headers, *Then* the trace records 200 and the `stream` field is `true`; docs/tooltip say "status at response headers".
**Files**: `src/providers/mod.rs`, `src/metrics/trace.rs` (new: `TraceScope`, `RequestOutcome`, minimal `AttemptRecord`; imports only `metrics` siblings, std, serde), `src/metrics/mod.rs`, `src/routing/router/mod.rs`, `src/routing/router/tests.rs`, `src/routing/router/tests/{selected_model,observability,rate_limit_hold}.rs`, `tests/trace_client_abort_e2e.rs` (new)

Tasks (order: **1.1.3i first**, then a-h; **1.1.3j** with 1.1.3g; **1.1.3k** last, after Task 1.1.5a1; every task <= 0.5 d):
- **1.1.3i** Characterization tests (`src/routing/router/tests/characterization.rs`) for the eight scenarios in the first AC, run green on unmodified `main` and committed before any other task in this story (0.5 d).
- **1.1.3j** `tests/trace_scope_adoption.rs` wrapper-adoption check with a literal allowlist; wired into `cargo test` so CI enforces it (0.25 d).
- **1.1.3a** Failing `trace_status` + outcome-mapping tests (table-driven, exhaustive helper) in `src/providers/mod.rs` tests (0.25 d).
- **1.1.3b** Implement `ProviderError::trace_status` next to `kind_label` (`src/providers/mod.rs:125`), exhaustive `match`, no wildcard (0.1 d).
- **1.1.3c1** (0.35 d) Failing router tests for terminal traces: 429 all-rate-limited (paused time), 400 validation with the `Err` still returned, exactly-one 200 trace. The fake-provider harness is `src/routing/router/tests.rs` (`AlwaysOkProvider`, `AlwaysErrProvider`, `CapturingProvider`) plus `src/routing/router/tests/*.rs` (`rate_limit_hold.rs` has the paused-time hold pattern); **not** `router/mod.rs`. Subscribe to the collector's `event_tx`.
- **1.1.3c2** (0.35 d) Failing cancellation and `Drop`-case tests: drop during hold, hanging upstream dropped after 5 s, drop after `begin` before dispatch, finish-then-drop emits once, the `Cancelled` vs `unfinished` split.
- **1.1.3d1** (0.5 d) Create `src/metrics/trace.rs` (imports only `crate::metrics` siblings and std/serde; **not** a crate-level leaf, see the Tech Debt row; `metrics -> providers`/`entrypoint::events` edges already exist and are not added to): `TraceScope::begin(metrics, body, stream, tokens_before)` (sets `compression_status = off`; `record_compression` is the deferred Epic 6, Story 6.1.3), `ScopeState`, `RequestOutcome`, minimal `AttemptRecord`, `finish(RequestOutcome)`, `mark_handler_returned()`; register `pub mod trace;` in `src/metrics/mod.rs` (it is NOT under `routing`; verify with `grep -rn "crate::routing" src/metrics` and `grep -nE "crate::(providers|routing)" src/metrics/trace.rs` returning nothing).
- **1.1.3d2a** (0.25 d) Poison-safe, non-panicking `Drop` implementing the three cases in the ACs (`Active && !handler_returned && !panicking` -> `cancelled`, else `unfinished`), `lock().unwrap_or_else(PoisonError::into_inner)` on the ring buffer and `event_tx`.
- **1.1.3d2b** (0.25 d) Move `push_request` + requested-model capture into `begin`; `MetricsCollector::finish_scope` does one ring-buffer replace and one emit via `RequestTraceData::from_detail`; keep `RequestTimingUpdate` Bedrock fields working; add `impl From<&ProviderError> for AttemptOutcome` in `src/providers/mod.rs`.
- **1.1.3e1** (0.25 d) Split `Router::dispatch` (`router/mod.rs:667-795`) mechanically: `dispatch` = compat wrapper (`TraceScope::begin` then `dispatch_traced` then `finish`); `dispatch_traced(&mut TraceScope, ..)` keeps the local-catalog early return; `dispatch_attempts` is the loop body. Characterization tests (1.1.3i) stay green with no edits.
- **1.1.3e2** (0.25 d) **The outer part keeps request-id creation, `push_request`, `push_original_body` and requested-model capture (now inside `TraceScope::begin`)** so the ring-buffer entry exists before `finish`. Delete the `record_first_attempt_timing` call in the Ok arm (`:755`).
- **1.1.3f** Migrate `selected_model`/`observability` tests per the AC; remove `set_selected_model*`, keep fields populated from `finish` (0.4 d).
- **1.1.3g** Gate + live check: Rust gate; with the Story 1.1.5 fake upstream, an unreachable upstream shows a 502/503 row (0.4 d).
- **1.1.3h** `Drop` safety tests: inner-returns-failure after the emulation loop started, drop-while-borrowed ordering, finish-then-drop mid-stream, poisoned ring-buffer/`event_tx` mutex, panic while holding the scope guard, `unfinished` vs `cancelled` counters (0.4 d). Handler-wrapper adoption per ingress handler is Story 2.1.6b.
- **1.1.3k** (0.4 d; needs Story 1.1.5 Task 1.1.5a1 and Tasks 1.1.3d2a, 1.1.3e1/e2) Real-socket client-abort e2e per the AC above in `tests/trace_client_abort_e2e.rs`: `Script::hang()` upstream, raw client abort after 200 ms, assert one 499 trace, ring-buffer entry, `trace_cancelled_total == 1`, `trace_unfinished_total == 0` (ordering/timeout assertions with wide tolerances, no paused time).
  Task sum: 0.5 (i) + 0.25 (j) + 0.25 (a) + 0.1 (b) + 0.7 (c1 + c2) + 0.5 (d1) + 0.5 (d2a + d2b) + 0.5 (e1 + e2) + 0.4 (f) + 0.4 (g) + 0.4 (h) + 0.4 (k) = 4.9 d.

#### Story 1.1.4: Trace table renders real values and honest absences (1.95 days)
**As an** operator, **I want** each cell to show a real value or an explicit "n/a (reason)", **so that** I never read `undefinedms` or a false 0.
**Acceptance Criteria**:
- Table shows request id (copyable, 8-char display, full in `title`), latency (to headers), tokens, status text + glyph.
  - *Given* fixture `request-trace-success.json`, *When* `RecentTraceTableComponent` renders, *Then* the row text contains `req_123`, `42ms`, `1,870 est.`, and `200 OK`.
  - *Given* fixture `request-trace-failure.json` (`statusCode: 503`, `firstByteMs: null`, `tokensAfter: null`), *Then* status reads `503 Unavailable`, the first-byte cell shows an en dash with sr-only text `n/a (no response)` **and** the same text in its `title` (hover), the dash is not a tab stop, and no cell contains `undefined` or `NaN`.
- Absent-value explanation is visible to keyboard users without per-cell tab stops (UX-1, UX-34).
  - *Given* any `n/a (reason)` cell, *Then* the en dash carries `title="n/a (reason)"` (hover) and is **not focusable** (no `tabindex`; a spec asserts the table has no more tab stops per row than its Inspect, copy and badge controls). Keyboard users get the reason as **visible text**: the footer disclosure "What do these dashes and chips mean?" (`aria-expanded`, Enter/Space toggles it) lists every reason string from the Column Truth Table with its meaning, and the Inspect drawer (Story 2.1.5) lists the trace's own reasons in a visible "Not recorded" block. Screen readers get a row-level description: the row's Inspect button has `aria-describedby` pointing at an sr-only element such as "First byte: n/a (not streamed). Tokens after: n/a (compression off)." Specs assert (a) title equals the sr-only reason for every Truth Table state, (b) after activating the footer disclosure by keyboard every reason string is present in visible (not `sr-only`, not `title`-only) text, (c) the row description lists exactly that row's n/a cells, (d) the legend line is present exactly once.
- `naReason` follows the Column Truth Table.
  - *Given* a table-driven spec over the trace kinds/states in the truth table (routed non-stream, routed stream with no `firstByteState`, a stream row with a non-null `firstByteMs`, cache hit, local, failed, cancelled), *Then* each first-byte and tokens-after cell text equals the table entry.
- Savings only shown when compressed.
  - *Given* `compressionStatus: "off"`, `"none"`, `"skipped"` or `"failed"`, *Then* no "saved" text (and `skipped`/`failed` read `n/a (compression skipped)` / `n/a (compression failed)`, never "no savings"); *Given* `compressed, tokensBefore: 2000, tokensAfter: 1240`, *Then* `2,000 -> 1,240 (-38%)`.
- Stream status caveat is visible.
  - *Given* `stream: true`, *Then* the status cell has `title="status at response headers"`.
- Header labels honest; rows keyed by `requestId`; empty state distinguishes connection state (UX-10, UX-11 reconnecting half).
  - *Given* `connectionStatus() === 'connected'` and zero traces, *Then* the text "No requests yet. Point a client at this proxy to see traces." is followed by the proxy URL (`window.location.origin`, shown in a `<code>` element) and a "Copy URL" button that writes that URL to the clipboard; the text never mentions the stream or a connection error. *Given* `'reconnecting'`, *Then* banner "Live feed interrupted - reconnecting (attempt N)" with `role="status"`; rows are kept and dimmed. (The disconnected banner and Reconnect button are Story 4.1.5 core.)
- Loading state (UX-17, table part).
  - *Given* `connectionStatus() === 'connecting'` (or `'reconnecting'` with no rows yet) and zero traces, *Then* 3 skeleton rows (`aria-hidden`) and the text "Connecting to live feed..." render instead of the empty-state message; when the first event or the `connected` status arrives the skeleton is replaced by rows or the empty state.
- `n/a` is honest in this project, where Epic 6 is deferred (pre-mortem P1-1). This story owns every `naReason` text for `compressionStatus` (`off`, `none`, `skipped`, `failed`, `compressed`), including `n/a (compression skipped)` and `n/a (compression failed)`; the deferred Epic 6 adds only the three streamed first-byte state texts (Story 6.1.5e).
  - *Given* a `stream: true` trace with `compressionStatus: "off"`, `firstByteMs: null` and no `firstByteState` field, *Then* the first-byte cell reads `n/a (not measured)` and the tokens-after cell `n/a (compression off)`.
- Copy and buffer footer (UX-03 row half, UX-32).
  - *Given* the copy button on a row, *When* activated, *Then* `navigator.clipboard.writeText` receives the full `requestId` and a single polite status region (`role="status"`, the same element Task 4.1.7c later extends) announces "Copied request ID" **only after the write promise resolves**.
  - *Given* `navigator.clipboard` is undefined (non-secure context such as `http://<lan-ip>` or an old browser) or `writeText` rejects, *Then* a shared `copyText()` helper (also used by the drawer and the empty-state Copy URL button) falls back to selecting the text in a temporary `readonly` input and `document.execCommand('copy')`; if that also fails, the status region announces "Copy failed - select the ID and copy it manually", the full ID is selected in place (`select()`), and "Copied" is never announced. Three specs: success, API-missing fallback success, both-fail message.
  - *Given* 101 traces pushed, *Then* the table holds 100 rows (oldest evicted) and a footer reads "Showing latest 100".
- Epic 1 boundary validation is recorded (product triad).
  - *Given* the Epic 1 build running against real traffic, *When* Tyler uses the table and the existing Inspect link (the pre-drawer `/sessions?requestId=` navigation) for one working day, *Then* Task 1.1.4i adds a dated entry to `gap-report.md` ("Epic 1 validation": days used, whether he reached for the table instead of `/metrics` or logs, what he missed); a "did not use it" result stops the plan for a decision by Tyler before Epic 2.
- Coverage note while Epic 2 has not shipped (engineering triad 6; see "Epic 1 trace-coverage boundary").
  - *Given* the Epic 1 build (constant `TRACE_COVERAGE_NOTE_ENABLED = true`), *Then* the footer reads "Rows cover requests that reach routing. Requests rejected before routing, cache hits and multi-round search requests are not shown as one row yet."; Task 2.1.6f flips the constant off and deletes the note and its spec.
**Files**: `ui/src/app/features/overview/components/recent-trace-table.component.ts`, `recent-trace-table.component.spec.ts` (new), `overview-dashboard.component.ts`, `ui/src/app/core/models/na-reason.ts` (new), `ui/src/app/core/util/copy-text.ts` (new)

Tasks: **1.1.4a** failing component + `naReason` specs (0.25 d); **1.1.4b** cells and formatting, `naReason`, status text map and glyph set from `trace-timeline.component.ts:44-47`, header "Latency (to headers)", "First byte" (0.4 d); **1.1.4c** tokens, keys (`track t.requestId`), empty/reconnect states, Overview pill text (`overview-dashboard.component.ts:32-34`) (0.25 d); **1.1.4d** gate, before/after screenshot, open Draft PR "Epic 1" (0.1 d); **1.1.4e** skeleton rows, empty-state proxy URL + Copy URL, copy announcement through one status region, `n/a (not measured)` case, latest-100 footer, each with its failing spec first (0.35 d); **1.1.4f** non-focusable dashes with `title`, the row-level `aria-describedby` description, the footer disclosure listing every reason string, the legend line, and the keyboard-visible-text spec per truth-table state (0.25 d); **1.1.4g** `copyText()` helper with `execCommand` fallback and failure state, announce only on success, three specs (0.15 d); **1.1.4h** coverage-note footer and its constant (0.1 d); **1.1.4i** Epic 1 validation record: after the Epic 1 PR merges and the binary has run on real traffic for one working day (calendar time, no engineering effort), write the `gap-report.md` entry per the AC (0.1 d). Task sum: 0.25 + 0.4 + 0.25 + 0.1 + 0.35 + 0.25 + 0.15 + 0.1 + 0.1 = 1.95 d.

#### Story 1.1.5: Controllable fake upstream harness (1.5 days; moved here from Story 5.1.1)
**As a** developer, **I want** an in-process axum fake upstream I can script (200, 429, 529, hang, `/v1/models`), **so that** Epics 1-3 and the e2e tests share one controllable upstream from week 1.
**Acceptance Criteria**:
- Scriptable behaviors.
  - *Given* `FakeUpstream::start(Script::sequence([Status(529), Ok(anthropic_message())]))`, *When* two requests hit it, *Then* the first gets 529 and the second a valid Anthropic message; `Script::hang()` never responds; `Script::models(["m1","m2"])` serves `GET /v1/models`.
- Usable from a config file and from tests.
  - *Given* `FakeUpstream::base_url()`, *When* written into a temp route config, *Then* the binary (`cargo run`) and in-process tests can both use it; port is OS-assigned.
- Streaming mode.
  - *Given* `Script::stream_chunks(["a","b"], delay)`, *Then* it emits Anthropic SSE frames with the configured inter-chunk delay (used by the exploratory pass; the first-byte tests that need the delays are the deferred Epic 6, Story 6.1.5).
- Pre-headers delay and `Retry-After`.
  - *Given* `Script::delay_headers(150ms).then(Ok(..))`, *Then* response headers arrive no earlier than 150 ms after the request; *Given* `Script::status_with_retry_after(429, 3)`, *Then* the 429 carries `Retry-After: 3` (drives `RateLimitedWithRetry` and hold tests); `Script::hang_after_headers()` sends headers then never sends a body byte (built here because it is harness-local and cheap; its only consumers are the deferred Epic 6 `ended_empty`/`client_closed` tests); `Script::hang()` never sends headers (drives the client-abort `cancelled` test, Task 1.1.3k in week 1-2 and again in Story 5.1.1).
- Request recorder.
  - *Given* any script, *When* a request is served, *Then* `received_bodies()` returns the parsed JSON bodies (and `received_headers()` the headers) in arrival order, so tests can assert what the upstream saw (Story 5.1.1 uses it; the deferred Stories 6.1.2 and 6.1.4 will too).
- Which tests use what.
  - In-crate `#[cfg(test)]` modules (router, catalog, `compress_step`) use the existing `router/tests.rs` fake providers because `tests/support/` is not reachable from them; `tests/*.rs` integration and e2e tests use `tests/support/fake_upstream.rs`. Tests that need real sockets and timers (first byte, abort) cannot use paused time and assert **ordering with wide tolerances** (headers < first byte; `firstByteMs >= configured delay`), not +/- 50 ms.
**Files**: `tests/support/fake_upstream.rs` (new, `mod support;` from test crates), `tests/support/mod.rs`, `src/entrypoint/test_support.rs` (reuse helpers; no change expected)

Tasks: **1.1.5a1** harness core (200/429/529/hang/models/stream) + self-test (0.5 d; **scheduled first, week 1**, because Task 1.1.3k needs `Script::hang()`); **1.1.5a2** `delay_headers`, `status_with_retry_after`, `hang_after_headers` and the body/header recorder with their self-tests (0.5 d); **1.1.5b** standalone `examples`-free launcher test helper that prints a base URL for manual runs (via `cargo test -- --ignored fake_upstream_serve`) (0.2 d); **1.1.5c** use it in Task 1.1.1a/1.1.3g and write the "which tests use what" note in `tests/support/mod.rs` (0.3 d). This story is scheduled with Story 1.1.1 so the Epic 1 and Epic 2 PR screenshots are reproducible.

---

## Phase 2: Requested -> routed, reason, attempts, Inspect (Epic 2)

### Epic 2.1: Explain the routing decision
**Goal**: For any trace an operator sees requested model -> routed model/provider, a reason chip, and every attempt with status and latency, in a drawer with a waterfall; emulation-loop, cache-hit and local traffic each appear as exactly one honest trace. **Effort: 11.55 days.**

#### Story 2.1.1: Trace domain types (ADR-002; 0.9 day)
**As a** developer, **I want** closed enums and a bounded log for attempts, **so that** illegal states cannot be represented and memory stays capped.
**Acceptance Criteria**:
- `AttemptLog` caps at 16 and flags truncation.
  - *Given* an empty `AttemptLog`, *When* 20 `AttemptRecord { iteration: 0, upstream: "bedrock", outcome: RateLimited, status: Some(429), latency_ms: 120 }` are pushed, *Then* `len() == 16`, `truncated() == true`, the first 16 are kept in order.
- `AttemptOutcome::from(&ProviderError)` and `AttemptErrorClass::from(&ProviderError)` (impls in `src/providers/mod.rs`, tests beside them) are exhaustive and secret-free.
  - *Given* `ProviderError::Upstream { status: 500, body: "sk-SENTINEL" }`, *When* converted and serialized to an `AttemptTrace`, *Then* JSON contains `"outcome":"upstream"`, `"status":500`, `"errorClass":"server_error"`, `"errorMessage":"Upstream server error (HTTP 500)"` and not `SENTINEL`; the same holds for every other variant carrying a body or message (`Auth("sk-SENTINEL")`, `Validation("sk-SENTINEL", 422)`, `ResponseShapeMismatch("sk-SENTINEL")`, `Upstream { status: 0, body: "arn:aws:iam::123456789012:role/x" }`): serialized `attempts` never contains `SENTINEL` or `123456789012`.
  - *Given* the variants in the glossary `AttemptErrorClass` row, *Then* a table-driven test over an exhaustive helper asserts each maps to the stated class (`Upstream{529}` and `Upstream{503}` -> `overloaded`, `Upstream{0}` -> `unreachable`, `Upstream{500}` -> `server_error`, `Upstream{404}` -> `client_error`, `RateLimitedWithRetry` -> `rate_limited`) and that an `Ok` attempt has `errorClass: null`, `errorMessage: null`.
- `RouteReason` has exactly the wire strings of the "Route reason and label maps" table.
  - *Given* each `RouteReason` value, *When* serialized, *Then* the strings are `session_pin`, `strategy`, `exploration`, `fallback`, `rate_limit_hold`, `default`, `cache`, `local` (a table-driven test over an exhaustive helper, so a new variant fails compilation), and `Strategy` sets `routeStrategy` to the `RoutingStrategy::name()` value while every other reason leaves it null.
- `RouteReason::for_request(&RouteInputs)` takes only plain values (no router/strategy types) and chooses the row chip deterministically (precedence `Cache`/`Local` (by `TraceKind`) > `SessionPin` > `Exploration` > `Fallback` > `RateLimitHold` > `Strategy` > `Default`).
  - *Given* a request with `session_id` pinned and one attempt, *Then* `SessionPin`; *Given* two attempts where the second follows a `RateLimited`, *Then* `Fallback`; *Given* `hold_retries == 2`, a single successful attempt and no failed attempts, *Then* exactly `RateLimitHold`; *Given* `hold_retries == 2` and one failed attempt before success, *Then* `Fallback` (higher precedence).
- A strategy has a name.
  - *Given* `FallbackStrategy` and `WeightedStrategy`, *When* `name()` is called, *Then* `"fallback"` / `"weighted"`; the trait method has a default of `"custom"`.
- `UpstreamName` lives in a leaf module.
  - *Given* `grep -rn "metrics::trace" src/routing/model_catalog.rs`, *Then* no hit (catalog imports `UpstreamName` from `src/metrics/ids.rs`).
**Files**: `src/metrics/trace.rs`, `src/metrics/ids.rs` (new), `src/providers/mod.rs`, `src/routing/strategy.rs`, `src/entrypoint/events.rs`

Tasks: **2.1.1a** failing unit tests: cap/truncation, outcome and error-class mapping for every `ProviderError` variant, reason wire strings and precedence (0.25 d); **2.1.1b** types `AttemptRecord` (with `error_class: Option<AttemptErrorClass>`), `AttemptOutcome` (+`Cancelled`), `AttemptErrorClass`, `RouteReason`, `RouteInputs`, `AttemptLog`, `MAX_ATTEMPTS = 16`, `RouteReason::for_request(&RouteInputs)` (lives in `metrics/trace.rs`, not the router; the router builds `RouteInputs` from `session_id` pin, `strategy.name()`, the exploration flag, `hold_retries` and the attempt list), `UpstreamName` in `ids.rs`, and the `From<&ProviderError>` impls for `AttemptOutcome` and `AttemptErrorClass` in `providers/mod.rs`; `UpstreamName` is parsed from `UpstreamRef.name: String` (`src/routing/strategy.rs:19`) at that boundary (0.3 d, includes the class enum and its exhaustive mapping); **2.1.1c** wire form `AttemptTrace { iteration, upstream, model, outcome, status, latencyMs, errorClass, errorMessage }` + `From<&AttemptRecord>` in `events.rs` (the message comes from a fixed per-class phrase table plus the numeric status, never from error text) and a sentinel-secret test; extend `request-trace-failure.json` with a 3-attempt chain (anthropic 529 `overloaded` -> bedrock 429 `rate_limited` -> openrouter 200) (0.2 d); **2.1.1d** add `fn name(&self) -> &'static str { "custom" }` to `RoutingStrategy` (`src/routing/strategy.rs:29`) and override in each strategy; the earlier draft's `Strategy` name does not exist, the trait is `RoutingStrategy` (0.15 d). Task sum: 0.25 + 0.3 + 0.2 + 0.15 = 0.9 d.

#### Story 2.1.2: Capture attempts and reason in `dispatch_attempts` (1.7 days)
**As an** operator, **I want** every try (including sheds and holds) recorded, **so that** the chain shows where time went.
**Acceptance Criteria**:
- Fallback chain captured in order.
  - *Given* upstreams `[anthropic, bedrock]` where `anthropic` returns `Upstream{status:529}` and `bedrock` returns Ok, *When* `dispatch` runs, *Then* the log is `[anthropic/upstream/529/overloaded, bedrock/ok/200]` and the final reason is `Fallback`.
- Shed recorded without tripping cooldown.
  - *Given* admission returns `Admit::Shed` for `anthropic`, *Then* attempt 1 is `anthropic/shed` (`errorClass: shed`) with `latency_ms == 0`, and `health` for `anthropic` is unchanged (ADR-004 comment at `router/mod.rs:722-726`).
- Hold-and-retry counted with exact reason.
  - *Given* all candidates in short cooldown, paused time, and `hold_retries` reaches 2 before one attempt succeeds, *Then* `RequestOutcome.hold_retries == 2` and `routeReason == "rate_limit_hold"` exactly (chip text `held`) (not merely "includes"); the test sets the cooldown so the hold wait is bounded.
- Per-attempt writes are gone.
  - *Given* the Task 2.1.3f counting test, *Then* a 3-attempt dispatch performs exactly one ring-buffer **replace** (`finish`); the `push_request` at `begin` is the only other lock acquisition (the single check; the earlier grep-only AC is dropped as a weak proxy).
- A cancelled request records the attempt that was in flight (no outcome exists before the upstream returns, so the scope creates it first).
  - *Given* `dispatch_attempts` about to await `provider.send(..)` for `anthropic`, *Then* it first calls `scope.begin_attempt(upstream, model, iteration)`, which stores a pending attempt (also setting `final_attempt_started_at`); each of the five outcome arms and the `Shed` branch calls `scope.complete_attempt(outcome, error_class, status)` (both derived from the same `&ProviderError`) which moves it into the `AttemptLog`; `Drop` of an `Active` scope converts a still-pending attempt into `AttemptOutcome::Cancelled` (`error_class: Cancelled`, `status: None`, `latency_ms` = elapsed). A pending attempt is never serialized by itself and adds no new wire value.
  - *Given* a never-responding upstream, `#[tokio::test(start_paused = true)]`, and the `dispatch` future dropped after 5 s of virtual time, *Then* the trace has one attempt `cancelled` with `latencyMs >= 5000`.
- Router file does not grow net.
  - *Given* `wc -l src/routing/router/mod.rs` before and after Tasks 2.1.2b-d, *Then* the after count is <= before (reason derivation and outcome mapping live in `metrics/trace.rs`).
**Files**: `src/routing/router/mod.rs`, `src/metrics/trace.rs`, `src/routing/router/tests/*.rs`

Tasks (sum 0.4 + 0.6 + 0.3 + 0.4 = 1.7 d): **2.1.2a** failing router tests for the scenarios (including the hanging-upstream cancelled attempt) using the `tests.rs` fakes, asserting on the ring-buffer entry / SSE event (no test-only closure hook) (0.4 d); **2.1.2b1** `scope.begin_attempt` before the `.await`, `scope.complete_attempt` in the five outcome arms using `AttemptOutcome::from(&e)`, `AttemptErrorClass::from(&e)`, `e.trace_status()`, `attempt_started.elapsed()`; no error text stored, only the closed class (0.3 d); **2.1.2b2** the pending-to-`Cancelled` conversion in the scope's `Drop` and its paused-time test wiring (0.3 d); **2.1.2c** `Shed` attempt record in the `Admit::Shed` branch (`router/mod.rs:727-733`); count `hold_retries` in the dispatch state (0.3 d); **2.1.2d** build `RouteInputs` from `session_id` pin (`effective_candidates`), `strategy.name()`, `chosen` exploration flag, `hold_retries`, attempt list and call `RouteReason::for_request`; delete `record_selection`'s mutex writes (`router/mod.rs:848-857`) and `set_selected_model*` callers; assert the file-size AC (0.4 d).

#### Story 2.1.3: `finish` replaces the ring-buffer entry once and emits the full trace through one builder (2.05 days)
**As an** operator, **I want** the trace to carry requested model, routed model/provider, reason and attempts, **so that** the UI can show them.
**Acceptance Criteria**:
- Trace carries the new fields.
  - *Given* a request with `model: "gpt-4o"` routed to upstream `openrouter` pinned to `gpt-4o-mini` after one failed attempt, *When* the terminal trace is serialized, *Then* JSON has `requestedModel:"gpt-4o"`, `routedModel:"gpt-4o-mini"`, `routedModelSource:"pinned"`, `routedProvider:"openrouter"`, `routeReason:"fallback"`, `routeStrategy:null`, `attempts` of length 2 (the failed one with `errorClass:"overloaded"` and `errorMessage:"Upstream overloaded (HTTP 529)"`, the second with both null), `attemptsTruncated:false`, `iterations:1`, `totalMs` set. A `Strategy` reason serializes `routeReason:"strategy"` with `routeStrategy:"weighted"`; a rate-limit hold serializes `"rate_limit_hold"`.
- Passthrough is labelled.
  - *Given* a non-pinned route and `model: "claude-haiku-4-5"`, *Then* `routedModel:"claude-haiku-4-5"`, `routedModelSource:"passthrough"`.
- One builder.
  - *Given* a `RequestDetail`, *When* `RequestTraceData::from_detail(&detail)` is called, *Then* it is the only constructor of `RequestTraceData` in non-test code (`sg --pattern 'RequestTraceData { $$$ }' --lang rust src` finds only `events.rs`); the SSE emit in `finish`, the stream first-byte re-emit and the Story 4.1.6 backfill all call it.
- Evicted entry needs no special case.
  - *Given* a ring buffer that evicted `req_old` before its terminal write, *When* `finish` runs, *Then* the trace is built from the scope-local `RequestDetail` (the scope holds the full detail), the SSE event is still emitted with identical content, the ring-buffer replace is skipped, and `trace_finish_evicted_total` increments.
- Legacy dashboard JSON stays additive.
  - *Given* `/metrics` `recent_requests`, *Then* existing keys `request_id`, `duration_ms`, `model` are unchanged and `attempts`, `route_reason` are present.
- SSE push SLO is measured.
  - *Given* a subscribed receiver and a `finish` call, *Then* the event is received within 100 ms (generous bound, `tokio::time::timeout`); records the requirement "SSE push < 100 ms".
- Always-on coarse hot-path regression test (non-`#[ignore]`, runs in every `cargo test`).
  - *Given* a `Router` with a fake instant provider and a collector, *When* 2,000 sequential dispatches each with a 3-attempt fallback chain run in a debug build, *Then* the whole loop finishes within 10 s (the expected cost is well under 0.5 s, so the bound has about 20x headroom and only an O(n^2), blocking call or per-attempt lock regression trips it), the ring buffer never exceeds 100 entries and the attempt log never exceeds 16; the release-only precise timing test (`trace_added_latency_should_be_within_noise_...`) stays `#[ignore]`.
**Files**: `src/metrics/mod.rs`, `src/metrics/trace.rs`, `src/entrypoint/events.rs`, `src/routing/router/mod.rs`

Tasks: **2.1.3a** failing tests for the ACs; extend fixture-equality with final fields (0.4 d); **2.1.3b** extend `RequestDetail` (additive for legacy JSON: `trace_revision`, `first_byte_state` (stays absent in this project), `final_attempt_started_at` (scope-local `Instant`, not serialized), `attempts`, `route_reason`, `routed_model`, `routed_model_source`, `routed_provider`, `trace_kind`, `iterations`, `total_ms`, `compression_status`, `stream`); `finish` mutates the scope-local detail, then one `replace_request` under the ring-buffer lock, then emit `RequestTraceData::from_detail(&detail)` (0.5 d); **2.1.3c** wire fields (camelCase) per the AC, including `routeStrategy` and the per-attempt `errorClass`/`errorMessage` (+0.1 d over the earlier estimate); `requested_model` captured at `TraceScope::begin` (ADR-004), mapping `"unknown"` placeholder to `None` (0.35 d); **2.1.3d (optional, cut-able)** `resolvedModel` only if Task 1.1.1c says yes (0.4 d, not counted in the epic total); **2.1.3e** counters `trace_finish_total`, `trace_finish_evicted_total`, `trace_cancelled_total` in `src/metrics/counters/mod.rs:317-355` surface and `debug!` on eviction (0.25 d); **2.1.3f** hot-path assertion: counting wrapper (cfg(test) counter in `replace_request`) asserts 1 replace per request (plus the `push_request` at `begin`) for a 3-attempt chain (was 3 `mutate_request` per attempt) (0.2 d); **2.1.3g** SSE latency test per the AC (0.15 d); **2.1.3h** always-on coarse regression test per the AC (0.2 d). Task sum: 0.4 + 0.5 + 0.35 + 0.25 + 0.2 + 0.15 + 0.2 = 2.05 d (2.1.3d optional, not counted).

#### Story 2.1.4: Trace row shows requested -> routed, reason chip, attempt badge, chip legend (1.2 days)
**As an** operator, **I want** the Model Route cell to show what was asked vs where it went, **so that** I can spot unexpected routing at a glance.
**Acceptance Criteria**:
- Different models show an arrow; equal ones collapse.
  - *Given* `requestedModel: "gpt-4o"`, `routedModel: "gpt-4o-mini"`, `routedProvider: "openrouter"`, *Then* line 1 reads `gpt-4o -> gpt-4o-mini` and line 2 `openrouter`; *Given* equal names, *Then* a single chip and no arrow.
- Reason chip and attempt badge use the single vocabulary of the "Route reason and label maps" table.
  - *Given* each wire `routeReason` (`session_pin`, `strategy`, `exploration`, `fallback`, `rate_limit_hold`, `default`, `cache`, `local`), *Then* the chip text is respectively `pinned`, `strategy`, `explore`, `fallback`, `held`, `default`, `cache`, `local` (table-driven spec); an unknown string renders the raw value, not blank.
  - *Given* `routeReason: "fallback"` and 3 attempts, *Then* a text chip `fallback` and a button-badge `x3 attempts`; *Given* 1 attempt, *Then* no badge; *Given* `iterations: 3`, *Then* an additional `3 search rounds` badge.
  - *Given* a final `statusCode: 200` with attempts `[anthropic/529, openrouter/200]`, *Then* the status cell carries the sub-label `fell back`; *Given* attempts `[anthropic/429, anthropic/200]` (same upstream), *Then* `retried`; *Given* a failed final status (502), *Then* the real code and no sub-label.
- Chip jargon is explained (UX-8).
  - *Given* each chip, *Then* it has a `title` equal to its legend string from the "Chip jargon legend" (`held` = "waited for a rate-limit cooldown, then retried", `default` = "no special rule; the default route", `explore` = "an occasional exploration pick to test another upstream", `pinned` = "a session pinned this conversation to one upstream", ...), the table footer has a "What do these dashes and chips mean?" disclosure button (`aria-expanded`) that lists all legend strings, and a table-driven spec asserts every wire reason has a tooltip and a legend row.
- Non-routed kinds.
  - *Given* `traceKind: "cache_hit"`, *Then* chip `cache` and no provider line; `"local"` -> chip `local`; status 499 -> chip `cancelled`.
- Passthrough shows provider + requested only.
  - *Given* `routedModelSource: "passthrough"`, *Then* no arrow, provider on line 2.
**Files**: `ui/src/app/features/overview/components/recent-trace-table.component.ts`, its spec, `ui/src/app/core/models/telemetry.models.ts`

Tasks: **2.1.4a** failing spec using `request-trace-failure.json` + cache/local/cancelled fixtures (0.3 d); **2.1.4b** model type + two-line cell; chip text map from the label-map table (`pinned`, `strategy`, `explore`, `fallback`, `held`, `default`, `cache`, `local`, plus the status-driven `cancelled`), `fell back`/`retried` derivation, `title` carries full names (0.5 d); **2.1.4c** gate, screenshot (0.2 d); **2.1.4d** chip `title`s, footer legend disclosure, table-driven spec (0.2 d). Task sum: 0.3 + 0.5 + 0.2 + 0.2 = 1.2 d.

#### Story 2.1.5: Inspect drawer with attempt waterfall, selectable attempt detail, Why block, narrow-viewport sheet (3.1 days)
**As an** operator, **I want** an Inspect drawer with route, reason, and a waterfall of attempts, **so that** I can see why a request went where it did and what each try cost.
**Acceptance Criteria**:
- Inspect opens a non-modal drawer for that trace, and the button has a unique label.
  - *Given* the table with trace `9f8e7d6c-1a2b-4c3d-8e9f-0a1b2c3d4e5f` (production ids are UUIDs; the aria-label uses the first 8 characters), *When* the Inspect button (`aria-label="Inspect trace 9f8e7d6c"`) is activated by Enter, *Then* a `role="dialog"` `aria-modal="false"` panel opens, focus moves to its heading, the URL gains `?requestId=9f8e7d6c-1a2b-4c3d-8e9f-0a1b2c3d4e5f`, and Esc closes it and returns focus to that button.
  - *Given* the drawer is open and the originating row has since left the 100-row buffer (or the drawer was opened from a deep link, so there is no originating button), *When* Esc is pressed, *Then* the drawer closes and focus moves to the trace table's `<caption>`-labelled table element (`tabindex="-1"`), never to `document.body` (UX-20 fallback).
- Waterfall on a shared time axis, readable without color.
  - *Given* the 3-attempt fixture (529 @ 0-310 ms, 429 @ 330-450 ms, 200 @ 470-4120 ms), *Then* an ordered list shows `1 anthropic / sonnet-4-5  x 529 overloaded  310ms`, `2 bedrock ...  x 429 rate limited  120ms`, `3 openrouter / gpt-4o-mini  ok 200  3650ms` (labels from the `errorClass` map), bars `aria-hidden`, and a sr-only summary "3 attempts; first two failed; third succeeded". Attempts from multiple iterations are grouped under "Round N".
- Selecting an attempt shows its sanitized error detail (UX-02, S3 step 3).
  - *Given* the 3-attempt fixture, *When* attempt 1's row button (`aria-pressed`) is activated by Enter or Space, *Then* a detail pane below the list (`aria-live` off; focus stays on the button) shows attempt number, upstream/model, status `529`, latency `310ms`, the label `overloaded` and the backend `errorMessage` `Upstream overloaded (HTTP 529)`; attempt 3 (`ok`) shows status and latency and no error line; by default the first failed attempt is selected so the cause is visible in 2 steps (Inspect, read) without a further click.
  - *Given* a failed attempt whose upstream body contained `sk-SENTINEL`, *Then* neither the list, the detail pane nor the captured event contains `SENTINEL`: the pane renders only `errorClass`, `errorMessage`, status and latency, never an upstream body.
- Attempts badge opens the drawer scrolled to ATTEMPTS (UX-3).
  - *Given* a row with `x3 attempts`, *When* the badge is activated, *Then* the drawer opens, the ATTEMPTS heading is scrolled into view (`scrollIntoView` called on it, spec spies it), focus still moves to the drawer heading, and the first failed attempt is selected; *Given* the plain Inspect button, *Then* the drawer opens at the top.
- Drawer content swap is announced (UX-8).
  - *Given* the drawer is open on trace A, *When* Inspect on trace B is activated, *Then* the content swaps in place, focus stays on the drawer heading, the heading text changes to B's id8 and the shared status region announces "Showing trace <id8 of B>"; the URL `requestId` param updates.
- Narrow viewports, focus containment and target size (UX-5).
  - *Given* a viewport of 640 px or narrower (Karma `window.innerWidth` shim plus the Tailwind breakpoint class assertion), *Then* the drawer renders as a full-width sheet with no horizontal scroll at 320 px (reflow, WCAG 1.4.10) and the close button remains visible; the sheet covers the table, so it behaves as a modal: `aria-modal="true"`, the rest of the page is `inert`, Tab and Shift+Tab cycle among the sheet's controls (close, copy, badges, attempt rows) and never reach the hidden table, Esc closes it and focus returns to the originating Inspect button (or to the table element when that row left the buffer).
  - *Given* a viewport wider than 640 px, *Then* the drawer stays `aria-modal="false"`, nothing is `inert`, Tab from the last drawer control continues to the rest of the page (no trap: the user can Tab out to the table), and Esc closes it from any control inside it. One spec per width asserts these.
  - *Given* any drawer or table control (Inspect, copy, close, attempt rows, badges), *Then* its bounding box is at least 24x24 CSS px (WCAG 2.5.8; a spec measures `getBoundingClientRect`).
- Status explanation.
  - *Given* any trace, *Then* the drawer states "Status shown is the upstream-class status; the client may have received a different code (for example 529 vs 503 depending on the endpoint)" and, for streams, "status at response headers".
- Why block names the model source, which pin and the reason (product/UX triad).
  - *Given* the fallback fixture (`requestedModel: "gpt-4o"`, `routedModel: "gpt-4o-mini"`, `routedModelSource: "pinned"`, `routedProvider: "openrouter"`, `routeReason: "fallback"`), *Then* ROUTE shows `Requested gpt-4o`, `Routed openrouter / gpt-4o-mini`, a model-source line "pinned: openrouter's configured model replaces the client's gpt-4o" and the Why line "moved to the next candidate after anthropic failed (overloaded, HTTP 529)". *Given* `routedModelSource: "passthrough"`, *Then* the model-source line reads "passthrough: the client's model name was sent unchanged to <routedProvider>". "Which pin" is the routed upstream's name and model: the wire carries no route or upstream-index field and the router has no named rules (ADR-004), so none is invented and no new wire field is added.
  - *Given* each `routeReason`, *Then* the Why text is exactly the "Drawer Why text" column of the "Route reason and label maps" table (`strategy` names the strategy from `routeStrategy`; `default`: "default route: no session pin, exploration or strategy decision applied"; `cache`: "served from the response cache; no upstream was called"; `local`: "served by a local model <routedModel>"; status 499: "client closed the connection"); one table-driven spec renders all eight reasons plus the cancelled case and asserts the Why and model-source lines.
- Absent values are listed as visible text (UX-34).
  - *Given* a trace with any `n/a` cell (for example a non-streamed cache hit), *Then* a "Not recorded" block lists one visible line per n/a column with the exact reason string (`First byte: n/a (served from cache)`, `Tokens after: n/a (served from cache)`); *Given* a trace with no n/a cell, *Then* the block is omitted.
- Evicted/unknown states and the open-drawer snapshot.
  - *Given* the drawer is open on trace A (opened from a row) and A is then evicted from the 100-row buffer, *Then* the drawer keeps rendering from the last data it received for A (a snapshot refreshed on every store update while A exists), shows a visible banner "No longer in live buffer - showing the last data received" in the shared status region, and does not blank, close or move focus; closing it follows the Esc rule above.
  - *Given* `?requestId=gone` not in the store, *Then* "This trace is no longer retained (buffer holds last 100)."; *Given* a trace with empty `attempts`, *Then* "Attempt detail not captured"; *Given* `cache_hit`, *Then* "Served from the response cache; no upstream attempt".
- Truncation flag shown.
  - *Given* `attemptsTruncated: true`, *Then* "showing first 16 attempts".
**Files**: `ui/src/app/features/overview/components/inspect-drawer.component.ts` (new), spec (new), `recent-trace-table.component.ts`, `overview-dashboard.component.ts`

Tasks: **2.1.5a** failing drawer specs (0.4 d); **2.1.5b1** drawer shell and route param (selected trace from `MetricsStore.traces()` and `ActivatedRoute` `requestId`, replacing the `/sessions?requestId=` navigation at `overview-dashboard.component.ts:100`) (0.3 d); **2.1.5b2** focus management, Esc, focus-return fallback to the table (0.3 d); **2.1.5c (cut-able)** waterfall bars, plain ordered list is the must-have (0.5 d); **2.1.5d** tokens block per Column Truth Table, do not embed `trace-timeline` (hardcoded defaults, `trace-timeline.component.ts:62-69`) (0.2 d); **2.1.5e** gate, induce a fallback with the Story 1.1.5 fake upstream, screenshot, Draft PR "Epic 2" (0.3 d); **2.1.5f** selectable attempt rows, detail pane, default selection, sentinel-secret drawer spec, focus-return fallback spec (0.25 d); **2.1.5g** narrow-viewport full-width sheet (<= 640 px), 24x24 target-size spec across drawer and table controls (0.2 d); **2.1.5h** attempts-badge scroll to ATTEMPTS and the drawer content-swap announcement, each with its spec (0.2 d); **2.1.5i** Why block (model-source line, per-reason Why text) and the "Not recorded" block with their table-driven specs (0.2 d); **2.1.5j** open-drawer snapshot and the "No longer in live buffer" banner (0.1 d); **2.1.5k** sheet focus containment at <= 640 px (`aria-modal`, `inert`, Tab cycle) and the no-trap/Esc behavior above 640 px, one spec per width (0.15 d). Task sum: 0.4 + 0.6 + 0.5 + 0.2 + 0.3 + 0.25 + 0.2 + 0.2 + 0.2 + 0.1 + 0.15 = 3.1 d.

#### Story 2.1.6: One trace per client request: emulation loop, cache hit, local (ADR-002 amendment; 2.6 days)
**As an** operator, **I want** every client request to produce exactly one trace even when the proxy dispatches several times, answers from cache, or serves a local model, **so that** the table and the error rate reflect real traffic.
**Acceptance Criteria**:
- Emulation loop aggregates.
  - *Given* a request with a server web_search tool (eligible route) that triggers 3 `dispatch` calls through `run_emulation_loop` (`src/entrypoint/messages/mod.rs:374-407`, `src/server_tools/loop.rs:125,165`), *When* it completes, *Then* exactly one `RequestTrace` with one `requestId`, `iterations: 3`, attempts from all three rounds tagged with `iteration` 0..2, `totalMs` >= the sum of round latencies, `statusCode` equal to the final response status; the ring buffer has one entry for it.
  - *Given* the loop fails on round 2 with a dispatch error, *Then* one trace with that error's status and attempts from both rounds.
- Cache hit traced.
  - *Given* a cacheable non-stream request whose key hits `response_cache` (`messages/mod.rs:130-136`), *Then* one trace `traceKind: "cache_hit"`, `statusCode: 200`, `routeReason: "cache"`, `attempts: []`, `routedProvider: null`; `requests_total` is incremented exactly once for the hit (no double count with `record_cache_hit`).
- Local dispatch traced.
  - *Given* a model that `local_catalog.resolve` matches (`router/mod.rs:682`), *Then* one trace `traceKind: "local"`, `routedProvider: "local"`, `routeReason: "local"`, one attempt, real status and latency; `dispatch_local` failures yield the mapped status.
- Counter semantics are stated and tested (verified: `record_request_success`/`failure` each add to `requests_total` **per upstream attempt**, `counters/mod.rs:149,171`; `record_cache_hit` adds one, `:165`).
  - *Given* a request that fails over anthropic 529 -> bedrock 429 -> openrouter 200, *Then* the dashboard has exactly 1 trace while `/metrics` `requests_total` rose by 3 (documented as "upstream attempts + cache hits") and `errors_total` by 2; the UI labels the `/metrics`-derived tiles "Upstream attempts" and the trace-derived error rate "per client request", never mixing the two on one tile.
- Error rate honesty.
  - *Given* a window of 10 routed 200s, 2 cache hits, 1 cancelled (499), 1 routed 503, *Then* the documented error rate counts `statusCode >= 400 && != 499` over routed + local traces, excludes cache hits from the denominator, and reports cancelled separately; defined here and consumed by Story 4.1.2.
- Handler wrapper and scope ownership are pinned.
  - *Given* `post_v1_messages` and `post_v1_chat_completions`, *Then* each is a thin wrapper that calls `TraceScope::begin`, runs the inner `async fn(&mut TraceScope, ..) -> HandlerResult`, calls `finish(result.outcome)` then `mark_handler_returned()` (Story 1.1.3 contract).
  - *Given* the emulation loop, the wrapper keeps owning the scope and `run_emulation_loop` receives `&mut TraceScope`, wrapping it in `ScopedDispatch<'s> { router, headers, est_tokens, scope: tokio::sync::Mutex<&'s mut TraceScope> }`, a struct that implements `Dispatch` directly (a closure cannot: the blanket `impl Dispatch for F` requires `Fut: 'static` and `Fn`, `src/server_tools/loop.rs:58-62`, but the trait method itself returns a future borrowing `&'a self`, `loop.rs:44-56`, so a direct impl may borrow). Each `dispatch` locks the mutex for the duration of one `dispatch_traced` (up to the ~45 s hold sleep) and increments the iteration. After `run_emulation_loop` returns, the borrow ends and the inner function returns its `HandlerResult`; the wrapper (not the inner function) calls `finish` then `mark_handler_returned`, so the scope can never be finished by two owners and `mark_handler_returned` always runs after `finish` and after every borrow is gone. Test: inner returns `Err`-derived outcome after the loop started -> one trace with that status (Story 1.1.3 AC).
- Fix every earlier "exactly one trace" claim to this definition.
  - *Given* Stories 1.1.3, 2.1.2, 4.1.1, 5.1.1, *Then* each AC says "per client request" (checked by review of this plan).
**Files**: `src/entrypoint/messages/mod.rs`, `src/entrypoint/chat_completions.rs`, `src/routing/router/mod.rs`, `src/metrics/trace.rs`, `src/server_tools/loop.rs` (read-only: `ScopedDispatch` implements the existing `Dispatch` trait), tests in `tests/server_tool_emulation.rs`, router tests

Tasks: **2.1.6a1** (0.3 d) failing tests: emulation loop with the fake upstream (extend `tests/server_tool_emulation.rs` patterns) asserting one trace; cache-hit and local tests; **2.1.6a2** (0.3 d) failing wrapper pre-dispatch-error tests (400, translation error, emulation round-2 failure) per handler, including the flip of the Epic 1 boundary test (a handler-level 400 now produces one trace); **2.1.6b1a** (0.3 d) `post_v1_messages`: create the scope before the cache lookup, `TraceScope::begin` after `clamp_context_budget` and `estimate_tokens` (so the "original" body in the ring buffer is the post-clamp body; documented in `ObservabilityCollector` comments), thin-wrapper/`HandlerResult` split; cache hit calls `scope.finish_cache_hit()`; **2.1.6b1b** (0.3 d) `post_v1_chat_completions`: the same wrapper/`HandlerResult` split and `begin` (it has no cache lookup, so no cache logic) - **2.1.6b1a/b1b own all wrapper adoption in both handlers; Task 2.1.6c does not touch the handlers**; **2.1.6b2** (0.5 d) `run_emulation_loop` (`messages/mod.rs:374-407`) takes `&mut TraceScope` and builds `ScopedDispatch` in place of the `|next| ..` closure at `:392-396` per the ownership AC, calling `dispatch_traced`; `iteration` incremented per call; **2.1.6b3** (0.1 d) one pre-dispatch-error test per handler passing; **2.1.6c** (0.3 d) `dispatch_traced` local branch records the single local attempt and returns; fix `TraceScope::begin` ordering so local requests also get an entry (router and `trace.rs` only); **2.1.6d** (0.3 d) state the per-attempt `requests_total` semantics in the counter doc comments, add the 3-attempt counter test and UI labels, reconcile `record_cache_hit` and the error-rate definition; update the `plan.md` ACs listed above; **2.1.6e** (0.1 d) gate; **2.1.6f** (0.1 d) flip `TRACE_COVERAGE_NOTE_ENABLED` off and delete the Epic 1 coverage note and its spec. Task sum: 0.3 + 0.3 + 0.3 + 0.3 + 0.5 + 0.1 + 0.3 + 0.3 + 0.1 + 0.1 = 2.6 d (b1 0.5 -> b1a + b1b 0.6 and c 0.4 -> 0.3, which moves the duplicated `chat_completions.rs` adoption out of c; story total unchanged).

---

## Phase 3: Upstream model catalog (Epic 3)

### Epic 3.1: Show what each upstream advertises
**Goal**: A cached, typed, sanitized per-upstream model list with refresh age/error, a UI view, and a warning for config references the upstream does not list. Refresh is on demand by default (ADR-003 revision): no startup, timer or hot-swap fan-out unless configured. **Effort: 9.85 days.**

#### Story 3.1.1: `ModelCatalog` core types and state transitions (0.75 day)
**As a** developer, **I want** a sum-typed catalog state, **so that** stale/error/unsupported are distinct and last-good is never lost.
**Acceptance Criteria**:
- Failure keeps last good list.
  - *Given* `UpstreamModels { state: Ok, models: 31 ids, refreshedAt: T0 }`, *When* a refresh fails with an auth error, *Then* state is `Stale`, `models.len() == 31`, `refreshedAt == T0`, `error.kind == Auth`.
- First failure with no cache is `Error`, never "0 models".
  - *Given* `Loading`, *When* the first refresh times out, *Then* state `Error`, `models` empty, `error.kind == Timeout`.
- Cap and truncation.
  - *Given* an upstream returning 1,200 ids, *When* stored, *Then* `models.len() == 1000`, `totalCount == 1200`, `truncated == true`.
**Files**: `src/routing/model_catalog.rs` (new; imports `UpstreamName` from `src/metrics/ids.rs`, never from the trace module), `src/routing/mod.rs`

##### Task 3.1.1a: Failing state-machine tests
- Pure tests on `UpstreamModels::apply(RefreshResult)`.
- Files: `src/routing/model_catalog.rs`

##### Task 3.1.1b: Types and `apply`
- `CatalogState`, `UpstreamModels`, `CatalogError`, `CatalogErrorKind`, `ModelCatalog { RwLock<HashMap<UpstreamName, UpstreamModels>> }`, `MAX_MODELS_SENT = 1000`; serde camelCase. `ModelInfo` mapped to `{ id, ownedBy }`.
- Files: `src/routing/model_catalog.rs`, `src/routing/mod.rs`

#### Story 3.1.2: Error classification and sanitization (0.5 day)
**As a** security-conscious operator, **I want** catalog errors to carry only a kind, status and fixed phrase, **so that** upstream bodies and keys never reach the browser.
**Acceptance Criteria**:
- Sentinel secret never surfaces.
  - *Given* a fake upstream returning `ProviderError::Upstream { status: 401, body: "bad key sk-ant-SENTINEL123 at https://x/v1/models?key=AIzaSENTINEL" }`, *When* classified, *Then* `CatalogError { kind: Auth, status: Some(401), message: "Upstream rejected credentials (401)" }` and the serialized catalog JSON does not contain `SENTINEL`.
- Bedrock status 0.
  - *Given* `Upstream { status: 0, body: "arn:aws:iam::123456789012:role/x not authorized" }`, *Then* `kind: Auth` or `Other` with `status: None` and no `123456789012` in output.
- `ModelUnsupported` maps to `Unsupported`.
  - *Given* `ProviderError::ModelUnsupported(_)`, *Then* `CatalogState::Unsupported`, not `Error`.
**Files**: `src/routing/model_catalog.rs`

##### Task 3.1.2a: Failing tests
- Table tests per `ProviderError` variant with sentinel strings.
- Files: `src/routing/model_catalog.rs`

##### Task 3.1.2b: `classify(&ProviderError) -> CatalogError`
- Exhaustive `match`; fixed phrases; full error `tracing::debug!` only.
- Files: `src/routing/model_catalog.rs`

#### Story 3.1.3: Named providers and catalog refresh orchestration (3.5 days)
**As an** operator, **I want** the catalog refreshed when I look at it (or on a timer only if I opt in), **so that** the list is fresh without background runs of credential helpers or paid calls, and one slow upstream never blocks the others.
**Acceptance Criteria**:
- Named providers are available without touching `RouterDeps`.
  - *Given* `Router` today stores `providers: Vec<Arc<dyn Provider>>` with no names (`router/mod.rs:55`), `from_config` discards names (`:557-560`) and `RouterDeps {` occurs at 36 sites (grep over `src` and `tests`, includes the definition), *When* a router is built by `from_config` from upstreams `[a, b]`, *Then* `Router::providers_named()` returns `[(UpstreamName("a"), Arc<dyn Provider>), (UpstreamName("b"), ..)]` in config order; the names arrive through a new builder `Router::with_provider_names(Vec<UpstreamName>)` (same carry-across style as `with_capability`, `:506`), `from_config` calls it, and `RouterDeps`/`Router::new` signatures are unchanged. `Router.providers` stays private. Scope is all `config.upstreams`; `select_primary_route` limits only the config warnings (Story 3.1.6).
  - *Given* a router built with `Router::new` and no builder call, or a name list whose length differs from `providers`, *Then* `providers_named()` returns an empty `Vec` (never a zip of mismatched lengths) and a `debug!` notes why.
  - `UpstreamName` is created from `UpstreamRef.name: String` (`src/routing/strategy.rs:19`) at this boundary (parse, do not re-validate downstream).
- Refresh is opt-in or on demand (default `on_demand`); `GET` has no side effects.
  - *Given* `CatalogRefreshMode::OnDemand` (default), *When* the server starts and after a route hot-swap, *Then* no `list_models` call is made (fake provider call counter == 0).
  - *Given* `GET /api/model-catalog`, *Then* it never schedules a refresh (counter stays 0 after 50 GETs). On-demand refresh is requested explicitly by a click: the Models view's "Refresh now" buttons call `POST /api/model-catalog/refresh` (all) or `?upstream=<name>`; **the view never sends a POST on open** (ADR-003 amendment; no hidden refresh). The optional `?ifStale=true` form stays on the server for interval mode and scripted callers: it refreshes only upstreams whose entry is `Loading` or older than the staleness threshold (default 30 min) and returns the cached snapshot immediately with `refreshing: true`.
  - *Given* `Interval { secs: 600 }` configured under `model_catalog.refresh`, *Then* the timer refreshes every 600 s; values below 300 s are clamped to 300 s with a `warn!`.
  - *Given* `Off`, *Then* only `POST /api/model-catalog/refresh` (without `ifStale`) triggers calls.
  - The setting and its defaults are documented in the config reference and in the Models view help text ("Refreshing runs each upstream's credential flow, which can prompt for SSO or incur API calls"; any local process or browser tab that can reach the dashboard port can request a refresh, bounded by the single-flight guard and retry floor below). The help text also states the exposure verified in code: the server listens on `127.0.0.1` only (`src/entrypoint/mod.rs:385`), the dashboard has no authentication and CORS allows any origin (`:260-263`), so a web page open in the same browser can send the POST; no new auth is added (out of scope), and the config reference carries the same note.
- Single-flight, per upstream, with an RAII guard.
  - *Given* a **gated** fake provider whose `list_models` awaits a `tokio::sync::Notify` the test releases only after all 10 requests have returned, *When* 10 `POST /api/model-catalog/refresh?ifStale=true` arrive within 1 s against stale entries, *Then* each upstream's `list_models` is called exactly once, later callers get the snapshot with `refreshing: true` and no new calls, and `catalog_refresh_coalesced_total == 9` (the counter counts POSTs that started no new work because every targeted upstream was already in flight). A fake that returns instantly would make later POSTs see fresh entries and skip instead of coalesce, so the test must use the gate.
  - The guard is an RAII type (`InFlight` per upstream in a `Mutex<HashSet<UpstreamName>>`; `Drop` removes the name), never a bare `AtomicBool`. *Given* a fake whose `list_models` panics, and separately a refresh task aborted via its `JoinHandle`, *Then* `refreshing` returns to `false` and the next POST starts a new refresh (the flag is not stuck).
  - `POST /api/model-catalog/refresh?upstream=<name>` refreshes only that upstream (other fakes' call counters stay 0); an unknown name is 404 with no upstream call; if that upstream is already in flight (for example from a full refresh) the request coalesces onto it and the response carries `coalesced: true`; a refresh-all skips upstreams already in flight and starts the rest.
- Retry floor for automatic refreshes (all error kinds).
  - *Given* an upstream whose last attempt (`lastAttemptAt`) failed with `Timeout` or a 5xx at T0, *When* an `ifStale` or interval refresh is considered at T0 + 60 s, *Then* it is skipped (`model_catalog.min_retry_secs`, default 300, measured from `lastAttemptAt`, not `refreshedAt`); at T0 + 301 s it runs once. The manual button bypasses the floor but not the 10 s throttle.
- Auth failures back off.
  - *Given* an upstream whose last refresh failed with `Auth`, *When* an on-demand or interval refresh would run, *Then* it is skipped until the manual Refresh button is used (state stays `Stale`/`Error` with the sanitized error).
- Concurrent, bounded.
  - *Given* three fake providers taking 1 s, 1 s and hanging forever, *When* `refresh_all` runs with a 10 s per-upstream timeout under `tokio::time::pause`, *Then* two entries become `Ok` at t=1 s and the third `Error{kind: Timeout}` at t=10 s (total 10 s).
- Manual refresh throttle (per upstream, measured from `lastAttemptAt`).
  - *Given* an upstream last attempted 3 s ago, *When* `POST /api/model-catalog/refresh` (all, or `?upstream=<name>`) arrives without `ifStale`, *Then* that upstream is not called and the response carries the cached snapshot with `retryAfterSecs: 7`; other upstreams outside the throttle window are still refreshed.
- A refresh in flight during a hot-swap cannot resurrect a pruned upstream.
  - *Given* a gated fake for upstream `c` with a refresh in flight, *When* `post_route` prunes the catalog to `[a, b]` and the gate is then released, *Then* no entry for `c` exists afterwards: the catalog keeps its `current` name set and its entries under one lock, `prune` replaces both, and `apply_result(name, ..)` writes only if `name` is in `current` (check and write under the same lock).
- Hot-swap prunes, never fans out by default.
  - *Given* a catalog with entries `a`, `b`, `c`, *When* `post_route` swaps in a router whose upstreams are `[a, b]`, *Then* `c` is removed (`retain` current upstream names), `a`/`b` are kept (marked `Stale` if their provider handle changed), and in `OnDemand` mode no `list_models` call is made. (Also avoids doubling the OpenRouter `/models` fetch that `build_providers` already does, `router/mod.rs:549-556`.)
- Changed event and logging.
  - *Given* a refresh finished, *Then* one `DashboardEvent::ModelCatalogChanged` is broadcast; each refresh emits exactly one log line: `info!` on success with `upstream`, `outcome=ok`, `model_count`, `duration_ms` and no model ids, or `warn!` on failure with `upstream`, `outcome=error`, `kind`, `status`, `duration_ms` and no message text (never both for one refresh).
**Files**: `src/routing/router/mod.rs` (`provider_names` field, `with_provider_names`, `providers_named`; `from_config` call), `src/routing/model_catalog.rs`, `src/config/*` (refresh mode, `min_retry_secs`), `src/entrypoint/events.rs`, `src/entrypoint/mod.rs`, `src/entrypoint/api.rs`

Tasks: **3.1.3a1** failing async tests (`#[tokio::test(start_paused = true)]`, fake `Provider`s from `router/tests.rs`, plus a gated and a panicking fake): gated single-flight, guard reset on panic/abort, prune race (0.4 d); **3.1.3a2** failing tests: per-upstream refresh, retry floor, auth backoff, modes (`on_demand`/`interval`/`off`), throttle, bounded concurrency (0.4 d); **3.1.3b** (**sequenced after Task 2.1.2d**: both edit `src/routing/router/mod.rs`, and 2.1.2d's net-line-count AC must not be measured against a moving file) `Router::provider_names` + `with_provider_names` + `providers_named()` + default/mismatch test; `from_config` carries names (0.5 d); **3.1.3c1** `refresh_all`/`refresh_one` with `JoinSet` + per-upstream timeout, one-lock `current`+entries structure so `prune` and `apply_result` cannot race (0.5 d); **3.1.3c2** RAII `InFlight` guard and per-upstream manual throttle (0.4 d); **3.1.3c3** retry floor from `lastAttemptAt` and auth backoff (0.2 d); **3.1.3d** config settings `model_catalog.refresh` (`on_demand | interval | off`, default `on_demand`) and `min_retry_secs`, docs (0.3 d); **3.1.3e1** (0.25 d) wiring: `EntrypointState.catalog` and `ifStale` / `upstream` query handling in the POST handler; **3.1.3e2** (0.25 d) optional interval loop next to the ticker (`src/entrypoint/mod.rs:167-225`, minimal lines; weak-ref/shutdown pattern from `openrouter/mod.rs:95-97`) and the hot-swap prune call in `post_route` (`api.rs:~134`, one line); **3.1.3f** `ModelCatalogChanged` variant (camelCase) + fixture `model-catalog-changed.json`; extend all-variants test (0.2 d); **3.1.3g** NFR stance docs: bind/auth/CORS note in the config reference and the Models help text string constant (shared with Story 3.1.5), backlog entry for CORS tightening (0.1 d). Task sum: 0.8 + 0.5 + 1.1 + 0.3 + 0.5 + 0.2 + 0.1 = 3.5 d (e1 + e2 re-slice the former 0.5 d task e).

#### Story 3.1.4: Catalog HTTP endpoints (1.2 days)
**As a** UI, **I want** `GET /api/model-catalog` and `POST /api/model-catalog/refresh`, **so that** I can render state and request a refresh.
**Acceptance Criteria**:
- Typed envelope with ages (and a `refreshing` flag).
  - *Given* catalog with `anthropic: Ok (42 models, refreshedAt 2026-10-09T14:00:00Z)`, `bedrock: Stale (31 cached, error Auth 403)`, *When* `GET /api/model-catalog`, *Then* 200 JSON `{"upstreams":{"anthropic":{"state":"ok","totalCount":42,...,"refreshedAt":"2026-10-09T14:00:00Z"},"bedrock":{"state":"stale","error":{"kind":"auth","status":403,"message":"..."}}},"configWarnings":[]}`.
- GET is a pure, non-blocking read.
  - *Given* `OnDemand` mode, a stale entry and a fake upstream that hangs, *When* `GET /api/model-catalog` is called, *Then* it returns within 100 ms with the cached snapshot and makes no upstream call; *When* `POST /api/model-catalog/refresh?ifStale=true` is called, *Then* it also returns within 100 ms (`refreshing: true`) and schedules one background refresh.
- Per-upstream refresh endpoint.
  - *Given* upstreams `anthropic` and `bedrock` with gated fakes, *When* `POST /api/model-catalog/refresh?upstream=bedrock` is called, *Then* it returns within 100 ms with `{"refreshing":true,"coalesced":false,"retryAfterSecs":null,"upstreams":{..}}` (the same snapshot shape as GET plus those three fields) and only `bedrock`'s fake is called; `?upstream=nope` returns 404 `{"error":"unknown upstream"}`; the same call while `bedrock` is in flight returns `coalesced:true` and makes no new call; each `UpstreamModels` entry carries its own `refreshing` flag so the UI can show busy per row.
- Legacy endpoint unchanged.
  - *Given* the existing `get_models` tests, *When* run, *Then* unchanged and passing.
- Secret test at HTTP level.
  - *Given* fake upstream error body with `SENTINEL`, *When* `GET /api/model-catalog` and an SSE frame are collected, *Then* neither contains `SENTINEL`.
**Files**: `src/entrypoint/models.rs` (new), `src/entrypoint/mod.rs` (route lines only), `tests/webui_integration_test.rs`

##### Task 3.1.4a: Failing integration tests
- Add to `tests/webui_integration_test.rs` following `test_dashboard_sessions_api` (`:~110`): GET shape, refresh throttle, per-upstream refresh (known, unknown, coalesced), sentinel check (0.2 d of the story's 1.2 d is the per-upstream cases).
- Files: `tests/webui_integration_test.rs`

##### Task 3.1.4b: Handlers
- `get_model_catalog`, `post_model_catalog_refresh` in `src/entrypoint/models.rs`; add `catalog: Arc<ModelCatalog>` to `EntrypointState` (`src/entrypoint/mod.rs:~106`).
- Files: `src/entrypoint/models.rs`, `src/entrypoint/mod.rs`

##### Task 3.1.4c: Route registration and gate
- Two route lines near `:259-356`; fmt/clippy/test.
- Files: `src/entrypoint/mod.rs`

#### Story 3.1.5: Upstream Models view (2.8 days)
**As an** operator, **I want** a page listing models per upstream with age and error state, **so that** I can see what each upstream offers.
**Acceptance Criteria**:
- States are text + glyph, never "0 models".
  - *Given* store data `bedrock: stale, 31 models, refreshedAt 2h ago, error auth 403`, *When* rendered, *Then* header reads `bedrock  STALE  31 models  refreshed 2h ago  auth failed (403)` and the 31 ids remain listed; *Given* `gemini: error` with no models, *Then* `ERROR` + Retry button and no "0 models" text; *Given* `unsupported`, *Then* `NOT SUPPORTED`.
- Search/filter and large lists.
  - *Given* 310 OpenRouter models, *When* typing `gpt-4o`, *Then* only matching ids render and matching sections auto-expand; the list renders at most 200 rows until "Show more" is pressed.
- Opening the page never refreshes (UX-7; ADR-003 amendment).
  - *Given* the Models route opens, *Then* the store issues exactly one `GET /api/model-catalog` and **no POST**; cached entries render with their age (`refreshed 3m ago`); an upstream that was never refreshed (`Loading` with `refreshing: false`, `lastAttemptAt` null) renders "Not loaded yet - press Refresh now" (not a spinner and not "0 models"); when every upstream is never-refreshed the page leads with one prominent "Refresh now" button and the credential-flow help text. A spinner/skeleton shows only while `refreshing: true`.
- Refresh and live update.
  - *Given* the view is open, *When* `ModelCatalogChanged` arrives, *Then* the store refetches once; *When* "Refresh now" (all) or one upstream's Refresh is clicked (no `ifStale`; the latter calls `?upstream=<name>`), *Then* the POST returns at once with `refreshing: true`, so busy does **not** end on the response: the affected button(s) stay disabled with `aria-busy="true"` and text "Refreshing" until a `ModelCatalogChanged` event is followed by a refetch in which that upstream's `refreshing` is `false` (equivalently its `lastAttemptAt` advanced); a 15 s safety timeout (above the 10 s per-upstream timeout) clears busy and shows "Refresh status unknown"; a `coalesced: true` response shows "Refresh already running" and keeps busy until the same condition; a `retryAfterSecs` response clears busy at once with "Try again in Ns".
- Complete state set (UX-2). Each state has the stated text and an exit action:
  - *Given* a healthy upstream that returned an empty list, *Then* the row reads `OK` and "Upstream reports 0 models." (distinct from `ERROR`, `NOT SUPPORTED` and "Not loaded yet"), with a Refresh action.
  - *Given* a catalog with no upstreams, *Then* the page reads "No upstreams configured." with a link "Open Config Editor" to `/config`.
  - *Given* `GET /api/model-catalog` fails (network error or 5xx), *Then* a page-level error "Could not load the model catalog (<status or offline>)." with a Retry button that refetches; SSE state is independent and not mentioned.
  - *Given* a search with no hits, *Then* "No models match 'x'." with a Clear search button; *Given* a search with hits, *Then* the shared status region announces "N matches" (throttled to the last value after 300 ms of no typing).
  - *Given* a refresh response with `coalesced: true`, *Then* that row shows the text "Refresh already running" (not only a disabled button) until its busy condition ends.
- Each upstream row has its own Refresh button (UX-05, one click) next to the page-level Refresh now (all).
  - *Given* two upstream rows, *When* `bedrock`'s Refresh is pressed, *Then* exactly one `POST ...refresh?upstream=bedrock` is sent and only that row goes busy.
- Refresh behavior is explained.
  - *Given* the Models view, *Then* help text states the active refresh mode and that refreshing runs each upstream's credential flow (can prompt for SSO or incur API calls); in `on_demand` mode the page shows "refreshed Nm ago" and a Refresh button, not an auto-timer claim.
- Navigation.
  - *Given* the app shell, *Then* a "Models" nav entry routes to `/models` (served under base-href `/dashboard/`, so the browser path is `/dashboard/models`, the path `design/ux.md` uses).
- Scope of the per-model view.
  - *Given* the Models view, *Then* it shows per-upstream sections with search, the refresh controls and the pinned-model warning block (Story 3.1.6); it does **not** show per-model "in config" markers, "used in rule/alias" annotations or an All/Config-only/Errors filter (removed from `design/ux.md` S4; backlog).
**Files**: `ui/src/app/core/stores/model-catalog.store.ts` (new) + spec, `ui/src/app/features/models/upstream-models-view.component.ts` (new) + spec, `ui/src/app/app.routes.ts`, `ui/src/app/core/components/app-shell.component.ts`, `ui/src/app/core/services/sse.service.ts` (event dispatch)

Tasks (every task <= 0.5 d; sum 0.5 + 0.4 + 0.5 + 0.5 + 0.4 + 0.2 + 0.3 = 2.8 d):

##### Task 3.1.5a (0.5 d): Failing store and component specs
- Specs with `HttpTestingController` (busy lifecycle: busy survives the POST response and clears on `refreshing: false`, the 15 s safety timeout, `coalesced` and `retryAfterSecs`; **no POST on open**; never-refreshed prompt; the full state set above) and the fixtures `model-catalog.json` (add under `ui/src/testing/fixtures/api/`, also asserted by a Rust test via `include_str!` so the endpoint shape is contract-pinned like the SSE one).
- Files: specs, fixtures

##### Task 3.1.5b (0.4 d): `ModelCatalogStore`
- Signal store copying `config.store.ts`/`benchmark.store.ts` (`isLoading`, `error`); single GET on open; refetch on `ModelCatalogChanged` via `MetricsStore.handleDashboardEvent` dispatch; no POST except from explicit refresh methods.
- Files: `model-catalog.store.ts`, `metrics.store.ts`

##### Task 3.1.5c1 (0.5 d): View component core and route
- Discriminated-union rendering of `CatalogState`; disclosure buttons with `aria-expanded`; search; pagination (200 rows); `<time datetime>` for ages.
- Files: `upstream-models-view.component.ts`, `app.routes.ts`

##### Task 3.1.5c2 (0.5 d): State set
- Healthy-empty, no upstreams + Config Editor link, page-level 5xx error with Retry, search no-match + Clear, "N matches" announcement, "Refresh already running" row text.
- Files: `upstream-models-view.component.ts`

##### Task 3.1.5c3 (0.4 d): Explicit refresh controls
- "Refresh now" page-level and per row, busy lifecycle, never-refreshed prompt, help text from the Task 3.1.3g constant.
- Files: `upstream-models-view.component.ts`, `model-catalog.store.ts`

##### Task 3.1.5d (0.2 d): Gate
- `ng test`; `CONSOLETTE_REQUIRE_UI=1 cargo build`; screenshot with one failing, one healthy and one never-refreshed upstream.
- Files: none

##### Task 3.1.5e (0.3 d): Nav entry and shell
- "Models" nav entry in the app shell routing to `/models`; shell spec.
- Files: `app-shell.component.ts`, spec

#### Story 3.1.6: Warn when config references a model the upstream does not list (1.1 days, never cut: UX-04 is Must)
**As an** operator, **I want** a warning for config entries pointing at unlisted models, **so that** typos and deprecations are caught.
**Acceptance Criteria**:
- Scope is the primary route only (`select_primary_route`); other routes' pins are not checked and the UI says so. The warning block is the only config cross-reference in the Models view (UX-04 is met for primary-route pinned models in 2 steps once the catalog has been refreshed - open page, read the block - and 3 steps when it has never been refreshed, because opening the page does not refresh: open, press Refresh now, read).
- Warning for pinned model absent upstream.
  - *Given* route upstream `openai` pinned to `gpt-5-turbo` and catalog `openai: Ok` without that id, *When* `GET /api/model-catalog`, *Then* `configWarnings` contains `{"upstream":"openai","model":"gpt-5-turbo","referencedBy":"route <name> upstream 0"}`.
- No false positives.
  - *Given* `anthropic` upstream in `Error` or `Loading`, or a `model_family` upstream, *Then* no warnings for it.
- Bedrock comparison uses normalization.
  - *Given* route model `us.anthropic.claude-3-5-sonnet-20241022-v1:0` and catalog id `anthropic.claude-3-5-sonnet-20241022-v1:0`, *Then* no warning (compared after `BedrockProvider::normalize_model`, per Task 1.1.1b).
- UI shows the block, and says what it does not check (product/UX triad).
  - *Given* a warning, *Then* a "Referenced by config but not listed upstream (1)" block appears with text, not color alone.
  - *Given* the block is rendered (with or without warnings), *Then* a visible caveat reads "Only models pinned on the first route are checked. Models reached through other routes (ignored by the router, `select_primary_route` warns and uses the first), model-family routes and fallback targets that live outside this route are not checked, so no warning here is not proof that every target exists." A spec asserts the caveat text is present both with zero and with one warning. (VERIFIED: `select_primary_route`, `src/routing/router/mod.rs:328-340`, takes the first route and logs a warning for the rest.)
**Files**: `src/routing/model_catalog.rs`, `src/entrypoint/models.rs`, `upstream-models-view.component.ts` + spec

##### Task 3.1.6a: Failing tests
- Rust tests for the three backend ACs; UI spec for the block.
- Files: `src/routing/model_catalog.rs`, spec

##### Task 3.1.6b: Implement `config_warnings(route, catalog)`
- Walk `RouteUpstreamRef` entries (`model`, `model_family`), compare ids per Task 1.1.1b rules; include in the GET envelope.
- Files: `src/routing/model_catalog.rs`, `src/entrypoint/models.rs`

##### Task 3.1.6c: UI block, unchecked-targets caveat (+0.1 d over the earlier 1.0 d story) + gate; open Draft PR "Epic 3"
- Files: `upstream-models-view.component.ts`

---

## Phase 4: Gap fixes from features.md (Epic 4)

### Epic 4.1: Remove dead ends, fabrications and silent failures
**Goal**: Fix the highest-value gaps that are small and high-trust-value; anything not finished goes to the gap report backlog. **Effort: 11.2 days.**
**Ordering rule (revised)**: the story order below is the *provisional* ranking from `research/features.md`. Story 5.1.3 (live exploratory run) is executed after Epics 1-3 land and **before any Epic 4 story starts**; Story 4.1.0 then re-ranks, adds, drops or re-sizes Epic 4 stories from that evidence. Reprioritization is explicitly allowed; the cut order in "Effort and Appetite" applies to whatever remains.

#### Story 4.1.0: Re-rank Epic 4 from the exploratory pass (0.5 day)
**As a** maintainer, **I want** Epic 4's scope chosen from live evidence, **so that** effort goes to the gaps that actually hurt.
**Acceptance Criteria**:
- *Given* `gap-report.md` rows from Story 5.1.3, *When* the re-rank finishes, *Then* each Epic 4 story is marked keep / drop / re-size with a one-line reason, and new gaps with severity High and effort <= 1 day are added as stories (or consciously sent to backlog), recorded in a "Re-rank" section of `gap-report.md`.
**Files**: `project_plans/webui-trace-model-mapping/gap-report.md`, this plan

Tasks: **4.1.0a** read the gap report and score each story by severity x effort (0.3 d); **4.1.0b** update this plan's Epic 4 list and the Effort table (0.2 d).

#### Story 4.1.1: Session Replay loads payloads for the right id; Inspect deep link works (gaps 6, 14) (1.0 day)
**As an** operator, **I want** clicking a session to load its payloads, **so that** replay is not a 404.
**Acceptance Criteria**:
- Session summary exposes the latest request id.
  - *Given* a session whose newest ring-buffer request is `req_77ab01cd`, *When* `GET /v1/dashboard/sessions`, *Then* the item has `latestRequestId: "req_77ab01cd"` (additive; existing keys unchanged).
- UI calls `/requests/{requestId}`.
  - *Given* a click on that session, *When* the component loads payloads, *Then* the HTTP call is `GET /requests/req_77ab01cd?stage=original`, not `/requests/<sessionId>`.
- Missing payload is a labelled state.
  - *Given* a 404 for `stage=compressed`, *Then* the pane reads "Compressed payload not captured" with the reason from the trace: "compression is off" when `compressionStatus` is `off`, "nothing was compressed" when `none` (in this project only `off` occurs; the compressed-body store is the deferred Epic 6, Story 6.1.4).
**Files**: `src/entrypoint/observability.rs`, `ui/src/app/features/sessions/session-replay-view.component.ts`, its spec, `ui/src/app/core/stores/session.store.ts`

##### Task 4.1.1a: Failing tests (Rust + UI)
- Files: `tests/webui_integration_test.rs`, `session-replay-view.component.spec.ts`

##### Task 4.1.1b: Add `latest_request_id` to `SessionSummary`
- Files: `src/entrypoint/observability.rs` (`:55-68`, builder `:72-156`)

##### Task 4.1.1c: Use it in UI; labelled empty state
- Files: `session-replay-view.component.ts`, `session.store.ts`

#### Story 4.1.2: Remove fabricated data (gaps 8, 9) (1.5 days)
**As an** operator, **I want** missing data shown as missing, **so that** I never act on invented numbers.
**Acceptance Criteria**:
- No seed rows.
  - *Given* an empty ring buffer, *When* `GET /v1/dashboard/benchmark`, *Then* `[]` and the UI shows "No traffic yet - benchmarks appear after requests flow".
- No constant success/cost.
  - *Given* ring-buffer requests only, *Then* `successRatePercent`, `errorRate`, `costPer1k` are `null` and the UI renders an en dash; *When* failure traces exist (Story 1.1.3), *Then* error rate follows the Story 2.1.6 definition: `statusCode >= 400 && != 499` over routed + local traces in the same window, cache hits excluded from the denominator, cancelled counted separately.
- No hardcoded timeline/token bar.
  - *Given* a selected session, *Then* the Pipeline timeline and token bar render only if real values exist, else "Not recorded"; the strings `Ingress` with `2` ms constant and `1250`/`450`/`320` no longer appear in the component source.
**Files**: `src/entrypoint/observability.rs` (`:181-299`), `ui/src/app/features/benchmarks/model-benchmarks-view.component.ts`, `ui/src/app/features/sessions/session-replay-view.component.ts`, specs

Tasks (sum 0.3 + 0.4 + 0.3 + 0.25 + 0.25 = 1.5 d):
##### Task 4.1.2a (0.3 d): Failing tests
- Files: `tests/webui_integration_test.rs`, specs

##### Task 4.1.2b1 (0.4 d): Backend: delete benchmark seed rows (`observability.rs:181-299`), make `successRatePercent`, `errorRate`, `costPer1k` `Option` (null without data)
- Files: `src/entrypoint/observability.rs`

##### Task 4.1.2b2 (0.3 d): Backend: error rate computed from trace statuses per the Story 2.1.6 definition (`>= 400 && != 499`, routed + local, cache hits out of the denominator, cancelled separate)
- Files: `src/entrypoint/observability.rs`, `src/metrics/mod.rs`

##### Task 4.1.2c1 (0.25 d): UI benchmark empty state and en dash for null rates
- Files: `model-benchmarks-view.component.ts`, spec

##### Task 4.1.2c2 (0.25 d): UI session pipeline timeline and token bar render only real values, else "Not recorded"
- Files: `session-replay-view.component.ts`, spec

#### Story 4.1.3: Config Editor stops lying (gap 10) (1.5 days, cut-able)
**As an** operator, **I want** saves to either persist or be clearly unavailable, **so that** a success toast means success.
**Acceptance Criteria**:
- Rate-limit section read-only (default decision).
  - *Given* the Config page, *When* rendered, *Then* rate-limit inputs are disabled with note "Rate limits are configured in the route file; editing is not supported here" and a save does not toast success for them. (If Tyler upgrades to "persist", replace with a PUT round-trip test.)
- Bearer-auth upstream keeps its auth type.
  - *Given* an upstream configured with `Bearer` auth, *When* `PUT /v1/dashboard/config` changes only its weight, *Then* the saved upstream still has `Bearer` auth (not `Apikey` + `x-api-key`).
- Invented fallback providers removed.
  - *Given* a config without `providers`, *Then* the editor shows an empty state, not `anthropic`/`openai` rows (`config-editor-view.component.ts:139-142`).
**Files**: `src/entrypoint/observability.rs` (`:466-469`, `:521-583`), `ui/src/app/features/config/config-editor-view.component.ts` + spec

Tasks (sum 0.4 + 0.4 + 0.2 + 0.3 + 0.2 = 1.5 d):
##### Task 4.1.3a (0.4 d): Failing tests
- Rust test for auth preservation (0.2 d); UI specs for disabled inputs and no invented rows (0.2 d).
- Files: `tests/webui_integration_test.rs` or `observability.rs` tests, spec

##### Task 4.1.3b1 (0.4 d): Preserve auth on PUT
- Only rewrite auth when the key actually changed; keep the existing `Auth` variant/header (`observability.rs:521-583`).
- Files: `src/entrypoint/observability.rs`

##### Task 4.1.3b2 (0.2 d): Round-trip tests for every auth variant (Apikey, Bearer, none) through PUT then GET
- Files: `tests/webui_integration_test.rs`

##### Task 4.1.3c1 (0.3 d): UI read-only rate limits with the explanatory note and no success toast for them
- Files: `config-editor-view.component.ts`

##### Task 4.1.3c2 (0.2 d): Remove invented `anthropic`/`openai` fallback providers (`:139-142`), empty state
- Files: `config-editor-view.component.ts`

#### Story 4.1.4: Real provider health and tick fields (gaps 2, 13, 18) (1.0 day, cut-able)
**As an** operator, **I want** health and KPIs from real data, **so that** the overview grid and cards mean something.
**Acceptance Criteria**:
- Health reflects `HealthRegistry`.
  - *Given* upstream `bedrock` in cooldown with 120 s remaining, *When* the ticker emits, *Then* `providerHealth.bedrock == "cooldown"` (values: `healthy | cooldown | unhealthy`) instead of constant `"healthy"` (`src/entrypoint/mod.rs:208-210`).
- Latency KPI backed and labelled.
  - *Given* 20 traces with `timeToHeadersMs` p50 = 800, *Then* the tick has `medianLatencyMs: 800` and the card reads "Median latency (to headers)"; `tps` is `null` when not computable and the card shows an en dash; `tokenSavingsPercent` is `null` and the card reads `n/a (compression off)` in this project (compression wiring is the deferred Epic 6; the field is computed from `compressed` traces when it lands); `medianLatencyMs` is joined by `medianFirstByteMs`, which is `null` and reads `n/a (not measured)` until the deferred first-byte measurement exists.
- No fake provider cards.
  - *Given* `providerHealth` empty, *Then* the grid shows "No upstreams reported", not Anthropic/AWS Bedrock/OpenAI (`provider-health-grid.component.ts:83-87`).
**Files**: `src/entrypoint/mod.rs`, `src/entrypoint/events.rs`, `src/routing/health.rs`, `ui/src/app/features/overview/components/provider-health-grid.component.ts`, `overview-dashboard.component.ts`, fixtures `metrics-tick.json`

Tasks (sum 0.3 + 0.4 + 0.3 = 1.0 d):
##### Task 4.1.4a (0.3 d): Failing tests
- Rust ticker unit test with a fake health registry; UI spec for grid/KPI; fixture update.
- Files: `src/entrypoint/mod.rs`, specs, fixture

##### Task 4.1.4b (0.4 d): Backend tick fields
- Read health snapshot (`src/routing/health.rs`), compute median from `MetricsCollector.histogram`; rename the old `medianTtftMs` (it never had a TTFT source; the UI calls first-token time out of scope) -> `medianLatencyMs` in model.
- Files: `src/entrypoint/mod.rs`, `src/entrypoint/events.rs`

##### Task 4.1.4c (0.3 d): UI cards and grid
- Files: UI components

#### Story 4.1.5: The UI consumes the rest of the event stream; honest reconnect (gaps 15, 20, 12) (1.6 days: core 0.75 never cut, remainder 0.85 cut-able)
**As an** operator, **I want** error events, lag, config changes and disconnects surfaced, **so that** a quiet UI does not hide a dead feed.
**Acceptance Criteria**:
- **Cut-able part (cut #5, 0.85 d incl. the Recent errors states):** `ConfigChanged` refreshes the Config page.
  - *Given* the Config page open, *When* `ConfigChanged{routeName:"default",strategy:"weighted"}` arrives, *Then* `ConfigStore.load()` is called once.
- **Core, never cut (UX-16, UX-11):** `system_lag` shows a chip.
  - *Given* `system_lag {skipped: 37}`, *Then* the Overview shows "feed lagging - 37 events skipped" (`role="status"`; the backend payload is `{"skipped": n}`, `src/entrypoint/events.rs`, so no milliseconds exist); *When* the next non-lag event arrives, *Then* the chip clears.
- **Core, never cut:** retries exhausted is distinct and recoverable.
  - *Given* 20 failed reconnect attempts (`maxRetryAttempts`), *Then* status is `disconnected`, a `role="alert"` banner "Live feed disconnected. Data below is stale." with a Reconnect button appears, and pressing it resets the counter and reconnects.
- **Cut-able part (cut #5):** `ErrorLogged` surfaces.
  - *Given* `ErrorLogged{provider:"openrouter", errorType:"rate_limited", statusCode:429, message:"..."}`, *Then* the last 20 appear in a small "Recent errors" list on Overview (newest first, each row text + glyph + time, `message` shown only as the backend's already-sanitized string).
  - *Given* no `ErrorLogged` since page load, *Then* the list reads "No errors since this page loaded." (not blank, not hidden); *Given* more than 20, *Then* the oldest drops and the footer reads "Showing latest 20"; the list has a caption and is a plain list (no `aria-live`; arrivals go through the shared status region as "N new errors" at most once per second).
**Files**: `ui/src/app/core/services/sse.service.ts`, `ui/src/app/core/stores/metrics.store.ts`, `ui/src/app/core/stores/config.store.ts`, `ui/src/app/features/overview/overview-dashboard.component.ts`, specs

##### Task 4.1.5a: Failing specs (core: lag chip, retries exhausted + Reconnect; rest: ConfigChanged, ErrorLogged)
- Files: `sse.service.spec.ts`, `metrics.store.spec.ts`, overview spec

##### Task 4.1.5b: Store handling and manual reconnect
- Files: `sse.service.ts`, `metrics.store.ts`, `config.store.ts`

##### Task 4.1.5c1 (0.3 d, core): UI banners (disconnected alert, Reconnect, lag chip)
- Files: `overview-dashboard.component.ts`

##### Task 4.1.5c2 (0.3 d, cut-able): Recent errors list with the empty and overflow states
- Files: `overview-dashboard.component.ts`

#### Story 4.1.6: Trace history survives a page refresh (gap 16) (1.0 day, cut-able)
**As an** operator, **I want** the last traces on load and after a reconnect, **so that** refresh does not erase context and missed events are recovered.
**Acceptance Criteria**:
- Backfill endpoint returns the same wire shape as SSE.
  - *Given* 3 completed requests in the ring buffer, *When* `GET /v1/dashboard/traces?limit=50`, *Then* 200 with an array of 3 objects each equal in shape to `request-trace-success.json` `data` (fixture-pinned, ADR-001), newest first.
- Store seeds and merges.
  - *Given* a page load, *When* `MetricsStore` initializes, *Then* it calls the endpoint once and upserts by `requestId` using the `traceRevision` rule (Story 1.1.2: equal-or-higher revision wins, so an older backfill snapshot never overwrites a newer live row, e.g. a measured `firstByteMs` with `pending`); *Given* a reconnect after `system_lag` or `disconnected`, *Then* it calls it again and no duplicates appear.
**Files**: `src/entrypoint/observability.rs` (or `entrypoint/models.rs`-style new `entrypoint/traces.rs`), `src/entrypoint/mod.rs` (route line), `ui/src/app/core/stores/metrics.store.ts`, specs

##### Task 4.1.6a: Failing tests
- Files: `tests/webui_integration_test.rs`, `metrics.store.spec.ts`

##### Task 4.1.6b: Endpoint from ring buffer
- Build `RequestTraceData` with the existing `RequestTraceData::from_detail(&RequestDetail)` (Story 1.1.2b / 2.1.3); no second builder.
- Files: `src/entrypoint/events.rs`, new handler file, `src/entrypoint/mod.rs`

##### Task 4.1.6c: Store seeding and merge
- Files: `metrics.store.ts`, `sse.service.ts`

#### Story 4.1.7: Accessibility pass (never cut in any part: UX-18, UX-22/23, UX-24 and UX-27 are Must) (3.1 days = 7a 0.3 + 7b 0.6 + 7c 0.6 + 7d 0.3 + 7e 0.3 + 7f 0.7 + 7g 0.3)
**As the keyboard-first operator of a single-operator loopback tool (Tyler), I want** semantic structure, visible focus, legible text and no jumping rows, **so that** the dashboard is usable the way I actually use it (keyboard, dark theme, long sessions). **Rationale (product triad)**: the Must items are tied to that user need, not to a compliance target: a visible focus ring and 24x24 targets (UX-18) because he drives the table with the keyboard; 4.5:1 contrast (UX-24) because the current neutral-500/600 text measures 3.78 and 2.29 (`research/ux.md`) on the dark background, a legibility defect for the only operator; held updates and one status region (UX-22/UX-23) because a live table that moves under a focused row makes Inspect unusable; reduced motion (UX-27) because it is one class change. The breadth extras (Task 4.1.7f2 forced-colors and non-text-contrast checks, Task 4.1.7g reflow) are in the no-cuts baseline but are the first descope candidates if the Must items slip.
**Acceptance Criteria**:
- Table semantics and labels.
  - *Given* the trace table, *Then* it has `<caption class="sr-only">Live request traces</caption>`, the Request ID cell is `<th scope="row">`, every Inspect button has `aria-label="Inspect trace <id8>"`.
- Contrast (Tasks 4.1.7b2 and 4.1.7f1; automated, not grep-only), including dimmed rows.
  - *Given* informational text classes, *Then* none use `text-neutral-500`, `text-neutral-600` or `text-slate-500` on the dark backgrounds (computed ratios 3.78, 2.29, 4.08 fail AA; neutral-400 is 7.11 per `research/ux.md`); a class-string grep spec asserts they are absent from `ui/src/app/features/**`, **and** the in-house a11y spec (Task 4.1.7f1) computes the contrast ratio of every rendered text node's `getComputedStyle` color against its resolved background and fails below 4.5:1 (3:1 for 18px+ or bold 14px+ text).
  - *Given* the table in the stale states (reconnecting and disconnected, rows "kept and dimmed"), *Then* the dimming is done with a color token that keeps every text node at or above 4.5:1 against its background (not `opacity`, which silently lowers contrast), and the same computed-contrast spec runs on both stale states and on a paused-with-waiting-rows state.
- Live region, Pause and held updates (Tasks 4.1.7c1 and 4.1.7c2; UX S2 is Must, UX-22 and UX-23 depend on it).
  - *Given* 12 traces arrive within 1 s, *Then* one polite `role="status"` message "12 new traces" is announced (not 12).
  - *Given* the Pause control, *Then* it is a single-signal toggle: the visible label swaps "Pause" / "Resume" and **there is no `aria-pressed`** (a label swap plus `aria-pressed` double-announces "Resume, toggle button, pressed"); the waiting count "N new traces waiting" is in the status region. Pause is manual: it holds until Resume.
  - *Mechanism for UX-23 (auto-hold)*: *Given* keyboard focus is inside the table element (any row control focused) and no manual pause, *When* new traces arrive, *Then* new rows are **held** in a buffer instead of being inserted (so no row moves under the focused control), updates to rows already shown apply in place (no row moves), the status region announces "N new traces waiting" (first hold immediately, then at most once per second) and a visible "Show N new" button appears in the table header; *When* focus leaves the table or "Show N new" is activated, *Then* the held rows are inserted newest-first in one step and "N new traces" is announced once. Manual Pause is independent: if both are active, Resume does not flush while focus is still inside the table. Specs: focus a row, push 5 traces, assert row identity and DOM order unchanged and the announcement; blur, assert flush; Pause then focus-out, assert no flush until Resume.
- Reduced motion and min size (Task 4.1.7d).
  - *Given* `prefers-reduced-motion: reduce`, *Then* the connection dot does not animate (`motion-safe:animate-pulse`); chips use >= 11px.
- Visible focus ring and target size (Task 4.1.7e; UX-18, UX-5).
  - *Given* every interactive control (Inspect, copy, Pause/Resume, Show N new, Reconnect, Refresh buttons, section toggles, model search, attempt rows, chip and dash legend disclosure, drawer close, the scroll regions below), *Then* it has a `focus-visible:` outline of at least 2 px whose color contrasts at least 3:1 with the adjacent background (the shared Tailwind class `focus-visible:outline focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-sky-300` is asserted by class-string spec on each component, and the computed outline color/width is asserted in the a11y spec after `element.focus()`; INFERRED that headless Chrome applies `:focus-visible` to programmatic focus, so the Manual keyboard pass in Story 5.1.3 measures the real ring with devtools) and a bounding box of at least 24x24 CSS px. n/a dashes are deliberately **not** controls (no tab stop, Story 1.1.4).
- Reflow at 320 px and 200% zoom (Task 4.1.7g; WCAG 1.4.10).
  - *Given* a 320 px-wide viewport (equivalent to 200% zoom on a 640 px-wide window; the Karma spec sets the host width to 320 px and a second case to 640 px with `zoom: 2`), *Then* the Overview page, the Models page and the drawer sheet have no page-level horizontal scroll (`document.documentElement.scrollWidth <= clientWidth`); the wide trace table scrolls horizontally inside one **focusable, named scroll region** (`role="region"`, `aria-label="Live request traces, scroll horizontally"`, `tabindex="0"`, with the shared focus ring) so keyboard users can scroll it and nothing is clipped; on the Models page each upstream section stacks (name, state and Refresh wrap onto separate lines) and long model ids wrap (`overflow-wrap: anywhere`) with no horizontal scroll. The spec asserts the region's role, name, `tabindex` and `scrollWidth > clientWidth` for the table, and `scrollWidth <= clientWidth` for the Models list with a 120-character model id.
- Automated a11y checks without a new dependency (Tasks 4.1.7f1 and 4.1.7f2).
  - *Given* the repo convention "no new npm packages", *Then* a small in-house `ui/src/testing/a11y-checks.ts` provides `expectAccessibleNames(root)`, `expectContrast(root)`, `expectFocusRing(root)`, `expectTargetSize(root)` (Task 4.1.7f1) and `expectNonTextContrast(root)` and `expectForcedColorsSafe(root)` (Task 4.1.7f2) over Karma-rendered components (the same helper backs UX-12, UX-18, UX-24, UX-26); each Overview, Models, drawer, Session, Benchmark and Config component spec calls them once. Adding `axe-core` as a devDependency (not present in `ui/package.json`, VERIFIED by grep) is an option that needs Tyler's approval; default is no. The Story 5.1.3 manual pass still covers screen-reader read-through, real keyboard use and a Chrome devtools forced-colors emulation.
  - *Accessible name for the Models search*: *Given* the Models page, *Then* the search input has an accessible name ("Search models", from a visible or `sr-only` `<label>`, not a placeholder alone) and `expectAccessibleNames` fails on any input, button or region without one.
  - *Non-text contrast (WCAG 1.4.11)*: *Given* the waterfall bars, the status/outcome glyphs, the focus ring and the chip borders, *Then* `expectNonTextContrast` asserts at least 3:1 against the adjacent background for each (bars also carry a text value, UX-26, so they are decorative for assistive tech but still meet 3:1).
  - *Forced colors*: *Given* the stylesheet source, *Then* `expectForcedColorsSafe` asserts that no state, chip, bar or glyph is distinguished by `background-color` or `color` alone (each also has a border, text or glyph) and that nothing sets `forced-color-adjust: none`; the Manual pass confirms with devtools "Emulate CSS forced-colors: active" that status text, chips, the focus ring and waterfall bars stay visible. (Karma cannot toggle the forced-colors media feature, so the automated half is a source-level assertion; this is stated, not hidden.)
- Carry-over for the deferred Epic 6 (not built or counted in this project).
  - *When* Epic 6 lands and the table starts showing `pending`, `n/a (stream ended before first byte)`, `n/a (client closed before first byte)` and `n/a (synthesized stream)`, *Then* those states must (a) appear in the footer dash legend and the row-level `aria-describedby` description, (b) pass `expectContrast` and `expectNonTextContrast` in their fixtures, (c) announce a `pending` to `measured` change only through the shared status region (at most once per second), and (d) keep the dash non-focusable. The Epic 6 stories (6.1.5e) carry these as ACs; this project's validation does not test them.
**Files**: components under `ui/src/app/features/**`, `app-shell.component.ts`, `ui/src/testing/a11y-checks.ts` (new), specs

##### Task 4.1.7a (0.3 d): Failing a11y specs and class-string grep test
- Files: specs

##### Task 4.1.7b1 (0.3 d): Roles, labels, caption, row headers, Models search label
- Files: feature components

##### Task 4.1.7b2 (0.3 d): Contrast replacement (class swap) and token-based dimming for stale/disconnected rows
- Files: feature components, shared style constants

##### Task 4.1.7c1 (0.3 d): Live region + single-signal Pause/Resume toggle
- Files: `recent-trace-table.component.ts`

##### Task 4.1.7c2 (0.3 d): UX-23 auto-hold while focus is inside the table, "Show N new" and the announcements
- Files: `recent-trace-table.component.ts`, `metrics.store.ts` (held buffer)

##### Task 4.1.7d (0.3 d): Reduced motion, chip sizes; open Draft PR "Epic 4"
- Files: `app-shell.component.ts`, components

##### Task 4.1.7e (0.3 d): Shared `focus-visible` ring class on every control, 24x24 minimum target size
- Files: feature components, shared style constants

##### Task 4.1.7f1 (0.5 d): In-house `a11y-checks.ts` core (`expectAccessibleNames`, `expectContrast`, `expectFocusRing`, `expectTargetSize`) and its adoption in the component specs
- Files: `ui/src/testing/a11y-checks.ts`, specs

##### Task 4.1.7f2 (0.2 d): `expectNonTextContrast`, `expectForcedColorsSafe` and the Models search name assertion
- Files: `ui/src/testing/a11y-checks.ts`, specs

##### Task 4.1.7g (0.3 d): Reflow at 320 px / 200% zoom: named focusable scroll region around the trace table, stacked Models rows, wrapped ids, and the spec
- Files: `recent-trace-table.component.ts`, `upstream-models-view.component.ts`, specs

---

## Phase 5: Exploratory pass and gap report (Epic 5)

### Epic 5.1: Prove it live and report what is left
**Goal**: Run the 14 scenarios from `research/features.md` section 6 against the built app, commit a gap report, and add cheap automated coverage for what the pass confirms. **Effort: 5.3 days.** **Sequencing change**: Story 5.1.3 (exploratory run) executes after Epics 1-3 land and before Epic 4 (feeding Story 4.1.0); Stories 5.1.1, 5.1.2 and 5.1.4 close the project.

#### Story 5.1.1: Rust end-to-end test with a fake upstream (1.8 days; the client-abort scenario moved to Task 1.1.3k)
**As a** maintainer, **I want** an integration test of request -> trace -> catalog, **so that** the core promise regresses loudly.
**Acceptance Criteria**:
- Fallback chain end to end.
  - *Given* an in-process axum fake upstream A returning 529 and B returning a valid Anthropic response, with a route `[A, B]`, *When* `POST /v1/messages` with `model: "claude-haiku-4-5"` is sent and the SSE stream read, *Then* one `RequestTrace` has `statusCode: 200`, `routedProvider: "B"`, `routeReason: "fallback"`, 2 attempts with A `upstream 529`; `/metrics` `requests_total` rose by 2 (per attempt, Story 2.1.6) while the dashboard shows 1 trace.
- Emulation loop, cache hit and local each produce one trace per client request (smoke-level repeat of Story 2.1.6 through the HTTP stack).
  - *Given* four client requests, *Then* the SSE stream carries exactly 4 `RequestTrace` events, one per client request:

    | # | Client request | `traceKind` | Notes |
    |---|----------------|-------------|-------|
    | 1 | server-tool request triggering 2 dispatches | `routed` | `iterations: 2` |
    | 2 | cacheable request, first send | `routed` | cache miss |
    | 3 | same cacheable request, second send | `cache_hit` | no attempts |
    | 4 | local-catalog model | `local` | `routedProvider: "local"` |
- Total failure traced.
  - *Given* both upstreams 500, *Then* one trace per client request with a 5xx `statusCode`, `attempts.length == 2`, and the client gets the error.
- Client abort through the real server is already proven by Task 1.1.3k (week 1-2), not re-implemented here.
  - *Given* the Story 1.1.3 test `tests/trace_client_abort_e2e.rs` (fake upstream `Script::hang()`, raw client aborts after 200 ms), *Then* once Task 2.1.2b2 has landed it also asserts attempts exactly `[<upstream>/cancelled]` with `latencyMs > 0`; Story 5.1.1 only re-runs it as part of the full gate and adds no second copy. (`hang_after_headers()` belongs to the deferred Epic 6 first-byte tests.)
- Catalog end to end.
  - *Given* the fake upstream serving `GET /v1/models`, *When* `POST /api/model-catalog/refresh` then `GET /api/model-catalog`, *Then* the upstream shows `state: "ok"` with its ids.
**Files**: `tests/trace_model_mapping_e2e.rs` (new) using the Story 1.1.5 harness `tests/support/fake_upstream.rs` and `src/entrypoint/test_support.rs`; the harness is NOT built here any more

##### Task 5.1.1a (0.5 d): Test scaffolding on the Story 1.1.5 harness
- Files: `tests/trace_model_mapping_e2e.rs`

##### Tasks 5.1.1b1-b3 (0.4 + 0.4 + 0.2 d): the four remaining scenarios
- **b1** fallback chain and total failure; **b2** the four-request table (loop, cache miss, cache hit, local); **b3** (0.2 d) catalog end to end (the client-abort scenario moved to Task 1.1.3k).
- Files: same

##### Task 5.1.1c (0.3 d): Gate
- Files: none

#### Story 5.1.2: Puppeteer smoke for the Overview page (cut-able) (0.5 day, cut-able)
**As a** maintainer, **I want** a headless smoke that the table shows non-blank cells, **so that** the original bug cannot silently return.
**Acceptance Criteria**:
- Given a running binary with a fake upstream and one request sent, *When* the script loads `/dashboard/overview`, *Then* the first row's text matches `/\b200 OK\b/`, contains no `undefined`, and the Inspect drawer opens on click showing "Requested".
**Files**: `ui/e2e/overview-smoke.mjs` (new), `ui/package.json` (script `e2e`; puppeteer already a devDependency)

##### Task 5.1.2a: Script
- Files: `ui/e2e/overview-smoke.mjs`

##### Task 5.1.2b: Run locally, document command in PR
- Files: `ui/package.json`

#### Story 5.1.3: Live exploratory run of the 14 scenarios (1.5 days, never cut)
**As an** operator-tester, **I want** every scenario executed and recorded, **so that** remaining gaps are evidenced rather than inferred.
**Acceptance Criteria**:
- All 14 scenarios have a recorded result.
  - *Given* the scenario list in `research/features.md` section 6, *When* the run finishes, *Then* `gap-report.md` has 14 rows each with command/steps, observed result, pass/fail/blocked, and a screenshot or captured output path under `project_plans/webui-trace-model-mapping/evidence/` (scenarios needing unavailable creds are marked `blocked` with the reason, never skipped silently).
- Outcome metric M6 is measured (requirements M6).
  - *Given* the fake-upstream fallback scenario, a session-pinned scenario and a cache hit, *When* the operator-tester answers "why did request X go to model Y" using only the UI, *Then* `gap-report.md` records for each walk-through the number of interactions and seconds (target: <= 2 interactions, < 60 s); a miss is a gap with severity S1.
- Value risk is checked (requirements "Value Risks").
  - *Given* a 15-minute real working session with live traffic and no prompting, *Then* the report records whether Inspect (and its attempt list) and the Models view were reached for unprompted and what was used instead; if the waterfall or Models are not used, the report recommends cutting the waterfall bars (cut #3) and Models extras first.
- Each INFERRED claim in research is re-labelled VERIFIED or refuted.
  - *Given* "PUT auth clobbers Bearer", "Session Replay payloads 404", "benchmarks seed rows", *Then* each has an observed result.
**Files**: `project_plans/webui-trace-model-mapping/gap-report.md`, `project_plans/webui-trace-model-mapping/evidence/*`

##### Task 5.1.3a: Environment
- Build with `CONSOLETTE_REQUIRE_UI=1`; fake upstream(s) from Story 1.1.5 plus a real upstream if credentials exist; 3 browser tabs (Overview, Models, Config).
- Files: none

##### Task 5.1.3b: Scenarios 1-7 (and the keyboard-only pass with a devtools-measured focus-ring contrast, UX-18; reduced-motion emulation, UX-27; forced-colors emulation; 200% zoom / 320 px reflow check of the table scroll region, Models page and drawer sheet)
- Files: `gap-report.md`, `evidence/`

##### Task 5.1.3c: Scenarios 8-14, the M6 timed walk-throughs and the usage check
- Files: `gap-report.md`, `evidence/`

#### Story 5.1.4: Gap report with dispositions (1.5 days)
**As a** maintainer, **I want** each gap fixed or backlogged with severity, **so that** nothing found is lost.
**Acceptance Criteria**:
- Every gap has severity and disposition.
  - *Given* the 20 ranked gaps in `research/features.md` section 5 plus new findings, *When* the report is finished, *Then* each row reads `fixed in <PR/commit>` or `backlog (severity: High|Med|Low, reason)`, and the counts in the summary line are derived by command (`grep -c`). Cutoff (requirements M5): High (S1) and Med (S2) rows must be `fixed` or carry an explicit re-rank reason from Story 4.1.0; only Low (S3) rows may be plain backlog.
- Known deferrals listed.
  - *Then* the backlog section includes: **Epic 6 as a whole (compression wiring + real first-byte tee, plan Stories 6.1.1-6.1.6 and ADR-005, deferred by Tyler on 2026-10-10, 9.9 days, own PR set and own validation)**, first-*token* time (first byte from deferred Story 6.1.5 will be the first stream byte, `message_start` precedes tokens), mid-stream failure status, in-progress start event, `resolvedModel` (if cut), compression for `count_tokens`/other endpoints, `session_compaction`/`system_prompt` token savings (different features), the cost-tracker `RequestId` vs trace `requestId` duality, pin/unpin UI, per-upstream model pin in Config UI, trace filter/search, deep link drawer -> Config Editor, merging local discovery into the catalog, migrating `/api/models` callers, moving `DashboardEvent`/`RequestTraceData` into a dependency-free `events` module so the `metrics -> entrypoint` edge can be cut (architecture review C1c), `Routed { model, source }` flattening if not done in Epic 2, tightening the dashboard CORS policy or adding a dashboard token (the bind/auth/CORS stance is documented, not changed), the existing original-body store's missing per-entry size cap.
- Success-metric proof.
  - *Then* the report quotes the Epic 5.1.3 screenshot showing every Live Request Traces cell as either a real value or an explicit `n/a (reason)` per the Column Truth Table (zero `undefined`/blank/false-0 cells; baseline 0 of 8 populated), and an attempt chain visible in the drawer. The report states plainly which columns are `n/a` in which situations (in this project: first byte `n/a (not streamed)` / `n/a (not measured)`, tokens-after `n/a (compression off)`, because Epic 6 is deferred); it does not count `n/a` as populated, and it reports the M6 walk-through numbers and the value-risk finding.
**Files**: `project_plans/webui-trace-model-mapping/gap-report.md`

##### Task 5.1.4a: Fill dispositions
- Files: `gap-report.md`

##### Task 5.1.4b: Fix quick wins found, one test-first commit each (time-boxed to 1 day)
- Files: per finding

##### Task 5.1.4c: Final gate and ship
- Full Rust and UI gates, `CONSOLETTE_REQUIRE_UI=1 cargo build`, then `/sdd:6-verify` and `/sdd:7-ship`.
- Files: none

---

## Phase 6 (DEFERRED FOLLOW-UP, not in this project's scope or effort): Real compression savings and real first-byte time (Epic 6)

> **Deferred follow-up (Tyler, 2026-10-10).** Everything in this phase is kept intact for the follow-up project but is **not part of this plan's scope, effort (48.4 days), schedule, cut order, validation "must pass" set or success metrics**. Nothing in Epics 1-5 depends on it: the wire fields it will fill (`compressionStatus`, `firstByteMs`, `firstByteState`, `traceRevision: 1`, `tokensAfter`) exist with constant or absent values from Story 1.1.2, every `n/a` text for them is owned by Story 1.1.4 (the three streamed first-byte states are the exception, Story 6.1.5e, which is **UI-only**), and requirements M1 is met through the defined `n/a (compression off)` / `n/a (not streamed)` / `n/a (not measured)` values. The estimates and task sizes below are the pre-deferral figures and must be re-baselined when the follow-up starts (for example Task 6.1.3c shrinks because Story 1.1.4 now owns the `skipped`/`failed` texts). The follow-up also inherits the carry-over a11y ACs in Story 4.1.7 and the `CostTrackingStream::new` facts in the Tech Debt table.

### Epic 6.1: Make `tokensBefore`/`tokensAfter`/`compressed` and `firstByteMs` measurements, not placeholders
**Goal**: Wire the existing compression engine into the request path (behind a flag, fail-open) so token savings are real, and measure real time-to-first-byte of streamed responses through the existing stream tee, for both streaming ingress formats, without altering or delaying the stream. **Effort: 9.9 days (pre-deferral estimate, not counted in this project).** Decision record: ADR-005. **Sequencing (corrected in triad-review repair iteration 1)**: the front half (6.1.1, 6.1.2a2/b/c/e, 6.1.5b) needs only Story 1.1.5; every task that edits `messages/mod.rs`, `chat_completions.rs` or `TraceScope` (6.1.2a1, 6.1.2d, 6.1.3b, 6.1.5c, 6.1.5d) waits for Story 2.1.6b because handlers adopt `TraceScope` only there, and 6.1.5c also needs Story 2.1.3. See the Dependency Visualization. Replaces the earlier "honest relabel" approach.

**Verified starting facts** (code opened at `main` 49ec827):
- `CompressionEngine::compress_request(Value) -> (Value, CompressionStats{bytes_before, bytes_after, compressed})` exists at `src/compression/engine.rs:109` and is constructed nowhere in the server; the only users are `src/bin/*`. It floor-checks at 1000 bytes (`:128`), skips if Rewind markers are present, compresses assistant text, log-like user text and `tool_result` JSON via `SmartCrusher`, injects Rewind markers, and **unconditionally calls `inject_rewind_tool`** (`:317`, `:513-526`), adding a `rewind_retrieve` tool to `tools[]`.
- The "compression" dashboard counters (`tokens_before`, `tokens_after`, `requests_compressed`, `src/metrics/counters/mod.rs:71-72,316-356`) are documented in-code as "permanently zero - nothing writes to these atomics".
- `RequestDetail::from_body` hardcodes `tokens_after: 0, compressed: false` (`src/metrics/mod.rs:99-100`); `compressed_bodies` storage and `push_compressed_body` already exist (`:193`, `:356`) and `GET /requests/{id}?stage=compressed` is already implemented but always 404s (`observability.rs:6-8`).
- Call sites: `post_v1_messages` (`messages/mod.rs:99-170`: `clamp_context_budget` -> `estimate_tokens` -> cache lookup -> `begin_cost_tracking` -> emulation branch or `dispatch`) and `post_v1_chat_completions` (`chat_completions.rs:34-57`, translates to an Anthropic body then `dispatch`). `session_compaction/*` (turn-level summarization, hooks) and `system_prompt/*` (cache alignment, verbosity) are different features that do not feed the per-request token-savings metric; they are out of scope. `tool_result_budget.rs` states its wiring is elided; it is also out of scope.
- Streaming tee: `CostTrackingStream` (`src/entrypoint/cost_tee.rs:45`) already wraps every streaming response: direct (`messages/mod.rs:228-250`), emulated-synthesized (`:478-508`), and `chat_completions.rs:73` (tee wraps the upstream Anthropic stream, then `OpenAiStreamTranslator` translates). No `criterion` dev-dependency exists.

#### Story 6.1.1: Spike: call site, rewind-tool decision, cost baseline (1.0 day)
**As a** developer, **I want** the intended call site and the risky behaviors of compression decided before wiring, **so that** Story 6.1.2 does not silently change client-visible behavior.
**Acceptance Criteria**:
- Call site is written down and verified.
  - *Given* the two ingress handlers, *When* the spike finishes, *Then* `research/spike-notes.md` states: the single insertion point (after cache lookup, before `begin_cost_tracking`/the emulation branch, so the cache key is computed from the original body and compression applies to emulated search too); that `chat_completions` has no cache lookup and compresses the translated Anthropic body at the same relative point; and that `TraceScope::begin` has already run (compression outcome is recorded with `record_compression`, Story 6.1.3).
- Rewind tool decision made, with its effects on the dispatched request captured.
  - *Given* `inject_rewind_tool` always adds `rewind_retrieve`, *When* the spike runs the engine on a real `messages` fixture and a fake upstream that answers with a `tool_use` named `rewind_retrieve`, *Then* the note records what the client sees and picks one of: (a) add `CompressionConfig.inject_rewind_tool: bool` (default false) so no tool is advertised; (b) proxy-side handling via the server-tool loop; default if the spike cannot decide is (a).
  - *Given* decision (a), *Then* the note asserts (1) with injection off the dispatched `tools` is byte-identical to the original request's `tools` (including absent); (2) the Rewind marker text does not tell the model to call a tool that is absent (if it does, the marker text is changed under the same option); (3) `should_emulate(&body)` (`messages/mod.rs:146`) is evaluated on the **original** body before compression, so a compression-injected `tools` entry can never flip server-tool emulation eligibility (Story 6.1.2 carries the matching test).
  - *Given* decision (b) instead, *Then* the note states how a tool-less request that gained `tools` is treated by `should_emulate` and by providers that reject unknown tools.
- Cost baseline measured, including the costs the budget must cover.
  - *Given* a 20 KB and a 200 KB `messages` body, *When* each of these is timed (release build, 100 runs): `body.clone()` (the engine takes `Value` by value, so fail-open needs one clone, `engine.rs:109`), `estimate_tokens` on the body (`body.to_string().len() / 4`, `messages/mod.rs:27`, a full re-serialization; it runs on both original and compressed bodies), `compress_request` on a runtime thread, and the `spawn_blocking` round-trip, *Then* p50 and p99 of each are recorded and become the basis for the budget in Story 6.1.2 (initial proposed: `max_body_bytes` 2 MB as the primary deterministic guard, `timeout_ms` = max(25, 2 x measured p99 at 200 KB) as the secondary guard).
**Files**: `project_plans/webui-trace-model-mapping/research/spike-notes.md`

Tasks: **6.1.1a** read the engine and both handlers, write the call-site note (0.25 d); **6.1.1b** rewind experiment, decision and the three tools/emulation assertions (0.4 d); **6.1.1c** timing baseline test run for clone, `estimate_tokens`, engine and `spawn_blocking` (`cargo test --release -- --ignored`) (0.35 d).

#### Story 6.1.2: Wire compression into the request path, flagged and fail-open (3.2 days)
**As an** operator, **I want** the proxy to compress eligible prompts when I enable it, **so that** token savings in the dashboard are real.

**Design (ADR-005 item 2, revised)**: `CompressionEngine::compress_request` is `async` but CPU-bound with only in-memory awaits (`rewind_store.insert`, `engine.rs:298`; the compress loop at `:190` has no `.await`), so `tokio::time::timeout` alone cannot interrupt it. `apply_compression`:
1. **Size cap first (primary, deterministic guard)**: skip when the already-computed request size (`estimate_tokens` result x 4, no extra serialization) exceeds `compression.max_body_bytes` (default 2 MB) -> original body, `skipped` (the engine never ran; `none` is reserved for "engine ran, nothing to save"), `compression_skipped_total`.
2. **Clone once, run off the runtime**: `let original = body.clone()` (cost measured in 6.1.1); the clone moves into `tokio::task::spawn_blocking` which drives the engine with `Handle::block_on(engine.compress_request(clone))`; the handler awaits `tokio::time::timeout(timeout_ms, join_handle)`. The caller keeps `body` and uses it on any failure, so the original is never lost.
3. **Map every failure to fail-open, with an honest status**: timeout, engine error value, or `JoinError` (panic: plus an `error!` with no body) -> `failed`, `compression_errors_total`; size cap or no free permit -> `skipped`, `compression_skipped_total`; engine ran and returned unchanged or below the floor -> `none`. Only `none` means "engine ran, nothing to save".
4. **A timed-out blocking task is abandoned, not cancelled**: it runs to completion on the blocking pool and its result is dropped (it may leave unused entries in the in-memory rewind store; harmless, bounded by the store's own cap). An `Arc<Semaphore>` (`compression.max_concurrent`, default 4) bounds abandoned plus active tasks, and the permit is **owned by the blocking closure**: `apply_compression` calls `try_acquire_owned()` before `spawn_blocking` and moves the `OwnedSemaphorePermit` into the closure, which drops it when it returns (normal return, engine error, or panic unwinding). The handler never holds the permit across `timeout`, so a timeout, or a client disconnect that drops the handler future and the `JoinHandle`, does not release it while the task still runs. No free permit -> skip compression (`skipped`, reason `busy`) rather than queueing.
5. **The caller keeps the original body**: `apply_compression(&EntrypointState, &Value) -> (Option<Value>, CompressionOutcome)` returns `Some(body)` only when compressed; the handler does not shadow `body`, and the response-cache key and the cache store (`messages/mod.rs:130-136` and its write path) keep using the original, pre-compression body.

**Acceptance Criteria**:
- Config flag and default.
  - *Given* no `compression` config, *Then* `compression.enabled == false` (decided default, ADR-005; Tyler may flip it later) and request bodies reach `dispatch` byte-identical to today (golden test over a fixture body); `compressionStatus == "off"` on the trace; no `spawn_blocking` call is made.
  - *Given* `compression.enabled: true`, `compress_floor_bytes: 1000` (existing default) and a body of 8 KB with a 6 KB log-like tool result, *When* the request is sent through `/v1/messages`, *Then* the body seen by the fake upstream (via the Story 1.1.5 `received_bodies()` recorder) is smaller than the original, still valid JSON, preserves `tool_use`/`tool_result` pairing, and `compressionStatus == "compressed"`.
- Both ingress formats.
  - *Given* the same logical request sent to `/v1/messages` and `/v1/chat/completions`, *Then* both result in the same compressed upstream body (chat is compressed after translation).
- Cache, emulation and tools interplay.
  - *Given* a cache hit, *Then* compression does not run (engine call counter 0, `compression_skipped_total` unchanged, trace is `cache_hit` with compression status `off`); *Given* a server-tool emulation request, *Then* compression runs once before the loop, not per iteration; *Given* a tool-less request that is eligible for emulation, *Then* eligibility is decided on the original body (compression cannot flip it).
- Fail-open with a budget (all of these are tests against a `Compressor` trait with test implementations; `CompressionEngine` is the production impl).
  - *Given* a **panicking** engine (test impl panics), *Then* the request is dispatched with the original body, `compressionStatus == "failed"`, `compression_errors_total == 1`, the runtime and other requests are unaffected, and the permit is released (a following request is not `busy`).
  - *Given* a **non-yielding slow** engine (test impl `std::thread::sleep(300 ms)`, `timeout_ms: 50`, real time, multi-thread runtime), *Then* the handler proceeds with the original body within `timeout_ms + 100 ms` (the timeout fired even though the engine never yields), `failed`, `compression_errors_total == 1`, and the abandoned task finishing later changes nothing; while the abandoned task still runs its permit stays taken (`available_permits() == max_concurrent - 1`) and returns when the closure ends.
  - *Given* a body larger than `max_body_bytes`, *Then* the engine is never invoked (call counter 0), `compressionStatus == "skipped"`, `compression_skipped_total == 1`.
  - *Given* `max_concurrent` permits all held by abandoned slow tasks, *Then* the next request skips compression immediately (`skipped`, reason `busy`) and is not delayed; and *Given* a request whose client disconnects (handler future dropped) while its compression task is running, *Then* the permit is still held until the blocking closure returns, not released at the drop.
  - *Given* the engine returns an error value, *Then* original body, `failed`, counter increments; the client sees no error in any of the above.
- The cache key and cache store use the original body.
  - *Given* `compression.enabled: true` and a cacheable non-stream `/v1/messages` request that gets compressed, *When* the response is stored and an identical request is then sent, *Then* the second request is a cache hit (engine call counter unchanged), and the cache key equals the key computed with compression off for the same body (no shadowed `body`).
- `CompressionEngine` meets the blocking-pool bound.
  - *Given* `fn assert_send_sync_static<T: Send + Sync + 'static>() {}` in a unit test, *Then* `assert_send_sync_static::<CompressionEngine>()` and `::<dyn Compressor>`-bound impls compile (the `Compressor` trait is declared `: Send + Sync + 'static`). Reading the types, `CompressionEngine` is `CompressionConfig { usize }`, `Arc<RewindStore>` (a `moka::future::Cache`), and the unit structs `TextCompressor`/`SmartCrusher` (`engine.rs:55-60`, `rewind.rs:34-36`, `smart_crusher.rs:18`, `text_compressor.rs:81`), so it should be `Send + Sync` automatically (INFERRED from field types, not compiled); the assertion turns that into a gate. The future driven by `block_on` need not be `Send`.
- No change to the bytes when not compressed.
  - *Given* `none` (below floor or Rewind markers present), *Then* the dispatched body equals the original (`==` on `serde_json::Value`).
- Hot-path cost.
  - *Given* the 20 KB and 200 KB fixtures, *When* the `#[ignore]`d **release-only** timing test runs (clone + `estimate_tokens` x2 + `spawn_blocking` round-trip + engine), *Then* the end-to-end added latency meets the Story 6.1.1 budget; numbers are recorded in the PR description. There is no debug-mode ratio test (timing-based, flaky); the always-on gates are the golden, byte-identity and fail-open tests above.
**Files**: new `src/entrypoint/compress_step.rs` (`apply_compression(&EntrypointState, &Value) -> (Option<Value>, CompressionOutcome)`, `Compressor` trait (`Send + Sync + 'static`)), `src/entrypoint/mod.rs` (state fields `compression: Arc<dyn Compressor>`, semaphore; route/state lines only), `src/entrypoint/messages/mod.rs`, `src/entrypoint/chat_completions.rs`, `src/config/*` (`enabled`, `timeout_ms`, `max_body_bytes`, `max_concurrent`), `src/compression/engine.rs` (option from 6.1.1), `src/metrics/counters/mod.rs`

Tasks (every task <= 0.5 d): **6.1.2a1** (0.45 d, **after 2.1.6b**) failing handler-level tests: golden unchanged-when-off, compressed-when-on, chat parity, cache/emulation/tools interplay, cache key from the original body; **6.1.2a2** (0.45 d, front half) failing `compress_step` unit tests: panic, non-yielding slow with permit held, size cap, busy, disconnect with permit held; **6.1.2b** config schema + engine construction in `EntrypointState` + the `Send + Sync + 'static` compile-time assertion (0.45 d, front half); **6.1.2c1** `compress_step.rs`: size cap, clone, `try_acquire_owned` before `spawn_blocking` with the permit moved into the closure (0.4 d, front half); **6.1.2c2** `block_on`, `timeout`, `JoinError` mapping, `failed`/`skipped`/`none` statuses (0.4 d, front half); **6.1.2c3** counters `compression_errors_total` / `compression_skipped_total` (0.25 d, front half); **6.1.2d** insert the single call in both handlers without shadowing `body` (emulation decision, cache key and cache store use the original; dispatch uses `Some(compressed)` else the original), pass post-compression `est_tokens` to `dispatch`, record the outcome with `scope.record_compression`; release-only budget test (0.5 d, **after 2.1.6b**); **6.1.2e** engine `inject_rewind_tool` option per 6.1.1 (0.3 d, front half). Task sum: 0.45 + 0.45 + 0.45 + 0.4 + 0.4 + 0.25 + 0.5 + 0.3 = 3.2 d.

#### Story 6.1.3: `tokens_after` accuracy and honest display (1.1 days)
**As an** operator, **I want** tokens-before/after to be computed the same way and shown with the right "n/a" text, **so that** savings are trustworthy.
**Acceptance Criteria**:
- Same estimator both sides.
  - *Given* a body, *Then* `tokens_before` and `tokens_after` are both `estimate_tokens(body)` (chars/4 of the JSON, `messages/mod.rs:27`) applied to the original (recorded by `TraceScope::begin`) and the post-compression body (recorded by `record_compression`) respectively, so the difference is not an artifact of two estimators; the UI labels both "est.".
- Ordering is honest: the outcome arrives after the scope exists.
  - *Given* `TraceScope::begin(.., tokens_before)`, *Then* the new scope has `compression_status: off`, `tokens_after: None`; *When* the handler calls `scope.record_compression(CompressionOutcome { status: Compressed, tokens_after: Some(1240) })` after `apply_compression`, *Then* the ring-buffer entry written by `finish` has real `tokens_after`/`compressed`, replacing the `from_body` constants; *Given* a cache hit, *Then* `record_compression` is never called and the trace stays `off`/`n/a (served from cache)`.
- Values per state.
  - *Given* `off`, *Then* `tokensAfter: null` and the cell reads `n/a (compression off)`; *Given* `skipped` or `failed`, *Then* `tokensAfter: null` and the cell reads `n/a (compression skipped)` / `n/a (compression failed)` (the original body was sent, but the run did not complete, so no "no savings" claim); *Given* `none`, *Then* `tokensAfter == tokensBefore` and no savings text; *Given* `compressed`, *Then* `tokensAfter < tokensBefore` and `2,000 -> 1,240 (-38%)` style text; cache hit -> `n/a (served from cache)`.
- Counters become real.
  - *Given* 3 compressed requests of 2,000 -> 1,200 est. tokens, *Then* `/metrics` `compression.total_tokens_saved == 2,400`, `total_requests_compressed == 3`, the stale comment in `counters/mod.rs:331-337` is corrected, and the MetricsTick `totalTokensSaved` equals it.
- `clamp_context_budget` ordering is documented.
  - *Given* `clamp_context_budget(&mut body)` runs before `begin`, *Then* `tokens_before` and the stored `stage=original` body are post-clamp, and a code comment in `begin` says so.
**Files**: `src/metrics/mod.rs`, `src/metrics/trace.rs` (`record_compression`), `src/metrics/counters/mod.rs`, `src/entrypoint/events.rs`, `ui/src/app/core/models/na-reason.ts`, `recent-trace-table.component.ts`, specs, fixtures `request-trace-compressed.json`

Tasks: **6.1.3a** failing tests (0.3 d); **6.1.3b** `TraceScope::record_compression`, counters, tick (0.4 d, after 2.1.6b because the handlers call it); **6.1.3c** (ownership note: Story 1.1.4 already implements the `skipped`/`failed` `naReason` texts, so this task is now fixtures `request-trace-compressed.json` and a wiring spec only; kept at 0.4 d until re-baselined) fixtures + spec (0.4 d). Task sum: 0.3 + 0.4 + 0.4 = 1.1 d.

#### Story 6.1.4: Store the compressed body for replay, with a retention and size policy (0.6 day)
**As an** operator, **I want** `stage=compressed` to return the body that was actually sent upstream, **so that** Session Replay can compare.
**Acceptance Criteria**:
- *Given* a compressed request, *When* `GET /requests/{id}?stage=compressed`, *Then* 200 with the post-compression body; *Given* `off` or `none`, *Then* 404 (as today) and the UI message from Story 4.1.1.
- The stale "known gap" doc comments at `observability.rs:6-8` and `metrics/mod.rs:188-190` are updated.
- Retention and size policy (NFR; the stored body is prompt content).
  - *Given* a request with status `compressed`, *Then* the body is stored only when its serialized size is at most `compression.max_body_bytes` (always true for a body the engine accepted, since larger inputs are `skipped`; asserted anyway); `none`, `skipped`, `failed` and `off` store nothing.
  - *Given* 101 compressed requests, *Then* `get_compressed_body` holds the latest 100 (the existing cap at `push_compressed_body`, `metrics/mod.rs:357-366`), the oldest is gone, and the body is never present in any SSE frame, `/metrics` JSON or log line (sentinel test on a prompt string); the policy is documented at the function: in memory only, 100 entries, worst case about 100 x `max_body_bytes` (200 MB at the 2 MB default; a deployment that cares lowers `max_body_bytes`).
**Files**: `src/entrypoint/compress_step.rs` or `TraceScope` (calls `push_compressed_body`), `src/entrypoint/observability.rs` (comments), tests

Tasks: **6.1.4a** failing test (0.15 d); **6.1.4b** push compressed body only when `compressed` (0.2 d); **6.1.4c** comments + UI label (0.15 d); **6.1.4d** retention/size policy tests and the doc comment (0.1 d).

#### Story 6.1.5: Real first-byte time through the stream tee, both SSE ingress formats (2.75 days)
**As an** operator, **I want** a real time-to-first-byte for streamed requests, **so that** the dashboard distinguishes a slow upstream from a slow stream.

**Design (ADR-005 item 5, revised)**:
- **One time origin**: `firstByteMs` and `timeToHeadersMs` are both measured from the send of the final attempt (the scope records `final_attempt_started_at: Instant` in `dispatch_attempts`), so `firstByteMs >= timeToHeadersMs`; `totalMs` (from `begin`) and the drawer carry the cost of earlier attempts and holds.
- **The slot exists before the tee and is filled at `finish`**: the inner handler builds the tee before the wrapper's `finish` runs (`direct_stream_response`, `messages/mod.rs:228-250`; `chat_completions.rs:73`), so the slot cannot be created at `finish`. Instead the handler calls `scope.first_byte_hook()` when it builds `CostTrackingStream`; it returns an `Arc<FirstByteSlot { start: Instant (the scope's `final_attempt_started_at`), state: AtomicU8, detail: std::sync::Mutex<Option<RequestDetail>>, metrics: Arc<MetricsCollector> }>` and the scope keeps its own `Arc` clone. `finish` for `stream: true` and a success outcome moves a clone of the final `RequestDetail` (small: attempts and scalars; bodies live in separate stores) into `detail`, and emits the revision-0 trace with `firstByteState: pending`. When the hook fires it takes the snapshot, sets `first_byte_ms` / `first_byte_state` and `trace_revision = 1`, replaces the ring-buffer entry if still present (`debug!` if evicted) and **always emits** via `from_detail`. One emit path (consistent with ADR-002 item 4).
- **Fires with no snapshot** (`detail` is still `None`, so `finish` has not run): this happens only when the tee is dropped before `finish`, for example the handler future is cancelled between tee creation and `finish` (the scope then emits the 499 trace and the tee drops with an empty slot) or the inner handler returns an error after building the tee. Rule: the CAS still resolves the state (so it fires once), but nothing is written or emitted, `first_byte_no_snapshot_total` increments and a `debug!` is logged; the scope's cancelled `Drop` never fills the slot. If `finish` later runs on a slot that is already resolved (error-return case), it reads the state and emits the revision-0 trace with that terminal `firstByteState` directly (`ended_empty`/`client_closed`) instead of `pending`, so the row cannot stay `pending`.
- **Exactly one outcome per stream**: `state` goes `Pending -> Measured` (first non-empty `Ok` chunk) or `Pending -> EndedEmpty` (the inner upstream stream returned `None` or an `Err` before any byte) or `Pending -> ClientClosed` (the tee was dropped before the inner stream terminated: a client abort) by compare-and-swap; the loser is a no-op. The tee gets a new `Drop` impl (none exists today: `grep 'impl Drop' src/entrypoint/cost_tee.rs` finds nothing) and a `terminated: bool` set when it sees `None`/`Err`; `Drop` with `terminated == false` and `Pending` resolves `ClientClosed`. A streamed row therefore never stays `pending`, and an upstream-ended stream is told apart from a client abort.
- **Synthesized error frame is never a first byte**: the hook fires on the upstream item inside `poll_next` before the tee substitutes `MID_STREAM_ERROR_FRAME` (`cost_tee.rs:38,:208`); an upstream `Err` as the first item resolves `EndedEmpty`, not `Measured`, even though the client receives the synthesized frame.
- `CostTrackingStream` gets the hook through a `with_first_byte_hook(Option<FirstByteHook>)` builder (the existing `CostTrackingStream::new` call sites stay untouched: 7 in `src` = 3 production at `chat_completions.rs:73`, `messages/mod.rs:235`, `:491` plus 4 in `cost_tee.rs` tests, VERIFIED by grep at `main` 49ec827; plus 1 in `tests/server_tool_emulation.rs:500`).

**Acceptance Criteria**:
- First byte measured for the Anthropic-format stream.
  - *Given* the Story 1.1.5 fake upstream configured with `stream_chunks(["a","b"], delay = 200 ms)` and `delay_headers(150 ms)`, *When* `POST /v1/messages` with `stream: true` is sent, *Then* the trace is first emitted at headers (`traceRevision: 0`, `firstByteMs: null`, `firstByteState: "pending"`) and re-emitted once (`traceRevision: 1`, `firstByteState: "measured"`) with `timeToHeadersMs >= 150`, `firstByteMs >= timeToHeadersMs`, and `firstByteMs` after the test client's first received chunk minus a 100 ms tolerance and before 5 s (real sockets; ordering assertions, not +/- 50 ms); `requestId` unchanged so the UI upserts.
- One origin, even after a fallback.
  - *Given* a first upstream failing after 400 ms and a second upstream with `delay_headers(150 ms)` streaming, *Then* the row shows `timeToHeadersMs` ~150 and `firstByteMs` >= that, while `totalMs` includes the 400 ms; the drawer lists both (a spec on a fallback fixture asserts first byte is never rendered smaller than Latency).
- First byte measured for the OpenAI-format stream.
  - *Given* the same upstream and `POST /v1/chat/completions` with `stream: true`, *Then* the same behavior (tee wraps the upstream stream before `OpenAiStreamTranslator`, `chat_completions.rs:73-86`), documented as "first byte of the upstream stream", not of the first translated chunk.
- Never stuck pending.
  - *Given* `Script::hang_after_headers()` and a client that disconnects (or an upstream stream that ends/errors) before any body byte, *Then* a client disconnect yields exactly one re-emit with `firstByteState: "client_closed"`, `firstByteMs: null`, the UI reads `n/a (client closed before first byte)`, and `first_byte_client_closed_total == 1`; an upstream stream that ends or errors before any byte yields `"ended_empty"` / `n/a (stream ended before first byte)` and `first_byte_ended_empty_total == 1`; no second re-emit occurs afterwards (CAS lost), and `trace_cancelled_total` is unchanged (the request scope finished at headers).
- Slot exists before the tee; fires with no snapshot.
  - *Given* a handler that builds the tee (`scope.first_byte_hook()`) and is then cancelled before `finish`, *Then* exactly one event is emitted (the 499 `cancelled` trace), no revision-1 re-emit, `first_byte_no_snapshot_total == 1`; *Given* the tee dropped unpolled and `finish` running afterwards, *Then* the revision-0 trace already carries `firstByteState: "client_closed"` rather than `pending`.
  - *Given* an upstream whose first item is an `Err`, *Then* the client sees `MID_STREAM_ERROR_FRAME` but the slot resolves `ended_empty`, never `measured`.
- Definition and exclusions.
  - *Given* the emulated-search synthesized stream (`messages/mod.rs:478`), *Then* no first-byte hook is attached and the cell reads `n/a (synthesized stream)`; non-stream, cache-hit and failed requests read per the Column Truth Table.
- Measured once.
  - *Given* a stream with many chunks, *Then* the hook body runs exactly once (set-once CAS), producing one extra ring-buffer replace and one extra SSE event per streamed request (the documented exception to "one replace per request" in Story 2.1.3).
- Evicted request has a deterministic outcome.
  - *Given* 100+ other requests complete during a slow stream so the ring-buffer entry is evicted, *Then* the re-emit still arrives with the full detail (from the slot snapshot), the ring buffer is not repopulated, and `trace_finish_evicted_total` increments.
- UI ordering.
  - *Given* a `traceRevision: 1` event followed by a delayed duplicate `traceRevision: 0` event, *Then* the row keeps the revision-1 values (Story 1.1.2 rule).
**Files**: `src/entrypoint/cost_tee.rs` (hook, builder, `Drop`), `src/entrypoint/messages/mod.rs`, `src/entrypoint/chat_completions.rs`, `src/metrics/trace.rs` (`first_byte_hook()` slot construction at tee-build time, snapshot fill at `finish`), `src/metrics/mod.rs`, `src/entrypoint/events.rs`, UI `na-reason.ts` + spec

Tasks (every task <= 0.5 d): **6.1.5a1** (0.4 d, after 2.1.6b) failing integration tests with the streaming fake upstream: Anthropic format, OpenAI format, fallback origin; **6.1.5a2** (0.4 d, after 2.1.6b) failing integration tests: ended-empty, client-closed, no-snapshot, eviction; **6.1.5b1** (0.45 d, front half) `FirstByteSlot` + `FirstByteHook` (CAS state, snapshot take, no-snapshot rule) with unit tests in `cost_tee.rs`; **6.1.5b2** (0.4 d, front half) `with_first_byte_hook` builder, fire on the first `Poll::Ready(Some(Ok(bytes)))` with non-empty bytes, `EndedEmpty` on `None`/`Err`, new tee `Drop` with the `terminated` flag for `ClientClosed`; **6.1.5c** (0.5 d, after 2.1.3 and 2.1.6b) `scope.first_byte_hook()` (slot built at tee-build time from `final_attempt_started_at`), `finish` filling or reading the slot, the single `from_detail` re-emit path, `trace_revision`/`first_byte_state` wiring; **6.1.5d** (0.4 d, after 2.1.6b) pass the hook from both handlers and not from the synthesized stream; **6.1.5e** (0.2 d, **UI-only**: touches `na-reason.ts` and Karma specs, no Rust) UI `pending` / `ended_empty` / `client_closed` texts, the carry-over a11y checks from Story 4.1.7, revision rule spec, fallback-fixture spec. Task sum: 0.4 + 0.4 + 0.45 + 0.4 + 0.5 + 0.4 + 0.2 = 2.75 d.

#### Story 6.1.6: The tee never alters or delays the stream; cost and failure behavior (1.25 days)
**As an** operator, **I want** proof that measuring first byte cannot corrupt or slow a stream, **so that** the proxy stays a transparent pipe.
**Acceptance Criteria**:
- Bytes unchanged.
  - *Given* a scripted upstream stream of 1,000 chunks including split UTF-8 and SSE frames spanning chunk boundaries, *When* read through the tee with and without the hook, *Then* the concatenated bytes and chunk boundaries are identical, order preserved.
- Errors and termination pass through, asserted at the tee.
  - *Given* an upstream stream that errors mid-way, *Then* the existing synthesized in-band error frame behavior (ADR-018) is unchanged and the slot state is `Measured` or `EndedEmpty` exactly once, and the synthesized `MID_STREAM_ERROR_FRAME` is not counted as a first byte.
  - *Given* the consumer drops the response mid-stream after the first byte, *Then* no panic, no leaked task, the slot stays `Measured` (no second re-emit, `first_byte_ended_empty_total` unchanged); *Given* the consumer drops before any byte, *Then* the slot becomes `ClientClosed` exactly once (`EndedEmpty` is reserved for an upstream stream that ended or errored); *Given* the tee is dropped before `finish` ran, *Then* the slot fires with no snapshot (no emit, `first_byte_no_snapshot_total == 1`). (The earlier "scope emits exactly one terminal event" assertion is dropped: the scope is already finished at headers, so it verified nothing.)
- Hook failure is harmless.
  - *Given* a poisoned ring-buffer mutex or a closed `event_tx`, *Then* the stream continues unaffected (hook swallows the failure via `PoisonError::into_inner`/ignored send result, logs at `debug`).
- Hot-path cost.
  - *Given* a 10,000-chunk stream, *When* timed with and without the hook in an `#[ignore]`d **release-only** test, *Then* with-hook / without-hook <= 1.10 and the per-chunk overhead after the first chunk is a single `AtomicU8` load; the first-chunk path does no allocation beyond the existing path and the hook never `await`s or blocks (a test with a hanging `event_tx` receiver proves no back-pressure because `broadcast::send` is non-blocking). There is no debug-mode **ratio** test (flaky).
  - *Given* the same 10,000-chunk stream in a debug build, *When* it is read through the tee with the hook attached (always-on, non-`#[ignore]`), *Then* the read completes within 5 s absolute (expected well under 0.2 s; about 25x headroom, so only a per-chunk lock, allocation storm or blocking call trips it) and the hook body ran exactly once; the byte-identity, set-once and hook-failure tests plus this coarse bound are the always-on gates.
**Files**: `src/entrypoint/cost_tee.rs` tests, `tests/` streaming test

Tasks (every task <= 0.5 d; need 6.1.5): **6.1.6a** (0.5 d) byte-identity, error/termination and drop-behavior tests at the tee; **6.1.6b** (0.2 d) hook-failure and no-back-pressure tests; **6.1.6c** (0.4 d) release-only `#[ignore]` ratio test; **6.1.6d** (0.15 d) always-on coarse absolute-bound test. Task sum: 0.5 + 0.2 + 0.4 + 0.15 = 1.25 d.
