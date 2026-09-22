//! `RouteUpstreamRef.model` pinning: the request body's `model` field gets
//! overridden per-candidate, and a per-model failure doesn't poison a
//! sibling model pinned at the same upstream index.

use super::*;

#[tokio::test]
#[allow(clippy::unwrap_used, clippy::expect_used)]
async fn dispatch_overrides_model_field_when_upstream_pins_one() {
    let received_body = Arc::new(std::sync::Mutex::new(None));
    let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(CapturingProvider {
        name: "pinned",
        received_body: received_body.clone(),
    })];
    let metrics = MetricsCollector::new();
    let candidates = vec![UpstreamRef {
        index: 0,
        name: "pinned".to_string(),
        weight: 1.0,
        model: Some("gpt-5.1-codex-max".to_string()),
    }];
    let router = fallback_router_with_metrics(candidates, providers, &metrics);

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
    let router = Router::new(RouterDeps {
        candidates: vec![upstream(0, "unpinned")],
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
    assert_eq!(body["model"], serde_json::json!("claude-sonnet-4-5"));
}

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
/// A `FallbackStrategy` router whose real `ModelAwareProvider` lives at
/// index 2 behind two unused placeholder upstreams (0, 1) — proving the
/// per-model retry isn't accidentally keyed off index 0.
fn router_with_model_aware_provider_at_index_2(
    provider: Arc<dyn Provider>,
    fail_model: &'static str,
) -> Router {
    Router::new(RouterDeps {
        candidates: vec![
            UpstreamRef {
                index: 2,
                name: "openrouter".to_string(),
                weight: 1.0,
                model: Some(fail_model.to_string()),
            },
            UpstreamRef {
                index: 2,
                name: "openrouter".to_string(),
                weight: 1.0,
                model: Some("c/d:free".to_string()),
            },
        ],
        providers: vec![
            Arc::new(AlwaysOkProvider {
                name: "unused-0",
                call_count: Arc::new(AtomicU32::new(0)),
            }),
            Arc::new(AlwaysOkProvider {
                name: "unused-1",
                call_count: Arc::new(AtomicU32::new(0)),
            }),
            provider,
        ],
        strategy: Arc::new(FallbackStrategy),
        health: Arc::new(HealthRegistry::new(300)),
        admission: Arc::new(AlwaysAllow),
        metrics: MetricsCollector::new(),
    })
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn dispatch_should_retry_different_model_after_one_model_failure() {
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = Arc::new(ModelAwareProvider {
        name: "openrouter",
        fail_model: "a/b:free",
        calls: calls.clone(),
    });
    let router = router_with_model_aware_provider_at_index_2(provider, "a/b:free");

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
/// A `FallbackStrategy` router whose real `ModelAwareProvider` lives at
/// index 3, behind 3 unused placeholder upstreams (0-2) — proving the
/// per-model retry isn't accidentally keyed off index 0, at a
/// different index/name/model-pair than
/// `router_with_model_aware_provider_at_index_2`.
fn router_with_model_aware_provider_at_index_3(
    provider: Arc<dyn Provider>,
    name: &str,
    model_a: &str,
    model_b: &str,
) -> Router {
    let providers: Vec<Arc<dyn Provider>> = vec![
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
        provider,
    ];
    Router::new(RouterDeps {
        candidates: vec![
            UpstreamRef {
                index: 3,
                name: name.to_string(),
                weight: 1.0,
                model: Some(model_a.to_string()),
            },
            UpstreamRef {
                index: 3,
                name: name.to_string(),
                weight: 1.0,
                model: Some(model_b.to_string()),
            },
        ],
        providers,
        strategy: Arc::new(FallbackStrategy),
        health: Arc::new(HealthRegistry::new(300)),
        admission: Arc::new(AlwaysAllow),
        metrics: MetricsCollector::new(),
    })
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn dispatch_should_not_poison_sibling_model_pin_at_same_index_for_fallback_strategy() {
    let calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = Arc::new(ModelAwareProvider {
        name: "primary",
        fail_model: "model-a",
        calls: calls.clone(),
    });
    let router =
        router_with_model_aware_provider_at_index_3(provider, "primary", "model-a", "model-b");

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
