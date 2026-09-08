# UX Research: gemini-provider

Research Agent 5 (UX), SDD Phase 2. Scope: how Tyler (sole operator) discovers
and diagnoses Gemini/Antigravity upstream state through config files, the
dashboard, and logs — not an end-user product UX review.

## 1. Comparable UX patterns

**What the dashboard does today** (`src/dashboard.rs`, `src/entrypoint/observability.rs`):
- `GET /dashboard` renders a static HTML/JS shell that polls `GET /metrics`
  every 30s and `GET /errors/summary` every 60s (`src/dashboard.rs:340,563,594-595`).
- Per-upstream cooldown state is a single boolean-ish signal: a colored dot
  (`status-active` green / `status-cooldown` amber, `src/dashboard.rs:41-42`)
  next to the upstream name, with remaining seconds appended in parens when
  cooling down (`src/dashboard.rs:360-366`, driven by `data.cooldowns[name]`
  from `HealthRegistry::remaining_secs`, `src/routing/health.rs:71-83`).
- There is **no distinction in that indicator between causes**. A Bedrock
  throttle, an Anthropic 5xx, and (today, hypothetically) an expired
  Antigravity token would all render identically: amber dot, seconds
  counting down. The only place a viewer could tell them apart is the
  separate "Recent Errors" / "Unique Error Types" tables further down the
  page, which show `error_type` and `message` per event but are not
  cross-referenced with the status dot above.
- `ProviderError` (`src/providers/mod.rs:33-55`) already has the right
  *shape* to support finer-grained surfacing — `Auth(String)` is a distinct
  variant from `RateLimited`/`Upstream{status,body}`/`Timeout` — but nothing
  in the dashboard today renders `is_auth()` differently from
  `is_transient()`. That's the gap this feature should close, not paper over.
- The dashboard explicitly avoids hardcoding upstream names (enforced by
  `dashboard.rs`'s own test `no_upstream_is_hardcoded_by_name`,
  `src/dashboard.rs:612-628`) — any Gemini-specific UI must follow the same
  rule: render from `/metrics` data dynamically, not add a `gemini-status`
  DOM id.

**External comparables** (dev-proxy/router operator UX, from general
knowledge of the category — LiteLLM, OpenRouter status pages, Caddy/Traefik
dashboards, `gcloud`/`aws` CLI auth-error conventions):
- The consistent pattern across CLI-and-dashboard dev tools is a **three-way
  split** in how "not working" is communicated:
  1. *Self-healing / transient* (rate limit, 5xx, timeout) → shown as a
     passive status (amber, a countdown, "retrying automatically") — no
     action implied.
  2. *Needs human action, tool-specific fix* (expired/revoked credential) →
     shown as a distinct color/icon (red, not amber) with the **exact
     remediation command** inline, because the whole point is the operator
     shouldn't have to go read logs to figure out what to run. `gcloud`'s own
     `ERROR: (gcloud.auth) You do not currently have an active account`
     always names `gcloud auth login` in the same breath; that's the bar.
  3. *Tool/protocol broke, not credential-related* (parse failure, schema
     drift) → shown as a hard failure distinct from both of the above,
     because retrying or re-authenticating won't fix it — only a code change
     will. Conflating this with (1) wastes Tyler's time retrying; conflating
     it with (2) sends him to `antigravity-cli login` for a problem that
     login can't fix.
- Consolette's current dashboard only implements bucket (1). Buckets (2) and
  (3) don't exist as visual states yet — they'd currently both render as
  "amber dot + generic error row," which is exactly the "waste time debugging
  a routing issue that's actually just an expired token" failure mode the
  research question flags.

**Recommendation**: add a third status-indicator class (e.g.
`status-auth-required`, distinct hue — red/orange, not the existing amber) to
the existing per-upstream status bar, driven by whether the *last* error
recorded for that upstream was `ProviderError::Auth`. Keep the amber dot for
`is_transient()`/`is_rate_limited()`. Add a fourth bucket for schema drift
(see §3) rather than folding it into the generic error table.

## 2. Mental model

Reading `Config::default()` (`src/config/schema.rs:314-370`) and the existing
fixtures (`tests/fixtures/toml_parity/bedrock_upstream.toml`,
`exec_auth_upstream.toml`) and README (`README.md:41-61`), Tyler's established
mental model for adding an upstream is:

```toml
[[upstreams]]
name = "<free-text-label>"
kind = "<anthropic|bedrock|openai>"
<kind-specific fields, flattened, no nesting>

[upstreams.auth]        # separate table, optional
type = "<bearer|apikey|exec>"
<auth-specific fields>
```

Two consistency points matter most for minimizing surprise on a `kind =
"gemini"` block:

1. **`kind`-specific fields are flat, not a nested sub-table.** `Bedrock`
   takes `aws_region`/`aws_profile`/`max_retries` directly under
   `[[upstreams]]` (`bedrock_upstream.toml:1-6`), and `Openai` takes
   `base_url` the same way (`schema.rs:117-119`, README example). A
   `Gemini` variant should follow suit — e.g. a `model` or
   `project_id`/`endpoint_override` field (if the internal API needs one)
   sitting flat alongside `name`/`kind`, not under `[upstreams.gemini]`.
   `#[serde(deny_unknown_fields)]` on `UpstreamKind` (`schema.rs:106`) means
   any deviation from this shape produces a hard parse error on start, which
   is itself a good, fast feedback loop for a typo — but only if the
   documented example matches the real shape.

2. **Auth is a separate `[upstreams.auth]` table, keyed by `type`, not baked
   into the upstream kind.** `exec_auth_upstream.toml` shows this cleanly:
   `kind = "anthropic"` plus a wholly separate `[upstreams.auth]` with
   `type = "exec"`. Since the requirements' preferred design is
   `AuthMethod::Exec` shelling out to `antigravity-cli` (mirroring `gcloud
   auth print-access-token`), the expected config Tyler would write by
   analogy is:

   ```toml
   [[upstreams]]
   name = "gemini"
   kind = "gemini"
   # any Gemini-specific fields here, flat

   [upstreams.auth]
   type = "exec"
   command = "antigravity-cli"
   args = ["print-access-token"]   # placeholder pending Phase-2-research subcommand name
   cache_ttl_secs = 300             # same default as any other exec upstream
   ```

   This requires zero new concepts for Tyler — same `[[upstreams]]` +
   `[upstreams.auth]` shape, same `exec` auth type he'd recognize from
   `exec_auth_upstream.toml`. The only genuinely new surprise is if
   `antigravity-cli` turns out to need keyring reads instead of a
   print-token subcommand (open question in requirements.md) — that would
   break the "just another exec auth" mental model and deserves a clearly
   different config shape (e.g. a `keychain`-flavored auth type) rather than
   quietly special-casing `AuthMethod::Exec`'s semantics for one provider.

3. Route wiring is unchanged — `[[routes.upstreams]] name = "gemini"` in a
   `fallback` or `weighted` route, exactly like the README's `my-openai-upstream`
   example. No new routing concept needed.

**Recommendation for `references/conf.d/00-providers.toml`** (this file does
not exist yet in the repo — it's a new deliverable per the requirements, not
an edit to an existing one): model it directly on the flat-field pattern
above, and put it next to equivalent Anthropic/Bedrock/OpenAI blocks so
Tyler (and future-Tyler) can visually diff the four kinds side by side in one
file, reinforcing "Gemini is just a fourth upstream kind," not a special case.

## 3. Error states

Three states need to be visually and textually distinguishable, mapped onto
the existing `ProviderError` classification (`src/providers/mod.rs:33-55`)
and `HealthRegistry` (`src/routing/health.rs`):

| State | `ProviderError` variant (new/existing) | Cooldown behavior | Dashboard surfacing | Log level/message |
|---|---|---|---|---|
| (a) Antigravity token expired/revoked | `ProviderError::Auth(String)` — already exists, reuse as-is | Should still trip `HealthRegistry` (so routing fails over), but ideally a *longer* or non-decaying cooldown, since a normal 300s cooldown implies "will self-heal soon," which is false for this case — it needs Tyler to run `antigravity-cli login` | New status class distinct from the amber "cooling down" dot (§1) — e.g. red, label "needs re-auth," and ideally the exact remediation command surfaced in a tooltip/subtitle | `ERROR`, per requirements.md's Observability Requirements: "Log (not just error) OAuth token refresh failures distinctly from request-level auth errors" — i.e. a refresh-failure log line should read differently from a per-request 401, e.g. `gemini upstream: token refresh failed via antigravity-cli — run 'antigravity-cli login' to re-authenticate` rather than a generic `auth error: 401` |
| (b) Internal API silently changed shape (parse failure) | **New**, per requirements.md scope: "fail closed... clear `ProviderError`... not a silent misparse." Needs a distinguishable variant, e.g. `ProviderError::Upstream{status, body}` is *not* enough because that implies an HTTP-level failure, not "200 OK but the JSON didn't match what we expected." Recommend a new variant such as `ProviderError::ResponseShapeMismatch(String)` or reuse `Validation` if it's semantically "we sent/received something we can't parse" — but `Validation` today means "client sent bad input," which is the wrong connotation for schema drift on a response we don't control. A dedicated variant keeps the drift signal unambiguous, matching the requirement: "Add explicit logging/metric on 'unexpected response shape from Gemini upstream' (schema-drift signal) distinct from ordinary `ProviderError::Upstream`." | Should trip `HealthRegistry` (protocol is broken until someone patches the provider code — no amount of retrying fixes it) | A *fourth*, most-alarming status class — this is the "your code needs to change" bucket, not "wait" or "re-auth." Should probably not auto-clear the way the amber cooldown does, since retrying every 300s against a permanently-changed API just spams logs; consider marking this state in the dashboard until service restart or explicit code fix | `ERROR`/`WARN` with the raw unexpected shape (truncated) logged once with full detail, then rate-limited on repeat, so Tyler can `grep` and paste the actual payload into a bug report/PR — this is the earliest signal that Google changed the protocol, called out explicitly as "the single biggest unknown" in requirements.md |
| (c) Internal endpoint down/rate-limiting like any other provider | `ProviderError::RateLimited` / `RateLimitedWithRetry` / `Timeout` / `Upstream{status,body}` — all already exist, no new variant needed | Standard `HealthRegistry` cooldown, same as Anthropic/Bedrock/OpenAI today | Existing amber "cooling down (Ns)" dot — no change needed, this is the case the dashboard already handles well | `WARN`/`INFO`, same volume/level as existing provider transient-failure logging — this is explicitly the "self-heals" bucket and should not be louder than what Anthropic/Bedrock/OpenAI already produce |

The throughline: **(a) and (b) both need a status that reads as "action
required, not self-healing," but they need *different* actions** — (a) says
"run this CLI command," (b) says "file/fix a bug." Collapsing them into one
"errored" bucket would still leave Tyler guessing which one he's looking at,
which is the exact failure mode requirements.md is trying to avoid ("so
Tyler doesn't waste time debugging a routing issue that's actually just an
expired token" — and the converse, running `antigravity-cli login` futilely
against a protocol-drift bug).

## 4. Accessibility

`src/dashboard.rs` is a single server-rendered HTML page with inline
`<style>`/`<script>` (no separate frontend framework/build). Relevant
existing patterns worth staying consistent with, since this is Tyler's own
tool (no formal WCAG audit needed, per scope):

- Color is not used as the *only* signal for status today — the cooldown dot
  is always paired with a text label (`Anthropic`, `Anthropic (42s)`,
  `src/dashboard.rs:360-366`) and error rows carry a text `error_type` badge
  alongside color (`src/dashboard.rs:85-89`). Any new "needs re-auth" /
  "schema drift" status should keep this pairing — color plus a short label
  like "needs re-auth" or "protocol drift," not a bare colored dot, so it's
  legible even if Tyler's glancing at it on a dim screen or the color choice
  clashes with a terminal/browser dark-mode override.
- The page is dark-themed only (`background: #0a0a0a`, no light-mode media
  query) — a new status color should be chosen against that fixed dark
  palette (existing palette: green `#10b981` active, amber `#f59e0b`
  cooldown, red-ish `#fca5a5`/`#7c2d12` used for the generic error badge).
  Reusing the existing error-badge red for "needs re-auth" would be a
  reasonable, zero-new-color choice, keeping "schema drift" as the one truly
  new visual treatment (e.g. a distinct purple/violet, unused elsewhere on
  the page, to avoid overloading red for two semantically different alarms).
- No ARIA roles/labels are present anywhere in the current HTML — it's
  entirely visual/mouse-driven (`onclick` handlers, no keyboard nav beyond
  `Escape` closing the modal, `src/dashboard.rs:524`). Not worth introducing
  ARIA scaffolding just for this feature; match the existing bar rather than
  raising the accessibility bar unilaterally for one upstream's status.

## 5. Job-to-be-done

**Functional job**: route Gemini traffic through the same fallback/weighted
routing, cost tracking, and dashboard Tyler already has for
Anthropic/Bedrock/OpenAI, so a Gemini call is a router decision (config +
route weight), not an IDE choice. This directly serves the stated baseline
gap: "no shared routing, no fallback into/out of the Claude/Bedrock
upstreams, no unified cost/metrics tracking" (requirements.md, Baseline).

**Emotional job**: consolidated control and confidence — one dashboard, one
mental model, one place to look when something's wrong, across every model
subscription Tyler pays for, rather than trusting each vendor's own IDE/CLI
UX (and each vendor's own opaque failure modes) separately. It's the same
job Tyler is already solving for Anthropic/Bedrock with this router; Gemini
via Antigravity is "don't make me learn/watch a fifth tool's health
signals" as much as it is "make Gemini available."

**Does this change prioritization?** Yes, in a way requirements.md's own
Risk Control section already gestures at but the Success Metrics don't
fully capture. Given:
- The wire protocol is undocumented and can drift *silently* (requirements.md
  Feasibility Risks, Rabbit Holes) — a broken translation layer that fails
  *quietly* (a misparse that produces a garbled-but-valid-looking response)
  is worse for this job-to-be-done than a broken layer that fails *loudly* on
  the dashboard. The emotional job is "I trust this system to tell me when
  something's wrong" — a silent misparse directly betrays that trust in a
  way a routing feature gap does not.
- Therefore: **the schema-drift-detection and auth-vs-drift-vs-transient
  dashboard distinction (§3 above) is not a nice-to-have polish pass — it's
  load-bearing for the entire job-to-be-done**, more so than, say, full
  tool-call round-tripping being byte-perfect on day one. The Staged Rollout
  in Risk Control (non-streaming text → streaming → tool calls) should be
  paired with an equivalent staging on observability: land the three-way
  error classification *alongside or before* streaming/tool-call work, not
  as a follow-up, since every later milestone depends on Tyler being able to
  tell "it's broken because Google changed something" from "it's broken
  because I need to log in again" from day one of non-streaming text landing.
- Concretely, this argues for pulling the dashboard/error-classification
  work for Gemini into the *first* internal milestone (non-streaming text),
  not treating it as generic "Observability Requirements" polish tacked on
  at the end — the requirements doc already flags this correctly in its
  Observability Requirements section, but the Success Metrics list doesn't
  mention it, so Phase 3 (plan) should make sure the task breakdown doesn't
  let it slip to "later."
