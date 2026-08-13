# ADR-006: ndotfiles / ansible-Managed Install (mirror the `aimee` block)

**Status**: Accepted
**Date**: 2026-07-17

## Context

FR-7 (CD-4) requires Consolette to be installed and managed by ndotfiles/ansible
the same way Tyler's other LaunchAgents are: a new ansible block that `cargo
build`s the binary, renders a launchd plist, links config via cfgcaddy, and loads
the agent — **idempotent** (a second run is a no-op; Success Metric 5). Config
files (`conf.d/*.toml`) are source-controlled in ndotfiles and cfgcaddy-linked;
`~/.config/consolette/*` does **not** need the `vendor-` prefix that `.claude/*`
requires (the prefix exists only to disambiguate files inside the single
`~/.claude/{skills,commands,agents}` directory symlinks).

Grounding: the existing **`aimee` block** (`ndotfiles/bootstrap/tasks.yml`
~lines 109-189) is the canonical pattern — `template` the plist (`register`),
`launchctl list <label>` (`register`, `failed_when: false`), unload-when-changed,
load-when (`rc != 0 or plist.changed`). `.cfgcaddy.yml` uses
`linker_dest: $HOME` and a `links:` list of individual file mappings; `launchd/`
holds `*.plist.j2` templates parameterized with `{{ brew_prefix }}` /
`{{ ansible_facts['env']['HOME'] }}`.

Consolette differs from `aimee` in one way: `aimee` is `brew install`ed from a tap;
Consolette is **built from source** in Tyler's `~/dotfiles/stapler-scripts/`
(soon `consolette`) checkout, so the block adds a `cargo build --release` step
gated on source changes for idempotency.

## Decision

Add a new `consolette`-tagged block to `ndotfiles/bootstrap/tasks.yml` mirroring
the `aimee` block, plus a cfgcaddy plist template and config links.

1. **Build step (idempotent):** `stat` the built `target/release/consolette`
   binary and compare against source mtime (or `cargo build`'s own change
   detection); run `command: cargo build --release` in the crate dir; then
   `copy`/`file` the binary to the path the plist references. Mark `changed_when`
   on actual build output so re-runs report `ok`.
2. **Plist template:** `ndotfiles/launchd/com.consolette.plist.j2`, parameterized
   by **binary path, port, and log paths** (FR-7.4), rendering the SAME canonical
   plist contract as the hand plist (ADR-005 / plan Story 6.3): label
   `com.consolette`; full env set (`CONSOLETTE_PORT`, `CLAUDE_CODE_OAUTH_TOKEN`,
   `AWS_PROFILE`, `AWS_REGION`, `HOME`, `PATH` — secrets from ansible vars/vault,
   never inline in the repo); `StandardOut/ErrorPath`; `KeepAlive=true`;
   **`RunAtLoad=false`** (matches the hand plist — the single settled value; ansible
   starts explicitly via `launchctl kickstart`); `ProcessType=Background`. Rendered to
   `~/Library/LaunchAgents/com.consolette.plist`.
3. **launchd lifecycle (verbatim aimee shape):** `template` (`register:
   consolette_plist`) → `launchctl list com.consolette` (`register`,
   `failed_when: false`, `changed_when: false`) → unload when
   `plist.changed and loaded.rc == 0` → **(after the old-agent teardown in item 5)**
   load when `loaded.rc != 0 or plist.changed` → `launchctl kickstart` to start
   (RunAtLoad=false). Darwin-guarded (`ansible_facts['os_family'] == 'Darwin'`).
4. **cfgcaddy config links:** add three `links:` entries to
   `ndotfiles/.cfgcaddy.yml` mapping
   `.config/consolette/conf.d/{00-providers,10-routing,20-ratelimit}.toml` →
   the same dest paths under `$HOME` — **no `vendor-` prefix** (these live in their
   own `~/.config/consolette/conf.d/` dir, not inside a shared `.claude` symlink).
   The conf.d files themselves live at
   `ndotfiles/.config/consolette/conf.d/*.toml` (copied from the crate's
   `references/conf.d/`, with a `PROJECT_ID` placeholder). **No secrets in the
   repo** — auth blocks reference env vars / keychain items only (NFR-2/6).
5. **Old-agent teardown — a real idempotent task, gated and ordered BEFORE the
   load in item 3** (both agents bind 47000, so the old must be stopped first):
   `launchctl list com.claude-proxy-rs` (`register: old_agent`, `failed_when: false`,
   `changed_when: false`) → `launchctl unload …/com.claude-proxy-rs.plist` **only
   when `old_agent.rc == 0`** (present), `changed_when` on the actual unload. A second
   run finds `rc != 0` and no-ops. This guarantees exactly one agent binds 47000
   through the cutover (parity with the ADR-005 `make migrate` path).

## Alternatives Considered

| Option | Rejected because |
|--------|-----------------|
| `brew install` Consolette (like `aimee`) | Consolette is a personal source project, not a tapped formula; building from the checkout keeps it version-controlled and hackable. |
| cfgcaddy-link the plist too | The plist is machine-parameterized (binary path, brew prefix); ansible `template` is the right tool. cfgcaddy handles only the static TOML config. |
| `vendor-` prefix on conf.d files | Only needed inside the shared `~/.claude/*` directory symlinks; `~/.config/consolette/conf.d/` is a dedicated dir, so plain names are fine (per CD-4). |
| `RunAtLoad=true` on the rendered agent | Rejected — settled at `false` for BOTH plists (ADR-005). Both agents bind 47000; ansible controls ordering (unload old → load → `kickstart`), so auto-start on load would risk a port conflict during cutover. |
| Always `cargo build` every run | Not idempotent; gate on source change so a second run is a no-op (Success Metric 5). |

## Consequences

- New files: `ndotfiles/launchd/com.consolette.plist.j2`,
  `ndotfiles/.config/consolette/conf.d/*.toml`; edits to
  `ndotfiles/bootstrap/tasks.yml` and `ndotfiles/.cfgcaddy.yml`.
- The install is reproducible on a fresh machine: `ansible-playbook` builds, links
  config, renders + loads the agent; re-runs are no-ops unless sources or the plist
  changed.
- Build-from-source adds a Rust toolchain prerequisite on the target machine
  (already present in Tyler's environment).
- `.cfgcaddy.yml` `ignore:` already excludes `bootstrap`/`launchd`; the new
  `.config/consolette/` tree is picked up by `links:` explicitly.
</content>
