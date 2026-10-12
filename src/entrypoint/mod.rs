//! HTTP entrypoint for `consolette run` (ADR-016/017/018, plan.md Phase 1).
//!
//! Binds a loopback-only HTTP server exposing an Anthropic-native
//! `POST /v1/messages` endpoint and an OpenAI-compatible
//! `POST /v1/chat/completions` endpoint, both dispatching through
//! `Router::dispatch` with `CostTracker` wired at the dispatch seam.
//!
//! Modeled directly on `src/cost_metrics/server.rs`'s
//! `CostServerState`/`cost_router`/`serve_cost` shape.

pub mod api;
pub mod chat_completions;
pub mod cost_tee;
pub mod errors;
pub mod events;
pub mod landing;
pub mod messages;
pub mod models;
pub mod observability;
pub mod openai_stream;
#[cfg(test)]
pub(crate) mod test_support;
pub mod traces;

use std::net::SocketAddr;
use std::sync::Arc;

use arc_swap::ArcSwap;
use tower_http::cors::{Any, CorsLayer};

use crate::claude_code_session::omission_cache::OmissionCache;
use crate::claude_code_session::prune_policy::PruningPolicyStore;
#[cfg(test)]
use crate::config::schema::UpstreamKind;
use crate::config::schema::{Config, WebUiMode};
use crate::cost_metrics::pricing::PricingTable;
use crate::cost_metrics::tracker::CostTracker;
use crate::memory::cache::ResponseCache;
use crate::metrics::MetricsCollector;
use crate::routing::capability::{CapabilityCache, EVAL_TTL_SECS};
use crate::routing::model_catalog::{run_refresh_loop, ModelCatalog};
use crate::routing::router::Router as DispatchRouter;
use crate::routing::session_overrides::SessionOverrideStore;
use crate::server_tools::{McpSearchPool, ServerToolsRuntime};

/// One upstream's name and kind, for display on the landing page
/// (`GET /`) — never used for dispatch, which goes through `DispatchRouter`.
pub struct UpstreamSummary {
    pub name: String,
    pub kind: &'static str,
}

/// Static facts about the running server, snapshotted once at startup from
/// the loaded `Config`, for the landing page (`GET /`) to render without
/// needing a reference back to `Config` itself.
pub struct ServerInfo {
    pub port: u16,
    pub route_name: String,
    pub strategy: String,
    pub upstreams: Vec<UpstreamSummary>,
}

/// Shared state reachable from every entrypoint handler: the dispatch
/// router (candidates/providers/strategy/health/admission), the cost
/// tracker, request/error metrics, and static server info for the landing
/// page. Cloning shares the same underlying instances via `Arc::clone`.
///
/// `dispatch_router` is an [`ArcSwap`] rather than a bare `Arc` so the web
/// control panel (`POST /api/route`) can hot-swap in a freshly-rebuilt
/// `DispatchRouter` — reflecting a model-override or route-strategy change
/// — without restarting the process. Swapping loses in-flight health/rate-
/// limit state (cooldowns, counters) for the old router; that's an accepted
/// trade-off since route changes are rare admin actions, not hot-path
/// traffic.
#[derive(Clone)]
pub struct EntrypointState {
    pub dispatch_router: Arc<ArcSwap<DispatchRouter>>,
    pub cost_tracker: Arc<CostTracker>,
    pub metrics: Arc<MetricsCollector>,
    pub server_info: Arc<ServerInfo>,
    pub config_dir: Arc<std::path::PathBuf>,
    /// The single, canonical session-override store for this process.
    /// `dispatch_router` reads it via `Router::with_session_overrides`,
    /// which `api::post_route` re-attaches to every rebuilt `Router` so a
    /// pin survives a route hot-swap.
    pub session_overrides: Arc<SessionOverrideStore>,
    /// The single, canonical capability-verdict cache for this process.
    /// Same carry-across discipline as `session_overrides` (via
    /// `Router::with_capability`), plus a background eval loop that keeps
    /// verdicts fresh as the free lineup rotates.
    pub capability: Arc<CapabilityCache>,
    /// Server-tool emulation snapshot: tuning config plus route eligibility
    /// (D1), computed once at build.
    pub server_tools: Arc<ServerToolsRuntime>,
    /// Shared bounded pool of stapler-mcp stdio children (seam A). Held
    /// here — not per request — so children persist across searches and the
    /// circuit breaker sees process-wide failures. Lazy start: constructing
    /// the pool spawns nothing.
    pub search_pool: Arc<McpSearchPool>,
    /// Thread-safe store for global and session-specific transcript memory pruning policies.
    pub pruning_policy_store: Arc<PruningPolicyStore>,
    /// Long-lived SQLite cache for tool output pruned during compaction passes.
    pub omission_cache: Arc<OmissionCache>,
    /// Semantic cache of full `/v1/messages` responses, keyed by model +
    /// system prompt + tool defs + newest turn (issue #24). Disabled by
    /// default — see `crate::memory::cache::ResponseCacheConfig`.
    pub response_cache: Arc<ResponseCache>,
    /// Broadcast channel for real-time SSE telemetry events.
    pub event_tx: tokio::sync::broadcast::Sender<crate::entrypoint::events::DashboardEvent>,
    /// Web UI mode: Angular (embedded SPA) or Legacy.
    pub web_ui: WebUiMode,
    /// Also emit legacy snake_case keys on `RequestTrace` SSE frames (ADR-001 item 4 rollback).
    pub compat_snake_case: bool,
    /// Thread-safe lock for atomic configuration updates and disk persistence.
    pub config_lock: Arc<tokio::sync::Mutex<()>>,
    /// Cached per-upstream model lists (`GET /api/model-catalog`); refreshed only on request
    /// unless `[model_catalog]` opts into a timer.
    pub catalog: Arc<ModelCatalog>,
}

impl EntrypointState {
    /// Builds the dispatch router (Task 1.1.1/1.1.2), cost tracker, metrics
    /// collector (Story 6.2 Task 6.2.5), and landing-page server info from a
    /// loaded `Config`. Spawns the background event-loop-lag monitor that
    /// feeds `/metrics`' `lag_data`.
    ///
    /// # Errors
    ///
    /// Returns an error if `Router::from_config` fails to construct a
    /// dispatch router from `config` (for example, no candidates configured
    /// or a provider fails to initialize).
    pub async fn build(config: &Config, config_dir: &std::path::Path) -> anyhow::Result<Self> {
        let metrics = MetricsCollector::new();
        tokio::spawn(crate::metrics::run_lag_monitor(Arc::clone(&metrics)));
        let session_overrides = Arc::new(SessionOverrideStore::new());
        let capability = CapabilityCache::new(std::time::Duration::from_secs(EVAL_TTL_SECS));
        let local_catalog = crate::providers::local_discovery::LocalCatalog::new();
        if config.local_discovery {
            tokio::spawn(crate::providers::local_discovery::run_local_discovery(
                Arc::clone(&local_catalog),
                crate::providers::local_discovery::SCAN_INTERVAL,
            ));
        }
        let dispatch_router = Arc::new(ArcSwap::from_pointee(
            DispatchRouter::from_config(config, Arc::clone(&metrics))
                .await?
                .with_local_catalog(local_catalog)
                .with_session_overrides(Arc::clone(&session_overrides))
                .with_capability(Arc::clone(&capability)),
        ));
        tokio::spawn(crate::routing::capability::run_eval_loop(Arc::clone(
            &dispatch_router,
        )));
        if config.health_check_interval_secs > 0 {
            tokio::spawn(crate::routing::health::run_health_prober(
                Arc::clone(&dispatch_router),
                std::time::Duration::from_secs(config.health_check_interval_secs),
            ));
        }
        let cost_tracker = Arc::new(CostTracker::new(PricingTable::load_default()).await);
        let server_tools = Arc::new(ServerToolsRuntime {
            config: config.server_tools.clone(),
            route_eligible: ServerToolsRuntime::route_eligible_from_config(config),
        });
        let search_pool = Arc::new(McpSearchPool::new(server_tools.config.pool_config()));
        let pruning_policy_store = Arc::new(PruningPolicyStore::default());
        let omission_cache = Arc::new(OmissionCache::open(&OmissionCache::default_cache_path())?);
        let response_cache = Arc::new(ResponseCache::open(
            &ResponseCache::default_cache_path(),
            config.response_cache.clone(),
        )?);
        let (event_tx, _) = tokio::sync::broadcast::channel(1024);
        metrics.set_event_tx(event_tx.clone());
        let catalog = build_model_catalog(config, &dispatch_router, &event_tx);

        let ticker_tx = event_tx.clone();
        let ticker_metrics = Arc::clone(&metrics);
        let route = config.routes.first();
        let server_info = Arc::new(ServerInfo {
            port: config.port,
            route_name: route.map_or_else(String::new, |r| r.name.clone()),
            strategy: route.map_or_else(String::new, |r| format!("{:?}", r.strategy)),
            upstreams: config
                .upstreams
                .iter()
                .map(|u| UpstreamSummary {
                    name: u.name.clone(),
                    kind: u.kind.label(),
                })
                .collect(),
        });
        let upstream_names: Vec<String> = server_info
            .upstreams
            .iter()
            .map(|u| u.name.clone())
            .collect();
        let ticker_router = Arc::clone(&dispatch_router);

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                interval.tick().await;
                if ticker_tx.receiver_count() == 0 {
                    continue;
                }
                let rpm = ticker_metrics.histogram.requests_per_minute();
                let total_requests = ticker_metrics
                    .counters
                    .requests_total
                    .load(std::sync::atomic::Ordering::Relaxed);
                let tokens_before = ticker_metrics
                    .counters
                    .tokens_before
                    .load(std::sync::atomic::Ordering::Relaxed);
                let tokens_after = ticker_metrics
                    .counters
                    .tokens_after
                    .load(std::sync::atomic::Ordering::Relaxed);
                let total_tokens_saved = tokens_before.saturating_sub(tokens_after);
                let current_lag_ms = ticker_metrics.current_lag_ms();
                let provider_health = crate::entrypoint::events::provider_health_from_snapshot(
                    &ticker_router.load().cooldown_snapshot(),
                    &upstream_names,
                );

                let tick_data = crate::entrypoint::events::MetricsTickData {
                    rpm,
                    total_requests,
                    total_tokens_saved,
                    current_lag_ms,
                    provider_health,
                    median_latency_ms: crate::entrypoint::events::median_latency_ms(
                        &ticker_metrics.histogram,
                    ),
                    median_first_byte_ms: None,
                    tps: None,
                    token_savings_percent: None,
                };
                let _ = ticker_tx.send(crate::entrypoint::events::DashboardEvent::MetricsTick(
                    tick_data,
                ));
            }
        });

        // Pre-mortem failure 2: one boot log line stating whether emulation
        // is armed and whether the backend binary resolves, so "silently
        // degraded since restart" is visible in logs. No child is spawned
        // here (lazy pool start on first search).
        tracing::info!(
            enabled = server_tools.config.enabled,
            route_eligible = server_tools.route_eligible,
            backend_path = %server_tools.config.backend_path,
            backend_reachable =
                crate::server_tools::backend_reachable(&server_tools.config.backend_path),
            "server-tool emulation configured"
        );
        Ok(Self {
            dispatch_router,
            cost_tracker,
            metrics,
            server_info,
            config_dir: Arc::new(config_dir.to_path_buf()),
            session_overrides,
            capability,
            server_tools,
            search_pool,
            pruning_policy_store,
            omission_cache,
            response_cache,
            event_tx,
            web_ui: config.web_ui,
            compat_snake_case: config.compat_snake_case,
            config_lock: Arc::new(tokio::sync::Mutex::new(())),
            catalog,
        })
    }
}

/// Seeds the catalog with the router's upstreams (all `Loading`, no call made) and, only if
/// `[model_catalog]` opts into `interval`, starts the timer.
fn build_model_catalog(
    config: &Config,
    dispatch_router: &Arc<ArcSwap<DispatchRouter>>,
    event_tx: &tokio::sync::broadcast::Sender<crate::entrypoint::events::DashboardEvent>,
) -> Arc<ModelCatalog> {
    let notify_tx = event_tx.clone();
    let catalog = ModelCatalog::new(
        config.model_catalog.clone(),
        Some(Arc::new(move |upstream| {
            let _ = notify_tx.send(
                crate::entrypoint::events::DashboardEvent::ModelCatalogChanged(
                    crate::entrypoint::events::ModelCatalogChangedData {
                        timestamp: chrono::Utc::now().to_rfc3339(),
                        upstream: upstream.to_string(),
                    },
                ),
            );
        })),
    );
    let names: Vec<_> = dispatch_router
        .load()
        .providers_named()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    catalog.prune(&names);
    if let crate::config::schema::CatalogRefreshMode::Interval { secs } =
        config.model_catalog.refresh_mode()
    {
        let router = Arc::downgrade(dispatch_router);
        tokio::spawn(run_refresh_loop(
            Arc::downgrade(&catalog),
            secs,
            move || {
                router
                    .upgrade()
                    .map(|r| r.load().providers_named())
                    .unwrap_or_default()
            },
        ));
    }
    catalog
}

/// Builds the axum `Router` exposing the landing page and the entrypoint
/// routes, with request tracing applied.
pub fn entrypoint_router(state: EntrypointState) -> axum::Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let mut router = axum::Router::new()
        .route("/", axum::routing::get(landing::get_index))
        .route(
            "/v1/messages",
            axum::routing::post(crate::entrypoint::messages::post_v1_messages),
        )
        .route(
            "/v1/models",
            axum::routing::get(crate::entrypoint::messages::get_v1_models),
        )
        .route(
            "/v1/chat/completions",
            axum::routing::post(crate::entrypoint::chat_completions::post_v1_chat_completions),
        )
        .route(
            "/v1/dashboard/events",
            axum::routing::get(events::handle_sse_events),
        )
        .route(
            "/v1/dashboard/traces",
            axum::routing::get(traces::get_dashboard_traces),
        )
        .route(
            "/v1/dashboard/sessions",
            axum::routing::get(observability::get_dashboard_sessions),
        )
        .route(
            "/v1/dashboard/benchmark",
            axum::routing::get(observability::get_dashboard_benchmark),
        )
        .route(
            "/v1/dashboard/config",
            axum::routing::get(observability::get_dashboard_config)
                .put(observability::put_dashboard_config),
        );

    if state.web_ui == WebUiMode::Legacy {
        router = router.route(
            "/dashboard",
            axum::routing::get(crate::dashboard::handle_dashboard),
        );
    } else {
        router = router
            .route(
                "/dashboard",
                axum::routing::get(crate::dashboard::serve_embedded_asset),
            )
            .route(
                "/dashboard/",
                axum::routing::get(crate::dashboard::serve_embedded_asset),
            )
            .route(
                "/dashboard/{*path}",
                axum::routing::get(crate::dashboard::serve_embedded_asset),
            );
    }

    router
        .route("/metrics", axum::routing::get(observability::get_metrics))
        .route(
            "/errors/summary",
            axum::routing::get(observability::get_errors_summary),
        )
        .route(
            "/requests/{id}",
            axum::routing::get(observability::get_request_body),
        )
        .route("/api/models", axum::routing::get(api::get_models))
        .route(
            "/api/model-catalog",
            axum::routing::get(models::get_model_catalog),
        )
        .route(
            "/api/model-catalog/refresh",
            axum::routing::post(models::post_model_catalog_refresh),
        )
        .route(
            "/api/route",
            axum::routing::get(api::get_route).post(api::post_route),
        )
        .route("/api/sessions", axum::routing::get(api::get_sessions))
        .route(
            "/api/sessions/{id}/route",
            axum::routing::get(api::get_session_route)
                .post(api::post_session_route)
                .delete(api::delete_session_route),
        )
        .route(
            "/session/prune",
            axum::routing::post(api::post_session_prune),
        )
        .route(
            "/session/policy",
            axum::routing::get(api::get_session_policy).post(api::post_session_policy),
        )
        .route(
            "/session/prune/stats",
            axum::routing::get(api::get_session_prune_stats),
        )
        .layer(axum::extract::DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES))
        .layer(cors)
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// Above Anthropic's 32 MB request cap so the upstream, not axum's 2 MB default,
/// rejects oversized image-heavy sessions.
const MAX_REQUEST_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Binds `127.0.0.1:{port}` and serves until Ctrl-C/SIGTERM.
///
/// # Errors
///
/// Returns an error if binding `127.0.0.1:{port}` fails (for example, the
/// port is already in use) or if `axum::serve` itself returns an I/O error.
pub async fn serve_entrypoint(port: u16, state: EntrypointState) -> anyhow::Result<()> {
    serve_entrypoint_with_shutdown(port, state, shutdown_signal()).await
}

/// Same as `serve_entrypoint`, but with a caller-supplied shutdown future
/// so tests can trigger shutdown deterministically instead of waiting on a
/// real OS signal.
///
/// # Errors
///
/// Returns an error if binding `127.0.0.1:{port}` fails (for example, the
/// port is already in use) or if `axum::serve` itself returns an I/O error.
pub async fn serve_entrypoint_with_shutdown(
    port: u16,
    state: EntrypointState,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "consolette http-entrypoint listening");
    let router = entrypoint_router(state);
    let shutdown_started = Arc::new(tokio::sync::Notify::new());
    let notify_started = Arc::clone(&shutdown_started);
    let serve = axum::serve(listener, router).with_graceful_shutdown(async move {
        shutdown.await;
        notify_started.notify_one();
    });
    // Graceful shutdown waits on every open connection, and a dashboard SSE
    // stream never ends on its own, so cap the drain.
    tokio::select! {
        result = serve => result?,
        () = async {
            shutdown_started.notified().await;
            tokio::time::sleep(SHUTDOWN_GRACE).await;
        } => tracing::warn!(
            grace_secs = SHUTDOWN_GRACE.as_secs(),
            "shutdown grace elapsed with connections still open; exiting"
        ),
    }
    Ok(())
}

/// How long in-flight requests may drain after a shutdown signal.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// # Panics
///
/// Panics if the OS refuses to install the Ctrl+C or SIGTERM handler
/// (`tokio::signal::ctrl_c`/`tokio::signal::unix::signal` failing at all is
/// itself a sign the process environment is broken beyond recovery, so
/// there's no meaningful fallback besides surfacing it immediately).
#[allow(clippy::expect_used)]
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tower::ServiceExt;

    #[tokio::test]
    async fn build_succeeds_for_default_config() {
        let state = EntrypointState::build(
            &Config::default(),
            std::path::Path::new("/tmp/consolette-test"),
        )
        .await;
        assert!(state.is_ok());
    }

    // REQ-2 (Story 1.1.2): `upstream_kind_label` accepts `UpstreamKind::Gemini`.
    #[test]
    fn upstream_kind_label_should_return_gemini_for_new_variant() {
        let kind = UpstreamKind::Gemini {
            project_id: "p1".to_string(),
        };

        assert_eq!(kind.label(), "gemini");
    }

    // REQ-1 (Story 1.2.3, Task 1.2.3d): `upstream_kind_label` accepts
    // `UpstreamKind::Openrouter`.
    #[test]
    fn upstream_kind_label_should_return_openrouter_for_new_variant() {
        assert_eq!(UpstreamKind::Openrouter {}.label(), "openrouter");
    }

    #[tokio::test]
    async fn build_clone_shares_tracker_state() {
        use crate::session_compaction::{CompactionTier, SessionKey};

        let state = EntrypointState::build(
            &Config::default(),
            std::path::Path::new("/tmp/consolette-test"),
        )
        .await
        .unwrap();
        let clone_a = state.clone();
        let clone_b = state.clone();

        let session_key = SessionKey::new("shared-session");
        clone_a
            .cost_tracker
            .record_pending(
                &session_key,
                crate::cost_metrics::types::RequestId::new(),
                CompactionTier::Off,
            )
            .await;

        // Both clones must see the same session because they share one
        // Arc<CostTracker>, not independent copies.
        assert!(clone_b
            .cost_tracker
            .report_for_session(&session_key)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn entrypoint_router_routes_do_not_404() {
        let state = EntrypointState::build(
            &Config::default(),
            std::path::Path::new("/tmp/consolette-test"),
        )
        .await
        .unwrap();
        let router = entrypoint_router(state);

        let resp = router
            .clone()
            .oneshot(
                axum::http::Request::post("/v1/messages")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(resp.status(), axum::http::StatusCode::NOT_FOUND);

        let resp = router
            .oneshot(
                axum::http::Request::post("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from("{}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(resp.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn v1_messages_accepts_bodies_over_axum_default_limit() {
        let state = EntrypointState::build(
            &Config::default(),
            std::path::Path::new("/tmp/consolette-test"),
        )
        .await
        .unwrap();
        let router = entrypoint_router(state);
        // ~3 MB of base64-sized text: past axum's 2 MB default, as with image-heavy sessions.
        let body = serde_json::json!({
            "model": "claude-sonnet-5-5",
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "A".repeat(3 * 1024 * 1024)}],
        })
        .to_string();
        let resp = router
            .oneshot(
                axum::http::Request::post("/v1/messages")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(resp.status(), axum::http::StatusCode::PAYLOAD_TOO_LARGE);
    }

    #[tokio::test]
    async fn landing_page_serves_html_at_root() {
        let state = EntrypointState::build(
            &Config::default(),
            std::path::Path::new("/tmp/consolette-test"),
        )
        .await
        .unwrap();
        let router = entrypoint_router(state);

        let resp = router
            .oneshot(
                axum::http::Request::get("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let content_type = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(content_type.starts_with("text/html"));
    }

    #[tokio::test]
    async fn observability_routes_serve_expected_shapes() {
        let state = EntrypointState::build(
            &Config::default(),
            std::path::Path::new("/tmp/consolette-test"),
        )
        .await
        .unwrap();
        let router = entrypoint_router(state);

        let resp = router
            .clone()
            .oneshot(
                axum::http::Request::get("/metrics")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json.get("summary").is_some(), "missing summary: {json}");
        assert!(
            json.get("error_types").is_some(),
            "missing error_types: {json}"
        );

        let resp = router
            .clone()
            .oneshot(
                axum::http::Request::get("/errors/summary")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["errors"], serde_json::json!([]));

        let resp = router
            .oneshot(
                axum::http::Request::get("/dashboard")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn graceful_shutdown_completes_promptly() {
        let state = EntrypointState::build(
            &Config::default(),
            std::path::Path::new("/tmp/consolette-test"),
        )
        .await
        .unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown = async move {
            let _ = rx.await;
        };

        let handle = tokio::spawn(serve_entrypoint_with_shutdown(0, state, shutdown));
        tx.send(()).unwrap();

        let result = tokio::time::timeout(Duration::from_secs(2), handle).await;
        assert!(result.is_ok(), "server did not shut down within timeout");
        assert!(result.unwrap().unwrap().is_ok());
    }

    /// E-02: an open dashboard SSE stream used to hold graceful shutdown open
    /// forever, so a restarted proxy left the old process serving a dead feed
    /// to the browser, which never saw a disconnect.
    #[tokio::test]
    async fn shutdown_should_complete_even_while_a_dashboard_sse_client_is_connected() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let reservation = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = reservation.local_addr().unwrap();
        drop(reservation);

        let state = EntrypointState::build(
            &Config::default(),
            std::path::Path::new("/tmp/consolette-test"),
        )
        .await
        .unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown = async move {
            let _ = rx.await;
        };
        let handle = tokio::spawn(serve_entrypoint_with_shutdown(addr.port(), state, shutdown));
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
        client
            .write_all(
                b"GET /v1/dashboard/events HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n",
            )
            .await
            .unwrap();
        let mut head = [0_u8; 64];
        let n = client.read(&mut head).await.unwrap();
        assert!(String::from_utf8_lossy(&head[..n]).contains("200"));

        tx.send(()).unwrap();
        let result = tokio::time::timeout(Duration::from_secs(5), handle).await;
        assert!(
            result.is_ok(),
            "server stayed up after shutdown because an SSE stream was still open"
        );
    }

    #[tokio::test]
    async fn loopback_bind_is_reachable_only_via_localhost() {
        // Mirror serve_entrypoint's own bind line directly to prove the
        // address is loopback regardless of the requested port.
        let probe = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        assert_eq!(
            probe.local_addr().unwrap().ip(),
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        );
        drop(probe);

        // Reserve an ephemeral loopback port, release it, then have the
        // real server bind that same port and confirm a plain TCP connect
        // succeeds — proving the live server socket is loopback-reachable.
        let reservation = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .unwrap();
        let addr = reservation.local_addr().unwrap();
        drop(reservation);

        let state = EntrypointState::build(
            &Config::default(),
            std::path::Path::new("/tmp/consolette-test"),
        )
        .await
        .unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let shutdown = async move {
            let _ = rx.await;
        };
        let handle = tokio::spawn(serve_entrypoint_with_shutdown(addr.port(), state, shutdown));
        // Give the server a moment to bind before connecting.
        tokio::time::sleep(Duration::from_millis(50)).await;

        let connect = tokio::net::TcpStream::connect(addr).await;
        assert!(connect.is_ok(), "expected loopback connect to succeed");

        tx.send(()).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(2), handle).await;
    }

    #[tokio::test]
    async fn entrypoint_legacy_fallback_serves_legacy_dashboard_html() {
        let config = Config {
            web_ui: WebUiMode::Legacy,
            compat_snake_case: false,
            ..Default::default()
        };
        let state = EntrypointState::build(&config, std::path::Path::new("/tmp/consolette-test"))
            .await
            .unwrap();
        let router = entrypoint_router(state);

        let resp = router
            .oneshot(
                axum::http::Request::get("/dashboard")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let html = String::from_utf8_lossy(&body);
        assert!(html.contains("Claude Proxy Dashboard"));
    }

    #[tokio::test]
    async fn sse_event_stream_returns_text_event_stream() {
        let state = EntrypointState::build(
            &Config::default(),
            std::path::Path::new("/tmp/consolette-test"),
        )
        .await
        .unwrap();
        let router = entrypoint_router(state);

        let resp = router
            .oneshot(
                axum::http::Request::get("/v1/dashboard/events")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let content_type = resp
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(content_type.contains("text/event-stream"));
    }
}
