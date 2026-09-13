//! `Router`: owns the dispatch loop shared by every strategy (ADR-003).
//!
//! Shrinks the candidate set per attempt (`already_tried`), reusing the old
//! `FallbackHandler::dispatch` error-class branching verbatim: validation and
//! auth errors return immediately (no failover); rate-limit errors trip the
//! upstream's cooldown and continue; other (transient) errors continue
//! without tripping cooldown. Same-upstream retries (e.g. Bedrock's
//! exponential backoff) stay inside the provider — the router only fails
//! over to a *different* upstream.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use http::HeaderMap;

use crate::auth::exec::ExecCredentialCache;
use crate::auth::{SecretResolver, SystemSecretResolver};
use crate::config::schema::{Config, FamilyMember, Strategy, UpstreamKind};
use crate::metrics::{MemberRecordClass, MetricsCollector};
use crate::providers::anthropic::AnthropicProvider;
use crate::providers::bedrock::BedrockProvider;
use crate::providers::gemini::GeminiProvider;
use crate::providers::openai::OpenaiProvider;
use crate::providers::{Provider, ProviderError, ProviderResponse};
use crate::ratelimit::{AdmissionControl, Admit, RateLimiters};

use super::family::{
    decide_route, DecideCtx, FamilyReason, FamilyRouteDecision, FamilyTable,
    DEFAULT_CONCURRENCY_CAP,
};
use super::health::{Availability, HealthRegistry};
use super::session_overrides::{extract_session_id, SessionOverrideStore, StickyPick};
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
    /// Immutable alias → member map rebuilt from config in `from_config`
    /// (auto-model-family Epic 1). Empty by default via `Router::new`.
    family_table: Arc<FamilyTable>,
    /// The active route's opt-in `family` alias. Dispatch expands
    /// `body["model"]` ONLY when it verbatim equals this alias — an
    /// un-gated alias leaks through untouched per existing pin semantics,
    /// which is also what makes route-swap rollback restore pins.
    active_family: Option<String>,
    /// Upstream name → position in `Config.upstreams` (== index into
    /// `providers` and key domain of `HealthRegistry`). Family members
    /// address upstreams by name and may name upstreams outside the route's
    /// candidate list, so resolution maps through here (falling back to a
    /// candidate-name search for test-built routers whose map is empty).
    upstream_indices: HashMap<String, usize>,
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
            UpstreamKind::Gemini { .. } => Arc::new(GeminiProvider::new(
                Arc::new(upstream.clone()),
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
            family_table: FamilyTable::empty(),
            active_family: None,
            upstream_indices: HashMap::new(),
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

    /// Swaps in a config-built [`FamilyTable`] plus the active route's
    /// opt-in family alias, replacing the empty-table default `Router::new`
    /// / `from_config` starts with. `from_config` calls this itself, so
    /// external callers only need it for test-injected tables.
    #[must_use]
    pub fn with_family_table(
        mut self,
        family_table: Arc<FamilyTable>,
        active_family: Option<String>,
    ) -> Self {
        self.family_table = family_table;
        self.active_family = active_family;
        self
    }

    /// Swaps in the upstream name → provider-index map `from_config` builds.
    /// Test-built routers skip this and resolve member upstreams through
    /// their candidate list instead (see [`upstream_index`](Self::upstream_index)).
    #[must_use]
    pub fn with_upstream_indices(mut self, upstream_indices: HashMap<String, usize>) -> Self {
        self.upstream_indices = upstream_indices;
        self
    }

    /// Provider index for an upstream name: the config-built map first, then
    /// the route's candidate list (covers test-built routers).
    fn upstream_index(&self, upstream: &str) -> Option<usize> {
        if let Some(index) = self.upstream_indices.get(upstream) {
            return Some(*index);
        }
        self.candidates
            .iter()
            .find(|c| c.name == upstream)
            .map(|c| c.index)
    }

    /// Epic 3 resolution seam: the ranked, exclusion-filtered member order
    /// for `alias`, with the `ResolutionSnapshot` + counters published as a
    /// side effect. `pub` so the perf-budget test times the real dispatch
    /// seam (not a copy of it).
    #[must_use]
    pub fn resolve_family(&self, alias: &str) -> FamilyRouteDecision {
        let health = &self.health;
        let index_of = |upstream: &str| -> Option<usize> { self.upstream_index(upstream) };
        let index_cooled = |upstream: &str| -> bool {
            self.upstream_index(upstream)
                .is_some_and(|index| !health.is_available(index))
        };
        let index_429_cooled = |upstream: &str| -> bool {
            self.upstream_index(upstream)
                .is_some_and(|index| health.is_backpressure_cooled(index))
        };
        let ctx = DecideCtx {
            runtime: &self.metrics.family,
            index_of: &index_of,
            index_cooled: &index_cooled,
            index_429_cooled: &index_429_cooled,
        };
        decide_route(&self.family_table, alias, &ctx)
    }

    /// Maps resolved members onto dispatchable candidates (provider index +
    /// per-member model override). Members with no addressable upstream are
    /// skipped with a WARN — validation guarantees mappability, so this is
    /// belt-and-braces.
    fn map_family_members(&self, ordered: &[FamilyMember], alias: &str) -> Vec<UpstreamRef> {
        ordered
            .iter()
            .filter_map(|m| {
                let Some(index) = self.upstream_index(&m.upstream) else {
                    tracing::warn!(
                        alias = %alias,
                        upstream = %m.upstream,
                        model = %m.model,
                        "family member has no addressable upstream; skipping"
                    );
                    return None;
                };
                Some(UpstreamRef {
                    index,
                    name: m.upstream.clone(),
                    weight: 1.0,
                    model: Some(m.model.clone()),
                })
            })
            .collect()
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
        };

        let admission = Arc::new(RateLimiters::new(&config.ratelimit)) as Arc<dyn AdmissionControl>;

        tracing::info!(
            route = %route.name,
            strategy = ?route.strategy,
            candidates = candidates.len(),
            "router assembled from config"
        );

        let family_table = Arc::new(FamilyTable::from_config(config));
        let active_family = route.family.clone();
        let upstream_indices: HashMap<String, usize> = config
            .upstreams
            .iter()
            .enumerate()
            .map(|(index, upstream)| (upstream.name.clone(), index))
            .collect();

        Ok(
            Router::new(candidates, providers, strategy, health, admission, metrics)
                .with_family_table(family_table, active_family)
                .with_upstream_indices(upstream_indices),
        )
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

    /// Whether `session_id`'s explicit pin is live on this route: a pin that
    /// maps into the current candidate list. Used by `dispatch` to assert
    /// pins-first order (a live pin bypasses family expansion entirely).
    fn session_pin_active(&self, session_id: Option<&str>) -> bool {
        session_id
            .and_then(|sid| self.session_overrides.get(sid))
            .is_some_and(|over| self.candidates.iter().any(|c| c.name == over.upstream))
    }

    /// Whether a stuck pick must be abandoned: the member was denylisted,
    /// personally backpressured, unmapped, or its upstream index cooled (any
    /// cause — a cooled member is not servable sticky). Shared-upstream 429
    /// nuance does not apply here: a 429 on the stuck member marks it
    /// personally (`note_backpressure`), which already invalidates above.
    fn sticky_pick_invalid(&self, stuck: &StickyPick) -> bool {
        if self
            .metrics
            .family
            .is_denylisted(&stuck.upstream, &stuck.model)
            || self
                .metrics
                .family
                .is_backpressured(&stuck.upstream, &stuck.model)
        {
            return true;
        }
        match self.upstream_index(&stuck.upstream) {
            None => true,
            Some(index) => !self.health.is_available(index),
        }
    }

    /// Sticky serve pool: the stuck member first (it was validated by the
    /// caller), then the alias's remaining table members in config order for
    /// in-request failover — minus already-excluded members, so a transient
    /// error on the stick never fails over onto a dead ID. Sibling exclusion
    /// mirrors `eligible_pool`: denylisted, personally-backpressured, and
    /// non-429-index-cooled members are out, while an unmarked sibling on a
    /// 429-cooled shared upstream stays eligible.
    fn map_sticky_pool(&self, alias: &str, stuck: &StickyPick) -> Vec<UpstreamRef> {
        let mut members = vec![FamilyMember {
            upstream: stuck.upstream.clone(),
            model: stuck.model.clone(),
        }];
        if let Some(all) = self.family_table.members(alias) {
            for m in all {
                if m.upstream == stuck.upstream && m.model == stuck.model {
                    continue;
                }
                if self.metrics.family.is_denylisted(&m.upstream, &m.model)
                    || self.metrics.family.is_backpressured(&m.upstream, &m.model)
                {
                    continue;
                }
                let index_cooled = self
                    .upstream_index(&m.upstream)
                    .is_some_and(|index| !self.health.is_available(index));
                let index_429_cooled = self
                    .upstream_index(&m.upstream)
                    .is_some_and(|index| self.health.is_backpressure_cooled(index));
                if index_cooled && !index_429_cooled {
                    continue;
                }
                members.push(m.clone());
            }
        }
        self.map_family_members(&members, alias)
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
    // Longest cohesive dispatch loop in the router; splitting the family
    // resolution step out (`resolve_family_step` extraction) is a logged
    // follow-up — allowed here until that lands, per the cmdcrush precedent.
    #[allow(clippy::too_many_lines)]
    pub async fn dispatch(
        &self,
        body: serde_json::Value,
        headers: HeaderMap,
        stream: bool,
        est_tokens: u32,
    ) -> Result<ProviderResponse, ProviderError> {
        // Attempt tracking is per-(upstream index, model): two family members
        // sharing one upstream are each addressable in a single request
        // (Story 3.1 AC3) — the second same-index member is NOT filtered by
        // the first's attempt. For non-family routes each candidate carries
        // a fixed index+model, so this reduces to the old per-index set.
        let mut already_tried: HashSet<(usize, String)> = HashSet::new();
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

        // Family route-gate (auto-model-family Epic 3): expand the alias
        // ONLY when the active route opts in via `family` AND the client
        // sent exactly that alias. Un-gated aliases and unknown aliases
        // (`UnknownAlias`) flow through untouched per existing pin semantics
        // — which is also what makes hot-swap rollback restore pins while
        // clients still send the alias.
        //
        // Resolution is the ranked Epic 3 path (`decide_route`: Epic 7's
        // paid guard inside, then rank → pre-dispatch exclusion →
        // hysteresis/probe/cap → snapshot publish, or the cooldown-scoped
        // SafetyNetBypass). The pool is iterated directly in rank order
        // (forced fallback — ranked order is meaningless under
        // `WeightedStrategy` random sampling, which validation rejects for
        // family routes).
        let mut dispatch_body = body.clone();
        let mut family_pool: Vec<UpstreamRef> = Vec::new();
        let mut family_active = false;
        let mut family_bypass = false;
        // The alias being served this request, if any — the success arm
        // uses it for the sticky failover correction below.
        let mut sticky_alias: Option<String> = None;
        // Pins-first (Epic 4 Story 4.1): a live session pin bypasses family
        // expansion entirely — a pin means "use this," not "prefer this."
        // `effective_candidates` above already narrowed to the pinned
        // single-candidate path; the gate below must not re-expand the alias.
        let pin_active = self.session_pin_active(session_id.as_deref());
        if let Some(alias) = self.active_family.as_deref() {
            let is_alias_request =
                body.get("model").and_then(serde_json::Value::as_str) == Some(alias) && !pin_active;
            if is_alias_request {
                sticky_alias = Some(alias.to_string());
                // Auto-stickiness (Epic 4 Story 4.2, STICKY-PER-SESSION): an
                // unpinned session reuses its stuck pick until the K-window
                // lapses (`served == sticky_every`) or the stuck member hits
                // a cooldown/exclusion event. Sticky serves skip
                // `resolve_family` — no snapshot/probe side effects — so an
                // exploration probe can never yank a mid-conversation session
                // off its model.
                let mut sticky_served = false;
                if let Some(sid) = session_id.as_deref() {
                    if let Some(stuck) = self.session_overrides.sticky_lookup(sid, alias) {
                        if stuck.served < self.session_overrides.sticky_every()
                            && !self.sticky_pick_invalid(&stuck)
                        {
                            // Single-lock check-and-increment: the lookup
                            // above is only a hint (window + health
                            // pre-check); `try_sticky_serve` re-checks the
                            // K-window and bumps `served` under one lock
                            // hold so concurrent requests can't overshoot K.
                            if let Some(stuck) = self.session_overrides.try_sticky_serve(sid, alias)
                            {
                                family_active = true;
                                family_pool = self.map_sticky_pool(alias, &stuck);
                                sticky_served = true;
                            }
                        }
                    }
                }
                if !sticky_served {
                    match self.resolve_family(alias) {
                        FamilyRouteDecision::UnknownAlias => {}
                        FamilyRouteDecision::Serve {
                            ordered, reason, ..
                        } => {
                            family_active = true;
                            family_pool = self.map_family_members(&ordered, alias);
                            // First family resolution for a session records
                            // the pick; probes are never stuck to (a session
                            // must not stick to a sampled member — the next
                            // request re-resolves to the real pick).
                            if reason != FamilyReason::Probe {
                                if let (Some(sid), Some(first)) =
                                    (session_id.as_deref(), ordered.first())
                                {
                                    self.session_overrides.sticky_record(
                                        sid,
                                        alias,
                                        &first.upstream,
                                        &first.model,
                                    );
                                }
                            }
                        }
                        FamilyRouteDecision::Bypass { ordered } => {
                            family_active = true;
                            family_bypass = true;
                            family_pool = self.map_family_members(&ordered, alias);
                        }
                        FamilyRouteDecision::Unavailable { all_denylisted } => {
                            return Err(if all_denylisted {
                                // The bypass must never mask 404/auth/validation:
                                // every candidate is 404-denylisted (or nothing
                                // is servable), so the validation error surfaces
                                // instead of a fabricated success.
                                ProviderError::Validation(
                                    format!(
                                        "family {alias}: all members excluded (404-denylisted)"
                                    ),
                                    404,
                                )
                            } else {
                                // All members 429-walled: error without retrying
                                // the rate-limited upstream (bypass declined).
                                ProviderError::Exhausted
                            });
                        }
                    }
                }
                if let Some(first) = family_pool.first() {
                    dispatch_body["model"] = serde_json::Value::String(
                        first.model.clone().unwrap_or_else(|| alias.to_string()),
                    );
                }
            }
        }

        loop {
            // Family requests iterate the resolved pool directly in rank
            // order. Deliberately NO `is_available` re-filter here:
            // pre-dispatch exclusion already ran in `decide_route`, and a
            // mid-request 429 trip on a shared upstream index must NOT block
            // the same-upstream sibling later in this same pool (the sibling
            // stays eligible; only the 429'd member is personally marked).
            let chosen = if family_active {
                family_pool
                    .iter()
                    .find(|u| {
                        !already_tried.contains(&(u.index, u.model.clone().unwrap_or_default()))
                    })
                    .cloned()
            } else {
                let healthy: Vec<UpstreamRef> = candidates
                    .iter()
                    .filter(|u| {
                        !already_tried.contains(&(u.index, u.model.clone().unwrap_or_default()))
                            && self.health.is_available(u.index)
                    })
                    .cloned()
                    .collect();

                self.strategy.select(&healthy)
            };
            let Some(chosen) = chosen else {
                break;
            };
            already_tried.insert((chosen.index, chosen.model.clone().unwrap_or_default()));

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
                    let mut b = dispatch_body.clone();
                    b["model"] = serde_json::Value::String(model.clone());
                    b
                }
                None => dispatch_body.clone(),
            };
            // Epic 2 dual-write key: the model ID actually sent to this
            // upstream (per-candidate override wins, else the dispatch body
            // — which carries the family-resolved ID on family routes), so
            // members sharing one upstream get separate stats buckets.
            let resolved_model: String = match &chosen.model {
                Some(m) => m.clone(),
                None => dispatch_body
                    .get("model")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(model.as_str())
                    .to_string(),
            };
            let attempt_started = std::time::Instant::now();
            // Concurrency cap (family path only, never the bypass): admit
            // one in-flight slot for this member just before sending — the
            // resolve-time partition already preferred under-cap members, and
            // this closes the race for concurrent bursts. A lost race skips
            // to the sibling instead of exceeding the cap.
            let member_slot: Option<(String, String)> = if family_active && !family_bypass {
                chosen.model.clone().map(|m| (chosen.name.clone(), m))
            } else {
                None
            };
            if let Some((slot_upstream, slot_model)) = &member_slot {
                if !self.metrics.family.try_acquire_inflight(
                    slot_upstream,
                    slot_model,
                    DEFAULT_CONCURRENCY_CAP,
                ) {
                    continue;
                }
            }
            let send_result = provider.send(request_body, headers.clone(), stream).await;
            if let Some((slot_upstream, slot_model)) = &member_slot {
                self.metrics
                    .family
                    .release_inflight(slot_upstream, slot_model);
            }
            match send_result {
                Ok(response) => {
                    self.record_attempt(
                        &chosen.name,
                        attempt_started,
                        Ok(()),
                        &model,
                        &resolved_model,
                    );
                    // Sticky failover correction: the resolve-time record
                    // pinned the pool head, but failover may have served a
                    // sibling — re-record the member that actually returned
                    // Ok so the session sticks to the working member, not
                    // the errored head. Re-record resets the K-window serve
                    // count (the new pick restarts at 1). Gated on an
                    // existing stick so exploration probes (never recorded)
                    // still never stick a session.
                    if family_active && !family_bypass {
                        if let (Some(sid), Some(stick_alias)) =
                            (session_id.as_deref(), sticky_alias.as_deref())
                        {
                            if self
                                .session_overrides
                                .sticky_lookup(sid, stick_alias)
                                .is_some()
                                && family_pool.first().is_some_and(|head| {
                                    head.name != chosen.name
                                        || head.model.clone().unwrap_or_default() != resolved_model
                                })
                            {
                                self.session_overrides.sticky_record(
                                    sid,
                                    stick_alias,
                                    &chosen.name,
                                    &resolved_model,
                                );
                            }
                        }
                    }
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
                    // Accepted limitation (Story 3.2): the first request
                    // after a fresh delist still fails here — validation
                    // returns immediately with no failover. The 404 writer in
                    // `record_attempt` feeds the denylist below, so requests
                    // N+1.. skip the dead ID pre-dispatch until the 1h TTL.
                    self.record_attempt(
                        &chosen.name,
                        attempt_started,
                        Err(&e),
                        &model,
                        &resolved_model,
                    );
                    return Err(e);
                }
                Err(e) if e.is_rate_limited() => {
                    self.record_attempt(
                        &chosen.name,
                        attempt_started,
                        Err(&e),
                        &model,
                        &resolved_model,
                    );
                    let override_duration = e.retry_after_secs().map(Duration::from_secs);
                    // 429 is backpressure, not quality: trip the shared index
                    // AND mark this member personally (same TTL on both, so
                    // index cool and personal mark agree on recovery). A
                    // same-upstream sibling with no personal mark stays
                    // eligible; the bypass never retries a 429-walled index.
                    let until = self
                        .health
                        .trip_backpressure(chosen.index, override_duration)
                        .unwrap_or_else(|| {
                            std::time::Instant::now() + self.health.default_cooldown_duration()
                        });
                    self.metrics
                        .family
                        .note_backpressure(&chosen.name, &resolved_model, until);
                    last_error = Some(e);
                }
                Err(e) if e.is_response_shape_mismatch() => {
                    // ADR-002: a 2xx body that doesn't match the documented
                    // shape won't self-heal on retry the way a rate limit
                    // does — trip cooldown immediately (first occurrence),
                    // using a longer override than the default so a
                    // permanently-broken Gemini endpoint isn't retried on
                    // every request forever.
                    self.record_attempt(
                        &chosen.name,
                        attempt_started,
                        Err(&e),
                        &model,
                        &resolved_model,
                    );
                    self.health.trip(
                        chosen.index,
                        Some(Duration::from_secs(ProviderError::DRIFT_COOLDOWN_SECS)),
                    );
                    last_error = Some(e);
                }
                Err(e) => {
                    self.record_attempt(
                        &chosen.name,
                        attempt_started,
                        Err(&e),
                        &model,
                        &resolved_model,
                    );
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

    /// Records one dispatch attempt's timing/outcome for `/metrics`
    /// (Task 3.4.5) — per-upstream request/success/error counts plus, on
    /// failure, the error-type breakdown and the deduplicated error tracker
    /// feeding `/errors/summary`. For a streaming response this measures
    /// time-to-headers only (`provider.send` returns once the stream is
    /// ready, not once it's fully consumed) — full stream duration would
    /// need a metrics-side tee analogous to `CostTrackingStream`.
    ///
    /// `model` is the request's model field (for the error tracker);
    /// `resolved_model` is the ID actually sent to this upstream, dual-
    /// written into the `FamilyRuntime` stats map keyed by
    /// `(upstream_name, resolved_model)` (auto-model-family Epic 2) — the
    /// existing per-upstream counters below are untouched by that seam.
    fn record_attempt(
        &self,
        upstream: &str,
        started: std::time::Instant,
        outcome: Result<(), &ProviderError>,
        model: &str,
        resolved_model: &str,
    ) {
        let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
        let outcome_err = outcome.as_ref().err().copied();
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
        // Epic 2 dual-write: per-(upstream, resolved model) decayed stats
        // for the family ranker. Classified per the write table inside
        // `record_member` (429/auth/validation never touch the quality
        // rate); the per-upstream counters above are byte-identical.
        //
        // Boundedness gate: only members of a configured family dual-write.
        // Non-family routes fall back to the raw client `body["model"]`
        // string for `resolved_model`, so an attacker-controlled model ID
        // would otherwise mint a fresh stats bucket per distinct value.
        // Non-members skip the write (and the denylist feed — exclusion is
        // a family-members-only concept); per-upstream counters are
        // untouched.
        if self.family_table.is_member(upstream, resolved_model) {
            let class = self.metrics.family.record_member(
                upstream,
                resolved_model,
                outcome_err,
                duration_ms,
            );
            if class == MemberRecordClass::DenylistFeed
                || matches!(outcome_err, Some(ProviderError::Validation(_, 404)))
            {
                self.metrics
                    .family
                    .denylist_insert(upstream, resolved_model);
            }
        }
    }

    /// Scope-clears `FamilyRuntime` stats/denylist entries whose
    /// `(upstream, model)` left the rebuilt [`FamilyTable`] — the single
    /// Epic 2 call on the `post_route` rebuild path (`api.rs`). The runtime
    /// itself is owned by the shared `MetricsCollector`, so all other
    /// learning survives the rebuild.
    pub fn prune_family_runtime(&self) {
        let live = self.metrics.family.member_keys();
        let retained = self.family_table.drop_stale_keys(&live);
        self.metrics.family.retain_members(&retained);
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
                family: None,
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
            family: None,
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

    // Epic 2 (Story 2.2): dispatch dual-writes per-(upstream, resolved
    // model) decayed stats alongside the untouched per-upstream counters —
    // but ONLY for members of a configured family (boundedness gate: junk
    // client model IDs on non-family routes must not mint stats buckets).
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_should_dual_write_member_stats_keyed_by_resolved_model() {
        use crate::config::schema::{FamilyMember, ModelFamily};
        use crate::routing::family::FamilyTable;

        let metrics = MetricsCollector::new();
        let config = Config {
            families: vec![ModelFamily {
                alias: "auto-coding".to_string(),
                members: vec![FamilyMember {
                    upstream: "openrouter".to_string(),
                    model: "model-a:free".to_string(),
                }],
                allow_paid: false,
            }],
            ..Config::default()
        };
        let router = Router::new(
            vec![UpstreamRef {
                index: 0,
                name: "openrouter".to_string(),
                weight: 1.0,
                model: Some("model-a:free".to_string()),
            }],
            vec![Arc::new(AlwaysOkProvider {
                name: "openrouter",
                call_count: Arc::new(AtomicU32::new(0)),
            })],
            Arc::new(FallbackStrategy),
            Arc::new(HealthRegistry::new(300)),
            Arc::new(AlwaysAllow),
            metrics.clone(),
        )
        .with_family_table(Arc::new(FamilyTable::from_config(&config)), None);

        let res = router
            .dispatch(
                serde_json::json!({"model": "auto-coding"}),
                HeaderMap::new(),
                false,
                0,
            )
            .await;
        assert!(res.is_ok());

        let view = metrics.family.member_view("openrouter", "model-a:free");
        assert_eq!(view.samples, 1);
        assert!(view.cold, "n=1 must still report cold (unknown ≠ perfect)");

        let upstream = metrics.counters.upstreams.get("openrouter").unwrap();
        assert_eq!(upstream.requests.load(Ordering::Relaxed), 1);
        assert_eq!(upstream.success.load(Ordering::Relaxed), 1);
    }

    // Boundedness gate: N dispatches with distinct junk client model IDs on
    // a non-family route must leave the MemberStats map empty (no per-junk
    // bucket is ever minted) while per-upstream counters still record every
    // attempt.
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_should_not_dual_write_member_stats_for_non_family_models() {
        let metrics = MetricsCollector::new();
        let router = Router::new(
            vec![UpstreamRef {
                index: 0,
                name: "openrouter".to_string(),
                weight: 1.0,
                model: None,
            }],
            vec![Arc::new(AlwaysOkProvider {
                name: "openrouter",
                call_count: Arc::new(AtomicU32::new(0)),
            })],
            Arc::new(FallbackStrategy),
            Arc::new(HealthRegistry::new(300)),
            Arc::new(AlwaysAllow),
            metrics.clone(),
        );

        for i in 0..25 {
            let res = router
                .dispatch(
                    serde_json::json!({"model": format!("junk-model-{i}")}),
                    HeaderMap::new(),
                    false,
                    0,
                )
                .await;
            assert!(res.is_ok());
        }

        assert!(
            metrics.family.stats.is_empty(),
            "junk client model IDs must not mint MemberStats buckets"
        );
        let upstream = metrics.counters.upstreams.get("openrouter").unwrap();
        assert_eq!(
            upstream.requests.load(Ordering::Relaxed),
            25,
            "per-upstream counters are untouched by the gate"
        );
    }

    // Sticky failover pool applies the same index-cool exclusion as
    // `eligible_pool`: a non-429-cooled sibling is excluded, while an
    // unmarked sibling on a 429-cooled shared upstream stays eligible.
    #[test]
    fn sticky_pool_should_exclude_index_cooled_sibling_but_keep_429_sibling() {
        use crate::config::schema::{FamilyMember, ModelFamily};
        use crate::routing::family::FamilyTable;

        fn sticky_router(health: Arc<HealthRegistry>) -> Router {
            let config = Config {
                families: vec![ModelFamily {
                    alias: "auto-coding".to_string(),
                    members: vec![
                        FamilyMember {
                            upstream: "mock".to_string(),
                            model: "model-a:free".to_string(),
                        },
                        FamilyMember {
                            upstream: "mock".to_string(),
                            model: "model-b:free".to_string(),
                        },
                    ],
                    allow_paid: false,
                }],
                ..Config::default()
            };
            Router::new(
                vec![upstream(0, "mock")],
                Vec::<Arc<dyn Provider>>::new(),
                Arc::new(FallbackStrategy),
                health,
                Arc::new(AlwaysAllow),
                MetricsCollector::new(),
            )
            .with_family_table(
                Arc::new(FamilyTable::from_config(&config)),
                Some("auto-coding".to_string()),
            )
        }

        let stuck = StickyPick {
            upstream: "mock".to_string(),
            model: "model-a:free".to_string(),
            served: 1,
        };

        let health = Arc::new(HealthRegistry::new(300));
        health.trip(0, None);
        let pool = sticky_router(health).map_sticky_pool("auto-coding", &stuck);
        assert_eq!(
            pool.len(),
            1,
            "index-cooled (non-429) sibling must be excluded from sticky failover"
        );
        assert_eq!(pool[0].model.as_deref(), Some("model-a:free"));

        let health = Arc::new(HealthRegistry::new(300));
        let _ = health.trip_backpressure(0, None);
        let pool = sticky_router(health).map_sticky_pool("auto-coding", &stuck);
        assert_eq!(
            pool.len(),
            2,
            "429-cooled shared-upstream sibling must stay eligible"
        );
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn dispatch_should_exclude_rate_limited_attempt_from_member_quality_rate() {
        let metrics = MetricsCollector::new();
        let router = Router::new(
            vec![UpstreamRef {
                index: 0,
                name: "openrouter".to_string(),
                weight: 1.0,
                model: Some("model-a:free".to_string()),
            }],
            vec![Arc::new(AlwaysErrProvider {
                name: "openrouter",
                error: || ProviderError::RateLimited,
                call_count: Arc::new(AtomicU32::new(0)),
            })],
            Arc::new(FallbackStrategy),
            Arc::new(HealthRegistry::new(300)),
            Arc::new(AlwaysAllow),
            metrics.clone(),
        );

        let res = router
            .dispatch(
                serde_json::json!({"model": "auto-coding"}),
                HeaderMap::new(),
                false,
                0,
            )
            .await;
        assert!(matches!(res, Err(ProviderError::RateLimited)));

        // Backpressure cools the upstream but never blames member quality.
        let view = metrics.family.member_view("openrouter", "model-a:free");
        assert_eq!(view.samples, 0);
        let upstream = metrics.counters.upstreams.get("openrouter").unwrap();
        assert_eq!(upstream.requests.load(Ordering::Relaxed), 1);
        assert_eq!(upstream.errors.load(Ordering::Relaxed), 1);
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
            remaining >= ProviderError::DRIFT_COOLDOWN_SECS - 1,
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

        let providers = build_providers(&config).await.unwrap();

        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].0, "gemini");
        assert_eq!(providers[0].1.name(), "gemini");
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

    // Epic 4 Story 4.1 (R6 unit row): a session pin beats family resolution.
    // Pins-first order is asserted in `dispatch` (`session_pin_active`
    // bypasses the family gate); this test pins `s1` to A while the ranked
    // family pick is B and proves the outgoing model is still A.
    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    #[allow(clippy::expect_used)]
    async fn session_pin_should_win_over_family_resolution_when_pin_exists() {
        use crate::config::schema::{FamilyMember, ModelFamily};
        use crate::routing::family::FamilyTable;
        use crate::routing::session_overrides::{SessionOverride, SessionOverrideStore};

        let received_a = Arc::new(std::sync::Mutex::new(None));
        let received_b = Arc::new(std::sync::Mutex::new(None));
        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(CapturingProvider {
                name: "mock-a",
                received_body: received_a.clone(),
            }),
            Arc::new(CapturingProvider {
                name: "mock-b",
                received_body: received_b.clone(),
            }),
        ];
        let metrics = MetricsCollector::new();
        // Seed the ranked pick to B: A warm-bad (25 timeouts), B warm-good.
        for _ in 0..25 {
            let _ = metrics.family.record_member(
                "mock-a",
                "model-a:free",
                Some(&ProviderError::Timeout),
                5000,
            );
            let _ = metrics
                .family
                .record_member("mock-b", "model-b:free", None, 50);
        }

        let config = Config {
            families: vec![ModelFamily {
                alias: "auto-coding".to_string(),
                members: vec![
                    FamilyMember {
                        upstream: "mock-a".to_string(),
                        model: "model-a:free".to_string(),
                    },
                    FamilyMember {
                        upstream: "mock-b".to_string(),
                        model: "model-b:free".to_string(),
                    },
                ],
                allow_paid: false,
            }],
            ..Config::default()
        };
        let table = Arc::new(FamilyTable::from_config(&config));

        let session_overrides = Arc::new(SessionOverrideStore::new());
        session_overrides.set(
            "s1".to_string(),
            SessionOverride {
                upstream: "mock-a".to_string(),
                model: Some("model-a:free".to_string()),
            },
        );
        let router = Router::new(
            vec![upstream(0, "mock-a"), upstream(1, "mock-b")],
            providers,
            Arc::new(FallbackStrategy),
            Arc::new(HealthRegistry::new(300)),
            Arc::new(AlwaysAllow),
            metrics,
        )
        .with_session_overrides(session_overrides)
        .with_family_table(table, Some("auto-coding".to_string()));

        // Control: unpinned `s2` resolves to the ranked pick B.
        let res = router
            .dispatch(
                serde_json::json!({"model": "auto-coding", "metadata": {"user_id": "s2"}}),
                HeaderMap::new(),
                false,
                0,
            )
            .await;
        assert!(res.is_ok());
        assert_eq!(
            received_b
                .lock()
                .unwrap()
                .clone()
                .expect("mock-b must serve s2")["model"],
            serde_json::json!("model-b:free")
        );

        // Pinned `s1` serves A despite the family pick being B.
        let res = router
            .dispatch(
                serde_json::json!({"model": "auto-coding", "metadata": {"user_id": "s1"}}),
                HeaderMap::new(),
                false,
                0,
            )
            .await;
        assert!(res.is_ok());
        assert_eq!(
            received_a
                .lock()
                .unwrap()
                .clone()
                .expect("mock-a must serve s1")["model"],
            serde_json::json!("model-a:free")
        );
    }
}
