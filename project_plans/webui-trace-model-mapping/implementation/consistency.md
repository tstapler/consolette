# Cross-artifact consistency: webui-trace-model-mapping

> **Superseded numbers (triad-review repair iteration 1, 2026-10-09).** Effort, appetite and cut-order figures in this document (for example 51.15 or 52.75 days, 43.1 days for Epics 1-5, "cuts 1-8", `compression.enabled` "undecided") are historical. The current total is **56.55 engineer-days** (Epics 1-5: 46.65; over the 45-day appetite by 11.55 days), the cut order is in `plan.md` "Effort and Appetite", and `compression.enabled` defaults to `false` (ADR-005, decided). Findings below are otherwise unchanged and were not rewritten.

Scope: `requirements.md`, `implementation/plan.md` (1031 lines), `design/ux.md` (300 lines), `decisions/ADR-001..005`. Line numbers are from the files as read on 2026-10-09; `requirements.md:27` changed mid-review (49.5 -> 51.15 days) and the later text is used. Prompt: `.claude/skills/sdd/skills/4-validate/cross-artifact-consistency-prompt.md`.

Summary: 3 BLOCKER, 10 CONCERN, 9 NITPICK. Coverage check found no gap (all five In Scope items map to Epics 1-6). Highest-severity finding: B2, three incompatible route-reason vocabularies (UX, ADR-004, plan) with no defined wire strings.

## BLOCKER

**B1. UX promises per-attempt error class plus redacted body; the plan stores neither** (ux.md vs plan.md/ADR-002)
- `design/ux.md:117,125,136,229` and UX-02 describe a selectable attempt with "error class and redacted body". `plan.md:27` ("no bodies, no error text"), `plan.md:374` (2.1.1c: `AttemptTrace { iteration, upstream, model, outcome, status, latency_ms }`), `plan.md:388` and ADR-002 item 6 ("never `to_string()`; no bodies") define no such field. No story implements selecting an attempt (ux.md:136).
- Resolution: drop "redacted body" and attempt selection from ux.md S3 and show only the `AttemptOutcome` label, status and latency; or add a sanitized `errorClass` field and a story for it.

**B2. Route-reason vocabulary differs in all three places, and wire strings are never defined** (ux.md vs ADR-004 vs plan.md)
- `design/ux.md:24`: `alias, strategy, rule, pinned, fallback, explore, default, no route`; `ux.md:296` assumes an `alias` chip.
- ADR-004 item 4: closed enum `SessionPin, Strategy, Exploration, Fallback, RateLimitHold, Default, Cache, Local`, and says alias is carried by `routedModelSource`, "not a separate reason". It has no `rule` and no `no route`.
- `plan.md:428` (2.1.4b) chips: `pinned, strategy, explore, fallback, held, default, cache, local, cancelled`; `plan.md:384` uses wire value `"held"` for `RateLimitHold`.
- ux.md:20,109 shows the matched rule name ("rule gpt-family") and `routeRule` in the S6 sample (ux.md:220); grep finds no `routeRule` in plan.md or the ADRs, so there is no data source for it.
- Resolution: pick one set. Suggested: ux adopts the ADR set, drops `alias`, `rule` and `no route` (or `no route` becomes a status-driven label), and the plan adds a table mapping enum -> wire string -> chip text (e.g. `SessionPin` -> `session_pin` -> `pinned`, `Exploration` -> `exploration` -> `explore`, `RateLimitHold` -> `rate_limit_hold` -> `held`). Add `routeRule` to the plan or remove it from ux.md.

**B3. Plan contradicts itself and ADR-005 on the status of a size-capped compression** (plan.md vs ADR-005)
- `plan.md:924` (Story 6.1.2 design step 1): size cap exceeded -> "original body, `none`, `compression_skipped_total`".
- Glossary `plan.md:41`, AC `plan.md:941`, ADR-005 item 12 and `ux.md:260`: it is `skipped`; `none` means "engine ran, nothing to save".
- Why it matters: under the Column Truth Table a `none` row reads as a real "no savings" value, the exact misreport `skipped` exists to prevent.
- Resolution: change `plan.md:924` to `skipped`.

## CONCERN

**C1. Appetite stated three ways** (requirements.md vs plan.md vs ADR-005)
- `requirements.md:27` says the approved appetite is ~9 weeks (extended by Tyler) and the plan is 51.15 days (~10.2 weeks), slightly over. `plan.md:5,143,157,175` still say "Appetite is 3-6 weeks" and "exceeds the 6-week appetite by ~4.2 weeks"; ADR-005 consequences (`ADR-005:24`) say "over the 6-week appetite". Against ~9 weeks (45 days) the overshoot is 6.15 days (~1.2 weeks), not 4.2 weeks. `plan.md:189` lists the appetite as an open question although Tyler already decided.
- Resolution: update plan.md and ADR-005 to the approved ~9 weeks and recompute the overshoot and the cut-order conclusion (`plan.md:175`) against 45 days.

**C2. Compression "flag" vs "No feature flag"** (requirements.md internal, vs plan/ADR-005)
- `requirements.md:44` ("flagged, fail-open") and `plan.md:136`, ADR-005 item 2 gate compression behind `compression.enabled`; `requirements.md:73` says "No feature flag".
- Resolution: reword `requirements.md:73` to "trace/catalog ungated; compression gated by `compression.enabled`".

**C3. Glossary count is wrong** (plan.md)
- `plan.md:55` claims "38" terms; the table (`plan.md:16-52`) has 37 rows (recounted with awk).
- Resolution: fix to 37 or add the missing term.

**C4. UX wireframe numbers break the first-byte invariant, and TTFT/TTFB/First byte are used interchangeably** (ux.md internal, vs plan glossary)
- `ux.md:49` says first byte is "never smaller than LATENCY*", but the wireframe rows (`ux.md:39-45`) show 2310/480 ms, 4120/910 ms, 1100/220 ms. The drawer (`ux.md:104`) shows Total 4120 ms, TTFT 910 ms, while the S6 sample (`ux.md:221`) has `timeToHeadersMs: 3650`, `firstByteMs: 3800`, `totalMs: 4120`. Row 2's LATENCY 4120 equals `totalMs`, not `timeToHeadersMs`.
- Naming: the column header is "TTFB" at `ux.md:38` but "First byte" at `ux.md:49`; the drawer says "TTFT" (`ux.md:104`); the plan says "First byte" (`plan.md:38,331`) and treats first-token time as a backlog item (`plan.md:876`). `plan.md:729` still carries `medianTtftMs` as the old name.
- Resolution: fix the sample numbers (e.g. row 2: latency 3650, first byte 3800), use "First byte" everywhere, and drop "TTFT".

**C5. S2 "Must" surfaces live only in cut-able stories** (ux.md vs plan.md)
- `ux.md:12-13` rates S2 (status banner, Pause/Resume) Must, plus UX-11, UX-16, UX-23. In the plan, Pause is Task 4.1.7c (`plan.md:795`, cut #1 at `plan.md:164`); the reconnect banner, Reconnect button and lag chip are Story 4.1.5 (cut #5, `plan.md:168`). Only the "reconnecting" banner is in a never-cut story (`plan.md:328`).
- Resolution: downgrade S2 to Should in ux.md, or move the banner and Reconnect into Story 1.1.4.

**C6. `system_lag` chip text does not match the payload** (ux.md vs plan.md vs code)
- `ux.md:77` shows "feed lagging (n ms) - some traces may be dropped"; `plan.md:741` shows "feed lagging - 37 events skipped". The backend payload is `{"skipped": n}` (`src/entrypoint/events.rs:76-79`), so milliseconds do not exist.
- Resolution: change ux.md:77 and UX-16 to the skipped-count wording.

**C7. Log-line contract (S7) differs from the plan, and "trace-emission drops" is reinterpreted** (ux.md, requirements.md vs plan.md)
- `ux.md:235` is `INFO ... provider= outcome= error_class= status= age_s= cached=`, with INFO for an error outcome. `plan.md:130` specifies `info!` with `upstream`, `outcome`, `model_count`, `duration_ms`, and `warn!` on failure with `kind` and `status`. Field names (`provider`/`upstream`, `error_class`/`kind`) and levels differ, and `age_s`/`cached` have no plan source.
- `requirements.md:70` and `ux.md:239` require logging trace-emission drops with a count; `plan.md:130` deliberately logs only ring-buffer evictions at `debug`.
- Resolution: choose one field set (plan's names are the implementable ones), update UX-30, and state in requirements that drop logging means evictions plus `system_lag`.

**C8. Attempt-outcome and error-class vocabulary is not shared** (ux.md vs plan.md/ADR-002/ADR-003)
- `ux.md:113-114,126` use words "overloaded", "throttled" and classes "auth, rate limit, overload, timeout, 5xx"; S4 uses "auth/timeout/5xx" (`ux.md:195`). The plan renders `x 529 upstream` and `x 429 rate_limited` (`plan.md:436`) from `AttemptOutcome` (`plan.md:28`), and the catalog uses `CatalogErrorKind` `auth|timeout|rate_limited|upstream|unsupported|other` (ADR-003 item 3). `ux.md:237` claims the log error class "matches the S4 UI labels" but neither set contains "overload" or "5xx".
- Resolution: add a label map (enum -> display text) in the plan and use it in the ux wireframes.

**C9. S6 sample field names are not the planned wire names** (ux.md vs plan.md)
- `ux.md:219-223` uses `model`, `provider`, `routeRule`, `attemptCount`, `finalStatus: "fell_back"`, and omits `id`, `routedModelSource`, `attempts`, `attemptsTruncated`. The plan's contract is `requestedModel`, `routedModel`, `routedModelSource`, `routedProvider`, `routeReason`, `attempts`, `attemptsTruncated`, `iterations`, `id` (`plan.md:246,400`). `fell back` / `retried` has no wire field in the plan.
- Resolution: regenerate the S6 sample from `request-trace-failure.json` once it exists, or mark it as indicative; state how the `fell back` / `retried` sub-label is derived (attempts and final status).

**C10. Models page promises per-model config markers the plan does not deliver** (ux.md vs plan.md)
- `ux.md:163,166,160` show `used in: rule "premium", alias "auto"`, per-model "in config / not in config", and an `[All | Config-only | Errors]` filter. Plan 3.1.5/3.1.6 gives a warning block with `referencedBy: "route <name> upstream 0"` (`plan.md:615`), covering the primary route only (`plan.md:613`), plus search; no per-model markers, no filter toggles, no alias lookup. UX-04 is met only for pinned primary-route models.
- Resolution: trim ux.md S4 to the plan's scope, or add a story.

## NITPICK

- **N1. Request ID shape.** `plan.md:21` says UUID; plan fixtures use `req_123`, `req_9f8e7d6c` while the aria-label uses `9f8e7d6c` (`plan.md:434,780`), which is not the 8-character prefix of `req_9f8e7d6c`; ux shows `a1b2c3d4` / `9f8e7d6c-...`. Use UUID-shaped fixture ids.
- **N2. S5 filter/search (UX-33) has no story.** `plan.md:876` backlogs it; ux.md keeps it as Should with UX-33. Mark UX-33 deferred.
- **N3. Drawer tabs and misc. UI states unplanned.** Pipeline/Payload-diff tabs and "Payload not captured" (`ux.md:119-121,149`); table skeleton rows (UX-17), "Showing latest 100" footer (UX-32), copy-ID "Copied" announcement (UX-03/22) appear nowhere in plan ACs (grep: no `skeleton`, `Showing latest`, `Copied`). `plan.md:2.1.5d` explicitly avoids embedding the pipeline timeline.
- **N4. Lazy-attempt states unreachable.** ux.md S3 "Loading (lazy attempt fetch)" / "Fetch failed" (`ux.md:144-145`, UX-17) imply a trace endpoint; the plan inlines attempts in the event, so those states never occur. Remove or mark conditional.
- **N5. Empty-state copy.** `ux.md:72` (URL + copy button) vs `plan.md:328` ("Point a client at this proxy").
- **N6. Route path.** ux `/dashboard/models` vs plan `/models` (`plan.md:591`); equivalent under base-href `/dashboard/` but unstated.
- **N7. `ifStale` not in the UX flow.** `ux.md:182` step 1 does not mention the automatic `POST ...refresh?ifStale=true` on open (ADR-003 item 1a, `plan.md:585`), though the S4 help text must disclose credential flows.
- **N8. Plan-only UI.** "3 search rounds" badge and "Round N" grouping (`plan.md:421,436`), Recent errors list (`plan.md:745`) and `GET /v1/dashboard/traces` backfill (`plan.md:761`) have no requirement or ux counterpart; they fall under the generic "fix highest-value gaps" scope item and Story 4.1.0 re-rank.
- **N9. ADR wording.** ADR-003 item 2 lists `UpstreamModels` without `state` and `refreshing` (glossary `plan.md:45` has them); ADR-005 item 11 gives `apply_compression -> (body, outcome)` then `(Option<Value>, outcome)` in the same item (plan uses the latter).

## Verified consistent (no finding)

- `compressionStatus` values `off|none|skipped|failed|compressed`: glossary, 1.1.2, 1.1.4, 6.1.2-6.1.3, ADR-005, `ux.md:221,260` (except B3).
- `traceKind` `routed|cache_hit|local`; `firstByteState` `pending|measured|ended_empty|client_closed`; the six `n/a (...)` strings in UX-07 match the Column Truth Table (`plan.md:61-71`).
- Requested/routed model definitions: ADR-004 items 1-2, glossary, 2.1.3 and 2.1.4 agree; `unknown` is surfaced as missing.
- Endpoints: `GET /api/model-catalog`, `POST /api/model-catalog/refresh[?upstream=|?ifStale=true]`, legacy `/api/models` untouched, `ModelCatalogChanged` event; identical in ux, ADR-003 and plan.
- Stale threshold (30 min on-demand, 2 x interval in interval mode) and 300 s retry floor: `ux.md:176,298`, ADR-003 items 1a and 6, `plan.md:188`, 3.1.3.
- Effort arithmetic: epic sums, 51.15 total, cut-order remainders (down to 33.0) and schedule weeks recomputed and correct; `requirements.md:27` now matches 51.15.
- Error-rate definition (`statusCode >= 400 && != 499`, cache hits excluded): ADR-002 item 9, 2.1.6, 4.1.2.
- Requirements coverage: contract fix/status/one-trace -> Epics 1 and 2.1.6; compression and first byte -> Epic 6; requested/routed/reason/attempts -> Epic 2; model listing -> Epic 3; exploratory report -> Epic 5 and 4.1.0.
