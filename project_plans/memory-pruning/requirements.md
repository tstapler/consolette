# Requirements: memory-pruning

**Date**: 2026-09-18
**Type**: feature addition
**Complexity**: 3 — system design / multi-component transcript memory pruning

## Problem Statement
Claude Code session transcripts accumulate large amounts of tool execution output (e.g., `Bash` output, file reads, grep searches, subagent responses) over time. Unchecked accumulation increases context window consumption, latency, and cost. While `src/claude_code_session/prune.rs` currently implements a basic binary threshold check for completed `tool_result` content, consolette lacks turn-based pruning policies (pruning tool outputs unreferenced after X turns, age-off after X turns), configurable heuristics, capacity/LRU thresholds, pattern matching, and an HTTP API endpoint to manage or trigger pruning.

## Baseline
Today, consolette's transcript pruning (`src/claude_code_session/prune.rs`) only performs an inline size check against flat character/word limits at the moment tool output is generated, caching long tool outputs into `OmissionCache`. It has no notion of session turn age, tool output reference tracking, configurable pruning policies, LRU/capacity limits on stored session history, or HTTP API endpoints for manual/policy-driven transcript memory pruning.

## Users / Consumers
- Claude Code proxy runtime (`src/claude_code_session/`, `src/bin/mcp-proxy/`, axum endpoints)
- CLI / MCP clients interacting with consolette
- Automated context compaction hooks and session management processes

## Success Metrics
- Ability to automatically prune tool call outputs that have not been referenced/used in X turns.
- Ability to automatically prune/omit tool call outputs older than X turns based on configurable policy.
- Support for capacity/LRU limits and tool name pattern/glob matching in pruning rules.
- Availability of HTTP API endpoints (`POST /session/prune`, `POST /session/policy`, `GET /session/prune/stats`) for manual triggering and policy inspection/updates.
- Zero loss of transcript structural integrity — pruned rows maintain correct schema and omission placeholders referencing `OmissionCache`.

## Appetite
Medium (1–2 weeks)

## Constraints
- Must integrate cleanly with existing `TranscriptRow`, `OmissionCache`, `native_compaction`, and `prune.rs` modules.
- Must preserve exact transcript JSON formatting guarantees for Claude Code session restoration.
- Must not introduce noticeable latency overhead to live streaming/proxying paths.

## Non-functional Requirements
- **Performance SLO**: Pruning pass execution < 10ms for transcripts up to 5,000 rows.
- **Scalability**: Handle session histories with hundreds of turns and tens of MBs of output smoothly.
- **Security classification**: Internal session data processing.
- **Data residency**: Local in-memory / local disk cache processing only.

## Scope
### In Scope
- Turn-based decay policy: prune/omit `tool_result` outputs after X turns if unreferenced or exceeding turn age threshold.
- Heuristic-based automatic pruning: configurable policy evaluating turn age, usage references, capacity limits, and tool name filters.
- HTTP API endpoints for triggering pruning and setting/querying pruning policies on active sessions.
- Enhancements to `src/claude_code_session/prune.rs` and related modules to support turn awareness, usage tracking, and multi-criteria pruning.
- Integration tests validating turn-based pruning, API endpoints, and cache integrity.

### Out of Scope
- Destructive deletion of original uncompressed raw inputs stored in `OmissionCache` unless explicitly requested by cache eviction policy.
- Modification of Claude Code's internal CLI client binary.

## Rabbit Holes
- Over-pruning recent tool results needed by Claude Code in current execution loop — must ensure turn counts count backwards from the latest assistant turn.
- Misinterpreting "referenced/used in X turns" — need a clear definition of tool output references (e.g. assistant tool_use referring to previous result content or turn distance).
- Concurrent transcript mutation while live session is appending rows — must ensure thread safety and immutable snapshot passes.

## Alternatives Considered
- Simple fixed window (sliding window of last N messages) — rejected as too naive, loses valuable system prompts or recent user instructions.
- Full LLM summarization on every turn — rejected due to high latency, token cost, and risk of hallucinated loss of precision.

## Feasibility Risks
- Interfacing with `mcp-proxy` or `axum` routing requires clean integration with existing session state structures (`ClaudeCodeSession`, `OmissionCache`, etc.).

## Observability Requirements
- Emit metrics on rows checked, rows pruned, bytes freed, and turn distance distribution during pruning passes.
- Log pruning events with tracing `info!` / `debug!` context.

## Risk Control
- Configurable dry-run flag in HTTP API to preview pruned rows without modifying transcript state.
- Rollback capability: omission placeholders embed `content_id` allowing rewind if needed.

## Open Questions
- What default turn-based policy thresholds (e.g., 5 turns unreferenced, 10 turns max tool result retention) provide the best balance of context compression vs model performance?
