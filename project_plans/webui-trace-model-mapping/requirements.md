# Requirements: webui-trace-model-mapping

**Date**: 2026-10-09
**Type**: feature addition
**Complexity**: 3 — system design

## Problem Statement
The Angular dashboard (`/dashboard/overview`) can't answer the operator's core question: "what did the client ask for, where did Consolette send it, and why?" Live Request Traces show blank request ID (`unknown`), duration, TTFT, tokens and status for every row; only the routed model is shown, never the client-requested one; and there is no view of which models each upstream currently advertises. Beyond these, the UI likely has other unimplemented or unwired features that have not been systematically found.

## Baseline
- `RequestTraceData` (`src/entrypoint/events.rs:27`) has no `#[serde(rename_all)]` — INFERRED to emit snake_case (`request_id`, `duration_ms`, `first_byte_ms`, `tokens_before`, `status_code`), while the UI model (`ui/src/app/core/models/telemetry.models.ts`) and `recent-trace-table.component.ts` read camelCase (`requestId`, `durationMs`, …) and expect a `compressionStatus` string the backend doesn't send (it sends `compressed: bool`). Root cause not yet confirmed by running it.
- The trace carries one `model`; `RequestDetail` (`src/metrics/mod.rs`) also holds `selected_model` / `selected_model_was_exploration`, but neither is on the trace event. `status_code` is hardcoded to 200 at the emit site, so failed/retried requests are indistinguishable.
- `GET /api/models` (`src/entrypoint/api.rs:54`) exists but the UI doesn't surface it.
- Operators today inspect `/metrics`, logs, or the legacy dashboard to reconstruct routing decisions.

## Users / Consumers
**Primary user: Tyler as the operator of a single-operator local proxy** (loopback-only, no multi-user; see the exposure stance below). He opens `/dashboard/overview` while debugging a live client session and needs to know what a client asked for, where Consolette sent it and why. Design trade-offs (a11y depth, no auth, no multi-user concerns) are judged against that one user, not a hypothetical team. Other consumers: the Angular SPA pages (Overview, Session Replay, Model Benchmarks, Config Editor) and any MCP or script reading `/v1/dashboard/events` SSE (none found yet; Task 1.1.1g confirms).

## Success Metrics
- **M1** 100% of rows in Live Request Traces show, for every column, either a real measured value or an explicit "n/a (reason)" defined in the plan's Column Truth Table (request ID, latency to headers, first byte for streamed requests, tokens before/after, status incl. 499 for cancelled). Zero `undefined`/blank/false-0 cells (baseline: 0 of 8 populated). **Decision (Tyler, 2026-10-10)**: compression wiring and the real first-byte tee (plan Epic 6) are a deferred follow-up project, so in this project the first-byte column reads `n/a (not streamed)` for non-streamed rows and `n/a (not measured)` for streamed rows, and tokens-after reads `n/a (compression off)`; real/explained is met by these defined `n/a` values (the earlier Decision C of 2026-10-09 is superseded). `n/a` is a defined, explained value, not a hidden blank, but it never counts as "populated": the gap report states which columns are `n/a` in which situations.
- **M2** One trace per client request: server-tool emulation loops, response-cache hits and local-model requests each produce exactly one trace; requests abandoned by the client still produce a terminal trace.
- **M3** For any trace, an operator can see requested model → routed model/provider, the routing reason (strategy/rule/session pin/fallback/exploration), and every fallback/retry attempt with status and latency, without leaving the UI.
- **M4** UI lists the models each configured upstream reports as available, with last-refresh time and error state per upstream.
- **M5** Exploratory-testing report enumerates gaps found across all four pages; each is either fixed or logged as a backlog item with severity. Severity cutoff: S1 (data wrong or misleading, or blocks the operator's core question) and S2 (feature dead end or error with no recovery) are fixed or consciously re-ranked in Story 4.1.0; S3 (cosmetic, polish) goes to the backlog. The gap report's High / Med / Low labels map to S1 / S2 / S3.
- **M6 (outcome)** An operator can answer "why did request X go to model Y?" from the UI in at most 2 interactions (open the Overview row's Inspect, read the Route and Attempts blocks) and under 60 seconds. Baseline: no UI path exists; the operator reconstructs the answer from `/metrics`, logs or the legacy dashboard (see Baseline). The pre-change time was not measured (UNVERIFIED, assumed minutes). Story 5.1.3 times three post-change walk-throughs (fallback, pinned, cache hit) and records interaction count and seconds in the gap report. **Target rationale**: the 2-interaction / 60-second bound is chosen, not derived (UNVERIFIED): the question comes up mid-debugging, and a lookup that takes longer than a minute loses to reading `/metrics` or logs, which is the status quo. If the measured walk-throughs miss but the operator still prefers the UI, the gap report records that and re-ranks the target rather than counting it a failure of the feature.
- **Early validation (Epic 1 boundary)**: the riskiest assumption is that Tyler will actually use the Overview table and Inspect. Before Epic 2 starts, he uses the fixed table plus the existing Inspect link on real traffic for one working day and records in `gap-report.md` whether he reached for it instead of `/metrics` or logs (Task 1.1.4i). A "no" re-scopes Epic 2 before 11.5 days are spent on it.

## Appetite
Large (~9 weeks = 45 engineer-days, extended from 3-6 by Tyler on 2026-10-09). **Decision (Tyler, 2026-10-10): commit to Epics 1-5 with no cuts; Epic 6 (compression wiring and real first-byte tee) is deferred to a follow-up project.** The plan totals 48.4 engineer-days (~9.7 weeks), 3.4 days (~0.7 weeks) over the approved 45 days. Of that overshoot, 1.65 days is the Epics 1-5 baseline (46.65) and 1.75 days comes from the final-pass UX, engineering and product additions (real-socket abort e2e pulled forward, a11y/reflow/drawer ACs, Epic 1 validation). `implementation/plan.md` "Effort and Appetite" is the single source of truth and keeps a cut order as a contingency, not applied by default. Earlier figures in `consistency.md` and `pre-mortem.md` are superseded.

**Roadmap fit (RICE-ish, INFERRED, single-operator so only the ratio to alternatives matters)**: Reach 1 operator (Tyler), daily during proxy debugging; Impact 3 of 3 (the core question is unanswerable in the UI today); Confidence 0.7 (value risk below); Effort 9.7 person-weeks, so (1 x 3 x 0.7) / 9.7 = about 0.22. No competing roadmap items are scored here, so the number is a placeholder for comparison, not a ranking.

## Constraints
- Single crate/binary, Axum 0.8, embedded Angular SPA (no CDN, air-gapped safe).
- No `unwrap`/`expect` in non-test production code; clippy/fmt clean.
- SSE event contract changes must not break existing consumers without a compatibility plan.
- UI must be built via the existing `build.rs`/Make path (Bazel is spike-only).

## Non-functional Requirements
- **Performance SLO**: SSE push < 100ms; added trace fields must not measurably slow the request hot path. Enforced by an always-on coarse regression test (Task 2.1.3h: generous absolute bound, runs on every `cargo test`) plus a release-only timing test (Task 2.1.3f). The tee and compression timing tests belong to the deferred Epic 6.
- **Scalability**: bounded trace/attempt storage (existing 100-entry ring buffer or explicit cap).
- **Security classification**: internal; traces must not leak API keys or prompt content; upstream model-list calls use existing provider auth.
- **Dashboard exposure stance (unchanged by this project, now documented)**: the server binds loopback only (`127.0.0.1:{port}`, `src/entrypoint/mod.rs:385`, pinned by a test at `:660`); the dashboard and `/v1/dashboard/events` have no authentication (multi-user auth is out of scope); CORS allows any origin (`src/entrypoint/mod.rs:260-263`). So any local process, and any web page open in the user's browser, can read the SSE stream and `GET /api/*`, and can send `POST /api/model-catalog/refresh`. Mitigations in scope: traces and catalog responses carry no prompts, keys or upstream error bodies (sentinel tests); the refresh endpoint is bounded by single-flight, a 10 s manual throttle and a retry floor; the Models view help text states the exposure. Not in scope and not changed: the existing `GET /requests/{id}` already returns stored prompt bodies under the same stance; tightening CORS or adding a token is logged as a backlog item (Story 5.1.4), not applied here. **Accepted risk, recorded as an explicit decision for Tyler** (plan "Risk Control", Decision R1): a web page open in the same browser can trigger `POST /api/model-catalog/refresh`, which can run an upstream's credential helper (SSO prompt) or a paid call; the mitigations above bound the rate, not the trigger.
- **Stored compressed prompt (deferred Epic 6, not part of this project)**: kept in memory only, never in SSE or on disk, 100 entries in lockstep with the ring buffer, stored only when the engine actually compressed and the body is at most `compression.max_body_bytes` (default 2 MB, so worst case about 200 MB at the default; the existing original-body store has the same 100-entry cap and no size cap). See Story 6.1.4.
- **Data residency**: no special requirements.

## Scope
### In Scope
- Fix trace field contract between backend and UI (root-cause first); real status codes; one trace per client request incl. cache hits, local dispatch, emulation loops and cancellations.
- Requested model, routed model/provider, routing reason, fallback/retry attempt chain on trace events and in Inspect view.
- Upstream available-model listing (per provider) in the UI, with refresh and error state; refresh is on demand by default (credential helpers and paid calls must not run on a hidden timer).
- Exploratory testing of the running app (all four pages, live traffic via proxy) producing a gap report; fix highest-value gaps within appetite.

### Deferred follow-up (not in this project's scope or effort)
- Wire the compression engine into the request path (flagged, fail-open) so token savings are real, and measure real first-byte time of streamed responses through the stream tee (both SSE ingress formats): plan Epic 6 and ADR-005, kept intact and listed in the backlog (Story 5.1.4). Until it lands, the trace carries `compressionStatus: "off"`, `tokensAfter: null`, `firstByteMs: null` and no `firstByteState`, and the UI shows the defined `n/a` values from the Column Truth Table. Nothing in Epics 1-5 depends on it.

### Out of Scope
- Bazel migration work.
- New routing strategies or provider integrations.
- Auth/multi-user for the dashboard.

## Rabbit Holes
- Capturing the attempt chain across retry/fallback paths in multiple providers (anthropic, bedrock, openai, gemini, openrouter) — instrumentation may be scattered.
- Model-list APIs differ per upstream (some have none; Bedrock needs a separate control-plane call); caching/staleness and auth failures.
- Mapping "requested model" through alias/family resolution (`auto-model-family`, `haiku-tier-model-mapping`) — which name counts as "requested".
- Session Replay/benchmarks may depend on data that isn't recorded yet.

## Alternatives Considered
- Add `#[serde(rename_all = "camelCase")]` backend-side vs. adapt UI model to snake_case (decide in planning; check other SSE consumers).
- Per-trace attempt chain in the ring buffer vs. a separate trace store.
- Live-query upstream `/models` on demand vs. periodic cached refresh.

## Feasibility Risks
- Trace hot-path overhead and ring-buffer cap (100) may be too small for useful history.
- Some upstreams may not expose a model-list endpoint.

## Observability Requirements
Log (structured, no secrets) one line per upstream model-list refresh (outcome, count or error kind and status, duration); "trace-emission drops" means ring-buffer evictions (counted, logged at `debug`) plus the existing SSE lag event (`system_lag`, shown in the UI as a skipped-event count); expose model-list refresh age/error per upstream in the UI.

## Risk Control
No feature flag for the trace and catalog work (additive fields on SSE events and new `/api` endpoints); the deferred compression wiring will be gated by `compression.enabled` (default `false`, ADR-005). Rollback = revert commit. Keep old field names alongside new ones for one release if any non-UI SSE consumer is found.

## Value Risks
- Operators may not actually use the Overview table, the new Models view or the attempt waterfall (the core need is the Overview row plus the Why line). Two checks: (1) at the Epic 1 boundary Tyler uses the fixed table and existing Inspect link on one day of real traffic and records whether he used them (Task 1.1.4i); a "no" re-scopes Epic 2 before it starts. (2) Story 5.1.3 includes a usage check, where the operator-tester runs a real session and records whether Inspect and Models were reached for unprompted. If not, the waterfall bars (cut #3) and Models extras are cut first and the finding goes to the gap report.

## Open Questions
- Confirm snake_case vs camelCase mismatch by capturing a real SSE event.
- Where are requested model, routing reason, and per-attempt data available in the dispatch path today?
- Which upstreams expose model lists, and what does `/api/models` already return?
- What other UI elements are stubbed, hardcoded, or unwired (Session Replay, Benchmarks, Config Editor)?
