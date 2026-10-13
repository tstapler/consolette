//! `GET /api/model-catalog` and `POST /api/model-catalog/refresh` (Story 3.1.4).
//!
//! GET is a pure read of the cached catalog. POST only schedules background
//! work and answers at once; completion arrives as `ModelCatalogChanged`.

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::EntrypointState;
use crate::metrics::ids::UpstreamName;
use crate::routing::model_catalog::{
    config_warnings, CatalogSnapshot, ConfigWarning, RefreshAck, Target, Trigger,
};

/// Snapshot plus the pinned-model warnings; the body of both endpoints.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CatalogEnvelope {
    #[serde(flatten)]
    snapshot: CatalogSnapshot,
    config_warnings: Vec<ConfigWarning>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RefreshEnvelope {
    refreshing: bool,
    coalesced: bool,
    retry_after_secs: Option<u64>,
    #[serde(flatten)]
    catalog: CatalogEnvelope,
}

#[derive(Debug, Default, Deserialize)]
pub struct RefreshQuery {
    upstream: Option<String>,
    #[serde(rename = "ifStale", default)]
    if_stale: bool,
}

type ApiError = (StatusCode, Json<Value>);

fn envelope(state: &EntrypointState) -> CatalogEnvelope {
    let snapshot = state.catalog.snapshot();
    let warnings = match crate::config::load(&state.config_dir) {
        Ok(config) => config_warnings(&config, &snapshot),
        Err(error) => {
            tracing::debug!(%error, "config unreadable; skipping catalog config warnings");
            Vec::new()
        }
    };
    CatalogEnvelope {
        snapshot,
        config_warnings: warnings,
    }
}

fn to_json<T: Serialize>(value: &T) -> Result<Json<Value>, ApiError> {
    serde_json::to_value(value).map(Json).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
    })
}

/// Cached per-upstream model lists with refresh age, error and a `refreshing`
/// flag per upstream, plus warnings for pinned models an upstream does not list.
///
/// # Errors
///
/// Returns 500 only if the response cannot be serialized.
pub async fn get_model_catalog(
    State(state): State<EntrypointState>,
) -> Result<Json<Value>, ApiError> {
    to_json(&envelope(&state))
}

/// Requests a background refresh and returns the cached snapshot immediately.
/// `?upstream=<name>` targets one upstream; `?ifStale=true` refreshes only
/// loading or old entries (subject to the retry floor and auth backoff).
///
/// # Errors
///
/// Returns 404 `{"error":"unknown upstream"}` for a name the router has no
/// provider for, and 500 only if the response cannot be serialized.
pub async fn post_model_catalog_refresh(
    State(state): State<EntrypointState>,
    Query(query): Query<RefreshQuery>,
) -> Result<Json<Value>, ApiError> {
    let providers = state.dispatch_router.load().providers_named();
    let wanted = query.upstream.as_deref().map(UpstreamName::from);
    let target = wanted.as_ref().map_or(Target::All, Target::One);
    let trigger = if query.if_stale {
        Trigger::IfStale
    } else {
        Trigger::Manual
    };
    let started = state
        .catalog
        .request_refresh(&providers, target, trigger)
        .map_err(|_| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({ "error": "unknown upstream" })),
            )
        })?;
    let RefreshAck {
        refreshing,
        coalesced,
        retry_after_secs,
    } = started.ack;
    to_json(&RefreshEnvelope {
        refreshing,
        coalesced,
        retry_after_secs,
        catalog: envelope(&state),
    })
}
