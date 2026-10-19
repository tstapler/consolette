# Changelog


### Bug Fixes

- Force ad-hoc re-sign to avoid macOS taskgated SIGKILL (ff6bd8a)

- Resolve clippy warnings and formatting for GitHub Actions test gate (dc6b2d0)

- Preserve retry_after header in OpenAI and Anthropic 429 response mapping (96e389e)

- Correct mock-server teardown doc comment (9e17cba)

- Address code review findings on anthropic base_url PR (9394c1f)

- Cap local discovery response size/ids, warn on new backend (0ee0482)

- Disable proactive health prober by default and isolate rate limits from circuit breaker (4fb798f)

- Address code review findings on response cache (#24) (0b67d37)

- Catch tools+reasoning_effort WrongEndpoint failures proactively and reactively (dbaa68c)

- Re-classify the max_completion_tokens retry's own failure (4a2a84f)

- Stop stripping fields the Anthropic API now supports (2ebb382)

- Await async responses translation in WrongEndpoint retry paths (972a1d8)

- Raise request body limit above axum's 2 MB default (017c5cc)

- Build the SPA with base-href /dashboard/ so assets resolve (074a730)

- Classify Anthropic Validation 401/403/429 as auth/rate-limited (3f0e6e2)

- Session Replay loads payloads by latestRequestId and lists sessions on first load (4.1.1) (4bb1a1b)

- Remove fabricated benchmark rows and constant rates (4.1.2) (2bd33e5)

- Config Editor keeps auth, sends only changes, rate limits read-only (4.1.3) (768718c)

- Real provider health and honest KPI cards (4.1.4) (ac45774)

- Bound graceful shutdown so open SSE streams cannot keep a stopped proxy alive (4.1.5 E-02) (56dcd84)

- SSE client detects silent streams and supports manual Reconnect (4.1.5 E-02) (87da0c5)

- Session Replay SAVED % shows an en dash when tokens_after was not recorded (E-05 residual) (8f32dcf)

- Benchmarks TTFT column shows n/a (not measured), not duration-equal values (E-06 residual) (e511102)

- Legend separates term from definition (E-26) (6419f2d)

- Drawer wording for cache hits and session pins (E-23) (a73e5b5)

- Focus ring, 4.5:1 text, 24px targets, reduced motion, forced-colors, drawer offset, skip link, nav menu, session row buttons (4.1.7b-e, g; E-01, E-04, E-07 to E-12, E-24, E-25) (71071e3)

- Models status region announces refresh start and finish; pinned-model warning has a text label (E-28) (6e4d253)

- Latency chart animation off under prefers-reduced-motion (E-09) (a976a85)

- Trace table scroll region contains sr-only spans so the page no longer scrolls horizontally; Config header wraps (E-24) (d243858)

- Record only 4xx/5xx upstream statuses; 499 is reserved for client aborts (7001b89)

- Drawer clears inert on destroy and keeps attempt DOM stable across live pushes (U1, U2) (a5cae86)

- Prune the model catalog on every router hot-swap via EntrypointState::swap_router (b3fe68b)

- Mask the provider API key input (S2) (df87a1d)

- Persistent live region for the reconnect banner; clear announce timer on destroy (S5) (f379e3a)

- Cap requested_model at 256 chars at ingest (S4) (baebe1b)

- Terminal trace revision outranks in-flight placeholder; backfill cannot downgrade a finished row (B1) (498f49c)

- Epoch per upstream so a hot-swapped provider cannot receive the old provider's refresh (B2) (0704575)

- Hold-retry budget is per dispatch_traced call, total still recorded on the trace (B3) (c861f08)

- SSE streams end on shutdown; in-flight requests get a 30s grace instead of 2s (B4) (3d4d3b1)

- Drop deprecated AtomicU8::fetch_update in ScopedDispatch (fb2b856)

- Key hot-swap resets on a config fingerprint, not Arc identity; notify on real change (C1) (c104ab8)

- A second Ctrl-C/SIGTERM during shutdown exits immediately (C2) (2bfdc25)

- Fidelity of system, usage, stop reason, trace model and cost on OpenAI paths (3b07635)

- Record the priced model on the live path so cost is computed (8010386)

- Price emulated-search and chat-completions requests by the routed model (968c573)

- Cache-inclusive prompt_tokens for chat completions; retry stream without stream_options (da17495)

- Scope the resolved model to the upstream that reported it (3301cfe)

- Web_search emulation follows the forced upstream's kind (8c184f7)

- Benchmarks and session summaries use the routed model (c594883)

- Per-model counters and local requests use the routed model (2841a72)

- Count failed family attempts under the resolved model (a1dc256)

- Normalize prefixes, reject blank selectors, cover session pin (a1d4b79)

- Optimize tiktoken deps in dev profile so session BI tests meet their timeout (4e3bb41)

- Address code-review findings on effort mapping and auto-mode (e59d006)

- Key the response cache on output_config and thinking (9e8f9c5)


### CI

- Fail on stale or placeholder embedded UI bundle (1.1.2f) (de2dbf1)


### Features

- Proactive upstream health probing, circuit breaking, and openrouter free model expansion (1e5c6e0)

- Redesign web UI to interactive Angular 19 SPA embedded in binary (4d2a63e)

- Add configurable base_url to the anthropic upstream kind (4fb2985)

- Auto-discover local Ollama/LM Studio/OpenAI-compatible models (3a38283)

- Query OpenRouter API key status to enforce daily free model request quota pre-flight (711c518)

- Add opt-in semantic response cache for /v1/messages (#24) (6f18cfe)

- CamelCase RequestTrace contract pinned by shared fixtures (1.1.2a-b) (b1c9c5f)

- Align SSE models and store to camelCase contract, upsert traces by requestId (1.1.2c-d) (2a6756a)

- Compat_snake_case dual-emit rollback for RequestTrace (1.1.2g) (012825d)

- Table-driven naReason and copyText helper with execCommand fallback (1.1.4a,g) (d8c202a)

- Trace table renders real values and honest n/a reasons (1.1.4b-f,h) (98dd0f4)

- ProviderError::trace_status maps every variant to an HTTP status (1.1.3a-b) (3bc391d)

- TraceScope emits one terminal trace per dispatch (1.1.3d-e) (376ec42)

- RoutingStrategy::name with fallback/weighted/custom defaults (2.1.1d) (70c96ce)

- Attempt log, route reason, error class and AttemptTrace wire form (2.1.1a-c) (1b514b3)

- Capture attempts and route reason in dispatch; finish writes the trace once (2.1.2, 2.1.3) (b2d6d05)

- Route cell shows requested -> routed, reason chip, attempts badge, chip legend (2.1.4) (b828752)

- Inspect drawer with attempt waterfall, Why block, sheet focus containment (2.1.5) (0bb7ac3)

- One trace per client request across handlers, emulation loop, cache hit and local (2.1.6a-c) (291bd04)

- Per-client-request error rate and attempt-based counter semantics (2.1.6d) (f5d93bc)

- Remove Epic 1 trace coverage note now every client request has one trace (2.1.6f) (8b1a9c0)

- ModelCatalog state types, sanitized error classification (3.1.1, 3.1.2) (9c5d033)

- Carry upstream names via with_provider_names/providers_named (3.1.3b) (cb9b1f4)

- [model_catalog] refresh mode and retry settings (3.1.3d) (5c3b4be)

- Single-flight refresh orchestration with throttle, retry floor and interval loop (3.1.3a, 3.1.3c) (7031e28)

- ModelCatalogChanged dashboard event (3.1.3f) (fcc2a4f)

- Config_warnings for pinned models the upstream does not list (3.1.6a, 3.1.6b) (b1db9e9)

- Model catalog endpoints, state wiring, interval loop, metrics (3.1.3e, 3.1.4) (534b7f3)

- Prune model catalog on route hot-swap without refreshing (3.1.3e2) (9a3cb3a)

- Model catalog store, labels and SSE dispatch (3.1.5a, 3.1.5b) (e775eb8)

- Upstream Models view with states, search, refresh and pinned-model warning block (3.1.5c, 3.1.6c) (98e13a4)

- Models nav entry (3.1.5e) (5baa7ed)

- Label /metrics-derived rate as upstream attempts (2.1.6d) (abd7963)

- Surface system_lag, disconnect alert, ErrorLogged and ConfigChanged (4.1.5) (fff9192)

- GET /v1/dashboard/traces backfill from the ring buffer (4.1.6) (afe24c4)

- Seed traces from backfill on load, lag and reconnect (4.1.6 E-20) (1f66c99)

- Live region, Pause/Resume and held updates while focus is in the table (4.1.7c1, c2; UX-22, UX-23) (399c240)

- Add claude-sonnet-5-5 and gpt-6.1-sol rates (a0a88d6)

- X-Consolette-Upstream header forces one upstream (3f91021)

- Add [[model_tiers]] schema and validation (d69fddb)

- Map client model tiers to per-upstream models (206e9ab)

- Parse client effort, per-model capability table, config and trace field (f6f1c45)

- Keep Claude Code's server-side classifier review usable through the proxy (2045e66)


### Refactor

- Move provider-name builder into router/provider_names.rs (feb0488)

- Move local dispatch into sibling module to restore the mod.rs line budget (f4f650d)


### Build

- Rebuild the embedded dashboard UI automatically via make stamp targets (28ee4b0)

- Build the dashboard UI from build.rs when its inputs change (f2633b5)

- Declare @angular/build as a direct devDependency (S3) (b9af40e)


### Spike

- Build consolette and its Angular UI with Bazel 9 (c2fcab5)

- Build the Angular UI inside the sandbox (49ec827)



### Bug Fixes

- Gate OCR on vision support, run it off the async runtime (d02ba7d)


### Features

- Implement turn-decay transcript memory pruning and HTTP control plane (e879983)

- OCR non-vision-model image attachments via tesseract (8faac0a)

- Per-model token/request counters on the dashboard (3cf2717)

- Hold requests through short rate-limit cooldowns (eac9b21)

- Add OpenAI Responses API support and model resolution (45a4347)


### Refactor

- Split counters.rs into a module, remove flag args (90161e9)

- Split router.rs into a module, fix long-fn/nesting/param-list (47eb596)

- Split messages.rs, fix long-fn/nesting/param-list (7213245)



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


