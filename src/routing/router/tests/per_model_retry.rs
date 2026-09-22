//! Per-model (`OpenRouter` free-pool) 429 handling: a 429 from one
//! per-model candidate must not cool down the shared upstream index while
//! untried siblings remain, and REQ-6's all-cooling-down exhaustion case.

use super::*;

/// Two per-model candidates sharing the openrouter upstream's index 0
/// (`"a/b:free"`/`"c/d:free"`) — the free-model pool most
/// `OpenrouterScoringStrategy` dispatch tests exercise (kibitzer
/// duplicate-code).
fn two_free_model_candidates() -> Vec<UpstreamRef> {
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
    ]
}

// Per-model 429 failover: a 429 from one per-model candidate must NOT
// cool down the shared upstream index while untried siblings remain —
// a single dispatch walks the full free-model pool before giving up.
// The whole-index trip is deferred until the last sibling also fails,
// so the next dispatch still backs off (account-wide-limit case).
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
    let router = Router::new(RouterDeps {
        candidates: two_free_model_candidates(),
        providers,
        strategy: openrouter_scoring_strategy(0),
        health: Arc::clone(&health),
        admission: Arc::new(AlwaysAllow),
        metrics: MetricsCollector::new(),
    });

    let res = router
        .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
        .await;

    assert!(matches!(res, Err(ProviderError::RateLimited)));
    assert!(
        !health.is_available(0),
        "the shared upstream index must be cooling down after every per-model candidate 429s"
    );
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "both per-model candidates must be attempted before giving up on rate limiting"
    );
}

// A per-model 429 must fail over to the next sibling model in the same
// dispatch, not return immediately: first model rate-limited, second
// succeeds → Ok, and the pool stays available (no whole-index trip)
// since exhaustion never happened.
/// A `Provider` test double that rate-limits the first model it's asked
/// for (`"a/b:free"`) and succeeds for any other, recording every model
/// it was called with in order.
struct FailOnceThenOk {
    calls: Arc<std::sync::Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl Provider for FailOnceThenOk {
    fn name(&self) -> &'static str {
        "openrouter"
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
        if model == "a/b:free" {
            Err(ProviderError::RateLimited)
        } else {
            Ok(ProviderResponse::Full(serde_json::json!({"ok": true})))
        }
    }

    async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn dispatch_should_retry_sibling_free_model_after_per_model_rate_limit() {
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(FailOnceThenOk {
        calls: Arc::clone(&calls),
    })];
    let health = Arc::new(HealthRegistry::new(300));
    let router = Router::new(RouterDeps {
        candidates: two_free_model_candidates(),
        providers,
        strategy: openrouter_scoring_strategy(0),
        health: Arc::clone(&health),
        admission: Arc::new(AlwaysAllow),
        metrics: MetricsCollector::new(),
    });

    let res = router
        .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
        .await;

    assert!(
        res.is_ok(),
        "a per-model 429 must fail over to the sibling free model"
    );
    let calls = calls.lock().unwrap();
    assert_eq!(calls.len(), 2, "both free models must be attempted");
    assert!(
        health.is_available(0),
        "a recovered pool (sibling succeeded) must not be left cooling down"
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
    let mut candidates = two_free_model_candidates();
    candidates.push(UpstreamRef {
        index: 1,
        name: "paid".to_string(),
        weight: 1.0,
        model: None,
    });
    let router = Router::new(RouterDeps {
        candidates,
        providers,
        strategy: openrouter_scoring_strategy(0),
        health: Arc::clone(&health),
        admission: Arc::new(AlwaysAllow),
        metrics,
    });
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
