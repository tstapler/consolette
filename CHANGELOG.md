# Changelog


### Bug Fixes

- Apply cargo fmt and resolve clippy too_many_lines to fix CI (cc8d5e8)

- Cache pruned content under destination session id (e2aae05)

- Allow CompactionTier::Off reconciliation without a counterfactual (cc6a718)


### CI

- Mint a scoped GitHub App token for the homebrew-tap push (5957fe2)


### Features

- Implement consolette core per ADRs 001-007 (all 12 acceptance criteria) (c84491e)

- Port claude-proxy-rs modules into consolette, satisfy clippy -D warnings (522ab2d)

- Add claude_code_session module skeleton with transcript parsing and boundary planning (b3ae7b2)

- Add tool-output pruning and omission cache (Epic 2.1-2.2) (4fc2529)

- Add Summarizer trait and subprocess-based ClaudeCliSummarizer (Epic 3.1-3.2) (2478e8f)

- Add destination-transcript writer (Epic 4.1) (fa152f9)

- Add MCP server and compact-session CLI (Epic 5.1-5.2) (b53df24)

- Add StructuralCollapse, SemanticDedup, IP/enum collapsing, and text-noise stages (38bd2e0)

- Add LogCrunch and QuantumLock stages (2382311)

- Add summarizer, plan/skill reinjection, and compact hooks (issue #7) (66034e0)

- Native compaction parsing + session BI dashboard (#13) (a101185)

- Wire ADR-007 plugin discovery and exec bin-dir resolution (62a2c6c)

- Distinguish failover exhaustion from upstream errors (9a1d0eb)

- Add HTTP proxy entrypoint with Anthropic and OpenAI-compat endpoints (6fde89e)

- Implement generic OpenaiProvider for UpstreamKind::Openai (468411d)

- Wire Anthropic<->OpenAI translation into OpenaiProvider (4001078)

- Context-analyzer feature (Phases 1, 2, 4, 5) (#14) (60c9168)


