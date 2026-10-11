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
    let names: Vec<&UpstreamName> = pairs.iter().map(|(n, _)| n).collect();
    assert_eq!(names, [&UpstreamName::from("a"), &UpstreamName::from("b")]);
    assert_eq!(pairs[1].1.name(), "b");
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
        .map(|(n, _)| n.to_string())
        .collect();
    assert_eq!(names, ["anthropic", "bedrock"]);
}
