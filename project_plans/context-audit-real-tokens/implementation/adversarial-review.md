# Adversarial Review: context-audit-real-tokens

**Date**: 2026-09-04
**Verdict**: CLEAN

## Blockers
- [x] **RESOLVED** — Plan targets a repo this SDD project doesn't live in. `plan.md` now
  has an "Implementation Location" section immediately after the header (lines 13-23)
  stating explicitly: "All file paths in this plan are relative to `~/dotfiles`... NOT
  the `tstapler/consolette` repo this SDD planning session ran in," and that
  "Phase 5 (`/sdd:5-implement`) for this plan must be run from a **fresh session with
  `~/dotfiles` as the working directory/repo**... not from this consolette worktree."
  This is exactly the re-homing instruction the original blocker asked for, placed
  where an implementer can't miss it. Every per-task "Files" line also now carries a
  "(relative to `~/dotfiles`...)" reminder (e.g. plan.md:212, 326). Confirmed by reading
  the updated plan.md directly.

## Concerns
- [x] **RESOLVED (accepted-gap acknowledgment)** — No CI trigger for the new test.
  Epic 1.1's goal statement (plan.md:161-166) now includes an explicit "**Accepted
  gap**" callout: `~/dotfiles/.github/workflows/ci.yml`'s `test` job doesn't discover
  or run this suite, enforcement is manual-only, matching the already-unrun
  `golang-profiling` precedent, and wiring CI discovery is explicitly called out of
  scope for this Small-appetite task. This doesn't fix the gap but satisfies the bar
  set for this re-review — a clear, undisguised acknowledgment rather than an implicit
  "test file added = regression caught" claim.
- [x] **RESOLVED (comment-based mitigation)** — Hand-copied schema drift risk in Task
  1.1.1e. The task (plan.md:271-279) now adds: "Since this is a hand-copied literal,
  not derived from the real schema, add a code comment on the `CREATE TABLE` in the
  test pointing back to `store_sqlite()`'s `CREATE TABLE` statement
  (`context_audit.py:229-244`) as the source of truth, so a future column change there
  is more likely to prompt updating this fixture too." This is a lighter fix than the
  original recommendation (derive via `PRAGMA table_info` instead of duplicating), but
  it's a real mitigation, not just an acknowledgment, and meets the "at least a
  one-line acknowledgment or mitigation" bar for this pass.

## Minors
- (carried forward, unchanged, not re-litigated) Task 1.2.1b's "Tests:" line in
  `SKILL.md` is disclosed scope-stretch beyond requirements.md's "docs audit, not new
  copy" framing — plan says so itself, still not worth blocking on.
- (carried forward, unchanged) Cosmetic path-style inconsistency between Task 1.1.1f's
  verification command (now `python3 -m unittest discover -s
  .claude/skills/context-audit/scripts/tests`, relative to a `~/dotfiles`-rooted cwd)
  and Task 1.2.1b's doc line (`~/.claude/skills/context-audit/scripts/tests`, via the
  `~/.claude` → dotfiles symlink). Both resolve correctly; still just a style mismatch.
- (carried forward, unchanged) No test exercises `usage` present at the top level but
  not a dict — already outside requirements.md's 4 scoped cases, noted for
  completeness only.
- **New**: plan.md's own "Unresolved Questions" section (lines 108-116) flags that it's
  still undecided *how* the `project_plans/context-audit-real-tokens/` tree itself gets
  from this consolette worktree into the `~/dotfiles` session Phase 5 needs — copy the
  directory over, vs. pointing `/sdd:5-implement` at this file via an explicit path.
  Explicitly left as a pragmatic call for whoever kicks off Phase 5, not silently
  glossed over. Doesn't block writing/approving the plan; worth resolving before
  actually invoking `/sdd:5-implement`.
