//! Epic 5a (Story 5.1, validation R3): `/metrics` exposes per-alias
//! resolution state, and `GET /api/route` marks family entries
//! unambiguously vs pinned entries.
//!
//! Drives three resolutions (picks A, A, B) through the canonical publisher
//! (`FamilyRuntime::record_snapshot` — the exact call Epic 3's dispatch
//! resolution makes), then reads the real HTTP paths: `GET /metrics` must
//! show `resolutions_total=3`, `current_pick=B`, `previous_pick=A`, while
//! the existing `providers`/`provider_latency` sections keep their shape.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use axum::body::to_bytes;
use axum::http::Request;
use tower::ServiceExt;

use consolette::entrypoint::{entrypoint_router, EntrypointState};
use consolette::metrics::{MetricsCollector, ResolutionSnapshot};
use consolette::routing::health::HealthRegistry;
use consolette::routing::router::Router;
use consolette::routing::strategy::{FallbackStrategy, RoutingStrategy, UpstreamRef};

fn write_conf_d(dir: &std::path::Path) {
    let conf_d = dir.join("conf.d");
    std::fs::create_dir_all(&conf_d).unwrap();
    std::fs::write(
        conf_d.join("00-upstreams.toml"),
        "[[upstreams]]\nname = \"mock\"\nkind = \"anthropic\"\n",
    )
    .unwrap();
    std::fs::write(
        conf_d.join("10-routing.toml"),
        "[[routes]]\nname = \"default\"\nstrategy = \"fallback\"\nfamily = \"auto-coding\"\n\n\
         [[routes.upstreams]]\nname = \"mock\"\n",
    )
    .unwrap();
    std::fs::write(
        conf_d.join("20-family.toml"),
        "[[model_families]]\nalias = \"auto-coding\"\nallow_paid = false\n\n\
         [[model_families.members]]\nupstream = \"mock\"\nmodel = \"model-a:free\"\n\n\
         [[model_families.members]]\nupstream = \"mock\"\nmodel = \"model-b:free\"\n",
    )
    .unwrap();
}

async fn state_with_family(dir: &std::path::Path) -> EntrypointState {
    let metrics = MetricsCollector::new();
    let router = Router::new(
        vec![UpstreamRef {
            index: 0,
            name: "mock".to_string(),
            weight: 1.0,
            model: None,
        }],
        Vec::new(),
        Arc::new(FallbackStrategy) as Arc<dyn RoutingStrategy>,
        Arc::new(HealthRegistry::new(300)),
        Arc::new(consolette::ratelimit::RateLimiters::new(
            &consolette::config::schema::RateLimitConfig::default(),
        )) as Arc<dyn consolette::ratelimit::AdmissionControl>,
        Arc::clone(&metrics),
    );
    EntrypointState {
        dispatch_router: Arc::new(arc_swap::ArcSwap::from_pointee(router)),
        cost_tracker: Arc::new(
            consolette::cost_metrics::tracker::CostTracker::new(
                consolette::cost_metrics::pricing::PricingTable::load_default(),
            )
            .await,
        ),
        metrics,
        server_info: Arc::new(consolette::entrypoint::ServerInfo {
            port: 0,
            route_name: "default".to_string(),
            strategy: "Fallback".to_string(),
            upstreams: vec![],
        }),
        config_dir: Arc::new(dir.to_path_buf()),
        session_overrides: Arc::new(
            consolette::routing::session_overrides::SessionOverrideStore::new(),
        ),
    }
}

/// Publishes one resolution exactly the way dispatch does
/// (`publish_resolution`: previous pick carried forward, figures recorded).
fn publish(
    state: &EntrypointState,
    model: &str,
    error_rate: f64,
    latency_p50_ms: u64,
    samples: usize,
) {
    let previous = state
        .metrics
        .family
        .snapshot("auto-coding")
        .map(|s| s.picked);
    state
        .metrics
        .family
        .record_snapshot(ResolutionSnapshot::new(
            "auto-coding",
            "mock",
            model,
            error_rate,
            latency_p50_ms,
            samples,
            previous,
        ));
}

async fn get_json(router: axum::Router, path: &str) -> serde_json::Value {
    let resp = router
        .oneshot(Request::get(path).body(axum::body::Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), axum::http::StatusCode::OK);
    let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[tokio::test]
async fn metrics_should_expose_resolutions_and_picks_when_alias_resolves_repeatedly() {
    let dir = tempfile::tempdir().unwrap();
    write_conf_d(dir.path());
    // The on-disk config must validate (same guards as load + hot-swap).
    let config = consolette::config::load(dir.path()).expect("family conf.d must load");
    assert_eq!(config.families.len(), 1);

    let state = state_with_family(dir.path()).await;

    // Three resolutions with picks A, A, B.
    publish(&state, "model-a:free", 0.02, 2100, 21);
    publish(&state, "model-a:free", 0.015, 2050, 22);
    publish(&state, "model-b:free", 0.0, 1800, 30);

    // One upstream request so the `providers` sections are non-empty —
    // their shape must be byte-identical to the pre-family schema.
    state.metrics.counters.record_request("mock", true, 80, 0);

    let router = entrypoint_router(state);
    let json = get_json(router.clone(), "/metrics").await;

    let family = &json["family"]["auto-coding"];
    assert!(family.is_object(), "family section must exist: {json}");
    assert_eq!(family["resolutions_total"], serde_json::json!(3));
    assert_eq!(family["fallback_to_default_total"], serde_json::json!(0));
    assert_eq!(family["current_pick"], serde_json::json!("model-b:free"));
    assert_eq!(family["previous_pick"], serde_json::json!("model-a:free"));
    assert!(family["last_change_at"].is_string());
    let members = family["members"].as_array().expect("members array");
    assert_eq!(members.len(), 2);
    assert_eq!(members[0]["model"], serde_json::json!("model-a:free"));
    for member in members {
        for key in ["model", "error_rate", "latency_p50_ms", "samples", "status"] {
            assert!(
                member.get(key).is_some(),
                "member entry must carry {key}: {member}"
            );
        }
    }

    // Existing sections keep their exact shape (Epic 2 regression scope).
    let providers = json["providers"].as_object().expect("providers map");
    assert_eq!(providers.len(), 1);
    let mock = &providers["mock"];
    assert_eq!(mock["requests"], serde_json::json!(1));
    assert_eq!(mock["success"], serde_json::json!(1));
    assert_eq!(mock["errors"], serde_json::json!(0));
    assert!(mock.get("last_error_kind").is_some());
    let latency = json["provider_latency"].as_object().expect("latency map");
    assert_eq!(latency.len(), 1);
    let mock_lat = &latency["mock"];
    assert!(mock_lat.get("avg_duration_ms").is_some());
    assert!(mock_lat.get("avg_first_byte_ms").is_some());
    assert_eq!(mock_lat["requests"], serde_json::json!(1));

    // `GET /api/route` marks the family entry unambiguously (alias,
    // members, current pick) vs pinned entries.
    let route = get_json(router, "/api/route").await;
    assert_eq!(route["name"], serde_json::json!("default"));
    assert_eq!(route["family"], serde_json::json!("auto-coding"));
    assert_eq!(route["entry_kind"], serde_json::json!("family"));
    let detail = &route["family_detail"];
    assert_eq!(detail["alias"], serde_json::json!("auto-coding"));
    assert_eq!(detail["current_pick"], serde_json::json!("model-b:free"));
    assert_eq!(detail["resolutions_total"], serde_json::json!(3));
    let detail_members = detail["members"].as_array().expect("member list");
    assert_eq!(detail_members.len(), 2);
    assert_eq!(
        detail_members[0]["model"],
        serde_json::json!("model-a:free")
    );
    assert_eq!(
        detail_members[1]["model"],
        serde_json::json!("model-b:free")
    );
}
