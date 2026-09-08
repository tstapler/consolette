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
#[allow(clippy::expect_used)] // assertion-adjacent lookup on a fixture whose shape the test itself asserts
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
#[allow(clippy::expect_used)] // assertion-adjacent lookup on a fixture whose shape the test itself asserts
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

/// UX Acceptance Tests, "Surface 1, bullet 2": Gemini's fields must be flat
/// directly under `[[upstreams]]`, never a nested `[upstreams.gemini]`
/// sub-table — validation.md names this exact grep-based substitute for the
/// manual "read the file" check.
#[test]
#[allow(clippy::expect_used)] // reading a fixture whose existence the parse test above already asserts
fn config_surface_gemini_fields_should_be_flat_not_nested() {
    let raw = std::fs::read_to_string(references_dir().join("conf.d/00-providers.toml"))
        .expect("references/conf.d/00-providers.toml is readable");

    // Check actual TOML table headers, not prose — the file's own comments
    // mention "[upstreams.gemini]" by name to document that it's absent.
    let has_nested_table = raw
        .lines()
        .map(str::trim)
        .any(|line| !line.starts_with('#') && line.starts_with("[upstreams.gemini]"));
    assert!(
        !has_nested_table,
        "gemini's project_id must sit flat under [[upstreams]], not in a nested \
         [upstreams.gemini] table"
    );
}
