# ADR-010: Local `Summarizer` trait, subprocess-only in v1 (not `providers::Provider`)

**Status**: Accepted
**Date**: 2026-08-13
**Relates to**: requirements.md open question 4

## Context

magic-compact summarizes old assistant turns by shelling out to
`claude -p --resume` and parsing `<summary>` tags out of its stdout
(`compact.ts:85-125,518-554`). consolette already has a `providers` module
(`src/providers/*`) that talks directly to model APIs (OpenAI/Anthropic/
Bedrock-shaped) for its proxy/routing use case. The open question is whether
summarization should shell out directly (mirroring magic-compact, with a
hard dependency on the `claude` CLI being on `PATH`) or go through
`providers::Provider` so it degrades to a direct API call when the CLI isn't
available.

## Decision

**A new local trait, `claude_code_session::summarize::Summarizer`**, with
exactly one v1 implementation, `ClaudeCliSummarizer`, that shells out to
`claude -p --resume <session_id>` via `tokio::process::Command` — following
the same subprocess pattern already established in `src/auth/exec.rs`
(resolve command via `PATH`/explicit path, check the executable is
user-owned and not world-writable, spawn, `tokio::time::timeout`-wrap the
wait, treat non-zero exit / timeout / unparseable stdout uniformly as a
typed error, never log stdout/stderr content).

```rust
#[async_trait::async_trait]
pub trait Summarizer {
    async fn summarize(&self, session_id: &str, turns: &[Turn]) -> anyhow::Result<Vec<TurnSummary>>;
}
```

**Not** routed through `providers::Provider`. Rationale:

- `providers::Provider` is shaped around consolette's proxy use case:
  request/response message arrays matching the Anthropic/OpenAI Messages
  API, dispatched through the router's fallback/weighted strategy and
  rate-limiter (ADRs 002-004). Summarization here is a single, local,
  stateful operation (`claude -p --resume <session_id>` resumes *that
  session's own accumulated context*, including anything the target model
  provider doesn't expose over a bare Messages API call — system prompt,
  prior tool results, etc.). Reimplementing that context reconstruction
  against `providers::Provider` would mean re-deriving everything
  `claude -p --resume` gets for free from Claude Code's own session state.
- The trait seam is deliberately minimal: it exists so a future
  provider-backed `Summarizer` impl can be added without changing
  `claude_code_session`'s call sites, but v1 ships only the CLI
  implementation. Standing up a second implementation is not blocked, but
  it is not commissioned as part of this design — the `claude` CLI-on-PATH
  dependency is accepted, matching magic-compact's own constraint, and
  failure (CLI missing, non-zero exit, unparseable output) is handled as a
  typed `Summarizer` error surfaced to the CLI/MCP caller, not silently
  degraded to a different summarization path.

## Alternatives Considered

| Option | Rejected because |
|---|---|
| Route through `providers::Provider` | Loses `claude -p --resume`'s free access to the target session's own accumulated context; would require re-threading system prompt/tool history that the CLI already reconstructs from the transcript being compacted. |
| No trait — call `tokio::process::Command` inline in `boundary.rs`/`mod.rs` | Forecloses a future non-subprocess implementation and makes the summarization step untestable without actually invoking the `claude` binary; a one-method trait costs little and keeps `mod.rs`'s orchestration logic mockable in tests. |

## Consequences

- Hard runtime dependency on `claude` being installed and on `PATH` for the
  summarization step (same as magic-compact) — `compact-session` fails with
  a clear, typed error if it's missing, rather than silently no-op'ing.
- `Summarizer` is easily fake-able in tests (a `FakeSummarizer` returning
  canned `TurnSummary`s), so `claude_code_session::mod.rs`'s orchestration
  (which turns get summarized, how the destination transcript is assembled)
  can be tested without a real subprocess.
- If a provider-backed summarizer is ever justified, it's an additive
  `impl Summarizer for ProviderSummarizer` — no change to `boundary.rs` or
  `mod.rs`.
