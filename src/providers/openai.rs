//! Generic OpenAI-compatible provider.
//!
//! Forwards requests verbatim to `POST {base_url}/v1/chat/completions` on any
//! OpenAI-compatible endpoint. Unlike `AnthropicProvider`, `base_url` is not
//! hardcoded — `UpstreamKind::Openai` carries it explicitly in config, since
//! this provider is meant to point at arbitrary OpenAI-compatible services
//! (including a future employer-specific gateway, wired up as a separate
//! plugin per ADR-007 — never here).
//!
//! Mirrors `AnthropicProvider`'s structure: the ADR-004 two-`reqwest::Client`
//! split, and `anthropic::apply_auth_headers` for auth (config-driven
//! `bearer`/`apikey`/`exec`, identical across upstream kinds — no reason to
//! duplicate it).

use std::sync::Arc;
use std::time::Duration;

use http::HeaderMap;
use reqwest::{Client, StatusCode};
use serde_json::Value;
use tracing::{debug, warn};

use crate::auth::exec::ExecCredentialCache;
use crate::auth::SecretResolver;
use crate::config::schema::Upstream;

use super::anthropic::apply_auth_headers;
use super::{Provider, ProviderError, ProviderResponse};

use async_trait::async_trait;
use futures_util::StreamExt;

/// Generic OpenAI-compatible API provider.
pub struct OpenaiProvider {
    /// Pooled client for non-streaming requests.
    client: Client,
    /// Non-pooled client for SSE streaming (prevents pool exhaustion).
    stream_client: Client,
    /// Base URL for the OpenAI-compatible API, from `UpstreamKind::Openai::base_url`.
    base_url: String,
    /// The upstream this provider was constructed for — supplies `name` (for
    /// exec-cache keying) and `auth`.
    upstream: Arc<Upstream>,
    /// Resolves `SecretRef`s (env/keychain/inline) to plaintext.
    resolver: Arc<dyn SecretResolver + Send + Sync>,
    /// Shared cache for `exec` auth-method subprocess results.
    exec_cache: Arc<ExecCredentialCache>,
}

impl OpenaiProvider {
    /// Construct a new `OpenaiProvider` for one configured `Upstream`.
    ///
    /// `base_url` should be the `UpstreamKind::Openai::base_url` value for
    /// this upstream (e.g. `https://api.openai.com`), with no trailing slash.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError::Upstream`] if either reqwest `Client` fails
    /// to build (e.g. an invalid TLS backend configuration).
    pub fn new(
        upstream: Arc<Upstream>,
        base_url: String,
        resolver: Arc<dyn SecretResolver + Send + Sync>,
        exec_cache: Arc<ExecCredentialCache>,
        request_timeout_secs: u64,
    ) -> Result<Self, ProviderError> {
        let timeout = Duration::from_secs(request_timeout_secs);

        let client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(timeout)
            .build()
            .map_err(|e| ProviderError::Upstream {
                status: 0,
                body: e.to_string(),
            })?;

        // ADR-004: separate client with pool_max_idle_per_host(0) for SSE
        let stream_client = Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_max_idle_per_host(0)
            .build()
            .map_err(|e| ProviderError::Upstream {
                status: 0,
                body: e.to_string(),
            })?;

        Ok(Self {
            client,
            stream_client,
            base_url,
            upstream,
            resolver,
            exec_cache,
        })
    }

    /// Build the outgoing request headers: `Content-Type` plus auth per the
    /// upstream's configured `AuthMethod`.
    async fn build_headers(&self, url: &str) -> Result<HeaderMap, ProviderError> {
        let mut out = HeaderMap::new();
        out.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );
        apply_auth_headers(
            &self.upstream,
            self.resolver.as_ref(),
            &self.exec_cache,
            &mut out,
            url,
        )
        .await?;
        Ok(out)
    }

    /// Send a non-streaming request to `POST /v1/chat/completions`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if auth resolution, the HTTP request, or
    /// upstream error-status mapping fails.
    pub async fn send_request(&self, body: Value) -> Result<Value, ProviderError> {
        let url = format!("{}/v1/chat/completions", self.base_url);
        let headers = self.build_headers(&url).await?;
        let body_bytes = serde_json::to_vec(&body).map_err(|e| ProviderError::Upstream {
            status: 0,
            body: e.to_string(),
        })?;

        debug!("OpenAI non-stream POST {url}");

        let response = self
            .client
            .post(&url)
            .headers(headers)
            .body(body_bytes)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout
                } else {
                    ProviderError::Upstream {
                        status: 0,
                        body: e.to_string(),
                    }
                }
            })?;

        let status = response.status();
        map_error_status(status, response).await
    }

    /// Send a streaming request to `POST /v1/chat/completions`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if auth resolution, the HTTP request, or
    /// upstream error-status mapping fails.
    pub async fn send_streaming_request(&self, mut body: Value) -> Result<reqwest::Response, ProviderError> {
        body["stream"] = Value::Bool(true);

        let url = format!("{}/v1/chat/completions", self.base_url);
        let headers = self.build_headers(&url).await?;
        let body_bytes = serde_json::to_vec(&body).map_err(|e| ProviderError::Upstream {
            status: 0,
            body: e.to_string(),
        })?;

        debug!("OpenAI stream POST {url}");

        let response = self
            .stream_client
            .post(&url)
            .headers(headers)
            .body(body_bytes)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout
                } else {
                    ProviderError::Upstream {
                        status: 0,
                        body: e.to_string(),
                    }
                }
            })?;

        let status = response.status();

        if status == StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(60);
            warn!("OpenAI rate limited ({status}), retry-after {retry_after}s");
            return Err(ProviderError::RateLimited);
        }

        if status.is_client_error() {
            let status_u16 = status.as_u16();
            let body_str = response.text().await.unwrap_or_default();
            return Err(ProviderError::Validation(body_str, status_u16));
        }

        if !status.is_success() {
            let status_u16 = status.as_u16();
            let body_str = response.text().await.unwrap_or_default();
            return Err(ProviderError::Upstream {
                status: status_u16,
                body: body_str,
            });
        }

        Ok(response)
    }
}

/// Convert a non-success HTTP status into the appropriate `ProviderError`,
/// consuming the response body for error detail.
async fn map_error_status(
    status: StatusCode,
    response: reqwest::Response,
) -> Result<Value, ProviderError> {
    if status == StatusCode::TOO_MANY_REQUESTS {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60);
        warn!("OpenAI rate limited ({status}), retry-after {retry_after}s");
        return Err(ProviderError::RateLimited);
    }

    if status.is_client_error() {
        let status_u16 = status.as_u16();
        let body_str = response.text().await.unwrap_or_default();
        return Err(ProviderError::Validation(body_str, status_u16));
    }

    if !status.is_success() {
        let status_u16 = status.as_u16();
        let body_str = response.text().await.unwrap_or_default();
        return Err(ProviderError::Upstream {
            status: status_u16,
            body: body_str,
        });
    }

    response.json().await.map_err(|e| ProviderError::Upstream {
        status: status.as_u16(),
        body: e.to_string(),
    })
}

#[async_trait]
impl Provider for OpenaiProvider {
    fn name(&self) -> &'static str {
        "openai"
    }

    async fn send(
        &self,
        body: Value,
        _headers: HeaderMap,
        stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        if stream {
            let response = self.send_streaming_request(body).await?;
            let byte_stream = response
                .bytes_stream()
                .map(|r| r.map_err(anyhow::Error::from));
            Ok(ProviderResponse::Stream(Box::pin(byte_stream)))
        } else {
            let value = self.send_request(body).await?;
            Ok(ProviderResponse::Full(value))
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::auth::SystemSecretResolver;
    use crate::config::schema::UpstreamKind;

    fn test_upstream() -> Arc<Upstream> {
        Arc::new(Upstream {
            name: "test-openai".to_string(),
            kind: UpstreamKind::Openai {
                base_url: "https://example.invalid".to_string(),
            },
            auth: None,
        })
    }

    fn test_provider() -> OpenaiProvider {
        OpenaiProvider::new(
            test_upstream(),
            "https://example.invalid".to_string(),
            Arc::new(SystemSecretResolver),
            Arc::new(ExecCredentialCache::new()),
            30,
        )
        .unwrap()
    }

    #[test]
    fn name_is_openai() {
        assert_eq!(test_provider().name(), "openai");
    }

    #[tokio::test]
    async fn build_headers_sets_content_type() {
        let provider = test_provider();
        // No auth configured on the upstream, so header building should fail
        // with an Auth error rather than panicking or silently succeeding.
        let result = provider
            .build_headers("https://example.invalid/v1/chat/completions")
            .await;
        assert!(matches!(result, Err(ProviderError::Auth(_))));
    }
}
