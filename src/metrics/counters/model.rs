//! Per-model request and token counters (`ProxyMetrics::models`).

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

use super::ProxyMetrics;

/// Per-model request and token statistics.
#[derive(Default)]
pub struct ModelCounters {
    pub requests: AtomicU64,
    pub input_tokens: AtomicU64,
    pub output_tokens: AtomicU64,
    pub total_tokens: AtomicU64,
    pub errors: AtomicU64,
    pub rate_limits: AtomicU64,
}

/// How one dispatch attempt against a model resolved. `RateLimited` is a
/// subset of `Error` (a rate limit only ever occurs on the error path), so
/// it still counts toward `errors` in addition to `rate_limits` — see
/// `ProxyMetrics::record_model_attempt`.
#[derive(Clone, Copy)]
pub enum ModelOutcome {
    Success,
    Error,
    RateLimited,
}

/// Soft cap on distinct `model` values tracked in `ProxyMetrics::models`.
/// Unlike `upstreams` (keyed by config-bounded upstream name), `model` is a
/// fully client-controlled request-body field — an unbounded client could
/// otherwise grow this `DashMap` for the life of the process. A model
/// already being tracked is never dropped past the cap; only a *new* one
/// is skipped, so this bounds cardinality without needing a configured
/// model allowlist threaded in from the caller (routes vary in whether
/// they even have a fixed model list).
const MAX_DISTINCT_MODELS: usize = 200;

impl ProxyMetrics {
    /// Whether `model` should be recorded: already-tracked models are
    /// always allowed through; a brand-new one is admitted only under
    /// `MAX_DISTINCT_MODELS`. `DashMap::len()` is a racy estimate under
    /// concurrent writers, so this is a soft, approximate cap, not an
    /// exact one.
    fn should_track_model(&self, model: &str) -> bool {
        self.models.contains_key(model) || self.models.len() < MAX_DISTINCT_MODELS
    }

    pub fn record_model_attempt(&self, model: &str, outcome: ModelOutcome) {
        if model.is_empty() || model == "unknown" || !self.should_track_model(model) {
            return;
        }
        let entry = self.models.entry(model.to_string()).or_default();
        entry.requests.fetch_add(1, Ordering::Relaxed);
        match outcome {
            ModelOutcome::Success => {}
            ModelOutcome::Error => {
                entry.errors.fetch_add(1, Ordering::Relaxed);
            }
            ModelOutcome::RateLimited => {
                entry.errors.fetch_add(1, Ordering::Relaxed);
                entry.rate_limits.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn record_model_tokens(&self, model: &str, input_tokens: u64, output_tokens: u64) {
        if model.is_empty() || model == "unknown" || !self.should_track_model(model) {
            return;
        }
        let entry = self.models.entry(model.to_string()).or_default();
        entry
            .input_tokens
            .fetch_add(input_tokens, Ordering::Relaxed);
        entry
            .output_tokens
            .fetch_add(output_tokens, Ordering::Relaxed);
        entry
            .total_tokens
            .fetch_add(input_tokens + output_tokens, Ordering::Relaxed);
    }

    #[must_use]
    pub fn models_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        for entry in &self.models {
            let k = entry.key().clone();
            let v = entry.value();
            map.insert(
                k,
                json!({
                    "requests": v.requests.load(Ordering::Relaxed),
                    "input_tokens": v.input_tokens.load(Ordering::Relaxed),
                    "output_tokens": v.output_tokens.load(Ordering::Relaxed),
                    "total_tokens": v.total_tokens.load(Ordering::Relaxed),
                    "errors": v.errors.load(Ordering::Relaxed),
                    "rate_limits": v.rate_limits.load(Ordering::Relaxed),
                }),
            );
        }
        Value::Object(map)
    }
}

#[cfg(test)]
mod tests {
    use super::super::ProxyMetrics;
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn record_model_attempt_success_only_increments_requests() {
        let m = ProxyMetrics::new();
        m.record_model_attempt("claude-sonnet-5", ModelOutcome::Success);

        let entry = m.models.get("claude-sonnet-5").unwrap();
        assert_eq!(entry.requests.load(Ordering::Relaxed), 1);
        assert_eq!(entry.errors.load(Ordering::Relaxed), 0);
        assert_eq!(entry.rate_limits.load(Ordering::Relaxed), 0);
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn record_model_attempt_error_increments_requests_and_errors_only() {
        let m = ProxyMetrics::new();
        m.record_model_attempt("claude-sonnet-5", ModelOutcome::Error);

        let entry = m.models.get("claude-sonnet-5").unwrap();
        assert_eq!(entry.requests.load(Ordering::Relaxed), 1);
        assert_eq!(entry.errors.load(Ordering::Relaxed), 1);
        assert_eq!(entry.rate_limits.load(Ordering::Relaxed), 0);
    }

    /// The doc comment on `ModelOutcome::RateLimited` states it double-counts
    /// into both `errors` and `rate_limits` (a rate limit is a subset of
    /// error) — this asserts that stated behavior directly.
    #[test]
    #[allow(clippy::unwrap_used)]
    fn record_model_attempt_rate_limited_double_counts_errors_and_rate_limits() {
        let m = ProxyMetrics::new();
        m.record_model_attempt("claude-sonnet-5", ModelOutcome::RateLimited);

        let entry = m.models.get("claude-sonnet-5").unwrap();
        assert_eq!(entry.requests.load(Ordering::Relaxed), 1);
        assert_eq!(entry.errors.load(Ordering::Relaxed), 1);
        assert_eq!(entry.rate_limits.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn record_model_attempt_ignores_empty_and_unknown_model_names() {
        let m = ProxyMetrics::new();
        m.record_model_attempt("", ModelOutcome::Success);
        m.record_model_attempt("unknown", ModelOutcome::Success);

        assert!(m.models.get("").is_none());
        assert!(m.models.get("unknown").is_none());
        assert!(m.models.is_empty());
    }

    #[test]
    fn record_model_tokens_ignores_empty_and_unknown_model_names() {
        let m = ProxyMetrics::new();
        m.record_model_tokens("", 10, 5);
        m.record_model_tokens("unknown", 10, 5);

        assert!(m.models.is_empty());
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn record_model_tokens_accumulates_across_multiple_calls() {
        let m = ProxyMetrics::new();
        m.record_model_tokens("claude-sonnet-5", 100, 50);
        m.record_model_tokens("claude-sonnet-5", 30, 20);

        let entry = m.models.get("claude-sonnet-5").unwrap();
        assert_eq!(entry.input_tokens.load(Ordering::Relaxed), 130);
        assert_eq!(entry.output_tokens.load(Ordering::Relaxed), 70);
        assert_eq!(entry.total_tokens.load(Ordering::Relaxed), 200);
    }

    #[test]
    fn models_json_reports_shape_and_values_per_model() {
        let m = ProxyMetrics::new();
        m.record_model_attempt("claude-sonnet-5", ModelOutcome::Success);
        m.record_model_attempt("claude-sonnet-5", ModelOutcome::RateLimited);
        m.record_model_tokens("claude-sonnet-5", 100, 50);

        let json = m.models_json();
        let entry = &json["claude-sonnet-5"];
        assert_eq!(entry["requests"], json!(2));
        assert_eq!(entry["errors"], json!(1));
        assert_eq!(entry["rate_limits"], json!(1));
        assert_eq!(entry["input_tokens"], json!(100));
        assert_eq!(entry["output_tokens"], json!(50));
        assert_eq!(entry["total_tokens"], json!(150));
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn should_track_model_caps_new_models_but_keeps_recording_existing_ones() {
        let m = ProxyMetrics::new();
        for i in 0..MAX_DISTINCT_MODELS {
            m.record_model_attempt(&format!("model-{i}"), ModelOutcome::Success);
        }
        assert_eq!(m.models.len(), MAX_DISTINCT_MODELS);

        // A brand-new model beyond the cap must be dropped, not tracked.
        m.record_model_attempt("one-too-many", ModelOutcome::Success);
        assert_eq!(m.models.len(), MAX_DISTINCT_MODELS);
        assert!(m.models.get("one-too-many").is_none());

        // An already-tracked model must still be recordable past the cap.
        m.record_model_attempt("model-0", ModelOutcome::Success);
        assert_eq!(
            m.models
                .get("model-0")
                .unwrap()
                .requests
                .load(Ordering::Relaxed),
            2
        );
    }
}
