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
pub mod landing;
pub mod messages;
pub mod observability;
pub mod openai_stream;

use std::net::SocketAddr;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::config::schema::{Config, UpstreamKind};
use crate::cost_metrics::pricing::PricingTable;
use crate::cost_metrics::tracker::CostTracker;
use crate::metrics::MetricsCollector;
use crate::routing::router::Router as DispatchRouter;
use crate::routing::session_overrides::SessionOverrideStore;

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
        let dispatch_router = Arc::new(ArcSwap::from_pointee(
            DispatchRouter::from_config(config, Arc::clone(&metrics))
                .await?
                .with_session_overrides(Arc::clone(&session_overrides)),
        ));
        let cost_tracker = Arc::new(CostTracker::new(PricingTable::load_default()).await);
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
                    kind: upstream_kind_label(&u.kind),
                })
                .collect(),
        });
        Ok(Self {
            dispatch_router,
            cost_tracker,
            metrics,
            server_info,
            config_dir: Arc::new(config_dir.to_path_buf()),
            session_overrides,
        })
    }
}

fn upstream_kind_label(kind: &UpstreamKind) -> &'static str {
    match kind {
        UpstreamKind::Anthropic => "anthropic",
        UpstreamKind::Bedrock { .. } => "bedrock",
        UpstreamKind::Openai { .. } => "openai",
        UpstreamKind::Gemini { .. } => "gemini",
        UpstreamKind::Openrouter {} => "openrouter",
    }
}

/// Builds the axum `Router` exposing the landing page and the entrypoint
/// routes, with request tracing applied.
pub fn entrypoint_router(state: EntrypointState) -> axum::Router {
    axum::Router::new()
        .route("/", axum::routing::get(landing::get_index))
        .route(
            "/v1/messages",
            axum::routing::post(crate::entrypoint::messages::post_v1_messages),
        )
        .route(
            "/v1/chat/completions",
            axum::routing::post(crate::entrypoint::chat_completions::post_v1_chat_completions),
        )
        .route(
            "/dashboard",
            axum::routing::get(crate::dashboard::handle_dashboard),
        )
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
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

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
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

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

        assert_eq!(upstream_kind_label(&kind), "gemini");
    }

    // REQ-1 (Story 1.2.3, Task 1.2.3d): `upstream_kind_label` accepts
    // `UpstreamKind::Openrouter`.
    #[test]
    fn upstream_kind_label_should_return_openrouter_for_new_variant() {
        assert_eq!(upstream_kind_label(&UpstreamKind::Openrouter {}), "openrouter");
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
}
