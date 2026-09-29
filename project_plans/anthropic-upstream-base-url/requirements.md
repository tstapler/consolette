# Configurable base URL for the `anthropic` upstream kind

Planned 2026-09-25 — research, plan, architecture/adversarial review, validation, and pre-mortem
all complete (see `implementation/`). Triad review: READY TO BUILD. Next step: `/sdd:5-implement`.

## Problem

`UpstreamKind::Anthropic` (`src/config/schema.rs:108`) is a bare unit variant.
`AnthropicProvider::new` (`src/providers/anthropic.rs`) hardcodes
`base_url: "https://api.anthropic.com".to_string()` with no override —
unlike `UpstreamKind::Openai { base_url: String }`, which already takes one.

Anyone who has an Anthropic Messages API-compatible endpoint that isn't
`api.anthropic.com` itself (an internal proxy, a corporate gateway, a
self-hosted relay) can't point a `kind = "anthropic"` upstream at it today —
they have to configure it as `kind = "openai"` instead, which pulls in the
whole OpenAI wire-format translation layer for a request that's already
Anthropic-shaped, or fork `AnthropicProvider` entirely.

## Desired outcome

`UpstreamKind::Anthropic` gains an optional `base_url` field, defaulting to
`https://api.anthropic.com` when unset (existing configs stay byte-identical,
matching this codebase's "completely unchanged for unset fields" convention
used elsewhere, e.g. `model_family`). `AnthropicProvider::new` takes that
value the same way `OpenaiProvider::new` already takes its `base_url` param.

## Known constraints

- Mirror `OpenaiProvider::new`'s existing `base_url: String` parameter
  exactly — same construction site pattern in `routing/router.rs::build_providers`.
- `AnthropicProvider`'s own doc comment currently states the hardcoded value
  "matches legacy's default" — that comment needs updating alongside the change.
- No test harness in `src/providers/anthropic.rs` today spins up a local mock
  server (unlike the `openai` module's axum-based one) — adding a real
  regression test for a configurable base URL means either building that
  harness or reusing/generalizing the openai module's.

## Open questions for planning

- Should `base_url` support the same auth-method flexibility (`Bearer`/`Apikey`/`Exec`)
  already on `Upstream.auth`, or does an Anthropic-compatible endpoint always
  expect the same auth shape as `api.anthropic.com` itself?
