//! Session pin/move dynamics (auto-model-family Epic 4, STICKY-PER-SESSION).
//!
//! Pins-first: a live session pin bypasses family expansion (`dispatch`
//! asserts this via `session_pin_active`). Auto-stickiness: an unpinned
//! session's first family resolution records the pick in the
//! `SessionOverrideStore`-compatible sticky table; later requests reuse it
//! until the K=50 window lapses or the stuck member hits a
//! cooldown/exclusion event. Explicit pins always win over auto-sticks.
//!
//! HTTP-endpoint mapping (these tests drive the same seams dispatch-level,
//! matching the `family_resolution`/`family_paid_alias` precedent):
//! - `store.set(..)` == `POST /api/sessions/{id}/route {upstream, model}`
//! - `store.clear(..)` == `DELETE /api/sessions/{id}/route` (204 either way)
//! - `store.get/list(..)` == `GET /api/sessions/{id}/route` / `GET /api/sessions`
//! - a body with `metadata.user_id` == `POST /v1/messages` (that handler
//!   passes the body straight into `dispatch`, metadata intact)
//! - `translate_openai_to_anthropic` + dispatch == `POST /v1/chat/completions`
//!   (that handler translates, then dispatches; the adapter must carry the
//!   session key through — Epic 4 Story 4.1 Task 2)
//!
//! Dashboard sessions-view note (Story 4.2 Task 3 — SKIP, recorded here per
//! plan): no dedicated sessions view is built in this epic. Per-session
//! serving state stays answerable via the family card's `pinned sessions: N`
//! count + `GET /api/sessions` link (ux.md criterion 12); Epic 5b owns the
//! card. No code in `dashboard.rs` is touched for this.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use http::HeaderMap;
use serde_json::json;

use consolette::config::schema::{Config, FamilyMember, ModelFamily};
use consolette::metrics::MetricsCollector;
use consolette::providers::{
    translate_openai_to_anthropic, ModelInfo, Provider, ProviderError, ProviderResponse,
};
use consolette::ratelimit::{AdmissionControl, Admit};
use consolette::routing::family::FamilyTable;
use consolette::routing::health::HealthRegistry;
use consolette::routing::router::Router;
use consolette::routing::session_overrides::{
    SessionOverride, SessionOverrideStore, STICKY_REEVALUATE_EVERY,
};
use consolette::routing::strategy::{FallbackStrategy, UpstreamRef};

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
        _headers: HeaderMap,
        _stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        self.received.lock().unwrap().push(body);
        Ok(ProviderResponse::Full(json!({"ok": true})))
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

struct SessionHarness {
    router: Router,
    store: Arc<SessionOverrideStore>,
    health: Arc<HealthRegistry>,
    metrics: Arc<MetricsCollector>,
    bodies_a: Arc<Mutex<Vec<serde_json::Value>>>,
    bodies_b: Arc<Mutex<Vec<serde_json::Value>>>,
}

fn session_harness() -> SessionHarness {
    let config = Config {
        families: vec![ModelFamily {
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
        }],
        ..Config::default()
    };
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
    let health = Arc::new(HealthRegistry::new(300));
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
        health.clone(),
        Arc::new(AlwaysAllow),
        metrics.clone(),
    )
    .with_session_overrides(store.clone())
    .with_family_table(table, Some("auto-coding".to_string()));

    SessionHarness {
        router,
        store,
        health,
        metrics,
        bodies_a,
        bodies_b,
    }
}

/// Seeds the ranked pick to B: A warm-bad (timeouts), B warm-good.
fn seed_pick_b(metrics: &MetricsCollector) {
    for _ in 0..25 {
        let _ = metrics.family.record_member(
            "mock-a",
            "model-a:free",
            Some(&ProviderError::Timeout),
            5000,
        );
        let _ = metrics
            .family
            .record_member("mock-b", "model-b:free", None, 50);
    }
}

fn messages_body(session: &str) -> serde_json::Value {
    json!({
        "model": "auto-coding",
        "messages": [{"role": "user", "content": "hi"}],
        "metadata": {"user_id": session},
    })
}

/// Dispatches once and returns the model ID the upstream actually received.
async fn dispatch_served_model(harness: &SessionHarness, body: serde_json::Value) -> String {
    let len_a = harness.bodies_a.lock().unwrap().len();
    let len_b = harness.bodies_b.lock().unwrap().len();
    harness
        .router
        .dispatch(body, HeaderMap::new(), false, 0)
        .await
        .expect("dispatch must succeed");
    let grew_a = harness.bodies_a.lock().unwrap().len() == len_a + 1;
    let grew_b = harness.bodies_b.lock().unwrap().len() == len_b + 1;
    assert!(
        grew_a ^ grew_b,
        "exactly one upstream must serve each request"
    );
    if grew_a {
        harness.bodies_a.lock().unwrap().last().unwrap()["model"]
            .as_str()
            .expect("model must be a string")
            .to_string()
    } else {
        harness.bodies_b.lock().unwrap().last().unwrap()["model"]
            .as_str()
            .expect("model must be a string")
            .to_string()
    }
}

#[tokio::test]
async fn dispatch_should_stick_to_pin_then_resume_family_when_pin_cleared() {
    // Story 4.1 AC1–AC3: pin beats family on `/v1/messages` AND the opencode
    // `/v1/chat/completions` shape; clear-pin resumes family resolution.
    let harness = session_harness();
    seed_pick_b(&harness.metrics);

    // Control first: unpinned traffic resolves to the ranked pick B.
    assert_eq!(
        dispatch_served_model(&harness, messages_body("s-other")).await,
        "model-b:free"
    );

    // `POST /api/sessions/s1/route {upstream: "mock-a", model: "model-a:free"}`
    harness.store.set(
        "s1".to_string(),
        SessionOverride {
            upstream: "mock-a".to_string(),
            model: Some("model-a:free".to_string()),
        },
    );

    // `/v1/messages` with `metadata.user_id="s1"`: pin wins over the B pick.
    assert_eq!(
        dispatch_served_model(&harness, messages_body("s1")).await,
        "model-a:free"
    );
    // Other sessions still resolve dynamically while `s1` is pinned.
    assert_eq!(
        dispatch_served_model(&harness, messages_body("s-other")).await,
        "model-b:free"
    );

    // Opencode-shaped `/v1/chat/completions` carrying the session key as the
    // OpenAI-native `user` field: the adapter must carry it through so the
    // pin applies here too.
    let openai_body = json!({
        "model": "auto-coding",
        "messages": [{"role": "user", "content": "hi"}],
        "user": "s1",
    });
    let translated = translate_openai_to_anthropic(&openai_body);
    assert_eq!(
        translated["model"],
        json!("auto-coding"),
        "the alias must survive translation into dispatch (Epic 6 AC2)"
    );
    assert_eq!(
        dispatch_served_model(&harness, translated).await,
        "model-a:free",
        "pinned session must stick to the pin on the opencode path"
    );

    // Same via `metadata.user_id` on the OpenAI shape.
    let openai_body = json!({
        "model": "auto-coding",
        "messages": [{"role": "user", "content": "hi"}],
        "metadata": {"user_id": "s1"},
    });
    let translated = translate_openai_to_anthropic(&openai_body);
    assert_eq!(
        dispatch_served_model(&harness, translated).await,
        "model-a:free"
    );

    // `DELETE /api/sessions/s1/route`: family resolution resumes for `s1`.
    harness.store.clear("s1");
    assert_eq!(
        dispatch_served_model(&harness, messages_body("s1")).await,
        "model-b:free"
    );
    // ... including on the opencode path.
    let translated = translate_openai_to_anthropic(&json!({
        "model": "auto-coding",
        "messages": [{"role": "user", "content": "hi"}],
        "user": "s1",
    }));
    assert_eq!(
        dispatch_served_model(&harness, translated).await,
        "model-b:free"
    );
}

#[tokio::test]
#[allow(non_snake_case)]
async fn session_should_stick_then_reevaluate_at_K50_when_no_health_event() {
    // Story 4.2 AC1–AC2: one session, cold start on A, 60 requests. The first
    // 50 serve the stuck pick A even after B demonstrably outranks it;
    // request 51 re-resolves to B and re-sticks. A cooldown event on a second
    // router triggers immediate re-evaluation instead of waiting for K.
    assert_eq!(
        STICKY_REEVALUATE_EVERY, 50,
        "confirmed STICKY-PER-SESSION cadence (plan Unresolved Questions)"
    );

    let harness = session_harness();
    let mut served = Vec::with_capacity(60);
    for _ in 0..50 {
        served.push(dispatch_served_model(&harness, messages_body("s3")).await);
    }
    assert!(
        served.iter().all(|m| m == "model-a:free"),
        "cold-start default A must serve the first 50 (got {served:?})"
    );

    // Poison A (overwhelmingly worse than B) AFTER the stick formed: requests
    // 1..50 already banked 50 A-successes, so outranking needs a heavy
    // history — 150 timeouts keep A at 75% err inside the 200-sample window.
    for _ in 0..150 {
        let _ = harness.metrics.family.record_member(
            "mock-a",
            "model-a:free",
            Some(&ProviderError::Timeout),
            5000,
        );
    }
    for _ in 0..25 {
        let _ = harness
            .metrics
            .family
            .record_member("mock-b", "model-b:free", None, 50);
    }

    for _ in 0..10 {
        served.push(dispatch_served_model(&harness, messages_body("s3")).await);
    }
    assert_eq!(served.len(), 60);
    assert!(
        served[..50].iter().all(|m| m == "model-a:free"),
        "first 50 must all serve the stuck pick A (got {:?})",
        &served[..50]
    );
    assert_eq!(
        served[50], "model-b:free",
        "request 51 must re-resolve to B once B outranks A"
    );
    assert!(
        served[51..].iter().all(|m| m == "model-b:free"),
        "post-reevaluation traffic must re-stick to B (got {:?})",
        &served[51..]
    );

    // Cooldown event: a fresh session sticks to A, then A's upstream cools —
    // the very next request re-evaluates immediately (no 50-request wait).
    let harness = session_harness();
    for _ in 0..3 {
        assert_eq!(
            dispatch_served_model(&harness, messages_body("s4")).await,
            "model-a:free"
        );
    }
    harness.health.trip(0, None);
    assert_eq!(
        dispatch_served_model(&harness, messages_body("s4")).await,
        "model-b:free",
        "cooldown on the stuck member must trigger immediate re-evaluation"
    );
    assert_eq!(
        dispatch_served_model(&harness, messages_body("s4")).await,
        "model-b:free",
        "post-event traffic must stick to the new pick"
    );
}

#[tokio::test]
async fn explicit_pin_to_member_should_override_family_for_one_session_only() {
    // Story 4.2 AC3: the explicit pin-to-member flow. Endpoint contract
    // (already implemented in `api.rs`, behavior unchanged here):
    //   POST /api/sessions/s2/route {"upstream":"mock-a","model":"model-a:free"}
    //     → 200 with the override echo
    //   ... traffic for s2 serves model-a:free verbatim ...
    //   GET /api/sessions → entry {"session_id":"s2","override":{"upstream":..,"model":..}}
    //   DELETE /api/sessions/s2/route → 204, family resumes.
    // `s2` stays on the pinned member while every other session keeps
    // resolving dynamically; the stored entry shows the member model ID
    // verbatim (what `GET /api/sessions` serves).
    let harness = session_harness();
    seed_pick_b(&harness.metrics);

    harness.store.set(
        "s2".to_string(),
        SessionOverride {
            upstream: "mock-a".to_string(),
            model: Some("model-a:free".to_string()),
        },
    );

    assert_eq!(
        dispatch_served_model(&harness, messages_body("s2")).await,
        "model-a:free",
        "explicitly pinned session must stay on the member"
    );
    assert_eq!(
        dispatch_served_model(&harness, messages_body("s-unpinned")).await,
        "model-b:free",
        "other sessions must still resolve dynamically"
    );

    let entry = harness
        .store
        .get("s2")
        .expect("pin must be listed while set");
    assert_eq!(entry.upstream, "mock-a");
    assert_eq!(
        entry.model.as_deref(),
        Some("model-a:free"),
        "`GET /api/sessions` must show the member model ID verbatim"
    );
    assert!(harness.store.list().contains_key("s2"));

    harness.store.clear("s2");
    assert_eq!(
        dispatch_served_model(&harness, messages_body("s2")).await,
        "model-b:free",
        "clear-pin must resume family resolution"
    );
}
