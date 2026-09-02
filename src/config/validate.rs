//! Post-`extract()` semantic validation (FR-1.5): reference checks figment's
//! own deserialization can't express.

use std::collections::HashSet;

use super::schema::Config;
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
