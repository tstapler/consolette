# Research: Build vs. Buy — openrouter-routing

Agent 6, SDD Phase 2. Question: for each piece of this feature, build from
scratch or source from an existing crate/service? Findings below are backed
by live crates.io/GitHub lookups (web search, 2026-09-05) plus a read of the
current codebase (`src/routing/health.rs`, `src/metrics/histogram.rs`,
`src/routing/strategy.rs`, `Cargo.toml`).

## 1. Existing OSS library/crate

### 1a. Rust OpenRouter client crate

Three community crates exist on crates.io:

| Crate | Version | Downloads | License | Notes |
|---|---|---|---|---|
| [`openrouter-rs`](https://crates.io/crates/openrouter-rs) ([GitHub](https://github.com/realmorrisliu/openrouter-rs)) | 0.14.0 (26 versions published) | 23,476 all-time | MIT | Domain-oriented client: chat, responses, rerank, audio, image/video gen, embeddings, files, presets, analytics, management — claims 95/95 OpenAPI paths covered |
| [`openrouter_api`](https://docs.rs/crate/openrouter_api/latest) | 0.7.0 | — | — | Type-state builder, also wraps MCP client connections |
| [`openrouter-sdk`](https://github.com/zaoinc/openrouter-sdk) | — | — | — | Thinner, less activity signal |

**Pros**: `openrouter-rs` in particular is actively maintained (26 releases), MIT-licensed, and covers far more surface (rerank, presets, analytics) than consolette needs.

**Cons**: All three are general-purpose OpenRouter SDKs with their own request/response types, retry/error handling, and HTTP client (their own `reqwest` config, not consolette's). Consolette's requirement (`src/providers/mod.rs`) is narrower and more constrained: `OpenrouterProvider` must implement the existing `Provider` trait, translate to/from consolette's own wire types the same way `OpenaiProvider` does today, integrate with consolette's `SecretRef`/`AuthMethod` auth, and route errors into consolette's own `ProviderError` taxonomy (`Exhausted`, `RateLimitedWithRetry`, etc.) — none of which an external SDK's types map onto for free. OpenRouter's Chat Completions API is OpenAI-compatible, and consolette already has `OpenaiProvider` (`src/providers/mod.rs`) doing exactly this shape of translation for other OpenAI-compatible upstreams — pulling in a whole extra SDK to duplicate that pattern with different types is net-negative: two request/response translation paths to maintain instead of one, and a new dependency surface (transitive `reqwest` version pin risk, MSRV) for what amounts to a handful of endpoints (`POST /chat/completions`, `GET /models`).

**Verdict: Not recommended.** Build `OpenrouterProvider` as a thin adapter that reuses `OpenaiProvider`'s request/response translation (composition or shared helper), talking to OpenRouter's endpoints directly via the existing `reqwest` client already in `Cargo.toml`. This matches the requirement's explicit constraint: "the existing `Provider` trait contract... no bespoke dispatch path."

### 1b. TTL-based caching crate vs. hand-rolled Mutex+Instant

**`moka` is already a direct dependency** (`Cargo.toml`: `moka = { version = "0.12", features = ["future"] }`, resolved at 0.12.16 in `Cargo.lock`) and is already used for TTL/capacity-bounded caching in five places in this codebase: [`src/compression/rewind.rs`](../../../src/compression/rewind.rs), [`src/cost_metrics/store.rs`](../../../src/cost_metrics/store.rs), [`src/memory/dedup.rs`](../../../src/memory/dedup.rs), [`src/memory/store.rs`](../../../src/memory/store.rs), and [`src/session_compaction/session_state.rs`](../../../src/session_compaction/session_state.rs). `src/memory/store.rs` in particular is a documented pattern: "In-memory key-value store backed by moka cache," using `moka::future::Cache`'s built-in TTL and atomic `get_with`/`get_or_init` semantics.

By contrast, the codebase's hand-rolled `Mutex`/`Instant` patterns (`src/routing/health.rs`'s `HealthRegistry`, `src/metrics/histogram.rs`'s `DurationHistogram`) solve a *different* problem: per-key cooldown state and a sliding-window sample buffer, not a cache with eviction/TTL semantics. Health is a tiny `DashMap<usize, ProviderState>` (bounded by upstream count, no eviction needed); the histogram is an unbounded-key, single-instance rolling window that needs percentile computation over raw samples — `moka` doesn't do percentile aggregation, so it wouldn't replace `DurationHistogram`'s role even for per-candidate extension.

**Pros of `moka` for the model-list cache**: zero new dependency, matches an established in-repo idiom (five prior uses), gives async-safe `get_with`/single-flight refresh for free (important since concurrent requests must not all hit OpenRouter's `/models` endpoint simultaneously on cache miss), and has native per-entry TTL plus manual `invalidate()` for the early-invalidation-on-stale-error requirement in Scope.

**Cons**: None material — this is squarely the tool already chosen for this exact shape of problem in this codebase.

**Verdict: Recommended.** Use `moka::future::Cache` (already a dependency) for the free-model-list cache — single entry keyed by upstream name/index, TTL from config, `invalidate()` called on the stale-signal error path. Do **not** hand-roll a new `Mutex<Option<(Instant, Vec<Model>)>>` — that would be reinventing what `moka` already gives this codebase elsewhere, and do **not** add `cached` (a different, simpler macro-based crate) since `moka` is already present and used for the identical use case.

### 1c. Existing "LLM router" crate/framework for architectural inspiration

No Rust crate specifically does latency/error/quality-based *model selection* (as opposed to request routing/proxying). The closest prior art is in other ecosystems:

- **LiteLLM Router** (Python) — ships multiple named strategies: `simple-shuffle` (default), `least-busy`, `usage-based-routing-v2`, `latency-based-routing`, `cost-based-routing`, plus an "adaptive" router using a multi-armed bandit over historical performance ([LiteLLM routing docs](https://docs.litellm.ai/docs/routing), [load balancing docs](https://docs.litellm.ai/docs/proxy/load_balancing)). Its latency-based strategy is the nearest analog to what this feature needs.
- **TensorZero** (Rust) — a Rust-based inference gateway with routing/retries/fallbacks/load-balancing, but the project is reported **discontinued** as of the current search results (community guidance now points to LiteLLM/Bifrost/Portkey instead) — not viable as a dependency or even as actively-maintained design reference.
- **Bifrost** — lowest-latency gateway (11µs overhead at 5K RPS) with automatic failover and semantic caching, but no published composite-scoring algorithm design to imitate; it's closed/commercial-leaning documentation, thin on the selection-algorithm internals.

**Verdict**: no crate to adopt (Not recommended as a dependency); LiteLLM's named-strategy taxonomy and its separate "latency-based" vs. "cost-based" strategies are useful *design* reference for Phase 3 (see section 4).

## 2. SaaS/managed API — does OpenRouter or a gateway already do this?

**OpenRouter's own provider routing** operates one level below what this feature needs: it picks among *upstream inference providers* for a single chosen model (e.g., picking which GPU host serves `meta-llama/llama-3-70b`), not among *different free models* by coding quality. It does not do cross-model quality-based selection — that choice is still the caller's to make, which is exactly the gap this feature fills.

**Portkey** — an AI gateway (now owned by Palo Alto Networks) with a free Developer tier (10K logs/month, "not for production" per its own docs) and a paid Production tier ($49/mo for 100K logs, scaling to $9/100K beyond, up to 3M). It offers routing rules, retries, and guardrails, with an open-source self-hostable gateway. It is a **control plane**, not a router with the free-model-quality composite scoring this feature specifies — using it would still require building the scoring logic, just inside Portkey's config DSL instead of Rust, and would add a second network hop (consolette → Portkey → OpenRouter) plus a second service to run/monitor for a single-user local proxy.

**LiteLLM proxy** — a comparable self-hosted Python gateway with the latency-based routing strategy noted above. Same objection applies: it's a whole second process (Python runtime, its own config format) sitting in front of consolette's job, duplicating the fallback/health/rate-limit machinery consolette *already has* (ADR-003, ADR-004) in a different language.

**Cost/fit assessment against consolette's stated purpose**: the [README](../../../README.md) and [CLAUDE.md](../../../CLAUDE.md) both frame consolette explicitly as *"a provider-agnostic LLM router"* shipping as *"a single Rust binary"* that a user points `ANTHROPIC_BASE_URL` at, with live-reloadable config and no restart required. Inserting Portkey or LiteLLM as a hosted proxy *in front of* consolette directly contradicts that framing — it would mean running (and keeping alive, and configuring, and securing) a second process just to get free-model quality routing that this feature's scope is a few hundred lines of Rust to add natively, reusing machinery (`HealthRegistry`, `RoutingStrategy`, `MetricsCollector`) that already exists in-process.

**Verdict: Not recommended.** Neither OpenRouter's native routing nor a third-party gateway substitutes for this feature; both would add a network hop and an out-of-process dependency that conflicts with consolette's single-binary, no-restart-config design goal, for a capability (cross-model quality scoring) neither actually provides out of the box.

## 3. LLM-generated implementation vs. battle-tested library

### 3a. The composite scoring algorithm (latency + error-rate + bench-rank)

Hand-rolling a small weighted-scoring function is reasonable *if* scoped tightly, per the Rabbit Holes section's own warning against building "a general configurable weighting system." The real risk isn't the arithmetic — it's normalization: mixing raw milliseconds, a 0.0–1.0 error fraction, and a benchmark rank/score on an unrelated scale (e.g., aider's pass-rate percentage) without first putting all three on a common scale (e.g., min-max or z-score normalization per candidate set) will produce a score dominated by whichever raw unit happens to have the largest numeric range — a classic "forgot to normalize" bug that unit tests over a *fixed* candidate set won't catch (the bug only shows up when the live candidate pool's latency/error spread changes shape). No existing Rust crate solves "combine 3 heterogeneous normalized signals into one weighted score" as a reusable abstraction (this is a 10–20 line function, not a library-shaped problem); the risk is design risk, not implementation risk, which argues for **the Phase 3 plan fixing one concrete formula and pinning it down with property-style tests** (e.g., "a candidate at the worst end of every signal never outscores one at the best end of every signal") rather than pulling in a dependency.

**Verdict: Viable to hand-roll**, contingent on Phase 3 fixing an explicit normalization step (not leaving it to the implementer) and adding tests asserting the score is monotonic in each signal — this is squarely aligned with the Rabbit Holes guidance to fix "a concrete, simple formula" rather than a generalized system.

### 3b. The coding-benchmark data itself

Two realistic sources surfaced:

- **Aider's polyglot leaderboard** — canonical data lives in [`Aider-AI/aider`](https://github.com/Aider-AI/aider) (leaderboard YAML under the repo, referenced from [aider.chat/docs/leaderboards](https://aider.chat/docs/leaderboards/)) and mirrored at [Aider-AI/polyglot-benchmark](https://github.com/Aider-AI/polyglot-benchmark). Aider is Apache-2.0 licensed, and the leaderboard numbers are published openly with a documented submission process (PRs against the leaderboard data files) — this is small, stable, human-readable data (per-model pass rates on ~225 exercises), well suited to hand-copying a handful of relevant free-model rows into a checked-in TOML table with a comment citing the source URL and retrieval date, exactly matching the requirement's "static, checked-in/config-editable default" language.
- **LiveBench** — has a `download_leaderboard.py` script in [LiveBench/LiveBench](https://github.com/livebench/livebench), but an [open GitHub issue (#82)](https://github.com/LiveBench/LiveBench/issues/82) shows leaderboard data has not reliably been available for download via a stable API/file in the past, and no clear license statement surfaced in search results. Less turnkey than aider's.

Neither source has a maintained Rust crate or structured API wrapping it — there's no `aider-benchmark-data` crate to depend on. The realistic choice is "hand-copy a small table from a published source" vs. "build a scraper," and the requirements doc has already ruled out the scraper (Out of Scope: "Automatically scraping/crawling third-party coding leaderboards on a schedule").

**Verdict: Recommended — hand-copy from aider's polyglot leaderboard** into the static TOML table, with a source-URL + retrieval-date comment for future refresh (matches Alternatives Considered's explicit rejection of live-fetching). LiveBench is a viable secondary/cross-check source but not the primary pick given its less consistent data-access story.

## 4. Fork or adapt

No Rust LLM gateway is close enough to consolette's architecture (trait-based `Provider`/`RoutingStrategy`/`HealthRegistry` split, per ADR-003) to fork or vendor code from — TensorZero (Rust) is reportedly discontinued and was schema/structured-inference-focused, not model-selection-focused; Bifrost's selection internals aren't published in enough depth to adapt. LiteLLM (Python) is the one system whose *design vocabulary* is worth imitating conceptually: its separation of named routing strategies (latency-based, cost-based, usage-based) as swappable policies is directly analogous to consolette's existing `RoutingStrategy` trait (`FallbackStrategy`, `WeightedStrategy` in `src/routing/strategy.rs`) — the new strategy this feature adds is naturally a third implementor of that same trait (e.g., `ScoredStrategy` or similar), not a new dispatch mechanism. This is a "look at how they named/separated concerns" reference, not a code-porting exercise (Python → Rust vendoring isn't meaningful here anyway).

**Verdict: Viable as design reference only** (LiteLLM's strategy taxonomy), **not recommended** to fork or vendor any existing gateway's code — consolette's existing `RoutingStrategy` abstraction is already the right shape to extend, per ADR-003 and the requirement's explicit constraint to use "the existing `Provider` trait contract... no bespoke dispatch path."

## Summary table

| Piece | Decision | Verdict |
|---|---|---|
| OpenRouter Rust client crate (`openrouter-rs`/`openrouter_api`) | Build thin adapter reusing `OpenaiProvider`'s translation path | Not recommended (as a dependency) |
| TTL cache for free-model list | Use `moka` (already a dependency, already the in-repo idiom in 5 modules) | Recommended |
| Rust "LLM router" crate for model selection | None exists; no adoption | Not recommended |
| OpenRouter's own provider routing / Portkey / LiteLLM proxy as a substitute | Would add a second process/hop, contradicts single-binary framing, doesn't solve cross-model quality scoring anyway | Not recommended |
| Hand-rolled composite scoring function | Build in Phase 3, with explicit normalization + monotonicity tests | Viable (with guardrails) |
| Coding-benchmark data source | Hand-copy a small table from aider's polyglot leaderboard (Apache-2.0, openly published) | Recommended |
| Fork/vendor an existing gateway's selection code | None close enough architecturally; LiteLLM's strategy taxonomy as design reference only | Not recommended (fork); Viable (reference) |
