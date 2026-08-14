//! Rolling latency histogram with 15-minute sliding window.
//!
//! Stores (timestamp, `duration_ms`) pairs in a `VecDeque` and trims entries
//! older than the window on every operation. Computes `p50`/`p95`/`p99` and rpm.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// A rolling-window histogram for request latency.
///
/// Thread-safe via an internal `Mutex`. The window defaults to 15 minutes.
pub struct DurationHistogram {
    /// (`sample_time`, `duration_ms`)
    samples: Mutex<VecDeque<(Instant, u64)>>,
    window: Duration,
}

impl DurationHistogram {
    /// Create a new histogram with the given rolling window.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            samples: Mutex::new(VecDeque::new()),
            window,
        }
    }

    /// Create a new histogram with a default 15-minute window.
    #[must_use]
    pub fn with_default_window() -> Self {
        Self::new(Duration::from_mins(15))
    }

    /// Record a new duration sample. Drops samples older than the window.
    pub fn record(&self, duration_ms: u64) {
        let now = Instant::now();
        let cutoff = now.checked_sub(self.window).unwrap_or(now);
        // A poisoned mutex only happens if another lock holder panicked; recovering
        // the inner data is safe since it is left in a structurally valid state.
        let mut samples = self
            .samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        samples.push_back((now, duration_ms));
        // Trim expired samples from the front.
        while samples.front().is_some_and(|(t, _)| *t < cutoff) {
            samples.pop_front();
        }
    }

    /// Compute (p50, p95, p99) over the current window.
    ///
    /// Returns `(0, 0, 0)` if the window is empty.
    ///
    /// # Panics
    ///
    /// Never panics in practice: `values` is checked non-empty before indexing,
    /// and the computed index is clamped to `values.len() - 1`.
    #[must_use]
    // `n` (a sample count bounded by the 15-minute window) and the percentile
    // fraction are both far too small to lose precision as `f64`.
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_sign_loss,
        clippy::cast_possible_truncation
    )]
    pub fn percentiles(&self) -> (u64, u64, u64) {
        let now = Instant::now();
        let cutoff = now.checked_sub(self.window).unwrap_or(now);
        let samples = self
            .samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut values: Vec<u64> = samples
            .iter()
            .filter(|(t, _)| *t >= cutoff)
            .map(|(_, d)| *d)
            .collect();

        if values.is_empty() {
            return (0, 0, 0);
        }

        values.sort_unstable();
        let n = values.len();
        let p = |pct: f64| values[((n as f64 * pct / 100.0) as usize).min(n - 1)];
        (p(50.0), p(95.0), p(99.0))
    }

    /// Requests per minute over the last 60 seconds.
    #[must_use]
    // The sample count within a 60-second window is trivially small relative
    // to `f64`'s exact-integer range.
    #[allow(clippy::cast_precision_loss)]
    pub fn requests_per_minute(&self) -> f64 {
        let now = Instant::now();
        let one_min_ago = now.checked_sub(Duration::from_mins(1)).unwrap_or(now);
        let samples = self
            .samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let count = samples.iter().filter(|(t, _)| *t >= one_min_ago).count();
        count as f64
    }

    /// Build RPM chart data: one bucket per minute for the last `minutes` minutes.
    ///
    /// Returns `Vec<(minute_label_string, count)>`.
    ///
    /// # Panics
    ///
    /// Never panics in practice: `idx` is bounds-checked against `buckets.len()`
    /// before indexing.
    #[must_use]
    // `minutes` is a small chart-window parameter (tens at most) and loop
    // indices are bounded by it, so these conversions never truncate or wrap.
    #[allow(
        clippy::cast_lossless,
        clippy::cast_possible_truncation,
        clippy::cast_possible_wrap
    )]
    pub fn rpm_chart_data(&self, minutes: u32) -> Vec<serde_json::Value> {
        let now = Instant::now();
        let samples = self
            .samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let mut buckets = vec![0u64; minutes as usize];
        let total_secs = u64::from(minutes) * 60;

        for (t, _) in samples.iter() {
            // How many seconds ago was this sample?
            if let Some(age) = now.checked_duration_since(*t) {
                let age_secs = age.as_secs();
                if age_secs < total_secs {
                    // Bucket index: 0 = oldest, last = most recent
                    let bucket_from_end = age_secs / 60;
                    let idx = (u64::from(minutes) - 1 - bucket_from_end) as usize;
                    if idx < buckets.len() {
                        buckets[idx] += 1;
                    }
                }
            }
        }

        buckets
            .iter()
            .enumerate()
            .map(|(i, &count)| {
                // Label as minutes-ago offset (simple HH:MM-style label would need system time)
                let offset_min = minutes as i64 - i as i64 - 1;
                serde_json::json!({
                    "minute": format!("-{offset_min}m"),
                    "requests": count
                })
            })
            .collect()
    }

    /// Build lag chart data for the last `minutes` minutes (stub — returns zeroed buckets).
    ///
    /// Lag data is tracked separately via the event loop lag probe; this method
    /// returns placeholder data so the dashboard chart renders without errors.
    #[must_use]
    pub fn lag_chart_data(&self, minutes: u32) -> Vec<serde_json::Value> {
        (0..minutes)
            .map(|i| {
                let offset_min = i64::from(minutes) - i64::from(i) - 1;
                serde_json::json!({
                    "minute": format!("-{offset_min}m"),
                    "max_ms": 0.0,
                    "avg_ms": 0.0
                })
            })
            .collect()
    }
}
