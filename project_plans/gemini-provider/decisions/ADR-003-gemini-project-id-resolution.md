# ADR-003: Gemini project-id resolution — config-supplied, not dynamic `loadCodeAssist`

**Status**: Accepted
**Date**: 2026-09-04
**Deciders**: Tyler Stapler (via Phase 3 planning)

## Context

The Cloud Code Assist request envelope (`research/stack.md`, `research/features.md` §5) wraps
every native Gemini request body in an outer object requiring a `project` field:

```json
{"project": "...", "model": "...", "requestType": "agent", "userAgent": "antigravity",
 "request": { /* native Gemini contents/parts/generationConfig/tools */ }}
```

Nothing in today's schema carries an equivalent value: `UpstreamKind` (`src/config/schema.rs:107-120`)
has no generic "arbitrary cloud project" concept, and `AuthMethod` doesn't resolve one either. Two
ways to obtain it:

1. **Config-supplied**: a new field directly on `UpstreamKind::Gemini`, resolved once at config-load
   time, the same shape as `UpstreamKind::Bedrock`'s `aws_region`/`aws_profile`
   (`src/config/schema.rs:109-116`).
2. **Dynamically resolved**: call the internal `v1internal:loadCodeAssist` endpoint at provider
   startup (or lazily on first use) to discover the operator's project/plan/credits info, and cache
   the resulting project id.

## Decision

**Config-supplied for v1.** Add a required `project_id: String` field to
`UpstreamKind::Gemini`:

```rust
Gemini {
    project_id: String,
},
```

`references/conf.d/00-providers.toml`'s example sets this explicitly (plan.md Story 1.8.1), and
Tyler fills in his real Antigravity project id by hand — a one-time, five-minute manual lookup
(via the Antigravity IDE's own account/project display, or a one-off manual
`loadCodeAssist` call during setup) rather than a codepath consolette maintains.

## Rationale

- **Consistency with existing precedent.** `UpstreamKind::Bedrock` already carries
  upstream-specific, config-supplied identity fields (`aws_region`, `aws_profile`) rather than
  resolving them dynamically at startup from an AWS metadata endpoint. `UpstreamKind::Gemini`
  carrying `project_id` the same way is the smaller, more consistent diff — no new "resolve at
  startup" lifecycle concept needs to be invented for this one upstream.
- **Avoids a second startup failure mode before the feature has ever worked once.** A dynamic
  `loadCodeAssist` call at startup/first-use adds: a new network call, a new cache with its own
  invalidation/staleness policy, and a new class of startup failure ("Gemini upstream configured
  but `loadCodeAssist` failed, so project id is unknown") — all *before* a single Gemini text
  completion has ever round-tripped successfully. Given the appetite is Large-but-not-unlimited and
  the staged rollout explicitly prioritizes getting non-streaming text working first
  (`requirements.md` Risk Control), deferring this complexity is the right sequencing call even if
  dynamic resolution is eventually nicer UX.
- **Single-operator, single-project reality.** `requirements.md`'s Users/Consumers section states
  Tyler is the sole operator. A personal Antigravity subscription overwhelmingly has one project id
  that doesn't rotate; the dynamic-resolution win (auto-adapting to a project change) has no real
  payoff at this usage scale.

## Consequences

- If Tyler's project id ever changes (e.g. a new Google Cloud project), he edits one config field
  and restarts — a one-line diff, matching the existing "rollback is deleting the config entry"
  Risk Control shape.
- A `loadCodeAssist`-based dynamic-resolution fallback is a documented, explicit follow-up (see
  plan.md's Unresolved Questions), to be built only if the config-supplied value proves
  insufficient in practice (e.g. Tyler starts using multiple Antigravity projects) — not built
  speculatively now.
- `list_models` (the `Provider` trait's other required method) can still call
  `v1internal:fetchAvailableModels` directly without needing `loadCodeAssist` first, since that
  endpoint's research-documented shape doesn't require a resolved project id as an input — only
  the chat-completion request envelope does.

## Rejected alternative

Dynamic resolution via `loadCodeAssist` at provider startup/first-use — rejected for v1 per
Rationale above: adds a new startup-time network dependency and caching-lifecycle question with no
payoff at Tyler's single-project usage scale; revisit only if a real multi-project need shows up.
