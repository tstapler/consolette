//! ADR-003: routing — pluggable selection strategy (`RoutingStrategy`) over
//! shared per-upstream health/cooldown state (`HealthRegistry`), with the
//! router owning the dispatch loop.
//!
//! Rate limiting (ADR-004) is deliberately not part of this module: it
//! integrates post-selection via a separate `AdmissionControl::admit()` call
//! the router will invoke once ADR-004 lands — `Availability` here is
//! health/cooldown only.
//!
//! Nothing outside tests calls this yet — CLI/MCP wiring lands with the HTTP
//! provider implementations. `allow(dead_code)` is temporary until then.
#![allow(dead_code)]

pub mod bench_table;
pub mod health;
pub mod model_stats;
pub mod openrouter_scoring;
pub mod router;
pub mod session_overrides;
pub mod strategy;

#[allow(unused_imports)]
pub use health::{Availability, HealthRegistry};
#[allow(unused_imports)]
pub use router::Router;
#[allow(unused_imports)]
pub use session_overrides::{SessionOverride, SessionOverrideStore};
#[allow(unused_imports)]
pub use strategy::{FallbackStrategy, RoutingStrategy, UpstreamRef, WeightedStrategy};
