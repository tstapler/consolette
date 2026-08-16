# UX Design: compaction-cost-metrics

Treatment: **condensed** — both surfaces are non-interactive, operator-facing (CLI text/JSON, HTTP JSON, log lines). No wireframes/flows needed.

---

## Step 1 — Surfaces

1. **CLI subcommand** — `consolette cost-report <session-key>` (text table) and `consolette cost-report <session-key> --json` (pretty JSON), per plan.md Epic 3.1 / Task 3.1.1a-d.
2. **HTTP JSON endpoint** — `GET /v1/cost/{session_key}` on the `consolette serve-cost` bootstrap server, per plan.md Epic 2.3 / 3.2.
3. **Pricing-fallback log line** — `tracing::warn!` emitted whenever the live pricing lookup fails and the tracker falls back to the static table (requirements.md Observability Requirements; plan.md Observability Plan, `src/cost_metrics/pricing.rs`), paired with the `cost_metrics_pricing_fallback_total` counter.

Both CLI and HTTP surfaces render the same `CostReport` value (plan.md Task 1.3.3a) — this is the mechanism, not just an intent, behind "the two surfaces agree."

---

## Step 2 — Condensed treatment per surface

### Surface 1: CLI — `consolette cost-report s1`

Sample output (`Reconciled`, `Full`-tier session, resolvable price):

```
$ consolette cost-report s1
session_key: s1
pricing_source: static

actual_tokens:            4,000  (exact)
actual_cost_usd:          $0.01  (static pricing)
counterfactual_tokens:    12,000  (estimated via AnthropicCountTokensApi)
compacted_tokens:         4,200  (estimated via AnthropicCountTokensApi)
tokens_saved:             7,800  (65.0%)
estimated_cost_saved_usd: $0.02  (static pricing)

by_tier:
  Full  counterfactual=12,000  compacted=4,200  saved=7,800 (65.0%)  actual=4,000

pending: 0   abandoned: 0
$ echo $?
0
```

`tokens_saved` is `counterfactual_tokens − compacted_tokens` — both figures come from the same `TokenEstimator` call (one run pre-compaction, one post-compaction), so the subtraction is unit-valid by construction (plan.md Story 1.3.3). `actual_tokens`/`actual_cost_usd` are the real, provider-billed figures (`TokenSource::Exact`), reported separately — they are what the operator is actually charged and a sanity check that `compacted_tokens` is in the right ballpark, never an operand of `tokens_saved`.

Sample output (`Pending` — compacted, request not yet completed):

```
$ consolette cost-report s2
session_key: s2
pricing_source: static

actual_tokens:            unavailable — request did not complete
actual_cost_usd:          unavailable — request did not complete
counterfactual_tokens:    9,800  (estimated via TiktokenO200k)
compacted_tokens:         unavailable — not yet reconciled
tokens_saved:             unavailable (not yet reconciled)
estimated_cost_saved_usd: unavailable (not yet reconciled)

pending: 1   abandoned: 0
$ echo $?
0
```

Sample JSON (`--json`, same `s1` session):

```json
{
  "session_key": "s1",
  "actual_tokens": 4000,
  "actual_source": "Exact",
  "actual_cost_usd": 0.01,
  "counterfactual_tokens": 12000,
  "counterfactual_source": { "Estimated": { "via": "AnthropicCountTokensApi" } },
  "compacted_tokens": 4200,
  "tokens_saved": 7800,
  "estimated_cost_saved_usd": 0.02,
  "pricing_source": "Static",
  "pending_count": 0,
  "abandoned_count": 0,
  "by_tier": [
    { "tier": "Full", "actual_tokens": 4000, "counterfactual_tokens": 12000, "compacted_tokens": 4200, "tokens_saved": 7800 }
  ]
}
```

Sample error (unknown session):

```
$ consolette cost-report ghost
error: no session found for key "ghost"
$ echo $?
1
```

**Acceptance criteria — CLI**
- `exact` vs `estimated` (and, within `estimated`, which `EstimatorKind`) is stated as a text suffix on every token figure (`(exact)`, `(estimated via …)`) — never conveyed by color alone, and still legible under `NO_COLOR=1` or when piped to a file.
- The three states never collapse into each other in text output: never-compacted/zero-savings prints `tokens_saved: 0 (0.0%)` as a real number; compacted-but-incomplete prints the literal string `unavailable — request did not complete` for `actual_tokens` and omits `tokens_saved`/`estimated_cost_saved_usd` rather than printing `0`; unknown session prints nothing but the `error:` line and exits non-zero.
- `--json` output is byte-for-byte `serde_json::to_string_pretty(&report)` on the same `CostReport` value the table is rendered from — no separate JSON-construction code path, so table and JSON cannot drift.
- A missing price renders as the literal string `unavailable` (never `$0.00`) for `estimated_cost_saved_usd`.
- A human operator can answer "did compaction save money" from the `tokens_saved`/`estimated_cost_saved_usd` lines alone, without reading `by_tier` or JSON — the top-level summary is sufficient for the one-glance check named in ux.md job 2.

### Surface 2: HTTP — `GET /v1/cost/{session_key}`

Sample request/response (same `s1` session):

```
GET /v1/cost/s1 HTTP/1.1
Host: 127.0.0.1:8787
```
```json
HTTP/1.1 200 OK
Content-Type: application/json

{
  "session_key": "s1",
  "actual_tokens": 4000,
  "actual_source": "Exact",
  "actual_cost_usd": 0.01,
  "counterfactual_tokens": 12000,
  "counterfactual_source": { "Estimated": { "via": "AnthropicCountTokensApi" } },
  "compacted_tokens": 4200,
  "tokens_saved": 7800,
  "estimated_cost_saved_usd": 0.02,
  "pricing_source": "Static",
  "pending_count": 0,
  "abandoned_count": 0,
  "by_tier": [
    { "tier": "Full", "actual_tokens": 4000, "counterfactual_tokens": 12000, "compacted_tokens": 4200, "tokens_saved": 7800 }
  ]
}
```

Sample error response:

```
GET /v1/cost/ghost HTTP/1.1
```
```json
HTTP/1.1 404 Not Found
Content-Type: application/json

{ "error": "session_not_found", "session_key": "ghost" }
```

**Acceptance criteria — HTTP**
- Response body is the same `CostReport` serialization the CLI's `--json` mode prints — one `Serialize` impl, two call sites (plan.md Story 3.2.1), so field names/types/nesting are identical across surfaces by construction.
- `actual_tokens`/`compacted_tokens`/`tokens_saved`/`estimated_cost_saved_usd`/`actual_cost_usd` are JSON `null` (never `0`) when the value is unknown (pending reconciliation) or unpriced — a consumer doing `if (tokens_saved > 0)` cannot mistake `null` for `0` in any statically-typed JSON consumer, since the field is absent of a numeric type in that case.
- `actual_source`/`counterfactual_source`/`pricing_source` are always present as explicit tagged values (`{"Estimated":{"via":"..."}}` / `"Exact"` / `"Static"` / `"Live"`), or `null` when the paired figure itself is unknown — a client never has to infer exactness from the presence/absence of another field.
- 404 body always includes both `error` and `session_key` so a script can log which key it queried without re-parsing the request.
- One `GET` returns everything needed to answer "did compaction save money for this session" — no follow-up call required.

### Surface 3: Pricing-fallback log line

Sample log output:

```
2026-08-15T14:02:11Z WARN cost_metrics::pricing: pricing table live refresh failed, falling back to static table model=claude-sonnet-5 reason="request timed out after 2s"
```

**Acceptance criteria — logging**
- Emitted exactly once per fallback event (not once per request affected by the stale price) so log volume doesn't scale with traffic.
- Includes the reason (`timeout`, `non-200`, `malformed JSON`) as structured text, not just "failed," so an operator can distinguish a transient network blip from a permanently broken upstream JSON shape.
- Paired with the `cost_metrics_pricing_fallback_total` counter increment in the same code path, so a metrics dashboard and a raw log grep never disagree about how many fallbacks occurred.

---

## Step 3 — Testable UX acceptance criteria (Given-When-Then)

Using plan.md's Domain Glossary field/type names (`CostReport`, `ReconciliationStatus`, `TokenSource`, `PricingSource`, `SessionNotFound`).

**Task completion in fewest steps**
1. *Given* a session key the operator already knows, *When* they run `consolette cost-report <key>`, *Then* one CLI invocation returns `actual_tokens`, `counterfactual_tokens`, `compacted_tokens`, `tokens_saved`, `estimated_cost_saved_usd`, and `actual_cost_usd` together — no second command or flag is required to see all the figures.
2. *Given* the same session key, *When* a script issues one `GET /v1/cost/<key>`, *Then* the JSON response contains the same fields in one round trip — no pagination, no second lookup for pricing metadata (`pricing_source` is inline).

**Error message + corrective action**
3. *Given* `session_key = "ghost"` has never been recorded by `CostTracker::record_pending` (i.e., `report_for_session` returns `Err(CostReportError::SessionNotFound)`), *When* the operator runs `consolette cost-report ghost`, *Then* stderr reads exactly `error: no session found for key "ghost"` and the process exits non-zero — the corrective action (check the session key spelling, or confirm the session has actually gone through `SessionCompactionPipeline::apply` at least once) is inferable directly from the message without consulting `--help`.
4. *Given* the same `SessionNotFound` condition, *When* a client issues `GET /v1/cost/ghost`, *Then* it receives `404 {"error":"session_not_found","session_key":"ghost"}` — the corrective action is the same as (3), and the `session_key` echo lets an automated caller confirm it queried the key it intended to.
5. *Given* a `Reconciled` `CostRecord` whose `model` has no entry in `PricingTable` (`price_for` returns `None`), *When* `report_for_session` is called, *Then* `estimated_cost_saved_usd` is `None`/`null` and the CLI table renders the literal text `unavailable` for that line — the corrective action ("add a pricing override for this model") is stated in the accompanying `pricing_source` context, not fabricated as `$0.00`.

**No dead ends**
6. *Given* a session with only `Pending` `CostRecord`s (`ReconciliationStatus::Pending`, no `record_actual_usage` call has landed yet), *When* `report_for_session` is called, *Then* `actual_tokens == None` and the CLI prints `unavailable — request did not complete` rather than erroring or silently omitting the session — the operator's next action ("re-run once the request completes") is stated inline, not left implicit.
7. *Given* a session with only `Reconciled` `CompactionTier::Off` records (compaction never elided anything), *When* `report_for_session` is called, *Then* it returns `Ok(report)` with `tokens_saved: Some(0)` — not an error and not an omitted row — so the operator sees "compaction ran, saved nothing" as a legible, actionable state (job 2 in research/ux.md: catch a misconfigured tier) rather than a dead end indistinguishable from "no data."
8. *Given* a session whose cache entry has TTL-expired (`SessionCostStore` eviction), *When* `report_for_session` is called, *Then* it returns `Err(SessionNotFound)` — identical in shape to a session never seen at all, so the operator's next action is unambiguous ("re-run compaction to regenerate data") and is never confused with the zero-savings state in (7).

**Accessibility (no color-only signaling; self-describing JSON)**
9. *Given* CLI table output piped to a file or viewed with `NO_COLOR=1`, *When* the operator reads it back, *Then* every token figure still carries its exact/estimated distinction as literal text (`(exact)`, `(estimated via AnthropicCountTokensApi)`) — removing ANSI color loses zero information.
10. *Given* the JSON response body alone (no accompanying CLI `--help` text), *When* a new consumer inspects it, *Then* `counterfactual_source: {"Estimated":{"via":"TiktokenO200k"}}` vs `"Exact"` and `pricing_source: "Static"` vs `"Live"` are self-describing enum tags — a reader does not need external documentation to know which fields are authoritative versus approximate.

---

## Cross-surface consistency

- CLI `--json` and HTTP `GET /v1/cost/{session_key}` MUST serialize the identical `CostReport` Rust value for the same session state (plan.md Task 3.2.1d integration test is the executable guarantee of this — this UX doc's "surfaces agree" criteria are satisfied by that test passing, not by independent formatting code in each surface).
- Any new field added to `CostReport` in the future must appear in both the CLI table and the JSON body in the same release — no field should exist in JSON only or table only, since the requirements.md success metric depends on structural parity.
