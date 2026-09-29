# Pitfalls: configurable `anthropic` upstream `base_url`

Research for `project_plans/anthropic-upstream-base-url/requirements.md`. Sources are this
repo at the working-tree commit on `main` (`712c4eb`); all line numbers below are current
`src/` state, not yet a permanent SHA (unmerged feature).

## 1. `anthropic-version` / `anthropic-beta` header forwarding

`AnthropicProvider::build_headers` (`src/providers/anthropic.rs:171-203`) unconditionally:
- Sets `Content-Type: application/json`.
- Forwards the client's `anthropic-version` header if present, else defaults to
  `"2023-06-01"` (line 188).
- Forwards `anthropic-beta` verbatim if present (lines 194-198).

None of this is gated on `base_url`. For a corporate relay that isn't `api.anthropic.com`,
this is a real but *low-severity* risk: worst case the relay ignores headers it doesn't
recognize (most HTTP servers do) or, less likely, rejects the request over an unexpected
header. There's no existing per-upstream mechanism to suppress/rewrite headers for other
kinds either (`OpenaiProvider` has its own fixed header set), so this isn't a new gap
introduced by the base_url change — it's a pre-existing "we always send Anthropic-shaped
headers" assumption that a configurable base_url merely makes reachable by a non-Anthropic
target for the first time. **Design recommendation**: don't scope-creep header
conditionality into this change. Note it as a known limitation in the doc comment/README
(alongside the base_url doc update the requirements already call for), and defer a
per-upstream header allow/deny-list to a follow-up if someone actually hits it.

## 2. `normalize_model_name`'s unconditional Bedrock-prefix stripping

`normalize_model_name` (`src/providers/anthropic.rs:124-133`) strips a `us.anthropic.`
prefix and a trailing `-v1`/`-v1:0` version suffix from *every* model string, called
unconditionally in both `send_request` (line 220) and `send_streaming_request` (line 295) —
no base_url check gates it either.

This is a genuine risk once `base_url` is configurable: a hypothetical alternate endpoint
that legitimately expects a model id shaped like `us.anthropic.claude-3-5-sonnet-20241022-v1:0`
verbatim (e.g. it fronts Bedrock itself, or otherwise mirrors Bedrock naming) would have its
model id silently corrupted before the request goes out. Silent corruption, not a rejected
request — worse than a clean failure because it will manifest as a confusing "model not
found" error from the *relay*, not from consolette. This is exactly the class of stale
legacy-carryover bug the session already found once in this file (`clean_request_body`
stripping fields the real API now supports) — same file, same "ported unconditionally from
legacy, never revisited for the new configurability axis" pattern.

**Recommendation**: this is worth a design decision, not just a mention. Options, cheapest
first:
- (a) Leave as-is and document the limitation (matches item 1's treatment) — acceptable only
  if the target audience for this feature is "same model catalog as api.anthropic.com,
  different host," which is the stated use case in the requirements doc (internal
  proxy/gateway/relay, not a differently-shaped API).
- (b) Gate normalization on whether `base_url` is the default (`https://api.anthropic.com`)
  — skip stripping when it's been overridden, on the theory that Bedrock-format ids only
  show up because Claude Code targets Bedrock elsewhere and this codepath exists to paper
  over that specifically for the real Anthropic API.
- (c) Add a separate config knob to opt out of normalization per-upstream.

(b) is the best risk/effort tradeoff and should be the default recommendation: it directly
addresses the corruption risk without adding new config surface, and is consistent with the
comment's own stated purpose ("Claude Code occasionally sends Bedrock-format names to *the
Anthropic endpoint*" — line 121-122, emphasis on which endpoint). Flag this as an open
question for the plan phase rather than deciding unilaterally here, since it changes
behavior beyond what the requirements doc scoped.

## 3. Silent misconfiguration / malformed base_url failure mode

Traced `OpenaiProvider::send_request`'s error handling (`src/providers/openai/mod.rs:170-208`,
esp. 196-206) as the pattern this feature will inherit unchanged for `anthropic`:

```rust
let response = self.client.post(&url).headers(headers).body(body_bytes).send().await
    .map_err(|e| {
        if e.is_timeout() {
            ProviderError::Timeout
        } else {
            ProviderError::Upstream { status: 0, body: e.to_string() }
        }
    })?;
```

`reqwest::Client::post` does not validate the URL eagerly — a malformed `base_url` (missing
scheme, trailing garbage, etc.) doesn't fail at `Client::builder()...build()` time (which
only fails on TLS/connector config problems) or at `.post(&url)` time; the parse error
surfaces only when `.send()` is awaited, as a generic `reqwest::Error`. That error is caught
by the `else` branch above and wrapped into `ProviderError::Upstream { status: 0, body:
e.to_string() }` — i.e. whatever `reqwest`'s own `Display` produces (something like
`builder error: relative URL without a base` for a missing-scheme case, or a DNS-resolution
message for a bad host). `AnthropicProvider::send_request`/`send_streaming_request`
(`src/providers/anthropic.rs:242-258`, `317+`) already use the identical `map_err` shape, so
this is not a new failure mode this feature introduces — it's the existing, already-shipped
behavior for `openai.base_url`, and the anthropic base_url field will just start exercising
the same path.

**Verdict**: this is `status: 0` plus a raw reqwest error string, not a clear
"your base_url config is invalid" message — confusing but not silent (it does surface as an
error to the caller, not a hang or wrong-endpoint success). Given the `openai` kind has
shipped this exact behavior with no reported confusion, **this is not blocking** for the
anthropic change — fixing it would be a pre-existing UX gap affecting both provider kinds
equally, better handled as an independent "validate base_url at config-load time" follow-up
(e.g. a `Url::parse` check in `config::validate`) than smuggled into this feature's diff.
Worth a one-line mention in the plan's risk section, not a blocking requirement.

## 4. Test/regression coverage risk

Confirmed: `src/providers/anthropic.rs` has no mock-server test harness today (searched the
file; no `axum`/`TcpListener` usage), while `src/providers/openai/mod.rs` has two
(`start_capturing_chat_completions_server` at line ~1320, `start_server` at line ~1597, both
built on `axum::Router` + `tokio::net::TcpListener::bind("127.0.0.1:0")` + a `Mutex<Option<Value>>`
capture cell, then a `provider_for(base_url)` helper that constructs an `OpenaiProvider`
pointed at the bound address).

This session already has one data point on what happens when this file loses coverage: the
`clean_request_body` fix earlier this session was "a pure deletion" per the prior context —
i.e., it removed code without the deletion being caught or guarded by a new regression test,
because none existed to guard it in the first place.

**Recommendation: this feature MUST add a real mock-server test, not defer it.** Reasoning:
- The feature is *specifically* about `base_url` becoming runtime-configurable and actually
  being honored by the outgoing request. A unit test on `normalize_model_name` or on config
  deserialization proves the config parses; it proves nothing about whether
  `AnthropicProvider` actually sends the request to the configured host instead of the
  hardcoded `https://api.anthropic.com`. Only an integration-shaped test (something binds a
  listener, `AnthropicProvider` is pointed at it, a request is sent, the listener asserts it
  received the request) closes that gap.
- The `openai` module's harness is already generalizable: `start_capturing_chat_completions_server`
  binds an arbitrary path/handler pair on `127.0.0.1:0` and returns `(base_url, captured, _server)`.
  The Anthropic surface differs only in path (`/v1/messages` vs `/v1/chat/completions`) and
  response shape — the harness's bind/serve/capture skeleton is directly reusable, not a
  from-scratch build. This caps the cost of "add a harness" well below "build an axum test
  server from nothing."
- Given this exact file already shipped one silent regression this session from missing
  coverage, shipping a second base-URL-shaped change to the same file with zero automated
  coverage repeats a pattern that's already caused a real bug here, not a hypothetical one.

Concretely: add an `#[cfg(test)] mod tests` block in `src/providers/anthropic.rs` (or a
shared test-support module both `anthropic.rs` and `openai/mod.rs` pull from, if the
duplication is judged worth generalizing during planning) with at minimum one test that
constructs an `AnthropicProvider` with a non-default `base_url` pointed at a local axum
listener and asserts the listener saw the request at `{base_url}/v1/messages`.

## 5. Interaction with the `deny_unknown_fields` + `flatten` caveat

The documented caveat sits directly above `Upstream` (`src/config/schema.rs:126-129`):

> Note: no `deny_unknown_fields` here — serde does not support combining it with `flatten`
> on the same struct. Typo protection for kind-specific fields still comes from
> `UpstreamKind`'s own `deny_unknown_fields`, since any field this struct doesn't recognize
> is handed to the flattened enum.

`UpstreamKind` itself *does* carry `#[serde(tag = "kind", rename_all = "lowercase",
deny_unknown_fields)]` (line 106). Adding a `#[serde(default = "default_anthropic_base_url")]
base_url: String` field to the `Anthropic` variant doesn't interact badly with either
caveat: `deny_unknown_fields` on a tagged enum only rejects fields *not* declared by whichever
variant the `kind` tag selects, and a `#[serde(default = ...)]` field is still a declared
field for that purpose — this is exactly the same mechanism `Bedrock`'s existing
`#[serde(default)]` fields (`aws_region`, `aws_profile`, `max_retries`, lines 110-115) and
`Openai`'s required `base_url: String` (line 118) already rely on today, both under the same
enum-level `deny_unknown_fields`. No new serde risk here.

**The real risk in this area is mechanical, not serde-semantic**: `UpstreamKind::Anthropic`
is currently a bare unit variant, matched as a *bare* pattern (no `{ .. }`) at every call
site:
- `src/config/validate.rs:44` — `UpstreamKind::Anthropic => "anthropic"`
- `src/entrypoint/mod.rs:173` — same shape
- `src/server_tools/mod.rs:217` — `Some(UpstreamKind::Anthropic) => {}`
- `src/routing/router.rs:89` — `UpstreamKind::Anthropic => Arc::new(AnthropicProvider::new(...))`
- Construction sites treating it as a value with no fields: `src/config/schema.rs:353`,
  `src/cost_metrics/estimator.rs:464`, `src/routing/router.rs:1865/1908/2092/2144`

Turning `Anthropic` into a struct variant (`Anthropic { base_url: String }`, mirroring
`Openai { base_url: String }` exactly, per the requirements' own constraint) makes every one
of these bare-pattern match arms and bare-value construction sites a compile error. This is
good news, not bad — it's caught at compile time, not silently — but it means the diff's
blast radius is wider than "two files" (`schema.rs` + `anthropic.rs`); at minimum the five
files above need a mechanical update (`UpstreamKind::Anthropic => ...` → `UpstreamKind::Anthropic { .. } => ...`,
or `{ base_url } => ...` where the value is needed, matching `router.rs:89`'s and
`router.rs:98`'s existing side-by-side pattern for `Openai`). Plan phase should enumerate
these call sites explicitly rather than relying on `cargo build` to surface them one at a
time.

## Summary of recommendations for the plan phase

| # | Risk | Verdict |
|---|------|---------|
| 1 | Header forwarding to non-Anthropic relay | Low severity, pre-existing gap widened not created — document, don't gate |
| 2 | Bedrock model-id normalization could corrupt a legitimate verbatim model id | Real risk — recommend gating normalization on `base_url == default`, but treat as an open design question for planning, not pre-decided here |
| 3 | Malformed base_url → confusing `status: 0` reqwest error | Pre-existing (`openai` kind already has it) — not blocking, worth a one-line mention only |
| 4 | Zero test coverage for a base-URL-honoring change, in a file that already had one coverage-gap regression this session | **Must add** a mock-server test; the `openai` module's axum harness is directly reusable, not a from-scratch build |
| 5 | `deny_unknown_fields`+`flatten` caveat | No actual serde conflict — but the unit→struct variant change breaks ~9 bare-pattern call sites across 6 files at compile time; enumerate them in the plan |
