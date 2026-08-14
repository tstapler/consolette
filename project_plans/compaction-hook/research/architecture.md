# Research: Architecture (Phase 2) — compaction-hook

Scope: where the new JSONL-transcript-compaction logic should live in consolette's
existing module/binary/MCP-tool structure, and how it should integrate with the
existing compression engine and provider/auth layers. Based on reading
`src/lib.rs`, `src/main.rs`, `src/mcp_gateway.rs`, all of `src/compression/*`,
`src/auth/mod.rs` + `src/auth/exec.rs`, `src/providers/mod.rs`,
`src/bin/cmdcrush/main.rs`, `src/bin/mcp-proxy/main.rs`, `Cargo.toml`,
`project_plans/consolette/implementation/plan.md`, and ADR-003/ADR-007 (ADR-001,
002, 004-006 not read in full this pass — routing/config/rate-limit/rename
decisions, not relevant to this feature).

## 1. Existing architecture, as verified

- **Lib + 3 bins.** `src/lib.rs` re-exports feature modules (`auth`, `compression`,
  `config`, `mcp_gateway`, `providers`, `routing`, `system_prompt`, etc.); three
  `[[bin]]`s share it: `consolette` (`src/main.rs`, the proxy server + CLI),
  `mcp-proxy` (`src/bin/mcp-proxy/main.rs`, its own `mod`s, not exported from the
  lib), `cmdcrush` (`src/bin/cmdcrush/main.rs`, a one-shot CLI that imports
  `consolette::compression::*` directly).
- **`src/main.rs`'s `Command::Mcp` is an unimplemented stub** —
  `anyhow::bail!("MCP server not yet implemented")` with a comment to wire up
  `rmcp` over stdio. This is a real gap, not a design choice: consolette has no
  working first-party MCP server today.
- **`src/mcp_gateway.rs` is a proxy/gateway to *external* upstream MCP servers**,
  mounted as axum `/mcp/{server_name}` routes (`GatewayHandler: ServerHandler`
  forwards `list_tools`/`call_tool` to a configured upstream via
  `StreamableHttpClientTransport`, applies an allowlist, TTL-caches the tool
  list). It is not a place where a native/first-party tool gets registered —
  there is no such registration pattern anywhere in the codebase today.
- **The actual working example of a native `rmcp` server is `mcp-proxy`'s own
  `run_serve`** (`src/bin/mcp-proxy/main.rs:56-88`): `rmcp::ServiceExt::serve`
  over `rmcp::transport::io::stdio()`, wrapping a hand-written `ProxyServer`
  (`server.rs`, not read). This — not `mcp_gateway.rs` — is the pattern to copy
  for exposing a native tool over stdio.
- **`src/compression/*` operates on proxy-side Claude API message JSON**
  (`serde_json::Value` request bodies), not JSONL transcript rows. Confirmed via
  `engine.rs`'s `CompressionEngine::compress_request` pipeline: floor-check →
  double-compression guard → per-content-block compression → tool-pair
  validation (revert if a `tool_result.tool_use_id` gets orphaned from its
  `tool_use.id`) → `RewindStore` archival → marker injection. This is a
  different data model from a JSONL transcript compactor, confirming the
  requirements doc's stated concern.
- **Six of the eight `compression` submodules are pure, data-model-agnostic
  text/JSON transforms**, independently usable outside the proxy-message shape:
  `text_compressor.rs` (`TextCompressor::compress(&str) -> String`: CR-collapse,
  ANSI-strip, consecutive-line dedup, blank-line normalization, timestamped-log
  dedup), `diff_compactor.rs` (`is_diff`/`compact_diff`), `line_truncate.rs`
  (`truncate_lines`), `path_collapse.rs` (`collapse_common_prefix`),
  `code_compressor.rs` (`compress_fenced_blocks`, tree-sitter comment-stripping
  in fenced code), `smart_crusher.rs` (`SmartCrusher::compress(&Value)`, JSON
  array field-elision). **`cmdcrush` already proves this reuse pattern**: its
  `compress_text` (`src/bin/cmdcrush/main.rs:155-204`) composes exactly these
  six primitives against arbitrary shell-command output, completely independent
  of the proxy message pipeline — this is the precedent to follow, not
  `engine.rs`.
- **`RewindStore`** (`src/compression/rewind.rs`): `moka::future::Cache`,
  TTL=10min, cap=500, SHA-256 8-hex-char keys, in-memory only. `cmdcrush` uses
  the same *marker format* (`format_rewind_marker`) but its own **on-disk**
  archival (`archive_original`: writes `<hash>.orig` under `--archive-dir`,
  retrieved via `cmdcrush --retrieve <hash>`) — i.e., the codebase already has
  two divergent omission-cache backends for two different lifetime
  requirements (proxy-request-scoped in-memory vs. CLI-invocation-scoped disk
  file). A transcript-compaction omission cache needs a *third* lifetime
  (must survive from compaction until a much-later `/resume`, potentially
  days), so a new backend is justified, not a re-use of either.
- **`src/auth/exec.rs` (ADR-007 §2) is the established subprocess-with-JSON
  pattern**: resolve command → check owner/permissions (`check_permissions`,
  rejects world-writable or non-owned binaries) → spawn via
  `tokio::process::Command` → write one JSON line to stdin → read stdout under
  `tokio::time::timeout` → treat any failure (non-zero exit, timeout,
  unparseable output) uniformly as "unavailable", with stdout/stderr never
  logged. This is the idiomatic consolette shape for "shell out to an external
  program and get structured output back," and should be followed for the
  `claude -p --resume` subprocess call.
- **`src/providers/*`** (`Provider` trait, `send(body, headers, stream) ->
  Result<ProviderResponse, ProviderError>`) is built for the proxy's own
  `/v1/messages`-shaped request/response cycle against Anthropic/Bedrock/an
  OpenAI-compatible endpoint. It has **no concept of a Claude Code session id,
  `--resume`, or session-continuation** — that is Claude Code CLI-specific
  behavior, not an Anthropic Messages API concept. Routing summarization
  through `providers::Provider` would mean re-implementing whatever
  session-state Claude Code's `--resume` reconstructs server-side, which is out
  of scope and would break the "preserve tool-call structure verbatim" goal
  magic-compact relies on `--resume` for.
- **plan.md / ADR-003 / ADR-007 conventions relevant here**: (1) "thin
  CLI/MCP transport, logic in independently testable modules" — plan.md's
  `Core` facade for the *proxy* explicitly keeps `cli.rs`/`mcp.rs` as adapters
  over shared logic; the same shape applies to a compaction feature (transcript
  module owns logic, CLI subcommand + MCP tool are both thin callers). (2)
  ADR-007 establishes the exec-subprocess-with-JSON-and-timeout pattern as the
  house style for external-process integration, which is exactly the shape
  needed for the `claude -p --resume` call. (3) No ADR yet exists for
  transcript compaction — if the omission-cache backend or a
  "pluggable summarizer" abstraction is adopted, that is a new
  architectural decision and should get its own ADR-008 in the plan phase.

## 2. Recommendations

### (a) Module location: new top-level `src/claude_code_session/`

Add `pub mod claude_code_session;` to `src/lib.rs`, alongside `compression`,
not nested under it. Rationale: the requirements doc's own framing is correct —
JSONL transcript rows (turn/chain reconstruction, `compact_boundary` rows,
session-id semantics) are a materially different data model from proxy
message arrays, so folding this into `src/compression/mod.rs`'s module list
would blur a currently-clean boundary (every existing `compression`
submodule either transforms proxy-message `Value`s or is a pure text/JSON
utility with no transcript awareness). The new module should *depend on*
`consolette::compression` for the six reusable primitives (import
`TextCompressor`, `compact_diff`/`is_diff`, `truncate_lines`,
`collapse_common_prefix`, `compress_fenced_blocks`, `SmartCrusher` directly,
exactly as `cmdcrush` does) rather than reimplementing them, but should own its
own transcript-specific types (turn/chain structs, boundary detection, the
omission cache, summarization subprocess wrapper, destination-session writer).

Suggested internal layout (mirroring `compression`'s own multi-file-with-mod.rs
shape):
```
src/claude_code_session/
  mod.rs           # re-exports; the public entry point (compact(session_path, opts) -> Result<CompactionReport>)
  transcript.rs    # JSONL read/parse, turn/chain reconstruction
  boundary.rs       # prior-compact_boundary detection (don't re-summarize already-summarized turns)
  omission_cache.rs # persistent (rusqlite) hash-keyed store for pruned tool I/O, survives to a later /resume
  summarize.rs      # subprocess wrapper around `claude -p --resume` (exec.rs-style: spawn, timeout, structured failure)
  writer.rs         # destination-session JSONL writer incl. system/compact_boundary row
```

### (b) Binary/subcommand: extend the existing `consolette` binary, not a new `[[bin]]`

Add a `Command::CompactSession { session: PathBuf, ... }` arm to `src/main.rs`'s
existing `Command` enum (parallel to `Run`/`Mcp`), calling straight into
`consolette::claude_code_session::compact(...)`. Do not add a fourth
`[[bin]]`. Reasoning: `cmdcrush` is a separate bin because it has its own
independent identity (its own metrics/SQLite exporter, its own OTel pipeline,
genuinely standalone "run a command and compress its output" use case
unrelated to the proxy); `mcp-proxy` is separate because it's a distinct MCP
gateway product with its own config schema. Compaction, by contrast, is one of
several things a Claude-Code-facing consolette install already needs to expose
(alongside the future first-party MCP server) — it belongs with `consolette`'s
own `Command` enum, consistent with the "thin CLI ... over shared logic"
convention in plan.md's `Core`-facade discussion. This also directly answers
open question (e): `consolette compact-session <path>` is sufficient as the
`UserPromptSubmit`-hook-equivalent entry point for this design pass; actual
hook-registration (`hooks.json`) is out of scope per requirements.md and can
simply shell out to this subcommand.

### (c) MCP tool exposure: implement the stubbed native MCP server, not the gateway

Register `read_omitted_content` (the `read_omitted_content`/omission-cache
retrieval tool) as a **first-party tool on a real implementation of
`src/main.rs`'s `mcp()`**, not in `src/mcp_gateway.rs`. Concretely:
finally implement `mcp()` using the exact pattern already proven in
`src/bin/mcp-proxy/main.rs:56-88` — `rmcp::ServiceExt::serve` over
`rmcp::transport::io::stdio()`, wrapping a new `ServerHandler` impl (e.g.
`src/mcp_server.rs`) whose `call_tool` dispatches `read_omitted_content` (and
optionally a `compact_session` tool wrapping the same logic as the CLI
subcommand) into `claude_code_session::omission_cache`. `mcp_gateway.rs`
remains solely the outbound-proxying gateway to *other* MCP servers; conflating
the two would misuse a component whose entire design (allowlist filtering,
upstream connection pooling, tool-list caching) assumes it's forwarding to an
external process. Implementing this also finally resolves the pre-existing
"MCP server not yet implemented" gap noted in `src/main.rs`, which the plan
phase should call out as a (small) incidental fix riding along with this
feature.

### (d) End-to-end data flow

```
consolette compact-session <session.jsonl>          (CLI subcommand, src/main.rs)
  -> claude_code_session::compact(path, opts)         (lib entry point)
     1. transcript::read(path)                        parse JSONL rows into typed rows
     2. transcript::reconstruct_turns(rows)            group user/assistant/tool-call/tool-result into turns
     3. boundary::find_last_compact_boundary(rows)      locate prior `system`/`compact_boundary` row, if any;
                                                         only turns after it are candidates for re-summarization
     4. omission_cache::prune(turns)                    for bulky completed tool I/O below the boundary:
                                                           - route text/JSON payloads through the SIX reused
                                                             compression::* primitives (same call shape as cmdcrush)
                                                           - persist originals (rusqlite, keyed by hash) with a
                                                             long/indefinite TTL (not moka's 10 min)
                                                           - replace with a placeholder + retrievable hash marker
     5. summarize::run(old_turns)                       spawn `claude -p --resume <session-id>` (exec.rs-style:
                                                         resolve/verify binary, JSON-or-text stdin, timeout,
                                                         uniform failure handling) to summarize old assistant
                                                         turns; user messages + tool-call structure pass through
                                                         verbatim, never sent through the subprocess
     6. writer::write_destination(new_turns, summary)   write a new session JSONL: preserved recent turns +
                                                         summary + a native `system`/`compact_boundary` row
     7. return new session id to the caller
  -> CLI prints "/resume <new-session-id>" (or MCP tool returns it as tool result)

read_omitted_content(hash)                              (MCP tool, native stdio server)
  -> omission_cache::retrieve(hash) -> original bytes/text, or "not found"/expired
```

### (e) Integration / consistency requirements

- **Reuse, don't fork, the six pure primitives** (`TextCompressor`,
  `is_diff`/`compact_diff`, `truncate_lines`, `collapse_common_prefix`,
  `compress_fenced_blocks`, `SmartCrusher`) directly from `consolette::compression`
  for pruning tool-result payloads inside transcript rows — same import shape
  `cmdcrush` already uses. Do **not** reuse `CompressionEngine::compress_request`
  or its tool-pair-validation logic verbatim; that pipeline's floor-check,
  double-compression guard, and revert-on-orphaned-tool-pair logic are
  specifically about protecting a live proxy request round-trip, not a
  one-shot transcript rewrite. If transcript pruning needs an equivalent
  "don't break a tool_use/tool_result pairing" guard, it should be a new,
  transcript-shaped check in `claude_code_session`, not a call into `engine.rs`.
- **New, dedicated omission-cache backend** (`rusqlite`, already a bundled
  dependency) instead of `RewindStore`'s `moka` cache or `cmdcrush`'s ad hoc
  `<hash>.orig` files — this cache must survive far longer than either existing
  backend's lifetime assumption (a live proxy request vs. one CLI invocation).
  Suggested location: `~/.local/state/consolette/compaction-omissions.sqlite`
  (or under `config_dir()`'s sibling state dir), with the same
  SHA-256-hash-keyed retrieval contract as `RewindStore`/`cmdcrush` for
  consistency of marker format (`format_rewind_marker` can likely be reused
  as-is for the injected placeholder text).
- **Summarization: shell out to `claude -p --resume`, not `providers::Provider`**,
  following `auth/exec.rs`'s subprocess pattern (resolve command, verify it's
  not world-writable/foreign-owned, spawn via `tokio::process::Command`,
  enforce a timeout, treat any non-zero-exit/timeout/unparseable-output
  uniformly as failure, never log stdout/stderr content). Do not route this
  through `src/providers/*` — that trait models a single Anthropic-Messages-API
  request/response, with no notion of Claude Code's session/`--resume`
  continuation semantics that magic-compact structurally depends on to
  preserve tool-call context. To keep the dependency from being an unstated
  hard requirement, define a small local `Summarizer` trait in
  `claude_code_session::summarize` with the subprocess implementation as the
  only production impl for now — this satisfies the "pluggable" spirit of the
  open question without taking on the larger scope of building session-replay
  semantics into `providers::Provider`.
- **New subsystem = new ADR.** Per plan.md's own convention ("ADR-driven
  decisions for non-standard choices"), the plan phase should draft an
  ADR-008 covering: the omission-cache backend choice (rusqlite over
  moka/files), the `Summarizer` trait + subprocess-only v1 implementation, and
  the decision to extend the `consolette` binary rather than add a new
  `[[bin]]`.
- **No regression to existing behavior**: nothing above touches
  `CompressionEngine`, `RewindStore`, `mcp_gateway.rs`'s upstream-proxying
  behavior, or `cmdcrush`'s archival format — the new module only *imports*
  shared pure functions, and implementing the previously-stubbed `mcp()` is
  additive (the command currently only errors, so there is no existing
  behavior to preserve there).
