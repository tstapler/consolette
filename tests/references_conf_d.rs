//! Story 1.8.1 (REQ-15): `references/conf.d/00-providers.toml` is the
//! copy-pasteable example showing all four upstream kinds side by side. This
//! test loads it through the real `Config` loader (not just generic
//! `toml::Value` parsing, which `tests/toml_parity.rs` already covers) to
//! confirm it round-trips against the actual schema with zero
//! `deny_unknown_fields` errors, and that the Gemini route keeps the
//! blast-radius-limiting shape the plan calls for.

use std::path::Path;

use consolette::config::load;
use consolette::config::schema::{Strategy, UpstreamKind};

fn references_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/references"))
}

#[test]
fn references_00_providers_toml_should_parse_cleanly_against_config_schema() {
    let config = load(references_dir())
        .unwrap_or_else(|e| panic!("references/conf.d/00-providers.toml failed to load: {e}"));

    let names: Vec<&str> = config.upstreams.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["anthropic", "bedrock", "openai", "gemini"],
        "expected all four upstream kinds side by side, in file order"
    );

    let gemini = config
        .upstreams
        .iter()
        .find(|u| u.name == "gemini")
        .expect("gemini upstream present");
    match &gemini.kind {
        UpstreamKind::Gemini { project_id } => {
            assert_eq!(project_id, "your-gcp-project-id");
        }
        other => panic!("expected UpstreamKind::Gemini, got {other:?}"),
    }
}

#[test]
fn references_00_providers_toml_gemini_route_should_default_to_fallback_strategy_with_gemini_listed_last(
) {
    let config = load(references_dir())
        .unwrap_or_else(|e| panic!("references/conf.d/00-providers.toml failed to load: {e}"));

    let route = config
        .routes
        .iter()
        .find(|r| r.upstreams.iter().any(|u| u.name == "gemini"))
        .expect("a route referencing the gemini upstream");

    assert_eq!(
        route.strategy,
        Strategy::Fallback,
        "the example must default to fallback, never weighted, to bound blast radius"
    );

    let order: Vec<&str> = route.upstreams.iter().map(|u| u.name.as_str()).collect();
    assert_eq!(
        order,
        vec!["anthropic", "bedrock", "gemini"],
        "gemini must be listed last in the fallback chain"
    );
}
