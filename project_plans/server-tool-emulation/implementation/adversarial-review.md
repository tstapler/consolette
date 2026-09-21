# Adversarial Review: Server-Tool Emulation plan

**Date**: 2026-09-13
**Verdict**: CONCERNS (0 blockers, 4 concerns, 2 minors)
**Mode**: inline self-review (no subagent tool available in this session);
plan re-checked against requirements.md, research/*, and the verified background.

## Blockers

(None — the plan respects C-1/C-2, keeps router semantics intact, and degrades
to the standing drop-fix on every backend failure path.)

## Concerns

- [ ] **C1 — Loop lives outside the router, but session-pin + hot-swap
  interplay is untested.** `POST /api/route` hot-swaps the router mid-loop
  (`ArcSwap`); iteration N+1 could dispatch under a different route than
  iteration N. Recommendation: snapshot the dispatch `Arc` once per client
  request (already the natural shape — `load()` once in the handler) and note
  it in T3.1.1a acceptance. — Accepted into plan (handler holds one `Arc`).
- [ ] **C2 — Mixed-turn passthrough leaks the synthetic function def.**
  If the model calls another (user) tool alongside `web_search`, the client
  receives `stop_reason: tool_use` with function-shape blocks — but the
  synthetic `web_search` def was in the upstream tool list, not the client's.
  Recommendation: strip synthetic defs from any client-visible tool surface
  and convert our executed calls to `server_tool_use` pairs even in mixed
  turns. — Noted for T1.1.2a.
- [ ] **C3 — `count_tokens`/estimate path ignores search results.**
  Pre-dispatch token estimate (`estimate_tokens`) runs before the loop, so
  admission control under-counts long search-augmented turns. Recommendation:
  document as known limitation; optionally re-check admission per iteration
  (follow-up, not V1 gate). — Document in T3.1.1a.
- [ ] **C4 — SSE synthesis vs `CostTrackingStream` double-count.**
  The tee counts streamed bytes while the loop already recorded per-iteration
  usage; reconciliation rule (sum-of-parts == total) must cover the Stream
  branch explicitly. Recommendation: validation case V-STREAM-03 asserts
  single-count. — Added to validation.md.

## Minors

- User function literally named `web_search` colliding with the synthetic def:
  rename outbound user fn to `web_search_user` (documented edge; rare).
- `allowed_callers: ["direct"]` (ZDR/dynamic-filtering disable marker) is
  silently ignored — fine for V1, note in docs.

## Verdict rationale

No concern breaks the architecture or violates constraints; all four are
handled as acceptance-criteria notes, not redesigns. Proceed to Phase 4.
