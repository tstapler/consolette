//! Upstream names for `Router::providers`, kept out of `RouterDeps` (built at
//! dozens of sites) and out of `mod.rs` (line-count budget).

use std::sync::Arc;

use super::Router;
use crate::metrics::ids::UpstreamName;
use crate::providers::Provider;

/// Names of `build_providers`' `(name, provider)` pairs, in order.
pub(super) fn names_of(providers: &[(String, Arc<dyn Provider>)]) -> Vec<UpstreamName> {
    providers
        .iter()
        .map(|(name, _)| UpstreamName::from(name.as_str()))
        .collect()
}

impl Router {
    /// Attaches config-order upstream names to `providers` (see `providers_named`).
    #[must_use]
    pub fn with_provider_names(mut self, names: Vec<UpstreamName>) -> Self {
        self.provider_names = names;
        self
    }

    /// `(name, provider)` pairs in config order; empty when names were never
    /// set or their count differs from the providers'.
    #[must_use]
    pub fn providers_named(&self) -> Vec<(UpstreamName, Arc<dyn Provider>)> {
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
            .cloned()
            .zip(self.providers.iter().map(Arc::clone))
            .collect()
    }
}
