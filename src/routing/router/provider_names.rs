//! Upstream names for `Router::providers`, kept out of `RouterDeps` (built at
//! dozens of sites) and out of `mod.rs` (line-count budget).

use std::sync::Arc;

use super::Router;
use crate::config::schema::Config;
use crate::metrics::ids::UpstreamName;
use crate::providers::Provider;
use crate::routing::model_catalog::{NamedProvider, UpstreamFingerprint};

/// Names of `build_providers`' `(name, provider)` pairs, in order.
pub(super) fn names_of(providers: &[(String, Arc<dyn Provider>)]) -> Vec<UpstreamName> {
    providers
        .iter()
        .map(|(name, _)| UpstreamName::from(name.as_str()))
        .collect()
}

/// Config-order fingerprints of every upstream. Hashes the serialized upstream (kind, base
/// URL, auth source and credential reference) plus the request timeout; only the hash is kept.
pub(super) fn fingerprints_of(config: &Config) -> Vec<UpstreamFingerprint> {
    use std::hash::{Hash, Hasher};
    config
        .upstreams
        .iter()
        .map(|upstream| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            serde_json::to_string(upstream)
                .unwrap_or_default()
                .hash(&mut hasher);
            format!("{:?}", config.request_timeout).hash(&mut hasher);
            UpstreamFingerprint(hasher.finish())
        })
        .collect()
}

impl Router {
    /// Attaches `fingerprints_of` output so the model catalog can tell a changed upstream
    /// from a rebuilt-but-identical one.
    #[must_use]
    pub fn with_fingerprints(mut self, fingerprints: Vec<UpstreamFingerprint>) -> Self {
        self.fingerprints = fingerprints;
        self
    }

    /// Attaches config-order upstream names to `providers` (see `providers_named`).
    #[must_use]
    pub fn with_provider_names(mut self, names: Vec<UpstreamName>) -> Self {
        self.provider_names = names;
        self
    }

    /// `(name, provider)` pairs in config order; empty when names were never
    /// set or their count differs from the providers'.
    #[must_use]
    pub fn providers_named(&self) -> Vec<NamedProvider> {
        if self.provider_names.len() != self.providers.len() {
            tracing::debug!(
                names = self.provider_names.len(),
                providers = self.providers.len(),
                "provider names unset or mismatched; providers_named is empty"
            );
            return Vec::new();
        }
        self.provider_names
            .iter()
            .zip(&self.providers)
            .enumerate()
            .map(|(index, (name, provider))| NamedProvider {
                name: name.clone(),
                provider: Arc::clone(provider),
                // No config fingerprint (hand-built router): instance identity is all we have.
                fingerprint: self.fingerprints.get(index).copied().unwrap_or_else(|| {
                    UpstreamFingerprint(Arc::as_ptr(provider).cast::<()>() as u64)
                }),
            })
            .collect()
    }
}
