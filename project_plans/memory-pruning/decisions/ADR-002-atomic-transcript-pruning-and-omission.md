# ADR-002: Safe Row-Mapping Transcript Mutation Pipeline Preserving Live File Appends and OmissionCache SQLite Transaction Safety

**Status**: Accepted  
**Date**: 2026-09-18  
**Relates to**: `project_plans/memory-pruning/requirements.md` (Constraints & Risk Controls); `project_plans/memory-pruning/research/pitfalls.md` (Critical Hazards & Concurrency Races); `project_plans/compaction-hook/decisions/ADR-009-omission-cache-and-mcp-session-scoping.md`

---

## Context

Modifying active Claude Code session transcripts on disk while a live agent session is running introduces severe concurrency, structural data integrity, and security risks flagged in `research/pitfalls.md`:

1. **Live Transcript Overwrite Race**: Claude Code CLI appends JSON lines to `~/.claude/projects/<project>/<session-id>.jsonl` continuously during execution. Reading a session file into memory, evaluating pruning, and writing back the full transcript without synchronization will overwrite and permanently erase any rows appended by the CLI during the pruning pass.
2. **Disconnected Parent Link Erasure (`chain_coverage < 1.0`)**: [`build_turns()`](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/claude_code_session/transcript.rs) reconstructs logical turns by walking `parent_uuid` links backward from the active tail row. When sessions undergo `--clear` or `--resume` after a crash, broken or missing parent links cause `chain_coverage` ratio to drop below 1.0. If transcript rewriting iterates solely over `Vec<Turn>`, all historical rows prior to the broken link are omitted from output, destroying session history.
3. **SQLite Transaction PK Collisions**: Concurrent pruning passes (e.g. CLI compaction vs. axum HTTP daemon) running [`OmissionCache::insert`](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/claude_code_session/omission_cache.rs) under default `DEFERRED` SQLite transactions race on `SELECT COUNT(*)` count computation, resulting in `PRIMARY KEY` constraint failures on `(session_id, content_id)`.
4. **Placeholder Double-Pruning & Schema Corruption**: Re-pruning an already-pruned transcript can treat placeholder text `[pruned: see read_omitted_content(...)]` as raw tool output, creating nested placeholders (`[pruned: see ...]` pointing to cached placeholder text). Furthermore, mutating JSON block schemas or restamping `sessionId` during in-place pruning breaks Claude Code `--resume` session restoration.

---

## Decision

We define an atomic, row-mapping transcript mutation pipeline and harden `OmissionCache` SQLite transaction handling.

### 1. Line-Preserving Row-Mapping Architecture

Transcript mutation MUST NOT reconstruct files by outputting `Vec<Turn>`. Instead, pruning operates as a 1:1 row-mapping pass over the complete `Vec<TranscriptRow>` parsed from the JSONL file:

```
+-------------------------------------------------------------------------------+
|                       Complete Vec<TranscriptRow>                             |
|  [Row 0: User] [Row 1: ToolResult] ... [Row K: Unlinked] ... [Row N: Tail]   |
+-------------------------------------------------------------------------------+
                                        |
                                        v
+-------------------------------------------------------------------------------+
|                      Turn & Reference Index Mapping                           |
|  - build_turns() maps active chain rows to (turn_index, turn_age)             |
|  - Unlinked rows (pre-break) and sidechain rows are identified                |
+-------------------------------------------------------------------------------+
                                        |
                                        v
+-------------------------------------------------------------------------------+
|                       1:1 Row Mapping Pass                                    |
|  - Active chain tool_result rows -> Evaluated by Pruning Engine               |
|  - Unlinked rows & metadata rows -> Passed through VERBATIM                   |
|  - Sidechain rows -> Evaluated by tool rule patterns (e.g. Agent/TaskOutput)  |
+-------------------------------------------------------------------------------+
                                        |
                                        v
+-------------------------------------------------------------------------------+
|                      Rewritten Complete Transcript File                       |
|  (Zero row omission, 100% preservation of disconnected pre-break history)    |
+-------------------------------------------------------------------------------+
```

### 2. Live File Protection & Atomic Writes

For in-place pruning of active session files on disk:
1. **Exclusive File Locking (`flock`)**: Acquire an exclusive file lock on the target `.jsonl` session file before reading.
2. **EOF Line Integrity Verification**: Read the file using line streaming (`BufReader::lines`). Inspect the trailing line at EOF: if the trailing line lacks a terminating newline `\n` or fails JSON deserialization due to an in-flight write, abort the pruning pass immediately without mutating the file on disk.
3. **Atomic Replacement**: Write pruned rows to a temporary file (`tempfile::NamedTempFile`) created within the *same target directory*. Upon completion, call `persist()` or atomic `rename` to swap the file atomically, ensuring readers never observe partial writes.
4. **Identity & `sessionId` Preservation**: In-place pruning MUST preserve all existing `sessionId`, `uuid`, and `parentUuid` values verbatim. `restamp_session_id` must ONLY run during full compaction passes that generate a distinct new session file.

### 3. `OmissionCache` Transaction Safety & Permissions

We update [`OmissionCache::insert`](file:///home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette/src/claude_code_session/omission_cache.rs):
1. **Immediate Write Transactions**: Begin insertion transactions using `TransactionBehavior::Immediate` (`conn.transaction_with_behavior(...)`). This acquires an immediate write lock on SQLite at query start, preventing concurrent count computation races across separate process handles.
2. **Collision Fallback Loop**: If a `PRIMARY KEY` collision occurs on `(session_id, content_id)`, retry count generation with a monotonic suffix increment loop (`omitted-001_1`) rather than failing the transaction.
3. **Permission Hardening**: Enforce `0600` permissions on `omission-cache.sqlite` and `0700` on parent directories using Unix `PermissionsExt`.

### 4. Idempotency & Schema Preservation

1. **Placeholder Short-Circuit**: Before evaluating a `tool_result` content block, inspect its string value. If `content.starts_with("[pruned: see read_omitted_content")`, `prune_tool_row` immediately returns `PrunedRow::Unchanged`. This eliminates nested placeholder generation during repeated pruning passes.
2. **Block Schema Integrity**: Preserve the enclosing `message.content` block array structure, keeping `type: "tool_result"`, `tool_use_id`, and `is_error` intact while replacing only the inner text payload with the standardized placeholder.

---

## Alternatives Considered

| Option | Reason for Rejection |
| :--- | :--- |
| **Rebuilding File from `Vec<Turn>` Only** | Rejection: Destroys pre-break transcript turns when `chain_coverage < 1.0` (from `--clear` / `--resume`), causing permanent data loss. |
| **Direct In-Place File Truncation/Overwrite** | Rejection: Vulnerable to append race conditions with Claude Code CLI, overwriting live turns appended during the pruning pass. |
| **Default `DEFERRED` SQLite Transactions** | Rejection: Causes multi-process write collisions and `SQLITE_BUSY` errors when CLI compaction and HTTP daemon run concurrently. |

---

## Consequences

### Positive
- Guarantees 100% transcript structural integrity and zero historical turn erasure, even across broken parent UUID chains.
- Safe for execution against live active session files without risking append data loss.
- Prevents SQLite primary key collisions in multi-process deployments.
- Maintains strict 100% backward compatibility with Claude Code `--resume` session restoration.

### Negative / Tradeoffs
- Requires file lock acquisition and temporary file allocation on disk during in-place pruning passes.
- Idempotency checks add string prefix verification overhead to `prune_tool_row` (negligible performance impact).
