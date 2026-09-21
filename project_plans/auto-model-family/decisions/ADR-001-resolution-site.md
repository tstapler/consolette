# ADR-001: Family resolution site — table + dispatch step (A+C), not strategy decorator

**Date**: 2026-09-12 · **Status**: Accepted

## Context

`RoutingStrategy::select` is documented pure and health-blind (ADR-003, `strategy.rs:1-6`); `Router::dispatch` owns read→pins→health-filter→select→send→record. stack.md §4 suggested a stats-ranked strategy impl; architecture.md §2-B rejects it as breaking ADR-003. Both entrypoints (`/v1/messages`, `/v1/chat/completions`) share `dispatch`.

## Decision

Adopt A+C: a `FamilyTable` (alias → members, built in `from_config`) plus a pure `FamilyResolver::rank` consulted at the top of `dispatch`, right after `effective_candidates` and before the health-filter loop. Alias match returns an ordered member list; the existing loop/strategy iterates it unchanged. No new `RoutingStrategy` impl.

## Consequences

- Strategy trait untouched; cooldown/admission/session-pin semantics preserved.
- Dispatch gains ~10 lines + one ranked-order computation over 2–3 members (negligible overhead).
- Session pins bypass family expansion (pins-first).

## Alternatives rejected

- Stats-aware strategy decorator: breaks ADR-003 purity, duplicates health filter.
- Per-entrypoint rewrite: duplicates logic, bypasses health/cooldown/session context.
