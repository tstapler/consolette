# Validation Plan: compaction-cost-metrics

**Date**: 2026-08-15

## Happy Path Scenario
Given a session with no prior `CostTracker` state (Baseline: token accounting is global and cost-blind), when the operator drives that session through `consolette serve-cost`'s `SessionCompactionPipeline::apply` at `CompactionTier::Full`, lets the request reconcile, and queries the result via both `consolette cost-report <key>` and `GET /v1/cost/<key>`, then both surfaces report identical non-zero `tokens_saved`/`estimated_cost_saved_usd` figures sourced from the same `CostReport`, proving compaction's cost effect is now measurable end to end. *(This anchors all test design below — error paths and edge cases are variations on this core scenario.)*

## Requirement → Test Mapping

| Requirement | Test File | Test Name | Type | Scenario |
|-------------|-----------|-----------|------|----------|
| REQ-1: Domain types (`RequestId`, `TokenCount`, `TokenSource`, `EstimatorKind`, `CostAmountUsd`, `ReconciliationStatus`) | `src/cost_metrics/types.rs` | `token_source_should_round_trip_serde_when_estimated_variant` | Unit | Happy path |
| REQ-1: Domain types | `src/cost_metrics/types.rs` | `request_id_default_should_equal_nil_uuid_when_not_explicitly_constructed` | Unit | Error path (guards `CompactionReport::default() == CompactionReport::default()` reflexivity, was arch C10/adv M4) |
| REQ-1: Domain types | `src/session_compaction/mod.rs` | `apply_should_generate_fresh_request_id_when_invoked` | Integration | `apply()` threads a real `RequestId::new()` into `CompactionReport` and `PostCompactContext`, not `Default` |
| REQ-2: `TiktokenEstimator` (OpenAI counterfactual) | `src/cost_metrics/estimator.rs` | `tiktoken_estimator_should_return_exact_o200k_count_when_messages_are_text_only` | Unit | Happy path (`"hello world"` → 2 tokens, `EstimatorKind::TiktokenO200k`) |
| REQ-2: `TiktokenEstimator` | `src/cost_metrics/estimator.rs` | `tiktoken_estimator_should_set_truncated_content_flag_when_image_block_present` | Unit | Error/edge path (non-text block excluded, `EstimateMeta.truncated_content == true`) |
| REQ-2: `AnthropicCountTokensEstimator` | `src/cost_metrics/estimator.rs` | `anthropic_count_tokens_estimator_should_return_estimated_source_when_mock_returns_200` | Unit | Happy path (mock `{"input_tokens":512}` → `TokenSource::Estimated{via: AnthropicCountTokensApi}`) |
| REQ-2: `AnthropicCountTokensEstimator` | `src/cost_metrics/estimator.rs` | `anthropic_count_tokens_estimator_should_return_rate_limited_error_when_mock_returns_429_without_retry` | Unit | Error path (exactly 1 request observed, no retry) |
| REQ-2: `AnthropicCountTokensEstimator` bounded concurrency + auth headers | `src/cost_metrics/estimator.rs` | `bounded_estimator_should_cap_concurrent_requests_when_burst_of_twenty_calls_issued` | Integration | Hand-rolled mock server (Task 1.2.2c) observes concurrency high-water mark ≤ N; separate assertion that `x-api-key`/`anthropic-version` headers are present |
| REQ-3: `SessionCostStore` atomic init | `src/cost_metrics/store.rs` | `get_or_init_should_return_same_arc_when_twenty_callers_race_on_fresh_key` | Unit | Happy path (moka `get_with` atomicity, `Arc::ptr_eq`, init closure runs once) |
| REQ-3: `SessionCostStore` non-creating read | `src/cost_metrics/store.rs` | `get_should_return_none_when_key_never_seen_and_must_not_create_entry` | Unit | Error path |
| REQ-3: `SessionCostStore` bounded ring | `src/cost_metrics/store.rs` | `push_record_should_evict_oldest_when_capacity_exceeded` | Integration | Insert `MAX_RECORDS_PER_SESSION + 1`, assert eviction and that `totals_by_tier` reflects only reconciled folds, not raw insert count |
| REQ-3: `CostTracker::record_pending` | `src/cost_metrics/tracker.rs` | `record_pending_should_insert_pending_row_synchronously_when_called` | Unit | Happy path (no network, no await beyond write lock) |
| REQ-3: `CostTracker` adverse ordering | `src/cost_metrics/tracker.rs` | `record_actual_usage_should_create_pending_row_when_it_arrives_before_record_pending` | Unit | Error path / edge (adverse ordering, was arch B3) |
| REQ-3: `CostTracker` idempotency | `src/cost_metrics/tracker.rs` | `record_actual_usage_should_replace_not_accumulate_totals_when_called_twice_for_same_request_id` | Unit | Error path (retry double-count guard, was adv B7) |
| REQ-3: `CostTracker` eviction safety | `src/cost_metrics/tracker.rs` | `record_actual_usage_should_return_session_not_found_when_entry_evicted_before_write` | Integration | moka TTL eviction + non-creating `get` (was adv B4) |
| REQ-3: `CostTracker::record_request_failed` | `src/cost_metrics/tracker.rs` | `record_request_failed_should_mark_abandoned_and_leave_totals_unchanged_when_pending_row_exists` | Unit | Happy path |
| REQ-3: `report_for_session` unit correctness | `src/cost_metrics/tracker.rs` | `report_for_session_should_compute_tokens_saved_as_counterfactual_minus_compacted_when_both_estimated_by_same_estimator` | Unit | Happy path (corrected same-estimator subtraction, was arch B4) |
| REQ-3: `report_for_session` pending state | `src/cost_metrics/tracker.rs` | `report_for_session_should_return_none_tokens_saved_when_only_pending_records_exist` | Unit | Error/edge path (never `0` in place of `None`) |
| REQ-3: `report_for_session` unknown session | `src/cost_metrics/tracker.rs` | `report_for_session_should_return_session_not_found_when_session_never_recorded` | Unit | Error path |
| REQ-3: `report_for_session` multi-model pricing | `src/cost_metrics/tracker.rs` | `report_for_session_should_sum_per_record_priced_costs_when_session_spans_two_models` | Integration | Two `Reconciled` records under different models; guards against report-time single-price bug (was arch B5.2) |
| REQ-4: `PricingTable::load_default` | `src/cost_metrics/pricing.rs` | `pricing_table_load_default_should_return_model_price_when_model_present_in_snapshot` | Unit | Happy path |
| REQ-4: `PricingTable::price_for` missing model | `src/cost_metrics/pricing.rs` | `price_for_should_return_none_when_model_absent_from_default_and_overrides` | Unit | Error path (never a zero/default price) |
| REQ-4: golden-value unit pricing | `src/cost_metrics/tracker.rs` | `cost_for_tokens_should_equal_exact_per_token_product_when_priced_at_write_time` | Unit | Happy path (`8000 * 0.000003 == 0.024`, pins per-token not per-million unit, was arch B5.2/adv B5) |
| REQ-4: live pricing refresh fallback | `src/cost_metrics/pricing.rs` | `pricing_refresh_task_should_leave_table_unchanged_and_log_warning_when_fetch_returns_500` | Integration | Mock LiteLLM endpoint returning 500; asserts table unchanged, one `tracing::warn!`, one metric increment |
| REQ-5: `CostTrackingHook::post_compact` | `src/cost_metrics/hook.rs` | `cost_tracking_hook_should_produce_pending_record_synchronously_when_apply_returns` | Integration | Registered hook + slow/never-resolving mock estimator proves synchronous insert independent of estimator completion |
| REQ-5: `CostTrackingHook` estimator failure | `src/cost_metrics/hook.rs` | `cost_tracking_hook_should_mark_abandoned_when_estimator_returns_rate_limited_error` | Unit | Error path |
| REQ-5: `apply()` latency isolation | `src/cost_metrics/hook.rs` | `apply_should_return_under_fifty_millis_when_estimator_mock_delays_five_hundred_millis` | Integration | NFR regression guard (cost accounting must not add hot-path latency) |
| REQ-6: `record_actual_usage_from_anthropic_response` | `src/providers/mod.rs` | `record_actual_usage_from_anthropic_response_should_combine_input_and_output_tokens_when_usage_present` | Unit | Happy path |
| REQ-6: usage parsing malformed response | `src/providers/mod.rs` | `record_actual_usage_from_anthropic_response_should_return_none_when_usage_field_missing` | Unit | Error path |
| REQ-7: read-time pending age-out sweep | `src/cost_metrics/tracker.rs` | `report_for_session_should_age_out_stale_pending_row_to_abandoned_when_older_than_pending_max_age` | Integration | Simulated old `recorded_at`, next read sweeps it |
| REQ-8: `serve-cost` server bootstrap | `src/cost_metrics/server.rs` | `serve_cost_should_respond_404_when_unknown_session_queried` | Integration | Real bind, `reqwest` GET against OS-assigned port |
| REQ-8: `serve-cost` pipeline/tracker sharing | `src/cost_metrics/server.rs` | `serve_cost_should_expose_apply_result_via_http_route_when_pipeline_and_route_share_same_tracker` | Integration | Direct regression guard for Blocker 1 — drive `apply()` through the process's pipeline, reconcile, assert HTTP route returns it |
| REQ-8: `handler_cost_report` happy path | `src/cost_metrics/server.rs` | `handler_cost_report_should_return_200_with_report_json_when_session_reconciled` | Unit | Happy path (axum `oneshot`, no full bind) |
| REQ-8: `handler_cost_report` not found | `src/cost_metrics/server.rs` | `handler_cost_report_should_return_404_with_session_key_echoed_when_session_unknown` | Unit | Error path |
| REQ-9: CLI `cost-report` happy path | `src/cost_metrics/client.rs` | `fetch_cost_report_should_deserialize_report_when_server_returns_200` | Unit | Happy path (mock server) |
| REQ-9: CLI `cost-report` unreachable server | `src/cost_metrics/client.rs` | `fetch_cost_report_should_return_unreachable_error_when_connection_refused` | Unit | Error path (distinct wording from not-found) |
| REQ-9: CLI/HTTP byte-identical agreement | `src/cost_metrics/server.rs` (or `tests/cost_metrics_integration.rs`) | `cost_report_client_and_raw_http_get_should_return_identical_json_when_hitting_same_server` | Integration | Guards against a future regression reintroducing an independent CLI-side computation (Task 3.2.1d) |
| REQ-9: table formatting for missing price | `src/cost_metrics/cli_format.rs` | `format_cost_report_table_should_render_unavailable_when_estimated_cost_saved_usd_is_none` | Unit | Error path (never `$0.00`) |
| REQ-10 (success metric): Full-vs-Off end-to-end | `tests/cost_metrics_end_to_end.rs` | `full_tier_session_should_report_materially_higher_tokens_saved_than_off_tier_session_when_same_history_applied` | Integration | Happy path — the requirements.md success metric itself |
| REQ-10: Off-tier zero-savings distinguishability | `tests/cost_metrics_end_to_end.rs` | `off_tier_session_should_report_zero_tokens_saved_not_error_when_compaction_elides_nothing` | Integration | Error/edge path (contrast case vs. `SessionNotFound`) |
| REQ-11: TTL eviction vs. zero-savings distinguishability | `src/cost_metrics/tracker.rs` | `report_for_session_should_return_session_not_found_when_cache_entry_ttl_expired` | Integration | ux.md core distinguishability requirement — must not collapse into `tokens_saved: Some(0)` |

## UX Acceptance Tests

| UX Criterion | Test File | Test Name | Tool | Steps |
|---|---|---|---|---|
| CLI: exact/estimated stated as text suffix, never color-only (ux.md AC1, AC9) | `cli_manual_checklist.md` (or `cli_format.rs` unit test) | `format_cost_report_table_should_print_estimated_via_suffix_when_source_is_estimated` | Manual + Unit | Run `consolette cost-report s1` with `NO_COLOR=1` and piped to a file; confirm `(exact)`/`(estimated via AnthropicCountTokensApi)` suffixes are present as literal text |
| CLI: three states never collapse — pending prints `unavailable — request did not complete` (ux.md AC2) | Manual | n/a | Manual | Query a session with only `Pending` records; confirm literal string appears and `tokens_saved`/`estimated_cost_saved_usd` lines are omitted, not `0` |
| CLI: `--json` is byte-identical source to table (ux.md AC3) | `cli_format.rs` | `format_cost_report_json_should_match_serde_to_string_pretty_when_given_same_report` | Unit | Assert no separate JSON-construction path |
| CLI: missing price renders `unavailable`, never `$0.00` (ux.md AC4) | Covered above (`format_cost_report_table_should_render_unavailable_when_estimated_cost_saved_usd_is_none`) | — | Unit | — |
| CLI: one-glance summary answers "did compaction save money" (ux.md AC5) | Manual | n/a | Manual | Run `cost-report s1`; confirm top-level `tokens_saved`/`estimated_cost_saved_usd` lines alone answer the question without reading `by_tier` |
| HTTP: `null` never `0` for unknown/pending values (ux.md AC on HTTP surface) | `server.rs` | `handler_cost_report_should_serialize_null_not_zero_when_tokens_saved_unknown` | Unit | Deserialize JSON body, assert field is JSON `null` |
| HTTP: `counterfactual_source`/`pricing_source` always explicit tagged values (ux.md AC10) | `server.rs` | `handler_cost_report_should_include_explicit_pricing_source_tag_when_response_rendered` | Unit | Assert `"Static"`/`"Live"` string present, not inferred |
| HTTP: 404 always includes `error` and `session_key` (ux.md HTTP AC4) | Covered above (`handler_cost_report_should_return_404_with_session_key_echoed_when_session_unknown`) | — | Unit | — |
| HTTP: one GET returns everything needed (ux.md HTTP AC5) | Manual | n/a | Manual | Single `curl GET /v1/cost/s1`; confirm no follow-up call needed for pricing metadata |
| Logging: fallback emitted exactly once per event, not per request (ux.md Surface 3 AC1) | `pricing.rs` | `pricing_refresh_task_should_log_warning_exactly_once_when_fetch_fails_once` | Unit | Assert log line count == 1 despite N requests served from stale table |
| Logging: reason included as structured text (ux.md Surface 3 AC2) | `pricing.rs` | `pricing_fallback_log_should_include_failure_reason_when_timeout_occurs` | Unit | Assert log message contains `"timed out"` / `"non-200"` / `"malformed JSON"` distinctly |
| Logging: log and metric never disagree (ux.md Surface 3 AC3) | `pricing.rs` | `pricing_fallback_should_increment_metric_in_same_code_path_as_log_line` | Unit | Assert both happen together, same branch |

## Test Stack
- **Unit**: Rust built-in `#[test]` / `#[tokio::test]` with `assert_eq!`/`assert!`; no separate assertion library (matches existing repo convention — `Cargo.toml` has no `assert2`/`pretty_assertions`).
- **Integration**: `#[tokio::test]` against real `moka` caches with short TTLs, `tokio::spawn` + `join_all` for concurrency races, and a hand-rolled `axum`/`tokio` local test server (`src/cost_metrics/test_support.rs`, Task 1.2.2c) for HTTP-mocking — no `wiremock` (repair iteration 1 decision, Blocker 11). `tests/cost_metrics_end_to_end.rs` and `tests/cost_metrics_integration.rs` in the repo's established `tests/` convention (`tests/toml_parity.rs` precedent).
- **E2E / UX**: Manual checklist — this is a CLI/HTTP tool, no browser UI. Manual steps run `consolette serve-cost` locally, then `consolette cost-report` / `curl` against it, per the table above.

## Coverage Targets and How to Measure

| Stack | Coverage command | Target |
|---|---|---|
| Rust | `cargo tarpaulin --out Stdout` | ≥80% line, with `src/cost_metrics/*` specifically ≥90% given this module *is* the observability feature |

- All public `CostTracker`/`SessionCostStore`/`TokenEstimator`/`PricingTable` methods: happy path + error paths covered (see table above).
- All external integrations (Anthropic `count_tokens`, LiteLLM pricing fetch, the `serve-cost` HTTP route): unit mocked (hand-rolled server) + at least one integration test each.
- UX acceptance criteria: every criterion in `design/ux.md` Step 3 (Given-When-Then items 1–10) plus Step 2's per-surface acceptance criteria has a corresponding test or manual step above.
- Migration Plan test: **N/A** — plan.md's own Migration Plan section states no schema/persisted-data changes (`pricing_default.json` is a checked-in fixture, not a migration); skipped per instructions Step 5.
