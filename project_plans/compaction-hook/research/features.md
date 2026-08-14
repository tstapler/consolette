# Phase 2 Research: magic-compact's compaction-hook mechanism (Claude Code path)

Source repo: `/Users/tstapler/code/github.com/tstapler/magic-compact` (TypeScript/Bun).
All line numbers below refer to files as read on 2026-08-13; no commit-SHA links exist for
this local, uncommitted-relative-to-consolette repo, so paths are given as
`packages/claude-code-plugin/src/<file>.ts:<line>` relative to the magic-compact repo root.

**Note on the prompt's assumed spec path**: `internal/Specs/ClaudeCode/Pruning.md`
(and any `internal/Specs/` directory) does **not exist** in this repo (`find` returned
nothing). `docs/ClaudeCode.md` references it as a broken relative link
(`../internal/Specs/ClaudeCode/Pruning.md`). The actual pruning rules exist only as code in
`packages/claude-code-plugin/src/prune.ts` — read directly below. There is also no test
suite for the Claude Code plugin package (only `packages/opencode-plugin/test/*.test.ts`
exist); all edge-case behavior below is inferred from reading the implementation, not from
tests or a spec doc.

## 1. JSONL transcript parsing — `transcript.ts`

`readTranscriptEntries` (`transcript.ts:301-309`): reads the whole file with
`readFile(path, "utf8")`, splits on `"\n"`, filters blank lines, and `JSON.parse`s each
line unconditionally.

- **Edge case NOT handled**: a malformed/non-JSON line throws uncaught from `JSON.parse`,
  which propagates up through `readActiveTranscriptRows` → `compactTranscript` → `hook.ts`'s
  top-level `try/catch`, which reports it as a generic `Magic Compact failed: <message>` and
  aborts (see Error Handling below). There is no line-skipping or partial-recovery for
  corrupt lines.
- Missing file: `readFile` throws `ENOENT`, caught the same generic way.
- Rows are split into two categories by `isTranscriptRow` (`transcript.ts:290-299`, requires
  `uuid: string` and `type` in `user|assistant|attachment|system`) vs. everything else
  (`readTranscriptEntries` returns `unknown[]`, and `readPreservedMetadataEntries` filters
  for non-transcript-row entries with allow-listed `type`s — see §7 below). This means
  **any other line type** (e.g. unknown future extension rows) is silently dropped — not
  copied to the destination, not erroring.
- Non-transcript-row entries with a `type` not in `PRESERVED_METADATA_TYPES` are silently
  dropped entirely (not even preserved) — this includes any custom/unknown top-level entry
  types.

## 2. Turn/chain reconstruction

### 2a. Active chain (`buildActiveChain`, `transcript.ts:146-206`)

Two-phase reconstruction, not a naive "read whole file top-to-bottom":

1. **Find the leaf**: build `rowsByUuid`, compute the set of UUIDs that appear as someone's
   `parentUuid` (`parentUuids`), and take `terminalRows` = rows never referenced as a parent.
   For each terminal row, walk backward via `parentUuid` until hitting a `user`/`assistant`
   row that has no `user`/`assistant` child (`hasUserAssistantChild`) — i.e., skip over
   trailing `attachment`/`system` rows to find the actual last human-visible turn row. Among
   all such candidates, pick the one with the lexicographically greatest ISO timestamp
   (`current.timestamp.localeCompare(leaf.timestamp) > 0`).
2. **Walk to root**: from that leaf, follow `parentUuid` back to `null`, reversing to get
   root→leaf order.
3. **Cycle detection**: both the leaf-search walk and the root walk maintain a `seen` Set
   and `throw new Error("Cycle detected in transcript parentUuid chain.")` if a UUID repeats
   — this is a real safety net against corrupt/self-referential transcripts, and is the kind
   of edge case a naive port could miss.
4. **Parallel/sibling recovery** (`recoverParallelToolRows`, `transcript.ts:208-258`): Claude
   Code can emit multiple `assistant` rows sharing the same `message.id` (parallel/streamed
   tool-call chunks) where only one is on the direct parent-chain walk. For each
   on-chain assistant row, this function finds (a) sibling assistant rows with the same
   `message.id` not yet in `seen`, and (b) `tool_result` rows whose `parentUuid` is either
   the anchor assistant or one of those siblings, not yet in `seen`. Both groups are sorted
   by timestamp and spliced in immediately after the anchor row. This explicitly "mirrors
   Claude Code's transcript loader behavior so compaction does not silently drop sibling
   streamed tool calls" (comment intent per `docs/ClaudeCode.md`) — an unstated but
   load-bearing requirement: a naive single-parent-chain walk **loses parallel tool-call
   results**.
5. If no leaf is found (empty/degenerate transcript), returns `[]` — this then causes
   `createPlan` in `compact.ts` to throw `"Transcript does not contain compactable
   conversation rows."` (see §8).

### 2b. Recompaction-boundary slicing (`readActiveTranscriptRows`, `transcript.ts:29-37`)

Before chain-walking, it finds the **last** row where `isCompactBoundary` is true
(`row.magicCompact?.boundary === true`, `transcript.ts:264-267`) via `findLastIndex`, and
slices to only rows *after* that boundary. Only that post-boundary slice is chain-walked.
This is distinct from the row-level `magicCompact.summary === true` marker used inside
`createPlan` (§3) — there are **two separate markers**: `boundary` (on the single synthetic
`system`-row... actually the boundary row here is `type: "user", isMeta: true` per
`compact.ts:164-179`, NOT `type: "system"` — see the discrepancy note in §6) and `summary`
(on each synthesized per-turn assistant summary row).

**IMPORTANT DISCREPANCY between docs and code**: `docs/ClaudeCode.md` states "The compacted
tail starts with a Claude-native compact boundary row: `type: "system"`, `subtype:
"compact_boundary"`" but the actual code in `compact.ts:164-179` constructs the boundary row
with `type: "user"`, `isMeta: true`, a `message` with the `POST_COMPACTION_NOTICE` text, and
`magicCompact: { boundary: true }` — **not** `type: "system"` / `subtype:
"compact_boundary"`. The plan/requirements doc's assumption (`system`/`compact_boundary`
row) matches the *docs*, not the *actual shipped TypeScript*. This must be flagged to the
user/plan phase: either the docs are stale, or there's a newer/different code path not
found. A thorough plan should decide whether consolette should replicate the doc's stated
`system`/`compact_boundary` semantics (matching literal Claude Code transcript format
expectations) or the actual TS behavior (`user`+`isMeta`+`magicCompact.boundary`). This is
the single most important finding for the plan phase's "exact recompaction-boundary
detection algorithm" open question.

### 2c. Turn grouping (`buildAssistantTurns`, `transcript.ts:114-144`)

- A "turn" starts at one or more consecutive **human** user rows (`isHumanUserRow`:
  `type === "user"`, not a tool-result row per `isToolResultRow` — checks
  `message.content` array for any block with `type === "tool_result"` — and `isMeta !==
  true`).
- Once an assistant row or tool-result row is seen, `assistantStarted = true`; a
  **subsequent** human user row starts a *new* turn. Consecutive human user rows before any
  assistant activity are grouped into the *same* turn (e.g., multi-part user messages).
- Tool-result rows and assistant rows accumulate onto the current turn.
- Rows appearing before any user row are dropped (if `!currentTurn`, `continue`).
- **Filter**: only turns containing at least one `assistant` row or tool-result row are
  kept — pure user-only turns (e.g. a final unanswered prompt) are excluded from
  `buildAssistantTurns`'s output, which matters for compaction planning (a trailing
  human-only turn won't itself be "eligible" — it's used only as `nextTurn` context in the
  summarization prompt, see `compact.ts:43`).

## 3. Recompaction-boundary detection (within a single compaction pass) — `compact.ts:59-83`

`createPlan`:
- Detect prior compaction via `isMagicCompactSummaryRow` (checks `row.magicCompact?.summary
  === true`) on any row within a turn's `rows`.
- `compactionStartIndex = turns.findLastIndex(...) + 1` — i.e., start immediately after the
  **last** turn that contains a previously-summarized row. This means: everything up to and
  including the last already-summarized turn is copied as `prefixTurns` (verbatim, not
  re-summarized); only turns *after* that are eligible for (re-)summarization.
- `keepTurns` (`N` from `/magic-compact [N]`): `compactionEndIndex = keepTurns <= 0 ?
  turns.length : max(compactionStartIndex, turns.length - keepTurns)`. This means `N` most
  recent turns are preserved verbatim (`preservedTurns`) — but never fewer than
  `compactionStartIndex` (i.e., `N` cannot "reach back" into already-summarized territory).
  The middle band `[compactionStartIndex, compactionEndIndex)` is `summarizedTurns`.
- If `summarizedTurns.length === 0` (e.g., everything is already summarized or `N` covers
  the whole remaining range), `compactTranscript` returns `false` and the caller (hook.ts)
  deletes the just-allocated destination file and reports a no-op message — **this is the
  "stop early and delete destination" behavior** from `docs/ClaudeCode.md` step 5.
- **Edge case**: if there are truly zero user/assistant rows anywhere in the active chain,
  `createPlan`'s `rows.find(...)` fails and it `throw`s `"Transcript does not contain
  compactable conversation rows."` — distinct from the "nothing new to compact" no-op path.

## 4. Tool-I/O pruning + omission cache — `prune.ts`, `omission.ts`

Pruning (`pruneTranscriptRow`, `prune.ts:22-48`) is applied **only** to tool rows inside
`summarizedTurns` (never to `prefixTurns` or `preservedTurns` — those are copied byte-for-byte
except for UUID/session/timestamp/parent rewriting), and only after
`keepOnlyToolBlocks` has stripped non-tool content blocks from that row's `message.content`
(`compact.ts:370-385`) — so summarized turns retain *only* `tool_use`/`tool_result` blocks
plus the one new synthetic summary-text assistant row.

Rules (exact, from code, more granular than `docs/ClaudeCode.md`'s prose summary):
- **Completed-only**: `completedToolUseIds` = tool_use IDs referenced by a **non-error**
  (`is_error !== true`) `tool_result` anywhere within the `summarizedTurns` set
  (`compact.ts:302-326`). Pruning of *inputs* is skipped entirely for tool_use blocks whose
  ID isn't in this set (i.e., calls that errored or that never got a result in the
  summarized range are left untouched — `prune.ts:58-60`). Pruning of *outputs* checks
  `is_error === true` directly on the block and skips it (`prune.ts:151-153`) — so error
  outputs are always preserved verbatim regardless of the completed-set membership.
- **Tool name resolution**: `toolNamesById` is built once from all `tool_use` blocks across
  `summarizedTurns` (`compact.ts:328-352`) and also incrementally updated inside
  `pruneToolInput` itself (`prune.ts:57`) as it walks each `tool_use` block, since a
  `tool_result` may need the name looked up from an *earlier* processed `tool_use` block
  within the same pass — order-dependent but both content is guaranteed sourced from the
  same `summarizedTurns` set so this always resolves.
- **Per-tool INPUT rules** (`pruneToolInput`, `prune.ts:50-148`) — only applied if the
  tool_use ID is "completed" and `input` is a record and tool isn't `AskUserQuestion`
  (always preserved, input+output, unconditionally — `prune.ts:63`, `prune.ts:158`):
  - `Bash`: `truncateBashCommand` — if `command` string length > 1024, keep first 512 chars
    + `"\n[REST OF COMMAND TRUNCATED]"`, cache full command, add
    `command_omission_notice` field with content ID (`prune.ts:263-280`). Threshold is a
    flat 1024-char length check, **not** the word/char `DEFAULT_LIMIT` used elsewhere.
  - `Write`: omit `content` field entirely if it exceeds `DEFAULT_LIMIT` (128 words / 1024
    chars) via `omitField` → replaces with literal string `"[Omitted]"` plus a sibling
    `<field>_omission_notice` field holding an XML-ish notice with the content ID.
  - `Edit`: omits **combined** `old_string`+`new_string` (joined with `\n`) as a single
    cached blob if the combination exceeds the limit; both fields individually get replaced
    with `"[Omitted]"`, and one shared `old_string_new_string_omission_notice` field is
    added.
  - `NotebookEdit`: omits `new_source` field if it exceeds limit.
  - `Agent`: omits `prompt` field if it exceeds limit.
  - `Workflow`: omits `script` field.
  - `SendMessage`: omits `message` field.
  - `ReportFindings`: omits `findings` field.
  - Any other tool name: input left untouched (no case, falls through).
- **Per-tool OUTPUT rules** (`pruneToolOutput`, `prune.ts:150-215`) — applied only if
  `is_error !== true`, tool name is resolvable via `toolNamesById`, and not
  `AskUserQuestion`:
  - `Skill`: output is **discarded without caching** — `block.content` is unconditionally
    replaced with a fixed string `"Skill output omitted due to compaction operation. If
    necessary, recall the skill."` — no content ID, no retrieval possible. This is an
    explicit, intentional exception to the "everything cached" pattern.
  - `Read`: output is **always** cached+omitted regardless of size (`prune.ts:173-185`) — no
    `exceeds()` size check gate, unlike the default path. Same for `NotebookEdit` output
    (`prune.ts:187-199`).
  - `Agent` / `TaskOutput`: use `AGENT_OUTPUT_LIMIT` (512 words / 4096 chars) instead of
    `DEFAULT_LIMIT` (128 words / 1024 chars) before omitting.
  - All other tools: `DEFAULT_LIMIT` (128 words / 1024 chars); only omitted if
    `exceeds()` is true (word count OR char count over threshold — an OR, not AND).
  - `stringifyContent` (`prune.ts:282-291`): non-string content (arrays/objects) is
    `JSON.stringify`'d before length/word checks — handles Claude Code's tool_result
    `content` sometimes being an array of blocks rather than a plain string.
- Every omission call goes through `allocateOmission` (`omission.ts:40-49`), which assigns
  a monotonically increasing `nextId` **per omission cache** (i.e., per destination
  session), formats the ID as `<last-12-chars-of-destination-sessionId>:omitted-NNN`
  (zero-padded to 3 digits — note: NOT reset per compaction pass across multiple
  compactions of the same lineage, since a fresh destination session ID is created each
  time, so cache IDs never collide across compactions of the same original transcript).

### Omission cache persistence (`omission.ts`)
- File: `~/.claude/magic-compact/{destinationSessionId}.json`, versioned
  (`version: 1`), format `{version, nextId, entries: {contentId: {content}}}`.
- Loaded once at the start of `buildCompactedRows` (`compact.ts:154`), mutated in place
  during pruning, and saved once at the end (`compact.ts:259`) — i.e., **not** streamed
  incrementally; if the process crashes mid-compaction, the destination transcript is never
  written (or partially written) and the cache file for that new session is simply never
  created — no orphaned cache risk, but also no partial-progress recovery.
- Cache validation on load (`isOmissionCache`, `omission.ts:127-139`) is defensive: if the
  file exists but doesn't match the expected shape (e.g. corrupted/foreign JSON), it's
  silently treated as if it didn't exist (`createEmptyCache()`), **losing any existing
  entries** rather than erroring — a real risk of orphaning previously-cached content IDs
  if the destination-session cache file gets corrupted after a first compaction.

### Retrieval (`read_omitted_content` MCP tool, §5 below + `omission.ts:51-66,94-109`)
- Because Claude Code MCP tool invocations don't carry the live session ID
  (`docs/ClaudeCode.md`: "Claude Code MCP tool calls do not provide session IDs"),
  `readOmittedContent` parses the 12-char session-suffix out of the `contentId` itself
  (`content.split(":", 1)`, must be exactly 12 chars), then scans **all files** in
  `~/.claude/magic-compact/` via `findSessionIdBySuffix` (`omission.ts:94-109`) for one
  whose filename ends with `<suffix>.json`, and loads that cache. This is an unstated but
  important design constraint: **the cache directory can accumulate one file per
  compaction ever run** (no eviction/GC logic found anywhere in this codebase) — cache
  files are never deleted. A Rust port should decide explicitly whether to add
  retention/cleanup, since magic-compact does not.
- If suffix parse fails or no matching file/entry exists, returns `null` → MCP tool replies
  with a human-readable "No omitted content found for Content ID: ..." message
  (`mcp.ts:51-52`) rather than erroring the MCP call.

## 5. `claude -p --resume` subprocess invocation — `compact.ts:85-125`

- Before summarizing, the **entire source transcript file is copied** to a brand-new
  session file (`copyTranscriptToNewSession` → `createTranscriptSession` +
  `copyFile(..., COPYFILE_EXCL)`, `transcript.ts:39-73`) — an "analysis" transcript. Session
  ID allocation retries up to 5 times generating a fresh UUID and checking
  `access(path, F_OK)` fails with ENOENT before use (avoids racing an existing file, though
  the retry loop does not use a lock — pure optimistic collision avoidance via
  `COPYFILE_EXCL`, which itself will throw on a very unlikely race, propagating up as a
  hard failure of the whole compaction).
- Subprocess command: `["claude", "-p", "--resume", <analysisTranscriptPath>, "--settings",
  JSON.stringify({disableAllHooks: true}), (optional) "--model", <model>, <prompt>]` via
  `Bun.spawn` with piped stdout/stderr.
- `--model` is only passed if `latestAssistantModel(rows)` (`compact.ts:127-143`) finds a
  real (non-empty, not `"<synthetic>"`) `message.model` string on the **most recent**
  assistant row scanning backward through the **active chain rows** (not just
  summarizedTurns) — if none found, omits `--model` entirely (lets `claude -p` pick its own
  default).
- **Failure handling**: waits for `exited` (exit code) concurrently with fully draining
  stdout/stderr as text. Non-zero exit → `throw new Error("Summary generation failed:
  ${stderr.trim()}")`. No retry, no timeout wrapper at this layer (the *hook's* declared
  150s timeout in `hooks.json` is the only ceiling, enforced by Claude Code itself killing
  the hook process, not by this code).
- **Cleanup**: analysis transcript is deleted in a `finally` block
  (`unlink(...).catch(() => undefined)` — deletion failures are swallowed silently) whether
  summarization succeeded or threw.
- **Response parsing** (`parseSummaries`, `compact.ts:518-554`): looks for **first**
  `<summary>` and **last** `</summary>` in stdout (tolerant of preamble/trailing chatter
  around the block); if either tag is missing or in the wrong order, throws. Inside, it
  regex-matches all `<user>...</user>` / `<assistant>...</assistant>` segments in document
  order, then pairs each `<user>` immediately followed by `<assistant>` — explicitly
  documented as robust to a common model misbehavior: echoing one extra trailing
  `<user>`/`<assistant>` pair for the "next turn" anchor that should have no summary
  (`compact.ts:528-531` comment). Stops once `expectedCount` pairs are collected; if fewer
  than expected pairs were found, throws `"Expected N summaries, received M user/assistant
  pairs."` — a strict count-match requirement, no partial-success/best-effort mode.
- **Prompt construction** (`buildCompactionPrompt`/`buildXmlTemplate`,
  `compact.ts:433-516`): explicit system-style prompt instructing the model to emit XML
  only, one `<assistant>` summary per turn, with the turn's user message reduced to only
  its **first line, truncated to 300 chars** (`getUserPromptText`, `compact.ts:507-516`) —
  this truncated echo is only used to anchor/align the model's output; the *actual* full
  user row content is preserved untouched in the final rebuilt transcript. An optional
  trailing `nextTurn` (first preserved turn, if any) is included as an unsummarized anchor
  so the model knows where summarization should stop, with an explicit instruction not to
  summarize it.

## 6. Writing the destination transcript / boundary row — `compact.ts:145-261`

Order of rows written to the destination file, all under a single fresh `sessionId`:
1. **Boundary row** (see discrepancy note above in §2b) — `type: "user"`, `isMeta: true`,
   `parentUuid: null`, synthetic `message` with `POST_COMPACTION_NOTICE` text (a hardcoded
   `<post-compaction-notice>` block instructing the model to call `read_omitted_content` if
   needed and warning it may need to reread files), `logicalParentUuid` pointing at the last
   original source row's UUID (an extra, non-standard field — presumably informational only
   since it isn't a real Claude Code transcript field), and `magicCompact: {boundary:
   true}`.
2. **`prefixTurns`** copied verbatim (only UUID/parentUuid/sessionId/timestamp rewritten via
   `copyRow`/`copyTurnRows`), preserving already-completed-summary turns from prior
   compactions.
3. **`summarizedTurns`**, each turn rebuilt as: original user row(s) verbatim (copied with
   new IDs) → one synthetic assistant summary row (`createAssistantSummaryRow`,
   `compact.ts:396-418`: clones the turn's *first* assistant row as a template — preserving
   whatever other message fields it had — but overwrites `content` to a single `text` block
   with the summary, sets `stop_reason: "end_turn"`, `stop_sequence: null`, tags
   `magicCompact: {summary: true}`) → each tool row from the original turn that
   `isToolRow` (has a `tool_use` or `tool_result` block), pruned per §4, in original order,
   parent-chained onto whichever copied row is their rewritten parent (falling back to
   the running `parentUuid` if the original parent wasn't itself copied — this handles tool
   rows whose original parent was, e.g., a text-only assistant chunk not otherwise copied).
4. **`preservedTurns`** (the most recent `N` turns) copied verbatim, same as `prefixTurns`.
5. All rows in a given call share **one single `timestamp`** (`new Date().toISOString()`
   computed once at the top of `buildCompactedRows`) — the entire destination transcript's
   non-preserved-metadata rows are stamped with the same instant, not the original
   per-row timestamps. This is a real, unstated behavior a naive port might not replicate
   (a Rust port needs to explicitly decide: keep original per-row timestamps for
   copied/prefix/preserved rows, or also flatten them — the TS code flattens **all**
   rewritten rows, including verbatim-copied prefix/preserved turns, not just the new
   synthetic summary/boundary rows).
6. **Preserved session metadata entries** (`readPreservedMetadataEntries`,
   `transcript.ts:75-91`) — non-transcript-row top-level entries whose `type` is in a fixed
   allow-list (`custom-title`, `ai-title`, `last-prompt`, `tag`, `agent-name`,
   `agent-color`, `agent-setting`, `mode`, `worktree-state`, `pr-link`, `task-summary`,
   `permission-mode`) are copied with `sessionId` rewritten to the destination ID (only if
   it matched the *source* session ID exactly) and prepended **before** all the compacted
   rows in the final file (`compact.ts:52-55`: `[...metadataEntries, ...compactedRows]`).
   Anything with an unrecognized `type` is silently dropped (not preserved, not erroring).
- Final write is a single `Bun.write` of the whole joined-JSONL string
  (`writeTranscriptEntries`, `transcript.ts:93-101`) — not streamed/appended — so the
  destination file only exists in complete form; no risk of a reader seeing a half-written
  file mid-compaction (atomicity depends on `Bun.write`'s underlying write semantics, which
  is not guaranteed atomic across all platforms — the TS code does not use a temp-file+rename
  pattern here, unlike `copyTranscriptToNewSession`'s `COPYFILE_EXCL` usage elsewhere).
- **Original source transcript is never modified** — this is explicit and load-bearing:
  `docs/ClaudeCode.md` states "The original active session is not modified" and "No
  separate backup session is created because the original session remains untouched" — the
  Rust port must guarantee it opens the source file read-only / never writes to it.

## 7. UserPromptSubmit hook wiring — `hooks.json`, `hook.ts`, `command.ts`

- `hooks/hooks.json` registers a `UserPromptSubmit` hook with matcher regex
  `^/(?:claude-magic-compact:)?magic-compact(?:\s+.*)?$` (matches both the raw slash
  command and Claude Code's namespaced-expansion form), invoking `bun
  "${CLAUDE_PLUGIN_ROOT}/src/hook.ts"` with a **150-second timeout** (explicitly justified
  in `docs/ClaudeCode.md` as needed because compaction includes subprocess summarization,
  vs. Claude Code's default prompt-submit timeout being too short) and a `statusMessage`
  shown to the user while running.
- `hook.ts` reads the entire hook JSON payload from stdin, delegates parsing to
  `parseHookInput` (`command.ts:11-32`, requires `session_id`, `transcript_path`,
  `hook_event_name === "UserPromptSubmit"`, `prompt` all present with correct types — throws
  a generic error otherwise) then `parseMagicCompactCommand` (`command.ts:34-44`): a second,
  **stricter** regex (`^\/(?:claude-magic-compact:)?magic-compact(?:\s+(\d+))?\s*$`,
  requiring `N` to be digits only if present) that returns `null` if the prompt doesn't
  match at all (meaning: hook silently no-ops via `suppressOutput: true`, letting the prompt
  fall through normally — this handles the matcher regex being intentionally looser
  (`.*` after the command) than what's actually valid, so `/magic-compact abc` matches the
  *hooks.json* matcher (invoking the hook) but then fails the *stricter* `command.ts` regex
  and throws `"Usage: /magic-compact [N: positive integer]"` — a user-facing usage error
  surfaced via the hook's `stopReason` with `continue: false` (so Claude Code doesn't hand
  the raw invalid command to the model as a normal prompt)).
- On success: allocates a destination session ID+path (again via `createTranscriptSession`,
  retried-unique-UUID logic), runs `compactTranscript`; if it returns `false` (nothing to
  compact), deletes the just-created (never-written) destination file and reports a no-op
  `stopReason`; if it returns `true`, best-effort re-labels the **original** session's title
  by appending a `custom-title` entry (`appendCustomTitle`, `transcript.ts:367-380`) prefixed
  `"[UNCOMPACTED] "` (avoiding double-prefixing if already so labeled) — wrapped in its own
  try/catch so a labeling failure doesn't fail the overall compaction (`hook.ts:42-52`
  comment: "Best-effort labeling; compaction already succeeded.") — then reports the
  `/resume <new-session-id>` success message.
- **All top-level errors** (JSON parse failure of stdin, missing fields, transcript read
  errors, cycle detection, subprocess failure, XML parse failure, cache errors) are caught
  by one outer `try/catch` in `main()` and reported uniformly as `Magic Compact failed:
  ${message}` with `continue: false` — there is no differentiated handling/retry per failure
  class, and no partial cleanup of an already-created (possibly partially-written, though
  practically it's write-once at the end so this is moot) destination transcript file on
  failure **after** `createTranscriptSession` succeeded but before/during
  `compactTranscript` — i.e., a failed compaction attempt (e.g. subprocess dies mid-run)
  can leak an allocated-but-empty destination session file that's never cleaned up (unlike
  the explicit `unlink` in the "nothing to compact" no-op path). This is a real
  resource-leak edge case a Rust port should close (e.g., via cleanup-on-error or a
  temp-path + atomic rename step).
- `.mcp.json` registers the sibling `read_omitted_content` MCP server (`bun
  "${CLAUDE_PLUGIN_ROOT}/src/mcp.ts"`) as a stdio MCP server, separate process from the hook.
- `skills/magic-compact/SKILL.md` is a "shim" skill so `/magic-compact` still resolves to
  *something* if the plugin is installed but the hook fails to intercept for some reason
  (e.g. plugin disabled) — it just tells the model to alert the user to check the plugin
  install/enable state, rather than doing anything itself.

## Key unstated/inferred requirements for the Rust port

1. **Read-only source guarantee**: never open the original `.jsonl` for writing; all output
   goes to a freshly allocated sibling session file.
2. **Idempotent-safe re-compaction**: must track a per-row "already summarized" marker
   (equivalent of `magicCompact.summary === true`) so repeated compaction runs on the same
   lineage don't re-summarize already-summarized turns, and a separate "boundary" marker so
   `readActiveTranscriptRows`-equivalent logic slices to only the latest post-boundary
   range — these are two independent concerns, not one.
3. **Non-negotiable format discrepancy to resolve with the user/plan**: whether to implement
   the boundary row as `docs/ClaudeCode.md` describes (`type: "system"`, `subtype:
   "compact_boundary"`) or as the actual shipped code does (`type: "user"`, `isMeta: true`,
   `magicCompact.boundary: true`). Since the user's stated success metric explicitly says
   "a native `system`/`compact_boundary` row," and Claude Code's own transcript loader is
   external/opaque, the plan phase must decide based on what a real Claude Code `/resume`
   actually recognizes — **this needs verification against live Claude Code behavior**, not
   just the magic-compact source, since magic-compact's own doc and code disagree with each
   other.
4. **Parallel tool-call recovery is not optional** — omitting it silently drops legitimate
   sibling tool-call results from parallel/streamed assistant turns; must be ported
   faithfully (§2a step 4).
5. **Cycle detection is required**, not just nice-to-have — corrupt/self-referential
   `parentUuid` chains must fail loudly rather than infinite-loop.
6. **Cache directory has no GC/eviction** — magic-compact leaves this as an unbounded-growth
   design; the plan phase should decide whether to intentionally match this (simplicity) or
   diverge and add retention, calling out the divergence explicitly per the constraints doc.
7. **Destination-file leak on mid-compaction failure** is an existing bug/gap in
   magic-compact (no cleanup of the allocated destination path if `compactTranscript` throws
   after file allocation but the "nothing to compact" `false`-return path is the only one
   cleaned up) — the Rust port should not blindly replicate this; it's a place to actually
   improve on the original with proper error-path cleanup (e.g. write to a temp path and
   rename into place only on success).
8. **Threshold/config surface is currently hardcoded**, not user-configurable: `DEFAULT_LIMIT`
   (128 words/1024 chars), `AGENT_OUTPUT_LIMIT` (512 words/4096 chars), Bash command 1024-char
   cutoff, hook timeout 150s. If consolette wants configurability (not required by magic-compact
   parity, but worth flagging as a design choice), these are the exact knobs.
9. **All non-transcript, non-allow-listed top-level JSONL entry types are silently dropped**
   when writing the destination — any custom metadata a user's setup relies on outside the
   fixed `PRESERVED_METADATA_TYPES` list will not survive compaction. Worth flagging as a
   known/accepted lossy behavior to replicate or intentionally fix.
10. **Sidechains**: `copySessionFields` explicitly forces `isSidechain = false` on the
    boundary-adjacent metadata-carrying row (`transcript.ts` — actually `compact.ts:428`
    inside `copySessionFields`, used only for the boundary row's base-row template), but
    there is **no other explicit sidechain/subagent-transcript handling anywhere** in
    `transcript.ts`, `compact.ts`, or `prune.ts` — `isSidechain` is part of the
    `TranscriptRow` type but never filtered on when reading/chaining/pruning. If Claude Code
    sidechain (subagent) rows appear interleaved in the same file, magic-compact's chain
    walk would treat them like any other row (matching on `uuid`/`parentUuid`/`type` alone).
    This is a gap worth flagging to the plan phase: it's unclear whether Claude Code's real
    transcripts interleave sidechains into the main file at all, or use separate files —
    this needs verification (not found in this repo) before the plan can claim parity here.
