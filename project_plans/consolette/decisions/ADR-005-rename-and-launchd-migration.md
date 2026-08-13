# ADR-005: Rename `claude-proxy-rs` → `consolette` + launchd Migration

**Status**: Accepted
**Date**: 2026-07-17

## Context

FR-6 (CD-5) fixes the rename: repo/binary `consolette`, config dir
`~/.config/consolette/conf.d/`, launchd label `com.consolette`. The second
`mcp-proxy` binary must keep working. A migration path off the old
`com.claude-proxy-rs` agent must not drop Tyler's daily Claude Code usage (Success
Metric 6). All existing features (compression, cache-aligner, verbosity, memory,
metrics, dashboard, mcp-gateway) carry over unchanged — this is a rename +
extension, not a rewrite (FR-6.4).

The current setup: crate `claude-proxy-rs` with two `[[bin]]` targets
(`claude-proxy-rs`, `mcp-proxy`); `com.claude-proxy-rs.plist` (label
`com.claude-proxy-rs`, port 47000, `RunAtLoad=false`, `KeepAlive=true`, logs at
`/tmp/claude-proxy-rs.*`); a `~/.cache/claude-proxy/mcp-filter-metrics.json` cache
path read by `/metrics`.

## Decision

1. **Crate + primary binary → `consolette`.** `Cargo.toml` `package.name =
   "consolette"`; `[[bin]] name = "consolette" path = "src/main.rs"`. Keep the
   `[[bin]] mcp-proxy` target unchanged.
2. **New `com.consolette.plist`** mirroring the current plist. There is ONE
   canonical plist contract (defined in plan Story 6.3); both the hand-authored
   `com.consolette.plist` and the ansible-rendered `com.consolette.plist.j2`
   (ADR-006) render it identically:
   - label `com.consolette`, `ProgramArguments = [<consolette binary path>]`,
     **same port 47000** (so clients need no change, NFR-1);
   - **FULL env set** the live `com.claude-proxy-rs.plist` injects, translated:
     `CONSOLETTE_PORT=47000` (the loader reads `CONSOLETTE_PORT`; the shim also
     accepts legacy `PROXY_PORT` — bare `PORT` is never read), `CLAUDE_CODE_OAUTH_TOKEN`,
     `AWS_PROFILE`, `AWS_REGION`, `HOME`, `PATH`. Former tuning vars
     (`COOLDOWN_SECONDS`, `REQUEST_TIMEOUT`, `BEDROCK_MAX_RETRIES`, `STAPLER_COMPRESS`,
     `COMPRESS_FLOOR_BYTES`, `CACHE_ALIGNER`, `VERBOSITY_LEVEL`, `MEMORY_MAX_ENTRIES`)
     move to `conf.d/*.toml` (still honored via env by the Story 1.5 shim if present);
   - **`RunAtLoad=false`** — the SINGLE settled value for BOTH plists;
   - `KeepAlive=true`, `ProcessType=Background`, logs at `/tmp/consolette.*`.
3. **Log + cache paths** move `/tmp/claude-proxy-rs.*` → `/tmp/consolette.*`; the
   mcp metrics cache `~/.cache/claude-proxy/` → `~/.cache/consolette/` (both the
   writer in `mcp-proxy` and the reader in `/metrics` updated together).
4. **Config dir** default `~/.config/consolette/conf.d/`.
5. **`make migrate` cutover:** `launchctl unload com.claude-proxy-rs` (ignore if
   absent) → `launchctl load com.consolette` → `launchctl start`/verify
   `curl -sf localhost:47000/health`. The **old plist is retained** until the soak
   window (Epic 8) passes; rollback = reverse the two launchctl calls.
6. **Feature preservation is asserted, not assumed:** a smoke checklist confirms
   compression/cache-aligner/verbosity/memory/metrics/dashboard/mcp-gateway all
   respond post-rename; `cargo clippy --deny warnings` stays clean (NFR-5).
7. **Sequencing:** the pure crate rename is low-risk and MAY be pulled forward as a
   mechanical first commit (so later epics author modules under `consolette`); the
   **launchd cutover is sequenced last** (after the functional epics) to keep the
   only user-facing-downtime-risk step at the end, behind the soak gate.

## Alternatives Considered

| Option | Rejected because |
|--------|-----------------|
| Keep the `claude-proxy-rs` binary name, alias only | CD-5 fixes the binary name `consolette`; a rename is in scope. |
| Change the listen port during the rename | Would force client reconfiguration (NFR-1). Keep 47000. |
| `RunAtLoad=true` on either plist | Rejected for BOTH the hand plist and the ansible-rendered agent (single settled value = `false`). Both `com.claude-proxy-rs` and `com.consolette` bind 47000, so auto-start on load risks two agents fighting for the port during cutover; start is always explicit (`make migrate` / ansible `launchctl kickstart` after the old agent is unloaded). |
| Delete `com.claude-proxy-rs` immediately | Risks dropping daily usage if the new agent misbehaves; retain until soak sign-off. |
| Drop the `mcp-proxy` binary | Still in use (mcp-gateway feature); kept. |

## Consequences

- One mechanical rename touches `Cargo.toml`, `src/main.rs` (doc, service string,
  log paths, cache path), and `Makefile` targets/label/install path.
- Two launchd agents coexist briefly during soak; only 47000 is bound, so exactly
  one must be running at a time — `make migrate` enforces unload-before-load.
- Existing `~/.config`/cache artifacts under the old name are orphaned; the
  migration doc notes optional cleanup after soak.
- No behavioral change to any feature module — the rename is name-only plus the
  config/routing extensions from the other epics.
</content>
