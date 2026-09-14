# Research: stapler-mcp Integration Seam

**Date**: 2026-09-13 | **Constraint**: backend MUST be stapler-mcp tooling
(requirements.md C-1). No new search vendor.

## Candidate seams

### A. Consolette as rmcp MCP client over stdio to the stapler-mcp binary (RECOMMENDED)

- How: consolette spawns the `stapler-mcp` native binary (path from config,
  default: `stapler-mcp` on `PATH`) as a child with stdio transport and speaks
  MCP `tools/call brave_web_search {query, count}` via the already-present
  `rmcp` client + `transport-io`.
- The child is itself a thin client: on startup it ensures the machine-wide
  daemon (`~/.stapler-mcp/daemon.sock`) is reachable, auto-spawning `--daemon`
  detached if needed. Consolette therefore inherits daemon sharing, caching,
  and key custody for free.

| Dimension | Assessment |
|---|---|
| Startup cost | First search pays child-spawn (~10–50ms) + daemon-ensure (0 if already up, ~100–300ms cold). Mitigated by a persistent child-process pool (see below), amortizing spawn to ~zero per request. |
| Per-request latency | One MCP round-trip over pipes + daemon socket proxy + Brave HTTP. Dominated by Brave API itself; proxy overhead is sub-ms-to-ms. |
| Credentials | `BRAVE_API_KEY` never leaves the daemon's env. Consolette config holds only a binary path. Nothing to redact beyond existing rules. Strongest option. |
| Failure modes | Binary missing / daemon unstartable / call timeout / tool missing — all observable as `Result::Err`, all degrade to drop behavior. Stale-socket safety already handled daemon-side. |
| Testability | Excellent: `BRAVE_API_BASE_URL` override + `STAPLER_MCP_HOME` isolation exist for real-daemon tests; plus a fake `rmcp` stdio server in consolette unit tests for hermetic coverage. |
| Coupling | Loose: tool name + JSON shapes only. Unknown fields tolerated. |

- **Pool shape (recommended)**: a small bounded pool (default 2 procs) of
  persistent stdio MCP clients, lazy-started on first search, health-checked
  with `ping`, evicted/restarted on failure, with per-call timeout and a
  consecutive-failure circuit breaker. Bounded ⇒ no fork storms (P9).
  Lazy ⇒ zero cost for non-search traffic. Shared via `Arc`.
- Why not spawn-per-search: correct but wasteful under concurrent load and
  harder to circuit-break. Pool is strictly better for ~100 lines more code,
  all inside the new module (testable).

### B. Shared-crate extraction (depend on stapler-mcp `crates/core`) — REJECTED

- Would pull schemars/fastembed/chromium-embeddable surface into the proxy
  binary, duplicate the daemon's HTTP/cache logic in-process, and force
  consolette to hold `BRAVE_API_KEY` (violates the spirit of C-1: keys stay in
  stapler-mcp's env). Couples release trains of two repos. Heavier binary,
  larger attack surface, no latency win worth it (Brave HTTP dominates anyway).

### C. HTTP seam (consolette → stapler-mcp over localhost HTTP) — REJECTED

- stapler-mcp exposes no HTTP server surface (socket-only by design:
  local-only, no port conflicts). Adding one means new listener code,
  auth decisions, and port management in a single-tenant tool — all cost, no
  benefit over stdio pipes, which already give us framing + backpressure.

### D. Shell-out per search (`stapler-mcp` CLI invocation returning JSON) — REJECTED

- No such batch/JSON CLI surface exists (it's an MCP server, not a CLI tool);
  inventing one is a feature request on another repo. Process-per-search cost
  with none of the pool benefits of A.

## Recommendation

**Seam A with a bounded persistent child-process pool.** Rationale: zero new
dependencies (`rmcp` client already in `Cargo.toml`), strongest credential
story (keys never enter consolette's address space), daemon sharing inherited
(cache, browser pool, single key holder), best testability (fake MCP server +
`BRAVE_API_BASE_URL` mock + `STAPLER_MCP_HOME` isolation), bounded resource
use. Fallback when the seam is down: degrade to drop (P6).

## Contract consolette depends on (pin in code comments + tests)

- Binary: `stapler-mcp` (native) on `PATH` or at configured absolute path.
- Tool: `brave_web_search`, input `{query: string, count?: number}`,
  output `{results: [{title, url, description}]}` (camelCase).
- Liveness: MCP `ping`. Env isolation: `STAPLER_MCP_HOME`.
- All of the above asserted by hermetic tests against a fake server so a
  stapler-mcp upgrade that breaks the contract fails fast in CI, not in prod.
