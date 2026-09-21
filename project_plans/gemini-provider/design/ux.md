# UX Design: gemini-provider

SDD Phase 3 UX design artifact. Builds directly on `research/ux.md` (Phase 2)
and is grounded in what `implementation/plan.md` (Phase 3 plan, Epics 1.2,
1.4, 1.5, 1.8) actually schedules — not aspirational beyond it. Audience is
Tyler, sole operator, via config files, the dashboard (`src/dashboard.rs`),
and logs. No WCAG audit; match existing patterns (research/ux.md §4).

## Surfaces designed (4 total)

1. Config file — `references/conf.d/00-providers.toml` (non-interactive)
2. Log/error output — `grep`-able ERROR/WARN lines (non-interactive)
3. Auth-helper CLI output — `antigravity-token-auth.py` stderr/exit code (non-interactive, but worth calling out separately since it's a distinct process boundary Tyler will hit directly when debugging)
4. Dashboard per-upstream status indicator (semi-interactive) — full treatment

---

## 1. Config surface: `references/conf.d/00-providers.toml`

Per Epic 1.8/Story 1.8.1, this is a new reference file with one block per
upstream kind side by side. Gemini's block, flat fields matching the
Bedrock/OpenAI pattern (research/ux.md §2):

```toml
[[upstreams]]
name = "gemini"
kind = "gemini"
project_id = "your-gcp-project-id"

[upstreams.auth]
type = "exec"
command = "references/bin/antigravity-token-auth.py"
cache_ttl_secs = 300

[[routes]]
name = "default"
strategy = "fallback"

[[routes.upstreams]]
name = "anthropic"

[[routes.upstreams]]
name = "bedrock"

[[routes.upstreams]]
name = "gemini"
```

**Acceptance criteria:**
- The Gemini block sits directly alongside the Anthropic/Bedrock/OpenAI blocks in the same file, so Tyler can diff all four kinds by eye without switching files.
- `kind = "gemini"` fields (`project_id`) are flat under `[[upstreams]]`, never a nested `[upstreams.gemini]` sub-table — consistent with `bedrock_upstream.toml`/`schema.rs:117-119`'s existing convention, so a typo produces the same `deny_unknown_fields` parse error Tyler already knows how to read, not a new failure shape.
- Auth is the ordinary `[upstreams.auth] type = "exec"` table pointing at `references/bin/antigravity-token-auth.py` — no new auth concept, no `keychain`-flavored special case (confirmed unnecessary now that Phase 2 resolved token acquisition to direct file reads via a helper script, not a keyring API).
- The example defaults to `strategy = "fallback"` with Gemini listed *last*, per plan.md Story 1.8.1 — reinforces "Gemini is the risky one, land behind the stable providers" as the copy-paste default, not something Tyler has to remember to configure defensively himself.
- The file parses cleanly against `Config` via the existing `tests/fixtures/toml_parity/` pattern (Task 1.8.1b) — this is the file's own acceptance test, and it doubles as the UX check: if it doesn't parse, the "5-minute copy-paste-edit" promise is broken.

---

## 2. Log/error surface (`grep`-able)

Three distinct, greppable line shapes corresponding to the three error
buckets from research/ux.md §3 — Tyler's actual workflow here is
`journalctl`/log-file `grep` for a keyword after noticing something's off on
the dashboard, so the log line itself must name the remediation, not just
the failure.

**Representative samples** (per plan.md Task 1.2.2b, ADR-002 observability plan):

```
# (a) auth — token expired, request-level
ERROR gemini upstream: token refresh failed via antigravity-token-auth.py msg="antigravity-cli token expired at 2026-08-20T00:00:00Z — run 'antigravity-cli login' (or reopen the Antigravity IDE) to mint a fresh token"

# (b) schema drift — first occurrence, full payload
WARN unexpected response shape from gemini upstream: missing field `candidates` body="{...truncated...}"

# (c) transient — same level/volume as existing providers, no change
WARN gemini upstream request failed, will retry another candidate: rate limited (retry_after=30s)
```

**Acceptance criteria:**
- `grep -i "token refresh failed"` or `grep "antigravity-cli login"` finds the exact remediation command in one line — Tyler never has to cross-reference a generic `401` against documentation to know what to run.
- `grep -i "unexpected response shape"` finds schema-drift events distinctly from ordinary upstream errors (`grep -i "response failed\|upstream error"` does not also match it) — the two failure classes are lexically distinguishable, not just distinguishable by reading the surrounding context.
- Schema-drift raw-payload logging fires at full detail once, then is rate-limited on repeat (per ADR-002 §Observability) — a broken endpoint retried across many client requests produces one detailed log Tyler can paste into a bug report, not a scrolling flood.
- Auth-failure logs are **not** rate-limited (each failed request logs distinctly) — this is intentional per plan.md Task 1.2.2b, since auth failures don't retry-loop internally (each is one real client-triggered attempt, bounded by Tyler's own request volume), so there's no flood risk to guard against, and log-level auth visibility should track actual usage volume 1:1.

---

## 3. Auth-helper CLI surface (`antigravity-token-auth.py`)

Directly testable standalone per Story 1.2.1's own acceptance criteria
(`echo '{...}' | references/bin/antigravity-token-auth.py`) — Tyler will run
this by hand when debugging before suspecting consolette itself.

**Representative samples:**

```
$ echo '{"upstream":"gemini","method":"POST","url":"..."}' | references/bin/antigravity-token-auth.py
{"headers":{"Authorization":"Bearer ya29.abc123","X-Goog-Api-Client":"...","Client-Metadata":"..."}}
$ echo $?
0

$ references/bin/antigravity-token-auth.py < /dev/null   # expired token
antigravity-cli token expired at 2026-08-20T00:00:00Z — run 'antigravity-cli login' (or reopen the Antigravity IDE) to mint a fresh token
$ echo $?
1
```

**Acceptance criteria:**
- Success path: exactly one JSON line on stdout, exit 0, nothing on stderr — safe to pipe into `jq` without stripping noise.
- Failure path (missing file, unparseable file, expired token): **no stdout at all**, human-readable message on stderr, non-zero exit — `AuthMethod::Exec`'s caller (`src/auth/exec.rs`) can never mistake a failure for a valid (but empty) header set, and Tyler running the command by hand sees the actionable message immediately rather than a JSON parse error.
- The expired-token message names the exact two remediation options (`antigravity-cli login` or reopen the IDE) in the same line as the failure — mirrors the `gcloud auth login` bar set in research/ux.md §1.

---

## 4. Dashboard status surface — full design

### 4.1 Wireframe

Four-state status bar, same `<span class="status-indicator">` + text-label
pairing already used at `src/dashboard.rs:360-366`, extended per plan.md
Story 1.5.2 / Task 1.5.2a-c. Colors are the actual values plan.md commits to
(not just research's suggestion): `status-active` `#10b981` green,
`status-cooldown` `#f59e0b` amber (both existing, unchanged),
`status-auth-required` `#ef4444` red (new — plan.md's chosen red, distinct
from the `#fca5a5`/`#7c2d12` error-table badge pair), `status-schema-drift`
`#8b5cf6` violet (new).

```
┌─ Claude Proxy ─────────────────────────────────────────────────── ↺ 14:32:07 ─┐
│                                                                                 │
│  ● Anthropic          ● Bedrock (42s)      ● Gemini (needs re-auth)            │
│  (green #10b981)      (amber #f59e0b)      (red #ef4444)                      │
│                                                                                 │
└─────────────────────────────────────────────────────────────────────────────┘

  Zoomed, all four states side by side (illustrative — never all on one real
  upstream simultaneously, shown together here to compare hue/label pairing):

  ●  Anthropic                          ← status-active,        green,  no suffix
  ●  Bedrock (42s)                      ← status-cooldown,      amber,  "(Ns)" countdown
  ●  Gemini (needs re-auth)             ← status-auth-required, red,    "(needs re-auth)"
  ●  Gemini (schema drift — code fix needed)
                                         ← status-schema-drift,  violet, "(schema drift — code fix needed)"

  Empty state (fresh start, zero traffic on any upstream):
  "No upstream traffic yet"             ← existing text, src/dashboard.rs:358, unchanged
```

Label text is exactly what Task 1.5.2c specifies: base label
(`displayName(name)`, e.g. "Gemini") plus a suffix appended only for the
non-green states — `" (Ns)"` for cooldown (existing), `" (needs re-auth)"`
for auth-required (new), `" (schema drift — code fix needed)"` for
schema-drift (new). No new upstream-specific DOM id or hardcoded name
anywhere — driven entirely by `data.providers[name].last_error_kind` and
`data.cooldowns[name]`, satisfying `no_upstream_is_hardcoded_by_name`
(`src/dashboard.rs:613-628`).

### 4.1a Color contrast check (added 2026-09-04, Phase 4 UX-lens repair loop)

The top of this doc says "No WCAG audit; match existing patterns" — that was true for the two
*existing* colors (`#10b981`/`#f59e0b`, already shipped and already matched against this same dark
background). It was never true for the two *new* colors this feature introduces, and no spot check
had actually been done for them. Computed here using the standard WCAG relative-luminance formula
(`L = 0.2126R + 0.7152G + 0.0722B`, each channel linearized via `((c+0.055)/1.055)^2.4` for
`c > 0.03928`, else `c/12.92`, where `c` is the channel normalized to 0-1; contrast ratio =
`(L_lighter + 0.05) / (L_darker + 0.05)`) against the dashboard's `#0a0a0a` background:

| Color | Role | Linearized luminance (L) | Contrast vs. `#0a0a0a` (L≈0.00304) | WCAG 4.5:1 (normal text) |
|---|---|---|---|---|
| `#ef4444` | `status-auth-required` (red) | ≈0.2290 | **≈5.26:1** | Passes |
| `#8b5cf6` | `status-schema-drift` (violet) | ≈0.1980 | **≈4.67:1** | Passes, narrowly |

Both pass the 4.5:1 normal-text threshold, independently recomputed and confirmed (not just taken
on faith from the task brief that flagged this gap) — `#8b5cf6` passes with very little headroom
(≈4% above the 4.5:1 floor), so any future retheming of the dashboard background away from
`#0a0a0a` should re-run this check before assuming the violet still passes. Per UX Acceptance
Criterion 8 (§5), color is never the sole signal for either state regardless — both are always
paired with a distinct text suffix — so a marginal-but-passing contrast ratio is a "nice to have
margin," not a single point of failure for distinguishability.

### 4.2 Interaction flow

| State | What Tyler sees | What he does |
|---|---|---|
| **Active (green)** | `● Gemini`, no suffix | Nothing — this is the steady state. |
| **Cooldown (amber)** | `● Gemini (900s)`, countdown ticking down on each 30s poll | Nothing — self-healing, per research/ux.md's bucket (1). Optionally glances at the countdown to know when it'll try again. |
| **Needs re-auth (red)** | `● Gemini (needs re-auth)` | Runs `antigravity-cli login` (or reopens the Antigravity IDE) — the exact command is not on the dashboard itself (no tooltip/hover infra exists on this page today, per research/ux.md §4: "no ARIA roles... entirely visual/mouse-driven"), but is one `grep "antigravity-cli login"` away in the logs (surface 2) or one manual run of the auth-helper script (surface 3) away. Dot returns to green automatically on the next successful request through Gemini — no dashboard action needed, no manual "clear" button. |
| **Schema drift (violet)** | `● Gemini (schema drift — code fix needed)` | Recognizes retrying/re-authing won't help; `grep -i "unexpected response shape"` in logs for the captured raw payload, files a note/PR to update the translation layer. **Corrected 2026-09-04 (Phase 4 UX-lens repair loop)**: the dot *does* clear the same mechanical way the auth-required state does — `record_attempt`'s success branch (plan.md Task 1.4.4c) resets `last_error_kind` to `None` on ANY successful request, schema-drift included, not just auth. This is correct behavior, not a bug: if the drift was a one-off transient corruption, a genuine subsequent successful parse means real recovery and should clear the status; if the drift is persistent, requests keep failing to parse and `last_error_kind` never gets the chance to clear, since no `Ok(_)` ever happens while genuinely broken. So in practice the dot persists as long as the protocol mismatch keeps happening — not via a special no-self-heal mechanism, and not literally "until Tyler restarts consolette or ships a fix" (see 4.3 for the corrected framing; `DRIFT_COOLDOWN_SECS = 1800` from ADR-002 only bounds how long the *cooldown* lasts, not how long the dashboard status persists). |
| **No traffic (empty)** | `"No upstream traffic yet"`, no colored dot for any upstream (including Gemini) | Nothing — see 4.3 first-request edge case. |

### 4.3 Error and edge-case handling

**Ignoring "needs re-auth" for a long time — does it get louder?**
No — it stays quiet-but-visible, by design, not by omission:
- The dashboard dot stays exactly `status-auth-required` red across repeated glances; it does not escalate color, blink, or add a second badge. This matches the existing precedent (cooldown dot doesn't get "more amber" the longer it's cooling).
- Each individual client request that hits the expired-token path still logs a distinct `tracing::error!` line (surface 2), so log *volume* naturally reflects how often Tyler is actually trying to use Gemini while broken — but the dashboard summary itself doesn't compound into anything louder than the single red dot.
- **Known implementation gap to flag for Phase 5, not to design around silently**: `Router::dispatch`'s existing `Err(e) if e.is_validation() || e.is_auth()` arm (`src/routing/router.rs:326-329`) returns the error immediately and never calls `self.health.trip(...)` — this is unchanged by this plan (ADR-002 only touches `ResponseShapeMismatch`). Task 1.5.2c's JS gates the red/violet classes behind `cooling` (`!cooling ? 'status-active' : ...`). If `cooling_down` never becomes `true` for a pure auth failure, the dot will incorrectly render green even though `last_error_kind == "auth"` — silently defeating the whole point of this feature. **This must be resolved before Story 1.5.2 is considered done**: either (a) have `record_attempt`'s auth branch also call a short/no-op-duration `health.trip` purely to flip `cooling_down: true` for display purposes (without changing the immediate-return/no-failover behavior), or (b) change the JS condition so `status-auth-required`/`status-schema-drift` are keyed off `last_error_kind` directly, independent of `cooling`, and only the plain amber case remains gated on `cooling`. Recommend (b) — it's a one-line JS change, doesn't touch `Router::dispatch` semantics, and is strictly more correct (a `last_error_kind` of `"auth"` is definitionally "not currently active" regardless of whether a formal cooldown got tripped).
- Since Gemini's own auth error doesn't trigger the router's failover loop (it returns to the caller immediately rather than trying the next candidate), a client request routed primarily to Gemini during a token-expiry window fails outright rather than silently succeeding via Anthropic/Bedrock — this is pre-existing `is_auth()` semantics shared by all four providers, not new to Gemini, and is out of scope to change here, but it means the red dot is the *only* dashboard signal Tyler gets that "some of my traffic is currently failing outright," which raises the stakes on fixing the gap above.

**Schema drift — does it get louder?**
No, and less so than auth: the raw-payload log line is intentionally rate-limited after the first occurrence (ADR-002) specifically to avoid flooding, since a permanently-broken endpoint would otherwise re-log the same detail on every request during the 1800s cooldown window and again every 1800s after. The dashboard dot stays violet at minimum for the `DRIFT_COOLDOWN_SECS` window (no earlier request is even attempted against Gemini while its `HealthRegistry` cooldown is tripped). **Whether it clears after that window is not a timer effect** — `last_error_kind` clears the same generic way for every error kind (plan.md Task 1.4.4c): only on the next *actual successful* request. If the protocol is still drifted once the cooldown window ends, the next attempted request just fails to parse again and re-trips both the cooldown and the violet status; if it happened to be a one-off corruption, the next successful request clears it, same as auth. So in practice the violet dot persists for as long as the drift keeps recurring — not via a distinct no-self-heal mechanism, just because a genuinely broken endpoint never produces the `Ok(_)` that would clear it.

**Detection latency — the dashboard reflects "as of last attempt," not continuous health (added 2026-09-04, Phase 4 UX-lens repair loop).**
Every status the dashboard shows — green, amber, red, or violet — is derived from the *last recorded attempt* against that upstream (`last_error_kind`/`cooling_down`), not from any active health-check. Because Gemini is Fallback-last, it can go idle for long stretches with zero attempts. A token can silently expire — or the protocol can silently drift — during one of those idle windows, and nothing on the dashboard changes: an upstream with no recent attempt keeps showing whatever its last-known state was (typically green, from before it went idle), because there is no new attempt to update it. The break only surfaces once a real client request actually gets routed to Gemini again — which, if Anthropic and Bedrock are both healthy, may not happen for a long time, and if it does happen while they're *also* failing, the first sign of trouble is a failed client request, not an early dashboard warning. This is an accepted characteristic of attempt-driven status, not a bug to fix here: a long-idle Gemini row showing green means "no attempt has been made recently," not "confirmed healthy right now." (This is the same underlying gap the weekly dashboard-check habit in `requirements.md`'s Risk Control addresses operationally — checking periodically substitutes for the health signal an idle upstream can't otherwise produce.)

**First request after startup, zero prior Gemini activity — must not misleadingly show cooldown/error state with zero data.**
Already correctly handled by existing dashboard code, unchanged by this feature: `statusBar.innerHTML` only renders an entry for `name` in `Object.keys(data.providers || {})` (`src/dashboard.rs:352,357-366`) — an upstream with zero recorded attempts simply isn't a key in `data.providers` yet, so it doesn't appear in the status bar at all (not as amber, not as red, not as a fake "unknown" grey state). Before Gemini's first request, the whole bar shows the existing `"No upstream traffic yet"` placeholder if *no* upstream has traffic, or simply omits the Gemini entry if other upstreams (Anthropic/Bedrock) already have traffic — either way, zero data never renders as a false-positive error/cooldown signal. **No new code needed for this edge case** — call it out in Story 1.5.1/1.5.2's tests as an explicit non-regression assertion (`data.providers` with no `"gemini"` key ⇒ no `gemini` span rendered), since it's easy to accidentally break while adding the `last_error_kind` lookup (e.g. `(data.providers[name] || {}).last_error_kind` per Task 1.5.2c already guards this correctly with the `|| {}` fallback — confirm this survives review).

---

## 5. UX Acceptance Criteria

Testable by Tyler looking at the running dashboard/logs/config, not by unit-test assertions (those live in plan.md) — though several are the human-observable face of a plan.md acceptance criterion.

1. **Distinguishability, ≤2 seconds**: Tyler can tell "self-healing" (amber) apart from "needs my action" (red or violet) apart from "fully healthy" (green) in at most 2 seconds of glancing at the status bar — three distinct hues on a fixed dark background (`#10b981`/`#f59e0b`/`#ef4444`/`#8b5cf6`), each always paired with a distinct text suffix, no two states sharing a suffix pattern.
2. **Distinguishability, ≤5 seconds**: Tyler can tell "needs re-auth" (red, "(needs re-auth)") apart from "schema drift" (violet, "(schema drift — code fix needed)") in at most 5 seconds — different hue *and* different, non-overlapping label text (neither suffix is a substring of the other), so a glance or a `Ctrl+F`-style scan of the label disambiguates even for a colorblind-unfriendly hue pair.
3. **Named remediation, no guessing**: the "needs re-auth" state's label plus the corresponding log line together name the exact command (`antigravity-cli login`) — Tyler is never left to infer a fix from a bare error code or HTTP status.
4. **Named remediation for drift, no false hope**: the "schema drift" state's label explicitly says "code fix needed" (not "retrying" or a countdown), so Tyler never wastes time re-running a request or re-authenticating against a state that neither can fix.
5. **No dead ends — auth**: the "needs re-auth" state self-heals visually (returns to green) on the very next successful request after Tyler runs the named remediation — no manual dashboard acknowledgment/dismiss action exists or is needed.
6. **No dead ends — drift**: the "schema drift" state names a human action (grep the logs, fix the translation code, restart/redeploy) as its resolution path in this design doc and in the ADR, even though it never clears on a fixed timer the way a rate-limit cooldown does. **Corrected 2026-09-04**: it isn't a permanent, no-self-heal state either — like every other `last_error_kind`, it clears automatically on the next actually-successful request (plan.md Task 1.4.4c), so a one-off transient corruption self-heals just like auth does; a genuinely broken endpoint just never produces that successful request, so in practice it stays violet until Tyler ships a fix. Either way, "clears only on real success, not on a timer" is documented here, not a silent gap.
7. **No false positives on cold start**: on a freshly started consolette with zero requests to any upstream, the dashboard shows the existing "No upstream traffic yet" placeholder (or simply omits Gemini from the bar if siblings have traffic) — never a colored dot implying error or cooldown for an upstream with no data.
8. **Consistency — color is never the only signal**: every new status class (`status-auth-required`, `status-schema-drift`) pairs its color with a distinct text label, matching the existing `status-active`/`status-cooldown` convention (research/ux.md §4) — verified structurally by the same kind of assertion `no_upstream_is_hardcoded_by_name` makes today, extended (Task 1.5.2d) to also assert the new CSS class strings and `last_error_kind`-driven (not name-driven) JS logic are present.
9. **Consistency — no hardcoded upstream identity**: the Gemini status states are rendered by the same generic, name-agnostic code path as every other upstream — confirmed by `no_upstream_is_hardcoded_by_name` (`src/dashboard.rs:613-628`) continuing to pass unmodified, and by Task 1.5.2d's new assertions not introducing any `gemini`-specific string into `DASHBOARD_HTML`.
10. **Config surface — no new mental model**: a Tyler-level reviewer (or future Tyler) can write a working `kind = "gemini"` upstream block by visually pattern-matching the existing Anthropic/Bedrock/OpenAI blocks in `references/conf.d/00-providers.toml`, without reading `src/config/schema.rs` first — verified by the file actually parsing (Task 1.8.1b) and by it being flat-field, non-nested, matching `bedrock_upstream.toml`'s shape.
11. **Auth-helper surface — fail-closed clarity**: running `references/bin/antigravity-token-auth.py` by hand with an expired/missing token produces zero stdout and a one-line, remediation-naming stderr message — Tyler can diagnose an auth problem without involving consolette or the dashboard at all.
12. **Escalation risk resolved before ship**: the "Known implementation gap" in §4.3 (auth errors not tripping `cooling_down`, which would silently prevent the red state from ever rendering per Task 1.5.2c's literal JS) is resolved — either via a display-only cooldown trip or a `last_error_kind`-first JS condition — and covered by an integration test that induces a real auth failure and asserts the dashboard-facing `/metrics` JSON would actually classify as `status-auth-required`, not silently fall through to `status-active`.
