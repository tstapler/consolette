//! Anthropic <-> Gemini request/response translation (Story 1.3.1 onward).
//!
//! Two independent directions live here:
//! - `translate_anthropic_request_to_gemini`: Anthropic Messages request ->
//!   the Cloud Code Assist `CloudCodeEnvelope` wrapping a native Gemini
//!   `generateContent` request body.
//! - `translate_gemini_response_to_anthropic`: a strictly-parsed
//!   `GeminiGenerateContentResponse` -> an Anthropic Messages response
//!   `serde_json::Value`.
//!
//! Text-only for Phase 1 — `GeminiPart` gains `functionCall`/`functionResponse`
//! variants in Phase 3 (see plan.md's Domain Glossary).

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::providers::ProviderError;

use super::tools::{GeminiToolCallState, ToolUseId};

// ---------------------------------------------------------------------------
// Request-direction wire structs (Task 1.3.1a)
// ---------------------------------------------------------------------------

/// The outer Cloud Code Assist wrapper every call is sent inside — sits
/// *around* the native Gemini request (ADR-003), not inside it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CloudCodeEnvelope {
    /// The GCP project id (ADR-003) — always the configured
    /// `UpstreamKind::Gemini::project_id`, never a value from the request body.
    pub project: String,
    pub model: String,
    pub request_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
    pub request: GeminiRequest,
}

/// The native Gemini `generateContent` request body, nested inside
/// [`CloudCodeEnvelope::request`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiRequest {
    pub contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<GeminiSystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GeminiGenerationConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<GeminiTool>>,
}

/// One entry of Gemini's top-level `tools[]` array — always exactly one
/// element in practice (all of an Anthropic request's `tools[]` collapsed
/// into a single `functionDeclarations` list), matching Gemini's documented
/// shape of one `Tool` object carrying every declared function.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiTool {
    pub function_declarations: Vec<GeminiFunctionDeclaration>,
}

/// One Anthropic `tools[]` entry translated into Gemini's
/// `functionDeclarations[]` shape (Task 3.2.1c) — `input_schema` is run
/// through [`sanitize_function_schema`] before being emitted as `parameters`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct GeminiFunctionDeclaration {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub parameters: Value,
}

/// A single Gemini `contents[]` entry — the Gemini analog of an Anthropic
/// `messages[]` entry. Role vocabulary differs (`"model"`, not `"assistant"`).
///
/// Shared by both directions: also used as `GeminiCandidate::content` on the
/// response side (Task 1.3.2a), since Phase 1's text-only `GeminiPart` shape
/// is identical either way.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiContent {
    pub role: String,
    #[serde(default)]
    pub parts: Vec<GeminiPart>,
}

/// A single Gemini content part. Text-only in Phase 1; Phase 3 (Story 3.2.1/
/// 3.2.2) adds `functionCall`/`functionResponse` — every field is optional
/// since a given part is exactly one of text/`functionCall`/
/// `functionResponse`, never more than one (Gemini's own tagged-union shape,
/// modeled here the same way `bedrock.rs`'s content-block `Value` juggling
/// does — as sibling `Option`s rather than a Rust `enum`, so `#[serde(flatten)]`-
/// free (de)serialization stays a straight field-by-field mapping).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub function_call: Option<GeminiFunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub function_response: Option<GeminiFunctionResponse>,
}

impl GeminiPart {
    fn text(text: String) -> Self {
        Self {
            text: Some(text),
            function_call: None,
            function_response: None,
        }
    }

    fn function_call(call: GeminiFunctionCall) -> Self {
        Self {
            text: None,
            function_call: Some(call),
            function_response: None,
        }
    }

    fn function_response(response: GeminiFunctionResponse) -> Self {
        Self {
            text: None,
            function_call: None,
            function_response: Some(response),
        }
    }
}

/// Gemini's `functionCall` part shape — `name`+`args` only. Unlike
/// Anthropic's `tool_use` block, Gemini carries no independent `id`; the
/// request/response round trip instead relies on
/// [`super::tools::GeminiToolCallState`] (Story 3.2.1/3.2.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GeminiFunctionCall {
    pub name: String,
    #[serde(default = "default_function_args")]
    pub args: Value,
}

/// Gemini's `functionResponse` part shape — `name`+`response`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GeminiFunctionResponse {
    pub name: String,
    pub response: Value,
}

fn default_function_args() -> Value {
    json!({})
}

/// Gemini's `systemInstruction` — always an **object**, never a bare string
/// (stack.md: "a plain string 400s").
#[derive(Debug, Clone, Serialize)]
pub(crate) struct GeminiSystemInstruction {
    pub parts: Vec<GeminiPart>,
}

/// Maps Anthropic's `max_tokens`/`temperature`/`top_p`/`stop_sequences` onto
/// Gemini's `generationConfig` object. Only fields actually present in the
/// Anthropic request are serialized (`skip_serializing_if` throughout).
#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiGenerationConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_k: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stop_sequences: Option<Vec<String>>,
}

// ---------------------------------------------------------------------------
// Request translation (Task 1.3.1b)
// ---------------------------------------------------------------------------

/// Translate an Anthropic Messages request body into the two-layer Cloud
/// Code Assist envelope.
///
/// Builds a fresh, call-local [`GeminiToolCallState`] and walks
/// `messages[]` in order (Story 3.2.1): each `tool_use` block registers its
/// `id -> name` mapping as it's encountered, and each `tool_result` block
/// consults that mapping to re-associate itself with the right function
/// name (Gemini's `functionResponse` carries no id of its own). This state
/// is never threaded in from outside or persisted across calls — Anthropic
/// always re-sends the full conversation history, `tool_use` blocks
/// included, so everything a `tool_result` needs is already present earlier
/// in this same `messages[]` array.
///
/// # Errors
///
/// Returns [`ProviderError::Validation`] if a `tool_result` block's
/// `tool_use_id` doesn't match any `tool_use` id seen earlier in
/// `messages[]` (Story 3.2.1, Task 3.2.1c). Never returns `Err` for any
/// other Phase 1/2/3.2.1 input.
pub(crate) fn translate_anthropic_request_to_gemini(
    anthropic: &Value,
    project_id: &str,
) -> Result<CloudCodeEnvelope, ProviderError> {
    let model = anthropic
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("gemini-3-pro")
        .to_string();

    let mut tool_call_state = GeminiToolCallState::new();
    let mut contents = Vec::new();
    if let Some(messages) = anthropic.get("messages").and_then(Value::as_array) {
        for msg in messages {
            let role = match msg.get("role").and_then(Value::as_str) {
                Some("assistant") => "model",
                _ => "user",
            }
            .to_string();
            let content = msg.get("content").cloned().unwrap_or(Value::Null);
            contents.push(GeminiContent {
                role,
                parts: content_to_gemini_parts(&content, &mut tool_call_state)?,
            });
        }
    }

    let system_instruction =
        anthropic
            .get("system")
            .and_then(Value::as_str)
            .map(|s| GeminiSystemInstruction {
                parts: vec![GeminiPart::text(s.to_string())],
            });

    let generation_config = build_generation_config(anthropic);
    let tools = build_gemini_tools(anthropic);

    Ok(CloudCodeEnvelope {
        project: project_id.to_string(),
        model,
        request_type: "agent".to_string(),
        user_agent: None,
        request: GeminiRequest {
            contents,
            system_instruction,
            generation_config,
            tools,
        },
    })
}

/// Anthropic `messages[].content` (string, or array of `text`/`tool_use`/
/// `tool_result` blocks) -> `Vec<GeminiPart>`, one part per recognized
/// block. Unrecognized block types are silently skipped, matching Phase 1's
/// existing text-only filtering behavior.
fn content_to_gemini_parts(
    content: &Value,
    tool_call_state: &mut GeminiToolCallState,
) -> Result<Vec<GeminiPart>, ProviderError> {
    match content {
        Value::String(s) => Ok(vec![GeminiPart::text(s.clone())]),
        Value::Array(arr) => {
            let mut parts = Vec::with_capacity(arr.len());
            for block in arr {
                if let Some(part) = block_to_gemini_part(block, tool_call_state)? {
                    parts.push(part);
                }
            }
            Ok(parts)
        }
        _ => Ok(Vec::new()),
    }
}

/// Translates one Anthropic content block (`text`/`tool_use`/`tool_result`)
/// into a `GeminiPart`, or `None` for an unrecognized block type.
fn block_to_gemini_part(
    block: &Value,
    tool_call_state: &mut GeminiToolCallState,
) -> Result<Option<GeminiPart>, ProviderError> {
    match block.get("type").and_then(Value::as_str) {
        Some("text") => Ok(block
            .get("text")
            .and_then(Value::as_str)
            .map(|text| GeminiPart::text(text.to_string()))),
        Some("tool_use") => {
            let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = block
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let args = block.get("input").cloned().unwrap_or_else(|| json!({}));

            tool_call_state.insert(ToolUseId::from(id.to_string()), name.clone());

            Ok(Some(GeminiPart::function_call(GeminiFunctionCall {
                name,
                args,
            })))
        }
        Some("tool_result") => {
            let tool_use_id = block
                .get("tool_use_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let name = tool_call_state
                .get(&ToolUseId::from(tool_use_id.to_string()))
                .ok_or_else(|| {
                    ProviderError::Validation(
                        format!(
                            "tool_result references unknown tool_use_id {tool_use_id:?} — no matching tool_use block seen earlier in this conversation"
                        ),
                        400,
                    )
                })?
                .to_string();
            let result = block.get("content").cloned().unwrap_or(Value::Null);

            Ok(Some(GeminiPart::function_response(
                GeminiFunctionResponse {
                    name,
                    response: json!({ "result": result }),
                },
            )))
        }
        _ => Ok(None),
    }
}

/// Translates the Anthropic request's top-level `tools[]` array into
/// Gemini's `functionDeclarations[]` shape (Task 3.2.1c), running each
/// tool's `input_schema` through [`sanitize_function_schema`] first.
/// Returns `None` when `tools[]` is absent or empty, matching
/// `generationConfig`'s "only present when needed" convention.
fn build_gemini_tools(anthropic: &Value) -> Option<Vec<GeminiTool>> {
    let tools = anthropic.get("tools").and_then(Value::as_array)?;

    let function_declarations: Vec<GeminiFunctionDeclaration> = tools
        .iter()
        .filter_map(|tool| {
            let name = tool.get("name").and_then(Value::as_str)?.to_string();
            let description = tool
                .get("description")
                .and_then(Value::as_str)
                .map(ToString::to_string);
            let input_schema = tool
                .get("input_schema")
                .cloned()
                .unwrap_or_else(|| json!({"type": "object"}));
            let parameters = sanitize_function_schema(&input_schema);

            Some(GeminiFunctionDeclaration {
                name,
                description,
                parameters,
            })
        })
        .collect();

    if function_declarations.is_empty() {
        None
    } else {
        Some(vec![GeminiTool {
            function_declarations,
        }])
    }
}

/// Builds `generationConfig` from whichever of `max_tokens`/`temperature`/
/// `top_p`/`stop_sequences` are present on the Anthropic request, or `None`
/// if none of them are present at all.
fn build_generation_config(anthropic: &Value) -> Option<GeminiGenerationConfig> {
    let max_output_tokens = anthropic
        .get("max_tokens")
        .and_then(Value::as_u64)
        .and_then(|v| u32::try_from(v).ok());
    let temperature = anthropic.get("temperature").and_then(Value::as_f64);
    let top_p = anthropic.get("top_p").and_then(Value::as_f64);
    let stop_sequences = anthropic
        .get("stop_sequences")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(ToString::to_string))
                .collect::<Vec<_>>()
        });

    if max_output_tokens.is_none()
        && temperature.is_none()
        && top_p.is_none()
        && stop_sequences.is_none()
    {
        return None;
    }

    Some(GeminiGenerationConfig {
        max_output_tokens,
        temperature,
        top_p,
        top_k: None,
        stop_sequences,
    })
}

// ---------------------------------------------------------------------------
// Tool schema sanitization (Task 3.1.1a) — Gemini's `functionDeclarations[].
// parameters` rejects JSON-Schema keywords Claude Code's tool schemas rely on
// heavily ($ref/$defs for shared definitions, patternProperties for dynamic
// keys). These can appear at any nesting depth inside a schema, not just the
// top level (unlike bedrock.rs's `clean_body`, which only strips top-level
// tool fields) — so this walks the full `Value` tree recursively.
// ---------------------------------------------------------------------------

/// Recursively strips the JSON-Schema keywords Gemini's `functionDeclarations
/// [].parameters` doesn't accept — `"$ref"`, `"$defs"`, `"patternProperties"`
/// — at every nesting level, leaving every other key/value untouched.
///
/// Not yet wired into `translate_anthropic_request_to_gemini` (Task 3.1.1c):
/// that function doesn't translate `tools[]` at all yet — Story 3.2.1 adds
/// `functionDeclarations[]` translation and is expected to call this on each
/// tool's `input_schema` before emitting it.
#[must_use]
pub(crate) fn sanitize_function_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut cleaned = serde_json::Map::with_capacity(map.len());
            for (key, value) in map {
                if key == "$ref" || key == "$defs" || key == "patternProperties" {
                    continue;
                }
                cleaned.insert(key.clone(), sanitize_function_schema(value));
            }
            Value::Object(cleaned)
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(sanitize_function_schema).collect())
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Response-direction wire structs (Task 1.3.2a) — strict parsing (ADR-002):
// no lenient defaults on required fields, so a missing/malformed `candidates`
// key fails deserialization rather than silently producing an empty response.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiGenerateContentResponse {
    pub candidates: Vec<GeminiCandidate>,
    #[serde(default)]
    pub usage_metadata: Option<GeminiUsageMetadata>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GeminiCandidate {
    #[serde(default)]
    pub content: GeminiContent,
    #[serde(default)]
    pub finish_reason: Option<String>,
}

impl Default for GeminiContent {
    fn default() -> Self {
        Self {
            role: "model".to_string(),
            parts: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_field_names)] // field names match Gemini's own `usageMetadata.*` keys
pub(crate) struct GeminiUsageMetadata {
    pub prompt_token_count: u64,
    pub candidates_token_count: u64,
    #[serde(default)]
    pub cached_content_token_count: Option<u64>,
    #[serde(default)]
    pub thoughts_token_count: Option<u64>,
    pub total_token_count: u64,
}

// ---------------------------------------------------------------------------
// finishReason mapping (Task 1.3.2b)
// ---------------------------------------------------------------------------

/// Maps Gemini's `finishReason` onto an Anthropic `stop_reason`, plus an
/// optional synthesized explanatory text block for the "generation
/// completed but was blocked" cases (`SAFETY`/`RECITATION`) — these are
/// deliberately NOT surfaced as a hard `ProviderError::Validation` (decision
/// #3): a safety-blocked-but-otherwise-completed generation is not a bad
/// *request*.
#[must_use]
pub(crate) fn map_gemini_finish_reason(reason: Option<&str>) -> (&'static str, Option<String>) {
    match reason {
        // "STOP" falls through to the wildcard arm below — same
        // ("end_turn", None) result — kept undestructured to avoid a
        // clippy::match_same_arms duplicate-arm warning.
        Some("MAX_TOKENS") => ("max_tokens", None),
        Some("SAFETY") => (
            "end_turn",
            Some(
                "[Gemini stopped generating: response blocked by safety filters (finishReason=SAFETY)]"
                    .to_string(),
            ),
        ),
        Some("RECITATION") => (
            "end_turn",
            Some(
                "[Gemini stopped generating: response blocked due to recitation (finishReason=RECITATION)]"
                    .to_string(),
            ),
        ),
        _ => ("end_turn", None),
    }
}

// ---------------------------------------------------------------------------
// Response translation (Task 1.3.2c)
// ---------------------------------------------------------------------------

/// Translate a strictly-parsed Gemini `generateContent` response into an
/// Anthropic Messages response body.
///
/// Each `functionCall` part becomes a `tool_use` block with a synthesized
/// `toolu_{uuid}` id (mirroring `openai.rs`'s `msg_{uuid}` pattern), and
/// registers that id into `tool_call_state` (Story 3.2.2) so a later
/// request-direction `tool_result` referencing it can be re-associated with
/// the right function name via `GeminiToolCallState::get` (Story 3.2.1).
/// Presence of a `functionCall` part takes precedence over the plain
/// `STOP`->`end_turn` mapping: `stop_reason` becomes `"tool_use"` regardless
/// of `finishReason`'s own value.
#[must_use]
pub(crate) fn translate_gemini_response_to_anthropic(
    response: &GeminiGenerateContentResponse,
    model: &str,
    tool_call_state: &mut GeminiToolCallState,
) -> Value {
    let mut content: Vec<Value> = Vec::new();
    let mut stop_reason: &'static str = "end_turn";
    let mut has_function_call = false;

    if let Some(candidate) = response.candidates.first() {
        for part in &candidate.content.parts {
            if let Some(text) = &part.text {
                content.push(json!({"type": "text", "text": text}));
            }
            if let Some(function_call) = &part.function_call {
                has_function_call = true;
                let tool_use_id = format!("toolu_{}", uuid::Uuid::new_v4());
                tool_call_state.insert(
                    ToolUseId::from(tool_use_id.clone()),
                    function_call.name.clone(),
                );
                content.push(json!({
                    "type": "tool_use",
                    "id": tool_use_id,
                    "name": function_call.name,
                    "input": function_call.args,
                }));
            }
        }

        let (mapped_stop_reason, synthesized) =
            map_gemini_finish_reason(candidate.finish_reason.as_deref());
        stop_reason = if has_function_call {
            "tool_use"
        } else {
            mapped_stop_reason
        };
        if let Some(text) = synthesized {
            content.push(json!({"type": "text", "text": text}));
        }
    }

    let usage = response.usage_metadata.as_ref().map_or_else(
        || {
            json!({
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0,
            })
        },
        |u| {
            json!({
                "input_tokens": u.prompt_token_count,
                "output_tokens": u.candidates_token_count,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0,
            })
        },
    );

    json!({
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": stop_reason,
        "usage": usage,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    // ────────────────────────────────────────────────────────────────────
    // Story 1.3.1 — request translation
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn translate_anthropic_request_to_gemini_should_produce_object_system_instruction_and_generation_config(
    ) {
        let anthropic = json!({
            "model": "gemini-3-pro",
            "system": "You are terse.",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1024,
            "temperature": 0.5,
        });

        let envelope = translate_anthropic_request_to_gemini(&anthropic, "my-gcp-project").unwrap();
        let value = serde_json::to_value(&envelope).unwrap();

        assert_eq!(
            value["request"]["systemInstruction"],
            json!({"parts": [{"text": "You are terse."}]})
        );
        assert_eq!(
            value["request"]["contents"],
            json!([{"role": "user", "parts": [{"text": "hi"}]}])
        );
        assert_eq!(
            value["request"]["generationConfig"],
            json!({"maxOutputTokens": 1024, "temperature": 0.5})
        );
    }

    #[test]
    fn translate_anthropic_request_to_gemini_should_omit_absent_generation_config_fields() {
        let anthropic = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 1024,
        });

        let envelope = translate_anthropic_request_to_gemini(&anthropic, "p1").unwrap();
        let value = serde_json::to_value(&envelope).unwrap();

        let generation_config = value["request"]["generationConfig"].as_object().unwrap();
        assert_eq!(generation_config.len(), 1, "only maxOutputTokens present");
        assert!(!generation_config.contains_key("topP"));
        assert!(!generation_config.contains_key("topK"));
        assert!(!generation_config.contains_key("stopSequences"));
    }

    #[test]
    fn translate_anthropic_request_to_gemini_should_use_configured_project_id_never_a_default() {
        let anthropic = json!({"messages": [{"role": "user", "content": "hi"}]});

        let envelope = translate_anthropic_request_to_gemini(&anthropic, "my-gcp-project").unwrap();

        assert_eq!(envelope.project, "my-gcp-project");
        assert_eq!(envelope.request_type, "agent");
    }

    #[test]
    fn translate_anthropic_request_to_gemini_should_never_substitute_a_default_project_id() {
        let anthropic = json!({"messages": [{"role": "user", "content": "hi"}]});

        // A distinctive, non-obvious project id — catches accidental
        // hardcoding of a plausible-looking default (e.g. "default-project").
        let envelope =
            translate_anthropic_request_to_gemini(&anthropic, "acme-corp-billing-2026").unwrap();

        assert_eq!(envelope.project, "acme-corp-billing-2026");
    }

    #[test]
    fn translate_anthropic_request_to_gemini_should_return_ok_for_every_phase_1_2_input() {
        let fixtures = [
            json!({"messages": []}),
            json!({"messages": [{"role": "user", "content": "hi"}]}),
            json!({
                "system": "be terse",
                "messages": [
                    {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                    {"role": "assistant", "content": "hello"},
                ],
                "max_tokens": 512,
                "temperature": 0.2,
                "top_p": 0.9,
                "stop_sequences": ["STOP"],
            }),
            json!({}),
        ];

        for fixture in fixtures {
            assert!(
                translate_anthropic_request_to_gemini(&fixture, "p1").is_ok(),
                "expected Ok for {fixture}"
            );
        }
    }

    #[test]
    fn translate_anthropic_request_to_gemini_should_map_assistant_role_to_model() {
        let anthropic = json!({
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"},
            ],
        });

        let envelope = translate_anthropic_request_to_gemini(&anthropic, "p1").unwrap();

        assert_eq!(envelope.request.contents[0].role, "user");
        assert_eq!(envelope.request.contents[1].role, "model");
    }

    // ────────────────────────────────────────────────────────────────────
    // Story 3.2.1 — request-direction tool_use/tool_result translation
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn translate_anthropic_request_to_gemini_should_map_tool_use_block_to_function_call_part_without_id(
    ) {
        let anthropic = json!({
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "toolu_01",
                    "name": "get_weather",
                    "input": {"city": "Boise"},
                }],
            }],
        });

        let envelope = translate_anthropic_request_to_gemini(&anthropic, "p1").unwrap();
        let value = serde_json::to_value(&envelope).unwrap();

        assert_eq!(
            value["request"]["contents"],
            json!([{
                "role": "model",
                "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "Boise"}}}],
            }])
        );
    }

    #[test]
    fn translate_anthropic_request_to_gemini_should_map_tool_result_to_function_response_via_tool_call_state(
    ) {
        let anthropic = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_01",
                        "name": "get_weather",
                        "input": {"city": "Boise"},
                    }],
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_01",
                        "content": "58F and sunny",
                    }],
                },
            ],
        });

        let envelope = translate_anthropic_request_to_gemini(&anthropic, "p1").unwrap();
        let value = serde_json::to_value(&envelope).unwrap();

        assert_eq!(
            value["request"]["contents"][1],
            json!({
                "role": "user",
                "parts": [{
                    "functionResponse": {
                        "name": "get_weather",
                        "response": {"result": "58F and sunny"},
                    },
                }],
            })
        );
    }

    #[test]
    fn translate_anthropic_request_to_gemini_should_return_validation_error_when_tool_result_references_unknown_tool_use_id(
    ) {
        let anthropic = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "toolu_never_seen",
                    "content": "irrelevant",
                }],
            }],
        });

        let err = translate_anthropic_request_to_gemini(&anthropic, "p1").unwrap_err();

        match err {
            ProviderError::Validation(msg, status) => {
                assert_eq!(status, 400);
                assert!(
                    msg.contains("toolu_never_seen"),
                    "expected error message to name the offending id, got: {msg}"
                );
            }
            other => panic!("expected ProviderError::Validation, got {other:?}"),
        }
    }

    #[test]
    fn translate_anthropic_request_to_gemini_should_translate_tools_array_into_sanitized_function_declarations(
    ) {
        let anthropic = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "name": "get_weather",
                "description": "Gets the weather",
                "input_schema": {
                    "type": "object",
                    "properties": {"city": {"$ref": "#/$defs/City"}},
                    "$defs": {"City": {"type": "string"}},
                },
            }],
        });

        let envelope = translate_anthropic_request_to_gemini(&anthropic, "p1").unwrap();
        let value = serde_json::to_value(&envelope).unwrap();

        assert_eq!(
            value["request"]["tools"],
            json!([{
                "functionDeclarations": [{
                    "name": "get_weather",
                    "description": "Gets the weather",
                    "parameters": {
                        "type": "object",
                        "properties": {"city": {}},
                    },
                }],
            }])
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // Story 1.3.2 — response translation + finishReason mapping
    // ────────────────────────────────────────────────────────────────────

    fn parse_gemini_response(value: Value) -> GeminiGenerateContentResponse {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn translate_gemini_response_to_anthropic_should_map_stop_to_end_turn_with_correct_usage() {
        let response = parse_gemini_response(json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "hello"}]},
                "finishReason": "STOP",
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15,
            },
        }));

        let anthropic = translate_gemini_response_to_anthropic(
            &response,
            "gemini-3-pro",
            &mut GeminiToolCallState::new(),
        );

        assert_eq!(
            anthropic["content"],
            json!([{"type": "text", "text": "hello"}])
        );
        assert_eq!(anthropic["stop_reason"], json!("end_turn"));
        assert_eq!(
            anthropic["usage"],
            json!({
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0,
            })
        );
    }

    #[test]
    fn translate_gemini_response_to_anthropic_should_synthesize_explanatory_text_when_finish_reason_is_safety(
    ) {
        let response = parse_gemini_response(json!({
            "candidates": [{
                "content": {"role": "model", "parts": []},
                "finishReason": "SAFETY",
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 0,
                "totalTokenCount": 10,
            },
        }));

        let anthropic = translate_gemini_response_to_anthropic(
            &response,
            "gemini-3-pro",
            &mut GeminiToolCallState::new(),
        );

        assert_eq!(anthropic["stop_reason"], json!("end_turn"));
        assert_eq!(
            anthropic["content"],
            json!([{
                "type": "text",
                "text": "[Gemini stopped generating: response blocked by safety filters (finishReason=SAFETY)]",
            }])
        );
    }

    #[test]
    fn translate_gemini_response_to_anthropic_should_map_max_tokens_finish_reason_to_max_tokens_stop_reason(
    ) {
        let response = parse_gemini_response(json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "hello"}]},
                "finishReason": "MAX_TOKENS",
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "totalTokenCount": 15,
            },
        }));

        let anthropic = translate_gemini_response_to_anthropic(
            &response,
            "gemini-3-pro",
            &mut GeminiToolCallState::new(),
        );

        assert_eq!(anthropic["stop_reason"], json!("max_tokens"));
    }

    // ────────────────────────────────────────────────────────────────────
    // Story 3.2.2 — response-direction functionCall translation
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn translate_gemini_response_to_anthropic_should_synthesize_tool_use_id_and_set_stop_reason_tool_use(
    ) {
        let response = parse_gemini_response(json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "Boise"}}}],
                },
                "finishReason": "STOP",
            }],
        }));
        let mut tool_call_state = GeminiToolCallState::new();

        let anthropic =
            translate_gemini_response_to_anthropic(&response, "gemini-3-pro", &mut tool_call_state);

        assert_eq!(anthropic["stop_reason"], json!("tool_use"));
        let blocks = anthropic["content"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0]["type"], json!("tool_use"));
        assert_eq!(blocks[0]["name"], json!("get_weather"));
        assert_eq!(blocks[0]["input"], json!({"city": "Boise"}));
        let id = blocks[0]["id"].as_str().unwrap();
        assert!(
            id.starts_with("toolu_"),
            "expected a synthesized toolu_<uuid> id, got {id}"
        );
    }

    #[test]
    fn translate_gemini_response_to_anthropic_should_register_synthesized_id_in_tool_call_state_for_later_lookup(
    ) {
        let response = parse_gemini_response(json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"functionCall": {"name": "get_weather", "args": {"city": "Boise"}}}],
                },
                "finishReason": "STOP",
            }],
        }));
        let mut tool_call_state = GeminiToolCallState::new();

        let anthropic =
            translate_gemini_response_to_anthropic(&response, "gemini-3-pro", &mut tool_call_state);

        let synthesized_id = anthropic["content"][0]["id"].as_str().unwrap().to_string();

        assert_eq!(
            tool_call_state.get(&ToolUseId::from(synthesized_id)),
            Some("get_weather")
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // Story 3.1.1 — sanitize_function_schema
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn sanitize_function_schema_should_strip_nested_ref_and_defs_while_preserving_sibling_fields()
    {
        let schema = json!({
            "type": "object",
            "properties": {
                "foo": {"$ref": "#/$defs/Foo", "description": "a foo"}
            },
            "$defs": {"Foo": {"type": "string"}}
        });

        let sanitized = sanitize_function_schema(&schema);

        assert_eq!(
            sanitized,
            json!({
                "type": "object",
                "properties": {
                    "foo": {"description": "a foo"}
                }
            })
        );
    }

    #[test]
    fn sanitize_function_schema_should_strip_pattern_properties_at_any_depth() {
        let schema = json!({
            "type": "object",
            "patternProperties": {"^S_": {"type": "string"}}
        });

        let sanitized = sanitize_function_schema(&schema);

        assert_eq!(sanitized, json!({"type": "object"}));
    }

    #[test]
    fn sanitize_function_schema_should_strip_keywords_buried_three_levels_deep() {
        let schema = json!({
            "type": "object",
            "properties": {
                "a": {
                    "type": "object",
                    "properties": {
                        "b": {
                            "type": "object",
                            "properties": {
                                "c": {
                                    "$ref": "#/$defs/Deep",
                                    "patternProperties": {"^x_": {"type": "number"}},
                                    "description": "deeply nested"
                                }
                            },
                            "$defs": {"Deep": {"type": "string"}}
                        }
                    }
                }
            }
        });

        let sanitized = sanitize_function_schema(&schema);

        assert_eq!(
            sanitized,
            json!({
                "type": "object",
                "properties": {
                    "a": {
                        "type": "object",
                        "properties": {
                            "b": {
                                "type": "object",
                                "properties": {
                                    "c": {"description": "deeply nested"}
                                }
                            }
                        }
                    }
                }
            })
        );
    }

    #[test]
    fn map_gemini_finish_reason_covers_known_and_fallback_cases() {
        assert_eq!(map_gemini_finish_reason(Some("STOP")), ("end_turn", None));
        assert_eq!(
            map_gemini_finish_reason(Some("MAX_TOKENS")),
            ("max_tokens", None)
        );
        assert_eq!(map_gemini_finish_reason(None), ("end_turn", None));
        assert_eq!(map_gemini_finish_reason(Some("OTHER")), ("end_turn", None));
    }
}
