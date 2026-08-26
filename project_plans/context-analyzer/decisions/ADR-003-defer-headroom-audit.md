# ADR-003: Defer the headroom/compression-ceiling audit to a follow-up project

**Status**: Accepted
**Date**: 2026-08-24
**Relates to**: requirements.md's resolved Open Question 3 and Out of Scope section; `research/build-vs-buy.md` §4

## Context

context-analyzer's `audit-headroom` view depends on `headroom-ai` (PyPI, Apache-2.0, pure-Python, latest `0.36.5` as of this research), a materially complex and actively-changing algorithm surface: field-level statistical analysis, Kneedle-algorithm bigram-coverage subset selection for JSON, AST-aware code compression, pattern clustering for logs, plus a reversible-compression cache mechanism. It is not a small formula that can be ported from a README description.

Three options exist: reimplement the algorithm in Rust from scratch, shell out to the real Python package as a subprocess, or defer the view entirely.

## Decision

Defer. Neither remaining option is acceptable as scoped: a from-scratch Rust reimplementation risks shipping wrong numbers under a "compression ceiling" claim Tyler would use to judge real engineering decisions elsewhere in consolette, guessing at thresholds a 36-version-deep upstream project has already tuned; shelling out to Python directly contradicts consolette's Rust-only constraint, not just in spirit but literally (introduces a Python runtime dependency into an explicitly Rust-only project).

All four other Scope items (hook capture, transcript ingestion, persistent store, and the other dashboard views) stand on their own without this view — it is the single most self-contained item in Scope.

## Consequences

- This feature ships without headroom/compression-ceiling auditing. Nothing in Phases 1–5 depends on it.
- If Tyler wants this view later, the follow-up project should first re-litigate the Rust-only constraint itself (a single, isolated, occasionally-invoked Python subprocess call for one offline audit view is a narrower ask than "depend on Python" reads as at first) rather than defaulting to a bespoke Rust port.
