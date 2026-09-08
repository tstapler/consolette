# Research: Feature Landscape — `gemini-provider`

Research Agent 2 (Features), SDD Phase 2. Scope: what translation work already
exists in this codebase, what Gemini's documented content schema looks like,
edge cases the existing translators don't handle, and unstated needs behind
the explicit requirements.

## 1. Existing Anthropic ↔ other-vendor translation in this codebase

**Key finding: there is much less precedent here than the research question
assumed.** Neither existing non-Anthropic provider does a full schema
translation of tool calls, images, or thinking blocks. `GeminiProvider` will
have to build most of that translation logic net-new, not adapt an existing
pattern.

### `src/providers/bedrock.rs` (1062 lines) — NOT a schema translator

Contrary to the "Converse API" framing in the research brief, `BedrockProvider`
calls `invoke_model`/`invoke_model_with_response_stream` (the raw
model-invoke API), not Bedrock's higher-level Converse API. Because Bedrock's
Anthropic models accept Anthropic's own Messages JSON almost verbatim, this
provider does **compatibility filtering**, not format translation:

- `normalize_model`/`to_bedrock_model_id` ([bedrock.rs:257-303](../../../src/providers/bedrock.rs#L257-L303)): strip `us./eu./ap.` region prefix, `anthropic.` vendor prefix, `-v1:0`-style version suffix, `-bedrock` suffix; map to a Bedrock cross-region inference ID via a hardcoded `MODEL_MAPPING` table, falling back to a constructed `us.anthropic.{model}-v1:0` guess with a `warn!` for unknown models.
- Beta-feature compatibility table (`BEDROCK_BETA_COMPAT`, [bedrock.rs:64-97](../../../src/providers/bedrock.rs#L64-L97)): `anthropic-beta` header values are filtered per-model — an unsupported or model-incompatible beta is silently dropped with a `debug!` log, not an error.
- `clean_body`/`clean_orphaned_tool_results` ([bedrock.rs:371-466](../../../src/providers/bedrock.rs#L371-L466)): strips fields Bedrock rejects (`defer_loading`, `input_examples`, `custom`, `cache_control`, `tool_reference` blocks) and drops `tool_result` blocks whose `tool_use_id` doesn't match a `tool_use` in the immediately preceding message (or has no `tool_use_id` at all) — Bedrock 400s on either.
- `validate_thinking` ([bedrock.rs:337-365](../../../src/providers/bedrock.rs#L337-L365)): clamps `thinking.budget_tokens` to `[1024, max_tokens]`, dropping `thinking` entirely if `max_tokens < 1024` (issue #8756 reference).
- Streaming: Bedrock's own JSON chunks are re-wrapped as `data: {json}\n\n` SSE frames verbatim ([bedrock.rs:717-724](../../../src/providers/bedrock.rs#L717-L724)) — no reshaping, since the chunk is already Anthropic-shaped.
- Auth is deliberately **not** `AuthMethod`-based — it uses the ambient AWS credential chain (`aws-config`) plus a supplemental SSO-expiry proactive-refresh check reading `~/.aws/sso/cache/*.json` and shelling to `aws sso login` if a TTY is present.

Takeaway for Gemini: this file's true reusable pattern is "validate/clean/map before sending, fail closed on shape drift" — not a content-block translator. Its `ProviderError` classification (`classify_invoke_error`/`classify_stream_error`, [bedrock.rs:611-667](../../../src/providers/bedrock.rs#L611-L667)) mapping throttling→`RateLimited`, validation→`Validation`, access-denied→`Auth`, timeout→`Timeout`, everything else→`Upstream{500,..}` is a good template to mirror for classifying Gemini/Cloud-Code-internal errors.

### `src/providers/openai.rs` (679 lines) + `src/providers/mod.rs` (translation fns) — thin, text-only translation

`OpenaiProvider::send` ([openai.rs:309-338](../../../src/providers/openai.rs#L309-L338)) is the actual analog for what `GeminiProvider` needs: `Provider::send` always receives/returns **Anthropic-wire-format JSON** at the trait boundary, and the provider translates internally before/after the real HTTP call. The pure translation functions live in `mod.rs`:

- `translate_anthropic_request_to_openai` ([mod.rs:335-380](../../../src/providers/mod.rs#L335-L380)): system string → leading `{"role":"system"}` message; **every message's content is flattened to a plain string via `extract_text_from_content`** ([mod.rs:253-265](../../../src/providers/mod.rs#L253-L265)), which just joins `block.text` fields with `\n`. **Tool_use/tool_result blocks have no dedicated handling — they're silently dropped** (extract_text_from_content only reads `.text`, and `tool_use`/`tool_result` blocks carry no top-level `text` field). Images are entirely unhandled (`openai_content_to_anthropic` explicitly skips `image_url` parts, and there's no Anthropic→OpenAI image path at all).
- `translate_openai_response_to_anthropic` ([mod.rs:390-444](../../../src/providers/mod.rs#L390-L444)): only reads `choices[0].message.content` as a plain string into a single `{"type":"text"}` block. No `tool_calls` handling on the response side either.
- `map_openai_finish_reason` ([mod.rs:448-453](../../../src/providers/mod.rs#L448-L453)): `"length"`→`"max_tokens"`, `"tool_calls"`→`"tool_use"`, everything else→`"end_turn"`. Note this stop-reason mapping exists even though the content-level tool_calls translation it implies doesn't.
- `extract_usage`/`AnthropicUsage` ([mod.rs:475-505](../../../src/providers/mod.rs#L475-L505)): a 4-field struct (`input_tokens`, `output_tokens`, `cache_creation_input_tokens`, `cache_read_input_tokens`) parsed from an Anthropic-shaped `usage` object, defaulting missing/malformed fields to 0 rather than failing extraction. This is the shape `GeminiProvider` must populate from Gemini's `usageMetadata` (see §2) — note Gemini has no cache-tier equivalent to `cache_creation_input_tokens`/`cache_read_input_tokens` (those will legitimately stay 0 unless/until Gemini's `cachedContentTokenCount` gets mapped there).
- **Streaming reconstruction** — `OpenaiToAnthropicStream` ([openai.rs:369-535](../../../src/providers/openai.rs#L369-L535)) is the most relevant precedent: it wraps an SSE byte stream in `eventsource_stream::Eventsource`, synthesizes the Anthropic bracketing events (`message_start`→`content_block_start`→N×`content_block_delta`→`content_block_stop`→`message_delta`→`message_stop`) that OpenAI's flatter per-chunk-delta stream has no equivalent for, using a `VecDeque<Bytes>` to buffer multiple synthetic frames produced from one inbound chunk. It only ever opens/tracks a single content block (`index: 0`, hardcoded text type) — no multi-block (e.g. thinking-then-text-then-tool_use) index tracking exists yet. `usage` in the synthesized `message_delta`/`message_start` is **hardcoded to 0** — OpenAI-compatible streaming here reports no real token counts at all.

**No thinking/extended-thinking block handling exists anywhere in `openai.rs` or `mod.rs`.** `bedrock.rs`'s `thinking` handling is only budget validation on the *outgoing* Anthropic-shaped body — Bedrock returns Anthropic's own `{"type":"thinking","thinking":...,"signature":...}` blocks unchanged, so there was never a need to translate them.

### `Provider` trait contract (`src/providers/mod.rs:113-143`)

`fn send(&self, body: Value, headers: HeaderMap, stream: bool) -> Result<ProviderResponse, ProviderError>` — single method, Anthropic-shaped body in, `ProviderResponse::Full(Value)` or `ProviderResponse::Stream(Pin<Box<dyn Stream<Item=Result<Bytes, anyhow::Error>>>>)` out. `list_models` returns `Vec<ModelInfo{id, owned_by}>`. `GeminiProvider` fits this unchanged — no trait changes needed.

### Metrics attribution needs no Gemini-specific code

`RequestDetail::from_body` ([src/metrics/mod.rs:58-80](../../../src/metrics/mod.rs)) derives `model`, `message_count`, and a `msg_types` content-block-type histogram generically from the **Anthropic-shaped** request body, and `provider` is populated from `Provider::name()`. As long as `GeminiProvider` receives/returns Anthropic-shaped JSON at the `send()` boundary (per the trait contract above), the dashboard/metrics requirement in the success metrics is satisfied for free — confirmed by reading the struct/method, no separate per-upstream branch exists to update.

## 2. Gemini's public, documented content schema (`generativelanguage.googleapis.com`)

Even though the internal Cloud Code endpoint is the actual wire target, its
request/response envelope wraps this same documented Gemini content schema
(confirmed in §3) — this is the load-bearing reference for what to translate
Anthropic Messages into.

**Roles:** `"user"` / `"model"` (not Anthropic's `"user"`/`"assistant"`) — straight rename.

**`Content`/`Part` shapes** (a `Content` has a `role` and a `parts[]` array):
- `text: string`
- `inlineData: {mimeType, data}` (base64) — out of scope per requirements (text + tool calls only this pass)
- `fileData: {fileUri, mimeType}` — out of scope
- `functionCall: {name, args, id}` — model-emitted tool invocation (analog of Anthropic's `tool_use` block)
- `functionResponse: {name, id, response}` — analog of Anthropic's `tool_result` block, but goes in a `role: "user"` `Content`, not a distinguished block type

**`GenerationConfig`:** `maxOutputTokens`, `temperature`, `topP`, `topK`, `stopSequences`, `candidateCount`, and — the Gemini-2.5+/3 analog of Anthropic's `thinking` — `thinkingConfig: {thinkingBudget, includeThoughts}`.

**Top-level request fields:** `systemInstruction` (text-only, analog of Anthropic's top-level `system`), `tools[]` (function declarations / code execution), `toolConfig: {functionCallingConfig: {mode: AUTO|ANY|NONE}}`), `safetySettings[]` (per-`HARM_CATEGORY_*` threshold — Anthropic has no equivalent concept at all).

**Response (`GenerateContentResponse`):** `candidates[]`, each with `content` (same Part shapes as above), `finishReason`, `safetyRatings[]`.

**`finishReason` values:** `STOP`, `MAX_TOKENS`, `SAFETY`, `RECITATION`, `OTHER` — a materially different and larger set than Anthropic's `end_turn`/`max_tokens`/`stop_sequence`/`tool_use`.

**`usageMetadata`:** `promptTokenCount`, `cachedContentTokenCount`, `candidatesTokenCount`, `thoughtsTokenCount` (thinking-model reasoning tokens — no Anthropic equivalent field name, would map to something like `output_tokens` inflation or a separate metric), `totalTokenCount`, plus per-modality `*TokensDetails[]` breakdowns.

**Streaming (`streamGenerateContent`):** SSE, each chunk a complete partial `GenerateContentResponse` whose `candidates[0].content.parts[]` accumulate the full response; `usageMetadata` arrives on the final chunk only — this is a flatter, closer-to-OpenAI streaming shape than Anthropic's own bracketed event stream, so `OpenaiToAnthropicStream`'s reconstruction pattern (§1) is the right model to adapt, not Anthropic's own SSE framing.

Sources: [ai.google.dev/api/generate-content](https://ai.google.dev/api/generate-content) (fetched 2026-09-04).

## 3. The internal `cloudcode-pa.googleapis.com/v1internal` envelope (Antigravity/Cloud Code Assist)

Three independent reverse-engineering projects agree closely on the shape,
giving reasonable confidence this is stable enough to design against (though
still explicitly unsupported/undocumented by Google):

**Endpoints:** `POST /v1internal:generateContent` (non-streaming), `POST /v1internal:streamGenerateContent?alt=sse` (streaming), plus `loadCodeAssist` (project/plan/credits info) and `fetchAvailableModels` (model list with quota) — the latter two matter for `Provider::list_models` and possibly for resolving a default `project` id (see envelope below).

**Request envelope** wraps the standard Gemini request shape from §2, but the wrapping fields sit **alongside**, not inside, the Gemini payload:

```json
{
  "project": "{project_id}",
  "model": "{model_id}",
  "request": {
    "contents": [...],
    "generationConfig": {...},
    "systemInstruction": {...},
    "tools": [...]
  },
  "requestType": "...",
  "userAgent": "antigravity",
  "requestId": "{unique_id}"
}
```

This means `GeminiProvider`'s "assemble native body" step is a two-layer operation: translate Anthropic→Gemini `request.*` shape (§2), then wrap it in this envelope with a `project` id that has to come from somewhere (config, or resolved via `loadCodeAssist` — an open question worth flagging to the plan, not fully resolved by this research pass).

**Required headers** (from two independent sources, consistent):
```
Authorization: Bearer {access_token}
Content-Type: application/json
User-Agent: antigravity/{version} {os}/{arch}      # e.g. "antigravity/1.15.8 windows/amd64"
X-Goog-Api-Client: google-cloud-sdk vscode_cloudshelleditor/0.1
Client-Metadata: {"ideType":"ANTIGRAVITY","platform":"...","pluginType":"GEMINI"}
```
plus `Accept: text/event-stream` for the streaming call. This directly answers one of the requirements doc's Open Questions ("what headers beyond the bearer token") — yes, `User-Agent`/`Client-Metadata`/`X-Goog-Api-Client` all appear necessary in every documented client, not just the legacy reverse-engineered plugin. Auth research (Agent 1's territory) should verify whether omitting them causes a hard rejection vs. degraded behavior.

**Streaming framing:** SSE (`alt=sse`), each `data:` line a full JSON object shaped `{"response": {candidates: [...]}, "traceId": ..., "metadata": ...}` — i.e. the same per-chunk-accumulates-`parts[]` pattern as public Gemini streaming, just wrapped in one more envelope layer.

**Function calls:** `functionCall: {name, args, id}` on the model side; `functionResponse` sent back inside a `role: "user"` `Content` — matches public Gemini exactly, confirming the internal endpoint really does reuse Gemini's content schema rather than inventing a new one.

**Errors:** standard Google API error envelope `{"error": {code, message, status, details[]}}`; 429s carry `retryDelay` (e.g. `"3.957525076s"`) inside `details[]` — directly usable for `ProviderError::RateLimitedWithRetry{retry_after}`.

Sources: [opencode-antigravity-auth ANTIGRAVITY_API_SPEC.md](https://github.com/NoeFabris/opencode-antigravity-auth/blob/main/docs/ANTIGRAVITY_API_SPEC.md), [picoclaw antigravity provider docs](https://docs.picoclaw.io/docs/providers/antigravity/), [elad12390/antigravity-proxy](https://github.com/elad12390/antigravity-proxy) (referenced, not separately fetched — the two docs above cross-confirm its shape). Confidence: MEDIUM — three community reverse-engineering efforts converge on the same envelope, but none is Google-authoritative and the protocol is explicitly called out in the requirements doc as subject to change without notice.

## 4. Edge cases / failure modes the existing Bedrock/OpenAI code does NOT handle, that Gemini will expose

1. **Mandatory `thought_signature` round-tripping (Gemini 3 specific, HIGH impact).** Gemini 3 Pro *enforces* (not just recommends) that every `functionCall` part it emits carries a `thought_signature`, and the caller must echo that exact, unmodified signature back on the corresponding function-call turn in the next request's history — omitting it produces a hard error ("Function call is missing a thought_signature in functionCall parts"), confirmed across three independent bug reports (`continuedev/continue#8785`, `sst/opencode#4832`, n8n community). This is *required even at minimal thinking levels*, not just when thinking is verbose. **Nothing in the existing codebase's translation layer has a concept of an opaque per-block signature that must be preserved verbatim through a round trip** — `mod.rs`'s tool-call handling doesn't exist at all yet (§1), so there's no existing pattern to extend, and Anthropic's own `thinking.signature` field (which the codebase never touches either, since Bedrock passes it through Anthropic-native) is the closest analog but attaches to a *separate* thinking block, not to the `tool_use` block itself. The Gemini translation layer needs a dedicated stash-and-replay mechanism: capture `thought_signature` on inbound `functionCall` parts, store it associated with the corresponding Anthropic `tool_use` block id (there's no natural Anthropic field for it — likely needs a provider-internal side-channel, e.g. re-encoding it into the `tool_use` block's otherwise-unused fields, or a request-scoped cache keyed by tool_use id), and replay it when that turn round-trips back in a later `functionResponse`. Sources: [Thought Signatures — Google AI docs](https://ai.google.dev/gemini-api/docs/generate-content/thought-signatures), [Thought signatures — Gemini Enterprise Agent Platform docs](https://docs.cloud.google.com/gemini-enterprise-agent-platform/models/thought-signatures).
2. **Stricter/different tool-schema validation.** Per the picoclaw integration notes, tool `functionDeclaration` schemas sent to the Antigravity-routed endpoint must have unsupported JSON Schema keywords stripped (`patternProperties`, `$ref`, and likely others per full JSON Schema draft support gaps) — Anthropic tool schemas commonly emit `$ref`/`$defs` for nested types (Claude Code's own tool definitions do), and neither `bedrock.rs`'s `clean_body` nor any OpenAI-path code does JSON-Schema sanitization at all. This needs new code, likely a schema-walking sanitizer analogous to `bedrock.rs`'s `clean_body` tool-field stripping but operating on nested schema structure, not top-level fields.
3. **`finishReason: SAFETY` / `RECITATION` have no Anthropic equivalent.** Anthropic's `stop_reason` enum (`end_turn`, `max_tokens`, `stop_sequence`, `tool_use`) has no "the model refused/was blocked" value the way `stop_reason` is structured — Claude instead just emits a refusal as ordinary text content with `end_turn`. A naive mapping (e.g. defaulting `SAFETY`→`end_turn` the way `map_openai_finish_reason`'s fallback arm does for unknown OpenAI reasons) would silently hide a safety block from callers as if the model had answered normally. This needs an explicit decision in planning: either map `SAFETY`/`RECITATION` to `end_turn` with synthesized text content explaining the block (most faithful to what a client expects to see), or invent handling via `ProviderError::Validation` — but the latter would incorrectly surface a completed-but-blocked generation as a hard request failure, which is arguably more wrong. Nothing in existing code (`map_openai_finish_reason`'s exhaustive-but-defaulting match) models "safety block" as a case distinct from "ordinary stop" at all.
4. **`safetySettings` and `safetyRatings` are a wholesale new concept.** Neither Bedrock nor OpenAI translation touches per-category harm thresholds; Gemini's request needs *some* `safetySettings` value sent (even if just "use defaults") and its response can carry `safetyRatings` per candidate that Anthropic responses have no field for — likely drop these from the returned Anthropic-shaped body (they don't map to anything the caller expects) but the finishReason interaction above still needs settling.
5. **Streaming multi-block reconstruction is currently single-block-only.** `OpenaiToAnthropicStream` (§1) hardcodes a single `index: 0` text content block for the whole stream. Gemini/Cloud-Code streaming can interleave function calls and text within one candidate's accumulating `parts[]`, and — once thinking is involved — potentially a thinking part too. The Gemini stream translator needs proper multi-index content-block tracking (open a new Anthropic `content_block_start` each time the part `type` changes: text→functionCall→text, etc.), which is new machinery beyond what `openai.rs` provides as a copyable pattern (OpenAI Chat Completions streaming's `tool_calls` deltas are index-keyed by call slot, which is at least structurally closer to what's needed here than Anthropic's own format is — worth studying OpenAI's `tool_calls[].index` delta-merging convention even though this codebase's own OpenAI translator never implemented it).
6. **`max_tokens` semantics/model context-window differences aren't validated anywhere.** Bedrock's `validate_thinking` clamps `thinking.budget_tokens` against `max_tokens`, but nothing in this codebase validates `max_tokens` itself against a model's true output-token ceiling (Bedrock just forwards whatever value Anthropic itself would reject). Gemini models have per-model `maxOutputTokens` ceilings that differ substantially by model (and a materially larger context window is one of the stated motivations for wanting Gemini at all, per requirements — see §5 below), so an unvalidated `max_tokens` passthrough risks either silent truncation (if Gemini clamps quietly) or a `Validation` error the router should be able to interpret cleanly. Worth a small per-model ceiling table analogous to `bedrock.rs`'s `MODEL_MAPPING`, sized to whichever Gemini model IDs get first-class support (§5).
7. **Protocol-drift detection has no existing precedent to reuse.** The requirements doc explicitly calls for "fail closed with a clear `ProviderError`, not a silent misparse" plus a distinct schema-drift log/metric signal when the internal API's response shape changes unexpectedly. Neither `bedrock.rs` nor `openai.rs` has this concept — both use `.and_then(Value::as_str).unwrap_or(default)`-style lenient parsing throughout, which is exactly the "silent misparse" pattern the requirements doc wants avoided for Gemini. This is new design surface: `GeminiProvider`'s response parsing should validate the shape strictly (e.g. via `serde` deserialization into a typed struct rather than ad hoc `Value` field access) and treat a deserialization failure as the distinct "schema drift" signal, separate from an ordinary `ProviderError::Upstream`.

## 5. Unstated needs behind the explicit requirements

- **Large-context-window use case.** The requirements doc's Problem Statement doesn't explicitly say *why* Tyler wants Gemini beyond "has the subscription," but Gemini's headline differentiator vs. Claude/GPT is its 1M-2M token context window. If that's a real motivating use case (long-document/large-codebase work), the `max_tokens`/context-length validation gap (§4.6) and the model-selection question below both become higher-priority than a bare "make it work" reading of the requirements would suggest — worth confirming with Tyler in planning whether large-context routing (e.g. auto-selecting a Gemini upstream for requests estimated to exceed Claude's context window) is an implicit want, even though it's not in-scope text today. Flagging as a question for `sdd:3-plan`, not assuming an answer.
- **`gemini-3-pro` as the practical target model, not "Gemini" generically.** The Decision section names `gemini-3-pro`/`gemini-2.5-flash` as example model IDs but doesn't commit to one. Given Tyler's subscription is Antigravity (positioned as Google's premium coding-agent product, paired with Gemini 3 as its flagship model per the referenced I/O 2026 launch), `gemini-3-pro` is the most likely "first-class" target — and that specifically means the mandatory `thought_signature` handling (§4.1) is not an edge case to defer, it's on the critical path for the model Tyler will actually use day one. A `gemini-2.5-flash`-only implementation could ship without solving §4.1 (thought signatures were optional pre-Gemini-3) but would not satisfy what Tyler is actually reaching for this provider to do. Recommend the plan explicitly scope "first-class" to whichever Gemini 3 variant(s) `fetchAvailableModels` reports for Tyler's account, with older 2.5-family models as best-effort/passthrough.
- **`RouteUpstreamRef.model` override interaction.** Consolette's routing already supports pinning a specific model per route ([schema.rs:146-155](../../../src/config/schema.rs#L146-L155)). Given the antigravity-cli's internal endpoint requires an internal `project` id in the envelope (§3) that has no equivalent in the current `AuthMethod`/`UpstreamKind` schema, the plan needs to decide where that project id lives — most naturally a new field on `UpstreamKind::Gemini` (mirroring how `UpstreamKind::Bedrock` carries `aws_region`/`aws_profile`) rather than overloading `RouteUpstreamRef.model`, which is about model selection, not per-account project scoping. This is implicit in "mirror the existing providers" but isn't spelled out in the requirements' Scope section.
- **Fallback ordering intent.** The Problem Statement frames Gemini as participating in "unified fallback/weighted-routing" alongside Anthropic/Bedrock, but doesn't say whether Tyler wants Gemini as a *primary* upstream for certain routes or purely a *fallback* target given the acknowledged protocol-stability risk (Feasibility Risks: "Google could change or block the internal protocol without notice"). Given that risk profile, an unstated-but-reasonable default would be routing Gemini into `Strategy::Fallback` chains behind Anthropic/Bedrock rather than `Strategy::Weighted` traffic-splitting on day one, since a weighted split sends steady-state production traffic through the least-stable upstream. Worth surfacing as a config-example recommendation in the plan's `references/conf.d/00-providers.toml` sample rather than assuming Tyler wants day-one weighted traffic on it.
