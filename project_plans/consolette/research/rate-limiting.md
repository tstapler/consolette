# Research: Per-Upstream Token-Bucket Rate Limiting (RPM + TPM) in Rust

**Question (OQ-4):** How to implement per-upstream RPM/TPM rate limiting in Rust, and
what the `20-ratelimit.toml` config shape should be.

**Date:** 2026-07-17 · **Scope:** FR-4 (per-upstream rate limiting), coexisting with
FR-3 routing/cooldown.

---

## Recommendation (TL;DR)

1. **Use `governor` (0.10.4, MIT, 14 Jun 2026)** — it is the standard, well-maintained
   GCRA rate limiter and directly supports variable-cost checks (`check_n` /
   `until_n_ready`) needed for TPM.
2. **Do NOT use governor's *keyed* limiter for this.** A keyed limiter shares **one
   `Quota` across all keys** — but our requirement is *independent per-upstream limits*
   (different RPM/TPM per upstream). Instead hold **one `DirectRateLimiter` per upstream
   per dimension**, stored in a `DashMap<UpstreamName, UpstreamLimiter>` (the repo already
   depends on `dashmap`). Each `UpstreamLimiter` owns two direct limiters: `rpm` (cost 1)
   and `tpm` (cost = estimated tokens).
3. **Two dimensions = admit only if both admit.** Check TPM (`check_n`) and RPM
   (`check`); if either denies → the request is not admitted.
4. **`on_breach = "shed"` (default)** maps to `check()`/`check_n()` returning `Err` →
   surface a `Shed` decision that the router treats **exactly like a cooldown** (skip in
   `fallback`, exclude + redistribute in `weighted`). **`on_breach = "delay"`** maps to
   `tokio::time::timeout(max_delay, until_n_ready(...))` — bounded async wait, falling back
   to shed on timeout.
5. **Config:** table-keyed-by-name in `20-ratelimit.toml`
   (`[ratelimit.upstreams.<name>]` with `rpm`, `tpm`, `on_breach`, `max_delay_ms`), plus a
   `[ratelimit.defaults]` table. Plain TOML, loads unchanged in Python `tomllib`. The
   table-of-tables form (not array-of-tables) is chosen deliberately because it
   **deep-merges cleanly** across lexical `conf.d` files (FR-1.1: tables merge by key,
   arrays replace).

---

## 1. `governor` evaluation

- **Version:** `governor = "0.10"` → resolves to **0.10.4** (released 14 Jun 2026, MIT,
  maintained by `boinkor-net`, f.k.a. `ratelimit_meter`). It is the de-facto Rust rate
  limiter; `tower_governor` (Axum middleware) is built on it, so it is battle-tested.
- **Algorithm — GCRA, not a classic token bucket.** governor implements the Generic Cell
  Rate Algorithm: a *continuous* leaky-bucket equivalent. Instead of refilling discrete
  tokens on a timer, it stores a single "theoretical arrival time" and computes
  admission arithmetically on each check (lock-free, one `AtomicU64` of state per
  limiter). Practical implications vs a naive token bucket:
  - **Replenishment is smooth/continuous**, not bursty-on-tick. `Quota::per_minute(60)`
    replenishes ~1 cell/sec, not 60 cells at the top of each minute.
  - **Burst = the quota's cell count by default.** `Quota::per_minute(n)` allows a burst
    of up to `n` before throttling; use `.allow_burst(m)` to decouple burst from rate.
  - This is *better* for a proxy than a tick-based bucket — no thundering herd at tick
    boundaries.
- **Variable-cost checks — yes, first-class.** `check_n(NonZeroU32)` (direct) and
  `check_key_n(&key, NonZeroU32)` (keyed) consume N cells atomically ("all-or-nothing").
  This is exactly what TPM needs: `cost = estimated_token_count`.
- **`Send + Sync`:** `RateLimiter` is `Send + Sync`; share via `Arc`. Safe to call from
  many concurrent Axum handlers.

### Why direct-per-upstream, not keyed

governor's keyed limiter (`DefaultKeyedRateLimiter<K>`, internally a `DashMap`) is for
*many keys sharing the same quota* (e.g. per-client-IP, all at 60 rpm). Our upstreams have
**different** limits (Anthropic 50 rpm / 100k tpm; a gateway 200 rpm / 400k tpm), so the
keyed limiter's single-`Quota` model doesn't fit. We reproduce the "keyed" ergonomics
ourselves with `DashMap<String, UpstreamLimiter>`, giving each upstream its own quotas —
and we already depend on `dashmap`.

### `check_n` return shape & the TPM burst gotcha

`check_n` returns `Result<Result<(), NotUntil>, InsufficientCapacity>`:

- `Ok(Ok(()))` — admitted, N cells charged.
- `Ok(Err(NotUntil))` — denied now; `NotUntil::wait_time_from(clock.now())` gives when it
  *would* be admitted (useful to set a shed cooldown or decide delay-vs-shed).
- `Err(InsufficientCapacity)` — **N exceeds the bucket's max burst**, so it can *never*
  succeed. This is a real hazard for TPM: if a single request's estimated tokens exceed
  the TPM burst, every check errors. Mitigation (see §4): set the TPM quota's burst >= the
  largest expected single request (e.g. burst = full `tpm`, which `Quota::per_minute(tpm)`
  already gives), and treat any residual `InsufficientCapacity` as a shed (or clamp
  `est_tokens` to capacity) rather than a hard error.

---

## 2. Two-dimensional limiting (RPM AND TPM)

Compose two direct limiters per upstream; admit only if **both** admit. Charge TPM by
estimated tokens via `check_n(NonZeroU32)`.

```rust
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use governor::{
    Quota, RateLimiter,
    clock::{Clock, DefaultClock},
    state::{InMemoryState, NotKeyed},
};

/// A single-state, in-memory, default-clock direct limiter.
type Direct = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

pub enum Breach {
    Shed,
    Delay { max: Duration },
}

pub struct UpstreamLimiter {
    rpm: Option<Arc<Direct>>, // cost = 1 per request
    tpm: Option<Arc<Direct>>, // cost = estimated token count
    breach: Breach,
    clock: DefaultClock,
}

pub enum Admit {
    Allowed,
    Shed,                 // router treats this like a cooldown
    Delayed(Duration),    // admitted after waiting this long
}

impl UpstreamLimiter {
    pub async fn admit(&self, est_tokens: u32) -> Admit {
        match &self.breach {
            Breach::Shed => self.try_admit_now(est_tokens),
            Breach::Delay { max } => self.admit_with_delay(est_tokens, *max).await,
        }
    }

    /// Immediate, non-blocking check (shed path).
    fn try_admit_now(&self, est_tokens: u32) -> Admit {
        // Check TPM first: it is the variable/expensive dimension and the one most
        // likely to deny a large request. NOTE the ordering caveat below.
        if let (Some(t), Some(n)) = (&self.tpm, NonZeroU32::new(est_tokens)) {
            match t.check_n(n) {
                Ok(Ok(())) => {}
                Ok(Err(_not_until)) => return Admit::Shed,
                Err(_insufficient_capacity) => return Admit::Shed, // req > bucket burst
            }
        }
        if let Some(r) = &self.rpm {
            if r.check().is_err() {
                return Admit::Shed;
            }
        }
        Admit::Allowed
    }

    /// Bounded async wait (delay path).
    async fn admit_with_delay(&self, est_tokens: u32, max: Duration) -> Admit {
        let start = std::time::Instant::now();
        let fut = async {
            if let (Some(t), Some(n)) = (&self.tpm, NonZeroU32::new(est_tokens)) {
                // until_n_ready errors only on InsufficientCapacity (req > burst).
                if t.until_n_ready(n).await.is_err() {
                    return false;
                }
            }
            if let Some(r) = &self.rpm {
                r.until_ready().await;
            }
            true
        };
        match tokio::time::timeout(max, fut).await {
            Ok(true) => Admit::Delayed(start.elapsed()),
            _ => Admit::Shed, // exceeded max_delay OR capacity error → shed
        }
    }
}
```

**Ordering caveat (partial consumption).** governor commits state on a *successful*
check; there is no atomic two-dimension check. If TPM admits but RPM then denies, a TPM
charge has been "spent" on a request that wasn't sent (and vice-versa). For a
single-tenant personal proxy this drift is negligible. Two ways to minimize it:
- Check the dimension **more likely to deny first** (usually TPM for big requests), so the
  second check rarely fails after the first succeeds.
- Or, for stricter accounting, "peek" via `NotUntil`/`wait_time_from` before committing —
  but this is over-engineering for this use case; document the accepted drift instead.

**Delay pre-check optimization (optional).** Before awaiting, you can call `check_n` once;
if it returns `Err(NotUntil)` whose `wait_time_from(now) > max_delay`, shed immediately
instead of spinning up a timeout future.

---

## 3. Shed vs delay, and the router seam

| `on_breach` | governor call | Result | Router behavior |
|---|---|---|---|
| `shed` (default) | `check()` / `check_n()` | immediate `Err` → `Admit::Shed` | Treat upstream as **cooling-down for this request**: `fallback` skips to next; `weighted` excludes it and redistributes weight. |
| `delay` | `tokio::time::timeout(max_delay_ms, until_n_ready(...))` | admit after wait, or `Admit::Shed` on timeout | On success dispatch normally (record delay); on timeout behave as shed. |

**The seam.** The current single-purpose equivalent is `FallbackState` in
`src/fallback.rs` (`should_use_fallback()`, `enter_cooldown()`, `try_exit_cooldown()`,
`remaining_secs()`) — it models exactly "primary is temporarily unavailable, route
around it." The sibling router-trait workstream (OQ-3) generalizes this into a
**per-upstream health/availability check** consulted at selection time. The rate limiter
plugs into that same seam:

- The router asks, per candidate upstream, "is this upstream available for this request?"
  Today that's `!should_use_fallback()`; generalized it's a health/cooldown predicate.
- Insert a rate-limit gate immediately *before dispatch* to the chosen upstream:
  `RateLimiters::admit(upstream_name, est_tokens)`.
  - `Admit::Allowed` / `Admit::Delayed` → dispatch.
  - `Admit::Shed` → treat identically to a cooldown for this attempt: the router advances
    to the next fallback candidate or re-picks among the remaining weighted-healthy set.

So "shed" does **not** need its own routing code path — it reuses the cooldown/health
exclusion the router already implements for 429s (FR-3.4). Optionally, on shed the limiter
can set a short real cooldown until `NotUntil::wait_time_from(now)`, so a hot upstream is
skipped for the whole replenishment window rather than re-checked every request.

Recommend defining a small trait so the router depends on an interface, not governor:

```rust
#[async_trait::async_trait]
pub trait AdmissionControl: Send + Sync {
    /// est_tokens = tiktoken-rs estimate for the request body.
    async fn admit(&self, upstream: &str, est_tokens: u32) -> Admit;
}
```

---

## 4. Config shape — `20-ratelimit.toml`

Table-keyed-by-upstream-name, with a defaults table. Plain TOML; `tomllib`-safe (bare keys
allow hyphens, so `model-gateway` needs no quoting).

```toml
# 20-ratelimit.toml — per-upstream RPM/TPM limits (FR-4)

# Applied to any upstream that omits a field.
[ratelimit.defaults]
on_breach    = "shed"   # "shed" | "delay"
max_delay_ms = 2000     # only used when on_breach = "delay"

[ratelimit.upstreams.anthropic]
rpm       = 50
tpm       = 100000
on_breach = "shed"      # limited → route falls through to Bedrock (reuses cooldown path)

[ratelimit.upstreams.bedrock]
rpm = 200
tpm = 400000
# on_breach + max_delay_ms inherited from [ratelimit.defaults]

[ratelimit.upstreams.model-gateway]
rpm          = 200
tpm          = 400000
on_breach    = "delay"  # smooth into the gateway's quota instead of shedding
max_delay_ms = 3000
```

Semantics:
- **Omitted `rpm`** → no RPM limit for that upstream (skip that governor).
- **Omitted `tpm`** → no TPM limit.
- **`on_breach` / `max_delay_ms`** fall back to `[ratelimit.defaults]`, then to built-in
  defaults (`shed`, `2000`).
- An upstream **absent** from `[ratelimit.upstreams]` is **unlimited** (no limiter built).

**Why table-of-tables, not array-of-tables.** FR-1.1 deep-merge merges *tables* by key but
*replaces* arrays. With `[ratelimit.upstreams.<name>]`, a later `conf.d` file can override
just one upstream's `tpm` and leave the rest intact. With `[[ratelimit]]` arrays, a later
file's array would replace the whole list. Table form is strictly better for layering.

Rust side (serde):

```rust
use std::collections::HashMap;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct RateLimitConfig {
    #[serde(default)]
    pub defaults: RateLimitDefaults,
    #[serde(default)]
    pub upstreams: HashMap<String, UpstreamLimit>,
}

#[derive(Debug, Deserialize)]
pub struct RateLimitDefaults {
    #[serde(default = "default_breach")]
    pub on_breach: BreachKind,
    #[serde(default = "default_max_delay_ms")]
    pub max_delay_ms: u64,
}

#[derive(Debug, Deserialize)]
pub struct UpstreamLimit {
    pub rpm: Option<u32>,
    pub tpm: Option<u32>,
    pub on_breach: Option<BreachKind>,
    pub max_delay_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BreachKind { Shed, Delay }

fn default_breach() -> BreachKind { BreachKind::Shed }
fn default_max_delay_ms() -> u64 { 2000 }
```

Python parity check (`python3 -c 'import tomllib; tomllib.load(open("20-ratelimit.toml","rb"))'`)
succeeds — no Rust-only constructs.

---

## 5. Concurrency, keying, and construction

- **Keying.** `DashMap<String /* upstream name */, UpstreamLimiter>`. `DashMap` gives
  lock-free-ish sharded concurrent reads; the limiters inside are themselves `Send+Sync`,
  so `admit()` is a cheap concurrent read + an atomic CAS inside governor. Wrap the direct
  limiters in `Arc` if you need to hand them out; otherwise store by value in the map.
- **Build from config at startup.** Iterate `RateLimitConfig.upstreams`, and for each
  build a `UpstreamLimiter` with `Quota::per_minute(NonZeroU32)` for whichever of
  rpm/tpm are present:

  ```rust
  fn quota_per_min(v: u32) -> Option<Quota> {
      NonZeroU32::new(v).map(Quota::per_minute) // default burst == v
  }

  // For TPM you generally want burst >= largest single request so big requests
  // don't hit InsufficientCapacity. Quota::per_minute(tpm) already sets burst == tpm,
  // which is a sensible default; expose an override only if needed:
  //   Quota::per_minute(rate_nz).allow_burst(burst_nz)
  ```
- **`NonZeroU32`.** governor requires non-zero quotas and non-zero `check_n` costs. Guard
  `est_tokens == 0` (empty/parse-fail) — either skip the TPM check or charge 1. The repo
  can add `nonzero_ext = "0.3"` for the `nonzero!` literal macro, or just use
  `NonZeroU32::new(x)`.
- **Dynamic reload (FR-1.6, stretch).** Direct limiters have no live quota mutation. On
  config change, **rebuild the affected `UpstreamLimiter` and swap it into the `DashMap`**
  (`map.insert(name, new)`). This resets that upstream's bucket state (acceptable — a
  personal proxy reload is rare and in-flight requests already hold their decision).
- **Dependency note.** governor 0.10 pulls `dashmap ^6.1` internally; the repo currently
  pins `dashmap = "5"` directly. Both can coexist in the tree, but to share `DashMap`
  types / dedupe the dependency, **bump the repo's direct `dashmap` to `6`** when adding
  governor. `tiktoken-rs = "0.5"` is already present for the token estimate (reuse the
  existing tokenizer/counter used by the `count_tokens`/compression paths).

---

## 6. Metrics (`/metrics` + `/dashboard`, FR-4.5)

The existing `ProxyMetrics` (`src/metrics/counters.rs`) hardcodes per-provider fields
(`requests_anthropic`, `requests_bedrock`). For arbitrary named upstreams, add a
**per-upstream rate-limit metrics map** rather than more hardcoded fields:

```rust
#[derive(Default)]
pub struct UpstreamRateMetrics {
    pub allowed:        AtomicU64,
    pub shed:           AtomicU64,
    pub delayed:        AtomicU64,
    pub tokens_charged: AtomicU64,   // sum of est_tokens admitted (TPM visibility)
    pub delay_ms_sum:   AtomicU64,   // for avg wait when on_breach = "delay"
}

// on ProxyMetrics (or a sibling struct):
pub rate_limits: DashMap<String, UpstreamRateMetrics>,
```

Increment in the admit path: `Allowed` → `allowed += 1`, `tokens_charged += est`;
`Shed` → `shed += 1`; `Delayed(d)` → `delayed += 1`, `delay_ms_sum += d`.

Expose under a `"ratelimit"` key in `to_json()`, per upstream:

```json
"ratelimit": {
  "anthropic": {
    "rpm_limit": 50, "tpm_limit": 100000,
    "allowed": 1234, "shed": 12, "delayed": 0,
    "tokens_charged": 5120000, "avg_delay_ms": 0,
    "in_cooldown": false
  }
}
```

Reuse the existing `err_rate_limit` counter for the aggregate too. If a shed sets a real
cooldown, surface remaining seconds like `FallbackState::remaining_secs()` does today.

---

## Open follow-ups / risks

- **TPM > burst:** requests whose estimate exceeds a small `tpm` always `InsufficientCapacity`.
  Default `Quota::per_minute(tpm)` (burst == tpm) covers up to a full minute's budget in
  one request; document that `tpm` must be >= the largest expected single request, and
  treat residual capacity errors as shed.
- **Token estimate accuracy:** `tiktoken-rs` (OpenAI BPE) is an *estimate* for
  Anthropic/Bedrock/gateway models; TPM is approximate. Acceptable for a personal
  guardrail; note it.
- **Two-dimension atomicity:** minor RPM/TPM accounting drift on cross-dimension denial
  (see §2) — accepted for single-tenant use.

---

## Sources

- governor crate docs (0.10.4, 14 Jun 2026): https://docs.rs/governor/latest/governor/
- `RateLimiter` (check/check_n/until_ready/until_n_ready, keyed variants): https://docs.rs/governor/latest/governor/struct.RateLimiter.html
- `Quota` (per_minute / allow_burst / burst semantics): https://docs.rs/governor/latest/governor/struct.Quota.html
- governor user's guide (GCRA, keyed vs direct, burst): https://docs.rs/governor/latest/governor/_guide/index.html
- governor repo: https://github.com/boinkor-net/governor
- "How to Implement Rate Limiting in Rust Without External Services" (governor keyed example, Jan 2026): https://oneuptime.com/blog/post/2026-01-07-rust-rate-limiting/view
- Shuttle "Implementing API Rate Limiting in Rust" (governor + tower): https://www.shuttle.dev/blog/2024/02/22/api-rate-limiting-rust
- Repo context: `stapler-scripts/claude-proxy-rs/Cargo.toml` (dashmap 5, tiktoken-rs 0.5),
  `src/fallback.rs` (cooldown seam), `src/metrics/counters.rs` (metrics shape).
