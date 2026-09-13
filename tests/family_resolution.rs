//! Route-gate + hot-swap rollback + Epic 3 resolution coverage
//! (auto-model-family).
//!
//! The dispatch gate lives in `Router::dispatch`: the `auto-coding` alias
//! expands to a family member ONLY when the active route opts in via
//! `family = "auto-coding"`. Epic 3 resolves through `decide_route` —
//! ranked pick (lowest error, then lowest latency), pre-dispatch denylist
//! exclusion, hysteresis, probe-every-25th, concurrency-cap overflow, and
//! the cooldown-scoped `SafetyNetBypass` — with per-(upstream, model) attempt
//! tracking so same-upstream members each fail over inside one request.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use http::HeaderMap;
use serde_json::json;

use consolette::config::schema::{Config, FamilyMember, ModelFamily};
use consolette::metrics::MetricsCollector;
use consolette::providers::{ModelInfo, Provider, ProviderError, ProviderResponse};
use consolette::ratelimit::{AdmissionControl, Admit};
use consolette::routing::family::{is_verifiably_free, FamilyTable, DEFAULT_CONCURRENCY_CAP};
use consolette::routing::health::{Availability, HealthRegistry};
use consolette::routing::router::Router;
use consolette::routing::strategy::{FallbackStrategy, UpstreamRef, WeightedStrategy};

struct CapturingProvider {
    received_body: Arc<std::sync::Mutex<Option<serde_json::Value>>>,
}

#[async_trait::async_trait]
impl Provider for CapturingProvider {
    fn name(&self) -> &'static str {
        "mock"
    }

    async fn send(
        &self,
        body: serde_json::Value,
        _headers: HeaderMap,
        _stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        *self.received_body.lock().unwrap() = Some(body);
        Ok(ProviderResponse::Full(json!({"ok": true})))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
}

struct AlwaysAllow;

#[async_trait::async_trait]
impl AdmissionControl for AlwaysAllow {
    async fn admit(&self, _upstream: &str, _est_tokens: u32) -> Admit {
        Admit::Allowed
    }
}

fn family_config() -> Config {
    Config {
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
    }
}

fn mock_router(
    model_pin: Option<&str>,
    table: Arc<FamilyTable>,
    active_family: Option<String>,
) -> (Router, Arc<std::sync::Mutex<Option<serde_json::Value>>>) {
    let received_body = Arc::new(std::sync::Mutex::new(None));
    let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(CapturingProvider {
        received_body: received_body.clone(),
    })];
    let router = Router::new(
        vec![UpstreamRef {
            index: 0,
            name: "mock".to_string(),
            weight: 1.0,
            model: model_pin.map(str::to_string),
        }],
        providers,
        Arc::new(FallbackStrategy),
        Arc::new(HealthRegistry::new(300)),
        Arc::new(AlwaysAllow),
        MetricsCollector::new(),
    )
    .with_family_table(table, active_family);
    (router, received_body)
}

fn seen_model(received_body: &Arc<std::sync::Mutex<Option<serde_json::Value>>>) -> String {
    received_body
        .lock()
        .unwrap()
        .clone()
        .expect("provider must have been called")["model"]
        .as_str()
        .expect("model must be a string")
        .to_string()
}

#[tokio::test]
async fn family_route_gate_should_leave_alias_untouched_when_route_has_no_family_field() {
    // Active route has no `family` field: the alias leaks verbatim per
    // existing (unpinned) forwarding semantics — no expansion happens.
    let (router, received_body) = mock_router(None, FamilyTable::empty(), None);

    let res = router
        .dispatch(json!({"model": "auto-coding"}), HeaderMap::new(), false, 0)
        .await;

    assert!(res.is_ok());
    assert_eq!(seen_model(&received_body), "auto-coding");
}

#[tokio::test]
async fn family_route_gate_should_resolve_alias_when_route_names_family() {
    // Active route opts in via `family = "auto-coding"`: the alias
    // resolves to the stub's first member; a non-alias model is untouched.
    let table = Arc::new(FamilyTable::from_config(&family_config()));
    let (router, received_body) = mock_router(None, table, Some("auto-coding".to_string()));

    let res = router
        .dispatch(json!({"model": "auto-coding"}), HeaderMap::new(), false, 0)
        .await;
    assert!(res.is_ok());
    assert_eq!(seen_model(&received_body), "model-a:free");

    let res = router
        .dispatch(
            json!({"model": "some-other-model"}),
            HeaderMap::new(),
            false,
            0,
        )
        .await;
    assert!(res.is_ok());
    assert_eq!(seen_model(&received_body), "some-other-model");
}

#[tokio::test]
async fn family_hot_swap_should_restore_pins_when_route_swapped_back() {
    // Rollback path: family route active → alias resolves; hot-swap back
    // to a pinned route (fresh `Router`, as `post_route` rebuilds) →
    // the same alias body carries the pin again.
    let table = Arc::new(FamilyTable::from_config(&family_config()));
    let (family_router, family_seen) = mock_router(None, table, Some("auto-coding".to_string()));
    family_router
        .dispatch(json!({"model": "auto-coding"}), HeaderMap::new(), false, 0)
        .await
        .expect("family dispatch must succeed");
    assert_eq!(seen_model(&family_seen), "model-a:free");

    let (pinned_router, pinned_seen) =
        mock_router(Some("pinned-model"), FamilyTable::empty(), None);
    pinned_router
        .dispatch(json!({"model": "auto-coding"}), HeaderMap::new(), false, 0)
        .await
        .expect("pinned dispatch must succeed");
    assert_eq!(
        seen_model(&pinned_seen),
        "pinned-model",
        "swapping back to a pinned route must restore pins even while the client still sends the alias"
    );
}

#[tokio::test]
async fn family_table_should_rebuild_from_config_on_hot_swap() {
    // The table half of the `post_route` rebuild: `Router::from_config`
    // builds it from the reloaded config (families ride conf.d, not
    // `RuntimeOverrides`). Unknown aliases resolve to `None` so dispatch
    // leaves the body untouched.
    let table = FamilyTable::from_config(&family_config());
    assert_eq!(
        table.resolve_model("auto-coding").as_deref(),
        Some("model-a:free")
    );
    assert_eq!(table.resolve_model("no-such-alias"), None);
}

// ────────────────────────────────────────────────────────────────────────────
// Epic 3 helpers: model-routed mock upstreams
// ────────────────────────────────────────────────────────────────────────────

/// One mock upstream whose per-model behavior the test scripts: models
/// absent from `fail_for` succeed; scripted models fail with the factory's
/// error. Every attempt is recorded (call count + model IDs in order).
struct MockUpstream {
    calls: AtomicU32,
    seen_models: Mutex<Vec<String>>,
    fail_for: Mutex<HashMap<String, Arc<dyn Fn() -> ProviderError + Send + Sync>>>,
}

impl MockUpstream {
    fn new(_name: &str) -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicU32::new(0),
            seen_models: Mutex::new(Vec::new()),
            fail_for: Mutex::new(HashMap::new()),
        })
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }

    fn seen_models(&self) -> Vec<String> {
        self.seen_models.lock().unwrap().clone()
    }

    fn fail_model(&self, model: &str, err: impl Fn() -> ProviderError + Send + Sync + 'static) {
        self.fail_for
            .lock()
            .unwrap()
            .insert(model.to_string(), Arc::new(err));
    }
}

#[async_trait::async_trait]
impl Provider for MockUpstream {
    fn name(&self) -> &'static str {
        // `name` borrows `self` but the trait wants `&'static str`; the mock
        // is only ever addressed by the router's candidate names, so the
        // static label below is never read in these tests.
        "mock-upstream"
    }

    async fn send(
        &self,
        body: serde_json::Value,
        _headers: HeaderMap,
        _stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        let model = body
            .get("model")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string();
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen_models.lock().unwrap().push(model.clone());
        if let Some(factory) = self.fail_for.lock().unwrap().get(&model) {
            return Err(factory());
        }
        Ok(ProviderResponse::Full(json!({"ok": true})))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
}

fn table_for(members: Vec<(&str, &str)>, alias: &str) -> Arc<FamilyTable> {
    let config = Config {
        families: vec![ModelFamily {
            alias: alias.to_string(),
            members: members
                .into_iter()
                .map(|(upstream, model)| FamilyMember {
                    upstream: upstream.to_string(),
                    model: model.to_string(),
                })
                .collect(),
            allow_paid: false,
        }],
        ..Config::default()
    };
    Arc::new(FamilyTable::from_config(&config))
}

#[allow(clippy::too_many_arguments)]
fn family_router_with(
    mocks: &[Arc<MockUpstream>],
    names: &[&str],
    table: Arc<FamilyTable>,
    active_family: Option<String>,
    metrics: Arc<MetricsCollector>,
    health: Arc<HealthRegistry>,
    weighted: bool,
) -> Router {
    let candidates = names
        .iter()
        .enumerate()
        .map(|(index, name)| UpstreamRef {
            index,
            name: (*name).to_string(),
            weight: 1.0,
            model: None,
        })
        .collect();
    let providers: Vec<Arc<dyn Provider>> = mocks
        .iter()
        .map(|m| m.clone() as Arc<dyn Provider>)
        .collect();
    let strategy: Arc<dyn consolette::routing::strategy::RoutingStrategy> = if weighted {
        Arc::new(WeightedStrategy)
    } else {
        Arc::new(FallbackStrategy)
    };
    Router::new(
        candidates,
        providers,
        strategy,
        health,
        Arc::new(AlwaysAllow),
        metrics,
    )
    .with_family_table(table, active_family)
}

fn two_mock_router(
    table: Arc<FamilyTable>,
    metrics: Arc<MetricsCollector>,
    health: Arc<HealthRegistry>,
) -> (Router, Arc<MockUpstream>, Arc<MockUpstream>) {
    let mock_a = MockUpstream::new("mock-a");
    let mock_b = MockUpstream::new("mock-b");
    let router = family_router_with(
        &[mock_a.clone(), mock_b.clone()],
        &["mock-a", "mock-b"],
        table,
        Some("auto-coding".to_string()),
        metrics,
        health,
        false,
    );
    (router, mock_a, mock_b)
}

fn seed_ok(metrics: &MetricsCollector, upstream: &str, model: &str, n: usize, latency_ms: u64) {
    for _ in 0..n {
        let _ = metrics
            .family
            .record_member(upstream, model, None, latency_ms);
    }
}

fn seed_timeout(
    metrics: &MetricsCollector,
    upstream: &str,
    model: &str,
    n: usize,
    latency_ms: u64,
) {
    let err = ProviderError::Timeout;
    for _ in 0..n {
        let _ = metrics
            .family
            .record_member(upstream, model, Some(&err), latency_ms);
    }
}

async fn dispatch_alias(router: &Router) -> Result<ProviderResponse, ProviderError> {
    router
        .dispatch(json!({"model": "auto-coding"}), HeaderMap::new(), false, 0)
        .await
}

// ────────────────────────────────────────────────────────────────────────────
// Story 3.1: ranked resolution + same-upstream failover
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn dispatch_should_route_to_healthiest_member_when_family_alias_received() {
    // A at 12.5% err (4 timeouts + 28 ok, n=32), B at 0% (30 ok, n=30):
    // the alias resolves to B and a ResolutionSnapshot is published.
    let metrics = MetricsCollector::new();
    let health = Arc::new(HealthRegistry::new(300));
    let table = table_for(
        vec![("mock-a", "model-a:free"), ("mock-b", "model-b:free")],
        "auto-coding",
    );
    let (router, mock_a, mock_b) = two_mock_router(table, metrics.clone(), health);

    seed_timeout(&metrics, "mock-a", "model-a:free", 4, 100);
    seed_ok(&metrics, "mock-a", "model-a:free", 28, 100);
    seed_ok(&metrics, "mock-b", "model-b:free", 30, 90);

    dispatch_alias(&router)
        .await
        .expect("family dispatch must succeed");
    assert_eq!(mock_a.calls(), 0);
    assert_eq!(mock_b.calls(), 1);
    assert_eq!(mock_b.seen_models(), vec!["model-b:free".to_string()]);

    let snapshot = metrics
        .family
        .snapshot("auto-coding")
        .expect("a ResolutionSnapshot must be published");
    assert_eq!(snapshot.picked.model, "model-b:free");
    assert_eq!(metrics.family.counter_snapshot("auto-coding"), (1, 0));
    assert_eq!(metrics.family.paid_resolutions("auto-coding"), 0);
}

#[tokio::test]
async fn dispatch_should_fail_over_to_same_upstream_member_when_first_member_errors() {
    // Story 3.1 AC3: both members share upstream `mock`; A fails
    // transiently mid-request, so B is tried IN THE SAME request —
    // attempt-tracking is per-(upstream, model), not per upstream index.
    let metrics = MetricsCollector::new();
    let health = Arc::new(HealthRegistry::new(300));
    let table = table_for(
        vec![("mock", "model-a:free"), ("mock", "model-b:free")],
        "auto-coding",
    );
    let mock = MockUpstream::new("mock");
    mock.fail_model("model-a:free", || ProviderError::Timeout);
    let router = family_router_with(
        std::slice::from_ref(&mock),
        &["mock"],
        table,
        Some("auto-coding".to_string()),
        metrics,
        health,
        false,
    );

    dispatch_alias(&router).await.expect("must fail over to B");
    assert_eq!(
        mock.seen_models(),
        vec!["model-a:free".to_string(), "model-b:free".to_string()]
    );
    assert_eq!(mock.calls(), 2);
}

#[tokio::test]
async fn dispatch_should_serve_ranked_order_when_family_route_uses_weighted() {
    // Story 3.1 AC4, forced-fallback half (validation rejects weighted
    // family routes at load; a directly-constructed weighted router still
    // serves ranked order because alias requests never consult the
    // strategy's random sampling).
    let metrics = MetricsCollector::new();
    let health = Arc::new(HealthRegistry::new(300));
    let table = table_for(
        vec![("mock-a", "model-a:free"), ("mock-b", "model-b:free")],
        "auto-coding",
    );
    let mock_a = MockUpstream::new("mock-a");
    let mock_b = MockUpstream::new("mock-b");
    let router = family_router_with(
        &[mock_a.clone(), mock_b.clone()],
        &["mock-a", "mock-b"],
        table,
        Some("auto-coding".to_string()),
        metrics.clone(),
        health,
        true,
    );

    seed_timeout(&metrics, "mock-a", "model-a:free", 10, 100);
    seed_ok(&metrics, "mock-a", "model-a:free", 22, 100);
    seed_ok(&metrics, "mock-b", "model-b:free", 30, 90);

    dispatch_alias(&router)
        .await
        .expect("family dispatch must succeed");
    assert_eq!(mock_a.calls(), 0, "ranked order must win, not a dice roll");
    assert_eq!(mock_b.calls(), 1);
}

// ────────────────────────────────────────────────────────────────────────────
// Story 3.2: denylist, 404-vs-400, bypass, shared-upstream-429
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn dispatch_should_fail_first_request_then_denylist_when_member_returns_404() {
    // Accepted limitation: the first request after a fresh delist still
    // fails (404 → Validation returns immediately, no failover) — then
    // feeds the denylist, so the next request skips the dead ID.
    let metrics = MetricsCollector::new();
    let health = Arc::new(HealthRegistry::new(300));
    let table = table_for(
        vec![("mock-a", "model-a:free"), ("mock-b", "model-b:free")],
        "auto-coding",
    );
    let (router, mock_a, mock_b) = two_mock_router(table, metrics.clone(), health);
    mock_a.fail_model("model-a:free", || {
        ProviderError::Validation("model not found".to_string(), 404)
    });

    let first = dispatch_alias(&router).await;
    assert!(
        matches!(first, Err(ProviderError::Validation(_, 404))),
        "first request must surface the 404"
    );
    assert_eq!(mock_a.calls(), 1);
    assert_eq!(mock_b.calls(), 0);
    assert!(
        metrics.family.is_denylisted("mock-a", "model-a:free"),
        "the 404 must feed the denylist"
    );

    dispatch_alias(&router)
        .await
        .expect("second request must skip the dead ID");
    assert_eq!(
        mock_a.calls(),
        1,
        "dead ID must not be retried pre-dispatch"
    );
    assert_eq!(mock_b.calls(), 1);
    assert_eq!(mock_b.seen_models(), vec!["model-b:free".to_string()]);
}

#[tokio::test]
async fn dispatch_should_skip_denylisted_member_before_dispatch_when_404_seen() {
    // Pre-dispatch exclusion: a denylisted ID is never sent to.
    let metrics = MetricsCollector::new();
    let health = Arc::new(HealthRegistry::new(300));
    let table = table_for(
        vec![("mock-a", "model-a:free"), ("mock-b", "model-b:free")],
        "auto-coding",
    );
    let (router, mock_a, mock_b) = two_mock_router(table, metrics.clone(), health);
    metrics.family.denylist_insert("mock-a", "model-a:free");

    dispatch_alias(&router)
        .await
        .expect("must serve the live member");
    assert_eq!(
        mock_a.calls(),
        0,
        "denylisted ID must be skipped before dispatch"
    );
    assert_eq!(mock_b.calls(), 1);
}

#[tokio::test]
async fn dispatch_should_never_quarantine_member_when_validation_is_400() {
    // 404-vs-400 discrimination: a client-caused 400 never quarantines —
    // the member stays eligible on the next request.
    let metrics = MetricsCollector::new();
    let health = Arc::new(HealthRegistry::new(300));
    let table = table_for(
        vec![("mock-a", "model-a:free"), ("mock-b", "model-b:free")],
        "auto-coding",
    );
    let (router, mock_a, mock_b) = two_mock_router(table, metrics.clone(), health);
    mock_a.fail_model("model-a:free", || {
        ProviderError::Validation("bad field".to_string(), 400)
    });

    assert!(dispatch_alias(&router).await.is_err());
    assert!(
        !metrics.family.is_denylisted("mock-a", "model-a:free"),
        "a 400 must never feed the denylist"
    );
    assert!(dispatch_alias(&router).await.is_err());
    assert_eq!(mock_a.calls(), 2, "the member stays eligible after a 400");
    assert_eq!(mock_b.calls(), 0);
}

#[tokio::test]
async fn dispatch_should_bypass_cooldown_when_all_members_cooled() {
    // Cooldown/empty-pool scope: every index cooled (non-429 cause) →
    // SafetyNetBypass serves least-bad + WARN + fallback counter.
    let metrics = MetricsCollector::new();
    let health = Arc::new(HealthRegistry::new(300));
    let table = table_for(
        vec![("mock-a", "model-a:free"), ("mock-b", "model-b:free")],
        "auto-coding",
    );
    let (router, mock_a, mock_b) = two_mock_router(table, metrics.clone(), health.clone());
    health.trip(0, None);
    health.trip(1, None);

    dispatch_alias(&router)
        .await
        .expect("bypass must serve least-bad");
    assert_eq!(mock_a.calls(), 1);
    assert_eq!(mock_b.calls(), 0);
    assert_eq!(
        metrics.family.counter_snapshot("auto-coding"),
        (1, 1),
        "bypass must bump fallback_to_default_total"
    );
}

#[tokio::test]
async fn dispatch_should_still_error_when_all_members_denylisted() {
    // 404-all-down: the bypass must NOT fabricate a success — the
    // validation error surfaces and no upstream is touched.
    let metrics = MetricsCollector::new();
    let health = Arc::new(HealthRegistry::new(300));
    let table = table_for(
        vec![("mock-a", "model-a:free"), ("mock-b", "model-b:free")],
        "auto-coding",
    );
    let (router, mock_a, mock_b) = two_mock_router(table, metrics.clone(), health);
    metrics.family.denylist_insert("mock-a", "model-a:free");
    metrics.family.denylist_insert("mock-b", "model-b:free");

    let res = dispatch_alias(&router).await;
    assert!(
        matches!(res, Err(ProviderError::Validation(_, 404))),
        "all-denylisted must surface a validation error"
    );
    assert_eq!(mock_a.calls(), 0);
    assert_eq!(mock_b.calls(), 0);
}

#[tokio::test]
async fn dispatch_should_never_escalate_to_paid_when_free_members_all_down() {
    // Free-never-paid: all free members cooled → bypass serves a
    // verifiably-free ID and the paid audit counter stays 0.
    let metrics = MetricsCollector::new();
    let health = Arc::new(HealthRegistry::new(300));
    let table = table_for(
        vec![("mock-a", "model-a:free"), ("mock-b", "model-b:free")],
        "auto-coding",
    );
    let (router, mock_a, _) = two_mock_router(table, metrics.clone(), health.clone());
    health.trip(0, None);
    health.trip(1, None);

    dispatch_alias(&router)
        .await
        .expect("bypass must serve least-bad free");
    assert_eq!(mock_a.calls(), 1);
    let served = mock_a.seen_models();
    assert_eq!(served, vec!["model-a:free".to_string()]);
    assert!(is_verifiably_free(&served[0]));
    assert_eq!(
        metrics.family.paid_resolutions("auto-coding"),
        0,
        "free alias must never accrue paid resolutions"
    );
    assert_eq!(metrics.family.counter_snapshot("auto-coding").1, 1);
}

#[tokio::test]
async fn dispatch_should_keep_sibling_eligible_when_shared_upstream_returns_429() {
    // Shared-upstream-429 gate: A 429s on upstream `mock` (cooling the
    // shared index), yet sibling B on the SAME upstream is still tried in
    // the same request — the cooldown excludes the index, the denylist
    // excludes the member, and B is neither.
    let metrics = MetricsCollector::new();
    let health = Arc::new(HealthRegistry::new(300));
    let table = table_for(
        vec![("mock", "model-a:free"), ("mock", "model-b:free")],
        "auto-coding",
    );
    let mock = MockUpstream::new("mock");
    mock.fail_model("model-a:free", || ProviderError::RateLimited);
    let router = family_router_with(
        std::slice::from_ref(&mock),
        &["mock"],
        table,
        Some("auto-coding".to_string()),
        metrics.clone(),
        health.clone(),
        false,
    );

    dispatch_alias(&router).await.expect("sibling B must serve");
    assert_eq!(
        mock.seen_models(),
        vec!["model-a:free".to_string(), "model-b:free".to_string()]
    );
    assert_eq!(
        mock.calls(),
        2,
        "A must be tried exactly once (no hammer retry)"
    );
    assert!(
        !health.is_available(0),
        "the shared index must be cooling (proves B served DESPITE the index cool)"
    );
    assert!(
        !metrics.family.is_denylisted("mock", "model-b:free"),
        "a 429 must never denylist the sibling"
    );
    assert!(
        metrics.family.is_backpressured("mock", "model-a:free"),
        "the 429'd member itself carries the personal backpressure mark"
    );
}

#[tokio::test]
async fn dispatch_should_error_without_hammering_when_all_members_rate_limited() {
    // Bypass never overrides 429s: both members 429 → error, each member
    // tried exactly once (no retry hammer on the rate-limited upstream).
    let metrics = MetricsCollector::new();
    let health = Arc::new(HealthRegistry::new(300));
    let table = table_for(
        vec![("mock", "model-a:free"), ("mock", "model-b:free")],
        "auto-coding",
    );
    let mock = MockUpstream::new("mock");
    mock.fail_model("model-a:free", || ProviderError::RateLimited);
    mock.fail_model("model-b:free", || ProviderError::RateLimited);
    let router = family_router_with(
        std::slice::from_ref(&mock),
        &["mock"],
        table,
        Some("auto-coding".to_string()),
        metrics.clone(),
        health,
        false,
    );

    let res = dispatch_alias(&router).await;
    assert!(res.is_err(), "all-429 must error");
    assert_eq!(mock.calls(), 2, "each member tried exactly once");
    assert_eq!(metrics.family.counter_snapshot("auto-coding").1, 0);
}

// ────────────────────────────────────────────────────────────────────────────
// Story 3.3: probe + concurrency cap
// ────────────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn dispatch_should_probe_non_pick_member_when_probe_request_due() {
    // Probe-every-25th: A is the stable pick (30 warm successes, B cold),
    // so over 100 requests the 25th/50th/75th/100th sample B — ≥3 land on
    // the non-pick member (each tagged reason=probe in logs).
    let metrics = MetricsCollector::new();
    let health = Arc::new(HealthRegistry::new(300));
    let table = table_for(
        vec![("mock-a", "model-a:free"), ("mock-b", "model-b:free")],
        "auto-coding",
    );
    let (router, mock_a, mock_b) = two_mock_router(table, metrics.clone(), health);

    seed_ok(&metrics, "mock-a", "model-a:free", 30, 100);

    for _ in 0..100 {
        dispatch_alias(&router)
            .await
            .expect("probe traffic must succeed");
    }
    let probed = mock_b.calls();
    assert!(
        probed >= 3,
        "≥3 of 100 requests must probe the non-pick member, got {probed}"
    );
    assert_eq!(
        mock_a.calls() + probed,
        100,
        "every request serves exactly one member"
    );
    assert_eq!(metrics.family.resolutions_seen("auto-coding"), 100);
}

#[tokio::test]
async fn dispatch_should_route_overflow_to_sibling_when_pick_at_concurrency_cap() {
    // Per-member cap (default 4): with the pick's slots full, the burst
    // overflows to the sibling instead of herding onto the pick.
    let metrics = MetricsCollector::new();
    let health = Arc::new(HealthRegistry::new(300));
    let table = table_for(
        vec![("mock-a", "model-a:free"), ("mock-b", "model-b:free")],
        "auto-coding",
    );
    let (router, mock_a, mock_b) = two_mock_router(table, metrics.clone(), health);

    for _ in 0..DEFAULT_CONCURRENCY_CAP {
        assert!(metrics.family.try_acquire_inflight(
            "mock-a",
            "model-a:free",
            DEFAULT_CONCURRENCY_CAP
        ));
    }
    assert!(!metrics.family.try_acquire_inflight(
        "mock-a",
        "model-a:free",
        DEFAULT_CONCURRENCY_CAP
    ));

    for _ in 0..10 {
        dispatch_alias(&router).await.expect("overflow must serve");
    }
    assert_eq!(mock_a.calls(), 0, "at-cap pick must be skipped");
    assert_eq!(mock_b.calls(), 10, "overflow routes to the sibling");
    assert_eq!(metrics.family.inflight_count("mock-b", "model-b:free"), 0);
}
