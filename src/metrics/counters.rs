//! Atomic counters for request, compression, and error metrics.
//!
//! All counters use `AtomicU64` for lock-free concurrent access.
//! The `ProxyMetrics` struct holds a complete snapshot of all proxy statistics.

use dashmap::DashMap;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

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

    // ---- Duration bucket counters ----
    pub duration_lt1s: AtomicU64,
    pub duration_1_5s: AtomicU64,
    pub duration_5_30s: AtomicU64,
    pub duration_30_60s: AtomicU64,
    pub duration_gt60s: AtomicU64,
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

            duration_lt1s: AtomicU64::new(0),
            duration_1_5s: AtomicU64::new(0),
            duration_5_30s: AtomicU64::new(0),
            duration_30_60s: AtomicU64::new(0),
            duration_gt60s: AtomicU64::new(0),
        }
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

            providers.insert(
                name.clone(),
                json!({
                    "requests": c.requests.load(Ordering::Relaxed),
                    "success": c.success.load(Ordering::Relaxed),
                    "errors": c.errors.load(Ordering::Relaxed),
                }),
            );

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
            }
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
}
