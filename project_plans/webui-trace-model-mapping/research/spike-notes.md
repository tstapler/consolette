# Spike notes: Story 1.1.1 (scratch)

Run 2026-10-10 on branch `feat/webui-trace-model-mapping`. Every claim is VERIFIED (command run or source opened) unless labelled INFERRED/UNVERIFIED.

## Summary

| Task | Result |
|------|--------|
| 1.1.1a SSE frame capture | PASS, wire is snake_case, no `id` key. Contradiction with plan: `status_code` is a hardcoded literal 200 (see below). |
| 1.1.1b `list_models` per adapter | Recorded below. No live calls (no credentials). |
| 1.1.1c `resolvedModel` | YES, feasible in 3 files via an accessor, NOT via the `ProviderResponse` return type. |
| 1.1.1d status mapping reuse | NO, as expected. |
| 1.1.1e Karma JSON fixture / stale bundle | `resolveJsonModule` is NOT needed. No `tsconfig.spec.json` change made. |
| 1.1.1f hotspots | Recorded; `src/routing/router.rs` is now `router/mod.rs`. |
| 1.1.1g legacy SSE consumers | **PASS: zero non-UI SSE consumers.** Task 1.1.2b is unblocked. |
| Compression absence | Confirmed, zero hits. |

## 1.1.1a Live SSE frames

Artifact: `research/sse-frames.txt` (first three frames of the raw capture).

Method (the plan's "throwaway axum fake" was replaced by a ~15-line Python stdlib fake because it needs no build; Story 1.1.5's harness was not used):
- Fake upstream `/tmp/spike111/fake.py` on `127.0.0.1:18081` answering `POST /v1/messages` with an Anthropic-shaped message and `GET /v1/models`.
- Config `/tmp/spike111/home/.config/consolette/conf.d/00-spike.toml`: `port = 18080`, one `kind = "anthropic"` upstream named `fake` with `base_url` at the fake and inline apikey auth, one `fallback` route.
- `CONSOLETTE_REQUIRE_UI=1 cargo build`, then `HOME=/tmp/spike111/home target/debug/consolette run`.
- `curl -sN http://127.0.0.1:18080/v1/dashboard/events > sse.raw` while `curl -X POST /v1/messages -d '{"model":"claude-haiku-4-5",...}'` returned `HTTP/1.1 200 OK`.

Captured frame (verbatim):

```
event: message
data: {"type":"RequestTrace","data":{"request_id":"05029c2a-...","timestamp":"2026-10-10T18:09:53.222465+00:00","provider":"fake","model":"claude-haiku-4-5","duration_ms":2,"first_byte_ms":2,"tokens_before":22,"tokens_after":0,"compressed":false,"status_code":200}}
```

Findings:
- Answer to Unresolved Question (a): **raw frame is snake_case** (`request_id`, `duration_ms`, `first_byte_ms`, `tokens_before`, `status_code`), and there is **no `id` key**. Matches the plan AC.
- Envelope is `{"type": "<Variant>", "data": {...}}` with SSE `event: message` for every event. The UI parser must keep that envelope.
- `MetricsTick` frame: `{"type":"MetricsTick","data":{"rpm":0.0,"total_requests":0,"total_tokens_saved":0,"current_lag_ms":2.15,"provider_health":{"fake":"healthy"}}}`. `provider_health` is a map keyed by upstream name (not an array) and all values were `"healthy"`. A tick is emitted about once per second; the first tick arrived before the request.
- **Contradicts the plan's framing:** `status_code` is hardcoded `200` at `src/metrics/mod.rs:292` (`status_code: 200,` inside the trace-event builder at `:285-293`); the trace is built only from `RequestDetail`, which has no status field. Failures are therefore invisible today, which confirms the feature's premise. `first_byte_ms` equalled `duration_ms` (2 and 2) in this run; `tokens_after` was 0 and `compressed` false.
- The trace is emitted from `src/metrics/mod.rs:299` through `event_tx` (`src/entrypoint/mod.rs:106`); the SSE handler subscribes at `src/entrypoint/events.rs:70`.

## 1.1.1b `list_models` behavior per adapter

No live calls were made: no Anthropic/OpenAI/OpenRouter/Gemini credentials or AWS profile were used in this run (UNVERIFIED against real upstreams). Findings are from source.

| Adapter | Source | Behavior | Id shape | Size | Credential side effects |
|---------|--------|----------|----------|------|-------------------------|
| Anthropic | `src/providers/anthropic.rs:574` (`fetch_models` at `:387`) | `GET {base_url}/v1/models`; returns a list (`data[].id`), `owned_by` hard-set to `"anthropic"`; non-2xx becomes `ProviderError` via `map_error_status` | Bare ids, e.g. `claude-haiku-4-5` | No `limit` or pagination parameter is sent. INFERRED: the real API's default page size (20) would silently truncate the list; verify live. | `build_headers` goes through `apply_auth_headers`, which can run an `exec` credential helper (cached by `ExecCredentialCache`) or resolve an env/keychain secret. A refresh can therefore spawn a helper. |
| Bedrock | `src/providers/bedrock.rs:904` | Builds a second control-plane client (`aws_sdk_bedrock::Client::new(&self.aws_cfg)`) and calls `ListFoundationModels`; any SDK error becomes `Upstream { status: 0, body: e.to_string() }` | Foundation-model ids without region prefix, e.g. `anthropic.claude-3-haiku-20240307-v1:0`; `owned_by` = provider name | Region-wide foundation model list (large, includes non-Anthropic vendors) | Uses the `aws_cfg` loaded at construction (`:226-232`) via the AWS default chain or configured `aws_profile` (SSO/credential_process can fire). Needs `bedrock:ListFoundationModels` IAM, which the data-plane `InvokeModel` role may lack. Error text is the SDK's `to_string()`, so it must be sanitized (matches the plan's `CatalogError` rule). |
| Gemini | `src/providers/gemini/mod.rs:497` (`fetch_models` at `:344`) | `POST {base_url}/v1internal:fetchAvailableModels` with body `{}`; reads `models[]`, strips a `models/` prefix; `owned_by` = `"google"`; non-2xx returns `Upstream` carrying the raw response body text | Bare ids after prefix strip | Whatever the undocumented endpoint returns | Auth via `build_headers` (OAuth/exec path). Undocumented, drifting protocol: a missing `models` key yields an empty list, not an error (`parse_available_models`, `:448-467`). |
| OpenAI | `src/providers/openai/mod.rs:865` (`fetch_models` at `:293`) | `GET {base_url}/v1/models`; `data[].id` plus `owned_by` | Provider-defined; `base_url` is user-configured so any OpenAI-compatible server | Unbounded (single call) | `build_headers` auth (bearer/apikey/exec). Same `list_models` already feeds `resolve_family` (`openai/mod.rs:704`), so it is already called on a family-route cache miss. |
| OpenRouter | `src/providers/openrouter/mod.rs:656` (`fetch_models_raw_at` in `models.rs:32`) | `GET https://openrouter.ai/api/v1/models` (const `BASE_URL`, `mod.rs:40`); `parse_model_infos` reads `data[].id` and `owned_by` (`models.rs:62-78`); non-2xx classified by `classify_error_response` | `vendor/model` slugs | Large (hundreds of entries, INFERRED) | Auth via `build_headers`; `list_models` and `list_free_models` share one HTTP call. |

Implications for ADR-003 refresh mode: refreshing every upstream on a timer would run credential helpers, SSO and AWS chains in the background, so `OnDemand` as the default is justified. Bedrock comparison key is `BedrockProvider::normalize_model` (`bedrock.rs:258`; test `normalize_model_strips_region_and_vendor_prefix` at `:937`), which strips `us.`/`eu.`/`ap.` and vendor prefixes. No adapter returns an `Ok` empty list as an explicit "unsupported" signal; all five either return a list or a `ProviderError`, so `CatalogState::Unsupported` must be set by the catalog from config/kind, not by an adapter.

## 1.1.1c `resolvedModel` feasibility

**Answer (c): YES, feasible within <= 3 files, but not through the return type.**

- Where the id is chosen: `resolution::resolve_family(...)` is called at `src/providers/openai/mod.rs:700`; the result is written into the outgoing body at `:711-714` (`obj.insert("model", resolved.model_id)`). `ResolvedModel` (with `model_id`, `endpoint`, `token_param`, `resolved_at`) is cached in `ResolutionState.cache: DashMap<String, ResolvedModel>` (`resolution.rs:279`), keyed by family.
- Why not the return type: `ProviderResponse` (`src/providers/mod.rs:24`) is `Full(Value) | Stream(..)` and is constructed at 12 sites across 5 provider files (anthropic.rs:567,570; bedrock.rs:895,900; gemini/mod.rs:491,494; openrouter/mod.rs:642,652; openai/mod.rs:767,777,829,837,859,866), so adding a field exceeds three files.
- Diff sketch (3 files): (1) `src/providers/mod.rs`: add a defaulted trait method `fn resolved_model_for(&self, family: &str) -> Option<String> { None }` on `Provider`. (2) `src/providers/openai/mod.rs`: implement it by reading `self.resolution.cache.get(family).map(|e| e.model_id.clone())`. (3) `src/routing/router/mod.rs`: after a successful `send` on a `model_family` candidate, call it and store the result on the attempt. The read is racy only across a concurrent re-resolution of the same family, which a TTL-bound cache makes rare; a hit within the TTL is the common case (`resolution.rs:832`).
- Non-streaming bonus: the OpenAI path's `Full` response carries the resolved id as `model` (it passes the post-resolution `model` into the translators at `openai/mod.rs:781-786`), but streams only carry it in the translator's first event, so the accessor is the uniform route.
- Conclusion for the plan: `resolvedModel` stays optional and may be implemented as a small extra task; nothing else depends on it.

## 1.1.1d Status-mapping reuse

**NO, not reusable for `trace_status`.**

`ProviderError` is at `src/providers/mod.rs:36-65`; the two mappers are `map_provider_error_anthropic` (`src/entrypoint/errors.rs:16`) and `map_provider_error_openai` (`:76`). Differences found:

| Variant | Anthropic mapper | OpenAI mapper |
|---------|------------------|---------------|
| `Timeout` | 529 (`errors.rs:38-41`) | 503 (`:102-103`) |
| `Exhausted` | 529 (`:46-49`) | 503 (`:107-108`) |
| `Validation(_, status)` | 400, ignores the carried status (`:18`) | 400 (`:82`) |
| `ModelUnsupported` | 404 | 404 |
| `Upstream { status }` | `from_u16(status)`, falling back to 500 (`:59-61`); `status: 0` (unreachable) therefore becomes 500 | same (`:120-123`) |
| `ResponseShapeMismatch` | 502 | 502 |
| `RateLimited` / `RateLimitedWithRetry` | 429 | 429 |

Reasons not to reuse: the mappers are split by client dialect (Timeout/Exhausted differ by 529 vs 503), they return `(StatusCode, retry_after, body)` rather than a status alone, `Upstream { status: 0 }` collapses to 500 (loses `Unreachable`), `Validation` drops the upstream status, and the Anthropic one uses `unwrap()` under `#[allow(clippy::unwrap_used)]` (`errors.rs:39-42`). A separate `ProviderError::trace_status()` is needed, as the plan expects. `kind_label()` (`providers/mod.rs:125`) is the existing typed classifier but has no `ResponseShapeMismatch`-independent status.

## 1.1.1e Karma JSON fixture import and stale bundle

**Answer (e): `resolveJsonModule` is NOT needed.** `ui/tsconfig.json` uses `"moduleResolution": "bundler"`, which implies `resolveJsonModule`; `npx tsc -p tsconfig.spec.json --showConfig` prints `"resolveJsonModule": true`. No change was made to `ui/tsconfig.spec.json` (so Task 1.1.2c has no tsconfig prerequisite).

Experiment (throwaway files, all deleted afterwards):
- `ui/src/testing/fixtures/sse/probe.json` plus `ui/src/app/probe.spec.ts` with `import probe from '../testing/fixtures/sse/probe.json'`.
- `cd ui && npx ng test --watch=false --browsers=ChromeHeadless --include='src/app/probe.spec.ts'` printed `TOTAL: 1 SUCCESS`.
- The import is type-checked: changing the assertion to `probe.data.nope` made the same command fail with `error TS2339: Property 'nope' does not exist on type '{ requestId: string; }'`. So a shared fixture gives the UI test real field-name checking.

Stale-bundle check (`CONSOLETTE_REQUIRE_UI=1 cargo build`):
- Baseline: `ui/dist/consolette/browser/main-NM7RMTOA.js`, md5 `2f31d9b68d90f1758bb055ce8676bc37`.
- After adding only the fixture and spec (not in the app bundle): `build.rs` re-ran `npm run build` (index.html mtime moved to 11:12) and the hash stayed `main-NM7RMTOA.js` / `2f31d9b6...`. Fixtures and specs do not leak into the bundle.
- After appending `console.log('spike-probe-1.1.1e');` to `ui/src/main.ts`: new bundle `main-TF4MDI27.js`, md5 `2e3a2cf2a2f43dd2a3c0011995e2100b`, and `grep -c spike-probe` = 1. So `CONSOLETTE_REQUIRE_UI=1 cargo build` does rebuild and re-hash on a real UI change.
- After reverting (`git checkout -- ui/src/main.ts`) and rebuilding: `main-NM7RMTOA.js` again, `grep -c spike-probe` = 0. The tree is restored.
- Gotcha for the plan: `build.rs` `UI_INPUTS` (`build.rs:17-25`) lists `ui/src`, `ui/public`, `package*.json`, `angular.json`, `tsconfig.json`, `tsconfig.app.json` but not `tsconfig.spec.json`. Harmless, since specs are not in the app bundle. A fixture or spec under `ui/src` does trigger a rebuild (it is under `ui/src`), which is cheap but unnecessary.
- Debug vs release embedding: `rust-embed = "8.5"` with default features (`Cargo.toml:81`, no `debug-embed`) and `#[folder = "ui/dist/consolette/browser"]` (`src/dashboard.rs:697-698`). INFERRED from rust-embed's documented default: debug builds read files from disk at request time, release builds embed. The `build.rs` placeholder (`<h1>Consolette Dashboard (Loading...)</h1>`) is written only when `index.html` is missing.
- Bazel: `ui/BUILD.bazel:9` uses `_UI_SRCS = glob(["src/**", "public/**"]) + [angular.json, package.json, tsconfig.json, tsconfig.app.json]`, so `ui/src/testing/**` is covered by the existing glob with no BUILD change. `tsconfig.spec.json` is not in `_UI_SRCS`; only matters if Bazel ever runs Karma specs (UNVERIFIED; this spike did not run Bazel).

## 1.1.1f Hotspot confirmation

`git log --since=6.months --name-only --format= | sort | uniq -c | sort -rn | head -12` (top of list):

```
22 src/providers/mod.rs
20 src/entrypoint/mod.rs
19 src/routing/router.rs   (path no longer exists; now src/routing/router/mod.rs)
16 src/config/schema.rs
14 src/main.rs
14 src/entrypoint/observability.rs
14 src/entrypoint/messages.rs   (now src/entrypoint/messages/mod.rs)
14 Cargo.toml
12 src/lib.rs
12 src/entrypoint/landing.rs
12 src/entrypoint/chat_completions.rs
11 src/providers/anthropic.rs
```

Ranking is churn only (no complexity). `router.rs` and `messages.rs` have since been split into directories, so the Tech Debt table paths should read `router/mod.rs` and `messages/mod.rs`. `providers/mod.rs`, `entrypoint/mod.rs`, `router` and `schema.rs` (the files this plan edits most) are all in the top four, consistent with the plan's hotspot list.

## 1.1.1g Legacy SSE consumer enumeration (HARD precondition of Task 1.1.2b)

**RESULT: PASS. Zero SSE consumers other than `ui/src/app/core/services/sse.service.ts`.** Task 1.1.2b is unblocked and Task 1.1.2g (dual-emit) stays cut-able.

Commands:

1. `grep -rnE "request_id|duration_ms|first_byte_ms|tokens_before|status_code" src tests` found 34 files (a name-only grep, not an SSE grep). Triage by category:
   - Producer of the SSE payload: `src/entrypoint/events.rs:28-33` (struct `RequestTraceData`) and `src/metrics/mod.rs:285-293` (builds it), `src/metrics/mod.rs:299` (sends).
   - `/metrics` REST consumer: `src/dashboard.rs:540-546` (legacy HTML dashboard reads `r.request_id`, `r.duration_ms`, `r.first_byte_ms` from the `/metrics` JSON; it never touches the SSE endpoint, confirmed by the next grep). Unaffected because `/metrics` stays additive.
   - Same field names, unrelated data (cost metrics, session compaction, claude_code_session, context_forensics, `src/bin/mcp-proxy/*`, `providers/mod.rs`, routing strategies, error_tracker, histogram, counters): not SSE. `src/mcp_gateway.rs` has zero hits for any of these names and for `event-stream`/`EventSource`.
2. The SSE-consumer grep: `grep -rnE "dashboard/events|EventSource|text/event-stream|DashboardEvent|dashboard_events" src tests ui/src` (both `--include` filters for `.rs/.ts/.js/.html`), minus `src/entrypoint/events.rs`. Hits grouped:
   - UI SSE consumer (the only one): `ui/src/app/core/services/sse.service.ts` (`url = '/v1/dashboard/events'` at `:32`, `new EventSource` at `:58`), consumed by `ui/src/app/core/stores/metrics.store.ts:41-52`. Specs: `sse.service.spec.ts`, `overview-dashboard.component.spec.ts` (mocks).
   - Producers (`broadcast::Sender<DashboardEvent>`): `src/metrics/error_tracker.rs:225,251,317`, `src/metrics/mod.rs:199,232,299`, `src/entrypoint/observability.rs:676`, `src/entrypoint/mod.rs:106,219`.
   - Server side route: `src/entrypoint/mod.rs:280`; the only `event_tx.subscribe()` is the SSE handler itself at `src/entrypoint/events.rs:70`. `grep -rnE "subscribe\(\)|Receiver|BroadcastStream"` over `src tests` outside `events.rs` returned nothing.
   - Rust tests that touch the endpoint (assert status/content-type only, do not parse payload fields): `src/entrypoint/mod.rs:742-756` and `tests/webui_integration_test.rs:102-112`. These keep passing across the camelCase change; they are not payload consumers.
   - `text/event-stream` hits in `providers/gemini`, `providers/openai/responses.rs`, `entrypoint/messages`, `entrypoint/chat_completions.rs`, `tests/fake_upstream_harness.rs`, `tests/support/fake_upstream.rs` are the LLM-response streaming path, unrelated to the dashboard endpoint.
3. Whole-repo text search (excluding node_modules, target, dist, .git, project_plans, src, tests) for `dashboard/events|EventSource` returned no files, so no scripts, docs or other tooling consume the endpoint.
4. `src/bin/mcp-proxy` and `src/mcp_gateway.rs` do not read the dashboard SSE (no hits above).

## Compression absence

`grep -rnE "CompressionEngine|compress_request" src --include='*.rs' | grep -v "^src/compression/\|^src/bin/"` printed nothing, so the engine is not constructed anywhere in the server. The `compressed: false` / `tokens_after: 0` values in the captured frame agree.

## Unresolved Questions owned by 1.1.1 (answers)

- (a) Raw frame snake_case? **Yes**, snake_case, no `id` key; envelope `{type, data}`; `status_code` is a hardcoded 200.
- (b) `list_models` per adapter: table above (all return a list or a `ProviderError`; Anthropic has no pagination; Bedrock and Gemini surface raw SDK/body text; any refresh can run a credential helper or AWS chain, supporting `OnDemand` default).
- (c) `resolvedModel` feasible? **Yes**, via a defaulted `Provider::resolved_model_for` accessor in 3 files; not via `ProviderResponse`.
- (e) Karma JSON fixture import: works with no tsconfig change and is type-checked; Bazel `glob(["src/**", ...])` already covers `ui/src/testing`.

## Not done / caveats

- No live `list_models` calls (no credentials in this session), so sizes and Anthropic truncation are INFERRED.
- Fake upstream was Python, not axum, and lives only in `/tmp/spike111` (not committed).
- Bazel was not run; the glob claim is from reading `ui/BUILD.bazel`.
