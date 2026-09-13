//! Post-`extract()` semantic validation (FR-1.5): reference checks figment's
//! own deserialization can't express.

use std::collections::HashSet;

use super::schema::{Config, Strategy};
use super::ConfigError;

/// Every route's upstream references must resolve to a declared upstream
/// name, every route's `family` alias must name a declared
/// `[[model_families]]` alias, every `ratelimit` key must reference a
/// declared upstream, and every family member's upstream must resolve too
/// (unknown member upstreams fail with the upstream name in the error).
///
/// # Errors
///
/// Returns [`ConfigError::UnknownUpstreamReference`] if a route, a
/// `ratelimit` entry, or a family member names an upstream not present in
/// `config.upstreams`, or [`ConfigError::UnknownFamilyAlias`] if a route's
/// `family` names an alias with no `[[model_families]]` entry.
pub fn validate_references(config: &Config) -> Result<(), ConfigError> {
    let known: HashSet<&str> = config.upstreams.iter().map(|u| u.name.as_str()).collect();
    let aliases: HashSet<&str> = config.families.iter().map(|f| f.alias.as_str()).collect();

    for route in &config.routes {
        for reference in &route.upstreams {
            if !known.contains(reference.name.as_str()) {
                return Err(ConfigError::UnknownUpstreamReference {
                    route: route.name.clone(),
                    upstream: reference.name.clone(),
                });
            }
        }
        if let Some(alias) = route.family.as_deref() {
            if !aliases.contains(alias) {
                return Err(ConfigError::UnknownFamilyAlias {
                    route: route.name.clone(),
                    alias: alias.to_string(),
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

    for family in &config.families {
        for member in &family.members {
            if !known.contains(member.upstream.as_str()) {
                return Err(ConfigError::UnknownUpstreamReference {
                    route: format!("family {:?}", family.alias),
                    upstream: member.upstream.clone(),
                });
            }
        }
    }

    Ok(())
}

/// Family routes must use the `fallback` strategy (Story 3.1 AC4): dispatch
/// iterates the ranked member list directly in order (forced fallback), and
/// ranked order is meaningless under `WeightedStrategy::select` random
/// sampling. A family route on `weighted` fails here naming route + alias —
/// dispatch additionally never consults the strategy for alias requests, so
/// a directly-constructed weighted router still serves ranked order.
///
/// # Errors
///
/// Returns [`ConfigError::WeightedFamilyRoute`] for the first family route
/// on a non-fallback strategy.
pub fn validate_family_strategy(config: &Config) -> Result<(), ConfigError> {
    for route in &config.routes {
        if let Some(alias) = route.family.as_deref() {
            if route.strategy != Strategy::Fallback {
                return Err(ConfigError::WeightedFamilyRoute {
                    route: route.name.clone(),
                    alias: alias.to_string(),
                });
            }
        }
    }
    Ok(())
}

/// `FreeGuard`: a free family (`allow_paid = false`) only admits member IDs
/// that are verifiably free — a `:free` suffix (fail-open so provider-side
/// rotation minting new `:free` IDs keeps loading) or a vendored pricing
/// snapshot entry priced at exactly zero. Anything else fails closed:
/// snapshot-listed paid IDs and pricing-unknown non-`:free` IDs are both
/// rejected. `allow_paid` families accept everything.
///
/// Reads the vendored [`crate::cost_metrics::pricing::vendored_default`]
/// snapshot synchronously; "says free" means the snapshot carries the ID at
/// input + output cost exactly zero (every currently vendored entry is
/// nonzero, so today that arm only fires for explicitly zero-priced rows).
///
/// # Errors
///
/// Returns [`ConfigError::PaidMemberInFreeFamily`] naming the alias + member
/// for the first offending member.
pub fn validate_free_guard(config: &Config) -> Result<(), ConfigError> {
    let pricing = crate::cost_metrics::pricing::vendored_default();
    for family in &config.families {
        if family.allow_paid {
            continue;
        }
        for member in &family.members {
            if !crate::cost_metrics::pricing::is_free_model_id(&member.model, pricing) {
                return Err(ConfigError::PaidMemberInFreeFamily {
                    alias: family.alias.clone(),
                    member: member.model.clone(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::super::schema::{
        Config, FamilyMember, ModelFamily, Route, RouteUpstreamRef, Strategy,
    };
    use super::super::ConfigError;
    use super::{validate_family_strategy, validate_free_guard, validate_references};

    fn free_family_config(alias: &str, allow_paid: bool, models: &[&str]) -> Config {
        Config {
            families: vec![ModelFamily {
                alias: alias.to_string(),
                members: models
                    .iter()
                    .map(|m| FamilyMember {
                        upstream: "anthropic".to_string(),
                        model: (*m).to_string(),
                    })
                    .collect(),
                allow_paid,
            }],
            ..Config::default()
        }
    }

    #[test]
    fn free_guard_should_accept_free_suffixed_id_when_pricing_unknown() {
        // `cohere/new-model:free` is absent from the vendored snapshot
        // (rotation) — the `:free` suffix keeps the free family loading.
        let config = free_family_config("auto-coding", false, &["cohere/new-model:free"]);
        assert!(
            validate_free_guard(&config).is_ok(),
            "unknown :free-suffixed ID must be accepted in a free family"
        );
    }

    #[test]
    fn free_guard_should_reject_paid_member_when_allow_paid_false() {
        // Snapshot-listed paid ID fails in the free family ...
        let config = free_family_config("auto-coding", false, &["gpt-4o"]);
        let err = validate_free_guard(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("auto-coding") && msg.contains("gpt-4o"),
            "error must name alias + member, got: {msg}"
        );

        // ... and so does a pricing-unknown non-`:free` ID (fail closed).
        let config = free_family_config("auto-coding", false, &["anthropic/mystery-model"]);
        let err = validate_free_guard(&config).unwrap_err();
        match err {
            super::super::ConfigError::PaidMemberInFreeFamily { alias, member } => {
                assert_eq!(alias, "auto-coding");
                assert_eq!(member, "anthropic/mystery-model");
            }
            other => panic!("expected PaidMemberInFreeFamily, got {other:?}"),
        }
    }

    #[test]
    fn free_guard_should_accept_paid_member_when_allow_paid_true() {
        // The paid alias admits snapshot-priced and pricing-unknown IDs alike.
        let config = free_family_config(
            "auto-coding-paid",
            true,
            &["gpt-4o", "anthropic/mystery-model"],
        );
        assert!(
            validate_free_guard(&config).is_ok(),
            "allow_paid families must accept any member ID"
        );
    }

    #[test]
    fn validate_references_should_reject_family_member_with_unknown_upstream() {
        let config = Config {
            families: vec![ModelFamily {
                alias: "auto-coding".to_string(),
                members: vec![FamilyMember {
                    upstream: "does-not-exist".to_string(),
                    model: "x-model:free".to_string(),
                }],
                allow_paid: false,
            }],
            ..Config::default()
        };
        let err = validate_references(&config).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("does-not-exist"),
            "error must name the unknown upstream, got: {msg}"
        );
    }

    #[test]
    fn validate_references_should_reject_route_with_unknown_family_alias() {
        // A route naming a nonexistent [[model_families]] alias must fail
        // load (never render as entry_kind "family" with null detail).
        let config = Config {
            routes: vec![Route {
                name: "default".to_string(),
                strategy: Strategy::Fallback,
                upstreams: vec![RouteUpstreamRef {
                    name: "anthropic".to_string(),
                    weight: None,
                    model: None,
                }],
                family: Some("no-such-alias".to_string()),
            }],
            ..Config::default()
        };
        match validate_references(&config) {
            Err(ConfigError::UnknownFamilyAlias { route, alias }) => {
                assert_eq!(route, "default");
                assert_eq!(alias, "no-such-alias");
                let msg = ConfigError::UnknownFamilyAlias { route, alias }.to_string();
                assert!(
                    msg.contains("default") && msg.contains("no-such-alias"),
                    "error must name route + alias, got: {msg}"
                );
            }
            Err(other) => panic!("expected UnknownFamilyAlias, got {other:?}"),
            Ok(()) => panic!("unknown family alias must be rejected"),
        }

        // A route naming a declared alias passes.
        let config = Config {
            families: vec![ModelFamily {
                alias: "auto-coding".to_string(),
                members: vec![FamilyMember {
                    upstream: "anthropic".to_string(),
                    model: "x-model:free".to_string(),
                }],
                allow_paid: false,
            }],
            routes: vec![Route {
                name: "default".to_string(),
                strategy: Strategy::Fallback,
                upstreams: vec![RouteUpstreamRef {
                    name: "anthropic".to_string(),
                    weight: None,
                    model: None,
                }],
                family: Some("auto-coding".to_string()),
            }],
            ..Config::default()
        };
        assert!(validate_references(&config).is_ok());
    }

    fn family_route_with(strategy: Strategy) -> Config {
        Config {
            routes: vec![Route {
                name: "family-route".to_string(),
                strategy,
                upstreams: vec![RouteUpstreamRef {
                    name: "anthropic".to_string(),
                    weight: None,
                    model: None,
                }],
                family: Some("auto-coding".to_string()),
            }],
            ..Config::default()
        }
    }

    #[test]
    fn weighted_family_route_should_be_rejected_when_strategy_is_weighted() {
        // Story 3.1 AC4: ranked order is meaningless under random sampling,
        // so a family route on `weighted` fails naming route + alias.
        let config = family_route_with(Strategy::Weighted);
        match validate_family_strategy(&config) {
            Err(ConfigError::WeightedFamilyRoute { route, alias }) => {
                assert_eq!(route, "family-route");
                assert_eq!(alias, "auto-coding");
                let msg = ConfigError::WeightedFamilyRoute { route, alias }.to_string();
                assert!(
                    msg.contains("family-route") && msg.contains("auto-coding"),
                    "error must name route + alias, got: {msg}"
                );
            }
            Err(other) => panic!("expected WeightedFamilyRoute, got {other:?}"),
            Ok(()) => panic!("weighted family route must be rejected"),
        }

        // Fallback family routes pass, and so do weighted routes with no
        // family field (existing weighted traffic is untouched).
        assert!(validate_family_strategy(&family_route_with(Strategy::Fallback)).is_ok());
        let mut plain_weighted = Config::default();
        plain_weighted.routes[0].strategy = Strategy::Weighted;
        assert!(validate_family_strategy(&plain_weighted).is_ok());
    }
}
