//! `[[model_tiers]]`: per-client-model overrides of a route candidate's model.

use std::collections::BTreeMap;

use super::*;
use crate::config::schema::{ModelTier, TierUpstreamOverride};

type Captured = Arc<std::sync::Mutex<Option<serde_json::Value>>>;

fn tier(name: &str, prefixes: &[&str], overrides: &[(&str, TierUpstreamOverride)]) -> ModelTier {
    ModelTier {
        name: name.to_string(),
        match_prefixes: prefixes.iter().map(ToString::to_string).collect(),
        upstreams: overrides
            .iter()
            .map(|(n, o)| ((*n).to_string(), o.clone()))
            .collect::<BTreeMap<_, _>>(),
    }
}

fn pin_model(model: &str) -> TierUpstreamOverride {
    TierUpstreamOverride {
        model: Some(model.to_string()),
        model_family: None,
    }
}

fn pin_family(family: &str) -> TierUpstreamOverride {
    TierUpstreamOverride {
        model: None,
        model_family: Some(family.to_string()),
    }
}

/// Two capturing upstreams ("alpha" default-pinned to `alpha-default`, "beta" unpinned) behind
/// a fallback router carrying `tiers`.
fn tiered_router(tiers: Vec<ModelTier>) -> (Router, Captured, Captured, Arc<MetricsCollector>) {
    let alpha: Captured = Arc::default();
    let beta: Captured = Arc::default();
    let metrics = MetricsCollector::new();
    let providers: Vec<Arc<dyn Provider>> = vec![
        Arc::new(CapturingProvider {
            name: "alpha",
            received_body: Arc::clone(&alpha),
        }),
        Arc::new(CapturingProvider {
            name: "beta",
            received_body: Arc::clone(&beta),
        }),
    ];
    let candidate = |index, name: &str, model: Option<&str>| UpstreamRef {
        index,
        name: name.to_string(),
        weight: 1.0,
        model: model.map(ToString::to_string),
        model_family: None,
    };
    let router = Router::new(RouterDeps {
        candidates: vec![
            candidate(0, "alpha", Some("alpha-default")),
            candidate(1, "beta", None),
        ],
        providers,
        strategy: Arc::new(FallbackStrategy),
        health: Arc::new(HealthRegistry::new(300)),
        admission: Arc::new(AlwaysAllow),
        metrics: Arc::clone(&metrics),
    })
    .with_model_tiers(tiers);
    (router, alpha, beta, metrics)
}

#[allow(clippy::unwrap_used)]
fn sent_model(captured: &Captured) -> Option<String> {
    captured
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|b| b["model"].as_str().map(ToString::to_string))
}

async fn dispatch_model(router: &Router, model: &str) {
    router
        .dispatch(
            serde_json::json!({"model": model}),
            HeaderMap::new(),
            false,
            0,
        )
        .await
        .unwrap_or_else(|e| panic!("dispatch failed: {e:?}"));
}

#[tokio::test]
async fn dispatch_should_apply_tier_override_when_requested_model_prefix_matches() {
    let (router, alpha, _, _) = tiered_router(vec![tier(
        "haiku",
        &["claude-haiku"],
        &[("alpha", pin_model("cheap-model"))],
    )]);

    dispatch_model(&router, "claude-haiku-4-5").await;

    assert_eq!(sent_model(&alpha).as_deref(), Some("cheap-model"));
}

#[tokio::test]
async fn dispatch_should_match_tier_prefix_case_insensitively() {
    let (router, alpha, _, _) = tiered_router(vec![tier(
        "haiku",
        &["Claude-HAIKU"],
        &[("alpha", pin_model("cheap-model"))],
    )]);

    dispatch_model(&router, "CLAUDE-haiku-4-5").await;

    assert_eq!(sent_model(&alpha).as_deref(), Some("cheap-model"));
}

#[tokio::test]
async fn dispatch_should_not_match_tier_on_non_prefix_substring() {
    let (router, alpha, _, _) = tiered_router(vec![tier(
        "haiku",
        &["haiku"],
        &[("alpha", pin_model("cheap-model"))],
    )]);

    dispatch_model(&router, "claude-haiku-4-5").await;

    assert_eq!(sent_model(&alpha).as_deref(), Some("alpha-default"));
}

#[tokio::test]
async fn dispatch_should_use_first_matching_tier_when_several_match() {
    let (router, alpha, _, _) = tiered_router(vec![
        tier(
            "first",
            &["claude-"],
            &[("alpha", pin_model("first-model"))],
        ),
        tier(
            "second",
            &["claude-haiku"],
            &[("alpha", pin_model("second-model"))],
        ),
    ]);

    dispatch_model(&router, "claude-haiku-4-5").await;

    assert_eq!(sent_model(&alpha).as_deref(), Some("first-model"));
}

#[tokio::test]
async fn dispatch_should_keep_route_defaults_when_no_tier_matches() {
    let (router, alpha, _, _) = tiered_router(vec![tier(
        "haiku",
        &["claude-haiku"],
        &[("alpha", pin_model("cheap-model"))],
    )]);

    dispatch_model(&router, "claude-opus-5").await;

    assert_eq!(sent_model(&alpha).as_deref(), Some("alpha-default"));
}

#[tokio::test]
async fn dispatch_should_override_only_the_upstreams_the_tier_names() {
    let (router, alpha, beta, _) = tiered_router(vec![tier(
        "haiku",
        &["claude-haiku"],
        &[
            ("beta", pin_model("beta-cheap")),
            ("not-in-route", pin_model("ignored")),
        ],
    )]);

    dispatch_model(&router, "claude-haiku-4-5").await;
    assert_eq!(
        sent_model(&alpha).as_deref(),
        Some("alpha-default"),
        "alpha has no override and is first in the route"
    );

    // Fail alpha over to beta to prove beta gets its tier model.
    router.health.trip(0, None);
    dispatch_model(&router, "claude-haiku-4-5").await;
    assert_eq!(sent_model(&beta).as_deref(), Some("beta-cheap"));
}

#[tokio::test]
async fn dispatch_should_record_tier_model_as_pinned_routed_model() {
    let (router, _, _, metrics) = tiered_router(vec![tier(
        "haiku",
        &["claude-haiku"],
        &[("alpha", pin_model("cheap-model"))],
    )]);

    dispatch_model(&router, "claude-haiku-4-5").await;

    let recent = metrics.get_recent_requests(1);
    assert_eq!(
        recent[0].requested_model.as_deref(),
        Some("claude-haiku-4-5")
    );
    assert_eq!(recent[0].routed_model.as_deref(), Some("cheap-model"));
    assert_eq!(
        recent[0].routed_model_source,
        Some(crate::metrics::trace::RoutedModelSource::Pinned)
    );
}

/// Records the body it was sent and reports a resolved model, as `OpenaiProvider::send` does.
struct FamilyCapturingProvider(Captured);

#[async_trait::async_trait]
impl Provider for FamilyCapturingProvider {
    fn name(&self) -> &str {
        "openai"
    }

    #[allow(clippy::unwrap_used)]
    async fn send(
        &self,
        body: serde_json::Value,
        _headers: HeaderMap,
        _stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        *self.0.lock().unwrap() = Some(body);
        crate::providers::note_resolved_model("gpt-6.1-sol");
        Ok(ProviderResponse::Full(serde_json::json!({"ok": true})))
    }

    async fn list_models(&self) -> Result<Vec<crate::providers::ModelInfo>, ProviderError> {
        Ok(Vec::new())
    }
}

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn dispatch_should_record_resolved_model_when_tier_selects_a_model_family() {
    let metrics = MetricsCollector::new();
    let sent: Captured = Arc::default();
    let router = Router::new(RouterDeps {
        candidates: vec![UpstreamRef {
            index: 0,
            name: "openai".to_string(),
            weight: 1.0,
            model: None,
            model_family: Some("gpt-default".to_string()),
        }],
        providers: vec![Arc::new(FamilyCapturingProvider(Arc::clone(&sent)))],
        strategy: Arc::new(FallbackStrategy),
        health: Arc::new(HealthRegistry::new(300)),
        admission: Arc::new(AlwaysAllow),
        metrics: Arc::clone(&metrics),
    })
    .with_model_tiers(vec![tier(
        "haiku",
        &["claude-haiku"],
        &[("openai", pin_family("gpt-6-luna"))],
    )]);

    dispatch_model(&router, "claude-haiku-4-5").await;

    let body = sent.lock().unwrap().clone().unwrap();
    assert_eq!(body[MODEL_FAMILY_BODY_KEY], "gpt-6-luna");
    let recent = metrics.get_recent_requests(1);
    assert_eq!(recent[0].routed_model.as_deref(), Some("gpt-6.1-sol"));
    assert_eq!(
        recent[0].routed_model_source,
        Some(crate::metrics::trace::RoutedModelSource::Pinned)
    );
}

#[tokio::test]
async fn dispatch_forced_should_apply_tier_override_for_the_forced_upstream() {
    let (router, _, beta, _) = tiered_router(vec![tier(
        "haiku",
        &["claude-haiku"],
        &[("beta", pin_model("beta-cheap"))],
    )]);
    let body = serde_json::json!({"model": "claude-haiku-4-5"});
    let mut scope = TraceScope::begin(&router.metrics, &body, false, 0);
    scope.force_upstream("beta".to_string());

    let res = router
        .dispatch_traced(&mut scope, body, HeaderMap::new(), false, 0)
        .await;

    assert!(res.is_ok());
    assert_eq!(sent_model(&beta).as_deref(), Some("beta-cheap"));
}

#[tokio::test]
async fn from_config_should_carry_model_tiers_into_the_router() {
    let config = crate::config::schema::Config {
        model_tiers: vec![tier(
            "haiku",
            &["claude-haiku"],
            &[("bedrock", pin_model("cheap-model"))],
        )],
        ..Default::default()
    };

    let router = Router::from_config(&config, MetricsCollector::new())
        .await
        .unwrap_or_else(|e| panic!("from_config failed: {e}"));

    let mapped = router.apply_model_tiers(
        router.candidates().to_vec(),
        &serde_json::json!({"model": "claude-haiku-4-5"}),
    );
    let model_of = |name: &str| {
        mapped
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.model.clone())
    };
    assert_eq!(model_of("bedrock"), Some(Some("cheap-model".to_string())));
    assert_eq!(model_of("anthropic"), Some(None));
}

#[tokio::test]
async fn dispatch_should_match_tier_prefix_with_surrounding_whitespace() {
    let (router, alpha, _, _) = tiered_router(vec![tier(
        "haiku",
        &["  Claude-Haiku "],
        &[("alpha", pin_model("cheap-model"))],
    )]);

    dispatch_model(&router, "claude-haiku-4-5").await;

    assert_eq!(sent_model(&alpha).as_deref(), Some("cheap-model"));
}

#[tokio::test]
async fn dispatch_should_prefer_session_pin_over_matching_tier() {
    let (router, alpha, beta, _) = tiered_router(vec![tier(
        "haiku",
        &["claude-haiku"],
        &[("beta", pin_model("beta-cheap"))],
    )]);
    let overrides = Arc::new(crate::routing::session_overrides::SessionOverrideStore::new());
    overrides.set(
        "sess-1".to_string(),
        crate::routing::session_overrides::SessionOverride {
            upstream: "beta".to_string(),
            model: Some("pinned-model".to_string()),
        },
    );
    let router = router.with_session_overrides(overrides);

    router
        .dispatch(
            serde_json::json!({"model": "claude-haiku-4-5", "metadata": {"user_id": "sess-1"}}),
            HeaderMap::new(),
            false,
            0,
        )
        .await
        .unwrap_or_else(|e| panic!("dispatch failed: {e:?}"));

    assert_eq!(sent_model(&alpha), None, "pin routes only to beta");
    assert_eq!(sent_model(&beta).as_deref(), Some("pinned-model"));
}
