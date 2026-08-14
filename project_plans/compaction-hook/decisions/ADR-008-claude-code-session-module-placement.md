# ADR-008: New `src/claude_code_session/` module, extending the `consolette` binary

**Status**: Accepted
**Date**: 2026-08-13
**Relates to**: requirements.md open questions 1 and 2

## Context

`compaction-hook` ports magic-compact's transcript-compaction mechanism into
consolette. Two open questions from `requirements.md` need a concrete answer
before tasks can be written:

1. Does the new logic live under `src/compression/` (alongside `engine.rs`,
   `smart_crusher.rs`, etc.) or as a new top-level module?
2. Does it ship as a new `[[bin]]` (like `cmdcrush`, `mcp-proxy`) or a new
   subcommand of the existing `consolette` binary?

Research (`research/stack.md`, `research/architecture.md`) found:

- `src/compression/*` (`engine.rs`, `smart_crusher.rs`, `text_compressor.rs`,
  `diff_compactor.rs`, `line_truncate.rs`, `path_collapse.rs`,
  `code_compressor.rs`) contains six reusable **pure** text/diff primitives
  (`TextCompressor::compress`, `is_diff`/`compact_diff`, `truncate_lines`,
  `collapse_common_prefix`, `compress_fenced_blocks`, `SmartCrusher::compress`)
  plus a proxy-specific, stateful `CompressionEngine`/`RewindStore` (moka
  cache, 10-minute TTL) built for the Claude-API-proxy request/response path.
- Neither existing JSONL reader (`src/learn/transcript.rs`,
  `src/bin/cmdcrush/main.rs`) lives under `src/compression/`, and neither
  models transcript turn/chain structure, compact-boundary rows, or
  destination-transcript writing.
- The domain object here — a Claude Code session JSONL file with
  parent-chain-linked rows, tool-call structure, and a resumable boundary —
  is materially different from a single proxy request/response message. The
  *only* genuine overlap is bulky-text pruning, which the six pure
  primitives already generalize.

## Decision

**New top-level module `src/claude_code_session/`**, not a `src/compression/`
submodule:

```
src/claude_code_session/
  mod.rs              # public API: `compact_session(...) -> anyhow::Result<CompactionReport>`
  transcript.rs        # JSONL parsing, tagged row enum, turn/chain reconstruction
  boundary.rs           # compact-boundary detection + the boundary row writer (ADR-011)
  prune.rs               # per-tool-name pruning rules (binary omit-over-threshold; see below)
  omission_cache.rs   # rusqlite-backed omission cache (ADR-009)
  summarize.rs          # `Summarizer` trait + subprocess impl (ADR-010)
  writer.rs               # atomic destination-transcript write (tmp + rename)
```

**Revision (post-adversarial-review, 2026-08-13): `prune.rs` does NOT call
`crate::compression`'s primitives.** The original draft of this ADR proposed
routing pruned content through the six pure primitives
(`text_compressor`, `diff_compactor`, `line_truncate`, `path_collapse`,
`code_compressor`) as a fallback chain before deciding prune-vs-keep. The
adversarial review (`implementation/adversarial-review.md`) correctly flagged
this as internally contradictory with `prune.rs`'s stated binary
omit-over-threshold contract (verbatim full content cached, or left
untouched — never a partially-compressed inline substitute) and as an
unacknowledged semantic divergence from magic-compact's own model, which
requirements.md requires be called out explicitly rather than silently
introduced. `prune.rs` is therefore a **self-contained** module: it measures
raw content length/word-count against per-tool thresholds and either leaves
a row unchanged or moves its full, uncompressed content into the omission
cache — see `implementation/plan.md` Story 2.1.1 for the corrected algorithm.
`src/compression/*`'s six primitives remain reserved for consolette's
separate proxy-compaction use case and are not called from
`claude_code_session`. It does **not** call
`compression::engine::CompressionEngine` or `compression::rewind::RewindStore`
either: those are stateful, TTL-cache-backed, and scoped to the proxy's
in-flight request/response pipeline, which has no analogue in a one-shot
transcript-file compaction, and reusing them would force transcript rows
through a cache designed to expire in 10 minutes — wrong for a cache that
must survive until a later `/resume`.

`pub mod claude_code_session;` is added to `src/lib.rs` alongside the
existing module list.

**Extends the existing `consolette` binary** with a new
`Command::CompactSession { session: PathBuf, ... }` variant in
`src/main.rs`, rather than a new `[[bin]]`. Rationale:

- `cmdcrush` and `mcp-proxy` are separate binaries because they have
  independent lifecycles (piped subprocess wrapper; long-running MCP
  server process respectively) unrelated to consolette's routing core.
  `compact-session` is a one-shot CLI operation in the same spirit as the
  existing `Run`/`Mcp` subcommands — no reason for a separate process
  boundary.
- The MCP-exposed half of this feature (`read_omitted_content`, ADR-009)
  runs inside consolette's own native MCP server (`main.rs`'s `mcp()`,
  currently a stub), so the CLI and MCP surfaces already share one binary;
  splitting `compact-session` into its own `[[bin]]` would require
  duplicating config/session-path resolution rather than sharing it via
  `claude_code_session::mod.rs`.

## Alternatives Considered

| Option | Rejected because |
|---|---|
| Extend `src/compression/*` in place | The transcript/turn/boundary/cache data model doesn't generalize from proxy message compression; would force compaction-specific types into a module whose existing contract is "pure text/diff functions." |
| New `[[bin]]` (e.g. `claude-compact`) | No independent process lifecycle need; would duplicate config-dir/session-path plumbing already available to `consolette`'s `main.rs`, and fragments the MCP tool surface across two binaries. |

## Consequences

- `src/compression/*`'s existing external behavior (proxy compression) is
  untouched — per the Revision above, `claude_code_session` does not call
  into it at all, so there is no new call site and no coupling in either
  direction. Future proxy-side compression changes cannot break
  `claude_code_session::prune`, and vice versa.
- `Cargo.toml` needs no new `[[bin]]` entry; only `src/lib.rs`'s module list
  and `src/main.rs`'s `Command` enum change structurally.
