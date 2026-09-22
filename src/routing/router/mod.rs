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
use crate::metrics::{MetricsCollector, RequestTimingUpdate};
use crate::providers::anthropic::AnthropicProvider;
use crate::providers::bedrock::BedrockProvider;
use crate::providers::gemini::GeminiProvider;
use crate::providers::openai::OpenaiProvider;
use crate::providers::openrouter::OpenrouterProvider;
use crate::providers::{Provider, ProviderError, ProviderResponse};
use crate::ratelimit::{AdmissionControl, Admit, RateLimiters};

use super::capability::CapabilityCache;
use super::health::{Availability, HealthRegistry};
use super::openrouter_scoring::OpenrouterScoringStrategy;
use super::session_overrides::{extract_session_id, SessionOverrideStore};
use super::strategy::{FallbackStrategy, RoutingStrategy, UpstreamRef, WeightedStrategy};

/// `hold_for_rate_limit_cooldown`'s retry budget — how many times `dispatch`
/// will hold and re-check candidates before giving up.
const MAX_HOLD_RETRIES: u32 = 3;
/// `hold_for_rate_limit_cooldown` only holds when the shortest live
/// cooldown among candidates is at most this many seconds; a longer wait
/// isn't worth insulating the caller from.
const MAX_HOLD_WAIT_SECS: u64 = 15;
/// Fixed jitter added on top of the computed wait, so a hold sleep always
/// slightly overshoots the cooldown's expiry instead of racing it.
const HOLD_JITTER_MS: u64 = 200;

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
    /// Capability admission verdicts, consulted alongside health on every
    /// dispatch (see `dispatch`'s doc comment). Same carry-across discipline
    /// as `session_overrides` via `with_capability`, so learned verdicts
    /// survive a route hot-swap.
    capability: Arc<CapabilityCache>,
}

/// The four non-`openrouter`-kind upstreams: a plain `Arc<dyn Provider>`,
/// no `ModelListCache` handle to thread back.
async fn build_non_openrouter_provider(
    upstream: &crate::config::schema::Upstream,
    resolver: &Arc<dyn SecretResolver + Send + Sync>,
    exec_cache: &Arc<ExecCredentialCache>,
    request_timeout_secs: u64,
) -> anyhow::Result<Arc<dyn Provider>> {
    Ok(match &upstream.kind {
        UpstreamKind::Anthropic => Arc::new(AnthropicProvider::new(
            Arc::new(upstream.clone()),
            Arc::clone(resolver),
            Arc::clone(exec_cache),
            request_timeout_secs,
        )?),
        UpstreamKind::Bedrock { .. } => {
            Arc::new(BedrockProvider::new(Arc::new(upstream.clone())).await)
        }
        UpstreamKind::Openai { base_url } => Arc::new(OpenaiProvider::new(
            Arc::new(upstream.clone()),
            base_url.clone(),
            Arc::clone(resolver),
            Arc::clone(exec_cache),
            request_timeout_secs,
        )?),
        UpstreamKind::Gemini { .. } => Arc::new(GeminiProvider::new(
            Arc::new(upstream.clone()),
            Arc::clone(resolver),
            Arc::clone(exec_cache),
            request_timeout_secs,
        )?),
        UpstreamKind::Openrouter {} => {
            unreachable!("build_providers dispatches openrouter-kind separately")
        }
    })
}

/// Builds one upstream's live `Provider`, plus its `Arc<OpenrouterProvider>`
/// handle when it's `openrouter`-kind — split out of `build_providers` so
/// the per-kind construction match isn't inlined into the loop that
/// assembles the full upstream list.
///
/// # Errors
///
/// Returns `Err` if the upstream fails to construct its `Provider`.
async fn build_provider_for_upstream(
    upstream: &crate::config::schema::Upstream,
    resolver: &Arc<dyn SecretResolver + Send + Sync>,
    exec_cache: &Arc<ExecCredentialCache>,
    request_timeout_secs: u64,
) -> anyhow::Result<(Arc<dyn Provider>, Option<Arc<OpenrouterProvider>>)> {
    if !matches!(upstream.kind, UpstreamKind::Openrouter {}) {
        let provider =
            build_non_openrouter_provider(upstream, resolver, exec_cache, request_timeout_secs)
                .await?;
        return Ok((provider, None));
    }

    // `OpenrouterProvider::new` already returns `Arc<OpenrouterProvider>`
    // (Task 1.2.1b), not a bare `Self`, so no extra `Arc::new(..)` wrap is
    // needed here. It also eagerly refreshes its model cache as an
    // invariant of construction (Story 2.1.2's Blocker-1 fix) — this
    // happens for every `openrouter`-kind upstream regardless of which
    // `Strategy` any route pairs it with.
    let provider = OpenrouterProvider::new(
        Arc::new(upstream.clone()),
        Arc::clone(resolver),
        Arc::clone(exec_cache),
        request_timeout_secs,
    )
    .await?;
    Ok((Arc::clone(&provider) as Arc<dyn Provider>, Some(provider)))
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
        let (provider, openrouter_provider) =
            build_provider_for_upstream(upstream, &resolver, &exec_cache, config.request_timeout)
                .await?;
        if let Some(openrouter_provider) = openrouter_provider {
            openrouter_providers.insert(index, openrouter_provider);
        }
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

/// Bedrock's own exponential backoff makes ADR-003 cooldown redundant (and
/// actively harmful — it would keep the router from retrying an upstream
/// Bedrock has already decided is healthy again). Gemini is a real network
/// upstream and must NOT be included here (see
/// project_plans/gemini-provider/implementation/plan.md Story 1.1.2).
fn disable_bedrock_cooldown(config: &Config, health: &HealthRegistry) {
    for (index, upstream) in config.upstreams.iter().enumerate() {
        if matches!(upstream.kind, UpstreamKind::Bedrock { .. }) {
            health.set_can_cooldown(index, false);
        }
    }
}

/// The one `Route` `from_config` builds a `Router` from — always the
/// first configured route; any others are logged and ignored.
///
/// # Errors
///
/// Returns `Err` if `config.routes` is empty.
fn select_primary_route(config: &Config) -> anyhow::Result<&Route> {
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
    Ok(route)
}

/// Resolves `route`'s upstream-name references into `UpstreamRef`s indexed
/// against `config.upstreams`.
///
/// # Errors
///
/// Returns `Err` naming `route` and the offending name if it references an
/// upstream not present in `config.upstreams`.
fn build_route_candidates(route: &Route, config: &Config) -> anyhow::Result<Vec<UpstreamRef>> {
    let mut candidates = Vec::with_capacity(route.upstreams.len());
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
    Ok(candidates)
}

/// Resolves `route`'s configured `Strategy` into a live `RoutingStrategy`.
///
/// # Errors
///
/// Returns `Err` under the same conditions as
/// [`build_openrouter_scored_strategy`] for an `OpenrouterScored` route.
fn build_strategy_for_route(
    route: &Route,
    candidates: &[UpstreamRef],
    config: &Config,
    openrouter_providers: &HashMap<usize, Arc<OpenrouterProvider>>,
) -> anyhow::Result<Arc<dyn RoutingStrategy>> {
    match route.strategy {
        Strategy::Fallback => Ok(Arc::new(FallbackStrategy) as Arc<dyn RoutingStrategy>),
        Strategy::Weighted => Ok(Arc::new(WeightedStrategy) as Arc<dyn RoutingStrategy>),
        Strategy::OpenrouterScored => {
            build_openrouter_scored_strategy(route, candidates, config, openrouter_providers)
        }
    }
}

/// Per-model (e.g. `OpenRouter` free-pool) 429s must not cool down the whole
/// shared upstream index while untried sibling models remain — otherwise one
/// model's rate limit exhausts the entire pool after a single attempt. Defers
/// the whole-index trip until the last sibling has also been tried, so a
/// single dispatch walks the full free-model pool before giving up.
/// Model-less (traditional) candidates keep immediate-trip behavior: tripping
/// their index can't block a different-index fallback.
fn should_defer_rate_limit_trip(
    candidates: &[UpstreamRef],
    already_tried: &HashSet<(usize, Option<String>)>,
    chosen: &UpstreamRef,
) -> bool {
    chosen.model.is_some()
        && candidates.iter().any(|u| {
            u.index == chosen.index && !already_tried.contains(&(u.index, u.model.clone()))
        })
}

/// [`Router::new`]'s constructor dependencies, grouped into one struct
/// (Clean Code's "introduce parameter object") now that the positional list
/// is 6 identifiers long.
pub struct RouterDeps {
    pub candidates: Vec<UpstreamRef>,
    pub providers: Vec<Arc<dyn Provider>>,
    pub strategy: Arc<dyn RoutingStrategy>,
    pub health: Arc<HealthRegistry>,
    pub admission: Arc<dyn AdmissionControl>,
    pub metrics: Arc<MetricsCollector>,
}

impl Router {
    #[must_use]
    pub fn new(deps: RouterDeps) -> Self {
        Self {
            candidates: deps.candidates,
            providers: deps.providers,
            strategy: deps.strategy,
            health: deps.health,
            admission: deps.admission,
            metrics: deps.metrics,
            session_overrides: Arc::new(SessionOverrideStore::new()),
            capability: CapabilityCache::new(Duration::from_secs(
                crate::routing::capability::EVAL_TTL_SECS,
            )),
        }
    }

    /// Swaps in a shared capability-verdict cache, replacing the empty one
    /// `Router::new`/`from_config` starts with. Same carry-across contract
    /// as `with_session_overrides`: `api::post_route` passes the
    /// `EntrypointState`'s existing `Arc<CapabilityCache>` so learned
    /// verdicts survive a route rebuild.
    #[must_use]
    pub fn with_capability(mut self, capability: Arc<CapabilityCache>) -> Self {
        self.capability = capability;
        self
    }

    /// The shared admission-verdict cache (used by the background
    /// evaluator and `/metrics`).
    #[must_use]
    pub fn capability(&self) -> Arc<CapabilityCache> {
        Arc::clone(&self.capability)
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

        let health = Arc::new(HealthRegistry::new(config.cooldown_seconds));
        disable_bedrock_cooldown(config, &health);

        let route = select_primary_route(config)?;
        let candidates = build_route_candidates(route, config)?;
        let strategy = build_strategy_for_route(route, &candidates, config, &openrouter_providers)?;

        let admission = Arc::new(RateLimiters::new(&config.ratelimit)) as Arc<dyn AdmissionControl>;

        tracing::info!(
            route = %route.name,
            strategy = ?route.strategy,
            candidates = candidates.len(),
            "router assembled from config"
        );

        Ok(Router::new(RouterDeps {
            candidates,
            providers,
            strategy,
            health,
            admission,
            metrics,
        }))
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

    /// Healthy candidates not yet tried for this request, minus
    /// capability-exiled models. When admission would refuse every healthy
    /// candidate, fails open (loudly) rather than refusing all traffic — a
    /// stale `Fail` must never cause a wider outage than the degraded model
    /// it describes.
    fn admitted_candidates(
        &self,
        candidates: &[UpstreamRef],
        already_tried: &HashSet<(usize, Option<String>)>,
    ) -> Vec<UpstreamRef> {
        let healthy: Vec<UpstreamRef> = candidates
            .iter()
            .filter(|u| {
                !already_tried.contains(&(u.index, u.model.clone()))
                    && self.health.is_available(u.index)
            })
            .cloned()
            .collect();

        // Capability admission: skip candidates whose pinned model freshly
        // failed its tool-call eval.
        let admitted: Vec<UpstreamRef> = healthy
            .iter()
            .filter(|u| self.capability.is_admitted(&u.model))
            .cloned()
            .collect();
        if admitted.is_empty() && !healthy.is_empty() {
            tracing::warn!("capability admission excluded all healthy candidates; failing open");
            healthy
        } else {
            admitted
        }
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

        let mut hold_retries: u32 = 0;

        loop {
            let healthy = self.admitted_candidates(&candidates, &already_tried);

            let Some(chosen) = self.strategy.select(&healthy) else {
                // Insulate agents from temporary rate-limit cooldowns: if
                // all candidates are in short cooldown, hold and retry.
                if self
                    .hold_for_rate_limit_cooldown(&candidates, &mut hold_retries)
                    .await
                {
                    already_tried.clear();
                    continue;
                }
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
                    self.record_first_attempt_timing(&request_id, &chosen.name, duration_ms);
                    return Ok(response);
                }
                Err(e) if e.is_validation() || e.is_auth() => {
                    self.record_dispatch_outcome(&chosen, attempt_started, Err(&e), &model);
                    return Err(e);
                }
                Err(e) if e.is_rate_limited() => {
                    self.record_dispatch_outcome(&chosen, attempt_started, Err(&e), &model);
                    self.maybe_trip_rate_limit(&candidates, &already_tried, &chosen, &e);
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

    /// Insulates agents from a brief across-the-board rate-limit cooldown:
    /// when `dispatch`'s selection loop finds every candidate cooling down
    /// (not unhealthy for any other reason), holds the request and signals
    /// a retry with `already_tried` cleared, up to `MAX_HOLD_RETRIES` times,
    /// but only when the *shortest* live cooldown among `candidates` is
    /// itself short enough (<= `MAX_HOLD_WAIT_SECS`) to be worth waiting
    /// out. Returns whether the caller should retry.
    ///
    /// Worst case this adds up to `MAX_HOLD_RETRIES *
    /// (MAX_HOLD_WAIT_SECS + HOLD_JITTER_MS)` (~45s) of latency before
    /// `dispatch` returns an error to the caller — a caller with a tighter
    /// timeout budget than that may see a client-side timeout before this
    /// returns.
    async fn hold_for_rate_limit_cooldown(
        &self,
        candidates: &[UpstreamRef],
        hold_retries: &mut u32,
    ) -> bool {
        if *hold_retries >= MAX_HOLD_RETRIES {
            return false;
        }
        let Some(wait_secs) = candidates
            .iter()
            .map(|c| self.health.remaining_secs(c.index))
            .filter(|&secs| secs > 0)
            .min()
        else {
            return false;
        };
        if wait_secs > MAX_HOLD_WAIT_SECS {
            return false;
        }
        tracing::info!(
            "All upstream candidates rate-limited / cooling down. Holding request for {}s to insulate agent (attempt {}/{})...",
            wait_secs,
            *hold_retries + 1,
            MAX_HOLD_RETRIES
        );
        tokio::time::sleep(Duration::from_secs(wait_secs) + Duration::from_millis(HOLD_JITTER_MS))
            .await;
        *hold_retries += 1;
        true
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

    /// Trips the whole-upstream cooldown for a rate-limited attempt, unless
    /// it is a per-model 429 with untried siblings at the same index (see
    /// `should_defer_rate_limit_trip`) — in that case the pool still has
    /// models worth trying, so the trip waits for the last sibling.
    fn maybe_trip_rate_limit(
        &self,
        candidates: &[UpstreamRef],
        already_tried: &HashSet<(usize, Option<String>)>,
        chosen: &UpstreamRef,
        error: &ProviderError,
    ) {
        if should_defer_rate_limit_trip(candidates, already_tried, chosen) {
            return;
        }
        let override_duration = error.retry_after_secs().map(Duration::from_secs);
        self.health.trip(chosen.index, override_duration);
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

    /// Route candidates eligible for capability evaluation: pinned model
    /// ids ending in `:free`. The suffix doubles as the paid guard —
    /// subscription/paid pins are never probed (probes cost real money)
    /// and stay admitted exactly as today. Deduped by (index, model).
    #[must_use]
    pub fn eval_targets(&self) -> Vec<(usize, String)> {
        let mut seen = HashSet::new();
        let mut targets = Vec::new();
        for candidate in &self.candidates {
            if let Some(model) = candidate.model.clone() {
                if model.ends_with(":free") && seen.insert((candidate.index, model.clone())) {
                    targets.push((candidate.index, model));
                }
            }
        }
        targets
    }

    /// Sends one body straight to a single upstream, bypassing
    /// metrics/stats/health/session-pins. Capability probes use this so
    /// eval traffic never moves production signals or trips cooldowns.
    ///
    /// # Errors
    ///
    /// Returns the provider's error, or a validation error for an
    /// out-of-range index (unreachable via [`Router::eval_targets`]).
    pub async fn probe_upstream(
        &self,
        index: usize,
        body: serde_json::Value,
    ) -> Result<ProviderResponse, ProviderError> {
        let Some(provider) = self.providers.get(index) else {
            return Err(ProviderError::Validation(
                format!("capability probe for unknown upstream index {index}"),
                500,
            ));
        };
        provider.send(body, HeaderMap::new(), false).await
    }

    /// The admission-verdict cache's `/metrics`-facing snapshot (mirrors
    /// `cooldown_snapshot`).
    #[must_use]
    pub fn capability_snapshot(&self) -> serde_json::Value {
        self.capability.snapshot()
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
                    .record_request_success(upstream, duration_ms, 0);
                // Story 1.4.4: a successful attempt clears the upstream's
                // last error classification, so the dashboard self-heals
                // instead of a single past failure permanently pinning its
                // status class.
                self.metrics.counters.set_last_error_kind(upstream, None);
            }
            Err(e) => {
                self.metrics
                    .counters
                    .record_request_failure(upstream, duration_ms, 0);
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
        let model_outcome = match outcome {
            Ok(()) => crate::metrics::ModelOutcome::Success,
            Err(e) if e.is_rate_limited() => crate::metrics::ModelOutcome::RateLimited,
            Err(_) => crate::metrics::ModelOutcome::Error,
        };
        let effective_model = chosen.model.as_deref().unwrap_or(model);
        self.metrics
            .counters
            .record_model_attempt(effective_model, model_outcome);
        self.strategy
            .record_outcome(chosen, duration_ms, success, error_kind);
    }

    /// Records a successful first-attempt's timing on `/metrics`. First-byte
    /// time isn't separately measured here (see `record_attempt`'s doc
    /// comment) — `provider.send` returning is the closest proxy we have for
    /// either a full response or a stream's headers, so both fields get the
    /// same `duration_ms`.
    fn record_first_attempt_timing(&self, request_id: &str, provider_name: &str, duration_ms: f64) {
        self.metrics.update_request_timing(
            request_id,
            RequestTimingUpdate {
                provider: provider_name,
                duration_ms,
                first_byte_ms: duration_ms,
                bedrock_invocation_ms: 0,
                bedrock_first_byte_ms: 0,
            },
        );
    }
}

#[cfg(test)]
mod tests;
