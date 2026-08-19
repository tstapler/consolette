# Build vs. Buy: compaction-cost-metrics

Agent 6, Phase 2 research. Scope: tokenizer, pricing data, session-scoped cache, and the
LLM-vs-battle-tested-library lens for the token-counting core.

## Codebase facts checked first

- `tiktoken-rs = "0.5"` is **already a direct dependency** (`Cargo.toml:88`), but grep across
  `src/` shows it is not imported anywhere yet — only referenced in a doc comment
  (`src/ratelimit/mod.rs:25`, "`est_tokens`... a rough estimate (e.g. via tiktoken)"). The
  char-count heuristic requirements.md rejects lives at `src/bin/mcp-proxy/server.rs:154`
  (`estimate_token_count`), a separate, unrelated function. So adopting tiktoken-rs for real
  requires no new dependency — just first real usage of one already vendored.
- `moka::future::Cache` is already used exactly once, in
  `src/session_compaction/session_state.rs`, with the shape: `Cache<SessionKey, Arc<RwLock<T>>>`,
  `get_or_default` inserting a fresh `Arc::new(RwLock::new(T::default()))` on miss and returning
  the existing `Arc` on hit. This is a proven "accumulate under a key across concurrent callers"
  pattern in this exact codebase — mutation happens by locking the `RwLock` inside the `Arc`, not
  by mutating the cache value in place, so it survives moka's internal eviction/insert semantics.
- `axum`, `reqwest` (rustls-tls, json, stream features), `clap` (derive), and `once_cell`/`sha2`
  are already present, covering the HTTP endpoint, any optional pricing-lookup HTTP client, and
  the CLI subcommand with no new deps.

## 1. Tokenizer

### Option A — `tiktoken-rs` (zurawiki/tiktoken-rs)

- **Maturity/maintenance**: Actively maintained — crates.io/docs.rs show releases through
  April–June 2026 (v0.10–0.12), 32 versions since Feb 2023, current releases add GPT-5.x/o-series
  vocab support and track upstream Rust edition bumps (2026 release requires Rust 1.85+). Widely
  used (~225 direct dependents per lib.rs). [crates.io/crates/tiktoken-rs](https://crates.io/crates/tiktoken-rs), [github.com/zurawiki/tiktoken-rs](https://github.com/zurawiki/tiktoken-rs)
- **License**: MIT.
- **MSRV**: 1.85+ as of the 2026 releases (Rust 2024 edition) — needs a compatibility check
  against consolette's toolchain pin before bumping past 0.5.
- **Encodings**: Ships `cl100k_base` and `o200k_base` (and older encodings) out of the box —
  covers the OpenAI-side counterfactual estimate.
- **Verdict for OpenAI-side counting**: **Recommended** — already a declared dependency, MIT,
  actively maintained, ships the needed encodings. No reason to hand-roll BPE for this piece.

### Option B — HuggingFace `tokenizers` crate

- General-purpose, supports arbitrary vocab/model files, but does not ship OpenAI's tiktoken
  encodings natively and is heavier (brings in more transitive deps) than needed for "count
  OpenAI-style tokens." **Not recommended** — no advantage over tiktoken-rs for this scope, and
  it isn't already a dependency.

### Option C — Anthropic's own tokenizer, for Anthropic requests specifically

This is the key finding requirements.md explicitly asked for: **Anthropic does not publish a
standalone tokenizer library** (no Rust crate, no pip package with the actual BPE vocab), but it
does publish an official **`POST /v1/messages/count_tokens`** API endpoint
(`client.messages.count_tokens()` in the SDKs) that returns the exact token count the model would
bill for a given request — same-model-exact, not an approximation.
[platform.claude.com/docs/en/build-with-claude/token-counting](https://platform.claude.com/docs/en/build-with-claude/token-counting), [docs.anthropic.com/en/api/messages-count-tokens](https://docs.anthropic.com/en/api/messages-count-tokens)

Anthropic's own docs state that **tiktoken should not be used to estimate Claude token counts** —
it undercounts by roughly 15–20% on typical text and more on code/non-English text, and Claude
4.7+ models use a newer tokenizer that produces ~30% more tokens than earlier Claude tokenizers
for the same text. Since the pipeline's "actual" tokens already come from
`usage.input_tokens`/`output_tokens` on real Anthropic responses (in scope per requirements.md),
the counterfactual side is the only place a tokenizer estimate is needed, and it is specifically
"how many tokens would this **uncompacted** history have cost" — i.e., a hypothetical request
that was never sent, so `usage` isn't available for it.

**Verdict**: For the Anthropic-side counterfactual, call `count_tokens` against the real,
uncompacted message history (an extra free-tier API call, not a text-based estimate) rather than
approximating with tiktoken. This is strictly more accurate than any local tokenizer and costs no
extra request-body computation — **Recommended**, with tiktoken-rs's `cl100k_base`/`o200k_base` as
the estimator for OpenAI-routed requests where no equivalent free official endpoint exists (verify
at implementation time; OpenAI's usage-based endpoints return actual counts post-hoc but not a
free pre-flight estimate the way Anthropic's does). Design note: this makes the counterfactual
estimator provider-dependent — Anthropic path calls `count_tokens` (network, cacheable per
history-prefix), OpenAI/other path falls back to tiktoken-rs (local, free, approximate). The
requirements.md phrase "tiktoken-based... real tokenizer" should be read as satisfied by
tiktoken-rs *for the OpenAI counterfactual*, while the Anthropic counterfactual should not use
tiktoken at all given Anthropic's own guidance that it's measurably wrong for Claude models.

## 2. Pricing data

### Vendoring LiteLLM's `model_prices_and_context_window.json`

- BerriAI/litellm publishes and continuously updates
  `model_prices_and_context_window.json` at the repo root (not under `enterprise/`), covering
  per-model input/output/cache-read/cache-write token pricing for Anthropic, OpenAI, and 100+
  other providers. Update cadence is very high — dozens of community PRs merge per month
  ([github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json](https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json)).
- **License**: litellm's LICENSE splits `enterprise/` (separate Enterprise License) from
  everything else (MIT). The pricing JSON lives outside `enterprise/`, so it's MIT —
  safe to vendor/sync a copy or a filtered subset.
  [github.com/BerriAI/litellm/blob/main/LICENSE](https://github.com/BerriAI/litellm/blob/main/LICENSE)
- **Verdict**: **Recommended as the seed/refresh source** for the static default pricing table —
  don't hand-maintain Anthropic/OpenAI prices from scratch. Vendor a small filtered snapshot
  (just the Anthropic + OpenAI models consolette actually routes to) at build time or via a
  periodic manual sync script, checked into the repo like a fixture — this satisfies the "no new
  persistent datastore" constraint (it's a static compiled-in/config-loaded table, not a live
  dependency) while avoiding hand-transcribing prices that already exist as maintained JSON.

### Direct research on official machine-readable pricing (requirements.md's explicit open question)

- **OpenAI**: No official machine-readable pricing endpoint or package. Pricing lives only as a
  rendered HTML page (`developers.openai.com/api/docs/pricing` / `openai.com/api/pricing`),
  updated without a versioned changelog. Confirmed by multiple third-party pricing aggregators
  that exist specifically to scrape/re-publish it as JSON (e.g. aipricing.guru's
  `/api/pricing.json`, itself just a scrape published daily) — the existence of these scrapers is
  itself evidence no official feed exists. **Finding: no.**
- **Anthropic**: No official pricing API/JSON found either (search turned up only the rendered
  docs pricing page and third-party trackers); Anthropic's `count_tokens` endpoint gives token
  counts, not prices. **Finding: no.**
- **Verdict on the "OPTIONAL live pricing lookup" scope item**: **Not recommended** to build
  against either provider's site directly (HTML scraping is brittle and explicitly against the
  spirit of "machine-readable"). If a live-refresh path is wanted, point it at LiteLLM's raw JSON
  file over HTTPS (`reqwest`, already a dependency) on a background interval, degrading to the
  vendored static snapshot on fetch/parse failure — this satisfies the constraint "must not add
  per-request latency if pricing lookup needs network" trivially, since it's a background refresh
  into an in-memory table, never inline with a request.

## 3. Session-scoped cache (moka)

Confirmed via `src/session_compaction/session_state.rs`: the existing `SessionStateStore` pattern
— `Cache<SessionKey, Arc<RwLock<StateStruct>>>` with `get_or_default` — already supports exactly
the access pattern this feature needs (accumulate/mutate a value under a key concurrently: moka
hands out the same `Arc` to every caller with that key, and callers serialize mutation through the
inner `RwLock`; different keys proceed independently with no contention). No new cache dependency
or design is needed.

**Verdict**: **Recommended** — build a second store, e.g. `SessionCostAggregator` /
`Cache<SessionKey, Arc<RwLock<SessionCostTotals>>>`, mirroring `SessionStateStore` verbatim (same
crate version, same TTL-eviction builder shape, likely the same or a slightly longer TTL since
cost totals are less transient than plan/skill reinjection state). This is squarely a
"reuse existing dependency and pattern," not a "verify alternative caches" situation — moka is
already proven in this exact role in this exact codebase.

## 4. LLM-generated vs. battle-tested library — the token-counting core specifically

Argument for **not** hand-rolling BPE tokenization:

- BPE tokenizer bugs are silent-by-construction: a wrong merge rule or off-by-one in the
  regex pre-tokenizer produces a token count that is wrong but never crashes, never NaNs, never
  fails a type check — it just quietly feeds a slightly-wrong number into every downstream
  actual-vs-counterfactual delta and every dollar figure this feature exists to produce. There is
  no natural runtime signal that would surface the bug; it would only be caught by manually
  cross-checking totals against a provider's dashboard, which is exactly the tedious reconciliation
  this feature is meant to replace.
- `tiktoken-rs` and Anthropic's `count_tokens` endpoint are each validated against millions of
  real production requests by their respective maintainers (OpenAI's actual billing pipeline, in
  Anthropic's case) — correctness here is not a matter of "does the algorithm look right in a
  code review," it's "does it match the vocab file and merge table the provider's real
  tokenizer/biller uses," which is only verifiable by using the provider's own artifact or a
  library that vendors it byte-for-byte (tiktoken-rs embeds the actual encoding files).
- A hand-written or LLM-generated BPE implementation would need to independently reproduce
  OpenAI's exact vocab + merge-rule + regex-split behavior (cl100k_base/o200k_base) to be
  trustworthy, which is strictly more work than depending on a crate that already does this and
  is exercised by hundreds of downstream consumers who would notice regressions before this
  project would.
- **Verdict**: adopt tiktoken-rs (already vendored) for the OpenAI counterfactual and Anthropic's
  official `count_tokens` endpoint for the Anthropic counterfactual; do not write custom BPE code
  for this feature under any circumstance. The only "build" work here is a thin adapter that
  calls one or the other per provider and normalizes the result into the report struct.

## Summary table

| Area | Option | Verdict |
|---|---|---|
| Tokenizer (OpenAI-side) | `tiktoken-rs` (already a dep) | Recommended |
| Tokenizer (OpenAI-side) | HuggingFace `tokenizers` | Not recommended |
| Tokenizer (Anthropic-side) | Anthropic `count_tokens` API | Recommended (more accurate than tiktoken; Anthropic explicitly warns against tiktoken for Claude) |
| Pricing table seed | Vendor LiteLLM's `model_prices_and_context_window.json` (MIT, outside `enterprise/`) | Recommended |
| Pricing table | Hand-maintain from scratch | Not recommended (redundant with an actively-updated MIT source) |
| Live pricing lookup | Poll LiteLLM's raw JSON in background, static fallback | Viable (optional per requirements.md) |
| Live pricing lookup | Scrape OpenAI/Anthropic pricing pages | Not recommended (no official machine-readable endpoint exists on either side — confirmed) |
| Session-scoped cache | Reuse `moka::future::Cache<SessionKey, Arc<RwLock<T>>>` pattern from `SessionStateStore` | Recommended |
| Session-scoped cache | Different cache crate | Not recommended (no gap the existing pattern doesn't cover) |
| Token-counting core | Hand-rolled/LLM-generated BPE | Not recommended (silent-corruption risk, no verification advantage over existing libraries) |
