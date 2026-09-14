# Requirements: daemon-install

**Date**: 2026-09-12
**Type**: feature addition
**Complexity**: 3 — system design

## Problem Statement
`consolette install` (`src/service.rs`) only knows how to write and load a macOS
`launchd` LaunchAgent. On Linux — Tyler's primary daily driver (Manjaro), and
also used via WSL2 — there is no equivalent: `consolette run` must be started
and kept alive by hand (observed today as a manually-attached tmux session,
`staplersquad_consolette`, with no supervision, no restart-on-crash, and no
start-on-login). There is also a published `tstapler/homebrew-tap/consolette`
formula with no `service do` block, so `brew services start consolette` —
the cross-platform daemon UX Homebrew users expect — doesn't work either.

## Baseline
Today, running consolette in the background means manually launching
`consolette run` in a terminal multiplexer (tmux) and remembering to restart
it after a crash, reboot, or logout. macOS users get real supervision via
`consolette install`; Linux and Homebrew-Services users get nothing.

## Users / Consumers
Tyler, across his own machines: Manjaro/Ubuntu Linux (primary), macOS (work),
occasional WSL2. Secondarily, anyone else who installs consolette via the
`tstapler/homebrew-tap` formula on either macOS or Linux.

## Success Metrics
- On Linux, `consolette install` writes and enables a `systemd --user` unit
  that starts consolette on login and restarts it on crash — replacing the
  manual tmux session as the way consolette runs in the background.
- `brew services start consolette` works on both macOS and Linux via a
  `service do` block in the tap formula, without requiring `consolette
  install` to have been run first.
- Re-running `consolette install` on either platform is idempotent (matches
  the existing macOS behavior: safe to re-run any time the binary or
  environment changes).

## Appetite
Medium (1–2 weeks)
*(Scope must fit the appetite. If it doesn't fit, cut scope — do not move the deadline.)*

## Constraints
- Must not change the existing macOS launchd behavior or its plist contract
  (ADR-005): label `com.consolette`, port 47000, `KeepAlive=true`,
  `ProcessType=Background`, logs at `/tmp/consolette.*`. This project adds a
  Linux path alongside it, not a replacement.
- The Homebrew formula (`consolette.rb` in `tstapler/homebrew-tap`) is
  auto-generated and auto-published by `cargo-dist` on every release
  (`dist-workspace.toml`: `installers = ["shell", "homebrew"]`,
  `publish-jobs = ["homebrew"]`). Any `service do` block must survive that
  regeneration — either cargo-dist supports emitting one, or the approach
  needs to fit within its templating/patching model. This is an open
  feasibility question for Phase 2 research, not assumed solvable.
- Linux daemon mechanism is `systemd --user` (matches the per-user, not
  system-wide, nature of the existing macOS LaunchAgent — consolette runs as
  Tyler's user, not as root).
- New Linux unit auto-starts by default (`WantedBy=default.target` +
  `systemctl --user enable`), unlike the macOS plist's deliberate
  `RunAtLoad=false` (that default existed only to avoid two agents (old
  `com.claude-proxy-rs` + new `com.consolette`) fighting over port 47000
  during the ADR-005 migration cutover — a concern that no longer applies).
- If `systemctl --user` is unreachable (e.g. WSL2 without systemd enabled),
  `consolette install` must fail with an actionable error message rather
  than silently writing a unit that never runs.

## Non-functional Requirements
- **Performance SLO**: not specified — install is a one-shot CLI command, not
  a hot path.
- **Scalability**: not applicable.
- **Security classification**: internal (personal tooling; unit files may
  carry secrets — see Constraints — via environment variables, same as the
  existing plist).
- **Data residency**: no special requirements.

## Scope
### In Scope
- Generalize `src/service.rs` / `consolette install` to detect the platform
  and take a systemd path on Linux, keeping the existing launchd path on
  macOS unchanged.
- New systemd `--user` unit template (equivalent contract to the launchd
  plist: binary path + `run` arg, env vars forwarded from `service.rs`'s
  existing allowlist, restart-on-failure, log destination).
- `consolette install [--start]` on Linux: writes the unit, runs
  `systemctl --user daemon-reload`, enables it (`systemctl --user enable`),
  and (with `--start`) starts it immediately — mirroring the existing
  `--start` flag's semantics on macOS.
- WSL2 / no-systemd detection with a clear error.
- A `service do` block added to the `tstapler/homebrew-tap/consolette.rb`
  formula (or to whatever cargo-dist mechanism produces it) so
  `brew services start|stop|restart consolette` works on both platforms.
- Update `README.md`'s CLI reference and RELEASE.md if the release/publish
  flow needs a documented manual step for the formula's service block.

### Out of Scope
- Any change to macOS launchd behavior, port, or env-var contract.
- System-wide (root/system unit) installation on Linux — user-level only.
- A Linux init-system other than systemd (no OpenRC/runit support).
- Automatic migration/uninstall tooling beyond what already exists
  (`consolette install` overwrites in place, same as today).
- Changes to consolette's actual proxy/runtime behavior — this is
  install/lifecycle tooling only.

## Rabbit Holes
- **cargo-dist + Homebrew service blocks**: cargo-dist auto-generates and
  auto-publishes `consolette.rb` on every release. If it has no native
  support for a `service do` block, hand-patching the generated formula will
  either get clobbered on the next release or require an `allow-dirty`-style
  carve-out (there's already one precedent for this in `dist-workspace.toml`,
  for the tap-repo GitHub App token step) — resolve this in Phase 2 research
  before committing to an approach in the plan.
- **Secrets in the systemd unit**: the macOS plist inlines env vars
  (`AWS_PROFILE`, `CLAUDE_CODE_OAUTH_TOKEN`, etc.) read from the installing
  user's shell environment at `consolette install` time. A systemd unit file
  is world-readable by default under `~/.config/systemd/user/` unless
  permissions are tightened — decide whether to keep parity (accept the same
  exposure the plist already has) or improve it (e.g. `EnvironmentFile=` with
  a `0600` file) without silently changing security posture the ADR didn't
  ask for.
- **WSL2 detection accuracy**: distinguishing "systemd not running in this
  WSL2 instance" from "systemd genuinely broken" from "not WSL2 at all, just
  a container without systemd" — a false-positive "enable systemd in
  /etc/wsl.conf" message on a real error would be confusing.

## Alternatives Considered
- **Homebrew Services only, no `consolette install` systemd path** — rejected
  by the user: both are wanted (see Scope decision), since not everyone
  installs via the tap.
- **System-wide systemd unit (`/etc/systemd/system/`)** — rejected: the
  macOS precedent is user-level (LaunchAgent, not LaunchDaemon), and
  consolette has no need to run before user login or as a different user.

## Feasibility Risks
- cargo-dist 0.32.0's Homebrew formula generation may not support a
  `service do` block at all, forcing either a version bump, a post-generation
  patch step in the tap repo's own CI, or dropping the Homebrew Services
  piece to a documented manual step.
- `systemctl --user` behavior varies across the target machines (Manjaro,
  Ubuntu, WSL2) — particularly around lingering (`loginctl enable-linger`)
  for headless/no-active-session start, which isn't covered by the "auto-start
  on login" success metric as stated but may be expected in practice on a
  server-like box.

## Observability Requirements
Standard request logging sufficient (`consolette run`'s existing stdout/stderr
logging, redirected to the same kind of log files the plist already uses —
`/tmp/consolette.*` equivalent on Linux via `StandardOutput=append:...` /
`StandardError=append:...`, or `journalctl --user -u consolette` as the
systemd-native alternative — decide in Phase 3 planning).

## Risk Control
Not needed — low risk. `consolette install` already overwrites-in-place and
is documented as safe to re-run; the new Linux path follows the same
contract. No feature flag or staged rollout needed since this only affects
users who explicitly run `consolette install` or `brew services start`.

## Open Questions
- Does cargo-dist 0.32.0 (pinned in `dist-workspace.toml`) support emitting a
  Homebrew `service do` block natively, or does the tap formula need a
  post-generation patch? (Phase 2 research)
- Should the systemd unit use `EnvironmentFile=` pointing at a
  `~/.config/consolette/env` file (chmod 600) instead of inlining secrets
  directly in the unit, improving on the plist's current exposure without
  being asked to? (Phase 3 planning / adversarial review)
- Does `loginctl enable-linger` need to be part of `consolette install` for
  the "auto-start" success metric to hold on a machine with no active login
  session (e.g. a headless box), or is "starts on next login" sufficient
  parity with what was asked?
