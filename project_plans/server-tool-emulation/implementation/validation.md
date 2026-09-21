# Validation Plan: Server-Tool Emulation (web_search)

**Date**: 2026-09-13 — test coverage map BEFORE any code exists (planning-only run).

## Test Stack

- **Unit**: `cargo test` (in-module `#[cfg(test)]`), hermetic — fake upstream
  `dispatch_fn`, fake `rmcp` stdio MCP server, no network, no daemon.
- **Integration**: `tests/server_tool_emulation.rs` — fake OpenAI-style
  upstream (scripted `tool_calls` responses) + fake MCP server asserting
  `{query, count}` shapes; `BRAVE_API_BASE_URL`/`STAPLER_MCP_HOME` isolation
  pattern reserved for live-daemon soak (manual, not CI).
- **Naming**: `method_should_expected_when_condition` per repo convention.

## Requirement → Test Mapping

| Requirement | Test | Type | Scenario |
|---|---|---|---|
| S-1 Full-path emulation | `loop_should_return_server_tool_blocks_when_upstream_calls_search` | Integration | Happy path |
| S-1 Full-path, no search called | `loop_should_return_verbatim_answer_when_model_never_searches` | Unit | Happy path |
| S-1 Stream path | `stream_should_emit_wellformed_sse_when_server_tool_present` (golden frames) | Integration | Happy path |
| S-1 Stream byte-shape | `sse_should_never_emit_function_tool_use_on_wire` | Unit | Contract |
| S-2 Cohere 400 class gone | `rewrite_should_emit_described_nonempty_schema_when_server_def_given` | Unit | Regression |
| S-2 e2e | `e2e_should_succeed_with_results_when_cohere_model_requests_search` | Integration | Regression |
| S-3 No server tools ⇒ untouched | `entrypoint_should_dispatch_once_unchanged_when_no_server_def` (fixture diff) | Integration | No-regression |
| S-4 Backend down ⇒ degrade | `loop_should_degrade_to_drop_when_executor_fails` | Integration | Error path |
| S-4 Binary missing | `pool_should_degrade_when_binary_missing` | Unit | Error path |
| S-4 Timeout | `search_should_timeout_and_continue_when_backend_hangs` (tokio test-util) | Unit | Error path |
| S-4 Circuit breaker | `breaker_should_skip_emulation_when_failures_consecutive` | Unit | Error path |
| Guards: iteration cap | `loop_should_terminate_at_cap_when_model_always_searches` | Unit | Guard |
| Guards: total deadline | `loop_should_terminate_at_deadline_when_iterations_slow` | Unit | Guard |
| Guards: empty query | `loop_should_feedback_error_when_query_empty` | Unit | Error path |
| Health non-poisoning (P2) | `executor_failure_should_not_trip_cooldown_when_backend_down` | Integration | Contract |
| Mixed turn (edge 3) | `map_should_preserve_tool_use_stop_when_other_tools_called` | Unit | Edge |
| Multi-search turn (edge 4) | `loop_should_execute_sequential_searches_when_multiple_calls` | Unit | Edge |
| Replay hygiene (P7) | `redispatch_should_send_function_history_when_server_blocks_present` | Unit | Contract |
| Usage aggregation (P8) | `usage_should_sum_iterations_when_loop_runs` | Unit | Contract |
| Stream single-count (C4) | `stream_cost_should_count_once_when_loop_recorded` (V-STREAM-03) | Integration | Contract |
| Version drift (P10) | `detect_should_match_all_known_and_future_versions` (parametrized) | Unit | Edge |
| Credentials (P4) | `config_should_carry_no_secrets_when_snapshot` + redaction test | Unit | Contract |
| Pool bound (P9) | `pool_should_hold_bound_when_requests_concurrent` | Integration | Load |

## Coverage Targets

- Unit coverage ≥80% (line) on `src/server_tools/`; every public function:
  happy path + error path.
- Every executor degrade branch (missing binary, timeout, bad JSON, missing
  tool, circuit-open) covered hermetically.
- Golden SSE fixture reviewed frame-by-frame once by hand, then pinned.

## Readiness gate (Phase 4 gate — inline)

| # | Criterion | Status |
|---|---|---|
| 1 | Every requirement (S-1..S-4 + guards) has ≥1 test case above | ✅ PASS (22 cases) |
| 2 | plan.md has no TODO/TBD placeholders | ✅ PASS |
| 3 | All ADRs referenced exist | ✅ PASS (none referenced) |
| 4 | No BLOCKERs in adversarial-review.md | ✅ PASS (CONCERNS verdict) |

**Gate verdict: PASS** → Phase 5 may proceed in a fresh session after owner
decisions D1–D3.
