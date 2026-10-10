# ADR-004: Definitions of "requested model", "routed model" and route reason

**Status**: Accepted (planning, 2026-10-09); revised after review

## Context
`dispatch` reads `body["model"]` into a local (`src/routing/router/mod.rs:676-681`) and `RequestDetail::from_body` stores it as `RequestDetail.model` (`src/metrics/mod.rs:96,116-121`), so today's trace `model` is already the raw client-requested name. The router rewrites the outbound body from `chosen.model` (`:736-743`) and injects `model_family` (`:744-746`); the concrete family-resolved id is chosen inside `providers/openai/resolution.rs` (`resolve_family` `:814`, `ResolvedModel` `:174`) and is not returned to the router. Bedrock rewrites ids inside the provider (`to_bedrock_model_id`).

## Decision
1. `requestedModel` = the raw `body["model"]` as the client sent it, captured in `TraceScope::begin` (the ingress handler) before any mutation, including compression and emulation-loop rewrites (equals today's `RequestDetail.model`; the placeholder `"unknown"` is surfaced as missing, not as a model name). Capturing at the handler means cache-hit requests have it too.
2. `routedModel` = `chosen.model` of the final successful (or last attempted) attempt when the route pins one; otherwise the requested name passes through and `routedModelSource = "passthrough"`. `routedProvider` = `chosen.name` (upstream name, never an index, so config hot-swap cannot mislabel history).
3. The provider-resolved wire id (family routes, Bedrock profile ids) is NOT in v1 scope by default. Task 1.1.1c verifies whether a small `ProviderResponse`-adjacent carrier is cheap; if yes it ships as optional `resolvedModel` in Story 2.1.3 (cut-able); if not it is a documented limitation and a backlog item.
4. `RouteReason` is a closed enum with a fixed UI vocabulary: `SessionPin`, `Strategy(name)` (name from the new `RoutingStrategy::name()` accessor), `Exploration`, `Fallback` (the attempt followed a failed/shed one), `RateLimitHold`, `Default`, `Cache`, `Local`. Precedence: `Cache`/`Local` (by `TraceKind`) > `SessionPin` > `Exploration` > `Fallback` > `RateLimitHold` > `Strategy` > `Default`. A single reason chip is derived for the row; per-attempt detail is in the drawer. The alias/family distinction is carried by `routedModelSource`, not a separate reason. There is no `alias`, `rule` or `no route` reason and no `routeRule` field: the router has no named-rule concept to source one from, and a request whose attempts all failed keeps its derived reason and shows the real failed status.
   **Wire vocabulary (the only one; `plan.md` "Route reason and label maps" and `design/ux.md` use the same table):**

   | `RouteReason` | `routeReason` wire string | Row chip |
   |---|---|---|
   | `SessionPin` | `session_pin` | `pinned` |
   | `Strategy` | `strategy` (plus `routeStrategy`, the `RoutingStrategy::name()` value; null for other reasons) | `strategy` |
   | `Exploration` | `exploration` | `explore` |
   | `Fallback` | `fallback` | `fallback` |
   | `RateLimitHold` | `rate_limit_hold` | `held` |
   | `Default` | `default` | `default` |
   | `Cache` | `cache` | `cache` |
   | `Local` | `local` | `local` |

   `cancelled` is a status-driven chip for `statusCode: 499`, not a reason; `fell back` / `retried` is a UI sub-label derived from the attempts and final status (no wire field).
5. Local-catalog requests (`dispatch_local`, `router/mod.rs:682-684`) are traced as `traceKind: "local"` with `routedProvider: "local"`; cache hits are `traceKind: "cache_hit"` with `routedProvider: null` (ADR-002 amendment, Story 2.1.6).

## Consequences
- (+) Requested vs routed is exact for pinned routes and honest ("passthrough") otherwise.
- (-) For family routes the UI cannot show the concrete model until the optional carrier lands.

## Alternatives rejected
- Post-alias requested name: loses what the client asked for; alias is recoverable as `routedModelSource`.
- Reading `resolution_state` at emit time: racy under concurrency, approximate by construction.
