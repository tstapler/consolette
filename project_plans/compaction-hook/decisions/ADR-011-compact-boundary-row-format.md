# ADR-011: Compact-boundary row matches Claude Code's native shape, not magic-compact's shipped code

**Status**: Accepted (empirically verified 2026-08-14 — see Empirical Verification below; supersedes this ADR's original decision)
**Date**: 2026-08-13
**Relates to**: requirements.md open question — recompaction-boundary detection algorithm; `research/features.md`'s documented docs-vs-code discrepancy

## Context

`research/features.md` found a direct discrepancy inside magic-compact
itself: `docs/ClaudeCode.md` describes the boundary row written into a
destination session transcript as a native
`type: "system"`, `subtype: "compact_boundary"` row. The actual shipped
TypeScript (`compact.ts:164-179`) writes something different:

```ts
{
  type: "user",
  isMeta: true,
  magicCompact: { boundary: true, ... },
  // ...
}
```

i.e., a `user`-typed, `isMeta: true` row carrying a plugin-namespaced
`magicCompact.boundary` marker — not the documented `system`/
`compact_boundary` shape. Since Claude Code's on-disk transcript schema is
undocumented upstream (per `research/pitfalls.md`), the docs describe
either aspirational or superseded behavior; the code is what Claude Code's
own `/resume` has actually been exercised against.

Idempotent recompaction also depends on two markers magic-compact checks
for on a later compaction pass: `magicCompact.boundary === true` (the
boundary row itself) and `magicCompact.summary === true` (rows produced by
a prior summarization), so a second `/magic-compact` run doesn't
re-summarize already-summarized content.

## Empirical Verification (2026-08-14)

Per this ADR's own stated fallback plan ("if it fails, the fallback is to
try the documented `system`/`compact_boundary` shape next"), Task 5.1.0's
companion risk (plan.md Epic 5.1) called for checking real, native Claude
Code session transcripts on disk before implementation, rather than relying
solely on magic-compact's reverse-engineered shipped code. Read-only
inspection (`grep -rl 'compact_boundary' ~/.claude/projects/`, no files
under `~/.claude/projects/` modified) found upward of 19 real transcripts
containing native `compact_boundary` rows, across at least two distinct
projects, produced by Claude Code's own built-in auto-compaction — not by
magic-compact or consolette. A representative row (session
`e05606c3-7ddf-4ad9-b992-8e448681bae5`):

```json
{
  "parentUuid": null,
  "logicalParentUuid": "c0657cec-0362-4941-91d3-72000df3b237",
  "isSidechain": false,
  "type": "system",
  "subtype": "compact_boundary",
  "content": "Conversation compacted",
  "level": "info",
  "compactMetadata": {
    "trigger": "auto",
    "preTokens": 108562,
    "postTokens": 17027,
    "cumulativeDroppedTokens": 91535,
    "durationMs": 189663,
    "preservedSegment": { "headUuid": "...", "anchorUuid": "...", "tailUuid": "c0657cec-0362-4941-91d3-72000df3b237" },
    "preservedMessages": { "anchorUuid": "...", "uuids": ["..."], "allUuids": ["..."] }
  },
  "uuid": "31ad2874-7102-49b1-9c4f-3389c043b644",
  "timestamp": "2026-08-06T16:42:13.957Z",
  "sessionId": "e05606c3-7ddf-4ad9-b992-8e448681bae5"
}
```

This **contradicts** this ADR's original decision and confirms the
opposite of what "Alternatives Considered" below assumed: the shape
actually written by Claude Code itself (native `/compact`, not a plugin) is
the **documented** `type: "system"` / `subtype: "compact_boundary"` shape,
with `parentUuid: null` and a separate `logicalParentUuid` pointing at the
last pre-boundary row — not magic-compact's `type: "user"` / `isMeta: true`
shape. magic-compact's shipped code, it turns out, was reverse-engineering
against a different (plugin-driven) code path, not against what native
Claude Code itself round-trips through `/resume`. Real, on-disk evidence
from Claude Code's own output beats a third-party plugin's shipped
behavior as precedent for what `/resume` tolerates.

## Decision (superseding the original decision above)

`writer.rs`'s `build_boundary_row` constructs a row matching the **real
observed native shape**, structurally:

```json
{
  "type": "system",
  "subtype": "compact_boundary",
  "uuid": "<generated>",
  "parentUuid": null,
  "logicalParentUuid": "<uuid of the last row before this boundary>",
  "isSidechain": false,
  "content": "Conversation compacted",
  "consoletteCompact": { "boundary": true, "sourceSessionId": "<uuid>", "prunedCount": <n> }
}
```

`compactMetadata`/`level` are not reproduced — they describe Claude Code's
own auto-compaction internals (token counts, timing) that consolette's
compactor has no equivalent for, and nothing in `/resume`'s observed
behavior suggests they're required (their absence does not change `type`/
`subtype`/`parentUuid`/`logicalParentUuid`, which is the structural part
of the shape that matters). consolette keeps its own `consoletteCompact`
marker object nested in the row rather than reusing Claude Code's
`compactMetadata` key, for the same reason as the original decision: so a
transcript produced by consolette's compactor is never confused with one
Claude Code's own native compaction produced.

`boundary.rs`'s `is_boundary_or_summary_row` needs no change — it already
scans `extra["consoletteCompact"]` for the `boundary`/`summary` markers,
independent of the row's `type`/`subtype`, so it continues to work
unchanged against this corrected shape. Summary rows (produced by
consolette's own summarization, not native to Claude Code — there is no
native equivalent to point to) are unaffected by this finding and keep
their original shape: `{ "...": "...", "consoletteCompact": { "summary": true } }`
on an otherwise-normal row.

## Alternatives Considered

| Option | Rejected because |
|---|---|
| Mirror magic-compact's shipped code (`type: "user"`, `isMeta: true`, `magicCompact.boundary`) — the original decision above | Empirically contradicted: real, native Claude Code `compact_boundary` rows on this machine use `type: "system"`/`subtype: "compact_boundary"`, not this shape. Kept in this ADR (rather than deleted) as the record of what was tried first and why it changed. |
| Invent a wholly new marker shape independent of both magic-compact and native Claude Code | Discards the strongest available precedent — real transcripts Claude Code's own `/resume` has actually processed — for no benefit. |

## Consequences

- `boundary.rs` and `writer.rs` depend on the corrected JSON shape
  documented above. `is_boundary_or_summary_row`'s detection logic did not
  need to change since it is `type`/`subtype`-agnostic.
- consolette's boundary rows are not currently interoperable with Claude
  Code's own native `compactMetadata`-bearing rows (no `compactMetadata` is
  written) — this is accepted for v1, as consolette's compactor has no
  equivalent internals (token counts, trigger, duration) to report, and
  nothing observed in `/resume`'s behavior requires it.
- Because the marker is namespaced (`consoletteCompact`, not
  `magicCompact` or Claude Code's own `compactMetadata`), consolette's
  compactor will not recognize (and will not attempt to avoid
  re-summarizing) content previously compacted by magic-compact or by
  Claude Code's own native auto-compaction, and vice versa. This is
  accepted as correct for v1: none of the three are required to be
  interoperable, only for each to be internally idempotent.
- Task 4.1.1e (manually running `consolette compact-session` against a
  real transcript and confirming `claude --resume <new-session-id>` works)
  remains outstanding — this empirical check strengthens confidence in the
  row shape but does not replace an actual `/resume` exercise, which
  requires a live Claude Code CLI session and is not automatable in CI.
