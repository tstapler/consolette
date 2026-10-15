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
                    kind: upstream.kind.label().to_string(),
                });
            }
        }
    }

    Ok(())
}

/// Validates `[[model_tiers]]`: unique names, non-empty `match` (no blank prefixes), every
/// upstream name declared, and exactly one of `model` / `model_family` per override, with
/// `model_family` only on `kind = "openai"` upstreams (same rules as route selectors).
///
/// # Errors
///
/// Returns [`ConfigError::InvalidModelTier`] naming the offending tier.
pub fn validate_model_tiers(config: &Config) -> Result<(), ConfigError> {
    let invalid = |tier: &str, reason: String| ConfigError::InvalidModelTier {
        tier: tier.to_string(),
        reason,
    };
    let mut seen: HashSet<&str> = HashSet::new();
    for tier in &config.model_tiers {
        if !seen.insert(tier.name.as_str()) {
            return Err(invalid(&tier.name, "duplicate tier name".to_string()));
        }
        if tier.match_prefixes.is_empty() || tier.match_prefixes.iter().any(|m| m.trim().is_empty())
        {
            return Err(invalid(
                &tier.name,
                "`match` must list at least one non-empty prefix".to_string(),
            ));
        }
        for (name, over) in &tier.upstreams {
            let Some(upstream) = config.upstreams.iter().find(|u| &u.name == name) else {
                return Err(invalid(&tier.name, format!("unknown upstream {name:?}")));
            };
            if over.model.is_some() == over.model_family.is_some() {
                return Err(invalid(
                    &tier.name,
                    format!("upstream {name:?} must set exactly one of model or model_family"),
                ));
            }
            if over.model_family.is_some() && !matches!(upstream.kind, UpstreamKind::Openai { .. })
            {
                return Err(invalid(
                    &tier.name,
                    format!(
                        "upstream {name:?} sets model_family but its kind is {:?}, not \"openai\"",
                        upstream.kind.label()
                    ),
                ));
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

    mod tiers {
        use super::super::validate_model_tiers;
        use super::{Config, ConfigError};

        const UPSTREAMS: &str = r#"
[[upstreams]]
name = "gw-openai"
kind = "openai"
base_url = "https://example.invalid"

[[upstreams]]
name = "gw-anthropic"
kind = "anthropic"

[[routes]]
name = "default"
strategy = "fallback"

[[routes.upstreams]]
name = "gw-openai"
model = "gpt-5"
"#;

        fn check(tiers: &str) -> Result<(), ConfigError> {
            let config: Config =
                toml::from_str(&format!("{UPSTREAMS}{tiers}")).expect("fragment should parse");
            validate_model_tiers(&config)
        }

        fn reason(tiers: &str) -> String {
            match check(tiers) {
                Err(ConfigError::InvalidModelTier { reason, .. }) => reason,
                other => panic!("expected InvalidModelTier, got {other:?}"),
            }
        }

        #[test]
        fn validate_model_tiers_should_accept_valid_tier() {
            let result = check(
                r#"
[[model_tiers]]
name = "haiku"
match = ["claude-haiku"]
[model_tiers.upstreams.gw-openai]
model_family = "gpt-6-luna"
[model_tiers.upstreams.gw-anthropic]
model = "claude-haiku-5-5"
"#,
            );

            assert!(result.is_ok(), "{result:?}");
        }

        #[test]
        fn validate_model_tiers_should_reject_unknown_upstream() {
            let r = reason(
                r#"
[[model_tiers]]
name = "haiku"
match = ["claude-haiku"]
[model_tiers.upstreams.nope]
model = "x"
"#,
            );

            assert!(r.contains("unknown upstream"), "{r}");
        }

        #[test]
        fn validate_model_tiers_should_reject_both_or_neither_selector() {
            let both = reason(
                r#"
[[model_tiers]]
name = "t"
match = ["a"]
[model_tiers.upstreams.gw-openai]
model = "x"
model_family = "y"
"#,
            );
            let neither = reason(
                r#"
[[model_tiers]]
name = "t"
match = ["a"]
[model_tiers.upstreams.gw-openai]
"#,
            );

            assert!(both.contains("exactly one"), "{both}");
            assert!(neither.contains("exactly one"), "{neither}");
        }

        #[test]
        fn validate_model_tiers_should_reject_model_family_on_non_openai_upstream() {
            let r = reason(
                r#"
[[model_tiers]]
name = "t"
match = ["a"]
[model_tiers.upstreams.gw-anthropic]
model_family = "y"
"#,
            );

            assert!(r.contains("not \"openai\""), "{r}");
        }

        #[test]
        fn validate_model_tiers_should_reject_empty_match() {
            let empty = reason("[[model_tiers]]\nname = \"t\"\nmatch = []\n");
            let blank = reason("[[model_tiers]]\nname = \"t\"\nmatch = [\" \"]\n");

            assert!(empty.contains("match"), "{empty}");
            assert!(blank.contains("match"), "{blank}");
        }

        #[test]
        fn validate_model_tiers_should_reject_duplicate_tier_names() {
            let r = reason(
                r#"
[[model_tiers]]
name = "t"
match = ["a"]
[[model_tiers]]
name = "t"
match = ["b"]
"#,
            );

            assert!(r.contains("duplicate"), "{r}");
        }
    }
}
