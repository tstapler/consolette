//! `RoutingStrategy` wiring: `record_outcome` hooks and `expand_candidates`
//! ordering relative to the health filter.

use super::*;

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
    let router = Router::new(RouterDeps {
        candidates: vec![upstream(0, "primary")],
        providers,
        strategy: Arc::new(RecordingStrategy {
            outcomes: outcomes.clone(),
        }),
        health: Arc::new(HealthRegistry::new(300)),
        admission: Arc::new(AlwaysAllow),
        metrics: MetricsCollector::new(),
    });

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
    let router = Router::new(RouterDeps {
        candidates: vec![upstream(0, "openrouter")],
        providers,
        strategy: Arc::new(ExpandingStrategy),
        health: Arc::new(HealthRegistry::new(300)),
        admission: Arc::new(AlwaysAllow),
        metrics: MetricsCollector::new(),
    });

    let res = router
        .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
        .await;

    assert!(res.is_ok());
    let body = received_body.lock().unwrap().clone().unwrap();
    // `select` (via `FallbackStrategy`-style "first healthy") picks the
    // first of the 2 expanded candidates.
    assert_eq!(body["model"], serde_json::json!("model-a"));
}
