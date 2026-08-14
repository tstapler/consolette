# Requirements: compaction-hook

**Date**: 2026-08-13
**Type**: feature addition (existing project: consolette)

## Problem Statement
Claude Code sessions accumulate large JSONL transcripts (`~/.claude/projects/{sanitizedCwd}/{sessionId}.jsonl`)
that eventually blow the context window. The `magic-compact` Claude Code plugin
(TypeScript/Bun, at `/Users/tstapler/code/github.com/tstapler/magic-compact`) solves this
today via a `/magic-compact` slash command backed by a `UserPromptSubmit` hook: it reads
the transcript, summarizes old assistant turns via `claude -p --resume` while preserving
user messages and tool-call structure verbatim, prunes bulky completed tool I/O (backed by
an omission cache + a `read_omitted_content` MCP retrieval tool), writes a new destination
session transcript with a native `system`/`compact_boundary` row, and tells the user to
`/resume <new-session-id>`.

The user wants this mechanism natively in Rust, as part of consolette, rather than running
a second TypeScript/Bun plugin alongside consolette's existing Rust compression subsystem
(`src/compression/*`) and MCP gateway (`src/mcp_gateway.rs`). consolette already does
context/message compaction for a different case — a claude-proxy-rs-derived Claude API
proxy — but has no notion of Claude Code's on-disk JSONL session transcripts, its
turn/chain structure, or the `compact_boundary` resume mechanism.

## Users / Consumers
- Claude Code end users (human developers) running consolette locally, who invoke
  compaction either via a slash-command-equivalent hook or an explicit CLI/MCP call.
- Claude Code itself, as the process that fires `UserPromptSubmit` hooks and that reads
  `~/.claude/projects/.../*.jsonl` transcripts and honors `system`/`compact_boundary` rows
  on `/resume`.
- Internally: consolette's own MCP gateway/tool surface (`src/mcp_gateway.rs`) and CLI
  (`src/main.rs`), which need a new subcommand and/or MCP tool exposing this behavior.

## Success Metrics
- Feature parity with magic-compact's documented behavior (per
  `magic-compact/docs/ClaudeCode.md`, `magic-compact/docs/Core.md`, and any pruning-rules
  doc under `magic-compact/internal/Specs/ClaudeCode/Pruning.md`) for: transcript parsing,
  turn/chain reconstruction, recompaction-boundary detection, tool-I/O pruning with
  omission-cache retrieval, subprocess-based summarization, and destination-session
  writing with a `system`/`compact_boundary` row.
- Implemented as idiomatic Rust fitting consolette's existing module/binary conventions
  (ADRs 001-007, `src/compression/*` patterns), not a naive transliteration of the
  TypeScript.
- Exposed through consolette's existing surfaces: a new CLI subcommand and/or an MCP tool
  registered in `src/mcp_gateway.rs`, following the pattern of consolette's other
  binaries/tools (`consolette`, `mcp-proxy`, `cmdcrush`).
- A working end-to-end path: given a real Claude Code session JSONL file, the tool
  produces a new destination transcript that Claude Code can `/resume` into, with old
  turns summarized and bulky tool I/O pruned but retrievable.

## Constraints
- Must reuse/extend consolette's existing `src/compression/*` module family where the
  domain overlaps (pruning bulky content, summarization heuristics) rather than
  introducing a parallel, divergent compression implementation — but only where the
  actual data model (JSONL transcript rows) genuinely matches; a JSONL-transcript
  compactor is a materially different problem from proxy-side message compression, so
  divergence must be justified, not assumed away.
  Reused vs. genuinely divergent must be identified explicitly in research/plan and
  called out to the user if it forces a new abstraction.
- Follow consolette's established architecture conventions: ADR-driven decisions for
  non-standard choices, thin CLI/MCP transport with logic in independently testable
  modules, `cargo-dist`/CI conventions unaffected.
- Summarization step depends on shelling out to `claude -p --resume` (or equivalent) as a
  subprocess — same dependency magic-compact has. This is an external-process integration
  point that must be designed for failure (subprocess missing, non-zero exit, malformed
  output).
- No specified deadline. No specified performance SLA beyond "usable interactively from a
  slash-command-style hook" (i.e., should not introduce unacceptable latency for a
  human waiting on a hook to run before their prompt is submitted).

## Scope
### In Scope
- Rust reimplementation of: JSONL transcript reading for
  `~/.claude/projects/{sanitizedCwd}/{sessionId}.jsonl`; turn/chain reconstruction
  (grouping user/assistant/tool-call/tool-result rows into logical turns); detection of
  prior compaction boundaries so re-compaction doesn't re-summarize already-summarized
  content; pruning of bulky completed tool I/O with an omission cache; a retrieval
  mechanism (MCP tool) equivalent to `read_omitted_content`; subprocess-based
  summarization of old assistant turns while preserving user messages and tool-call
  structure verbatim; writing a new destination session JSONL with a native
  `system`/`compact_boundary` row; a way for the user to invoke this (CLI subcommand
  and/or `UserPromptSubmit`-hook-compatible entry point) and to be told the new
  session ID to `/resume`.
- Design work only in this SDD run: ideate → research → plan → validate. No
  implementation in this pass.

### Out of Scope
- Any changes to consolette's existing Claude-API-proxy compression subsystem's
  *external behavior* (its current callers/behavior must not regress) — new code may
  live alongside or reuse internals, but this is not a rewrite of that subsystem.
- Packaging/distributing this as an actual Claude Code plugin manifest (hooks.json,
  plugin marketplace metadata) — the deliverable is the Rust logic and its
  CLI/MCP surface in consolette; wiring it into a live `UserPromptSubmit` hook
  registration is a follow-on integration step, not blocked here but not required
  for this design to be considered complete.
- Feature-for-feature UI/UX polish of the TypeScript plugin's user-facing messages —
  behavior parity on the mechanism, not string-for-string output parity.

## Open Questions
- Does the new logic belong under `src/compression/` (e.g., a new
  `transcript_compactor.rs` alongside `engine.rs`, `smart_crusher.rs`, etc.) or as a
  new top-level module (e.g., `src/claude_code_session/`) that *calls into*
  `src/compression/*` for the parts that generalize (text/diff pruning)? Research and
  plan phases must answer this with a concrete recommendation.
- Should the omission cache and `read_omitted_content` retrieval tool be a new MCP tool
  registered in `src/mcp_gateway.rs`, or a new `[[bin]]` entry, or both (MCP tool backed
  by shared logic also reachable from the CLI)?
- What is the exact recompaction-boundary detection algorithm in magic-compact's actual
  TypeScript implementation (not just the docs) — this must be read from
  `magic-compact/packages/*` source, not inferred from `docs/ClaudeCode.md` alone.
- Is `claude -p --resume` subprocess invocation something consolette should shell out to
  directly (mirroring magic-compact), or should summarization be pluggable (e.g., via
  consolette's own `providers` module, since consolette already talks to model APIs)?
  This affects whether the feature has an unnecessary hard dependency on the `claude`
  CLI being installed and on PATH.
- Where exactly does a `UserPromptSubmit`-hook-equivalent entry point plug in, given
  consolette has no existing hook-invocation surface — is a new CLI subcommand
  (e.g., `consolette compact-session`) sufficient, with hook wiring left as a documented
  follow-up, or does this need actual hook-registration support in consolette itself?
