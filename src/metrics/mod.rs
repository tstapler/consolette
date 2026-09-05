//! Metrics collection: atomic counters, rolling histogram, error tracker,
//! request ring buffer, event-loop lag probe, and the `/metrics` JSON handler.

pub mod counters;
pub mod error_tracker;
pub mod histogram;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::time::sleep;

pub use counters::ProxyMetrics;
pub use error_tracker::{AggregatedError, ErrorRecord, ErrorTracker};
pub use histogram::DurationHistogram;

// Re-export so callers can use metrics::AggregatedError etc. without
// specifying the sub-module path.
#[allow(unused_imports)]
pub use error_tracker::{compute_fingerprint, extract_signature, normalize_message};

// ────────────────────────────────────────────────────────────────────────────
// RequestDetail — per-request ring-buffer entry
// ────────────────────────────────────────────────────────────────────────────

/// Lightweight per-request record for the dashboard "Recent Requests" table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestDetail {
    pub request_id: String,
    pub timestamp: String,
    pub model: String,
    pub provider: String,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub compressed: bool,
    pub stream: bool,
    /// JSON-encoded `{content-block-type: count}` map (e.g.
    /// `{"text":2,"tool_use":1}`) — the dashboard's `fmtTypes()` does its
    /// own `JSON.parse` on this, so it must stay a JSON object string, not
    /// a display string.
    pub msg_types: String,
    pub has_context_management: bool,
    pub message_count: u32,
    pub duration_ms: f64,
    pub first_byte_ms: f64,
    pub bedrock_invocation_ms: u64,
    pub bedrock_first_byte_ms: u64,
    /// The request's `metadata.user_id` verbatim, if present — the session
    /// key `routing::session_overrides` pins against. `None` for a request
    /// with no `metadata.user_id` (can't be session-pinned either).
    pub session_id: Option<String>,
}

impl RequestDetail {
    /// Builds the initial ring-buffer entry for an incoming request, before
    /// dispatch has picked an upstream: `provider` starts empty and timing
    /// fields start at zero, filled in later via
    /// [`MetricsCollector::update_request_timing`] once dispatch resolves.
    /// `message_count`/`msg_types`/`has_context_management` are derived from
    /// the Anthropic-shaped request `body` (already true for both
    /// `/v1/messages` and `/v1/chat/completions`, since the latter is
    /// translated to Anthropic's wire format before `Router::dispatch`).
    #[must_use]
    pub fn from_body(
        request_id: String,
        stream: bool,
        tokens_before: u64,
        body: &serde_json::Value,
        session_id: Option<String>,
    ) -> Self {
        let model = body
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();

        let messages = body.get("messages").and_then(serde_json::Value::as_array);
        let message_count = messages.map_or(0, |m| u32::try_from(m.len()).unwrap_or(u32::MAX));

        let mut type_counts: std::collections::BTreeMap<String, u32> =
            std::collections::BTreeMap::new();
        for message in messages.into_iter().flatten() {
            match message.get("content") {
                Some(serde_json::Value::String(_)) => {
                    *type_counts.entry("text".to_string()).or_insert(0) += 1;
                }
                Some(serde_json::Value::Array(blocks)) => {
                    for block in blocks {
                        let block_type = block
                            .get("type")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("unknown");
                        *type_counts.entry(block_type.to_string()).or_insert(0) += 1;
                    }
                }
                _ => {}
            }
        }
        let msg_types = serde_json::to_string(&type_counts).unwrap_or_else(|_| "{}".to_string());

        Self {
            request_id,
            timestamp: Utc::now().to_rfc3339(),
            model,
            provider: String::new(),
            tokens_before,
            tokens_after: 0,
            compressed: false,
            stream,
            msg_types,
            has_context_management: body.get("context_management").is_some(),
            message_count,
            duration_ms: 0.0,
            first_byte_ms: 0.0,
            bedrock_invocation_ms: 0,
            bedrock_first_byte_ms: 0,
            session_id,
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// LagSample — event loop lag ring buffer entry
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct LagSample {
    timestamp: Instant,
    lag_ms: f64,
}

// ────────────────────────────────────────────────────────────────────────────
// MetricsCollector — the central hub
// ────────────────────────────────────────────────────────────────────────────

/// All metrics state, designed to be held in an `Arc<MetricsCollector>` in
/// `AppState` and shared across request handlers.
pub struct MetricsCollector {
    pub counters: Arc<ProxyMetrics>,
    pub histogram: Arc<DurationHistogram>,
    pub error_tracker: Arc<ErrorTracker>,
    /// Ring buffer: last 100 requests (newest first).
    recent_requests: Mutex<VecDeque<RequestDetail>>,
    /// Ring buffer of `(request_id, original request body)`, capped and
    /// evicted in lockstep with `recent_requests` — backs the dashboard's
    /// `GET /requests/{id}?stage=original` body inspector. There is no
    /// `compressed` counterpart yet: that stage needs the `compression`
    /// module wired into dispatch, which isn't in scope here.
    original_bodies: Mutex<VecDeque<(String, serde_json::Value)>>,
    /// Rolling event-loop lag samples (15-min window).
    lag_samples: Mutex<VecDeque<LagSample>>,
    /// Most recent lag measurement in milliseconds.
    current_lag_ms: Mutex<f64>,
}

impl MetricsCollector {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            counters: Arc::new(ProxyMetrics::new()),
            histogram: Arc::new(DurationHistogram::with_default_window()),
            error_tracker: Arc::new(ErrorTracker::new()),
            recent_requests: Mutex::new(VecDeque::new()),
            original_bodies: Mutex::new(VecDeque::new()),
            lag_samples: Mutex::new(VecDeque::new()),
            current_lag_ms: Mutex::new(0.0),
        })
    }

    // ── Request ring buffer ──────────────────────────────────────────────────

    /// Push a new request detail to the ring buffer (capped at 100 entries).
    pub fn push_request(&self, detail: RequestDetail) {
        let mut buf = self
            .recent_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if buf.len() == 100 {
            buf.pop_back();
        }
        buf.push_front(detail);
    }

    /// Update timing fields on an existing request by ID.
    pub fn update_request_timing(
        &self,
        request_id: &str,
        provider: &str,
        duration_ms: f64,
        first_byte_ms: f64,
        bedrock_invocation_ms: u64,
        bedrock_first_byte_ms: u64,
    ) {
        let mut buf = self
            .recent_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for r in buf.iter_mut() {
            if r.request_id == request_id {
                r.provider = provider.to_string();
                r.duration_ms = (duration_ms * 10.0).round() / 10.0;
                r.first_byte_ms = (first_byte_ms * 10.0).round() / 10.0;
                r.bedrock_invocation_ms = bedrock_invocation_ms;
                r.bedrock_first_byte_ms = bedrock_first_byte_ms;
                return;
            }
        }
    }

    /// Caches a request's original (pre-dispatch) body, capped at 100
    /// entries in lockstep with the `recent_requests` ring buffer, for the
    /// dashboard's `GET /requests/{id}?stage=original` inspector.
    pub fn push_original_body(&self, request_id: String, body: serde_json::Value) {
        let mut buf = self
            .original_bodies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if buf.len() == 100 {
            buf.pop_back();
        }
        buf.push_front((request_id, body));
    }

    /// Looks up a cached original body by request id. `None` once evicted
    /// from the ring buffer (oldest entries fall off after 100 requests).
    #[must_use]
    pub fn get_original_body(&self, request_id: &str) -> Option<serde_json::Value> {
        let buf = self
            .original_bodies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        buf.iter()
            .find(|(id, _)| id == request_id)
            .map(|(_, body)| body.clone())
    }

    /// Get the last `n` requests (newest first).
    #[must_use]
    pub fn get_recent_requests(&self, n: usize) -> Vec<RequestDetail> {
        let buf = self
            .recent_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        buf.iter().take(n).cloned().collect()
    }

    // ── Event loop lag ───────────────────────────────────────────────────────

    /// Record an event loop lag sample, trimming entries older than 15 minutes.
    pub fn record_lag(&self, lag_ms: f64) {
        let now = Instant::now();
        let cutoff = now.checked_sub(Duration::from_mins(15)).unwrap_or(now);
        let mut samples = self
            .lag_samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        samples.push_back(LagSample {
            timestamp: now,
            lag_ms,
        });
        while samples.front().is_some_and(|s| s.timestamp < cutoff) {
            samples.pop_front();
        }
        *self
            .current_lag_ms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = lag_ms;
    }

    fn current_lag_ms(&self) -> f64 {
        *self
            .current_lag_ms
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Build the `lag_data` array for the 15-minute lag chart (1 bucket per minute).
    // `minutes` is a fixed small constant (16) and all bucket indices are
    // bounds-checked before use, so these conversions never truncate, wrap,
    // or lose meaningful precision.
    #[allow(
        clippy::cast_lossless,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap,
        clippy::cast_precision_loss
    )]
    fn lag_chart_data(&self) -> Vec<serde_json::Value> {
        let now = Instant::now();
        let minutes: u32 = 16;
        let mut max_buckets = vec![0.0f64; minutes as usize];
        let mut sum_buckets = vec![0.0f64; minutes as usize];
        let mut count_buckets = vec![0u32; minutes as usize];

        let lag_samples = self
            .lag_samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for s in lag_samples.iter() {
            if let Some(age) = now.checked_duration_since(s.timestamp) {
                let age_secs = age.as_secs();
                let total_secs = u64::from(minutes) * 60;
                if age_secs < total_secs {
                    let bucket_from_end = age_secs / 60;
                    let idx = (u64::from(minutes) - 1 - bucket_from_end) as usize;
                    if idx < max_buckets.len() {
                        if s.lag_ms > max_buckets[idx] {
                            max_buckets[idx] = s.lag_ms;
                        }
                        sum_buckets[idx] += s.lag_ms;
                        count_buckets[idx] += 1;
                    }
                }
            }
        }
        drop(lag_samples);

        (0..minutes as usize)
            .map(|i| {
                let offset_min = minutes as i64 - i as i64 - 1;
                let avg = if count_buckets[i] > 0 {
                    sum_buckets[i] / count_buckets[i] as f64
                } else {
                    0.0
                };
                json!({
                    "minute": format!("-{offset_min}m"),
                    "max_ms": (max_buckets[i] * 100.0).round() / 100.0,
                    "avg_ms": (avg * 100.0).round() / 100.0
                })
            })
            .collect()
    }

    // ── Full metrics JSON for GET /metrics ───────────────────────────────────

    /// Build the full `/metrics` JSON response (wire-compatible with the
    /// legacy Python proxy's `/metrics` endpoint).
    #[must_use]
    pub fn to_metrics_json(&self) -> serde_json::Value {
        let counters_json = self.counters.to_json();

        // RPM chart data from histogram
        let rpm_data = self.histogram.rpm_chart_data(16);

        // Latency percentiles
        let (p50, p95, p99) = self.histogram.percentiles();
        let rpm = self.histogram.requests_per_minute();

        // Recent requests
        let recent_requests: Vec<serde_json::Value> = self
            .get_recent_requests(20)
            .iter()
            .map(|r| serde_json::to_value(r).unwrap_or(serde_json::Value::Null))
            .collect();

        // Recent errors
        let recent_errors: Vec<serde_json::Value> = self
            .error_tracker
            .get_recent(20)
            .iter()
            .map(|e| serde_json::to_value(e).unwrap_or(serde_json::Value::Null))
            .collect();

        // Merge counters JSON with additional fields
        let mut result = counters_json;

        // Performance overlay
        result["performance"] = json!({
            "p50_ms": p50,
            "p95_ms": p95,
            "p99_ms": p99,
            "rpm": (rpm * 10.0).round() / 10.0
        });

        result["rpm_data"] = serde_json::Value::Array(rpm_data);
        result["lag_data"] = serde_json::Value::Array(self.lag_chart_data());
        result["current_lag_ms"] = json!(self.current_lag_ms());
        result["recent_requests"] = serde_json::Value::Array(recent_requests);
        result["recent_errors"] = serde_json::Value::Array(recent_errors);
        result["timestamp"] = json!(Utc::now().to_rfc3339());

        // `cooldowns` is merged in by the HTTP handler
        // (`observability::get_metrics`) from `Router::cooldown_snapshot()`
        // — `MetricsCollector` itself has no reference to `Router`/
        // `HealthRegistry` (Story 1.5.1).

        result
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        // Can't return Arc here — provide the unwrapped struct for completeness.
        Self {
            counters: Arc::new(ProxyMetrics::new()),
            histogram: Arc::new(DurationHistogram::with_default_window()),
            error_tracker: Arc::new(ErrorTracker::new()),
            recent_requests: Mutex::new(VecDeque::new()),
            original_bodies: Mutex::new(VecDeque::new()),
            lag_samples: Mutex::new(VecDeque::new()),
            current_lag_ms: Mutex::new(0.0),
        }
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Event loop lag probe
// ────────────────────────────────────────────────────────────────────────────

/// Spawn a background Tokio task that measures scheduler skew every second.
///
/// Each iteration sleeps for 10ms then measures how much extra time elapsed.
/// Semantically equivalent to the legacy Python proxy's `_monitor_event_loop_lag()`.
pub async fn measure_event_loop_lag() -> Duration {
    let start = Instant::now();
    sleep(Duration::from_millis(10)).await;
    let elapsed = start.elapsed();
    elapsed.saturating_sub(Duration::from_millis(10))
}

/// Long-running lag monitoring task. Call with `tokio::spawn`.
pub async fn run_lag_monitor(metrics: Arc<MetricsCollector>) {
    loop {
        let lag = measure_event_loop_lag().await;
        let lag_ms = lag.as_secs_f64() * 1000.0;
        metrics.record_lag(lag_ms);
        if lag_ms > 200.0 {
            tracing::warn!(lag_ms, "event loop lag elevated");
        }
        // Sleep for the remainder of a 1s interval (already consumed ~10ms).
        sleep(Duration::from_millis(990)).await;
    }
}
