# ADR-004: `ArcSwap<DispatchRouter>` for Instant Zero-Downtime Proxy Configuration Hot-Reloading

**Status**: Accepted  
**Date**: 2026-09-27  
**Project**: webui-redesign  

## Context

The `webui-redesign` introduces an interactive Proxy Configuration Editor allowing operators to visually view and modify provider priorities, rate-limit thresholds, fallback cascades, and routing rules via `PUT /v1/dashboard/config`.

Modifying routing rules at runtime must take effect instantly across all worker threads without restarting the Rust process or dropping active, in-flight LLM requests. Furthermore, incoming proxy requests must execute routing lookups with minimal lock overhead.

## Decision

We will store the live proxy dispatch router inside `ArcSwap<DispatchRouter>` within `EntrypointState`.

On `PUT /v1/dashboard/config`:
1. Validate the updated configuration payload against safety constraints (verifying model selectors, upstream reference validity, and domain allowlists for `base_url`).
2. Write changes to the TOML configuration file on disk (`<config_dir>/runtime-overrides.toml`).
3. Rebuild the `DispatchRouter` instance.
4. Atomically hot-swap the router pointer using `ArcSwap::store()`.
5. Broadcast a `DashboardEvent::ConfigChanged` SSE notification to all connected browser clients.

## Alternatives Considered

- **`RwLock<DispatchRouter>`**: Rejected because read operations (which occur on every single incoming proxy request) require acquiring read locks, introducing lock contention and latency overhead under high concurrent request volume.
- **Process Restart Requirement**: Rejected because restarting the binary drops active streaming LLM connections and creates unacceptable service disruption for proxy clients.
- **`Mutex<DispatchRouter>`**: Rejected due to severe lock contention that serializes parallel request handling across worker threads.

## Rationale

`ArcSwap` provides wait-free, lock-free read access (`load()`) with zero lock contention for high-throughput proxy request processing. Atomic pointer updates (`store()`) allow replacing the entire routing table in nanoseconds, guaranteeing that in-flight requests complete cleanly using their initial router reference while new requests immediately use the updated routing rules.

## Consequences

**Positive:**
- Zero downtime and zero latency penalty for proxy request routing during live configuration updates.
- Atomic pointer swaps guarantee consistent routing decisions per request lifecycle.
- Hot-reload updates are immediately persisted to disk and broadcast to all connected UI clients.
- Pre-validation of configuration rules prevents invalid state mutations or process panics.

**Negative / Risks:**
- Invalid payloads or unvalidated `base_url` inputs could introduce SSRF risks if non-loopback bindings are used (mitigated by strict hostname allowlists, loopback IP checks, and pre-validation).
- API key masking must be enforced on `GET /v1/dashboard/config` to prevent key exposure in UI responses.

**Follow-up work:**
- Update `EntrypointState` to wrap `DispatchRouter` in `ArcSwap`.
- Implement `PUT /v1/dashboard/config` handler with schema validation, TOML persistence, pointer swap, and SSE notification broadcast.

## Related

- Requirements: `project_plans/webui-redesign/requirements.md`
- Research: `project_plans/webui-redesign/research/architecture.md`
- Research: `project_plans/webui-redesign/research/pitfalls.md`
