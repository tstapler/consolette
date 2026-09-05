//! `GeminiToAnthropicStream`: translates a Cloud Code Assist SSE byte stream
//! (`v1internal:streamGenerateContent?alt=sse`) into an Anthropic Messages
//! SSE event stream (Story 2.1.1).
//!
//! Structurally mirrors `OpenaiToAnthropicStream`
//! (`src/providers/openai.rs:369-535`) — same `VecDeque<Bytes>` buffering
//! pattern and Anthropic bracketing shape (`message_start` /
//! `content_block_start` / `content_block_delta` / `content_block_stop` /
//! `message_delta` / `message_stop`) — but tracks **multiple** indexed
//! content blocks instead of a single hardcoded index-0 text block, since
//! Gemini/Cloud-Code streaming can interleave text + (Phase 3)
//! `functionCall` parts within one candidate's accumulating `parts[]` (see
//! plan.md's Domain Glossary and the "Streaming reconstruction" Pattern
//! Decisions row).
//!
//! Each SSE `data:` line's JSON wraps the native Gemini response in an extra
//! `"response"` layer: `{"response": {"candidates": [...], "usageMetadata":
//! {...}}}` — unwrapped before walking `candidates[0].content.parts[]`.
//!
//! Unparseable-chunk handling (fail-closed, mirroring `bedrock.rs`'s
//! streaming precedent at `src/providers/bedrock.rs:719-735`) is Epic 2.2's
//! job, not this story's — an unparseable `data:` line is skipped for now,
//! matching `OpenaiToAnthropicStream`'s existing `continue`-on-parse-failure
//! behavior.

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use eventsource_stream::Eventsource;
use futures_core::Stream;
use serde_json::{json, Value};
use tracing::warn;

use super::translate::{map_gemini_finish_reason, GeminiUsageMetadata};

/// Which Anthropic content-block type a Gemini part maps to. Only `Text`
/// exists through Phase 2 — Phase 3 adds a `FunctionCall` variant once tool
/// calls land (see plan.md's Domain Glossary `GeminiPart` entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlockKind {
    Text,
    /// Test-only stand-in for a second real variant (not added until Phase
    /// 3's `FunctionCall`). Exists solely so
    /// `gemini_to_anthropic_stream_should_open_new_indexed_block_when_part_type_changes`
    /// can prove `resolve_block_index`'s type-change branch generically
    /// opens a *new* index — the "new Gemini-part-position" branch is
    /// already exercised by `Text`-only production code on its own, but the
    /// "an already-tracked position's kind changes" branch has no real
    /// second variant to trigger it with until Phase 3.
    #[cfg(test)]
    TestOther,
}

impl BlockKind {
    fn anthropic_block_json(self) -> Value {
        match self {
            BlockKind::Text => json!({"type": "text", "text": ""}),
            #[cfg(test)]
            BlockKind::TestOther => json!({"type": "test_other"}),
        }
    }
}

/// Classifies a raw Gemini `parts[]` entry's `BlockKind`. Only `Text` exists
/// through Phase 2 — Phase 3 adds `functionCall` detection here.
fn classify_part_kind(_part: &Value) -> BlockKind {
    BlockKind::Text
}

/// Translates a Cloud Code Assist `streamGenerateContent` SSE byte stream
/// into an Anthropic Messages SSE event stream.
///
/// `active_blocks[i]` tracks the `BlockKind` of the Anthropic content block
/// opened for Gemini part-position `i` — instead of
/// `OpenaiToAnthropicStream`'s single implicit index-0 assumption, a new
/// Gemini part-position (or a part whose kind changes) gets its own,
/// newly-appended index rather than corrupting/overwriting an existing one.
pub(crate) struct GeminiToAnthropicStream<S> {
    inner: eventsource_stream::EventStream<S>,
    id: String,
    model: String,
    started: bool,
    finished: bool,
    done: bool,
    active_blocks: Vec<BlockKind>,
    pending: VecDeque<Bytes>,
}

impl<S> GeminiToAnthropicStream<S> {
    fn frame(event: &str, data: &Value) -> Bytes {
        Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
    }

    /// Push the synthetic `message_start` event, if not already emitted, so
    /// every stream opens with a well-formed Anthropic preamble even if the
    /// first Gemini chunk carries no parts yet.
    fn ensure_started(&mut self) {
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

    /// Resolves the Anthropic block index a Gemini part at `part_index`
    /// (with the given `kind`) should emit into. If `part_index` has no
    /// tracked block yet, or its tracked kind differs from `kind`, opens a
    /// **new**, appended index (never reusing/overwriting an existing one)
    /// and emits its `content_block_start` event. Otherwise reuses the
    /// existing index for `part_index`.
    fn resolve_block_index(&mut self, part_index: usize, kind: BlockKind) -> usize {
        self.ensure_started();
        if let Some(existing_kind) = self.active_blocks.get(part_index) {
            if *existing_kind == kind {
                return part_index;
            }
        }
        let new_index = self.active_blocks.len();
        self.active_blocks.push(kind);
        self.pending.push_back(Self::frame(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": new_index,
                "content_block": kind.anthropic_block_json(),
            }),
        ));
        new_index
    }

    fn push_delta(&mut self, index: usize, text: &str) {
        self.pending.push_back(Self::frame(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": index,
                "delta": {"type": "text_delta", "text": text}
            }),
        ));
    }

    /// Emits `content_block_stop` for every open block, then
    /// `message_delta`/`message_stop`, and marks the stream finished.
    /// Idempotent.
    fn close(&mut self, stop_reason: &str, usage: &Value) {
        if self.finished {
            return;
        }
        self.ensure_started();
        self.finished = true;
        for index in 0..self.active_blocks.len() {
            self.pending.push_back(Self::frame(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": index}),
            ));
        }
        self.pending.push_back(Self::frame(
            "message_delta",
            &json!({
                "type": "message_delta",
                "delta": {"stop_reason": stop_reason},
                "usage": usage
            }),
        ));
        self.pending.push_back(Self::frame(
            "message_stop",
            &json!({"type": "message_stop"}),
        ));
    }
}

impl<S> GeminiToAnthropicStream<S>
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
            active_blocks: Vec::new(),
            pending: VecDeque::new(),
        }
    }
}

/// Maps a `GeminiUsageMetadata` (already parsed off the chunk's
/// `usageMetadata` field, reusing Story 1.3.2's wire struct) onto the
/// `message_delta` event's `usage` object.
fn usage_delta_json(usage_metadata: Option<&GeminiUsageMetadata>) -> Value {
    usage_metadata.map_or_else(
        || json!({"input_tokens": 0, "output_tokens": 0}),
        |u| {
            json!({
                "input_tokens": u.prompt_token_count,
                "output_tokens": u.candidates_token_count,
            })
        },
    )
}

impl<S> Stream for GeminiToAnthropicStream<S>
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
                    this.close("end_turn", &usage_delta_json(None));
                }
                Poll::Ready(Some(Err(e))) => {
                    warn!(error = %e, "gemini->anthropic stream translator: eventsource parse error");
                    this.close("end_turn", &usage_delta_json(None));
                }
                Poll::Ready(Some(Ok(event))) => {
                    let Ok(parsed) = serde_json::from_str::<Value>(&event.data) else {
                        // Unparseable data line: skipped for now (Epic 2.2
                        // makes this fail-closed instead — see module docs).
                        continue;
                    };
                    let response_body = parsed.get("response").unwrap_or(&parsed);

                    let candidate = response_body
                        .get("candidates")
                        .and_then(Value::as_array)
                        .and_then(|a| a.first());

                    let Some(candidate) = candidate else {
                        continue;
                    };

                    if let Some(parts) = candidate
                        .get("content")
                        .and_then(|c| c.get("parts"))
                        .and_then(Value::as_array)
                    {
                        for (part_index, part) in parts.iter().enumerate() {
                            let kind = classify_part_kind(part);
                            let index = this.resolve_block_index(part_index, kind);
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                if !text.is_empty() {
                                    this.push_delta(index, text);
                                }
                            }
                        }
                    }

                    if let Some(reason) = candidate.get("finishReason").and_then(Value::as_str) {
                        let (stop_reason, synthesized) = map_gemini_finish_reason(Some(reason));
                        if let Some(text) = synthesized {
                            let index =
                                this.resolve_block_index(this.active_blocks.len(), BlockKind::Text);
                            this.push_delta(index, &text);
                        }
                        let usage_metadata = response_body
                            .get("usageMetadata")
                            .and_then(|v| serde_json::from_value::<GeminiUsageMetadata>(v.clone()).ok());
                        this.close(stop_reason, &usage_delta_json(usage_metadata.as_ref()));
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
    use futures_util::stream::{self, StreamExt};

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

    async fn drain<S>(s: S) -> Vec<Bytes>
    where
        S: Stream<Item = Result<Bytes, anyhow::Error>>,
    {
        s.map(|item| item.unwrap()).collect().await
    }

    // REQ-18 — happy path: a single text block's bracketing sequence matches
    // `OpenaiToAnthropicStream`'s existing single-block shape.
    #[tokio::test]
    async fn gemini_to_anthropic_stream_should_emit_correct_bracketing_sequence_for_single_text_block(
    ) {
        let inner = stream::iter(vec![
            Ok(sse(
                r#"{"response":{"candidates":[{"content":{"parts":[{"text":"Hel"}]}}]}}"#,
            )),
            Ok(sse(
                r#"{"response":{"candidates":[{"content":{"parts":[{"text":"lo"}]},"finishReason":"STOP"}],"usageMetadata":{"promptTokenCount":5,"candidatesTokenCount":2,"totalTokenCount":7}}}"#,
            )),
        ]);
        let translator = GeminiToAnthropicStream::new(inner, "gemini-3-pro".to_string());
        let out = drain(translator).await;

        let events: Vec<String> = out.iter().map(|f| parse_event(f).0).collect();
        assert_eq!(
            events,
            vec![
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop",
            ]
        );

        let (_, block_start) = parse_event(&out[1]);
        assert_eq!(block_start["index"], 0);
        assert_eq!(block_start["content_block"]["type"], "text");

        let (_, delta1) = parse_event(&out[2]);
        assert_eq!(delta1["delta"]["text"], "Hel");
        let (_, delta2) = parse_event(&out[3]);
        assert_eq!(delta2["delta"]["text"], "lo");

        let (_, block_stop) = parse_event(&out[4]);
        assert_eq!(block_stop["index"], 0);

        let (_, message_delta) = parse_event(&out[5]);
        assert_eq!(message_delta["delta"]["stop_reason"], "end_turn");
        assert_eq!(message_delta["usage"]["input_tokens"], 5);
        assert_eq!(message_delta["usage"]["output_tokens"], 2);
    }

    // REQ-18 — edge path: proves `resolve_block_index`'s type-change branch
    // generically opens a *new*, appended index rather than
    // reusing/overwriting an existing one. Exercised as a white-box unit
    // test directly against `resolve_block_index` (rather than through a
    // full SSE fixture) because only `BlockKind::Text` exists as a real
    // variant through Phase 2 — a `#[cfg(test)]`-only second variant
    // (`BlockKind::TestOther`) stands in for the real second variant Phase
    // 3 adds, so the *mechanism* (not a specific Phase-3 kind) is what's
    // proven here. See `BlockKind::TestOther`'s doc comment for why this
    // approach was chosen over a synthetic-JSON end-to-end test.
    #[test]
    fn gemini_to_anthropic_stream_should_open_new_indexed_block_when_part_type_changes() {
        let inner = stream::iter(Vec::<Result<Bytes, anyhow::Error>>::new());
        let mut translator = GeminiToAnthropicStream::new(inner, "gemini-3-pro".to_string());

        // First time part-position 0 is seen: opens index 0.
        let first_index = translator.resolve_block_index(0, BlockKind::Text);
        assert_eq!(first_index, 0);

        // Same position, unchanged kind: reuses index 0 — not a new block.
        let same_index = translator.resolve_block_index(0, BlockKind::Text);
        assert_eq!(same_index, 0);

        // Position 0's kind changes: a brand-new index is opened, never
        // reusing or silently merging into index 0.
        let changed_index = translator.resolve_block_index(0, BlockKind::TestOther);
        assert_eq!(changed_index, 1);
        assert_eq!(
            translator.active_blocks,
            vec![BlockKind::Text, BlockKind::TestOther]
        );

        // A brand-new part-position (never seen before) also always opens
        // its own new index, exercising the same branch with real Phase-2
        // types only.
        let new_position_index = translator.resolve_block_index(5, BlockKind::Text);
        assert_eq!(new_position_index, 2);
    }

    #[tokio::test]
    async fn empty_stream_still_produces_well_formed_bracketing_events() {
        let inner = stream::iter(Vec::<Result<Bytes, anyhow::Error>>::new());
        let translator = GeminiToAnthropicStream::new(inner, "gemini-3-pro".to_string());
        let out = drain(translator).await;

        let events: Vec<String> = out.iter().map(|f| parse_event(f).0).collect();
        assert_eq!(
            events,
            vec!["message_start", "message_delta", "message_stop"]
        );
    }
}
