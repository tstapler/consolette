# Validation Plan: `memory-pruning`

**Project**: `memory-pruning`  
**Target Architecture**: Turn-Based Multi-Criteria Transcript Memory Pruning Engine & HTTP Control Plane  
**Target Directory**: `/home/tstapler/.stapler-squad/repos/github.com/tstapler/consolette`  
**Date**: 2026-09-18  

---

## 1. Executive Summary

This document establishes the test suite design and requirement-to-test traceability matrix for the `memory-pruning` project. The validation strategy covers unit, integration, and UX/acceptance testing across all 5 implementation epics, verifying:
- Multi-criteria policy evaluation (`PruningPolicy`), glob matching, and `PruningPolicyStore`.
- Turn age calculation $A_i = (N-1) - i$, trailing turn protection (`preserve_recent_turns`), and subagent sidechain indexing.
- Tool output reference tracking (`ReferenceMap`), `ToolNameMap` lookup mapping, error output preservation, and idempotency guards.
- Transcript cumulative capacity caps (`max_tool_context_bytes`), LRU eviction, and precedence rule enforcement.
- Exclusive file locking (`flock`), incomplete EOF line detection, size/mtime pre-rename validation, atomic tempfile replacement, and `--resume` compatibility.
- `OmissionCache` SQLite `TransactionBehavior::Immediate` transaction locking and suffix retry loop (`omitted-001_1`).
- RESTful HTTP control plane endpoints (`POST /session/prune`, `POST /session/policy`, `GET /session/prune/stats`), dry-run simulation mode (`dry_run: true`), and strict UUID v4 path traversal validation.

---

## 2. Requirement-to-Test Traceability Matrix

| Requirement ID | Requirement Description | Test Cases | Target Component |
| :--- | :--- | :--- | :--- |
| **REQ-1** | Turn-Based Decay Policy & Relative Turn Age Calculation | `UT-TURN-001`, `UT-TURN-002`, `UT-TURN-003`, `UT-TURN-004`, `UT-EVAL-002`, `IT-MUT-001` | `src/claude_code_session/transcript.rs`, `prune.rs` |
| **REQ-2** | Unreferenced Tool Output Eviction & Assistant Reference Scanning | `UT-REF-001`, `UT-REF-002`, `UT-REF-003`, `UT-EVAL-003`, `IT-MUT-001` | `src/claude_code_session/transcript.rs`, `prune.rs` |
| **REQ-3** | Heuristic & Glob Rules, ToolNameMap, Error Preservation, Idempotency Guard | `UT-POLICY-001`, `UT-POLICY-002`, `UT-POLICY-003`, `UT-LOOKUP-001`, `UT-LOOKUP-002`, `UT-EVAL-001`, `UT-EVAL-004`, `UT-IDEM-001`, `IT-API-003` | `src/claude_code_session/prune.rs` |
| **REQ-4** | Capacity Bounds, LRU Eviction & Trailing Turn Precedence Rule | `UT-CAP-001`, `UT-CAP-002`, `UT-CAP-003` | `src/claude_code_session/prune.rs` |
| **REQ-5** | Safe Atomic Rewriting, Flock, Pre-Rename Mtime/Size Check, OmissionCache Multi-Process Retries | `UT-LOCK-001`, `UT-CACHE-001`, `UT-CACHE-002`, `UT-CACHE-003`, `IT-MUT-001`, `IT-MUT-002`, `IT-MUT-003`, `IT-MUT-004`, `IT-MUT-005`, `IT-MUT-006`, `IT-CACHE-004`, `AT-RESTORE-001`, `AT-RACE-001` | `src/claude_code_session/transcript.rs`, `prune.rs`, `omission_cache.rs` |
| **REQ-6** | HTTP Control Plane Endpoints, Dry-Run Simulation & UUID Path Resolution | `UT-SEC-001`, `IT-API-001`, `IT-API-002`, `IT-API-004`, `IT-API-005`, `IT-API-006`, `IT-API-007`, `IT-API-008`, `AT-E2E-001`, `AT-SEC-001` | `src/entrypoint/api.rs`, `mod.rs` |
| **REQ-7** | Non-Functional SLO (<10ms), Observability & Security Isolation | `AT-SLO-001`, `AT-SEC-001`, `IT-API-008`, `IT-API-005` | System-wide benchmark / security test |

---

## 3. Detailed Test Suite Specifications

### 3.1 Unit Test Specifications (`unit` - 25 Test Cases)

#### Epics 1 & 3: Policy Data Structures & Multi-Criteria Engine
- **UT-POLICY-001**: `PruningPolicy` struct default creation and JSON serde serialization/deserialization.
- **UT-POLICY-002**: Glob pattern evaluation using `glob::Pattern` for tool names (`Bash`, `Agent`, `mcp__*`, `TaskOutput`).
- **UT-POLICY-003**: `PruningPolicyStore` thread-safety under concurrent readers/writers and per-session policy override fallback to global defaults.
- **UT-EVAL-001**: Single-row character (`default_limit_chars`) and word (`default_limit_words`) threshold pruning evaluation.
- **UT-EVAL-002**: Turn age decay threshold evaluation (`max_turn_age`) against relative turn age $A_i$.
- **UT-EVAL-003**: Unreferenced turn decay evaluation (`unreferenced_turn_decay`) asserting unreferenced outputs are evicted after decay window.
- **UT-EVAL-004**: Diagnostic error output preservation when `is_error == true` and `preserve_error_outputs == true`.
- **UT-IDEM-001**: Idempotency guard verifying strings starting with `"[pruned: see read_omitted_content"` return `PrunedRow::Unchanged` without creating nested placeholders.

#### Epic 2: Turn Reconstruction, Reference Resolution & Tool Name Lookup
- **UT-TURN-001**: Turn sequence reconstruction calculating total turns $N$ and relative turn age $A_i = (N-1) - i$.
- **UT-TURN-002**: Trailing turn protection boundary check marking turns $i \ge N - \text{preserve\_recent\_turns}$ as protected.
- **UT-TURN-003**: Subagent sidechain row mapping, verifying sidechain tool rows receive the turn index of their parent main-chain assistant turn.
- **UT-TURN-004**: Edge case handling for single-turn transcripts ($N=1$) verifying no out-of-bounds relative age underflow.
- **UT-REF-001**: Direct `tool_use_id` reference resolution scanning assistant turns.
- **UT-REF-002**: Tool input parameter path reference scanning (`file_path`, `path`, `command`, `id`) matching output targets across assistant turns.
- **UT-REF-003**: Prevention of false-negative evictions when a tool output target is cited in subsequent assistant text.
- **UT-LOOKUP-001**: `ToolNameMap` index building from assistant turn `tool_use` blocks.
- **UT-LOOKUP-002**: Tool name lookup resolution for API `tool_result` blocks using `ToolNameMap` during pruning pass.

#### Epic 3: Capacity Bounds & LRU Eviction Pass
- **UT-CAP-001**: Transcript unpruned tool context cumulative byte calculation against `max_tool_context_bytes`.
- **UT-CAP-002**: Candidate sorting logic for LRU eviction ordered by `(is_referenced, last_reference_turn_index, turn_index)` ascending.
- **UT-CAP-003**: Trailing turn protection precedence over capacity caps: verifies LRU eviction stops at recent protected turns and emits `tracing::warn!`.

#### Epic 4: File Integrity & SQLite Transaction Safety
- **UT-LOCK-001**: EOF partial-line detection verifying un-terminated lines at EOF trigger clean parse abortion.
- **UT-CACHE-001**: `OmissionCache::insert` using `TransactionBehavior::Immediate` write locks.
- **UT-CACHE-002**: Primary key collision monotonic suffix retry loop (`omitted-001_1`, `omitted-001_2`, ...).
- **UT-CACHE-003**: SQLite file and directory POSIX permissions enforcement (`0600` file / `0700` dir).
- **UT-SEC-001**: Strict UUID v4 string parsing (`uuid::Uuid::parse_str`) rejecting malformed strings or path traversal inputs.

---

### 3.2 Integration Test Specifications (`integration` - 15 Test Cases)

#### Epic 4: Safe Transcript Mutation & Live File Appends
- **IT-MUT-001**: 1:1 row-mapping pass over complete `Vec<TranscriptRow>` preserving disconnected historical turns when `chain_coverage < 1.0`.
- **IT-MUT-002**: Sidechain row metadata preservation without chain coverage data loss during transcript rewriting.
- **IT-MUT-003**: Exclusive file locking (`flock`) acquisition and release during active session file pruning.
- **IT-MUT-004**: Pre-rename file size and `mtime` verification aborting swap when on-disk file size/mtime change concurrently.
- **IT-MUT-005**: Atomic temporary file replacement (`tempfile` + rename) within the target project directory.
- **IT-MUT-006**: Transcript identity preservation verifying `sessionId`, `uuid`, and `parentUuid` restamping rules.
- **IT-CACHE-004**: Multi-threaded and multi-process concurrent insertion stress test into `OmissionCache`.

#### Epic 5: HTTP API Control Plane Integration
- **IT-API-001**: `POST /session/prune` live mode (`dry_run: false`) verifying file rewrite and `OmissionCache` record creation.
- **IT-API-002**: `POST /session/prune` dry-run mode (`dry_run: true`) verifying in-memory simulation without disk or SQLite mutations.
- **IT-API-003**: `POST /session/prune` with inline `policy_override` evaluating request-scoped policy rules.
- **IT-API-004**: `resolve_session_path` resolving session UUIDs to `~/.claude/projects/*/<session_id>.jsonl`.
- **IT-API-005**: Security rejection of path traversal payloads (`../../../etc/passwd`, invalid UUIDs) returning `400 Bad Request`.
- **IT-API-006**: `POST /session/policy` updating global default policy in `PruningPolicyStore`.
- **IT-API-007**: `POST /session/policy` setting and clearing per-session policy overrides.
- **IT-API-008**: `GET /session/prune/stats` returning accurate total turns, total rows, pruned count, omission cache size, and active policy.

---

### 3.3 UX & System Acceptance Specifications (`ux_acceptance` - 5 Test Cases)

- **AT-RESTORE-001**: End-to-end Claude Code `--resume` session restoration compatibility check using a pruned transcript file.
- **AT-E2E-001**: Full transcript lifecycle test: live session execution -> dry-run HTTP API preview -> live HTTP API pruning pass -> stats inspection.
- **AT-RACE-001**: Concurrency stress test executing rapid CLI file appends while executing concurrent HTTP API pruning passes under `flock` and pre-rename mtime validation.
- **AT-SLO-001**: Performance benchmark verifying pruning pass execution time is < 10ms for transcripts up to 5,000 rows.
- **AT-SEC-001**: Network and process isolation test verifying HTTP daemon loopback `127.0.0.1` binding and rejection of non-loopback requests.

---

## 4. Test Case Distribution & Metrics Summary

| Test Type | Count | Percentage |
| :--- | :--- | :--- |
| **Unit Tests (`unit`)** | 25 | 55.6% |
| **Integration Tests (`integration`)** | 15 | 33.3% |
| **UX / System Acceptance (`ux_acceptance`)** | 5 | 11.1% |
| **Total Test Cases** | **45** | **100.0%** |

- **Requirements Coverage Fraction**: **7 / 7 (100.0%)**
- **Unmapped Requirements**: None.

---

## 5. Implementation Readiness Gate Checklist

| # | Criterion | Pass? | Evidence / Notes |
|---|-----------|-------|------------------|
| 1 | Every requirement in `requirements.md` has $\ge 1$ test case in `validation.md` | **PASS** | All 7 requirements mapped to 45 dedicated unit, integration, and acceptance tests. |
| 2 | `plan.md` has no TODO/TBD placeholders in architecture or task sections | **PASS** | `plan.md` audited; zero TODO/TBD placeholders found. |
| 3 | All ADRs referenced in `plan.md` exist on disk | **PASS** | `ADR-001`, `ADR-002`, `ADR-003` exist in `decisions/`. |
| 4 | No BLOCKER items remain in `adversarial-review.md` (or file is absent) | **PASS** | `adversarial-review.md` reports "Verdict: CLEAN" with all 7 previous findings resolved. |
| 5 | No BLOCKER items remain in `architecture-review.md` (or file is absent) | **PASS** | File absent (no architecture review blockers). |
| 6 | For schema changes: Migration Plan section in `plan.md` defines reversibility + zero-downtime strategy | **PASS** | `plan.md` preserves `TranscriptRow` JSON block schema compatibility and SQLite `OmissionCache` schema. |
| 7 | No P1 items remain open in `pre-mortem.md` (or file is absent) | **PASS** | File absent (no pre-mortem P1 blockers). |
| 8 | Architecture read of `plan.md`: no planned package would cycle-import an existing one; no planned module takes on more than one layer's responsibility | **PASS** | Clean separation of concerns between domain logic (`claude_code_session`) and transport layer (`entrypoint/api`). |

---

## 6. Readiness Gate Verdict

**VERDICT: PASS**

The project `memory-pruning` meets all implementation readiness criteria. All 7 requirements are fully covered by 45 designed test cases across unit, integration, and acceptance tiers. All architectural decisions, concurrency protections, and adversarial review findings are addressed. The project is ready to proceed to implementation (`/sdd:5-implement`).
