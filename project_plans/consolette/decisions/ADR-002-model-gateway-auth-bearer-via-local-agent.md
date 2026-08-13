# ADR-002: Model Gateway Auth — `bearer` via the SBN Dev Agent Local Proxy (mTLS via the internal identity system is a follow-up)

**Status**: Accepted
**Date**: 2026-07-17

## Context

FR-5 requires a working, documented internal Model Gateway upstream usable from
Tyler's **personal (employer-managed) Mac laptop** — off-mesh, not from a deployed
service. CD-2 fixes pluggable per-upstream auth keyed `auth = bearer | apikey |
internal`, shipping `bearer`/`apikey` now and treating mTLS via the internal
identity system as a documented, non-blocking follow-up. The critical open question
(OQ-1) was whether a bearer/API-key path to the gateway exists from a laptop.

Research (`research/model-gateway-auth.md`, confidence HIGH) found:

- **There is no off-mesh bearer token or API key** the gateway itself validates.
  Every documented client config uses a **dummy** bearer (`sk-dummy`); real inbound
  auth is `oidc-envoy` (mTLS via the internal identity system) plus a per-caller
  Gandalf policy (`NCP-copilot-prod-{PROJECT_ID}`).
- Three laptop-viable modes exist:
  - **A. `bearer` → SBN Dev Agent local proxy** (`http://localhost:9123/proxy/{PROJECT_ID}`):
    the local agent mints internal identity credentials on the developer's behalf;
    Consolette speaks plain HTTP with a dummy bearer. ~zero code. This is exactly
    how Claude Code, Codex, Cursor, Goose, Aider, and Zed are wired internally today.
  - **B. `bearer` → self-run mesh sidecar** (`newt --app-type mesh start`,
    `…{internal-mesh-vip}:7002/proxy/{PROJECT}`): also plain HTTP + dummy bearer.
  - **C. native `auth = internal`** (direct Discovery VIP
    `…{internal-service-vip}:7004` with the laptop's internal identity
    system *user* cert + `x-internal-project-id` header): feasible from a
    laptop, but native-Rust cert plumbing is unproven (no public Rust crate for
    this internal identity system found).
- The gateway supports the **Anthropic-native `/v1/messages` catch-all
  passthrough** (Jira NCP-2914) — Claude Code speaks Anthropic natively, so no
  OpenAI translation is needed for that path.
- **Ports differ by path form** (`:9123`/`:9124` local agent, `:2002` OpenAI-direct
  mesh, `:7002` `/proxy` prefix mesh, `:7004` direct VIP) — config must not hardcode
  a port/URL.
- **Internal policy forbids personal OpenAI/Anthropic vendor keys** for work use.

## Decision

1. **Ship `bearer` mode now, targeting the SBN Dev Agent local proxy (Option A).**
   The Model Gateway upstream is `kind = openai` (or Anthropic-native passthrough)
   with `base_url = "http://localhost:9123/proxy/{PROJECT_ID}"` and
   `auth = { type = "bearer", token = "sk-dummy" }`. The `base_url` (host+port+path)
   is fully config-driven — **never hardcode a port** — so Option B (self-run mesh
   sidecar) works by only changing `base_url`.
2. **Support both gateway paths.** OpenAI-compatible (`/v1/chat/completions`) via
   the `openai` provider, and the Anthropic-native `/v1/messages` passthrough for
   Claude Code (an `openai`-kind upstream with an `anthropic_passthrough` flag /
   base ending at `/proxy/{PROJECT_ID}`).
3. **Reserve `auth = internal` in the schema; runtime is a validated stub now.**
   Config accepts and validates `type = "internal"` (with `project_id`, `app`) but
   dispatch returns a clear `ProviderError::Auth("auth=internal not yet
   implemented; see ADR-002")`. When implemented, **v2 shells out to
   the internal identity CLI (`internal-identity curl -a copilotdppython … :7004`)**
   first (proven, matches every doc), and may later graduate to native
   reqwest+rustls once cert-material paths and rotation are verified.
4. **Do NOT offer a "bring your own personal vendor key" mode** for work use — the
   Model Gateway is the sanctioned path (internal policy guidance).

## Alternatives Considered

| Option | Rejected because |
|--------|-----------------|
| Native `auth = internal` (rustls) now | No public Rust crate for this internal identity system; cert/key locations + rotation unverified (research risk #2). High effort, MEDIUM confidence — explicitly out of scope per CD-2. |
| Internal identity CLI shell-out now | Feasible and proven, but adds a subprocess + cert-rotation handling; unnecessary while the SBN Dev Agent local proxy gives the same result with ~zero code. Kept as the v2 bridge. |
| Personal Anthropic/OpenAI vendor key | Forbidden by internal policy for work use. |
| Hardcode a gateway URL/port | Ports differ by path form (`:9123/:2002/:7002/:7004`); hardcoding breaks Option B and the direct-VIP future. `base_url` is config-driven. |
| Mesh-only (Option B) as the default | Requires running `newt --app-type mesh start`; the SBN Dev Agent (Option A) is the paved laptop road. B remains reachable by changing `base_url`. |

## Consequences

- The gateway integration is net-new; today's proxy has no Model Gateway target.
- **Hard dependency** on the SBN Dev Agent running on `:9123` (Java-oriented
  install `newt --app-type=java-project install-dev-agent`; must survive reboots).
  Documented as a prerequisite; when the agent is down, the upstream fails cleanly
  and the router falls through to other upstreams (Epic 3).
- **Prereqs (documented in `references/model-gateway.md`):** VPN on; project
  registered at `go/modelgateway`; user in the project's Gandalf policy
  (`NCP-copilot-prod-{PROJECT_ID}`) — missing membership → 401, wrong/absent
  project → 404.
- `token = "sk-dummy"` is honest only mechanically — identity comes from the local
  agent. The doc states this plainly so it is not mistaken for a real credential.
- Rate limits are **shared across all internal users, per model** (e.g. Claude 3.7
  Sonnet ~1M tok/min shared) — feeds Epic 4's per-upstream limiting + cooldown.
- Future `auth = internal` is unblocked: schema + validation already accept it;
  only the runtime resolver needs the internal identity CLI shell-out.

## Update (2026-07-20) — superseded by ADR-007

Per Tyler's plugin-format directive, **decision §3 is superseded**: the core no
longer has an `internal` auth type at all. Core ships `bearer | apikey | exec`
(generic credential-helper). Auth via the internal identity system is delivered
as the **internal-identity plugin's** `exec` helper (`bin/consolette-internal-auth-helper`,
shipped in ndotfiles), keeping all internal hostnames/project-ids/internal-identity
logic out of the public repo.
Options A/B (dummy bearer to a local proxy) in §1–2 are unchanged and remain the
default. See ADR-007 for the plugin format + credential-helper exec protocol.
</content>
