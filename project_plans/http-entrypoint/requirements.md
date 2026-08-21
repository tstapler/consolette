# Requirements: http-entrypoint

**Date**: 2026-08-19
**Type**: feature addition
**Complexity**: 3 — system design

## Problem Statement
consolette has a fully-wired config engine (ADR-001), routing strategy and dispatch loop (ADR-003), plugin discovery and exec credential-helper auth (ADR-007), and concrete `Provider` implementations for Anthropic and Bedrock — but nothing binds a port and turns incoming HTTP requests into calls through that stack. The `Run` command is currently a stub. Anyone pointing an Anthropic-API client (or an OpenAI-compatible client) at consolette today gets nothing: there is no live seam. This is confirmed directly by a TODO in `src/providers/mod.rs`'s `translate_and_record`, which states cost-recording on the failure path "belongs at the nearest `Provider::send`/`Router::dispatch` error-handling seam once one exists... no such seam is wired into a running server yet."

## Baseline
Today, `consolette run` (or equivalent) does not serve traffic. There is no working proxy. Anyone wanting proxying, failover, or the plugin/exec-auth features already built has no way to actually use them — the only path to exercising this code is through unit tests.

## Users / Consumers
- Local CLI tools configured to talk to an Anthropic-compatible or OpenAI-compatible HTTP API (e.g. Claude Code pointed at a local base URL), running on the same machine as consolette.
- The consolette maintainer, validating that the router/auth/config stack works end-to-end.

## Success Metrics
- A client sends a real `/v1/messages` (Anthropic-native) request to the bound port and receives a valid response (streamed or full) routed through an actual configured upstream — not a stub.
- The same is true for an OpenAI-compatible chat-completions-shaped request, translated internally via the existing `translate_openai_to_anthropic`/`translate_anthropic_to_openai` functions.
- A `CostTracker` record exists for a completed request (success path) via `record_actual_usage_from_anthropic_response`, closing the gap the `translate_and_record` TODO calls out.
- `cargo test` passes including new integration-style tests that exercise the bound server, not just unit tests of the underlying pieces.

## Appetite
Large (3–6 weeks)
*(Scope must fit the appetite. If it doesn't fit, cut scope — do not move the deadline.)*

## Constraints
- Must bind loopback-only (127.0.0.1), matching the existing NFR-6 convention from the whole-project requirements ("localhost-bind only"). No TLS, no remote-bind configurability in this pass.
- Must dispatch through the existing `Router::dispatch` (ADR-003) rather than talking to `Provider`s directly — this is what makes failover, admission control, and cost tracking apply uniformly.
- Must resolve `Config.port` (already a pre-existing top-level config field, default 47000) as the bind port — no new config surface for this.
- `axum` 0.8 and `axum-extra` 0.10 are already declared as dependencies in `Cargo.toml` — no new HTTP framework dependency is expected.

## Non-functional Requirements
- **Performance SLO**: preserve existing whole-project characteristics — <100ms startup, <50MB idle (carried over from NFR-3 in `project_plans/consolette/requirements.md`).
- **Scalability**: single local proxy instance, one user's request volume — not applicable at scale.
- **Security classification**: internal/local-only. Loopback bind only (see Constraints); secrets remain resolved via existing env/keychain/exec-helper mechanisms, never logged (carried over from NFR-6).
- **Data residency**: not applicable — local process only.

## Scope
### In Scope
- Replace the `Run` command stub with a real HTTP server bound to `Config.port` on loopback.
- An Anthropic-native `/v1/messages` (and related, e.g. `/v1/messages` streaming variant) endpoint.
- An OpenAI-compatible endpoint, translating requests/responses via the already-implemented `translate_openai_to_anthropic`/`translate_anthropic_to_openai`/`translate_and_record` functions in `src/providers/mod.rs`.
- Dispatching both endpoint families through the single default `Route`/`Router::dispatch` (ADR-003) — no per-request route selection logic beyond that.
- Exec/plugin auth (ADR-007) applying transparently, since it's already composed into the upstream/provider resolution path `Router::dispatch` uses.
- Streaming responses back to the client (SSE for Anthropic-native, and the OpenAI-compatible streaming shape) — including the ADR-003 streaming-failover discipline (failover only possible pre-first-byte; mid-stream failures surface as an in-band error event, not transparent retry).
- Wiring `CostTracker` recording (`record_actual_usage_from_anthropic_response` on success; `record_request_failed` on the failure path) at the new dispatch seam, per the outstanding TODO.
- Direct ship — no feature flag. Rollback is `git revert` / reinstalling the prior binary version if something regresses.

### Out of Scope
- Multi-route selection (by model, header, or any other dimension) — this pass wires exactly one default `Route`. Selecting among multiple configured routes is a future pass.
- Remote/non-loopback bind, TLS/HTTPS termination.
- Any new config surface beyond the existing `Config.port`.
- Admin/management endpoints (health checks, metrics-scrape endpoints) beyond what already exists for `serve-cost`.

## Rabbit Holes
- **Streaming failover boundary** (ADR-003): once the first byte of an SSE/stream response has been flushed to the client, cross-upstream failover is no longer possible — only an in-band `error` event. Getting the exact point-of-no-return right (buffering vs. flushing) is easy to get subtly wrong and worth explicit test coverage.
- **Dual request-format translation correctness**: the existing `translate_openai_to_anthropic`/`translate_anthropic_to_openai` functions are unit-tested in isolation but have never been exercised through a live end-to-end request; edge cases (tool calls, multi-block content, stop reasons) may surface only once wired to real Router dispatch.
- **Cost-tracking on partial/failed streams**: recording usage correctly when a stream is cut short mid-response (vs. a clean success or a pre-first-byte failure) has three distinct code paths and is easy to under-cover.

## Alternatives Considered
- Building only the Anthropic-native endpoint and deferring OpenAI-compat to a later pass — rejected per the user's explicit choice to do both from day one, since the translation functions already exist and are otherwise dead code.
- Feature-flagging the new server behind a flag with staged rollout — rejected: this is a single-user local proxy tool, not a shared service, so the operational risk a flag mitigates doesn't apply here.

## Feasibility Risks
- The concrete `Provider` implementations (`AnthropicProvider`, `BedrockProvider`) have not yet been exercised via a live dispatch loop — any latent bugs in `send`/`send_streaming_request` will surface for the first time under this work.
- `HealthRegistry`/`Availability`/`RoutingStrategy` (ADR-003) were confirmed to exist by import but not read in file-content detail — their exact interaction with a real async HTTP handler (DashMap-guard-across-await hazards called out in ADR-003) needs care during implementation.

## Observability Requirements
Standard request logging (method, path, upstream selected, status, latency) via the existing `tracing` instrumentation patterns already used elsewhere in the codebase (e.g. `src/auth/exec.rs`'s `tracing::warn!`). No new alerting/oncall infrastructure — this is a local single-user tool. Cost-metrics recording (in scope, see above) doubles as the primary observability signal for request volume and outcome.

## Risk Control
Ship directly, no feature flag — single-user local proxy tool. Rollback procedure: `git revert` the merge commit, or reinstall the prior released binary version, if the new entrypoint regresses existing behavior.

## Open Questions
- Exact shape of the OpenAI-compatible endpoint path (`/v1/chat/completions`?) and how much of the OpenAI schema surface (tool calls, `n>1`, logprobs) needs support beyond what `translate_openai_to_anthropic` already handles — deferred to Phase 2 research.
- Whether `HealthRegistry`/`Availability`/`strategy.rs` internals hold any surprises for async-handler integration — deferred to Phase 2 research (full read of `src/routing/health.rs` and `strategy.rs`).
