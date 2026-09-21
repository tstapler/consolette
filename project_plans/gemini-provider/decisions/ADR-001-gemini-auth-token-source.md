# ADR-001: Gemini auth token source — Exec wrapper script, not a `keyring` core dependency

**Status**: Accepted
**Date**: 2026-09-04
**Deciders**: Tyler Stapler (via Phase 3 planning)

## Context

Two Phase 2 research files disagree on how `GeminiProvider` should obtain the Antigravity CLI
OAuth token:

- `project_plans/gemini-provider/research/architecture.md` §4 recommends keeping
  `AuthMethod::Exec` exactly as it exists today (`src/config/schema.rs:91-99`,
  `src/auth/exec.rs`) and pointing `command` at a **new small wrapper script**, outside `src/`,
  that reads the plain JSON token file at `~/.gemini/antigravity-cli/antigravity-oauth-token` and
  emits `{"headers": {...}}` on stdout per the ADR-007 §2 stdin/stdout contract
  (`project_plans/consolette/decisions/ADR-007-plugin-format-and-credential-helper.md`). Zero new
  Rust dependencies, zero new core auth variant.
- `project_plans/gemini-provider/research/build-vs-buy.md` recommends adding the `keyring` crate
  (v4.2.0, MIT/Apache-2.0, actively maintained) and a new `SecretRef::Keyring` core variant,
  because `antigravity-cli`'s own docs claim keyring-first token storage, and a
  `KeyringTokenStore` pattern already exists in Tyler's `taste-playlist` project for reuse.

`research/stack.md` ran a live `secret-tool search --all` on this machine across every plausible
service-name guess (antigravity, google, oauth, gemini, cloudcode) and got **zero results**. The
only concrete, verified artifact on this machine is the plain JSON file at
`~/.gemini/antigravity-cli/antigravity-oauth-token` (mode 600), which does exist (and was found
already expired — see ADR-002-adjacent auth-failure handling, out of this ADR's scope).

## Decision

**Use the wrapper-script + existing `AuthMethod::Exec` approach for v1.** Do not add the
`keyring` crate or a `SecretRef::Keyring` variant now.

Concretely:
- A new script, `references/bin/antigravity-token-auth.py` (Python 3, stdlib only — no new
  dependency of any kind, Rust or otherwise), reads
  `~/.gemini/antigravity-cli/antigravity-oauth-token`, checks the `expiry` field, and either:
  - emits `{"headers": {"Authorization": "Bearer <access_token>", "X-Goog-Api-Client": "...",
    "Client-Metadata": "..."}}` on stdout and exits 0, or
  - exits non-zero with a stderr message (never stdout — ADR-007 §6 forbids helper stdout/stderr
    content leaking into logs, so the *fact* of failure is what `AuthError::Exec` surfaces, not
    the message text) when the token is missing, unparseable, or expired.
- The Gemini upstream's `[upstreams.auth]` table in
  `references/conf.d/00-providers.toml` is `type = "exec"`, `command =
  "references/bin/antigravity-token-auth.py"` — structurally identical to the existing
  `tests/fixtures/toml_parity/exec_auth_upstream.toml` fixture. No `src/config/schema.rs` change
  beyond the existing `AuthMethod::Exec` variant (already present, lines 91-99).
- No change to `src/auth/exec.rs`, `src/auth/mod.rs`, or `SecretRef` (`src/config/schema.rs:30-34`).

## Rationale

1. **Verified need beats documented-but-unobserved need.** `stack.md`'s own live probe on this
   exact machine found no keyring entry. The requirements doc's Constraints section and the
   `ponytail` lean-engineering principle both point the same direction: build the minimum that
   satisfies the concrete, observed artifact (the file), not a documented fallback path nobody has
   hit yet.
2. **Zero new attack surface / zero new dependency.** `keyring` v4 is well-maintained, but adding
   it plus a new `SecretRef::Keyring` core variant means new code paths (including — per
   `build-vs-buy.md` — a `spawn_blocking` wrapper, since `keyring`'s API is synchronous and a
   Secret Service call blocks on a D-Bus round-trip) that would sit permanently unexercised until
   a real keyring-backed token appears on some machine. That is speculative generality the
   requirements' "must not weaken or complicate the existing three providers' auth/config
   contracts" constraint argues against by extension: a new core `SecretRef` variant is exactly
   the kind of core-contract expansion `AuthMethod::Exec`'s own doc comment
   (`src/config/schema.rs:72-75`) already rejects for a *different* one-off case (the employer's
   internal identity system), on the same reasoning.
3. **`AuthMethod::Exec` already generalizes this correctly.** ADR-007 §4 exists precisely so a
   plugin/one-off integration ships as a credential-helper script, not a new core auth mode. A
   Gemini-specific wrapper script is the same shape of solution the codebase already chose for the
   employer-internal-identity case.
4. **Headless/no-keyring is the realistic runtime state on this machine anyway.**
   `research/pitfalls.md` documents `antigravity-cli` issues #479/#57/#632: the CLI's own
   file-based/keyring token source frequently fails in headless SSH sessions — and this machine
   runs the `ssh-bastion-client` Ansible role. A keyring-dependent auth path would be *less*
   reliable here than the file-based path, not more.

## Consequences

- If Google/`antigravity-cli` ever moves to keyring-only storage (removing the file), this
  decision must be revisited — the wrapper script would need a keyring read added, which is the
  point at which the `keyring` crate (or a shelled-out `secret-tool`/`security` call from the
  wrapper script itself, keeping it out of Rust core entirely) becomes justified. See the "Build
  keyring fallback only if observed" line in plan.md's Unresolved Questions — this is a documented
  follow-up, not a blocked requirement.
- Token refresh is explicitly **not** attempted by the wrapper script (see plan.md Story 1.2.2):
  refreshing would require extracting `agy`'s OAuth `client_id` from its binary, which crosses
  back into the harvested-credential territory the requirements' Constraints section rules out.
  The wrapper fails closed with an actionable message when the token is expired.
- **Refresh-token race condition — deliberate non-decision, not an unconsidered gap.**
  `research/pitfalls.md` §2 and `requirements.md`'s carried-forward "token refresh mechanism is
  unknown" open question both flag a potential race between something rewriting
  `~/.gemini/antigravity-cli/antigravity-oauth-token` and a reader observing it mid-write. This
  decision's "consolette never attempts its own refresh" choice narrows that exposure
  substantially: the only remaining race is between a **manual** `antigravity-cli login` (or the
  Antigravity IDE) rewriting the file and the wrapper script's own read of it happening to land
  mid-write — there is no consolette-internal refresh loop that could race with itself or with
  in-flight requests. This residual case is treated as a rare, self-healing event for v1: the
  wrapper script's read either succeeds (gets the old or new token, both plausibly valid) or fails
  to parse (a torn write), which surfaces as an ordinary `ProviderError::Auth`/exec-failure that
  the *next* request naturally retries against a by-then-complete file. No explicit read-retry or
  file-locking logic is added for this — it would be speculative hardening against an event that
  self-heals on the very next request, at Tyler's single-operator usage scale.

## Rejected alternative

Add `keyring` v4 + `SecretRef::Keyring` core variant (`build-vs-buy.md`'s recommendation) —
rejected per Rationale #1-#3 above: unverified need on this machine, new core-contract surface,
extra async-wrapping complexity, for a storage backend nothing here currently uses.
