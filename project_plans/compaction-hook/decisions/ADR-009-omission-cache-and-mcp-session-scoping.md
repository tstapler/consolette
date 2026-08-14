# ADR-009: `rusqlite` omission cache with mandatory session-scoped MCP retrieval

**Status**: Accepted
**Date**: 2026-08-13
**Relates to**: requirements.md open question 3; the HIGHEST PRIORITY SECURITY RISK flagged in `research/pitfalls.md`

## Context

magic-compact prunes bulky completed tool I/O out of the transcript it writes
to the destination session, replacing it with an omission-cache reference
(`<12-char-suffix>:omitted-NNN`) that a `read_omitted_content` MCP tool can
later resolve back to the full content. Its cache file lives at
`~/.claude/magic-compact/{destinationSessionId}.json`.

`research/pitfalls.md` found a real vulnerability in magic-compact's retrieval
path: `findSessionIdBySuffix` (`omission.ts:94-109`) resolves the *session*
half of a content ID by **suffix-matching across all cached sessions on
disk**, not by binding to the MCP-calling session's own identity. Any Claude
Code session can retrieve any other session's pruned tool output by
guessing or enumerating 12-character suffixes. The task brief for this plan
states explicitly that fixing this is "a mandatory design requirement, not
optional hardening" — not something the Rust port may carry forward.

Separately, the cache must be a **long-lived** store (content must remain
retrievable across `/resume` sessions, potentially days later) — ruling out
consolette's existing `moka`-based `RewindStore` (10-minute TTL, in-memory,
built for the live proxy request path) and `cmdcrush`'s `<hash>.orig`
sidecar-file pattern (no expiry policy, no session binding at all).

## Decision

**Backend**: a single `rusqlite` (already pinned, `bundled` feature) database
at `~/.claude/consolette/omission-cache.sqlite`, one row per omitted content
blob:

```sql
CREATE TABLE IF NOT EXISTS omitted_content (
    session_id   TEXT NOT NULL,
    content_id   TEXT NOT NULL,   -- e.g. "omitted-003", unique only within a session
    content      TEXT NOT NULL,
    tool_name    TEXT NOT NULL,
    created_at   TEXT NOT NULL,   -- RFC3339, via chrono
    PRIMARY KEY (session_id, content_id)
);
```

**Every read is scoped by `session_id` in the `WHERE` clause — never by
`content_id` alone.** This is the direct fix for the
`findSessionIdBySuffix` vulnerability: there is no suffix-matching, no
cross-session scan, and no code path that can resolve a `content_id` without
an accompanying `session_id` that the caller does not control.

**MCP exposure, and the honest scope of what this fixes**: a new native MCP
tool `read_omitted_content` registered in consolette's own first-party stdio
MCP server (`main.rs`'s `mcp()`, currently a stub — see ADR-008), **not** in
`src/mcp_gateway.rs` (which is an outbound proxy to *external* MCP servers
and has no notion of this cache).

This design eliminates magic-compact's specific *suffix-scan enumeration*
vulnerability (`findSessionIdBySuffix` scanning all cached sessions on disk
to resolve a bare content ID) — there is no code path that resolves a
`content_id` without an accompanying `session_id`. **It does not eliminate
the underlying trust-model weakness**: `session_id` is still a plain,
client-supplied string with no independent verification of caller identity,
so an MCP caller that already knows or guesses another session's UUID (and
`content_id`s are low-entropy sequential strings — `omitted-001`,
`omitted-002` — easier to guess once a `session_id` is known than
magic-compact's own 12-char suffix) can still retrieve that session's cached
content by supplying it directly. This is a narrower attack surface than
magic-compact's bug (no enumeration across all sessions; correctness of
well-behaved clients is preserved), but it is **not** "session-scoping" in
the sense of the server independently authenticating which session is
calling — it is closer to "the query can't succeed without also being told
the right answer to `WHERE session_id = ?`."

A short spike (Task 5.1.0, see plan.md Epic 5.1) must check, before this
tool ships, whether Claude Code launches a native MCP server **per session**
(one stdio process per session, analogous to the `session_id`-bearing
`UserPromptSubmit` hook payload) or as a single shared/global process
(the topology that caused magic-compact's actual bug per
`research/pitfalls.md`). If per-session, consolette can bind `session_id` at
process-spawn time from the hook invocation's own environment/argv rather
than trusting a per-call MCP argument — a real authentication boundary, not
a query-shape mitigation — and that becomes the v1 design instead of the
client-supplied-argument approach. If the spike confirms a shared/global
process (matching magic-compact's actual topology), v1 ships with the
client-supplied `session_id` design described above, and this is recorded
here as an **accepted, explicit residual risk** for v1 (not "mandatory
design requirement, fully closed") pending real per-connection session
binding support in consolette's MCP transport — tracked as the documented
follow-up already listed in plan.md.

**Task 5.1.0 spike finding (2026-08-13)**: confirmed, high confidence, that
Claude Code spawns one stdio MCP subprocess **per session**, not a
shared/global process. Evidence: direct `ps -ef` inspection on this machine
showed 8 concurrent `claude` CLI processes each with their own independent
MCP child processes (including a real `magic-compact` MCP server instance
scoped to exactly one session); corroborated by
[anthropics/claude-code#28860](https://github.com/anthropics/claude-code/issues/28860)
(a feature request for cross-session MCP server *sharing*, which explicitly
describes today's N-sessions × M-servers spawn behavior) and
[#29688](https://github.com/anthropics/claude-code/issues/29688).

However, whether Claude Code sets a reliable, undocumented env var (e.g.
`CLAUDE_CODE_SESSION_ID`) in the spawned server's environment — the
mechanism that would let consolette actually *read* the session ID at
spawn time rather than merely benefit from per-process isolation — is
**unverified**: `ps eww` against the running subprocesses on this machine
returned no environment (sandbox permission limits, not proof of absence),
and no primary documentation was found confirming the variable's name or
existence. Per-session process isolation without a confirmed way to read
the session ID inside that process does not, by itself, let
`read_omitted_content` drop the `session_id` argument.

**Decision for v1**: ship the client-supplied `session_id` design as
originally specified (`call_tool("read_omitted_content", {session_id,
content_id})`) — the confirmed per-session topology is documented here as
support for a **future** enhancement (bind `session_id` at spawn time,
removing it from the tool's argument surface entirely) once the env var
mechanism is confirmed, but is not built into v1 given that confirmation
gap. The residual risk described above (client-supplied `session_id` is
unauthenticated) remains accepted for v1, unchanged by this finding.

**Growth**: no eviction in v1 (matches magic-compact's own unbounded-growth
behavior, called out in `research/pitfalls.md` as a real but lower-priority
risk); a `consolette compact-session --gc` follow-up is noted in the plan's
"documented follow-ups" but not built in this pass. A failed/aborted
`compact_session` run can leave orphaned cache rows for a destination
`session_id` whose transcript was never written (the atomic writer prevents
a partial *transcript* but does not roll back cache inserts already
committed during pruning); this is accepted as part of the same
already-accepted unbounded-growth tradeoff, not a separate open item.

**Hardening applied to the store itself**: the sqlite file is created with
`0600` permissions and its parent directory with `0700`
(`std::os::unix::fs::PermissionsExt`, set immediately after creation) —
`research/pitfalls.md` flags the absence of this in magic-compact's own
cache file as a real risk given the cache stores "potentially the most
sensitive raw content" (secrets in Bash output, file contents). The
connection is opened with `PRAGMA journal_mode=WAL` and a `busy_timeout` of
5000ms, and `insert`'s content-id numbering (`SELECT COUNT(*) ...` then
`INSERT`) is wrapped in a single `rusqlite` transaction, so concurrent
inserts for the same `session_id` (parallel tool-row pruning, or a
double-fired hook) cannot race on the count or fail with `SQLITE_BUSY`.

## Alternatives Considered

| Option | Rejected because |
|---|---|
| Reuse `compression::rewind::RewindStore` (moka, 10-min TTL) | Wrong lifetime — content must survive until a later `/resume`, which can be arbitrarily far in the future. |
| Per-session JSON file (`~/.claude/consolette/{session_id}.json`, matching magic-compact) | Reproduces the exact shape that enabled suffix-based cross-session enumeration; a single scoped table with no session-agnostic lookup path is a stronger structural guarantee than "remember not to add a suffix-scan helper." |
| `cmdcrush`-style `<hash>.orig` sidecar files | No session binding at all — any process with filesystem access can read any hash's content; doesn't even meet magic-compact's (broken) bar. |

## Consequences

- New `rusqlite` table, migration-free (single `CREATE TABLE IF NOT EXISTS`
  at startup — no schema versioning needed for a v1 single-table cache).
- `read_omitted_content`'s contract is: given `(session_id, content_id)`,
  return content or "not found" — never "which session is this from."
- Epic/story acceptance criteria (see `implementation/plan.md`) must include
  a test asserting that a `content_id` valid for session A returns "not
  found" when queried with session B's `session_id`.
