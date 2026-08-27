//! consolette — provider-agnostic LLM router library.
//!
//! Split out as a library (mirroring the legacy `claude_proxy_rs` crate) so
//! the `consolette` server binary and the `mcp-proxy`/`cmdcrush` companion
//! binaries can share the feature modules (compression, memory, metrics,
//! learn, `system_prompt`) and the ADR-001..004/007 config/auth/routing/
//! ratelimit abstractions without duplicating code.

pub mod auth;
pub mod claude_code_session;
pub mod compression;
pub mod config;
pub mod context_forensics;
pub mod cost_metrics;
pub mod dashboard;
pub mod entrypoint;
pub mod learn;
pub mod mcp_gateway;
pub mod memory;
pub mod metrics;
pub mod providers;
pub mod ratelimit;
pub mod routing;
pub mod service;
pub mod session_compaction;
pub mod system_prompt;
