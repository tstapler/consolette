//! `Router::with_provider_names` / `providers_named` (Story 3.1.3b).
use super::*;
use crate::metrics::ids::UpstreamName;

fn ok_provider(name: &'static str) -> Arc<dyn Provider> {
    Arc::new(AlwaysOkProvider {
        name,
        call_count: Arc::new(AtomicU32::new(0)),
    })
}

fn router_with_two_providers() -> Router {
    fallback_router_with_metrics(
        vec![upstream(0, "a"), upstream(1, "b")],
        vec![ok_provider("a"), ok_provider("b")],
        &MetricsCollector::new(),
    )
}

#[tokio::test]
async fn providers_named_should_return_config_order_and_empty_when_names_missing_or_length_mismatch(
) {
    let no_builder = router_with_two_providers();
    assert!(no_builder.providers_named().is_empty());

    let mismatched = router_with_two_providers().with_provider_names(vec!["a".into()]);
    assert!(mismatched.providers_named().is_empty());

    let named = router_with_two_providers().with_provider_names(vec!["a".into(), "b".into()]);
    let pairs = named.providers_named();
    let names: Vec<&UpstreamName> = pairs.iter().map(|p| &p.name).collect();
    assert_eq!(names, [&UpstreamName::from("a"), &UpstreamName::from("b")]);
    assert_eq!(pairs[1].provider.name(), "b");
}

#[tokio::test]
#[allow(clippy::expect_used)]
async fn from_config_should_carry_upstream_names_in_config_order() {
    let router = Router::from_config(&Config::default(), MetricsCollector::new())
        .await
        .expect("default config builds");
    let names: Vec<String> = router
        .providers_named()
        .into_iter()
        .map(|p| p.name.to_string())
        .collect();
    assert_eq!(names, ["anthropic", "bedrock"]);
}

fn fingerprints(config: &Config) -> Vec<crate::routing::model_catalog::UpstreamFingerprint> {
    super::super::provider_names::fingerprints_of(config)
}

#[test]
fn fingerprints_should_ignore_route_changes_and_track_base_url() {
    let base = Config::default();
    let mut route_changed = base.clone();
    route_changed.routes.reverse();
    route_changed.cooldown_seconds += 1;
    assert_eq!(fingerprints(&base), fingerprints(&route_changed));

    let mut url_changed = base.clone();
    if let UpstreamKind::Anthropic { base_url } = &mut url_changed.upstreams[0].kind {
        *base_url = "https://example.invalid".into();
    }
    let (before, after) = (fingerprints(&base), fingerprints(&url_changed));
    assert_ne!(before[0], after[0]);
    assert_eq!(before[1], after[1]);
}
