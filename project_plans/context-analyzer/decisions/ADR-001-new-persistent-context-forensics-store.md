# ADR-001: New `rusqlite`-backed context-forensics store, not an extension of `cost_metrics::store`

**Status**: Accepted
**Date**: 2026-08-24
**Relates to**: requirements.md's resolved Open Question 2; `research/stack.md` §1, `research/architecture.md` §1, `research/pitfalls.md` §5

## Context

The requirements doc left open whether context-analyzer's persistent store (sessions, per-call composition, turns, hook events, subagents — five-plus tables, must survive process restarts, must support cross-session historical queries) should extend `cost_metrics::store::SessionCostStore` or stand alone.

`SessionCostStore` is a `moka::future::Cache` with `max_capacity(1000)` and a 1-hour TTL (`src/cost_metrics/store.rs:130-136`) — explicitly designed to be safe to lose on restart, scoped to `SessionCompactionPipeline`'s own actual-vs-counterfactual accounting (`cost_metrics/mod.rs:1-6`). It has no persistence layer of any kind today.

`rusqlite` (`bundled` feature) is already a direct dependency (`Cargo.toml:95`) with two working precedents for exactly this shape of problem: `claude_code_session::omission_cache::OmissionCache` (permission-hardened, WAL-mode, `Mutex<Connection>`) and `bin/cmdcrush/metrics_store::SqliteMetricsExporter`.

## Decision

Build a new, separate store: `src/context_forensics/store.rs`'s `ContextForensicsStore`, `rusqlite`-backed, at `~/.claude/consolette/context-forensics.sqlite`, following `OmissionCache`'s exact hardening pattern (`0700`/`0600`, WAL mode). Do not extend `SessionCostStore`, and do not add a second SQLite/ORM dependency.

## Consequences

- Two stores now exist under `cost_metrics`-adjacent code for genuinely different data-durability needs — acceptable, since retrofitting durability onto a TTL-evicted cache would fight its own eviction invariants (a materially worse outcome than a second small store).
- No new Rust dependency: `rusqlite` was already present for this exact class of problem.
- The new store's schema is free to grow (hook_events, subagents, proxy_cross_check) without touching `cost_metrics::store`'s existing tested behavior.
