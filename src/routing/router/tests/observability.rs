//! `/metrics`-facing observability: per-upstream counters, error
//! classification, drift-cooldown, the recent-requests ring buffer, and
//! `RequestDetail.selected_model`/`selected_model_was_exploration`.

use super::*;

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
    let candidates = vec![upstream(0, "primary"), upstream(1, "fallback")];
    let router = fallback_router_with_metrics(candidates, providers, &metrics);

    let res = router
        .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
        .await;
    assert!(res.is_ok());

    assert_primary_failed_and_fallback_succeeded(&metrics);
    assert_rate_limit_error_was_classified(&metrics);
}

/// The "primary" upstream recorded exactly one failed attempt and
/// "fallback" recorded exactly one successful one.
#[allow(clippy::unwrap_used)]
fn assert_primary_failed_and_fallback_succeeded(metrics: &MetricsCollector) {
    let primary = metrics.counters.upstreams.get("primary").unwrap();
    assert_eq!(primary.requests.load(Ordering::Relaxed), 1);
    assert_eq!(primary.errors.load(Ordering::Relaxed), 1);
    drop(primary);

    let fallback = metrics.counters.upstreams.get("fallback").unwrap();
    assert_eq!(fallback.requests.load(Ordering::Relaxed), 1);
    assert_eq!(fallback.success.load(Ordering::Relaxed), 1);
    drop(fallback);
}

/// The dashboard's rate-limit counter and error tracker both saw the
/// primary's `RateLimited` failure.
fn assert_rate_limit_error_was_classified(metrics: &MetricsCollector) {
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
    let router = Router::new(RouterDeps {
        candidates: vec![upstream(0, "gemini"), upstream(1, "anthropic")],
        providers,
        strategy: Arc::new(FallbackStrategy),
        health: Arc::clone(&health),
        admission: Arc::new(AlwaysAllow),
        metrics,
    });

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

/// A `FallbackStrategy` router over gemini (errors with a response-shape
/// mismatch)/anthropic/bedrock, each with an independent call counter.
fn gemini_drift_cooldown_router() -> (Router, Arc<AtomicU32>, Arc<AtomicU32>, Arc<AtomicU32>) {
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
    let candidates = vec![
        upstream(0, "gemini"),
        upstream(1, "anthropic"),
        upstream(2, "bedrock"),
    ];
    let router = fallback_router_with_metrics(candidates, providers, &MetricsCollector::new());
    (router, gemini_calls, anthropic_calls, bedrock_calls)
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn dispatch_should_leave_anthropic_and_bedrock_dispatch_unaffected_when_gemini_trips_drift_cooldown(
) {
    let (router, gemini_calls, anthropic_calls, bedrock_calls) = gemini_drift_cooldown_router();

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
async fn dispatch_should_return_last_response_shape_mismatch_error_when_all_candidates_exhausted() {
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
    let router = Router::new(RouterDeps {
        candidates: vec![upstream(0, "gemini"), upstream(1, "gemini-2")],
        providers,
        strategy: Arc::new(FallbackStrategy),
        health: Arc::new(HealthRegistry::new(300)),
        admission: Arc::new(AlwaysAllow),
        metrics,
    });

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
    let metrics = MetricsCollector::new();
    let router = single_ok_provider_fallback_router("primary", metrics.clone());

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
    let candidates = vec![upstream(0, "gemini")];
    let router = fallback_router_with_metrics(candidates, providers, &metrics);

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

/// A `FallbackStrategy` router over "anthropic"/"gemini", with "gemini"
/// (index 1) already tripped for ~15 minutes.
fn router_with_gemini_tripped_for_15_mins() -> Router {
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
    Router::new(RouterDeps {
        candidates: vec![upstream(0, "anthropic"), upstream(1, "gemini")],
        providers,
        strategy: Arc::new(FallbackStrategy),
        health,
        admission: Arc::new(AlwaysAllow),
        metrics: MetricsCollector::new(),
    })
}

#[tokio::test]
#[allow(clippy::expect_used)]
async fn cooldown_snapshot_should_report_real_remaining_seconds_for_a_tripped_candidate() {
    let snapshot = router_with_gemini_tripped_for_15_mins().cooldown_snapshot();

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
    let router = single_ok_provider_fallback_router("anthropic", MetricsCollector::new());

    let snapshot = router.cooldown_snapshot();

    assert_eq!(
        snapshot["anthropic"],
        serde_json::json!({"cooling_down": false, "remaining_seconds": 0})
    );
}
