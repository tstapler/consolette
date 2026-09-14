---
name: proxy-error-audit
description: Audit every error currently exposed by the running consolette proxy (metrics, error summary, dashboard, logs) and fix each one that is fixable in-proxy. Use when asked to audit proxy errors, triage dashboard red states, clear /errors/summary backlogs, or investigate repeated 4xx/5xx/529 responses.
---

# Proxy Error Audit

Systematic pass over all error surfaces the proxy exposes, then fix what is
fixable in-proxy. Do not fix upstream outages or client bugs in-proxy — route
around them or report them.

## 1. Collect current state (read-only first)

Default base is `http://127.0.0.1:47000` unless `CONSOLETTE_PORT` overrides it.
Confirm the proxy is running before concluding "no errors":

```bash
curl -s http://127.0.0.1:47000/ | head -c 500
curl -s http://127.0.0.1:47000/metrics | python3 -m json.tool | head -n 120
curl -s http://127.0.0.1:47000/errors/summary | python3 -m json.tool
curl -s http://127.0.0.1:47000/api/route | python3 -m json.tool
```

Capture, per source:

- `/metrics`: `summary` (total_requests/errors/fallbacks, error_rate),
  `providers.<name>.last_error_kind`, `cooldowns.<name>` (cooling_down +
  remaining_seconds), `recent_errors` (20), `recent_requests` (20),
  `count_tokens` (failures here break auto-compaction).
- `/errors/summary`: every `AggregatedError` — fingerprint (first 8 chars is
  enough to reference), provider, error_type, count, first_seen/last_seen,
  message. Sorted by last_seen desc; work top-down.
- `/dashboard` (or its backing fields): provider dots — red
  (`status-auth-required`), purple (`status-schema-drift`), amber
  (`status-cooldown`), green (`status-active`). Color is never the only
  signal; read the text suffix too. See `src/dashboard.rs`.
- Logs (only if the HTTP surfaces are ambiguous):
  `/tmp/consolette.out.log`, `/tmp/consolette.err.log`, or
  `journalctl --user -u consolette -f` on systemd hosts.

If the proxy is not running, say so and stop — do not invent errors from
stale logs.

## 2. Classify each error

Map every fingerprint to the shared vocabulary in
`src/providers/mod.rs:35` (`ProviderError`) via `kind_label()`:

| kind | Dashboard signal | Fixable in-proxy? | First look |
|---|---|---|---|
| `validation` | 400, no cooldown | **Often yes** — request translation / schema sanitization | `src/providers/mod.rs:589` (`translate_tool_definition`, `sanitize_schema_patterns`, `sanitize_schema_cohere_subset`), provider `classify_*_error` fns, `src/entrypoint/errors.rs` |
| `response_shape_mismatch` | purple `schema drift — code fix needed`, 1800s drift cooldown | **Yes — code fix** | `src/providers/gemini/error.rs`, `DRIFT_COOLDOWN_SECS`, translation fallbacks in `src/providers/mod.rs:839` |
| `rate_limited` | amber `status-cooldown`, 429 + `Retry-After` | **Mitigate** (cooldown/routing), not eliminate | `src/routing/health.rs` (`trip`, `remaining_secs`), `Router::dispatch` failover, `POST /api/route` weights |
| `timeout` | 529 (Anthropic) / 503 (OpenAI shape) | **Mitigate** — `request_timeout` config, failover | conf.d `request_timeout`, `is_transient()` branch in `src/routing/router.rs` |
| `upstream` (5xx passthrough) | per-upstream 5xx | **Route around**, don't patch the body | `cooldown_snapshot()`, add/widen candidates |
| `exhausted` | 529/503, all candidates tried | **Yes — routing/config** | `GET /api/models`, widen route, add upstream, session pin |
| `model_unsupported` | 404 | **Yes — config** | `consolette list-models`, `POST /api/route` model overrides, `POST /api/sessions/{id}/route` pin |
| `auth` | red `needs re-auth`, never cools down | **No code fix** — credential problem | secret resolver / exec creds / expired token; re-auth, then verify dot goes green |

Rule: `auth` never trips `HealthRegistry` by design
(`src/entrypoint/observability.rs` test documents this). Do not "fix" it by
adding a cooldown. `Exhausted` (whole pool empty) is distinct from one
upstream's `Upstream{status:503}` — check which one `/metrics` reports.

## 3. Triage loop (one pass per fingerprint, highest count/recency first)

For each fingerprint:

1. **Read the normalized message.** `ErrorTracker` strips UUIDs/ARNs/hex
   (`src/metrics/error_tracker.rs:121` `normalize_message`). The fingerprint
   groups one logical error; do not chase individual IDs inside `<UUID>` /
   `<HEX_ID>` / `<MODEL_ARN>` placeholders.
2. **Find a recent request.** Correlate `recent_errors[].timestamp` with
   `recent_requests[]`, then pull the redacted original body:
   `GET /requests/{id}?stage=original`. (`stage=compressed` always 404s —
   known gap, not an error.)
3. **Reproduce at the right layer.** Prefer a unit-level repro against the
   pure function (`translate_*`, `sanitize_*`, `classify_*_error`,
   `extract_signature`) over a live end-to-end call. Enable redacted payload
   logging only if needed:
   `echo 'CONSOLETTE_LOG_BODIES=1' >> ~/.config/consolette/env`,
   restart, `journalctl ... | grep consolette::bodies`, then **turn it off
   afterward**. Secrets stay masked via `redact_bodies`; never log or paste
   raw keys.
4. **Decide fixable-in-proxy or not:**
   - Fixable: bad translation, missing fallback (e.g. empty content with
     `length` stop and no reasoning-text fallback), over-strict schema
     passthrough (Fireworks `title`/`default:null`/bad-`pattern` 400s,
     Cohere subset violations), wrong status mapping in
     `src/entrypoint/errors.rs`, missing model override, too-narrow route,
     wrong timeout/cooldown default.
   - Not fixable: expired/revoked credentials, upstream outage, client
     sending an invalid model, caller budget exceeded. For these: route
     around (fallback/weighted weights, session pin via
     `POST /api/sessions/{id}/route`), document, move on.

## 4. Fix (only fixable items)

- Keep transport thin; put logic in the testable pure function (translator,
  sanitizer, classifier), not the Axum handler. Follow existing patterns:
  degrade to well-formed output (`{}` args, empty text block) rather than
  dropping a call — dropping is what stalls agentic loops.
- Session pins are in-memory only; global route changes persist to
  `runtime-overrides.toml` via `POST /api/route` and hot-swap the router
  (losing old cooldown state — acceptable for rare admin actions).
- After each fix: `cargo fmt --all --check`, then the narrowest relevant
  `cargo test`, then `cargo clippy --workspace --all-targets -- -D warnings`.
  Per repo rules (`unwrap_used`/`expect_used` denied): no new unwraps in
  production code.
- Re-query `/metrics` + `/errors/summary`: the fixed fingerprint's `count`
  must stop growing and the provider dot must return to green. A new
  fingerprint for the same symptom means the normalization split it — check
  `normalize_message` coverage before claiming victory.

## 5. Report

End with a table, one row per fingerprint audited:

`fingerprint | provider | kind | count | verdict (fixed-in-proxy / routed-around / upstream-or-client, no proxy change) | evidence (test name, /metrics before→after, or log line)`

Plus: any `auth` items needing the user to re-auth (which upstream, which
credential), and any `response_shape_mismatch` items that need a follow-up
code change with the drifting field named.

## Boundaries

- Do not hand-edit `.github/workflows/release.yml` (regenerate via
  `dist generate`); do not push version tags; do not commit unless asked.
- Do not leave `CONSOLETTE_LOG_BODIES=1` on.
- Do not "fix" upstream or client errors by loosening validation so bad
  requests succeed silently — surface them with the correct
  `map_provider_error_{anthropic,openai}` envelope instead.
