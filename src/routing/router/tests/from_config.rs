//! `Router::from_config`/`build_providers` construction and validation.

use super::*;

/// Builds a config-schema `Upstream` with an inline bearer-token secret
/// (Story 4.3.1's tests below): most of Epic 4.3's `from_config` unit
/// tests need real construction-time auth (`OpenrouterProvider::new`
/// resolves headers eagerly), not `auth: None`.
fn bearer_upstream(
    name: &str,
    kind: crate::config::schema::UpstreamKind,
    token: &str,
) -> crate::config::schema::Upstream {
    crate::config::schema::Upstream {
        name: name.to_string(),
        kind,
        auth: Some(crate::config::schema::AuthMethod::Bearer {
            token: crate::config::schema::SecretRef::Inline {
                value: token.to_string(),
            },
        }),
    }
}

/// Asserts a `from_config` validation error's message names both the
/// offending route and upstream — the same two-part assertion repeated
/// across the openrouter-strategy-pairing rejection tests below
/// (kibitzer duplicate-code).
fn assert_error_names_route_and_upstream(err: &anyhow::Error, route: &str, upstream: &str) {
    let msg = err.to_string();
    assert!(msg.contains(route), "error must name the route, got: {msg}");
    assert!(
        msg.contains(upstream),
        "error must name the upstream, got: {msg}"
    );
}

#[tokio::test]
async fn from_config_default_config_produces_two_candidates_in_order() {
    // `AnthropicProvider::new` doesn't resolve the bearer-token secret
    // at construction time (only at send-time), so no env var needs to
    // be set for this to succeed.
    let config = Config::default();
    #[allow(clippy::expect_used)]
    let router = Router::from_config(&config, MetricsCollector::new())
        .await
        .expect("Config::default() must build a Router");
    assert_eq!(router.candidates.len(), 2);
    assert_eq!(router.candidates[0].index, 0);
    assert_eq!(router.candidates[0].name, "anthropic");
    assert_eq!(router.candidates[1].index, 1);
    assert_eq!(router.candidates[1].name, "bedrock");
    assert_eq!(router.providers[0].name(), "anthropic");
    assert_eq!(router.providers[1].name(), "bedrock");
}

#[tokio::test]
#[allow(clippy::expect_used)]
async fn from_config_openai_kind_builds_successfully() {
    use crate::config::schema::{Route, RouteUpstreamRef, Upstream, UpstreamKind};

    let config = Config {
        upstreams: vec![Upstream {
            name: "my-openai-upstream".to_string(),
            kind: UpstreamKind::Openai {
                base_url: "https://example.invalid".to_string(),
            },
            auth: None,
        }],
        routes: vec![Route {
            name: "default".to_string(),
            strategy: Strategy::Fallback,
            upstreams: vec![RouteUpstreamRef {
                name: "my-openai-upstream".to_string(),
                weight: None,
                model: None,
            }],
        }],
        ..Config::default()
    };

    #[allow(clippy::expect_used)]
    let router = Router::from_config(&config, MetricsCollector::new())
        .await
        .expect("Openai-kind upstream must build a Provider");
    assert_eq!(router.candidates[0].name, "my-openai-upstream");
    assert_eq!(router.providers[0].name(), "openai");
}

#[tokio::test]
async fn from_config_empty_routes_bails() {
    let config = Config {
        routes: vec![],
        ..Config::default()
    };
    let Err(err) = Router::from_config(&config, MetricsCollector::new()).await else {
        panic!("empty routes must fail")
    };
    assert!(err.to_string().contains("no routes configured"));
}

#[tokio::test]
async fn from_config_multi_route_uses_first() {
    use crate::config::schema::{Route, RouteUpstreamRef};

    let mut config = Config::default();
    let route_a = config.routes[0].clone();
    let route_b = Route {
        name: "secondary".to_string(),
        strategy: Strategy::Fallback,
        upstreams: vec![RouteUpstreamRef {
            name: "bedrock".to_string(),
            weight: None,
            model: None,
        }],
    };
    config.routes = vec![route_a, route_b];

    #[allow(clippy::expect_used)]
    let router = Router::from_config(&config, MetricsCollector::new())
        .await
        .expect("multi-route config must still build");
    assert_eq!(router.candidates.len(), 2);
    assert_eq!(router.candidates[0].name, "anthropic");
    assert_eq!(router.candidates[1].name, "bedrock");
}

// REQ-2 (Story 1.1.2): the exhaustive `UpstreamKind` match in
// `build_providers` accepts `Gemini` and constructs a stub provider.

#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn build_providers_should_construct_provider_for_upstream_kind_gemini() {
    let config = Config {
        upstreams: vec![crate::config::schema::Upstream {
            name: "gemini".to_string(),
            kind: UpstreamKind::Gemini {
                project_id: "p1".to_string(),
            },
            auth: None,
        }],
        ..Config::default()
    };

    let (providers, openrouter_providers) = build_providers(&config).await.unwrap();

    assert_eq!(providers.len(), 1);
    assert_eq!(providers[0].0, "gemini");
    assert_eq!(providers[0].1.name(), "gemini");
    assert!(
        openrouter_providers.is_empty(),
        "a non-openrouter upstream must not appear in the openrouter-index map"
    );
}

// REQ-1 (Story 1.2.3, Task 1.2.3d): the exhaustive `UpstreamKind` match
// in `build_providers` accepts `Openrouter` and additionally returns the
// index -> `Arc<OpenrouterProvider>` map alongside the existing
// providers vec.
//
// `OpenrouterProvider::new` performs a live eager model-cache refresh as
// an invariant of construction (Story 2.1.2) — a failure there is
// logged, not propagated (see `OpenrouterProvider::new`'s doc comment),
// so this test doesn't depend on live network access to pass; it only
// asserts the construction/wiring contract this story owns.
#[tokio::test]
#[allow(clippy::unwrap_used)]
async fn build_providers_should_construct_provider_for_upstream_kind_openrouter() {
    let config = Config {
        upstreams: vec![
            crate::config::schema::Upstream {
                name: "anthropic".to_string(),
                kind: UpstreamKind::Anthropic,
                auth: Some(crate::config::schema::AuthMethod::Bearer {
                    token: crate::config::schema::SecretRef::Inline {
                        value: "sk-ant-test".to_string(),
                    },
                }),
            },
            crate::config::schema::Upstream {
                name: "openrouter".to_string(),
                kind: UpstreamKind::Openrouter {},
                auth: Some(crate::config::schema::AuthMethod::Bearer {
                    token: crate::config::schema::SecretRef::Inline {
                        value: "sk-or-v1-test".to_string(),
                    },
                }),
            },
        ],
        ..Config::default()
    };

    let (providers, openrouter_providers) = build_providers(&config).await.unwrap();

    assert_eq!(providers.len(), 2);
    assert_eq!(providers[1].0, "openrouter");
    assert_eq!(providers[1].1.name(), "openrouter");
    assert_eq!(openrouter_providers.len(), 1);
    assert!(openrouter_providers.contains_key(&1));
}

// REQ-1/Blocker 5 (Story 4.3.1, Task 4.3.1b): a route using
// `strategy = "openrouter_scored"` whose upstreams resolve to no
// `openrouter`-kind upstream at all fails `from_config`, naming the
// route.
#[tokio::test]
async fn from_config_should_reject_openrouter_scored_route_without_openrouter_upstream() {
    use crate::config::schema::{Route, RouteUpstreamRef};

    let config = Config {
        upstreams: vec![bearer_upstream(
            "anthropic",
            UpstreamKind::Anthropic,
            "sk-ant-test",
        )],
        routes: vec![Route {
            name: "or-route".to_string(),
            strategy: Strategy::OpenrouterScored,
            upstreams: vec![RouteUpstreamRef {
                name: "anthropic".to_string(),
                weight: None,
                model: None,
            }],
        }],
        ..Config::default()
    };

    let Err(err) = Router::from_config(&config, MetricsCollector::new()).await else {
        panic!("openrouter_scored route without an openrouter-kind upstream must fail")
    };
    assert!(
        err.to_string().contains("or-route"),
        "error must name the route, got: {err}"
    );
}

// REQ-1 (Story 4.3.1, Task 4.3.1a): a route using
// `strategy = "openrouter_scored"` with a matching `openrouter`-kind
// upstream builds a `Router` whose strategy is an
// `OpenrouterScoringStrategy` wired to that upstream's index and cache.
/// A single `openrouter`-kind upstream, on an `openrouter_scored`
/// route, whose `AuthMethod::Exec` points at a nonexistent binary — the
/// same hermetic-failure technique `cache.rs`'s `broken_auth_provider`
/// test helper uses (no wiremock/mockito in this repo), so
/// `OpenrouterProvider::new`'s eager cache refresh fails deterministically
/// in `build_headers`, before any network I/O is attempted.
fn config_with_broken_auth_openrouter_route() -> Config {
    use crate::config::schema::{AuthMethod, Route, RouteUpstreamRef, Upstream};

    Config {
        upstreams: vec![Upstream {
            name: "openrouter".to_string(),
            kind: UpstreamKind::Openrouter {},
            auth: Some(AuthMethod::Exec {
                command: "/nonexistent-binary-xyz-consolette-test".to_string(),
                args: vec![],
                cache_ttl_secs: 0,
                timeout_secs: 1,
            }),
        }],
        routes: vec![Route {
            name: "or-route".to_string(),
            strategy: Strategy::OpenrouterScored,
            upstreams: vec![RouteUpstreamRef {
                name: "openrouter".to_string(),
                weight: None,
                model: None,
            }],
        }],
        ..Config::default()
    }
}

#[tokio::test]
#[allow(clippy::expect_used, clippy::unwrap_used)]
async fn from_config_should_wire_openrouter_scoring_strategy_to_matching_upstream() {
    let config = config_with_broken_auth_openrouter_route();

    let router = Router::from_config(&config, MetricsCollector::new())
        .await
        .expect("openrouter_scored route with a matching openrouter-kind upstream must build");
    assert_eq!(router.candidates.len(), 1);
    assert_eq!(router.candidates[0].index, 0);
    assert_eq!(router.candidates[0].name, "openrouter");
    assert_eq!(router.providers[0].name(), "openrouter");

    // Proves the wired strategy is really `OpenrouterScoringStrategy`,
    // not `FallbackStrategy`/`WeightedStrategy`: with the eager cache
    // refresh having failed above, `model_cache.snapshot()` is `None`.
    // `OpenrouterScoringStrategy::expand_candidates` fans a
    // `None`-snapshot candidate out to zero entries, so `dispatch`
    // returns `Exhausted` without ever attempting a provider call —
    // `Fallback`/`Weighted` would instead pass the lone candidate
    // through unchanged and actually attempt one (which would fail
    // differently, via the same broken exec auth, not `Exhausted`).
    let res = router
        .dispatch(serde_json::json!({}), HeaderMap::new(), false, 0)
        .await;
    match res {
        Err(ProviderError::Exhausted) => {}
        Err(other) => panic!(
            "expected Err(Exhausted) proving expand_candidates fanned the empty cache to \
             zero candidates, got Err({other:?})"
        ),
        Ok(_) => panic!(
            "expected Err(Exhausted) proving expand_candidates fanned the empty cache to \
             zero candidates, got Ok(_)"
        ),
    }
}

// Blocker 1 (architecture-review), Story 4.3.1d/e: the *other*
// direction of the symmetric validation — an `openrouter`-kind upstream
// referenced by a `Fallback` route fails `from_config`, naming both the
// route and the upstream.
#[tokio::test]
async fn from_config_should_reject_fallback_route_referencing_openrouter_upstream() {
    use crate::config::schema::{Route, RouteUpstreamRef};

    let config = Config {
        upstreams: vec![bearer_upstream(
            "or",
            UpstreamKind::Openrouter {},
            "sk-or-v1-test",
        )],
        routes: vec![Route {
            name: "r1".to_string(),
            strategy: Strategy::Fallback,
            upstreams: vec![RouteUpstreamRef {
                name: "or".to_string(),
                weight: None,
                model: None,
            }],
        }],
        ..Config::default()
    };

    let Err(err) = Router::from_config(&config, MetricsCollector::new()).await else {
        panic!("a Fallback route referencing an openrouter-kind upstream must fail")
    };
    assert_error_names_route_and_upstream(&err, "r1", "or");
}

// Blocker 1 (architecture-review), `Weighted` direction — same as above.
#[tokio::test]
async fn from_config_should_reject_weighted_route_referencing_openrouter_upstream() {
    use crate::config::schema::{Route, RouteUpstreamRef};

    let config = Config {
        upstreams: vec![bearer_upstream(
            "or",
            UpstreamKind::Openrouter {},
            "sk-or-v1-test",
        )],
        routes: vec![Route {
            name: "r1".to_string(),
            strategy: Strategy::Weighted,
            upstreams: vec![RouteUpstreamRef {
                name: "or".to_string(),
                weight: None,
                model: None,
            }],
        }],
        ..Config::default()
    };

    let Err(err) = Router::from_config(&config, MetricsCollector::new()).await else {
        panic!("a Weighted route referencing an openrouter-kind upstream must fail")
    };
    assert_error_names_route_and_upstream(&err, "r1", "or");
}

// Blocker 1 (architecture-review), mixed-upstream direction: a
// `Fallback` route mixing an `openrouter`-kind upstream with a
// non-openrouter upstream is still rejected — the presence of *any*
// openrouter-kind upstream in a non-`OpenrouterScored` route's
// `upstreams` list is sufficient to reject it, regardless of what else
// is in that list.
#[tokio::test]
async fn from_config_should_reject_mixed_upstream_fallback_route_containing_openrouter_upstream() {
    use crate::config::schema::{Route, RouteUpstreamRef};

    let config = Config {
        upstreams: vec![
            bearer_upstream("anthropic", UpstreamKind::Anthropic, "sk-ant-test"),
            bearer_upstream("or", UpstreamKind::Openrouter {}, "sk-or-v1-test"),
        ],
        routes: vec![Route {
            name: "r1".to_string(),
            strategy: Strategy::Fallback,
            upstreams: vec![
                RouteUpstreamRef {
                    name: "anthropic".to_string(),
                    weight: None,
                    model: None,
                },
                RouteUpstreamRef {
                    name: "or".to_string(),
                    weight: None,
                    model: None,
                },
            ],
        }],
        ..Config::default()
    };

    let Err(err) = Router::from_config(&config, MetricsCollector::new()).await else {
        panic!(
            "a Fallback route mixing an openrouter-kind upstream with a non-openrouter \
             upstream must still fail"
        )
    };
    assert_error_names_route_and_upstream(&err, "r1", "or");
}

// Task 4.3.1g (adversarial-review Concern), reverse mixed-upstream
// direction: an `openrouter_scored` route must not itself list a
// non-openrouter (e.g. paid) upstream — without this guard, a
// misconfigured route mixing a free `openrouter`-kind pool with a paid
// upstream could silently fall through to the paid upstream once the
// free pool is exhausted, spending real money on a route believed to be
// free-only.
#[tokio::test]
async fn from_config_should_reject_openrouter_scored_route_mixing_paid_upstream() {
    use crate::config::schema::{Route, RouteUpstreamRef};

    let config = Config {
        upstreams: vec![
            bearer_upstream("or", UpstreamKind::Openrouter {}, "sk-or-v1-test"),
            bearer_upstream("paid-anthropic", UpstreamKind::Anthropic, "sk-ant-test"),
        ],
        routes: vec![Route {
            name: "r2".to_string(),
            strategy: Strategy::OpenrouterScored,
            upstreams: vec![
                RouteUpstreamRef {
                    name: "or".to_string(),
                    weight: None,
                    model: None,
                },
                RouteUpstreamRef {
                    name: "paid-anthropic".to_string(),
                    weight: None,
                    model: None,
                },
            ],
        }],
        ..Config::default()
    };

    let Err(err) = Router::from_config(&config, MetricsCollector::new()).await else {
        panic!("an openrouter_scored route mixing in a non-openrouter (paid) upstream must fail")
    };
    assert_error_names_route_and_upstream(&err, "r2", "paid-anthropic");
}
