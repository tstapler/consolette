# Requirements: context-analyzer

**Date**: 2026-08-24
**Type**: feature addition
**Complexity**: 3 — system design

## Problem Statement
Tyler has no visibility inside consolette into where his Claude Code (and Codex CLI) token/context spend actually goes — which turns, tool calls, or stale context are driving cost. He wants context-window forensics (per-call/per-turn composition, context growth, cross-session trends, message inspection) equivalent to [manavgup/context-analyzer](https://github.com/manavgup/context-analyzer), but native to consolette rather than a second standalone tool.

## Baseline
Today Tyler's options are: (a) consolette's existing `cost_metrics` dashboard, which only tracks session-level actual-vs-counterfactual token/cost for its own compaction pipeline and only sees traffic that's routed through consolette's proxy; or (b) running context-analyzer (Python) standalone, which duplicates infrastructure — its own Claude Code hooks, its own SQLite DB, its own dashboard — with no connection to consolette's data or store.

## Users / Consumers
Tyler (single user), via consolette's CLI / dashboard / MCP server.

## Success Metrics
For any Claude Code or Codex CLI session, Tyler can open one consolette-served dashboard and identify (1) the top token-cost contributor per turn (Tool I/O vs Conversation vs System) and (2) the turn/session where context crossed a chosen budget threshold — without leaving consolette or running a second tool.

## Appetite
Large (3–6 weeks)
*(Scope must fit the appetite. If it doesn't fit, cut scope — do not move the deadline. See Rabbit Holes and Feasibility Risks for where to cut first.)*

## Constraints
Solo project, personal use. Must not break Tyler's existing Claude Code `~/.claude/settings.json` hooks — install/uninstall of any new hooks must be additive, backed up, and reversible, mirroring context-analyzer's own idempotent `context-tracker up`/`down` design. (consolette's existing `session_compaction::hooks` module is an in-process proxy-side hook trait, unrelated to Claude Code CLI's own settings.json hooks — no reuse there.)

## Non-functional Requirements
- **Performance SLO**: not specified — informal personal tool, no SLA
- **Scalability**: not applicable — single user, session sizes bounded by whatever Claude Code/Codex CLI produce (context-analyzer's own numbers reference up to ~1M-token sessions)
- **Security classification**: internal/personal — transcripts contain full conversation content; store and dashboard must stay local (localhost-only), consistent with context-analyzer's local-only design
- **Data residency**: local disk only, no cloud upload

## Scope
### In Scope
- Claude Code hook install/uninstall (mirrors context-analyzer's `up`/`down`: idempotent, backs up settings.json first) capturing tool calls, compaction events, session lifecycle, and subagent activity
- Transcript ingestion for Claude Code (`~/.claude/projects/*.jsonl`) and Codex CLI (`~/.codex/sessions/`) session logs, parsed for exact API token usage (input/output/cache_read/cache_creation)
- Persistent store for sessions, per-call token breakdowns, turns, hook events, and subagents — a new `rusqlite`-backed store, separate from `cost_metrics::store` (resolved from Open Questions below: that store is in-memory/TTL-evicted, the wrong shape for durable cross-session data)
- Dashboard views, matching context-analyzer's feature set:
  - Composition breakdown (Tool I/O vs Conversation vs System) per call/turn
  - Context-growth-per-turn chart with budget thresholds (200K/500K/700K/1M) and autocompact line
  - Cross-session analytics (cost/call vs peak context, trends, sortable session table)
  - Message inspector (full turn content) and cache-read churn chart
- Where both exist, cross-check transcript-derived usage against consolette's existing proxy-captured usage (per decision below: transcripts are primary, proxy data supplements/cross-checks)
- Expose the new store's data via `consolette mcp` MCP tool(s), alongside the dashboard (resolved from Open Questions below)

### Out of Scope
- Publishing consolette as a package for other users' use of this feature — this is Tyler's personal tooling, not a general release
- Depending on or porting context-analyzer's actual Python code or SQLite DB — independent Rust reimplementation informed by its design and public README only (context-analyzer is MIT-licensed; note the license if any snippet is ever directly referenced, but no verbatim copying is planned)
- Windows-specific path handling beyond whatever consolette already supports
- **Offline headroom/compression-ceiling audit** (context-analyzer's `audit-headroom`) — deferred to a follow-up (see Open Questions resolution). Reimplementing its Python dependency in Rust carries real correctness risk; shelling out to Python violates consolette's Rust-only constraint.

## Rabbit Holes
- **Headroom/compression-ceiling audit** depends on a third-party Python package (`headroom-ai`) with no Rust equivalent — options are an FFI/subprocess shim to Python or a from-scratch Rust reimplementation; both are open-ended. **Resolved: deferred to a follow-up**, out of scope for this pass (see Out of Scope).
- **Transcript/rollout-log schema parsing** for Claude Code and Codex CLI: both are undocumented, version-drifting formats owned by external tools. Parsing them robustly — and staying correct as those tools evolve — has unbounded depth; scope the initial parser to the fields actually needed for the four dashboard views, not full schema coverage.
- **`~/.claude/settings.json` hook installation** mutates a live, shared file that other tooling (consolette's own future features, other Claude Code plugins) may also touch. Getting this wrong breaks Tyler's actual Claude Code hooks — treat the install/uninstall path with the same care as context-analyzer's own backup-first design.
- **Store schema design**: whether to extend consolette's existing `cost_metrics::store` (currently in-memory, no SQLite) with context-analyzer's tables (blocks, turns, hook_events, subagents, subagent_api_calls, tool_result_offloads) or introduce a separate store. Could sprawl if not scoped tightly in Phase 3.
- Overall scope is genuinely large: 4 dashboard view groups × 2 CLI tools (Claude Code + Codex) × cross-checking against proxy data, inside one Large appetite. Phase 3 planning should sequence epics so a usable subset (e.g., composition + context-growth for Claude Code transcripts only) ships even if the full scope doesn't fit.

## Alternatives Considered
- Keep running context-analyzer (Python) standalone alongside consolette — rejected; Tyler wants this functionality inside consolette, sharing its data/store/dashboard rather than a second tool.
- Extend only consolette's existing proxy-based `cost_metrics` pipeline, without transcript ingestion — rejected; it misses any session not routed through consolette's proxy and misses hook-only signals (compaction events, subagent activity) that only the transcript/hooks capture.

## Feasibility Risks
- Headroom audit's Python dependency (see Rabbit Holes)
- Transcript/rollout-log schema drift for Claude Code and Codex CLI
- Settings.json hook install safety — a bug here has blast radius beyond this feature
- Total scope (4 view groups × 2 CLIs × dual data source) may not fit even a Large appetite; needs explicit epic sequencing in Phase 3 with a fallback cut line

## Observability Requirements
Standard request logging sufficient — no oncall alerting (personal tool, no SLA). Ingestion should be resilient to malformed input: a malformed transcript line or unreadable session file should log a warning and be skipped, not crash the dashboard or ingestion pipeline.

## Risk Control
Hook install/uninstall must be idempotent and reversible: back up `~/.claude/settings.json` before modifying it, and provide an explicit uninstall path that restores it — mirroring context-analyzer's own `up`/`down` design. No feature flag needed (personal, single-user tool); rollback is the uninstall command plus the settings.json backup.

## Open Questions
*(resolved after Phase 2 research)*
- ~~Should the new dashboard/analytics also be exposed via `consolette mcp`?~~ **Resolved: yes** — include MCP exposure in this pass, alongside the dashboard (user decision).
- ~~Should ingestion reuse `cost_metrics::store` or a new persistent store?~~ **Resolved: new `rusqlite`-backed store.** `cost_metrics::store::SessionCostStore` is in-memory/moka/TTL-evicted (process-lifetime only) — wrong shape for durable cross-session data. `rusqlite` is already a dependency (`Cargo.toml:95`) with two direct precedents to copy: `claude_code_session/omission_cache.rs`, `bin/cmdcrush/metrics_store.rs`.
- ~~Headroom/compression-ceiling audit approach?~~ **Resolved: defer to a follow-up**, not included in this pass. Reimplementing `headroom-ai`'s actively-churning algorithm in Rust carries real correctness risk; shelling out to Python violates consolette's Rust-only constraint. Revisit once the rest of the feature ships.
