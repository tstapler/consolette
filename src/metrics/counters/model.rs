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

impl ProxyMetrics {
    pub fn record_model_attempt(&self, model: &str, outcome: ModelOutcome) {
        if model.is_empty() || model == "unknown" {
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
        if model.is_empty() || model == "unknown" {
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
