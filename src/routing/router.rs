//! `Router`: owns the dispatch loop shared by every strategy (ADR-003).
//!
//! Shrinks the candidate set per attempt (`already_tried`), reusing the old
//! `FallbackHandler::dispatch` error-class branching verbatim: validation and
//! auth errors return immediately (no failover); rate-limit errors trip the
//! upstream's cooldown and continue; other (transient) errors continue
//! without tripping cooldown. Same-upstream retries (e.g. Bedrock's
//! exponential backoff) stay inside the provider — the router only fails
//! over to a *different* upstream.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use http::HeaderMap;

use crate::providers::{Provider, ProviderError, ProviderResponse};
use crate::ratelimit::{AdmissionControl, Admit};

use super::health::{Availability, HealthRegistry};
use super::strategy::{RoutingStrategy, UpstreamRef};

/// Owns the dispatch loop for one route: a fixed candidate list, a selection
/// strategy, and the shared health registry. `providers` is indexed by the
/// same upstream index as `candidates` and `HealthRegistry`.
pub struct Router {
    candidates: Vec<UpstreamRef>,
    providers: Vec<Arc<dyn Provider>>,
    strategy: Arc<dyn RoutingStrategy>,
    health: Arc<HealthRegistry>,
    admission: Arc<dyn AdmissionControl>,
}

impl Router {
    pub fn new(
        candidates: Vec<UpstreamRef>,
        providers: Vec<Arc<dyn Provider>>,
        strategy: Arc<dyn RoutingStrategy>,
        health: Arc<HealthRegistry>,
        admission: Arc<dyn AdmissionControl>,
    ) -> Self {
        Self {
            candidates,
            providers,
            strategy,
            health,
            admission,
        }
    }

    /// Dispatches a request, re-selecting a different upstream on rate-limit
    /// or transient failure until candidates are exhausted. `est_tokens` is
    /// the caller's estimate of this request's token cost, used for the
    /// chosen upstream's TPM dimension (ADR-004); upstreams with no TPM
    /// limiter ignore it.
    pub async fn dispatch(
        &self,
        body: serde_json::Value,
        headers: HeaderMap,
        stream: bool,
        est_tokens: u32,
    ) -> Result<ProviderResponse, ProviderError> {
        let mut already_tried: HashSet<usize> = HashSet::new();
        let mut last_error: Option<ProviderError> = None;

        loop {
            let healthy: Vec<UpstreamRef> = self
                .candidates
                .iter()
                .filter(|u| !already_tried.contains(&u.index) && self.health.is_available(u.index))
                .cloned()
                .collect();

            let Some(chosen) = self.strategy.select(&healthy) else {
                break;
            };
            already_tried.insert(chosen.index);

            // ADR-004: post-selection admission check, before the provider
            // call — a Shed re-selects from the remaining pool via the same
            // loop that handles a 429, without tripping the 300s cooldown
            // (local admission control is a separate seam from ADR-003
            // health).
            match self.admission.admit(&chosen.name, est_tokens).await {
                Admit::Allowed | Admit::Delayed(_) => {}
                Admit::Shed => {
                    last_error = Some(ProviderError::RateLimited);
                    continue;
                }
            }

            let provider = &self.providers[chosen.index];
            match provider.send(body.clone(), headers.clone(), stream).await {
                Ok(response) => return Ok(response),
                Err(e) if e.is_validation() || e.is_auth() => return Err(e),
                Err(e) if e.is_rate_limited() => {
                    let override_duration = e.retry_after_secs().map(Duration::from_secs);
                    self.health.trip(chosen.index, override_duration);
                    last_error = Some(e);
                }
                Err(e) => {
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or(ProviderError::Upstream {
            status: 503,
            body: "no healthy upstreams available".to_string(),
        }))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::routing::strategy::{FallbackStrategy, WeightedStrategy};

    struct AlwaysOkProvider {
        name: &'static str,
        call_count: Arc<AtomicU32>,
    }

    #[async_trait::async_trait]
    impl Provider for AlwaysOkProvider {
        fn name(&self) -> &str {
            self.name
        }

        async fn send(
            &self,
            _body: serde_json::Value,
            _headers: HeaderMap,
            _stream: bool,
        ) -> Result<ProviderResponse, ProviderError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Ok(ProviderResponse::Full(serde_json::json!({"ok": true})))
        }
    }

    struct AlwaysErrProvider {
        name: &'static str,
        error: fn() -> ProviderError,
        call_count: Arc<AtomicU32>,
    }

    #[async_trait::async_trait]
    impl Provider for AlwaysErrProvider {
        fn name(&self) -> &str {
            self.name
        }

        async fn send(
            &self,
            _body: serde_json::Value,
            _headers: HeaderMap,
            _stream: bool,
        ) -> Result<ProviderResponse, ProviderError> {
            self.call_count.fetch_add(1, Ordering::SeqCst);
            Err((self.error)())
        }
    }

    fn upstream(index: usize, name: &str) -> UpstreamRef {
        UpstreamRef {
            index,
            name: name.to_string(),
            weight: 1.0,
        }
    }

    struct AlwaysAllow;

    #[async_trait::async_trait]
    impl AdmissionControl for AlwaysAllow {
        async fn admit(&self, _upstream: &str, _est_tokens: u32) -> Admit {
            Admit::Allowed
        }
    }

    fn fallback_router(providers: Vec<Arc<dyn Provider>>, health: Arc<HealthRegistry>) -> Router {
        Router::new(
            vec![upstream(0, "primary"), upstream(1, "fallback")],
            providers,
            Arc::new(FallbackStrategy),
            health,
            Arc::new(AlwaysAllow),
        )
    }

    #[tokio::test]
    async fn normal_routes_to_first_healthy() {
        let primary_calls = Arc::new(AtomicU32::new(0));
        let fallback_calls = Arc::new(AtomicU32::new(0));
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysOkProvider {
                name: "primary",
                call_count: primary_calls.clone(),
            }),
            Arc::new(AlwaysOkProvider {
                name: "fallback",
                call_count: fallback_calls.clone(),
            }),
        ];
        let router = fallback_router(providers, Arc::new(HealthRegistry::new(300)));

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(res.is_ok());
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn rate_limit_trips_cooldown_and_fails_over() {
        let primary_calls = Arc::new(AtomicU32::new(0));
        let fallback_calls = Arc::new(AtomicU32::new(0));
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysErrProvider {
                name: "primary",
                error: || ProviderError::RateLimited,
                call_count: primary_calls.clone(),
            }),
            Arc::new(AlwaysOkProvider {
                name: "fallback",
                call_count: fallback_calls.clone(),
            }),
        ];
        let health = Arc::new(HealthRegistry::new(300));
        let router = fallback_router(providers, health.clone());

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(res.is_ok());
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
        assert!(!health.is_available(0), "primary should be in cooldown");
    }

    #[tokio::test]
    async fn validation_error_is_not_retried() {
        let primary_calls = Arc::new(AtomicU32::new(0));
        let fallback_calls = Arc::new(AtomicU32::new(0));
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysErrProvider {
                name: "primary",
                error: || ProviderError::Validation("bad field".into(), 400),
                call_count: primary_calls.clone(),
            }),
            Arc::new(AlwaysOkProvider {
                name: "fallback",
                call_count: fallback_calls.clone(),
            }),
        ];
        let router = fallback_router(providers, Arc::new(HealthRegistry::new(300)));

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(res.is_err());
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn cooldown_skips_primary() {
        let primary_calls = Arc::new(AtomicU32::new(0));
        let fallback_calls = Arc::new(AtomicU32::new(0));
        let health = Arc::new(HealthRegistry::new(300));
        health.trip(0, None);
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysOkProvider {
                name: "primary",
                call_count: primary_calls.clone(),
            }),
            Arc::new(AlwaysOkProvider {
                name: "fallback",
                call_count: fallback_calls.clone(),
            }),
        ];
        let router = fallback_router(providers, health);

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(res.is_ok());
        assert_eq!(primary_calls.load(Ordering::SeqCst), 0);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cooldown_expiry_resumes_primary() {
        let primary_calls = Arc::new(AtomicU32::new(0));
        let fallback_calls = Arc::new(AtomicU32::new(0));
        let health = Arc::new(HealthRegistry::new(300));
        health.trip(0, Some(Duration::from_millis(1)));
        std::thread::sleep(Duration::from_millis(20));
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysOkProvider {
                name: "primary",
                call_count: primary_calls.clone(),
            }),
            Arc::new(AlwaysOkProvider {
                name: "fallback",
                call_count: fallback_calls.clone(),
            }),
        ];
        let router = fallback_router(providers, health);

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(res.is_ok());
        assert_eq!(primary_calls.load(Ordering::SeqCst), 1);
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn all_upstreams_unhealthy_returns_error() {
        let health = Arc::new(HealthRegistry::new(300));
        health.trip(0, None);
        health.trip(1, None);
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysOkProvider {
                name: "primary",
                call_count: Arc::new(AtomicU32::new(0)),
            }),
            Arc::new(AlwaysOkProvider {
                name: "fallback",
                call_count: Arc::new(AtomicU32::new(0)),
            }),
        ];
        let router = fallback_router(providers, health);

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(matches!(
            res,
            Err(ProviderError::Upstream { status: 503, .. })
        ));
    }

    struct ShedFor {
        upstream: &'static str,
    }

    #[async_trait::async_trait]
    impl AdmissionControl for ShedFor {
        async fn admit(&self, upstream: &str, _est_tokens: u32) -> Admit {
            if upstream == self.upstream {
                Admit::Shed
            } else {
                Admit::Allowed
            }
        }
    }

    #[tokio::test]
    async fn admission_shed_reselects_without_tripping_health() {
        let primary_calls = Arc::new(AtomicU32::new(0));
        let fallback_calls = Arc::new(AtomicU32::new(0));
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysOkProvider {
                name: "primary",
                call_count: primary_calls.clone(),
            }),
            Arc::new(AlwaysOkProvider {
                name: "fallback",
                call_count: fallback_calls.clone(),
            }),
        ];
        let health = Arc::new(HealthRegistry::new(300));
        let router = Router::new(
            vec![upstream(0, "primary"), upstream(1, "fallback")],
            providers,
            Arc::new(FallbackStrategy),
            health.clone(),
            Arc::new(ShedFor {
                upstream: "primary",
            }),
        );

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(res.is_ok());
        assert_eq!(
            primary_calls.load(Ordering::SeqCst),
            0,
            "shed upstream must never reach the provider"
        );
        assert_eq!(fallback_calls.load(Ordering::SeqCst), 1);
        assert!(
            health.is_available(0),
            "a local admission shed must not trip the ADR-003 health cooldown"
        );
    }

    #[tokio::test]
    async fn admission_shed_on_all_candidates_returns_error() {
        let health = Arc::new(HealthRegistry::new(300));
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysOkProvider {
                name: "primary",
                call_count: Arc::new(AtomicU32::new(0)),
            }),
            Arc::new(AlwaysOkProvider {
                name: "fallback",
                call_count: Arc::new(AtomicU32::new(0)),
            }),
        ];
        let router = Router::new(
            vec![upstream(0, "primary"), upstream(1, "fallback")],
            providers,
            Arc::new(FallbackStrategy),
            health,
            Arc::new(AlwaysShed),
        );

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(matches!(res, Err(ProviderError::RateLimited)));
    }

    struct AlwaysShed;

    #[async_trait::async_trait]
    impl AdmissionControl for AlwaysShed {
        async fn admit(&self, _upstream: &str, _est_tokens: u32) -> Admit {
            Admit::Shed
        }
    }

    #[tokio::test]
    async fn weighted_never_selects_a_cooled_down_upstream() {
        let a_calls = Arc::new(AtomicU32::new(0));
        let b_calls = Arc::new(AtomicU32::new(0));
        let health = Arc::new(HealthRegistry::new(300));
        health.trip(0, None); // "a" cooled down
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysOkProvider {
                name: "a",
                call_count: a_calls.clone(),
            }),
            Arc::new(AlwaysOkProvider {
                name: "b",
                call_count: b_calls.clone(),
            }),
        ];
        let router = Router::new(
            vec![
                UpstreamRef {
                    index: 0,
                    name: "a".to_string(),
                    weight: 0.7,
                },
                UpstreamRef {
                    index: 1,
                    name: "b".to_string(),
                    weight: 0.3,
                },
            ],
            providers,
            Arc::new(WeightedStrategy),
            health,
            Arc::new(AlwaysAllow),
        );

        for _ in 0..10 {
            let res = router
                .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
                .await;
            assert!(res.is_ok());
        }

        assert_eq!(
            a_calls.load(Ordering::SeqCst),
            0,
            "cooled upstream must never be selected"
        );
        assert_eq!(b_calls.load(Ordering::SeqCst), 10);
    }
}
