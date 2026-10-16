//! Metrics collection: atomic counters, rolling histogram, error tracker,
//! request ring buffer, event-loop lag probe, and the `/metrics` JSON handler.

pub mod counters;
pub mod error_tracker;
pub mod histogram;
pub mod ids;
pub mod trace;

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::time::sleep;

pub use counters::{ModelOutcome, ProxyMetrics};
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
    /// The specific per-model candidate dispatch actually selected (e.g.
    /// `"deepseek/deepseek-chat-v3.1:free"`), populated post-selection via
    /// [`trace::TraceScope::record_selection`] (Story 5.1.3). `None` at
    /// construction and for every non-model-pinned route
    /// (`FallbackStrategy`/`WeightedStrategy` candidates always carry
    /// `model: None`).
    pub selected_model: Option<String>,
    /// Whether `selected_model`'s pick was an epsilon-greedy exploration
    /// choice rather than the greedy argmax one (Pre-mortem P2 #2),
    /// populated post-selection via
    /// [`trace::TraceScope::record_selection`]. `None` at
    /// construction, for every non-model-pinned route, and for a strategy
    /// that doesn't track the distinction — same nullability convention as
    /// `selected_model`.
    pub selected_model_was_exploration: Option<bool>,
    /// HTTP status of the request's terminal trace; 0 until the owning
    /// `TraceScope` finishes (or drops).
    #[serde(default)]
    pub status_code: u16,
    /// The client's `body["model"]` verbatim; `None` when the request named none.
    #[serde(default)]
    pub requested_model: Option<String>,
    /// The client's effort level (`output_config.effort`, else derived from a legacy thinking
    /// budget); `None` when the request carried neither.
    #[serde(default)]
    pub client_effort: Option<String>,
    #[serde(default)]
    pub routed_model: Option<String>,
    #[serde(default)]
    pub routed_model_source: Option<trace::RoutedModelSource>,
    #[serde(default)]
    pub routed_provider: Option<String>,
    /// `RouteReason` wire string, set when the request's terminal trace is written.
    #[serde(default)]
    pub route_reason: Option<String>,
    #[serde(default)]
    pub route_strategy: Option<String>,
    #[serde(default)]
    pub attempts: Vec<trace::AttemptRecord>,
    #[serde(default)]
    pub attempts_truncated: bool,
    #[serde(default)]
    pub iterations: u8,
    #[serde(default)]
    pub hold_retries: u8,
    #[serde(default)]
    pub trace_kind: crate::entrypoint::events::TraceKind,
    #[serde(default)]
    pub compression_status: crate::entrypoint::events::CompressionStatus,
    #[serde(default)]
    pub trace_revision: u8,
    #[serde(default)]
    pub first_byte_state: Option<crate::entrypoint::events::FirstByteState>,
    #[serde(default)]
    pub total_ms: Option<u64>,
}

/// Client-supplied `model` is untrusted; cap what the ring buffer and SSE keep.
const MAX_REQUESTED_MODEL_CHARS: usize = 256;

impl RequestDetail {
    /// The model that actually served the request: the routed model when the router recorded
    /// one, else the (client-requested) `model`. Use for per-model aggregation.
    #[must_use]
    pub fn effective_model(&self) -> &str {
        self.routed_model.as_deref().unwrap_or(&self.model)
    }

    /// Builds the initial ring-buffer entry for an incoming request, before
    /// dispatch has picked an upstream: `provider` starts empty and timing
    /// fields start at zero, filled in later via
    /// [`MetricsCollector::finish_detail`] once the request's `TraceScope` finishes.
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
        let messages = body.get("messages").and_then(serde_json::Value::as_array);
        let message_count = messages.map_or(0, |m| u32::try_from(m.len()).unwrap_or(u32::MAX));

        Self {
            request_id,
            timestamp: Utc::now().to_rfc3339(),
            model: request_model_name(body),
            provider: String::new(),
            tokens_before,
            tokens_after: 0,
            compressed: false,
            stream,
            msg_types: content_block_type_counts_json(messages),
            has_context_management: body.get("context_management").is_some(),
            message_count,
            duration_ms: 0.0,
            first_byte_ms: 0.0,
            bedrock_invocation_ms: 0,
            bedrock_first_byte_ms: 0,
            session_id,
            selected_model: None,
            selected_model_was_exploration: None,
            status_code: 0,
            requested_model: body
                .get("model")
                .and_then(serde_json::Value::as_str)
                .map(|m| m.chars().take(MAX_REQUESTED_MODEL_CHARS).collect()),
            client_effort: crate::effort::ClientEffort::from_request(body)
                .effective_level()
                .map(|l| l.as_str().to_string()),
            routed_model: None,
            routed_model_source: None,
            routed_provider: None,
            route_reason: None,
            route_strategy: None,
            attempts: Vec::new(),
            attempts_truncated: false,
            iterations: 0,
            hold_retries: 0,
            trace_kind: crate::entrypoint::events::TraceKind::default(),
            compression_status: crate::entrypoint::events::CompressionStatus::default(),
            trace_revision: 0,
            first_byte_state: None,
            total_ms: None,
        }
    }
}

fn request_model_name(body: &serde_json::Value) -> String {
    body.get("model")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string()
}

/// Counts content-block types across a request's `messages` (e.g.
/// `{"text":2,"tool_use":1}`), JSON-encoded for `RequestDetail::msg_types`.
fn content_block_type_counts_json(messages: Option<&Vec<serde_json::Value>>) -> String {
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
    serde_json::to_string(&type_counts).unwrap_or_else(|_| "{}".to_string())
}

// ────────────────────────────────────────────────────────────────────────────
// LagSample — event loop lag ring buffer entry
// ────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct LagSample {
    timestamp: Instant,
    lag_ms: f64,
}

/// Buckets a lag sample's age into one of `minutes` 1-minute-wide buckets
/// counting backward from `now` (bucket 0 = oldest, `minutes - 1` = most
/// recent), or `None` if the sample predates `now` or has aged out of the
/// window.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
fn lag_bucket_index(now: Instant, sample_at: Instant, minutes: u32) -> Option<usize> {
    let age_secs = now.checked_duration_since(sample_at)?.as_secs();
    let total_secs = u64::from(minutes) * 60;
    if age_secs >= total_secs {
        return None;
    }
    let bucket_from_end = age_secs / 60;
    let idx = (u64::from(minutes) - 1 - bucket_from_end) as usize;
    (idx < minutes as usize).then_some(idx)
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
    /// Ring buffer of `(request_id, compressed request body)`, capped at 100 entries.
    compressed_bodies: Mutex<VecDeque<(String, serde_json::Value)>>,
    /// Rolling event-loop lag samples (15-min window).
    lag_samples: Mutex<VecDeque<LagSample>>,
    /// Most recent lag measurement in milliseconds.
    current_lag_ms: Mutex<f64>,
    pub event_tx:
        Mutex<Option<tokio::sync::broadcast::Sender<crate::entrypoint::events::DashboardEvent>>>,
    #[cfg(test)]
    push_calls: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    replace_calls: std::sync::atomic::AtomicU64,
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
            compressed_bodies: Mutex::new(VecDeque::new()),
            lag_samples: Mutex::new(VecDeque::new()),
            current_lag_ms: Mutex::new(0.0),
            event_tx: Mutex::new(None),
            #[cfg(test)]
            push_calls: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            replace_calls: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub fn set_event_tx(
        &self,
        tx: tokio::sync::broadcast::Sender<crate::entrypoint::events::DashboardEvent>,
    ) {
        if let Ok(mut guard) = self.event_tx.lock() {
            *guard = Some(tx.clone());
        }
        self.error_tracker.set_event_tx(tx);
    }

    // ── Request ring buffer ──────────────────────────────────────────────────

    /// Push a new request detail to the ring buffer (capped at 100 entries).
    pub fn push_request(&self, detail: RequestDetail) {
        #[cfg(test)]
        self.push_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut buf = self
            .recent_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if buf.len() == 100 {
            buf.pop_back();
        }
        buf.push_front(detail);
    }

    /// Overwrites the ring-buffer entry with the scope's final copy; `false` when
    /// the entry has already aged out.
    fn replace_request(&self, detail: &RequestDetail) -> bool {
        #[cfg(test)]
        self.replace_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut buf = self
            .recent_requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        buf.iter_mut()
            .find(|r| r.request_id == detail.request_id)
            .map(|r| *r = detail.clone())
            .is_some()
    }

    #[cfg(test)]
    pub(crate) fn push_request_count(&self) -> u64 {
        self.push_calls.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) fn replace_request_count(&self) -> u64 {
        self.replace_calls
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Writes a request's terminal state (one ring-buffer replace) and emits exactly one
    /// `RequestTrace` built from `detail`, even if the ring-buffer entry was evicted.
    /// Called only from `TraceScope::finish` and its `Drop`, so it must stay poison-safe
    /// and non-panicking.
    pub(crate) fn finish_detail(&self, detail: &RequestDetail) {
        self.counters
            .trace_finish_total
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if !self.replace_request(detail) {
            self.counters
                .trace_finish_evicted_total
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::debug!(request_id = %detail.request_id, "trace finished after ring-buffer eviction");
        }
        let guard = self
            .event_tx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(ref tx) = *guard {
            let _ = tx.send(crate::entrypoint::events::DashboardEvent::RequestTrace(
                crate::entrypoint::events::RequestTraceData::from_detail(detail),
            ));
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

    /// Caches a request's compressed body, capped at 100 entries.
    pub fn push_compressed_body(&self, request_id: String, body: serde_json::Value) {
        let mut buf = self
            .compressed_bodies
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if buf.len() == 100 {
            buf.pop_back();
        }
        buf.push_front((request_id, body));
    }

    /// Looks up a cached compressed body by request id.
    #[must_use]
    pub fn get_compressed_body(&self, request_id: &str) -> Option<serde_json::Value> {
        let buf = self
            .compressed_bodies
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

    #[must_use]
    pub fn current_lag_ms(&self) -> f64 {
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
            let Some(idx) = lag_bucket_index(now, s.timestamp, minutes) else {
                continue;
            };
            if s.lag_ms > max_buckets[idx] {
                max_buckets[idx] = s.lag_ms;
            }
            sum_buckets[idx] += s.lag_ms;
            count_buckets[idx] += 1;
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
    ///
    /// `cooldowns` is merged in separately by the HTTP handler
    /// (`observability::get_metrics`) from `Router::cooldown_snapshot()` —
    /// `MetricsCollector` itself has no reference to `Router`/
    /// `HealthRegistry` (Story 1.5.1).
    #[must_use]
    pub fn to_metrics_json(&self) -> serde_json::Value {
        let mut result = self.counters.to_json();
        self.merge_performance_overlay(&mut result);
        self.merge_recent_activity(&mut result);
        result["timestamp"] = json!(Utc::now().to_rfc3339());
        result
    }

    /// Merges the latency-percentile/RPM/lag-chart overlay into `result`.
    fn merge_performance_overlay(&self, result: &mut serde_json::Value) {
        let rpm_data = self.histogram.rpm_chart_data(16);
        let (p50, p95, p99) = self.histogram.percentiles();
        let rpm = self.histogram.requests_per_minute();

        result["performance"] = json!({
            "p50_ms": p50,
            "p95_ms": p95,
            "p99_ms": p99,
            "rpm": (rpm * 10.0).round() / 10.0
        });
        result["rpm_data"] = serde_json::Value::Array(rpm_data);
        result["lag_data"] = serde_json::Value::Array(self.lag_chart_data());
        result["current_lag_ms"] = json!(self.current_lag_ms());
    }

    /// Merges the recent-requests/recent-errors arrays into `result`.
    fn merge_recent_activity(&self, result: &mut serde_json::Value) {
        let recent_requests: Vec<serde_json::Value> = self
            .get_recent_requests(20)
            .iter()
            .map(|r| serde_json::to_value(r).unwrap_or(serde_json::Value::Null))
            .collect();
        let recent_errors: Vec<serde_json::Value> = self
            .error_tracker
            .get_recent(20)
            .iter()
            .map(|e| serde_json::to_value(e).unwrap_or(serde_json::Value::Null))
            .collect();

        result["recent_requests"] = serde_json::Value::Array(recent_requests);
        result["recent_errors"] = serde_json::Value::Array(recent_errors);
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
            compressed_bodies: Mutex::new(VecDeque::new()),
            lag_samples: Mutex::new(VecDeque::new()),
            current_lag_ms: Mutex::new(0.0),
            event_tx: Mutex::new(None),
            #[cfg(test)]
            push_calls: std::sync::atomic::AtomicU64::new(0),
            #[cfg(test)]
            replace_calls: std::sync::atomic::AtomicU64::new(0),
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

// ────────────────────────────────────────────────────────────────────────────
// Error rate per client request
// ────────────────────────────────────────────────────────────────────────────

/// Error rate over finished traces, per client request (not per upstream attempt).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TraceErrorRate {
    /// Routed and local traces that were not cancelled by the client.
    pub requests: u64,
    /// Of those, the ones with `status_code >= 400`.
    pub errors: u64,
    /// Client-closed (499) traces, reported apart from the rate.
    pub cancelled: u64,
    /// `errors / requests`, `0.0` for an empty window.
    pub rate: f64,
}

/// Counts `status_code >= 400 && != 499` over routed and local traces. Cache hits are left
/// out of the denominator (no upstream was tried) and cancelled requests are neither error
/// nor success; unfinished entries (status 0) are skipped.
#[must_use]
pub fn trace_error_rate(traces: &[RequestDetail]) -> TraceErrorRate {
    use crate::entrypoint::events::TraceKind;
    const CANCELLED: u16 = 499;
    let (mut requests, mut errors, mut cancelled) = (0_u64, 0_u64, 0_u64);
    for trace in traces {
        if trace.trace_kind == TraceKind::CacheHit || trace.status_code == 0 {
            continue;
        }
        if trace.status_code == CANCELLED {
            cancelled += 1;
            continue;
        }
        requests += 1;
        if trace.status_code >= 400 {
            errors += 1;
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let rate = if requests == 0 {
        0.0
    } else {
        errors as f64 / requests as f64
    };
    TraceErrorRate {
        requests,
        errors,
        cancelled,
        rate,
    }
}

#[cfg(test)]
mod requested_model_tests {
    use super::*;

    #[test]
    fn from_body_should_record_client_effort_from_level_or_legacy_budget() {
        let build = |body: serde_json::Value| {
            RequestDetail::from_body("r".into(), false, 0, &body, None).client_effort
        };
        let level = serde_json::json!({"output_config": {"effort": "xhigh"}});
        assert_eq!(build(level).as_deref(), Some("xhigh"));
        let budget = serde_json::json!({"thinking": {"type": "enabled", "budget_tokens": 2048}});
        assert_eq!(build(budget).as_deref(), Some("medium"));
        assert_eq!(build(serde_json::json!({})), None);
        for body in [
            serde_json::json!({"thinking": {"type": "adaptive"}}),
            serde_json::json!({"thinking": {"type": "enabled"}}),
            serde_json::json!({"output_config": {"effort": "ludicrous"}}),
        ] {
            assert_eq!(build(body.clone()), None, "{body}");
        }
    }

    #[test]
    #[allow(clippy::unwrap_used)]
    fn from_body_should_truncate_requested_model_to_256_chars_on_a_char_boundary() {
        let long = "é".repeat(300);
        let body = serde_json::json!({ "model": long });
        let detail = RequestDetail::from_body("id".to_string(), false, 0, &body, None);
        let kept = detail.requested_model.unwrap();
        assert_eq!(kept.chars().count(), 256);
        assert!(long.starts_with(&kept));
        let short = RequestDetail::from_body(
            "id".to_string(),
            false,
            0,
            &serde_json::json!({ "model": "gpt-4o" }),
            None,
        );
        assert_eq!(short.requested_model.as_deref(), Some("gpt-4o"));
    }
}

#[cfg(test)]
mod error_rate_tests {
    use super::*;
    use crate::entrypoint::events::TraceKind;

    fn trace(kind: TraceKind, status_code: u16) -> RequestDetail {
        let mut detail =
            RequestDetail::from_body("id".to_string(), false, 0, &serde_json::json!({}), None);
        detail.trace_kind = kind;
        detail.status_code = status_code;
        detail
    }

    #[test]
    fn error_rate_should_exclude_cache_hits_and_cancelled_from_numerator_and_denominator() {
        let mut window: Vec<RequestDetail> =
            (0..10).map(|_| trace(TraceKind::Routed, 200)).collect();
        window.extend((0..2).map(|_| trace(TraceKind::CacheHit, 200)));
        window.push(trace(TraceKind::Routed, 499));
        window.push(trace(TraceKind::Routed, 503));

        let rate = trace_error_rate(&window);

        assert_eq!(rate.requests, 11);
        assert_eq!(rate.errors, 1);
        assert_eq!(rate.cancelled, 1);
        assert!((rate.rate - 1.0 / 11.0).abs() < f64::EPSILON);
    }

    #[test]
    fn error_rate_should_count_local_traces_and_be_zero_for_an_empty_window() {
        let rate = trace_error_rate(&[trace(TraceKind::Local, 502), trace(TraceKind::Local, 200)]);
        assert_eq!((rate.requests, rate.errors), (2, 1));
        assert!(trace_error_rate(&[]).rate.abs() < f64::EPSILON);
    }
}
