# Haiku-tier model mapping

Queued 2026-09-25 — not yet planned. Run `/sdd:2-research` and `/sdd:3-plan` before implementing.

## Problem

`Router::dispatch` (`src/routing/router.rs:536-546`) sets a chosen candidate's
`model`/`model_family` purely from the static route config — it never inspects
the incoming request's own `model` field. A request for `claude-haiku-4-5` and
one for `claude-opus-4-6` hitting the same route get the identical
`model_family` treatment. There's no way to route cheap/fast-tier requests to
a cheaper upstream model family than expensive-tier requests.

## Desired outcome

A route upstream can declare more than one `model_family` (or `model`),
keyed by which Claude model tier the incoming request named, so Haiku-tier
traffic can resolve against a cheaper family than Sonnet/Opus-tier traffic
hitting the same upstream.

## Known constraints

- `RouteUpstreamRef` (`src/config/schema.rs`) currently has one `model` xor
  one `model_family` field, validated mutually exclusive by
  `validate::validate_model_selectors`. Adding tiered selection means either
  a new field shape here or a tier-keyed map alongside the existing fields.
- Claude Code sends the client-requested Anthropic model id (`claude-haiku-*`,
  `claude-sonnet-*`, `claude-opus-*`) in the request body's `model` field
  before any route-level rewrite — `Router::dispatch` has access to it, just
  doesn't use it today.
- Needs a tier-classification step (prefix/substring match on the incoming
  `model` string) distinct from the existing per-family candidate resolution
  in `src/providers/openai/resolution.rs`, which only ever sees the *outgoing*
  family, not the original request.

## Open questions for planning

- Exactly which tiers matter — Haiku vs. everything else, or Haiku/Sonnet/Opus
  as three distinct buckets?
- Does an upstream with no tiered config keep today's single-family behavior
  unconditionally (matching this codebase's stated "completely unchanged for
  unset fields" convention elsewhere)?
