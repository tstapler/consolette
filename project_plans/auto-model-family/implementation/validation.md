# Validation Plan: auto-model-family

**Date**: 2026-09-12

## Happy Path Scenario (one sentence)

Given today's two pinned :free models in fallback, when a client sends the family alias, then the proxy serves the healthiest member with zero manual route edits.

## Requirement → Test Mapping

| Requirement | Test File | Test Name | Type | Scenario |
|---|---|---|---|---|
| R1 alias resolution ranked error-rate then latency | `src/routing/family.rs` (inline `#[cfg(test)]`) | `family_resolver_should_pick_lowest_error_member_when_both_healthy` | unit happy | `FamilyResolver` ranks two `FamilyMember`s from `MemberStats` (`QualityError` EWMA 12.5% n=8 vs 0% n=10); B wins |
| R1 alias resolution ranked error-rate then latency | `src/routing/family.rs` (inline `#[cfg(test)]`) | `family_resolver_should_return_no_candidate_when_alias_unknown` | unit error | Unknown `FamilyAlias` yields empty ranking / `QualityError::UnknownAlias`; dispatch leaves body untouched |
| R1 alias resolution ranked error-rate then latency | `tests/family_resolution.rs` | `dispatch_should_route_to_healthiest_member_when_family_alias_received` | integration | `FamilyTable` with 2 mock-upstream members on a `fallback` family route; `POST /v1/messages` with `model="auto-coding"`; outgoing body carries B's ID and a `ResolutionSnapshot` is published |
| R1 cold-start / stability signals | `src/routing/family.rs` (inline `#[cfg(test)]`) | `family_resolver_should_serve_cold_start_default_when_samples_below_threshold` | unit happy | No member meets minimum samples → `ColdStartDefault` (config-order first healthy) flagged `cold` |
| R1 cold-start / stability signals | `src/routing/family.rs` (inline `#[cfg(test)]`) | `family_resolver_should_keep_incumbent_when_challenger_margin_below_hysteresis` | unit error | Challenger 5% better on latency with `HysteresisMargin` requiring 10% → pick stays (no flap) |
| R1 cold-start / stability signals | `tests/family_resolution.rs` | `dispatch_should_probe_non_pick_member_when_probe_request_due` | integration | 100 dispatched family requests with probe-every-25th; ≥3 carry an `ExplorationProbe` to a non-pick member (excluding denylisted/cooled) |
| R2 opencode provider family IDs | `src/entrypoint/chat_completions.rs` (inline `#[cfg(test)]`) | `chat_completions_should_carry_family_alias_into_dispatch_when_model_is_auto_coding` | unit happy | `FamilyAlias` survives `translate_openai_to_anthropic` into dispatch (overwrite tested downstream, not pre-translate interception) |
| R2 opencode provider family IDs | `src/routing/family.rs` (inline `#[cfg(test)]`) | `family_table_should_leave_alias_untouched_when_route_has_no_family_field` | unit error | Active route without `family` field + `model="auto-coding"` → no expansion (verbatim leak per pin semantics) |
| R2 opencode provider family IDs | `tests/family_opencode_path.rs` | `chat_completions_should_overwrite_alias_with_resolved_id_when_family_route_active` | integration | `POST /v1/chat/completions` with `model="auto-coding"` against mock upstreams; upstream never sees the verbatim alias; `GET /api/models` surfaces the alias with family label |
| R3 dashboard live pick + why | `src/metrics/member_stats.rs` (inline `#[cfg(test)]`) | `resolution_snapshot_should_record_pick_and_margins_when_resolution_completes` | unit happy | After a ranked resolution, `ResolutionSnapshot` holds chosen member + error-rate/latency figures + timestamp + previous pick |
| R3 dashboard live pick + why | `src/entrypoint/observability.rs` (inline `#[cfg(test)]`) | `metrics_family_section_should_report_cold_status_when_no_resolutions_yet` | unit error | Zero resolutions → `family` section shows alias with `cold` status, not an error |
| R3 dashboard live pick + why | `tests/family_metrics.rs` | `metrics_should_expose_resolutions_and_picks_when_alias_resolves_repeatedly` | integration | 3 resolutions (A,A,B) → `GET /metrics` shows `resolutions_total=3`, `current_pick=B`, `previous_pick=A`; existing `providers` sections byte-identical in shape |
| R4 free-only default | `src/config/validate.rs` (inline `#[cfg(test)]`) | `free_guard_should_accept_free_suffixed_id_when_pricing_unknown` | unit happy | `FreeGuard` fail-open: rotated unknown `cohere/new-model:free` (absent from vendored pricing snapshot) loads clean in a free family |
| R4 free-only default | `src/config/validate.rs` (inline `#[cfg(test)]`) | `free_guard_should_reject_paid_member_when_allow_paid_false` | unit error | Free family listing `anthropic/claude-paid-model` (or pricing-unknown non-`:free` ID → fail-closed) fails with `ConfigError::PaidMemberInFreeFamily` naming alias + member |
| R4 free-only default | `tests/family_free_guard.rs` | `post_route_should_reject_paid_member_when_hot_swapped_into_free_family` | integration | `POST /api/route` injecting a paid ID into the free family → 400 naming alias + member (mirrors `post_route_rejects_unknown_upstream`); config with unknown upstream member rejected with upstream name |
| R5 paid opt-in alias | `src/routing/family.rs` (inline `#[cfg(test)]`) | `family_resolver_should_resolve_paid_member_when_allow_paid_true` | unit happy | `auto-coding-paid` with `allow_paid=true` resolves a paid ID; paid-resolutions counter increments |
| R5 paid opt-in alias | `src/routing/family.rs` (inline `#[cfg(test)]`) | `safety_net_bypass_should_never_serve_paid_id_when_free_members_all_down` | unit error | `SafetyNetBypass` (cooldown/empty-pool scope) serves least-bad free member + WARN + `fallback_to_default_total++`; never a paid ID, never overrides 429 cooldowns, never masks 404/auth/validation |
| R5 paid opt-in alias | `tests/family_paid_alias.rs` | `dispatch_should_keep_free_and_paid_stats_separate_when_both_aliases_resolve` | integration | Traffic on both aliases against mock upstreams; per-alias counters separate; free↔paid `MemberStats` tables don't leak |
| R6 session-aware pins | `src/routing/router.rs` (inline `#[cfg(test)]`) | `session_pin_should_win_over_family_resolution_when_pin_exists` | unit happy | `SessionPin` (`s1` → `model-a:free`) beats a `FamilyResolver` pick of B via `effective_candidates` single-candidate bypass |
| R6 session-aware pins | `src/routing/session_overrides.rs` (inline `#[cfg(test)]`) | `extract_session_id_should_return_none_when_metadata_missing` | unit error | Opencode-shaped body without `metadata.user_id`/`user` → no pin applies; family resolution proceeds (documents the dropped-key failure mode Epic 4 fixes) |
| R6 session-aware pins | `tests/family_sessions.rs` | `dispatch_should_stick_to_pin_then_resume_family_when_pin_cleared` | integration | `s1` pinned via `POST /api/sessions/s1/route` sticks to A on `/v1/messages` while others resolve dynamically; `DELETE /api/sessions/s1/route` → family resumes; opencode-shaped `/v1/chat/completions` with session key also sticks (adapter carries `metadata`) |
| R1 resolution overhead budget | `tests/family_perf.rs` | `resolution_overhead_should_stay_under_1ms_p99_when_family_has_8_members` | Integration (benchmark-style) | Rank + dispatch-seam timing vs static-pin baseline at family sizes ≤8; asserts p99 ≤1ms (Story 3.3 AC4 gate before rollout) |
| R6 sticky sessions (K=50) | `tests/family_sessions.rs` | `session_should_stick_then_reevaluate_at_K50_when_no_health_event` | Integration | 60 family requests on one session: first 50 all serve pick A, request 51+ re-resolves (picks B once B outranks); cooldown event triggers immediate re-eval |

Requirements key: R1 = per-request ranked resolution (incl. `ColdStartDefault`, `HysteresisMargin`, `ExplorationProbe`, `SafetyNetBypass`, `ResolutionSnapshot`); R2 = opencode family IDs; R3 = dashboard/why via `ResolutionSnapshot`; R4 = `FreeGuard` free-only; R5 = paid alias; R6 = `SessionPin` precedence + dynamic pin/move.

Extra dispatch-exclusion coverage (Story 3.2, counted under R1/R6, not separate requirements): first-request-after-delist fails then denylists (`Validation(_,404)` only; 400 never quarantines; `ModelUnsupported` feeds denylist); pre-dispatch exclusion skips dead IDs; shared-upstream 429 keeps the sibling member eligible; route-gate + hot-swap rollback (family route → pinned route restores pins) in `tests/family_resolution.rs`; same-upstream per-`(upstream, model)` attempt tracking; weighted-strategy family rejected / forced-`FallbackStrategy`; `FamilyRuntime` (stats, 1h-TTL denylist, snapshots, counters, probe/hysteresis state) survives `post_route` rebuild keyed by `(upstream_name, model_id)`; 429/auth/validation excluded from `QualityError` rate per the per-class write table.

## UX Acceptance Tests

| UX Criterion | Test File | Test Name | Tool | Steps |
|---|---|---|---|---|
| 1 Glance | manual | `dashboard_should_show_pick_and_why_above_stat_cards` | browser + eye | 1. Open `GET /dashboard`. 2. Confirm family card is above stat-cards. 3. Read pick + `err %` + `p50` within 5s, no scroll |
| 2 Drill | manual | `member_table_should_rank_label_and_allow_copy` | browser + clipboard | 1. Read ranked member table with `active/cooldown/excluded:<reason>` labels. 2. Copy a member ID as selectable text. 3. Confirm excluded rows greyed AND labeled |
| 3 Rollback link | manual | `route_link_should_round_trip_to_pinned_traffic` | browser + curl | 1. Click card's `GET /api/route` link. 2. `POST /api/route` pinned route; confirm traffic restored without restart and card reflects pinned state |
| 4 Cold start | manual | `card_should_show_cold_start_default_on_fresh_stats` | browser + curl | 1. Fresh stats (restart). 2. Confirm `Cold start — serving config-order default (<model-id>)` with `cold-start-default` label (never `0%`/empty); clears after N requests |
| 5 All-down bypass | manual | `banner_should_show_bypass_and_never_serve_paid` | curl + browser | 1. Force all members unhealthy (cooldown). 2. Confirm banner `All auto-coding members unhealthy — bypassed cooldown and served <model-id> at <time>` with red border + text label. 3. Follow rollback hint; confirm free alias never served a paid ID |
| 6 Paid-selected | manual | `paid_card_should_stand_apart_with_counter` | browser + curl | 1. Select paid alias. 2. Confirm distinct `PAID — may spend` card + `paid resolutions: N` increments |
| 7 Delisted member | manual | `delisted_row_should_grey_with_reason_while_traffic_flows` | curl + browser | 1. Delist one member (404). 2. Confirm greyed row `excluded: not in catalog (last checked <time>)` + pick line notes `dead IDs excluded before dispatch`; traffic unaffected |
| 8 Tie/flap | manual | `pick_should_stay_stable_across_polls_on_near_tie` | browser (2×30s polls) | 1. Force near-tie stats. 2. Confirm `last change` + `previous pick` visible and pick stable across polls (hysteresis, no oscillation) |
| 9 Stale stats | manual | `card_should_always_show_stats_window_age` | browser | 1. Read card's `stats window: last N reqs / since <time>` line; age of signal always visible |
| 10 CDN-blocked | manual | `card_should_render_text_with_charts_broken` | browser (block `cdn.jsdelivr.net`) + reload | 1. Block CDN, reload dashboard. 2. Confirm pick + err % + p50 fully readable as server-rendered HTML with charts broken |
| 11 No color-only | manual | `status_rows_should_pair_every_dot_with_text` | browser (grayscale/devtools) | 1. Inspect each status row with color ignored. 2. Every dot paired with a text label |

## Test Stack

- Unit: `cargo test` inline (`#[cfg(test)] mod tests` in the listed `src/` modules; pure `FamilyResolver`/`FreeGuard`/`MemberStats` logic, no I/O).
- Integration: `cargo test --test family_*` in `tests/` with mock-upstream doubles and real dispatch/config/`POST /api/route`/`GET /metrics` paths; external integrations (upstreams, pricing snapshot via vendored `PricingTable::load_default`) mocked except ≥1 integration each exercised through the HTTP path.
- E2E/UX: manual + curl checklist above (dashboard is local HTML; no browser-automation stack in repo).

## Coverage Targets

- Rust: `cargo tarpaulin --out Stdout`, ≥80% line on touched modules (`src/routing/family.rs`, `src/metrics/member_stats.rs`, `src/config/validate.rs`, affected dispatch/session/metrics/dashboard paths).
- All public items (`FamilyTable::from_config`, `FamilyResolver::rank`, `MemberStats::record`, `FreeGuard` validation, snapshot/counter publishers) get happy + error tests.
- External integrations mocked + ≥1 integration test each through the real HTTP path (`/v1/messages`, `/v1/chat/completions`, `/metrics`, `/dashboard`, `POST /api/route`).
- Each `ux.md` criterion (§3, 11 items) mapped above; no criterion unmapped.

## Migration

N/A — stats are in-memory only (`FamilyRuntime` owned by `MetricsCollector`); restarts reset rankings to `ColdStartDefault` by design. No schema/data migration exists, so no migration test is written.
