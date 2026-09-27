//! `HealthRegistry`: per-upstream circuit breaker & cooldown state (ADR-003).
//!
//! Generalizes per-upstream health to a 3-state Circuit Breaker:
//! - `Closed` (Normal): Requests routed normally. Consecutive failures trip to `Open`.
//! - `Open` (Tripped): Upstream unavailable for routing (`is_available` returns false).
//!   After cooldown, transitions to `HalfOpen`.
//! - `HalfOpen` (Testing): Allows single probes / test traffic. 2 consecutive successes
//!   transition back to `Closed`. A failure trips back to `Open`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

/// The state of a circuit breaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CircuitState {
    Closed,
    Open,
    HalfOpen,
}

/// State machine tracking failures and successes for a single upstream.
#[derive(Debug, Clone)]
pub struct CircuitBreaker {
    state: CircuitState,
    consecutive_failures: u32,
    consecutive_successes: u32,
    failure_threshold: u32,
    success_threshold: u32,
    open_until: Option<Instant>,
    cooldown_duration: Duration,
}

impl CircuitBreaker {
    #[must_use]
    pub fn new(failure_threshold: u32, cooldown_duration: Duration) -> Self {
        Self {
            state: CircuitState::Closed,
            consecutive_failures: 0,
            consecutive_successes: 0,
            failure_threshold: failure_threshold.max(1),
            success_threshold: 2,
            open_until: None,
            cooldown_duration,
        }
    }

    pub fn state(&mut self, now: Instant) -> CircuitState {
        if self.state == CircuitState::Open {
            if let Some(until) = self.open_until {
                if now >= until {
                    self.state = CircuitState::HalfOpen;
                    self.consecutive_successes = 0;
                    self.open_until = None;
                }
            }
        }
        self.state
    }

    pub fn record_success(&mut self, now: Instant) {
        match self.state(now) {
            CircuitState::Closed => {
                self.consecutive_failures = 0;
            }
            CircuitState::HalfOpen => {
                self.consecutive_successes += 1;
                if self.consecutive_successes >= self.success_threshold {
                    self.state = CircuitState::Closed;
                    self.consecutive_failures = 0;
                    self.consecutive_successes = 0;
                    self.open_until = None;
                }
            }
            CircuitState::Open => {}
        }
    }

    pub fn record_failure(&mut self, now: Instant) {
        match self.state(now) {
            CircuitState::Closed => {
                self.consecutive_failures += 1;
                if self.consecutive_failures >= self.failure_threshold {
                    self.state = CircuitState::Open;
                    self.open_until = Some(now + self.cooldown_duration);
                    self.consecutive_failures = 0;
                    self.consecutive_successes = 0;
                }
            }
            CircuitState::HalfOpen => {
                self.state = CircuitState::Open;
                self.open_until = Some(now + self.cooldown_duration);
                self.consecutive_failures = 0;
                self.consecutive_successes = 0;
            }
            CircuitState::Open => {
                self.open_until = Some(now + self.cooldown_duration);
            }
        }
    }

    pub fn trip(&mut self, now: Instant, override_duration: Option<Duration>) {
        let duration = override_duration.unwrap_or(self.cooldown_duration);
        self.state = CircuitState::Open;
        self.open_until = Some(now + duration);
        self.consecutive_failures = 0;
        self.consecutive_successes = 0;
    }

    #[must_use]
    pub fn is_available(&mut self, now: Instant) -> bool {
        self.state(now) != CircuitState::Open
    }

    #[must_use]
    pub fn remaining_secs(&mut self, now: Instant) -> u64 {
        if self.state(now) == CircuitState::Open {
            if let Some(until) = self.open_until {
                if until > now {
                    return (until - now).as_secs();
                }
            }
        }
        0
    }
}

/// Health/cooldown-only availability check.
pub trait Availability {
    fn is_available(&self, idx: usize) -> bool;
}

/// Per-upstream circuit breaker registry, keyed by upstream index.
pub struct HealthRegistry {
    breakers: DashMap<usize, CircuitBreaker>,
    cooldown_duration: Duration,
    failure_threshold: u32,
    /// Upstreams that never enter cooldown (e.g. Bedrock). Defaults to allowed when unset.
    can_cooldown: DashMap<usize, bool>,
}

impl HealthRegistry {
    #[must_use]
    pub fn new(cooldown_secs: u64) -> Self {
        Self::with_thresholds(cooldown_secs, 3)
    }

    #[must_use]
    pub fn with_thresholds(cooldown_secs: u64, failure_threshold: u32) -> Self {
        Self {
            breakers: DashMap::new(),
            cooldown_duration: Duration::from_secs(cooldown_secs),
            failure_threshold,
            can_cooldown: DashMap::new(),
        }
    }

    pub fn set_can_cooldown(&self, idx: usize, allowed: bool) {
        self.can_cooldown.insert(idx, allowed);
    }

    pub fn record_success(&self, idx: usize) {
        let mut entry = self.breakers.entry(idx).or_insert_with(|| {
            CircuitBreaker::new(self.failure_threshold, self.cooldown_duration)
        });
        entry.record_success(Instant::now());
    }

    pub fn record_failure(&self, idx: usize) {
        let allowed = self.can_cooldown.get(&idx).is_none_or(|v| *v);
        if !allowed {
            return;
        }
        let mut entry = self.breakers.entry(idx).or_insert_with(|| {
            CircuitBreaker::new(self.failure_threshold, self.cooldown_duration)
        });
        entry.record_failure(Instant::now());
    }

    pub fn trip(&self, idx: usize, override_duration: Option<Duration>) {
        let allowed = self.can_cooldown.get(&idx).is_none_or(|v| *v);
        if !allowed {
            return;
        }
        let mut entry = self.breakers.entry(idx).or_insert_with(|| {
            CircuitBreaker::new(self.failure_threshold, self.cooldown_duration)
        });
        entry.trip(Instant::now(), override_duration);
    }

    #[must_use]
    pub fn remaining_secs(&self, idx: usize) -> u64 {
        if let Some(mut entry) = self.breakers.get_mut(&idx) {
            entry.remaining_secs(Instant::now())
        } else {
            0
        }
    }

    #[must_use]
    pub fn get_circuit_state(&self, idx: usize) -> CircuitState {
        if let Some(mut entry) = self.breakers.get_mut(&idx) {
            entry.state(Instant::now())
        } else {
            CircuitState::Closed
        }
    }
}

impl Availability for HealthRegistry {
    fn is_available(&self, idx: usize) -> bool {
        let allowed = self.can_cooldown.get(&idx).is_none_or(|v| *v);
        if !allowed {
            return true;
        }
        if let Some(mut entry) = self.breakers.get_mut(&idx) {
            entry.is_available(Instant::now())
        } else {
            true
        }
    }
}

/// Lightweight background task that periodically probes upstreams for health.
pub async fn run_health_prober(
    router: Arc<arc_swap::ArcSwap<crate::routing::router::Router>>,
    interval: Duration,
) {
    let mut timer = tokio::time::interval(interval);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        timer.tick().await;
        let current_router = router.load();
        for candidate in current_router.candidates() {
            let index = candidate.index;
            let model_id = candidate.model.as_deref().unwrap_or("gpt-3.5-turbo");
            let probe_body = serde_json::json!({
                "model": model_id,
                "messages": [{"role": "user", "content": "ping"}],
                "max_tokens": 1
            });

            match current_router.probe_upstream(index, probe_body).await {
                Ok(_) => {
                    current_router.health.record_success(index);
                }
                Err(err) => {
                    if !err.is_validation() && !err.is_auth() {
                        current_router.health.record_failure(index);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn circuit_breaker_state_transitions() {
        let mut cb = CircuitBreaker::new(3, Duration::from_millis(50));
        let mut now = Instant::now();

        // Closed initially
        assert_eq!(cb.state(now), CircuitState::Closed);
        assert!(cb.is_available(now));

        // 2 failures: still closed
        cb.record_failure(now);
        cb.record_failure(now);
        assert_eq!(cb.state(now), CircuitState::Closed);

        // 3rd failure: trips to Open
        cb.record_failure(now);
        assert_eq!(cb.state(now), CircuitState::Open);
        assert!(!cb.is_available(now));

        // Before cooldown: stays Open
        assert_eq!(cb.state(now), CircuitState::Open);

        // After cooldown: transitions to HalfOpen
        now += Duration::from_millis(60);
        assert_eq!(cb.state(now), CircuitState::HalfOpen);
        assert!(cb.is_available(now));

        // Success 1 in HalfOpen: stays HalfOpen
        cb.record_success(now);
        assert_eq!(cb.state(now), CircuitState::HalfOpen);

        // Success 2 in HalfOpen: transitions back to Closed
        cb.record_success(now);
        assert_eq!(cb.state(now), CircuitState::Closed);
    }

    #[test]
    fn circuit_breaker_half_open_failure_re_trips() {
        let mut cb = CircuitBreaker::new(3, Duration::from_millis(50));
        let mut now = Instant::now();

        cb.trip(now, None);
        assert_eq!(cb.state(now), CircuitState::Open);

        now += Duration::from_millis(60);
        assert_eq!(cb.state(now), CircuitState::HalfOpen);

        // Failure in HalfOpen immediately trips back to Open
        cb.record_failure(now);
        assert_eq!(cb.state(now), CircuitState::Open);
        assert!(!cb.is_available(now));
    }

    #[test]
    fn unknown_upstream_is_available_by_default() {
        let registry = HealthRegistry::new(300);
        assert!(registry.is_available(0));
    }

    #[test]
    fn tripped_upstream_is_unavailable_until_expiry() {
        let registry = HealthRegistry::new(300);
        registry.trip(0, Some(Duration::from_millis(1)));
        assert!(!registry.is_available(0));
        std::thread::sleep(Duration::from_millis(20));
        assert!(registry.is_available(0));
    }

    #[test]
    fn can_cooldown_false_prevents_trip() {
        let registry = HealthRegistry::new(300);
        registry.set_can_cooldown(1, false);
        registry.trip(1, None);
        assert!(registry.is_available(1));
    }

    #[test]
    fn failure_threshold_trips_registry() {
        let registry = HealthRegistry::with_thresholds(300, 2);
        assert_eq!(registry.get_circuit_state(0), CircuitState::Closed);

        registry.record_failure(0);
        assert_eq!(registry.get_circuit_state(0), CircuitState::Closed);

        registry.record_failure(0);
        assert_eq!(registry.get_circuit_state(0), CircuitState::Open);
        assert!(!registry.is_available(0));
    }
}
