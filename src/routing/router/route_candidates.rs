//! Primary-route selection and candidate resolution for `Router::from_config`, kept out of
//! `mod.rs` (line-count budget, see `tests/observability.rs`).

use crate::config::schema::{Config, Route};
use crate::routing::strategy::UpstreamRef;

/// The one `Route` `from_config` builds a `Router` from — always the
/// first configured route; any others are logged and ignored.
///
/// # Errors
///
/// Returns `Err` if `config.routes` is empty.
pub(super) fn select_primary_route(config: &Config) -> anyhow::Result<&Route> {
    let route = config
        .routes
        .first()
        .ok_or_else(|| anyhow::anyhow!("no routes configured"))?;
    if config.routes.len() > 1 {
        tracing::warn!(
            ignored = ?config.routes[1..].iter().map(|r| &r.name).collect::<Vec<_>>(),
            "multiple routes configured; using the first"
        );
    }
    Ok(route)
}

/// Resolves `route`'s upstream-name references into `UpstreamRef`s indexed
/// against `config.upstreams`.
///
/// # Errors
///
/// Returns `Err` naming `route` and the offending name if it references an
/// upstream not present in `config.upstreams`.
pub(super) fn build_route_candidates(
    route: &Route,
    config: &Config,
) -> anyhow::Result<Vec<UpstreamRef>> {
    let mut candidates = Vec::with_capacity(route.upstreams.len());
    for route_upstream in &route.upstreams {
        let index = config
            .upstreams
            .iter()
            .position(|u| u.name == route_upstream.name)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "route \"{}\" references unknown upstream \"{}\"",
                    route.name,
                    route_upstream.name
                )
            })?;
        candidates.push(UpstreamRef {
            index,
            name: route_upstream.name.clone(),
            weight: route_upstream.weight.unwrap_or(1.0),
            model: route_upstream.model.clone(),
            model_family: route_upstream.model_family.clone(),
        });
    }
    Ok(candidates)
}
