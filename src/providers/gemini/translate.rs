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

/// A single Gemini content part. Text-only in Phase 1 — `functionCall`/
/// `functionResponse` variants land in Phase 3.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct GeminiPart {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub text: Option<String>,
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
/// `Result`-returning from Phase 1 onward even though every Phase 1/2 input
/// returns `Ok` — Phase 3's Story 3.3.2 needs an `Err` path (a `tool_use` id
/// with no cached `thought_signature`), and designing the signature fallible
/// now avoids a breaking one-call-site signature change later.
///
/// # Errors
///
/// Never returns `Err` for any Phase 1/2 (text-only, no tool calls) input —
/// intentionally `Result`-returning ahead of Phase 3's Story 3.3.2, which
/// needs an `Err` path (a `tool_use` id with no cached `thought_signature`);
/// see plan.md Task 1.3.1b.
#[allow(clippy::unnecessary_wraps)]
pub(crate) fn translate_anthropic_request_to_gemini(
    anthropic: &Value,
    project_id: &str,
) -> Result<CloudCodeEnvelope, ProviderError> {
    let model = anthropic
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("gemini-3-pro")
        .to_string();

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
                parts: content_to_gemini_parts(&content),
            });
        }
    }

    let system_instruction =
        anthropic
            .get("system")
            .and_then(Value::as_str)
            .map(|s| GeminiSystemInstruction {
                parts: vec![GeminiPart {
                    text: Some(s.to_string()),
                }],
            });

    let generation_config = build_generation_config(anthropic);

    Ok(CloudCodeEnvelope {
        project: project_id.to_string(),
        model,
        request_type: "agent".to_string(),
        user_agent: None,
        request: GeminiRequest {
            contents,
            system_instruction,
            generation_config,
        },
    })
}

/// Anthropic `messages[].content` (string or array of `{"type":"text",...}`
/// blocks) -> `Vec<GeminiPart>`, one part per text block.
fn content_to_gemini_parts(content: &Value) -> Vec<GeminiPart> {
    match content {
        Value::String(s) => vec![GeminiPart {
            text: Some(s.clone()),
        }],
        Value::Array(arr) => arr
            .iter()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .map(|text| GeminiPart {
                text: Some(text.to_string()),
            })
            .collect(),
        _ => Vec::new(),
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
#[must_use]
pub(crate) fn translate_gemini_response_to_anthropic(
    response: &GeminiGenerateContentResponse,
    model: &str,
) -> Value {
    let mut content: Vec<Value> = Vec::new();
    let mut stop_reason: &'static str = "end_turn";

    if let Some(candidate) = response.candidates.first() {
        for part in &candidate.content.parts {
            if let Some(text) = &part.text {
                content.push(json!({"type": "text", "text": text}));
            }
        }

        let (mapped_stop_reason, synthesized) =
            map_gemini_finish_reason(candidate.finish_reason.as_deref());
        stop_reason = mapped_stop_reason;
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

        let anthropic = translate_gemini_response_to_anthropic(&response, "gemini-3-pro");

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

        let anthropic = translate_gemini_response_to_anthropic(&response, "gemini-3-pro");

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

        let anthropic = translate_gemini_response_to_anthropic(&response, "gemini-3-pro");

        assert_eq!(anthropic["stop_reason"], json!("max_tokens"));
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
