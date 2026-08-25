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
//! `project_plans/context-analyzer/implementation/plan.md`; some pieces are
//! built ahead of their caller (matches `src/providers/mod.rs`'s same
//! precedent) — see each such item's own `#[allow(dead_code)]` for why,
//! rather than a blanket module-level allow that could hide unrelated dead
//! code.

pub mod budget;
pub mod composition;
pub mod cross_check;
pub mod hook_event;
pub mod hooks_install;
pub mod ingest_claude_code;
pub mod mcp_server;
pub mod refresh;
pub mod server;
pub mod store;
pub mod usage;
