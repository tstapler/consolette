# Changelog


### Bug Fixes

- Retry launchctl bootstrap after bootout races (cac3a4d)

- Remove hardcoded anthropic/bedrock assumptions (41996c9)

- Surface array and reasoning content in OpenAI translations (84ec94a)

- Forward tools and translate tool_calls for Claude Code (5660a21)

- Claude Code session restore via /v1/models and model echo (16a7d1e)

- Sanitize tool schemas, log stream requests, model catalog (b951d2f)

- Walk the full free-model pool before giving up on 429s (fb2efcd)

- Sanitize tool schemas for Cohere-bound requests (47f9bdd)

- Map tools/tool_choice in OpenAI-to-Anthropic translation (285fc62)

- Keep coding-agent tool loops alive across translators (1c970b3)

- Capability eval stays undecided when no probe answered (92f5d67)


### Features

- Add list_models to Provider trait, implement for all upstreams (3edc092)

- Add RouteUpstreamRef.model per-upstream request override (ec3f32b)

- Add discoverability landing page at GET / (44a56a0)

- Add `consolette install` for the macOS LaunchAgent (7ccd29d)

- Begin Story 6.2 with dashboard/metrics port (ebb2ac8)

- Generalize per-upstream request attribution (Task 3.4.5) (c53f18a)

- Web control panel for model override and route strategy (ef6f052)

- Populate the Recent Requests ring buffer (43ab977)

- Wire up request-body inspector endpoint (0de0ce7)

- Per-session model/upstream steering (dc73046)

- Add Gemini and OpenRouter upstreams with quality-based free-model routing (#16) (f28c197)


### Build

- Re-sign cargo-installed binaries to fix macOS SIGKILL (aae574f)


### Merge

- Sync local main with origin/main after PR #16 squash-merge (61affcd)



### Bug Fixes

- Add missing [profile.dist] to Cargo.toml (082aaf6)



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


