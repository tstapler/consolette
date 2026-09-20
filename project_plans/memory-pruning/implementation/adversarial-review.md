# Adversarial Review Report: `memory-pruning` (Re-Review)

**Project**: `memory-pruning`  
**Target Architecture**: Turn-Based Multi-Criteria Transcript Memory Pruning Engine & HTTP Control Plane  
**Target Repository**: `/home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette`  
**Date**: 2026-09-18  
**Reviewer**: Adversarial Reviewer Agent  

---

## 1. Executive Summary

A follow-up adversarial review was conducted on the updated implementation plan (`project_plans/memory-pruning/implementation/plan.md`) for the `memory-pruning` project. The review evaluated the plan against project requirements (`project_plans/memory-pruning/requirements.md`), accepted Architecture Decision Records (`ADR-001`, `ADR-002`, `ADR-003`), background research files (`architecture.md`, `features.md`, `pitfalls.md`, `stack.md`), and the existing `consolette` codebase (`src/claude_code_session/` and `src/entrypoint/`).

The updated implementation plan (`plan.md`) successfully addresses all **7 previous technical concerns, concurrency hazards, and missing tasks** identified in the initial review. The plan now incorporates explicit tool name lookup resolution, robust reference tracking, size/mtime pre-rename checks for live append protection, UUID-to-path session resolution, subagent turn age indexing, SQLite primary key collision suffix retries, and trailing turn protection precedence over capacity limits.

No remaining blockers, critical hazards, or architectural gaps were identified. The implementation plan is fully verified and ready for execution.

---

## 2. Re-Review Summary Matrix

| Category | Finding Count | Previous Status | Current Status | Resolution Details |
| :--- | :--- | :--- | :--- | :--- |
| **Tool Name Resolution** | Finding 1 | **High Risk** | **Resolved** | Task 2.2.4 builds `ToolNameMap` mapping `tool_use.id` to `tool_name` from assistant turns for glob rule matching on API `tool_result` blocks. |
| **Reference Tracking** | Finding 2 | **High Risk** | **Resolved** | Tasks 2.2.1–2.2.3 inspect assistant turns for linked `tool_use` parameters (file paths, tool IDs) to prevent false-negative evictions. |
| **Live Append Protection** | Finding 3 | **Critical Hazard** | **Resolved** | Task 4.2.4 implements pre-rename file size and `mtime` verification before atomic swap to protect against un-locked CLI appends. |
| **Session Path Resolution** | Finding 4 | **High Risk** | **Resolved** | Task 5.1.0 adds `resolve_session_path` resolving `session_id` UUIDs to `~/.claude/projects/*/<session_id>.jsonl` with traversal validation. |
| **Subagent Sidechain Indexing** | Finding 5 | **Medium Impact** | **Resolved** | Task 2.1.4 maps sidechain tool rows to parent main-chain assistant turn indices for accurate turn age decay. |
| **SQLite PK Retry Loop** | Finding 6 | **Consistency Gap** | **Resolved** | Task 4.3.1 updates `OmissionCache::insert` to use `Immediate` transactions with a monotonic suffix retry loop (`omitted-001_1`). |
| **Precedence Rule Bounds** | Finding 7 | **Consistency Gap** | **Resolved** | Task 3.2.2 enforces `preserve_recent_turns` precedence over LRU capacity caps, emitting `tracing::warn!` if recent turns exceed budget. |

---

## 3. Detailed Verification of Previous Findings

### 3.1 Architectural Alignment & Feasibility

#### Finding 1: Unresolved Tool Name Lookup Deficit in Pruning Rules
- **Previous Finding**: Real Claude API `tool_result` content blocks carry no `tool_name` field, causing glob rules to default to `"unknown"` and fail matching.
- **Verification**: `plan.md` Task 2.2.4 defines building `ToolNameMap` during transcript scanning by indexing `tool_use.id -> tool_name` from assistant turns. Task 3.1.1 and the sequence diagram confirm passing resolved tool names to `prune_tool_row_with_policy`.
- **Status**: **RESOLVED**

#### Finding 2: Reference Tracking False-Negative Eviction Risk
- **Previous Finding**: Assistant text outputs rarely cite literal `tool_use_id` strings, risking false-negative eviction of 99% of active tool outputs.
- **Verification**: `plan.md` Task 2.2.1 and Task 2.2.2 explicitly extend `ReferenceMap` resolution to cross-reference input parameters (`file_path`, `path`, `command`, `id`) and placeholder references across assistant turns.
- **Status**: **RESOLVED**

---

### 3.2 Concurrency, Data Loss & Security Hazards

#### Finding 3: Advisory `flock` Ineffectiveness Against Live CLI Appends
- **Previous Finding**: Advisory `flock` does not block Claude Code CLI appends during pruning, risking permanent erasure of newly appended rows during atomic replacement.
- **Verification**: `plan.md` Task 4.2.4 adds a pre-rename validation check comparing file size and `mtime` against the initial read snapshot immediately before atomic `rename`, aborting and retrying if the file was modified in-flight.
- **Status**: **RESOLVED**

---

### 3.3 Tasks & Technical Implementation Completeness

#### Finding 4: Missing Transcript Path Resolution in Axum Control Plane
- **Previous Finding**: `POST /session/prune` received a `session_id` UUID, which could not be opened directly as a file path.
- **Verification**: `plan.md` Task 5.1.0 introduces `resolve_session_path` to resolve session UUIDs within `~/.claude/projects/*/<session_id>.jsonl`. Task 5.1.5 adds strict UUID v4 sanitization to block path traversal.
- **Status**: **RESOLVED**

#### Finding 5: Subagent Sidechain Rows Unindexed by Turn Age
- **Previous Finding**: Subagent sidechain rows (`is_sidechain == true`) were unindexed by turn age, bypassing turn-age decay rules.
- **Verification**: `plan.md` Task 2.1.4 explicitly assigns turn indices to sidechain tool results based on the parent main-chain assistant turn index.
- **Status**: **RESOLVED**

#### Finding 6: Missing SQLite Primary Key Collision Suffix Retry Loop
- **Previous Finding**: `plan.md` lacked the monotonic suffix retry loop required by `ADR-002` during SQLite primary key collisions.
- **Verification**: `plan.md` Task 4.3.1 and the sequence diagram incorporate `TransactionBehavior::Immediate` alongside a suffix retry fallback loop (`omitted-001_1`, `omitted-001_2`, ...).
- **Status**: **RESOLVED**

---

### 3.4 Consistency with ADRs & Research

#### Finding 7: Missing Precedence Rule Enforcement for Capacity Bounds
- **Previous Finding**: `plan.md` did not enforce `preserve_recent_turns` precedence over `max_tool_context_bytes` as required by `ADR-001`.
- **Verification**: `plan.md` Task 3.2.2 explicitly stops LRU eviction when protected recent turns remain and emits `tracing::warn!` if protected turns alone exceed byte budget.
- **Status**: **RESOLVED**

---

## 4. Final Verdict

Verdict: CLEAN
