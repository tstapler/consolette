//! Post-`extract()` semantic validation (FR-1.5): reference checks figment's
//! own deserialization can't express.

use std::collections::HashSet;

use super::schema::{Config, UpstreamKind};
use super::ConfigError;

/// Every route's upstream references must resolve to a declared upstream
/// name, and every `ratelimit` key must reference a declared upstream.
///
/// # Errors
///
/// Returns [`ConfigError::UnknownUpstreamReference`] if a route or
/// `ratelimit` entry names an upstream not present in `config.upstreams`.
pub fn validate_references(config: &Config) -> Result<(), ConfigError> {
    let known: HashSet<&str> = config.upstreams.iter().map(|u| u.name.as_str()).collect();

    for route in &config.routes {
        for reference in &route.upstreams {
            if !known.contains(reference.name.as_str()) {
                return Err(ConfigError::UnknownUpstreamReference {
                    route: route.name.clone(),
                    upstream: reference.name.clone(),
                });
            }
        }
    }

    for upstream_name in config.ratelimit.upstreams.keys() {
        if !known.contains(upstream_name.as_str()) {
            return Err(ConfigError::UnknownUpstreamReference {
                route: "ratelimit".to_string(),
                upstream: upstream_name.clone(),
            });
        }
    }

    Ok(())
}

fn upstream_kind_label(kind: &UpstreamKind) -> &'static str {
    match kind {
        UpstreamKind::Anthropic => "anthropic",
        UpstreamKind::Bedrock { .. } => "bedrock",
        UpstreamKind::Openai { .. } => "openai",
        UpstreamKind::Gemini { .. } => "gemini",
        UpstreamKind::Openrouter {} => "openrouter",
    }
}

/// Enforces `RouteUpstreamRef.model`/`model_family` as mutually exclusive,
/// required-on-`kind = "openai"` selectors (Story 1.2.2), and that
/// `model_family` — an internal dispatch key only `OpenaiProvider::send`
/// knows to strip (Story 1.3.3) — never reaches a non-OpenAI upstream, where
/// it would otherwise leak straight into that upstream's real request body.
///
/// Unresolved upstream references are skipped here; [`validate_references`]
/// is responsible for reporting those.
///
/// # Errors
///
/// Returns [`ConfigError::ConflictingModelSelector`] if a `kind = "openai"`
/// route upstream sets both `model` and `model_family`, or neither.
/// Returns [`ConfigError::ModelFamilyOnNonOpenaiUpstream`] if any route
/// upstream sets `model_family` on an upstream whose kind isn't `openai`.
pub fn validate_model_selectors(config: &Config) -> Result<(), ConfigError> {
    for route in &config.routes {
        for reference in &route.upstreams {
            let Some(upstream) = config.upstreams.iter().find(|u| u.name == reference.name) else {
                continue;
            };
            let is_openai = matches!(upstream.kind, UpstreamKind::Openai { .. });

            if is_openai && reference.model.is_some() == reference.model_family.is_some() {
                // Both `Some` (conflict) or both `None` (nothing selected) —
                // exactly one of the two is required on an openai upstream.
                return Err(ConfigError::ConflictingModelSelector {
                    route: route.name.clone(),
                    upstream: reference.name.clone(),
                });
            }

            if reference.model_family.is_some() && !is_openai {
                return Err(ConfigError::ModelFamilyOnNonOpenaiUpstream {
                    route: route.name.clone(),
                    upstream: reference.name.clone(),
                    kind: upstream_kind_label(&upstream.kind).to_string(),
                });
            }
        }
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::validate_model_selectors;
    use super::ConfigError;
    use crate::config::schema::Config;

    #[test]
    fn validate_model_selectors_should_reject_config_error_when_both_model_and_model_family_set() {
        let toml = r#"
[[upstreams]]
name = "model-gateway-openai"
kind = "openai"
base_url = "https://example.invalid"

[[routes]]
name = "coding"
strategy = "fallback"

[[routes.upstreams]]
name = "model-gateway-openai"
model = "gpt-5.1"
model_family = "gpt-5"
"#;
        let config: Config = toml::from_str(toml).expect("fragment should parse");

        let result = validate_model_selectors(&config);

        match result {
            Err(ConfigError::ConflictingModelSelector { route, upstream }) => {
                assert_eq!(route, "coding");
                assert_eq!(upstream, "model-gateway-openai");
            }
            other => panic!("expected ConflictingModelSelector, got {other:?}"),
        }
    }

    #[test]
    fn validate_model_selectors_should_reject_when_openai_upstream_sets_neither_model_nor_model_family(
    ) {
        let toml = r#"
[[upstreams]]
name = "model-gateway-openai"
kind = "openai"
base_url = "https://example.invalid"

[[routes]]
name = "coding"
strategy = "fallback"

[[routes.upstreams]]
name = "model-gateway-openai"
"#;
        let config: Config = toml::from_str(toml).expect("fragment should parse");

        let result = validate_model_selectors(&config);

        assert!(
            matches!(result, Err(ConfigError::ConflictingModelSelector { .. })),
            "expected ConflictingModelSelector, got {result:?}"
        );
    }

    #[test]
    fn validate_model_selectors_should_reject_model_family_on_non_openai_upstream_kind() {
        let toml = r#"
[[upstreams]]
name = "anthropic"
kind = "anthropic"

[[routes]]
name = "default"
strategy = "fallback"

[[routes.upstreams]]
name = "anthropic"
model_family = "gpt-5"
"#;
        let config: Config = toml::from_str(toml).expect("fragment should parse");

        let result = validate_model_selectors(&config);

        match result {
            Err(ConfigError::ModelFamilyOnNonOpenaiUpstream {
                route,
                upstream,
                kind,
            }) => {
                assert_eq!(route, "default");
                assert_eq!(upstream, "anthropic");
                assert_eq!(kind, "anthropic");
            }
            other => panic!("expected ModelFamilyOnNonOpenaiUpstream, got {other:?}"),
        }
    }

    #[test]
    fn validate_model_selectors_should_accept_when_exactly_one_of_model_or_model_family_is_set() {
        let toml = r#"
[[upstreams]]
name = "model-gateway-openai"
kind = "openai"
base_url = "https://example.invalid"

[[upstreams]]
name = "model-gateway-openai-pinned"
kind = "openai"
base_url = "https://example.invalid"

[[routes]]
name = "coding"
strategy = "fallback"

[[routes.upstreams]]
name = "model-gateway-openai"
model_family = "gpt-5"

[[routes.upstreams]]
name = "model-gateway-openai-pinned"
model = "gpt-5.1"
"#;
        let config: Config = toml::from_str(toml).expect("fragment should parse");

        let result = validate_model_selectors(&config);

        assert!(result.is_ok(), "expected Ok, got {result:?}");
    }
}
