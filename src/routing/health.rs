//! `HealthRegistry`: per-upstream cooldown state (ADR-003).
//!
//! Generalizes the old single-slot `FallbackState` (primary-only) to N
//! upstreams, keyed by index into `Config.upstreams`. Reuses the same atomic
//! check-and-clear cooldown semantics via `DashMap::get_mut`'s exclusive
//! per-key guard.
//!
//! Hard rule: never hold a `DashMap` guard across `.await`. All health ops
//! here are pure-sync nanosecond `Instant` comparisons, so this holds by
//! construction as long as no `.await` is added inside these methods.

use std::time::{Duration, Instant};

use dashmap::DashMap;

#[derive(Debug, Clone, Copy)]
enum ProviderState {
    Normal,
    Cooldown { until: Instant },
}

/// Health/cooldown-only availability check. `HealthRegistry` is the only
/// implementor — rate limiting is deliberately not a source here (ADR-003):
/// the router applies this as a pre-selection filter, then rate limiting
/// integrates post-selection via a separate `AdmissionControl::admit()` call
/// (ADR-004).
pub trait Availability {
    fn is_available(&self, idx: usize) -> bool;
}

/// Per-upstream cooldown state, keyed by upstream index.
pub struct HealthRegistry {
    state: DashMap<usize, ProviderState>,
    cooldown_duration: Duration,
    /// Upstreams that never enter cooldown (e.g. Bedrock — ADR-003: "Bedrock
    /// never cools down"). Defaults to allowed when unset.
    can_cooldown: DashMap<usize, bool>,
}

impl HealthRegistry {
    #[must_use]
    pub fn new(cooldown_secs: u64) -> Self {
        Self {
            state: DashMap::new(),
            cooldown_duration: Duration::from_secs(cooldown_secs),
            can_cooldown: DashMap::new(),
        }
    }

    /// Marks whether `idx` is allowed to enter cooldown at all.
    pub fn set_can_cooldown(&self, idx: usize, allowed: bool) {
        self.can_cooldown.insert(idx, allowed);
    }

    /// Trips `idx` into cooldown for `override_duration` (e.g. from a parsed
    /// `Retry-After`) or the registry default. A no-op if `idx` was marked
    /// `can_cooldown = false`.
    pub fn trip(&self, idx: usize, override_duration: Option<Duration>) {
        let allowed = self.can_cooldown.get(&idx).is_none_or(|v| *v);
        if !allowed {
            return;
        }
        let duration = override_duration.unwrap_or(self.cooldown_duration);
        let until = Instant::now() + duration;
        self.state.insert(idx, ProviderState::Cooldown { until });
    }

    /// Remaining cooldown in seconds (0 if not in cooldown) — used by
    /// metrics reporting per-upstream cooldown state.
    #[must_use]
    pub fn remaining_secs(&self, idx: usize) -> u64 {
        match self.state.get(&idx).map(|s| *s) {
            Some(ProviderState::Cooldown { until }) => {
                let now = Instant::now();
                if until > now {
                    (until - now).as_secs()
                } else {
                    0
                }
            }
            _ => 0,
        }
    }
}

impl Availability for HealthRegistry {
    /// Atomically checks-and-clears an expired cooldown, TOCTOU-safe via
    /// `DashMap::get_mut`'s exclusive per-key guard.
    fn is_available(&self, idx: usize) -> bool {
        let Some(mut entry) = self.state.get_mut(&idx) else {
            return true;
        };
        match *entry {
            ProviderState::Normal => true,
            ProviderState::Cooldown { until } => {
                if Instant::now() >= until {
                    *entry = ProviderState::Normal;
                    true
                } else {
                    false
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn remaining_secs_reports_zero_when_normal() {
        let registry = HealthRegistry::new(300);
        assert_eq!(registry.remaining_secs(0), 0);
    }

    #[test]
    fn remaining_secs_reports_nonzero_during_cooldown() {
        let registry = HealthRegistry::new(300);
        registry.trip(0, Some(Duration::from_mins(1)));
        assert!(registry.remaining_secs(0) > 0);
    }
}
