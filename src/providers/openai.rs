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

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures_core::Stream;
use http::HeaderMap;
use reqwest::{Client, StatusCode};
use serde_json::{json, Value};
use tracing::{debug, warn};

use crate::auth::exec::ExecCredentialCache;
use crate::auth::SecretResolver;
use crate::config::schema::Upstream;

use super::anthropic::apply_auth_headers;
use super::{map_openai_finish_reason, Provider, ProviderError, ProviderResponse};

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

        if crate::providers::bodies_logged() {
            tracing::info!(
                target: "consolette::bodies",
                upstream = %self.upstream.name,
                body = %crate::providers::redact_bodies(&body),
                "openai upstream request"
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
            if let Ok(ref ok) = out {
                tracing::info!(
                    target: "consolette::bodies",
                    upstream = %self.upstream.name,
                    status = %status,
                    body = %crate::providers::redact_bodies(ok),
                    "openai upstream response"
                );
            }
        }
        out
    }

    /// Fetch the list of models from `GET /v1/models`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if auth resolution, the HTTP request, or
    /// upstream error-status mapping fails.
    pub async fn fetch_models(&self) -> Result<Value, ProviderError> {
        let url = format!("{}/v1/models", self.base_url);
        let headers = self.build_headers(&url).await?;

        debug!("OpenAI GET {url}");

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
        map_error_status(status, response).await
    }

    /// Send a streaming request to `POST /v1/chat/completions`.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] if auth resolution, the HTTP request, or
    /// upstream error-status mapping fails.
    pub async fn send_streaming_request(
        &self,
        mut body: Value,
    ) -> Result<reqwest::Response, ProviderError> {
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
        // `Provider::send` always receives/returns Anthropic-wire-format JSON
        // at the trait boundary (see `BedrockProvider` for the same pattern)
        // — translate to/from native OpenAI shape here, internal to this
        // provider.
        let model = body
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string();
        let openai_body = super::translate_anthropic_request_to_openai(&body);

        if stream {
            let response = self.send_streaming_request(openai_body).await?;
            let byte_stream = response
                .bytes_stream()
                .map(|r| r.map_err(anyhow::Error::from));
            let translated = OpenaiToAnthropicStream::new(byte_stream, model);
            Ok(ProviderResponse::Stream(Box::pin(translated)))
        } else {
            let value = self.send_request(openai_body).await?;
            let anthropic_value = super::translate_openai_response_to_anthropic(&value);
            Ok(ProviderResponse::Full(anthropic_value))
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
                let owned_by = entry
                    .get("owned_by")
                    .and_then(Value::as_str)
                    .map(ToString::to_string);
                Some(super::ModelInfo { id, owned_by })
            })
            .collect();
        Ok(models)
    }
}

/// Translates an `OpenAI` `chat.completion.chunk` SSE byte stream into an
/// Anthropic Messages SSE event stream — the reverse of
/// `entrypoint::openai_stream::OpenAiStreamTranslator`. Anthropic's SSE
/// protocol requires bracketing events (`message_start`,
/// `content_block_start`, ... `content_block_stop`, `message_delta`,
/// `message_stop`) that `OpenAI`'s flatter chunk stream has no equivalent
/// for, so a single inbound chunk can produce more than one outbound frame;
/// `pending` buffers those extras between polls.
///
/// Block layout: text is always block 0 (possibly empty); tool calls become
/// blocks 1..n in order of first appearance. Tool argument fragments
/// accumulate per `OpenAI` wire `index` and stream out as `input_json_delta`
/// events, so chunked arguments reassemble into one valid JSON object.
/// Four independent lifecycle flags (`started`/`finished`/`done` for the
/// overall stream plus `text_started` for block 0); splitting them into a
/// state machine would obscure the one-way pipeline, hence the allow.
#[allow(clippy::struct_excessive_bools)]
pub(crate) struct OpenaiToAnthropicStream<S> {
    inner: eventsource_stream::EventStream<S>,
    id: String,
    model: String,
    started: bool,
    finished: bool,
    done: bool,
    pending: VecDeque<Bytes>,
    text_started: bool,
    tools: Vec<ToolSlot>,
}

/// One in-progress tool call block: arguments accumulate here until the
/// closing stop, so split fragments reassemble into valid JSON.
struct ToolSlot {
    /// `OpenAI` wire `index` from the chunk (need not be dense).
    oi_index: usize,
    /// Block index on the Anthropic face (1-based; 0 is text).
    block_index: usize,
    id: String,
    name: String,
    args: String,
    started: bool,
    stopped: bool,
}

impl<S> OpenaiToAnthropicStream<S>
where
    S: Stream<Item = Result<Bytes, anyhow::Error>>,
{
    pub(crate) fn new(inner: S, model: String) -> Self {
        Self {
            inner: inner.eventsource(),
            id: format!("msg_{}", uuid::Uuid::new_v4()),
            model,
            started: false,
            finished: false,
            done: false,
            pending: VecDeque::new(),
            text_started: false,
            tools: Vec::new(),
        }
    }

    fn frame(event: &str, data: &Value) -> Bytes {
        Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
    }

    /// Push the synthetic `message_start`, so every stream opens with a
    /// well-formed Anthropic preamble even if the first `OpenAI` chunk
    /// carries no text.
    fn ensure_message_started(&mut self) {
        if self.started {
            return;
        }
        self.started = true;
        self.pending.push_back(Self::frame(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "content": [],
                    "model": self.model,
                    "stop_reason": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            }),
        ));
    }

    /// Push the text `content_block_start` (index 0). Text always owns block
    /// 0 — possibly empty — so tool blocks can take stable indices 1..n.
    fn ensure_text_started(&mut self) {
        self.ensure_message_started();
        if self.text_started {
            return;
        }
        self.text_started = true;
        self.pending.push_back(Self::frame(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {"type": "text", "text": ""}
            }),
        ));
    }

    fn push_delta(&mut self, text: &str) {
        self.ensure_text_started();
        self.pending.push_back(Self::frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "text_delta", "text": text}
            }),
        ));
    }

    /// Fold one `OpenAI` tool-call delta entry into its Anthropic block:
    /// accumulate id/name/arguments by wire `index`, open the block once
    /// id and name are known, and stream argument fragments as
    /// `input_json_delta` events.
    fn push_tool_delta(
        &mut self,
        oi_index: usize,
        id: Option<&str>,
        name: Option<&str>,
        args_frag: &str,
    ) {
        self.ensure_message_started();
        let pos = if let Some(pos) = self.tools.iter().position(|t| t.oi_index == oi_index) {
            pos
        } else {
            // OpenAI indices need not be dense; Anthropic block indices
            // must be, so slots take 1-based positions in appearance
            // order regardless of the wire index.
            self.tools.push(ToolSlot {
                oi_index,
                block_index: self.tools.len() + 1,
                id: String::new(),
                name: String::new(),
                args: String::new(),
                started: false,
                stopped: false,
            });
            self.tools.len() - 1
        };
        // NOTE: position-by-wire-index assumes the gateway reuses one wire
        // index per call; a gateway renumbering mid-stream would split one
        // call into two blocks (fail-closed, still well-formed).
        let slot = &mut self.tools[pos];
        if slot.id.is_empty() {
            if let Some(id) = id {
                slot.id = id.to_string();
            }
        }
        if slot.name.is_empty() {
            if let Some(name) = name {
                slot.name = name.to_string();
            }
        }
        slot.args.push_str(args_frag);
        if !slot.started && !slot.id.is_empty() && !slot.name.is_empty() {
            slot.started = true;
            let (block_index, id, name, args) = (
                slot.block_index,
                slot.id.clone(),
                slot.name.clone(),
                slot.args.clone(),
            );
            self.pending.push_back(Self::frame(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": block_index,
                    "content_block": {
                        "type": "tool_use", "id": id, "name": name, "input": {}
                    }
                }),
            ));
            if !args.is_empty() {
                self.pending.push_back(Self::frame(
                    "content_block_delta",
                    &json!({
                        "type": "content_block_delta",
                        "index": block_index,
                        "delta": {"type": "input_json_delta", "partial_json": args}
                    }),
                ));
            }
        } else if slot.started && !args_frag.is_empty() {
            let block_index = slot.block_index;
            self.pending.push_back(Self::frame(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": {"type": "input_json_delta", "partial_json": args_frag}
                }),
            ));
        }
    }

    /// Emit the closing stops for every open block plus the
    /// `message_delta`/`message_stop` sequence. Idempotent. A stream that
    /// opened any tool block always closes `tool_use` (mirroring the
    /// non-streaming rule: blocks present ⇔ matching stop reason).
    fn close(&mut self, stop_reason: &str) {
        if self.finished {
            return;
        }
        self.ensure_text_started();
        self.finished = true;
        let mut stop_reason = stop_reason.to_string();
        for slot in &mut self.tools {
            if slot.started && !slot.stopped {
                slot.stopped = true;
                let block_index = slot.block_index;
                self.pending.push_back(Self::frame(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": block_index}),
                ));
            }
        }
        if self.tools.iter().any(|t| t.started) {
            stop_reason = "tool_use".to_string();
        }
        self.pending.push_back(Self::frame(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": 0}),
        ));
        self.pending.push_back(Self::frame(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason},
                "usage": {"output_tokens": 0}
            }),
        ));
        self.pending.push_back(Self::frame(
            "message_stop",
            &json!({"type": "message_stop"}),
        ));
    }
}

impl<S> Stream for OpenaiToAnthropicStream<S>
where
    S: Stream<Item = Result<Bytes, anyhow::Error>> + Unpin,
{
    type Item = Result<Bytes, anyhow::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        loop {
            if let Some(frame) = this.pending.pop_front() {
                return Poll::Ready(Some(Ok(frame)));
            }
            if this.done {
                return Poll::Ready(None);
            }
            if this.finished {
                this.done = true;
                continue;
            }

            match Pin::new(&mut this.inner).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    this.close("end_turn");
                }
                Poll::Ready(Some(Err(e))) => {
                    warn!(error = %e, "openai->anthropic stream translator: eventsource parse error");
                    this.close("end_turn");
                }
                Poll::Ready(Some(Ok(event))) => {
                    if event.data == "[DONE]" {
                        this.close("end_turn");
                        continue;
                    }
                    if crate::providers::bodies_logged() {
                        tracing::info!(
                            target: "consolette::bodies",
                            model = %this.model,
                            chunk = %event.data,
                            "openai upstream stream chunk"
                        );
                    }
                    let Ok(parsed) = serde_json::from_str::<Value>(&event.data) else {
                        continue;
                    };

                    let choice = parsed
                        .get("choices")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first());

                    // Delta content arrives as a string on most gateways,
                    // but OpenRouter-style responses may send an array of
                    // parts or put tokens in `reasoning_content`/`reasoning`
                    // (reasoning models). Either shape collapsing to ""
                    // is what produced empty client text with nonzero
                    // usage, so extract text from all of them.
                    let delta = choice.and_then(|c| c.get("delta"));
                    let mut text = delta
                        .and_then(|d| d.get("content"))
                        .map(crate::providers::extract_text_from_content)
                        .unwrap_or_default();
                    if text.is_empty() {
                        text = delta
                            .and_then(|d| d.get("reasoning_content").or_else(|| d.get("reasoning")))
                            .map(crate::providers::extract_text_from_content)
                            .unwrap_or_default();
                    }
                    if !text.is_empty() {
                        this.push_delta(&text);
                    }

                    if let Some(calls) = delta
                        .and_then(|d| d.get("tool_calls"))
                        .and_then(Value::as_array)
                    {
                        for (pos, call) in calls.iter().enumerate() {
                            let oi_index = call
                                .get("index")
                                .and_then(Value::as_u64)
                                .map_or(pos, |i| usize::try_from(i).unwrap_or(pos));
                            let function = call.get("function");
                            this.push_tool_delta(
                                oi_index,
                                call.get("id").and_then(Value::as_str),
                                function.and_then(|f| f.get("name")).and_then(Value::as_str),
                                function
                                    .and_then(|f| f.get("arguments"))
                                    .and_then(Value::as_str)
                                    .unwrap_or(""),
                            );
                        }
                    }

                    if let Some(reason) = choice
                        .and_then(|c| c.get("finish_reason"))
                        .and_then(Value::as_str)
                    {
                        this.close(map_openai_finish_reason(Some(reason)));
                    }
                }
            }
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

    // ────────────────────────────────────────────────────────────────────
    // OpenaiToAnthropicStream
    // ────────────────────────────────────────────────────────────────────

    mod stream_translator {
        use super::*;
        use futures_util::stream;

        async fn drain<S>(s: S) -> Vec<Bytes>
        where
            S: Stream<Item = Result<Bytes, anyhow::Error>>,
        {
            s.map(|item| item.unwrap()).collect().await
        }

        fn sse(data: &str) -> Bytes {
            Bytes::from(format!("data: {data}\n\n"))
        }

        fn parse_event(frame: &Bytes) -> (String, Value) {
            let text = String::from_utf8(frame.to_vec()).unwrap();
            let mut event = String::new();
            let mut data = String::new();
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("event: ") {
                    event = rest.to_string();
                } else if let Some(rest) = line.strip_prefix("data: ") {
                    data = rest.to_string();
                }
            }
            (event, serde_json::from_str(&data).unwrap())
        }

        #[tokio::test]
        async fn content_delta_is_bracketed_by_start_and_stop_events() {
            let inner = stream::iter(vec![
                Ok(sse(
                    r#"{"choices":[{"delta":{"content":"Hi"},"finish_reason":null}]}"#,
                )),
                Ok(sse(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#)),
                Ok(sse("[DONE]")),
            ]);
            let translator = OpenaiToAnthropicStream::new(inner, "gpt-4o".to_string());
            let out = drain(translator).await;

            let events: Vec<String> = out.iter().map(|f| parse_event(f).0).collect();
            assert_eq!(
                events,
                vec![
                    "message_start",
                    "content_block_start",
                    "content_block_delta",
                    "content_block_stop",
                    "message_delta",
                    "message_stop",
                ]
            );

            let (_, delta_data) = parse_event(&out[2]);
            assert_eq!(delta_data["delta"]["text"], "Hi");

            let (_, message_delta_data) = parse_event(&out[4]);
            assert_eq!(message_delta_data["delta"]["stop_reason"], "end_turn");
        }

        #[tokio::test]
        async fn length_finish_reason_maps_to_max_tokens_stop_reason() {
            let inner = stream::iter(vec![Ok(sse(
                r#"{"choices":[{"delta":{"content":"x"},"finish_reason":"length"}]}"#,
            ))]);
            let translator = OpenaiToAnthropicStream::new(inner, "gpt-4o".to_string());
            let out = drain(translator).await;

            let (_, message_delta_data) = parse_event(&out[4]);
            assert_eq!(message_delta_data["delta"]["stop_reason"], "max_tokens");
        }

        #[tokio::test]
        async fn array_and_reasoning_deltas_produce_text() {
            // Gateway may send delta content as parts or put tokens in
            // `reasoning_content`; both must surface as text deltas rather
            // than vanishing (the empty-stream symptom).
            let inner = stream::iter(vec![
                Ok(sse(
                    r#"{"choices":[{"delta":{"content":[{"type":"text","text":"A"}]},"finish_reason":null}]}"#,
                )),
                Ok(sse(
                    r#"{"choices":[{"delta":{"reasoning_content":"th"},"finish_reason":null}]}"#,
                )),
                Ok(sse(r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#)),
                Ok(sse("[DONE]")),
            ]);
            let translator = OpenaiToAnthropicStream::new(inner, "gpt-4o".to_string());
            let out = drain(translator).await;

            let texts: Vec<String> = out
                .iter()
                .map(parse_event)
                .filter(|(e, _)| e == "content_block_delta")
                .map(|(_, d)| d["delta"]["text"].as_str().unwrap_or("").to_string())
                .collect();
            assert_eq!(texts, vec!["A".to_string(), "th".to_string()]);
        }

        #[tokio::test]
        #[allow(clippy::expect_used)]
        async fn chunked_tool_call_accumulates_into_one_tool_use_block() {
            // Arguments split across chunks must reassemble: block indices
            // stable, partial_json concatenates to valid JSON, stop is
            // tool_use. This is the streaming half of the premature-stop
            // repro (Claude Code streams).
            let inner = stream::iter(vec![
                Ok(sse(
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_time","arguments":""}}]},"finish_reason":null}]}"#,
                )),
                Ok(sse(
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"zone\":"}}]},"finish_reason":null}]}"#,
                )),
                Ok(sse(
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"utc\"}"}}]},"finish_reason":null}]}"#,
                )),
                Ok(sse(
                    r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}]}"#,
                )),
                Ok(sse("[DONE]")),
            ]);
            let translator = OpenaiToAnthropicStream::new(inner, "gpt-4o".to_string());
            let out = drain(translator).await;
            let events: Vec<(String, Value)> = out.iter().map(parse_event).collect();
            let kinds: Vec<&str> = events.iter().map(|(e, _)| e.as_str()).collect();

            // Text block 0 opens (possibly empty), tool block takes index 1.
            let tool_start = events
                .iter()
                .find(|(e, d)| {
                    e == "content_block_start" && d["content_block"]["type"] == "tool_use"
                })
                .expect("tool_use content_block_start");
            assert_eq!(tool_start.1["index"], 1);
            assert_eq!(tool_start.1["content_block"]["id"], "call_1");
            assert_eq!(tool_start.1["content_block"]["name"], "get_time");

            let partials: String = events
                .iter()
                .filter(|(e, d)| {
                    e == "content_block_delta" && d["delta"]["type"] == "input_json_delta"
                })
                .map(|(_, d)| {
                    d["delta"]["partial_json"]
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                })
                .collect();
            assert_eq!(partials, "{\"zone\":\"utc\"}");
            serde_json::from_str::<Value>(&partials).expect("reassembled args parse");

            // Every start has a matching stop on the same index.
            for idx in [0, 1] {
                let starts = events
                    .iter()
                    .filter(|(e, d)| e == "content_block_start" && d["index"] == idx)
                    .count();
                let stops = events
                    .iter()
                    .filter(|(e, d)| e == "content_block_stop" && d["index"] == idx)
                    .count();
                assert_eq!((starts, stops), (1, 1), "bracket mismatch at {idx}");
            }

            let (_, message_delta_data) = parse_event(&out[out.len() - 2]);
            assert_eq!(message_delta_data["delta"]["stop_reason"], "tool_use");
            assert_eq!(kinds.last(), Some(&"message_stop"));
        }

        #[tokio::test]
        async fn empty_stream_still_produces_well_formed_bracketing_events() {
            let inner: futures_util::stream::Iter<
                std::vec::IntoIter<Result<Bytes, anyhow::Error>>,
            > = stream::iter(vec![]);
            let translator = OpenaiToAnthropicStream::new(inner, "gpt-4o".to_string());
            let out = drain(translator).await;

            let events: Vec<String> = out.iter().map(|f| parse_event(f).0).collect();
            assert_eq!(
                events,
                vec![
                    "message_start",
                    "content_block_start",
                    "content_block_stop",
                    "message_delta",
                    "message_stop",
                ]
            );
        }
    }
}
