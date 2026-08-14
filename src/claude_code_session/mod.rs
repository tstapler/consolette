//! Claude Code session transcript compaction.
//!
//! See `project_plans/compaction-hook/decisions/ADR-008-claude-code-session-module-placement.md`
//! for why this lives as a self-contained top-level module rather than
//! under `crate::compression`.

pub mod boundary;
pub mod omission_cache;
pub mod prune;
pub mod summarize;
pub mod transcript;
pub mod writer;
