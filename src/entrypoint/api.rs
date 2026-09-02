//! Web control-panel API: `GET /api/models` (what's available per upstream)
//! and `GET`/`POST /api/route` (current route; change model override,
//! route strategy, and upstream membership/weights).
//!
//! `POST /api/route` persists the new route into
//! `<config_dir>/runtime-overrides.toml` (surviving a restart) and hot-swaps
//! the live `DispatchRouter` via `EntrypointState::dispatch_router`'s
//! `ArcSwap`, so the change takes effect immediately for new requests.

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use crate::config::schema::Route;
use crate::config::RuntimeOverrides;
use crate::routing::router::{build_providers, Router as DispatchRouter};

use super::EntrypointState;

fn config_load_error(e: &crate::config::ConfigError) -> (StatusCode, Json<Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({ "error": e.to_string() })),
    )
}

/// Lists every configured upstream's live model catalog, so the control
/// panel can offer real model ids instead of free text. Best-effort per
/// upstream: one upstream's `list_models` failure doesn't fail the whole
/// response, it just reports that upstream's error inline.
///
/// # Errors
///
/// Returns 500 if the on-disk config fails to load or a `Provider` fails
/// to construct for a configured upstream.
pub async fn get_models(
    State(state): State<EntrypointState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let config = crate::config::load(&state.config_dir).map_err(|e| config_load_error(&e))?;
    let providers = build_providers(&config).await.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": e.to_string() })),
        )
    })?;

    let mut upstreams = serde_json::Map::new();
    for (name, provider) in &providers {
        let entry = match provider.list_models().await {
            Ok(models) => json!(models
                .into_iter()
                .map(|m| json!({ "id": m.id, "owned_by": m.owned_by }))
                .collect::<Vec<_>>()),
            Err(e) => json!({ "error": e.to_string() }),
        };
        upstreams.insert(name.clone(), entry);
    }

    Ok(Json(Value::Object(upstreams)))
}

/// The active route, as currently loaded from conf.d + runtime overrides.
///
/// # Errors
///
/// Returns 500 if the on-disk config fails to load, or 404 if no route is
/// configured.
pub async fn get_route(
    State(state): State<EntrypointState>,
) -> Result<Json<Route>, (StatusCode, Json<Value>)> {
    let config = crate::config::load(&state.config_dir).map_err(|e| config_load_error(&e))?;
    config.routes.into_iter().next().map(Json).ok_or((
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "no route configured" })),
    ))
}

/// Replaces the active route: validates the proposed route's upstream
/// references against the currently configured upstreams, persists it to
/// `runtime-overrides.toml`, then hot-swaps the live dispatch router.
///
/// # Errors
///
/// Returns 400 if the route references an unknown upstream, 500 if the
/// config can't be (re)loaded, the override can't be saved, or the new
/// `DispatchRouter` can't be built (e.g. a provider fails to initialize).
pub async fn post_route(
    State(state): State<EntrypointState>,
    Json(route): Json<Route>,
) -> Result<Json<Route>, (StatusCode, Json<Value>)> {
    let mut candidate =
        crate::config::load(&state.config_dir).map_err(|e| config_load_error(&e))?;
    let overrides = RuntimeOverrides {
        route: Some(route.clone()),
    };
    overrides.apply(&mut candidate);
    crate::config::validate_references(&candidate).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": e.to_string() })),
        )
    })?;

    overrides.save(&state.config_dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({ "error": format!("failed to persist runtime overrides: {e}") })),
        )
    })?;

    let new_router = DispatchRouter::from_config(&candidate, std::sync::Arc::clone(&state.metrics))
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to rebuild router: {e}") })),
            )
        })?;
    state.dispatch_router.store(std::sync::Arc::new(new_router));

    Ok(Json(route))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::{RouteUpstreamRef, Strategy};

    #[allow(clippy::unwrap_used)]
    fn write_conf_d(dir: &std::path::Path, upstreams: &str, route: &str) {
        let conf_d = dir.join("conf.d");
        std::fs::create_dir_all(&conf_d).unwrap();
        std::fs::write(conf_d.join("00-upstreams.toml"), upstreams).unwrap();
        std::fs::write(conf_d.join("10-routing.toml"), route).unwrap();
    }

    #[allow(clippy::unwrap_used)]
    fn sample_config_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        write_conf_d(
            dir.path(),
            r#"
[[upstreams]]
name = "anthropic"
kind = "anthropic"

[[upstreams]]
name = "bedrock"
kind = "bedrock"
"#,
            r#"
[[routes]]
name = "default"
strategy = "fallback"

[[routes.upstreams]]
name = "anthropic"
"#,
        );
        dir
    }

    #[allow(clippy::unwrap_used)]
    async fn state_for(config_dir: &std::path::Path) -> EntrypointState {
        let config = crate::config::load(config_dir).unwrap();
        EntrypointState::build(&config, config_dir).await.unwrap()
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn get_route_reflects_conf_d_defaults() {
        let dir = sample_config_dir();
        let state = state_for(dir.path()).await;

        let Json(route) = get_route(State(state)).await.unwrap();
        assert_eq!(route.name, "default");
        assert_eq!(route.strategy, Strategy::Fallback);
        assert_eq!(route.upstreams.len(), 1);
        assert_eq!(route.upstreams[0].name, "anthropic");
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn post_route_rejects_unknown_upstream() {
        let dir = sample_config_dir();
        let state = state_for(dir.path()).await;

        let bogus = Route {
            name: "default".to_string(),
            strategy: Strategy::Fallback,
            upstreams: vec![RouteUpstreamRef {
                name: "does-not-exist".to_string(),
                weight: None,
                model: None,
            }],
        };

        let err = post_route(State(state), Json(bogus)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(
            !RuntimeOverrides::path(dir.path()).exists(),
            "an invalid route must not be persisted"
        );
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn post_route_persists_and_hot_swaps() {
        let dir = sample_config_dir();
        let state = state_for(dir.path()).await;

        let new_route = Route {
            name: "default".to_string(),
            strategy: Strategy::Weighted,
            upstreams: vec![RouteUpstreamRef {
                name: "bedrock".to_string(),
                weight: Some(1.0),
                model: Some("pinned-model".to_string()),
            }],
        };

        let Json(applied) = post_route(State(state.clone()), Json(new_route.clone()))
            .await
            .unwrap();
        assert_eq!(applied, new_route);

        // Persisted to disk.
        let persisted = RuntimeOverrides::load(dir.path()).unwrap();
        assert_eq!(persisted.route, Some(new_route.clone()));

        // A subsequent GET (re-reading from disk) reflects the change.
        let Json(fetched) = get_route(State(state.clone())).await.unwrap();
        assert_eq!(fetched, new_route);

        // The live router was hot-swapped, not just the on-disk config.
        let live = state.dispatch_router.load();
        assert_eq!(live.candidate_names(), vec!["bedrock".to_string()]);
    }
}
