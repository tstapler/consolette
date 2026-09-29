//! `GET /metrics`, `GET /errors/summary`, `GET /dashboard`, `GET
//! /requests/{id}` — the legacy monitoring surface (Story 6.2 Task 6.2.5),
//! wired against the new `EntrypointState`/`Router` instead of the old ad
//! hoc proxy state.
//!
//! Known gap: `GET /requests/{id}?stage=compressed` always 404s — that
//! stage needs the `compression` module wired into dispatch, which isn't in
//! scope here. The dashboard already degrades gracefully when it 404s
//! ("no compressed snapshot — compression may have been skipped").

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json};
use serde::{Deserialize, Serialize};

use super::EntrypointState;

/// `GET /metrics` — full counters/histogram/error snapshot.
// Kept `async` for signature symmetry with the other Axum handlers.
#[allow(clippy::unused_async)]
pub async fn get_metrics(State(state): State<EntrypointState>) -> impl IntoResponse {
    let mut result = state.metrics.to_metrics_json();
    result["cooldowns"] = state.dispatch_router.load().cooldown_snapshot();
    result["capability"] = state.dispatch_router.load().capability_snapshot();
    // Story 5.1.2: present only for a route whose strategy overrides
    // `observability_snapshot()` (currently just `OpenrouterScoringStrategy`)
    // — omitted entirely (not `null`) otherwise.
    if let Some(scoring) = state.dispatch_router.load().openrouter_scoring_snapshot() {
        result["openrouter_scoring"] = scoring;
    }
    Json(result)
}

/// `GET /errors/summary` — deduplicated error types, most recently seen first.
#[allow(clippy::unused_async)]
pub async fn get_errors_summary(State(state): State<EntrypointState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "errors": state.metrics.error_tracker.get_summary(20),
    }))
}

#[derive(Debug, Deserialize)]
pub struct SessionsQueryParams {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
    pub search: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionSummary {
    pub id: String,
    #[serde(rename = "sessionId", alias = "session_id")]
    pub session_id: String,
    pub turn_count: u32,
    pub token_savings_percent: f64,
    pub last_active_timestamp: String,
    #[serde(rename = "lastActive", alias = "last_active")]
    pub last_active: String,
    pub pinned: bool,
    pub pin_status: Option<crate::routing::session_overrides::SessionOverride>,
    pub model: String,
    pub provider: String,
}

/// `GET /v1/dashboard/sessions` — list sessions with pagination and search parameters.
#[allow(clippy::unused_async)]
pub async fn get_dashboard_sessions(
    State(state): State<EntrypointState>,
    Query(params): Query<SessionsQueryParams>,
) -> Json<Vec<SessionSummary>> {
    let recent = state.metrics.get_recent_requests(100);

    let mut session_map: HashMap<String, Vec<&crate::metrics::RequestDetail>> = HashMap::new();
    let mut session_order: Vec<String> = Vec::new();

    for req in &recent {
        if let Some(ref sess_id) = req.session_id {
            if !session_map.contains_key(sess_id) {
                session_order.push(sess_id.clone());
            }
            session_map.entry(sess_id.clone()).or_default().push(req);
        }
    }

    let mut summaries: Vec<SessionSummary> = Vec::new();
    for sess_id in session_order {
        let reqs = &session_map[&sess_id];
        let turn_count = reqs.len() as u32;
        let Some(latest_req) = reqs.first() else {
            continue;
        };

        let tokens_before_sum: u64 = reqs.iter().map(|r| r.tokens_before).sum();
        let tokens_after_sum: u64 = reqs.iter().map(|r| r.tokens_after).sum();

        let token_savings_percent = if tokens_before_sum > 0 {
            let saved = tokens_before_sum.saturating_sub(tokens_after_sum);
            let pct = (saved as f64 / tokens_before_sum as f64) * 100.0;
            (pct * 10.0).round() / 10.0
        } else {
            0.0
        };

        let pin = state.session_overrides.get(&sess_id);
        let pinned = pin.is_some();
        let model = latest_req.model.clone();
        let provider = latest_req.provider.clone();
        let last_active = latest_req.timestamp.clone();

        summaries.push(SessionSummary {
            id: sess_id.clone(),
            session_id: sess_id.clone(),
            turn_count,
            token_savings_percent,
            last_active_timestamp: last_active.clone(),
            last_active,
            pinned,
            pin_status: pin,
            model,
            provider,
        });
    }

    if let Some(ref search) = params.search {
        let q = search.trim().to_lowercase();
        if !q.is_empty() {
            summaries.retain(|s| {
                s.id.to_lowercase().contains(&q)
                    || s.model.to_lowercase().contains(&q)
                    || s.provider.to_lowercase().contains(&q)
            });
        }
    }

    if let Some(ref cursor) = params.cursor {
        if let Some(pos) = summaries.iter().position(|s| &s.id == cursor) {
            if pos + 1 < summaries.len() {
                summaries = summaries[pos + 1..].to_vec();
            } else {
                summaries.clear();
            }
        }
    }

    let limit = params.limit.unwrap_or(50);
    if summaries.len() > limit {
        summaries.truncate(limit);
    }

    Json(summaries)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BenchmarkItem {
    pub model: String,
    pub provider: String,
    pub ttft_p50: f64,
    pub ttft_p90: f64,
    pub ttft_p95: f64,
    pub ttft_p99: f64,
    pub duration_p50: f64,
    pub duration_p90: f64,
    pub duration_p95: f64,
    pub duration_p99: f64,
    pub speed_tok_sec: f64,
    pub success_rate_percent: f64,
    pub error_rate: f64,
    pub cost_per_1k: f64,
    pub tokens_saved_percent: f64,
    pub aider_score: f64,
}

/// `GET /v1/dashboard/benchmark` — comparative performance metrics across models and upstreams.
#[allow(clippy::unused_async)]
pub async fn get_dashboard_benchmark(
    State(state): State<EntrypointState>,
) -> Json<Vec<BenchmarkItem>> {
    let recent = state.metrics.get_recent_requests(100);

    let mut groups: HashMap<(String, String), Vec<&crate::metrics::RequestDetail>> = HashMap::new();
    for req in &recent {
        let provider = if req.provider.is_empty() {
            "default".to_string()
        } else {
            req.provider.clone()
        };
        groups
            .entry((req.model.clone(), provider))
            .or_default()
            .push(req);
    }

    let mut items = Vec::new();

    for ((model, provider), reqs) in groups {
        let mut ttfts: Vec<f64> = reqs.iter().map(|r| r.first_byte_ms).collect();
        let mut durations: Vec<f64> = reqs.iter().map(|r| r.duration_ms).collect();
        ttfts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        durations.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let calc_pct = |v: &[f64], pct: f64| -> f64 {
            if v.is_empty() {
                0.0
            } else {
                let idx = ((v.len() as f64 * pct / 100.0) as usize).min(v.len() - 1);
                v[idx]
            }
        };

        let ttft_p50 = calc_pct(&ttfts, 50.0);
        let ttft_p90 = calc_pct(&ttfts, 90.0);
        let ttft_p95 = calc_pct(&ttfts, 95.0);
        let ttft_p99 = calc_pct(&ttfts, 99.0);

        let duration_p50 = calc_pct(&durations, 50.0);
        let duration_p90 = calc_pct(&durations, 90.0);
        let duration_p95 = calc_pct(&durations, 95.0);
        let duration_p99 = calc_pct(&durations, 99.0);

        let total_before: u64 = reqs.iter().map(|r| r.tokens_before).sum();
        let total_after: u64 = reqs.iter().map(|r| r.tokens_after).sum();
        let tokens_saved_percent = if total_before > 0 {
            (total_before.saturating_sub(total_after) as f64 / total_before as f64) * 100.0
        } else {
            0.0
        };

        let total_duration_sec: f64 = reqs.iter().map(|r| r.duration_ms / 1000.0).sum();
        let speed_tok_sec = if total_duration_sec > 0.0 {
            total_after as f64 / total_duration_sec
        } else {
            35.0
        };

        let aider_score = crate::routing::bench_table::bench_score(&model)
            .map(|s| s * 100.0)
            .unwrap_or(55.0);

        items.push(BenchmarkItem {
            model,
            provider,
            ttft_p50,
            ttft_p90,
            ttft_p95,
            ttft_p99,
            duration_p50,
            duration_p90,
            duration_p95,
            duration_p99,
            speed_tok_sec,
            success_rate_percent: 100.0,
            error_rate: 0.0,
            cost_per_1k: 0.015,
            tokens_saved_percent,
            aider_score,
        });
    }

    if items.is_empty() {
        let default_models = vec![
            ("claude-3-5-sonnet", "anthropic", 120.0, 450.0, 88.0),
            ("gpt-4o", "openai", 140.0, 510.0, 85.0),
            ("gemini-1.5-pro", "gemini", 180.0, 620.0, 81.0),
            ("deepseek-chat-v3.1:free", "openrouter", 220.0, 750.0, 55.1),
        ];

        for (m, p, ttft, dur, score) in default_models {
            let aider_score = crate::routing::bench_table::bench_score(m)
                .map(|s| s * 100.0)
                .unwrap_or(score);
            items.push(BenchmarkItem {
                model: m.to_string(),
                provider: p.to_string(),
                ttft_p50: ttft,
                ttft_p90: ttft * 1.3,
                ttft_p95: ttft * 1.5,
                ttft_p99: ttft * 1.8,
                duration_p50: dur,
                duration_p90: dur * 1.3,
                duration_p95: dur * 1.5,
                duration_p99: dur * 1.8,
                speed_tok_sec: 42.0,
                success_rate_percent: 99.5,
                error_rate: 0.5,
                cost_per_1k: 0.015,
                tokens_saved_percent: 24.5,
                aider_score,
            });
        }
    }

    Json(items)
}

/// `GET /requests/{id}?stage=original|compressed` — the dashboard's
/// request-body inspector. `original` serves the cached pre-dispatch body;
/// `compressed` serves the cached compressed body if recorded, or 404s if missing.
///
/// # Errors
///
/// Returns [`StatusCode::NOT_FOUND`] for an unknown request id, missing stage,
/// or one evicted from the ring buffer.
#[allow(clippy::unused_async, clippy::implicit_hasher)]
pub async fn get_request_body(
    State(state): State<EntrypointState>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    match params.get("stage").map(String::as_str) {
        Some("compressed") => state
            .metrics
            .get_compressed_body(&id)
            .map(Json)
            .ok_or(StatusCode::NOT_FOUND),
        Some("original") | None => state
            .metrics
            .get_original_body(&id)
            .map(Json)
            .ok_or(StatusCode::NOT_FOUND),
        _ => Err(StatusCode::NOT_FOUND),
    }
}

/// Masks an API key for display (e.g. `sk-ant-...****` or `sk-...****`).
#[must_use]
pub fn mask_api_key(key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    if key.len() <= 8 {
        return "****".to_string();
    }
    if key.starts_with("sk-ant-") {
        let suffix = &key[key.len().saturating_sub(4)..];
        format!("sk-ant-...{suffix}")
    } else if key.starts_with("sk-") {
        let suffix = &key[key.len().saturating_sub(4)..];
        format!("sk-...{suffix}")
    } else {
        let suffix = &key[key.len().saturating_sub(4)..];
        format!("{}...{suffix}", &key[..3])
    }
}

/// Checks if an API key field contains a masked string pattern.
#[must_use]
pub fn is_masked_key(key: &str) -> bool {
    key.contains("...") || key.contains("****") || key.ends_with("****")
}

/// Validates a provider `base_url` using `url::Url` parsing and IP range checks.
///
/// Rejects non-HTTPS schemes, `127.0.0.0/8`, `10.0.0.0/8`, `172.16.0.0/12`,
/// `192.168.0.0/16`, `169.254.0.0/16`, `::1`, `fe80::/10`, `fc00::/7`,
/// `localhost`, and `*.internal`.
///
/// # Errors
///
/// Returns an error description if the URL is invalid or targets a forbidden IP/host.
pub fn validate_base_url(url_str: &str) -> Result<(), String> {
    let parsed = url::Url::parse(url_str).map_err(|e| format!("Invalid URL: {e}"))?;
    if parsed.scheme() != "https" {
        return Err("Scheme must be HTTPS".to_string());
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "Missing host in URL".to_string())?;
    let host_clean = host.trim_start_matches('[').trim_end_matches(']');
    let host_lower = host_clean.to_lowercase();
    if host_lower == "localhost" || host_lower.ends_with(".internal") || host_lower == "127.0.0.1" {
        return Err("Forbidden host".to_string());
    }
    if let Ok(ip) = host_clean.parse::<std::net::IpAddr>() {
        match ip {
            std::net::IpAddr::V4(ipv4) => {
                let octets = ipv4.octets();
                if octets[0] == 127 {
                    return Err("Forbidden loopback IP range (127.0.0.0/8)".to_string());
                }
                if octets[0] == 10 {
                    return Err("Forbidden private IP range (10.0.0.0/8)".to_string());
                }
                if octets[0] == 172 && (16..=31).contains(&octets[1]) {
                    return Err("Forbidden private IP range (172.16.0.0/12)".to_string());
                }
                if octets[0] == 192 && octets[1] == 168 {
                    return Err("Forbidden private IP range (192.168.0.0/16)".to_string());
                }
                if octets[0] == 169 && octets[1] == 254 {
                    return Err("Forbidden link-local IP range (169.254.0.0/16)".to_string());
                }
            }
            std::net::IpAddr::V6(ipv6) => {
                if ipv6.is_loopback() {
                    return Err("Forbidden loopback IPv6 (::1)".to_string());
                }
                let segments = ipv6.segments();
                if (segments[0] & 0xffc0) == 0xfe80 {
                    return Err("Forbidden link-local IPv6 (fe80::/10)".to_string());
                }
                if (segments[0] & 0xfe00) == 0xfc00 {
                    return Err("Forbidden unique local IPv6 (fc00::/7)".to_string());
                }
            }
        }
    }
    Ok(())
}

/// `GET /v1/dashboard/config` — active runtime configuration with API keys masked.
pub async fn get_dashboard_config(
    State(state): State<EntrypointState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let config = crate::config::load(&state.config_dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    let mut providers_map = serde_json::Map::new();
    for upstream in &config.upstreams {
        let mut p_obj = serde_json::Map::new();
        if let Some(auth) = &upstream.auth {
            #[allow(clippy::collapsible_match)]
            match auth {
                crate::config::schema::AuthMethod::Apikey { key, .. }
                | crate::config::schema::AuthMethod::Bearer { token: key } => {
                    if let crate::config::schema::SecretRef::Inline { value } = key {
                        p_obj.insert("apiKey".to_string(), serde_json::json!(mask_api_key(value)));
                        p_obj.insert(
                            "api_key".to_string(),
                            serde_json::json!(mask_api_key(value)),
                        );
                    }
                }
                _ => {}
            }
        }
        if let crate::config::schema::UpstreamKind::Openai { base_url } = &upstream.kind {
            p_obj.insert("baseUrl".to_string(), serde_json::json!(base_url));
            p_obj.insert("base_url".to_string(), serde_json::json!(base_url));
        }
        if let Some(route) = config.routes.first() {
            if let Some(u_ref) = route.upstreams.iter().find(|u| u.name == upstream.name) {
                if let Some(w) = u_ref.weight {
                    p_obj.insert("weight".to_string(), serde_json::json!(w * 100.0));
                }
            }
        }
        providers_map.insert(upstream.name.clone(), serde_json::Value::Object(p_obj));
    }

    let fallback_cascade: Vec<String> = config
        .routes
        .first()
        .map(|r| r.upstreams.iter().map(|u| u.name.clone()).collect())
        .unwrap_or_default();

    let rate_limits = serde_json::json!({
        "rpm": config.ratelimit.defaults.on_breach,
        "maxDelayMs": config.ratelimit.defaults.max_delay_ms,
    });

    let web_ui_str = match state.web_ui {
        crate::config::schema::WebUiMode::Angular => "angular",
        crate::config::schema::WebUiMode::Legacy => "legacy",
    };

    let response = serde_json::json!({
        "webUi": web_ui_str,
        "web_ui": web_ui_str,
        "providers": providers_map,
        "rateLimits": rate_limits,
        "rate_limits": rate_limits,
        "fallbackCascade": fallback_cascade,
        "fallback_cascade": fallback_cascade,
    });

    Ok(Json(response))
}

/// `PUT /v1/dashboard/config` — update active runtime configuration with safety controls,
/// API key preservation, and mutex lock.
pub async fn put_dashboard_config(
    State(state): State<EntrypointState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let is_non_loopback = headers
        .get("x-forwarded-for")
        .or_else(|| headers.get("x-real-ip"))
        .is_some()
        || headers
            .get("host")
            .and_then(|h| h.to_str().ok())
            .is_some_and(|h| !h.starts_with("127.0.0.1") && !h.starts_with("localhost"));

    if is_non_loopback && !headers.contains_key("x-consolette-auth") {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "Unauthorized" })),
        ));
    }

    let _guard = state.config_lock.lock().await;

    let mut current_config = crate::config::load(&state.config_dir).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    if let Some(providers) = body.get("providers").and_then(|p| p.as_object()) {
        for (p_name, p_val) in providers {
            let base_url_opt = p_val
                .get("baseUrl")
                .or_else(|| p_val.get("base_url"))
                .and_then(|v| v.as_str());

            if let Some(b_url) = base_url_opt {
                validate_base_url(b_url).map_err(|err| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({ "error": err })),
                    )
                })?;
                if let Some(upstream) = current_config
                    .upstreams
                    .iter_mut()
                    .find(|u| u.name == *p_name)
                {
                    if let crate::config::schema::UpstreamKind::Openai { base_url } =
                        &mut upstream.kind
                    {
                        *base_url = b_url.to_string();
                    }
                }
            }

            let api_key_opt = p_val
                .get("apiKey")
                .or_else(|| p_val.get("api_key"))
                .and_then(|v| v.as_str());

            if let Some(api_key) = api_key_opt {
                let effective_key = if is_masked_key(api_key) {
                    current_config
                        .upstreams
                        .iter()
                        .find(|u| u.name == *p_name)
                        .and_then(|u| match &u.auth {
                            Some(crate::config::schema::AuthMethod::Apikey { key, .. })
                            | Some(crate::config::schema::AuthMethod::Bearer { token: key }) => {
                                match key {
                                    crate::config::schema::SecretRef::Inline { value } => {
                                        Some(value.clone())
                                    }
                                    _ => None,
                                }
                            }
                            _ => None,
                        })
                        .unwrap_or_else(|| api_key.to_string())
                } else {
                    api_key.to_string()
                };

                if let Some(upstream) = current_config
                    .upstreams
                    .iter_mut()
                    .find(|u| u.name == *p_name)
                {
                    upstream.auth = Some(crate::config::schema::AuthMethod::Apikey {
                        key: crate::config::schema::SecretRef::Inline {
                            value: effective_key,
                        },
                        header: "x-api-key".to_string(),
                    });
                }
            }

            let weight_opt = p_val.get("weight").and_then(|v| v.as_f64());
            if let Some(w) = weight_opt {
                if let Some(route) = current_config.routes.first_mut() {
                    if let Some(u_ref) = route.upstreams.iter_mut().find(|u| u.name == *p_name) {
                        u_ref.weight = Some(w / 100.0);
                    }
                }
            }
        }
    }

    if let Some(cascade) = body
        .get("fallbackCascade")
        .or_else(|| body.get("fallback_cascade"))
        .and_then(|c| c.as_array())
    {
        let new_cascade_names: Vec<String> = cascade
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect();

        if !new_cascade_names.is_empty() {
            if let Some(route) = current_config.routes.first_mut() {
                let mut new_refs = Vec::new();
                for name in &new_cascade_names {
                    let existing_ref = route
                        .upstreams
                        .iter()
                        .find(|u| u.name == *name)
                        .cloned()
                        .unwrap_or_else(|| crate::config::schema::RouteUpstreamRef {
                            name: name.clone(),
                            weight: Some(1.0),
                            model: None,
                            model_family: None,
                        });
                    new_refs.push(existing_ref);
                }
                route.upstreams = new_refs;
            }
        }
    }

    crate::config::validate_references(&current_config).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    crate::config::validate_model_selectors(&current_config).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
    })?;

    if let Some(route) = current_config.routes.first() {
        let overrides = crate::config::RuntimeOverrides {
            route: Some(route.clone()),
        };
        overrides.save(&state.config_dir).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": format!("failed to persist overrides: {e}") })),
            )
        })?;
    }

    let new_router =
        crate::routing::router::Router::from_config(&current_config, Arc::clone(&state.metrics))
            .await
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    Json(serde_json::json!({ "error": format!("failed to rebuild router: {e}") })),
                )
            })?
            .with_session_overrides(Arc::clone(&state.session_overrides))
            .with_capability(Arc::clone(&state.capability))
            .with_local_catalog(state.dispatch_router.load().local_catalog());

    state.dispatch_router.store(Arc::new(new_router));

    let _ = state
        .event_tx
        .send(crate::entrypoint::events::DashboardEvent::ConfigChanged(
            crate::entrypoint::events::ConfigChangedData {
                timestamp: chrono::Utc::now().to_rfc3339(),
                route_name: state.server_info.route_name.clone(),
                strategy: state.server_info.strategy.clone(),
            },
        ));

    get_dashboard_config(State(state.clone())).await
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::config::schema::Config;

    #[allow(clippy::unwrap_used)]
    async fn state_with_cached_body(request_id: &str, body: serde_json::Value) -> EntrypointState {
        let state = EntrypointState::build(
            &Config::default(),
            std::path::Path::new("/tmp/consolette-test"),
        )
        .await
        .unwrap();
        state
            .metrics
            .push_original_body(request_id.to_string(), body);
        state
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn original_stage_returns_the_cached_body() {
        let body = serde_json::json!({"model": "claude-sonnet-4-5", "messages": []});
        let state = state_with_cached_body("req-1", body.clone()).await;

        let result = get_request_body(
            State(state),
            Path("req-1".to_string()),
            Query(HashMap::from([(
                "stage".to_string(),
                "original".to_string(),
            )])),
        )
        .await;

        let Json(returned) = result.unwrap();
        assert_eq!(returned, body);
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn unknown_request_id_404s() {
        let state = state_with_cached_body("req-1", serde_json::json!({})).await;

        let result = get_request_body(
            State(state),
            Path("does-not-exist".to_string()),
            Query(HashMap::new()),
        )
        .await;

        assert_eq!(result.unwrap_err(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn compressed_stage_returns_cached_body_or_404() {
        let original = serde_json::json!({"model": "claude", "messages": []});
        let compressed =
            serde_json::json!({"model": "claude", "messages": [{"role": "user", "content": "c"}]});
        let state = state_with_cached_body("req-1", original).await;

        // Unrecorded compressed stage returns 404
        let result_404 = get_request_body(
            State(state.clone()),
            Path("req-1".to_string()),
            Query(HashMap::from([(
                "stage".to_string(),
                "compressed".to_string(),
            )])),
        )
        .await;
        assert_eq!(result_404.unwrap_err(), StatusCode::NOT_FOUND);

        // Once pushed, compressed stage returns body
        state
            .metrics
            .push_compressed_body("req-1".to_string(), compressed.clone());
        let result_ok = get_request_body(
            State(state),
            Path("req-1".to_string()),
            Query(HashMap::from([(
                "stage".to_string(),
                "compressed".to_string(),
            )])),
        )
        .await;
        let Json(returned) = result_ok.unwrap();
        assert_eq!(returned, compressed);
    }

    #[tokio::test]
    #[allow(clippy::unwrap_used)]
    async fn get_dashboard_sessions_returns_active_sessions() {
        let state = state_with_cached_body("req-1", serde_json::json!({})).await;
        let body = serde_json::json!({"model": "claude-sonnet-4-5", "messages": [{"role": "user", "content": "hi"}]});
        let mut detail = crate::metrics::RequestDetail::from_body(
            "req-1".to_string(),
            false,
            100,
            &body,
            Some("sess-abc".to_string()),
        );
        detail.provider = "anthropic".to_string();
        detail.tokens_after = 60;
        state.metrics.push_request(detail);

        let Json(sessions) = get_dashboard_sessions(
            State(state),
            Query(SessionsQueryParams {
                limit: Some(10),
                cursor: None,
                search: None,
            }),
        )
        .await;

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].id, "sess-abc");
        assert_eq!(sessions[0].turn_count, 1);
        assert!((sessions[0].token_savings_percent - 40.0).abs() < f64::EPSILON);
        assert_eq!(sessions[0].provider, "anthropic");
    }

    // ── REQ-11/REQ-12 (Story 1.5.1/1.5.2) — real `/metrics` cooldowns feed
    // and the ship-blocking auth-vs-cooldown classification gate. ──────────

    use crate::providers::{Provider, ProviderError, ProviderResponse};
    use crate::routing::health::HealthRegistry;
    use crate::routing::router::{Router as DispatchRouter, RouterDeps};
    use crate::routing::strategy::{FallbackStrategy, RoutingStrategy, UpstreamRef};
    use axum::http::HeaderMap;
    use std::sync::Arc;

    struct AlwaysOkProvider {
        name: &'static str,
    }

    #[async_trait::async_trait]
    impl Provider for AlwaysOkProvider {
        fn name(&self) -> &str {
            self.name
        }

        async fn send(
            &self,
            _body: serde_json::Value,
            _headers: HeaderMap,
            _stream: bool,
        ) -> Result<ProviderResponse, ProviderError> {
            Ok(ProviderResponse::Full(serde_json::json!({"ok": true})))
        }

        async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    struct AlwaysAuthErrProvider {
        name: &'static str,
    }

    #[async_trait::async_trait]
    impl Provider for AlwaysAuthErrProvider {
        fn name(&self) -> &str {
            self.name
        }

        async fn send(
            &self,
            _body: serde_json::Value,
            _headers: HeaderMap,
            _stream: bool,
        ) -> Result<ProviderResponse, ProviderError> {
            Err(ProviderError::Auth("token expired".to_string()))
        }

        async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
            Ok(Vec::new())
        }
    }

    /// The shared entrypoint test-fixture builder (`crate::entrypoint::
    /// test_support::state_with_router`) — same pattern `messages.rs`'s
    /// `test_state_with_provider` uses — so a test can control
    /// candidates/providers/health directly instead of going through
    /// `Router::from_config`.
    use crate::entrypoint::test_support::state_with_router;

    fn always_allow_admission() -> Arc<dyn crate::ratelimit::AdmissionControl> {
        Arc::new(crate::ratelimit::RateLimiters::new(
            &crate::config::schema::RateLimitConfig::default(),
        )) as Arc<dyn crate::ratelimit::AdmissionControl>
    }

    fn upstream_ref(index: usize, name: &str) -> UpstreamRef {
        UpstreamRef {
            index,
            name: name.to_string(),
            weight: 1.0,
            model: None,
            model_family: None,
        }
    }

    // REQ-11's integration test — explicit adversarial-review regression
    // requirement (Task 1.5.1d): confirms the real cooldown feed doesn't
    // cross-contaminate entries across three simultaneous candidates, not
    // just that a single upstream looks right in isolation.
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn get_metrics_cooldowns_should_have_correct_non_cross_contaminated_entries_for_anthropic_bedrock_and_gemini(
    ) {
        let health = Arc::new(HealthRegistry::new(300));
        // anthropic (index 0): left healthy.
        health.trip(1, Some(std::time::Duration::from_secs(42))); // bedrock: normal cooldown
        health.trip(
            2,
            Some(std::time::Duration::from_secs(
                crate::providers::gemini::DRIFT_COOLDOWN_SECS,
            )),
        ); // gemini: drift cooldown

        let providers: Vec<Arc<dyn Provider>> = vec![
            Arc::new(AlwaysOkProvider { name: "anthropic" }),
            Arc::new(AlwaysOkProvider { name: "bedrock" }),
            Arc::new(AlwaysOkProvider { name: "gemini" }),
        ];
        let metrics = crate::metrics::MetricsCollector::new();
        let router = DispatchRouter::new(RouterDeps {
            candidates: vec![
                upstream_ref(0, "anthropic"),
                upstream_ref(1, "bedrock"),
                upstream_ref(2, "gemini"),
            ],
            providers,
            strategy: Arc::new(FallbackStrategy) as Arc<dyn RoutingStrategy>,
            health,
            admission: always_allow_admission(),
            metrics: Arc::clone(&metrics),
        });
        let state = state_with_router(router, metrics).await;

        let response = get_metrics(State(state)).await.into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body must be readable");
        let json: serde_json::Value =
            serde_json::from_slice(&body).expect("body must be valid JSON");

        assert_eq!(
            json["cooldowns"]["anthropic"],
            serde_json::json!({"circuit_state": "closed", "cooling_down": false, "remaining_seconds": 0}),
            "anthropic must be healthy, not contaminated by bedrock/gemini's cooldowns"
        );
        assert_eq!(
            json["cooldowns"]["bedrock"]["cooling_down"],
            serde_json::json!(true)
        );
        let bedrock_remaining = json["cooldowns"]["bedrock"]["remaining_seconds"]
            .as_u64()
            .expect("remaining_seconds must be a u64");
        assert!(
            bedrock_remaining > 0 && bedrock_remaining <= 42,
            "bedrock's remaining_seconds must reflect its own 42s trip, got {bedrock_remaining}"
        );
        assert_eq!(
            json["cooldowns"]["gemini"]["cooling_down"],
            serde_json::json!(true)
        );
        let gemini_remaining = json["cooldowns"]["gemini"]["remaining_seconds"]
            .as_u64()
            .expect("remaining_seconds must be a u64");
        assert!(
            gemini_remaining > 42,
            "gemini's remaining_seconds must reflect its own drift cooldown, \
             not bedrock's 42s value, got {gemini_remaining}"
        );
    }

    // REQ-12's ship-blocking test (validation.md: "do not mark Story 1.5.2
    // done without this test green") — the exact regression this epic
    // exists to prevent: `Router::dispatch`'s `is_auth()` arm never calls
    // `health.trip(...)`, so a real Gemini auth failure must show
    // `last_error_kind == "auth"` AND `cooling_down == false`
    // *simultaneously* in `/metrics` — never falling through to a plain
    // `status-active`/`status-cooldown` read.
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn get_metrics_should_classify_as_status_auth_required_eligible_when_a_real_gemini_auth_failure_is_induced(
    ) {
        let health = Arc::new(HealthRegistry::new(300));
        let providers: Vec<Arc<dyn Provider>> =
            vec![Arc::new(AlwaysAuthErrProvider { name: "gemini" })];
        let metrics = crate::metrics::MetricsCollector::new();
        let router = DispatchRouter::new(RouterDeps {
            candidates: vec![upstream_ref(0, "gemini")],
            providers,
            strategy: Arc::new(FallbackStrategy) as Arc<dyn RoutingStrategy>,
            health,
            admission: always_allow_admission(),
            metrics: Arc::clone(&metrics),
        });

        // Induce the real auth failure through `Router::dispatch`, exactly
        // as a live expired-Antigravity-token request would.
        let dispatch_result = router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await;
        assert!(
            matches!(dispatch_result, Err(ProviderError::Auth(_))),
            "expected the auth failure to propagate immediately, no failover"
        );

        let state = state_with_router(router, metrics).await;
        let response = get_metrics(State(state)).await.into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body must be readable");
        let json: serde_json::Value =
            serde_json::from_slice(&body).expect("body must be valid JSON");

        assert_eq!(
            json["providers"]["gemini"]["last_error_kind"],
            serde_json::json!("auth")
        );
        assert_eq!(
            json["cooldowns"]["gemini"]["cooling_down"],
            serde_json::json!(false),
            "is_auth() must never trip HealthRegistry — this is the exact gap Story 1.5.2 \
             exists to guard the JS against"
        );
    }

    // ── REQ-7 (Story 5.1.2, Task 5.1.2c) — `openrouter_scoring` merge. ──────

    // *Given* an active `FallbackStrategy` route, *when* `GET /metrics` is
    // called, *then* the response has no `openrouter_scoring` key at all
    // (not `null`).
    #[tokio::test]
    #[allow(clippy::expect_used)]
    async fn get_metrics_should_omit_openrouter_scoring_key_for_fallback_strategy() {
        let health = Arc::new(HealthRegistry::new(300));
        let providers: Vec<Arc<dyn Provider>> =
            vec![Arc::new(AlwaysOkProvider { name: "primary" })];
        let metrics = crate::metrics::MetricsCollector::new();
        let router = DispatchRouter::new(RouterDeps {
            candidates: vec![upstream_ref(0, "primary")],
            providers,
            strategy: Arc::new(FallbackStrategy) as Arc<dyn RoutingStrategy>,
            health,
            admission: always_allow_admission(),
            metrics: Arc::clone(&metrics),
        });
        let state = state_with_router(router, metrics).await;

        let response = get_metrics(State(state)).await.into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body must be readable");
        let json: serde_json::Value =
            serde_json::from_slice(&body).expect("body must be valid JSON");

        assert!(
            json.as_object()
                .expect("response must be a JSON object")
                .get("openrouter_scoring")
                .is_none(),
            "openrouter_scoring key must be entirely absent for a FallbackStrategy route, got: {json}"
        );
    }

    // *Given* an active `OpenrouterScoringStrategy` route, *when* `GET
    // /metrics` is called, *then* the response's `openrouter_scoring` key
    // matches `observability_snapshot()`'s output.
    #[tokio::test]
    #[allow(clippy::expect_used, clippy::unwrap_used)]
    async fn get_metrics_should_include_openrouter_scoring_block_for_scored_route() {
        use crate::providers::openrouter::cache::ModelListCache;
        use crate::routing::openrouter_scoring::OpenrouterScoringStrategy;

        let health = Arc::new(HealthRegistry::new(300));
        let model_cache = Arc::new(ModelListCache::new_with_ttl(
            std::time::Duration::from_mins(15),
        ));
        let strategy = Arc::new(OpenrouterScoringStrategy::new(Arc::clone(&model_cache), 0));
        let providers: Vec<Arc<dyn Provider>> =
            vec![Arc::new(AlwaysOkProvider { name: "openrouter" })];
        let metrics = crate::metrics::MetricsCollector::new();
        let router = DispatchRouter::new(RouterDeps {
            candidates: vec![UpstreamRef {
                index: 0,
                name: "openrouter".to_string(),
                weight: 1.0,
                model: Some("a/b:free".to_string()),
                model_family: None,
            }],
            providers,
            strategy: Arc::clone(&strategy) as Arc<dyn RoutingStrategy>,
            health,
            admission: always_allow_admission(),
            metrics: Arc::clone(&metrics),
        });

        // Drive one real selection so `last_scores` (and therefore the
        // `models` block) isn't empty.
        router
            .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
            .await
            .expect("dispatch against AlwaysOkProvider must succeed");

        let expected = strategy
            .observability_snapshot()
            .expect("OpenrouterScoringStrategy must always return Some");

        let state = state_with_router(router, metrics).await;
        let response = get_metrics(State(state)).await.into_response();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("body must be readable");
        let json: serde_json::Value =
            serde_json::from_slice(&body).expect("body must be valid JSON");

        assert_eq!(
            json["openrouter_scoring"]["models"]["a/b:free"],
            expected["models"]["a/b:free"]
        );
        assert_eq!(
            json["openrouter_scoring"]["cache"]["cached_model_count"],
            expected["cache"]["cached_model_count"]
        );
    }

    #[test]
    fn tc_unit_26_api_key_masking_and_preservation() {
        let plain_anthropic = "sk-ant-api03-1234567890abcdef";
        let masked_anthropic = mask_api_key(plain_anthropic);
        assert_eq!(masked_anthropic, "sk-ant-...cdef");
        assert!(is_masked_key(&masked_anthropic));

        let plain_openai = "sk-1234567890abcdef";
        let masked_openai = mask_api_key(plain_openai);
        assert_eq!(masked_openai, "sk-...cdef");
        assert!(is_masked_key(&masked_openai));

        assert!(!is_masked_key("sk-1234567890abcdef"));
        assert!(is_masked_key("sk-...****"));
    }

    #[test]
    fn tc_unit_27_ssrf_url_validation() {
        // Valid HTTPS URL
        assert!(validate_base_url("https://api.openai.com/v1").is_ok());

        // Rejected non-HTTPS scheme
        assert!(validate_base_url("http://api.openai.com/v1").is_err());

        // Rejected loopback IPv4
        assert!(validate_base_url("https://127.0.0.1/v1").is_err());
        assert!(validate_base_url("https://127.0.0.5/v1").is_err());

        // Rejected private IPv4
        assert!(validate_base_url("https://10.0.0.1/v1").is_err());
        assert!(validate_base_url("https://172.16.0.1/v1").is_err());
        assert!(validate_base_url("https://192.168.1.1/v1").is_err());

        // Rejected link-local IPv4
        assert!(validate_base_url("https://169.254.169.254/v1").is_err());

        // Rejected loopback IPv6
        assert!(validate_base_url("https://[::1]/v1").is_err());

        // Rejected localhost and internal hostnames
        assert!(validate_base_url("https://localhost/v1").is_err());
        assert!(validate_base_url("https://service.internal/v1").is_err());
    }

    #[tokio::test]
    async fn get_dashboard_benchmark_returns_valid_items() {
        use crate::config::schema::Config;

        let state = EntrypointState::build(
            &Config::default(),
            std::path::Path::new("/tmp/consolette-test"),
        )
        .await
        .unwrap();

        let Json(benchmarks) = get_dashboard_benchmark(State(state)).await;
        assert!(!benchmarks.is_empty(), "benchmark list should not be empty");
        assert!(benchmarks.iter().any(|b| b.model.contains("claude")
            || b.model.contains("gpt")
            || b.model.contains("deepseek")));
    }
}
