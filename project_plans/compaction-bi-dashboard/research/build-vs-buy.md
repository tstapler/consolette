# Build vs. Buy: compaction-bi-dashboard

Scope per `requirements.md`: a native-compaction parser plus a local, read-only, single-operator BI table served from the existing `serve-cost` axum process — no new dependency, no external service, no build step (Constraints; Alternatives Considered).

## 1. Sortable/filterable table: vanilla JS vs. a table library

**Hand-rolled vanilla JS (~100-200 lines)**
- Pros: zero dependency, zero supply-chain surface, trivially inlined into the single static HTML page the requirements call for; full control over the exact two filter dimensions needed (project/path substring, compaction-status enum) and the "native-reported vs. consolette-estimated" confidence-indicator rendering, which is bespoke to this dataset and not something a generic table library models; ~1,135 rows is trivial for `Array.sort`/`.filter` with no virtualization needed.
- Cons: someone has to write and test click-to-sort (asc/desc toggle, numeric vs. string comparators) and substring filtering by hand — a known, small, well-understood amount of code.
- Verdict: **Recommended.**

**Single-file vanilla-JS table library (e.g. `simple-datatables`, `list.js`, `Tabulator` in its single-script build)**
- Pros: sort/filter/pagination come free; some (Tabulator) also give column-resize and CSV export.
- Cons: even the "no build step" single-file builds are non-trivial (`list.js` ~10-15 KB min, `simple-datatables` ~30-50 KB, `Tabulator` core ~180 KB+) for a feature that needs two filters and one sort behavior; every one of these ships as a versioned artifact that has to be vendored into the repo (since a CDN `<script src>` is a non-starter here — the requirements' "no new frontend dependency" and the loopback-only/no-auth security posture (Non-functional Requirements) both argue against a local tool phoning out to a CDN for its own UI, and an offline/airgapped operator run would break entirely); vendoring means committing a third-party minified blob to a dependency-light single-crate Rust repo, plus a vendoring/update story that doesn't otherwise exist in this codebase; the library's generic sort/filter API would still need custom glue for the "compaction-status" categorical filter and the confidence-indicator styling, eroding most of the "buy" savings anyway.
- Verdict: **Not recommended** — the vendoring/CDN trade-off costs more than it saves for a two-filter, one-sort table.

## 2. SaaS / managed API

Not applicable. Every requirement in scope reads local `~/.claude/projects/**/*.jsonl` files containing the operator's own private conversation transcripts (Data Residency: "not applicable — purely local filesystem reads, no new data leaves the process"; Security classification: "internal/operator-only tooling"). There is no cost-estimation, storage, or rendering task here that benefits from a hosted API, and sending session transcripts to any third party would violate the stated data-residency and security posture outright.
- Verdict: **Not recommended / not applicable.**

## 3. Hand-written serde parsing vs. a generic "Claude Code transcript parser" crate

- The repo already has an established, working pattern for exactly this shape of problem: `boundary.rs`'s `is_compacted()`/`extract_compaction_metrics()` deserialize the `consoletteCompact` marker via `serde_json` into a typed `CompactionMetrics` struct, treating fields as optional and skip-on-mismatch rather than panicking (confirmed by direct read of `src/claude_code_session/boundary.rs`, e.g. `row.fields().extra.get("consoletteCompact")?.as_object()?` chains that no-op on absence).
- `compactMetadata`/`compact_boundary` is a Claude Code CLI-internal, undocumented, versioned-by-the-CLI format (Rabbit Holes: "shape may vary across Claude Code versions" — treat every field optional/best-effort, skip-and-log). There is no public spec for it, so any third-party crate claiming to parse it would itself be reverse-engineered from the same undocumented source consolette has direct access to, with no more authority and an extra dependency's worth of version-drift risk on top. A cursory knowledge check turns up no crates.io crate for "Claude Code transcript" or "compactMetadata" parsing — unsurprising, since this is an undocumented internal format of a single vendor's CLI, not a public protocol.
- A generic dependency would also need to match this repo's specific error-handling contract (best-effort, skip-and-log per session, never abort the whole aggregation — Rabbit Holes) — a constraint a generic library is unlikely to have designed around, since consolette's needs here are unusually specific (partial/optional field tolerance across silently-changing vendor internals).
- Verdict: **Recommended: hand-written serde struct + custom parser, following the existing `CompactionMetrics`/`extract_compaction_metrics` pattern**, adding a sibling `NativeCompactionMetrics` type with all fields `Option<T>` and the same skip-and-log discipline. This is strictly safer and more consistent with the codebase than adopting an unproven, likely-nonexistent crate for an undocumented format.

## 4. Fork/adapt an existing LLM observability dashboard (LangSmith, Helicone, etc.)

- Pros: these platforms already do sortable/filterable multi-run tables, cost estimation, and token accounting at scale.
- Cons: all of them are built around a fundamentally different shape of problem — ingesting live traces from an SDK/proxy into a hosted (or self-hosted, but still service-oriented) backend with its own datastore, auth model, and UI framework (React-based in every case surveyed). Consolette's requirement is the opposite: a single read-only pass over already-on-disk `.jsonl` files, in-memory only, no persistent datastore (Constraints: "No new persistent datastore"), no auth, single operator, single static-per-request fetch (Out of Scope explicitly excludes "a general-purpose charting/BI framework ... authentication, or multi-user access"). Adopting any of these means bringing in a database, a server framework mismatch with the existing axum/serde-only Rust stack, a JS build pipeline, and an ingestion model consolette doesn't have (there's no live trace stream — it's post-hoc `.jsonl` scanning) — none of which this feature needs and all of which the requirements explicitly rule out (Alternatives Considered: "A full frontend framework (React/etc.) ... rejected as unnecessary weight").
- The actual UI need — one HTML table, two filters, N sortable columns — is a small fraction of what any of these platforms provide; forking one would mean deleting far more than would be kept.
- Verdict: **Not recommended** — architecturally mismatched and scope-inverted (whole platform vs. one local read-only table) relative to what's actually required.

## Summary

| Decision point | Recommendation |
|---|---|
| Table UI | Hand-rolled vanilla JS (~100-200 lines), inlined in the served HTML page |
| SaaS/managed API | Not applicable — 100% local, private data |
| Transcript/metadata parsing | Hand-written serde struct, extending the existing `CompactionMetrics`/`extract_compaction_metrics` pattern in `boundary.rs` |
| Fork an observability platform | Not recommended — scope and architecture mismatch |

Net: this feature should be entirely build, no buy — every "buy" option either violates an explicit constraint (no CDN/new dependency, no new datastore, no external service) or is scope-inverted relative to the actual ask (one local table vs. a whole platform).
