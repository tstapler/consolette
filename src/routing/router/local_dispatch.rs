//! Local-model dispatch (`local/<backend>/<model>`), kept out of `mod.rs`
//! (line-count budget, see `tests/observability.rs`).

use std::sync::Arc;

use http::HeaderMap;

use super::{Router, LOCAL_REQUEST_TIMEOUT_SECS};
use crate::auth::exec::ExecCredentialCache;
use crate::auth::SystemSecretResolver;
use crate::config::schema::UpstreamKind;
use crate::metrics::trace::TraceScope;
use crate::providers::local_discovery::LocalTarget;
use crate::providers::openai::OpenaiProvider;
use crate::providers::{Provider, ProviderError, ProviderResponse};

impl Router {
    pub(super) fn local_target(&self, body: &serde_json::Value) -> Option<LocalTarget> {
        let model = body.get("model").and_then(serde_json::Value::as_str)?;
        self.local_catalog.resolve(model)
    }

    /// Sends a request for a discovered `local/<backend>/<model>` straight to
    /// that backend's OpenAI-compatible endpoint, bypassing route candidates:
    /// the client named the model explicitly, so there is nothing to select.
    pub(super) async fn dispatch_local(
        &self,
        scope: &mut TraceScope,
        target: LocalTarget,
        mut body: serde_json::Value,
        headers: HeaderMap,
        stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        let provider = match self.local_providers.get(&target.base_url) {
            Some(p) => Arc::clone(&p),
            None => {
                let upstream = Arc::new(crate::config::schema::Upstream {
                    name: format!("local/{}", target.backend.slug()),
                    kind: UpstreamKind::Openai {
                        base_url: target.base_url.clone(),
                    },
                    // OpenaiProvider requires an auth method; local servers
                    // ignore the header, so a placeholder bearer suffices.
                    auth: Some(crate::config::schema::AuthMethod::Bearer {
                        token: crate::config::schema::SecretRef::Inline {
                            value: "local".to_string(),
                        },
                    }),
                });
                let p: Arc<dyn Provider> = Arc::new(OpenaiProvider::new(
                    upstream,
                    target.base_url.clone(),
                    Arc::new(SystemSecretResolver),
                    Arc::new(ExecCredentialCache::new()),
                    LOCAL_REQUEST_TIMEOUT_SECS,
                    Arc::clone(&self.metrics.counters),
                )?);
                self.local_providers
                    .insert(target.base_url.clone(), Arc::clone(&p));
                p
            }
        };
        scope.begin_local_attempt(&target.model);
        body["model"] = serde_json::Value::String(target.model);
        let result = provider.send(body, headers, stream).await;
        scope.complete_local_attempt(result.as_ref().err());
        result
    }
}
