# Implementation Plan: Server-Tool Emulation (web_search)

**Feature**: In-proxy emulation of Anthropic `web_search` server tool via stapler-mcp
**Date**: 2026-09-13
**Status**: Ready for implementation (pending owner decisions D1–D3 below)
**ADRs**: None (no non-standard choices; rmcp client already a dependency)

---

## Dependency Visualization

```
Phase 1 (pure core)          Phase 2 (I/O + loop)         Phase 3 (wire-up)
─────────────────────        ────────────────────         ─────────────────
T1 detect/rewrite  ──┐
T2 accumulate/map  ──┼──▶ T5 executor trait+pool ──▶ T7 Full-path loop ──▶ T9 entrypoint wire-up
T3 SSE synthesize ──┘         T6 mapping brave→ws_result ─┘   T8 Stream buffering ─┘
                                                        T10 config knobs ──▶ T11 metrics/logs
                                                                              T12 e2e + docs
```

---

## Phase 1: Pure core (`src/server_tools/`, no I/O)

### Epic 1.1: Server-tool shapes as data

**Goal**: Detect, rewrite, accumulate, and map server-tool turns with pure,
unit-tested functions.

#### Story 1.1.1: Detect + rewrite

**As an** operator, **I want** server web_search defs reliably detected and
rewritten to an upstream-callable function, **so that** non-executing
upstreams can participate in search.
**Acceptance Criteria**:
- `is_server_web_search_def(tool)` true for all three versioned types
  (`web_search_20250305/20260209/20260318`) with `name == web_search`; false
  for user functions, including one coincidentally named `web_search` without
  a `web_search_*` type.
- `rewrite_for_upstream(body)` swaps each server def for a synthetic function
  def (`name: web_search`, `{query: string}` required, description present,
  `input_schema` non-empty so the Cohere 400 class is impossible) and records
  the extracted `max_uses` / domain / location hints in a sidecar struct
  (never sent upstream).
- Original server defs are removed from the outbound body; all other tools
  pass through untouched.
**Files**: `src/server_tools/detect.rs` (new), `src/server_tools/mod.rs` (new)

##### Task 1.1.1a: detect + rewrite pure functions + unit tests (~5 min)
- Implement `ServerWebSearchDef { max_uses, allowed_domains,
  blocked_domains, user_location }`, `is_server_web_search_def`,
  `rewrite_for_upstream`.
- Files: `src/server_tools/detect.rs`, `src/server_tools/mod.rs`

#### Story 1.1.2: Accumulate + map back

**As a** client, **I want** the final turn to look like a native server-tool
response, **so that** Claude Code renders search + answer normally.
**Acceptance Criteria**:
- `accumulate` converts each upstream `tool_use(name=web_search)` into a
  `server_tool_use` block (id `srvtoolu_…` minted deterministically per
  iteration) paired with executor output mapped to `web_search_tool_result`.
- Non-search `tool_use` blocks pass through; mixed turn ⇒ `stop_reason:
  tool_use` preserved (client executes the rest).
- `usage` aggregates: summed input/output tokens + 
  `server_tool_use.web_search_requests == searches executed`.
- Lossy-mapping table documented in code comments (no encrypted blobs, no
  citation attachments, domain/location filters logged-and-ignored in V1).
**Files**: `src/server_tools/mapping.rs` (new)

##### Task 1.1.2a: mapping pure functions + unit tests (~5 min)
- Implement `to_server_tool_use`, `to_web_search_tool_result`
  (truncate `description` per result cap with `…(truncated)` marker),
  `aggregate_usage`, history back-conversion
  (server-shape → function-shape for re-dispatch).
- Files: `src/server_tools/mapping.rs`

#### Story 1.1.3: SSE synthesis (streaming V1)

**As a** streaming client, **I want** a well-formed Anthropic SSE sequence for
emulated turns, **so that** `stream: true` works without mid-stream execution.
**Acceptance Criteria**:
- `synthesize_sse(final_message)` emits `message_start`,
  `content_block_start/delta/stop` per block (server blocks as JSON deltas),
  `message_stop`, with `stop_reason: end_turn` on success.
- Never emits function-shape `tool_use` on the wire; golden frame-by-frame
  test.
**Files**: `src/server_tools/sse.rs` (new)

##### Task 1.1.3a: SSE synthesizer + golden test (~5 min)
- Files: `src/server_tools/sse.rs`

---

## Phase 2: Executor + loop

### Epic 2.1: stapler-mcp executor

**Goal**: Bounded, credential-clean search execution over seam A (pool).

#### Story 2.1.1: Executor trait + rmcp pool

**As an** operator, **I want** searches executed through a bounded pool with
degrade-on-failure, **so that** backend outages never fail requests.
**Acceptance Criteria**:
- `SearchExecutor` trait (`search(query, count) -> Result<Vec<SearchResult>,
  ExecutorError>`); production impl spawns/manages ≤N persistent stdio MCP
  children (`stapler-mcp` binary path from config), lazy start, `ping`
  health-check, per-call timeout, consecutive-failure circuit breaker.
- `ExecutorError` → error-content `tool_result` (never `ProviderError`,
  never touches `HealthRegistry`).
- Fake `rmcp` stdio server in tests; contract test pins tool name + shapes.
**Files**: `src/server_tools/executor.rs` (new)

##### Task 2.1.1a: trait + error type + fake-server harness (~5 min)
##### Task 2.1.1b: pool (spawn, ping, timeout, breaker) (~5 min)
- Files: `src/server_tools/executor.rs`

#### Story 2.1.2: Loop orchestrator

**As a** client, **I want** multi-turn search handled inside the proxy,
**so that** follow-up searches ground the final answer.
**Acceptance Criteria**:
- `run_loop(initial_body, dispatch_fn, executor, limits)` implements the
  architecture.md algorithm; terminates on: no new `web_search` tool_use,
  `iterations >= min(max_uses, max_iters, HARD_CEILING=10)`, total deadline,
  executor circuit-open (degrade: return best answer so far).
- Each iteration calls the injected `dispatch_fn` (router in prod, fake in
  tests) — loop itself has no routing logic.
- "Model always searches" test terminates at the cap with a complete answer.
**Files**: `src/server_tools/loop.rs` (new)

##### Task 2.1.2a: orchestrator + termination tests (~5 min)
- Files: `src/server_tools/loop.rs`

#### Story 2.1.3: Browser fallback when Brave is unconfigured

**As an** operator without a Brave key, **I want** searches to fall back to
browser-driven search, **so that** emulation works with zero API keys.
**Scraping lives in stapler-mcp, not here** (owner decision 2026-09-13):
stapler-mcp ships a new `browser_web_search` tool that owns the DDG
navigation + extraction internally; consolette only orchestrates the
fallback chain over seam A.
**Acceptance Criteria**:
- Prerequisite (other repo): stapler-mcp provides `browser_web_search`
  ([stapler-mcp#46](https://github.com/tstapler/stapler-mcp/issues/46))
  with Brave-mirroring contract — input `{query: string, count?: number}`,
  output `{results: [{title, url, description}]}` — so consolette maps both
  backends through the identical `SearchResult` path. Consolette work is
  blocked on the tool existing for live e2e, but NOT for unit work: develop
  against the fake MCP server + contract test, go live when stapler-mcp
  ships it.
- Fallback chain per search: `brave_web_search` → (on missing-key/auth
  error only) `browser_web_search` → (on browser failure) executor error ⇒
  drop-degrade. Missing-key detection matches stapler-mcp's stable
  `"BRAVE_API_KEY is not set"` error string (pinned by contract test);
  401/403 also fall back; 429/5xx/timeouts propagate as executor errors
  (no silent masking of billing/rate signals).
- Separate `browser_timeout_ms` (default 30000 — browser search runs 3–10s
  vs ~1s Brave); `server_tool_searches_total{outcome,backend}` labels
  `brave` vs `browser` so the dashboard shows which path serves traffic.
- No system Chrome on the daemon host ⇒ browser path fails fast ⇒ drop.
  Documented as an operational requirement in README/config example
  (Chrome lives with the daemon, never with the proxy).
- Tests: fake-MCP fallback test (brave errors with the missing-key string
  ⇒ `browser_web_search` called, results returned in the uniform shape);
  429-from-Brave test asserts NO browser call; contract test pins the new
  tool's name + input/output shapes so a stapler-mcp drift fails fast in
  consolette CI.
**Files**: `src/server_tools/executor.rs` (chain wiring), metrics, docs

##### Task 2.1.3a: fallback chain + contract tests (~5 min)
##### Task 2.1.3b: backend-labeled metrics + docs (~5 min)

---

## Phase 3: Wire-up (thin transport)

### Epic 3.1: Entrypoint + config + observability

**Goal**: Expose emulation on `POST /v1/messages` with safe defaults.

#### Story 3.1.1: Full-path wire-up

**As a** client, **I want** `stream: false` search requests emulated,
**so that** I get grounded answers on any upstream.
**Acceptance Criteria**:
- `post_v1_messages` branches to the loop only when body has a server
  web_search def AND route upstream kind is non-Anthropic; else today's path
  byte-identical (regression test with fixture diff).
- Per-iteration `CostTracker` records under the same session/request id.
- Executor failure ⇒ drop-degrade (strip def, single dispatch), never an
  error response for a backend outage.
**Files**: `src/entrypoint/messages.rs` (thin branch only), 
`src/server_tools/mod.rs`

##### Task 3.1.1a: entrypoint branch + cost accumulation (~5 min)
##### Task 3.1.1b: capability gate + non-Anthropic routing check (~5 min)

#### Story 3.1.2: Streaming (buffered) support

**As a** streaming client, **I want** `stream: true` search requests to work,
**so that** Claude Code's default mode is covered.
**Acceptance Criteria**:
- Stream + server def ⇒ internal `stream: false` loop, then synthesized SSE
  via `CostTrackingStream`-compatible framing; cost tee counts final bytes.
- Time-to-first-byte regression documented; non-search streams untouched.
**Files**: `src/entrypoint/messages.rs` (thin), `src/server_tools/sse.rs`

##### Task 3.1.2a: buffered-stream branch + e2e SSE test (~5 min)

#### Story 3.1.3: Config + observability

**As an** operator, **I want** safe-default knobs and visibility, **so that**
I can tune without reading code.
**Acceptance Criteria**:
- `[server_tools]` table: `enabled=true`, `backend_path="stapler-mcp"`,
  `max_iterations=5`, `per_search_timeout_ms=15000`, `total_timeout_ms=120000`,
  `browser_timeout_ms=30000` (Story 2.1.3 fallback path),
  `max_results=5`, `pool_size=2`. All optional; missing table ⇒ defaults.
- Metrics: `server_tool_searches_total{outcome}`, `server_tool_iterations`,
  loop latency; logs note ignored domain/location filters; no secrets logged
  (redaction test).
**Files**: `src/config/schema.rs` (additive), metrics + log lines

##### Task 3.1.3a: config knobs + metrics/logs (~5 min)

#### Story 3.1.4: E2E + docs

**Acceptance Criteria**: hermetic e2e (fake upstream + fake MCP server) for
Full and Stream; Cohere-400 regression test now succeeds with results;
backend-down test degrades to drop. README/config-example snippet added.
**Files**: `tests/server_tool_emulation.rs` (new), config example

##### Task 3.1.4a: e2e tests + docs (~5 min)

---

## Decisions (owner — decided 2026-09-13)

- **D1 — Bedrock**: EMULATE EVERYWHERE (owner overrode passthrough
  recommendation — uniform loop, Bedrock included).
- **D2 — Defaults**: ACCEPTED — `max_iterations=5` (hard ceiling 10),
  timeouts 15s per-search / 120s total, pool size 2.
- **D3 — `count` mapping**: YES — `max_results` config → Brave `count` per
  search, default 5.
- **D4 — Browser fallback**: YES — chain `brave_web_search` →
  stapler-mcp-side browser search → drop. Scraping lives in stapler-mcp
  (new `browser_web_search` tool, Brave-mirroring contract); consolette only
  wires the chain. Fallback triggers on missing-key (`"BRAVE_API_KEY is not
  set"`) + 401/403 only; 429/5xx propagate. `browser_timeout_ms=30000`
  default, backend-labeled metrics. See Story 2.1.3. Consolette live-e2e for
  the fallback path is blocked until stapler-mcp ships the tool; unit work
  proceeds against the fake server + contract test.

## Task count

15 tasks across 4 epics / 9 stories (was 13/8 before Story 2.1.3). Max 3–5
files per task; transport edits confined to `src/entrypoint/messages.rs`
branches + config schema additive fields.
