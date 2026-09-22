//! Shared test-only fixtures for entrypoint handler tests (`messages`,
//! `chat_completions`, `observability`) — avoids re-deriving the same
//! ~15-field `EntrypointState` construction in every test module.

use std::sync::Arc;

use crate::metrics::MetricsCollector;
use crate::routing::router::Router as DispatchRouter;

use super::EntrypointState;

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
    }
}
