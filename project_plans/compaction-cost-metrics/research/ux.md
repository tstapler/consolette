# UX Research: compaction-cost-metrics

Scope: operator-facing CLI subcommand + JSON API endpoint only. No GUI/dashboard concerns considered.

## Codebase conventions observed

- `src/main.rs` CLI is bare `clap::Subcommand` + plain `println!` — no table-formatting crate (`comfy-table`, `tabled`, `prettytable`) is in the dependency tree today. Adding one is a real decision, not a given (see Recommendation 1).
- The one existing precedent for structured CLI output is `src/bin/mcp-proxy/cli.rs:181` — `println!("{}", serde_json::to_string_pretty(&info)?)`. That's the house style for "give me JSON" output: pretty-printed `serde_json`, not a bespoke serializer.
- No existing HTTP JSON API endpoints were found under `src/` for this binary (axum/Json usage in the repo belongs to `metrics`, `learn`, `memory`, and the separate `cmdcrush`/`mcp-proxy` binaries) — so there's no existing endpoint-shape convention to match; this feature sets the precedent for consolette's own admin API.

## 1. Comparable CLI UX patterns for cost/before-after data

- **AWS Cost Explorer CLI / Cost and Usage Reports**: presents cost as a time-bucketed table with a running total row; always tags the granularity and currency explicitly in a header line rather than per-row, to avoid repetition. Estimates (vs. finalized billing) get an explicit `Estimated: true` field in JSON and a footnote/asterisk in table view — never a bare number that looks final.
- **`ccusage`-style community tools (Claude Code token usage)**: the dominant pattern is a single table with columns `Date | Model | Input | Output | Cache Read | Cache Write | Cost`, plus a `Total` footer row. They lean on cost being *derived* (tokens × known per-model rate) and are transparent that it's an estimate based on published pricing, not a billed invoice.
- **`litellm` cost tracking**: emits both a running `spend` field in JSON (exact, from real usage) and per-call cost estimates; the two are never merged into one field — they're separate keys (`spend` vs `response_cost`) so a consumer can tell which is authoritative.
- **`docker stats` / `kubectl top`**: live resource comparisons use fixed-width columns with consistent units (single unit per column, not mixed), and never color-code the *only* signal — color is decoration on top of text that already states the value.
- **Takeaway for this feature**: use a table with one row per session (or one row per metric if only one session is queried) and explicit column headers that name the source (`Actual Tokens`, `Est. Tokens (no compaction)`, `Est. Savings`, `Est. $ Saved`), plus a summary/total line when multiple sessions are shown. Do not merge actual and estimated into a single ambiguous "tokens" column.

## 2. Operator mental model for "how much did compaction save me"

Operators asking this question want, in priority order:
1. **Dollar amount** — the bottom-line answer to "was this worth it," since token counts alone don't map cleanly to spend across tiers/models.
2. **Percentage saved** — needed to judge whether a tier change is *proportionally* significant (saving 2% isn't worth tuning risk; saving 40% is).
3. **Absolute tokens** — the underlying evidence, useful for someone tuning `TierThresholds` who thinks in context-window budget, not dollars.

All three should be presented together, not asked for separately — an operator comparing tiers wants dollars and percentage in the same glance without recomputing by hand. Recommended shape per session:
```
actual_tokens: 12,400          (exact)
counterfactual_tokens: 41,200  (estimated)
tokens_saved: 28,800 (69.9%)
estimated_cost_saved_usd: $0.43
```
Uncertainty should be conveyed **once per number**, not repeated as prose: a short suffix/flag (`(estimated)` in text output, `"counterfactual_is_estimated": true` in JSON) beats a paragraph disclaimer. Put the caveat close to the number it qualifies — an operator skimming a table of ten sessions won't scroll to a global footnote.

## 3. Terminal-output accessibility equivalent

No WCAG applies to a CLI/JSON tool, but the equivalent failure mode is **color-only signaling that disappears under `--no-color`, piping, `| less`, CI logs, or `NO_COLOR=1`**. Concretely:
- If exact-vs-estimated is ever color-coded (e.g. green for actual, yellow for estimated) in table output, it must *also* carry a textual marker (`*`, `(est.)`, a labeled column) so the distinction survives non-TTY output. Consolette should default to text markers and treat color as a bonus, following the `NO_COLOR` convention already common in Rust CLI tools (`clap`/`anstream` respect it automatically if used).
- JSON output needs no color; the flag (`counterfactual_is_estimated: bool` or a `token_source: "exact" | "estimated"` enum) is the accessibility-equivalent guarantee there — always present, never inferred from formatting.

## 4. Error states

| Condition | CLI behavior | API behavior |
|---|---|---|
| Session key doesn't exist | Non-zero exit, clear stderr message (`error: no session found for key "<key>"`) | 404 with `{"error": "session_not_found", "session_key": "..."}` |
| Session exists but has no compaction history yet (never compacted) | Print the session with actual == counterfactual (savings = 0), not an error — compaction not yet triggered is a valid, informative state, not a failure | 200 with `tokens_saved: 0`, `compaction_applied: false` (or similar) so a naive "did it save money" check doesn't misread absence as error |
| Counterfactual-but-no-actual data (compacted but request never completed) | Show counterfactual figure with actual explicitly `null`/absent and a note (`actual: unavailable — request did not complete`), not a fabricated zero | 200 with `actual_tokens: null`, and do not compute/emit `tokens_saved` or `estimated_cost_saved_usd` when the actual side is missing — a null minus a number is not zero savings, it's "unknown" |

General principle: never let "no data" collapse into "zero" — zero is a specific, meaningful value (compaction ran and saved nothing) and must stay distinguishable from "we don't know."

## 5. Job-to-be-done

Two distinct jobs, both worth naming explicitly since they lead to different emphasis in output:
1. **Prove a tuning change worked** — after adjusting `TierThresholds`, the operator wants before/after evidence that a *specific* change reduced cost, ideally comparable across two time windows or two synthetic sessions (this maps directly to the Full-vs-Off synthetic-session success metric in requirements.md). Output should make cross-session/cross-run comparison easy — stable column order, sortable-by-savings output, consistent units.
2. **Catch a misconfigured tier before it costs real money** — a monitoring/spot-check job: operator periodically runs the CLI (or scrapes the JSON endpoint) to sanity-check that compaction is actually engaging and not silently disabled or misfiring (e.g. `Off` tier applied where `Full` was intended). This job needs the *absence* of savings to be loud and legible, not buried — a session showing `tokens_saved: 0` when a high tier was configured is the actionable signal, so it should not look identical to "no compaction ran" (ties back to error-state distinction above: compacted-but-zero-savings vs never-compacted must be distinguishable).

## Recommendations summary

1. **Table crate decision**: since no table crate exists yet, either add a minimal one (`comfy-table` is common and low-dependency) or hand-format aligned columns with `format!` — proportionate to a CLI that likely lists at most tens of sessions, not hundreds. Follow the existing `serde_json::to_string_pretty` precedent for the `--json`/API-mirroring path so CLI and API can share a serialization type and structurally cannot disagree (satisfies the requirements.md "two surfaces agree" success metric almost for free).
2. Always emit both the number and its confidence tag adjacent to each other, never color-only.
3. Never silently coerce missing actual/counterfactual data into 0 — use `null`/`Option::None` end to end and surface it as a distinct state in both table and JSON output.
