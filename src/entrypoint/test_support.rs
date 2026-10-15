//! Shared test-only fixtures for entrypoint handler tests (`messages`,
//! `chat_completions`, `observability`) — avoids re-deriving the same
//! ~15-field `EntrypointState` construction in every test module.

use std::sync::Arc;

use crate::metrics::MetricsCollector;
use crate::routing::router::Router as DispatchRouter;

use super::EntrypointState;

/// A fresh, disabled-by-default `ResponseCache` backed by a leaked
/// `TempDir`.
///
/// Leaked deliberately: a bare `&tempfile::tempdir().unwrap().path()...`
/// inline expression is dropped (deleting the directory) at the end of
/// *that* statement, not at the end of the test — a real bug this exact
/// helper replaces, since `ResponseCache::open` needs its backing directory
/// to outlive every request made against the `EntrypointState` it's
/// installed into.
#[allow(clippy::unwrap_used)]
pub(crate) fn leaked_response_cache(
    config: crate::memory::cache::ResponseCacheConfig,
) -> Arc<crate::memory::cache::ResponseCache> {
    let dir: &'static tempfile::TempDir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    Arc::new(
        crate::memory::cache::ResponseCache::open(
            &dir.path().join("response-cache.sqlite"),
            config,
        )
        .unwrap(),
    )
}

/// Builds an `EntrypointState` wrapping a caller-supplied `Router` so a
/// test can control candidates/providers/health directly instead of going
/// through `Router::from_config`.
#[allow(clippy::unwrap_used)]
pub(crate) async fn state_with_router(
    router: DispatchRouter,
    metrics: Arc<MetricsCollector>,
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
        session_overrides: Arc::new(crate::routing::session_overrides::SessionOverrideStore::new()),
        capability: crate::routing::capability::CapabilityCache::new(
            std::time::Duration::from_secs(crate::routing::capability::EVAL_TTL_SECS),
        ),
        server_tools: Arc::new(crate::server_tools::ServerToolsRuntime::default()),
        search_pool: Arc::new(crate::server_tools::McpSearchPool::new(
            crate::server_tools::ServerToolsConfig::default().pool_config(),
        )),
        pruning_policy_store: Arc::new(
            crate::claude_code_session::prune_policy::PruningPolicyStore::default(),
        ),
        omission_cache: Arc::new(
            crate::claude_code_session::omission_cache::OmissionCache::open(
                &tempfile::tempdir().unwrap().path().join("cache.sqlite"),
            )
            .unwrap(),
        ),
        response_cache: leaked_response_cache(crate::memory::cache::ResponseCacheConfig::default()),
        event_tx: tokio::sync::broadcast::channel(1024).0,
        web_ui: crate::config::schema::WebUiMode::Angular,
        compat_snake_case: false,
        config_lock: Arc::new(tokio::sync::Mutex::new(())),
        shutdown: tokio_util::sync::CancellationToken::new(),
        catalog: crate::routing::model_catalog::ModelCatalog::new(
            crate::config::schema::ModelCatalogConfig::default(),
            None,
        ),
    }
}

/// Upstream double for cost-attribution tests: optionally reports a
/// family-resolved model, then answers 1000 input / 200 output tokens
/// (Full JSON or a complete Anthropic SSE stream).
pub(crate) struct CostProbeProvider {
    pub note_resolved: bool,
}

#[async_trait::async_trait]
impl crate::providers::Provider for CostProbeProvider {
    fn name(&self) -> &str {
        "probe"
    }

    async fn send(
        &self,
        _body: serde_json::Value,
        _headers: http::HeaderMap,
        stream: bool,
    ) -> Result<crate::providers::ProviderResponse, crate::providers::ProviderError> {
        use crate::providers::ProviderResponse;
        if self.note_resolved {
            crate::providers::note_resolved_model(COST_PROBE_ROUTED_MODEL);
        }
        let usage = serde_json::json!({"input_tokens": 1000, "output_tokens": 200});
        if stream {
            let frames = [
                "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"m\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"x\",\"usage\":{\"input_tokens\":1,\"output_tokens\":0}}}\n\n".to_string(),
                format!("event: message_delta\ndata: {}\n\n", serde_json::json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":usage})),
                "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_string(),
            ];
            let s = futures_util::stream::iter(
                frames
                    .into_iter()
                    .map(|f| Ok::<_, anyhow::Error>(bytes::Bytes::from(f))),
            );
            return Ok(ProviderResponse::Stream(Box::pin(s)));
        }
        Ok(ProviderResponse::Full(serde_json::json!({
            "id": "msg_1", "type": "message", "role": "assistant", "model": "unknown",
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": usage
        })))
    }

    async fn list_models(
        &self,
    ) -> Result<Vec<crate::providers::ModelInfo>, crate::providers::ProviderError> {
        Ok(Vec::new())
    }
}

/// The routed model the probe reports/pins; present in the default pricing table.
pub(crate) const COST_PROBE_ROUTED_MODEL: &str = "gpt-6.1-sol";

/// A state whose single route is a pin (`pinned == true`: `model` set, provider reports
/// nothing) or a `model_family` (provider reports the resolved model).
pub(crate) async fn cost_probe_state(pinned: bool) -> EntrypointState {
    use crate::routing::health::HealthRegistry;
    use crate::routing::strategy::{FallbackStrategy, UpstreamRef};

    let metrics = MetricsCollector::new();
    let router = DispatchRouter::new(crate::routing::router::RouterDeps {
        candidates: vec![UpstreamRef {
            index: 0,
            name: "probe".to_string(),
            weight: 1.0,
            model: pinned.then(|| COST_PROBE_ROUTED_MODEL.to_string()),
            model_family: (!pinned).then(|| "gpt-6".to_string()),
        }],
        providers: vec![Arc::new(CostProbeProvider {
            note_resolved: !pinned,
        })],
        strategy: Arc::new(FallbackStrategy) as Arc<dyn crate::routing::strategy::RoutingStrategy>,
        health: Arc::new(HealthRegistry::new(300)),
        admission: Arc::new(crate::ratelimit::RateLimiters::new(
            &crate::config::schema::RateLimitConfig::default(),
        )) as Arc<dyn crate::ratelimit::AdmissionControl>,
        metrics: Arc::clone(&metrics),
    });
    state_with_router(router, metrics).await
}

/// Asserts the one cost row was priced by [`COST_PROBE_ROUTED_MODEL`]
/// (the requested model in these tests is absent from the pricing table).
/// 1000 in + 200 out at 2e-6 / 1e-5.
#[allow(clippy::unwrap_used)]
pub(crate) async fn assert_cost_priced_by_routed_model(state: &EntrypointState) {
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let rows = state.cost_tracker.snapshot_records().await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let cost = rows[0].cost.unwrap().0; // None means the row was never priced
    assert!((cost - 0.004).abs() < 1e-9, "got {cost}");
}
