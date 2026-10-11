//! Characterization of `Router::dispatch` as it behaved before the
//! `TraceScope` refactor (Story 1.1.3, Task 1.1.3i; pre-mortem P1-2).
//!
//! Written and green against the unmodified router, committed alone, and
//! never edited in the refactor PR: a changed expectation needs its own
//! reviewed commit with a reason. Each scenario records the provider the
//! ring-buffer entry names, `selected_model`/`selected_model_was_exploration`,
//! the number of upstream calls, the `hold_retries` count, the `Ok`/`Err`
//! variant and the `last_error` kind.
//!
//! `hold_retries` is not exposed, so it is derived from wall-clock time spent
//! in `hold_for_rate_limit_cooldown`: each hold sleeps the cooldown's floored
//! whole seconds (2 s for the 2050 ms cooldown used here) plus jitter.

use super::*;

/// What a single `dispatch` call visibly did.
#[derive(Debug, PartialEq, Eq)]
struct Observed {
    ring_entries: usize,
    ring_provider: Option<String>,
    selected_model: Option<String>,
    was_exploration: Option<bool>,
    upstream_calls: u32,
    hold_retries: u64,
    /// `"ok"` or `"err:<kind_label>"`.
    result: String,
}

struct Counted {
    ok: Arc<AtomicU32>,
}

fn counted() -> Counted {
    Counted {
        ok: Arc::new(AtomicU32::new(0)),
    }
}

fn ok_provider(name: &'static str, calls: &Counted) -> Arc<dyn Provider> {
    Arc::new(AlwaysOkProvider {
        name,
        call_count: Arc::clone(&calls.ok),
    })
}

fn err_provider(
    name: &'static str,
    error: fn() -> ProviderError,
    calls: &Counted,
) -> Arc<dyn Provider> {
    Arc::new(AlwaysErrProvider {
        name,
        error,
        call_count: Arc::clone(&calls.ok),
    })
}

async fn observe(
    router: &Router,
    metrics: &Arc<MetricsCollector>,
    body: serde_json::Value,
    calls: &Counted,
) -> Observed {
    let started = std::time::Instant::now();
    let res = router.dispatch(body, HeaderMap::new(), false, 0).await;
    let elapsed = started.elapsed();
    let recent = metrics.get_recent_requests(10);
    let entry = recent.first();
    Observed {
        ring_entries: recent.len(),
        ring_provider: entry.map(|e| e.provider.clone()).filter(|p| !p.is_empty()),
        selected_model: entry.and_then(|e| e.selected_model.clone()),
        was_exploration: entry.and_then(|e| e.selected_model_was_exploration),
        upstream_calls: calls.ok.load(Ordering::SeqCst),
        hold_retries: elapsed.as_secs() / 2,
        result: match res {
            Ok(_) => "ok".to_string(),
            Err(e) => format!("err:{}", e.kind_label()),
        },
    }
}

fn observed(
    ring_provider: Option<&str>,
    upstream_calls: u32,
    hold_retries: u64,
    result: &str,
) -> Observed {
    Observed {
        ring_entries: 1,
        ring_provider: ring_provider.map(str::to_string),
        selected_model: None,
        was_exploration: None,
        upstream_calls,
        hold_retries,
        result: result.to_string(),
    }
}

async fn all_candidates_ok() -> Observed {
    let calls = counted();
    let metrics = MetricsCollector::new();
    let router = fallback_router_with_metrics(
        vec![upstream(0, "a"), upstream(1, "b")],
        vec![ok_provider("a", &calls), ok_provider("b", &calls)],
        &metrics,
    );
    observe(&router, &metrics, serde_json::json!({}), &calls).await
}

async fn first_upstream_529_then_second_ok() -> Observed {
    let calls = counted();
    let metrics = MetricsCollector::new();
    let router = fallback_router_with_metrics(
        vec![upstream(0, "a"), upstream(1, "b")],
        vec![
            err_provider(
                "a",
                || ProviderError::Upstream {
                    status: 529,
                    body: "overloaded".to_string(),
                },
                &calls,
            ),
            ok_provider("b", &calls),
        ],
        &metrics,
    );
    observe(&router, &metrics, serde_json::json!({}), &calls).await
}

async fn all_upstreams_rate_limited() -> Observed {
    let calls = counted();
    let metrics = MetricsCollector::new();
    // 300 s cooldown after the first 429 is above MAX_HOLD_WAIT_SECS, so no hold.
    let router = fallback_router_with_metrics(
        vec![upstream(0, "a"), upstream(1, "b")],
        vec![
            err_provider("a", || ProviderError::RateLimited, &calls),
            err_provider("b", || ProviderError::RateLimited, &calls),
        ],
        &metrics,
    );
    observe(&router, &metrics, serde_json::json!({}), &calls).await
}

async fn rate_limit_hold_then_success() -> Observed {
    let calls = counted();
    let metrics = MetricsCollector::new();
    let health = Arc::new(HealthRegistry::new(300));
    health.trip(0, Some(Duration::from_millis(2050)));
    let router = Router::new(RouterDeps {
        candidates: vec![upstream(0, "only")],
        providers: vec![ok_provider("only", &calls)],
        strategy: Arc::new(FallbackStrategy),
        health,
        admission: Arc::new(AlwaysAllow),
        metrics: Arc::clone(&metrics),
    });
    observe(&router, &metrics, serde_json::json!({}), &calls).await
}

async fn validation_error() -> Observed {
    let calls = counted();
    let metrics = MetricsCollector::new();
    let router = fallback_router_with_metrics(
        vec![upstream(0, "a"), upstream(1, "b")],
        vec![
            err_provider(
                "a",
                || ProviderError::Validation("bad".to_string(), 400),
                &calls,
            ),
            ok_provider("b", &calls),
        ],
        &metrics,
    );
    observe(&router, &metrics, serde_json::json!({}), &calls).await
}

async fn session_pinned_route() -> Observed {
    let calls = counted();
    let metrics = MetricsCollector::new();
    let overrides = Arc::new(SessionOverrideStore::new());
    overrides.set(
        "sess-1".to_string(),
        crate::routing::session_overrides::SessionOverride {
            upstream: "b".to_string(),
            model: None,
        },
    );
    let router = fallback_router_with_metrics(
        vec![upstream(0, "a"), upstream(1, "b")],
        vec![ok_provider("a", &calls), ok_provider("b", &calls)],
        &metrics,
    )
    .with_session_overrides(overrides);
    let body = serde_json::json!({"metadata": {"user_id": "sess-1"}});
    observe(&router, &metrics, body, &calls).await
}

/// Always selects the first healthy candidate and reports it as an
/// exploration pick, so the flag is deterministic.
struct AlwaysExploringStrategy;

impl RoutingStrategy for AlwaysExploringStrategy {
    fn select(&self, healthy: &[UpstreamRef]) -> Option<UpstreamRef> {
        healthy.first().cloned()
    }

    fn last_selection_was_exploration(&self, _model_id: &str) -> Option<bool> {
        Some(true)
    }
}

async fn exploration_pick() -> Observed {
    let calls = counted();
    let metrics = MetricsCollector::new();
    let router = Router::new(RouterDeps {
        candidates: vec![UpstreamRef {
            model: Some("m/x:free".to_string()),
            ..upstream(0, "openrouter")
        }],
        providers: vec![ok_provider("openrouter", &calls)],
        strategy: Arc::new(AlwaysExploringStrategy),
        health: Arc::new(HealthRegistry::new(300)),
        admission: Arc::new(AlwaysAllow),
        metrics: Arc::clone(&metrics),
    });
    observe(&router, &metrics, serde_json::json!({}), &calls).await
}

async fn local_catalog_model() -> Observed {
    let calls = counted();
    let metrics = MetricsCollector::new();
    let catalog = crate::providers::local_discovery::LocalCatalog::new();
    catalog.set(
        crate::providers::local_discovery::LocalBackend::Ollama,
        Some(crate::providers::local_discovery::Endpoint {
            // Port 1 refuses connections: the local path fails without a network.
            base_url: "http://127.0.0.1:1".to_string(),
            models: vec!["m".to_string()],
        }),
    );
    let router = fallback_router_with_metrics(
        vec![upstream(0, "a")],
        vec![ok_provider("a", &calls)],
        &metrics,
    )
    .with_local_catalog(catalog);
    let body = serde_json::json!({"model": "local/ollama/m"});
    observe(&router, &metrics, body, &calls).await
}

#[tokio::test]
async fn dispatch_characterization_should_match_recorded_provider_selected_model_retries_hold_and_result_for_eight_scenarios(
) {
    assert_eq!(
        all_candidates_ok().await,
        observed(Some("a"), 1, 0, "ok"),
        "all candidates ok"
    );
    assert_eq!(
        first_upstream_529_then_second_ok().await,
        observed(Some("b"), 2, 0, "ok"),
        "first upstream 529 then second ok"
    );
    assert_eq!(
        all_upstreams_rate_limited().await,
        observed(None, 2, 0, "err:rate_limited"),
        "all upstreams rate limited"
    );
    assert_eq!(
        rate_limit_hold_then_success().await,
        observed(Some("only"), 1, 1, "ok"),
        "rate-limit hold then success"
    );
    assert_eq!(
        validation_error().await,
        observed(None, 1, 0, "err:validation"),
        "validation error"
    );
    assert_eq!(
        session_pinned_route().await,
        observed(Some("b"), 1, 0, "ok"),
        "session-pinned route"
    );
    assert_eq!(
        exploration_pick().await,
        Observed {
            selected_model: Some("m/x:free".to_string()),
            was_exploration: Some(true),
            ..observed(Some("openrouter"), 1, 0, "ok")
        },
        "exploration pick"
    );
    assert_eq!(
        local_catalog_model().await,
        Observed {
            ring_entries: 1,
            ..observed(None, 0, 0, "err:upstream")
        },
        "local-catalog model is traced (Story 2.1.6) but bypasses the fallback providers"
    );
}
