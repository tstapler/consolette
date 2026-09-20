# ADR-003: RESTful Axum Control Plane for Session Pruning Management and Thread-Safe Session Pruning State

**Status**: Accepted  
**Date**: 2026-09-18  
**Relates to**: `project_plans/memory-pruning/requirements.md` (HTTP API Endpoints & Observability); `project_plans/memory-pruning/research/architecture.md`; `project_plans/memory-pruning/research/pitfalls.md` (API Security & Input Validation)

---

## Context

Consolette's primary entrypoint ([`src/entrypoint/mod.rs`](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/entrypoint/mod.rs) and [`src/entrypoint/api.rs`](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/entrypoint/api.rs)) routes HTTP API requests for agent session proxying, capability management, and metrics.

Currently, consolette lacks an HTTP control plane to:
1. Manually or programmatically trigger pruning passes over active session transcripts.
2. Inspect transcript context memory statistics, turn metrics, and omission cache sizes.
3. Dynamically inspect or update global default pruning policies and per-session policy overrides at runtime.

Furthermore, exposing HTTP endpoints for transcript mutation introduces path traversal vulnerabilities (e.g. malicious `session_id = "../../../etc/passwd"`) and unauthenticated policy manipulation risks if not properly secured and validated.

---

## Decision

We implement a RESTful axum control plane mounted under `/session` in consolette's primary HTTP router and integrate thread-safe pruning state into `EntrypointState`.

### 1. HTTP Control Plane Endpoint Specification

The following endpoints are registered in `entrypoint_router` ([`src/entrypoint/mod.rs`](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/entrypoint/mod.rs)):

```rust
pub fn entrypoint_router(state: EntrypointState) -> axum::Router {
    axum::Router::new()
        // ... existing routes ...
        .route("/session/prune", axum::routing::post(api::post_session_prune))
        .route("/session/policy", axum::routing::post(api::post_session_policy))
        .route("/session/prune/stats", axum::routing::get(api::get_session_prune_stats))
        .with_state(state)
}
```

#### A. `POST /session/prune`
Triggers an immediate pruning pass over a target session transcript.

- **Request Schema**:
```json
{
  "session_id": "string (required, UUID v4 format)",
  "dry_run": "boolean (optional, default: false)",
  "policy_override": "PruningPolicy (optional)"
}
```
- **Dry-Run Mode Guardrail**: When `dry_run: true`, the pruning engine simulates evaluation in memory, calculating rows pruned, bytes freed, and token savings without mutating transcript files on disk or inserting records into [`OmissionCache`](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/claude_code_session/omission_cache.rs). This prevents counter sequence pollution (`omitted-001`) during preview passes.
- **Response Schema (200 OK)**:
```json
{
  "session_id": "94bd08fd-a105-4b01-b25d-160489a204e2",
  "rows_evaluated": 184,
  "rows_pruned": 22,
  "bytes_freed": 145200,
  "estimated_tokens_saved": 36300,
  "pruned_by_reason": {
    "size_threshold": 8,
    "turn_age": 9,
    "unreferenced_decay": 3,
    "capacity_lru": 2
  },
  "dry_run": false
}
```

#### B. `POST /session/policy`
Queries or updates the active global default policy or a specific session policy override.

- **Request Schema**:
```json
{
  "session_id": "string (optional - null or omitted updates global default)",
  "policy": "PruningPolicy (required)"
}
```
- **Response Schema (200 OK)**:
```json
{
  "status": "updated",
  "scope": "session",
  "session_id": "94bd08fd-a105-4b01-b25d-160489a204e2"
}
```

#### C. `GET /session/prune/stats`
Retrieves transcript context memory metrics and omission cache statistics for a session.

- **Query Parameters**: `session_id=<uuid>`
- **Response Schema (200 OK)**:
```json
{
  "session_id": "94bd08fd-a105-4b01-b25d-160489a204e2",
  "total_turns": 28,
  "total_rows": 210,
  "pruned_rows_count": 35,
  "omission_cache_entries": 35,
  "current_tool_output_bytes": 42100,
  "historical_bytes_pruned": 512000,
  "active_policy": { ... }
}
```

### 2. Thread-Safe `EntrypointState` Wiring

We extend `EntrypointState` in [`src/entrypoint/mod.rs`](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/entrypoint/mod.rs):

```rust
#[derive(Clone)]
pub struct EntrypointState {
    // ... existing fields ...
    pub pruning_policy_store: Arc<PruningPolicyStore>,
    pub omission_cache: Arc<OmissionCache>,
}
```

`PruningPolicyStore` uses `RwLock` primitives internally to allow concurrent, lock-free read access during proxy operations while allowing write access for policy updates via `POST /session/policy`.

### 3. API Security & Input Validation Guardrails

1. **Localhost Network Binding**: HTTP server binding is strictly restricted to `127.0.0.1` (loopback), ensuring external network actors cannot send unauthenticated HTTP requests to inject policies or trigger pruning.
2. **UUID Format Sanitization**: All incoming `session_id` string parameters are validated against UUID v4 format (`uuid::Uuid::parse_str`). Any request containing path traversal characters (`..`, `/`, `\`) is rejected immediately with `400 Bad Request`.
3. **Canonical Path Constraints**: Session transcript files are resolved exclusively within canonical project transcript paths (`~/.claude/projects/`).

---

## Alternatives Considered

| Option | Reason for Rejection |
| :--- | :--- |
| **CLI-Only Commands (No HTTP API)** | Rejection: Prevents automated context compaction hooks, background daemons, and management dashboards from triggering or inspecting pruning state dynamically. |
| **Unvalidated File Path Inputs** | Rejection: Creates critical path traversal vulnerabilities allowing unauthorized read/write access to arbitrary system files. |
| **Mutating Cache Counters during Dry-Run** | Rejection: Pollutes SQLite sequence counters (`omitted-001`), leading to primary key gaps and collisions in subsequent live pruning passes. |

---

## Consequences

### Positive
- Provides a comprehensive RESTful control plane for session pruning management, stats monitoring, and policy updates.
- Dry-run mode enables safe previewing of aggressive pruning policies without data or cache state mutations.
- Strict UUID validation and local binding eliminate path traversal and remote execution vulnerabilities.

### Negative / Tradeoffs
- Expands `EntrypointState` and `axum` router surface area.
- Requires maintenance of API documentation and response schema compatibility.
