//! Epic 1 Story 1.2 hot-swap coverage (auto-model-family): a `POST
//! /api/route` that would hot-swap a paid member into the free family is
//! rejected with 400 naming alias + member, and family members referencing
//! unknown upstreams are rejected naming the upstream — mirroring
//! `post_route_rejects_unknown_upstream`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;

use consolette::config::schema::{Route, RouteUpstreamRef, Strategy};
use consolette::config::RuntimeOverrides;
use consolette::entrypoint::api::post_route;
use consolette::entrypoint::EntrypointState;

const UPSTREAMS_TOML: &str = r#"
[[upstreams]]
name = "anthropic"
kind = "anthropic"

[[upstreams]]
name = "bedrock"
kind = "bedrock"
"#;

const ROUTE_TOML: &str = r#"
[[routes]]
name = "default"
strategy = "fallback"

[[routes.upstreams]]
name = "anthropic"
"#;

const FREE_FAMILY_TOML: &str = r#"
[[model_families]]
alias = "auto-coding"

[[model_families.members]]
upstream = "anthropic"
model = "model-a:free"

[[model_families.members]]
upstream = "bedrock"
model = "model-b:free"
"#;

fn write_conf_d(dir: &std::path::Path, family_toml: &str) {
    let conf_d = dir.join("conf.d");
    std::fs::create_dir_all(&conf_d).unwrap();
    std::fs::write(conf_d.join("00-upstreams.toml"), UPSTREAMS_TOML).unwrap();
    std::fs::write(conf_d.join("10-routing.toml"), ROUTE_TOML).unwrap();
    std::fs::write(conf_d.join("20-family.toml"), family_toml).unwrap();
}

fn valid_route() -> Route {
    Route {
        name: "default".to_string(),
        strategy: Strategy::Fallback,
        upstreams: vec![RouteUpstreamRef {
            name: "anthropic".to_string(),
            weight: None,
            model: None,
        }],
        family: None,
    }
}

async fn state_for(config_dir: &std::path::Path) -> EntrypointState {
    let config = consolette::config::load(config_dir).unwrap();
    EntrypointState::build(&config, config_dir).await.unwrap()
}

#[tokio::test]
async fn post_route_should_reject_paid_member_when_hot_swapped_into_free_family() {
    let dir = tempfile::tempdir().unwrap();
    write_conf_d(dir.path(), FREE_FAMILY_TOML);
    let state = state_for(dir.path()).await;

    // A conf.d edit injects a snapshot-priced paid ID into the free
    // family; the next hot-swap must refuse it with 400 naming alias +
    // member (fail closed), persisting nothing and keeping the live
    // router on the previous route.
    std::fs::write(
        dir.path().join("conf.d/20-family.toml"),
        FREE_FAMILY_TOML.replace("model-b:free", "gpt-4o"),
    )
    .unwrap();

    let err = post_route(State(state.clone()), Json(valid_route()))
        .await
        .unwrap_err();
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    let msg = err.1 .0["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("auto-coding") && msg.contains("gpt-4o"),
        "rejection must name alias + member, got: {msg}"
    );
    assert!(
        !RuntimeOverrides::path(dir.path()).exists(),
        "a rejected hot-swap must not be persisted"
    );
    assert_eq!(
        state.dispatch_router.load().candidate_names(),
        vec!["anthropic".to_string()],
        "the live router must keep serving the previous route"
    );
}

#[tokio::test]
async fn post_route_should_reject_unknown_family_alias() {
    let dir = tempfile::tempdir().unwrap();
    write_conf_d(dir.path(), FREE_FAMILY_TOML);
    let state = state_for(dir.path()).await;

    // The posted route opts into an alias with no [[model_families]]
    // entry: 400 naming route + alias (never a live "family" route with
    // null detail), persisting nothing and keeping the live router.
    let mut route = valid_route();
    route.name = "default".to_string();
    route.family = Some("no-such-alias".to_string());

    let err = post_route(State(state.clone()), Json(route))
        .await
        .unwrap_err();
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    let msg = err.1 .0["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("default") && msg.contains("no-such-alias"),
        "rejection must name route + alias, got: {msg}"
    );
    assert!(
        !RuntimeOverrides::path(dir.path()).exists(),
        "a rejected hot-swap must not be persisted"
    );
    assert_eq!(
        state.dispatch_router.load().candidate_names(),
        vec!["anthropic".to_string()],
        "the live router must keep serving the previous route"
    );
}

#[tokio::test]
async fn post_route_should_reject_family_member_with_unknown_upstream() {
    let dir = tempfile::tempdir().unwrap();
    write_conf_d(dir.path(), FREE_FAMILY_TOML);
    let state = state_for(dir.path()).await;

    std::fs::write(
        dir.path().join("conf.d/20-family.toml"),
        FREE_FAMILY_TOML.replace("upstream = \"bedrock\"", "upstream = \"does-not-exist\""),
    )
    .unwrap();

    let err = post_route(State(state), Json(valid_route()))
        .await
        .unwrap_err();
    assert_eq!(err.0, StatusCode::BAD_REQUEST);
    let msg = err.1 .0["error"].as_str().unwrap_or_default();
    assert!(
        msg.contains("does-not-exist"),
        "rejection must name the unknown upstream, got: {msg}"
    );
}
