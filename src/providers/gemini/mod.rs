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

use crate::config::schema::Upstream;

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
