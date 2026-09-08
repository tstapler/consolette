# Research: Build vs. Buy — Gemini/Antigravity Provider

**Phase**: SDD Phase 2 (Research Agent 6)
**Project**: `gemini-provider`
**Date**: 2026-09-04

## Question

Should consolette get Gemini/Antigravity support via (1) an existing Rust crate,
(2) a sidecar proxy process fronted by the existing `OpenaiProvider`, (3) a
hand-written native `GeminiProvider` derived from reading a reference
implementation, or (4) a fork/vendor of an existing reverse-engineering
project?

## 1. Existing OSS Rust crate

Searched crates.io/GitHub for Gemini client crates: `gemini-rs`, `gemini-rust`,
`google-generative-ai-rs`, `google-gemini-rs`, `aigw-gemini`, `adk-gemini`.

Every one of them targets the **public** documented surface —
`generativelanguage.googleapis.com` (AI Studio key) or Vertex AI
(`*-aiplatform.googleapis.com`, service-account OAuth). None target
`cloudcode-pa.googleapis.com`'s internal `v1internal:streamGenerateContent`
Cloud Code Assist protocol — unsurprising, since that endpoint is
undocumented and explicitly not a supported external surface. `google-generative-ai-rs`
(avastmick) is reported unmaintained ("Gemini is now OpenAI-API compatible,"
per its own maintenance note); `gemini-rs` looks the most actively maintained
of the bunch but is equally scoped to the public API.

An Antigravity-issued token is **not valid** against `generativelanguage.googleapis.com`
(per the requirements doc's Decision section — confirmed by why `antigravity-cli`
exists as a separate official tool in the first place). So even the best-maintained
of these crates would authenticate against the wrong endpoint and fail outright —
this isn't a translation-quality gap, it's a hard protocol mismatch.

**Keyring crate** (separate sub-question, needed regardless of the
build-vs-buy answer for the wire protocol): `keyring` v4.2.0 (released
2026-08-29) is mature — ~4M downloads/month, used in 1,274 crates, dual
MIT/Apache-2.0, actively maintained (Dan Brotsky + co-maintainer). On Linux it
supports the D-Bus Secret Service (gnome-keyring, the backend Manjaro/GNOME
uses) via `zbus-secret-service-keyring-store`, plus a `linux-keyutils`
backend. **Prior art already exists in Tyler's own ecosystem**: a
`KeyringTokenStore` in `~/.stapler-squad/workspaces/.../crates/infrastructure/src/persistence/keyring_token_store.rs`
(`taste-playlist` project) wraps `keyring` v4 for OAuth-token storage,
including the `spawn_blocking` wrapper needed because `keyring`'s API is
synchronous (a Linux Secret Service call blocks on a D-Bus round-trip), and
explicit handling for headless environments with no Secret Service daemon —
this is a directly reusable pattern for Phase 3, not just an abstract
recommendation.

**Consolette-specific finding relevant to this decision**: `SecretRef::Keychain`
in `src/auth/mod.rs:53` currently shells out to macOS's `security
find-generic-password` and is explicitly **macOS-only** ("no keychain crate
dependency needed for a single read-only lookup" — true when every current
provider's secrets live in env vars or macOS Keychain). Reading
`antigravity-cli`'s Linux-keyring-stored token means this codepath needs a
Linux implementation for the first time — the `keyring` crate is the natural
way to add it.

- **Pros**: `keyring` crate is a clean, low-risk, well-trodden dependency
  addition for the token-storage side of the problem.
- **Cons**: No existing crate solves the actual hard problem (Gemini/Cloud-Code-internal
  wire translation) — zero reuse available there.
- **Verdict**: **Not recommended** for the wire protocol (nothing exists to
  buy). **Recommended** for token storage — add `keyring` as a dependency
  when Phase 3 implements the auth path, following the `KeyringTokenStore`
  pattern already proven in `taste-playlist`.

## 2. SaaS/managed API or sidecar proxy (adapting a reverse-engineering project)

The requirements doc names `elad12390/antigravity-proxy` as the reference
project. On inspection it is **stale and non-functional**: its own README
states `STATUS: Work In Progress - NOT WORKING`, archived 2025-12-15, targets
an old sandbox subdomain (`daily-cloudcode-pa.sandbox.googleapis.com`), and
documents that it never solved the actual interception problem (certificate
pinning + a socket-based/TLS handshake defeated its MITM approach). It is a
Python/mitmproxy experiment, not a working proxy, and does **not** expose an
OpenAI- or Anthropic-compatible surface. **This reference project should be
discarded — the plan.md written in Phase 3 should not cite it as prior art
to port from.**

Searching further surfaced three more recent community proxies that *do*
work against current Antigravity and *do* expose OpenAI/Anthropic-compatible
surfaces:

| Project | Surface exposed | Auth story | Language | Status (as of 2026-09-04) |
|---|---|---|---|---|
| `frieser/antigravity-proxy` | OpenAI-compatible `/v1/chat/completions`, SSE | Reads OAuth tokens from local `antigravity-accounts.json`, does account rotation/health scoring across multiple accounts | TypeScript/Bun, Docker | **Archived 2026-08-10** (read-only) |
| `NikkeTryHard/zerogravity` | OpenAI (`/v1/chat/completions`), Anthropic (`/v1/messages`), Gemini v1beta passthrough | Requires `zg extract` to pull refresh tokens out of the Antigravity desktop app; explicitly warns third-party OAuth flows get fingerprinted/flagged differently | Compiled language, Docker-only | **Discontinued** ("interest lower than hoped"), 657★/48 forks, 69 commits |
| `zhe-gu/zero-gravity`, `alessandrobrunoh/openai-proxy-for-antigravity` | Similar OpenAI-compatible surfaces | Not deep-dived (pattern already established by the two above) | — | Not assessed further — same category |

The pattern across **every** project in this space, old and new, is the
same: reverse-engineered, unofficial, and short-lived — median project
lifetime looks to be well under a year before being archived or
discontinued. This is a symptom of the underlying instability the
requirements doc already flagged (Google can change the internal protocol
without notice), not a one-off. Running any of them as a sidecar means
consolette's Gemini support is hostage to a third party's abandonment
schedule on top of Google's own protocol churn — two independent failure
sources instead of one.

Running one as a sidecar behind consolette's existing `OpenaiProvider` is
technically the least-code path (zero new Rust translation code — just an
`UpstreamKind::Openai` entry pointed at `localhost:<port>`), but:

- **Pros**: Fastest to a working state; leans entirely on OpenAI-compatible
  code paths consolette already has; no new Rust protocol-translation code
  to write or maintain.
- **Cons**: (a) Every candidate is archived/discontinued — adopting a dead
  project as a runtime dependency for ongoing personal infra is itself a
  maintenance liability, arguably worse than writing native code, since bugs
  in someone else's abandoned TypeScript/Docker image can't be fixed
  upstream. (b) Introduces an out-of-process sidecar, Docker or a Bun/Node
  runtime, a footprint consolette otherwise avoids entirely (single Rust
  binary). (c) `zerogravity`'s own docs warn that extracting tokens outside
  the official OAuth flow (which is exactly what `antigravity-cli`'s
  official OAuth flow is *not* doing here — these proxies extract tokens a
  different way) risks account flagging — a real risk to Tyler's actual
  Antigravity subscription, which the requirements doc's "official OAuth
  only" constraint was specifically written to avoid. (d) None of them fit
  the requirement to authenticate via `antigravity-cli`'s official,
  keyring-stored OAuth token — they each have their own separate
  token-acquisition mechanism, which reintroduces exactly the
  harvested-credential risk the requirements doc rejected in its Decision
  section.
- **Verdict**: **Not recommended.** Every option in this category is
  unmaintained today, and the two most-starred/most-recent ones use
  auth mechanisms the requirements explicitly ruled out (Constraints: "Must
  not embed a harvested/third-party OAuth client secret" / must go through
  official `antigravity-cli` OAuth). Adopting one would both violate a
  stated constraint and add a dead dependency.

## 3. LLM-generated implementation vs. battle-tested reference

Given #1 (no crate solves the real problem) and #2 (no sidecar candidate is
usable as-is), the real choice is how to *write* the native Rust translation
code, not whether to write it:

**(a) Hand-write in Rust, using `frieser/antigravity-proxy` and
`NikkeTryHard/zerogravity`'s source as reference material** (not
`elad12390`'s — it never worked). Both are readable, request/response shapes
for `/v1/chat/completions` → Cloud-Code-internal translation are concrete and
inspectable even though the projects are archived. Archived just means
"frozen," not "unavailable" — the source is still a legitimate reverse-engineering
reference, same as reading protocol-capture traffic directly.

**(b) Run one as a sidecar** — already rejected in #2.

**(c) Hybrid — port only the request/response shape translation from a
reference project, write auth (keyring read + refresh) fresh against
`antigravity-cli`'s actual official flow.** This is really (a) with an
explicit acknowledgment that the auth path can't be borrowed from any
existing proxy anyway, since none of them use official `antigravity-cli`
OAuth (see #2d) — auth code is being written fresh regardless of which
translation approach is chosen.

Given #2 eliminates the sidecar option, the real comparison collapses to:
write the translation layer in Rust (informed by reading 2-3 reference
implementations for the request/response shape, since none is authoritative)
vs. not building this at all. There is no "battle-tested" option on the
table — every implementation of this protocol in existence, official or
community, is derived from reverse-engineering, because Google has not
published one. The risk profile is the same whether Tyler's Rust code or a
TypeScript sidecar does the translating; the only variable is which codebase
carries that risk and how easy it is to fix when the protocol drifts.

- **Verdict**: **(a)/(c) recommended** — hand-write the Rust translation,
  using `frieser/antigravity-proxy` and `NikkeTryHard/zerogravity` source
  (both MIT-equivalent-in-spirit per #4 below — verify each repo's actual
  LICENSE file before copying any code verbatim, not just referencing shape)
  as reference material for the request/response schema and streaming
  framing, while writing auth fresh against the official `antigravity-cli`
  OAuth/keyring flow. This keeps the one component under Tyler's direct
  control — a `ProviderError` fail-closed classification and `HealthRegistry`
  cooldown, per the requirements' resilience requirement, is something Tyler
  can add to his own Rust code but can't add to an abandoned TypeScript
  sidecar he doesn't maintain.

## 4. Fork or vendor licensing

- `elad12390/antigravity-proxy`: **MIT** — permissively licensed, but the
  project doesn't work, so there's nothing functional to fork; only useful
  as a very early historical reference for the endpoint shape (and even that
  is stale — it targets a `sandbox` subdomain).
- `frieser/antigravity-proxy`: license not surfaced by the README fetch in
  this research pass — **verify the actual `LICENSE` file in Phase 3** before
  reusing any code beyond "read for reference."
- `NikkeTryHard/zerogravity`: **MIT**, confirmed via README.
- General guidance regardless of license: these are all reverse-engineered
  implementations, so copying code verbatim (versus reading it to understand
  the wire shape and writing consolette's own translation in idiomatic Rust
  matching `anthropic.rs`/`openai.rs`'s existing structure) is not obviously
  desirable even where the license permits it — none of them use
  consolette's `Provider` trait, `ProviderError` classification, or
  ADR-004 two-client split, so a line-for-line port would need heavy rewriting
  anyway. Treat them as **reference material for protocol shape**, not as
  vendor-and-adapt candidates.
- **Verdict**: **Viable but low-value** to fork/vendor directly; **recommended**
  to read as reference material under whichever license each carries (MIT
  confirmed for two of the three).

## Final Recommendation

> **Superseded in part by ADR-001** (`project_plans/gemini-provider/decisions/ADR-001-gemini-auth-token-source.md`):
> the `keyring` crate / `SecretRef::Keyring` recommendation below was written before `research/stack.md`'s
> live `secret-tool search` probe found zero keyring entries and confirmed the token lives in a plain
> JSON file. ADR-001 is the final decision on auth-token source (wrapper script + `AuthMethod::Exec`,
> no keyring dependency) — see that ADR, not this section, for what the plan actually implements. The
> native-`GeminiProvider`-in-Rust conclusion below (the wire-protocol build-vs-buy question) is
> unaffected and still stands.

**Build a native `GeminiProvider` in Rust** (option 3a), informed by reading
`frieser/antigravity-proxy` and `NikkeTryHard/zerogravity`'s source for the
Cloud-Code-internal request/response and streaming-framing shape — not
`elad12390/antigravity-proxy`, which the requirements doc names but which
turns out to be a dead, non-functional MITM experiment and should be dropped
from the Phase 3 plan's reference list. Add the `keyring` crate (v4,
MIT/Apache-2.0, already proven in Tyler's `taste-playlist` project's
`KeyringTokenStore`) to read `antigravity-cli`'s Secret-Service-stored OAuth
token directly on Linux, since `antigravity-cli` has **no token-printing
subcommand** (confirmed: only `agy` and `/logout` are documented; `agy
--print` runs a model prompt, not a credential export) — the
`AuthMethod::Exec` credential-helper path from the requirements' "preferred
approach" is **not viable as originally hoped** and Phase 3 should plan
around direct keyring/file reads from the start, not treat it as a fallback
to confirm.

This also surfaces a load-bearing consolette-side finding for Phase 3: the
existing `SecretRef::Keychain` (`src/auth/mod.rs`) is hardcoded to macOS's
`security` CLI and does nothing on Linux — the Gemini provider's auth code
will need genuinely new Linux-keyring-read logic, not a config-only reuse of
an existing secret-resolution path. Do not scope Phase 3 as "reuse
`AuthMethod::Exec`" without first deciding whether to (i) extend
`SecretRef`/`SecretResolver` with a Linux-capable keyring variant reusable by
future providers, or (ii) keep the keyring read fully internal to
`GeminiProvider` since it's a one-off need today. Recommend (i) if the
`keyring` crate is being added anyway — the marginal cost of a generic
`SecretRef::Keyring` is small and benefits future non-Gemini upstreams too,
but this is a Phase 3 architecture call, not a Phase 2 research conclusion.

Do not adopt any sidecar/proxy option — every community reverse-engineering
project surveyed (4 of them) is archived or discontinued as of this
research, and the two most functional ones use token-acquisition mechanisms
the requirements doc's Constraints section already ruled out. Building
native keeps the fail-closed/`ProviderError`/`HealthRegistry`-cooldown
resilience requirements (which are consolette-specific, not something an
external proxy provides) inside code Tyler actually maintains.
