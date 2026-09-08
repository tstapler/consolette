//! `Router`: owns the dispatch loop shared by every strategy (ADR-003).
//!
//! Shrinks the candidate set per attempt (`already_tried`), reusing the old
//! `FallbackHandler::dispatch` error-class branching verbatim: validation and
//! auth errors return immediately (no failover); rate-limit errors trip the
//! upstream's cooldown and continue; other (transient) errors continue
//! without tripping cooldown. Same-upstream retries (e.g. Bedrock's
//! exponential backoff) stay inside the provider — the router only fails
//! over to a *different* upstream.

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use http::HeaderMap;

use crate::auth::exec::ExecCredentialCache;
use crate::auth::{SecretResolver, SystemSecretResolver};
use crate::config::schema::{Config, Route, Strategy, UpstreamKind};
use crate::metrics::MetricsCollector;
use crate::providers::anthropic::AnthropicProvider;
use crate::providers::bedrock::BedrockProvider;
use crate::providers::gemini::GeminiProvider;
use crate::providers::openai::OpenaiProvider;
use crate::providers::openrouter::OpenrouterProvider;
use crate::providers::{Provider, ProviderError, ProviderResponse};
use crate::ratelimit::{AdmissionControl, Admit, RateLimiters};

use super::health::{Availability, HealthRegistry};
use super::openrouter_scoring::OpenrouterScoringStrategy;
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
/// Additionally returns a `HashMap<usize, Arc<OpenrouterProvider>>` (upstream
/// index -> concrete handle) for every `openrouter`-kind upstream, alongside
/// the usual `Vec<(String, Arc<dyn Provider>)>` — a type-driven-design
/// choice (plan.md's Pattern Decisions) over `dyn Any`-downcasting, so
/// `OpenrouterScoringStrategy` (Epic 4.3) can share the same
/// `Arc<ModelListCache>` the provider already populated, without widening
/// the `Provider` trait itself.
///
/// # Errors
///
/// Returns `Err` if any upstream fails to construct its `Provider`.
pub async fn build_providers(
    config: &Config,
) -> anyhow::Result<(
    Vec<(String, Arc<dyn Provider>)>,
    HashMap<usize, Arc<OpenrouterProvider>>,
)> {
    let resolver: Arc<dyn SecretResolver + Send + Sync> = Arc::new(SystemSecretResolver);
    let exec_cache = Arc::new(ExecCredentialCache::new());

    let mut providers = Vec::with_capacity(config.upstreams.len());
    let mut openrouter_providers = HashMap::new();
    for (index, upstream) in config.upstreams.iter().enumerate() {
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
            UpstreamKind::Gemini { .. } => Arc::new(GeminiProvider::new(
                Arc::new(upstream.clone()),
                Arc::clone(&resolver),
                Arc::clone(&exec_cache),
                config.request_timeout,
            )?),
            UpstreamKind::Openrouter {} => {
                // `OpenrouterProvider::new` already returns `Arc<OpenrouterProvider>`
                // (Task 1.2.1b), not a bare `Self`, so no extra `Arc::new(..)`
                // wrap is needed here. It also eagerly refreshes its model
                // cache as an invariant of construction (Story 2.1.2's
                // Blocker-1 fix) — this happens for every `openrouter`-kind
                // upstream regardless of which `Strategy` any route pairs it
                // with.
                let provider = OpenrouterProvider::new(
                    Arc::new(upstream.clone()),
                    Arc::clone(&resolver),
                    Arc::clone(&exec_cache),
                    config.request_timeout,
                )
                .await?;
                openrouter_providers.insert(index, Arc::clone(&provider));
                provider as Arc<dyn Provider>
            }
        };
        providers.push((upstream.name.clone(), provider));
    }
    Ok((providers, openrouter_providers))
}

/// Story 4.3.1d (architecture-review Blocker 1, defense-in-depth half of
/// plan.md's money-safety mechanism 1): an `openrouter`-kind upstream may
/// only be dispatched to under `Strategy::OpenrouterScored`. Population of
/// its `ModelListCache` is unconditional as of Story 2.1.2 (every
/// `openrouter`-kind upstream is always safe to *send* to, regardless of
/// strategy), but a `Fallback`/`Weighted` route referencing one is still a
/// config mistake worth rejecting loudly at load time rather than letting
/// it "work" untested. Checked over every route in `config.routes`,
/// independent of which one `Router::from_config` actually builds a
/// `Router` from.
///
/// Also enforces the *reverse* direction (Task 4.3.1g, adversarial-review
/// Concern): a route using `strategy = "openrouter_scored"` must not
/// itself list any non-`openrouter`-kind (e.g. paid) upstream. Without
/// this, a single misconfigured route mixing an `openrouter`-kind upstream
/// with a paid one could let `dispatch()` silently fall through to the
/// paid upstream once the free pool is exhausted — precisely the "silently
/// spend money" outcome this feature exists to prevent (a paid fallback
/// must be a separate route, per requirements.md's Scope).
///
/// # Errors
///
/// Returns `Err` naming the route and upstream if any route pairs an
/// `openrouter`-kind upstream with a non-`OpenrouterScored` strategy, or if
/// an `openrouter_scored` route lists any non-`openrouter`-kind upstream.
fn validate_openrouter_strategy_pairing(config: &Config) -> anyhow::Result<()> {
    for candidate_route in &config.routes {
        for route_upstream in &candidate_route.upstreams {
            let Some(upstream) = config
                .upstreams
                .iter()
                .find(|u| u.name == route_upstream.name)
            else {
                // An unknown-upstream reference is reported separately, by
                // `Router::from_config`'s own candidate-building loop, when
                // (if) this is the route actually being built.
                continue;
            };
            let is_openrouter = matches!(upstream.kind, UpstreamKind::Openrouter {});
            if is_openrouter && candidate_route.strategy != Strategy::OpenrouterScored {
                return Err(anyhow::anyhow!(
                    "route \"{}\" references openrouter-kind upstream \"{}\" under strategy {:?}; openrouter-kind upstreams may only be used with strategy = \"openrouter_scored\"",
                    candidate_route.name,
                    route_upstream.name,
                    candidate_route.strategy
                ));
            }
            if !is_openrouter && candidate_route.strategy == Strategy::OpenrouterScored {
                return Err(anyhow::anyhow!(
                    "route \"{}\" uses strategy = \"openrouter_scored\" but upstream \"{}\" is not openrouter-kind; mixing a scored free-model pool with a paid upstream in one route risks silent paid fallback on exhaustion — configure the paid upstream as a separate route instead",
                    candidate_route.name,
                    route_upstream.name
                ));
            }
        }
    }
    Ok(())
}

/// Task 4.3.1a: constructs the `OpenrouterScoringStrategy` for a
/// `Strategy::OpenrouterScored` route. `expand_candidates` fans the one
/// static `openrouter`-kind `UpstreamRef` out into one per currently-cached
/// free model, so exactly one such upstream is expected among `candidates`.
///
/// # Errors
///
/// Returns `Err` naming `route` if it references no `openrouter`-kind
/// upstream, or (defensively) if the matched upstream has no corresponding
/// entry in `openrouter_providers`.
fn build_openrouter_scored_strategy(
    route: &Route,
    candidates: &[UpstreamRef],
    config: &Config,
    openrouter_providers: &HashMap<usize, Arc<OpenrouterProvider>>,
) -> anyhow::Result<Arc<dyn RoutingStrategy>> {
    let openrouter_index = candidates
        .iter()
        .find(|c| matches!(config.upstreams[c.index].kind, UpstreamKind::Openrouter {}))
        .map(|c| c.index)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "route \"{}\" uses strategy = \"openrouter_scored\" but references no openrouter-kind upstream",
                route.name
            )
        })?;
    let provider = openrouter_providers.get(&openrouter_index).ok_or_else(|| {
        anyhow::anyhow!(
            "route \"{}\": openrouter-kind upstream at index {} has no corresponding OpenrouterProvider",
            route.name,
            openrouter_index
        )
    })?;
    Ok(Arc::new(OpenrouterScoringStrategy::new(
        provider.model_cache(),
        openrouter_index,
    )) as Arc<dyn RoutingStrategy>)
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
    /// if `config.routes` is empty, if a route references an upstream name
    /// not present in `config.upstreams`, if a route pairs an
    /// `openrouter`-kind upstream with a `Strategy` other than
    /// `OpenrouterScored` (Story 4.3.1's symmetric validation — checked
    /// across *every* configured route, not just the one actually built),
    /// or if a `Strategy::OpenrouterScored` route references no
    /// `openrouter`-kind upstream.
    pub async fn from_config(
        config: &Config,
        metrics: Arc<MetricsCollector>,
    ) -> anyhow::Result<Router> {
        // Validate before any live network call/background-task spawn:
        // `build_providers` eagerly fetches `/models` and spawns a
        // background refresh task for every `openrouter`-kind upstream, so a
        // misconfigured route should be rejected before either happens, not
        // after.
        validate_openrouter_strategy_pairing(config)?;

        let (providers, openrouter_providers) = build_providers(config).await?;
        let providers: Vec<Arc<dyn Provider>> = providers
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
        // Gemini is a real network upstream — do NOT add its indices here
        // (see project_plans/gemini-provider/implementation/plan.md Story 1.1.2).
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
            Strategy::OpenrouterScored => {
                build_openrouter_scored_strategy(route, &candidates, config, &openrouter_providers)?
            }
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
        let mut already_tried: HashSet<(usize, Option<String>)> = HashSet::new();
        let mut last_error: Option<ProviderError> = None;
        let model = body
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown")
            .to_string();

        let session_id = extract_session_id(&body);
        let candidates = self.effective_candidates(session_id.as_deref());
        let candidates = self.strategy.expand_candidates(candidates);

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
                .filter(|u| {
                    !already_tried.contains(&(u.index, u.model.clone()))
                        && self.health.is_available(u.index)
                })
                .cloned()
                .collect();

            let Some(chosen) = self.strategy.select(&healthy) else {
                break;
            };
            already_tried.insert((chosen.index, chosen.model.clone()));
            self.record_selection(&request_id, &chosen);

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
            let outcome = provider.send(request_body, headers.clone(), stream).await;

            match outcome {
                Ok(response) => {
                    self.record_dispatch_outcome(&chosen, attempt_started, Ok(()), &model);
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
                    self.record_dispatch_outcome(&chosen, attempt_started, Err(&e), &model);
                    return Err(e);
                }
                Err(e) if e.is_rate_limited() => {
                    self.record_dispatch_outcome(&chosen, attempt_started, Err(&e), &model);
                    let override_duration = e.retry_after_secs().map(Duration::from_secs);
                    self.health.trip(chosen.index, override_duration);
                    last_error = Some(e);
                }
                Err(e) if e.is_response_shape_mismatch() => {
                    // ADR-002: a 2xx body that doesn't match the documented
                    // shape won't self-heal on retry the way a rate limit
                    // does — trip cooldown immediately (first occurrence),
                    // using a longer override than the default so a
                    // permanently-broken Gemini endpoint isn't retried on
                    // every request forever.
                    self.record_dispatch_outcome(&chosen, attempt_started, Err(&e), &model);
                    self.health.trip(
                        chosen.index,
                        Some(Duration::from_secs(
                            crate::providers::gemini::DRIFT_COOLDOWN_SECS,
                        )),
                    );
                    last_error = Some(e);
                }
                Err(e) => {
                    self.record_dispatch_outcome(&chosen, attempt_started, Err(&e), &model);
                    last_error = Some(e);
                }
            }
        }

        if last_error.is_none() {
            self.attribute_exhausted_kind(&candidates);
        }

        Err(last_error.unwrap_or(ProviderError::Exhausted))
    }

    /// Story 5.1.3 / Pre-mortem P2 #2: records the just-chosen candidate's
    /// model (last attempt wins across retries, mirroring how
    /// `update_request_timing` already overwrites `provider` on each retry)
    /// plus whether this selection was an epsilon-greedy exploration pick
    /// rather than the greedy argmax choice — both `None` for every
    /// non-model-pinned route and, for the latter, for any strategy that
    /// doesn't track the explore/greedy distinction.
    fn record_selection(&self, request_id: &str, chosen: &UpstreamRef) {
        self.metrics
            .set_selected_model(request_id, chosen.model.clone());
        let was_exploration = chosen
            .model
            .as_deref()
            .and_then(|m| self.strategy.last_selection_was_exploration(m));
        self.metrics
            .set_selected_model_was_exploration(request_id, was_exploration);
    }

    /// REQ-6 (Task 4.3.1f, design/ux.md §6): a full-pool exhaustion reached
    /// without ever attempting a candidate (every one of `candidates` was
    /// already unavailable/cooling-down before this dispatch even started)
    /// still needs the same `last_error_kind` attribution a per-attempt
    /// failure gets via `record_attempt` — otherwise a spike in exhaustion
    /// is invisible in `recent_errors`/upstream counters and only shows up
    /// in the client-facing 503/529 body. Every candidate reaching this
    /// branch shares the pool that just got exhausted, so every distinct
    /// upstream name among them is attributed once.
    fn attribute_exhausted_kind(&self, candidates: &[UpstreamRef]) {
        let mut attributed: HashSet<&str> = HashSet::new();
        for c in candidates {
            if attributed.insert(c.name.as_str()) {
                self.metrics
                    .counters
                    .set_last_error_kind(&c.name, Some(ProviderError::Exhausted.kind_label()));
            }
        }
    }

    /// Names of the upstreams this router currently dispatches to, in
    /// candidate order — used by the web control panel to confirm a route
    /// change actually took effect on the live router, not just on disk.
    #[must_use]
    pub fn candidate_names(&self) -> Vec<String> {
        self.candidates.iter().map(|c| c.name.clone()).collect()
    }

    /// Real per-candidate cooldown state from `HealthRegistry`, for
    /// `/metrics`' `cooldowns` field (Story 1.5.1) — replaces the previous
    /// hardcoded `anthropic`/`bedrock`-only placeholder in
    /// `MetricsCollector::to_metrics_json`, which silently produced fake
    /// data for every other upstream (including Gemini).
    #[must_use]
    pub fn cooldown_snapshot(&self) -> serde_json::Value {
        let mut result = serde_json::Map::with_capacity(self.candidates.len());
        for candidate in &self.candidates {
            let remaining = self.health.remaining_secs(candidate.index);
            result.insert(
                candidate.name.clone(),
                serde_json::json!({
                    "cooling_down": remaining > 0,
                    "remaining_seconds": remaining,
                }),
            );
        }
        serde_json::Value::Object(result)
    }

    /// The active strategy's `/metrics`-facing observability blob (Story
    /// 5.1.2, Task 5.1.2a) — `Some` only when `self.strategy` overrides
    /// `observability_snapshot()` (currently just `OpenrouterScoringStrategy`),
    /// `None` for `FallbackStrategy`/`WeightedStrategy`'s default. The HTTP
    /// handler (`entrypoint::observability::get_metrics`) merges this in
    /// under the `openrouter_scoring` key only when it's `Some`, omitting
    /// the key entirely otherwise.
    #[must_use]
    pub fn openrouter_scoring_snapshot(&self) -> Option<serde_json::Value> {
        self.strategy.observability_snapshot()
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
                // Story 1.4.4: a successful attempt clears the upstream's
                // last error classification, so the dashboard self-heals
                // instead of a single past failure permanently pinning its
                // status class.
                self.metrics.counters.set_last_error_kind(upstream, None);
            }
            Err(e) => {
                self.metrics
                    .counters
                    .record_request(upstream, false, duration_ms, 0);
                self.metrics.counters.record_error_kind(e);
                self.metrics
                    .counters
                    .set_last_error_kind(upstream, Some(e.kind_label()));
                let _ = self
                    .metrics
                    .error_tracker
                    .push(&e.to_string(), upstream, model);
            }
        }
    }

    /// Records one dispatch attempt's outcome into both the `/metrics`
    /// counters (`record_attempt`) and the selection strategy's own rolling
    /// stats (`RoutingStrategy::record_outcome`) — the two are always called
    /// together (Story 3.1.2, Task 3.1.2d), so this bundles them to avoid
    /// repeating the same outcome/duration plumbing at all 5 `dispatch`
    /// match arms.
    fn record_dispatch_outcome(
        &self,
        chosen: &UpstreamRef,
        attempt_started: std::time::Instant,
        outcome: Result<(), &ProviderError>,
        model: &str,
    ) {
        self.record_attempt(&chosen.name, attempt_started, outcome, model);
        let duration_ms = u64::try_from(attempt_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let (success, error_kind) = match outcome {
            Ok(()) => (true, None),
            Err(e) => (false, Some(e.kind_label())),
        };
        self.strategy
            .record_outcome(chosen, duration_ms, success, error_kind);
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

    /// Builds a config-schema `Upstream` with an inline bearer-token secret
    /// (Story 4.3.1's tests below): most of Epic 4.3's `from_config` unit
    /// tests need real construction-time auth (`OpenrouterProvider::new`
    /// resolves headers eagerly), not `auth: None`.
    fn bearer_upstream(
        name: &str,
        kind: crate::config::schema::UpstreamKind,
        token: &str,
    ) -> crate::config::schema::Upstream {
        crate::config::schema::Upstream {
            name: name.to_string(),
            kind,
            auth: Some(crate::config::schema::AuthMethod::Bearer {
                token: crate::config::schema::SecretRef::Inline {
                    value: token.to_string(),
                },
            }),
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

    // REQ-9 (Story 1.4.3, ADR-002) — focus area.
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_should_trip_cooldown_for_drift_cooldown_secs_when_candidate_returns_response_shape_mismatch(
    ) {
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysErrProvider {
                name: "gemini",
                error: || ProviderError::ResponseShapeMismatch("bad shape".to_string()),
                call_count: Arc::new(AtomicU32::new(0)),
            }),
            Arc::new(AlwaysOkProvider {
                name: "anthropic",
                call_count: Arc::new(AtomicU32::new(0)),
            }),
        ];
        let metrics = MetricsCollector::new();
        let health = Arc::new(HealthRegistry::new(300));
        let router = Router::new(
            vec![upstream(0, "gemini"), upstream(1, "anthropic")],
            providers,
            Arc::new(FallbackStrategy),
            Arc::clone(&health),
            Arc::new(AlwaysAllow),
            metrics,
        );

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(res.is_ok(), "must fail over to the healthy candidate");
        let remaining = health.remaining_secs(0);
        assert!(
            remaining >= crate::providers::gemini::DRIFT_COOLDOWN_SECS - 1,
            "gemini's cooldown must be tripped for ~DRIFT_COOLDOWN_SECS, got {remaining}s remaining"
        );
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_should_leave_anthropic_and_bedrock_dispatch_unaffected_when_gemini_trips_drift_cooldown(
    ) {
        let gemini_calls = Arc::new(AtomicU32::new(0));
        let anthropic_calls = Arc::new(AtomicU32::new(0));
        let bedrock_calls = Arc::new(AtomicU32::new(0));
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysErrProvider {
                name: "gemini",
                error: || ProviderError::ResponseShapeMismatch("bad shape".to_string()),
                call_count: Arc::clone(&gemini_calls),
            }),
            Arc::new(AlwaysOkProvider {
                name: "anthropic",
                call_count: Arc::clone(&anthropic_calls),
            }),
            Arc::new(AlwaysOkProvider {
                name: "bedrock",
                call_count: Arc::clone(&bedrock_calls),
            }),
        ];
        let metrics = MetricsCollector::new();
        let health = Arc::new(HealthRegistry::new(300));
        let router = Router::new(
            vec![
                upstream(0, "gemini"),
                upstream(1, "anthropic"),
                upstream(2, "bedrock"),
            ],
            providers,
            Arc::new(FallbackStrategy),
            Arc::clone(&health),
            Arc::new(AlwaysAllow),
            metrics,
        );

        // First dispatch: gemini errors and trips its own cooldown,
        // anthropic serves the response. bedrock is never tried (fallback
        // stops at the first success).
        let res1 = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;
        assert!(res1.is_ok());
        assert_eq!(gemini_calls.load(Ordering::SeqCst), 1);
        assert_eq!(anthropic_calls.load(Ordering::SeqCst), 1);
        assert_eq!(bedrock_calls.load(Ordering::SeqCst), 0);

        // Second dispatch, while gemini is still cooling down: anthropic's
        // (and bedrock's, transitively) dispatch behavior is completely
        // unaffected — gemini is simply excluded from the healthy pool, not
        // retried, and not erroring anyone else's request.
        let res2 = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;
        assert!(res2.is_ok());
        assert_eq!(
            gemini_calls.load(Ordering::SeqCst),
            1,
            "gemini must not be retried while cooling down"
        );
        assert_eq!(anthropic_calls.load(Ordering::SeqCst), 2);
        assert_eq!(bedrock_calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_should_return_last_response_shape_mismatch_error_when_all_candidates_exhausted(
    ) {
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysErrProvider {
                name: "gemini",
                error: || ProviderError::ResponseShapeMismatch("bad shape 1".to_string()),
                call_count: Arc::new(AtomicU32::new(0)),
            }),
            Arc::new(AlwaysErrProvider {
                name: "gemini-2",
                error: || ProviderError::ResponseShapeMismatch("bad shape 2".to_string()),
                call_count: Arc::new(AtomicU32::new(0)),
            }),
        ];
        let metrics = MetricsCollector::new();
        let router = Router::new(
            vec![upstream(0, "gemini"), upstream(1, "gemini-2")],
            providers,
            Arc::new(FallbackStrategy),
            Arc::new(HealthRegistry::new(300)),
            Arc::new(AlwaysAllow),
            metrics,
        );

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        match res {
            Err(ProviderError::ResponseShapeMismatch(_)) => {}
            Err(other) => panic!("expected Err(ResponseShapeMismatch(_)), got Err({other:?})"),
            Ok(_) => panic!("expected Err(ResponseShapeMismatch(_)), got Ok(_)"),
        }
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

    // REQ-2 (Story 1.1.2): the exhaustive `UpstreamKind` match in
    // `build_providers` accepts `Gemini` and constructs a stub provider.

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn build_providers_should_construct_provider_for_upstream_kind_gemini() {
        let config = Config {
            upstreams: vec![crate::config::schema::Upstream {
                name: "gemini".to_string(),
                kind: UpstreamKind::Gemini {
                    project_id: "p1".to_string(),
                },
                auth: None,
            }],
            ..Config::default()
        };

        let (providers, openrouter_providers) = build_providers(&config).await.unwrap();

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].0, "gemini");
        assert_eq!(providers[0].1.name(), "gemini");
        assert!(
            openrouter_providers.is_empty(),
            "a non-openrouter upstream must not appear in the openrouter-index map"
        );
    }

    // REQ-1 (Story 1.2.3, Task 1.2.3d): the exhaustive `UpstreamKind` match
    // in `build_providers` accepts `Openrouter` and additionally returns the
    // index -> `Arc<OpenrouterProvider>` map alongside the existing
    // providers vec.
    //
    // `OpenrouterProvider::new` performs a live eager model-cache refresh as
    // an invariant of construction (Story 2.1.2) — a failure there is
    // logged, not propagated (see `OpenrouterProvider::new`'s doc comment),
    // so this test doesn't depend on live network access to pass; it only
    // asserts the construction/wiring contract this story owns.
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn build_providers_should_construct_provider_for_upstream_kind_openrouter() {
        let config = Config {
            upstreams: vec![
                crate::config::schema::Upstream {
                    name: "anthropic".to_string(),
                    kind: UpstreamKind::Anthropic,
                    auth: Some(crate::config::schema::AuthMethod::Bearer {
                        token: crate::config::schema::SecretRef::Inline {
                            value: "sk-ant-test".to_string(),
                        },
                    }),
                },
                crate::config::schema::Upstream {
                    name: "openrouter".to_string(),
                    kind: UpstreamKind::Openrouter {},
                    auth: Some(crate::config::schema::AuthMethod::Bearer {
                        token: crate::config::schema::SecretRef::Inline {
                            value: "sk-or-v1-test".to_string(),
                        },
                    }),
                },
            ],
            ..Config::default()
        };

        let (providers, openrouter_providers) = build_providers(&config).await.unwrap();

        assert_eq!(providers.len(), 2);
        assert_eq!(providers[1].0, "openrouter");
        assert_eq!(providers[1].1.name(), "openrouter");
        assert_eq!(openrouter_providers.len(), 1);
        assert!(openrouter_providers.contains_key(&1));
    }

    // REQ-1/Blocker 5 (Story 4.3.1, Task 4.3.1b): a route using
    // `strategy = "openrouter_scored"` whose upstreams resolve to no
    // `openrouter`-kind upstream at all fails `from_config`, naming the
    // route.
    #[tokio::test]
    async fn from_config_should_reject_openrouter_scored_route_without_openrouter_upstream() {
        use crate::config::schema::{Route, RouteUpstreamRef};

        let config = Config {
            upstreams: vec![bearer_upstream(
                "anthropic",
                UpstreamKind::Anthropic,
                "sk-ant-test",
            )],
            routes: vec![Route {
                name: "or-route".to_string(),
                strategy: Strategy::OpenrouterScored,
                upstreams: vec![RouteUpstreamRef {
                    name: "anthropic".to_string(),
                    weight: None,
                    model: None,
                }],
            }],
            ..Config::default()
        };

        let Err(err) = Router::from_config(&config, MetricsCollector::new()).await else {
            panic!("openrouter_scored route without an openrouter-kind upstream must fail")
        };
        assert!(
            err.to_string().contains("or-route"),
            "error must name the route, got: {err}"
        );
    }

    // REQ-1 (Story 4.3.1, Task 4.3.1a): a route using
    // `strategy = "openrouter_scored"` with a matching `openrouter`-kind
    // upstream builds a `Router` whose strategy is an
    // `OpenrouterScoringStrategy` wired to that upstream's index and cache.
    #[tokio::test]
    #[allow(clippy::expect_used, clippy::unwrap_used)]
    async fn from_config_should_wire_openrouter_scoring_strategy_to_matching_upstream() {
        use crate::config::schema::{AuthMethod, Route, RouteUpstreamRef, Upstream};

        // A broken `AuthMethod::Exec` (nonexistent binary) makes
        // `OpenrouterProvider::new`'s eager cache refresh fail
        // deterministically in `build_headers`, before any network I/O is
        // attempted — the same hermetic-failure technique
        // `cache.rs`'s `broken_auth_provider` test helper uses, chosen
        // because this repo has no wiremock/mockito and a bearer-token
        // upstream would otherwise make a real, network-dependent request.
        let config = Config {
            upstreams: vec![Upstream {
                name: "openrouter".to_string(),
                kind: UpstreamKind::Openrouter {},
                auth: Some(AuthMethod::Exec {
                    command: "/nonexistent-binary-xyz-consolette-test".to_string(),
                    args: vec![],
                    cache_ttl_secs: 0,
                    timeout_secs: 1,
                }),
            }],
            routes: vec![Route {
                name: "or-route".to_string(),
                strategy: Strategy::OpenrouterScored,
                upstreams: vec![RouteUpstreamRef {
                    name: "openrouter".to_string(),
                    weight: None,
                    model: None,
                }],
            }],
            ..Config::default()
        };

        let router = Router::from_config(&config, MetricsCollector::new())
            .await
            .expect("openrouter_scored route with a matching openrouter-kind upstream must build");
        assert_eq!(router.candidates.len(), 1);
        assert_eq!(router.candidates[0].index, 0);
        assert_eq!(router.candidates[0].name, "openrouter");
        assert_eq!(router.providers[0].name(), "openrouter");

        // Proves the wired strategy is really `OpenrouterScoringStrategy`,
        // not `FallbackStrategy`/`WeightedStrategy`: with the eager cache
        // refresh having failed above, `model_cache.snapshot()` is `None`.
        // `OpenrouterScoringStrategy::expand_candidates` fans a
        // `None`-snapshot candidate out to zero entries, so `dispatch`
        // returns `Exhausted` without ever attempting a provider call —
        // `Fallback`/`Weighted` would instead pass the lone candidate
        // through unchanged and actually attempt one (which would fail
        // differently, via the same broken exec auth, not `Exhausted`).
        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;
        match res {
            Err(ProviderError::Exhausted) => {}
            Err(other) => panic!(
                "expected Err(Exhausted) proving expand_candidates fanned the empty cache to \
                 zero candidates, got Err({other:?})"
            ),
            Ok(_) => panic!(
                "expected Err(Exhausted) proving expand_candidates fanned the empty cache to \
                 zero candidates, got Ok(_)"
            ),
        }
    }

    // Blocker 1 (architecture-review), Story 4.3.1d/e: the *other*
    // direction of the symmetric validation — an `openrouter`-kind upstream
    // referenced by a `Fallback` route fails `from_config`, naming both the
    // route and the upstream.
    #[tokio::test]
    async fn from_config_should_reject_fallback_route_referencing_openrouter_upstream() {
        use crate::config::schema::{Route, RouteUpstreamRef};

        let config = Config {
            upstreams: vec![bearer_upstream(
                "or",
                UpstreamKind::Openrouter {},
                "sk-or-v1-test",
            )],
            routes: vec![Route {
                name: "r1".to_string(),
                strategy: Strategy::Fallback,
                upstreams: vec![RouteUpstreamRef {
                    name: "or".to_string(),
                    weight: None,
                    model: None,
                }],
            }],
            ..Config::default()
        };

        let Err(err) = Router::from_config(&config, MetricsCollector::new()).await else {
            panic!("a Fallback route referencing an openrouter-kind upstream must fail")
        };
        let msg = err.to_string();
        assert!(msg.contains("r1"), "error must name the route, got: {msg}");
        assert!(
            msg.contains("or"),
            "error must name the upstream, got: {msg}"
        );
    }

    // Blocker 1 (architecture-review), `Weighted` direction — same as above.
    #[tokio::test]
    async fn from_config_should_reject_weighted_route_referencing_openrouter_upstream() {
        use crate::config::schema::{Route, RouteUpstreamRef};

        let config = Config {
            upstreams: vec![bearer_upstream(
                "or",
                UpstreamKind::Openrouter {},
                "sk-or-v1-test",
            )],
            routes: vec![Route {
                name: "r1".to_string(),
                strategy: Strategy::Weighted,
                upstreams: vec![RouteUpstreamRef {
                    name: "or".to_string(),
                    weight: None,
                    model: None,
                }],
            }],
            ..Config::default()
        };

        let Err(err) = Router::from_config(&config, MetricsCollector::new()).await else {
            panic!("a Weighted route referencing an openrouter-kind upstream must fail")
        };
        let msg = err.to_string();
        assert!(msg.contains("r1"), "error must name the route, got: {msg}");
        assert!(
            msg.contains("or"),
            "error must name the upstream, got: {msg}"
        );
    }

    // Blocker 1 (architecture-review), mixed-upstream direction: a
    // `Fallback` route mixing an `openrouter`-kind upstream with a
    // non-openrouter upstream is still rejected — the presence of *any*
    // openrouter-kind upstream in a non-`OpenrouterScored` route's
    // `upstreams` list is sufficient to reject it, regardless of what else
    // is in that list.
    #[tokio::test]
    async fn from_config_should_reject_mixed_upstream_fallback_route_containing_openrouter_upstream(
    ) {
        use crate::config::schema::{Route, RouteUpstreamRef};

        let config = Config {
            upstreams: vec![
                bearer_upstream("anthropic", UpstreamKind::Anthropic, "sk-ant-test"),
                bearer_upstream("or", UpstreamKind::Openrouter {}, "sk-or-v1-test"),
            ],
            routes: vec![Route {
                name: "r1".to_string(),
                strategy: Strategy::Fallback,
                upstreams: vec![
                    RouteUpstreamRef {
                        name: "anthropic".to_string(),
                        weight: None,
                        model: None,
                    },
                    RouteUpstreamRef {
                        name: "or".to_string(),
                        weight: None,
                        model: None,
                    },
                ],
            }],
            ..Config::default()
        };

        let Err(err) = Router::from_config(&config, MetricsCollector::new()).await else {
            panic!(
                "a Fallback route mixing an openrouter-kind upstream with a non-openrouter \
                 upstream must still fail"
            )
        };
        let msg = err.to_string();
        assert!(msg.contains("r1"), "error must name the route, got: {msg}");
        assert!(
            msg.contains("or"),
            "error must name the upstream, got: {msg}"
        );
    }

    // Task 4.3.1g (adversarial-review Concern), reverse mixed-upstream
    // direction: an `openrouter_scored` route must not itself list a
    // non-openrouter (e.g. paid) upstream — without this guard, a
    // misconfigured route mixing a free `openrouter`-kind pool with a paid
    // upstream could silently fall through to the paid upstream once the
    // free pool is exhausted, spending real money on a route believed to be
    // free-only.
    #[tokio::test]
    async fn from_config_should_reject_openrouter_scored_route_mixing_paid_upstream() {
        use crate::config::schema::{Route, RouteUpstreamRef};

        let config = Config {
            upstreams: vec![
                bearer_upstream("or", UpstreamKind::Openrouter {}, "sk-or-v1-test"),
                bearer_upstream("paid-anthropic", UpstreamKind::Anthropic, "sk-ant-test"),
            ],
            routes: vec![Route {
                name: "r2".to_string(),
                strategy: Strategy::OpenrouterScored,
                upstreams: vec![
                    RouteUpstreamRef {
                        name: "or".to_string(),
                        weight: None,
                        model: None,
                    },
                    RouteUpstreamRef {
                        name: "paid-anthropic".to_string(),
                        weight: None,
                        model: None,
                    },
                ],
            }],
            ..Config::default()
        };

        let Err(err) = Router::from_config(&config, MetricsCollector::new()).await else {
            panic!(
                "an openrouter_scored route mixing in a non-openrouter (paid) upstream must fail"
            )
        };
        let msg = err.to_string();
        assert!(msg.contains("r2"), "error must name the route, got: {msg}");
        assert!(
            msg.contains("paid-anthropic"),
            "error must name the offending non-openrouter upstream, got: {msg}"
        );
    }

    // Story 1.3.4 replaced `GeminiProvider::stub` with the real, fallible
    // `GeminiProvider::new` (ADR-004 two-client split) above — construction
    // only fails on a `reqwest::Client` build error (no auth validation at
    // construction time, matching `AnthropicProvider::new`/`OpenaiProvider::new`),
    // so `build_providers_should_construct_provider_for_upstream_kind_gemini`
    // above still exercises the happy path with `auth: None` correctly.

    // REQ-6 (Story 1.3.4f): "does the translated response actually flow back
    // through `Router::dispatch` correctly" — a fake `Provider` standing in
    // for `GeminiProvider` (rescoped away from a mocked-HTTP-server
    // integration test per validation.md's Test Stack Notes; the real
    // translation logic itself is unit-tested directly in
    // `providers::gemini::translate`/`providers::gemini::mod`).
    struct FakeGeminiLikeProvider;

    #[async_trait::async_trait]
    impl Provider for FakeGeminiLikeProvider {
        fn name(&self) -> &'static str {
            "gemini"
        }

        async fn send(
            &self,
            _body: serde_json::Value,
            _headers: HeaderMap,
            _stream: bool,
        ) -> Result<ProviderResponse, ProviderError> {
            // Mirrors the Anthropic-shaped body `GeminiProvider::send`
            // returns after translating a STOP-finish-reason Gemini response
            // (Story 1.3.2's example fixture).
            Ok(ProviderResponse::Full(serde_json::json!({
                "type": "message",
                "role": "assistant",
                "model": "gemini-3-pro",
                "content": [{"type": "text", "text": "hello"}],
                "stop_reason": "end_turn",
                "usage": {
                    "input_tokens": 10,
                    "output_tokens": 5,
                    "cache_creation_input_tokens": 0,
                    "cache_read_input_tokens": 0,
                },
            })))
        }

        async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_should_flow_translated_gemini_shaped_response_back_unchanged_and_attribute_metrics_to_gemini(
    ) {
        let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(FakeGeminiLikeProvider)];
        let metrics = MetricsCollector::new();
        let router = Router::new(
            vec![upstream(0, "gemini")],
            providers,
            Arc::new(FallbackStrategy),
            Arc::new(HealthRegistry::new(300)),
            Arc::new(AlwaysAllow),
            metrics.clone(),
        );

        let body = serde_json::json!({
            "model": "gemini-3-pro",
            "messages": [{"role": "user", "content": "hi"}],
        });
        let result = router
            .dispatch(body, HeaderMap::new(), false, 0)
            .await
            .unwrap();

        match result {
            ProviderResponse::Full(value) => {
                assert_eq!(value["content"][0]["text"], "hello");
                assert_eq!(value["stop_reason"], "end_turn");
            }
            ProviderResponse::Stream(_) => panic!("expected a full response, not a stream"),
        }

        let gemini_counters = metrics.counters.upstreams.get("gemini").unwrap();
        assert_eq!(gemini_counters.requests.load(Ordering::Relaxed), 1);
        assert_eq!(gemini_counters.success.load(Ordering::Relaxed), 1);
    }

    // REQ-11 (Story 1.5.1) — `Router::cooldown_snapshot()` real feed.

    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn cooldown_snapshot_should_report_real_remaining_seconds_for_a_tripped_candidate() {
        let health = Arc::new(HealthRegistry::new(300));
        health.trip(1, Some(Duration::from_mins(15)));
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysOkProvider {
                name: "anthropic",
                call_count: Arc::new(AtomicU32::new(0)),
            }),
            Arc::new(AlwaysOkProvider {
                name: "gemini",
                call_count: Arc::new(AtomicU32::new(0)),
            }),
        ];
        let router = Router::new(
            vec![upstream(0, "anthropic"), upstream(1, "gemini")],
            providers,
            Arc::new(FallbackStrategy),
            health,
            Arc::new(AlwaysAllow),
            MetricsCollector::new(),
        );

        let snapshot = router.cooldown_snapshot();

        assert_eq!(
            snapshot["anthropic"]["cooling_down"],
            serde_json::json!(false)
        );
        assert_eq!(
            snapshot["anthropic"]["remaining_seconds"],
            serde_json::json!(0)
        );
        assert_eq!(snapshot["gemini"]["cooling_down"], serde_json::json!(true));
        let remaining = snapshot["gemini"]["remaining_seconds"]
            .as_u64()
            .expect("remaining_seconds must be a u64");
        assert!(
            remaining > 0 && remaining <= 900,
            "expected ~900s remaining, got {remaining}"
        );
    }

    #[tokio::test]
    async fn cooldown_snapshot_should_report_zero_remaining_seconds_for_a_healthy_candidate() {
        let health = Arc::new(HealthRegistry::new(300));
        let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(AlwaysOkProvider {
            name: "anthropic",
            call_count: Arc::new(AtomicU32::new(0)),
        })];
        let router = Router::new(
            vec![upstream(0, "anthropic")],
            providers,
            Arc::new(FallbackStrategy),
            health,
            Arc::new(AlwaysAllow),
            MetricsCollector::new(),
        );

        let snapshot = router.cooldown_snapshot();

        assert_eq!(
            snapshot["anthropic"],
            serde_json::json!({"cooling_down": false, "remaining_seconds": 0})
        );
    }

    /// A `Provider` test double that succeeds or fails per the request
    /// body's `model` field — used to prove `already_tried`'s widened key
    /// lets a sibling per-model candidate at the *same* upstream index stay
    /// selectable after another model at that index fails.
    struct ModelAwareProvider {
        name: &'static str,
        fail_model: &'static str,
        calls: Arc<std::sync::Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl Provider for ModelAwareProvider {
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
            let model = body["model"].as_str().unwrap_or("").to_string();
            self.calls.lock().unwrap().push(model.clone());
            if model == self.fail_model {
                Err(ProviderError::ModelUnsupported(model))
            } else {
                Ok(ProviderResponse::Full(serde_json::json!({"ok": true})))
            }
        }

        async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    // REQ-4 (Story 3.1.2, Task 3.1.2e): widening `already_tried` from
    // `HashSet<usize>` to `HashSet<(usize, Option<String>)>` lets the
    // dispatch loop retry a *different* free model sharing the same
    // upstream index after one model's attempt fails, instead of wrongly
    // declaring the whole pool exhausted.
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_should_retry_different_model_after_one_model_failure() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(ModelAwareProvider {
            name: "openrouter",
            fail_model: "a/b:free",
            calls: calls.clone(),
        })];
        let router = Router::new(
            vec![
                UpstreamRef {
                    index: 2,
                    name: "openrouter".to_string(),
                    weight: 1.0,
                    model: Some("a/b:free".to_string()),
                },
                UpstreamRef {
                    index: 2,
                    name: "openrouter".to_string(),
                    weight: 1.0,
                    model: Some("c/d:free".to_string()),
                },
            ],
            vec![
                Arc::new(AlwaysOkProvider {
                    name: "unused-0",
                    call_count: Arc::new(AtomicU32::new(0)),
                }),
                Arc::new(AlwaysOkProvider {
                    name: "unused-1",
                    call_count: Arc::new(AtomicU32::new(0)),
                }),
                providers[0].clone(),
            ],
            Arc::new(FallbackStrategy),
            Arc::new(HealthRegistry::new(300)),
            Arc::new(AlwaysAllow),
            MetricsCollector::new(),
        );

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(
            res.is_ok(),
            "must retry the sibling free model at the same index after the first fails"
        );
        let calls = calls.lock().unwrap();
        assert_eq!(*calls, vec!["a/b:free".to_string(), "c/d:free".to_string()]);
    }

    // Task 3.1.2f (architecture-review Concern, `research/architecture.md`
    // §3.4): the `already_tried` widening is a real, intentional, and
    // disclosed behavior change for `Fallback`/`Weighted` routes too, not
    // just an internal detail of the OpenRouter path — `RouteUpstreamRef.model`
    // is a general config field usable under any `UpstreamKind`, and two
    // route-upstream entries at the same index with different model pins are
    // a legitimate existing config shape. This is *not* a regression:
    // `FallbackStrategy::select`'s own logic is completely unmodified.
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_should_not_poison_sibling_model_pin_at_same_index_for_fallback_strategy() {
        let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
        let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(ModelAwareProvider {
            name: "primary",
            fail_model: "model-a",
            calls: calls.clone(),
        })];
        let router = Router::new(
            vec![
                UpstreamRef {
                    index: 3,
                    name: "primary".to_string(),
                    weight: 1.0,
                    model: Some("model-a".to_string()),
                },
                UpstreamRef {
                    index: 3,
                    name: "primary".to_string(),
                    weight: 1.0,
                    model: Some("model-b".to_string()),
                },
            ],
            vec![
                Arc::new(AlwaysOkProvider {
                    name: "unused-0",
                    call_count: Arc::new(AtomicU32::new(0)),
                }),
                Arc::new(AlwaysOkProvider {
                    name: "unused-1",
                    call_count: Arc::new(AtomicU32::new(0)),
                }),
                Arc::new(AlwaysOkProvider {
                    name: "unused-2",
                    call_count: Arc::new(AtomicU32::new(0)),
                }),
                providers[0].clone(),
            ],
            Arc::new(FallbackStrategy),
            Arc::new(HealthRegistry::new(300)),
            Arc::new(AlwaysAllow),
            MetricsCollector::new(),
        );

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(
            res.is_ok(),
            "model-a's failure must not poison model-b at the same index"
        );
        let calls = calls.lock().unwrap();
        assert_eq!(*calls, vec!["model-a".to_string(), "model-b".to_string()]);
    }

    /// One recorded `record_outcome` call: `(upstream name, success, error_kind)`.
    type RecordedOutcome = (String, bool, Option<&'static str>);

    /// A `RoutingStrategy` test double recording every `record_outcome`
    /// call, so `dispatch`'s wiring of the 3 new trait hooks can be verified
    /// directly rather than only indirectly through selection behavior.
    struct RecordingStrategy {
        outcomes: Arc<std::sync::Mutex<Vec<RecordedOutcome>>>,
    }

    impl RoutingStrategy for RecordingStrategy {
        fn select(&self, healthy: &[UpstreamRef]) -> Option<UpstreamRef> {
            healthy.first().cloned()
        }

        #[allow(clippy::unwrap_used)]
        fn record_outcome(
            &self,
            candidate: &UpstreamRef,
            _duration_ms: u64,
            success: bool,
            error_kind: Option<&'static str>,
        ) {
            self.outcomes
                .lock()
                .unwrap()
                .push((candidate.name.clone(), success, error_kind));
        }
    }

    // REQ-4 (Story 3.1.2, Task 3.1.2e/d): `Router::dispatch` calls
    // `strategy.record_outcome` once per attempt, after `provider.send()`
    // resolves, with the right `success`/`error_kind` — here, a
    // `ModelUnsupported` error's catch-all match arm.
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_should_call_record_outcome_with_error_kind_on_model_unsupported() {
        let outcomes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(AlwaysErrProvider {
            name: "primary",
            error: || ProviderError::ModelUnsupported("bad-model".to_string()),
            call_count: Arc::new(AtomicU32::new(0)),
        })];
        let router = Router::new(
            vec![upstream(0, "primary")],
            providers,
            Arc::new(RecordingStrategy {
                outcomes: outcomes.clone(),
            }),
            Arc::new(HealthRegistry::new(300)),
            Arc::new(AlwaysAllow),
            MetricsCollector::new(),
        );

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(res.is_err());
        let recorded = outcomes.lock().unwrap();
        assert_eq!(
            *recorded,
            vec![("primary".to_string(), false, Some("model_unsupported"))]
        );
    }

    // REQ-4 (Story 3.1.2, Task 3.1.2c): `expand_candidates` is called once,
    // before the health filter — proven via a strategy whose
    // `expand_candidates` fans one static candidate into two.
    struct ExpandingStrategy;

    impl RoutingStrategy for ExpandingStrategy {
        fn select(&self, healthy: &[UpstreamRef]) -> Option<UpstreamRef> {
            healthy.first().cloned()
        }

        fn expand_candidates(&self, candidates: Vec<UpstreamRef>) -> Vec<UpstreamRef> {
            candidates
                .into_iter()
                .flat_map(|c| {
                    vec![
                        UpstreamRef {
                            model: Some("model-a".to_string()),
                            ..c.clone()
                        },
                        UpstreamRef {
                            model: Some("model-b".to_string()),
                            ..c
                        },
                    ]
                })
                .collect()
        }
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_should_expand_candidates_before_the_health_filter() {
        let received_body = Arc::new(std::sync::Mutex::new(None));
        let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(CapturingProvider {
            name: "openrouter",
            received_body: received_body.clone(),
        })];
        let router = Router::new(
            vec![upstream(0, "openrouter")],
            providers,
            Arc::new(ExpandingStrategy),
            Arc::new(HealthRegistry::new(300)),
            Arc::new(AlwaysAllow),
            MetricsCollector::new(),
        );

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(res.is_ok());
        let body = received_body.lock().unwrap().clone().unwrap();
        // `select` (via `FallbackStrategy`-style "first healthy") picks the
        // first of the 2 expanded candidates.
        assert_eq!(body["model"], serde_json::json!("model-a"));
    }

    // REQ-4 (Story 4.2.3, Task 4.2.3c, ADR-002): a 429 from one per-model
    // `OpenrouterScoringStrategy` candidate trips `HealthRegistry` for the
    // *shared* upstream index those candidates all share, making every
    // other per-model candidate at that index unavailable too — proving
    // ADR-002's claim that the existing whole-upstream cooldown already
    // delivers `Retry-After` fidelity without a sibling per-model registry.
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn record_outcome_rate_limited_should_trip_health_registry_for_shared_index() {
        let call_count = Arc::new(AtomicU32::new(0));
        let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(AlwaysErrProvider {
            name: "openrouter",
            error: || ProviderError::RateLimited,
            call_count: Arc::clone(&call_count),
        })];
        let health = Arc::new(HealthRegistry::new(300));
        let model_cache = Arc::new(
            crate::providers::openrouter::cache::ModelListCache::new_with_ttl(Duration::from_mins(
                15,
            )),
        );
        let strategy = Arc::new(
            crate::routing::openrouter_scoring::OpenrouterScoringStrategy::new(model_cache, 0),
        );
        let router = Router::new(
            vec![
                UpstreamRef {
                    index: 0,
                    name: "openrouter".to_string(),
                    weight: 1.0,
                    model: Some("a/b:free".to_string()),
                },
                UpstreamRef {
                    index: 0,
                    name: "openrouter".to_string(),
                    weight: 1.0,
                    model: Some("c/d:free".to_string()),
                },
            ],
            providers,
            strategy,
            Arc::clone(&health),
            Arc::new(AlwaysAllow),
            MetricsCollector::new(),
        );

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(matches!(res, Err(ProviderError::RateLimited)));
        assert!(
            !health.is_available(0),
            "the shared upstream index must be cooling down after one per-model 429"
        );
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "the sibling per-model candidate must never be attempted once the shared index cools down"
        );
    }

    // ── REQ-7 (Story 5.1.3, Task 5.1.3c) — `RequestDetail.selected_model`. ──

    // *Given* a dispatch that selects a per-model candidate
    // `UpstreamRef{model: Some("a/b:free"), ..}`, *when* the request
    // completes, *then* its `RequestDetail.selected_model ==
    // Some("a/b:free".to_string())`.
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_should_set_selected_model_on_request_detail() {
        let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(AlwaysOkProvider {
            name: "openrouter",
            call_count: Arc::new(AtomicU32::new(0)),
        })];
        let metrics = MetricsCollector::new();
        let router = Router::new(
            vec![UpstreamRef {
                index: 0,
                name: "openrouter".to_string(),
                weight: 1.0,
                model: Some("a/b:free".to_string()),
            }],
            providers,
            Arc::new(FallbackStrategy),
            Arc::new(HealthRegistry::new(300)),
            Arc::new(AlwaysAllow),
            Arc::clone(&metrics),
        );

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(res.is_ok());
        let recent = metrics.get_recent_requests(1);
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].selected_model, Some("a/b:free".to_string()));
    }

    // *Given* a dispatch on `FallbackStrategy` (candidates always have
    // `model: None`), *when* the request completes, *then*
    // `RequestDetail.selected_model == None`.
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_should_leave_selected_model_none_for_fallback_strategy() {
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
            Arc::clone(&metrics),
        );

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(res.is_ok());
        let recent = metrics.get_recent_requests(1);
        assert_eq!(recent.len(), 1);
        assert_eq!(
            recent[0].selected_model, None,
            "FallbackStrategy candidates always carry model: None"
        );
    }

    // ── REQ-6 (Task 4.3.1f): all-free-candidates-cooling-down exhaustion. ──

    /// Builds an `OpenrouterScoringStrategy`-driven `Router` with 2 live
    /// per-model candidates sharing the openrouter upstream's index 0 (ADR-002:
    /// one whole-upstream `HealthRegistry` cooldown covers every per-model
    /// candidate at that index) plus a second, healthy "paid" candidate at
    /// index 1 — proving exhaustion doesn't fall back to it even though it's
    /// available, matching `OpenrouterScoringStrategy::select`'s own
    /// index-scoping defense-in-depth.
    fn openrouter_router_with_two_tripped_free_candidates(
        metrics: Arc<MetricsCollector>,
    ) -> (Router, Arc<HealthRegistry>, Arc<AtomicU32>, Arc<AtomicU32>) {
        let openrouter_calls = Arc::new(AtomicU32::new(0));
        let paid_calls = Arc::new(AtomicU32::new(0));
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysOkProvider {
                name: "openrouter",
                call_count: Arc::clone(&openrouter_calls),
            }),
            Arc::new(AlwaysOkProvider {
                name: "paid",
                call_count: Arc::clone(&paid_calls),
            }),
        ];
        let health = Arc::new(HealthRegistry::new(300));
        // Both free-model candidates are already cooling down *before*
        // dispatch is ever called — the literal REQ-6 scenario, not a
        // cooldown tripped mid-call (that's the sibling
        // `record_outcome_rate_limited_should_trip_health_registry_for_shared_index`
        // test, which returns `RateLimited`, not `Exhausted`, for that call).
        health.trip(0, None);
        let model_cache = Arc::new(
            crate::providers::openrouter::cache::ModelListCache::new_with_ttl(Duration::from_mins(
                15,
            )),
        );
        let strategy = Arc::new(OpenrouterScoringStrategy::new(model_cache, 0));
        let router = Router::new(
            vec![
                UpstreamRef {
                    index: 0,
                    name: "openrouter".to_string(),
                    weight: 1.0,
                    model: Some("a/b:free".to_string()),
                },
                UpstreamRef {
                    index: 0,
                    name: "openrouter".to_string(),
                    weight: 1.0,
                    model: Some("c/d:free".to_string()),
                },
                UpstreamRef {
                    index: 1,
                    name: "paid".to_string(),
                    weight: 1.0,
                    model: None,
                },
            ],
            providers,
            strategy,
            Arc::clone(&health),
            Arc::new(AlwaysAllow),
            metrics,
        );
        (router, health, openrouter_calls, paid_calls)
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_should_return_exhausted_when_all_free_model_candidates_are_cooling_down() {
        let (router, health, openrouter_calls, paid_calls) =
            openrouter_router_with_two_tripped_free_candidates(MetricsCollector::new());
        assert!(
            !health.is_available(0),
            "precondition: the free-model pool must already be cooling down"
        );

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        match res {
            Err(ProviderError::Exhausted) => {}
            Err(other) => panic!(
                "expected Err(Exhausted) with 2 live but cooling-down free-model candidates, \
                 got Err({other:?})"
            ),
            Ok(_) => panic!(
                "expected Err(Exhausted) with 2 live but cooling-down free-model candidates, \
                 got Ok(_)"
            ),
        }
        assert_eq!(
            openrouter_calls.load(Ordering::SeqCst),
            0,
            "a cooling-down candidate must never be attempted"
        );
        assert_eq!(
            paid_calls.load(Ordering::SeqCst),
            0,
            "exhaustion of the free pool must not fall back to the healthy paid upstream"
        );
    }

    // REQ-6: exhaustion attributes the same `last_error_kind`/`kind_label()
    // == "exhausted"` classification the dashboard already reads for a
    // per-attempt failure (design/ux.md §6) — even though, in this
    // all-cooling-down-before-dispatch scenario, no attempt is ever made to
    // trigger `record_attempt`'s usual `set_last_error_kind` call.
    #[tokio::test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    async fn dispatch_should_attribute_exhausted_kind_to_dashboard_counters() {
        let metrics = MetricsCollector::new();
        let (router, _health, _openrouter_calls, _paid_calls) =
            openrouter_router_with_two_tripped_free_candidates(Arc::clone(&metrics));

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(matches!(res, Err(ProviderError::Exhausted)));
        let kind = *metrics
            .counters
            .upstreams
            .get("openrouter")
            .expect("dispatch must record a last_error_kind entry for the openrouter upstream")
            .last_error_kind
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            kind,
            Some(ProviderError::Exhausted.kind_label()),
            "exhaustion must attribute kind_label() == \"exhausted\", matching the per-attempt \
             record_attempt path"
        );
    }

    // ── Pre-mortem P2 #2: `explore` flag reaches `RequestDetail`. ──

    // *Given* a dispatch whose selection is forced onto the greedy branch
    // (a single candidate — `select()` always takes the "sole candidate"
    // path, which epsilon-greedy still marks `explore: false` for), *when*
    // the request completes, *then* `RequestDetail.selected_model_was_exploration
    // == Some(false)`.
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_should_set_selected_model_was_exploration_false_for_sole_candidate() {
        let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(AlwaysOkProvider {
            name: "openrouter",
            call_count: Arc::new(AtomicU32::new(0)),
        })];
        let metrics = MetricsCollector::new();
        let model_cache = Arc::new(
            crate::providers::openrouter::cache::ModelListCache::new_with_ttl(Duration::from_mins(
                15,
            )),
        );
        let strategy = Arc::new(OpenrouterScoringStrategy::new(model_cache, 0));
        let router = Router::new(
            vec![UpstreamRef {
                index: 0,
                name: "openrouter".to_string(),
                weight: 1.0,
                model: Some("only/model:free".to_string()),
            }],
            providers,
            strategy,
            Arc::new(HealthRegistry::new(300)),
            Arc::new(AlwaysAllow),
            Arc::clone(&metrics),
        );

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(res.is_ok());
        let recent = metrics.get_recent_requests(1);
        assert_eq!(recent.len(), 1);
        // `OpenrouterScoringStrategy::select` still runs its epsilon-greedy
        // coin flip even with a single candidate, but "sole candidate" is
        // returned either way; `last_explore` records whichever branch was
        // actually taken. Assert it's populated (`Some(_)`), not a specific
        // bool, since the explore roll is genuinely random -- the sibling
        // test below pins it deterministically via `record_outcome`'s
        // absence of randomness instead.
        assert!(
            recent[0].selected_model_was_exploration.is_some(),
            "OpenrouterScoringStrategy must always report an explore/greedy outcome for a \
             selected model, got {:?}",
            recent[0].selected_model_was_exploration
        );
    }

    // *Given* a dispatch on `FallbackStrategy` (no explore/greedy concept),
    // *when* the request completes, *then*
    // `RequestDetail.selected_model_was_exploration == None`.
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_should_leave_selected_model_was_exploration_none_for_fallback_strategy() {
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
            Arc::clone(&metrics),
        );

        let res = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;

        assert!(res.is_ok());
        let recent = metrics.get_recent_requests(1);
        assert_eq!(recent.len(), 1);
        assert_eq!(
            recent[0].selected_model_was_exploration, None,
            "FallbackStrategy has no explore/greedy distinction to report"
        );
    }
}
