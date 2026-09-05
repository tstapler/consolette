//! `GeminiProvider`: Cloud Code Assist upstream (ADR-001/ADR-003).
//!
//! Wired-but-inert skeleton (Epic 1.1): `GeminiProvider` compiles and
//! satisfies the `Provider` trait, but `send()` is a stub — real request/
//! response translation lands starting Epic 1.3.

mod error;
mod tools;
mod translate;

use std::sync::Arc;

use async_trait::async_trait;
use http::HeaderMap;

use crate::config::schema::{Upstream, UpstreamKind};

use super::{ModelInfo, Provider, ProviderError, ProviderResponse};

/// Provider for Google's Cloud Code Assist endpoint, structurally mirroring
/// `OpenaiProvider` more than `AnthropicProvider` (see Domain Glossary).
pub struct GeminiProvider {
    /// The upstream this provider was constructed for — supplies `name`,
    /// `auth`, and (via `UpstreamKind::Gemini::project_id`) the Cloud Code
    /// Assist envelope's `project` field.
    upstream: Arc<Upstream>,
}

impl GeminiProvider {
    /// Wired-but-inert stub constructor (Task 1.1.2a/b). The real fallible
    /// constructor (`GeminiProvider::new`, building the ADR-004 client pair
    /// and validating auth) lands in Story 1.3.4.
    #[must_use]
    pub fn stub(upstream: Arc<Upstream>) -> Self {
        Self { upstream }
    }

    /// The configured Cloud Code Assist project id (ADR-003), used verbatim
    /// as `CloudCodeEnvelope.project` on every outgoing request.
    //
    // TODO(Epic 1.3, Story 1.3.1/1.3.4): send() must call self.project_id()
    // when building the outgoing envelope — see plan.md Story 1.7.1.
    #[must_use]
    pub fn project_id(&self) -> &str {
        match &self.upstream.kind {
            UpstreamKind::Gemini { project_id } => project_id,
            // Can't happen — GeminiProvider is only ever constructed for a
            // Gemini-kind upstream, per build_providers's match arm.
            other => unreachable!("GeminiProvider constructed for non-Gemini upstream: {other:?}"),
        }
    }
}

#[async_trait]
impl Provider for GeminiProvider {
    fn name(&self) -> &'static str {
        "gemini"
    }

    async fn send(
        &self,
        _body: serde_json::Value,
        _headers: HeaderMap,
        _stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        Err(ProviderError::ModelUnsupported(
            "gemini stub not yet implemented".to_string(),
        ))
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError> {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // REQ-14 (Story 1.7.1): `project_id()` returns the exact configured
    // string from `UpstreamKind::Gemini`, never a default/guess.

    #[test]
    fn project_id_accessor_should_return_configured_project_id_from_upstream_kind_gemini() {
        let upstream = Arc::new(Upstream {
            name: "gemini".to_string(),
            kind: UpstreamKind::Gemini {
                project_id: "my-gcp-project".to_string(),
            },
            auth: None,
        });
        let provider = GeminiProvider::stub(upstream);

        assert_eq!(provider.project_id(), "my-gcp-project");
    }
}
