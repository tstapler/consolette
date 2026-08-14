# Pitfalls Research: compaction-hook (magic-compact -> consolette port)

Evidence gathered by reading magic-compact's actual TypeScript source (not just
`docs/ClaudeCode.md`): `packages/claude-code-plugin/src/{transcript,compact,prune,omission,mcp,hook,command}.ts`.
File:line references are to that repo unless noted.

## 1. On-disk session state: parsing/mutation risks

**Pitfall — no per-line JSON error recovery.**
`readTranscriptEntries` (transcript.ts:301-309) does `content.split("\n").map(line => JSON.parse(line))`
with zero try/catch. One malformed line — from a partial concurrent write, a future
Claude Code version emitting a line consolette's parser doesn't expect, or manual editing —
throws and aborts the *entire* read, with no partial-recovery path.
*Mitigation for Rust port*: parse line-by-line, collect `(line_no, Result<Value, Error>)`;
skip and log unparseable lines by default (configurable strict mode) rather than aborting
the whole compaction. Never silently drop a line without at least a warning — data loss
must be visible.

**Pitfall — no read/write concurrency guard against Claude Code itself.**
Nothing in transcript.ts takes a lock or checks mtime/size before/after reading. If Claude
Code is actively appending to the *source* transcript while consolette reads it (e.g. the
hook fires but Claude Code's own process is still flushing the turn that triggered
`UserPromptSubmit`), the read is not guaranteed atomic — JSONL append is normally safe to
tail (each line is a complete write) but a torn last line is possible.
*Mitigation*: after reading, drop any trailing line that isn't valid JSON (treat it as
"still being written") instead of erroring; never truncate/rewrite the *source* file in
place (magic-compact never does — it always writes to a new destination file, see #3).

**Pitfall — a hand-rolled schema-drift guard already had to be added, evidence it's a real risk.**
`buildActiveChain` (transcript.ts:146-206) explicitly detects and throws on cycles in the
`parentUuid` chain (`"Cycle detected in transcript parentUuid chain."`, appears twice).
This is direct evidence the original authors hit corrupt/unexpected chain structures in
practice. `isTranscriptRow` (transcript.ts:290-299) only validates `uuid` is a string and
`type` is one of 4 known values — everything else is treated as an open bag
(`JsonRecord = Record<string, unknown>`), i.e. the TS code deliberately avoids a strict
schema so it degrades if Claude Code adds new optional fields.
*Mitigation*: in Rust, do **not** model `TranscriptRow` as a `#[serde(deny_unknown_fields)]`
struct — that would break on the first Claude Code schema addition. Use a struct with the
handful of fields the compactor actually needs, plus `#[serde(flatten)] extra:
serde_json::Map<String, Value>` to round-trip everything else byte-for-byte. Keep the same
cycle-detection guard (`seen: HashSet<Uuid>`) — port it, don't drop it as "shouldn't happen."

**Pitfall — no handling for future Claude Code schema versions.**
Nothing versions the transcript format; `PRESERVED_METADATA_TYPES` (transcript.ts:327-340)
is a hardcoded set of known top-level entry types Claude Code may write (`custom-title`,
`ai-title`, `tag`, `worktree-state`, etc.). If Claude Code adds a new metadata entry type,
magic-compact silently drops it when copying to the destination session (it's filtered by
`isPreservedMetadataEntry`, which only recognizes the hardcoded set) — this is a real,
currently-unmitigated data-loss bug in the original, not a hypothetical.
*Mitigation*: for the Rust port, invert the default — preserve *any* top-level non-transcript-row
entry by default (copy through, rewriting `sessionId` only) unless it's specifically known to be
per-source-session (there may be none), rather than an allowlist that silently drops unknown
future types.

## 2. Subprocess invocation (`claude -p --resume ...`)

**Evidence from compact.ts:85-125 (`generateSummaries`).**
- Uses `Bun.spawn(args, ...)` with args as an **array**, not a shell string — good, no shell
  injection risk from transcript content ending up in the prompt. Preserve this in Rust:
  `std::process::Command::new("claude").args([...])`, never build a shell string.
- **No timeout.** `Promise.all([stdout.text(), stderr.text(), summaryProcess.exited])`
  will hang forever if `claude -p` hangs (e.g. waiting on stdin, a stuck MCP server it
  loads, a network stall on model calls). Since this runs from a `UserPromptSubmit` hook
  the user is blocked on, an unbounded hang directly violates the stated
  "usable interactively" constraint in requirements.md.
  *Mitigation*: wrap the subprocess call in a hard timeout (`tokio::time::timeout` or
  `wait_timeout` crate for sync); on timeout, kill the process group and fail the
  compaction with a clear, recoverable error (leave source untouched — see #3).
- **PATH resolution**: `"claude"` is resolved via PATH with no explicit check that it
  exists first. A missing/renamed binary produces an OS-level spawn error, not a clean
  message.
  *Mitigation*: `which`/`Command::new("claude").output()` preflight (or catch spawn
  `ErrorKind::NotFound` specifically) and surface "claude CLI not found on PATH" rather
  than a raw OS error.
- **Working directory / auth context**: relies on ambient `cwd`/env inherited from the
  hook process; not explicitly set. If consolette's subcommand is invoked from a different
  cwd than the original session's project directory, `claude -p --resume <path>` may
  resolve project-scoped config/auth differently than intended.
  *Mitigation*: explicitly set the child's cwd to the session's project directory
  (derivable from the transcript path's `{sanitizedCwd}` segment) rather than inheriting
  the parent's.
- **stdout format coupling**: `parseSummaries` (compact.ts:518-554) parses raw stdout via
  a fragile regex looking for `<summary>`/`<user>`/`<assistant>` tags the *prompt itself
  requested the model produce* — this is inherently brittle (depends on the model
  following instructions), not versioned, and throws hard on any deviation ("Expected N
  summaries, received M"). It also assumes UTF-8 text (`Response.text()`); no length cap
  before buffering fully into memory.
  *Mitigation*: keep this fragility but bound its blast radius — never mutate the source
  transcript before this parse succeeds (already true in the original); consider using
  `claude -p --output-format json` if available instead of scraping free-text tags, to
  reduce format coupling to the CLI's own JSON schema (still versioned, but less brittle
  than ad hoc tag-matching in a persuaded natural-language response). Cap stdout read size
  to avoid unbounded memory use on a runaway/looping subprocess.
- **Analysis-copy cleanup relies on `finally` + swallowed unlink error** (compact.ts:106-124):
  `await unlink(analysis.transcriptPath).catch(() => undefined)` — if this leaks (e.g.
  process killed via SIGKILL before `finally` runs), a stray full-transcript copy is left
  in `~/.claude/projects/...` indefinitely. Not itself a correctness bug, but a residual
  disk/privacy concern (full transcript, possibly with secrets, left on disk with no
  scheduled cleanup).
  *Mitigation*: Rust port should use a `tempfile`-crate-style guard (`Drop` impl) so the
  copy is removed even on `?`-propagated early returns, and consider placing the analysis
  copy under a directory with tighter permissions/TTL rather than alongside real sessions.

## 3. Data loss specific to compaction

**Primary existing safeguard — verify and preserve this property.**
The whole design already treats the *source* transcript as read-only: `compactTranscript`
(compact.ts:28-57) never writes to `sourceTranscriptPath`; it only ever creates a **new**
destination file via `createTranscriptSession` (random UUID, `COPYFILE_EXCL`-style
existence check, transcript.ts:51-73) and writes fully-formed content there via
`writeTranscriptEntries` in one `Bun.write` call (transcript.ts:93-101). This is the single
most important property to preserve in the Rust port: **compaction must be a pure
copy-transform, never an in-place mutation of the file Claude Code itself owns.** If the
Rust port ever grows an "in place" mode for convenience, that is the one place where a
crash mid-write turns into unrecoverable data loss (the *original* session), not just a
failed compaction attempt.

**Pitfall — destination write is not atomic.**
`Bun.write(transcriptPath, joinedString)` writes the whole destination in one call, but
that is not guaranteed atomic at the OS level for large files (Bun implements it as an
open+write, not write-to-temp+rename). A crash/kill mid-write leaves a truncated, corrupt
*destination* JSONL that Claude Code would fail to parse on `/resume`. Because it's a
`.jsonl` with `COPYFILE_EXCL`-guaranteed-fresh UUID filename, this is a **destination-only**
failure (source is safe) but still a bad user experience: the hook reports a resumable
session ID that turns out broken.
*Mitigation*: write to `destination.jsonl.tmp` then `rename()` into place (atomic on POSIX
same-filesystem rename) before reporting success to the user/hook output. Only announce
the new session ID after the rename succeeds.

**Pitfall — pruning depends entirely on the omission cache surviving.**
Tool I/O is pruned (prune.ts) and replaced with a `Content ID` pointing into
`~/.claude/magic-compact/{sessionId}.json` (omission.ts). If that cache file is lost,
corrupted, or evicted (nothing currently evicts it, but there's also no cap on its growth —
see below), `read_omitted_content` permanently loses access to the original content with no
fallback — the summarized/pruned transcript is the only remaining copy, and it explicitly
says "if necessary, reread" for tool types where re-reading is possible (Read, NotebookEdit)
but has no recovery path for one-shot outputs (Bash stdout, Agent/Skill output, `Write`
contents, `Edit` old/new strings).
*Mitigation*: (a) never delete/evict cache entries automatically — cap total on-disk size
per session and warn/refuse further pruning rather than silently overwriting entries; (b)
`loadOmissionCache`/`saveOmissionCache` do a naive load-modify-save with no locking
(omission.ts:15-38) — concurrent compactions of the same session (e.g. user re-runs the
hook twice quickly) can race and lose entries from whichever write loses; use an
advisory file lock (`fs2`/`fd-lock` crate) around the read-modify-write, or make cache
entries content-addressed (hash-based ID) so re-adding the same content is idempotent
rather than racy; (c) the destination transcript should carry a stable reference to which
cache file it depends on so a broken/missing cache is detectable and reported clearly
rather than manifesting only when the model happens to call `read_omitted_content`.

**Pitfall — no unbounded growth control on the omission cache.**
`allocateOmission` just appends a new numbered entry (omission.ts:40-49) forever; nothing
prunes old entries. Over many compactions of a long-lived project, this file grows without
bound. Not itself a correctness bug, but worth deciding explicitly in the plan phase
(size cap? per-session TTL? none, matching original behavior?).

## 4. Rust-specific porting risks

**Async/sync mismatch.** consolette's `src/compression/*` and MCP gateway
(`src/mcp_gateway.rs`) run under `rmcp`/tokio (async). This feature needs: (a) blocking
file I/O (transcript read/write — fine via `tokio::fs` or `spawn_blocking`), and (b) a
**child process with a hard timeout** whose stdout/stderr must be fully drained
concurrently with the wait (exactly what `Bun.spawn` + `Promise.all` does). Doing this
correctly in tokio requires `tokio::process::Command` with `.stdout(Stdio::piped())` and
reading both streams concurrently (`tokio::join!`) to avoid deadlock from a full pipe
buffer if you naively `wait()` before draining — a classic subprocess-hang bug distinct
from magic-compact's own no-timeout issue. *Mitigation*: use `tokio::process`, not
`std::process`, inside the MCP-gateway/async path; if the CLI subcommand is a separate
sync `[[bin]]`, it's fine to use blocking `std::process::Command` there with an explicit
watchdog thread or `wait_timeout` crate — but pick one pattern and keep the core
"invoke summarizer" logic transport-agnostic (returns a `Result` given already-read
transcript data) so both call sites share it, matching the "thin CLI/MCP, logic in
testable modules" convention in CLAUDE.md.

**serde model risk for a loosely-typed, evolving schema.** As above (#1): do not model
`TranscriptRow` with `deny_unknown_fields`; use `#[serde(flatten)] extra: Map<String,
Value>` so a Claude Code schema update that adds fields doesn't break deserialization,
and so round-tripping preserves fields the compactor doesn't understand (mirrors
`structuredClone` + targeted field mutation in the TS, e.g. `copyRow` in compact.ts:288-300).
Write a round-trip test: parse a real transcript line, re-serialize, byte-compare (modulo
intentional field rewrites) — this is the test that would have caught the
`PRESERVED_METADATA_TYPES` silent-drop bug above.

**Two incompatible JSON-handling/compression patterns.** consolette already has a
compression engine (`src/compression/engine.rs`, `smart_crusher.rs` per requirements.md)
for proxy-side message compression. The requirements doc already flags this as an open
question; from a pure pitfall-avoidance standpoint: pruning heuristics in prune.ts
(per-tool-name field omission, word/char thresholds) are a **different algorithm shape**
than compacting a flat message-content array for an API request — don't force-fit
transcript-row pruning through the existing compression engine's types just to "reuse"
it; that risks the existing subsystem's regression surface (explicitly out of scope to
break, per requirements.md) growing types/branches it wasn't designed for. If genuine
overlap exists (e.g. a generic "truncate text over N words/chars, return content-address
for retrieval" primitive), extract *that* primitive into a shared low-level helper, but
keep the JSONL-transcript-specific orchestration (turn/chain reconstruction, boundary
detection, per-tool-name omission rules) in its own module.

## 5. Security: secrets in transcripts, `read_omitted_content` as an exfiltration surface

**Confirmed real gap in the original, not hypothetical — `findSessionIdBySuffix` has no
session scoping.** `readOmittedContent` (omission.ts:51-66) takes a bare `contentId`
string like `{12-char-suffix}:omitted-003` and calls `findSessionIdBySuffix`
(omission.ts:94-109), which **scans every file in `~/.claude/magic-compact/` for one whose
name ends in that suffix** — there is no check that the suffix belongs to the session that
is *currently* calling the MCP tool. Because the MCP server (mcp.ts) is a single
long-lived process registered globally for Claude Code (not spun up per-session), **any
conversation that can call `read_omitted_content` can retrieve cached omitted content from
any other session on the same machine**, provided it can produce/guess a valid content ID
(the format is discoverable — it's literally written into the pruned transcript's omission
notices for the current session, so a determined agent knows the exact ID shape and could
enumerate `omitted-000..999` for a different session's 12-char suffix, itself derivable
from that session's own transcript filename). This is a **cross-session data-exfiltration
vector**, and it's the single highest-priority risk to design against for a port that will
run as a shared MCP tool inside consolette's own gateway.
*Mitigation for the Rust port*: the retrieval tool call **must** be scoped to the calling
session — thread the `sessionId` the MCP request/tool-call arrived under (Claude Code's MCP
tool-call context carries this, or it can be threaded through consolette's own gateway
session state) and only permit reads from that exact session's cache file, never a
suffix-based cross-session directory scan. Never accept a bare "look up by ID" model where
the ID alone determines scope with no session binding.

**Secrets in tool output.** Transcripts and the omission cache verbatim-store raw tool
input/output (Bash commands/output, file contents, agent messages) which commonly contain
credentials, tokens, or PII. Neither magic-compact's cache file nor its destination
transcript is encrypted; only `appendCustomTitle`'s single small metadata write sets
restrictive permissions (`mode: 0o600`, transcript.ts:377-379) — **`saveOmissionCache`
(omission.ts:31-38) sets no explicit mode**, so on a shared or misconfigured-umask machine
the cache holding potentially the most sensitive raw content could be group/world-readable.
*Mitigation*: explicitly create the omission cache directory/file with `0o700`/`0o600`
(Rust: `std::os::unix::fs::PermissionsExt`, and treat as best-effort/no-op on non-Unix)
rather than relying on inherited umask; document that cache/destination-transcript
confidentiality is only as strong as the filesystem permissions on
`~/.claude/{magic-compact,projects}` generally — this is a pre-existing property of Claude
Code's own transcript storage, not something the port can fully fix, but it should not
regress it (e.g. never write the cache world-readable when the source transcript isn't).

## 6. General TS/Bun -> Rust porting gotchas found in this codebase

- `Bun.write`, `Bun.file`, `Bun.spawn`, `Bun.stdin.text()` — all Bun-runtime sugar with no
  1:1 Rust equivalent; each needs an explicit Rust idiom (tokio::fs / std::fs +
  temp-then-rename for atomicity, tokio::process::Command, stdin read-to-string). None of
  these are semantically identical to their naive replacement — e.g. `Bun.write` doesn't
  create parent directories (this code calls `mkdir(dirname(path), {recursive:true})`
  separately in omission.ts:36 but *not* before the destination transcript write in
  transcript.ts:93-101, relying on the directory already existing since it's a sibling of
  the source file — verify this assumption holds for the Rust port's destination path
  logic too).
- ES2023 `Array.prototype.findLastIndex`/`toReversed()` (transcript.ts:33, compact.ts:128) —
  purely a stylistic convenience, direct Rust equivalents (`.rposition()`/iterate
  reversed) exist; no functional gap, just note during line-by-line porting so reversed-
  order logic isn't accidentally inverted.
- `structuredClone` (compact.ts, used repeatedly for row copies) deep-clones including
  the flattened "extra" JS object; Rust's `serde_json::Value` / the flattened struct
  field clone via `#[derive(Clone)]` is a direct equivalent as long as the "extra fields"
  bag is `serde_json::Value`, not a stringly-typed reserialization — avoid a common
  Rust-port trap of round-tripping through `to_string`/`from_str` for a "clone", which is
  slower and would mask a serialization bug as success.
- JS's loose truthiness (`if (!content)`, `.filter(Boolean)`) needed care mapping to
  Rust's explicit `Option`/`is_empty()` checks — e.g. `stringifyContent` (prune.ts:282-292)
  returns `""` for null/undefined and the caller checks truthiness; the Rust equivalent
  should return `Option<String>` or check `.is_empty()` explicitly rather than trying to
  preserve JS's blurred null/undefined/empty-string equivalence, which has already caused
  at least one subtlety in the original (`getUserText` returns `""` uniformly for "no
  message" and "empty message", transcript.ts is fine with that, but a literal Rust port
  should decide intentionally rather than accidentally collapsing the distinction).

## Summary of file/line evidence for follow-up during planning

- Cross-session leak: `packages/claude-code-plugin/src/omission.ts:94-109` (`findSessionIdBySuffix`)
- No subprocess timeout: `packages/claude-code-plugin/src/compact.ts:85-125` (`generateSummaries`)
- Non-atomic destination write: `packages/claude-code-plugin/src/transcript.ts:93-101` (`writeTranscriptEntries`)
- Source-file-is-read-only design (the thing to preserve): `packages/claude-code-plugin/src/compact.ts:28-57`
- Cycle-detection guard (evidence schema drift is a real concern): `packages/claude-code-plugin/src/transcript.ts:170,196`
- Silent-drop of unknown metadata types: `packages/claude-code-plugin/src/transcript.ts:311-313,327-340`
- Missing cache file permissions: `packages/claude-code-plugin/src/omission.ts:31-38` vs. `packages/claude-code-plugin/src/transcript.ts:377-379`
- No cache locking (race on concurrent compaction of same session): `packages/claude-code-plugin/src/omission.ts:15-38`
