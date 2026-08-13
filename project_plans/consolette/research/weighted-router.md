# Weighted + Fallback Cross-Provider Router (Consolette)

Research on implementing a **health-aware weighted router** that coexists with the
existing **ordered fallback** behind one strategy interface, sharing a single
429/health cooldown. Grounded in the current `claude-proxy-rs` code
(`src/fallback.rs`, `src/providers/*`, `src/main.rs`).

---

## 1. Recommended design (top-line)

The current code conflates **three concerns** inside `FallbackHandler::dispatch`:

1. **Health/cooldown state** — today a single-slot `FallbackState` (primary only).
2. **Selection policy** — today hardcoded "primary, then fallback".
3. **Dispatch/retry orchestration** — the error-class match + retry loop.

The refactor separates them so the selection policy becomes pluggable while the
cooldown state and the dispatch loop are shared by **all** strategies:

```
Router  (strategy-agnostic orchestrator; owns the dispatch loop)
 ├─ upstreams: Vec<Arc<Upstream>>          // config-ordered; each = name + Arc<dyn Provider> + weight + can_cooldown
 ├─ health:   Arc<HealthRegistry>          // per-upstream cooldown, FACTORED OUT of strategy
 └─ strategy: Arc<dyn RoutingStrategy>     // fallback | weighted (pure selection)
```

### Trait sketch

```rust
/// A candidate the router may select. Cheap to clone (index + metadata).
#[derive(Clone, Copy)]
pub struct UpstreamRef {
    pub idx: usize,          // stable index into Router.upstreams
    pub name: &'static str,  // for logging
    pub weight: u32,         // configured weight (used by `weighted` only)
}

/// PURE selection policy. Stateless w.r.t. health: it receives ONLY the
/// already-filtered healthy candidates and returns which one to try next.
/// Health/cooldown is deliberately NOT visible here — that's what keeps
/// fallback and weighted sharing one cooldown implementation.
pub trait RoutingStrategy: Send + Sync {
    fn select(&self, healthy: &[UpstreamRef]) -> Option<UpstreamRef>;
    fn name(&self) -> &'static str;
}

/// Ordered fallback == current behavior. Candidates arrive in config
/// priority order, so "first healthy" reproduces primary→fallback exactly.
pub struct FallbackStrategy;
impl RoutingStrategy for FallbackStrategy {
    fn select(&self, healthy: &[UpstreamRef]) -> Option<UpstreamRef> {
        healthy.first().copied()
    }
    fn name(&self) -> &'static str { "fallback" }
}

/// Weighted random over healthy candidates. Excluding a cooled-down upstream
/// is automatic: it never enters `healthy`, so the remaining peers' relative
/// weights are preserved == proportional redistribution.
pub struct WeightedStrategy;
impl RoutingStrategy for WeightedStrategy {
    fn select(&self, healthy: &[UpstreamRef]) -> Option<UpstreamRef> {
        use rand::distributions::{Distribution, WeightedIndex};
        if healthy.is_empty() { return None; }
        let weights = healthy.iter().map(|u| u.weight.max(1));
        let dist = WeightedIndex::new(weights).ok()?;   // Err only if all zero
        let i = dist.sample(&mut rand::thread_rng());
        Some(healthy[i])
    }
    fn name(&self) -> &'static str { "weighted" }
}
```

### The retry seam (how the requested `select(&[UpstreamRef])` signature handles retries)

Keep the requested signature exactly — **no `attempt` parameter needed**. The
**router** owns the loop and shrinks the candidate set each iteration:

```text
loop:
  candidates = health.healthy_subset(all_upstreams) MINUS already_tried
  pick = strategy.select(&candidates)?          // fallback→first, weighted→random
  match upstreams[pick.idx].send(...).await:
    Ok(resp)                       => return Ok(resp)
    Err(e) if e.is_validation()    => return Err(e)          // 4xx: no failover
    Err(e) if e.is_auth()          => return Err(e)          // 401: no failover
    Err(e) if e.is_rate_limited()  => health.trip(pick.idx, e.retry_after());   // then continue
    Err(_transient / unsupported)  => { /* continue */ }
  already_tried.insert(pick.idx)
```

- `fallback` shrinking → deterministically walks config order (identical to today).
- `weighted` shrinking → re-samples the remaining healthy pool (auto-redistribution).

This is the **near-verbatim reuse** of the existing `dispatch` match arms — only
"which upstream is next" changes from hardcoded to `strategy.select(...)`.

### Reuse plan vs. `fallback.rs`

| Existing element | Fate |
|---|---|
| `Provider` trait (`name`, `send`) | **Unchanged** — reused as-is. |
| `ProviderState { Normal, Cooldown{until} }` enum | **Moved** into `health.rs`, reused per-upstream (the "reuse the state machine" ask). |
| `try_exit_cooldown` atomic check-and-clear (ADR-006 TOCTOU fix) | **Reused** per entry via `DashMap::get_mut`. |
| `FallbackState` (single slot) | **Generalized** → `HealthRegistry` keyed by upstream idx. |
| `FallbackHandler<P,F>` dispatch loop + error arms | **Generalized** → `Router` over `Vec<Arc<dyn Provider>>`; arms kept. |
| Bedrock "never cooldown" + internal exp-backoff | **Preserved** via per-upstream `can_cooldown: bool`; keep same-upstream retries inside the provider. |

---

## 2. Current-state summary (what exists today)

**`Provider` trait** (`src/fallback.rs:44`): `fn name(&self) -> &str` and
`async fn send(body, headers, stream) -> Result<ProviderResponse, ProviderError>`.
Implemented by `AnthropicProvider` (`providers/anthropic.rs:404`) and
`BedrockProvider` (`providers/bedrock.rs:698`).

**Cooldown state machine** (`FallbackState`, `src/fallback.rs:81`), the ADR-006 design:

- `ProviderState = Normal | Cooldown { until: Instant }`, behind
  `Arc<tokio::sync::RwLock<ProviderState>>`. **Single slot — primary only.**
- `should_use_fallback()` (read lock): `Cooldown && now < until`.
- `enter_cooldown(override_dur)` (write): sets `Cooldown { until }`; honors
  `Retry-After` via `override_duration`, else default `cooldown_duration` (300s).
- `try_exit_cooldown()` (write): **atomically** checks expiry and resets to
  `Normal` — this is the TOCTOU-safe transition ADR-006 calls out.
- `remaining_secs()` — used by `/metrics` (`main.rs:276`).

**Dispatch** (`FallbackHandler<P, F>::dispatch`, `src/fallback.rs:210`): hardcoded
2-provider machine. Steps: (1) `should_use_fallback` → skip primary; else
`try_exit_cooldown`; (2) try primary — on `is_validation`/`is_auth` return
immediately, on `is_rate_limited` `enter_cooldown` + fall through, on
`ModelUnsupported`/transient fall through; (3) fallback loop with Bedrock
`2^n` exponential backoff (`bedrock_max_retries`), **Bedrock never cooled down**;
(4) all-failed → `503`.

**Error classification** (`providers/mod.rs:43`): `ProviderError` with
`is_rate_limited()`, `is_validation()`, `is_auth()`, `is_transient()`,
`retry_after_secs()`. 429/529 → `RateLimited`; 4xx → `Validation`; etc.
(Providers map status at `anthropic.rs:230` / `bedrock.rs:classify_*`.)

**Wiring** (`main.rs`): `AppState` holds
`Arc<FallbackHandler<AnthropicProvider, BedrockProvider>>` + `Arc<FallbackState>`
(`main.rs:61-62`), built at `main.rs:113-122`. `handle_messages` calls
`state.fallback.dispatch(body, headers, is_stream, &request_id)` (`main.rs:494`).
Config is env-only (`config.rs`); `cooldown_seconds` default 300.

**Available deps** (`Cargo.toml`): `dashmap = "5"` **already present**; `tokio`,
`async-trait`, `backoff`, `tokio-retry` present. **`rand` is NOT yet a
dependency** — must be added for weighted selection.

---

## 3. Weighted selection algorithm

**Recommendation: `rand::distributions::WeightedIndex`** over a hand-rolled
cumulative scan.

- **Correctness**: builds an O(n) cumulative table, samples in O(log n), handles
  arbitrary `u32` weights, and returns `Err` only when all weights are zero
  (guard by `.max(1)` or by treating all-zero as "any healthy").
- **Redistribution is free**: only healthy upstreams are in the slice, so a
  cooled-down peer's weight is simply absent and the survivors keep their
  relative proportions — exactly OpenRouter-style proportional reweighting. No
  explicit "redistribute" math needed.
- Candidate sets are tiny (2–5), so a manual cumulative-sum scan is also fine;
  prefer `WeightedIndex` for clarity and zero-weight edge handling.

**Dependency**: add `rand = "0.8"` (WeightedIndex at `rand::distributions`,
RNG via `rand::thread_rng()`). Note API drift: in `rand 0.9` these moved to
`rand::distr::weighted::WeightedIndex` and `rand::rng()`. Pin 0.8 unless the
project wants latest.

**RNG choice**: `rand::thread_rng()` is thread-local, lock-free, and synchronous
(instantaneous) — no async or shared-state concerns. For deterministic tests,
make the strategy generic over `R: Rng` or inject a seeded `StdRng`; default to
`thread_rng`.

---

## 4. Concurrency / thread-safety (async + tokio)

**Per-upstream health → `DashMap`** (already a dependency):

```rust
pub struct HealthRegistry {
    states: DashMap<usize, ProviderState>,  // ProviderState reused from fallback.rs
    default_cooldown: Duration,
}
impl HealthRegistry {
    pub fn is_healthy(&self, idx: usize) -> bool { /* Normal, or Cooldown expired */ }
    pub fn trip(&self, idx: usize, over: Option<Duration>) { /* set Cooldown{until} */ }
    pub fn try_expire(&self, idx: usize) { /* get_mut → atomic check-and-clear */ }
    pub fn healthy_subset(&self, all: &[UpstreamRef]) -> Vec<UpstreamRef> { /* filter */ }
    pub fn remaining_secs(&self, idx: usize) -> u64 { /* for /metrics */ }
}
```

- `DashMap::get_mut(idx)` gives an **exclusive per-key guard**, preserving the
  ADR-006 atomic check-expiry-then-reset in `try_expire`.
- **Deviation from ADR-006** (which chose `tokio::sync::RwLock` to avoid blocking
  the runtime): DashMap uses sync per-shard locks, but critical sections here are
  a couple of `Instant` comparisons (nanoseconds) with no contention, so runtime
  blocking is a non-issue. **Hard rule: never hold a DashMap ref across an
  `.await`.** All health ops are pure-sync and instant, so this holds. If you'd
  rather stay literally within ADR-006, `Arc<RwLock<HashMap<usize, ProviderState>>>`
  is the conservative alternative (single global lock, coarser but async-native).
  DashMap is preferred: per-key sharding means tripping one upstream never blocks
  reads of another.

**Strategy is stateless / immutable**: `Arc<dyn RoutingStrategy>` shared freely.
`FallbackStrategy` holds nothing; `WeightedStrategy` holds only immutable config
(and uses `thread_rng`, no shared RNG). No locks on the hot selection path.

**Router**: `Arc<Router>` in `AppState` (replaces the current
`Arc<FallbackHandler<...>>`). `upstreams: Vec<Arc<Upstream>>` is immutable after
startup.

---

## 5. Rate-limit seam (sibling workstream)

Goal: a rate-limited upstream should look **unhealthy/cooling to the selector**
so both strategies compose without knowing rate limiting exists.

**Seam: make availability a composable predicate.** The router filters candidates
by the logical AND of all availability sources — cooldown-health today, a rate
limiter tomorrow:

```rust
pub trait Availability: Send + Sync {
    fn is_available(&self, idx: usize) -> bool;   // false == invisible to selection
}
// HealthRegistry impls it (cooldown). A RateLimiter impls it (bucket empty).
// Router: candidates = all.filter(|u| sources.iter().all(|s| s.is_available(u.idx)))
```

A locally rate-limited upstream returns `is_available == false` → excluded from
`healthy_subset` → invisible to **both** fallback and weighted. Weighted's
proportional redistribution then applies automatically.

**Two durations, one effect**: a server **429** trips the long `trip()` cooldown
(300s); a **proactive local RPM/token-bucket** limiter should mark the upstream
unavailable only for its short refill window (seconds), *without* a 300s cooldown.
Both surface identically as "not in the healthy set" — that's the composition
point. Keep the two mechanisms separate; only the `Availability` result is shared.

---

## 6. Streaming caveats

**Selection happens once, up front — identical for both strategies.** Weighted
picks a single upstream before any bytes flush; that upstream owns the request for
its lifetime. There is no mid-stream re-weighting.

**Failover window** — how far in a retry is still possible:

- **Non-streaming (buffered)**: fully failover-able. The whole cross-upstream
  retry loop is safe; the client sees nothing until a provider succeeds.
- **Streaming (SSE)**: failover is only safe **before the response body is handed
  to axum / before the first byte flushes**. The current providers already
  structure this correctly: `AnthropicProvider::send_streaming_request`
  (`anthropic.rs:230-253`) inspects the HTTP status and returns `Err`
  (429 / 4xx / 5xx) **before** returning the live stream. So a connect-time
  failure is a normal `Err` and the router's loop can re-select another healthy
  upstream — as long as that loop runs **inside** `dispatch` and only yields the
  `ProviderResponse::Stream` to `handle_messages` (`main.rs:509`) once a body is
  successfully open.
- **Once `Ok(Stream)` is returned and bytes flow, you cannot switch upstreams** —
  axum has already sent `200 OK` + partial SSE to the client. A mid-stream failure
  can only be surfaced as an SSE `error` event, never a transparent failover.
  Known limitation: Anthropic may send `200` then emit an `overloaded_error`
  event mid-stream — **not** failover-able here. Bedrock's impl already injects a
  mid-stream error as an SSE frame (`bedrock.rs:730-735`) rather than retrying.

**Design implication**: the router's connect-time retry loop must complete (open a
working stream body, or exhaust candidates) *before* returning to the handler.
This matches the current `FallbackHandler` shape, so no new hazard is introduced —
just preserve "await `send()` to success/terminal-error before handing the stream
outward."

**Per-upstream vs. cross-upstream retries**: keep same-upstream retries (e.g.
Bedrock's `2^n` timeout backoff, `bedrock.rs:644`) **inside the provider**; the
router loop handles only *failover to a different upstream*. Clean separation:
"retry me" = provider's job, "try someone else" = router's job.

---

## Sources / crates

- Current code: `claude-proxy-rs/src/fallback.rs`, `src/providers/mod.rs`,
  `src/providers/anthropic.rs`, `src/providers/bedrock.rs`, `src/main.rs`,
  `src/config.rs`, `Cargo.toml`.
- ADR-006 (`project_plans/claude-proxy-rs/decisions/ADR-006-fallback-state-machine-rwlock.md`)
  — rationale for the custom `RwLock<ProviderState>` cooldown machine.
- `rand` crate — `WeightedIndex` (`rand::distributions` in 0.8; `rand::distr::weighted`
  in 0.9) for weighted sampling; `thread_rng()` for lock-free per-task RNG.
- `dashmap` (already a dependency) — sharded per-key concurrent map for
  per-upstream health; never hold a guard across `.await`.
- OpenRouter provider routing (weight-based probabilistic split with
  health-based exclusion) — behavioral reference for `strategy = "weighted"`.
```
