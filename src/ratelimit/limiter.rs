//! `UpstreamLimiter`: per-upstream RPM/TPM admission over `governor`'s GCRA
//! limiters (ADR-004).
//!
//! Each dimension (RPM, TPM) is a separate direct (non-keyed) limiter —
//! there's exactly one upstream per `UpstreamLimiter`, so keying by anything
//! would be pointless. TPM is checked before RPM (ADR-004: check the
//! dimension more likely to deny first, since a request that fails on TPM
//! shouldn't also consume an RPM cell it can't use).

use std::num::NonZeroU32;
use std::time::Duration;

use governor::clock::{Clock, DefaultClock, ReasonablyRealtime, Reference};
use governor::middleware::NoOpMiddleware;
use governor::state::{InMemoryState, NotKeyed};
use governor::{Quota, RateLimiter};

use crate::config::schema::OnBreach;

type DirectLimiter<C> =
    RateLimiter<NotKeyed, InMemoryState, C, NoOpMiddleware<<C as Clock>::Instant>>;

/// Outcome of an admission check (ADR-004).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admit {
    /// Admitted with no wait.
    Allowed,
    /// Admitted after waiting `Duration` (delay mode only).
    Delayed(Duration),
    /// Denied — caller should shed (fail over to another upstream).
    Shed,
}

/// Per-upstream RPM/TPM limiters plus the resolved breach policy.
pub struct UpstreamLimiter<C: Clock = DefaultClock> {
    rpm: Option<DirectLimiter<C>>,
    tpm: Option<DirectLimiter<C>>,
    on_breach: OnBreach,
    max_delay: Duration,
    clock: C,
}

fn quota_per_minute(n: u32) -> Option<Quota> {
    NonZeroU32::new(n).map(Quota::per_minute)
}

impl<C: Clock + Clone> UpstreamLimiter<C> {
    pub fn new_with_clock(
        rpm: Option<u32>,
        tpm: Option<u32>,
        on_breach: OnBreach,
        max_delay: Duration,
        clock: C,
    ) -> Self {
        Self {
            rpm: rpm
                .and_then(quota_per_minute)
                .map(|q| RateLimiter::direct_with_clock(q, clock.clone())),
            tpm: tpm
                .and_then(quota_per_minute)
                .map(|q| RateLimiter::direct_with_clock(q, clock.clone())),
            on_breach,
            max_delay,
            clock,
        }
    }

    /// Non-blocking admission check (shed mode, and the fast path for delay
    /// mode). Checks TPM before RPM; short-circuits on the first denial so a
    /// TPM-denied request doesn't also consume an RPM cell it can't use.
    fn try_admit_sync(&self, est_tokens: u32) -> Result<(), ()> {
        if let (Some(tpm), Some(n)) = (&self.tpm, NonZeroU32::new(est_tokens)) {
            match tpm.check_n(n) {
                Ok(Ok(())) => {}
                _ => return Err(()),
            }
        }
        if let Some(rpm) = &self.rpm {
            if rpm.check().is_err() {
                return Err(());
            }
        }
        Ok(())
    }
}

impl<C: Clock + ReasonablyRealtime + Clone> UpstreamLimiter<C> {
    /// Full admission check, applying `on_breach` (ADR-004). Delay mode
    /// awaits `until_ready`/`until_n_ready` up to `max_delay` before
    /// shedding; a request too large to ever fit (`InsufficientCapacity`)
    /// sheds immediately regardless of mode.
    pub async fn admit(&self, est_tokens: u32) -> Admit {
        if self.try_admit_sync(est_tokens).is_ok() {
            return Admit::Allowed;
        }
        if self.on_breach == OnBreach::Shed {
            return Admit::Shed;
        }

        let start = self.clock.now();
        if let Some(n) = NonZeroU32::new(est_tokens) {
            if let Some(tpm) = &self.tpm {
                match tokio::time::timeout(self.max_delay, tpm.until_n_ready(n)).await {
                    Ok(Ok(())) => {}
                    _ => return Admit::Shed,
                }
            }
        }
        if let Some(rpm) = &self.rpm {
            if tokio::time::timeout(self.max_delay, rpm.until_ready())
                .await
                .is_err()
            {
                return Admit::Shed;
            }
        }
        let elapsed = self.clock.now().duration_since(start);
        Admit::Delayed(elapsed.into())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use governor::clock::FakeRelativeClock;

    fn limiter(
        rpm: Option<u32>,
        tpm: Option<u32>,
        on_breach: OnBreach,
    ) -> UpstreamLimiter<FakeRelativeClock> {
        UpstreamLimiter::new_with_clock(
            rpm,
            tpm,
            on_breach,
            Duration::from_millis(50),
            FakeRelativeClock::default(),
        )
    }

    #[test]
    fn unbounded_dimension_never_denies() {
        let l = limiter(None, None, OnBreach::Shed);
        assert!(l.try_admit_sync(1000).is_ok());
    }

    #[test]
    fn rpm_denies_after_burst_exhausted() {
        let l = limiter(Some(1), None, OnBreach::Shed);
        assert!(l.try_admit_sync(0).is_ok());
        assert!(l.try_admit_sync(0).is_err());
    }

    #[test]
    fn tpm_denies_when_estimate_exceeds_budget() {
        let l = limiter(None, Some(100), OnBreach::Shed);
        assert!(l.try_admit_sync(50).is_ok());
        assert!(l.try_admit_sync(60).is_err());
    }

    #[test]
    fn tpm_short_circuits_before_consuming_rpm() {
        let l = limiter(Some(5), Some(10), OnBreach::Shed);
        assert!(l.try_admit_sync(20).is_err());
        // RPM cell must be untouched — a fresh check for a cheap request
        // still succeeds.
        assert!(l.try_admit_sync(1).is_ok());
    }

    // `admit` requires `ReasonablyRealtime`, which `FakeRelativeClock`
    // deliberately doesn't implement (it only advances when told to) — so
    // async admit tests below use the real `DefaultClock` with small
    // durations instead.

    fn real_limiter(
        rpm: Option<u32>,
        tpm: Option<u32>,
        on_breach: OnBreach,
        max_delay: Duration,
    ) -> UpstreamLimiter<DefaultClock> {
        UpstreamLimiter::new_with_clock(rpm, tpm, on_breach, max_delay, DefaultClock::default())
    }

    #[tokio::test]
    async fn shed_mode_denies_immediately_without_waiting() {
        let l = real_limiter(Some(1), None, OnBreach::Shed, Duration::from_millis(50));
        assert_eq!(l.admit(0).await, Admit::Allowed);
        assert_eq!(l.admit(0).await, Admit::Shed);
    }

    #[tokio::test]
    async fn allowed_when_within_budget() {
        let l = real_limiter(
            Some(5),
            Some(1000),
            OnBreach::Delay,
            Duration::from_millis(50),
        );
        assert_eq!(l.admit(10).await, Admit::Allowed);
    }

    #[tokio::test]
    async fn delay_mode_waits_then_admits() {
        // A hand-built fast-refill quota (50ms/cell) so the wait is short
        // enough for a test but still long enough to assert `Delayed(d) > 0`.
        let quota = Quota::with_period(Duration::from_millis(50))
            .unwrap()
            .allow_burst(NonZeroU32::new(1).unwrap());
        let l: UpstreamLimiter<DefaultClock> = UpstreamLimiter {
            rpm: Some(RateLimiter::direct(quota)),
            tpm: None,
            on_breach: OnBreach::Delay,
            max_delay: Duration::from_secs(2),
            clock: DefaultClock::default(),
        };
        assert_eq!(l.admit(0).await, Admit::Allowed);
        match l.admit(0).await {
            Admit::Delayed(d) => assert!(d > Duration::ZERO),
            other => panic!("expected Delayed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn delay_mode_sheds_after_max_delay_timeout() {
        let l = real_limiter(Some(1), None, OnBreach::Delay, Duration::from_millis(1));
        assert_eq!(l.admit(0).await, Admit::Allowed);
        assert_eq!(l.admit(0).await, Admit::Shed);
    }
}
