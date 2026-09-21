# ADR-001: Multi-Criteria Pruning Policy Data Model with Turn Age Decay, Unreferenced Eviction, Glob Matching, and Capacity LRU Context Budgeting

**Status**: Accepted  
**Date**: 2026-09-18  
**Relates to**: `project_plans/memory-pruning/requirements.md` (Scope & Policy Requirements); `project_plans/memory-pruning/research/architecture.md`; `project_plans/memory-pruning/research/features.md`

---

## Context

Consolette's existing transcript pruning implementation in [`src/claude_code_session/prune.rs`](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/claude_code_session/prune.rs) relies on hardcoded, binary threshold checks (`DEFAULT_LIMIT_CHARS`, `BASH_LIMIT_CHARS`, `AGENT_OUTPUT_LIMIT_CHARS`) applied at single-row generation time. 

As long-running Claude Code sessions progress across dozens or hundreds of turns, this baseline reveals major context management limitations:
1. **Turn Blindness**: Historical tool outputs (such as file reads or build outputs from 20 turns prior) remain unpruned indefinitely if they fall below the flat character threshold, unnecessarily inflating context window token counts, TTFT (time-to-first-token) prefill latency, and API cost.
2. **Reference Blindness**: The system cannot distinguish between a tool output actively referenced or utilized in subsequent assistant turns and one that was consumed once and never cited again.
3. **Capacity Blindness**: Cumulative tool execution outputs across a transcript can aggregate tens of thousands of tokens without triggering any single-row character limit.
4. **Lack of Runtime Configuration**: Thresholds are fixed compile-time constants without support for tool-name pattern matching, per-session policy overrides, or dynamic API updates.

A comprehensive multi-criteria pruning policy and data model is required to govern turn age decay, unreferenced tool output eviction, pattern-based tool matching, and total transcript tool context capacity bounds.

---

## Decision

We establish a multi-criteria pruning policy data model (`PruningPolicy`) and thread-safe policy store (`PruningPolicyStore`) in `src/claude_code_session/prune_policy.rs`.

### 1. `PruningPolicy` & `ToolPruningRule` Data Structures

```rust
use serde::{Deserialize, Serialize};

/// Multi-criteria context pruning policy configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PruningPolicy {
    /// Master toggle for turn-based pruning.
    pub enabled: bool,
    /// Default flat character threshold (fallback when no pattern matches).
    pub default_limit_chars: usize,
    /// Default flat word threshold (fallback).
    pub default_limit_words: usize,
    /// Maximum turn age before a tool output is unconditionally pruned (e.g., 8 turns).
    pub max_turn_age: Option<usize>,
    /// Turns without an assistant reference before unreferenced output is evicted (e.g., 4 turns).
    pub unreferenced_turn_decay: Option<usize>,
    /// Maximum cumulative bytes allocated to historical unpruned tool outputs in transcript.
    pub max_tool_context_bytes: Option<usize>,
    /// Number of trailing recent turns strictly protected from age/decay pruning (default: 2).
    pub preserve_recent_turns: usize,
    /// Preserve tool execution results where is_error == true.
    pub preserve_error_outputs: bool,
    /// Tool-name specific glob rules and overrides.
    pub tool_rules: Vec<ToolPruningRule>,
}

/// Tool-specific matching rule supporting glob patterns.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolPruningRule {
    /// Glob pattern matching tool name (e.g., "Bash", "Read", "Agent", "mcp__*").
    pub pattern: String,
    /// Character threshold override for this tool class.
    pub limit_chars: Option<usize>,
    /// Maximum turn age override for this tool class.
    pub max_turn_age: Option<usize>,
    /// Unreferenced turn decay override for this tool class.
    pub unreferenced_turn_decay: Option<usize>,
    /// Force pruning regardless of size once age threshold is met.
    pub force_prune: bool,
}
```

### 2. Multi-Criteria Evaluation Pipeline

During a pruning pass over a transcript, each tool output row is evaluated through a strict multi-stage rule sequence:

1. **Active Recent Turn Protection**: Tool results in turns where $\text{turn\_index} \ge N - \text{preserve\_recent\_turns}$ are protected verbatim. Age decay and unreferenced eviction NEVER prune active trailing turns (default: 2 turns).
2. **Diagnostic Error Preservation**: If `is_error == true` and `preserve_error_outputs == true`, the tool result is preserved to retain failure diagnostics for assistant self-correction.
3. **Pattern Rule Resolution**: Tool names are matched against `tool_rules` using `glob::Pattern`. The first matching rule overrides default thresholds (`limit_chars`, `max_turn_age`, `unreferenced_turn_decay`).
4. **Flat Size Threshold**: If $\text{char\_len} > \text{limit\_chars}$ or $\text{word\_count} > \text{limit\_words}$, the output is marked for pruning (`Reason::SizeThreshold`).
5. **Turn Age Decay**: If $\text{turn\_age} > \text{max\_turn\_age}$, the output is marked for pruning (`Reason::TurnAge`).
6. **Unreferenced Turn Decay**: If $\text{turn\_age} > \text{unreferenced\_turn\_decay}$ and the tool result's `tool_use_id` or output target has not been referenced by any assistant turn, it is marked for pruning (`Reason::UnreferencedDecay`). To prevent premature context eviction, `unreferenced_turn_decay` enforces a conservative minimum turn floor ($\ge 3$ turns).

### 3. Capacity & LRU Eviction Pass

Following multi-criteria filtering:
1. Compute $\text{TotalToolBytes} = \sum \text{len}(\text{tool\_result\_text})$ for all unpruned tool results remaining in the active transcript.
2. If `max_tool_context_bytes` is configured and $\text{TotalToolBytes} > \text{max\_tool\_context\_bytes}$:
   - Sort candidate tool results outside the `preserve_recent_turns` window by `(is_referenced, last_referenced_turn_index, turn_index)` ascending.
   - Evict candidates sequentially into [`OmissionCache`](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/claude_code_session/omission_cache.rs), incrementing `Reason::CapacityLru`, until $\text{TotalToolBytes} \le \text{max\_tool\_context\_bytes}$.
   - **Precedence Rule**: `preserve_recent_turns` takes precedence over LRU eviction. If protected recent turns alone exceed `max_tool_context_bytes`, emit a `tracing::warn!` metric without forcefully pruning recent active turns.

### 4. Thread-Safe `PruningPolicyStore`

We implement a concurrent, in-memory policy manager allowing global defaults and per-session overrides:

```rust
use std::collections::HashMap;
use std::sync::RwLock;

pub struct PruningPolicyStore {
    global_policy: RwLock<PruningPolicy>,
    session_overrides: RwLock<HashMap<String, PruningPolicy>>,
}
```

---

## Alternatives Considered

| Option | Reason for Rejection |
| :--- | :--- |
| **Fixed Sliding Window (Last N Messages)** | Rejection: Blindly drops system prompts, initial instructions, and active task definitions; lacks tool-specific sensitivity. |
| **Full LLM Summarization per Turn** | Rejection: Introduces high per-turn latency overhead, API token expense, and risk of hallucinated information loss. |
| **Flat Character Limits Only (Status Quo)** | Rejection: Turn-blind and reference-blind; allows historical tool context to accumulate indefinitely until context window exhaustion. |

---

## Consequences

### Positive
- Provides fine-grained, policy-driven control over transcript context memory footprint.
- Automatically reclaims context tokens from stale, unreferenced tool outputs while preserving critical recent turns and diagnostic error traces.
- Supports flexible tool-name wildcard rules (`mcp__*`, `Bash`, `Agent`) matching diverse execution workflows.
- Thread-safe `PruningPolicyStore` enables dynamic runtime policy updates without server restarts.

### Negative / Tradeoffs
- Multi-criteria evaluation requires an initial turn-reconstruction and reference-analysis pass over the transcript prior to pruning.
- Policy complexity requires thorough test coverage to validate interaction between turn decay, unreferenced eviction, and capacity LRU limits.
