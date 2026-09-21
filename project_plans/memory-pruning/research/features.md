# Research: Feature Landscape — memory-pruning

**Date**: 2026-09-18  
**Scope**: Analysis of LLM agent tool-loop context management, tool result pruning heuristics (Claude Code, magic-compact `prune.ts`, turn-decay, unreferenced eviction, capacity/LRU bounds), existing consolette session infrastructure (`src/claude_code_session/`), and proposed API/policy specifications for `memory-pruning`.

---

## 1. Executive Summary & System Context

As LLM agent tool loops (such as Claude Code) execute long-running coding and research tasks, session transcripts accumulate extensive tool inputs and outputs (`Bash` stdout/stderr, `Read` file contents, `Grep` search matches, `Edit` diffs, subagent turns). Without proactive transcript memory pruning:
1. **Context Window Exhaustion**: Transcripts hit token limits (200k/1M tokens), causing catastrophic context truncation or forced expensive auto-compaction.
2. **Inference Latency & Cost Inflation**: Every turn resends prior tool outputs in the prompt context, linearly increasing per-turn prefill cost and TTFT (time-to-first-token).
3. **Model Distraction**: Outdated or unreferenced tool outputs from past turns pollute the attention mechanism with stale code snapshots or completed command output.

Consolette currently possesses a baseline inline size pruner (`src/claude_code_session/prune.rs`) and an SQLite-backed `OmissionCache` (`src/claude_code_session/omission_cache.rs`). However, the existing implementation operates purely as a binary, flat-threshold check at row generation time. It lacks turn-age awareness, usage/reference tracking, context-wide capacity bounds, pattern-based tool matching, and an HTTP API surface for external control or policy updates.

This research document analyzes state-of-the-art context compaction and tool output eviction heuristics, evaluates consolette's current codebase, and details the feature design required to deliver turn-decay, reference-eviction, capacity/LRU bounds, and HTTP management endpoints in `memory-pruning`.

---

## 2. Existing Baseline Analysis (`src/claude_code_session/`)

### 2.1 Current `prune.rs` Implementation
In `src/claude_code_session/prune.rs`, pruning is triggered via `prune_tool_row`:
* **Flat Thresholds**:
  * `DEFAULT_LIMIT_CHARS` = 1024 chars, `DEFAULT_LIMIT_WORDS` = 128 words.
  * `BASH_LIMIT_CHARS` = 1024 chars (flat char length check on `Bash` output).
  * `AGENT_OUTPUT_LIMIT_CHARS` = 4096 chars, `AGENT_OUTPUT_LIMIT_WORDS` = 512 words (for `Agent` and `TaskOutput`).
* **Evaluation Scope**: Single-row evaluation at the moment tool output is processed.
* **Limitations**:
  1. **Turn Blindness**: Does not know how many turns ago a tool result was produced. A 500-character file read from 30 turns ago is preserved forever, consuming context unnecessarily.
  2. **Reference Blindness**: Cannot determine if the assistant ever referenced, parsed, or used the tool output in subsequent turns.
  3. **No Capacity Control**: If an agent runs 50 `Read` operations that are each 900 characters long, all 50 remain in context (~45k characters / ~11k tokens) because none individually exceed 1024 characters.
  4. **Static Rules**: Thresholds are hardcoded constants; no runtime policy adjustment or glob pattern matching.

### 2.2 Current `omission_cache.rs` Infrastructure
* **Storage**: SQLite database (`omission_cache.sqlite`) schema storing `(session_id, content_id, content, tool_name, created_at)`.
* **Key Format**: Monotonic content IDs per session (`omitted-001`, `omitted-002`).
* **Retrieval**: Served over stdio via `read_omitted_content` MCP tool (`src/claude_code_session/mcp_server.rs`).
* **Placeholder Format**: `[pruned: see read_omitted_content(session_id, "content_id")]`.
* **Strength**: Reversible, isolated across sessions, robust SQLite transactional storage with directory permission hardening (`0700`).

---

## 3. Comparative Analysis of Agent Context Pruning Heuristics

### 3.1 Magic-Compact (`prune.ts`) Pattern & Special Cases
Analysis of `magic-compact` (`packages/claude-code-plugin/src/prune.ts`):
* **Completed vs. Errored Filtering**:
  * Tool calls that returned an error (`is_error === true`) are preserved verbatim in `prune.ts` so the model retains full diagnostic context for debugging.
  * Only successful/completed tool outputs are eligible for heavy pruning.
* **Tool-Specific Semantics**:
  * `Skill` tool outputs: Discarded completely without caching (`"Skill output omitted due to compaction operation..."`).
  * `Read` / `NotebookEdit` outputs: Cached and omitted unconditionally during compaction regardless of character count.
  * `AskUserQuestion`: Inputs and outputs are strictly preserved (never omitted) to keep user interaction history intact.

### 3.2 Turn-Decay & Age-Off Strategies
Turn-decay strategies adjust the pruning threshold as a function of turn age ($A = T_{\text{current}} - T_{\text{tool}}$):

$$\text{Threshold}(A) = \max\left(\text{MinThreshold}, \text{BaseThreshold} \times \gamma^A\right)$$

* **Fixed Turn Window (Hard Decay)**: Tool outputs older than $N$ turns (e.g. $N = 5$) are automatically pruned or omitted unless flagged as critical.
* **Dynamic Decay (Soft Decay)**:
  * Turns $0 \dots 2$ (Immediate Context): Keep tool results up to high threshold (e.g., 4096 chars).
  * Turns $3 \dots 5$ (Recent Context): Reduce threshold to moderate limit (e.g., 512 chars).
  * Turns $> 5$ (Historical Context): Omit all non-essential tool outputs ($0$ char threshold), keeping only lightweight placeholders.

### 3.3 Unreferenced Tool Result Eviction
In multi-turn agent execution loops, many tool outputs (such as directory listings, intermediate search results, or raw build logs) are consumed immediately in the next turn and never referenced again.
* **Reference Identification**:
  * Direct Tool Call Linkage: `tool_use_id` referenced in assistant logic.
  * Content Citation: Assistant text referring to file paths, function signatures, or output tokens emitted by the tool result.
* **Eviction Policy**:
  * If a tool output in turn $T$ has not been referenced by any assistant turn up to turn $T + k$ (e.g. $k = 3$), it is classified as "unreferenced history" and evicted to `OmissionCache`.

### 3.4 Capacity Bounds & LRU / LRR Eviction
* **Total Tool Context Budget**: Establish a cumulative token/character cap for all unpruned tool results in the active transcript (e.g., max 20,000 tokens / 80,000 chars allocated to historical tool outputs).
* **LRU (Least Recently Used) Eviction**: When total tool output context exceeds the budget, prune tool outputs starting from the oldest turn ($T_{\text{tool}}$ lowest).
* **LRR (Least Recently Referenced) Eviction**: Prioritize pruning tool outputs that have zero references or the longest elapsed turns since last reference.

### 3.5 Tool Name Pattern & Glob Matching
Different tool classes require distinct retention characteristics:
* **File Reading (`Read`, `Glob`, `Grep`)**: High volume, short temporal relevance. Decays quickly after $2 \dots 3$ turns.
* **Command Execution (`Bash`, `Exec`)**: Medium volume; build/test errors must be preserved until resolved; successful test outputs decay after $3 \dots 5$ turns.
* **Subagent Output (`Agent`, `TaskOutput`)**: High context density; summarized or decay-pruned after child task completes.
* **Glob Rules**: Allow users/configs to define rules like `mcp__*` -> 512 chars max, `git_*` -> prune after 2 turns.

---

## 4. Feature Architecture & Specification for `memory-pruning`

### 4.1 Policy Configuration Model (`PruningPolicy`)
A serializable policy struct specifying multi-criteria pruning parameters:

```rust
pub struct PruningPolicy {
    /// Global enable/disable flag.
    pub enabled: bool,
    /// Default flat char threshold (fallback).
    pub default_limit_chars: usize,
    /// Maximum age in turns before a tool result is forcibly pruned.
    pub max_turn_age: Option<usize>,
    /// Number of turns without a reference before an unreferenced tool result is pruned.
    pub unreferenced_turn_decay: Option<usize>,
    /// Maximum cumulative character budget for tool outputs across the active transcript.
    pub max_tool_context_bytes: Option<usize>,
    /// Preserve tool outputs if `is_error == true`.
    pub preserve_error_outputs: bool,
    /// Tool-specific glob rules and overrides.
    pub tool_rules: Vec<ToolPruningRule>,
}

pub struct ToolPruningRule {
    /// Glob pattern matching tool name (e.g. "Bash", "Read", "mcp__*").
    pub pattern: String,
    /// Character threshold override.
    pub limit_chars: Option<usize>,
    /// Turn age override.
    pub max_turn_age: Option<usize>,
    /// Force unconditional pruning during turn decay passes.
    pub force_prune: bool,
}
```

### 4.2 Turn-Aware Pruning Engine (`src/claude_code_session/prune.rs`)
The pruning engine evaluates a transcript's active turn chain (`Vec<Turn>`):

```
+-----------------------------------------------------------------------------------+
|                            Transcript Row Sequence                                |
+-----------------------------------------------------------------------------------+
                                          |
                                          v
+-----------------------------------------------------------------------------------+
|                        1. Turn & Reference Reconstruction                         |
|  - Group rows into Turns (0 .. N-1, where N-1 is latest turn)                      |
|  - Compute turn distance: age = (N - 1) - turn_index                             |
|  - Build reference map: assistant turns referencing tool_use_ids / file paths     |
+-----------------------------------------------------------------------------------+
                                          |
                                          v
+-----------------------------------------------------------------------------------+
|                       2. Multi-Criteria Evaluation Pass                           |
|  For each tool_result row:                                                        |
|  - Skip if turn_index >= N - preserve_recent_turns (active turn protection)       |
|  - Skip if is_error && policy.preserve_error_outputs                              |
|  - Check Rule 1: Flat Size Exceeded? (size > tool_limit)                          |
|  - Check Rule 2: Turn Age Exceeded? (age > max_turn_age)                          |
|  - Check Rule 3: Unreferenced Decay? (age > decay_turns && !is_referenced)         |
+-----------------------------------------------------------------------------------+
                                          |
                                          v
+-----------------------------------------------------------------------------------+
|                       3. Capacity / LRU Enforcement Pass                          |
|  If sum(remaining_tool_output_bytes) > max_tool_context_bytes:                    |
|  - Sort remaining unpruned tool results by (last_referenced_turn, turn_index) ASC |
|  - Evict oldest/least-referenced tool results until within context budget        |
+-----------------------------------------------------------------------------------+
                                          |
                                          v
+-----------------------------------------------------------------------------------+
|                      4. OmissionCache Insertion & Rewriting                       |
|  - Insert original text into OmissionCache under (session_id, content_id)        |
|  - Replace row content with [pruned: see read_omitted_content(...)] placeholder   |
+-----------------------------------------------------------------------------------+
```

### 4.3 HTTP API Specification (`axum` Router)

To enable runtime management, policy injection, and monitoring, `consolette` will expose three HTTP endpoints under `/session`:

#### 1. `POST /session/prune`
Triggers an immediate pruning pass on a session transcript.

* **Request Body**:
```json
{
  "session_id": "94bd08fd-a105-4b01-b25d-160489a204e2",
  "dry_run": false,
  "policy_override": {
    "max_turn_age": 5,
    "unreferenced_turn_decay": 3,
    "max_tool_context_bytes": 50000
  }
}
```
* **Response (200 OK)**:
```json
{
  "session_id": "94bd08fd-a105-4b01-b25d-160489a204e2",
  "rows_evaluated": 142,
  "rows_pruned": 18,
  "bytes_freed": 124500,
  "estimated_tokens_saved": 31125,
  "pruned_by_reason": {
    "size_threshold": 6,
    "turn_age": 7,
    "unreferenced_decay": 3,
    "capacity_lru": 2
  },
  "dry_run": false
}
```

#### 2. `POST /session/policy`
Updates the active global or per-session pruning policy.

* **Request Body**:
```json
{
  "session_id": "94bd08fd-a105-4b01-b25d-160489a204e2", // optional, null for global default
  "policy": {
    "enabled": true,
    "default_limit_chars": 1024,
    "max_turn_age": 8,
    "unreferenced_turn_decay": 4,
    "max_tool_context_bytes": 80000,
    "preserve_error_outputs": true,
    "tool_rules": [
      { "pattern": "Read", "limit_chars": 512, "max_turn_age": 3, "force_prune": false },
      { "pattern": "mcp__*", "limit_chars": 1024, "max_turn_age": 5, "force_prune": false }
    ]
  }
}
```
* **Response (200 OK)**:
```json
{
  "status": "updated",
  "scope": "session",
  "session_id": "94bd08fd-a105-4b01-b25d-160489a204e2"
}
```

#### 3. `GET /session/prune/stats`
Retrieves pruning statistics and transcript memory metrics for a session.

* **Query Parameters**: `session_id=<uuid>`
* **Response (200 OK)**:
```json
{
  "session_id": "94bd08fd-a105-4b01-b25d-160489a204e2",
  "total_turns": 24,
  "total_rows": 180,
  "pruned_rows_count": 32,
  "omission_cache_entries": 32,
  "current_tool_output_bytes": 34200,
  "historical_bytes_pruned": 412000,
  "active_policy": {
    "enabled": true,
    "max_turn_age": 8,
    "unreferenced_turn_decay": 4
  }
}
```

---

## 5. Edge Cases, Hazards, & Guardrails

1. **Active Turn Protection (Trailing Turn Boundary)**:
   * *Hazard*: Pruning the tool result of the *current* or *immediately preceding* turn while the LLM is actively attempting to process it.
   * *Guardrail*: The most recent $M$ turns (default $M = 1$ or $2$) must be protected by a `preserve_recent_turns` floor, preventing turn-decay or unreferenced eviction from touching live interaction turns.

2. **Reference Misidentification**:
   * *Hazard*: Evicting a tool output as "unreferenced" when the assistant actually used its findings implicitly (without quoting `tool_use_id`).
   * *Guardrail*: Combine `unreferenced_turn_decay` with a minimum age threshold (e.g. only evict unreferenced tool outputs after at least 3 turns have elapsed). Never evict in turn 1 after tool execution.

3. **Transcript Schema Integrity & JSONL Round-tripping**:
   * *Hazard*: Pruning row content in a way that breaks Claude Code's expected JSON format, causing session loading or `/resume` failure.
   * *Guardrail*: Preserve all top-level row fields (`uuid`, `parentUuid`, `type`, `message.role`), modifying *only* the inner string or block of `message.content` with standardized `[pruned: see read_omitted_content(session_id, "content_id")]` placeholders.

4. **Concurrent Access & Mutability**:
   * *Hazard*: Running a pruning pass while Claude Code or an HTTP proxy is appending new rows to the session file.
   * *Guardrail*: Perform snapshot parsing (`parse_session_file`), execute in-memory pruning pass, and write via atomic file replacement (temp file write + rename) or in-memory state update.

5. **Dry-Run Mode**:
   * *Hazard*: User wants to test aggressive policy settings without corrupting or permanently altering active transcript memory.
   * *Guardrail*: Support `dry_run: true` in `POST /session/prune`, returning exact calculation of rows that *would* be pruned, bytes freed, and token savings without writing to disk or `OmissionCache`.

---

## 6. Key Recommendations for Implementation Phase

1. **Policy Hierarchy**: Implement a clean default policy fallback (`DefaultPruningPolicy`) that can be overridden per session via `POST /session/policy` or CLI flags.
2. **Reuse Existing Primitives**:
   * Leverage `src/claude_code_session/omission_cache.rs` for reversible storage.
   * Extend `src/claude_code_session/transcript.rs` `build_turns` for turn distance and reference map construction.
   * Enhance `src/claude_code_session/prune.rs` with `PruningPolicy` multi-criteria logic.
3. **HTTP Server Integration**: Add `/session/prune`, `/session/policy`, and `/session/prune/stats` routes to consolette's `axum` HTTP server (or `mcp-proxy` server).
4. **Validation Requirements**:
   * Unit tests for turn decay, unreferenced eviction, and LRU capacity bounds.
   * Integration tests verifying HTTP API endpoints, dry-run responses, and `OmissionCache` round-tripping.
