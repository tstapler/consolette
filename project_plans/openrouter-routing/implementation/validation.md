# Validation Plan: openrouter-routing

**Date**: 2026-09-07

## Happy Path Scenario
Given a `conf.d/*.toml` with an `openrouter`-kind upstream and a route configured with `strategy = "openrouter_scored"` (the Baseline's replacement for hand-listing one free model), when Tyler's Anthropic-API client sends a chat request to that route, then `Router::dispatch` fans the route out to the currently-cached free-model pool, `OpenrouterScoringStrategy::select()` picks the best-scoring healthy candidate by composite score, `OpenrouterProvider::send()` proxies the request to that specific free model on OpenRouter's Chat Completions API, and the response — plus the chosen model id and its score breakdown — is observable afterward via `RequestDetail.selected_model` and `GET /metrics`'s `openrouter_scoring` block, without ever having hand-listed that model in config.

Error paths (pool exhaustion, cache staleness, unranked models, config mistakes) are variations on this core flow, not equal-priority items — each is one candidate failing/being replaced within the same dispatch loop, except total exhaustion, which is this flow's one fail-closed terminus.

---

## Requirement → Test Mapping

### Success Metric / Scope: `UpstreamKind::Openrouter` + `OpenrouterProvider` (requirements.md Success Metrics #1, Scope #1)

| Requirement | Test File | Test Name | Type | Scenario |
|-------------|-----------|-----------|------|----------|
| REQ-1: `kind = "openrouter"` parses to `UpstreamKind::Openrouter`, `strategy = "openrouter_scored"` parses to `Strategy::OpenrouterScored` | `src/config/schema.rs` | `upstream_kind_should_deserialize_to_openrouter_variant` | Unit | Happy path — Task 1.1.1c |
| REQ-1: unknown field under `kind = "openrouter"` is rejected, not silently ignored | `src/config/schema.rs` | `upstream_kind_openrouter_should_reject_unknown_field` | Unit | Error path — Task 1.1.1c |
| REQ-1: `send()` forwards Chat-Completions request with bearer auth + `HTTP-Referer`/`X-Title` headers | `src/providers/openrouter/mod.rs` | `send_should_forward_request_with_auth_and_referer_headers` | Unit | Happy path — Task 1.2.1e |
| REQ-1: model-not-found response classifies to `ProviderError::ModelUnsupported`, not generic `Upstream` | `src/providers/openrouter/mod.rs` | `send_should_return_model_unsupported_when_model_not_found_response` | Unit | Error path — Task 1.2.1e |
| REQ-1: 429 with `Retry-After` classifies to `RateLimitedWithRetry`, without it to `RateLimited` | `src/providers/openrouter/mod.rs` | `send_should_classify_rate_limit_with_and_without_retry_after` | Unit | Error path — Task 1.2.1e |
| REQ-1: `build_providers` constructs an `Arc<OpenrouterProvider>` and returns the index→provider map alongside the existing `Vec<(String, Arc<dyn Provider>)>`, and an end-to-end request against a mock OpenRouter server succeeds | `src/routing/router.rs` | `build_providers_should_construct_provider_for_upstream_kind_openrouter` | Integration | Mock HTTP server — Task 1.2.3d, Task 4.3.1c |
| REQ-1: `upstream_kind_label` returns `"openrouter"` for the new variant | `src/entrypoint/mod.rs` | `upstream_kind_label_should_return_openrouter_for_new_variant` | Unit | Happy path — Task 1.2.3d |

### Scope: Free-model auto-discovery via `/models` (requirements.md Success Metrics #2, Scope #2)

| Requirement | Test File | Test Name | Type | Scenario |
|-------------|-----------|-----------|------|----------|
| REQ-2: `list_models()` returns every reported model unfiltered | `src/providers/openrouter/models.rs` | `list_models_should_return_all_reported_models_unfiltered` | Unit | Happy path — Task 1.2.2d |
| REQ-2: `list_free_models()` returns only price==0 entries as `FreeModelEntry{id, price_prompt, price_completion}`, carrying price forward | `src/providers/openrouter/models.rs` | `list_free_models_should_return_only_zero_priced_models` | Unit | Happy path — Task 1.2.2d |
| REQ-2: malformed pricing field is treated as not-free (fail-soft), not a parse error | `src/providers/openrouter/models.rs` | `list_free_models_should_treat_malformed_pricing_as_not_free` | Unit | Error path — Task 1.2.2d |
| REQ-2: `list_models()`/`list_free_models()` share one HTTP GET via `fetch_models_raw()` against a mock `/models` endpoint | `src/providers/openrouter/models.rs` | `fetch_models_raw_should_be_reused_by_both_list_methods` | Integration | Mock HTTP server — Task 1.2.2d |

### Scope: `ModelListCache` — TTL + early invalidation (requirements.md Success Metrics #2, Scope #3)

| Requirement | Test File | Test Name | Type | Scenario |
|-------------|-----------|-----------|------|----------|
| REQ-3: `snapshot()` is `None` before first refresh, `Some(..)` after | `src/providers/openrouter/cache.rs` | `snapshot_should_return_none_before_first_refresh_and_some_after` | Unit | Happy path — Task 2.1.1c |
| REQ-3: `snapshot()` returns `None` again once the TTL expires and no refresh has run | `src/providers/openrouter/cache.rs` | `snapshot_should_return_none_after_ttl_expires` | Unit | Error/edge path — Task 2.1.1c |
| REQ-3: `invalidate()` clears the entry immediately, not waiting for TTL | `src/providers/openrouter/cache.rs` | `invalidate_should_clear_entry_immediately` | Unit | Happy path — Task 2.1.1c |
| REQ-3: `OpenrouterProvider::new()` eagerly populates `model_cache` before returning, unconditionally of which `Strategy` later references the upstream | `src/providers/openrouter/mod.rs` | `new_should_eagerly_populate_cache_before_returning_regardless_of_strategy` | Integration | Mock HTTP server — Task 2.1.2d |
| REQ-3: an unreachable OpenRouter endpoint still returns `Ok(Arc<OpenrouterProvider>)` with `snapshot() == None`, logged not propagated | `src/providers/openrouter/mod.rs` | `new_should_return_ok_with_empty_cache_when_upstream_unreachable` | Integration | Error path, mock HTTP server — Task 2.1.2d |
| REQ-3: the background refresh task exits the first time its `Weak` handles fail to upgrade (provider dropped) | `src/providers/openrouter/mod.rs` | `background_refresh_task_should_exit_when_provider_is_dropped` | Integration | Task 2.1.2d |
| REQ-3: a minority of cached models 404ing within 60s invalidates the cache and sets `last_invalidation_reason = "model_not_found:<id>"` | `src/providers/openrouter/cache.rs` | `record_not_found_and_maybe_invalidate_should_invalidate_on_minority_404` | Unit | Happy path — Task 2.1.3c |
| REQ-3: every cached model 404ing within 60s is suppressed as systemic (data-policy toggle), not invalidated | `src/providers/openrouter/cache.rs` | `record_not_found_and_maybe_invalidate_should_suppress_systemic_404` | Unit | Error/edge path — Task 2.1.3c |
| REQ-3: 404s older than the 60s window are pruned and don't count toward "everyone just failed" | `src/providers/openrouter/cache.rs` | `record_not_found_and_maybe_invalidate_should_prune_stale_404_entries` | Unit | Edge path — Task 2.1.3c |

### Scope: New composite-scoring `RoutingStrategy` (requirements.md Success Metrics #3, Scope #4)

| Requirement | Test File | Test Name | Type | Scenario |
|-------------|-----------|-----------|------|----------|
| REQ-4: `sample_count()` distinguishes 0 real samples from real samples of value 0 | `src/metrics/histogram.rs` | `sample_count_should_return_zero_for_empty_histogram` | Unit | Happy path — Task 3.2.1b |
| REQ-4: `RollingErrorRate::error_rate()` returns `None` cold-start, correct fraction otherwise, trims by window, recovers from a poisoned mutex | `src/routing/model_stats.rs` | `error_rate_should_return_none_when_cold_start` | Unit | Happy path — Task 3.2.2b |
| REQ-4: `error_rate()` computed fraction is wrong when samples are stale/poisoned recovery fails | `src/routing/model_stats.rs` | `error_rate_should_exclude_samples_older_than_window` | Unit | Error/edge path — Task 3.2.2b |
| REQ-4: `ModelStats::new()` bundles both trackers cold | `src/routing/model_stats.rs` | `model_stats_new_should_report_cold_start_on_both_trackers` | Unit | Happy path — Task 3.2.3b |
| REQ-4: composite score strictly favors best-latency/error/bench candidate over worst, across pool sizes 2/3/5 | `src/routing/openrouter_scoring.rs` | `score_should_rank_best_candidate_above_worst_candidate_across_pool_sizes` | Unit | Happy path — Task 4.2.5a |
| REQ-4: a cold-start, unranked candidate scores exactly `0.5` in isolation and between WORST/BEST in a shared pool | `src/routing/openrouter_scoring.rs` | `score_should_default_to_neutral_when_cold_start_and_unranked` | Unit | Error/edge path — Task 4.2.5b |
| REQ-4: `WEIGHT_ERROR + WEIGHT_LATENCY + WEIGHT_BENCH == 1.0` | `src/routing/openrouter_scoring.rs` | `scoring_weights_should_sum_to_one` | Unit | Task 4.2.5c |
| REQ-4: an unranked model logs `tracing::warn!` exactly once per model id, not once per `select()` call | `src/routing/openrouter_scoring.rs` | `select_should_warn_once_per_unranked_model_id` | Unit | Error/edge path — Task 4.2.1c/d |
| REQ-4: `select()` returns `None` on an empty pool, always returns the sole candidate with one, and over many trials prefers the higher-scoring candidate ~90% of the time while still occasionally picking the lower one (ε=0.1) | `src/routing/openrouter_scoring.rs` | `select_should_prefer_higher_scoring_candidate_most_of_the_time` | Unit | Happy path — Task 4.2.2b |
| REQ-4: `select()` returns `None` when `healthy` is empty | `src/routing/openrouter_scoring.rs` | `select_should_return_none_when_no_healthy_candidates` | Unit | Error path — Task 4.2.2b |
| REQ-4: a successful attempt records `(duration_ms, true)` into the right model's `ModelStats` | `src/routing/openrouter_scoring.rs` | `record_outcome_should_record_success_into_model_stats` | Unit | Happy path — Task 4.2.3c |
| REQ-4: a `rate_limited` failure records the real failure plus `RATE_LIMIT_SYNTHETIC_FAILURES` synthetic failures (ADR-002), and this measurably trips `HealthRegistry` for the shared upstream index so every per-model candidate there cools down together | `src/routing/router.rs` | `record_outcome_rate_limited_should_trip_health_registry_for_shared_index` | Integration | `HealthRegistry` + real `Router` — Task 4.2.3c |
| REQ-4: `RoutingStrategy`'s 3 new methods default to identity/no-op/`None` and leave `FallbackStrategy`/`WeightedStrategy`'s existing test suite unmodified | `src/routing/strategy.rs` | `expand_candidates_should_return_unchanged_candidates_by_default` | Unit | Happy path (regression) — Task 3.1.1b |
| REQ-4: widening `already_tried` to `(usize, Option<String>)` lets the dispatch loop retry a different free model after one model's attempt fails, without breaking existing `already_tried`-dependent tests (`model: None` cases) | `src/routing/router.rs` | `dispatch_should_retry_different_model_after_one_model_failure` | Integration | Real `Router::dispatch` loop — Task 3.1.2e |

### Scope: Coding-benchmark ranking table (requirements.md Scope #5)

| Requirement | Test File | Test Name | Type | Scenario |
|-------------|-----------|-----------|------|----------|
| REQ-5: `bench_score(id)` returns `Some(pass_rate / 100.0)` for a tabled model | `src/routing/bench_table.rs` | `bench_score_should_return_pass_rate_for_known_model` | Unit | Happy path — Task 4.1.1c |
| REQ-5: `bench_score(id)` returns `None` for a model absent from the table (fails soft to the neutral default upstream, not fabricated) | `src/routing/bench_table.rs` | `bench_score_should_return_none_for_unranked_model` | Unit | Error path — Task 4.1.1c |

### Scope: Exhaustion behavior (requirements.md Success Metrics #4, Scope #6)

| Requirement | Test File | Test Name | Type | Scenario |
|-------------|-----------|-----------|------|----------|
| REQ-6: when every free-model candidate in the pool is cooling down/rate-limited, dispatch fails with `ProviderError::Exhausted` and does not fall back to a paid upstream | `src/routing/router.rs` | `dispatch_should_return_exhausted_when_all_free_model_candidates_are_cooling_down` | Integration | Real `Router::dispatch` + `HealthRegistry`, all per-model candidates tripped |
| REQ-6: exhaustion increments the same `last_error_kind`/`kind_label() == "exhausted"` attribution the dashboard already shows, so it's visible in aggregate | `src/routing/router.rs` | `dispatch_should_attribute_exhausted_kind_to_dashboard_counters` | Unit | Happy path (regression of existing `kind_label` behavior against openrouter candidates) |

### Scope: Config-load-time / construction-time money-safety validation (architecture-review Blocker 1)

| Requirement | Test File | Test Name | Type | Scenario |
|-------------|-----------|-----------|------|----------|
| REQ-1/Blocker 5: a route with `strategy = "openrouter_scored"` but no `openrouter`-kind upstream fails `from_config`, naming the route | `src/routing/router.rs` | `from_config_should_reject_openrouter_scored_route_without_openrouter_upstream` | Unit | Error path — Task 4.3.1b |
| REQ-1: a route with `strategy = "openrouter_scored"` and an `openrouter`-kind upstream builds a `Router` whose strategy is wired to that upstream's index and cache | `src/routing/router.rs` | `from_config_should_wire_openrouter_scoring_strategy_to_matching_upstream` | Unit | Happy path — Task 4.3.1a |

### Scope: Observability — per-candidate score components + cache state via `to_metrics_json` (requirements.md Observability Requirements, Scope #7)

| Requirement | Test File | Test Name | Type | Scenario |
|-------------|-----------|-----------|------|----------|
| REQ-7: `observability_snapshot()` returns `{cache: {...}, models: {...}}` with one `models` entry per scored model, all 5 fields present | `src/routing/openrouter_scoring.rs` | `observability_snapshot_should_include_all_scored_models` | Unit | Happy path — Task 5.1.1b |
| REQ-7: `GET /metrics` omits `openrouter_scoring` entirely (not `null`) for a `FallbackStrategy` route | `src/entrypoint/observability.rs` | `get_metrics_should_omit_openrouter_scoring_key_for_fallback_strategy` | Unit | Error/edge path — Task 5.1.2c |
| REQ-7: `GET /metrics` includes `openrouter_scoring` matching `observability_snapshot()`'s output for a scored route | `src/entrypoint/observability.rs` | `get_metrics_should_include_openrouter_scoring_block_for_scored_route` | Integration | HTTP handler + real `Router` — Task 5.1.2c |
| REQ-7: `RequestDetail.selected_model` is populated from the chosen candidate's model once selected | `src/routing/router.rs` | `dispatch_should_set_selected_model_on_request_detail` | Unit | Happy path — Task 5.1.3c |
| REQ-7: `RequestDetail.selected_model` stays `None` for a `FallbackStrategy` dispatch (candidates always carry `model: None`) | `src/routing/router.rs` | `dispatch_should_leave_selected_model_none_for_fallback_strategy` | Unit | Error/edge path (regression) — Task 5.1.3c |
| REQ-7: `select()` emits exactly one `tracing::debug!` event per selecting call, naming the model id and its 4 score fields | `src/routing/openrouter_scoring.rs` | `select_should_emit_debug_log_with_score_breakdown` | Unit | Happy path — Task 5.1.4b |

---

### Blocker-fix regression tests (repair-loop findings — explicit, not folded into the rows above)

| Blocker | Test File | Test Name | Type | Scenario (cites plan.md task) |
|---------|-----------|-----------|------|-------------------------------|
| Blocker 1 (adversarial-review): session-pin passthrough — `expand_candidates` must not destroy a `SessionOverrideStore` pin | `src/routing/openrouter_scoring.rs` | `expand_candidates_should_pass_through_unchanged_when_session_pinned` | Unit | Given a candidate `UpstreamRef{index: 2, model: Some("pinned/x:free")}` (as `effective_candidates` produces for a pinned session) and a warm 5-model cache snapshot, `expand_candidates` returns exactly that one candidate unchanged — Task 4.2.4c |
| Blocker 1 (adversarial-review), cold-cache variant | `src/routing/openrouter_scoring.rs` | `expand_candidates_should_pass_through_pinned_candidate_when_cache_cold` | Unit | Same pinned candidate against a `None` cache snapshot still passes through unchanged (not dropped, unlike an unpinned `model: None` candidate) — Task 4.2.4c |
| Blocker 3 (adversarial-review): `cached_count == 1` is unsatisfiable under the general minority-vs-systemic rule and must always invalidate | `src/providers/openrouter/cache.rs` | `record_not_found_and_maybe_invalidate_should_always_invalidate_when_cached_count_is_one` | Unit | Given a cached list of exactly 1 model, a 404 against it invalidates (`last_invalidation_reason == "model_not_found:<id>"`), never `"suppressed_systemic_404"` — Task 2.1.3b/c |
| Blocker 4 (adversarial-review): real invalidation must trigger an immediate on-demand refetch, not wait up to 5 minutes for the next periodic tick | `src/providers/openrouter/cache.rs` | `record_not_found_and_maybe_invalidate_should_trigger_immediate_refresh_on_real_invalidation` | Integration | Given a cache invalidated via the real-staleness branch, `snapshot()` returns `Some(..)` again well within one mock-server round trip, without waiting for the periodic tick — Task 2.1.3d/e |
| Blocker 4 (adversarial-review), single-flight guard | `src/providers/openrouter/cache.rs` | `trigger_immediate_refresh_should_allow_only_one_in_flight_refresh` | Integration | Two real-invalidation triggers within milliseconds of each other produce exactly 1 mock `/models` call, not 2 — Task 2.1.3d/e |
| Blocker 2 (architecture-review / adversarial-review): per-dispatch price recheck — `send()` must verify the *specific selected model's* cached price, not just id membership | `src/providers/openrouter/mod.rs` | `send_should_return_model_unsupported_when_cached_price_is_nonzero` | Unit | Given a model present in the cache snapshot but with a nonzero cached price (constructed via the test-only cache constructor), `send()` returns `ModelUnsupported` with zero mock-server calls recorded — Task 2.1.2c/d |
| Blocker 1 (architecture-review): symmetric kind-vs-strategy validation — an `openrouter`-kind upstream may only be dispatched under `Strategy::OpenrouterScored` | `src/routing/router.rs` | `from_config_should_reject_fallback_route_referencing_openrouter_upstream` | Unit | Given a route `{strategy: Fallback, upstreams: ["or"]}` where `"or"` is `openrouter`-kind, `from_config` returns `Err` naming both the route and the upstream — Task 4.3.1d/e |
| Blocker 1 (architecture-review), `Weighted` direction | `src/routing/router.rs` | `from_config_should_reject_weighted_route_referencing_openrouter_upstream` | Unit | Same as above with `strategy: Weighted` — Task 4.3.1d/e |
| Blocker 1 (architecture-review), mixed-upstream direction | `src/routing/router.rs` | `from_config_should_reject_mixed_upstream_fallback_route_containing_openrouter_upstream` | Unit | A `Fallback` route mixing an `openrouter`-kind upstream with a non-openrouter upstream is still rejected — presence of any openrouter-kind upstream is sufficient — Task 4.3.1d/e |

### Money-safety backstop — post-hoc nonzero-cost detection (Story 1.2.4, not one of the 5 named blockers but part of the same Risk Control layering)

| Requirement | Test File | Test Name | Type | Scenario |
|-------------|-----------|-----------|------|----------|
| A nonzero per-request cost on a nominally-free-routed request hard-invalidates the cache immediately and logs at `error` level | `src/providers/openrouter/mod.rs` | `send_should_hard_invalidate_cache_on_nonzero_cost_for_free_model` | Unit | Happy/detection path (gated on Task 1.2.4a's research finding) — Task 1.2.4c |
| A response with no cost field, or `cost == 0.0`, causes no invalidation and no `error!` event (baseline unaffected) | `src/providers/openrouter/mod.rs` | `send_should_not_invalidate_cache_when_cost_is_zero_or_absent` | Unit | Error/edge (regression) path — Task 1.2.4c |

---

## UX Acceptance Tests

| UX Criterion | Test File | Test Name | Tool | Steps |
|---|---|---|---|---|
| 1. Model attribution: operator determines which model served a request in ≤5s from `recent_requests.selected_model` | Manual checklist | `manual_dashboard_selected_model_lookup` | `curl` + dashboard JSON viewer | Configure an `openrouter`/`openrouter_scored` route, send one chat request, `GET /metrics`, locate the row by `request_id`/`timestamp` in `recent_requests`, confirm `selected_model` is a real free-model id (not `null`) and matches the model the mock/real upstream actually received. |
| 2. Score auditability: operator sees latency/error-rate/bench-rank + composite for every currently-cached free model via one `GET /metrics` call | Manual checklist | `manual_metrics_openrouter_scoring_block` | `curl https://localhost/metrics \| jq .openrouter_scoring` | Send a few requests across at least 2 free models (or wait for cold-start defaults), `GET /metrics`, confirm `openrouter_scoring.models` has one entry per cached model with `latency_p50_ms`, `error_rate`, `bench_rank`, `composite_score`, `sample_count`; confirm an unranked model shows `bench_rank: null`, not `0.0`. |
| 3. Decision auditability: operator answers "why was model X picked" from the `DEBUG` selection log line alone | Manual checklist | `manual_debug_log_selection_reason` | `RUST_LOG=debug` + log tail (`grep`) | Run consolette with `RUST_LOG=debug`, send a request, grep the log for `openrouter candidate selected`, confirm it names the chosen model plus `norm_latency`/`norm_error`/`bench_score`/`composite`, and that exactly one such line appears per request. |
| 4. Fail-closed error is client-visible and named: pool exhaustion returns 529/503 with message `"all upstream candidates exhausted"` | Manual checklist | `manual_pool_exhaustion_error_body` | `curl` against a route whose free-model pool is fully cooled down (e.g. force all candidates' `Retry-After` via repeated mock 429s) | Trip cooldown on every free-model candidate, send a client request, confirm HTTP 529 (Anthropic-shaped) or 503 (OpenAI-shaped) with `message` containing `"all upstream candidates exhausted"` verbatim, and confirm `recent_errors`/upstream counters show a matching `exhausted` spike in the same `GET /metrics` call. |
| 5. Fail-soft is never client-visible: an unranked model or single stale bench/model-list entry never produces a client-facing error | Manual checklist | `manual_unranked_model_no_client_error` | `curl` + log tail + `GET /metrics` | Configure a free model absent from `BENCH_TABLE`, send a request that lands on it, confirm HTTP 200 (normal success), confirm exactly one `WARN` log line (`missing from BENCH_TABLE`) appears (not once per request on repeat), and confirm `openrouter_scoring.models[<id>].bench_rank == null` persists across subsequent `GET /metrics` calls. |
| 6. The two failure modes are distinguishable within 5s of looking | Manual checklist | `manual_distinguish_exhaustion_vs_staleness` | `curl` + `GET /metrics` side by side | Reproduce criterion 4 (exhaustion) and criterion 5 (unranked/stale) in two separate runs; confirm exhaustion shows a client error body *and* an error-kind counter spike, while staleness shows *only* `bench_rank: null`/a log line with the request itself succeeding — confirm an operator can tell which occurred from the response status alone. |
| 7. No dead ends: every error/config-mistake surface names what's wrong specifically enough that the next action is inferable from the message alone | Manual checklist | `manual_no_dead_end_error_messages` | Config edit + `consolette` startup / `curl` | Reproduce 3 cases: (a) stray field under `kind = "openrouter"` → parse error names the field; (b) `strategy = "openrouter_scored"` route with no `openrouter` upstream → startup error names the route; (c) pool exhaustion → response names "exhausted" verbatim. Confirm each message alone (no source read) tells Tyler the next action. |
| 8. Zero behavior change for non-opted-in config: every existing `conf.d/*.toml` continues to parse and dispatch identically | Automated regression (not purely manual) | `existing_conf_d_fixtures_should_parse_and_dispatch_unchanged` | `cargo test` over existing fixture-based config tests in `src/config/schema.rs`/`src/routing/router.rs` | Run the full pre-existing config-parsing and dispatch test suite unmodified after this feature lands; confirm zero regressions (ties to Migration Plan's "no existing valid config becomes invalid"). |
| 9. Accessibility | N/A | — | — | Not applicable — no interactive/visual surface introduced (design/ux.md Scope note). |

---

## Test Stack
- **Unit**: Rust's built-in `#[test]` / `#[tokio::test]` harness (`cargo test`), `assert!`/`assert_eq!` — matching every existing test module in this codebase (`src/**/*.rs`'s `#[cfg(test)] mod tests` blocks). No new assertion library.
- **Integration**: same harness, plus a mock HTTP server for OpenRouter's `/models` and `/chat/completions` endpoints (the wiremock-or-equivalent pattern already used by `src/providers/openai.rs`'s and `src/providers/gemini/mod.rs`'s test modules — reused, not a new dependency), and `tracing-test`-or-equivalent (checked for existing in-repo precedent first per plan.md Task 5.1.4b) for asserting log events.
- **E2E / UX**: manual checklist (`curl`/`jq` against `GET /metrics`, log tail with `RUST_LOG=debug`, and config-file edits) — this feature's only user-facing surfaces are non-interactive JSON/TOML/log surfaces (design/ux.md Scope note), so no browser/UI automation applies.

## Coverage Targets and How to Measure

| Stack | Coverage command | Target |
|---|---|---|
| Rust | `cargo tarpaulin --out Stdout` | ≥80% line |

- All public service methods (`OpenrouterProvider::{new,send,list_models,list_free_models}`, `ModelListCache::{snapshot,refresh,invalidate,record_not_found_and_maybe_invalidate,trigger_immediate_refresh}`, `OpenrouterScoringStrategy::{select,expand_candidates,record_outcome,observability_snapshot}`): happy path + error paths covered per the mapping above.
- All external integrations (OpenRouter `/models`, OpenRouter `/chat/completions`): unit mocked (Task 1.2.1e, 1.2.2d) + at least one integration test each (Task 1.2.3d/4.3.1c, Task 2.1.2d).
- UX acceptance criteria: all 9 consolidated criteria in design/ux.md have a corresponding manual-checklist row above (criterion 9 explicitly N/A).

## Migration test
N/A — no data/schema migration in this feature (config-schema addition only, per plan.md's Migration Plan section: additive enum variants, `deny_unknown_fields` unchanged, no on-disk state format changes). Skipped per Step 5.
