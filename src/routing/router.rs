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
use crate::metrics::MetricsCollector;
use crate::providers::anthropic::AnthropicProvider;
use crate::providers::bedrock::BedrockProvider;
use crate::providers::openai::OpenaiProvider;
use crate::providers::{Provider, ProviderError, ProviderResponse};
use crate::ratelimit::{AdmissionControl, Admit, RateLimiters};

use super::health::{Availability, HealthRegistry};
use super::session_overrides::{extract_session_id, SessionOverrideStore};
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
    metrics: Arc<MetricsCollector>,
    /// Session-scoped route pins, consulted before `strategy` on every
    /// dispatch (see `dispatch`'s doc comment). Defaults to an empty store
    /// via `Router::new`; `EntrypointState::build`/`api::post_route` carry
    /// the *same* `Arc` across a route hot-swap via `with_session_overrides`
    /// so a pin isn't lost just because the global route changed.
    session_overrides: Arc<SessionOverrideStore>,
}

/// Builds a live `Provider` for every configured upstream, keyed by its
/// config name — independent of any route, so `consolette list-models` can
/// enumerate every upstream's models, including ones no route currently
/// selects.
///
/// # Errors
///
/// Returns `Err` if any upstream fails to construct its `Provider`.
pub async fn build_providers(config: &Config) -> anyhow::Result<Vec<(String, Arc<dyn Provider>)>> {
    let resolver: Arc<dyn SecretResolver + Send + Sync> = Arc::new(SystemSecretResolver);
    let exec_cache = Arc::new(ExecCredentialCache::new());

    let mut providers = Vec::with_capacity(config.upstreams.len());
    for upstream in &config.upstreams {
        let provider: Arc<dyn Provider> = match &upstream.kind {
            UpstreamKind::Anthropic => Arc::new(AnthropicProvider::new(
                Arc::new(upstream.clone()),
                Arc::clone(&resolver),
                Arc::clone(&exec_cache),
                config.request_timeout,
            )?),
            UpstreamKind::Bedrock { .. } => {
                Arc::new(BedrockProvider::new(Arc::new(upstream.clone())).await)
            }
            UpstreamKind::Openai { base_url } => Arc::new(OpenaiProvider::new(
                Arc::new(upstream.clone()),
                base_url.clone(),
                Arc::clone(&resolver),
                Arc::clone(&exec_cache),
                config.request_timeout,
            )?),
        };
        providers.push((upstream.name.clone(), provider));
    }
    Ok(providers)
}

impl Router {
    #[must_use]
    pub fn new(
        candidates: Vec<UpstreamRef>,
        providers: Vec<Arc<dyn Provider>>,
        strategy: Arc<dyn RoutingStrategy>,
        health: Arc<HealthRegistry>,
        admission: Arc<dyn AdmissionControl>,
        metrics: Arc<MetricsCollector>,
    ) -> Self {
        Self {
            candidates,
            providers,
            strategy,
            health,
            admission,
            metrics,
            session_overrides: Arc::new(SessionOverrideStore::new()),
        }
    }

    /// Swaps in a shared session-override store, replacing the empty one
    /// `Router::new`/`from_config` starts with. Used to carry live pins
    /// across a route hot-swap (`api::post_route` rebuilds the `Router` via
    /// `from_config`, then calls this with the `EntrypointState`'s existing
    /// `Arc<SessionOverrideStore>` before storing the new router).
    #[must_use]
    pub fn with_session_overrides(mut self, session_overrides: Arc<SessionOverrideStore>) -> Self {
        self.session_overrides = session_overrides;
        self
    }

    /// Assembles a fully dispatch-ready `Router` from a loaded [`Config`]:
    /// builds a live [`Provider`] per configured upstream, resolves the
    /// first `Route`'s candidate list/strategy, and wires the health
    /// registry and admission control.
    ///
    /// # Errors
    ///
    /// Returns `Err` if any upstream fails to construct its `Provider`,
    /// if `config.routes` is empty, or if a route references an upstream
    /// name not present in `config.upstreams`.
    pub async fn from_config(
        config: &Config,
        metrics: Arc<MetricsCollector>,
    ) -> anyhow::Result<Router> {
        let providers: Vec<Arc<dyn Provider>> = build_providers(config)
            .await?
            .into_iter()
            .map(|(_, provider)| provider)
            .collect();
        let bedrock_indices: Vec<usize> = config
            .upstreams
            .iter()
            .enumerate()
            .filter(|(_, u)| matches!(u.kind, UpstreamKind::Bedrock { .. }))
            .map(|(idx, _)| idx)
            .collect();

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
                model: route_upstream.model.clone(),
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
            candidates, providers, strategy, health, admission, metrics,
        ))
    }

    /// This dispatch's candidate list: the route's normal `self.candidates`,
    /// unless `session_id` has a pin (`SessionOverrideStore`) whose upstream
    /// is still part of this route, in which case that one upstream (with
    /// the pin's model override, if any, else the upstream's own) replaces
    /// it entirely — a pin means "use this," not "prefer this," so a pinned
    /// upstream that's unhealthy still fails the request rather than
    /// silently falling over to a different one. Falls back to the normal
    /// candidates if the pinned upstream isn't in this route at all (e.g. a
    /// route change removed it).
    fn effective_candidates(&self, session_id: Option<&str>) -> Vec<UpstreamRef> {
        let Some(over) = session_id.and_then(|sid| self.session_overrides.get(sid)) else {
            return self.candidates.clone();
        };
        let Some(pinned) = self.candidates.iter().find(|c| c.name == over.upstream) else {
            tracing::warn!(
                session = session_id.unwrap_or(""),
                upstream = %over.upstream,
                "session-pinned upstream not in current route; falling back to normal routing"
            );
            return self.candidates.clone();
        };
        vec![UpstreamRef {
            index: pinned.index,
            name: pinned.name.clone(),
            weight: pinned.weight,
            model: over.model.clone().or_else(|| pinned.model.clone()),
        }]
    }

    /// Dispatches a request, re-selecting a different upstream on rate-limit
    /// or transient failure until candidates are exhausted. `est_tokens` is
    /// the caller's estimate of this request's token cost, used for the
    /// chosen upstream's TPM dimension (ADR-004); upstreams with no TPM
    /// limiter ignore it. A session-scoped pin
    /// (`SessionOverrideStore`/`effective_candidates`) takes precedence over
    /// this route's normal candidate list.
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
        let model = body
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();

        let session_id = extract_session_id(&body);
        let candidates = self.effective_candidates(session_id.as_deref());

        let request_id = uuid::Uuid::new_v4().to_string();
        self.metrics
            .push_request(crate::metrics::RequestDetail::from_body(
                request_id.clone(),
                stream,
                u64::from(est_tokens),
                &body,
                session_id.clone(),
            ));
        self.metrics
            .push_original_body(request_id.clone(), body.clone());

        loop {
            let healthy: Vec<UpstreamRef> = candidates
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
            let request_body = match &chosen.model {
                Some(model) => {
                    let mut b = body.clone();
                    b["model"] = serde_json::Value::String(model.clone());
                    b
                }
                None => body.clone(),
            };
            let attempt_started = std::time::Instant::now();
            match provider.send(request_body, headers.clone(), stream).await {
                Ok(response) => {
                    self.record_attempt(&chosen.name, attempt_started, Ok(()), &model);
                    #[allow(clippy::cast_precision_loss)]
                    let duration_ms = attempt_started.elapsed().as_secs_f64() * 1000.0;
                    // First-byte time isn't separately measured here (see
                    // `record_attempt`'s doc comment) — `provider.send`
                    // returning is the closest proxy we have for either a
                    // full response or a stream's headers.
                    self.metrics.update_request_timing(
                        &request_id,
                        &chosen.name,
                        duration_ms,
                        duration_ms,
                        0,
                        0,
                    );
                    return Ok(response);
                }
                Err(e) if e.is_validation() || e.is_auth() => {
                    self.record_attempt(&chosen.name, attempt_started, Err(&e), &model);
                    return Err(e);
                }
                Err(e) if e.is_rate_limited() => {
                    self.record_attempt(&chosen.name, attempt_started, Err(&e), &model);
                    let override_duration = e.retry_after_secs().map(Duration::from_secs);
                    self.health.trip(chosen.index, override_duration);
                    last_error = Some(e);
                }
                Err(e) => {
                    self.record_attempt(&chosen.name, attempt_started, Err(&e), &model);
                    last_error = Some(e);
                }
            }
        }

        Err(last_error.unwrap_or(ProviderError::Exhausted))
    }

    /// Names of the upstreams this router currently dispatches to, in
    /// candidate order — used by the web control panel to confirm a route
    /// change actually took effect on the live router, not just on disk.
    #[must_use]
    pub fn candidate_names(&self) -> Vec<String> {
        self.candidates.iter().map(|c| c.name.clone()).collect()
    }

    /// Records one dispatch attempt's timing/outcome for `/metrics`
    /// (Task 3.4.5) — per-upstream request/success/error counts plus, on
    /// failure, the error-type breakdown and the deduplicated error tracker
    /// feeding `/errors/summary`. For a streaming response this measures
    /// time-to-headers only (`provider.send` returns once the stream is
    /// ready, not once it's fully consumed) — full stream duration would
    /// need a metrics-side tee analogous to `CostTrackingStream`.
    fn record_attempt(
        &self,
        upstream: &str,
        started: std::time::Instant,
        outcome: Result<(), &ProviderError>,
        model: &str,
    ) {
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match outcome {
            Ok(()) => {
                self.metrics
                    .counters
                    .record_request(upstream, true, duration_ms, 0);
            }
            Err(e) => {
                self.metrics
                    .counters
                    .record_request(upstream, false, duration_ms, 0);
                self.metrics.counters.record_error_kind(e);
                let _ = self
                    .metrics
                    .error_tracker
                    .push(&e.to_string(), upstream, model);
            }
        }
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

        async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
            Ok(Vec::new())
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

        async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    fn upstream(index: usize, name: &str) -> UpstreamRef {
        UpstreamRef {
            index,
            name: name.to_string(),
            weight: 1.0,
            model: None,
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
            MetricsCollector::new(),
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
            MetricsCollector::new(),
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
            MetricsCollector::new(),
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
                    model: None,
                },
                UpstreamRef {
                    index: 1,
                    name: "b".to_string(),
                    weight: 0.3,
                    model: None,
                },
            ],
            providers,
            Arc::new(WeightedStrategy),
            health,
            Arc::new(AlwaysAllow),
            MetricsCollector::new(),
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
        let router = Router::from_config(&config, MetricsCollector::new())
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
    #[allow(clippy::expect_used)]
    async fn from_config_openai_kind_builds_successfully() {
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
                    model: None,
                }],
            }],
            ..Config::default()
        };

        #[allow(clippy::expect_used)]
        let router = Router::from_config(&config, MetricsCollector::new())
            .await
            .expect("Openai-kind upstream must build a Provider");
        assert_eq!(router.candidates[0].name, "my-openai-upstream");
        assert_eq!(router.providers[0].name(), "openai");
    }

    #[tokio::test]
    async fn from_config_empty_routes_bails() {
        let config = Config {
            routes: vec![],
            ..Config::default()
        };
        let Err(err) = Router::from_config(&config, MetricsCollector::new()).await else {
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
                model: None,
            }],
        };
        config.routes = vec![route_a, route_b];

        #[allow(clippy::expect_used)]
        let router = Router::from_config(&config, MetricsCollector::new())
            .await
            .expect("multi-route config must still build");
        assert_eq!(router.candidates.len(), 2);
        assert_eq!(router.candidates[0].name, "anthropic");
        assert_eq!(router.candidates[1].name, "bedrock");
    }

    struct CapturingProvider {
        name: &'static str,
        received_body: Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    }

    #[async_trait::async_trait]
    impl Provider for CapturingProvider {
        fn name(&self) -> &str {
            self.name
        }

        #[allow(clippy::unwrap_used)]
        async fn send(
            &self,
            body: serde_json::Value,
            _headers: HeaderMap,
            _stream: bool,
        ) -> Result<ProviderResponse, ProviderError> {
            *self.received_body.lock().unwrap() = Some(body);
            Ok(ProviderResponse::Full(serde_json::json!({"ok": true})))
        }

        async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn dispatch_overrides_model_field_when_upstream_pins_one() {
        let received_body = Arc::new(std::sync::Mutex::new(None));
        let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(CapturingProvider {
            name: "pinned",
            received_body: received_body.clone(),
        })];
        let metrics = MetricsCollector::new();
        let router = Router::new(
            vec![UpstreamRef {
                index: 0,
                name: "pinned".to_string(),
                weight: 1.0,
                model: Some("gpt-5.1-codex-max".to_string()),
            }],
            providers,
            Arc::new(FallbackStrategy),
            Arc::new(HealthRegistry::new(300)),
            Arc::new(AlwaysAllow),
            metrics.clone(),
        );

        let res = router
            .dispatch(
                serde_json::json!({"model": "claude-sonnet-4-5"}),
                HeaderMap::new(),
                false,
                0,
            )
            .await;

        assert!(res.is_ok());
        let body = received_body
            .lock()
            .unwrap()
            .clone()
            .expect("provider must have been called");
        assert_eq!(body["model"], serde_json::json!("gpt-5.1-codex-max"));

        let pinned = metrics.counters.upstreams.get("pinned").unwrap();
        assert_eq!(pinned.requests.load(Ordering::Relaxed), 1);
        assert_eq!(pinned.success.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn dispatch_leaves_model_field_untouched_when_upstream_has_no_override() {
        let received_body = Arc::new(std::sync::Mutex::new(None));
        let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(CapturingProvider {
            name: "unpinned",
            received_body: received_body.clone(),
        })];
        let router = Router::new(
            vec![upstream(0, "unpinned")],
            providers,
            Arc::new(FallbackStrategy),
            Arc::new(HealthRegistry::new(300)),
            Arc::new(AlwaysAllow),
            MetricsCollector::new(),
        );

        let res = router
            .dispatch(
                serde_json::json!({"model": "claude-sonnet-4-5"}),
                HeaderMap::new(),
                false,
                0,
            )
            .await;

        assert!(res.is_ok());
        let body = received_body
            .lock()
            .unwrap()
            .clone()
            .expect("provider must have been called");
        assert_eq!(body["model"], serde_json::json!("claude-sonnet-4-5"));
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_attributes_per_upstream_metrics_across_a_failover() {
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysErrProvider {
                name: "primary",
                error: || ProviderError::RateLimited,
                call_count: Arc::new(AtomicU32::new(0)),
            }),
            Arc::new(AlwaysOkProvider {
                name: "fallback",
                call_count: Arc::new(AtomicU32::new(0)),
            }),
        ];
        let metrics = MetricsCollector::new();
        let router = Router::new(
            vec![upstream(0, "primary"), upstream(1, "fallback")],
            providers,
            Arc::new(FallbackStrategy),
            Arc::new(HealthRegistry::new(300)),
            Arc::new(AlwaysAllow),
            metrics.clone(),
        );

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;
        assert!(res.is_ok());

        let primary = metrics.counters.upstreams.get("primary").unwrap();
        assert_eq!(primary.requests.load(Ordering::Relaxed), 1);
        assert_eq!(primary.errors.load(Ordering::Relaxed), 1);
        drop(primary);

        let fallback = metrics.counters.upstreams.get("fallback").unwrap();
        assert_eq!(fallback.requests.load(Ordering::Relaxed), 1);
        assert_eq!(fallback.success.load(Ordering::Relaxed), 1);
        drop(fallback);

        assert_eq!(
            metrics.counters.err_rate_limit.load(Ordering::Relaxed),
            1,
            "the primary's RateLimited error must be classified"
        );
        assert_eq!(
            metrics.error_tracker.get_summary(10).len(),
            1,
            "the primary's failure must be pushed into the error tracker"
        );
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_populates_the_recent_requests_ring_buffer() {
        let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(AlwaysOkProvider {
            name: "primary",
            call_count: Arc::new(AtomicU32::new(0)),
        })];
        let metrics = MetricsCollector::new();
        let router = Router::new(
            vec![upstream(0, "primary")],
            providers,
            Arc::new(FallbackStrategy),
            Arc::new(HealthRegistry::new(300)),
            Arc::new(AlwaysAllow),
            metrics.clone(),
        );

        let body = serde_json::json!({
            "model": "claude-sonnet-4-5",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [{"type": "text", "text": "hello"}]},
            ],
        });
        let res = router.dispatch(body, HeaderMap::new(), false, 0).await;
        assert!(res.is_ok());

        let recent = metrics.get_recent_requests(10);
        assert_eq!(recent.len(), 1, "dispatch must push exactly one entry");
        let detail = &recent[0];
        assert_eq!(detail.model, "claude-sonnet-4-5");
        assert_eq!(detail.provider, "primary", "must be filled in on success");
        assert_eq!(detail.message_count, 2);
        let msg_types: serde_json::Value = serde_json::from_str(&detail.msg_types).unwrap();
        assert_eq!(msg_types["text"], 2, "one plain-string + one text block");
    }
}
