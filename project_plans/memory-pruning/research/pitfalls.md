# Pitfalls & Failure Modes Research: memory-pruning

**Date**: 2026-09-18  
**Scope**: Technical hazard analysis, concurrency race conditions, turn counting ambiguities, reference-tracking edge cases, JSON schema breaking risks, and security traps for the `memory-pruning` feature in `consolette`.  
**Target Codebase**: `src/claude_code_session/` (`prune.rs`, `transcript.rs`, `omission_cache.rs`, `writer.rs`, `boundary.rs`, `mod.rs`) and `src/bin/mcp-proxy/` / `src/cost_metrics/server.rs`.

---

## 1. Executive Summary & Risk Matrix

The `memory-pruning` feature expands consolette's transcript management from a single-row flat-character check (`src/claude_code_session/prune.rs`) into a turn-aware, multi-criteria pruning engine with turn-decay policies, reference tracking, context budget (capacity/LRU) enforcement, and HTTP control endpoints (`POST /session/prune`, `POST /session/policy`, `GET /session/prune/stats`).

While this capability dramatically reduces LLM context window consumption and token cost, modifying active session transcripts introduces severe structural, concurrency, data integrity, and security hazards.

### Summary Hazard Matrix

| Hazard | Severity | Primary Module | Impact |
| :--- | :--- | :--- | :--- |
| **Active Transcript Append Race** | **CRITICAL** | `transcript.rs` / `writer.rs` | Pruning a live session file while Claude Code CLI appends new rows causes **permanent loss of recent conversation turns**. |
| **Disconnected Chain Data Erasure** | **CRITICAL** | `transcript.rs` / `mod.rs` | Reconstructing transcripts solely from `build_turns()` drops turns prior to dangling parent links (`chain_coverage < 1.0`), **erasing historical session turns**. |
| **Reference Tracking False Negatives** | **HIGH** | `prune.rs` | Checking only `tool_use_id` in assistant text marks 99% of tool outputs as "unreferenced", causing **premature eviction of active context**. |
| **`tool_result` Schema Corruption** | **HIGH** | `prune.rs` / `writer.rs` | Substituting inner `content` blocks with raw strings instead of preserving expected block types or error states breaks **Claude Code CLI session restoration (`--resume`)**. |
| **Multi-Process SQLite PK Collision** | **HIGH** | `omission_cache.rs` | Concurrent pruning passes across separate SQLite connection handles collide on `(session_id, content_id)` generation under `DEFERRED` transactions. |
| **Unauthenticated HTTP Path Traversal** | **HIGH** | Axum Router | Arbitrary `session_id` input in `POST /session/prune` can allow **unauthorized file read/write across project directories**. |

---

## 2. Concurrency & Race Conditions

### 2.1 Live Transcript Append vs. Pruning Pass (Data Loss Hazard)

Claude Code CLI appends JSON lines to `~/.claude/projects/<project>/<session-id>.jsonl` continuously during execution. When an HTTP request (`POST /session/prune`) or background job triggers a pruning pass on an active session:

1. **The Overwrite Race (Lost Appends)**:
   - `parse_session_file` reads the `.jsonl` file into memory (e.g. 100 rows).
   - During the 5–10ms pruning pass, Claude Code CLI streams 2 new rows (`user` query and `assistant` response start) to the file on disk.
   - The pruning pass finishes and writes back the pruned transcript via atomic file replacement (`tempfile` + `rename`).
   - **Result**: The 2 newly appended rows on disk are overwritten and permanently lost. The active session state in Claude Code CLI becomes corrupted or out of sync with disk.

2. **Partial Line Read at EOF**:
   - `parse_session_file` (`transcript.rs:164-189`) uses `BufReader::lines()` to stream rows.
   - If a line is mid-write when `parse_session_file` reads EOF, `serde_json::from_str::<TranscriptRow>` fails.
   - `parse_session_file` logs `tracing::warn!("skipping unparseable transcript row")` and continues (line 179).
   - **Result**: A valid turn that was in the process of being flushed is treated as unparseable, skipped, and deleted when the pruned transcript is rewritten.

#### Guardrail Requirements:
* **File Locking / Append-Only In-Place Mutation**: Live session files must NEVER be blindly overwritten from an out-of-date in-memory snapshot. An exclusive file lock (`flock`) or atomic append verification (verifying file length and read offset before replacing) must be enforced.
* **Partial Line Preservation**: `parse_session_file` must distinguish between bad JSON in historical lines vs. incomplete writes at EOF. If the trailing line lacks a newline or fails parsing at EOF, pruning MUST abort rather than skip and drop the line.

### 2.2 `OmissionCache` SQLite Concurrency & Locking

`OmissionCache` (`omission_cache.rs`) manages pruned tool I/O in `omission-cache.sqlite`.

```rust
// omission_cache.rs:120-143
pub fn insert(&self, session_id: &str, tool_name: &str, content: &str) -> Result<String> {
    let mut conn = self.conn.lock().unwrap();
    let tx = conn.transaction()?;
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM omitted_content WHERE session_id = ?1",
        params![session_id], |row| row.get(0),
    )?;
    let content_id = format!("omitted-{:03}", count + 1);
    ...
```

1. **Single-Process Internal Thread Safety**:
   - Within a single process, `self.conn` is protected by `Mutex<Connection>`. Serialized insertion within one process is thread-safe.
2. **Multi-Process / CLI vs. Daemon Race Condition**:
   - If a CLI tool (e.g. `consolette compact`) and the Axum HTTP daemon run concurrently, two separate `OmissionCache` instances hold open handles to the same SQLite database.
   - SQLite WAL mode permits concurrent readers, but `conn.transaction()` defaults to `DEFERRED` isolation.
   - Process A reads `count = 5` and computes `content_id = "omitted-006"`.
   - Process B reads `count = 5` concurrently and computes `content_id = "omitted-006"`.
   - Process A commits `omitted-006`.
   - Process B attempts to insert `omitted-006` and fails with `rusqlite::Error::SqliteFailure` (PRIMARY KEY constraint violation on `(session_id, content_id)`).

#### Guardrail Requirements:
* **`IMMEDIATE` / `EXCLUSIVE` Transactions**: `OmissionCache::insert` must start transactions with `conn.transaction_with_behavior(TransactionBehavior::Immediate)` to acquire a write lock at query start, preventing concurrent count computation races.
* **UUID-based or Monotonic Content IDs with Collision Fallback**: If a primary key collision occurs, retry with a fallback loop or use UUID/timestamp suffix rather than returning a fatal error.

---

## 3. Turn Counting Ambiguity & Structural Edge Cases

### 3.1 `build_turns()` vs. Disconnected Transcript Chains

`transcript.rs::build_turns` parses transcript rows into logical `Turn` objects by walking `parent_uuid` links backward from the **last non-sidechain row** (`transcript.rs:256-295`).

```
+------------------+     +------------------+           +------------------+     +------------------+
| Turn 1 (u1/a1)   | --> | Turn 2 (u2/a2)   |  (BROKEN) | Turn 3 (u3/a3)   | --> | Turn 4 (u4/a4)   |
| Disconnected Root|     | (Dangling Link)  |     X     | Resume Session   |     | Active Tail      |
+------------------+     +------------------+           +------------------+     +------------------+
                                                        ^
                                                        |
                                            build_turns starts here!
```

1. **The Disconnected Chain Trap**:
   - When a session undergoes `--clear` or `--resume` after a crash, Claude Code creates a new root row whose `parentUuid` is `null` or points to a non-existent UUID.
   - `build_turns` stops walking backward at the missing parent link (`transcript.rs:280-288`).
   - `chain_coverage(rows, turns)` (`transcript.rs:403-460`) explicitly measures this: `chain_coverage.ratio()` can be $< 1.0$ (e.g., $0.50$ when half the messages are before the break).
2. **Data Erasure Risk**:
   - If the pruning engine reconstructs a pruned transcript by iterating ONLY over `Vec<Turn>` returned by `build_turns()`, **all turns before the broken link will be omitted from the output file, destroying history!**

#### Guardrail Requirements:
* **Line-Preserving Pass Model**: Pruning MUST operate as a row-by-row mapping or in-place modification over the complete `Vec<TranscriptRow>`, using `build_turns` ONLY to compute turn indices for active chain rows. Unlinked rows must pass through un-modified.

### 3.2 Sidechain & Subagent Turn Handling

In Claude Code transcripts, subagent and sidechain executions mark rows with `"isSidechain": true`.

1. **Exclusion in `build_turns()`**:
   - `build_turns` explicitly filters out sidechain rows (`transcript.rs:251-253`).
   - Sidechain tool results are NOT attached to standard user/assistant `Turn` structures.
2. **The Unpruned Sidechain Leak**:
   - Subagents (e.g. `Agent` tool execution) often generate massive tool results (e.g., 50KB file reads inside subagents).
   - If pruning logic relies on turn age $A = (N - 1) - \text{turn\_index}$, sidechain tool results will have no turn index and remain unpruned indefinitely, leaking tokens into the transcript.

#### Guardrail Requirements:
* Sidechain tool result rows must be assigned the turn index of their parent assistant row on the main chain, or evaluated against tool-name pattern rules (`Agent`, `TaskOutput`) independently of main chain turn age.

### 3.3 Dynamic Turn Age & Relative Distance Drift

Turn age is defined as:

$$A_i = (N - 1) - i$$

where $N$ is the total number of turns on the active chain, and $i$ is the 0-indexed turn position.

1. **Age Drift**:
   - At Turn 5 ($N=6$), Turn 1 has age $A = (6 - 1) - 1 = 4$. If `max_turn_age = 5`, Turn 1 is NOT pruned.
   - At Turn 7 ($N=8$), Turn 1 now has age $A = (8 - 1) - 1 = 6$. Turn 1 now exceeds `max_turn_age` and IS pruned.
2. **Turn Boundary Indexing**:
   - Turn indices must be calculated backwards starting from the *most recent assistant turn*, NOT the user turn. If a user turn has been sent but the assistant has not yet responded (mid-execution turn), $N$ must reflect the last completed assistant response to prevent premature turn age increment.

---

## 4. Tool Output Reference Tracking Edge Cases

### 4.1 Ambiguity of "Referenced" Tool Results

The `memory-pruning` requirement specifies that unreferenced tool outputs decay faster after $K$ turns (`unreferenced_turn_decay`). However, defining whether a tool output was "referenced" or "used" by an assistant is highly error-prone:

```
[Turn 1] Assistant calls Read("src/config.rs") -> tool_use_id: "toolu_01A"
[Turn 1] User/Tool returns 2000 chars of code -> tool_result
[Turn 2] Assistant says: "I examined the config file and found we need to update the port setting."
[Turn 3] Assistant calls Edit("src/config.rs", ...) -> tool_use_id: "toolu_02B"
```

1. **The `tool_use_id` Fallacy (False Negative Over-Pruning)**:
   - Tool results carry `tool_use_id: "toolu_01A"`.
   - In subsequent turns (Turn 2, Turn 3), the assistant NEVER writes `"toolu_01A"` in its natural language output text.
   - If reference tracking requires `assistant_text.contains(tool_use_id)`, 99% of all tool outputs will be flagged as **unreferenced**, triggering aggressive decay and evicting file reads the model is actively relying on!
2. **The Plain Text Match Fallacy (False Positive Under-Pruning)**:
   - If reference tracking checks if any word in the tool result appears in subsequent assistant text, common words ("fn", "struct", "error", "path") will cause almost every tool result to be marked "referenced", rendering decay useless.
3. **Explicit Citation Tracking**:
   - Valid references MUST be identified by:
     1. **Path Alignment**: Assistant tool calls in turn $J > I$ operating on the exact file path target of a `Read`/`Edit` in turn $I$.
     2. **Tool Chain Parentage**: Assistant `tool_use` blocks in turn $I$ linked directly to `tool_result` blocks in turn $I$.
     3. **Placeholder ID Lookup**: Assistant text referencing `read_omitted_content` or explicit `content_id` placeholders.

#### Guardrail Requirements:
* Default policies must NOT rely exclusively on strict string matching for reference tracking. `unreferenced_turn_decay` must operate conservatively with a minimum turn floor (e.g. $K \ge 3$) to prevent premature eviction of implicit context.

---

## 5. JSON Schema Breaking & Transcript Integrity Traps

### 5.1 `TranscriptRow` Serde & Structural Loss

`TranscriptRow` (`transcript.rs:64-132`) handles Claude Code's un-specced JSONL format using custom `Deserialize`/`Serialize` and `RowFields.extra`.

```rust
// transcript.rs:33-45
pub struct RowFields {
    pub uuid: String,
    pub parent_uuid: Option<String>,
    pub is_sidechain: bool,
    pub is_meta: bool,
    pub message: Option<Value>,
    pub extra: Map<String, Value>,
}
```

1. **`message.content` Block Structure**:
   - Claude Code tool result rows carry `message.content` as an array of content blocks:
     ```json
     {
       "type": "user",
       "uuid": "t1",
       "message": {
         "role": "user",
         "content": [
           {
             "type": "tool_result",
             "tool_use_id": "toolu_123",
             "content": "output text...",
             "is_error": false
           }
         ]
       }
     }
     ```
   - In `prune.rs:208-232`, `prune_tool_row` mutates `block["content"]` in-place:
     ```rust
     *inner = Value::String(placeholder.clone());
     ```
2. **Nested Content Array Mutation Hazard**:
   - Sometimes `tool_result.content` is an array of text objects: `[{"type": "text", "text": "..."}]`.
   - Overwriting `tool_result.content` with a bare string `Value::String("[pruned: ...]")` alters the JSON schema shape from `Array` to `String`.
   - While Claude Code CLI handles both, some strict tool deserializers in downstream extensions expect array format.
3. **Error Output Preservation Hazard (`is_error: true`)**:
   - If a tool execution failed (`"is_error": true`), pruning its stdout/stderr destroys the error trace needed for the model to self-correct.
   - **Rule**: `prune.rs` MUST preserve all tool results where `is_error == true` unless `policy.preserve_error_outputs == false`.

### 5.2 `sessionId` Restamping vs In-Place Pruning

`compact_session` (`mod.rs:165-295`) calls `writer.rs::restamp_session_id`, updating the `sessionId` field in all rows to match `out_path`'s file stem.

1. **In-Place Pruning Hazard**:
   - For live session pruning (`POST /session/prune`), the session file is pruned *in-place*.
   - If `restamp_session_id` is run with a new UUID, the transcript's internal `sessionId` fields no longer match the file stem on disk (`~/.claude/projects/.../<original-id>.jsonl`).
   - Claude Code CLI fails to resume the session (`claude --resume <original-id>`) because row session IDs mismatch.

#### Guardrail Requirements:
* In-place pruning passes MUST preserve the exact existing `sessionId` and UUID mappings. `restamp_session_id` must ONLY run during full compaction passes that generate a new session file.

### 5.3 `OmissionCache` Key Mismatch

The placeholder format generated by `prune.rs:205` is:

```
[pruned: see read_omitted_content(session_id, "omitted-001")]
```

If the `session_id` string passed to `prune_tool_row` differs by even a single character from the `session_id` passed to `OmissionCache::insert`:
- The placeholder text will reference `session_id_A`.
- The cache record will be stored under `session_id_B`.
- When the MCP tool calls `read_omitted_content(session_id_A, "omitted-001")`, `OmissionCache::get` (`omission_cache.rs:157-170`) returns `Ok(None)` because lookups require an exact `(session_id, content_id)` composite key match!

---

## 6. Security & Data Exposure Risks

### 6.1 `OmissionCache` Data Privacy & Permissions

`OmissionCache` stores unredacted tool I/O in `omission-cache.sqlite`. Tool outputs frequently contain sensitive information:
- API keys, secrets, JWT tokens in command outputs or environment dumps.
- Source code, credentials, or private configuration files.

1. **Permission Hardening**:
   - `OmissionCache::open` (`omission_cache.rs:55-106`) enforces `0700` on the parent directory and `0600` on the database file.
   - **Vulnerability**: If `OmissionCache` is initialized in a public directory (e.g. `/tmp/omission-cache.sqlite`), `0700` permission enforcement will fail if `/tmp` permissions prevent `chmod` or if another user has write access to the parent folder.
2. **Cross-Session Inspection Leakage**:
   - ADR-009 established that `OmissionCache::get` MUST require `(session_id, content_id)`.
   - Any endpoint or MCP handler that exposes a global search or un-scoped `content_id` lookup violates cross-session isolation and creates a secret-exfiltration vulnerability.

### 6.2 HTTP API Vulnerabilities (`POST /session/prune`)

Exposing HTTP API endpoints for session pruning introduces specific attack vectors:

1. **Arbitrary File Access / Path Traversal**:
   - If `POST /session/prune` accepts `session_id` or `transcript_path` in its payload, a malicious client could send:
     ```json
     { "session_id": "../../../etc/passwd" }
     ```
   - If the server constructs paths via `Path::join`, it could attempt to parse and overwrite system files or transcripts belonging to other users.
2. **Unauthenticated Policy Injection**:
   - If `POST /session/policy` lacks authentication or local-only network binding (`127.0.0.1`), external network actors could inject aggressive pruning policies (`default_limit_chars: 0`), corrupting session contexts.

#### Guardrail Requirements:
* All HTTP routes MUST bind exclusively to `127.0.0.1`.
* `session_id` inputs MUST be strictly validated against UUID v4 format (`uuid::Uuid::parse_str`) to prevent path traversal attempts.

---

## 7. Multi-Criteria Pruning Engine & Capacity Bounds (LRU)

### 7.1 Policy Conflict: Active Turn Protection vs. Context Budget

The policy defines both:
1. `preserve_recent_turns: usize` (e.g., protect last 2 turns from any pruning).
2. `max_tool_context_bytes: usize` (e.g., total unpruned tool output budget = 50,000 bytes).

```
+-----------------------------------------------------------------------+
|  Turn 1 (Old): 10,000 bytes                                           |
|  Turn 2 (Old): 15,000 bytes                                           |
|  Turn 3 (Recent - Protected): 40,000 bytes                            |  <-- Single recent tool
|  Turn 4 (Active - Protected): 30,000 bytes                            |      result exceeds budget!
+-----------------------------------------------------------------------+
Total unpruned tool bytes = 95,000 bytes (Budget = 50,000 bytes)
```

1. **The Priority Conflict**:
   - If Turns 3 and 4 alone consume 70,000 bytes, the total context budget (50,000 bytes) is exceeded EVEN IF turns 1 and 2 are fully pruned!
   - If LRU eviction strictly enforces `max_tool_context_bytes`, it must prune Turn 3 or 4, violating `preserve_recent_turns`.
   - If active turn protection takes precedence, the context budget will be exceeded.
2. **Resolution Rule**:
   - `preserve_recent_turns` MUST take absolute precedence over `max_tool_context_bytes` for turn decay, BUT individual oversized tool outputs within recent turns MUST still be subjected to flat threshold checks (`default_limit_chars`).
   - The capacity LRU eviction pass should only target turns outside the `preserve_recent_turns` window. If recent turns exceed `max_tool_context_bytes`, a warning metric (`tracing::warn!`) must be emitted.

### 7.2 Dry-Run Mode & Sequence Pollution

When `POST /session/prune` is invoked with `dry_run: true`:
1. The engine computes rows to be pruned and estimated token savings.
2. **Sequence Pollution Hazard**:
   - If dry-run execution invokes `OmissionCache::insert`, it will increment the per-session `content_id` counter (`omitted-001`, `omitted-002`) and insert temporary records into SQLite.
   - When a subsequent non-dry-run pass executes, `content_id`s will skip numbers or collide with cached dry-run entries.

#### Guardrail Requirements:
* Dry-run evaluation MUST perform in-memory simulation only, generating dummy `content_id` strings (e.g., `omitted-dryrun-001`) without acquiring SQLite locks or calling `OmissionCache::insert`.

### 7.3 Idempotency & Double-Pruning

A pruned transcript row contains placeholder text:

```json
{
  "type": "tool_result",
  "content": "[pruned: see read_omitted_content(session_id, \"omitted-001\")]"
}
```

1. **Re-Pruning Hazard**:
   - If a second pruning pass runs on an already-pruned transcript:
   - The placeholder string has length 64 characters (below 1024 char threshold), so flat checking leaves it alone.
   - **BUT** if turn-decay policy enforces `max_turn_age = 0` (force-prune all tool outputs), `prune_tool_row` might treat the placeholder string as raw content and insert `"[pruned: see ... ]"` into `OmissionCache` again!
   - This produces nested placeholders: `[pruned: see read_omitted_content(session_id, "omitted-002")]` pointing to cached text that is *itself* a placeholder string!

#### Guardrail Requirements:
* `prune_tool_row` MUST check if `content` text already starts with `"[pruned: see read_omitted_content"`. If so, it MUST be classified immediately as `PrunedRow::Unchanged`.

---

## 8. Actionable Architectural Recommendations

To address these hazards, the implementation of `memory-pruning` should adhere to the following core design rules:

1. **Row-Mapping Architecture**: Never discard rows or reconstruct transcript files purely from `build_turns()` output. Walk the full `Vec<TranscriptRow>` sequentially, mapping turn indices onto active chain rows while passing unlinked or sidechain rows through safely.
2. **Atomic In-Place File Protection**: For live session pruning, acquire an exclusive lock (`flock`) on the session `.jsonl` file, verify EOF integrity, and write via atomic temporary file replacement within the same project directory.
3. **Transaction Isolation**: Update `OmissionCache::insert` to use `TransactionBehavior::Immediate` to prevent multi-process primary key collisions on `(session_id, content_id)`.
4. **Placeholder & Schema Preservation**: Retain exact `tool_result` JSON structure, preserve `is_error: true` rows, and never mutate `sessionId` fields during in-place pruning.
5. **Conservative Reference Decay**: Set default `unreferenced_turn_decay` floors ($\ge 3$ turns) and combine path-matching with `tool_use_id` tracking to avoid false-negative context eviction.
6. **Strict API Input Validation**: Bind all HTTP routes to `127.0.0.1` and enforce strict UUID v4 parsing on all `session_id` path and body parameters.
7. **Idempotency Guard**: Short-circuit pruning on any `tool_result` block that already contains a `[pruned: see read_omitted_content` placeholder.
