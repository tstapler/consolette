# Stack Research: compaction-cost-metrics

Agent 1 (Stack) — SDD Phase 2

## 1. Tokenizer crate: `tiktoken-rs`

- **Already a direct dependency**: `Cargo.toml:88` pins `tiktoken-rs = "0.5"`; `Cargo.lock` resolves it to **0.5.9** (MIT-licensed, `zurawiki/tiktoken-rs`).
- **Currently unused in code**: `grep -rn "tiktoken_rs::" src/` returns nothing. The only mention is a comment at `src/ratelimit/mod.rs:25` ("`est_tokens` is a rough estimate (e.g. via tiktoken) used for the TPM ..."), i.e. the crate is provisioned but not wired up anywhere yet — this feature would be its first real caller.
- **Encodings needed are supported**: the crate exposes `cl100k_base`/`cl100k_base_singleton` (GPT-4/3.5-turbo family — closest public approximation for Anthropic's undisclosed tokenizer) and `o200k_base`/`o200k_base_singleton` (GPT-4o/o1/o3/GPT-5 family), plus `get_bpe_from_model`/`get_bpe_from_tokenizer` helpers to pick an encoding by model name. Exactly what's needed for an OpenAI counterfactual and an Anthropic-approximation counterfactual.
- **Version drift**: upstream has moved well past 0.5.x (recent tags include 0.7.0, 0.9.x, and a 0.12.0 docs.rs listing observed in a mid-2026 search — the exact newest tag should be re-verified with `cargo search tiktoken-rs` / crates.io at implementation time, since web-search results were inconsistent about the very latest number). The newer releases raise MSRV toward Rust 1.85 (via vendored upstream code, not a stated MSRV bump in Cargo.toml itself). Recommendation: **bump the existing pin** to the current latest 0.x rather than introduce a second tokenizer crate — the crate is already vetted into the dependency tree and its API surface (`cl100k_base`, `o200k_base`, `CoreBPE::encode`) has been stable across those versions.
- **Anthropic accuracy caveat** (already flagged in requirements' Feasibility Risks): tiktoken is an OpenAI BPE tokenizer; Anthropic's actual tokenizer is undisclosed and not tiktoken-compatible, so any Anthropic token estimate via this crate is an approximation, not exact. Document this explicitly wherever counterfactual-for-Anthropic numbers are surfaced (CLI output, API JSON) so operators don't read it as ground truth.

## 2. Machine-readable pricing data

- **No first-party JSON pricing endpoint from Anthropic or OpenAI.** Both providers publish pricing only as human-readable docs pages (`docs.anthropic.com/.../pricing`, `platform.openai.com/docs/pricing`) — confirmed via web search, no `/pricing.json`-style API from either vendor.
- **Community-maintained option**: LiteLLM's `model_prices_and_context_window.json` (github.com/BerriAI/litellm) is the well-known, actively maintained model→price mapping the requirements doc calls out by name. It's a plain JSON file in a public GitHub repo (fetchable over HTTPS, no auth), covering Anthropic, OpenAI, and 100+ other providers/models with per-token input/output pricing and context-window sizes.
- **Other third-party aggregators** (OpenRouter's `GET /api/v1/models`, TLDL's `/api/pricing.json`, aipricing.guru's `/api/pricing.json`) exist but are unofficial re-publications of the same docs-page data — no more authoritative than LiteLLM's file, and less established/version-controlled.
- **Recommendation for the "optional best-effort live lookup"**: fetch LiteLLM's raw JSON file from GitHub (or vendor a periodic snapshot) as the live-refresh source, with the built-in static table (Anthropic + OpenAI defaults) as the hard fallback — satisfies "must degrade cleanly to static table" without inventing a bespoke scraper against either vendor's docs HTML.

## 3. Existing dependency versions relevant to this feature (`Cargo.toml`)

| Crate | Pinned version | Relevance |
|---|---|---|
| `moka` | `0.12` (feature `future`) | Reuse pattern from `SessionStateStore` (`src/session_compaction/session_state.rs:18`, `moka::future::Cache`) for the new session-scoped cost aggregator — no new cache-library dependency needed. |
| `reqwest` | `0.12` (default-features off; `json`, `stream`, `rustls-tls`) | Already the HTTP client in the tree — use it for the optional async/background pricing-lookup fetch (LiteLLM JSON or provider docs) rather than adding a second HTTP client crate. Already rustls-based, consistent with `aws-smithy-http-client`'s `rustls-ring` feature elsewhere in the manifest. |
| `tokio` | `1` (`full`) | Background refresh task for pricing lookup can use a `tokio::spawn` + interval, matching existing async patterns; satisfies the constraint that pricing lookup must never block `SessionCompactionPipeline::apply`. |
| `serde` / `serde_json` | `1` | For (de)serializing the pricing table (built-in default + user overrides) and the new API endpoint's JSON response — both already project-standard. |
| `axum` | `0.8` (feature `macros`) | New HTTP endpoint should follow existing router conventions (check `src/metrics/mod.rs`'s `/metrics` handler for the established path/response-shape pattern). |
| `clap` | `4` (feature `derive`) | New CLI subcommand should follow existing derive-based subcommand pattern. |
| `dashmap` | `6` | Alternative to moka if a non-TTL, non-evicting concurrent map is preferred for aggregation state — but moka is the constraint-mandated choice per requirements. |
| `tiktoken-rs` | `0.5` (locked `0.5.9`) | See §1 — already present, unused, needs version bump consideration. |
| `chrono` | `0.4` (feature `serde`) | Available for any timestamped aggregation fields. |
| `once_cell` | `1` | Available for lazily-initialized static default pricing table. |

No version conflicts identified: `reqwest`, `tokio`, `moka`, `serde_json` are all already resolved once in `Cargo.lock` and none of the proposed new work requires a second copy of any of these (e.g., no need for a second HTTP client for pricing lookups — `reqwest` covers it).

## Sources

- [tiktoken-rs — crates.io](https://crates.io/crates/tiktoken-rs)
- [zurawiki/tiktoken-rs — GitHub](https://github.com/zurawiki/tiktoken-rs)
- [tiktoken-rs — docs.rs](https://docs.rs/crate/tiktoken-rs/latest)
- [litellm/model_prices_and_context_window.json — BerriAI/litellm](https://github.com/BerriAI/litellm/blob/main/model_prices_and_context_window.json)
- [LiteLLM: Add Model Pricing & Context Window docs](https://docs.litellm.ai/docs/provider_registration/add_model_pricing)
- [Claude Platform Docs — Pricing](https://platform.claude.com/docs/en/about-claude/pricing)
- Repo: `Cargo.toml`, `Cargo.lock` (read directly), `src/ratelimit/mod.rs:25`, `src/session_compaction/session_state.rs:18`
