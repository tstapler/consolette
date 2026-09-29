# Research: Architecture — anthropic-upstream-base-url

Scope: exact diff shape for giving `UpstreamKind::Anthropic` an optional
`base_url`, reusing the construction-site map already documented in
`project_plans/gemini-provider/research/architecture.md` (that doc's §2,
lines ~35-42, and §5, lines ~92-116) rather than re-deriving it. Since that
research landed, `UpstreamKind::Gemini { project_id: String }` and its
`GeminiProvider` arm are now real code (`src/routing/router.rs:106-111`,
`src/entrypoint/mod.rs:176`) — a second worked precedent alongside `Openai`.

## 1. `UpstreamKind` enum and the `deny_unknown_fields`/`flatten` caveat

Current enum (`src/config/schema.rs:105-124`):

```rust
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum UpstreamKind {
    Anthropic,
    Bedrock { #[serde(default)] aws_region: Option<String>, ... },
    Openai { base_url: String },
    Gemini { project_id: String },
    Openrouter {},
}
```

The comment immediately above `Upstream` (`schema.rs:126-129`) explains the
`flatten` caveat: `Upstream` flattens `UpstreamKind` into itself and *cannot*
also carry `deny_unknown_fields` (serde doesn't support combining the two),
so all typo protection lives on `UpstreamKind` itself. **This does not
interact with adding a field to the `Anthropic` variant** — `deny_unknown_fields`
on the enum only rejects fields *not* declared by whichever variant is
selected (`kind = "anthropic"`); adding `base_url` to that variant simply
means one more field name is now accepted for that tag value. No caveat
applies in the other direction either: `Anthropic` moving from a unit
variant to a struct variant is a non-breaking shape change for
`#[serde(tag = "kind")]` enums as long as the new field is optional
(`#[serde(default)]`), matching exactly what `model_family: Option<String>`
does elsewhere (`schema.rs:167-168`, same file, same `#[serde(default)]`
idiom cited in the requirements doc).

Required change:

```rust
Anthropic {
    #[serde(default)]
    base_url: Option<String>,
},
```

Existing TOML with `kind = "anthropic"` and no `base_url` key deserializes
identically to today (unit-variant configs already omit any body fields, so
`{ kind = "anthropic" }` continues to parse — the struct-with-all-defaulted-fields
shape is serde-equivalent to a unit variant for deserialization purposes).

## 2. `AnthropicProvider::new` vs `OpenaiProvider::new` — minimal diff shape

`AnthropicProvider::new` (`src/providers/anthropic.rs:82-117`) hardcodes the
base URL at construction:

```rust
pub fn new(
    upstream: Arc<Upstream>,
    resolver: Arc<dyn SecretResolver + Send + Sync>,
    exec_cache: Arc<ExecCredentialCache>,
    request_timeout_secs: u64,
) -> Result<Self, ProviderError> {
    ...
    Ok(Self {
        client,
        stream_client,
        base_url: "https://api.anthropic.com".to_string(),   // <- hardcoded
        upstream,
        resolver,
        exec_cache,
    })
}
```

`OpenaiProvider::new` (`src/providers/openai/mod.rs:99-129`) already takes
`base_url: String` as its second positional parameter (between `upstream`
and `resolver`) and threads it straight into the struct — no transformation,
no validation, no trailing-slash handling beyond the doc comment's "with no
trailing slash" convention note.

Minimal diff for `AnthropicProvider::new`: add a `base_url: String`
parameter in the same position `OpenaiProvider` uses it (after `upstream`,
before `resolver`) and use it verbatim in place of the hardcoded literal.
The default-resolution (`Option<String>` → `"https://api.anthropic.com"` if
`None`) belongs at the **call site** in `router.rs::build_providers`, not
inside `AnthropicProvider::new` — this mirrors how `Openai { base_url }`'s
`base_url` is already a required `String` by the time it reaches
`OpenaiProvider::new`; the enum-vs-provider-layer split (`Option` in config,
concrete `String` in the provider constructor) stays consistent across both
providers rather than introducing a second place that understands "empty
means default."

The doc comment at `anthropic.rs:52-56` needs updating — it currently
asserts:

```rust
/// Base URL for the Anthropic API. `UpstreamKind::Anthropic` carries no
/// base-URL override field in the new schema (only `Openai` does), so
/// this is hardcoded, matching legacy's default. See the final port
/// report for this gap.
base_url: String,
```

Both factual claims ("carries no base-URL override field", "hardcoded")
become false the moment this ships; replace with wording parallel to
`OpenaiProvider`'s own field doc (no comment needed at all on the struct
field, since `base_url: String` is now self-explanatory the same way
`OpenaiProvider`'s is) — and drop the "final port report" pointer, which
was explaining a gap this change closes.

## 3. Every other `UpstreamKind::Anthropic` match/construction site

`grep -rn "UpstreamKind::Anthropic" src/` returns 12 hits, more than the two
call sites the requirements doc names by way of example — the full list,
by file:

- **`src/routing/router.rs:89`** — `build_providers()`'s
  `UpstreamKind::Anthropic => Arc::new(AnthropicProvider::new(...))` arm.
  **The one substantive change**: destructure `{ base_url }` and pass
  `base_url.clone().unwrap_or_else(|| "https://api.anthropic.com".to_string())`
  as the new positional constructor arg (§2). Every other site below is a
  compile-fix, not a logic change.
- **`src/entrypoint/mod.rs:173`** — `upstream_kind_label()`'s
  `UpstreamKind::Anthropic => "anthropic"` arm.
- **`src/config/validate.rs:44`** — a second, independent `match` returning
  `"anthropic"` (label for validation error messages, separate function from
  `upstream_kind_label`, not previously cross-referenced in the Gemini
  research doc).
- **`src/server_tools/mod.rs:217`** — `Some(UpstreamKind::Anthropic) => {}` (a
  no-op arm in some `Option<&UpstreamKind>` match, needs `{ .. }` added).
- All four of the above are unit-pattern matches on the tag only; a struct
  variant with a named field requires braces (`UpstreamKind::Anthropic { .. }`)
  even when the field is ignored — **each of these four lines fails to
  compile as written today** the moment the variant gains a field, and each
  needs the identical one-token fix. None needs new logic: the label stays
  `"anthropic"` and the no-op stays a no-op regardless of `base_url`.
- **Construction sites** (not matches — same braces requirement, opposite
  direction): `src/config/schema.rs:353`, `src/routing/router.rs:1865`,
  `src/cost_metrics/estimator.rs:464` (all `kind: UpstreamKind::Anthropic,`
  struct-literal field values in test fixtures), plus
  `src/routing/router.rs:1908, 2092, 2144` (bare-value test-helper calls,
  e.g. `bearer_upstream("anthropic", UpstreamKind::Anthropic, "sk-ant-test")`).
  All are test fixtures (none in non-test code), all fail to compile
  identically, all get the same mechanical fix
  (`UpstreamKind::Anthropic { base_url: None }` unless a given test
  specifically wants to exercise a custom URL).
- **`src/providers/anthropic.rs:53,71`** — doc-comment prose mentioning
  `UpstreamKind::Anthropic` by name, not code; covered by the doc-comment
  rewrite in §2, not a compile concern.
- **`src/config/load.rs`** — confirmed via grep to contain zero
  `UpstreamKind::Anthropic` references; its three `UpstreamKind::Bedrock { .. }`
  matches (legacy-env-shim, per the Gemini research §2) are unaffected. No
  change needed.

Net: one behavioral change (`router.rs:89`) plus roughly ten mechanical
`{ .. }`/`{ base_url: None }` compile fixes the Rust compiler will point at
directly (exhaustiveness/shape-checking catches every one, matches and
literals alike) — more sites than the requirements doc's own examples
named, but all low-risk and compiler-enforced, not hidden runtime behavior
to hunt for by hand.

## 4. Simple additive change vs. hotspot — churn/size check

```
git log --oneline -- src/providers/anthropic.rs | wc -l   → 7 commits
git log --oneline -- src/config/schema.rs      | wc -l   → 7 commits
wc -l src/providers/anthropic.rs                          → 606 lines
wc -l src/config/schema.rs                                → 534 lines
git log --oneline -3 -- src/providers/anthropic.rs
  57eb548 fix(providers): stop stripping fields the Anthropic API now supports
  b951d2f fix(providers): sanitize tool schemas, log stream requests, model catalog
  84ec94a fix(providers): surface array and reasoning content in OpenAI translations
```

7 commits touching `anthropic.rs` in this repo's history (a young repo per
the recent-commits list in the session's git status) is low churn in
absolute terms, and none of the three most recent commits touch the
constructor or `base_url` — they're all response/request-translation fixes
in the `send`/streaming path, a different region of the same file. `schema.rs`
at 534 lines with 7 commits is similarly unremarkable: most of its growth is
additive (new `UpstreamKind` variants, new `Option` fields with
`#[serde(default)]`), not rework of existing fields. Neither file shows
churn-hotspot signal (repeated fixes to the *same* lines) or a structural
smell (God-object growth, tangled responsibilities) — `schema.rs` is a
flat data-schema module and `anthropic.rs` cleanly separates construction
(`new`), pure helpers (`normalize_model_name`, `map_error_status`), and
`Provider` trait impl, matching the pattern already validated for `Gemini`
and `Openai`.

## Recommendation: extend-as-is

This is a small, mechanical, additive change with a well-established
precedent (`Openai { base_url: String }`, and now `Gemini { project_id: String }`)
already in the codebase for *two* other providers. There is no existing
SOLID/Clean-Architecture violation in the touched files worth fixing or
isolating behind a seam first:

- The enum/provider split (`Option<String>` in config, `String` resolved-with-default
  passed into the constructor) is already the working pattern — extending it
  to a third variant doesn't add complexity, it removes an inconsistency
  (`Anthropic` was the odd one out).
- The doc-comment fix (§2) and the five/six mechanical compile-fix sites (§3)
  are proportionate to a same-shape change already made twice before; no
  refactor of `build_providers`'s match structure or `AnthropicProvider`'s
  internals is warranted.
- The one open question worth flagging for planning (per the requirements'
  own "Open questions" section) is auth-method flexibility for a
  non-`api.anthropic.com` endpoint — that's a product/scope decision, not an
  architecture one; nothing in `AuthMethod`'s existing `Bearer`/`Apikey`/`Exec`
  handling (`apply_auth_headers`, shared with `Openai`/`Gemini` per the
  Gemini research §5) needs to change to support it, since auth is already
  orthogonal to `base_url` in the schema (`Upstream.auth` is a sibling field,
  not nested inside `UpstreamKind`).

## Test-harness note (constraint from requirements, not resolved here)

The requirements doc flags that `src/providers/anthropic.rs` has no
axum-based mock-server test harness today (unlike `openai`'s). Confirming
this is a planning-phase decision (build/reuse a harness) rather than an
architecture one — no code in `anthropic.rs`'s current shape blocks adding
one; a real regression test for a configurable `base_url` needs *some*
local HTTP listener, and generalizing the existing `openai` module's harness
(rather than duplicating it) is worth evaluating in `sdd:3-plan` given both
providers now share the identical "construct a client pointed at a
caller-supplied base URL" shape.
