# UX Design: openai-model-resolution

No interactive end-user UI. Three non-interactive surfaces: the conf.d
config field an operator writes, the dashboard status-pill extension an
operator reads, and the `/metrics` JSON shape a script or human reads.
Condensed entries per `sdd:3-plan` non-interactive-surface convention —
one representative sample + acceptance criteria each, no wireframes.

## Surface 1: conf.d config field (`model_family`)

Operator-authored TOML on a `RouteUpstreamRef`, additive alongside the
existing `model` field (per requirements.md Risk Control — opt-in, no
behavior change for configs that don't set it).

```toml
[[routes.upstreams]]
name = "model-gateway"
kind = "openai"
base_url = "https://model-gateway.example.net/v1"
# Either `model` (existing, static pin) OR `model_family` (new, dynamic) —
# setting both is a config validation error, not a silent precedence rule.
model_family = "gpt-5-codex"   # prefix match against /v1/models ids, newest-first
```

Acceptance criteria:
- A config with only `model` set (today's shape) validates and behaves identically to pre-feature consolette — zero-diff regression.
- A config setting both `model` and `model_family` on the same upstream ref fails validation with an error naming both fields and the upstream, not a generic "invalid config" message.
- A config setting `model_family` to a prefix that matches zero ids in a later `/v1/models` response fails closed at resolution time (exhausted state, Surface 2/3), not by silently falling back to the literal `model_family` string as a model id.
- The field is documented in `references/conf.d/` next to `model` with one example of each (static pin vs. family), so an operator copying the reference file sees the opt-in choice explicitly, not just the new field in isolation.

## Surface 2: dashboard status pill (`src/dashboard.rs`)

Extends the existing per-upstream status bar (`loadMetrics()`,
[src/dashboard.rs:358-376](https://github.com/tstapler/consolette/blob/main/src/dashboard.rs#L358-L376))
rather than adding a new widget — same `cls` ternary, one more branch.

```
● Model-gateway                              (green, status-active — resolved to newest, no suffix)
● Model-gateway (using gpt-5, newest unavailable)   (amber, status-cooldown-style — fallback active)
● Model-gateway (all candidates failed — check gpt-5-codex family)  (red, status-resolution-exhausted)
```

Acceptance criteria:
- An operator can tell, from the status bar alone with no click and no log read, whether a `model_family` upstream is on its newest candidate, on a fallback, or exhausted — the three states map to three visually distinct treatments (no special mark / amber / red), matching the existing green-cooldown-amber-red vocabulary the pill already uses so an operator doesn't have to learn a new color meaning.
- The fallback state (amber) never uses the same red class as `status-auth-required`/`status-schema-drift` — collapsing "healthy but not newest" into the same visual weight as "broken" is the single failure mode research.ux.md §3 calls out as unacceptable (either alert fatigue or a hidden outage).
- The exhausted state is sticky: it does not clear on the next poll interval just because a request happened to succeed on a stale cache; it only clears when `set_last_error_kind`-equivalent logic observes an actual successful resolution, consistent with the self-healing (never blind-timer) rule already documented at [src/metrics/counters.rs:24-30](https://github.com/tstapler/consolette/blob/main/src/metrics/counters.rs#L24-L30).
- Exhausted state's label names the family (e.g. "check gpt-5-codex family") so the operator's next action is visible on the pill itself — no dead end requiring a separate lookup to find which family died. This is asserted directly in plan.md Story 5.2.2's acceptance criteria (not left implicit here only).
- A `model`-pinned (non-family) upstream's pill is byte-for-byte unchanged — no new suffix, no new class — confirming the extension is additive.

## Surface 3: `/metrics` JSON shape

Per-upstream detail one level below the glance surface (research.ux.md
§2 point 3) — the dashboard renders only the current rollup.

```json
{
  "providers": {
    "model-gateway": {
      "last_error_kind": null,
      "resolved_model": "gpt-5",
      "resolution_state": "fallback",
      "resolution_family": "gpt-5-codex"
    }
  }
}
```

Fields are flat, top-level siblings on the provider object (matching plan.md
Story 5.2.1's committed shape — chosen over an earlier draft that nested
these under a `"resolution": {...}` sub-object, to avoid two incompatible
specs of the same wire shape and because the flat fields already have
committed, specific acceptance criteria and tests). `resolved_model`,
`resolution_state`, and `resolution_family` are present only for upstreams
with `model_family` configured; absent (not `null`) for static-pin upstreams
— see Story 5.2.1's acceptance criteria for the absent-vs-present contract.

Per-candidate attempt history (which candidates were tried, in what order,
with what classified failure reason) is intentionally **not** embedded in
this per-upstream rollup — the plan's Domain Glossary already commits to
`ProbeAttempt` not being persisted beyond a counter increment (only the
cache's current winner is remembered). That history is available instead
via the existing `resolution_attempts_total{upstream, family, candidate,
outcome}` counter series (plan.md Story 5.1.1, extended with a distinct
`outcome="other"` label per pre-mortem.md P1 #1) — an operator or script
can already distinguish "this candidate is dead" from "that was a blip" by
querying that counter, without a duplicate array shape here.

Acceptance criteria:
- `resolved_model`/`resolution_state`/`resolution_family` are absent for upstreams that use a static `model` pin — a script that doesn't know about this feature yet sees no shape change on existing upstreams.
- `resolution_state` is one of exactly `"newest" | "fallback" | "exhausted"` (matching the dashboard's three visual states one-to-one) — no fourth undocumented value a consumer has to guess about.
- `resolution_family` lets a script identify which family an exhausted/fallback upstream is resolving, without needing a separate config lookup — this is the JSON-side half of Surface 2's "no dead end" rule (the family name is also what the dashboard pill's exhausted label surfaces).
- A candidate's classified failure reason is queryable via `resolution_attempts_total`'s `outcome` label (`success`/`advance`/`retry_responses`/`transient`/`other`/`exhausted`), not via a field on this per-upstream object.

## Cross-surface UX acceptance criteria

1. An operator can tell from the dashboard alone, without reading logs, whether resolution picked the newest model or a fallback (Surface 2, states `newest` vs `fallback`).
2. No dead ends: an exhausted-candidates state always has a visible next action — which family/upstream to check — surfaced both on the pill label (Surface 2, Story 5.2.2) and in the `resolution_family` field (Surface 3, Story 5.2.1), never just a bare "error" indicator.
3. A harmless, expected fallback (feature working as designed) never looks visually identical to a real outage (feature exhausted) — verified by the two states using different CSS classes/colors, not a shared "non-default" treatment.
4. The exhausted state is sticky until a real resolved success occurs — an operator who steps away and comes back still sees the red state if it was never actually fixed, never a state that silently self-cleared on a timer.
5. An operator authoring config can distinguish the two supported modes (`model` vs `model_family`) from the reference docs alone, and gets a specific, actionable validation error (not a generic failure) if they misconfigure both fields on one upstream ref.
6. A script polling `/metrics` on an upstream that never opted into `model_family` sees zero shape change from today's JSON — additive, not a breaking migration for existing consumers.
