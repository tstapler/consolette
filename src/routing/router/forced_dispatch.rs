//! `X-Consolette-Upstream` dispatch: send one request to one named upstream, with no fallback.
//! Kept out of `mod.rs` (line-count budget, see `tests/observability.rs`).

use http::HeaderMap;

use super::Router;
use crate::metrics::trace::TraceScope;
use crate::providers::{ProviderError, ProviderResponse};
use crate::routing::strategy::UpstreamRef;

impl Router {
    /// Names of every configured upstream, in config order. Falls back to the route's own
    /// candidates for hand-built routers that never had names attached.
    #[must_use]
    pub fn configured_upstream_names(&self) -> Vec<String> {
        if self.provider_names.len() == self.providers.len() {
            return self
                .provider_names
                .iter()
                .map(ToString::to_string)
                .collect();
        }
        let mut names: Vec<String> = Vec::new();
        for candidate in &self.candidates {
            if !names.contains(&candidate.name) {
                names.push(candidate.name.clone());
            }
        }
        names
    }

    /// The single candidate a forced request may use: the upstream's first route entry (so a
    /// pinned `model` / `model_family` still applies), or a plain entry when it is not routed.
    fn forced_candidate(&self, upstream: &str) -> Option<UpstreamRef> {
        if let Some(entry) = self.candidates.iter().find(|c| c.name == upstream) {
            // Weight is irrelevant for a one-candidate pool, but 0 would starve the strategy.
            return Some(UpstreamRef {
                weight: 1.0,
                ..entry.clone()
            });
        }
        if self.provider_names.len() != self.providers.len() {
            return None;
        }
        let index = self
            .provider_names
            .iter()
            .position(|n| n.to_string() == upstream)?;
        Some(UpstreamRef {
            index,
            name: upstream.to_string(),
            weight: 1.0,
            model: None,
            model_family: None,
        })
    }

    /// Dispatches to `upstream` only. Rate limits and health still apply; a failure is
    /// returned as-is rather than failing over. Session pins are ignored.
    pub(super) async fn dispatch_forced(
        &self,
        scope: &mut TraceScope,
        upstream: &str,
        body: serde_json::Value,
        headers: HeaderMap,
        stream: bool,
        est_tokens: u32,
    ) -> Result<ProviderResponse, ProviderError> {
        let Some(candidate) = self.forced_candidate(upstream) else {
            return Err(ProviderError::Validation(
                format!(
                    "unknown upstream in X-Consolette-Upstream; configured upstreams: [{}]",
                    self.configured_upstream_names().join(", ")
                ),
                400,
            ));
        };
        scope.record_route(false, self.strategy.name());
        self.dispatch_attempts(scope, &[candidate], body, headers, stream, est_tokens)
            .await
    }
}
