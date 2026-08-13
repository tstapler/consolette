//! ADR-004: rate limiting via `governor`'s GCRA limiters, one
//! `UpstreamLimiter` per upstream, admitted post-selection (see
//! `routing::router::Router::dispatch`).
//!
//! `HealthRegistry` (ADR-003) is deliberately not touched here: rate limiting
//! is a separate `AdmissionControl::admit()` seam, not a health source —
//! polling `check`/`check_n` as an `Availability` predicate would corrupt
//! governor's internal state (a successful check commits a cell).

pub mod limiter;

use std::time::Duration;

use async_trait::async_trait;
use dashmap::DashMap;
use governor::clock::DefaultClock;

use crate::config::schema::RateLimitConfig;

pub use limiter::{Admit, UpstreamLimiter};

/// Post-selection admission check for one upstream (ADR-004).
#[async_trait]
pub trait AdmissionControl: Send + Sync {
    /// `est_tokens` is a rough estimate (e.g. via tiktoken) used for the TPM
    /// dimension; upstreams with no TPM limit ignore it.
    async fn admit(&self, upstream: &str, est_tokens: u32) -> Admit;
}

/// Builds `UpstreamLimiter`s from `RateLimitConfig`, keyed by upstream name.
/// An upstream absent from `config.upstreams` has no limiter at all — every
/// dimension is unlimited (ADR-004).
pub struct RateLimiters {
    limiters: DashMap<String, UpstreamLimiter<DefaultClock>>,
}

impl RateLimiters {
    pub fn new(config: &RateLimitConfig) -> Self {
        let limiters = DashMap::new();
        for (name, limit) in &config.upstreams {
            let (on_breach, max_delay_ms) = config.resolved_breach(name);
            limiters.insert(
                name.clone(),
                UpstreamLimiter::new_with_clock(
                    limit.rpm,
                    limit.tpm,
                    on_breach,
                    Duration::from_millis(max_delay_ms),
                    DefaultClock::default(),
                ),
            );
        }
        Self { limiters }
    }
}

#[async_trait]
impl AdmissionControl for RateLimiters {
    async fn admit(&self, upstream: &str, est_tokens: u32) -> Admit {
        let Some(limiter) = self.limiters.get(upstream) else {
            return Admit::Allowed;
        };
        limiter.admit(est_tokens).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{OnBreach, RateLimit, RateLimitDefaults};
    use std::collections::HashMap;

    fn config_with(name: &str, rpm: Option<u32>, tpm: Option<u32>) -> RateLimitConfig {
        let mut upstreams = HashMap::new();
        upstreams.insert(
            name.to_string(),
            RateLimit {
                rpm,
                tpm,
                on_breach: Some(OnBreach::Shed),
                max_delay_ms: None,
            },
        );
        RateLimitConfig {
            defaults: RateLimitDefaults::default(),
            upstreams,
        }
    }

    #[tokio::test]
    async fn upstream_absent_from_config_is_unlimited() {
        let registry = RateLimiters::new(&RateLimitConfig::default());
        assert_eq!(registry.admit("anthropic", 1000).await, Admit::Allowed);
    }

    #[tokio::test]
    async fn configured_upstream_sheds_once_exhausted() {
        let config = config_with("anthropic", Some(1), None);
        let registry = RateLimiters::new(&config);
        assert_eq!(registry.admit("anthropic", 0).await, Admit::Allowed);
        assert_eq!(registry.admit("anthropic", 0).await, Admit::Shed);
    }

    #[tokio::test]
    async fn other_upstreams_unaffected_by_a_configured_one() {
        let config = config_with("anthropic", Some(1), None);
        let registry = RateLimiters::new(&config);
        assert_eq!(registry.admit("anthropic", 0).await, Admit::Allowed);
        assert_eq!(registry.admit("bedrock", 0).await, Admit::Allowed);
    }
}
