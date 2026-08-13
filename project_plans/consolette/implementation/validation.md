# Validation Plan: Consolette

**Date**: 2026-07-17
**Status**: Phase 4 — test design (pre-implementation)
**Inputs**: `requirements.md` (FR-1..FR-7, NFR-1..6, CD-1..CD-5), `implementation/plan.md`,
`implementation/adversarial-review.md`, `implementation/architecture-review.md`, `research/*.md`

This plan designs the test suite **before** any code is written. Every requirement maps to
at least one test. Tests that the reviews specifically called out as gaps are flagged
**[review]**. Tests that cannot run unattended (VPN / SBN Dev Agent / Gandalf dependencies)
are flagged **[MANUAL]** and must NOT gate CI or the soak.

---

## Test Stack

- **Unit**: Rust `#[cfg(test)]` modules + `cargo test`. Assertions via std `assert!/assert_eq!`.
- **Config fixtures**: `tempfile` (already a dev-dependency) for building throwaway `conf.d/`
  directories; small `.toml` fixtures under `tests/fixtures/conf.d/`.
- **Rate-limiter time control**: `governor::clock::FakeRelativeClock` — `UpstreamLimiter` must
  be generic over `governor::clock::Clock` so refill windows are advanced deterministically
  (no wall-clock sleeps). **[review S6]**
- **Auth resolution**: a `SecretResolver` trait (env + keychain impls + a test double) so
  `bearer`/`apikey` resolution is unit-testable without shelling to `security` or mutating
  process-global env. **[review S7]**
- **HTTP integration**: `wiremock` (add as dev-dependency) or a hand-rolled `axum` stub
  upstream to assert routing/failover/weighting/rate-limit behavior against fake upstreams
  without real providers. Reuse the existing integration-test style in `tests/`.
- **Schema portability**: a `python3 -c 'import tomllib; ...'` harness step over every shipped
  `conf.d/*.toml` fixture (CD-1 / NFR-2). **[review S5 / adversarial]**
- **Ansible**: `ansible-playbook --check` + a double-run idempotency assertion (second run
  reports `changed=0` for the consolette block).

## Coverage Targets

- Unit line coverage ≥80% on new modules (`config/`, `upstream/`, `routing/`, `ratelimit/`).
- Every `AuthMethod` variant: happy path + failure path.
- Both routing strategies: happy path + all-unhealthy path.
- Rate limiter: shed path + delay path + per-upstream independence.
- No behavioral regression vs today's Anthropic→Bedrock default (golden test).

---

## Requirement → Test Mapping

### FR-1: Layered config.d configuration

| Req | Test name | Type | Scenario |
|---|---|---|---|
| FR-1.1 | `conf_d_merges_in_lexical_order_later_wins` | Unit | Two files `00-*.toml`/`10-*.toml` set same scalar → later file wins; glob is explicitly sorted (not FS order). |
| FR-1.1 | `conf_d_deep_merges_tables_but_replaces_arrays` | Unit | Nested tables union across files; a redeclared `[[upstreams]]` array in a later file **replaces** the earlier array (documents the arrays-replace rule). |
| FR-1.2 | `env_overrides_file_values_at_highest_precedence` | Unit | `CONSOLETTE_PORT` env beats a `port` set in conf.d beats the built-in default. |
| FR-1.2 | `env_overlay_restricted_to_allowlist` | Unit | An env var outside the allowlist does NOT mutate nested `upstreams`/`routes`. **[review]** |
| FR-1.3 | `empty_conf_d_yields_working_defaults` | Unit | No files present → default config equals today's env-only defaults (back-compat). |
| FR-1.4 | `all_example_conf_d_parse_in_python_tomllib` | Integration | `tomllib.load` succeeds on every shipped `00/10/20-*.toml` fixture; build fails on error. **[review S5]** |
| FR-1.5 | `bad_toml_error_names_offending_file` | Unit | Malformed TOML → error message contains the file path. |
| FR-1.5 | `unknown_key_rejected_by_deny_unknown_fields` | Unit | A typo'd key (`upstreems`) → hard error naming the field. |
| FR-1.5 | `route_referencing_unknown_upstream_fails_fast` | Unit | `validate_references()` errors naming the route + missing upstream. |
| FR-1.5 | `route_referencing_unknown_auth_type_fails_fast` | Unit | `validate_references()` rejects an `auth.type` outside `bearer`/`apikey`/`exec` naming the route + offending value. (No `internal` variant exists — superseded by ADR-007.) |
| FR-1.6 | `config_wrapped_for_reload` (scoped per ADR-001 final) | Unit | Config accessed via the chosen wrapper; if ArcSwap retained, a swap of the derived runtime bundle is atomic and `HealthRegistry` persists; if plain `Arc<Config>`, assert reload is documented out-of-scope. **[review M2]** |
| CD-1/NFR-2 | `config_schema_is_tomllib_portable` | Integration | Same as FR-1.4 — the shared-schema constraint is executably enforced, not asserted. |

### FR-2: Upstream + pluggable auth abstraction

| Req | Test name | Type | Scenario |
|---|---|---|---|
| FR-2.1 | `upstream_parses_name_kind_baseurl_auth` | Unit | `[[upstreams]]` with each `kind` (anthropic/bedrock/openai) deserializes. |
| FR-2.2 | `bearer_auth_injects_authorization_header` | Unit | `auth=bearer` → `Authorization: Bearer <resolved>` on the outbound request (via `SecretResolver` double). |
| FR-2.2 | `apikey_auth_injects_configurable_header` | Unit | `auth=apikey` → key in the configured header (default `x-api-key`; `Authorization` for OpenAI-style). |
| FR-2.2 | `exec_auth_runs_helper_and_applies_headers` | Unit | `auth=exec` spawns the configured helper via a fake-command double, applies the JSON-returned headers, and treats non-zero exit/timeout/unparseable output as upstream-unavailable (ADR-007). |
| FR-2.3 | `secret_ref_env_resolves_without_leaking` | Unit | Env-ref secret resolves to value; `Debug`/display never prints the value. |
| FR-2.3 | `secret_ref_keychain_resolves_via_resolver_double` | Unit | Keychain ref resolves through the mockable `SecretResolver` (no real `security` call). **[review S7]** |
| FR-2.3/NFR-6 | `config_redacted_in_dashboard_and_metrics` | Unit | Rendering config in `/metrics`, `/health`, `dashboard.rs` masks auth blocks. **[review]** |
| FR-2.3/NFR-6 | `non_dummy_inline_secret_warns_or_rejects` | Unit | `SecretRef::Inline` with a value other than `sk-dummy`-style placeholder → warn/reject. **[review]** |
| FR-2.4 | `default_config_reproduces_anthropic_then_bedrock` | Integration | With empty conf.d, request flow = Anthropic primary, Bedrock on 429 — **golden no-regression test**. |
| FR-2.5 | `openai_upstream_forwards_to_arbitrary_base_url` | Integration | `kind=openai` with a custom `base_url` posts an OpenAI Chat Completions body to the stub. |
| FR-2.5/S3 | `openai_provider_translates_anthropic_body_roundtrip` | Integration | Router passes canonical Anthropic body → `OpenAiProvider` translates out to OpenAI and back to Anthropic response shape. **[review S3]** |

### FR-3: Routing strategies (fallback | weighted)

| Req | Test name | Type | Scenario |
|---|---|---|---|
| FR-3.1 | `route_matches_by_model_and_endpoint` | Unit | A request's model/endpoint selects the correct route. |
| FR-3.2 | `fallback_strategy_selects_first_healthy` | Unit | `FallbackStrategy::select` returns the first healthy candidate (config order). |
| FR-3.2 | `fallback_reproduces_current_primary_then_fallback` | Integration | Primary 429 → next upstream tried, in order (identical to today). |
| FR-3.3 | `weighted_strategy_splits_by_configured_ratio` | Unit | Over N samples with a seeded RNG, selection frequency ≈ weights (tolerance band). |
| FR-3.3 | `weighted_redistributes_weight_around_unhealthy` | Unit | A cooled-down upstream is excluded; survivors keep relative proportions. |
| FR-3.4 | `cooldown_applies_to_fallback_and_weighted` | Unit | A 429 trips `HealthRegistry`; both strategies skip the upstream until cooldown expires. |
| FR-3.4 | `retry_after_header_sets_cooldown_duration` | Unit | `Retry-After` overrides the default 300s cooldown (ADR-006 behavior preserved). |
| FR-3.5 | `bedrock_never_enters_cooldown` | Unit | Per-upstream `can_cooldown=false` keeps Bedrock always selectable. |
| FR-3.5 | `dispatch_loop_reuses_error_class_arms` | Unit | 4xx validation/401 auth → no failover (return); 429/transient → failover. |
| FR-3.6 | `all_unhealthy_returns_503_with_retry_guidance` | Integration | Every upstream cooling → 503 with retry message. |
| FR-3 (stream) | `streaming_failover_only_before_first_byte` | Integration | Connect-time 429 on a streaming request → failover; mid-stream error → SSE error event, no failover. **[review]** |
| NFR-1 | `openai_endpoints_use_router` | Integration | `/chat/completions` + `/v1/chat/completions` (`handle_openai_compat`) dispatch through `Router` (weighted/rate-limit/gateway apply); `handle_dry_run` + `/v1/models` still resolve. **[review BLOCKER]** |

### FR-4: Per-upstream rate limiting

| Req | Test name | Type | Scenario |
|---|---|---|---|
| FR-4.1 | `rpm_and_tpm_parse_from_ratelimit_toml` | Unit | `[ratelimit.upstreams.<name>]` rpm/tpm/on_breach/max_delay_ms deserialize; defaults inherited. |
| FR-4.2 | `rate_limit_is_per_upstream_independent` | Unit | Exhausting upstream A's bucket does not throttle upstream B (FakeRelativeClock). |
| FR-4.3 | `on_breach_shed_returns_shed_and_router_falls_through` | Unit+Integration | Breach with `shed` → `Admit::Shed` → router adds to `already_tried` and re-selects (reuses cooldown-exclusion path). |
| FR-4.3 | `on_breach_delay_waits_then_admits_or_sheds_on_timeout` | Unit | `delay` waits up to `max_delay_ms` (fake clock) then admits; exceeding it sheds. |
| FR-4.4 | `tpm_charged_by_tiktoken_estimate` | Unit | `check_n(est_tokens)` charged from the tiktoken-rs estimate; estimation gated to when a TPM limiter exists (NFR-3). **[review minor]** |
| FR-4.4 | `tpm_request_exceeding_burst_is_shed_not_errored` | Unit | `InsufficientCapacity` (est > burst) → shed, not a hard error. |
| FR-4.5 | `ratelimit_decisions_visible_in_metrics` | Unit | allowed/shed/delayed/tokens_charged per upstream appear under `/metrics` `"ratelimit"`. |

### FR-5: Internal Model Gateway upstream

| Req | Test name | Type | Scenario |
|---|---|---|---|
| FR-5.1 | `model_gateway_upstream_config_parses` | Unit | The shipped example `model-gateway` upstream (`kind=openai`/anthropic, base_url=local agent, `auth=bearer` dummy) parses and validates. |
| FR-5.1 | `model_gateway_end_to_end_completion` | Integration | Real completion through `localhost:9123/proxy/{PROJECT}`. **[MANUAL — needs VPN + SBN Dev Agent + go/modelgateway project + Gandalf policy]** |
| FR-5.2 | `gateway_anthropic_v1_messages_passthrough` | Integration | Claude Code `/v1/messages` routed to the gateway catch-all (no OpenAI translation). **[MANUAL]** |
| FR-5.1 | `gateway_startup_probe_distinguishes_failures` | Unit+Integration | Startup reachability probe emits distinct actionable errors: agent-down (:9123 closed) vs 401 (Gandalf) vs 404 (project). **[review]** |
| FR-5.3 | (doc check) | Manual | Docs state base URLs/ports, endpoint paths, VPN+project+Gandalf prereqs, and the internal-identity-system mTLS follow-up. |

### FR-6: Rename claude-proxy-rs → consolette

| Req | Test name | Type | Scenario |
|---|---|---|---|
| FR-6.1 | `cargo_build_produces_consolette_and_mcp_proxy` | Integration | Both binaries build under the new crate name. |
| FR-6.2 | `plist_carries_full_env_set` | Unit/lint | Rendered `com.consolette.plist` includes `CLAUDE_CODE_OAUTH_TOKEN`, `AWS_PROFILE`, `AWS_REGION`, and all tuning vars — not just PORT/HOME/PATH; the port var name matches what the loader reads. **[review BLOCKER]** |
| FR-6.2 | `migrate_unloads_old_loads_new_no_double_bind` | Integration | `make migrate`/ansible unloads `com.claude-proxy-rs` before loading `com.consolette` (both bind 47000); `/health` returns ok after. **[review BLOCKER]** |
| FR-6.3 | `config_dir_log_makefile_use_new_name` | Unit | Paths reference `~/.config/consolette`, `/tmp/consolette*.log`, `com.consolette`. |
| FR-6.4 | `all_existing_features_preserved_after_rename` | Integration | Compression, cache-aligner, verbosity, memory, metrics, dashboard, mcp-gateway endpoints still function (smoke suite). |
| FR-6.4/back-compat | `every_legacy_env_var_still_takes_effect` | Unit | Each of `PROXY_PORT, COOLDOWN_SECONDS, REQUEST_TIMEOUT, BEDROCK_MAX_RETRIES, STAPLER_COMPRESS, COMPRESS_FLOOR_BYTES, CACHE_ALIGNER, VERBOSITY_LEVEL, MEMORY_MAX_ENTRIES, AWS_PROFILE, AWS_REGION, CLAUDE_CODE_OAUTH_TOKEN` still applies (mapped or default-baked); `AWS_REGION` vs bedrock `options.aws_region` precedence asserted. **[review BLOCKER]** |

### FR-7: ndotfiles / ansible install

| Req | Test name | Type | Scenario |
|---|---|---|---|
| FR-7.1 | `ansible_block_builds_renders_loads` | Integration | The block runs `cargo build --release`, renders the plist template, and `launchctl load`s (mirrors the aimee block). |
| FR-7.2 | `cfgcaddy_links_conf_d_without_vendor_prefix` | Unit | `.cfgcaddy.yml` links `conf.d/*.toml` → `~/.config/consolette/conf.d/` (no `vendor-` prefix). |
| FR-7.3 | `ansible_block_is_idempotent` | Integration | Second `ansible-playbook` run reports `changed=0` for the consolette block; no duplicate agent; running instance untouched. |
| FR-7.3 | `old_agent_unload_gated_on_presence` | Integration | Unload task runs only when `launchctl list com.claude-proxy-rs` succeeds; new agent loads only after old is stopped. **[review]** |
| FR-7.4 | `plist_template_parameterized` | Unit | `.plist.j2` renders port, binary path, and log paths from variables. |

### Cross-cutting / NFR

| Req | Test name | Type | Scenario |
|---|---|---|---|
| NFR-1 | `wire_compatible_claude_code_and_openai` | Integration | `/v1/messages` and `/(v1/)chat/completions` behave unchanged for clients. |
| NFR-3 | `routing_decision_is_o_routes_no_latency_regression` | Bench/smoke | Per-request routing overhead negligible; startup <100ms, idle <50MB preserved. |
| NFR-4 | `bad_single_route_does_not_crash_proxy` | Unit | One misconfigured route/upstream fails fast at startup with a clear message; runtime degrades gracefully. |
| NFR-5 | `clippy_deny_warnings_clean` | CI | `cargo clippy --deny warnings` passes on new modules. |
| N9 | `adr006_deviation_recorded` | Doc | ADR notes the `RwLock`→`DashMap` health-state change is traceable. |
| C10 | `dashmap6_all_call_sites_compile` | Integration | After the 5→6 bump, `cache.rs`, `slots.rs`, `memory/`, `metrics/`, `mcp-proxy` all compile+test (standalone early gate). **[review]** |

---

## Readiness Gate Dependencies (for Phase 4 gate)

1. Every FR-1..FR-7 requirement above has ≥1 mapped test — **satisfied by this document**.
2. Blocker-driven tests exist for: full plist env (`plist_carries_full_env_set`), complete
   legacy-env shim (`every_legacy_env_var_still_takes_effect`), OpenAI-on-router
   (`openai_endpoints_use_router`) — these correspond to the three adversarial BLOCKERs and
   must be present in the final plan's acceptance criteria.
3. The `[MANUAL]` Model Gateway end-to-end tests are explicitly excluded from CI/soak gating
   (unattended runs cannot satisfy VPN + SBN Dev Agent + Gandalf).

## Open Testing Risks

- **Model Gateway e2e is not automatable** — real verification is a manual checklist gated on
  VPN + local agent + project membership. CI can only assert config parse + startup probe.
- **TPM accuracy** — tiktoken-rs is an estimate for non-OpenAI models; TPM tests assert
  relative behavior (shed/independence), not exact token accounting.
- **Weighted split is probabilistic** — assert frequency within a tolerance band using a
  seeded RNG; do not assert exact counts.
- **Idempotency of `cargo build` in ansible** — prefer letting cargo decide `changed` (avoid
  a fragile mtime reimplementation) so the idempotency test is meaningful.
