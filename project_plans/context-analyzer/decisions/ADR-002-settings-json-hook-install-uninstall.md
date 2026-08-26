# ADR-002: `~/.claude/settings.json` hook install/uninstall — marker-based targeted removal, not backup-restore

**Status**: Accepted
**Date**: 2026-08-24
**Relates to**: requirements.md Constraints ("additive, backed up, reversible"); `research/pitfalls.md` §3; `research/architecture.md` §4

## Context

No code anywhere in this repo touches `settings.json` today (`grep -rn "settings.json" src/` returns zero hits) — this is entirely new surface, and it's the one part of this feature that mutates a file Tyler's other tooling (Claude Code itself, possibly `stapler-scripts/llm-sync`) depends on live. `settings.json`'s hook schema allows multiple entries per event array; Tyler may already have unrelated hooks configured (e.g. an RTK hook on `PostToolUse`), and may add more between `up` and a later `down`.

A naive "reversible" design — back up the file before `up`, restore from that backup on `down` — satisfies the letter of "backed up and reversible" but not the intent: if Tyler configures a new, unrelated hook after `up` runs, a blind restore-from-backup on `down` silently discards that addition too.

## Decision

`up` appends consolette's hook entries to each relevant event's array (never overwrites the array), each entry carrying a stable, recognizable `HookMarker` (a distinctive command-string literal — the `consolette context-hook <event>` invocation itself is sufficiently distinctive, no separate metadata key needed). `up` is idempotent: re-running it checks for an entry carrying the marker already present in that event's array before appending. `down` removes exactly the marker-carrying entries it finds, leaving every other entry (including ones added after `up` ran) untouched. `down` never restores from the Story 4.1.1 backup file; the backup exists purely as a manual-recovery safety net, not as the uninstall mechanism.

**Array-order invariant (pre-mortem P1)**: `up` always inserts consolette's entry *after* every pre-existing entry in that event's array — never before, never reordering existing entries. Claude Code runs an event's hooks in array order, so any pre-existing hook with an order-dependent side effect (e.g. a command-rewrite hook) keeps executing first, unaffected by `up`. This is an explicit invariant, not an incidental consequence of "append" — a future change to the install logic must preserve it.

Writes are atomic: write to `settings.json.tmp` in the same directory, `fsync`, then `rename()` over the original — never truncate-and-rewrite in place, so a crash mid-write can't leave Claude Code reading a half-written file.

## Consequences

- `down` is correct even when time has passed and Tyler has hand-edited `settings.json` in between — the property the requirements' Constraints section actually cares about.
- The backup file is retained as a secondary safety net (manual recovery only), not load-bearing for the primary uninstall path.
- Every write goes through `serde_json::Value` (not a strict typed struct), so unknown top-level keys and other tools'/plugins' hook entries round-trip byte-for-byte through fields this feature doesn't touch.
