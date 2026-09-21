# Stack Research: Memory Pruning

**Project**: `memory-pruning`  
**Date**: 2026-09-18  
**Target Architecture**: Turn-based transcript memory pruning for Claude Code sessions in Rust (`consolette`)

---

## 1. Language & Compiler Standards

- **Rust Edition**: 2021 Edition (configured in [Cargo.toml](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/Cargo.toml)).
- **Clippy Lints**: Strict crate-level lints configured in `Cargo.toml` (`lints.clippy`):
  - `all = "warn"`
  - `pedantic = "warn"`
  - `unwrap_used = "warn"`
  - `expect_used = "warn"`
  - Production code MUST strictly adhere to pedantic lints and eliminate unhandled `.unwrap()` / `.expect()` calls.
- **Async Runtime**: `tokio` (v1.x with `full` features) for async I/O, task execution, and HTTP server routing.

---

## 2. Crate Dependencies & Technical Choices

| Dependency | Version in `Cargo.toml` | Purpose in `memory-pruning` | Selection Rationale |
| :--- | :--- | :--- | :--- |
| `glob` | `0.3` | Tool name pattern matching (`Bash*`, `mcp__*`, `Agent*`) | Fast, lightweight wildcard pattern matching; already in `Cargo.toml`. |
| `regex` | `1` | Complex tool output/name regex filter evaluation | Standard Rust regex engine for fine-grained pattern filters; already in `Cargo.toml`. |
| `serde` & `serde_json` | `1` | JSON serialization/deserialization of transcript JSONL rows, policy configs, and HTTP payloads | Foundation of `TranscriptRow` parsing and API payloads. |
| `chrono` | `0.4` | Timestamp management (RFC 3339), age calculation, and metrics timestamps | Used across `OmissionCache` and `src/memory/mod.rs`. |
| `axum` & `axum-extra` | `0.8` / `0.10` | HTTP routing for `/session/prune`, `/session/policy`, and `/session/prune/stats` | Main web framework used in `src/entrypoint/mod.rs` and `src/memory/mod.rs`. |
| `rusqlite` | `0.40` (bundled) | Storage for `OmissionCache` storing omitted tool outputs | Transactional, WAL-mode SQLite storage backing `omitted_content`. |
| `dashmap` / `tokio::sync::RwLock` | `6` / `1` | Thread-safe session policy state and metrics aggregation | High-concurrency state management across axum request handlers. |
| `tracing` | `0.1` | Structured logging and pruning telemetry | Standard logging framework in consolette (`info!`, `debug!`, `warn!`). |

---

## 3. Pattern Matching (Glob vs. Regex)

Turn-based pruning policies require matching tool outputs by tool name (e.g., matching `Bash`, `mcp__*`, `Agent`, `TaskOutput`) or output patterns.

- **Glob Matching (`glob::Pattern`)**:
  - **Syntax**: `glob::Pattern::new("mcp__*")` or `glob::Pattern::new("Bash*")`.
  - **Performance**: Linear scan over tool name strings with $O(N)$ complexity and minimal memory footprint.
  - **Use Case**: User-facing policy configurations where glob patterns like `*` and `?` provide intuitive filtering.
  - **Availability**: Provided by `glob = "0.3"` in `Cargo.toml`.

- **Regex Matching (`regex::Regex`)**:
  - **Syntax**: `regex::Regex::new(pattern)`.
  - **Performance**: Pre-compiled linear-time DFA execution. Pre-compiled regex instances cached in policy structs avoid runtime compilation overhead.
  - **Use Case**: Advanced tool output regex filters (e.g., filtering specific log output patterns or error streams).

**Decision**: Standardize on `glob::Pattern` for tool name matching rules, with optional pre-compiled `regex::Regex` for advanced content pattern matching.

---

## 4. Turn Calculation & Timestamp Data Structures

### 4.1. Turn Model & Sequence Indexing
From [src/claude_code_session/transcript.rs](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/claude_code_session/transcript.rs):
```rust
pub struct Turn {
    pub user_row: TranscriptRow,
    pub assistant_rows: Vec<TranscriptRow>,
    pub tool_rows: Vec<TranscriptRow>,
}
```
- **Active Turn Index ($T_{active}$)**: For a session with $N$ reconstructed turns, the turn sequence is indexed $0 \dots N-1$, where $T_{active} = N - 1$.
- **Turn Age / Distance ($D$)**: For a tool result generated in turn $T_{tool}$, its turn distance from active execution is:
  \[
  D = T_{active} - T_{tool} = (N - 1) - T_{tool}
  \]
- **Unreferenced Turn Count ($U$)**: Number of turns elapsed since any assistant row last referenced the tool result's `tool_use_id` or output string.

### 4.2. Policy Data Structure
```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PruningPolicy {
    pub max_unreferenced_turns: Option<usize>,
    pub max_turn_age: Option<usize>,
    pub min_char_threshold: usize,
    pub min_word_threshold: usize,
    pub max_retained_bytes: Option<usize>,
    pub tool_patterns: Vec<String>,
}
```

---

## 5. Axum HTTP Routing & Integration Architecture

### 5.1. Route Specification
Endpoints will be integrated into the axum router in [src/entrypoint/mod.rs](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/entrypoint/mod.rs) or a dedicated sub-router:

- `POST /session/prune` (or `/api/sessions/{id}/prune`): Triggers a manual or policy-driven pruning pass on active session transcripts.
  - Supports query parameter `dry_run=true` to preview pruned rows without mutating transcript state.
- `POST /session/policy` (or `/api/sessions/{id}/policy`): Queries or updates current pruning policies for active sessions.
- `GET /session/prune/stats` (or `/api/sessions/{id}/prune/stats`): Returns metrics on rows checked, rows pruned, bytes freed, and turn distance distributions.

### 5.2. Axum State Wiring
Following the `MemoryAppState` model in [src/memory/mod.rs](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/memory/mod.rs):
```rust
pub struct SessionPruningState {
    pub omission_cache: std::sync::Arc<crate::claude_code_session::omission_cache::OmissionCache>,
    pub policies: dashmap::DashMap<String, PruningPolicy>,
    pub stats: std::sync::Arc<tokio::sync::RwLock<PruningStats>>,
}
```

---

## 6. Existing Module Integration & Codebase Synergy

1. **[src/claude_code_session/prune.rs](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/claude_code_session/prune.rs)**:
   - Provides binary tool row pruning (`prune_tool_row`), threshold comparisons (`DEFAULT_LIMIT_CHARS`, `BASH_LIMIT_CHARS`, `AGENT_OUTPUT_LIMIT_CHARS`), and placeholder generation (`[pruned: see read_omitted_content(session_id, "{content_id}")]`).
   - Will be extended to support turn distance calculations, policy matching, and batch transcript pruning.

2. **[src/claude_code_session/transcript.rs](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/claude_code_session/transcript.rs)**:
   - Provides `parse_session_file`, `build_turns`, `TranscriptRow`, and `Turn` reconstruction logic.
   - Provides exact turn boundary awareness necessary to calculate $T_{active} - T_{tool}$.

3. **[src/claude_code_session/omission_cache.rs](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/claude_code_session/omission_cache.rs)**:
   - Stores raw unredacted tool output in SQLite WAL file (`~/.claude/consolette/omission-cache.sqlite`).
   - Provides scoped retrieval by `(session_id, content_id)`.

4. **[src/memory/](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/memory/)**:
   - Cross-agent shared memory KV store (`MemoryStore`, `DedupState`).
   - Offers architectural reference for axum handlers, TTL calculations, and thread-safe state management.
