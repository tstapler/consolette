//! Context-analyzer feature: exact API usage extraction, cache-aware
//! pricing, composition classification, and dashboard surfacing for Claude
//! Code (and later Codex) sessions.
//!
//! A new top-level module, sibling to `claude_code_session`, `cost_metrics`,
//! `metrics`, `ratelimit`, `routing` — it *depends on* both
//! `claude_code_session` (parsed transcript rows/turns) and `cost_metrics`
//! (pricing) for narrow, specific reuse, but does not extend either. See
//! `project_plans/context-analyzer/research/architecture.md` §1
//! ("Integration points: new top-level module, depending on both existing
//! modules — extending neither").
//!
//! Built up epic-by-epic per
//! `project_plans/context-analyzer/implementation/plan.md`; Epic 1.1
//! introduces only [`usage::extract_call_usage`], which has no caller yet
//! outside its own unit tests — later epics (1.2's store, 1.3's ingestion
//! pipeline) wire it in. Matches `src/providers/mod.rs`'s same
//! built-ahead-of-its-caller precedent.
#![allow(dead_code)]

pub mod store;
pub mod usage;
