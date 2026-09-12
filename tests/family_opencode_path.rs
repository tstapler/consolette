//! Opencode provider path (auto-model-family Epic 6, validation R2):
//! `POST /v1/chat/completions` with `model="auto-coding"` against mock
//! upstreams — the upstream never sees the verbatim alias because dispatch
//! resolution overwrites it with the resolved real ID.
//!
//! HTTP-endpoint mapping (drives the real `entrypoint_router`, not dispatch
//! directly, matching the `family_metrics` precedent):
//! - `POST /v1/chat/completions {model: "auto-coding", ...}` == the opencode
//!   `consolette` provider request (translated via
//!   `translate_openai_to_anthropic`, then dispatched; the alias survives
//!   translation — proven unit-level by
//!   `chat_completions_should_carry_family_alias_into_dispatch_when_model_is_auto_coding`
//!   — and is overwritten here).
//! - `store.set/clear(..)` == `POST/DELETE /api/sessions/{id}/route` (the
//!   AC3 session-pin leg asserts Epic 4 Story 4.1 AC3 from the opencode path
//!   rather than re-implementing it).
//! - `GET /api/models` == the passthrough catalog check: the endpoint keeps
//!   its per-upstream shape; the synthetic alias is NOT injected there (see
//!   the C1 note below), the family label lives in the repo-external
//!   `opencode.json` `consolette` provider `models` map (README documents
//!   the exact snippet).
//!
//! C1 note (adversarial-review C1, still open): `GET /api/models` passes
//! through each upstream's live catalog (`src/entrypoint/api.rs`), so a
//! synthetic alias can never appear there without new injection code — and
//! Epic 6 owns no `api.rs` behavior change. This test therefore asserts the
//! passthrough shape stays intact (per-upstream entries present) instead of
//! asserting alias injection, and records the gap rather than faking it.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use serde_json::json;
use tower::ServiceExt;

use consolette::config::schema::{Config, FamilyMember, ModelFamily};
use consolette::entrypoint::{entrypoint_router, EntrypointState};
use consolette::metrics::MetricsCollector;
use consolette::providers::{ModelInfo, Provider, ProviderError, ProviderResponse};
use consolette::ratelimit::{AdmissionControl, Admit};
use consolette::routing::family::FamilyTable;
use consolette::routing::health::HealthRegistry;
use consolette::routing::router::Router;
use consolette::routing::session_overrides::{SessionOverride, SessionOverrideStore};
use consolette::routing::strategy::{FallbackStrategy, UpstreamRef};

/// Mock upstream: records every body it receives and answers with a minimal
/// Anthropic-shaped message (the shape `translate_and_record` needs: `model`,
/// `content`, `usage`), so the chat-completions handler can translate back
/// to the OpenAI envelope.
struct CapturingProvider {
    name: String,
    received: Arc<Mutex<Vec<serde_json::Value>>>,
}

#[async_trait::async_trait]
impl Provider for CapturingProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn send(
        &self,
        body: serde_json::Value,
        _headers: http::HeaderMap,
        _stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        self.received.lock().unwrap().push(body);
        Ok(ProviderResponse::Full(json!({
            "id": "msg_opencode_1",
            "type": "message",
            "role": "assistant",
            "model": "model-a:free",
            "content": [{"type": "text", "text": "hello from the family"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 10, "output_tokens": 5}
        })))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
}

struct AlwaysAllow;

#[async_trait::async_trait]
impl AdmissionControl for AlwaysAllow {
    async fn admit(&self, _upstream: &str, _est_tokens: u32) -> Admit {
        Admit::Allowed
    }
}

struct OpencodeHarness {
    state: EntrypointState,
    store: Arc<SessionOverrideStore>,
    bodies_a: Arc<Mutex<Vec<serde_json::Value>>>,
    bodies_b: Arc<Mutex<Vec<serde_json::Value>>>,
}

/// On-disk conf.d the harness's `config_dir` points at. The upstreams are
/// `openai`-kind pointed at a dead loopback port: `OpenaiProvider::new`
/// constructs without I/O, and `list_models` fails fast (connection refused)
/// into the documented per-upstream `{"error": ...}` entry — exactly the
/// passthrough shape the `GET /api/models` leg asserts.
fn write_conf_d(dir: &std::path::Path) {
    let conf_d = dir.join("conf.d");
    std::fs::create_dir_all(&conf_d).unwrap();
    std::fs::write(
        conf_d.join("00-upstreams.toml"),
        "[[upstreams]]\nname = \"mock-a\"\nkind = \"openai\"\nbase_url = \"http://127.0.0.1:9\"\n\n\
         [[upstreams]]\nname = \"mock-b\"\nkind = \"openai\"\nbase_url = \"http://127.0.0.1:9\"\n",
    )
    .unwrap();
    std::fs::write(
        conf_d.join("10-routing.toml"),
        "[[routes]]\nname = \"default\"\nstrategy = \"fallback\"\nfamily = \"auto-coding\"\n\n\
         [[routes.upstreams]]\nname = \"mock-a\"\n\n\
         [[routes.upstreams]]\nname = \"mock-b\"\n",
    )
    .unwrap();
    std::fs::write(
        conf_d.join("20-family.toml"),
        "[[model_families]]\nalias = \"auto-coding\"\nallow_paid = false\n\n\
         [[model_families.members]]\nupstream = \"mock-a\"\nmodel = \"model-a:free\"\n\n\
         [[model_families.members]]\nupstream = \"mock-b\"\nmodel = \"model-b:free\"\n",
    )
    .unwrap();
}

async fn opencode_harness() -> OpencodeHarness {
    let dir = tempfile::tempdir().unwrap();
    write_conf_d(dir.path());
    // Keep the tempdir alive for the whole test via the config_dir Arc:
    // leak the path by forgetting the TempDir handle after cloning the path.
    // (The directory is cleaned up by the OS temp reaper; correctness of the
    // test never depends on post-test cleanup.)
    let config_dir = dir.keep();

    let mut config = Config::default();
    config.families = vec![ModelFamily {
        alias: "auto-coding".to_string(),
        members: vec![
            FamilyMember {
                upstream: "mock-a".to_string(),
                model: "model-a:free".to_string(),
            },
            FamilyMember {
                upstream: "mock-b".to_string(),
                model: "model-b:free".to_string(),
            },
        ],
        allow_paid: false,
    }];
    let table = Arc::new(FamilyTable::from_config(&config));

    let bodies_a = Arc::new(Mutex::new(Vec::new()));
    let bodies_b = Arc::new(Mutex::new(Vec::new()));
    let providers: Vec<Arc<dyn Provider>> = vec![
        Arc::new(CapturingProvider {
            name: "mock-a".to_string(),
            received: bodies_a.clone(),
        }),
        Arc::new(CapturingProvider {
            name: "mock-b".to_string(),
            received: bodies_b.clone(),
        }),
    ];
    let metrics = MetricsCollector::new();
    let store = Arc::new(SessionOverrideStore::new());
    let router = Router::new(
        vec![
            UpstreamRef {
                index: 0,
                name: "mock-a".to_string(),
                weight: 1.0,
                model: None,
            },
            UpstreamRef {
                index: 1,
                name: "mock-b".to_string(),
                weight: 1.0,
                model: None,
            },
        ],
        providers,
        Arc::new(FallbackStrategy),
        Arc::new(HealthRegistry::new(300)),
        Arc::new(AlwaysAllow),
        metrics.clone(),
    )
    .with_session_overrides(store.clone())
    .with_family_table(table, Some("auto-coding".to_string()));

    let state = EntrypointState {
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
        config_dir: Arc::new(config_dir),
        session_overrides: store.clone(),
    };
    OpencodeHarness {
        state,
        store,
        bodies_a,
        bodies_b,
    }
}

async fn post_chat(
    state: &EntrypointState,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let router = entrypoint_router(state.clone());
    let resp = router
        .oneshot(
            Request::post("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

/// Models each mock upstream has received so far (in arrival order).
fn seen_models(harness: &OpencodeHarness) -> (Vec<String>, Vec<String>) {
    let models = |bodies: &Arc<Mutex<Vec<serde_json::Value>>>| {
        bodies
            .lock()
            .unwrap()
            .iter()
            .map(|b| {
                b["model"]
                    .as_str()
                    .expect("model must be a string")
                    .to_string()
            })
            .collect::<Vec<_>>()
    };
    (models(&harness.bodies_a), models(&harness.bodies_b))
}

#[tokio::test]
async fn chat_completions_should_overwrite_alias_with_resolved_id_when_family_route_active() {
    // Story 6.1 AC2 + AC1-dispatch-half: the opencode-shaped request carries
    // `model="auto-coding"`; the translated body still carries the alias into
    // dispatch (unit-proven), and dispatch overwrites it with the resolved
    // real ID — the upstream never sees the verbatim alias.
    let harness = opencode_harness().await;

    let (status, json) = post_chat(
        &harness.state,
        json!({
            "model": "auto-coding",
            "messages": [{"role": "user", "content": "hi"}],
            "stream": false,
        }),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "chat completion must succeed: {json}"
    );
    assert_eq!(json["object"], json!("chat.completion"));
    assert_eq!(
        json["choices"][0]["message"]["content"],
        json!("hello from the family")
    );

    // Cold start (no stats yet): config-order first member serves.
    let (seen_a, seen_b) = seen_models(&harness);
    assert_eq!(seen_a, vec!["model-a:free".to_string()]);
    assert!(seen_b.is_empty());
    assert!(
        !seen_a
            .iter()
            .chain(seen_b.iter())
            .any(|m| m == "auto-coding"),
        "upstream must never see the verbatim alias"
    );

    // Story 6.1 AC3 (asserted from the opencode path, not re-implemented):
    // Epic 4 Story 4.1 AC3 is green (`family_sessions` 3/3 incl. the
    // opencode-shaped pin test), so a pinned session on
    // `/v1/chat/completions` must stick to the pin here too.
    // `POST /api/sessions/s1/route {upstream: "mock-b", model: "model-b:free"}`.
    harness.store.set(
        "s1".to_string(),
        SessionOverride {
            upstream: "mock-b".to_string(),
            model: Some("model-b:free".to_string()),
        },
    );
    let (status, _) = post_chat(
        &harness.state,
        json!({
            "model": "auto-coding",
            "messages": [{"role": "user", "content": "hi"}],
            "user": "s1",
            "stream": false,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (seen_a, seen_b) = seen_models(&harness);
    assert_eq!(
        seen_a.len(),
        1,
        "pinned session must not touch mock-a again"
    );
    assert_eq!(seen_b, vec!["model-b:free".to_string()]);
    assert!(
        !seen_a
            .iter()
            .chain(seen_b.iter())
            .any(|m| m == "auto-coding"),
        "pinned opencode traffic must not leak the alias either"
    );

    // `DELETE /api/sessions/s1/route`: family resolution resumes for `s1`.
    harness.store.clear("s1");
    let (status, _) = post_chat(
        &harness.state,
        json!({
            "model": "auto-coding",
            "messages": [{"role": "user", "content": "hi"}],
            "user": "s1",
            "stream": false,
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (seen_a, seen_b) = seen_models(&harness);
    assert_eq!(seen_a.len(), 2, "clear-pin must resume family resolution");
    assert_eq!(seen_a[1], "model-a:free".to_string());
    assert_eq!(seen_b.len(), 1);

    // Story 6.1 AC1 `GET /api/models` half (passthrough check): the endpoint
    // keeps its per-upstream shape — one entry per configured upstream, each
    // carrying that upstream's live catalog (here: the documented inline
    // `{"error": ...}` since nothing listens on the dead port). The family
    // alias itself is NOT injected (C1: injection would be an `api.rs`
    // behavior change, out of Epic 6 scope); opencode surfaces the alias via
    // its own `consolette` provider `models` map (README documents it).
    let router = entrypoint_router(harness.state.clone());
    let resp = router
        .oneshot(Request::get("/api/models").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let models: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let upstreams = models.as_object().expect("/api/models must be an object");
    assert!(
        upstreams.contains_key("mock-a"),
        "passthrough must list mock-a: {models}"
    );
    assert!(
        upstreams.contains_key("mock-b"),
        "passthrough must list mock-b: {models}"
    );
    assert!(
        models["mock-a"].get("error").is_some(),
        "dead-port upstream must report its error inline, not fail the response: {models}"
    );
}
