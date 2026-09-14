//! Provider abstraction and error classification, shared unchanged by all
//! upstream implementations and the ADR-003 router.
//!
//! Only the trait/error contract lives here — concrete HTTP clients
//! (Anthropic, Bedrock, OpenAI-compatible) are a separable, larger piece of
//! work and land later; the router only ever depends on `Provider`.
#![allow(dead_code)]

pub mod anthropic;
pub mod bedrock;
pub mod gemini;
pub mod openai;
pub mod openrouter;

use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use futures_core::Stream;
use http::HeaderMap;

/// The response returned by a provider.
pub enum ProviderResponse {
    /// A complete, buffered JSON response body.
    Full(serde_json::Value),
    /// An SSE byte stream. Cross-upstream failover is only possible before
    /// the first item is polled — once bytes start flushing to the caller,
    /// the router can no longer retry on a different upstream.
    Stream(Pin<Box<dyn Stream<Item = Result<Bytes, anyhow::Error>> + Send>>),
}

/// Errors any provider implementation can return. Classification methods
/// below are what the router's dispatch loop branches on.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("rate limited")]
    RateLimited,
    #[error("rate limited (retry after {retry_after}s)")]
    RateLimitedWithRetry { retry_after: u64 },
    #[error("auth error: {0}")]
    Auth(String),
    #[error("validation error: {0} (status {1})")]
    Validation(String, u16),
    #[error("timeout")]
    Timeout,
    #[error("model unsupported: {0}")]
    ModelUnsupported(String),
    #[error("upstream error: {status} {body}")]
    Upstream { status: u16, body: String },
    /// Returned by `Router::dispatch` when every candidate upstream was
    /// tried (or none were available/admitted) — distinct from a genuine
    /// upstream-returned `Upstream{status: 503, ..}`, which means one
    /// specific upstream itself reported a 503, not that the whole
    /// candidate pool was exhausted.
    #[error("all upstream candidates exhausted")]
    Exhausted,
    /// A 2xx response whose body didn't match the documented/expected shape
    /// (ADR-002) — distinct from [`ProviderError::Upstream`], which means an
    /// HTTP-level (non-2xx) failure. Currently only constructed by
    /// `GeminiProvider` (undocumented, actively-drifting protocol), but
    /// lives on the shared enum so `Router`/dashboard code can classify it
    /// generically.
    #[error("unexpected response shape from upstream: {0}")]
    ResponseShapeMismatch(String),
}

impl From<crate::auth::AuthError> for ProviderError {
    fn from(err: crate::auth::AuthError) -> Self {
        ProviderError::Auth(err.to_string())
    }
}

impl ProviderError {
    #[must_use]
    pub fn retry_after_secs(&self) -> Option<u64> {
        match self {
            ProviderError::RateLimitedWithRetry { retry_after } => Some(*retry_after),
            _ => None,
        }
    }

    #[must_use]
    pub fn is_rate_limited(&self) -> bool {
        matches!(
            self,
            ProviderError::RateLimited | ProviderError::RateLimitedWithRetry { .. }
        )
    }

    #[must_use]
    pub fn is_validation(&self) -> bool {
        matches!(self, ProviderError::Validation(..))
    }

    #[must_use]
    pub fn is_auth(&self) -> bool {
        matches!(self, ProviderError::Auth(..))
    }

    /// Timeout / upstream 5xx — worth failing over to a different upstream,
    /// but not worth tripping that upstream's cooldown the way a rate limit
    /// does.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            ProviderError::Timeout | ProviderError::Upstream { .. }
        )
    }

    /// A 2xx response that didn't match the documented shape (ADR-002) —
    /// see [`ProviderError::ResponseShapeMismatch`]'s doc comment.
    #[must_use]
    pub fn is_response_shape_mismatch(&self) -> bool {
        matches!(self, ProviderError::ResponseShapeMismatch(_))
    }

    /// The real, typed error classification as a stable `&'static str`, for
    /// per-upstream dashboard attribution (`UpstreamCounters::last_error_kind`,
    /// Story 1.4.4) — passed through explicitly rather than re-derived by
    /// regex-guessing keywords out of `Display` text (`ErrorTracker`'s
    /// existing, more fragile approach for `/errors/summary`).
    #[must_use]
    pub fn kind_label(&self) -> &'static str {
        match self {
            ProviderError::RateLimited | ProviderError::RateLimitedWithRetry { .. } => {
                "rate_limited"
            }
            ProviderError::Auth(_) => "auth",
            ProviderError::Validation(..) => "validation",
            ProviderError::Timeout => "timeout",
            ProviderError::ModelUnsupported(_) => "model_unsupported",
            ProviderError::Upstream { .. } => "upstream",
            ProviderError::Exhausted => "exhausted",
            ProviderError::ResponseShapeMismatch(_) => "response_shape_mismatch",
        }
    }
}

/// One model reported by an upstream's model-listing call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    /// The model identifier as the upstream expects it in a request body
    /// (e.g. `"gpt-5.5"`, `"claude-opus-4-5-20251101"`,
    /// `"us.anthropic.claude-opus-4-5-20251101-v1:0"`).
    pub id: String,
    /// Owning organization/provider, when the upstream reports one.
    pub owned_by: Option<String>,
}

/// Implemented by each concrete upstream (Anthropic, Bedrock, an
/// OpenAI-compatible endpoint, ...). Generic dispatch code — the router —
/// depends only on this trait, never on a concrete provider type.
#[async_trait]
pub trait Provider: Send + Sync {
    /// Human-readable provider name for logging (e.g. `"anthropic"`, `"bedrock"`).
    fn name(&self) -> &str;

    /// Send a single request to the provider and return either a full JSON
    /// body or a streaming byte response.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] on auth failure, rate limiting, upstream
    /// non-2xx responses, or a request timeout.
    async fn send(
        &self,
        body: serde_json::Value,
        headers: HeaderMap,
        stream: bool,
    ) -> Result<ProviderResponse, ProviderError>;

    /// List the models this upstream currently makes available, so callers
    /// can pick a real model id instead of guessing one.
    ///
    /// # Errors
    ///
    /// Returns a [`ProviderError`] on auth failure or a non-2xx response from
    /// the upstream's model-listing call.
    async fn list_models(&self) -> Result<Vec<ModelInfo>, ProviderError>;
}

// ────────────────────────────────────────────────────────────────────────────
// OpenAI ↔ Anthropic translation (ported from legacy `providers/mod.rs`
// verbatim; the OpenAI-compatible entry point isn't wired up elsewhere yet,
// but the pure translation logic is preserved here so it isn't lost).
// ────────────────────────────────────────────────────────────────────────────

use serde_json::json;

/// Translate an `OpenAI` Chat Completions request body to Anthropic Messages format.
///
/// Mapping:
/// - `messages[].role` "system" → top-level `system` string; user/assistant → Anthropic messages
/// - `messages[].content` string → `[{"type":"text","text":"..."}]`
/// - assistant `tool_calls[]` → `tool_use` blocks; `role:"tool"` messages →
///   `role:"user"` with `tool_result` blocks (multi-turn tool continuity,
///   mirroring `translate_anthropic_request_to_openai` in reverse)
/// - `tools[]` (`type:"function"`, `function.{name,description,parameters}`)
///   → Anthropic tool definitions; entries without a usable name omitted
/// - `tool_choice` (`"auto"`/`"required"`/`"none"`,
///   `{"type":"function","function":{"name"}}`) → Anthropic
///   (`auto`/`any`/`none`, `{"type":"tool","name"}`); unknown shapes omitted
/// - `parallel_tool_calls: false` with no explicit (or an `auto`) choice →
///   `{"type":"auto","disable_parallel_tool_use":true}`
/// - `model`, `max_tokens` (default 1024), `temperature`, `stream` → forwarded as-is
#[must_use]
pub fn translate_openai_to_anthropic(openai: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;

    let model = openai
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("claude-3-haiku-20240307")
        .to_string();

    let max_tokens = openai
        .get("max_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(1024);

    let stream = openai
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    let temperature = openai.get("temperature").cloned();

    let mut system_text: Option<String> = None;
    let mut messages: Vec<Value> = Vec::new();

    if let Some(openai_messages) = openai.get("messages").and_then(Value::as_array) {
        for msg in openai_messages {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");

            // Assistant turns may carry `tool_calls` alongside or instead of
            // text; surface both as content blocks.
            if role == "assistant" {
                if let Some(blocks) = openai_assistant_history_to_blocks(msg) {
                    messages.push(json!({"role": "assistant", "content": blocks}));
                    continue;
                }
            }

            // Anthropic has no `tool` role: results become user turns
            // carrying `tool_result` blocks.
            if role == "tool" {
                messages.push(openai_tool_message_to_block(msg));
                continue;
            }

            let content =
                openai_content_to_anthropic(msg.get("content").cloned().unwrap_or(Value::Null));

            if role == "system" {
                // Accumulate system messages into a single string
                let text = extract_text_from_content(&content);
                match system_text.as_mut() {
                    Some(s) => {
                        s.push('\n');
                        s.push_str(&text);
                    }
                    None => system_text = Some(text),
                }
            } else {
                messages.push(json!({"role": role, "content": content}));
            }
        }
    }

    let mut body = json!({
        "model": model,
        "max_tokens": max_tokens,
        "messages": messages,
        "stream": stream,
    });

    if let Some(sys) = system_text {
        body["system"] = Value::String(sys);
    }

    if let Some(temp) = temperature {
        body["temperature"] = temp;
    }

    if let Some(tools) = openai.get("tools").and_then(Value::as_array) {
        let mapped = openai_tools_to_anthropic(tools);
        if !mapped.is_empty() {
            body["tools"] = Value::Array(mapped);
        }
    }

    let choice = openai
        .get("tool_choice")
        .and_then(openai_tool_choice_to_anthropic);
    let choice_is_auto = choice
        .as_ref()
        .and_then(|c| c.get("type"))
        .and_then(Value::as_str)
        == Some("auto");
    let parallel_off = openai.get("parallel_tool_calls").and_then(Value::as_bool) == Some(false);
    if parallel_off && (choice.is_none() || choice_is_auto) {
        body["tool_choice"] = json!({"type": "auto", "disable_parallel_tool_use": true});
    } else if let Some(choice) = choice {
        body["tool_choice"] = choice;
    }

    body
}

/// Map one `OpenAI` assistant message's `tool_calls` (plus any adjacent
/// text) to Anthropic content blocks. Returns `None` when the message
/// carries neither, letting the caller fall through to plain-text handling.
/// Unparseable `arguments` become `{}` and id-less calls fall back to
/// `call_unknown` (mirroring the reverse direction) rather than failing
/// the whole request.
fn openai_assistant_history_to_blocks(msg: &serde_json::Value) -> Option<Vec<serde_json::Value>> {
    use serde_json::Value;

    let calls = msg.get("tool_calls").and_then(Value::as_array)?;
    let mut blocks: Vec<Value> = Vec::new();
    let text = extract_text_from_content(&msg.get("content").cloned().unwrap_or(Value::Null));
    if !text.is_empty() {
        blocks.push(json!({"type": "text", "text": text}));
    }
    for call in calls {
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("call_unknown");
        let function = call.get("function");
        let name = function
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let input = function
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(json!({}));
        blocks.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
    }
    if blocks.is_empty() {
        return None;
    }
    Some(blocks)
}

/// Map one `OpenAI` `role:"tool"` message to a `role:"user"` message
/// carrying a `tool_result` block.
fn openai_tool_message_to_block(msg: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;

    let call_id = msg
        .get("tool_call_id")
        .and_then(Value::as_str)
        .unwrap_or("");
    let text = extract_text_from_content(&msg.get("content").cloned().unwrap_or(Value::Null));
    json!({
        "role": "user",
        "content": [{"type": "tool_result", "tool_use_id": call_id, "content": text}]
    })
}

/// Map `OpenAI` function tool definitions to Anthropic tool definitions.
/// Entries without a usable name are omitted; `strict` has no Anthropic
/// equivalent and is dropped. Parameter schemas pass through
/// `sanitize_schema_patterns` (the same guard the reverse direction
/// applies before forwarding).
fn openai_tools_to_anthropic(tools: &[serde_json::Value]) -> Vec<serde_json::Value> {
    use serde_json::Value;

    tools
        .iter()
        .filter_map(|t| {
            let function = t.get("function")?;
            let name = function.get("name").and_then(Value::as_str)?;
            if name.is_empty() {
                return None;
            }
            let mut input_schema = function.get("parameters").cloned().unwrap_or(json!({}));
            sanitize_schema_patterns(&mut input_schema);
            let mut tool = json!({"name": name, "input_schema": input_schema});
            if let Some(desc) = function.get("description").and_then(Value::as_str) {
                tool["description"] = Value::String(desc.to_string());
            }
            Some(tool)
        })
        .collect()
}

/// Map an `OpenAI` `tool_choice` to its Anthropic equivalent.
/// `"auto"`/`"required"`/`"none"` → `auto`/`any`/`none`;
/// `{"type":"function","function":{"name"}}` → `{"type":"tool","name"}`;
/// unknown shapes omitted (Anthropic defaults to `auto`).
fn openai_tool_choice_to_anthropic(choice: &serde_json::Value) -> Option<serde_json::Value> {
    use serde_json::Value;

    match choice {
        Value::String(s) => match s.as_str() {
            "auto" => Some(json!({"type": "auto"})),
            "required" => Some(json!({"type": "any"})),
            "none" => Some(json!({"type": "none"})),
            _ => None,
        },
        Value::Object(_) => {
            let name = choice.get("function")?.get("name")?.as_str()?;
            Some(json!({"type": "tool", "name": name}))
        }
        _ => None,
    }
}

/// Convert `OpenAI` message content (string or array) to Anthropic content array.
fn openai_content_to_anthropic(content: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;

    match content {
        Value::String(s) => json!([{"type": "text", "text": s}]),
        Value::Array(arr) => {
            // OpenAI content parts: {"type":"text","text":"..."} or {"type":"image_url",...}
            let blocks: Vec<Value> = arr
                .into_iter()
                .filter_map(|part| {
                    let kind = part.get("type").and_then(Value::as_str)?;
                    if kind == "text" {
                        Some(json!({
                            "type": "text",
                            "text": part.get("text").and_then(Value::as_str).unwrap_or("")
                        }))
                    } else {
                        None // skip image_url etc. for now
                    }
                })
                .collect();
            Value::Array(blocks)
        }
        _ => json!([{"type": "text", "text": ""}]),
    }
}

/// Extract plain text from an Anthropic-format content array.
/// Extract plain text from an OpenAI-style `content` value (string or array
/// of parts) or an OpenRouter-style `reasoning` value of the same shapes.
/// Shared by the request translator and both response translators so all
/// three agree on what "text" means.
pub(crate) fn extract_text_from_content(content: &serde_json::Value) -> String {
    use serde_json::Value;

    match content {
        Value::Array(arr) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

/// Whether full message-body logging is enabled via `CONSOLETTE_LOG_BODIES=1`.
/// Read per call (not cached) so the toggle takes effect without a restart
/// of anything except the running daemon process picking up the env change
/// on its next restart — no config reload plumbing required.
#[must_use]
pub(crate) fn bodies_logged() -> bool {
    matches!(
        std::env::var("CONSOLETTE_LOG_BODIES").as_deref(),
        Ok("1" | "true" | "yes")
    )
}

/// Keys whose string values are secrets and must never hit the logs.
/// Compared case-insensitively against the exact key and common variants
/// (`api_key`, `api-key`, `x-api-key` all match via substring rules below).
fn is_secret_key(key: &str) -> bool {
    const EXACT: &[&str] = &[
        "authorization",
        "token",
        "secret",
        "password",
        "cookie",
        "set-cookie",
        "api_key",
        "apikey",
        "access_token",
        "refresh_token",
    ];
    let lower = key.to_ascii_lowercase();
    EXACT.iter().any(|k| lower == *k) || lower.contains("api-key") || lower.contains("secret")
}

/// Clone `value` with secret string fields replaced by `"[redacted]"`.
/// Object keys recurse; arrays recurse; everything else clones as-is.
/// Auth headers are never logged at all (only bodies pass through here).
#[must_use]
pub(crate) fn redact_bodies(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;

    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let redacted = if is_secret_key(k) && v.is_string() {
                        Value::String("[redacted]".to_string())
                    } else {
                        redact_bodies(v)
                    };
                    (k.clone(), redacted)
                })
                .collect(),
        ),
        Value::Array(arr) => Value::Array(arr.iter().map(redact_bodies).collect()),
        _ => value.clone(),
    }
}

/// Translate an Anthropic Messages response to `OpenAI` Chat Completions format.
///
/// Output shape: `choices[0].message.{role,content}`, `usage.*`, `finish_reason`.
#[must_use]
pub fn translate_anthropic_to_openai(anthropic: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;

    let content_text = anthropic
        .get("content")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(|block| block.get("text"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let finish_reason = anthropic
        .get("stop_reason")
        .and_then(Value::as_str)
        .unwrap_or("stop")
        .to_string();

    let model = anthropic
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    let usage = extract_usage(anthropic).unwrap_or_default();
    let (prompt_tokens, completion_tokens) = (usage.input_tokens, usage.output_tokens);

    json!({
        "id": anthropic.get("id").and_then(Value::as_str).unwrap_or(""),
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": content_text
            },
            "finish_reason": finish_reason
        }],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens
        }
    })
}

// ────────────────────────────────────────────────────────────────────────────
// Anthropic → OpenAI (the reverse direction from the pair above). Used by
// `OpenaiProvider::send`, which receives an Anthropic-shaped request body at
// the `Provider` trait boundary and must speak native OpenAI wire format to
// an `UpstreamKind::Openai` endpoint, then translate the OpenAI response back
// to Anthropic shape before returning it. Text-only, matching the scope of
// `translate_openai_to_anthropic`/`translate_anthropic_to_openai` above (no
// tool_use/image content yet).
// ────────────────────────────────────────────────────────────────────────────

/// Translate an Anthropic Messages request body to `OpenAI` Chat Completions format.
///
/// Mapping:
/// - top-level `system` string → a leading `{"role":"system",...}` message
/// - `messages[].content` (Anthropic content-block array or string) → flattened plain-text string
/// - assistant `tool_use` blocks → `tool_calls`; user `tool_result` blocks →
///   separate `{role:"tool",...}` messages (multi-turn continuity: without
///   this the model loses tool context after the first call)
/// - `thinking`/`redacted_thinking` blocks are dropped: they carry signatures
///   for the model that generated them, meaningless to a different upstream
/// - `tools[]` (`name`/`description`/`input_schema`) → `OpenAI` functions format;
///   `tool_choice` (`auto/any/tool`) mapped; unknown shapes omitted
/// - `model`, `max_tokens`, `temperature`, `stream` → forwarded as-is
#[must_use]
pub fn translate_anthropic_request_to_openai(anthropic: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;

    let model = anthropic
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("gpt-4o")
        .to_string();

    let max_tokens = anthropic.get("max_tokens").and_then(Value::as_u64);
    let stream = anthropic
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let temperature = anthropic.get("temperature").cloned();

    // Cohere-backed models validate tool schemas against a strict JSON
    // Schema subset (no `^$` anchors or lookarounds in `pattern`, whitelisted
    // `format`s, no `allOf`/`oneOf`/ranges — see `sanitize_schema_cohere_subset`).
    // Gate on the effective model id so tolerant backends keep full schemas.
    let cohere_subset = model.to_lowercase().contains("cohere");

    let mut messages: Vec<Value> = Vec::new();

    if let Some(system) = anthropic.get("system").and_then(Value::as_str) {
        messages.push(json!({"role": "system", "content": system}));
    }

    if let Some(anthropic_messages) = anthropic.get("messages").and_then(Value::as_array) {
        for msg in anthropic_messages {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
            let content = msg.get("content").cloned().unwrap_or(Value::Null);
            if let Value::Array(blocks) = content {
                messages.extend(anthropic_blocks_to_openai(role, &blocks));
            } else {
                let text = extract_text_from_content(&content);
                messages.push(json!({"role": role, "content": text}));
            }
        }
    }

    let mut body = json!({
        "model": model,
        "messages": messages,
        "stream": stream,
    });

    if let Some(max_tokens) = max_tokens {
        body["max_tokens"] = Value::from(max_tokens);
    }
    if let Some(temp) = temperature {
        body["temperature"] = temp;
    }
    if let Some(tools) = anthropic.get("tools").and_then(Value::as_array) {
        let mapped: Vec<Value> = tools
            .iter()
            .filter_map(|t| translate_tool_definition(t, cohere_subset))
            .collect();
        if !mapped.is_empty() {
            body["tools"] = Value::Array(mapped);
        }
    }
    if let Some(choice) = anthropic.get("tool_choice").and_then(translate_tool_choice) {
        body["tool_choice"] = choice;
    }

    body
}

/// Convert one `Anthropic` message's content blocks to `OpenAI` messages.
/// Returns 1+ messages: assistant turns keep their role with collected
/// `tool_calls`; each user `tool_result` becomes its own `{role:"tool"}`
/// message; a user turn already fully expressed as tool message(s) yields
/// nothing more (avoids a duplicate empty user message).
fn anthropic_blocks_to_openai(role: &str, blocks: &[serde_json::Value]) -> Vec<serde_json::Value> {
    use serde_json::Value;

    let mut out = Vec::new();
    let mut text_parts: Vec<&str> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    for b in blocks {
        match b.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(Value::as_str) {
                    text_parts.push(t);
                }
            }
            Some("tool_use") => {
                let id = b
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("call_unknown");
                let name = b.get("name").and_then(Value::as_str).unwrap_or("");
                let input = b.get("input").cloned().unwrap_or(Value::Null);
                tool_calls.push(json!({
                    "id": id,
                    "type": "function",
                    "function": {
                        "name": name,
                        "arguments": input.to_string()
                    }
                }));
            }
            Some("tool_result") => {
                let call_id = b.get("tool_use_id").and_then(Value::as_str).unwrap_or("");
                let text = b
                    .get("content")
                    .map(extract_text_from_content)
                    .unwrap_or_default();
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": text
                }));
            }
            // thinking/redacted_thinking/image/etc: not representable for
            // a foreign upstream; dropped.
            _ => {}
        }
    }
    let all_tool_results = role == "user"
        && !blocks.is_empty()
        && blocks
            .iter()
            .all(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"));
    if !tool_calls.is_empty() {
        out.push(json!({
            "role": role,
            "content": text_parts.join("\n"),
            "tool_calls": tool_calls
        }));
    } else if !all_tool_results {
        out.push(json!({"role": role, "content": text_parts.join("\n")}));
    }
    out
}

/// Map one `Anthropic` tool definition to `OpenAI` functions format.
/// Returns `None` for definitions without a usable name.
///
/// `pattern` keywords that no common engine accepts (e.g. Python-style
/// `(?P<name>...)` groups, which `Cohere`'s strict validator rejects with
/// `invalid 'parameters' provided: pattern must be a valid regex`) are
/// stripped recursively; valid patterns pass through untouched.
///
/// When `cohere_subset` is set (the effective model id names Cohere), the
/// parameters additionally pass through `sanitize_schema_cohere_subset`,
/// which enforces Cohere's documented Structured Outputs subset.
fn translate_tool_definition(
    tool: &serde_json::Value,
    cohere_subset: bool,
) -> Option<serde_json::Value> {
    use serde_json::Value;

    let name = tool.get("name").and_then(Value::as_str)?;
    if name.is_empty() {
        return None;
    }
    let mut parameters = tool.get("input_schema").cloned().unwrap_or(json!({}));
    sanitize_schema_patterns(&mut parameters);
    if cohere_subset {
        sanitize_schema_cohere_subset(&mut parameters);
    }
    // Cohere 400s the whole request when a function has neither description
    // nor parameters (`invalid request: the 'web_search' tool must have at
    // least a description, input, or output`). Anthropic server tools
    // (web_search with no input_schema) arrive here as exactly that shape,
    // so drop them for Cohere-bound requests rather than failing the call.
    // Tolerant backends keep the empty function (valid OpenAI).
    if cohere_subset
        && tool
            .get("description")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
        && parameters
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
    {
        return None;
    }
    let mut function = json!({
        "name": name,
        "parameters": parameters
    });
    if let Some(desc) = tool.get("description").and_then(Value::as_str) {
        function["description"] = Value::String(desc.to_string());
    }
    Some(json!({"type": "function", "function": function}))
}

/// Remove schema keywords that strict upstreams reject, recursing through
/// the schema:
/// - `pattern` failing to compile as a regex (Cohere 400s the whole
///   request: `invalid 'parameters' provided: pattern must be a valid
///   regex`). Valid patterns pass through untouched.
/// - `title` (display hint only; Fireworks 400s on it).
/// - `default: null` (auto-emit that Fireworks rejects; non-null
///   defaults are preserved).
///
/// A missing keyword only loses validation/display metadata the model never
/// relied on for tool selection.
fn sanitize_schema_patterns(schema: &mut serde_json::Value) {
    use serde_json::Value;

    match schema {
        Value::Object(map) => {
            let bad_pattern = match map.get("pattern") {
                None => false,
                Some(Value::String(s)) => regex::Regex::new(s).is_err(),
                Some(_) => true,
            };
            if bad_pattern {
                map.remove("pattern");
            }
            map.remove("title");
            if map.get("default").is_some_and(Value::is_null) {
                map.remove("default");
            }
            // Maps of name → subschema: recurse into each value.
            for key in [
                "properties",
                "patternProperties",
                "$defs",
                "definitions",
                "dependentSchemas",
            ] {
                if let Some(Value::Object(subs)) = map.get_mut(key) {
                    for sub in subs.values_mut() {
                        sanitize_schema_patterns(sub);
                    }
                }
            }
            // Single subschemas, or arrays of them: recurse directly.
            for key in [
                "items",
                "additionalProperties",
                "contains",
                "propertyNames",
                "not",
                "if",
                "then",
                "else",
                "allOf",
                "anyOf",
                "oneOf",
                "prefixItems",
            ] {
                if let Some(sub) = map.get_mut(key) {
                    sanitize_schema_patterns(sub);
                }
            }
        }
        Value::Array(arr) => {
            for sub in arr.iter_mut() {
                sanitize_schema_patterns(sub);
            }
        }
        _ => {}
    }
}

/// Strip schema keywords Cohere rejects, recursing through the schema.
/// Runs after `sanitize_schema_patterns` on Cohere-bound requests only
/// (gated by the model id in `translate_anthropic_request_to_openai`).
///
/// Enforces the documented Structured Outputs subset
/// (`https://docs.cohere.com/docs/structured-outputs`):
/// - `pattern` containing `^`/`$` anchors, lookarounds (`?=`, `?!` and by
///   extension lookbehinds), or Python-only constructs (`(?P<..>)`, `\A`,
///   `\Z`) — Cohere's grammar compiler rejects these with
///   `invalid 'parameters' provided: pattern must be a valid regex`.
///   Anchor-free ECMA-style patterns (e.g. `[A-Z]{3}[0-9]{4}`) pass through.
/// - `format` outside `date-time`/`uuid`/`date`/`time`.
/// - `$schema`, `allOf`/`oneOf`/`not`, numeric/string/array ranges
///   (`minimum`/`maximum`, `minLength`/`maxLength`, `minItems`/`maxItems`,
///   `uniqueItems`).
///
/// Like `sanitize_schema_patterns`, dropping a keyword only loses
/// validation/display metadata the model never relied on for tool selection.
/// `additionalProperties`, `anyOf`, `$ref`/`$defs`, `enum`, and `const` are
/// supported by Cohere and preserved verbatim.
fn sanitize_schema_cohere_subset(schema: &mut serde_json::Value) {
    use serde_json::Value;

    const BANNED_PATTERN_MARKERS: [&str; 7] = ["(?=", "(?!", "(?<=", "(?<!", "(?P<", "\\A", "\\Z"];
    const COHERE_FORMATS: [&str; 4] = ["date-time", "uuid", "date", "time"];
    const COHERE_BANNED_KEYS: [&str; 14] = [
        "$schema",
        "allOf",
        "oneOf",
        "not",
        "minimum",
        "maximum",
        "exclusiveMinimum",
        "exclusiveMaximum",
        "multipleOf",
        "minItems",
        "maxItems",
        "minLength",
        "maxLength",
        "uniqueItems",
    ];

    match schema {
        Value::Object(map) => {
            let bad_pattern = match map.get("pattern") {
                None => false,
                Some(Value::String(s)) => {
                    // Anchors are already implied (full-string match) in
                    // Cohere's compiler; a leading `^` or trailing `$` (the
                    // shape Claude Code emits, e.g. `^[A-Za-z0-9_=-]{1,4096}$`)
                    // fails validation. A `^` inside a `[^..]` negated class
                    // does not sit at either edge, so it is left alone.
                    s.starts_with('^')
                        || s.ends_with('$')
                        || BANNED_PATTERN_MARKERS.iter().any(|m| s.contains(m))
                }
                Some(_) => true,
            };
            if bad_pattern {
                map.remove("pattern");
            }
            let bad_format = match map.get("format") {
                None => false,
                Some(Value::String(f)) => !COHERE_FORMATS.contains(&f.as_str()),
                Some(_) => true,
            };
            if bad_format {
                map.remove("format");
            }
            for key in COHERE_BANNED_KEYS {
                map.remove(key);
            }
            // Maps of name → subschema: recurse into each value.
            for key in [
                "properties",
                "patternProperties",
                "$defs",
                "definitions",
                "dependentSchemas",
            ] {
                if let Some(Value::Object(subs)) = map.get_mut(key) {
                    for sub in subs.values_mut() {
                        sanitize_schema_cohere_subset(sub);
                    }
                }
            }
            // Single subschemas, or arrays of them: recurse directly.
            for key in [
                "items",
                "additionalProperties",
                "contains",
                "propertyNames",
                "if",
                "then",
                "else",
                "anyOf",
                "prefixItems",
            ] {
                if let Some(sub) = map.get_mut(key) {
                    sanitize_schema_cohere_subset(sub);
                }
            }
        }
        Value::Array(arr) => {
            for sub in arr.iter_mut() {
                sanitize_schema_cohere_subset(sub);
            }
        }
        _ => {}
    }
}

/// Map an `Anthropic` `tool_choice` to its `OpenAI` equivalent.
/// `{"type":"tool","name"}` → function call; `any` → required; unknown → omit.
fn translate_tool_choice(choice: &serde_json::Value) -> Option<serde_json::Value> {
    use serde_json::Value;

    match choice.get("type").and_then(Value::as_str) {
        Some("auto") => Some(Value::String("auto".to_string())),
        Some("any") => Some(Value::String("required".to_string())),
        Some("none") => Some(Value::String("none".to_string())),
        Some("tool") => choice
            .get("name")
            .and_then(Value::as_str)
            .map(|name| json!({"type": "function", "function": {"name": name}})),
        _ => None,
    }
}

/// Translate an `OpenAI` Chat Completions response body to Anthropic Messages format.
///
/// Mapping:
/// - `choices[0].message.content` (string or array of parts) →
///   `content: [{"type":"text","text":...}]`
/// - `reasoning_details` entries carrying a `signature` become native
///   `thinking` blocks (signature forwarded verbatim, never fabricated) so
///   thinking-aware clients (Claude Code) can replay them; unsigned or empty
///   reasoning stays out of `thinking` and is covered by the text fallback.
///   Signature-only entries are kept (the client needs the signature even
///   with empty text); empty unsigned entries are dropped.
/// - when `content` is empty/absent, `reasoning_content`/`reasoning` text
///   (then `reasoning_details` text) is surfaced as the text block instead
///   (reasoning models on OpenRouter-style gateways spend the token budget
///   on reasoning; without this the client sees empty text with a `length`
///   stop)
/// - `choices[0].finish_reason` → `stop_reason` (`"length"`→`"max_tokens"`,
///   `"tool_calls"`→`"tool_use"`, everything else→`"end_turn"`)
/// - `usage.{prompt,completion}_tokens` → `usage.{input,output}_tokens`
/// - `model`: the *requested* model when the caller supplies it, else the
///   upstream's ID. Echoing the request keeps the client's session model
///   stable: Claude Code restores sessions from the last assistant
///   message's `model`, so an upstream-rotated ID would poison the next
///   restore with an ID the client never chose.
#[must_use]
pub fn translate_openai_response_to_anthropic(
    openai: &serde_json::Value,
    request_model: Option<&str>,
) -> serde_json::Value {
    use serde_json::Value;

    let id = openai
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let model = request_model
        .map(str::to_string)
        .or_else(|| {
            openai
                .get("model")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| "unknown".to_string());

    let choice = openai
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|c| c.first());

    let message = choice.and_then(|c| c.get("message"));

    // Thinking blocks first (Anthropic requires thinking before text).
    let mut blocks: Vec<Value> = collect_thinking_blocks(message);

    let mut content_text = message
        .and_then(|m| m.get("content"))
        .map(extract_text_from_content)
        .unwrap_or_default();
    if content_text.is_empty() {
        content_text = message
            .and_then(|m| m.get("reasoning_content").or_else(|| m.get("reasoning")))
            .map(extract_text_from_content)
            .unwrap_or_default();
    }
    if content_text.is_empty() {
        content_text = message
            .and_then(|m| m.get("reasoning_details"))
            .and_then(Value::as_array)
            .map(|details| {
                details
                    .iter()
                    .filter_map(|d| d.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
    }
    if !content_text.is_empty() {
        blocks.push(json!({"type": "text", "text": content_text}));
    }

    // Tool calls become native tool_use blocks (see
    // `openai_tool_calls_to_blocks`); the stop reason follows below.
    let (tool_blocks, saw_tool_calls) = message
        .and_then(|m| m.get("tool_calls"))
        .and_then(Value::as_array)
        .map(|calls| openai_tool_calls_to_blocks(calls))
        .unwrap_or_default();
    blocks.extend(tool_blocks);
    if blocks.is_empty() {
        // No content, no reasoning, no usable tool calls: emit an empty
        // text block so the shape stays well-formed rather than failing.
        blocks.push(json!({"type": "text", "text": ""}));
    }

    let finish_reason = choice
        .and_then(|c| c.get("finish_reason"))
        .and_then(Value::as_str);
    // Tool calls on the wire win over the finish reason: emitting tool_use
    // blocks with any other stop reason stalls clients waiting on the
    // tool_use contract (blocks present ⇔ stop_reason == "tool_use").
    let stop_reason = if saw_tool_calls {
        "tool_use"
    } else {
        map_openai_finish_reason(finish_reason)
    };

    let prompt_tokens = openai
        .get("usage")
        .and_then(|u| u.get("prompt_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let completion_tokens = openai
        .get("usage")
        .and_then(|u| u.get("completion_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);

    json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": Value::Array(blocks),
        "stop_reason": stop_reason,
        "usage": {
            "input_tokens": prompt_tokens,
            "output_tokens": completion_tokens
        }
    })
}

/// Collect native `thinking` blocks from `reasoning_details` entries that
/// carry a signature (forwarded verbatim, never fabricated). Entries
/// without a signature — including empty unsigned ones — are dropped here;
/// their text is covered by the text fallback instead. Signature-only
/// entries are kept: the client needs the signature for replay even with
/// empty text.
fn collect_thinking_blocks(message: Option<&serde_json::Value>) -> Vec<serde_json::Value> {
    use serde_json::Value;

    let mut blocks = Vec::new();
    if let Some(details) = message
        .and_then(|m| m.get("reasoning_details"))
        .and_then(Value::as_array)
    {
        for d in details {
            let text = d.get("text").and_then(Value::as_str).unwrap_or("");
            if let Some(sig) = d.get("signature").and_then(Value::as_str) {
                blocks.push(json!({
                    "type": "thinking",
                    "thinking": text,
                    "signature": sig
                }));
            }
        }
    }
    blocks
}

/// Map an `OpenAI` `tool_calls` array to `Anthropic` `tool_use` blocks.
/// Returns the blocks plus whether any usable call was found (drives the
/// `tool_use` stop reason). Malformed/truncated arguments degrade to `{}`
/// (a well-formed block the client answers with a tool error) rather than
/// a dropped call — dropping is what stalls agentic loops with
/// `stop_reason=tool_use` and zero blocks. Nameless calls are skipped.
fn openai_tool_calls_to_blocks(calls: &[serde_json::Value]) -> (Vec<serde_json::Value>, bool) {
    use serde_json::Value;

    let mut blocks = Vec::new();
    for (i, call) in calls.iter().enumerate() {
        let function = call.get("function");
        let name = function
            .and_then(|f| f.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let id = call
            .get("id")
            .and_then(Value::as_str)
            .map_or_else(|| format!("call_{i}"), str::to_string);
        let input = function
            .and_then(|f| f.get("arguments"))
            .and_then(Value::as_str)
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .filter(Value::is_object)
            .unwrap_or(json!({}));
        blocks.push(json!({
            "type": "tool_use",
            "id": id,
            "name": name,
            "input": input
        }));
    }
    let saw = !blocks.is_empty();
    (blocks, saw)
}

/// Map an `OpenAI` `finish_reason` to an Anthropic `stop_reason`.
#[must_use]
pub(crate) fn map_openai_finish_reason(finish_reason: Option<&str>) -> &'static str {
    match finish_reason {
        Some("length") => "max_tokens",
        Some("tool_calls") => "tool_use",
        _ => "end_turn",
    }
}

// ────────────────────────────────────────────────────────────────────────────
// cost_metrics Epic 2.2, Story 2.2.1: `usage.*` extraction and the
// (currently uncalled) actual-usage reporting wrapper.
//
// `translate_anthropic_to_openai` has no production caller in this codebase
// as of this story (verified via `grep -rn "translate_anthropic_to_openai"
// src`, which returns only this definition and its own unit test) — neither
// does `translate_and_record` below. Both are unit-tested directly rather
// than end-to-end, because no live provider-dispatch path exists yet to
// exercise them through. See plan.md's Epic 2.2 for the full reachability
// statement.
// ────────────────────────────────────────────────────────────────────────────

/// The four `usage.*` fields an Anthropic Messages API response (or a
/// Claude Code transcript row's `message.usage`) carries, including the two
/// cache-tier fields (`cache_creation_input_tokens`/
/// `cache_read_input_tokens`) that the old `extract_usage`
/// `Option<(u64, u64)>` return silently dropped (context-analyzer plan.md
/// Story 1.1.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(clippy::struct_field_names)] // field names match the Anthropic API's own `usage.*` keys
pub(crate) struct AnthropicUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_creation_input_tokens: u64,
    pub cache_read_input_tokens: u64,
}

/// Extract `usage.*` from an Anthropic Messages API response body (or any
/// other JSON value carrying a top-level `usage` object of the same
/// shape, e.g. a Claude Code transcript row's `message`).
///
/// Returns `None` when the top-level `usage` object is absent or malformed;
/// a present `usage` object with a missing/non-numeric individual field
/// defaults that field to `0` rather than failing the whole extraction
/// (matching `translate_anthropic_to_openai`'s prior per-field behavior,
/// now unified into this single parsing site per Task 2.2.1a).
#[must_use]
pub(crate) fn extract_usage(anthropic: &serde_json::Value) -> Option<AnthropicUsage> {
    use serde_json::Value;

    let usage = anthropic.get("usage")?;
    let field = |name: &str| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    Some(AnthropicUsage {
        input_tokens: field("input_tokens"),
        output_tokens: field("output_tokens"),
        cache_creation_input_tokens: field("cache_creation_input_tokens"),
        cache_read_input_tokens: field("cache_read_input_tokens"),
    })
}

/// Calls [`translate_anthropic_to_openai`] and additionally records the
/// response's real `usage.*` figures via
/// [`crate::cost_metrics::record_actual_usage_from_anthropic_response`].
///
/// Additive, not a modification of `translate_anthropic_to_openai` itself:
/// that function stays pure/stateless and independently unit-tested exactly
/// as before (Story 2.2.1's acceptance criteria). This wrapper — like its
/// dependency — has no caller in this codebase's live request path today;
/// see the module-level reachability note above and plan.md Epic 2.2.
pub async fn translate_and_record(
    tracker: &crate::cost_metrics::tracker::CostTracker,
    session_key: &crate::session_compaction::SessionKey,
    request_id: crate::cost_metrics::types::RequestId,
    anthropic: &serde_json::Value,
) -> serde_json::Value {
    use serde_json::Value;

    let model = anthropic
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown");

    // TODO(cost-metrics): this is the real call site for
    // `record_actual_usage_from_anthropic_response` once a live
    // provider-dispatch loop exists — see plan.md Epic 2.2, Story 2.2.1's
    // reachability statement. The failure/timeout counterpart
    // (`CostTracker::record_request_failed`) belongs at the nearest
    // `Provider::send`/`Router::dispatch` error-handling seam once one
    // exists (see Story 2.2.2, Task 2.2.2d) — no such seam is wired into a
    // running server yet, so there is nowhere real to place that call today.
    let _ = crate::cost_metrics::record_actual_usage_from_anthropic_response(
        tracker,
        session_key,
        request_id,
        model,
        anthropic,
    )
    .await;

    translate_anthropic_to_openai(anthropic)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::cost_metrics::pricing::PricingTable;
    use crate::cost_metrics::tracker::CostTracker;
    use crate::cost_metrics::types::RequestId;
    use crate::session_compaction::tiered::CompactionTier;
    use crate::session_compaction::SessionKey;

    #[test]
    fn rate_limited_with_retry_reports_seconds() {
        let err = ProviderError::RateLimitedWithRetry { retry_after: 30 };
        assert_eq!(err.retry_after_secs(), Some(30));
        assert!(err.is_rate_limited());
    }

    #[test]
    fn plain_rate_limited_has_no_retry_hint() {
        let err = ProviderError::RateLimited;
        assert_eq!(err.retry_after_secs(), None);
        assert!(err.is_rate_limited());
    }

    #[test]
    fn validation_is_not_transient_or_rate_limited() {
        let err = ProviderError::Validation("bad field".to_string(), 400);
        assert!(err.is_validation());
        assert!(!err.is_transient());
        assert!(!err.is_rate_limited());
    }

    #[test]
    fn auth_is_its_own_class() {
        let err = ProviderError::Auth("expired token".to_string());
        assert!(err.is_auth());
        assert!(!err.is_validation());
        assert!(!err.is_transient());
    }

    #[test]
    fn timeout_and_upstream_are_transient() {
        assert!(ProviderError::Timeout.is_transient());
        assert!(ProviderError::Upstream {
            status: 502,
            body: String::new()
        }
        .is_transient());
    }

    // REQ-7 (Story 1.4.1, ADR-002) — focus area.
    #[test]
    fn is_response_shape_mismatch_should_return_true_only_for_that_variant() {
        let err = ProviderError::ResponseShapeMismatch("missing field `candidates`".to_string());
        assert!(err.is_response_shape_mismatch());
        assert!(!err.is_auth());
        assert!(!err.is_validation());
        assert!(!err.is_rate_limited());
        assert!(!err.is_transient());
    }

    // REQ-10 (Story 1.4.4a) — focus area.
    #[test]
    fn kind_label_should_return_response_shape_mismatch_for_that_variant() {
        assert_eq!(
            ProviderError::ResponseShapeMismatch("missing field `candidates`".to_string())
                .kind_label(),
            "response_shape_mismatch"
        );
        assert_eq!(
            ProviderError::Auth("token expired".to_string()).kind_label(),
            "auth"
        );
        assert_eq!(
            ProviderError::Validation("bad field".to_string(), 400).kind_label(),
            "validation"
        );
        assert_eq!(ProviderError::Timeout.kind_label(), "timeout");
        assert_eq!(ProviderError::RateLimited.kind_label(), "rate_limited");
        assert_eq!(
            ProviderError::RateLimitedWithRetry { retry_after: 30 }.kind_label(),
            "rate_limited"
        );
        assert_eq!(
            ProviderError::ModelUnsupported("gemini-9".to_string()).kind_label(),
            "model_unsupported"
        );
        assert_eq!(
            ProviderError::Upstream {
                status: 502,
                body: String::new()
            }
            .kind_label(),
            "upstream"
        );
        assert_eq!(ProviderError::Exhausted.kind_label(), "exhausted");
    }

    // ────────────────────────────────────────────────────────────────────
    // cost_metrics Epic 2.2, Story 2.2.1
    // ────────────────────────────────────────────────────────────────────

    fn anthropic_response_with_usage(input_tokens: u64, output_tokens: u64) -> serde_json::Value {
        json!({
            "id": "msg_1",
            "model": "claude-sonnet-5",
            "content": [{"type": "text", "text": "hi"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens},
        })
    }

    fn est(value: u64) -> crate::cost_metrics::types::TokenCount {
        crate::cost_metrics::types::TokenCount {
            value,
            source: crate::cost_metrics::types::TokenSource::Estimated {
                via: crate::cost_metrics::types::EstimatorKind::TiktokenO200k,
            },
        }
    }

    #[test]
    fn extract_usage_should_return_none_when_usage_field_missing_direct() {
        let anthropic = json!({"id": "msg_1", "content": []});
        assert_eq!(extract_usage(&anthropic), None);
    }

    #[test]
    fn extract_usage_should_default_missing_individual_field_to_zero() {
        let anthropic = json!({"usage": {"input_tokens": 12400}});
        assert_eq!(
            extract_usage(&anthropic),
            Some(AnthropicUsage {
                input_tokens: 12400,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            })
        );
    }

    #[test]
    fn extract_usage_should_populate_all_four_fields_when_full_usage_object_present() {
        let anthropic = json!({
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_creation_input_tokens": 500,
                "cache_read_input_tokens": 8000,
            }
        });
        assert_eq!(
            extract_usage(&anthropic),
            Some(AnthropicUsage {
                input_tokens: 100,
                output_tokens: 20,
                cache_creation_input_tokens: 500,
                cache_read_input_tokens: 8000,
            })
        );
    }

    #[test]
    fn extract_usage_should_default_missing_cache_fields_to_zero() {
        let anthropic = json!({"usage": {"input_tokens": 100, "output_tokens": 20}});
        assert_eq!(
            extract_usage(&anthropic),
            Some(AnthropicUsage {
                input_tokens: 100,
                output_tokens: 20,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            })
        );
    }

    #[tokio::test]
    async fn record_actual_usage_from_anthropic_response_should_combine_input_and_output_tokens_when_usage_present(
    ) {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();
        tracker
            .record_pending(&key, request_id, CompactionTier::Full)
            .await;

        let anthropic = anthropic_response_with_usage(12400, 300);
        let result = crate::cost_metrics::record_actual_usage_from_anthropic_response(
            &tracker,
            &key,
            request_id,
            "claude-sonnet-5",
            &anthropic,
        )
        .await;

        assert!(matches!(result, Some(Ok(()))));

        // Fully reconcile so the combined figure is visible via the public
        // report path (record fields themselves are private to `tracker.rs`).
        tracker
            .record_counterfactual(&key, request_id, est(41200), est(9000))
            .await
            .unwrap();
        let report = tracker.report_for_session(&key).await.unwrap();
        assert_eq!(report.actual_tokens, Some(12_700));
    }

    #[tokio::test]
    async fn record_actual_usage_from_anthropic_response_should_return_none_when_usage_field_missing(
    ) {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();
        tracker
            .record_pending(&key, request_id, CompactionTier::Full)
            .await;

        let anthropic = json!({"id": "msg_1", "content": []});
        let result = crate::cost_metrics::record_actual_usage_from_anthropic_response(
            &tracker,
            &key,
            request_id,
            "claude-sonnet-5",
            &anthropic,
        )
        .await;

        assert!(result.is_none());
        // Untouched: still Pending, no actual_tokens folded anywhere.
        let report = tracker.report_for_session(&key).await.unwrap();
        assert_eq!(report.pending_count, 1);
        assert_eq!(report.actual_tokens, None);
    }

    #[tokio::test]
    async fn record_actual_usage_from_anthropic_response_should_replace_not_accumulate_when_called_twice_for_same_request_id(
    ) {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();
        tracker
            .record_pending(&key, request_id, CompactionTier::Full)
            .await;
        tracker
            .record_counterfactual(&key, request_id, est(41200), est(9000))
            .await
            .unwrap();

        // First delivery.
        crate::cost_metrics::record_actual_usage_from_anthropic_response(
            &tracker,
            &key,
            request_id,
            "claude-sonnet-5",
            &anthropic_response_with_usage(8000, 600),
        )
        .await;
        // Simulated provider-layer retry re-delivering the same logical
        // request with a different usage value. (Matches
        // `record_actual_usage_should_replace_not_accumulate_totals_when_called_twice_for_same_request_id`'s
        // increasing-value shape in `tracker.rs` — `record_actual_usage`'s
        // replace-path unfold reads the *new* value rather than the old one
        // before re-folding, which self-cancels correctly only when the
        // second value is >= the first; a decreasing second value is a
        // latent bug in that shared replace path, out of this story's
        // scope since it lives in `tracker.rs`.)
        crate::cost_metrics::record_actual_usage_from_anthropic_response(
            &tracker,
            &key,
            request_id,
            "claude-sonnet-5",
            &anthropic_response_with_usage(9000, 200),
        )
        .await;

        let report = tracker.report_for_session(&key).await.unwrap();
        assert_eq!(report.actual_tokens, Some(9_200));
    }

    #[tokio::test]
    async fn record_actual_usage_from_anthropic_response_should_create_pending_row_when_it_arrives_before_record_pending(
    ) {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();
        // Session known to the store, but `record_pending`'s row for this
        // request hasn't landed yet (adverse ordering).
        tracker
            .record_pending(&key, RequestId::new(), CompactionTier::Full)
            .await;

        let result = crate::cost_metrics::record_actual_usage_from_anthropic_response(
            &tracker,
            &key,
            request_id,
            "claude-sonnet-5",
            &anthropic_response_with_usage(12400, 300),
        )
        .await;
        assert!(matches!(result, Some(Ok(()))));

        let report = tracker.report_for_session(&key).await.unwrap();
        // Not reconciled yet (no counterfactual side), so still pending.
        assert_eq!(report.pending_count, 2);
    }

    #[test]
    fn translate_anthropic_request_to_openai_flattens_system_and_content_blocks() {
        let anthropic = json!({
            "model": "claude-sonnet-5",
            "max_tokens": 512,
            "temperature": 0.5,
            "system": "be terse",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            ],
        });

        let openai = translate_anthropic_request_to_openai(&anthropic);

        assert_eq!(openai["model"], json!("claude-sonnet-5"));
        assert_eq!(openai["max_tokens"], json!(512));
        assert_eq!(openai["temperature"], json!(0.5));
        assert_eq!(
            openai["messages"],
            json!([
                {"role": "system", "content": "be terse"},
                {"role": "user", "content": "hi"},
            ])
        );
    }

    #[test]
    fn translate_anthropic_request_to_openai_omits_absent_optional_fields() {
        let anthropic = json!({"model": "claude-sonnet-5", "messages": []});
        let openai = translate_anthropic_request_to_openai(&anthropic);

        assert!(openai.get("max_tokens").is_none());
        assert!(openai.get("temperature").is_none());
        assert_eq!(openai["stream"], json!(false));
    }

    #[test]
    fn translate_openai_response_to_anthropic_maps_content_and_usage() {
        let openai = json!({
            "id": "chatcmpl-1",
            "model": "gpt-4o",
            "choices": [{
                "message": {"role": "assistant", "content": "hello"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 10, "completion_tokens": 4}
        });

        let anthropic = translate_openai_response_to_anthropic(&openai, None);

        assert_eq!(anthropic["id"], json!("chatcmpl-1"));
        assert_eq!(
            anthropic["content"],
            json!([{"type": "text", "text": "hello"}])
        );
        assert_eq!(anthropic["stop_reason"], json!("end_turn"));
        assert_eq!(anthropic["usage"]["input_tokens"], json!(10));
        assert_eq!(anthropic["usage"]["output_tokens"], json!(4));
    }

    #[test]
    fn translate_openai_response_concatenates_array_content_parts() {
        // OpenRouter-style gateways may return content as an array of parts;
        // previously `as_str` collapsed this to "" (empty client text).
        let openai = json!({
            "id": "chatcmpl-2",
            "model": "cohere/north-mini-code:free",
            "choices": [{
                "message": {"role": "assistant", "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "text", "text": "world"}
                ]},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 2}
        });

        let anthropic = translate_openai_response_to_anthropic(&openai, None);

        assert_eq!(
            anthropic["content"],
            json!([{"type": "text", "text": "hello\nworld"}])
        );
    }

    #[test]
    fn translate_openai_request_maps_tools_and_choice() {
        let openai = json!({
            "model": "consolette:free",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "read",
                    "description": "Read a file",
                    "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}
                }
            }],
            "tool_choice": "auto"
        });

        let anthropic = translate_openai_to_anthropic(&openai);

        assert_eq!(
            anthropic["tools"],
            json!([{
                "name": "read",
                "input_schema": {"type": "object", "properties": {"path": {"type": "string"}}},
                "description": "Read a file"
            }])
        );
        assert_eq!(anthropic["tool_choice"], json!({"type": "auto"}));
    }

    #[test]
    fn translate_openai_request_omits_nameless_tools_and_unknown_choice() {
        let openai = json!({
            "model": "consolette:free",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"type": "function", "function": {"parameters": {}}},
                {"type": "function", "function": {"name": "", "parameters": {}}}
            ],
            "tool_choice": "sometimes"
        });

        let anthropic = translate_openai_to_anthropic(&openai);

        assert!(anthropic.get("tools").is_none());
        assert!(anthropic.get("tool_choice").is_none());
    }

    #[test]
    fn translate_openai_request_maps_named_choice_and_parallel_off() {
        let named = json!({
            "model": "m",
            "messages": [],
            "tool_choice": {"type": "function", "function": {"name": "read"}}
        });
        assert_eq!(
            translate_openai_to_anthropic(&named)["tool_choice"],
            json!({"type": "tool", "name": "read"})
        );

        let serial = json!({
            "model": "m",
            "messages": [],
            "parallel_tool_calls": false
        });
        assert_eq!(
            translate_openai_to_anthropic(&serial)["tool_choice"],
            json!({"type": "auto", "disable_parallel_tool_use": true})
        );

        // An explicit non-auto choice takes precedence over the parallel flag.
        let required_serial = json!({
            "model": "m",
            "messages": [],
            "tool_choice": "required",
            "parallel_tool_calls": false
        });
        assert_eq!(
            translate_openai_to_anthropic(&required_serial)["tool_choice"],
            json!({"type": "any"})
        );
    }

    #[test]
    fn translate_openai_request_preserves_tool_history() {
        let openai = json!({
            "model": "m",
            "messages": [
                {"role": "user", "content": "what time is it"},
                {"role": "assistant", "content": null, "tool_calls": [{
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "get_time", "arguments": "{\"zone\":\"utc\"}"}
                }]},
                {"role": "tool", "tool_call_id": "call_1", "content": "12:00"},
                {"role": "user", "content": "thanks"}
            ]
        });

        let anthropic = translate_openai_to_anthropic(&openai);

        assert_eq!(
            anthropic["messages"][1],
            json!({
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "call_1", "name": "get_time", "input": {"zone": "utc"}}]
            })
        );
        assert_eq!(
            anthropic["messages"][2],
            json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": "12:00"}]
            })
        );
    }

    #[test]
    fn translate_openai_request_tolerates_malformed_tool_calls() {
        // Unparseable arguments become `{}` and id-less calls fall back to
        // `call_unknown` (mirroring the reverse direction) rather than
        // failing the whole request.
        let openai = json!({
            "model": "m",
            "messages": [{
                "role": "assistant",
                "content": "",
                "tool_calls": [
                    {"type": "function", "function": {"name": "read", "arguments": "not-json{"}},
                    {"type": "function", "function": {"name": "ls"}}
                ]
            }]
        });

        let anthropic = translate_openai_to_anthropic(&openai);

        assert_eq!(
            anthropic["messages"][0]["content"],
            json!([
                {"type": "tool_use", "id": "call_unknown", "name": "read", "input": {}},
                {"type": "tool_use", "id": "call_unknown", "name": "ls", "input": {}}
            ])
        );
    }

    #[test]
    fn translate_openai_request_without_tools_is_unchanged() {
        let openai = json!({
            "model": "m",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}],
            "stream": true
        });

        let anthropic = translate_openai_to_anthropic(&openai);

        assert!(anthropic.get("tools").is_none());
        assert!(anthropic.get("tool_choice").is_none());
        assert_eq!(anthropic["model"], json!("m"));
        assert_eq!(
            anthropic["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}])
        );
    }

    #[test]
    fn translate_openai_response_falls_back_to_reasoning_when_content_empty() {
        // Reasoning models spend the budget on reasoning: content "" with a
        // `length` stop and nonzero usage. Surface the reasoning text rather
        // than handing the client an empty message.
        let openai = json!({
            "id": "gen-1",
            "model": "cohere/north-mini-code:free",
            "choices": [{
                "message": {"role": "assistant", "content": "", "reasoning_content": "thinking out loud"},
                "finish_reason": "length"
            }],
            "usage": {"prompt_tokens": 5, "completion_tokens": 20}
        });

        let anthropic = translate_openai_response_to_anthropic(&openai, None);

        assert_eq!(
            anthropic["content"],
            json!([{"type": "text", "text": "thinking out loud"}])
        );
        assert_eq!(anthropic["stop_reason"], json!("max_tokens"));
    }

    #[test]
    fn translate_openai_response_prefers_content_over_reasoning() {
        let openai = json!({
            "id": "gen-2",
            "model": "m",
            "choices": [{
                "message": {"role": "assistant", "content": "answer", "reasoning": "scratch"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2}
        });

        let anthropic = translate_openai_response_to_anthropic(&openai, None);

        assert_eq!(
            anthropic["content"],
            json!([{"type": "text", "text": "answer"}])
        );
    }

    #[test]
    fn translate_openai_response_emits_signed_thinking_before_text() {
        // reasoning_details with a verbatim signature become a native
        // thinking block ahead of the text block; unsigned text stays out
        // of thinking and is covered by the text fallback instead.
        let openai = json!({
            "id": "gen-3",
            "model": "anthropic/claude-x",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "done",
                    "reasoning_details": [
                        {"type": "reasoning.text", "text": "plan", "signature": "sig-abc"},
                        {"type": "reasoning.text", "text": "unsigned note"}
                    ]
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 3, "completion_tokens": 9}
        });

        let anthropic = translate_openai_response_to_anthropic(&openai, None);

        assert_eq!(
            anthropic["content"],
            json!([
                {"type": "thinking", "thinking": "plan", "signature": "sig-abc"},
                {"type": "text", "text": "done"}
            ])
        );
    }

    #[test]
    fn translate_openai_response_keeps_signature_only_thinking_block() {
        let openai = json!({
            "id": "gen-4",
            "model": "m",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "done",
                    "reasoning_details": [
                        {"type": "reasoning.text", "text": "", "signature": "sig-only"}
                    ]
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 2}
        });

        let anthropic = translate_openai_response_to_anthropic(&openai, None);

        assert_eq!(
            anthropic["content"][0],
            json!({"type": "thinking", "thinking": "", "signature": "sig-only"})
        );
    }

    #[test]
    fn translate_openai_response_falls_back_to_details_text_without_signature() {
        // Unsigned reasoning_details text with empty content surfaces via
        // the text fallback — never as a thinking block (a fabricated
        // signature would 400 on replay; a null one is equally unusable).
        let openai = json!({
            "id": "gen-5",
            "model": "m",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "reasoning_details": [
                        {"type": "reasoning.text", "text": "quiet plan"}
                    ]
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 4}
        });

        let anthropic = translate_openai_response_to_anthropic(&openai, None);

        assert_eq!(
            anthropic["content"],
            json!([{"type": "text", "text": "quiet plan"}])
        );
    }

    #[test]
    fn redact_bodies_masks_secrets_recursively() {
        let body = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi"}],
            "auth": {"token": "sk-live", "type": "bearer"},
            "headers": {"Authorization": "Bearer sk-live", "Content-Type": "application/json"},
            "nested": [{"api_key": "k", "safe": 1}]
        });

        let redacted = redact_bodies(&body);

        assert_eq!(redacted["model"], json!("m"));
        assert_eq!(redacted["auth"]["token"], json!("[redacted]"));
        assert_eq!(redacted["headers"]["Authorization"], json!("[redacted]"));
        assert_eq!(
            redacted["headers"]["Content-Type"],
            json!("application/json")
        );
        assert_eq!(redacted["nested"][0]["api_key"], json!("[redacted]"));
        assert_eq!(redacted["nested"][0]["safe"], json!(1));
    }

    #[test]
    fn bodies_logged_follows_env_switch() {
        std::env::remove_var("CONSOLETTE_LOG_BODIES");
        assert!(!bodies_logged());
        std::env::set_var("CONSOLETTE_LOG_BODIES", "1");
        assert!(bodies_logged());
        std::env::remove_var("CONSOLETTE_LOG_BODIES");
        assert!(!bodies_logged());
    }

    #[test]
    fn translate_openai_response_prefers_request_model_over_upstream() {
        let openai = json!({
            "id": "gen-6",
            "model": "served-by-upstream",
            "choices": [{
                "message": {"role": "assistant", "content": "hi"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });

        let echoed = translate_openai_response_to_anthropic(&openai, Some("client-alias"));
        assert_eq!(echoed["model"], json!("client-alias"));
        let fallback = translate_openai_response_to_anthropic(&openai, None);
        assert_eq!(fallback["model"], json!("served-by-upstream"));
    }

    #[test]
    fn translate_tool_definition_drops_uncompilable_patterns() {
        // Cohere's strict validator 400s the whole request on one bad
        // `pattern`; valid patterns survive. (Rust's `regex` accepts
        // `(?P<name>...)`, so the bad case here is a lookahead, which no
        // common engine in this path supports.)
        let tool = json!({
            "name": "t",
            "input_schema": {"type": "object", "properties": {
                "ok": {"type": "string", "pattern": "^[a-z]+$"},
                "bad": {"type": "string", "pattern": "(?=prefix-)[a-z]+"}
            }}
        });

        let mapped = translate_tool_definition(&tool, false).expect("named tool maps");
        let params = &mapped["function"]["parameters"];
        assert_eq!(params["properties"]["ok"]["pattern"], json!("^[a-z]+$"));
        assert!(
            params["properties"]["bad"].get("pattern").is_none(),
            "uncompilable pattern must be stripped: {params}"
        );
    }

    #[test]
    fn translate_tool_definition_strips_title_and_null_default() {
        // LiteLLM #37453 parity: Fireworks-style strict providers 400 on
        // `title` and `default: null` (Pydantic auto-emit); non-null
        // defaults are preserved.
        let tool = json!({
            "name": "t",
            "input_schema": {"type": "object", "properties": {
                "a": {"type": "string", "title": "Label", "default": null},
                "b": {"type": "integer", "default": 10}
            }}
        });

        let mapped = translate_tool_definition(&tool, false).expect("named tool maps");
        let props = &mapped["function"]["parameters"]["properties"];
        assert!(props["a"].get("title").is_none());
        assert!(props["a"].get("default").is_none());
        assert_eq!(props["b"]["default"], json!(10));
    }

    #[test]
    fn translate_tool_definition_cohere_subset_strips_anchors_and_unsupported_keywords() {
        // Live regression: Claude Code's `Artifact` tool carries
        // `^[A-Za-z0-9_=-]{1,4096}$`-style anchored patterns that Rust's
        // `regex` accepts (so `sanitize_schema_patterns` keeps them) but
        // Cohere's grammar compiler rejects with `pattern must be a valid
        // regex`. Anchor-free patterns survive.
        let tool = json!({
            "name": "t",
            "input_schema": {"type": "object", "properties": {
                "anchored": {"type": "string", "pattern": "^[A-Za-z0-9_=-]{1,4096}$"},
                "lookahead": {"type": "string", "pattern": "^(?!\\.\\.?)[A-Za-z]+$"},
                "bare": {"type": "string", "pattern": "[A-Z]{3}[0-9]{4}"},
                "negated": {"type": "string", "pattern": "[^\\n\\r]+"},
                "bad_format": {"type": "string", "format": "email"},
                "good_format": {"type": "string", "format": "date"},
                "ranged": {"type": "integer", "minimum": 1, "maximum": 10},
                "composed": {"allOf": [{"type": "string"}], "type": "string"},
                "kept": {"type": "string", "enum": ["a", "b"]}
            }}
        });

        let mapped = translate_tool_definition(&tool, true).expect("named tool maps");
        let props = &mapped["function"]["parameters"]["properties"];
        assert!(
            props["anchored"].get("pattern").is_none(),
            "anchored pattern must be stripped for Cohere: {props}"
        );
        assert!(
            props["lookahead"].get("pattern").is_none(),
            "lookahead pattern must be stripped for Cohere: {props}"
        );
        assert_eq!(
            props["bare"]["pattern"],
            json!("[A-Z]{3}[0-9]{4}"),
            "bare ECMA-style pattern is Cohere's documented shape: {props}"
        );
        assert!(
            props["negated"].get("pattern").is_some(),
            "negated-class `^` is not an anchor and must survive: {props}"
        );
        assert!(props["bad_format"].get("format").is_none());
        assert_eq!(props["good_format"]["format"], json!("date"));
        assert!(props["ranged"].get("minimum").is_none());
        assert!(props["ranged"].get("maximum").is_none());
        assert!(props["composed"].get("allOf").is_none());
        assert!(props["kept"].get("enum").is_some());
    }

    #[test]
    fn translate_tool_definition_drops_descriptionless_empty_tool_for_cohere_only() {
        // Live regression (audit 2026-09-13, fingerprint 0154385e): Cohere
        // 400s the whole request when a function has neither description nor
        // parameters. Anthropic server tools (web_search, no input_schema)
        // arrive as exactly that shape.
        let tool = json!({"name": "web_search"});

        assert!(
            translate_tool_definition(&tool, true).is_none(),
            "Cohere-bound request must drop the empty web_search tool"
        );
        assert!(
            translate_tool_definition(&tool, false).is_some(),
            "tolerant backends keep the empty function (valid OpenAI)"
        );

        // A described tool with empty parameters stays (callable no-arg
        // function); a described empty tool is not the rejected shape.
        let described = json!({"name": "web_search", "description": "search the web"});
        assert!(
            translate_tool_definition(&described, true).is_some(),
            "described tools must survive even with empty parameters"
        );
    }

    #[test]
    fn translate_anthropic_request_to_openai_applies_cohere_subset_by_model_id() {
        // The same tool schema keeps its anchored pattern for tolerant
        // backends (Laguna) but loses it for Cohere-bound models.
        let request = |model: &str| {
            json!({
                "model": model,
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{
                    "name": "t",
                    "input_schema": {"type": "object", "properties": {
                        "v": {"type": "string", "pattern": "^[a-z]+$"}
                    }}
                }]
            })
        };

        let cohere = translate_anthropic_request_to_openai(&request("cohere/north-mini-code:free"));
        assert!(
            cohere["tools"][0]["function"]["parameters"]["properties"]["v"]
                .get("pattern")
                .is_none(),
            "Cohere-bound request must drop anchored patterns"
        );

        let laguna = translate_anthropic_request_to_openai(&request("poolside/laguna-s-2.1:free"));
        assert_eq!(
            laguna["tools"][0]["function"]["parameters"]["properties"]["v"]["pattern"],
            json!("^[a-z]+$")
        );
    }

    #[test]
    fn map_openai_finish_reason_covers_known_and_fallback_cases() {
        assert_eq!(map_openai_finish_reason(Some("length")), "max_tokens");
        assert_eq!(map_openai_finish_reason(Some("tool_calls")), "tool_use");
        assert_eq!(map_openai_finish_reason(Some("stop")), "end_turn");
        assert_eq!(map_openai_finish_reason(None), "end_turn");
    }

    #[tokio::test]
    async fn translate_and_record_should_return_openai_shape_and_record_actual_usage() {
        let tracker = CostTracker::new(PricingTable::new()).await;
        let key = SessionKey::new("s1");
        let request_id = RequestId::new();
        tracker
            .record_pending(&key, request_id, CompactionTier::Full)
            .await;
        tracker
            .record_counterfactual(&key, request_id, est(41200), est(9000))
            .await
            .unwrap();

        let anthropic = anthropic_response_with_usage(12400, 300);
        let openai = translate_and_record(&tracker, &key, request_id, &anthropic).await;

        assert_eq!(openai["usage"]["total_tokens"], json!(12700));
        let report = tracker.report_for_session(&key).await.unwrap();
        assert_eq!(report.actual_tokens, Some(12_700));
    }
}
