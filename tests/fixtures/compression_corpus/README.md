# Compression corpus (issue #6 traffic analysis)

Real tool-output samples pulled from local Claude Code transcripts
(`~/.claude/projects/**/*.jsonl`), used to evaluate whether the claw-compactor
stages in [#6](https://github.com/tstapler/consolette/issues/6) (SearchCrunch,
Cortex, Photon, Nexus, TokenOpt, Abbrev) are worth building against real
traffic instead of a hypothetical one.

## Provenance and scope

- Scanned 492 transcript files across non-worktree project directories.
- Excluded entirely: personal-life project dirs (home renovation, wedding
  planning, personal wiki, packing lists) and any work/employer-scoped
  project dirs. Nothing from those was read for this analysis.
- Only `tool_result` content blocks were sampled (compiler/test/lint/git
  output) — never user or assistant prose, and never raw image bytes.
- Every candidate was checked against a secret-pattern filter (API key/token/
  private-key shapes) before being considered; any match was dropped, not
  redacted-and-kept.
- Kept: absolute paths under `/home/tstapler` rewritten to `/home/user`,
  the account username replaced with `user`, real repo names replaced with
  `example`/`myproject`/`otherapp`, commit author identity replaced. Content
  and structure (line-for-line tool output) are otherwise verbatim.

## Aggregate stats (informs the #6 verdict)

| Signal | Result |
|---|---|
| Files / lines scanned | 492 files, 96,458 transcript lines |
| `image` content blocks | 15 total (12 PNG, 3 JPEG), ~4.8MB base64 combined |
| `tool_result` blocks | 10,363 total; 68.6% under 1KB, 29.1% 1-10KB, 2.3% 10-100KB, none over 100KB |
| Non-consecutive duplicate content (candidate for smarter dedup) | ~70 blocks (~2% of the 2-30KB population checked) |

## What this says about #6's six stages

- **Photon** (base64 image downsize) — 15 image blocks in 96k lines of real
  traffic. Not worth a new image-processing dependency for.
- **SearchCrunch/Cortex** (content-type router) — still not warranted; see
  `dedup/` below for what content-type detection would actually be
  discriminating between, and note the existing pipeline's self-gating regex
  stages already cover most of it without a separate router.
- **Nexus / TokenOpt / Abbrev** — `dedup/dedup_git_log.txt` is the one sample
  that's genuine new evidence: a `git show`-style multi-file commit repeats
  its full commit-message header once per file. That's a narrow,
  deterministic win (hoist the repeated header) — not a case for an ML
  classifier or lossy filler-trimming.

## Layout

- `dedup/` — tool output containing non-consecutive repeated content (test
  runners, thread dumps, CI summaries, stack traces, git log) — the shape
  SearchCrunch/Nexus would operate on.
- `markdown/` — markdown/config/script tool output — the shape TokenOpt/
  Abbrev would operate on.
