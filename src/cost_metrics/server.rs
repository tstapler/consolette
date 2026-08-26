//! `consolette serve-cost` — Epic 2.3 (ADR-012, Amendment).
//!
//! The single live process that constructs `SessionCompactionPipeline` with
//! `CostTrackingHook` registered, owns the one `Arc<CostTracker>`, and hosts
//! the one HTTP route (`GET /v1/cost/{session_key}`) reading from it. Per
//! the plan's repair iteration 1: the CLI (`cost-report`, Epic 3.1) is a
//! `reqwest` client of this route rather than an independent constructor of
//! equivalent state, so "the CLI and HTTP surfaces agree" is a structural
//! property of there being exactly one process/tracker, not two
//! independent implementations sharing a type definition.
//!
//! Out of scope for this epic (see plan.md's explicit scope boundary): no
//! `providers`/`routing` wiring drives `pipeline.apply()` from a live
//! request here. This process proves the pipeline+tracker+route can be
//! constructed sharing one `Arc<CostTracker>`; a later epic is responsible
//! for feeding it real compaction traffic.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use tokio::net::TcpListener;
use tokio::sync::watch;

use crate::claude_code_session::session_bi::{
    build_session_bi_snapshot, spawn_session_bi_refresh_task, SessionBiSnapshot,
    SESSION_BI_PER_FILE_TIMEOUT, SESSION_BI_REFRESH_INTERVAL, SESSION_BI_SCAN_CONCURRENCY,
};
use crate::context_forensics::refresh::{
    spawn_context_forensics_refresh_task, CONTEXT_FORENSICS_REFRESH_INTERVAL,
};
use crate::context_forensics::server::{context_router, context_unavailable_router};
use crate::context_forensics::store::ContextForensicsStore;
use crate::cost_metrics::estimator::TiktokenEstimator;
use crate::cost_metrics::hook::CostTrackingHook;
use crate::cost_metrics::pricing::{spawn_pricing_refresh_task, PricingTable, LITELLM_PRICING_URL};
use crate::cost_metrics::report::CostReportError;
use crate::cost_metrics::tracker::CostTracker;
use crate::session_compaction::{SessionCompactionPipeline, SessionKey, TierThresholds};

/// Vanilla HTML/JS/CSS dashboard shell, compiled into the binary (see
/// `pricing.rs`'s `pricing_default.json` for the precedent). No build step,
/// no CDN dependency (ADR-015) — the file fetches `/v1/dashboard/sessions`
/// client-side and does its own sort/filter/render.
const DASHBOARD_HTML: &str = include_str!("dashboard.html");

/// How often the background task re-fetches `LITELLM_PRICING_URL` (Story
/// 1.4.2). Pricing drifts on the order of days/weeks, not minutes, so this
/// favors a low, predictable request rate over freshness.
const PRICING_REFRESH_INTERVAL: Duration = Duration::from_hours(24);

/// Default loopback bind port (Story 2.3.1's resolved bind-address
/// blocker): operator-only/internal, so loopback-only with no public bind
/// option in this epic.
pub const DEFAULT_PORT: u16 = 8787;

/// Model tag `CostTrackingHook`'s `TiktokenEstimator` estimates against.
/// `serve-cost` doesn't front any real provider traffic yet (out of scope
/// for this epic), so there is no real per-request model to thread through;
/// this is a fixed placeholder used only if/when something drives
/// `apply()` through this process's pipeline.
const BOOTSTRAP_MODEL: &str = "claude-sonnet-5";

/// The one live pipeline + tracker this process owns. The pipeline and the
/// `/v1/cost` route below share the identical `Arc<CostTracker>` —
/// constructing them separately (as the pre-repair-iteration-1 draft did)
/// is exactly the bug this type exists to make structurally impossible.
pub struct CostServerState {
    pub tracker: Arc<CostTracker>,
    pub pipeline: SessionCompactionPipeline,
    /// Story 1.4.2's background refresh task. Held only to keep it alive for
    /// `CostServerState`'s lifetime (`tokio::spawn` already detaches it, so
    /// nothing ever `.await`s this) — dropped, and thus aborted, when the
    /// server shuts down.
    _pricing_refresh: tokio::task::JoinHandle<()>,
    /// Latest session BI snapshot, read by `handler_dashboard_sessions`.
    /// `watch::Receiver` is `Clone`, so each request handler gets its own
    /// cheap clone rather than contending on a shared lock.
    pub session_bi_rx: watch::Receiver<Arc<SessionBiSnapshot>>,
    /// Mirrors `_pricing_refresh`: held only to keep the background scan
    /// task alive for this state's lifetime.
    _session_bi_refresh: tokio::task::JoinHandle<()>,
    /// `Some` when [`ContextForensicsStore::open`] succeeded at startup;
    /// `None` when it failed (Story 1.4.4's fail-open contract — this
    /// feature going down must never take `cost_metrics`'s own working
    /// dashboard down with it). `serve_cost` merges either
    /// [`context_router`] or [`context_unavailable_router`] depending on
    /// which this is.
    pub context_store: Option<Arc<ContextForensicsStore>>,
    /// Mirrors `_session_bi_refresh`; `None` alongside `context_store: None`.
    _context_refresh: Option<tokio::task::JoinHandle<()>>,
    /// Cloned before the original is moved into
    /// `CostTracker::new_with_pricing_receiver` — `context_router`'s
    /// `/v1/context/sessions/summary` route (Story 2.1.1) needs its own
    /// handle on the same live-refreshing pricing table so cross-session
    /// cost figures use the same cache-aware rates `/v1/cost` does.
    pub pricing_rx: watch::Receiver<Arc<PricingTable>>,
}

impl CostServerState {
    /// Real constructor (Task 3.1.2a/b): builds the shared tracker, a
    /// `TiktokenEstimator`-backed `CostTrackingHook`, the
    /// `SessionCompactionPipeline` with that hook registered, Story 1.4.2's
    /// background pricing-refresh task, and an eager initial session-BI
    /// snapshot scanned from `session_glob` before spawning the periodic
    /// refresh task — so the very first `/dashboard` request after startup
    /// doesn't race an empty snapshot.
    ///
    /// Takes the glob directly (never resolves `$HOME` itself) so tests can
    /// point it at a fixture directory instead of a real
    /// `~/.claude/projects` tree — see [`Self::build`] for the thin
    /// `$HOME`-resolving wrapper real callers use. Delegates to
    /// [`Self::build_with_session_glob_and_context_store_path`] using
    /// [`ContextForensicsStore::default_store_path`].
    pub async fn build_with_session_glob(session_glob: &str) -> Self {
        Self::build_with_session_glob_and_context_store_path(
            session_glob,
            &ContextForensicsStore::default_store_path(),
        )
        .await
    }

    /// Real constructor, parameterized on the context-forensics store path
    /// too (Task 1.4.4d: tests need to point this at a path that can't be
    /// opened, to exercise the fail-open contract without touching a real
    /// `~/.claude/consolette/context-forensics.sqlite`).
    ///
    /// [`ContextForensicsStore::open`] failing here is handled fail-open,
    /// not propagated via `?`: it's logged via `tracing::error!` and
    /// `context_store` is left `None` — every other part of this state
    /// (tracker, pipeline, session-BI snapshot) still constructs normally.
    pub async fn build_with_session_glob_and_context_store_path(
        session_glob: &str,
        context_store_path: &std::path::Path,
    ) -> Self {
        let (pricing_tx, pricing_rx) = watch::channel(Arc::new(PricingTable::load_default()));
        let pricing_refresh = spawn_pricing_refresh_task(
            pricing_tx,
            reqwest::Client::new(),
            LITELLM_PRICING_URL.to_string(),
            PRICING_REFRESH_INTERVAL,
        );
        let context_pricing_rx = pricing_rx.clone();
        let tracker = Arc::new(CostTracker::new_with_pricing_receiver(pricing_rx).await);
        let hook = Arc::new(CostTrackingHook::new(
            Arc::clone(&tracker),
            Arc::new(TiktokenEstimator::new()),
            BOOTSTRAP_MODEL,
        ));
        let mut pipeline = SessionCompactionPipeline::new(TierThresholds::default()).await;
        pipeline.register_hook(hook);

        let initial_snapshot = build_session_bi_snapshot(
            session_glob,
            BOOTSTRAP_MODEL,
            SESSION_BI_SCAN_CONCURRENCY,
            SESSION_BI_PER_FILE_TIMEOUT,
        )
        .await;
        let (session_bi_tx, session_bi_rx) = watch::channel(Arc::new(initial_snapshot));
        let session_bi_refresh = spawn_session_bi_refresh_task(
            session_bi_tx,
            session_glob.to_string(),
            BOOTSTRAP_MODEL.to_string(),
            SESSION_BI_REFRESH_INTERVAL,
            SESSION_BI_SCAN_CONCURRENCY,
            SESSION_BI_PER_FILE_TIMEOUT,
        );

        let (context_store, context_refresh) = match ContextForensicsStore::open(context_store_path)
        {
            Ok(store) => {
                let store = Arc::new(store);
                let refresh = spawn_context_forensics_refresh_task(
                    Arc::clone(&store),
                    session_glob.to_string(),
                    CONTEXT_FORENSICS_REFRESH_INTERVAL,
                )
                .await;
                (Some(store), Some(refresh))
            }
            Err(error) => {
                tracing::error!(
                    %error,
                    path = %context_store_path.display(),
                    "context forensics store failed to open; context-forensics routes disabled"
                );
                (None, None)
            }
        };

        CostServerState {
            tracker,
            pipeline,
            _pricing_refresh: pricing_refresh,
            session_bi_rx,
            _session_bi_refresh: session_bi_refresh,
            context_store,
            _context_refresh: context_refresh,
            pricing_rx: context_pricing_rx,
        }
    }

    /// Thin wrapper (Task 3.1.2b): resolves `$HOME` once into the default
    /// `~/.claude/projects/**/*.jsonl` glob and delegates to
    /// [`Self::build_with_session_glob`]. Keeping this signature unchanged
    /// (`async fn build() -> Self`, no `Result`) matters: it's the
    /// constructor the pre-existing
    /// `serve_cost_should_expose_apply_result_via_http_route_when_pipeline_and_route_share_same_tracker`
    /// test and real `serve_cost` both call directly.
    ///
    /// # Panics
    ///
    /// Panics if the `HOME` environment variable is not set — matching
    /// `discover_sessions`'s own behavior for the same condition, just
    /// surfaced at process-startup time instead of at first scan.
    #[allow(clippy::expect_used)]
    pub async fn build() -> Self {
        let home = std::env::var("HOME").expect("HOME environment variable not set");
        let session_glob = format!("{home}/.claude/projects/**/*.jsonl");
        Self::build_with_session_glob(&session_glob).await
    }
}

/// Builds the `/v1/cost/{session_key}` route group, `State`-sharing
/// `tracker` with whatever else in this process holds it.
#[must_use = "dropping the router without serving it drops the route registration"]
pub fn cost_router(tracker: Arc<CostTracker>) -> Router {
    Router::new()
        .route("/v1/cost/{session_key}", get(handler_cost_report))
        .with_state(tracker)
}

/// `GET /v1/cost/{session_key}`: `200` + the session's `CostReport` as JSON,
/// or `404` + `{"error":"session_not_found","session_key":...}` (ux.md's
/// error-states table) when the session was never seen or was evicted.
async fn handler_cost_report(
    State(tracker): State<Arc<CostTracker>>,
    Path(session_key): Path<String>,
) -> impl IntoResponse {
    let key = SessionKey::new(session_key.clone());
    match tracker.report_for_session(&key).await {
        Ok(report) => (StatusCode::OK, Json(report)).into_response(),
        Err(CostReportError::SessionNotFound) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "session_not_found",
                "session_key": session_key,
            })),
        )
            .into_response(),
    }
}

/// Builds the `/dashboard` and `/v1/dashboard/sessions` route group,
/// `State`-sharing the session-BI watch receiver.
#[must_use = "dropping the router without serving it drops the route registration"]
pub fn dashboard_router(session_bi_rx: watch::Receiver<Arc<SessionBiSnapshot>>) -> Router {
    Router::new()
        .route("/dashboard", get(|| async { Html(DASHBOARD_HTML) }))
        .route("/v1/dashboard/sessions", get(handler_dashboard_sessions))
        .with_state(session_bi_rx)
}

/// `GET /v1/dashboard/sessions`: `200` + the latest [`SessionBiSnapshot`] as
/// JSON. Always succeeds — an empty/never-yet-scanned corpus is a valid
/// snapshot (empty `rows`), not an error state; `dashboard.html`'s
/// `#status-banner` distinguishes "loading"/"empty" from "fetch failed" on
/// the client side.
async fn handler_dashboard_sessions(
    State(session_bi_rx): State<watch::Receiver<Arc<SessionBiSnapshot>>>,
) -> impl IntoResponse {
    let snapshot = session_bi_rx.borrow().clone();
    (StatusCode::OK, Json(snapshot)).into_response()
}

/// `consolette serve-cost` entry point (Tasks 2.3.1b/c): builds the shared
/// state, binds `127.0.0.1:{port}`, and serves the route forever.
///
/// # Errors
///
/// Returns an error if the port can't be bound or the server fails while
/// serving.
pub async fn serve_cost(port: u16) -> anyhow::Result<()> {
    let state = CostServerState::build().await;
    let context_routes = match &state.context_store {
        Some(store) => context_router(Arc::clone(store), state.pricing_rx.clone()),
        None => context_unavailable_router(),
    };
    let router = cost_router(Arc::clone(&state.tracker))
        .merge(dashboard_router(state.session_bi_rx.clone()))
        .merge(context_routes);

    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let listener = TcpListener::bind(addr).await?;
    tracing::info!(%addr, "consolette serve-cost listening");
    axum::serve(listener, router).await?;

    // Keep `state.pipeline` alive for the server's whole lifetime even
    // though nothing calls `apply()` on it yet in this epic — dropping it
    // early would be a pointless footgun for whatever wires real traffic
    // through it next.
    drop(state);
    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tower::ServiceExt;

    use super::*;
    use crate::cost_metrics::types::TokenCount;

    async fn tracker() -> Arc<CostTracker> {
        Arc::new(CostTracker::new(PricingTable::new()).await)
    }

    fn tc(value: u64) -> TokenCount {
        TokenCount {
            value,
            source: crate::cost_metrics::types::TokenSource::Exact,
        }
    }

    /// Task 2.3.1d: real bind, `reqwest` GET against an OS-assigned port.
    #[tokio::test]
    async fn serve_cost_should_respond_404_when_unknown_session_queried() {
        let tracker = tracker().await;
        let router = cost_router(tracker);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let url = format!("http://{addr}/v1/cost/nonexistent");
        let response = reqwest::get(&url).await.expect("request should succeed");
        assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
        let body: serde_json::Value = response.json().await.expect("body should be JSON");
        assert_eq!(
            body,
            json!({"error": "session_not_found", "session_key": "nonexistent"})
        );
    }

    /// Task 2.3.1e: direct regression guard for Blocker 1 — construct the
    /// process's pipeline+tracker+route exactly as `serve_cost` does, drive
    /// one `apply()` call through the pipeline, reconcile it, and assert
    /// the HTTP route (via axum's `oneshot`, no real bind) returns it.
    ///
    /// Uses `build_with_session_glob_and_context_store_path` against
    /// isolated tempdirs rather than the real `build()` — this test only
    /// exercises `tracker`/`pipeline`, not context-forensics, and pointing
    /// it at the real `$HOME/.claude/projects` + shared
    /// `~/.claude/consolette/context-forensics.sqlite` (as `build()` does)
    /// made it race every other test/process doing the same against that
    /// one real file, and scan Tyler's real (7000+ file, 3GB+) transcript
    /// directory on every run — a correctness-neutral but severe slowdown,
    /// not a hang in the ingestion logic itself (`refresh.rs` already
    /// bounds each file with `CONTEXT_FORENSICS_PER_FILE_TIMEOUT`).
    #[tokio::test]
    async fn serve_cost_should_expose_apply_result_via_http_route_when_pipeline_and_route_share_same_tracker(
    ) {
        let session_dir = tempfile::tempdir().expect("tempdir should succeed");
        let session_pattern = format!("{}/*.jsonl", session_dir.path().display());
        let context_store_dir = tempfile::tempdir().expect("tempdir should succeed");
        let context_store_path = context_store_dir.path().join("cf.sqlite");
        let state = CostServerState::build_with_session_glob_and_context_store_path(
            &session_pattern,
            &context_store_path,
        )
        .await;
        let router = cost_router(Arc::clone(&state.tracker));

        let key = SessionKey::new("serve-cost-e2e");
        let messages = json!([{"role": "user", "content": "hello world, this is a test message"}]);
        let (_out, report) = state.pipeline.apply(&key, &messages, 0.95).await;
        let request_id = report.request_id;

        // Synthetic reconciliation: real usage arrives. This may race the
        // hook's spawned counterfactual estimation (either order is valid
        // per `record_actual_usage`'s doc comment) — the fold only
        // completes once both sides have landed, which the poll below
        // waits out.
        state
            .tracker
            .record_actual_usage(&key, request_id, tc(5))
            .await
            .expect("session is known to the tracker");

        // Let the hook's spawned counterfactual estimation land and fold.
        let mut attempts = 0;
        while state
            .tracker
            .report_for_session(&key)
            .await
            .expect("session should exist")
            .tokens_saved
            .is_none()
            && attempts < 50
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
            attempts += 1;
        }

        let request = axum::http::Request::builder()
            .uri("/v1/cost/serve-cost-e2e")
            .body(axum::body::Body::empty())
            .expect("request should build");
        let response = router.oneshot(request).await.expect("route should respond");
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("body should be JSON");
        assert_eq!(body["session_key"], "serve-cost-e2e");
        assert!(
            body["tokens_saved"].is_number(),
            "expected a reconciled, non-null tokens_saved, got {body:?}"
        );
    }

    /// Task 2.3.1e's unit-level sibling (`handler_cost_report` happy path):
    /// axum `oneshot`, no full bind.
    #[tokio::test]
    async fn handler_cost_report_should_return_200_with_report_json_when_session_reconciled() {
        let tracker = tracker().await;
        let key = SessionKey::new("s1");
        let request_id = crate::cost_metrics::types::RequestId::new();

        tracker
            .record_pending(
                &key,
                request_id,
                crate::session_compaction::CompactionTier::Full,
            )
            .await;
        tracker
            .record_counterfactual(&key, request_id, tc(12000), tc(4000))
            .await
            .expect("session is known");
        tracker
            .record_actual_usage(&key, request_id, tc(4000))
            .await
            .expect("session is known");

        let router = cost_router(tracker);
        let request = axum::http::Request::builder()
            .uri("/v1/cost/s1")
            .body(axum::body::Body::empty())
            .expect("request should build");
        let response = router.oneshot(request).await.expect("route should respond");
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("body should be JSON");
        assert_eq!(body["session_key"], "s1");
        assert_eq!(body["tokens_saved"], 8000);
    }

    #[tokio::test]
    async fn handler_cost_report_should_return_404_with_session_key_echoed_when_session_unknown() {
        let tracker = tracker().await;
        let router = cost_router(tracker);

        let request = axum::http::Request::builder()
            .uri("/v1/cost/ghost")
            .body(axum::body::Body::empty())
            .expect("request should build");
        let response = router.oneshot(request).await.expect("route should respond");
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("body should be JSON");
        assert_eq!(
            body,
            json!({"error": "session_not_found", "session_key": "ghost"})
        );
    }

    /// ux.md AC: unknown/pending numeric fields serialize as JSON `null`,
    /// never `0` — a session with only a `Pending` row (no reconciliation
    /// yet) must show `tokens_saved: null`, not `Some(0)`.
    #[tokio::test]
    async fn handler_cost_report_should_serialize_null_not_zero_when_tokens_saved_unknown() {
        let tracker = tracker().await;
        let key = SessionKey::new("pending-only");
        let request_id = crate::cost_metrics::types::RequestId::new();
        tracker
            .record_pending(
                &key,
                request_id,
                crate::session_compaction::CompactionTier::Full,
            )
            .await;

        let router = cost_router(tracker);
        let request = axum::http::Request::builder()
            .uri("/v1/cost/pending-only")
            .body(axum::body::Body::empty())
            .expect("request should build");
        let response = router.oneshot(request).await.expect("route should respond");
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("body should be JSON");
        assert!(body["tokens_saved"].is_null());
        assert!(body["actual_tokens"].is_null());
        assert!(body["counterfactual_source"].is_null());
    }

    /// ux.md AC10: `pricing_source` is always an explicit tagged value
    /// (`"Static"`/`"Live"`), not inferred/omitted.
    #[tokio::test]
    async fn handler_cost_report_should_include_explicit_pricing_source_tag_when_response_rendered()
    {
        let tracker = tracker().await;
        let key = SessionKey::new("s1");
        let request_id = crate::cost_metrics::types::RequestId::new();
        tracker
            .record_pending(
                &key,
                request_id,
                crate::session_compaction::CompactionTier::Full,
            )
            .await;

        let router = cost_router(tracker);
        let request = axum::http::Request::builder()
            .uri("/v1/cost/s1")
            .body(axum::body::Body::empty())
            .expect("request should build");
        let response = router.oneshot(request).await.expect("route should respond");

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        let body: serde_json::Value = serde_json::from_slice(&body).expect("body should be JSON");
        assert_eq!(body["pricing_source"], "Static");
    }

    /// Task 3.2.1d / repair iteration 1: with the CLI now a `reqwest` client
    /// of this route rather than an independent `report_for_session`
    /// caller, "the two surfaces agree" is a structural property, not
    /// something worth proving by comparing two separately-computed values.
    /// This guards against a *future* regression reintroducing an
    /// independent CLI-side computation: bind the real server, call
    /// `fetch_cost_report` (Epic 3.1) against it, and separately issue a raw
    /// `reqwest::get`, and assert both deserialize to identical
    /// `CostReport` values.
    #[tokio::test]
    async fn cost_report_client_and_raw_http_get_should_return_identical_json_when_hitting_same_server(
    ) {
        let tracker = tracker().await;
        let key = SessionKey::new("s1");
        let request_id = crate::cost_metrics::types::RequestId::new();

        tracker
            .record_pending(
                &key,
                request_id,
                crate::session_compaction::CompactionTier::Full,
            )
            .await;
        tracker
            .record_counterfactual(&key, request_id, tc(12000), tc(4000))
            .await
            .expect("session is known");
        tracker
            .record_actual_usage(&key, request_id, tc(4000))
            .await
            .expect("session is known");

        let router = cost_router(tracker);
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        tokio::spawn(async move {
            let _ = axum::serve(listener, router).await;
        });

        let server = format!("http://{addr}");

        let via_client = crate::cost_metrics::client::fetch_cost_report(&server, "s1")
            .await
            .expect("client fetch should succeed");

        let via_raw_get = reqwest::get(format!("{server}/v1/cost/s1"))
            .await
            .expect("raw request should succeed")
            .json::<crate::cost_metrics::report::CostReport>()
            .await
            .expect("raw response should deserialize as CostReport");

        assert_eq!(
            via_client, via_raw_get,
            "fetch_cost_report must not transform the response beyond deserialization"
        );
    }

    fn user_row(uuid: &str, text: &str) -> String {
        format!(
            r#"{{"uuid":"{uuid}","parentUuid":null,"type":"user","timestamp":"2024-01-01T00:00:00Z","message":{{"role":"user","content":"{text}"}}}}"#
        )
    }

    fn assistant_row(uuid: &str, parent: &str, text: &str) -> String {
        format!(
            r#"{{"uuid":"{uuid}","parentUuid":"{parent}","type":"assistant","timestamp":"2024-01-01T00:00:01Z","message":{{"role":"assistant","content":"{text}"}}}}"#
        )
    }

    /// Task 3.3.1c: end-to-end dashboard route test using
    /// `build_with_session_glob` against a fixture directory — must never
    /// use `build()` (which scans real `$HOME/.claude/projects`) for this or
    /// any other filesystem-touching test.
    #[tokio::test]
    async fn dashboard_sessions_route_should_return_fixture_rows_via_build_with_session_glob() {
        let dir = tempfile::tempdir().expect("tempdir should succeed");
        let session_path = dir.path().join("fixture-session.jsonl");
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&session_path).expect("fixture file should create");
            writeln!(f, "{}", user_row("u1", "hello")).expect("write should succeed");
            writeln!(f, "{}", assistant_row("a1", "u1", "hi")).expect("write should succeed");
        }
        let pattern = format!("{}/*.jsonl", dir.path().display());

        let state = CostServerState::build_with_session_glob(&pattern).await;
        let router = dashboard_router(state.session_bi_rx.clone());

        let request = axum::http::Request::builder()
            .uri("/v1/dashboard/sessions")
            .body(axum::body::Body::empty())
            .expect("request should build");
        let response = router.oneshot(request).await.expect("route should respond");
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        let snapshot: crate::claude_code_session::session_bi::SessionBiSnapshot =
            serde_json::from_slice(&body).expect("body should deserialize as SessionBiSnapshot");
        assert_eq!(snapshot.rows.len(), 1);
        assert!(snapshot.parse_failures.is_empty());
    }

    /// Task 3.2.1c: `GET /dashboard` returns the compiled-in HTML shell.
    #[tokio::test]
    async fn dashboard_route_should_return_html_shell() {
        let dir = tempfile::tempdir().expect("tempdir should succeed");
        let pattern = format!("{}/*.jsonl", dir.path().display());
        let state = CostServerState::build_with_session_glob(&pattern).await;
        let router = dashboard_router(state.session_bi_rx.clone());

        let request = axum::http::Request::builder()
            .uri("/dashboard")
            .body(axum::body::Body::empty())
            .expect("request should build");
        let response = router.oneshot(request).await.expect("route should respond");
        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body should read");
        let body = String::from_utf8(body.to_vec()).expect("body should be utf8");
        assert!(body.contains("<html") || body.contains("<!DOCTYPE") || body.contains("<!doctype"));
    }

    /// Task 1.4.4c: `GET /dashboard/context` returns the context-forensics
    /// dashboard's HTML shell, and `GET /dashboard` (the pre-existing
    /// session-BI dashboard) is unaffected by mounting it alongside —
    /// regression guard against `research/features.md`'s route-collision
    /// risk.
    #[tokio::test]
    async fn context_dashboard_route_should_return_200_and_existing_dashboard_route_unaffected() {
        let session_dir = tempfile::tempdir().expect("tempdir should succeed");
        let session_pattern = format!("{}/*.jsonl", session_dir.path().display());
        let context_store_dir = tempfile::tempdir().expect("tempdir should succeed");
        let context_store_path = context_store_dir.path().join("cf.sqlite");

        let state = CostServerState::build_with_session_glob_and_context_store_path(
            &session_pattern,
            &context_store_path,
        )
        .await;
        assert!(
            state.context_store.is_some(),
            "context store should open successfully against a fresh temp path"
        );

        let router = dashboard_router(state.session_bi_rx.clone()).merge(context_router(
            Arc::clone(state.context_store.as_ref().expect("checked above")),
            state.pricing_rx.clone(),
        ));

        let context_response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/dashboard/context")
                    .body(axum::body::Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");
        assert_eq!(context_response.status(), StatusCode::OK);

        let dashboard_response = router
            .oneshot(
                axum::http::Request::builder()
                    .uri("/dashboard")
                    .body(axum::body::Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");
        assert_eq!(dashboard_response.status(), StatusCode::OK);
    }

    /// Task 1.4.4d: `ContextForensicsStore::open` failing (path points at a
    /// file where a directory is expected, mirroring `OmissionCache`'s own
    /// open-failure test simulation) must not prevent the rest of
    /// `CostServerState` from constructing, and the fallback
    /// `context_unavailable_router` must return `503` rather than the
    /// process failing to start.
    #[tokio::test]
    async fn build_should_leave_context_store_none_and_dashboard_still_works_when_context_store_path_invalid(
    ) {
        let session_dir = tempfile::tempdir().expect("tempdir should succeed");
        let session_pattern = format!("{}/*.jsonl", session_dir.path().display());

        // A file where `ContextForensicsStore::open` expects to create a
        // parent directory — `create_dir_all` fails against an existing
        // regular file at that path.
        let blocker_dir = tempfile::tempdir().expect("tempdir should succeed");
        let blocker_file = blocker_dir.path().join("blocker");
        std::fs::write(&blocker_file, b"not a directory").expect("write should succeed");
        let invalid_context_store_path = blocker_file.join("context-forensics.sqlite");

        let state = CostServerState::build_with_session_glob_and_context_store_path(
            &session_pattern,
            &invalid_context_store_path,
        )
        .await;
        assert!(
            state.context_store.is_none(),
            "context store should fail open, not be constructed, against an invalid path"
        );

        let router =
            dashboard_router(state.session_bi_rx.clone()).merge(context_unavailable_router());

        let dashboard_response = router
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/dashboard")
                    .body(axum::body::Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");
        assert_eq!(dashboard_response.status(), StatusCode::OK);

        let context_response = router
            .oneshot(
                axum::http::Request::builder()
                    .uri("/dashboard/context")
                    .body(axum::body::Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("route should respond");
        assert_eq!(context_response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
