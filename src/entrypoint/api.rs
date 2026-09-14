//! Web control-panel API: `GET /api/models` (what's available per upstream),
//! `GET`/`POST /api/route` (current route; change model override, route
//! strategy, and upstream membership/weights), and
//! `GET /api/sessions`/`GET`/`POST`/`DELETE /api/sessions/{id}/route` (pin
//! one session to a specific upstream/model, ahead of the global route).
//!
//! `POST /api/route` persists the new route into
//! `<config_dir>/runtime-overrides.toml` (surviving a restart) and hot-swaps
//! the live `DispatchRouter` via `EntrypointState::dispatch_router`'s
//! `ArcSwap`, so the change takes effect immediately for new requests.
//!
//! Session pins (`/api/sessions/{id}/route`) are the opposite on
//! persistence: they live only in `EntrypointState::session_overrides`
//! (in-memory), scoped to one session's lifetime rather than the process's
//! on-disk config — see `routing::session_overrides` for why, and for the
//! caveat on how a session id is derived from a request.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde_json::{json, Value};

use crate::config::schema::Route;
use crate::config::RuntimeOverrides;
use crate::routing::router::{build_providers, Router as DispatchRouter};
use crate::routing::session_overrides::SessionOverride;

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
    let (providers, _) = build_providers(&config).await.map_err(|e| {
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
        })?
        .with_session_overrides(std::sync::Arc::clone(&state.session_overrides))
        .with_capability(std::sync::Arc::clone(&state.capability));
    state.dispatch_router.store(std::sync::Arc::new(new_router));

    // Re-evaluate admission against the new pins without blocking the
    // response: a newly pinned model that cannot emit tool calls is
    // excluded from dispatch within about a minute.
    let router_for_eval = state.dispatch_router.load_full();
    tokio::spawn(async move {
        crate::routing::capability::evaluate_round(&router_for_eval).await;
    });

    Ok(Json(route))
}

/// Every session id seen in the last 100 requests (the same ring buffer
/// `GET /requests/{id}` reads from), newest-seen first, each annotated with
/// its current pin if one is set. A session with no `metadata.user_id`
/// never appears here (see `routing::session_overrides`), so it also can't
/// be pinned.
pub async fn get_sessions(State(state): State<EntrypointState>) -> Json<Value> {
    let mut seen = std::collections::HashSet::new();
    let mut sessions = Vec::new();
    for req in state.metrics.get_recent_requests(100) {
        let Some(session_id) = req.session_id else {
            continue;
        };
        if !seen.insert(session_id.clone()) {
            continue;
        }
        let over = state.session_overrides.get(&session_id);
        sessions.push(json!({
            "session_id": session_id,
            "last_seen": req.timestamp,
            "override": over,
        }));
    }
    Json(json!({ "sessions": sessions }))
}

/// The pin currently set for one session, or 404 if none is set.
///
/// # Errors
///
/// Returns 404 if no override is currently set for `session_id`.
pub async fn get_session_route(
    State(state): State<EntrypointState>,
    Path(session_id): Path<String>,
) -> Result<Json<SessionOverride>, (StatusCode, Json<Value>)> {
    state.session_overrides.get(&session_id).map(Json).ok_or((
        StatusCode::NOT_FOUND,
        Json(json!({ "error": "no override set for this session" })),
    ))
}

/// Pins one session to a specific upstream (and optionally a model),
/// overriding the global route for just that session's requests going
/// forward. Takes effect immediately — no router rebuild needed, since
/// `Router::dispatch` consults the same `SessionOverrideStore` on every
/// call.
///
/// # Errors
///
/// Returns 400 if `upstream` doesn't match any currently configured
/// upstream (checked against the on-disk config, not just the active
/// route, so a session can be pinned to an upstream outside it).
pub async fn post_session_route(
    State(state): State<EntrypointState>,
    Path(session_id): Path<String>,
    Json(over): Json<SessionOverride>,
) -> Result<Json<SessionOverride>, (StatusCode, Json<Value>)> {
    let config = crate::config::load(&state.config_dir).map_err(|e| config_load_error(&e))?;
    if !config.upstreams.iter().any(|u| u.name == over.upstream) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("unknown upstream \"{}\"", over.upstream) })),
        ));
    }
    state.session_overrides.set(session_id, over.clone());
    Ok(Json(over))
}

/// Clears a session's pin, if one was set. Idempotent: 204 either way.
pub async fn delete_session_route(
    State(state): State<EntrypointState>,
    Path(session_id): Path<String>,
) -> StatusCode {
    state.session_overrides.clear(&session_id);
    StatusCode::NO_CONTENT
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
