# Research: Pitfalls — gemini-provider

Research Agent 4 (Pitfalls), SDD Phase 2. Scope: known failure modes and risks
for adding a `GeminiProvider` that talks to Google's undocumented
`cloudcode-pa.googleapis.com` `v1internal:streamGenerateContent` protocol via
an `antigravity-cli`-issued OAuth token. See `requirements.md` for full
context; this doc does not repeat the Decision/Rabbit Holes sections there,
it substantiates and extends them with evidence.

## 1. Undocumented-API pitfalls — general pattern, with prior art

Cross-cutting pattern seen across every reverse-engineered "IDE companion"
backend researched below: **the backend is not a stable API contract, it's an
implementation detail of one specific first-party client, and the vendor
polices that.** Concretely, four failure classes recur:

- **Silent protocol drift.** Endpoint paths, request/response shapes, and
  model-name mappings change without a changelog. Real examples against this
  exact endpoint family:
  - [`shekohex/opencode-google-antigravity-auth#9`](https://github.com/shekohex/opencode-google-antigravity-auth/issues/9) and the
    [`NoeFabris/opencode-antigravity-auth`](https://github.com/NoeFabris/opencode-antigravity-auth) issue series (#103, #115, #510, #416) — repeated
    `404 Requested entity was not found` on `cloudcode-pa.googleapis.com/v1internal:streamGenerateContent`
    caused by the internal API expecting a different model-name string than
    the one the client sent, especially around Gemini 3 model rollouts.
    `shekohex`'s repo is now archived/read-only — a maintainer simply stopped
    chasing the drift.
  - [`lbjlaq/Antigravity-Manager#1904`](https://github.com/lbjlaq/Antigravity-Manager/issues/1904), [`#1496`](https://github.com/lbjlaq/Antigravity-Manager/issues/1496), [`#2115`](https://github.com/lbjlaq/Antigravity-Manager/issues/2115), [`#3049`](https://github.com/lbjlaq/Antigravity-Manager/issues/3049) — a mix of
    mid-stream connection resets, "invalid argument" on `Antigravity-Request`,
    and a "Cloud Code Private API has not been used in project ... before or
    it is disabled" error — i.e. undocumented per-project enablement state
    that isn't visible until a request fails.
  - [`elad12390/antigravity-proxy`](https://github.com/elad12390/antigravity-proxy) (named directly in the requirements doc as
    prior art) — its own `README`/`IMPORTANT_NOTES.md` state the project
    **does not work against the current Antigravity build**: Antigravity's
    connection flow is a handshake followed by a socket-based/TLS flow, not a
    simple HTTPS POST a basic MITM proxy can intercept, and the internal
    endpoint moved to `daily-cloudcode-pa.sandbox.googleapis.com` in some
    builds. The project is explicitly framed as "educational/research only,"
    not a maintained integration — a warning sign about how fast this surface
    moves for anyone treating it as stable infrastructure.
  - There is also an official acknowledgment that the client is expected to
    track Google's own version skew: a [Google Antigravity forum thread](https://discuss.ai.google.dev/t/bug-remote-servers-ssh-wsl-using-staging-endpoint-daily-cloudcode-pa-instead-of-production-fix-included/139412)
    documents remote/SSH sessions being silently routed to the **staging**
    endpoint (`daily-cloudcode-pa...`) instead of production, purely as a
    side effect of environment detection logic in the official client —
    something a from-scratch consolette implementation must not accidentally
    reproduce or diverge from without knowing which one it's hitting.

- **IDE-mimicking header/fingerprint requirements.** [`vahapogut/antigravity-add-model`](https://github.com/vahapogut/antigravity-add-model)'s
  README documents the real request shape: `User-Agent: antigravity/1.15.8
  windows/amd64`, an `X-Goog-Api-Client` header, and a `Client-Metadata` JSON
  blob (`ideType: "ANTIGRAVITY"`, `platform`, `pluginType: "GEMINI"`). This
  confirms the requirements doc's open question — the internal endpoint is
  keyed off more than the bearer token, and those values encode a specific
  IDE build. Whether official `antigravity-cli`'s traffic still requires
  equivalent headers (vs. official-client tokens getting a pass) is unverified
  and stays an open question for Phase 2's live-traffic capture.

- **Anti-abuse/fingerprinting has escalated to TLS-level.** [`decolua/9router#1138`](https://github.com/decolua/9router/issues/1138),
  titled "implement multi-layer detection evasion (TLS JA3, session headers,
  and payload fingerprinting)" for Antigravity support, is direct evidence
  that JA3/TLS fingerprinting is an active detection vector on this backend
  today, not a theoretical concern — a plain `reqwest` client's default TLS
  stack fingerprint differs from Antigravity's Go/Electron client stack.
  `consolette`'s `reqwest::Client` (ADR-004 two-client split) will present a
  different JA3 fingerprint than the real Antigravity IDE/CLI by default.

- **Comparable products (Cursor, Windsurf) show the same arc.** Cursor
  actively blocks Agent-mode requests carrying custom API keys and hardcodes
  its router to force its own billing backend, and its ToS explicitly
  prohibits reverse engineering/decompiling ([Cursor forum thread](https://forum.cursor.com/t/api-sdk-terms-of-use-question/159741)). Reverse-engineering writeups exist for both
  ([TensorZero: Reverse Engineering Cursor's LLM Client](https://www.tensorzero.com/blog/reverse-engineering-cursors-llm-client/),
  [dev.to: Reverse-Engineered Cursor IDE to run on GitHub Copilot](https://dev.to/jacksonkasi/how-i-reverse-engineered-cursor-ide-to-run-on-github-copilot-a-proxy-architecture-deep-dive-2jin),
  [`dwgx/WindsurfAPI`](https://github.com/dwgx/WindsurfAPI)), confirming this
  is a well-trodden and continually-contested category, not unique to Google.

**Bottom line for this project**: treat the wire format, model IDs, and
required headers as a moving target that *will* drift on Google's timeline,
not consolette's. Detection is by observation (errors), not by advance
notice.

## 2. OAuth token lifecycle pitfalls

- **Keyring access fails specifically in headless/SSH sessions** — directly
  relevant since this machine's `bootstrap`'s `ssh-bastion-client` role means
  Claude Code (and by extension anything shelling out to `antigravity-cli`)
  sometimes runs over SSH with no desktop session:
  - [`google-antigravity/antigravity-cli#479`](https://github.com/google-antigravity/antigravity-cli/issues/479) — "File-based token
    storage is write-only: fresh process rejects a valid
    `antigravity-oauth-token` in headless Linux containers." The CLI's own
    token-source initialization only reads from the OS keyring or a live
    in-process OAuth completion — the file-based store it *writes* to in
    headless mode is never read back by a fresh process. This is a
    Google-side bug in `antigravity-cli` itself, but it means "run
    `antigravity-cli` once to get a token, then read the file it wrote" is
    not reliably safe to assume as a fallback.
  - [`#57`](https://github.com/google-antigravity/antigravity-cli/issues/57) — the CLI does not persist auth across terminal sessions in headless
    Linux/WSL: works for the duration of one terminal session, then forces
    full browser re-auth on the next. Reported as a severe regression vs. the
    legacy `gemini-cli`.
  - [`#632`](https://github.com/google-antigravity/antigravity-cli/issues/632) — no supported token/env-var auth path for headless/Docker/CI —
    env vars like `AGY_API_KEY`/`GEMINI_API_KEY` are ignored by the CLI.
  - General Linux background: headless/SSH sessions commonly have **no
    D-Bus Secret Service running at all** (no `gnome-keyring-daemon`/KWallet),
    which is the underlying reason keyring-backed tools fail outside a
    desktop session — a broadly known pattern (see the ArchWiki
    GNOME/Keyring page and multiple CLI tools' open issues, e.g.
    [`jonhadfield/sn-cli#94`](https://github.com/jonhadfield/sn-cli/issues/94)).
  - **Implication for the `AuthMethod::Exec` design**: if `antigravity-cli`'s
    token-printing subcommand (if one exists) itself depends on keyring
    access, an `Exec`-based credential helper invoked from a headless
    consolette instance (e.g. running as a long-lived daemon over SSH,
    or under systemd with no session keyring) can fail in exactly the way
    `#479`/`#57` describe — not a "bad token," but "no token source at all."
    This needs to be verified for real during Phase 2's `antigravity-cli`
    install/inspection step, and the failure mode needs a distinct,
    actionable `ProviderError::Auth` message (see §5) rather than surfacing
    as an opaque exec-helper failure.

- **Refresh-token race conditions under concurrent access.** Rotating
  (single-use) OAuth refresh tokens break when two processes race to refresh
  the same token: the first consumes it, the second gets a 404/invalidated
  error, and some providers respond to the replay by revoking the whole token
  family (see [WorkOS: "OAuth token refresh has a race condition"](https://workos.com/blog/oauth-refresh-token-race-condition),
  [Nango: concurrency with OAuth token refreshes](https://nango.dev/blog/concurrency-with-oauth-token-refreshes/),
  and directly analogous real incidents in `anthropics/claude-code`
  ([#25609](https://github.com/anthropics/claude-code/issues/25609), [#27933](https://github.com/anthropics/claude-code/issues/27933)) and
  `openai/codex` ([#10332](https://github.com/openai/codex/issues/10332))). Relevant here because
  `antigravity-cli` itself may run concurrently with consolette's `Exec`
  credential helper — e.g. the user manually runs `antigravity-cli login` or
  the Antigravity IDE is open at the same time consolette shells out for a
  fresh token — and if the underlying OAuth token is rotating, two refreshers
  racing can revoke the token consolette is mid-request with.
  [`openclaw/openclaw#7549`](https://github.com/openclaw/openclaw/issues/7549) ("Google Antigravity OAuth refresh token not
  being used - forces re-auth every hour") is a concrete report of this
  provider family's refresh path being unreliable in practice.
  `AuthMethod::Exec`'s existing `cache_ttl_secs` (see
  [`src/auth/mod.rs:118-140`](src/auth/mod.rs#L118-L140)) already reduces how
  often consolette itself calls the helper, which narrows this window but
  does not eliminate races with the IDE or a manual CLI invocation running in
  parallel.

- **External revocation mid-session (logout, admin action, or a ToS-driven
  suspension — see §4) leaves a long-running consolette process holding a
  now-dead token** until its next `Exec` cache expiry. There's no push
  notification from Google; the first signal is the next request failing.
  This is functionally identical to Bedrock's already-solved "AWS SSO session
  expired" case (`BedrockProvider::do_sso_login`,
  [`src/providers/bedrock.rs:572-605`](src/providers/bedrock.rs#L572-L605)) — that code already
  distinguishes "no TTY, can't interactively re-auth" (returns
  `ProviderError::Auth` with an actionable message telling the operator to
  run `aws sso login`) from "have a TTY, attempt it inline." The Gemini
  provider should mirror this shape but the corresponding manual step is
  `antigravity-cli login` (or whatever verb Phase 2's CLI inspection finds) —
  and, per the Observability Requirements in `requirements.md`, this needs to
  be logged as a distinct "upstream fully down until human re-auth" event,
  not folded into ordinary per-request auth-error logging.

## 3. This codebase: what happens today on an unparseable 200 response

Read `src/providers/bedrock.rs` and `src/routing/health.rs` in full for this
question. Findings:

- **Fail-closed-on-parse-failure is already the pattern, and it's cheap to
  follow.** Both Bedrock code paths that deserialize a response body treat a
  `serde_json` parse failure as a hard `ProviderError::Upstream`, never as a
  best-effort partial parse:
  - Non-streaming: [`src/providers/bedrock.rs:809-813`](src/providers/bedrock.rs#L809-L813) —
    `serde_json::from_slice::<Value>` failure becomes
    `ProviderError::Upstream { status: 500, body: "Failed to parse Bedrock response: {e}" }`.
  - Streaming: [`src/providers/bedrock.rs:719-735`](src/providers/bedrock.rs#L719-L735) — a chunk that
    doesn't parse as JSON sends `Err(ProviderError::Upstream{..})` down the
    channel and **breaks the stream** rather than emitting a corrupted or
    partial SSE event. Nothing here tries to guess at meaning from a body
    that doesn't match the expected shape — a `GeminiProvider` should do
    exactly the same for both its non-streaming JSON body and its streaming
    chunk framing (whatever that framing turns out to be — SSE, JSON-lines,
    or otherwise, per the open question in `requirements.md`).

- **But a parse failure is classified as "transient," not "trip cooldown."**
  This is the important gap, found in `src/providers/mod.rs:90-99` and
  `src/routing/router.rs:272-343`:
  - `ProviderError::Upstream{..}` and `ProviderError::Timeout` are marked
    `is_transient()` — the doc comment literally says "worth failing over to
    a different upstream, but **not worth tripping that upstream's cooldown**
    the way a rate limit does" ([`src/providers/mod.rs:90-99`](src/providers/mod.rs#L90-L99)).
  - In `Router::dispatch`'s match arms ([`src/routing/router.rs:326-340`](src/routing/router.rs#L326-L340)),
    only `is_rate_limited()` errors call `self.health.trip(...)`. The
    catch-all arm for everything else (including `Upstream`) just records the
    attempt and moves to the next candidate for *that* request — it does
    **not** call `HealthRegistry::trip`.
  - Net effect: if Gemini's internal protocol silently drifts and every
    request starts returning an unparseable 200, `Router::dispatch` fails
    over per-request (good — it won't corrupt a response, and it won't block
    the Anthropic/Bedrock/OpenAI upstreams), but it will **re-select and
    re-try the broken Gemini upstream again on the very next incoming
    request**, indefinitely, since nothing ever cools it down. There's no
    existing signal that distinguishes "this upstream is transiently slow"
    from "this upstream's response shape fundamentally changed and every
    future request will fail the same way" — both currently produce the same
    `ProviderError::Upstream` and the same no-cooldown behavior.
  - This is genuinely **new territory**, not something to copy from Bedrock:
    Bedrock's parse failures are presumably rare/genuinely transient (a
    stable, versioned AWS API), so leaving them off cooldown is a reasonable
    choice there. For an admittedly-undocumented, actively-drifting internal
    Google protocol, "unparseable response" is a much stronger a-priori
    signal of a systemic break, not a blip — the current three-provider
    codebase has no precedent for treating it that way.

- No existing metric or log line distinguishes "unexpected response shape"
  from an ordinary upstream error — confirmed by grep across
  `src/providers/*.rs` for `"Failed to parse"`/`"unexpected"`/`"malformed"`:
  only Bedrock's two generic parse-failure messages exist, and they're
  indistinguishable in the metrics pipeline from a normal 5xx. This matches
  and confirms the requirements doc's own Observability Requirement #2
  ("Add explicit logging/metric on 'unexpected response shape from Gemini
  upstream' ... distinct from ordinary `ProviderError::Upstream`") — that
  requirement is correctly identifying a real, currently-nonexistent
  capability, not restating something already there.

## 4. ToS / legal risk — fact-finding only

This is deliberately fact-finding, not legal advice, per the task framing.

- **Real, non-hypothetical enforcement has already happened at scale against
  this exact product family.** [`google-gemini/gemini-cli` Discussion #20632,
  "Addressing Antigravity Bans & Reinstating Access"](https://github.com/google-gemini/gemini-cli/discussions/20632)
  (also covered on [Hacker News](https://news.ycombinator.com/item?id=47195371)) documents Google
  rolling out bans explicitly for **"use of 3rd party tools or proxies to
  access Antigravity resources and quotas."** Because ban enforcement happens
  at a shared backend/abuse-prevention layer, an Antigravity ban also took
  out affected users' Gemini CLI and Gemini Code Assist access — i.e. the
  blast radius of an enforcement action is broader than just "the proxy stops
  working." Google later ran a system-wide automated unban for accounts
  flagged by this specific wave, acknowledging the disruption was broader
  than intended — but that's a post-hoc correction of a mass action, not a
  guarantee against future, better-targeted enforcement.
- Corroborating individual reports of the same enforcement category:
  - [`badlogic/pi-mono#3999`](https://github.com/badlogic/pi-mono/issues/3999) — a specific 403 from the Cloud Code Assist
    API: *"This service has been disabled in this account for violation of
    Terms of Service. Please submit an appeal to continue using this
    product."* — reported against a third-party provider integration
    (`google-gemini-cli` provider in `pi-mono`), i.e. a client conceptually
    similar in shape to the one this project is building.
  - Multiple parallel Google AI Developers Forum threads describe the same
    pattern: "[Urgent] Mass 403 ToS Bans on Gemini API/Antigravity for Open
    Source CLI Users," "Unintentional ToS Violation / Silent Ban," "Banned
    from using Antigravity / gemini-cli for over 4 months."
  - Search results characterize the operative ToS language as prohibiting
    "directly accessing the services powering Gemini CLI (e.g. the Gemini
    Code Assist service) using third-party software, tools, or services,"
    and separately prohibiting "harvest[ing] or piggyback[ing] on Gemini
    CLI's OAuth authentication to access backend services." (These
    characterizations come from search-result summaries, not from this agent
    directly opening and quoting Google's ToS document text — flagged here as
    UNVERIFIED phrasing pending someone reading the actual ToS page; the
    *fact that enforcement actions occurred* is corroborated by multiple
    independent primary sources above and is VERIFIED.)
- No evidence found (in this search pass) of Google pursuing cease-and-desist
  letters or legal action against reverse-engineering projects for this
  specific product; the enforcement mechanism observed everywhere in the
  results is **automated account/API suspension**, not litigation. That
  matches the pattern Tyler's `requirements.md` already anticipated
  ("Feasibility Risks": *"ToS risk remains nonzero even with official
  OAuth"*) — the evidence here upgrades that from a hypothetical to a
  documented, actively-occurring enforcement pattern, specifically triggered
  by "third-party tool" usage patterns (unusual request headers/absence of
  IDE fingerprints, unusual traffic volume/timing, non-Antigravity TLS
  fingerprints) — i.e. exactly the surface this project's own header/TLS
  fidelity choices (§1) determine detectability against.

## 5. What to explicitly design against

Given §1–4, concrete recommendations for Phase 3 (plan) to weigh, ranked by
how directly they follow from evidence above rather than speculation:

1. **A protocol-drift circuit breaker, separate from `HealthRegistry`'s
   rate-limit cooldown.** §3 shows today's `is_transient()` classification
   deliberately keeps `Upstream` errors off cooldown, which is wrong for a
   drifting-by-nature backend. Concretely: introduce a way for the Gemini
   provider (or the router) to distinguish "N consecutive parse/shape
   failures" from "N consecutive ordinary upstream errors" and trip
   `HealthRegistry` (or a new, longer-duration cooldown state) on the former
   — this can reuse `HealthRegistry::trip`'s existing `override_duration`
   parameter rather than needing a new registry type. A single malformed
   response shouldn't cool down the upstream (could be one bad chunk); a
   run of them from every request should, since retrying a systemically
   broken upstream at full request rate produces zero successes and wastes
   the fallback flow's time budget on every request.
2. **Emit the schema-drift signal the requirements doc already calls for**
   (Observability Requirement #2) as a genuinely distinct metric/log
   line/counter — confirmed in §3 that nothing like this exists today, so
   it's new code, not "extend an existing hook."
3. **Design the `Exec` auth-failure path to distinguish "no token source at
   all" (headless/keyring-absent, per §2's `antigravity-cli#479`/`#57`) from
   "token present but rejected/expired."** The former needs a message
   pointing at whatever interactive step Phase 2's CLI inspection finds
   (mirroring Bedrock's `has_tty` branch in
   [`do_sso_login`](src/providers/bedrock.rs#L572-L605) verbatim, including
   the has-TTY-vs-not fork) rather than a generic auth error — this machine's
   own `ssh-bastion-client` usage pattern means "no TTY, no keyring session"
   is a realistic runtime state, not an edge case.
4. **Log OAuth refresh failures distinctly from request-level 401/403s**
   (already called out in Observability Requirements) — and additionally,
   per §2's race-condition findings, treat a sudden auth failure *immediately
   after* a manual `antigravity-cli login`/IDE-open event as a plausible
   refresh-token race rather than assuming the token is simply dead; a retry
   after a short delay before declaring the upstream down may avoid an
   unnecessary full re-auth prompt. (This is a judgment call for Phase 3, not
   a hard requirement — the evidence supports the race being *possible* here,
   not that it *will* occur at Tyler's single-operator request volume.)
5. **Do not over-invest in header/TLS-fidelity mimicry as a hard requirement
   up front.** §1 shows this is a real, active arms race (JA3 spoofing
   projects exist), but §4 shows enforcement so far has targeted proxies
   serving many users' traffic at volume/pattern anomalies more consistent
   with resale or automation at scale, not necessarily a single personal
   `reqwest` client making normal-cadence requests. Given the Large-appetite,
   single-operator, personal-use framing already accepted in
   `requirements.md`, treat exact IDE-header replication (`User-Agent`,
   `Client-Metadata`, `X-Goog-Api-Client` per §1's
   `antigravity-add-model` findings) as a Phase 2 capture-and-copy task (get
   it right, since wrong headers cause outright rejection per the rabbit
   hole already flagged) rather than escalating into TLS-fingerprint evasion
   engineering, which is a materially larger and more adversarial undertaking
   than this project's stated scope.
6. **Document the manual re-auth procedure explicitly** (e.g. a `RELEASE.md`-
   or `README`-adjacent runbook note: "if the Gemini upstream logs repeated
   auth failures, run `antigravity-cli login` — or the correct verb Phase 2
   determines — then verify with `<command>`"). Given §2/§4 evidence that
   both organic expiry (headless persistence bugs) and involuntary
   revocation (ToS enforcement) are documented real outcomes for this
   specific product, "a human needs to re-auth" is a *when*, not an *if*, for
   personal long-running use — this is exactly the gap Bedrock's
   `do_sso_login` closes for AWS SSO expiry, so the design target is parity
   with that, not a new pattern.
7. **Accept, don't engineer around, the ToS risk** — per `requirements.md`'s
   own framing, Tyler has already accepted this for personal use. The
   concrete design implication is only: keep the "rollback is deleting the
   config entry" Risk Control as-is (already true per `requirements.md`'s
   Risk Control section), since a suspension is external and unpredictable —
   there's no engineering mitigation available for the suspension itself,
   only for detecting and failing closed when it happens (items 1–4 above).
