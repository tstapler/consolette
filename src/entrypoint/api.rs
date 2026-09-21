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

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::claude_code_session::prune::{
    extract_tool_result_with_map, PruneExecutionReport, PruneReasonBreakdown, PruningPolicy,
};
use crate::claude_code_session::transcript::{
    build_turns, parse_session_file, prune_session_file_with_policy, resolve_session_path,
    ToolNameMap,
};
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
    // A throwaway `ProxyMetrics` instance: this listing-only path never
    // dispatches a real request (only `list_models()`), so it never touches
    // Epic 5.1's resolution counters — no need to thread the live
    // `Router`/`MetricsCollector` in just for this.
    let (providers, _) = build_providers(
        &config,
        std::sync::Arc::new(crate::metrics::ProxyMetrics::new()),
    )
    .await
    .map_err(|e| {
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
/// Returns 400 if the route references an unknown upstream or violates the
/// `model`/`model_family` selector rules (Story 1.2.2), 500 if the config
/// can't be (re)loaded, the override can't be saved, or the new
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
    crate::config::validate_model_selectors(&candidate).map_err(|e| {
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

// ---------------------------------------------------------------------------
// Transcript Memory Pruning Control Plane DTOs & Endpoints
// ---------------------------------------------------------------------------

/// Request payload for `POST /session/prune`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PruneRequest {
    pub session_id: String,
    #[serde(default)]
    pub dry_run: bool,
    #[serde(default)]
    pub policy_override: Option<PruningPolicy>,
}

/// Response payload for `POST /session/prune`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruneResponse {
    pub session_id: String,
    pub rows_evaluated: usize,
    pub rows_pruned: usize,
    pub bytes_freed: usize,
    pub estimated_tokens_saved: usize,
    pub pruned_by_reason: PruneReasonBreakdown,
    pub dry_run: bool,
}

impl From<PruneExecutionReport> for PruneResponse {
    fn from(report: PruneExecutionReport) -> Self {
        Self {
            session_id: report.session_id,
            rows_evaluated: report.rows_evaluated,
            rows_pruned: report.rows_pruned,
            bytes_freed: report.bytes_freed,
            estimated_tokens_saved: report.estimated_tokens_saved,
            pruned_by_reason: report.pruned_by_reason,
            dry_run: report.dry_run,
        }
    }
}

/// Request payload for `POST /session/policy`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PrunePolicyRequest {
    #[serde(default)]
    pub session_id: Option<String>,
    pub policy: PruningPolicy,
}

pub type PolicyRequest = PrunePolicyRequest;

/// Response payload for `POST /session/policy` and `GET /session/policy`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrunePolicyResponse {
    #[serde(default)]
    pub session_id: Option<String>,
    pub policy: PruningPolicy,
}

pub type PolicyResponse = PrunePolicyResponse;

/// Query parameters for `GET /session/policy`.
#[derive(Debug, Clone, Deserialize)]
pub struct PrunePolicyQuery {
    pub session_id: Option<String>,
}

/// Query parameters for `GET /session/prune/stats`.
#[derive(Debug, Clone, Deserialize)]
pub struct PruneStatsQuery {
    pub session_id: Option<String>,
}

/// Response payload for `GET /session/prune/stats`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruneStatsResponse {
    #[serde(default)]
    pub session_id: Option<String>,
    pub total_turns: usize,
    pub total_rows: usize,
    pub rows_pruned: usize,
    pub omission_cache_entries: usize,
    pub active_policy: PruningPolicy,
}

pub type SessionPruningStats = PruneStatsResponse;

/// Endpoint: `POST /session/prune`
///
/// Evaluates multi-criteria pruning policies against a transcript session file on disk.
/// Supports `dry_run: true` mode which computes pruning metrics in-memory without disk or SQLite mutations.
///
/// # Errors
/// Returns 400 Bad Request for invalid session UUIDs or path traversal attempts.
/// Returns 404 Not Found if the transcript file does not exist.
/// Returns 500 Internal Server Error if transcript resolution or pruning pass fails.
pub async fn post_session_prune(
    State(state): State<EntrypointState>,
    Json(payload): Json<PruneRequest>,
) -> Result<Json<PruneResponse>, (StatusCode, Json<Value>)> {
    if uuid::Uuid::parse_str(&payload.session_id).is_err() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": format!("invalid session_id UUID: {}", payload.session_id) })),
        ));
    }

    let path = resolve_session_path(&payload.session_id).map_err(|e| {
        let msg = e.to_string();
        if msg.contains("invalid session_id UUID") || msg.contains("path traversal") {
            (StatusCode::BAD_REQUEST, Json(json!({ "error": msg })))
        } else {
            (StatusCode::NOT_FOUND, Json(json!({ "error": msg })))
        }
    })?;

    let policy = payload.policy_override.clone().unwrap_or_else(|| {
        state
            .pruning_policy_store
            .get_policy(Some(&payload.session_id))
    });

    let (_out_rows, report, _stats) =
        prune_session_file_with_policy(&path, &state.omission_cache, &policy, payload.dry_run)
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(json!({ "error": format!("pruning failed: {e}") })),
                )
            })?;

    Ok(Json(PruneResponse::from(report)))
}

/// Endpoint: `POST /session/policy`
///
/// Updates global or per-session pruning policies in [`PruningPolicyStore`].
///
/// # Errors
/// Returns 400 Bad Request if `session_id` is provided but is not a valid UUID string.
pub async fn post_session_policy(
    State(state): State<EntrypointState>,
    Json(payload): Json<PrunePolicyRequest>,
) -> Result<Json<PrunePolicyResponse>, (StatusCode, Json<Value>)> {
    if let Some(ref sid) = payload.session_id {
        if uuid::Uuid::parse_str(sid).is_err() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid session_id UUID: {sid}") })),
            ));
        }
        state
            .pruning_policy_store
            .set_session_policy(sid, payload.policy.clone());
    } else {
        state
            .pruning_policy_store
            .set_global_policy(payload.policy.clone());
    }

    Ok(Json(PrunePolicyResponse {
        session_id: payload.session_id,
        policy: payload.policy,
    }))
}

/// Endpoint: `GET /session/policy`
///
/// Queries global or per-session pruning policy in [`PruningPolicyStore`].
///
/// # Errors
/// Returns 400 Bad Request if `session_id` is provided but is not a valid UUID string.
pub async fn get_session_policy(
    State(state): State<EntrypointState>,
    Query(query): Query<PrunePolicyQuery>,
) -> Result<Json<PrunePolicyResponse>, (StatusCode, Json<Value>)> {
    if let Some(ref sid) = query.session_id {
        if uuid::Uuid::parse_str(sid).is_err() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid session_id UUID: {sid}") })),
            ));
        }
    }

    let policy = state
        .pruning_policy_store
        .get_policy(query.session_id.as_deref());

    Ok(Json(PrunePolicyResponse {
        session_id: query.session_id,
        policy,
    }))
}

/// Endpoint: `GET /session/prune/stats`
///
/// Returns analytics on transcript turns, total rows, pruned rows, omission cache entries, and active policy.
///
/// # Errors
/// Returns 400 Bad Request if `session_id` is provided but is not a valid UUID string.
/// Returns 404 Not Found if `session_id` is provided but transcript file does not exist.
pub async fn get_session_prune_stats(
    State(state): State<EntrypointState>,
    Query(query): Query<PruneStatsQuery>,
) -> Result<Json<PruneStatsResponse>, (StatusCode, Json<Value>)> {
    let (total_turns, total_rows, rows_pruned) = if let Some(ref sid) = query.session_id {
        if uuid::Uuid::parse_str(sid).is_err() {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({ "error": format!("invalid session_id UUID: {sid}") })),
            ));
        }
        let path = resolve_session_path(sid).map_err(|e| {
            let msg = e.to_string();
            if msg.contains("invalid session_id UUID") || msg.contains("path traversal") {
                (StatusCode::BAD_REQUEST, Json(json!({ "error": msg })))
            } else {
                (StatusCode::NOT_FOUND, Json(json!({ "error": msg })))
            }
        })?;

        let rows = parse_session_file(&path).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to parse session file: {e}") })),
            )
        })?;
        let turns = build_turns(&rows).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to build turns: {e}") })),
            )
        })?;

        let tool_name_map = ToolNameMap::build(&rows);
        let pruned_count = rows
            .iter()
            .filter(|row| {
                if let Some((_, text)) = extract_tool_result_with_map(row, Some(&tool_name_map)) {
                    text.trim_start()
                        .starts_with("[pruned: see read_omitted_content")
                } else {
                    false
                }
            })
            .count();

        (turns.len(), rows.len(), pruned_count)
    } else {
        (0, 0, 0)
    };

    let cache_entries = state
        .omission_cache
        .count_entries(query.session_id.as_deref())
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": format!("failed to count omission cache entries: {e}") })),
            )
        })?;

    let active_policy = state
        .pruning_policy_store
        .get_policy(query.session_id.as_deref());

    Ok(Json(PruneStatsResponse {
        session_id: query.session_id,
        total_turns,
        total_rows,
        rows_pruned,
        omission_cache_entries: cache_entries,
        active_policy,
    }))
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::items_after_statements,
    clippy::similar_names
)]
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
                model_family: None,
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
    async fn post_route_rejects_conflicting_model_selectors() {
        let dir = sample_config_dir();
        write_conf_d(
            dir.path(),
            r#"
[[upstreams]]
name = "anthropic"
kind = "anthropic"

[[upstreams]]
name = "model-gateway-openai"
kind = "openai"
base_url = "https://example.invalid"
"#,
            r#"
[[routes]]
name = "default"
strategy = "fallback"

[[routes.upstreams]]
name = "anthropic"
"#,
        );
        let state = state_for(dir.path()).await;

        let bad = Route {
            name: "default".to_string(),
            strategy: Strategy::Fallback,
            upstreams: vec![RouteUpstreamRef {
                name: "model-gateway-openai".to_string(),
                weight: None,
                model: Some("gpt-5.1".to_string()),
                model_family: Some("gpt-5".to_string()),
            }],
        };

        let err = post_route(State(state), Json(bad)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(
            !RuntimeOverrides::path(dir.path()).exists(),
            "a route violating model selector rules must not be persisted"
        );
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn post_route_rejects_model_family_on_non_openai_upstream() {
        let dir = sample_config_dir();
        let state = state_for(dir.path()).await;

        let bad = Route {
            name: "default".to_string(),
            strategy: Strategy::Fallback,
            upstreams: vec![RouteUpstreamRef {
                name: "anthropic".to_string(),
                weight: None,
                model: None,
                model_family: Some("gpt-5".to_string()),
            }],
        };

        let err = post_route(State(state), Json(bad)).await.unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
        assert!(
            !RuntimeOverrides::path(dir.path()).exists(),
            "a route leaking model_family onto a non-openai upstream must not be persisted"
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
                model_family: None,
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

    fn create_test_session_file(dir: &std::path::Path, session_id: &str) -> std::path::PathBuf {
        let proj_dir = dir.join("proj_1");
        std::fs::create_dir_all(&proj_dir).unwrap();
        let file_path = proj_dir.join(format!("{session_id}.jsonl"));
        let mut f = std::fs::File::create(&file_path).unwrap();
        use std::io::Write;
        let big_content = "x".repeat(2000);
        let l1 = r#"{"type":"user","uuid":"u1","parentUuid":null,"isSidechain":false,"isMeta":false,"message":{"role":"user","content":"hello"}}"#;
        let l2 = r#"{"type":"assistant","uuid":"a1","parentUuid":"u1","isSidechain":false,"isMeta":false,"message":{"role":"assistant","content":[{"type":"text","text":"doing task"}]}}"#;
        let l3 = format!(
            r#"{{"type":"user","uuid":"t1","parentUuid":"a1","isSidechain":false,"isMeta":false,"message":{{"role":"user","content":[{{"type":"tool_result","tool_use_id":"tu1","content":"{big_content}","tool_name":"Bash"}}]}}}}"#
        );
        let l4 = r#"{"type":"user","uuid":"u2","parentUuid":"t1","isSidechain":false,"isMeta":false,"message":{"role":"user","content":"next turn"}}"#;
        let l5 = r#"{"type":"assistant","uuid":"a2","parentUuid":"u2","isSidechain":false,"isMeta":false,"message":{"role":"assistant","content":[{"type":"text","text":"done"}]}}"#;
        let l6 = r#"{"type":"user","uuid":"u3","parentUuid":"a2","isSidechain":false,"isMeta":false,"message":{"role":"user","content":"final turn"}}"#;
        let l7 = r#"{"type":"assistant","uuid":"a3","parentUuid":"u3","isSidechain":false,"isMeta":false,"message":{"role":"assistant","content":[{"type":"text","text":"bye"}]}}"#;
        writeln!(f, "{l1}").unwrap();
        writeln!(f, "{l2}").unwrap();
        writeln!(f, "{l3}").unwrap();
        writeln!(f, "{l4}").unwrap();
        writeln!(f, "{l5}").unwrap();
        writeln!(f, "{l6}").unwrap();
        writeln!(f, "{l7}").unwrap();
        file_path
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn sec_001_path_traversal_and_uuid_validation() {
        // UT-SEC-001 / IT-API-005: Security rejection of path traversal payloads
        let err1 = resolve_session_path("../../../etc/passwd").unwrap_err();
        assert!(err1.to_string().contains("invalid session_id UUID"));

        let err2 = resolve_session_path("invalid-uuid-string").unwrap_err();
        assert!(err2.to_string().contains("invalid session_id UUID"));
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn api_policy_management_endpoints() {
        // IT-API-006 & IT-API-007: POST /session/policy and GET /session/policy
        let dir = sample_config_dir();
        let state = state_for(dir.path()).await;

        // Set global policy
        let global_policy = PruningPolicy {
            default_limit_chars: 4096,
            ..PruningPolicy::default()
        };
        let req_global = PrunePolicyRequest {
            session_id: None,
            policy: global_policy.clone(),
        };
        let Json(res_global) = post_session_policy(State(state.clone()), Json(req_global))
            .await
            .unwrap();
        assert_eq!(res_global.policy.default_limit_chars, 4096);

        // Query global policy
        let Json(get_global) = get_session_policy(
            State(state.clone()),
            Query(PrunePolicyQuery { session_id: None }),
        )
        .await
        .unwrap();
        assert_eq!(get_global.policy.default_limit_chars, 4096);

        // Set session policy override
        let sid = uuid::Uuid::new_v4().to_string();
        let session_policy = PruningPolicy {
            default_limit_chars: 8192,
            ..PruningPolicy::default()
        };
        let req_sess = PrunePolicyRequest {
            session_id: Some(sid.clone()),
            policy: session_policy.clone(),
        };
        let Json(res_sess) = post_session_policy(State(state.clone()), Json(req_sess))
            .await
            .unwrap();
        assert_eq!(res_sess.session_id, Some(sid.clone()));
        assert_eq!(res_sess.policy.default_limit_chars, 8192);

        // Query session policy
        let Json(get_sess) = get_session_policy(
            State(state.clone()),
            Query(PrunePolicyQuery {
                session_id: Some(sid.clone()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(get_sess.policy.default_limit_chars, 8192);

        // Reject invalid UUID in policy request
        let bogus_req = PrunePolicyRequest {
            session_id: Some("not-a-uuid".to_string()),
            policy: PruningPolicy::default(),
        };
        let err = post_session_policy(State(state), Json(bogus_req))
            .await
            .unwrap_err();
        assert_eq!(err.0, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn api_prune_dry_run_and_live_mode() {
        // IT-API-001, IT-API-002, IT-API-003: POST /session/prune (dry-run & live & override)
        let config_dir = sample_config_dir();
        let state = state_for(config_dir.path()).await;

        let temp_proj = tempfile::tempdir().unwrap();
        std::env::set_var("CONSOLETTE_PROJECTS_DIR", temp_proj.path());

        let sid = uuid::Uuid::new_v4().to_string();
        let file_path = create_test_session_file(temp_proj.path(), &sid);

        // 1. Dry-run mode
        let req_dry = PruneRequest {
            session_id: sid.clone(),
            dry_run: true,
            policy_override: None,
        };
        let Json(res_dry) = post_session_prune(State(state.clone()), Json(req_dry))
            .await
            .unwrap();
        assert_eq!(res_dry.session_id, sid);
        assert!(res_dry.dry_run);
        assert!(res_dry.rows_pruned > 0);

        // Verify file was NOT modified in dry-run mode
        let file_content_dry = std::fs::read_to_string(&file_path).unwrap();
        assert!(file_content_dry.contains("x".repeat(2000).as_str()));

        // 2. Live mode
        let req_live = PruneRequest {
            session_id: sid.clone(),
            dry_run: false,
            policy_override: None,
        };
        let Json(res_live) = post_session_prune(State(state.clone()), Json(req_live))
            .await
            .unwrap();
        assert_eq!(res_live.session_id, sid);
        assert!(!res_live.dry_run);
        assert!(res_live.rows_pruned > 0);

        // Verify file WAS modified in live mode (placeholder inserted)
        let file_content_live = std::fs::read_to_string(&file_path).unwrap();
        assert!(file_content_live.contains("[pruned: see read_omitted_content"));

        // 3. Stats endpoint (IT-API-008)
        let Json(stats) = get_session_prune_stats(
            State(state.clone()),
            Query(PruneStatsQuery {
                session_id: Some(sid.clone()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(stats.session_id, Some(sid.clone()));
        assert_eq!(stats.rows_pruned, 1);
        assert_eq!(stats.omission_cache_entries, 1);
        assert!(stats.total_turns > 0);
        assert!(stats.total_rows > 0);
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn api_prune_rejects_invalid_uuid_and_missing_file() {
        let config_dir = sample_config_dir();
        let state = state_for(config_dir.path()).await;

        // Invalid UUID payload -> 400
        let req_bad = PruneRequest {
            session_id: "../../../etc/passwd".to_string(),
            dry_run: true,
            policy_override: None,
        };
        let err_bad = post_session_prune(State(state.clone()), Json(req_bad))
            .await
            .unwrap_err();
        assert_eq!(err_bad.0, StatusCode::BAD_REQUEST);

        // Missing session UUID -> 404
        let missing_uuid = uuid::Uuid::new_v4().to_string();
        let req_missing = PruneRequest {
            session_id: missing_uuid,
            dry_run: true,
            policy_override: None,
        };
        let err_missing = post_session_prune(State(state), Json(req_missing))
            .await
            .unwrap_err();
        assert_eq!(err_missing.0, StatusCode::NOT_FOUND);
    }
}
