//! Atomic counters for request, compression, and error metrics.
//!
//! All counters use `AtomicU64` for lock-free concurrent access.
//! The `ProxyMetrics` struct holds a complete snapshot of all proxy statistics.

use dashmap::DashMap;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

/// Per-candidate outcome label for [`ProxyMetrics::record_resolution_attempt`]
/// (Story 5.1.1, `openai-model-resolution` project). `Other` is deliberately
/// a distinct series from `Transient` (pre-mortem.md P1 #1): an
/// `Other`-classified probe failure means the resolution loop's
/// classification table didn't recognize the error at all — a different
/// operator signal from a genuine transient blip — so folding the two
/// together would hide exactly the failure mode this project exists to fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionOutcome {
    Success,
    Advance,
    RetryResponses,
    Transient,
    Other,
    Exhausted,
}

impl ResolutionOutcome {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ResolutionOutcome::Success => "success",
            ResolutionOutcome::Advance => "advance",
            ResolutionOutcome::RetryResponses => "retry_responses",
            ResolutionOutcome::Transient => "transient",
            ResolutionOutcome::Other => "other",
            ResolutionOutcome::Exhausted => "exhausted",
        }
    }
}

/// Per-family resolution state (Story 5.1.2, Domain Glossary). Unlike
/// `ProxyMetrics::resolution_exhausted_total` (a cumulative counter that
/// never resets), this is self-healing: it flips back to `Newest`/`Fallback`
/// on the next successful resolution for the family, so the dashboard status
/// (Story 5.2.2) doesn't stay stuck red after a real recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResolutionState {
    /// The top-ranked (first) candidate in the walk resolved successfully.
    Newest,
    /// A lower-ranked candidate won because higher-ranked ones failed.
    Fallback,
    /// Every candidate in the family failed; no model is currently resolved.
    Exhausted,
}

impl ResolutionState {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ResolutionState::Newest => "newest",
            ResolutionState::Fallback => "fallback",
            ResolutionState::Exhausted => "exhausted",
        }
    }
}

/// Per-upstream request/latency counters, keyed by upstream name in
/// `ProxyMetrics::upstreams` (mirrors `ratelimit::RateLimiters`' `DashMap<String, _>`
/// keying, ADR-004). Replaces the old hardcoded `requests_anthropic`/
/// `requests_bedrock` fields so any configured upstream — not just the
/// original two — shows up in `/metrics` (Task 3.4.5).
#[derive(Default)]
pub struct UpstreamCounters {
    pub requests: AtomicU64,
    pub success: AtomicU64,
    pub errors: AtomicU64,
    pub duration_sum_ms: AtomicU64,
    pub duration_count: AtomicU64,
    pub first_byte_sum_ms: AtomicU64,
    pub first_byte_count: AtomicU64,
    /// The most recent *typed* `ProviderError::kind_label()` classification
    /// for this upstream — `None` after a successful request (Story 1.4.4:
    /// root-cause fix so the dashboard self-heals instead of pinning a
    /// stale error state forever after one past failure). Set via
    /// `ProxyMetrics::set_last_error_kind`, never derived by re-guessing
    /// keywords out of an error's `Display` text.
    pub last_error_kind: std::sync::Mutex<Option<&'static str>>,
}

/// All proxy metrics as atomic counters.
///
/// Designed for concurrent access with no locks — each field is independently
/// updated by request handlers and background tasks.
pub struct ProxyMetrics {
    // ---- Request counters ----
    pub requests_total: AtomicU64,
    pub requests_success: AtomicU64,
    pub errors_total: AtomicU64,
    pub fallback_switches: AtomicU64,

    /// Per-upstream breakdown, keyed by upstream config name (e.g.
    /// `"anthropic"`, `"bedrock"`, `"model-gateway-openai"`).
    pub upstreams: DashMap<String, UpstreamCounters>,

    // ---- Error type counters ----
    pub err_timeout: AtomicU64,
    pub err_auth: AtomicU64,
    pub err_rate_limit: AtomicU64,
    pub err_validation: AtomicU64,

    // ---- Compression counters ----
    pub tokens_before: AtomicU64,
    pub tokens_after: AtomicU64,
    pub requests_compressed: AtomicU64,

    // ---- Cache counters ----
    pub cache_aligner_applied: AtomicU64,
    pub cache_hits_estimated: AtomicU64,
    pub cache_misses_estimated: AtomicU64,

    // ---- Learn counters ----
    pub learn_patterns_found: AtomicU64,
    pub verbosity_applied: AtomicU64,

    // ---- Memory store counters ----
    pub memory_puts: AtomicU64,
    pub memory_gets: AtomicU64,
    pub memory_dedup_hits: AtomicU64,

    // ---- count_tokens counters ----
    pub count_tokens_total: AtomicU64,
    pub count_tokens_failures: AtomicU64,

    // ---- Server-tool emulation counters (plan: server-tool-emulation) ----
    pub server_tool_searches_total: AtomicU64,
    pub server_tool_searches_ok: AtomicU64,
    pub server_tool_search_failures: AtomicU64,
    pub server_tool_iterations_total: AtomicU64,
    /// Per-backend hits, keyed `"brave"` / `"browser"` (D4 dashboard label).
    pub server_tool_backend_hits: DashMap<String, AtomicU64>,

    // ---- Duration bucket counters ----
    pub duration_lt1s: AtomicU64,
    pub duration_1_5s: AtomicU64,
    pub duration_5_30s: AtomicU64,
    pub duration_30_60s: AtomicU64,
    pub duration_gt60s: AtomicU64,

    // ---- openai-model-resolution counters (Epic 5.1) ----
    /// Story 5.1.1: one increment per resolution-walk candidate outcome,
    /// keyed `(upstream, family, candidate, outcome)` where `outcome` is a
    /// [`ResolutionOutcome::label`].
    pub resolution_attempts: DashMap<(String, String, String, String), AtomicU64>,
    /// Story 5.1.2: cumulative, never-decrementing count of resolution walks
    /// that exhausted every candidate for a family, keyed `(upstream,
    /// family)`. The self-healing signal is `resolution_state` below, not
    /// this counter.
    pub resolution_exhausted_total: DashMap<(String, String), AtomicU64>,
    /// Story 5.1.2: current resolution state per family (keyed by family
    /// name), self-healing on the next successful resolution.
    pub resolution_state: DashMap<String, ResolutionState>,
    /// Story 5.2.1: the model id the most recent successful resolution
    /// walk landed on, keyed by family. Cleared (not left stale) on
    /// exhaustion, since a resolved model id from before every candidate
    /// started failing is no longer something a caller should route to.
    pub resolved_model: DashMap<String, String>,
    /// Story 5.2.1: which family each upstream resolves against, keyed by
    /// upstream name. Populated as a side effect of
    /// `record_resolution_attempt`/`record_resolution_exhausted` (both
    /// already know the upstream/family pair) so `to_json()` can attach
    /// flat `resolution_*` fields to the right `providers[upstream]` entry
    /// without needing config access — see Task 5.2.1a.
    pub upstream_family: DashMap<String, String>,
}

impl ProxyMetrics {
    /// Create a new zeroed metrics instance.
    #[must_use]
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            requests_total: AtomicU64::new(0),
            requests_success: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            fallback_switches: AtomicU64::new(0),

            upstreams: DashMap::new(),

            err_timeout: AtomicU64::new(0),
            err_auth: AtomicU64::new(0),
            err_rate_limit: AtomicU64::new(0),
            err_validation: AtomicU64::new(0),

            tokens_before: AtomicU64::new(0),
            tokens_after: AtomicU64::new(0),
            requests_compressed: AtomicU64::new(0),

            cache_aligner_applied: AtomicU64::new(0),
            cache_hits_estimated: AtomicU64::new(0),
            cache_misses_estimated: AtomicU64::new(0),

            learn_patterns_found: AtomicU64::new(0),
            verbosity_applied: AtomicU64::new(0),

            memory_puts: AtomicU64::new(0),
            memory_gets: AtomicU64::new(0),
            memory_dedup_hits: AtomicU64::new(0),

            count_tokens_total: AtomicU64::new(0),
            count_tokens_failures: AtomicU64::new(0),

            server_tool_searches_total: AtomicU64::new(0),
            server_tool_searches_ok: AtomicU64::new(0),
            server_tool_search_failures: AtomicU64::new(0),
            server_tool_iterations_total: AtomicU64::new(0),
            server_tool_backend_hits: DashMap::new(),

            duration_lt1s: AtomicU64::new(0),
            duration_1_5s: AtomicU64::new(0),
            duration_5_30s: AtomicU64::new(0),
            duration_30_60s: AtomicU64::new(0),
            duration_gt60s: AtomicU64::new(0),

            resolution_attempts: DashMap::new(),
            resolution_exhausted_total: DashMap::new(),
            resolution_state: DashMap::new(),
            resolved_model: DashMap::new(),
            upstream_family: DashMap::new(),
        }
    }

    /// Story 5.1.1: record one resolution-walk candidate outcome.
    pub fn record_resolution_attempt(
        &self,
        upstream: &str,
        family: &str,
        candidate: &str,
        outcome: ResolutionOutcome,
    ) {
        self.upstream_family
            .insert(upstream.to_string(), family.to_string());
        self.resolution_attempts
            .entry((
                upstream.to_string(),
                family.to_string(),
                candidate.to_string(),
                outcome.label().to_string(),
            ))
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Story 5.1.2/5.2.1: a resolution walk for `family` resolved
    /// successfully on `model_id`, with `state` being
    /// `ResolutionState::Newest` (the top-ranked candidate won) or
    /// `ResolutionState::Fallback` (a lower-ranked candidate won because
    /// higher-ranked ones failed) — never `ResolutionState::Exhausted`,
    /// which is only set by [`ProxyMetrics::record_resolution_exhausted`].
    /// Self-heals `resolution_state` and `resolved_model` — the
    /// dashboard-visible flags that clear/update on success, unlike
    /// `resolution_exhausted_total`.
    pub fn record_resolution_success(&self, family: &str, state: ResolutionState, model_id: &str) {
        debug_assert_ne!(
            state,
            ResolutionState::Exhausted,
            "record_resolution_success must not be called with Exhausted"
        );
        self.resolution_state.insert(family.to_string(), state);
        self.resolved_model
            .insert(family.to_string(), model_id.to_string());
    }

    /// Story 5.1.2: every candidate in `family` on `upstream` was exhausted.
    /// Increments the cumulative `resolution_exhausted_total` counter (never
    /// decrements — that's `resolution_state`'s job), sets the self-healing
    /// `resolution_state` flag, clears the now-stale `resolved_model` (Story
    /// 5.2.1: a dashboard consumer should see "no model resolved," not a
    /// model id that stopped working), and logs at `error` level, per the
    /// Observability Plan's "human-must-notice" requirement for this signal.
    pub fn record_resolution_exhausted(&self, upstream: &str, family: &str) {
        self.upstream_family
            .insert(upstream.to_string(), family.to_string());
        self.resolution_exhausted_total
            .entry((upstream.to_string(), family.to_string()))
            .or_insert_with(|| AtomicU64::new(0))
            .fetch_add(1, Ordering::Relaxed);
        self.resolution_state
            .insert(family.to_string(), ResolutionState::Exhausted);
        self.resolved_model.remove(family);
        tracing::error!(
            upstream,
            family,
            "resolution exhausted every candidate for this family"
        );
    }

    /// Record one dispatch attempt against a specific upstream, updating the
    /// global totals, that upstream's own bucket, and the duration
    /// histogram bucket. Called once per upstream actually tried — a
    /// request that fails over from upstream A to upstream B records twice,
    /// once per attempt (`fallback_switches` tracks the failover itself).
    pub fn record_request(
        &self,
        upstream: &str,
        success: bool,
        duration_ms: u64,
        first_byte_ms: u64,
    ) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        if success {
            self.requests_success.fetch_add(1, Ordering::Relaxed);
        } else {
            self.errors_total.fetch_add(1, Ordering::Relaxed);
        }

        let entry = self.upstreams.entry(upstream.to_string()).or_default();
        entry.requests.fetch_add(1, Ordering::Relaxed);
        if success {
            entry.success.fetch_add(1, Ordering::Relaxed);
        } else {
            entry.errors.fetch_add(1, Ordering::Relaxed);
        }
        entry
            .duration_sum_ms
            .fetch_add(duration_ms, Ordering::Relaxed);
        entry.duration_count.fetch_add(1, Ordering::Relaxed);
        if first_byte_ms > 0 {
            entry
                .first_byte_sum_ms
                .fetch_add(first_byte_ms, Ordering::Relaxed);
            entry.first_byte_count.fetch_add(1, Ordering::Relaxed);
        }
        drop(entry);

        // Duration bucket
        match duration_ms {
            d if d < 1_000 => {
                self.duration_lt1s.fetch_add(1, Ordering::Relaxed);
            }
            d if d < 5_000 => {
                self.duration_1_5s.fetch_add(1, Ordering::Relaxed);
            }
            d if d < 30_000 => {
                self.duration_5_30s.fetch_add(1, Ordering::Relaxed);
            }
            d if d < 60_000 => {
                self.duration_30_60s.fetch_add(1, Ordering::Relaxed);
            }
            _ => {
                self.duration_gt60s.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    /// Sets (or, with `None`, clears) the given upstream's most recent
    /// typed error classification (Story 1.4.4). Called with `Some(kind)`
    /// on a failed dispatch attempt and `None` on a successful one, so a
    /// past failure never permanently pins the dashboard's status class
    /// after the upstream recovers.
    pub fn set_last_error_kind(&self, upstream: &str, kind: Option<&'static str>) {
        let entry = self.upstreams.entry(upstream.to_string()).or_default();
        *entry
            .last_error_kind
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = kind;
    }

    /// Record one emulated search against a backend (`"brave"`/`"browser"`),
    /// updating totals, the outcome split, and the per-backend breakdown.
    pub fn record_server_tool_search(&self, backend: &str, ok: bool) {
        self.server_tool_searches_total
            .fetch_add(1, Ordering::Relaxed);
        if ok {
            self.server_tool_searches_ok.fetch_add(1, Ordering::Relaxed);
        } else {
            self.server_tool_search_failures
                .fetch_add(1, Ordering::Relaxed);
        }
        self.server_tool_backend_hits
            .entry(backend.to_string())
            .or_default()
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Record loop iterations for one emulated request.
    pub fn record_server_tool_iterations(&self, iterations: u64) {
        self.server_tool_iterations_total
            .fetch_add(iterations, Ordering::Relaxed);
    }

    /// Classify a dispatch failure into the `error_types` breakdown
    /// (`timeout`/`auth`/`rate_limit`/`validation`) shown in `/metrics`.
    pub fn record_error_kind(&self, err: &crate::providers::ProviderError) {
        if matches!(err, crate::providers::ProviderError::Timeout) {
            self.err_timeout.fetch_add(1, Ordering::Relaxed);
        }
        if err.is_auth() {
            self.err_auth.fetch_add(1, Ordering::Relaxed);
        }
        if err.is_rate_limited() {
            self.err_rate_limit.fetch_add(1, Ordering::Relaxed);
        }
        if err.is_validation() {
            self.err_validation.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Snapshot the server-tool emulation counters, including the
    /// per-backend (`brave`/`browser`) breakdown for the D4 dashboard.
    fn server_tools_json(&self) -> Value {
        let backends: serde_json::Map<String, Value> = self
            .server_tool_backend_hits
            .iter()
            .map(|entry| {
                (
                    entry.key().clone(),
                    json!(entry.value().load(Ordering::Relaxed)),
                )
            })
            .collect();
        json!({
            "searches_total": self.server_tool_searches_total.load(Ordering::Relaxed),
            "searches_ok": self.server_tool_searches_ok.load(Ordering::Relaxed),
            "search_failures": self.server_tool_search_failures.load(Ordering::Relaxed),
            "iterations_total": self.server_tool_iterations_total.load(Ordering::Relaxed),
            "by_backend": backends
        })
    }

    /// Story 5.1.1: per-`(upstream, family, candidate, outcome)` attempt
    /// tally for the `/metrics` endpoint.
    fn resolution_attempts_json(&self) -> Vec<Value> {
        self.resolution_attempts
            .iter()
            .map(|entry| {
                let (upstream, family, candidate, outcome) = entry.key().clone();
                json!({
                    "upstream": upstream,
                    "family": family,
                    "candidate": candidate,
                    "outcome": outcome,
                    "count": entry.value().load(Ordering::Relaxed),
                })
            })
            .collect()
    }

    /// Story 5.1.2: per-`(upstream, family)` cumulative exhaustion tally for
    /// the `/metrics` endpoint.
    fn resolution_exhausted_json(&self) -> Vec<Value> {
        self.resolution_exhausted_total
            .iter()
            .map(|entry| {
                let (upstream, family) = entry.key().clone();
                json!({
                    "upstream": upstream,
                    "family": family,
                    "count": entry.value().load(Ordering::Relaxed),
                })
            })
            .collect()
    }

    /// Story 5.1.2: the self-healing per-family resolution state for the
    /// `/metrics` endpoint.
    fn resolution_state_json(&self) -> serde_json::Map<String, Value> {
        self.resolution_state
            .iter()
            .map(|entry| (entry.key().clone(), json!(entry.value().label())))
            .collect()
    }

    /// Story 5.1.1/5.1.2: snapshots the resolution-walk counters — the
    /// per-candidate-outcome attempt tally, the cumulative exhaustion tally,
    /// and the self-healing per-family state — for the `/metrics` endpoint.
    fn resolution_json(&self) -> Value {
        json!({
            "attempts": self.resolution_attempts_json(),
            "exhausted": self.resolution_exhausted_json(),
            "state": self.resolution_state_json(),
        })
    }

    /// Builds the `providers` and `provider_latency` sections of `/metrics`
    /// from the per-upstream `DashMap`, one entry per upstream actually
    /// dispatched to at least once.
    #[allow(clippy::cast_precision_loss)]
    fn upstream_json(&self) -> (Value, Value) {
        let mut providers = serde_json::Map::new();
        let mut provider_latency = serde_json::Map::new();

        for entry in &self.upstreams {
            let name = entry.key().clone();
            let c = entry.value();

            let last_error_kind = *c
                .last_error_kind
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut obj = serde_json::Map::new();
            obj.insert(
                "requests".to_string(),
                json!(c.requests.load(Ordering::Relaxed)),
            );
            obj.insert(
                "success".to_string(),
                json!(c.success.load(Ordering::Relaxed)),
            );
            obj.insert(
                "errors".to_string(),
                json!(c.errors.load(Ordering::Relaxed)),
            );
            obj.insert("last_error_kind".to_string(), json!(last_error_kind));

            // Story 5.2.1: flat resolved_model/resolution_state/resolution_family
            // siblings on the provider object, present only for upstreams
            // that have actually run a resolution walk (i.e. have
            // `model_family` configured) — absent, not null, otherwise.
            if let Some(family) = self.upstream_family.get(&name) {
                let family = family.clone();
                obj.insert("resolution_family".to_string(), json!(family));
                if let Some(state) = self.resolution_state.get(&family) {
                    obj.insert("resolution_state".to_string(), json!(state.label()));
                }
                if let Some(model_id) = self.resolved_model.get(&family) {
                    obj.insert("resolved_model".to_string(), json!(model_id.clone()));
                }
            }

            providers.insert(name.clone(), Value::Object(obj));

            let dur_count = c.duration_count.load(Ordering::Relaxed);
            let dur_avg = c
                .duration_sum_ms
                .load(Ordering::Relaxed)
                .checked_div(dur_count)
                .unwrap_or(0);
            let fb_count = c.first_byte_count.load(Ordering::Relaxed);
            let fb_avg = c
                .first_byte_sum_ms
                .load(Ordering::Relaxed)
                .checked_div(fb_count)
                .unwrap_or(0);
            provider_latency.insert(
                name,
                json!({
                    "avg_duration_ms": dur_avg,
                    "avg_first_byte_ms": fb_avg,
                    "requests": dur_count,
                }),
            );
        }

        (Value::Object(providers), Value::Object(provider_latency))
    }

    /// Snapshot all counters into a `serde_json::Value` for the `/metrics` endpoint.
    #[must_use]
    // Counter values stay far below 2^52, so the `u64 as f64` conversions below
    // never lose precision in practice.
    #[allow(clippy::cast_precision_loss)]
    // One flat `json!` literal mirroring the legacy `/metrics` response shape
    // field-for-field; splitting it into helper functions would scatter the
    // shape across multiple places for no behavioral benefit.
    #[allow(clippy::too_many_lines)]
    pub fn to_json(&self) -> Value {
        let total = self.requests_total.load(Ordering::Relaxed);
        let success = self.requests_success.load(Ordering::Relaxed);
        let errors = self.errors_total.load(Ordering::Relaxed);
        let fallbacks = self.fallback_switches.load(Ordering::Relaxed);

        let success_rate = if total > 0 {
            (success as f64 / total as f64) * 100.0
        } else {
            0.0
        };
        let error_rate = if total > 0 {
            (errors as f64 / total as f64) * 100.0
        } else {
            0.0
        };

        let (providers, provider_latency) = self.upstream_json();

        // Compression stats
        let tokens_before = self.tokens_before.load(Ordering::Relaxed);
        let tokens_after = self.tokens_after.load(Ordering::Relaxed);
        let tokens_saved = tokens_before.saturating_sub(tokens_after);
        let avg_ratio = if tokens_before > 0 {
            tokens_saved as f64 / tokens_before as f64
        } else {
            0.0
        };

        let ct_total = self.count_tokens_total.load(Ordering::Relaxed);
        let ct_failures = self.count_tokens_failures.load(Ordering::Relaxed);
        let ct_failure_rate = if ct_total > 0 {
            ct_failures as f64 / ct_total as f64
        } else {
            0.0
        };

        json!({
            "summary": {
                "total_requests": total,
                "total_success": success,
                "total_errors": errors,
                "total_fallbacks": fallbacks,
                "success_rate": (success_rate * 100.0).round() / 100.0,
                "error_rate": (error_rate * 100.0).round() / 100.0
            },
            "providers": providers,
            "provider_latency": provider_latency,
            // compression/memory/learn/count_tokens below: counters carried
            // over from the legacy proxy's metrics schema for features that
            // were never ported into `Router::dispatch()` (prompt
            // compression, memory dedup, pattern learning, a real
            // `count_tokens` call). Nothing writes to these atomics, so
            // they're permanently zero — the dashboard cards they feed
            // render but never move. Wiring one up is a new feature, not a
            // metrics fix; see git history/ADRs before reviving one.
            "compression": {
                "total_tokens_before": tokens_before,
                "total_tokens_after": tokens_after,
                "total_tokens_saved": tokens_saved,
                "total_requests_compressed": self.requests_compressed.load(Ordering::Relaxed),
                "avg_compression_ratio": (avg_ratio * 1000.0).round() / 1000.0
            },
            "memory": {
                "puts": self.memory_puts.load(Ordering::Relaxed),
                "gets": self.memory_gets.load(Ordering::Relaxed),
                "dedup_hits": self.memory_dedup_hits.load(Ordering::Relaxed)
            },
            "learn": {
                "patterns_found": self.learn_patterns_found.load(Ordering::Relaxed)
            },
            "cache": {
                "aligner_applied": self.cache_aligner_applied.load(Ordering::Relaxed),
                "hits_estimated": self.cache_hits_estimated.load(Ordering::Relaxed),
                "misses_estimated": self.cache_misses_estimated.load(Ordering::Relaxed)
            },
            "duration_distribution": {
                "< 1s": self.duration_lt1s.load(Ordering::Relaxed),
                "1-5s": self.duration_1_5s.load(Ordering::Relaxed),
                "5-30s": self.duration_5_30s.load(Ordering::Relaxed),
                "30-60s": self.duration_30_60s.load(Ordering::Relaxed),
                "> 60s": self.duration_gt60s.load(Ordering::Relaxed)
            },
            "count_tokens": {
                "total": ct_total,
                "failures": ct_failures,
                "failure_rate": (ct_failure_rate * 1000.0).round() / 1000.0,
                "last_count": 0u64,
                "last_model": ""
            },
            "error_types": {
                "timeout": self.err_timeout.load(Ordering::Relaxed),
                "auth": self.err_auth.load(Ordering::Relaxed),
                "rate_limit": self.err_rate_limit.load(Ordering::Relaxed),
                "validation": self.err_validation.load(Ordering::Relaxed)
            },
            "server_tools": self.server_tools_json(),
            "resolution": self.resolution_json()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::ProviderError;

    #[test]
    fn record_error_kind_classifies_each_variant() {
        let m = ProxyMetrics::new();
        m.record_error_kind(&ProviderError::Timeout);
        m.record_error_kind(&ProviderError::Auth("bad token".to_string()));
        m.record_error_kind(&ProviderError::RateLimited);
        m.record_error_kind(&ProviderError::Validation("bad field".to_string(), 400));

        assert_eq!(m.err_timeout.load(Ordering::Relaxed), 1);
        assert_eq!(m.err_auth.load(Ordering::Relaxed), 1);
        assert_eq!(m.err_rate_limit.load(Ordering::Relaxed), 1);
        assert_eq!(m.err_validation.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn record_request_updates_totals_and_duration_bucket() {
        let m = ProxyMetrics::new();
        m.record_request("upstream-a", true, 500, 0);
        m.record_request("upstream-a", false, 2_000, 0);

        assert_eq!(m.requests_total.load(Ordering::Relaxed), 2);
        assert_eq!(m.requests_success.load(Ordering::Relaxed), 1);
        assert_eq!(m.errors_total.load(Ordering::Relaxed), 1);
        assert_eq!(m.duration_lt1s.load(Ordering::Relaxed), 1);
        assert_eq!(m.duration_1_5s.load(Ordering::Relaxed), 1);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn record_request_tracks_any_upstream_name_independently() {
        let m = ProxyMetrics::new();
        m.record_request("model-gateway-openai", true, 100, 20);
        m.record_request("model-gateway-openai", true, 300, 0);
        m.record_request("bedrock", false, 1_000, 0);

        let gateway = m.upstreams.get("model-gateway-openai").unwrap();
        assert_eq!(gateway.requests.load(Ordering::Relaxed), 2);
        assert_eq!(gateway.success.load(Ordering::Relaxed), 2);
        assert_eq!(gateway.duration_sum_ms.load(Ordering::Relaxed), 400);
        assert_eq!(gateway.first_byte_count.load(Ordering::Relaxed), 1);
        drop(gateway);

        let bedrock = m.upstreams.get("bedrock").unwrap();
        assert_eq!(bedrock.requests.load(Ordering::Relaxed), 1);
        assert_eq!(bedrock.errors.load(Ordering::Relaxed), 1);
        drop(bedrock);

        assert!(m.upstreams.get("anthropic").is_none());
    }

    // REQ-10 (Story 1.4.4b/c) — focus area.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn set_last_error_kind_should_set_to_auth_when_gemini_auth_error_recorded() {
        let m = ProxyMetrics::new();
        m.set_last_error_kind("gemini", Some("auth"));

        let entry = m.upstreams.get("gemini").unwrap();
        assert_eq!(
            *entry
                .last_error_kind
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Some("auth")
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn set_last_error_kind_should_overwrite_not_coexist_with_stale_prior_kind() {
        let m = ProxyMetrics::new();
        m.set_last_error_kind("gemini", Some("auth"));
        m.set_last_error_kind("gemini", Some("response_shape_mismatch"));

        let entry = m.upstreams.get("gemini").unwrap();
        assert_eq!(
            *entry
                .last_error_kind
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            Some("response_shape_mismatch")
        );
    }

    // REQ-10 — the self-healing clear-on-success case (Story 1.4.4
    // acceptance criterion).
    #[test]
    #[allow(clippy::unwrap_used)]
    fn set_last_error_kind_should_reset_to_none_after_subsequent_success() {
        let m = ProxyMetrics::new();
        m.set_last_error_kind("gemini", Some("auth"));
        m.set_last_error_kind("gemini", None);

        let entry = m.upstreams.get("gemini").unwrap();
        assert_eq!(
            *entry
                .last_error_kind
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            None
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn to_json_should_expose_last_error_kind_per_upstream() {
        let m = ProxyMetrics::new();
        m.record_request("gemini", false, 100, 0);
        m.set_last_error_kind("gemini", Some("response_shape_mismatch"));

        let json = m.to_json();
        assert_eq!(
            json["providers"]["gemini"]["last_error_kind"],
            json!("response_shape_mismatch")
        );
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn to_json_reports_one_providers_entry_per_upstream_seen() {
        let m = ProxyMetrics::new();
        m.record_request("anthropic", true, 100, 0);
        m.record_request("model-gateway-openai", true, 200, 0);

        let json = m.to_json();
        let providers = json["providers"].as_object().unwrap();
        assert_eq!(providers.len(), 2);
        assert_eq!(providers["anthropic"]["requests"], json!(1));
        assert_eq!(providers["model-gateway-openai"]["requests"], json!(1));

        let latency = json["provider_latency"].as_object().unwrap();
        assert_eq!(latency["anthropic"]["avg_duration_ms"], json!(100));
    }

    #[test]
    fn record_server_tool_search_should_label_backends_separately() {
        let m = ProxyMetrics::new();
        m.record_server_tool_search("brave", true);
        m.record_server_tool_search("brave", true);
        m.record_server_tool_search("browser", true);
        m.record_server_tool_search("unserved", false);
        m.record_server_tool_iterations(3);

        assert_eq!(m.server_tool_searches_total.load(Ordering::Relaxed), 4);
        assert_eq!(m.server_tool_searches_ok.load(Ordering::Relaxed), 3);
        assert_eq!(m.server_tool_search_failures.load(Ordering::Relaxed), 1);
        assert_eq!(m.server_tool_iterations_total.load(Ordering::Relaxed), 3);

        let json = m.to_json();
        assert_eq!(json["server_tools"]["searches_total"], json!(4));
        assert_eq!(json["server_tools"]["by_backend"]["brave"], json!(2));
        assert_eq!(json["server_tools"]["by_backend"]["browser"], json!(1));
        assert_eq!(json["server_tools"]["by_backend"]["unserved"], json!(1));
    }

    // ────────────────────────────────────────────────────────────────────
    // Epic 5.1: openai-model-resolution metrics
    // ────────────────────────────────────────────────────────────────────

    // validation.md: In-Scope 5 / Story 5.1.1.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn resolution_attempts_total_should_increment_once_per_candidate_outcome_with_correct_labels() {
        let m = ProxyMetrics::new();
        m.record_resolution_attempt(
            "model-gateway-openai",
            "gpt-5",
            "v3",
            ResolutionOutcome::Advance,
        );
        m.record_resolution_attempt(
            "model-gateway-openai",
            "gpt-5",
            "v2",
            ResolutionOutcome::Success,
        );

        let json = m.to_json();
        let attempts = json["resolution"]["attempts"].as_array().unwrap();
        assert_eq!(attempts.len(), 2);
        assert!(attempts
            .iter()
            .any(|a| a["upstream"] == "model-gateway-openai"
                && a["family"] == "gpt-5"
                && a["candidate"] == "v3"
                && a["outcome"] == "advance"
                && a["count"] == 1));
        assert!(attempts
            .iter()
            .any(|a| a["upstream"] == "model-gateway-openai"
                && a["family"] == "gpt-5"
                && a["candidate"] == "v2"
                && a["outcome"] == "success"
                && a["count"] == 1));
    }

    // pre-mortem.md P1 #1 / Task 5.1.1d: `other` must never be folded into
    // `transient` — they must be distinct series.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn resolution_attempts_total_should_track_other_and_transient_as_distinct_series() {
        let m = ProxyMetrics::new();
        m.record_resolution_attempt("gw", "gpt-5", "v3", ResolutionOutcome::Other);

        let attempts = m.to_json()["resolution"]["attempts"].clone();
        let attempts = attempts.as_array().unwrap();
        assert_eq!(
            attempts.len(),
            1,
            "an Other-classified abort must increment exactly one series"
        );
        assert_eq!(attempts[0]["outcome"], "other");

        // A genuinely transient failure on the same candidate is a second,
        // independent series, never merged into the `other` count.
        m.record_resolution_attempt("gw", "gpt-5", "v3", ResolutionOutcome::Transient);
        let attempts = m.to_json()["resolution"]["attempts"].clone();
        let attempts = attempts.as_array().unwrap();
        let other = attempts.iter().find(|a| a["outcome"] == "other").unwrap();
        let transient = attempts
            .iter()
            .find(|a| a["outcome"] == "transient")
            .unwrap();
        assert_eq!(other["count"], 1);
        assert_eq!(transient["count"], 1);
    }

    // validation.md: In-Scope 5 / Story 5.1.2 — cumulative, never decrements.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn resolution_exhausted_total_should_remain_nonzero_after_a_subsequent_successful_resolution() {
        let m = ProxyMetrics::new();
        m.record_resolution_exhausted("model-gateway-openai", "gpt-5");

        let json = m.to_json();
        let exhausted = json["resolution"]["exhausted"].as_array().unwrap();
        assert_eq!(exhausted.len(), 1);
        assert_eq!(exhausted[0]["count"], 1);

        // A subsequent successful resolution for the same family must not
        // decrement or clear the cumulative counter — only the separate
        // `resolution_state` flag self-heals.
        m.record_resolution_success("gpt-5", ResolutionState::Newest, "gpt-5.3-codex");

        let json = m.to_json();
        let exhausted = json["resolution"]["exhausted"].as_array().unwrap();
        assert_eq!(
            exhausted[0]["count"], 1,
            "resolution_exhausted_total must never decrement on a later success"
        );
    }

    // validation.md: In-Scope 5 / Story 5.1.2 — state flag self-heals.
    #[test]
    fn resolution_state_should_reset_to_newest_or_fallback_after_exhaustion_followed_by_success() {
        let m = ProxyMetrics::new();
        m.record_resolution_exhausted("model-gateway-openai", "gpt-5");
        assert_eq!(
            m.to_json()["resolution"]["state"]["gpt-5"],
            json!("exhausted")
        );

        m.record_resolution_success("gpt-5", ResolutionState::Newest, "gpt-5.3-codex");
        assert_eq!(m.to_json()["resolution"]["state"]["gpt-5"], json!("newest"));

        m.record_resolution_exhausted("model-gateway-openai", "gpt-5");
        m.record_resolution_success("gpt-5", ResolutionState::Fallback, "gpt-5.2-codex");
        assert_eq!(
            m.to_json()["resolution"]["state"]["gpt-5"],
            json!("fallback")
        );
    }

    // Story 5.2.1: flat resolved_model/resolution_state/resolution_family
    // siblings on providers[upstream] — the dashboard's own per-upstream
    // rendering shape, distinct from (and additional to) the "resolution"
    // aggregate section above.
    #[test]
    fn to_json_should_expose_flat_resolution_fields_on_the_resolving_upstream() {
        let m = ProxyMetrics::new();
        m.record_request("model-gateway-openai", true, 100, 0);
        m.record_resolution_attempt(
            "model-gateway-openai",
            "gpt-5-codex",
            "gpt-5.3-codex",
            ResolutionOutcome::Success,
        );
        m.record_resolution_success("gpt-5-codex", ResolutionState::Newest, "gpt-5.3-codex");

        let json = m.to_json();
        let provider = &json["providers"]["model-gateway-openai"];
        assert_eq!(provider["resolved_model"], json!("gpt-5.3-codex"));
        assert_eq!(provider["resolution_state"], json!("newest"));
        assert_eq!(provider["resolution_family"], json!("gpt-5-codex"));
    }

    // Story 5.2.1: an exhausted family still names which family to check
    // (the JSON-side half of ux.md's "no dead end" rule), even though
    // `resolved_model` is no longer present (Task 5.2.1a/b acceptance
    // criteria).
    #[test]
    fn to_json_should_keep_resolution_family_but_drop_resolved_model_when_exhausted() {
        let m = ProxyMetrics::new();
        m.record_request("model-gateway-openai", true, 100, 0);
        m.record_resolution_attempt(
            "model-gateway-openai",
            "gpt-5-codex",
            "gpt-5.3-codex",
            ResolutionOutcome::Advance,
        );
        m.record_resolution_exhausted("model-gateway-openai", "gpt-5-codex");

        let json = m.to_json();
        let provider = &json["providers"]["model-gateway-openai"];
        assert_eq!(provider["resolution_state"], json!("exhausted"));
        assert_eq!(provider["resolution_family"], json!("gpt-5-codex"));
        assert!(
            provider.get("resolved_model").is_none(),
            "resolved_model must be absent (not null/stale) once the family is exhausted, got {provider:?}"
        );
    }

    // Story 5.2.1: a static-pin upstream (no `model_family`, so no
    // resolution walk ever runs against it) must not gain any of the new
    // fields — they're absent, not present-as-null.
    #[test]
    fn to_json_should_omit_resolution_fields_for_a_static_pin_upstream() {
        let m = ProxyMetrics::new();
        m.record_request("anthropic", true, 100, 0);

        let json = m.to_json();
        let provider = &json["providers"]["anthropic"];
        assert!(provider.get("resolved_model").is_none());
        assert!(provider.get("resolution_state").is_none());
        assert!(provider.get("resolution_family").is_none());
    }
}
