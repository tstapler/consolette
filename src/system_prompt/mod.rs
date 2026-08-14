//! System prompt pipeline: single-pass `CacheAligner` + Verbosity Steering.
//!
//! ADR-008: FR-7 and FR-9 MUST run in one pass. Separate passes corrupt each other's
//! cache markers and bust the Anthropic KV cache on every request.

pub mod cache_aligner;
pub mod quantum_lock;
pub mod verbosity;

use std::sync::Arc;

use serde_json::{json, Value};
use tracing::debug;

use crate::config::schema::Config;
use cache_aligner::should_add_cache_control;
use quantum_lock::stabilize_dynamic_fragments;
use verbosity::{is_continuation_turn, verbosity_suffix};

pub struct SystemPromptPipeline {
    config: Arc<Config>,
}

impl SystemPromptPipeline {
    #[must_use]
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }

    /// Apply `CacheAligner` + Verbosity Steering in a single pass.
    ///
    /// Input cases handled:
    /// 1. No `system` field → optionally inject verbosity-only block (no `cache_control`)
    /// 2. `system` is a `String` → convert to block 0, apply pipeline
    /// 3. `system` is an `Array` → use as-is, apply pipeline
    ///
    /// Output: `system` field replaced with a 1- or 2-element block array.
    #[must_use]
    pub fn apply(&self, mut body: Value) -> Value {
        let verbosity = self.config.verbosity_level;
        let cache_aligner = self.config.cache_aligner;

        // Determine if this is a continuation turn (tool results only).
        let is_continuation = body
            .get("messages")
            .and_then(Value::as_array)
            .is_some_and(|msgs| is_continuation_turn(msgs));

        let suffix = if is_continuation {
            verbosity_suffix(verbosity)
        } else {
            ""
        };

        let system = body.get("system").cloned();

        match system {
            None => {
                // No system prompt. Only act if verbosity suffix is needed.
                if !suffix.is_empty() {
                    let block = json!({"type": "text", "text": suffix});
                    body["system"] = json!([block]);
                }
                // Both features off or not a continuation → no-op.
            }

            Some(Value::String(s)) => {
                let blocks = build_output_blocks(&s, suffix, cache_aligner);
                body["system"] = json!(blocks);
            }

            Some(Value::Array(arr)) => {
                // Concatenate all text content from the original blocks into a single string.
                let combined = extract_text_from_blocks(&arr);
                let blocks = build_output_blocks(&combined, suffix, cache_aligner);
                body["system"] = json!(blocks);
            }

            Some(_) => {
                // Unknown shape — leave untouched.
                debug!("system_prompt: unexpected system field type, skipping pipeline");
            }
        }

        body
    }
}

/// Extract and concatenate text from an array of system blocks.
fn extract_text_from_blocks(arr: &[Value]) -> String {
    arr.iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the 1- to 3-block output array.
///
/// Block 0: original content, with dynamic fragments (UUIDs, timestamps,
/// tokens) rewritten to stable placeholders when `cache_aligner` is on
/// (QuantumLock/CachePrefixManager), plus optional `cache_control` if the
/// resulting text is stable enough. Middle block (if any dynamic fragments
/// were found): the real values, so the model still sees them, but as a
/// block that never gets `cache_control` and so doesn't need to stay
/// byte-identical across requests. Last block: verbosity suffix (if
/// non-empty), no `cache_control`.
fn build_output_blocks(original_text: &str, suffix: &str, cache_aligner: bool) -> Vec<Value> {
    let (stable_text, dynamic_trailer) = if cache_aligner {
        stabilize_dynamic_fragments(original_text)
    } else {
        (original_text.to_string(), None)
    };

    let mut primary = json!({"type": "text", "text": stable_text});

    if cache_aligner && should_add_cache_control(&stable_text) {
        primary["cache_control"] = json!({"type": "ephemeral"});
    } else if cache_aligner {
        debug!(
            text_len = stable_text.len(),
            "system_prompt: skipping cache_control (volatile or below threshold)"
        );
    }

    let mut output = vec![primary];

    if let Some(trailer) = dynamic_trailer {
        output.push(json!({"type": "text", "text": trailer}));
    }

    if !suffix.is_empty() {
        output.push(json!({"type": "text", "text": suffix}));
    }

    output
}

#[cfg(test)]
#[allow(clippy::unwrap_used)] // test assertions on well-formed fixtures
mod tests {
    use super::*;
    use serde_json::json;

    fn make_pipeline(cache_aligner: bool, verbosity_level: u8) -> SystemPromptPipeline {
        let cfg = Config {
            cache_aligner,
            verbosity_level,
            ..Config::default()
        };
        SystemPromptPipeline::new(Arc::new(cfg))
    }

    fn continuation_body(system: &Value) -> Value {
        json!({
            "system": system,
            "messages": [
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "x", "content": "done"}]}
            ]
        })
    }

    fn new_question_body(system: &Value) -> Value {
        json!({
            "system": system,
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "What is the meaning of life?"}]}
            ]
        })
    }

    #[test]
    fn no_system_no_verbosity_noop() {
        let p = make_pipeline(true, 0);
        let body = json!({"messages": [{"role": "user", "content": "hi"}]});
        let out = p.apply(body);
        assert!(out.get("system").is_none());
    }

    #[test]
    fn no_system_with_verbosity_continuation() {
        let p = make_pipeline(true, 2);
        let body = json!({
            "messages": [
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "x", "content": "ok"}]}
            ]
        });
        let out = p.apply(body);
        let system = out["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        assert_eq!(system[0]["type"], "text");
        assert!(system[0].get("cache_control").is_none());
    }

    #[test]
    fn string_system_becomes_two_blocks_on_continuation() {
        let p = make_pipeline(true, 2);
        let long_text = "You are a helpful assistant. ".repeat(20); // >100 chars, stable
        let body = continuation_body(&json!(long_text));
        let out = p.apply(body);
        let system = out["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert_eq!(system[0]["text"], long_text);
        assert!(system[0].get("cache_control").is_some());
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
        assert!(system[1].get("cache_control").is_none());
        assert!(!system[1]["text"].as_str().unwrap().is_empty());
    }

    #[test]
    fn string_system_new_question_no_verbosity_block() {
        let p = make_pipeline(true, 2);
        let long_text = "You are a helpful assistant. ".repeat(20);
        let body = new_question_body(&json!(long_text));
        let out = p.apply(body);
        let system = out["system"].as_array().unwrap();
        // No verbosity suffix on new questions.
        assert_eq!(system.len(), 1);
        assert!(system[0].get("cache_control").is_some());
    }

    #[test]
    fn volatile_text_gets_stabilized_and_cached() {
        // QuantumLock/CachePrefixManager: rather than permanently skipping
        // cache_control on any volatile content, the UUID is rewritten to a
        // stable placeholder in block 0 (which can now be cached) and the
        // real value moves to a separate, never-cached trailer block.
        let p = make_pipeline(true, 0);
        let volatile = "Session ID: 550e8400-e29b-41d4-a716-446655440000. ".repeat(5);
        let body = new_question_body(&json!(volatile));
        let out = p.apply(body);
        let system = out["system"].as_array().unwrap();
        assert_eq!(system.len(), 2);
        assert!(system[0].get("cache_control").is_some());
        assert!(!system[0]["text"].as_str().unwrap().contains("550e8400"));
        assert!(system[1].get("cache_control").is_none());
        assert!(system[1]["text"].as_str().unwrap().contains("550e8400"));
    }

    #[test]
    fn short_text_skips_cache_control() {
        let p = make_pipeline(true, 0);
        let body = new_question_body(&json!("Short."));
        let out = p.apply(body);
        let system = out["system"].as_array().unwrap();
        assert!(system[0].get("cache_control").is_none());
    }

    #[test]
    fn cache_aligner_disabled_omits_cache_control() {
        let p = make_pipeline(false, 0);
        let long_text = "You are a helpful assistant. ".repeat(20);
        let body = new_question_body(&json!(long_text));
        let out = p.apply(body);
        let system = out["system"].as_array().unwrap();
        assert!(system[0].get("cache_control").is_none());
    }

    #[test]
    fn array_system_blocks_concatenated() {
        let p = make_pipeline(true, 0);
        let body = new_question_body(&json!([
            {"type": "text", "text": "You are a helpful assistant. ".repeat(10)},
            {"type": "text", "text": "Always be polite. ".repeat(10)}
        ]));
        let out = p.apply(body);
        let system = out["system"].as_array().unwrap();
        assert_eq!(system.len(), 1);
        let text = system[0]["text"].as_str().unwrap();
        assert!(text.contains("helpful assistant"));
        assert!(text.contains("Always be polite"));
    }
}
