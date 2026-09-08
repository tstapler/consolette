//! Per-model rolling stats for the composite OpenRouter scoring strategy
//! (ADR-003): a rolling error-rate tracker (`RollingErrorRate`) plus the
//! bundle (`ModelStats`) that pairs it with the existing latency
//! `DurationHistogram`.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::metrics::DurationHistogram;

/// A rolling-window error-rate tracker, mirroring `DurationHistogram`'s
/// shape and `PoisonError` recovery discipline.
///
/// Thread-safe via an internal `Mutex`. The window defaults to 15 minutes.
pub struct RollingErrorRate {
    /// (`sample_time`, `success`)
    samples: Mutex<VecDeque<(Instant, bool)>>,
    window: Duration,
}

impl RollingErrorRate {
    /// Create a new tracker with the given rolling window.
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self {
            samples: Mutex::new(VecDeque::new()),
            window,
        }
    }

    /// Create a new tracker with a default 15-minute window.
    #[must_use]
    pub fn with_default_window() -> Self {
        Self::new(Duration::from_mins(15))
    }

    /// Record a request outcome. Drops samples older than the window.
    pub fn record(&self, success: bool) {
        let now = Instant::now();
        let cutoff = now.checked_sub(self.window).unwrap_or(now);
        // A poisoned mutex only happens if another lock holder panicked; recovering
        // the inner data is safe since it is left in a structurally valid state.
        let mut samples = self
            .samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        samples.push_back((now, success));
        // Trim expired samples from the front.
        while samples.front().is_some_and(|(t, _)| *t < cutoff) {
            samples.pop_front();
        }
    }

    /// Fraction of samples in the window that were failures.
    ///
    /// Returns `None` on cold start (no samples in the window) — distinct
    /// from a real `0.0` error rate.
    #[must_use]
    // `error_count`/`n` are both bounded by the number of requests within a
    // 15-minute window, far too small to lose precision as `f64`.
    #[allow(clippy::cast_precision_loss)]
    pub fn error_rate(&self) -> Option<f64> {
        let now = Instant::now();
        let cutoff = now.checked_sub(self.window).unwrap_or(now);
        let samples = self
            .samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        let in_window: Vec<bool> = samples
            .iter()
            .filter(|(t, _)| *t >= cutoff)
            .map(|(_, success)| *success)
            .collect();

        if in_window.is_empty() {
            return None;
        }

        let error_count = in_window.iter().filter(|success| !**success).count();
        Some(error_count as f64 / in_window.len() as f64)
    }

    /// Count of samples currently in the rolling window.
    #[must_use]
    pub fn sample_count(&self) -> usize {
        let now = Instant::now();
        let cutoff = now.checked_sub(self.window).unwrap_or(now);
        let samples = self
            .samples
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        samples.iter().filter(|(t, _)| *t >= cutoff).count()
    }
}

/// Per-model rolling state: a latency histogram plus an error-rate tracker.
///
/// One `ModelStats` per model id, held in the `DashMap<String, ModelStats>`
/// owned by `OpenrouterScoringStrategy`.
pub struct ModelStats {
    pub latency: DurationHistogram,
    pub errors: RollingErrorRate,
}

impl ModelStats {
    /// Construct a new, cold-start `ModelStats` — both trackers empty.
    #[must_use]
    pub fn new() -> Self {
        Self {
            latency: DurationHistogram::with_default_window(),
            errors: RollingErrorRate::with_default_window(),
        }
    }
}

impl Default for ModelStats {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_rate_should_return_none_when_cold_start() {
        let tracker = RollingErrorRate::with_default_window();
        assert_eq!(tracker.error_rate(), None);
    }

    #[test]
    fn error_rate_should_return_correct_fraction() {
        let tracker = RollingErrorRate::with_default_window();
        tracker.record(true);
        tracker.record(true);
        tracker.record(true);
        tracker.record(false);
        assert_eq!(tracker.error_rate(), Some(0.25));
    }

    #[test]
    fn error_rate_should_exclude_samples_older_than_window() {
        let tracker = RollingErrorRate::new(Duration::from_millis(50));
        tracker.record(false);
        std::thread::sleep(Duration::from_millis(100));
        // A fresh success sample should trim the stale failure on write,
        // leaving only this one success in the window.
        tracker.record(true);
        assert_eq!(tracker.error_rate(), Some(0.0));
    }

    #[test]
    fn model_stats_new_should_report_cold_start_on_both_trackers() {
        let stats = ModelStats::new();
        assert_eq!(stats.latency.sample_count(), 0);
        assert_eq!(stats.errors.error_rate(), None);
    }
}
