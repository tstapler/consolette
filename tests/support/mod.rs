//! Shared helpers for `tests/*.rs` integration and e2e crates (`mod support;`).
//!
//! Which tests use what: in-crate `#[cfg(test)]` modules (router, catalog,
//! `compress_step`) cannot reach this directory and use the fake providers in
//! `src/router/tests.rs`. Integration and e2e tests under `tests/` use
//! [`fake_upstream::FakeUpstream`]. Tests needing real sockets and timers
//! assert ordering with wide tolerances, not exact milliseconds.
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)] // test-only harness

pub mod fake_upstream;
