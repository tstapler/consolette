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

use crate::auth::exec::ExecCredentialCache;
use crate::auth::{SecretResolver, SystemSecretResolver};
use crate::config::schema::{Config, Strategy, UpstreamKind};
use crate::providers::anthropic::AnthropicProvider;
use crate::providers::bedrock::BedrockProvider;
use crate::providers::{Provider, ProviderError, ProviderResponse};
use crate::ratelimit::{AdmissionControl, Admit, RateLimiters};

use super::health::{Availability, HealthRegistry};
use super::strategy::{FallbackStrategy, RoutingStrategy, UpstreamRef, WeightedStrategy};

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
    #[must_use]
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

    /// Assembles a fully dispatch-ready `Router` from a loaded [`Config`]:
    /// builds a live [`Provider`] per configured upstream, resolves the
    /// first `Route`'s candidate list/strategy, and wires the health
    /// registry and admission control.
    ///
    /// # Errors
    ///
    /// Returns `Err` if any upstream fails to construct its `Provider`
    /// (e.g. `UpstreamKind::Openai`, which has no `Provider` implementation
    /// yet), if `config.routes` is empty, or if a route references an
    /// upstream name not present in `config.upstreams`.
    pub async fn from_config(config: &Config) -> anyhow::Result<Router> {
        let resolver: Arc<dyn SecretResolver + Send + Sync> = Arc::new(SystemSecretResolver);
        let exec_cache = Arc::new(ExecCredentialCache::new());

        let mut providers: Vec<Arc<dyn Provider>> = Vec::with_capacity(config.upstreams.len());
        let mut bedrock_indices: Vec<usize> = Vec::new();
        for (idx, upstream) in config.upstreams.iter().enumerate() {
            match &upstream.kind {
                UpstreamKind::Anthropic => {
                    let provider = AnthropicProvider::new(
                        Arc::new(upstream.clone()),
                        Arc::clone(&resolver),
                        Arc::clone(&exec_cache),
                        config.request_timeout,
                    )?;
                    providers.push(Arc::new(provider) as Arc<dyn Provider>);
                }
                UpstreamKind::Bedrock { .. } => {
                    let provider = BedrockProvider::new(Arc::new(upstream.clone())).await;
                    bedrock_indices.push(idx);
                    providers.push(Arc::new(provider) as Arc<dyn Provider>);
                }
                UpstreamKind::Openai { .. } => {
                    anyhow::bail!(
                        "upstream \"{}\": UpstreamKind::Openai has no Provider implementation yet",
                        upstream.name
                    );
                }
            }
        }

        let health = Arc::new(HealthRegistry::new(config.cooldown_seconds));
        for idx in bedrock_indices {
            health.set_can_cooldown(idx, false);
        }

        let route = config
            .routes
            .first()
            .ok_or_else(|| anyhow::anyhow!("no routes configured"))?;
        if config.routes.len() > 1 {
            tracing::warn!(
                ignored = ?config.routes[1..].iter().map(|r| &r.name).collect::<Vec<_>>(),
                "multiple routes configured; using the first"
            );
        }

        let mut candidates: Vec<UpstreamRef> = Vec::with_capacity(route.upstreams.len());
        for route_upstream in &route.upstreams {
            let index = config
                .upstreams
                .iter()
                .position(|u| u.name == route_upstream.name)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "route \"{}\" references unknown upstream \"{}\"",
                        route.name,
                        route_upstream.name
                    )
                })?;
            candidates.push(UpstreamRef {
                index,
                name: route_upstream.name.clone(),
                weight: route_upstream.weight.unwrap_or(1.0),
            });
        }

        let strategy: Arc<dyn RoutingStrategy> = match route.strategy {
            Strategy::Fallback => Arc::new(FallbackStrategy) as Arc<dyn RoutingStrategy>,
            Strategy::Weighted => Arc::new(WeightedStrategy) as Arc<dyn RoutingStrategy>,
        };

        let admission = Arc::new(RateLimiters::new(&config.ratelimit)) as Arc<dyn AdmissionControl>;

        tracing::info!(
            route = %route.name,
            strategy = ?route.strategy,
            candidates = candidates.len(),
            "router assembled from config"
        );

        Ok(Router::new(
            candidates, providers, strategy, health, admission,
        ))
    }

    /// Dispatches a request, re-selecting a different upstream on rate-limit
    /// or transient failure until candidates are exhausted. `est_tokens` is
    /// the caller's estimate of this request's token cost, used for the
    /// chosen upstream's TPM dimension (ADR-004); upstreams with no TPM
    /// limiter ignore it.
    ///
    /// # Errors
    ///
    /// Returns the last [`ProviderError`] encountered once every candidate
    /// upstream has been tried (or none were available/admitted).
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

        Err(last_error.unwrap_or(ProviderError::Exhausted))
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

        assert!(matches!(res, Err(ProviderError::Exhausted)));
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

    #[tokio::test]
    async fn from_config_default_config_produces_two_candidates_in_order() {
        // `AnthropicProvider::new` doesn't resolve the bearer-token secret
        // at construction time (only at send-time), so no env var needs to
        // be set for this to succeed.
        let config = Config::default();
        #[allow(clippy::expect_used)]
        let router = Router::from_config(&config)
            .await
            .expect("Config::default() must build a Router");
        assert_eq!(router.candidates.len(), 2);
        assert_eq!(router.candidates[0].index, 0);
        assert_eq!(router.candidates[0].name, "anthropic");
        assert_eq!(router.candidates[1].index, 1);
        assert_eq!(router.candidates[1].name, "bedrock");
        assert_eq!(router.providers[0].name(), "anthropic");
        assert_eq!(router.providers[1].name(), "bedrock");
    }

    #[tokio::test]
    async fn from_config_openai_kind_bails_with_expected_message() {
        use crate::config::schema::{Route, RouteUpstreamRef, Upstream, UpstreamKind};

        let config = Config {
            upstreams: vec![Upstream {
                name: "my-openai-upstream".to_string(),
                kind: UpstreamKind::Openai {
                    base_url: "https://example.invalid".to_string(),
                },
                auth: None,
            }],
            routes: vec![Route {
                name: "default".to_string(),
                strategy: Strategy::Fallback,
                upstreams: vec![RouteUpstreamRef {
                    name: "my-openai-upstream".to_string(),
                    weight: None,
                }],
            }],
            ..Config::default()
        };

        let Err(err) = Router::from_config(&config).await else {
            panic!("Openai-kind upstream must fail to build a Provider")
        };
        assert_eq!(
            err.to_string(),
            "upstream \"my-openai-upstream\": UpstreamKind::Openai has no Provider implementation yet"
        );
    }

    #[tokio::test]
    async fn from_config_empty_routes_bails() {
        let config = Config {
            routes: vec![],
            ..Config::default()
        };
        let Err(err) = Router::from_config(&config).await else {
            panic!("empty routes must fail")
        };
        assert!(err.to_string().contains("no routes configured"));
    }

    #[tokio::test]
    async fn from_config_multi_route_uses_first() {
        use crate::config::schema::{Route, RouteUpstreamRef};

        let mut config = Config::default();
        let route_a = config.routes[0].clone();
        let route_b = Route {
            name: "secondary".to_string(),
            strategy: Strategy::Fallback,
            upstreams: vec![RouteUpstreamRef {
                name: "bedrock".to_string(),
                weight: None,
            }],
        };
        config.routes = vec![route_a, route_b];

        #[allow(clippy::expect_used)]
        let router = Router::from_config(&config)
            .await
            .expect("multi-route config must still build");
        assert_eq!(router.candidates.len(), 2);
        assert_eq!(router.candidates[0].name, "anthropic");
        assert_eq!(router.candidates[1].name, "bedrock");
    }
}
