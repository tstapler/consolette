//! Epic 7 paid-alias isolation (auto-model-family Story 7.1).
//!
//! Traffic on both aliases against mock upstreams: per-alias
//! `ResolutionCounters` stay separate and the free↔paid stats-key sets
//! (via [`scoped_stats_key`]) are disjoint, so neither side leaks into the
//! other. Dispatch-level resolution goes through the real `Router::dispatch`
//! family gate; counter recording uses the paid-aware seam Epic 3 will wire
//! into dispatch (call site noted in `family.rs`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use http::HeaderMap;
use serde_json::json;

use consolette::config::schema::{Config, FamilyMember, ModelFamily};
use consolette::config::validate_free_guard;
use consolette::metrics::MetricsCollector;
use consolette::providers::{ModelInfo, Provider, ProviderError, ProviderResponse};
use consolette::ratelimit::{AdmissionControl, Admit};
use consolette::routing::family::{
    is_verifiably_free, scoped_stats_key, FamilyTable, PerAliasCounters,
};
use consolette::routing::health::HealthRegistry;
use consolette::routing::router::Router;
use consolette::routing::strategy::{FallbackStrategy, UpstreamRef};

struct CapturingProvider {
    received_body: Arc<std::sync::Mutex<Option<serde_json::Value>>>,
}

#[async_trait::async_trait]
impl Provider for CapturingProvider {
    fn name(&self) -> &str {
        "mock"
    }

    async fn send(
        &self,
        body: serde_json::Value,
        _headers: HeaderMap,
        _stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        *self.received_body.lock().unwrap() = Some(body);
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

/// Both-alias config (Story 7.1 Task 1): free pool + opt-in paid pool.
/// `gpt-4o` is the vendored-snapshot paid fixture (proven paid by the
/// FreeGuard tests); `:free`-suffixed IDs are verifiably free.
fn both_alias_families() -> Vec<ModelFamily> {
    vec![
        ModelFamily {
            alias: "auto-coding".to_string(),
            members: vec![
                FamilyMember {
                    upstream: "mock".to_string(),
                    model: "model-a:free".to_string(),
                },
                FamilyMember {
                    upstream: "mock".to_string(),
                    model: "model-b:free".to_string(),
                },
            ],
            allow_paid: false,
        },
        ModelFamily {
            alias: "auto-coding-paid".to_string(),
            members: vec![FamilyMember {
                upstream: "mock".to_string(),
                model: "gpt-4o".to_string(),
            }],
            allow_paid: true,
        },
    ]
}

fn mock_router(
    table: Arc<FamilyTable>,
    active_family: &str,
) -> (Router, Arc<std::sync::Mutex<Option<serde_json::Value>>>) {
    let received_body = Arc::new(std::sync::Mutex::new(None));
    let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(CapturingProvider {
        received_body: received_body.clone(),
    })];
    let router = Router::new(
        vec![UpstreamRef {
            index: 0,
            name: "mock".to_string(),
            weight: 1.0,
            model: None,
        }],
        providers,
        Arc::new(FallbackStrategy),
        Arc::new(HealthRegistry::new(300)),
        Arc::new(AlwaysAllow),
        MetricsCollector::new(),
    )
    .with_family_table(table, Some(active_family.to_string()));
    (router, received_body)
}

fn seen_model(received_body: &Arc<std::sync::Mutex<Option<serde_json::Value>>>) -> String {
    received_body
        .lock()
        .unwrap()
        .clone()
        .expect("provider must have been called")["model"]
        .as_str()
        .expect("model must be a string")
        .to_string()
}

#[tokio::test]
async fn dispatch_should_keep_free_and_paid_stats_separate_when_both_aliases_resolve() {
    // Load-time isolation: the two-alias config passes FreeGuard (paid IDs
    // live only under allow_paid=true), while the same paid ID in the free
    // family fails closed.
    let mut config = Config::default();
    config.families = both_alias_families();
    assert!(
        validate_free_guard(&config).is_ok(),
        "paid alias config must load clean under allow_paid=true"
    );
    let mut polluted = config.clone();
    polluted.families[0].members.push(FamilyMember {
        upstream: "mock".to_string(),
        model: "gpt-4o".to_string(),
    });
    assert!(
        validate_free_guard(&polluted).is_err(),
        "paid ID in the free family must fail closed at load"
    );

    // Dispatch half: each alias resolves through the real dispatch gate to
    // its own pool's ID — the free alias never serves the paid ID.
    let table = Arc::new(FamilyTable::from_config(&config));
    let mut counters = PerAliasCounters::new();

    let (free_router, free_seen) = mock_router(table.clone(), "auto-coding");
    free_router
        .dispatch(json!({"model": "auto-coding"}), HeaderMap::new(), false, 0)
        .await
        .expect("free alias dispatch must succeed");
    let free_served = seen_model(&free_seen);
    assert_eq!(free_served, "model-a:free");
    assert!(is_verifiably_free(&free_served));
    counters.record_resolution("auto-coding", !is_verifiably_free(&free_served));

    let (paid_router, paid_seen) = mock_router(table.clone(), "auto-coding-paid");
    paid_router
        .dispatch(
            json!({"model": "auto-coding-paid"}),
            HeaderMap::new(),
            false,
            0,
        )
        .await
        .expect("paid alias dispatch must succeed");
    let paid_served = seen_model(&paid_seen);
    assert_eq!(paid_served, "gpt-4o");
    counters.record_resolution("auto-coding-paid", !is_verifiably_free(&paid_served));

    // Counter separation: one bucket per alias, paid signal only on the
    // paid side — the free bucket is the paid-leak audit (stays 0).
    let free = counters.get("auto-coding").expect("free counters recorded");
    let paid = counters
        .get("auto-coding-paid")
        .expect("paid counters recorded");
    assert_eq!(free.resolutions_total, 1);
    assert_eq!(free.paid_resolutions, 0);
    assert_eq!(free.fallback_to_default_total, 0);
    assert_eq!(paid.resolutions_total, 1);
    assert_eq!(paid.paid_resolutions, 1);
    assert_eq!(paid.fallback_to_default_total, 0);

    // Stats-key separation (the alias-scoping discipline Epic 2 adopts for
    // `MemberStatsMap`): free member keys and paid member keys are disjoint,
    // so per-model stats can never leak across the free↔paid boundary even
    // when both members share one upstream.
    let free_keys: Vec<_> = config.families[0]
        .members
        .iter()
        .map(|m| scoped_stats_key("auto-coding", &m.upstream, &m.model))
        .collect();
    let paid_keys: Vec<_> = config.families[1]
        .members
        .iter()
        .map(|m| scoped_stats_key("auto-coding-paid", &m.upstream, &m.model))
        .collect();
    assert_eq!(free_keys.len(), 2);
    assert_eq!(paid_keys.len(), 1);
    for key in &paid_keys {
        assert!(
            !free_keys.contains(key),
            "paid stats key {key:?} must not collide with any free stats key"
        );
    }
}
