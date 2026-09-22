//! Basic `dispatch` failover/cooldown/admission/weighted-selection behavior.

use super::*;
use crate::routing::strategy::WeightedStrategy;

fn fallback_router(providers: Vec<Arc<dyn Provider>>, health: Arc<HealthRegistry>) -> Router {
    Router::new(RouterDeps {
        candidates: vec![upstream(0, "primary"), upstream(1, "fallback")],
        providers,
        strategy: Arc::new(FallbackStrategy),
        health,
        admission: Arc::new(AlwaysAllow),
        metrics: MetricsCollector::new(),
    })
}

/// Two `AlwaysOkProvider`s named "primary"/"fallback" plus their
/// independent call counters — the `providers` most primary/fallback
/// failover tests need (kibitzer duplicate-code: this exact
/// construction was repeated verbatim across 6 tests).
fn two_ok_providers() -> (Vec<Arc<dyn Provider>>, Arc<AtomicU32>, Arc<AtomicU32>) {
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
    (providers, primary_calls, fallback_calls)
}

/// An `AlwaysErrProvider` named "primary" (failing with `error`) plus an
/// `AlwaysOkProvider` named "fallback", with their independent call
/// counters — the single-failure-then-healthy-fallback setup shared by
/// `dispatch`'s failover tests (kibitzer duplicate-code).
fn err_then_ok_providers(
    error: fn() -> ProviderError,
) -> (Vec<Arc<dyn Provider>>, Arc<AtomicU32>, Arc<AtomicU32>) {
    let primary_calls = Arc::new(AtomicU32::new(0));
    let fallback_calls = Arc::new(AtomicU32::new(0));
    let providers: Vec<Arc<dyn Provider>> = vec![
        Arc::new(AlwaysErrProvider {
            name: "primary",
            error,
            call_count: primary_calls.clone(),
        }),
        Arc::new(AlwaysOkProvider {
            name: "fallback",
            call_count: fallback_calls.clone(),
        }),
    ];
    (providers, primary_calls, fallback_calls)
}

#[tokio::test]
async fn normal_routes_to_first_healthy() {
    let (providers, primary_calls, fallback_calls) = two_ok_providers();
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
    let (providers, primary_calls, fallback_calls) =
        err_then_ok_providers(|| ProviderError::RateLimited);
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
    let (providers, primary_calls, fallback_calls) =
        err_then_ok_providers(|| ProviderError::Validation("bad field".into(), 400));
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
    let health = Arc::new(HealthRegistry::new(300));
    health.trip(0, None);
    let (providers, primary_calls, fallback_calls) = two_ok_providers();
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
    let health = Arc::new(HealthRegistry::new(300));
    health.trip(0, Some(Duration::from_millis(1)));
    std::thread::sleep(Duration::from_millis(20));
    let (providers, primary_calls, fallback_calls) = two_ok_providers();
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
    let (providers, _, _) = two_ok_providers();
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
    let (providers, primary_calls, fallback_calls) = two_ok_providers();
    let health = Arc::new(HealthRegistry::new(300));
    let router = Router::new(RouterDeps {
        candidates: vec![upstream(0, "primary"), upstream(1, "fallback")],
        providers,
        strategy: Arc::new(FallbackStrategy),
        health: health.clone(),
        admission: Arc::new(ShedFor {
            upstream: "primary",
        }),
        metrics: MetricsCollector::new(),
    });

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
    let (providers, _, _) = two_ok_providers();
    let router = Router::new(RouterDeps {
        candidates: vec![upstream(0, "primary"), upstream(1, "fallback")],
        providers,
        strategy: Arc::new(FallbackStrategy),
        health,
        admission: Arc::new(AlwaysShed),
        metrics: MetricsCollector::new(),
    });

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

/// A `WeightedStrategy` router over two candidates ("a" weight 0.7,
/// "b" weight 0.3) with "a" already cooled down, plus their independent
/// call counters.
fn weighted_router_with_a_cooled_down() -> (Router, Arc<AtomicU32>, Arc<AtomicU32>) {
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
    let router = Router::new(RouterDeps {
        candidates: vec![
            UpstreamRef {
                index: 0,
                name: "a".to_string(),
                weight: 0.7,
                model: None,
                model_family: None,
            },
            UpstreamRef {
                index: 1,
                name: "b".to_string(),
                weight: 0.3,
                model: None,
                model_family: None,
            },
        ],
        providers,
        strategy: Arc::new(WeightedStrategy),
        health,
        admission: Arc::new(AlwaysAllow),
        metrics: MetricsCollector::new(),
    });
    (router, a_calls, b_calls)
}

#[tokio::test]
async fn weighted_never_selects_a_cooled_down_upstream() {
    let (router, a_calls, b_calls) = weighted_router_with_a_cooled_down();

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

// ── model_family propagation (openai-model-resolution Epic 1.2/1.3) ──

#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn dispatch_should_inject_internal_model_family_body_key_when_chosen_candidate_has_model_family(
) {
    let received_body = Arc::new(std::sync::Mutex::new(None));
    let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(CapturingProvider {
        name: "family",
        received_body: received_body.clone(),
    })];
    let router = Router::new(RouterDeps {
        candidates: vec![UpstreamRef {
            index: 0,
            name: "family".to_string(),
            weight: 1.0,
            model: None,
            model_family: Some("gpt-5".to_string()),
        }],
        providers,
        strategy: Arc::new(FallbackStrategy),
        health: Arc::new(HealthRegistry::new(300)),
        admission: Arc::new(AlwaysAllow),
        metrics: MetricsCollector::new(),
    });

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
    assert_eq!(
        body[crate::providers::openai::MODEL_FAMILY_BODY_KEY],
        serde_json::json!("gpt-5")
    );
}

#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn dispatch_should_leave_request_body_unchanged_when_chosen_candidate_has_no_model_family() {
    let received_body = Arc::new(std::sync::Mutex::new(None));
    let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(CapturingProvider {
        name: "no-family",
        received_body: received_body.clone(),
    })];
    let router = Router::new(RouterDeps {
        candidates: vec![upstream(0, "no-family")],
        providers,
        strategy: Arc::new(FallbackStrategy),
        health: Arc::new(HealthRegistry::new(300)),
        admission: Arc::new(AlwaysAllow),
        metrics: MetricsCollector::new(),
    });

    let sent = serde_json::json!({"model": "claude-sonnet-4-5"});
    let res = router
        .dispatch(sent.clone(), HeaderMap::new(), false, 0)
        .await;

    assert!(res.is_ok());
    let body = received_body
        .lock()
        .unwrap()
        .clone()
        .expect("provider must have been called");
    assert_eq!(body, sent);
    assert!(body
        .get(crate::providers::openai::MODEL_FAMILY_BODY_KEY)
        .is_none());
}

// ── Story 2.3.4 (openai-model-resolution plan.md): resolution exhaustion
// must never trip `HealthRegistry` state beyond what an ordinary
// `Upstream{..}` error already does today (pitfalls.md's "the single most
// important thing to guard"). This test is a verification gate, not a
// feature — plan.md: "This test failing blocks Phase 2 completion." ──

// *Given* a route with a single upstream whose provider returns the same
// `ProviderError::Upstream{status: 0, ..}` shape `OpenaiProvider`'s
// resolution walk returns on exhaustion (Epic 2.3's `walk_candidates`/Story
// 2.3.2b), *when* `Router::dispatch` handles it, *then* `HealthRegistry`'s
// per-upstream state (`is_available`, `remaining_secs`) is unchanged from
// before the dispatch — the catch-all `Err(e) => { .. }` arm in `dispatch`
// (no `health.trip()` call) already treats it exactly like any other
// `Upstream{..}` error, with no special-casing for resolution exhaustion.
#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn dispatch_should_leave_health_registry_state_unchanged_when_model_family_resolution_exhausts(
) {
    let call_count = Arc::new(AtomicU32::new(0));
    let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(AlwaysErrProvider {
        name: "openai",
        error: || ProviderError::Upstream {
            status: 0,
            body: "all candidates in family \"gpt-5\" exhausted".to_string(),
        },
        call_count: Arc::clone(&call_count),
    })];
    let health = Arc::new(HealthRegistry::new(300));
    let router = Router::new(RouterDeps {
        candidates: vec![UpstreamRef {
            index: 0,
            name: "openai".to_string(),
            weight: 1.0,
            model: None,
            model_family: Some("gpt-5".to_string()),
        }],
        providers,
        strategy: Arc::new(FallbackStrategy),
        health: Arc::clone(&health),
        admission: Arc::new(AlwaysAllow),
        metrics: MetricsCollector::new(),
    });

    let before_available = health.is_available(0);
    let before_remaining = health.remaining_secs(0);

    let res = router
        .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
        .await;

    assert!(
        matches!(res, Err(ProviderError::Upstream { status: 0, .. })),
        "expected the resolution-exhaustion-shaped Upstream error to propagate"
    );
    assert_eq!(call_count.load(Ordering::SeqCst), 1);
    assert_eq!(
        health.is_available(0),
        before_available,
        "resolution exhaustion must not change HealthRegistry availability"
    );
    assert_eq!(
        health.remaining_secs(0),
        before_remaining,
        "resolution exhaustion must not trip a cooldown"
    );
    assert!(
        health.is_available(0),
        "an ordinary Upstream{{..}} error must never trip cooldown on its own"
    );
}
