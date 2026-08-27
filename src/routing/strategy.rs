//! `RoutingStrategy`: pure, health-blind selection (ADR-003).
//!
//! Receives only an already health-filtered candidate slice — cooldown state
//! is invisible here, which is what lets `FallbackStrategy` and
//! `WeightedStrategy` share the same `HealthRegistry` cooldown machinery in
//! `super::health` without knowing about it.

use rand::distributions::{Distribution, WeightedIndex};
use rand::thread_rng;

/// A candidate upstream, as seen by a `RoutingStrategy`.
///
/// `index` is the upstream's position in `Config.upstreams` — the same key
/// `HealthRegistry` uses — so a strategy's selection maps straight back to
/// health/cooldown state without a name lookup.
#[derive(Debug, Clone, PartialEq)]
pub struct UpstreamRef {
    pub index: usize,
    pub name: String,
    pub weight: f64,
    pub model: Option<String>,
}

/// Pure selection over an already health-filtered candidate slice.
pub trait RoutingStrategy: Send + Sync {
    fn select(&self, healthy: &[UpstreamRef]) -> Option<UpstreamRef>;
}

/// Ordered fallback: first healthy candidate wins. Candidates arrive in
/// config order, so "first healthy" reproduces primary-then-fallback
/// behavior exactly.
pub struct FallbackStrategy;

impl RoutingStrategy for FallbackStrategy {
    fn select(&self, healthy: &[UpstreamRef]) -> Option<UpstreamRef> {
        healthy.first().cloned()
    }
}

/// Weighted split (OpenRouter-style). A cooled-down peer is simply absent
/// from `healthy`, so survivors keep their relative proportions — proportional
/// redistribution falls out for free, with no explicit reweighting math.
pub struct WeightedStrategy;

impl RoutingStrategy for WeightedStrategy {
    fn select(&self, healthy: &[UpstreamRef]) -> Option<UpstreamRef> {
        if healthy.is_empty() {
            return None;
        }
        // `.max(1)`: an all-zero-weight slice would otherwise make
        // `WeightedIndex::new` fail instead of selecting uniformly.
        // Cast is intentional fixed-point quantization: `.max(0.0)` already
        // rules out sign loss, and sub-milliweight truncation is negligible.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let weights: Vec<u64> = healthy
            .iter()
            .map(|u| ((u.weight.max(0.0)) * 1000.0) as u64)
            .map(|w| w.max(1))
            .collect();
        let dist = WeightedIndex::new(&weights).ok()?;
        let idx = dist.sample(&mut thread_rng());
        healthy.get(idx).cloned()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn upstream(index: usize, name: &str, weight: f64) -> UpstreamRef {
        UpstreamRef {
            index,
            name: name.to_string(),
            weight,
            model: None,
        }
    }

    #[test]
    fn fallback_selects_first_healthy() {
        let candidates = vec![upstream(0, "a", 1.0), upstream(1, "b", 1.0)];
        let selected = FallbackStrategy.select(&candidates);
        assert_eq!(selected.unwrap().name, "a");
    }

    #[test]
    fn fallback_returns_none_when_empty() {
        assert!(FallbackStrategy.select(&[]).is_none());
    }

    #[test]
    fn weighted_returns_none_when_empty() {
        assert!(WeightedStrategy.select(&[]).is_none());
    }

    #[test]
    fn weighted_always_selects_from_candidates() {
        let candidates = vec![upstream(0, "a", 0.7), upstream(1, "b", 0.3)];
        for _ in 0..50 {
            let selected = WeightedStrategy.select(&candidates).unwrap();
            assert!(candidates.iter().any(|c| c.index == selected.index));
        }
    }

    #[test]
    fn weighted_handles_all_zero_weights() {
        let candidates = vec![upstream(0, "a", 0.0), upstream(1, "b", 0.0)];
        assert!(WeightedStrategy.select(&candidates).is_some());
    }
}
