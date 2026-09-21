//! `OpenAI` Responses API (`/v1/responses`) translation logic: request/response
//! translation and streaming. Populated in Phase 3 of the
//! `openai-model-resolution` project.
//!
//! Epic 3.2 (this file): non-streaming translation only —
//! [`translate_anthropic_request_to_responses`] (Anthropic request ->
//! Responses `input`) and [`translate_responses_response_to_anthropic`]
//! (Responses `output[]` -> Anthropic `content`). Streaming
//! (`ResponsesToAnthropicStream`) is Epic 3.3's job.
//!
//! Per architecture.md §2, the **input** side reuses `providers::mod.rs`'s
//! shared Anthropic-block-walking helper (`anthropic_blocks_to_openai`) for
//! tool-call/tool-result extraction rather than re-walking Anthropic content
//! blocks here — the two wire shapes only diverge in how the *already
//! extracted* call id/name/arguments/tool-result text get repackaged. The
//! **output** side has no such shared code: `choices[0].message` and
//! `output[].type` are structurally different documents.

use serde_json::{json, Value};

use crate::providers::anthropic_blocks_to_openai;

/// Convert one already-`anthropic_blocks_to_openai`-shaped `OpenAI` chat
/// message into its Responses API `input` item(s). A `{role: "tool", ...}`
/// message (produced by a `tool_result` block) becomes a
/// `function_call_output` item; any other message becomes a `message` item
/// (if it carries non-empty content) followed by one `function_call` item
/// per entry in its `tool_calls` array (produced by `tool_use` blocks).
fn openai_message_to_responses_items(message: &Value) -> Vec<Value> {
    let role = message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or("user");

    if role == "tool" {
        let call_id = message
            .get("tool_call_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        let output = message.get("content").and_then(Value::as_str).unwrap_or("");
        return vec![json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output,
        })];
    }

    let mut items = Vec::new();
    let content = message.get("content").and_then(Value::as_str).unwrap_or("");
    if !content.is_empty() {
        let part_type = if role == "assistant" {
            "output_text"
        } else {
            "input_text"
        };
        items.push(json!({
            "type": "message",
            "role": role,
            "content": [{"type": part_type, "text": content}],
        }));
    }

    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            let function = call.get("function");
            items.push(json!({
                "type": "function_call",
                "call_id": call.get("id").and_then(Value::as_str).unwrap_or(""),
                "name": function.and_then(|f| f.get("name")).and_then(Value::as_str).unwrap_or(""),
                "arguments": function.and_then(|f| f.get("arguments")).and_then(Value::as_str).unwrap_or("{}"),
            }));
        }
    }
    items
}

/// One Anthropic message's content blocks -> zero or more Responses `input`
/// items. Reuses [`anthropic_blocks_to_openai`] (the same block-walking
/// logic chat/completions' translation uses) to extract tool-call/
/// tool-result data, then repackages that already-extracted shape into
/// Responses' item taxonomy — no separate block-walk of `blocks` happens
/// here.
fn anthropic_blocks_to_responses_items(role: &str, blocks: &[Value]) -> Vec<Value> {
    anthropic_blocks_to_openai(role, blocks)
        .iter()
        .flat_map(openai_message_to_responses_items)
        .collect()
}

/// Map one Anthropic tool definition to a Responses API tool definition.
/// Responses' tool shape is flat (`{"type": "function", "name": ..., ...}`),
/// unlike chat/completions' `{"type": "function", "function": {...}}`
/// nesting — reuses `translate_tool_definition` (the same schema
/// sanitization chat/completions uses) and re-flattens its output, so a
/// schema-sanitization fix in one path is not silently missing from the
/// other.
fn translate_tool_definition_to_responses(tool: &Value) -> Option<Value> {
    let openai_shaped = crate::providers::translate_tool_definition(tool, false)?;
    let function = openai_shaped.get("function")?;
    Some(json!({
        "type": "function",
        "name": function.get("name").cloned().unwrap_or(Value::Null),
        "description": function.get("description").cloned().unwrap_or(Value::Null),
        "parameters": function.get("parameters").cloned().unwrap_or(json!({})),
    }))
}

/// Translate an Anthropic-shaped request body into a Responses API
/// (`POST /v1/responses`) body.
///
/// A single-turn, plain-string user message with no other turns collapses to
/// `input`'s flat-string shorthand (`input: "hi"`); anything richer (prior
/// turns, tool calls/results, non-string content) becomes the typed
/// `input: [...]` item array.
// By-value `Value` params (here and on the sibling response-side function
// below) match the plan/validation-test call convention
// (`translate_anthropic_request_to_responses(json!({...}))`, no `&`) rather
// than borrowing like `translate_anthropic_request_to_openai`.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn translate_anthropic_request_to_responses(anthropic: Value) -> Value {
    let model = anthropic
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("gpt-5")
        .to_string();
    let max_tokens = anthropic.get("max_tokens").and_then(Value::as_u64);
    let temperature = anthropic.get("temperature").cloned();

    let mut items: Vec<Value> = Vec::new();

    if let Some(system) = anthropic.get("system").and_then(Value::as_str) {
        items.push(json!({
            "type": "message",
            "role": "system",
            "content": [{"type": "input_text", "text": system}],
        }));
    }

    if let Some(messages) = anthropic.get("messages").and_then(Value::as_array) {
        for msg in messages {
            let role = msg.get("role").and_then(Value::as_str).unwrap_or("user");
            let content = msg.get("content").cloned().unwrap_or(Value::Null);
            if let Value::Array(blocks) = content {
                items.extend(anthropic_blocks_to_responses_items(role, &blocks));
            } else {
                let text = crate::providers::extract_text_from_content(&content);
                if !text.is_empty() {
                    let part_type = if role == "assistant" {
                        "output_text"
                    } else {
                        "input_text"
                    };
                    items.push(json!({
                        "type": "message",
                        "role": role,
                        "content": [{"type": part_type, "text": text}],
                    }));
                }
            }
        }
    }

    let input = match items.as_slice() {
        [single]
            if single.get("type").and_then(Value::as_str) == Some("message")
                && single.get("role").and_then(Value::as_str) == Some("user") =>
        {
            single
                .get("content")
                .and_then(Value::as_array)
                .and_then(|parts| parts.first())
                .and_then(|part| part.get("text"))
                .and_then(Value::as_str)
                .map_or_else(
                    || Value::Array(items.clone()),
                    |text| Value::String(text.to_string()),
                )
        }
        _ => Value::Array(items),
    };

    let mut body = json!({
        "model": model,
        "input": input,
    });

    if let Some(max_tokens) = max_tokens {
        body["max_output_tokens"] = Value::from(max_tokens);
    }
    if let Some(temp) = temperature {
        body["temperature"] = temp;
    }
    if let Some(tools) = anthropic.get("tools").and_then(Value::as_array) {
        let mapped: Vec<Value> = tools
            .iter()
            .filter_map(translate_tool_definition_to_responses)
            .collect();
        if !mapped.is_empty() {
            body["tools"] = Value::Array(mapped);
        }
    }

    body
}

/// Extract the concatenated text of a Responses `message` output item's
/// `content` parts (`output_text` parts carry a `text` field; anything else
/// is skipped rather than causing a panic on an unrecognized part shape).
fn message_item_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default()
}

/// Anthropic's `thinking` content block requires a non-empty `signature`
/// field, which the real Anthropic API uses to verify a thinking block
/// hasn't been tampered with before being echoed back on a later turn.
/// Responses API `reasoning` items carry no equivalent natively. The
/// closest analog is the item's own `encrypted_content` (opaque
/// Responses-side reasoning state, present when the caller requested
/// `include: ["reasoning.encrypted_content"]`) — prefer that when present,
/// since it's at least round-trippable state from the same item. Otherwise
/// fall back to a clearly-marked opaque token derived from the item's
/// stable `id`. Neither is cryptographically meaningful, and consolette's
/// own translators don't re-verify inbound `thinking.signature` values
/// either — the only real requirement (Epic 3.5 / plan.md) is that the
/// field is present, non-empty, and stable for a given item.
fn reasoning_signature(item_id: &str, encrypted_content: Option<&str>) -> String {
    match encrypted_content {
        Some(enc) if !enc.is_empty() => format!("consolette.opaque-thinking-sig.v1:enc:{enc}"),
        _ => format!("consolette.opaque-thinking-sig.v1:id:{item_id}"),
    }
}

/// Extract the concatenated text of a Responses `reasoning` output item's
/// `summary` parts.
fn reasoning_item_text(item: &Value) -> String {
    item.get("summary")
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// Translate a non-streaming Responses API response body into an
/// Anthropic-shaped Messages response.
///
/// Mirrors `translate_openai_response_to_anthropic`'s content-block ordering
/// rule (thinking blocks before `text`/`tool_use`) and its "never emit zero
/// content blocks" fallback. An output item of an unrecognized `type` is
/// skipped rather than causing a panic — the Responses API output taxonomy
/// is expected to grow new item types over time.
#[allow(clippy::needless_pass_by_value)]
pub(crate) fn translate_responses_response_to_anthropic(responses_body: Value) -> Value {
    let id = responses_body
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let model = responses_body
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();

    let mut thinking_blocks: Vec<Value> = Vec::new();
    let mut content_blocks: Vec<Value> = Vec::new();
    let mut saw_tool_calls = false;

    if let Some(output) = responses_body.get("output").and_then(Value::as_array) {
        for item in output {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    let text = message_item_text(item);
                    if !text.is_empty() {
                        content_blocks.push(json!({"type": "text", "text": text}));
                    }
                }
                Some("function_call") => {
                    saw_tool_calls = true;
                    let call_id = item
                        .get("call_id")
                        .and_then(Value::as_str)
                        .unwrap_or("call_unknown");
                    let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                    let input = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .and_then(|args| serde_json::from_str::<Value>(args).ok())
                        .unwrap_or_else(|| json!({}));
                    content_blocks.push(json!({
                        "type": "tool_use",
                        "id": call_id,
                        "name": name,
                        "input": input,
                    }));
                }
                Some("reasoning") => {
                    let text = reasoning_item_text(item);
                    if !text.is_empty() {
                        let item_id = item.get("id").and_then(Value::as_str).unwrap_or("unknown");
                        let encrypted_content =
                            item.get("encrypted_content").and_then(Value::as_str);
                        thinking_blocks.push(json!({
                            "type": "thinking",
                            "thinking": text,
                            "signature": reasoning_signature(item_id, encrypted_content),
                        }));
                    }
                }
                // Unrecognized output item type: skip, never panic — the
                // Responses API output taxonomy is expected to grow.
                _ => {}
            }
        }
    }

    let mut blocks = thinking_blocks;
    blocks.extend(content_blocks);
    if blocks.is_empty() {
        blocks.push(json!({"type": "text", "text": ""}));
    }

    let stop_reason = if saw_tool_calls {
        "tool_use"
    } else {
        match responses_body
            .get("incomplete_details")
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
        {
            Some("max_output_tokens") => "max_tokens",
            _ => "end_turn",
        }
    };

    let input_tokens = responses_body
        .get("usage")
        .and_then(|u| u.get("input_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let output_tokens = responses_body
        .get("usage")
        .and_then(|u| u.get("output_tokens"))
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
            "input_tokens": input_tokens,
            "output_tokens": output_tokens
        }
    })
}

// ────────────────────────────────────────────────────────────────────────
// Epic 3.3: streaming Responses API -> Anthropic SSE translation
// ────────────────────────────────────────────────────────────────────────

use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures_core::Stream;
use tracing::warn;

/// The Anthropic content-block shape a Responses `output_item` maps to.
/// Unlike `OpenaiToAnthropicStream`'s flat text/tool split, every item kind
/// here (including `reasoning`) gets its own tracked block, addressed by
/// `item_id` rather than a wire index (see plan.md's `ItemId` glossary
/// entry and pitfalls.md §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponsesBlockKind {
    Text,
    ToolUse,
    /// Attributes deltas to the correct block with no cross-item mixing
    /// (Story 3.3.1) and finalizes a non-empty `signature` on close (Story
    /// 3.5.1) — emits genuine `type: "thinking"` content blocks, not
    /// mislabeled `text` ones (pitfalls.md §2).
    Thinking,
}

/// One tracked Responses `output_item`, keyed by its `item_id` in
/// [`ResponsesToAnthropicStream::item_index`]. `block_index` is the stable,
/// dense Anthropic-face index assigned in item-appearance order.
struct ResponsesBlock {
    kind: ResponsesBlockKind,
    block_index: usize,
    started: bool,
    stopped: bool,
    /// Kept so a `Thinking` block's closing `signature_delta` can be
    /// derived even in the `close()` safety-net path, which only has the
    /// block (not the full `response.output_item.done` item payload) to
    /// work with.
    item_id: String,
}

/// Translates a Responses API (`/v1/responses`) SSE byte stream into an
/// Anthropic Messages SSE event stream — the Responses-API counterpart to
/// [`super::OpenaiToAnthropicStream`], but structurally independent: it
/// tracks state by `item_id` (a string, addressing a tree of `output[]`
/// items) rather than a flat wire `index`, because the two APIs' addressing
/// models are incompatible (pitfalls.md §2; plan.md's `ItemId`/
/// `ResponsesToAnthropicStream` glossary entries). Do not extend
/// `OpenaiToAnthropicStream` to also cover this — that was deliberately
/// rejected as a pattern decision (plan.md's Pattern Decisions table).
pub(crate) struct ResponsesToAnthropicStream<S> {
    inner: eventsource_stream::EventStream<S>,
    id: String,
    model: String,
    started: bool,
    finished: bool,
    done: bool,
    pending: VecDeque<Bytes>,
    /// `item_id` -> position in `blocks`.
    item_index: HashMap<String, usize>,
    /// Appearance-ordered; index into this `Vec` equals the Anthropic-face
    /// `block_index`.
    blocks: Vec<ResponsesBlock>,
}

impl<S> ResponsesToAnthropicStream<S>
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
            item_index: HashMap::new(),
            blocks: Vec::new(),
        }
    }

    fn frame(event: &str, data: &Value) -> Bytes {
        Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
    }

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

    /// Register a newly-`added` output item and emit its `content_block_start`
    /// immediately — unlike Chat Completions tool calls (which need id+name
    /// accumulated before a block can open), a Responses `output_item.added`
    /// event already carries full item metadata (`role`/`call_id`/`name`),
    /// so there's no "pending until known" state to track here.
    fn open_item(&mut self, item: &Value) {
        let Some(item_id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        if self.item_index.contains_key(item_id) {
            return;
        }
        let Some(item_type) = item.get("type").and_then(Value::as_str) else {
            return;
        };

        self.ensure_message_started();
        let block_index = self.blocks.len();

        let content_block = match item_type {
            "message" => Some(json!({"type": "text", "text": ""})),
            "function_call" => {
                let call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .unwrap_or(item_id);
                let name = item.get("name").and_then(Value::as_str).unwrap_or("");
                Some(json!({"type": "tool_use", "id": call_id, "name": name, "input": {}}))
            }
            "reasoning" => Some(json!({"type": "thinking", "thinking": "", "signature": ""})),
            // Unrecognized item type: the Responses output taxonomy is
            // expected to grow; skip rather than track/panic (mirrors the
            // non-streaming translator's fallback).
            _ => None,
        };
        let Some(content_block) = content_block else {
            return;
        };
        let kind = match item_type {
            "function_call" => ResponsesBlockKind::ToolUse,
            "reasoning" => ResponsesBlockKind::Thinking,
            _ => ResponsesBlockKind::Text,
        };

        self.item_index.insert(item_id.to_string(), block_index);
        self.blocks.push(ResponsesBlock {
            kind,
            block_index,
            started: true,
            stopped: false,
            item_id: item_id.to_string(),
        });
        self.pending.push_back(Self::frame(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": block_index,
                "content_block": content_block
            }),
        ));
    }

    fn push_delta(&mut self, item_id: &str, expected_kind: ResponsesBlockKind, text: &str) {
        if text.is_empty() {
            return;
        }
        let Some(&idx) = self.item_index.get(item_id) else {
            return;
        };
        let Some(block) = self.blocks.get(idx) else {
            return;
        };
        if block.kind != expected_kind || !block.started || block.stopped {
            return;
        }
        let delta = match expected_kind {
            ResponsesBlockKind::Text => json!({"type": "text_delta", "text": text}),
            ResponsesBlockKind::ToolUse => {
                json!({"type": "input_json_delta", "partial_json": text})
            }
            ResponsesBlockKind::Thinking => json!({"type": "thinking_delta", "thinking": text}),
        };
        self.pending.push_back(Self::frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": block.block_index,
                "delta": delta
            }),
        ));
    }

    /// Close one item's block on `response.output_item.done`. Idempotent —
    /// a `done` for an item never opened (unrecognized type) is a no-op.
    ///
    /// Takes the full `item` payload (not just its id) because a `Thinking`
    /// block's `signature` is finalized here, at the end of the block —
    /// mirroring how Anthropic's own extended-thinking streaming finalizes
    /// a thinking block's signature via a `signature_delta` just before
    /// `content_block_stop`, rather than up front at `content_block_start`
    /// — and the finished item is the only place `encrypted_content` (if
    /// requested) would appear.
    fn close_item(&mut self, item: &Value) {
        let Some(item_id) = item.get("id").and_then(Value::as_str) else {
            return;
        };
        let Some(&idx) = self.item_index.get(item_id) else {
            return;
        };
        let Some(block) = self.blocks.get_mut(idx) else {
            return;
        };
        if !block.started || block.stopped {
            return;
        }
        block.stopped = true;
        let block_index = block.block_index;
        let kind = block.kind;

        if kind == ResponsesBlockKind::Thinking {
            let encrypted_content = item.get("encrypted_content").and_then(Value::as_str);
            let signature = reasoning_signature(item_id, encrypted_content);
            self.pending.push_back(Self::frame(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": block_index,
                    "delta": {"type": "signature_delta", "signature": signature}
                }),
            ));
        }

        self.pending.push_back(Self::frame(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": block_index}),
        ));
    }

    /// Close every still-open block (safety net for a terminal event
    /// arriving before every item's own `output_item.done`) and emit the
    /// terminal `message_delta`/`message_stop` pair. `stop_reason ==
    /// "error"` is Story 3.3.2's mid-stream-application-error signal — the
    /// choice documented there: a distinguishing `stop_reason` value on
    /// `message_delta`, not a separate Anthropic `error` SSE event type,
    /// since that's the minimum bar Anthropic-compatible clients are
    /// guaranteed to branch on (they already inspect `stop_reason`).
    fn close(&mut self, stop_reason: &str) {
        if self.finished {
            return;
        }
        self.ensure_message_started();
        self.finished = true;

        let saw_tool_use = self
            .blocks
            .iter()
            .any(|b| b.kind == ResponsesBlockKind::ToolUse && b.started);

        for i in 0..self.blocks.len() {
            let block = &self.blocks[i];
            if block.started && !block.stopped {
                let block_index = block.block_index;
                let kind = block.kind;
                let item_id = block.item_id.clone();
                self.blocks[i].stopped = true;
                if kind == ResponsesBlockKind::Thinking {
                    // No `response.output_item.done` arrived for this
                    // item before the stream ended (e.g. a `response.failed`
                    // mid-block) — no `encrypted_content` is available here,
                    // so fall back to the id-derived signature.
                    let signature = reasoning_signature(&item_id, None);
                    self.pending.push_back(Self::frame(
                        "content_block_delta",
                        &json!({
                            "type": "content_block_delta",
                            "index": block_index,
                            "delta": {"type": "signature_delta", "signature": signature}
                        }),
                    ));
                }
                self.pending.push_back(Self::frame(
                    "content_block_stop",
                    &json!({"type": "content_block_stop", "index": block_index}),
                ));
            }
        }

        let stop_reason = if stop_reason == "end_turn" && saw_tool_use {
            "tool_use"
        } else {
            stop_reason
        };

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

impl<S> ResponsesToAnthropicStream<S>
where
    S: Stream<Item = Result<Bytes, anyhow::Error>>,
{
    /// Dispatch one parsed Responses SSE event JSON payload to the
    /// appropriate state-machine transition. Split out of `poll_next` to
    /// keep the poll loop itself flat.
    fn dispatch_event(&mut self, parsed: &Value) {
        fn item_id_delta(parsed: &Value) -> Option<(&str, &str)> {
            let item_id = parsed.get("item_id").and_then(Value::as_str)?;
            let delta = parsed.get("delta").and_then(Value::as_str)?;
            Some((item_id, delta))
        }

        let event_type = parsed.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "response.output_item.added" => {
                if let Some(item) = parsed.get("item") {
                    self.open_item(item);
                }
            }
            "response.output_text.delta" => {
                if let Some((item_id, delta)) = item_id_delta(parsed) {
                    self.push_delta(item_id, ResponsesBlockKind::Text, delta);
                }
            }
            "response.function_call_arguments.delta" => {
                if let Some((item_id, delta)) = item_id_delta(parsed) {
                    self.push_delta(item_id, ResponsesBlockKind::ToolUse, delta);
                }
            }
            "response.reasoning_summary_text.delta" => {
                if let Some((item_id, delta)) = item_id_delta(parsed) {
                    self.push_delta(item_id, ResponsesBlockKind::Thinking, delta);
                }
            }
            "response.output_item.done" => {
                if let Some(item) = parsed.get("item") {
                    self.close_item(item);
                }
            }
            "response.completed" => {
                self.close("end_turn");
            }
            "response.failed" | "response.error" => {
                // Story 3.3.2: a mid-stream application-level failure —
                // surfaced as `stop_reason: "error"` rather than a silent
                // `end_turn`, so a coding-agent tool loop can tell a
                // genuine completion from a truncated one (pitfalls.md §2).
                self.close("error");
            }
            // Unrecognized/uninteresting event (e.g. `response.created`,
            // `response.function_call_arguments.done`): no-op. The
            // Responses SSE taxonomy is expected to grow.
            _ => {}
        }
    }
}

impl<S> Stream for ResponsesToAnthropicStream<S>
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
                    // Transport/SSE-parse error — distinct from a
                    // `response.failed` application-level event below
                    // (Story 3.3.2), same as `OpenaiToAnthropicStream`'s
                    // transport-error handling.
                    warn!(error = %e, "responses->anthropic stream translator: eventsource parse error");
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
                            "openai responses upstream stream chunk"
                        );
                    }
                    let Ok(parsed) = serde_json::from_str::<Value>(&event.data) else {
                        continue;
                    };
                    this.dispatch_event(&parsed);
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ────────────────────────────────────────────────────────────────────
    // Story 3.2.1: translate_anthropic_request_to_responses
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn translate_anthropic_request_to_responses_should_map_single_user_turn_to_input() {
        let anthropic = json!({
            "model": "gpt-5.3-codex",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 100,
        });

        let result = translate_anthropic_request_to_responses(anthropic);

        assert_eq!(result["model"], json!("gpt-5.3-codex"));
        assert_eq!(result["input"], json!("hi"));
        assert_eq!(result["max_output_tokens"], json!(100));
    }

    #[test]
    fn translate_anthropic_request_to_responses_should_map_multi_turn_conversation_to_input_items()
    {
        let anthropic = json!({
            "model": "gpt-5.3-codex",
            "messages": [
                {"role": "user", "content": "what's the time?"},
                {"role": "assistant", "content": "Let me check."},
                {"role": "user", "content": "thanks"},
            ],
        });

        let result = translate_anthropic_request_to_responses(anthropic);

        let input = result["input"]
            .as_array()
            .expect("multi-turn input must be an array");
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["role"], json!("user"));
        assert_eq!(input[0]["content"][0]["text"], json!("what's the time?"));
        assert_eq!(input[1]["role"], json!("assistant"));
        assert_eq!(input[1]["content"][0]["type"], json!("output_text"));
        assert_eq!(input[2]["content"][0]["text"], json!("thanks"));
    }

    #[test]
    fn translate_anthropic_request_to_responses_should_reuse_shared_block_walker_for_tool_result_block(
    ) {
        let anthropic = json!({
            "model": "gpt-5.3-codex",
            "messages": [
                {"role": "user", "content": "what's the time?"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "get_time", "input": {}},
                ]},
                {"role": "user", "content": [
                    {"type": "text", "text": "ignored alongside the tool_result"},
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "12:00 UTC"},
                ]},
            ],
        });

        let result = translate_anthropic_request_to_responses(anthropic);

        let input = result["input"].as_array().expect("input must be an array");
        let function_call = input
            .iter()
            .find(|item| item["type"] == "function_call")
            .expect("assistant tool_use block must produce a function_call item");
        assert_eq!(function_call["call_id"], json!("call_1"));
        assert_eq!(function_call["name"], json!("get_time"));

        let function_call_output = input
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .expect("tool_result block must produce a function_call_output item");
        assert_eq!(function_call_output["call_id"], json!("call_1"));
        assert_eq!(function_call_output["output"], json!("12:00 UTC"));
    }

    // ────────────────────────────────────────────────────────────────────
    // Story 3.4.1: tool_use/tool_result round trip
    // ────────────────────────────────────────────────────────────────────

    /// validation.md's exact acceptance-criterion test for Story 3.4.1: a
    /// `tool_result` block referencing a prior `tool_use` id must produce a
    /// `function_call_output` item with the matching `call_id`.
    #[test]
    fn translate_anthropic_request_to_responses_should_map_tool_result_to_function_call_output_with_matching_call_id(
    ) {
        let anthropic = json!({
            "model": "gpt-5.3-codex",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "get_time", "input": {}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "12:00 UTC"},
                ]},
            ],
        });

        let result = translate_anthropic_request_to_responses(anthropic);

        let input = result["input"].as_array().expect("input must be an array");
        let function_call_output = input
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .expect("tool_result block must produce a function_call_output item");
        assert_eq!(function_call_output["call_id"], json!("call_1"));
        assert_eq!(function_call_output["output"], json!("12:00 UTC"));
    }

    /// Task 3.4.1b: full two-turn round trip. pitfalls.md warns this bug
    /// class ("`tool_use_id`"/"`call_id`" mismatch) "only manifests on turn
    /// two" — a turn-1-only or turn-2-only test would miss a translator
    /// that silently renamed or dropped the id between the two directions.
    /// Turn 1: a Responses API response containing a `function_call` item
    /// is parsed into an Anthropic `tool_use` block via
    /// `translate_responses_response_to_anthropic`. Turn 2: that same
    /// `tool_use` block, plus a `tool_result` block referencing its id, is
    /// fed back into `translate_anthropic_request_to_responses`. The
    /// resulting `function_call_output`'s `call_id` must match the
    /// original turn-1 `call_id` end-to-end, with no manual re-typing of
    /// the id in between.
    #[test]
    fn translate_anthropic_request_to_responses_should_round_trip_call_id_across_two_turns() {
        let turn_1_response = json!({
            "id": "resp_1", "model": "gpt-5.3-codex",
            "output": [{
                "type": "function_call", "call_id": "call_1",
                "name": "get_time", "arguments": "{}",
            }],
        });
        let turn_1_anthropic = translate_responses_response_to_anthropic(turn_1_response);
        let tool_use_block = turn_1_anthropic["content"]
            .as_array()
            .and_then(|blocks| blocks.iter().find(|b| b["type"] == "tool_use"))
            .expect("turn 1 must produce a tool_use block")
            .clone();
        let original_call_id = tool_use_block["id"]
            .as_str()
            .expect("tool_use block must carry an id")
            .to_string();
        let turn_2_request = json!({
            "model": "gpt-5.3-codex",
            "messages": [
                {"role": "assistant", "content": [tool_use_block]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": original_call_id, "content": "12:00 UTC"},
                ]},
            ],
        });
        let responses_request = translate_anthropic_request_to_responses(turn_2_request);
        let input = responses_request["input"]
            .as_array()
            .expect("input must be an array");
        let function_call_output = input
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .expect("tool_result block must produce a function_call_output item");
        assert_eq!(function_call_output["call_id"], json!(original_call_id));
        assert_eq!(original_call_id, "call_1");
    }

    #[test]
    fn translate_anthropic_request_to_responses_should_map_tool_definitions_to_flat_shape() {
        let anthropic = json!({
            "model": "gpt-5.3-codex",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{
                "name": "get_time",
                "description": "Get the current time",
                "input_schema": {"type": "object", "properties": {}},
            }],
        });

        let result = translate_anthropic_request_to_responses(anthropic);

        let tools = result["tools"].as_array().expect("tools must be present");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["type"], json!("function"));
        assert_eq!(tools[0]["name"], json!("get_time"));
        assert_eq!(tools[0]["description"], json!("Get the current time"));
        assert!(
            tools[0].get("function").is_none(),
            "Responses tool shape must be flat, not nested under a `function` key"
        );
    }

    // ────────────────────────────────────────────────────────────────────
    // Story 3.2.2: translate_responses_response_to_anthropic
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn translate_responses_response_to_anthropic_should_map_message_output_item_to_text_block() {
        let responses_body = json!({
            "id": "resp_1",
            "model": "gpt-5.3-codex",
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "hello"}]}],
            "usage": {"input_tokens": 10, "output_tokens": 4},
        });

        let result = translate_responses_response_to_anthropic(responses_body);

        assert_eq!(
            result["content"],
            json!([{"type": "text", "text": "hello"}])
        );
        assert_eq!(result["stop_reason"], json!("end_turn"));
        assert_eq!(result["usage"]["input_tokens"], json!(10));
        assert_eq!(result["usage"]["output_tokens"], json!(4));
    }

    #[test]
    fn translate_responses_response_to_anthropic_should_map_function_call_item_to_tool_use_block() {
        let responses_body = json!({
            "id": "resp_1",
            "model": "gpt-5.3-codex",
            "output": [{
                "type": "function_call",
                "call_id": "call_1",
                "name": "get_time",
                "arguments": "{}",
            }],
        });

        let result = translate_responses_response_to_anthropic(responses_body);

        assert_eq!(
            result["content"],
            json!([{"type": "tool_use", "id": "call_1", "name": "get_time", "input": {}}])
        );
        assert_eq!(result["stop_reason"], json!("tool_use"));
    }

    #[test]
    fn translate_responses_response_to_anthropic_should_map_reasoning_item_to_thinking_block() {
        let responses_body = json!({
            "id": "resp_1",
            "model": "gpt-5.3-codex",
            "output": [
                {"type": "reasoning", "summary": [{"type": "summary_text", "text": "thinking it through"}]},
                {"type": "message", "content": [{"type": "output_text", "text": "done"}]},
            ],
        });

        let result = translate_responses_response_to_anthropic(responses_body);

        let content = result["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], json!("thinking"));
        assert_eq!(content[0]["thinking"], json!("thinking it through"));
        assert_eq!(content[1]["type"], json!("text"));
    }

    // ────────────────────────────────────────────────────────────────────
    // Story 3.5.1: reasoning-item -> `thinking` block signature contract
    // ────────────────────────────────────────────────────────────────────

    #[test]
    fn translate_responses_response_to_anthropic_should_map_reasoning_item_to_thinking_block_with_signature(
    ) {
        let responses_body = json!({
            "id": "resp_1",
            "model": "gpt-5.3-codex",
            "output": [{
                "type": "reasoning",
                "id": "reasoning_1",
                "summary": [{"type": "summary_text", "text": "Let me think..."}],
            }],
        });

        let result = translate_responses_response_to_anthropic(responses_body);

        let content = result["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], json!("thinking"));
        assert_eq!(content[0]["thinking"], json!("Let me think..."));
        let signature = content[0]["signature"]
            .as_str()
            .expect("thinking block must carry a string signature");
        assert!(
            !signature.is_empty(),
            "signature must be non-empty per Anthropic's thinking-block contract"
        );
    }

    #[test]
    fn translate_responses_response_to_anthropic_should_prefer_encrypted_content_over_id_for_reasoning_signature(
    ) {
        let responses_body = json!({
            "id": "resp_1",
            "model": "gpt-5.3-codex",
            "output": [{
                "type": "reasoning",
                "id": "reasoning_1",
                "encrypted_content": "opaque-blob-abc123",
                "summary": [{"type": "summary_text", "text": "Let me think..."}],
            }],
        });

        let result = translate_responses_response_to_anthropic(responses_body);

        let content = result["content"].as_array().unwrap();
        let signature = content[0]["signature"].as_str().unwrap();
        assert!(
            signature.contains("opaque-blob-abc123"),
            "signature should be derived from encrypted_content when present, got {signature}"
        );
    }

    #[test]
    fn translate_responses_response_to_anthropic_should_skip_unknown_output_item_type_without_panicking(
    ) {
        let responses_body = json!({
            "id": "resp_1",
            "model": "gpt-5.3-codex",
            "output": [
                {"type": "some_future_item_type", "whatever": "shape"},
                {"type": "message", "content": [{"type": "output_text", "text": "hello"}]},
            ],
        });

        let result = translate_responses_response_to_anthropic(responses_body);

        assert_eq!(
            result["content"],
            json!([{"type": "text", "text": "hello"}])
        );
    }

    #[test]
    fn translate_responses_response_to_anthropic_should_emit_empty_text_block_when_output_is_empty()
    {
        let responses_body = json!({"id": "resp_1", "model": "gpt-5.3-codex", "output": []});

        let result = translate_responses_response_to_anthropic(responses_body);

        assert_eq!(result["content"], json!([{"type": "text", "text": ""}]));
    }

    // ────────────────────────────────────────────────────────────────────
    // ResponsesToAnthropicStream (Epic 3.3)
    // ────────────────────────────────────────────────────────────────────

    mod stream_translator {
        use super::*;
        use futures_util::{stream, StreamExt};

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

        /// Story 3.3.1's first acceptance criterion, hand-constructed
        /// (synthetic, not a real capture — Epic 6.1 owns real fixtures):
        /// `response.created` -> `response.output_item.added` (message) ->
        /// `response.output_text.delta` x3 -> `response.output_item.done` ->
        /// `response.completed` must produce `message_start` ->
        /// `content_block_start` -> `content_block_delta` x3 ->
        /// `content_block_stop` -> `message_delta` (`end_turn`) ->
        /// `message_stop`.
        #[tokio::test]
        async fn responses_to_anthropic_stream_should_emit_content_block_delta_sequence_for_message_item(
        ) {
            let inner = stream::iter(vec![
                Ok(sse(r#"{"type":"response.created"}"#)),
                Ok(sse(
                    r#"{"type":"response.output_item.added","item":{"id":"item_1","type":"message","role":"assistant","content":[]}}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.output_text.delta","item_id":"item_1","delta":"Hel"}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.output_text.delta","item_id":"item_1","delta":"lo,"}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.output_text.delta","item_id":"item_1","delta":" world"}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.output_item.done","item":{"id":"item_1","type":"message"}}"#,
                )),
                Ok(sse(r#"{"type":"response.completed","response":{}}"#)),
            ]);
            let translator = ResponsesToAnthropicStream::new(inner, "gpt-5.3-codex".to_string());
            let out = drain(translator).await;
            let events: Vec<(String, Value)> = out.iter().map(parse_event).collect();
            let kinds: Vec<&str> = events.iter().map(|(e, _)| e.as_str()).collect();

            assert_eq!(
                kinds,
                vec![
                    "message_start",
                    "content_block_start",
                    "content_block_delta",
                    "content_block_delta",
                    "content_block_delta",
                    "content_block_stop",
                    "message_delta",
                    "message_stop",
                ]
            );

            let texts: Vec<String> = events
                .iter()
                .filter(|(e, _)| e == "content_block_delta")
                .map(|(_, d)| d["delta"]["text"].as_str().unwrap_or("").to_string())
                .collect();
            assert_eq!(texts, vec!["Hel", "lo,", " world"]);

            let (_, message_delta) = &events[6];
            assert_eq!(message_delta["delta"]["stop_reason"], "end_turn");
        }

        /// Story 3.3.1's second acceptance criterion: a `reasoning` item's
        /// deltas interleaved between two independently-`added`
        /// `function_call` items' deltas must attribute every delta to its
        /// own `item_id`-derived block index with zero cross-item mixing.
        #[tokio::test]
        async fn responses_to_anthropic_stream_should_not_mix_content_across_interleaved_item_ids()
        {
            let inner = stream::iter(vec![
                Ok(sse(
                    r#"{"type":"response.output_item.added","item":{"id":"item_call_1","type":"function_call","call_id":"call_1","name":"get_weather"}}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.output_item.added","item":{"id":"item_call_2","type":"function_call","call_id":"call_2","name":"get_time"}}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.output_item.added","item":{"id":"item_reasoning","type":"reasoning","summary":[]}}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.function_call_arguments.delta","item_id":"item_call_1","delta":"{\"a\":"}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.reasoning_summary_text.delta","item_id":"item_reasoning","delta":"Thinking A"}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.function_call_arguments.delta","item_id":"item_call_2","delta":"{\"b\":"}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.reasoning_summary_text.delta","item_id":"item_reasoning","delta":" more"}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.function_call_arguments.delta","item_id":"item_call_1","delta":"1}"}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.output_item.done","item":{"id":"item_call_1","type":"function_call"}}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.function_call_arguments.delta","item_id":"item_call_2","delta":"2}"}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.output_item.done","item":{"id":"item_call_2","type":"function_call"}}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.output_item.done","item":{"id":"item_reasoning","type":"reasoning"}}"#,
                )),
                Ok(sse(r#"{"type":"response.completed","response":{}}"#)),
            ]);
            let translator = ResponsesToAnthropicStream::new(inner, "gpt-5.3-codex".to_string());
            let out = drain(translator).await;
            let events: Vec<(String, Value)> = out.iter().map(parse_event).collect();

            let starts: Vec<&Value> = events
                .iter()
                .filter(|(e, _)| e == "content_block_start")
                .map(|(_, d)| d)
                .collect();
            assert_eq!(starts.len(), 3);
            // Appearance order: call_1 -> index 0, call_2 -> index 1,
            // reasoning -> index 2.
            assert_eq!(starts[0]["index"], 0);
            assert_eq!(starts[0]["content_block"]["type"], "tool_use");
            assert_eq!(starts[0]["content_block"]["id"], "call_1");
            assert_eq!(starts[1]["index"], 1);
            assert_eq!(starts[1]["content_block"]["id"], "call_2");
            assert_eq!(starts[2]["index"], 2);
            assert_eq!(starts[2]["content_block"]["type"], "thinking");

            let mut by_index: std::collections::HashMap<u64, String> =
                std::collections::HashMap::new();
            for (event, data) in &events {
                if event != "content_block_delta" {
                    continue;
                }
                let index = data["index"].as_u64().unwrap();
                let fragment = data["delta"]["partial_json"]
                    .as_str()
                    .or_else(|| data["delta"]["thinking"].as_str())
                    .unwrap_or("");
                by_index.entry(index).or_default().push_str(fragment);
            }
            assert_eq!(by_index.get(&0).map(String::as_str), Some(r#"{"a":1}"#));
            assert_eq!(by_index.get(&1).map(String::as_str), Some(r#"{"b":2}"#));
            assert_eq!(
                by_index.get(&2).map(String::as_str),
                Some("Thinking A more")
            );

            let stops: Vec<u64> = events
                .iter()
                .filter(|(e, _)| e == "content_block_stop")
                .map(|(_, d)| d["index"].as_u64().unwrap())
                .collect();
            assert_eq!(stops, vec![0, 1, 2]);
        }

        /// Story 3.3.2: a `response.failed` event mid-stream must produce a
        /// terminal event distinguishable from normal `end_turn`
        /// completion — chosen signal: `message_delta` with `stop_reason:
        /// "error"` (documented on `ResponsesToAnthropicStream::close`).
        #[tokio::test]
        async fn responses_to_anthropic_stream_should_emit_distinguishable_error_state_when_response_failed_event_arrives(
        ) {
            let inner = stream::iter(vec![
                Ok(sse(
                    r#"{"type":"response.output_item.added","item":{"id":"item_1","type":"message","role":"assistant","content":[]}}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.output_text.delta","item_id":"item_1","delta":"partial"}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.failed","response":{"error":{"message":"content policy violation"}}}"#,
                )),
            ]);
            let translator = ResponsesToAnthropicStream::new(inner, "gpt-5.3-codex".to_string());
            let out = drain(translator).await;
            let events: Vec<(String, Value)> = out.iter().map(parse_event).collect();

            let (_, message_delta) = events
                .iter()
                .find(|(e, _)| e == "message_delta")
                .expect("message_delta must be emitted");
            assert_eq!(message_delta["delta"]["stop_reason"], "error");
            assert_ne!(message_delta["delta"]["stop_reason"], "end_turn");

            assert_eq!(events.last().unwrap().0, "message_stop");

            // The in-progress text block must still be cleanly closed
            // before the terminal events, not left dangling.
            assert!(events
                .iter()
                .any(|(e, d)| e == "content_block_stop" && d["index"] == 0));
        }

        /// Task 3.4.2a: integration test chaining Story 3.3.1's streaming
        /// output into Story 3.4.1's request-building input. A streamed
        /// `function_call` item completing with `call_id: "call_2"` must
        /// produce a `tool_use` block whose `id` survives, unchanged, into
        /// a next-turn `translate_anthropic_request_to_responses` call as
        /// the matching `function_call_output`'s `call_id` — the same bug
        /// class as the non-streaming round trip above, but sourced from
        /// the streaming translator's `content_block_start` event instead
        /// of the non-streaming response body.
        #[tokio::test]
        async fn responses_tool_use_round_trip_should_preserve_call_id_across_stream_then_next_turn_translation(
        ) {
            let inner = stream::iter(vec![
                Ok(sse(
                    r#"{"type":"response.output_item.added","item":{"id":"item_call_2","type":"function_call","call_id":"call_2","name":"get_time"}}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.function_call_arguments.delta","item_id":"item_call_2","delta":"{}"}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.output_item.done","item":{"id":"item_call_2","type":"function_call"}}"#,
                )),
                Ok(sse(r#"{"type":"response.completed","response":{}}"#)),
            ]);
            let translator = ResponsesToAnthropicStream::new(inner, "gpt-5.3-codex".to_string());
            let out = drain(translator).await;
            let events: Vec<(String, Value)> = out.iter().map(parse_event).collect();

            let (_, block_start) = events
                .iter()
                .find(|(e, d)| {
                    e == "content_block_start" && d["content_block"]["type"] == "tool_use"
                })
                .expect("streamed function_call item must start a tool_use block");
            let streamed_call_id = block_start["content_block"]["id"]
                .as_str()
                .expect("tool_use block must carry an id")
                .to_string();
            assert_eq!(streamed_call_id, "call_2");

            let tool_use_block = json!({
                "type": "tool_use",
                "id": streamed_call_id,
                "name": block_start["content_block"]["name"],
                "input": {},
            });
            let turn_2_request = json!({
                "model": "gpt-5.3-codex",
                "messages": [
                    {"role": "assistant", "content": [tool_use_block]},
                    {"role": "user", "content": [
                        {"type": "tool_result", "tool_use_id": streamed_call_id, "content": "12:00 UTC"},
                    ]},
                ],
            });
            let responses_request = translate_anthropic_request_to_responses(turn_2_request);

            let input = responses_request["input"]
                .as_array()
                .expect("input must be an array");
            let function_call_output = input
                .iter()
                .find(|item| item["type"] == "function_call_output")
                .expect("tool_result block must produce a function_call_output item");
            assert_eq!(function_call_output["call_id"], json!(streamed_call_id));
        }

        /// Story 3.5.1's streaming acceptance criterion: a streamed
        /// `reasoning` item's added/delta/done lifecycle must produce a
        /// `thinking`-typed block (not mislabeled `text`, per pitfalls.md
        /// §2) whose signature is finalized — via a `signature_delta` —
        /// just before `content_block_stop`, mirroring how Anthropic's own
        /// extended-thinking streaming finalizes a block's signature at the
        /// end rather than the start.
        #[tokio::test]
        async fn responses_to_anthropic_stream_should_emit_thinking_typed_block_for_streamed_reasoning_item(
        ) {
            let inner = stream::iter(vec![
                Ok(sse(
                    r#"{"type":"response.output_item.added","item":{"id":"item_reasoning","type":"reasoning","summary":[]}}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.reasoning_summary_text.delta","item_id":"item_reasoning","delta":"Let me think"}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.reasoning_summary_text.delta","item_id":"item_reasoning","delta":"..."}"#,
                )),
                Ok(sse(
                    r#"{"type":"response.output_item.done","item":{"id":"item_reasoning","type":"reasoning","encrypted_content":"enc_abc"}}"#,
                )),
                Ok(sse(r#"{"type":"response.completed","response":{}}"#)),
            ]);
            let translator = ResponsesToAnthropicStream::new(inner, "gpt-5.3-codex".to_string());
            let out = drain(translator).await;
            let events: Vec<(String, Value)> = out.iter().map(parse_event).collect();

            let (_, block_start) = events
                .iter()
                .find(|(e, _)| e == "content_block_start")
                .expect("reasoning item must open a content block");
            assert_eq!(block_start["content_block"]["type"], json!("thinking"));
            assert_ne!(block_start["content_block"]["type"], json!("text"));

            let thinking_deltas: Vec<&str> = events
                .iter()
                .filter(|(e, d)| {
                    e == "content_block_delta" && d["delta"]["type"] == "thinking_delta"
                })
                .map(|(_, d)| d["delta"]["thinking"].as_str().unwrap_or(""))
                .collect();
            assert_eq!(thinking_deltas, vec!["Let me think", "..."]);

            let (_, signature_delta) = events
                .iter()
                .find(|(e, d)| {
                    e == "content_block_delta" && d["delta"]["type"] == "signature_delta"
                })
                .expect("thinking block must finalize a signature_delta before closing");
            let signature = signature_delta["delta"]["signature"]
                .as_str()
                .expect("signature_delta must carry a string signature");
            assert!(!signature.is_empty());
            assert!(
                signature.contains("enc_abc"),
                "signature should be derived from the done item's encrypted_content, got {signature}"
            );

            // The signature_delta must land before content_block_stop, not
            // after — signatures finalize at the end of the block.
            let signature_delta_pos = events
                .iter()
                .position(|(e, d)| {
                    e == "content_block_delta" && d["delta"]["type"] == "signature_delta"
                })
                .unwrap();
            let stop_pos = events
                .iter()
                .position(|(e, _)| e == "content_block_stop")
                .unwrap();
            assert!(signature_delta_pos < stop_pos);
        }

        // ────────────────────────────────────────────────────────────────
        // Story 6.1.2: real (redacted) Responses API SSE transcripts
        // ────────────────────────────────────────────────────────────────
        //
        // Every test above feeds a hand-constructed `stream::iter` of
        // pre-chunked `Bytes` straight into `ResponsesToAnthropicStream`,
        // bypassing the real `reqwest`/`eventsource_stream` wire path
        // (pitfalls.md §5's flagged gap — the same gap already present in
        // `OpenaiToAnthropicStream`'s own Chat Completions tests, which this
        // project inherits rather than fixes). The tests below instead spin
        // up a bare `tokio::net::TcpListener`, write a transcript's raw
        // bytes across several separate socket writes (forcing an SSE event
        // to split across two TCP reads), and drive the translator from a
        // real `reqwest::Response::bytes_stream()` — the same code path
        // `OpenaiProvider::send` uses in production.
        //
        // Fixture provenance (tests/fixtures/openai_responses_api/*.sse):
        // this sandbox has no SBN Dev Agent/VPN access to capture a real
        // ExampleCorp Model Gateway transcript (same gap flagged for Task
        // 1.1.2c), so per Task 6.1.2a's fallback these are NOT real
        // captures. Every event's field set (names, required-vs-optional,
        // `sequence_number`/`output_index`/`content_index` bookkeeping) is
        // copied from OpenAI's own public, MIT-licensed `openai-python` SDK
        // type stubs (github.com/openai/openai-python,
        // `src/openai/types/responses/response_*_event.py`, e.g.
        // `ResponseTextDeltaEvent`, `ResponseFunctionCallArgumentsDeltaEvent`,
        // `ResponseReasoningSummaryTextDeltaEvent`), not invented from
        // scratch or guessed from prose docs. That is a real, citable
        // source for the wire *shape* — but the specific ids/text/ordering
        // here are still synthetic, not a genuine captured trace. Be honest
        // about that distinction: these are realistic-shaped fixtures, not
        // a substitute for a real capture.
        #[allow(
            clippy::unwrap_used,
            clippy::expect_used,
            clippy::items_after_statements
        )]
        mod real_wire_fixtures {
            use super::*;
            use std::time::Duration;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            use tokio::net::TcpListener;

            /// Serve `body`'s bytes as a single HTTP response, split across
            /// several separate socket writes (not one `write_all`) so a
            /// real client reads it in multiple chunks — exercising
            /// `eventsource_stream`'s cross-read buffering rather than a
            /// hand-fed, pre-chunked `Bytes` vec.
            async fn serve_once_in_chunks(body: &'static str) -> String {
                let listener = TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("mock server bind should succeed");
                let addr = listener
                    .local_addr()
                    .expect("mock server local_addr should succeed");

                tokio::spawn(async move {
                    let (mut socket, _) = listener
                        .accept()
                        .await
                        .expect("mock server accept should succeed");

                    // Drain the request (headers only, no body expected)
                    // before writing a response.
                    let mut buf = [0u8; 4096];
                    loop {
                        let n = socket
                            .read(&mut buf)
                            .await
                            .expect("mock server read should succeed");
                        if n == 0 || buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }

                    let header = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    socket
                        .write_all(header.as_bytes())
                        .await
                        .expect("mock server header write should succeed");

                    // Small, line-boundary-blind chunks so an SSE event's
                    // `data:` line can land split across two writes/reads.
                    const CHUNK: usize = 37;
                    for piece in body.as_bytes().chunks(CHUNK) {
                        socket
                            .write_all(piece)
                            .await
                            .expect("mock server chunk write should succeed");
                        socket
                            .flush()
                            .await
                            .expect("mock server flush should succeed");
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                    let _ = socket.shutdown().await;
                });

                format!("http://{addr}")
            }

            async fn drain_real(url: String, model: &str) -> Vec<(String, Value)> {
                let response = reqwest::Client::new()
                    .get(url)
                    .send()
                    .await
                    .expect("real HTTP GET against the mock server should succeed");
                let byte_stream = response
                    .bytes_stream()
                    .map(|r| r.map_err(anyhow::Error::from));
                let translator = ResponsesToAnthropicStream::new(byte_stream, model.to_string());
                let out = drain(translator).await;
                out.iter().map(parse_event).collect()
            }

            /// Task 6.1.2a/b: plain-text streaming transcript, fed through a
            /// real socket rather than a hand-fed `Bytes` vec.
            #[tokio::test]
            async fn responses_to_anthropic_stream_should_handle_real_wire_text_transcript_over_tcp(
            ) {
                let fixture =
                    include_str!("../../../tests/fixtures/openai_responses_api/text_stream.sse");
                let url = serve_once_in_chunks(fixture).await;
                let events = drain_real(url, "gpt-4.1-mini").await;
                let kinds: Vec<&str> = events.iter().map(|(e, _)| e.as_str()).collect();

                assert_eq!(
                    kinds,
                    vec![
                        "message_start",
                        "content_block_start",
                        "content_block_delta",
                        "content_block_delta",
                        "content_block_delta",
                        "content_block_stop",
                        "message_delta",
                        "message_stop",
                    ]
                );
                let texts: Vec<String> = events
                    .iter()
                    .filter(|(e, _)| e == "content_block_delta")
                    .map(|(_, d)| d["delta"]["text"].as_str().unwrap_or("").to_string())
                    .collect();
                assert_eq!(texts, vec!["Hel", "lo,", " world"]);
                let (_, message_delta) = events
                    .iter()
                    .find(|(e, _)| e == "message_delta")
                    .expect("message_delta must be emitted");
                assert_eq!(message_delta["delta"]["stop_reason"], "end_turn");
            }

            /// Task 6.1.2c: tool-call streaming transcript, likewise fed
            /// through a real socket.
            #[tokio::test]
            async fn responses_to_anthropic_stream_should_handle_real_wire_tool_call_transcript_over_tcp(
            ) {
                let fixture = include_str!(
                    "../../../tests/fixtures/openai_responses_api/tool_call_stream.sse"
                );
                let url = serve_once_in_chunks(fixture).await;
                let events = drain_real(url, "gpt-4.1-mini").await;

                let (_, block_start) = events
                    .iter()
                    .find(|(e, d)| {
                        e == "content_block_start" && d["content_block"]["type"] == "tool_use"
                    })
                    .expect("function_call item must open a tool_use block");
                assert_eq!(block_start["content_block"]["name"], "get_weather");

                let partial_json: String = events
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
                assert_eq!(partial_json, r#"{"city":"San Francisco"}"#);
                serde_json::from_str::<Value>(&partial_json)
                    .expect("reassembled arguments must parse as valid JSON");

                let (_, message_delta) = events
                    .iter()
                    .find(|(e, _)| e == "message_delta")
                    .expect("message_delta must be emitted");
                assert_eq!(message_delta["delta"]["stop_reason"], "tool_use");
            }

            /// Task 6.1.2c (nice-to-have, not skipped): reasoning-item
            /// streaming transcript. `OpenAI`'s public `openai-python` SDK
            /// stubs document this event family
            /// (`ResponseReasoningSummaryTextDeltaEvent` et al.) clearly
            /// enough to build a faithful fixture, so this one is included.
            #[tokio::test]
            async fn responses_to_anthropic_stream_should_handle_real_wire_reasoning_transcript_over_tcp(
            ) {
                let fixture = include_str!(
                    "../../../tests/fixtures/openai_responses_api/reasoning_stream.sse"
                );
                let url = serve_once_in_chunks(fixture).await;
                let events = drain_real(url, "gpt-4.1-mini").await;

                let (_, block_start) = events
                    .iter()
                    .find(|(e, _)| e == "content_block_start")
                    .expect("reasoning item must open a content block");
                assert_eq!(block_start["content_block"]["type"], json!("thinking"));

                let thinking: String = events
                    .iter()
                    .filter(|(e, d)| {
                        e == "content_block_delta" && d["delta"]["type"] == "thinking_delta"
                    })
                    .map(|(_, d)| d["delta"]["thinking"].as_str().unwrap_or(""))
                    .collect();
                assert_eq!(thinking, "Let me think about this...");

                let (_, message_delta) = events
                    .iter()
                    .find(|(e, _)| e == "message_delta")
                    .expect("message_delta must be emitted");
                assert_eq!(message_delta["delta"]["stop_reason"], "end_turn");
            }
        }
    }
}
