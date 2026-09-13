//! Anthropic API provider.
//!
//! Forwards requests to `https://api.anthropic.com/v1/messages`, cleaning
//! Claude Code / Bedrock-specific fields that the Anthropic API rejects.
//!
//! Ported from the legacy `claude-proxy-rs` `AnthropicProvider`. The pure
//! request/response logic (`clean_request_body`, `normalize_model_name`,
//! `map_error_status`, the two reqwest clients from ADR-004) carries over
//! unchanged. What changed is auth and config:
//!
//! - Legacy sniffed the token shape (`sk-ant-api-*` vs OAuth) to decide
//!   `x-api-key` vs `Authorization: Bearer`. The new schema makes that an
//!   explicit per-`Upstream` choice (`AuthMethod::Bearer`/`Apikey`/`Exec`),
//!   so header selection is now driven by config, not by inspecting the
//!   secret's contents.
//! - Legacy took a flat `Config` with `claude_code_oauth_token`. That field
//!   no longer exists; the token (or API key, or exec helper) now comes from
//!   the `Upstream`'s `auth: Option<AuthMethod>` plus a `SecretResolver`.
//!
//! Cross-provider fallback/cooldown is NOT reimplemented here — `Router` +
//! `HealthRegistry` own that (see `routing::router::Router::dispatch`). This
//! provider only ever makes a single attempt per `send()` call.

use std::sync::Arc;
use std::time::Duration;

use http::HeaderMap;
use reqwest::{Client, StatusCode};
use serde_json::Value;
use tracing::{debug, info, warn};

use crate::auth::exec::ExecCredentialCache;
use crate::auth::{AuthError, SecretResolver};
use crate::config::schema::{AuthMethod, Upstream};

use super::{Provider, ProviderError, ProviderResponse};

use async_trait::async_trait;
use futures_util::StreamExt;

/// Anthropic API provider.
///
/// Holds two separate `reqwest` clients as specified by ADR-004:
/// - `client`: pooled, for short non-streaming requests
/// - `stream_client`: `pool_max_idle_per_host(0)`, for long-lived SSE streams
pub struct AnthropicProvider {
    /// Pooled client for non-streaming requests.
    client: Client,
    /// Non-pooled client for SSE streaming (prevents pool exhaustion).
    stream_client: Client,
    /// Base URL for the Anthropic API. `UpstreamKind::Anthropic` carries no
    /// base-URL override field in the new schema (only `Openai` does), so
    /// this is hardcoded, matching legacy's default. See the final port
    /// report for this gap.
    base_url: String,
    /// The upstream this provider was constructed for — supplies `name` (for
    /// logging/exec-cache keying) and `auth`.
    upstream: Arc<Upstream>,
    /// Resolves `SecretRef`s (env/keychain/inline) to plaintext.
    resolver: Arc<dyn SecretResolver + Send + Sync>,
    /// Shared cache for `exec` auth-method subprocess results.
    exec_cache: Arc<ExecCredentialCache>,
}

impl AnthropicProvider {
    /// Construct a new `AnthropicProvider` for one configured `Upstream`.
    ///
    /// Callers at HTTP-server startup have a `&config::schema::Config`; they
    /// should locate the relevant `Upstream` (by `UpstreamKind::Anthropic`)
    /// and pass it here wrapped in `Arc`, along with a shared
    /// `SecretResolver` (e.g. `Arc::new(SystemSecretResolver)`) and a shared
    /// `ExecCredentialCache` (one per process, so exec-auth subprocess
    /// results are cached across requests/upstreams). `request_timeout_secs`
    /// comes from the top-level `Config::request_timeout`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError::Upstream`] if either reqwest `Client`
    /// fails to build (e.g. an invalid TLS backend configuration).
    pub fn new(
        upstream: Arc<Upstream>,
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
            base_url: "https://api.anthropic.com".to_string(),
            upstream,
            resolver,
            exec_cache,
        })
    }

    /// Normalize a model name for the Anthropic API.
    ///
    /// Claude Code occasionally sends Bedrock-format names (e.g.
    /// `us.anthropic.claude-3-5-sonnet-20241022-v1:0`) to the Anthropic
    /// endpoint.  Strip the prefix and version suffix so the API accepts it.
    fn normalize_model_name(model: &str) -> String {
        let model = if let Some(stripped) = model.strip_prefix("us.anthropic.") {
            stripped.to_string()
        } else {
            model.to_string()
        };

        // Remove trailing version suffix: -v1:0 or -v1
        MODEL_VERSION_RE.replace(&model, "").to_string()
    }

    /// Apply this upstream's configured `AuthMethod` to `out`.
    ///
    /// This intentionally does NOT call `auth::AuthMethodExt::apply` even
    /// though that's the trait built for exactly this purpose: that trait is
    /// declared `#[async_trait(?Send)]` (needed because it takes `&dyn
    /// SecretResolver` with no `Send` bound), which makes its returned
    /// future a `!Send` trait object. `Provider::send` is a default
    /// `#[async_trait]` (Send-bounded) method, so awaiting a `!Send` future
    /// across a suspension point inside it fails to compile — the two traits
    /// have incompatible Send requirements as they stand today.
    ///
    /// The workaround: `SecretResolver::resolve` is itself synchronous (no
    /// `.await` needed for `Bearer`/`Apikey`), and `ExecCredentialCache::get_or_run`
    /// is a concrete (non-trait-object) async fn that IS `Send`. So this
    /// reproduces `AuthMethod::apply`'s dispatch using those two primitives
    /// directly, which keeps `send()`'s generated future `Send` while still
    /// going through the real ADR-002 resolver/cache rather than
    /// reimplementing secret resolution or exec dispatch.
    async fn apply_auth(&self, out: &mut HeaderMap, url: &str) -> Result<(), ProviderError> {
        apply_auth_headers(
            &self.upstream,
            self.resolver.as_ref(),
            &self.exec_cache,
            out,
            url,
        )
        .await
    }

    /// Build the outgoing request headers.
    ///
    /// - Always sets `Content-Type: application/json`.
    /// - Forwards `anthropic-version` and `anthropic-beta` from the client if
    ///   present, otherwise falls back to the default version.
    /// - Sets auth per the upstream's configured `AuthMethod` (see
    ///   `apply_auth`).
    async fn build_headers(
        &self,
        incoming: &HeaderMap,
        url: &str,
    ) -> Result<HeaderMap, ProviderError> {
        let mut out = HeaderMap::new();

        // content-type is always application/json
        out.insert(
            reqwest::header::CONTENT_TYPE,
            reqwest::header::HeaderValue::from_static("application/json"),
        );

        // Forward anthropic-version (fall back to stable default)
        let version = incoming
            .get("anthropic-version")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("2023-06-01");
        if let Ok(v) = reqwest::header::HeaderValue::from_str(version) {
            out.insert("anthropic-version", v);
        }

        // Forward anthropic-beta if present
        if let Some(beta) = incoming.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(beta) {
                out.insert("anthropic-beta", v);
            }
        }

        self.apply_auth(&mut out, url).await?;

        Ok(out)
    }

    /// Send a non-streaming request to `POST /v1/messages`.
    ///
    /// Returns the full response body as a `serde_json::Value` plus the HTTP status.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if header construction/auth fails, the
    /// request times out, or the upstream responds with a non-2xx status.
    pub async fn send_request(
        &self,
        mut body: Value,
        incoming_headers: &HeaderMap,
    ) -> Result<(Value, StatusCode), ProviderError> {
        // Normalize model name
        if let Some(model) = body.get("model").and_then(|v| v.as_str()) {
            let normalized = Self::normalize_model_name(model);
            body["model"] = Value::String(normalized);
        }

        // Clean the request body (strips Bedrock-specific / unsupported fields)
        clean_request_body(&mut body);

        let url = format!("{}/v1/messages", self.base_url);
        let headers = self.build_headers(incoming_headers, &url).await?;
        let body_bytes = serde_json::to_vec(&body).map_err(|e| ProviderError::Upstream {
            status: 0,
            body: e.to_string(),
        })?;

        debug!("Anthropic non-stream POST {url}");

        if crate::providers::bodies_logged() {
            tracing::info!(
                target: "consolette::bodies",
                upstream = %self.upstream.name,
                body = %crate::providers::redact_bodies(&body),
                "anthropic upstream request"
            );
        }

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
        let out = map_error_status(status, response).await;
        if crate::providers::bodies_logged() {
            if let Ok((ref ok, _)) = out {
                tracing::info!(
                    target: "consolette::bodies",
                    upstream = %self.upstream.name,
                    status = %status,
                    body = %crate::providers::redact_bodies(ok),
                    "anthropic upstream response"
                );
            }
        }
        out
    }

    /// Send a streaming request to `POST /v1/messages`.
    ///
    /// Returns the raw `reqwest::Response` for the caller to drive as an SSE
    /// byte stream.  The caller is responsible for iterating `bytes_stream()`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if header construction/auth fails, the
    /// request times out, or the upstream responds with a non-2xx status.
    pub async fn send_streaming_request(
        &self,
        mut body: Value,
        incoming_headers: &HeaderMap,
    ) -> Result<reqwest::Response, ProviderError> {
        // Ensure stream flag is set
        body["stream"] = Value::Bool(true);

        // Normalize model name
        if let Some(model) = body.get("model").and_then(|v| v.as_str()) {
            let normalized = Self::normalize_model_name(model);
            body["model"] = Value::String(normalized);
        }

        // Clean the request body
        clean_request_body(&mut body);

        let url = format!("{}/v1/messages", self.base_url);
        let headers = self.build_headers(incoming_headers, &url).await?;
        let body_bytes = serde_json::to_vec(&body).map_err(|e| ProviderError::Upstream {
            status: 0,
            body: e.to_string(),
        })?;

        debug!("Anthropic stream POST {url}");

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

        if status == StatusCode::TOO_MANY_REQUESTS || status.as_u16() == 529 {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(60);
            warn!("Anthropic rate limited ({status}), retry-after {retry_after}s");
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

    /// Fetch the list of models from `GET /v1/models`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if header construction/auth fails, the
    /// request times out, or the upstream responds with a non-2xx status.
    pub async fn fetch_models(&self) -> Result<Value, ProviderError> {
        let url = format!("{}/v1/models", self.base_url);
        let headers = self.build_headers(&HeaderMap::new(), &url).await?;

        debug!("Anthropic GET {url}");

        let response = self
            .client
            .get(&url)
            .headers(headers)
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
        let (value, _status) = map_error_status(status, response).await?;
        Ok(value)
    }
}

/// Apply `upstream`'s configured [`AuthMethod`] to `out`.
///
/// Extracted from [`AnthropicProvider::apply_auth`] as a free function (see
/// `project_plans/compaction-cost-metrics/implementation/plan.md` Task
/// 1.2.2a) so `cost_metrics::estimator::AnthropicCountTokensEstimator` can
/// reuse the exact same secret-resolution path — `x-api-key`/`Bearer`
/// header selection driven by config, `exec` helper dispatch through the
/// shared [`ExecCredentialCache`] — without duplicating it or requiring a
/// full [`AnthropicProvider`] instance.
///
/// # Errors
///
/// Returns [`ProviderError::Auth`] if no `AuthMethod` is configured, the
/// secret fails to resolve, or the resolved value isn't a valid header
/// value.
pub async fn apply_auth_headers(
    upstream: &Upstream,
    resolver: &(dyn SecretResolver + Send + Sync),
    exec_cache: &ExecCredentialCache,
    out: &mut HeaderMap,
    url: &str,
) -> Result<(), ProviderError> {
    let method = upstream
        .auth
        .as_ref()
        .ok_or_else(|| ProviderError::Auth("no auth configured for upstream".to_string()))?;

    let result: Result<(), AuthError> = match method {
        AuthMethod::Bearer { token } => {
            let value = resolver.resolve(token)?;
            insert_header(out, "authorization", &format!("Bearer {value}"))
        }
        AuthMethod::Apikey { key, header } => {
            let value = resolver.resolve(key)?;
            insert_header(out, header, &value)
        }
        AuthMethod::Exec {
            command,
            args,
            cache_ttl_secs,
            timeout_secs,
        } => {
            match exec_cache
                .get_or_run(
                    &upstream.name,
                    command,
                    args,
                    Duration::from_secs(*cache_ttl_secs),
                    Duration::from_secs(*timeout_secs),
                    "POST",
                    url,
                )
                .await
            {
                Ok(helper_headers) => {
                    for (name, value) in &helper_headers {
                        out.insert(name.clone(), value.clone());
                    }
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
    };

    result.map_err(|e| ProviderError::Auth(e.to_string()))
}

/// Insert a header, converting name/value validation failures into
/// `AuthError` the same way `auth::AuthMethodExt::apply`'s private helper
/// does (kept local since that helper isn't `pub`).
fn insert_header(headers: &mut HeaderMap, name: &str, value: &str) -> Result<(), AuthError> {
    let header_name = http::HeaderName::from_bytes(name.as_bytes())
        .map_err(|_| AuthError::InvalidHeaderName(name.to_string()))?;
    let header_value = http::HeaderValue::from_str(value)
        .map_err(|_| AuthError::InvalidHeaderValue(name.to_string()))?;
    headers.insert(header_name, header_value);
    Ok(())
}

// ---------------------------------------------------------------------------
// Body cleaning (ADR-007 — serde_json::Value throughout, no typed structs)
// ---------------------------------------------------------------------------

/// Lazy-compiled regex for stripping Bedrock model version suffixes.
static MODEL_VERSION_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    #[allow(clippy::unwrap_used)] // literal pattern is known-valid at compile time; cannot fail
    regex::Regex::new(r"-v\d+(?::\d+)?$").unwrap()
});

/// Clean a request body in-place, removing fields that the Anthropic API
/// rejects.  Matches the Python `_clean_request_body` implementation.
///
/// Strips:
/// - From each `tools[]` entry: `defer_loading`, `input_examples`, `custom`,
///   `cache_control`
/// - From `messages[*].content[*]` of type `tool_result`: removes any
///   `content[]` items whose `type` is not in the supported set
/// - From `system[]` `cache_control` objects: removes nested `ephemeral.scope`
/// - Top-level: `output_config`, `context_management`
pub fn clean_request_body(body: &mut Value) {
    // 1. Clean tools[]
    if let Some(tools) = body.get_mut("tools").and_then(|v| v.as_array_mut()) {
        for tool in tools.iter_mut() {
            if let Some(obj) = tool.as_object_mut() {
                for field in &["defer_loading", "input_examples", "custom", "cache_control"] {
                    if obj.remove(*field).is_some() {
                        debug!("Removed '{field}' from tool definition");
                    }
                }
            }
        }
    }

    // 2. Clean messages[*].content[*] — remove unsupported types from tool_result content
    if let Some(messages) = body.get_mut("messages").and_then(|v| v.as_array_mut()) {
        for message in messages.iter_mut() {
            if let Some(content) = message.get_mut("content").and_then(|v| v.as_array_mut()) {
                for item in content.iter_mut() {
                    if item.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                        if let Some(inner) = item.get_mut("content").and_then(|v| v.as_array_mut())
                        {
                            let before = inner.len();
                            inner.retain(|c| {
                                c.get("type").and_then(|t| t.as_str()).is_none_or(|t| {
                                    matches!(
                                        t,
                                        "text"
                                            | "image"
                                            | "document"
                                            | "search_result"
                                            | "tool_use"
                                            | "tool_result"
                                    )
                                })
                            });
                            let removed = before - inner.len();
                            if removed > 0 {
                                debug!(
                                    "Removed {removed} unsupported content block(s) from tool_result"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    // 3. Clean system[*].cache_control.ephemeral.scope
    if let Some(system) = body.get_mut("system").and_then(|v| v.as_array_mut()) {
        for item in system.iter_mut() {
            if let Some(cc) = item
                .get_mut("cache_control")
                .and_then(|v| v.as_object_mut())
            {
                if let Some(ephemeral) = cc.get_mut("ephemeral").and_then(|v| v.as_object_mut()) {
                    if ephemeral.remove("scope").is_some() {
                        debug!("Removed 'scope' from system[].cache_control.ephemeral");
                    }
                }
            }
        }
    }

    // 4. Strip top-level Bedrock-specific fields
    if let Some(obj) = body.as_object_mut() {
        for field in &["output_config", "context_management"] {
            if obj.remove(*field).is_some() {
                info!("Stripped Bedrock-specific top-level field '{field}'");
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Error mapping helper
// ---------------------------------------------------------------------------

/// Convert a non-success HTTP status into the appropriate `ProviderError`,
/// consuming the response body for error detail.
async fn map_error_status(
    status: StatusCode,
    response: reqwest::Response,
) -> Result<(Value, StatusCode), ProviderError> {
    if status == StatusCode::TOO_MANY_REQUESTS || status.as_u16() == 529 {
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(60);
        warn!("Anthropic rate limited ({status}), retry-after {retry_after}s");
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

    let resp_value: Value = response.json().await.map_err(|e| ProviderError::Upstream {
        status: status.as_u16(),
        body: e.to_string(),
    })?;

    Ok((resp_value, status))
}

// ---------------------------------------------------------------------------
// Provider trait impl (wires AnthropicProvider into the ADR-003 Router)
// ---------------------------------------------------------------------------

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    async fn send(
        &self,
        body: Value,
        headers: HeaderMap,
        stream: bool,
    ) -> Result<ProviderResponse, ProviderError> {
        if stream {
            let response = self.send_streaming_request(body, &headers).await?;
            let byte_stream = response
                .bytes_stream()
                .map(|r| r.map_err(anyhow::Error::from));
            Ok(ProviderResponse::Stream(Box::pin(byte_stream)))
        } else {
            let (value, _status) = self.send_request(body, &headers).await?;
            Ok(ProviderResponse::Full(value))
        }
    }

    async fn list_models(&self) -> Result<Vec<super::ModelInfo>, ProviderError> {
        let value = self.fetch_models().await?;
        let models = value
            .get("data")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|entry| {
                let id = entry.get("id").and_then(Value::as_str)?.to_string();
                Some(super::ModelInfo {
                    id,
                    owned_by: Some("anthropic".to_string()),
                })
            })
            .collect();
        Ok(models)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // test assertions on serde_json::Value shapes we just constructed
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_model_name_strips_bedrock_prefix_and_suffix() {
        assert_eq!(
            AnthropicProvider::normalize_model_name("us.anthropic.claude-3-5-sonnet-20241022-v1:0"),
            "claude-3-5-sonnet-20241022"
        );
    }

    #[test]
    fn normalize_model_name_leaves_plain_names_alone() {
        assert_eq!(
            AnthropicProvider::normalize_model_name("claude-3-5-sonnet-20241022"),
            "claude-3-5-sonnet-20241022"
        );
    }

    #[test]
    fn clean_request_body_strips_tool_fields() {
        let mut body = json!({
            "tools": [
                {"name": "t1", "defer_loading": true, "cache_control": {"type": "ephemeral"}}
            ]
        });
        clean_request_body(&mut body);
        let tool = &body["tools"][0];
        assert!(tool.get("defer_loading").is_none());
        assert!(tool.get("cache_control").is_none());
        assert_eq!(tool["name"], "t1");
    }

    #[test]
    fn clean_request_body_strips_top_level_bedrock_fields() {
        let mut body = json!({"output_config": {}, "context_management": {}, "model": "x"});
        clean_request_body(&mut body);
        assert!(body.get("output_config").is_none());
        assert!(body.get("context_management").is_none());
        assert_eq!(body["model"], "x");
    }

    #[test]
    fn clean_request_body_filters_unsupported_tool_result_content() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "content": [
                        {"type": "text", "text": "ok"},
                        {"type": "tool_reference"}
                    ]
                }]
            }]
        });
        clean_request_body(&mut body);
        let inner = body["messages"][0]["content"][0]["content"]
            .as_array()
            .unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0]["type"], "text");
    }
}
