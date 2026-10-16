//! `[[model_tiers]]` application: per-client-model overrides of a route candidate's pinned
//! model. Kept out of `mod.rs` (line-count budget, see `tests/observability.rs`).

use super::Router;
use crate::config::schema::ModelTier;
use crate::routing::strategy::UpstreamRef;

/// First tier whose `match` has a prefix of `requested`; prefixes are pre-normalized
/// (trimmed, lowercased) by [`Router::with_model_tiers`].
pub(super) fn matching_tier<'a>(tiers: &'a [ModelTier], requested: &str) -> Option<&'a ModelTier> {
    let requested = requested.to_lowercase();
    tiers.iter().find(|tier| {
        tier.match_prefixes
            .iter()
            .any(|prefix| requested.starts_with(prefix.as_str()))
    })
}

impl Router {
    /// Carries `[[model_tiers]]` onto a router (hot-swaps rebuild via `from_config`, which
    /// calls this with the new config's tiers).
    #[must_use]
    pub fn with_model_tiers(mut self, mut model_tiers: Vec<ModelTier>) -> Self {
        for tier in &mut model_tiers {
            for prefix in &mut tier.match_prefixes {
                *prefix = prefix.trim().to_lowercase();
            }
        }
        self.model_tiers = model_tiers;
        self
    }

    /// Replaces each candidate's model / model family with the matching tier's override for
    /// that upstream. No match, or no override for an upstream, leaves the candidate as the
    /// route defined it; route membership and order never change.
    pub(super) fn apply_model_tiers(
        &self,
        mut candidates: Vec<UpstreamRef>,
        body: &serde_json::Value,
    ) -> Vec<UpstreamRef> {
        let Some(requested) = body.get("model").and_then(serde_json::Value::as_str) else {
            return candidates;
        };
        let Some(tier) = matching_tier(&self.model_tiers, requested) else {
            return candidates;
        };
        for candidate in &mut candidates {
            if let Some(over) = tier.upstreams.get(&candidate.name) {
                // An effort-only override must keep the route's model.
                if over.model.is_some() || over.model_family.is_some() {
                    candidate.model.clone_from(&over.model);
                    candidate.model_family.clone_from(&over.model_family);
                }
            }
        }
        candidates
    }
}
