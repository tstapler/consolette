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
    result["response_cache"] = state.response_cache.stats_json();
    result["model_catalog"] = state.catalog.stats_json();
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
    /// `None` when `tokens_after` was never recorded (compression is off), not 100.
    pub token_savings_percent: Option<f64>,
    pub last_active_timestamp: String,
    #[serde(rename = "lastActive", alias = "last_active")]
    pub last_active: String,
    pub pinned: bool,
    pub pin_status: Option<crate::routing::session_overrides::SessionOverride>,
    pub model: String,
    pub provider: String,
    /// Newest ring-buffer request of the session: the id `GET /requests/{id}` accepts.
    pub latest_request_id: String,
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

        // tokens_after stays 0 while compression is off; that is "not recorded", not a 100% saving.
        let token_savings_percent = if tokens_before_sum > 0 && tokens_after_sum > 0 {
            let saved = tokens_before_sum.saturating_sub(tokens_after_sum);
            let pct = (saved as f64 / tokens_before_sum as f64) * 100.0;
            Some((pct * 10.0).round() / 10.0)
        } else {
            None
        };

        let pin = state.session_overrides.get(&sess_id);
        let pinned = pin.is_some();
        let model = latest_req.effective_model().to_string();
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
            latest_request_id: latest_req.request_id.clone(),
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
    /// `None` when no output tokens were recorded (nothing to divide).
    pub speed_tok_sec: Option<f64>,
    /// `None` until finished traces exist; follows `metrics::trace_error_rate`.
    pub success_rate_percent: Option<f64>,
    pub error_rate: Option<f64>,
    /// Always `None`: no price data is recorded.
    pub cost_per_1k: Option<f64>,
    pub tokens_saved_percent: f64,
    /// `None` for a model absent from the bench table.
    pub aider_score: Option<f64>,
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
            .entry((req.effective_model().to_string(), provider))
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
        let speed_tok_sec = (total_duration_sec > 0.0 && total_after > 0)
            .then(|| total_after as f64 / total_duration_sec);

        let aider_score = crate::routing::bench_table::bench_score(&model).map(|s| s * 100.0);

        let finished: Vec<crate::metrics::RequestDetail> =
            reqs.iter().map(|r| (*r).clone()).collect();
        let rate = crate::metrics::trace_error_rate(&finished);
        let round1 = |v: f64| (v * 10.0).round() / 10.0;
        let (error_rate, success_rate_percent) = if rate.requests == 0 {
            (None, None)
        } else {
            (
                Some(round1(rate.rate * 100.0)),
                Some(round1((1.0 - rate.rate) * 100.0)),
            )
        };

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
            success_rate_percent,
            error_rate,
            cost_per_1k: None,
            tokens_saved_percent,
            aider_score,
        });
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

/// The `base_url` of upstream kinds that have one.
fn upstream_base_url(kind: &crate::config::schema::UpstreamKind) -> Option<&str> {
    use crate::config::schema::UpstreamKind;
    match kind {
        UpstreamKind::Anthropic { base_url } | UpstreamKind::Openai { base_url } => {
            Some(base_url.as_str())
        }
        _ => None,
    }
}

fn upstream_base_url_mut(kind: &mut crate::config::schema::UpstreamKind) -> Option<&mut String> {
    use crate::config::schema::UpstreamKind;
    match kind {
        UpstreamKind::Anthropic { base_url } | UpstreamKind::Openai { base_url } => Some(base_url),
        _ => None,
    }
}

/// Auth after the operator typed a new key: keeps the existing variant (and `Apikey`
/// header) so a Bearer upstream stays Bearer; an upstream with no inline-key auth
/// gets `Apikey` + `x-api-key`.
fn auth_with_new_key(
    existing: Option<&crate::config::schema::AuthMethod>,
    new_key: String,
) -> crate::config::schema::AuthMethod {
    use crate::config::schema::{AuthMethod, SecretRef};
    let secret = SecretRef::Inline { value: new_key };
    match existing {
        Some(AuthMethod::Bearer { .. }) => AuthMethod::Bearer { token: secret },
        Some(AuthMethod::Apikey { header, .. }) => AuthMethod::Apikey {
            key: secret,
            header: header.clone(),
        },
        _ => AuthMethod::Apikey {
            key: secret,
            header: "x-api-key".to_string(),
        },
    }
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
        if let Some(base_url) = upstream_base_url(&upstream.kind) {
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

    // Read-only: limits live in the route file and PUT does not persist them (E-14).
    let rate_limits = serde_json::json!({
        "onBreach": config.ratelimit.defaults.on_breach,
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
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|u| !u.is_empty());

            if let Some(b_url) = base_url_opt {
                let upstream = current_config
                    .upstreams
                    .iter_mut()
                    .find(|u| u.name == *p_name);
                // An unchanged URL is not re-validated, so one legacy `http://` upstream
                // cannot reject the whole save (E-15).
                let unchanged = upstream
                    .as_ref()
                    .and_then(|u| upstream_base_url(&u.kind))
                    .is_some_and(|current| current == b_url);
                if !unchanged {
                    validate_base_url(b_url).map_err(|err| {
                        (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({ "error": format!("{p_name}: {err}") })),
                        )
                    })?;
                    if let Some(base_url) =
                        upstream.and_then(|u| upstream_base_url_mut(&mut u.kind))
                    {
                        *base_url = b_url.to_string();
                    }
                }
            }

            let api_key_opt = p_val
                .get("apiKey")
                .or_else(|| p_val.get("api_key"))
                .and_then(|v| v.as_str())
                .filter(|k| !k.is_empty() && !is_masked_key(k));

            // A masked key is the GET echo, not a new secret: leave auth untouched.
            if let Some(api_key) = api_key_opt {
                if let Some(upstream) = current_config
                    .upstreams
                    .iter_mut()
                    .find(|u| u.name == *p_name)
                {
                    upstream.auth = Some(auth_with_new_key(
                        upstream.auth.as_ref(),
                        api_key.to_string(),
                    ));
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

    state.swap_router(new_router);

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
    async fn put_dashboard_config_should_prune_catalog_to_the_swapped_routers_upstreams() {
        let dir = tempfile::tempdir().unwrap();
        let state =
            EntrypointState::build_with_cache_dir(&Config::default(), dir.path(), Some(dir.path()))
                .await
                .unwrap();
        let stale = crate::metrics::ids::UpstreamName::from("removed-upstream");
        state.catalog.prune(&[stale]);
        assert!(state
            .catalog
            .snapshot()
            .upstreams
            .contains_key("removed-upstream"));

        let _ = put_dashboard_config(
            State(state.clone()),
            HeaderMap::new(),
            Json(serde_json::json!({})),
        )
        .await
        .unwrap();

        assert!(!state
            .catalog
            .snapshot()
            .upstreams
            .contains_key("removed-upstream"));
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
    async fn get_dashboard_sessions_should_report_no_savings_when_tokens_after_was_never_recorded()
    {
        let state = state_with_cached_body("req-1", serde_json::json!({})).await;
        let body = serde_json::json!({"model": "claude-sonnet-4-5", "messages": [{"role": "user", "content": "hi"}]});
        let detail = crate::metrics::RequestDetail::from_body(
            "req-1".to_string(),
            false,
            100,
            &body,
            Some("sess-none".to_string()),
        );
        assert_eq!(
            detail.tokens_after, 0,
            "precondition: tokens_after unrecorded"
        );
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

        assert_eq!(sessions[0].token_savings_percent, None);
        let wire = serde_json::to_value(&sessions[0]).unwrap();
        assert!(wire["tokenSavingsPercent"].is_null(), "{wire}");
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
        assert_eq!(sessions[0].token_savings_percent, Some(40.0));
        assert_eq!(sessions[0].provider, "anthropic");
    }

    #[tokio::test]
    async fn get_dashboard_sessions_reports_the_routed_model() {
        let state = state_with_cached_body("req-1", serde_json::json!({})).await;
        let body = serde_json::json!({"model": "claude-sonnet-5-5", "messages": []});
        let mut detail = crate::metrics::RequestDetail::from_body(
            "req-1".to_string(),
            false,
            0,
            &body,
            Some("sess-abc".to_string()),
        );
        detail.routed_model = Some("gpt-6.1-sol".to_string());
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

        assert_eq!(sessions[0].model, "gpt-6.1-sol");
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
    async fn get_dashboard_benchmark_groups_by_routed_model_not_requested_model() {
        use crate::config::schema::Config;

        let state = EntrypointState::build(
            &Config::default(),
            std::path::Path::new("/tmp/consolette-test"),
        )
        .await
        .unwrap();
        let body = serde_json::json!({"model": "claude-sonnet-5-5", "messages": []});
        let mut routed =
            crate::metrics::RequestDetail::from_body("a".into(), false, 0, &body, None);
        routed.provider = "model-gateway-openai".into();
        routed.routed_model = Some("gpt-6.1-sol".into());
        let mut unrouted =
            crate::metrics::RequestDetail::from_body("b".into(), false, 0, &body, None);
        unrouted.provider = "model-gateway-anthropic".into();
        state.metrics.push_request(routed);
        state.metrics.push_request(unrouted);

        let Json(benchmarks) = get_dashboard_benchmark(State(state)).await;
        let mut rows: Vec<(String, String)> = benchmarks
            .into_iter()
            .map(|b| (b.model, b.provider))
            .collect();
        rows.sort();

        assert_eq!(
            rows,
            vec![
                (
                    "claude-sonnet-5-5".to_string(),
                    "model-gateway-anthropic".to_string()
                ),
                (
                    "gpt-6.1-sol".to_string(),
                    "model-gateway-openai".to_string()
                ),
            ]
        );
    }

    #[tokio::test]
    async fn get_dashboard_benchmark_is_empty_without_traffic() {
        use crate::config::schema::Config;

        let state = EntrypointState::build(
            &Config::default(),
            std::path::Path::new("/tmp/consolette-test"),
        )
        .await
        .unwrap();

        let Json(benchmarks) = get_dashboard_benchmark(State(state)).await;
        assert!(benchmarks.is_empty(), "no seed rows: {benchmarks:?}");
    }
}
