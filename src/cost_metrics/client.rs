//! `reqwest` client of `consolette serve-cost`'s `GET /v1/cost/{session_key}`
//! route — the CLI (`cost-report`, Epic 3.1) side of the Remote Facade
//! pattern from plan.md's repair iteration 1. This module never constructs a
//! `CostTracker` or `SessionCompactionPipeline`: it only speaks HTTP to
//! whatever `serve-cost` process is running, so "the CLI and HTTP surfaces
//! agree" is a structural property of parsing the exact same response body,
//! not two independent implementations sharing a type definition.

use reqwest::StatusCode;

use crate::cost_metrics::report::CostReport;

/// The three outcomes `cost-report`'s acceptance criteria distinguish:
/// the server being unreachable, the session being unknown to a reachable
/// server, and everything else (unexpected status, unparseable body).
#[derive(Debug, thiserror::Error)]
pub enum CostClientError {
    /// Could not even connect to `server` (connection refused, DNS failure,
    /// etc.) — distinct from a `404`, which means the server answered but
    /// has no record of the session.
    #[error("could not reach cost server: {0}")]
    Unreachable(#[source] reqwest::Error),
    /// The server responded `404 {"error":"session_not_found", ...}`.
    #[error("no session found for key {session_key:?}")]
    NotFound { session_key: String },
    /// Any other non-2xx status or a response body that didn't deserialize
    /// as a [`CostReport`].
    #[error("cost server request failed: {0}")]
    Other(#[source] reqwest::Error),
}

/// `GET {server}/v1/cost/{session}`, deserializing a `200` body into a
/// [`CostReport`]. `server` is a full base URL (e.g.
/// `http://127.0.0.1:8787`), no trailing slash required.
///
/// # Errors
///
/// Returns [`CostClientError::Unreachable`] when the connection itself
/// fails, [`CostClientError::NotFound`] on a `404`, and
/// [`CostClientError::Other`] for any other non-2xx status or a body that
/// fails to deserialize as a [`CostReport`].
#[allow(clippy::expect_used)]
pub async fn fetch_cost_report(server: &str, session: &str) -> Result<CostReport, CostClientError> {
    let url = format!("{}/v1/cost/{}", server.trim_end_matches('/'), session);

    let response = reqwest::get(&url).await.map_err(|err| {
        if err.is_connect() {
            CostClientError::Unreachable(err)
        } else {
            CostClientError::Other(err)
        }
    })?;

    match response.status() {
        StatusCode::OK => response
            .json::<CostReport>()
            .await
            .map_err(CostClientError::Other),
        StatusCode::NOT_FOUND => Err(CostClientError::NotFound {
            session_key: session.to_string(),
        }),
        _ => Err(CostClientError::Other(
            response
                .error_for_status()
                .expect_err("route returns only 200/404 today; any other status is an error"),
        )),
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use axum::extract::{Path, State};
    use axum::http::StatusCode as AxumStatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::{Json, Router};
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;

    /// Minimal hand-rolled `GET /v1/cost/{session_key}` double — the same
    /// "no `wiremock`, use `axum`+`tokio` directly" approach as Task 1.2.2c's
    /// `test_support::MockServer`, sized for this module's own route rather
    /// than reusing that struct (which is hardcoded to
    /// `POST /v1/messages/count_tokens`, Epic 1.2's route, not this one).
    async fn start_mock_cost_server(response: serde_json::Value, status: AxumStatusCode) -> String {
        #[derive(Clone)]
        struct MockState {
            response: serde_json::Value,
            status: AxumStatusCode,
        }

        async fn handler(
            State(state): State<std::sync::Arc<MockState>>,
            Path(_session_key): Path<String>,
        ) -> impl IntoResponse {
            (state.status, Json(state.response.clone()))
        }

        let state = std::sync::Arc::new(MockState { response, status });
        let app = Router::new()
            .route("/v1/cost/{session_key}", get(handler))
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock server bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        format!("http://{addr}")
    }

    #[tokio::test]
    async fn fetch_cost_report_should_deserialize_report_when_server_returns_200() {
        let body = json!({
            "session_key": "s1",
            "actual_tokens": 4000,
            "actual_source": "Exact",
            "actual_cost_usd": 0.01,
            "counterfactual_tokens": 12000,
            "counterfactual_source": { "Estimated": { "via": "AnthropicCountTokensApi" } },
            "compacted_tokens": 4200,
            "tokens_saved": 7800,
            "estimated_cost_saved_usd": 0.02,
            "pricing_source": "Static",
            "pending_count": 0,
            "abandoned_count": 0,
            "by_tier": []
        });
        let server = start_mock_cost_server(body, AxumStatusCode::OK).await;

        let report = fetch_cost_report(&server, "s1")
            .await
            .expect("200 with a valid body should deserialize");

        assert_eq!(report.session_key, "s1");
        assert_eq!(report.tokens_saved, Some(7800));
    }

    #[tokio::test]
    async fn fetch_cost_report_should_return_not_found_error_when_server_returns_404() {
        let body = json!({"error": "session_not_found", "session_key": "ghost"});
        let server = start_mock_cost_server(body, AxumStatusCode::NOT_FOUND).await;

        let err = fetch_cost_report(&server, "ghost")
            .await
            .expect_err("404 should surface as NotFound");

        match err {
            CostClientError::NotFound { session_key } => assert_eq!(session_key, "ghost"),
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn fetch_cost_report_should_return_unreachable_error_when_connection_refused() {
        // Bind an ephemeral port, capture its address, then drop the
        // listener immediately — nothing is listening there anymore, so a
        // request to it is a connection refused, not a timeout.
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind should succeed");
        let addr = listener.local_addr().expect("local_addr should succeed");
        drop(listener);

        let server = format!("http://{addr}");
        let err = fetch_cost_report(&server, "s1")
            .await
            .expect_err("connection refused should surface as Unreachable");

        match err {
            CostClientError::Unreachable(_) => {}
            other => panic!("expected Unreachable, got {other:?}"),
        }
    }
}
