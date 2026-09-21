# Stack Research: auto-model-family

**Date**: 2026-09-12
**Scope**: synthetic family alias (e.g. `auto-coding`) resolved per-request to best real model ID by local error-rate then latency.

## 1. Existing in-repo modules that apply

| Concern | File | What exists today |
|---|---|---|
| Dispatch loop | `src/routing/router.rs:135-294` (`from_config`, `effective_candidates`, `dispatch`) | `from_config` builds `Vec<UpstreamRef>` from `Route.upstreams` by name lookup (`position()` on `Config.upstreams`), picks `FallbackStrategy`/`WeightedStrategy`, wires `HealthRegistry` + `RateLimiters` + `MetricsCollector`. `dispatch` reads `body["model"]` (`router.rs:260-264`), applies session-pin override, filters `!already_tried && health.is_available`, calls `strategy.select(&healthy)`, then per-attempt model-override substitution (see `dispatch_overrides_model_field_when_upstream_pins_one`, `router.rs:956`). **Family resolution slots in here**: rewrite the alias `model` to a concrete member ID *before* the healthy-filter/select step, so a delisted (404) member is excluded pre-dispatch — failover cannot save it since validation/auth errors return immediately with no failover (file header `router.rs:1-9`). |
| Selection trait | `src/routing/strategy.rs:11-64` | `UpstreamRef { index, name, weight, model }` (`index` = position in `Config.upstreams`, shared key with `HealthRegistry`). `trait RoutingStrategy: Send + Sync { fn select(&self, healthy: &[UpstreamRef]) -> Option<UpstreamRef> }`. `FallbackStrategy` = first-healthy; `WeightedStrategy` = `rand 0.8` `WeightedIndex` over `weight*1000 as u64` (`.max(1)` uniform fallback). New `AdaptiveFamilyStrategy` (or a pre-pass that reorders `healthy` by stats) implements this same trait — pure, health-blind, unit-testable without I/O. |
| Cooldown / availability | `src/routing/health.rs:1-80` | `HealthRegistry { state: DashMap<usize, ProviderState>, cooldown_duration, can_cooldown }`. `trip(idx, override)`, `is_available(idx)`, `remaining_secs(idx)`. Hard rule: never hold a `DashMap` guard across `.await` (all ops sync `Instant` compares). Bedrock opted out via `set_can_cooldown(idx,false)` (`router.rs:152-157`). Family ranking composes with this: stats rank *within* the health-filtered set; cooldown stays the hard-exclusion signal. |
| Stats source (lifetime) | `src/metrics/counters.rs:15-31,44-46,221-266` | `ProxyMetrics { upstreams: DashMap<String, UpstreamCounters> }`, `UpstreamCounters { requests/success/errors: AtomicU64, duration_sum_ms/count, first_byte_sum_ms/count, last_error_kind: Mutex<Option<&'static str>> }`. `upstream_json()` emits `providers` (requests/success/errors) + `provider_latency` (avg_duration_ms, avg_first_byte_ms) for `/metrics`. **Gap (matches requirements rabbit hole)**: keyed by upstream *name*, lifetime sums, no per-model dimension, no decay — a once-bad model stays penalized forever. |
| Stats source (windowed) | `src/metrics/histogram.rs:1-50` | `DurationHistogram { samples: Mutex<VecDeque<(Instant,u64)>>, window: Duration }`, default 15 min, trims on every `record()`, `percentiles() -> (p50,p95,p99)`. This is the in-repo template for any windowed family-stats store — same `Mutex<VecDeque<(Instant, _ Vass)>>` shape, no new dependency. |
| Error taxonomy | `src/metrics/error_tracker.rs:1-80`, `counters.rs:200-215` | `normalize_message`/`extract_signature`/`compute_fingerprint` (SHA-256 `provider:operation:error_type:msg`), 100-entry ring buffer; `record_error_kind` splits `timeout/auth/rate_limit/validation` via `ProviderError::is_auth/is_rate_limited/is_validation`. Family error-rate should count only *quality* failures (transient/upstream/5xx + timeouts), excluding auth/validation — same predicate set the router's no-failover branch uses. |
| Config schema | `src/config/schema.rs:149-166` (`RouteUpstreamRef`, `Route`) | `RouteUpstreamRef { name, weight: Option<f64>, model: Option<String> }` — today's pin is `model`. `Route { name, strategy: Fallback|Weighted, upstreams: Vec<RouteUpstreamRef> }`, `deny_unknown_fields`. Family membership most naturally lands as a new struct (e.g. `Family { alias, members: Vec<ModelMember>, ... }`) or an extended ref variant, keeping `RouteUpstreamRef` untouched so pinned entries remain a working fallback (rollback requirement). `UpstreamKind::Openai { base_url }` (`schema.rs:117-119`) is the OpenRouter wire. |
| Hot-swap / rollback | `src/entrypoint/api.rs:78-133`, `src/entrypoint/mod.rs:158-159` | `GET /api/route` returns first route; `POST /api/route` validates via `validate_references`, persists to `runtime-overrides.toml` (`RuntimeOverrides`), rebuilds `DispatchRouter::from_config` and `store()`s it into the `ArcSwap`, re-attaching `session_overrides`. This is the required rollback path — resolution must ship as opt-in route entry, no changes needed to the swap mechanics; only additive per-alias counters in `/metrics`. |
| Ingress (opencode) | `src/entrypoint/chat_completions.rs:34-58`, `src/providers/openai.rs:1-80` | opencode hits `POST /v1/chat/completions` with `body["model"]="auto-coding"`; handler extracts `model`, translates via `translate_openai_to_anthropic`, calls `Router::dispatch`. `OpenaiProvider` is the generic forwarder (`POST {base_url}/v1/chat/completions`, dual `reqwest::Client` pooled/streaming split, `list_models()` backs `GET /api/models`). Alias must be intercepted *before* `translate_openai_to_anthropic` or inside `dispatch`'s model read — otherwise the alias leaks to OpenRouter verbatim. |
| Dashboard | `src/dashboard.rs:1-60` (inlined `DASHBOARD_HTML`, 803 lines) | Polls `/metrics` (30 s) + `/errors/summary` (60 s), Chart.js. "Live pick + why" is a small additive panel reading new `family` section of `/metrics` (current pick + error-rate/latency figures) — keep to the two ranking signals per requirements. |
| Supporting | `src/ratelimit/limiter.rs` (`RateLimiters`, `AdmissionControl::admit` post-selection per ADR-004), `src/routing/session_overrides.rs` (pin precedence via `effective_candidates`), `src/entrypoint/observability.rs` (`/metrics` JSON handler), `src/cost_metrics/` (pricing/usage — source of free-vs-paid flag for default-free vs opt-in-paid alias) | No changes required; family interacts post-admission-check like any other candidate. |

## 2. Libraries already in `Cargo.toml` that cover this

- `dashmap 6` — per-member stats map (same keying pattern as `ProxyMetrics::upstreams` and `HealthRegistry::state`).
- `rand 0.8` (pinned; 0.9 moved `WeightedIndex`/`thread_rng` paths) — keep for any weighted tie-break; do not bump in this feature.
- `arc-swap 1.9.2` — publish the resolved pick for the dashboard without locking dispatch.
- `tokio 1 (full)`, `axum 0.8`, `serde/serde_json 1`, `chrono 0.4 (serde)`, `uuid 1 (v4+serde)` — request IDs, timestamps, JSON surface.
- `moka 0.12 (future)` — available if OpenRouter catalog refresh needs a TTL cache; not needed for stats.
- `governor 0.10`, `backoff 0.4 (tokio)`, `tokio-retry 0.3` — unrelated (rate limiting / provider-internal retries stay inside providers per `router.rs` header).

## 3. New dependencies: none required (recommended)

Hand-roll decay in ~20–40 lines following `histogram.rs` rather than adding a crate:

- **Error-rate**: `Mutex<VecDeque<(Instant, bool)>>` per family member (bounded, e.g. last N=50 or last T=15–30 min, trimmed on record like `DurationHistogram::record`), or lighter: two `AtomicU64` EWMA values. Either reuses only `std` + already-present `dashmap`.
- **Latency**: reuse `DurationHistogram` per member verbatim (p50 for ranking; averages in `counters.rs` are lifetime means and unsuitable).
- **EWMA formula** (if chosen over window): `ema = alpha * sample + (1 - alpha) * ema`, `alpha ≈ 0.2–0.3` for latency, separate faster `alpha` for error bit; store as `AtomicU64` of `f64::to_bits` for lock-free updates.

If a crate is preferred anyway, community options (all *optional*, none necessary at single-user volume):

| Crate | Fit | Caveat |
|---|---|---|
| `erwanor/ewma` (uneven-timeseries EWMA, `Smoothing::Static`/`Dynamic` with `tau`) | Closest semantic match for irregular request gaps | Tiny unmaintained crate; audit burden exceeds hand-rolled 20 lines |
| `metrics-lib` (`Gauge` with EMA helpers, tumbling-window `RateMeter`) | General metrics toolkit | Pulls an entire metrics framework for two signals; overkill |
| `latency_tracker` (lock-free striped histogram, sliding window percentiles) | High-throughput percentile reads | Compile-time env-var config (`LATENCY_TRACKER_WINDOW` etc.), designed for multi-threaded services — wrong scale for a loopback single-user proxy; `DurationHistogram` already does this |

Recommendation: **zero new deps**. Cold start falls back to config order (requirements feasibility risk), decay via windowed deque or EWMA in `src/routing/` or `src/metrics/` behind the existing `RoutingStrategy` trait so it stays unit-testable.

## 4. Rust patterns to follow

- New strategy as `impl RoutingStrategy` (pure fn of `&[UpstreamRef]` + stats snapshot) — keeps ADR-003 health-blind separation; health/cooldown and rate-limit admission stay untouched.
- Index-keyed identity (`UpstreamRef.index` ↔ `HealthRegistry`) — extend to `(upstream_idx, model_id)` composite key for per-model stats; do not re-key existing `ProxyMetrics::upstreams` (dashboard compat), add a parallel `DashMap<(usize, String), MemberStats>`.
- `Arc<dyn RoutingStrategy>` + `ArcSwap` router swap + `Arc<MetricsCollector>` sharing — resolution reads stats via atomics/lock-free snapshot, never holds a guard across `.await`.
- `deny_unknown_fields` serde configs — any new `Family` table must add explicit fields; unknown-field rejection is the typo guard.
- `#[lints.clippy] pedantic + unwrap_used/expect_used warn` (`Cargo.toml:9-14`) — no `.unwrap()` in resolution path; return deterministic default on empty stats.
