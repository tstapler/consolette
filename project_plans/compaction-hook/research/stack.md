# Stack research: compaction-hook

## Existing consolette dependencies relevant to this feature

`/Users/tstapler/code/github.com/tstapler/consolette/Cargo.toml` already carries everything
this feature needs; no new crates are required for the core mechanism.

| Concern | Existing crate (pinned version, from Cargo.lock) | Notes |
|---|---|---|
| JSON parsing/serialization | `serde` 1.0.229 (`derive`), `serde_json` 1.0.151 | Already used with untagged enums for flexible transcript shapes (see below). |
| CLI | `clap` 4.6.6 (`derive`) | New subcommand should follow `src/main.rs` conventions. |
| MCP server/tools | `rmcp` 2.2.0 (`server`, `client`, `transport-io`, streamable-http) | `src/mcp_gateway.rs` is the registration point for a `read_omitted_content`-equivalent tool. |
| Async runtime | `tokio` 1.53.1 (`full`) | `tokio::process::Command` already used in `src/auth/exec.rs:184` and `src/providers/bedrock.rs:538` for subprocess invocation with captured stdout. |
| Glob-based file discovery | `glob` 0.3 | Already used to find `~/.claude/projects/**/*.jsonl` in `src/learn/transcript.rs`. |
| Structured logging | `tracing` 0.1 / `tracing-subscriber` 0.3 | Used throughout for defensive-parse diagnostics. |
| Error handling | `anyhow` 1 (app-level), `thiserror` 1 (typed library errors) | Matches ADR conventions. |
| Timestamps | `chrono` 0.4 (`serde`) | Already used for the `.tmp`-then-rename metrics snapshot pattern. |
| Temp files (tests) | `tempfile` 3 (dev-dependency) | Used pervasively in existing tests for fixture JSONL files. |
| Hashing (for omission-cache keys) | `sha2` 0.10, `hex` 0.4 | Already present for the compression subsystem; reusable for cache keys. |
| Compression/pruning primitives | `src/compression/{engine,diff_compactor,text_compressor,line_truncate,path_collapse,code_compressor,smart_crusher,rewind}.rs` | Existing bulky-content pruning logic for the Claude-API-proxy path; candidate for reuse in tool-I/O pruning (see requirements' reuse-vs-divergence question — a plan-phase call, not answered here). |

No `[[bin]]` changes are needed to *add* dependencies; a new binary or subcommand would just
reference the existing `[lib]` (`src/lib.rs`) target, consistent with `cmdcrush` and
`mcp-proxy`.

## magic-compact's TS/Bun dependencies (parity reference only)

`/Users/tstapler/code/github.com/tstapler/magic-compact/package.json` lists **no runtime
dependencies at all** — only devDependencies (`typescript`, `eslint`, `prettier`, `@types/bun`,
`@types/node`). This means magic-compact's JSONL parsing, subprocess invocation, and file I/O
are implemented directly against Bun/Node built-ins (`Bun.file`, `node:child_process`, etc.),
not third-party libraries. There is nothing to map 1:1 to a Rust crate — Rust's stdlib +
`serde_json` + `tokio::process` cover the same ground natively, arguably with stronger typing.

## Existing JSONL/transcript parsing utilities found in `src/`

Two independent JSONL-transcript readers already exist — this is the single most important
finding for the plan phase's "where does this belong" question:

1. **`/Users/tstapler/code/github.com/tstapler/consolette/src/learn/transcript.rs`** — a
   defensive JSONL parser for `~/.claude/projects/{sanitizedCwd}/{sessionId}.jsonl`. It:
   - Uses `serde(untagged)` for `message.content` (`RawContent::Text(String)` vs
     `RawContent::Blocks(Vec<RawContentBlock>)`), matching the shape variance the requirements
     doc calls out (user/assistant/tool_use/tool_result rows).
   - Deserializes line-by-line via `content.lines()` + `serde_json::from_str::<RawEntry>(line)`
     per line, silently skipping unparseable lines/unknown `type` values (schema-instability
     tolerant, matching the "no official spec" constraint in the requirements doc).
   - Already models `uuid`, `parentUuid`, `isSidechain`, `isMeta` — the same parent-chain
     linkage fields a turn/chain reconstruction needs.
   - Currently **discards** everything except plain-text `user`/`assistant` content (explicitly
     skips `tool_use`, `tool_result`, `thinking`, sidechain, and meta entries) — it was built for
     correction-pattern mining, not full-fidelity transcript reconstruction, so it is a strong
     *pattern* reference but not directly reusable as-is for compaction (which needs tool-call
     structure preserved, not discarded).
   - Reads the whole file via `std::fs::read_to_string` rather than streaming with `BufRead` —
     fine at current transcript sizes but worth revisiting if large-session performance matters.

2. **`/Users/tstapler/code/github.com/tstapler/consolette/src/bin/cmdcrush/main.rs`** — a second,
   separate JSONL reader (`BufReader` + `.lines()`, `std::io::{BufRead, BufReader}`) that scans
   `*.jsonl` transcripts for `tool_result` blocks to re-run recorded tool invocations. This one
   *does* stream line-by-line via `BufReader::lines()` rather than reading the whole file into
   memory, and specifically extracts tool_result content — closer in shape to what compaction's
   tool-I/O pruning needs.

Neither module currently handles `system`/`compact_boundary` rows, turn/chain grouping beyond
raw parent-uuid links, or writing a *new* JSONL file. The plan phase should decide whether the
new transcript-compaction logic:
- extends `src/learn/transcript.rs`'s types (its `RawEntry`/`RawContent`/`RawContentBlock` shape
  is a good starting point but would need broadening to retain tool_use/tool_result/system rows
  instead of discarding them), or
- is a new sibling module that shares only the untagged-enum modeling pattern.

This is exactly the "does it belong in `src/compression/` or a new
`src/claude_code_session/` module" open question from requirements.md — both existing readers
currently live outside `src/compression/`, which weakens (but doesn't foreclose) the case for
extending `src/compression/*` directly, since neither of the two prior JSONL-reading efforts
puts transcript-shape logic in `compression/`.

## Recommended crates for this feature (all already present — no additions)

- **JSONL streaming/line-based parsing**: `serde_json::from_str` per line via
  `BufReader::lines()` (as in `cmdcrush/main.rs`) rather than `std::fs::read_to_string` (as in
  `learn/transcript.rs`) — streaming is preferable for a compactor since transcripts are, by
  definition, the large files needing compaction. No new crate needed.
- **Flexible row modeling**: `serde_json::Value` + `#[serde(untagged)]` enums, following
  `learn/transcript.rs`'s `RawContent`/`RawContentBlock` precedent. Given the requirements doc
  lists five distinct row shapes (user, assistant, tool_use, tool_result,
  system/compact_boundary), a tagged enum keyed on the `type` field (with `#[serde(other)]` or
  a catch-all `Unknown(serde_json::Value)` variant) is likely a better fit than a single
  untagged enum, to preserve round-trip fidelity for rewriting the destination transcript —
  this is a plan-phase design decision, not a new-dependency decision.
- **Subprocess invocation with stdout capture**: `tokio::process::Command`, matching the
  existing pattern in `src/auth/exec.rs:184` (spawns a child, captures stdout) and
  `src/providers/bedrock.rs:538`. Since the CLI subcommand and MCP tool surface both run inside
  consolette's existing Tokio runtime (`tokio = { features = ["full"] }`), `tokio::process` is
  the correct choice over `std::process::Command` (which would need `spawn_blocking` to avoid
  blocking the runtime) — consistent with the "should not introduce unacceptable latency"
  constraint.
- **Atomic file writes**: no new crate — `src/bin/mcp-proxy/metrics.rs` already implements the
  needed pattern (write to `path.with_extension("tmp")`, then `std::fs::rename` to the final
  path) for exactly this reason ("atomically replaces the file... to avoid partial reads").
  The destination-session JSONL write should reuse this same write-tmp-then-rename idiom.
- **Omission-cache hashing**: `sha2` + `hex`, already pinned, sufficient for content-addressed
  cache keys if the omission cache is keyed by hash of pruned content (mirrors typical
  content-addressable cache design; magic-compact's own keying scheme should still be checked
  against its actual TS source per the requirements doc's open question on recompaction-boundary
  detection).

## Version constraints to respect

- `serde` 1.0.229 / `serde_json` 1.0.151 — new code must use the same major/minor family
  already resolved in `Cargo.lock`; do not introduce a second `serde_json` via a transitive
  dependency bump.
- `tokio` 1.53.1 with the `full` feature set already enabled — `tokio::process`,
  `tokio::fs`, and `tokio::io` are all already available with no `Cargo.toml` change.
- `clap` 4.6.6 (`derive`) — new subcommand(s) should be added as `clap::Subcommand` variants
  consistent with existing `src/main.rs` structure (not inspected in this pass beyond
  Cargo.toml; confirm exact CLI enum shape in the plan phase before wiring a new subcommand).
- `rmcp` 2.2.0 with `server`/`client`/`transport-io` features — a new MCP tool (e.g.
  `read_omitted_content`) should follow whatever tool-registration pattern
  `src/mcp_gateway.rs` already uses for its existing tools (not inspected in this pass; the
  plan phase should read `src/mcp_gateway.rs` directly).
- `[lints.clippy] unwrap_used = "warn"`, `expect_used = "warn"`, `pedantic = "warn"` apply
  repo-wide — new subprocess/JSONL code must handle `Result`s via `?`/`anyhow` context rather
  than `.unwrap()`/`.expect()`, matching `learn/transcript.rs`'s existing defensive style.
