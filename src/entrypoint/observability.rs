//! `GET /metrics`, `GET /errors/summary`, `GET /dashboard`, `GET
//! /requests/{id}` — the legacy monitoring surface (Story 6.2 Task 6.2.5),
//! wired against the new `EntrypointState`/`Router` instead of the old ad
//! hoc proxy state.
//!
//! Known gap: `GET /requests/{id}?stage=compressed` always 404s — that
//! stage needs the `compression` module wired into dispatch, which isn't in
//! scope here. The dashboard already degrades gracefully when it 404s
//! ("no compressed snapshot — compression may have been skipped").

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json};

use super::EntrypointState;

/// `GET /metrics` — full counters/histogram/error snapshot.
// Kept `async` for signature symmetry with the other Axum handlers.
#[allow(clippy::unused_async)]
pub async fn get_metrics(State(state): State<EntrypointState>) -> impl IntoResponse {
    let mut result = state.metrics.to_metrics_json();
    result["cooldowns"] = state.dispatch_router.load().cooldown_snapshot();
    // Story 5.1.2: present only for a route whose strategy overrides
    // `observability_snapshot()` (currently just `OpenrouterScoringStrategy`)
    // — omitted entirely (not `null`) otherwise.
    if let Some(scoring) = state.dispatch_router.load().openrouter_scoring_snapshot() {
        result["openrouter_scoring"] = scoring;
    }
    Json(result)
}

/// `GET /errors/summary` — deduplicated error types, most recently seen first.
#[allow(clippy::unused_async)]
pub async fn get_errors_summary(State(state): State<EntrypointState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "errors": state.metrics.error_tracker.get_summary(20),
    }))
}

/// `GET /requests/{id}?stage=original|compressed` — the dashboard's
/// request-body inspector. `original` serves the cached pre-dispatch body;
/// `compressed` always 404s (see module docs) until the `compression`
/// module is wired into dispatch. Any other `stage` value, or the request
/// having already fallen off the 100-entry ring buffer, also 404s.
///
/// # Errors
///
/// Returns [`StatusCode::NOT_FOUND`] for `stage=compressed`, an unknown
/// request id, or one evicted from the ring buffer.
#[allow(clippy::unused_async, clippy::implicit_hasher)]
pub async fn get_request_body(
    State(state): State<EntrypointState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    match params.get("stage").map(String::as_str) {
        Some("compressed") => Err(StatusCode::NOT_FOUND),
        _ => state
            .metrics
            .get_original_body(&id)
            .map(Json)
            .ok_or(StatusCode::NOT_FOUND),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::Config;

    #[allow(clippy::unwrap_used)]
    async fn state_with_cached_body(request_id: &str, body: serde_json::Value) -> EntrypointState {
        let state = EntrypointState::build(
            &Config::default(),
            std::path::Path::new("/tmp/consolette-test"),
        )
        .await
        .unwrap();
        state
            .metrics
            .push_original_body(request_id.to_string(), body);
        state
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn original_stage_returns_the_cached_body() {
        let body = serde_json::json!({"model": "claude-sonnet-4-5", "messages": []});
        let state = state_with_cached_body("req-1", body.clone()).await;

        let result = get_request_body(
            State(state),
            Path("req-1".to_string()),
            Query(HashMap::from([(
                "stage".to_string(),
                "original".to_string(),
            )])),
        )
        .await;

        let Json(returned) = result.unwrap();
        assert_eq!(returned, body);
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn unknown_request_id_404s() {
        let state = state_with_cached_body("req-1", serde_json::json!({})).await;

        let result = get_request_body(
            State(state),
            Path("does-not-exist".to_string()),
            Query(HashMap::new()),
        )
        .await;

        assert_eq!(result.unwrap_err(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn compressed_stage_always_404s() {
        let state = state_with_cached_body("req-1", serde_json::json!({})).await;

        let result = get_request_body(
            State(state),
            Path("req-1".to_string()),
            Query(HashMap::from([(
                "stage".to_string(),
                "compressed".to_string(),
            )])),
        )
        .await;

        assert_eq!(
            result.unwrap_err(),
            StatusCode::NOT_FOUND,
            "compression isn't wired into dispatch yet"
        );
    }

    // ── REQ-11/REQ-12 (Story 1.5.1/1.5.2) — real `/metrics` cooldowns feed
    // and the ship-blocking auth-vs-cooldown classification gate. ──────────

    use crate::providers::{Provider, ProviderError, ProviderResponse};
    use crate::routing::health::HealthRegistry;
    use crate::routing::router::Router as DispatchRouter;
    use crate::routing::strategy::{FallbackStrategy, RoutingStrategy, UpstreamRef};
    use axum::http::HeaderMap;
    use std::sync::Arc;

    struct AlwaysOkProvider {
        name: &'static str,
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
            Ok(ProviderResponse::Full(serde_json::json!({"ok": true})))
        }

        async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    struct AlwaysAuthErrProvider {
        name: &'static str,
    }

    #[async_trait::async_trait]
    impl Provider for AlwaysAuthErrProvider {
        fn name(&self) -> &str {
            self.name
        }

        async fn send(
            &self,
            _body: serde_json::Value,
            _headers: HeaderMap,
            _stream: bool,
        ) -> Result<ProviderResponse, ProviderError> {
            Err(ProviderError::Auth("token expired".to_string()))
        }

        async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    /// Builds an `EntrypointState` wrapping a caller-supplied `Router` — the
    /// same pattern `messages.rs`'s `test_state_with_provider` uses — so a
    /// test can control candidates/providers/health directly instead of
    /// going through `Router::from_config`.
    #[allow(clippy::unwrap_used)]
    async fn state_with_router(
        router: DispatchRouter,
        metrics: Arc<crate::metrics::MetricsCollector>,
    ) -> EntrypointState {
        EntrypointState {
            dispatch_router: Arc::new(arc_swap::ArcSwap::from_pointee(router)),
            cost_tracker: Arc::new(
                crate::cost_metrics::tracker::CostTracker::new(
                    crate::cost_metrics::pricing::PricingTable::load_default(),
                )
                .await,
            ),
            metrics,
            server_info: Arc::new(crate::entrypoint::ServerInfo {
                port: 0,
                route_name: "test".to_string(),
                strategy: "Fallback".to_string(),
                upstreams: vec![],
            }),
            config_dir: Arc::new(std::path::PathBuf::from("/tmp/consolette-test")),
            session_overrides: Arc::new(
                crate::routing::session_overrides::SessionOverrideStore::new(),
            ),
        }
    }

    fn always_allow_admission() -> Arc<dyn crate::ratelimit::AdmissionControl> {
        Arc::new(crate::ratelimit::RateLimiters::new(
            &crate::config::schema::RateLimitConfig::default(),
        )) as Arc<dyn crate::ratelimit::AdmissionControl>
    }

    fn upstream_ref(index: usize, name: &str) -> UpstreamRef {
        UpstreamRef {
            index,
            name: name.to_string(),
            weight: 1.0,
            model: None,
        }
    }

    // REQ-11's integration test — explicit adversarial-review regression
    // requirement (Task 1.5.1d): confirms the real cooldown feed doesn't
    // cross-contaminate entries across three simultaneous candidates, not
    // just that a single upstream looks right in isolation.
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn get_metrics_cooldowns_should_have_correct_non_cross_contaminated_entries_for_anthropic_bedrock_and_gemini(
    ) {
        let health = Arc::new(HealthRegistry::new(300));
        // anthropic (index 0): left healthy.
        health.trip(1, Some(std::time::Duration::from_secs(42))); // bedrock: normal cooldown
        health.trip(
            2,
            Some(std::time::Duration::from_secs(
                crate::providers::gemini::DRIFT_COOLDOWN_SECS,
            )),
        ); // gemini: drift cooldown

        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysOkProvider { name: "anthropic" }),
            Arc::new(AlwaysOkProvider { name: "bedrock" }),
            Arc::new(AlwaysOkProvider { name: "gemini" }),
        ];
        let metrics = crate::metrics::MetricsCollector::new();
        let router = DispatchRouter::new(
            vec![
                upstream_ref(0, "anthropic"),
                upstream_ref(1, "bedrock"),
                upstream_ref(2, "gemini"),
            ],
            providers,
            Arc::new(FallbackStrategy) as Arc<dyn RoutingStrategy>,
            health,
            always_allow_admission(),
            Arc::clone(&metrics),
        );
        let state = state_with_router(router, metrics).await;

        let response = get_metrics(State(state)).await.into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body must be readable");
        let json: serde_json::Value =
            serde_json::from_slice(&body).expect("body must be valid JSON");

        assert_eq!(
            json["cooldowns"]["anthropic"],
            serde_json::json!({"cooling_down": false, "remaining_seconds": 0}),
            "anthropic must be healthy, not contaminated by bedrock/gemini's cooldowns"
        );
        assert_eq!(
            json["cooldowns"]["bedrock"]["cooling_down"],
            serde_json::json!(true)
        );
        let bedrock_remaining = json["cooldowns"]["bedrock"]["remaining_seconds"]
            .as_u64()
            .expect("remaining_seconds must be a u64");
        assert!(
            bedrock_remaining > 0 && bedrock_remaining <= 42,
            "bedrock's remaining_seconds must reflect its own 42s trip, got {bedrock_remaining}"
        );
        assert_eq!(
            json["cooldowns"]["gemini"]["cooling_down"],
            serde_json::json!(true)
        );
        let gemini_remaining = json["cooldowns"]["gemini"]["remaining_seconds"]
            .as_u64()
            .expect("remaining_seconds must be a u64");
        assert!(
            gemini_remaining > 42,
            "gemini's remaining_seconds must reflect its own drift cooldown, \
             not bedrock's 42s value, got {gemini_remaining}"
        );
    }

    // REQ-12's ship-blocking test (validation.md: "do not mark Story 1.5.2
    // done without this test green") — the exact regression this epic
    // exists to prevent: `Router::dispatch`'s `is_auth()` arm never calls
    // `health.trip(...)`, so a real Gemini auth failure must show
    // `last_error_kind == "auth"` AND `cooling_down == false`
    // *simultaneously* in `/metrics` — never falling through to a plain
    // `status-active`/`status-cooldown` read.
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn get_metrics_should_classify_as_status_auth_required_eligible_when_a_real_gemini_auth_failure_is_induced(
    ) {
        let health = Arc::new(HealthRegistry::new(300));
        let providers: Vec<Arc<dyn Provider>> =
            vec![Arc::new(AlwaysAuthErrProvider { name: "gemini" })];
        let metrics = crate::metrics::MetricsCollector::new();
        let router = DispatchRouter::new(
            vec![upstream_ref(0, "gemini")],
            providers,
            Arc::new(FallbackStrategy) as Arc<dyn RoutingStrategy>,
            health,
            always_allow_admission(),
            Arc::clone(&metrics),
        );

        // Induce the real auth failure through `Router::dispatch`, exactly
        // as a live expired-Antigravity-token request would.
        let dispatch_result = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;
        assert!(
            matches!(dispatch_result, Err(ProviderError::Auth(_))),
            "expected the auth failure to propagate immediately, no failover"
        );

        let state = state_with_router(router, metrics).await;
        let response = get_metrics(State(state)).await.into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body must be readable");
        let json: serde_json::Value =
            serde_json::from_slice(&body).expect("body must be valid JSON");

        assert_eq!(
            json["providers"]["gemini"]["last_error_kind"],
            serde_json::json!("auth")
        );
        assert_eq!(
            json["cooldowns"]["gemini"]["cooling_down"],
            serde_json::json!(false),
            "is_auth() must never trip HealthRegistry — this is the exact gap Story 1.5.2 \
             exists to guard the JS against"
        );
    }

    // ── REQ-7 (Story 5.1.2, Task 5.1.2c) — `openrouter_scoring` merge. ──────

    // *Given* an active `FallbackStrategy` route, *when* `GET /metrics` is
    // called, *then* the response has no `openrouter_scoring` key at all
    // (not `null`).
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn get_metrics_should_omit_openrouter_scoring_key_for_fallback_strategy() {
        let health = Arc::new(HealthRegistry::new(300));
        let providers: Vec<Arc<dyn Provider>> =
            vec![Arc::new(AlwaysOkProvider { name: "primary" })];
        let metrics = crate::metrics::MetricsCollector::new();
        let router = DispatchRouter::new(
            vec![upstream_ref(0, "primary")],
            providers,
            Arc::new(FallbackStrategy) as Arc<dyn RoutingStrategy>,
            health,
            always_allow_admission(),
            Arc::clone(&metrics),
        );
        let state = state_with_router(router, metrics).await;

        let response = get_metrics(State(state)).await.into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body must be readable");
        let json: serde_json::Value =
            serde_json::from_slice(&body).expect("body must be valid JSON");

        assert!(
            json.as_object()
                .expect("response must be a JSON object")
                .get("openrouter_scoring")
                .is_none(),
            "openrouter_scoring key must be entirely absent for a FallbackStrategy route, got: {json}"
        );
    }

    // *Given* an active `OpenrouterScoringStrategy` route, *when* `GET
    // /metrics` is called, *then* the response's `openrouter_scoring` key
    // matches `observability_snapshot()`'s output.
    #[tokio::test]
    #[allow(clippy::expect_used, clippy::unwrap_used)]
    async fn get_metrics_should_include_openrouter_scoring_block_for_scored_route() {
        use crate::providers::openrouter::cache::ModelListCache;
        use crate::routing::openrouter_scoring::OpenrouterScoringStrategy;

        let health = Arc::new(HealthRegistry::new(300));
        let model_cache = Arc::new(ModelListCache::new_with_ttl(
            std::time::Duration::from_mins(15),
        ));
        let strategy = Arc::new(OpenrouterScoringStrategy::new(Arc::clone(&model_cache), 0));
        let providers: Vec<Arc<dyn Provider>> =
            vec![Arc::new(AlwaysOkProvider { name: "openrouter" })];
        let metrics = crate::metrics::MetricsCollector::new();
        let router = DispatchRouter::new(
            vec![UpstreamRef {
                index: 0,
                name: "openrouter".to_string(),
                weight: 1.0,
                model: Some("a/b:free".to_string()),
            }],
            providers,
            Arc::clone(&strategy) as Arc<dyn RoutingStrategy>,
            health,
            always_allow_admission(),
            Arc::clone(&metrics),
        );

        // Drive one real selection so `last_scores` (and therefore the
        // `models` block) isn't empty.
        router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await
            .expect("dispatch against AlwaysOkProvider must succeed");

        let expected = strategy
            .observability_snapshot()
            .expect("OpenrouterScoringStrategy must always return Some");

        let state = state_with_router(router, metrics).await;
        let response = get_metrics(State(state)).await.into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body must be readable");
        let json: serde_json::Value =
            serde_json::from_slice(&body).expect("body must be valid JSON");

        assert_eq!(
            json["openrouter_scoring"]["models"]["a/b:free"],
            expected["models"]["a/b:free"]
        );
        assert_eq!(
            json["openrouter_scoring"]["cache"]["cached_model_count"],
            expected["cache"]["cached_model_count"]
        );
    }
}
